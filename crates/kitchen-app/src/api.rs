//! The socket API behind `kitchenctl`. Newline-delimited JSON
//! ([`kitchen_proto::Request`] / [`kitchen_proto::Response`]).
//!
//! Every human keyboard action has an API method here.

use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use kitchen_proto::{Caller, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::engine::{Engine, NewTab, UiCommand};

/// Bind the local socket and serve forever on a background thread.
pub fn serve(engine: Arc<Engine>) -> Result<()> {
    let path = kitchen_proto::default_socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        // A live app answers; a stale socket file refuses the connection.
        if UnixStream::connect(&path).is_ok() {
            bail!("another kitchen is already listening on {}", path.display());
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::thread::Builder::new().name("api-accept".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let engine = engine.clone();
            let _ = std::thread::Builder::new().name("api-conn".into()).spawn(move || serve_conn(&engine, stream));
        }
    })?;
    Ok(())
}

fn serve_conn(engine: &Arc<Engine>, stream: UnixStream) {
    let Ok(read_half) = stream.try_clone() else { return };
    let mut writer = stream;
    for line in BufReader::new(read_half).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let resp = handle_line(engine, &line);
        if kitchen_proto::write_line(&mut writer, &resp).is_err() {
            break;
        }
    }
}

/// Parse and answer one request line from the local socket.
pub fn handle_line(engine: &Arc<Engine>, line: &str) -> Response {
    handle_line_from(engine, line, false)
}

/// Parse and answer one request line. `remote`: it came through a devbox
/// relay, so plugin methods are restricted (see [`plugin_dispatch`]).
pub fn handle_line_from(engine: &Arc<Engine>, line: &str, remote: bool) -> Response {
    match serde_json::from_str::<Request>(line) {
        Ok(req) => {
            let id = req.id;
            let result = if req.method.starts_with("plugin.") || req.method.starts_with("decor.") {
                plugin_dispatch(engine, &req.method, req.params, &req.caller, remote)
            } else if remote && req.method == "debug.keys" {
                Err(anyhow!("debug.keys can't be called from a devbox"))
            } else {
                dispatch(engine, &req.method, req.params, &req.caller)
            };
            match result {
                Ok(v) => Response::ok(id, v),
                Err(e) => Response::err(id, format!("{e:#}")),
            }
        }
        Err(e) => Response::err(0, format!("bad request: {e}")),
    }
}

fn p<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|e| anyhow!("bad params: {e}"))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Target {
    workspace: Option<String>,
    tab: Option<String>,
}

/// `plugin.*` and `decor.*`. Socket pushes always land in `ext.<ns>` so they
/// can't overwrite a plugin's own sets; devbox callers may not reload
/// plugins and may only run commands marked `remote = true`.
fn plugin_dispatch(engine: &Arc<Engine>, method: &str, params: Value, caller: &Caller, remote: bool) -> Result<Value> {
    use crate::plugins::decor::{self, FileKey, Kind};
    let host = engine.plugins().cloned().ok_or_else(|| anyhow!("plugins are not running"))?;
    #[derive(Deserialize)]
    struct DecorParams {
        ns: String,
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        worktree: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        items: Value,
        #[serde(default)]
        version: Option<String>,
    }
    // A file from params, defaulting host/worktree to the calling tab's.
    let file = |d: &DecorParams| -> Result<Option<FileKey>> {
        let Some(path) = d.path.clone() else { return Ok(None) };
        let tab = caller.tab_id.as_deref().and_then(|t| engine.find_tab(t));
        let host = d.host.clone().or_else(|| tab.as_ref().map(|(w, _)| w.host().label().to_string())).ok_or_else(|| anyhow!("host is required outside a kitchen tab"))?;
        let worktree = d.worktree.clone().or_else(|| tab.as_ref().map(|(_, t)| t.row.worktree.clone())).ok_or_else(|| anyhow!("worktree is required outside a kitchen tab"))?;
        let path = path.strip_prefix(&format!("{}/", worktree.trim_end_matches('/'))).map(String::from).unwrap_or(path);
        Ok(Some(FileKey { host, worktree, path }))
    };
    let ext = |ns: &str| format!("ext.{}", ns.trim_start_matches("ext."));
    match method {
        "plugin.list" => Ok(serde_json::to_value(host.list())?),
        "plugin.timers" => Ok(serde_json::to_value(host.timers_info())?),
        "plugin.reload" => {
            if remote {
                bail!("plugin.reload is only allowed from this mac");
            }
            #[derive(Deserialize, Default)]
            struct P {
                #[serde(default)]
                name: Option<String>,
            }
            let P { name } = p(params)?;
            Ok(json!({ "reloaded": host.reload(name.as_deref())? }))
        }
        "plugin.run" => {
            #[derive(Deserialize)]
            struct P {
                command: String,
            }
            let P { command } = p(params)?;
            let tab = caller.tab_id.clone().or_else(|| engine.selected_workspace().and_then(|w| w.selected().map(|t| t.row.id.clone()))).unwrap_or_default();
            host.invoke(&command, crate::plugins::command_context(engine, &tab, None), remote)?;
            Ok(json!({ "started": command }))
        }
        "decor.set" => {
            let d: DecorParams = p(params)?;
            let key = file(&d)?.ok_or_else(|| anyhow!("path is required"))?;
            let ns = ext(&d.ns);
            let items = match d.items {
                Value::Null => Value::Array(Vec::new()),
                v => v,
            };
            match d.kind.as_deref().map(Kind::parse) {
                None | Some(Some(Kind::Diagnostics)) => host.decor.set_diagnostics(&key, &ns, decor::parse_diagnostics(items).map_err(|e| anyhow!(e))?, d.version),
                Some(Some(Kind::Highlights)) => host.decor.set_highlights(&key, &ns, decor::parse_highlights(items).map_err(|e| anyhow!(e))?, d.version),
                Some(Some(Kind::Comments)) => host.decor.set_comments(&key, &ns, decor::parse_comments(items).map_err(|e| anyhow!(e))?),
                Some(Some(Kind::Render)) => host.decor.set_render(&key, &ns, decor::parse_render(items).map_err(|e| anyhow!(e))?, d.version),
                Some(None) => bail!("kind is diagnostics, highlights, comments or render"),
            }
            engine.ctx.request_repaint();
            Ok(json!({ "ns": ns, "path": key.path }))
        }
        "decor.clear" => {
            let d: DecorParams = p(params)?;
            let key = file(&d)?;
            let kind = match d.kind.as_deref() {
                None => None,
                Some(k) => Some(Kind::parse(k).ok_or_else(|| anyhow!("kind is diagnostics, highlights, comments or render"))?),
            };
            host.decor.clear(key.as_ref(), &ext(&d.ns), kind);
            engine.ctx.request_repaint();
            Ok(json!({ "ns": ext(&d.ns) }))
        }
        _ => bail!("unknown method {method}"),
    }
}

pub fn dispatch(engine: &Arc<Engine>, method: &str, params: Value, caller: &Caller) -> Result<Value> {
    match method {
        // ------------------------------------------------------------ system
        "system.ping" => Ok(json!({"pong": true, "version": env!("CARGO_PKG_VERSION"), "protocol": kitchen_proto::PROTOCOL_VERSION})),
        "system.identify" => {
            let ws = caller.workspace_id.as_deref().and_then(|id| engine.workspace(id));
            let tab = caller.tab_id.as_deref().and_then(|id| engine.find_tab(id)).map(|(_, t)| t);
            let selected = engine.selected_workspace();
            Ok(json!({
                "caller": {
                    "workspace": ws.as_ref().map(|w| json!({"id": w.row.id, "name": w.row.name, "host": w.row.host, "repo_path": w.row.repo_path})),
                    "tab": tab.as_ref().map(|t| json!({"id": t.row.id, "title": t.row.title, "worktree": t.row.worktree, "branch": t.row.branch, "plan_file": t.row.plan_file()})),
                },
                "focused": {
                    "workspace": selected.as_ref().map(|w| w.row.id.clone()),
                    "tab": selected.as_ref().and_then(|w| w.selected().map(|t| t.row.id.clone())),
                },
                "socket_path": kitchen_proto::default_socket_path(),
            }))
        }

        // ------------------------------------------------------------ workspaces
        "workspace.list" => Ok(serde_json::to_value(engine.workspace_info())?),
        "workspace.current" => {
            let id = engine.resolve_workspace(None, caller)?;
            let info = engine.workspace_info().into_iter().find(|w| w.id == id);
            Ok(serde_json::to_value(info)?)
        }
        "workspace.create" => {
            #[derive(Deserialize)]
            struct P {
                #[serde(default)]
                name: String,
                #[serde(default)]
                host: String,
                repo: String,
            }
            let a: P = p(params)?;
            let row = engine.create_workspace(&a.name, &a.host, &a.repo)?;
            Ok(json!({"id": row.id, "name": row.name, "host": row.host, "repo_path": row.repo_path}))
        }
        "workspace.select" => {
            let t: Target = p(params)?;
            let id = engine.resolve_workspace(t.workspace.as_deref(), caller)?;
            engine.select_workspace(&id);
            Ok(json!({"id": id}))
        }
        "workspace.next" | "workspace.prev" => {
            engine.cycle_workspace(if method == "workspace.next" { 1 } else { -1 });
            Ok(json!({"id": engine.selected_workspace().map(|w| w.row.id)}))
        }
        "workspace.rename" => {
            #[derive(Deserialize)]
            struct P {
                workspace: Option<String>,
                name: String,
            }
            let a: P = p(params)?;
            let id = engine.resolve_workspace(a.workspace.as_deref(), caller)?;
            engine.rename_workspace(&id, &a.name)?;
            Ok(json!({"id": id}))
        }
        "workspace.close" => {
            let t: Target = p(params)?;
            let id = engine.resolve_workspace(t.workspace.as_deref(), caller)?;
            engine.close_workspace(&id)?;
            Ok(json!({"closed": id}))
        }
        "workspace.mark_read" => {
            #[derive(Deserialize)]
            struct P {
                workspace: Option<String>,
                #[serde(default)]
                all: bool,
            }
            let a: P = p(params)?;
            if a.all {
                engine.mark_read(None)?;
                return Ok(json!({"all": true}));
            }
            let id = engine.resolve_workspace(a.workspace.as_deref(), caller)?;
            engine.mark_read(Some(&id))?;
            Ok(json!({"id": id}))
        }
        "workspace.bootstrap" => {
            let t: Target = p(params)?;
            let id = engine.resolve_workspace(t.workspace.as_deref(), caller)?;
            engine.bootstrap_workspace(&id)?;
            Ok(json!({"id": id}))
        }

        // ------------------------------------------------------------ tabs
        "tab.list" => {
            let t: Target = p(params)?;
            let id = engine.resolve_workspace(t.workspace.as_deref(), caller)?;
            let info = engine.workspace_info().into_iter().find(|w| w.id == id).context("workspace vanished")?;
            Ok(serde_json::to_value(info.tabs)?)
        }
        "tab.create" => {
            #[derive(Deserialize)]
            struct P {
                workspace: Option<String>,
                #[serde(flatten)]
                opts: NewTab,
            }
            let a: P = p(params)?;
            let ws = engine.resolve_workspace(a.workspace.as_deref(), caller)?;
            let row = engine.create_tab(&ws, a.opts)?;
            Ok(json!({"id": row.id, "workspace": ws, "title": row.title, "worktree": row.worktree,
                      "branch": row.branch, "tmux_session": row.tmux_session, "plan_file": row.plan_file()}))
        }
        "tab.agent_session" => {
            #[derive(Deserialize)]
            struct P {
                session_id: String,
            }
            let a: P = p(params)?;
            // Only the tab's own agent reports (its hook runs in the tab): no
            // falling back to the selected tab.
            let tab = caller.tab_id.as_deref().context("not called from a kitchen tab")?;
            engine.set_agent_session(tab, &a.session_id)?;
            Ok(json!({"tab": tab}))
        }
        "tab.select" => {
            let t: Target = p(params)?;
            let id = engine.resolve_tab(t.tab.as_deref(), t.workspace.as_deref(), caller)?;
            engine.select_tab(&id);
            Ok(json!({"id": id}))
        }
        "tab.rename" => {
            #[derive(Deserialize)]
            struct P {
                workspace: Option<String>,
                tab: Option<String>,
                title: String,
            }
            let a: P = p(params)?;
            let id = engine.resolve_tab(a.tab.as_deref(), a.workspace.as_deref(), caller)?;
            engine.rename_tab(&id, &a.title)?;
            Ok(json!({"id": id}))
        }
        "tab.close" => {
            let t: Target = p(params)?;
            let id = engine.resolve_tab(t.tab.as_deref(), t.workspace.as_deref(), caller)?;
            engine.close_tab(&id)?;
            Ok(json!({"closed": id}))
        }
        "tab.send" => {
            #[derive(Deserialize)]
            struct P {
                workspace: Option<String>,
                tab: Option<String>,
                text: String,
                #[serde(default)]
                enter: bool,
            }
            let a: P = p(params)?;
            let id = engine.resolve_tab(a.tab.as_deref(), a.workspace.as_deref(), caller)?;
            engine.send_text(&id, &a.text, a.enter)?;
            Ok(json!({"id": id}))
        }
        "tab.send_key" => {
            #[derive(Deserialize)]
            struct P {
                workspace: Option<String>,
                tab: Option<String>,
                keys: Vec<String>,
            }
            let a: P = p(params)?;
            let id = engine.resolve_tab(a.tab.as_deref(), a.workspace.as_deref(), caller)?;
            let keys: Vec<&str> = a.keys.iter().map(|s| s.as_str()).collect();
            engine.send_keys(&id, &keys)?;
            Ok(json!({"id": id}))
        }
        "tab.read_screen" => {
            #[derive(Deserialize)]
            struct P {
                workspace: Option<String>,
                tab: Option<String>,
                scrollback: Option<u32>,
            }
            let a: P = p(params)?;
            let id = engine.resolve_tab(a.tab.as_deref(), a.workspace.as_deref(), caller)?;
            Ok(json!({"id": id, "text": engine.read_screen(&id, a.scrollback)?}))
        }

        // ------------------------------------------------------------ viewer / plan (UI)
        "viewer.open" => {
            #[derive(Deserialize)]
            struct P {
                workspace: Option<String>,
                tab: Option<String>,
                path: Option<String>,
                line: Option<usize>,
                diff: Option<bool>,
            }
            let a: P = p(params)?;
            let id = engine.resolve_tab(a.tab.as_deref(), a.workspace.as_deref(), caller)?;
            engine.select_tab(&id);
            engine.push_ui(UiCommand::OpenViewer { tab_id: id.clone(), path: a.path, line: a.line, diff: a.diff });
            Ok(json!({"id": id}))
        }
        "plan.path" => {
            let t: Target = p(params)?;
            let id = engine.resolve_tab(t.tab.as_deref(), t.workspace.as_deref(), caller)?;
            let (_, tab) = engine.find_tab(&id).context("no such tab")?;
            Ok(json!({"tab": id, "plan_file": tab.row.plan_file()}))
        }
        "plan.open" => {
            let t: Target = p(params)?;
            let id = engine.resolve_tab(t.tab.as_deref(), t.workspace.as_deref(), caller)?;
            engine.select_tab(&id);
            engine.push_ui(UiCommand::OpenPlan { tab_id: id.clone() });
            Ok(json!({"tab": id}))
        }
        "plan.ready" => {
            let t: Target = p(params)?;
            let id = engine.resolve_tab(t.tab.as_deref(), t.workspace.as_deref(), caller)?;
            let (ws, tab) = engine.find_tab(&id).context("no such tab")?;
            engine.push_ui(UiCommand::ReloadPlan { tab_id: id.clone() });
            engine.notify(Some(&ws.row.id), Some(&id), "plan ready", &format!("{} — review with ⌘⇧I", tab.row.title))?;
            Ok(json!({"tab": id}))
        }
        "plan.comments" => {
            #[derive(Deserialize)]
            struct P {
                workspace: Option<String>,
                tab: Option<String>,
                #[serde(default)]
                all: bool,
            }
            let a: P = p(params)?;
            let id = engine.resolve_tab(a.tab.as_deref(), a.workspace.as_deref(), caller)?;
            let comments: Vec<_> =
                engine.plan_comments(&id)?.into_iter().filter(|c| a.all || c.state != "resolved").collect();
            Ok(serde_json::to_value(comments)?)
        }
        "plan.resolve" => {
            #[derive(Deserialize)]
            struct P {
                id: i64,
            }
            let a: P = p(params)?;
            engine.resolve_plan_comment(a.id)?;
            Ok(json!({"resolved": a.id}))
        }

        // ------------------------------------------------------------ prompt
        // Claude Code's UserPromptSubmit hook in a tab (`kitchenctl __prompt-context`).
        "prompt.context" => {
            #[derive(Deserialize)]
            struct P {
                prompt: String,
            }
            let a: P = p(params)?;
            Ok(json!({"text": crate::prompt_context::build(engine, caller, &a.prompt)}))
        }

        // ------------------------------------------------------------ notifications
        "notify" => {
            #[derive(Deserialize)]
            struct P {
                title: String,
                #[serde(default)]
                body: String,
                workspace: Option<String>,
                tab: Option<String>,
            }
            let a: P = p(params)?;
            let tab = engine.resolve_tab(a.tab.as_deref(), a.workspace.as_deref(), caller).ok();
            let ws = tab.as_deref().and_then(|t| engine.find_tab(t)).map(|(w, _)| w.row.id);
            let id = engine.notify(ws.as_deref(), tab.as_deref(), &a.title, &a.body)?;
            Ok(json!({"id": id}))
        }
        "notification.list" => {
            #[derive(Deserialize, Default)]
            #[serde(default)]
            struct P {
                unread: bool,
                limit: Option<i64>,
            }
            let a: P = p(params)?;
            Ok(serde_json::to_value(engine.db().notifications(a.unread, a.limit.unwrap_or(50))?)?)
        }

        // ------------------------------------------------------------ debug
        "debug.screenshot" => {
            #[derive(Deserialize)]
            struct P {
                path: String,
            }
            let a: P = p(params)?;
            let (tx, rx) = std::sync::mpsc::channel();
            engine.push_ui(UiCommand::Screenshot { path: a.path.clone(), reply: tx });
            rx.recv_timeout(std::time::Duration::from_secs(10))
                .map_err(|_| anyhow!("timed out waiting for the window to render"))?
                .map_err(|e| anyhow!(e))?;
            Ok(json!({"path": a.path}))
        }

        "debug.keys" => {
            #[derive(Deserialize)]
            struct P {
                keys: String,
            }
            let a: P = p(params)?;
            let events = crate::plugins::keys::parse_sequence(&a.keys).map_err(|e| anyhow!(e))?;
            let n = events.len();
            engine.push_ui(UiCommand::Keys(events));
            engine.ctx.request_repaint();
            Ok(json!({"events": n}))
        }

        "worktree.list" | "worktree.remove" => worktree(engine, method, params, caller),

        _ => Err(anyhow!("unknown method {method:?}; see `kitchenctl --help`")),
    }
}

/// `worktree.*`: inspect and tidy the repo's worktrees.
fn worktree(engine: &Arc<Engine>, method: &str, params: Value, caller: &Caller) -> Result<Value> {
    use kitchen_core::git::Repo;

    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct P {
        workspace: Option<String>,
        path: Option<String>,
        force: bool,
        /// Include a dirty flag per worktree (runs `git status` in each).
        status: bool,
    }
    let a: P = p(params)?;
    let ws_id = engine.resolve_workspace(a.workspace.as_deref(), caller)?;
    let ws = engine.workspace(&ws_id).context("workspace vanished")?;
    let host = ws.host();
    let repo = Repo::new(&host, &ws.row.repo_path);
    let tab_for = |path: &str| ws.tabs.iter().find(|t| t.row.worktree == path).map(|t| t.row.id.clone());

    match method {
        "worktree.list" => {
            let list = repo.worktrees()?;
            let spares = repo.spare_paths();
            let out: Vec<Value> = list
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    // git lists the main checkout first.
                    let role = if i == 0 {
                        "main"
                    } else if spares.contains(&w.path) {
                        "spare"
                    } else {
                        "edit"
                    };
                    let dirty = a.status.then(|| repo.is_dirty(&w.path).ok()).flatten();
                    json!({"path": w.path, "branch": w.branch, "head": w.head, "role": role, "tab": tab_for(&w.path), "dirty": dirty})
                })
                .collect();
            Ok(Value::Array(out))
        }
        "worktree.remove" => {
            let given = host.expand_home(&a.path.context("path is required")?)?;
            let list = repo.worktrees()?;
            // Match git's canonical spelling of the path (e.g. /private/tmp).
            let top = host
                .run_ok(&["git", "-C", &given, "rev-parse", "--show-toplevel"], None)
                .map(|s| s.trim().to_string())
                .ok();
            let idx = list
                .iter()
                .position(|w| w.path == given || Some(&w.path) == top.as_ref())
                .with_context(|| format!("{given} is not a worktree of {}", ws.row.repo_path))?;
            let wt = &list[idx];
            if idx == 0 {
                bail!("refusing to remove the main checkout {}", wt.path);
            }
            if let Some(tab) = tab_for(&wt.path) {
                bail!("tab {tab} uses {}; close it first (kitchenctl tab close --tab {tab})", wt.path);
            }
            if !a.force && repo.is_dirty(&wt.path)? {
                bail!("{} has uncommitted changes; pass --force to discard them", wt.path);
            }
            repo.remove_worktree(&wt.path, a.force)?;
            Ok(json!({"removed": wt.path, "branch": wt.branch}))
        }
        _ => unreachable!("routed by dispatch"),
    }
}
