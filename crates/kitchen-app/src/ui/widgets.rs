//! Small pieces of the kitchen's look shared by every view: badges, section
//! labels, key hints, buttons, the frame of dialogs and popups, empty states,
//! the marks on tabs and workspaces.

use std::time::Duration;

use egui::text::LayoutJob;
use egui::{Color32, CornerRadius, Response, RichText, Sense, TextFormat, Ui};

use crate::engine::{CheckState, PrState};
use crate::theme;

/// Height of the header row atop the viewer and the plan.
pub const HEADER_HEIGHT: f32 = 36.0;

/// The mark in a tab's or workspace's corner, most urgent first (`Ord`
/// ranks them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mark {
    /// Its agent is blocked on William.
    NeedsYou,
    /// Its agent finished, and William hasn't looked yet.
    Unread,
    /// Its agent is taking a turn.
    Working,
}

/// How often the working ring spins, and how long one spin takes, in seconds.
const SPIN_EVERY: f64 = 3.0;
const SPIN: f64 = 0.6;

/// Frame interval during a spin (18 frames each; every frame redraws the
/// whole window).
const SPIN_FRAME: Duration = Duration::from_millis(33);

/// Paint `mark` centred on `at` with `p` (the caller's clipped painter): an
/// orange `!` (NeedsYou), the yellow dot of radius `r` (Unread), or an open
/// ring in `working_color` (Working). Every `SPIN_EVERY` the ring turns once
/// and swells and brightens as it turns; between spins it holds still, so
/// nothing is redrawn for it.
pub fn paint_mark(p: &egui::Painter, at: egui::Pos2, r: f32, mark: Mark, working_color: Color32) {
    match mark {
        Mark::NeedsYou => {
            p.text(at, egui::Align2::CENTER_CENTER, "!", theme::ui_font_semibold(theme::TEXT_BODY), theme::ORANGE);
        }
        Mark::Unread => {
            p.circle_filled(at, r, theme::YELLOW);
        }
        Mark::Working => {
            let spin = spin(p.ctx()).unwrap_or(0.0);
            // One clockwise turn, easing in and out; the swell peaks halfway.
            let turn = spin * spin * (3.0 - 2.0 * spin) * std::f32::consts::TAU;
            let swell = (spin * std::f32::consts::PI).sin();
            let radius = r + 1.0 + 1.5 * swell;
            let color = working_color.lerp_to_gamma(Color32::WHITE, 0.45 * swell);
            // Three quarters of a circle, open at the top right when still.
            let points: Vec<_> = (0..=18)
                .map(|i| -std::f32::consts::FRAC_PI_2 + i as f32 / 18.0 * 1.5 * std::f32::consts::PI - turn)
                .map(|a| at + radius * egui::vec2(-a.cos(), a.sin()))
                .collect();
            p.add(egui::Shape::line(points, egui::Stroke::new(1.5 + 0.5 * swell, color)));
        }
    }
}

/// Where the shared spin clock is: Some(0 to 1) during a spin, None between
/// spins or while the kitchen isn't focused. Every working ring runs on this
/// one clock, so one burst of frames covers them all. Asks for the frames
/// the spin needs: the next one during it, else the start of the next spin.
fn spin(ctx: &egui::Context) -> Option<f32> {
    // A hidden (test) kitchen never has focus; it shows what a focused one would.
    let (now, focused) = ctx.input(|i| (i.time, i.focused || crate::hidden::active()));
    if !focused {
        return None; // regaining focus repaints, and that frame asks again
    }
    let phase = now % SPIN_EVERY;
    if phase < SPIN {
        ctx.request_repaint_after(SPIN_FRAME);
        Some((phase / SPIN) as f32)
    } else {
        ctx.request_repaint_after(Duration::from_secs_f64(SPIN_EVERY - phase));
        None
    }
}

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

/// The color of a PR check in `state`: green done, grey waiting on others,
/// yellow for the author to act.
pub fn check_color(state: CheckState) -> Color32 {
    match state {
        CheckState::Done => theme::GREEN,
        CheckState::Waiting => theme::MUTED,
        CheckState::Needed => theme::YELLOW,
    }
}

/// The icon of a PR check in `state`, filling the square `rect`: a check
/// mark (done), a clock (waiting), an exclamation mark in a ring (needed).
pub fn check_icon(painter: &egui::Painter, rect: egui::Rect, state: CheckState) {
    let color = check_color(state);
    let unit = rect.width() / 16.0;
    let at = |x: f32, y: f32| rect.min + egui::vec2(x, y) * unit;
    let stroke = egui::Stroke::new(1.75 * unit, color);
    match state {
        CheckState::Done => {
            painter.line_segment([at(2.5, 8.5), at(6.0, 12.0)], stroke);
            painter.line_segment([at(6.0, 12.0), at(13.5, 4.5)], stroke);
        }
        CheckState::Waiting => {
            painter.circle_stroke(at(8.0, 8.0), 6.25 * unit, stroke);
            painter.line_segment([at(8.0, 8.0), at(8.0, 4.5)], stroke);
            painter.line_segment([at(8.0, 8.0), at(10.5, 9.5)], stroke);
        }
        CheckState::Needed => {
            painter.circle_stroke(at(8.0, 8.0), 6.25 * unit, stroke);
            painter.line_segment([at(8.0, 4.5), at(8.0, 9.0)], stroke);
            painter.circle_filled(at(8.0, 11.5), 1.0 * unit, color);
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

/// `⌘T new  ⌘W close`: keys brighter than what they do; hints with no key are skipped.
pub fn key_hints(ui: &mut Ui, hints: &[(String, &str)]) {
    let mut job = LayoutJob::default();
    let key = TextFormat { font_id: theme::ui_font_semibold(theme::TEXT_SMALL), color: theme::MUTED, ..Default::default() };
    let label = TextFormat { font_id: theme::ui_font(theme::TEXT_SMALL), color: theme::SUBTLE, ..Default::default() };
    for (i, (k, what)) in hints.iter().filter(|(k, _)| !k.is_empty()).enumerate() {
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
        .shadow(theme::WINDOW_SHADOW)
        .inner_margin(egui::Margin::same(theme::SPACE_XL as i8))
}

/// A centered title, an optional line under it, and the keys to press
/// (hints with no key are skipped).
pub fn empty_state(ui: &mut Ui, title: &str, body: Option<&str>, hints: &[(String, &str)]) {
    let hints: Vec<&(String, &str)> = hints.iter().filter(|(k, _)| !k.is_empty()).collect();
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
