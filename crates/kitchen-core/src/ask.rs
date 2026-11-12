//! ⌘⇧A in the viewer and the plan reviewer: a question about some lines of
//! a file or of the plan, answered next to them, and its follow-ups. A
//! thread is a Claude Code session of its own on the tab's host: its first
//! question forks the tab agent's session (`claude -p --resume <id>
//! --fork-session`), so the answers have the agent's context, model and
//! tools; without an agent session it is a fresh one. Follow-ups resume it.
//! Its transcript is kept in [`ASKS_DIR`], out of the worktree's sessions,
//! until the thread is closed.

use anyhow::Result;
use rand::Rng;

use crate::db::TabRow;
use crate::host::Host;
use crate::trace::{self, Session};
use crate::util::shell_quote;

/// Most file context sent with a question, in bytes.
const MAX_CONTEXT: usize = 60_000;

/// Put before a question to a fork of the agent's session.
const SIDE_QUESTION: &str = "[A side question from William, asked in a fork of this session from \
    the kitchen's viewer. Only your replies reach him, shown under the lines he asks about; \
    your session will not see them. He may ask follow-ups here. Answer the question, then stop: \
    do not go on with your task, and do not change any files.]\n\n";

/// Tools a question may not use: it answers, the agent changes the files.
const READ_ONLY: &str = "Edit Write NotebookEdit";

/// Claude Code project folder that holds the threads' transcripts, so the
/// worktree's newest session stays the agent's (`--continue`,
/// `trace::agent_session`). `--resume <id>` finds a session in any project.
pub const ASKS_DIR: &str = "$HOME/.claude/projects/-kitchen-asks";

/// Days a thread's transcript is kept after its last answer, if the viewer
/// that asked never closed it.
const KEEP_DAYS: u32 = 7;

/// Where and by whom a question is answered.
#[derive(Debug, Clone)]
pub struct Asker {
    pub host: Host,
    /// Where the model runs: with a session, the worktree it belongs to.
    pub cwd: String,
    /// The tab agent's session, forked for the question.
    pub session: Option<Session>,
    /// The model without a session (`ask_model`).
    pub model: String,
    /// The thread's own session id: its first question creates it.
    pub thread: String,
}

/// What is asked in a thread.
pub enum Prompt<'a> {
    File(Question<'a>),
    Plan(PlanQuestion<'a>),
    /// A follow-up: the thread already has the lines and the context.
    FollowUp(&'a str),
}

impl Asker {
    /// The asker for a question in tab `tab` (on `host`): a fork of its
    /// agent's session, or `model` in `cwd` (the checkout viewed) if it has
    /// none. Blocking: reads the host's transcripts.
    pub fn for_tab(host: Host, tab: Option<&TabRow>, cwd: &str, model: &str) -> Asker {
        let found = tab.and_then(|t| Some((t, trace::agent_session(&host, &t.worktree, t.agent_session.as_deref())?)));
        let (cwd, session) = match found {
            Some((t, s)) => (t.worktree.clone(), Some(s)),
            None => (cwd.to_string(), None),
        };
        Asker { host, cwd, session, model: model.to_string(), thread: new_session_id() }
    }

    /// The model the thread runs (`None`: Claude Code's default).
    fn thread_model(&self) -> Option<&str> {
        match &self.session {
            Some(s) => s.model.as_deref(),
            None => Some(&self.model),
        }
    }

    fn argv(&self, follow_up: bool) -> Vec<&str> {
        let mut argv = vec!["claude", "-p", "--output-format", "text", "--disallowedTools", READ_ONLY];
        match (&self.session, follow_up) {
            (_, true) => argv.extend(["--resume", &self.thread]),
            (Some(s), false) => argv.extend(["--resume", &s.id, "--fork-session", "--session-id", &self.thread]),
            (None, false) => argv.extend(["--session-id", &self.thread]),
        }
        if let Some(m) = self.thread_model() {
            argv.extend(["--model", m]);
        }
        argv
    }
}

/// A random (v4) UUID, as Claude Code's `--session-id` takes.
fn new_session_id() -> String {
    let mut b: [u8; 16] = rand::rng().random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// Moves thread `id`'s transcript (and its folder of subagent and tool
/// output) from the project it was created in to [`ASKS_DIR`], and drops
/// threads idle for [`KEEP_DAYS`].
fn stash_script(id: &str) -> String {
    format!(
        "k=\"{ASKS_DIR}\"; mkdir -p \"$k\"; \
         for f in \"$HOME\"/.claude/projects/*/{id}.jsonl \"$HOME\"/.claude/projects/*/{id}; do \
         [ -e \"$f\" ] && [ \"${{f%/*}}\" != \"$k\" ] && mv \"$f\" \"$k\"/; done; \
         find \"$k\" -mindepth 1 -maxdepth 1 -mtime +{KEEP_DAYS} -exec rm -rf {{}} +; true",
        id = shell_quote(id),
    )
}

/// What the question is about.
pub struct Question<'a> {
    /// The checkout `path` is in (a PR's is not the agent's worktree).
    pub root: &'a str,
    pub path: &'a str,
    /// The asked lines' 1-based numbers (`first == last` for one line).
    pub first: usize,
    pub last: usize,
    /// The lines are removed ones, from the diff base.
    pub base: bool,
    /// The asked lines as shown.
    pub quote: &'a str,
    /// The current file (it is cut down to the lines around the question if large).
    pub source: &'a str,
    pub text: &'a str,
}

pub fn build_prompt(q: &Question) -> String {
    let (from, ctx) = excerpt(q.source, q.first.max(1), MAX_CONTEXT);
    let to = from + ctx.lines().count().saturating_sub(1);
    let lines = if q.last > q.first { format!("lines {}–{}", q.first, q.last) } else { format!("line {}", q.first) };
    let side = if q.base { " (removed by this change; the numbers are the old file's)" } else { "" };
    format!(
        "A developer reading `{path}` (in `{root}`) in a code viewer asks a question about {lines}{side}:\n\
         ```\n{quote}\n```\n\n\
         Question: {text}\n\n\
         Answer in plain text (no Markdown headings or bold), briefly: a few sentences, \
         or a short code snippet if that answers it best. You may read other files of the \
         repository if the answer needs them; do not change anything.\n\n\
         File `{path}`, lines {from}–{to}:\n```\n{ctx}\n```\n",
        root = q.root,
        path = q.path,
        quote = q.quote,
        text = q.text,
    )
}

/// A question about lines of the tab's plan.
pub struct PlanQuestion<'a> {
    pub path: &'a str,
    pub first: usize,
    pub last: usize,
    pub quote: &'a str,
    /// The whole plan (cut down around the question if large).
    pub plan: &'a str,
    pub text: &'a str,
}

pub fn build_plan_prompt(q: &PlanQuestion) -> String {
    let (from, ctx) = excerpt(q.plan, q.first.max(1), MAX_CONTEXT);
    let to = from + ctx.lines().count().saturating_sub(1);
    let lines = if q.last > q.first { format!("lines {}–{}", q.first, q.last) } else { format!("line {}", q.first) };
    format!(
        "William is reviewing an implementation plan an agent wrote, `{path}`, and asks a \
         question about {lines}:\n\
         ```\n{quote}\n```\n\n\
         Question: {text}\n\n\
         Answer in plain text (no Markdown headings or bold), briefly: a few sentences, \
         or a short code snippet if that answers it best. If you wrote the plan, say why it \
         says what it says. You may read files of the repository if the answer needs them; \
         do not change anything.\n\n\
         Plan `{path}`, lines {from}–{to}:\n```\n{ctx}\n```\n",
        path = q.path,
        quote = q.quote,
        text = q.text,
    )
}

/// The lines of `source` around 1-based `line`, at most `max` bytes, and
/// the number of the first.
fn excerpt(source: &str, line: usize, max: usize) -> (usize, String) {
    if source.len() <= max {
        return (1, source.to_string());
    }
    let lines: Vec<&str> = source.lines().collect();
    let at = (line - 1).min(lines.len().saturating_sub(1));
    let (mut a, mut b, mut size) = (at, at, lines.get(at).map_or(0, |l| l.len() + 1));
    loop {
        let mut grew = false;
        for take_before in [true, false] {
            let next = if take_before { a.checked_sub(1) } else { Some(b + 1).filter(|&i| i < lines.len()) };
            if let Some(i) = next.filter(|&i| size + lines[i].len() + 1 <= max) {
                size += lines[i].len() + 1;
                if take_before { a = i } else { b = i }
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    (a + 1, lines.get(a..=b).unwrap_or_default().join("\n"))
}

/// Ask in `asker`'s thread. Blocking.
pub fn ask(asker: &Asker, prompt: &Prompt) -> Result<String> {
    let (text, follow_up) = match prompt {
        Prompt::File(q) => (build_prompt(q), false),
        Prompt::Plan(q) => (build_plan_prompt(q), false),
        Prompt::FollowUp(t) => (t.to_string(), true),
    };
    let text = if asker.session.is_some() && !follow_up { format!("{SIDE_QUESTION}{text}") } else { text };
    let out = asker.host.run_with_stdin(&asker.argv(follow_up), Some(&asker.cwd), Some(text.as_bytes()));
    if !follow_up {
        // A failed first question may still have started the session.
        let _ = asker.host.sh(&stash_script(&asker.thread), None);
    }
    Ok(out?.into_result("claude -p")?.trim().to_string())
}

/// Delete the thread's transcript. Blocking.
pub fn close(asker: &Asker) {
    let t = shell_quote(&asker.thread);
    let _ = asker.host.sh(&format!("rm -rf \"{ASKS_DIR}\"/{t}.jsonl \"{ASKS_DIR}\"/{t}"), None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_quotes_the_lines_and_the_file() {
        let src = "fn a() {}\nfn b() {}\n";
        let q = Question { root: "/w", path: "x.rs", first: 2, last: 2, base: false, quote: "fn b() {}", source: src, text: "what is b?" };
        let p = build_prompt(&q);
        assert!(p.contains("reading `x.rs` (in `/w`)"), "{p}");
        assert!(p.contains("about line 2:\n```\nfn b() {}\n```"), "{p}");
        assert!(p.contains("Question: what is b?"));
        assert!(p.contains("File `x.rs`, lines 1–2:"));
        let q = Question { first: 1, last: 2, base: true, ..q };
        assert!(build_prompt(&q).contains("lines 1–2 (removed by this change"));
    }

    #[test]
    fn plan_prompt_carries_the_plan() {
        let plan = "# Plan\n## F1 cache\n";
        let q = PlanQuestion { path: "/w/.kitchen/plan.md", first: 2, last: 2, quote: "## F1 cache", plan, text: "why?" };
        let p = build_plan_prompt(&q);
        assert!(p.contains("about line 2:\n```\n## F1 cache\n```"), "{p}");
        assert!(p.contains("Plan `/w/.kitchen/plan.md`, lines 1–2:"));
    }

    #[test]
    fn a_thread_forks_the_session_then_resumes_itself() {
        let session = Session { id: "0b7e-41".into(), model: Some("claude-opus-5-5".into()) };
        let asker = Asker { host: Host::Local, cwd: "/w".into(), session: Some(session), model: "sonnet".into(), thread: "t-1".into() };
        let argv = |a: &Asker, follow_up| a.argv(follow_up).join(" ");
        assert!(argv(&asker, false).contains("--disallowedTools Edit Write NotebookEdit"));
        assert!(argv(&asker, false).ends_with("--resume 0b7e-41 --fork-session --session-id t-1 --model claude-opus-5-5"), "{}", argv(&asker, false));
        assert!(argv(&asker, true).ends_with("--resume t-1 --model claude-opus-5-5"), "{}", argv(&asker, true));
        let fresh = Asker { session: None, ..asker };
        assert!(argv(&fresh, false).ends_with("--session-id t-1 --model sonnet"));
        assert!(argv(&fresh, true).ends_with("--resume t-1 --model sonnet"));
        // A tab without a transcript gets the fresh model in the checkout viewed.
        let worktree = format!("/kitchen-ask-test-{}", std::process::id());
        let tab = TabRow { id: "t".into(), workspace_id: "w".into(), title: "t".into(), worktree, branch: None, tmux_session: "t".into(), position: 0, agent: true, agent_session: Some("0b7e-41".into()) };
        let asker = Asker::for_tab(Host::Local, Some(&tab), "/pr", "sonnet");
        assert_eq!((asker.cwd.as_str(), asker.session), ("/pr", None));
    }

    #[test]
    fn session_ids_are_v4_uuids() {
        let id = new_session_id();
        let parts: Vec<usize> = id.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12], "{id}");
        assert!(id.as_bytes()[14] == b'4' && b"89ab".contains(&id.as_bytes()[19]), "{id}");
        assert_ne!(id, new_session_id());
    }

    #[test]
    fn a_thread_is_stashed_out_of_its_project() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".claude/projects");
        let (wt, asks) = (projects.join("-w-repo"), projects.join("-kitchen-asks"));
        std::fs::create_dir_all(wt.join("t-1/subagents")).unwrap();
        std::fs::write(wt.join("t-1.jsonl"), "{}\n").unwrap();
        std::fs::write(wt.join("agent.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(&asks).unwrap();
        std::fs::write(asks.join("stale.jsonl"), "{}\n").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400 * (KEEP_DAYS as u64 + 1));
        std::fs::File::options().write(true).open(asks.join("stale.jsonl")).unwrap().set_modified(old).unwrap();
        let sh = |script: String| assert!(std::process::Command::new("sh").arg("-c").arg(script).env("HOME", home.path()).status().unwrap().success());
        sh(stash_script("t-1"));
        let names = |d: &std::path::Path| {
            let mut v: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            v.sort();
            v
        };
        assert_eq!(names(&wt), ["agent.jsonl"]);
        assert_eq!(names(&asks), ["t-1", "t-1.jsonl"]);
        // Stashing again (a retried first question) leaves it there.
        sh(stash_script("t-1"));
        assert_eq!(names(&asks), ["t-1", "t-1.jsonl"]);
    }

    #[test]
    fn large_files_are_cut_around_the_line() {
        let src: String = (1..=100).map(|i| format!("line{i:03}\n")).collect();
        // 8 bytes a line: 5 lines fit in 40 bytes.
        assert_eq!(excerpt(&src, 50, 40), (48, "line048\nline049\nline050\nline051\nline052".into()));
        assert_eq!(excerpt(&src, 1, 24), (1, "line001\nline002\nline003".into()));
        assert_eq!(excerpt(&src, 100, 24), (98, "line098\nline099\nline100".into()));
        assert_eq!(excerpt("short", 1, 40), (1, "short".into()));
    }
}
