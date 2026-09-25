//! GitHub through the `gh` binary on the mac (where `gh auth` lives), no
//! matter which host the workspace's checkout is on.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::host::Host;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrInfo {
    pub slug: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub state: String,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub is_draft: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThreadComment {
    pub id: String,
    pub database_id: i64,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewThread {
    pub id: String,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub path: String,
    /// Line in the current diff (None when outdated).
    pub line: Option<i64>,
    pub original_line: Option<i64>,
    pub start_line: Option<i64>,
    /// `LEFT` (base) or `RIGHT` (head).
    pub side: String,
    pub comments: Vec<ThreadComment>,
}

impl ReviewThread {
    /// Best line to anchor this thread on in the head version of the file.
    pub fn anchor_line(&self) -> Option<i64> {
        self.line.or(self.original_line)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    pub slug: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PostedComment {
    pub id: i64,
    pub url: String,
}

/// Where a new review comment attaches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommentAnchor {
    pub path: String,
    /// None = file-level comment.
    pub line: Option<i64>,
    pub start_line: Option<i64>,
    pub side: String,
}

pub struct Gh {
    host: Host,
}

impl Default for Gh {
    fn default() -> Self {
        Self { host: Host::Local }
    }
}

const PR_FIELDS: &str = "number,title,url,author,state,headRefName,headRefOid,baseRefName,isDraft";

impl Gh {
    fn gh(&self, args: &[&str]) -> Result<String> {
        let mut argv = vec!["gh"];
        argv.extend_from_slice(args);
        self.host.run_ok(&argv, None)
    }

    fn gh_json(&self, args: &[&str]) -> Result<Value> {
        let out = self.gh(args)?;
        serde_json::from_str(&out).with_context(|| format!("parse gh output: {}", truncate(&out, 200)))
    }

    pub fn viewer_login(&self) -> Result<String> {
        Ok(self.gh(&["api", "user", "--jq", ".login"])?.trim().to_string())
    }

    pub fn pr_view(&self, slug: &str, number: u64) -> Result<PrInfo> {
        let n = number.to_string();
        let v = self.gh_json(&["pr", "view", &n, "-R", slug, "--json", PR_FIELDS])?;
        pr_from_json(slug, &v)
    }

    /// Open PR whose head is `branch`, if any.
    pub fn pr_for_branch(&self, slug: &str, branch: &str) -> Result<Option<PrInfo>> {
        let v = self.gh_json(&["pr", "list", "-R", slug, "--head", branch, "--state", "open", "--json", PR_FIELDS])?;
        match v.as_array().and_then(|a| a.first()) {
            Some(p) => Ok(Some(pr_from_json(slug, p)?)),
            None => Ok(None),
        }
    }

    pub fn review_threads(&self, slug: &str, number: u64) -> Result<Vec<ReviewThread>> {
        let (owner, name) = split_slug(slug)?;
        let query = r#"
query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id isResolved isOutdated path line originalLine startLine diffSide
          comments(first: 50) {
            nodes { id databaseId body createdAt url author { login } }
          }
        }
      }
    }
  }
}"#;
        let mut threads = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let q = format!("query={query}");
            let o = format!("owner={owner}");
            let nm = format!("name={name}");
            let num = format!("number={number}");
            let mut args = vec!["api", "graphql", "-f", &q, "-f", &o, "-f", &nm, "-F", &num];
            let c;
            if let Some(cur) = &cursor {
                c = format!("cursor={cur}");
                args.push("-f");
                args.push(&c);
            }
            let v = self.gh_json(&args)?;
            let rt = &v["data"]["repository"]["pullRequest"]["reviewThreads"];
            for t in rt["nodes"].as_array().into_iter().flatten() {
                threads.push(thread_from_json(t));
            }
            if rt["pageInfo"]["hasNextPage"].as_bool() == Some(true) {
                cursor = rt["pageInfo"]["endCursor"].as_str().map(String::from);
            } else {
                break;
            }
        }
        Ok(threads)
    }

    /// Publish one review comment. Line comments must land on a line in the
    /// PR diff; GitHub rejects others, so the caller should fall back to
    /// [`CommentAnchor`] with `line: None` (file-level) for those.
    pub fn post_review_comment(
        &self,
        slug: &str,
        number: u64,
        commit_sha: &str,
        anchor: &CommentAnchor,
        body: &str,
    ) -> Result<PostedComment> {
        let endpoint = format!("repos/{slug}/pulls/{number}/comments");
        let mut fields = vec![
            format!("body={body}"),
            format!("commit_id={commit_sha}"),
            format!("path={}", anchor.path),
        ];
        let mut typed = Vec::new();
        match anchor.line {
            Some(line) => {
                typed.push(format!("line={line}"));
                fields.push(format!("side={}", anchor.side));
                if let Some(start) = anchor.start_line.filter(|s| *s < line) {
                    typed.push(format!("start_line={start}"));
                    fields.push(format!("start_side={}", anchor.side));
                }
            }
            None => fields.push("subject_type=file".into()),
        }
        let mut args = vec!["api", "-X", "POST", &endpoint];
        for f in &fields {
            args.push("-f");
            args.push(f);
        }
        for f in &typed {
            args.push("-F");
            args.push(f);
        }
        let v = self.gh_json(&args)?;
        posted_from_json(&v)
    }

    pub fn reply(&self, slug: &str, number: u64, comment_id: i64, body: &str) -> Result<PostedComment> {
        let endpoint = format!("repos/{slug}/pulls/{number}/comments/{comment_id}/replies");
        let b = format!("body={body}");
        let v = self.gh_json(&["api", "-X", "POST", &endpoint, "-f", &b])?;
        posted_from_json(&v)
    }

    pub fn set_thread_resolved(&self, thread_id: &str, resolved: bool) -> Result<()> {
        let mutation = if resolved { "resolveReviewThread" } else { "unresolveReviewThread" };
        let q = format!("query=mutation($id: ID!) {{ {mutation}(input: {{threadId: $id}}) {{ thread {{ id isResolved }} }} }}");
        let id = format!("id={thread_id}");
        let v = self.gh_json(&["api", "graphql", "-f", &q, "-f", &id])?;
        if let Some(errs) = v.get("errors") {
            bail!("{mutation}: {errs}");
        }
        Ok(())
    }

    /// Open PRs where the viewer's review is requested.
    pub fn review_requested(&self) -> Result<Vec<SearchHit>> {
        let v = self.gh_json(&[
            "search",
            "prs",
            "--review-requested=@me",
            "--state=open",
            "--limit",
            "50",
            "--json",
            "number,title,url,author,repository",
        ])?;
        Ok(v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                Some(SearchHit {
                    slug: p["repository"]["nameWithOwner"].as_str()?.to_string(),
                    number: p["number"].as_u64()?,
                    title: p["title"].as_str().unwrap_or_default().to_string(),
                    url: p["url"].as_str().unwrap_or_default().to_string(),
                    author: p["author"]["login"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect())
    }

    /// Logins that requested reviews on the PR (ReviewRequestedEvent actors).
    pub fn review_requesters(&self, slug: &str, number: u64) -> Result<Vec<String>> {
        let (owner, name) = split_slug(slug)?;
        let q = "query=query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { \
                 pullRequest(number: $number) { timelineItems(last: 50, itemTypes: [REVIEW_REQUESTED_EVENT]) { \
                 nodes { ... on ReviewRequestedEvent { actor { login } } } } } } }";
        let o = format!("owner={owner}");
        let nm = format!("name={name}");
        let num = format!("number={number}");
        let v = self.gh_json(&["api", "graphql", "-f", q, "-f", &o, "-f", &nm, "-F", &num])?;
        Ok(parse_requesters(&v))
    }

    /// Unified diff of the PR (for hunk membership checks).
    pub fn pr_diff(&self, slug: &str, number: u64) -> Result<String> {
        let n = number.to_string();
        self.gh(&["pr", "diff", &n, "-R", slug])
    }
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

pub fn split_slug(slug: &str) -> Result<(&str, &str)> {
    slug.split_once('/').filter(|(o, n)| !o.is_empty() && !n.is_empty()).context("expected owner/name")
}

fn pr_from_json(slug: &str, v: &Value) -> Result<PrInfo> {
    Ok(PrInfo {
        slug: slug.to_string(),
        number: v["number"].as_u64().context("pr number")?,
        title: v["title"].as_str().unwrap_or_default().to_string(),
        url: v["url"].as_str().unwrap_or_default().to_string(),
        author: v["author"]["login"].as_str().unwrap_or_default().to_string(),
        state: v["state"].as_str().unwrap_or_default().to_string(),
        head_ref: v["headRefName"].as_str().unwrap_or_default().to_string(),
        head_sha: v["headRefOid"].as_str().unwrap_or_default().to_string(),
        base_ref: v["baseRefName"].as_str().unwrap_or_default().to_string(),
        is_draft: v["isDraft"].as_bool().unwrap_or(false),
    })
}

fn thread_from_json(t: &Value) -> ReviewThread {
    ReviewThread {
        id: t["id"].as_str().unwrap_or_default().to_string(),
        is_resolved: t["isResolved"].as_bool().unwrap_or(false),
        is_outdated: t["isOutdated"].as_bool().unwrap_or(false),
        path: t["path"].as_str().unwrap_or_default().to_string(),
        line: t["line"].as_i64(),
        original_line: t["originalLine"].as_i64(),
        start_line: t["startLine"].as_i64(),
        side: t["diffSide"].as_str().unwrap_or("RIGHT").to_string(),
        comments: t["comments"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| ThreadComment {
                id: c["id"].as_str().unwrap_or_default().to_string(),
                database_id: c["databaseId"].as_i64().unwrap_or_default(),
                author: c["author"]["login"].as_str().unwrap_or("ghost").to_string(),
                body: c["body"].as_str().unwrap_or_default().to_string(),
                created_at: c["createdAt"].as_str().unwrap_or_default().to_string(),
                url: c["url"].as_str().unwrap_or_default().to_string(),
            })
            .collect(),
    }
}

fn parse_requesters(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["actor"]["login"].as_str().map(String::from))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn posted_from_json(v: &Value) -> Result<PostedComment> {
    Ok(PostedComment {
        id: v["id"].as_i64().with_context(|| format!("unexpected response: {v}"))?,
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
    })
}

/// Which lines of each file a unified diff touches, per side. Used to decide
/// whether a comment can be a line comment or must be file-level.
#[derive(Debug, Default, Clone)]
pub struct DiffLines {
    /// path -> (RIGHT lines in hunks, LEFT lines in hunks)
    pub files: std::collections::HashMap<String, (std::collections::BTreeSet<i64>, std::collections::BTreeSet<i64>)>,
}

impl DiffLines {
    pub fn parse(diff: &str) -> DiffLines {
        let mut out = DiffLines::default();
        let mut path: Option<String> = None;
        let (mut old, mut new) = (0i64, 0i64);
        for line in diff.lines() {
            if let Some(p) = line.strip_prefix("+++ ") {
                path = p.strip_prefix("b/").map(String::from).or_else(|| (p != "/dev/null").then(|| p.to_string()));
                continue;
            }
            if line.starts_with("--- ") || line.starts_with("diff --git") || line.starts_with("index ") {
                continue;
            }
            if let Some(h) = line.strip_prefix("@@ ") {
                // @@ -a,b +c,d @@
                let mut parts = h.split_whitespace();
                old = parts.next().and_then(|s| s.trim_start_matches('-').split(',').next()?.parse().ok()).unwrap_or(0);
                new = parts.next().and_then(|s| s.trim_start_matches('+').split(',').next()?.parse().ok()).unwrap_or(0);
                continue;
            }
            let Some(p) = &path else { continue };
            let entry = out.files.entry(p.clone()).or_default();
            match line.as_bytes().first() {
                Some(b'+') => {
                    entry.0.insert(new);
                    new += 1;
                }
                Some(b'-') => {
                    entry.1.insert(old);
                    old += 1;
                }
                Some(b' ') => {
                    entry.0.insert(new);
                    entry.1.insert(old);
                    new += 1;
                    old += 1;
                }
                _ => {}
            }
        }
        out
    }

    pub fn contains(&self, path: &str, side: &str, line: i64) -> bool {
        self.files.get(path).map(|(r, l)| if side == "LEFT" { l.contains(&line) } else { r.contains(&line) }).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_lines() {
        let d = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1,3 +1,4 @@\n a\n-b\n+c\n+d\n e\n";
        let dl = DiffLines::parse(d);
        assert!(dl.contains("x.rs", "RIGHT", 1));
        assert!(dl.contains("x.rs", "RIGHT", 3));
        assert!(dl.contains("x.rs", "LEFT", 2));
        assert!(!dl.contains("x.rs", "RIGHT", 9));
        assert!(!dl.contains("y.rs", "RIGHT", 1));
    }

    #[test]
    fn requesters_parse() {
        let v: Value = serde_json::from_str(
            r#"{"data":{"repository":{"pullRequest":{"timelineItems":{"nodes":[
                {"actor":{"login":"bob"}},{"actor":{"login":"amy"}},{"actor":{"login":"bob"}},{}]}}}}}"#,
        )
        .unwrap();
        assert_eq!(parse_requesters(&v), vec!["amy".to_string(), "bob".to_string()]);
    }

    #[test]
    fn thread_parse() {
        let v: Value = serde_json::from_str(
            r#"{"id":"T1","isResolved":false,"isOutdated":false,"path":"a.rs","line":null,"originalLine":7,
                "startLine":null,"diffSide":"RIGHT","comments":{"nodes":[{"id":"C","databaseId":5,"body":"hi",
                "createdAt":"x","url":"u","author":{"login":"bob"}}]}}"#,
        )
        .unwrap();
        let t = thread_from_json(&v);
        assert_eq!(t.anchor_line(), Some(7));
        assert_eq!(t.comments[0].author, "bob");
    }
}
