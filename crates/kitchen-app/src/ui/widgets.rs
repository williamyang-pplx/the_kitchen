//! Small pieces of the kitchen's look shared by every view: badges, section
//! labels, key hints, buttons, the frame of dialogs and popups, empty states.

use egui::text::LayoutJob;
use egui::{Color32, CornerRadius, Response, RichText, Sense, TextFormat, Ui};

use crate::engine::PrState;
use crate::theme;

/// Height of the header row atop the viewer and the plan.
pub const HEADER_HEIGHT: f32 = 36.0;

/// A path in monospace: its folders dimmer than its name.
pub fn path_job(path: &str, size: f32) -> LayoutJob {
    let split = path.trim_end_matches('/').rfind('/').map(|i| i + 1).unwrap_or(0);
    let mut job = LayoutJob::default();
    let format = |color| TextFormat { font_id: egui::FontId::monospace(size), color, valign: egui::Align::Center, ..Default::default() };
    job.append(&path[..split], 0.0, format(theme::SUBTLE));
    job.append(&path[split..], 0.0, format(theme::TEXT));
    job
}

/// An uppercase section label with a little tracking ("WORKSPACES").
pub fn section_label(text: &str, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let format = TextFormat { font_id: theme::ui_font_semibold(theme::TEXT_LABEL), color, extra_letter_spacing: 0.6, ..Default::default() };
    job.append(&text.to_uppercase(), 0.0, format);
    job
}

/// GitHub's octicon for a PR in `state`, in GitHub's color for it, filling
/// the square `rect`: git-pull-request (open), its draft and closed forms,
/// and git-merge (merged).
pub fn pr_icon(painter: &egui::Painter, rect: egui::Rect, state: PrState) {
    let color = match state {
        PrState::Open => Color32::from_rgb(0x3f, 0xb9, 0x50),
        PrState::Draft => Color32::from_rgb(0x91, 0x98, 0xa1),
        PrState::Merged => Color32::from_rgb(0xab, 0x7d, 0xf8),
        PrState::Closed => Color32::from_rgb(0xf8, 0x51, 0x49),
    };
    // Drawn on the octicons' 16-unit grid.
    let unit = rect.width() / 16.0;
    let at = |x: f32, y: f32| rect.min + egui::vec2(x, y) * unit;
    let stroke = egui::Stroke::new(1.5 * unit, color);
    let ring = |x: f32, y: f32| painter.circle_stroke(at(x, y), 1.75 * unit, stroke);
    let line = |a: (f32, f32), b: (f32, f32)| painter.line_segment([at(a.0, a.1), at(b.0, b.1)], stroke);
    // Every form starts from the base branch on the left: a ring at each end.
    ring(4.0, 3.5);
    ring(4.0, 12.5);
    line((4.0, 5.25), (4.0, 10.75));
    match state {
        PrState::Merged => {
            // The head curving out of the top ring into the right one.
            ring(12.0, 8.0);
            let curve = [at(4.0, 5.25), at(4.0, 8.0), at(7.0, 8.0), at(10.25, 8.0)];
            painter.add(egui::epaint::CubicBezierShape::from_points_stroke(curve, false, Color32::TRANSPARENT, stroke));
        }
        PrState::Open => {
            // The head, and an arrow from it back to the base.
            ring(12.0, 12.5);
            line((12.0, 10.75), (12.0, 6.5));
            let curve = [at(12.0, 6.5), at(12.0, 3.5), at(9.0, 3.5)];
            painter.add(egui::epaint::QuadraticBezierShape::from_points_stroke(curve, false, Color32::TRANSPARENT, stroke));
            line((10.5, 2.0), (9.0, 3.5));
            line((9.0, 3.5), (10.5, 5.0));
        }
        PrState::Closed => {
            ring(12.0, 12.5);
            line((10.25, 2.25), (13.75, 5.75));
            line((13.75, 2.25), (10.25, 5.75));
        }
        PrState::Draft => {
            ring(12.0, 12.5);
            painter.circle_filled(at(12.0, 8.0), 0.9 * unit, color);
            painter.circle_filled(at(12.0, 4.0), 0.9 * unit, color);
        }
    }
}

/// A small tinted badge: `color` text on a faint wash of it.
pub fn pill(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_string(), theme::ui_font_semibold(theme::TEXT_LABEL), color);
    let pad = egui::vec2(6.0, 2.0);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + 2.0 * pad, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_S), color.gamma_multiply(0.16));
    ui.painter().galley(rect.min + pad, galley, color);
    resp
}

/// `⌘T new  ⌘W close`: keys brighter than what they do.
pub fn key_hints(ui: &mut Ui, hints: &[(&str, &str)]) {
    let mut job = LayoutJob::default();
    let key = TextFormat { font_id: theme::ui_font_semibold(theme::TEXT_SMALL), color: theme::MUTED, ..Default::default() };
    let label = TextFormat { font_id: theme::ui_font(theme::TEXT_SMALL), color: theme::SUBTLE, ..Default::default() };
    for (i, (k, what)) in hints.iter().enumerate() {
        job.append(k, if i == 0 { 0.0 } else { 14.0 }, key.clone());
        job.append(what, 5.0, label.clone());
    }
    ui.label(job);
}

/// The button a dialog submits with.
pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).font(theme::ui_font_semibold(theme::TEXT_BODY)).color(theme::BG)).fill(theme::ACCENT).stroke(egui::Stroke::NONE)
}

/// A frameless text button that lights up on hover; `on` keeps it lit.
pub fn flat_button(ui: &mut Ui, text: &str, on: bool) -> Response {
    let color = if on { theme::ACCENT } else { theme::SUBTLE };
    let galley = ui.painter().layout_no_wrap(text.to_string(), theme::ui_font(theme::TEXT_SMALL), color);
    let pad = egui::vec2(8.0, 4.0);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + 2.0 * pad, Sense::click());
    let fill = if on {
        theme::ACCENT.gamma_multiply(0.14)
    } else if resp.hovered() {
        theme::HOVER
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS), fill);
    let text_color = if resp.hovered() && !on { theme::TEXT } else { color };
    ui.painter().galley_with_override_text_color(rect.min + pad, galley, text_color);
    resp
}

/// The frame dialogs and popups float in.
pub fn overlay_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(theme::OVERLAY)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(CornerRadius::same(theme::RADIUS_L))
        .shadow(egui::Shadow { offset: [0, 10], blur: 32, spread: 0, color: Color32::from_black_alpha(120) })
        .inner_margin(egui::Margin::same(theme::SPACE_L as i8))
}

/// A centered title, an optional line under it, and the keys to press.
pub fn empty_state(ui: &mut Ui, title: &str, body: Option<&str>, hints: &[(&str, &str)]) {
    let height = 26.0 + body.map_or(0.0, |_| 22.0) + theme::SPACE_L + hints.len() as f32 * 30.0;
    ui.add_space(((ui.available_height() - height) / 2.0 - 24.0).max(0.0));
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(title).font(theme::ui_font_semibold(theme::TEXT_HEADING)).color(theme::TEXT));
        if let Some(body) = body {
            ui.add_space(4.0);
            ui.label(RichText::new(body).font(theme::ui_font(theme::TEXT_BODY)).color(theme::SUBTLE));
        }
        ui.add_space(theme::SPACE_L);
    });
    // Each hint row centered as a whole: a keycap, then what it does.
    for (k, what) in hints {
        let key_w = ui.painter().layout_no_wrap(k.to_string(), theme::ui_font_semibold(theme::TEXT_BODY), theme::TEXT).size().x + 16.0;
        let what_w = ui.painter().layout_no_wrap(what.to_string(), theme::ui_font(theme::TEXT_BODY), theme::TEXT).size().x;
        ui.horizontal(|ui| {
            ui.add_space(((ui.available_width() - key_w - what_w - 10.0) / 2.0).max(0.0));
            keycap(ui, k);
            ui.add_space(4.0);
            ui.label(RichText::new(*what).font(theme::ui_font(theme::TEXT_BODY)).color(theme::MUTED));
        });
        ui.add_space(6.0);
    }
}

/// A key drawn as a small raised cap.
pub fn keycap(ui: &mut Ui, key: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(key.to_string(), theme::ui_font_semibold(theme::TEXT_BODY), theme::TEXT);
    let pad = egui::vec2(8.0, 3.0);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + 2.0 * pad, Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(theme::RADIUS_S), theme::SELECTED, egui::Stroke::new(1.0, theme::FAINT), egui::StrokeKind::Inside);
    ui.painter().galley(rect.min + pad, galley, theme::TEXT);
    resp
}
