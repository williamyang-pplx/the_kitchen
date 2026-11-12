//! The file viewer (one per tab). With changes (a PR shown with ⌘⇧P, a tab the
//! agent worked in) it opens on the first changed file's diff, next to a
//! tree of just the changed files; without, on a folder view at a default
//! folder such as `aster/`. Plain / diff / outline modes, plugin
//! renderings of a file (the markdown plugin's ⌘⇧V), vim navigation, jump-to-definition with a jump list, ⌘M comments that
//! plugins post and draw (the github plugin: PR review comments), and ⌘⇧A questions the model answers under the line. All file access goes
//! through the workspace's [`Host`] on background threads, so devbox
//! workspaces work the same.

mod ask;
mod changes_tree;
pub mod code_view;
pub mod doc;
pub mod highlight;
pub mod markdown;
mod nav;
pub mod picker;
mod plugin_glue;
mod view_menu;
pub mod vim;

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

use egui::{RichText, Ui};
use kitchen_core::git::{ChangedFile, Listing, Repo};
use kitchen_core::host::Host;
use kitchen_core::summaries;
use kitchen_symbols::Symbol;

use crate::engine::{Engine, MenuRow, Shown, View};
use crate::links::Link;
use crate::panels::{PanelCmd, PanelCtx};
use crate::theme;
use crate::ui::widgets;
use crate::ui::widgets::HEADER_HEIGHT;
use code_view::{CodeView, ViewState};
use doc::{Doc, DocLine, Unfolded};
use highlight::Spans;
use picker::{Picker, PickerResult};
use vim::{Vim, VimAction};

/// Most symbols summarized per model call.
const MAX_SUMMARY_SYMBOLS: usize = 80;
/// Tracked-file lists kept by [`tracked_files`] (agi: ~14 MB each).
const TREES_KEPT: usize = 4;
/// A file read longer than this shows the frying pan instead of text.
const COOK_AFTER: std::time::Duration = std::time::Duration::from_millis(300);

struct FileData {
    text: String,
    spans: Option<Spans>,
    symbols: Vec<Symbol>,
    /// Content hash (plugins' decoration version).
    version: String,
    /// Size + mtime when read, to notice changes on disk.
    meta: String,
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

/// One row of the folder view.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    /// Shown text: `../`, `name/` or `name`.
    label: String,
    /// Worktree-relative path (a directory without trailing slash).
    path: String,
    is_dir: bool,
}

enum Msg {
    /// HEAD and the changes; `quiet` = a background refresh (keep the view as is).
    Files { listing: Listing, quiet: bool },
    /// Every file of the worktree at index checksum `index` (or why it couldn't be listed).
    Tree { index: String, result: Result<Arc<Vec<String>>, String> },
    FilesFailed(String),
    File { path: String, result: Result<FileData, String> },
    Base { path: String, data: BaseData },
    /// The text search's answer to nav query `token` (gd and gr).
    Grep { token: u64, locs: Vec<crate::plugins::Location> },
    Summaries { path: String, map: HashMap<String, String>, error: Option<String> },
    /// Result of a freshness check of `path` (`None`: could not stat).
    Stat { path: String, meta: Option<String> },
    /// A `gd`/`gr` answer (`None`: the provider couldn't tell).
    Nav { token: u64, answer: Option<Vec<crate::plugins::Location>> },
    /// The model's answer in a ⌘⇧A thread.
    Answer(crate::ask_thread::Answer),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum HunkGoto {
    First,
    Last,
}

/// `FileData::meta` of a deleted file (shown as all removed lines).
const DELETED: &str = "deleted";

enum PickerKind {
    Files,
    Defs(Vec<Loc>),
    /// Like `Defs`, with 1-based raw columns (from plugins / git grep).
    RawLocs(Vec<Loc>),
    /// ⌘⇧S: the menu the rows came from (None: no plugin answered yet) and
    /// each row's view, the worktree row included.
    Views { menu: Option<Arc<Vec<MenuRow>>>, views: Vec<(View, Shown)> },
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
    /// Every file (tracked, minus deleted, plus changed), sorted.
    all: Arc<Vec<String>>,
    /// State of `all` (the changes arrive first and are enough for a diff).
    files_state: Option<Result<(), String>>,
    /// Merge base of HEAD and origin/<default>, for diffs.
    merge_base: Option<String>,
    /// Index checksum of the last listing, and the one `all` was listed at.
    index: Option<String>,
    all_index: Option<String>,

    current: Option<String>,
    /// Folder shown in the file list, and in the main area while browsing.
    dir: String,
    /// The main area shows `dir` instead of the current file (`-` goes up).
    browsing: bool,
    /// Rows of the folder doc while browsing.
    entries: Vec<Entry>,
    /// Entry to put the cursor on once the folder doc is built.
    select_entry: Option<String>,
    /// The first file list decided where the viewer opens.
    routed: bool,
    /// Keyboard is in the file list (⌘⇧H), not the code (⌘⇧L).
    list_focus: bool,
    /// Keyboard cursor in the file list: changed files, then folder entries.
    list_cursor: usize,
    /// The list cursor moved: scroll it into view.
    list_scroll: bool,
    /// Pending `g` of `gg` in the file list.
    list_g: bool,
    /// The file list shows the repo's folders instead of the changed-files tree.
    list_all: bool,
    /// Where to put the cursor once the open file's diff is built.
    hunk_goto: Option<HunkGoto>,
    diff: bool,
    outline: bool,
    /// Show plugins' renderings of files that have one (`kitchen.render`).
    rendered: bool,
    /// The rendering the doc was built from.
    render_shown: Option<Arc<Vec<crate::plugins::decor::RenderRow>>>,
    files: HashMap<String, FileData>,
    bases: HashMap<String, BaseData>,
    loading: HashSet<String>,
    load_errors: HashMap<String, String>,

    doc: Doc,
    /// The folds opened in each file's diff, kept while the viewer lives.
    unfolded: HashMap<String, Unfolded>,
    doc_key: Option<(String, bool, bool, bool)>,
    /// The file `doc` shows and whether folded, also after `doc_key` is
    /// reset: a rebuild of the same file keeps the cursor on its line.
    doc_shown: Option<(String, bool)>,
    /// Outline row -> 1-based source line.
    outline_lines: Vec<usize>,
    vim: Vim,
    view: ViewState,
    pending_goto: Option<(usize, usize)>,
    back: Vec<Loc>,
    fwd: Vec<Loc>,

    picker: Option<(PickerKind, Picker)>,
    summaries: HashMap<String, HashMap<String, String>>,
    summary_pending: HashSet<String>,
    show_files: bool,
    status: Option<(String, bool)>,
    /// Paths being re-read because they changed on disk.
    reloading: HashSet<String>,
    stat_at: std::time::Instant,
    stat_pending: bool,
    changes_at: std::time::Instant,
    changes_pending: bool,
    overlay: Option<plugin_glue::OverlayCache>,
    /// Keys typed after the `\` leader, and when it was pressed.
    leader: Option<(String, std::time::Instant)>,
    /// The `gd`/`gr` query in flight (answers for older ones are dropped).
    pending_nav: Option<nav::PendingNav>,
    nav_token: u64,
    /// `pending_goto`'s column is a raw 1-based column, not a display one.
    goto_raw: bool,
    /// The ⌘M comment field, and the comment id it replies to.
    editor: Option<code_view::Editor>,
    reply_to: Option<String>,
    /// ⌘Enter arrived; submit once the editor has seen this frame's input.
    submit_pending: bool,
    /// ⌘⇧A threads, and the last one's id.
    asks: Vec<ask::Ask>,
    ask_seq: u64,
    /// The thread the open question field follows up on.
    follow_up: Option<u64>,
    /// A thread that just answered: its follow-up field may open.
    offered: Option<u64>,
    answerer: crate::ask_thread::Answerer,
    /// When ⌘⇧S last asked the plugins for the PR menu (until it changes).
    asked: Option<std::time::Instant>,
    /// Since when the code area has been waiting for files.
    loading_since: Option<std::time::Instant>,
}

impl Viewer {
    /// A viewer of the tab's own worktree (the app goes through `at` and its cache).
    #[cfg(test)]
    pub fn new(engine: &Arc<Engine>, ws: &crate::engine::Workspace, tab: &crate::engine::Tab) -> Self {
        Self::at(engine, ws.host(), &tab.row.id, tab.row.worktree.clone(), tab.diff_base(ws))
    }

    /// A viewer for tab `tab_id` on any worktree of its host (a PR's, `kitchen.view`).
    pub fn at(engine: &Arc<Engine>, host: Host, tab_id: &str, worktree: String, base_ref: String) -> Self {
        let (tx, rx) = channel();
        let mut v = Self {
            engine: engine.clone(),
            tab_id: tab_id.to_string(),
            host,
            worktree,
            base_ref,
            tx,
            rx,
            changed: Vec::new(),
            all: Arc::default(),
            files_state: None,
            merge_base: None,
            index: None,
            all_index: None,
            current: None,
            dir: String::new(),
            browsing: true,
            entries: Vec::new(),
            select_entry: None,
            routed: false,
            list_focus: false,
            list_cursor: 0,
            list_scroll: false,
            list_g: false,
            list_all: false,
            hunk_goto: None,
            diff: false,
            outline: false,
            rendered: false,
            render_shown: None,
            files: HashMap::new(),
            bases: HashMap::new(),
            loading: HashSet::new(),
            load_errors: HashMap::new(),
            doc: Doc::default(),
            unfolded: HashMap::new(),
            doc_key: None,
            doc_shown: None,
            outline_lines: Vec::new(),
            vim: Vim::default(),
            view: ViewState::default(),
            pending_goto: None,
            back: Vec::new(),
            fwd: Vec::new(),
            picker: None,
            summaries: HashMap::new(),
            summary_pending: HashSet::new(),
            show_files: true,
            status: None,
            reloading: HashSet::new(),
            stat_at: std::time::Instant::now(),
            changes_at: std::time::Instant::now(),
            changes_pending: false,
            stat_pending: false,
            overlay: None,
            leader: None,
            pending_nav: None,
            nav_token: 0,
            goto_raw: false,
            editor: None,
            reply_to: None,
            submit_pending: false,
            asks: Vec::new(),
            follow_up: None,
            offered: None,
            ask_seq: 0,
            answerer: kitchen_core::ask::ask,
            asked: None,
            loading_since: None,
        };
        v.load_file_list(false);
        v
    }

    // ------------------------------------------------------------ loading

    /// List HEAD and the changes, then (unless a quiet re-list found nothing
    /// new) every file. The changes come first: a diff doesn't wait for a big
    /// repo's file list.
    fn load_file_list(&mut self, quiet: bool) {
        if !quiet {
            self.files_state = None;
        }
        // Cleared when the listing lands; no re-list starts meanwhile.
        self.changes_pending = true;
        // A quiet re-list that finds the same index and changes sends no file
        // list, unless the last one failed (then it is what recovers).
        let known = (quiet && matches!(self.files_state, Some(Ok(())))).then(|| (self.all_index.clone(), self.changed.clone()));
        let (host, wt, base_ref, tx, ctx) =
            (self.host.clone(), self.worktree.clone(), self.base_ref.clone(), self.tx.clone(), self.engine.ctx.clone());
        std::thread::spawn(move || {
            let mut listing = match Repo::new(&host, &wt).listing(&wt, &base_ref) {
                Ok(l) => l,
                Err(e) => {
                    let _ = tx.send(Msg::FilesFailed(format!("{e:#}")));
                    ctx.request_repaint();
                    return;
                }
            };
            changes_tree::sort(&mut listing.changed);
            let same = known.is_some_and(|(index, changed)| index.as_deref() == Some(listing.index.as_str()) && changed == listing.changed);
            let (index, changed) = (listing.index.clone(), listing.changed.clone());
            let _ = tx.send(Msg::Files { listing, quiet });
            ctx.request_repaint();
            if same {
                return;
            }
            let result = tracked_files(&host, &wt, &index).map(|t| merge_all(&t, &changed)).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Tree { index, result });
            ctx.request_repaint();
        });
    }

    /// `path` on the host: worktree-relative, or already absolute (outside the worktree).
    fn abs_path(&self, path: &str) -> String {
        if path.starts_with('/') { path.to_string() } else { format!("{}/{}", self.worktree, path) }
    }

    fn ensure_loaded(&mut self, path: &str) {
        if self.files.contains_key(path) || self.loading.contains(path) || self.load_errors.contains_key(path) {
            return;
        }
        if self.changed.iter().any(|c| c.path == path && c.status == 'D') {
            let data = FileData { text: String::new(), spans: None, symbols: Vec::new(), version: String::new(), meta: DELETED.into() };
            self.files.insert(path.to_string(), data);
            self.doc_key = None;
            return;
        }
        self.read_file(path);
    }

    /// Read `path` in the background (the old contents stay shown meanwhile).
    fn read_file(&mut self, path: &str) {
        self.loading.insert(path.to_string());
        let (host, full, path_s, tx, ctx) =
            (self.host.clone(), self.abs_path(path), path.to_string(), self.tx.clone(), self.engine.ctx.clone());
        std::thread::spawn(move || {
            let result = plugin_glue::read_with_meta(&host, &full).map_err(|e| format!("{e:#}")).map(|(meta, text)| FileData {
                spans: highlight::highlight(&path_s, &text),
                symbols: kitchen_symbols::extract(&path_s, &text),
                version: kitchen_core::util::content_hash(&text),
                meta,
                text,
            });
            let _ = tx.send(Msg::File { path: path_s, result });
            ctx.request_repaint();
        });
    }

    fn ensure_base(&mut self, path: &str) {
        let key = format!("base:{path}");
        let Some(base) = self.merge_base.clone().filter(|_| !path.starts_with('/')) else { return };
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
                Msg::Files { listing, quiet: true } => {
                    self.changes_pending = false;
                    self.index = Some(listing.index);
                    let base_moved = self.merge_base.as_deref() != Some(listing.base.as_str());
                    if !base_moved && listing.changed == self.changed {
                        continue;
                    }
                    if base_moved {
                        self.bases.clear();
                        self.merge_base = Some(listing.base);
                        if !self.browsing {
                            self.doc_key = None;
                        }
                    }
                    self.changed = listing.changed;
                    // A deleted file may be back.
                    self.files.retain(|_, f| f.meta != DELETED);
                }
                Msg::Files { listing, .. } => {
                    self.changes_pending = false;
                    self.changed = listing.changed;
                    self.merge_base = Some(listing.base);
                    self.index = Some(listing.index);
                    self.doc_key = None;
                    // A diff needs only the changes; an empty list waits for the files.
                    if !self.changed.is_empty() && !std::mem::replace(&mut self.routed, true) && self.current.is_none() {
                        self.route();
                    }
                    self.refresh_picker_items();
                }
                Msg::Tree { index, result } => {
                    if self.index.as_deref() != Some(index.as_str()) {
                        continue; // an older listing's
                    }
                    match result {
                        Ok(all) => {
                            if self.browsing && self.files_state.is_some() && *all != *self.all {
                                // Rebuild the folder view, cursor on the same entry.
                                self.select_entry = self.entries.get(self.vim.line).map(|e| e.path.clone());
                            }
                            self.all = all;
                            self.all_index = Some(index);
                            self.files_state = Some(Ok(()));
                        }
                        Err(e) => self.files_state = Some(Err(e)),
                    }
                    if self.browsing {
                        self.doc_key = None;
                    }
                    if !std::mem::replace(&mut self.routed, true) && self.current.is_none() {
                        self.route();
                    }
                    self.refresh_picker_items();
                }
                Msg::FilesFailed(e) => {
                    self.changes_pending = false;
                    self.files_state = Some(Err(e));
                }
                Msg::File { path, result } => {
                    self.loading.remove(&path);
                    match result {
                        Ok(data) => {
                            let changed = self.reloading.remove(&path);
                            let same = self.files.get(&path).is_some_and(|old| old.version == data.version);
                            if !(changed && same) {
                                if let Some(p) = self.engine.plugins() {
                                    p.file_loaded(self.file_key(&path), std::sync::Arc::from(data.text.as_str()), changed);
                                }
                            }
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
                Msg::Grep { token, locs } => self.finish_grep(token, locs),
                Msg::Nav { token, answer } => self.finish_nav(token, answer),
                Msg::Answer(ans) => self.finish_ask(ans),
                Msg::Summaries { path, map, error } => {
                    self.summary_pending.remove(&path);
                    if let Some(e) = error {
                        self.status = Some((format!("summaries: {e}"), true));
                    }
                    self.summaries.entry(path).or_default().extend(map);
                    self.doc_key = None;
                }
                Msg::Stat { path, meta } => {
                    self.stat_pending = false;
                    let known = self.files.get(&path).map(|f| f.meta.clone());
                    if meta.is_some() && known.is_some() && meta != known && !self.loading.contains(&path) {
                        self.reloading.insert(path.clone());
                        self.read_file(&path);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ navigation

    fn here(&self) -> Option<Loc> {
        if self.browsing {
            return None;
        }
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
        l.first_no().or_else(|| {
            // Removed line: the next current-file line.
            self.doc.lines[self.vim.line..].iter().find_map(|l| l.first_no())
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
        self.browsing = false;
        self.goto_raw = false;
        // Files outside the worktree (an installed package) keep the list where it was.
        if !path.starts_with('/') {
            self.dir = parent_dir(path).to_string();
        }
        if self.current.as_deref() != Some(path) {
            self.current = Some(path.to_string());
            self.editor = None;
            self.vim.anchor = None;
            self.doc_key = None;
            self.pending_goto = Some((line.unwrap_or(1), 0));
            let changed = self.changed.iter().any(|c| c.path == path);
            self.hunk_goto = (line.is_none() && changed).then_some(HunkGoto::First);
        } else if let Some(l) = line {
            self.pending_goto = Some((l, 0));
            // An explicit line beats a first change still waiting for the base.
            self.hunk_goto = None;
        }
        self.outline = self.outline && line.is_none();
        self.ensure_loaded(path);
        if self.diff {
            self.ensure_base(path);
        }
    }

    /// Where a fresh viewer opens: the first changed file's diff, or the
    /// first configured default folder that exists (e.g. `aster/`).
    fn route(&mut self) {
        if !self.changed.is_empty() {
            self.diff = true;
            self.open_changed(0, false);
            return;
        }
        let defaults = self.engine.settings().viewer_default_dirs;
        let dir = defaults
            .iter()
            .map(|d| d.trim_matches('/'))
            .find(|d| !d.is_empty() && self.all.iter().any(|p| p.starts_with(&format!("{d}/"))))
            .unwrap_or("")
            .to_string();
        self.browse(&dir, None);
    }

    /// Show folder `dir` in the main area, cursor on `select` if given.
    fn browse(&mut self, dir: &str, select: Option<String>) {
        self.dir = dir.trim_matches('/').to_string();
        self.browsing = true;
        self.outline = false;
        self.vim.anchor = None;
        self.doc_key = None;
        self.select_entry = select;
        self.vim.set_cursor(0, 0);
        self.view.scroll_to(0);
    }

    /// `-`: up one folder (from a file: its folder).
    fn browse_parent(&mut self) {
        if self.browsing {
            if self.dir.is_empty() {
                return;
            }
            let from = self.dir.clone();
            self.browse(&parent_dir(&from).to_string(), Some(from));
        } else if let Some(cur) = self.current.clone() {
            self.browse(&parent_dir(&cur).to_string(), Some(cur));
        } else {
            self.browse("", None);
        }
    }

    fn open_entry(&mut self, idx: usize) {
        let Some(e) = self.entries.get(idx).cloned() else { return };
        if e.is_dir {
            let select = if e.label == "../" { Some(self.dir.clone()) } else { None };
            self.browse(&e.path, select);
        } else {
            self.outline = false;
            self.open(&e.path, None, true);
        }
    }

    fn goto_loc(&mut self, loc: Loc) {
        // A line, so a changed file doesn't go to its first change (and the outline closes).
        self.open(&loc.path, Some(loc.line), false);
        self.pending_goto = Some((loc.line, loc.col));
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

    /// ⌘⇧J / ⌘⇧K, `]f` / `[f`: the next / previous changed file (wrapping).
    fn step_file(&mut self, delta: isize) {
        let n = self.changed.len();
        if n == 0 {
            self.status = Some(("no changed files".into(), false));
            return;
        }
        let next = match self.current_changed() {
            Some(i) => (i as isize + delta).rem_euclid(n as isize) as usize,
            None if delta < 0 => n - 1,
            None => 0,
        };
        self.open_changed(next, true);
    }

    /// Index of the open file among the changed files.
    fn current_changed(&self) -> Option<usize> {
        let cur = self.current.as_deref().filter(|_| !self.browsing)?;
        self.changed.iter().position(|c| c.path == cur)
    }

    fn open_changed(&mut self, i: usize, record: bool) {
        let Some(path) = self.changed.get(i).map(|c| c.path.clone()) else { return };
        self.outline = false;
        self.open(&path, None, record);
        self.list_scroll = true;
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
            Some(t) => self.cursor_to_row(t),
            None if fwd => self.step_file(1),
            // Back past the first change: the previous file's last one.
            None if self.changed.len() > 1 || self.current_changed().is_none() => {
                self.step_file(-1);
                self.hunk_goto = Some(HunkGoto::Last);
            }
            None => self.status = Some(("first change".into(), false)),
        }
    }

    fn handle_actions(&mut self, actions: Vec<VimAction>) {
        for a in actions {
            match a {
                VimAction::Enter if self.browsing => self.open_entry(self.vim.line),
                VimAction::Parent => self.browse_parent(),
                VimAction::NextDiag => self.step_diag(true),
                VimAction::PrevDiag => self.step_diag(false),
                VimAction::JumpToDef(_)
                | VimAction::References(_)
                | VimAction::NextHunk
                | VimAction::PrevHunk
                | VimAction::OpenFold
                | VimAction::OpenAllFolds
                | VimAction::CloseAllFolds
                    if self.browsing => {}
                VimAction::JumpToDef(name) => self.jump_to_def(name),
                VimAction::References(name) => self.find_references(name),
                VimAction::JumpBack => self.jump_back(),
                VimAction::JumpForward => self.jump_forward(),
                VimAction::Enter if self.open_fold_at(self.vim.line) => {}
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
                VimAction::OpenFold => {
                    if !self.open_fold_at(self.vim.line) {
                        self.status = Some(("no fold under the cursor".into(), false));
                    }
                }
                VimAction::OpenAllFolds => self.set_all_folds(true),
                VimAction::CloseAllFolds => self.set_all_folds(false),
                VimAction::NextFile => self.step_file(1),
                VimAction::PrevFile => self.step_file(-1),
                VimAction::BigMove { from } => {
                    if let (Some(path), Some(l)) = (self.current.clone(), self.doc.lines.get(from.0)) {
                        if let Some(line) = l.first_no() {
                            let loc = Loc { path, line, col: from.1 };
                            if self.back.last() != Some(&loc) {
                                self.back.push(loc);
                            }
                        }
                    }
                }
                VimAction::Escape => {
                    self.close_asks();
                    self.status = None;
                }
                VimAction::ScrollBy(_) | VimAction::Center | VimAction::Top | VimAction::Bottom => {}
            }
        }
    }

    // ------------------------------------------------------------ doc

    fn rebuild_doc(&mut self) {
        if self.browsing {
            let key = (format!("\0{}", self.dir), false, false, self.files_state.is_some());
            if self.doc_key.as_ref() != Some(&key) {
                self.doc_key = Some(key);
                self.doc_shown = None;
                self.entries = dir_entries(&self.all, &self.dir);
                self.doc = folder_doc(&self.entries, &self.changed);
                let sel = self.select_entry.take();
                let idx = sel.and_then(|s| self.entries.iter().position(|e| e.path == s)).unwrap_or(if self.dir.is_empty() { 0 } else { 1.min(self.entries.len().saturating_sub(1)) });
                self.vim.set_cursor(idx, 0);
                let rows = if self.view.rows == 0 { 30 } else { self.view.rows };
                self.view.scroll_to(idx.saturating_sub(rows.saturating_sub(3)));
            }
            return;
        }
        let Some(path) = self.current.clone() else {
            self.doc = Doc::default();
            self.doc_shown = None;
            return;
        };
        let rows = self.render_rows(&path).filter(|_| !self.outline);
        if !opt_ptr_eq(&rows, &self.render_shown) {
            self.doc_key = None;
        }
        let has_base = self.bases.contains_key(&path);
        let key = (path.clone(), self.diff && has_base, self.outline, self.files.contains_key(&path));
        if self.doc_key.as_ref() == Some(&key) {
            if self.files.contains_key(&path) {
                if let Some((line, col)) = self.pending_goto.take() {
                    let col = self.goto_col(line, col);
                    self.place_cursor(line, col);
                }
            }
            return;
        }
        // Keep the cursor on the same source line across rebuilds. The fold
        // hiding it opens for a goto, or when the file showed unfolded until
        // now (its base just arrived, ⌘D); not after zM, and not for the
        // line-1 stand-in open() queues next to a first-change goto.
        let folding = self.diff && has_base && !self.outline;
        let shown = self.doc_shown.take().filter(|(p, _)| *p == path);
        let unfold = self.hunk_goto.is_none() && (self.pending_goto.is_some() || shown.as_ref().is_some_and(|s| !s.1));
        let keep = self.pending_goto.take().or_else(|| {
            if shown.is_some() {
                self.current_source_line().map(|l| (l, self.vim.col))
            } else {
                None
            }
        });
        self.doc_key = Some(key);
        self.doc_shown = Some((path.clone(), folding));
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
            let start = std::time::Instant::now();
            let doc = Doc::diff(base.text.as_deref().unwrap_or(""), &file.text, base.spans.as_deref(), file.spans.as_deref());
            log::debug!(target: "kitchen::viewer", "{:.0}ms diff {path} ({} rows)", start.elapsed().as_secs_f64() * 1000.0, doc.lines.len());
            doc
        } else {
            Doc::plain(&file.text, file.spans.as_deref())
        };
        if let Some(rows) = &rows {
            self.doc = Doc::rendered(&self.doc, rows);
        }
        self.render_shown = rows;
        if folding {
            self.doc = self.doc.folded(self.unfolded.get(&path).unwrap_or(&Unfolded::default()));
        }
        if let Some((line, col)) = keep {
            let col = self.goto_col(line, col);
            if unfold {
                self.place_cursor(line, col);
            } else {
                self.land_cursor(line, col);
            }
        }
        if !self.diff || self.outline {
            self.hunk_goto = None;
        } else if has_base {
            let starts = self.doc.hunk_starts();
            let row = match self.hunk_goto.take() {
                Some(HunkGoto::First) => starts.first().copied(),
                Some(HunkGoto::Last) => starts.last().copied(),
                None => None,
            };
            if let Some(r) = row {
                self.cursor_to_row(r);
            }
        }
        self.vim.anchor = None;
        if self.outline {
            self.request_summaries(&path);
        }
    }

    /// The rendering of `path` to show: rendered mode is on and a plugin
    /// rendered the text the viewer holds.
    fn render_rows(&self, path: &str) -> Option<Arc<Vec<crate::plugins::decor::RenderRow>>> {
        let file = self.files.get(path).filter(|_| self.rendered)?;
        self.engine.plugins()?.decor.get(&self.file_key(path))?.render(&file.version).cloned()
    }

    /// Put the cursor on source line `line` (1-based), scrolling if needed. A
    /// line inside a fold opens the fold; the cursor goes there once the doc
    /// is rebuilt (next frame).
    fn place_cursor(&mut self, line: usize, col: usize) {
        let fold = self.doc.index_of_new(line).filter(|&r| !self.outline && self.doc.lines[r].fold.is_some());
        if fold.is_some_and(|r| self.open_fold_at(r)) {
            self.pending_goto = Some((line, col));
            return;
        }
        self.land_cursor(line, col);
    }

    /// Put the cursor on the row showing source line `line` (a fold row if it
    /// is folded), scrolling if needed.
    fn land_cursor(&mut self, line: usize, col: usize) {
        let idx = if self.outline {
            self.outline_lines.iter().rposition(|&l| l <= line).unwrap_or(0)
        } else {
            self.doc.index_of_new(line).unwrap_or(self.doc.lines.len().saturating_sub(1))
        };
        // A fold row's text is empty.
        let col = if self.doc.lines.get(idx).is_some_and(|l| l.fold.is_some()) { 0 } else { col };
        self.vim.set_cursor(idx, col);
        self.scroll_to_row(idx);
    }

    /// Open the fold on doc row `row`, if there is one; the next rebuild
    /// shows its lines and keeps the cursor line. False when `row` is not a
    /// fold, or while the ⌘M / ⌘⇧A field is open (its rows are doc indices
    /// a rebuild would move).
    fn open_fold_at(&mut self, row: usize) -> bool {
        let (Some(f), Some(path)) = (self.doc.lines.get(row).and_then(|l| l.fold), self.current.clone()) else { return false };
        if self.editor.is_some() {
            return false;
        }
        self.unfolded.entry(path).or_default().opened.insert(f.first);
        self.doc_key = None;
        true
    }

    /// zR (`open`) / zM: open every fold in the file, or fold it all again.
    fn set_all_folds(&mut self, open: bool) {
        let diff = !self.outline && self.doc_key.as_ref().is_some_and(|k| k.1);
        let Some(path) = self.current.clone().filter(|_| diff) else {
            self.status = Some(("folds are for diffs (⌘D)".into(), false));
            return;
        };
        if self.editor.is_some() {
            return;
        }
        let u = self.unfolded.entry(path).or_default();
        u.all = open;
        if !open {
            u.opened.clear();
        }
        self.doc_key = None;
    }

    /// Put the cursor on document row `idx`, a third of the way down.
    fn cursor_to_row(&mut self, idx: usize) {
        self.vim.set_cursor(idx, 0);
        self.scroll_to_row(idx);
    }

    fn scroll_to_row(&mut self, idx: usize) {
        // rows is 0 before the first frame: assume a typical viewport.
        let rows = if self.view.rows == 0 { 30 } else { self.view.rows };
        if idx < self.view.top || idx >= self.view.top + rows {
            self.view.scroll_to(idx.saturating_sub(rows / 3));
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

    // ------------------------------------------------------------ panel API

    pub fn command(&mut self, cmd: PanelCmd, _engine: &Arc<Engine>) {
        match cmd {
            PanelCmd::Comment => self.start_comment(),
            PanelCmd::Ask => self.start_ask(),
            // Submit after this frame's typing reaches the editor.
            PanelCmd::Submit if self.editor.is_some() => self.submit_pending = true,
            PanelCmd::Submit => self.status = Some(("⌘M comments on the cursor line (V: a range)".into(), false)),
            PanelCmd::ToggleDiff => {
                self.diff = !self.diff;
                self.outline = false;
                self.doc_key = None;
                if let Some(p) = self.current.clone() {
                    if self.diff {
                        self.ensure_base(&p);
                    }
                }
                if self.browsing {
                    let mode = if self.diff { "diff" } else { "plain" };
                    self.status = Some((format!("files open in {mode} mode"), false));
                }
            }
            PanelCmd::ToggleOutline if self.browsing => self.status = Some(("open a file for its outline".into(), false)),
            PanelCmd::ToggleOutline => {
                self.outline = !self.outline;
                self.doc_key = None;
            }
            PanelCmd::Render(on) => {
                self.rendered = on.unwrap_or(!self.rendered);
                self.doc_key = None;
                if self.rendered && !self.browsing {
                    self.outline = false;
                }
                let path = self.current.clone().filter(|_| !self.browsing);
                self.status = match path {
                    Some(p) if self.rendered && self.files.contains_key(&p) && self.render_rows(&p).is_none() => {
                        Some((format!("no plugin rendered {p}; it shows as text"), false))
                    }
                    _ if self.browsing => Some((format!("files open {}", if self.rendered { "rendered" } else { "as text" }), false)),
                    _ => None,
                };
            }
            PanelCmd::FilePicker => self.open_file_picker(),
            PanelCmd::FocusList => {
                if !self.list_focus {
                    self.list_focus = true;
                    self.show_files = true;
                    self.outline = false;
                    self.list_cursor = self.list_home();
                    self.list_scroll = true;
                }
            }
            PanelCmd::FocusCode => self.list_focus = false,
            PanelCmd::ToggleFiles => {
                self.show_files = !self.show_files;
                self.list_focus = self.list_focus && self.show_files;
            }
            PanelCmd::NextFile => self.step_file(1),
            PanelCmd::PrevFile => self.step_file(-1),
            PanelCmd::Diagnostics => self.open_diagnostics_picker(),
            PanelCmd::Reload => {
                self.files.clear();
                self.bases.clear();
                self.load_errors.clear();
                self.doc_key = None;
                self.load_file_list(false);
                if let Some(p) = self.current.clone() {
                    self.ensure_loaded(&p);
                }
            }
            PanelCmd::ViewMenu => self.open_view_menu(),
            PanelCmd::Open { path, line, diff } => {
                if let Some(d) = diff {
                    self.diff = d;
                    self.doc_key = None;
                }
                match path {
                    // A folder (trailing `/`, the worktree itself included).
                    Some(p) if p.ends_with('/') => {
                        let wt = format!("{}/", self.worktree.trim_end_matches('/'));
                        let dir = p.strip_prefix(&wt).unwrap_or(&p);
                        self.routed = true;
                        self.browse(dir, None);
                    }
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

    /// An open file picker follows the file list.
    fn refresh_picker_items(&mut self) {
        if let Some((PickerKind::Files, picker)) = &mut self.picker {
            let (items, bonus) = file_items(&self.changed, &self.all, &self.dir);
            picker.set_items(items, bonus);
            picker.loading = self.files_state.is_none();
        }
    }

    fn open_file_picker(&mut self) {
        let (items, bonus) = file_items(&self.changed, &self.all, &self.dir);
        let mut picker = Picker::with_bonus("Open file", items, bonus);
        picker.hint = "Changed files and this folder first".into();
        picker.loading = self.files_state.is_none();
        self.picker = Some((PickerKind::Files, picker));
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut PanelCtx) {
        self.poll();
        self.check_nav_timeout();
        self.check_fresh();
        self.check_changes();
        self.refresh_overlay();
        self.refresh_view_menu(ui.ctx());
        // The base may only become known after the file was opened.
        if let (true, Some(p)) = (self.diff, self.current.clone()) {
            self.ensure_base(&p);
        }
        self.rebuild_doc();
        if ui.input(|i| i.pointer.any_pressed()) && ui.rect_contains_pointer(ui.max_rect()) {
            cx.clicked = true;
        }
        let font_size = cx.engine.settings().font_size;

        // Header: the path (folders dimmer than the name), what is shown, and the file list toggle.
        let header = ui.available_rect_before_wrap();
        let header = egui::Rect::from_min_size(header.min, egui::vec2(header.width(), HEADER_HEIGHT));
        let mut header_ui = ui.new_child(egui::UiBuilder::new().max_rect(header).layout(egui::Layout::left_to_right(egui::Align::Center)));
        header_ui.add_space(theme::SPACE_M);
        let path = if self.browsing {
            format!("{}/", self.dir)
        } else {
            self.current.clone().unwrap_or_else(|| "no file — ⌘P to pick".into())
        };
        header_ui.label(widgets::path_job(&path, 12.5));
        header_ui.add_space(theme::SPACE_XS);
        let (mode, color) = match (self.browsing, self.outline, self.diff) {
            (true, ..) => ("FOLDER", theme::PURPLE),
            (_, true, _) => ("OUTLINE", theme::CYAN),
            _ if self.render_shown.is_some() => ("RENDERED", theme::YELLOW),
            (_, _, true) => ("DIFF", theme::GREEN),
            _ => ("FILE", theme::CYAN),
        };
        widgets::pill(&mut header_ui, mode, color);
        if self.diff && !self.outline && !self.browsing {
            let (a, d) = self.doc.changed_count();
            header_ui.add_space(theme::SPACE_XS);
            header_ui.label(RichText::new(format!("+{a}")).font(theme::ui_font_semibold(theme::TEXT_SMALL)).color(theme::GREEN));
            header_ui.label(RichText::new(format!("−{d}")).font(theme::ui_font_semibold(theme::TEXT_SMALL)).color(theme::RED));
        }
        header_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(theme::SPACE_S);
            if widgets::flat_button(ui, "File list", self.show_files).on_hover_text("Show / hide the file list  ⌘B").clicked_by(egui::PointerButton::Primary) {
                self.show_files = !self.show_files;
            }
            // Which of the tab's PRs is on screen; a click switches.
            if let Some(label) = self.shown_entry_label() {
                if widgets::flat_button(ui, &label, false).on_hover_text("Switch PR  ⌘⇧S").clicked_by(egui::PointerButton::Primary) {
                    self.open_view_menu();
                }
            }
        });
        ui.painter().hline(header.x_range(), header.bottom() - 0.5, egui::Stroke::new(1.0, theme::BORDER));
        ui.advance_cursor_after_rect(header);

        // Footer (status line) first so the body gets the rest.
        let footer_h = 20.0;
        let body = ui.available_rect_before_wrap();
        let body_rect = egui::Rect::from_min_max(body.min, egui::pos2(body.max.x, body.max.y - footer_h));
        let footer_rect = egui::Rect::from_min_max(egui::pos2(body.min.x, body.max.y - footer_h), body.max);

        let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(body_rect));
        let mut code_rect = body_rect;
        if !self.show_files || self.outline {
            self.list_focus = false;
        }
        if self.show_files && !self.outline {
            let list_w = (body_rect.width() * 0.28).clamp(150.0, 260.0);
            let list_rect = egui::Rect::from_min_size(body_rect.min, egui::vec2(list_w, body_rect.height()));
            code_rect = egui::Rect::from_min_max(egui::pos2(list_rect.right() + 1.0, body_rect.top()), body_rect.max);
            let mut list_ui = body_ui.new_child(egui::UiBuilder::new().max_rect(list_rect));
            let keys = cx.focused && self.list_focus && self.picker.is_none();
            self.file_list(&mut list_ui, keys);
            // Mark which half of the viewer has the keyboard.
            if cx.focused && self.picker.is_none() {
                let r = if self.list_focus { list_rect } else { code_rect };
                ui.painter().rect_stroke(r.shrink(1.0), egui::CornerRadius::same(2), egui::Stroke::new(1.5, theme::FOCUS.gamma_multiply(0.7)), egui::StrokeKind::Inside);
            }
        }
        let mut code_ui = body_ui.new_child(egui::UiBuilder::new().max_rect(code_rect));
        // The picker floats over the code, centered near its top.
        let picked = self.picker.as_mut().map(|(_, picker)| {
            let width = (code_rect.width() - 48.0).clamp(320.0, 760.0);
            let pos = egui::pos2(code_rect.center().x - width / 2.0, code_rect.top() + 24.0);
            let max_height = (code_rect.height() - 48.0).clamp(160.0, 520.0);
            egui::Area::new(egui::Id::new(("picker", &self.tab_id)))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(ui.ctx(), |ui| {
                    ui.set_width(width);
                    ui.set_max_height(max_height);
                    picker.ui(ui, cx.focused)
                })
                .inner
        });
        if matches!(picked, Some(PickerResult::Cancelled | PickerResult::Chosen(_))) && self.provisional_picker() {
            // The user acted on the text matches: a late provider answer is dropped.
            self.pending_nav = None;
            self.status = None;
        }
        match picked {
            None | Some(PickerResult::Open) => {}
            Some(PickerResult::Cancelled) => self.picker = None,
            Some(PickerResult::Chosen(i)) => match self.picker.take() {
                Some((PickerKind::Files, picker)) => {
                    let path = picker.items[i].0.clone();
                    self.outline = false;
                    self.open(&path, None, true);
                }
                Some((PickerKind::Defs(locs), _)) => self.record_and_go(locs[i].clone()),
                Some((PickerKind::RawLocs(locs), _)) => self.record_and_go_raw(locs[i].clone()),
                Some((PickerKind::Views { mut views, .. }, _)) => {
                    let (view, at) = views.swap_remove(i);
                    if at != self.shown() {
                        cx.view = Some(view);
                    } else {
                        // Say so, or picking it looks like a switch that failed.
                        let name = self.shown_entry_label().map(|l| l.split(" · ").next().unwrap_or(&l).to_string());
                        self.status = Some((format!("{} is already on screen", name.as_deref().unwrap_or("this view")), false));
                    }
                }
                None => {}
            },
        }
        self.rebuild_doc();
        // (text, still loading)
        let placeholder = match (&self.current, &self.files_state) {
            (_, None) if self.browsing => Some(("loading files…".to_string(), true)),
            (_, Some(Err(e))) if self.browsing => Some((format!("cannot list files: {e}"), false)),
            _ if self.browsing => None,
            (None, _) => Some(("⌘P to open a file".into(), false)),
            (Some(p), _) if !self.files.contains_key(p) => Some(match self.load_errors.get(p) {
                Some(e) => (format!("cannot read {p}: {e}"), false),
                None => (format!("loading {p}…"), true),
            }),
            _ => None,
        };
        // The frying pan while a new viewer lists its files (it picks up
        // where a PR switch's pan left off), and for a file read once it
        // passes COOK_AFTER, so quick reads don't flash it.
        let loading = placeholder.as_ref().is_some_and(|(_, l)| *l);
        let since = *self.loading_since.get_or_insert_with(std::time::Instant::now);
        if !loading {
            self.loading_since = None;
        }
        if let Some((text, true)) = &placeholder
            && (self.files_state.is_none() || since.elapsed() >= COOK_AFTER)
        {
            let caption = self.shown_entry_label().map(|l| format!("cooking {}…", l.split(" · ").next().unwrap_or(&l))).unwrap_or_else(|| text.clone());
            crate::ui::cooking::cooking(&mut code_ui, &caption);
        } else if let Some((text, loading)) = placeholder {
            if loading {
                ui.ctx().request_repaint_after(COOK_AFTER);
            }
            code_ui.centered_and_justified(|ui| ui.label(RichText::new(text).color(theme::DIM)));
        } else {
            let focused = cx.focused && self.picker.is_none() && !self.list_focus;
            self.offer_follow_up(focused);
            if focused && self.editor.is_none() {
                self.leader_keys(&mut code_ui);
            }
            let comments = self.comment_blocks();
            let blocks: Vec<code_view::Block> = comments.into_iter().map(|(b, _)| b).chain(self.ask_blocks()).collect();
            let overlay = self.overlay.as_ref().filter(|o| !self.browsing && !self.outline && self.current.as_deref() == Some(o.path.as_str())).map(|o| &o.overlay);
            let mut out = CodeView {
                doc: &self.doc,
                vim: &mut self.vim,
                state: &mut self.view,
                blocks: &blocks,
                editor: self.editor.as_mut(),
                focused,
                font_size,
                two_gutters: self.diff && !self.outline && !self.browsing,
                relative: cx.engine.settings().relative_line_numbers,
                overlay,
                id: egui::Id::new(("viewer", &self.tab_id)),
            }
            .show(&mut code_ui);
            if out.clicked {
                cx.clicked = true;
                self.list_focus = false;
            }
            for url in out.urls {
                self.engine.open_link(&self.tab_id, Link::Url(url));
            }
            if out.cancel_editor {
                self.editor = None;
                // egui drops the field's focus on that Esc, so vim saw it
                // too: it must not also close the thread under the field.
                out.actions.retain(|a| !matches!(a, VimAction::Escape));
            }
            if std::mem::take(&mut self.submit_pending) || out.submit_editor {
                if self.editor.as_ref().is_some_and(|e| e.ask) {
                    self.submit_ask();
                } else {
                    self.submit_comment();
                }
            }
            self.handle_actions(out.actions);
        }

        // Status line.
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
                ui.label(RichText::new("-- VISUAL --").size(11.5).color(theme::PURPLE));
            } else if let Some((keys, _)) = &self.leader {
                ui.label(RichText::new(format!("\\{keys}")).monospace().size(12.0).color(theme::PURPLE));
            } else if let Some((text, color)) = self.cursor_diagnostic() {
                ui.label(RichText::new(text).size(11.5).color(color));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                let pos = format!("{}  L{}:{}", self.vim.pending_display(), self.current_source_line().unwrap_or(0), self.vim.col + 1);
                ui.label(RichText::new(pos).monospace().size(11.0).color(theme::SUBTLE));
            });
        });
        ui.advance_cursor_after_rect(body);
    }

    /// Folder entries shown under the changed files.
    fn list_entries(&self) -> Vec<Entry> {
        if self.browsing && self.doc_key.as_ref().is_some_and(|k| k.0 == format!("\0{}", self.dir)) {
            self.entries.clone()
        } else {
            dir_entries(&self.all, &self.dir)
        }
    }

    /// The file list is the changed-files tree (else the folder list).
    fn tree_mode(&self) -> bool {
        !self.list_all && !self.changed.is_empty()
    }

    /// Where the list cursor lands on ⌘⇧H: the open file, else the first folder entry.
    fn list_home(&self) -> usize {
        let entries = self.list_entries();
        let pr = self.changed.len();
        let cur = self.current.as_deref().filter(|_| !self.browsing);
        if let Some(i) = cur.and_then(|c| entries.iter().position(|e| e.path == c)) {
            return pr + i;
        }
        if let Some(i) = cur.and_then(|c| self.changed.iter().position(|e| e.path == c)) {
            return i;
        }
        let skip_up = entries.first().is_some_and(|e| e.label == "../") && entries.len() > 1;
        pr + skip_up as usize
    }

    /// Open list row `idx` (a changed file or a folder entry).
    fn activate_row(&mut self, idx: usize, entries: &[Entry]) {
        let pr = self.changed.len();
        if idx < pr {
            let c = &self.changed[idx];
            if c.status == 'D' {
                self.status = Some((format!("{} was deleted", c.path), false));
                return;
            }
            let p = c.path.clone();
            self.outline = false;
            self.open(&p, None, true);
            return;
        }
        let Some(e) = entries.get(idx - pr).cloned() else { return };
        if e.is_dir {
            let from = self.dir.clone();
            self.browse(&e.path, (e.path.len() < from.len()).then_some(from.clone()));
            // Cursor: the folder we came up from, else the first entry.
            let entries = dir_entries(&self.all, &e.path);
            let going_up = e.path.len() < from.len();
            let pos = entries
                .iter()
                .position(|x| going_up && x.label != "../" && x.path == from)
                .unwrap_or((!e.path.is_empty() && entries.len() > 1) as usize);
            self.list_cursor = pr + pos;
            self.list_scroll = true;
        } else {
            self.outline = false;
            self.open(&e.path, None, true);
        }
    }

    /// vim-style keys for the file list: j/k, gg/G, ctrl-d/u, Enter/l/o
    /// open, h/- up a folder.
    fn list_keys(&mut self, ui: &mut Ui, entries: &[Entry]) {
        let rows = self.changed.len() + entries.len();
        if rows == 0 {
            return;
        }
        let mut activate = false;
        let mut up = false;
        let keys = code_view::collect_keys(ui);
        for k in keys {
            use vim::VimKey as K;
            let g = std::mem::take(&mut self.list_g);
            let before = self.list_cursor;
            match k {
                K::Char('j') | K::Down => self.list_cursor = (self.list_cursor + 1).min(rows - 1),
                K::Char('k') | K::Up => self.list_cursor = self.list_cursor.saturating_sub(1),
                K::Ctrl('d') | K::PageDown => self.list_cursor = (self.list_cursor + 15).min(rows - 1),
                K::Ctrl('u') | K::PageUp => self.list_cursor = self.list_cursor.saturating_sub(15),
                K::Char('G') | K::End => self.list_cursor = rows - 1,
                K::Char('g') if g => self.list_cursor = 0,
                K::Char('g') => self.list_g = true,
                K::Home => self.list_cursor = 0,
                K::Enter | K::Char('l') | K::Char('o') | K::Right => activate = true,
                K::Char('h') | K::Char('-') | K::Left | K::Backspace => up = true,
                K::Char('a') if !self.changed.is_empty() => {
                    self.list_all = false;
                    self.list_scroll = true;
                    return;
                }
                K::Esc => self.list_focus = false,
                _ => {}
            }
            if self.list_cursor != before {
                self.list_scroll = true;
            }
        }
        self.list_cursor = self.list_cursor.min(rows - 1);
        if activate {
            self.activate_row(self.list_cursor, entries);
        } else if up && !self.dir.is_empty() {
            let parent = parent_dir(&self.dir).to_string();
            let pr = self.changed.len();
            let up_row = entries.iter().position(|e| e.label == "../").map(|i| pr + i);
            match up_row {
                Some(r) => self.activate_row(r, entries),
                None => self.browse(&parent, None),
            }
        }
    }

    /// Keys for the changed-files tree: j/k (gg/G, ctrl-d/u) open the
    /// next / previous changed file right away; Enter/l/Esc go to the code,
    /// `a` switches to the folder list.
    fn tree_keys(&mut self, ui: &mut Ui) {
        let last = self.changed.len().saturating_sub(1);
        for k in code_view::collect_keys(ui) {
            use vim::VimKey as K;
            let g = std::mem::take(&mut self.list_g);
            let at = self.current_changed();
            let target = match k {
                K::Char('j') | K::Down => Some(at.map_or(0, |i| (i + 1).min(last))),
                K::Char('k') | K::Up => Some(at.map_or(0, |i| i.saturating_sub(1))),
                K::Ctrl('d') | K::PageDown => Some(at.map_or(0, |i| (i + 15).min(last))),
                K::Ctrl('u') | K::PageUp => Some(at.map_or(0, |i| i.saturating_sub(15))),
                K::Char('G') | K::End => Some(last),
                K::Char('g') if g => Some(0),
                K::Char('g') => {
                    self.list_g = true;
                    None
                }
                K::Home => Some(0),
                K::Enter | K::Char('l') | K::Char('o') | K::Right | K::Esc => {
                    self.list_focus = false;
                    None
                }
                K::Char('a') => {
                    self.list_all = true;
                    self.list_cursor = self.list_home();
                    self.list_scroll = true;
                    return;
                }
                _ => None,
            };
            if let Some(t) = target.filter(|t| Some(*t) != at) {
                self.open_changed(t, false);
            }
        }
    }

    /// The changed files as a tree (see [`changes_tree`]); the open file is the cursor.
    fn changes_list(&mut self, ui: &mut Ui, keys: bool) {
        if keys {
            self.tree_keys(ui);
        }
        let cur = self.current_changed();
        let scroll = std::mem::take(&mut self.list_scroll);
        let paths: Vec<&str> = self.changed.iter().map(|c| c.path.as_str()).collect();
        let rows = changes_tree::rows(&paths);
        let mut clicked: Option<usize> = None;
        let mut all = false;
        egui::ScrollArea::vertical().id_salt(("changes", &self.tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(theme::SPACE_S);
            ui.horizontal(|ui| {
                ui.add_space(theme::SPACE_M);
                changed_header(ui, &self.changed);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(theme::SPACE_XS);
                    all = widgets::flat_button(ui, "All files", false).on_hover_text("The whole repo  a").clicked_by(egui::PointerButton::Primary);
                });
            });
            ui.add_space(2.0);
            let f = |col, size| egui::TextFormat { font_id: theme::ui_font(size), color: col, ..Default::default() };
            for r in &rows {
                let indent = r.depth as f32 * 12.0;
                let mut job = egui::text::LayoutJob::default();
                let Some(i) = r.file else {
                    job.append(&r.label, indent, f(theme::SUBTLE, theme::TEXT_SMALL));
                    list_row(ui, false, job);
                    continue;
                };
                let c = &self.changed[i];
                let open = cur == Some(i);
                job.append(&format!("{} ", c.status), indent, egui::TextFormat { font_id: egui::FontId::monospace(10.5), color: status_color(c.status), ..Default::default() });
                job.append(&r.label, 0.0, f(if open { theme::TEXT } else { theme::MUTED }, theme::TEXT_BODY));
                job.append(&format!("+{}", c.added), 8.0, f(theme::GREEN.gamma_multiply(0.75), theme::TEXT_SMALL));
                job.append(&format!("−{}", c.removed), 4.0, f(theme::RED.gamma_multiply(0.75), theme::TEXT_SMALL));
                let resp = row(ui, open && self.list_focus, open, job).on_hover_text(&c.path);
                if scroll && open {
                    resp.scroll_to_me(None);
                }
                if resp.clicked_by(egui::PointerButton::Primary) {
                    clicked = Some(i);
                }
            }
        });
        if let Some(i) = clicked {
            self.open_changed(i, true);
        }
        if all {
            self.list_all = true;
            self.list_cursor = self.list_home();
        }
    }

    /// `keys`: the list has the keyboard.
    fn file_list(&mut self, ui: &mut Ui, keys: bool) {
        let r = ui.max_rect();
        ui.painter().rect_filled(r, egui::CornerRadius::ZERO, theme::PANEL);
        ui.painter().vline(r.right() + 0.5, r.y_range(), egui::Stroke::new(1.0, theme::BORDER));
        if self.tree_mode() {
            return self.changes_list(ui, keys);
        }
        let entries = self.list_entries();
        if keys {
            self.list_keys(ui, &entries);
        }
        // The keys may have changed folders.
        let entries = self.list_entries();
        let pr = self.changed.len();
        self.list_cursor = self.list_cursor.min((pr + entries.len()).saturating_sub(1));
        let cursor = self.list_focus.then_some(self.list_cursor);
        let scroll = std::mem::take(&mut self.list_scroll);
        let mut clicked: Option<usize> = None;
        let mut browse: Option<String> = None;
        egui::ScrollArea::vertical().id_salt(("files", &self.tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(theme::SPACE_S);
            if !self.changed.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(theme::SPACE_M);
                    changed_header(ui, &self.changed).on_hover_text("a: back to the tree of changes");
                });
                ui.add_space(2.0);
            }
            for (i, c) in self.changed.iter().enumerate() {
                let open = self.current.as_deref() == Some(c.path.as_str()) && !self.browsing;
                let name = c.path.rsplit('/').next().unwrap_or(&c.path);
                let mut job = egui::text::LayoutJob::default();
                let f = |col, size| egui::TextFormat { font_id: theme::ui_font(size), color: col, ..Default::default() };
                job.append(&format!("{} ", c.status), 0.0, egui::TextFormat { font_id: egui::FontId::monospace(10.5), color: status_color(c.status), ..Default::default() });
                job.append(name, 0.0, f(if open { theme::TEXT } else { theme::MUTED }, theme::TEXT_BODY));
                job.append(&format!("+{}", c.added), 8.0, f(theme::GREEN.gamma_multiply(0.75), theme::TEXT_SMALL));
                job.append(&format!("−{}", c.removed), 4.0, f(theme::RED.gamma_multiply(0.75), theme::TEXT_SMALL));
                let r = row(ui, cursor == Some(i), open, job).on_hover_text(&c.path);
                if scroll && cursor == Some(i) {
                    r.scroll_to_me(None);
                }
                if r.clicked_by(egui::PointerButton::Primary) {
                    clicked = Some(i);
                }
            }
            ui.add_space(8.0);
            // Breadcrumb: each segment browses to that folder.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(8.0);
                let crumb = |ui: &mut Ui, text: &str| ui.add(egui::Button::new(RichText::new(text).font(theme::ui_font_semibold(theme::TEXT_LABEL)).color(theme::SUBTLE)).frame(false)).clicked_by(egui::PointerButton::Primary);
                if crumb(ui, "ROOT") {
                    browse = Some(String::new());
                }
                let mut acc = String::new();
                for seg in self.dir.split('/').filter(|s| !s.is_empty()) {
                    acc = if acc.is_empty() { seg.to_string() } else { format!("{acc}/{seg}") };
                    ui.label(RichText::new(" / ").size(10.5).color(theme::DIM));
                    if crumb(ui, seg) {
                        browse = Some(acc.clone());
                    }
                }
            });
            let changed_under = changed_counts(&self.changed);
            for (i, e) in entries.iter().enumerate() {
                let open = !e.is_dir && self.current.as_deref() == Some(e.path.as_str()) && !self.browsing;
                let n = if e.label == "../" { 0 } else { changed_under.get(e.path.as_str()).copied().unwrap_or(0) };
                let color = match (e.is_dir, n > 0) {
                    (true, _) => theme::ACCENT.gamma_multiply(0.9),
                    (false, true) => theme::YELLOW,
                    _ if open => theme::TEXT,
                    _ => theme::MUTED,
                };
                let text = if n > 0 && e.is_dir { format!("{}  ●{n}", e.label) } else { e.label.clone() };
                let r = row(ui, cursor == Some(pr + i), open, RichText::new(text).font(theme::ui_font(theme::TEXT_BODY)).color(color)).on_hover_text(&e.path);
                if scroll && cursor == Some(pr + i) {
                    r.scroll_to_me(None);
                }
                if r.clicked_by(egui::PointerButton::Primary) {
                    clicked = Some(pr + i);
                }
            }
            if entries.is_empty() && self.files_state.is_some() {
                ui.label(RichText::new("    (empty)").font(theme::ui_font(theme::TEXT_SMALL)).color(theme::SUBTLE));
            }
        });
        if let Some(i) = clicked {
            self.list_cursor = i;
            self.activate_row(i, &entries);
        }
        if let Some(d) = browse {
            let select = (d.len() < self.dir.len()).then(|| self.dir.clone());
            self.browse(&d, select);
        }
    }
}

/// Git status letter colors: added and deleted stand out, modified stays quiet.
/// "CHANGED FILES  n" and the lines the changes add and remove in all.
fn changed_header(ui: &mut Ui, changed: &[ChangedFile]) -> egui::Response {
    let mut job = widgets::section_label(&format!("Changed files  {}", changed.len()), theme::SUBTLE);
    let (added, removed) = diff_totals(changed);
    let f = |color| egui::TextFormat { font_id: theme::ui_font_semibold(theme::TEXT_SMALL), color, ..Default::default() };
    job.append(&format!("+{added}"), 8.0, f(theme::GREEN));
    job.append(&format!("−{removed}"), 4.0, f(theme::RED));
    ui.label(job)
}

/// Lines added and removed across `changed`.
fn diff_totals(changed: &[ChangedFile]) -> (u64, u64) {
    changed.iter().fold((0, 0), |(a, r), c| (a + u64::from(c.added), r + u64::from(c.removed)))
}

fn status_color(status: char) -> egui::Color32 {
    match status {
        'A' => theme::GREEN,
        'D' => theme::RED,
        'R' => theme::PURPLE,
        _ => theme::SUBTLE,
    }
}

/// A file-list row: `cursor` is the keyboard cursor, `open` the open file.
fn row(ui: &mut Ui, cursor: bool, open: bool, text: impl Into<egui::WidgetText>) -> egui::Response {
    let r = list_row(ui, cursor || open, text);
    if cursor {
        let bar = egui::Rect::from_min_size(r.rect.min + egui::vec2(4.0, 3.0), egui::vec2(2.5, r.rect.height() - 6.0));
        ui.painter().rect_filled(bar, egui::CornerRadius::ZERO, theme::FOCUS);
    }
    r
}

/// A full-width, flat, left-aligned row (file list, picker).
pub(crate) fn list_row(ui: &mut Ui, selected: bool, text: impl Into<egui::WidgetText>) -> egui::Response {
    let pad = egui::vec2(10.0, 3.0);
    let width = ui.available_width();
    let galley = text.into().into_galley(ui, Some(egui::TextWrapMode::Truncate), width - 2.0 * pad.x, egui::TextStyle::Button);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, galley.size().y + 2.0 * pad.y), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if selected {
            theme::SELECTED
        } else if resp.hovered() {
            theme::HOVER
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect.shrink2(egui::vec2(4.0, 0.0)), egui::CornerRadius::same(theme::RADIUS_S), fill);
        ui.painter().galley(rect.min + pad, galley, theme::TEXT);
    }
    resp
}

fn opt_ptr_eq<T>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn parent_dir(path: &str) -> &str {
    path.trim_end_matches('/').rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Rows of folder `dir`: `../`, subfolders, then files (both sorted).
/// Tracked files of `worktree` while its index checksum is `index`, shared
/// by every viewer on it.
fn tracked_files(host: &Host, worktree: &str, index: &str) -> anyhow::Result<Arc<Vec<String>>> {
    type Trees = Mutex<Vec<((Host, String, String), Arc<Vec<String>>)>>;
    static TREES: OnceLock<Trees> = OnceLock::new();
    let trees = TREES.get_or_init(Default::default);
    let key = (host.clone(), worktree.to_string(), index.to_string());
    if let Some((_, t)) = trees.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| *k == key) {
        return Ok(t.clone());
    }
    let files = Arc::new(Repo::new(host, worktree).tracked_files(worktree)?);
    let mut t = trees.lock().unwrap_or_else(|e| e.into_inner());
    t.retain(|(k, _)| *k != key);
    t.push((key, files.clone()));
    let n = t.len();
    if n > TREES_KEPT {
        t.drain(..n - TREES_KEPT);
    }
    Ok(files)
}

/// `tracked` without deleted (and renamed-away) paths, plus every other
/// changed path (new, untracked), sorted. `tracked` itself when that changes
/// nothing, so viewers share one list.
fn merge_all(tracked: &Arc<Vec<String>>, changed: &[ChangedFile]) -> Arc<Vec<String>> {
    fn gone(c: &ChangedFile) -> impl Iterator<Item = &str> {
        c.old_path.as_deref().into_iter().chain((c.status == 'D').then_some(c.path.as_str()))
    }
    let has = |p: &str| tracked.binary_search_by(|t| t.as_str().cmp(p)).is_ok();
    let differs = changed.iter().any(|c| gone(c).any(|p| has(p)) || (c.status != 'D' && !has(&c.path)));
    if !differs {
        return tracked.clone();
    }
    let removed: HashSet<&str> = changed.iter().flat_map(gone).collect();
    let mut all: Vec<String> = tracked.iter().filter(|p| !removed.contains(p.as_str())).cloned().collect();
    all.extend(changed.iter().filter(|c| c.status != 'D').map(|c| c.path.clone()));
    all.sort();
    all.dedup();
    Arc::new(all)
}

fn dir_entries(all: &[String], dir: &str) -> Vec<Entry> {
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let mut dirs: Vec<&str> = Vec::new();
    let mut files: Vec<&str> = Vec::new();
    // `all` is sorted, so everything under `prefix` is contiguous.
    let start = all.partition_point(|p| p.as_str() < prefix.as_str());
    for p in all[start..].iter().take_while(|p| p.starts_with(&prefix)) {
        let rest = &p[prefix.len()..];
        match rest.split_once('/') {
            Some((d, _)) => {
                if dirs.last() != Some(&d) {
                    dirs.push(d);
                }
            }
            None => files.push(rest),
        }
    }
    dirs.sort_unstable();
    dirs.dedup();
    let mut out = Vec::new();
    if !dir.is_empty() {
        out.push(Entry { label: "../".into(), path: parent_dir(dir).to_string(), is_dir: true });
    }
    out.extend(dirs.into_iter().map(|d| Entry { label: format!("{d}/"), path: format!("{prefix}{d}"), is_dir: true }));
    out.extend(files.into_iter().map(|f| Entry { label: f.to_string(), path: format!("{prefix}{f}"), is_dir: false }));
    out
}

/// Changed files per path and per containing folder (at every level).
fn changed_counts(changed: &[ChangedFile]) -> HashMap<&str, usize> {
    let mut m = HashMap::new();
    for c in changed {
        *m.entry(c.path.as_str()).or_default() += 1;
        let mut p = c.path.as_str();
        while let Some((d, _)) = p.rsplit_once('/') {
            *m.entry(d).or_default() += 1;
            p = d;
        }
    }
    m
}

/// The folder view as a document, so vim motions and search work in it.
fn folder_doc(entries: &[Entry], changed: &[ChangedFile]) -> Doc {
    let counts = changed_counts(changed);
    let by_path: HashMap<&str, &ChangedFile> = changed.iter().map(|c| (c.path.as_str(), c)).collect();
    let lines = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut l = DocLine::plain(e.label.clone(), i + 1);
            let color = if e.is_dir { theme::ACCENT } else if by_path.contains_key(e.path.as_str()) { theme::YELLOW } else { theme::TEXT };
            l.spans = vec![(0..l.text.len(), color)];
            if e.label != "../" {
                if let Some(c) = by_path.get(e.path.as_str()) {
                    l.suffix = Some(format!("{} +{} −{}", c.status, c.added, c.removed));
                } else if let Some(n) = counts.get(e.path.as_str()).filter(|_| e.is_dir) {
                    l.suffix = Some(format!("● {n} changed"));
                }
            }
            l
        })
        .collect();
    Doc { lines }
}

/// File picker items: every file, with changed files and files in `dir` boosted.
fn file_items(changed: &[ChangedFile], all: &[String], dir: &str) -> (Vec<(String, String)>, Vec<i64>) {
    let by_path: HashMap<&str, &ChangedFile> = changed.iter().map(|c| (c.path.as_str(), c)).collect();
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let mut items = Vec::with_capacity(all.len() + changed.len());
    let mut bonus = Vec::with_capacity(all.len() + changed.len());
    for c in changed.iter().filter(|c| c.status != 'D') {
        items.push((c.path.clone(), format!("{} +{} −{}", c.status, c.added, c.removed)));
        bonus.push(120);
    }
    for p in all.iter().filter(|p| !by_path.contains_key(p.as_str())) {
        items.push((p.clone(), String::new()));
        bonus.push(if !prefix.is_empty() && p.starts_with(&prefix) { 40 } else { 0 });
    }
    (items, bonus)
}

fn kind_color(kind: &str) -> egui::Color32 {
    match kind {
        "class" | "struct" | "enum" | "trait" | "interface" | "type" => theme::YELLOW,
        "impl" | "module" => theme::PURPLE,
        _ => theme::ACCENT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{Harness, TestRepo};
    use doc::LineKind;

    fn paths(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn cf(path: &str, status: char, old: Option<&str>) -> ChangedFile {
        ChangedFile { path: path.into(), old_path: old.map(String::from), status, added: 0, removed: 0 }
    }

    #[test]
    fn diff_totals_add_up_every_file() {
        let changed = [ChangedFile { added: 3, removed: 1, ..cf("a.rs", 'M', None) }, ChangedFile { added: 40, removed: 0, ..cf("b.rs", 'A', None) }];
        assert_eq!(diff_totals(&changed), (43, 1));
        assert_eq!(diff_totals(&[]), (0, 0));
    }

    #[test]
    fn merge_all_tracks_changes() {
        let tracked = Arc::new(paths(&["a.rs", "b/c.rs", "old.rs"]));
        // Modified files only: the same list, shared.
        assert!(Arc::ptr_eq(&merge_all(&tracked, &[cf("a.rs", 'M', None)]), &tracked));
        assert!(Arc::ptr_eq(&merge_all(&tracked, &[]), &tracked));
        let all = merge_all(&tracked, &[cf("a.rs", 'D', None), cf("b/new.rs", 'A', None), cf("z.rs", 'R', Some("old.rs"))]);
        assert_eq!(*all, paths(&["b/c.rs", "b/new.rs", "z.rs"]));
        // A deleted file that the index already dropped changes nothing.
        assert!(Arc::ptr_eq(&merge_all(&tracked, &[cf("gone.rs", 'D', None)]), &tracked));
    }

    /// A tab lists once on open, re-lists only the changes while the
    /// worktree is unchanged, and folds new files into the file list.
    #[test]
    fn live_changes_single_listing() {
        let h = Harness::new(TestRepo::new("mkdir aster && echo a > aster/a.rs"));
        let mut v = h.viewer();
        let settle = |v: &mut Viewer| {
            h.settle(v, &|v| v.files_state.is_some() && !v.changes_pending);
            h.frame(v);
        };
        settle(&mut v);
        assert!(v.browsing && v.dir == "aster", "no changes: opens on the default folder");
        assert!(!v.changes_pending && v.index.is_some());
        let first = v.all.clone();
        // Nothing changed: the quiet re-list keeps the very same file list.
        v.changes_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
        h.frame(&mut v);
        assert!(v.changes_pending, "a quiet re-list started");
        settle(&mut v);
        assert!(Arc::ptr_eq(&first, &v.all), "unchanged worktree: no new file list");
        // An agent adds a file: it shows up as changed and in the folder.
        h.repo.sh("echo b > aster/b.rs");
        v.changes_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
        h.frame(&mut v);
        settle(&mut v);
        h.settle(&mut v, &|v| v.all.iter().any(|p| p == "aster/b.rs"));
        assert!(v.changed.iter().any(|c| c.path == "aster/b.rs" && c.status == 'A'));
        assert_eq!(*v.all, paths(&["aster/a.rs", "aster/b.rs"]));
        assert!(v.entries.iter().any(|e| e.path == "aster/b.rs"), "folder view rebuilt");
    }

    #[test]
    fn folders() {
        let all = paths(&["README.md", "aster/a.py", "aster/sub/b.py", "aster/sub/c.py", "asterisk/x", "rust/lib.rs"]);
        let e = dir_entries(&all, "aster");
        let labels: Vec<&str> = e.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, vec!["../", "sub/", "a.py"]);
        assert_eq!(e[1].path, "aster/sub");
        let root: Vec<String> = dir_entries(&all, "").into_iter().map(|e| e.label).collect();
        assert_eq!(root, vec!["aster/", "asterisk/", "rust/", "README.md"]);
        assert_eq!(parent_dir("aster/sub"), "aster");
        assert_eq!(parent_dir("aster"), "");
    }

    #[test]
    fn file_list_keyboard() {
        let h = Harness::new(TestRepo::new("mkdir -p aster/a aster/b && echo 1 > aster/a/x.py && echo 2 > aster/b/y.py && echo 3 > aster/top.py"));
        let mut v = h.viewer();
        h.settle(&mut v, &|v| v.files_state.is_some());
        h.frame(&mut v);
        assert_eq!(v.dir, "aster", "no changes: default folder");
        v.command(PanelCmd::FocusList, &h.engine);
        h.frame(&mut v);
        // Rows: ../ a/ b/ top.py — the cursor starts on a/.
        assert_eq!(v.list_cursor, 1);
        h.key(&mut v, egui::Key::Enter);
        assert_eq!(v.dir, "aster/a");
        h.key(&mut v, egui::Key::Enter); // x.py
        assert_eq!(v.current.as_deref(), Some("aster/a/x.py"));
        assert!(v.list_focus, "opening a file keeps the list focused");
        h.text(&mut v, "h");
        assert_eq!(v.dir, "aster");
        assert_eq!(v.list_entries()[v.list_cursor].path, "aster/a", "cursor back on the folder we left");
        h.text(&mut v, "j");
        h.text(&mut v, "j");
        h.text(&mut v, "l");
        assert_eq!(v.current.as_deref(), Some("aster/top.py"));
        v.command(PanelCmd::FocusCode, &h.engine);
        assert!(!v.list_focus);
    }

    /// A tab with changes opens on the first changed file's diff; the tree
    /// list, ⌘⇧J/⌘⇧K, `[c` and ⌘B step through them without browsing folders.
    #[test]
    fn changed_files_flow() {
        let h = Harness::new(TestRepo::new("mkdir -p src/deep/er && printf '1\\n2\\n3\\n4\\n5\\n6\\n7\\n8\\n' > src/a.rs && echo gone > old.txt && echo z > z.md"));
        h.repo.sh("printf '1\\nX\\n3\\n4\\n5\\n6\\nY\\n8\\n' > src/a.rs && echo new > src/deep/er/b.rs && git rm -q old.txt && git add . && git commit -qm change");
        let mut v = h.viewer();
        // Settle: list, file and base reads run on threads.
        let settle = |v: &mut Viewer| {
            h.settle(v, &|v| {
                let p = v.current.clone().unwrap_or_default();
                v.files_state.is_some() && v.files.contains_key(&p) && v.bases.contains_key(&p) && v.hunk_goto.is_none()
            });
            h.frame(v);
        };
        settle(&mut v);
        let order: Vec<&str> = v.changed.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(order, vec!["src/deep/er/b.rs", "src/a.rs", "old.txt"], "tree order: folders first");
        assert!(!v.browsing && v.diff && v.tree_mode());
        assert_eq!(v.current.as_deref(), Some("src/deep/er/b.rs"), "opens on the first changed file");
        assert!(v.doc.hunk_starts().contains(&v.vim.line), "cursor on its first change");
        // ⌘⇧J: next changed file, cursor on its first change (line 2 of a.rs).
        v.command(PanelCmd::NextFile, &h.engine);
        settle(&mut v);
        assert_eq!(v.current.as_deref(), Some("src/a.rs"));
        assert_eq!(v.current_source_line(), Some(2));
        // The deleted file shows as removed lines.
        v.command(PanelCmd::NextFile, &h.engine);
        settle(&mut v);
        assert_eq!(v.current.as_deref(), Some("old.txt"));
        assert_eq!(v.doc.changed_count(), (0, 1));
        // `[c` at the first change: the previous file's last change.
        h.text(&mut v, "[");
        h.text(&mut v, "c");
        settle(&mut v);
        assert_eq!(v.current.as_deref(), Some("src/a.rs"));
        assert_eq!(v.current_source_line(), Some(7));
        // The tree list: j/k open files straight away; `a` toggles the folder list.
        v.command(PanelCmd::FocusList, &h.engine);
        h.text(&mut v, "k");
        assert_eq!(v.current.as_deref(), Some("src/deep/er/b.rs"));
        h.text(&mut v, "G");
        assert_eq!(v.current.as_deref(), Some("old.txt"));
        h.text(&mut v, "a");
        assert!(!v.tree_mode());
        h.text(&mut v, "a");
        assert!(v.tree_mode());
        // ⌘B hides and shows the list.
        v.command(PanelCmd::ToggleFiles, &h.engine);
        assert!(!v.show_files && !v.list_focus);
        v.command(PanelCmd::ToggleFiles, &h.engine);
        assert!(v.show_files);
    }

    /// Ctrl-o / Ctrl-i return to the line each file was left on, even in a
    /// changed file's diff (where a plain open goes to the first change).
    #[test]
    fn jump_list_restores_lines_in_changed_files() {
        let h = Harness::new(TestRepo::new("printf '1\\n2\\n3\\n4\\n5\\n6\\n7\\n8\\n' > a.rs"));
        h.repo.sh("printf '1\\nX\\n3\\n4\\n5\\n6\\n7\\n8\\n' > a.rs && seq 10 > new.rs && git add . && git commit -qm change");
        let mut v = h.viewer();
        let settle = |v: &mut Viewer| {
            h.settle(v, &|v| {
                let p = v.current.clone().unwrap_or_default();
                v.files_state.is_some() && v.files.contains_key(&p) && v.bases.contains_key(&p) && v.hunk_goto.is_none() && v.pending_goto.is_none()
            });
            h.frame(v);
        };
        settle(&mut v);
        assert!(v.diff);
        v.open("new.rs", None, true);
        settle(&mut v);
        v.place_cursor(7, 0);
        v.open("a.rs", None, true);
        settle(&mut v);
        assert_eq!(v.current_source_line(), Some(2), "a plain open goes to the first change");
        v.place_cursor(5, 0);
        v.jump_back();
        settle(&mut v);
        assert_eq!((v.current.as_deref(), v.current_source_line()), (Some("new.rs"), Some(7)));
        v.jump_forward();
        settle(&mut v);
        assert_eq!((v.current.as_deref(), v.current_source_line()), (Some("a.rs"), Some(5)));
        // A jump within a file that still has its first change queued.
        v.open("new.rs", None, true);
        v.goto_loc(Loc { path: "new.rs".into(), line: 9, col: 0 });
        settle(&mut v);
        assert_eq!(v.current_source_line(), Some(9));
    }

    /// A file opened at a folded line before its base arrives (the plain
    /// file first, then the diff) lands on that line, its fold open.
    #[test]
    fn open_at_folded_line_before_the_base() {
        let h = Harness::new(TestRepo::new("seq 1 60 > a.rs"));
        h.repo.sh("seq 1 60 | sed -e 's/^10$/ten/' > a.rs && git commit -qam change");
        let mut v = h.viewer();
        v.command(PanelCmd::Open { path: Some("a.rs".into()), line: Some(6), diff: Some(true) }, &h.engine);
        h.settle(&mut v, &|v| v.bases.contains_key("a.rs") && v.pending_goto.is_none() && v.doc.lines.iter().any(|l| l.new_no == Some(6)));
        h.frame(&mut v);
        assert_eq!(v.current_source_line(), Some(6));
        assert!(v.doc.lines.iter().any(|l| l.fold.is_some()), "the rest stays folded");
    }

    /// A diff opens folded around its changes: a jump into a fold opens it,
    /// zM folds again, Enter / zo open the fold under the cursor, zR all.
    #[test]
    fn diffs_open_folded() {
        let h = Harness::new(TestRepo::new("seq 1 60 > a.rs"));
        h.repo.sh("seq 1 60 | sed -e 's/^10$/ten/' -e 's/^50$/fifty/' > a.rs && git commit -qam change");
        let mut v = h.viewer();
        let settle = |v: &mut Viewer| {
            h.settle(v, &|v| v.files.contains_key("a.rs") && v.bases.contains_key("a.rs") && v.hunk_goto.is_none() && v.pending_goto.is_none());
            h.frame(v);
        };
        let folds = |v: &Viewer| v.doc.lines.iter().filter_map(|l| l.fold).map(|f| (f.first, f.last)).collect::<Vec<_>>();
        settle(&mut v);
        assert_eq!(v.current.as_deref(), Some("a.rs"));
        assert_eq!(folds(&v), vec![(1, 6), (14, 46), (54, 60)], "the line-1 goto of a fresh open leaves the first fold shut");
        assert_eq!(v.current_source_line(), Some(10), "cursor on the first change");
        h.text(&mut v, "zo");
        assert_eq!(v.status.as_ref().map(|s| s.0.as_str()), Some("no fold under the cursor"));
        // A jump into a fold opens it and lands on the line.
        v.place_cursor(30, 0);
        settle(&mut v);
        assert_eq!(v.current_source_line(), Some(30));
        assert_eq!(folds(&v), vec![(1, 6), (54, 60)]);
        // zM folds it again: the cursor waits on its fold row.
        h.text(&mut v, "zM");
        h.frame(&mut v);
        assert_eq!(folds(&v).len(), 3);
        assert!(v.doc.lines[v.vim.line].fold.is_some());
        assert_eq!(v.current_source_line(), Some(14));
        // Enter opens it again; zR opens them all, and that outlives a rebuild.
        h.key(&mut v, egui::Key::Enter);
        h.frame(&mut v);
        assert_eq!(folds(&v), vec![(1, 6), (54, 60)]);
        assert_eq!(v.current_source_line(), Some(14));
        h.text(&mut v, "zR");
        h.frame(&mut v);
        assert!(folds(&v).is_empty());
        v.command(PanelCmd::ToggleDiff, &h.engine);
        h.frame(&mut v);
        v.command(PanelCmd::ToggleDiff, &h.engine);
        settle(&mut v);
        assert!(v.diff && folds(&v).is_empty());
    }

    /// ⌘M … ⌘Enter hands the comment to the `comment` listener with its
    /// side and line; the comments it sets draw under their rows, and ⌘M on
    /// such a row replies.
    #[test]
    fn rendered_markdown_takes_comments() {
        let plugins = std::env::temp_dir().join(format!("kt-plugins-{}", kitchen_core::util::new_id()));
        let plug = plugins.join("notes");
        std::fs::create_dir_all(&plug).unwrap();
        std::fs::write(plug.join("plugin.toml"), "name = \"notes\"\napi = 4\n").unwrap();
        std::fs::write(
            plug.join("init.lua"),
            r#"kitchen.on("comment", function(ev)
  local body = ev.text .. "|" .. ev.side .. ":" .. ev.line .. "|" .. tostring(ev.start_line) .. "|" .. ev.quote
  kitchen.comments.set(ev.file, { { id = "c1", line = ev.line, side = ev.side, title = "@me", body = body } })
end)"#,
        )
        .unwrap();
        let bundled = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        std::fs::create_dir_all(plugins.join("md")).unwrap();
        std::os::unix::fs::symlink(bundled.join("markdown"), plugins.join("md/markdown")).unwrap();
        let h = Harness::new(TestRepo::new("printf '# T\\n\\nold para\\n' > README.md"));
        h.repo.sh("printf '# T\\n\\nline *one*\\nline two\\n' > README.md && git commit -qam change");
        let engine = &h.engine;
        engine.set_plugins(crate::plugins::PluginHost::start(engine, vec![plugins.clone(), plugins.join("md")]));
        let mut v = h.viewer();
        h.settle(&mut v, &|v| v.current.as_deref() == Some("README.md") && v.bases.contains_key("README.md") && engine.plugins().unwrap().list().len() == 2);
        v.command(PanelCmd::Render(None), engine);
        h.settle(&mut v, &|v| v.render_shown.is_some());
        let rows: Vec<(LineKind, Option<usize>, &str, bool)> = v.doc.lines.iter().map(|l| (l.kind, l.new_no, l.text.as_str(), l.render.is_some())).collect();
        assert_eq!(
            rows,
            vec![
                (LineKind::Normal, Some(1), "# T", true),
                (LineKind::Removed, None, "old para", false),
                (LineKind::Added, Some(3), "line *one* line two", true),
            ]
        );
        assert!(v.doc.lines[0].covers_new(2), "the blank line joins the heading's row");
        // ⌘M on the paragraph comments on both of its lines.
        v.place_cursor(4, 0);
        assert_eq!(v.vim.line, 2, "line 4 is in the paragraph's row");
        v.command(PanelCmd::Comment, engine);
        assert!(v.editor.as_ref().unwrap().label.starts_with("Comment on lines 3–4"));
        h.frame(&mut v);
        h.text(&mut v, "nice");
        v.command(PanelCmd::Submit, engine);
        h.frame(&mut v);
        h.settle(&mut v, &|v| !v.comment_blocks().is_empty());
        let (block, _) = v.comment_blocks().remove(0);
        assert_eq!((block.after, block.body.as_str()), (Some(2), "nice|new:4|3|line *one*\nline two"), "the source lines are quoted");
        // Back to text: the comment stays on its line.
        v.command(PanelCmd::Render(None), engine);
        h.settle(&mut v, &|v| v.render_shown.is_none() && v.doc.lines.iter().all(|l| l.render.is_none()));
        let row = v.comment_blocks()[0].0.after.unwrap();
        assert_eq!(v.doc.lines[row].new_no, Some(4));
        let _ = std::fs::remove_dir_all(&plugins);
    }

    #[test]
    fn comments_go_to_plugins_and_come_back() {
        let plugins = std::env::temp_dir().join(format!("kt-plugins-{}", kitchen_core::util::new_id()));
        let plug = plugins.join("notes");
        std::fs::create_dir_all(&plug).unwrap();
        std::fs::write(plug.join("plugin.toml"), "name = \"notes\"\napi = 4\n").unwrap();
        std::fs::write(
            plug.join("init.lua"),
            r#"kitchen.on("comment", function(ev)
  local body = ev.text .. "|" .. ev.side .. ":" .. ev.line .. "|" .. tostring(ev.start_line) .. "|" .. (ev.reply_to or "-")
  kitchen.comments.set(ev.file, { { id = "c1", line = ev.line, side = ev.side, title = "@me", body = body } })
end)"#,
        )
        .unwrap();
        let h = Harness::new(TestRepo::new("printf 'a\\nb\\nc\\n' > a.py"));
        h.repo.sh("printf 'a\\nB\\nc\\n' > a.py && git commit -qam change");
        let engine = &h.engine;
        engine.set_plugins(crate::plugins::PluginHost::start(engine, vec![plugins.clone()]));
        let mut v = h.viewer();
        h.settle(&mut v, &|v| v.current.as_deref() == Some("a.py") && v.bases.contains_key("a.py") && v.hunk_goto.is_none() && !engine.plugins().unwrap().listeners("comment").is_empty());
        // The first change starts with the removed `b`: a base-side line.
        assert_eq!(v.doc.lines[v.vim.line].text, "b");
        v.command(PanelCmd::Comment, engine);
        assert!(v.editor.is_some());
        h.frame(&mut v);
        h.text(&mut v, "why?");
        v.command(PanelCmd::Submit, engine);
        h.frame(&mut v);
        assert!(v.editor.is_none(), "submitted");
        h.settle(&mut v, &|v| !v.comment_blocks().is_empty());
        let (block, id) = v.comment_blocks().remove(0);
        assert_eq!((block.after, id.as_deref()), (Some(v.vim.line), Some("c1")));
        assert_eq!(block.body, "why?|old:2|nil|-");
        // ⌘M on the commented row replies to it.
        v.command(PanelCmd::Comment, engine);
        assert!(v.editor.as_ref().unwrap().label.starts_with("Reply to @me"));
        h.frame(&mut v);
        h.text(&mut v, "ok");
        v.command(PanelCmd::Submit, engine);
        h.frame(&mut v);
        h.settle(&mut v, &|v| v.comment_blocks().first().is_some_and(|(b, _)| b.body.ends_with("|c1")));
        let _ = std::fs::remove_dir_all(&plugins);
    }

    #[test]
    fn questions_are_answered_under_their_line() {
        let h = Harness::new(TestRepo::new("printf 'a\\nb\\nc\\n' > a.py"));
        h.repo.sh("printf 'a\\nB\\nc\\n' > a.py && git commit -qam change");
        let engine = &h.engine;
        let mut v = h.viewer();
        v.answerer = |a, p| match p {
            kitchen_core::ask::Prompt::File(q) => Ok(format!("{}|{}|{}:{}-{}|{}|{}|{}", a.model, a.session.is_some(), q.path, q.first, q.last, q.base, q.quote, q.text)),
            kitchen_core::ask::Prompt::FollowUp(t) => Ok(format!("{}|{t}", a.thread.len())),
            kitchen_core::ask::Prompt::Plan(_) => unreachable!(),
        };
        h.settle(&mut v, &|v| v.current.as_deref() == Some("a.py") && v.bases.contains_key("a.py") && v.hunk_goto.is_none());
        // The first change starts with the removed `b`; V takes the added `B` too.
        assert_eq!(v.doc.lines[v.vim.line].text, "b");
        h.text(&mut v, "V");
        h.text(&mut v, "j");
        v.command(PanelCmd::Ask, engine);
        assert!(v.editor.as_ref().is_some_and(|e| e.ask && e.label.starts_with("Ask the agent about lines")), "{:?}", v.editor.as_ref().map(|e| &e.label));
        h.frame(&mut v);
        h.text(&mut v, "why?");
        // Enter asks (⌘Enter would too).
        h.key(&mut v, egui::Key::Enter);
        assert!(v.editor.is_none(), "submitted");
        let row = v.vim.line;
        assert_eq!(v.doc.lines[row].text, "B");
        assert_eq!(v.ask_blocks().first().map(|b| (b.after, b.body.as_str())), Some((Some(row), "thinking… 0s")));
        h.settle(&mut v, &|v| v.ask_blocks().first().is_some_and(|b| !b.dim));
        let b = v.ask_blocks().remove(0);
        // Mixed sides: about the last line, in the current file.
        assert_eq!(b.body, "sonnet|false|a.py:2-2|false|b\nB|why?");
        assert!(b.title.starts_with("why?"));
        // The answer opens a follow-up field under the thread; it asks in
        // the thread's session (a uuid).
        let follow_up = |v: &Viewer| v.editor.as_ref().filter(|e| e.ask && e.follow_up && e.end == row && e.label.starts_with("Follow up on “why?”")).is_some();
        assert!(follow_up(&v), "{:?}", v.editor.as_ref().map(|e| &e.label));
        h.frame(&mut v);
        h.text(&mut v, "and B?");
        h.key(&mut v, egui::Key::Enter);
        assert!(v.editor.is_none(), "submitted");
        h.settle(&mut v, &|v| v.ask_blocks().len() == 2 && v.ask_blocks().iter().all(|b| !b.dim));
        let b = v.ask_blocks().remove(1);
        assert_eq!((b.after, b.title.as_str(), b.body.as_str()), (Some(row), "↳ and B?", "36|and B?"));
        // Esc closes the field, ⌘⇧A on the thread's line opens it again.
        h.settle(&mut v, &follow_up);
        h.key(&mut v, egui::Key::Escape);
        assert!(v.editor.is_none() && v.ask_blocks().len() == 2, "Esc in the field keeps the thread");
        v.command(PanelCmd::Ask, engine);
        assert!(follow_up(&v));
        h.key(&mut v, egui::Key::Escape);
        // An answer that lands while the cursor is elsewhere opens nothing.
        v.place_cursor(1, 0);
        v.offered = v.asks.first().map(|a| a.thread.id);
        h.frame(&mut v);
        assert!(v.editor.is_none());
        // Esc elsewhere keeps the thread; Esc on its line closes it.
        h.key(&mut v, egui::Key::Escape);
        assert_eq!(v.ask_blocks().len(), 2);
        // Elsewhere ⌘⇧A asks a new question.
        v.command(PanelCmd::Ask, engine);
        assert!(v.editor.as_ref().is_some_and(|e| e.label.starts_with("Ask the agent")));
        h.key(&mut v, egui::Key::Escape);
        v.cursor_to_row(row);
        h.key(&mut v, egui::Key::Escape);
        assert!(v.ask_blocks().is_empty());
    }

    /// Drives a real viewer against `KT_VIEWER_HOST`:`KT_VIEWER_WT` (e.g. a devbox worktree).
    #[test]
    #[ignore]
    fn live_picker() {
        let host = std::env::var("KT_VIEWER_HOST").unwrap_or_else(|_| "local".into());
        let wt = std::env::var("KT_VIEWER_WT").expect("KT_VIEWER_WT");
        let (ctx, engine) = crate::test_util::engine(&host, &wt, &["t"]);
        let w = engine.workspaces()[0].clone();
        let mut v = Viewer::new(&engine, &w, &w.tabs[0]);
        let frame = |v: &mut Viewer, events: Vec<egui::Event>| {
            let mut out = ctx.run_ui(egui::RawInput { events, screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 700.0))), ..Default::default() }, |ui| {
                let mut cx = PanelCtx { engine: &engine, focused: true, clicked: false, view: None };
                v.ui(ui, &mut cx);
            });
            out.textures_delta.clear();
        };
        frame(&mut v, vec![]);
        v.command(PanelCmd::FilePicker, &engine);
        for _ in 0..3 {
            frame(&mut v, vec![]);
        }
        for c in ["m", "a", "i", "n"] {
            frame(&mut v, vec![egui::Event::Text(c.into())]);
        }
        let start = std::time::Instant::now();
        while v.files_state.is_none() && start.elapsed().as_secs() < 10 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            frame(&mut v, vec![]);
        }
        frame(&mut v, vec![]);
        let (_, p) = v.picker.as_ref().expect("picker open");
        eprintln!("files={} query={:?} items={} dir={:?} browsing={} entries={}", v.all.len(), p.query, p.items.len(), v.dir, v.browsing, v.entries.len());
        assert!(!p.items.is_empty(), "late file list reaches the open picker");
        eprintln!("top match for 'main': {:?}", p.filtered.first().map(|f| &p.items[f.0].0));
        for e in v.entries.iter().take(8) {
            eprintln!("  {}", e.label);
        }
    }
}
