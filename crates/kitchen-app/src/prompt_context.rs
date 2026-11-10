//! Context the kitchen adds to a prompt William sends a tab's agent. Claude
//! Code's UserPromptSubmit hook runs `kitchenctl __prompt-context`, which
//! calls `prompt.context`; Claude Code adds the returned text to the prompt.
//!
//! Today the text names the tabs the prompt mentions ("tab 2", "agi tab 3"),
//! with how to message their agents.

use std::collections::HashSet;
use std::sync::LazyLock;

use kitchen_core::host::Host;
use kitchen_proto::Caller;
use regex::Regex;

use crate::engine::{Engine, Tab, Workspace};

/// Most tab mentions one prompt resolves (which also bounds the note's size).
const MAX_REFS: usize = 4;

/// A tab mention in a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TabRef {
    /// `None`: the caller's workspace.
    workspace: Option<WsRef>,
    /// 1-based position in the workspace (⌘N).
    index: usize,
}

/// How a mention names its workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WsRef {
    /// `agi tab 2`: a workspace name, lowercased.
    Name(String),
    /// `workspace 2 tab 3`: 1-based sidebar position.
    Index(usize),
}

/// `[workspace N | <word>] tab[s] [#]N` followed by `, M`, `and M`, `& M`
/// or `, and M`, case-insensitive.
static TAB_MENTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:workspace\s+(\d+)\s+|([\w.-]+)\s+)?tabs?\s*#?(\d+)((?:(?:\s*,\s*(?:(?:and|&)\s*)?|\s+and\s+|\s*&\s*)#?\d+)*)\b")
        .unwrap()
});

static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());

/// The tabs `prompt` mentions, in order, without repeats, at most
/// [`MAX_REFS`]. The word before `tab` names a workspace only when it is one
/// of `workspace_names` (compared lowercased); otherwise ("the tab 2") the
/// mention is of the caller's workspace.
fn tab_refs(prompt: &str, workspace_names: &[String]) -> Vec<TabRef> {
    let mut refs: Vec<TabRef> = Vec::new();
    for c in TAB_MENTION.captures_iter(prompt) {
        let workspace = match (c.get(1), c.get(2)) {
            (Some(n), _) => n.as_str().parse().ok().map(WsRef::Index),
            (None, Some(word)) => {
                let word = word.as_str().to_lowercase();
                workspace_names.iter().any(|n| n.to_lowercase() == word).then_some(WsRef::Name(word))
            }
            (None, None) => None,
        };
        let rest = c.get(4).map_or("", |m| m.as_str());
        let indexes = std::iter::once(&c[3]).chain(NUMBER.find_iter(rest).map(|m| m.as_str()));
        for index in indexes.filter_map(|n| n.parse::<usize>().ok()).filter(|&n| n >= 1) {
            let r = TabRef { workspace: workspace.clone(), index };
            if !refs.contains(&r) {
                refs.push(r);
            }
        }
    }
    refs.truncate(MAX_REFS);
    refs
}

/// The note for `prompt` sent from `caller`'s tab. It is empty when the
/// caller is not a kitchen tab or the prompt mentions no tab.
pub fn build(engine: &Engine, caller: &Caller, prompt: &str) -> String {
    let Some((ws, me)) = caller.tab_id.as_deref().and_then(|t| engine.find_tab(t)) else { return String::new() };
    let workspaces = engine.workspaces();
    let names: Vec<String> = workspaces.iter().map(|w| w.row.name.clone()).collect();
    let refs = tab_refs(prompt, &names);
    if refs.is_empty() {
        return String::new();
    }
    let my_index = ws.tabs.iter().position(|t| t.row.id == me.row.id).map_or(0, |i| i + 1);
    let mut lines = vec![format!(
        "You are tab {my_index} in workspace {} (on {}). When you message another tab, say which tab you are.",
        ws.row.name,
        machine(&ws.host())
    )];
    let mut seen = HashSet::new();
    for r in &refs {
        let target = match &r.workspace {
            None => Some(&ws),
            Some(WsRef::Name(n)) => workspaces.iter().find(|w| w.row.name.to_lowercase() == *n),
            Some(WsRef::Index(i)) => i.checked_sub(1).and_then(|i| workspaces.get(i)),
        };
        let Some(target) = target else {
            if let Some(WsRef::Index(i)) = &r.workspace {
                lines.push(format!("No workspace {i} (there are {}).", workspaces.len()));
            }
            continue;
        };
        let name = &target.row.name;
        match target.tabs.get(r.index - 1) {
            None if target.tabs.is_empty() => lines.push(format!("No tab {} in workspace {name} (it has no tabs).", r.index)),
            None => lines.push(format!("No tab {} in workspace {name} (it has tabs 1-{}).", r.index, target.tabs.len())),
            Some(t) if !seen.insert(t.row.id.clone()) => {}
            Some(t) if t.row.id == me.row.id => lines.push(format!("Tab {} of workspace {name} is you.", r.index)),
            Some(t) => lines.push(tab_line(target, r.index, t, &ws.host())),
        }
    }
    format!(
        "<kitchen-context>\nAdded by the kitchen for the tabs this prompt mentions.\n{}\n</kitchen-context>",
        lines.join("\n")
    )
}

/// One mentioned tab: who it is, and how to message its agent from a tab on
/// `caller_host`.
fn tab_line(ws: &Workspace, index: usize, tab: &Tab, caller_host: &Host) -> String {
    let row = &tab.row;
    let about = format!(
        "Tab {index} of workspace {}: \"{}\", branch {}, worktree {}, tmux session {}.",
        ws.row.name,
        row.title,
        row.branch.as_deref().unwrap_or("none"),
        row.worktree,
        row.tmux_session
    );
    let send = format!("`kitchenctl tab send -t {} --enter \"<text>\"`", row.id);
    let how = if !row.agent {
        format!("No agent runs in it; {send} types into its shell.")
    } else if ws.host() == *caller_host {
        format!(
            "Message its agent with SendMessage to the ListAgents peer whose tmux session is {}; if no peer has it, use {send}.",
            row.tmux_session
        )
    } else {
        format!("It runs on {}, not this machine, so ListAgents can't see it; message it with {send}.", machine(&ws.host()))
    };
    format!("{about}\n  {how}")
}

/// `this mac`, or `devbox <alias>`.
fn machine(host: &Host) -> String {
    match host {
        Host::Local => "this mac".into(),
        Host::Ssh(a) => format!("devbox {a}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kitchen_core::db::{Db, TabRow, WorkspaceRow};

    use super::*;

    /// An engine with workspaces `(id = name, host, tab ids)`, in sidebar order.
    fn engine_of(workspaces: &[(&str, &str, &[&str])]) -> Arc<Engine> {
        let db = Db::open_in_memory().unwrap();
        for (i, (ws, host, tabs)) in workspaces.iter().enumerate() {
            let row = WorkspaceRow { id: ws.to_string(), name: ws.to_string(), host: host.to_string(), repo_path: format!("/r/{ws}"), github_slug: None, default_branch: None, position: i as i64, pinned: false };
            db.insert_workspace(&row).unwrap();
            for (j, tab) in tabs.iter().enumerate() {
                let row = TabRow { id: tab.to_string(), workspace_id: ws.to_string(), title: format!("title {tab}"), worktree: format!("/r/{ws}-{tab}"), branch: Some(format!("william/{tab}")), tmux_session: format!("kt_{ws}_{tab}"), position: j as i64, agent: true, agent_session: None };
                db.insert_tab(&row).unwrap();
            }
        }
        Engine::new(egui::Context::default(), db).unwrap()
    }

    fn caller(tab: &str) -> Caller {
        Caller { tab_id: Some(tab.into()), ..Default::default() }
    }

    fn here(index: usize) -> TabRef {
        TabRef { workspace: None, index }
    }

    #[test]
    fn finds_tab_mentions() {
        let names = ["agi".to_string()];
        let refs = |p: &str| tab_refs(p, &names);
        assert_eq!(refs("tell tab 2 to rebase"), [here(2)]);
        assert_eq!(refs("Tab #3"), [here(3)]);
        for p in ["tabs 2 and 4", "tabs 2, 4", "tabs 2 & 4"] {
            assert_eq!(refs(p), [here(2), here(4)], "{p}");
        }
        assert_eq!(refs("tabs 2, 3, and 4"), [here(2), here(3), here(4)]);
        assert_eq!(refs("tab2"), [here(2)]);
        let agi = |index| TabRef { workspace: Some(WsRef::Name("agi".into())), index };
        assert_eq!(refs("agi tab 3"), [agi(3)]);
        assert_eq!(refs("AGI tab 3"), [agi(3)]);
        assert_eq!(refs("the tab 2"), [here(2)]);
        assert_eq!(refs("workspace 2 tab 1"), [TabRef { workspace: Some(WsRef::Index(2)), index: 1 }]);
        for p in ["stab 2", "tablet 2", "the tab key", "no tabs here", "#2", "t2"] {
            assert_eq!(refs(p), [], "{p}");
        }
    }

    #[test]
    fn caps_and_dedupes_mentions() {
        assert_eq!(tab_refs("tab 2, then tab 2 again", &[]), [here(2)]);
        assert_eq!(tab_refs("tabs 1, 2, 3, 4, 5, 6", &[]), [here(1), here(2), here(3), here(4)]);
    }

    #[test]
    fn names_tab_and_send() {
        let e = engine_of(&[("agi", "local", &["a", "b"])]);
        let text = build(&e, &caller("a"), "ask tab 2 to rebase");
        for want in ["<kitchen-context>", "You are tab 1 in workspace agi (on this mac)", "Tab 2 of workspace agi: \"title b\"", "branch william/b", "kt_agi_b", "SendMessage", "kitchenctl tab send -t b"] {
            assert!(text.contains(want), "{want:?} missing from\n{text}");
        }
        assert!(text.ends_with("</kitchen-context>"), "{text}");
    }

    #[test]
    fn missing_tabs_and_self() {
        let e = engine_of(&[("agi", "local", &["a", "b"])]);
        let text = |p: &str| build(&e, &caller("a"), p);
        assert!(text("tab 9").contains("No tab 9 in workspace agi (it has tabs 1-2)."), "{}", text("tab 9"));
        assert!(text("tab 1").contains("Tab 1 of workspace agi is you."), "{}", text("tab 1"));
        assert!(text("workspace 7 tab 1").contains("No workspace 7"), "{}", text("workspace 7 tab 1"));
        let both = text("tab 2 and agi tab 2");
        assert_eq!(both.matches("Tab 2 of workspace agi").count(), 1, "{both}");
    }

    #[test]
    fn other_host_uses_kitchenctl() {
        let e = engine_of(&[("agi", "local", &["a"]), ("dot", "dev1", &["d"]), ("loop", "localhost", &["l"])]);
        let text = build(&e, &caller("a"), "dot tab 1");
        assert!(text.contains("It runs on devbox dev1, not this machine") && text.contains("kitchenctl tab send -t d"), "{text}");
        assert!(!text.contains("SendMessage"), "{text}");
        let text = build(&e, &caller("a"), "workspace 3 tab 1");
        assert!(text.contains("SendMessage to the ListAgents peer whose tmux session is kt_loop_l"), "{text}");
    }

    #[test]
    fn empty_without_mentions_or_outside_a_tab() {
        let e = engine_of(&[("agi", "local", &["a", "b"])]);
        assert_eq!(build(&e, &caller("a"), "rebase onto main"), "");
        assert_eq!(build(&e, &Caller::default(), "tab 2"), "");
    }
}
