//! Git operations, always exec'd through a [`Host`] so local and devbox
//! repositories behave the same.
//!
//! Worktree layout follows the existing convention on both machines:
//! siblings of the main checkout named `<repo>-<slug>`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::host::Host;
use crate::util::shell_quote;

/// A spare detached worktree kept checked out next to the repo, so a new
/// tab can take it over instead of paying for a full checkout.
pub const SPARE_SUFFIX: &str = "kitchen-spare";

/// Spare worktrees kept per repo (each is a full checkout).
pub const SPARE_COUNT: usize = 2;

/// `git worktree lock` reason on a spare while it is being fast-forwarded.
const REFRESH_LOCK: &str = "kitchen-refresh";

/// Prefix for background git work: below the agents' CPU and I/O priority.
const NICE: &str = "nice -n 10 $(command -v ionice >/dev/null && echo ionice -c2 -n7)";

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

/// What [`Repo::checkout_worktree`] puts in a worktree.
#[derive(Debug, Clone, Copy)]
pub enum Checkout<'a> {
    /// A branch: the local one, else a new one tracking `origin/<branch>`.
    Branch(&'a str),
    /// A detached HEAD at a rev (a PR whose branch isn't on origin).
    Detached(&'a str),
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

    /// New branch off the freshly fetched default branch, in its own worktree.
    pub fn add_worktree_new_branch(&self, branch: &str, path: &str, base: &str) -> Result<Worktree> {
        let start = format!("origin/{base}");
        self.git(&["worktree", "add", "--no-track", "-b", branch, path, &start])?;
        self.describe(path)
    }

    /// Worktree with `to` checked out, after fetching `fetch` refspecs
    /// (`+src:dst`) from origin; one round trip, `.kitchen/` git-excluded.
    /// A branch already checked out somewhere is used where it is, else the
    /// worktree goes at `path`, taking over a spare when there is one (a
    /// checkout of a big repo from scratch takes seconds).
    ///
    /// An existing worktree is moved forward, never reset: a branch is
    /// fast-forwarded to its upstream, and a detached worktree is checked out
    /// at the rev or switched onto the branch. git refuses whatever would
    /// overwrite local changes. Also says whether HEAD moved (a new worktree
    /// counts as moved).
    pub fn checkout_worktree(&self, path: &str, to: Checkout, fetch: &[String]) -> Result<(Worktree, bool)> {
        let r = shell_quote(self.path);
        let fetch = if fetch.is_empty() { String::new() } else { format!("{} || exit 11\n", fetch_cmd(&r, fetch)) };
        let checkout = match to {
            Checkout::Branch(branch) => format!(
                r#"b={b}
W=$(git -C {r} worktree list --porcelain | awk -v ref="branch refs/heads/$b" '/^worktree /{{w=substr($0,10)}} $0==ref{{print w; exit}}')
[ -n "$W" ] && P=$W
if git -C {r} rev-parse --verify --quiet "refs/heads/$b" >/dev/null; then local=1
elif git -C {r} rev-parse --verify --quiet "refs/remotes/origin/$b" >/dev/null; then local=
else echo "branch $b exists neither locally nor on origin" >&2; exit 14; fi
# --track needs a fetch refspec covering origin/$b; single-branch clones only map main.
[ -n "$local" ] || git -C {r} config --get-all remote.origin.fetch | grep -qxF -e '+refs/heads/*:refs/remotes/origin/*' -e "+refs/heads/$b:refs/remotes/origin/$b" \
  || git -C {r} remote set-branches --add origin "$b" || exit 13
co() {{ if [ -n "$local" ]; then git -C "$1" checkout --quiet "$b"; else git -C "$1" checkout --quiet --track -b "$b" "origin/$b"; fi; }}
before=$(git -C "$P" rev-parse HEAD 2>/dev/null)
if [ -e "$P" ]; then
  git -C "$P" rev-parse --show-toplevel >/dev/null 2>&1 || {{ echo "$P exists but is not a git worktree" >&2; exit 12; }}
  cur=$(git -C "$P" symbolic-ref --quiet --short HEAD)
  if [ "$cur" = "$b" ]; then
    if git -C "$P" rev-parse --verify --quiet '@{{u}}' >/dev/null; then git -C "$P" merge --quiet --ff-only '@{{u}}' || exit 13; fi
  elif [ -z "$cur" ]; then
    co "$P" || exit 13
  else
    echo "$P is on branch $cur, not $b" >&2; exit 12
  fi
elif take_spare "$P"; then
  co "$P" || {{ give_back "$P"; exit 13; }}
elif [ -n "$local" ]; then
  git -C {r} worktree add --quiet "$P" "$b" || exit 13
else
  git -C {r} worktree add --quiet --track -b "$b" "$P" "origin/$b" || exit 13
fi"#,
                b = shell_quote(branch),
            ),
            Checkout::Detached(rev) => format!(
                r#"git -C {r} rev-parse --verify --quiet {rv} >/dev/null || {{ echo "unknown revision {rev}" >&2; exit 14; }}
before=$(git -C "$P" rev-parse HEAD 2>/dev/null)
if [ -e "$P" ]; then
  git -C "$P" rev-parse --show-toplevel >/dev/null 2>&1 || {{ echo "$P exists but is not a git worktree" >&2; exit 12; }}
  git -C "$P" checkout --quiet --detach {rv} || exit 13
elif take_spare "$P"; then
  git -C "$P" checkout --quiet --detach {rv} || {{ give_back "$P"; exit 13; }}
else
  git -C {r} worktree add --quiet --detach "$P" {rv} || exit 13
fi"#,
                rv = shell_quote(&format!("{rev}^{{commit}}")),
            ),
        };
        let script = format!(
            r#"{fetch}P={p}
{take_spare}
{checkout}
{exclude}
[ "$before" = "$(git -C "$P" rev-parse HEAD)" ] && echo KT_MOVED=0 || echo KT_MOVED=1
{report}"#,
            p = shell_quote(path),
            take_spare = take_spare_snippet(&r, &self.spare_paths()),
            exclude = exclude_snippet("\"$P\""),
            report = report_snippet("\"$P\""),
        );
        let out = self.host.sh_ok(&script, None)?;
        Ok((parse_report(&out)?, out.lines().any(|l| l == "KT_MOVED=1")))
    }

    /// New branch `<prefix><slug>` (first free slug: `slug`, `slug-2`, …)
    /// off `origin/<base>` as last fetched (the app's spare warmer keeps it
    /// fresh), checked out in the sibling worktree `<repo>-<slug>`, with
    /// `.kitchen/` git-excluded. One round trip to the host, no network.
    pub fn create_edit_worktree(&self, base: &str, prefix: &str, slug: &str) -> Result<Worktree> {
        // `<repo>-kitchen-*` names belong to the kitchen (spares, PR tabs).
        let slug = if slug.starts_with("kitchen") { format!("tab-{slug}") } else { slug.to_string() };
        let (r, b, pf, sl) = (shell_quote(self.path), shell_quote(base), shell_quote(prefix), shell_quote(&slug));
        let parent = shell_quote(self.parent_dir());
        let name = shell_quote(self.repo_name());
        let script = format!(
            r#"s={sl}; n=1
while [ -e {parent}/{name}-"$s" ] || git -C {r} rev-parse --verify --quiet "refs/heads/"{pf}"$s" >/dev/null; do
  n=$((n+1)); s={sl}-$n
done
P={parent}/{name}-"$s"
{take_spare}
# Take over a pre-checked-out spare (instant), else check out from scratch.
if take_spare "$P"; then
  if [ "$(git -C "$P" rev-parse HEAD)" = "$(git -C {r} rev-parse origin/{b})" ]; then
    # Same commit: pure ref updates, no index refresh (~0.6s saved on big repos).
    git -C "$P" branch --no-track {pf}"$s" origin/{b} && git -C "$P" symbolic-ref HEAD refs/heads/{pf}"$s" || {{ give_back "$P"; exit 13; }}
  else
    git -C "$P" checkout --quiet --no-track -b {pf}"$s" origin/{b} || {{ git -C {r} branch -D {pf}"$s" >/dev/null 2>&1; give_back "$P"; exit 13; }}
  fi
else
  git -C {r} worktree add --quiet --no-track -b {pf}"$s" "$P" origin/{b} || exit 13
fi
{exclude}
echo "KT_TAKEN=$TAKEN"
{report}"#,
            take_spare = take_spare_snippet(&r, &self.spare_paths()),
            exclude = exclude_snippet("\"$P\""),
            report = report_snippet("\"$P\""),
        );
        let out = self.host.sh(&script, None)?.into_result("create edit worktree")?;
        if out.lines().any(|l| l == "KT_TAKEN=") {
            log::info!(target: "kitchen::exec", "{}: no free spare, full checkout", self.repo_name());
        }
        parse_report(&out)
    }

    /// The spare worktrees, first preferred: `<repo>-kitchen-spare`,
    /// `<repo>-kitchen-spare-2`, …
    pub fn spare_paths(&self) -> Vec<String> {
        (1..=SPARE_COUNT)
            .map(|n| if n == 1 { self.sibling_path(SPARE_SUFFIX) } else { self.sibling_path(&format!("{SPARE_SUFFIX}-{n}")) })
            .collect()
    }

    /// Keep [`SPARE_COUNT`] spares detached at `origin/<base>`: check out
    /// missing ones and fast-forward stale ones, so taking one over is pure
    /// ref updates. A spare being refreshed is `git worktree lock`ed, which
    /// makes takeovers skip it. Worktrees on a branch are never touched.
    /// With `fetch`, `origin/<base>` is fetched first in the same round trip
    /// (retried once: a concurrent fetch may hold the ref lock); a fetch that
    /// still fails is logged and the spares go to the local ref anyway. The
    /// fetch never prompts and gives up on a stalled transfer, so it cannot
    /// hang the caller's refresh forever.
    /// Slow on big repos and niced; run it in the background, one at a time.
    pub fn ensure_spare_worktree(&self, base: &str, fetch: bool) -> Result<()> {
        let (r, b) = (shell_quote(self.path), shell_quote(base));
        let spares: Vec<String> = self.spare_paths().iter().map(|p| shell_quote(p)).collect();
        let fetch = if fetch {
            format!(
                "f() {{ GIT_TERMINAL_PROMPT=0 {NICE} git -c http.lowSpeedLimit=1000 -c http.lowSpeedTime=30 {}; }}\nf || {{ sleep 1; f; }} || echo KT_FETCH_FAILED\n",
                fetch_cmd(&r, &[branch_refspec(base)]).trim_start_matches("git ")
            )
        } else {
            String::new()
        };
        let script = format!(
            r#"{fetch}err=
for SP in {spares}; do
  if [ ! -e "$SP/.git" ]; then
    [ -e "$SP" ] && continue
    # (--force: the folder may be gone while git still has it registered)
    {NICE} git -C {r} worktree add --force --quiet --detach "$SP" origin/{b} || err=13
    continue
  fi
  A="$(git -C "$SP" rev-parse --absolute-git-dir)"
  # A lock of ours outlived its refresh (the app quit mid-way).
  [ "$(cat "$A/locked" 2>/dev/null)" = {reason} ] && git -C {r} worktree unlock "$SP"
  git -C "$SP" symbolic-ref -q HEAD >/dev/null && continue
  # Nothing works in an unlocked spare but us: an index.lock is left over
  # from a git that was killed; reset it to HEAD before going on.
  [ -e "$A/locked" ] || [ ! -e "$A/index.lock" ] || {{ rm -f "$A/index.lock"; git -C "$SP" reset -q --hard HEAD 2>/dev/null; }}
  [ "$(git -C "$SP" rev-parse HEAD)" = "$(git -C {r} rev-parse origin/{b})" ] && continue
  git -C {r} worktree lock --reason {reason} "$SP" 2>/dev/null || continue
  {NICE} git -C "$SP" checkout --quiet --detach origin/{b}; rc=$?
  git -C {r} worktree unlock "$SP"
  [ $rc = 0 ] || err=$rc
done
exit ${{err:-0}}"#,
            spares = spares.join(" "),
            reason = REFRESH_LOCK,
        );
        let out = self.host.sh_ok(&script, None)?;
        if out.lines().any(|l| l == "KT_FETCH_FAILED") {
            log::warn!("{}: fetch of origin/{base} failed; spares left at the local ref", self.repo_name());
        }
        Ok(())
    }

    /// Once per workspace setup: turn on git's untracked-file cache unless the
    /// repo configures it (big repos: `git status` stops walking every
    /// directory), and run the cheap maintenance tasks, niced. No repacking:
    /// partial clones keep their promisor packs as they are. Returns what it
    /// changed, for the log.
    pub fn tune(&self) -> Result<Vec<String>> {
        let script = format!(
            r#"git config --get core.untrackedCache >/dev/null || {{ git config core.untrackedCache true && echo "set core.untrackedCache=true"; }}
{NICE} git maintenance run --task=loose-objects --task=commit-graph >/dev/null 2>&1 && echo "maintenance: loose-objects, commit-graph"
true"#
        );
        let out = self.host.sh_ok(&script, Some(self.path))?;
        Ok(out.lines().map(String::from).filter(|l| !l.is_empty()).collect())
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

    /// Everything the file viewer needs but the tracked-file list, in one
    /// round trip: HEAD, a checksum of the index's file list (~60 ms on agi),
    /// the merge base of HEAD and `base_ref`, and the files
    /// changed since it, untracked files included (as added).
    pub fn listing(&self, dir: &str, base_ref: &str) -> Result<Listing> {
        let (d, b) = (shell_quote(dir), shell_quote(base_ref));
        let script = format!(
            "cd {d} || exit 3\n\
             git rev-parse HEAD | tr -d '\\n'; printf '\\036'\n\
             git ls-files --cached -z | cksum | tr -d '\\n'; printf '\\036'\n\
             base=$(git merge-base HEAD {b} 2>/dev/null) || base={b}\n\
             printf '%s' \"$base\"; printf '\\036'\n\
             git diff -M --name-status -z \"$base\"; printf '\\036'\n\
             git diff -M --numstat -z \"$base\"; printf '\\036'\n\
             git ls-files --others --exclude-standard -z\n"
        );
        let out = self.host.sh_ok(&script, None).with_context(|| format!("list files in {dir}"))?;
        parse_listing(&out)
    }

    /// Every file in the worktree's index, sorted (agi: 136k paths, 10 MB,
    /// sent gzipped from devboxes: ~1.2 MB).
    pub fn tracked_files(&self, dir: &str) -> Result<Vec<String>> {
        let raw = match self.host {
            Host::Local => self.git_in(dir, &["ls-files", "--cached", "-z"])?.into_bytes(),
            Host::Ssh(_) => {
                let script = format!(
                    "f=$(mktemp) || exit 3\n\
                     git -C {d} ls-files --cached -z > \"$f\" || {{ rm -f \"$f\"; exit 4; }}\n\
                     if command -v gzip >/dev/null; then gzip -1c \"$f\"; else cat \"$f\"; fi; rc=$?\n\
                     rm -f \"$f\"; exit $rc",
                    d = shell_quote(dir)
                );
                gunzip_if_gzipped(self.host.sh_bytes_ok(&script, None).with_context(|| format!("list tracked files in {dir}"))?)?
            }
        };
        let text = String::from_utf8_lossy(&raw);
        Ok(text.split('\0').filter(|p| !p.is_empty()).map(String::from).collect())
    }

    /// Files of worktree `dir` (tracked, and untracked but not ignored) in a
    /// folder of `dir` whose path ends with `suffix` on whole components
    /// (`a.md` matches `docs/a.md`, not `docs/xa.md` or the root's `a.md`),
    /// relative to `dir`. Matched in the index, so tracked folders are not
    /// walked; the untracked scan costs ~0.5 s on agi.
    pub fn find_by_suffix(&self, dir: &str, suffix: &str) -> Result<Vec<String>> {
        let pattern = format!("*/{suffix}");
        let out = self.git_in(dir, &["ls-files", "--cached", "--others", "--exclude-standard", "-z", "--", &pattern])?;
        let mut files: Vec<String> = out.split('\0').filter(|p| !p.is_empty()).map(String::from).collect();
        // An unmerged path is listed once per stage.
        files.dedup();
        Ok(files)
    }

    /// File content at `rev` (`None` when the path does not exist there).
    pub fn show(&self, dir: &str, rev: &str, path: &str) -> Result<Option<String>> {
        let spec = format!("{rev}:{path}");
        let out = self.host.run(&["git", "-C", dir, "show", &spec], None)?;
        Ok(if out.ok() { Some(out.stdout) } else { None })
    }
}

/// `+refs/heads/<b>:refs/remotes/origin/<b>`.
pub fn branch_refspec(branch: &str) -> String {
    format!("+refs/heads/{branch}:refs/remotes/origin/{branch}")
}

/// `git fetch origin <refspecs…>` in the already-quoted `repo`: no tags, no
/// FETCH_HEAD, no other refs (big repos have thousands).
fn fetch_cmd(repo: &str, refspecs: &[String]) -> String {
    let specs: Vec<String> = refspecs.iter().map(|s| shell_quote(s)).collect();
    format!("git -C {repo} fetch --quiet --no-tags --no-write-fetch-head origin {}", specs.join(" "))
}

/// Shell lines that append `.kitchen/` to the repo's shared `info/exclude`
/// (idempotent). `dir` is an already-quoted worktree path.
pub fn exclude_snippet(dir: &str) -> String {
    format!(
        r#"f="$(git -C {dir} rev-parse --path-format=absolute --git-common-dir)/info/exclude"; mkdir -p "$(dirname "$f")"; grep -qxF '.kitchen/' "$f" 2>/dev/null || echo '.kitchen/' >> "$f""#
    )
}

/// Shell functions `take_spare DEST`, which moves the first available spare
/// to DEST and succeeds only if one was moved, and `give_back DEST`, which
/// moves it back (a checkout after the takeover failed). `git worktree move`
/// refuses a locked spare (being refreshed); spares on a branch are not
/// spares, and one with an `index.lock` (a git killed mid-way) is left to
/// the next refresh. `repo` is quoted.
fn take_spare_snippet(repo: &str, spares: &[String]) -> String {
    let spares: Vec<String> = spares.iter().map(|p| shell_quote(p)).collect();
    format!(
        r#"TAKEN=
take_spare() {{ for SP in {}; do [ -e "$SP/.git" ] || continue; git -C "$SP" symbolic-ref -q HEAD >/dev/null && continue; [ -e "$(git -C "$SP" rev-parse --absolute-git-dir)/index.lock" ] && continue; git -C {repo} worktree move "$SP" "$1" 2>/dev/null && TAKEN=$SP && return 0; done; return 1; }}
give_back() {{ [ -n "$TAKEN" ] && git -C {repo} worktree move "$1" "$TAKEN" 2>/dev/null; true; }}"#,
        spares.join(" ")
    )
}

/// `bytes`, gunzipped when they are a gzip stream.
fn gunzip_if_gzipped(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len() * 8);
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&bytes[..]), &mut out).context("gunzip")?;
    Ok(out)
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

/// Result of [`Repo::listing`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Listing {
    /// The worktree's HEAD.
    pub head: String,
    /// Checksum of the index's file list: [`Repo::tracked_files`] is the
    /// same while it is.
    pub index: String,
    /// Merge base of HEAD and the base ref (the base ref itself when there is none).
    pub base: String,
    /// Largest change first; untracked files count as added.
    pub changed: Vec<ChangedFile>,
}

fn parse_listing(out: &str) -> Result<Listing> {
    let parts: Vec<&str> = out.split('\x1e').collect();
    let [head, index, base, status, numstat, others] = parts[..] else {
        bail!("unexpected listing output ({} sections)", parts.len());
    };
    let mut changed = parse_name_status_z(status);
    let stats = parse_numstat_z(numstat);
    for f in &mut changed {
        if let Some((a, r)) = stats.get(&f.path) {
            f.added = *a;
            f.removed = *r;
        }
    }
    for p in others.split('\0').filter(|p| !p.is_empty()) {
        if !changed.iter().any(|f| f.path == p) {
            changed.push(ChangedFile { path: p.to_string(), old_path: None, status: 'A', added: 0, removed: 0 });
        }
    }
    changed.sort_by(|a, b| (b.added + b.removed).cmp(&(a.added + a.removed)).then(a.path.cmp(&b.path)));
    Ok(Listing { head: head.trim().to_string(), index: index.trim().to_string(), base: base.trim().to_string(), changed })
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

    #[test]
    fn one_trip_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        sh(
            root,
            "git init -q -b main origin && cd origin && git config user.email t@t && git config user.name t \
             && mkdir -p aster/src && echo one > aster/src/a.rs && echo x > top.md && echo y > 'aster/ünï cødé.md' \
             && git add . && git commit -qm init \
             && cd .. && git clone -q origin repo && cd repo && echo two >> aster/src/a.rs && echo new > 'aster/sp ace.rs'",
        );
        let dir = root.join("repo").display().to_string();
        let repo = Repo::new(&Host::Local, &dir);
        let l = repo.listing(&dir, "origin/main").unwrap();
        assert_eq!(l.head.len(), 40, "{:?}", l.head);
        assert_eq!(l.base, l.head, "nothing committed: the merge base is HEAD");
        assert!(!l.index.is_empty());
        sh(root.join("repo").as_path(), "git add 'aster/sp ace.rs'");
        let staged = repo.listing(&dir, "origin/main").unwrap();
        assert_ne!(staged.index, l.index, "staging a file changes the index checksum");
        assert_eq!(staged.head, l.head);
        sh(root.join("repo").as_path(), "git rm -q --cached 'aster/sp ace.rs'");
        assert_eq!(repo.listing(&dir, "origin/main").unwrap().index, l.index);
        let changed: Vec<&str> = l.changed.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(changed, vec!["aster/src/a.rs", "aster/sp ace.rs"]);
        assert_eq!(l.changed[0].added, 1);
        assert_eq!(repo.tracked_files(&dir).unwrap(), vec!["aster/src/a.rs", "aster/ünï cødé.md", "top.md"]);
        for (suffix, want) in [
            ("a.rs", vec!["aster/src/a.rs"]),
            ("src/a.rs", vec!["aster/src/a.rs"]),
            ("rc/a.rs", vec![]),
            ("sp ace.rs", vec!["aster/sp ace.rs"]),
            ("top.md", vec![]),
        ] {
            assert_eq!(repo.find_by_suffix(&dir, suffix).unwrap(), want, "suffix={suffix:?}");
        }
    }

    #[test]
    fn gunzip_passthrough_and_gzip() {
        assert_eq!(gunzip_if_gzipped(b"a\0b\0".to_vec()).unwrap(), b"a\0b\0");
        let gz = std::process::Command::new("sh").arg("-c").arg("printf 'a\\000b\\000' | gzip -1c").output().unwrap().stdout;
        assert_eq!(gunzip_if_gzipped(gz).unwrap(), b"a\0b\0");
    }

    fn sh(dir: &std::path::Path, script: &str) {
        let ok = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).status().unwrap().success();
        assert!(ok, "{script}");
    }

    fn exists(p: &str) -> bool {
        std::path::Path::new(p).exists()
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

        sh(root, "cd origin && echo three > c.txt && git add . && git commit -qm more");
        repo.fetch().unwrap();

        // Spares: two, detached at origin/main. New tabs take them in turn
        // (slug collisions get -2, .kitchen/ excluded), then check out fresh.
        repo.ensure_spare_worktree("main", false).unwrap();
        let spares = repo.spare_paths();
        assert_eq!(spares.len(), SPARE_COUNT);
        assert!(spares.iter().all(|s| exists(&format!("{s}/c.txt"))));
        let a = repo.create_edit_worktree("main", "william/", "fast").unwrap();
        assert!(!exists(&spares[0]) && exists(&spares[1]), "first spare moved into the tab");
        assert!(exists(&format!("{}/c.txt", a.path)));
        let b = repo.create_edit_worktree("main", "william/", "fast").unwrap();
        assert!(!exists(&spares[1]), "second spare moved into the second tab");
        let c = repo.create_edit_worktree("main", "william/", "fast").unwrap();
        assert!(a.path.ends_with("/repo-fast") && b.path.ends_with("/repo-fast-2") && c.path.ends_with("/repo-fast-3"), "{} {} {}", a.path, b.path, c.path);
        assert_eq!(b.branch.as_deref(), Some("william/fast-2"));
        let excl = std::fs::read_to_string(root.join("repo/.git/info/exclude")).unwrap();
        assert_eq!(excl.matches(".kitchen/").count(), 1);
        // Kitchen-owned names are never handed to a tab.
        let k = repo.create_edit_worktree("main", "william/", "kitchen-spare-2").unwrap();
        assert!(k.path.ends_with("/repo-tab-kitchen-spare-2"), "{}", k.path);

        // Refresh re-creates missing spares and fast-forwards stale ones;
        // a spare someone put on a branch is left alone.
        repo.ensure_spare_worktree("main", false).unwrap();
        sh(root, "cd origin && echo five > f.txt && git add . && git commit -qm five");
        repo.fetch().unwrap();
        sh(std::path::Path::new(&spares[1]), "git switch -qc mine");
        repo.ensure_spare_worktree("main", false).unwrap();
        assert!(exists(&format!("{}/f.txt", spares[0])), "stale spare fast-forwarded");
        assert!(!exists(&format!("{}/f.txt", spares[1])), "branch worktree untouched");
        let d = repo.create_edit_worktree("main", "william/", "skip").unwrap();
        assert!(exists(&spares[1]), "a spare on a branch is not taken over");
        assert!(!exists(&spares[0]) && exists(&format!("{}/f.txt", d.path)));
        sh(std::path::Path::new(&spares[1]), "git switch -q --detach && git branch -qD mine");

        // A spare being refreshed (locked) is skipped: fresh checkout instead.
        sh(root, &format!("git -C repo worktree lock --reason {REFRESH_LOCK} {}", shell_quote(&spares[1])));
        let e = repo.create_edit_worktree("main", "william/", "locked").unwrap();
        assert!(exists(&spares[1]) && exists(&format!("{}/f.txt", e.path)));
        // ...and a stale lock of ours is cleared by the next refresh.
        repo.ensure_spare_worktree("main", false).unwrap();
        let f = repo.create_edit_worktree("main", "william/", "unlocked").unwrap();
        assert!(exists(&f.path));
        assert!(!exists(&spares[0]) || !exists(&spares[1]), "a spare was taken");

        // A spare with a leftover index.lock (a git killed mid-way) isn't
        // taken over; the next refresh clears it.
        repo.ensure_spare_worktree("main", false).unwrap();
        let lock = |sp: &str| format!("{}/index.lock", repo.git_in(sp, &["rev-parse", "--absolute-git-dir"]).unwrap().trim());
        let l0 = lock(&spares[0]);
        std::fs::write(&l0, "").unwrap();
        let g = repo.create_edit_worktree("main", "william/", "lockfile").unwrap();
        assert!(exists(&spares[0]) && exists(&l0), "spare with index.lock left alone");
        assert!(exists(&g.path));
        repo.ensure_spare_worktree("main", false).unwrap();
        assert!(!exists(&l0), "refresh cleared the stale index.lock");
        // A spare folder deleted behind git's back is re-created, and the
        // other spare is still refreshed.
        sh(root, &format!("rm -rf {}", shell_quote(&spares[1])));
        repo.ensure_spare_worktree("main", false).unwrap();
        assert!(spares.iter().all(|s| exists(&format!("{s}/.git"))), "deleted spare re-created");

        // New tabs never fetch: a commit on origin reaches them once a
        // spare refresh fetches it, which brings both spares along.
        sh(root, "cd origin && echo six > s.txt && git add . && git commit -qm six");
        let h = repo.create_edit_worktree("main", "william/", "nofetch").unwrap();
        assert!(!exists(&format!("{}/s.txt", h.path)), "no fetch on the edit-tab path");
        repo.ensure_spare_worktree("main", true).unwrap();
        assert!(spares.iter().all(|s| exists(&format!("{s}/s.txt"))), "fetched and fast-forwarded");
        // A failed fetch still brings the spares to the local origin/main.
        sh(root, "cd origin && echo seven > v.txt && git add . && git commit -qm seven");
        repo.fetch().unwrap();
        sh(root, "git -C repo remote set-url origin file:///nonexistent && cd origin && echo eight > e.txt && git add . && git commit -qm eight");
        repo.ensure_spare_worktree("main", true).unwrap();
        assert!(spares.iter().all(|s| exists(&format!("{s}/v.txt")) && !exists(&format!("{s}/e.txt"))), "fetch failure tolerated");
        sh(root, &format!("git -C repo remote set-url origin {}", shell_quote(&root.join("origin").display().to_string())));
    }

    /// Existing branches and PR heads, fetched and checked out in one trip,
    /// then moved forward (never reset) when opened again.
    #[test]
    fn checkout_worktree_branch_and_detached() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        sh(
            root,
            "git init -q -b main origin && cd origin && git config user.email t@t && git config user.name t \
             && echo one > a.txt && git add . && git commit -qm init \
             && git checkout -qb pr && echo pr > p.txt && git add . && git commit -qm pr \
             && git update-ref refs/pull/7/head HEAD && git checkout -q main \
             && cd .. && git clone -q origin repo && git -C repo config user.email t@t && git -C repo config user.name t",
        );
        let host = Host::Local;
        let repo_path = root.join("repo").display().to_string();
        let repo = Repo::new(&host, &repo_path);
        let read = |p: &str| std::fs::read_to_string(p).unwrap();

        // A branch only on origin: fetched, takes over a spare, writable.
        repo.ensure_spare_worktree("main", false).unwrap();
        let spares = repo.spare_paths();
        let path = repo.sibling_path("kitchen-pr-7");
        let fetch = [branch_refspec("pr")];
        let (wt, moved) = repo.checkout_worktree(&path, Checkout::Branch("pr"), &fetch).unwrap();
        assert!(moved && wt.branch.as_deref() == Some("pr") && exists(&format!("{path}/p.txt")));
        assert!(!exists(&spares[0]), "took over a spare");
        std::fs::write(format!("{path}/a.txt"), "local edit\n").unwrap();
        let excl = read(&format!("{repo_path}/.git/info/exclude"));
        assert_eq!(excl.matches(".kitchen/").count(), 1);

        // Same head: nothing moves. A new push fast-forwards, keeping local edits.
        let (_, moved) = repo.checkout_worktree(&path, Checkout::Branch("pr"), &fetch).unwrap();
        assert!(!moved);
        sh(root, "cd origin && git checkout -q pr && echo more >> p.txt && git commit -qam more && git checkout -q main");
        let (_, moved) = repo.checkout_worktree(&path, Checkout::Branch("pr"), &fetch).unwrap();
        assert!(moved);
        assert_eq!(read(&format!("{path}/p.txt")), "pr\nmore\n");
        assert_eq!(read(&format!("{path}/a.txt")), "local edit\n", "local edits survive");

        // Reused where it is checked out, whatever path is asked for.
        let (again, _) = repo.checkout_worktree("/nonexistent", Checkout::Branch("pr"), &[]).unwrap();
        assert!(again.path.ends_with("/repo-kitchen-pr-7"), "{}", again.path);

        // Diverged: the fast-forward fails and the tree is left alone.
        sh(std::path::Path::new(&path), "git commit -qam mine");
        sh(root, "cd origin && git checkout -q pr && echo other >> p.txt && git commit -qam other && git checkout -q main");
        assert!(repo.checkout_worktree(&path, Checkout::Branch("pr"), &fetch).unwrap_err().to_string().contains("exit 13"));
        assert_eq!(read(&format!("{path}/p.txt")), "pr\nmore\n");

        // Detached at a PR ref; a conflicting local edit stops a re-checkout.
        let det = repo.sibling_path("kitchen-pr-8");
        let specs = ["+refs/pull/7/head:refs/kitchen/pr/7".to_string()];
        let (wt, moved) = repo.checkout_worktree(&det, Checkout::Detached("refs/kitchen/pr/7"), &specs).unwrap();
        assert!(moved && wt.detached && exists(&format!("{det}/p.txt")));
        std::fs::write(format!("{det}/p.txt"), "edited\n").unwrap();
        sh(root, "cd origin && git checkout -q pr && git update-ref refs/pull/7/head HEAD && git checkout -q main");
        assert!(repo.checkout_worktree(&det, Checkout::Detached("refs/kitchen/pr/7"), &specs).unwrap_err().to_string().contains("exit 13"));
        assert_eq!(read(&format!("{det}/p.txt")), "edited\n");

        // A detached worktree asked for a branch is switched onto it.
        sh(std::path::Path::new(&det), "git checkout -q -- p.txt");
        sh(root, "cd origin && git branch -q other main");
        let (wt, _) = repo.checkout_worktree(&det, Checkout::Branch("other"), &[branch_refspec("other")]).unwrap();
        assert_eq!(wt.branch.as_deref(), Some("other"));

        // On another branch, or no such branch: refused.
        sh(root, "cd origin && git branch -q third main");
        let e = repo.checkout_worktree(&det, Checkout::Branch("third"), &[branch_refspec("third")]).unwrap_err();
        assert!(e.to_string().contains("exit 12"), "{e:#}");
        let e = repo.checkout_worktree(&det, Checkout::Branch("nope"), &[]).unwrap_err();
        assert!(e.to_string().contains("exit 14"), "{e:#}");
    }

    /// A single-branch clone (origin maps only main) can still open a
    /// branch that is only on origin, tracking it.
    #[test]
    fn checkout_worktree_in_single_branch_clone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        sh(
            root,
            "git init -q -b main origin && cd origin && git config user.email t@t && git config user.name t \
             && echo one > a.txt && git add . && git commit -qm init \
             && git checkout -qb pr && echo pr > p.txt && git add . && git commit -qm pr && git checkout -q main \
             && cd .. && git clone -q --single-branch origin repo",
        );
        let repo_path = root.join("repo").display().to_string();
        let repo = Repo::new(&Host::Local, &repo_path);
        let path = repo.sibling_path("kitchen-pr-7");
        let fetch = [branch_refspec("pr")];
        let (wt, _) = repo.checkout_worktree(&path, Checkout::Branch("pr"), &fetch).unwrap();
        assert_eq!(wt.branch.as_deref(), Some("pr"));
        assert_eq!(repo.git(&["-C", &path, "rev-parse", "--abbrev-ref", "@{u}"]).unwrap().trim(), "origin/pr");
        // Opening it again leaves origin's refspecs as they are.
        repo.checkout_worktree(&path, Checkout::Branch("pr"), &fetch).unwrap();
        let specs = repo.git(&["config", "--get-all", "remote.origin.fetch"]).unwrap();
        assert_eq!(specs.lines().count(), 2, "{specs}");
    }

    #[test]
    fn tune_sets_untracked_cache_once() {
        let tmp = tempfile::tempdir().unwrap();
        sh(tmp.path(), "git init -q r && cd r && git config user.email t@t && git config user.name t && echo a > a && git add . && git commit -qm i");
        let dir = tmp.path().join("r").display().to_string();
        let repo = Repo::new(&Host::Local, &dir);
        assert!(repo.tune().unwrap().iter().any(|l| l.contains("core.untrackedCache")));
        assert!(!repo.tune().unwrap().iter().any(|l| l.contains("core.untrackedCache")), "left alone once set");
        sh(tmp.path(), "cd r && git config core.untrackedCache false");
        repo.tune().unwrap();
        assert_eq!(repo.git(&["config", "core.untrackedCache"]).unwrap().trim(), "false", "user's choice kept");
    }
}
