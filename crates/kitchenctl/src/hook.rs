//! Claude Code Stop hook: is the main agent done, or only between
//! background subagents?
//!
//! The main agent's turn ends (and Stop fires) each time a background
//! subagent reports back, while others still run. The session transcript
//! records each launch (a tool result with `isAsync` and an `agentId`) and
//! each report (a `<task-notification>` with that `<task-id>`); the agent is
//! done when every launch has reported.
//!
//! The hook input also names the agent's session, which a revived tab resumes.

use std::collections::HashSet;

use serde_json::Value;

/// Background subagents launched in `transcript` (JSONL) that have not
/// reported back yet.
pub fn pending_agents(transcript: &str) -> usize {
    let mut launched = HashSet::new();
    let mut reported = HashSet::new();
    for line in transcript.lines() {
        // Cheap filter: most lines are neither.
        if !line.contains("async_launched") && !line.contains("<task-notification>") {
            continue;
        }
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        let r = &e["toolUseResult"];
        if r["isAsync"] == true
            && let Some(id) = r["agentId"].as_str()
        {
            launched.insert(id.to_string());
        }
        for text in strings(&e) {
            for chunk in text.split("<task-notification>").skip(1) {
                if let Some(id) = between(chunk, "<task-id>", "</task-id>")
                    && between(chunk, "<status>", "</status>") != Some("running")
                {
                    reported.insert(id.to_string());
                }
            }
        }
    }
    launched.difference(&reported).count()
}

/// Whether a Stop hook with this stdin should notify: yes unless the
/// transcript shows background subagents still running. Unreadable input
/// notifies (better a spare banner than a missed one).
pub fn should_notify_on_stop(hook_input: &str) -> bool {
    let input: Value = serde_json::from_str(hook_input).unwrap_or_default();
    let Some(path) = input["transcript_path"].as_str() else { return true };
    std::fs::read_to_string(path).map_or(true, |t| pending_agents(&t) == 0)
}

/// The `session_id` of a hook's stdin, if it has one.
pub fn session_id(hook_input: &str) -> Option<String> {
    let input: Value = serde_json::from_str(hook_input).ok()?;
    input["session_id"].as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// Every string in `v`, recursively.
fn strings(v: &Value) -> Vec<&str> {
    match v {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().flat_map(strings).collect(),
        Value::Object(m) => m.values().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let len = s[start..].find(close)?;
    Some(s[start..start + len].trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn launch(id: &str) -> String {
        json!({"type": "user", "toolUseResult": {"isAsync": true, "status": "async_launched", "agentId": id}}).to_string()
    }

    fn report(id: &str, status: &str) -> String {
        let text = format!("<task-notification>\n<task-id>{id}</task-id>\n<status>{status}</status>\n</task-notification>");
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}}).to_string()
    }

    #[test]
    fn counts_launches_without_reports() {
        let t = [launch("a1"), launch("a2"), report("a1", "completed")].join("\n");
        assert_eq!(pending_agents(&t), 1);
        let t = [t, report("a2", "failed")].join("\n");
        assert_eq!(pending_agents(&t), 0);
    }

    #[test]
    fn running_status_is_not_a_report() {
        let t = [launch("a1"), report("a1", "running")].join("\n");
        assert_eq!(pending_agents(&t), 1);
    }

    #[test]
    fn background_bash_does_not_hold_back() {
        // Long-lived background commands (dev servers) never report; only agents count.
        let bash = json!({"type": "user", "toolUseResult": {"backgroundTaskId": "b1"}}).to_string();
        assert_eq!(pending_agents(&bash), 0);
    }

    #[test]
    fn reads_session_id() {
        assert_eq!(session_id(r#"{"session_id": "abc-1", "transcript_path": "/x"}"#).as_deref(), Some("abc-1"));
        assert_eq!(session_id(r#"{"session_id": ""}"#), None);
        assert_eq!(session_id("not json"), None);
    }

    #[test]
    fn notifies_when_transcript_is_missing() {
        assert!(should_notify_on_stop("not json"));
        assert!(should_notify_on_stop(r#"{"transcript_path": "/nonexistent/x.jsonl"}"#));
    }

    #[test]
    fn holds_back_while_agents_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, launch("a1")).unwrap();
        let input = json!({"transcript_path": path}).to_string();
        assert!(!should_notify_on_stop(&input));
        std::fs::write(&path, [launch("a1"), report("a1", "completed")].join("\n")).unwrap();
        assert!(should_notify_on_stop(&input));
    }
}
