//! Read-only code widget with vim navigation, used by the file viewer and
//! the plan reviewer. It paints a [`Doc`], inline blocks (plan
//! comments) and an optional comment editor, and turns plain
//! keys into [`Vim`] motions while focused. Rendered rows (`kitchen.render`)
//! are drawn in a proportional font and wrap instead of scrolling sideways;
//! a row with Markdown source is drawn by egui_commonmark.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::text::{CCursor, LayoutJob, TextFormat};
use egui::{Color32, CornerRadius, FontId, Galley, Key, Modifiers, Pos2, Rect, Sense, Shape, Stroke, Ui, Vec2};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use super::doc::{Doc, Fold, LineKind};
use super::highlight::group_color;
use super::vim::{Vim, VimAction, VimKey, Viewport};
use crate::plugins::decor::{RenderRow, RenderSpan};
use crate::theme;

/// Background of rendered code (blocks and inline).
const CODE_BG: Color32 = Color32::from_rgb(0x24, 0x28, 0x3b);
/// Body text of rendered Markdown; bold and headings are `theme::TEXT`.
const PROSE: Color32 = Color32::from_rgb(0xc0, 0xca, 0xf5);

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
    /// Heights of Markdown rows by doc row, for the doc length, width and font
    /// size in `md_key`; painting refreshes them, a new key clears them.
    md_heights: HashMap<usize, f32>,
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
#[derive(Debug, Clone)]
pub struct Block {
    /// Doc index it follows; `None` = above the first line (file-level).
    pub after: Option<usize>,
    pub title: String,
    pub body: String,
    pub color: Color32,
    pub dim: bool,
    /// A ⌘⇧A answer: `title` is the question, drawn as the card's heading.
    pub question: bool,
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
        Block { after: Some(after), title: question.to_string(), body, color, dim, question: true }
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
}

#[derive(Debug, Default)]
pub struct Output {
    pub actions: Vec<VimAction>,
    pub clicked: bool,
    /// Esc pressed inside the editor.
    pub cancel_editor: bool,
    /// Enter pressed in a question field.
    pub submit_editor: bool,
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
/// Inside a card: padding around its text, around a code block's text, and
/// between its parts.
const CARD_PAD_X: f32 = 12.0;
const CARD_PAD_Y: f32 = 8.0;
const CODE_PAD: f32 = 6.0;
const PART_GAP: f32 = 6.0;
/// Widest a card gets, in code columns: prose wider than this is hard to read.
const CARD_COLS: f32 = 100.0;
/// Code in a card: fenced blocks on a panel darker than the card, inline
/// spans on a lighter pill.
const CARD_CODE_BG: Color32 = theme::BG;
const CARD_INLINE_BG: Color32 = theme::HOVER;
/// Room an inline code pill takes on either side of its text.
const PILL_PAD_X: f32 = 4.0;
const PILL_PAD_Y: f32 = 1.0;

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
                let md = r.markdown.as_deref().unwrap_or_default();
                let h = draw_markdown(ui, rect, at, rect.right() - at.x - 16.0, md, font_size, self.id.with(("md", idx)), paint);
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
    fn blocks_height(&self, ui: &Ui, after: Option<usize>) -> f32 {
        self.blocks.iter().filter(|b| b.after == after).map(|b| layout_block(ui, b, self.block_width, self.font_size).height + BLOCK_GAP).sum()
    }

    /// Row `idx`'s extent. A Markdown row's height comes from `md_heights`
    /// when known; otherwise it is measured invisibly and stored.
    fn extent(&self, ui: &mut Ui, idx: usize, md_heights: &mut HashMap<usize, f32>) -> Extent {
        let lead = if idx == 0 { self.blocks_height(ui, None) } else { 0.0 };
        let render = self.doc.lines[idx].render.as_deref();
        let markdown = render.is_some_and(|r| r.markdown.is_some());
        let row = match md_heights.get(&idx).filter(|_| markdown) {
            Some(h) => *h,
            // A text (or fold) row is always one row tall.
            None if render.is_none() => self.row_h,
            None => {
                let h = self.layout_row(ui, idx, self.rect.top(), 0, false).2;
                if markdown {
                    md_heights.insert(idx, h);
                }
                h
            }
        };
        let field = match self.editor {
            Some((end, h)) if end == idx => h + EDITOR_GAP,
            _ => 0.0,
        };
        Extent { lead, row, unit: lead + row + field + self.blocks_height(ui, Some(idx)) }
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
            state.md_heights.clear();
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
            for key in collect_keys(ui) {
                let acts = vim.key(key, &lines, Viewport { top: state.top, rows: state.rows });
                for a in acts {
                    match a {
                        // Keys scroll by whole rows.
                        VimAction::ScrollBy(d) => state.scroll_to((state.top as isize + d).clamp(0, n as isize - 1) as usize),
                        VimAction::Center | VimAction::Top | VimAction::Bottom => {
                            let cur = vim.line.min(n - 1);
                            let mut ext = |i| geo.extent(ui, i, &mut state.md_heights);
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
        (state.top, state.offset) = scroll_px(state.top, state.offset, -dy, height, n, &mut |i| geo.extent(ui, i, &mut state.md_heights));
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
            let mut ext = |i| geo.extent(ui, i, &mut state.md_heights);
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
                        (i, ext(i).row + editor_h + EDITOR_GAP)
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

        let draw_blocks = |after: Option<usize>, y: &mut f32, ui: &mut Ui| {
            for b in blocks.iter().filter(|b| b.after == after) {
                if *y > rect.bottom() {
                    break;
                }
                *y += draw_block(ui, &painter, b, Pos2::new(rect.left() + gutter + 10.0, *y + BLOCK_TOP), block_width, font_size) + BLOCK_GAP;
            }
        };

        if state.top == 0 {
            draw_blocks(None, &mut y, ui);
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
                state.md_heights.insert(idx, row_height);
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
            // blocks (a long thread would push it out of view).
            if let Some(ed) = editor.as_deref() {
                if ed.end == idx {
                    editor_rect = Some(Rect::from_min_size(Pos2::new(rect.left() + gutter + 10.0, y + BLOCK_TOP), Vec2::new(block_width, editor_h)));
                    y += editor_h + EDITOR_GAP;
                }
            }
            draw_blocks(Some(idx), &mut y, ui);
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

/// Draws Markdown `md` at `at`, `width` wide, clipped to `clip`; returns its
/// height. Its widgets are disabled, so a click lands on the row (cursor, ⌘M)
/// and keys stay with vim. `paint: false` lays it out invisibly, to measure.
#[allow(clippy::too_many_arguments)]
fn draw_markdown(ui: &mut Ui, clip: Rect, at: Pos2, width: f32, md: &str, font_size: f32, id: egui::Id, paint: bool) -> f32 {
    let cache = ui.data_mut(|d| d.get_temp_mut_or_default::<Arc<Mutex<CommonMarkCache>>>(egui::Id::new("commonmark")).clone());
    let max = Rect::from_min_size(at, Vec2::new(width.max(80.0), f32::INFINITY));
    let mut builder = egui::UiBuilder::new().max_rect(max).layout(egui::Layout::top_down(egui::Align::Min));
    // The measuring copy gets its own ids, so it never clashes with the painted one.
    builder = if paint { builder.id_salt(id) } else { builder.id_salt(id.with("measure")).invisible() };
    let mut child = ui.new_child(builder);
    child.set_clip_rect(clip.intersect(ui.clip_rect()));
    child.set_style(markdown_style(ui.style(), font_size));
    child.disable();
    CommonMarkViewer::new().show(&mut child, &mut cache.lock().unwrap_or_else(|e| e.into_inner()), md);
    child.min_rect().height()
}

/// Fonts and colors for rendered Markdown at the viewer's `font_size`.
fn markdown_style(base: &egui::Style, font_size: f32) -> egui::Style {
    use egui::TextStyle;
    let mut s = base.clone();
    let body = font_size + 1.0;
    s.text_styles = [
        (TextStyle::Small, theme::ui_font(body * 0.85)),
        (TextStyle::Body, theme::ui_font(body)),
        (TextStyle::Button, theme::ui_font(body)),
        (TextStyle::Heading, theme::ui_font_bold(body * 2.0)),
        (TextStyle::Monospace, FontId::monospace(font_size * 0.95)),
    ]
    .into();
    let v = &mut s.visuals;
    // Text colors come from the widget visuals: plain text dimmer than
    // bold and headings (egui draws `strong` as a brighter color).
    v.override_text_color = None;
    v.widgets.noninteractive.fg_stroke.color = PROSE;
    v.widgets.inactive.fg_stroke.color = PROSE;
    v.widgets.active.fg_stroke.color = theme::TEXT;
    v.code_bg_color = CODE_BG;
    v.extreme_bg_color = CODE_BG;
    // Disabled only so it takes no input; drawn at full strength.
    v.disabled_alpha = 1.0;
    s.spacing.item_spacing.y = 5.0;
    s
}

/// A card laid out, shared by painting and measuring. Offsets are from
/// the card's top.
struct CardLayout {
    title: Arc<Galley>,
    /// "Esc closes", right of a question's title.
    hint: Option<Arc<Galley>>,
    /// Where a question's divider goes.
    divider_y: Option<f32>,
    /// The body, top to bottom, with each part's offset.
    parts: Vec<(Part, f32)>,
    height: f32,
}

/// A piece of a card's body.
enum Part {
    /// Text, with the pills behind its inline code (relative to the galley).
    Prose(Arc<Galley>, Vec<Rect>),
    /// A fenced block, drawn on a panel.
    Code(Arc<Galley>),
}

/// Lays out card `b` at `width`. Comments carry a small colored title (who,
/// which lines, state); a question is the heading of its answer. Code in
/// the body, fenced or in backticks, is set in monospace.
fn layout_block(ui: &Ui, b: &Block, width: f32, font_size: f32) -> CardLayout {
    let inner = width - CARD_PAD_X * 2.0;
    let text_color = if b.dim { theme::SUBTLE } else { theme::TEXT };
    let hint = b.question.then(|| ui.fonts_mut(|f| f.layout_no_wrap("Esc closes".into(), theme::ui_font(font_size - 2.0), theme::SUBTLE)));
    let hint_w = hint.as_ref().map_or(0.0, |h| h.size().x + CARD_PAD_X);
    let (title_font, title_color) = if b.question { (theme::ui_font_semibold(font_size), theme::TEXT) } else { (theme::ui_font(font_size - 1.5), b.color) };
    let title = ui.fonts_mut(|f| f.layout(b.title.clone(), title_font, title_color, inner - hint_w));
    let mut y = CARD_PAD_Y + title.size().y;
    let divider_y = b.question.then(|| {
        y += PART_GAP;
        let at = y;
        y += 1.0 + PART_GAP;
        at
    });
    y += if b.question { 0.0 } else { 2.0 };
    let mut parts = Vec::new();
    for (code, text) in split_fences(&b.body) {
        if code {
            let galley = ui.fonts_mut(|f| f.layout(text, FontId::monospace(font_size * 0.92), text_color, inner - CODE_PAD * 2.0));
            y += CODE_PAD;
            let h = galley.size().y;
            parts.push((Part::Code(galley), y));
            y += h + CODE_PAD + PART_GAP;
        } else {
            let size = font_size - 0.5;
            let (plain, code) = (theme::ui_font(size), FontId::monospace(size * 0.95));
            // How far below the prose's baseline code's would sit.
            let lift = (baseline(ui, &plain) - baseline(ui, &code)).max(0.0);
            let (job, spans) = prose_job(&text, plain, code, lift, text_color, inner);
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            let h = galley.size().y;
            let pills = pills(&galley, &spans);
            parts.push((Part::Prose(galley, pills), y));
            y += h + PART_GAP;
        }
    }
    let height = if parts.is_empty() { y } else { y - PART_GAP } + CARD_PAD_Y;
    CardLayout { title, hint, divider_y, parts, height }
}

/// `text` cut at its ``` and ~~~ fences into runs of prose (`false`) and
/// code (`true`, without the fence lines and the fence's indent). Blank
/// lines around a run are dropped; an unclosed fence runs to the end.
fn split_fences(text: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let mut push = |code: bool, lines: &mut Vec<&str>| {
        let run = lines.join("\n");
        let run = if code { run.trim_matches('\n').to_string() } else { run.trim().to_string() };
        if !run.is_empty() {
            out.push((code, run));
        }
        lines.clear();
    };
    // The open fence's marker and indent.
    let mut fence: Option<(&str, usize)> = None;
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let t = line.trim_start_matches(' ');
        let indent = line.len() - t.len();
        match fence {
            None => match ["```", "~~~"].into_iter().find(|m| t.starts_with(m)) {
                Some(m) => {
                    push(false, &mut lines);
                    fence = Some((m, indent));
                }
                None => lines.push(line),
            },
            Some((m, _)) if t.starts_with(m) && t.trim_end().chars().all(|c| m.starts_with(c)) => {
                push(true, &mut lines);
                fence = None;
            }
            Some((_, n)) => lines.push(&line[indent.min(n)..]),
        }
    }
    push(fence.is_some(), &mut lines);
    out
}

/// Distance from the top of a row of `font` to its baseline.
fn baseline(ui: &Ui, font: &FontId) -> f32 {
    let galley = ui.fonts_mut(|f| f.layout_no_wrap("x".into(), font.clone(), Color32::WHITE));
    galley.rows.first().and_then(|r| r.row.glyphs.first()).map_or(0.0, |g| g.pos.y)
}

/// Prose in `plain` with its `backtick` spans in `code`, and the spans'
/// character ranges in the job's text (for their pills). `lift` is how much
/// higher code's baseline sits than the prose's in a row of its own.
fn prose_job(text: &str, plain: FontId, code: FontId, lift: f32, color: Color32, wrap: f32) -> (LayoutJob, Vec<std::ops::Range<usize>>) {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap;
    let line_height = plain.size * 1.45;
    let plain = TextFormat { font_id: plain, color, line_height: Some(line_height), ..Default::default() };
    // Prose sits at the top of its tall row. Code is bottom-aligned in a row
    // `lift` shorter than the prose's, which puts it on the same baseline.
    let code = TextFormat { font_id: code, line_height: Some(line_height - lift), valign: egui::Align::BOTTOM, ..plain.clone() };
    let mut spans = Vec::new();
    let mut chars = 0;
    // The text after a code span needs room for its pill.
    let mut after_code = false;
    let mut push = |job: &mut LayoutJob, text: &str, format: &TextFormat, lead: f32| {
        job.append(text, lead, format.clone());
        let start = chars;
        chars += text.chars().count();
        start..chars
    };
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let before = &rest[..open];
        let lead = if after_code { pill_room(before.chars().next()) } else { 0.0 };
        match after.find(['`', '\n']) {
            Some(len) if len > 0 && after[len..].starts_with('`') => {
                push(&mut job, before, &plain, lead);
                // At the start of a line the pill reaches into the margin, as
                // it does on a wrapped line (egui drops room at a wrap).
                let lead = match before.chars().next_back().or(after_code.then_some('`')) {
                    None | Some('\n') => 0.0,
                    prev => pill_room(prev),
                };
                spans.push(push(&mut job, &after[..len], &code, lead));
                after_code = true;
                rest = &after[len + 1..];
            }
            // An unpaired backtick stays as it is.
            _ => {
                push(&mut job, &rest[..open + 1], &plain, lead);
                after_code = false;
                rest = after;
            }
        }
    }
    let lead = if after_code { pill_room(rest.chars().next()) } else { 0.0 };
    push(&mut job, rest, &plain, lead);
    (job, spans)
}

/// Space between a pill's text and the character `next` to it: a space
/// already parts them (as does the end of the text), punctuation sits a
/// little clear of the pill's edge.
fn pill_room(next: Option<char>) -> f32 {
    match next {
        None | Some(' ' | '\n' | '\t') => PILL_PAD_X,
        Some(_) => PILL_PAD_X + 2.0,
    }
}

/// The pills behind `galley`'s code `spans` (character ranges), one per row
/// a span covers, relative to the galley.
fn pills(galley: &Galley, spans: &[std::ops::Range<usize>]) -> Vec<Rect> {
    let mut out = Vec::new();
    let mut start = 0;
    for row in &galley.rows {
        let glyphs = &row.row.glyphs;
        for s in spans {
            let (lo, hi) = (s.start.max(start), s.end.min(start + glyphs.len()));
            if lo >= hi {
                continue;
            }
            let (first, last) = (&glyphs[lo - start], &glyphs[hi - 1 - start]);
            let top = row.pos.y + first.pos.y - first.font_face_ascent;
            let min = Pos2::new(row.pos.x + first.pos.x - PILL_PAD_X, top - PILL_PAD_Y);
            let max = Pos2::new(row.pos.x + last.pos.x + last.advance_width + PILL_PAD_X, top + first.font_face_height + PILL_PAD_Y);
            out.push(Rect::from_min_max(min, max));
        }
        start += glyphs.len() + usize::from(row.ends_with_newline);
    }
    out
}

fn draw_block(ui: &Ui, painter: &egui::Painter, b: &Block, at: Pos2, width: f32, font_size: f32) -> f32 {
    let l = layout_block(ui, b, width, font_size);
    let r = Rect::from_min_size(at, Vec2::new(width, l.height));
    // Faded cards (thinking, resolved) still sit a step above the background.
    painter.rect_filled(r, 5.0, if b.dim { theme::HOVER } else { theme::SELECTED });
    painter.rect_filled(Rect::from_min_size(r.min, Vec2::new(3.0, l.height)), 2.0, b.color);
    let left = r.left() + CARD_PAD_X;
    let first_row = l.title.rows.first().map_or(0.0, |row| row.height());
    painter.galley(Pos2::new(left, r.top() + CARD_PAD_Y), l.title, theme::TEXT);
    if let Some(hint) = l.hint {
        let y = r.top() + CARD_PAD_Y + (first_row - hint.size().y) / 2.0;
        painter.galley(Pos2::new(r.right() - CARD_PAD_X - hint.size().x, y), hint, theme::SUBTLE);
    }
    if let Some(dy) = l.divider_y {
        painter.hline(left..=r.right() - CARD_PAD_X, r.top() + dy, Stroke::new(1.0, theme::FAINT));
    }
    for (part, dy) in l.parts {
        let at = Pos2::new(left, r.top() + dy);
        match part {
            Part::Code(galley) => {
                let panel = Rect::from_min_size(Pos2::new(left, at.y - CODE_PAD), Vec2::new(width - CARD_PAD_X * 2.0, galley.size().y + CODE_PAD * 2.0));
                painter.rect_filled(panel, 6.0, CARD_CODE_BG);
                painter.galley(at + Vec2::new(CODE_PAD, 0.0), galley, theme::TEXT);
            }
            Part::Prose(galley, pills) => {
                for pill in pills {
                    let pill = pill.translate(at.to_vec2());
                    painter.rect_filled(pill, pill.height() / 2.0, CARD_INLINE_BG);
                }
                painter.galley(at, galley, theme::TEXT);
            }
        }
    }
    l.height
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
    fn split_fences_cuts_code_out_of_prose() {
        let body = "Both are colors:\n\n  ```rust\n  const A: u8 = 1;\n    nested();\n  ```\n\nSo use `A`.\n```\nunclosed";
        assert_eq!(
            split_fences(body),
            vec![
                (false, "Both are colors:".to_string()),
                (true, "const A: u8 = 1;\n  nested();".to_string()),
                (false, "So use `A`.".to_string()),
                (true, "unclosed".to_string()),
            ]
        );
        assert_eq!(split_fences("plain\n\ntext"), vec![(false, "plain\n\ntext".to_string())]);
        // A fence line with text after the marks inside code does not close it.
        assert_eq!(split_fences("~~~\n```inner\n~~~"), vec![(true, "```inner".to_string())]);
    }

    #[test]
    fn prose_job_sets_backtick_spans_in_monospace() {
        // A backtick with no partner on its line stays as it is.
        let (job, spans) = prose_job("use `row_at` here\nlone ` tick\n`a\nb`", theme::ui_font(12.0), FontId::monospace(12.0), 1.0, theme::TEXT, 400.0);
        assert_eq!(spans, vec![4..10], "row_at, in characters of the job's text");
        let runs: Vec<(&str, bool)> = job
            .sections
            .iter()
            .map(|s| (&job.text[s.byte_range.start.0..s.byte_range.end.0], s.format.font_id.family == egui::FontFamily::Monospace))
            .filter(|(t, _)| !t.is_empty())
            .collect();
        assert_eq!(runs, vec![("use ", false), ("row_at", true), (" here\nlone ` tick\n`a\nb`", false)]);
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
