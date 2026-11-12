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

/// A tab's own kitchen files, relative to its worktree. Plans live here
/// rather than at the worktree's root so tabs that share a worktree (read
/// tabs share the main checkout) or reuse one keep separate plans.
pub fn tab_dir(tab_id: &str) -> String {
    format!("{KITCHEN_DIR}/tabs/{tab_id}")
}

pub fn plan_file_for(worktree: &str, tab_id: &str) -> String {
    format!("{}/{}/plan.md", worktree.trim_end_matches('/'), tab_dir(tab_id))
}

/// Where the tab's agent lists the PRs it opened (`$KITCHEN_PRS_FILE`).
pub fn prs_file_for(worktree: &str, tab_id: &str) -> String {
    format!("{}/{}/prs", worktree.trim_end_matches('/'), tab_dir(tab_id))
}

/// Claude Code's plan mode writes the tab's plans here when kitchen launches
/// it with this as `plansDirectory` (relative to the worktree, which it must
/// be inside).
pub fn plans_dir(tab_id: &str) -> String {
    format!("{}/plans", tab_dir(tab_id))
}

/// The plan document a tab shows, and when it last changed.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanDoc {
    pub path: String,
    /// Modification time, in seconds (as `util::now`).
    pub modified: i64,
}

/// The tab's `plan.md` or its newest plan-mode file, whichever changed last.
pub fn current_plan(host: &Host, worktree: &str, tab_id: &str) -> Option<PlanDoc> {
    // GNU stat first (devboxes), then BSD (this mac).
    let script = format!(
        "p=$(ls -t {f} {d}/*.md 2>/dev/null | head -1); [ -n \"$p\" ] || exit 0\n\
         stat -c %Y \"$p\" 2>/dev/null || stat -f %m \"$p\"; printf '%s\\n' \"$p\"",
        f = crate::util::shell_quote(&plan_file_for(worktree, tab_id)),
        d = crate::util::shell_quote(&format!("{}/{}", worktree.trim_end_matches('/'), plans_dir(tab_id))),
    );
    let out = host.sh(&script, None).ok()?;
    let mut lines = out.stdout.lines();
    let modified = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim().to_string();
    Some(PlanDoc { path, modified })
}

/// Whether `c` shows on `plan`: it was made on that file, and isn't a
/// resolved comment the plan has since moved past (rewritten after it was
/// resolved, e.g. a new plan in the same plan-mode file).
pub fn shows_on(c: &PlanComment, plan: &PlanDoc) -> bool {
    c.plan_path.as_deref() == Some(plan.path.as_str())
        && !(c.state == "resolved" && c.resolved_at.is_some_and(|t| plan.modified > t))
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
            plan_path: Some("/w/p.md".into()),
            state: "open".into(),
            created_at: 0,
            resolved_at: None,
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
        let msg = feedback_message("/w/.kitchen/tabs/t/plan.md", &[&a]);
        assert!(msg.contains("[comment 7] line 3: `## F1`"));
        assert!(msg.contains("kitchenctl plan resolve"));
    }

    #[test]
    fn comments_show_on_their_plan_until_it_moves_past_them() {
        let plan = PlanDoc { path: "/w/p.md".into(), modified: 100 };
        let open = pc(1, 1, "x");
        assert!(shows_on(&open, &plan));
        assert!(!shows_on(&PlanComment { plan_path: Some("/w/old.md".into()), ..open.clone() }, &plan), "another plan's");
        assert!(!shows_on(&PlanComment { plan_path: None, ..open.clone() }, &plan), "from before plans had paths");
        let resolved = |at| PlanComment { state: "resolved".into(), resolved_at: Some(at), ..open.clone() };
        assert!(shows_on(&resolved(100), &plan), "resolved after the last revision");
        assert!(!shows_on(&resolved(99), &plan), "the plan changed since");
    }

    #[test]
    fn discover() {
        let tmp = tempfile::tempdir().unwrap();
        let w = tmp.path().display().to_string();
        std::process::Command::new("git").args(["init", "-q", &w]).status().unwrap();
        assert_eq!(current_plan(&Host::Local, &w, "a"), None);
        Host::Local.write_file(&plan_file_for(&w, "a"), "# p").unwrap();
        let doc = current_plan(&Host::Local, &w, "a").expect("plan.md");
        assert_eq!(doc.path, plan_file_for(&w, "a"));
        assert!((doc.modified - crate::util::now()).abs() < 60, "{doc:?}");
        assert_eq!(current_plan(&Host::Local, &w, "b"), None, "another tab's plan");
        let mode = format!("{w}/{}/jolly-otter.md", plans_dir("a"));
        Host::Local.write_file(&mode, "# m").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(&mode).unwrap().set_modified(later).unwrap();
        assert_eq!(current_plan(&Host::Local, &w, "a").unwrap().path, mode, "the newer file");
    }
}
