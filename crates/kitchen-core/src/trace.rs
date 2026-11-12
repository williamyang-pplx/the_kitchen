//! A tab agent's Claude Code session, found from its transcripts
//! (`~/.claude/projects/<worktree>/<session>.jsonl` on the tab's host), so
//! a ⌘⇧A question can go to a fork of it.

use serde_json::Value;

use crate::host::Host;
use crate::util::shell_quote;

/// Most transcript bytes searched for the agent's latest reply.
const MODEL_TAIL: usize = 4_000_000;

/// Claude Code's folder name for a project: the path with every character
/// that is not a letter or digit replaced by `-`.
pub fn project_dir_name(worktree: &str) -> String {
    worktree.trim_end_matches('/').chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// An agent's Claude Code session.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    /// The model of its latest reply (`None`: no reply found).
    pub model: Option<String>,
}

/// The session of the agent working in `worktree`: `reported` (the id its
/// Stop hook last reported) if its transcript is there, else the newest
/// one. `None` if there is none. Blocking: one host round trip.
pub fn agent_session(host: &Host, worktree: &str, reported: Option<&str>) -> Option<Session> {
    let out = host.sh_bytes_ok(&session_script(worktree, reported), None).ok()?;
    parse_session(&String::from_utf8_lossy(&out))
}

/// Prints the session's id, then the reply lines of its tail (the model is
/// in the last one). Fails if there is no session.
fn session_script(worktree: &str, reported: Option<&str>) -> String {
    format!(
        "d=\"$HOME/.claude/projects/{dir}\"; f=\"$d\"/{id}.jsonl; \
         [ -f \"$f\" ] || f=$(ls -t \"$d\"/*.jsonl 2>/dev/null | head -1); \
         [ -f \"$f\" ] && {{ basename \"$f\" .jsonl; tail -c {MODEL_TAIL} \"$f\" | grep -F '\"type\":\"assistant\"' | tail -n 20; }}",
        dir = project_dir_name(worktree),
        id = shell_quote(reported.unwrap_or_default()),
    )
}

/// The output of [`agent_session`]'s script: the session id, then
/// transcript lines.
fn parse_session(out: &str) -> Option<Session> {
    let mut lines = out.lines();
    let id = lines.next()?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let model = lines
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["type"] == "assistant" && e["isSidechain"] != true)
        .filter_map(|e| e["message"]["model"].as_str().map(String::from))
        // `<synthetic>`: Claude Code's own messages (API errors).
        .filter(|m| !m.starts_with('<'))
        .last();
    Some(Session { id, model })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_dir_matches_claude_code() {
        assert_eq!(project_dir_name("/Users/w/Documents/the_kitchen"), "-Users-w-Documents-the-kitchen");
        assert_eq!(project_dir_name("/home/w/.wt/a.b/"), "-home-w--wt-a-b");
    }

    #[test]
    fn the_session_has_the_latest_real_model() {
        let reply = |model: &str| json!({"type": "assistant", "message": {"model": model, "content": []}}).to_string();
        let sub = json!({"type": "assistant", "isSidechain": true, "message": {"model": "claude-haiku-5-5"}}).to_string();
        let out = format!("0b7e-41\n{{\"partial\n{}\n{}\n{}\n{sub}\n", reply("claude-sonnet-5-5"), reply("claude-opus-5-5"), reply("<synthetic>"));
        assert_eq!(parse_session(&out), Some(Session { id: "0b7e-41".into(), model: Some("claude-opus-5-5".into()) }));
        assert_eq!(parse_session("0b7e-41\n"), Some(Session { id: "0b7e-41".into(), model: None }));
        assert_eq!(parse_session(""), None);
    }

    #[test]
    fn finds_the_reported_session_then_the_newest() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/projects").join(project_dir_name("/w/repo"));
        std::fs::create_dir_all(&dir).unwrap();
        let reply = json!({"type": "assistant", "message": {"model": "claude-opus-5-5"}}).to_string();
        std::fs::write(dir.join("old.jsonl"), format!("{reply}\n")).unwrap();
        std::fs::write(dir.join("new.jsonl"), "{\"type\":\"user\"}\n").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(dir.join("new.jsonl")).unwrap().set_modified(later).unwrap();
        let found = |worktree, reported| {
            let out = std::process::Command::new("sh").arg("-c").arg(session_script(worktree, reported)).env("HOME", home.path()).output().unwrap();
            parse_session(&String::from_utf8_lossy(&out.stdout)).filter(|_| out.status.success())
        };
        assert_eq!(found("/w/repo", Some("old")), Some(Session { id: "old".into(), model: Some("claude-opus-5-5".into()) }));
        assert_eq!(found("/w/repo", Some("gone")).map(|s| s.id), Some("new".into()));
        assert_eq!(found("/w/repo", None).map(|s| s.id), Some("new".into()));
        assert_eq!(found("/w/other", None), None);
    }
}
