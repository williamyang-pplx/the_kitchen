//! Markdown as the viewer draws it, in cards (review comments, ⌘⇧A
//! answers, plan comments) and in the markdown plugin's rendering of a
//! file. egui_commonmark lays out and draws all of it, code included, in
//! one pass; `github` first turns GitHub's flavour into what it draws.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

use egui::{Color32, FontId, Pos2, Rect, Ui, Vec2};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use regex::{Captures, Regex};

use super::highlight;
use crate::theme;

/// Background of code in rendered Markdown rows, fenced and inline.
pub const CODE_BG: Color32 = Color32::from_rgb(0x24, 0x28, 0x3b);
/// Body text.
pub const PROSE: Color32 = Color32::from_rgb(0xc0, 0xca, 0xf5);
/// Bold text and headings: egui_commonmark marks them by color only, so
/// they need to stand out from `PROSE`.
const STRONG: Color32 = Color32::from_rgb(0xf0, 0xf3, 0xff);
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

/// `src` with each fence's language as the file extension
/// egui_commonmark finds its grammar by (`python` → `py`), and a
/// ```` ```suggestion ```` highlighted as `path` under a "Suggested change"
/// line, as GitHub shows one.
fn fences(src: &str, path: Option<&str>) -> String {
    let path_ext = path.and_then(|p| std::path::Path::new(p).extension()).map(|e| e.to_string_lossy().into_owned());
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for (event, range) in Parser::new_ext(src, OPTIONS).into_offset_iter() {
        let Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) = event else { continue };
        let Some(lang) = info.split_whitespace().next() else { continue };
        let suggestion = lang == "suggestion";
        let ext = if suggestion { path_ext.clone().unwrap_or_default() } else { highlight::fence_extension(lang).unwrap_or_else(|| lang.to_string()) };
        // The info string sits on the fence's first line, after the marks.
        let line_end = src[range.start..].find('\n').map_or(src.len(), |i| range.start + i);
        let Some(at) = src[range.start..line_end].find(lang).map(|i| range.start + i) else { continue };
        if ext != lang {
            edits.push((at..at + lang.len(), ext));
        }
        let line_start = src[..range.start].rfind('\n').map_or(0, |i| i + 1);
        if suggestion && src[line_start..range.start].trim().is_empty() {
            edits.push((range.start..range.start, "*Suggested change*\n\n".to_string() + &src[line_start..range.start]));
        }
    }
    edits.sort_by_key(|(r, _)| r.start);
    let mut out = String::with_capacity(src.len() + 32);
    let mut last = 0;
    for (r, new) in edits {
        out.push_str(&src[last..r.start]);
        out.push_str(&new);
        last = r.end;
    }
    out.push_str(&src[last..]);
    out
}

/// egui_commonmark's cache (its grammars and themes), with the names of
/// the viewer themes added to it, shared by every draw.
#[derive(Default)]
struct Renderer {
    cache: CommonMarkCache,
    themes: HashSet<String>,
}

/// Draws `text` at `at`, `width` wide, clipped to `clip`; returns its height.
/// egui_commonmark lays it all out. Its widgets are disabled unless ⌘ is
/// held, so a plain click lands on the row (cursor, ⌘M) and keys stay with
/// vim; ⌘-click opens a link (see `drain_open_urls`). `paint: false` lays it
/// out invisibly, to measure.
pub fn draw(ui: &mut Ui, text: &Text, at: Pos2, width: f32, clip: Rect, id: egui::Id, paint: bool) -> f32 {
    let src = fences(&github(text.src, text.repo), text.path);
    let renderer = ui.data_mut(|d| d.get_temp_mut_or_default::<Arc<Mutex<Renderer>>>(egui::Id::new("markdown renderer")).clone());
    let mut renderer = renderer.lock().unwrap_or_else(|e| e.into_inner());
    // Code takes the viewer's colors, on the background it sits on.
    let bg = text.code_bg;
    let theme_name = format!("kitchen-{:02x}{:02x}{:02x}", bg.r(), bg.g(), bg.b());
    if renderer.themes.insert(theme_name.clone()) {
        if let Err(e) = renderer.cache.add_syntax_theme_from_bytes(theme_name.clone(), highlight::tm_theme(&theme_name, bg).as_bytes()) {
            log::warn!("markdown code theme: {e}");
        }
    }
    let max = Rect::from_min_size(at, Vec2::new(width.max(80.0), f32::INFINITY));
    let mut builder = egui::UiBuilder::new().max_rect(max).layout(egui::Layout::top_down(egui::Align::Min));
    // The measuring copy gets its own ids, so it never clashes with the painted one.
    builder = if paint { builder.id_salt(id) } else { builder.id_salt(id.with("measure")).invisible() };
    let mut child = ui.new_child(builder);
    child.set_clip_rect(clip.intersect(ui.clip_rect()));
    child.set_style(markdown_style(ui, text));
    if !(paint && ui.input(|i| i.modifiers.command)) {
        child.disable();
    }
    CommonMarkViewer::new().syntax_theme_dark(&theme_name).syntax_theme_light(&theme_name).show(&mut child, &mut renderer.cache, &src);
    child.min_rect().height()
}

/// Fonts and colors for Markdown at `text.size`, faded when `text.dim`.
fn markdown_style(ui: &Ui, text: &Text) -> egui::Style {
    use egui::TextStyle;
    let mut s = ui.style().as_ref().clone();
    let body = text.size;
    s.text_styles = [
        (TextStyle::Small, theme::ui_font(body * 0.85)),
        (TextStyle::Body, theme::ui_font(body)),
        (TextStyle::Button, theme::ui_font(body)),
        (TextStyle::Heading, theme::ui_font_bold(body * 2.0)),
        (TextStyle::Monospace, FontId::monospace(body * 0.88)),
    ]
    .into();
    let (prose, strong) = if text.dim { (theme::SUBTLE, theme::MUTED) } else { (PROSE, STRONG) };
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
    // egui_commonmark lays a line out as a wrapped row of widgets, its list
    // markers centered in the row: a row as tall as the text keeps a marker
    // level with the text beside it (egui's default is a button's height).
    s.spacing.interact_size.y = ui.fonts_mut(|f| f.row_height(&theme::ui_font(body)));
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

    #[test]
    fn fences_name_grammars_by_extension() {
        assert_eq!(fences("```python\nx = 1\n```", None), "```py\nx = 1\n```");
        assert_eq!(fences("~~~rust title\nfn f() {}\n~~~", None), "~~~rs title\nfn f() {}\n~~~");
        assert_eq!(fences("```\nplain\n```", None), "```\nplain\n```", "no language: plain, as on GitHub");
        assert_eq!(fences("a\n\n```suggestion\nfix()\n```", Some("src/a.py")), "a\n\n*Suggested change*\n\n```py\nfix()\n```");
        assert_eq!(fences("`python` inline", None), "`python` inline");
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
