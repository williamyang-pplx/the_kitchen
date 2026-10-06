//! Wire protocol shared by the kitchen app and `kitchenctl`.
//!
//! Transport: newline-delimited JSON over a unix socket. One request line,
//! one response line, then the connection may carry further requests.
//! Locally the app listens on [`default_socket_path`]; on a devbox the
//! `kitchenctl __relay` process listens on the same path and tunnels every
//! line back to the app over the workspace's ssh connection.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

/// Environment variables injected into every kitchen terminal.
pub const ENV_SOCKET: &str = "KITCHEN_SOCKET_PATH";
pub const ENV_WORKSPACE: &str = "KITCHEN_WORKSPACE_ID";
pub const ENV_TAB: &str = "KITCHEN_TAB_ID";
pub const ENV_PLAN_FILE: &str = "KITCHEN_PLAN_FILE";
/// Rev the tab's changes are measured against (diff with `git diff $(git merge-base HEAD $KITCHEN_DIFF_BASE)`).
pub const ENV_DIFF_BASE: &str = "KITCHEN_DIFF_BASE";

/// `~/.kitchen` on every host (mac and devbox alike). `KITCHEN_STATE_DIR`
/// overrides it (tests run an isolated kitchen this way).
pub fn state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("KITCHEN_STATE_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".kitchen")
}

pub fn default_socket_path() -> PathBuf {
    std::env::var_os(ENV_SOCKET).map(PathBuf::from).unwrap_or_else(|| state_dir().join("kitchen.sock"))
}

/// Write `msg` as one JSON line and flush.
pub fn write_line(mut w: impl std::io::Write, msg: &impl Serialize) -> std::io::Result<()> {
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    w.write_all(s.as_bytes())?;
    w.flush()
}

/// Who is calling: filled from the terminal environment so commands can
/// default to "the workspace/tab I am running in".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Caller {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl Caller {
    pub fn from_env() -> Self {
        Self {
            workspace_id: std::env::var(ENV_WORKSPACE).ok().filter(|s| !s.is_empty()),
            tab_id: std::env::var(ENV_TAB).ok().filter(|s| !s.is_empty()),
            cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub caller: Caller,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(id: u64, result: Value) -> Self {
        Self { id, ok: true, result, error: None }
    }
    pub fn err(id: u64, error: impl Into<String>) -> Self {
        Self { id, ok: false, result: Value::Null, error: Some(error.into()) }
    }
}

/// One line on the relay's stdio: `rid` pairs a request with its response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayFrame {
    pub rid: u64,
    pub line: String,
}

// ---------------------------------------------------------------- shared views

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceInfo {
    pub id: String,
    /// 1-based position in the sidebar; `workspace:<index>` is accepted as a handle.
    pub index: usize,
    pub name: String,
    /// `local` or the ssh alias of the devbox.
    pub host: String,
    pub repo_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    pub selected: bool,
    pub tabs: Vec<TabInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabInfo {
    pub id: String,
    /// 1-based position in the workspace; Cmd+<index> focuses it.
    pub index: usize,
    pub title: String,
    pub worktree: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub tmux_session: String,
    pub selected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_file: Option<String>,
    /// Rev the viewer diffs against (`origin/<default>` when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_base: Option<String>,
    #[serde(default)]
    pub unread: bool,
}
