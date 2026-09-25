//! Modal dialogs: new workspace (⌘N), new edit tab (⌘E), close confirmations.
//! Enter submits, Esc cancels, Tab moves between fields.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use egui::{Key, RichText};

use crate::engine::{Engine, NewTab, Tab, Workspace};
use crate::theme;

pub enum DialogOutcome {
    Open,
    /// Cancelled.
    Closed,
    /// Submitted.
    Done,
}

pub enum Dialog {
    NewWorkspace(NewWorkspace),
    NewEditTab(NewEditTab),
    ConfirmCloseTab { id: String, title: String },
    ConfirmCloseWorkspace { id: String, name: String },
}

pub struct NewWorkspace {
    name: String,
    host_idx: usize,
    repo: String,
    hosts: Arc<Mutex<Vec<HostChoice>>>,
    repos: Arc<Mutex<HashMap<String, Vec<String>>>>,
    requested: std::collections::HashSet<String>,
    first_frame: bool,
}

#[derive(Clone, Debug)]
pub struct HostChoice {
    pub alias: String,
    pub devbox: bool,
}

pub struct NewEditTab {
    task: String,
    target: String,
    first_frame: bool,
}

impl Dialog {
    pub fn new_workspace() -> Dialog {
        let hosts = Arc::new(Mutex::new(ssh_hosts()));
        Dialog::NewWorkspace(NewWorkspace {
            name: String::new(),
            host_idx: 0,
            repo: String::new(),
            hosts,
            repos: Arc::new(Mutex::new(HashMap::new())),
            requested: Default::default(),
            first_frame: true,
        })
    }

    pub fn new_edit_tab() -> Dialog {
        Dialog::NewEditTab(NewEditTab { task: String::new(), target: String::new(), first_frame: true })
    }

    pub fn confirm_close(tab: &Tab) -> Dialog {
        Dialog::ConfirmCloseTab { id: tab.row.id.clone(), title: tab.row.title.clone() }
    }

    pub fn confirm_close_workspace(ws: &Workspace) -> Dialog {
        Dialog::ConfirmCloseWorkspace { id: ws.row.id.clone(), name: ws.row.name.clone() }
    }

    pub fn show(&mut self, ctx: &egui::Context, engine: &Arc<Engine>) -> DialogOutcome {
        let esc = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape));
        if esc {
            return DialogOutcome::Closed;
        }
        let mut outcome = DialogOutcome::Open;
        egui::Modal::new(egui::Id::new("kitchen-dialog")).show(ctx, |ui| {
            ui.set_width(520.0);
            outcome = match self {
                Dialog::NewWorkspace(d) => d.ui(ui, engine),
                Dialog::NewEditTab(d) => d.ui(ui, engine),
                Dialog::ConfirmCloseTab { id, title } => confirm(
                    ui,
                    &format!("Close tab “{title}”?"),
                    "Its tmux session (and any agent running in it) ends. The worktree and branch stay on disk.",
                    || {
                        let id = id.clone();
                        engine.spawn("close tab", move |e| e.close_tab(&id));
                    },
                ),
                Dialog::ConfirmCloseWorkspace { id, name } => confirm(
                    ui,
                    &format!("Remove workspace “{name}”?"),
                    "All its tabs' tmux sessions end. Repos and worktrees stay on disk.",
                    || {
                        let id = id.clone();
                        engine.spawn("remove workspace", move |e| e.close_workspace(&id));
                    },
                ),
            };
        });
        outcome
    }
}

fn confirm(ui: &mut egui::Ui, title: &str, body: &str, on_yes: impl FnOnce()) -> DialogOutcome {
    ui.heading(title);
    ui.add_space(6.0);
    ui.label(RichText::new(body).color(theme::DIM));
    ui.add_space(10.0);
    let enter = ui.input(|i| i.key_pressed(Key::Enter));
    let mut out = DialogOutcome::Open;
    ui.horizontal(|ui| {
        if ui.button("Close  ⏎").clicked() || enter {
            on_yes();
            out = DialogOutcome::Done;
        }
        if ui.button("Cancel  esc").clicked() {
            out = DialogOutcome::Closed;
        }
    });
    out
}

impl NewWorkspace {
    fn ui(&mut self, ui: &mut egui::Ui, engine: &Arc<Engine>) -> DialogOutcome {
        ui.heading("New workspace");
        ui.label(RichText::new("A repository on this mac or on a devbox. Devbox setup pulls your dotfiles and links skills.").color(theme::DIM).size(12.0));
        ui.add_space(8.0);
        let hosts = self.hosts.lock().unwrap().clone();
        let host = hosts.get(self.host_idx).cloned().unwrap_or(HostChoice { alias: "local".into(), devbox: false });

        ui.label("Location");
        egui::ComboBox::from_id_salt("host")
            .width(480.0)
            .selected_text(host_label(&host))
            .show_ui(ui, |ui| {
                for (i, h) in hosts.iter().enumerate() {
                    ui.selectable_value(&mut self.host_idx, i, host_label(h));
                }
            });
        ui.add_space(6.0);

        ui.label("Repository path");
        let repo_resp = ui.add(egui::TextEdit::singleline(&mut self.repo).hint_text(if host.alias == "local" { "~/Documents/air" } else { "~/air" }).desired_width(480.0));
        if self.first_frame {
            repo_resp.request_focus();
            self.first_frame = false;
        }
        self.request_repos(&host.alias);
        let suggestions: Vec<String> = self
            .repos
            .lock()
            .unwrap()
            .get(&host.alias)
            .map(|v| v.iter().filter(|r| r.contains(self.repo.trim())).take(6).cloned().collect())
            .unwrap_or_default();
        if !suggestions.is_empty() && !suggestions.iter().any(|s| s == self.repo.trim()) {
            ui.horizontal_wrapped(|ui| {
                for s in suggestions {
                    if ui.small_button(&s).clicked() {
                        self.repo = s;
                    }
                }
            });
        }
        ui.add_space(6.0);
        ui.label("Name (optional)");
        ui.add(egui::TextEdit::singleline(&mut self.name).hint_text("defaults to the repo name").desired_width(480.0));
        ui.add_space(10.0);

        let enter = ui.input(|i| i.key_pressed(Key::Enter));
        let mut out = DialogOutcome::Open;
        ui.horizontal(|ui| {
            let can = !self.repo.trim().is_empty();
            if (ui.add_enabled(can, egui::Button::new("Create  ⏎")).clicked() || enter) && can {
                let (name, host, repo) = (self.name.clone(), host.alias.clone(), self.repo.clone());
                engine.spawn("new workspace", move |e| e.create_workspace(&name, &host, &repo).map(|_| ()));
                out = DialogOutcome::Done;
            }
            if ui.button("Cancel  esc").clicked() {
                out = DialogOutcome::Closed;
            }
        });
        out
    }

    /// List git repos on a host once (background), for path suggestions.
    fn request_repos(&mut self, alias: &str) {
        if !self.requested.insert(alias.to_string()) {
            return;
        }
        let repos = self.repos.clone();
        let alias = alias.to_string();
        std::thread::spawn(move || {
            let host = kitchen_core::host::Host::parse(&alias);
            let script = if host.is_local() {
                "for d in \"$HOME\"/Documents/*/.git \"$HOME\"/*/.git; do [ -d \"$d\" ] && dirname \"$d\"; done 2>/dev/null"
            } else {
                "for d in \"$HOME\"/*/.git; do [ -d \"$d\" ] && dirname \"$d\"; done 2>/dev/null"
            };
            let home = host.home().unwrap_or_default();
            let list: Vec<String> = host
                .sh(script, None)
                .map(|o| o.stdout.lines().map(|l| l.replacen(&home, "~", 1)).collect())
                .unwrap_or_default();
            repos.lock().unwrap().insert(alias, list);
        });
    }
}

fn host_label(h: &HostChoice) -> String {
    if h.alias == "local" {
        "local — this mac".into()
    } else if h.devbox {
        format!("devbox — {}", h.alias)
    } else {
        format!("ssh — {}", h.alias)
    }
}

/// `local`, then devbox aliases, then other concrete ssh hosts from ~/.ssh/config.
pub fn ssh_hosts() -> Vec<HostChoice> {
    let home = std::env::var("HOME").unwrap_or_default();
    let text = std::fs::read_to_string(format!("{home}/.ssh/config")).unwrap_or_default();
    let mut out = vec![HostChoice { alias: "local".into(), devbox: false }];
    out.extend(parse_ssh_config(&text));
    out.sort_by_key(|h| (h.alias != "local", !h.devbox, h.alias.clone()));
    out
}

pub fn parse_ssh_config(text: &str) -> Vec<HostChoice> {
    let mut out: Vec<HostChoice> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let mut parts = line.splitn(2, char::is_whitespace);
        let key = parts.next().unwrap_or("").to_ascii_lowercase();
        let val = parts.next().unwrap_or("").trim();
        if key == "host" {
            current.clear();
            for alias in val.split_whitespace() {
                if alias.contains('*') || alias.contains('?') || alias.starts_with('!') {
                    continue;
                }
                current.push(out.len());
                out.push(HostChoice { alias: alias.to_string(), devbox: false });
            }
        } else if key == "hostname" && val.contains("devbox") {
            for &i in &current {
                out[i].devbox = true;
            }
        }
    }
    out
}

impl NewEditTab {
    fn ui(&mut self, ui: &mut egui::Ui, engine: &Arc<Engine>) -> DialogOutcome {
        ui.heading("New edit tab");
        ui.label(
            RichText::new("Gets its own worktree + branch off the latest origin. With a task, the agent starts in the planning phase (/low-level-plan) and writes the plan for review (⌘L).")
                .color(theme::DIM)
                .size(12.0),
        );
        ui.add_space(8.0);
        ui.label("Task / ticket (planning input)");
        let task = ui.add(
            egui::TextEdit::multiline(&mut self.task)
                .hint_text("e.g. AI-9586: add retries to the judge client — or paste the high-level plan")
                .desired_rows(5)
                .desired_width(500.0),
        );
        if self.first_frame {
            task.request_focus();
            self.first_frame = false;
        }
        ui.add_space(6.0);
        ui.label("…or an existing PR number / branch (optional)");
        ui.add(egui::TextEdit::singleline(&mut self.target).hint_text("1234, #1234, a PR URL, or william/my-branch").desired_width(500.0));
        ui.add_space(10.0);
        let submit = ui.input(|i| i.key_pressed(Key::Enter) && (i.modifiers.command || !task.has_focus()));
        let mut out = DialogOutcome::Open;
        ui.horizontal(|ui| {
            if ui.button("Create  ⌘⏎").clicked() || submit {
                let opts = self.to_new_tab();
                if let Some(ws) = engine.selected_workspace() {
                    let id = ws.row.id.clone();
                    engine.spawn("new edit tab", move |e| e.create_tab(&id, opts).map(|_| ()));
                }
                out = DialogOutcome::Done;
            }
            if ui.button("Cancel  esc").clicked() {
                out = DialogOutcome::Closed;
            }
        });
        out
    }

    fn to_new_tab(&self) -> NewTab {
        let target = self.target.trim();
        let (pr, branch) = parse_target(target);
        let task = Some(self.task.trim().to_string()).filter(|t| !t.is_empty());
        NewTab { kind: Some("edit".into()), task, pr, branch, ..Default::default() }
    }
}

/// `1234`, `#1234`, `.../pull/1234` -> PR; anything else non-empty -> branch.
pub fn parse_target(target: &str) -> (Option<u64>, Option<String>) {
    let t = target.trim();
    if t.is_empty() {
        return (None, None);
    }
    let num = t.trim_start_matches('#');
    if let Ok(n) = num.parse::<u64>() {
        return (Some(n), None);
    }
    if let Some(rest) = t.split("/pull/").nth(1) {
        if let Ok(n) = rest.split(|c: char| !c.is_ascii_digit()).next().unwrap_or("").parse::<u64>() {
            return (Some(n), None);
        }
    }
    (None, Some(t.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(parse_target("#12"), (Some(12), None));
        assert_eq!(parse_target("https://github.com/o/r/pull/99/files"), (Some(99), None));
        assert_eq!(parse_target("william/x"), (None, Some("william/x".into())));
        assert_eq!(parse_target(" "), (None, None));
    }

    #[test]
    fn ssh_config() {
        let cfg = "Host *\n  ForwardAgent yes\nHost air devbox2\n  HostName ssh.devbox.p.perplexity.ai\nHost gpu\n  HostName 1.2.3.4\n";
        let h = parse_ssh_config(cfg);
        assert_eq!(h.len(), 3);
        assert!(h[0].devbox && h[1].devbox && !h[2].devbox);
    }
}
