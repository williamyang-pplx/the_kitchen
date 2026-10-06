//! App end of the devbox API tunnel.
//!
//! The devbox ssh gateway refuses `-R` port and socket forwarding, so for
//! each devbox the app keeps one `ssh <alias> kitchenctl __relay` running.
//! The relay listens on the devbox's `~/.kitchen/kitchen.sock` and sends each
//! request line to us as a [`RelayFrame`] on its stdout; we answer through
//! [`api::handle_line`] and write the response frame to its stdin. When the
//! app exits, the relay's stdin closes and it cleans up after itself.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use kitchen_proto::RelayFrame;

use crate::api;
use crate::engine::Engine;

/// Remote command: the binary bootstrap installs, run by the login shell.
const REMOTE_RELAY: &str = "exec \"$HOME/.kitchen/bin/kitchenctl\" __relay";

const MIN_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A relay that stayed up this long counts as healthy; backoff resets.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);

fn running() -> &'static Mutex<HashSet<String>> {
    static RUNNING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

/// Keep one `ssh <alias> kitchenctl __relay` running for this host.
/// Idempotent: a second call for the same alias does nothing.
pub fn ensure_relay(engine: &Arc<Engine>, alias: &str) {
    if !running().lock().unwrap().insert(alias.to_string()) {
        return;
    }
    let engine = engine.clone();
    let alias = alias.to_string();
    let _ = std::thread::Builder::new().name(format!("relay-{alias}")).spawn(move || supervise(&engine, &alias));
}

fn host_still_used(engine: &Engine, alias: &str) -> bool {
    engine.workspaces().iter().any(|w| w.row.host == alias)
}

fn supervise(engine: &Arc<Engine>, alias: &str) {
    let mut backoff = MIN_BACKOFF;
    let mut failures = 0u32;
    loop {
        let started = Instant::now();
        let result = run_once(engine, alias);
        if !host_still_used(engine, alias) {
            break;
        }
        if started.elapsed() >= HEALTHY_AFTER {
            backoff = MIN_BACKOFF;
            failures = 0;
        }
        failures += 1;
        match &result {
            // Toast the first failure of a streak only; later ones go to the log.
            Err(e) if failures == 1 => engine.toast_err(format!("{alias}: kitchenctl relay down, retrying: {e:#}")),
            Err(e) => log::warn!("relay {alias}: {e:#}"),
            Ok(()) => log::info!("relay {alias} exited; restarting"),
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
    running().lock().unwrap().remove(alias);
}

/// One relay session; returns when the ssh child exits.
fn run_once(engine: &Arc<Engine>, alias: &str) -> Result<()> {
    let mut child = Command::new("ssh")
        .args(kitchen_core::host::ssh_base_args())
        .arg("-T")
        .arg(alias)
        .arg("--")
        .arg(REMOTE_RELAY)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn ssh")?;
    let stdin = Arc::new(Mutex::new(child.stdin.take().context("relay stdin")?));
    let stdout = child.stdout.take().context("relay stdout")?;
    let stderr = child.stderr.take().context("relay stderr")?;

    let tail = Arc::new(Mutex::new(Vec::<String>::new()));
    let tail2 = tail.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut t = tail2.lock().unwrap();
            t.push(line);
            let n = t.len();
            if n > 20 {
                t.drain(..n - 20);
            }
        }
    });

    let mut connected = false;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(frame) = serde_json::from_str::<RelayFrame>(&line) else {
            log::warn!("relay {alias}: ignoring non-frame output: {line}");
            continue;
        };
        if frame.rid == 0 {
            connected = true;
            log::info!("relay {alias} connected: {}", frame.line);
            continue;
        }
        let engine = engine.clone();
        let stdin = stdin.clone();
        // Requests can block (tab creation fetches); answer each on its own thread.
        std::thread::spawn(move || {
            let resp = api::handle_line_from(&engine, &frame.line, true);
            let Ok(resp_line) = serde_json::to_string(&resp) else { return };
            let _ = kitchen_proto::write_line(&mut *stdin.lock().unwrap(), &RelayFrame { rid: frame.rid, line: resp_line });
        });
    }
    let status = child.wait()?;
    if connected && status.success() {
        return Ok(());
    }
    let tail = tail.lock().unwrap().join(" | ");
    bail!("ssh exited with {status}{}", if tail.is_empty() { String::new() } else { format!(": {tail}") })
}
