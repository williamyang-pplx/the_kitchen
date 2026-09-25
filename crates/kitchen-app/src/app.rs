//! The window: workspace sidebar, tab strip, terminal, side panel
//! (viewer / plan / inbox), dialogs, and the Cmd keymap.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use egui::{Align, Color32, Key, Layout, Modifiers, RichText, Sense, Ui};
use kitchen_proto::TabKind;

use crate::engine::{Engine, NewTab, Tab, UiCommand, Workspace};
use crate::keymap::{self, Action};
use crate::panels::{PanelCmd, PanelCtx};
use crate::terminal::{TermSession, TerminalView};
use crate::theme;
use crate::ui::dialogs::{Dialog, DialogOutcome};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Terminal,
    Side,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SideKind {
    Viewer,
    Plan,
    Inbox,
}

enum TermSlot {
    /// Waiting for the tmux session to be (re)created on the host.
    Preparing,
    Live(TermSession),
    Failed { msg: String, retry_at: Instant, attempts: u32 },
    /// Worktree is gone; nothing to attach to.
    Gone(String),
}

pub struct KitchenApp {
    engine: Arc<Engine>,
    slots: HashMap<String, TermSlot>,
    attach_tx: Sender<(String, anyhow::Result<bool>)>,
    attach_rx: Receiver<(String, anyhow::Result<bool>)>,
    focus: Focus,
    side: Option<SideKind>,
    viewers: HashMap<String, crate::viewer::Viewer>,
    plans: HashMap<String, crate::plan_view::PlanView>,
    inbox: crate::inbox::Inbox,
    dialog: Option<Dialog>,
    screenshots: Vec<(String, Sender<Result<(), String>>)>,
    /// Test hook: take a screenshot after a delay, then exit.
    auto_shot: Option<(Instant, String)>,
}

impl KitchenApp {
    pub fn new(engine: Arc<Engine>) -> Self {
        let (attach_tx, attach_rx) = channel();
        let auto_shot = std::env::var("KITCHEN_SCREENSHOT").ok().map(|p| {
            let secs = std::env::var("KITCHEN_SCREENSHOT_DELAY").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
            (Instant::now() + Duration::from_secs(secs), p)
        });
        let inbox = crate::inbox::Inbox::new(&engine);
        Self {
            engine,
            slots: HashMap::new(),
            attach_tx,
            attach_rx,
            focus: Focus::Terminal,
            side: None,
            viewers: HashMap::new(),
            plans: HashMap::new(),
            inbox,
            dialog: None,
            screenshots: Vec::new(),
            auto_shot,
        }
    }

    // ------------------------------------------------------------ terminals

    fn ensure_terminal(&mut self, ctx: &egui::Context, ws: &Workspace, tab: &Tab) {
        let id = tab.row.id.clone();
        let needs_prepare = match self.slots.get(&id) {
            None => true,
            Some(TermSlot::Live(s)) => s.exited(),
            Some(TermSlot::Failed { retry_at, .. }) => Instant::now() >= *retry_at,
            Some(TermSlot::Preparing) | Some(TermSlot::Gone(_)) => false,
        };
        if !needs_prepare {
            return;
        }
        let attempts = match self.slots.get(&id) {
            Some(TermSlot::Failed { attempts, .. }) => *attempts,
            _ => 0,
        };
        self.slots.insert(id.clone(), TermSlot::Preparing);
        let engine = self.engine.clone();
        let tx = self.attach_tx.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let r = engine.ensure_session(&id);
            let _ = tx.send((id, r));
            ctx2.request_repaint();
        });
        let _ = (ws, attempts);
    }

    fn poll_attach(&mut self, ctx: &egui::Context) {
        while let Ok((tab_id, result)) = self.attach_rx.try_recv() {
            let Some((ws, tab)) = self.engine.find_tab(&tab_id) else {
                self.slots.remove(&tab_id);
                continue;
            };
            let prev_attempts = 0;
            let slot = match result {
                Ok(true) => {
                    let argv = ws.host().tmux_attach_argv(&tab.row.tmux_session);
                    let mut env = HashMap::new();
                    env.insert("LANG".into(), std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into()));
                    match TermSession::spawn(ctx, &argv, Some(&home()), env) {
                        Ok(s) => TermSlot::Live(s),
                        Err(e) => TermSlot::Failed {
                            msg: format!("spawn failed: {e:#}"),
                            retry_at: Instant::now() + Duration::from_secs(3),
                            attempts: prev_attempts + 1,
                        },
                    }
                }
                Ok(false) => TermSlot::Gone(format!("worktree {} no longer exists", tab.row.worktree)),
                Err(e) => {
                    let attempts = prev_attempts + 1;
                    TermSlot::Failed {
                        msg: format!("{e:#}"),
                        retry_at: Instant::now() + Duration::from_secs((2u64 << attempts.min(4)).min(30)),
                        attempts,
                    }
                }
            };
            self.slots.insert(tab_id, slot);
        }
    }

    // ------------------------------------------------------------ commands

    fn handle_ui_commands(&mut self, ctx: &egui::Context) {
        for cmd in self.engine.take_ui_commands() {
            match cmd {
                UiCommand::OpenViewer { tab_id, path, line, diff } => {
                    self.side = Some(SideKind::Viewer);
                    self.focus = Focus::Side;
                    self.panel_command_for(&tab_id, SideKind::Viewer, PanelCmd::Open { path, line, diff });
                }
                UiCommand::OpenPlan { tab_id } => {
                    self.side = Some(SideKind::Plan);
                    self.focus = Focus::Side;
                    self.panel_command_for(&tab_id, SideKind::Plan, PanelCmd::Reload);
                }
                UiCommand::ReloadPlan { tab_id } => {
                    if let Some(p) = self.plans.get_mut(&tab_id) {
                        p.command(PanelCmd::Reload, &self.engine);
                    }
                }
                UiCommand::OpenInbox => {
                    self.side = Some(SideKind::Inbox);
                    self.focus = Focus::Side;
                }
                UiCommand::FocusTerminal => self.focus = Focus::Terminal,
                UiCommand::Screenshot { path, reply } => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
                    self.screenshots.push((path, reply));
                }
            }
        }
    }

    fn panel_command_for(&mut self, tab_id: &str, kind: SideKind, cmd: PanelCmd) {
        let Some((ws, tab)) = self.engine.find_tab(tab_id) else { return };
        match kind {
            SideKind::Viewer => self
                .viewers
                .entry(tab_id.to_string())
                .or_insert_with(|| crate::viewer::Viewer::new(&self.engine, &ws, &tab))
                .command(cmd, &self.engine),
            SideKind::Plan => self
                .plans
                .entry(tab_id.to_string())
                .or_insert_with(|| crate::plan_view::PlanView::new(&self.engine, &ws, &tab))
                .command(cmd, &self.engine),
            SideKind::Inbox => self.inbox.command(cmd, &self.engine),
        }
    }

    fn run_action(&mut self, action: Action) {
        let engine = self.engine.clone();
        let ws = engine.selected_workspace();
        let tab = ws.as_ref().and_then(|w| w.selected().cloned());
        match action {
            Action::NewWorkspace => self.dialog = Some(Dialog::new_workspace()),
            Action::NextWorkspace => engine.cycle_workspace(1),
            Action::PrevWorkspace => engine.cycle_workspace(-1),
            Action::GoToTab(n) => {
                engine.select_tab_index(n);
                self.focus = Focus::Terminal;
            }
            Action::NewEditTab => {
                if ws.is_some() {
                    self.dialog = Some(Dialog::new_edit_tab());
                } else {
                    engine.toast_err("create a workspace first (⌘N)");
                }
            }
            Action::NewReadonlyTab => {
                if let Some(ws) = ws {
                    let id = ws.row.id.clone();
                    engine.spawn("read-only tab", move |e| {
                        e.create_tab(&id, NewTab { kind: Some("readonly".into()), ..Default::default() }).map(|_| ())
                    });
                    self.focus = Focus::Terminal;
                } else {
                    engine.toast_err("create a workspace first (⌘N)");
                }
            }
            Action::CloseTab => {
                if let Some(t) = tab {
                    self.dialog = Some(Dialog::confirm_close(&t));
                }
            }
            Action::ToggleFocus => {
                self.focus = match (self.focus, self.side) {
                    (Focus::Terminal, Some(_)) => Focus::Side,
                    (Focus::Terminal, None) => {
                        self.side = Some(SideKind::Viewer);
                        Focus::Side
                    }
                    (Focus::Side, _) => Focus::Terminal,
                }
            }
            Action::OpenViewer => self.open_side(SideKind::Viewer, Some(PanelCmd::FilePicker)),
            Action::OpenPlan => self.open_side(SideKind::Plan, Some(PanelCmd::Reload)),
            Action::OpenInbox => self.open_side(SideKind::Inbox, None),
            Action::Comment => self.side_command(PanelCmd::Comment),
            Action::Submit => self.side_command(PanelCmd::Submit),
            Action::ToggleDiff => {
                if self.side.is_none() {
                    self.side = Some(SideKind::Viewer);
                }
                self.focus = Focus::Side;
                self.side_command(PanelCmd::ToggleDiff);
            }
            Action::ToggleOutline => {
                if self.side.is_none() {
                    self.side = Some(SideKind::Viewer);
                }
                self.focus = Focus::Side;
                self.side_command(PanelCmd::ToggleOutline);
            }
            Action::CloseSide => {
                self.side = None;
                self.focus = Focus::Terminal;
            }
        }
    }

    /// Open (or, when it is already open and focused, re-trigger) a side panel.
    fn open_side(&mut self, kind: SideKind, cmd_if_focused: Option<PanelCmd>) {
        if self.side == Some(kind) && self.focus == Focus::Side {
            if let Some(cmd) = cmd_if_focused {
                self.side_command(cmd);
            } else {
                self.side = None;
                self.focus = Focus::Terminal;
            }
            return;
        }
        self.side = Some(kind);
        self.focus = Focus::Side;
        if kind == SideKind::Plan {
            self.side_command(PanelCmd::Reload);
        }
    }

    fn side_command(&mut self, cmd: PanelCmd) {
        let Some(kind) = self.side else { return };
        if kind == SideKind::Inbox {
            self.inbox.command(cmd, &self.engine);
            return;
        }
        let Some(tab) = self.engine.selected_workspace().and_then(|w| w.selected().map(|t| t.row.id.clone())) else {
            return;
        };
        self.panel_command_for(&tab, kind, cmd);
    }

    // ------------------------------------------------------------ drawing

    fn sidebar(&mut self, ui: &mut Ui) {
        let workspaces = self.engine.workspaces();
        let selected = self.engine.state().selected.clone();
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(RichText::new("WORKSPACES").size(11.0).color(theme::DIM).strong());
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, ws) in workspaces.iter().enumerate() {
                let is_sel = selected.as_deref() == Some(&ws.row.id);
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 58.0), Sense::click());
                if is_sel {
                    ui.painter().rect_filled(rect.shrink2(egui::vec2(4.0, 1.0)), 6.0, theme::SELECTED);
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(rect.min + egui::vec2(4.0, 8.0), egui::vec2(3.0, rect.height() - 16.0)),
                        2.0,
                        theme::ACCENT,
                    );
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect.shrink2(egui::vec2(4.0, 1.0)), 6.0, Color32::from_rgb(0x1c, 0x1d, 0x28));
                }
                let x = rect.left() + 14.0;
                let p = ui.painter();
                let host_color = if ws.host().is_local() { theme::GREEN } else { theme::PURPLE };
                p.text(egui::pos2(x, rect.top() + 8.0), egui::Align2::LEFT_TOP, &ws.row.name, egui::FontId::proportional(14.0), theme::TEXT);
                let unread = ws.tabs.iter().any(|t| t.unread);
                if unread {
                    p.circle_filled(egui::pos2(rect.right() - 16.0, rect.top() + 15.0), 4.0, theme::YELLOW);
                }
                let hint = if i < 9 { format!("{}", i + 1) } else { String::new() };
                p.text(egui::pos2(rect.right() - 28.0, rect.top() + 9.0), egui::Align2::RIGHT_TOP, hint, egui::FontId::monospace(10.0), theme::DIM);
                let sub = format!("{} · {} · {} tab{}", ws.row.host, ws.repo_name(), ws.tabs.len(), if ws.tabs.len() == 1 { "" } else { "s" });
                p.text(egui::pos2(x, rect.top() + 27.0), egui::Align2::LEFT_TOP, sub, egui::FontId::proportional(11.5), host_color);
                let (status, color) = match (&ws.error, &ws.status) {
                    (Some(e), _) => (format!("⚠ {e}"), theme::RED),
                    (None, Some(s)) => (format!("⋯ {s}"), theme::YELLOW),
                    _ => (String::new(), theme::DIM),
                };
                if !status.is_empty() {
                    let status: String = status.chars().take(40).collect();
                    p.text(egui::pos2(x, rect.top() + 42.0), egui::Align2::LEFT_TOP, status, egui::FontId::proportional(10.5), color);
                }
                if resp.clicked() {
                    self.engine.select_workspace(&ws.row.id);
                    self.focus = Focus::Terminal;
                }
                resp.context_menu(|ui| {
                    if ui.button("Re-run setup (dotfiles, skills, kitchenctl)").clicked() {
                        let id = ws.row.id.clone();
                        self.engine.spawn("bootstrap", move |e| e.bootstrap_workspace(&id));
                        ui.close();
                    }
                    if ui.button("Remove workspace").clicked() {
                        self.dialog = Some(Dialog::confirm_close_workspace(ws));
                        ui.close();
                    }
                });
            }
            if workspaces.is_empty() {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No workspaces").color(theme::DIM));
                    ui.label(RichText::new("⌘N to create one").color(theme::DIM).size(11.0));
                });
            }
        });
    }

    fn footer(&mut self, ui: &mut Ui) {
        let drafts = self.inbox.pending_count();
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            if ui.add(egui::Button::new(RichText::new("＋ Workspace  ⌘N").size(12.0)).frame(false)).clicked() {
                self.run_action(Action::NewWorkspace);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(8.0);
                let label = if drafts > 0 { format!("Inbox {drafts}  ⌘I") } else { "Inbox  ⌘I".into() };
                let color = if drafts > 0 { theme::YELLOW } else { theme::DIM };
                if ui.add(egui::Button::new(RichText::new(label).size(12.0).color(color)).frame(false)).clicked() {
                    self.run_action(Action::OpenInbox);
                }
            });
        });
    }

    fn tab_strip(&mut self, ui: &mut Ui, ws: &Workspace) {
        ui.horizontal_centered(|ui| {
            ui.add_space(6.0);
            let sel = ws.selected().map(|t| t.row.id.clone());
            for (i, tab) in ws.tabs.iter().enumerate() {
                let is_sel = sel.as_deref() == Some(&tab.row.id);
                let (badge, badge_color) = match tab.kind() {
                    TabKind::Edit => ("RW", theme::GREEN),
                    TabKind::Readonly => ("RO", theme::CYAN),
                };
                let title: String = tab.row.title.chars().take(28).collect();
                let mut job = egui::text::LayoutJob::default();
                let fmt = |c: Color32, size: f32| egui::TextFormat { font_id: egui::FontId::proportional(size), color: c, ..Default::default() };
                job.append(&format!("{} ", i + 1), 0.0, fmt(theme::DIM, 11.0));
                job.append(badge, 0.0, fmt(badge_color, 10.5));
                job.append(&format!("  {title}"), 0.0, fmt(if is_sel { theme::TEXT } else { theme::DIM }, 12.5));
                if tab.unread {
                    job.append("  ●", 0.0, fmt(theme::YELLOW, 11.0));
                }
                let btn = egui::Button::new(job)
                    .fill(if is_sel { theme::SELECTED } else { Color32::TRANSPARENT })
                    .corner_radius(5.0)
                    .min_size(egui::vec2(0.0, 24.0));
                let resp = ui.add(btn).on_hover_text(format!(
                    "{}\n{}{}",
                    tab.row.worktree,
                    tab.row.branch.as_deref().map(|b| format!("branch {b}")).unwrap_or_else(|| "detached".into()),
                    tab.row.pr_number.map(|n| format!(" · PR #{n}")).unwrap_or_default()
                ));
                if resp.clicked() {
                    self.engine.select_tab(&tab.row.id);
                    self.focus = Focus::Terminal;
                }
                if resp.middle_clicked() {
                    self.dialog = Some(Dialog::confirm_close(tab));
                }
            }
            ui.add_space(8.0);
            if ui.add(egui::Button::new(RichText::new("＋ edit ⌘E").size(11.5).color(theme::GREEN)).frame(false)).clicked() {
                self.run_action(Action::NewEditTab);
            }
            if ui.add(egui::Button::new(RichText::new("＋ read-only ⌘R").size(11.5).color(theme::CYAN)).frame(false)).clicked() {
                self.run_action(Action::NewReadonlyTab);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(8.0);
                for (kind, label, action) in [
                    (SideKind::Inbox, "Inbox ⌘I", Action::OpenInbox),
                    (SideKind::Plan, "Plan ⌘L", Action::OpenPlan),
                    (SideKind::Viewer, "Files ⌘P", Action::OpenViewer),
                ] {
                    let on = self.side == Some(kind);
                    let text = RichText::new(label).size(11.5).color(if on { theme::ACCENT } else { theme::DIM });
                    if ui.add(egui::Button::new(text).frame(false)).clicked() {
                        if on {
                            self.side = None;
                            self.focus = Focus::Terminal;
                        } else {
                            self.run_action(action);
                        }
                    }
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let toast = {
            let st = self.engine.state();
            st.toasts.last().filter(|t| t.at.elapsed() < Duration::from_secs(if t.error { 12 } else { 5 })).cloned()
        };
        ui.horizontal_centered(|ui| {
            ui.add_space(8.0);
            let focus = match (self.focus, self.side) {
                (Focus::Terminal, _) => "TERMINAL",
                (Focus::Side, Some(SideKind::Viewer)) => "VIEWER",
                (Focus::Side, Some(SideKind::Plan)) => "PLAN",
                (Focus::Side, Some(SideKind::Inbox)) => "INBOX",
                (Focus::Side, None) => "TERMINAL",
            };
            ui.label(RichText::new(focus).size(10.5).strong().color(theme::BG).background_color(theme::ACCENT));
            if let Some(t) = toast {
                ui.label(RichText::new(t.text).size(11.5).color(if t.error { theme::RED } else { theme::TEXT }));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(8.0);
                ui.label(
                    RichText::new("⌘J/K workspace · ⌘1-9 tab · ⌘` focus · ⌘M comment · ⌘D diff · ⌘O outline")
                        .size(10.5)
                        .color(theme::DIM),
                );
            });
        });
        if self.engine.state().toasts.last().map(|t| t.at.elapsed() < Duration::from_secs(12)).unwrap_or(false) {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    }

    fn terminal_area(&mut self, ui: &mut Ui, ws: &Workspace, tab: &Tab) {
        self.ensure_terminal(ui.ctx(), ws, tab);
        let font_size = self.engine.settings().font_size;
        let focused = self.focus == Focus::Terminal && self.dialog.is_none();
        match self.slots.get_mut(&tab.row.id) {
            Some(TermSlot::Live(session)) => {
                if session.take_bell() {
                    // A bell in a background tab marks it unread.
                }
                let resp = TerminalView::new(session).font_size(font_size).focused(focused).show(ui);
                if resp.clicked() || resp.drag_started() {
                    self.focus = Focus::Terminal;
                }
            }
            Some(TermSlot::Preparing) | None => centered_note(ui, &format!("attaching to {} on {}…", tab.row.tmux_session, ws.row.host), theme::DIM),
            Some(TermSlot::Failed { msg, attempts, .. }) => {
                let msg = format!("cannot attach (attempt {attempts}): {msg}\nretrying…");
                centered_note(ui, &msg, theme::RED);
                ui.ctx().request_repaint_after(Duration::from_secs(1));
            }
            Some(TermSlot::Gone(msg)) => {
                let msg = format!("{msg}\n⌘W closes this tab");
                centered_note(ui, &msg, theme::RED)
            }
        }
    }

    fn side_area(&mut self, ui: &mut Ui, ws: &Workspace, tab: Option<&Tab>) {
        let focused = self.focus == Focus::Side && self.dialog.is_none();
        let kind = self.side.unwrap_or(SideKind::Viewer);
        let mut cx = PanelCtx { engine: &self.engine, focused, clicked: false };
        match (kind, tab) {
            (SideKind::Inbox, _) => self.inbox.ui(ui, &mut cx),
            (SideKind::Viewer, Some(tab)) => self
                .viewers
                .entry(tab.row.id.clone())
                .or_insert_with(|| crate::viewer::Viewer::new(&self.engine, ws, tab))
                .ui(ui, &mut cx),
            (SideKind::Plan, Some(tab)) => self
                .plans
                .entry(tab.row.id.clone())
                .or_insert_with(|| crate::plan_view::PlanView::new(&self.engine, ws, tab))
                .ui(ui, &mut cx),
            (_, None) => centered_note(ui, "open a tab first (⌘E / ⌘R)", theme::DIM),
        }
        if cx.clicked {
            self.focus = Focus::Side;
        }
    }

    fn save_screenshots(&mut self, ctx: &egui::Context) {
        let images: Vec<_> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| if let egui::Event::Screenshot { image, .. } = e { Some(image.clone()) } else { None })
                .collect()
        });
        for image in images {
            let result = write_png(&image);
            if let Some((path, reply)) = (!self.screenshots.is_empty()).then(|| self.screenshots.remove(0)) {
                let r = result.and_then(|bytes| std::fs::write(&path, bytes).map_err(|e| e.to_string()));
                let _ = reply.send(r);
            } else if let Some((_, path)) = self.auto_shot.take() {
                if let Ok(bytes) = result {
                    let _ = std::fs::write(&path, bytes);
                }
                std::process::exit(0);
            }
        }
        if let Some((at, _)) = &self.auto_shot {
            if Instant::now() >= *at {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            } else {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
    }

    fn drop_stale_state(&mut self) {
        let live: std::collections::HashSet<String> =
            self.engine.workspaces().iter().flat_map(|w| w.tabs.iter().map(|t| t.row.id.clone())).collect();
        self.slots.retain(|id, _| live.contains(id));
        self.viewers.retain(|id, _| live.contains(id));
        self.plans.retain(|id, _| live.contains(id));
    }
}

impl eframe::App for KitchenApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_attach(&ctx);
        self.handle_ui_commands(&ctx);
        self.drop_stale_state();

        if let Some(dialog) = &mut self.dialog {
            match dialog.show(&ctx, &self.engine) {
                DialogOutcome::Open => {}
                DialogOutcome::Closed => self.dialog = None,
                DialogOutcome::Done => {
                    self.dialog = None;
                    self.focus = Focus::Terminal;
                }
            }
        } else {
            for action in keymap::collect(&ctx) {
                self.run_action(action);
            }
            // Esc in a side panel that did not use it hands focus back.
            if self.focus == Focus::Side && ctx.input(|i| i.modifiers.command && i.key_pressed(Key::Escape)) {
                self.focus = Focus::Terminal;
            }
        }

        let ws = self.engine.selected_workspace();
        let tab = ws.as_ref().and_then(|w| w.selected().cloned());

        egui::Panel::left("workspaces")
            .exact_size(236.0)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(theme::SIDEBAR))
            .show(ui, |ui| {
                egui::Panel::bottom("sidebar-footer").frame(egui::Frame::NONE.fill(theme::SIDEBAR).inner_margin(6.0)).show(ui, |ui| self.footer(ui));
                self.sidebar(ui);
            });
        egui::Panel::bottom("status")
            .exact_size(22.0)
            .frame(egui::Frame::NONE.fill(theme::SIDEBAR))
            .show(ui, |ui| self.status_bar(ui));
        if let Some(ws) = &ws {
            egui::Panel::top("tabs")
                .exact_size(34.0)
                .frame(egui::Frame::NONE.fill(theme::PANEL).inner_margin(egui::Margin::symmetric(0, 4)))
                .show(ui, |ui| self.tab_strip(ui, ws));
        }
        if self.side.is_some() {
            if let Some(ws) = &ws {
                let width = ui.available_width();
                egui::Panel::right("side")
                    .resizable(true)
                    .default_size(width * 0.5)
                    .size_range(280.0..=width - 200.0)
                    .frame(egui::Frame::NONE.fill(theme::BG).stroke(egui::Stroke::new(1.0, if self.focus == Focus::Side { theme::ACCENT } else { theme::BORDER })))
                    .show(ui, |ui| self.side_area(ui, ws, tab.as_ref()));
            }
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(theme::BG)).show(ui, |ui| match (&ws, &tab) {
            (Some(ws), Some(tab)) => self.terminal_area(ui, ws, tab),
            (Some(ws), None) => {
                let status = ws.status.clone().map(|s| format!("\n\n{s}")).unwrap_or_default();
                centered_note(ui, &format!("{}\n\n⌘E  new edit tab (own worktree + branch, starts planning)\n⌘R  new read-only tab (latest {})\n{status}", ws.row.name, ws.row.default_branch.as_deref().unwrap_or("main")), theme::DIM)
            }
            (None, _) => centered_note(ui, "the kitchen\n\n⌘N  new workspace (local repo or devbox)", theme::DIM),
        });
        self.save_screenshots(&ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }
}

fn centered_note(ui: &mut Ui, text: &str, color: Color32) {
    ui.centered_and_justified(|ui| {
        ui.label(RichText::new(text).color(color).size(13.0));
    });
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

fn write_png(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, image.size[0] as u32, image.size[1] as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| e.to_string())?;
        w.write_image_data(image.as_raw()).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

#[allow(dead_code)]
fn _assert_modifiers(_: Modifiers) {}
