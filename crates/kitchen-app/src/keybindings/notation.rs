//! How keys are written in keybindings.toml and shown in hints: chords
//! (`cmd+shift+p`), vim-notation sequences (`gg`, `<C-d>`, `<leader>e`) and
//! mouse bindings (`cmd+click`, `middle-click`).

use egui::{Event, InputState, Key, Modifiers};

/// The modifiers of one press, matched exactly (⌘Y is not ⌘⇧Y).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods {
    pub cmd: bool,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    pub fn of(m: Modifiers) -> Mods {
        Mods { cmd: m.mac_cmd || m.command, shift: m.shift, alt: m.alt, ctrl: m.ctrl }
    }

    /// Cmd, alt or ctrl: the press is a chord, not typing.
    pub fn any_held(&self) -> bool {
        self.cmd || self.alt || self.ctrl
    }

    fn label(&self) -> String {
        [(self.cmd, '⌘'), (self.ctrl, '⌃'), (self.alt, '⌥'), (self.shift, '⇧')].iter().filter(|(on, _)| *on).map(|(_, c)| *c).collect()
    }
}

/// One keystroke: a typed character (layout-aware, like vim: `G`, `]`, ` `)
/// or a non-text key / a key with cmd, alt or ctrl (`<C-d>`, `<CR>`, `cmd+t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Press {
    Char(char),
    Key { key: Key, mods: Mods },
}

impl Press {
    /// `key` pressed with `m`, as `presses` reads it: None for a text key
    /// typed without cmd / alt / ctrl (its `Event::Text` is the press).
    pub fn of_key(key: Key, m: Modifiers) -> Option<Press> {
        let mods = Mods::of(m);
        if mods.any_held() {
            Some(Press::Key { key, mods })
        } else if non_text(key) {
            // ⇧↓ is ↓, ⇧⏎ is ⏎.
            Some(Press::Key { key, mods: Mods::default() })
        } else {
            None
        }
    }

    /// "⌘⇧P", "⌃D", "⏎", "G": ⌘ ⌃ ⌥ ⇧ then the key glyph.
    pub fn label(&self) -> String {
        match self {
            Press::Char(' ') => "space".into(),
            Press::Char(c) => c.to_string(),
            Press::Key { key, mods } => format!("{}{}", mods.label(), key_glyph(*key)),
        }
    }
}

fn key_glyph(key: Key) -> &'static str {
    match key {
        Key::Enter => "⏎",
        Key::Escape => "esc",
        Key::ArrowUp => "↑",
        Key::ArrowDown => "↓",
        Key::ArrowLeft => "←",
        Key::ArrowRight => "→",
        Key::Backspace => "⌫",
        Key::Delete => "⌦",
        Key::Tab => "⇥",
        Key::Space => "space",
        Key::PageUp => "PgUp",
        Key::PageDown => "PgDn",
        Key::Minus => "-",
        k => k.symbol_or_name(),
    }
}

/// Keys that type no text: they are presses on their own.
fn non_text(key: Key) -> bool {
    use Key::*;
    matches!(key, Escape | Enter | Backspace | Tab | Delete | Insert | ArrowUp | ArrowDown | ArrowLeft | ArrowRight | PageUp | PageDown | Home | End)
        || key.name().starts_with('F') && key.name().len() > 1 && key.name()[1..].chars().all(|c| c.is_ascii_digit())
}

/// A binding: one press (a chord) or several (a sequence).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Binding(pub Vec<Press>);

impl Binding {
    /// Parse one value from the file. `cmd+shift+p` / `ctrl+d` are chords;
    /// anything else is vim notation, `<leader>` expanded to `leader`.
    pub fn parse(s: &str, leader: &[Press]) -> Result<Binding, String> {
        if s.is_empty() {
            return Err("an empty key".into());
        }
        if let Some(p) = parse_chord(s)? {
            return Ok(Binding(vec![p]));
        }
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(c) = rest.chars().next() {
            if c == '<' {
                if let Some(end) = rest.find('>').filter(|&e| e > 1) {
                    let token = &rest[1..end];
                    rest = &rest[end + 1..];
                    if token.eq_ignore_ascii_case("leader") {
                        if leader.is_empty() {
                            return Err(format!("<leader> in {s:?}: no leader here"));
                        }
                        out.extend_from_slice(leader);
                    } else {
                        out.push(parse_vim_key(token).map_err(|e| format!("{e} in {s:?}"))?);
                    }
                    continue;
                }
            }
            rest = &rest[c.len_utf8()..];
            out.push(Press::Char(c));
        }
        Ok(Binding(out))
    }

    /// The presses' labels joined ("gg", "⌘⇧P", "\e").
    pub fn label(&self) -> String {
        self.0.iter().map(Press::label).collect()
    }

    /// One press with cmd, alt or ctrl.
    pub fn is_chord(&self) -> bool {
        matches!(self.0.as_slice(), [Press::Key { mods, .. }] if mods.any_held())
    }
}

/// `cmd+shift+p`: every part but the last is a modifier name. None when `s`
/// is not written as a chord.
fn parse_chord(s: &str) -> Result<Option<Press>, String> {
    let parts: Vec<&str> = s.split('+').collect();
    let Some((last, mods)) = parts.split_last().filter(|(l, m)| !l.is_empty() && !m.is_empty() && m.iter().all(|p| !p.is_empty())) else { return Ok(None) };
    let mut m = Mods::default();
    for p in mods {
        match p.trim().to_ascii_lowercase().as_str() {
            "cmd" | "command" | "super" => m.cmd = true,
            "shift" => m.shift = true,
            "alt" | "opt" | "option" | "meta" => m.alt = true,
            "ctrl" | "control" => m.ctrl = true,
            _ if mods.len() == 1 && p.chars().count() == 1 => return Ok(None), // `a+b` typed
            other => return Err(format!("unknown modifier {other:?} in {s:?}")),
        }
    }
    let name = last.trim();
    let key = parse_key(&name.to_ascii_lowercase()).ok_or_else(|| format!("unknown key {name:?} in {s:?}"))?;
    Ok(Some(press(key, m, s)?))
}

/// The press `key` with `m` stands for, as `presses` reports it.
fn press(key: Key, m: Mods, s: &str) -> Result<Press, String> {
    if m.any_held() || non_text(key) {
        return Ok(Press::Key { key, mods: if m.any_held() { m } else { Mods::default() } });
    }
    // Typed: the character.
    let name = key.name();
    match (name.chars().next(), name.chars().count()) {
        (Some(c), 1) if c.is_ascii_alphabetic() => Ok(Press::Char(if m.shift { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() })),
        _ if key == Key::Space => Ok(Press::Char(' ')),
        _ if !m.shift => key_char(key).map(Press::Char).ok_or_else(|| format!("{s:?}: write the character itself")),
        _ => Err(format!("{s:?}: write the shifted character itself")),
    }
}

/// The character a plain key types on a US layout.
fn key_char(key: Key) -> Option<char> {
    let s = key.symbol_or_name();
    let mut cs = s.chars();
    match (cs.next(), cs.next()) {
        (Some(c), None) => Some(if key == Key::Minus { '-' } else { c.to_ascii_lowercase() }),
        _ => None,
    }
}

/// One `<…>` token of vim notation: `C-d`, `CR`, `S-Tab`.
fn parse_vim_key(token: &str) -> Result<Press, String> {
    let mut m = Mods::default();
    let mut rest = token;
    while rest.len() > 2 && rest.as_bytes()[1] == b'-' {
        match rest.as_bytes()[0].to_ascii_lowercase() {
            b'c' => m.ctrl = true,
            b'a' | b'm' => m.alt = true,
            b'd' => m.cmd = true,
            b's' => m.shift = true,
            _ => return Err(format!("unknown modifier in <{token}>")),
        }
        rest = &rest[2..];
    }
    let named = match rest.to_ascii_lowercase().as_str() {
        "cr" | "enter" | "return" => Some(Key::Enter),
        "esc" => Some(Key::Escape),
        "bs" => Some(Key::Backspace),
        "del" => Some(Key::Delete),
        "tab" => Some(Key::Tab),
        "space" => Some(Key::Space),
        "up" => Some(Key::ArrowUp),
        "down" => Some(Key::ArrowDown),
        "left" => Some(Key::ArrowLeft),
        "right" => Some(Key::ArrowRight),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "lt" if !m.any_held() => return Ok(Press::Char('<')),
        "lt" => Some(Key::Comma),
        _ => None,
    };
    let key = match named {
        Some(k) => k,
        None if rest.chars().count() == 1 => {
            let c = rest.chars().next().unwrap_or(' ');
            if !m.any_held() {
                // <S-x>: the uppercase letter.
                return match c {
                    c if c.is_ascii_alphabetic() && m.shift => Ok(Press::Char(c.to_ascii_uppercase())),
                    c if !m.shift => Ok(Press::Char(c)),
                    _ => Err(format!("<{token}>: write the shifted character itself")),
                };
            }
            parse_key(&c.to_ascii_lowercase().to_string()).ok_or_else(|| format!("unknown key in <{token}>"))?
        }
        None => return Err(format!("unknown key <{token}>")),
    };
    press(key, m, token)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Click {
    Primary,
    Middle,
}

/// `cmd+click`, `middle-click`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MouseBinding {
    pub mods: Mods,
    pub button: Click,
}

impl MouseBinding {
    pub fn parse(s: &str) -> Result<MouseBinding, String> {
        let parts: Vec<&str> = s.split('+').map(str::trim).collect();
        let Some((last, mods)) = parts.split_last() else { return Err("an empty click".into()) };
        let button = match last.to_ascii_lowercase().as_str() {
            "click" => Click::Primary,
            "middle-click" | "middleclick" => Click::Middle,
            _ => return Err(format!("unknown click {last:?} in {s:?} (click, middle-click)")),
        };
        let mut m = Mods::default();
        for p in mods {
            match p.to_ascii_lowercase().as_str() {
                "cmd" | "command" | "super" => m.cmd = true,
                "shift" => m.shift = true,
                "alt" | "opt" | "option" | "meta" => m.alt = true,
                "ctrl" | "control" => m.ctrl = true,
                other => return Err(format!("unknown modifier {other:?} in {s:?}")),
            }
        }
        Ok(MouseBinding { mods: m, button })
    }

    /// "⌘-click", "middle-click".
    pub fn label(&self) -> String {
        let what = match self.button {
            Click::Primary => "click",
            Click::Middle => "middle-click",
        };
        let mods = self.mods.label();
        if mods.is_empty() { what.to_string() } else { format!("{mods}-{what}") }
    }

    pub fn button(&self) -> egui::PointerButton {
        match self.button {
            Click::Primary => egui::PointerButton::Primary,
            Click::Middle => egui::PointerButton::Middle,
        }
    }
}

/// This frame's presses, in event order. A key held with cmd / alt / ctrl
/// or a key that types nothing is a `Press::Key`; every typed character
/// comes from `Event::Text` as a `Press::Char` (Shift never sits on a
/// letter, Space is `Char(' ')`). Text is dropped while ctrl or cmd is held,
/// and the character an alt key composes too. Repeats count.
pub fn presses(input: &InputState) -> Vec<Press> {
    let mut out = Vec::new();
    let mut after_alt = false;
    for e in &input.events {
        match e {
            Event::Key { key, pressed: true, modifiers, .. } => {
                let p = Press::of_key(*key, *modifiers);
                after_alt = matches!(p, Some(Press::Key { mods, .. }) if mods.alt);
                out.extend(p);
            }
            Event::Text(t) => {
                if !(std::mem::take(&mut after_alt) || input.modifiers.ctrl || input.modifiers.mac_cmd) {
                    out.extend(t.chars().map(Press::Char));
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether `press` happened this frame, without consuming it. With
/// `repeats = false`, held-key repeats don't count.
pub fn pressed(input: &InputState, press: &Press, repeats: bool) -> bool {
    input.events.iter().any(|e| is_press(e, press, repeats) || matches!((e, press), (Event::Text(t), Press::Char(c)) if t.contains(*c)))
}

fn is_press(e: &Event, press: &Press, repeats: bool) -> bool {
    match e {
        Event::Key { key, pressed: true, repeat, modifiers, .. } if repeats || !repeat => match press {
            Press::Key { .. } => Press::of_key(*key, *modifiers).as_ref() == Some(press),
            // The key that typed it.
            Press::Char(c) => !Mods::of(*modifiers).any_held() && char_key(*c) == Some(*key),
        },
        _ => false,
    }
}

/// Consume the events behind `press` this frame (the `Key` and the `Text`
/// it typed), like `consume_key`; false when it wasn't pressed. With
/// `repeats = false`, held-key repeats don't count.
pub fn take(input: &mut InputState, press: &Press, repeats: bool) -> bool {
    match press {
        Press::Key { mods, .. } => {
            let Some(i) = input.events.iter().position(|e| is_press(e, press, repeats)) else { return false };
            input.events.remove(i);
            if mods.alt && matches!(input.events.get(i), Some(Event::Text(_))) {
                input.events.remove(i);
            }
            true
        }
        Press::Char(c) => {
            let Some(i) = input.events.iter().position(|e| matches!(e, Event::Text(t) if t.contains(*c))) else { return false };
            if let Event::Text(t) = &mut input.events[i] {
                if let Some(at) = t.find(*c) {
                    t.replace_range(at..at + c.len_utf8(), "");
                }
                if t.is_empty() {
                    input.events.remove(i);
                }
            }
            if let Some(k) = input.events.iter().position(|e| is_press(e, press, true)) {
                input.events.remove(k);
            }
            true
        }
    }
}

/// The key that types `c` without modifiers (Shift aside).
fn char_key(c: char) -> Option<Key> {
    Key::from_name(&c.to_ascii_uppercase().to_string())
}

/// A key name (`a`, `enter`, `]`, `period`) → `Key`.
pub(crate) fn parse_key(k: &str) -> Option<Key> {
    let named = match k {
        "enter" | "return" | "cr" => Some(Key::Enter),
        "esc" | "escape" => Some(Key::Escape),
        "tab" => Some(Key::Tab),
        "space" => Some(Key::Space),
        "up" => Some(Key::ArrowUp),
        "down" => Some(Key::ArrowDown),
        "left" => Some(Key::ArrowLeft),
        "right" => Some(Key::ArrowRight),
        "backspace" | "bs" => Some(Key::Backspace),
        "delete" | "del" => Some(Key::Delete),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "," | "comma" => Some(Key::Comma),
        "." | "period" => Some(Key::Period),
        ";" | "semicolon" => Some(Key::Semicolon),
        "/" | "slash" => Some(Key::Slash),
        "\\" | "backslash" => Some(Key::Backslash),
        "[" => Some(Key::OpenBracket),
        "]" => Some(Key::CloseBracket),
        "-" | "minus" => Some(Key::Minus),
        "=" | "equals" => Some(Key::Equals),
        "`" | "backtick" => Some(Key::Backtick),
        "'" | "quote" => Some(Key::Quote),
        _ => None,
    };
    named.or_else(|| Key::from_name(&k.to_ascii_uppercase())).or_else(|| Key::from_name(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, mods: Mods) -> Press {
        Press::Key { key, mods }
    }

    const CMD: Mods = Mods { cmd: true, shift: false, alt: false, ctrl: false };
    const CTRL: Mods = Mods { cmd: false, shift: false, alt: false, ctrl: true };

    #[test]
    fn chords_sequences_and_clicks_parse() {
        let b = |s: &str| Binding::parse(s, &[Press::Char('\\')]).unwrap().0;
        assert_eq!(b("cmd+shift+p"), vec![key(Key::P, Mods { shift: true, ..CMD })]);
        assert_eq!(b("<C-d>"), vec![key(Key::D, CTRL)]);
        assert_eq!(b("<C-]>"), vec![key(Key::CloseBracket, CTRL)]);
        assert_eq!(b("gg"), vec![Press::Char('g'), Press::Char('g')]);
        assert_eq!(b("]c"), vec![Press::Char(']'), Press::Char('c')]);
        assert_eq!(b("<Space>"), vec![Press::Char(' ')]);
        assert_eq!(b("<leader>e"), vec![Press::Char('\\'), Press::Char('e')]);
        assert_eq!(b("<S-g>"), vec![Press::Char('G')]);
        assert_eq!(b("<CR>"), vec![key(Key::Enter, Mods::default())]);
        assert_eq!(b("shift+enter"), vec![key(Key::Enter, Mods::default())], "⇧⏎ is ⏎");
        assert_eq!(b("+"), vec![Press::Char('+')]);
        assert_eq!(b("cmd+period"), vec![key(Key::Period, CMD)]);
        assert!(Binding::parse("cmd+shift+p", &[]).unwrap().is_chord());
        assert!(!Binding::parse("gg", &[]).unwrap().is_chord());
        assert_eq!(MouseBinding::parse("cmd+click").unwrap(), MouseBinding { mods: CMD, button: Click::Primary });
        assert_eq!(MouseBinding::parse("middle-click").unwrap(), MouseBinding { mods: Mods::default(), button: Click::Middle });
        for bad in ["cmd+foo", "<X-a>", "", "<nope>", "foo+t"] {
            let e = Binding::parse(bad, &[]).unwrap_err();
            assert!(!e.contains('\n'), "{e}");
        }
        assert!(Binding::parse("<leader>e", &[]).is_err(), "no leader");
        assert!(MouseBinding::parse("cmd+tap").is_err());
    }

    #[test]
    fn labels_read_like_the_old_hints() {
        let l = |s: &str| Binding::parse(s, &[Press::Char('\\')]).unwrap().label();
        assert_eq!(l("cmd+shift+p"), "⌘⇧P");
        assert_eq!(l("cmd+enter"), "⌘⏎");
        assert_eq!(l("<C-o>"), "⌃O");
        assert_eq!(l("<leader>e"), "\\e");
        assert_eq!(l("cmd+period"), "⌘.");
        assert_eq!(l("<Down>"), "↓");
        assert_eq!(MouseBinding::parse("cmd+click").unwrap().label(), "⌘-click");
        assert_eq!(MouseBinding::parse("middle-click").unwrap().label(), "middle-click");
    }

    fn frame_presses(events: Vec<Event>) -> Vec<Press> {
        let ctx = egui::Context::default();
        let mut out = Vec::new();
        let mut o = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| out = ui.input(presses));
        o.textures_delta.clear();
        out
    }

    #[test]
    fn typed_keys_are_one_press() {
        let ev = |key, modifiers| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        assert_eq!(frame_presses(vec![ev(Key::G, Modifiers::SHIFT), Event::Text("G".into())]), vec![Press::Char('G')]);
        assert_eq!(frame_presses(vec![ev(Key::Space, Modifiers::NONE), Event::Text(" ".into())]), vec![Press::Char(' ')]);
        assert_eq!(frame_presses(vec![Event::ModifiersChanged(Modifiers::CTRL), ev(Key::D, Modifiers::CTRL)]), vec![key(Key::D, CTRL)]);
        assert_eq!(frame_presses(vec![ev(Key::ArrowDown, Modifiers::SHIFT)]), vec![key(Key::ArrowDown, Mods::default())]);
        assert_eq!(frame_presses(vec![ev(Key::Y, Modifiers::ALT), Event::Text("¥".into())]), vec![key(Key::Y, Mods { alt: true, ..Mods::default() })], "the composed ¥ is not typed");
    }

    #[test]
    fn take_consumes_the_key_and_its_text() {
        let ctx = egui::Context::default();
        let ev = |key| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        let events = vec![ev(Key::J), Event::Text("jk".into()), ev(Key::Enter)];
        let mut left = Vec::new();
        let mut o = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
            ui.input_mut(|i| {
                assert!(take(i, &Press::Char('j'), true));
                assert!(take(i, &key(Key::Enter, Mods::default()), true));
                assert!(!take(i, &key(Key::Enter, Mods::default()), true));
            });
            left = ui.input(|i| i.events.clone());
        });
        o.textures_delta.clear();
        assert_eq!(left, vec![Event::Text("k".into())]);
    }
}
