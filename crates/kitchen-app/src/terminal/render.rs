//! Painting the terminal grid and routing input to the pty.

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::CursorShape;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, CornerRadius, EventFilter, FontId, Modifiers, PointerButton, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2};

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
        } else if resp.has_focus() {
            // Another pane has the keyboard: holding egui focus here would turn
            // its Enter/Space into a fake click on the terminal (and steal focus).
            resp.surrender_focus();
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
        if session.predictor.pending() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
        }
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

/// The cursor's viewport cell when it is visible on the live screen.
fn live_cursor(session: &TermSession) -> Option<(usize, usize)> {
    cursor_of(&session.term.lock())
}

fn cursor_of(term: &alacritty_terminal::Term<super::Listener>) -> Option<(usize, usize)> {
    if term.grid().display_offset() != 0 || !term.mode().contains(TermMode::SHOW_CURSOR) {
        return None;
    }
    let p = term.grid().cursor.point;
    Some((p.line.0.max(0) as usize, p.column.0))
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
                    session.predictor.reset();
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
                let cursor = live_cursor(session);
                let cols = session.size().cols;
                session.predictor.typed(&text, cursor, cols);
                session.write(text.into_bytes());
                wrote = true;
            }
            egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                session.write(text.into_bytes());
                wrote = true;
            }
            egui::Event::Paste(text) => {
                session.predictor.reset();
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
    } else if resp.clicked_by(egui::PointerButton::Primary) {
        session.term.lock().selection = None;
    }
}

fn paint(ui: &mut Ui, rect: Rect, session: &mut TermSession, geom: &Geom, font: &FontId, focused: bool) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::ZERO, BACKGROUND);

    let term_arc = session.term.clone();
    let term = term_arc.lock();
    // Check predictions against exactly what is painted: cursor and cells from
    // this one lock (cmux drew guesses a cell off when they were read apart).
    if session.predictor.pending() {
        let cursor = cursor_of(&term);
        session.predictor.validate(|row, col| term.grid()[Point::new(Line(row as i32), Column(col))].c, cursor);
    }
    let content = term.renderable_content();
    let overrides = content.colors;
    let offset = content.display_offset as i32;
    let selection = content.selection;
    let cursor = content.cursor;

    // Cells covered by a shown prediction: the prediction replaces the cell's
    // glyph (never drawn on top of it — that overlapped real text).
    let preds = session.predictor.visible();
    let covered = |row: usize, col: usize| offset == 0 && preds.iter().any(|p| p.row == row && p.col == col);

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
        let c = if covered(row, col) { ' ' } else { cell.c };
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

    // Predicted (not yet echoed) characters, underlined; the cursor is drawn after them.
    for p in preds.iter().filter(|_| offset == 0) {
        if p.row < geom.rows && p.col < geom.cols {
            let r = geom.cell_rect(p.col, p.row, 1);
            painter.text(r.left_top(), egui::Align2::LEFT_TOP, p.c, font.clone(), colors::FOREGROUND);
            painter.hline(r.x_range(), r.bottom() - 1.0, Stroke::new(1.0, colors::FOREGROUND.gamma_multiply(0.5)));
        }
    }
    let mut cursor = cursor;
    if let Some(last) = preds.last() {
        cursor.point = Point::new(Line(last.row as i32 - offset), Column((last.col + 1).min(geom.cols - 1)));
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

#[cfg(test)]
mod bench {
    use super::*;
    use std::collections::HashMap;

    /// `cargo test --release -p kitchen-app bench_frame -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_frame() {
        let ctx = egui::Context::default();
        let mut session = TermSession::spawn(
            &ctx,
            &["sh".into(), "-c".into(), "for i in $(seq 1 60); do printf '\\033[3%dmline %d the quick brown fox \\033[1mjumps\\033[0m over the lazy dog 0123456789 ✓ 日本\\n' $((i%7)) $i; done; sleep 30".into()],
            None,
            HashMap::new(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 860.0))),
            ..Default::default()
        };
        let mut run = |n: usize, mutate: bool| {
            let t = std::time::Instant::now();
            for i in 0..n {
                if mutate {
                    // Simulate typing: one cell changes per frame.
                    let c = char::from(b'a' + (i % 26) as u8);
                    session.term.lock().grid_mut()[Point::new(Line(10), Column(i % 80))].c = c;
                }
                let _ = ctx.run_ui(input(), |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        TerminalView::new(&mut session).focused(true).show(ui);
                    });
                });
            }
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        };
        run(5, false);
        println!("static frame: {:.3} ms", run(200, false));
        println!("typing frame: {:.3} ms", run(200, true));
    }
}

#[cfg(test)]
mod latency {
    use super::*;
    use std::collections::HashMap;

    /// Echo latency through a real attach command:
    /// `KT_ATTACH='ssh ... tmux attach -t x' cargo test --release -p kitchen-app echo_latency -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn echo_latency() {
        let Ok(cmd) = std::env::var("KT_ATTACH") else { return };
        let ctx = egui::Context::default();
        let session = TermSession::spawn(&ctx, &["sh".into(), "-c".into(), cmd], None, HashMap::new()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
        let count = |s: &TermSession| {
            let t = s.term.lock();
            let mut n = 0;
            for row in t.grid().display_iter() {
                if row.cell.c == 'q' {
                    n += 1;
                }
            }
            n
        };
        let mut samples = Vec::new();
        for _ in 0..15 {
            let before = count(&session);
            let t = std::time::Instant::now();
            session.write(&b"q"[..]);
            while count(&session) == before && t.elapsed().as_secs() < 3 {
                std::thread::sleep(std::time::Duration::from_micros(500));
            }
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("echo ms: median {:.1} p90 {:.1} max {:.1}", samples[7], samples[13], samples[14]);
        session.write(&b"\x15"[..]);
    }
}

#[cfg(test)]
mod perceived {
    use super::*;
    use std::collections::HashMap;

    /// Keypress-to-drawn latency through TerminalView, prediction on:
    /// `KT_ATTACH=... cargo test --release -p kitchen-app perceived_latency -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn perceived_latency() {
        let Ok(cmd) = std::env::var("KT_ATTACH") else { return };
        let ctx = egui::Context::default();
        let mut s = TermSession::spawn(&ctx, &["sh".into(), "-c".into(), cmd], None, HashMap::new()).unwrap();
        s.set_predictive_echo(true);
        std::thread::sleep(std::time::Duration::from_secs(3));
        let frame = |s: &mut TermSession, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0))),
                events,
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    TerminalView::new(s).focused(true).show(ui);
                });
            });
        };
        let shown = |s: &TermSession, n: usize| {
            let t = s.term.lock();
            let echoed = t.grid().display_iter().filter(|c| c.cell.c == 'q').count();
            echoed + s.predictor.visible().iter().filter(|p| p.c == 'q').count() >= n
        };
        let mut out = Vec::new();
        for i in 0..12 {
            let before = { let t = s.term.lock(); t.grid().display_iter().filter(|c| c.cell.c == 'q').count() };
            let t0 = std::time::Instant::now();
            frame(&mut s, vec![egui::Event::Text("q".into())]);
            while !shown(&s, before + 1) && t0.elapsed().as_secs() < 3 {
                frame(&mut s, vec![]);
                std::thread::sleep(std::time::Duration::from_micros(500));
            }
            out.push((i, t0.elapsed().as_secs_f64() * 1000.0));
            // Let the echo land before the next key (steady typing).
            std::thread::sleep(std::time::Duration::from_millis(150));
            frame(&mut s, vec![]);
        }
        println!("{}", out.iter().map(|(i, ms)| format!("#{i}:{ms:.1}ms")).collect::<Vec<_>>().join(" "));
        s.write(&b"\x15"[..]);
    }
}


#[cfg(test)]
mod devbox_typing {
    use super::*;
    use std::collections::HashMap;

    /// Fast typing on a real devbox shell (zsh + autosuggestions):
    /// predictions stay contiguous from the real cursor every frame, never
    /// stack on one cell, and the line ends up right.
    /// `KT_ATTACH=... cargo test -p kitchen-app typing_burst -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn typing_burst() {
        let Ok(cmd) = std::env::var("KT_ATTACH") else { return };
        let ctx = egui::Context::default();
        let mut s = TermSession::spawn(&ctx, &["sh".into(), "-c".into(), cmd], None, HashMap::new()).unwrap();
        s.set_predictive_echo(true);
        std::thread::sleep(std::time::Duration::from_secs(3));
        let frame = |s: &mut TermSession, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    TerminalView::new(s).focused(true).show(ui);
                });
            });
            out.textures_delta.clear();
        };
        let check = |s: &mut TermSession| -> usize {
            let term_arc = s.term.clone();
            let term = term_arc.lock();
            let cur = cursor_of(&term);
            s.predictor.validate(|row, col| term.grid()[Point::new(Line(row as i32), Column(col))].c, cur);
            let preds = s.predictor.visible();
            if let (Some(first), Some((r, c))) = (preds.first(), cur) {
                assert_eq!(first.row, r, "predictions on the cursor's row");
                assert!(first.col >= c, "prediction behind the cursor: {first:?} cursor {c}");
            }
            for w in preds.windows(2) {
                assert_eq!(w[1].col, w[0].col + 1, "predictions contiguous");
            }
            preds.len()
        };
        let line = "echo hello world";
        for round in 0..3 {
            frame(&mut s, vec![]);
            let mut max_shown = 0;
            for ch in line.chars() {
                frame(&mut s, vec![egui::Event::Text(ch.to_string())]);
                max_shown = max_shown.max(check(&mut s));
                std::thread::sleep(std::time::Duration::from_millis(30));
                frame(&mut s, vec![]);
                max_shown = max_shown.max(check(&mut s));
            }
            let t0 = std::time::Instant::now();
            while s.predictor.pending() && t0.elapsed().as_secs() < 3 {
                frame(&mut s, vec![]);
                check(&mut s);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(!s.predictor.pending(), "every prediction confirmed");
            let row = live_cursor(&s).unwrap().0;
            let text: String = {
                let t = s.term.lock();
                (0..t.columns()).map(|c| t.grid()[Point::new(Line(row as i32), Column(c))].c).collect()
            };
            assert!(text.contains(line), "line on screen: {text:?}");
            println!("round {round}: up to {max_shown} characters shown ahead of the echo; line ok");
            // Clear the line (Ctrl-U) for the next round.
            s.write(&b"\x15"[..]);
            std::thread::sleep(std::time::Duration::from_millis(300));
            frame(&mut s, vec![]);
        }
    }
}
