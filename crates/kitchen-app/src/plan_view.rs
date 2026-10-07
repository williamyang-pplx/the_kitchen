//! Plan reviewer (⌘⇧I): the tab's low-level plan as a vim-navigable
//! document with inline comments. ⌘M comments on the cursor line or visual
//! selection, ⌘Enter submits it; ⌘Enter with no comment open sends every
//! open comment to the tab's agent in one message. The file is re-read
//! every couple of seconds so the agent's revisions show up live.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use egui::{RichText, Ui};
use kitchen_core::db::PlanComment;
use kitchen_core::host::Host;
use kitchen_core::plan;

use crate::engine::{Engine, Tab, Workspace};
use crate::panels::{PanelCmd, PanelCtx};
use crate::theme;
use crate::viewer::code_view::{Block, CodeView, Editor, ViewState};
use crate::viewer::doc::Doc;
use crate::viewer::highlight;
use crate::viewer::vim::Vim;

const POLL: Duration = Duration::from_secs(2);

enum Msg {
    Loaded { path: Option<String>, text: Option<String> },
    Sent(Result<usize, String>),
}

pub struct PlanView {
    engine: Arc<Engine>,
    tab_id: String,
    host: Host,
    worktree: String,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    path: Option<String>,
    text: Option<String>,
    loaded_once: bool,
    in_flight: bool,
    last_poll: Option<Instant>,
    doc: Doc,
    comments: Vec<PlanComment>,
    vim: Vim,
    view: ViewState,
    editor: Option<Editor>,
    status: Option<(String, bool)>,
    /// ⌘Enter arrived; submit once the editor has seen this frame's input.
    submit_pending: bool,
}

impl PlanView {
    pub fn new(engine: &Arc<Engine>, ws: &Workspace, tab: &Tab) -> Self {
        let (tx, rx) = channel();
        Self {
            engine: engine.clone(),
            tab_id: tab.row.id.clone(),
            host: ws.host(),
            worktree: tab.row.worktree.clone(),
            tx,
            rx,
            path: None,
            text: None,
            loaded_once: false,
            in_flight: false,
            last_poll: None,
            doc: Doc::default(),
            comments: Vec::new(),
            vim: Vim::default(),
            view: ViewState::default(),
            editor: None,
            status: None,
            submit_pending: false,
        }
    }

    fn poll_file(&mut self, force: bool) {
        if self.in_flight || (!force && self.last_poll.map(|t| t.elapsed() < POLL).unwrap_or(false)) {
            return;
        }
        self.in_flight = true;
        self.last_poll = Some(Instant::now());
        let (host, wt, tx, ctx) = (self.host.clone(), self.worktree.clone(), self.tx.clone(), self.engine.ctx.clone());
        std::thread::spawn(move || {
            let path = plan::current_plan_path(&host, &wt);
            let text = path.as_deref().and_then(|p| host.read_file(p).ok());
            let _ = tx.send(Msg::Loaded { path, text });
            ctx.request_repaint();
        });
    }

    fn reload_comments(&mut self) {
        self.comments = self.engine.plan_comments(&self.tab_id).unwrap_or_default();
    }

    fn receive(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Loaded { path, text } => {
                    self.in_flight = false;
                    self.loaded_once = true;
                    self.path = path;
                    if text != self.text {
                        self.text = text;
                        self.rebuild();
                    }
                }
                Msg::Sent(r) => match r {
                    Ok(n) => {
                        self.status = Some((format!("sent {n} comment(s) to the agent"), false));
                        self.reload_comments();
                    }
                    Err(e) => self.status = Some((e, true)),
                },
            }
        }
    }

    fn rebuild(&mut self) {
        let text = self.text.clone().unwrap_or_default();
        let spans = highlight::highlight("plan.md", &text);
        self.doc = Doc::plain(&text, spans.as_deref());
        // Comments follow their quoted line when the agent rewrites the plan.
        self.reload_comments();
        let moved = plan::reanchor(&self.comments, &text);
        if !moved.is_empty() {
            let db = self.engine.db();
            for (id, line) in moved {
                let _ = db.update_plan_comment_anchor(id, line);
            }
            drop(db);
            self.reload_comments();
        }
        self.vim.line = self.vim.line.min(self.doc.lines.len().saturating_sub(1));
    }

    fn blocks(&self) -> Vec<Block> {
        let last = self.doc.lines.len().saturating_sub(1);
        self.comments
            .iter()
            .map(|c| {
                let (label, color, dim) = match c.state.as_str() {
                    "sent" => ("sent to agent", theme::ACCENT, false),
                    "resolved" => ("✓ resolved", theme::DIM, true),
                    _ => ("open", theme::YELLOW, false),
                };
                let lines = match c.end_line {
                    Some(e) if e > c.line => format!("L{}–{e}", c.line),
                    _ => format!("L{}", c.line),
                };
                let at = (c.end_line.unwrap_or(c.line).max(1) as usize - 1).min(last);
                Block { after: Some(at), title: format!("comment #{} · {lines} · {label}", c.id), body: c.body.clone(), color, dim }
            })
            .collect()
    }

    /// Save the open comment field as a plan comment.
    fn submit_comment(&mut self) {
        let Some(ed) = self.editor.take() else { return };
        let body = ed.text.trim().to_string();
        if body.is_empty() {
            self.editor = Some(ed);
            return;
        }
        let quote = self.doc.lines.get(ed.start).map(|l| l.text.clone()).unwrap_or_default();
        let end = (ed.end > ed.start).then_some(ed.end as i64 + 1);
        match self.engine.add_plan_comment(&self.tab_id, ed.start as i64 + 1, end, &quote, &body) {
            Ok(id) => self.status = Some((format!("comment #{id} saved · ⌘Enter (no field open) sends open comments to the agent"), false)),
            Err(e) => self.status = Some((format!("{e:#}"), true)),
        }
        self.vim.anchor = None;
        self.reload_comments();
    }

    pub fn command(&mut self, cmd: PanelCmd, _engine: &Arc<Engine>) {
        match cmd {
            PanelCmd::Comment => {
                if self.doc.lines.is_empty() {
                    self.status = Some(("no plan to comment on yet".into(), false));
                    return;
                }
                let (start, end) = self.vim.visual_lines().unwrap_or((self.vim.line, self.vim.line));
                let lines = if end > start { format!("lines {}–{}", start + 1, end + 1) } else { format!("line {}", start + 1) };
                self.editor = Some(Editor {
                    start,
                    end,
                    text: String::new(),
                    label: format!("Plan comment on {lines} — stays local until you send (⌘Enter with no field open)"),
                    needs_focus: true,
                });
            }
            // Submit after this frame's typing reaches the editor.
            PanelCmd::Submit if self.editor.is_some() => self.submit_pending = true,
            PanelCmd::Submit => {
                let open = self.comments.iter().filter(|c| c.state == "open").count();
                if open == 0 {
                    self.status = Some(("no open comments to send (⌘M to comment)".into(), false));
                    return;
                }
                self.status = Some((format!("sending {open} comment(s)…"), false));
                let (engine, tab, tx) = (self.engine.clone(), self.tab_id.clone(), self.tx.clone());
                std::thread::spawn(move || {
                    let r = engine.send_plan_feedback(&tab).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::Sent(r));
                    engine.ctx.request_repaint();
                });
            }
            PanelCmd::Reload => {
                self.reload_comments();
                self.poll_file(true);
            }
            PanelCmd::ToggleDiff | PanelCmd::ToggleOutline | PanelCmd::FilePicker | PanelCmd::FocusList | PanelCmd::FocusCode | PanelCmd::ToggleFiles | PanelCmd::NextFile | PanelCmd::PrevFile | PanelCmd::Diagnostics | PanelCmd::Render(_) | PanelCmd::Open { .. } => {
                self.status = Some(("plan reviewer: ⌘M comment · ⌘Enter submit / send to agent".into(), false));
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut PanelCtx) {
        self.receive();
        self.poll_file(false);
        ui.ctx().request_repaint_after(POLL);
        if ui.input(|i| i.pointer.any_pressed()) && ui.rect_contains_pointer(ui.max_rect()) {
            cx.clicked = true;
        }

        let open = self.comments.iter().filter(|c| c.state == "open").count();
        let sent = self.comments.iter().filter(|c| c.state == "sent").count();
        let resolved = self.comments.iter().filter(|c| c.state == "resolved").count();
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(RichText::new("PLAN").font(theme::ui_font_semibold(10.5)).color(theme::BG).background_color(theme::YELLOW));
            let path = self.path.as_deref().map(|p| p.strip_prefix(&format!("{}/", self.worktree)).unwrap_or(p).to_string());
            ui.label(RichText::new(path.unwrap_or_else(|| "—".into())).monospace().size(12.0));
            ui.label(RichText::new(format!("{open} open · {sent} sent · {resolved} resolved")).size(11.0).color(theme::DIM));
        });
        ui.separator();

        let full = ui.available_rect_before_wrap();
        let footer_h = 20.0;
        let body = egui::Rect::from_min_max(full.min, egui::pos2(full.max.x, full.max.y - footer_h));
        let footer = egui::Rect::from_min_max(egui::pos2(full.min.x, full.max.y - footer_h), full.max);
        let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(body));

        let empty = if !self.loaded_once {
            Some("loading plan…".into())
        } else if self.text.is_none() {
            Some(format!(
                "the agent hasn't written a plan yet\n\nit will appear here when the agent writes {}\n(or a plan-mode file under .kitchen/plans/)",
                plan::plan_file_for(".")
            ))
        } else {
            None
        };
        if let Some(text) = empty {
            body_ui.centered_and_justified(|ui| ui.label(RichText::new(text).color(theme::DIM)));
        } else {
            let blocks = self.blocks();
            let out = CodeView {
                doc: &self.doc,
                vim: &mut self.vim,
                state: &mut self.view,
                blocks: &blocks,
                editor: self.editor.as_mut(),
                focused: cx.focused,
                font_size: cx.engine.settings().font_size,
                two_gutters: false,
                relative: self.engine.settings().relative_line_numbers,
                overlay: None,
                id: egui::Id::new(("plan", &self.tab_id)),
            }
            .show(&mut body_ui);
            if out.clicked {
                cx.clicked = true;
            }
            if out.cancel_editor {
                self.editor = None;
            }
            if std::mem::take(&mut self.submit_pending) {
                self.submit_comment();
            }
        }

        let busy = self.vim.search_input.is_some() || self.vim.message.is_some() || self.status.is_some() || self.vim.anchor.is_some();
        let mut f = ui.new_child(egui::UiBuilder::new().max_rect(footer));
        f.horizontal_centered(|ui| {
            ui.add_space(8.0);
            if let Some((q, fwd)) = &self.vim.search_input {
                ui.label(RichText::new(format!("{}{q}▏", if *fwd { '/' } else { '?' })).monospace().size(12.0));
            } else if let Some(m) = &self.vim.message {
                ui.label(RichText::new(m).size(11.5).color(theme::YELLOW));
            } else if let Some((s, err)) = &self.status {
                ui.label(RichText::new(s).size(11.5).color(if *err { theme::RED } else { theme::YELLOW }));
            } else if self.vim.anchor.is_some() {
                ui.label(RichText::new("-- VISUAL --  ⌘M comment on selection").size(11.5).color(theme::PURPLE));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(format!("L{}", self.vim.line + 1)).monospace().size(11.0).color(theme::DIM));
                if !busy {
                    ui.label(RichText::new("⌘M comment · ⌘Enter submit / send all to agent").size(10.5).color(theme::DIM));
                }
            });
        });
        ui.advance_cursor_after_rect(full);
    }
}
