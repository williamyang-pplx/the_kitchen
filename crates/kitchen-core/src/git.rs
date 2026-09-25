//! Git operations, always exec'd through a [`Host`] so local and devbox
//! repositories behave the same.
//!
//! Worktree layout follows the existing convention on both machines:
//! siblings of the main checkout named `<repo>-<slug>`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::host::Host;
use crate::util::shell_quote;

/// Directory name of the shared read-only worktree, next to the repo.
pub const READONLY_SUFFIX: &str = "kitchen-readonly";

/// A spare detached worktree kept checked out next to the repo, so a new
/// edit tab can take it over instead of paying for a full checkout.
pub const SPARE_SUFFIX: &str = "kitchen-spare";

pub struct Repo<'a> {
    pub host: &'a Host,
    /// Absolute path of the main checkout.
    pub path: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub path: String,
    pub head: String,
    pub branch: Option<String>,
    pub detached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// Path in the new tree.
    pub path: String,
    /// Path in the base tree when the file was renamed.
    pub old_path: Option<String>,
    /// `A`dded, `M`odified, `D`eleted, `R`enamed.
    pub status: char,
    pub added: u32,
    pub removed: u32,
}

impl<'a> Repo<'a> {
    pub fn new(host: &'a Host, path: &'a str) -> Self {
        Self { host, path }
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        let mut argv = vec!["git", "-C", self.path];
        argv.extend_from_slice(args);
        self.host.run_ok(&argv, None)
    }

    fn git_in(&self, dir: &str, args: &[&str]) -> Result<String> {
        let mut argv = vec!["git", "-C", dir];
        argv.extend_from_slice(args);
        self.host.run_ok(&argv, None)
    }

    pub fn repo_name(&self) -> &str {
        self.path.trim_end_matches('/').rsplit('/').next().unwrap_or(self.path)
    }

    pub fn parent_dir(&self) -> &str {
        let p = self.path.trim_end_matches('/');
        match p.rfind('/') {
            Some(0) => "/",
            Some(i) => &p[..i],
            None => ".",
        }
    }

    pub fn sibling_path(&self, slug: &str) -> String {
        format!("{}/{}-{}", self.parent_dir(), self.repo_name(), slug)
    }

    pub fn readonly_path(&self) -> String {
        self.sibling_path(READONLY_SUFFIX)
    }

    pub fn is_repo(&self) -> bool {
        self.git(&["rev-parse", "--git-dir"]).is_ok()
    }

    pub fn origin_url(&self) -> Result<String> {
        Ok(self.git(&["remote", "get-url", "origin"])?.trim().to_string())
    }

    /// `owner/name` for a GitHub origin, if origin is on GitHub.
    pub fn github_slug(&self) -> Option<String> {
        parse_github_slug(&self.origin_url().ok()?)
    }

    pub fn fetch(&self) -> Result<()> {
        self.git(&["fetch", "--prune", "--quiet", "origin"]).map(|_| ())
    }

    /// `main`/`master`/…: origin's HEAD, falling back to probing.
    pub fn default_branch(&self) -> Result<String> {
        if let Ok(r) = self.git(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
            if let Some(b) = r.trim().strip_prefix("origin/") {
                return Ok(b.to_string());
            }
        }
        for b in ["main", "master"] {
            if self.git(&["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{b}")]).is_ok() {
                return Ok(b.to_string());
            }
        }
        bail!("cannot determine default branch of {}", self.path)
    }

    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        Ok(parse_worktree_porcelain(&self.git(&["worktree", "list", "--porcelain"])?))
    }

    pub fn worktree_for_branch(&self, branch: &str) -> Result<Option<Worktree>> {
        Ok(self.worktrees()?.into_iter().find(|w| w.branch.as_deref() == Some(branch)))
    }

    pub fn local_branch_exists(&self, branch: &str) -> bool {
        self.git(&["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok()
    }

    fn remote_branch_exists(&self, branch: &str) -> bool {
        self.git(&["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}")]).is_ok()
    }

    /// New branch off the freshly fetched default branch, in its own worktree.
    pub fn add_worktree_new_branch(&self, branch: &str, path: &str, base: &str) -> Result<Worktree> {
        let start = format!("origin/{base}");
        self.git(&["worktree", "add", "--no-track", "-b", branch, path, &start])?;
        self.describe(path)
    }

    /// Check out an existing branch (e.g. a PR head) in a worktree, reusing
    /// the worktree that already has it checked out.
    pub fn add_worktree_existing_branch(&self, branch: &str, path: &str) -> Result<Worktree> {
        if let Some(wt) = self.worktree_for_branch(branch)? {
            return Ok(wt);
        }
        if !self.remote_branch_exists(branch) {
            let _ = self.git(&["fetch", "--quiet", "origin", &format!("{branch}:refs/remotes/origin/{branch}")]);
        }
        if self.local_branch_exists(branch) {
            self.git(&["worktree", "add", path, branch])?;
        } else if self.remote_branch_exists(branch) {
            let upstream = format!("origin/{branch}");
            self.git(&["worktree", "add", "--track", "-b", branch, path, &upstream])?;
        } else {
            bail!("branch {branch} exists neither locally nor on origin");
        }
        self.describe(path)
    }

    /// Detached, read-only worktree at `rev`, re-used and moved when it
    /// exists. Files are made non-writable so edits fail loudly. With
    /// `fetch = Some(branch)`, `git fetch origin <branch>` runs first. One
    /// round trip to the host.
    pub fn sync_readonly_worktree(&self, path: &str, rev: &str) -> Result<Worktree> {
        self.sync_readonly_worktree_fetching(path, rev, None)
    }

    pub fn sync_readonly_worktree_fetching(&self, path: &str, rev: &str, fetch: Option<&str>) -> Result<Worktree> {
        let (r, p, rv) = (shell_quote(self.path), shell_quote(path), shell_quote(rev));
        let fetch = fetch.map(|b| format!("{} || exit 11\n", fetch_branch_cmd(&r, b))).unwrap_or_default();
        let script = format!(
            r#"{fetch}lock() {{ find {p} -path {p}/.git -prune -o -exec chmod "$1" {{}} + 2>/dev/null; true; }}
if [ -e {p} ]; then
  git -C {p} rev-parse --show-toplevel >/dev/null 2>&1 || {{ echo "{path} exists but is not a git worktree" >&2; exit 12; }}
  lock u+w
  git -C {p} checkout --quiet --detach --force {rv} && git -C {p} clean -fdq; rc=$?
  lock a-w
  [ $rc = 0 ] || exit $rc
else
  git -C {r} worktree add --quiet --detach {p} {rv} || exit 13
  lock a-w
fi
{report}"#,
            report = report_snippet(&p),
        );
        parse_report(&self.host.sh_ok(&script, None)?)
    }

    /// New branch `<prefix><slug>` (first free slug: `slug`, `slug-2`, …)
    /// off freshly fetched `origin/<base>`, checked out in the sibling
    /// worktree `<repo>-<slug>`, with `.kitchen/` git-excluded. One round
    /// trip to the host.
    pub fn create_edit_worktree(&self, base: &str, prefix: &str, slug: &str) -> Result<Worktree> {
        let (r, b, pf, sl) = (shell_quote(self.path), shell_quote(base), shell_quote(prefix), shell_quote(slug));
        let parent = shell_quote(self.parent_dir());
        let name = shell_quote(self.repo_name());
        let script = format!(
            r#"{fetch} || exit 11
s={sl}; n=1
while [ -e {parent}/{name}-"$s" ] || git -C {r} rev-parse --verify --quiet "refs/heads/"{pf}"$s" >/dev/null; do
  n=$((n+1)); s={sl}-$n
done
P={parent}/{name}-"$s"
SP={parent}/{name}-{spare}
# Take over the pre-checked-out spare (instant), else check out from scratch.
if [ -e "$SP/.git" ] && git -C {r} worktree move "$SP" "$P" 2>/dev/null; then
  if [ "$(git -C "$P" rev-parse HEAD)" = "$(git -C {r} rev-parse origin/{b})" ]; then
    # Same commit: pure ref updates, no index refresh (~0.6s saved on big repos).
    git -C "$P" branch --no-track {pf}"$s" origin/{b} && git -C "$P" symbolic-ref HEAD refs/heads/{pf}"$s" || exit 13
  else
    git -C "$P" checkout --quiet --no-track -b {pf}"$s" origin/{b} || exit 13
  fi
else
  git -C {r} worktree add --quiet --no-track -b {pf}"$s" "$P" origin/{b} || exit 13
fi
{exclude}
{report}"#,
            spare = SPARE_SUFFIX,
            fetch = fetch_branch_cmd(&r, base),
            exclude = exclude_snippet("\"$P\""),
            report = report_snippet("\"$P\""),
        );
        parse_report(&self.host.sh_ok(&script, None)?)
    }

    /// Check out the spare worktree (detached at `origin/<base>`) if it is
    /// missing. Slow on big repos; run it in the background.
    pub fn ensure_spare_worktree(&self, base: &str) -> Result<()> {
        let sp = shell_quote(&self.sibling_path(SPARE_SUFFIX));
        let script = format!(
            "[ -e {sp}/.git ] || git -C {r} worktree add --quiet --detach {sp} origin/{b}",
            r = shell_quote(self.path),
            b = shell_quote(base)
        );
        self.host.sh_ok(&script, None).map(|_| ())
    }

    /// Toggle write permission on every file and directory under `path`,
    /// leaving the worktree's `.git` link file alone.
    pub fn set_writable(&self, path: &str, writable: bool) -> Result<()> {
        let mode = if writable { "u+w" } else { "a-w" };
        let script = format!(
            "find {p} -path {p}/.git -prune -o -exec chmod {mode} {{}} + 2>/dev/null; true",
            p = crate::util::shell_quote(path)
        );
        self.host.sh_ok(&script, None).map(|_| ())
    }

    pub fn describe(&self, path: &str) -> Result<Worktree> {
        // git reports canonical paths (e.g. /private/var on macOS); ask it.
        let top = self.git_in(path, &["rev-parse", "--show-toplevel"])?.trim().to_string();
        self.worktrees()?
            .into_iter()
            .find(|w| w.path == path || w.path == top)
            .with_context(|| format!("worktree {path} missing after creation"))
    }

    pub fn remove_worktree(&self, path: &str, force: bool) -> Result<()> {
        let _ = self.set_writable(path, true);
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(path);
        self.git(&args).map(|_| ())
    }

    pub fn is_dirty(&self, dir: &str) -> Result<bool> {
        Ok(!self.git_in(dir, &["status", "--porcelain"])?.trim().is_empty())
    }

    pub fn head_sha(&self, dir: &str) -> Result<String> {
        Ok(self.git_in(dir, &["rev-parse", "HEAD"])?.trim().to_string())
    }

    pub fn current_branch(&self, dir: &str) -> Option<String> {
        let b = self.git_in(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok()?;
        Some(b.trim().to_string()).filter(|s| !s.is_empty())
    }

    pub fn merge_base(&self, dir: &str, a: &str, b: &str) -> Result<String> {
        Ok(self.git_in(dir, &["merge-base", a, b])?.trim().to_string())
    }

    /// Files changed between `base` and the worktree (committed + uncommitted),
    /// largest change first.
    pub fn changed_files(&self, dir: &str, base: &str) -> Result<Vec<ChangedFile>> {
        let status = self.git_in(dir, &["diff", "-M", "--name-status", "-z", base])?;
        let numstat = self.git_in(dir, &["diff", "-M", "--numstat", "-z", base])?;
        let mut files = parse_name_status_z(&status);
        let stats = parse_numstat_z(&numstat);
        for f in &mut files {
            if let Some((a, r)) = stats.get(&f.path) {
                f.added = *a;
                f.removed = *r;
            }
        }
        // Untracked files count as added.
        let untracked = self.git_in(dir, &["ls-files", "--others", "--exclude-standard", "-z"])?;
        for p in untracked.split('\0').filter(|p| !p.is_empty()) {
            if !files.iter().any(|f| f.path == p) {
                files.push(ChangedFile { path: p.to_string(), old_path: None, status: 'A', added: 0, removed: 0 });
            }
        }
        files.sort_by(|a, b| (b.added + b.removed).cmp(&(a.added + a.removed)).then(a.path.cmp(&b.path)));
        Ok(files)
    }

    /// Every tracked (and untracked-but-not-ignored) file in the worktree.
    pub fn ls_files(&self, dir: &str) -> Result<Vec<String>> {
        let out = self.git_in(dir, &["ls-files", "--cached", "--others", "--exclude-standard", "-z"])?;
        let mut v: Vec<String> = out.split('\0').filter(|p| !p.is_empty()).map(String::from).collect();
        v.sort();
        v.dedup();
        Ok(v)
    }

    /// File content at `rev` (`None` when the path does not exist there).
    pub fn show(&self, dir: &str, rev: &str, path: &str) -> Result<Option<String>> {
        let spec = format!("{rev}:{path}");
        let out = self.host.run(&["git", "-C", dir, "show", &spec], None)?;
        Ok(if out.ok() { Some(out.stdout) } else { None })
    }
}

/// Fetch exactly one branch into its remote-tracking ref: no tags, no
/// FETCH_HEAD, no other refs (big repos have thousands). `repo` is quoted.
fn fetch_branch_cmd(repo: &str, branch: &str) -> String {
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    format!("git -C {repo} fetch --quiet --no-tags --no-write-fetch-head origin {}", shell_quote(&refspec))
}

/// Shell lines that append `.kitchen/` to the repo's shared `info/exclude`
/// (idempotent). `dir` is an already-quoted worktree path.
pub fn exclude_snippet(dir: &str) -> String {
    format!(
        r#"f="$(git -C {dir} rev-parse --path-format=absolute --git-common-dir)/info/exclude"; mkdir -p "$(dirname "$f")"; grep -qxF '.kitchen/' "$f" 2>/dev/null || echo '.kitchen/' >> "$f""#
    )
}

/// Shell lines printing `KT_PATH=`/`KT_HEAD=`/`KT_BRANCH=` for [`parse_report`].
fn report_snippet(dir: &str) -> String {
    format!(
        r#"echo "KT_PATH=$(git -C {dir} rev-parse --show-toplevel)"; echo "KT_HEAD=$(git -C {dir} rev-parse HEAD)"; echo "KT_BRANCH=$(git -C {dir} symbolic-ref --quiet --short HEAD)""#
    )
}

fn parse_report(out: &str) -> Result<Worktree> {
    let mut wt = Worktree { path: String::new(), head: String::new(), branch: None, detached: false };
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("KT_PATH=") {
            wt.path = v.to_string();
        } else if let Some(v) = line.strip_prefix("KT_HEAD=") {
            wt.head = v.to_string();
        } else if let Some(v) = line.strip_prefix("KT_BRANCH=") {
            wt.branch = Some(v.to_string()).filter(|b| !b.is_empty());
        }
    }
    wt.detached = wt.branch.is_none();
    if wt.path.is_empty() {
        bail!("worktree script produced no path: {out}");
    }
    Ok(wt)
}

pub fn parse_github_slug(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.split_once("github.com/").map(|(_, r)| r))?;
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

pub fn parse_worktree_porcelain(s: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    for block in s.split("\n\n") {
        let mut wt = Worktree { path: String::new(), head: String::new(), branch: None, detached: false };
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                wt.path = p.to_string();
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                wt.head = h.to_string();
            } else if let Some(b) = line.strip_prefix("branch ") {
                wt.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            } else if line == "detached" {
                wt.detached = true;
            }
        }
        if !wt.path.is_empty() {
            out.push(wt);
        }
    }
    out
}

fn parse_name_status_z(s: &str) -> Vec<ChangedFile> {
    let mut out = Vec::new();
    let mut it = s.split('\0').filter(|p| !p.is_empty());
    while let Some(code) = it.next() {
        let status = code.chars().next().unwrap_or('M');
        if status == 'R' || status == 'C' {
            let old = it.next().unwrap_or_default().to_string();
            let new = it.next().unwrap_or_default().to_string();
            out.push(ChangedFile { path: new, old_path: Some(old), status: 'R', added: 0, removed: 0 });
        } else if let Some(p) = it.next() {
            out.push(ChangedFile { path: p.to_string(), old_path: None, status, added: 0, removed: 0 });
        }
    }
    out
}

fn parse_numstat_z(s: &str) -> std::collections::HashMap<String, (u32, u32)> {
    // Format: "<a>\t<r>\t<path>\0" or for renames "<a>\t<r>\t\0<old>\0<new>\0".
    let mut out = std::collections::HashMap::new();
    let mut it = s.split('\0');
    while let Some(rec) = it.next() {
        if rec.is_empty() {
            continue;
        }
        let mut f = rec.splitn(3, '\t');
        let a = f.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let r = f.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let path = f.next().unwrap_or_default();
        if path.is_empty() {
            let _old = it.next();
            if let Some(new) = it.next() {
                out.insert(new.to_string(), (a, r));
            }
        } else {
            out.insert(path.to_string(), (a, r));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_parsing() {
        assert_eq!(parse_github_slug("https://github.com/ppl-ai/air.git").as_deref(), Some("ppl-ai/air"));
        assert_eq!(parse_github_slug("git@github.com:ppl-ai/air.git").as_deref(), Some("ppl-ai/air"));
        assert_eq!(parse_github_slug("https://x-token:abc@github.com/a/b").as_deref(), Some("a/b"));
        assert_eq!(parse_github_slug("https://gitlab.com/a/b"), None);
    }

    #[test]
    fn worktree_porcelain() {
        let s = "worktree /r\nHEAD abc\nbranch refs/heads/main\n\nworktree /r-x\nHEAD def\ndetached\n";
        let w = parse_worktree_porcelain(s);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].branch.as_deref(), Some("main"));
        assert!(w[1].detached);
    }

    #[test]
    fn name_status() {
        let s = "M\0a.rs\0R100\0old.rs\0new.rs\0A\0b.rs\0";
        let f = parse_name_status_z(s);
        assert_eq!(f.len(), 3);
        assert_eq!(f[1].path, "new.rs");
        assert_eq!(f[1].old_path.as_deref(), Some("old.rs"));
        let n = parse_numstat_z("1\t2\ta.rs\0003\t4\t\0old.rs\0new.rs\0");
        assert_eq!(n["a.rs"], (1, 2));
        assert_eq!(n["new.rs"], (3, 4));
    }

    fn sh(dir: &std::path::Path, script: &str) {
        let ok = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).status().unwrap().success();
        assert!(ok, "{script}");
    }

    /// Full worktree lifecycle against a real local origin.
    #[test]
    fn worktree_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        sh(
            root,
            "git init -q -b main origin && cd origin && git config user.email t@t && git config user.name t \
             && echo one > a.txt && git add . && git commit -qm init && cd .. && git clone -q origin repo",
        );
        let host = Host::Local;
        let repo_path = root.join("repo").display().to_string();
        let repo = Repo::new(&host, &repo_path);
        repo.fetch().unwrap();
        assert_eq!(repo.default_branch().unwrap(), "main");

        let edit = repo.sibling_path("feature");
        let wt = repo.add_worktree_new_branch("william/feature", &edit, "main").unwrap();
        assert_eq!(wt.branch.as_deref(), Some("william/feature"));
        std::fs::write(format!("{edit}/b.txt"), "two\n").unwrap();
        let changed = repo.changed_files(&edit, "origin/main").unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, "b.txt");

        let ro = repo.readonly_path();
        repo.sync_readonly_worktree(&ro, "origin/main").unwrap();
        assert!(std::fs::write(format!("{ro}/a.txt"), "x").is_err(), "read-only worktree must reject writes");
        // Advance origin and resync: the read-only tree follows.
        sh(root, "cd origin && echo three > c.txt && git add . && git commit -qm more");
        repo.fetch().unwrap();
        repo.sync_readonly_worktree(&ro, "origin/main").unwrap();
        assert!(std::path::Path::new(&format!("{ro}/c.txt")).exists());

        // Re-opening an existing branch reuses its worktree.
        let again = repo.add_worktree_existing_branch("william/feature", "/nonexistent").unwrap();
        assert!(again.path.ends_with("/repo-feature"), "{}", again.path);
        repo.remove_worktree(&ro, true).unwrap();

        // One-shot edit worktree: slug collisions get -2, .kitchen/ excluded;
        // the first takes over the spare worktree, the second checks out fresh.
        repo.ensure_spare_worktree("main").unwrap();
        let spare = repo.sibling_path(SPARE_SUFFIX);
        assert!(std::path::Path::new(&spare).join("c.txt").exists());
        let a = repo.create_edit_worktree("main", "william/", "fast").unwrap();
        assert!(!std::path::Path::new(&spare).exists(), "spare was moved into the new tab");
        assert!(std::path::Path::new(&a.path).join("c.txt").exists());
        let b = repo.create_edit_worktree("main", "william/", "fast").unwrap();
        assert!(a.path.ends_with("/repo-fast") && b.path.ends_with("/repo-fast-2"), "{} {}", a.path, b.path);
        assert_eq!(b.branch.as_deref(), Some("william/fast-2"));
        let excl = std::fs::read_to_string(root.join("repo/.git/info/exclude")).unwrap();
        assert_eq!(excl.matches(".kitchen/").count(), 1);
        // Read-only sync with a fetch in the same round trip.
        let ro2 = repo.sync_readonly_worktree_fetching(&repo.readonly_path(), "origin/main", Some("main")).unwrap();
        assert!(ro2.detached);
    }
}
