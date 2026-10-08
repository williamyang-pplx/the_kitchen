//! The window: workspace sidebar, tab strip, terminal, side panel
//! (viewer / plan), dialogs, and the Cmd keymap.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use egui::{Align, Color32, Key, Layout, Modifiers, RichText, Sense, Ui};

use crate::engine::{Engine, Tab, UiCommand, Workspace};
use crate::keymap::{self, Action};
use crate::panels::{PanelCmd, PanelCtx};
use crate::terminal::{TermSession, TerminalView};
use crate::theme;
use crate::ui::dialogs::{Dialog, DialogOutcome};
use crate::ui::widgets;

/// Height of the macOS title bar the window draws under.
const TITLEBAR_HEIGHT: f32 = 28.0;

/// What fills the tab area (and has the keyboard): the terminal, or the
/// tab's side panel (viewer / plan). ⌘. switches between them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Terminal,
    Side,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SideKind {
    Viewer,
    Plan,
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
    /// Consecutive failed attach attempts per tab (drives the retry backoff).
    attempts: HashMap<String, u32>,
    attach_tx: Sender<(String, anyhow::Result<bool>)>,
    attach_rx: Receiver<(String, anyhow::Result<bool>)>,
    /// Tabs showing their side panel instead of the terminal (⌘.); each tab
    /// keeps its own choice when you switch away and back.
    side_shown: HashSet<String>,
    /// The side panel per tab id; no entry = none opened yet.
    sides: HashMap<String, SideKind>,
    viewers: HashMap<String, crate::viewer::Viewer>,
    plans: HashMap<String, crate::plan_view::PlanView>,
    dialog: Option<Dialog>,
    screenshots: Vec<(String, Sender<Result<(), String>>)>,
    /// Input events from `kitchenctl debug keys`, fed to the next frame.
    injected: Vec<egui::Event>,
    /// ⌘Q pressed; closes the window at the end of the frame.
    quit_requested: bool,
    /// Test hook: take a screenshot after a delay, then exit.
    auto_shot: Option<(Instant, String)>,
    /// ⌘⇧P command palette.
    palette: Option<(Vec<PaletteItem>, crate::viewer::picker::Picker)>,
    /// The ⌘/ shortcut list is showing.
    shortcuts: bool,
    /// Workspaces whose dashboard (⌘D) replaces the terminal area.
    dashboard: HashSet<String>,
    /// The workspace plugins were last told has its dashboard on screen.
    dashboard_shown: Option<String>,
}

#[derive(Clone)]
enum PaletteItem {
    Command(String),
    ReloadPlugins,
    Diagnostics,
}

impl KitchenApp {
    pub fn new(engine: Arc<Engine>) -> Self {
        let (attach_tx, attach_rx) = channel();
        let auto_shot = std::env::var("KITCHEN_SCREENSHOT").ok().map(|p| {
            let secs = std::env::var("KITCHEN_SCREENSHOT_DELAY").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
            (Instant::now() + Duration::from_secs(secs), p)
        });
        Self {
            engine,
            slots: HashMap::new(),
            attempts: HashMap::new(),
            attach_tx,
            attach_rx,
            side_shown: HashSet::new(),
            sides: HashMap::new(),
            viewers: HashMap::new(),
            plans: HashMap::new(),
            dialog: None,
            shortcuts: false,
            screenshots: Vec::new(),
            injected: Vec::new(),
            quit_requested: false,
            auto_shot,
            palette: None,
            dashboard: HashSet::new(),
            dashboard_shown: None,
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
        if let Some(TermSlot::Failed { attempts, .. }) = self.slots.get(&id) {
            self.attempts.insert(id.clone(), *attempts);
        }
        self.slots.insert(id.clone(), TermSlot::Preparing);
        let engine = self.engine.clone();
        let tx = self.attach_tx.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let r = engine.ensure_session(&id);
            let _ = tx.send((id, r));
            ctx2.request_repaint();
        });
        let _ = ws;
    }

    fn poll_attach(&mut self, ctx: &egui::Context) {
        while let Ok((tab_id, result)) = self.attach_rx.try_recv() {
            let Some((ws, tab)) = self.engine.find_tab(&tab_id) else {
                self.slots.remove(&tab_id);
                continue;
            };
            let prev_attempts = self.attempts.get(&tab_id).copied().unwrap_or(0);
            let slot = match result {
                Ok(true) => {
                    let argv = ws.host().tmux_attach_argv(&tab.row.tmux_session);
                    let mut env = HashMap::new();
                    env.insert("LANG".into(), std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into()));
                    match TermSession::spawn(ctx, &argv, Some(&home()), env) {
                        Ok(mut s) => {
                            self.attempts.remove(&tab_id);
                            s.set_predictive_echo(!ws.host().is_local());
                            if !ws.host().is_local() {
                                // Claude Code redraws ~6KB per wheel step; batch them per 25ms.
                                s.set_wheel_interval(Duration::from_millis(25));
                            }
                            TermSlot::Live(s)
                        }
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
                    self.open_side_for(&tab_id, SideKind::Viewer);
                    self.panel_command_for(&tab_id, SideKind::Viewer, PanelCmd::Open { path, line, diff });
                }
                UiCommand::OpenPlan { tab_id } => {
                    self.open_side_for(&tab_id, SideKind::Plan);
                    self.panel_command_for(&tab_id, SideKind::Plan, PanelCmd::Reload);
                }
                UiCommand::ResetViewer { tab_id, view: None } => {
                    self.viewers.remove(&tab_id);
                }
                UiCommand::ResetViewer { tab_id, view: Some((worktree, base)) } => {
                    let Some((ws, _)) = self.engine.find_tab(&tab_id) else { continue };
                    self.viewers.insert(tab_id.clone(), crate::viewer::Viewer::at(&self.engine, ws.host(), &tab_id, worktree, base));
                    self.open_side_for(&tab_id, SideKind::Viewer);
                }
                UiCommand::Render { tab_id, on } => {
                    if let Some(v) = self.viewers.get_mut(&tab_id) {
                        v.command(PanelCmd::Render(on), &self.engine);
                    }
                }
                UiCommand::ReloadPlan { tab_id } => {
                    if let Some(p) = self.plans.get_mut(&tab_id) {
                        p.command(PanelCmd::Reload, &self.engine);
                    }
                }
                UiCommand::Screenshot { path, reply } => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
                    self.screenshots.push((path, reply));
                }
                UiCommand::Keys(events) => {
                    self.injected.extend(events);
                    ctx.request_repaint();
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
                self.hide_dashboard();
            }
            Action::FocusFileList => self.side_command(PanelCmd::FocusList),
            Action::FocusCode => self.side_command(PanelCmd::FocusCode),
            Action::NextFile => self.side_command(PanelCmd::NextFile),
            Action::PrevFile => self.side_command(PanelCmd::PrevFile),
            Action::Palette => self.open_palette(),
            Action::Shortcuts => self.shortcuts = !self.shortcuts,
            Action::Plugin(_) => {} // resolved in `ui` against the registry snapshot
            Action::PrevTab => {
                engine.cycle_tab(-1);
                self.hide_dashboard();
            }
            Action::NextTab => {
                engine.cycle_tab(1);
                self.hide_dashboard();
            }
            Action::Quit => self.quit_requested = true,
            Action::NewTab => {
                if let Some(ws) = &ws {
                    // Fetch and refresh the spares while the name is typed.
                    engine.refill_spare(&ws.row.id, true);
                    self.dialog = Some(Dialog::new_tab());
                } else {
                    engine.toast_err("create a workspace first (⌘N)");
                }
            }
            Action::RenameTab => {
                if let Some(t) = tab {
                    self.dialog = Some(Dialog::rename_tab(&t));
                }
            }
            Action::RenameWorkspace => {
                if let Some(ws) = ws {
                    self.dialog = Some(Dialog::rename_workspace(&ws));
                }
            }
            Action::CloseTab => {
                if let Some(t) = tab {
                    self.dialog = Some(Dialog::confirm_close(&t));
                }
            }
            // VS Code's ⌘B: the viewer's file list (opening the viewer if needed).
            Action::ToggleFiles => {
                if self.viewer_focused() {
                    self.side_command(PanelCmd::ToggleFiles);
                } else {
                    self.open_side(SideKind::Viewer, None);
                }
            }
            Action::FilePicker => self.side_command(PanelCmd::FilePicker),
            Action::OpenPlan => self.open_side(SideKind::Plan, Some(PanelCmd::Reload)),
            Action::Comment => self.side_command(PanelCmd::Comment),
            Action::Submit => self.side_command(PanelCmd::Submit),
            Action::DiffOrDashboard if self.focus() == Focus::Side && self.side().is_some() => self.side_command(PanelCmd::ToggleDiff),
            Action::DiffOrDashboard => {
                if let Some(ws) = &ws {
                    self.toggle_dashboard(&ws.row.id);
                }
            }
            Action::ToggleOutline => {
                if self.side().is_none() {
                    self.set_side(SideKind::Viewer);
                }
                self.set_focus(Focus::Side);
                self.side_command(PanelCmd::ToggleOutline);
            }
            // ⌘. switches the whole tab between the terminal and the viewer.
            Action::ToggleViewer => {
                if self.focus() == Focus::Side && self.side().is_some() {
                    self.set_focus(Focus::Terminal);
                } else {
                    self.open_side(SideKind::Viewer, None);
                }
            }
        }
    }

    fn toggle_dashboard(&mut self, ws: &str) {
        if !self.dashboard.remove(ws) {
            self.dashboard.insert(ws.to_string());
        }
        self.set_focus(Focus::Terminal);
    }

    /// Back to the terminal: the selected workspace's dashboard goes away.
    fn hide_dashboard(&mut self) {
        if let Some(ws) = self.engine.selected_workspace() {
            self.dashboard.remove(&ws.row.id);
        }
    }

    /// Tell plugins which workspace's dashboard is on screen (at most the selected one).
    fn sync_dashboard(&mut self, selected: Option<&str>) {
        let now = selected.filter(|id| self.dashboard.contains(*id)).map(str::to_string);
        if now == self.dashboard_shown {
            return;
        }
        if let Some(plugins) = self.engine.plugins() {
            if let Some(old) = &self.dashboard_shown {
                plugins.dashboard_shown(old, false);
            }
            if let Some(new) = &now {
                plugins.dashboard_shown(new, true);
            }
        }
        self.dashboard_shown = now;
    }

    /// Show a side panel (or, when it is already shown, re-trigger it or go
    /// back to the terminal).
    fn open_side(&mut self, kind: SideKind, cmd_if_focused: Option<PanelCmd>) {
        if self.side() == Some(kind) && self.focus() == Focus::Side {
            match cmd_if_focused {
                Some(cmd) => self.side_command(cmd),
                None => self.set_focus(Focus::Terminal),
            }
            return;
        }
        self.set_side(kind);
        self.set_focus(Focus::Side);
        if kind == SideKind::Plan {
            self.side_command(PanelCmd::Reload);
        }
    }

    /// Where plugin chords apply right now.
    fn plugin_context(&self) -> crate::plugins::When {
        use crate::plugins::When;
        match (self.focus(), self.side()) {
            (Focus::Side, Some(SideKind::Viewer)) => When::Viewer,
            (Focus::Side, Some(SideKind::Plan)) => When::Plan,
            _ => When::Terminal,
        }
    }

    fn selected_tab_id(&self) -> Option<String> {
        self.engine.selected_workspace().and_then(|w| w.selected().map(|t| t.row.id.clone()))
    }

    /// What the selected tab shows: its side panel if ⌘. put it there.
    fn focus(&self) -> Focus {
        match self.selected_tab_id() {
            Some(id) if self.side_shown.contains(&id) && self.sides.contains_key(&id) => Focus::Side,
            _ => Focus::Terminal,
        }
    }

    fn set_focus(&mut self, focus: Focus) {
        let Some(id) = self.selected_tab_id() else { return };
        match focus {
            Focus::Side => self.side_shown.insert(id),
            Focus::Terminal => self.side_shown.remove(&id),
        };
    }

    /// The side panel open on the selected tab.
    fn side(&self) -> Option<SideKind> {
        self.selected_tab_id().and_then(|id| self.sides.get(&id).copied())
    }

    /// Make `kind` the selected tab's side panel.
    fn set_side(&mut self, kind: SideKind) {
        if let Some(id) = self.selected_tab_id() {
            self.sides.insert(id, kind);
        }
    }

    /// Open `kind` on `tab_id`; it takes the keyboard only if that tab is
    /// the selected one.
    fn open_side_for(&mut self, tab_id: &str, kind: SideKind) {
        self.sides.insert(tab_id.to_string(), kind);
        if self.selected_tab_id().as_deref() == Some(tab_id) {
            self.set_focus(Focus::Side);
        }
    }

    /// Plugin chords for the focused context (context-specific ones first).
    fn plugin_chords(&self) -> (Option<Arc<crate::plugins::Registry>>, keymap::PluginChords) {
        use crate::plugins::When;
        let Some(p) = self.engine.plugins() else { return (None, Vec::new()) };
        let reg = p.registry();
        if reg.chords.is_empty() {
            return (Some(reg), Vec::new());
        }
        let here = self.plugin_context();
        let ft = if here == When::Viewer { self.selected_tab_id().and_then(|t| self.viewers.get(&t)).and_then(|v| v.current_filetype()) } else { None };
        let mut chords: Vec<(usize, crate::plugins::keys::Chord, bool)> = reg
            .chords
            .iter()
            .enumerate()
            .filter(|(_, c)| (c.when == When::Any || c.when == here) && c.filetype.as_ref().is_none_or(|f| ft.as_ref() == Some(f)))
            .map(|(i, c)| (i, c.chord, c.when == When::Any))
            .collect();
        chords.sort_by_key(|c| c.2);
        (Some(reg.clone()), chords.into_iter().map(|(i, c, _)| (i, c)).collect())
    }

    fn run_plugin_command(&mut self, command: &str) {
        let Some(p) = self.engine.plugins().cloned() else { return };
        let tab = self.selected_tab_id().unwrap_or_default();
        let viewer = self.viewers.get(&tab).map(|v| v.context());
        let ctx = crate::plugins::command_context(&self.engine, &tab, viewer);
        if let Err(e) = p.invoke(command, ctx, false) {
            self.engine.toast_err(format!("{e:#}"));
        }
    }

    fn open_palette(&mut self) {
        if self.palette.is_some() {
            self.palette = None;
            return;
        }
        let mut items: Vec<(PaletteItem, (String, String))> = Vec::new();
        if let Some(p) = self.engine.plugins() {
            let reg = p.registry();
            for c in &reg.commands {
                let chords: Vec<String> = reg.chords.iter().filter(|k| k.command == c.name).map(|k| k.chord.label()).chain(reg.viewer_maps.iter().filter(|m| m.command == c.name).map(|m| format!("\\{}", m.keys))).collect();
                let detail = [c.desc.clone(), chords.join(" ")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  ·  ");
                items.push((PaletteItem::Command(c.name.clone()), (c.name.clone(), detail)));
            }
        }
        items.push((PaletteItem::Diagnostics, ("viewer: diagnostics in this file".into(), "]d / [d".into())));
        items.push((PaletteItem::ReloadPlugins, ("plugins: reload all".into(), format!("{}", kitchen_proto::state_dir().join("plugins").display()))));
        let (kinds, labels): (Vec<PaletteItem>, Vec<(String, String)>) = items.into_iter().unzip();
        let mut picker = crate::viewer::picker::Picker::new("Commands", labels);
        picker.hint = "Run a command, or paste a PR link".into();
        self.palette = Some((kinds, picker));
    }

    fn palette_ui(&mut self, ctx: &egui::Context) {
        let Some((_, picker)) = &mut self.palette else { return };
        // Enter on a link (`owner/repo#12`, a URL): the plugins' `open`
        // handlers get it with the selected tab's command ctx.
        let link = Some(picker.query.trim().to_string()).filter(|q| crate::ui::dialogs::is_link(q));
        if let Some(link) = link.filter(|_| ctx.input(|i| i.key_pressed(Key::Enter))) {
            self.palette = None;
            let mut ev = crate::plugins::command_context(&self.engine, &self.selected_tab_id().unwrap_or_default(), None);
            ev["text"] = link.clone().into();
            if !self.engine.plugins().is_some_and(|p| p.emit("open", ev)) {
                self.engine.toast_err(format!("no plugin opens {link}"));
            }
            return;
        }
        let mut result = crate::viewer::picker::PickerResult::Open;
        egui::Area::new(egui::Id::new("palette")).order(egui::Order::Foreground).anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 96.0)).show(ctx, |ui| {
            ui.set_width(640.0);
            ui.set_max_height(460.0);
            result = picker.ui(ui, true);
        });
        match result {
            crate::viewer::picker::PickerResult::Open => {}
            crate::viewer::picker::PickerResult::Cancelled => self.palette = None,
            crate::viewer::picker::PickerResult::Chosen(i) => {
                let item = self.palette.take().and_then(|(kinds, _)| kinds.get(i).cloned());
                match item {
                    Some(PaletteItem::Command(c)) => self.run_plugin_command(&c),
                    Some(PaletteItem::ReloadPlugins) => {
                        if let Some(p) = self.engine.plugins().cloned() {
                            match p.reload(None) {
                                Ok(names) => self.engine.toast(format!("reloaded {} plugin(s)", names.len())),
                                Err(e) => self.engine.toast_err(format!("{e:#}")),
                            }
                        }
                    }
                    Some(PaletteItem::Diagnostics) => {
                        if let Some(tab) = self.selected_tab_id() {
                            self.set_side(SideKind::Viewer);
                            self.set_focus(Focus::Side);
                            self.panel_command_for(&tab, SideKind::Viewer, PanelCmd::Diagnostics);
                        }
                    }
                    None => {}
                }
            }
        }
    }

    fn viewer_focused(&self) -> bool {
        self.focus() == Focus::Side && self.side() == Some(SideKind::Viewer)
    }

    fn side_command(&mut self, cmd: PanelCmd) {
        let Some(kind) = self.side() else { return };
        let Some(tab) = self.engine.selected_workspace().and_then(|w| w.selected().map(|t| t.row.id.clone())) else {
            return;
        };
        self.panel_command_for(&tab, kind, cmd);
    }

    // ------------------------------------------------------------ drawing

    fn sidebar(&mut self, ui: &mut Ui) {
        let workspaces = self.engine.workspaces();
        let selected = self.engine.state().selected.clone();
        let pinned_count = workspaces.iter().filter(|w| w.row.pinned).count();
        let header = |ui: &mut Ui, text: &str| {
            ui.add_space(theme::SPACE_M);
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                ui.label(widgets::section_label(text, theme::SUBTLE));
            });
            ui.add_space(theme::SPACE_XS);
        };
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let mut rects = Vec::with_capacity(workspaces.len());
            // (index, released this frame) of the row being dragged.
            let mut drag: Option<(usize, bool)> = None;
            for (i, ws) in workspaces.iter().enumerate() {
                // Pinned workspaces get their own group above the rest.
                if i == 0 {
                    header(ui, if pinned_count > 0 { "Pinned" } else { "Workspaces" });
                } else if i == pinned_count {
                    header(ui, "Workspaces");
                }
                let is_sel = selected.as_deref() == Some(&ws.row.id);
                let (status, status_color) = match (&ws.error, &ws.status) {
                    (Some(e), _) => (format!("⚠ {e}"), theme::RED),
                    (None, Some(s)) => (format!("⋯ {s}"), theme::YELLOW),
                    _ => (String::new(), theme::DIM),
                };
                let height = if status.is_empty() { 46.0 } else { 62.0 };
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click_and_drag());
                rects.push(rect);
                if resp.dragged_by(egui::PointerButton::Primary) {
                    drag = Some((i, false));
                } else if resp.drag_stopped_by(egui::PointerButton::Primary) {
                    drag = Some((i, true));
                }
                let card = rect.shrink2(egui::vec2(8.0, 1.0));
                if drag.is_some_and(|(d, _)| d == i) || (resp.hovered() && !is_sel) {
                    ui.painter().rect_filled(card, theme::RADIUS as f32, theme::HOVER);
                } else if is_sel {
                    ui.painter().rect_filled(card, theme::RADIUS as f32, theme::SELECTED);
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(card.min + egui::vec2(0.0, 10.0), egui::vec2(3.0, card.height() - 20.0)),
                        2.0,
                        theme::ACCENT,
                    );
                }
                let x = card.left() + 12.0;
                let p = ui.painter();
                let name_color = if is_sel { theme::TEXT } else { theme::MUTED };
                p.text(egui::pos2(x, rect.top() + 8.0), egui::Align2::LEFT_TOP, &ws.row.name, theme::ui_font_semibold(13.0), name_color);
                if ws.tabs.iter().any(|t| t.unread) {
                    p.circle_filled(egui::pos2(card.right() - 12.0, rect.top() + 16.0), 3.5, theme::YELLOW);
                }
                let tabs = match ws.tabs.len() {
                    0 => "no tabs".to_string(),
                    1 => "1 tab".to_string(),
                    n => format!("{n} tabs"),
                };
                let sub = format!("{} · {} · {tabs}", ws.row.host, ws.repo_name());
                p.text(egui::pos2(x, rect.top() + 26.0), egui::Align2::LEFT_TOP, sub, theme::ui_font(theme::TEXT_SMALL), theme::SUBTLE);
                if !status.is_empty() {
                    let status: String = status.chars().take(36).collect();
                    p.text(egui::pos2(x, rect.top() + 42.0), egui::Align2::LEFT_TOP, status, theme::ui_font(theme::TEXT_SMALL), status_color);
                }
                let resp = resp.on_hover_text(format!("{}:{}\nworkspace:{}", ws.row.host, ws.row.repo_path, i + 1));
                if resp.clicked_by(egui::PointerButton::Primary) {
                    self.engine.select_workspace(&ws.row.id);
                }
                resp.context_menu(|ui| {
                    let pin_label = if ws.row.pinned { "Unpin" } else { "Pin to top" };
                    if ui.button(pin_label).clicked_by(egui::PointerButton::Primary) {
                        if let Err(e) = self.engine.set_workspace_pinned(&ws.row.id, !ws.row.pinned) {
                            self.engine.toast_err(format!("{e:#}"));
                        }
                        ui.close();
                    }
                    if ui.button("Re-run setup (dotfiles, skills, kitchenctl)").clicked_by(egui::PointerButton::Primary) {
                        let id = ws.row.id.clone();
                        self.engine.spawn("bootstrap", move |e| e.bootstrap_workspace(&id));
                        ui.close();
                    }
                    if ui.button("Remove workspace").clicked_by(egui::PointerButton::Primary) {
                        self.dialog = Some(Dialog::confirm_close_workspace(ws));
                        ui.close();
                    }
                });
            }
            if let Some((from, released)) = drag {
                self.workspace_drag(ui, &workspaces, &rects, pinned_count, from, released);
            }
            if workspaces.is_empty() {
                header(ui, "Workspaces");
                ui.horizontal(|ui| {
                    ui.add_space(18.0);
                    ui.label(RichText::new("None yet").font(theme::ui_font(theme::TEXT_SMALL)).color(theme::SUBTLE));
                });
            }
        });
    }

    /// Drag-to-reorder in the sidebar: draw the drop line while dragging,
    /// move the workspace on release. Drops stay inside the dragged
    /// workspace's group (pinned or unpinned).
    fn workspace_drag(&mut self, ui: &Ui, workspaces: &[Workspace], rects: &[egui::Rect], pinned_count: usize, from: usize, released: bool) {
        let Some(pointer) = ui.ctx().pointer_interact_pos() else { return };
        let slot = rects.iter().position(|r| pointer.y < r.center().y).unwrap_or(rects.len());
        let slot = if workspaces[from].row.pinned { slot.min(pinned_count) } else { slot.clamp(pinned_count, rects.len()) };
        if released {
            if slot != from && slot != from + 1 {
                if let Err(e) = self.engine.move_workspace(&workspaces[from].row.id, slot) {
                    self.engine.toast_err(format!("{e:#}"));
                }
            }
            return;
        }
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        let y = match rects.get(slot) {
            Some(r) => r.top(),
            None => rects[rects.len() - 1].bottom(),
        };
        let x = rects[from].x_range().shrink(8.0);
        ui.painter().hline(x, y, egui::Stroke::new(2.0, theme::ACCENT));
    }

    /// The strip behind the transparent macOS title bar: where you are
    /// (workspace › tab, and its branch), and dragging it moves the window.
    fn titlebar(&mut self, ui: &mut Ui, ws: Option<&Workspace>, tab: Option<&Tab>) {
        let rect = ui.max_rect();
        let resp = ui.interact(rect, ui.id().with("titlebar"), Sense::click_and_drag());
        if resp.drag_started_by(egui::PointerButton::Primary) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let mut job = egui::text::LayoutJob::default();
        let fmt = |font: egui::FontId, color: Color32| egui::TextFormat { font_id: font, color, valign: Align::Center, ..Default::default() };
        match (ws, tab) {
            (Some(ws), Some(tab)) => {
                job.append(&ws.row.name, 0.0, fmt(theme::ui_font(12.0), theme::SUBTLE));
                job.append("›", 7.0, fmt(theme::ui_font(12.0), theme::DIM));
                job.append(&tab.row.title, 7.0, fmt(theme::ui_font_semibold(12.0), theme::MUTED));
                if let Some(b) = &tab.row.branch {
                    job.append(b, 10.0, fmt(egui::FontId::monospace(11.0), theme::DIM));
                }
            }
            (Some(ws), None) => job.append(&ws.row.name, 0.0, fmt(theme::ui_font_semibold(12.0), theme::MUTED)),
            _ => job.append("the kitchen", 0.0, fmt(theme::ui_font_semibold(12.0), theme::MUTED)),
        }
        let galley = ui.painter().layout_job(job);
        // Clear of the traffic lights on the left.
        let x = (rect.center().x - galley.size().x / 2.0).max(rect.left() + 84.0);
        ui.painter().galley(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley, theme::MUTED);
    }

    fn footer(&mut self, ui: &mut Ui) {
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), Sense::click());
        let card = rect.shrink2(egui::vec2(8.0, 2.0));
        if resp.hovered() {
            ui.painter().rect_filled(card, theme::RADIUS as f32, theme::HOVER);
        }
        let color = if resp.hovered() { theme::TEXT } else { theme::SUBTLE };
        let p = ui.painter();
        p.text(egui::pos2(card.left() + 12.0, card.center().y), egui::Align2::LEFT_CENTER, "+   New workspace", theme::ui_font(theme::TEXT_BODY), color);
        p.text(egui::pos2(card.right() - 10.0, card.center().y), egui::Align2::RIGHT_CENTER, "⌘N", theme::ui_font(theme::TEXT_SMALL), theme::DIM);
        if resp.clicked_by(egui::PointerButton::Primary) {
            self.run_action(Action::NewWorkspace);
        }
    }

    fn tab_strip(&mut self, ui: &mut Ui, ws: &Workspace) {
        let strip = ui.max_rect();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.add_space(theme::SPACE_S);
            let sel = ws.selected().map(|t| t.row.id.clone());
            for (i, tab) in ws.tabs.iter().enumerate() {
                let is_sel = sel.as_deref() == Some(&tab.row.id);
                let title: String = tab.row.title.chars().take(32).collect();
                let resp = tab_button(ui, strip, Some(i + 1), &title, is_sel, tab.unread, false).on_hover_text(format!(
                    "{}\n{}",
                    tab.row.worktree,
                    tab.row.branch.as_deref().map(|b| format!("branch {b}")).unwrap_or_else(|| "detached".into())
                ));
                if resp.clicked_by(egui::PointerButton::Primary) {
                    self.engine.select_tab(&tab.row.id);
                    self.dashboard.remove(&ws.row.id);
                }
                if resp.middle_clicked() {
                    self.dialog = Some(Dialog::confirm_close(tab));
                }
            }
            for p in &ws.pending {
                let title: String = p.title.chars().take(32).collect();
                tab_button(ui, strip, None, &format!("⋯ {title}"), p.focus, false, true).on_hover_text("fetching origin and preparing the worktree…");
            }
            ui.add_space(theme::SPACE_XS);
            if icon_button(ui, strip, "+").on_hover_text("New tab  ⌘T").clicked_by(egui::PointerButton::Primary) {
                self.run_action(Action::NewTab);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.add_space(theme::SPACE_S);
                // Rightmost: the dashboard (⌘D).
                let on = self.dashboard.contains(&ws.row.id);
                if crate::dashboard_view::button(ui, on).clicked_by(egui::PointerButton::Primary) {
                    self.toggle_dashboard(&ws.row.id);
                }
                for (kind, label, keys, action) in [
                    (SideKind::Plan, "Plan", "⌘⇧I", Action::OpenPlan),
                    (SideKind::Viewer, "Viewer", "⌘.", Action::ToggleFiles),
                ] {
                    let on = self.focus() == Focus::Side && self.side() == Some(kind) && !self.dashboard.contains(&ws.row.id);
                    if widgets::flat_button(ui, label, on).on_hover_text(format!("{label}  {keys}")).clicked_by(egui::PointerButton::Primary) {
                        if on {
                            self.set_focus(Focus::Terminal);
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
        let rect = ui.max_rect();
        ui.painter().hline(rect.x_range(), rect.top() + 0.5, egui::Stroke::new(1.0, theme::BORDER));
        ui.horizontal_centered(|ui| {
            ui.add_space(theme::SPACE_S);
            let (focus, hints): (&str, &[(&str, &str)]) = match (self.focus(), self.side()) {
                (Focus::Side, Some(SideKind::Viewer)) => ("VIEWER", &[("⌘P", "find file"), ("⌘M", "comment"), ("⌘D", "diff"), ("⌘.", "terminal"), ("⌘/", "shortcuts")]),
                (Focus::Side, Some(SideKind::Plan)) => ("PLAN", &[("⌘M", "comment"), ("⌘⏎", "submit"), ("⌘.", "viewer"), ("⌘/", "shortcuts")]),
                _ => ("TERMINAL", &[("⌘T", "new tab"), ("⌘.", "viewer"), ("⌘⇧P", "commands"), ("⌘/", "shortcuts")]),
            };
            widgets::pill(ui, focus, theme::ACCENT);
            if let Some(t) = toast {
                ui.add_space(theme::SPACE_XS);
                ui.label(RichText::new(t.text).font(theme::ui_font(theme::TEXT_SMALL)).color(if t.error { theme::RED } else { theme::MUTED }));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(theme::SPACE_M);
                widgets::key_hints(ui, hints);
            });
        });
        if self.engine.state().toasts.last().map(|t| t.at.elapsed() < Duration::from_secs(12)).unwrap_or(false) {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    }

    /// ⌘/: every shortcut, grouped, over whatever is shown.
    fn shortcuts_ui(&mut self, ctx: &egui::Context) {
        if !self.shortcuts {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            self.shortcuts = false;
            return;
        }
        let area = egui::Area::new(egui::Id::new("shortcuts")).order(egui::Order::Foreground).anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -20.0)).show(ctx, |ui| {
            widgets::overlay_frame().inner_margin(egui::Margin::same(theme::SPACE_XL as i8)).show(ui, |ui| {
                ui.set_width(720.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Keyboard shortcuts").font(theme::ui_font_semibold(theme::TEXT_TITLE)).color(theme::TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        widgets::key_hints(ui, &[("esc", "close")]);
                    });
                });
                ui.add_space(theme::SPACE_L);
                ui.columns(2, |cols| {
                    for (col, groups) in cols.iter_mut().zip(SHORTCUTS) {
                        for (title, rows) in groups {
                            col.label(widgets::section_label(title, theme::SUBTLE));
                            col.add_space(theme::SPACE_XS);
                            egui::Grid::new(("shortcuts", title)).num_columns(2).spacing([theme::SPACE_M, 5.0]).min_col_width(96.0).show(col, |ui| {
                                for (keys, what) in rows.iter() {
                                    ui.label(RichText::new(*keys).font(theme::ui_font_semibold(theme::TEXT_BODY)).color(theme::TEXT));
                                    ui.label(RichText::new(*what).font(theme::ui_font(theme::TEXT_BODY)).color(theme::MUTED));
                                    ui.end_row();
                                }
                            });
                            col.add_space(theme::SPACE_L);
                        }
                    }
                });
            });
        });
        if area.response.clicked_elsewhere() {
            self.shortcuts = false;
        }
    }

    fn terminal_area(&mut self, ui: &mut Ui, ws: &Workspace, tab: &Tab) {
        self.ensure_terminal(ui.ctx(), ws, tab);
        let font_size = self.engine.settings().font_size;
        let focused = self.focus() == Focus::Terminal && self.dialog.is_none() && self.palette.is_none();
        match self.slots.get_mut(&tab.row.id) {
            Some(TermSlot::Live(session)) => {
                if session.take_bell() {
                    // A bell in a background tab marks it unread.
                }
                let reg = self.engine.plugins().map(|p| p.registry());
                let rules = reg.as_ref().map(|r| &r.links[..]).unwrap_or(&[]);
                // Breathing room between the text and the window chrome.
                let margin = egui::Margin { left: 12, right: 8, top: 8, bottom: 4 };
                let (resp, link) = egui::Frame::NONE.inner_margin(margin).show(ui, |ui| TerminalView::new(session).font_size(font_size).focused(focused).links(rules).show(ui)).inner;
                if resp.clicked_by(egui::PointerButton::Primary) || resp.drag_started() {
                    self.set_focus(Focus::Terminal);
                }
                if let Some(link) = link {
                    self.engine.open_link(&tab.row.id, link);
                }
            }
            Some(TermSlot::Preparing) | None => centered_note(ui, &format!("attaching to {} on {}…", tab.row.tmux_session, ws.row.host), theme::SUBTLE),
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
        let focused = self.focus() == Focus::Side && self.dialog.is_none() && self.palette.is_none();
        let kind = self.side().unwrap_or(SideKind::Viewer);
        let mut cx = PanelCtx { engine: &self.engine, focused, clicked: false };
        match (kind, tab) {
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
            (_, None) => centered_note(ui, "open a tab first (⌘T)", theme::SUBTLE),
        }
        if cx.clicked {
            self.set_focus(Focus::Side);
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
        self.sides.retain(|id, _| live.contains(id));
        self.side_shown.retain(|id| live.contains(id));
    }
}

impl eframe::App for KitchenApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if !self.injected.is_empty() {
            raw_input.events.append(&mut self.injected);
        }
        keymap::unpaste_shift_v(raw_input, ctx.input(|i| i.modifiers));
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
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
                    self.set_focus(Focus::Terminal);
                }
            }
        } else {
            let (reg, chords) = self.plugin_chords();
            for action in keymap::collect(&ctx, &chords) {
                // Chords belong to what is on screen; out-of-scope ones are swallowed.
                let side_focused = self.focus() == Focus::Side && self.side().is_some();
                if !action.allowed(side_focused, self.viewer_focused()) {
                    continue;
                }
                match (action, &reg) {
                    (Action::Plugin(i), Some(reg)) => {
                        if let Some(c) = reg.chords.get(i) {
                            let command = c.command.clone();
                            self.run_plugin_command(&command);
                        }
                    }
                    _ => self.run_action(action),
                }
            }
            // Esc in a side panel that did not use it hands focus back.
            if self.focus() == Focus::Side && ctx.input(|i| i.modifiers.command && i.key_pressed(Key::Escape)) {
                self.set_focus(Focus::Terminal);
            }
        }
        let ws = self.engine.selected_workspace();
        let tab = ws.as_ref().and_then(|w| w.selected().cloned());
        self.sync_dashboard(ws.as_ref().map(|w| w.row.id.as_str()));

        egui::Panel::top("titlebar")
            .exact_size(TITLEBAR_HEIGHT)
            .frame(egui::Frame::NONE.fill(theme::SIDEBAR))
            .show(ui, |ui| self.titlebar(ui, ws.as_ref(), tab.as_ref()));
        egui::Panel::left("workspaces")
            .exact_size(236.0)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(theme::SIDEBAR))
            .show(ui, |ui| {
                egui::Panel::bottom("sidebar-footer").frame(egui::Frame::NONE.fill(theme::SIDEBAR).inner_margin(egui::Margin::symmetric(0, 6))).show(ui, |ui| self.footer(ui));
                self.sidebar(ui);
            });
        egui::Panel::bottom("status")
            .exact_size(26.0)
            .frame(egui::Frame::NONE.fill(theme::SIDEBAR))
            .show(ui, |ui| self.status_bar(ui));
        if let Some(ws) = &ws {
            egui::Panel::top("tabs")
                .exact_size(38.0)
                .frame(egui::Frame::NONE.fill(theme::SIDEBAR).inner_margin(egui::Margin { left: 0, right: 0, top: 6, bottom: 0 }))
                .show(ui, |ui| self.tab_strip(ui, ws));
        }
        let creating = ws.as_ref().and_then(|w| w.pending.iter().rev().find(|p| p.focus).cloned());
        let dashboard = ws.as_ref().is_some_and(|w| self.dashboard.contains(&w.row.id));
        let side_shown = self.focus() == Focus::Side && creating.is_none() && !dashboard;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(theme::BG)).show(ui, |ui| match (&ws, &tab) {
            (Some(ws), _) if creating.is_some() => {
                let p = creating.as_ref().unwrap();
                let body = format!("on {} · fetching origin, preparing the worktree, starting tmux", ws.row.host);
                widgets::empty_state(ui, &format!("Creating “{}”", p.title), Some(&body), &[]);
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
            (Some(ws), _) if dashboard => crate::dashboard_view::ui(ui, &self.engine, ws),
            (Some(ws), tab) if side_shown => self.side_area(ui, ws, tab.as_ref()),
            (Some(ws), Some(tab)) => self.terminal_area(ui, ws, tab),
            (Some(ws), None) => {
                let body = ws.status.clone().unwrap_or_else(|| format!("{} · {}", ws.row.host, ws.row.repo_path));
                widgets::empty_state(ui, &ws.row.name, Some(&body), &[("⌘T", "New tab, in its own worktree and branch"), ("⌘⇧P", "Review a PR from its link")]);
            }
            (None, _) => widgets::empty_state(ui, "the kitchen", Some("A workspace is a repo on this mac or on a devbox."), &[("⌘N", "New workspace")]),
        });
        self.palette_ui(&ctx);
        self.shortcuts_ui(&ctx);
        self.save_screenshots(&ctx);
        if profiling() {
            let ms = frame_start.elapsed().as_secs_f64() * 1000.0;
            let events = ctx.input(|i| i.events.len());
            log::info!("frame {ms:.2}ms events={events}");
        }
        if self.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }
}

fn centered_note(ui: &mut Ui, text: &str, color: Color32) {
    ui.centered_and_justified(|ui| {
        ui.label(RichText::new(text).color(color).font(theme::ui_font(theme::TEXT_BODY)));
    });
}

/// A tab in the strip, as tall as the strip: the selected one takes the
/// content's color, so it reads as the top of what is shown below.
fn tab_button(ui: &mut Ui, strip: egui::Rect, number: Option<usize>, title: &str, selected: bool, unread: bool, pending: bool) -> egui::Response {
    let mut job = egui::text::LayoutJob::default();
    let fmt = |font: egui::FontId, color: Color32| egui::TextFormat { font_id: font, color, valign: Align::Center, italics: pending, ..Default::default() };
    if let Some(n) = number {
        job.append(&n.to_string(), 0.0, fmt(theme::ui_font(theme::TEXT_SMALL), theme::DIM));
    }
    let title_color = if selected { theme::TEXT } else { theme::SUBTLE };
    job.append(title, if number.is_some() { 7.0 } else { 0.0 }, fmt(theme::ui_font(theme::TEXT_BODY), title_color));
    let galley = ui.painter().layout_job(job);
    let dot = if unread { 12.0 } else { 0.0 };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(galley.size().x + 28.0 + dot, strip.height()), Sense::click());
    let top = egui::CornerRadius { nw: theme::RADIUS, ne: theme::RADIUS, sw: 0, se: 0 };
    if selected {
        ui.painter().rect_filled(rect, top, theme::BG);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect.shrink2(egui::vec2(0.0, 0.0)), top, theme::HOVER);
    }
    let text_color = if resp.hovered() && !selected { theme::MUTED } else { title_color };
    let pos = egui::pos2(rect.left() + 14.0, rect.center().y - galley.size().y / 2.0);
    let width = galley.size().x;
    if resp.hovered() && !selected {
        ui.painter().galley_with_override_text_color(pos, galley, text_color);
    } else {
        ui.painter().galley(pos, galley, text_color);
    }
    if unread {
        ui.painter().circle_filled(egui::pos2(pos.x + width + 9.0, rect.center().y), 3.0, theme::YELLOW);
    }
    resp
}

/// A small square button holding one glyph.
fn icon_button(ui: &mut Ui, strip: egui::Rect, glyph: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(26.0, strip.height()), Sense::click());
    let square = egui::Rect::from_center_size(rect.center(), egui::vec2(24.0, 24.0));
    if resp.hovered() {
        ui.painter().rect_filled(square, theme::RADIUS as f32, theme::HOVER);
    }
    let color = if resp.hovered() { theme::TEXT } else { theme::SUBTLE };
    ui.painter().text(square.center(), egui::Align2::CENTER_CENTER, glyph, theme::ui_font(16.0), color);
    resp
}

type ShortcutGroup = (&'static str, &'static [(&'static str, &'static str)]);

/// The ⌘/ list, in two columns.
const SHORTCUTS: [&[ShortcutGroup]; 2] = [
    &[
        (
            "Workspaces and tabs",
            &[
                ("⌘N", "new workspace"),
                ("⌘J  ⌘K", "next / previous workspace"),
                ("⌘⇧R", "rename workspace"),
                ("⌘T", "new tab"),
                ("⌘1 … ⌘9", "go to tab"),
                ("⌘H  ⌘L", "previous / next tab"),
                ("⌘R", "rename tab"),
                ("⌘W", "close tab"),
            ],
        ),
        (
            "Everywhere",
            &[
                ("⌘.", "terminal ↔ viewer"),
                ("⌘⇧I", "plan"),
                ("⌘D", "dashboard (diff in a panel)"),
                ("⌘⇧P", "commands, or a PR link"),
                ("⌘/", "this list"),
                ("⌘Q", "quit"),
            ],
        ),
    ],
    &[
        (
            "Viewer",
            &[
                ("⌘P", "find a file"),
                ("⌘B", "show / hide the file list"),
                ("⌘⇧H  ⌘⇧L", "file list ↔ code"),
                ("⌘⇧J  ⌘⇧K", "next / previous changed file"),
                ("⌘O", "outline"),
                ("]c  [c", "next / previous change"),
                ("gd  gr", "definition / references"),
                ("⌃O", "back"),
                ("-", "parent folder"),
            ],
        ),
        ("Comments", &[("⌘M", "comment on the line or selection"), ("⌘⏎", "submit (plan: send all to the agent)")]),
    ],
];

/// `KITCHEN_PROFILE=1` logs the build time of every frame.
fn profiling() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("KITCHEN_PROFILE").is_some())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{self, TestRepo};

    fn key(key: Key, modifiers: Modifiers) -> Vec<egui::Event> {
        vec![
            egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers },
            egui::Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers },
        ]
    }

    /// Enter in the viewer's file list (into a folder, or `../`) keeps the
    /// keyboard in the viewer; it must never land in the terminal.
    #[test]
    fn enter_in_file_list_keeps_viewer_focus() {
        let repo = TestRepo::new("mkdir -p .agents/skills aster/x && echo 1 > .agents/skills/a.md && echo 2 > aster/x/b.py && echo 3 > README.md");
        let (ctx, engine) = test_util::engine("local", &repo.path(), &["t"]);
        let mut app = KitchenApp::new(engine.clone());
        let term = TermSession::spawn(&ctx, &["sh".into(), "-c".into(), "cat >/dev/null".into()], None, HashMap::new()).unwrap();
        app.slots.insert("t".into(), TermSlot::Live(term));
        let mut frame = eframe::Frame::_new_kittest();
        let mut step = |app: &mut KitchenApp, events: Vec<egui::Event>| {
            let raw = egui::RawInput { events, screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))), ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| eframe::App::ui(app, ui, &mut frame));
            out.textures_delta.clear();
        };
        for _ in 0..3 {
            step(&mut app, vec![]);
        }
        assert_eq!(app.focus(), Focus::Terminal);
        // ⌘.: the viewer; wait for the file list; ⌘⇧H: into the list.
        step(&mut app, key(Key::Period, Modifiers::COMMAND));
        assert_eq!(app.focus(), Focus::Side);
        for _ in 0..200 {
            step(&mut app, vec![]);
            std::thread::sleep(Duration::from_millis(10));
        }
        step(&mut app, key(Key::H, Modifiers::COMMAND | Modifiers::SHIFT));
        step(&mut app, vec![]);
        // Root rows: .agents/ aster/ README.md — gg, then Enter into .agents/.
        for c in ["-", "-", "g", "g"] {
            step(&mut app, vec![egui::Event::Text(c.into())]);
        }
        for round in 0..3 {
            step(&mut app, key(Key::Enter, Modifiers::NONE));
            step(&mut app, vec![]);
            assert_eq!(app.focus(), Focus::Side, "Enter #{round} moved focus to the terminal");
            step(&mut app, vec![egui::Event::Text("g".into())]);
            step(&mut app, vec![egui::Event::Text("g".into())]);
        }
    }

    /// The side panel belongs to the tab it was opened on: other tabs keep
    /// theirs closed, and switching to one hands the keyboard back.
    #[test]
    fn side_panel_is_per_tab() {
        let repo = TestRepo::new("echo 1 > a");
        let (ctx, engine) = test_util::engine("local", &repo.path(), &["t1", "t2"]);
        let mut app = KitchenApp::new(engine.clone());
        for id in ["t1", "t2"] {
            let term = TermSession::spawn(&ctx, &["sh".into(), "-c".into(), "cat >/dev/null".into()], None, HashMap::new()).unwrap();
            app.slots.insert(id.into(), TermSlot::Live(term));
        }
        let mut frame = eframe::Frame::_new_kittest();
        let mut step = |app: &mut KitchenApp, events: Vec<egui::Event>| {
            let raw = egui::RawInput { events, screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))), ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| eframe::App::ui(app, ui, &mut frame));
            out.textures_delta.clear();
        };
        engine.select_tab_index(1);
        step(&mut app, vec![]);
        // ⌘. swaps t1's terminal for the viewer.
        step(&mut app, key(Key::Period, Modifiers::COMMAND));
        assert_eq!(app.sides.get("t1"), Some(&SideKind::Viewer));
        assert_eq!(app.focus(), Focus::Side);
        // t2 has no viewer: it shows its terminal.
        engine.select_tab_index(2);
        step(&mut app, vec![]);
        assert_eq!(app.side(), None);
        assert!(!app.sides.contains_key("t2"));
        assert_eq!(app.focus(), Focus::Terminal);
        // Back on t1, its viewer is still on screen.
        engine.select_tab_index(1);
        step(&mut app, vec![]);
        assert_eq!((app.focus(), app.side()), (Focus::Side, Some(SideKind::Viewer)));
        // Tab keys work from the viewer too: ⌘L to t2 (its terminal), ⌘1 back (t1's viewer).
        step(&mut app, key(Key::L, Modifiers::COMMAND));
        assert_eq!(app.selected_tab_id().as_deref(), Some("t2"));
        assert_eq!(app.focus(), Focus::Terminal);
        step(&mut app, key(Key::Num1, Modifiers::COMMAND));
        assert_eq!(app.selected_tab_id().as_deref(), Some("t1"));
        assert_eq!(app.focus(), Focus::Side);
        // ⌘. on t1 brings its terminal back, and t1 remembers that too.
        step(&mut app, key(Key::Period, Modifiers::COMMAND));
        step(&mut app, key(Key::L, Modifiers::COMMAND));
        step(&mut app, key(Key::Num1, Modifiers::COMMAND));
        assert_eq!((app.focus(), app.side()), (Focus::Terminal, Some(SideKind::Viewer)));
        // ⌘. back and forth: terminal, viewer, terminal; the viewer stays open.
        step(&mut app, key(Key::Period, Modifiers::COMMAND));
        assert_eq!((app.focus(), app.side()), (Focus::Side, Some(SideKind::Viewer)));
        step(&mut app, key(Key::Period, Modifiers::COMMAND));
        assert_eq!((app.focus(), app.side()), (Focus::Terminal, Some(SideKind::Viewer)));
        // From the plan, ⌘. goes to the terminal, and then to the viewer.
        step(&mut app, key(Key::I, Modifiers::COMMAND | Modifiers::SHIFT));
        assert_eq!((app.focus(), app.side()), (Focus::Side, Some(SideKind::Plan)));
        step(&mut app, key(Key::Period, Modifiers::COMMAND));
        assert_eq!(app.focus(), Focus::Terminal);
        step(&mut app, key(Key::Period, Modifiers::COMMAND));
        assert_eq!((app.focus(), app.side()), (Focus::Side, Some(SideKind::Viewer)));
    }
}
