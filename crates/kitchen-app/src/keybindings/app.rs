//! App-level actions: the `[global]`, `[terminal]`, `[panel]` and `[viewer]`
//! chords of keybindings.toml, and the `[edit]` keys. Which pane an action
//! works in is its place; the keys are in the file. A tab shows either its
//! terminal or its side panel (viewer / plan); ⌘. switches between them.

use egui::{Event, Key, Modifiers};

use super::notation::Mods;
use super::{Cmd, EditCmd, KeyFocus, Keymap, Press};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    NewWorkspace,
    NextWorkspace,
    PrevWorkspace,
    GoToTab(usize),
    PrevTab,
    NextTab,
    NewTab,
    /// A tab in the workspace's main checkout, no worktree or branch.
    NewMainCheckoutTab,
    CloseTab,
    /// The selected workspace's most recently closed tab, back.
    ReopenTab,
    FilePicker,
    /// Viewer: move between its file list and code.
    FocusFileList,
    FocusCode,
    /// Viewer: step through the changed files.
    NextFile,
    PrevFile,
    ToggleFiles,
    OpenPlan,
    RenameTab,
    RenameWorkspace,
    Comment,
    /// Viewer / plan: ask the agent about the cursor line / selection.
    Ask,
    Submit,
    ToggleOutline,
    ToggleViewer,
    Quit,
    Palette,
    /// The list of every shortcut.
    Shortcuts,
    /// The viewer's PR menu; from the terminal, the viewer opens first.
    ViewMenu,
    /// Outside a panel: the workspace dashboard.
    Dashboard,
    /// In a panel: diff / plain.
    ToggleDiff,
    /// In a panel: the keyboard back to the terminal.
    FocusTerminal,
}

/// Consume this frame's chords that resolve to app or plugin commands from
/// `focus` outward. Exact modifiers, so ⌘R never fires ⌘⇧R. A ⌘ chord bound
/// to an app action somewhere else is swallowed; other chords (a vim `<C-d>`)
/// are left for the focused widget.
pub fn collect(ctx: &egui::Context, km: &Keymap, focus: KeyFocus, filetype: Option<&str>) -> Vec<Cmd> {
    let mut out = Vec::new();
    ctx.input_mut(|i| {
        let mut j = 0;
        while j < i.events.len() {
            let Event::Key { key, pressed: true, modifiers, .. } = i.events[j] else {
                j += 1;
                continue;
            };
            let Some(press @ Press::Key { mods, .. }) = Press::of_key(key, modifiers).filter(|p| matches!(p, Press::Key { mods, .. } if mods.any_held())) else {
                j += 1;
                continue;
            };
            let take = match km.lookup(focus, filetype, &press) {
                Some(c @ (Cmd::App(_) | Cmd::Plugin(_))) => {
                    out.push(c.clone());
                    true
                }
                Some(_) => false,
                None => km.bound_anywhere(&press),
            };
            if !take {
                j += 1;
                continue;
            }
            i.events.remove(j);
            // Option chords also type a composed character (⌥Y → ¥).
            if mods.alt && matches!(i.events.get(j), Some(Event::Text(_))) {
                i.events.remove(j);
            }
            // ⌘⇧V comes with the paste it stands for (see `edit_keys`).
            let stands_for = matches!((i.events.get(j), key), (Some(Event::Paste(_)), Key::V) | (Some(Event::Copy), Key::C) | (Some(Event::Cut), Key::X));
            if stands_for {
                i.events.remove(j);
            }
        }
    });
    out
}

/// What the default [edit] keys do without help: winit's Copy / Cut / Paste
/// for ⌘C / ⌘X / ⌘V, and the keys TextEdit reads.
fn native(press: &Press) -> Option<EditCmd> {
    Keymap::defaults().edit_cmd(press)
}

/// The events an [edit] command stands for.
fn edit_events(cmd: EditCmd) -> Vec<Event> {
    let key = |key, modifiers| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
    match cmd {
        EditCmd::Copy => vec![Event::Copy],
        EditCmd::Cut => vec![Event::Cut],
        EditCmd::Paste => crate::attach::clipboard_text().map(Event::Paste).into_iter().collect(),
        EditCmd::SelectAll => vec![key(Key::A, Modifiers::COMMAND)],
        EditCmd::Undo => vec![key(Key::Z, Modifiers::COMMAND)],
        EditCmd::Redo => vec![key(Key::Z, Modifiers::COMMAND | Modifiers::SHIFT)],
    }
}

/// `[edit]`: rewrite raw input so the bound keys copy / cut / paste / select
/// all / undo / redo and the defaults you moved do nothing. Runs first in
/// `raw_input_hook`. `modifiers`: those held before these events.
pub fn edit_keys(raw: &mut egui::RawInput, km: &Keymap, mut modifiers: Modifiers) {
    let events = std::mem::take(&mut raw.events);
    let mut out = Vec::with_capacity(events.len());
    let mut skip_text = false;
    for ev in events {
        if std::mem::take(&mut skip_text) && matches!(ev, Event::Text(_)) {
            continue;
        }
        match &ev {
            Event::ModifiersChanged(m) => modifiers = *m,
            // winit sends these for ⌘C / ⌘X / ⌘V with any other modifiers, and no key.
            Event::Copy | Event::Cut | Event::Paste(_) => {
                let (key, cmd) = match ev {
                    Event::Copy => (Key::C, EditCmd::Copy),
                    Event::Cut => (Key::X, EditCmd::Cut),
                    _ => (Key::V, EditCmd::Paste),
                };
                let press = Press::Key { key, mods: Mods::of(modifiers) };
                match km.edit_cmd(&press) {
                    Some(c) if c == cmd => out.push(ev),
                    Some(other) => out.extend(edit_events(other)),
                    // A default you moved elsewhere.
                    None if native(&press) == Some(cmd) && !km.binds(&press) => {}
                    // Unbound or bound to a chord (a plugin's ⌘⇧V): the key in
                    // front; `collect` takes both if the chord fires, else it
                    // still pastes.
                    None => {
                        out.push(Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers });
                        out.push(ev);
                    }
                }
                continue;
            }
            Event::Key { key, pressed: true, modifiers: m, .. } => {
                if let Some(press) = Press::of_key(*key, *m) {
                    match km.edit_cmd(&press) {
                        Some(cmd) if native(&press) == Some(cmd) => {}
                        Some(cmd) => {
                            out.extend(edit_events(cmd));
                            skip_text = true;
                            continue;
                        }
                        None if native(&press).is_some() && !km.binds(&press) => {
                            skip_text = true;
                            continue;
                        }
                        None => {}
                    }
                }
            }
            _ => {}
        }
        out.push(ev);
    }
    raw.events = out;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::Binding;
    use crate::plugins::{ChordEntry, Registry, When};

    fn run(km: &Keymap, focus: KeyFocus, raw: egui::RawInput) -> (Vec<Cmd>, Vec<Event>) {
        let ctx = egui::Context::default();
        let mut out = (Vec::new(), Vec::new());
        let mut output = ctx.run_ui(raw, |ui| {
            out.0 = collect(ui.ctx(), km, focus, None);
            out.1 = ui.input(|i| i.events.clone());
        });
        output.textures_delta.clear();
        out
    }

    fn key_events(key: Key, modifiers: Modifiers) -> egui::RawInput {
        let event = Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        egui::RawInput { events: vec![Event::ModifiersChanged(modifiers), event], ..Default::default() }
    }

    fn press_in(focus: KeyFocus, key: Key, modifiers: Modifiers) -> Vec<Cmd> {
        run(&Keymap::defaults(), focus, key_events(key, modifiers)).0
    }

    fn press(key: Key, modifiers: Modifiers) -> Vec<Cmd> {
        press_in(KeyFocus::Viewer, key, modifiers)
    }

    fn app(a: Action) -> Vec<Cmd> {
        vec![Cmd::App(a)]
    }

    #[test]
    fn chords_act_on_the_focused_pane() {
        let cmd = Modifiers::COMMAND;
        // Terminal: tab keys yes, panel keys no (swallowed).
        assert_eq!(press_in(KeyFocus::Terminal, Key::T, cmd), app(Action::NewTab));
        assert_eq!(press_in(KeyFocus::Terminal, Key::Num2, cmd), app(Action::GoToTab(2)));
        let (cmds, left) = run(&Keymap::defaults(), KeyFocus::Terminal, key_events(Key::M, cmd));
        assert!(cmds.is_empty() && !left.iter().any(|e| matches!(e, Event::Key { .. })), "⌘M in the terminal is swallowed");
        assert!(press_in(KeyFocus::Terminal, Key::P, cmd).is_empty());
        assert!(press_in(KeyFocus::Terminal, Key::H, cmd | Modifiers::SHIFT).is_empty());
        // Viewer: panel keys yes, tab create / close keys swallowed.
        let (cmds, left) = run(&Keymap::defaults(), KeyFocus::Viewer, key_events(Key::T, cmd));
        assert!(cmds.is_empty() && !left.iter().any(|e| matches!(e, Event::Key { .. })), "⌘T in the viewer is swallowed");
        assert!(press(Key::W, cmd).is_empty());
        assert_eq!(press(Key::O, cmd), app(Action::ToggleOutline));
        assert_eq!(press_in(KeyFocus::FileList, Key::P, cmd), app(Action::FilePicker));
        // Viewer-only keys do nothing in the plan; the plan takes questions.
        for k in [Key::H, Key::L, Key::J, Key::K] {
            assert!(press_in(KeyFocus::Plan, k, cmd | Modifiers::SHIFT).is_empty(), "{k:?}");
        }
        assert!(press_in(KeyFocus::Plan, Key::P, cmd).is_empty(), "⌘P in the plan does nothing");
        assert_eq!(press_in(KeyFocus::Plan, Key::A, cmd | Modifiers::SHIFT), app(Action::Ask));
        // ⌘D: the diff in a panel, the dashboard elsewhere.
        assert_eq!(press(Key::D, cmd), app(Action::ToggleDiff));
        assert_eq!(press_in(KeyFocus::Terminal, Key::D, cmd), app(Action::Dashboard));
        assert_eq!(press_in(KeyFocus::Plan, Key::Escape, cmd), app(Action::FocusTerminal));
        // Global keys work everywhere but in dialogs.
        for focus in [KeyFocus::Terminal, KeyFocus::Viewer, KeyFocus::FileList, KeyFocus::Plan, KeyFocus::PlanInput, KeyFocus::Palette, KeyFocus::ViewerMenu] {
            for (k, m, a) in [(Key::N, cmd, Action::NewWorkspace), (Key::L, cmd, Action::NextTab), (Key::Period, cmd, Action::ToggleViewer), (Key::S, cmd | Modifiers::SHIFT, Action::ViewMenu)] {
                assert_eq!(press_in(focus, k, m), app(a), "{focus:?} {k:?}");
            }
        }
        assert!(press_in(KeyFocus::Dialog, Key::N, cmd).is_empty());
        // Plain keys and vim's ⌃D stay for the widget.
        let (cmds, left) = run(&Keymap::defaults(), KeyFocus::Viewer, key_events(Key::D, Modifiers::CTRL));
        assert!(cmds.is_empty() && left.iter().any(|e| matches!(e, Event::Key { key: Key::D, .. })));
    }

    /// ⌘⇧S opens the PR menu, and no plugin can take it.
    #[test]
    fn cmd_shift_s_is_the_pr_menu() {
        assert_eq!(press(Key::S, Modifiers::COMMAND | Modifiers::SHIFT), app(Action::ViewMenu));
        let b = Binding::parse("cmd+shift+s", &[]).unwrap();
        assert_eq!(Keymap::defaults().builtin_conflict(super::super::Place::Viewer, &b).as_deref(), Some("[global] view_menu"));
    }

    fn with_chord(chord: &str, when: When) -> Keymap {
        let mut reg = Registry::default();
        reg.chords.push(ChordEntry { plugin: "p".into(), chord: Binding::parse(chord, &[]).unwrap(), command: "p.c".into(), when, filetype: None });
        Keymap::defaults().with_plugins(&reg)
    }

    #[test]
    fn plugin_chords_match_exactly_and_skip_builtins() {
        let km = with_chord("cmd+shift+y", When::Any);
        let cs = Modifiers::COMMAND | Modifiers::SHIFT;
        assert_eq!(run(&km, KeyFocus::Terminal, key_events(Key::Y, cs)).0, vec![Cmd::Plugin("p.c".into())]);
        assert!(run(&km, KeyFocus::Terminal, key_events(Key::Y, Modifiers::COMMAND)).0.is_empty(), "⌘Y is not ⌘⇧Y");
        let d = Keymap::defaults();
        let b = |s: &str| Binding::parse(s, &[]).unwrap();
        assert!(d.builtin_conflict(super::super::Place::Global, &b("cmd+c")).is_some());
        assert!(d.builtin_conflict(super::super::Place::Global, &b("cmd+h")).is_some());
        assert!(d.builtin_conflict(super::super::Place::Global, &b("cmd+shift+h")).is_some());
        assert!(d.builtin_conflict(super::super::Place::Global, &b("cmd+shift+u")).is_none());
        // ⌥ chords take the composed character with them.
        let km = with_chord("alt+y", When::Viewer);
        let mut raw = key_events(Key::Y, Modifiers::ALT);
        raw.events.push(Event::Text("¥".into()));
        let (cmds, left) = run(&km, KeyFocus::Viewer, raw);
        assert_eq!(cmds, vec![Cmd::Plugin("p.c".into())]);
        assert!(!left.iter().any(|e| matches!(e, Event::Text(_))));
    }

    fn paste_run(km: &Keymap, modifiers: Modifiers) -> (Vec<Cmd>, bool) {
        let mut raw = egui::RawInput { events: vec![Event::ModifiersChanged(modifiers), Event::Paste("x".into())], ..Default::default() };
        edit_keys(&mut raw, km, Modifiers::NONE);
        let (cmds, left) = run(km, KeyFocus::Viewer, raw);
        (cmds, left.iter().any(|e| matches!(e, Event::Paste(_))))
    }

    #[test]
    fn cmd_shift_v_reaches_plugin_chords() {
        let cs = Modifiers::COMMAND | Modifiers::SHIFT;
        let km = with_chord("cmd+shift+v", When::Viewer);
        assert_eq!(paste_run(&km, cs), (vec![Cmd::Plugin("p.c".into())], false), "the chord takes the paste too");
        assert_eq!(paste_run(&km, Modifiers::COMMAND), (vec![], true), "⌘V is a paste");
    }

    #[test]
    fn cmd_shift_v_still_pastes_when_unbound() {
        assert_eq!(paste_run(&Keymap::defaults(), Modifiers::COMMAND | Modifiers::SHIFT), (vec![], true));
    }

    #[test]
    fn cmd_shift_a_survives_the_edit_remap() {
        let cs = Modifiers::COMMAND | Modifiers::SHIFT;
        let mut raw = key_events(Key::A, cs);
        edit_keys(&mut raw, &Keymap::defaults(), Modifiers::NONE);
        assert_eq!(run(&Keymap::defaults(), KeyFocus::Viewer, raw).0, app(Action::Ask));
        // ⌘A itself is left for the text field.
        let mut raw = key_events(Key::A, Modifiers::COMMAND);
        edit_keys(&mut raw, &Keymap::defaults(), Modifiers::NONE);
        assert!(raw.events.iter().any(|e| matches!(e, Event::Key { key: Key::A, .. })));
    }

    #[test]
    fn moved_edit_keys() {
        let km = Keymap::with_user("[edit]\ncopy = [\"cmd+shift+c\"]\n");
        let copies = |modifiers: Modifiers, km: &Keymap| {
            let mut raw = egui::RawInput { events: vec![Event::ModifiersChanged(modifiers), Event::Copy], ..Default::default() };
            edit_keys(&mut raw, km, Modifiers::NONE);
            raw.events.iter().filter(|e| matches!(e, Event::Copy)).count()
        };
        assert_eq!(copies(Modifiers::COMMAND, &km), 0, "⌘C no longer copies");
        assert_eq!(copies(Modifiers::COMMAND | Modifiers::SHIFT, &km), 1, "⌘⇧C does");
        assert_eq!(copies(Modifiers::COMMAND, &Keymap::defaults()), 1);

        let km = Keymap::with_user("[edit]\ncopy = [\"ctrl+c\"]\n");
        let mut raw = key_events(Key::C, Modifiers::CTRL);
        edit_keys(&mut raw, &km, Modifiers::NONE);
        assert!(raw.events.iter().any(|e| matches!(e, Event::Copy)), "⌃C copies");
        assert!(!raw.events.iter().any(|e| matches!(e, Event::Key { key: Key::C, .. })), "and never reaches the terminal");

        let km = Keymap::with_user("[edit]\nselect_all = [\"ctrl+a\"]\n");
        let mut raw = key_events(Key::A, Modifiers::COMMAND);
        edit_keys(&mut raw, &km, Modifiers::NONE);
        assert!(!raw.events.iter().any(|e| matches!(e, Event::Key { .. })), "⌘A moved away does nothing");
        let mut raw = key_events(Key::A, Modifiers::CTRL);
        edit_keys(&mut raw, &km, Modifiers::NONE);
        assert!(raw.events.iter().any(|e| matches!(e, Event::Key { key: Key::A, modifiers, .. } if modifiers.command && !modifiers.ctrl)), "⌃A selects all");
    }

    #[test]
    fn shifted_chords_do_not_trigger_plain_ones() {
        let cs = Modifiers::COMMAND | Modifiers::SHIFT;
        let cmd = Modifiers::COMMAND;
        assert_eq!(press(Key::I, cs), app(Action::OpenPlan));
        assert_eq!(press(Key::R, cs), app(Action::RenameWorkspace));
        assert_eq!(press(Key::H, cmd), app(Action::PrevTab));
        assert_eq!(press(Key::H, cs), app(Action::FocusFileList));
        assert_eq!(press(Key::L, cs), app(Action::FocusCode));
        assert_eq!(press(Key::J, cs), app(Action::NextFile));
        assert_eq!(press(Key::K, cs), app(Action::PrevFile));
        assert_eq!(press(Key::A, cs), app(Action::Ask));
        assert_eq!(press(Key::J, cmd), app(Action::NextWorkspace));
        assert_eq!(press(Key::L, cmd), app(Action::NextTab));
        assert_eq!(press(Key::R, cmd), app(Action::RenameTab));
        assert_eq!(press_in(KeyFocus::Terminal, Key::T, cmd), app(Action::NewTab));
        assert_eq!(press_in(KeyFocus::Terminal, Key::I, cmd), app(Action::NewMainCheckoutTab));
        assert_eq!(press(Key::T, cs), app(Action::ReopenTab));
        assert!(press(Key::E, cmd).is_empty(), "⌘E is not bound");
        assert_eq!(press(Key::P, cs), app(Action::Palette));
        assert_eq!(press(Key::P, cmd), app(Action::FilePicker));
        assert_eq!(press(Key::Period, cmd), app(Action::ToggleViewer));
        assert_eq!(press(Key::Num3, cmd), app(Action::GoToTab(3)));
        assert!(press(Key::H, Modifiers::NONE).is_empty(), "plain keys belong to the terminal/viewer");
        assert!(press(Key::H, Modifiers::ALT).is_empty() && press(Key::Backtick, cmd).is_empty(), "⌥H and ⌘` are not bound");
    }
}
