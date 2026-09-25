//! Every kitchen tab is a tmux session on the workspace's host. The GUI only
//! attaches to it, so tabs survive app restarts, laptop sleep and dropped
//! ssh links, and agents can drive them headlessly (`send`, `read-screen`).

use anyhow::Result;

use crate::host::Host;
use crate::util::shell_join;

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

    /// Create a detached session in `cwd` with `env`, then type `command`
    /// (if any) into its shell, so the shell survives the command exiting.
    pub fn new_session(&self, name: &str, cwd: &str, env: &[(String, String)], command: Option<&str>) -> Result<()> {
        if !self.has_session(name) {
            let mut args: Vec<String> =
                ["new-session", "-d", "-s", name, "-c", cwd, "-x", "200", "-y", "50"].iter().map(|s| s.to_string()).collect();
            for (k, v) in env {
                args.push("-e".into());
                args.push(format!("{k}={v}"));
            }
            let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            self.tmux(&refs)?;
            // One round trip for all options.
            let mut script = Vec::new();
            for (opt, val) in SESSION_OPTIONS {
                script.push(shell_join(&[TMUX[0], TMUX[1], TMUX[2], "set-option", "-t", name, opt, val]));
            }
            let _ = self.host.sh(&script.join("; "), None);
        }
        if let Some(cmd) = command.filter(|c| !c.trim().is_empty()) {
            self.send_text(name, cmd, true)?;
        }
        Ok(())
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

    pub fn set_environment(&self, name: &str, key: &str, value: &str) -> Result<()> {
        self.tmux(&["set-environment", "-t", name, key, value]).map(|_| ())
    }
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
        let mut screen = String::new();
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            screen = t.capture(&name, None).unwrap();
            if screen.lines().any(|l| l.trim() == "hello-kitchen") {
                break;
            }
        }
        assert!(screen.lines().any(|l| l.trim() == "hello-kitchen"), "screen: {screen}");
        assert!(t.list_kitchen_sessions().unwrap().contains(&name));
        t.kill_session(&name).unwrap();
        assert!(!t.has_session(&name));
    }
}
