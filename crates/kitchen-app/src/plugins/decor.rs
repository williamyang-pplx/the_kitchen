//! Decorations pushed by plugins (and by any program over the socket):
//! diagnostics and highlights, in namespaced sets per file. A set replaces
//! the previous set of the same namespace and kind wholesale.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Most items kept per set.
pub const MAX_ITEMS: usize = 10_000;

/// A file as the viewer holds it. Edit tabs have their own worktrees, so the
/// worktree is part of the identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileKey {
    /// `local` or the devbox ssh alias.
    pub host: String,
    pub worktree: String,
    /// Relative to the worktree (absolute for files outside it).
    pub path: String,
}

impl FileKey {
    pub fn abs(&self) -> String {
        if self.path.starts_with('/') {
            return self.path.clone();
        }
        format!("{}/{}", self.worktree.trim_end_matches('/'), self.path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    pub fn parse(s: &str) -> Severity {
        match s.to_ascii_lowercase().as_str() {
            "error" | "e" | "fatal" => Severity::Error,
            "warning" | "warn" | "w" => Severity::Warning,
            "hint" => Severity::Hint,
            _ => Severity::Info,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
            Severity::Hint => "hint",
        }
    }
}

/// Positions: 1-based lines, 1-based columns counted in characters of the
/// raw line (before tab expansion), `end_col` exclusive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub line: usize,
    #[serde(default = "one")]
    pub col: usize,
    #[serde(default)]
    pub end_line: Option<usize>,
    #[serde(default)]
    pub end_col: Option<usize>,
    #[serde(default = "default_severity")]
    pub severity: Severity,
    pub message: String,
    #[serde(default)]
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Highlight {
    pub line: usize,
    #[serde(default = "one")]
    pub col: usize,
    /// Exclusive; `None` = to the end of the line.
    #[serde(default)]
    pub end_col: Option<usize>,
    /// A TextMate scope resolved through the theme (`keyword`, `comment`, …)
    /// or `#rrggbb`.
    pub group: String,
}

fn one() -> usize {
    1
}
fn default_severity() -> Severity {
    Severity::Warning
}

/// Parse diagnostics from JSON (Lua tables or socket params). Severity may
/// be a name (`error`, `warn`, …) or an LSP number (1 error … 4 hint).
pub fn parse_diagnostics(v: serde_json::Value) -> Result<Vec<Diagnostic>, String> {
    let serde_json::Value::Array(items) = v else { return Err("diagnostics must be a list".into()) };
    items
        .into_iter()
        .enumerate()
        .map(|(i, mut item)| {
            if let Some(sev) = item.get("severity").cloned() {
                let s = match sev {
                    serde_json::Value::Number(n) => match n.as_u64() {
                        Some(1) => Severity::Error,
                        Some(2) => Severity::Warning,
                        Some(4) => Severity::Hint,
                        _ => Severity::Info,
                    },
                    serde_json::Value::String(s) => Severity::parse(&s),
                    _ => Severity::Warning,
                };
                item["severity"] = serde_json::Value::String(s.label().into());
            }
            let d: Diagnostic = serde_json::from_value(item).map_err(|e| format!("diagnostic #{}: {e}", i + 1))?;
            if d.line == 0 {
                return Err(format!("diagnostic #{}: lines are 1-based", i + 1));
            }
            Ok(d)
        })
        .collect()
}

pub fn parse_highlights(v: serde_json::Value) -> Result<Vec<Highlight>, String> {
    let serde_json::Value::Array(items) = v else { return Err("highlights must be a list".into()) };
    items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let h: Highlight = serde_json::from_value(item).map_err(|e| format!("highlight #{}: {e}", i + 1))?;
            if h.line == 0 {
                return Err(format!("highlight #{}: lines are 1-based", i + 1));
            }
            Ok(h)
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Diagnostics,
    Highlights,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "diagnostics" => Some(Kind::Diagnostics),
            "highlights" => Some(Kind::Highlights),
            _ => None,
        }
    }
}

/// One namespace's decorations for a file.
#[derive(Debug, Clone, Default)]
pub struct Set {
    pub ns: String,
    /// Content hash of the text the items were computed from; `None` = always shown.
    pub diag_version: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
    pub hl_version: Option<String>,
    pub highlights: Vec<Highlight>,
}

#[derive(Debug, Clone, Default)]
pub struct FileDecor {
    pub sets: Vec<Set>,
}

impl FileDecor {
    /// Diagnostics valid for the text with hash `version`, sorted by position.
    pub fn diagnostics(&self, version: &str) -> Vec<(&str, &Diagnostic)> {
        let mut v: Vec<(&str, &Diagnostic)> = self
            .sets
            .iter()
            .filter(|s| s.diag_version.as_deref().is_none_or(|v| v == version))
            .flat_map(|s| s.diagnostics.iter().map(move |d| (s.ns.as_str(), d)))
            .collect();
        v.sort_by(|a, b| (a.1.line, a.1.col, a.1.severity).cmp(&(b.1.line, b.1.col, b.1.severity)));
        v
    }

    pub fn highlights(&self, version: &str) -> Vec<&Highlight> {
        self.sets.iter().filter(|s| s.hl_version.as_deref().is_none_or(|v| v == version)).flat_map(|s| s.highlights.iter()).collect()
    }
}

/// All decorations, readable by the UI without holding a lock: it compares
/// [`DecorStore::generation`] and only then fetches the file's `Arc`.
#[derive(Default)]
pub struct DecorStore {
    files: Mutex<HashMap<FileKey, Arc<FileDecor>>>,
    generation: AtomicU64,
}

impl DecorStore {
    fn files(&self) -> std::sync::MutexGuard<'_, HashMap<FileKey, Arc<FileDecor>>> {
        self.files.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn get(&self, key: &FileKey) -> Option<Arc<FileDecor>> {
        self.files().get(key).cloned()
    }

    pub fn set_diagnostics(&self, key: &FileKey, ns: &str, mut items: Vec<Diagnostic>, version: Option<String>) {
        items.truncate(MAX_ITEMS);
        self.update(key, ns, |s| {
            s.diagnostics = items;
            s.diag_version = version;
        });
    }

    pub fn set_highlights(&self, key: &FileKey, ns: &str, mut items: Vec<Highlight>, version: Option<String>) {
        items.truncate(MAX_ITEMS);
        self.update(key, ns, |s| {
            s.highlights = items;
            s.hl_version = version;
        });
    }

    fn update(&self, key: &FileKey, ns: &str, f: impl FnOnce(&mut Set)) {
        let mut files = self.files();
        let mut decor = files.get(key).map(|d| (**d).clone()).unwrap_or_default();
        let idx = match decor.sets.iter().position(|s| s.ns == ns) {
            Some(i) => i,
            None => {
                decor.sets.push(Set { ns: ns.to_string(), ..Default::default() });
                decor.sets.len() - 1
            }
        };
        f(&mut decor.sets[idx]);
        decor.sets.retain(|s| !s.diagnostics.is_empty() || !s.highlights.is_empty());
        if decor.sets.is_empty() {
            files.remove(key);
        } else {
            files.insert(key.clone(), Arc::new(decor));
        }
        drop(files);
        self.bump();
    }

    /// Clear `kind` (or everything) of namespace `ns` for one file or all files.
    pub fn clear(&self, key: Option<&FileKey>, ns: &str, kind: Option<Kind>) {
        let mut files = self.files();
        let keys: Vec<FileKey> = match key {
            Some(k) => vec![k.clone()],
            None => files.keys().cloned().collect(),
        };
        for k in keys {
            let Some(d) = files.get(&k) else { continue };
            let mut decor = (**d).clone();
            for s in decor.sets.iter_mut().filter(|s| s.ns == ns) {
                if kind != Some(Kind::Highlights) {
                    s.diagnostics.clear();
                }
                if kind != Some(Kind::Diagnostics) {
                    s.highlights.clear();
                }
            }
            decor.sets.retain(|s| !s.diagnostics.is_empty() || !s.highlights.is_empty());
            if decor.sets.is_empty() {
                files.remove(&k);
            } else {
                files.insert(k, Arc::new(decor));
            }
        }
        drop(files);
        self.bump();
    }

    /// Drop every namespace owned by `plugin` (`plugin` and `plugin.*`).
    pub fn clear_owner(&self, plugin: &str) {
        let prefix = format!("{plugin}.");
        let mut files = self.files();
        files.retain(|_, d| {
            if d.sets.iter().any(|s| s.ns == plugin || s.ns.starts_with(&prefix)) {
                let mut decor = (**d).clone();
                decor.sets.retain(|s| s.ns != plugin && !s.ns.starts_with(&prefix));
                let keep = !decor.sets.is_empty();
                *d = Arc::new(decor);
                keep
            } else {
                true
            }
        });
        drop(files);
        self.bump();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> FileKey {
        FileKey { host: "local".into(), worktree: "/r".into(), path: "a.py".into() }
    }

    fn diag(line: usize) -> Diagnostic {
        Diagnostic { line, col: 1, end_line: None, end_col: None, severity: Severity::Error, message: "x".into(), code: None }
    }

    #[test]
    fn sets_replace_and_versions_filter() {
        let s = DecorStore::default();
        let g0 = s.generation();
        s.set_diagnostics(&key(), "ruff", vec![diag(1), diag(2)], Some("v1".into()));
        s.set_diagnostics(&key(), "ruff", vec![diag(3)], Some("v1".into()));
        s.set_diagnostics(&key(), "ext.tool", vec![diag(4)], None);
        assert!(s.generation() > g0);
        let d = s.get(&key()).unwrap();
        let lines: Vec<usize> = d.diagnostics("v1").iter().map(|(_, d)| d.line).collect();
        assert_eq!(lines, vec![3, 4]);
        let stale: Vec<usize> = d.diagnostics("v2").iter().map(|(_, d)| d.line).collect();
        assert_eq!(stale, vec![4], "versioned set hidden once the text changed");
        s.clear_owner("ruff");
        assert_eq!(s.get(&key()).unwrap().sets.len(), 1);
        s.clear(None, "ext.tool", None);
        assert!(s.get(&key()).is_none());
    }
}
