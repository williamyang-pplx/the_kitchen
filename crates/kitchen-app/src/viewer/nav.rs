//! `gd` / `gr`: ask a plugin provider (e.g. a language server) first. A slow
//! provider gets a text search alongside it, shown until it answers; none, or
//! one that declines, gets the text search alone.

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
/// A provider that hasn't answered by then gets a text search alongside it.
const RACE_AFTER: Duration = Duration::from_millis(400);
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
}

/// Where the text search of a nav query is.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Grep {
    NotStarted,
    Running,
    /// Finished with nothing, while the provider still works.
    Empty,
    /// Its results are in the picker, while the provider still works.
    Shown,
}

pub(super) struct PendingNav {
    token: u64,
    kind: NavKind,
    name: String,
    started: Instant,
    /// Cursor when gd/gr was pressed (ranking, and the definition we stand on).
    here: Option<Loc>,
    /// The provider was asked and hasn't answered.
    provider: bool,
    grep: Grep,
}

impl Viewer {
    /// `gd`: a provider's answer, else the symbol/grep search.
    pub(super) fn jump_to_def(&mut self, name: String) {
        if !self.ask_provider(NavKind::Definition, &name) {
            self.text_search(NavKind::Definition, name, self.here());
        }
    }

    /// `gr`: a provider's answer, else `git grep -w`.
    pub(super) fn find_references(&mut self, name: String) {
        if !self.ask_provider(NavKind::References, &name) {
            self.text_search(NavKind::References, name, self.here());
        }
    }

    fn start_nav(&mut self, kind: NavKind, name: &str, provider: bool) -> u64 {
        self.nav_token += 1;
        self.status = None;
        let (token, here) = (self.nav_token, self.here());
        self.pending_nav = Some(PendingNav { token, kind, name: name.to_string(), started: Instant::now(), here, provider, grep: Grep::NotStarted });
        token
    }

    /// Hand the query to a plugin; false when no plugin answers for this file.
    fn ask_provider(&mut self, kind: NavKind, name: &str) -> bool {
        let Some(plugins) = self.engine.plugins().cloned() else { return false };
        let Some(ft) = self.current_filetype() else { return false };
        if !plugins.has_provider(kind.provider(), &ft) {
            return false;
        }
        let ctx = crate::plugins::command_context(&self.engine, &self.tab_id, Some(self.context()));
        let token = self.start_nav(kind, name, true);
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
        let what = if kind == NavKind::Definition { "finding the definition of" } else { "finding references to" };
        self.status = Some((format!("{what} {name}…"), false));
        self.engine.ctx.request_repaint_after(RACE_AFTER);
        true
    }

    /// No provider (or it declined): the text search is the answer.
    /// `gd`: a unique definition in this file jumps straight there.
    fn text_search(&mut self, kind: NavKind, name: String, here: Option<Loc>) {
        if kind == NavKind::Definition {
            let Some(here) = here.clone() else { return };
            let local = self.local_defs(&name, &here.path);
            if let [only] = local.as_slice() {
                if only.line != here.line {
                    let loc = Loc { path: only.path.clone(), line: only.line, col: only.col };
                    self.record_and_go_raw(loc);
                    return;
                }
            }
        }
        self.start_nav(kind, &name, false);
        if let Some(p) = self.pending_nav.as_mut() {
            p.here = here;
        }
        self.status = Some((format!("searching for {name}…"), false));
        self.start_grep();
    }

    /// Definitions of `name` in the loaded file `path`.
    fn local_defs(&self, name: &str, path: &str) -> Vec<Location> {
        let Some(f) = self.files.get(path) else { return Vec::new() };
        f.symbols.iter().filter(|s| s.name == name).map(|s| def_location(path, &f.text, s)).collect()
    }

    /// Spawn the text search for the pending query (`Msg::Grep` comes back).
    fn start_grep(&mut self) {
        let Some(p) = self.pending_nav.as_ref().filter(|p| p.grep == Grep::NotStarted) else { return };
        let (token, name, kind) = (p.token, p.name.clone(), p.kind);
        let (tx, ctx, host, wt) = (self.tx.clone(), self.engine.ctx.clone(), self.host.clone(), self.worktree.clone());
        match kind {
            NavKind::Definition => {
                let Some(here) = p.here.clone() else {
                    // Nowhere to search from; a provider may still answer.
                    if !p.provider {
                        self.pending_nav = None;
                        self.status = Some((kind.nothing(&name), true));
                    }
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
                    // Drop the definition we're standing on.
                    locs.retain(|l| !(l.path == here.path && l.line == here.line));
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
        if let Some(p) = self.pending_nav.as_mut() {
            p.grep = Grep::Running;
        }
    }

    /// Per frame: start the text search once the provider is slow, and stop
    /// waiting for it at [`NAV_TIMEOUT`].
    pub(super) fn check_nav_timeout(&mut self) {
        let Some(p) = self.pending_nav.as_ref().filter(|p| p.provider) else { return };
        let elapsed = p.started.elapsed();
        if p.grep == Grep::NotStarted && elapsed > RACE_AFTER {
            self.start_grep();
        }
        let Some(p) = self.pending_nav.as_mut() else { return };
        if elapsed <= NAV_TIMEOUT {
            let deadline = if p.grep == Grep::NotStarted { RACE_AFTER } else { NAV_TIMEOUT };
            self.engine.ctx.request_repaint_after(deadline.saturating_sub(elapsed).max(Duration::from_millis(10)));
            return;
        }
        log::info!(target: "kitchen::nav", "{:?} {}: no answer from the language server in {NAV_TIMEOUT:?}", p.kind, p.name);
        p.provider = false;
        match p.grep {
            // Its result is final now.
            Grep::Running => {}
            Grep::Shown => {
                self.pending_nav = None;
                self.status = None;
            }
            Grep::Empty | Grep::NotStarted => {
                let msg = p.kind.nothing(&p.name);
                self.pending_nav = None;
                self.status = Some((msg, true));
            }
        }
    }

    /// The provider's answer (`None`: it couldn't tell).
    pub(super) fn finish_nav(&mut self, token: u64, answer: Option<Vec<Location>>) {
        let Some(mut p) = self.pending_nav.take_if(|p| p.token == token) else { return };
        let n = answer.as_ref().map(Vec::len);
        log::info!(target: "kitchen::nav", "{:?} {}: language server → {n:?} in {:?}", p.kind, p.name, p.started.elapsed());
        p.provider = false;
        match (answer, p.grep) {
            (Some(locs), Grep::Shown) => {
                let ours = matches!(self.picker, Some((PickerKind::RawLocs(_), _)));
                self.status = None;
                match locs.len() {
                    _ if !ours => {} // the user moved on to another picker
                    0 => {}          // keep the text matches as the answer
                    1 => {
                        self.picker = None;
                        self.show_locations(p.kind, &p.name, locs);
                    }
                    _ => self.replace_picker(p.kind, &p.name, locs),
                }
            }
            (Some(locs), _) => {
                self.status = None;
                self.show_locations(p.kind, &p.name, locs);
            }
            (None, Grep::NotStarted) => {
                let (kind, name, here) = (p.kind, p.name, p.here);
                self.text_search(kind, name, here);
            }
            (None, Grep::Running) => self.pending_nav = Some(p),
            (None, Grep::Empty) => self.status = Some((p.kind.nothing(&p.name), true)),
            (None, Grep::Shown) => self.status = None,
        }
    }

    /// The text search's answer.
    pub(super) fn finish_grep(&mut self, token: u64, locs: Vec<Location>) {
        let Some(p) = self.pending_nav.as_mut().filter(|p| p.token == token) else { return };
        log::info!(target: "kitchen::nav", "{:?} {}: text search → {} in {:?}", p.kind, p.name, locs.len(), p.started.elapsed());
        let (kind, name) = (p.kind, p.name.clone());
        if !p.provider {
            self.pending_nav = None;
            self.status = None;
            self.show_locations(kind, &name, locs);
            return;
        }
        // The provider still works: show what we have meanwhile.
        match locs.len() {
            0 => {
                p.grep = Grep::Empty;
                self.status = Some((format!("no text match for {name}; waiting for the language server…"), false));
            }
            1 => {
                self.pending_nav = None;
                self.status = None;
                self.show_locations(kind, &name, locs);
            }
            _ => {
                p.grep = Grep::Shown;
                self.show_locations(kind, &name, locs);
                let what = if kind == NavKind::Definition { "finding the definition of" } else { "finding references to" };
                self.status = Some((format!("{what} {name}… (text matches shown)"), false));
            }
        }
    }

    /// The open picker shows the pending query's text matches.
    pub(super) fn provisional_picker(&self) -> bool {
        self.pending_nav.as_ref().is_some_and(|p| p.grep == Grep::Shown) && matches!(self.picker, Some((PickerKind::RawLocs(_), _)))
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

    /// Swap the text matches in the picker for the provider's answer, keeping the query.
    fn replace_picker(&mut self, kind: NavKind, name: &str, locs: Vec<Location>) {
        let Some((PickerKind::RawLocs(old), picker)) = self.picker.as_mut() else { return };
        let (title, items) = picker_items(kind, name, &locs);
        picker.title = title;
        picker.set_items(items, vec![0; locs.len()]);
        *old = locs.iter().map(loc_of).collect();
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

    /// A viewer on a fresh git repo holding `files`, driven frame by frame.
    struct Harness {
        ctx: egui::Context,
        engine: std::sync::Arc<crate::engine::Engine>,
        dir: std::path::PathBuf,
    }

    impl Harness {
        fn new(files: &[(&str, &str)]) -> (Harness, Viewer) {
            let dir = std::env::temp_dir().join(format!("kt-nav-{}", kitchen_core::util::new_id()));
            for (path, text) in files {
                let p = dir.join(path);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, text).unwrap();
            }
            let d = dir.display().to_string();
            git(&d, "git init -q -b main && git config user.email t@t && git config user.name t && git add . && git commit -qm i && git update-ref refs/remotes/origin/main HEAD");
            let ctx = egui::Context::default();
            let db = kitchen_core::db::Db::open_in_memory().unwrap();
            let ws = kitchen_core::db::WorkspaceRow { id: "w".into(), name: "w".into(), host: "local".into(), repo_path: d.clone(), github_slug: None, default_branch: Some("main".into()), position: 0, pinned: false };
            let tab = kitchen_core::db::TabRow { id: "t".into(), workspace_id: "w".into(), kind: "readonly".into(), title: "t".into(), worktree: d.clone(), branch: None, tmux_session: "x".into(), plan_file: None, position: 0, diff_base: None };
            db.insert_workspace(&ws).unwrap();
            db.insert_tab(&tab).unwrap();
            let engine = crate::engine::Engine::new(ctx.clone(), db).unwrap();
            let w = engine.workspaces()[0].clone();
            let v = Viewer::new(&engine, &w, &w.tabs[0]);
            (Harness { ctx, engine, dir }, v)
        }

        fn frame_with(&self, v: &mut Viewer, events: Vec<egui::Event>) {
            use crate::panels::PanelCtx;
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 700.0))), events, ..Default::default() };
            let mut out = self.ctx.run_ui(input, |ui| {
                let mut cx = PanelCtx { engine: &self.engine, focused: true, clicked: false };
                v.ui(ui, &mut cx);
            });
            out.textures_delta.clear();
        }

        fn settle(&self, v: &mut Viewer, done: &dyn Fn(&Viewer) -> bool) {
            for _ in 0..300 {
                self.frame_with(v, Vec::new());
                if done(v) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("viewer never settled");
        }

        fn key(&self, v: &mut Viewer, key: egui::Key) {
            let ev = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
            self.frame_with(v, vec![ev]);
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
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
        let (h, mut v) = Harness::new(&[("m.py", &m), ("n.py", "a = 1\nb = 2\nuse_loader = _loader\n")]);
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

    /// A slow provider: the text search starts after RACE_AFTER and its
    /// results show until the provider answers (or the user acts on them).
    #[test]
    fn slow_provider_races_text_search() {
        let (h, mut v) = Harness::new(&[
            ("use.py", "from a import foo, bar\nfoo()\nbar()\nnone_here()\n"),
            ("a.py", "def foo():\n    pass\n\n\ndef bar():\n    pass\n"),
            ("b.py", "class foo:\n    pass\n"),
        ]);
        v.open("use.py", Some(2), false);
        h.settle(&mut v, &|v| v.files.contains_key("use.py") && v.current_source_line() == Some(2));
        // A provider asked `RACE_AFTER` ago, so this frame starts the text search.
        let ask = |v: &mut Viewer, kind: NavKind, name: &str| {
            let t = v.start_nav(kind, name, true);
            v.pending_nav.as_mut().unwrap().started = Instant::now() - RACE_AFTER * 2;
            t
        };
        let grep_is = |g: Grep| move |v: &Viewer| v.pending_nav.as_ref().is_some_and(|p| p.grep == g);
        let loc = |path: &str, line: usize| Location { path: path.into(), line, col: 1, text: Some("x".into()) };

        // (a) Two text matches: a provisional picker; the provider's answer
        // replaces its items in place and keeps the typed query.
        let t = ask(&mut v, NavKind::Definition, "foo");
        h.settle(&mut v, &grep_is(Grep::Shown));
        assert!(v.provisional_picker());
        let (_, picker) = v.picker.as_mut().unwrap();
        assert_eq!(picker.items.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), vec!["a.py:1", "b.py:1"]);
        picker.query = "a".into();
        v.finish_nav(t, Some(vec![loc("a.py", 1), loc("c.py", 9)]));
        assert!(v.pending_nav.is_none());
        let (kind, picker) = v.picker.as_ref().unwrap();
        assert!(matches!(kind, PickerKind::RawLocs(l) if l.len() == 2 && l[1].path == "c.py"));
        assert_eq!((picker.query.as_str(), picker.items[1].0.as_str()), ("a", "c.py:9"));
        h.key(&mut v, egui::Key::Escape);
        assert!(v.picker.is_none());

        // (b) Cancelling the provisional picker ends the query: a late answer is dropped.
        let t = ask(&mut v, NavKind::Definition, "foo");
        h.settle(&mut v, &grep_is(Grep::Shown));
        h.key(&mut v, egui::Key::Escape);
        assert!(v.picker.is_none() && v.pending_nav.is_none());
        v.finish_nav(t, Some(vec![loc("c.py", 9)]));
        assert_eq!(v.current.as_deref(), Some("use.py"));

        // (g) ⌘P replaces the provisional picker: a late answer leaves it alone.
        let t = ask(&mut v, NavKind::Definition, "foo");
        h.settle(&mut v, &grep_is(Grep::Shown));
        v.open_file_picker();
        assert!(!v.provisional_picker());
        v.finish_nav(t, Some(vec![loc("a.py", 1), loc("c.py", 9)]));
        assert!(matches!(v.picker, Some((PickerKind::Files, _))));
        v.picker = None;

        // (f) The provider's single answer closes the provisional picker and jumps.
        let t = ask(&mut v, NavKind::Definition, "foo");
        h.settle(&mut v, &grep_is(Grep::Shown));
        v.finish_nav(t, Some(vec![Location { path: "a.py".into(), line: 1, col: 5, text: None }]));
        assert!(v.picker.is_none());
        h.settle(&mut v, &|v| v.current.as_deref() == Some("a.py") && v.pending_goto.is_none());
        v.jump_back();
        h.settle(&mut v, &|v| v.current.as_deref() == Some("use.py") && v.pending_goto.is_none());

        // (c) One text match jumps at once; the provider's late answer is dropped.
        let back = v.back.len();
        let t = ask(&mut v, NavKind::Definition, "bar");
        h.settle(&mut v, &|v| v.current.as_deref() == Some("a.py") && v.pending_goto.is_none());
        assert_eq!((v.current_source_line(), v.back.len()), (Some(5), back + 1));
        v.finish_nav(t, Some(vec![loc("c.py", 9)]));
        assert_eq!((v.current.as_deref(), v.back.len()), (Some("a.py"), back + 1));
        v.jump_back();
        h.settle(&mut v, &|v| v.current.as_deref() == Some("use.py") && v.pending_goto.is_none());

        // (e) No text match, then the provider can't tell: "no definition", query over.
        let t = ask(&mut v, NavKind::Definition, "none_here");
        h.settle(&mut v, &grep_is(Grep::Empty));
        v.finish_nav(t, None);
        assert!(v.pending_nav.is_none());
        assert_eq!(v.status, Some(("no definition found for none_here".into(), true)));

        // (d) The provider can't tell while the text search runs: its result is final.
        let t = v.start_nav(NavKind::References, "foo", true);
        v.pending_nav.as_mut().unwrap().started = Instant::now() - RACE_AFTER * 2;
        v.check_nav_timeout();
        assert!(grep_is(Grep::Running)(&v));
        v.finish_nav(t, None);
        h.settle(&mut v, &|v| v.picker.is_some());
        assert!(v.pending_nav.is_none());
        let (_, picker) = v.picker.as_ref().unwrap();
        assert_eq!(picker.title, "4 references to foo");
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
