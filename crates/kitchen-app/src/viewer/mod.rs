//! The file viewer (one per tab): PR-relevant files first, plain / diff /
//! outline modes, vim navigation, jump-to-definition with a jump list, and
//! inline PR comments. All file access goes through the workspace's
//! [`Host`] on background threads, so devbox workspaces work the same.

pub mod code_view;
pub mod doc;
pub mod highlight;
pub mod picker;
pub mod vim;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use egui::{RichText, Ui};
use kitchen_core::gh::CommentAnchor;
use kitchen_core::git::{ChangedFile, Repo};
use kitchen_core::host::Host;
use kitchen_core::summaries;
use kitchen_symbols::Symbol;

use crate::engine::{Engine, Tab, Workspace};
use crate::panels::{PanelCmd, PanelCtx};
use crate::review;
use crate::theme;
use code_view::{Block, CodeView, Editor, ViewState};
use doc::{Doc, DocLine, LineKind};
use highlight::Spans;
use picker::{Picker, PickerResult};
use vim::{Vim, VimAction};

/// Most definition candidates read per jump (bounds ssh round trips).
const MAX_DEF_FILES: usize = 20;
/// Most symbols summarized per model call.
const MAX_SUMMARY_SYMBOLS: usize = 80;

struct FileData {
    text: String,
    spans: Option<Spans>,
    symbols: Vec<Symbol>,
}

struct BaseData {
    /// `None` when the file does not exist at the base (new file).
    text: Option<String>,
    spans: Option<Spans>,
}

/// A position in the worktree: path + 1-based line in the current file.
#[derive(Clone, Debug, PartialEq)]
struct Loc {
    path: String,
    line: usize,
    col: usize,
}

enum Msg {
    Files { changed: Vec<ChangedFile>, all: Vec<String>, base: String },
    FilesFailed(String),
    File { path: String, result: Result<FileData, String> },
    Base { path: String, data: BaseData },
    Defs { name: String, hits: Vec<(String, Symbol)> },
    Summaries { path: String, map: HashMap<String, String>, error: Option<String> },
    Posted(Result<String, String>),
}

enum PickerKind {
    Files,
    Defs(Vec<Loc>),
}

pub struct Viewer {
    engine: Arc<Engine>,
    tab_id: String,
    host: Host,
    worktree: String,
    base_ref: String,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    changed: Vec<ChangedFile>,
    all: Vec<String>,
    files_state: Option<Result<(), String>>,
    /// Merge base of HEAD and origin/<default>, for diffs.
    merge_base: Option<String>,

    current: Option<String>,
    diff: bool,
    outline: bool,
    files: HashMap<String, FileData>,
    bases: HashMap<String, BaseData>,
    loading: HashSet<String>,
    load_errors: HashMap<String, String>,

    doc: Doc,
    doc_key: Option<(String, bool, bool, bool)>,
    /// Outline row -> 1-based source line.
    outline_lines: Vec<usize>,
    vim: Vim,
    view: ViewState,
    pending_goto: Option<(usize, usize)>,
    back: Vec<Loc>,
    fwd: Vec<Loc>,

    picker: Option<(PickerKind, Picker)>,
    editor: Option<Editor>,
    summaries: HashMap<String, HashMap<String, String>>,
    summary_pending: HashSet<String>,
    show_files: bool,
    status: Option<(String, bool)>,
    pending_def: Option<(String, Loc, Vec<(String, Symbol)>)>,
    /// ⌘Enter arrived; submit once the editor has seen this frame's input.
    submit_pending: bool,
}

impl Viewer {
    pub fn new(engine: &Arc<Engine>, ws: &Workspace, tab: &Tab) -> Self {
        let (tx, rx) = channel();
        let mut v = Self {
            engine: engine.clone(),
            tab_id: tab.row.id.clone(),
            host: ws.host(),
            worktree: tab.row.worktree.clone(),
            base_ref: format!("origin/{}", ws.row.default_branch.as_deref().unwrap_or("main")),
            tx,
            rx,
            changed: Vec::new(),
            all: Vec::new(),
            files_state: None,
            merge_base: None,
            current: None,
            diff: false,
            outline: false,
            files: HashMap::new(),
            bases: HashMap::new(),
            loading: HashSet::new(),
            load_errors: HashMap::new(),
            doc: Doc::default(),
            doc_key: None,
            outline_lines: Vec::new(),
            vim: Vim::default(),
            view: ViewState::default(),
            pending_goto: None,
            back: Vec::new(),
            fwd: Vec::new(),
            picker: None,
            editor: None,
            summaries: HashMap::new(),
            summary_pending: HashSet::new(),
            show_files: true,
            status: None,
            pending_def: None,
            submit_pending: false,
        };
        v.load_file_list();
        v
    }

    // ------------------------------------------------------------ loading

    fn load_file_list(&mut self) {
        self.files_state = None;
        let (host, wt, base_ref, tx, ctx) = (self.host.clone(), self.worktree.clone(), self.base_ref.clone(), self.tx.clone(), self.engine.ctx.clone());
        std::thread::spawn(move || {
            let repo = Repo::new(&host, &wt);
            let result = (|| -> anyhow::Result<Msg> {
                let base = repo.merge_base(&wt, "HEAD", &base_ref).unwrap_or(base_ref);
                let changed = repo.changed_files(&wt, &base)?;
                let all = repo.ls_files(&wt)?;
                Ok(Msg::Files { changed, all, base })
            })();
            let _ = tx.send(result.unwrap_or_else(|e| Msg::FilesFailed(format!("{e:#}"))));
            ctx.request_repaint();
        });
    }

    fn ensure_loaded(&mut self, path: &str) {
        if self.files.contains_key(path) || self.loading.contains(path) || self.load_errors.contains_key(path) {
            return;
        }
        self.loading.insert(path.to_string());
        let (host, full, path_s, tx, ctx) =
            (self.host.clone(), format!("{}/{}", self.worktree, path), path.to_string(), self.tx.clone(), self.engine.ctx.clone());
        std::thread::spawn(move || {
            let result = host.read_file(&full).map_err(|e| format!("{e:#}")).map(|text| FileData {
                spans: highlight::highlight(&path_s, &text),
                symbols: kitchen_symbols::extract(&path_s, &text),
                text,
            });
            let _ = tx.send(Msg::File { path: path_s, result });
            ctx.request_repaint();
        });
    }

    fn ensure_base(&mut self, path: &str) {
        let key = format!("base:{path}");
        let Some(base) = self.merge_base.clone() else { return };
        if self.bases.contains_key(path) || self.loading.contains(&key) {
            return;
        }
        self.loading.insert(key);
        let old_path = self.changed.iter().find(|c| c.path == path).and_then(|c| c.old_path.clone()).unwrap_or_else(|| path.to_string());
        let (host, wt, path_s, tx, ctx) = (self.host.clone(), self.worktree.clone(), path.to_string(), self.tx.clone(), self.engine.ctx.clone());
        std::thread::spawn(move || {
            let text = Repo::new(&host, &wt).show(&wt, &base, &old_path).ok().flatten();
            let spans = text.as_deref().and_then(|t| highlight::highlight(&old_path, t));
            let _ = tx.send(Msg::Base { path: path_s, data: BaseData { text, spans } });
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Files { changed, all, base } => {
                    self.changed = changed;
                    self.all = all;
                    self.merge_base = Some(base);
                    self.files_state = Some(Ok(()));
                    if self.current.is_none() {
                        if let Some(first) = self.changed.iter().find(|c| c.status != 'D').map(|c| c.path.clone()) {
                            self.diff = true;
                            self.open(&first, None, false);
                        }
                    }
                }
                Msg::FilesFailed(e) => self.files_state = Some(Err(e)),
                Msg::File { path, result } => {
                    self.loading.remove(&path);
                    match result {
                        Ok(data) => {
                            self.files.insert(path.clone(), data);
                        }
                        Err(e) => {
                            self.load_errors.insert(path.clone(), e);
                        }
                    }
                    self.doc_key = None;
                }
                Msg::Base { path, data } => {
                    self.loading.remove(&format!("base:{path}"));
                    self.bases.insert(path, data);
                    self.doc_key = None;
                }
                Msg::Defs { name, hits } => self.finish_defs(&name, hits),
                Msg::Summaries { path, map, error } => {
                    self.summary_pending.remove(&path);
                    if let Some(e) = error {
                        self.status = Some((format!("summaries: {e}"), true));
                    }
                    self.summaries.entry(path).or_default().extend(map);
                    self.doc_key = None;
                }
                Msg::Posted(r) => match r {
                    Ok(url) => self.engine.toast(format!("comment posted {url}")),
                    Err(e) => self.engine.toast_err(format!("comment not posted: {e}")),
                },
            }
        }
    }

    // ------------------------------------------------------------ navigation

    fn here(&self) -> Option<Loc> {
        let path = self.current.clone()?;
        let line = self.current_source_line().unwrap_or(1);
        Some(Loc { path, line, col: self.vim.col })
    }

    /// 1-based line in the current file under the cursor.
    fn current_source_line(&self) -> Option<usize> {
        if self.outline {
            return self.outline_lines.get(self.vim.line).copied();
        }
        let l = self.doc.lines.get(self.vim.line)?;
        l.new_no.or_else(|| {
            // Removed line: the next current-file line.
            self.doc.lines[self.vim.line..].iter().find_map(|l| l.new_no)
        })
    }

    fn open(&mut self, path: &str, line: Option<usize>, record: bool) {
        if record {
            if let Some(here) = self.here() {
                if self.back.last() != Some(&here) {
                    self.back.push(here);
                }
                self.fwd.clear();
            }
        }
        if self.current.as_deref() != Some(path) {
            self.current = Some(path.to_string());
            self.editor = None;
            self.vim.anchor = None;
            self.doc_key = None;
            self.pending_goto = Some((line.unwrap_or(1), 0));
        } else if let Some(l) = line {
            self.pending_goto = Some((l, 0));
        }
        self.outline = self.outline && line.is_none();
        self.ensure_loaded(path);
        if self.diff {
            self.ensure_base(path);
        }
    }

    fn goto_loc(&mut self, loc: Loc) {
        self.outline = false;
        self.open(&loc.path.clone(), None, false);
        self.pending_goto = Some((loc.line, loc.col));
    }

    fn jump_to_def(&mut self, name: String) {
        let Some(here) = self.here() else { return };
        let local: Vec<(String, Symbol)> = self
            .files
            .get(&here.path)
            .map(|f| f.symbols.iter().filter(|s| s.name == name).map(|s| (here.path.clone(), s.clone())).collect())
            .unwrap_or_default();
        // A unique definition in this file (that we're not already on) wins.
        if local.len() == 1 && local[0].1.line != here.line {
            let s = &local[0].1;
            let loc = Loc { path: here.path.clone(), line: s.line, col: s.col };
            self.record_and_go(loc);
            return;
        }
        self.status = Some((format!("searching for definition of {name}…"), false));
        self.pending_def = Some((name.clone(), here.clone(), local));
        let (host, wt, tx, ctx, from) = (self.host.clone(), self.worktree.clone(), self.tx.clone(), self.engine.ctx.clone(), here.path);
        std::thread::spawn(move || {
            let hits = search_definitions(&host, &wt, &name, &from);
            let _ = tx.send(Msg::Defs { name, hits });
            ctx.request_repaint();
        });
    }

    fn finish_defs(&mut self, name: &str, hits: Vec<(String, Symbol)>) {
        let Some((pending, here, mut all)) = self.pending_def.take() else { return };
        if pending != name {
            return;
        }
        for h in hits {
            if !all.iter().any(|(p, s)| *p == h.0 && s.line == h.1.line) {
                all.push(h);
            }
        }
        // Drop the definition we're standing on.
        all.retain(|(p, s)| !(p == &here.path && s.line == here.line));
        match all.len() {
            0 => self.status = Some((format!("no definition found for {name}"), true)),
            1 => {
                self.status = None;
                let (p, s) = &all[0];
                let loc = Loc { path: p.clone(), line: s.line, col: s.col };
                self.record_and_go(loc);
            }
            _ => {
                self.status = None;
                let locs: Vec<Loc> = all.iter().map(|(p, s)| Loc { path: p.clone(), line: s.line, col: s.col }).collect();
                let items = all.iter().map(|(p, s)| (format!("{p}:{}", s.line), format!("{} {}", s.kind, s.signature))).collect();
                self.picker = Some((PickerKind::Defs(locs), Picker::new(format!("definitions of {name}"), items)));
            }
        }
    }

    fn record_and_go(&mut self, loc: Loc) {
        if let Some(here) = self.here() {
            self.back.push(here);
            self.fwd.clear();
        }
        self.goto_loc(loc);
    }

    fn jump_back(&mut self) {
        let Some(loc) = self.back.pop() else {
            self.status = Some(("jump list is empty".into(), false));
            return;
        };
        if let Some(here) = self.here() {
            self.fwd.push(here);
        }
        self.goto_loc(loc);
    }

    fn jump_forward(&mut self) {
        let Some(loc) = self.fwd.pop() else { return };
        if let Some(here) = self.here() {
            self.back.push(here);
        }
        self.goto_loc(loc);
    }

    fn step_file(&mut self, delta: isize) {
        let paths: Vec<String> = self.changed.iter().filter(|c| c.status != 'D').map(|c| c.path.clone()).collect();
        if paths.is_empty() {
            self.status = Some(("no changed files".into(), false));
            return;
        }
        let cur = self.current.as_ref().and_then(|c| paths.iter().position(|p| p == c));
        let next = match cur {
            Some(i) => (i as isize + delta).rem_euclid(paths.len() as isize) as usize,
            None => 0,
        };
        self.outline = false;
        self.open(&paths[next].clone(), None, true);
    }

    fn step_hunk(&mut self, fwd: bool) {
        if !self.diff {
            self.status = Some(("]c / [c move between changes in diff mode (⌘D)".into(), false));
            return;
        }
        let starts = self.doc.hunk_starts();
        let cur = self.vim.line;
        let target = if fwd { starts.iter().find(|&&s| s > cur).copied() } else { starts.iter().rev().find(|&&s| s < cur).copied() };
        match target {
            Some(t) => self.vim.set_cursor(t, 0),
            None if fwd => self.step_file(1),
            None => self.status = Some(("first change".into(), false)),
        }
    }

    fn handle_actions(&mut self, actions: Vec<VimAction>) {
        for a in actions {
            match a {
                VimAction::JumpToDef(name) => self.jump_to_def(name),
                VimAction::JumpBack => self.jump_back(),
                VimAction::JumpForward => self.jump_forward(),
                VimAction::Enter => {
                    if self.outline {
                        if let Some(line) = self.current_source_line() {
                            let path = self.current.clone().unwrap_or_default();
                            self.record_and_go(Loc { path, line, col: 0 });
                        }
                    }
                }
                VimAction::NextHunk => self.step_hunk(true),
                VimAction::PrevHunk => self.step_hunk(false),
                VimAction::NextFile => self.step_file(1),
                VimAction::PrevFile => self.step_file(-1),
                VimAction::BigMove { from } => {
                    if let (Some(path), Some(l)) = (self.current.clone(), self.doc.lines.get(from.0)) {
                        if let Some(line) = l.new_no {
                            let loc = Loc { path, line, col: from.1 };
                            if self.back.last() != Some(&loc) {
                                self.back.push(loc);
                            }
                        }
                    }
                }
                VimAction::Escape => self.status = None,
                VimAction::ScrollBy(_) | VimAction::Center | VimAction::Top | VimAction::Bottom => {}
            }
        }
    }

    // ------------------------------------------------------------ doc

    fn rebuild_doc(&mut self) {
        let Some(path) = self.current.clone() else {
            self.doc = Doc::default();
            return;
        };
        let has_base = self.bases.contains_key(&path);
        let key = (path.clone(), self.diff && has_base, self.outline, self.files.contains_key(&path));
        if self.doc_key.as_ref() == Some(&key) {
            if self.files.contains_key(&path) {
                if let Some((line, col)) = self.pending_goto.take() {
                    self.place_cursor(line, col);
                }
            }
            return;
        }
        // Keep the cursor on the same source line across rebuilds.
        let keep = self.pending_goto.take().or_else(|| {
            if self.doc_key.as_ref().map(|k| k.0 == path).unwrap_or(false) {
                self.current_source_line().map(|l| (l, self.vim.col))
            } else {
                None
            }
        });
        self.doc_key = Some(key);
        let Some(file) = self.files.get(&path) else {
            self.doc = Doc::default();
            self.pending_goto = keep;
            return;
        };
        self.outline_lines.clear();
        self.doc = if self.outline {
            let sums = self.summaries.get(&path);
            let mut d = Doc::default();
            for s in &file.symbols {
                let indent = "  ".repeat(s.depth());
                let text = format!("{indent}{}", s.signature);
                let summary = sums.and_then(|m| m.get(&s.path())).cloned().or_else(|| {
                    if self.summary_pending.contains(&path) { Some("summarizing…".into()) } else { None }
                });
                let mut line = DocLine::plain(text, s.line);
                line.suffix = summary.map(|t| format!("— {t}"));
                let sig_start = indent.len();
                line.spans = vec![(sig_start..line.text.len(), kind_color(&s.kind))];
                d.lines.push(line);
                self.outline_lines.push(s.line);
            }
            d
        } else if self.diff && has_base {
            let base = &self.bases[&path];
            Doc::diff(base.text.as_deref().unwrap_or(""), &file.text, base.spans.as_deref(), file.spans.as_deref())
        } else {
            Doc::plain(&file.text, file.spans.as_deref())
        };
        if let Some((line, col)) = keep {
            self.place_cursor(line, col);
        }
        self.vim.anchor = None;
        if self.outline {
            self.request_summaries(&path);
        }
    }

    /// Put the cursor on source line `line` (1-based), scrolling if needed.
    fn place_cursor(&mut self, line: usize, col: usize) {
        let idx = if self.outline {
            self.outline_lines.iter().rposition(|&l| l <= line).unwrap_or(0)
        } else {
            self.doc.index_of_new(line).unwrap_or(self.doc.lines.len().saturating_sub(1))
        };
        self.vim.set_cursor(idx, col);
        // rows is 0 before the first frame: assume a typical viewport.
        let rows = if self.view.rows == 0 { 30 } else { self.view.rows };
        if idx < self.view.top || idx >= self.view.top + rows {
            self.view.top = idx.saturating_sub(rows / 3);
        }
    }

    /// Fill summaries for the current file from the DB, and ask the model for
    /// the missing ones in the background.
    fn request_summaries(&mut self, path: &str) {
        let Some(file) = self.files.get(path) else { return };
        if self.summary_pending.contains(path) {
            return;
        }
        let mut have = self.summaries.remove(path).unwrap_or_default();
        let mut missing = Vec::new();
        {
            let db = self.engine.db();
            for s in &file.symbols {
                if have.contains_key(&s.path()) {
                    continue;
                }
                let key = summaries::symbol_key(path, s);
                let hash = summaries::symbol_hash(&file.text, s);
                match db.summary(&key, &hash) {
                    Ok(Some(sum)) => {
                        have.insert(s.path(), sum);
                    }
                    _ => missing.push(s.clone()),
                }
            }
        }
        self.summaries.insert(path.to_string(), have);
        if missing.is_empty() {
            return;
        }
        missing.truncate(MAX_SUMMARY_SYMBOLS);
        self.summary_pending.insert(path.to_string());
        self.doc_key = None;
        let (engine, tx, path_s, text, model) = (self.engine.clone(), self.tx.clone(), path.to_string(), file.text.clone(), self.engine.settings().summary_model);
        std::thread::spawn(move || {
            let refs: Vec<&Symbol> = missing.iter().collect();
            let (map, error) = match summaries::generate(&model, &path_s, &text, &refs) {
                Ok(map) => {
                    let db = engine.db();
                    for s in &missing {
                        if let Some(sum) = map.get(&s.path()) {
                            let _ = db.put_summary(&summaries::symbol_key(&path_s, s), &summaries::symbol_hash(&text, s), &path_s, sum, &model);
                        }
                    }
                    (map, None)
                }
                Err(e) => (HashMap::new(), Some(format!("{e:#}"))),
            };
            let _ = tx.send(Msg::Summaries { path: path_s, map, error });
            engine.ctx.request_repaint();
        });
    }

    // ------------------------------------------------------------ comments

    fn open_editor(&mut self) {
        if self.outline || self.doc.lines.is_empty() || self.current.is_none() {
            self.status = Some(("open a file (not the outline) to comment".into(), false));
            return;
        }
        let (start, end) = self.vim.visual_lines().unwrap_or((self.vim.line, self.vim.line));
        let anchor = self.anchor_for(start, end);
        let pr = review::pr_for_tab(&self.engine, &self.tab_id);
        let target = match &pr {
            Some(p) => format!("→ PR #{} on GitHub", p.number),
            None => "→ no open PR for this branch yet (posting will fail)".into(),
        };
        let lines = match anchor.start_line {
            Some(s) => format!("L{s}–{}", anchor.line.unwrap_or(s)),
            None => format!("L{}", anchor.line.unwrap_or(0)),
        };
        self.editor = Some(Editor {
            start,
            end,
            text: String::new(),
            label: format!("Comment on {} {lines} ({}) {target}", anchor.path, anchor.side),
            needs_focus: true,
        });
    }

    /// GitHub anchor for doc lines `start..=end`: LEFT (base) when the last
    /// line is a deleted line, RIGHT otherwise.
    fn anchor_for(&self, start: usize, end: usize) -> CommentAnchor {
        let path = self.current.clone().unwrap_or_default();
        let end_line = &self.doc.lines[end];
        let left = end_line.kind == LineKind::Removed;
        let no = |l: &DocLine| if left { l.old_no.filter(|_| l.kind != LineKind::Added) } else { l.new_no };
        let line = no(end_line).map(|n| n as i64);
        let start_line = self.doc.lines[start..=end].iter().find_map(no).map(|n| n as i64).filter(|s| Some(*s) != line);
        CommentAnchor { path, line, start_line, side: if left { "LEFT".into() } else { "RIGHT".into() } }
    }

    fn submit_editor(&mut self) {
        let Some(ed) = self.editor.take() else { return };
        let body = ed.text.trim().to_string();
        if body.is_empty() {
            self.editor = Some(ed);
            self.status = Some(("empty comment".into(), true));
            return;
        }
        let anchor = self.anchor_for(ed.start, ed.end);
        self.vim.anchor = None;
        self.status = Some(("posting comment…".into(), false));
        let (engine, tab, tx) = (self.engine.clone(), self.tab_id.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let r = review::post_comment(&engine, &tab, anchor, &body).map(|p| p.url).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Posted(r));
            engine.ctx.request_repaint();
        });
    }

    /// Inline notes for the current file: PR threads and agent drafts.
    fn blocks(&self) -> Vec<Block> {
        let Some(path) = &self.current else { return Vec::new() };
        if self.outline {
            return Vec::new();
        }
        let find = |side: &str, line: Option<i64>| -> Option<usize> {
            let n = line? as usize;
            if side == "LEFT" && self.diff {
                self.doc.index_of_old(n).or_else(|| self.doc.lines.iter().position(|l| l.old_no == Some(n)))
            } else {
                self.doc.lines.iter().position(|l| l.new_no == Some(n))
            }
        };
        let mut out = Vec::new();
        for t in review::threads_for_tab(&self.engine, &self.tab_id).iter().filter(|t| &t.path == path) {
            let after = find(&t.side, t.anchor_line());
            let first = t.comments.first();
            let title = format!(
                "{}{} · {} comment{}{}",
                if t.is_resolved { "✓ resolved · " } else { "" },
                first.map(|c| c.author.as_str()).unwrap_or("?"),
                t.comments.len(),
                if t.comments.len() == 1 { "" } else { "s" },
                if t.is_outdated { " · outdated" } else { "" }
            );
            let body = t.comments.iter().map(|c| format!("{}: {}", c.author, c.body.trim())).collect::<Vec<_>>().join("\n\n");
            out.push(Block { after, title, body, color: if t.is_resolved { theme::DIM } else { theme::ACCENT }, dim: t.is_resolved });
        }
        for d in review::drafts_for_tab(&self.engine, &self.tab_id).iter().filter(|d| &d.path == path && d.state == "draft") {
            let after = find(&d.side, d.line);
            let title = format!(
                "agent draft #{}{} — not public until approved in the inbox (⌘I)",
                d.id,
                d.severity.as_deref().map(|s| format!(" · {s}")).unwrap_or_default()
            );
            out.push(Block { after, title, body: d.body.clone(), color: theme::PURPLE, dim: false });
        }
        out
    }

    // ------------------------------------------------------------ panel API

    pub fn command(&mut self, cmd: PanelCmd, _engine: &Arc<Engine>) {
        match cmd {
            PanelCmd::Comment => self.open_editor(),
            PanelCmd::Submit => {
                if self.editor.is_some() {
                    // Submit after this frame's typing reaches the editor.
                    self.submit_pending = true;
                } else {
                    self.status = Some(("⌘M opens a comment field first".into(), false));
                }
            }
            PanelCmd::ToggleDiff => {
                self.diff = !self.diff;
                self.outline = false;
                self.doc_key = None;
                if let Some(p) = self.current.clone() {
                    if self.diff {
                        self.ensure_base(&p);
                    }
                }
            }
            PanelCmd::ToggleOutline => {
                self.outline = !self.outline;
                self.editor = None;
                self.doc_key = None;
            }
            PanelCmd::FilePicker => self.open_file_picker(),
            PanelCmd::Reload => {
                self.files.clear();
                self.bases.clear();
                self.load_errors.clear();
                self.doc_key = None;
                self.load_file_list();
                if let Some(p) = self.current.clone() {
                    self.ensure_loaded(&p);
                }
            }
            PanelCmd::Open { path, line, diff } => {
                if let Some(d) = diff {
                    self.diff = d;
                    self.doc_key = None;
                }
                match path {
                    Some(p) => {
                        let p = p.strip_prefix(&format!("{}/", self.worktree)).map(String::from).unwrap_or(p);
                        self.outline = false;
                        self.open(&p, line, true);
                    }
                    None => self.open_file_picker(),
                }
            }
        }
    }

    fn open_file_picker(&mut self) {
        let mut items: Vec<(String, String)> =
            self.changed.iter().map(|c| (c.path.clone(), format!("{} +{} −{}", c.status, c.added, c.removed))).collect();
        let changed: HashSet<&str> = self.changed.iter().map(|c| c.path.as_str()).collect();
        items.extend(self.all.iter().filter(|p| !changed.contains(p.as_str())).map(|p| (p.clone(), String::new())));
        self.picker = Some((PickerKind::Files, Picker::new("open file (PR files first)", items)));
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut PanelCtx) {
        self.poll();
        // The base may only become known after the file was opened.
        if let (true, Some(p)) = (self.diff, self.current.clone()) {
            self.ensure_base(&p);
        }
        self.rebuild_doc();
        if ui.input(|i| i.pointer.any_pressed()) && ui.rect_contains_pointer(ui.max_rect()) {
            cx.clicked = true;
        }
        let font_size = cx.engine.settings().font_size;

        // Header.
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let path = self.current.clone().unwrap_or_else(|| "no file — ⌘P to pick".into());
            ui.label(RichText::new(path).monospace().size(12.5).color(theme::TEXT));
            let mode = if self.outline { "OUTLINE" } else if self.diff { "DIFF" } else { "FILE" };
            ui.label(RichText::new(mode).size(10.5).strong().color(theme::BG).background_color(if self.diff { theme::GREEN } else { theme::CYAN }));
            if self.diff && !self.outline {
                let (a, d) = self.doc.changed_count();
                ui.label(RichText::new(format!("+{a} −{d}")).size(11.0).color(theme::DIM));
            }
            match review::pr_for_tab(cx.engine, &self.tab_id) {
                Some(pr) => ui.label(RichText::new(format!("PR #{}", pr.number)).size(11.0).color(theme::ACCENT)),
                None => ui.label(RichText::new("no PR").size(11.0).color(theme::DIM)),
            };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                let label = if self.show_files { "hide files" } else { "files" };
                if ui.add(egui::Button::new(RichText::new(label).size(11.0).color(theme::DIM)).frame(false)).clicked() {
                    self.show_files = !self.show_files;
                }
            });
        });
        ui.separator();

        // Footer (status line) first so the body gets the rest.
        let footer_h = 20.0;
        let body = ui.available_rect_before_wrap();
        let body_rect = egui::Rect::from_min_max(body.min, egui::pos2(body.max.x, body.max.y - footer_h));
        let footer_rect = egui::Rect::from_min_max(egui::pos2(body.min.x, body.max.y - footer_h), body.max);

        let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(body_rect));
        let mut code_rect = body_rect;
        if self.show_files && !self.outline {
            let list_w = (body_rect.width() * 0.28).clamp(150.0, 260.0);
            let list_rect = egui::Rect::from_min_size(body_rect.min, egui::vec2(list_w, body_rect.height()));
            code_rect = egui::Rect::from_min_max(egui::pos2(list_rect.right() + 1.0, body_rect.top()), body_rect.max);
            let mut list_ui = body_ui.new_child(egui::UiBuilder::new().max_rect(list_rect));
            self.file_list(&mut list_ui);
        }
        let mut code_ui = body_ui.new_child(egui::UiBuilder::new().max_rect(code_rect));
        let picked = self.picker.as_mut().map(|(_, picker)| picker.ui(&mut code_ui));
        match picked {
            None | Some(PickerResult::Open) => {}
            Some(PickerResult::Cancelled) => self.picker = None,
            Some(PickerResult::Chosen(i)) => match self.picker.take().map(|(k, _)| k) {
                Some(PickerKind::Files) => {
                    let path = picker_path(&self.changed, &self.all, i);
                    self.outline = false;
                    self.open(&path, None, true);
                }
                Some(PickerKind::Defs(locs)) => self.record_and_go(locs[i].clone()),
                None => {}
            },
        }
        self.rebuild_doc();
        let placeholder = match &self.current {
            None => Some(match &self.files_state {
                None => "loading changed files…".to_string(),
                Some(Err(e)) => format!("cannot list files: {e}"),
                Some(Ok(())) if self.changed.is_empty() => "no changes vs the base branch — ⌘P to open any file".into(),
                Some(Ok(())) => "⌘P to open a file".into(),
            }),
            Some(p) if !self.files.contains_key(p) => {
                Some(self.load_errors.get(p).map(|e| format!("cannot read {p}: {e}")).unwrap_or_else(|| format!("loading {p}…")))
            }
            _ => None,
        };
        if let Some(text) = placeholder {
            code_ui.centered_and_justified(|ui| ui.label(RichText::new(text).color(theme::DIM)));
        } else {
            let blocks = self.blocks();
            let focused = cx.focused && self.picker.is_none();
            let out = CodeView {
                doc: &self.doc,
                vim: &mut self.vim,
                state: &mut self.view,
                blocks: &blocks,
                editor: self.editor.as_mut(),
                focused,
                font_size,
                two_gutters: self.diff && !self.outline,
                id: egui::Id::new(("viewer", &self.tab_id)),
            }
            .show(&mut code_ui);
            if out.clicked {
                cx.clicked = true;
            }
            if out.cancel_editor {
                self.editor = None;
            }
            self.handle_actions(out.actions);
            if std::mem::take(&mut self.submit_pending) {
                self.submit_editor();
            }
        }

        // Status line.
        let busy = self.vim.search_input.is_some() || self.vim.message.is_some() || self.status.is_some() || self.vim.anchor.is_some();
        let mut footer = ui.new_child(egui::UiBuilder::new().max_rect(footer_rect));
        footer.horizontal_centered(|ui| {
            ui.add_space(8.0);
            if let Some((q, fwd)) = &self.vim.search_input {
                ui.label(RichText::new(format!("{}{q}▏", if *fwd { '/' } else { '?' })).monospace().size(12.0));
            } else if let Some(m) = &self.vim.message {
                ui.label(RichText::new(m).size(11.5).color(theme::YELLOW));
            } else if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).size(11.5).color(if *err { theme::RED } else { theme::YELLOW }));
            } else if self.vim.anchor.is_some() {
                ui.label(RichText::new("-- VISUAL --  ⌘M comment on selection").size(11.5).color(theme::PURPLE));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                let pos = format!("{}  L{}:{}", self.vim.pending_display(), self.current_source_line().unwrap_or(0), self.vim.col + 1);
                ui.label(RichText::new(pos).monospace().size(11.0).color(theme::DIM));
                if !busy {
                    ui.label(RichText::new("gd def · ^o back · ]c hunk · ]f file · ⌘D ⌘O ⌘M").size(10.5).color(theme::DIM));
                }
            });
        });
        ui.advance_cursor_after_rect(body);
    }

    fn file_list(&mut self, ui: &mut Ui) {
        ui.painter().rect_filled(ui.max_rect(), egui::CornerRadius::ZERO, theme::PANEL);
        let mut open: Option<String> = None;
        egui::ScrollArea::vertical().id_salt(("files", &self.tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("  PR FILES ({})", self.changed.len())).size(10.5).strong().color(theme::DIM));
            for c in &self.changed {
                let sel = self.current.as_deref() == Some(c.path.as_str());
                let color = match c.status {
                    'A' => theme::GREEN,
                    'D' => theme::RED,
                    'R' => theme::YELLOW,
                    _ => theme::ACCENT,
                };
                let name = c.path.rsplit('/').next().unwrap_or(&c.path);
                let mut job = egui::text::LayoutJob::default();
                let f = |col, size| egui::TextFormat { font_id: egui::FontId::proportional(size), color: col, ..Default::default() };
                job.append(&format!("{} ", c.status), 0.0, f(color, 11.0));
                job.append(name, 0.0, f(if sel { theme::TEXT } else { theme::TEXT.gamma_multiply(0.85) }, 12.0));
                job.append(&format!("  +{} −{}", c.added, c.removed), 0.0, f(theme::DIM, 10.5));
                let r = list_row(ui, sel, job).on_hover_text(&c.path);
                if r.clicked() && c.status != 'D' {
                    open = Some(c.path.clone());
                }
            }
            ui.add_space(8.0);
            let changed: HashSet<&str> = self.changed.iter().map(|c| c.path.as_str()).collect();
            let rest: Vec<&String> = self.all.iter().filter(|p| !changed.contains(p.as_str())).collect();
            ui.label(RichText::new(format!("  ALL FILES ({})", rest.len())).size(10.5).strong().color(theme::DIM));
            for p in rest.iter().take(2000) {
                let sel = self.current.as_deref() == Some(p.as_str());
                let color = if sel { theme::TEXT } else { theme::DIM };
                if list_row(ui, sel, RichText::new(p.as_str()).size(11.5).color(color)).clicked() {
                    open = Some((*p).clone());
                }
            }
            if rest.len() > 2000 {
                ui.label(RichText::new("… ⌘P to search the rest").size(11.0).color(theme::DIM));
            }
        });
        if let Some(p) = open {
            self.outline = false;
            self.open(&p, None, true);
        }
    }
}

/// A full-width, flat row in the file list.
pub(crate) fn list_row(ui: &mut Ui, selected: bool, text: impl Into<egui::WidgetText>) -> egui::Response {
    let fill = if selected { theme::SELECTED } else { egui::Color32::TRANSPARENT };
    ui.add(egui::Button::new(text).fill(fill).stroke(egui::Stroke::NONE).min_size(egui::vec2(ui.available_width(), 0.0)).wrap_mode(egui::TextWrapMode::Truncate))
}

fn picker_path(changed: &[ChangedFile], all: &[String], i: usize) -> String {
    if i < changed.len() {
        return changed[i].path.clone();
    }
    let set: HashSet<&str> = changed.iter().map(|c| c.path.as_str()).collect();
    all.iter().filter(|p| !set.contains(p.as_str())).nth(i - changed.len()).cloned().unwrap_or_default()
}

fn kind_color(kind: &str) -> egui::Color32 {
    match kind {
        "class" | "struct" | "enum" | "trait" | "interface" | "type" => theme::YELLOW,
        "impl" | "module" => theme::PURPLE,
        _ => theme::ACCENT,
    }
}

/// Repo-wide definition search: `git grep` for lines that mention `name`
/// next to a definition keyword, then parse just those files. Bounded, one
/// grep plus at most [`MAX_DEF_FILES`] reads — cheap over ssh too.
fn search_definitions(host: &Host, worktree: &str, name: &str, from: &str) -> Vec<(String, Symbol)> {
    let Ok(out) = host.run(&["git", "-C", worktree, "grep", "-n", "-I", "-w", "--no-color", "-e", name], None) else {
        return Vec::new();
    };
    let def_re = regex::Regex::new(&format!(
        r"\b(def|class|fn|func|function|struct|enum|trait|interface|type|const|let|var|impl|mod|module)\b.*\b{}\b",
        regex::escape(name)
    ))
    .expect("valid regex");
    let mut files: Vec<String> = Vec::new();
    for line in out.stdout.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(path), Some(_), Some(text)) = (parts.next(), parts.next(), parts.next()) else { continue };
        if def_re.is_match(text) && !files.iter().any(|f| f == path) {
            files.push(path.to_string());
        }
    }
    let dir = |p: &str| p.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
    let from_dir = dir(from);
    files.sort_by_key(|p| (p != from, dir(p) != from_dir, p.clone()));
    let mut hits = Vec::new();
    for path in files.into_iter().take(MAX_DEF_FILES) {
        let Ok(text) = host.read_file(&format!("{worktree}/{path}")) else { continue };
        for s in kitchen_symbols::extract(&path, &text) {
            if s.name == name {
                hits.push((path.clone(), s));
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_wide_definition_search() {
        let tmp = std::env::temp_dir().join(format!("kt-defs-{}", kitchen_core::util::new_id()));
        std::fs::create_dir_all(tmp.join("pkg")).unwrap();
        std::fs::write(tmp.join("pkg/a.py"), "from b import helper\nhelper()\n").unwrap();
        std::fs::write(tmp.join("pkg/b.py"), "def helper():\n    return 1\n").unwrap();
        std::fs::write(tmp.join("c.rs"), "fn helper() {}\n").unwrap();
        let d = tmp.display().to_string();
        let run = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&d).args(args).status().unwrap().success());
        run(&["init", "-q"]);
        run(&["add", "."]);
        let hits = search_definitions(&Host::Local, &d, "helper", "pkg/a.py");
        let found: Vec<(String, usize)> = hits.iter().map(|(p, s)| (p.clone(), s.line)).collect();
        assert_eq!(found, vec![("pkg/b.py".to_string(), 1), ("c.rs".to_string(), 1)]);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
