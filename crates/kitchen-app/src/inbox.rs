//! Review inbox (⌘I): agent-drafted comments waiting for William's approval,
//! the automatic review runs that produced them, and the watch list.
//!
//! Keys while focused: j/k move · Enter open in viewer · a approve & publish ·
//! x reject · e edit (⌘Enter saves, Esc cancels) · r refresh.
//! Nothing here is ever published without the `a` keypress (or its button).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Color32, Key, RichText};
use kitchen_core::db::{Draft, ReviewRun};

use crate::engine::{Engine, UiCommand};
use crate::panels::{PanelCmd, PanelCtx};
use crate::theme;

const REFRESH: Duration = Duration::from_secs(2);

pub struct Inbox {
    drafts: Vec<Draft>,
    runs: Vec<ReviewRun>,
    watch: Vec<String>,
    selected: usize,
    editing: Option<(i64, String)>,
    watch_input: String,
    /// Drafts with a publish/reject in flight.
    busy: Arc<Mutex<HashSet<i64>>>,
    last_refresh: Option<Instant>,
    scroll_to_selected: bool,
}

impl Inbox {
    pub fn new(engine: &Arc<Engine>) -> Self {
        let mut me = Self {
            drafts: Vec::new(),
            runs: Vec::new(),
            watch: Vec::new(),
            selected: 0,
            editing: None,
            watch_input: String::new(),
            busy: Default::default(),
            last_refresh: None,
            scroll_to_selected: false,
        };
        me.refresh(engine);
        me
    }

    /// Drafts waiting for approval (shown on the sidebar badge).
    pub fn pending_count(&self) -> usize {
        self.drafts.len()
    }

    fn refresh(&mut self, engine: &Arc<Engine>) {
        let db = engine.db();
        if let Ok(d) = db.drafts_with_state("draft") {
            self.drafts = d;
        }
        if let Ok(r) = db.review_runs(15) {
            self.runs = r;
        }
        if let Ok(w) = db.watch_list() {
            self.watch = w;
        }
        drop(db);
        self.selected = self.selected.min(self.drafts.len().saturating_sub(1));
        if let Some((id, _)) = &self.editing {
            if !self.drafts.iter().any(|d| d.id == *id) {
                self.editing = None;
            }
        }
        self.last_refresh = Some(Instant::now());
    }

    fn maybe_refresh(&mut self, engine: &Arc<Engine>) {
        if self.last_refresh.map(|t| t.elapsed() >= REFRESH).unwrap_or(true) {
            self.refresh(engine);
        }
    }

    pub fn command(&mut self, cmd: PanelCmd, engine: &Arc<Engine>) {
        match cmd {
            PanelCmd::Submit => self.save_edit(engine),
            PanelCmd::Comment => self.start_edit(),
            PanelCmd::Reload => self.refresh(engine),
            _ => {}
        }
    }

    fn current(&self) -> Option<&Draft> {
        self.drafts.get(self.selected)
    }

    fn start_edit(&mut self) {
        if let Some(d) = self.current() {
            self.editing = Some((d.id, d.body.clone()));
        }
    }

    fn save_edit(&mut self, engine: &Arc<Engine>) {
        if let Some((id, body)) = self.editing.take() {
            match crate::review::edit_draft(engine, id, &body) {
                Ok(()) => engine.toast("draft updated"),
                Err(e) => engine.toast_err(format!("edit draft: {e:#}")),
            }
            self.refresh(engine);
        }
    }

    fn approve(&mut self, engine: &Arc<Engine>) {
        let Some(d) = self.current().cloned() else { return };
        if !self.busy.lock().unwrap().insert(d.id) {
            return;
        }
        let busy = self.busy.clone();
        engine.spawn("publish comment", move |e| {
            let r = crate::review::approve_draft(e, d.id);
            busy.lock().unwrap().remove(&d.id);
            let posted = r?;
            e.toast(format!("published to {}#{}: {}", d.github_slug, d.pr_number, posted.url));
            Ok(())
        });
    }

    fn reject(&mut self, engine: &Arc<Engine>) {
        let Some(d) = self.current().cloned() else { return };
        match crate::review::reject_draft(engine, d.id) {
            Ok(()) => engine.toast(format!("rejected draft on {}", d.path)),
            Err(e) => engine.toast_err(format!("reject: {e:#}")),
        }
        self.refresh(engine);
    }

    /// Open the draft's review tab in the viewer at its line.
    fn open(&mut self, engine: &Arc<Engine>) {
        let Some(d) = self.current().cloned() else { return };
        let run = d.run_id.and_then(|id| self.runs.iter().find(|r| r.id == id).cloned());
        let tab = run.as_ref().and_then(|r| crate::review::run_tab(engine, r)).or_else(|| {
            engine
                .workspaces()
                .into_iter()
                .filter(|w| w.row.github_slug.as_deref() == Some(d.github_slug.as_str()))
                .flat_map(|w| w.tabs)
                .find(|t| t.row.pr_number == Some(d.pr_number))
                .map(|t| t.row.id)
        });
        match tab {
            Some(tab_id) => {
                engine.select_tab(&tab_id);
                engine.push_ui(UiCommand::OpenViewer {
                    tab_id,
                    path: Some(d.path.clone()),
                    line: d.line.map(|l| l as usize),
                    diff: Some(true),
                });
            }
            None => engine.toast_err(format!(
                "no tab has PR #{}; open one with `kitchenctl tab new --kind readonly --pr {}`",
                d.pr_number, d.pr_number
            )),
        }
    }

    fn handle_keys(&mut self, ui: &egui::Ui, engine: &Arc<Engine>) {
        if self.editing.is_some() {
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                self.editing = None;
            }
            return;
        }
        // A focused text field (watch-list input) owns the keyboard.
        if ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        let pressed = |k: Key| ui.input(|i| i.modifiers.is_none() && i.key_pressed(k));
        let n = self.drafts.len();
        if n > 0 && (pressed(Key::J) || pressed(Key::ArrowDown)) {
            self.selected = (self.selected + 1).min(n - 1);
            self.scroll_to_selected = true;
        }
        if pressed(Key::K) || pressed(Key::ArrowUp) {
            self.selected = self.selected.saturating_sub(1);
            self.scroll_to_selected = true;
        }
        if pressed(Key::G) {
            self.selected = 0;
            self.scroll_to_selected = true;
        }
        if n > 0 && ui.input(|i| i.modifiers.shift_only() && i.key_pressed(Key::G)) {
            self.selected = n - 1;
            self.scroll_to_selected = true;
        }
        if pressed(Key::Enter) {
            self.open(engine);
        }
        if pressed(Key::A) {
            self.approve(engine);
        }
        if pressed(Key::X) {
            self.reject(engine);
        }
        if pressed(Key::E) {
            self.start_edit();
        }
        if pressed(Key::R) {
            self.refresh(engine);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &mut PanelCtx) {
        let engine = cx.engine.clone();
        self.maybe_refresh(&engine);
        ui.ctx().request_repaint_after(REFRESH);
        if cx.focused {
            self.handle_keys(ui, &engine);
        }
        let resp = ui.interact(ui.max_rect(), ui.id().with("inbox-bg"), egui::Sense::click());
        if resp.clicked() {
            cx.clicked = true;
        }

        egui::Frame::NONE.inner_margin(12.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Review inbox").size(16.0).strong());
                ui.label(RichText::new(format!("{} pending", self.drafts.len())).color(theme::YELLOW));
            });
            ui.label(
                RichText::new("Agent comments stay private until you approve each one.  j/k move · ⏎ open · a approve & publish · x reject · e edit")
                    .size(11.5)
                    .color(theme::DIM),
            );
            ui.add_space(8.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.drafts_ui(ui, &engine, cx);
                ui.add_space(14.0);
                self.runs_ui(ui);
                ui.add_space(14.0);
                self.watch_ui(ui, &engine);
            });
        });
    }

    fn drafts_ui(&mut self, ui: &mut egui::Ui, engine: &Arc<Engine>, cx: &mut PanelCtx) {
        if self.drafts.is_empty() {
            ui.label(RichText::new("No drafts waiting.").color(theme::DIM));
            return;
        }
        let busy = self.busy.lock().unwrap().clone();
        let mut group: Option<(String, i64)> = None;
        let mut action: Option<(usize, &'static str)> = None;
        for i in 0..self.drafts.len() {
            let d = self.drafts[i].clone();
            let key = (d.github_slug.clone(), d.pr_number);
            if group.as_ref() != Some(&key) {
                let title = d
                    .run_id
                    .and_then(|id| self.runs.iter().find(|r| r.id == id))
                    .and_then(|r| r.title.clone())
                    .unwrap_or_default();
                ui.add_space(6.0);
                ui.label(RichText::new(format!("{}#{}  {title}", d.github_slug, d.pr_number)).strong().color(theme::ACCENT));
                group = Some(key);
            }
            let selected = i == self.selected;
            let frame = egui::Frame::NONE
                .fill(if selected { theme::SELECTED } else { Color32::TRANSPARENT })
                .stroke(egui::Stroke::new(1.0, if selected && cx.focused { theme::ACCENT } else { theme::BORDER }))
                .corner_radius(6.0)
                .inner_margin(8.0);
            let r = frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let sev = d.severity.clone().unwrap_or_else(|| "note".into());
                    let color = match sev.as_str() {
                        "blocker" => theme::RED,
                        "major" => theme::YELLOW,
                        "minor" => theme::CYAN,
                        _ => theme::DIM,
                    };
                    ui.label(RichText::new(sev.to_uppercase()).size(10.5).strong().color(color));
                    let loc = match (d.start_line, d.line) {
                        (Some(s), Some(l)) => format!("{}:{s}-{l}", d.path),
                        (None, Some(l)) => format!("{}:{l}{}", d.path, if d.side == "LEFT" { " (old)" } else { "" }),
                        _ => format!("{} (file)", d.path),
                    };
                    ui.label(RichText::new(loc).monospace().size(12.0));
                    if busy.contains(&d.id) {
                        ui.label(RichText::new("publishing…").color(theme::YELLOW).size(11.0));
                    }
                });
                match &mut self.editing {
                    Some((id, body)) if *id == d.id => {
                        let te = ui.add(egui::TextEdit::multiline(body).desired_rows(4).desired_width(f32::INFINITY));
                        te.request_focus();
                        ui.label(RichText::new("⌘Enter save · Esc cancel").size(10.5).color(theme::DIM));
                    }
                    _ => {
                        ui.label(RichText::new(&d.body).size(12.5));
                    }
                }
                ui.horizontal(|ui| {
                    if ui.small_button("approve & publish (a)").clicked() {
                        action = Some((i, "approve"));
                    }
                    if ui.small_button("reject (x)").clicked() {
                        action = Some((i, "reject"));
                    }
                    if ui.small_button("edit (e)").clicked() {
                        action = Some((i, "edit"));
                    }
                    if ui.small_button("open (⏎)").clicked() {
                        action = Some((i, "open"));
                    }
                });
            });
            if selected && self.scroll_to_selected {
                r.response.scroll_to_me(Some(egui::Align::Center));
                self.scroll_to_selected = false;
            }
            if r.response.interact(egui::Sense::click()).clicked() {
                self.selected = i;
                cx.clicked = true;
            }
        }
        if let Some((i, what)) = action {
            self.selected = i;
            cx.clicked = true;
            match what {
                "approve" => self.approve(engine),
                "reject" => self.reject(engine),
                "edit" => self.start_edit(),
                _ => self.open(engine),
            }
        }
    }

    fn runs_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("AUTOMATIC REVIEWS").size(11.0).strong().color(theme::DIM));
        if self.runs.is_empty() {
            ui.label(RichText::new("None yet. PRs from people on the watch list that request your review are reviewed automatically.").size(11.5).color(theme::DIM));
            return;
        }
        for r in &self.runs {
            let color = match r.status.as_str() {
                "done" => theme::GREEN,
                "running" | "queued" => theme::YELLOW,
                "failed" => theme::RED,
                _ => theme::DIM,
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&r.status).size(11.0).strong().color(color));
                ui.label(RichText::new(format!("{}#{}", r.github_slug, r.pr_number)).size(12.0));
                ui.label(RichText::new(r.title.clone().unwrap_or_default()).size(12.0).color(theme::DIM));
                if let Some(a) = &r.author {
                    ui.label(RichText::new(format!("@{a}")).size(11.0).color(theme::PURPLE));
                }
                ui.label(RichText::new(&r.head_sha[..r.head_sha.len().min(8)]).monospace().size(10.5).color(theme::DIM));
            });
            if let Some(e) = &r.error {
                ui.label(RichText::new(format!("  {e}")).size(11.0).color(theme::RED));
            }
        }
    }

    fn watch_ui(&mut self, ui: &mut egui::Ui, engine: &Arc<Engine>) {
        ui.label(RichText::new("WATCH LIST").size(11.0).strong().color(theme::DIM));
        ui.label(RichText::new("When one of these people requests your review, their PR is reviewed with /deep-review-other-pr.").size(11.5).color(theme::DIM));
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            for login in &self.watch {
                if ui.button(format!("@{login}  ×")).on_hover_text("remove").clicked() {
                    remove = Some(login.clone());
                }
            }
        });
        if let Some(l) = remove {
            let r = engine.db().unwatch(&l);
            if let Err(e) = r {
                engine.toast_err(format!("unwatch: {e:#}"));
            }
            self.refresh(engine);
        }
        ui.horizontal(|ui| {
            let te = ui.add(egui::TextEdit::singleline(&mut self.watch_input).hint_text("github login").desired_width(200.0));
            let enter = te.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if (ui.button("add").clicked() || enter) && !self.watch_input.trim().is_empty() {
                let login = self.watch_input.trim().trim_start_matches('@').to_string();
                let r = engine.db().watch(&login);
                if let Err(e) = r {
                    engine.toast_err(format!("watch: {e:#}"));
                }
                self.watch_input.clear();
                self.refresh(engine);
            }
        });
    }
}
