//! Markdown as the viewer draws it, in cards (review comments, ⌘⇧A
//! answers, plan comments) and in the markdown plugin's rows. `github`
//! turns GitHub's flavour into what egui_commonmark draws; `segments` cuts
//! top-level code out for the viewer's highlighter; `draw` paints both.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Pos2, Rect, Sense, Ui, Vec2};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use regex::{Captures, Regex};

use super::doc::expand_tabs;
use super::highlight::{self, Spans};
use crate::theme;

/// Background of code in rendered Markdown rows, fenced and inline.
pub const CODE_BG: Color32 = Color32::from_rgb(0x24, 0x28, 0x3b);
/// Body text; bold and headings are `theme::TEXT`.
pub const PROSE: Color32 = Color32::from_rgb(0xc0, 0xca, 0xf5);
/// Room around a code panel's text.
const CODE_PAD: f32 = 6.0;
/// Highlighted snippets kept before the cache is emptied.
const SPANS_MAX: usize = 256;
/// What egui_commonmark parses with (`parser_options` in
/// egui_commonmark_backend 0.25), so both see the same blocks and links.
const OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_FOOTNOTES)
    .union(Options::ENABLE_DEFINITION_LIST);

/// Markdown to draw: its source and the look it takes.
pub struct Text<'a> {
    pub src: &'a str,
    /// `owner/repo` that `#12`, `@al` and SHAs link to (GitHub comments).
    pub repo: Option<&'a str>,
    /// The file the text is about; a suggestion is highlighted as it.
    pub path: Option<&'a str>,
    /// Body text size.
    pub size: f32,
    /// Faded body text (a dim card).
    pub dim: bool,
    /// Behind code, fenced and inline.
    pub code_bg: Color32,
}

/// A run of the source: prose for egui_commonmark, or top-level code.
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    /// Markdown, with the document's link definitions appended when it uses `]`.
    Prose(String),
    /// A fenced or indented block's text, without the fence lines.
    Code { lang: String, text: String, suggestion: bool },
}

/// `src` cut into prose and the code blocks at its top level; code inside a
/// list item or quote stays in the prose. Blank prose is dropped.
pub fn segments(src: &str) -> Vec<Segment> {
    let mut iter = Parser::new_ext(src, OPTIONS).into_offset_iter();
    // (block range, lang, text) of each top-level code block.
    let mut blocks: Vec<(Range<usize>, String, String)> = Vec::new();
    let mut depth = 0usize;
    let mut code: Option<(Range<usize>, String, String)> = None;
    for (event, range) in iter.by_ref() {
        match event {
            Event::Start(tag) => {
                if let (0, Tag::CodeBlock(kind)) = (depth, &tag) {
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    code = Some((range, lang, String::new()));
                }
                depth += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(c) = code.take() {
                        blocks.push(c);
                    }
                }
            }
            Event::Text(t) => {
                if let Some(c) = code.as_mut() {
                    c.2.push_str(&t);
                }
            }
            _ => {}
        }
    }
    let defs: Vec<&str> = iter.reference_definitions().iter().filter_map(|(_, d)| src.get(d.span.clone())).map(str::trim).collect();
    let mut out = Vec::new();
    let prose = |text: &str, out: &mut Vec<Segment>| {
        let text = trim_blank_lines(text);
        if text.is_empty() {
            return;
        }
        let mut text = text.to_string();
        if !defs.is_empty() && text.contains(']') {
            text.push_str("\n\n");
            text.push_str(&defs.join("\n"));
        }
        out.push(Segment::Prose(text));
    };
    let mut at = 0;
    for (range, lang, text) in blocks {
        prose(&src[at..range.start.max(at)], &mut out);
        let text = text.strip_suffix('\n').unwrap_or(&text).to_string();
        let suggestion = lang == "suggestion";
        out.push(Segment::Code { lang, text, suggestion });
        at = range.end;
    }
    prose(&src[at.min(src.len())..], &mut out);
    out
}

/// `text` without its leading blank lines and trailing whitespace (a
/// first line's indent is kept: it may be meaningful).
fn trim_blank_lines(text: &str) -> &str {
    let text = text.trim_end();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        if !line.trim().is_empty() {
            break;
        }
        start += line.len();
    }
    &text[start..]
}

/// GitHub-flavored `src` as egui_commonmark should draw it, as on GitHub:
/// HTML turned into Markdown (the viewer prints HTML as text), images as
/// links (it loads none), bare URLs linked, and with `repo` `#12`,
/// `o/r#12`, `@user` and commit SHAs linked. Code is never touched, and
/// running it on its own output changes nothing.
pub fn github(src: &str, repo: Option<&str>) -> String {
    let src = src.replace("\r\n", "\n");
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    // Open links and code blocks: their text is left alone.
    let mut skip = 0usize;
    let mut links = 0usize;
    // The open image: its range, url and alt text so far.
    let mut image: Option<(Range<usize>, String, String)> = None;
    // The open HTML block: its events were handled as a whole.
    let mut html_block = false;
    // `<a href>` targets of open inline links.
    let mut hrefs: Vec<String> = Vec::new();
    // Contiguous text events (the parser splits text at `_`, `*`, …).
    let mut run: Option<Range<usize>> = None;
    let flush = |run: &mut Option<Range<usize>>, edits: &mut Vec<(Range<usize>, String)>| {
        if let Some(r) = run.take() {
            let new = text_edit(&src[r.clone()], repo);
            if new != src[r.clone()] {
                edits.push((r, new));
            }
        }
    };
    for (event, range) in Parser::new_ext(&src, OPTIONS).into_offset_iter() {
        if !matches!(event, Event::Text(_)) || skip > 0 || image.is_some() {
            flush(&mut run, &mut edits);
        }
        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                image = Some((range, dest_url.to_string(), String::new()));
            }
            Event::End(TagEnd::Image) => {
                if let Some((r, url, alt)) = image.take() {
                    let label = if alt.is_empty() { "image".to_string() } else { format!("image: {alt}") };
                    // A link can't hold a link: inside one the image is its label.
                    let new = if links > 0 { label } else { format!("[{label}]({})", link_dest(&url)) };
                    edits.push((r, new));
                }
            }
            Event::Start(Tag::Link { .. }) => {
                skip += 1;
                links += 1;
            }
            Event::End(TagEnd::Link) => {
                skip = skip.saturating_sub(1);
                links = links.saturating_sub(1);
            }
            Event::Start(Tag::CodeBlock(_)) => skip += 1,
            Event::End(TagEnd::CodeBlock) => skip = skip.saturating_sub(1),
            Event::Start(Tag::HtmlBlock) => {
                html_block = true;
                let new = html_edit(&src[range.clone()]);
                if new != src[range.clone()] {
                    // An emptied block still ends the paragraph before it.
                    edits.push((range, if new.trim().is_empty() { "\n".into() } else { new }));
                }
            }
            Event::End(TagEnd::HtmlBlock) => html_block = false,
            Event::InlineHtml(html) if image.is_none() => {
                let new = inline_html_edit(&html, &mut hrefs);
                if new != *html {
                    edits.push((range, new));
                }
            }
            Event::Text(t) => {
                if let Some(img) = image.as_mut() {
                    img.2.push_str(&t);
                } else if skip == 0 && !html_block {
                    run = match run.take() {
                        Some(r) if r.end == range.start => Some(r.start..range.end),
                        Some(r) => {
                            flush(&mut Some(r), &mut edits);
                            Some(range)
                        }
                        None => Some(range),
                    };
                }
            }
            _ => {}
        }
    }
    flush(&mut run, &mut edits);
    // Back to front, so earlier ranges stay put; nested edits (an image in
    // a replaced range) can't happen, but never apply an overlapping one.
    edits.sort_by_key(|(r, _)| r.start);
    let mut out = String::with_capacity(src.len());
    let mut at = 0;
    for (r, new) in edits {
        if r.start < at {
            continue;
        }
        out.push_str(&src[at..r.start]);
        out.push_str(&new);
        at = r.end;
    }
    out.push_str(&src[at..]);
    out
}

/// A link destination as Markdown: in `<…>` when it has spaces or parens.
fn link_dest(url: &str) -> String {
    if url.contains([' ', '(', ')']) { format!("<{url}>") } else { url.to_string() }
}

/// The regexes `html_edit` applies, in order.
struct HtmlRules {
    comment: Regex,
    summary: Regex,
    details: Regex,
    br: Regex,
    a: Regex,
    img: Regex,
    md_img: Regex,
    attr_src: Regex,
    attr_alt: Regex,
    bold: Regex,
    italic: Regex,
    code: Regex,
    any_tag: Regex,
}

fn html_rules() -> &'static HtmlRules {
    static RULES: OnceLock<HtmlRules> = OnceLock::new();
    RULES.get_or_init(|| {
        let re = |s: &str| Regex::new(s).expect("valid regex");
        HtmlRules {
            comment: re(r"(?s)<!--.*?-->"),
            summary: re(r"(?is)<summary\b[^>]*>(.*?)</summary\s*>"),
            details: re(r"(?i)</?details\b[^>]*>"),
            br: re(r"(?i)<br\s*/?>"),
            a: re(r#"(?is)<a\b[^>]*?\bhref\s*=\s*["']([^"']*)["'][^>]*>(.*?)</a\s*>"#),
            img: re(r"(?i)<img\b[^>]*>"),
            md_img: re(r"!\[([^\]]*)\]\(([^)\s]+)\)"),
            attr_src: re(r#"(?i)\bsrc\s*=\s*["']([^"']*)["']"#),
            attr_alt: re(r#"(?i)\balt\s*=\s*["']([^"']*)["']"#),
            bold: re(r"(?i)</?(?:b|strong)\s*>"),
            italic: re(r"(?i)</?(?:i|em)\s*>"),
            code: re(r"(?i)</?(?:code|kbd)\s*>"),
            any_tag: re(r"</?[A-Za-z][A-Za-z0-9-]*(?:\s[^>]*)?/?>"),
        }
    })
}

/// An `<img>` tag as a link to its picture.
fn img_link(rules: &HtmlRules, tag: &str) -> String {
    let Some(src) = rules.attr_src.captures(tag).map(|c| c[1].to_string()) else { return String::new() };
    let label = match rules.attr_alt.captures(tag).map(|c| c[1].trim().to_string()) {
        Some(alt) if !alt.is_empty() => format!("image: {alt}"),
        _ => "image".to_string(),
    };
    format!("[{label}]({})", link_dest(&src))
}

/// HTML as the Markdown GitHub would show for it: comments dropped,
/// `<summary>` a bold `▸` line, `<br>` a hard break, links, images and
/// emphasis as Markdown, any other tag dropped with its content kept.
fn html_edit(html: &str) -> String {
    let r = html_rules();
    let s = r.comment.replace_all(html, "");
    let s = r.summary.replace_all(&s, |c: &Captures| format!("\n\n**▸ {}**\n\n", c[1].trim()));
    let s = r.details.replace_all(&s, "");
    let s = r.br.replace_all(&s, "  \n");
    let s = r.a.replace_all(&s, |c: &Captures| format!("[{}]({})", c[2].trim(), link_dest(&c[1])));
    let s = r.img.replace_all(&s, |c: &Captures| img_link(r, &c[0]));
    // Markdown lines after a tag belong to its HTML block until a blank line.
    let s = r.md_img.replace_all(&s, |c: &Captures| {
        let label = if c[1].trim().is_empty() { "image".to_string() } else { format!("image: {}", c[1].trim()) };
        format!("[{label}]({})", &c[2])
    });
    let s = r.bold.replace_all(&s, "**");
    let s = r.italic.replace_all(&s, "*");
    let s = r.code.replace_all(&s, "`");
    r.any_tag.replace_all(&s, "").into_owned()
}

/// One inline HTML tag (or comment): as `html_edit`, with `<a href>` and
/// `</a>`, which arrive as separate events, paired through `hrefs`.
fn inline_html_edit(tag: &str, hrefs: &mut Vec<String>) -> String {
    static OPEN_A: OnceLock<Regex> = OnceLock::new();
    let open_a = OPEN_A.get_or_init(|| Regex::new(r#"(?is)^<a\b[^>]*?\bhref\s*=\s*["']([^"']*)["'][^>]*>$"#).expect("valid regex"));
    if let Some(c) = open_a.captures(tag) {
        hrefs.push(c[1].to_string());
        return "[".into();
    }
    if tag.trim().eq_ignore_ascii_case("</a>") {
        return hrefs.pop().map_or_else(String::new, |href| format!("]({})", link_dest(&href)));
    }
    html_edit(tag)
}

/// Text with its bare URLs, and with `repo` its references, linked.
fn text_edit(text: &str, repo: Option<&str>) -> String {
    static REFS: OnceLock<Regex> = OnceLock::new();
    let refs = REFS.get_or_init(|| {
        Regex::new(concat!(
            r"(?P<url>https?://[^\s<>\[\]]+)",
            r"|(?P<xref>[A-Za-z0-9][\w.-]*/[\w.-]+#\d+)\b",
            r"|(?P<issue>#\d+)\b",
            r"|(?P<user>@[A-Za-z0-9][A-Za-z0-9-]{0,38})\b",
            r"|(?P<sha>\b[0-9a-f]{7,40})\b",
        ))
        .expect("valid regex")
    });
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for c in refs.captures_iter(text) {
        let m = c.get(0).expect("whole match");
        // Inside a word (an email, `a#1`, hex in an identifier), or an entity (`&#123;`).
        let prev = text[..m.start()].chars().next_back();
        if prev.is_some_and(|p| p.is_alphanumeric() || matches!(p, '_' | '&' | '/' | '@' | '#')) {
            continue;
        }
        let link = if let Some(url) = c.name("url") {
            // Sentence punctuation and an unbalanced `)` stay outside the link.
            let mut u = url.as_str();
            loop {
                let trimmed = u.trim_end_matches(['.', ',', ':', ';', '!', '?', '\'', '"', '*', '_', '~']);
                let trimmed = if trimmed.ends_with(')') && trimmed.matches('(').count() < trimmed.matches(')').count() { &trimmed[..trimmed.len() - 1] } else { trimmed };
                if trimmed.len() == u.len() {
                    break;
                }
                u = trimmed;
            }
            out.push_str(&text[at..m.start()]);
            out.push_str(&format!("<{u}>"));
            at = m.start() + u.len();
            continue;
        } else if let Some(repo) = repo {
            if let Some(x) = c.name("xref") {
                let (r, n) = x.as_str().split_once('#').expect("xref has #");
                format!("[{}](https://github.com/{r}/issues/{n})", x.as_str())
            } else if let Some(i) = c.name("issue") {
                format!("[{}](https://github.com/{repo}/issues/{})", i.as_str(), &i.as_str()[1..])
            } else if let Some(u) = c.name("user") {
                format!("[{}](https://github.com/{})", u.as_str(), &u.as_str()[1..])
            } else if let Some(s) = c.name("sha").filter(|s| s.as_str().bytes().any(|b| b.is_ascii_digit()) && s.as_str().bytes().any(|b| b.is_ascii_alphabetic())) {
                format!("[`{}`](https://github.com/{repo}/commit/{})", &s.as_str()[..7], s.as_str())
            } else {
                continue;
            }
        } else {
            continue;
        };
        out.push_str(&text[at..m.start()]);
        out.push_str(&link);
        at = m.end();
    }
    out.push_str(&text[at..]);
    out
}

/// Draws `text` at `at`, `width` wide, clipped to `clip`; returns its height.
/// Its widgets are disabled unless ⌘ is held, so a plain click lands on the
/// row (cursor, ⌘M) and keys stay with vim; ⌘-click opens a link (see
/// `drain_open_urls`). `paint: false` lays it out invisibly, to measure.
#[allow(clippy::too_many_arguments)]
pub fn draw(ui: &mut Ui, text: &Text, at: Pos2, width: f32, clip: Rect, id: egui::Id, paint: bool) -> f32 {
    let segments = segments(&github(text.src, text.repo));
    let cache = ui.data_mut(|d| d.get_temp_mut_or_default::<Arc<Mutex<CommonMarkCache>>>(egui::Id::new("commonmark")).clone());
    let max = Rect::from_min_size(at, Vec2::new(width.max(80.0), f32::INFINITY));
    let mut builder = egui::UiBuilder::new().max_rect(max).layout(egui::Layout::top_down(egui::Align::Min));
    // The measuring copy gets its own ids, so it never clashes with the painted one.
    builder = if paint { builder.id_salt(id) } else { builder.id_salt(id.with("measure")).invisible() };
    let mut child = ui.new_child(builder);
    child.set_clip_rect(clip.intersect(ui.clip_rect()));
    child.set_style(markdown_style(ui.style(), text));
    if !(paint && ui.input(|i| i.modifiers.command)) {
        child.disable();
    }
    for (i, segment) in segments.iter().enumerate() {
        match segment {
            Segment::Prose(md) => {
                child.push_id(i, |ui| CommonMarkViewer::new().show(ui, &mut cache.lock().unwrap_or_else(|e| e.into_inner()), md));
            }
            Segment::Code { lang, text: code, suggestion } => {
                let spans = if text.dim { None } else { code_spans(&child, lang, text.path, *suggestion, code) };
                code_panel(&mut child, code, spans.as_deref(), *suggestion, text);
            }
        }
    }
    child.min_rect().height()
}

/// A code segment: a "Suggested change" line over a suggestion, then the
/// code in monospace on a panel, colored by `spans`, wrapped at the width.
fn code_panel(ui: &mut Ui, code: &str, spans: Option<&Spans>, suggestion: bool, text: &Text) {
    let base = if text.dim { theme::SUBTLE } else { PROSE };
    if suggestion {
        ui.label(egui::RichText::new("Suggested change").font(theme::ui_font(text.size - 1.5)).color(theme::SUBTLE));
    }
    let font = FontId::monospace(text.size * 0.88);
    let mut job = LayoutJob::default();
    job.wrap.max_width = (ui.available_width() - CODE_PAD * 2.0).max(40.0);
    let format = |color| TextFormat { font_id: font.clone(), color, ..Default::default() };
    for (i, line) in code.lines().enumerate() {
        if i > 0 {
            job.append("\n", 0.0, format(base));
        }
        let line = expand_tabs(line);
        let empty = Vec::new();
        let line_spans = spans.and_then(|s| s.get(i)).unwrap_or(&empty);
        let mut cuts = vec![0, line.len()];
        for (r, _) in line_spans {
            cuts.extend([r.start.min(line.len()), r.end.min(line.len())]);
        }
        cuts.sort_unstable();
        cuts.dedup();
        for w in cuts.windows(2) {
            let (a, b) = (w[0], w[1]);
            if a >= b || !line.is_char_boundary(a) || !line.is_char_boundary(b) {
                continue;
            }
            let color = line_spans.iter().rev().find(|(r, _)| r.start <= a && r.end >= b).map_or(base, |(_, c)| *c);
            job.append(&line[a..b], 0.0, format(color));
        }
    }
    if job.text.is_empty() {
        job.append("", 0.0, format(base));
    }
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let size = Vec2::new(ui.available_width(), galley.size().y + CODE_PAD * 2.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, 6.0, text.code_bg);
        ui.painter().galley(rect.min + Vec2::splat(CODE_PAD), galley, base);
    }
}

type SpanCache = Arc<Mutex<HashMap<u64, Option<Arc<Spans>>>>>;

/// Highlight spans for a code segment, cached in egui's temp data by
/// language and text: a suggestion is highlighted as `path`, other code by
/// its fence's language; no language, no colors (as on GitHub).
fn code_spans(ui: &Ui, lang: &str, path: Option<&str>, suggestion: bool, code: &str) -> Option<Arc<Spans>> {
    let path = path.filter(|_| suggestion);
    if path.is_none() && (lang.is_empty() || suggestion) {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (lang, path, code).hash(&mut h);
    let key = h.finish();
    let cache = ui.data_mut(|d| d.get_temp_mut_or_default::<SpanCache>(egui::Id::new("markdown spans")).clone());
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(spans) = cache.get(&key) {
        return spans.clone();
    }
    if cache.len() >= SPANS_MAX {
        cache.clear();
    }
    let spans = match path {
        Some(p) => highlight::highlight(p, code),
        None => highlight::highlight_lang(lang, code),
    }
    .map(Arc::new);
    cache.insert(key, spans.clone());
    spans
}

/// Fonts and colors for Markdown at `text.size`, faded when `text.dim`.
fn markdown_style(base: &egui::Style, text: &Text) -> egui::Style {
    use egui::TextStyle;
    let mut s = base.clone();
    let body = text.size;
    s.text_styles = [
        (TextStyle::Small, theme::ui_font(body * 0.85)),
        (TextStyle::Body, theme::ui_font(body)),
        (TextStyle::Button, theme::ui_font(body)),
        (TextStyle::Heading, theme::ui_font_bold(body * 2.0)),
        (TextStyle::Monospace, FontId::monospace(body * 0.88)),
    ]
    .into();
    let (prose, strong) = if text.dim { (theme::SUBTLE, theme::MUTED) } else { (PROSE, theme::TEXT) };
    let v = &mut s.visuals;
    // Text colors come from the widget visuals: plain text dimmer than
    // bold and headings (egui draws `strong` as a brighter color).
    v.override_text_color = None;
    v.widgets.noninteractive.fg_stroke.color = prose;
    v.widgets.inactive.fg_stroke.color = prose;
    v.widgets.active.fg_stroke.color = strong;
    v.code_bg_color = text.code_bg;
    v.extreme_bg_color = text.code_bg;
    // Disabled only so it takes no input; drawn at full strength.
    v.disabled_alpha = 1.0;
    s.spacing.item_spacing.y = 5.0;
    // With ⌘ held a click opens a link instead of selecting text.
    s.interaction.selectable_labels = false;
    s
}

/// The http(s) URLs links asked egui to open this frame, taken out of its
/// output: eframe here opens none (it is built without `links`), so the
/// caller opens them with `Link::Url`. Other links (relative, `#heading`)
/// are dropped.
pub fn drain_open_urls(ctx: &egui::Context) -> Vec<String> {
    let mut urls = Vec::new();
    ctx.output_mut(|o| {
        o.commands.retain(|c| match c {
            egui::OutputCommand::OpenUrl(open) => {
                if open.url.starts_with("https://") || open.url.starts_with("http://") {
                    urls.push(open.url.clone());
                }
                false
            }
            _ => true,
        })
    });
    urls
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(lang: &str, text: &str) -> Segment {
        Segment::Code { lang: lang.into(), text: text.into(), suggestion: lang == "suggestion" }
    }

    fn prose(text: &str) -> Segment {
        Segment::Prose(text.into())
    }

    #[test]
    fn segments_cut_top_level_code() {
        let src = "Both are colors:\n\n```rust\nconst A: u8 = 1;\n  nested();\n```\n\nSo use `A`.\n";
        assert_eq!(segments(src), vec![prose("Both are colors:"), code("rust", "const A: u8 = 1;\n  nested();"), prose("So use `A`.")]);
        // Code in a list item or a quote is the renderer's.
        let nested = "- item\n\n  ```\n  x\n  ```\n\n> ```\n> y\n> ```";
        assert_eq!(segments(nested), vec![prose(nested)]);
        assert_eq!(segments("text\n\n    indented\n"), vec![prose("text"), code("", "indented")]);
        assert_eq!(segments("```suggestion\nfixed();\n```"), vec![code("suggestion", "fixed();")]);
        assert_eq!(segments("a\n```py\nunclosed"), vec![prose("a"), code("py", "unclosed")]);
        assert_eq!(segments("~~~\n```inner\n~~~"), vec![code("", "```inner")], "a fence line inside code doesn't close it");
        assert_eq!(segments("plain\n\ntext"), vec![prose("plain\n\ntext")]);
        // Link definitions go along with each prose segment that may use them.
        let refs = segments("see [x][r]\n\n```\nc\n```\n\nand [y][r]\n\n[r]: https://x.y \"T\"");
        assert_eq!(refs[0], prose("see [x][r]\n\n[r]: https://x.y \"T\""));
        assert!(matches!(&refs[2], Segment::Prose(p) if p.starts_with("and [y][r]") && p.ends_with("[r]: https://x.y \"T\"")));
    }

    #[test]
    fn github_cleans_html() {
        let body = "Fixes it.\r\n<!-- template: say why -->\r\n\r\n\r\n\r\n<details><summary> Logs </summary>\n\n```\nok\n```\n</details>\nline<br>next\n![shot](https://x/y.png) <img width=\"10\" src=\"https://x/z.png\">";
        let got = github(body, None);
        assert!(!got.contains("<!--") && !got.contains("details>") && !got.contains("<img"), "{got}");
        assert!(got.contains("**▸ Logs**") && got.contains("```\nok\n```"), "{got}");
        assert!(got.contains("line  \nnext"), "{got}");
        assert!(got.contains("[image: shot](https://x/y.png) [image](https://x/z.png)"), "{got}");
        // A dropped comment still ends the paragraph before it.
        assert_eq!(github("Fixes it.\n<!-- hint -->\nMore", None), "Fixes it.\n\nMore");
        assert_eq!(github("a <b>bold</b>, <i>it</i>, <kbd>⌘</kbd>, H<sub>2</sub>O", None), "a **bold**, *it*, `⌘`, H2O");
        assert_eq!(github("go <a href=\"https://x.y\">there</a>", None), "go [there](https://x.y)");
        assert_eq!(github("<p align=\"center\">\n<a href=\"https://x\">home</a>\n</p>\n", None).trim(), "[home](https://x)");
        // An unclosed <details> is dropped and the rest stays.
        assert_eq!(github("<details>\n\nrest", None).trim(), "rest");
    }

    #[test]
    fn github_leaves_code_alone() {
        let src = "`<b>x</b> https://x #12`\n\n```\n<!-- keep --> https://x.y #12 @al\n```\n\n    <br> https://z";
        assert_eq!(github(src, Some("o/r")), src);
    }

    #[test]
    fn github_autolinks_urls() {
        assert_eq!(github("see https://x.y/a.", None), "see <https://x.y/a>.");
        assert_eq!(github("(https://x/y)", None), "(<https://x/y>)");
        assert_eq!(github("https://x/a_(b)", None), "<https://x/a_(b)>");
        assert_eq!(github("https://x.com/a_b_c d", None), "<https://x.com/a_b_c> d", "split text events are joined first");
        for linked in ["[t](https://x)", "<https://x>"] {
            assert_eq!(github(linked, None), linked);
        }
    }

    #[test]
    fn github_links_references() {
        let r = Some("o/r");
        assert_eq!(github("fixes #12", r), "fixes [#12](https://github.com/o/r/issues/12)");
        assert_eq!(github("see a/b#3", r), "see [a/b#3](https://github.com/a/b/issues/3)");
        assert_eq!(github("cc @al-x.", r), "cc [@al-x](https://github.com/al-x).");
        assert_eq!(github("in deadbee1", r), "in [`deadbee`](https://github.com/o/r/commit/deadbee1)");
        for plain in ["me@al.com", "a#1", "deadbeef", "1234567", "[@al](https://u)", "&#123;"] {
            assert_eq!(github(plain, r), plain);
        }
        assert_eq!(github("https://x/commit/deadbee1", r), "<https://x/commit/deadbee1>", "a SHA in a URL stays the URL's");
        assert_eq!(github("fixes #12 @al", None), "fixes #12 @al", "no repo, no references");
        assert_eq!(github("# 12", r), "# 12", "a heading");
    }

    #[test]
    fn github_is_idempotent() {
        for src in [
            "see https://x.y and https://x/a_(b).",
            "fixes #12, a/b#3, @al, deadbee1",
            "<details><summary>S</summary>\n\nbody\n</details>",
            "![alt](https://x/y.png) and <img src=\"https://x/z.png\">",
            "go <a href=\"https://x.y\">there</a>",
        ] {
            let once = github(src, Some("o/r"));
            assert_eq!(github(&once, Some("o/r")), once, "{src}");
        }
    }

    /// One frame with the kitchen's fonts, run twice (fonts apply at a pass's start).
    fn frame(f: impl FnMut(&mut Ui)) {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut f = f;
        ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
        ctx.run_ui(egui::RawInput::default(), |ui| f(ui)).textures_delta.clear();
    }

    #[test]
    fn draw_measures_what_it_paints() {
        frame(|ui| {
            let clip = ui.max_rect();
            let at = clip.min;
            let text = |src| Text { src, repo: None, path: None, size: 13.0, dim: false, code_bg: CODE_BG };
            let body = "Some **bold** text and a [link](https://x).\n\n| a | b |\n|---|---|\n| 1 | 2 |";
            let measured = draw(ui, &text(body), at, 400.0, clip, egui::Id::new("a"), false);
            let painted = draw(ui, &text(body), at, 400.0, clip, egui::Id::new("a"), true);
            assert!(measured > 20.0);
            assert_eq!(measured, painted);
            let with_code = format!("{body}\n\n```rust\nfn main() {{}}\n```");
            assert!(draw(ui, &text(&with_code), at, 400.0, clip, egui::Id::new("b"), false) > measured + 10.0);
        });
    }

    #[test]
    fn drain_open_urls_keeps_web_links() {
        frame(|ui| {
            for url in ["https://x.y", "docs/a.md", "#x"] {
                ui.ctx().open_url(egui::OpenUrl::same_tab(url));
            }
            assert_eq!(drain_open_urls(ui.ctx()), vec!["https://x.y".to_string()]);
            assert!(ui.ctx().output(|o| o.commands.is_empty()));
        });
    }
}
