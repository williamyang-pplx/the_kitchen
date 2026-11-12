//! ⌘⇧A: ask the tab's agent (a fork of its session, see `kitchen_core::ask`)
//! about the cursor line or selection. The question field opens under the
//! lines like a ⌘M comment; the answers are drawn there as cards until Esc
//! on their line closes them. ⌘⇧A on the line again asks a follow-up in the
//! same thread. Answers are not stored.

use kitchen_core::ask;

use super::code_view::{Block, Editor};
use super::{Msg, Viewer};
use crate::ask_thread::Thread;
use crate::plugins::decor::Side;

pub(super) struct Ask {
    path: String,
    /// The last asked line: the answers go under it.
    side: Side,
    line: usize,
    thread: Thread,
}

impl Viewer {
    /// ⌘⇧A: a question field under the cursor line or visual selection. On
    /// a line with answers (and no selection), a follow-up to the last.
    pub(super) fn start_ask(&mut self) {
        if self.browsing || self.outline || !self.current.as_ref().is_some_and(|p| self.files.contains_key(p)) {
            self.status = Some(("open a file to ask about it".into(), false));
            return;
        }
        self.reply_to = None;
        self.list_focus = false;
        self.follow_up = self.vim.visual_lines().is_none().then(|| self.asks_on_cursor().last().copied()).flatten();
        if let Some(a) = self.follow_up.and_then(|id| self.asks.iter().find(|a| a.thread.id == id)) {
            let label = format!("Follow up on “{}” — Enter asks · Esc cancels", a.thread.question());
            self.editor = Some(Editor { start: self.vim.line, end: self.vim.line, text: String::new(), label, needs_focus: true, ask: true });
            return;
        }
        let (start, end) = self.vim.visual_lines().unwrap_or((self.vim.line, self.vim.line));
        let (first, last) = (self.row_line(start, false).1.unwrap_or(start + 1), self.row_line(end, true).1.unwrap_or(end + 1));
        let what = if end > start || last > first { format!("lines {first}–{last}") } else { format!("line {last}") };
        let who = self.engine.find_tab(&self.tab_id).map_or_else(|| self.engine.settings().ask_model, |_| "the agent".into());
        let label = format!("Ask {who} about {what} — Enter asks · Esc cancels");
        self.editor = Some(Editor { start, end, text: String::new(), label, needs_focus: true, ask: true });
    }

    /// Send the question field to the model; its card shows "thinking… Ns" until it answers.
    pub(super) fn submit_ask(&mut self) {
        let Some(ed) = self.editor.take() else { return };
        let text = ed.text.trim().to_string();
        let Some(path) = self.current.clone().filter(|_| !text.is_empty()) else {
            self.editor = Some(ed);
            return;
        };
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
        self.asks.push(Ask { path: path.clone(), side, line, thread: Thread::new(id, &text) });
        self.vim.anchor = None;
        let model = self.engine.settings().ask_model;
        let tab = self.engine.find_tab(&self.tab_id).map(|(_, t)| t.row);
        let (host, root, tx, ctx, answer) = (self.host.clone(), self.worktree.clone(), self.tx.clone(), self.engine.ctx.clone(), self.answerer);
        std::thread::spawn(move || {
            let asker = ask::Asker::for_tab(host, tab.as_ref(), &root, &model);
            let source = source.unwrap_or_default();
            let q = ask::Question { root: &root, path: &path, first, last: line, base: side == Side::Old, quote: &quote, source: &source, text: &text };
            let result = answer(&asker, &ask::Prompt::File(q)).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Answer(crate::ask_thread::Answer { id, asker, result }));
            ctx.request_repaint();
        });
    }

    pub(super) fn finish_ask(&mut self, ans: crate::ask_thread::Answer) {
        match self.asks.iter_mut().find(|a| a.thread.id == ans.id) {
            Some(a) => a.thread.finish(ans.asker, ans.result),
            // Closed while it was being answered.
            None => crate::ask_thread::discard(ans),
        }
    }

    /// The open file's questions and answers, as cards under their lines.
    pub(super) fn ask_blocks(&self) -> Vec<Block> {
        let Some(path) = self.current.as_ref().filter(|_| !self.browsing && !self.outline) else { return Vec::new() };
        self.asks
            .iter()
            .filter(|a| &a.path == path)
            .filter_map(|a| Some((self.row_at(a.side, a.line)?, a)))
            .flat_map(|(row, a)| a.thread.blocks(row, &self.engine.ctx))
            .collect()
    }

    /// The threads under the cursor line, oldest first.
    fn asks_on_cursor(&self) -> Vec<u64> {
        let Some(path) = self.current.as_ref() else { return Vec::new() };
        let row = Some(self.vim.line);
        self.asks.iter().filter(|a| &a.path == path && self.row_at(a.side, a.line) == row).map(|a| a.thread.id).collect()
    }

    /// Esc: close the threads under the cursor line.
    pub(super) fn close_asks(&mut self) {
        let shut = self.asks_on_cursor();
        let (closed, kept) = std::mem::take(&mut self.asks).into_iter().partition(|a| shut.contains(&a.thread.id));
        self.asks = kept;
        crate::ask_thread::close(closed.into_iter().map(|a: Ask| a.thread));
    }
}
