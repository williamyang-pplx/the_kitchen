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

pub fn ui(ui: &mut Ui, engine: &Arc<Engine>, ws: &Workspace) {
    // Relative times ("12m ago") tick; store changes repaint on their own.
    ui.ctx().request_repaint_after(Duration::from_secs(1));
    let Some(plugins) = engine.plugins() else { return };
    let now = wall_now();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(18, 14)).show(ui, |ui| {
            ui.label(RichText::new(format!("{} @ {}", ws.row.name, ws.row.host)).font(theme::ui_font_semibold(14.0)).color(theme::TEXT));
            ui.add_space(10.0);
            timers(ui, &plugins.timers_info(), &ws.row.id, now);
            if let Some(sections) = plugins.dashboard.get(&ws.row.id) {
                for (_, list) in sections.iter() {
                    for s in list {
                        section(ui, s);
                    }
                }
            }
        });
    });
}

fn heading(ui: &mut Ui, title: &str, note: Option<&str>) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title.to_uppercase()).font(theme::ui_font_semibold(11.0)).color(theme::DIM));
        if let Some(n) = note {
            ui.label(RichText::new(n).font(theme::ui_font(11.0)).color(theme::MUTED));
        }
    });
    ui.add_space(2.0);
}

fn small(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text.into()).font(theme::ui_font(12.0)).color(color)
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
    heading(ui, "Timers", (list.is_empty()).then_some("no plugin timers"));
    for t in list {
        ui.horizontal(|ui| {
            ui.label(small(format!("{}.{}", t.plugin, t.name), if t.running { theme::YELLOW } else { theme::TEXT })).on_hover_text(&t.desc);
            ui.label(small(timer_when(t, now), theme::MUTED));
        });
        let indent = |ui: &mut Ui, text: RichText| {
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.label(text);
            });
        };
        if let Some(e) = &t.state.last_error {
            indent(ui, small(e.lines().next().unwrap_or(e), theme::RED));
        }
        let all = stats_line(&t.state.stats);
        if !all.is_empty() {
            indent(ui, small(format!("last run: {all}"), theme::DIM));
        }
        let here = stats_line(t.state.workspaces.get(ws).unwrap_or(&serde_json::Value::Null));
        if !here.is_empty() {
            indent(ui, small(format!("this workspace: {here}"), theme::DIM));
        }
    }
    ui.add_space(8.0);
}

fn section(ui: &mut Ui, s: &Section) {
    heading(ui, &s.title, s.note.as_deref());
    egui::Grid::new(("dash", &s.title)).num_columns(3).spacing([14.0, 3.0]).show(ui, |ui| {
        for r in &s.rows {
            row(ui, r);
            ui.end_row();
        }
    });
    ui.add_space(8.0);
}

fn row(ui: &mut Ui, r: &Row) {
    let label: String = r.label.chars().take(60).collect();
    ui.label(small(label, if r.mark { theme::ACCENT } else { theme::TEXT })).on_hover_text(&r.label);
    ui.label(small(&r.value, theme::DIM));
    match r.fraction {
        Some(f) => bar(ui, f),
        None => {
            ui.label("");
        }
    }
}

/// A thin fill bar, amber past 80 %, red past 95 %.
fn bar(ui: &mut Ui, f: f32) {
    let f = f.clamp(0.0, 1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(120.0, 6.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3.0, theme::BORDER);
    let color = if f > 0.95 { theme::RED } else if f > 0.8 { theme::YELLOW } else { theme::ACCENT };
    let mut fill = rect;
    fill.set_width(rect.width() * f);
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
        assert_eq!(stats_line(&serde_json::json!({"note": "ok"})), "note ok");
    }
}
