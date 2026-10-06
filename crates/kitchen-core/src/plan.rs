//! The planning phase of a tab.
//!
//! Before implementing, the tab's agent runs the `low-level-plan` skill and
//! writes the plan to a markdown file in the worktree. William comments on
//! it inline in the plan reviewer; the comments go back to the agent as one
//! message, the agent revises the file, and the loop repeats.

use crate::db::PlanComment;
use crate::host::Host;

/// Directory (inside each worktree) that holds kitchen-owned files. Added to
/// the repo's `info/exclude` so it never shows up in `git status`.
pub const KITCHEN_DIR: &str = ".kitchen";

pub fn plan_file_for(worktree: &str) -> String {
    format!("{}/{KITCHEN_DIR}/plan.md", worktree.trim_end_matches('/'))
}

/// Claude Code's plan mode writes its plans here when kitchen launches it
/// with `plansDirectory` set (must be inside the project root).
pub fn plans_dir_for(worktree: &str) -> String {
    format!("{}/{KITCHEN_DIR}/plans", worktree.trim_end_matches('/'))
}

/// The plan document to show: `.kitchen/plan.md` or the newest plan-mode
/// file under `.kitchen/plans/`, whichever changed last.
pub fn current_plan_path(host: &Host, worktree: &str) -> Option<String> {
    let script = format!(
        "ls -t {f} {d}/*.md 2>/dev/null | head -1",
        f = crate::util::shell_quote(&plan_file_for(worktree)),
        d = crate::util::shell_quote(&plans_dir_for(worktree)),
    );
    let out = host.sh(&script, None).ok()?;
    let p = out.stdout.trim();
    (!p.is_empty()).then(|| p.to_string())
}

/// Instructions typed into a fresh tab's agent to start planning.
pub fn planning_prompt(task: &str, plan_file: &str) -> String {
    format!(
        "/low-level-plan {task}\n\n\
         Kitchen workflow: this tab is a dedicated worktree for one PR. First produce the \
         low-level plan (no implementation yet). Write the complete plan as markdown to \
         {plan_file} (create the directory if needed) and keep that file updated after every \
         revision — William reviews it in the kitchen plan viewer and leaves inline comments. \
         When you finish a revision run `kitchenctl plan ready`. When comments arrive, read them \
         with `kitchenctl plan comments`, revise {plan_file}, then mark each handled comment with \
         `kitchenctl plan resolve <id>`. Do not implement until William approves the plan."
    )
}

/// Move each comment to the line that still carries its quoted text,
/// preferring the occurrence nearest the old position. Returns
/// `(comment id, new 1-based line)` for comments that moved.
pub fn reanchor(comments: &[PlanComment], text: &str) -> Vec<(i64, i64)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut moved = Vec::new();
    for c in comments {
        let quote = c.quote.trim();
        if quote.is_empty() {
            continue;
        }
        let cur = (c.line - 1).max(0) as usize;
        if lines.get(cur).map(|l| l.trim() == quote).unwrap_or(false) {
            continue;
        }
        let best = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim() == quote)
            .min_by_key(|(i, _)| (*i as i64 - cur as i64).abs())
            .map(|(i, _)| i as i64 + 1);
        if let Some(line) = best {
            moved.push((c.id, line));
        }
    }
    moved
}

/// One message carrying every open comment, sent to the tab's agent.
pub fn feedback_message(plan_file: &str, comments: &[&PlanComment]) -> String {
    let mut s = format!(
        "William left {} comment(s) on the plan in {plan_file}. Revise the plan to address each one, \
         keep item IDs stable, rewrite the file, then run `kitchenctl plan resolve <id>` for each \
         comment you addressed and `kitchenctl plan ready` when done.\n",
        comments.len()
    );
    for c in comments {
        let range = match c.end_line {
            Some(e) if e > c.line => format!("lines {}-{}", c.line, e),
            _ => format!("line {}", c.line),
        };
        s.push_str(&format!("\n[comment {}] {range}: `{}`\n{}\n", c.id, c.quote.trim(), c.body.trim()));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pc(id: i64, line: i64, quote: &str) -> PlanComment {
        PlanComment {
            id,
            tab_id: "t".into(),
            line,
            end_line: None,
            quote: quote.into(),
            body: "b".into(),
            state: "open".into(),
            created_at: 0,
        }
    }

    #[test]
    fn reanchor_follows_quote() {
        let text = "# Plan\nnew line\n## F1\nbody\n## F2\n";
        let comments = vec![pc(1, 2, "## F1"), pc(2, 1, "# Plan"), pc(3, 3, "gone")];
        assert_eq!(reanchor(&comments, text), vec![(1, 3)]);
    }

    #[test]
    fn feedback_lists_every_comment() {
        let a = pc(7, 3, "## F1");
        let msg = feedback_message("/w/.kitchen/plan.md", &[&a]);
        assert!(msg.contains("[comment 7] line 3: `## F1`"));
        assert!(msg.contains("kitchenctl plan resolve"));
    }

    #[test]
    fn discover() {
        let tmp = tempfile::tempdir().unwrap();
        let w = tmp.path().display().to_string();
        std::process::Command::new("git").args(["init", "-q", &w]).status().unwrap();
        assert_eq!(current_plan_path(&Host::Local, &w), None);
        Host::Local.write_file(&plan_file_for(&w), "# p").unwrap();
        assert_eq!(current_plan_path(&Host::Local, &w), Some(plan_file_for(&w)));
    }
}
