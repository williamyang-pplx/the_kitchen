//! One request/response over the kitchen socket.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use kitchen_proto::{Caller, Request, Response};
use serde_json::Value;

/// Why a call failed: the app answered with an error, or nothing answered.
#[derive(Debug)]
pub enum CallError {
    Api(String),
    Connect(anyhow::Error),
}

pub fn call(socket: &Path, method: &str, params: Value) -> Result<Value, CallError> {
    let resp = roundtrip(socket, method, params).map_err(CallError::Connect)?;
    if resp.ok { Ok(resp.result) } else { Err(CallError::Api(resp.error.unwrap_or_else(|| "unknown error".into()))) }
}

fn roundtrip(socket: &Path, method: &str, params: Value) -> Result<Response> {
    let stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "cannot reach the kitchen at {} — is the app running? (on a devbox the relay starts \
             when the app has a workspace on this host)",
            socket.display()
        )
    })?;
    // Tab creation fetches and builds worktrees; allow it time.
    stream.set_read_timeout(Some(Duration::from_secs(600)))?;
    let req = Request { id: 1, method: method.to_string(), params, caller: Caller::from_env() };
    kitchen_proto::write_line(&stream, &req)?;
    let mut reader = BufReader::new(&stream);
    let mut out = String::new();
    reader.read_line(&mut out).context("read response")?;
    if out.trim().is_empty() {
        anyhow::bail!("the kitchen closed the connection without answering");
    }
    serde_json::from_str(&out).context("parse response")
}
