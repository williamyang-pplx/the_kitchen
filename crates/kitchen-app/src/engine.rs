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
use kitchen_core::git::{Checkout, Repo, branch_refspec};
use kitchen_core::host::Host;
use kitchen_core::tmux::{self, Tmux};
use kitchen_core::util::{new_id, shell_quote, slugify};
use kitchen_core::plan;
use kitchen_proto::{Caller, TabInfo, WorkspaceInfo};

use crate::links::Link;
use crate::settings::Settings;

/// How often each workspace's warmer fetches `origin/<default>` and
/// fast-forwards its spares (opening the ⌘T dialog also does it once).
const SPARE_REFRESH: std::time::Duration = std::time::Duration::from_secs(300);

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
    /// Rev the tab's changes are shown against.
    pub fn diff_base(&self, ws: &Workspace) -> String {
        diff_base_of(&ws.row)
    }
}

fn diff_base_of(ws: &WorkspaceRow) -> String {
    format!("origin/{}", ws.default_branch.as_deref().unwrap_or("main"))
}

/// Environment of a tab's shell (same for a new and a re-created session).
fn tab_env(ws: &WorkspaceRow, tab: &TabRow, host: &Host) -> Vec<(String, String)> {
    let mut env = vec![
        (kitchen_proto::ENV_WORKSPACE.to_string(), ws.id.clone()),
        (kitchen_proto::ENV_TAB.to_string(), tab.id.clone()),
        (kitchen_proto::ENV_DIFF_BASE.to_string(), diff_base_of(ws)),
    ];
    env.extend(local_socket_env(host));
    env.push((kitchen_proto::ENV_PLAN_FILE.to_string(), tab.plan_file()));
    env
}

/// Requests for the UI thread (it owns terminals and viewer state).
#[derive(Debug, Clone)]
pub enum UiCommand {
    OpenViewer { tab_id: String, path: Option<String>, line: Option<usize>, diff: Option<bool> },
    OpenPlan { tab_id: String },
    /// The tab's viewer shows plugin renderings (`kitchen.render.show`; `None` toggles).
    Render { tab_id: String, on: Option<bool> },
    ReloadPlan { tab_id: String },
    /// Drop the tab's viewer (its worktree moved), or replace it with one on
    /// `view` = (worktree, diff base) (`kitchen.view`).
    ResetViewer { tab_id: String, view: Option<(String, String)> },
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

/// Options for a new tab (from ⌘T, plugins or `kitchenctl tab new`).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct NewTab {
    /// Ticket / task description. The agent starts planning on it.
    pub task: Option<String>,
    /// Existing branch to check out (where it is already checked out, if it is).
    pub branch: Option<String>,
    pub title: Option<String>,
    /// Command typed into the new shell instead of the default agent.
    pub command: Option<String>,
    /// Start no agent at all.
    pub no_agent: bool,
    pub focus: Option<bool>,
}

/// What `kitchen.view` shows in a tab's viewer instead of the tab's worktree.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct View {
    pub tab: String,
    /// Checked out detached in `<repo>-<name>`. None: the tab's own worktree again.
    pub rev: Option<String>,
    pub name: Option<String>,
    /// Refspecs fetched from origin first (same round trip as the checkout).
    #[serde(default)]
    pub fetch: Vec<String>,
    /// What the viewer diffs against (default `origin/<default>`).
    pub base: Option<String>,
}

/// Releases a key from [`Engine::opening`] when its checkout is done.
struct OpeningGuard<'a> {
    engine: &'a Engine,
    key: String,
}

impl Drop for OpeningGuard<'_> {
    fn drop(&mut self) {
        self.engine.opening.lock().unwrap().remove(&self.key);
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
        self.keep_spares_warm(ws_id);
        let host = ws.host();
        match Repo::new(&host, &ws.row.repo_path).tune() {
            Ok(done) => done.iter().for_each(|d| log::info!("{}: {d}", ws.row.name)),
            Err(e) => log::warn!("{}: repo tuning: {e:#}", ws.row.name),
        }
        Ok(())
    }

    /// Bring the workspace's spare worktrees to `origin/<default>` in the
    /// background (create missing ones, fast-forward stale ones); `fetch`
    /// fetches it first, in the same round trip. Called by the workspace's
    /// warmer, when the ⌘T dialog opens, after other fetches of the default
    /// branch and whenever a spare was taken over. One refresh per repo at a
    /// time; a request during one makes it run once more when it is done (a
    /// spare may have been taken meanwhile), fetching if any request asked to.
    pub fn refill_spare(self: &Arc<Self>, ws_id: &str, fetch: bool) {
        let Some(ws) = self.workspace(ws_id) else { return };
        let key = format!("spares:{}:{}", ws.row.host, ws.row.repo_path);
        // Two flags: one plain "again" would lose a fetch asked for while a
        // refill-only run was going.
        let (again, again_fetch) = (format!("{key}:again"), format!("{key}:again-fetch"));
        {
            let mut opening = self.opening.lock().unwrap();
            if !opening.insert(key.clone()) {
                opening.insert(again);
                if fetch {
                    opening.insert(again_fetch);
                }
                return;
            }
        }
        let me = self.clone();
        std::thread::Builder::new()
            .name("spare worktrees".into())
            .spawn(move || {
                let _guard = OpeningGuard { engine: &me, key };
                let host = ws.host();
                let repo = Repo::new(&host, &ws.row.repo_path);
                // (no cached default branch: without one the spares would never move)
                let Some(base) = ws.row.default_branch.clone().or_else(|| repo.default_branch().ok()) else {
                    log::warn!("{}: spare worktrees: no default branch", ws.row.name);
                    return;
                };
                let mut fetch = fetch;
                loop {
                    if let Err(e) = repo.ensure_spare_worktree(&base, fetch) {
                        log::warn!("{}: spare worktrees: {e:#}", ws.row.name);
                    }
                    let (more, more_fetch) = {
                        let mut opening = me.opening.lock().unwrap();
                        (opening.remove(&again), opening.remove(&again_fetch))
                    };
                    if !more {
                        break;
                    }
                    fetch = more_fetch;
                }
            })
            .expect("spawn thread");
    }

    /// Keep the workspace's spares hot so a new tab never waits on the
    /// network: fetch and refresh them now and every [`SPARE_REFRESH`] until
    /// the workspace is closed. One warmer per workspace; repeat calls do
    /// nothing.
    pub fn keep_spares_warm(self: &Arc<Self>, ws_id: &str) {
        let key = format!("warm:{ws_id}");
        if !self.opening.lock().unwrap().insert(key.clone()) {
            return;
        }
        let me = self.clone();
        let ws_id = ws_id.to_string();
        std::thread::Builder::new()
            .name("spare warmer".into())
            .spawn(move || {
                let _guard = OpeningGuard { engine: &me, key };
                while me.workspace(&ws_id).is_some() {
                    me.refill_spare(&ws_id, true);
                    std::thread::sleep(SPARE_REFRESH);
                }
            })
            .expect("spawn thread");
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

    /// Create a tab: worktree + tmux session + agent. Opening a branch that
    /// a tab already has moves that tab forward instead. Blocking; call from
    /// a background thread.
    pub fn create_tab(self: &Arc<Self>, ws_id: &str, opts: NewTab) -> Result<TabRow> {
        let ws = self.workspace(ws_id).context("no such workspace")?;
        let host = ws.host();
        let repo = Repo::new(&host, &ws.row.repo_path);
        let branch = opts.branch.as_deref().filter(|b| !b.is_empty());
        // Where a branch goes: a worktree named after it. `kitchen-*` names
        // are the kitchen's (spares, PR views).
        let target = branch.map(|b| {
            let slug = slugify(b.rsplit('/').next().unwrap_or(b), 40);
            let slug = if slug.starts_with("kitchen") { format!("tab-{slug}") } else { slug };
            (Checkout::Branch(b), unique_path(&host, &repo.sibling_path(&slug)))
        });
        let fetch: Vec<String> = branch.map(branch_refspec).into_iter().collect();
        let _guard = match &target {
            Some((to, path)) => {
                let key = format!("open:{ws_id}:{}", branch.unwrap_or_default());
                if !self.opening.lock().unwrap().insert(key.clone()) {
                    bail!("{} is already being opened", opts.title.as_deref().unwrap_or(path));
                }
                let guard = OpeningGuard { engine: self, key };
                // Same sibling dir of this workspace's repo (stored paths are
                // resolved, e.g. /private/tmp, so compare the name), or the
                // same branch.
                let dir_name = |p: &str| p.trim_end_matches('/').rsplit('/').next().map(str::to_string);
                let open = ws.tabs.iter().find(|t| dir_name(&t.row.worktree) == dir_name(path) || t.row.branch.as_deref() == branch);
                if let Some(tab) = open {
                    return self.reopen_tab(&ws, tab, *to, &fetch, &opts);
                }
                Some(guard)
            }
            None => None,
        };
        let focus = opts.focus.unwrap_or(true);
        let settings = self.settings();
        let tab_id = new_id();
        let pending_title = opts
            .title
            .clone()
            .or_else(|| opts.task.as_deref().and_then(|t| t.lines().next()).map(String::from))
            .or_else(|| branch.map(String::from))
            .unwrap_or_else(|| "new tab".into());
        self.set_pending(ws_id, Some(PendingTab { id: tab_id.clone(), title: pending_title, focus }));
        let result = (|| -> Result<TabRow> {
            // New-branch tabs start from origin/<default> as the spare warmer
            // last fetched it (at most SPARE_REFRESH old, usually fresher:
            // opening ⌘T fetches), so they never wait on the network.
            // Existing branches are fetched in the same host round trip as
            // their checkout.
            let (wt, title) = match &target {
                Some((to, path)) => {
                    let checked = repo.checkout_worktree(path, *to, &fetch);
                    // The fetch may have moved origin/<default>, and the checkout
                    // may have taken over a spare (even if it failed after).
                    self.refill_spare(ws_id, false);
                    (checked?.0, branch.unwrap_or_default().to_string())
                }
                None => {
                    let base = match &ws.row.default_branch {
                        Some(b) => b.clone(),
                        None => repo.default_branch()?,
                    };
                    let label = opts.title.clone().or(opts.task.clone()).unwrap_or_else(|| format!("tab-{tab_id}"));
                    let first = label.lines().next().unwrap_or(&label).to_string();
                    let mut slug = slugify(&first, 40);
                    if slug.is_empty() {
                        slug = format!("tab-{tab_id}");
                    }
                    let wt = repo.create_edit_worktree(&base, &settings.branch_prefix, &slug);
                    self.refill_spare(ws_id, false);
                    (wt?, first)
                }
            };
            let title = opts.title.clone().unwrap_or(title);

            let session = tmux::session_name(ws_id, &tab_id);
            let row = TabRow {
                id: tab_id.clone(),
                workspace_id: ws_id.to_string(),
                title,
                worktree: wt.path,
                branch: wt.branch,
                tmux_session: session,
                position: ws.tabs.iter().map(|t| t.row.position).max().unwrap_or(0) + 1,
            };
            let worktree = &row.worktree;
            let env = tab_env(&ws.row, &row, &host);
            let mut setup = None;
            let command = if opts.no_agent {
                None
            } else if let Some(c) = opts.command.clone() {
                Some(c)
            } else {
                let (cmd, prompt_setup) = agent_command(worktree, &settings, opts.task.as_deref());
                setup = prompt_setup;
                Some(cmd)
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
            }
        }
        if focus {
            self.select_tab(&row.id);
        }
        self.repaint();
        Ok(row)
    }

    /// Act on a ⌘-clicked terminal link of `tab_id`.
    pub fn open_link(self: &Arc<Self>, tab_id: &str, link: Link) {
        let tab_id = tab_id.to_string();
        match link {
            Link::File(f) => self.spawn("open link", move |engine| {
                let (ws, tab) = engine.find_tab(&tab_id).context("the tab is gone")?;
                let host = ws.host();
                let path = file_link_path(&host, &tab.row.worktree, &f.path)?;
                let path = path.trim_end_matches('/');
                if host.is_dir(path) {
                    // The folder view lists the worktree's files only.
                    let wt = tab.row.worktree.trim_end_matches('/');
                    if path != wt && !path.starts_with(&format!("{wt}/")) {
                        bail!("{path} is a folder outside the worktree");
                    }
                    // A trailing `/` tells the viewer to browse the folder.
                    engine.push_ui(UiCommand::OpenViewer { tab_id, path: Some(format!("{path}/")), line: None, diff: None });
                    return Ok(());
                }
                // A relative path not under the worktree root: the worktree's
                // file whose path ends with it, nearest the root.
                let path = if host.exists(path) {
                    path.to_string()
                } else if is_searchable(&f.path) {
                    let wt = tab.row.worktree.trim_end_matches('/');
                    let matches = Repo::new(&host, wt).find_by_suffix(wt, f.path.trim_start_matches("./"))?;
                    let m = closest(&matches).with_context(|| format!("no such file {}", f.path))?;
                    format!("{wt}/{m}")
                } else {
                    bail!("no such file {path}");
                };
                engine.push_ui(UiCommand::OpenViewer { tab_id, path: Some(path), line: f.line, diff: None });
                Ok(())
            }),
            // Opened on this Mac, devbox tabs included.
            Link::Url(url) => self.spawn("open url", move |_| {
                let status = std::process::Command::new("open").arg(&url).status()?;
                if !status.success() {
                    bail!("open {url} failed");
                }
                Ok(())
            }),
            Link::Plugin { command, text, groups } => {
                let Some(plugins) = self.plugins() else { return };
                let mut ctx = crate::plugins::command_context(self, &tab_id, None);
                ctx["link"] = serde_json::json!({ "text": text, "groups": groups });
                if let Err(e) = plugins.invoke(&command, ctx, false) {
                    self.toast_err(format!("{e:#}"));
                }
            }
        }
    }

    /// Re-open of a branch tab: move its worktree forward to the
    /// (re-fetched) branch, keep the session, and refresh the viewer if HEAD
    /// moved.
    fn reopen_tab(self: &Arc<Self>, ws: &Workspace, tab: &Tab, to: Checkout, fetch: &[String], opts: &NewTab) -> Result<TabRow> {
        let (wt, moved) = Repo::new(&ws.host(), &ws.row.repo_path).checkout_worktree(&tab.row.worktree, to, fetch)?;
        self.refill_spare(&ws.row.id, false);
        if moved {
            self.push_ui(UiCommand::ResetViewer { tab_id: tab.row.id.clone(), view: None });
            self.toast(format!("{} updated to {}", tab.row.title, &wt.head[..wt.head.len().min(10)]));
        }
        if opts.focus.unwrap_or(true) {
            self.select_tab(&tab.row.id);
        }
        Ok(tab.row.clone())
    }

    /// Show `v` in its tab's viewer: `v.rev` checked out detached in the
    /// sibling worktree `<repo>-<name>` (taking over a spare when new), or the
    /// tab's own worktree. Blocking; call from a background thread.
    pub fn view(self: &Arc<Self>, v: View) -> Result<()> {
        let (ws, tab) = self.find_tab(&v.tab).context("no such tab")?;
        let base = v.base.unwrap_or_else(|| tab.diff_base(&ws));
        let Some(rev) = v.rev else {
            self.push_ui(UiCommand::ResetViewer { tab_id: v.tab, view: Some((tab.row.worktree, base)) });
            return Ok(());
        };
        let Some(name) = v.name.filter(|n| !n.is_empty()) else { bail!("kitchen.view needs a name with a rev") };
        let host = ws.host();
        let repo = Repo::new(&host, &ws.row.repo_path);
        let path = repo.sibling_path(&slugify(&name, 60));
        let key = format!("open:{}:{path}", ws.row.id);
        if !self.opening.lock().unwrap().insert(key.clone()) {
            bail!("{path} is already being opened");
        }
        let _guard = OpeningGuard { engine: self, key };
        let checked = repo.checkout_worktree(&path, Checkout::Detached(&rev), &v.fetch);
        // The checkout may have taken over a spare (even if it failed after).
        self.refill_spare(&ws.row.id, false);
        let (wt, _) = checked?;
        self.push_ui(UiCommand::ResetViewer { tab_id: v.tab, view: Some((wt.path, base)) });
        Ok(())
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
        crate::notify::post(id, title, body, tab_id);
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
        let msg = plan::feedback_message(&tab.row.plan_file(), &open);
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

/// The shell command that starts a tab's agent, plus a setup snippet
/// that writes the planning prompt file (run in the same host round trip as
/// the tmux session). With a task, the agent begins in the planning phase
/// (`/low-level-plan`).
fn agent_command(worktree: &str, settings: &Settings, task: Option<&str>) -> (String, Option<String>) {
    let base = format!("{} --settings {}", settings.edit_agent_command, shell_quote(&agent_settings_json()));
    let Some(task) = task.map(str::trim).filter(|t| !t.is_empty()) else {
        return (base, None);
    };
    let dir = format!("{worktree}/{}", plan::KITCHEN_DIR);
    let prompt_path = format!("{dir}/prompt.md");
    let setup = format!(
        "mkdir -p {} && printf %s {} > {}",
        shell_quote(&dir),
        shell_quote(&plan::planning_prompt(task, &plan::plan_file_for(worktree))),
        shell_quote(&prompt_path)
    );
    (format!("{base} \"$(cat {})\"", shell_quote(&prompt_path)), Some(setup))
}

/// Where a printed path points: `~/…` expanded on the host, absolute as-is,
/// anything else under `worktree`.
fn file_link_path(host: &Host, worktree: &str, path: &str) -> Result<String> {
    if path.starts_with("~/") {
        return host.expand_home(path);
    }
    if path.starts_with('/') {
        return Ok(path.to_string());
    }
    Ok(format!("{worktree}/{}", path.trim_start_matches("./")))
}

/// A printed path worth searching the worktree for: relative, not `~/` or
/// `../` (those name one place), not a folder.
fn is_searchable(path: &str) -> bool {
    !path.starts_with(['/', '~']) && !path.starts_with("../") && !path.ends_with('/')
}

/// The match nearest the worktree root: fewest components, then by name.
fn closest(matches: &[String]) -> Option<&String> {
    matches.iter().min_by_key(|m| (m.matches('/').count(), m.as_str()))
}

/// `--settings` for agents the kitchen starts: plan files land where the
/// plan reviewer looks, and Stop/Notification hooks badge the
/// tab through `kitchenctl notify` so a finished or blocked agent is visible
/// from any workspace. Stop stays quiet while background subagents run.
fn agent_settings_json() -> String {
    let hook = |matcher: &str, command: String| {
        serde_json::json!([{ "matcher": matcher, "hooks": [{ "type": "command",
            "command": format!("command -v kitchenctl >/dev/null && {{ {command}; }} >/dev/null 2>&1; true") }] }])
    };
    let args = |title: &str| format!("--title '{title}' --body \"$(basename \"$PWD\")\"");
    let stop = hook(
        "",
        // A kitchenctl older than __agent-stopped exits 2; plain notify then.
        format!("kitchenctl __agent-stopped {a} || kitchenctl notify {a}", a = args("agent finished")),
    );
    // Only prompts that block the agent; idle_prompt repeats Stop a minute later.
    let blocked = hook(
        "permission_prompt|elicitation_dialog|elicitation_url_dialog",
        format!("kitchenctl notify {}", args("agent needs you")),
    );
    serde_json::json!({ "hooks": { "Stop": stop, "Notification": blocked }, "plansDirectory": ".kitchen/plans" }).to_string()
}

fn tab_infos(w: &Workspace) -> Vec<TabInfo> {
    let sel = w.selected().map(|t| t.row.id.clone());
    w.tabs
        .iter()
        .enumerate()
        .map(|(i, t)| TabInfo {
            id: t.row.id.clone(),
            index: i + 1,
            title: t.row.title.clone(),
            worktree: t.row.worktree.clone(),
            branch: t.row.branch.clone(),
            tmux_session: t.row.tmux_session.clone(),
            selected: sel.as_deref() == Some(&t.row.id),
            plan_file: t.row.plan_file(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A view checks its rev out beside the repo (once) and points the tab's
    /// viewer there; without a rev, back at the tab's own worktree.
    #[test]
    fn view_checks_out_a_rev_beside_the_repo() {
        let repo = crate::test_util::TestRepo::new("echo 1 > a");
        repo.sh("git checkout -qb pr && echo 2 > a && git commit -qam pr && git checkout -q main");
        let (_ctx, engine) = crate::test_util::engine("local", &repo.path(), &["t"]);
        let view = |rev: Option<&str>| View { tab: "t".into(), rev: rev.map(String::from), name: Some("kitchen-pr-7".into()), fetch: vec![], base: Some("main".into()) };
        let shown = |e: &Engine| {
            e.take_ui_commands().into_iter().find_map(|c| match c {
                UiCommand::ResetViewer { tab_id, view } if tab_id == "t" => view,
                _ => None,
            })
        };
        engine.view(view(Some("pr"))).unwrap();
        let (wt, base) = shown(&engine).unwrap();
        assert!(wt.ends_with("-kitchen-pr-7") && base == "main", "{wt} {base}");
        assert_eq!(std::fs::read_to_string(format!("{wt}/a")).unwrap(), "2\n");
        engine.view(view(Some("pr"))).unwrap();
        assert_eq!(shown(&engine).unwrap().0, wt, "the same worktree again");
        engine.view(View { rev: None, name: None, base: None, ..view(None) }).unwrap();
        assert_eq!(shown(&engine), Some((repo.path(), "origin/main".into())));
        assert!(engine.view(View { name: None, ..view(Some("pr")) }).is_err(), "a rev needs a name");
        // The siblings (the view's, and the spare its checkout refilled).
        while engine.opening.lock().unwrap().iter().any(|k| k.starts_with("spares:")) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let prefix = format!("{}-", repo.dir.file_name().unwrap().to_string_lossy());
        for d in std::fs::read_dir(repo.dir.parent().unwrap()).unwrap().flatten() {
            if d.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_dir_all(d.path());
            }
        }
    }

    #[test]
    fn file_link_paths() {
        let home = std::env::var("HOME").unwrap();
        let wt = "/w/repo-x";
        for (path, want) in [
            ("aster/foo.py", format!("{wt}/aster/foo.py")),
            ("./aster/foo.py", format!("{wt}/aster/foo.py")),
            ("../other/a.rs", format!("{wt}/../other/a.rs")),
            ("/home/dev/agi/a.rs", "/home/dev/agi/a.rs".to_string()),
            ("~/x.lua", format!("{home}/x.lua")),
        ] {
            assert_eq!(file_link_path(&Host::Local, wt, path).unwrap(), want, "path={path:?}");
        }
    }

    #[test]
    fn link_search() {
        let paths = |ps: &[&str]| ps.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        assert_eq!(closest(&paths(&["a/b/x.md", "z/x.md", "c/x.md"])).unwrap(), "c/x.md");
        assert_eq!(closest(&[]), None);
        for p in ["x.md", "./a/x.md", "a/x.md"] {
            assert!(is_searchable(p), "{p:?}");
        }
        for p in ["/a/x", "~/x", "../x", "dir/"] {
            assert!(!is_searchable(p), "{p:?}");
        }
    }

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
