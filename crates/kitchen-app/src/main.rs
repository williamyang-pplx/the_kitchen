//! the kitchen — a cmux-style workspace manager for agent-driven PR work.

mod api;
mod app;
mod engine;
mod inbox;
mod keymap;
mod panels;
mod plan_view;
mod relay;
mod review;
mod settings;
mod terminal;
mod theme;
mod ui;
mod viewer;

use std::sync::Arc;

fn main() -> eframe::Result {
    kitchen_core::host::augment_local_path();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let db_path = kitchen_proto::state_dir().join("kitchen.db");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("the kitchen")
            .with_inner_size([1400.0, 860.0])
            .with_min_inner_size([800.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "the kitchen",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            let db = kitchen_core::db::Db::open(&db_path)?;
            let engine = engine::Engine::new(cc.egui_ctx.clone(), db)?;
            if let Err(e) = api::serve(engine.clone()) {
                engine.toast_err(format!("API socket: {e:#}"));
            }
            startup(&engine);
            Ok(Box::new(app::KitchenApp::new(engine)))
        }),
    )
}

/// Background setup on launch: relink local skills, refresh every devbox
/// (pull dotfiles, relink skills, update kitchenctl, start its relay).
fn startup(engine: &Arc<engine::Engine>) {
    engine.spawn("local setup", |e| {
        let s = e.settings();
        let report = kitchen_core::bootstrap::bootstrap_host(
            &kitchen_core::host::Host::Local,
            &kitchen_core::bootstrap::Options {
                dotfiles_local: s.dotfiles_local.clone(),
                dotfiles_remote_url: s.dotfiles_remote_url.clone(),
                dotfiles_remote_dir: s.dotfiles_remote_dir.clone(),
            },
        )?;
        for w in report.warnings {
            e.toast_err(format!("local setup: {w}"));
        }
        Ok(())
    });
    for ws in engine.workspaces() {
        if !ws.host().is_local() {
            let id = ws.row.id.clone();
            engine.spawn("devbox setup", move |e| e.bootstrap_workspace(&id));
        }
    }
}
