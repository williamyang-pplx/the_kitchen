//! Chord strings like `cmd+shift+y` → egui modifiers + key.

use egui::{Key, Modifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub cmd: bool,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub key: Key,
}

impl Chord {
    pub fn parse(s: &str) -> Result<Chord, String> {
        let mut c = Chord { cmd: false, shift: false, alt: false, ctrl: false, key: Key::A };
        let mut key = None;
        for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
            match part.as_str() {
                "cmd" | "command" | "super" => c.cmd = true,
                "shift" => c.shift = true,
                "alt" | "opt" | "option" | "meta" => c.alt = true,
                "ctrl" | "control" => c.ctrl = true,
                k if key.is_none() => key = Some(parse_key(k).ok_or_else(|| format!("unknown key {k:?} in {s:?}"))?),
                _ => return Err(format!("more than one key in {s:?}")),
            }
        }
        c.key = key.ok_or_else(|| format!("no key in {s:?}"))?;
        if !(c.cmd || c.alt || c.ctrl) {
            return Err(format!("{s:?} needs cmd, alt or ctrl (plain keys belong to the terminal/viewer)"));
        }
        Ok(c)
    }

    /// Exact modifier match (egui's logical matching would let ⌘Y fire ⌘⇧Y).
    pub fn matches(&self, key: Key, m: Modifiers) -> bool {
        key == self.key && (m.mac_cmd || m.command) == self.cmd && m.shift == self.shift && m.alt == self.alt && m.ctrl == self.ctrl
    }

    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push('⌃');
        }
        if self.alt {
            s.push('⌥');
        }
        if self.shift {
            s.push('⇧');
        }
        if self.cmd {
            s.push('⌘');
        }
        s.push_str(self.key.name());
        s
    }
}

/// Keystrokes for `kitchenctl debug keys`: plain characters are typed, and
/// `<esc>`, `<cr>`, `<c-o>`, `<cmd-shift-p>`, `<alt-h>` press keys/chords.
pub fn parse_sequence(s: &str) -> Result<Vec<egui::Event>, String> {
    let mut out = Vec::new();
    let press = |out: &mut Vec<egui::Event>, key: Key, modifiers: Modifiers| {
        for pressed in [true, false] {
            out.push(egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers });
        }
    };
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == '<' {
            if let Some(end) = rest.find('>').filter(|&e| e > 1) {
                let token = rest[1..end].to_ascii_lowercase();
                rest = &rest[end + 1..];
                let mut m = Modifiers::default();
                let mut parts: Vec<&str> = token.split('-').collect();
                let name = if token.ends_with("--") { "-" } else { parts.pop().unwrap_or("") };
                for p in parts.iter().filter(|p| !p.is_empty()) {
                    match *p {
                        "c" | "ctrl" => m.ctrl = true,
                        "cmd" | "d" => {
                            m.mac_cmd = true;
                            m.command = true;
                        }
                        "s" | "shift" => m.shift = true,
                        "a" | "alt" | "m" | "opt" => m.alt = true,
                        _ => return Err(format!("unknown modifier {p:?} in <{token}>")),
                    }
                }
                let key = parse_key(name).ok_or_else(|| format!("unknown key {name:?} in <{token}>"))?;
                press(&mut out, key, m);
                continue;
            }
        }
        rest = &rest[c.len_utf8()..];
        if let Some(key) = parse_key(&c.to_string()) {
            let shift = c.is_ascii_uppercase();
            out.push(egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers { shift, ..Default::default() } });
            out.push(egui::Event::Text(c.to_string()));
            out.push(egui::Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers { shift, ..Default::default() } });
        } else {
            out.push(egui::Event::Text(c.to_string()));
        }
    }
    Ok(out)
}

fn parse_key(k: &str) -> Option<Key> {
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

    #[test]
    fn parses_chords() {
        let c = Chord::parse("cmd+shift+y").unwrap();
        assert!(c.cmd && c.shift && !c.alt && c.key == Key::Y);
        assert!(c.matches(Key::Y, Modifiers { mac_cmd: true, command: true, shift: true, ..Default::default() }));
        assert!(!c.matches(Key::Y, Modifiers { mac_cmd: true, command: true, ..Default::default() }));
        assert_eq!(Chord::parse("alt+5").unwrap().key, Key::Num5);
        assert_eq!(Chord::parse("ctrl+enter").unwrap().key, Key::Enter);
        assert!(Chord::parse("y").is_err());
        assert_eq!(Chord::parse("cmd+c").unwrap().key, Key::C, "letters are keys, not modifier aliases");
        assert_eq!(Chord::parse("alt+s").unwrap().key, Key::S);
        assert!(Chord::parse("cmd+nope").is_err());
    }

    #[test]
    fn key_sequences() {
        let ev = parse_sequence("gd<c-o><cmd-shift-p>é").unwrap();
        let texts: String = ev.iter().filter_map(|e| if let egui::Event::Text(t) = e { Some(t.as_str()) } else { None }).collect();
        assert_eq!(texts, "gdé");
        let chords: Vec<(Key, bool, bool, bool)> = ev
            .iter()
            .filter_map(|e| match e {
                egui::Event::Key { key, pressed: true, modifiers: m, .. } if m.ctrl || m.command => Some((*key, m.ctrl, m.command, m.shift)),
                _ => None,
            })
            .collect();
        assert_eq!(chords, vec![(Key::O, true, false, false), (Key::P, false, true, true)]);
        assert!(parse_sequence("<x-y>").is_err());
    }
}
