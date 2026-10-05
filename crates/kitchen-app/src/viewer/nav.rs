//! `gd` / `gr`: ask a plugin provider (e.g. a language server) first, and
//! fall back to text search when there is none, it declines, or it is slow.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use kitchen_core::host::Host;
use kitchen_core::util::shell_quote;

use super::picker::Picker;
use super::{Loc, Msg, PickerKind, Viewer};
use crate::plugins::Location;

/// A provider that hasn't answered by then is abandoned for text search.
const NAV_TIMEOUT: Duration = Duration::from_secs(20);
/// Most text-search references listed.
const MAX_REFS: usize = 500;
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
}

pub(super) struct PendingNav {
    token: u64,
    kind: NavKind,
    name: String,
    started: Instant,
}

impl Viewer {
    /// `gd`: a provider's answer, else the symbol/grep search.
    pub(super) fn jump_to_def(&mut self, name: String) {
        if !self.ask_provider(NavKind::Definition, &name) {
            self.grep_definition(name);
        }
    }

    /// `gr`: a provider's answer, else `git grep -w`.
    pub(super) fn find_references(&mut self, name: String) {
        if !self.ask_provider(NavKind::References, &name) {
            self.grep_references(name);
        }
    }

    fn start_nav(&mut self, kind: NavKind, name: &str) -> u64 {
        self.nav_token += 1;
        self.pending_nav = Some(PendingNav { token: self.nav_token, kind, name: name.to_string(), started: Instant::now() });
        self.nav_token
    }

    /// Hand the query to a plugin; false when no plugin answers for this file.
    fn ask_provider(&mut self, kind: NavKind, name: &str) -> bool {
        let Some(plugins) = self.engine.plugins().cloned() else { return false };
        let Some(ft) = self.current_filetype() else { return false };
        if !plugins.has_provider(kind.provider(), &ft) {
            return false;
        }
        let ctx = crate::plugins::command_context(&self.engine, &self.tab_id, Some(self.context()));
        let token = self.start_nav(kind, name);
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
        self.engine.ctx.request_repaint_after(NAV_TIMEOUT);
        true
    }

    pub(super) fn finish_nav(&mut self, token: u64, answer: Option<Vec<Location>>) {
        let Some(p) = self.pending_nav.take_if(|p| p.token == token) else { return };
        self.status = None;
        match (p.kind, answer) {
            (NavKind::Definition, None) => self.grep_definition(p.name),
            (NavKind::References, None) => self.grep_references(p.name),
            (kind, Some(locs)) => self.show_locations(kind, &p.name, locs),
        }
    }

    /// A provider that never answers must not leave gd/gr hanging.
    pub(super) fn check_nav_timeout(&mut self) {
        if !self.pending_nav.as_ref().is_some_and(|p| p.started.elapsed() > NAV_TIMEOUT) {
            return;
        }
        let Some(p) = self.pending_nav.take() else { return };
        self.engine.toast_err(format!("no answer about {} from the language server; using text search", p.name));
        match p.kind {
            NavKind::Definition => self.grep_definition(p.name),
            NavKind::References => self.grep_references(p.name),
        }
    }

    fn grep_references(&mut self, name: String) {
        let token = self.start_nav(NavKind::References, &name);
        self.status = Some((format!("searching for {name}…"), false));
        let (tx, ctx, host, wt) = (self.tx.clone(), self.engine.ctx.clone(), self.host.clone(), self.worktree.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Nav { token, answer: Some(grep_refs(&host, &wt, &name)) });
            ctx.request_repaint();
        });
    }

    fn show_locations(&mut self, kind: NavKind, name: &str, locs: Vec<Location>) {
        let loc = |l: &Location| Loc { path: l.path.clone(), line: l.line, col: l.col };
        match locs.len() {
            0 if kind == NavKind::Definition => self.status = Some((format!("no definition found for {name}"), true)),
            0 => self.status = Some((format!("no references to {name}"), true)),
            1 => self.record_and_go_raw(loc(&locs[0])),
            n => {
                let title = match kind {
                    NavKind::Definition => format!("definitions of {name}"),
                    NavKind::References => format!("{n} references to {name}"),
                };
                let items = locs.iter().map(|l| (format!("{}:{}", l.path, l.line), l.text.as_deref().unwrap_or("").trim().to_string())).collect();
                self.picker = Some((PickerKind::RawLocs(locs.iter().map(loc).collect()), Picker::new(title, items)));
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

/// `git grep -w` for `name`: every whole-word occurrence, with its line.
fn grep_refs(host: &Host, worktree: &str, name: &str) -> Vec<Location> {
    let Ok(out) = host.run(&["git", "-C", worktree, "grep", "-n", "-I", "-w", "--column", "--no-color", "-e", name], None) else {
        return Vec::new();
    };
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

    /// Provider answers move the cursor (raw columns → display columns),
    /// open other files, list several answers, and `None` falls back to grep.
    #[test]
    fn provider_answers_drive_the_viewer() {
        use crate::panels::PanelCtx;
        let tmp = std::env::temp_dir().join(format!("kt-nav-{}", kitchen_core::util::new_id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let d = tmp.display().to_string();
        let lines: Vec<String> = (1..=300).map(|i| if i == 123 { "\tself._loader = Loader()".to_string() } else { format!("x{i} = {i}") }).collect();
        std::fs::write(tmp.join("m.py"), lines.join("\n") + "\n").unwrap();
        std::fs::write(tmp.join("n.py"), "a = 1\nb = 2\nuse_loader = _loader\n").unwrap();
        let run = |sh: &str| assert!(std::process::Command::new("sh").arg("-c").arg(sh).current_dir(&d).status().unwrap().success());
        run("git init -q -b main && git config user.email t@t && git config user.name t && git add . && git commit -qm i && git update-ref refs/remotes/origin/main HEAD");
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let db = kitchen_core::db::Db::open_in_memory().unwrap();
        let ws = kitchen_core::db::WorkspaceRow { id: "w".into(), name: "w".into(), host: "local".into(), repo_path: d.clone(), github_slug: None, default_branch: Some("main".into()), position: 0, pinned: false };
        let tab = kitchen_core::db::TabRow { id: "t".into(), workspace_id: "w".into(), kind: "readonly".into(), title: "t".into(), worktree: d.clone(), branch: None, tmux_session: "x".into(), plan_file: None, position: 0, diff_base: None };
        db.insert_workspace(&ws).unwrap();
        db.insert_tab(&tab).unwrap();
        let engine = crate::engine::Engine::new(ctx.clone(), db).unwrap();
        let w = engine.workspaces()[0].clone();
        let mut v = Viewer::new(&engine, &w, &w.tabs[0]);
        let frame = |v: &mut Viewer| {
            let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 700.0))), ..Default::default() }, |ui| {
                let mut cx = PanelCtx { engine: &engine, focused: true, clicked: false };
                v.ui(ui, &mut cx);
            });
            out.textures_delta.clear();
        };
        let settle = |v: &mut Viewer, done: &dyn Fn(&Viewer) -> bool| {
            for _ in 0..200 {
                frame(v);
                if done(v) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("viewer never settled");
        };
        v.open("m.py", Some(216), false);
        settle(&mut v, &|v| v.files.contains_key("m.py") && v.current_source_line() == Some(216));

        // One answer in this file, after a tab: line 123, raw col 3 → display col 5.
        let t = v.start_nav(NavKind::Definition, "_loader");
        v.finish_nav(t, Some(vec![Location { path: "m.py".into(), line: 123, col: 3, text: None }]));
        settle(&mut v, &|v| v.pending_goto.is_none());
        assert_eq!((v.current_source_line(), v.vim.col), (Some(123), 5));
        assert_eq!(v.back.last().map(|l| l.line), Some(216), "jump list");

        // Another file.
        let t = v.start_nav(NavKind::Definition, "_loader");
        v.finish_nav(t, Some(vec![Location { path: "n.py".into(), line: 3, col: 14, text: None }]));
        settle(&mut v, &|v| v.current.as_deref() == Some("n.py") && v.files.contains_key("n.py") && v.pending_goto.is_none());
        assert_eq!((v.current_source_line(), v.vim.col), (Some(3), 13));

        // A stale answer (older token) is ignored.
        let old = v.start_nav(NavKind::Definition, "a");
        let _new = v.start_nav(NavKind::Definition, "b");
        v.finish_nav(old, Some(vec![Location { path: "m.py".into(), line: 1, col: 1, text: None }]));
        assert_eq!(v.current.as_deref(), Some("n.py"));

        // "Can't tell" references: git grep finds both files, listed in the picker.
        let t = v.start_nav(NavKind::References, "_loader");
        v.finish_nav(t, None);
        settle(&mut v, &|v| v.picker.is_some());
        let (kind, picker) = v.picker.as_ref().unwrap();
        assert!(matches!(kind, PickerKind::RawLocs(l) if l.len() == 2));
        assert_eq!(picker.items.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), vec!["m.py:123", "n.py:3"]);
        let _ = std::fs::remove_dir_all(&tmp);
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
