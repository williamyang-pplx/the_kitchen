//! Pure review logic, kept free of I/O so it is unit-testable:
//! draft validation, placing a comment on the PR diff, poll decisions, the
//! resolve policy, and the prompt/command that runs an automatic review.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::gh::DiffLines;
use crate::util::shell_quote;

/// A comment an agent (or William) wants to leave, before placement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CommentInput {
    pub path: String,
    pub line: Option<i64>,
    pub start_line: Option<i64>,
    pub side: Option<String>,
    pub body: String,
}

/// Where a comment lands on GitHub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Placement {
    /// Line (or multi-line range) comment inside the diff.
    Line { line: i64, start_line: Option<i64>, side: String },
    /// File-level comment; `line_ref` is the line it talks about, if any.
    File { line_ref: Option<(i64, Option<i64>)> },
}

impl Placement {
    pub fn is_file_level(&self) -> bool {
        matches!(self, Placement::File { .. })
    }
}

/// Normalize and check a comment. Returns the cleaned input.
pub fn validate(input: &CommentInput) -> Result<CommentInput> {
    let path = input.path.trim().trim_start_matches("./").trim_start_matches("b/").to_string();
    if path.is_empty() {
        bail!("path is required");
    }
    if path.starts_with('/') || path.split('/').any(|p| p == "..") {
        bail!("path must be relative to the repository root: {path:?}");
    }
    let body = input.body.trim().to_string();
    if body.is_empty() {
        bail!("comment body is empty");
    }
    let side = match input.side.as_deref().map(|s| s.trim().to_ascii_uppercase()) {
        None => "RIGHT".to_string(),
        Some(s) if s.is_empty() => "RIGHT".to_string(),
        Some(s) if s == "LEFT" || s == "RIGHT" => s,
        Some(s) => bail!("side must be LEFT or RIGHT, got {s:?}"),
    };
    if let Some(l) = input.line {
        if l < 1 {
            bail!("line must be >= 1");
        }
    }
    let start_line = match (input.start_line, input.line) {
        (Some(_), None) => bail!("start_line needs line"),
        (Some(s), Some(l)) if s > l => bail!("start_line ({s}) is after line ({l})"),
        (Some(s), Some(l)) if s == l => None,
        (Some(s), _) if s < 1 => bail!("start_line must be >= 1"),
        (s, _) => s,
    };
    Ok(CommentInput { path, line: input.line, start_line, side: Some(side), body })
}

/// Decide whether a (validated) comment can be a line comment on this diff.
/// A range must lie entirely in the diff; otherwise, or when the line is
/// outside every hunk, it becomes a file-level comment that names the line.
pub fn place(diff: &DiffLines, c: &CommentInput) -> Placement {
    let side = c.side.clone().unwrap_or_else(|| "RIGHT".into());
    let Some(line) = c.line else {
        return Placement::File { line_ref: None };
    };
    let in_diff = |l: i64| diff.contains(&c.path, &side, l);
    let all = match c.start_line {
        Some(s) => (s..=line).all(in_diff),
        None => in_diff(line),
    };
    if all {
        Placement::Line { line, start_line: c.start_line, side }
    } else {
        Placement::File { line_ref: Some((line, c.start_line)) }
    }
}

/// Body for a file-level comment that refers to specific lines.
pub fn file_level_body(body: &str, line_ref: Option<(i64, Option<i64>)>) -> String {
    match line_ref {
        None => body.to_string(),
        Some((l, Some(s))) => format!("L{s}-L{l}: {body}"),
        Some((l, None)) => format!("L{l}: {body}"),
    }
}

/// Should a review-requested PR be auto-reviewed? Yes when its author, or
/// whoever requested William's review, is on the watch list.
pub fn should_auto_review(author: &str, requesters: &[String], watch: &[String]) -> bool {
    let norm = |s: &str| s.trim().trim_start_matches('@').to_ascii_lowercase();
    let watched: Vec<String> = watch.iter().map(|w| norm(w)).collect();
    watched.contains(&norm(author)) || requesters.iter().any(|r| watched.contains(&norm(r)))
}

/// Agents may only resolve threads William started (his own comments).
pub fn can_resolve(thread_first_author: Option<&str>, viewer: &str) -> bool {
    match thread_first_author {
        Some(a) => a.eq_ignore_ascii_case(viewer),
        None => false,
    }
}

/// `https://github.com/o/r/pull/12[/files]` -> (`o/r`, 12).
pub fn parse_pr_url(url: &str) -> Option<(String, u64)> {
    let rest = url.trim().split("github.com/").nth(1)?;
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next()? != "pull" {
        return None;
    }
    let n = parts.next()?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some((format!("{owner}/{name}"), n))
}

/// Instructions for the automatic reviewer.
pub fn review_prompt(pr_url: &str, run_id: i64) -> String {
    format!(
        "/deep-review-other-pr {pr_url}\n\n\
         You are running unattended inside the kitchen (review run {run_id}). The current directory is a \
         read-only checkout of the PR head.\n\
         RULES:\n\
         - NEVER post anything to GitHub: no `gh pr comment`, `gh pr review`, `gh api` writes, no pushes. \
           William approves every comment himself before it becomes public.\n\
         - Record EVERY finding you would have reported as a draft, one command per finding:\n\
           kitchenctl review draft add --run {run_id} --path <repo-relative path> --line <line in the new file> \
         [--start-line <first line of a range>] [--side RIGHT|LEFT] --severity <blocker|major|minor|nit> --body '<comment text as you would post it>'\n\
           Use --side LEFT only for lines that exist only in the old file. Omit --line for file-level comments.\n\
         - Write each body as the final review comment (concise, specific, with the suggested fix).\n\
         - When finished, run: kitchenctl review done --run {run_id}\n"
    )
}

/// Tools the unattended reviewer may never use: anything that writes to
/// GitHub or the repo.
pub const REVIEW_DISALLOWED_TOOLS: &[&str] = &[
    "Edit",
    "Write",
    "NotebookEdit",
    "Bash(gh pr comment:*)",
    "Bash(gh pr review:*)",
    "Bash(gh pr edit:*)",
    "Bash(gh pr merge:*)",
    "Bash(gh pr close:*)",
    "Bash(gh issue comment:*)",
    // Reads go through `gh pr view/diff`; the raw API can write, so it is off.
    "Bash(gh api:*)",
    "Bash(git push:*)",
    "Bash(curl:*)",
];

/// Shell command typed into the review tab. The trailing `review done` makes
/// sure the run is closed even if the agent forgets.
pub fn review_command(prompt_file: &str, run_id: i64) -> String {
    let disallowed = REVIEW_DISALLOWED_TOOLS.iter().map(|t| shell_quote(t)).collect::<Vec<_>>().join(" ");
    format!(
        "claude -p \"$(cat {pf})\" --allowedTools 'Bash(kitchenctl review:*)' --disallowedTools {disallowed}; \
         kitchenctl review done --run {run_id} --exit $?",
        pf = shell_quote(prompt_file)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff() -> DiffLines {
        DiffLines::parse("diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -10,3 +10,4 @@\n a\n-b\n+c\n+d\n e\n")
    }

    fn input(line: Option<i64>, start: Option<i64>, side: Option<&str>) -> CommentInput {
        CommentInput { path: "x.rs".into(), line, start_line: start, side: side.map(String::from), body: "fix".into() }
    }

    #[test]
    fn validation() {
        assert!(validate(&CommentInput { path: "".into(), body: "b".into(), ..Default::default() }).is_err());
        assert!(validate(&CommentInput { path: "../x".into(), body: "b".into(), ..Default::default() }).is_err());
        assert!(validate(&CommentInput { path: "/abs".into(), body: "b".into(), ..Default::default() }).is_err());
        assert!(validate(&input(Some(3), None, Some("up"))).is_err());
        assert!(validate(&input(Some(3), Some(5), None)).is_err());
        assert!(validate(&input(None, Some(5), None)).is_err());
        assert!(validate(&CommentInput { body: "  ".into(), ..input(Some(1), None, None) }).is_err());
        let v = validate(&CommentInput { path: "./a/b.rs".into(), ..input(Some(3), Some(3), Some("left")) }).unwrap();
        assert_eq!(v.path, "a/b.rs");
        assert_eq!(v.side.as_deref(), Some("LEFT"));
        assert_eq!(v.start_line, None, "single-line range collapses");
    }

    #[test]
    fn placement() {
        let d = diff();
        assert_eq!(place(&d, &validate(&input(Some(11), None, None)).unwrap()), Placement::Line { line: 11, start_line: None, side: "RIGHT".into() });
        assert_eq!(
            place(&d, &validate(&input(Some(12), Some(10), None)).unwrap()),
            Placement::Line { line: 12, start_line: Some(10), side: "RIGHT".into() }
        );
        assert_eq!(place(&d, &validate(&input(Some(11), None, Some("LEFT"))).unwrap()), Placement::Line { line: 11, start_line: None, side: "LEFT".into() });
        // Outside any hunk -> file-level with a line reference.
        let p = place(&d, &validate(&input(Some(50), None, None)).unwrap());
        assert_eq!(p, Placement::File { line_ref: Some((50, None)) });
        // Range partially outside -> file-level.
        assert!(place(&d, &validate(&input(Some(12), Some(2), None)).unwrap()).is_file_level());
        // Other file -> file-level.
        let other = CommentInput { path: "y.rs".into(), ..input(Some(11), None, None) };
        assert!(place(&d, &validate(&other).unwrap()).is_file_level());
        assert_eq!(file_level_body("b", Some((5, Some(3)))), "L3-L5: b");
        assert_eq!(file_level_body("b", Some((5, None))), "L5: b");
        assert_eq!(file_level_body("b", None), "b");
    }

    #[test]
    fn auto_review_policy() {
        let watch = vec!["Alice".to_string(), "@bob".to_string()];
        assert!(should_auto_review("alice", &[], &watch));
        assert!(should_auto_review("carol", &["bob".into()], &watch));
        assert!(!should_auto_review("carol", &["dave".into()], &watch));
        assert!(!should_auto_review("alice", &[], &[]));
    }

    #[test]
    fn resolve_policy() {
        assert!(can_resolve(Some("williamyang-pplx"), "WilliamYang-pplx"));
        assert!(!can_resolve(Some("alice"), "williamyang-pplx"));
        assert!(!can_resolve(None, "williamyang-pplx"));
    }

    #[test]
    fn pr_urls() {
        assert_eq!(parse_pr_url("https://github.com/ppl-ai/air/pull/9542/files"), Some(("ppl-ai/air".into(), 9542)));
        assert_eq!(parse_pr_url("https://github.com/ppl-ai/air/issues/1"), None);
        assert_eq!(parse_pr_url("nope"), None);
    }

    #[test]
    fn command_forbids_posting() {
        let c = review_command("/h/.kitchen/runs/3.md", 3);
        assert!(c.contains("'Bash(gh pr comment:*)'"));
        assert!(c.contains("kitchenctl review done --run 3"));
        assert!(review_prompt("u", 3).contains("--run 3"));
    }
}
