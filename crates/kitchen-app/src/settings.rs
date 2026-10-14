//! User-tunable settings, persisted as one JSON blob in the settings table.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Prefix for branches created by new tabs.
    pub branch_prefix: String,
    /// Agent started in a new tab (the planning prompt is appended).
    pub edit_agent_command: String,
    /// Local dotfiles checkout; skills are symlinked from here.
    pub dotfiles_local: String,
    pub dotfiles_remote_url: String,
    /// Where devboxes keep the dotfiles checkout.
    pub dotfiles_remote_dir: String,
    /// Model used for outline summaries (`claude -p --model <this>`).
    pub summary_model: String,
    /// Model that answers ⌘⇧A questions in the viewer.
    pub ask_model: String,
    pub font_size: f32,
    /// Folders the viewer opens at when a tab has no changes (first that exists).
    pub viewer_default_dirs: Vec<String>,
    /// Viewer / plan gutters count lines from the cursor (vim `relativenumber`).
    pub relative_line_numbers: bool,
    /// Plugin folders besides `~/.kitchen/plugins` (`~` is expanded).
    pub plugin_dirs: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            branch_prefix: "william/".into(),
            edit_agent_command: "claude".into(),
            dotfiles_local: format!("{home}/Documents/dotfiles"),
            dotfiles_remote_url: "https://github.com/williamyang-pplx/dotfiles".into(),
            dotfiles_remote_dir: "~/.dotfiles".into(),
            summary_model: "haiku".into(),
            ask_model: "sonnet".into(),
            font_size: 13.0,
            viewer_default_dirs: vec!["aster".into()],
            relative_line_numbers: true,
            plugin_dirs: Vec::new(),
        }
    }
}
