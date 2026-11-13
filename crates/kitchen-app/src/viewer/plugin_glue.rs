//! The viewer's side of plugins: file identity and freshness, the paint-time
//! decoration overlay, `]d`/`[d`, the `\` leader maps and command context.

use std::time::{Duration, Instant};

use egui::{Color32, Ui};
use kitchen_core::host::Host;
use kitchen_core::util::shell_quote;

use super::code_view::{Block, Editor, Overlay};
use super::doc::LineKind;
use super::picker::Picker;
use super::{Loc, Msg, PickerKind, Viewer};
use crate::plugins::decor::{FileKey, Severity, Side};
use crate::theme;

/// How often the open file is re-`stat`ed while the viewer is shown.
const STAT_EVERY: Duration = Duration::from_secs(3);
/// A leader sequence is abandoned after this long.
const LEADER_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct OverlayCache {
    generation: u64,
    pub path: String,
    version: String,
    pub overlay: Overlay,
    /// (1-based line, display column, severity, message) sorted by position.
    diags: Vec<(usize, usize, Severity, String)>,
}

/// File contents plus a size/mtime stamp, in one round trip.
pub(super) fn read_with_meta(host: &Host, path: &str) -> anyhow::Result<(String, String)> {
    match host {
        Host::Local => {
            let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
            Ok((local_meta(path).unwrap_or_default(), text))
        }
        Host::Ssh(_) => {
            let q = shell_quote(path);
            let out = host.sh_ok(&format!("{} && echo && cat -- {q}", stat_script(&q)), None)?;
            let (meta, text) = out.split_once('\n').unwrap_or(("", ""));
            Ok((meta.trim().to_string(), text.to_string()))
        }
    }
}

fn stat_script(q: &str) -> String {
    // GNU stat, then BSD stat.
    format!("{{ stat -c '%s %Y' -- {q} 2>/dev/null || stat -f '%z %m' -- {q}; }} | tr -d '\\n'")
}

fn local_meta(path: &str) -> Option<String> {
    let m = std::fs::metadata(path).ok()?;
    let secs = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{} {secs}", m.len()))
}

/// The tab-expanded text before 1-based raw column `col`.
fn expanded_prefix(raw: &str, col: usize) -> String {
    super::doc::expand_tabs(&raw.chars().take(col.saturating_sub(1)).collect::<String>())
}

/// Byte offset in the tab-expanded line of 1-based raw column `col`.
fn display_offset(raw: &str, col: usize) -> usize {
    expanded_prefix(raw, col).len()
}

/// Character column in the tab-expanded line of 1-based raw column `col`.
pub(super) fn display_col(raw: &str, col: usize) -> usize {
    expanded_prefix(raw, col).chars().count()
}

/// 1-based raw column of display column `display` (0-based, tabs expanded).
fn raw_col(raw: &str, display: usize) -> usize {
    let mut width = 0;
    for (i, c) in raw.chars().enumerate() {
        let w = if c == '\t' { 4 - width % 4 } else { 1 };
        if width + w > display {
            return i + 1;
        }
        width += w;
    }
    raw.chars().count() + 1
}

fn severity_color(s: Severity) -> Color32 {
    match s {
        Severity::Error => theme::RED,
        Severity::Warning => theme::YELLOW,
        Severity::Info => theme::CYAN,
        Severity::Hint => theme::DIM,
    }
}

impl Viewer {
    pub(super) fn file_key(&self, path: &str) -> FileKey {
        FileKey { host: self.host.label().to_string(), worktree: self.worktree.clone(), path: path.to_string() }
    }

    /// Re-stat the current file every few seconds; a change re-reads it.
    pub(super) fn check_fresh(&mut self) {
        let Some(path) = self.current.clone().filter(|p| self.files.contains_key(p)) else { return };
        if self.stat_pending || self.stat_at.elapsed() < STAT_EVERY || self.loading.contains(&path) {
            if !self.stat_pending {
                self.engine.ctx.request_repaint_after(STAT_EVERY);
            }
            return;
        }
        self.stat_at = Instant::now();
        self.stat_pending = true;
        let (host, full, tx, ctx) = (self.host.clone(), self.abs_path(&path), self.tx.clone(), self.engine.ctx.clone());
        std::thread::spawn(move || {
            let meta = match &host {
                Host::Local => local_meta(&full),
                Host::Ssh(_) => host.sh_ok(&stat_script(&shell_quote(&full)), None).ok().map(|s| s.trim().to_string()),
            };
            let _ = tx.send(Msg::Stat { path, meta });
            ctx.request_repaint();
        });
    }

    /// Every few seconds re-list the changes quietly (one small round trip;
    /// the file list is only re-sent when they or HEAD moved).
    pub(super) fn check_changes(&mut self) {
        if self.changes_pending || self.changes_at.elapsed() < STAT_EVERY {
            if !self.changes_pending {
                self.engine.ctx.request_repaint_after(STAT_EVERY);
            }
            return;
        }
        self.changes_at = Instant::now();
        self.load_file_list(true);
    }

    /// Rebuild the overlay when the file's decorations or text changed.
    pub(super) fn refresh_overlay(&mut self) {
        let Some(plugins) = self.engine.plugins() else { return };
        let Some(path) = self.current.clone() else { return };
        let Some(file) = self.files.get(&path) else { return };
        let generation = plugins.decor.generation();
        if self.overlay.as_ref().is_some_and(|o| o.generation == generation && o.path == path && o.version == file.version) {
            return;
        }
        let decor = plugins.decor.get(&self.file_key(&path));
        let raw: Vec<&str> = file.text.lines().collect();
        let mut overlay = Overlay::default();
        let mut diags = Vec::new();
        let mut best: std::collections::HashMap<usize, Severity> = std::collections::HashMap::new();
        if let Some(decor) = decor {
            for h in decor.highlights(&file.version) {
                let Some(line) = raw.get(h.line - 1) else { continue };
                let Some(color) = super::highlight::group_color(&h.group) else { continue };
                let expanded_len = display_offset(line, usize::MAX);
                let start = display_offset(line, h.col);
                let end = h.end_col.map(|c| display_offset(line, c)).unwrap_or(expanded_len).max(start);
                overlay.lines.entry(h.line).or_default().spans.push((start..end, color));
            }
            for (ns, d) in decor.diagnostics(&file.version) {
                let Some(first) = raw.get(d.line - 1) else { continue };
                let color = severity_color(d.severity);
                let last = d.end_line.unwrap_or(d.line).clamp(d.line, d.line + 50).min(raw.len());
                for n in d.line..=last {
                    let line = raw[n - 1];
                    let full = display_offset(line, usize::MAX);
                    let start = if n == d.line { display_offset(line, d.col) } else { 0 };
                    let end = match (n == last, d.end_col) {
                        (true, Some(c)) => display_offset(line, c),
                        (true, None) if n == d.line => {
                            // No end: underline the word at the column (at least one char).
                            let rest = &line.chars().skip(d.col.saturating_sub(1)).collect::<String>();
                            let word = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').count().max(1);
                            display_offset(line, d.col + word)
                        }
                        _ => full,
                    }
                    .min(full.max(start + 1));
                    overlay.lines.entry(n).or_default().underlines.push((start..end.max(start), color));
                }
                let lo = overlay.lines.entry(d.line).or_default();
                if best.get(&d.line).is_none_or(|b| d.severity < *b) {
                    best.insert(d.line, d.severity);
                    lo.sign = Some(('●', color));
                    let msg: String = d.message.lines().next().unwrap_or("").chars().take(140).collect();
                    lo.virt = Some((msg, color));
                }
                let code = d.code.as_deref().map(|c| format!(" [{c}]")).unwrap_or_default();
                diags.push((d.line, display_col(first, d.col), d.severity, format!("{}{code}: {}  ({ns})", d.severity.label(), d.message)));
            }
        }
        self.overlay = Some(OverlayCache { generation, path, version: file.version.clone(), overlay, diags });
    }

    /// The diagnostic under the cursor, for the status line.
    pub(super) fn cursor_diagnostic(&self) -> Option<(String, Color32)> {
        let o = self.overlay.as_ref().filter(|o| self.current.as_deref() == Some(o.path.as_str()) && !self.browsing)?;
        let line = self.current_source_line()?;
        let d = o.diags.iter().filter(|d| d.0 == line).min_by_key(|d| d.2)?;
        Some((d.3.clone(), severity_color(d.2)))
    }

    pub(super) fn step_diag(&mut self, forward: bool) {
        let Some(o) = self.overlay.as_ref().filter(|o| self.current.as_deref() == Some(o.path.as_str()) && !self.browsing) else {
            self.status = Some(("no diagnostics".into(), false));
            return;
        };
        let here = (self.current_source_line().unwrap_or(1), self.vim.col);
        let target = if forward {
            o.diags.iter().find(|d| (d.0, d.1) > here).or(o.diags.first())
        } else {
            o.diags.iter().rev().find(|d| (d.0, d.1) < here).or(o.diags.last())
        };
        match target.cloned() {
            Some((line, col, _, msg)) => {
                if let Some(here) = self.here() {
                    self.back.push(here);
                }
                self.outline = false;
                self.place_cursor(line, col);
                self.status = Some((msg, false));
            }
            None => self.status = Some(("no diagnostics".into(), false)),
        }
    }

    pub(super) fn open_diagnostics_picker(&mut self) {
        let Some(o) = self.overlay.as_ref().filter(|o| self.current.as_deref() == Some(o.path.as_str())) else {
            self.status = Some(("no diagnostics in this file".into(), false));
            return;
        };
        if o.diags.is_empty() {
            self.status = Some(("no diagnostics in this file".into(), false));
            return;
        }
        let path = o.path.clone();
        let locs: Vec<Loc> = o.diags.iter().map(|d| Loc { path: path.clone(), line: d.0, col: d.1 }).collect();
        let items = o.diags.iter().map(|d| (format!("{}:{}  {}", d.0, d.1 + 1, d.3), String::new())).collect();
        self.picker = Some((PickerKind::Defs(locs), Picker::new(format!("diagnostics in {path}"), items)));
    }

    /// Plugin comments on the open file as blocks under their rows, with
    /// their ids. Old-side lines only map onto a diff.
    pub(super) fn comment_blocks(&self) -> Vec<(Block, Option<String>)> {
        let Some(plugins) = self.engine.plugins() else { return Vec::new() };
        let Some(path) = self.current.as_ref().filter(|_| !self.browsing && !self.outline) else { return Vec::new() };
        let Some(decor) = plugins.decor.get(&self.file_key(path)) else { return Vec::new() };
        decor
            .comments()
            .map(|c| {
                let row = c.line.and_then(|n| self.row_at(c.side, n));
                let mut title = c.title.clone();
                if let (None, Some(n)) = (row, c.line) {
                    let side = if c.side == Side::Old { "base " } else { "" };
                    title.push_str(&format!(" · {side}L{n}"));
                }
                let block = Block {
                    after: row,
                    title,
                    body: c.body.clone(),
                    color: theme::CYAN,
                    dim: c.dim,
                    replies: c.replies.clone(),
                    repo: c.repo.clone(),
                    ..Default::default()
                };
                (block, c.id.clone())
            })
            .collect()
    }

    /// The doc row showing line `n` of `side` (a fold row if it is folded: a
    /// comment or answer on a hidden line goes under its fold).
    pub(super) fn row_at(&self, side: Side, n: usize) -> Option<usize> {
        let is_diff = self.doc_key.as_ref().is_some_and(|k| k.1);
        self.doc.lines.iter().position(|l| match side {
            Side::New => l.kind != LineKind::Removed && l.covers_new(n),
            Side::Old => is_diff && (l.old_no == Some(n) || l.fold.is_some_and(|f| f.covers_old(n))),
        })
    }

    /// Where doc row `i` is: a removed line on the base side, everything else
    /// in the current file. A rendered or fold row stands for its source
    /// lines: a range ends (`last`) on its last.
    pub(super) fn row_line(&self, i: usize, last: bool) -> (Side, Option<usize>) {
        match self.doc.lines.get(i) {
            Some(l) if l.kind == LineKind::Removed => (Side::Old, l.old_no),
            Some(l) if last => (Side::New, l.last_no()),
            Some(l) => (Side::New, l.first_no()),
            None => (Side::New, None),
        }
    }

    /// The source text of doc rows `start..=end`, also for rendered and fold rows.
    pub(super) fn quote(&self, path: &str, start: usize, end: usize) -> String {
        let raw: Vec<&str> = self.files.get(path).map(|f| f.text.lines().collect()).unwrap_or_default();
        let lines: Vec<&str> = (start..=end)
            .filter_map(|i| self.doc.lines.get(i))
            .flat_map(|l| match (l.render.is_some() || l.fold.is_some(), l.first_no(), l.last_no()) {
                (true, Some(a), Some(b)) => raw.get(a - 1..b.min(raw.len())).unwrap_or_default().to_vec(),
                _ => vec![l.text.as_str()],
            })
            .collect();
        lines.join("\n")
    }

    /// ⌘M: a comment field under the cursor line or visual selection. On a
    /// line with a comment it replies to that comment.
    pub(super) fn start_comment(&mut self) {
        if self.browsing || self.outline || !self.current.as_ref().is_some_and(|p| self.files.contains_key(p)) {
            self.status = Some(("open a file to comment on it".into(), false));
            return;
        }
        let who = self.engine.plugins().map(|p| p.listeners("comment")).unwrap_or_default();
        if who.is_empty() {
            self.status = Some(("no plugin takes comments (the github plugin does, for a PR shown with ⌘⇧P)".into(), false));
            return;
        }
        let (start, end) = self.vim.visual_lines().unwrap_or((self.vim.line, self.vim.line));
        let thread = self.comment_blocks().into_iter().find(|(b, id)| b.after == Some(end) && id.is_some());
        let (first, last) = (self.row_line(start, false).1.unwrap_or(start + 1), self.row_line(end, true).1.unwrap_or(end + 1));
        let what = match &thread {
            Some((b, _)) => format!("Reply to {}", b.title),
            None if end > start || last > first => format!("Comment on lines {first}–{last}"),
            None => format!("Comment on line {last}"),
        };
        self.reply_to = thread.and_then(|(_, id)| id);
        self.list_focus = false;
        self.editor = Some(Editor { start, end, text: String::new(), label: format!("{what} — ⌘Enter sends it to {} · Esc cancels", who.join(", ")), needs_focus: true, ask: false, follow_up: false });
    }

    /// Hand the comment field to the `comment` listeners.
    pub(super) fn submit_comment(&mut self) {
        let Some(ed) = self.editor.take() else { return };
        let text = ed.text.trim().to_string();
        if text.is_empty() {
            self.editor = Some(ed);
            return;
        }
        let Some(path) = self.current.clone() else { return };
        let ((side, line), (start_side, start_line)) = (self.row_line(ed.end, true), self.row_line(ed.start, false));
        let multi = (side, line) != (start_side, start_line);
        let k = self.file_key(&path);
        let ev = serde_json::json!({
            "file": { "host": k.host, "worktree": k.worktree, "path": k.path, "abs": k.abs() },
            "path": path,
            "line": line,
            "side": side,
            "start_line": if multi { start_line } else { None },
            "start_side": if multi { Some(start_side) } else { None },
            "text": text,
            "quote": self.quote(&path, ed.start, ed.end),
            "reply_to": self.reply_to.take(),
            "tab": crate::plugins::command_context(&self.engine, &self.tab_id, None),
        });
        let sent = self.engine.plugins().is_some_and(|p| p.emit("comment", ev));
        self.status = Some(if sent { ("comment sent".into(), false) } else { ("no plugin took the comment".into(), true) });
        self.vim.anchor = None;
    }

    /// The viewer state handed to plugin commands.
    pub fn context(&self) -> serde_json::Value {
        let sel = self.vim.visual_lines().map(|(a, b)| {
            let doc = &self.doc.lines;
            serde_json::json!({ "from": doc.get(a).and_then(|l| l.first_no()), "to": doc.get(b).and_then(|l| l.last_no()) })
        });
        let line = if self.browsing { None } else { self.current_source_line() };
        let raw_line = line.and_then(|n| self.current.as_ref().and_then(|p| self.files.get(p)).and_then(|f| f.text.lines().nth(n - 1)));
        let word = self.doc.lines.get(self.vim.line).and_then(|l| super::vim::word_at(&l.text, self.vim.col));
        let file = self.current.as_ref().filter(|_| !self.browsing).map(|p| {
            let k = self.file_key(p);
            serde_json::json!({ "host": k.host, "worktree": k.worktree, "path": k.path, "abs": k.abs() })
        });
        serde_json::json!({
            "path": if self.browsing { None } else { self.current.clone() },
            "file": file,
            // The worktree shown (a PR's, after `kitchen.view`), even while browsing.
            "worktree": self.worktree,
            "line": line,
            // 1-based character of the raw line (tabs count once), like diagnostics.
            "col": raw_line.map(|l| raw_col(l, self.vim.col)).unwrap_or(self.vim.col + 1),
            "word": word,
            "sel": sel,
            "dir": self.dir,
            // Diff mode (⌘D) rather than the plain file.
            "diff": self.diff,
            "version": self.current.as_ref().and_then(|p| self.files.get(p)).map(|f| f.version.clone()),
            "menu": self.menu_row(),
        })
    }

    /// While the ⌘⇧S menu is up, the plugin's entry under its cursor:
    /// `{label, section, url}`, `{}` on a row that is none (the tab's own
    /// worktree). None while the menu is down.
    fn menu_row(&self) -> Option<serde_json::Value> {
        let Some((super::PickerKind::Views { menu, .. }, picker)) = &self.picker else { return None };
        // The menu's rows are its entries, in order, then the worktree row.
        let entry = menu.as_ref().zip(picker.selected_item()).and_then(|(m, i)| m.get(i));
        Some(entry.map_or_else(|| serde_json::json!({}), |(e, _)| serde_json::json!({ "label": e.label, "section": e.section, "url": e.url })))
    }

    pub fn current_filetype(&self) -> Option<String> {
        self.current.as_deref().filter(|_| !self.browsing).map(crate::plugins::filetype_of)
    }

    /// `\` + keys → a plugin viewer map. Consumes the keys so vim never sees them.
    pub(super) fn leader_keys(&mut self, ui: &mut Ui) {
        let Some(plugins) = self.engine.plugins().cloned() else { return };
        if self.leader.as_ref().is_some_and(|(_, at)| at.elapsed() > LEADER_TIMEOUT) {
            self.leader = None;
        }
        let reg = plugins.registry();
        if reg.viewer_maps.is_empty() && self.leader.is_none() {
            return;
        }
        let mut typed: Vec<char> = Vec::new();
        let mut cancel = false;
        ui.input_mut(|i| {
            let mut j = 0;
            while j < i.events.len() {
                match &i.events[j] {
                    egui::Event::Text(t) if self.leader.is_some() || t.starts_with('\\') || !typed.is_empty() => {
                        typed.extend(t.chars());
                        i.events.remove(j);
                    }
                    egui::Event::Key { key: egui::Key::Escape, pressed: true, .. } if self.leader.is_some() => {
                        cancel = true;
                        i.events.remove(j);
                    }
                    _ => j += 1,
                }
            }
        });
        if cancel {
            self.leader = None;
            return;
        }
        for c in typed {
            match &mut self.leader {
                None if c == '\\' => self.leader = Some((String::new(), Instant::now())),
                None => {}
                Some((keys, _)) => {
                    keys.push(c);
                    let keys = keys.clone();
                    if let Some(m) = reg.viewer_maps.iter().find(|m| m.keys == keys) {
                        self.leader = None;
                        let ctx = crate::plugins::command_context(&self.engine, &self.tab_id, Some(self.context()));
                        if let Err(e) = plugins.invoke(&m.command, ctx, false) {
                            self.status = Some((format!("{e:#}"), true));
                        }
                    } else if !reg.viewer_maps.iter().any(|m| m.keys.starts_with(&keys)) {
                        self.leader = None;
                        self.status = Some((format!("no mapping for \\{keys}"), false));
                    }
                }
            }
        }
        if self.leader.is_some() {
            ui.ctx().request_repaint_after(LEADER_TIMEOUT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_columns_map_through_tabs() {
        assert_eq!(display_offset("\tx = 1", 2), 4);
        assert_eq!(display_col("\tx = 1", 2), 4);
        assert_eq!(display_offset("ab\tc", 4), 4);
        assert_eq!(display_offset("é = 1", 2), 2, "bytes, not chars");
        assert_eq!(display_col("é = 1", 2), 1);
        assert_eq!(display_offset("abc", 99), 3);
        assert_eq!(raw_col("\tx = 1", 4), 2, "display col 4 is the x after a tab");
        assert_eq!(raw_col("\tx = 1", 2), 1, "inside the tab");
        assert_eq!(raw_col("ab", 0), 1);
        assert_eq!(raw_col("ab", 9), 3);
    }
}
