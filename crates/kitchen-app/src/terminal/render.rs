//! Painting the terminal grid and routing input to the pty.

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::CursorShape;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, CornerRadius, EventFilter, FontId, Key, Modifiers, PointerButton, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2};

use super::colors::{self, BACKGROUND, CURSOR, SELECTION};
use super::input::{key_bytes, paste_bytes, sgr_mouse};
use super::{GridSize, TermSession};

const PAD: f32 = 6.0;

pub struct TerminalView<'a> {
    session: &'a mut TermSession,
    font_size: f32,
    has_focus: bool,
}

impl<'a> TerminalView<'a> {
    pub fn new(session: &'a mut TermSession) -> Self {
        Self { session, font_size: 13.0, has_focus: false }
    }

    pub fn font_size(mut self, size: f32) -> Self {
        self.font_size = size;
        self
    }

    /// Whether keyboard input goes to this terminal.
    pub fn focused(mut self, focused: bool) -> Self {
        self.has_focus = focused;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let TerminalView { session, font_size, has_focus } = self;
        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let font = FontId::monospace(font_size);
        let (cw, ch) = ui.ctx().fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let inner = rect.shrink(PAD);
        let cols = ((inner.width() / cw).floor() as usize).max(2);
        let rows = ((inner.height() / ch).floor() as usize).max(1);
        session.resize(GridSize { cols, rows }, (cw.round() as u16, ch.round() as u16));

        if has_focus {
            resp.request_focus();
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    resp.id,
                    EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
            handle_keyboard(ui, session);
        }
        if has_focus != session.focused {
            session.focused = has_focus;
            if session.term.lock().mode().contains(TermMode::FOCUS_IN_OUT) {
                session.write(if has_focus { &b"\x1b[I"[..] } else { &b"\x1b[O"[..] });
            }
        }
        let geom = Geom { origin: inner.min, cw, ch, cols, rows };
        handle_mouse(ui, &resp, session, &geom);
        if let Some(text) = session.take_clipboard() {
            ui.ctx().copy_text(text);
        }
        paint(ui, rect, session, &geom, &font, has_focus);
        resp
    }
}

struct Geom {
    origin: Pos2,
    cw: f32,
    ch: f32,
    cols: usize,
    rows: usize,
}

impl Geom {
    /// Viewport cell under `pos`, clamped into the grid, plus which half of it.
    fn cell_at(&self, pos: Pos2) -> (usize, usize, Side) {
        let x = ((pos.x - self.origin.x) / self.cw).max(0.0);
        let y = ((pos.y - self.origin.y) / self.ch).max(0.0);
        let col = (x as usize).min(self.cols - 1);
        let row = (y as usize).min(self.rows - 1);
        let side = if x.fract() < 0.5 { Side::Left } else { Side::Right };
        (col, row, side)
    }

    fn cell_rect(&self, col: usize, row: usize, width: usize) -> Rect {
        Rect::from_min_size(
            Pos2::new(self.origin.x + col as f32 * self.cw, self.origin.y + row as f32 * self.ch),
            Vec2::new(self.cw * width as f32, self.ch),
        )
    }
}

fn handle_keyboard(ui: &mut Ui, session: &mut TermSession) {
    let events = ui.input(|i| i.events.clone());
    let mode = *session.term.lock().mode();
    let mut skip_text = false;
    let mut wrote = false;
    for ev in events {
        match ev {
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
                if modifiers.mac_cmd || modifiers.command && !modifiers.ctrl {
                    continue; // Cmd chords are kitchen shortcuts.
                }
                if let Some(bytes) = key_bytes(key, modifiers, mode) {
                    session.write(bytes);
                    wrote = true;
                    // Option/Ctrl combos also emit a composed Text event; drop it.
                    skip_text = modifiers.alt || modifiers.ctrl;
                }
            }
            egui::Event::Text(text) => {
                if std::mem::take(&mut skip_text) {
                    continue;
                }
                if ui.input(|i| i.modifiers.mac_cmd) {
                    continue;
                }
                session.write(text.into_bytes());
                wrote = true;
            }
            egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                session.write(text.into_bytes());
                wrote = true;
            }
            egui::Event::Paste(text) => {
                session.write(paste_bytes(&text, mode));
                wrote = true;
            }
            egui::Event::Copy => {
                if let Some(sel) = session.term.lock().selection_to_string() {
                    ui.ctx().copy_text(sel);
                }
            }
            _ => {}
        }
    }
    if wrote {
        // Typing snaps back to the live screen and clears a stale selection.
        let mut term = session.term.lock();
        term.scroll_display(Scroll::Bottom);
        term.selection = None;
    }
}

fn handle_mouse(ui: &mut Ui, resp: &Response, session: &mut TermSession, geom: &Geom) {
    let Some(pos) = ui.input(|i| i.pointer.latest_pos()) else { return };
    let hovered = resp.rect.contains(pos);
    let mode = *session.term.lock().mode();
    let mods: Modifiers = ui.input(|i| i.modifiers);
    let (col, row, side) = geom.cell_at(pos);
    // Shift bypasses mouse reporting so local selection always works.
    let report = mode.intersects(TermMode::MOUSE_MODE) && !mods.shift;

    if hovered {
        let (lines, unit_is_line) = ui.input(|i| {
            let mut dy = 0.0;
            let mut line_unit = false;
            for e in &i.events {
                if let egui::Event::MouseWheel { unit, delta, .. } = e {
                    dy += delta.y;
                    line_unit |= *unit == egui::MouseWheelUnit::Line;
                }
            }
            (dy, line_unit)
        });
        if lines != 0.0 {
            session.scroll_acc += if unit_is_line { lines } else { lines / geom.ch };
            let whole = session.scroll_acc.trunc() as i32;
            session.scroll_acc -= whole as f32;
            if whole != 0 {
                let n = whole.unsigned_abs() as usize;
                let up = whole > 0;
                if report {
                    for _ in 0..n {
                        session.write(sgr_mouse(if up { 64 } else { 65 }, col, row, true, false, mods));
                    }
                } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
                    let seq: &[u8] = match (up, mode.contains(TermMode::APP_CURSOR)) {
                        (true, true) => b"\x1bOA",
                        (true, false) => b"\x1b[A",
                        (false, true) => b"\x1bOB",
                        (false, false) => b"\x1b[B",
                    };
                    session.write(seq.repeat(n));
                } else {
                    session.term.lock().scroll_display(Scroll::Delta(whole));
                }
            }
        }
    }

    if report {
        for (button, code) in [(PointerButton::Primary, 0u8), (PointerButton::Middle, 1), (PointerButton::Secondary, 2)] {
            let bit = 1 << code;
            if hovered && ui.input(|i| i.pointer.button_pressed(button)) {
                session.mouse_buttons_down |= bit;
                session.write(sgr_mouse(code, col, row, true, false, mods));
            }
            if session.mouse_buttons_down & bit != 0 && ui.input(|i| i.pointer.button_released(button)) {
                session.mouse_buttons_down &= !bit;
                session.write(sgr_mouse(code, col, row, false, false, mods));
            }
        }
        if session.mouse_buttons_down & 1 != 0
            && mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
            && ui.input(|i| i.pointer.delta() != Vec2::ZERO)
        {
            session.write(sgr_mouse(0, col, row, true, true, mods));
        }
        return;
    }

    let display_offset = session.term.lock().grid().display_offset();
    let point = Point::new(Line(row as i32 - display_offset as i32), Column(col));
    if resp.drag_started_by(PointerButton::Primary) {
        session.term.lock().selection = Some(Selection::new(SelectionType::Simple, point, side));
    } else if resp.dragged_by(PointerButton::Primary) {
        if let Some(sel) = session.term.lock().selection.as_mut() {
            sel.update(point, side);
        }
    } else if resp.double_clicked() {
        session.term.lock().selection = Some(Selection::new(SelectionType::Semantic, point, side));
    } else if resp.clicked() {
        session.term.lock().selection = None;
    }
}

fn paint(ui: &mut Ui, rect: Rect, session: &TermSession, geom: &Geom, font: &FontId, focused: bool) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::ZERO, BACKGROUND);

    let term = session.term.lock();
    let content = term.renderable_content();
    let overrides = content.colors;
    let offset = content.display_offset as i32;
    let selection = content.selection;
    let cursor = content.cursor;

    // Per-row text jobs (ASCII only, so every glyph is exactly one cell wide)
    // plus a list of wide/non-ASCII glyphs painted individually.
    let mut jobs: Vec<LayoutJob> = (0..geom.rows).map(|_| LayoutJob::default()).collect();
    let mut row_cols: Vec<usize> = vec![0; geom.rows];
    let mut singles: Vec<(usize, usize, char, Color32)> = Vec::new();
    let default_fmt = TextFormat { font_id: font.clone(), color: colors::FOREGROUND, ..Default::default() };

    for indexed in content.display_iter {
        let row = indexed.point.line.0 + offset;
        if row < 0 || row as usize >= geom.rows {
            continue;
        }
        let row = row as usize;
        let col = indexed.point.column.0;
        if col >= geom.cols {
            continue;
        }
        let cell = &indexed.cell;
        if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        let mut fg = colors::resolve(cell.fg, overrides);
        let mut bg = colors::resolve(cell.bg, overrides);
        let mut default_bg = colors::is_default_bg(cell.bg);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
            default_bg = false;
        }
        if cell.flags.contains(Flags::DIM) {
            fg = fg.gamma_multiply(0.66);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if selection.map(|s| s.contains(indexed.point)).unwrap_or(false) {
            bg = SELECTION;
            default_bg = false;
        }
        let width = if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
        if !default_bg {
            painter.rect_filled(geom.cell_rect(col, row, width), CornerRadius::ZERO, bg);
        }

        // Pad the job with spaces up to this column.
        let job = &mut jobs[row];
        if row_cols[row] < col {
            job.append(&" ".repeat(col - row_cols[row]), 0.0, default_fmt.clone());
        }
        let c = cell.c;
        let ascii = c.is_ascii() && !c.is_ascii_control() && width == 1;
        let fmt = TextFormat {
            font_id: font.clone(),
            color: fg,
            italics: cell.flags.contains(Flags::ITALIC),
            underline: if cell.flags.intersects(Flags::ALL_UNDERLINES) { Stroke::new(1.0, fg) } else { Stroke::NONE },
            strikethrough: if cell.flags.contains(Flags::STRIKEOUT) { Stroke::new(1.0, fg) } else { Stroke::NONE },
            ..Default::default()
        };
        if ascii {
            append_char(job, c, fmt);
        } else {
            append_char(job, ' ', fmt);
            if width == 2 {
                job.append(" ", 0.0, default_fmt.clone());
            }
            if c != ' ' && !c.is_control() {
                singles.push((col, row, c, fg));
            }
        }
        row_cols[row] = col + width;
    }

    for (row, job) in jobs.into_iter().enumerate() {
        if job.text.trim().is_empty() && job.sections.iter().all(|s| s.format.underline == Stroke::NONE) {
            continue;
        }
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        painter.galley(geom.cell_rect(0, row, 1).min, galley, colors::FOREGROUND);
    }
    for (col, row, c, color) in singles {
        let r = geom.cell_rect(col, row, 1);
        painter.text(r.left_top(), egui::Align2::LEFT_TOP, c, font.clone(), color);
    }

    // Cursor (only on the live screen).
    let cursor_row = cursor.point.line.0 + offset;
    if cursor.shape != CursorShape::Hidden && cursor_row >= 0 && (cursor_row as usize) < geom.rows {
        let r = geom.cell_rect(cursor.point.column.0.min(geom.cols - 1), cursor_row as usize, 1);
        match (focused, cursor.shape) {
            (true, CursorShape::Beam) => {
                painter.rect_filled(Rect::from_min_size(r.min, Vec2::new(2.0, r.height())), CornerRadius::ZERO, CURSOR);
            }
            (true, CursorShape::Underline) => {
                painter.rect_filled(
                    Rect::from_min_size(Pos2::new(r.min.x, r.max.y - 2.0), Vec2::new(r.width(), 2.0)),
                    CornerRadius::ZERO,
                    CURSOR,
                );
            }
            (true, _) => {
                painter.rect_filled(r, CornerRadius::ZERO, CURSOR.gamma_multiply(0.75));
                let c = term.grid()[cursor.point].c;
                if c != ' ' {
                    painter.text(r.left_top(), egui::Align2::LEFT_TOP, c, font.clone(), BACKGROUND);
                }
            }
            (false, _) => {
                painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, CURSOR), egui::StrokeKind::Inside);
            }
        }
    }

    if offset > 0 {
        let label = format!("↑ {offset} / {}", term.grid().history_size());
        let pos = Pos2::new(rect.right() - 8.0, rect.top() + 6.0);
        painter.text(pos, egui::Align2::RIGHT_TOP, label, FontId::proportional(11.0), Color32::from_gray(160));
    }
}

fn append_char(job: &mut LayoutJob, c: char, fmt: TextFormat) {
    // Extend the previous section when the style matches: far fewer sections.
    if let Some(last) = job.sections.last_mut() {
        if last.format == fmt && last.byte_range.end.0 == job.text.len() {
            job.text.push(c);
            last.byte_range.end.0 = job.text.len();
            return;
        }
    }
    let mut buf = [0u8; 4];
    job.append(c.encode_utf8(&mut buf), 0.0, fmt);
}
