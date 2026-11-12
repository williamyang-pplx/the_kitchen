//! The kitchen's loading animation: a frying pan on the stove, tossing an
//! egg that flips in the air and sizzles when it lands, steam rising.
//! Painted with egui shapes (no images); it repaints itself while shown.

use egui::epaint::EllipseShape;
use egui::{Color32, Painter, Pos2, Shape, Stroke, Ui, pos2, vec2};

use crate::theme;

/// One toss: the egg flies for the first half, sizzles for the rest.
const TOSS_SECS: f32 = 1.6;

const PAN: Color32 = Color32::from_rgb(0x2b, 0x2f, 0x40);
const PAN_FLOOR: Color32 = Color32::from_rgb(0x20, 0x23, 0x31);
const PAN_RIM: Color32 = Color32::from_rgb(0x56, 0x5f, 0x89);
const HANDLE: Color32 = Color32::from_rgb(0x8a, 0x5a, 0x3c);
const EGG_WHITE: Color32 = Color32::from_rgb(0xf2, 0xee, 0xe4);
const EGG_BROWNED: Color32 = Color32::from_rgb(0xe4, 0xc9, 0x96);
const YOLK: Color32 = Color32::from_rgb(0xff, 0xb3, 0x2e);
const FLAME: Color32 = Color32::from_rgb(0x7a, 0xa2, 0xf7);

/// The pan, centered in the space left in `ui`, with `caption` under it
/// ("cooking #123…").
pub fn cooking(ui: &mut Ui, caption: &str) {
    let rect = ui.available_rect_before_wrap();
    let t = ui.input(|i| i.time) as f32;
    let r = (rect.width().min(rect.height()) * 0.11).clamp(34.0, 64.0);
    let center = rect.center() - vec2(r * 0.35, r * 0.4);
    paint_pan(ui.painter(), center, r, t);
    let caption_at = pos2(rect.center().x, center.y + r * 1.25);
    ui.painter().text(
        caption_at,
        egui::Align2::CENTER_TOP,
        caption,
        theme::ui_font(theme::TEXT_BODY),
        theme::SUBTLE,
    );
    // Keep the space so nothing else lays out over the pan.
    ui.allocate_rect(rect, egui::Sense::hover());
    ui.ctx().request_repaint();
}

/// Pan of radius `r` around `c` at time `t` (seconds).
fn paint_pan(p: &Painter, c: Pos2, r: f32, t: f32) {
    let u = (t / TOSS_SECS).fract();
    // The pan dips as it throws, then settles.
    let flying = u < 0.5;
    let dip = if flying { (u / 0.5 * std::f32::consts::PI).sin() * r * 0.06 } else { 0.0 };
    let c = c + vec2(0.0, -dip);

    flames(p, c + vec2(0.0, r * 0.62), r, t);

    // Handle, behind the rim on the right.
    let handle_from = c + vec2(r * 0.9, -r * 0.02);
    let handle_to = c + vec2(r * 2.05, -r * 0.22 - dip * 0.5);
    p.line_segment([handle_from, handle_to], Stroke::new(r * 0.16, PAN_RIM));
    let grip_from = handle_from + (handle_to - handle_from) * 0.45;
    p.line_segment([grip_from, handle_to], Stroke::new(r * 0.2, HANDLE));

    // Body, floor and rim: an ellipse seen from slightly above.
    p.add(Shape::ellipse_filled(c + vec2(0.0, r * 0.08), vec2(r, r * 0.4), PAN));
    p.add(Shape::ellipse_filled(c, vec2(r * 0.86, r * 0.31), PAN_FLOOR));
    p.add(Shape::ellipse_stroke(c, vec2(r * 0.93, r * 0.35), Stroke::new(r * 0.07, PAN_RIM)));

    // The egg: tossed up and flipped once per toss, then it sizzles.
    let (lift, flip) = if flying {
        let k = u / 0.5;
        ((k * std::f32::consts::PI).sin() * r * 1.05, k * std::f32::consts::PI)
    } else {
        (0.0, 0.0)
    };
    let egg = c + vec2(0.0, -lift - r * 0.02);
    // Seen edge-on mid-flip: squashed vertically; the browned side shows
    // while it is upside down.
    let face = flip.cos();
    let squash = face.abs().max(0.18);
    let tilt = if flying { (flip * 0.5).sin() * 0.35 } else { 0.0 };
    let shadow = (1.0 - lift / (r * 1.1)).clamp(0.2, 1.0);
    p.add(Shape::ellipse_filled(c + vec2(0.0, r * 0.05), vec2(r * 0.42 * shadow, r * 0.11 * shadow), Color32::from_black_alpha(70)));
    p.add(Shape::Ellipse(EllipseShape {
        center: egg,
        radius: vec2(r * 0.46, r * 0.19 * squash),
        fill: if face >= 0.0 { EGG_WHITE } else { EGG_BROWNED },
        stroke: Stroke::NONE,
        angle: tilt,
    }));
    // A second blob makes the white look poured, not stamped.
    p.add(Shape::Ellipse(EllipseShape {
        center: egg + vec2(-r * 0.16, r * 0.03 * squash),
        radius: vec2(r * 0.26, r * 0.15 * squash),
        fill: if face >= 0.0 { EGG_WHITE } else { EGG_BROWNED },
        stroke: Stroke::NONE,
        angle: tilt,
    }));
    if face > 0.3 {
        p.add(Shape::ellipse_filled(egg + vec2(r * 0.06, -r * 0.03 * squash), vec2(r * 0.13, r * 0.1 * squash), YOLK));
        p.circle_filled(egg + vec2(r * 0.03, -r * 0.07 * squash), r * 0.03, Color32::from_rgba_unmultiplied(255, 240, 200, 160));
    }

    if !flying {
        sizzle(p, c, r, t);
    }
    steam(p, c, r, t);
}

/// Blue gas flames under the pan, flickering.
fn flames(p: &Painter, base: Pos2, r: f32, t: f32) {
    for i in 0..5 {
        let x = (i as f32 - 2.0) * r * 0.3;
        let h = r * (0.16 + 0.07 * (t * 9.0 + i as f32 * 1.7).sin().abs());
        let w = r * 0.07;
        let at = base + vec2(x, 0.0);
        let tip = at + vec2((t * 6.0 + i as f32).sin() * w * 0.4, -h);
        p.add(Shape::convex_polygon(vec![at + vec2(-w, 0.0), tip, at + vec2(w, 0.0)], FLAME.gamma_multiply(0.75), Stroke::NONE));
    }
}

/// Oil specks popping around the landed egg.
fn sizzle(p: &Painter, c: Pos2, r: f32, t: f32) {
    for i in 0..7 {
        let seed = i as f32 * 2.399;
        let life = ((t * 2.3 + seed * 0.37).fract() * 2.0).min(1.0);
        let angle = seed + (t * 0.7).floor() * 1.3;
        let spread = r * (0.5 + 0.25 * life);
        let at = c + vec2(angle.cos() * spread, angle.sin() * spread * 0.32 - life * r * 0.25);
        let alpha = ((1.0 - life) * 200.0) as u8;
        p.circle_filled(at, r * 0.025 + r * 0.02 * (1.0 - life), Color32::from_rgba_unmultiplied(0xff, 0xe2, 0xa8, alpha));
    }
}

/// Three wisps of steam rising and fading.
fn steam(p: &Painter, c: Pos2, r: f32, t: f32) {
    for i in 0..3 {
        let x0 = c.x + (i as f32 - 1.0) * r * 0.38;
        let age = (t * 0.45 + i as f32 / 3.0).fract();
        let base = c.y - r * 0.35 - age * r * 0.9;
        let points: Vec<Pos2> = (0..10)
            .map(|k| {
                let y = base - k as f32 * r * 0.07;
                let x = x0 + ((y / r) * 3.0 + t * 2.5 + i as f32 * 2.0).sin() * r * 0.09;
                pos2(x, y)
            })
            .collect();
        let alpha = ((1.0 - age) * age * 4.0 * 110.0) as u8;
        p.add(Shape::line(points, Stroke::new(r * 0.045, Color32::from_rgba_unmultiplied(0xc0, 0xca, 0xf5, alpha))));
    }
}
