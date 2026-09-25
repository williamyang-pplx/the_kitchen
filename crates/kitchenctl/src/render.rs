//! Human-readable output. `--json` bypasses all of this and prints the raw result.

use serde_json::Value;

fn s(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn opt(v: &Value, prefix: &str) -> String {
    if v.is_null() { String::new() } else { format!("{prefix}{}", s(v)) }
}

fn tabs(out: &mut String, tabs: &Value, indent: &str) {
    for t in tabs.as_array().into_iter().flatten() {
        let mark = if t["selected"].as_bool() == Some(true) { "*" } else { " " };
        let kind = if t["kind"] == "readonly" { "RO" } else { "RW" };
        let unread = if t["unread"].as_bool() == Some(true) { " ●" } else { "" };
        out.push_str(&format!(
            "{indent}{mark} tab:{} [{kind}] {}{unread}  (id {}{}{})\n{indent}    {}\n",
            s(&t["index"]),
            s(&t["title"]),
            s(&t["id"]),
            opt(&t["branch"], ", branch "),
            opt(&t["pr_number"], ", PR #"),
            s(&t["worktree"]),
        ));
    }
}

/// Render a successful result for `method`.
pub fn render(method: &str, v: &Value) -> String {
    let mut out = String::new();
    match method {
        "system.ping" => out.push_str(&format!("pong (kitchen {}, protocol {})\n", s(&v["version"]), s(&v["protocol"]))),
        "system.identify" => {
            let ws = &v["caller"]["workspace"];
            let tab = &v["caller"]["tab"];
            if ws.is_null() {
                out.push_str("caller: not inside a kitchen tab\n");
            } else {
                out.push_str(&format!("workspace: {} (id {}, {} {})\n", s(&ws["name"]), s(&ws["id"]), s(&ws["host"]), s(&ws["repo_path"])));
            }
            if !tab.is_null() {
                out.push_str(&format!(
                    "tab:       {} [{}] (id {})\nworktree:  {}\n{}{}{}",
                    s(&tab["title"]),
                    s(&tab["kind"]),
                    s(&tab["id"]),
                    s(&tab["worktree"]),
                    opt(&tab["branch"], "branch:    ") + if tab["branch"].is_null() { "" } else { "\n" },
                    opt(&tab["pr_number"], "pr:        #") + if tab["pr_number"].is_null() { "" } else { "\n" },
                    opt(&tab["plan_file"], "plan:      ") + if tab["plan_file"].is_null() { "" } else { "\n" },
                ));
            }
            out.push_str(&format!("focused:   workspace {} tab {}\n", s(&v["focused"]["workspace"]), s(&v["focused"]["tab"])));
        }
        "workspace.list" => {
            for w in v.as_array().into_iter().flatten() {
                let mark = if w["selected"].as_bool() == Some(true) { "*" } else { " " };
                out.push_str(&format!(
                    "{mark} workspace:{} {}  [{}] {}  (id {})\n",
                    s(&w["index"]),
                    s(&w["name"]),
                    s(&w["host"]),
                    s(&w["repo_path"]),
                    s(&w["id"])
                ));
                tabs(&mut out, &w["tabs"], "    ");
            }
            if out.is_empty() {
                out.push_str("no workspaces (create one: kitchenctl workspace new --repo <path>)\n");
            }
        }
        "workspace.current" => {
            out.push_str(&format!("{} [{}] {} (id {})\n", s(&v["name"]), s(&v["host"]), s(&v["repo_path"]), s(&v["id"])));
            tabs(&mut out, &v["tabs"], "  ");
        }
        "tab.list" => {
            tabs(&mut out, v, "");
            if out.is_empty() {
                out.push_str("no tabs\n");
            }
        }
        "tab.create" => out.push_str(&format!(
            "opened {} tab {} (id {})\nworktree: {}\n{}",
            s(&v["kind"]),
            s(&v["title"]),
            s(&v["id"]),
            s(&v["worktree"]),
            opt(&v["branch"], "branch:   ") + "\n"
        )),
        "tab.read_screen" => {
            let text = s(&v["text"]);
            out.push_str(text.trim_end_matches('\n'));
            out.push('\n');
        }
        "plan.path" => out.push_str(&format!("{}\n", s(&v["plan_file"]))),
        "plan.comments" => {
            for c in v.as_array().into_iter().flatten() {
                let range = if c["end_line"].is_null() { s(&c["line"]) } else { format!("{}-{}", s(&c["line"]), s(&c["end_line"])) };
                out.push_str(&format!(
                    "[{}] line {range} ({}): `{}`\n    {}\n",
                    s(&c["id"]),
                    s(&c["state"]),
                    s(&c["quote"]).trim(),
                    s(&c["body"]).replace('\n', "\n    ")
                ));
            }
            if out.is_empty() {
                out.push_str("no open plan comments\n");
            }
        }
        "worktree.list" => {
            for w in v.as_array().into_iter().flatten() {
                let role = s(&w["role"]);
                let dirty = if w["dirty"].as_bool() == Some(true) { " (dirty)" } else { "" };
                let head = if w["branch"].is_null() { format!("detached {}", &s(&w["head"]).chars().take(10).collect::<String>()) } else { s(&w["branch"]) };
                out.push_str(&format!("{:<9} {}{dirty}\n          {head}{}\n", role, s(&w["path"]), opt(&w["tab"], "  tab ")));
            }
        }
        "pr.threads" => {
            for t in v.as_array().into_iter().flatten() {
                let state = if t["is_resolved"].as_bool() == Some(true) { "resolved" } else { "open" };
                let line = t["line"].as_i64().or(t["original_line"].as_i64()).map(|l| format!(":{l}")).unwrap_or_default();
                out.push_str(&format!("{} [{state}] {}{line}\n", s(&t["id"]), s(&t["path"])));
                for c in t["comments"].as_array().into_iter().flatten() {
                    out.push_str(&format!("    @{}: {}\n", s(&c["author"]), s(&c["body"]).replace('\n', "\n      ")));
                }
            }
            if out.is_empty() {
                out.push_str("no review threads\n");
            }
        }
        "review.draft.list" => {
            for d in v.as_array().into_iter().flatten() {
                let line = if d["line"].is_null() { "file".to_string() } else { format!("{}:{}", s(&d["side"]), s(&d["line"])) };
                out.push_str(&format!(
                    "[{}] {} {}#{} {} {line}{}\n    {}\n",
                    s(&d["id"]),
                    s(&d["state"]),
                    s(&d["github_slug"]),
                    s(&d["pr_number"]),
                    s(&d["path"]),
                    opt(&d["severity"], " "),
                    s(&d["body"]).replace('\n', "\n    ")
                ));
            }
            if out.is_empty() {
                out.push_str("no drafts\n");
            }
        }
        "review.runs" => {
            for r in v.as_array().into_iter().flatten() {
                out.push_str(&format!(
                    "run {} {} {}#{} by {} — {}\n",
                    s(&r["id"]),
                    s(&r["status"]),
                    s(&r["github_slug"]),
                    s(&r["pr_number"]),
                    s(&r["author"]),
                    s(&r["title"])
                ));
            }
            if out.is_empty() {
                out.push_str("no review runs\n");
            }
        }
        "review.watch.list" => {
            for l in v.as_array().into_iter().flatten() {
                out.push_str(&format!("{}\n", s(l)));
            }
            if out.is_empty() {
                out.push_str("watch list is empty\n");
            }
        }
        "notification.list" => {
            for n in v.as_array().into_iter().flatten() {
                let unread = if n["read"].as_bool() == Some(true) { " " } else { "●" };
                out.push_str(&format!("{unread} [{}] {}: {}\n", s(&n["id"]), s(&n["title"]), s(&n["body"])));
            }
            if out.is_empty() {
                out.push_str("no notifications\n");
            }
        }
        _ => out.push_str(&generic(v)),
    }
    out
}

/// Fallback: `ok` plus any scalar fields, or pretty JSON for nested values.
fn generic(v: &Value) -> String {
    match v {
        Value::Null => "ok\n".into(),
        Value::Object(m) if m.values().all(|x| !x.is_object() && !x.is_array()) => {
            let mut out = String::from("ok");
            for (k, x) in m {
                if !x.is_null() {
                    out.push_str(&format!("  {k}={}", s(x)));
                }
            }
            out.push('\n');
            out
        }
        other => format!("{}\n", serde_json::to_string_pretty(other).unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn workspace_list() {
        let v = json!([{"id":"a1","index":1,"name":"air","host":"local","repo_path":"/r","selected":true,
            "tabs":[{"id":"t1","index":1,"kind":"edit","title":"Fix","worktree":"/r-fix","branch":"william/fix","selected":true,"unread":true}]}]);
        let out = render("workspace.list", &v);
        assert!(out.contains("* workspace:1 air  [local] /r"));
        assert!(out.contains("* tab:1 [RW] Fix ●  (id t1, branch william/fix)"));
    }

    #[test]
    fn generic_scalars() {
        assert_eq!(render("tab.send", &json!({"id": "x"})), "ok  id=x\n");
        assert_eq!(render("whatever", &Value::Null), "ok\n");
    }

    #[test]
    fn screen_text_verbatim() {
        assert_eq!(render("tab.read_screen", &json!({"id":"x","text":"$ ls\na\n\n\n"})), "$ ls\na\n");
    }
}
