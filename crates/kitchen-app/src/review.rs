//! GitHub PR review: human comments from the viewer, agent drafts, the
//! approval gate, resolution sync, and auto-review of watched people's PRs.
//!
//! INTERFACE CONTRACT (used by the viewer): all functions are cheap for the
//! UI thread unless marked "blocking"; cached lookups kick off a background
//! refresh and return what they have.
//!
//! Invariant: agent drafts reach GitHub only through [`approve_draft`],
//! which is called from a keypress in the inbox. [`dispatch`] (the socket
//! API) has no method that publishes a draft.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use kitchen_core::db::{Draft, NewDraft};
use kitchen_core::gh::{CommentAnchor, DiffLines, Gh, PostedComment, PrInfo, ReviewThread};
use kitchen_core::review::{self as logic, CommentInput, Placement};
use kitchen_proto::Caller;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::engine::{Engine, NewTab};

const PR_TTL: Duration = Duration::from_secs(120);
const THREADS_TTL: Duration = Duration::from_secs(60);
/// Thread caches not asked for in this long stop being refreshed.
const THREADS_IDLE: Duration = Duration::from_secs(300);
/// A review run that never reported back is failed after this long.
const RUN_TIMEOUT: Duration = Duration::from_secs(3 * 3600);

type PrKey = (String, u64);

struct Cached<T> {
    value: T,
    fetched: Instant,
}

#[derive(Default)]
struct Cache {
    /// tab id -> its PR (None = the tab has no open PR).
    pr_by_tab: HashMap<String, Cached<Option<PrInfo>>>,
    threads: HashMap<PrKey, Cached<Vec<ReviewThread>>>,
    threads_wanted: HashMap<PrKey, Instant>,
    /// (slug, number, head sha) -> hunk lines.
    diffs: HashMap<(String, u64, String), Arc<DiffLines>>,
    inflight: HashSet<String>,
    viewer: Option<String>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Run `f` on a background thread unless a job with the same key is running.
fn background(engine: &Arc<Engine>, key: String, f: impl FnOnce(&Arc<Engine>) -> Result<()> + Send + 'static) {
    if !cache().lock().unwrap().inflight.insert(key.clone()) {
        return;
    }
    let engine = engine.clone();
    std::thread::spawn(move || {
        if let Err(e) = f(&engine) {
            log::warn!("{key}: {e:#}");
        }
        cache().lock().unwrap().inflight.remove(&key);
        engine.ctx.request_repaint();
    });
}

// ================================================================ lookups

/// Blocking: find the tab's PR on GitHub and cache it.
fn fetch_pr_for_tab(engine: &Arc<Engine>, tab_id: &str) -> Result<Option<PrInfo>> {
    let (ws, tab) = engine.find_tab(tab_id).context("no such tab")?;
    let Some(slug) = ws.row.github_slug.clone() else {
        return Ok(None);
    };
    let gh = Gh::default();
    let pr = match (tab.row.pr_number, &tab.row.branch) {
        (Some(n), _) => Some(gh.pr_view(&slug, n as u64)?),
        (None, Some(b)) => gh.pr_for_branch(&slug, b)?,
        (None, None) => None,
    };
    cache().lock().unwrap().pr_by_tab.insert(tab_id.to_string(), Cached { value: pr.clone(), fetched: Instant::now() });
    Ok(pr)
}

/// The PR a tab works on (its `pr_number`, or the open PR for its branch).
/// Cached; returns `None` until a background lookup finishes.
#[allow(dead_code)] // consumed by the viewer
pub fn pr_for_tab(engine: &Arc<Engine>, tab_id: &str) -> Option<PrInfo> {
    let (value, stale) = {
        let c = cache().lock().unwrap();
        match c.pr_by_tab.get(tab_id) {
            Some(e) => (e.value.clone(), e.fetched.elapsed() > PR_TTL),
            None => (None, true),
        }
    };
    if stale {
        let id = tab_id.to_string();
        background(engine, format!("pr:{tab_id}"), move |e| fetch_pr_for_tab(e, &id).map(|_| ()));
    }
    value
}

/// Blocking: the tab's PR, from cache when fresh.
fn pr_for_tab_blocking(engine: &Arc<Engine>, tab_id: &str) -> Result<PrInfo> {
    let cached = cache().lock().unwrap().pr_by_tab.get(tab_id).filter(|e| e.fetched.elapsed() < PR_TTL).map(|e| e.value.clone());
    let pr = match cached {
        Some(v) => v,
        None => fetch_pr_for_tab(engine, tab_id)?,
    };
    pr.ok_or_else(|| anyhow!("this tab's branch has no open pull request"))
}

fn fetch_threads(key: &PrKey) -> Result<Vec<ReviewThread>> {
    let threads = Gh::default().review_threads(&key.0, key.1)?;
    cache().lock().unwrap().threads.insert(key.clone(), Cached { value: threads.clone(), fetched: Instant::now() });
    Ok(threads)
}

fn refresh_threads(engine: &Arc<Engine>, key: PrKey) {
    background(engine, format!("threads:{}#{}", key.0, key.1), move |_| fetch_threads(&key).map(|_| ()));
}

/// Review threads of the tab's PR (cached, refreshed in the background
/// every ~60s while requested).
#[allow(dead_code)] // consumed by the viewer
pub fn threads_for_tab(engine: &Arc<Engine>, tab_id: &str) -> Vec<ReviewThread> {
    let Some(pr) = pr_for_tab(engine, tab_id) else { return Vec::new() };
    let key = (pr.slug.clone(), pr.number);
    let (value, stale) = {
        let mut c = cache().lock().unwrap();
        c.threads_wanted.insert(key.clone(), Instant::now());
        match c.threads.get(&key) {
            Some(e) => (e.value.clone(), e.fetched.elapsed() > THREADS_TTL),
            None => (Vec::new(), true),
        }
    };
    if stale {
        refresh_threads(engine, key);
    }
    value
}

/// Agent drafts (not yet public) for the tab's PR, shown inline in the viewer.
#[allow(dead_code)] // consumed by the viewer
pub fn drafts_for_tab(engine: &Arc<Engine>, tab_id: &str) -> Vec<Draft> {
    let Some(pr) = pr_for_tab(engine, tab_id) else { return Vec::new() };
    engine
        .db()
        .drafts_for_pr(&pr.slug, pr.number as i64)
        .map(|v| v.into_iter().filter(|d| d.state == "draft").collect())
        .unwrap_or_default()
}

/// Blocking: hunk lines of the PR at `head`.
fn diff_lines(slug: &str, number: u64, head: &str) -> Result<Arc<DiffLines>> {
    let key = (slug.to_string(), number, head.to_string());
    if let Some(d) = cache().lock().unwrap().diffs.get(&key) {
        return Ok(d.clone());
    }
    let d = Arc::new(DiffLines::parse(&Gh::default().pr_diff(slug, number)?));
    cache().lock().unwrap().diffs.insert(key, d.clone());
    Ok(d)
}

/// Blocking: the GitHub login of the kitchen's user.
fn viewer_login() -> Result<String> {
    if let Some(v) = cache().lock().unwrap().viewer.clone() {
        return Ok(v);
    }
    let v = Gh::default().viewer_login()?;
    cache().lock().unwrap().viewer = Some(v.clone());
    Ok(v)
}

// ================================================================ publishing

/// Blocking: publish `input` on the PR at its current head, as a line comment
/// when the line is in the diff, else file-level with a line reference.
fn publish(pr: &PrInfo, input: &CommentInput) -> Result<PostedComment> {
    let input = logic::validate(input)?;
    let diff = diff_lines(&pr.slug, pr.number, &pr.head_sha)?;
    let gh = Gh::default();
    match logic::place(&diff, &input) {
        Placement::Line { line, start_line, side } => {
            let anchor = CommentAnchor { path: input.path.clone(), line: Some(line), start_line, side };
            gh.post_review_comment(&pr.slug, pr.number, &pr.head_sha, &anchor, &input.body)
        }
        Placement::File { line_ref } => {
            let anchor = CommentAnchor { path: input.path.clone(), line: None, start_line: None, side: "RIGHT".into() };
            gh.post_review_comment(&pr.slug, pr.number, &pr.head_sha, &anchor, &logic::file_level_body(&input.body, line_ref))
        }
    }
}

/// Blocking. Publish William's own comment (typed with ⌘M, sent with ⌘Enter)
/// to the tab's PR. Falls back to a file-level comment when the line is not
/// in the PR diff.
#[allow(dead_code)] // consumed by the viewer
pub fn post_comment(engine: &Arc<Engine>, tab_id: &str, anchor: CommentAnchor, body: &str) -> Result<PostedComment> {
    let pr = pr_for_tab_blocking(engine, tab_id)?;
    let input = CommentInput { path: anchor.path, line: anchor.line, start_line: anchor.start_line, side: Some(anchor.side), body: body.to_string() };
    let posted = publish(&pr, &input)?;
    refresh_threads(engine, (pr.slug.clone(), pr.number));
    Ok(posted)
}

/// Blocking, UI-only (never reachable from the socket API): publish one
/// approved agent draft.
pub fn approve_draft(engine: &Arc<Engine>, draft_id: i64) -> Result<PostedComment> {
    let draft = engine.db().draft(draft_id)?.context("no such draft")?;
    if draft.state != "draft" {
        bail!("draft {draft_id} is already {}", draft.state);
    }
    // Publish against the PR's current head; a moved line degrades to file-level.
    let pr = Gh::default().pr_view(&draft.github_slug, draft.pr_number as u64)?;
    let input = CommentInput {
        path: draft.path.clone(),
        line: draft.line,
        start_line: draft.start_line,
        side: Some(draft.side.clone()),
        body: draft.body.clone(),
    };
    let posted = publish(&pr, &input)?;
    engine.db().mark_draft_published(draft_id, Some(posted.id), Some(&posted.url))?;
    refresh_threads(engine, (pr.slug.clone(), pr.number));
    engine.ctx.request_repaint();
    Ok(posted)
}

pub fn reject_draft(engine: &Arc<Engine>, draft_id: i64) -> Result<()> {
    engine.db().reject_draft(draft_id)?;
    engine.ctx.request_repaint();
    Ok(())
}

pub fn edit_draft(engine: &Arc<Engine>, draft_id: i64, body: &str) -> Result<()> {
    if body.trim().is_empty() {
        bail!("comment body is empty");
    }
    engine.db().edit_draft(draft_id, body.trim())?;
    engine.ctx.request_repaint();
    Ok(())
}

// ================================================================ auto review

/// The tab id that hosts a review run, if it is still open.
pub fn run_tab(engine: &Arc<Engine>, run: &kitchen_core::db::ReviewRun) -> Option<String> {
    let session = run.tmux_session.as_deref()?;
    engine.workspaces().into_iter().flat_map(|w| w.tabs).find(|t| t.row.tmux_session == session).map(|t| t.row.id)
}

/// Blocking: open a read-only review tab for run `run_id` and start the
/// unattended reviewer in it.
pub fn launch_run(engine: &Arc<Engine>, run_id: i64) -> Result<()> {
    let run = engine.db().review_run(run_id)?.context("no such run")?;
    let result = (|| -> Result<()> {
        let mut candidates: Vec<_> =
            engine.workspaces().into_iter().filter(|w| w.row.github_slug.as_deref() == Some(run.github_slug.as_str())).collect();
        candidates.sort_by_key(|w| !w.host().is_local());
        let ws = candidates.into_iter().next().ok_or_else(|| anyhow!("no workspace for repo {}", run.github_slug))?;
        let host = ws.host();
        let prompt_file = format!("{}/.kitchen/runs/{run_id}.md", host.home()?);
        let url = run.url.clone().unwrap_or_else(|| format!("https://github.com/{}/pull/{}", run.github_slug, run.pr_number));
        host.write_file(&prompt_file, &logic::review_prompt(&url, run_id))?;
        let tab = engine.create_tab(
            &ws.row.id,
            NewTab {
                kind: Some("readonly".into()),
                pr: Some(run.pr_number as u64),
                title: Some(format!("review #{} {}", run.pr_number, run.title.clone().unwrap_or_default())),
                command: Some(logic::review_command(&prompt_file, run_id)),
                focus: Some(false),
                ..Default::default()
            },
        )?;
        engine.db().start_review_run(run_id, &ws.row.id, &tab.worktree, &tab.tmux_session)?;
        Ok(())
    })();
    if let Err(e) = &result {
        engine.db().finish_review_run(run_id, "failed", Some(&format!("{e:#}")))?;
    }
    engine.ctx.request_repaint();
    result
}

/// Blocking: queue (and launch) a review of `slug#number` at its head.
/// Returns `None` when this head was already reviewed.
pub fn queue_review(engine: &Arc<Engine>, pr: &PrInfo) -> Result<Option<i64>> {
    let id = engine.db().insert_review_run(&pr.slug, pr.number as i64, &pr.head_sha, Some(&pr.author), Some(&pr.title), Some(&pr.url))?;
    if let Some(id) = id {
        launch_run(engine, id)?;
    }
    Ok(id)
}

/// One pass of the review-request poller.
pub fn poll_once(engine: &Arc<Engine>) -> Result<usize> {
    let watch = engine.db().watch_list()?;
    if watch.is_empty() {
        return Ok(0);
    }
    let gh = Gh::default();
    let mut started = 0;
    for hit in gh.review_requested()? {
        let requesters = if logic::should_auto_review(&hit.author, &[], &watch) {
            Vec::new()
        } else {
            gh.review_requesters(&hit.slug, hit.number).unwrap_or_default()
        };
        if !logic::should_auto_review(&hit.author, &requesters, &watch) {
            continue;
        }
        let pr = gh.pr_view(&hit.slug, hit.number)?;
        match queue_review(engine, &pr) {
            Ok(Some(_)) => started += 1,
            Ok(None) => {}
            Err(e) => engine.toast_err(format!("auto-review {}#{}: {e:#}", pr.slug, pr.number)),
        }
    }
    // Runs that never reported back.
    let running = engine.db().runs_with_status("running")?;
    for run in running {
        let age = kitchen_core::util::now() - run.started_at.unwrap_or(0);
        if age > RUN_TIMEOUT.as_secs() as i64 {
            engine.db().finish_review_run(run.id, "failed", Some("timed out without `kitchenctl review done`"))?;
        }
    }
    Ok(started)
}

/// Keep recently viewed PRs' threads fresh, so resolutions made by agents
/// (or on github.com) show up without a manual reload.
fn refresh_wanted_threads(engine: &Arc<Engine>) {
    let keys: Vec<PrKey> = {
        let mut c = cache().lock().unwrap();
        c.threads_wanted.retain(|_, at| at.elapsed() < THREADS_IDLE);
        c.threads_wanted.keys().cloned().collect()
    };
    for k in keys {
        refresh_threads(engine, k);
    }
}

/// Start the background pollers (review requests + thread resolution).
pub fn start_poller(engine: &Arc<Engine>) {
    let e = engine.clone();
    std::thread::Builder::new()
        .name("review-poller".into())
        .spawn(move || {
            let mut last_poll: Option<Instant> = None;
            loop {
                let s = e.settings();
                let every = Duration::from_secs(s.review_poll_secs.max(30));
                if s.review_poll_enabled && last_poll.map(|t| t.elapsed() >= every).unwrap_or(true) {
                    last_poll = Some(Instant::now());
                    match poll_once(&e) {
                        Ok(n) if n > 0 => e.toast(format!("started {n} automatic review(s)")),
                        Ok(_) => {}
                        Err(err) => log::warn!("review poll: {err:#}"),
                    }
                }
                refresh_wanted_threads(&e);
                std::thread::sleep(THREADS_TTL);
            }
        })
        .expect("spawn review poller");
}

// ================================================================ API

#[derive(Deserialize, Default)]
#[serde(default)]
struct TabTarget {
    workspace: Option<String>,
    tab: Option<String>,
}

fn p<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|e| anyhow!("bad params: {e}"))
}

fn thread_json(t: &ReviewThread) -> Value {
    let first = t.comments.first();
    json!({
        "id": t.id,
        "path": t.path,
        "line": t.anchor_line(),
        "start_line": t.start_line,
        "side": t.side,
        "resolved": t.is_resolved,
        "outdated": t.is_outdated,
        "author": first.map(|c| c.author.clone()),
        "body": first.map(|c| c.body.clone()),
        "url": first.map(|c| c.url.clone()),
        "replies": t.comments.iter().skip(1).map(|c| json!({"author": c.author, "body": c.body})).collect::<Vec<_>>(),
    })
}

/// API methods under `review.*` and `pr.*`. `None` = not ours.
pub fn dispatch(engine: &Arc<Engine>, method: &str, params: Value, caller: &Caller) -> Option<Result<Value>> {
    let r = match method {
        "pr.info" => (|| {
            let t: TabTarget = p(params)?;
            let tab = engine.resolve_tab(t.tab.as_deref(), t.workspace.as_deref(), caller)?;
            Ok(serde_json::to_value(fetch_pr_for_tab(engine, &tab)?)?)
        })(),
        "pr.threads" => (|| {
            #[derive(Deserialize, Default)]
            #[serde(default)]
            struct P {
                workspace: Option<String>,
                tab: Option<String>,
                unresolved: bool,
            }
            let a: P = p(params)?;
            let tab = engine.resolve_tab(a.tab.as_deref(), a.workspace.as_deref(), caller)?;
            let pr = fetch_pr_for_tab(engine, &tab)?.ok_or_else(|| anyhow!("this tab's branch has no open pull request"))?;
            let threads = fetch_threads(&(pr.slug.clone(), pr.number))?;
            let list: Vec<Value> = threads.iter().filter(|t| !a.unresolved || !t.is_resolved).map(thread_json).collect();
            Ok(json!({"pr": pr.number, "slug": pr.slug, "url": pr.url, "threads": list}))
        })(),
        "pr.resolve" => (|| {
            #[derive(Deserialize)]
            struct P {
                workspace: Option<String>,
                tab: Option<String>,
                thread_id: String,
                reply: Option<String>,
            }
            let a: P = p(params)?;
            let tab = engine.resolve_tab(a.tab.as_deref(), a.workspace.as_deref(), caller)?;
            let pr = fetch_pr_for_tab(engine, &tab)?.ok_or_else(|| anyhow!("this tab's branch has no open pull request"))?;
            let key = (pr.slug.clone(), pr.number);
            let threads = fetch_threads(&key)?;
            let thread = threads.iter().find(|t| t.id == a.thread_id).ok_or_else(|| anyhow!("thread {} is not on PR #{}", a.thread_id, pr.number))?;
            let viewer = viewer_login()?;
            let first = thread.comments.first();
            if !logic::can_resolve(first.map(|c| c.author.as_str()), &viewer) {
                bail!("only threads started by {viewer} can be resolved by agents; this one is by {}", first.map(|c| c.author.as_str()).unwrap_or("?"));
            }
            let gh = Gh::default();
            if let (Some(reply), Some(c)) = (a.reply.as_deref().filter(|r| !r.trim().is_empty()), first) {
                gh.reply(&pr.slug, pr.number, c.database_id, reply)?;
            }
            if !thread.is_resolved {
                gh.set_thread_resolved(&thread.id, true)?;
            }
            refresh_threads(engine, key);
            Ok(json!({"resolved": thread.id}))
        })(),
        "review.inbox.open" => {
            engine.push_ui(crate::engine::UiCommand::OpenInbox);
            Ok(json!({"opened": true}))
        }
        "review.watch.list" => engine.db().watch_list().map(|w| json!(w)),
        "review.watch.add" | "review.watch.remove" => (|| {
            #[derive(Deserialize)]
            struct P {
                login: String,
            }
            let a: P = p(params)?;
            let login = a.login.trim().trim_start_matches('@').to_string();
            if login.is_empty() {
                bail!("login is empty");
            }
            if method == "review.watch.add" {
                engine.db().watch(&login)?;
            } else {
                engine.db().unwatch(&login)?;
            }
            Ok(json!(engine.db().watch_list()?))
        })(),
        "review.runs" => (|| {
            #[derive(Deserialize, Default)]
            #[serde(default)]
            struct P {
                limit: Option<i64>,
            }
            let a: P = p(params)?;
            Ok(serde_json::to_value(engine.db().review_runs(a.limit.unwrap_or(20))?)?)
        })(),
        "review.start" => (|| {
            #[derive(Deserialize)]
            struct P {
                pr_url: String,
            }
            let a: P = p(params)?;
            let (slug, n) = logic::parse_pr_url(&a.pr_url).ok_or_else(|| anyhow!("not a GitHub PR URL: {}", a.pr_url))?;
            let pr = Gh::default().pr_view(&slug, n)?;
            if let Some(id) = queue_review(engine, &pr)? {
                return Ok(json!({"run": id, "head_sha": pr.head_sha}));
            }
            // Same head again: retry only a failed or orphaned run.
            let retry = engine.db().requeue_failed_run(&slug, n as i64, &pr.head_sha)?;
            match retry {
                Some(id) => {
                    launch_run(engine, id)?;
                    Ok(json!({"run": id, "head_sha": pr.head_sha, "retry": true}))
                }
                None => bail!("{slug}#{n} was already reviewed at {}", pr.head_sha),
            }
        })(),
        "review.draft.add" => (|| {
            #[derive(Deserialize)]
            struct P {
                run: Option<i64>,
                slug: Option<String>,
                pr: Option<u64>,
                workspace: Option<String>,
                tab: Option<String>,
                path: String,
                line: Option<i64>,
                start_line: Option<i64>,
                side: Option<String>,
                body: String,
                severity: Option<String>,
            }
            let a: P = p(params)?;
            let (slug, number, head, run_id) = if let Some(run_id) = a.run {
                let run = engine.db().review_run(run_id)?.ok_or_else(|| anyhow!("no review run {run_id}"))?;
                (run.github_slug, run.pr_number as u64, run.head_sha, Some(run_id))
            } else if let (Some(slug), Some(n)) = (a.slug.clone(), a.pr) {
                let pr = Gh::default().pr_view(&slug, n)?;
                (slug, n, pr.head_sha, None)
            } else {
                let tab = engine.resolve_tab(a.tab.as_deref(), a.workspace.as_deref(), caller)?;
                let pr = pr_for_tab_blocking(engine, &tab)?;
                (pr.slug, pr.number, pr.head_sha, None)
            };
            let input = logic::validate(&CommentInput { path: a.path, line: a.line, start_line: a.start_line, side: a.side, body: a.body })?;
            let diff = diff_lines(&slug, number, &head)?;
            let placement = logic::place(&diff, &input);
            let (line, start_line, side, body) = match &placement {
                Placement::Line { line, start_line, side } => (Some(*line), *start_line, side.clone(), input.body.clone()),
                Placement::File { line_ref } => (None, None, "RIGHT".to_string(), logic::file_level_body(&input.body, *line_ref)),
            };
            let id = engine.db().add_draft(&NewDraft {
                run_id,
                github_slug: slug,
                pr_number: number as i64,
                head_sha: Some(head),
                path: input.path,
                line,
                start_line,
                side: Some(side),
                body,
                severity: a.severity.map(|s| s.to_ascii_lowercase()),
            })?;
            engine.ctx.request_repaint();
            Ok(json!({"id": id, "file_level": placement.is_file_level(), "state": "draft",
                      "note": "saved as a draft; William approves it before anything is posted"}))
        })(),
        "review.draft.list" => (|| {
            #[derive(Deserialize, Default)]
            #[serde(default)]
            struct P {
                state: Option<String>,
                run: Option<i64>,
                slug: Option<String>,
                pr: Option<i64>,
            }
            let a: P = p(params)?;
            let db = engine.db();
            let drafts = match (a.run, a.slug, a.pr) {
                (Some(r), _, _) => db.drafts_for_run(r)?,
                (None, Some(s), Some(n)) => db.drafts_for_pr(&s, n)?,
                _ => db.drafts_with_state(a.state.as_deref().unwrap_or("draft"))?,
            };
            let drafts: Vec<_> = drafts.into_iter().filter(|d| a.state.as_deref().map(|s| d.state == s).unwrap_or(true)).collect();
            Ok(serde_json::to_value(drafts)?)
        })(),
        "review.done" => (|| {
            #[derive(Deserialize)]
            struct P {
                run: i64,
                exit: Option<i32>,
                error: Option<String>,
            }
            let a: P = p(params)?;
            let run = engine.db().review_run(a.run)?.ok_or_else(|| anyhow!("no review run {}", a.run))?;
            if run.status != "running" && run.status != "queued" {
                // The trailing `review done` after the agent's own call is a no-op.
                return Ok(json!({"run": a.run, "status": run.status}));
            }
            let drafts = engine.db().drafts_for_run(a.run)?.len();
            let failed = a.error.is_some() || (a.exit.unwrap_or(0) != 0 && drafts == 0);
            let status = if failed { "failed" } else { "done" };
            let error = a.error.clone().or_else(|| failed.then(|| format!("reviewer exited with {}", a.exit.unwrap_or(0))));
            engine.db().finish_review_run(a.run, status, error.as_deref())?;
            let tab = run_tab(engine, &run);
            let ws = run.workspace_id.clone();
            let title = format!("review #{} {}", run.pr_number, if failed { "failed" } else { "ready" });
            let body = if failed {
                error.clone().unwrap_or_default()
            } else {
                format!("{drafts} draft comment(s) on {} — approve in the inbox (⌘I)", run.title.clone().unwrap_or_default())
            };
            engine.notify(ws.as_deref(), tab.as_deref(), &title, &body)?;
            Ok(json!({"run": a.run, "status": status, "drafts": drafts}))
        })(),
        _ => return None,
    };
    Some(r)
}
