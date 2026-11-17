//! `gd` / `gr`: ask a plugin provider (e.g. a language server) and wait for
//! it; no answer, an error, a self-answer or a timeout falls back to a text
//! search, which opens a list unless the scope walk is certain. Answers that
//! arrive after the user left the spot are dropped.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use kitchen_core::host::Host;
use kitchen_core::util::shell_quote;
use kitchen_symbols::Symbol;

use super::picker::Picker;
use super::{Loc, Msg, PickerKind, Viewer};
use crate::plugins::Location;

/// A provider that hasn't answered by then is no longer waited for.
const NAV_TIMEOUT: Duration = Duration::from_secs(20);
/// Most text-search references listed.
const MAX_REFS: usize = 500;
/// Most `git grep` lines read for references before ranking.
const MAX_GREP_LINES: usize = 5000;
/// Most definition candidate files read per jump (one round trip, after ranking).
const MAX_DEF_FILES: usize = 20;
/// Most candidate paths listed for ranking.
const MAX_DEF_CANDIDATES: usize = 2000;
/// Larger definition candidates are skipped.
const MAX_DEF_FILE_BYTES: usize = crate::plugins::MAX_TEXT;
/// Most locations given a preview line.
const MAX_PREVIEWS: usize = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NavKind {
    Definition,
    References,
}

impl NavKind {
    fn provider(self) -> &'static str {
        match self {
            NavKind::Definition => "definition",
            NavKind::References => "references",
        }
    }

    fn nothing(self, name: &str) -> String {
        match self {
            NavKind::Definition => format!("no definition found for {name}"),
            NavKind::References => format!("no references to {name}"),
        }
    }

    fn finding(self, name: &str) -> String {
        match self {
            NavKind::Definition => format!("finding the definition of {name}…"),
            NavKind::References => format!("finding references to {name}…"),
        }
    }
}

pub(super) struct PendingNav {
    token: u64,
    kind: NavKind,
    name: String,
    started: Instant,
    /// Cursor when gd/gr was pressed: ranking, and whether an answer still applies.
    here: Option<Loc>,
    /// 1-based raw column where the asked word starts.
    word_col: Option<usize>,
    /// Asked from a removed diff row: `here` is then the next current line.
    removed: bool,
    /// The provider was asked and hasn't answered.
    provider: bool,
    /// The status line this query set, cleared with it.
    status: Option<String>,
}

impl Viewer {
    /// `gd` / `gr`: a provider's answer, else the text search (symbols and
    /// grep for a definition, `git grep -w` for references).
    pub(super) fn nav(&mut self, kind: NavKind, name: String) {
        if self.current.as_deref().is_some_and(|p| kitchen_symbols::is_keyword(p, &name)) {
            self.status = Some((format!("{name} is a keyword"), false));
            return;
        }
        let word_col = self.word_here().map(|(col, _)| col);
        // A removed row is old text: the provider only knows the current file.
        let removed = self.on_removed_row();
        if removed || !self.ask_provider(kind, &name, word_col) {
            self.text_search(kind, name, self.here(), word_col, removed, None);
        }
    }

    fn start_nav(&mut self, kind: NavKind, name: &str, here: Option<Loc>, word_col: Option<usize>, removed: bool, provider: bool) -> u64 {
        self.nav_token += 1;
        self.status = None;
        let token = self.nav_token;
        self.pending_nav = Some(PendingNav { token, kind, name: name.to_string(), started: Instant::now(), here, word_col, removed, provider, status: None });
        token
    }

    /// Show `msg` for the pending query; it goes when the query does.
    fn nav_status(&mut self, msg: String) {
        if let Some(p) = self.pending_nav.as_mut() {
            p.status = Some(msg.clone());
        }
        self.status = Some((msg, false));
    }

    /// Clear the status line if it still shows what `p` put there.
    fn clear_nav_status(&mut self, p: &PendingNav) {
        if p.status.is_some() && self.status.as_ref().map(|s| &s.0) == p.status.as_ref() {
            self.status = None;
        }
    }

    /// End the pending query early (another file, Esc, or the user moved):
    /// forget it and clear the status it set.
    pub(super) fn abandon_nav(&mut self) {
        if let Some(p) = self.pending_nav.take() {
            self.clear_nav_status(&p);
        }
    }

    /// The cursor is still where `p` was asked: same file and line.
    fn still_here(&self, p: &PendingNav) -> bool {
        match (&p.here, self.here()) {
            (Some(a), Some(b)) => a.path == b.path && a.line == b.line,
            (a, b) => a.is_none() && b.is_none(),
        }
    }

    /// Hand the query to a plugin; false when no plugin answers for this file.
    fn ask_provider(&mut self, kind: NavKind, name: &str, word_col: Option<usize>) -> bool {
        let Some(plugins) = self.engine.plugins().cloned() else { return false };
        let Some(ft) = self.current_filetype() else { return false };
        if !plugins.has_provider(kind.provider(), &ft) {
            return false;
        }
        let ctx = crate::plugins::command_context(&self.engine, &self.tab_id, Some(self.context()));
        let token = self.start_nav(kind, name, self.here(), word_col, false, true);
        let (tx, egui_ctx, host, wt) = (self.tx.clone(), self.engine.ctx.clone(), self.host.clone(), self.worktree.clone());
        let asked = plugins.provide(kind.provider(), &ft, ctx, move |answer| {
            // Off the plugin's thread: previews may need a round trip.
            std::thread::spawn(move || {
                let answer = answer.map(|mut locs| {
                    if locs.len() > 1 {
                        fill_previews(&host, &wt, &mut locs);
                    }
                    locs
                });
                let _ = tx.send(Msg::Nav { token, answer });
                egui_ctx.request_repaint();
            });
        });
        if !asked {
            self.pending_nav = None;
            return false;
        }
        self.nav_status(kind.finding(name));
        self.engine.ctx.request_repaint_after(NAV_TIMEOUT);
        true
    }

    /// The text search: no provider, or it had nothing. `gd` on a name the
    /// scope walk binds in this file jumps straight there; anything else the
    /// search finds opens a list. `status` says why it runs.
    fn text_search(&mut self, kind: NavKind, name: String, here: Option<Loc>, word_col: Option<usize>, removed: bool, status: Option<String>) {
        if kind == NavKind::Definition && !removed {
            let Some(here) = here.clone() else {
                self.status = None;
                return;
            };
            let bound = word_col.zip(self.files.get(&here.path)).and_then(|(col, f)| kitchen_symbols::resolve_local(&here.path, &f.text, here.line, col).map(|d| (d, col)));
            if let Some((def, col)) = bound {
                if (def.line, def.col) != (here.line, col) {
                    self.status = None;
                    self.record_and_go_raw(Loc { path: here.path.clone(), line: def.line, col: def.col });
                    return;
                }
            }
        }
        self.start_nav(kind, &name, here, word_col, removed, false);
        self.nav_status(status.unwrap_or_else(|| format!("searching for {name}…")));
        self.start_grep();
    }

    /// Spawn the text search for the pending query (`Msg::Grep` comes back).
    fn start_grep(&mut self) {
        let Some(p) = self.pending_nav.as_ref() else { return };
        let (token, name, kind, removed) = (p.token, p.name.clone(), p.kind, p.removed);
        let (tx, ctx, host, wt) = (self.tx.clone(), self.engine.ctx.clone(), self.host.clone(), self.worktree.clone());
        match kind {
            NavKind::Definition => {
                let Some(here) = p.here.clone() else {
                    self.pending_nav = None;
                    self.status = Some((kind.nothing(&name), true));
                    return;
                };
                let python = matches!(crate::plugins::filetype_of(&here.path).as_str(), "py" | "pyi");
                let imports = self.files.get(&here.path).filter(|_| python).map(|f| kitchen_symbols::python_imports(&f.text)).unwrap_or_default();
                std::thread::spawn(move || {
                    let mut locs = search_definitions(&host, &wt, &name, &here.path, &imports);
                    // Drop the definition we're standing on (on a removed row
                    // `here` is the next line, which may well be the answer).
                    if !removed {
                        locs.retain(|l| !(l.path == here.path && l.line == here.line));
                    }
                    let _ = tx.send(Msg::Grep { token, locs, capped: false });
                    ctx.request_repaint();
                });
            }
            NavKind::References => {
                let from = p.here.as_ref().map(|h| h.path.clone()).unwrap_or_default();
                std::thread::spawn(move || {
                    let (locs, capped) = grep_refs(&host, &wt, &name, &from);
                    let _ = tx.send(Msg::Grep { token, locs, capped });
                    ctx.request_repaint();
                });
            }
        }
    }

    /// Per frame: stop waiting for a provider at [`NAV_TIMEOUT`] and search
    /// text instead (when the user is still where they asked).
    pub(super) fn check_nav_timeout(&mut self) {
        let Some(p) = self.pending_nav.as_ref().filter(|p| p.provider) else { return };
        let elapsed = p.started.elapsed();
        if elapsed <= NAV_TIMEOUT {
            self.engine.ctx.request_repaint_after(NAV_TIMEOUT.saturating_sub(elapsed).max(Duration::from_millis(10)));
            return;
        }
        log::info!(target: "kitchen::nav", "{:?} {}: no answer from the language server in {NAV_TIMEOUT:?}", p.kind, p.name);
        let Some(p) = self.pending_nav.take() else { return };
        if !self.still_here(&p) {
            self.clear_nav_status(&p);
            return;
        }
        let msg = format!("no answer from the language server for {}; searching text…", p.name);
        self.text_search(p.kind, p.name, p.here, p.word_col, p.removed, Some(msg));
    }

    /// The provider's answer (`None`: it couldn't tell).
    pub(super) fn finish_nav(&mut self, token: u64, answer: Option<Vec<Location>>) {
        let Some(p) = self.pending_nav.take_if(|p| p.token == token) else { return };
        let n = answer.as_ref().map(Vec::len);
        log::info!(target: "kitchen::nav", "{:?} {}: language server → {n:?} in {:?}", p.kind, p.name, p.started.elapsed());
        if !self.still_here(&p) {
            self.clear_nav_status(&p);
            return;
        }
        // A definition that is the asked word itself (a pytest fixture, say) is no answer.
        let answer = answer.filter(|locs| !(p.kind == NavKind::Definition && answers_itself(&p, locs)));
        match answer {
            Some(locs) => {
                self.clear_nav_status(&p);
                self.show_locations(p.kind, &p.name, locs, NavSource::Provider, p.here.as_ref());
            }
            None => {
                let msg = format!("the language server found nothing for {}; searching text…", p.name);
                self.text_search(p.kind, p.name, p.here, p.word_col, p.removed, Some(msg));
            }
        }
    }

    /// The text search's answer.
    pub(super) fn finish_grep(&mut self, token: u64, locs: Vec<Location>, capped: bool) {
        let Some(p) = self.pending_nav.take_if(|p| p.token == token) else { return };
        log::info!(target: "kitchen::nav", "{:?} {}: text search → {} in {:?}", p.kind, p.name, locs.len(), p.started.elapsed());
        self.clear_nav_status(&p);
        if !self.still_here(&p) {
            return;
        }
        self.show_locations(p.kind, &p.name, locs, NavSource::Text { capped }, p.here.as_ref());
    }

    /// One answer from the provider jumps there; anything else, and any text
    /// match (it may be another `name` entirely), opens a list. The only
    /// reference being the spot asked from is said, not jumped to.
    fn show_locations(&mut self, kind: NavKind, name: &str, locs: Vec<Location>, source: NavSource, here: Option<&Loc>) {
        let only_here = kind == NavKind::References && locs.len() == 1 && here.is_some_and(|h| h.path == locs[0].path && h.line == locs[0].line);
        if only_here {
            self.status = Some((format!("the only reference to {name} is here"), false));
            return;
        }
        match (locs.len(), source) {
            (0, _) => self.status = Some((kind.nothing(name), true)),
            (1, NavSource::Provider) => self.record_and_go_raw(loc_of(&locs[0])),
            _ => {
                let (title, items) = picker_items(kind, name, &locs, source);
                self.picker = Some((PickerKind::RawLocs(locs.iter().map(loc_of).collect()), Picker::new(title, items)));
            }
        }
    }

    /// Like `record_and_go`, for a 1-based raw column (as plugins give).
    pub(super) fn record_and_go_raw(&mut self, loc: Loc) {
        self.record_and_go(loc);
        self.goto_raw = true;
    }

    /// Convert a pending raw column to the display column once the file is here.
    pub(super) fn goto_col(&mut self, line: usize, col: usize) -> usize {
        if !std::mem::take(&mut self.goto_raw) {
            return col;
        }
        let text = self.current.as_ref().and_then(|p| self.files.get(p)).and_then(|f| f.text.lines().nth(line.saturating_sub(1)));
        super::plugin_glue::display_col(text.unwrap_or(""), col)
    }
}

/// Every location is the word the query was asked about.
fn answers_itself(p: &PendingNav, locs: &[Location]) -> bool {
    let (Some(here), Some(col)) = (&p.here, p.word_col) else { return false };
    let width = p.name.chars().count();
    !locs.is_empty() && locs.iter().all(|l| l.path == here.path && l.line == here.line && (col..col + width).contains(&l.col))
}

fn loc_of(l: &Location) -> Loc {
    Loc { path: l.path.clone(), line: l.line, col: l.col }
}

/// Where a list of locations came from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum NavSource {
    Provider,
    Text { capped: bool },
}

fn picker_items(kind: NavKind, name: &str, locs: &[Location], source: NavSource) -> (String, Vec<(String, String)>) {
    let capped = matches!(source, NavSource::Text { capped: true });
    let mut title = match kind {
        NavKind::Definition => format!("definitions of {name}"),
        NavKind::References => format!("{}{} references to {name}", locs.len(), if capped { "+" } else { "" }),
    };
    if matches!(source, NavSource::Text { .. }) {
        title.push_str(" (text matches)");
    }
    let items = locs
        .iter()
        .map(|l| {
            let shared = locs.iter().filter(|o| o.path == l.path && o.line == l.line).count() > 1;
            (picker_label(l, shared), l.text.as_deref().unwrap_or("").trim().to_string())
        })
        .collect();
    (title, items)
}

/// `file:line` first (the picker cuts long labels at the end), then the
/// folder: `b.py:12  pkg/sub/`. `with_col` adds the column, for two hits on
/// one line. Paths outside the worktree keep only their last two folders.
fn picker_label(l: &Location, with_col: bool) -> String {
    let (dir, file) = l.path.rsplit_once('/').unwrap_or(("", &l.path));
    let mut label = format!("{file}:{}", l.line);
    if with_col {
        label.push_str(&format!(":{}", l.col));
    }
    if !dir.is_empty() {
        let shown = if dir.starts_with('/') {
            let parts: Vec<&str> = dir.rsplit('/').take(2).collect();
            format!("…/{}", parts.into_iter().rev().collect::<Vec<_>>().join("/"))
        } else {
            dir.to_string()
        };
        label.push_str(&format!("  {shown}/"));
    }
    label
}

/// A symbol in `text` (the file at `path`) as a location: 1-based raw column,
/// and its first line (`def f(x):`, `X = 1`) as the preview.
fn def_location(path: &str, text: &str, s: &Symbol) -> Location {
    let line = text.lines().nth(s.line.saturating_sub(1)).unwrap_or("");
    // Symbol columns are 0-based bytes.
    let chars = line.get(..s.col).map(|p| p.chars().count()).unwrap_or(s.col);
    Location { path: path.to_string(), line: s.line, col: chars + 1, text: Some(s.signature.clone()) }
}

/// Repo-wide definition search in two round trips. The first lists the
/// files of `from`'s language whose lines may define `name`, and the files
/// of the modules `from` imports; they are ranked here. The second reads
/// `from` and the top [`MAX_DEF_FILES`] and keeps their definitions of `name`.
pub(super) fn search_definitions(host: &Host, worktree: &str, name: &str, from: &str, imports: &[String]) -> Vec<Location> {
    let globs: Vec<String> = kitchen_symbols::globs_for(from).iter().map(|g| shell_quote(g)).collect();
    let modules: Vec<String> = module_paths(imports).iter().map(|m| shell_quote(m)).collect();
    let list = format!(
        "cd {wt} && {{ git -c core.quotePath=false grep -l -I -E {pats} -- {globs} 2>/dev/null | head -n {MAX_DEF_CANDIDATES} | sed 's/^/g /'; {mods} }}",
        wt = shell_quote(worktree),
        pats = def_patterns(from, name),
        globs = globs.join(" "),
        mods = if modules.is_empty() { String::new() } else { format!("git -c core.quotePath=false ls-files -- {} 2>/dev/null | sed 's/^/m /';", modules.join(" ")) },
    );
    let Ok(out) = host.sh(&list, None) else { return Vec::new() };
    let (mut grep, mut imported) = (Vec::new(), Vec::new());
    for line in out.stdout.lines() {
        match line.split_once(' ') {
            Some(("g", p)) => grep.push(p.to_string()),
            Some(("m", p)) => imported.push(p.to_string()),
            _ => {}
        }
    }
    let mut paths = vec![from.to_string()];
    paths.extend(rank_candidates(grep, imported, from).into_iter().filter(|p| p != from).take(MAX_DEF_FILES));
    let nonce = kitchen_core::util::new_id();
    let read = format!(
        "cd {wt} && N={nonce} && for p in {paths}; do [ -f \"$p\" ] || continue; \
           [ \"$(wc -c < \"$p\" | tr -d ' ')\" -le {bytes} ] || continue; \
           printf '\\036%s %s\\n' \"$N\" \"$p\"; cat -- \"$p\"; echo; done",
        wt = shell_quote(worktree),
        paths = paths.iter().map(|p| shell_quote(p)).collect::<Vec<_>>().join(" "),
        bytes = MAX_DEF_FILE_BYTES,
    );
    let Ok(out) = host.sh(&read, None) else { return Vec::new() };
    split_batch(&out.stdout, &nonce)
        .into_iter()
        .flat_map(|(path, text)| kitchen_symbols::definitions_named(path, text, name).into_iter().map(|s| def_location(path, text, &s)).collect::<Vec<_>>())
        .collect()
}

/// `git grep -E` arguments for lines that may define `name` in files like
/// `from`. POSIX classes only: macOS `git grep -E` has no `\b` or `\s`.
fn def_patterns(from: &str, name: &str) -> String {
    let n = regex::escape(name);
    let python = matches!(crate::plugins::filetype_of(from).as_str(), "py" | "pyi");
    if python {
        // A name that only appears in a parenthesised multi-line import isn't matched.
        let pats = [
            format!("(^|[^[:alnum:]_])(def|class)[[:space:]]+{n}([^[:alnum:]_]|$)"),
            format!("^[[:space:]]*{n}[[:space:]]*(:[^=]*)?=([^=]|$)"),
            format!("self\\.{n}[[:space:]]*(:[^=]*)?=([^=]|$)"),
        ];
        return pats.iter().map(|p| format!("-e {}", shell_quote(p))).collect::<Vec<_>>().join(" ");
    }
    format!("-w -e {} --and -e {}", shell_quote(&n), shell_quote("(def|class|fn|func|function|struct|enum|trait|interface|type|const|let|var|impl|mod|module)"))
}

/// Pathspecs for the files a Python module may live in: "a.b" ->
/// `**/a/b.py`, `**/a/b.pyi`, `**/a/b/__init__.py` (any prefix: src layouts).
fn module_paths(imports: &[String]) -> Vec<String> {
    imports
        .iter()
        .flat_map(|m| {
            let p = m.replace('.', "/");
            [format!(":(glob)**/{p}.py"), format!(":(glob)**/{p}.pyi"), format!(":(glob)**/{p}/__init__.py")]
        })
        .collect()
}

/// Order: the files of modules `from` imports, then most leading folders
/// shared with `from`, then path.
fn rank_candidates(grep: Vec<String>, imported: Vec<String>, from: &str) -> Vec<String> {
    let shared = |p: &str| from.split('/').zip(p.split('/')).take_while(|(a, b)| a == b).count();
    let mut grep = grep;
    grep.retain(|p| !imported.contains(p));
    grep.sort_by(|a, b| shared(b).cmp(&shared(a)).then_with(|| a.cmp(b)));
    let mut out = imported;
    out.sort();
    out.dedup();
    out.extend(grep);
    out
}

/// `\x1e<nonce> <path>` header, contents, one newline: (path, contents) per file.
fn split_batch<'a>(out: &'a str, nonce: &str) -> Vec<(&'a str, &'a str)> {
    let sep = format!("\u{1e}{nonce} ");
    out.split(sep.as_str())
        .skip(1)
        .filter_map(|chunk| {
            let (path, rest) = chunk.split_once('\n')?;
            Some((path, rest.strip_suffix('\n').unwrap_or(rest)))
        })
        .collect()
}

/// `git grep -w` for `name` in `from`'s language: the current file first,
/// then its folder, then the rest; and whether more than [`MAX_REFS`] matched.
fn grep_refs(host: &Host, worktree: &str, name: &str, from: &str) -> (Vec<Location>, bool) {
    let globs: Vec<String> = kitchen_symbols::globs_for(from).iter().map(|g| shell_quote(g)).collect();
    let script = format!(
        "git -C {} -c core.quotePath=false grep -n -I -w -F --column --no-color -e {} -- {} | head -n {MAX_GREP_LINES}",
        shell_quote(worktree),
        shell_quote(name),
        globs.join(" ")
    );
    let Ok(out) = host.sh(&script, None) else { return (Vec::new(), false) };
    let mut locs = parse_grep(&out.stdout);
    let dir = |p: &str| p.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
    let from_dir = dir(from);
    locs.sort_by_key(|l| if l.path == from { 0 } else if dir(&l.path) == from_dir { 1 } else { 2 });
    let capped = locs.len() > MAX_REFS;
    locs.truncate(MAX_REFS);
    (locs, capped)
}

fn parse_grep(out: &str) -> Vec<Location> {
    out.lines()
        .filter_map(|l| {
            let mut parts = l.splitn(4, ':');
            let (path, line, col, text) = (parts.next()?, parts.next()?.parse().ok()?, parts.next()?.parse::<usize>().ok()?, parts.next()?);
            // git reports a 1-based byte column.
            let chars = text.get(..col.saturating_sub(1)).map(|p| p.chars().count()).unwrap_or(0);
            Some(Location { path: path.to_string(), line, col: chars + 1, text: Some(text.to_string()) })
        })
        .collect()
}

/// Fill in missing preview lines, one round trip for all files.
fn fill_previews(host: &Host, worktree: &str, locs: &mut [Location]) {
    let abs = |p: &str| if p.starts_with('/') { p.to_string() } else { format!("{worktree}/{p}") };
    let mut want: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for l in locs.iter().filter(|l| l.text.is_none()).take(MAX_PREVIEWS) {
        want.entry(abs(&l.path)).or_default().push(l.line);
    }
    if want.is_empty() {
        return;
    }
    let mut got: BTreeMap<(String, usize), String> = BTreeMap::new();
    match host {
        Host::Local => {
            for (file, lines) in &want {
                let Ok(text) = std::fs::read_to_string(file) else { continue };
                let all: Vec<&str> = text.lines().collect();
                for &n in lines {
                    if let Some(t) = all.get(n.wrapping_sub(1)) {
                        got.insert((file.clone(), n), t.to_string());
                    }
                }
            }
        }
        Host::Ssh(_) => {
            let files: Vec<(&String, &Vec<usize>)> = want.iter().collect();
            let script: String = files
                .iter()
                .enumerate()
                .map(|(i, (file, lines))| {
                    let list = lines.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
                    format!(
                        "printf '\\036{i}\\n'; awk -v L={list} 'BEGIN{{n=split(L,a,\",\");for(i=1;i<=n;i++)w[a[i]]=1}} (NR in w){{print NR\":\"$0}}' {} 2>/dev/null\n",
                        shell_quote(file)
                    )
                })
                .collect();
            let Ok(out) = host.sh_ok(&script, None) else { return };
            let mut file: Option<&String> = None;
            for line in out.lines() {
                if let Some(i) = line.strip_prefix('\u{1e}') {
                    file = i.parse::<usize>().ok().and_then(|i| files.get(i)).map(|f| f.0);
                } else if let (Some(f), Some((n, t))) = (file, line.split_once(':')) {
                    if let Ok(n) = n.parse() {
                        got.insert((f.clone(), n), t.to_string());
                    }
                }
            }
        }
    }
    for l in locs.iter_mut().filter(|l| l.text.is_none()) {
        l.text = got.get(&(abs(&l.path), l.line)).cloned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_output_to_locations() {
        let locs = parse_grep("a/b.py:3:5:    foo(x)\nc.py:10:6:é = foo:bar\nnot a match\n");
        assert_eq!(locs.len(), 2);
        assert_eq!((locs[0].path.as_str(), locs[0].line, locs[0].col), ("a/b.py", 3, 5));
        assert_eq!(locs[1].col, 5, "byte column → characters");
        assert_eq!(locs[1].text.as_deref(), Some("é = foo:bar"));
    }

    use crate::panels::PanelCmd;
    use crate::test_util::{Harness, TestRepo};

    /// A viewer on a fresh git repo holding `files`.
    fn harness(files: &[(&str, &str)]) -> (Harness, Viewer) {
        let h = Harness::new(TestRepo::with_files(files));
        let v = h.viewer();
        (h, v)
    }

    fn git(dir: &str, sh: &str) {
        assert!(std::process::Command::new("sh").arg("-c").arg(sh).current_dir(dir).status().unwrap().success());
    }

    /// Provider answers move the cursor (raw columns → display columns),
    /// open other files, list several answers, and `None` falls back to grep.
    #[test]
    fn provider_answers_drive_the_viewer() {
        let lines: Vec<String> = (1..=300).map(|i| if i == 123 { "\tself._loader = Loader()".to_string() } else { format!("x{i} = {i}") }).collect();
        let m = lines.join("\n") + "\n";
        let (h, mut v) = harness(&[("m.py", &m), ("n.py", "a = 1\nb = 2\nuse_loader = _loader\n")]);
        v.open("m.py", Some(216), false);
        h.settle(&mut v, &|v| v.files.contains_key("m.py") && v.current_source_line() == Some(216));

        // One answer in this file, after a tab: line 123, raw col 3 → display col 5.
        let t = v.start_nav(NavKind::Definition, "_loader", v.here(), None, false, true);
        v.finish_nav(t, Some(vec![Location { path: "m.py".into(), line: 123, col: 3, text: None }]));
        h.settle(&mut v, &|v| v.pending_goto.is_none());
        assert_eq!((v.current_source_line(), v.vim.col), (Some(123), 5));
        assert_eq!(v.back.last().map(|l| l.line), Some(216), "jump list");

        // Another file.
        let t = v.start_nav(NavKind::Definition, "_loader", v.here(), None, false, true);
        v.finish_nav(t, Some(vec![Location { path: "n.py".into(), line: 3, col: 14, text: None }]));
        h.settle(&mut v, &|v| v.current.as_deref() == Some("n.py") && v.files.contains_key("n.py") && v.pending_goto.is_none());
        assert_eq!((v.current_source_line(), v.vim.col), (Some(3), 13));

        // A stale answer (older token) is ignored.
        let old = v.start_nav(NavKind::Definition, "a", v.here(), None, false, true);
        let _new = v.start_nav(NavKind::Definition, "b", v.here(), None, false, true);
        v.finish_nav(old, Some(vec![Location { path: "m.py".into(), line: 1, col: 1, text: None }]));
        assert_eq!(v.current.as_deref(), Some("n.py"));

        // "Can't tell" references: git grep finds both files, listed in the picker, the current file first.
        let t = v.start_nav(NavKind::References, "_loader", v.here(), None, false, true);
        v.finish_nav(t, None);
        h.settle(&mut v, &|v| v.picker.is_some());
        let (kind, picker) = v.picker.as_ref().unwrap();
        assert!(matches!(kind, PickerKind::RawLocs(l) if l.len() == 2));
        assert_eq!(picker.items.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), vec!["n.py:3", "m.py:123"]);
    }

    fn repo() -> (Harness, Viewer) {
        let (h, mut v) = harness(&[
            ("use.py", "from a import foo, bar\nfoo()\nbar()\nnone_here()\n"),
            ("a.py", "def foo():\n    pass\n\n\ndef bar():\n    pass\n"),
            ("b.py", "class foo:\n    pass\n"),
        ]);
        v.open("use.py", Some(2), false);
        h.settle(&mut v, &|v| v.files.contains_key("use.py") && v.current_source_line() == Some(2));
        (h, v)
    }

    fn status(v: &Viewer) -> Option<&str> {
        v.status.as_ref().map(|s| s.0.as_str())
    }

    /// A slow provider is waited for: no text search runs meanwhile.
    #[test]
    fn slow_provider_runs_no_text_search() {
        let (_h, mut v) = repo();
        v.start_nav(NavKind::Definition, "foo", v.here(), None, false, true);
        v.pending_nav.as_mut().unwrap().started = Instant::now() - Duration::from_secs(5);
        v.check_nav_timeout();
        assert!(v.pending_nav.as_ref().is_some_and(|p| p.provider), "still waiting for the provider");
        assert!(v.picker.is_none());
    }

    /// "Can't tell" falls back to the text search, and the status says so.
    #[test]
    fn provider_none_falls_back_to_text_search() {
        let (h, mut v) = repo();
        let t = v.start_nav(NavKind::Definition, "foo", v.here(), None, false, true);
        v.finish_nav(t, None);
        assert_eq!(status(&v), Some("the language server found nothing for foo; searching text…"));
        h.settle(&mut v, &|v| v.picker.is_some());
        assert!(v.status.is_none());
        assert_eq!(v.picker.as_ref().unwrap().1.items.len(), 2);
    }

    #[test]
    fn timeout_falls_back_to_text_search() {
        let (h, mut v) = repo();
        v.start_nav(NavKind::Definition, "foo", v.here(), None, false, true);
        v.pending_nav.as_mut().unwrap().started = Instant::now() - NAV_TIMEOUT * 2;
        v.check_nav_timeout();
        assert_eq!(status(&v), Some("no answer from the language server for foo; searching text…"));
        h.settle(&mut v, &|v| v.picker.is_some());
    }

    /// A definition that is the asked word itself (pytest fixtures) is no answer.
    #[test]
    fn self_answer_falls_back_to_text_search() {
        let (h, mut v) = repo();
        let t = v.start_nav(NavKind::Definition, "foo", v.here(), None, false, true);
        v.pending_nav.as_mut().unwrap().word_col = Some(1);
        v.finish_nav(t, Some(vec![Location { path: "use.py".into(), line: 2, col: 1, text: None }]));
        assert_eq!(status(&v), Some("the language server found nothing for foo; searching text…"));
        h.settle(&mut v, &|v| v.picker.is_some());
    }

    /// An answer that arrives after the user opened another file, moved to
    /// another line or pressed Esc is dropped, with its status.
    #[test]
    fn late_answers_are_dropped() {
        let (h, mut v) = repo();
        let elsewhere = || Some(vec![Location { path: "b.py".into(), line: 1, col: 7, text: None }]);

        let t = v.start_nav(NavKind::Definition, "foo", v.here(), None, false, true);
        v.nav_status(NavKind::Definition.finding("foo"));
        v.open("a.py", Some(1), false);
        assert!(v.pending_nav.is_none() && v.status.is_none(), "opening a file abandons the query");
        v.finish_nav(t, elsewhere());
        assert_eq!(v.current.as_deref(), Some("a.py"));

        v.open("use.py", Some(2), false);
        h.settle(&mut v, &|v| v.current_source_line() == Some(2));
        let t = v.start_nav(NavKind::Definition, "foo", v.here(), None, false, true);
        v.nav_status(NavKind::Definition.finding("foo"));
        v.vim.line += 1;
        assert_eq!(v.current_source_line(), Some(3));
        v.finish_nav(t, elsewhere());
        assert_eq!((v.current.as_deref(), v.status.as_ref()), (Some("use.py"), None), "moved away: dropped");

        v.start_nav(NavKind::Definition, "bar", v.here(), None, false, true);
        v.nav_status(NavKind::Definition.finding("bar"));
        h.key(&mut v, egui::Key::Escape);
        assert!(v.pending_nav.is_none() && v.status.is_none(), "Esc abandons the query");
    }

    #[test]
    fn keyword_is_not_searched() {
        let (_h, mut v) = repo();
        v.nav(NavKind::Definition, "return".into());
        assert!(v.pending_nav.is_none());
        assert_eq!(status(&v), Some("return is a keyword"));
    }

    /// On a removed diff row the text search runs from the next current line
    /// without dropping a definition that sits on it.
    #[test]
    fn removed_row_searches_text_without_dropping_the_next_line() {
        let h = Harness::new(TestRepo::new("printf 'x = 1\\nold = helper()\\ndef helper():\\n    pass\\n' > a.py"));
        h.repo.sh("printf 'x = 1\\ndef helper():\\n    pass\\n' > a.py && git commit -qam change");
        let mut v = h.viewer();
        h.settle(&mut v, &|v| v.current.as_deref() == Some("a.py") && v.bases.contains_key("a.py") && v.hunk_goto.is_none());
        assert!(v.on_removed_row(), "{:?}", v.doc.lines[v.vim.line].text);
        assert_eq!(v.context()["removed"], true);
        v.nav(NavKind::Definition, "helper".into());
        h.settle(&mut v, &|v| v.picker.is_some());
        let (_, picker) = v.picker.as_ref().unwrap();
        assert_eq!(picker.items.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), vec!["a.py:2"], "the def on the next line is kept");
    }

    /// gd asks about the word it shows: its start, after a tab, and in the outline.
    #[test]
    fn word_here_uses_the_word_start() {
        let (h, mut v) = harness(&[("t.py", "\tchat_reply.usage = 1\nclass A:\n    def run(self):\n        pass\n")]);
        v.open("t.py", Some(1), false);
        h.settle(&mut v, &|v| v.files.contains_key("t.py") && v.current_source_line() == Some(1));
        // Display "    chat_reply.usage": the cursor on the `.` (column 14).
        v.vim.col = 14;
        assert_eq!(v.word_here(), Some((13, "usage".to_string())));
        assert_eq!(v.context()["col"], 13);
        v.command(PanelCmd::ToggleOutline);
        h.settle(&mut v, &|v| v.outline && v.doc.lines.len() == 2);
        v.vim.line = 1; // "  def run(self):"
        v.vim.col = 6;
        assert_eq!(v.word_here(), Some((9, "run".to_string())));
    }

    /// The definition search: only the asking file's language, variables
    /// too, imported modules first, then the nearest folders; at most
    /// MAX_DEF_FILES files read, big files skipped.
    #[test]
    fn repo_wide_definition_search() {
        let mut files: Vec<(String, String)> = vec![
            ("pkg/a.py".into(), "from pkg.b import helper\nimport pkg.c\nhelper()\n".into()),
            ("pkg/b.py".into(), "def helper():\n    return 1".into()), // no trailing newline
            ("pkg/c.py".into(), "helper = 2\n".into()),
            ("c.rs".into(), "fn helper() {}\n".into()),
            ("far/deep/x.py".into(), "class helper:\n    pass\n".into()),
            ("big/huge.py".into(), format!("def helper():\n    pass\n{}", "#\n".repeat(MAX_DEF_FILE_BYTES / 2 + 1))),
        ];
        for i in 0..25 {
            files.push((format!("z/m{i:02}.py"), "class helper:\n    pass\n".into()));
        }
        let dir = std::env::temp_dir().join(format!("kt-defs-{}", kitchen_core::util::new_id()));
        for (p, t) in &files {
            std::fs::create_dir_all(dir.join(p).parent().unwrap()).unwrap();
            std::fs::write(dir.join(p), t).unwrap();
        }
        let d = dir.display().to_string();
        git(&d, "git init -q && git add .");

        let imports = kitchen_symbols::python_imports(&files[0].1);
        let hits = search_definitions(&Host::Local, &d, "helper", "pkg/a.py", &imports);
        let found: Vec<(&str, usize, usize)> = hits.iter().map(|l| (l.path.as_str(), l.line, l.col)).collect();
        assert_eq!(&found[..3], &[("pkg/b.py", 1, 5), ("pkg/c.py", 1, 1), ("far/deep/x.py", 1, 7)], "imported modules first, raw columns");
        assert!(found.len() <= MAX_DEF_FILES, "{found:?}");
        assert!(!found.iter().any(|f| f.0.ends_with(".rs")), "only Python for a .py file");
        assert!(!found.iter().any(|f| f.0.starts_with("big/")), "files over the limit are skipped");
        assert_eq!(hits[0].text.as_deref(), Some("def helper():"));

        // Rust from Rust; nothing for an unknown name.
        let hits = search_definitions(&Host::Local, &d, "helper", "x.rs", &[]);
        assert_eq!(hits.iter().map(|l| l.path.as_str()).collect::<Vec<_>>(), vec!["c.rs"]);
        assert!(search_definitions(&Host::Local, &d, "nope", "x.rs", &[]).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A single text match is still a list: it may be another `bar` entirely.
    #[test]
    fn text_fallback_opens_a_list_even_for_one_match() {
        let (h, mut v) = repo();
        v.open("use.py", Some(3), false);
        h.settle(&mut v, &|v| v.current_source_line() == Some(3));
        v.nav(NavKind::Definition, "bar".into());
        h.settle(&mut v, &|v| v.picker.is_some());
        let (_, picker) = v.picker.as_ref().unwrap();
        assert_eq!(picker.items.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), vec!["a.py:5"]);
        assert_eq!(picker.title, "definitions of bar (text matches)");
        assert_eq!(v.current.as_deref(), Some("use.py"));
    }

    /// A name the scope walk binds in this file jumps there, no search.
    #[test]
    fn exact_local_hit_jumps() {
        let (h, mut v) = harness(&[("f.py", "def f(x):\n    y = x\n    return y\n")]);
        v.open("f.py", Some(3), false);
        h.settle(&mut v, &|v| v.files.contains_key("f.py") && v.current_source_line() == Some(3));
        v.vim.col = 11;
        v.nav(NavKind::Definition, "y".into());
        h.settle(&mut v, &|v| v.pending_goto.is_none());
        assert!(v.pending_nav.is_none(), "no text search");
        assert_eq!((v.current_source_line(), v.vim.col), (Some(2), 4));
    }

    #[test]
    fn grep_refs_ranks_current_folder_first_and_reports_cap() {
        let many = "foo\n".repeat(MAX_REFS + 1);
        let files = [("a/x.py", "foo\n"), ("a/y.py", "foo\n"), ("b/z.py", "foo\n"), ("c.rs", "foo\n"), ("m.py", many.as_str())];
        let dir = std::env::temp_dir().join(format!("kt-refs-{}", kitchen_core::util::new_id()));
        for (p, t) in &files {
            std::fs::create_dir_all(dir.join(p).parent().unwrap()).unwrap();
            std::fs::write(dir.join(p), t).unwrap();
        }
        let d = dir.display().to_string();
        git(&d, "git init -q && git add .");
        let (locs, capped) = grep_refs(&Host::Local, &d, "foo", "a/y.py");
        assert_eq!(locs.iter().take(2).map(|l| l.path.as_str()).collect::<Vec<_>>(), vec!["a/y.py", "a/x.py"]);
        assert!(capped && locs.len() == MAX_REFS);
        assert!(!locs.iter().any(|l| l.path == "c.rs"), "only Python");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn loc(path: &str, line: usize, col: usize) -> Location {
        Location { path: path.into(), line, col, text: None }
    }

    #[test]
    fn picker_labels_put_file_and_line_first() {
        let (_, items) = picker_items(NavKind::Definition, "x", &[loc("pplx/python/apps/aster/src/aster/common/env.py", 12, 1), loc("top.py", 3, 1), loc("/Users/me/.venv/lib/python3.12/site-packages/httpx/_client.py", 40, 5)], NavSource::Provider);
        let labels: Vec<&str> = items.iter().map(|i| i.0.as_str()).collect();
        assert_eq!(labels, vec!["env.py:12  pplx/python/apps/aster/src/aster/common/", "top.py:3", "_client.py:40  …/site-packages/httpx/"]);
    }

    #[test]
    fn duplicate_line_rows_show_columns() {
        let (_, items) = picker_items(NavKind::References, "s", &[loc("a.py", 8, 11), loc("a.py", 8, 25), loc("a.py", 9, 1)], NavSource::Provider);
        assert_eq!(items.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), vec!["a.py:8:11", "a.py:8:25", "a.py:9"]);
    }

    #[test]
    fn capped_references_title_says_500_plus() {
        let locs: Vec<Location> = (1..=MAX_REFS).map(|i| loc("a.py", i, 1)).collect();
        assert_eq!(picker_items(NavKind::References, "x", &locs, NavSource::Text { capped: true }).0, "500+ references to x (text matches)");
        assert_eq!(picker_items(NavKind::References, "x", &locs[..2], NavSource::Provider).0, "2 references to x");
    }

    /// `gr` whose only result is the spot asked from says so instead of jumping in place.
    #[test]
    fn single_reference_at_cursor_does_not_jump() {
        let (_h, mut v) = repo();
        let t = v.start_nav(NavKind::References, "foo", v.here(), None, false, true);
        v.finish_nav(t, Some(vec![loc("use.py", 2, 1)]));
        assert_eq!(status(&v), Some("the only reference to foo is here"));
        assert!(v.picker.is_none() && v.back.is_empty());
    }

    #[test]
    fn batch_output_splits_per_file() {
        let out = "\u{1e}n1 a.py\none\ntwo\n\n\u{1e}n1 b.py\n\n\u{1e}n1 c.py\nlast\n\u{1e}other x\n";
        assert_eq!(split_batch(out, "n1"), vec![("a.py", "one\ntwo\n"), ("b.py", ""), ("c.py", "last\n\u{1e}other x")]);
    }

    #[test]
    fn previews_from_files() {
        let dir = std::env::temp_dir().join(format!("kt-previews-{}", kitchen_core::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m.py"), "one\ntwo\nthree\n").unwrap();
        let wt = dir.display().to_string();
        let mut locs = vec![
            Location { path: "m.py".into(), line: 3, col: 1, text: None },
            Location { path: format!("{wt}/m.py"), line: 1, col: 1, text: None },
            Location { path: "m.py".into(), line: 2, col: 1, text: Some("kept".into()) },
            Location { path: "m.py".into(), line: 99, col: 1, text: None },
        ];
        fill_previews(&Host::Local, &wt, &mut locs);
        let texts: Vec<Option<&str>> = locs.iter().map(|l| l.text.as_deref()).collect();
        assert_eq!(texts, vec![Some("three"), Some("one"), Some("kept"), None]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
