//! Command-line grammar and its mapping to socket API calls.
//!
//! Every subcommand maps to exactly one API method (see [`Cli::to_call`]),
//! except the few that run without the app: `guide`, `version`, and the
//! hidden `__relay` / `__index` / `__agent-stopped` / `__agent-session` helpers.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{Map, Value, json};

#[derive(Parser, Debug)]
#[command(
    name = "kitchenctl",
    version,
    about = "Control the kitchen (workspaces, tabs, worktrees, plans) from a terminal or an agent",
    long_about = "Control the kitchen from a terminal or an agent.\n\n\
        Inside a kitchen tab, commands default to the caller's workspace and tab \
        (from KITCHEN_WORKSPACE_ID / KITCHEN_TAB_ID). Handles accept an id, a 1-based \
        index (`2`, `workspace:2`, `tab:3`), or a name/title.\n\n\
        Run `kitchenctl guide` for the agent guide (works without the app running).",
    after_help = "Examples:\n  kitchenctl identify\n  kitchenctl tab new --task \"AI-123: add retries\"\n  \
        kitchenctl tab send --tab 2 --enter \"git status\"\n  kitchenctl plan comments\n  kitchenctl --json workspace list"
)]
pub struct Cli {
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    pub json: bool,
    /// Socket path (default: $KITCHEN_SOCKET_PATH or ~/.kitchen/kitchen.sock).
    #[arg(long, global = true, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Check that the kitchen app is reachable.
    Ping,
    /// Print the kitchenctl version (no app needed).
    Version,
    /// Show the calling workspace/tab and what the app has focused.
    #[command(after_help = "Example:\n  kitchenctl --json identify")]
    Identify,
    /// Print the agent guide as markdown (no app needed).
    Guide,
    /// Call a raw API method with JSON params.
    #[command(after_help = "Example:\n  kitchenctl rpc tab.list '{\"workspace\":\"1\"}'")]
    Rpc {
        method: String,
        /// JSON object of params.
        params: Option<String>,
    },
    /// Workspaces: one repo on this mac or on a devbox.
    #[command(subcommand)]
    Workspace(WorkspaceCmd),
    /// Terminal tabs, each in its own worktree.
    #[command(subcommand)]
    Tab(TabCmd),
    /// Git worktrees of a workspace's repo.
    #[command(subcommand)]
    Worktree(WorktreeCmd),
    /// The file viewer panel.
    #[command(subcommand)]
    Viewer(ViewerCmd),
    /// The planning phase of a tab.
    #[command(subcommand)]
    Plan(PlanCmd),
    /// Plugins: Lua in ~/.kitchen/plugins/<name>/ (see docs/plugins.md).
    #[command(subcommand)]
    Plugin(PluginCmd),
    /// Push diagnostics / highlights into the file viewer from any program.
    #[command(subcommand)]
    Decor(DecorCmd),
    /// Raise a notification (badges the tab, shows a macOS banner).
    #[command(after_help = "Example:\n  kitchenctl notify --title \"tests green\" --body \"ready for review\"")]
    Notify {
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        body: String,
        #[command(flatten)]
        target: TabTarget,
    },
    /// List recent notifications.
    Notifications {
        /// Only unread ones.
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Debugging helpers.
    #[command(subcommand)]
    Debug(DebugCmd),

    /// Devbox side of the API tunnel (started by the app over ssh).
    #[command(name = "__relay", hide = true)]
    Relay,
    /// Print symbols of a file as JSON (used by the viewer on devboxes).
    #[command(name = "__index", hide = true)]
    Index { paths: Vec<PathBuf> },
    /// Claude Code Stop hook (hook JSON on stdin): `notify`, unless background
    /// subagents are still running.
    #[command(name = "__agent-stopped", hide = true)]
    AgentStopped {
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        body: String,
    },
    /// Claude Code Stop hook (hook JSON on stdin): tell the app the tab's
    /// agent session, which a revived tab resumes.
    #[command(name = "__agent-session", hide = true)]
    AgentSession,
}

#[derive(Args, Debug, Default, Clone)]
pub struct WsTarget {
    /// Workspace handle (id, index, `workspace:N`, or name). Default: caller's, then selected.
    #[arg(long, short = 'w')]
    pub workspace: Option<String>,
}

#[derive(Args, Debug, Default, Clone)]
pub struct TabTarget {
    /// Workspace handle. Default: caller's, then selected.
    #[arg(long, short = 'w')]
    pub workspace: Option<String>,
    /// Tab handle (id, index, `tab:N`, or title). Default: caller's tab, then the selected tab.
    #[arg(long, short = 't')]
    pub tab: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum WorkspaceCmd {
    /// List workspaces and their tabs.
    #[command(visible_alias = "ls")]
    List,
    /// Print the caller's (or selected) workspace.
    Current,
    /// Create a workspace for a repo on this mac or a devbox.
    #[command(after_help = "Examples:\n  kitchenctl workspace new --repo ~/Documents/air\n  \
        kitchenctl workspace new --host oval-bear-fern --repo ~/air --name \"air @ devbox\"")]
    New {
        /// Repository path on the host (`~` expands on that host).
        #[arg(long)]
        repo: String,
        /// `local` (default) or an ssh alias from ~/.ssh/config (devboxes).
        #[arg(long, default_value = "local")]
        host: String,
        #[arg(long, default_value = "")]
        name: String,
    },
    /// Focus a workspace.
    Select { workspace: String },
    /// Focus the next workspace (⌘J).
    Next,
    /// Focus the previous workspace (⌘K).
    Prev,
    /// Rename a workspace.
    Rename {
        name: String,
        #[command(flatten)]
        target: WsTarget,
    },
    /// Remove a workspace (ends its tmux sessions; repo and worktrees stay).
    Close { workspace: String },
    /// Clear a workspace's unread dots (and mark its notifications read).
    Read {
        #[command(flatten)]
        target: WsTarget,
        /// Every workspace.
        #[arg(long, conflicts_with = "workspace")]
        all: bool,
    },
    /// Re-run host setup: pull dotfiles, link skills, install kitchenctl, start the relay.
    Bootstrap {
        #[command(flatten)]
        target: WsTarget,
    },
}

#[derive(Subcommand, Debug)]
pub enum TabCmd {
    /// List tabs of a workspace.
    #[command(visible_alias = "ls")]
    List {
        #[command(flatten)]
        target: WsTarget,
    },
    /// Open a tab in its own worktree, on a new branch off the latest origin
    /// or on --branch; with --task the agent starts planning (/low-level-plan).
    #[command(after_help = "Examples:\n  kitchenctl tab new --task \"AI-9586: retry judge calls\"\n  \
        kitchenctl tab new --branch william/old-work\n  kitchenctl tab new --no-agent")]
    New {
        /// Task/ticket text; the agent starts the planning phase with it.
        #[arg(long)]
        task: Option<String>,
        /// Read the task text from a file (`-` for stdin).
        #[arg(long, conflicts_with = "task", value_name = "FILE")]
        task_file: Option<String>,
        /// Existing branch to check out (fetched first; a tab already on it is reused).
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Command typed into the new shell instead of the default agent.
        #[arg(long)]
        command: Option<String>,
        /// Start a plain shell, no agent.
        #[arg(long)]
        no_agent: bool,
        /// Run in the workspace's main checkout: no worktree or branch (like ⌘I).
        #[arg(long, conflicts_with = "branch")]
        main_checkout: bool,
        /// Don't switch the window to the new tab.
        #[arg(long)]
        no_focus: bool,
        #[command(flatten)]
        target: WsTarget,
    },
    /// Focus a tab (like ⌘<N>).
    Select {
        tab: String,
        #[command(flatten)]
        target: WsTarget,
    },
    /// Rename a tab.
    Rename {
        title: String,
        #[command(flatten)]
        target: TabTarget,
    },
    /// Close a tab (ends its tmux session; the worktree stays).
    Close {
        #[command(flatten)]
        target: TabTarget,
    },
    /// Type text into a tab. `-` reads the text from stdin.
    #[command(after_help = "Examples:\n  kitchenctl tab send --tab 2 --enter \"make test\"\n  cat prompt.md | kitchenctl tab send --tab 3 --enter -")]
    Send {
        text: String,
        /// Press Return after the text.
        #[arg(long)]
        enter: bool,
        #[command(flatten)]
        target: TabTarget,
    },
    /// Press keys by tmux name (Enter, Escape, C-c, Up, …).
    #[command(name = "send-key", after_help = "Example:\n  kitchenctl tab send-key --tab 2 C-c")]
    SendKey {
        #[arg(required = true)]
        keys: Vec<String>,
        #[command(flatten)]
        target: TabTarget,
    },
    /// Print the tab's screen as text.
    #[command(name = "read-screen")]
    ReadScreen {
        /// Include this many lines of scrollback.
        #[arg(long)]
        scrollback: Option<u32>,
        #[command(flatten)]
        target: TabTarget,
    },
}

#[derive(Subcommand, Debug)]
pub enum PluginCmd {
    /// List plugins, their status and commands.
    #[command(visible_alias = "ls")]
    List,
    /// Reload one plugin (or all). Plugins also reload when their files change.
    Reload { name: Option<String> },
    /// Run a plugin command, e.g. `ruff.explain`.
    Run {
        command: String,
        #[command(flatten)]
        target: TabTarget,
    },
    /// Plugin timers (`kitchen.every`): interval, runs, last fired, next due, last stats.
    Timers,
    /// Print the end of a plugin's log (~/.kitchen/logs/plugins/<name>.log; no app needed).
    Logs {
        name: String,
        #[arg(long, short = 'n', default_value_t = 40)]
        lines: usize,
    },
}

#[derive(Subcommand, Debug)]
pub enum DecorCmd {
    /// Replace the diagnostics (or highlights) of namespace `ext.<ns>` for a file.
    #[command(after_help = "Items are JSON: [{\"line\":3,\"col\":5,\"end_col\":9,\"severity\":\"error\",\"message\":\"…\"}]\n(lines/cols 1-based, cols in characters, end_col exclusive).\nExample:\n  mypy --output json app.py | my-convert | kitchenctl decor set --ns mypy app.py --items -")]
    Set {
        /// Worktree-relative (or absolute) path.
        path: String,
        #[arg(long)]
        ns: String,
        /// JSON list, or `-` for stdin.
        #[arg(long)]
        items: String,
        /// `diagnostics` (default) or `highlights`.
        #[arg(long)]
        kind: Option<String>,
        /// Content hash the items were computed for (hidden once the file changes).
        #[arg(long)]
        version: Option<String>,
        /// Default: the calling tab's host.
        #[arg(long)]
        host: Option<String>,
        /// Default: the calling tab's worktree.
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Clear namespace `ext.<ns>` (for one file, or everywhere).
    Clear {
        path: Option<String>,
        #[arg(long)]
        ns: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        worktree: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum WorktreeCmd {
    /// List worktrees of the workspace's repo and which tab uses each.
    #[command(visible_alias = "ls")]
    List {
        /// Also report uncommitted changes (runs `git status` in each worktree).
        #[arg(long)]
        status: bool,
        #[command(flatten)]
        target: WsTarget,
    },
    /// Remove a worktree. Refuses dirty worktrees without --force, the main
    /// checkout, and worktrees an open tab uses.
    Remove {
        path: String,
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        target: WsTarget,
    },
}

#[derive(Subcommand, Debug)]
pub enum ViewerCmd {
    /// Open a file in the tab's viewer panel.
    #[command(after_help = "Example:\n  kitchenctl viewer open src/lib.rs --line 42 --diff")]
    Open {
        path: Option<String>,
        #[arg(long)]
        line: Option<usize>,
        /// Show the file as a diff against the branch base (merge base with origin).
        #[arg(long)]
        diff: bool,
        #[command(flatten)]
        target: TabTarget,
    },
}

#[derive(Subcommand, Debug)]
pub enum PlanCmd {
    /// Print the tab's plan file path.
    Path {
        #[command(flatten)]
        target: TabTarget,
    },
    /// Show the plan in the plan reviewer (⌘⇧I).
    Open {
        #[command(flatten)]
        target: TabTarget,
    },
    /// Tell William a plan revision is ready (reloads the reviewer, notifies).
    Ready {
        #[command(flatten)]
        target: TabTarget,
    },
    /// List William's comments on the plan (unresolved by default).
    Comments {
        /// Include resolved comments.
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        target: TabTarget,
    },
    /// Mark a plan comment as addressed.
    Resolve { id: i64 },
}

#[derive(Subcommand, Debug)]
pub enum DebugCmd {
    /// Save a PNG of the kitchen window.
    Screenshot { path: String },
    /// Type into the focused pane: plain characters, plus <esc> <cr> <c-o> <cmd-shift-p> <alt-h>.
    #[command(after_help = "Example:\n  kitchenctl debug keys 'gd'\n  kitchenctl debug keys '<cmd-p>main<cr>'")]
    Keys { keys: String },
}

/// cmux-style aliases that clap cannot express: `kitchenctl --skill` = `guide`.
pub fn normalize_args(mut argv: Vec<String>) -> Vec<String> {
    if argv.get(1).map(String::as_str) == Some("--skill") {
        argv[1] = "guide".into();
    }
    argv
}

/// What a command needs done.
#[derive(Debug, PartialEq)]
pub enum Call {
    Api { method: String, params: Value },
    PluginLogs { name: String, lines: usize },
    Guide,
    Version,
    Relay,
    Index(Vec<PathBuf>),
    /// Nothing to do (exit 0).
    Skip,
}

fn api(method: &str, params: Value) -> Call {
    Call::Api { method: method.to_string(), params: strip_nulls(params) }
}

/// Drop `null` fields so the server applies its defaults.
fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).collect::<Map<_, _>>()),
        other => other,
    }
}

fn ws(t: &WsTarget) -> Value {
    json!({ "workspace": t.workspace })
}

fn tab(t: &TabTarget) -> Value {
    json!({ "workspace": t.workspace, "tab": t.tab })
}

fn merge(mut base: Value, extra: Value) -> Value {
    if let (Value::Object(b), Value::Object(e)) = (&mut base, extra) {
        b.extend(e);
    }
    base
}

/// Read `-` as stdin, anything else as a file path.
fn read_text(src: &str, stdin: &mut dyn FnMut() -> Result<String>) -> Result<String> {
    if src == "-" { stdin() } else { std::fs::read_to_string(src).with_context(|| format!("read {src}")) }
}

impl Cli {
    pub fn to_call(&self) -> Result<Call> {
        self.to_call_with(&mut || {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
            Ok(s)
        })
    }

    /// Build the API call; `stdin` supplies text for `-` arguments.
    pub fn to_call_with(&self, stdin: &mut dyn FnMut() -> Result<String>) -> Result<Call> {
        Ok(match &self.command {
            Command::Ping => api("system.ping", json!({})),
            Command::Version => Call::Version,
            Command::Identify => api("system.identify", json!({})),
            Command::Guide => Call::Guide,
            Command::Rpc { method, params } => {
                let params = match params {
                    Some(p) => serde_json::from_str(p).context("params must be JSON")?,
                    None => json!({}),
                };
                Call::Api { method: method.clone(), params }
            }
            Command::Workspace(c) => match c {
                WorkspaceCmd::List => api("workspace.list", json!({})),
                WorkspaceCmd::Current => api("workspace.current", json!({})),
                WorkspaceCmd::New { repo, host, name } => api("workspace.create", json!({"repo": repo, "host": host, "name": name})),
                WorkspaceCmd::Select { workspace } => api("workspace.select", json!({"workspace": workspace})),
                WorkspaceCmd::Next => api("workspace.next", json!({})),
                WorkspaceCmd::Prev => api("workspace.prev", json!({})),
                WorkspaceCmd::Rename { name, target } => api("workspace.rename", merge(ws(target), json!({"name": name}))),
                WorkspaceCmd::Close { workspace } => api("workspace.close", json!({"workspace": workspace})),
                WorkspaceCmd::Read { target, all } => api("workspace.mark_read", merge(ws(target), json!({"all": all}))),
                WorkspaceCmd::Bootstrap { target } => api("workspace.bootstrap", ws(target)),
            },
            Command::Tab(c) => match c {
                TabCmd::List { target } => api("tab.list", ws(target)),
                TabCmd::New { task, task_file, branch, title, command, no_agent, main_checkout, no_focus, target } => {
                    let task = match task_file {
                        Some(f) => Some(read_text(f, stdin)?),
                        None => task.clone(),
                    };
                    api(
                        "tab.create",
                        merge(
                            ws(target),
                            json!({
                                "task": task, "branch": branch, "title": title, "command": command,
                                "no_agent": no_agent, "main_checkout": main_checkout, "focus": !no_focus,
                            }),
                        ),
                    )
                }
                TabCmd::Select { tab: t, target } => api("tab.select", merge(ws(target), json!({"tab": t}))),
                TabCmd::Rename { title, target } => api("tab.rename", merge(tab(target), json!({"title": title}))),
                TabCmd::Close { target } => api("tab.close", tab(target)),
                TabCmd::Send { text, enter, target } => {
                    let text = if text == "-" { stdin()? } else { text.clone() };
                    api("tab.send", merge(tab(target), json!({"text": text, "enter": enter})))
                }
                TabCmd::SendKey { keys, target } => api("tab.send_key", merge(tab(target), json!({"keys": keys}))),
                TabCmd::ReadScreen { scrollback, target } => {
                    api("tab.read_screen", merge(tab(target), json!({"scrollback": scrollback})))
                }
            },
            Command::Worktree(c) => match c {
                WorktreeCmd::List { status, target } => api("worktree.list", merge(ws(target), json!({"status": status}))),
                WorktreeCmd::Remove { path, force, target } => {
                    api("worktree.remove", merge(ws(target), json!({"path": path, "force": force})))
                }
            },
            Command::Viewer(ViewerCmd::Open { path, line, diff, target }) => api(
                "viewer.open",
                merge(tab(target), json!({"path": path, "line": line, "diff": if *diff { Some(true) } else { None }})),
            ),
            Command::Plan(c) => match c {
                PlanCmd::Path { target } => api("plan.path", tab(target)),
                PlanCmd::Open { target } => api("plan.open", tab(target)),
                PlanCmd::Ready { target } => api("plan.ready", tab(target)),
                PlanCmd::Comments { all, target } => api("plan.comments", merge(tab(target), json!({"all": all}))),
                PlanCmd::Resolve { id } => api("plan.resolve", json!({"id": id})),
            },
            Command::Plugin(c) => match c {
                PluginCmd::List => api("plugin.list", json!({})),
                PluginCmd::Timers => api("plugin.timers", json!({})),
                PluginCmd::Reload { name } => api("plugin.reload", json!({"name": name})),
                PluginCmd::Run { command, target } => api("plugin.run", merge(tab(target), json!({"command": command}))),
                PluginCmd::Logs { name, lines } => Call::PluginLogs { name: name.clone(), lines: *lines },
            },
            Command::Decor(c) => match c {
                DecorCmd::Set { path, ns, items, kind, version, host, worktree } => {
                    let items: Value = serde_json::from_str(&read_text(items, stdin)?).context("--items must be JSON")?;
                    api("decor.set", json!({"path": path, "ns": ns, "items": items, "kind": kind, "version": version, "host": host, "worktree": worktree}))
                }
                DecorCmd::Clear { path, ns, kind, host, worktree } => {
                    api("decor.clear", json!({"path": path, "ns": ns, "kind": kind, "host": host, "worktree": worktree}))
                }
            },
            Command::Notify { title, body, target } => api("notify", merge(tab(target), json!({"title": title, "body": body}))),
            Command::AgentStopped { title, body } => {
                if crate::hook::should_notify_on_stop(&stdin()?) {
                    api("notify", json!({"title": title, "body": body}))
                } else {
                    Call::Skip
                }
            }
            Command::AgentSession => match crate::hook::session_id(&stdin()?) {
                Some(id) => api("tab.agent_session", json!({"session_id": id})),
                None => Call::Skip,
            },
            Command::Notifications { unread, limit } => api("notification.list", json!({"unread": unread, "limit": limit})),
            Command::Debug(DebugCmd::Screenshot { path }) => {
                let abs = std::path::absolute(path).unwrap_or_else(|_| PathBuf::from(path));
                api("debug.screenshot", json!({"path": abs}))
            }
            Command::Debug(DebugCmd::Keys { keys }) => api("debug.keys", json!({"keys": keys})),
            Command::Relay => Call::Relay,
            Command::Index { paths } => Call::Index(paths.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(args: &[&str]) -> Call {
        let mut argv = vec!["kitchenctl"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).unwrap().to_call_with(&mut || Ok("from stdin".into())).unwrap()
    }

    fn api_call(args: &[&str]) -> (String, Value) {
        match call(args) {
            Call::Api { method, params } => (method, params),
            other => panic!("expected api call, got {other:?}"),
        }
    }

    #[test]
    fn verifies_clap_definition() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn tab_new_with_task() {
        let (m, p) = api_call(&["tab", "new", "--task", "AI-1: x", "--branch", "b", "-w", "2"]);
        assert_eq!(m, "tab.create");
        assert_eq!(p, json!({"workspace": "2", "task": "AI-1: x", "branch": "b", "no_agent": false, "main_checkout": false, "focus": true}));
    }

    #[test]
    fn tab_new_no_agent_no_focus() {
        let (_, p) = api_call(&["tab", "new", "--no-agent", "--no-focus"]);
        assert_eq!(p["focus"], false);
        assert_eq!(p["no_agent"], true);
    }

    #[test]
    fn tab_new_main_checkout() {
        let (_, p) = api_call(&["tab", "new", "--main-checkout"]);
        assert_eq!(p["main_checkout"], true);
        assert!(Cli::try_parse_from(["kitchenctl", "tab", "new", "--main-checkout", "--branch", "b"]).is_err(), "a main-checkout tab has no branch");
    }

    #[test]
    fn workspace_read() {
        assert_eq!(api_call(&["workspace", "read", "-w", "2"]), ("workspace.mark_read".into(), json!({"workspace": "2", "all": false})));
        assert_eq!(api_call(&["workspace", "read", "--all"]).1, json!({"all": true}));
        assert!(Cli::try_parse_from(["kitchenctl", "workspace", "read", "--all", "-w", "2"]).is_err());
    }

    #[test]
    fn send_reads_stdin_for_dash() {
        let (m, p) = api_call(&["tab", "send", "-t", "tab:3", "--enter", "-"]);
        assert_eq!(m, "tab.send");
        assert_eq!(p, json!({"tab": "tab:3", "text": "from stdin", "enter": true}));
    }

    #[test]
    fn send_key_and_screen() {
        assert_eq!(api_call(&["tab", "send-key", "C-c", "Enter"]).1, json!({"keys": ["C-c", "Enter"]}));
        assert_eq!(api_call(&["tab", "read-screen", "--scrollback", "100"]).1, json!({"scrollback": 100}));
    }

    #[test]
    fn workspace_new_defaults_local() {
        let (m, p) = api_call(&["workspace", "new", "--repo", "~/air"]);
        assert_eq!(m, "workspace.create");
        assert_eq!(p, json!({"repo": "~/air", "host": "local", "name": ""}));
    }

    #[test]
    fn plan_commands() {
        assert_eq!(api_call(&["plan", "resolve", "9"]), ("plan.resolve".into(), json!({"id": 9})));
        assert_eq!(api_call(&["plan", "comments", "--all"]).1, json!({"all": true}));
    }

    #[test]
    fn rpc_passthrough_and_local_commands() {
        assert_eq!(api_call(&["rpc", "tab.list", "{\"workspace\":\"1\"}"]), ("tab.list".into(), json!({"workspace": "1"})));
        assert_eq!(call(&["guide"]), Call::Guide);
        let argv = normalize_args(vec!["kitchenctl".into(), "--skill".into()]);
        assert_eq!(Cli::try_parse_from(argv).unwrap().to_call().unwrap(), Call::Guide);
        assert_eq!(call(&["__relay"]), Call::Relay);
        assert!(Cli::try_parse_from(["kitchenctl", "--json", "ping"]).unwrap().json);
        assert!(Cli::try_parse_from(["kitchenctl", "ping", "--json"]).unwrap().json, "--json is global");
    }
}
