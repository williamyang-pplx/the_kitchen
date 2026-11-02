//! What a tab's agent did, read back from its Claude Code session
//! transcript (`~/.claude/projects/<worktree>/<session>.jsonl` on the tab's
//! host) and cut down to plain text another model can read as context.

use serde_json::Value;

use crate::host::Host;

/// Most transcript bytes fetched from the host: the tail, where the latest
/// planning round is.
const MAX_FETCH: usize = 32_000_000;

/// Longest text kept from one message, tool input and tool result.
const MAX_TEXT: usize = 4_000;
const MAX_TOOL_INPUT: usize = 300;
const MAX_TOOL_RESULT: usize = 600;

/// Claude Code's folder name for a project: the path with every character
/// that is not a letter or digit replaced by `-`.
pub fn project_dir_name(worktree: &str) -> String {
    worktree.trim_end_matches('/').chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// The tail of the newest session transcript of the agent working in
/// `worktree`, as raw JSONL. `None` if there is none.
pub fn latest_transcript(host: &Host, worktree: &str) -> Option<String> {
    let script = format!(
        "f=$(ls -t \"$HOME/.claude/projects/{dir}\"/*.jsonl 2>/dev/null | head -1); [ -n \"$f\" ] && tail -c {MAX_FETCH} \"$f\"",
        dir = project_dir_name(worktree),
    );
    let out = host.sh_bytes_ok(&script, None).ok()?;
    let text = String::from_utf8_lossy(&out).into_owned();
    (!text.trim().is_empty()).then_some(text)
}

/// The conversation in a transcript, one entry per message, tool call and
/// tool result, at most `max` bytes. The first prompt (the task) is always
/// kept; past the budget, the oldest entries after it are dropped.
pub fn condense(jsonl: &str, max: usize) -> String {
    let entries: Vec<String> = jsonl.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).flat_map(|e| entries(&e)).collect();
    let Some((first, rest)) = entries.split_first() else { return String::new() };
    let mut size = first.len();
    let mut tail = Vec::new();
    for e in rest.iter().rev() {
        if size + e.len() + 1 > max {
            break;
        }
        size += e.len() + 1;
        tail.push(e.as_str());
    }
    let mut out = first.clone();
    if tail.len() < rest.len() {
        out.push_str("\n[… earlier entries omitted …]");
    }
    for e in tail.iter().rev() {
        out.push('\n');
        out.push_str(e);
    }
    out
}

/// The readable entries of one transcript line. Kitchen bookkeeping,
/// expanded skill text (`isMeta`), subagent turns and thinking are skipped.
fn entries(e: &Value) -> Vec<String> {
    let role = e["type"].as_str().unwrap_or_default();
    if !matches!(role, "user" | "assistant") || e["isMeta"] == true || e["isSidechain"] == true {
        return Vec::new();
    }
    let who = if role == "user" { "William" } else { "agent" };
    match &e["message"]["content"] {
        Value::String(s) => vec![format!("[{who}] {}", clip(s.trim(), MAX_TEXT))],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b["type"].as_str()? {
                "text" => Some(format!("[{who}] {}", clip(b["text"].as_str()?.trim(), MAX_TEXT))),
                "tool_use" => Some(format!("[agent → {}] {}", b["name"].as_str()?, clip(&b["input"].to_string(), MAX_TOOL_INPUT))),
                "tool_result" => Some(format!("[tool result] {}", clip(result_text(&b["content"]).trim(), MAX_TOOL_RESULT))),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// `s` cut to at most `max` bytes (on a char boundary), marked when cut.
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = (0..=max).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
    format!("{}… [{} more bytes]", &s[..end], s.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jsonl(lines: &[Value]) -> String {
        lines.iter().map(|l| l.to_string() + "\n").collect()
    }

    #[test]
    fn project_dir_matches_claude_code() {
        assert_eq!(project_dir_name("/Users/w/Documents/the_kitchen"), "-Users-w-Documents-the-kitchen");
        assert_eq!(project_dir_name("/home/w/.wt/a.b/"), "-home-w--wt-a-b");
    }

    #[test]
    fn keeps_the_conversation_and_skips_the_rest() {
        let t = jsonl(&[
            json!({"type": "permission-mode", "permissionMode": "default"}),
            json!({"type": "user", "message": {"role": "user", "content": "/low-level-plan add a cache"}}),
            json!({"type": "user", "isMeta": true, "message": {"content": "skill body"}}),
            json!({"type": "assistant", "message": {"content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "Reading the code."},
                {"type": "tool_use", "name": "Read", "input": {"file_path": "/w/src/a.rs"}},
            ]}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": [{"type": "text", "text": "fn a() {}"}]}]}}),
            json!({"type": "assistant", "isSidechain": true, "message": {"content": [{"type": "text", "text": "subagent"}]}}),
        ]) + "not json\n";
        assert_eq!(
            condense(&t, 10_000),
            "[William] /low-level-plan add a cache\n[agent] Reading the code.\n[agent → Read] {\"file_path\":\"/w/src/a.rs\"}\n[tool result] fn a() {}"
        );
    }

    #[test]
    fn over_budget_keeps_the_task_and_the_latest() {
        let msg = |role: &str, text: &str| json!({"type": role, "message": {"content": text}});
        let t = jsonl(&[msg("user", "task"), msg("assistant", "old"), msg("assistant", "mid"), msg("user", "new")]);
        assert_eq!(condense(&t, 30), "[William] task\n[… earlier entries omitted …]\n[William] new");
        assert_eq!(condense("", 30), "");
    }

    #[test]
    fn clip_marks_the_cut() {
        assert_eq!(clip("abcdef", 10), "abcdef");
        assert_eq!(clip("abcdef", 3), "abc… [3 more bytes]");
        assert_eq!(clip("ééé", 3), "é… [4 more bytes]");
    }
}
