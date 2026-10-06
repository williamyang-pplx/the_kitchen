//! Modal dialogs: new workspace (⌘N), new tab (⌘E), rename tab (⌘R) / workspace (⌘⇧R), close confirmations.
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
    NewTab(NewTabDialog),
    ConfirmCloseTab { id: String, title: String },
    ConfirmCloseWorkspace { id: String, name: String },
    Rename(Rename),
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

pub struct Rename {
    /// Renaming a workspace rather than a tab.
    workspace: bool,
    id: String,
    name: String,
    first_frame: bool,
}

/// ⌘E: a name, for the tab title and the worktree/branch slug, or a link
/// (a PR) for the plugins to open.
pub struct NewTabDialog {
    text: String,
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

    pub fn new_tab() -> Dialog {
        // A link just copied from the browser is the likely input (selected,
        // so typing a name replaces it).
        let clip = std::process::Command::new("pbpaste").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        let text = clip.filter(|c| c.starts_with("https://") && !c.contains(char::is_whitespace)).unwrap_or_default();
        Dialog::NewTab(NewTabDialog { text, first_frame: true })
    }

    pub fn confirm_close(tab: &Tab) -> Dialog {
        Dialog::ConfirmCloseTab { id: tab.row.id.clone(), title: tab.row.title.clone() }
    }

    pub fn confirm_close_workspace(ws: &Workspace) -> Dialog {
        Dialog::ConfirmCloseWorkspace { id: ws.row.id.clone(), name: ws.row.name.clone() }
    }

    pub fn rename_tab(tab: &Tab) -> Dialog {
        Dialog::Rename(Rename { workspace: false, id: tab.row.id.clone(), name: tab.row.title.clone(), first_frame: true })
    }

    pub fn rename_workspace(ws: &Workspace) -> Dialog {
        Dialog::Rename(Rename { workspace: true, id: ws.row.id.clone(), name: ws.row.name.clone(), first_frame: true })
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
                Dialog::NewTab(d) => d.ui(ui, engine),
                Dialog::Rename(d) => d.ui(ui, engine),
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

impl Rename {
    fn ui(&mut self, ui: &mut egui::Ui, engine: &Arc<Engine>) -> DialogOutcome {
        ui.heading(if self.workspace { "Rename workspace" } else { "Rename tab" });
        ui.add_space(8.0);
        let field = ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(500.0));
        if self.first_frame {
            field.request_focus();
            let mut state = egui::TextEdit::load_state(ui.ctx(), field.id).unwrap_or_default();
            let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(self.name.chars().count()));
            state.cursor.set_char_range(Some(all));
            state.store(ui.ctx(), field.id);
            self.first_frame = false;
        }
        ui.add_space(10.0);
        let name = self.name.trim().to_string();
        let submit = ui.input(|i| i.key_pressed(Key::Enter)) && !name.is_empty();
        let mut out = DialogOutcome::Open;
        ui.horizontal(|ui| {
            if ui.add_enabled(!name.is_empty(), egui::Button::new("Rename  ⏎")).clicked() || submit {
                let (id, workspace) = (self.id.clone(), self.workspace);
                engine.spawn("rename", move |e| if workspace { e.rename_workspace(&id, &name) } else { e.rename_tab(&id, &name) });
                out = DialogOutcome::Done;
            }
            if ui.button("Cancel  esc").clicked() {
                out = DialogOutcome::Closed;
            }
        });
        out
    }
}

impl NewTabDialog {
    fn ui(&mut self, ui: &mut egui::Ui, engine: &Arc<Engine>) -> DialogOutcome {
        ui.heading("New tab");
        ui.label(
            RichText::new("A name: its own worktree + branch off the latest origin. A PR link: the PR's branch. Claude runs in it.")
                .color(theme::DIM)
                .size(12.0),
        );
        ui.add_space(8.0);
        let field = ui.add(
            egui::TextEdit::singleline(&mut self.text).hint_text("tab name, or https://github.com/ppl-ai/air/pull/14211").desired_width(500.0),
        );
        if self.first_frame {
            field.request_focus();
            let mut state = egui::TextEdit::load_state(ui.ctx(), field.id).unwrap_or_default();
            let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(self.text.chars().count()));
            state.cursor.set_char_range(Some(all));
            state.store(ui.ctx(), field.id);
            self.first_frame = false;
        }
        ui.add_space(10.0);
        let submit = ui.input(|i| i.key_pressed(Key::Enter));
        let mut out = DialogOutcome::Open;
        ui.horizontal(|ui| {
            if ui.button("Create  ⏎").clicked() || submit {
                let text = self.text.trim().to_string();
                if is_link(&text) {
                    engine.spawn("open", move |e| e.open_text(&text));
                } else if let Some(ws) = engine.selected_workspace() {
                    let opts = NewTab { title: Some(text).filter(|t| !t.is_empty()), ..Default::default() };
                    let id = ws.row.id.clone();
                    engine.spawn("new tab", move |e| e.create_tab(&id, opts).map(|_| ()));
                }
                out = DialogOutcome::Done;
            }
            if ui.button("Cancel  esc").clicked() {
                out = DialogOutcome::Closed;
            }
        });
        out
    }
}

/// Text the plugins open (a URL, or `owner/repo#12`) rather than a tab name.
fn is_link(text: &str) -> bool {
    let short_ref = text.split_once('#').is_some_and(|(repo, n)| repo.matches('/').count() == 1 && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    !text.contains(char::is_whitespace) && (text.contains("://") || text.starts_with("github.com/") || short_ref)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_versus_names() {
        for link in ["https://github.com/ppl-ai/air/pull/14211", "github.com/o/r/pull/7", "ppl-ai/air#9"] {
            assert!(is_link(link), "{link}");
        }
        for name in ["", "fix retries", "AI-123: judge", "issue #12", "a/b#x"] {
            assert!(!is_link(name), "{name}");
        }
    }

    #[test]
    fn ssh_config() {
        let cfg = "Host *\n  ForwardAgent yes\nHost air devbox2\n  HostName ssh.devbox.p.perplexity.ai\nHost gpu\n  HostName 1.2.3.4\n";
        let h = parse_ssh_config(cfg);
        assert_eq!(h.len(), 3);
        assert!(h[0].devbox && h[1].devbox && !h[2].devbox);
    }

    /// Enter in the ⌘E dialog once re-locked the egui context inside
    /// `ui.input` and hung the app; now it just submits.
    #[test]
    fn edit_tab_enter_does_not_deadlock() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let ctx = egui::Context::default();
            let engine = Engine::new(ctx.clone(), kitchen_core::db::Db::open_in_memory().unwrap()).unwrap();
            let mut dialog = Dialog::new_tab();
            let mut step = |events: Vec<egui::Event>| {
                let mut outcome = DialogOutcome::Closed;
                let mut out = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| outcome = dialog.show(ui.ctx(), &engine));
                out.textures_delta.clear();
                outcome
            };
            step(vec![]);
            step(vec![]);
            let enter = egui::Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
            tx.send(matches!(step(vec![enter]), DialogOutcome::Done)).unwrap();
        });
        let done = rx.recv_timeout(std::time::Duration::from_secs(20)).expect("Enter in the new tab dialog deadlocked");
        assert!(done, "Enter in the name field should submit");
    }
}
