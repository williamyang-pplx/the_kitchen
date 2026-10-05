//! the kitchen — a cmux-style workspace manager for agent-driven PR work.

mod api;
mod app;
mod engine;
mod keymap;
mod links;
mod panels;
mod plan_view;
mod plugins;
mod relay;
mod settings;
mod terminal;
mod theme;
mod ui;
mod viewer;

use std::sync::Arc;

fn main() -> eframe::Result {
    kitchen_core::host::augment_local_path();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).format_timestamp_millis().init();
    let db_path = kitchen_proto::state_dir().join("kitchen.db");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("the kitchen")
            .with_inner_size([1400.0, 860.0])
            .with_min_inner_size([800.0, 480.0])
            // Draw under a transparent title bar so it takes the app's dark
            // colors instead of the light system one; app.rs paints it.
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false)
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon-256.png")).unwrap_or_default()),
        // No default macOS menu: its ⌘H (Hide) and ⌘Q items would swallow
        // those chords before the kitchen sees them. ⌘Q is handled in keymap.rs.
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::macos::EventLoopBuilderExtMacOS;
            builder.with_default_menu(false);
        })),
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
            let mut dirs = vec![kitchen_proto::state_dir().join("plugins")];
            let home = std::env::var("HOME").unwrap_or_default();
            dirs.extend(engine.settings().plugin_dirs.iter().map(|d| std::path::PathBuf::from(d.replacen('~', &home, 1))));
            engine.set_plugins(plugins::PluginHost::start(&engine, dirs));
            viewer::highlight::warm(); // after the plugins added their grammars
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
