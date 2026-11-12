//! Plan reviewer (⌘⇧I): the tab's low-level plan as a vim-navigable
//! document with inline comments. ⌘M comments on the cursor line or visual
//! selection, ⌘Enter submits it; ⌘Enter with no comment open sends every
//! open comment to the tab's agent in one message. ⌘⇧A asks the model a
//! question about the lines, with the agent's planning session as context;
//! the answer shows under them until Esc on their line. The file is re-read
//! every couple of seconds so the agent's revisions show up live.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use egui::{RichText, Ui};
use kitchen_core::db::PlanComment;
use kitchen_core::host::Host;
use kitchen_core::{ask, plan, trace};

use crate::engine::{Engine, Tab, Workspace};
use crate::panels::{PanelCmd, PanelCtx};
use crate::theme;
use crate::ui::widgets;
use crate::viewer::code_view::{Block, CodeView, Editor, ViewState};
use crate::viewer::doc::Doc;
use crate::viewer::highlight;
use crate::viewer::vim::{Vim, VimAction};

const POLL: Duration = Duration::from_secs(2);

enum Msg {
    Loaded { plan: Option<plan::PlanDoc>, text: Option<String> },
    Sent(Result<usize, String>),
    Answer { id: u64, result: Result<String, String> },
}

/// Answers a plan question (the model; a fake in tests): (model, cwd, question).
type Answerer = fn(&str, Option<&str>, &ask::PlanQuestion) -> anyhow::Result<String>;

/// A ⌘⇧A question and, once the model replies, its answer.
struct PlanAsk {
    id: u64,
    /// Doc index of the last asked line: the answer goes under it.
    line: usize,
    question: String,
    asked: Instant,
    /// `None` while the model is answering.
    answer: Option<Result<String, String>>,
}

pub struct PlanView {
    engine: Arc<Engine>,
    tab_id: String,
    host: Host,
    worktree: String,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// The plan shown (the tab's newest plan file) and when it last changed.
    plan: Option<plan::PlanDoc>,
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
    asks: Vec<PlanAsk>,
    ask_seq: u64,
    answerer: Answerer,
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
            plan: None,
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
            asks: Vec::new(),
            ask_seq: 0,
            answerer: ask::answer_plan,
        }
    }

    fn poll_file(&mut self, force: bool) {
        if self.in_flight || (!force && self.last_poll.map(|t| t.elapsed() < POLL).unwrap_or(false)) {
            return;
        }
        self.in_flight = true;
        self.last_poll = Some(Instant::now());
        let (host, wt, tab, tx, ctx) = (self.host.clone(), self.worktree.clone(), self.tab_id.clone(), self.tx.clone(), self.engine.ctx.clone());
        std::thread::spawn(move || {
            let plan = plan::current_plan(&host, &wt, &tab);
            let text = plan.as_ref().and_then(|doc| host.read_file(&doc.path).ok());
            let _ = tx.send(Msg::Loaded { plan, text });
            ctx.request_repaint();
        });
    }

    /// The comments that show on the plan: made on this file, minus resolved
    /// ones the plan has since been rewritten past.
    fn reload_comments(&mut self) {
        let all = self.engine.plan_comments(&self.tab_id).unwrap_or_default();
        self.comments = match &self.plan {
            Some(doc) => all.into_iter().filter(|c| plan::shows_on(c, doc)).collect(),
            None => Vec::new(),
        };
    }

    fn receive(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Loaded { plan, text } => {
                    self.in_flight = false;
                    self.loaded_once = true;
                    let changed = plan != self.plan;
                    self.plan = plan;
                    if text != self.text {
                        self.text = text;
                        self.rebuild();
                    } else if changed {
                        self.reload_comments();
                    }
                }
                Msg::Sent(r) => match r {
                    Ok(n) => {
                        self.status = Some((format!("sent {n} comment(s) to the agent"), false));
                        self.reload_comments();
                    }
                    Err(e) => self.status = Some((e, true)),
                },
                Msg::Answer { id, result } => {
                    // A question closed while it was being answered is gone.
                    if let Some(a) = self.asks.iter_mut().find(|a| a.id == id) {
                        a.answer = Some(result);
                    }
                }
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
        if self.asks.iter().any(|a| a.answer.is_none()) {
            // The thinking card counts seconds.
            self.engine.ctx.request_repaint_after(Duration::from_millis(500));
        }
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
                Block { after: Some(at), title: format!("comment #{} · {lines} · {label}", c.id), body: c.body.clone(), color, dim, question: false }
            })
            .chain(self.asks.iter().map(|a| Block::answer(a.line.min(last), &a.question, a.answer.as_ref(), a.asked)))
            .collect()
    }

    /// ⌘⇧A: a question field under the cursor line or visual selection.
    fn start_ask(&mut self) {
        if self.doc.lines.is_empty() {
            self.status = Some(("no plan to ask about yet".into(), false));
            return;
        }
        let (start, end) = self.vim.visual_lines().unwrap_or((self.vim.line, self.vim.line));
        let what = if end > start { format!("lines {}–{}", start + 1, end + 1) } else { format!("line {}", start + 1) };
        let model = self.engine.settings().ask_model;
        let label = format!("Ask {model} about {what} (with the planning session as context) — Enter asks · Esc cancels");
        self.editor = Some(Editor { start, end, text: String::new(), label, needs_focus: true, ask: true });
    }

    /// Send the question field to the model, with the plan and the agent's
    /// planning session; its block shows "thinking… Ns" until it answers.
    fn submit_ask(&mut self) {
        let Some(ed) = self.editor.take() else { return };
        let text = ed.text.trim().to_string();
        if text.is_empty() {
            self.editor = Some(ed);
            return;
        }
        self.ask_seq += 1;
        let id = self.ask_seq;
        self.asks.push(PlanAsk { id, line: ed.end, question: text.clone(), asked: Instant::now(), answer: None });
        self.vim.anchor = None;
        let quote = self.doc.lines.get(ed.start..=ed.end).unwrap_or_default().iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        let plan_text = self.text.clone().unwrap_or_default();
        let path = self.plan.as_ref().map(|d| d.path.clone()).unwrap_or_else(|| plan::plan_file_for(&self.worktree, &self.tab_id));
        let model = self.engine.settings().ask_model;
        // On this mac the model may read the rest of the repo.
        let cwd = matches!(self.host, Host::Local).then(|| self.worktree.clone());
        let (host, wt, tx, ctx, answer) = (self.host.clone(), self.worktree.clone(), self.tx.clone(), self.engine.ctx.clone(), self.answerer);
        let (first, last) = (ed.start + 1, ed.end + 1);
        std::thread::spawn(move || {
            let session = trace::latest_transcript(&host, &wt).map(|t| trace::condense(&t, ask::MAX_TRACE)).filter(|t| !t.is_empty());
            let q = ask::PlanQuestion { path: &path, first, last, quote: &quote, plan: &plan_text, trace: session.as_deref(), text: &text };
            let result = answer(&model, cwd.as_deref(), &q).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Answer { id, result });
            ctx.request_repaint();
        });
    }

    /// Save the open comment field as a plan comment.
    fn submit_comment(&mut self) {
        let Some(ed) = self.editor.take() else { return };
        let body = ed.text.trim().to_string();
        if body.is_empty() {
            self.editor = Some(ed);
            return;
        }
        let Some(path) = self.plan.as_ref().map(|d| d.path.clone()) else {
            self.status = Some(("no plan to comment on yet".into(), false));
            return;
        };
        let quote = self.doc.lines.get(ed.start).map(|l| l.text.clone()).unwrap_or_default();
        let end = (ed.end > ed.start).then_some(ed.end as i64 + 1);
        match self.engine.add_plan_comment(&self.tab_id, &path, ed.start as i64 + 1, end, &quote, &body) {
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
                    ask: false,
                });
            }
            PanelCmd::Ask => self.start_ask(),
            // Submit after this frame's typing reaches the editor.
            PanelCmd::Submit if self.editor.is_some() => self.submit_pending = true,
            PanelCmd::Submit => {
                let open = self.comments.iter().filter(|c| c.state == "open").count();
                let Some(path) = self.plan.as_ref().map(|d| d.path.clone()).filter(|_| open > 0) else {
                    self.status = Some(("no open comments to send (⌘M to comment)".into(), false));
                    return;
                };
                self.status = Some((format!("sending {open} comment(s)…"), false));
                let (engine, tab, tx) = (self.engine.clone(), self.tab_id.clone(), self.tx.clone());
                std::thread::spawn(move || {
                    let r = engine.send_plan_feedback(&tab, &path).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::Sent(r));
                    engine.ctx.request_repaint();
                });
            }
            PanelCmd::Reload => {
                self.reload_comments();
                self.poll_file(true);
            }
            PanelCmd::ToggleDiff | PanelCmd::ToggleOutline | PanelCmd::FilePicker | PanelCmd::FocusList | PanelCmd::FocusCode | PanelCmd::ToggleFiles | PanelCmd::NextFile | PanelCmd::PrevFile | PanelCmd::Diagnostics | PanelCmd::Render(_) | PanelCmd::Open { .. } | PanelCmd::ViewMenu => {
                self.status = Some(("plan reviewer: ⌘M comment · ⌘⇧A ask · ⌘Enter submit / send to agent".into(), false));
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
        let header = ui.available_rect_before_wrap();
        let header = egui::Rect::from_min_size(header.min, egui::vec2(header.width(), widgets::HEADER_HEIGHT));
        let mut header_ui = ui.new_child(egui::UiBuilder::new().max_rect(header).layout(egui::Layout::left_to_right(egui::Align::Center)));
        header_ui.add_space(theme::SPACE_M);
        let path = self.plan.as_ref().map(|d| d.path.as_str()).map(|p| p.strip_prefix(&format!("{}/", self.worktree)).unwrap_or(p).to_string());
        header_ui.label(widgets::path_job(&path.unwrap_or_else(|| "—".into()), 12.5));
        header_ui.add_space(theme::SPACE_XS);
        widgets::pill(&mut header_ui, "PLAN", theme::YELLOW);
        header_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(theme::SPACE_M);
            let count = |ui: &mut Ui, n: usize, what: &str, color: egui::Color32| {
                ui.label(RichText::new(format!("{n} {what}")).font(theme::ui_font(theme::TEXT_SMALL)).color(if n > 0 { color } else { theme::DIM }));
            };
            count(ui, resolved, "resolved", theme::SUBTLE);
            count(ui, sent, "sent", theme::ACCENT);
            count(ui, open, "open", theme::YELLOW);
        });
        ui.painter().hline(header.x_range(), header.bottom() - 0.5, egui::Stroke::new(1.0, theme::BORDER));
        ui.advance_cursor_after_rect(header);

        let full = ui.available_rect_before_wrap();
        let footer_h = 20.0;
        let body = egui::Rect::from_min_max(full.min, egui::pos2(full.max.x, full.max.y - footer_h));
        let footer = egui::Rect::from_min_max(egui::pos2(full.min.x, full.max.y - footer_h), full.max);
        let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(body));

        let empty = if !self.loaded_once {
            Some("loading plan…".into())
        } else if self.text.is_none() {
            Some(format!(
                "the agent hasn't written a plan yet\n\nit will appear here when the agent writes {}\n(or a plan-mode file under {}/)",
                plan::plan_file_for(".", &self.tab_id),
                plan::plans_dir(&self.tab_id)
            ))
        } else {
            None
        };
        if let Some(text) = empty {
            body_ui.centered_and_justified(|ui| ui.label(RichText::new(text).font(theme::ui_font(theme::TEXT_BODY)).color(theme::SUBTLE)));
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
            if std::mem::take(&mut self.submit_pending) || out.submit_editor {
                if self.editor.as_ref().is_some_and(|e| e.ask) {
                    self.submit_ask();
                } else {
                    self.submit_comment();
                }
            }
            if out.actions.iter().any(|a| matches!(a, VimAction::Escape)) {
                // Esc: close the answers under the cursor line.
                let line = self.vim.line;
                let last = self.doc.lines.len().saturating_sub(1);
                self.asks.retain(|a| a.line.min(last) != line);
                self.status = None;
            }
        }

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
                ui.label(RichText::new("-- VISUAL --  ⌘M comment on selection · ⌘⇧A ask about it").size(11.5).color(theme::PURPLE));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(format!("L{}", self.vim.line + 1)).monospace().size(11.0).color(theme::SUBTLE));
            });
        });
        ui.advance_cursor_after_rect(full);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{TestRepo, engine};

    fn frame(ctx: &egui::Context, engine: &Arc<Engine>, p: &mut PlanView, events: Vec<egui::Event>) {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 700.0))), events, ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            let mut cx = PanelCtx { engine, focused: true, clicked: false, view: None };
            p.ui(ui, &mut cx);
        });
        out.textures_delta.clear();
    }

    fn key(key: egui::Key) -> Vec<egui::Event> {
        vec![egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }]
    }

    fn settle(ctx: &egui::Context, engine: &Arc<Engine>, p: &mut PlanView, done: &dyn Fn(&PlanView) -> bool) {
        for _ in 0..300 {
            frame(ctx, engine, p, Vec::new());
            if done(p) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("plan view never settled");
    }

    /// Tabs sharing a worktree keep their own plans; a tab shows the comments
    /// made on its current plan file, until that file is rewritten past them.
    #[test]
    fn plans_and_comments_are_per_tab_and_per_plan() {
        let repo = TestRepo::with_files(&[(".kitchen/tabs/t/plan.md", "# Plan\n## F1\n")]);
        let (ctx, engine) = engine("local", &repo.path(), &["t", "u"]);
        let w = engine.workspaces()[0].clone();
        let mut other = PlanView::new(&engine, &w, &w.tabs[1]);
        settle(&ctx, &engine, &mut other, &|p| p.loaded_once);
        assert_eq!((other.plan.as_ref(), other.text.as_ref()), (None, None), "not tab t's plan");

        let path = format!("{}/.kitchen/tabs/t/plan.md", repo.path());
        let db = engine.db();
        let resolved = db.add_plan_comment("t", &path, 2, None, "## F1", "done").unwrap();
        db.set_plan_comment_state(resolved, "resolved").unwrap();
        let open = db.add_plan_comment("t", &path, 1, None, "# Plan", "still open").unwrap();
        db.add_plan_comment("t", "/elsewhere/old-plan.md", 1, None, "# Plan", "another plan's").unwrap();
        drop(db);
        let ids = |p: &PlanView| p.comments.iter().map(|c| c.id).collect::<Vec<_>>();
        let mut p = PlanView::new(&engine, &w, &w.tabs[0]);
        settle(&ctx, &engine, &mut p, &|p| p.doc.lines.len() == 2);
        assert_eq!(ids(&p), [open, resolved]);

        // The agent writes the next plan into the same file.
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        p.command(PanelCmd::Reload, &engine);
        settle(&ctx, &engine, &mut p, &|p| p.comments.len() == 1);
        assert_eq!(ids(&p), [open], "the resolved one is the old plan's");
        assert_eq!(engine.current_plan_comments("t").unwrap().iter().map(|c| c.id).collect::<Vec<_>>(), [open]);
    }

    #[test]
    fn questions_are_answered_under_their_lines() {
        let repo = TestRepo::with_files(&[(".kitchen/tabs/t/plan.md", "# Plan\n## F1 cache\nuse an LRU\n")]);
        let (ctx, engine) = engine("local", &repo.path(), &["t"]);
        let w = engine.workspaces()[0].clone();
        let mut p = PlanView::new(&engine, &w, &w.tabs[0]);
        p.answerer = |model, cwd, q| Ok(format!("{model}|{}|{}-{}|{}|{}|{}", cwd.is_some(), q.first, q.last, q.quote, q.plan.lines().count(), q.text));
        settle(&ctx, &engine, &mut p, &|p| p.doc.lines.len() == 3);
        frame(&ctx, &engine, &mut p, vec![egui::Event::Text("jV".into())]);
        frame(&ctx, &engine, &mut p, vec![egui::Event::Text("j".into())]);
        p.command(PanelCmd::Ask, &engine);
        assert!(p.editor.as_ref().is_some_and(|e| e.ask && e.label.starts_with("Ask sonnet about lines 2–3")), "{:?}", p.editor.as_ref().map(|e| &e.label));
        frame(&ctx, &engine, &mut p, Vec::new());
        frame(&ctx, &engine, &mut p, vec![egui::Event::Text("why LRU?".into())]);
        frame(&ctx, &engine, &mut p, key(egui::Key::Enter));
        assert!(p.editor.is_none(), "submitted");
        let ask = |p: &PlanView| p.blocks().into_iter().find(|b| b.title.starts_with("why LRU?"));
        assert_eq!(ask(&p).map(|b| (b.after, b.body)), Some((Some(2), "thinking… 0s".into())));
        settle(&ctx, &engine, &mut p, &|p| ask(p).is_some_and(|b| !b.dim));
        assert_eq!(ask(&p).unwrap().body, "sonnet|true|2-3|## F1 cache\nuse an LRU|3|why LRU?");
        // Esc elsewhere keeps it; Esc on its line closes it.
        frame(&ctx, &engine, &mut p, vec![egui::Event::Text("k".into())]);
        frame(&ctx, &engine, &mut p, key(egui::Key::Escape));
        assert!(ask(&p).is_some());
        frame(&ctx, &engine, &mut p, vec![egui::Event::Text("j".into())]);
        frame(&ctx, &engine, &mut p, key(egui::Key::Escape));
        assert!(ask(&p).is_none());
    }
}
