//! Manual end-to-end test of fast tabs and ⌘P against a real devbox repo.
//!
//! Drives the real entry points (Engine::create_tab, the github plugin's ⌘I
//! handler via Engine::open_text, Engine::refill_spare, Viewer::new/ui)
//! against a real host. Creates and removes its own tabs, worktrees,
//! branches, PR refs and locks; leaves the spares refreshed and the shared
//! read-only worktree at origin/<default> (as the app would).
//!
//! Don't open tabs of the same repo in a running kitchen meanwhile: it
//! shares the spares and the read-only worktree.
//!
//! ```sh
//! KT_E2E_HOST=willbox-1 KT_E2E_REPO=/home/dev/agi KT_E2E_SLUG=ppl-ai/agi KT_E2E_PR=234588 \
//!   cargo test -p kitchen-app e2e_fast_tabs -- --ignored --nocapture --test-threads=1
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use kitchen_core::db::TabRow;
use kitchen_core::git::{Repo, SPARE_COUNT};
use kitchen_core::host::Host;

use super::Viewer;
use crate::engine::{Engine, NewTab};
use crate::panels::{PanelCmd, PanelCtx};

/// Lock reason the test puts on a spare (cleanup removes it).
const HOLD: &str = "kt-e2e-hold";

struct Env {
    host: Host,
    repo: String,
    slug: String,
    pr: u64,
}

impl Env {
    fn from_env() -> Env {
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is required (see the module docs)"));
        Env { host: Host::parse(&var("KT_E2E_HOST")), repo: var("KT_E2E_REPO"), slug: var("KT_E2E_SLUG"), pr: var("KT_E2E_PR").parse().expect("KT_E2E_PR") }
    }
}

/// Step results: every step runs, the test fails at the end if any failed.
#[derive(Default)]
struct Report {
    rows: Vec<(bool, String)>,
}

impl Report {
    fn check(&mut self, ok: bool, what: impl Into<String>) -> bool {
        let what = what.into();
        eprintln!("{} {what}", if ok { "PASS" } else { "FAIL" });
        self.rows.push((ok, what));
        ok
    }

    fn within(&mut self, took: Duration, limit: Duration, what: &str) -> bool {
        self.check(took <= limit, format!("{what}: {:.2}s (limit {:.1}s)", took.as_secs_f64(), limit.as_secs_f64()))
    }

    fn finish(&self) {
        let failed: Vec<&String> = self.rows.iter().filter(|r| !r.0).map(|r| &r.1).collect();
        eprintln!("\n{} checks, {} failed", self.rows.len(), failed.len());
        for f in &failed {
            eprintln!("  FAIL {f}");
        }
        assert!(failed.is_empty(), "{} e2e checks failed", failed.len());
    }
}

/// Shell on the target: `Some(stdout)` trimmed, `None` on any failure.
fn sh(host: &Host, script: &str) -> Option<String> {
    match host.sh(script, None) {
        Ok(o) if o.ok() => Some(o.stdout.trim().to_string()),
        Ok(o) => {
            eprintln!("  (sh exit {}: {})", o.status, o.stderr.trim());
            None
        }
        Err(e) => {
            eprintln!("  (sh: {e:#})");
            None
        }
    }
}

fn q(s: &str) -> String {
    kitchen_core::util::shell_quote(s)
}

/// Seconds `git status --porcelain` takes in `wt`, and its output.
fn status(host: &Host, wt: &str) -> (f64, Option<String>) {
    let start = Instant::now();
    let out = sh(host, &format!("git -C {} status --porcelain", q(wt)));
    (start.elapsed().as_secs_f64(), out)
}

fn head(host: &Host, wt: &str) -> Option<String> {
    sh(host, &format!("git -C {} rev-parse HEAD", q(wt)))
}

/// The worktree's admin dir. It is named after the path the worktree was
/// created at and `git worktree move` keeps it, so a tab that took over a
/// spare has an admin dir `<repo>-kitchen-spare*`.
fn admin_dir(host: &Host, wt: &str) -> Option<String> {
    sh(host, &format!("git -C {} rev-parse --absolute-git-dir", q(wt)))
}

fn took_spare(host: &Host, wt: &str) -> bool {
    admin_dir(host, wt).is_some_and(|d| d.rsplit('/').next().is_some_and(|n| n.contains("-kitchen-spare")))
}

fn origin_head(host: &Host, repo: &str, base: &str) -> Option<String> {
    sh(host, &format!("git -C {} rev-parse origin/{base}", q(repo)))
}

/// Creating a file in the worktree's top folder fails (probe removed if not).
fn locked(host: &Host, wt: &str) -> bool {
    let p = q(&format!("{wt}/kt-e2e-probe"));
    sh(host, &format!("if touch {p} 2>/dev/null; then rm -f {p}; echo WRITABLE; else echo LOCKED; fi")).as_deref() == Some("LOCKED")
}

/// Every spare exists, is detached at origin/<base> and not locked.
fn spares_fresh(host: &Host, repo: &Repo, base: &str) -> bool {
    let Some(want) = origin_head(host, repo.path, base) else { return false };
    repo.spare_paths().iter().all(|sp| {
        sh(
            host,
            &format!(
                "git -C {sp} symbolic-ref -q HEAD >/dev/null && echo BRANCH; git -C {sp} rev-parse HEAD; test -e \"$(git -C {sp} rev-parse --git-dir)/locked\" && echo LOCKED; true",
                sp = q(sp)
            ),
        )
        .as_deref()
            == Some(want.as_str())
    })
}

/// Spares fresh and staying fresh for a few seconds (the refresh thread is done).
fn spares_settled(host: &Host, repo: &Repo, base: &str) -> bool {
    let ok = wait_for(Duration::from_secs(300), || spares_fresh(host, repo, base)).is_some();
    std::thread::sleep(Duration::from_secs(4));
    ok && spares_fresh(host, repo, base)
}

fn wait_for(limit: Duration, mut f: impl FnMut() -> bool) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if f() {
            return Some(start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Undoes everything the test did, also when a check panics.
struct Cleanup {
    engine: Arc<Engine>,
    host: Host,
    repo: String,
    base: String,
    /// `mv` to undo first (from, to).
    restore: Vec<(String, String)>,
    tabs: Vec<String>,
    worktrees: Vec<String>,
    branches: Vec<String>,
    refs: Vec<String>,
    /// Files the test planted (index.lock).
    files: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let h = &self.host;
        for (from, to) in self.restore.drain(..).rev() {
            let _ = sh(h, &format!("[ -e {f} ] && mv {f} {t}; true", f = q(&from), t = q(&to)));
        }
        for f in &self.files {
            let _ = sh(h, &format!("rm -f {}", q(f)));
        }
        for t in &self.tabs {
            let _ = self.engine.close_tab(t);
        }
        let repo = Repo::new(h, &self.repo);
        // Also any worktree of ours a failed create left behind.
        for w in repo.worktrees().unwrap_or_default() {
            if w.path.rsplit('/').next().is_some_and(|n| n.contains("-kt-e2e-")) && !self.worktrees.contains(&w.path) {
                self.worktrees.push(w.path);
            }
        }
        for wt in &self.worktrees {
            if let Err(e) = repo.remove_worktree(wt, true) {
                eprintln!("cleanup: {wt}: {e:#}");
            }
        }
        for b in &self.branches {
            let _ = h.run(&["git", "-C", &self.repo, "branch", "-D", b], None);
        }
        for r in &self.refs {
            let _ = h.run(&["git", "-C", &self.repo, "update-ref", "-d", r], None);
        }
        let r = q(&self.repo);
        for sp in repo.spare_paths() {
            let _ = sh(h, &format!("[ \"$(cat \"$(git -C {sp} rev-parse --git-dir)/locked\" 2>/dev/null)\" = {HOLD} ] && git -C {r} worktree unlock {sp}; true", sp = q(&sp)));
        }
        // A spare folder the test deleted, still registered: let git forget it.
        let _ = sh(h, &format!("git -C {r} worktree prune"));
        // origin/<base> may have been moved back by the fetch-contention step.
        let _ = sh(h, &format!("git -C {r} fetch -q --no-tags origin +refs/heads/{b}:refs/remotes/origin/{b}", b = self.base));
        eprintln!("cleanup: {} tabs, {} worktrees, {} branches", self.tabs.len(), self.worktrees.len(), self.branches.len());
    }
}

fn edit_tab(engine: &Arc<Engine>, clean: &mut Cleanup, ws_id: &str, name: &str) -> anyhow::Result<TabRow> {
    let row = engine.create_tab(ws_id, NewTab { kind: Some("edit".into()), title: Some(name.into()), no_agent: true, focus: Some(false), ..Default::default() })?;
    clean.tabs.push(row.id.clone());
    clean.worktrees.push(row.worktree.clone());
    clean.branches.extend(row.branch.clone());
    Ok(row)
}

fn readonly_tab(engine: &Arc<Engine>, clean: &mut Cleanup, ws_id: &str, name: &str) -> anyhow::Result<TabRow> {
    let row = engine.create_tab(ws_id, NewTab { kind: Some("readonly".into()), title: Some(name.into()), focus: Some(false), ..Default::default() })?;
    clean.tabs.push(row.id.clone());
    Ok(row)
}

/// One UI frame of `v`.
fn frame(engine: &Arc<Engine>, v: &mut Viewer) {
    let mut out = engine.ctx.run_ui(egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))), ..Default::default() }, |ui| {
        let mut cx = PanelCtx { engine, focused: true, clicked: false };
        v.ui(ui, &mut cx);
    });
    out.textures_delta.clear();
}

/// Frame `v` until `f` holds (or `limit`); how long it took.
fn frame_until(engine: &Arc<Engine>, v: &mut Viewer, limit: Duration, f: impl Fn(&Viewer) -> bool) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < limit {
        frame(engine, v);
        if f(v) {
            return Some(start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

fn viewer(engine: &Arc<Engine>, ws_id: &str, tab_id: &str) -> Viewer {
    let ws = engine.workspace(ws_id).unwrap();
    let tab = ws.tab(tab_id).unwrap().clone();
    Viewer::new(engine, &ws, &tab)
}

fn listed(v: &Viewer) -> bool {
    matches!(v.files_state, Some(Ok(())))
}

fn diff_shown(v: &Viewer) -> bool {
    v.current.as_ref().is_some_and(|p| v.files.contains_key(p) && v.bases.contains_key(p)) && !v.doc.lines.is_empty()
}

/// Force the next quiet poll now and frame until it landed.
fn poll_now(engine: &Arc<Engine>, v: &mut Viewer) -> Option<Duration> {
    v.changes_at = Instant::now() - Duration::from_secs(10);
    frame(engine, v);
    if !v.changes_pending {
        return None;
    }
    frame_until(engine, v, Duration::from_secs(20), |v| !v.changes_pending)
}

#[test]
#[ignore]
fn e2e_fast_tabs() {
    let env = Env::from_env();
    let _ = env_logger::builder().is_test(true).try_init();
    let host = env.host.clone();
    let repo = Repo::new(&host, &env.repo);
    let base = repo.default_branch().expect("default branch");
    let mut r = Report::default();

    let pr_path = repo.sibling_path(&format!("kitchen-ro-pr-{}", env.pr));
    let pr_ref = format!("refs/kitchen/pr/{}", env.pr);
    let listed_wts = sh(&host, &format!("git -C {} worktree list --porcelain", q(&env.repo))).expect("worktree list");
    assert!(
        !listed_wts.contains(&format!("-kitchen-ro-pr-{}\n", env.pr)) && sh(&host, &format!("test -e {} || echo FREE", q(&pr_path))).as_deref() == Some("FREE"),
        "{pr_path} already exists (a live tab?); pick another KT_E2E_PR"
    );
    assert!(sh(&host, &format!("git -C {} rev-parse -q --verify {pr_ref} || echo FREE", q(&env.repo))).as_deref() == Some("FREE"), "{pr_ref} exists");

    // An isolated engine: in-memory DB, read-only agents off (no claude on the box).
    let engine_for = |ws_id: &str| {
        let db = kitchen_core::db::Db::open_in_memory().unwrap();
        let settings = crate::settings::Settings { autostart_readonly_agent: false, ..Default::default() };
        db.set_setting("settings", &serde_json::to_string(&settings).unwrap()).unwrap();
        db.insert_workspace(&kitchen_core::db::WorkspaceRow {
            id: ws_id.to_string(),
            name: "e2e".into(),
            host: host.label().to_string(),
            repo_path: env.repo.clone(),
            github_slug: Some(env.slug.clone()),
            default_branch: Some(base.clone()),
            position: 0,
            pinned: false,
        })
        .unwrap();
        Engine::new(egui::Context::default(), db).unwrap()
    };
    let ws_id = format!("e2e{}", kitchen_core::util::new_id());
    let engine = engine_for(&ws_id);
    // Only this branch's github plugin.
    let plugin_dir = std::env::temp_dir().join(format!("kt-e2e-plugins-{}", kitchen_core::util::new_id()));
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::os::unix::fs::symlink(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/github"), plugin_dir.join("github")).unwrap();
    engine.set_plugins(crate::plugins::PluginHost::start(&engine, vec![plugin_dir.clone()]));
    let mut clean = Cleanup {
        engine: engine.clone(),
        host: host.clone(),
        repo: env.repo.clone(),
        base: base.clone(),
        restore: vec![],
        tabs: vec![],
        worktrees: vec![],
        branches: vec![],
        refs: vec![],
        files: vec![],
    };
    let r_q = q(&env.repo);
    let tracked_count = |wt: &str| sh(&host, &format!("git -C {} ls-files | wc -l", q(wt))).and_then(|n| n.trim().parse::<usize>().ok());
    eprintln!("target: {} {} ({:?} files), PR {}#{}", host.label(), env.repo, tracked_count(&env.repo), env.slug, env.pr);

    // 0. Repo tuning (bootstrap step) is quick and idempotent.
    let start = Instant::now();
    let tuned = repo.tune();
    r.check(tuned.is_ok(), format!("Repo::tune: {tuned:?}"));
    r.within(start.elapsed(), Duration::from_secs(60), "Repo::tune (maintenance, untracked cache)");
    r.check(sh(&host, &format!("git -C {r_q} config core.untrackedCache")).as_deref() == Some("true"), "core.untrackedCache on");

    // 1. Spares: a refill brings SPARE_COUNT of them to origin/<base>.
    sh(&host, &format!("git -C {r_q} fetch -q --no-tags origin +refs/heads/{base}:refs/remotes/origin/{base}"));
    engine.refill_spare(&ws_id);
    let start = Instant::now();
    r.check(spares_settled(&host, &repo, &base), format!("{SPARE_COUNT} spares fresh at origin/{base} after a refill ({:.0}s)", start.elapsed().as_secs_f64()));

    // 2. PR tab from a link (⌘I): gh and the fetch run at once; takes over a spare.
    let gh_start = Instant::now();
    let gh_head = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("export PATH=/opt/homebrew/bin:/usr/local/bin:$PATH; gh pr view {} -R {} --json headRefOid -q .headRefOid", env.pr, env.slug))
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let gh_secs = gh_start.elapsed();
    let url = format!("https://github.com/{}/pull/{}", env.slug, env.pr);
    let find_pr = || engine.workspace(&ws_id).unwrap().tabs.iter().find(|t| t.row.worktree.ends_with(&format!("kitchen-ro-pr-{}", env.pr))).cloned();
    engine.open_text(&url).expect("github plugin handles the link");
    let opened = wait_for(Duration::from_secs(120), || find_pr().is_some());
    let pr_tab = find_pr();
    if pr_tab.is_some() || sh(&host, &format!("test -e {} && echo Y", q(&pr_path))).is_some() {
        clean.worktrees.push(pr_path.clone());
        clean.refs.push(pr_ref.clone());
    }
    r.check(pr_tab.is_some(), "PR link opens a read-only tab");
    if let Some(d) = opened {
        r.within(d, Duration::from_secs(5), "PR tab open (gh ‖ fetch, worktree, tmux)");
        eprintln!("  (gh alone {:.2}s)", gh_secs.as_secs_f64());
    }
    if let Some(t) = &pr_tab {
        clean.tabs.push(t.row.id.clone());
        let h = head(&host, &t.row.worktree);
        r.check(!gh_head.is_empty() && h.as_deref() == Some(gh_head.as_str()), format!("PR worktree at the PR head ({h:?} vs gh {gh_head})"));
        r.check(t.row.diff_base.as_deref() == Some(&format!("origin/{base}")), format!("diff base origin/{base} ({:?})", t.row.diff_base));
        r.check(took_spare(&host, &t.row.worktree), format!("PR worktree took over a spare ({:?})", admin_dir(&host, &t.row.worktree)));
        r.check(locked(&host, &t.row.worktree), "PR worktree folders are locked");
        let (secs, out) = status(&host, &t.row.worktree);
        r.check(out.as_deref() == Some(""), format!("PR worktree clean ({out:?})"));
        r.check(secs < 2.5, format!("git status in the PR worktree {secs:.2}s < 2.5s (no re-hash after locking)"));
    }

    // 3. ⌘P on the PR tab: the first diff comes before the full file list.
    if let Some(t) = &pr_tab {
        let mut v = viewer(&engine, &ws_id, &t.row.id);
        let start = Instant::now();
        let shown = frame_until(&engine, &mut v, Duration::from_secs(30), diff_shown).map(|_| start.elapsed().as_secs_f64());
        let all = frame_until(&engine, &mut v, Duration::from_secs(30), listed).map(|_| start.elapsed().as_secs_f64());
        r.check(shown.is_some_and(|s| s < 1.5), format!("PR ⌘P first diff {shown:?}s < 1.5s"));
        r.check(all.is_some_and(|s| s < 4.0), format!("PR ⌘P full file list {all:?}s < 4s"));
        r.check(shown.zip(all).is_some_and(|(s, a)| s <= a), "the diff shows no later than the file list");
        r.check(Some(v.all.len()) == tracked_count(&t.row.worktree), format!("PR file list = git ls-files ({} paths)", v.all.len()));
        r.check(v.diff && !v.changed.is_empty() && v.doc.hunk_starts().contains(&v.vim.line), format!("diff mode on {:?}, cursor on its first change", v.current));
    }

    // 4. Edit tabs back to back: each takes a spare by ref updates only (the
    //    spare's index isn't rewritten), at the freshly fetched origin/<base>.
    r.check(spares_settled(&host, &repo, &base), "spares refilled after the PR tab took one");
    let index_stamp = |admin: &str| sh(&host, &format!("echo \"{admin} $(stat -c %y {})\"", q(&format!("{admin}/index"))));
    let before: Vec<String> = repo.spare_paths().iter().filter_map(|s| admin_dir(&host, s)).filter_map(|a| index_stamp(&a)).collect();
    let mut edits = Vec::new();
    for name in ["kt e2e a", "kt e2e b"] {
        let start = Instant::now();
        let row = edit_tab(&engine, &mut clean, &ws_id, name);
        let took = start.elapsed();
        let Ok(row) = row else {
            r.check(false, format!("edit tab {name:?}: {:#}", row.unwrap_err()));
            continue;
        };
        r.within(took, Duration::from_secs(3), &format!("edit tab {name:?}"));
        let admin = admin_dir(&host, &row.worktree).unwrap_or_default();
        r.check(took_spare(&host, &row.worktree), format!("edit tab {name:?} took over a spare ({admin})"));
        let now = index_stamp(&admin).unwrap_or_default();
        r.check(before.contains(&now), format!("edit tab {name:?}: ref-only takeover, index untouched ({now})"));
        r.check(head(&host, &row.worktree) == origin_head(&host, &env.repo, &base), format!("edit tab {name:?} at origin/{base}"));
        r.check(row.branch.as_deref().is_some_and(|b| b.ends_with(&kitchen_core::util::slugify(name, 40))), format!("branch {:?}", row.branch));
        let (secs, out) = status(&host, &row.worktree);
        r.check(out.as_deref() == Some("") && secs < 2.5, format!("edit tab {name:?} clean, status {secs:.2}s"));
        edits.push(row);
    }
    r.check(sh(&host, &format!("grep -cxF .kitchen/ \"$(git -C {r_q} rev-parse --path-format=absolute --git-common-dir)/info/exclude\"")).as_deref() == Some("1"), ".kitchen/ excluded exactly once");

    // 5. Re-opening the PR link: same tab, no move, selected again.
    if let (Some(t), Some(e)) = (&pr_tab, edits.first()) {
        engine.select_tab(&e.id);
        let n = engine.workspace(&ws_id).unwrap().tabs.len();
        let start = Instant::now();
        engine.open_text(&url).unwrap();
        let selected = || engine.workspace(&ws_id).unwrap().selected_tab.as_deref() == Some(t.row.id.as_str());
        let done = wait_for(Duration::from_secs(30), selected);
        r.check(done.is_some() && engine.workspace(&ws_id).unwrap().tabs.len() == n, "re-opening the PR link reuses its tab");
        r.within(start.elapsed(), Duration::from_secs(4), "PR re-open");
        r.check(head(&host, &t.row.worktree).as_deref() == Some(gh_head.as_str()), "PR worktree still at the PR head");
    }

    // 6. A spare git refuses to move (locked by someone else) is skipped.
    r.check(spares_settled(&host, &repo, &base), "spares refilled after two edit tabs");
    let spares = repo.spare_paths();
    let lock_of = |sp: &str| sh(&host, &format!("cat \"$(git -C {} rev-parse --git-dir)/locked\" 2>/dev/null; true", q(sp))).unwrap_or_default();
    sh(&host, &format!("git -C {r_q} worktree lock --reason {HOLD} {}", q(&spares[0])));
    let spare0_admin = admin_dir(&host, &spares[0]).unwrap_or_default();
    match edit_tab(&engine, &mut clean, &ws_id, "kt e2e c") {
        Ok(row) => {
            r.check(lock_of(&spares[0]) == HOLD, "locked spare left in place");
            r.check(admin_dir(&host, &row.worktree).is_some_and(|d| d != spare0_admin), "the tab didn't take the locked spare");
        }
        Err(e) => {
            r.check(false, format!("edit tab next to a locked spare: {e:#}"));
        }
    }
    sh(&host, &format!("git -C {r_q} worktree unlock {}", q(&spares[0])));

    // 7. A refill request that arrives while a refresh is past that spare is
    //    not lost: remove spare 2, refill, and once the refresh is busy on
    //    spare 2 take spare 1.
    r.check(spares_settled(&host, &repo, &base), "spares fresh before the refill race");
    sh(&host, &format!("git -C {r_q} worktree remove --force {}", q(&spares[1])));
    engine.refill_spare(&ws_id);
    let busy = wait_for(Duration::from_secs(60), || sh(&host, &format!("test -e {} && echo Y", q(&spares[1]))).is_some());
    r.check(busy.is_some(), "refresh is re-creating spare 2");
    let d = edit_tab(&engine, &mut clean, &ws_id, "kt e2e d");
    r.check(d.as_ref().is_ok_and(|t| took_spare(&host, &t.worktree)), "a tab takes spare 1 during the refresh");
    let fresh = wait_for(Duration::from_secs(180), || spares_fresh(&host, &repo, &base));
    r.check(fresh.is_some(), format!("both spares fresh again without another trigger ({fresh:?})"));
    engine.refill_spare(&ws_id); // (repair for the next steps if the check failed)

    // 8. A spare left with a stale index.lock (a refresh killed mid-checkout)
    //    doesn't break the next tab.
    r.check(spares_settled(&host, &repo, &base), "spares fresh before the index.lock step");
    let lock_file = format!("{}/index.lock", admin_dir(&host, &spares[0]).unwrap_or_default());
    sh(&host, &format!("touch {}", q(&lock_file)));
    clean.files.push(lock_file.clone());
    let e = edit_tab(&engine, &mut clean, &ws_id, "kt e2e e");
    r.check(e.is_ok(), format!("edit tab with a stale index.lock in a spare ({:?})", e.as_ref().err().map(|e| format!("{e:#}"))));
    if let Ok(row) = &e {
        let (_, out) = status(&host, &row.worktree);
        r.check(out.as_deref() == Some(""), format!("that tab's worktree is clean ({out:?})"));
    }

    // 9. A deleted (still registered) spare folder is re-created.
    sh(&host, &format!("rm -f {}", q(&lock_file)));
    r.check(spares_settled(&host, &repo, &base), "spares fresh before deleting a folder");
    sh(&host, &format!("rm -rf {}", q(&spares[1])));
    engine.refill_spare(&ws_id);
    r.check(spares_settled(&host, &repo, &base), "a deleted (still registered) spare is re-created");
    let _ = sh(&host, &format!("git -C {r_q} worktree prune")); // (repair if it wasn't)
    engine.refill_spare(&ws_id);

    // 10. Two kitchens (two engines) refreshing the same stale spare at once
    //     leave clean spares.
    r.check(spares_settled(&host, &repo, &base), "spares fresh before the two-kitchen step");
    sh(&host, &format!("git -C {} checkout -q --detach origin/{base}~3", q(&spares[1])));
    let ws2 = format!("e2e{}", kitchen_core::util::new_id());
    let engine2 = engine_for(&ws2);
    engine.refill_spare(&ws_id);
    engine2.refill_spare(&ws2);
    r.check(spares_settled(&host, &repo, &base), "two kitchens' refreshes settle");
    for sp in &spares {
        let (_, out) = status(&host, sp);
        r.check(out.as_deref() == Some(""), format!("spare {sp} clean after concurrent refreshes ({out:?})"));
    }

    // 11. Fetches that really move origin/<base> while a tab is created
    //     (another tab, a plugin) don't fail it.
    let contend = format!(
        "nohup sh -c 'end=$(($(date +%s)+20)); while [ $(date +%s) -lt $end ]; do git -C {r_q} update-ref refs/remotes/origin/{base} origin/{base}~1; git -C {r_q} fetch -q --no-tags origin +refs/heads/{base}:refs/remotes/origin/{base}; done' >/dev/null 2>&1 &"
    );
    sh(&host, &contend);
    std::thread::sleep(Duration::from_secs(2));
    let f = edit_tab(&engine, &mut clean, &ws_id, "kt e2e f");
    r.check(f.is_ok(), format!("edit tab during contended fetches ({:?})", f.as_ref().err().map(|e| format!("{e:#}"))));
    let ro_c = readonly_tab(&engine, &mut clean, &ws_id, "kt e2e ro-contended");
    r.check(ro_c.is_ok(), format!("read-only tab during contended fetches ({:?})", ro_c.as_ref().err().map(|e| format!("{e:#}"))));
    std::thread::sleep(Duration::from_secs(20));
    sh(&host, &format!("git -C {r_q} fetch -q --no-tags origin +refs/heads/{base}:refs/remotes/origin/{base}"));

    // 12. Read-only main on the shared worktree (old builds locked its files):
    //     moved back, then re-opened.
    let ro_path = repo.readonly_path();
    let _ = repo.sync_readonly_worktree(&ro_path, &format!("origin/{base}~50"));
    let probe_file = sh(&host, &format!("cd {} && for f in README.md .gitignore; do git diff --quiet origin/{base}~50 origin/{base} -- \"$f\" && [ -e \"$f\" ] && {{ echo \"$f\"; break; }}; done", q(&ro_path))).filter(|f| !f.is_empty()).unwrap_or_else(|| ".gitignore".into());
    let ctime = |f: &str| sh(&host, &format!("stat -c %Z {}", q(&format!("{ro_path}/{f}"))));
    let before_ctime = ctime(&probe_file);
    let start = Instant::now();
    let ro = readonly_tab(&engine, &mut clean, &ws_id, "kt e2e ro");
    let took = start.elapsed();
    match &ro {
        Ok(row) => {
            r.within(took, Duration::from_secs(3), "read-only main tab (50 commits to catch up)");
            r.check(row.worktree.ends_with("kitchen-readonly"), format!("shared read-only worktree {}", row.worktree));
            r.check(head(&host, &ro_path) == origin_head(&host, &env.repo, &base), format!("read-only at origin/{base}"));
            r.check(locked(&host, &ro_path), "read-only folders locked");
            r.check(ctime(&probe_file) == before_ctime, format!("an unchanged file's ctime didn't move ({probe_file})"));
            let (secs, out) = status(&host, &ro_path);
            r.check(out.as_deref() == Some("") && secs < 2.5, format!("read-only clean, status {secs:.2}s ({out:?})"));
            let start = Instant::now();
            let again = readonly_tab(&engine, &mut clean, &ws_id, "kt e2e ro again");
            r.check(again.is_ok(), "read-only re-open");
            r.within(start.elapsed(), Duration::from_secs(2), "read-only re-open at the same commit");
        }
        Err(e) => {
            r.check(false, format!("read-only main tab: {e:#}"));
        }
    }

    // 13. Folder-only lock: in-place edits to files succeed (documented), and
    //     a re-open at the same commit should not keep them.
    let tracked_file = format!("{ro_path}/{probe_file}");
    let edited = sh(&host, &format!("chmod u+w {f} 2>/dev/null; printf 'kt-e2e\\n' >> {f} && echo OK", f = q(&tracked_file))).is_some();
    r.check(edited, "in-place edit to a read-only file succeeds (folders-only lock)");
    if edited {
        let again = readonly_tab(&engine, &mut clean, &ws_id, "kt e2e ro revert");
        let (_, out) = status(&host, &ro_path);
        r.check(again.is_ok() && out.as_deref() == Some(""), format!("re-open at the same commit reverts the edit ({out:?})"));
        sh(&host, &format!("cd {ro} && git show HEAD:{f} > {f}", ro = q(&ro_path), f = q(&probe_file)));
    }

    // 14. Two read-only main tabs at once (double ⌘I) while the worktree has
    //     to move.
    let _ = repo.sync_readonly_worktree(&ro_path, &format!("origin/{base}~200"));
    let (e1, e2, w1, w2) = (engine.clone(), engine.clone(), ws_id.clone(), ws_id.clone());
    let t1 = std::thread::spawn(move || e1.create_tab(&w1, NewTab { kind: Some("readonly".into()), title: Some("kt e2e ro3".into()), focus: Some(false), ..Default::default() }));
    std::thread::sleep(Duration::from_millis(300));
    let t2 = std::thread::spawn(move || e2.create_tab(&w2, NewTab { kind: Some("readonly".into()), title: Some("kt e2e ro4".into()), focus: Some(false), ..Default::default() }));
    let (a1, a2) = (t1.join().unwrap(), t2.join().unwrap());
    for t in [&a1, &a2].into_iter().flatten() {
        clean.tabs.push(t.id.clone());
    }
    r.check(a1.is_ok() && a2.is_ok(), format!("double ⌘I: both read-only tabs open ({:?} / {:?})", a1.as_ref().err().map(|e| format!("{e:#}")), a2.as_ref().err().map(|e| format!("{e:#}"))));
    let _ = repo.sync_readonly_worktree(&ro_path, &format!("origin/{base}"));

    // 15. ⌘P on read-only main: folder view after the list; a second viewer
    //     on the same worktree + HEAD reuses the cached list.
    if let Ok(row) = &ro {
        let mut v = viewer(&engine, &ws_id, &row.id);
        let first = frame_until(&engine, &mut v, Duration::from_secs(30), |v| listed(v) && v.browsing);
        r.check(first.is_some_and(|d| d < Duration::from_secs(4)), format!("read-only ⌘P folder view {first:?} < 4s"));
        r.check(Some(v.all.len()) == tracked_count(&ro_path) && v.changed.is_empty(), format!("read-only file list = git ls-files ({} paths)", v.all.len()));
        let mut v2 = viewer(&engine, &ws_id, &row.id);
        let second = frame_until(&engine, &mut v2, Duration::from_secs(30), listed);
        r.check(second.zip(first).is_some_and(|(b, a)| b < a) && Arc::ptr_eq(&v.all, &v2.all), format!("second viewer reuses the cached list ({second:?} vs {first:?})"));
    }

    // 16. Edit tab, live.
    if let Some(row) = edits.first() {
        let wt = q(&row.worktree);
        let mut v = viewer(&engine, &ws_id, &row.id);
        r.check(frame_until(&engine, &mut v, Duration::from_secs(20), |v| listed(v) && !v.changes_pending).is_some(), "edit tab viewer lists");
        // An idle poll: about one listing, and no new file list.
        let all0 = v.all.clone();
        let poll = poll_now(&engine, &mut v);
        r.check(poll.is_some_and(|d| d < Duration::from_millis(1500)), format!("idle 3 s poll {poll:?} < 1.5s"));
        frame_until(&engine, &mut v, Duration::from_millis(500), |_| false);
        r.check(Arc::ptr_eq(&all0, &v.all), "idle poll keeps the same file list (no re-send)");
        // A new file shows up as changed and in the list.
        sh(&host, &format!("echo e2e > {wt}/kt_e2e_new_file.txt"));
        poll_now(&engine, &mut v);
        let seen = frame_until(&engine, &mut v, Duration::from_secs(20), |v| v.all.iter().any(|p| p == "kt_e2e_new_file.txt"));
        r.check(seen.is_some() && v.changed.iter().any(|c| c.path == "kt_e2e_new_file.txt"), format!("new file listed as changed ({seen:?})"));
        r.check(v.all.len() == all0.len() + 1, "new file merged into the file list");
        // One failed poll (git unreachable), then recovery with nothing changed.
        let dotgit = format!("{}/.git", row.worktree);
        let away = format!("{dotgit}.kt-e2e-away");
        clean.restore.push((away.clone(), dotgit.clone()));
        sh(&host, &format!("mv {} {}", q(&dotgit), q(&away)));
        poll_now(&engine, &mut v);
        let failed = matches!(v.files_state, Some(Err(_)));
        sh(&host, &format!("mv {} {}", q(&away), q(&dotgit)));
        clean.restore.clear();
        r.check(failed, "a poll with git unreachable reports the error");
        poll_now(&engine, &mut v);
        poll_now(&engine, &mut v);
        r.check(listed(&v), format!("the viewer recovers on the next good poll ({:?})", v.files_state));
        // ⌘R while a quiet poll is in flight still ends listed.
        v.changes_at = Instant::now() - Duration::from_secs(10);
        frame(&engine, &mut v);
        v.command(PanelCmd::Reload, &engine);
        let reloaded = frame_until(&engine, &mut v, Duration::from_secs(20), |v| listed(v) && !v.changes_pending);
        r.check(reloaded.is_some(), "⌘R during a quiet poll ends with the file list loaded");
    }

    // 17. A file staged before the first listing, then unstaged and deleted,
    //     leaves the file list (the cache holds the index of that HEAD).
    if let Ok(g) = edit_tab(&engine, &mut clean, &ws_id, "kt e2e g") {
        let wt = q(&g.worktree);
        sh(&host, &format!("echo g > {wt}/kt_e2e_ghost.txt && git -C {wt} add kt_e2e_ghost.txt"));
        let mut v = viewer(&engine, &ws_id, &g.id);
        let had = frame_until(&engine, &mut v, Duration::from_secs(20), |v| listed(v) && v.all.iter().any(|p| p == "kt_e2e_ghost.txt"));
        r.check(had.is_some(), "staged file in the list");
        sh(&host, &format!("git -C {wt} rm -q --cached kt_e2e_ghost.txt && rm {wt}/kt_e2e_ghost.txt"));
        poll_now(&engine, &mut v);
        let gone = frame_until(&engine, &mut v, Duration::from_secs(10), |v| !v.all.iter().any(|p| p == "kt_e2e_ghost.txt"));
        r.check(gone.is_some(), "unstaged + deleted file leaves the list");
        v.command(PanelCmd::Reload, &engine);
        frame_until(&engine, &mut v, Duration::from_secs(20), |v| listed(v) && !v.changes_pending);
        r.check(!v.all.iter().any(|p| p == "kt_e2e_ghost.txt"), "and stays gone after ⌘R");
    }

    // 18. A rev tab for a rev that doesn't exist fails cleanly, spares intact.
    let bad = engine.create_tab(&ws_id, NewTab { kind: Some("readonly".into()), rev: Some("refs/kitchen/pr/kt-e2e-missing".into()), name: Some("kt-e2e-missing".into()), no_agent: true, focus: Some(false), ..Default::default() });
    r.check(bad.is_err(), "unknown rev → error");
    r.check(sh(&host, &format!("test -e {} && echo Y", q(&repo.sibling_path("kitchen-ro-kt-e2e-missing")))).is_none(), "no worktree left for it");
    r.check(spares_settled(&host, &repo, &base), "spares intact after the failed open");

    drop(clean);
    let _ = std::fs::remove_dir_all(&plugin_dir);
    // Nothing of ours left behind.
    let left = sh(&host, &format!("git -C {r_q} worktree list | grep -c -e kt-e2e -e kitchen-ro-pr-{n} ; git -C {r_q} branch --list '*kt-e2e*' | wc -l; ls -d {p}/*kt-e2e* 2>/dev/null | wc -l; git -C {r_q} for-each-ref refs/kt-e2e refs/kitchen/pr/{n} | wc -l", n = env.pr, p = q(repo.parent_dir())));
    r.check(left.as_deref().is_some_and(|l| l.split_whitespace().all(|n| n == "0")), format!("cleanup left nothing ({left:?})"));
    let locks = sh(&host, &format!("for d in \"$(git -C {r_q} rev-parse --path-format=absolute --git-common-dir)\"/worktrees/*; do [ -e \"$d/locked\" ] && echo \"$d\"; done; true"));
    r.check(locks.as_deref() == Some(""), format!("no worktree locks left ({locks:?})"));
    r.check(head(&host, &ro_path) == origin_head(&host, &env.repo, &base), "shared read-only worktree left at origin/<base>");
    r.finish();
}
