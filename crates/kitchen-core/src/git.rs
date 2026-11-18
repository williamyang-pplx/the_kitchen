//! Git operations, always exec'd through a [`Host`] so local and devbox
//! repositories behave the same.
//!
//! Worktree layout follows the existing convention on both machines:
//! siblings of the main checkout named `<repo>-<slug>`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

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

/// Small packs (under [`SMALL_PACK_KIB`]) that make [`Repo::compact_packs`]
/// roll them into one. A partial clone keeps a pack per fetch and on-demand
/// download (a devbox's agi had 178k, ~660 more an hour), and every git
/// command pays for each one: 0.35 s each with them indexed, 2.6 s without.
const SMALL_PACKS: usize = 50;

/// Packs at least this big are kept as they are when the small ones are
/// rolled up, so a compaction rewrites at most about this much.
const SMALL_PACK_KIB: u64 = 64 * 1024;

/// How often [`Repo::listing`] scans for untracked files while neither HEAD
/// nor the index moves.
pub const UNTRACKED_EVERY: Duration = Duration::from_secs(30);

pub struct Repo<'a> {
    pub host: &'a Host,
    /// Absolute path of the main checkout.
    pub path: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: String,
    pub head: String,
    pub branch: Option<String>,
}

/// What [`Repo::checkout_worktree`] puts in a worktree.
#[derive(Debug, Clone, Copy)]
pub enum Checkout<'a> {
    /// A branch: the local one, else a new one tracking `origin/<branch>`.
    Branch(&'a str),
    /// A detached HEAD at a rev (a PR whose branch isn't on origin).
    Detached(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// `owner/name` for a GitHub origin, if origin is on GitHub.
    pub fn github_slug(&self) -> Option<String> {
        parse_github_slug(&self.git(&["remote", "get-url", "origin"]).ok()?)
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

    /// The branch the main checkout is on, even before its first commit
    /// (what a repo with nothing on origin will push as its default).
    pub fn head_branch(&self) -> Result<String> {
        let b = self.git(&["symbolic-ref", "--quiet", "--short", "HEAD"]).unwrap_or_default();
        let b = b.trim();
        if b.is_empty() {
            bail!("cannot determine default branch of {}: nothing on origin, and HEAD is detached", self.path)
        }
        Ok(b.to_string())
    }

    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        Ok(parse_worktree_porcelain(&self.git(&["worktree", "list", "--porcelain"])?))
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
        self.checkout_worktree_unless(path, to, fetch, &[])
    }

    /// [`Self::checkout_worktree`], skipping the fetch when every `(rev,
    /// commit)` in `have` already holds: `rev` names `commit` here (e.g. a
    /// PR's ref and base branch at the commits GitHub reports). A fetch from
    /// GitHub is seconds; most switches back to a PR find nothing new.
    pub fn checkout_worktree_unless(&self, path: &str, to: Checkout, fetch: &[String], have: &[(String, String)]) -> Result<(Worktree, bool)> {
        let out = self.host.sh_ok(&self.checkout_script(path, to, fetch, have), None)?;
        Ok((parse_report(&out)?, out.lines().any(|l| l == "KT_MOVED=1")))
    }

    /// [`Self::checkout_worktree_unless`], then [`Self::listing`] of the
    /// worktree against `base_ref` in the same round trip, so a viewer of it
    /// starts with its changes. The listing's memo comes back for the
    /// viewer's next listings.
    pub fn checkout_and_list(&self, path: &str, to: Checkout, fetch: &[String], have: &[(String, String)], base_ref: &str, mut memo: ListingMemo) -> Result<(Worktree, bool, Listing, ListingMemo)> {
        let started = Instant::now();
        let script = format!("{}\nprintf '\\035'\n{}", self.checkout_script(path, to, fetch, have), listing_script(path, base_ref, &memo));
        let out = self.host.sh_ok(&script, None)?;
        let (checkout, listing) = out.split_once('\x1d').with_context(|| format!("checkout of {path} printed no listing"))?;
        let listing = read_listing(listing, &mut memo, started).with_context(|| format!("list files in {path}"))?;
        Ok((parse_report(checkout)?, checkout.lines().any(|l| l == "KT_MOVED=1"), listing, memo))
    }

    fn checkout_script(&self, path: &str, to: Checkout, fetch: &[String], have: &[(String, String)]) -> String {
        let r = shell_quote(self.path);
        let fetch = match (fetch.is_empty(), have.is_empty()) {
            (true, _) => String::new(),
            (false, true) => format!("{} || exit 11\n", fetch_cmd(&r, fetch)),
            (false, false) => {
                let checks: Vec<String> = have
                    .iter()
                    .map(|(rev, commit)| format!("[ \"$(git -C {r} rev-parse --verify --quiet {})\" = {} ]", shell_quote(&format!("{rev}^{{commit}}")), shell_quote(commit)))
                    .collect();
                format!("if ! {{ {}; }}; then {} || exit 11; fi\n", checks.join(" && "), fetch_cmd(&r, fetch))
            }
        };
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
elif take_spare "$P" && co "$P"; then :
else
  give_back "$P"
  if [ -n "$local" ]; then git -C {r} worktree add --quiet "$P" "$b"; else git -C {r} worktree add --quiet --track -b "$b" "$P" "origin/$b"; fi || exit 13
fi"#,
                b = shell_quote(branch),
            ),
            Checkout::Detached(rev) => format!(
                r#"git -C {r} rev-parse --verify --quiet {rv} >/dev/null || {{ echo "unknown revision {rev}" >&2; exit 14; }}
before=$(git -C "$P" rev-parse HEAD 2>/dev/null)
if [ -e "$P" ]; then
  git -C "$P" rev-parse --show-toplevel >/dev/null 2>&1 || {{ echo "$P exists but is not a git worktree" >&2; exit 12; }}
  git -C "$P" checkout --quiet --detach {rv} || exit 13
elif take_spare "$P" && git -C "$P" checkout --quiet --detach {rv}; then :
else
  give_back "$P"
  git -C {r} worktree add --quiet --detach "$P" {rv} || exit 13
fi"#,
                rv = shell_quote(&format!("{rev}^{{commit}}")),
            ),
        };
        format!(
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
        )
    }

    /// New branch `<prefix><slug>` (first free slug: `slug`, `slug-2`, …)
    /// off `origin/<base>` as last fetched (the app's spare warmer keeps it
    /// fresh), checked out in the sibling worktree `<repo>-<slug>`, with
    /// `.kitchen/` git-excluded. One round trip to the host, no network.
    /// Without `origin/<base>` (nothing pushed yet) the branch starts from
    /// the local `<base>`, and in a repo with no commits at all it starts
    /// empty (`--orphan`, git 2.42+).
    pub fn create_edit_worktree(&self, base: &str, prefix: &str, slug: &str) -> Result<Worktree> {
        // `<repo>-kitchen-*` names belong to the kitchen (spares, PR views).
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
if git -C {r} rev-parse --verify --quiet refs/remotes/origin/{b} >/dev/null; then START=refs/remotes/origin/{b}
elif git -C {r} rev-parse --verify --quiet refs/heads/{b} >/dev/null; then START=refs/heads/{b}
else START=; fi
spare_checkout() {{
  if [ "$(git -C "$P" rev-parse HEAD)" = "$(git -C {r} rev-parse "$START")" ]; then
    # Same commit: pure ref updates, no index refresh (~0.6s saved on big repos).
    git -C "$P" branch --no-track {pf}"$s" "$START" && git -C "$P" symbolic-ref HEAD refs/heads/{pf}"$s"
  else
    git -C "$P" checkout --quiet --no-track -b {pf}"$s" "$START"
  fi
}}
# No commits to start from: a branch with no history. Else take over a
# pre-checked-out spare (instant), else check out from scratch.
if [ -z "$START" ]; then
  git -C {r} worktree add --quiet --orphan -b {pf}"$s" "$P" || {{ echo "the repo has no commits yet, and starting an empty branch needs git 2.42+" >&2; exit 13; }}
elif take_spare "$P" && spare_checkout; then :
else
  [ -n "$TAKEN" ] && git -C {r} branch -D {pf}"$s" >/dev/null 2>&1
  give_back "$P"
  git -C {r} worktree add --quiet --no-track -b {pf}"$s" "$P" "$START" || exit 13
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
    /// makes takeovers skip it. Files a killed checkout left in a spare are
    /// thrown away. Worktrees on a branch are never touched.
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
  # from a git that was killed; reset it to HEAD (and drop the files it
  # wrote) before going on.
  [ -e "$A/locked" ] || [ ! -e "$A/index.lock" ] || {{ rm -f "$A/index.lock"; git -C "$SP" reset -q --hard HEAD 2>/dev/null; git -C "$SP" clean -fdq; }}
  [ "$(git -C "$SP" rev-parse HEAD)" = "$(git -C {r} rev-parse origin/{b})" ] && continue
  git -C {r} worktree lock --reason {reason} "$SP" 2>/dev/null || continue
  # A checkout git refuses (local changes or untracked files in the way,
  # left by a killed one) is forced: a spare holds nobody's work.
  {NICE} git -C "$SP" checkout --quiet --detach origin/{b} 2>/dev/null \
    || {{ {NICE} git -C "$SP" checkout --quiet --force --detach origin/{b} && {NICE} git -C "$SP" clean -fdq; }}; rc=$?
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
    /// directory) and run the cheap maintenance tasks, niced. Returns what it
    /// changed, for the log. (The spare warmer compacts the packs, see
    /// [`Self::compact_packs`].)
    pub fn tune(&self) -> Result<Vec<String>> {
        let script = format!(
            r#"git config --get core.untrackedCache >/dev/null || {{ git config core.untrackedCache true && echo "set core.untrackedCache=true"; }}
{NICE} git maintenance run --task=loose-objects --task=commit-graph >/dev/null 2>&1 && echo "maintenance: loose-objects, commit-graph"
true"#
        );
        let out = self.host.sh_ok(&script, Some(self.path))?;
        Ok(out.lines().map(String::from).filter(|l| !l.is_empty()).collect())
    }

    /// With [`SMALL_PACKS`] or more packs under [`SMALL_PACK_KIB`], roll them
    /// up (`git repack -A -d`: one promisor pack, and one for objects of our
    /// own; the big packs kept as they are) and
    /// index every pack in a multi-pack index. Devboxes turn git's own
    /// maintenance off, so without this a partial clone's packs pile up and
    /// every git command slows down (a devbox's agi at 178k packs: 0.35 s for
    /// a lookup). Promisor packs stay promisor packs, and a fetch's pack
    /// still being written has a `.keep` and is left alone. One compaction
    /// per repo at a time (workspaces share repos); niced. What it did, for
    /// the log (None: nothing to do).
    pub fn compact_packs(&self) -> Result<Option<String>> {
        self.compact_packs_under(SMALL_PACK_KIB)
    }

    fn compact_packs_under(&self, small_kib: u64) -> Result<Option<String>> {
        let out = self.host.sh_ok(&compact_packs_snippet(small_kib), Some(self.path))?;
        Ok(out.lines().find(|l| l.starts_with("packs compacted")).map(String::from))
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
    /// round trip: HEAD, a checksum of the index's file list, the merge base
    /// of HEAD and `base_ref`, and the files changed since it, untracked
    /// files included as added (unless `memo` is
    /// [`ListingMemo::tracked_only`]). `memo` is the previous listing of `dir`:
    /// the checksum (`ls-files | cksum`, ~60 ms on agi) is reused while the
    /// index file is unchanged, and untracked files (~0.5 s on agi) are
    /// scanned only when HEAD or the index moved, or every
    /// [`UNTRACKED_EVERY`].
    pub fn listing(&self, dir: &str, base_ref: &str, memo: &mut ListingMemo) -> Result<Listing> {
        let started = Instant::now();
        let out = self.host.sh_ok(&listing_script(dir, base_ref, memo), None).with_context(|| format!("list files in {dir}"))?;
        read_listing(&out, memo, started)
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

/// The script of [`Repo::listing`]: sections split by `\x1e`.
fn listing_script(dir: &str, base_ref: &str, memo: &ListingMemo) -> String {
    let (d, b) = (shell_quote(dir), shell_quote(base_ref));
    let scan = !memo.tracked_only && memo.scanned_at.is_none_or(|t| t.elapsed() >= UNTRACKED_EVERY);
    let moved = if memo.tracked_only { "false".to_string() } else { format!("[ \"$head\" != {} ] || [ \"$stamp\" != {} ]", shell_quote(&memo.head), shell_quote(&memo.stamp)) };
    let (prev_stamp, prev_index) = (shell_quote(&memo.stamp), shell_quote(&memo.index));
    // The index is stat'ed before it is read: a write in between changes
    // the stamp, and the next listing reads it again.
    format!(
        "cd {d} || exit 3\n\
         head=$(git rev-parse HEAD); printf '%s\\036' \"$head\"\n\
         idx=$(git rev-parse --git-path index)\n\
         stamp=$({{ stat -c '%s %y %i' -- \"$idx\" || stat -f '%z %Fm %i' -- \"$idx\"; }} 2>/dev/null | tr -d '\\n')\n\
         printf '%s\\036' \"$stamp\"\n\
         if [ -n \"$stamp\" ] && [ \"$stamp\" = {prev_stamp} ]; then printf '%s' {prev_index}; \
         else git ls-files --cached -z | cksum | tr -d '\\n'; fi; printf '\\036'\n\
         base=$(git merge-base HEAD {b} 2>/dev/null) || base={b}\n\
         printf '%s\\036' \"$base\"\n\
         git diff -M --raw --numstat -z \"$base\"; printf '\\036'\n\
         if [ {scan} = 1 ] || {moved}; then \
         printf y; git ls-files --others --exclude-standard -z; fi\n",
        scan = scan as u8,
    )
}

/// [`Repo::listing`] from its script's output, updating `memo`.
fn read_listing(out: &str, memo: &mut ListingMemo, started: Instant) -> Result<Listing> {
    let parts: Vec<&str> = out.split('\x1e').collect();
    let [head, stamp, index, base, diff, others] = parts[..] else {
        bail!("unexpected listing output ({} sections)", parts.len());
    };
    if let Some(others) = others.strip_prefix('y') {
        memo.untracked = others.split('\0').filter(|p| !p.is_empty()).map(String::from).collect();
        memo.scanned_at = Some(started);
    }
    let mut changed = parse_diff_z(diff);
    for p in &memo.untracked {
        if !changed.iter().any(|f| f.path == *p) {
            changed.push(ChangedFile { path: p.clone(), old_path: None, status: 'A', added: 0, removed: 0 });
        }
    }
    changed.sort_by(|a, b| (b.added + b.removed).cmp(&(a.added + a.removed)).then(a.path.cmp(&b.path)));
    memo.head = head.trim().to_string();
    memo.stamp = stamp.trim().to_string();
    memo.index = index.trim().to_string();
    Ok(Listing { head: memo.head.clone(), index: memo.index.clone(), base: base.trim().to_string(), changed })
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
/// moves it back if one was taken (a checkout after the takeover failed, e.g.
/// on files a killed refresh left behind: the next refresh cleans it, and the
/// caller checks out from scratch instead). `git worktree move`
/// refuses a locked spare (being refreshed); spares on a branch are not
/// spares, and one with an `index.lock` (a git killed mid-way) is left to
/// the next refresh. `repo` is quoted.
fn take_spare_snippet(repo: &str, spares: &[String]) -> String {
    let spares: Vec<String> = spares.iter().map(|p| shell_quote(p)).collect();
    format!(
        r#"TAKEN=
take_spare() {{ for SP in {}; do [ -e "$SP/.git" ] || continue; git -C "$SP" symbolic-ref -q HEAD >/dev/null && continue; [ -e "$(git -C "$SP" rev-parse --absolute-git-dir)/index.lock" ] && continue; git -C {repo} worktree move "$SP" "$1" 2>/dev/null && TAKEN=$SP && return 0; done; return 1; }}
give_back() {{ [ -n "$TAKEN" ] && git -C {repo} worktree move "$1" "$TAKEN" 2>/dev/null; TAKEN=; true; }}"#,
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

/// Shell (run in the repo) for [`Repo::compact_packs`]; prints "packs
/// compacted: …" when it compacted.
fn compact_packs_snippet(small_kib: u64) -> String {
    format!(
            r#"d=$(git rev-parse --path-format=absolute --git-path objects/pack) || exit 3
n=$(find "$d" -maxdepth 1 -name '*.pack' -size -{small_kib}k | wc -l | tr -d ' ')
[ "$n" -ge {SMALL_PACKS} ] || exit 0
# A lock a killed compaction left is stale after a day.
l="$d/kitchen-compact.lock"
find "$l" -maxdepth 0 -mmin +1440 -exec rmdir {{}} \; 2>/dev/null
mkdir "$l" 2>/dev/null || exit 0
keep=$(find "$d" -maxdepth 1 -name '*.pack' ! -size -{small_kib}k | sed 's|.*/|--keep-pack=|')
# git keeps a descriptor per pack it reads, up to the limit: with a high
# one, a repack of 100k+ packs holds that many (on a mac, a few such gits
# filled the system's file table). Below 10240, git closes packs as it goes.
f=$(ulimit -n); {{ [ "$f" = unlimited ] || [ "$f" -gt 10240 ]; }} && ulimit -n 10240
{NICE} git repack -A -d -q --write-midx --threads=2 $keep >/dev/null 2>&1; rc=$?
rmdir "$l"
[ $rc = 0 ] || exit $rc
echo "packs compacted: $n small packs rolled up, $(find "$d" -maxdepth 1 -name '*.pack' | wc -l | tr -d ' ') packs now""#
    )
}

/// Shell lines printing `KT_PATH=`/`KT_HEAD=`/`KT_BRANCH=` for [`parse_report`].
fn report_snippet(dir: &str) -> String {
    format!(
        r#"echo "KT_PATH=$(git -C {dir} rev-parse --show-toplevel)"; echo "KT_HEAD=$(git -C {dir} rev-parse --verify --quiet HEAD)"; echo "KT_BRANCH=$(git -C {dir} symbolic-ref --quiet --short HEAD)""#
    )
}

fn parse_report(out: &str) -> Result<Worktree> {
    let mut wt = Worktree { path: String::new(), head: String::new(), branch: None };
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("KT_PATH=") {
            wt.path = v.to_string();
        } else if let Some(v) = line.strip_prefix("KT_HEAD=") {
            wt.head = v.to_string();
        } else if let Some(v) = line.strip_prefix("KT_BRANCH=") {
            wt.branch = Some(v.to_string()).filter(|b| !b.is_empty());
        }
    }
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
        let mut wt = Worktree { path: String::new(), head: String::new(), branch: None };
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                wt.path = p.to_string();
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                wt.head = h.to_string();
            } else if let Some(b) = line.strip_prefix("branch ") {
                wt.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
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

/// What [`Repo::listing`] reuses from the previous listing of a worktree.
#[derive(Debug, Clone, Default)]
pub struct ListingMemo {
    head: String,
    /// The index file's size, mtime and inode ("" when it could not be stat'ed).
    stamp: String,
    /// The index's file-list checksum when the index had `stamp`.
    index: String,
    untracked: Vec<String>,
    /// When `untracked` was scanned (None: scan on the next listing).
    scanned_at: Option<Instant>,
    /// Never scan for untracked files (see [`ListingMemo::tracked_only`]).
    tracked_only: bool,
}

impl ListingMemo {
    /// Listings that never scan for untracked files: a PR's checkout has
    /// none worth showing, and on agi the scan is most of a listing (~0.8 s
    /// on a devbox).
    pub fn tracked_only() -> Self {
        Self { tracked_only: true, ..Self::default() }
    }

    /// Scan for untracked files on the next listing, whatever moved.
    pub fn rescan(&mut self) {
        self.scanned_at = None;
    }
}

/// `git diff --raw --numstat -z` output: the raw records
/// (`:<modes> <shas> <status>\0<path>\0`, a rename `…R<score>\0<old>\0<new>\0`),
/// then the numstat ones (`<a>\t<r>\t<path>\0`, a rename `<a>\t<r>\t\0<old>\0<new>\0`;
/// `-` for a binary file's counts).
fn parse_diff_z(s: &str) -> Vec<ChangedFile> {
    let mut out = Vec::new();
    let mut stats = std::collections::HashMap::new();
    let mut it = s.split('\0').filter(|p| !p.is_empty());
    while let Some(rec) = it.next() {
        if let Some(header) = rec.strip_prefix(':') {
            let status = header.rsplit(' ').next().and_then(|code| code.chars().next()).unwrap_or('M');
            if status == 'R' || status == 'C' {
                let old = it.next().unwrap_or_default().to_string();
                let new = it.next().unwrap_or_default().to_string();
                out.push(ChangedFile { path: new, old_path: Some(old), status: 'R', added: 0, removed: 0 });
            } else if let Some(p) = it.next() {
                out.push(ChangedFile { path: p.to_string(), old_path: None, status, added: 0, removed: 0 });
            }
            continue;
        }
        let mut f = rec.splitn(3, '\t');
        let a = f.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let r = f.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let path = match f.next().unwrap_or_default() {
            "" => it.nth(1), // the new path, after the old one
            p => Some(p),
        };
        if let Some(p) = path {
            stats.insert(p, (a, r));
        }
    }
    for f in &mut out {
        if let Some(&(a, r)) = stats.get(f.path.as_str()) {
            (f.added, f.removed) = (a, r);
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
        assert!(w[1].branch.is_none());
    }

    #[test]
    fn raw_and_numstat() {
        let z = "0000000 0000000";
        let s = format!(
            ":100644 100644 {z} M\0a.rs\0:100644 100644 {z} R085\0old.rs\0new.rs\0:000000 100644 {z} A\0:b.rs\0\
             :100644 100644 {z} M\0bin.dat\0\
             1\t2\ta.rs\0003\t4\t\0old.rs\0new.rs\0005\t0\t:b.rs\0-\t-\tbin.dat\0"
        );
        let f = parse_diff_z(&s);
        let got: Vec<(&str, Option<&str>, char, u32, u32)> = f.iter().map(|c| (c.path.as_str(), c.old_path.as_deref(), c.status, c.added, c.removed)).collect();
        assert_eq!(
            got,
            vec![("a.rs", None, 'M', 1, 2), ("new.rs", Some("old.rs"), 'R', 3, 4), (":b.rs", None, 'A', 5, 0), ("bin.dat", None, 'M', 0, 0)]
        );
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
        let l = repo.listing(&dir, "origin/main", &mut ListingMemo::default()).unwrap();
        assert_eq!(l.head.len(), 40, "{:?}", l.head);
        assert_eq!(l.base, l.head, "nothing committed: the merge base is HEAD");
        assert!(!l.index.is_empty());
        sh(root.join("repo").as_path(), "git add 'aster/sp ace.rs'");
        let staged = repo.listing(&dir, "origin/main", &mut ListingMemo::default()).unwrap();
        assert_ne!(staged.index, l.index, "staging a file changes the index checksum");
        assert_eq!(staged.head, l.head);
        sh(root.join("repo").as_path(), "git rm -q --cached 'aster/sp ace.rs'");
        assert_eq!(repo.listing(&dir, "origin/main", &mut ListingMemo::default()).unwrap().index, l.index);
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

    /// A re-list with the last one's memo reuses the index checksum while
    /// the index is unchanged, and finds new untracked files when HEAD or
    /// the index moved, or when the scan is due.
    #[test]
    fn relisting_reuses_the_memo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        sh(root, "git init -q -b main repo && cd repo && git config user.email t@t && git config user.name t && echo one > a.rs && git add . && git commit -qm init");
        let dir = root.join("repo").display().to_string();
        let repo = Repo::new(&Host::Local, &dir);
        let mut memo = ListingMemo::default();
        let first = repo.listing(&dir, "main", &mut memo).unwrap();
        assert!(!memo.stamp.is_empty(), "the index was stat'ed");
        // A checksum the index could not have: reused while the stamp holds.
        memo.index = "memo".into();
        assert_eq!(repo.listing(&dir, "main", &mut memo).unwrap().index, "memo");
        // A new untracked file waits for the next scan...
        sh(&root.join("repo"), "echo new > b.rs");
        assert!(repo.listing(&dir, "main", &mut memo).unwrap().changed.is_empty());
        memo.rescan();
        let changed: Vec<String> = repo.listing(&dir, "main", &mut memo).unwrap().changed.into_iter().map(|c| c.path).collect();
        assert_eq!(changed, vec!["b.rs"]);
        // ...and staging one rewrites the index: a new checksum and a scan.
        sh(&root.join("repo"), "git add b.rs && echo newer > c.rs");
        let staged = repo.listing(&dir, "main", &mut memo).unwrap();
        assert_ne!(staged.index, "memo");
        assert_ne!(staged.index, first.index, "the file list changed");
        let changed: Vec<&str> = staged.changed.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(changed, vec!["b.rs", "c.rs"]);
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

        // Stale spares a killed checkout left dirty (an edit to a file the
        // checkout changes, an untracked file where it writes one): git
        // refuses to move them, so a new tab checks out from scratch and
        // hands the spare back, and the next refresh throws the leftovers
        // away.
        sh(root, "cd origin && echo nine > n.txt && echo two > a.txt && git add . && git commit -qm nine");
        repo.fetch().unwrap();
        std::fs::write(format!("{}/a.txt", spares[0]), "left over\n").unwrap();
        std::fs::write(format!("{}/n.txt", spares[1]), "left over\n").unwrap();
        let i = repo.create_edit_worktree("main", "william/", "dirty").unwrap();
        assert!(spares.iter().all(|s| exists(s)), "dirty spares handed back");
        assert_eq!(std::fs::read_to_string(format!("{}/n.txt", i.path)).unwrap(), "nine\n");
        assert_eq!(i.branch.as_deref(), Some("william/dirty"));
        let path = repo.sibling_path("dirty-branch");
        let (wt, _) = repo.checkout_worktree(&path, Checkout::Branch("william/dirty"), &[]).unwrap();
        assert_eq!(wt.path, i.path, "branch reused where it is");
        let det = repo.sibling_path("dirty-detached");
        let (wt, _) = repo.checkout_worktree(&det, Checkout::Detached("origin/main"), &[]).unwrap();
        assert!(wt.branch.is_none() && exists(&format!("{det}/n.txt")) && spares.iter().all(|s| exists(s)), "detached: fresh checkout");
        repo.ensure_spare_worktree("main", false).unwrap();
        for s in &spares {
            assert_eq!(std::fs::read_to_string(format!("{s}/n.txt")).unwrap(), "nine\n", "{s} refreshed");
            assert_eq!(repo.git_in(s, &["status", "--porcelain"]).unwrap(), "", "{s} clean");
        }
    }

    /// Repos with nothing on origin: new tabs start an empty branch until
    /// there is a commit, then branch off the local default branch.
    #[test]
    fn edit_worktree_in_empty_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        sh(root, "git init -q -b trunk solo && git init -q --bare -b main origin && git clone -q origin clone 2>/dev/null");
        for name in ["solo", "clone"] {
            let path = root.join(name).display().to_string();
            let repo = Repo::new(&Host::Local, &path);
            assert!(repo.default_branch().is_err(), "{name}: nothing on origin");
            let base = repo.head_branch().unwrap();
            assert_eq!(base, if name == "solo" { "trunk" } else { "main" });
            let wt = repo.create_edit_worktree(&base, "william/", "first").unwrap();
            assert!(wt.path.ends_with(&format!("/{name}-first")), "{}", wt.path);
            assert_eq!(wt.branch.as_deref(), Some("william/first"));
            assert_eq!(wt.head, "", "no commit yet");
            std::fs::write(format!("{}/new.txt", wt.path), "x\n").unwrap();
            let l = repo.listing(&wt.path, &format!("origin/{base}"), &mut ListingMemo::default()).unwrap();
            let changed: Vec<&str> = l.changed.iter().map(|c| c.path.as_str()).collect();
            assert_eq!(changed, vec!["new.txt"], "{name}: untracked files show as added");
        }

        // Committed but never pushed: off the local branch.
        let solo = root.join("solo");
        sh(&solo, "git config user.email t@t && git config user.name t && echo one > a.txt && git add . && git commit -qm init");
        let path = solo.display().to_string();
        let repo = Repo::new(&Host::Local, &path);
        let wt = repo.create_edit_worktree("trunk", "william/", "second").unwrap();
        assert_eq!(wt.head.len(), 40);
        assert!(exists(&format!("{}/a.txt", wt.path)));
    }

    /// Existing branches and PR heads, fetched and checked out in one trip,
    /// then moved forward (never reset) when opened again.
    /// A PR view skips its fetch when its refs already name the commits
    /// GitHub reports, and fetches when one is stale or missing.
    #[test]
    fn checkout_skips_the_fetch_when_the_commits_are_here() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        sh(
            root,
            "git init -q -b main origin && cd origin && git config user.email t@t && git config user.name t \
             && echo one > a.txt && git add . && git commit -qm init \
             && git checkout -qb pr && echo pr > p.txt && git add . && git commit -qm pr \
             && git update-ref refs/pull/7/head HEAD && git checkout -q main \
             && cd .. && git clone -q origin repo",
        );
        let host = Host::Local;
        let repo_path = root.join("repo").display().to_string();
        let repo = Repo::new(&host, &repo_path);
        let rev = |r: &str| repo.git(&["rev-parse", r]).unwrap().trim().to_string();
        let origin = |r: &str| Repo::new(&host, &root.join("origin").display().to_string()).git(&["rev-parse", r]).unwrap().trim().to_string();
        let det = repo.sibling_path("kitchen-pr-7");
        let specs = ["+refs/pull/7/head:refs/kitchen/pr/7".to_string()];
        // Not here yet: fetched.
        let have = vec![("refs/kitchen/pr/7".to_string(), origin("refs/pull/7/head"))];
        let (wt, moved) = repo.checkout_worktree_unless(&det, Checkout::Detached("refs/kitchen/pr/7"), &specs, &have).unwrap();
        assert!(moved && wt.head == origin("refs/pull/7/head"));
        // Here: no fetch, so a refspec that can't be fetched doesn't matter.
        let broken = ["+refs/pull/nope/head:refs/kitchen/pr/7".to_string()];
        let (_, moved) = repo.checkout_worktree_unless(&det, Checkout::Detached("refs/kitchen/pr/7"), &broken, &have).unwrap();
        assert!(!moved);
        // GitHub reports a newer head: fetched (and here the fetch fails).
        let stale = vec![("refs/kitchen/pr/7".to_string(), origin("main"))];
        let e = repo.checkout_worktree_unless(&det, Checkout::Detached("refs/kitchen/pr/7"), &broken, &stale).unwrap_err();
        assert!(e.to_string().contains("exit 11"), "{e:#}");
        assert_eq!(rev("refs/kitchen/pr/7"), origin("refs/pull/7/head"));

        // Checked out and listed in one trip, as a separate listing lists it;
        // a tracked-only listing leaves an untracked file out.
        std::fs::write(format!("{det}/stray.txt"), "x").unwrap();
        let (_, moved, listing, mut memo) = repo.checkout_and_list(&det, Checkout::Detached("refs/kitchen/pr/7"), &broken, &have, "origin/main", ListingMemo::tracked_only()).unwrap();
        assert!(!moved);
        let paths: Vec<&str> = listing.changed.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["p.txt"]);
        assert_eq!(listing, repo.listing(&det, "origin/main", &mut memo).unwrap(), "its memo lists the same");
        let all = repo.listing(&det, "origin/main", &mut ListingMemo::default()).unwrap();
        assert!(all.changed.iter().any(|c| c.path == "stray.txt"), "a default memo lists untracked files");
        let _ = std::fs::remove_dir_all(&det);
    }

    /// A partial clone's small packs are rolled up once there are enough.
    #[test]
    fn compact_packs_rolls_up_a_partial_clones_downloads() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let n = SMALL_PACKS + 2;
        // Git's own maintenance off, as on a devbox.
        sh(
            root,
            &format!(
                "git init -q -b main origin && cd origin && git config uploadpack.allowFilter true \
                  && for i in $(seq {n}); do head -c 3000 /dev/urandom | base64 > f$i; done && git add . && git -c user.email=t@t -c user.name=t commit -qm init \
                 && cd .. && git clone -q --filter=blob:none --no-checkout \"file://$PWD/origin\" r \
                 && git -C r config gc.auto 0 && git -C r config maintenance.auto false"
            ),
        );
        let r = root.join("r");
        let host = Host::Local;
        let path = r.display().to_string();
        let repo = Repo::new(&host, &path);
        let pack_dir = r.join(".git/objects/pack");
        let count = |ext: &str| std::fs::read_dir(&pack_dir).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == ext)).count();
        // One download, one pack: too few to compact.
        sh(&r, "git show HEAD:f1 >/dev/null");
        assert_eq!(repo.compact_packs().unwrap(), None, "few packs: left as they are");
        sh(&r, &format!("for i in $(seq 2 {n}); do git show HEAD:f$i >/dev/null; done"));
        // A commit of our own: a loose object, in no promisor pack.
        sh(&r, "git -c user.email=t@t -c user.name=t commit -q --allow-empty -m mine");
        assert!(count("pack") > SMALL_PACKS, "{} packs", count("pack"));
        let done = repo.compact_packs().unwrap().expect("compacted");
        assert!(done.starts_with("packs compacted"), "{done}");
        // The downloads in one promisor pack, our commit in a pack of its own.
        assert_eq!((count("pack"), count("promisor")), (2, 1), "{done}");
        assert!(pack_dir.join("multi-pack-index").exists());
        assert!(!pack_dir.join("kitchen-compact.lock").exists());
        assert_eq!(repo.compact_packs().unwrap(), None);
        sh(&r, &format!("git cat-file -e HEAD~1:f{n} && git log -1 --format=%s | grep -qx mine && git fsck --connectivity-only 2>/dev/null"));
        // Still a partial clone: a new file on origin is downloaded on demand.
        sh(root, "cd origin && echo new > g && git add g && git -c user.email=t@t -c user.name=t commit -qm g");
        sh(&r, "git fetch -q origin && git show origin/main:g | grep -qx new");
        // Another compaction holds the repo: this one waits for its turn.
        sh(root, &format!("cd origin && for i in $(seq {n}); do echo $i > h$i; done && git add . && git -c user.email=t@t -c user.name=t commit -qm h"));
        sh(&r, &format!("git fetch -q origin && for i in $(seq {n}); do git cat-file -p origin/main:h$i >/dev/null; done"));
        std::fs::create_dir(pack_dir.join("kitchen-compact.lock")).unwrap();
        assert_eq!(repo.compact_packs().unwrap(), None, "locked: no second compaction");
        std::fs::remove_dir(pack_dir.join("kitchen-compact.lock")).unwrap();
        // Big packs are kept as they are: here, with 8 KiB as "big", the
        // packs compacted so far (the f files), while the new downloads (the
        // h files, a line each) roll up.
        let big: Vec<_> = std::fs::read_dir(&pack_dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|x| x == "pack") && p.metadata().unwrap().len() >= 8 * 1024).collect();
        assert!(!big.is_empty());
        assert!(repo.compact_packs_under(8).unwrap().is_some());
        assert!(big.iter().all(|p| p.exists()), "kept: {big:?}");
        sh(&r, &format!("git cat-file -p origin/main:h{n} | grep -qx {n} && git cat-file -e HEAD~1:f1 && git fsck --connectivity-only 2>/dev/null"));
    }

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
        assert!(moved && wt.branch.is_none() && exists(&format!("{det}/p.txt")));
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
