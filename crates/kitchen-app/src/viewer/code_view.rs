//! Read-only code widget with vim navigation, used by the file viewer and
//! the plan reviewer. It paints a [`Doc`], inline blocks (plan
//! comments) and an optional comment editor, and turns plain
//! keys into [`Vim`] motions while focused. Rendered rows (`kitchen.render`)
//! are drawn in a proportional font and wrap instead of scrolling sideways;
//! a row with Markdown source is drawn by egui_commonmark.

use std::sync::{Arc, Mutex};

use egui::text::{CCursor, LayoutJob, TextFormat};
use egui::{Color32, CornerRadius, FontId, Galley, Key, Pos2, Rect, Sense, Shape, Stroke, Ui, Vec2};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use super::doc::{Doc, LineKind};
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
    pub top: usize,
    pub left: usize,
    /// Text rows that fit, from the last frame.
    pub rows: usize,
    wheel: f32,
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
}

#[derive(Debug, Default)]
pub struct Output {
    pub actions: Vec<VimAction>,
    pub clicked: bool,
    /// Esc pressed inside the editor.
    pub cancel_editor: bool,
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

const EDITOR_HEIGHT: f32 = 118.0;

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
        state.rows = ((rect.height() / row_h).floor() as usize).max(1);

        let editor_id = id.with("editor");
        let editor_focused = ui.memory(|m| m.has_focus(editor_id));
        let lines = doc.texts();

        // ---- keys -> vim
        if focused && !editor_focused && n > 0 {
            for key in collect_keys(ui) {
                let acts = vim.key(key, &lines, Viewport { top: state.top, rows: state.rows });
                for a in acts {
                    match a {
                        VimAction::ScrollBy(d) => state.top = (state.top as isize + d).clamp(0, n.saturating_sub(1) as isize) as usize,
                        VimAction::Center => state.top = vim.line.saturating_sub(state.rows / 2),
                        VimAction::Top => state.top = vim.line,
                        VimAction::Bottom => state.top = (vim.line + 1).saturating_sub(state.rows),
                        other => out.actions.push(other),
                    }
                }
            }
        }

        // ---- wheel
        if resp.hovered() || resp.contains_pointer() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            let dx = ui.input(|i| i.smooth_scroll_delta.x);
            state.wheel -= dy / row_h;
            let whole = state.wheel.trunc() as isize;
            if whole != 0 {
                state.wheel -= whole as f32;
                state.top = (state.top as isize + whole).clamp(0, n.saturating_sub(1) as isize) as usize;
                // Keep the cursor inside the viewport, like vim does.
                if vim.line < state.top {
                    vim.set_cursor(state.top, vim.col);
                } else if vim.line >= state.top + state.rows {
                    vim.set_cursor(state.top + state.rows - 1, vim.col);
                }
            }
            if dx.abs() > 4.0 {
                state.left = (state.left as isize - (dx / cw) as isize).max(0) as usize;
            }
        }

        // ---- keep cursor visible
        if vim.line < state.top {
            state.top = vim.line;
        }
        if vim.line >= state.top + state.rows {
            state.top = vim.line + 1 - state.rows;
        }

        let max_no = doc.lines.iter().filter_map(|l| l.new_no.max(l.old_no)).max().unwrap_or(1);
        let digits = max_no.to_string().len().max(3) as f32;
        // A sign column only when some line has a sign.
        let sign_w = if overlay.is_some_and(|o| o.lines.values().any(|l| l.sign.is_some())) { cw + 6.0 } else { 0.0 };
        let one_gutter = digits * cw + 12.0;
        let gutter = sign_w + if two_gutters { one_gutter * 2.0 } else { one_gutter };
        let text_x = rect.left() + gutter + 6.0;
        let visible_cols = (((rect.right() - text_x) / cw).floor() as usize).max(8);
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
        painter.rect_filled(Rect::from_min_size(rect.min, Vec2::new(gutter, rect.height())), CornerRadius::ZERO, theme::PANEL);

        let visual = vim.anchor.map(|(al, ac, linewise)| {
            let (s, e) = if (al, ac) <= (vim.line, vim.col) { ((al, ac), (vim.line, vim.col)) } else { ((vim.line, vim.col), (al, ac)) };
            (s, e, linewise)
        });

        let mut y = rect.top();
        let mut row_hits: Vec<(Rect, usize, Hit)> = Vec::new();
        let mut cursor_drawn = false;
        let block_width = rect.width() - gutter - 24.0;
        let mut editor_rect: Option<Rect> = None;

        let draw_blocks = |after: Option<usize>, y: &mut f32, ui: &mut Ui| {
            for b in blocks.iter().filter(|b| b.after == after) {
                if *y > rect.bottom() {
                    break;
                }
                *y += draw_block(ui, &painter, b, Pos2::new(rect.left() + gutter + 10.0, *y + 2.0), block_width) + 6.0;
            }
        };

        if state.top == 0 {
            draw_blocks(None, &mut y, ui);
        }
        for idx in state.top..n {
            if y + row_h > rect.bottom() + row_h {
                break;
            }
            let line = &doc.lines[idx];
            let rendered = line.render.as_deref();
            let over = line.new_no.filter(|_| line.kind != LineKind::Removed).and_then(|n| overlay.and_then(|o| o.lines.get(&n)));
            // Backgrounds go under the text, which a Markdown row draws while
            // it is laid out.
            let under = painter.add(Shape::Noop);
            let mut under_shapes: Vec<Shape> = Vec::new();
            // Layout first: a rendered row wraps, so it may be taller than a text row.
            let (galley, text_pos, row_height) = match rendered {
                Some(r) if r.markdown.is_some() => {
                    let at = Pos2::new(text_x + cw * 0.8 + r.indent as f32 * cw * 2.0, y + 3.0);
                    let md = r.markdown.as_deref().unwrap_or_default();
                    let h = draw_markdown(ui, rect, at, rect.right() - at.x - 16.0, md, font_size, id.with(("md", idx)));
                    (None, at, (h + 8.0).max(row_h))
                }
                Some(r) => {
                    let quote = if r.block.as_deref() == Some("quote") { cw * 1.5 } else { 0.0 };
                    // Clear of the diff's +/- marker.
                    let x = text_x + cw * 0.8 + r.indent as f32 * cw * 2.0 + quote;
                    let job = render_job(r, &line.text, &line.spans, font_size, (rect.right() - x - 16.0).max(cw * 8.0));
                    let galley = ui.fonts_mut(|f| f.layout_job(job));
                    // Room above a heading.
                    let pad = (r.size.unwrap_or(1.0) - 1.0).max(0.0) * font_size * 0.8;
                    let h = (galley.size().y + 2.0 + pad).max(row_h);
                    (Some(galley), Pos2::new(x, y + 1.0 + pad), h)
                }
                None => (Some(ui.fonts_mut(|f| f.layout_job(line_job(line, state.left, &font, over)))), Pos2::new(text_x, y + 1.0), row_h),
            };
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
                // A row taller than the view counts as shown once it is at the top.
                cursor_drawn = y + row_height <= rect.bottom() || idx == state.top;
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
                cursor_drawn = y + row_h <= rect.bottom();
            }
            y += row_height;
            // The comment field goes right under its line, above the line's
            // blocks (a long thread would push it out of view).
            if let Some(ed) = editor.as_deref() {
                if ed.end == idx {
                    editor_rect = Some(Rect::from_min_size(Pos2::new(rect.left() + gutter + 10.0, y + 2.0), Vec2::new(block_width, EDITOR_HEIGHT)));
                    y += EDITOR_HEIGHT + 8.0;
                }
            }
            draw_blocks(Some(idx), &mut y, ui);
        }
        if n == 0 {
            painter.text(Pos2::new(text_x, rect.top() + 8.0), egui::Align2::LEFT_TOP, "(empty)", font.clone(), theme::DIM);
        }

        // ---- editor
        if let (Some(ed), Some(er)) = (editor.as_deref_mut(), editor_rect) {
            let er = er.intersect(rect);
            if er.height() > 30.0 {
                painter.rect_filled(er.expand(3.0), 5.0, theme::PANEL);
                painter.rect_stroke(er.expand(3.0), 5.0, Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Outside);
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(er));
                child.label(egui::RichText::new(&ed.label).size(11.0).color(theme::ACCENT));
                let te = egui::TextEdit::multiline(&mut ed.text)
                    .id(editor_id)
                    .hint_text("Write a comment…   ⌘Enter submit · Esc cancel")
                    .desired_rows(4)
                    .desired_width(er.width())
                    .font(FontId::proportional(font_size))
                    .background_color(theme::BG)
                    .text_color(theme::TEXT);
                let r = child.add(te);
                if ed.needs_focus {
                    r.request_focus();
                    ed.needs_focus = false;
                }
                if (r.has_focus() || r.lost_focus()) && ui.input(|i| i.key_pressed(Key::Escape)) {
                    out.cancel_editor = true;
                }
            }
        }

        // ---- scroll correction when blocks pushed the cursor (or the
        // comment field) below the fold
        let editor_hidden = editor.as_deref().is_some_and(|ed| ed.end >= state.top && editor_rect.is_none_or(|r| r.bottom() > rect.bottom()));
        if editor_hidden || (!cursor_drawn && vim.line >= state.top && n > 0 && editor.is_none()) {
            state.top += 1;
            ui.ctx().request_repaint();
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
        ui.advance_cursor_after_rect(rect);
        out
    }
}

/// Layout for one line from its spans, starting at char column `left`.
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
/// and keys stay with vim.
fn draw_markdown(ui: &mut Ui, clip: Rect, at: Pos2, width: f32, md: &str, font_size: f32, id: egui::Id) -> f32 {
    let cache = ui.data_mut(|d| d.get_temp_mut_or_default::<Arc<Mutex<CommonMarkCache>>>(egui::Id::new("commonmark")).clone());
    let max = Rect::from_min_size(at, Vec2::new(width.max(80.0), f32::INFINITY));
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt(id).max_rect(max).layout(egui::Layout::top_down(egui::Align::Min)));
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

fn draw_block(ui: &Ui, painter: &egui::Painter, b: &Block, at: Pos2, width: f32) -> f32 {
    let title_font = FontId::proportional(11.5);
    let body_font = FontId::proportional(12.5);
    let text_color = if b.dim { theme::DIM } else { theme::TEXT };
    let title = ui.fonts_mut(|f| f.layout(b.title.clone(), title_font, b.color, width - 16.0));
    let body = ui.fonts_mut(|f| f.layout(b.body.clone(), body_font, text_color, width - 16.0));
    let h = title.size().y + body.size().y + 12.0;
    let r = Rect::from_min_size(at, Vec2::new(width, h));
    painter.rect_filled(r, 5.0, if b.dim { theme::PANEL } else { theme::SELECTED });
    painter.rect_filled(Rect::from_min_size(r.min, Vec2::new(3.0, h)), 2.0, b.color);
    let th = title.size().y;
    painter.galley(at + Vec2::new(10.0, 5.0), title, b.color);
    painter.galley(at + Vec2::new(10.0, 7.0 + th), body, text_color);
    h
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
