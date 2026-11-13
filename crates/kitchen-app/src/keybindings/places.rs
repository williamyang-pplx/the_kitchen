//! Where in the kitchen a key is pressed, and what each place can do.
//! keybindings.toml only chooses the keys.
//!
//! This is the only keybindings file naming `app::Action` and `vim::VimCmd`;
//! those modules use keybindings, keybindings uses only their command types.

use super::app::Action;
use crate::plugins::When;
use crate::viewer::vim::VimCmd;

/// A section of keybindings.toml.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Place {
    Global,
    Terminal,
    Panel,
    Viewer,
    Vim,
    FileList,
    Plan,
    Picker,
    Menu,
    Dialog,
    Input,
    Edit,
}

impl Place {
    /// Also the ⌘/ group order.
    pub const ALL: [Place; 12] = [
        Place::Global,
        Place::Terminal,
        Place::Panel,
        Place::Viewer,
        Place::Vim,
        Place::FileList,
        Place::Plan,
        Place::Picker,
        Place::Menu,
        Place::Dialog,
        Place::Input,
        Place::Edit,
    ];

    pub fn section(self) -> &'static str {
        match self {
            Place::Global => "global",
            Place::Terminal => "terminal",
            Place::Panel => "panel",
            Place::Viewer => "viewer",
            Place::Vim => "vim",
            Place::FileList => "file_list",
            Place::Plan => "plan",
            Place::Picker => "picker",
            Place::Menu => "menu",
            Place::Dialog => "dialog",
            Place::Input => "input",
            Place::Edit => "edit",
        }
    }

    /// The ⌘/ group title.
    pub fn title(self) -> &'static str {
        match self {
            Place::Global => "Everywhere",
            Place::Terminal => "Terminal",
            Place::Panel => "Viewer and plan",
            Place::Viewer => "Viewer",
            Place::Vim => "Vim motions",
            Place::FileList => "File list",
            Place::Plan => "Plan",
            Place::Picker => "Pickers",
            Place::Menu => "PR menu",
            Place::Dialog => "Dialogs",
            Place::Input => "Ask and search fields",
            Place::Edit => "Editing",
        }
    }

    pub fn from_section(s: &str) -> Option<Place> {
        Place::ALL.into_iter().find(|p| p.section() == s)
    }
}

/// What has keyboard focus; picks the chain of places a key is looked up in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyFocus {
    Terminal,
    /// The viewer's code, normal mode.
    Viewer,
    /// The viewer's file list: no vim motions there.
    FileList,
    /// The viewer's ask field or search prompt.
    ViewerInput,
    Plan,
    PlanInput,
    /// ⌘⇧P: terminal keys are dead while it is open.
    Palette,
    /// ⌘P over the viewer: the viewer's keys still work.
    ViewerPicker,
    /// ⌘⇧S.
    ViewerMenu,
    /// Global chords are dead while a dialog is up.
    Dialog,
}

impl KeyFocus {
    pub const ALL: [KeyFocus; 10] = [
        KeyFocus::Terminal,
        KeyFocus::Viewer,
        KeyFocus::FileList,
        KeyFocus::ViewerInput,
        KeyFocus::Plan,
        KeyFocus::PlanInput,
        KeyFocus::Palette,
        KeyFocus::ViewerPicker,
        KeyFocus::ViewerMenu,
        KeyFocus::Dialog,
    ];

    /// Innermost place first.
    pub fn chain(self) -> &'static [Place] {
        use Place::*;
        match self {
            KeyFocus::Terminal => &[Terminal, Global],
            KeyFocus::Viewer => &[Viewer, Vim, Panel, Global],
            KeyFocus::FileList => &[FileList, Viewer, Panel, Global],
            KeyFocus::ViewerInput => &[Input, Viewer, Panel, Global],
            KeyFocus::Plan => &[Plan, Vim, Panel, Global],
            KeyFocus::PlanInput => &[Input, Plan, Panel, Global],
            KeyFocus::Palette => &[Picker, Global],
            KeyFocus::ViewerPicker => &[Picker, Viewer, Panel, Global],
            KeyFocus::ViewerMenu => &[Menu, Picker, Viewer, Panel, Global],
            KeyFocus::Dialog => &[Dialog],
        }
    }

    /// The text-field focus of a vim focus (the search prompt).
    pub fn input(self) -> KeyFocus {
        match self {
            KeyFocus::Plan | KeyFocus::PlanInput => KeyFocus::PlanInput,
            _ => KeyFocus::ViewerInput,
        }
    }
}

impl When {
    /// The place a plugin's chords for this `when` join.
    pub fn place(self) -> Place {
        match self {
            When::Any => Place::Global,
            When::Terminal => Place::Terminal,
            When::Viewer => Place::Viewer,
            When::Plan => Place::Plan,
        }
    }
}

/// What a binding does.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Cmd {
    App(Action),
    Vim(VimCmd),
    List(ListCmd),
    Menu(MenuCmd),
    /// `[dialog]` and `[input]`.
    Field(FieldCmd),
    Edit(EditCmd),
    Click(ClickCmd),
    /// `<plugin>.<command>`.
    Plugin(String),
}

impl From<Action> for Cmd {
    fn from(a: Action) -> Cmd {
        Cmd::App(a)
    }
}

impl From<VimCmd> for Cmd {
    fn from(c: VimCmd) -> Cmd {
        Cmd::Vim(c)
    }
}

impl From<FieldCmd> for Cmd {
    fn from(c: FieldCmd) -> Cmd {
        Cmd::Field(c)
    }
}

impl From<MenuCmd> for Cmd {
    fn from(c: MenuCmd) -> Cmd {
        Cmd::Menu(c)
    }
}

impl From<ListCmd> for Cmd {
    fn from(c: ListCmd) -> Cmd {
        Cmd::List(c)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ListCmd {
    Down,
    Up,
    PageDown,
    PageUp,
    Top,
    Bottom,
    Open,
    Parent,
    Switch,
    Leave,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MenuCmd {
    Up,
    Down,
    Choose,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FieldCmd {
    Submit,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EditCmd {
    Copy,
    Cut,
    Paste,
    SelectAll,
    Undo,
    Redo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClickCmd {
    OpenLink,
    CloseTab,
}

/// One action a section accepts: its name in the file, what it does, its ⌘/ text.
pub struct ActionDef {
    pub name: &'static str,
    pub cmd: Cmd,
    pub desc: &'static str,
}

const fn def(name: &'static str, cmd: Cmd, desc: &'static str) -> ActionDef {
    ActionDef { name, cmd, desc }
}

const fn app(name: &'static str, a: Action, desc: &'static str) -> ActionDef {
    def(name, Cmd::App(a), desc)
}

const fn vim(name: &'static str, c: VimCmd, desc: &'static str) -> ActionDef {
    def(name, Cmd::Vim(c), desc)
}

static GLOBAL: [ActionDef; 26] = [
    app("new_workspace", Action::NewWorkspace, "new workspace"),
    app("next_workspace", Action::NextWorkspace, "next workspace"),
    app("prev_workspace", Action::PrevWorkspace, "previous workspace"),
    app("rename_workspace", Action::RenameWorkspace, "rename workspace"),
    app("prev_tab", Action::PrevTab, "previous tab"),
    app("next_tab", Action::NextTab, "next tab"),
    app("go_to_tab_1", Action::GoToTab(1), "tab 1"),
    app("go_to_tab_2", Action::GoToTab(2), "tab 2"),
    app("go_to_tab_3", Action::GoToTab(3), "tab 3"),
    app("go_to_tab_4", Action::GoToTab(4), "tab 4"),
    app("go_to_tab_5", Action::GoToTab(5), "tab 5"),
    app("go_to_tab_6", Action::GoToTab(6), "tab 6"),
    app("go_to_tab_7", Action::GoToTab(7), "tab 7"),
    app("go_to_tab_8", Action::GoToTab(8), "tab 8"),
    app("go_to_tab_9", Action::GoToTab(9), "tab 9"),
    app("rename_tab", Action::RenameTab, "rename tab"),
    app("reopen_tab", Action::ReopenTab, "reopen closed tab"),
    app("toggle_viewer", Action::ToggleViewer, "terminal ↔ viewer"),
    app("toggle_files", Action::ToggleFiles, "show / hide the viewer's file list"),
    app("open_plan", Action::OpenPlan, "plan"),
    app("view_menu", Action::ViewMenu, "switch between the tab's PRs"),
    app("dashboard", Action::Dashboard, "dashboard"),
    app("palette", Action::Palette, "commands, or a PR link"),
    app("shortcuts", Action::Shortcuts, "this list"),
    app("quit", Action::Quit, "quit"),
    def("close_tab_click", Cmd::Click(ClickCmd::CloseTab), "close a tab in the tab strip"),
];

static TERMINAL: [ActionDef; 4] = [
    app("new_tab", Action::NewTab, "new edit tab"),
    app("new_main_checkout_tab", Action::NewMainCheckoutTab, "new read tab"),
    app("close_tab", Action::CloseTab, "close tab"),
    def("open_link_click", Cmd::Click(ClickCmd::OpenLink), "open the path or URL under the pointer"),
];

static PANEL: [ActionDef; 6] = [
    app("comment", Action::Comment, "comment on the line or selection"),
    app("ask", Action::Ask, "ask about the line or selection (again: follow up)"),
    app("submit", Action::Submit, "send the comment (plan: send all to the agent)"),
    app("toggle_diff", Action::ToggleDiff, "diff / plain"),
    app("toggle_outline", Action::ToggleOutline, "outline"),
    app("focus_terminal", Action::FocusTerminal, "back to the terminal"),
];

static VIEWER: [ActionDef; 5] = [
    app("file_picker", Action::FilePicker, "find a file"),
    app("focus_file_list", Action::FocusFileList, "to the file list"),
    app("focus_code", Action::FocusCode, "to the code"),
    app("next_file", Action::NextFile, "next changed file"),
    app("prev_file", Action::PrevFile, "previous changed file"),
];

static VIM: [ActionDef; 60] = [
    vim("down", VimCmd::Down, "down"),
    vim("up", VimCmd::Up, "up"),
    vim("left", VimCmd::Left, "left"),
    vim("right", VimCmd::Right, "right"),
    vim("line_start", VimCmd::LineStart, "start of line"),
    vim("first_non_blank", VimCmd::FirstNonBlank, "first non-blank"),
    vim("line_end", VimCmd::LineEnd, "end of line"),
    vim("word_next", VimCmd::WordNext, "next word"),
    vim("big_word_next", VimCmd::BigWordNext, "next WORD"),
    vim("word_prev", VimCmd::WordPrev, "previous word"),
    vim("big_word_prev", VimCmd::BigWordPrev, "previous WORD"),
    vim("word_end", VimCmd::WordEnd, "end of word"),
    vim("big_word_end", VimCmd::BigWordEnd, "end of WORD"),
    vim("go_to_line", VimCmd::GoToLine, "last line (with a count: that line)"),
    vim("go_to_top", VimCmd::GoToTop, "first line"),
    vim("view_top", VimCmd::ViewTop, "top of the view"),
    vim("view_middle", VimCmd::ViewMiddle, "middle of the view"),
    vim("view_bottom", VimCmd::ViewBottom, "bottom of the view"),
    vim("paragraph_next", VimCmd::ParagraphNext, "next paragraph"),
    vim("paragraph_prev", VimCmd::ParagraphPrev, "previous paragraph"),
    vim("match_bracket", VimCmd::MatchBracket, "matching bracket"),
    vim("find_forward", VimCmd::FindForward, "find a character"),
    vim("find_backward", VimCmd::FindBackward, "find a character backwards"),
    vim("till_forward", VimCmd::TillForward, "till a character"),
    vim("till_backward", VimCmd::TillBackward, "till a character backwards"),
    vim("repeat_find", VimCmd::RepeatFind, "repeat the find"),
    vim("repeat_find_back", VimCmd::RepeatFindBack, "repeat the find backwards"),
    vim("half_page_down", VimCmd::HalfPageDown, "half a page down"),
    vim("half_page_up", VimCmd::HalfPageUp, "half a page up"),
    vim("page_down", VimCmd::PageDown, "page down"),
    vim("page_up", VimCmd::PageUp, "page up"),
    vim("scroll_down", VimCmd::ScrollDown, "scroll down a line"),
    vim("scroll_up", VimCmd::ScrollUp, "scroll up a line"),
    vim("scroll_center", VimCmd::ScrollCenter, "cursor line to the middle"),
    vim("scroll_top", VimCmd::ScrollTop, "cursor line to the top"),
    vim("scroll_bottom", VimCmd::ScrollBottom, "cursor line to the bottom"),
    vim("open_fold", VimCmd::OpenFold, "open the fold"),
    vim("open_all_folds", VimCmd::OpenAllFolds, "open every fold"),
    vim("close_all_folds", VimCmd::CloseAllFolds, "fold the file again"),
    vim("enter", VimCmd::Enter, "open (outline: the symbol)"),
    vim("escape", VimCmd::Escape, "leave visual mode, close"),
    vim("jump_to_def", VimCmd::JumpToDef, "definition"),
    vim("references", VimCmd::References, "references"),
    vim("jump_back", VimCmd::JumpBack, "back"),
    vim("jump_forward", VimCmd::JumpForward, "forward"),
    vim("next_hunk", VimCmd::NextHunk, "next change"),
    vim("prev_hunk", VimCmd::PrevHunk, "previous change"),
    vim("next_file", VimCmd::NextFile, "next changed file"),
    vim("prev_file", VimCmd::PrevFile, "previous changed file"),
    vim("next_diag", VimCmd::NextDiag, "next diagnostic"),
    vim("prev_diag", VimCmd::PrevDiag, "previous diagnostic"),
    vim("parent", VimCmd::Parent, "the folder one level up"),
    vim("search_forward", VimCmd::SearchForward, "search"),
    vim("search_backward", VimCmd::SearchBackward, "search backwards"),
    vim("search_next", VimCmd::SearchNext, "next match"),
    vim("search_prev", VimCmd::SearchPrev, "previous match"),
    vim("search_word", VimCmd::SearchWord, "search the word under the cursor"),
    vim("search_word_back", VimCmd::SearchWordBack, "search the word backwards"),
    vim("visual", VimCmd::Visual, "visual mode"),
    vim("visual_line", VimCmd::VisualLine, "visual line mode"),
];

static FILE_LIST: [ActionDef; 10] = [
    def("down", Cmd::List(ListCmd::Down), "down"),
    def("up", Cmd::List(ListCmd::Up), "up"),
    def("page_down", Cmd::List(ListCmd::PageDown), "page down"),
    def("page_up", Cmd::List(ListCmd::PageUp), "page up"),
    def("top", Cmd::List(ListCmd::Top), "first row"),
    def("bottom", Cmd::List(ListCmd::Bottom), "last row"),
    def("open", Cmd::List(ListCmd::Open), "open (changed files: back to the code)"),
    def("parent", Cmd::List(ListCmd::Parent), "the folder one level up"),
    def("switch", Cmd::List(ListCmd::Switch), "changed files ↔ all files"),
    def("leave", Cmd::List(ListCmd::Leave), "back to the code"),
];

static PICKER: [ActionDef; 4] = [
    def("up", Cmd::Menu(MenuCmd::Up), "up"),
    def("down", Cmd::Menu(MenuCmd::Down), "down"),
    def("choose", Cmd::Menu(MenuCmd::Choose), "open"),
    def("cancel", Cmd::Menu(MenuCmd::Cancel), "close"),
];

static MENU: [ActionDef; 2] = [def("up", Cmd::Menu(MenuCmd::Up), "up"), def("down", Cmd::Menu(MenuCmd::Down), "down")];

static DIALOG: [ActionDef; 2] = [def("submit", Cmd::Field(FieldCmd::Submit), "submit"), def("cancel", Cmd::Field(FieldCmd::Cancel), "cancel")];

static INPUT: [ActionDef; 2] = [def("submit", Cmd::Field(FieldCmd::Submit), "ask / search"), def("cancel", Cmd::Field(FieldCmd::Cancel), "close")];

static EDIT: [ActionDef; 6] = [
    def("copy", Cmd::Edit(EditCmd::Copy), "copy"),
    def("cut", Cmd::Edit(EditCmd::Cut), "cut"),
    def("paste", Cmd::Edit(EditCmd::Paste), "paste (images too, for Claude)"),
    def("select_all", Cmd::Edit(EditCmd::SelectAll), "select all"),
    def("undo", Cmd::Edit(EditCmd::Undo), "undo"),
    def("redo", Cmd::Edit(EditCmd::Redo), "redo"),
];

/// The actions of a place, in ⌘/ order: the only list of action names.
pub fn actions(place: Place) -> &'static [ActionDef] {
    match place {
        Place::Global => &GLOBAL,
        Place::Terminal => &TERMINAL,
        Place::Panel => &PANEL,
        Place::Viewer => &VIEWER,
        Place::Vim => &VIM,
        Place::FileList => &FILE_LIST,
        Place::Plan => &[],
        Place::Picker => &PICKER,
        Place::Menu => &MENU,
        Place::Dialog => &DIALOG,
        Place::Input => &INPUT,
        Place::Edit => &EDIT,
    }
}

/// The action `cmd` is in `place`, if it is one.
pub fn def_of(place: Place, cmd: &Cmd) -> Option<&'static ActionDef> {
    actions(place).iter().find(|d| d.cmd == *cmd)
}
