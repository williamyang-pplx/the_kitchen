//! Local echo prediction for high-latency (devbox) terminals, after mosh.
//!
//! Every keystroke to a devbox makes a network round trip before its echo
//! appears. For printable characters typed at a visible cursor we draw the
//! character immediately and check it against the grid once the real echo
//! arrives. Predictions are only *shown* while recent ones were confirmed —
//! so in contexts that don't echo (vim normal mode, password prompts) a
//! single miss turns display off until typing echoes again.

use std::time::{Duration, Instant};

/// Confirmed predictions needed before predictions are displayed.
const TRUST_AFTER: u32 = 2;
/// A prediction not echoed within this window counts as a miss.
const TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, PartialEq)]
pub struct Pred {
    pub c: char,
    /// Viewport row / column where the echo is expected.
    pub row: usize,
    pub col: usize,
    pub at: Instant,
}

#[derive(Debug, Default)]
pub struct Predictor {
    pub enabled: bool,
    preds: Vec<Pred>,
    streak: u32,
}

impl Predictor {
    /// Record typed text. `cursor` is the live cursor (viewport row, col),
    /// `cols` the grid width; `None` cursor means it is hidden (no guess).
    pub fn typed(&mut self, text: &str, cursor: Option<(usize, usize)>, cols: usize) {
        if !self.enabled {
            return;
        }
        let Some((crow, ccol)) = cursor else {
            self.preds.clear();
            return;
        };
        for c in text.chars() {
            if !(c.is_ascii_graphic() || c == ' ') {
                self.preds.clear();
                return;
            }
            let (row, col) = match self.preds.last() {
                Some(p) => (p.row, p.col + 1),
                None => (crow, ccol),
            };
            if col >= cols {
                return; // line wrap: too uncertain
            }
            self.preds.push(Pred { c, row, col, at: Instant::now() });
        }
    }

    /// A non-text key (Enter, arrows, Backspace, a paste…): outcomes of
    /// earlier predictions are still checked, but new ones start fresh.
    pub fn reset(&mut self) {
        self.preds.clear();
    }

    /// Check pending predictions against the grid (`cell(row, col)`).
    pub fn validate(&mut self, cell: impl Fn(usize, usize) -> char, cursor_row: Option<usize>) {
        let now = Instant::now();
        while let Some(p) = self.preds.first() {
            if cell(p.row, p.col) == p.c {
                self.preds.remove(0);
                self.streak = self.streak.saturating_add(1);
            } else if now.duration_since(p.at) > TIMEOUT || cursor_row.is_some_and(|r| r != p.row) {
                // Missed, or the program moved elsewhere: stop guessing.
                self.preds.clear();
                self.streak = 0;
            } else {
                break;
            }
        }
    }

    /// Predictions to draw (empty until recent ones were confirmed).
    pub fn visible(&self) -> &[Pred] {
        if self.enabled && self.streak >= TRUST_AFTER { &self.preds } else { &[] }
    }

    pub fn pending(&self) -> bool {
        !self.preds.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(s: &'static str) -> impl Fn(usize, usize) -> char {
        move |_, col| s.chars().nth(col).unwrap_or(' ')
    }

    #[test]
    fn trust_builds_from_confirmed_echo() {
        let mut p = Predictor { enabled: true, ..Default::default() };
        p.typed("ab", Some((0, 2)), 80);
        assert!(p.visible().is_empty(), "not trusted yet");
        p.validate(grid("> ab"), Some(0));
        assert!(!p.pending());
        p.typed("c", Some((0, 4)), 80);
        assert_eq!(p.visible().len(), 1, "trusted after two confirmations");
        assert_eq!((p.visible()[0].row, p.visible()[0].col), (0, 4));
    }

    #[test]
    fn miss_revokes_trust() {
        let mut p = Predictor { enabled: true, streak: 5, ..Default::default() };
        p.typed("j", Some((3, 0)), 80);
        assert_eq!(p.visible().len(), 1);
        // vim normal mode: the cursor moves down instead of echoing.
        p.validate(grid(""), Some(4));
        assert!(p.visible().is_empty() && !p.pending());
        p.typed("k", Some((4, 0)), 80);
        assert!(p.visible().is_empty(), "untrusted after a miss");
    }

    #[test]
    fn hidden_cursor_and_controls_do_not_predict() {
        let mut p = Predictor { enabled: true, streak: 9, ..Default::default() };
        p.typed("x", None, 80);
        assert!(!p.pending());
        p.typed("\t", Some((0, 0)), 80);
        assert!(!p.pending());
        let mut off = Predictor::default();
        off.typed("x", Some((0, 0)), 80);
        assert!(!off.pending(), "disabled for local tabs");
    }
}
