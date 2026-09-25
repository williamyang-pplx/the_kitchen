//! One-line, model-written summaries of functions and classes for the
//! viewer's outline mode. Stored in the `summaries` table keyed by the
//! symbol and a hash of its source — never written into the code.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use kitchen_symbols::Symbol;

use crate::host::Host;
use crate::util::content_hash;

/// Source files larger than this are sent truncated.
const MAX_PROMPT_SOURCE: usize = 60_000;

/// DB key for a symbol: `<file>::<Parent.name>`.
pub fn symbol_key(path: &str, sym: &Symbol) -> String {
    format!("{path}::{}", sym.path())
}

/// Hash of the symbol's own lines, so editing one function only
/// invalidates that function's summary.
pub fn symbol_hash(source: &str, sym: &Symbol) -> String {
    let body: Vec<&str> = source.lines().skip(sym.line.saturating_sub(1)).take(sym.end_line + 1 - sym.line.max(1)).collect();
    content_hash(&body.join("\n"))
}

pub fn build_prompt(path: &str, source: &str, symbols: &[&Symbol]) -> String {
    let mut src = source;
    if src.len() > MAX_PROMPT_SOURCE {
        let mut cut = MAX_PROMPT_SOURCE;
        while !src.is_char_boundary(cut) {
            cut -= 1;
        }
        src = &src[..cut];
    }
    let list: Vec<String> = symbols.iter().map(|s| format!("- {} ({}, line {})", s.path(), s.kind, s.line)).collect();
    format!(
        "Summarize each listed definition from the file `{path}` in one short sentence (max 15 words): \
         what it does or represents, not how. Reply with ONLY a JSON object mapping each name exactly \
         as listed to its summary, no prose, no code fences.\n\nDefinitions:\n{}\n\nFile:\n```\n{src}\n```\n",
        list.join("\n")
    )
}

/// Pull the `{name: summary}` object out of a model reply, tolerating code
/// fences and surrounding prose.
pub fn parse_response(text: &str) -> Result<HashMap<String, String>> {
    let start = text.find('{').context("no JSON object in reply")?;
    let end = text.rfind('}').context("no JSON object in reply")?;
    if end < start {
        bail!("no JSON object in reply");
    }
    let v: serde_json::Value = serde_json::from_str(&text[start..=end]).context("reply is not valid JSON")?;
    let obj = v.as_object().context("reply is not a JSON object")?;
    Ok(obj
        .iter()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.trim().to_string())))
        .filter(|(_, v)| !v.is_empty())
        .collect())
}

/// Ask the model (via `claude -p` on the mac) for summaries of `symbols`.
/// Blocking; returns symbol path -> summary.
pub fn generate(model: &str, path: &str, source: &str, symbols: &[&Symbol]) -> Result<HashMap<String, String>> {
    if symbols.is_empty() {
        return Ok(HashMap::new());
    }
    let prompt = build_prompt(path, source, symbols);
    let out = Host::Local.run_with_stdin(&["claude", "-p", "--model", model, "--output-format", "text"], Some("/tmp"), Some(prompt.as_bytes()))?;
    let text = out.into_result("claude -p")?;
    parse_response(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json() {
        let r = parse_response("Sure:\n```json\n{\"Greeter.greet\": \"Greets someone.\", \"x\": 3}\n```").unwrap();
        assert_eq!(r.get("Greeter.greet").map(String::as_str), Some("Greets someone."));
        assert!(!r.contains_key("x"));
        assert!(parse_response("no json").is_err());
    }

    #[test]
    fn hash_covers_only_the_symbol() {
        let a = "def f():\n    return 1\n\ndef g():\n    return 2\n";
        let b = "def f():\n    return 1\n\ndef g():\n    return 3\n";
        let sa = kitchen_symbols::extract("a.py", a);
        let sb = kitchen_symbols::extract("a.py", b);
        assert_eq!(symbol_hash(a, &sa[0]), symbol_hash(b, &sb[0]));
        assert_ne!(symbol_hash(a, &sa[1]), symbol_hash(b, &sb[1]));
        assert_eq!(symbol_key("a.py", &sa[0]), "a.py::f");
        assert!(build_prompt("a.py", a, &[&sa[0]]).contains("- f (function, line 1)"));
    }
}
