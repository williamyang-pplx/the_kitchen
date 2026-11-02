//! ⌘⇧A in the viewer and the plan reviewer: a question about some lines of
//! a file or of the plan, answered by the model (`claude -p` on the mac)
//! next to them. Nothing is stored.

use anyhow::Result;

use crate::host::Host;

/// Most file context sent with a question, in bytes.
const MAX_CONTEXT: usize = 60_000;

/// Most of the planning agent's transcript sent with a plan question, in bytes.
pub const MAX_TRACE: usize = 100_000;

/// What the question is about.
pub struct Question<'a> {
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
        "A developer reading `{path}` in a code viewer asks a question about {lines}{side}:\n\
         ```\n{quote}\n```\n\n\
         Question: {text}\n\n\
         Answer in plain text (no Markdown headings or bold), briefly: a few sentences, \
         or a short code snippet if that answers it best. You may read other files of the \
         repository if the answer needs them; do not change anything.\n\n\
         File `{path}`, lines {from}–{to}:\n```\n{ctx}\n```\n",
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
    /// The planning agent's session, condensed (see `trace::condense`).
    pub trace: Option<&'a str>,
    pub text: &'a str,
}

pub fn build_plan_prompt(q: &PlanQuestion) -> String {
    let (from, ctx) = excerpt(q.plan, q.first.max(1), MAX_CONTEXT);
    let to = from + ctx.lines().count().saturating_sub(1);
    let lines = if q.last > q.first { format!("lines {}–{}", q.first, q.last) } else { format!("line {}", q.first) };
    let trace = match q.trace {
        Some(t) => format!(
            "\n\nThe session in which the agent wrote this plan (William's prompts, the agent's \
             replies, its tool calls and their results, oldest first, long parts cut):\n\
             <planning-session>\n{t}\n</planning-session>\n"
        ),
        None => String::new(),
    };
    format!(
        "William is reviewing an implementation plan an agent wrote, `{path}`, and asks a \
         question about {lines}:\n\
         ```\n{quote}\n```\n\n\
         Question: {text}\n\n\
         Answer in plain text (no Markdown headings or bold), briefly: a few sentences, \
         or a short code snippet if that answers it best. Use the planning session below \
         to explain why the plan says what it says when it can. You may read files of the \
         repository if the answer needs them; do not change anything.\n\n\
         Plan `{path}`, lines {from}–{to}:\n```\n{ctx}\n```{trace}",
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

/// Ask the model. Blocking. `cwd`: where it may read the repository from
/// (the worktree on this mac; `None` for a devbox's, which it can't reach).
pub fn answer(model: &str, cwd: Option<&str>, q: &Question) -> Result<String> {
    run(model, cwd, &build_prompt(q))
}

/// Ask the model about the plan. Blocking. `cwd` as for [`answer`].
pub fn answer_plan(model: &str, cwd: Option<&str>, q: &PlanQuestion) -> Result<String> {
    run(model, cwd, &build_plan_prompt(q))
}

fn run(model: &str, cwd: Option<&str>, prompt: &str) -> Result<String> {
    // Not saved as a session: the worktree's newest session must stay the
    // tab agent's (`--continue`, `trace::latest_transcript`).
    let argv = [
        "claude",
        "-p",
        "--model",
        model,
        "--output-format",
        "text",
        "--no-session-persistence",
        "--disallowedTools",
        "Bash Edit Write NotebookEdit",
    ];
    let out = Host::Local.run_with_stdin(&argv, Some(cwd.unwrap_or("/tmp")), Some(prompt.as_bytes()))?;
    Ok(out.into_result("claude -p")?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_quotes_the_lines_and_the_file() {
        let src = "fn a() {}\nfn b() {}\n";
        let q = Question { path: "x.rs", first: 2, last: 2, base: false, quote: "fn b() {}", source: src, text: "what is b?" };
        let p = build_prompt(&q);
        assert!(p.contains("about line 2:\n```\nfn b() {}\n```"), "{p}");
        assert!(p.contains("Question: what is b?"));
        assert!(p.contains("File `x.rs`, lines 1–2:"));
        let q = Question { first: 1, last: 2, base: true, ..q };
        assert!(build_prompt(&q).contains("lines 1–2 (removed by this change"));
    }

    #[test]
    fn plan_prompt_carries_the_plan_and_the_trace() {
        let plan = "# Plan\n## F1 cache\n";
        let q = PlanQuestion { path: "/w/.kitchen/plan.md", first: 2, last: 2, quote: "## F1 cache", plan, trace: None, text: "why?" };
        let p = build_plan_prompt(&q);
        assert!(p.contains("about line 2:\n```\n## F1 cache\n```"), "{p}");
        assert!(p.contains("Plan `/w/.kitchen/plan.md`, lines 1–2:"));
        assert!(!p.contains("<planning-session>"));
        let p = build_plan_prompt(&PlanQuestion { trace: Some("[William] add a cache"), ..q });
        assert!(p.contains("<planning-session>\n[William] add a cache\n</planning-session>"), "{p}");
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
