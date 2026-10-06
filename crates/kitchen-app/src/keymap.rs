//! The kitchen's keyboard shortcuts. App-level actions are Cmd+<key>, plus
//! Option+H/L for tiling-style pane focus; the terminal and the vim-style
//! viewer never see these chords.
//!
//! Chords act on the focused pane only (see [`Action::scope`]): tab keys
//! (⌘T ⌘W) work in the terminal, panel keys (⌘M ⌘O ⌘Enter) in a focused
//! side panel, ⌘P and ⌘⇧H/J/K/L in a focused viewer; elsewhere they are
//! swallowed. Tab and workspace switching (⌘H/L ⌘1-9 ⌘J/K) work everywhere. The rest is global; ⌘D toggles diff in a focused side panel
//! and the workspace dashboard elsewhere, and ⌘. hides the other pane: the
//! side panel from the terminal, the terminal from a focused side panel.
//!
//! | keys        | action                                        |
//! |-------------|-----------------------------------------------|
//! | ⌘N          | new workspace                                 |
//! | ⌘J / ⌘K     | next / previous workspace                     |
//! | ⌘1 … ⌘9     | go to tab N of the current workspace          |
//! | ⌘H / ⌘L     | previous / next tab                           |
//! | ⌘⇧H / ⌘⇧L   | focused viewer: file list ↔ code              |
//! | ⌘⇧J / ⌘⇧K   | focused viewer: next / previous changed file  |
//! | ⌘T          | new tab: a name or a PR link — own worktree   |
//! | ⌘-click     | open the path / URL under the pointer (terminal) |
//! | ⌘R          | rename the current tab                        |
//! | ⌘W          | close tab                                     |
//! | ⌘P          | fuzzy file picker (focused viewer only)       |
//! | ⌘B          | show / hide the viewer's file list            |
//! | ⌘⇧I         | plan reviewer                                 |
//! | ⌘⇧R         | rename the current workspace                  |
//! | ⌘⇧P         | command palette (plugin commands)             |
//! | ⌥H / ⌥L     | focus the left (terminal) / right (side) pane |
//! | ⌘`          | switch focus terminal ↔ side panel            |
//! | ⌘M          | comment on the cursor line / selection        |
//! | ⌘Enter      | submit comment (plan: send comments to agent) |
//! | ⌘D          | dashboard (focused side panel: diff / plain)  |
//! | ⌘O          | toggle outline (collapsed) mode               |
//! | ⌘.          | open the file viewer / close the side panel (focused side panel: hide / show the terminal) |
//! | ⌘Q          | quit                                          |
//! | ⌘C / ⌘V     | copy / paste (handled by the focused widget)  |

use egui::{Key, KeyboardShortcut, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    NewWorkspace,
    NextWorkspace,
    PrevWorkspace,
    GoToTab(usize),
    PrevTab,
    NextTab,
    NewTab,
    CloseTab,
    FilePicker,
    /// Viewer: ⌘⇧H / ⌘⇧L move between its file list and code.
    FocusFileList,
    FocusCode,
    /// Viewer: ⌘⇧J / ⌘⇧K step through the changed files.
    NextFile,
    PrevFile,
    ToggleFiles,
    OpenPlan,
    RenameTab,
    RenameWorkspace,
    ToggleFocus,
    /// Tiling-style pane focus: terminal is the left pane, side panel the right.
    FocusLeft,
    FocusRight,
    Comment,
    Submit,
    /// ⌘D: diff / plain in a focused side panel, else the workspace dashboard.
    DiffOrDashboard,
    ToggleOutline,
    ToggleViewer,
    Quit,
    Palette,
    /// A plugin chord: index into the registry snapshot passed to [`collect`].
    Plugin(usize),
}

/// Which pane an action belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Global,
    /// Tabs: only while the terminal pane is focused.
    Terminal,
    /// Panel keys: only while a side panel (viewer, plan) is focused.
    Side,
}

impl Action {
    pub fn scope(self) -> Scope {
        match self {
            Action::NewTab | Action::CloseTab => Scope::Terminal,
            Action::Comment | Action::Submit | Action::ToggleOutline | Action::FilePicker | Action::FocusFileList | Action::FocusCode | Action::NextFile | Action::PrevFile => Scope::Side,
            _ => Scope::Global,
        }
    }

    /// May this chord act with the given pane focused? `viewer`: the focused
    /// side panel is the file viewer.
    pub fn allowed(self, side_focused: bool, viewer: bool) -> bool {
        match self.scope() {
            Scope::Global => true,
            Scope::Terminal => !side_focused,
            Scope::Side => side_focused && (viewer || !self.viewer_only()),
        }
    }

    /// Side-panel keys only the file viewer has.
    fn viewer_only(self) -> bool {
        matches!(self, Action::FilePicker | Action::FocusFileList | Action::FocusCode | Action::NextFile | Action::PrevFile)
    }
}

/// Chords that need Shift, checked before the plain ⌘ table (egui matches
/// ⌘I against ⌘⇧I too, so the shifted chord must be consumed first).
pub const SHIFT_BINDINGS: &[(Key, Action)] = &[
    (Key::I, Action::OpenPlan),
    (Key::R, Action::RenameWorkspace),
    (Key::P, Action::Palette),
    (Key::H, Action::FocusFileList),
    (Key::L, Action::FocusCode),
    (Key::J, Action::NextFile),
    (Key::K, Action::PrevFile),
];

/// Option chords (no Cmd).
pub const ALT_BINDINGS: &[(Key, Action)] = &[(Key::H, Action::FocusLeft), (Key::L, Action::FocusRight)];

pub const BINDINGS: &[(Key, Action)] = &[
    (Key::N, Action::NewWorkspace),
    (Key::J, Action::NextWorkspace),
    (Key::K, Action::PrevWorkspace),
    (Key::T, Action::NewTab),
    (Key::R, Action::RenameTab),
    (Key::W, Action::CloseTab),
    (Key::P, Action::FilePicker),
    (Key::B, Action::ToggleFiles),
    (Key::H, Action::PrevTab),
    (Key::L, Action::NextTab),
    (Key::Backtick, Action::ToggleFocus),
    (Key::M, Action::Comment),
    (Key::Enter, Action::Submit),
    (Key::D, Action::DiffOrDashboard),
    (Key::O, Action::ToggleOutline),
    (Key::Period, Action::ToggleViewer),
    (Key::Q, Action::Quit),
];

const DIGITS: [Key; 9] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];

/// Keys the focused widget handles itself (copy, paste, …).
const RESERVED: &[Key] = &[Key::C, Key::V, Key::X, Key::A, Key::Z];

/// The built-in binding a plugin chord would collide with, if any.
pub fn builtin_conflict(c: &crate::plugins::keys::Chord) -> Option<String> {
    let only = |cmd: bool, shift: bool, alt: bool| c.cmd == cmd && c.shift == shift && c.alt == alt && !c.ctrl;
    if only(false, false, true) && ALT_BINDINGS.iter().any(|(k, _)| *k == c.key) {
        return Some(format!("⌥{} pane focus", c.key.name()));
    }
    if only(true, true, false) && SHIFT_BINDINGS.iter().any(|(k, _)| *k == c.key) {
        return Some(format!("⌘⇧{}", c.key.name()));
    }
    if only(true, false, false) && (BINDINGS.iter().any(|(k, _)| *k == c.key) || DIGITS.contains(&c.key) || RESERVED.contains(&c.key)) {
        return Some(format!("⌘{}", c.key.name()));
    }
    None
}

/// Plugin chords that apply in the current context, as (registry index, chord).
pub type PluginChords = Vec<(usize, crate::plugins::keys::Chord)>;

/// Consume every bound chord pressed this frame and return its action.
/// Order: built-in ⌥ → built-in ⌘⇧ → plugin chords (exact modifiers) →
/// built-in ⌘ → digits.
pub fn collect(ctx: &egui::Context, plugin: &PluginChords) -> Vec<Action> {
    let mut out = Vec::new();
    ctx.input_mut(|i| {
        for (key, action) in ALT_BINDINGS {
            if consume_alt(&mut i.events, *key) {
                out.push(*action);
            }
        }
        for (key, action) in SHIFT_BINDINGS {
            if i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, *key)) {
                out.push(*action);
            }
        }
        for (idx, chord) in plugin {
            let pos = i.events.iter().position(|e| matches!(e, egui::Event::Key { key, pressed: true, modifiers, .. } if chord.matches(*key, *modifiers)));
            if let Some(pos) = pos {
                i.events.remove(pos);
                // Option chords also type a composed character (⌥Y → ¥).
                if chord.alt && matches!(i.events.get(pos), Some(egui::Event::Text(_))) {
                    i.events.remove(pos);
                }
                out.push(Action::Plugin(*idx));
            }
        }
        for (key, action) in BINDINGS {
            if i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, *key)) {
                out.push(*action);
            }
        }
        for (n, key) in DIGITS.iter().enumerate() {
            if i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, *key)) {
                out.push(Action::GoToTab(n + 1));
            }
        }
    });
    out
}

/// Remove an Option+`key` press, along with the composed character macOS
/// sends right after it (⌥H types "˙"), so neither reaches the terminal.
fn consume_alt(events: &mut Vec<egui::Event>, key: Key) -> bool {
    let is_chord = |e: &egui::Event| {
        matches!(e, egui::Event::Key { key: k, pressed: true, modifiers: m, .. }
            if *k == key && m.alt && !m.ctrl && !m.command && !m.mac_cmd && !m.shift)
    };
    let mut found = false;
    while let Some(idx) = events.iter().position(is_chord) {
        found = true;
        events.remove(idx);
        if matches!(events.get(idx), Some(egui::Event::Text(_))) {
            events.remove(idx);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_act_on_the_focused_pane() {
        // Terminal focused: tab keys yes, panel keys no.
        assert!(Action::NewTab.allowed(false, false));
        assert!(Action::GoToTab(2).allowed(false, false));
        assert!(!Action::Comment.allowed(false, false));
        assert!(!Action::FilePicker.allowed(false, false));
        assert!(!Action::FocusFileList.allowed(false, false));
        // Viewer focused: panel keys yes, tab create / close keys no.
        assert!(!Action::NewTab.allowed(true, true));
        assert!(!Action::CloseTab.allowed(true, true));
        assert!(Action::ToggleOutline.allowed(true, true));
        // Viewer-only keys are swallowed in the plan.
        for a in [Action::FilePicker, Action::FocusFileList, Action::FocusCode, Action::NextFile, Action::PrevFile] {
            assert!(a.allowed(true, true) && !a.allowed(true, false), "{a:?}");
        }
        // Global keys work everywhere.
        for (side, viewer) in [(false, false), (true, true), (true, false)] {
            for a in [Action::DiffOrDashboard, Action::NewWorkspace, Action::NextWorkspace, Action::PrevWorkspace, Action::PrevTab, Action::NextTab, Action::GoToTab(3), Action::ToggleFiles, Action::OpenPlan, Action::RenameTab, Action::RenameWorkspace, Action::Palette, Action::FocusLeft, Action::FocusRight, Action::ToggleFocus, Action::ToggleViewer, Action::Quit] {
                assert!(a.allowed(side, viewer), "{a:?}");
            }
        }
    }

    fn press_events(events: Vec<egui::Event>) -> (Vec<Action>, Vec<egui::Event>) {
        let ctx = egui::Context::default();
        let mut out = (Vec::new(), Vec::new());
        let mut output = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
            out.0 = collect(ui.ctx(), &Vec::new());
            out.1 = ui.input(|i| i.events.clone());
        });
        output.textures_delta.clear();
        out
    }

    fn press(key: Key, modifiers: Modifiers) -> Vec<Action> {
        let event = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        press_events(vec![egui::Event::ModifiersChanged(modifiers), event]).0
    }

    #[test]
    fn plugin_chords_match_exactly_and_skip_builtins() {
        use crate::plugins::keys::Chord;
        let y = Chord::parse("cmd+shift+y").unwrap();
        let chords = vec![(7, y)];
        let run = |key, modifiers| {
            let ctx = egui::Context::default();
            let event = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
            let mut out = Vec::new();
            let mut o = ctx.run_ui(egui::RawInput { events: vec![egui::Event::ModifiersChanged(modifiers), event], ..Default::default() }, |ui| {
                out = collect(ui.ctx(), &chords);
            });
            o.textures_delta.clear();
            out
        };
        assert_eq!(run(Key::Y, Modifiers::COMMAND | Modifiers::SHIFT), vec![Action::Plugin(7)]);
        assert!(run(Key::Y, Modifiers::COMMAND).is_empty(), "⌘Y is not ⌘⇧Y");
        assert!(builtin_conflict(&Chord::parse("cmd+h").unwrap()).is_some());
        assert!(builtin_conflict(&Chord::parse("cmd+shift+p").unwrap()).is_some());
        assert!(builtin_conflict(&Chord::parse("alt+l").unwrap()).is_some());
        assert!(builtin_conflict(&Chord::parse("cmd+c").unwrap()).is_some());
        assert!(builtin_conflict(&Chord::parse("cmd+shift+h").unwrap()).is_some());
        assert!(builtin_conflict(&Chord::parse("cmd+shift+u").unwrap()).is_none());
    }

    #[test]
    fn option_focus_chords_swallow_composed_text() {
        let key = egui::Event::Key { key: Key::H, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::ALT };
        let (actions, left) = press_events(vec![egui::Event::ModifiersChanged(Modifiers::ALT), key, egui::Event::Text("˙".into())]);
        assert_eq!(actions, vec![Action::FocusLeft]);
        assert!(!left.iter().any(|e| matches!(e, egui::Event::Text(_) | egui::Event::Key { .. })), "{left:?}");
        assert_eq!(press(Key::L, Modifiers::ALT), vec![Action::FocusRight]);
        assert!(press(Key::B, Modifiers::ALT).is_empty(), "other Option keys stay with the terminal");
    }

    #[test]
    fn shifted_chords_do_not_trigger_plain_ones() {
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        assert_eq!(press(Key::I, cmd_shift), vec![Action::OpenPlan]);
        assert_eq!(press(Key::R, cmd_shift), vec![Action::RenameWorkspace]);
        assert_eq!(press(Key::H, Modifiers::COMMAND), vec![Action::PrevTab]);
        assert_eq!(press(Key::H, cmd_shift), vec![Action::FocusFileList]);
        assert_eq!(press(Key::L, cmd_shift), vec![Action::FocusCode]);
        assert_eq!(press(Key::J, cmd_shift), vec![Action::NextFile]);
        assert_eq!(press(Key::K, cmd_shift), vec![Action::PrevFile]);
        assert_eq!(press(Key::J, Modifiers::COMMAND), vec![Action::NextWorkspace]);
        assert_eq!(press(Key::L, Modifiers::COMMAND), vec![Action::NextTab]);
        assert_eq!(press(Key::R, Modifiers::COMMAND), vec![Action::RenameTab]);
        assert_eq!(press(Key::T, Modifiers::COMMAND), vec![Action::NewTab]);
        assert!(press(Key::E, Modifiers::COMMAND).is_empty(), "⌘E is no longer bound");
        assert_eq!(press(Key::P, cmd_shift), vec![Action::Palette]);
        assert_eq!(press(Key::P, Modifiers::COMMAND), vec![Action::FilePicker]);
        assert_eq!(press(Key::Period, Modifiers::COMMAND), vec![Action::ToggleViewer]);
        assert_eq!(press(Key::Num3, Modifiers::COMMAND), vec![Action::GoToTab(3)]);
        assert!(press(Key::H, Modifiers::NONE).is_empty(), "plain keys belong to the terminal/viewer");
    }
}
