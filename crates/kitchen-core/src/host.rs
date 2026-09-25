//! Where a workspace lives: this mac, or a devbox reached over ssh.
//!
//! Remote commands go through one ssh ControlMaster per alias, so after the
//! first connection every exec costs a round trip, not a handshake.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::util::{shell_join, shell_quote};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Host {
    Local,
    Ssh(String),
}

impl Host {
    /// `""`/`local`/`localhost` mean this mac; anything else is an ssh alias.
    pub fn parse(s: &str) -> Host {
        match s.trim() {
            "" | "local" | "localhost" => Host::Local,
            alias => Host::Ssh(alias.to_string()),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Host::Local => "local",
            Host::Ssh(a) => a,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Host::Local)
    }

    /// Run `argv` in `cwd` and capture its output. Never fails on a non-zero
    /// exit; check [`Output::ok`] or use [`Host::run_ok`].
    pub fn run(&self, argv: &[&str], cwd: Option<&str>) -> Result<Output> {
        self.run_with_stdin(argv, cwd, None)
    }

    pub fn run_with_stdin(&self, argv: &[&str], cwd: Option<&str>, stdin: Option<&[u8]>) -> Result<Output> {
        let mut cmd = match self {
            Host::Local => {
                let mut c = Command::new(argv[0]);
                c.args(&argv[1..]);
                if let Some(cwd) = cwd {
                    c.current_dir(cwd);
                }
                c
            }
            Host::Ssh(alias) => {
                let mut script = String::new();
                if let Some(cwd) = cwd {
                    script.push_str(&format!("cd {} && ", shell_quote(cwd)));
                }
                script.push_str(&shell_join(argv));
                let mut c = Command::new("ssh");
                c.args(ssh_base_args()).arg(alias).arg("--").arg(script);
                c
            }
        };
        run_command(&mut cmd, stdin).with_context(|| format!("[{}] {}", self.label(), shell_join(argv)))
    }

    /// Like [`Host::run`] but turns a non-zero exit into an error carrying stderr.
    pub fn run_ok(&self, argv: &[&str], cwd: Option<&str>) -> Result<String> {
        let out = self.run(argv, cwd)?;
        out.into_result(&format!("[{}] {}", self.label(), shell_join(argv)))
    }

    /// Run a shell script (`sh -c`). Use for pipelines; prefer [`Host::run`].
    pub fn sh(&self, script: &str, cwd: Option<&str>) -> Result<Output> {
        self.run(&["sh", "-c", script], cwd)
    }

    pub fn sh_ok(&self, script: &str, cwd: Option<&str>) -> Result<String> {
        let out = self.sh(script, cwd)?;
        out.into_result(&format!("[{}] sh -c {script}", self.label()))
    }

    pub fn read_file(&self, path: &str) -> Result<String> {
        match self {
            Host::Local => std::fs::read_to_string(path).with_context(|| format!("read {path}")),
            Host::Ssh(_) => self.run_ok(&["cat", "--", path], None),
        }
    }

    pub fn write_file(&self, path: &str, contents: &str) -> Result<()> {
        match self {
            Host::Local => {
                if let Some(parent) = std::path::Path::new(path).parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, contents).with_context(|| format!("write {path}"))
            }
            Host::Ssh(_) => {
                let script = format!(
                    "mkdir -p \"$(dirname {p})\" && cat > {p}",
                    p = shell_quote(path)
                );
                self.run_with_stdin(&["sh", "-c", &script], None, Some(contents.as_bytes()))?
                    .into_result("write remote file")
                    .map(|_| ())
            }
        }
    }

    pub fn exists(&self, path: &str) -> bool {
        match self {
            Host::Local => std::path::Path::new(path).exists(),
            Host::Ssh(_) => self.run(&["test", "-e", path], None).map(|o| o.ok()).unwrap_or(false),
        }
    }

    pub fn home(&self) -> Result<String> {
        match self {
            Host::Local => Ok(std::env::var("HOME").context("HOME unset")?),
            Host::Ssh(_) => Ok(self.sh_ok("printf %s \"$HOME\"", None)?.trim().to_string()),
        }
    }

    /// Expand a leading `~` against this host's home directory.
    pub fn expand_home(&self, path: &str) -> Result<String> {
        if path == "~" {
            return self.home();
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return Ok(format!("{}/{}", self.home()?, rest));
        }
        Ok(path.to_string())
    }

    /// argv for a pty that attaches to a tmux session on this host.
    pub fn tmux_attach_argv(&self, session: &str) -> Vec<String> {
        let tmux = ["tmux", "-L", crate::tmux::SOCKET_NAME, "attach-session", "-t", session];
        match self {
            Host::Local => tmux.iter().map(|s| s.to_string()).collect(),
            Host::Ssh(alias) => {
                let mut v = vec!["ssh".to_string()];
                v.extend(ssh_base_args());
                v.push("-tt".into());
                v.push(alias.clone());
                v.push("--".into());
                v.push(shell_join(&tmux));
                v
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    pub fn into_result(self, what: &str) -> Result<String> {
        if self.ok() {
            Ok(self.stdout)
        } else {
            let msg = if self.stderr.trim().is_empty() { self.stdout.trim() } else { self.stderr.trim() };
            bail!("{what} failed (exit {}): {msg}", self.status)
        }
    }
}

fn run_command(cmd: &mut Command, stdin: Option<&[u8]>) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn()?;
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("stdin piped");
        let input = input.to_vec();
        // Write from a thread so a large stdin cannot deadlock against a full stdout pipe.
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    let out = child.wait_with_output()?;
    Ok(Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

pub fn ssh_control_dir() -> PathBuf {
    kitchen_proto::state_dir().join("ssh")
}

/// Options shared by every ssh invocation: multiplexed, non-interactive,
/// with keepalives so a sleeping laptop notices a dead link quickly.
pub fn ssh_base_args() -> Vec<String> {
    let dir = ssh_control_dir();
    let _ = std::fs::create_dir_all(&dir);
    vec![
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={}/%C", dir.display()),
        "-o".into(),
        "ControlPersist=30m".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=15".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
    ]
}

/// GUI apps launched from Finder get a bare PATH; make sure the tools the
/// kitchen shells out to (git, gh, tmux, claude) resolve.
pub fn augment_local_path() {
    let home = std::env::var("HOME").unwrap_or_default();
    let extra = [
        format!("{home}/.local/bin"),
        format!("{home}/.cargo/bin"),
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
        "/usr/sbin".to_string(),
        "/sbin".to_string(),
    ];
    let current = std::env::var("PATH").unwrap_or_default();
    let mut parts: Vec<String> = current.split(':').filter(|s| !s.is_empty()).map(String::from).collect();
    for p in extra {
        if !parts.contains(&p) {
            parts.push(p);
        }
    }
    // SAFETY: called once at startup before any other thread exists.
    unsafe { std::env::set_var("PATH", parts.join(":")) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host() {
        assert_eq!(Host::parse(""), Host::Local);
        assert_eq!(Host::parse("local"), Host::Local);
        assert_eq!(Host::parse("oval-bear-fern"), Host::Ssh("oval-bear-fern".into()));
    }

    #[test]
    fn local_run() {
        let out = Host::Local.run(&["sh", "-c", "echo hi; echo err >&2; exit 3"], None).unwrap();
        assert_eq!(out.status, 3);
        assert_eq!(out.stdout.trim(), "hi");
        assert_eq!(out.stderr.trim(), "err");
        assert!(Host::Local.run_ok(&["false"], None).is_err());
    }

    #[test]
    fn local_stdin() {
        let out = Host::Local.run_with_stdin(&["cat"], None, Some(b"abc")).unwrap();
        assert_eq!(out.stdout, "abc");
    }

    #[test]
    fn ssh_attach_argv_quotes_remote_command() {
        let argv = Host::Ssh("box".into()).tmux_attach_argv("kt_a_b");
        assert_eq!(argv.last().unwrap(), "tmux -L kitchen attach-session -t kt_a_b");
        assert!(argv.contains(&"-tt".to_string()));
    }
}
