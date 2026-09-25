//! User-tunable settings, persisted as one JSON blob in the settings table.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Prefix for branches created by edit tabs.
    pub branch_prefix: String,
    /// Agent started in a new edit tab (the planning prompt is appended).
    pub edit_agent_command: String,
    /// Agent started in a new read-only tab. Plan mode + no edit tools.
    pub readonly_agent_command: String,
    pub autostart_readonly_agent: bool,
    /// Local dotfiles checkout; skills are symlinked from here.
    pub dotfiles_local: String,
    pub dotfiles_remote_url: String,
    /// Where devboxes keep the dotfiles checkout.
    pub dotfiles_remote_dir: String,
    /// Seconds between checks for PRs that request William's review.
    pub review_poll_secs: u64,
    pub review_poll_enabled: bool,
    /// Model used for outline summaries (`claude -p --model <this>`).
    pub summary_model: String,
    pub font_size: f32,
}

impl Default for Settings {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            branch_prefix: "william/".into(),
            edit_agent_command: "claude".into(),
            readonly_agent_command: "claude --permission-mode plan --disallowedTools Edit,Write,NotebookEdit".into(),
            autostart_readonly_agent: true,
            dotfiles_local: format!("{home}/Documents/dotfiles"),
            dotfiles_remote_url: "https://github.com/williamyang-pplx/dotfiles".into(),
            dotfiles_remote_dir: "~/.dotfiles".into(),
            review_poll_secs: 180,
            review_poll_enabled: true,
            summary_model: "haiku".into(),
            font_size: 13.0,
        }
    }
}
