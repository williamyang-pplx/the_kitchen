//! Local echo prediction for high-latency (devbox) terminals, after mosh's
//! `PredictionEngine`.
//!
//! Every keystroke to a devbox makes a network round trip before its echo
//! appears. For printable characters typed at a visible cursor we show the
//! character immediately and check it against the grid once the real echo
//! arrives:
//!
//! * A prediction is **confirmed** only when the real cursor has moved past
//!   its cell and the cell holds the predicted character. Matching the cell
//!   alone is not enough: zsh-autosuggestions' dim ghost text already holds
//!   the next characters, and trusting it made later predictions land on top
//!   of earlier ones (overlapping, "blurred together" words).
//! * Predictions belong to **epochs**. Anything we can't predict (Enter,
//!   arrows, control keys, a miss) starts a new epoch, and an epoch's
//!   predictions stay hidden until one of them is confirmed. So the first
//!   character after Enter waits for its echo — which keeps password prompts
//!   and vim normal mode from showing guesses — and the rest of the word
//!   appears instantly.

use std::time::{Duration, Instant};

/// A prediction not echoed within this window counts as a miss.
const TIMEOUT: Duration = Duration::from_millis(1500);
/// This many *visible* misses within `MISS_WINDOW` pause prediction for `PAUSE`.
const MISSES_TO_PAUSE: usize = 4;
const MISS_WINDOW: Duration = Duration::from_secs(10);
const PAUSE: Duration = Duration::from_secs(30);
/// Echoes faster than this don't need predicting (mosh/cmux use 20–30 ms).
const MIN_LATENCY: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, PartialEq)]
pub struct Pred {
    pub c: char,
    /// Viewport row / column where the echo is expected.
    pub row: usize,
    pub col: usize,
    pub at: Instant,
    epoch: u64,
}

#[derive(Debug)]
pub struct Predictor {
    pub enabled: bool,
    preds: Vec<Pred>,
    /// Epoch new predictions join.
    epoch: u64,
    /// Latest epoch with a confirmed prediction; later epochs are hidden.
    confirmed: u64,
    /// Smoothed keystroke-to-echo time of confirmed predictions.
    latency: Option<Duration>,
    /// When shown guesses turned out wrong (recent ones only).
    misses: Vec<Instant>,
    paused_until: Option<Instant>,
}

impl Default for Predictor {
    fn default() -> Self {
        // epoch > confirmed: nothing is shown until the first echo confirms.
        Predictor { enabled: false, preds: Vec::new(), epoch: 1, confirmed: 0, latency: None, misses: Vec::new(), paused_until: None }
    }
}

impl Predictor {
    /// Record typed text. `cursor` is the live cursor (viewport row, col),
    /// `cols` the grid width; `None` cursor means it is hidden.
    pub fn typed(&mut self, text: &str, cursor: Option<(usize, usize)>, cols: usize) {
        if !self.enabled {
            return;
        }
        for c in text.chars() {
            if !(c.is_ascii_graphic() || c == ' ') {
                self.reset();
                return;
            }
            // After the last prediction, else at the cursor (which has passed
            // every confirmed prediction).
            let Some((row, col)) = self.preds.last().map(|p| (p.row, p.col + 1)).or(cursor) else {
                self.reset();
                return;
            };
            if col + 1 >= cols {
                self.reset(); // line wrap: too uncertain
                return;
            }
            self.preds.push(Pred { c, row, col, at: Instant::now(), epoch: self.epoch });
        }
    }

    /// A key we don't predict (Enter, arrows, Backspace, a paste…): drop the
    /// guesses and hide new ones until an echo confirms them again.
    pub fn reset(&mut self) {
        self.preds.clear();
        self.epoch += 1;
    }

    /// Check pending predictions against the grid (`cell(row, col)`) and the
    /// live cursor.
    pub fn validate(&mut self, cell: impl Fn(usize, usize) -> char, cursor: Option<(usize, usize)>) {
        let now = Instant::now();
        while let Some(p) = self.preds.first() {
            let passed = cursor.is_some_and(|(r, c)| r == p.row && c > p.col);
            let elsewhere = cursor.is_some_and(|(r, _)| r != p.row);
            if passed && cell(p.row, p.col) == p.c {
                self.confirmed = self.confirmed.max(p.epoch);
                let took = now.duration_since(p.at);
                self.latency = Some(self.latency.map_or(took, |l| (l * 7 + took) / 8));
                self.preds.remove(0);
            } else if passed || elsewhere || now.duration_since(p.at) > TIMEOUT {
                // Echoed something else, the program moved on, or no echo at all.
                if !self.visible().is_empty() {
                    self.note_miss(now);
                }
                self.reset();
            } else {
                break;
            }
        }
    }

    /// A guess the user saw was wrong; too many in a row pause prediction.
    fn note_miss(&mut self, now: Instant) {
        self.misses.retain(|t| now.duration_since(*t) < MISS_WINDOW);
        self.misses.push(now);
        if self.misses.len() >= MISSES_TO_PAUSE {
            self.misses.clear();
            self.paused_until = Some(now + PAUSE);
        }
    }

    /// Predictions to draw: those in confirmed epochs (none while paused, or
    /// when echoes come back fast anyway).
    pub fn visible(&self) -> &[Pred] {
        let paused = self.paused_until.is_some_and(|t| Instant::now() < t);
        let fast = self.latency.is_some_and(|l| l < MIN_LATENCY);
        if !self.enabled || paused || fast {
            return &[];
        }
        let n = self.preds.iter().take_while(|p| p.epoch <= self.confirmed).count();
        &self.preds[..n]
    }

    pub fn pending(&self) -> bool {
        !self.preds.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-row grid whose text is `s` (column 0 = first char).
    fn grid(s: &str) -> impl Fn(usize, usize) -> char + '_ {
        move |_, col| s.chars().nth(col).unwrap_or(' ')
    }

    /// Enabled, with a devbox-like echo latency (tests confirm instantly).
    fn on() -> Predictor {
        Predictor { enabled: true, latency: Some(Duration::from_millis(90)), ..Default::default() }
    }

    #[test]
    fn first_echo_unlocks_the_rest_of_the_word() {
        let mut p = on();
        p.typed("l", Some((0, 2)), 80);
        assert!(p.visible().is_empty(), "first character of an epoch waits for its echo");
        p.validate(grid("> l"), Some((0, 3)));
        assert!(!p.pending());
        p.typed("s", Some((0, 3)), 80);
        assert_eq!(p.visible().len(), 1, "same epoch: shown immediately");
        assert_eq!((p.visible()[0].row, p.visible()[0].col), (0, 3));
    }

    /// zsh-autosuggestions: ghost text already holds the next characters.
    /// It must not confirm predictions before the cursor moves.
    #[test]
    fn ghost_text_does_not_confirm() {
        let mut p = on();
        p.typed("g", Some((0, 2)), 80);
        p.validate(grid("> g"), Some((0, 3)));
        // Echo of `g` arrived; zsh shows the suggestion "it status" after it.
        p.typed("i", Some((0, 3)), 80);
        p.validate(grid("> git status"), Some((0, 3)));
        assert_eq!(p.visible().len(), 1, "ghost `i` is not the echo: still predicted");
        p.typed("t", Some((0, 3)), 80);
        let cols: Vec<usize> = p.visible().iter().map(|x| x.col).collect();
        assert_eq!(cols, vec![3, 4], "`t` goes after `i`, not on top of it");
        p.validate(grid("> git status"), Some((0, 4)));
        assert_eq!(p.visible().iter().map(|x| x.c).collect::<String>(), "t");
        p.validate(grid("> git status"), Some((0, 5)));
        assert!(!p.pending());
    }

    #[test]
    fn miss_hides_guesses_until_echo_resumes() {
        let mut p = on();
        p.typed("a", Some((3, 0)), 80);
        p.validate(grid("a"), Some((3, 1)));
        p.typed("j", Some((3, 1)), 80);
        assert_eq!(p.visible().len(), 1);
        // vim normal mode: the cursor moves down instead of echoing.
        p.validate(grid("a"), Some((4, 1)));
        assert!(p.visible().is_empty() && !p.pending());
        p.typed("k", Some((4, 1)), 80);
        assert!(p.visible().is_empty(), "new epoch: hidden until confirmed");
        // Echoed a different character than predicted: a miss too.
        p.validate(grid("aX"), Some((4, 2)));
        assert!(!p.pending());
    }

    #[test]
    fn repeated_visible_misses_pause_prediction() {
        let mut p = on();
        p.latency = Some(Duration::from_millis(80));
        for i in 0..MISSES_TO_PAUSE {
            p.confirmed = p.epoch; // pretend the epoch is trusted, so the guess shows
            p.typed("z", Some((0, i)), 80);
            assert_eq!(p.visible().len(), 1);
            p.validate(grid("q"), Some((0, i + 1))); // echoed something else
        }
        p.confirmed = p.epoch;
        p.typed("z", Some((0, 9)), 80);
        assert!(p.pending() && p.visible().is_empty(), "paused after repeated misses");
    }

    #[test]
    fn fast_echo_needs_no_prediction() {
        let mut p = on();
        p.latency = Some(Duration::from_millis(3));
        p.confirmed = p.epoch;
        p.typed("a", Some((0, 0)), 80);
        assert!(p.visible().is_empty());
    }

    #[test]
    fn enter_and_controls_start_a_hidden_epoch() {
        let mut p = on();
        p.typed("x", Some((0, 0)), 80);
        p.validate(grid("x"), Some((0, 1)));
        p.reset(); // Enter: e.g. a password prompt follows
        p.typed("hunter2", Some((1, 10)), 80);
        assert!(p.visible().is_empty(), "nothing echoes at a password prompt, so nothing is shown");
        p.typed("\t", Some((0, 0)), 80);
        assert!(!p.pending(), "controls aren't predicted");
        p.typed("x", None, 80);
        assert!(!p.pending(), "hidden cursor and no earlier guess: no anchor");
        let mut off = Predictor::default();
        off.typed("x", Some((0, 0)), 80);
        assert!(!off.pending(), "disabled for local tabs");
    }
}
