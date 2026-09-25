//! The socket API behind `kitchenctl`. Newline-delimited JSON
//! ([`kitchen_proto::Request`] / [`kitchen_proto::Response`]).
//!
//! Every human keyboard action has an API method here, with one deliberate
//! exception: publishing agent review drafts to GitHub. That only happens
//! from a keypress in the review inbox (see `review.rs`), so no agent can
//! make a comment public on its own.

use std::io::{BufRead, BufReader, Write};
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
        let Ok(mut out) = serde_json::to_string(&resp) else { break };
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() {
            break;
        }
    }
}

/// Parse and answer one request line (also used by the devbox relay).
pub fn handle_line(engine: &Arc<Engine>, line: &str) -> Response {
    match serde_json::from_str::<Request>(line) {
        Ok(req) => {
            let id = req.id;
            match dispatch(engine, &req.method, req.params, &req.caller) {
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
                    "tab": tab.as_ref().map(|t| json!({"id": t.row.id, "kind": t.row.kind, "title": t.row.title, "worktree": t.row.worktree, "branch": t.row.branch, "pr_number": t.row.pr_number, "plan_file": t.row.plan_file})),
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
            Ok(json!({"id": row.id, "workspace": ws, "kind": row.kind, "title": row.title, "worktree": row.worktree,
                      "branch": row.branch, "tmux_session": row.tmux_session, "plan_file": row.plan_file}))
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
            Ok(json!({"tab": id, "plan_file": tab.row.plan_file}))
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
            engine.notify(Some(&ws.row.id), Some(&id), "plan ready", &format!("{} — review with ⌘L", tab.row.title))?;
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

        _ => crate::review::dispatch(engine, method, params, caller)
            .unwrap_or_else(|| Err(anyhow!("unknown method {method:?}; see `kitchenctl help`"))),
    }
}
