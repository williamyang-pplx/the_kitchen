//! The kitchen's state and every action on it.
//!
//! The engine is shared (`Arc<Engine>`) between the UI thread, the socket
//! API threads and background jobs. State sits behind short-lived locks;
//! slow work (git, ssh, gh) runs on the calling thread *without* holding a
//! lock, so a slow devbox never freezes the window. Keyboard shortcuts and
//! `kitchenctl` commands end up calling the same methods here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, bail};
use kitchen_core::db::{Db, PlanComment, TabRow, WorkspaceRow};
use kitchen_core::git::Repo;
use kitchen_core::host::Host;
use kitchen_core::tmux::{self, Tmux};
use kitchen_core::util::{new_id, shell_quote, slugify};
use kitchen_core::{gh, plan};
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
}

/// Requests for the UI thread (it owns terminals and viewer state).
#[derive(Debug, Clone)]
pub enum UiCommand {
    OpenViewer { tab_id: String, path: Option<String>, line: Option<usize>, diff: Option<bool> },
    OpenPlan { tab_id: String },
    OpenInbox,
    ReloadPlan { tab_id: String },
    FocusTerminal,
    Screenshot { path: String, reply: std::sync::mpsc::Sender<Result<(), String>> },
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

/// Options for a new tab (from Cmd+E/Cmd+R dialogs or `kitchenctl tab new`).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct NewTab {
    pub kind: Option<String>,
    /// Ticket / task description. Edit tabs start planning on it.
    pub task: Option<String>,
    pub pr: Option<u64>,
    pub branch: Option<String>,
    pub title: Option<String>,
    /// Command typed into the new shell instead of the default agent.
    pub command: Option<String>,
    /// Start no agent at all.
    pub no_agent: bool,
    pub focus: Option<bool>,
}

pub struct Engine {
    pub ctx: egui::Context,
    db: Mutex<Db>,
    state: Mutex<State>,
    ui_cmds: Mutex<Vec<UiCommand>>,
    settings: Mutex<Settings>,
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
            state.workspaces.push(Workspace { row, tabs, selected_tab, status: None, error: None });
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
        }))
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

    pub fn set_settings(&self, s: Settings) -> Result<()> {
        self.db().set_setting("settings", &serde_json::to_string(&s)?)?;
        *self.settings.lock().unwrap() = s;
        Ok(())
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
        Ok(())
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
        let settings = self.settings();
        let tab_id = new_id();
        let busy = format!("{} tab…", if kind == TabKind::Edit { "creating edit" } else { "opening read-only" });
        self.set_ws_status(ws_id, Some(&busy), None);
        let result = (|| -> Result<TabRow> {
            // Always start from what is on the remote right now.
            repo.fetch().context("git fetch origin")?;
            let base = match &ws.row.default_branch {
                Some(b) => b.clone(),
                None => repo.default_branch()?,
            };
            let pr = match (opts.pr, &ws.row.github_slug) {
                (Some(n), Some(slug)) => Some(gh::Gh::default().pr_view(slug, n)?),
                (Some(_), None) => bail!("workspace has no GitHub remote; cannot open a PR"),
                _ => None,
            };
            let (worktree, branch, title) = match kind {
                TabKind::Edit => {
                    let (wt, title) = if let Some(pr) = &pr {
                        (repo.add_worktree_existing_branch(&pr.head_ref, &repo.sibling_path(&format!("pr-{}", pr.number)))?, format!("#{} {}", pr.number, pr.title))
                    } else if let Some(b) = opts.branch.as_deref().filter(|b| !b.is_empty()) {
                        let slug = slugify(b.rsplit('/').next().unwrap_or(b), 40);
                        (repo.add_worktree_existing_branch(b, &unique_path(&host, &repo.sibling_path(&slug)))?, b.to_string())
                    } else {
                        let label = opts.title.clone().or(opts.task.clone()).unwrap_or_else(|| format!("tab-{tab_id}"));
                        let first = label.lines().next().unwrap_or(&label).to_string();
                        let slug = unique_slug(&repo, &slugify(&first, 40), &settings.branch_prefix, &tab_id);
                        let branch = format!("{}{slug}", settings.branch_prefix);
                        (repo.add_worktree_new_branch(&branch, &repo.sibling_path(&slug), &base)?, first)
                    };
                    (wt.path.clone(), wt.branch.clone(), title)
                }
                TabKind::Readonly => {
                    if let Some(pr) = &pr {
                        let path = repo.sibling_path(&format!("kitchen-review-{}", pr.number));
                        let rev = format!("refs/remotes/origin/kitchen-pr/{}", pr.number);
                        let spec = format!("pull/{}/head:{rev}", pr.number);
                        host.run_ok(&["git", "-C", &ws.row.repo_path, "fetch", "--quiet", "--force", "origin", &spec], None)?;
                        let wt = repo.sync_readonly_worktree(&path, &rev)?;
                        (wt.path, Some(pr.head_ref.clone()), format!("review #{} {}", pr.number, pr.title))
                    } else {
                        let wt = repo.sync_readonly_worktree(&repo.readonly_path(), &format!("origin/{base}"))?;
                        (wt.path, None, opts.title.clone().unwrap_or_else(|| format!("read-only {base}")))
                    }
                }
            };
            let title = opts.title.clone().unwrap_or(title);
            plan::ensure_excluded(&host, &worktree)?;
            let plan_file = (kind == TabKind::Edit).then(|| plan::plan_file_for(&worktree));

            let session = tmux::session_name(ws_id, &tab_id);
            let mut env = vec![
                (kitchen_proto::ENV_WORKSPACE.to_string(), ws_id.to_string()),
                (kitchen_proto::ENV_TAB.to_string(), tab_id.clone()),
                (kitchen_proto::ENV_TAB_KIND.to_string(), kind.as_str().to_string()),
            ];
            if kind == TabKind::Readonly {
                env.push((kitchen_proto::ENV_READONLY.to_string(), "1".into()));
            }
            if let Some(p) = &plan_file {
                env.push((kitchen_proto::ENV_PLAN_FILE.to_string(), p.clone()));
            }
            let command = if opts.no_agent {
                None
            } else if let Some(c) = opts.command.clone() {
                Some(c)
            } else {
                match kind {
                    TabKind::Edit => Some(self.edit_agent_command(&host, &worktree, &settings, opts.task.as_deref(), plan_file.as_deref())?),
                    TabKind::Readonly => settings.autostart_readonly_agent.then(|| settings.readonly_agent_command.clone()),
                }
            };
            Tmux::new(&host).new_session(&session, &worktree, &env, command.as_deref())?;

            let row = TabRow {
                id: tab_id.clone(),
                workspace_id: ws_id.to_string(),
                kind: kind.as_str().to_string(),
                title,
                worktree,
                branch,
                pr_number: pr.as_ref().map(|p| p.number as i64),
                tmux_session: session,
                plan_file,
                position: ws.tabs.iter().map(|t| t.row.position).max().unwrap_or(0) + 1,
            };
            self.db().insert_tab(&row)?;
            Ok(row)
        })();
        self.set_ws_status(ws_id, None, None);
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

    /// The shell command that starts an edit tab's agent. With a task, the
    /// agent begins in the planning phase (`/low-level-plan`).
    fn edit_agent_command(
        &self,
        host: &Host,
        worktree: &str,
        settings: &Settings,
        task: Option<&str>,
        plan_file: Option<&str>,
    ) -> Result<String> {
        let base = settings.edit_agent_command.clone();
        // Plan mode writes into .kitchen/plans so the plan reviewer sees it.
        let plans_settings = r#"{"plansDirectory":".kitchen/plans"}"#;
        let base = format!("{base} --settings {}", shell_quote(plans_settings));
        let Some(task) = task.map(str::trim).filter(|t| !t.is_empty()) else {
            return Ok(base);
        };
        let plan_file = plan_file.context("edit tab without plan file")?;
        let prompt_path = format!("{worktree}/{}/prompt.md", plan::KITCHEN_DIR);
        host.write_file(&prompt_path, &plan::planning_prompt(task, plan_file))?;
        Ok(format!("{base} \"$(cat {})\"", shell_quote(&prompt_path)))
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
        let (ws, tab) = self.find_tab(tab_id).context("no such tab")?;
        let host = ws.host();
        let t = Tmux::new(&host);
        if t.has_session(&tab.row.tmux_session) {
            return Ok(true);
        }
        if !host.exists(&tab.row.worktree) {
            return Ok(false);
        }
        let mut env = vec![
            (kitchen_proto::ENV_WORKSPACE.to_string(), ws.row.id.clone()),
            (kitchen_proto::ENV_TAB.to_string(), tab.row.id.clone()),
            (kitchen_proto::ENV_TAB_KIND.to_string(), tab.row.kind.clone()),
        ];
        if tab.kind() == TabKind::Readonly {
            env.push((kitchen_proto::ENV_READONLY.to_string(), "1".into()));
        }
        if let Some(p) = &tab.row.plan_file {
            env.push((kitchen_proto::ENV_PLAN_FILE.to_string(), p.clone()));
        }
        t.new_session(&tab.row.tmux_session, &tab.row.worktree, &env, None)?;
        Ok(true)
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
            pr_number: t.row.pr_number.map(|n| n as u64),
            tmux_session: t.row.tmux_session.clone(),
            selected: sel.as_deref() == Some(&t.row.id),
            plan_file: t.row.plan_file.clone(),
            unread: t.unread,
        })
        .collect()
}

/// `slug`, or `slug-2`, `slug-3`… — the first whose worktree dir and branch
/// are both free.
fn unique_slug(repo: &Repo, slug: &str, prefix: &str, fallback: &str) -> String {
    let slug = if slug.is_empty() { format!("tab-{fallback}") } else { slug.to_string() };
    let free = |s: &str| !repo.host.exists(&repo.sibling_path(s)) && !repo.local_branch_exists(&format!("{prefix}{s}"));
    if free(&slug) {
        return slug;
    }
    (2..100).map(|n| format!("{slug}-{n}")).find(|s| free(s)).unwrap_or_else(|| format!("{slug}-{fallback}"))
}

/// `path`, or `path-2`, `path-3`… — the first that does not exist yet.
fn unique_path(host: &Host, path: &str) -> String {
    if !host.exists(path) {
        return path.to_string();
    }
    (2..100).map(|n| format!("{path}-{n}")).find(|p| !host.exists(p)).unwrap_or_else(|| format!("{path}-{}", new_id()))
}

fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Tab ids to their live terminal sessions — owned by the UI thread.
pub type Sessions = HashMap<String, crate::terminal::TermSession>;
