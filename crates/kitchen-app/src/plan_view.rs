//! Plan reviewer (⌘⇧I): the tab's low-level plan as a vim-navigable
//! document with inline comments. ⌘M comments on the cursor line or visual
//! selection, ⌘Enter submits it; ⌘Enter with no comment open sends every
//! open comment to the tab's agent in one message. ⌘⇧A asks the planning
//! agent (a fork of its session) a question about the lines; the answers
//! show under them, with a follow-up field (see `ask_thread`), until Esc on
//! their lines. The file is re-read
//! every couple of seconds so the agent's revisions show up live.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use egui::{RichText, Ui};
use kitchen_core::db::PlanComment;
use kitchen_core::host::Host;
use kitchen_core::{ask, plan};

use crate::ask_thread::{self, Thread};
use crate::engine::{Engine, Tab, Workspace};
use crate::links::Link;
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
    Answer(ask_thread::Answer),
}

/// A ⌘⇧A thread.
struct PlanAsk {
    /// Doc indices of the asked lines: the answers go under the last.
    start: usize,
    line: usize,
    thread: Thread,
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
    /// The thread the open question field follows up on.
    follow_up: Option<u64>,
    /// A thread that just answered: its follow-up field may open.
    offered: Option<u64>,
    answerer: ask_thread::Answerer,
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
            follow_up: None,
            offered: None,
            answerer: ask::ask,
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
                Msg::Answer(ans) => match self.asks.iter_mut().find(|a| a.thread.id == ans.id) {
                    Some(a) => {
                        if ans.result.is_ok() {
                            self.offered = Some(ans.id);
                        }
                        a.thread.finish(ans.asker, ans.result);
                    }
                    // Closed while it was being answered.
                    None => ask_thread::discard(ans),
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
                Block { after: Some(at), title: format!("comment #{} · {lines} · {label}", c.id), body: c.body.clone(), color, dim, ..Default::default() }
            })
            .chain(self.asks.iter().flat_map(|a| a.thread.blocks(a.line.min(last), &self.engine.ctx)))
            .collect()
    }

    /// ⌘⇧A: a question field under the cursor line or visual selection. On
    /// a thread's lines (and no selection), a follow-up to the last thread.
    fn start_ask(&mut self) {
        if self.doc.lines.is_empty() {
            self.status = Some(("no plan to ask about yet".into(), false));
            return;
        }
        self.follow_up = self.vim.visual_lines().is_none().then(|| self.asks_on_cursor().last().copied()).flatten();
        if let Some(id) = self.follow_up {
            self.open_follow_up(id);
            return;
        }
        let (start, end) = self.vim.visual_lines().unwrap_or((self.vim.line, self.vim.line));
        let what = if end > start { format!("lines {}–{}", start + 1, end + 1) } else { format!("line {}", start + 1) };
        let who = self.engine.find_tab(&self.tab_id).map_or_else(|| self.engine.settings().ask_model, |_| "the planning agent".into());
        let label = format!("Ask {who} about {what} — Enter asks · Esc cancels");
        self.editor = Some(Editor { start, end, text: String::new(), label, needs_focus: true, ask: true, follow_up: false });
    }

    /// Send the question field, with the plan, to a fork of the planning
    /// agent's session; its block shows "thinking… Ns" until it answers.
    fn submit_ask(&mut self) {
        let Some(ed) = self.editor.take() else { return };
        let text = ed.text.trim().to_string();
        if text.is_empty() {
            self.editor = Some(ed);
            return;
        }
        if let Some(id) = self.follow_up.take() {
            let (tx, ctx, answer) = (self.tx.clone(), self.engine.ctx.clone(), self.answerer);
            let Some(a) = self.asks.iter_mut().find(|a| a.thread.id == id) else { return };
            let sent = a.thread.follow_up(&text, answer, move |ans| {
                let _ = tx.send(Msg::Answer(ans));
                ctx.request_repaint();
            });
            if let Err(why) = sent {
                self.status = Some((why.into(), true));
                self.follow_up = Some(id);
                self.editor = Some(ed);
            }
            return;
        }
        self.ask_seq += 1;
        let id = self.ask_seq;
        self.asks.push(PlanAsk { start: ed.start, line: ed.end, thread: Thread::new(id, &text) });
        self.vim.anchor = None;
        let quote = self.doc.lines.get(ed.start..=ed.end).unwrap_or_default().iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        let plan_text = self.text.clone().unwrap_or_default();
        let path = self.plan.as_ref().map(|d| d.path.clone()).unwrap_or_else(|| plan::plan_file_for(&self.worktree, &self.tab_id));
        let model = self.engine.settings().ask_model;
        let tab = self.engine.find_tab(&self.tab_id).map(|(_, t)| t.row);
        let (host, wt, tx, ctx, answer) = (self.host.clone(), self.worktree.clone(), self.tx.clone(), self.engine.ctx.clone(), self.answerer);
        let (first, last) = (ed.start + 1, ed.end + 1);
        std::thread::spawn(move || {
            let asker = ask::Asker::for_tab(host, tab.as_ref(), &wt, &model);
            let q = ask::PlanQuestion { path: &path, first, last, quote: &quote, plan: &plan_text, text: &text };
            let result = answer(&asker, &ask::Prompt::Plan(q)).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Answer(ask_thread::Answer { id, asker, result }));
            ctx.request_repaint();
        });
    }

    /// The threads about the cursor line, oldest first.
    fn asks_on_cursor(&self) -> Vec<u64> {
        let last = self.doc.lines.len().saturating_sub(1);
        self.asks.iter().filter(|a| (a.start.min(last)..=a.line.min(last)).contains(&self.vim.line)).map(|a| a.thread.id).collect()
    }

    /// The follow-up field of thread `id`, under its cards.
    fn open_follow_up(&mut self, id: u64) {
        let last = self.doc.lines.len().saturating_sub(1);
        let Some(a) = self.asks.iter().find(|a| a.thread.id == id) else { return };
        if let Some(why) = a.thread.blocked() {
            self.status = Some((why.into(), true));
            return;
        }
        self.follow_up = Some(id);
        self.editor = Some(a.thread.editor(a.line.min(last)));
    }

    /// A thread just answered: open its follow-up field if William is still
    /// on its lines in this view (`focused`) and writing nothing else.
    fn offer_follow_up(&mut self, focused: bool) {
        let Some(id) = self.offered.take() else { return };
        if focused && self.editor.is_none() && self.vim.anchor.is_none() && self.asks_on_cursor().contains(&id) {
            self.open_follow_up(id);
        } else {
            self.status = Some(("answered · ⌘⇧A on its lines follows up · Esc there closes it".into(), false));
        }
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
                    follow_up: false,
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
            self.offer_follow_up(cx.focused);
            let blocks = self.blocks();
            let mut out = CodeView {
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
                repo: None,
                id: egui::Id::new(("plan", &self.tab_id)),
            }
            .show(&mut body_ui);
            if out.clicked {
                cx.clicked = true;
            }
            for url in out.urls {
                self.engine.open_link(&self.tab_id, Link::Url(url));
            }
            if out.cancel_editor {
                self.editor = None;
                // egui drops the field's focus on that Esc, so vim saw it
                // too: it must not also close the thread under the field.
                out.actions.retain(|a| !matches!(a, VimAction::Escape));
            }
            if std::mem::take(&mut self.submit_pending) || out.submit_editor {
                if self.editor.as_ref().is_some_and(|e| e.ask) {
                    self.submit_ask();
                } else {
                    self.submit_comment();
                }
            }
            if out.actions.iter().any(|a| matches!(a, VimAction::Escape)) {
                // Esc: close the threads under the cursor line.
                let shut = self.asks_on_cursor();
                let (closed, kept): (Vec<PlanAsk>, _) = std::mem::take(&mut self.asks).into_iter().partition(|a| shut.contains(&a.thread.id));
                self.asks = kept;
                ask_thread::close(closed.into_iter().map(|a| a.thread));
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
        p.answerer = |a, p| match p {
            ask::Prompt::Plan(q) => Ok(format!("{}|{}|{}-{}|{}|{}|{}", a.model, a.session.is_some(), q.first, q.last, q.quote, q.plan.lines().count(), q.text)),
            ask::Prompt::FollowUp(t) => Ok(format!("follow|{t}")),
            ask::Prompt::File(_) => unreachable!(),
        };
        settle(&ctx, &engine, &mut p, &|p| p.doc.lines.len() == 3);
        frame(&ctx, &engine, &mut p, vec![egui::Event::Text("jV".into())]);
        frame(&ctx, &engine, &mut p, vec![egui::Event::Text("j".into())]);
        p.command(PanelCmd::Ask, &engine);
        assert!(p.editor.as_ref().is_some_and(|e| e.ask && e.label.starts_with("Ask the planning agent about lines 2–3")), "{:?}", p.editor.as_ref().map(|e| &e.label));
        frame(&ctx, &engine, &mut p, Vec::new());
        frame(&ctx, &engine, &mut p, vec![egui::Event::Text("why LRU?".into())]);
        frame(&ctx, &engine, &mut p, key(egui::Key::Enter));
        assert!(p.editor.is_none(), "submitted");
        let ask = |p: &PlanView| p.blocks().into_iter().find(|b| b.title.starts_with("why LRU?"));
        assert_eq!(ask(&p).map(|b| (b.after, b.body)), Some((Some(2), "thinking… 0s".into())));
        settle(&ctx, &engine, &mut p, &|p| ask(p).is_some_and(|b| !b.dim));
        assert_eq!(ask(&p).unwrap().body, "sonnet|false|2-3|## F1 cache\nuse an LRU|3|why LRU?");
        // The answer opens a follow-up field under the thread.
        let follow_up = |p: &PlanView| p.editor.as_ref().filter(|e| e.follow_up && e.end == 2 && e.label.starts_with("Follow up on “why LRU?”")).is_some();
        assert!(follow_up(&p));
        // Esc closes it; ⌘⇧A on any asked line opens it again.
        frame(&ctx, &engine, &mut p, key(egui::Key::Escape));
        assert!(p.editor.is_none() && ask(&p).is_some());
        frame(&ctx, &engine, &mut p, vec![egui::Event::Text("k".into())]);
        p.command(PanelCmd::Ask, &engine);
        assert!(follow_up(&p), "{:?}", p.editor.as_ref().map(|e| &e.label));
        frame(&ctx, &engine, &mut p, Vec::new());
        frame(&ctx, &engine, &mut p, vec![egui::Event::Text("how big?".into())]);
        frame(&ctx, &engine, &mut p, key(egui::Key::Enter));
        let follow = |p: &PlanView| p.blocks().into_iter().find(|b| b.title == "↳ how big?");
        settle(&ctx, &engine, &mut p, &|p| follow(p).is_some_and(|b| !b.dim));
        assert_eq!(follow(&p).map(|b| (b.after, b.body)), Some((Some(2), "follow|how big?".into())));
        // Esc elsewhere keeps it; Esc on its lines closes it.
        frame(&ctx, &engine, &mut p, key(egui::Key::Escape));
        frame(&ctx, &engine, &mut p, vec![egui::Event::Text("kk".into())]);
        frame(&ctx, &engine, &mut p, key(egui::Key::Escape));
        assert!(ask(&p).is_some());
        frame(&ctx, &engine, &mut p, vec![egui::Event::Text("j".into())]);
        frame(&ctx, &engine, &mut p, key(egui::Key::Escape));
        assert!(ask(&p).is_none() && follow(&p).is_none());
    }
}
