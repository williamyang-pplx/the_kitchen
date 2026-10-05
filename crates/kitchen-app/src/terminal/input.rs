//! egui key events -> bytes a terminal program expects (xterm encoding).
//!
//! Cmd chords never reach here: they belong to the kitchen. Option acts as
//! Meta (ESC prefix), which is what shells and vim want.

use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use egui::{Key, Modifiers};

/// xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
fn modifier_param(m: Modifiers) -> u8 {
    1 + (m.shift as u8) + 2 * (m.alt as u8) + 4 * (m.ctrl as u8)
}

/// Bytes for a key press, or `None` when the key produces text instead (the
/// matching `Event::Text` carries it).
pub fn key_bytes(key: Key, m: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let param = modifier_param(m);
    let csi_mod = |final_byte: char| -> Vec<u8> {
        if param > 1 {
            format!("\x1b[1;{param}{final_byte}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            format!("\x1b[{final_byte}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if param > 1 { format!("\x1b[{n};{param}~").into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
    };
    let alt_prefix = |mut v: Vec<u8>| -> Vec<u8> {
        if m.alt {
            v.insert(0, 0x1b);
        }
        v
    };
    Some(match key {
        Key::Enter => alt_prefix(vec![b'\r']),
        Key::Tab if m.shift => b"\x1b[Z".to_vec(),
        Key::Tab => alt_prefix(vec![b'\t']),
        Key::Backspace if m.ctrl => vec![0x08],
        Key::Backspace => alt_prefix(vec![0x7f]),
        Key::Escape => vec![0x1b],
        Key::ArrowUp => csi_mod('A'),
        Key::ArrowDown => csi_mod('B'),
        Key::ArrowRight => csi_mod('C'),
        Key::ArrowLeft => csi_mod('D'),
        Key::Home => csi_mod('H'),
        Key::End => csi_mod('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => b"\x1bOP".to_vec(),
        Key::F2 => b"\x1bOQ".to_vec(),
        Key::F3 => b"\x1bOR".to_vec(),
        Key::F4 => b"\x1bOS".to_vec(),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        _ if m.ctrl => return ctrl_bytes(key, m.alt),
        _ if m.alt => {
            // Option+letter: ESC + the unmodified character.
            let c = key_char(key)?;
            let c = if m.shift { c.to_ascii_uppercase() } else { c };
            vec![0x1b, c as u8]
        }
        _ => return None,
    })
}

fn ctrl_bytes(key: Key, alt: bool) -> Option<Vec<u8>> {
    let b = match key {
        Key::Space | Key::Num2 => 0x00,
        Key::OpenBracket | Key::Num3 => 0x1b,
        Key::Backslash | Key::Num4 => 0x1c,
        Key::CloseBracket | Key::Num5 => 0x1d,
        Key::Num6 => 0x1e,
        Key::Minus | Key::Num7 | Key::Slash => 0x1f,
        Key::Num8 | Key::Questionmark => 0x7f,
        _ => {
            let c = key_char(key)?;
            if !c.is_ascii_alphabetic() {
                return None;
            }
            (c.to_ascii_lowercase() as u8) & 0x1f
        }
    };
    Some(if alt { vec![0x1b, b] } else { vec![b] })
}

fn key_char(key: Key) -> Option<char> {
    let name = key.name();
    let mut chars = name.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return match key {
            Key::Minus => Some('-'),
            Key::Period => Some('.'),
            Key::Comma => Some(','),
            Key::Slash => Some('/'),
            Key::Semicolon => Some(';'),
            Key::Backtick => Some('`'),
            _ => None,
        };
    }
    Some(c.to_ascii_lowercase())
}

/// Wrap pasted text in bracketed-paste markers when the program asked for them.
pub fn paste_bytes(text: &str, mode: TermMode) -> Vec<u8> {
    // Normalize newlines the way terminals do: CR submits a line.
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let safe = body.replace("\x1b[201~", "");
        format!("\x1b[200~{safe}\x1b[201~").into_bytes()
    } else {
        body.into_bytes()
    }
}

/// Turns wheel deltas into whole-line scroll steps.
///
/// A fullscreen program (Claude Code) redraws its whole screen for every
/// wheel report, and on a devbox each report is a round trip. So the first
/// step of a gesture goes out at once (no waiting for a full line of
/// trackpad travel), and later steps can be paced: whatever accumulates in
/// `interval` goes out as one batch, which the program draws once.
#[derive(Default)]
pub(crate) struct Wheel {
    /// Fractional lines carried between events.
    acc: f32,
    /// Whole lines not sent yet (positive = up).
    pending: i32,
    /// The gesture has not produced a line yet.
    first_step: bool,
    /// Direction of the last report: 1 up, -1 down, 0 none yet.
    last_dir: i8,
    last_event: Option<Instant>,
    last_sent: Option<Instant>,
    /// Minimum time between sends; zero sends every frame.
    pub interval: Duration,
}

/// A wheel pause longer than this starts a new gesture.
const GESTURE_GAP: Duration = Duration::from_millis(150);
/// Travel (in lines) that sends the first step of a gesture.
const FIRST_STEP: f32 = 0.25;

impl Wheel {
    /// Add `lines` of wheel travel (positive = up).
    pub fn feed(&mut self, lines: f32, now: Instant) {
        if lines == 0.0 {
            return;
        }
        if self.last_event.is_none_or(|t| now - t > GESTURE_GAP) {
            self.acc = 0.0;
            self.first_step = true;
        }
        self.last_event = Some(now);
        self.acc += lines;
        if self.first_step && self.acc.abs() >= FIRST_STEP {
            self.acc = self.acc.signum() * self.acc.abs().max(1.0);
            self.first_step = false;
        }
        let whole = self.acc.trunc();
        self.acc -= whole;
        self.pending += whole as i32;
    }

    /// Lines to scroll now (positive = up). `paced` holds them back until
    /// `interval` has passed since the last send.
    pub fn take(&mut self, now: Instant, paced: bool) -> i32 {
        if self.pending == 0 || paced && self.last_sent.is_some_and(|t| now - t < self.interval) {
            return 0;
        }
        self.last_sent = Some(now);
        std::mem::take(&mut self.pending)
    }

    /// Whether a step `up` (or down) turns around from the last reported
    /// one (the first report counts as a turn).
    pub fn turns(&mut self, up: bool) -> bool {
        let dir = if up { 1 } else { -1 };
        std::mem::replace(&mut self.last_dir, dir) != dir
    }

    /// How long until held-back lines can go out.
    pub fn due_in(&self, now: Instant) -> Option<Duration> {
        (self.pending != 0).then(|| self.last_sent.map_or(Duration::ZERO, |t| self.interval.saturating_sub(now - t)))
    }
}

/// SGR (1006) mouse report. `button`: 0 left, 1 middle, 2 right, 64/65 wheel.
pub fn sgr_mouse(button: u8, col: usize, row: usize, pressed: bool, motion: bool, m: Modifiers) -> Vec<u8> {
    let mut b = button as u32;
    if motion {
        b += 32;
    }
    if m.shift {
        b += 4;
    }
    if m.alt {
        b += 8;
    }
    if m.ctrl {
        b += 16;
    }
    format!("\x1b[<{b};{};{}{}", col + 1, row + 1, if pressed { 'M' } else { 'm' }).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;

    #[test]
    fn basics() {
        assert_eq!(key_bytes(Key::Enter, NONE, TermMode::NONE), Some(b"\r".to_vec()));
        assert_eq!(key_bytes(Key::ArrowUp, NONE, TermMode::NONE), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes(Key::ArrowUp, NONE, TermMode::APP_CURSOR), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_bytes(Key::A, NONE, TermMode::NONE), None, "plain letters arrive as Text");
    }

    #[test]
    fn modified() {
        let ctrl = Modifiers { ctrl: true, ..NONE };
        assert_eq!(key_bytes(Key::C, ctrl, TermMode::NONE), Some(vec![3]));
        assert_eq!(key_bytes(Key::OpenBracket, ctrl, TermMode::NONE), Some(vec![0x1b]));
        let alt = Modifiers { alt: true, ..NONE };
        assert_eq!(key_bytes(Key::B, alt, TermMode::NONE), Some(vec![0x1b, b'b']));
        let shift = Modifiers { shift: true, ..NONE };
        assert_eq!(key_bytes(Key::ArrowRight, shift, TermMode::NONE), Some(b"\x1b[1;2C".to_vec()));
        assert_eq!(key_bytes(Key::Tab, shift, TermMode::NONE), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn paste() {
        assert_eq!(paste_bytes("a\nb", TermMode::NONE), b"a\rb".to_vec());
        assert_eq!(paste_bytes("x", TermMode::BRACKETED_PASTE), b"\x1b[200~x\x1b[201~".to_vec());
    }

    #[test]
    fn mouse() {
        assert_eq!(sgr_mouse(64, 0, 0, true, false, NONE), b"\x1b[<64;1;1M".to_vec());
        assert_eq!(sgr_mouse(0, 4, 2, false, false, NONE), b"\x1b[<0;5;3m".to_vec());
    }
}
