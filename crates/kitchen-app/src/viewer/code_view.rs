//! Read-only code widget with vim navigation, used by the file viewer and
//! the plan reviewer. It paints a [`Doc`], inline blocks (plan
//! comments) and an optional comment editor, and turns plain
//! keys into [`Vim`] motions while focused. Rendered rows (`kitchen.render`)
//! are drawn in a proportional font and wrap instead of scrolling sideways.
//! A row with Markdown source and every card's text are drawn by
//! [`markdown::draw`], as GitHub would show them.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use egui::text::{CCursor, LayoutJob, TextFormat};
use egui::{Color32, CornerRadius, FontId, Galley, Key, Modifiers, Pos2, Rect, Sense, Shape, Stroke, Ui, Vec2};

use super::doc::{Doc, Fold, LineKind};
use super::highlight::group_color;
use super::markdown::{self, CODE_BG};
use super::vim::{Vim, VimAction, VimKey, Viewport};
use crate::plugins::decor::{RenderRow, RenderSpan, Reply};
use crate::theme;

/// Where a row's text was drawn, for clicks.
enum Hit {
    /// Monospace text at the view's column grid.
    Text,
    /// A rendered row's galley, drawn at the position.
    Galley(Pos2, Arc<Galley>),
    /// Markdown: the row as a whole.
    Block,
}

/// Scroll position; owned by the panel so it survives frames.
#[derive(Debug, Default, Clone)]
pub struct ViewState {
    /// First doc row whose unit (the row, its comment field and its blocks)
    /// is on screen.
    pub(super) top: usize,
    /// Pixels of that unit scrolled above the view's top edge.
    offset: f32,
    pub left: usize,
    /// Text rows that fit, from the last frame.
    pub rows: usize,
    /// Cursor row at the end of the last frame: a change means the cursor moved.
    last_line: Option<usize>,
    /// A `g` held back while scrolling a tall row: `gg` goes to its top.
    g_pending: bool,
    /// Heights measured invisibly, for the doc length, width and font size
    /// in `md_key`; painting refreshes them, a new key clears them.
    heights: Heights,
    md_key: (usize, u32, u32),
    /// Height of the open comment field, measured as it was last painted (it
    /// grows with its text); `None` until it is painted.
    editor_h: Option<f32>,
}

impl ViewState {
    /// Show row `top` at the view's top edge.
    pub(super) fn scroll_to(&mut self, top: usize) {
        self.top = top;
        self.offset = 0.0;
    }
}

/// Measured heights: Markdown rows by doc row, cards by `card_key`. Cards
/// no longer shown are dropped every frame (a thinking card's text changes
/// every second).
#[derive(Debug, Default, Clone)]
struct Heights {
    rows: HashMap<usize, f32>,
    cards: HashMap<u64, f32>,
}

/// Heights in pixels of one doc row's unit, measured as it is painted.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Extent {
    /// File-level blocks drawn above the row (row 0 only).
    lead: f32,
    /// The row itself.
    row: f32,
    /// lead + row + comment field + the row's blocks.
    unit: f32,
}

/// A note drawn under a line.
#[derive(Debug, Clone, Default)]
pub struct Block {
    /// Doc index it follows; `None` = above the first line (file-level).
    pub after: Option<usize>,
    pub title: String,
    /// Markdown.
    pub body: String,
    pub color: Color32,
    pub dim: bool,
    /// A ⌘⇧A answer: `title` is the question, drawn as the card's heading.
    pub question: bool,
    /// Later comments of its thread, each under its own header.
    pub replies: Vec<Reply>,
    /// `owner/repo` that `#12`, `@al` and SHAs in the text link to.
    pub repo: Option<String>,
    /// The file the card is on, for suggestion blocks.
    pub path: Option<String>,
}

impl Block {
    /// A ⌘⇧A question under row `after`, with its answer (`None` while the
    /// model thinks; the card counts the seconds since `asked`).
    pub fn answer(after: usize, question: &str, answer: Option<&Result<String, String>>, asked: Instant) -> Block {
        let (body, color, dim) = match answer {
            None => (format!("thinking… {}s", asked.elapsed().as_secs()), theme::PURPLE, true),
            Some(Ok(t)) => (t.clone(), theme::PURPLE, false),
            Some(Err(e)) => (e.clone(), theme::RED, false),
        };
        Block { after: Some(after), title: question.to_string(), body, color, dim, question: true, ..Default::default() }
    }
}

/// Plugin decorations for one display line, in display (tab-expanded) byte
/// offsets. Applied while painting; never baked into the [`Doc`].
#[derive(Debug, Clone, Default)]
pub struct LineOverlay {
    /// Foreground colors that override the syntax colors.
    pub spans: Vec<(std::ops::Range<usize>, Color32)>,
    /// Underlined ranges (diagnostics).
    pub underlines: Vec<(std::ops::Range<usize>, Color32)>,
    /// Gutter sign.
    pub sign: Option<(char, Color32)>,
    /// Dim text after the line (the first diagnostic's message).
    pub virt: Option<(String, Color32)>,
}

/// Decorations by 1-based current-file line (`DocLine::new_no`).
#[derive(Debug, Clone, Default)]
pub struct Overlay {
    pub lines: std::collections::HashMap<usize, LineOverlay>,
}

/// The ⌘M comment field.
#[derive(Debug, Clone)]
pub struct Editor {
    /// First and last doc index the comment covers.
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub label: String,
    pub needs_focus: bool,
    /// A ⌘⇧A question: Enter submits it (⇧Enter: a new line).
    pub ask: bool,
    /// A ⌘⇧A follow-up: drawn under the line's cards (the thread), not above them.
    pub follow_up: bool,
}

#[derive(Debug, Default)]
pub struct Output {
    pub actions: Vec<VimAction>,
    pub clicked: bool,
    /// Esc pressed inside the editor.
    pub cancel_editor: bool,
    /// Enter pressed in a question field.
    pub submit_editor: bool,
    /// Links ⌘-clicked in rendered Markdown, to open with `Link::Url`.
    pub urls: Vec<String>,
}

pub struct CodeView<'a> {
    pub doc: &'a Doc,
    pub vim: &'a mut Vim,
    pub state: &'a mut ViewState,
    pub blocks: &'a [Block],
    pub editor: Option<&'a mut Editor>,
    pub focused: bool,
    pub font_size: f32,
    /// Show base-side line numbers too (diff mode).
    pub two_gutters: bool,
    /// vim `relativenumber`: other rows show their distance from the cursor
    /// (the count for `5j`); the cursor row shows its real line number.
    pub relative: bool,
    /// Plugin highlights / diagnostics for the current file.
    pub overlay: Option<&'a Overlay>,
    pub id: egui::Id,
}

/// Comment field height before it is first painted: its label and four rows.
const EDITOR_HEIGHT: f32 = 118.0;
/// Space above a card or the comment field, and under it.
const BLOCK_TOP: f32 = 4.0;
const BLOCK_GAP: f32 = 12.0;
const EDITOR_GAP: f32 = 12.0;
/// Inside a card: padding around its text, and between its parts.
const CARD_PAD_X: f32 = 12.0;
const CARD_PAD_Y: f32 = 8.0;
const PART_GAP: f32 = 6.0;
/// Widest a card gets, in code columns: prose wider than this is hard to read.
const CARD_COLS: f32 = 100.0;
/// Behind code in a card: darker than the card.
const CARD_CODE_BG: Color32 = theme::BG;

/// What painting and measuring share: the frame's metrics and inputs. Built
/// once per frame in `show`.
struct Geometry<'a> {
    doc: &'a Doc,
    blocks: &'a [Block],
    /// Doc row the comment field sits under, and the field's height.
    editor: Option<(usize, f32)>,
    font: FontId,
    font_size: f32,
    cw: f32,
    row_h: f32,
    rect: Rect,
    text_x: f32,
    block_width: f32,
    overlay: Option<&'a Overlay>,
    id: egui::Id,
}

impl Geometry<'_> {
    /// Plugin decorations of row `idx` (current-file lines only).
    fn over(&self, idx: usize) -> Option<&LineOverlay> {
        let line = &self.doc.lines[idx];
        line.new_no.filter(|_| line.kind != LineKind::Removed).and_then(|n| self.overlay.and_then(|o| o.lines.get(&n)))
    }

    /// Lays out row `idx` with its top at `y`, scrolled `left` columns: its
    /// galley (None for a Markdown row, which draws itself), where the text
    /// goes, and its height. `paint: false` draws a Markdown row invisibly.
    fn layout_row(&self, ui: &mut Ui, idx: usize, y: f32, left: usize, paint: bool) -> (Option<Arc<Galley>>, Pos2, f32) {
        let (rect, cw, font_size) = (self.rect, self.cw, self.font_size);
        let line = &self.doc.lines[idx];
        match line.render.as_deref() {
            Some(r) if r.markdown.is_some() => {
                let at = Pos2::new(self.text_x + cw * 0.8 + r.indent as f32 * cw * 2.0, y + 3.0);
                let text = markdown::Text { src: r.markdown.as_deref().unwrap_or_default(), repo: None, path: None, size: font_size + 1.0, dim: false, code_bg: CODE_BG };
                let h = markdown::draw(ui, &text, at, rect.right() - at.x - 16.0, rect, self.id.with(("md", idx)), paint);
                (None, at, (h + 8.0).max(self.row_h))
            }
            Some(r) => {
                let quote = if r.block.as_deref() == Some("quote") { cw * 1.5 } else { 0.0 };
                // Clear of the diff's +/- marker.
                let x = self.text_x + cw * 0.8 + r.indent as f32 * cw * 2.0 + quote;
                let job = render_job(r, &line.text, &line.spans, font_size, (rect.right() - x - 16.0).max(cw * 8.0));
                let galley = ui.fonts_mut(|f| f.layout_job(job));
                // Room above a heading.
                let pad = (r.size.unwrap_or(1.0) - 1.0).max(0.0) * font_size * 0.8;
                let h = (galley.size().y + 2.0 + pad).max(self.row_h);
                (Some(galley), Pos2::new(x, y + 1.0 + pad), h)
            }
            None => {
                let job = match &line.fold {
                    Some(f) => LayoutJob::simple_singleline(fold_label(f), self.font.clone(), theme::SUBTLE),
                    None => line_job(line, left, &self.font, self.over(idx)),
                };
                (Some(ui.fonts_mut(|f| f.layout_job(job))), Pos2::new(self.text_x, y + 1.0), self.row_h)
            }
        }
    }

    /// Height of the blocks drawn after `after`, with their gaps.
    fn blocks_height(&self, ui: &mut Ui, after: Option<usize>, heights: &mut Heights) -> f32 {
        let mut h = 0.0;
        for (i, b) in self.blocks.iter().enumerate().filter(|(_, b)| b.after == after) {
            h += card_height(ui, b, self.block_width, self.font_size, self.id.with(("card", i)), heights) + BLOCK_GAP;
        }
        h
    }

    /// Row `idx`'s extent. A Markdown row's height comes from `heights`
    /// when known; otherwise it is measured invisibly and stored.
    fn extent(&self, ui: &mut Ui, idx: usize, heights: &mut Heights) -> Extent {
        let lead = if idx == 0 { self.blocks_height(ui, None, heights) } else { 0.0 };
        let render = self.doc.lines[idx].render.as_deref();
        let markdown = render.is_some_and(|r| r.markdown.is_some());
        let row = match heights.rows.get(&idx).filter(|_| markdown) {
            Some(h) => *h,
            // A text (or fold) row is always one row tall.
            None if render.is_none() => self.row_h,
            None => {
                let h = self.layout_row(ui, idx, self.rect.top(), 0, false).2;
                if markdown {
                    heights.rows.insert(idx, h);
                }
                h
            }
        };
        let field = match self.editor {
            Some((end, h)) if end == idx => h + EDITOR_GAP,
            _ => 0.0,
        };
        let below = self.blocks_height(ui, Some(idx), heights);
        Extent { lead, row, unit: lead + row + field + below }
    }
}

impl CodeView<'_> {
    pub fn show(self, ui: &mut Ui) -> Output {
        let CodeView { doc, vim, state, blocks, mut editor, focused, font_size, two_gutters, relative, overlay, id } = self;
        let mut out = Output::default();
        let rect = ui.available_rect_before_wrap();
        let resp = ui.interact(rect, id.with("area"), Sense::click());
        let font = FontId::monospace(font_size);
        let (cw, ch) = ui.ctx().fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let row_h = ch + 2.0;
        let n = doc.lines.len();
        let height = rect.height();
        state.rows = ((height / row_h).floor() as usize).max(1);
        let md_key = (n, rect.width().to_bits(), font_size.to_bits());
        if state.md_key != md_key {
            state.heights = Heights::default();
            state.md_key = md_key;
        }

        let editor_id = id.with("editor");
        let editor_focused = ui.memory(|m| m.has_focus(editor_id));
        let lines = doc.texts();

        let max_no = doc.lines.iter().filter_map(|l| l.new_no.max(l.old_no)).max().unwrap_or(1);
        let digits = max_no.to_string().len().max(3) as f32;
        // A sign column only when some line has a sign.
        let sign_w = if overlay.is_some_and(|o| o.lines.values().any(|l| l.sign.is_some())) { cw + 6.0 } else { 0.0 };
        let one_gutter = digits * cw + 12.0;
        let gutter = sign_w + if two_gutters { one_gutter * 2.0 } else { one_gutter };
        let text_x = rect.left() + gutter + 6.0;
        let visible_cols = (((rect.right() - text_x) / cw).floor() as usize).max(8);
        let block_width = (rect.width() - gutter - 24.0).min(CARD_COLS * cw + CARD_PAD_X * 2.0);
        let shown: HashSet<u64> = blocks.iter().map(|b| card_key(b, block_width)).collect();
        state.heights.cards.retain(|k, _| shown.contains(k));
        if editor.is_none() {
            state.editor_h = None;
        }
        let editor_h = state.editor_h.unwrap_or(EDITOR_HEIGHT);
        let geo = Geometry {
            doc,
            blocks,
            editor: editor.as_deref().map(|ed| (ed.end, editor_h)),
            font: font.clone(),
            font_size,
            cw,
            row_h,
            rect,
            text_x,
            block_width,
            overlay,
            id,
        };
        let before = (state.top, state.offset);

        // ---- keys -> vim
        if focused && !editor_focused && n > 0 {
            let mut keys = Vec::new();
            for key in collect_keys(ui) {
                // A row taller than the view (a whole rendered document)
                // scrolls under the motion keys until its edge shows.
                let cur = vim.line.min(n - 1);
                let row = rows_in_view(state.top, state.offset, height, n, &mut |i| geo.extent(ui, i, &mut state.heights)).into_iter().find(|r| r.0 == cur);
                let delta = row.filter(|r| r.2 > height).and_then(|(_, top, h)| {
                    let (at_top, at_bottom) = (top >= -0.5, top + h <= height + 0.5);
                    let held_g = std::mem::take(&mut state.g_pending);
                    let d = match key {
                        VimKey::Char('j') | VimKey::Down => row_h * 2.0,
                        VimKey::Char('k') | VimKey::Up => -row_h * 2.0,
                        VimKey::Ctrl('e') => row_h,
                        VimKey::Ctrl('y') => -row_h,
                        VimKey::Ctrl('d') => height / 2.0,
                        VimKey::Ctrl('u') => -height / 2.0,
                        VimKey::Ctrl('f') | VimKey::PageDown => height,
                        VimKey::Ctrl('b') | VimKey::PageUp => -height,
                        VimKey::Char('G') => h,
                        VimKey::Char('g') if held_g => -h,
                        VimKey::Char('g') => {
                            state.g_pending = true;
                            return Some(0.0);
                        }
                        _ => {
                            if held_g {
                                keys.push(VimKey::Char('g'));
                            }
                            return None;
                        }
                    };
                    // At the row's edge the motion leaves it, as on any row.
                    if (d > 0.0 && at_bottom) || (d < 0.0 && at_top) {
                        if held_g {
                            keys.push(VimKey::Char('g'));
                        }
                        return None;
                    }
                    // No further than the row's edges.
                    Some(d.clamp(top.min(0.0), (top + h - height).max(0.0)))
                });
                match delta {
                    Some(d) => (state.top, state.offset) = scroll_px(state.top, state.offset, d, height, n, &mut |i| geo.extent(ui, i, &mut state.heights)),
                    None => keys.push(key),
                }
            }
            for key in keys.drain(..) {
                let acts = vim.key(key, &lines, Viewport { top: state.top, rows: state.rows });
                for a in acts {
                    match a {
                        // Keys scroll by whole rows.
                        VimAction::ScrollBy(d) => state.scroll_to((state.top as isize + d).clamp(0, n as isize - 1) as usize),
                        VimAction::Center | VimAction::Top | VimAction::Bottom => {
                            let cur = vim.line.min(n - 1);
                            let mut ext = |i| geo.extent(ui, i, &mut state.heights);
                            let row = ext(cur).row;
                            let y = match a {
                                VimAction::Center => (height - row) / 2.0,
                                VimAction::Top => 0.0,
                                _ => height - row,
                            };
                            (state.top, state.offset) = anchor(cur, y.max(0.0), &mut ext);
                        }
                        other => out.actions.push(other),
                    }
                }
            }
        }

        // ---- wheel (pixels; egui already eases them). With a zero delta this
        // still settles a unit that shrank or a doc that got shorter.
        let (mut dy, mut dx) = (0.0, 0.0);
        if resp.hovered() || resp.contains_pointer() {
            dy = ui.input(|i| i.smooth_scroll_delta.y);
            dx = ui.input(|i| i.smooth_scroll_delta.x);
        }
        (state.top, state.offset) = scroll_px(state.top, state.offset, -dy, height, n, &mut |i| geo.extent(ui, i, &mut state.heights));
        if dx.abs() > 4.0 {
            state.left = (state.left as isize - (dx / cw) as isize).max(0) as usize;
        }

        // ---- who leads: a moved (or lost) cursor pulls the view, a moved view
        // pulls the cursor onto its nearest fully visible row.
        if n > 0 {
            let cur = vim.line.min(n - 1);
            let view_moved = (state.top, state.offset) != before;
            let just_opened = editor.as_deref().filter(|ed| ed.needs_focus);
            let cursor_moved = state.last_line != Some(vim.line) || just_opened.is_some();
            let mut ext = |i| geo.extent(ui, i, &mut state.heights);
            let rows = rows_in_view(state.top, state.offset, height, n, &mut ext);
            if view_moved && !cursor_moved {
                let line = nearest_visible(&rows, cur, height);
                if line != vim.line {
                    vim.set_cursor(line, vim.col);
                }
            } else if cursor_moved || !rows.iter().any(|r| r.0 == cur) {
                let (idx, need) = match just_opened {
                    Some(ed) => {
                        let i = ed.end.min(n - 1);
                        let e = ext(i);
                        // A follow-up field is under the thread: show the thread too.
                        (i, if ed.follow_up { e.unit - e.lead } else { e.row + editor_h + EDITOR_GAP })
                    }
                    None => (cur, ext(cur).row),
                };
                (state.top, state.offset) = follow(state.top, state.offset, height, n, idx, need, &mut ext);
            }
        } else {
            (state.top, state.offset) = (0, 0.0);
        }

        let cursor_rendered = doc.lines.get(vim.line).is_some_and(|l| l.render.is_some());
        if cursor_rendered {
            // Rendered rows wrap; they never scroll sideways.
        } else if vim.col < state.left {
            state.left = vim.col;
        } else if vim.col >= state.left + visible_cols {
            state.left = vim.col + 1 - visible_cols;
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::ZERO, theme::BG);

        let visual = vim.anchor.map(|(al, ac, linewise)| {
            let (s, e) = if (al, ac) <= (vim.line, vim.col) { ((al, ac), (vim.line, vim.col)) } else { ((vim.line, vim.col), (al, ac)) };
            (s, e, linewise)
        });

        // The top unit starts `offset` above the view; the painter clips it.
        let mut y = rect.top() - state.offset;
        let mut row_hits: Vec<(Rect, usize, Hit)> = Vec::new();
        let mut editor_rect: Option<Rect> = None;

        let draw_blocks = |after: Option<usize>, y: &mut f32, ui: &mut Ui, heights: &mut Heights| {
            for (i, b) in blocks.iter().enumerate().filter(|(_, b)| b.after == after) {
                if *y > rect.bottom() {
                    break;
                }
                let card_id = id.with(("card", i));
                let h = card_height(ui, b, block_width, font_size, card_id, heights);
                let painted = draw_card(ui, Some(&painter), b, Pos2::new(rect.left() + gutter + 10.0, *y + BLOCK_TOP), block_width, font_size, card_id, h);
                // Painting refreshes the height: a table (an egui grid) settles its
                // size over a frame or two.
                if (painted - h).abs() > 0.5 {
                    heights.cards.insert(card_key(b, block_width), painted);
                    ui.ctx().request_repaint();
                }
                *y += h + BLOCK_GAP;
            }
        };

        if state.top == 0 {
            draw_blocks(None, &mut y, ui, &mut state.heights);
        }
        for idx in state.top..n {
            if y > rect.bottom() {
                break;
            }
            let line = &doc.lines[idx];
            let rendered = line.render.as_deref();
            let over = geo.over(idx);
            // Backgrounds go under the text, which a Markdown row draws while
            // it is laid out.
            let under = painter.add(Shape::Noop);
            let mut under_shapes: Vec<Shape> = Vec::new();
            // Layout first: a rendered row wraps, so it may be taller than a text row.
            let (galley, text_pos, row_height) = geo.layout_row(ui, idx, y, state.left, true);
            if rendered.is_some_and(|r| r.markdown.is_some()) {
                state.heights.rows.insert(idx, row_height);
            }
            let row = Rect::from_min_size(Pos2::new(rect.left(), y), Vec2::new(rect.width(), row_height));
            row_hits.push((
                row,
                idx,
                match (&galley, rendered) {
                    (_, None) => Hit::Text,
                    (Some(g), Some(_)) => Hit::Galley(text_pos, g.clone()),
                    (None, Some(_)) => Hit::Block,
                },
            ));
            let bg = match line.kind {
                LineKind::Added => Some(theme::ADDED_BG),
                LineKind::Removed => Some(theme::REMOVED_BG),
                LineKind::Normal => None,
            };
            if let Some(bg) = bg {
                under_shapes.push(Shape::rect_filled(row, CornerRadius::ZERO, bg));
                // A bar down the left edge marks the change, as on GitHub.
                let bar = if line.kind == LineKind::Added { theme::GREEN } else { theme::RED };
                under_shapes.push(Shape::rect_filled(Rect::from_min_size(row.min, Vec2::new(3.0, row_height)), CornerRadius::ZERO, bar.gamma_multiply(0.8)));
            }
            if line.fold.is_some() {
                under_shapes.push(Shape::rect_filled(row, CornerRadius::ZERO, theme::PANEL));
            }
            match rendered.and_then(|r| r.block.as_deref()) {
                Some("code") => {
                    let r = Rect::from_min_max(Pos2::new(text_pos.x - 6.0, y), Pos2::new(rect.right() - 10.0, y + row_height));
                    under_shapes.push(Shape::rect_filled(r, CornerRadius::ZERO, CODE_BG));
                }
                Some("quote") => {
                    under_shapes.push(Shape::rect_filled(Rect::from_min_size(Pos2::new(text_pos.x - cw * 1.2, y), Vec2::new(3.0, row_height)), CornerRadius::ZERO, theme::DIM));
                }
                Some("rule") => {
                    painter.hline(text_pos.x..=rect.right() - 16.0, y + row_height / 2.0, Stroke::new(1.0, theme::BORDER));
                }
                _ => {}
            }
            if idx == vim.line {
                under_shapes.push(Shape::rect_filled(row, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(0x7a, 0xa2, 0xf7, 18)));
            }
            // Visual selection (whole rows on rendered rows).
            if let Some(((sl, sc), (el, ec), linewise)) = visual {
                if idx >= sl && idx <= el && rendered.is_some() {
                    let r = Rect::from_min_max(Pos2::new(text_pos.x - 4.0, y), Pos2::new(rect.right() - 10.0, y + row_height));
                    under_shapes.push(Shape::rect_filled(r, CornerRadius::ZERO, Color32::from_rgb(0x33, 0x46, 0x7c).gamma_multiply(0.8)));
                } else if idx >= sl && idx <= el {
                    let len = line.text.chars().count().max(1);
                    let (c0, c1) = if linewise {
                        (0, len)
                    } else {
                        (if idx == sl { sc } else { 0 }, if idx == el { ec + 1 } else { len })
                    };
                    let x0 = text_x + (c0.saturating_sub(state.left)) as f32 * cw;
                    let x1 = text_x + (c1.saturating_sub(state.left)) as f32 * cw;
                    under_shapes.push(Shape::rect_filled(
                        Rect::from_min_max(Pos2::new(x0, y), Pos2::new(x1.max(x0 + cw), y + row_h)),
                        CornerRadius::ZERO,
                        Color32::from_rgb(0x33, 0x46, 0x7c),
                    ));
                }
            }
            painter.set(under, Shape::Vec(under_shapes));
            // Gutter numbers.
            let num_color = if idx == vim.line { theme::TEXT } else { theme::DIM };
            let small = FontId::monospace(font_size - 1.0);
            // The current-file gutter; relative rows count doc rows, like vim motions.
            let number = if relative && idx != vim.line {
                Some(idx.abs_diff(vim.line))
            } else {
                line.new_no.or(if relative { line.old_no } else { None })
            };
            // Numbers and signs line up with the text's first row.
            let ty = text_pos.y;
            if two_gutters {
                if let Some(o) = line.old_no.filter(|_| line.kind != LineKind::Added) {
                    painter.text(Pos2::new(rect.left() + sign_w + one_gutter - 6.0, ty), egui::Align2::RIGHT_TOP, o, small.clone(), num_color);
                }
                if let Some(nn) = number {
                    painter.text(Pos2::new(rect.left() + gutter - 6.0, ty), egui::Align2::RIGHT_TOP, nn, small.clone(), num_color);
                }
                let marker = match line.kind {
                    LineKind::Added => "+",
                    LineKind::Removed => "-",
                    LineKind::Normal => "",
                };
                painter.text(Pos2::new(rect.left() + gutter + 1.0, ty), egui::Align2::LEFT_TOP, marker, small, if line.kind == LineKind::Added { theme::GREEN } else { theme::RED });
            } else if let Some(nn) = number {
                painter.text(Pos2::new(rect.left() + gutter - 6.0, ty), egui::Align2::RIGHT_TOP, nn, small, num_color);
            }
            if let Some((c, color)) = over.and_then(|o| o.sign) {
                painter.text(Pos2::new(rect.left() + 3.0, ty), egui::Align2::LEFT_TOP, c, FontId::monospace(font_size - 1.0), color);
            }
            // Text (a Markdown row drew its own).
            if let Some(g) = galley.as_ref().filter(|_| rendered.is_none_or(|r| r.block.as_deref() != Some("rule"))) {
                painter.galley(text_pos, g.clone(), theme::TEXT);
            }
            let after_x = text_x + (line.text.chars().count().saturating_sub(state.left) + 2) as f32 * cw;
            if rendered.is_some() {
                // Columns of plugin decorations don't map onto a rendering.
            } else if let Some(suffix) = &line.suffix {
                painter.text(Pos2::new(after_x, y + 1.0), egui::Align2::LEFT_TOP, suffix, FontId::proportional(font_size - 0.5), theme::DIM);
            } else if let Some((text, color)) = over.and_then(|o| o.virt.as_ref()) {
                painter.text(Pos2::new(after_x, y + 1.0), egui::Align2::LEFT_TOP, text, FontId::proportional(font_size - 0.5), color.gamma_multiply(0.75));
            }
            // Cursor: a bar in a rendered row (beside a Markdown row), a block in a text row.
            if idx == vim.line && rendered.is_some() {
                let bar = match &galley {
                    Some(g) => {
                        let at = g.pos_from_cursor(CCursor::new(vim.col)).translate(text_pos.to_vec2());
                        Rect::from_min_size(at.min, Vec2::new(2.0, at.height().max(ch)))
                    }
                    None => Rect::from_min_size(Pos2::new(text_pos.x - 7.0, y + 3.0), Vec2::new(2.0, row_height - 6.0)),
                };
                painter.rect_filled(bar, CornerRadius::ZERO, if focused && !editor_focused { theme::TEXT.gamma_multiply(0.9) } else { theme::DIM });
            } else if idx == vim.line && vim.col >= state.left {
                let x = text_x + (vim.col - state.left) as f32 * cw;
                let r = Rect::from_min_size(Pos2::new(x, y + 1.0), Vec2::new(cw, ch));
                if focused && !editor_focused {
                    painter.rect_filled(r, CornerRadius::ZERO, theme::TEXT.gamma_multiply(0.8));
                    if let Some(c) = line.text.chars().nth(vim.col).filter(|c| !c.is_whitespace()) {
                        painter.text(r.min, egui::Align2::LEFT_TOP, c, font.clone(), theme::BG);
                    }
                } else {
                    painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, theme::DIM), egui::StrokeKind::Inside);
                }
            }
            y += row_height;
            // The comment field goes right under its line, above the line's
            // blocks (a long thread would push it out of view); a follow-up
            // field under them, like the next message of the thread.
            let field = editor.as_deref().filter(|ed| ed.end == idx).map(|ed| ed.follow_up);
            let mut place_field = |y: &mut f32| {
                editor_rect = Some(Rect::from_min_size(Pos2::new(rect.left() + gutter + 10.0, *y + BLOCK_TOP), Vec2::new(block_width, editor_h)));
                *y += editor_h + EDITOR_GAP;
            };
            if field == Some(false) {
                place_field(&mut y);
            }
            draw_blocks(Some(idx), &mut y, ui, &mut state.heights);
            if field == Some(true) {
                place_field(&mut y);
            }
        }
        if n == 0 {
            painter.text(Pos2::new(text_x, rect.top() + 8.0), egui::Align2::LEFT_TOP, "(empty)", font.clone(), theme::DIM);
        }

        // ---- editor: it slides under the edges like a block. A focused field
        // scrolled out of view is still laid out (out of sight), since egui
        // drops the focus of a widget that skips a frame.
        if let Some(ed) = editor.as_deref_mut() {
            let er = editor_rect
                .filter(|er| er.intersects(rect))
                .or_else(|| editor_focused.then(|| Rect::from_min_size(Pos2::new(rect.left() + gutter + 10.0, rect.bottom() + 10.0), Vec2::new(block_width, editor_h))));
            if let Some(er) = er {
                painter.rect_filled(er.expand(3.0), 5.0, theme::PANEL);
                painter.rect_stroke(er.expand(3.0), 5.0, Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Outside);
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(er));
                child.set_clip_rect(rect.intersect(ui.clip_rect()));
                child.label(egui::RichText::new(&ed.label).font(theme::ui_font(font_size - 1.0)).color(theme::ACCENT));
                let te = egui::TextEdit::multiline(&mut ed.text)
                    .id(editor_id)
                    .hint_text(if ed.ask { "Ask about this code…" } else { "Write a comment…" })
                    .desired_rows(3)
                    .desired_width(er.width())
                    .font(FontId::proportional(font_size))
                    .background_color(theme::BG)
                    .text_color(theme::TEXT);
                // Taken before the field sees it, which would add a new line
                // (`consume_key` ignores Shift, so ⇧Enter is let through first).
                if ed.ask && editor_focused && ui.input_mut(|i| !i.modifiers.shift && i.consume_key(Modifiers::NONE, Key::Enter)) {
                    out.submit_editor = true;
                }
                let r = child.add(te);
                // The field grows with its text: the rows below move down next frame.
                let h = child.min_rect().height();
                if state.editor_h != Some(h) {
                    state.editor_h = Some(h);
                    ui.ctx().request_repaint();
                }
                if ed.needs_focus {
                    r.request_focus();
                    ed.needs_focus = false;
                }
                if (r.has_focus() || r.lost_focus()) && ui.input(|i| i.key_pressed(Key::Escape)) {
                    out.cancel_editor = true;
                }
            }
        }

        // ---- mouse
        if resp.clicked_by(egui::PointerButton::Primary) {
            out.clicked = true;
            if let Some(pos) = resp.interact_pointer_pos() {
                if let Some((_, idx, hit)) = row_hits.iter().find(|(r, ..)| r.contains(pos)) {
                    let col = match hit {
                        Hit::Galley(at, galley) => galley.cursor_from_pos(pos - *at).index.0,
                        Hit::Block => 0,
                        Hit::Text => (((pos.x - text_x) / cw).max(0.0) as usize) + state.left,
                    };
                    vim.set_cursor(*idx, col.min(doc.lines[*idx].text.chars().count().saturating_sub(1)));
                }
            }
        }
        state.last_line = Some(vim.line);
        ui.advance_cursor_after_rect(rect);
        out.urls = markdown::drain_open_urls(ui.ctx());
        out
    }
}

/// Moves the view `dy` pixels (down when positive) from `(top, offset)` and
/// returns the new `(top, offset)`, with `offset` inside the top unit. Stops at
/// the start, and at the end once the last row is at the top edge or its unit's
/// end at the bottom edge, whichever scrolls further.
fn scroll_px(top: usize, offset: f32, dy: f32, height: f32, n: usize, extent: &mut dyn FnMut(usize) -> Extent) -> (usize, f32) {
    if n == 0 {
        return (0, 0.0);
    }
    let (mut top, mut offset) = (top.min(n - 1), offset + dy);
    while offset < 0.0 {
        if top == 0 {
            offset = 0.0;
            break;
        }
        top -= 1;
        offset += extent(top).unit;
    }
    while top + 1 < n {
        let unit = extent(top).unit;
        if offset < unit {
            break;
        }
        offset -= unit;
        top += 1;
    }
    if top + 1 == n {
        let e = extent(top);
        offset = offset.min(e.lead.max(e.unit - height).max(0.0));
    }
    (top, offset)
}

/// The `(top, offset)` that puts the top of row `idx` `y` pixels below the
/// view's top edge (clamped: the view can't start above row 0).
fn anchor(idx: usize, y: f32, extent: &mut dyn FnMut(usize) -> Extent) -> (usize, f32) {
    // The unit starts above its row by the file-level blocks.
    let mut y = y - extent(idx).lead;
    let mut i = idx;
    while y > 0.0 && i > 0 {
        i -= 1;
        y -= extent(i).unit;
    }
    if y > 0.0 { (0, 0.0) } else { (i, -y) }
}

/// Scrolls the least needed to show `need` pixels from the top of row `idx`
/// inside a view `height` tall; a row taller than the view goes to the top.
/// Going up to row 0 also shows the file-level blocks above it, if they fit.
fn follow(top: usize, offset: f32, height: f32, n: usize, idx: usize, need: f32, extent: &mut dyn FnMut(usize) -> Extent) -> (usize, f32) {
    let rows = rows_in_view(top, offset, height, n, extent);
    let above = match rows.iter().find(|r| r.0 == idx) {
        Some(&(_, y, _)) if y >= 0.0 && y + need <= height => return (top, offset),
        Some(&(_, y, _)) => y < 0.0,
        None => rows.first().map_or(idx < top, |r| idx < r.0),
    };
    let y = if !above {
        (height - need).max(0.0)
    } else if idx == 0 {
        extent(0).lead.min((height - need).max(0.0))
    } else {
        0.0
    };
    anchor(idx, y, extent)
}

/// The rows whose unit (row, comment field, blocks) has some pixel on screen,
/// top to bottom: `(idx, row top, row height)`, with y measured from the
/// view's top edge (negative above it).
fn rows_in_view(top: usize, offset: f32, height: f32, n: usize, extent: &mut dyn FnMut(usize) -> Extent) -> Vec<(usize, f32, f32)> {
    let mut out = Vec::new();
    let mut y = -offset;
    for idx in top..n {
        if y >= height {
            break;
        }
        let e = extent(idx);
        if y + e.unit > 0.0 {
            out.push((idx, y + e.lead, e.row));
        }
        y += e.unit;
    }
    out
}

/// Where the cursor goes when the view moved under it: `cur` if its row is
/// fully on screen, else the nearest row that is; with none (a row or comment
/// taller than the view), `cur` if its unit shows, else the top row.
fn nearest_visible(rows: &[(usize, f32, f32)], cur: usize, height: f32) -> usize {
    let full: Vec<usize> = rows.iter().filter(|(_, y, h)| *y >= 0.0 && y + h <= height).map(|r| r.0).collect();
    match (full.first(), full.last()) {
        (Some(&first), _) if cur < first => first,
        (_, Some(&last)) if cur > last => last,
        (Some(_), _) => cur,
        _ if rows.iter().any(|r| r.0 == cur) => cur,
        _ => rows.first().map_or(cur, |r| r.0),
    }
}

/// Layout for one line from its spans, starting at char column `left`.
/// The text of a fold row.
fn fold_label(f: &Fold) -> String {
    format!("⋯  {} unchanged lines  ·  zo shows them", f.last + 1 - f.first)
}

fn line_job(line: &super::doc::DocLine, left: usize, font: &FontId, over: Option<&LineOverlay>) -> LayoutJob {
    let mut job = LayoutJob::default();
    let text = &line.text;
    let start = text.char_indices().nth(left).map(|(b, _)| b).unwrap_or(text.len());
    let base = if line.kind == LineKind::Removed && line.spans.is_empty() { theme::TEXT.gamma_multiply(0.8) } else { theme::TEXT };
    // Segment boundaries from every span edge; each segment takes the last
    // covering color (plugin spans after syntax spans) and any underline.
    let empty = Vec::new();
    let (ospans, ounder) = over.map(|o| (&o.spans, &o.underlines)).unwrap_or((&empty, &empty));
    let mut cuts: Vec<usize> = vec![start, text.len()];
    for (r, _) in line.spans.iter().chain(ospans).chain(ounder) {
        cuts.push(r.start.clamp(start, text.len()));
        cuts.push(r.end.clamp(start, text.len()));
    }
    cuts.sort_unstable();
    cuts.dedup();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b || !text.is_char_boundary(a) || !text.is_char_boundary(b) {
            continue;
        }
        let covers = |r: &std::ops::Range<usize>| r.start <= a && r.end >= b;
        let color = ospans.iter().rev().find(|(r, _)| covers(r)).or_else(|| line.spans.iter().rev().find(|(r, _)| covers(r))).map(|(_, c)| *c).unwrap_or(base);
        let underline = ounder.iter().rev().find(|(r, _)| covers(r)).map(|(_, c)| Stroke::new(1.5, *c)).unwrap_or(Stroke::NONE);
        job.append(&text[a..b], 0.0, TextFormat { font_id: font.clone(), color, underline, ..Default::default() });
    }
    job
}

/// Layout for a rendered row: proportional text (monospace in code) in the
/// row's and its spans' styles, wrapped at `wrap`. `syntax` colors (byte
/// ranges) show where no span sets a color.
fn render_job(r: &RenderRow, text: &str, syntax: &[(std::ops::Range<usize>, Color32)], font_size: f32, wrap: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap;
    let size = font_size * r.size.unwrap_or(1.0).clamp(0.5, 3.0);
    let (code_block, quote) = (r.block.as_deref() == Some("code"), r.block.as_deref() == Some("quote"));
    let base = r.group.as_deref().and_then(group_color).unwrap_or(if quote { theme::MUTED } else { theme::TEXT });
    // 1-based character columns → byte offsets.
    let offsets: Vec<usize> = text.char_indices().map(|(b, _)| b).chain([text.len()]).collect();
    let byte = |col: usize| offsets[col.saturating_sub(1).min(offsets.len() - 1)];
    let spans: Vec<(std::ops::Range<usize>, &RenderSpan)> = r.spans.iter().map(|s| (byte(s.col)..s.end_col.map_or(text.len(), byte), s)).collect();
    let mut cuts = vec![0, text.len()];
    for range in spans.iter().map(|(r, _)| r).chain(syntax.iter().map(|(r, _)| r)) {
        cuts.extend([range.start.min(text.len()), range.end.min(text.len())]);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let plain = if code_block { FontId::monospace(size) } else if r.bold { theme::ui_font_bold(size) } else { theme::ui_font(size) };
    if text.is_empty() {
        // Still one row of the row's font, for its height and the cursor.
        job.append("", 0.0, TextFormat { font_id: plain, color: base, ..Default::default() });
        return job;
    }
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b || !text.is_char_boundary(a) || !text.is_char_boundary(b) {
            continue;
        }
        let on: Vec<&RenderSpan> = spans.iter().filter(|(range, _)| range.start <= a && range.end >= b).map(|(_, s)| *s).collect();
        let any = |f: fn(&RenderSpan) -> bool| on.iter().any(|s| f(s));
        let code = !code_block && any(|s| s.code);
        let font_id = if code {
            FontId::monospace(size * 0.9)
        } else if !code_block && any(|s| s.bold) {
            theme::ui_font_bold(size)
        } else {
            plain.clone()
        };
        let color = on
            .iter()
            .rev()
            .find_map(|s| s.group.as_deref().and_then(group_color))
            .or_else(|| syntax.iter().rev().find(|(range, _)| range.start <= a && range.end >= b).map(|(_, c)| *c))
            .unwrap_or(base);
        let stroke = |yes: bool| if yes { Stroke::new(1.0, color) } else { Stroke::NONE };
        job.append(
            &text[a..b],
            0.0,
            TextFormat {
                font_id,
                color,
                background: if code { CODE_BG } else { Color32::TRANSPARENT },
                italics: quote || any(|s| s.italic),
                underline: stroke(any(|s| s.underline)),
                strikethrough: stroke(any(|s| s.strike)),
                ..Default::default()
            },
        );
    }
    job
}

/// Card `b`'s height at `width`: measured invisibly once, then from `heights`.
fn card_height(ui: &mut Ui, b: &Block, width: f32, font_size: f32, id: egui::Id, heights: &mut Heights) -> f32 {
    let key = card_key(b, width);
    if let Some(h) = heights.cards.get(&key) {
        return *h;
    }
    let h = draw_card(ui, None, b, ui.max_rect().min, width, font_size, id, 0.0);
    heights.cards.insert(key, h);
    h
}

/// What a card's height depends on: its text, its kind and the width
/// (`dim` only fades it).
fn card_key(b: &Block, width: f32) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (&b.title, &b.body, &b.repo, &b.path, b.question, width.to_bits()).hash(&mut h);
    for r in &b.replies {
        (&r.title, &r.body).hash(&mut h);
    }
    h.finish()
}

/// Paints card `b` at `at`, `height` tall, with `painter`; without one it
/// only lays the card out. Returns its laid-out height. Comments carry a
/// small colored title (who, which lines, state), a question is the heading
/// of its answer; then the body, and each reply under a faint rule and its
/// own header, all as Markdown.
#[allow(clippy::too_many_arguments)]
fn draw_card(ui: &mut Ui, painter: Option<&egui::Painter>, b: &Block, at: Pos2, width: f32, font_size: f32, id: egui::Id, height: f32) -> f32 {
    let inner = width - CARD_PAD_X * 2.0;
    let (left, right) = (at.x + CARD_PAD_X, at.x + width - CARD_PAD_X);
    if let Some(p) = painter {
        // Faded cards (thinking, resolved) still sit a step above the background.
        let r = Rect::from_min_size(at, Vec2::new(width, height));
        p.rect_filled(r, 5.0, if b.dim { theme::HOVER } else { theme::SELECTED });
        p.rect_filled(Rect::from_min_size(r.min, Vec2::new(3.0, height)), 2.0, b.color);
    }
    let hint = b.question.then(|| ui.fonts_mut(|f| f.layout_no_wrap("Esc closes".into(), theme::ui_font(font_size - 2.0), theme::SUBTLE)));
    let hint_w = hint.as_ref().map_or(0.0, |h| h.size().x + CARD_PAD_X);
    let (title_font, title_color) = if b.question { (theme::ui_font_semibold(font_size), theme::TEXT) } else { (theme::ui_font(font_size - 1.5), b.color) };
    let title = ui.fonts_mut(|f| f.layout(b.title.clone(), title_font, title_color, inner - hint_w));
    let mut y = at.y + CARD_PAD_Y + title.size().y;
    if let Some(p) = painter {
        let first_row = title.rows.first().map_or(0.0, |row| row.height());
        if let Some(hint) = hint {
            let hy = at.y + CARD_PAD_Y + (first_row - hint.size().y) / 2.0;
            p.galley(Pos2::new(right - hint.size().x, hy), hint, theme::SUBTLE);
        }
        p.galley(Pos2::new(left, at.y + CARD_PAD_Y), title, theme::TEXT);
    }
    let rule = |y: &mut f32| {
        *y += PART_GAP;
        if let Some(p) = painter {
            p.hline(left..=right, *y, Stroke::new(1.0, theme::FAINT));
        }
        *y += 1.0 + PART_GAP;
    };
    if b.question {
        rule(&mut y);
    } else {
        y += 2.0;
    }
    let clip = painter.map_or_else(|| ui.clip_rect(), |p| p.clip_rect());
    let text = |src| markdown::Text { src, repo: b.repo.as_deref(), path: b.path.as_deref(), size: font_size - 0.5, dim: b.dim, code_bg: CARD_CODE_BG };
    if !b.body.trim().is_empty() {
        y += markdown::draw(ui, &text(&b.body), Pos2::new(left, y), inner, clip, id.with("body"), painter.is_some());
    }
    for (i, r) in b.replies.iter().enumerate() {
        rule(&mut y);
        let header = ui.fonts_mut(|f| f.layout(r.title.clone(), theme::ui_font(font_size - 1.5), theme::SUBTLE, inner));
        let header_h = header.size().y;
        if let Some(p) = painter {
            p.galley(Pos2::new(left, y), header, theme::SUBTLE);
        }
        y += header_h + 2.0;
        y += markdown::draw(ui, &text(&r.body), Pos2::new(left, y), inner, clip, id.with(("reply", i)), painter.is_some());
    }
    y + CARD_PAD_Y - at.y
}

/// This frame's plain keys as vim keys (Cmd chords were consumed upstream).
pub(crate) fn collect_keys(ui: &Ui) -> Vec<VimKey> {
    let mut out = Vec::new();
    ui.input(|i| {
        for e in &i.events {
            match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } if !modifiers.command || modifiers.ctrl && !modifiers.mac_cmd => {
                    if modifiers.ctrl {
                        let c = match key {
                            Key::CloseBracket => Some(']'),
                            k => k.name().chars().next().filter(|_| k.name().len() == 1).map(|c| c.to_ascii_lowercase()),
                        };
                        if let Some(c) = c {
                            out.push(VimKey::Ctrl(c));
                        }
                        continue;
                    }
                    let k = match key {
                        Key::Escape => VimKey::Esc,
                        Key::Enter => VimKey::Enter,
                        Key::Backspace => VimKey::Backspace,
                        Key::ArrowUp => VimKey::Up,
                        Key::ArrowDown => VimKey::Down,
                        Key::ArrowLeft => VimKey::Left,
                        Key::ArrowRight => VimKey::Right,
                        Key::PageUp => VimKey::PageUp,
                        Key::PageDown => VimKey::PageDown,
                        Key::Home => VimKey::Home,
                        Key::End => VimKey::End,
                        _ => continue,
                    };
                    out.push(k);
                }
                egui::Event::Text(t) if !i.modifiers.ctrl && !i.modifiers.mac_cmd => out.extend(t.chars().map(VimKey::Char)),
                _ => {}
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten 20 px rows; row 3 carries a 300 px comment, row 0 a 50 px
    /// file-level block above it.
    fn ext(i: usize) -> Extent {
        let lead = if i == 0 { 50.0 } else { 0.0 };
        let below = if i == 3 { 300.0 } else { 0.0 };
        Extent { lead, row: 20.0, unit: lead + 20.0 + below }
    }

    const N: usize = 10;
    const H: f32 = 200.0;

    #[test]
    fn scroll_px_moves_through_a_tall_comment_pixel_by_pixel() {
        // Row 3's unit starts at 50 + 3 * 20 = 110 px.
        let mut at = (0, 0.0);
        for _ in 0..200 {
            at = scroll_px(at.0, at.1, 1.0, H, N, &mut ext);
        }
        assert_eq!(at, (3, 90.0), "200 px down is 90 px into row 3's comment");
        at = scroll_px(at.0, at.1, 1.0, H, N, &mut ext);
        assert_eq!(at, (3, 91.0));
    }

    #[test]
    fn scroll_px_crosses_units_both_ways_and_stops_at_both_ends() {
        assert_eq!(scroll_px(3, 10.0, 320.0, H, N, &mut ext), (4, 10.0));
        assert_eq!(scroll_px(4, 10.0, -320.0, H, N, &mut ext), (3, 10.0));
        assert_eq!(scroll_px(2, 0.0, -1000.0, H, N, &mut ext), (0, 0.0));
        // The end: the last row reaches the top edge, no further.
        assert_eq!(scroll_px(0, 0.0, 10_000.0, H, N, &mut ext), (N - 1, 0.0));
        // A doc that shrank under the view.
        assert_eq!(scroll_px(40, 5.0, 0.0, H, N, &mut ext), (N - 1, 0.0));
        assert_eq!(scroll_px(4, 5.0, 0.0, H, 0, &mut ext), (0, 0.0));
    }

    #[test]
    fn scroll_px_reaches_the_end_of_a_tall_comment_on_the_last_row() {
        let mut tall_last = |i: usize| Extent { lead: 0.0, row: 20.0, unit: if i == 1 { 520.0 } else { 20.0 } };
        // Its unit's end may reach the bottom edge: 520 - 200.
        assert_eq!(scroll_px(0, 0.0, 10_000.0, H, 2, &mut tall_last), (1, 320.0));
    }

    #[test]
    fn anchor_counts_file_level_blocks_and_clamps_at_the_start() {
        // Row 0's row is 50 px into its unit.
        assert_eq!(anchor(0, 0.0, &mut ext), (0, 50.0));
        assert_eq!(anchor(0, 80.0, &mut ext), (0, 0.0));
        // Row 4 at 100 px: row 3's unit (320) is cut 220 px.
        assert_eq!(anchor(4, 100.0, &mut ext), (3, 220.0));
        assert_eq!(anchor(4, 0.0, &mut ext), (4, 0.0));
    }

    #[test]
    fn follow_scrolls_just_enough_and_leaves_a_visible_row_alone() {
        // Row 2 is on screen at the start.
        assert_eq!(follow(0, 0.0, H, N, 2, 20.0, &mut ext), (0, 0.0));
        // Row 4 is below row 3's comment: it ends up at the bottom edge.
        assert_eq!(follow(0, 0.0, H, N, 4, 20.0, &mut ext), (3, 140.0));
        // Back up to row 1 from below: it goes to the top edge.
        assert_eq!(follow(5, 0.0, H, N, 1, 20.0, &mut ext), (1, 0.0));
        // Up to row 0: the file-level blocks above it show too.
        assert_eq!(follow(5, 0.0, H, N, 0, 20.0, &mut ext), (0, 0.0));
        // A row cut by the top edge comes fully into view.
        assert_eq!(follow(4, 5.0, H, N, 4, 20.0, &mut ext), (4, 0.0));
    }

    #[test]
    fn rows_in_view_includes_rows_cut_by_either_edge() {
        // 200 px into row 3's unit: its row is above the edge, its comment on screen.
        let rows = rows_in_view(3, 200.0, H, N, &mut ext);
        assert_eq!(rows.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(), vec![(3, -200.0), (4, 120.0), (5, 140.0), (6, 160.0), (7, 180.0)]);
        // 10 px into row 1: rows 1 (cut) and 2 show, then row 3 and part of its comment.
        let rows = rows_in_view(1, 10.0, H, N, &mut ext);
        assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(rows[0].1, -10.0);
    }

    #[test]
    fn nearest_visible_prefers_fully_shown_rows() {
        let rows = rows_in_view(1, 10.0, H, N, &mut ext);
        assert_eq!(nearest_visible(&rows, 1, H), 2, "row 1 is cut by the top edge");
        assert_eq!(nearest_visible(&rows, 2, H), 2);
        assert_eq!(nearest_visible(&rows, 8, H), 3);
        // Only row 3's comment on screen: the cursor stays on row 3, or goes there.
        let rows = rows_in_view(3, 100.0, H, N, &mut ext);
        assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), vec![3]);
        assert_eq!(nearest_visible(&rows, 3, H), 3);
        assert_eq!(nearest_visible(&rows, 7, H), 3);
        assert_eq!(nearest_visible(&rows, 1, H), 3);
    }
}
