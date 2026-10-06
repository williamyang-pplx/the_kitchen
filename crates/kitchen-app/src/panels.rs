//! The contract between the window and its side panels (viewer, plan
//! reviewer). The window owns focus and the Cmd keymap; panels read
//! plain (vim) keys themselves while `PanelCtx::focused` is true.

use std::sync::Arc;

use crate::engine::Engine;

/// Cmd shortcuts forwarded to the focused panel.
#[derive(Debug, Clone)]
pub enum PanelCmd {
    /// ⌘M: open a comment field on the cursor line / visual selection.
    Comment,
    /// ⌘Enter: submit the open comment field (or the panel's primary action).
    Submit,
    /// ⌘D
    ToggleDiff,
    /// ⌘O
    ToggleOutline,
    /// ⌘P while the viewer is focused.
    FilePicker,
    /// Re-read files / refresh remote state.
    Reload,
    /// ⌘⇧H / ⌘⇧L while the viewer is focused: its file list / its code.
    FocusList,
    FocusCode,
    /// ⌘B: show / hide the viewer's file list.
    ToggleFiles,
    /// ⌘⇧J / ⌘⇧K while the viewer is focused: next / previous changed file.
    NextFile,
    PrevFile,
    /// Palette: pick among the current file's plugin diagnostics.
    Diagnostics,
    /// Open a file (from `kitchenctl viewer open`), or browse a folder if
    /// `path` ends in `/`.
    Open { path: Option<String>, line: Option<usize>, diff: Option<bool> },
}

pub struct PanelCtx<'a> {
    pub engine: &'a Arc<Engine>,
    /// Keyboard input belongs to this panel.
    pub focused: bool,
    /// Set by the panel when the user clicked into it (moves focus there).
    pub clicked: bool,
}
