//! Host setup run when a workspace is created (and once per app start):
//!
//! * devbox: pull (or clone) the dotfiles repo from GitHub, relink skills,
//!   import its `bazelrc`, install the `kitchenctl` binary, probe the tools
//!   the kitchen needs;
//! * mac: relink skills and import `bazelrc` from the local dotfiles checkout.
//!
//! Skills are always symlinks into the dotfiles checkout, one per skill,
//! mirroring the dotfiles `install.sh` layout (`skills/<category>/<skill>`).
//! Existing non-symlink entries are never touched — they are reported.

use anyhow::{Context, Result};

use crate::host::Host;
use crate::util::shell_quote;

#[derive(Debug, Clone)]
pub struct Options {
    pub dotfiles_local: String,
    pub dotfiles_remote_url: String,
    pub dotfiles_remote_dir: String,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub linked: usize,
    pub warnings: Vec<String>,
    pub cli_installed: bool,
}

/// Tools a workspace host must have. Missing ones are reported, never installed.
const REQUIRED_TOOLS: &[&str] = &["git", "tmux"];
const OPTIONAL_TOOLS: &[&str] = &["claude", "gh"];

pub fn bootstrap_host(host: &Host, opts: &Options) -> Result<Report> {
    let mut report = Report::default();
    host.sh_ok("mkdir -p \"$HOME/.kitchen\" && chmod 700 \"$HOME/.kitchen\"", None)?;
    let dotfiles = match host {
        Host::Local => opts.dotfiles_local.clone(),
        Host::Ssh(_) => {
            let dir = host.expand_home(&opts.dotfiles_remote_dir)?;
            sync_dotfiles(host, &dir, &opts.dotfiles_remote_url, &mut report)?;
            dir
        }
    };
    link_skills(host, &dotfiles, &mut report)?;
    import_bazelrc(host, &dotfiles, &mut report)?;
    probe_tools(host, &mut report)?;
    if host.is_local() {
        // An isolated test kitchen (KITCHEN_STATE_DIR) must not repoint the real install.
        if std::env::var_os("KITCHEN_STATE_DIR").is_none() {
            if let Err(e) = link_local_cli(&mut report) {
                report.warnings.push(format!("kitchenctl not linked: {e:#}"));
            }
        }
    } else {
        match install_cli(host) {
            Ok(installed) => report.cli_installed = installed,
            Err(e) => report.warnings.push(format!("kitchenctl not installed: {e:#}")),
        }
    }
    Ok(report)
}

/// Point `~/.local/bin/kitchenctl` at the kitchenctl built next to the app.
/// Never replaces a real file, only a missing entry or an existing symlink.
fn link_local_cli(report: &mut Report) -> Result<()> {
    let exe = std::env::current_exe()?;
    let Some(cli) = exe.parent().map(|d| d.join("kitchenctl")).filter(|p| p.exists()) else {
        return Ok(()); // not built alongside the app (e.g. a bare `cargo run`)
    };
    let home = std::env::var("HOME").context("HOME unset")?;
    let dest = std::path::Path::new(&home).join(".local/bin/kitchenctl");
    match std::fs::symlink_metadata(&dest) {
        Ok(m) if m.file_type().is_symlink() => {
            if std::fs::read_link(&dest)? == cli {
                return Ok(());
            }
            std::fs::remove_file(&dest)?;
        }
        Ok(_) => {
            report.warnings.push(format!("{} is not a symlink; left alone", dest.display()));
            return Ok(());
        }
        Err(_) => std::fs::create_dir_all(dest.parent().expect("has parent"))?,
    }
    std::os::unix::fs::symlink(&cli, &dest)?;
    report.cli_installed = true;
    Ok(())
}

/// `git pull` the dotfiles checkout, cloning it first when missing.
pub fn sync_dotfiles(host: &Host, dir: &str, url: &str, report: &mut Report) -> Result<()> {
    let d = shell_quote(dir);
    let script = format!(
        "if [ -d {d}/.git ]; then git -C {d} pull --ff-only --quiet 2>&1 || echo 'WARN dotfiles pull failed (local changes?)'; \
         else git clone --quiet {u} {d} 2>&1; fi",
        u = shell_quote(url)
    );
    let out = host.sh_ok(&script, None).context("sync dotfiles")?;
    collect_warnings(&out, report);
    Ok(())
}

/// Symlink `<dotfiles>/skills/*/*` into `~/.claude/skills` and `~/.codex/skills`.
pub fn link_skills(host: &Host, dotfiles: &str, report: &mut Report) -> Result<()> {
    let script = format!(
        r#"set -u
DOT={dot}
[ -d "$DOT/skills" ] || {{ echo "WARN no skills directory in $DOT"; exit 0; }}
n=0
for root in "$HOME/.claude/skills" "$HOME/.codex/skills"; do
  mkdir -p "$root"
  for src in "$DOT"/skills/*/*/; do
    src="${{src%/}}"
    [ -f "$src/SKILL.md" ] || continue
    dest="$root/$(basename "$src")"
    if [ -L "$dest" ]; then
      [ "$(readlink "$dest")" = "$src" ] || ln -sfn "$src" "$dest"
    elif [ -e "$dest" ]; then
      echo "WARN $dest is not a symlink; left alone"
      continue
    else
      ln -s "$src" "$dest"
    fi
    n=$((n+1))
  done
done
echo "LINKED $n""#,
        dot = shell_quote(dotfiles)
    );
    let out = host.sh_ok(&script, None).context("link skills")?;
    for line in out.lines() {
        if let Some(n) = line.strip_prefix("LINKED ") {
            report.linked = n.trim().parse().unwrap_or(0);
        }
    }
    collect_warnings(&out, report);
    Ok(())
}

/// Make `~/.bazelrc` `try-import` the dotfiles `bazelrc` (the shared remote
/// cache), as the dotfiles `install.sh` does, so every repo and worktree on
/// the host gets it. Appends the line once; the rest of the file is left alone.
pub fn import_bazelrc(host: &Host, dotfiles: &str, report: &mut Report) -> Result<()> {
    let line = shell_quote(&format!("try-import {dotfiles}/bazelrc"));
    let script = format!(
        "grep -qxF {line} \"$HOME/.bazelrc\" 2>/dev/null || \
         printf '\\n# Added by the kitchen: shared Bazel settings (remote cache).\\n%s\\n' {line} >> \"$HOME/.bazelrc\" \
         || echo 'WARN could not update ~/.bazelrc'"
    );
    let out = host.sh_ok(&script, None).context("import bazelrc")?;
    collect_warnings(&out, report);
    Ok(())
}

fn probe_tools(host: &Host, report: &mut Report) -> Result<()> {
    let all: Vec<&str> = REQUIRED_TOOLS.iter().chain(OPTIONAL_TOOLS).copied().collect();
    let script = format!("for t in {}; do command -v \"$t\" >/dev/null 2>&1 || echo \"MISSING $t\"; done; true", all.join(" "));
    let out = host.sh_ok(&script, None)?;
    for line in out.lines() {
        if let Some(t) = line.strip_prefix("MISSING ") {
            let hint = if REQUIRED_TOOLS.contains(&t) { "required" } else { "some features disabled" };
            report.warnings.push(format!("{t} not found on {} ({hint})", host.label()));
        }
    }
    Ok(())
}

fn collect_warnings(out: &str, report: &mut Report) {
    for line in out.lines() {
        if let Some(w) = line.strip_prefix("WARN ") {
            report.warnings.push(w.to_string());
        }
    }
}

// ------------------------------------------------------------------ kitchenctl

/// Rust target triple for a devbox `uname -m`.
pub fn linux_triple(uname_m: &str) -> Option<&'static str> {
    match uname_m.trim() {
        "x86_64" | "amd64" => Some("x86_64-unknown-linux-musl"),
        "aarch64" | "arm64" => Some("aarch64-unknown-linux-musl"),
        _ => None,
    }
}

/// Where a cross-built `kitchenctl` for `triple` may live on the mac.
pub fn cli_binary_candidates(triple: &str) -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.join(format!("kitchenctl-{triple}")));
        }
    }
    v.push(kitchen_proto::state_dir().join("dist").join(format!("kitchenctl-{triple}")));
    // Development builds: <workspace>/target/<triple>/release/kitchenctl.
    let ws_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    v.push(ws_root.join("target").join(triple).join("release").join("kitchenctl"));
    v
}

/// Copy the matching Linux `kitchenctl` to `~/.kitchen/bin` on a devbox and
/// link it into `~/.local/bin`. Skips the upload when the hash matches.
pub fn install_cli(host: &Host) -> Result<bool> {
    let arch = host.run_ok(&["uname", "-m"], None)?;
    let triple = linux_triple(&arch).with_context(|| format!("unsupported devbox arch {}", arch.trim()))?;
    let local = cli_binary_candidates(triple)
        .into_iter()
        .find(|p| p.exists())
        .with_context(|| format!("no kitchenctl build for {triple}; run scripts/build-cli-linux.sh"))?;
    let bytes = std::fs::read(&local)?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let remote_hash = host.sh("cat \"$HOME/.kitchen/bin/kitchenctl.hash\" 2>/dev/null", None)?.stdout;
    if remote_hash.trim() == hash {
        return Ok(false);
    }
    let script = format!(
        "set -e; mkdir -p \"$HOME/.kitchen/bin\" \"$HOME/.local/bin\"; \
         cat > \"$HOME/.kitchen/bin/kitchenctl.tmp\"; chmod +x \"$HOME/.kitchen/bin/kitchenctl.tmp\"; \
         mv \"$HOME/.kitchen/bin/kitchenctl.tmp\" \"$HOME/.kitchen/bin/kitchenctl\"; \
         ln -sfn \"$HOME/.kitchen/bin/kitchenctl\" \"$HOME/.local/bin/kitchenctl\"; \
         printf %s {hash} > \"$HOME/.kitchen/bin/kitchenctl.hash\""
    );
    host.run_with_stdin(&["sh", "-c", &script], None, Some(&bytes))?.into_result("upload kitchenctl")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Held by tests that point HOME at a temp dir.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn links_skills_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dot = root.join("dot");
        std::fs::create_dir_all(dot.join("skills/cat/alpha")).unwrap();
        std::fs::write(dot.join("skills/cat/alpha/SKILL.md"), "x").unwrap();
        std::fs::create_dir_all(dot.join("skills/cat/not-a-skill")).unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".claude/skills/blocker")).unwrap();
        std::fs::create_dir_all(dot.join("skills/cat/blocker")).unwrap();
        std::fs::write(dot.join("skills/cat/blocker/SKILL.md"), "x").unwrap();

        // Run the script with a fake HOME.
        let script_host = Host::Local;
        let dot_s = dot.display().to_string();
        let run = || {
            let mut r = Report::default();
            let _home = HOME_LOCK.lock().unwrap();
            let old = std::env::var("HOME").unwrap();
            // SAFETY: test-only; HOME_LOCK serializes the tests that set HOME.
            unsafe { std::env::set_var("HOME", &home) };
            let res = link_skills(&script_host, &dot_s, &mut r);
            unsafe { std::env::set_var("HOME", old) };
            res.unwrap();
            r
        };
        let r1 = run();
        let r2 = run();
        assert_eq!(r1.linked, 3, "alpha into .claude and .codex, blocker into .codex only");
        assert_eq!(r2.linked, 3);
        let link = std::fs::read_link(home.join(".claude/skills/alpha")).unwrap();
        assert_eq!(link, dot.join("skills/cat/alpha"));
        assert!(r1.warnings.iter().any(|w| w.contains("blocker")), "{:?}", r1.warnings);
        assert!(!home.join(".claude/skills/not-a-skill").exists());
    }

    #[test]
    fn imports_bazelrc_once_keeping_existing_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::write(home.join(".bazelrc"), "startup --max_idle_secs=600").unwrap();
        let _home = HOME_LOCK.lock().unwrap();
        let old = std::env::var("HOME").unwrap();
        // SAFETY: test-only; HOME_LOCK serializes the tests that set HOME.
        unsafe { std::env::set_var("HOME", home) };
        let mut r = Report::default();
        let res = import_bazelrc(&Host::Local, "/dot files", &mut r).and_then(|_| import_bazelrc(&Host::Local, "/dot files", &mut r));
        unsafe { std::env::set_var("HOME", old) };
        res.unwrap();
        let rc = std::fs::read_to_string(home.join(".bazelrc")).unwrap();
        assert!(rc.starts_with("startup --max_idle_secs=600\n"), "{rc}");
        assert_eq!(rc.matches("try-import /dot files/bazelrc\n").count(), 1, "{rc}");
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn triples() {
        assert_eq!(linux_triple("x86_64\n"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(linux_triple("aarch64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(linux_triple("sparc"), None);
    }
}
