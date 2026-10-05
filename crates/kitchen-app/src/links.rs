//! Links in terminal text: what a ⌘-click on a span of a line opens.
//! Rules, first match covering the pointer wins: plugin rules
//! (`kitchen.link`), URLs, file paths. OSC 8 hyperlinks are checked before all
//! of these by the terminal (they live on the cells, not in the text).

use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

use crate::plugins::LinkEntry;

/// A file reference as printed: path plus optional 1-based line. A trailing
/// `:col` is matched (so it is not part of the path) but dropped: the viewer
/// opens at a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    /// As printed: absolute, `~/…`, or relative (to the tab's worktree).
    pub path: String,
    pub line: Option<usize>,
}

/// What a click opens.
#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    File(FileRef),
    Url(String),
    /// A plugin rule matched: run `command` (`<plugin>.<name>`) with the match.
    Plugin { command: String, text: String, groups: Vec<Option<String>> },
}

/// A link and the byte range of the line it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkMatch {
    pub range: Range<usize>,
    pub link: Link,
}

/// http(s) URLs; trailing sentence punctuation is cut by `trim_url`.
static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"https?://[^\s<>"'`]+"#).unwrap());

/// Runs of path characters, plus an optional `:line[:col]` or `#Lline`.
/// `is_path` decides which runs are paths.
static FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:~/|\.{1,2}/|/)?[\w.@+-]+(?:/[\w.@+-]+)*(?::(\d+)(?::\d+)?|#L(\d+))?").unwrap());

/// The link covering byte `at` of `line`, if any.
pub fn link_at(line: &str, at: usize, plugin_rules: &[LinkEntry]) -> Option<LinkMatch> {
    let covers = |r: &Range<usize>| r.start <= at && at < r.end;
    for rule in plugin_rules {
        let Some(caps) = rule.regex.captures_iter(line).find(|c| covers(&c.get(0).unwrap().range())) else { continue };
        let m = caps.get(0).unwrap();
        let groups = caps.iter().skip(1).map(|g| g.map(|g| g.as_str().to_string())).collect();
        return Some(LinkMatch { range: m.range(), link: Link::Plugin { command: rule.command.clone(), text: m.as_str().to_string(), groups } });
    }
    if let Some(m) = URL.find_iter(line).find(|m| covers(&m.range())) {
        let url = trim_url(m.as_str());
        let range = m.start()..m.start() + url.len();
        return covers(&range).then(|| LinkMatch { range, link: Link::Url(url.to_string()) });
    }
    let caps = FILE.captures_iter(line).find(|c| covers(&c.get(0).unwrap().range()))?;
    let m = caps.get(0).unwrap();
    let line_no = caps.get(1).or(caps.get(2)).and_then(|n| n.as_str().parse().ok());
    let suffix_at = caps.get(1).or(caps.get(2)).map(|n| n.start() - if caps.get(2).is_some() { 2 } else { 1 }).unwrap_or(m.end());
    let path = &line[m.start()..suffix_at];
    // A trailing `.` ends the sentence, not the file name.
    let path = if line_no.is_none() { path.trim_end_matches('.') } else { path };
    if !is_path(path) {
        return None;
    }
    let end = if line_no.is_some() { m.end() } else { m.start() + path.len() };
    let range = m.start()..end;
    covers(&range).then(|| LinkMatch { range, link: Link::File(FileRef { path: path.to_string(), line: line_no }) })
}

/// A run that names a file: it has a `/`, or a `.ext` starting with a
/// letter (so `1.2.3` is not a path) on a name of 4+ characters (so `e.g`
/// and `i.e` are not either).
fn is_path(s: &str) -> bool {
    if s.contains('/') {
        return s.trim_matches(|c| c == '/' || c == '.' || c == '~').len() > 0;
    }
    match s.rsplit_once('.') {
        Some((base, ext)) => !base.is_empty() && ext.starts_with(|c: char| c.is_ascii_alphabetic()) && s.len() >= 4,
        None => false,
    }
}

/// `url` without trailing sentence punctuation; a `)` stays when it closes a
/// `(` inside the URL (Wikipedia-style links).
fn trim_url(url: &str) -> &str {
    let mut u = url;
    loop {
        let Some(last) = u.chars().last() else { return u };
        let unbalanced = last == ')' && u.matches('(').count() < u.matches(')').count();
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | ']' | '\'' | '"') || unbalanced {
            u = &u[..u.len() - last.len_utf8()];
        } else {
            return u;
        }
    }
}

/// A link from an OSC 8 hyperlink's URI: `file://[host]/p` → File, else Url.
pub fn from_uri(uri: &str) -> Link {
    match uri.strip_prefix("file://") {
        Some(rest) => {
            let path = rest.find('/').map(|i| &rest[i..]).unwrap_or(rest);
            Link::File(FileRef { path: percent_decode(path), line: None })
        }
        None => Link::Url(uri.to_string()),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%').then(|| s.get(i + 1..i + 3)).flatten().and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, line: Option<usize>) -> Option<Link> {
        Some(Link::File(FileRef { path: path.into(), line }))
    }

    fn url(u: &str) -> Option<Link> {
        Some(Link::Url(u.into()))
    }

    #[test]
    fn links_under_the_pointer() {
        let cases: &[(&str, usize, Option<Link>)] = &[
            ("see aster/foo.py:42 now", 6, file("aster/foo.py", Some(42))),
            ("see aster/foo.py:42 now", 17, file("aster/foo.py", Some(42))),
            ("foo.py:42:7", 2, file("foo.py", Some(42))),
            ("Read(/home/dev/agi/a.rs)", 10, file("/home/dev/agi/a.rs", None)),
            ("~/x.lua", 0, file("~/x.lua", None)),
            ("./src/a.rs#L9", 3, file("./src/a.rs", Some(9))),
            ("(see foo.py).", 7, file("foo.py", None)),
            ("ends in foo.py.", 9, file("foo.py", None)),
            ("version 1.2.3", 9, None),
            ("e.g. this", 1, None),
            ("plain words", 2, None),
            ("open https://example.com/a?b=1). now", 10, url("https://example.com/a?b=1")),
            ("https://en.wikipedia.org/wiki/Rust_(language)", 5, url("https://en.wikipedia.org/wiki/Rust_(language)")),
            ("https://example.com/x.py", 22, url("https://example.com/x.py")),
            ("see aster/foo.py:42 now", 1, None),
        ];
        for (line, at, want) in cases {
            assert_eq!(link_at(line, *at, &[]).map(|m| m.link), *want, "line={line:?} at={at}");
        }
    }

    #[test]
    fn match_range_covers_the_suffix() {
        let m = link_at("x aster/foo.py:42 y", 3, &[]).unwrap();
        assert_eq!(m.range, 2..17);
        let m = link_at("ends in foo.py.", 9, &[]).unwrap();
        assert_eq!(m.range, 8..14);
    }

    #[test]
    fn plugin_rules_win() {
        let rules = [LinkEntry { plugin: "t".into(), regex: Regex::new(r"AI-(\d+)").unwrap(), command: "t.open".into() }];
        let m = link_at("fix AI-9586 soon", 6, &rules).unwrap();
        assert_eq!(m.range, 4..11);
        assert_eq!(m.link, Link::Plugin { command: "t.open".into(), text: "AI-9586".into(), groups: vec![Some("9586".into())] });
        let rules = [LinkEntry { plugin: "t".into(), regex: Regex::new(r"https://linear\.app/\S+").unwrap(), command: "t.l".into() }];
        assert!(matches!(link_at("https://linear.app/x", 3, &rules).unwrap().link, Link::Plugin { .. }));
        assert_eq!(link_at("https://example.com", 3, &rules).unwrap().link, Link::Url("https://example.com".into()));
    }

    #[test]
    fn uris() {
        assert_eq!(from_uri("file://host/tmp/a%20b.rs"), Link::File(FileRef { path: "/tmp/a b.rs".into(), line: None }));
        assert_eq!(from_uri("file:///tmp/x"), Link::File(FileRef { path: "/tmp/x".into(), line: None }));
        assert_eq!(from_uri("https://x.dev"), Link::Url("https://x.dev".into()));
    }
}
