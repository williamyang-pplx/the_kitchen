//! Every kitchen tab is a tmux session on the workspace's host. The GUI only
//! attaches to it, so tabs survive app restarts, laptop sleep and dropped
//! ssh links, and agents can drive them headlessly (`send`, `read-screen`).

use anyhow::Result;

use crate::host::Host;
use crate::util::{shell_join, shell_quote};

pub const SESSION_PREFIX: &str = "kt_";

/// The kitchen runs its own tmux server (`tmux -L kitchen`) on every host,
/// isolated from any personal tmux server and its state.
pub const SOCKET_NAME: &str = "kitchen";

/// `tmux -L kitchen` as argv, for building commands.
pub const TMUX: [&str; 3] = ["tmux", "-L", SOCKET_NAME];

pub fn session_name(workspace_id: &str, tab_id: &str) -> String {
    format!("{SESSION_PREFIX}{workspace_id}_{tab_id}")
}

pub struct Tmux<'a> {
    pub host: &'a Host,
}

/// Options applied to every kitchen-owned session. The kitchen draws its own
/// tab bar, so tmux's status line is hidden.
const SESSION_OPTIONS: &[(&str, &str)] = &[
    ("status", "off"),
    ("mouse", "on"),
    ("escape-time", "0"),
    ("history-limit", "50000"),
    ("set-clipboard", "on"),
    ("allow-passthrough", "on"),
    ("focus-events", "on"),
];

impl<'a> Tmux<'a> {
    pub fn new(host: &'a Host) -> Self {
        Self { host }
    }

    fn tmux(&self, args: &[&str]) -> Result<String> {
        let mut argv = TMUX.to_vec();
        argv.extend_from_slice(args);
        self.host.run_ok(&argv, None)
    }

    pub fn has_session(&self, name: &str) -> bool {
        let target = format!("={name}");
        self.host.run(&[TMUX[0], TMUX[1], TMUX[2], "has-session", "-t", &target], None).map(|o| o.ok()).unwrap_or(false)
    }

    /// Create a detached session in `cwd` with `env` (unless it exists) and
    /// type `command` (if any) into its new shell, so the shell survives the
    /// command exiting.
    ///
    /// Everything runs as one script on the host — one ssh round trip for a
    /// devbox — including waiting for the shell prompt, since keystrokes
    /// typed while a shell is still initializing get discarded.
    pub fn new_session(&self, name: &str, cwd: &str, env: &[(String, String)], command: Option<&str>) -> Result<()> {
        self.new_session_with_setup(name, cwd, env, command, None)
    }

    /// [`Tmux::new_session`], running the shell snippet `setup` first in the
    /// same round trip (e.g. writing a prompt file the command reads).
    pub fn new_session_with_setup(
        &self,
        name: &str,
        cwd: &str,
        env: &[(String, String)],
        command: Option<&str>,
        setup: Option<&str>,
    ) -> Result<()> {
        let mut script = new_session_script(name, cwd, env, command);
        if let Some(setup) = setup {
            script = format!("{setup} || exit 21\n{script}");
        }
        self.host.sh_ok(&script, None).map(|_| ())
    }

    /// Make sure session `name` exists, recreating it in `cwd` when it died
    /// and typing `command` (if any) into the new one. Returns `false` (and
    /// creates nothing) when `cwd` no longer exists. One round trip.
    pub fn ensure_session(&self, name: &str, cwd: &str, env: &[(String, String)], command: Option<&str>) -> Result<bool> {
        let script = format!(
            "if [ ! -d {} ]; then echo KT_GONE; else {}; fi",
            shell_quote(cwd),
            new_session_script(name, cwd, env, command)
        );
        Ok(!self.host.sh_ok(&script, None)?.contains("KT_GONE"))
    }

    pub fn kill_session(&self, name: &str) -> Result<()> {
        if self.has_session(name) {
            let target = format!("={name}");
            self.tmux(&["kill-session", "-t", &target])?;
        }
        Ok(())
    }

    /// Type literal text into the session; `enter` presses Return afterwards.
    pub fn send_text(&self, name: &str, text: &str, enter: bool) -> Result<()> {
        // Multi-line text goes through a paste buffer so the receiving program
        // (e.g. Claude Code) sees one bracketed paste, not N submitted lines.
        if text.contains('\n') {
            let buf = format!("kt-{}", crate::util::new_id());
            let script = format!(
                "tmux -L {SOCKET_NAME} load-buffer -b {b} - && tmux -L {SOCKET_NAME} paste-buffer -p -d -b {b} -t {t}",
                b = buf,
                t = crate::util::shell_quote(name)
            );
            self.host.run_with_stdin(&["sh", "-c", &script], None, Some(text.as_bytes()))?.into_result("tmux paste")?;
        } else if !text.is_empty() {
            self.tmux(&["send-keys", "-t", name, "-l", text])?;
        }
        if enter {
            // Give TUIs a beat to finish consuming the paste before Return.
            if text.contains('\n') {
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            self.tmux(&["send-keys", "-t", name, "Enter"])?;
        }
        Ok(())
    }

    /// Send tmux key names (`Enter`, `C-c`, `Escape`, `Up`, ...).
    pub fn send_keys(&self, name: &str, keys: &[&str]) -> Result<()> {
        let mut args = vec!["send-keys", "-t", name];
        args.extend_from_slice(keys);
        self.tmux(&args).map(|_| ())
    }

    /// Visible screen (and optionally `scrollback` lines of history) as text.
    pub fn capture(&self, name: &str, scrollback: Option<u32>) -> Result<String> {
        let start = scrollback.map(|n| format!("-{n}"));
        let mut args = vec!["capture-pane", "-p", "-J", "-t", name];
        if let Some(s) = &start {
            args.push("-S");
            args.push(s);
        }
        self.tmux(&args)
    }

    /// Names of live kitchen sessions on this host.
    pub fn list_kitchen_sessions(&self) -> Result<Vec<String>> {
        let out = self.host.run(&[TMUX[0], TMUX[1], TMUX[2], "list-sessions", "-F", "#{session_name}"], None)?;
        if !out.ok() {
            return Ok(Vec::new()); // no server running
        }
        Ok(out.stdout.lines().filter(|l| l.starts_with(SESSION_PREFIX)).map(String::from).collect())
    }
}

/// Shell script behind [`Tmux::new_session`].
fn new_session_script(name: &str, cwd: &str, env: &[(String, String)], command: Option<&str>) -> String {
    let t = format!("tmux -L {SOCKET_NAME}");
    let s = shell_quote(name);
    let mut create: Vec<String> =
        ["new-session", "-d", "-s", name, "-c", cwd, "-x", "200", "-y", "50"].iter().map(|x| x.to_string()).collect();
    for (k, v) in env {
        create.push("-e".into());
        create.push(format!("{k}={v}"));
    }
    let options = SESSION_OPTIONS
        .iter()
        .map(|(opt, val)| format!("set-option -t {s} {opt} {val}"))
        .collect::<Vec<_>>()
        .join(" \\; ");
    let mut start = format!("{t} {create} && {t} {options} >/dev/null", create = shell_join(&create));
    if let Some(cmd) = command.filter(|c| !c.trim().is_empty()) {
        // Wait (on the host, 50ms polls) until the pane is non-blank and has
        // been unchanged for 300ms, capped at 8s; then type the command.
        start.push_str(&format!(
            " && {{ last=; stable=0; i=0; while [ $i -lt 160 ]; do \
               cur=$({t} capture-pane -p -t {s}); \
               if [ \"$cur\" = \"$last\" ] && [ -n \"$(printf %s \"$cur\" | tr -d '[:space:]')\" ]; then \
                 stable=$((stable+1)); [ $stable -ge 6 ] && break; else stable=0; fi; \
               last=$cur; i=$((i+1)); sleep 0.05; done; \
             {t} send-keys -t {s} -l {cmd} && {t} send-keys -t {s} Enter; }}",
            cmd = shell_quote(cmd)
        ));
    }
    format!("if ! {t} has-session -t ={s} 2>/dev/null; then {start}; fi")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_lifecycle() {
        if Host::Local.run(&["tmux", "-V"], None).map(|o| !o.ok()).unwrap_or(true) {
            return; // tmux not installed
        }
        let t = Tmux::new(&Host::Local);
        let name = session_name("test", &crate::util::new_id());
        let env = vec![("KITCHEN_TEST_VAR".to_string(), "hello-kitchen".to_string())];
        t.new_session(&name, "/tmp", &env, Some("echo \"$KITCHEN_TEST_VAR\"")).unwrap();
        assert!(t.has_session(&name));
        let shows = |line: &str| {
            let mut screen = String::new();
            for _ in 0..40 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                screen = t.capture(&name, None).unwrap();
                if screen.lines().any(|l| l.trim() == line) {
                    return Ok(());
                }
            }
            Err(screen)
        };
        shows("hello-kitchen").unwrap();
        assert!(t.list_kitchen_sessions().unwrap().contains(&name));

        // A live session is left alone; a dead one comes back running the command.
        assert!(t.ensure_session(&name, "/tmp", &env, Some("echo typed-$((40+2))")).unwrap());
        assert!(shows("typed-42").is_err(), "typed into a live session");
        t.kill_session(&name).unwrap();
        assert!(!t.has_session(&name));
        assert!(t.ensure_session(&name, "/tmp", &env, Some("echo typed-$((40+2))")).unwrap());
        shows("typed-42").unwrap();
        assert!(!t.ensure_session(&name, "/nonexistent-kitchen-dir", &env, None).unwrap(), "gone worktree");
        t.kill_session(&name).unwrap();
    }
}
