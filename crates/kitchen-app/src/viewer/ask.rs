//! ⌘⇧A: ask the model about the cursor line or selection. The question
//! field opens under the lines like a ⌘M comment; the answer is drawn there
//! as a block until Esc on its line closes it. Answers are not stored.

use std::time::{Duration, Instant};

use kitchen_core::ask;
use kitchen_core::host::Host;

use super::code_view::{Block, Editor};
use super::{Msg, Viewer};
use crate::plugins::decor::Side;

/// Answers a question (the model; a fake in tests): (model, cwd, question).
pub(super) type Answerer = fn(&str, Option<&str>, &ask::Question) -> anyhow::Result<String>;

pub(super) struct Ask {
    id: u64,
    path: String,
    /// The last asked line: the answer goes under it.
    side: Side,
    line: usize,
    question: String,
    asked: Instant,
    /// `None` while the model is answering.
    answer: Option<Result<String, String>>,
}

impl Viewer {
    /// ⌘⇧A: a question field under the cursor line or visual selection.
    pub(super) fn start_ask(&mut self) {
        if self.browsing || self.outline || !self.current.as_ref().is_some_and(|p| self.files.contains_key(p)) {
            self.status = Some(("open a file to ask about it".into(), false));
            return;
        }
        let (start, end) = self.vim.visual_lines().unwrap_or((self.vim.line, self.vim.line));
        let (first, last) = (self.row_line(start, false).1.unwrap_or(start + 1), self.row_line(end, true).1.unwrap_or(end + 1));
        let what = if end > start || last > first { format!("lines {first}–{last}") } else { format!("line {last}") };
        let model = self.engine.settings().ask_model;
        self.reply_to = None;
        self.list_focus = false;
        let label = format!("Ask {model} about {what} — Enter asks · Esc cancels");
        self.editor = Some(Editor { start, end, text: String::new(), label, needs_focus: true, ask: true });
    }

    /// Send the question field to the model; its block shows "thinking… Ns" until it answers.
    pub(super) fn submit_ask(&mut self) {
        let Some(ed) = self.editor.take() else { return };
        let text = ed.text.trim().to_string();
        let Some(path) = self.current.clone().filter(|_| !text.is_empty()) else {
            self.editor = Some(ed);
            return;
        };
        let ((start_side, first), (side, last)) = (self.row_line(ed.start, false), self.row_line(ed.end, true));
        let Some(line) = last else { return };
        let first = first.filter(|_| start_side == side).unwrap_or(line);
        // Removed lines are numbered in the base.
        let source = match side {
            Side::Old => self.bases.get(&path).and_then(|b| b.text.clone()),
            Side::New => self.files.get(&path).map(|f| f.text.clone()),
        };
        let quote = self.quote(&path, ed.start, ed.end);
        self.ask_seq += 1;
        let id = self.ask_seq;
        self.asks.push(Ask { id, path: path.clone(), side, line, question: text.clone(), asked: Instant::now(), answer: None });
        self.vim.anchor = None;
        let model = self.engine.settings().ask_model;
        // On this mac the model may read the rest of the repo.
        let cwd = matches!(self.host, Host::Local).then(|| self.worktree.clone());
        let (tx, ctx, answer) = (self.tx.clone(), self.engine.ctx.clone(), self.answerer);
        std::thread::spawn(move || {
            let source = source.unwrap_or_default();
            let q = ask::Question { path: &path, first, last: line, base: side == Side::Old, quote: &quote, source: &source, text: &text };
            let result = answer(&model, cwd.as_deref(), &q).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Answer { id, result });
            ctx.request_repaint();
        });
    }

    pub(super) fn finish_ask(&mut self, id: u64, result: Result<String, String>) {
        // A question closed while it was being answered is gone.
        if let Some(a) = self.asks.iter_mut().find(|a| a.id == id) {
            a.answer = Some(result);
        }
    }

    /// The open file's questions, as blocks under their lines.
    pub(super) fn ask_blocks(&self) -> Vec<Block> {
        let Some(path) = self.current.as_ref().filter(|_| !self.browsing && !self.outline) else { return Vec::new() };
        if self.asks.iter().any(|a| a.answer.is_none()) {
            // The thinking card counts seconds.
            self.engine.ctx.request_repaint_after(Duration::from_millis(500));
        }
        self.asks
            .iter()
            .filter(|a| &a.path == path)
            .filter_map(|a| Some(Block::answer(self.row_at(a.side, a.line)?, &a.question, a.answer.as_ref(), a.asked)))
            .collect()
    }

    /// Esc: close the answers under the cursor line.
    pub(super) fn close_asks(&mut self) {
        let Some(path) = self.current.clone() else { return };
        let row = Some(self.vim.line);
        let shut: Vec<u64> = self.asks.iter().filter(|a| a.path == path && self.row_at(a.side, a.line) == row).map(|a| a.id).collect();
        self.asks.retain(|a| !shut.contains(&a.id));
    }
}
