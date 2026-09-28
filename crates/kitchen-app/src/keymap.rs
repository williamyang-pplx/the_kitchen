//! The kitchen's keyboard shortcuts. Every app-level action is Cmd+<key>;
//! the terminal and the vim-style viewer never see these chords.
//!
//! | keys        | action                                        |
//! |-------------|-----------------------------------------------|
//! | ⌘N          | new workspace                                 |
//! | ⌘J / ⌘K     | next / previous workspace                     |
//! | ⌘1 … ⌘9     | go to tab N of the current workspace          |
//! | ⌘H / ⌘L     | previous / next tab in the workspace          |
//! | ⌘E          | new edit (read/write) tab — own worktree      |
//! | ⌘R          | new read-only tab — shared read-only worktree |
//! | ⌘W          | close tab                                     |
//! | ⌘P          | file viewer (again: file picker)              |
//! | ⌘⇧R         | plan reviewer                                 |
//! | ⌘I          | agent-review inbox                            |
//! | ⌘⇧H / ⌘⇧L   | focus the left (terminal) / right (side) pane |
//! | ⌘`          | switch focus terminal ↔ side panel            |
//! | ⌘M          | comment on the cursor line / selection        |
//! | ⌘Enter      | submit comment (plan: send comments to agent) |
//! | ⌘D          | toggle diff / plain file                      |
//! | ⌘O          | toggle outline (collapsed) mode               |
//! | ⌘.          | close the side panel                          |
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
    NewEditTab,
    NewReadonlyTab,
    CloseTab,
    OpenViewer,
    OpenPlan,
    OpenInbox,
    ToggleFocus,
    /// Tiling-style pane focus: terminal is the left pane, side panel the right.
    FocusLeft,
    FocusRight,
    Comment,
    Submit,
    ToggleDiff,
    ToggleOutline,
    CloseSide,
    Quit,
}

/// Chords that need Shift, checked before the plain ⌘ table (egui matches
/// ⌘R against ⌘⇧R too, so the shifted chord must be consumed first).
pub const SHIFT_BINDINGS: &[(Key, Action)] =
    &[(Key::R, Action::OpenPlan), (Key::H, Action::FocusLeft), (Key::L, Action::FocusRight)];

pub const BINDINGS: &[(Key, Action)] = &[
    (Key::N, Action::NewWorkspace),
    (Key::J, Action::NextWorkspace),
    (Key::K, Action::PrevWorkspace),
    (Key::E, Action::NewEditTab),
    (Key::R, Action::NewReadonlyTab),
    (Key::W, Action::CloseTab),
    (Key::P, Action::OpenViewer),
    (Key::H, Action::PrevTab),
    (Key::L, Action::NextTab),
    (Key::I, Action::OpenInbox),
    (Key::Backtick, Action::ToggleFocus),
    (Key::M, Action::Comment),
    (Key::Enter, Action::Submit),
    (Key::D, Action::ToggleDiff),
    (Key::O, Action::ToggleOutline),
    (Key::Period, Action::CloseSide),
    (Key::Q, Action::Quit),
];

const DIGITS: [Key; 9] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];

/// Consume every Cmd chord pressed this frame and return its action.
pub fn collect(ctx: &egui::Context) -> Vec<Action> {
    let mut out = Vec::new();
    ctx.input_mut(|i| {
        for (key, action) in SHIFT_BINDINGS {
            if i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, *key)) {
                out.push(*action);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key, modifiers: Modifiers) -> Vec<Action> {
        let ctx = egui::Context::default();
        let event = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        let mut out = Vec::new();
        let _ = ctx.run_ui(egui::RawInput { events: vec![egui::Event::ModifiersChanged(modifiers), event], ..Default::default() }, |ui| {
            out = collect(ui.ctx());
        });
        out
    }

    #[test]
    fn shifted_chords_do_not_trigger_plain_ones() {
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        assert_eq!(press(Key::H, cmd_shift), vec![Action::FocusLeft]);
        assert_eq!(press(Key::L, cmd_shift), vec![Action::FocusRight]);
        assert_eq!(press(Key::R, cmd_shift), vec![Action::OpenPlan]);
        assert_eq!(press(Key::H, Modifiers::COMMAND), vec![Action::PrevTab]);
        assert_eq!(press(Key::L, Modifiers::COMMAND), vec![Action::NextTab]);
        assert_eq!(press(Key::R, Modifiers::COMMAND), vec![Action::NewReadonlyTab]);
        assert_eq!(press(Key::Num3, Modifiers::COMMAND), vec![Action::GoToTab(3)]);
        assert!(press(Key::H, Modifiers::NONE).is_empty(), "plain keys belong to the terminal/viewer");
    }
}
