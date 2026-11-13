//! Key sequences for `kitchenctl debug keys` (`gd<c-o><cmd-shift-p>`) → egui events.

use egui::{Key, Modifiers};

use crate::keybindings::notation::parse_key;

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

#[cfg(test)]
mod tests {
    use super::*;

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
