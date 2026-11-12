//! ⌘⇧A in the viewer and the plan reviewer: a question about some lines of
//! a file or of the plan, answered next to them. It goes to a fork of the
//! tab agent's Claude Code session (`claude -p --resume <id> --fork-session`
//! on the tab's host), so the answer has the agent's context, model and
//! tools; without a session, to a fresh `claude -p`. Nothing is stored.

use anyhow::Result;

use crate::db::TabRow;
use crate::host::Host;
use crate::trace::{self, Session};

/// Most file context sent with a question, in bytes.
const MAX_CONTEXT: usize = 60_000;

/// Put before a question to a fork of the agent's session.
const SIDE_QUESTION: &str = "[A side question from William, asked in a fork of this session from \
    the kitchen's viewer. Only your reply reaches him, shown under the lines he asks about; \
    your session will not see it. Answer the question, then stop: do not go on with your task, \
    and do not change any files.]\n\n";

/// Tools a question may not use: it answers, the agent changes the files.
const READ_ONLY: &str = "Edit Write NotebookEdit";

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
        Asker { host, cwd, session, model: model.to_string() }
    }

    fn argv(&self) -> Vec<&str> {
        let mut argv = vec!["claude", "-p", "--output-format", "text", "--disallowedTools", READ_ONLY];
        // Not saved as a session: the worktree's sessions must stay the
        // agent's (`--continue`, `trace::agent_session`).
        argv.push("--no-session-persistence");
        match &self.session {
            Some(s) => {
                argv.extend(["--resume", &s.id, "--fork-session"]);
                if let Some(m) = &s.model {
                    argv.extend(["--model", m]);
                }
            }
            None => argv.extend(["--model", &self.model]),
        }
        argv
    }
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

/// Ask about lines of a file. Blocking.
pub fn answer(asker: &Asker, q: &Question) -> Result<String> {
    run(asker, &build_prompt(q))
}

/// Ask about lines of the plan. Blocking.
pub fn answer_plan(asker: &Asker, q: &PlanQuestion) -> Result<String> {
    run(asker, &build_plan_prompt(q))
}

fn run(asker: &Asker, prompt: &str) -> Result<String> {
    let prompt = if asker.session.is_some() { format!("{SIDE_QUESTION}{prompt}") } else { prompt.to_string() };
    let out = asker.host.run_with_stdin(&asker.argv(), Some(&asker.cwd), Some(prompt.as_bytes()))?;
    Ok(out.into_result("claude -p")?.trim().to_string())
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
    fn a_session_is_forked_with_its_model() {
        let session = Session { id: "0b7e-41".into(), model: Some("claude-opus-5-5".into()) };
        let asker = Asker { host: Host::Local, cwd: "/w".into(), session: Some(session), model: "sonnet".into() };
        let argv = asker.argv().join(" ");
        assert!(argv.ends_with("--no-session-persistence --resume 0b7e-41 --fork-session --model claude-opus-5-5"), "{argv}");
        assert!(argv.contains("--disallowedTools Edit Write NotebookEdit"), "{argv}");
        let fresh = Asker { session: None, ..asker };
        assert!(fresh.argv().join(" ").ends_with("--no-session-persistence --model sonnet"));
        // A tab without a transcript gets the fresh model in the checkout viewed.
        let worktree = format!("/kitchen-ask-test-{}", std::process::id());
        let tab = TabRow { id: "t".into(), workspace_id: "w".into(), title: "t".into(), worktree, branch: None, tmux_session: "t".into(), position: 0, agent: true, agent_session: Some("0b7e-41".into()) };
        let asker = Asker::for_tab(Host::Local, Some(&tab), "/pr", "sonnet");
        assert_eq!((asker.cwd.as_str(), asker.session), ("/pr", None));
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
