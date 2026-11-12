//! Syntax highlighting. Languages with a tree-sitter grammar (see
//! `kitchen_symbols::highlight_grammar`) use its highlights query, like
//! neovim; everything else uses syntect with bat's syntaxes (two-face). Runs
//! on loader threads; the result is per-line color spans over tab-expanded text.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Mutex, OnceLock};

use egui::Color32;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color, StyleModifier, Theme, ThemeItem, ThemeSettings};
use syntect::parsing::SyntaxSet;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use super::doc::expand_tabs;

pub type Spans = Vec<Vec<(Range<usize>, Color32)>>;

/// Files longer than this are shown unhighlighted.
const MAX_LINES: usize = 20_000;

/// Highlight groups: tree-sitter capture (a capture like `function.method`
/// falls back to `function`), the TextMate scopes syntect grammars use for
/// it, and its tokyonight.nvim color. Anything else is plain text.
const GROUPS: &[(&str, &str, u32)] = &[
    ("comment", "comment", 0x565f89),
    ("string", "string", 0x9ece6a),
    ("string.escape", "constant.character.escape", 0xbb9af7),
    ("string.special", "string.regexp", 0xb4f9f8),
    ("number", "constant.numeric", 0xff9e64),
    ("boolean", "constant.language", 0xff9e64),
    ("constant", "constant, support.constant", 0xff9e64),
    ("keyword", "keyword, storage", 0xbb9af7),
    ("operator", "keyword.operator", 0x89ddff),
    ("punctuation", "punctuation", 0x89ddff),
    ("punctuation.bracket", "punctuation.section", 0xa9b1d6),
    ("function", "entity.name.function, variable.function, support.function", 0x7aa2f7),
    ("function.builtin", "support.function.builtin", 0x2ac3de),
    ("function.macro", "support.macro, entity.name.macro", 0x7dcfff),
    ("constructor", "entity.name.class, support.class", 0x2ac3de),
    ("type", "entity.name.type, support.type", 0x2ac3de),
    ("type.builtin", "support.type.primitive, storage.type.primitive", 0x27a1b9),
    ("property", "variable.other.member, variable.other.property, entity.name.tag.yaml", 0x73daca),
    ("variable.parameter", "variable.parameter", 0xe0af68),
    ("variable.builtin", "variable.language", 0xf7768e),
    ("attribute", "entity.other.attribute-name, meta.annotation", 0x7dcfff),
    ("module", "entity.name.namespace", 0x7dcfff),
    ("label", "entity.name.label", 0x7aa2f7),
    ("tag", "entity.name.tag", 0xf7768e),
    ("", "markup.heading", 0x7aa2f7),
    ("", "markup.inserted", 0x9ece6a),
    ("", "markup.deleted", 0xf7768e),
];

fn rgb(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// Extra grammar folders (plugins' `syntaxes/`), set once at startup before
/// the first file is highlighted.
static EXTRA_SYNTAX_DIRS: OnceLock<Vec<std::path::PathBuf>> = OnceLock::new();

pub fn set_extra_syntax_dirs(dirs: Vec<std::path::PathBuf>) {
    let _ = EXTRA_SYNTAX_DIRS.set(dirs);
}

/// Build the syntect grammars and theme on a thread, so the first file
/// opened doesn't wait for them. Call after [`set_extra_syntax_dirs`]: the
/// set is built once, with whatever folders are known then.
pub fn warm() {
    std::thread::spawn(|| {
        let start = std::time::Instant::now();
        let _ = (syntaxes(), theme());
        log::debug!(target: "kitchen::viewer", "{:.0}ms highlighter ready", start.elapsed().as_secs_f64() * 1000.0);
    });
}

fn syntaxes() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(|| {
        let dirs = EXTRA_SYNTAX_DIRS.get().cloned().unwrap_or_default();
        if dirs.is_empty() {
            return two_face::syntax::extra_newlines();
        }
        let mut b = two_face::syntax::extra_newlines().into_builder();
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
        return Some(rgb(v));
    }
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Color32>>>> = OnceLock::new();
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

/// A code fence's language (`python`, `rs`, `sh`, …) as the file
/// extension syntect looks grammars up by (`py`, `rs`, `sh`).
pub fn fence_extension(lang: &str) -> Option<String> {
    syntaxes().find_syntax_by_token(lang).and_then(|s| s.file_extensions.first().cloned())
}

/// [`GROUPS`] as a TextMate theme (`.tmTheme`) on `background`: the
/// viewer's colors for renderers that take a theme file (egui_commonmark).
pub fn tm_theme(name: &str, background: Color32) -> String {
    let hex = |c: Color32| format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>name</key><string>{name}</string><key>settings</key><array>\
         <dict><key>settings</key><dict><key>background</key><string>{}</string><key>foreground</key><string>{}</string></dict></dict>",
        hex(background),
        hex(crate::theme::TEXT)
    );
    for (_, scopes, v) in GROUPS {
        out.push_str(&format!("<dict><key>scope</key><string>{scopes}</string><key>settings</key><dict><key>foreground</key><string>{}</string></dict></dict>", hex(rgb(*v))));
    }
    out.push_str("</array></dict></plist>\n");
    out
}

/// [`GROUPS`] as a syntect theme.
fn theme() -> &'static Theme {
    static T: OnceLock<Theme> = OnceLock::new();
    T.get_or_init(|| {
        let color = |v: u32| Color { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8, a: 0xff };
        let t = crate::theme::TEXT;
        Theme {
            settings: ThemeSettings { foreground: Some(Color { r: t.r(), g: t.g(), b: t.b(), a: 0xff }), ..Default::default() },
            scopes: GROUPS
                .iter()
                .map(|(_, scopes, v)| ThemeItem {
                    scope: scopes.parse().expect("valid selector"),
                    style: StyleModifier { foreground: Some(color(*v)), ..Default::default() },
                })
                .collect(),
            ..Default::default()
        }
    })
}

/// The tree-sitter highlighter for `path`'s language, built once per language.
fn tree_sitter_config(path: &str) -> Option<&'static HighlightConfiguration> {
    static CONFIGS: OnceLock<Mutex<HashMap<String, Option<&'static HighlightConfiguration>>>> = OnceLock::new();
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    let mut configs = CONFIGS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    *configs.entry(ext).or_insert_with(|| {
        let (language, highlights, locals) = kitchen_symbols::highlight_grammar(path)?;
        let mut config = HighlightConfiguration::new(language, "", &highlights, "", &locals)
            .inspect_err(|e| log::warn!("highlight query for {path}: {e}"))
            .ok()?;
        // Plain `@variable` is default text; drop it, since the last matching
        // pattern wins and some queries (Go) list it after `@function`.
        config.query.disable_capture("variable");
        config.configure(&GROUPS.iter().map(|g| g.0).collect::<Vec<_>>());
        Some(&*Box::leak(Box::new(config)))
    })
}

fn tree_sitter_highlight(config: &HighlightConfiguration, text: &str) -> Option<Spans> {
    let mut colors = vec![crate::theme::TEXT; text.len()];
    let mut stack = Vec::new();
    for event in Highlighter::new().highlight(config, text.as_bytes(), None, |_| None).ok()? {
        match event.ok()? {
            HighlightEvent::HighlightStart(h) => stack.push(rgb(GROUPS[h.0].2)),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                if let Some(&c) = stack.last() {
                    colors[start..end].fill(c);
                }
            }
        }
    }
    let mut out = Vec::new();
    for line in text.lines() {
        let offset = line.as_ptr() as usize - text.as_ptr() as usize;
        let mut spans: Vec<(Range<usize>, Color32)> = Vec::new();
        let (mut col, mut pos) = (0, 0);
        for (i, ch) in line.char_indices() {
            // Same widths as expand_tabs.
            let (width, len) = if ch == '\t' { (4 - col % 4, 4 - col % 4) } else { (1, ch.len_utf8()) };
            let c = colors[offset + i];
            match spans.last_mut() {
                Some((r, last)) if *last == c => r.end += len,
                _ => spans.push((pos..pos + len, c)),
            }
            col += width;
            pos += len;
        }
        out.push(spans);
    }
    Some(out)
}

/// Highlight a fenced code block's `text` by its info string (`rust`, `py`,
/// `sh`, …): a language name or extension.
pub fn highlight_lang(lang: &str, text: &str) -> Option<Spans> {
    let ext = syntaxes().find_syntax_by_token(lang).and_then(|s| s.file_extensions.first().cloned()).unwrap_or_else(|| lang.to_string());
    highlight(&format!("code.{ext}"), text)
}

/// Highlight `text` as the language of `path` (by file name, extension,
/// then first line).
pub fn highlight(path: &str, text: &str) -> Option<Spans> {
    let lines = text.lines().count();
    if lines > MAX_LINES {
        return None;
    }
    let start = std::time::Instant::now();
    if let Some(spans) = tree_sitter_config(path).and_then(|c| tree_sitter_highlight(c, text)) {
        log::debug!(target: "kitchen::viewer", "{:.0}ms highlight {path} ({lines} lines, tree-sitter)", start.elapsed().as_secs_f64() * 1000.0);
        return Some(spans);
    }
    let ss = syntaxes();
    let file = path.rsplit('/').next().unwrap_or(path);
    let ext = file.rsplit('.').next().unwrap_or("");
    let first = text.lines().next().unwrap_or("");
    let syntax = ss
        .find_syntax_by_extension(file)
        .or_else(|| ss.find_syntax_by_extension(ext))
        .or_else(|| ss.find_syntax_by_first_line(first))?;
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
    log::debug!(target: "kitchen::viewer", "{:.0}ms highlight {path} ({lines} lines)", start.elapsed().as_secs_f64() * 1000.0);
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
