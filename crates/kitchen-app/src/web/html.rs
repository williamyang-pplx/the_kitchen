//! Markdown → HTML as GitHub renders it. With a repo, GitHub's own renderer
//! (`gh api markdown`, gfm, the repo as context) makes it: byte for byte what
//! github.com shows, `#12`, `@al` and SHAs linked. Until that arrives, and
//! when it can't (no repo, offline, no gh), comrak renders it here, with the
//! viewer's syntax colors, sanitized by ammonia.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

use crate::viewer::doc::expand_tabs;
use crate::viewer::highlight;

/// GitHub renders this many texts at once.
const FETCHERS: usize = 4;
/// Bumped when the cached HTML's shape changes.
const CACHE_VERSION: u32 = 1;

/// The HTML for `markdown`: GitHub's when it has rendered it (cached in
/// memory and on disk), else comrak's, and with a `repo` GitHub's is asked
/// for in the background (a repaint follows when it lands).
pub fn get(markdown: &str, repo: Option<&str>) -> Arc<str> {
    let key = key(markdown, repo);
    let state = state();
    if let Some(html) = state.lock().unwrap_or_else(|e| e.into_inner()).html.get(&key) {
        return html.clone();
    }
    // The first time a text is shown: GitHub's HTML from the disk cache, or ask GitHub.
    if let Some(repo) = repo.filter(|_| state.lock().unwrap_or_else(|e| e.into_inner()).asked.insert(key)) {
        if let Some(html) = std::fs::read_to_string(cache_path(key)).ok().map(Arc::<str>::from) {
            state.lock().unwrap_or_else(|e| e.into_inner()).html.insert(key, html.clone());
            return html;
        }
        let _ = fetcher().send(Fetch { key, markdown: markdown.to_string(), repo: repo.to_string() });
    }
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    s.local.entry(key).or_insert_with(|| local(markdown).into()).clone()
}

/// Where GitHub-rendered HTML is kept between runs.
fn cache_path(key: u64) -> std::path::PathBuf {
    kitchen_proto::state_dir().join("cache").join("markdown").join(format!("{key:016x}.html"))
}

fn key(markdown: &str, repo: Option<&str>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (CACHE_VERSION, markdown, repo).hash(&mut h);
    h.finish()
}

#[derive(Default)]
struct State {
    /// GitHub's HTML by key.
    html: HashMap<u64, Arc<str>>,
    /// comrak's HTML by key, shown until GitHub's arrives.
    local: HashMap<u64, Arc<str>>,
    /// Keys GitHub was asked for (done, failed or on the way).
    asked: HashSet<u64>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// The egui context to repaint when GitHub's HTML lands.
pub fn set_repaint(ctx: &egui::Context) {
    let _ = REPAINT.set(ctx.clone());
}
static REPAINT: OnceLock<egui::Context> = OnceLock::new();

struct Fetch {
    key: u64,
    markdown: String,
    repo: String,
}

/// The queue `FETCHERS` threads take texts to GitHub from.
fn fetcher() -> &'static Sender<Fetch> {
    static TX: OnceLock<Sender<Fetch>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = channel::<Fetch>();
        let rx = Arc::new(Mutex::new(rx));
        for _ in 0..FETCHERS {
            let rx: Arc<Mutex<Receiver<Fetch>>> = rx.clone();
            std::thread::spawn(move || {
                loop {
                    let Ok(job) = rx.lock().unwrap_or_else(|e| e.into_inner()).recv() else { return };
                    match github(&job.markdown, &job.repo) {
                        Ok(html) => {
                            let path = cache_path(job.key);
                            if let Some(dir) = path.parent() {
                                let _ = std::fs::create_dir_all(dir);
                            }
                            let _ = std::fs::write(&path, &html);
                            state().lock().unwrap_or_else(|e| e.into_inner()).html.insert(job.key, html.into());
                            if let Some(ctx) = REPAINT.get() {
                                ctx.request_repaint();
                            }
                        }
                        Err(e) => log::debug!(target: "kitchen::web", "GitHub markdown for {}: {e:#}", job.repo),
                    }
                }
            });
        }
        tx
    })
}

/// GitHub's rendering of `markdown` for `repo` (`POST /markdown`, gfm).
fn github(markdown: &str, repo: &str) -> anyhow::Result<String> {
    let body = serde_json::json!({ "text": markdown, "mode": "gfm", "context": repo }).to_string();
    let mut child = std::process::Command::new("gh")
        .args(["api", "markdown", "--input", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    child.stdin.take().expect("piped stdin").write_all(body.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!("gh api markdown: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// comrak's GitHub-flavored HTML for `markdown`, code colored by the
/// viewer's highlighter, sanitized (its text may come from anywhere).
pub fn local(markdown: &str) -> String {
    let mut options = comrak::Options::default();
    let e = &mut options.extension;
    (e.table, e.tasklist, e.strikethrough, e.autolink, e.footnotes, e.alerts) = (true, true, true, true, true, true);
    options.render.r#unsafe = true;
    options.render.tasklist_classes = true;
    let mut plugins = comrak::options::Plugins::default();
    plugins.render.codefence_syntax_highlighter = Some(&Highlighter);
    sanitize(&comrak::markdown_to_html_with_plugins(markdown, &options, &plugins))
}

/// What may stay of local HTML: GitHub's own allowances, plus the classes,
/// task-list checkboxes and code colors comrak writes.
fn sanitize(html: &str) -> String {
    let mut b = ammonia::Builder::default();
    b.add_tags(["input", "details", "summary"]).add_generic_attributes(["class", "align"]).add_tag_attributes("input", ["type", "checked", "disabled"]);
    b.add_tag_attributes("span", ["style"]).add_tag_attributes("img", ["src", "alt", "width", "height"]);
    b.clean(html).to_string()
}

/// comrak's fenced code in the viewer's colors.
struct Highlighter;

impl comrak::adapters::SyntaxHighlighterAdapter for Highlighter {
    fn write_highlighted(&self, out: &mut dyn std::fmt::Write, lang: Option<&str>, code: &str) -> std::fmt::Result {
        let spans = lang.filter(|l| !l.is_empty()).and_then(|l| highlight::highlight_lang(l, code));
        for (i, line) in code.split_inclusive('\n').enumerate() {
            let (text, newline) = line.strip_suffix('\n').map_or((line, ""), |t| (t, "\n"));
            let text = expand_tabs(text);
            let line_spans = spans.as_ref().and_then(|s| s.get(i)).map_or(&[][..], |s| &s[..]);
            let mut at = 0;
            for (range, color) in line_spans {
                let (start, end) = (range.start.min(text.len()), range.end.min(text.len()));
                if start < at || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                    continue;
                }
                write!(out, "{}", escape(&text[at..start]))?;
                write!(out, "<span style=\"color:#{:02x}{:02x}{:02x}\">{}</span>", color.r(), color.g(), color.b(), escape(&text[start..end]))?;
                at = end;
            }
            write!(out, "{}{newline}", escape(&text[at..]))?;
        }
        Ok(())
    }

    fn write_pre_tag<'s>(&self, out: &mut dyn std::fmt::Write, attributes: HashMap<&'static str, Cow<'s, str>>) -> std::fmt::Result {
        comrak::html::write_opening_tag(out, "pre", attributes)
    }

    fn write_code_tag<'s>(&self, out: &mut dyn std::fmt::Write, attributes: HashMap<&'static str, Cow<'s, str>>) -> std::fmt::Result {
        comrak::html::write_opening_tag(out, "code", attributes)
    }
}

/// `text` safe inside HTML.
pub fn escape(text: &str) -> Cow<'_, str> {
    if !text.contains(['<', '>', '&', '"']) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_renders_github_flavor() {
        let html = local("| a |\n|---|\n| 1 |\n\n- [x] done\n\n> [!NOTE]\n> hi\n\nsee https://x.y and ~~old~~");
        assert!(html.contains("<table>") && html.contains("<td>1</td>"), "{html}");
        assert!(html.contains("type=\"checkbox\"") && html.contains("checked") && html.contains("task-list-item"), "{html}");
        assert!(html.contains("markdown-alert-note"), "{html}");
        assert!(html.contains("<a href=\"https://x.y\"") && html.contains("<del>old</del>"), "{html}");
    }

    #[test]
    fn local_strips_scripts_and_keeps_details() {
        let html = local("<script>alert(1)</script>\n\n<details><summary>S</summary>\n\nbody\n</details>\n\n<img src=\"x.png\" onerror=\"alert(1)\">");
        assert!(!html.contains("<script") && !html.contains("onerror"), "{html}");
        assert!(html.contains("<details>") && html.contains("<summary>S</summary>"), "{html}");
    }

    #[test]
    fn local_colors_fenced_code() {
        let html = local("```rust\nfn main() {}\n```");
        assert!(html.contains("<span style=\"color:#"), "{html}");
        assert!(html.contains("main"), "{html}");
        // No language, no colors (as on GitHub); HTML in code is escaped.
        let plain = local("```\n<b>x</b>\n```");
        assert!(!plain.contains("<span style") && plain.contains("&lt;b&gt;"), "{plain}");
    }

    #[test]
    fn get_without_repo_is_local_and_stable() {
        let a = get("**hi**", None);
        assert_eq!(&*a, local("**hi**"));
        assert!(Arc::ptr_eq(&a, &get("**hi**", None)), "cached");
    }

    /// GitHub's renderer links issues against the repo (needs gh and the network).
    #[test]
    #[ignore]
    fn github_links_issues() {
        let html = github("Fixes #1", "cli/cli").unwrap();
        assert!(html.contains("https://github.com/cli/cli/"), "{html}");
    }
}
