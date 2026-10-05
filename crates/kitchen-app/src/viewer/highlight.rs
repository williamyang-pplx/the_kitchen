//! Syntax highlighting (syntect, pure-Rust regex engine). Runs on loader
//! threads; the result is per-line color spans over tab-expanded text.

use std::ops::Range;
use std::sync::OnceLock;

use egui::Color32;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use super::doc::expand_tabs;

pub type Spans = Vec<Vec<(Range<usize>, Color32)>>;

/// Files longer than this are shown unhighlighted.
const MAX_LINES: usize = 20_000;

/// Extra grammar folders (plugins' `syntaxes/`), set once at startup before
/// the first file is highlighted.
static EXTRA_SYNTAX_DIRS: OnceLock<Vec<std::path::PathBuf>> = OnceLock::new();

pub fn set_extra_syntax_dirs(dirs: Vec<std::path::PathBuf>) {
    let _ = EXTRA_SYNTAX_DIRS.set(dirs);
}

fn syntaxes() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(|| {
        let dirs = EXTRA_SYNTAX_DIRS.get().cloned().unwrap_or_default();
        if dirs.is_empty() {
            return SyntaxSet::load_defaults_newlines();
        }
        let mut b = SyntaxSet::load_defaults_newlines().into_builder();
        for d in dirs {
            if let Err(e) = b.add_from_folder(&d, true) {
                log::warn!("plugin syntaxes in {}: {e}", d.display());
            }
        }
        b.build()
    })
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
        let mut set = ThemeSet::load_defaults();
        set.themes.remove("base16-ocean.dark").or_else(|| set.themes.into_values().next()).expect("syntect ships themes")
    })
}

/// Highlight `text` as the language of `path` (by extension, then first line).
pub fn highlight(path: &str, text: &str) -> Option<Spans> {
    let ss = syntaxes();
    let ext = path.rsplit('.').next().unwrap_or("");
    let first = text.lines().next().unwrap_or("");
    let syntax = ss
        .find_syntax_by_extension(ext)
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
}
