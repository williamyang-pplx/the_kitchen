//! `gd` / `gr`: ask a plugin provider (e.g. a language server) and wait for
//! it; no answer, an error, a self-answer or a timeout falls back to a text
//! search. Answers that arrive after the user left the spot are dropped.

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
/// Most definition candidate files read per jump (one round trip).
const MAX_DEF_FILES: usize = 20;
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
    /// `gd`: a provider's answer, else the symbol/grep search.
    pub(super) fn jump_to_def(&mut self, name: String) {
        self.nav(NavKind::Definition, name);
    }

    /// `gr`: a provider's answer, else `git grep -w`.
    pub(super) fn find_references(&mut self, name: String) {
        self.nav(NavKind::References, name);
    }

    fn nav(&mut self, kind: NavKind, name: String) {
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

    fn start_nav(&mut self, kind: NavKind, name: &str, provider: bool) -> u64 {
        self.nav_token += 1;
        self.status = None;
        let (token, here) = (self.nav_token, self.here());
        self.pending_nav = Some(PendingNav { token, kind, name: name.to_string(), started: Instant::now(), here, word_col: None, removed: false, provider, status: None });
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
        let token = self.start_nav(kind, name, true);
        if let Some(p) = self.pending_nav.as_mut() {
            p.word_col = word_col;
        }
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

    /// The text search: no provider, or it had nothing. `gd`: a unique
    /// definition in this file jumps straight there. `status` says why it runs.
    fn text_search(&mut self, kind: NavKind, name: String, here: Option<Loc>, word_col: Option<usize>, removed: bool, status: Option<String>) {
        if kind == NavKind::Definition && !removed {
            let Some(here) = here.clone() else {
                self.status = None;
                return;
            };
            let local = self.local_defs(&name, &here.path);
            if let [only] = local.as_slice() {
                if only.line != here.line {
                    self.status = None;
                    let loc = Loc { path: only.path.clone(), line: only.line, col: only.col };
                    self.record_and_go_raw(loc);
                    return;
                }
            }
        }
        self.start_nav(kind, &name, false);
        if let Some(p) = self.pending_nav.as_mut() {
            (p.here, p.word_col, p.removed) = (here, word_col, removed);
        }
        self.nav_status(status.unwrap_or_else(|| format!("searching for {name}…")));
        self.start_grep();
    }

    /// Definitions of `name` in the loaded file `path`.
    fn local_defs(&self, name: &str, path: &str) -> Vec<Location> {
        let Some(f) = self.files.get(path) else { return Vec::new() };
        f.symbols.iter().filter(|s| s.name == name).map(|s| def_location(path, &f.text, s)).collect()
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
                let local = self.local_defs(&name, &here.path);
                std::thread::spawn(move || {
                    let mut locs = local;
                    for l in search_definitions(&host, &wt, &name, &here.path) {
                        if !locs.iter().any(|k| k.path == l.path && k.line == l.line) {
                            locs.push(l);
                        }
                    }
                    // Drop the definition we're standing on (on a removed row
                    // `here` is the next line, which may well be the answer).
                    if !removed {
                        locs.retain(|l| !(l.path == here.path && l.line == here.line));
                    }
                    let _ = tx.send(Msg::Grep { token, locs });
                    ctx.request_repaint();
                });
            }
            NavKind::References => {
                std::thread::spawn(move || {
                    let _ = tx.send(Msg::Grep { token, locs: grep_refs(&host, &wt, &name) });
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
                self.show_locations(p.kind, &p.name, locs);
            }
            None => {
                let msg = format!("the language server found nothing for {}; searching text…", p.name);
                self.text_search(p.kind, p.name, p.here, p.word_col, p.removed, Some(msg));
            }
        }
    }

    /// The text search's answer.
    pub(super) fn finish_grep(&mut self, token: u64, locs: Vec<Location>) {
        let Some(p) = self.pending_nav.take_if(|p| p.token == token) else { return };
        log::info!(target: "kitchen::nav", "{:?} {}: text search → {} in {:?}", p.kind, p.name, locs.len(), p.started.elapsed());
        self.clear_nav_status(&p);
        if !self.still_here(&p) {
            return;
        }
        self.show_locations(p.kind, &p.name, locs);
    }

    fn show_locations(&mut self, kind: NavKind, name: &str, locs: Vec<Location>) {
        match locs.len() {
            0 => self.status = Some((kind.nothing(name), true)),
            1 => self.record_and_go_raw(loc_of(&locs[0])),
            _ => {
                let (title, items) = picker_items(kind, name, &locs);
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

fn picker_items(kind: NavKind, name: &str, locs: &[Location]) -> (String, Vec<(String, String)>) {
    let title = match kind {
        NavKind::Definition => format!("definitions of {name}"),
        NavKind::References => format!("{} references to {name}", locs.len()),
    };
    let items = locs.iter().map(|l| (format!("{}:{}", l.path, l.line), l.text.as_deref().unwrap_or("").trim().to_string())).collect();
    (title, items)
}

/// A symbol in `text` (the file at `path`) as a location: 1-based raw column,
/// and `kind signature` as its preview.
fn def_location(path: &str, text: &str, s: &Symbol) -> Location {
    let line = text.lines().nth(s.line.saturating_sub(1)).unwrap_or("");
    // Symbol columns are 0-based bytes.
    let chars = line.get(..s.col).map(|p| p.chars().count()).unwrap_or(s.col);
    Location { path: path.to_string(), line: s.line, col: chars + 1, text: Some(format!("{} {}", s.kind, s.signature)) }
}

/// Repo-wide definition search in one round trip: git grep keeps the files
/// with a line that mentions `name` and a definition keyword, ranks them
/// (`from`, then its folder, then the rest) and prints the first
/// [`MAX_DEF_FILES`], each after a `\x1e<nonce> <path>` line. They are parsed here.
pub(super) fn search_definitions(host: &Host, worktree: &str, name: &str, from: &str) -> Vec<Location> {
    let nonce = kitchen_core::util::new_id();
    let from_dir = from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let script = format!(
        "cd {wt} && N={nonce} && \
         git -c core.quotePath=false grep -l -I -w -E -e {name} --and -e {keywords} -- \
         | F={from} D={dir} awk 'BEGIN {{ f = ENVIRON[\"F\"]; d = ENVIRON[\"D\"] }} \
             {{ p = $0; q = p; if (!sub(/\\/[^\\/]*$/, \"\", q)) q = \"\"; print (p != f) (q != d) \"\\t\" p }}' \
         | LC_ALL=C sort | cut -f2- \
         | {{ n=0; while [ \"$n\" -lt {max} ] && IFS= read -r p; do \
               [ \"$(wc -c < \"$p\" | tr -d ' ')\" -le {bytes} ] || continue; \
               n=$((n+1)); printf '\\036%s %s\\n' \"$N\" \"$p\"; cat -- \"$p\"; echo; \
             done; }}",
        wt = shell_quote(worktree),
        name = shell_quote(&regex::escape(name)),
        keywords = shell_quote("(def|class|fn|func|function|struct|enum|trait|interface|type|const|let|var|impl|mod|module)"),
        from = shell_quote(from),
        dir = shell_quote(from_dir),
        max = MAX_DEF_FILES,
        bytes = MAX_DEF_FILE_BYTES,
    );
    let Ok(out) = host.sh(&script, None) else { return Vec::new() };
    split_batch(&out.stdout, &nonce)
        .into_iter()
        .flat_map(|(path, text)| kitchen_symbols::extract(path, text).into_iter().filter(|s| s.name == name).map(|s| def_location(path, text, &s)).collect::<Vec<_>>())
        .collect()
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

/// `git grep -w` for `name`: the first [`MAX_REFS`] whole-word occurrences, with their lines.
fn grep_refs(host: &Host, worktree: &str, name: &str) -> Vec<Location> {
    let script = format!(
        "git -C {} -c core.quotePath=false grep -n -I -w -F --column --no-color -e {} -- | head -n {MAX_REFS}",
        shell_quote(worktree),
        shell_quote(name)
    );
    let Ok(out) = host.sh(&script, None) else { return Vec::new() };
    parse_grep(&out.stdout)
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
        .take(MAX_REFS)
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
        let t = v.start_nav(NavKind::Definition, "_loader", true);
        v.finish_nav(t, Some(vec![Location { path: "m.py".into(), line: 123, col: 3, text: None }]));
        h.settle(&mut v, &|v| v.pending_goto.is_none());
        assert_eq!((v.current_source_line(), v.vim.col), (Some(123), 5));
        assert_eq!(v.back.last().map(|l| l.line), Some(216), "jump list");

        // Another file.
        let t = v.start_nav(NavKind::Definition, "_loader", true);
        v.finish_nav(t, Some(vec![Location { path: "n.py".into(), line: 3, col: 14, text: None }]));
        h.settle(&mut v, &|v| v.current.as_deref() == Some("n.py") && v.files.contains_key("n.py") && v.pending_goto.is_none());
        assert_eq!((v.current_source_line(), v.vim.col), (Some(3), 13));

        // A stale answer (older token) is ignored.
        let old = v.start_nav(NavKind::Definition, "a", true);
        let _new = v.start_nav(NavKind::Definition, "b", true);
        v.finish_nav(old, Some(vec![Location { path: "m.py".into(), line: 1, col: 1, text: None }]));
        assert_eq!(v.current.as_deref(), Some("n.py"));

        // "Can't tell" references: git grep finds both files, listed in the picker.
        let t = v.start_nav(NavKind::References, "_loader", true);
        v.finish_nav(t, None);
        h.settle(&mut v, &|v| v.picker.is_some());
        let (kind, picker) = v.picker.as_ref().unwrap();
        assert!(matches!(kind, PickerKind::RawLocs(l) if l.len() == 2));
        assert_eq!(picker.items.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), vec!["m.py:123", "n.py:3"]);
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
        v.start_nav(NavKind::Definition, "foo", true);
        v.pending_nav.as_mut().unwrap().started = Instant::now() - Duration::from_secs(5);
        v.check_nav_timeout();
        assert!(v.pending_nav.as_ref().is_some_and(|p| p.provider), "still waiting for the provider");
        assert!(v.picker.is_none());
    }

    /// "Can't tell" falls back to the text search, and the status says so.
    #[test]
    fn provider_none_falls_back_to_text_search() {
        let (h, mut v) = repo();
        let t = v.start_nav(NavKind::Definition, "foo", true);
        v.finish_nav(t, None);
        assert_eq!(status(&v), Some("the language server found nothing for foo; searching text…"));
        h.settle(&mut v, &|v| v.picker.is_some());
        assert!(v.status.is_none());
        assert_eq!(v.picker.as_ref().unwrap().1.items.len(), 2);
    }

    #[test]
    fn timeout_falls_back_to_text_search() {
        let (h, mut v) = repo();
        v.start_nav(NavKind::Definition, "foo", true);
        v.pending_nav.as_mut().unwrap().started = Instant::now() - NAV_TIMEOUT * 2;
        v.check_nav_timeout();
        assert_eq!(status(&v), Some("no answer from the language server for foo; searching text…"));
        h.settle(&mut v, &|v| v.picker.is_some());
    }

    /// A definition that is the asked word itself (pytest fixtures) is no answer.
    #[test]
    fn self_answer_falls_back_to_text_search() {
        let (h, mut v) = repo();
        let t = v.start_nav(NavKind::Definition, "foo", true);
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

        let t = v.start_nav(NavKind::Definition, "foo", true);
        v.nav_status(NavKind::Definition.finding("foo"));
        v.open("a.py", Some(1), false);
        assert!(v.pending_nav.is_none() && v.status.is_none(), "opening a file abandons the query");
        v.finish_nav(t, elsewhere());
        assert_eq!(v.current.as_deref(), Some("a.py"));

        v.open("use.py", Some(2), false);
        h.settle(&mut v, &|v| v.current_source_line() == Some(2));
        let t = v.start_nav(NavKind::Definition, "foo", true);
        v.nav_status(NavKind::Definition.finding("foo"));
        v.vim.line += 1;
        assert_eq!(v.current_source_line(), Some(3));
        v.finish_nav(t, elsewhere());
        assert_eq!((v.current.as_deref(), v.status.as_ref()), (Some("use.py"), None), "moved away: dropped");

        v.start_nav(NavKind::Definition, "bar", true);
        v.nav_status(NavKind::Definition.finding("bar"));
        h.key(&mut v, egui::Key::Escape);
        assert!(v.pending_nav.is_none() && v.status.is_none(), "Esc abandons the query");
    }

    #[test]
    fn keyword_is_not_searched() {
        let (_h, mut v) = repo();
        v.jump_to_def("return".into());
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
        v.jump_to_def("helper".into());
        h.settle(&mut v, &|v| v.pending_nav.is_none() && v.pending_goto.is_none());
        assert!(!v.on_removed_row() && v.current_source_line() == Some(2), "jumped to the def on the next line");
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
        v.command(PanelCmd::ToggleOutline, &h.engine);
        h.settle(&mut v, &|v| v.outline && v.doc.lines.len() == 2);
        v.vim.line = 1; // "  def run(self):"
        v.vim.col = 6;
        assert_eq!(v.word_here(), Some((9, "run".to_string())));
    }

    /// One round trip finds the definitions: ranked from the asking file's
    /// folder, at most MAX_DEF_FILES files read, big files skipped.
    #[test]
    fn repo_wide_definition_search() {
        let mut files: Vec<(String, String)> = vec![
            ("pkg/a.py".into(), "from b import helper\nhelper()\n".into()),
            ("pkg/b.py".into(), "def helper():\n    return 1".into()), // no trailing newline
            ("c.rs".into(), "fn helper() {}\n".into()),
            ("top.rs".into(), "\tfn helper() {}\n".into()),
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

        let hits = search_definitions(&Host::Local, &d, "helper", "pkg/a.py");
        let found: Vec<(&str, usize, usize)> = hits.iter().map(|l| (l.path.as_str(), l.line, l.col)).collect();
        assert_eq!(found.len(), MAX_DEF_FILES, "{found:?}");
        assert_eq!(&found[..3], &[("pkg/b.py", 1, 5), ("c.rs", 1, 4), ("top.rs", 1, 5)], "own folder first; raw columns");
        assert!(!found.iter().any(|f| f.0.starts_with("big/")), "files over the limit are skipped");
        assert_eq!(hits[0].text.as_deref(), Some("function def helper():"));

        // A top-level file ranks its top-level neighbours first.
        let hits = search_definitions(&Host::Local, &d, "helper", "x.rs");
        assert_eq!(hits.iter().take(2).map(|l| l.path.as_str()).collect::<Vec<_>>(), vec!["c.rs", "top.rs"]);
        assert!(search_definitions(&Host::Local, &d, "nope", "x.rs").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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
