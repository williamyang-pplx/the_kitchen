//! The workspace dashboard (⌘D, or the ▦ at the right end of the tab strip):
//! every plugin timer's runs, then the sections plugins publish for this
//! workspace (`kitchen.dashboard.set`), e.g. worktrees, disk and RAM.

use std::sync::Arc;
use std::time::Duration;

use egui::{Color32, RichText, Ui};

use crate::engine::{Engine, Workspace};
use crate::plugins::dashboard::{Row, Section};
use crate::plugins::timers::{TimerInfo, wall_now};
use crate::theme;
use crate::ui::widgets;

pub fn ui(ui: &mut Ui, engine: &Arc<Engine>, ws: &Workspace) {
    // Relative times ("12m ago") tick; store changes repaint on their own.
    ui.ctx().request_repaint_after(Duration::from_secs(1));
    let Some(plugins) = engine.plugins() else { return };
    let now = wall_now();
    let timers_list = plugins.timers_info();
    let sections: Vec<Section> = plugins.dashboard.get(&ws.row.id).map(|s| s.iter().flat_map(|(_, list)| list.iter().cloned()).collect()).unwrap_or_default();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(theme::SPACE_XL as i8, 20)).show(ui, |ui| {
            ui.label(RichText::new(&ws.row.name).font(theme::ui_font_semibold(theme::TEXT_HEADING)).color(theme::TEXT));
            ui.add_space(2.0);
            ui.label(RichText::new(format!("{} · {}", ws.row.host, ws.row.repo_path)).font(theme::ui_font(theme::TEXT_BODY)).color(theme::SUBTLE));
            ui.add_space(theme::SPACE_XL - 4.0);
            // Cards fill columns left to right; as many columns as fit.
            let n = ((ui.available_width() + theme::SPACE_L) / 440.0).floor().clamp(1.0, 3.0) as usize;
            ui.spacing_mut().item_spacing.x = theme::SPACE_L;
            ui.columns(n, |cols| {
                for c in cols.iter_mut() {
                    c.spacing_mut().item_spacing.y = theme::SPACE_L;
                }
                card(&mut cols[0], |ui| timers(ui, &timers_list, &ws.row.id, now));
                for (i, s) in sections.iter().enumerate() {
                    card(&mut cols[(i + 1) % n], |ui| section(ui, s));
                }
            });
        });
    });
}

/// A raised card holding one dashboard section.
fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::NONE
        .fill(theme::HOVER)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(theme::RADIUS_L))
        .inner_margin(egui::Margin::same(theme::SPACE_L as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            add(ui);
        });
}

fn heading(ui: &mut Ui, title: &str, note: Option<&str>) {
    ui.horizontal(|ui| {
        ui.label(widgets::section_label(title, theme::SUBTLE));
        if let Some(n) = note {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(n).font(theme::ui_font(theme::TEXT_SMALL)).color(theme::DIM));
            });
        }
    });
    ui.add_space(theme::SPACE_XS + 2.0);
}

fn small(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text.into()).font(theme::ui_font(theme::TEXT_BODY)).color(color)
}

/// "3h 5m", "42s".
pub fn span(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 if s % 3600 / 60 == 0 => format!("{}h", s / 3600),
        3600..86400 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d", s / 86400),
    }
}

/// `k v · k v` for a flat stats map, in key order.
pub fn stats_line(v: &serde_json::Value) -> String {
    let Some(m) = v.as_object() else { return String::new() };
    m.iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => format!("{k} {s}"),
            v => format!("{k} {v}"),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A timer's last-run stats on workspace `ws`'s dashboard: this workspace's
/// first, then the whole pass's, labelled so a total isn't read as this
/// workspace's ("all workspaces" only when the timer reports per workspace).
pub fn stats_lines(t: &TimerInfo, ws: &str) -> Vec<String> {
    let here = stats_line(t.state.workspaces.get(ws).unwrap_or(&serde_json::Value::Null));
    let all = stats_line(&t.state.stats);
    let mut out = Vec::new();
    if !here.is_empty() {
        out.push(format!("last run, this workspace: {here}"));
    }
    if !all.is_empty() {
        out.push(if t.state.workspaces.is_object() { format!("last run, all workspaces: {all}") } else { format!("last run: {all}") });
    }
    out
}

/// What one timer row says about when it runs: "every 1h · last 12m ago (took 4m) · next in 48m".
pub fn timer_when(t: &TimerInfo, now: f64) -> String {
    let mut out = format!("every {}", span(t.interval_secs));
    match (t.state.last_fired, t.running) {
        (Some(f), true) => out.push_str(&format!(" · running for {}", span(now - f))),
        (Some(f), false) => {
            out.push_str(&format!(" · last {} ago", span(now - f)));
            if let Some(d) = t.state.last_duration_secs {
                out.push_str(&format!(" (took {})", span(d)));
            }
        }
        (None, _) => out.push_str(" · not run yet"),
    }
    if !t.running {
        out.push_str(&if t.next_due <= now { " · due now".to_string() } else { format!(" · next in {}", span(t.next_due - now)) });
    }
    out.push_str(&format!(" · {} run{}", t.state.runs, if t.state.runs == 1 { "" } else { "s" }));
    out
}

fn timers(ui: &mut Ui, list: &[TimerInfo], ws: &str, now: f64) {
    heading(ui, "Timers", None);
    if list.is_empty() {
        ui.label(small("no plugin timers", theme::SUBTLE));
    }
    for (i, t) in list.iter().enumerate() {
        if i > 0 {
            ui.add_space(theme::SPACE_S);
        }
        ui.horizontal(|ui| {
            if t.running {
                ui.label(RichText::new("●").font(theme::ui_font(theme::TEXT_SMALL)).color(theme::YELLOW));
            }
            ui.label(RichText::new(format!("{}.{}", t.plugin, t.name)).font(theme::ui_font_semibold(theme::TEXT_BODY)).color(theme::TEXT)).on_hover_text(&t.desc);
        });
        ui.label(RichText::new(timer_when(t, now)).font(theme::ui_font(theme::TEXT_SMALL)).color(theme::SUBTLE));
        if let Some(e) = &t.state.last_error {
            ui.label(RichText::new(e.lines().next().unwrap_or(e)).font(theme::ui_font(theme::TEXT_SMALL)).color(theme::RED));
        }
        for line in stats_lines(t, ws) {
            ui.label(RichText::new(line).font(theme::ui_font(theme::TEXT_SMALL)).color(theme::DIM));
        }
    }
}

fn section(ui: &mut Ui, s: &Section) {
    heading(ui, &s.title, s.note.as_deref());
    for r in &s.rows {
        row(ui, r);
    }
}

/// Label on the left; the value right-aligned before a bar, when there is one.
fn row(ui: &mut Ui, r: &Row) {
    const BAR: f32 = 96.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::hover());
    let bar_w = if r.fraction.is_some() { BAR + theme::SPACE_M } else { 0.0 };
    let value = ui.painter().layout_no_wrap(r.value.clone(), theme::ui_font(theme::TEXT_SMALL), theme::SUBTLE);
    let value_right = rect.right() - bar_w;
    let label_max = (value_right - value.size().x - theme::SPACE_M - rect.left()).max(40.0);
    let mut job = egui::text::LayoutJob::simple_singleline(r.label.clone(), theme::ui_font(theme::TEXT_BODY), if r.mark { theme::ACCENT } else { theme::MUTED });
    job.wrap = egui::text::TextWrapping::truncate_at_width(label_max);
    let label = ui.painter().layout_job(job);
    let p = ui.painter();
    p.galley(egui::pos2(rect.left(), rect.center().y - label.size().y / 2.0), label, theme::MUTED);
    p.galley(egui::pos2(value_right - value.size().x, rect.center().y - value.size().y / 2.0), value, theme::SUBTLE);
    if let Some(f) = r.fraction {
        bar(ui, egui::Rect::from_min_size(egui::pos2(rect.right() - BAR, rect.center().y - 3.0), egui::vec2(BAR, 6.0)), f);
    }
    resp.on_hover_text(&r.label);
}

/// A fill bar, amber past 80 %, red past 95 %.
fn bar(ui: &Ui, rect: egui::Rect, f: f32) {
    let f = f.clamp(0.0, 1.0);
    let p = ui.painter();
    p.rect_filled(rect, 3.0, theme::BORDER);
    let color = if f > 0.95 { theme::RED } else if f > 0.8 { theme::YELLOW } else { theme::ACCENT };
    let mut fill = rect;
    fill.set_width((rect.width() * f).max(if f > 0.0 { 3.0 } else { 0.0 }));
    p.rect_filled(fill, 3.0, color);
}

/// The tab-strip button: a tiny 2×2 grid, lit while the dashboard shows.
pub fn button(ui: &mut Ui, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
    let color = if on { theme::ACCENT } else if resp.hovered() { theme::TEXT } else { theme::MUTED };
    if on {
        ui.painter().rect_filled(rect, 5.0, theme::ACCENT.gamma_multiply(0.18));
    }
    let c = rect.center();
    for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let sq = egui::Rect::from_center_size(c + egui::vec2(dx * 3.2, dy * 3.2), egui::vec2(5.0, 5.0));
        ui.painter().rect_filled(sq, 1.0, color);
    }
    resp.on_hover_text("Dashboard ⌘D")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::timers::TimerState;

    #[test]
    fn timer_rows_read_naturally() {
        assert_eq!((span(42.0), span(125.0), span(3600.0), span(3900.0), span(90000.0)), ("42s".into(), "2m".into(), "1h".into(), "1h 5m".into(), "1d".into()));
        let mut t = TimerInfo {
            plugin: "prune-worktrees".into(),
            name: "pass".into(),
            desc: String::new(),
            interval_secs: 3600.0,
            next_due: 1000.0 + 3600.0,
            running: false,
            state: TimerState { runs: 3, last_fired: Some(1000.0), last_duration_secs: Some(240.0), stats: serde_json::json!({"kept": 41, "removed": 5}), ..Default::default() },
        };
        assert_eq!(timer_when(&t, 1000.0 + 720.0), "every 1h · last 12m ago (took 4m) · next in 48m · 3 runs");
        t.running = true;
        assert_eq!(timer_when(&t, 1060.0), "every 1h · running for 1m · 3 runs");
        assert_eq!(stats_line(&t.state.stats), "kept 41 · removed 5");
        assert_eq!(stats_lines(&t, "w1"), ["last run: kept 41 · removed 5"], "no per-workspace stats");
        t.state.workspaces = serde_json::json!({ "w1": { "kept": 3, "removed": 0 } });
        assert_eq!(stats_lines(&t, "w1"), ["last run, this workspace: kept 3 · removed 0", "last run, all workspaces: kept 41 · removed 5"]);
        assert_eq!(stats_lines(&t, "w2"), ["last run, all workspaces: kept 41 · removed 5"]);
        assert_eq!(stats_line(&serde_json::json!({"note": "ok"})), "note ok");
    }
}
