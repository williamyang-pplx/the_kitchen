//! Modal dialogs: new workspace (⌘N), new tab (⌘T), rename tab (⌘R) / workspace (⌘⇧R), close confirmations.
//! Enter submits, Esc cancels, Tab moves between fields.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use egui::{Key, RichText};

use crate::engine::{Engine, NewTab, Tab, Workspace};
use crate::theme;
use crate::ui::widgets;

/// Width of a dialog's content (its fields span it).
const FIELD_WIDTH: f32 = 480.0;

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

/// ⌘T: a name, for the tab title and the worktree/branch slug, or a link
/// (a PR) for the plugins to open. ⌘I: a name for a tab in the main checkout.
pub struct NewTabDialog {
    text: String,
    main_checkout: bool,
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

    pub fn new_tab(main_checkout: bool) -> Dialog {
        Dialog::NewTab(NewTabDialog { text: String::new(), main_checkout, first_frame: true })
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
        egui::Modal::new(egui::Id::new("kitchen-dialog"))
            .frame(widgets::overlay_frame().inner_margin(egui::Margin::same(theme::SPACE_XL as i8)))
            .backdrop_color(egui::Color32::from_black_alpha(140))
            .show(ctx, |ui| {
                ui.set_width(FIELD_WIDTH);
                outcome = match self {
                    Dialog::NewWorkspace(d) => d.ui(ui, engine),
                    Dialog::NewTab(d) => d.ui(ui, engine),
                    Dialog::Rename(d) => d.ui(ui, engine),
                    Dialog::ConfirmCloseTab { id, title } => confirm(
                        ui,
                        &format!("Close tab “{title}”?"),
                        "Its tmux session (and any agent running in it) ends. The worktree and branch stay on disk.",
                        "Close tab",
                        || {
                            let id = id.clone();
                            engine.spawn("close tab", move |e| e.close_tab(&id));
                        },
                    ),
                    Dialog::ConfirmCloseWorkspace { id, name } => confirm(
                        ui,
                        &format!("Remove workspace “{name}”?"),
                        "All its tabs' tmux sessions end. Repos and worktrees stay on disk.",
                        "Remove",
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

fn confirm(ui: &mut egui::Ui, title: &str, body: &str, yes: &str, on_yes: impl FnOnce()) -> DialogOutcome {
    heading(ui, title, Some(body));
    buttons(ui, yes, true, on_yes)
}

/// A dialog's title, and the sentence under it.
fn heading(ui: &mut egui::Ui, title: &str, body: Option<&str>) {
    ui.label(RichText::new(title).font(theme::ui_font_semibold(theme::TEXT_HEADING)).color(theme::TEXT));
    if let Some(body) = body {
        ui.add_space(theme::SPACE_XS);
        ui.label(RichText::new(body).font(theme::ui_font(theme::TEXT_BODY)).color(theme::SUBTLE));
    }
    ui.add_space(theme::SPACE_L + 2.0);
}

/// A field's label, just above it.
fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::ui_font_semibold(theme::TEXT_SMALL)).color(theme::MUTED));
    ui.add_space(2.0);
}

/// A single-line text field spanning the dialog.
fn field<'t>(text: &'t mut String, hint: &str) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text)
        .font(theme::ui_font(theme::TEXT_BODY))
        .margin(egui::Margin::symmetric(10, 7))
        .hint_text(RichText::new(hint).color(theme::DIM))
        .desired_width(FIELD_WIDTH)
}

/// Right-aligned "Cancel" and "<label> ⏎" (enabled when `can`; Enter presses it too).
fn buttons(ui: &mut egui::Ui, label: &str, can: bool, on_yes: impl FnOnce()) -> DialogOutcome {
    let enter = ui.input(|i| i.key_pressed(Key::Enter));
    let mut out = DialogOutcome::Open;
    ui.add_space(theme::SPACE_XL);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().button_padding = egui::vec2(14.0, 6.0);
        if (ui.add_enabled(can, widgets::primary_button(&format!("{label}  ⏎"))).clicked() || enter) && can {
            on_yes();
            out = DialogOutcome::Done;
        }
        if ui.add(egui::Button::new(RichText::new("Cancel").font(theme::ui_font(theme::TEXT_BODY)))).clicked() {
            out = DialogOutcome::Closed;
        }
    });
    out
}

/// Focus `field` and select all of its `text` (a dialog's first frame).
fn focus_select_all(ui: &egui::Ui, field: &egui::Response, text: &str) {
    field.request_focus();
    let mut state = egui::TextEdit::load_state(ui.ctx(), field.id).unwrap_or_default();
    let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()));
    state.cursor.set_char_range(Some(all));
    state.store(ui.ctx(), field.id);
}

impl NewWorkspace {
    fn ui(&mut self, ui: &mut egui::Ui, engine: &Arc<Engine>) -> DialogOutcome {
        heading(ui, "New workspace", Some("A repository on this mac or on a devbox. Devbox setup pulls your dotfiles and links skills."));
        let hosts = self.hosts.lock().unwrap().clone();
        let host = hosts.get(self.host_idx).cloned().unwrap_or(HostChoice { alias: "local".into(), devbox: false });

        field_label(ui, "Location");
        ui.spacing_mut().interact_size.y = 32.0;
        egui::ComboBox::from_id_salt("host")
            .width(FIELD_WIDTH)
            .selected_text(host_label(&host))
            .show_ui(ui, |ui| {
                for (i, h) in hosts.iter().enumerate() {
                    ui.selectable_value(&mut self.host_idx, i, host_label(h));
                }
            });
        ui.add_space(theme::SPACE_M);

        field_label(ui, "Repository path");
        let repo_resp = ui.add(field(&mut self.repo, if host.alias == "local" { "~/Documents/air" } else { "~/air" }));
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
            ui.add_space(theme::SPACE_XS + 2.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                ui.spacing_mut().button_padding = egui::vec2(8.0, 2.0);
                for s in suggestions {
                    let chip = egui::Button::new(RichText::new(&s).font(theme::ui_font(theme::TEXT_SMALL)).color(theme::MUTED)).corner_radius(theme::RADIUS_L);
                    if ui.add(chip).clicked() {
                        self.repo = s;
                    }
                }
            });
        }
        ui.add_space(theme::SPACE_M);
        field_label(ui, "Name (optional)");
        ui.add(field(&mut self.name, "defaults to the repo name"));

        let (name, host, repo) = (self.name.clone(), host.alias.clone(), self.repo.clone());
        buttons(ui, "Create", !repo.trim().is_empty(), || {
            engine.spawn("new workspace", move |e| e.create_workspace(&name, &host, &repo).map(|_| ()));
        })
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
        heading(ui, if self.workspace { "Rename workspace" } else { "Rename tab" }, None);
        let resp = ui.add(field(&mut self.name, "name"));
        if self.first_frame {
            focus_select_all(ui, &resp, &self.name);
            self.first_frame = false;
        }
        let name = self.name.trim().to_string();
        let (id, workspace) = (self.id.clone(), self.workspace);
        buttons(ui, "Rename", !name.is_empty(), || {
            engine.spawn("rename", move |e| if workspace { e.rename_workspace(&id, &name) } else { e.rename_tab(&id, &name) });
        })
    }
}

impl NewTabDialog {
    fn ui(&mut self, ui: &mut egui::Ui, engine: &Arc<Engine>) -> DialogOutcome {
        let sub = if self.main_checkout {
            "Claude running in the main checkout, with no worktree or branch of its own."
        } else {
            "Its own worktree and branch off the latest origin, with Claude running in it."
        };
        heading(ui, if self.main_checkout { "New tab in the main checkout" } else { "New tab" }, Some(sub));
        let resp = ui.add(field(&mut self.text, "tab name"));
        if self.first_frame {
            focus_select_all(ui, &resp, &self.text);
            self.first_frame = false;
        }
        let text = self.text.trim().to_string();
        let main_checkout = self.main_checkout;
        buttons(ui, "Create", true, || {
            if let Some(ws) = engine.selected_workspace() {
                let opts = NewTab { title: Some(text).filter(|t| !t.is_empty()), main_checkout, ..Default::default() };
                let id = ws.row.id.clone();
                engine.spawn("new tab", move |e| e.create_tab(&id, opts).map(|_| ()));
            }
        })
    }
}

/// Text the plugins open (a URL, or `owner/repo#12`) rather than a command (⌘⇧P).
pub(crate) fn is_link(text: &str) -> bool {
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

    /// Enter in the ⌘T dialog once re-locked the egui context inside
    /// `ui.input` and hung the app; now it just submits.
    #[test]
    fn edit_tab_enter_does_not_deadlock() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let ctx = egui::Context::default();
            crate::theme::install(&ctx); // the UI font families
            let engine = Engine::new(ctx.clone(), kitchen_core::db::Db::open_in_memory().unwrap()).unwrap();
            let mut dialog = Dialog::new_tab(false);
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
