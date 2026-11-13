//! Key sequences (`gg`, `]c`, `<leader>e`) across frames: one matcher per
//! focused widget, fed this frame's presses.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::notation::{Binding, Press};
use super::places::Cmd;

/// What feeding one press did.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// A binding completed.
    Fire(Cmd),
    /// Waiting for more keys (`pending()` shows them in the footer).
    Pending,
    /// Not a binding: these presses, oldest first, for the widget to drop
    /// or re-feed (the list re-feeds the last one, vim drops them).
    Unbound(Vec<Press>),
}

#[derive(Clone, Debug, Default)]
pub struct Matcher {
    pending: Vec<Press>,
    /// A complete binding that is also a prefix of a longer one, waiting,
    /// and how many of `pending` it took.
    exact: Option<(Cmd, usize)>,
    since: Option<Instant>,
}

impl Matcher {
    /// Feed one press. Expired state is settled first (as `tick` would), so a
    /// stale `g` never joins a `g` typed seconds later.
    pub fn feed(&mut self, press: Press, table: &SeqTable, now: Instant, timeout: Duration) -> Vec<Step> {
        let mut out: Vec<Step> = self.tick(now, timeout).into_iter().collect();
        self.push(press, table, now, &mut out);
        out
    }

    fn push(&mut self, press: Press, table: &SeqTable, now: Instant, out: &mut Vec<Step>) {
        self.pending.push(press);
        let exact = table.exact(&self.pending).cloned();
        let longer = table.has_longer(&self.pending);
        match (exact, longer) {
            (Some(cmd), false) => {
                self.reset();
                out.push(Step::Fire(cmd));
            }
            (Some(cmd), true) => {
                self.exact = Some((cmd, self.pending.len()));
                self.since = Some(now);
                out.push(Step::Pending);
            }
            (None, true) => {
                self.since = Some(now);
                out.push(Step::Pending);
            }
            (None, false) => match self.exact.take() {
                Some((cmd, n)) => {
                    let rest = self.pending.split_off(n);
                    self.reset();
                    out.push(Step::Fire(cmd));
                    for p in rest {
                        self.push(p, table, now, out);
                    }
                }
                None => {
                    let keys = std::mem::take(&mut self.pending);
                    self.reset();
                    out.push(Step::Unbound(keys));
                }
            },
        }
    }

    /// Called every frame while `is_pending()`: fire or drop after the timeout.
    pub fn tick(&mut self, now: Instant, timeout: Duration) -> Option<Step> {
        let since = self.since?;
        if now.duration_since(since) < timeout {
            return None;
        }
        let exact = self.exact.take();
        let keys = std::mem::take(&mut self.pending);
        self.reset();
        Some(match exact {
            Some((cmd, _)) => Step::Fire(cmd),
            None => Step::Unbound(keys),
        })
    }

    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The labels, for the footer ("g", "\").
    pub fn pending(&self) -> String {
        Binding(self.pending.clone()).label()
    }

    /// Focus left the widget: forget a half-typed sequence.
    pub fn reset(&mut self) {
        *self = Matcher::default();
    }
}

/// The keys one focus chain reads itself (vim, list, field keys, `<C-d>`
/// included; app and plugin chords excluded), innermost place first on
/// equal keys.
#[derive(Clone, Debug, Default)]
pub struct SeqTable {
    entries: Vec<(Binding, Cmd)>,
    prefixes: HashSet<Vec<Press>>,
}

impl SeqTable {
    pub fn new(entries: Vec<(Binding, Cmd)>) -> SeqTable {
        let mut prefixes = HashSet::new();
        for (b, _) in &entries {
            for n in 1..b.0.len() {
                prefixes.insert(b.0[..n].to_vec());
            }
        }
        SeqTable { entries, prefixes }
    }

    pub fn exact(&self, keys: &[Press]) -> Option<&Cmd> {
        self.entries.iter().find(|(b, _)| b.0 == keys).map(|(_, c)| c)
    }

    pub fn has_longer(&self, keys: &[Press]) -> bool {
        self.prefixes.contains(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer::vim::VimCmd;

    fn table(bs: &[(&str, VimCmd)]) -> SeqTable {
        SeqTable::new(bs.iter().map(|(s, c)| (Binding::parse(s, &[]).unwrap(), Cmd::Vim(*c))).collect())
    }

    #[test]
    fn sequences_prefixes_and_timeouts() {
        let t = table(&[("gg", VimCmd::GoToTop), ("j", VimCmd::Down), ("z", VimCmd::ScrollCenter), ("zz", VimCmd::ScrollTop)]);
        let to = Duration::from_secs(2);
        let now = Instant::now();
        let ch = Press::Char;
        let mut m = Matcher::default();
        assert_eq!(m.feed(ch('g'), &t, now, to), vec![Step::Pending]);
        assert_eq!(m.pending(), "g");
        assert_eq!(m.feed(ch('g'), &t, now, to), vec![Step::Fire(Cmd::Vim(VimCmd::GoToTop))]);
        assert!(!m.is_pending());

        assert_eq!(m.feed(ch('g'), &t, now, to), vec![Step::Pending]);
        assert_eq!(m.feed(ch('x'), &t, now, to), vec![Step::Unbound(vec![ch('g'), ch('x')])]);

        let esc = Press::Key { key: egui::Key::Escape, mods: Default::default() };
        m.feed(ch('g'), &t, now, to);
        assert_eq!(m.feed(esc, &t, now, to), vec![Step::Unbound(vec![ch('g'), esc])]);

        // A stale `g` is dropped, not joined with a new one.
        m.feed(ch('g'), &t, now, to);
        let later = now + to + Duration::from_millis(1);
        assert_eq!(m.feed(ch('g'), &t, later, to), vec![Step::Unbound(vec![ch('g')]), Step::Pending]);
        m.reset();

        // Complete and a prefix: fires on the timeout, or on a key that doesn't continue it.
        assert_eq!(m.feed(ch('z'), &t, now, to), vec![Step::Pending]);
        assert_eq!(m.tick(now, to), None);
        assert_eq!(m.tick(later, to), Some(Step::Fire(Cmd::Vim(VimCmd::ScrollCenter))));
        m.feed(ch('z'), &t, now, to);
        assert_eq!(m.feed(ch('j'), &t, now, to), vec![Step::Fire(Cmd::Vim(VimCmd::ScrollCenter)), Step::Fire(Cmd::Vim(VimCmd::Down))]);
        m.feed(ch('z'), &t, now, to);
        assert_eq!(m.feed(ch('z'), &t, now, to), vec![Step::Fire(Cmd::Vim(VimCmd::ScrollTop))]);
    }
}
