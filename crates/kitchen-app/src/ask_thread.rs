//! A ⌘⇧A thread in the viewer or the plan reviewer: a question about some
//! lines, its follow-ups and their answers, drawn as cards under the lines.
//! ⌘⇧A on the lines again asks a follow-up; Esc on them closes the thread
//! and deletes its session (see `kitchen_core::ask`).

use std::time::{Duration, Instant};

use kitchen_core::ask::{self, Asker, Prompt};

use crate::viewer::code_view::Block;

/// Answers in a thread (the model; a fake in tests).
pub type Answerer = fn(&Asker, &Prompt) -> anyhow::Result<String>;

/// A worker's answer to the last question of thread `id`.
pub struct Answer {
    pub id: u64,
    /// Who answered: follow-ups go to the same session.
    pub asker: Asker,
    pub result: Result<String, String>,
}

struct Turn {
    question: String,
    asked: Instant,
    /// `None` while the model is answering.
    answer: Option<Result<String, String>>,
}

pub struct Thread {
    pub id: u64,
    /// Set by the first answer.
    asker: Option<Asker>,
    turns: Vec<Turn>,
}

impl Thread {
    pub fn new(id: u64, question: &str) -> Thread {
        Thread { id, asker: None, turns: vec![Turn { question: question.to_string(), asked: Instant::now(), answer: None }] }
    }

    pub fn pending(&self) -> bool {
        self.turns.last().is_some_and(|t| t.answer.is_none())
    }

    /// The first question.
    pub fn question(&self) -> &str {
        &self.turns[0].question
    }

    /// Ask `text` in the thread on a worker; `send` gets the answer.
    /// `Err`: why it can't be asked now.
    pub fn follow_up(&mut self, text: &str, answer: Answerer, send: impl FnOnce(Answer) + Send + 'static) -> Result<(), &'static str> {
        if self.pending() {
            return Err("the last question is still being answered");
        }
        // The first answer failed: there may be no session to resume.
        let Some(asker) = self.asker.clone().filter(|_| self.turns[0].answer.as_ref().is_some_and(|a| a.is_ok())) else {
            return Err("the question failed: Esc closes it, then ask again");
        };
        self.turns.push(Turn { question: text.to_string(), asked: Instant::now(), answer: None });
        let (id, text) = (self.id, text.to_string());
        std::thread::spawn(move || {
            let result = answer(&asker, &Prompt::FollowUp(&text)).map_err(|e| format!("{e:#}"));
            send(Answer { id, asker, result });
        });
        Ok(())
    }

    pub fn finish(&mut self, asker: Asker, result: Result<String, String>) {
        self.asker.get_or_insert(asker);
        if let Some(t) = self.turns.last_mut() {
            t.answer = Some(result);
        }
    }

    /// The cards under row `after`, oldest first. Asks for a repaint while
    /// one thinks (its card counts the seconds).
    pub fn blocks(&self, after: usize, ctx: &egui::Context) -> impl Iterator<Item = Block> + '_ {
        if self.pending() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        self.turns.iter().enumerate().map(move |(i, t)| {
            let title = if i == 0 { t.question.clone() } else { format!("↳ {}", t.question) };
            Block::answer(after, &title, t.answer.as_ref(), t.asked)
        })
    }
}

/// Delete the threads' sessions, on a worker.
pub fn close(threads: impl IntoIterator<Item = Thread>) {
    let askers: Vec<Asker> = threads.into_iter().filter_map(|t| t.asker).collect();
    if !askers.is_empty() {
        std::thread::spawn(move || askers.iter().for_each(ask::close));
    }
}

/// An answer for a thread that was closed while it was being answered.
pub fn discard(answer: Answer) {
    std::thread::spawn(move || ask::close(&answer.asker));
}
