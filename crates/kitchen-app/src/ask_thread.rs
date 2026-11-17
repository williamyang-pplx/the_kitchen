//! A ⌘⇧A thread in the viewer or the plan reviewer: a question about some
//! lines, its follow-ups and their answers, drawn as cards under the lines.
//! When an answer arrives a follow-up field opens under the cards (if the
//! cursor is still on the thread's lines); ⌘⇧A on them opens it again. Esc
//! on them closes the thread and deletes its session (see `kitchen_core::ask`).

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use kitchen_core::ask::{self, Asker, Prompt};

use crate::viewer::code_view::{Block, Editor};

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

    fn pending(&self) -> bool {
        self.turns.last().is_some_and(|t| t.answer.is_none())
    }

    /// Why a follow-up can't be asked now, if it can't.
    pub fn blocked(&self) -> Option<String> {
        if self.pending() {
            Some("the last question is still being answered".into())
        } else if !self.turns[0].answer.as_ref().is_some_and(|a| a.is_ok()) {
            // There may be no session to resume.
            Some(failed())
        } else {
            None
        }
    }

    /// The follow-up field, under the thread's cards at doc row `row`.
    pub fn editor(&self, row: usize) -> Editor {
        let label = ask_label(&format!("Follow up on “{}”", self.turns[0].question));
        Editor { start: row, end: row, text: String::new(), label, needs_focus: true, ask: true, follow_up: true }
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

/// Put `ans` in its thread among `threads`, and offer a follow-up
/// (`offered`) if it's an answer.
pub fn finish<'a>(threads: impl IntoIterator<Item = &'a mut Thread>, offered: &mut Option<u64>, ans: Answer) {
    let Some(t) = threads.into_iter().find(|t| t.id == ans.id) else {
        // Closed while it was being answered.
        std::thread::spawn(move || ask::close(&ans.asker));
        return;
    };
    if ans.result.is_ok() {
        *offered = Some(ans.id);
    }
    t.asker.get_or_insert(ans.asker);
    if let Some(turn) = t.turns.last_mut() {
        turn.answer = Some(ans.result);
    }
}

/// Ask `text` in thread `id` of `threads` (if still open) on a worker; the
/// answer comes back on `tx` as `msg`. `Err`: why it can't be asked now.
pub fn follow_up<'a, M: Send + 'static>(threads: impl IntoIterator<Item = &'a mut Thread>, id: u64, text: &str, answer: Answerer, tx: Sender<M>, msg: fn(Answer) -> M, ctx: egui::Context) -> Result<(), String> {
    let Some(t) = threads.into_iter().find(|t| t.id == id) else { return Ok(()) };
    if let Some(why) = t.blocked() {
        return Err(why);
    }
    let Some(asker) = t.asker.clone() else { return Err(failed()) };
    t.turns.push(Turn { question: text.to_string(), asked: Instant::now(), answer: None });
    let text = text.to_string();
    std::thread::spawn(move || {
        let result = answer(&asker, &Prompt::FollowUp(&text)).map_err(|e| format!("{e:#}"));
        let _ = tx.send(msg(Answer { id, asker, result }));
        ctx.request_repaint();
    });
    Ok(())
}

/// Close the threads among `asks` (`thread` gets an ask's) whose ids are in
/// `shut`, and delete their sessions on a worker.
pub fn close<T>(asks: &mut Vec<T>, shut: &[u64], thread: impl Fn(&mut T) -> &mut Thread) {
    let askers: Vec<Asker> = asks.extract_if(.., |a| shut.contains(&thread(a).id)).filter_map(|mut a| thread(&mut a).asker.take()).collect();
    if !askers.is_empty() {
        std::thread::spawn(move || askers.iter().for_each(ask::close));
    }
}

/// "{what} — Enter asks · Esc cancels", with the live `[input]` keys.
pub fn ask_label(what: &str) -> String {
    use crate::keybindings::{FieldCmd, Place, labelled};
    labelled(what, &[(Place::Input, FieldCmd::Submit.into(), "asks"), (Place::Input, FieldCmd::Cancel.into(), "cancels")])
}

/// "answered · ⌘⇧A on its lines follows up · Esc there closes it".
pub fn answered_note() -> String {
    use crate::keybindings::{Place, app::Action, hint_line};
    use crate::viewer::vim::VimCmd;
    let keys = hint_line(&[(Place::Panel, Action::Ask.into(), "on its lines follows up"), (Place::Vim, VimCmd::Escape.into(), "there closes it")]);
    if keys.is_empty() { "answered".into() } else { format!("answered · {keys}") }
}

/// Why a failed thread takes no follow-up.
fn failed() -> String {
    match crate::keybindings::hint(crate::keybindings::Place::Vim, crate::viewer::vim::VimCmd::Escape) {
        k if k.is_empty() => "the question failed: close it, then ask again".into(),
        k => format!("the question failed: {k} closes it, then ask again"),
    }
}

