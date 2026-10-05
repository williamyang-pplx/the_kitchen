//! Syntax highlighting (syntect, pure-Rust regex engine). Runs on loader
//! threads; the result is per-line color spans over tab-expanded text.

use std::ops::Range;
use std::sync::OnceLock;

use egui::Color32;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxDefinition, SyntaxSet};

use super::doc::expand_tabs;

pub type Spans = Vec<Vec<(Range<usize>, Color32)>>;

/// Files longer than this are shown unhighlighted.
const MAX_LINES: usize = 20_000;

/// Enabled plugin folders, set once at startup before the first file is
/// highlighted: their `syntaxes/*.sublime-syntax` join the grammars, and the
/// first `themes/*.tmTheme` replaces the default theme.
static PLUGIN_DIRS: OnceLock<Vec<std::path::PathBuf>> = OnceLock::new();

pub fn set_plugin_dirs(dirs: Vec<std::path::PathBuf>) {
    let _ = PLUGIN_DIRS.set(dirs);
}

/// Files with `ext` in the `sub` folder of each plugin, sorted per plugin.
fn plugin_files(sub: &str, ext: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for d in PLUGIN_DIRS.get().into_iter().flatten() {
        let Ok(rd) = std::fs::read_dir(d.join(sub)) else { continue };
        let mut files: Vec<_> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == ext)).collect();
        files.sort();
        out.extend(files);
    }
    out
}

fn syntaxes() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(|| with_syntaxes(&plugin_files("syntaxes", "sublime-syntax")))
}

/// The bundled grammars plus `files`, added one at a time so a grammar that
/// fails to compile skips only itself.
fn with_syntaxes(files: &[std::path::PathBuf]) -> SyntaxSet {
    if files.is_empty() {
        return SyntaxSet::load_defaults_newlines();
    }
    let mut b = SyntaxSet::load_defaults_newlines().into_builder();
    for f in files {
        let name = f.file_stem().map(|n| n.to_string_lossy().into_owned());
        let def = std::fs::read_to_string(f)
            .map_err(|e| e.to_string())
            .and_then(|s| SyntaxDefinition::load_from_str(&s, true, name.as_deref()).map_err(|e| e.to_string()));
        match def {
            Ok(def) => b.add(def),
            Err(e) => log::warn!("plugin syntax {}: {e}", f.display()),
        }
    }
    b.build()
}

/// Color for a highlight group: `#rrggbb`, or a TextMate scope
/// (`keyword`, `comment`, `entity.name.function`, …) through the theme.
pub fn group_color(group: &str) -> Option<Color32> {
    if let Some(hex) = group.strip_prefix('#') {
        let v = u32::from_str_radix(hex, 16).ok().filter(|_| hex.len() == 6)?;
        return Some(Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8));
    }
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, Option<Color32>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(c) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(group) {
        return *c;
    }
    let color = match group {
        "error" => Some(crate::theme::RED),
        "warning" => Some(crate::theme::YELLOW),
        "info" | "hint" => Some(crate::theme::CYAN),
        "todo" => Some(crate::theme::YELLOW),
        _ => syntect::parsing::Scope::new(group).ok().map(|scope| {
            let h = syntect::highlighting::Highlighter::new(theme());
            let style = h.style_for_stack(&[scope]);
            Color32::from_rgb(style.foreground.r, style.foreground.g, style.foreground.b)
        }),
    };
    cache.lock().unwrap_or_else(|e| e.into_inner()).insert(group.to_string(), color);
    color
}

fn theme() -> &'static Theme {
    static T: OnceLock<Theme> = OnceLock::new();
    T.get_or_init(|| {
        let plugin = plugin_files("themes", "tmTheme").into_iter().find_map(|f| {
            ThemeSet::get_theme(&f).inspect_err(|e| log::warn!("plugin theme {}: {e}", f.display())).ok()
        });
        if let Some(t) = plugin {
            return t;
        }
        let mut set = ThemeSet::load_defaults();
        set.themes.remove("base16-ocean.dark").or_else(|| set.themes.into_values().next()).expect("syntect ships themes")
    })
}

/// Highlight `text` as the language of `path` (by file name, extension,
/// then first line).
pub fn highlight(path: &str, text: &str) -> Option<Spans> {
    let ss = syntaxes();
    let file = path.rsplit('/').next().unwrap_or(path);
    let ext = file.rsplit('.').next().unwrap_or("");
    let first = text.lines().next().unwrap_or("");
    let syntax = ss
        .find_syntax_by_extension(file)
        .or_else(|| ss.find_syntax_by_extension(ext))
        .or_else(|| match ext {
            "ts" | "tsx" | "mts" | "cts" | "jsx" | "mjs" | "cjs" => ss.find_syntax_by_extension("js"),
            "toml" | "cfg" | "ini" => ss.find_syntax_by_extension("ini"),
            _ => None,
        })
        .or_else(|| ss.find_syntax_by_first_line(first))?;
    if text.lines().count() > MAX_LINES {
        return None;
    }
    let mut h = HighlightLines::new(syntax, theme());
    let mut out = Vec::new();
    for line in text.lines() {
        let expanded = expand_tabs(line) + "\n";
        let regions = h.highlight_line(&expanded, ss).ok()?;
        let mut spans = Vec::new();
        let mut pos = 0;
        for (style, piece) in regions {
            let end = (pos + piece.len()).min(expanded.len() - 1);
            if end > pos {
                let c = style.foreground;
                spans.push((pos..end, Color32::from_rgb(c.r, c.g, c.b)));
            }
            pos += piece.len();
        }
        out.push(spans);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust() {
        let s = highlight("a.rs", "fn main() {\n\tlet x = 1;\n}\n").unwrap();
        assert_eq!(s.len(), 3);
        assert!(s[0].len() > 1, "keywords and names get distinct colors");
        assert_eq!(s[1].last().unwrap().0.end, "    let x = 1;".len());
        assert!(highlight("x.unknownext", "plain").is_none());
    }
    /// Every bundled plugin's grammars and `tests/syntax_test_*` files, in
    /// Sublime's syntax-test format: a header line `<comment> SYNTAX TEST
    /// "…/Name.sublime-syntax"`, then assertion lines `<comment> ^^^ selector`
    /// (the columns under the carets) or `<comment> <- selector` (the comment's
    /// column), checked against the last line that isn't an assertion.
    #[test]
    fn plugin_syntax_tests() {
        use syntect::highlighting::ScopeSelectors;
        use syntect::parsing::{ParseState, ScopeStack};
        use std::str::FromStr;

        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let mut dirs: Vec<_> = std::fs::read_dir(&root).unwrap().filter_map(|e| e.ok().map(|e| e.path())).collect();
        dirs.sort();
        // KITCHEN_PLUGIN_TEST=name runs just that plugin's tests.
        if let Ok(only) = std::env::var("KITCHEN_PLUGIN_TEST") {
            dirs.retain(|d| d.file_name().is_some_and(|n| *n == *only));
        }
        let mut failures = Vec::new();
        let mut checked = 0;
        for dir in dirs {
            let mut grammars: Vec<_> = std::fs::read_dir(dir.join("syntaxes")).into_iter().flatten().filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "sublime-syntax")).collect();
            grammars.sort();
            for g in &grammars {
                let src = std::fs::read_to_string(g).unwrap();
                if let Err(e) = SyntaxDefinition::load_from_str(&src, true, None) {
                    failures.push(format!("{}: {e}", g.display()));
                }
            }
            let ss = with_syntaxes(&grammars);
            let tests = std::fs::read_dir(dir.join("tests")).into_iter().flatten().filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("syntax_test")));
            for t in tests {
                let text = std::fs::read_to_string(&t).unwrap();
                let header = text.lines().next().unwrap_or("");
                let (comment, rest) = header.split_once("SYNTAX TEST").unwrap_or_else(|| panic!("{}: no SYNTAX TEST header", t.display()));
                let comment = comment.trim();
                let stem = rest.trim().trim_matches('"').rsplit('/').next().unwrap().trim_end_matches(".sublime-syntax");
                let scope = grammars.iter().find(|g| g.file_stem().is_some_and(|s| s == stem)).map(|g| {
                    let src = std::fs::read_to_string(g).unwrap();
                    SyntaxDefinition::load_from_str(&src, true, None).unwrap().scope
                });
                let Some(syntax) = scope.and_then(|s| ss.find_syntax_by_scope(s)) else {
                    failures.push(format!("{}: grammar {stem} not found", t.display()));
                    continue;
                };
                let mut state = ParseState::new(syntax);
                let mut stack = ScopeStack::new();
                // Scope stack at each character of the last code line (columns
                // count characters, as in Sublime).
                let mut columns: Vec<ScopeStack> = Vec::new();
                for (n, line) in text.lines().enumerate() {
                    let body = line.trim_start();
                    let indent = line.chars().count() - body.chars().count();
                    let assertion = body.strip_prefix(comment).filter(|_| n > 0).map(|r| r.trim_start()).filter(|r| r.starts_with('^') || r.starts_with("<-"));
                    let line_nl = format!("{line}\n");
                    let ops = state.parse_line(&line_nl, &ss).unwrap();
                    let mut at = Vec::with_capacity(line_nl.len());
                    let mut ops = ops.into_iter().peekable();
                    for (i, _) in line_nl.char_indices() {
                        while let Some((_, op)) = ops.next_if(|(pos, _)| *pos <= i) {
                            stack.apply(&op).unwrap();
                        }
                        at.push(stack.clone());
                    }
                    for (_, op) in ops {
                        stack.apply(&op).unwrap();
                    }
                    let Some(r) = assertion else {
                        columns = at;
                        continue;
                    };
                    let (cols, selector) = if let Some(sel) = r.strip_prefix("<-") {
                        (indent..indent + 1, sel)
                    } else {
                        let byte = line.find('^').unwrap();
                        let start = line[..byte].chars().count();
                        let len = line[byte..].chars().take_while(|c| *c == '^').count();
                        (start..start + len, &line[byte + len..])
                    };
                    let selector = selector.trim();
                    // A bare `- x` means "not x" (syntect needs something before the `-`).
                    let (negate, positive) = match selector.strip_prefix('-') {
                        Some(x) => (true, x.trim()),
                        None => (false, selector),
                    };
                    let sel = ScopeSelectors::from_str(positive).unwrap_or_else(|e| panic!("{}:{}: bad selector {selector:?}: {e:?}", t.display(), n + 1));
                    checked += 1;
                    for c in cols {
                        // Past the end, the newline's scope (what carries on to the next line).
                        let Some(s) = columns.get(c).or(columns.last()) else {
                            failures.push(format!("{}:{}: no line to check", t.display(), n + 1));
                            break;
                        };
                        if sel.does_match(s.as_slice()).is_some() == negate {
                            failures.push(format!("{}:{} col {c}: expected {selector}, got {s:?}", t.display(), n + 1));
                            break;
                        }
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{} of {checked} assertions failed:\n{}", failures.len(), failures.join("\n"));
    }

    #[test]
    fn plugin_themes_load() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        for e in std::fs::read_dir(root).unwrap().flatten() {
            for t in std::fs::read_dir(e.path().join("themes")).into_iter().flatten().flatten() {
                ThemeSet::get_theme(t.path()).unwrap_or_else(|err| panic!("{}: {err}", t.path().display()));
            }
        }
    }
}
