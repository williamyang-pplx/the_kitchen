//! The kitchen's keyboard shortcuts. Every app-level action is Cmd+<key>;
//! the terminal and the vim-style viewer never see these chords.
//!
//! | keys        | action                                        |
//! |-------------|-----------------------------------------------|
//! | ⌘N          | new workspace                                 |
//! | ⌘J / ⌘K     | next / previous workspace                     |
//! | ⌘1 … ⌘9     | go to tab N of the current workspace          |
//! | ⌘E          | new edit (read/write) tab — own worktree      |
//! | ⌘R          | new read-only tab — shared read-only worktree |
//! | ⌘W          | close tab                                     |
//! | ⌘P          | file viewer (again: file picker)              |
//! | ⌘L          | plan reviewer                                 |
//! | ⌘I          | agent-review inbox                            |
//! | ⌘`          | switch focus terminal ↔ side panel            |
//! | ⌘M          | comment on the cursor line / selection        |
//! | ⌘Enter      | submit comment (plan: send comments to agent) |
//! | ⌘D          | toggle diff / plain file                      |
//! | ⌘O          | toggle outline (collapsed) mode               |
//! | ⌘.          | close the side panel                          |
//! | ⌘C / ⌘V     | copy / paste (handled by the focused widget)  |

use egui::{Key, KeyboardShortcut, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    NewWorkspace,
    NextWorkspace,
    PrevWorkspace,
    GoToTab(usize),
    NewEditTab,
    NewReadonlyTab,
    CloseTab,
    OpenViewer,
    OpenPlan,
    OpenInbox,
    ToggleFocus,
    Comment,
    Submit,
    ToggleDiff,
    ToggleOutline,
    CloseSide,
}

pub const BINDINGS: &[(Key, Action)] = &[
    (Key::N, Action::NewWorkspace),
    (Key::J, Action::NextWorkspace),
    (Key::K, Action::PrevWorkspace),
    (Key::E, Action::NewEditTab),
    (Key::R, Action::NewReadonlyTab),
    (Key::W, Action::CloseTab),
    (Key::P, Action::OpenViewer),
    (Key::L, Action::OpenPlan),
    (Key::I, Action::OpenInbox),
    (Key::Backtick, Action::ToggleFocus),
    (Key::M, Action::Comment),
    (Key::Enter, Action::Submit),
    (Key::D, Action::ToggleDiff),
    (Key::O, Action::ToggleOutline),
    (Key::Period, Action::CloseSide),
];

const DIGITS: [Key; 9] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];

/// Consume every Cmd chord pressed this frame and return its action.
pub fn collect(ctx: &egui::Context) -> Vec<Action> {
    let mut out = Vec::new();
    ctx.input_mut(|i| {
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
