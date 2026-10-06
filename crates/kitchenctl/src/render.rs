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
        let unread = if t["unread"].as_bool() == Some(true) { " ●" } else { "" };
        out.push_str(&format!(
            "{indent}{mark} tab:{} {}{unread}  (id {}{})\n{indent}    {}\n",
            s(&t["index"]),
            s(&t["title"]),
            s(&t["id"]),
            opt(&t["branch"], ", branch "),
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
                    "tab:       {} (id {})\nworktree:  {}\n{}{}{}",
                    s(&tab["title"]),
                    s(&tab["id"]),
                    s(&tab["worktree"]),
                    opt(&tab["branch"], "branch:    ") + if tab["branch"].is_null() { "" } else { "\n" },
                    opt(&tab["plan_file"], "plan:      ") + if tab["plan_file"].is_null() { "" } else { "\n" },
                    opt(&tab["diff_base"], "diff base: ") + if tab["diff_base"].is_null() { "" } else { "\n" },
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
            "opened tab {} (id {})\nworktree: {}\n{}",
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
        "plugin.list" => {
            for p in v.as_array().into_iter().flatten() {
                let status = match s(&p["status"]["state"]).as_str() {
                    "failed" => format!("FAILED: {}", s(&p["status"]["detail"])),
                    other => other.to_string(),
                };
                out.push_str(&format!("{:<12} {:<8} {}  [{}]\n", s(&p["name"]), s(&p["version"]), s(&p["description"]), status));
                let cmds: Vec<String> = p["commands"].as_array().into_iter().flatten().map(s).collect();
                if !cmds.is_empty() {
                    out.push_str(&format!("{:<12} commands: {}\n", "", cmds.join(", ")));
                }
                out.push_str(&format!("{:<12} {}\n", "", s(&p["dir"])));
            }
            if out.is_empty() {
                out.push_str("no plugins (put them in ~/.kitchen/plugins/<name>/)\n");
            }
        }
        "plugin.timers" => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            let ago = |t: &Value| t.as_f64().map(|t| format!("{:.0}s ago", now - t)).unwrap_or_else(|| "never".into());
            for t in v.as_array().into_iter().flatten() {
                let next = if t["running"].as_bool() == Some(true) { "running".to_string() } else { format!("next in {:.0}s", (t["next_due"].as_f64().unwrap_or(now) - now).max(0.0)) };
                out.push_str(&format!(
                    "{}.{}  every {:.0}s  runs {}  last {}  {}\n",
                    s(&t["plugin"]),
                    s(&t["name"]),
                    t["interval_secs"].as_f64().unwrap_or(0.0),
                    t["runs"],
                    ago(&t["last_fired"]),
                    next
                ));
                if let Some(e) = t["last_error"].as_str() {
                    out.push_str(&format!("    error: {e}\n"));
                }
                if t["stats"].is_object() {
                    out.push_str(&format!("    stats: {}\n", t["stats"]));
                }
            }
            if out.is_empty() {
                out.push_str("no plugin timers\n");
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
            "tabs":[{"id":"t1","index":1,"title":"Fix","worktree":"/r-fix","branch":"william/fix","selected":true,"unread":true}]}]);
        let out = render("workspace.list", &v);
        assert!(out.contains("* workspace:1 air  [local] /r"));
        assert!(out.contains("* tab:1 Fix ●  (id t1, branch william/fix)"));
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
