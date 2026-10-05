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

/// Build the grammars and theme on a thread, so the first file opened
/// doesn't wait for them. Call after [`set_extra_syntax_dirs`]: the set is
/// built once, with whatever folders are known then.
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
        let files = plugin_files("syntaxes", "sublime-syntax");
        if files.is_empty() {
            return SyntaxSet::load_defaults_newlines();
        }
        // One file at a time, so a grammar that fails to compile skips only itself.
        let mut b = SyntaxSet::load_defaults_newlines().into_builder();
        for f in files {
            let name = f.file_stem().map(|n| n.to_string_lossy().into_owned());
            let def = std::fs::read_to_string(&f)
                .map_err(|e| e.to_string())
                .and_then(|s| SyntaxDefinition::load_from_str(&s, true, name.as_deref()).map_err(|e| e.to_string()));
            match def {
                Ok(def) => b.add(def),
                Err(e) => log::warn!("plugin syntax {}: {e}", f.display()),
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
    let lines = text.lines().count();
    if lines > MAX_LINES {
        return None;
    }
    let start = std::time::Instant::now();
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
