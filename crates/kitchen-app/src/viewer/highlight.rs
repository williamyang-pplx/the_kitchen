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

fn syntaxes() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(SyntaxSet::load_defaults_newlines)
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
