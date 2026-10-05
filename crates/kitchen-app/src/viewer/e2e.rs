//! Manual end-to-end test of fast tabs and ⌘P against a real devbox repo.
//!
//! Drives the real entry points (Engine::create_tab, the github plugin's ⌘I
//! handler via Engine::open_text, Engine::refill_spare, Viewer::new) against
//! a real host. Creates and then removes its own tabs, worktrees, branches
//! and PR refs; leaves the spares refreshed and the shared read-only
//! worktree at origin/<default> (as the app would).
//!
//! ```sh
//! KT_E2E_HOST=willbox-1 KT_E2E_REPO=/home/dev/agi KT_E2E_SLUG=ppl-ai/agi KT_E2E_PR=234588 \
//!   cargo test -p kitchen-app e2e_fast_tabs -- --ignored --nocapture --test-threads=1
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use kitchen_core::git::{Repo, SPARE_COUNT};
use kitchen_core::host::Host;

use super::Viewer;
use crate::engine::{Engine, NewTab};
use crate::panels::PanelCtx;

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

/// Shell on the target; stdout trimmed, or `ERR <msg>` (never panics).
fn sh(host: &Host, script: &str) -> String {
    match host.sh(script, None) {
        Ok(o) if o.ok() => o.stdout.trim().to_string(),
        Ok(o) => format!("ERR exit {}: {}", o.status, o.stderr.trim()),
        Err(e) => format!("ERR {e:#}"),
    }
}

fn q(s: &str) -> String {
    kitchen_core::util::shell_quote(s)
}

/// Seconds `git status --porcelain` takes in `wt`, and its output.
fn status(host: &Host, wt: &str) -> (f64, String) {
    let start = Instant::now();
    let out = sh(host, &format!("git -C {} status --porcelain", q(wt)));
    (start.elapsed().as_secs_f64(), out)
}

/// The worktree's admin dir is named after the path it was created at, and
/// `git worktree move` keeps it: a tab that took over a spare points into
/// `.git/worktrees/<repo>-kitchen-spare*`.
fn took_spare(host: &Host, wt: &str) -> bool {
    sh(host, &format!("cat {}/.git", q(wt))).contains("-kitchen-spare")
}

fn origin_head(host: &Host, repo: &str, base: &str) -> String {
    sh(host, &format!("git -C {} rev-parse origin/{base}", q(repo)))
}

/// Writes into the worktree's top folder fail (directories are locked).
fn locked(host: &Host, wt: &str) -> bool {
    sh(host, &format!("touch {}/kt-e2e-probe 2>/dev/null && echo WRITABLE || echo LOCKED", q(wt))) == "LOCKED"
}

/// Every spare exists, is detached at origin/<base> and not locked.
fn spares_fresh(host: &Host, repo: &Repo, base: &str) -> bool {
    let head = origin_head(host, repo.path, base);
    repo.spare_paths().iter().all(|sp| {
        let s = sh(
            host,
            &format!(
                "git -C {sp} symbolic-ref -q HEAD >/dev/null && echo BRANCH; git -C {sp} rev-parse HEAD; test -e \"$(git -C {sp} rev-parse --git-dir)/locked\" && echo LOCKED; true",
                sp = q(sp)
            ),
        );
        s == head
    })
}

fn wait_for(limit: Duration, mut f: impl FnMut() -> bool) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if f() {
            return Some(start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

/// Removes everything the test created, also when a check panics.
struct Cleanup {
    engine: Arc<Engine>,
    host: Host,
    repo: String,
    tabs: Vec<String>,
    worktrees: Vec<String>,
    branches: Vec<String>,
    refs: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for t in &self.tabs {
            let _ = self.engine.close_tab(t);
        }
        let repo = Repo::new(&self.host, &self.repo);
        for wt in &self.worktrees {
            if let Err(e) = repo.remove_worktree(wt, true) {
                eprintln!("cleanup: {wt}: {e:#}");
            }
        }
        for b in &self.branches {
            let _ = self.host.run(&["git", "-C", &self.repo, "branch", "-D", b], None);
        }
        for r in &self.refs {
            let _ = self.host.run(&["git", "-C", &self.repo, "update-ref", "-d", r], None);
        }
        for sp in repo.spare_paths() {
            let _ = sh(&self.host, &format!("[ \"$(cat \"$(git -C {sp} rev-parse --git-dir)/locked\" 2>/dev/null)\" = kitchen-refresh ] && git -C {r} worktree unlock {sp}; true", sp = q(&sp), r = q(&self.repo)));
        }
        eprintln!("cleanup: removed {} tabs, {} worktrees, {} branches", self.tabs.len(), self.worktrees.len(), self.branches.len());
    }
}

/// Opens a viewer on `tab_id` and frames it until `done`; seconds until
/// the first diff (or folder view) and until the full file list.
fn view(engine: &Arc<Engine>, ws_id: &str, tab_id: &str, limit: Duration) -> (Viewer, Option<f64>, Option<f64>) {
    let ws = engine.workspace(ws_id).unwrap();
    let tab = ws.tab(tab_id).unwrap().clone();
    let ctx = engine.ctx.clone();
    let start = Instant::now();
    let mut v = Viewer::new(engine, &ws, &tab);
    let (mut shown, mut listed) = (None, None);
    while start.elapsed() < limit && (shown.is_none() || listed.is_none()) {
        let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))), ..Default::default() }, |ui| {
            let mut cx = PanelCtx { engine, focused: true, clicked: false };
            v.ui(ui, &mut cx);
        });
        out.textures_delta.clear();
        let t = start.elapsed().as_secs_f64();
        let diff_up = v.current.as_ref().is_some_and(|p| v.files.contains_key(p) && v.bases.contains_key(p)) && !v.doc.lines.is_empty();
        let folder_up = v.browsing && v.files_state.is_some();
        if shown.is_none() && (diff_up || folder_up) {
            shown = Some(t);
        }
        if listed.is_none() && matches!(v.files_state, Some(Ok(()))) {
            listed = Some(t);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    (v, shown, listed)
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

    // An isolated engine: in-memory DB, readonly agents off (no claude on the box).
    let db = kitchen_core::db::Db::open_in_memory().unwrap();
    let settings = crate::settings::Settings { autostart_readonly_agent: false, ..Default::default() };
    db.set_setting("settings", &serde_json::to_string(&settings).unwrap()).unwrap();
    let ws_id = format!("e2e{}", kitchen_core::util::new_id());
    db.insert_workspace(&kitchen_core::db::WorkspaceRow {
        id: ws_id.clone(),
        name: "e2e".into(),
        host: host.label().to_string(),
        repo_path: env.repo.clone(),
        github_slug: Some(env.slug.clone()),
        default_branch: Some(base.clone()),
        position: 0,
        pinned: false,
    })
    .unwrap();
    let engine = Engine::new(egui::Context::default(), db).unwrap();
    // Only this branch's github plugin.
    let plugin_dir = std::env::temp_dir().join(format!("kt-e2e-plugins-{}", kitchen_core::util::new_id()));
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::os::unix::fs::symlink(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/github"), plugin_dir.join("github")).unwrap();
    engine.set_plugins(crate::plugins::PluginHost::start(&engine, vec![plugin_dir.clone()]));
    let mut clean = Cleanup { engine: engine.clone(), host: host.clone(), repo: env.repo.clone(), tabs: vec![], worktrees: vec![], branches: vec![], refs: vec![] };
    let pr_path = repo.sibling_path(&format!("kitchen-ro-pr-{}", env.pr));
    let pr_ref = format!("refs/kitchen/pr/{}", env.pr);
    r.check(sh(&host, &format!("test -e {} && echo EXISTS || echo FREE", q(&pr_path))) == "FREE", format!("precondition: {pr_path} does not exist yet"));
    eprintln!("target: {} {} ({} files), PR {}#{}", host.label(), env.repo, sh(&host, &format!("git -C {} ls-files | wc -l", q(&env.repo))), env.slug, env.pr);

    // 1. Spares: refill brings SPARE_COUNT of them to origin/<base>.
    sh(&host, &format!("git -C {} fetch -q --no-tags origin +refs/heads/{base}:refs/remotes/origin/{base}", q(&env.repo)));
    engine.refill_spare(&ws_id);
    let took = wait_for(Duration::from_secs(300), || spares_fresh(&host, &repo, &base));
    r.check(took.is_some(), format!("{SPARE_COUNT} spares fresh at origin/{base} after refill ({:.0}s)", took.map(|d| d.as_secs_f64()).unwrap_or(-1.0)));

    // 2. PR tab from a link (⌘I): gh + fetch in parallel, takes over a spare.
    let gh_head = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("export PATH=/opt/homebrew/bin:/usr/local/bin:$PATH; gh pr view {} -R {} --json headRefOid -q .headRefOid", env.pr, env.slug))
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let url = format!("https://github.com/{}/pull/{}", env.slug, env.pr);
    let start = Instant::now();
    engine.open_text(&url).expect("github plugin handles the link");
    clean.worktrees.push(pr_path.clone());
    clean.refs.push(pr_ref.clone());
    let find_pr = || engine.workspace(&ws_id).unwrap().tabs.iter().find(|t| t.row.worktree.ends_with(&format!("kitchen-ro-pr-{}", env.pr))).cloned();
    let opened = wait_for(Duration::from_secs(120), || find_pr().is_some()).map(|_| start.elapsed());
    let pr_tab = find_pr();
    r.check(pr_tab.is_some(), "PR link opens a read-only tab");
    if let Some(d) = opened {
        r.within(d, Duration::from_secs(5), "PR tab open (gh + fetch + worktree + tmux)");
    }
    if let Some(t) = &pr_tab {
        clean.tabs.push(t.row.id.clone());
        let head = sh(&host, &format!("git -C {} rev-parse HEAD", q(&t.row.worktree)));
        r.check(!gh_head.is_empty() && head == gh_head, format!("PR worktree at the PR head ({head} vs gh {gh_head})"));
        r.check(t.row.diff_base.as_deref() == Some(&format!("origin/{base}")), format!("diff base origin/{base} ({:?})", t.row.diff_base));
        r.check(took_spare(&host, &t.row.worktree), "PR worktree took over a spare (no full checkout)");
        r.check(locked(&host, &t.row.worktree), "PR worktree folders are locked");
        let (secs, out) = status(&host, &t.row.worktree);
        r.check(out.is_empty(), format!("PR worktree clean ({out:.200})"));
        r.check(secs < 2.5, format!("git status in the PR worktree {secs:.2}s < 2.5s (no re-hash after locking)"));
        r.check(sh(&host, &format!("git -C {} rev-parse --verify -q {pr_ref}", q(&env.repo))) == gh_head, "refs/kitchen/pr/N fetched");
    }

    // 3. ⌘P on the PR tab: first diff before the full file list.
    if let Some(t) = &pr_tab {
        let (v, shown, listed) = view(&engine, &ws_id, &t.row.id, Duration::from_secs(30));
        r.check(shown.is_some_and(|s| s < 1.5), format!("PR ⌘P first diff {shown:?}s < 1.5s"));
        r.check(listed.is_some_and(|s| s < 4.0), format!("PR ⌘P full file list {listed:?}s < 4s ({} files)", v.all.len()));
        r.check(v.all.len() > 1000 && v.diff && !v.changed.is_empty(), format!("PR viewer in diff mode on {:?}, {} changed", v.current, v.changed.len()));
        r.check(v.doc.hunk_starts().contains(&v.vim.line), "cursor on the first change");
    }

    // 5. Edit tabs back to back: each takes a spare (ref-only) and is fast.
    let took = wait_for(Duration::from_secs(300), || spares_fresh(&host, &repo, &base));
    r.check(took.is_some(), "spares refilled after the PR tab took one");
    let main_head = origin_head(&host, &env.repo, &base);
    let mut edits = Vec::new();
    for name in ["kt e2e a", "kt e2e b"] {
        let start = Instant::now();
        let row = engine.create_tab(&ws_id, NewTab { kind: Some("edit".into()), title: Some(name.into()), no_agent: true, focus: Some(false), ..Default::default() });
        let took = start.elapsed();
        let Ok(row) = row else {
            r.check(false, format!("edit tab {name:?}: {:#}", row.unwrap_err()));
            continue;
        };
        clean.tabs.push(row.id.clone());
        clean.worktrees.push(row.worktree.clone());
        clean.branches.extend(row.branch.clone());
        r.within(took, Duration::from_secs(3), &format!("edit tab {name:?}"));
        r.check(took_spare(&host, &row.worktree), format!("edit tab {name:?} took over a spare"));
        r.check(sh(&host, &format!("git -C {} rev-parse HEAD", q(&row.worktree))) == main_head, format!("edit tab {name:?} at origin/{base}"));
        r.check(row.branch.as_deref().is_some_and(|b| b.ends_with(&kitchen_core::util::slugify(name, 40))), format!("branch {:?}", row.branch));
        let (secs, out) = status(&host, &row.worktree);
        r.check(out.is_empty() && secs < 2.5, format!("edit tab {name:?} clean, status {secs:.2}s"));
        edits.push(row);
    }
    // Re-opening the PR link: no new tab, no move; it selects the PR tab again.
    if let (Some(t), Some(e)) = (&pr_tab, edits.first()) {
        engine.select_tab(&e.id);
        let n = engine.workspace(&ws_id).unwrap().tabs.len();
        let start = Instant::now();
        engine.open_text(&url).unwrap();
        let selected = || engine.workspace(&ws_id).unwrap().selected_tab.as_deref() == Some(t.row.id.as_str());
        let done = wait_for(Duration::from_secs(30), selected);
        r.check(done.is_some() && engine.workspace(&ws_id).unwrap().tabs.len() == n, "re-opening the PR link reuses its tab");
        r.within(start.elapsed(), Duration::from_secs(4), "PR re-open (gh + fetch + same-head check)");
        r.check(sh(&host, &format!("git -C {} rev-parse HEAD", q(&t.row.worktree))) == gh_head, "PR worktree still at the PR head");
    }
    r.check(sh(&host, &format!("grep -cxF .kitchen/ \"$(git -C {} rev-parse --git-common-dir)/info/exclude\"", q(&env.repo))) == "1", ".kitchen/ excluded exactly once");

    // 6. A locked spare (git refuses to move it) is skipped; a stale
    //    kitchen-refresh lock is cleared by the next refresh.
    let took = wait_for(Duration::from_secs(300), || spares_fresh(&host, &repo, &base));
    r.check(took.is_some(), "spares refilled after two edit tabs");
    std::thread::sleep(Duration::from_secs(5)); // let the refresh thread finish its no-op pass
    let spares = repo.spare_paths();
    let lock_reason = |sp: &str| sh(&host, &format!("cat \"$(git -C {} rev-parse --git-dir)/locked\" 2>/dev/null; true", q(sp)));
    sh(&host, &format!("git -C {} worktree lock --reason e2e-hold {}", q(&env.repo), q(&spares[0])));
    let c = engine.create_tab(&ws_id, NewTab { kind: Some("edit".into()), title: Some("kt e2e c".into()), no_agent: true, focus: Some(false), ..Default::default() });
    match c {
        Ok(row) => {
            clean.tabs.push(row.id.clone());
            clean.worktrees.push(row.worktree.clone());
            clean.branches.extend(row.branch.clone());
            r.check(lock_reason(&spares[0]) == "e2e-hold", "locked spare left in place");
            r.check(took_spare(&host, &row.worktree), "the edit tab took the other spare");
        }
        Err(e) => {
            r.check(false, format!("edit tab next to a locked spare: {e:#}"));
        }
    }
    sh(&host, &format!("git -C {r} worktree unlock {sp} && git -C {r} worktree lock --reason kitchen-refresh {sp}", r = q(&env.repo), sp = q(&spares[0])));
    std::thread::sleep(Duration::from_secs(5));
    engine.refill_spare(&ws_id);
    let took = wait_for(Duration::from_secs(300), || spares_fresh(&host, &repo, &base));
    r.check(took.is_some() && lock_reason(&spares[0]).is_empty(), "refresh cleared the stale kitchen-refresh lock and refilled");

    // 7. Read-only main tab on the shared worktree (old code locked its files).
    let ro_path = repo.readonly_path();
    let start = Instant::now();
    let ro = engine.create_tab(&ws_id, NewTab { kind: Some("readonly".into()), title: Some("kt e2e ro".into()), focus: Some(false), ..Default::default() });
    let took = start.elapsed();
    match &ro {
        Ok(row) => {
            clean.tabs.push(row.id.clone());
            r.within(took, Duration::from_secs(3), "read-only main tab");
            r.check(row.worktree.ends_with("kitchen-readonly"), format!("shared read-only worktree {}", row.worktree));
            r.check(sh(&host, &format!("git -C {} rev-parse HEAD", q(&ro_path))) == origin_head(&host, &env.repo, &base), format!("read-only at origin/{base}"));
            r.check(locked(&host, &ro_path), "read-only folders locked");
            let (secs, out) = status(&host, &ro_path);
            r.check(out.is_empty() && secs < 2.5, format!("read-only clean, status {secs:.2}s ({out:.200})"));
            // A second sync at the same commit: no checkout.
            let start = Instant::now();
            let again = repo.sync_readonly_worktree_fetching(&ro_path, &format!("origin/{base}"), &[]);
            r.check(again.as_ref().is_ok_and(|(_, moved)| !moved), "re-sync at the same commit doesn't move");
            r.within(start.elapsed(), Duration::from_secs(2), "re-sync at the same commit");
        }
        Err(e) => {
            r.check(false, format!("read-only main tab: {e:#}"));
        }
    }

    // 8. ⌘P on read-only main: folder view after the list; a second viewer
    //    on the same worktree + HEAD gets the list from the cache.
    if let Ok(row) = &ro {
        let (v, shown, listed) = view(&engine, &ws_id, &row.id, Duration::from_secs(30));
        r.check(listed.is_some_and(|s| s < 4.0), format!("read-only ⌘P file list {listed:?}s < 4s ({} files)", v.all.len()));
        r.check(shown.is_some() && v.browsing && v.changed.is_empty(), format!("read-only opens on folder {:?}", v.dir));
        let (v2, _, listed2) = view(&engine, &ws_id, &row.id, Duration::from_secs(30));
        r.check(listed2.zip(listed).is_some_and(|(b, a)| b < a) && Arc::ptr_eq(&v.all, &v2.all), format!("second viewer reuses the cached list ({listed2:?}s)"));
    }

    // 9. Edit tab, live: an agent's new file shows up as changed and in the list.
    if let Some(row) = edits.first() {
        let ws = engine.workspace(&ws_id).unwrap();
        let tab = ws.tab(&row.id).unwrap().clone();
        let mut v = Viewer::new(&engine, &ws, &tab);
        let ctx = engine.ctx.clone();
        let frame = |v: &mut Viewer| {
            let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))), ..Default::default() }, |ui| {
                let mut cx = PanelCtx { engine: &engine, focused: true, clicked: false };
                v.ui(ui, &mut cx);
            });
            out.textures_delta.clear();
        };
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) && v.files_state.is_none() {
            frame(&mut v);
            std::thread::sleep(Duration::from_millis(20));
        }
        let first = v.all.clone();
        sh(&host, &format!("echo e2e > {}/kt_e2e_new_file.txt", q(&row.worktree)));
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) && !v.all.iter().any(|p| p == "kt_e2e_new_file.txt") {
            frame(&mut v);
            std::thread::sleep(Duration::from_millis(50));
        }
        r.check(v.changed.iter().any(|c| c.path == "kt_e2e_new_file.txt"), format!("edit tab: new file listed as changed after {:.1}s", start.elapsed().as_secs_f64()));
        r.check(v.all.iter().any(|p| p == "kt_e2e_new_file.txt") && v.all.len() == first.len() + 1, "edit tab: new file merged into the file list");
    }

    drop(clean);
    let _ = std::fs::remove_dir_all(&plugin_dir);
    // Nothing of ours left behind.
    let left = sh(&host, &format!("git -C {} worktree list | grep -c -e kt-e2e -e kitchen-ro-pr-{} ; git -C {} branch --list '*kt-e2e*' | wc -l", q(&env.repo), env.pr, q(&env.repo)));
    r.check(left.split_whitespace().all(|n| n == "0"), format!("cleanup left nothing ({left})"));
    r.finish();
}
