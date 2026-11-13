//! The window: workspace sidebar, tab strip, terminal, side panel
//! (viewer / plan), dialogs, and dispatching the keymap's chords.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use egui::{Align, Color32, Layout, Modifiers, RichText, Sense, Ui};

use crate::attach;
use crate::engine::{Engine, Shown, Tab, UiCommand, View, Workspace};
use crate::viewer::Viewer;
use crate::keybindings::app::Action;
use crate::keybindings::{self, Cmd, FieldCmd, KeyFocus, MenuCmd, Place, places};
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
    /// Every viewer a tab has shown, by (tab, (worktree, base)), kept until
    /// the tab closes: showing one again (⌘⇧S) brings back its cursor,
    /// folds and loaded files.
    viewers: HashMap<(String, Shown), Viewer>,
    /// What each tab's viewer shows now (absent: its worktree vs its diff base).
    shown: HashMap<String, Shown>,
    /// The latest switch per tab (`Engine::next_view_seq`): an older
    /// `ShowViewer` is dropped, so a slow checkout can't take the screen back.
    view_seqs: HashMap<String, u64>,
    /// A PR menu pick being checked out, by tab: (its name, the switch's
    /// number). Its viewer shows the frying pan until the view arrives.
    switching: HashMap<String, (String, u64)>,
    plans: HashMap<String, crate::plan_view::PlanView>,
    dialog: Option<Dialog>,
    screenshots: Vec<(String, Sender<Result<(), String>>)>,
    /// Input events from `kitchenctl debug keys`, fed to the next frame.
    injected: Vec<egui::Event>,
    /// The paste key was pressed this frame (see attach.rs).
    paste: bool,
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
            shown: HashMap::new(),
            view_seqs: HashMap::new(),
            switching: HashMap::new(),
            plans: HashMap::new(),
            dialog: None,
            shortcuts: false,
            screenshots: Vec::new(),
            injected: Vec::new(),
            paste: false,
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
                UiCommand::ShowViewer { tab_id, view, seq } => {
                    if self.view_seqs.get(&tab_id).is_some_and(|&last| seq < last) {
                        continue; // a later switch won
                    }
                    self.view_seqs.insert(tab_id.clone(), seq);
                    if self.switching.get(&tab_id).is_some_and(|(_, s)| seq >= *s) {
                        self.switching.remove(&tab_id);
                    }
                    self.show_viewer(&tab_id, view);
                }
                UiCommand::ViewFailed { tab_id, seq } => {
                    if self.switching.get(&tab_id).is_some_and(|(_, s)| *s == seq) {
                        self.switching.remove(&tab_id);
                    }
                }
                UiCommand::ReloadViewer { tab_id, worktree } => self.reload_viewers(&tab_id, &worktree),
                UiCommand::Render { tab_id, on } => {
                    if let Some(v) = self.viewer_key(&tab_id).and_then(|k| self.viewers.get_mut(&k)) {
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
            SideKind::Viewer => tab_viewer(&mut self.viewers, &self.shown, &self.engine, &ws, &tab).command(cmd, &self.engine),
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
            // From the terminal too: the viewer opens with the menu up.
            Action::ViewMenu => {
                if !self.viewer_focused() {
                    self.set_side(SideKind::Viewer);
                    self.set_focus(Focus::Side);
                }
                self.side_command(PanelCmd::ViewMenu);
            }
            Action::Shortcuts => self.shortcuts = !self.shortcuts,
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
                    self.dialog = Some(Dialog::new_tab(false));
                } else {
                    engine.toast_err(with_key("create a workspace first", Place::Global, Action::NewWorkspace));
                }
            }
            Action::NewMainCheckoutTab => {
                if ws.is_some() {
                    self.dialog = Some(Dialog::new_tab(true));
                } else {
                    engine.toast_err(with_key("create a workspace first", Place::Global, Action::NewWorkspace));
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
            Action::ReopenTab => {
                if let Some(ws) = &ws {
                    let id = ws.row.id.clone();
                    engine.spawn("reopen tab", move |e| e.reopen_closed_tab(&id));
                    self.hide_dashboard();
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
            Action::Ask => self.side_command(PanelCmd::Ask),
            Action::Submit => self.side_command(PanelCmd::Submit),
            Action::ToggleDiff => self.side_command(PanelCmd::ToggleDiff),
            Action::Dashboard => {
                if let Some(ws) = &ws {
                    self.toggle_dashboard(&ws.row.id);
                }
            }
            Action::FocusTerminal => self.set_focus(Focus::Terminal),
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

    /// What has keyboard focus, and the open file's filetype for plugin chords.
    fn key_focus(&self) -> (KeyFocus, Option<String>) {
        if self.dialog.is_some() {
            return (KeyFocus::Dialog, None);
        }
        if self.palette.is_some() {
            return (KeyFocus::Palette, None);
        }
        let tab = self.selected_tab_id();
        match (self.focus(), self.side(), tab) {
            (Focus::Side, Some(SideKind::Viewer), Some(t)) => match self.viewer_key(&t).and_then(|k| self.viewers.get(&k)) {
                Some(v) => (v.key_focus(), v.current_filetype()),
                None => (KeyFocus::Viewer, None),
            },
            (Focus::Side, Some(SideKind::Plan), Some(t)) => (self.plans.get(&t).map_or(KeyFocus::Plan, |p| p.key_focus()), None),
            _ => (KeyFocus::Terminal, None),
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

    /// The cache key of the viewer `tab_id` shows: `shown[tab]`, else its
    /// worktree vs its diff base. Creates nothing.
    fn viewer_key(&self, tab_id: &str) -> Option<(String, Shown)> {
        let shown = match self.shown.get(tab_id) {
            Some(s) => s.clone(),
            None => {
                let (ws, tab) = self.engine.find_tab(tab_id)?;
                (tab.row.worktree.clone(), tab.diff_base(&ws))
            }
        };
        Some((tab_id.to_string(), shown))
    }

    /// Point `tab_id`'s viewer at `view` (a cached viewer of it, or a new
    /// one) and open the tab's viewer side.
    fn show_viewer(&mut self, tab_id: &str, view: Shown) {
        let Some((ws, _)) = self.engine.find_tab(tab_id) else { return };
        let engine = &self.engine;
        self.viewers
            .entry((tab_id.to_string(), view.clone()))
            .or_insert_with(|| Viewer::at(engine, ws.host(), tab_id, view.0.clone(), view.1.clone()));
        self.shown.insert(tab_id.to_string(), view);
        self.open_side_for(tab_id, SideKind::Viewer);
    }

    /// A row picked in the PR menu. A cached view shows now and its checkout
    /// is refreshed behind it; any other is checked out first, as
    /// `kitchen.view` does. Either way this switch's number is taken now, so
    /// an older switch still checking out can't take the screen back.
    fn choose_view(&mut self, v: View) {
        let Some(shown) = self.engine.shown_by(&v) else { return };
        let seq = self.engine.next_view_seq();
        self.view_seqs.insert(v.tab.clone(), seq);
        if self.viewers.contains_key(&(v.tab.clone(), shown.clone())) {
            self.switching.remove(&v.tab);
            self.show_viewer(&v.tab.clone(), shown);
            self.engine.spawn("refresh view", move |e| e.refresh_view(v));
        } else {
            // The PR's name for the frying pan's caption, from its menu row.
            let name = self.engine.views_of(&v.tab).and_then(|m| m.iter().find(|(_, s)| *s == shown).map(|(e, _)| e.label.split_whitespace().next().unwrap_or(&e.label).to_string()));
            self.switching.insert(v.tab.clone(), (name.unwrap_or_else(|| "the view".into()), seq));
            let tab = v.tab.clone();
            self.engine.spawn("view", move |e| {
                e.view(v, Some(seq)).inspect_err(|_| e.push_ui(UiCommand::ViewFailed { tab_id: tab, seq }))
            });
        }
    }

    /// `worktree` moved: rebuild `tab_id`'s viewers of it (shown or cached),
    /// each with its base, reopened at its file and line.
    fn reload_viewers(&mut self, tab_id: &str, worktree: &str) {
        let Some((ws, _)) = self.engine.find_tab(tab_id) else { return };
        let keys: Vec<(String, Shown)> = self.viewers.keys().filter(|(t, (w, _))| t == tab_id && w == worktree).cloned().collect();
        for key in keys {
            let Some(old) = self.viewers.remove(&key) else { continue };
            let (path, line, diff) = old.location();
            let mut v = Viewer::at(&self.engine, ws.host(), tab_id, key.1.0.clone(), key.1.1.clone());
            if path.is_some() {
                v.command(PanelCmd::Open { path, line, diff: Some(diff) }, &self.engine);
            }
            self.viewers.insert(key, v);
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

    fn run_plugin_command(&mut self, command: &str) {
        let Some(p) = self.engine.plugins().cloned() else { return };
        let tab = self.selected_tab_id().unwrap_or_default();
        let viewer = self.viewer_key(&tab).and_then(|k| self.viewers.get(&k)).map(|v| v.context());
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
        let km = keybindings::current();
        let keys = km.plugin_keys();
        if let Some(p) = self.engine.plugins() {
            let reg = p.registry();
            for c in &reg.commands {
                let chords = keys.get(&c.name).map(|k| k.join(" ")).unwrap_or_default();
                let detail = [c.desc.clone(), chords].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  ·  ");
                items.push((PaletteItem::Command(c.name.clone()), (c.name.clone(), detail)));
            }
        }
        let diag = [crate::viewer::vim::VimCmd::NextDiag, crate::viewer::vim::VimCmd::PrevDiag].map(|c| km.hint(Place::Vim, &Cmd::Vim(c), 1)).into_iter().filter(|k| !k.is_empty()).collect::<Vec<_>>().join(" / ");
        items.push((PaletteItem::Diagnostics, ("viewer: diagnostics in this file".into(), diag)));
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
        let km = keybindings::current();
        let choose = ctx.input(|i| keybindings::presses(i).iter().any(|p| km.lookup(KeyFocus::Palette, None, p) == Some(&Cmd::Menu(MenuCmd::Choose))));
        if let Some(link) = link.filter(|_| choose) {
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
                    if ws.tabs.iter().any(|t| t.unread) && ui.button("Mark as read").clicked_by(egui::PointerButton::Primary) {
                        if let Err(e) = self.engine.mark_read(Some(&ws.row.id)) {
                            self.engine.toast_err(format!("{e:#}"));
                        }
                        ui.close();
                    }
                    let others_unread = workspaces.iter().any(|w| w.row.id != ws.row.id && w.tabs.iter().any(|t| t.unread));
                    if others_unread && ui.button("Mark all workspaces as read").clicked_by(egui::PointerButton::Primary) {
                        if let Err(e) = self.engine.mark_read(None) {
                            self.engine.toast_err(format!("{e:#}"));
                        }
                        ui.close();
                    }
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
    /// (workspace › tab, and its branch). Dragging it moves the window, and
    /// double-clicking it zooms the window to fill the screen (and back),
    /// like a native title bar.
    fn titlebar(&mut self, ui: &mut Ui, ws: Option<&Workspace>, tab: Option<&Tab>) {
        let rect = ui.max_rect();
        let resp = ui.interact(rect, ui.id().with("titlebar"), Sense::click_and_drag());
        if resp.drag_started_by(egui::PointerButton::Primary) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if resp.double_clicked_by(egui::PointerButton::Primary) {
            let zoomed = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!zoomed));
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
        p.text(egui::pos2(card.right() - 10.0, card.center().y), egui::Align2::RIGHT_CENTER, keybindings::hint(Place::Global, Action::NewWorkspace), theme::ui_font(theme::TEXT_SMALL), theme::DIM);
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
                let kind = if tab.in_main_checkout(ws) { theme::TAB_READ } else { theme::TAB_EDIT };
                let resp = tab_button(ui, strip, Some(i + 1), &title, Some(kind), is_sel, tab.unread, false).on_hover_text(format!(
                    "{}\n{}",
                    tab.row.worktree,
                    tab.row.branch.as_deref().map(|b| format!("branch {b}")).unwrap_or_else(|| "detached".into())
                ));
                if resp.clicked_by(egui::PointerButton::Primary) {
                    self.engine.select_tab(&tab.row.id);
                    self.dashboard.remove(&ws.row.id);
                }
                if keybindings::current().clicked(Place::Global, places::ClickCmd::CloseTab, &resp, ui.input(|i| i.modifiers)) {
                    self.dialog = Some(Dialog::confirm_close(tab));
                }
            }
            for p in &ws.pending {
                let title: String = p.title.chars().take(32).collect();
                tab_button(ui, strip, None, &format!("⋯ {title}"), None, p.focus, false, true).on_hover_text("fetching origin and preparing the worktree…");
            }
            ui.add_space(theme::SPACE_XS);
            let tip = [("New edit tab", Action::NewTab), ("New read tab", Action::NewMainCheckoutTab)].map(|(what, a)| with_key_spaced(what, Place::Terminal, a)).join("\n");
            if icon_button(ui, strip, "+").on_hover_text(tip).clicked_by(egui::PointerButton::Primary) {
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
                    (SideKind::Plan, "Plan", Action::OpenPlan, Action::OpenPlan),
                    (SideKind::Viewer, "Viewer", Action::ToggleViewer, Action::ToggleFiles),
                ] {
                    let on = self.focus() == Focus::Side && self.side() == Some(kind) && !self.dashboard.contains(&ws.row.id);
                    if widgets::flat_button(ui, label, on).on_hover_text(with_key_spaced(label, Place::Global, keys)).clicked_by(egui::PointerButton::Primary) {
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
            use Action as A;
            use Place::{Global as G, Panel as P, Terminal as T, Viewer as V};
            let (focus, hints): (&str, &[(Place, Action, &str)]) = match (self.focus(), self.side()) {
                (Focus::Side, Some(SideKind::Viewer)) => ("VIEWER", &[(V, A::FilePicker, "find file"), (P, A::Comment, "comment"), (P, A::ToggleDiff, "diff"), (G, A::ToggleViewer, "terminal"), (G, A::Shortcuts, "shortcuts")]),
                (Focus::Side, Some(SideKind::Plan)) => ("PLAN", &[(P, A::Comment, "comment"), (P, A::Submit, "submit"), (G, A::ToggleViewer, "viewer"), (G, A::Shortcuts, "shortcuts")]),
                _ => ("TERMINAL", &[(T, A::NewTab, "new edit tab"), (T, A::NewMainCheckoutTab, "new read tab"), (G, A::ToggleViewer, "viewer"), (G, A::Palette, "commands"), (G, A::Shortcuts, "shortcuts")]),
            };
            let km = keybindings::current();
            let hints: Vec<(String, &str)> = hints.iter().map(|(p, a, what)| (km.hint(*p, &Cmd::App(*a), 1), *what)).collect();
            widgets::pill(ui, focus, theme::ACCENT);
            if let Some(t) = toast {
                ui.add_space(theme::SPACE_XS);
                ui.label(RichText::new(t.text).font(theme::ui_font(theme::TEXT_SMALL)).color(if t.error { theme::RED } else { theme::MUTED }));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(theme::SPACE_M);
                widgets::key_hints(ui, &hints);
            });
        });
        if self.engine.state().toasts.last().map(|t| t.at.elapsed() < Duration::from_secs(12)).unwrap_or(false) {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    }

    /// ⌘/: every binding of the live keymap, grouped by place, over whatever is shown.
    fn shortcuts_ui(&mut self, ctx: &egui::Context) {
        if !self.shortcuts {
            return;
        }
        let km = keybindings::current();
        let close = |p: &keybindings::Press| km.lookup(KeyFocus::Dialog, None, p) == Some(&Cmd::Field(FieldCmd::Cancel));
        if ctx.input_mut(|i| keybindings::presses(i).into_iter().find(close).is_some_and(|p| keybindings::take(i, &p, true))) {
            self.shortcuts = false;
            return;
        }
        let commands = self.engine.plugins().map(|p| p.registry()).unwrap_or_default();
        let groups = shortcut_groups(&km, &commands);
        let half = groups.iter().map(|(_, rows)| rows.len() + 2).sum::<usize>() / 2;
        let area = egui::Area::new(egui::Id::new("shortcuts")).order(egui::Order::Foreground).anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -20.0)).show(ctx, |ui| {
            widgets::overlay_frame().inner_margin(egui::Margin::same(theme::SPACE_XL as i8)).show(ui, |ui| {
                ui.set_width(760.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Keyboard shortcuts").font(theme::ui_font_semibold(theme::TEXT_TITLE)).color(theme::TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        widgets::key_hints(ui, &[(km.hint(Place::Dialog, &Cmd::Field(FieldCmd::Cancel), 1), "close")]);
                    });
                });
                ui.label(RichText::new(format!("change them in {}", keybindings::path().display())).font(theme::ui_font(theme::TEXT_SMALL)).color(theme::DIM));
                ui.add_space(theme::SPACE_L);
                let max_h = (ctx.content_rect().height() - 180.0).max(200.0);
                egui::ScrollArea::vertical().max_height(max_h).show(ui, |ui| {
                    ui.columns(2, |cols| {
                        let mut used = 0;
                        for (title, rows) in &groups {
                            let col = if used < half { &mut cols[0] } else { &mut cols[1] };
                            used += rows.len() + 2;
                            col.label(widgets::section_label(title, theme::SUBTLE));
                            col.add_space(theme::SPACE_XS);
                            egui::Grid::new(("shortcuts", title)).num_columns(2).spacing([theme::SPACE_M, 5.0]).min_col_width(96.0).show(col, |ui| {
                                for (keys, what) in rows {
                                    ui.label(RichText::new(keys).font(theme::ui_font_semibold(theme::TEXT_BODY)).color(theme::TEXT));
                                    ui.label(RichText::new(what).font(theme::ui_font(theme::TEXT_BODY)).color(theme::MUTED));
                                    ui.end_row();
                                }
                            });
                            col.add_space(theme::SPACE_L);
                        }
                    });
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
                // ⌘V with an image, and dropped files, paste paths Claude Code can read.
                let mut sources = Vec::new();
                if focused && self.paste {
                    sources.push(attach::Source::Clipboard);
                }
                let dropped: Vec<_> = ui.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
                let drop = !dropped.is_empty() && self.dialog.is_none() && self.palette.is_none();
                if drop {
                    sources.extend(dropped.into_iter().map(attach::Source::File));
                }
                if !sources.is_empty() {
                    attach::deliver(&self.engine, ws.host(), sources, session.paster());
                }
                if drop || resp.clicked_by(egui::PointerButton::Primary) || resp.drag_started() {
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
                let msg = match keybindings::hint(Place::Terminal, Action::CloseTab) {
                    k if k.is_empty() => msg.clone(),
                    k => format!("{msg}\n{k} closes this tab"),
                };
                centered_note(ui, &msg, theme::RED)
            }
        }
    }

    fn side_area(&mut self, ui: &mut Ui, ws: &Workspace, tab: Option<&Tab>) {
        let focused = self.focus() == Focus::Side && self.dialog.is_none() && self.palette.is_none();
        let kind = self.side().unwrap_or(SideKind::Viewer);
        let mut cx = PanelCtx { engine: &self.engine, focused, clicked: false, view: None };
        match (kind, tab) {
            (SideKind::Viewer, Some(tab)) => tab_viewer(&mut self.viewers, &self.shown, &self.engine, ws, tab).ui(ui, &mut cx),
            (SideKind::Plan, Some(tab)) => self
                .plans
                .entry(tab.row.id.clone())
                .or_insert_with(|| crate::plan_view::PlanView::new(&self.engine, ws, tab))
                .ui(ui, &mut cx),
            (_, None) => {
                let keys: Vec<String> = [Action::NewTab, Action::NewMainCheckoutTab].map(|a| keybindings::hint(Place::Terminal, a)).into_iter().filter(|k| !k.is_empty()).collect();
                let note = if keys.is_empty() { "open a tab first".to_string() } else { format!("open a tab first ({})", keys.join(" or ")) };
                centered_note(ui, &note, theme::SUBTLE)
            }
        }
        // A PR being checked out: the frying pan over the viewer (the PR
        // menu, a foreground overlay, still opens above it).
        if let (SideKind::Viewer, Some((name, _))) = (kind, tab.and_then(|t| self.switching.get(&t.row.id))) {
            let rect = ui.max_rect();
            let layer = egui::LayerId::new(egui::Order::Middle, egui::Id::new("cooking"));
            let mut cover = ui.new_child(egui::UiBuilder::new().max_rect(rect).layer_id(layer));
            cover.painter().rect_filled(rect, 0.0, theme::BG);
            crate::ui::cooking::cooking(&mut cover, &format!("cooking {name}…"));
        }
        let (clicked, picked) = (cx.clicked, cx.view.take());
        if clicked {
            self.set_focus(Focus::Side);
        }
        if let Some(v) = picked {
            self.choose_view(v);
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
        self.viewers.retain(|(id, _), _| live.contains(id));
        self.shown.retain(|id, _| live.contains(id));
        self.view_seqs.retain(|id, _| live.contains(id));
        self.switching.retain(|id, _| live.contains(id));
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
        keybindings::app::edit_keys(raw_input, &keybindings::current(), ctx.input(|i| i.modifiers));
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
        let ctx = ui.ctx().clone();
        // Rendered Markdown asked for last frame is laid out off screen (and drawn once ready).
        crate::web::pump(frame, &ctx);
        self.poll_attach(&ctx);
        self.paste = attach::take_paste();
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
            // Chords belong to what has focus; ⌘ chords bound elsewhere are swallowed.
            let (focus, filetype) = self.key_focus();
            for cmd in keybindings::app::collect(&ctx, &keybindings::current(), focus, filetype.as_deref()) {
                match cmd {
                    Cmd::App(a) => self.run_action(a),
                    Cmd::Plugin(c) => self.run_plugin_command(&c),
                    _ => {}
                }
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
                let hints = [
                    (keybindings::hint(Place::Terminal, Action::NewTab), "New edit tab, in its own worktree and branch"),
                    (keybindings::hint(Place::Terminal, Action::NewMainCheckoutTab), "New read tab, in the main checkout"),
                    (keybindings::hint(Place::Global, Action::Palette), "Review a PR from its link"),
                ];
                widgets::empty_state(ui, &ws.row.name, Some(&body), &hints);
            }
            (None, _) => widgets::empty_state(ui, "the kitchen", Some("A workspace is a repo on this mac or on a devbox."), &[(keybindings::hint(Place::Global, Action::NewWorkspace), "New workspace")]),
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
/// `kind`: the tab's color (edit or read tab); None for a pending tab.
#[allow(clippy::too_many_arguments)]
fn tab_button(ui: &mut Ui, strip: egui::Rect, number: Option<usize>, title: &str, kind: Option<Color32>, selected: bool, unread: bool, pending: bool) -> egui::Response {
    let mut job = egui::text::LayoutJob::default();
    let fmt = |font: egui::FontId, color: Color32| egui::TextFormat { font_id: font, color, valign: Align::Center, italics: pending, ..Default::default() };
    if let Some(n) = number {
        job.append(&n.to_string(), 0.0, fmt(theme::ui_font(theme::TEXT_SMALL), theme::DIM));
    }
    let title_color = match (kind, selected) {
        (Some(c), true) => c,
        (Some(c), false) => c.gamma_multiply(0.8),
        (None, true) => theme::TEXT,
        (None, false) => theme::SUBTLE,
    };
    let title_font = if selected { theme::ui_font_semibold(theme::TEXT_BODY) } else { theme::ui_font(theme::TEXT_BODY) };
    job.append(title, if number.is_some() { 7.0 } else { 0.0 }, fmt(title_font, title_color));
    let galley = ui.painter().layout_job(job);
    let dot = if unread { 12.0 } else { 0.0 };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(galley.size().x + 28.0 + dot, strip.height()), Sense::click());
    let top = egui::CornerRadius { nw: theme::RADIUS, ne: theme::RADIUS, sw: 0, se: 0 };
    if selected {
        ui.painter().rect_filled(rect, top, theme::BG);
        // A bright outline on the top and sides; the bottom stays open
        // into the content below (clipped off).
        let outline = egui::Rect::from_min_max(rect.min + egui::vec2(0.75, 0.75), rect.max + egui::vec2(-0.75, 8.0));
        ui.painter().with_clip_rect(rect).rect_stroke(outline, top, egui::Stroke::new(1.5, kind.unwrap_or(theme::ACCENT)), egui::StrokeKind::Middle);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect.shrink2(egui::vec2(0.0, 0.0)), top, theme::HOVER);
    }
    let text_color = if resp.hovered() && !selected { kind.unwrap_or(theme::MUTED) } else { title_color };
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

/// "{what} ({key})", or just `what` when the action has no key.
fn with_key(what: &str, place: Place, a: Action) -> String {
    match keybindings::hint(place, a) {
        k if k.is_empty() => what.to_string(),
        k => format!("{what} ({k})"),
    }
}

/// "{what}  {key}" (a tooltip), or just `what`.
fn with_key_spaced(what: &str, place: Place, a: Action) -> String {
    match keybindings::hint(place, a) {
        k if k.is_empty() => what.to_string(),
        k => format!("{what}  {k}"),
    }
}

/// The ⌘/ list: per place, its bound actions (keys, what) in file order,
/// then the plugin commands with keys. ⌘1 … ⌘9 are one row.
fn shortcut_groups(km: &keybindings::Keymap, reg: &crate::plugins::Registry) -> Vec<(String, Vec<(String, String)>)> {
    let mut out = Vec::new();
    for place in Place::ALL {
        let mut rows: Vec<(String, String)> = Vec::new();
        for def in places::actions(place) {
            let keys = km.hint(place, &def.cmd, usize::MAX);
            if keys.is_empty() {
                continue;
            }
            match def.cmd {
                Cmd::App(Action::GoToTab(n)) if n > 1 => {}
                Cmd::App(Action::GoToTab(1)) => {
                    let last = km.hint(place, &Cmd::App(Action::GoToTab(9)), 1);
                    let first = km.hint(place, &def.cmd, 1);
                    rows.push((if last.is_empty() { first } else { format!("{first} … {last}") }, "go to tab".into()));
                }
                _ => rows.push((keys, def.desc.to_string())),
            }
        }
        if !rows.is_empty() {
            out.push((place.title().to_string(), rows));
        }
    }
    let plugin: Vec<(String, String)> = km
        .plugin_keys()
        .into_iter()
        .map(|(name, keys)| {
            let desc = reg.commands.iter().find(|c| c.name == name).map(|c| c.desc.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| name.clone());
            (keys.join("  "), desc)
        })
        .collect();
    if !plugin.is_empty() {
        out.push(("Plugins".into(), plugin));
    }
    out
}

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

/// The viewer `tab` shows, created on its worktree and diff base if none. A
/// free function over just the fields it needs, so a caller can keep
/// `&self.engine` borrowed for a `PanelCtx`.
fn tab_viewer<'a>(viewers: &'a mut HashMap<(String, Shown), Viewer>, shown: &HashMap<String, Shown>, engine: &Arc<Engine>, ws: &Workspace, tab: &Tab) -> &'a mut Viewer {
    let key = shown.get(&tab.row.id).cloned().unwrap_or_else(|| (tab.row.worktree.clone(), tab.diff_base(ws)));
    viewers.entry((tab.row.id.clone(), key.clone())).or_insert_with(|| Viewer::at(engine, ws.host(), &tab.row.id, key.0, key.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{self, TestRepo};
    use egui::Key;

    #[test]
    fn shortcuts_list_reads_the_keymap() {
        use crate::plugins::{ChordEntry, CommandEntry, Registry, When};
        let rows = |km: &keybindings::Keymap, reg: &Registry, group: &str| shortcut_groups(km, reg).into_iter().find(|(t, _)| t == group).map(|(_, r)| r).unwrap_or_default();
        let reg = Registry::default();
        let d = keybindings::Keymap::defaults();
        let everywhere = rows(&d, &reg, "Everywhere");
        assert!(everywhere.contains(&("⌘Q".into(), "quit".into())));
        assert!(everywhere.contains(&("⌘1 … ⌘9".into(), "go to tab".into())));
        assert!(rows(&d, &reg, "Vim motions").contains(&("j  ↓".into(), "down".into())));
        let km = keybindings::Keymap::with_user("[global]\nquit = [\"cmd+shift+q\"]\n[vim]\ndown = []\n");
        assert!(rows(&km, &reg, "Everywhere").contains(&("⌘⇧Q".into(), "quit".into())));
        assert!(!rows(&km, &reg, "Vim motions").iter().any(|(_, what)| what == "down"), "unbound: not listed");
        let mut reg = Registry::default();
        reg.commands.push(CommandEntry { plugin: "p".into(), name: "p.x".into(), desc: "do x".into(), remote: false });
        reg.chords.push(ChordEntry { plugin: "p".into(), chord: keybindings::Binding::parse("cmd+shift+y", &[]).unwrap(), command: "p.x".into(), when: When::Any, filetype: None });
        assert_eq!(rows(&d.with_plugins(&reg), &reg, "Plugins"), vec![("⌘⇧Y".to_string(), "do x".to_string())]);
    }

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

    #[derive(Debug)]
    struct Dropped(std::path::PathBuf);

    impl egui::DroppedFile for Dropped {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
        fn bytes(&self) -> Result<Vec<u8>, String> {
            std::fs::read(&self.0).map_err(|e| e.to_string())
        }
    }

    /// A file dropped on a local tab's terminal is pasted as its path,
    /// escaped the way Claude Code reads dropped files.
    #[test]
    fn dropped_file_pastes_its_path() {
        let repo = TestRepo::new("echo 1 > a");
        let (ctx, engine) = test_util::engine("local", &repo.path(), &["t"]);
        let mut app = KitchenApp::new(engine.clone());
        let out = repo.dir.join("pasted");
        let script = format!("stty -icanon -echo; cat > {}", kitchen_core::util::shell_quote(out.to_str().unwrap()));
        let term = TermSession::spawn(&ctx, &["sh".into(), "-c".into(), script], None, HashMap::new()).unwrap();
        app.slots.insert("t".into(), TermSlot::Live(term));
        let mut frame = eframe::Frame::_new_kittest();
        let mut step = |app: &mut KitchenApp, dropped_files: Vec<egui::DroppedFileHandle>| {
            let screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0)));
            let raw = egui::RawInput { dropped_files, screen_rect, ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| eframe::App::ui(app, ui, &mut frame));
            out.textures_delta.clear();
        };
        step(&mut app, vec![]);
        std::thread::sleep(Duration::from_millis(300)); // let stty run first
        step(&mut app, vec![std::sync::Arc::new(Dropped("/tmp/Screen Shot (1).png".into()))]);
        let want = r"/tmp/Screen\ Shot\ \(1\).png";
        let mut got = String::new();
        for _ in 0..100 {
            got = std::fs::read_to_string(&out).unwrap_or_default();
            if got == want {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(got, want);
    }

    /// The viewer cache: a cached view comes back as it was, a switch
    /// overtaken by a later one is dropped, a moved worktree rebuilds its
    /// viewers with their bases, and ⌘⇧S from the terminal opens the menu.
    #[test]
    fn viewer_cache_switches_back_and_drops_late_views() {
        let repo = TestRepo::new("echo 1 > a");
        let (ctx, engine) = test_util::engine("local", &repo.path(), &["t"]);
        let mut app = KitchenApp::new(engine.clone());
        let wt = repo.path();
        let (a, b): (Shown, Shown) = ((wt.clone(), "origin/a".into()), (wt.clone(), "origin/b".into()));
        let rev_less = |base: &str| View { tab: "t".into(), at: crate::engine::ViewAt { base: Some(base.into()), ..Default::default() } };
        let opened = |app: &KitchenApp, key: &Shown| app.viewers.get(&("t".to_string(), key.clone())).and_then(|v| v.location().0);

        // Numbers come from the engine, as the UI's and the engine's switches do.
        let (s1, s2) = (engine.next_view_seq(), engine.next_view_seq());
        engine.push_ui(UiCommand::ShowViewer { tab_id: "t".into(), view: a.clone(), seq: s1 });
        app.handle_ui_commands(&ctx);
        app.panel_command_for("t", SideKind::Viewer, PanelCmd::Open { path: Some("a".into()), line: Some(1), diff: Some(false) });
        engine.push_ui(UiCommand::ShowViewer { tab_id: "t".into(), view: b.clone(), seq: s2 });
        app.handle_ui_commands(&ctx);
        assert_eq!(app.shown.get("t"), Some(&b));

        // Back to a, from the cache: the same viewer, its file still open.
        app.choose_view(rev_less("origin/a"));
        assert_eq!(app.shown.get("t"), Some(&a));
        assert_eq!(opened(&app, &a).as_deref(), Some("a"));
        // b's switch was overtaken.
        engine.push_ui(UiCommand::ShowViewer { tab_id: "t".into(), view: b.clone(), seq: s2 });
        app.handle_ui_commands(&ctx);
        assert_eq!(app.shown.get("t"), Some(&a), "a late ShowViewer is dropped");

        // An uncached row: numbered at the pick, cooking until the engine
        // answers, then shown.
        app.choose_view(rev_less("origin/c"));
        assert!(app.switching.contains_key("t"), "the frying pan while it checks out");
        let c: Shown = (wt.clone(), "origin/c".into());
        for _ in 0..200 {
            app.handle_ui_commands(&ctx);
            if app.shown.get("t") == Some(&c) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.shown.get("t"), Some(&c));
        assert!(!app.switching.contains_key("t"), "done cooking");
        // A switch that fails stops cooking too (the error is toasted).
        let missing = View { tab: "t".into(), at: crate::engine::ViewAt { rev: Some("refs/nope".into()), name: Some("kitchen-pr-0".into()), ..Default::default() } };
        app.choose_view(missing);
        assert!(app.switching.contains_key("t"));
        for _ in 0..200 {
            app.handle_ui_commands(&ctx);
            if !app.switching.contains_key("t") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!app.switching.contains_key("t"), "a failed switch stops cooking");
        assert_eq!(app.shown.get("t"), Some(&c), "and leaves the view as it was");
        test_util::remove_siblings(&engine, &repo);

        // The worktree moved: its viewers are rebuilt, each with its base and file.
        app.reload_viewers("t", &wt);
        assert_eq!(opened(&app, &a).as_deref(), Some("a"), "rebuilt at its file");
        assert!(app.viewers.contains_key(&("t".to_string(), a.clone())), "with its base");
        assert_eq!(app.viewer_key("t"), Some(("t".to_string(), c.clone())));
        app.shown.clear();
        assert_eq!(app.viewer_key("t"), Some(("t".to_string(), (wt.clone(), "origin/main".into()))), "default: worktree vs diff base");

        // ⌘⇧S from the terminal: the viewer, with the menu up.
        app.set_focus(Focus::Terminal);
        app.run_action(Action::ViewMenu);
        assert_eq!(app.focus(), Focus::Side);
        assert!(app.viewer_key("t").and_then(|k| app.viewers.get(&k)).is_some_and(|v| v.menu_open()));
    }
}
