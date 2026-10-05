//! The kitchen's state and every action on it.
//!
//! The engine is shared (`Arc<Engine>`) between the UI thread, the socket
//! API threads and background jobs. State sits behind short-lived locks;
//! slow work (git, ssh, gh) runs on the calling thread *without* holding a
//! lock, so a slow devbox never freezes the window. Keyboard shortcuts and
//! `kitchenctl` commands end up calling the same methods here.

use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, bail};
use kitchen_core::db::{Db, PlanComment, TabRow, WorkspaceRow};
use kitchen_core::git::{Repo, branch_refspec};
use kitchen_core::host::Host;
use kitchen_core::tmux::{self, Tmux};
use kitchen_core::util::{new_id, shell_quote, slugify};
use kitchen_core::plan;
use kitchen_proto::{Caller, TabInfo, TabKind, WorkspaceInfo};

use crate::settings::Settings;

#[derive(Debug, Clone)]
pub struct Workspace {
    pub row: WorkspaceRow,
    pub tabs: Vec<Tab>,
    pub selected_tab: Option<String>,
    /// Transient status shown under the workspace name ("bootstrapping…").
    pub status: Option<String>,
    pub error: Option<String>,
    /// Tabs being created right now, shown in the tab strip immediately.
    pub pending: Vec<PendingTab>,
}

#[derive(Debug, Clone)]
pub struct PendingTab {
    pub id: String,
    pub title: String,
    pub kind: TabKind,
    /// The new tab will take focus; the terminal area shows its progress.
    pub focus: bool,
}

impl Workspace {
    pub fn host(&self) -> Host {
        Host::parse(&self.row.host)
    }

    pub fn repo_name(&self) -> &str {
        self.row.repo_path.trim_end_matches('/').rsplit('/').next().unwrap_or(&self.row.repo_path)
    }

    pub fn tab(&self, id: &str) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.row.id == id)
    }

    pub fn selected(&self) -> Option<&Tab> {
        self.selected_tab.as_deref().and_then(|id| self.tab(id)).or(self.tabs.first())
    }
}

#[derive(Debug, Clone)]
pub struct Tab {
    pub row: TabRow,
    pub unread: bool,
}

impl Tab {
    pub fn kind(&self) -> TabKind {
        TabKind::parse(&self.row.kind).unwrap_or(TabKind::Edit)
    }

    /// Rev the tab's changes are shown against.
    pub fn diff_base(&self, ws: &Workspace) -> String {
        diff_base_of(&ws.row, &self.row)
    }
}

fn diff_base_of(ws: &WorkspaceRow, tab: &TabRow) -> String {
    tab.diff_base.clone().unwrap_or_else(|| format!("origin/{}", ws.default_branch.as_deref().unwrap_or("main")))
}

/// Environment of a tab's shell (same for a new and a re-created session).
fn tab_env(ws: &WorkspaceRow, tab: &TabRow, host: &Host) -> Vec<(String, String)> {
    let mut env = vec![
        (kitchen_proto::ENV_WORKSPACE.to_string(), ws.id.clone()),
        (kitchen_proto::ENV_TAB.to_string(), tab.id.clone()),
        (kitchen_proto::ENV_TAB_KIND.to_string(), tab.kind.clone()),
        (kitchen_proto::ENV_DIFF_BASE.to_string(), diff_base_of(ws, tab)),
    ];
    env.extend(local_socket_env(host));
    if tab.kind == TabKind::Readonly.as_str() {
        env.push((kitchen_proto::ENV_READONLY.to_string(), "1".into()));
    }
    if let Some(p) = &tab.plan_file {
        env.push((kitchen_proto::ENV_PLAN_FILE.to_string(), p.clone()));
    }
    env
}

/// Requests for the UI thread (it owns terminals and viewer state).
#[derive(Debug, Clone)]
pub enum UiCommand {
    OpenViewer { tab_id: String, path: Option<String>, line: Option<usize>, diff: Option<bool> },
    OpenPlan { tab_id: String },
    ReloadPlan { tab_id: String },
    /// Drop the tab's viewer (its worktree moved or its diff base changed).
    ResetViewer { tab_id: String },
    Screenshot { path: String, reply: std::sync::mpsc::Sender<Result<(), String>> },
    /// Feed input events to the window (`kitchenctl debug keys`).
    Keys(Vec<egui::Event>),
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub error: bool,
    pub at: std::time::Instant,
}

#[derive(Default)]
pub struct State {
    pub workspaces: Vec<Workspace>,
    pub selected: Option<String>,
    pub toasts: Vec<Toast>,
}

/// Options for a new tab (from Cmd+E/Cmd+I or `kitchenctl tab new`).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct NewTab {
    pub kind: Option<String>,
    /// Ticket / task description. Edit tabs start planning on it.
    pub task: Option<String>,
    pub branch: Option<String>,
    /// Read-only tab at this rev (checked out in its own worktree, named
    /// after `name`) instead of the latest default branch.
    pub rev: Option<String>,
    /// Refspecs fetched from origin first (same round trip as the checkout).
    pub fetch: Vec<String>,
    /// What the viewer diffs against (default `origin/<default>`).
    pub diff_base: Option<String>,
    /// Worktree name for `rev` tabs; a tab already on it is reused.
    pub name: Option<String>,
    pub title: Option<String>,
    /// Command typed into the new shell instead of the default agent.
    pub command: Option<String>,
    /// Start no agent at all.
    pub no_agent: bool,
    pub focus: Option<bool>,
}

/// Releases a worktree from [`Engine::opening`] when a rev tab is done.
struct OpeningGuard<'a> {
    engine: &'a Engine,
    path: String,
}

impl Drop for OpeningGuard<'_> {
    fn drop(&mut self) {
        self.engine.opening.lock().unwrap().remove(&self.path);
    }
}

pub struct Engine {
    pub ctx: egui::Context,
    db: Mutex<Db>,
    state: Mutex<State>,
    ui_cmds: Mutex<Vec<UiCommand>>,
    settings: Mutex<Settings>,
    /// Tabs whose tmux session was just created: the first attach can skip
    /// the "does the session still exist?" round trip.
    fresh_sessions: Mutex<std::collections::HashSet<String>>,
    /// Worktrees of rev tabs being opened (a second open waits its turn).
    opening: Mutex<std::collections::HashSet<String>>,
    plugins: std::sync::OnceLock<Arc<crate::plugins::PluginHost>>,
}

impl Engine {
    pub fn new(ctx: egui::Context, db: Db) -> Result<Arc<Engine>> {
        let settings = db
            .setting("settings")?
            .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
            .unwrap_or_default();
        let mut state = State::default();
        for row in db.workspaces()? {
            let tabs = db.tabs(&row.id)?.into_iter().map(|row| Tab { row, unread: false }).collect();
            let selected_tab = db.setting(&format!("selected_tab:{}", row.id))?;
            state.workspaces.push(Workspace { row, tabs, selected_tab, status: None, error: None, pending: Vec::new() });
        }
        state.selected = db.setting("selected_workspace")?.filter(|id| state.workspaces.iter().any(|w| &w.row.id == id));
        if state.selected.is_none() {
            state.selected = state.workspaces.first().map(|w| w.row.id.clone());
        }
        Ok(Arc::new(Engine {
            ctx,
            db: Mutex::new(db),
            state: Mutex::new(state),
            ui_cmds: Mutex::new(Vec::new()),
            settings: Mutex::new(settings),
            fresh_sessions: Mutex::new(Default::default()),
            opening: Mutex::new(Default::default()),
            plugins: std::sync::OnceLock::new(),
        }))
    }

    pub fn set_plugins(&self, host: Arc<crate::plugins::PluginHost>) {
        let _ = self.plugins.set(host);
    }

    /// The plugin host (absent only before startup finished, and in tests).
    pub fn plugins(&self) -> Option<&Arc<crate::plugins::PluginHost>> {
        self.plugins.get()
    }

    pub fn db(&self) -> MutexGuard<'_, Db> {
        self.db.lock().unwrap()
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    fn repaint(&self) {
        self.ctx.request_repaint();
    }

    pub fn push_ui(&self, cmd: UiCommand) {
        self.ui_cmds.lock().unwrap().push(cmd);
        self.repaint();
    }

    pub fn take_ui_commands(&self) -> Vec<UiCommand> {
        std::mem::take(&mut *self.ui_cmds.lock().unwrap())
    }

    pub fn toast(&self, text: impl Into<String>) {
        self.push_toast(text.into(), false);
    }

    pub fn toast_err(&self, text: impl Into<String>) {
        self.push_toast(text.into(), true);
    }

    fn push_toast(&self, text: String, error: bool) {
        if error {
            log::warn!("{text}");
        }
        let mut st = self.state();
        st.toasts.push(Toast { text, error, at: std::time::Instant::now() });
        let n = st.toasts.len();
        if n > 20 {
            st.toasts.drain(..n - 20);
        }
        drop(st);
        self.repaint();
    }

    /// Run `f` on a background thread; errors become red toasts.
    pub fn spawn<F>(self: &Arc<Self>, what: &'static str, f: F)
    where
        F: FnOnce(&Arc<Engine>) -> Result<()> + Send + 'static,
    {
        let me = self.clone();
        std::thread::Builder::new()
            .name(what.into())
            .spawn(move || {
                if let Err(e) = f(&me) {
                    me.toast_err(format!("{what}: {e:#}"));
                }
                me.repaint();
            })
            .expect("spawn thread");
    }

    // ================================================================ lookup

    pub fn workspaces(&self) -> Vec<Workspace> {
        self.state().workspaces.clone()
    }

    pub fn workspace(&self, id: &str) -> Option<Workspace> {
        self.state().workspaces.iter().find(|w| w.row.id == id).cloned()
    }

    pub fn selected_workspace(&self) -> Option<Workspace> {
        let st = self.state();
        let id = st.selected.clone()?;
        st.workspaces.iter().find(|w| w.row.id == id).cloned()
    }

    pub fn find_tab(&self, tab_id: &str) -> Option<(Workspace, Tab)> {
        let st = self.state();
        for w in &st.workspaces {
            if let Some(t) = w.tab(tab_id) {
                return Some((w.clone(), t.clone()));
            }
        }
        None
    }

    /// Resolve a workspace handle: id, `workspace:N`, `N` (1-based), or name.
    /// `None` means the caller's workspace, then the selected one.
    pub fn resolve_workspace(&self, handle: Option<&str>, caller: &Caller) -> Result<String> {
        let st = self.state();
        let handle = handle.map(str::trim).filter(|h| !h.is_empty());
        let Some(h) = handle else {
            if let Some(id) = &caller.workspace_id {
                if st.workspaces.iter().any(|w| &w.row.id == id) {
                    return Ok(id.clone());
                }
            }
            return st.selected.clone().ok_or_else(|| anyhow!("no workspaces yet; create one with `kitchenctl workspace new`"));
        };
        let idx = h.strip_prefix("workspace:").unwrap_or(h);
        if let Ok(n) = idx.parse::<usize>() {
            if n >= 1 && n <= st.workspaces.len() {
                return Ok(st.workspaces[n - 1].row.id.clone());
            }
        }
        st.workspaces
            .iter()
            .find(|w| w.row.id == h || w.row.name == h)
            .map(|w| w.row.id.clone())
            .ok_or_else(|| anyhow!("no workspace matches {h:?}"))
    }

    /// Resolve a tab handle: id, `tab:N`, `N` (1-based within the workspace),
    /// or title. `None` means the caller's tab, then the workspace's selected tab.
    pub fn resolve_tab(&self, handle: Option<&str>, workspace: Option<&str>, caller: &Caller) -> Result<String> {
        let handle = handle.map(str::trim).filter(|h| !h.is_empty());
        if handle.is_none() && workspace.is_none() {
            if let Some(id) = &caller.tab_id {
                if self.find_tab(id).is_some() {
                    return Ok(id.clone());
                }
            }
        }
        if let Some(h) = handle {
            if self.find_tab(h).is_some() {
                return Ok(h.to_string());
            }
        }
        let ws_id = self.resolve_workspace(workspace, caller)?;
        let ws = self.workspace(&ws_id).context("workspace vanished")?;
        match handle {
            None => ws.selected().map(|t| t.row.id.clone()).ok_or_else(|| anyhow!("workspace {} has no tabs", ws.row.name)),
            Some(h) => {
                let idx = h.strip_prefix("tab:").unwrap_or(h);
                if let Ok(n) = idx.parse::<usize>() {
                    if n >= 1 && n <= ws.tabs.len() {
                        return Ok(ws.tabs[n - 1].row.id.clone());
                    }
                }
                ws.tabs
                    .iter()
                    .find(|t| t.row.title == h || t.row.tmux_session == h)
                    .map(|t| t.row.id.clone())
                    .ok_or_else(|| anyhow!("no tab matches {h:?} in workspace {}", ws.row.name))
            }
        }
    }

    pub fn workspace_info(&self) -> Vec<WorkspaceInfo> {
        let st = self.state();
        st.workspaces
            .iter()
            .enumerate()
            .map(|(i, w)| WorkspaceInfo {
                id: w.row.id.clone(),
                index: i + 1,
                name: w.row.name.clone(),
                host: w.row.host.clone(),
                repo_path: w.row.repo_path.clone(),
                github_slug: w.row.github_slug.clone(),
                default_branch: w.row.default_branch.clone(),
                selected: st.selected.as_deref() == Some(&w.row.id),
                tabs: tab_infos(w),
            })
            .collect()
    }

    // ================================================================ workspaces

    /// Create a workspace for `repo_path` on `host` ("local" or an ssh alias).
    /// Verifies the repo, then bootstraps the host in the background.
    pub fn create_workspace(self: &Arc<Self>, name: &str, host: &str, repo_path: &str) -> Result<WorkspaceRow> {
        let h = Host::parse(host);
        let repo_path = h.expand_home(repo_path.trim())?;
        let repo_path = repo_path.trim_end_matches('/').to_string();
        let repo = Repo::new(&h, &repo_path);
        if !repo.is_repo() {
            bail!("{repo_path} on {} is not a git repository", h.label());
        }
        let github_slug = repo.github_slug();
        let default_branch = repo.default_branch().ok();
        let name = if name.trim().is_empty() {
            match &h {
                Host::Local => repo.repo_name().to_string(),
                Host::Ssh(a) => format!("{} @ {a}", repo.repo_name()),
            }
        } else {
            name.trim().to_string()
        };
        let row = {
            let st = self.state();
            WorkspaceRow {
                id: new_id(),
                name,
                host: h.label().to_string(),
                repo_path,
                github_slug,
                default_branch,
                position: st.workspaces.iter().map(|w| w.row.position).max().unwrap_or(0) + 1,
                pinned: false,
            }
        };
        self.db().insert_workspace(&row)?;
        {
            let mut st = self.state();
            st.workspaces.push(Workspace {
                row: row.clone(),
                tabs: Vec::new(),
                selected_tab: None,
                status: Some("setting up…".into()),
                error: None,
                pending: Vec::new(),
            });
        }
        self.select_workspace(&row.id);
        let id = row.id.clone();
        self.spawn("bootstrap", move |me| me.bootstrap_workspace(&id));
        Ok(row)
    }

    /// Host setup that must happen before tabs are useful: dotfiles, skills,
    /// kitchenctl, and the API relay for devboxes. Idempotent.
    pub fn bootstrap_workspace(self: &Arc<Self>, ws_id: &str) -> Result<()> {
        let ws = self.workspace(ws_id).context("no such workspace")?;
        self.set_ws_status(ws_id, Some("syncing dotfiles…"), None);
        let settings = self.settings();
        let report = kitchen_core::bootstrap::bootstrap_host(&ws.host(), &kitchen_core::bootstrap::Options {
            dotfiles_local: settings.dotfiles_local.clone(),
            dotfiles_remote_url: settings.dotfiles_remote_url.clone(),
            dotfiles_remote_dir: settings.dotfiles_remote_dir.clone(),
        });
        match report {
            Ok(r) => {
                for w in &r.warnings {
                    self.toast_err(format!("{}: {w}", ws.row.name));
                }
                self.set_ws_status(ws_id, None, None);
            }
            Err(e) => {
                self.set_ws_status(ws_id, None, Some(format!("{e:#}")));
                return Err(e);
            }
        }
        if let Host::Ssh(alias) = ws.host() {
            crate::relay::ensure_relay(self, &alias);
        }
        self.refill_spare(ws_id);
        let host = ws.host();
        match Repo::new(&host, &ws.row.repo_path).tune() {
            Ok(done) => done.iter().for_each(|d| log::info!("{}: {d}", ws.row.name)),
            Err(e) => log::warn!("{}: repo tuning: {e:#}", ws.row.name),
        }
        Ok(())
    }

    /// Bring the workspace's spare worktrees to `origin/<default>` in the
    /// background (create missing ones, fast-forward stale ones). Called after
    /// every fetch of the default branch and whenever a spare was taken over.
    /// One refresh per repo at a time; a request during one makes it run
    /// once more when it is done (a spare may have been taken meanwhile).
    pub fn refill_spare(self: &Arc<Self>, ws_id: &str) {
        let Some(ws) = self.workspace(ws_id) else { return };
        let Some(base) = ws.row.default_branch.clone() else { return };
        let key = format!("spares:{}:{}", ws.row.host, ws.row.repo_path);
        let again = format!("{key}:again");
        {
            let mut opening = self.opening.lock().unwrap();
            if !opening.insert(key.clone()) {
                opening.insert(again);
                return;
            }
        }
        let me = self.clone();
        std::thread::Builder::new()
            .name("spare worktrees".into())
            .spawn(move || {
                let _guard = OpeningGuard { engine: &me, path: key };
                let host = ws.host();
                loop {
                    if let Err(e) = Repo::new(&host, &ws.row.repo_path).ensure_spare_worktree(&base) {
                        log::warn!("{}: spare worktrees: {e:#}", ws.row.name);
                    }
                    if !me.opening.lock().unwrap().remove(&again) {
                        break;
                    }
                }
            })
            .expect("spawn thread");
    }

    /// Hold the workspace's shared read-only worktree while it is synced (a
    /// second sync at the same time would fight over its index and modes);
    /// waits for one in progress, which usually leaves nothing to do.
    pub fn hold_readonly(&self, ws: &Workspace) -> Result<impl Drop + '_> {
        let path = Repo::new(&ws.host(), &ws.row.repo_path).readonly_path();
        let start = std::time::Instant::now();
        while !self.opening.lock().unwrap().insert(path.clone()) {
            if start.elapsed() > std::time::Duration::from_secs(300) {
                bail!("the read-only worktree has been syncing for 5 minutes; try again later");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Ok(OpeningGuard { engine: self, path })
    }

    fn set_pending(&self, ws_id: &str, tab: Option<PendingTab>) {
        let mut st = self.state();
        if let (Some(w), Some(t)) = (st.workspaces.iter_mut().find(|w| w.row.id == ws_id), tab) {
            w.pending.push(t);
        }
        drop(st);
        self.repaint();
    }

    fn clear_pending(&self, ws_id: &str, tab_id: &str) {
        let mut st = self.state();
        if let Some(w) = st.workspaces.iter_mut().find(|w| w.row.id == ws_id) {
            w.pending.retain(|p| p.id != tab_id);
        }
        drop(st);
        self.repaint();
    }

    fn set_ws_status(&self, ws_id: &str, status: Option<&str>, error: Option<String>) {
        let mut st = self.state();
        if let Some(w) = st.workspaces.iter_mut().find(|w| w.row.id == ws_id) {
            w.status = status.map(String::from);
            w.error = error;
        }
        drop(st);
        self.repaint();
    }

    pub fn select_workspace(&self, id: &str) {
        let mut st = self.state();
        if st.workspaces.iter().any(|w| w.row.id == id) {
            st.selected = Some(id.to_string());
        }
        drop(st);
        let _ = self.db().set_setting("selected_workspace", id);
        self.repaint();
    }

    /// Cmd+J / Cmd+K: move the selection down/up the sidebar, wrapping.
    pub fn cycle_workspace(&self, delta: isize) {
        let next = {
            let st = self.state();
            let n = st.workspaces.len();
            if n == 0 {
                return;
            }
            let cur = st.selected.as_ref().and_then(|id| st.workspaces.iter().position(|w| &w.row.id == id)).unwrap_or(0);
            st.workspaces[((cur as isize + delta).rem_euclid(n as isize)) as usize].row.id.clone()
        };
        self.select_workspace(&next);
    }

    /// Sidebar drag: move workspace `id` to insertion slot `to` (0..=len, in
    /// the current order). Pinned and unpinned workspaces each stay in their
    /// own group, so a drop past the boundary lands at the group's edge.
    pub fn move_workspace(&self, id: &str, to: usize) -> Result<()> {
        self.rearrange_workspace(id, |from, _| if to > from { to - 1 } else { to }, None)
    }

    /// Pin a workspace (it joins the bottom of the pinned group at the top of
    /// the sidebar) or unpin it (it becomes the first unpinned workspace).
    pub fn set_workspace_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        self.rearrange_workspace(id, |_, pinned_count| pinned_count, Some(pinned))
    }

    /// Remove `id`, optionally change its pin state, re-insert it at
    /// `slot(from, pinned_count_without_it)` clamped to its group, then persist
    /// every position.
    fn rearrange_workspace(&self, id: &str, slot: impl FnOnce(usize, usize) -> usize, pinned: Option<bool>) -> Result<()> {
        let order = {
            let mut st = self.state();
            let from = st.workspaces.iter().position(|w| w.row.id == id).context("no such workspace")?;
            let mut ws = st.workspaces.remove(from);
            if let Some(p) = pinned {
                ws.row.pinned = p;
            }
            let pinned_count = st.workspaces.iter().filter(|w| w.row.pinned).count();
            let to = slot(from, pinned_count);
            let to = if ws.row.pinned { to.min(pinned_count) } else { to.clamp(pinned_count, st.workspaces.len()) };
            st.workspaces.insert(to, ws);
            for (i, w) in st.workspaces.iter_mut().enumerate() {
                w.row.position = i as i64;
            }
            st.workspaces.iter().map(|w| (w.row.id.clone(), w.row.position, w.row.pinned)).collect::<Vec<_>>()
        };
        self.repaint();
        self.db().reorder_workspaces(&order)
    }

    pub fn rename_workspace(&self, id: &str, name: &str) -> Result<()> {
        let row = {
            let mut st = self.state();
            let w = st.workspaces.iter_mut().find(|w| w.row.id == id).context("no such workspace")?;
            w.row.name = name.to_string();
            w.row.clone()
        };
        self.db().update_workspace(&row)
    }

    /// Remove a workspace from the kitchen and end its tmux sessions.
    /// Worktrees stay on disk.
    pub fn close_workspace(&self, id: &str) -> Result<()> {
        let ws = self.workspace(id).context("no such workspace")?;
        let host = ws.host();
        let t = Tmux::new(&host);
        for tab in &ws.tabs {
            let _ = t.kill_session(&tab.row.tmux_session);
        }
        self.db().delete_workspace(id)?;
        let mut st = self.state();
        st.workspaces.retain(|w| w.row.id != id);
        if st.selected.as_deref() == Some(id) {
            st.selected = st.workspaces.first().map(|w| w.row.id.clone());
        }
        drop(st);
        self.repaint();
        Ok(())
    }

    // ================================================================ tabs

    /// Create a tab: worktree + tmux session (+ agent). Blocking; call from a
    /// background thread.
    pub fn create_tab(self: &Arc<Self>, ws_id: &str, opts: NewTab) -> Result<TabRow> {
        let ws = self.workspace(ws_id).context("no such workspace")?;
        let kind = match opts.kind.as_deref() {
            Some(k) => TabKind::parse(k).with_context(|| format!("unknown tab kind {k:?} (edit|readonly)"))?,
            None => TabKind::Edit,
        };
        let host = ws.host();
        let repo = Repo::new(&host, &ws.row.repo_path);
        let rev_path = opts.rev.as_ref().filter(|_| kind == TabKind::Readonly).map(|rev| {
            let name = opts.name.clone().unwrap_or_else(|| rev.clone());
            repo.sibling_path(&format!("kitchen-ro-{}", slugify(&name, 60)))
        });
        let _guard = match &rev_path {
            Some(path) => {
                if !self.opening.lock().unwrap().insert(path.clone()) {
                    bail!("{} is already being opened", opts.title.as_deref().unwrap_or(path));
                }
                // Same sibling dir of this workspace's repo (stored paths are
                // resolved, e.g. /private/tmp, so compare the name).
                let dir_name = |p: &str| p.trim_end_matches('/').rsplit('/').next().map(str::to_string);
                if let Some(tab) = ws.tabs.iter().find(|t| dir_name(&t.row.worktree) == dir_name(path)) {
                    let r = self.resync_rev_tab(&ws, tab, &opts);
                    self.opening.lock().unwrap().remove(path);
                    return r;
                }
                Some(OpeningGuard { engine: self, path: path.clone() })
            }
            None => None,
        };
        let settings = self.settings();
        let tab_id = new_id();
        let pending_title = opts
            .title
            .clone()
            .or_else(|| opts.task.as_deref().and_then(|t| t.lines().next()).map(String::from))
            .or_else(|| opts.branch.clone())
            .unwrap_or_else(|| if kind == TabKind::Edit { "new tab".into() } else { "read-only".into() });
        self.set_pending(ws_id, Some(PendingTab { id: tab_id.clone(), title: pending_title, kind, focus: opts.focus.unwrap_or(true) }));
        let result = (|| -> Result<TabRow> {
            // Every path below fetches from origin first, so tabs always start
            // from what is on the remote right now. The common paths (new
            // branch, read-only main) do it inside a single host round trip.
            let base = match &ws.row.default_branch {
                Some(b) => b.clone(),
                None => repo.default_branch()?,
            };
            let existing_branch = opts.branch.as_deref().filter(|b| !b.is_empty() && kind == TabKind::Edit);
            if existing_branch.is_some() {
                repo.fetch().context("git fetch origin")?;
            }
            let (worktree, branch, title) = match kind {
                TabKind::Edit => {
                    let (wt, title) = if let Some(b) = existing_branch {
                        let slug = slugify(b.rsplit('/').next().unwrap_or(b), 40);
                        (repo.add_worktree_existing_branch(b, &unique_path(&host, &repo.sibling_path(&slug)))?, b.to_string())
                    } else {
                        let label = opts.title.clone().or(opts.task.clone()).unwrap_or_else(|| format!("tab-{tab_id}"));
                        let first = label.lines().next().unwrap_or(&label).to_string();
                        let mut slug = slugify(&first, 40);
                        if slug.is_empty() {
                            slug = format!("tab-{tab_id}");
                        }
                        let wt = repo.create_edit_worktree(&base, &settings.branch_prefix, &slug);
                        self.refill_spare(ws_id);
                        let wt = wt?;
                        (wt, first)
                    };
                    (wt.path.clone(), wt.branch.clone(), title)
                }
                TabKind::Readonly => {
                    let (path, rev, fetch) = match (&rev_path, &opts.rev) {
                        (Some(p), Some(rev)) => (p.clone(), rev.clone(), opts.fetch.clone()),
                        _ => (repo.readonly_path(), format!("origin/{base}"), vec![branch_refspec(&base)]),
                    };
                    // (rev worktrees are held by `_guard` above)
                    let _shared = if rev_path.is_none() { Some(self.hold_readonly(&ws)?) } else { None };
                    let synced = repo.sync_readonly_worktree_fetching(&path, &rev, &fetch);
                    // The fetch may have moved origin/<default>, and a new rev
                    // worktree may have taken over a spare (even if it failed after).
                    self.refill_spare(ws_id);
                    let (wt, _) = synced?;
                    (wt.path, None, opts.title.clone().unwrap_or_else(|| format!("read-only {base}")))
                }
            };
            let title = opts.title.clone().unwrap_or(title);
            if existing_branch.is_some() {
                // (new-branch worktrees were excluded by their creation script)
                plan::ensure_excluded(&host, &worktree)?;
            }
            let plan_file = (kind == TabKind::Edit).then(|| plan::plan_file_for(&worktree));

            let session = tmux::session_name(ws_id, &tab_id);
            let row = TabRow {
                id: tab_id.clone(),
                workspace_id: ws_id.to_string(),
                kind: kind.as_str().to_string(),
                title,
                worktree,
                branch,
                tmux_session: session,
                plan_file,
                position: ws.tabs.iter().map(|t| t.row.position).max().unwrap_or(0) + 1,
                diff_base: opts.diff_base.clone().filter(|_| rev_path.is_some()),
            };
            let (worktree, plan_file) = (&row.worktree, &row.plan_file);
            let env = tab_env(&ws.row, &row, &host);
            let mut setup = None;
            let command = if opts.no_agent {
                None
            } else if let Some(c) = opts.command.clone() {
                Some(c)
            } else {
                match kind {
                    TabKind::Edit => {
                        let (cmd, prompt_setup) = edit_agent_command(worktree, &settings, opts.task.as_deref(), plan_file.as_deref())?;
                        setup = prompt_setup;
                        Some(cmd)
                    }
                    TabKind::Readonly => settings.autostart_readonly_agent.then(|| {
                        format!("{} --settings {}", settings.readonly_agent_command, shell_quote(&agent_settings_json(false)))
                    }),
                }
            };
            Tmux::new(&host).new_session_with_setup(&row.tmux_session, worktree, &env, command.as_deref(), setup.as_deref())?;
            self.fresh_sessions.lock().unwrap().insert(tab_id.clone());
            self.db().insert_tab(&row)?;
            Ok(row)
        })();
        self.clear_pending(ws_id, &tab_id);
        let row = result?;
        {
            let mut st = self.state();
            if let Some(w) = st.workspaces.iter_mut().find(|w| w.row.id == ws_id) {
                w.tabs.push(Tab { row: row.clone(), unread: false });
                if opts.focus.unwrap_or(true) {
                    w.selected_tab = Some(row.id.clone());
                }
            }
        }
        if opts.focus.unwrap_or(true) {
            self.select_tab(&row.id);
        }
        self.repaint();
        Ok(row)
    }

    /// Text from ⌘I / `kitchenctl tab open` (a PR link, …): handed to the
    /// plugins' `open` handlers, which open a tab with `kitchen.tab_open`.
    pub fn open_text(&self, text: &str) -> Result<()> {
        let ev = serde_json::json!({ "text": text.trim(), "workspaces": self.workspace_info() });
        let handled = self.plugins().is_some_and(|p| p.emit("open", ev));
        if !handled {
            bail!("no plugin opens {:?} (enable the github plugin for PR links)", text.trim());
        }
        Ok(())
    }

    /// Re-open of a rev tab: move its worktree to the (re-fetched) rev, keep
    /// the session, and refresh the viewer if anything changed.
    fn resync_rev_tab(self: &Arc<Self>, ws: &Workspace, tab: &Tab, opts: &NewTab) -> Result<TabRow> {
        let rev = opts.rev.as_deref().unwrap_or_default();
        let (wt, moved) = Repo::new(&ws.host(), &ws.row.repo_path).sync_readonly_worktree_fetching(&tab.row.worktree, rev, &opts.fetch)?;
        self.refill_spare(&ws.row.id);
        let mut row = tab.row.clone();
        let base_changed = opts.diff_base.is_some() && opts.diff_base != row.diff_base;
        if base_changed {
            row.diff_base = opts.diff_base.clone();
            self.db().update_tab(&row)?;
            let mut st = self.state();
            if let Some(t) = st.workspaces.iter_mut().flat_map(|w| w.tabs.iter_mut()).find(|t| t.row.id == row.id) {
                t.row = row.clone();
            }
        }
        if moved || base_changed {
            self.push_ui(UiCommand::ResetViewer { tab_id: row.id.clone() });
        }
        if moved {
            self.toast(format!("{} updated to {}", row.title, &wt.head[..wt.head.len().min(10)]));
        }
        if opts.focus.unwrap_or(true) {
            self.select_tab(&row.id);
        }
        Ok(row)
    }

    pub fn select_tab(&self, tab_id: &str) {
        let mut ws_id = None;
        {
            let mut st = self.state();
            for w in st.workspaces.iter_mut() {
                if let Some(t) = w.tabs.iter_mut().find(|t| t.row.id == tab_id) {
                    t.unread = false;
                    w.selected_tab = Some(tab_id.to_string());
                    ws_id = Some(w.row.id.clone());
                }
            }
            if let Some(id) = &ws_id {
                st.selected = Some(id.clone());
            }
        }
        if let Some(ws) = ws_id {
            let db = self.db();
            let _ = db.set_setting(&format!("selected_tab:{ws}"), tab_id);
            let _ = db.set_setting("selected_workspace", &ws);
            let _ = db.mark_notifications_read(Some(tab_id));
        }
        self.repaint();
    }

    /// Cmd+<n>: focus the n-th (1-based) tab of the selected workspace.
    pub fn select_tab_index(&self, n: usize) {
        let id = self.selected_workspace().and_then(|w| w.tabs.get(n.wrapping_sub(1)).map(|t| t.row.id.clone()));
        if let Some(id) = id {
            self.select_tab(&id);
        }
    }

    /// ⌘H / ⌘L: previous / next tab of the selected workspace, wrapping.
    pub fn cycle_tab(&self, delta: isize) {
        let Some(ws) = self.selected_workspace() else { return };
        let n = ws.tabs.len();
        if n == 0 {
            return;
        }
        let cur = ws.selected().and_then(|t| ws.tabs.iter().position(|x| x.row.id == t.row.id)).unwrap_or(0);
        let next = ws.tabs[((cur as isize + delta).rem_euclid(n as isize)) as usize].row.id.clone();
        self.select_tab(&next);
    }

    pub fn rename_tab(&self, tab_id: &str, title: &str) -> Result<()> {
        let row = {
            let mut st = self.state();
            let t = st
                .workspaces
                .iter_mut()
                .flat_map(|w| w.tabs.iter_mut())
                .find(|t| t.row.id == tab_id)
                .context("no such tab")?;
            t.row.title = title.to_string();
            t.row.clone()
        };
        self.db().update_tab(&row)
    }

    /// Close a tab: end its tmux session. The worktree stays on disk (it may
    /// hold unpushed work); remove it with `kitchenctl worktree remove`.
    pub fn close_tab(&self, tab_id: &str) -> Result<()> {
        let (ws, tab) = self.find_tab(tab_id).context("no such tab")?;
        let _ = Tmux::new(&ws.host()).kill_session(&tab.row.tmux_session);
        self.db().delete_tab(tab_id)?;
        let mut st = self.state();
        if let Some(w) = st.workspaces.iter_mut().find(|w| w.row.id == ws.row.id) {
            let idx = w.tabs.iter().position(|t| t.row.id == tab_id).unwrap_or(0);
            w.tabs.retain(|t| t.row.id != tab_id);
            if w.selected_tab.as_deref() == Some(tab_id) {
                w.selected_tab = w.tabs.get(idx.min(w.tabs.len().saturating_sub(1))).map(|t| t.row.id.clone());
            }
        }
        drop(st);
        self.repaint();
        Ok(())
    }

    /// (host, tmux session) for a tab.
    pub fn tab_target(&self, tab_id: &str) -> Result<(Host, String)> {
        let (ws, tab) = self.find_tab(tab_id).context("no such tab")?;
        Ok((ws.host(), tab.row.tmux_session))
    }

    pub fn send_text(&self, tab_id: &str, text: &str, enter: bool) -> Result<()> {
        let (host, session) = self.tab_target(tab_id)?;
        Tmux::new(&host).send_text(&session, text, enter)
    }

    pub fn send_keys(&self, tab_id: &str, keys: &[&str]) -> Result<()> {
        let (host, session) = self.tab_target(tab_id)?;
        Tmux::new(&host).send_keys(&session, keys)
    }

    pub fn read_screen(&self, tab_id: &str, scrollback: Option<u32>) -> Result<String> {
        let (host, session) = self.tab_target(tab_id)?;
        Tmux::new(&host).capture(&session, scrollback)
    }

    /// Make sure a tab's tmux session exists (recreating a dead one in the
    /// same worktree). Returns false when the worktree itself is gone.
    pub fn ensure_session(&self, tab_id: &str) -> Result<bool> {
        if self.fresh_sessions.lock().unwrap().remove(tab_id) {
            return Ok(true); // created moments ago by create_tab
        }
        let (ws, tab) = self.find_tab(tab_id).context("no such tab")?;
        let host = ws.host();
        let env = tab_env(&ws.row, &tab.row, &host);
        Tmux::new(&host).ensure_session(&tab.row.tmux_session, &tab.row.worktree, &env)
    }

    // ================================================================ notifications

    /// Record a notification, badge the tab, and raise a macOS banner.
    pub fn notify(&self, ws_id: Option<&str>, tab_id: Option<&str>, title: &str, body: &str) -> Result<i64> {
        let id = self.db().add_notification(ws_id, tab_id, title, body)?;
        if let Some(tab) = tab_id {
            let selected = self.selected_workspace().and_then(|w| w.selected().map(|t| t.row.id.clone()));
            if selected.as_deref() != Some(tab) || !self.ctx.input(|i| i.focused) {
                let mut st = self.state();
                if let Some(t) = st.workspaces.iter_mut().flat_map(|w| w.tabs.iter_mut()).find(|t| t.row.id == tab) {
                    t.unread = true;
                }
            }
        }
        let script = format!(
            "display notification {} with title {}",
            applescript_string(body),
            applescript_string(&format!("kitchen: {title}"))
        );
        let _ = std::process::Command::new("osascript").arg("-e").arg(script).spawn();
        self.toast(format!("{title}: {body}"));
        Ok(id)
    }

    // ================================================================ plans

    pub fn plan_comments(&self, tab_id: &str) -> Result<Vec<PlanComment>> {
        self.db().plan_comments(tab_id)
    }

    pub fn add_plan_comment(&self, tab_id: &str, line: i64, end_line: Option<i64>, quote: &str, body: &str) -> Result<i64> {
        let id = self.db().add_plan_comment(tab_id, line, end_line, quote, body)?;
        self.repaint();
        Ok(id)
    }

    /// Send every open plan comment to the tab's agent as one message.
    pub fn send_plan_feedback(&self, tab_id: &str) -> Result<usize> {
        let (_, tab) = self.find_tab(tab_id).context("no such tab")?;
        let comments = self.plan_comments(tab_id)?;
        let open: Vec<&PlanComment> = comments.iter().filter(|c| c.state == "open").collect();
        if open.is_empty() {
            bail!("no open plan comments to send");
        }
        let plan_file = tab.row.plan_file.clone().unwrap_or_default();
        let msg = plan::feedback_message(&plan_file, &open);
        self.send_text(tab_id, &msg, true)?;
        let db = self.db();
        for c in &open {
            db.set_plan_comment_state(c.id, "sent")?;
        }
        drop(db);
        self.repaint();
        Ok(open.len())
    }

    pub fn resolve_plan_comment(&self, id: i64) -> Result<()> {
        if !self.db().set_plan_comment_state(id, "resolved")? {
            bail!("no plan comment {id}");
        }
        self.repaint();
        Ok(())
    }
}

/// The shell command that starts an edit tab's agent, plus a setup snippet
/// that writes the planning prompt file (run in the same host round trip as
/// the tmux session). With a task, the agent begins in the planning phase
/// (`/low-level-plan`).
fn edit_agent_command(
    worktree: &str,
    settings: &Settings,
    task: Option<&str>,
    plan_file: Option<&str>,
) -> Result<(String, Option<String>)> {
    let base = format!("{} --settings {}", settings.edit_agent_command, shell_quote(&agent_settings_json(true)));
    let Some(task) = task.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok((base, None));
    };
    let plan_file = plan_file.context("edit tab without plan file")?;
    let dir = format!("{worktree}/{}", plan::KITCHEN_DIR);
    let prompt_path = format!("{dir}/prompt.md");
    let setup = format!(
        "mkdir -p {} && printf %s {} > {}",
        shell_quote(&dir),
        shell_quote(&plan::planning_prompt(task, plan_file)),
        shell_quote(&prompt_path)
    );
    Ok((format!("{base} \"$(cat {})\"", shell_quote(&prompt_path)), Some(setup)))
}

/// `--settings` for agents the kitchen starts: plan files land where the
/// plan reviewer looks (edit tabs), and Stop/Notification hooks badge the
/// tab through `kitchenctl notify` so a finished or blocked agent is visible
/// from any workspace.
fn agent_settings_json(plans: bool) -> String {
    let hook = |title: &str| {
        serde_json::json!([{ "hooks": [{ "type": "command",
            "command": format!("command -v kitchenctl >/dev/null && kitchenctl notify --title '{title}' --body \"$(basename \"$PWD\")\" >/dev/null 2>&1; true") }] }])
    };
    let mut v = serde_json::json!({ "hooks": { "Stop": hook("agent finished"), "Notification": hook("agent needs you") } });
    if plans {
        v["plansDirectory"] = serde_json::json!(".kitchen/plans");
    }
    v.to_string()
}

fn tab_infos(w: &Workspace) -> Vec<TabInfo> {
    let sel = w.selected().map(|t| t.row.id.clone());
    w.tabs
        .iter()
        .enumerate()
        .map(|(i, t)| TabInfo {
            id: t.row.id.clone(),
            index: i + 1,
            kind: t.kind(),
            title: t.row.title.clone(),
            worktree: t.row.worktree.clone(),
            branch: t.row.branch.clone(),
            tmux_session: t.row.tmux_session.clone(),
            selected: sel.as_deref() == Some(&t.row.id),
            plan_file: t.row.plan_file.clone(),
            diff_base: t.row.diff_base.clone(),
            unread: t.unread,
        })
        .collect()
}

/// `path`, or `path-2`, `path-3`… — the first that does not exist yet.
fn unique_path(host: &Host, path: &str) -> String {
    if !host.exists(path) {
        return path.to_string();
    }
    (2..100).map(|n| format!("{path}-{n}")).find(|p| !host.exists(p)).unwrap_or_else(|| format!("{path}-{}", new_id()))
}

/// Local tabs point kitchenctl at this app's socket explicitly (it differs
/// from the default when the app runs with `KITCHEN_STATE_DIR`). Devbox tabs
/// use the relay's default socket there.
fn local_socket_env(host: &Host) -> Option<(String, String)> {
    host.is_local()
        .then(|| (kitchen_proto::ENV_SOCKET.to_string(), kitchen_proto::default_socket_path().display().to_string()))
}

fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_with(ids: &[&str]) -> Arc<Engine> {
        let db = Db::open_in_memory().unwrap();
        for (i, id) in ids.iter().enumerate() {
            db.insert_workspace(&WorkspaceRow {
                id: id.to_string(),
                name: id.to_string(),
                host: "local".into(),
                repo_path: "/r".into(),
                github_slug: None,
                default_branch: None,
                position: i as i64,
                pinned: false,
            })
            .unwrap();
        }
        Engine::new(egui::Context::default(), db).unwrap()
    }

    fn order(e: &Engine) -> Vec<String> {
        e.workspaces().into_iter().map(|w| w.row.id).collect()
    }

    #[test]
    fn move_and_pin_workspaces() {
        let e = engine_with(&["a", "b", "c", "d"]);
        e.move_workspace("a", 3).unwrap();
        assert_eq!(order(&e), ["b", "c", "a", "d"]);
        e.move_workspace("d", 0).unwrap();
        assert_eq!(order(&e), ["d", "b", "c", "a"]);

        e.set_workspace_pinned("c", true).unwrap();
        e.set_workspace_pinned("a", true).unwrap();
        assert_eq!(order(&e), ["c", "a", "d", "b"]);
        // An unpinned workspace can't be dragged above the pinned group,
        // and a pinned one can't leave it.
        e.move_workspace("b", 0).unwrap();
        assert_eq!(order(&e), ["c", "a", "b", "d"]);
        e.move_workspace("c", 4).unwrap();
        assert_eq!(order(&e), ["a", "c", "b", "d"]);
        e.set_workspace_pinned("a", false).unwrap();
        assert_eq!(order(&e), ["c", "a", "b", "d"]);

        // The order survives a restart.
        let rows = e.db().workspaces().unwrap();
        assert_eq!(rows.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), ["c", "a", "b", "d"]);
        assert_eq!(rows.iter().map(|w| w.pinned).collect::<Vec<_>>(), [true, false, false, false]);
    }
}
