//! Test fixtures: a git repo on disk, an engine whose one workspace holds
//! it, and a viewer on it driven frame by frame.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::engine::Engine;
use crate::panels::PanelCtx;
use crate::viewer::Viewer;

/// A git repo in a fresh temp dir, removed on drop.
pub struct TestRepo {
    pub dir: PathBuf,
}

impl TestRepo {
    /// `setup` (sh, run in the repo) makes the files; they become one
    /// commit that `origin/main` points at.
    pub fn new(setup: &str) -> TestRepo {
        TestRepo::create(&[], setup)
    }

    pub fn with_files(files: &[(&str, &str)]) -> TestRepo {
        TestRepo::create(files, "true")
    }

    fn create(files: &[(&str, &str)], setup: &str) -> TestRepo {
        let repo = TestRepo { dir: std::env::temp_dir().join(format!("kt-test-{}", kitchen_core::util::new_id())) };
        std::fs::create_dir_all(&repo.dir).unwrap();
        for (path, text) in files {
            let p = repo.dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        repo.sh(&format!(
            "git init -q -b main && git config user.email t@t && git config user.name t && {setup} \
             && git add . && git commit -qm i && git update-ref refs/remotes/origin/main HEAD"
        ));
        repo
    }

    pub fn path(&self) -> String {
        self.dir.display().to_string()
    }

    pub fn sh(&self, sh: &str) {
        assert!(std::process::Command::new("sh").arg("-c").arg(sh).current_dir(&self.dir).status().unwrap().success(), "{sh}");
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// An engine with one workspace `w` on `host` at `wt`, with a tab per id
/// in `tabs` (all on `wt`).
pub fn engine(host: &str, wt: &str, tabs: &[&str]) -> (egui::Context, Arc<Engine>) {
    use kitchen_core::db::{Db, TabRow, WorkspaceRow};
    let ctx = egui::Context::default();
    crate::theme::install(&ctx);
    let db = Db::open_in_memory().unwrap();
    let ws = WorkspaceRow { id: "w".into(), name: "w".into(), host: host.into(), repo_path: wt.into(), github_slug: None, default_branch: Some("main".into()), position: 0, pinned: false };
    db.insert_workspace(&ws).unwrap();
    for (i, id) in tabs.iter().enumerate() {
        let tab = TabRow { id: id.to_string(), workspace_id: "w".into(), title: id.to_string(), worktree: wt.into(), branch: None, tmux_session: id.to_string(), position: i as i64, agent: true, agent_session: None };
        db.insert_tab(&tab).unwrap();
    }
    let engine = Engine::new(ctx.clone(), db).unwrap();
    (ctx, engine)
}

/// A viewer's surroundings: the engine of a one-tab workspace on `repo`.
pub struct Harness {
    pub ctx: egui::Context,
    pub engine: Arc<Engine>,
    pub repo: TestRepo,
}

impl Harness {
    pub fn new(repo: TestRepo) -> Harness {
        let (ctx, engine) = engine("local", &repo.path(), &["t"]);
        Harness { ctx, engine, repo }
    }

    /// A viewer on the tab.
    pub fn viewer(&self) -> Viewer {
        let w = self.engine.workspaces()[0].clone();
        Viewer::new(&self.engine, &w, &w.tabs[0])
    }

    pub fn frame_with(&self, v: &mut Viewer, events: Vec<egui::Event>) {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 700.0))), events, ..Default::default() };
        let mut out = self.ctx.run_ui(input, |ui| {
            let mut cx = PanelCtx { engine: &self.engine, focused: true, clicked: false };
            v.ui(ui, &mut cx);
        });
        out.textures_delta.clear();
    }

    pub fn frame(&self, v: &mut Viewer) {
        self.frame_with(v, Vec::new());
    }

    pub fn text(&self, v: &mut Viewer, text: &str) {
        self.frame_with(v, vec![egui::Event::Text(text.into())]);
    }

    pub fn key(&self, v: &mut Viewer, key: egui::Key) {
        let ev = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        self.frame_with(v, vec![ev]);
    }

    /// Frames until `done` (background reads land between them).
    pub fn settle(&self, v: &mut Viewer, done: &dyn Fn(&Viewer) -> bool) {
        for _ in 0..300 {
            self.frame(v);
            if done(v) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("viewer never settled");
    }
}
