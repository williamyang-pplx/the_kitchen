//! Terminal tabs: an alacritty_terminal emulator fed by a pty that runs
//! `tmux attach` (locally, or through ssh for devbox workspaces).

mod colors;
mod input;
mod predict;
mod render;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};

pub use render::TerminalView;

/// Grid size handed to alacritty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Side effects the emulator asks the UI to perform.
#[derive(Default)]
struct Shared {
    title: Mutex<Option<String>>,
    clipboard: Mutex<Option<String>>,
    bell: AtomicBool,
    exited: AtomicBool,
}

#[derive(Clone)]
pub struct Listener {
    ctx: egui::Context,
    shared: Arc<Shared>,
    sender: Arc<OnceLock<EventLoopSender>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::Wakeup | TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => {}
            TermEvent::Title(t) => *self.shared.title.lock().unwrap() = Some(t),
            TermEvent::ResetTitle => *self.shared.title.lock().unwrap() = None,
            TermEvent::ClipboardStore(_, text) => *self.shared.clipboard.lock().unwrap() = Some(text),
            TermEvent::PtyWrite(text) => {
                if let Some(s) = self.sender.get() {
                    let _ = s.send(Msg::Input(Cow::Owned(text.into_bytes())));
                }
            }
            TermEvent::Bell => self.shared.bell.store(true, Ordering::Relaxed),
            TermEvent::ChildExit(_) | TermEvent::Exit => self.shared.exited.store(true, Ordering::Relaxed),
            TermEvent::ClipboardLoad(..) | TermEvent::ColorRequest(..) | TermEvent::TextAreaSizeRequest(..) => {}
        }
        self.ctx.request_repaint();
    }
}

/// One running pty + emulator. Dropping it shuts the pty down (the tmux
/// session on the host keeps running).
pub struct TermSession {
    pub term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    shared: Arc<Shared>,
    size: GridSize,
    cell: (u16, u16),
    /// Scroll remainder (in lines) carried between wheel events.
    pub(crate) scroll_acc: f32,
    pub(crate) focused: bool,
    pub(crate) mouse_buttons_down: u8,
    /// Local echo prediction (enabled for devbox tabs).
    pub(crate) predictor: predict::Predictor,
}

impl TermSession {
    /// Spawn `argv` in a pty sized `size`.
    pub fn spawn(ctx: &egui::Context, argv: &[String], cwd: Option<&str>, env: HashMap<String, String>) -> anyhow::Result<Self> {
        let size = GridSize { cols: 120, rows: 40 };
        let cell = (8u16, 16u16);
        let mut env = env;
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
        let opts = PtyOptions {
            shell: Some(Shell::new(argv[0].clone(), argv[1..].to_vec())),
            working_directory: cwd.map(Into::into),
            drain_on_exit: false,
            env,
        };
        let ws = window_size(size, cell);
        let pty = tty::new(&opts, ws, 0)?;
        let shared = Arc::new(Shared::default());
        let sender_cell = Arc::new(OnceLock::new());
        let listener = Listener { ctx: ctx.clone(), shared: shared.clone(), sender: sender_cell.clone() };
        let config = TermConfig { scrolling_history: 10_000, ..Default::default() };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, listener.clone())));
        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)?;
        let sender = event_loop.channel();
        let _ = sender_cell.set(sender.clone());
        event_loop.spawn();
        Ok(Self {
            term,
            sender,
            shared,
            size,
            cell,
            scroll_acc: 0.0,
            focused: false,
            mouse_buttons_down: 0,
            predictor: Default::default(),
        })
    }

    /// Show typed characters before the (remote) echo arrives.
    pub fn set_predictive_echo(&mut self, on: bool) {
        self.predictor.enabled = on;
    }

    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let _ = self.sender.send(Msg::Input(bytes.into()));
    }

    pub fn resize(&mut self, size: GridSize, cell: (u16, u16)) {
        if size == self.size && cell == self.cell {
            return;
        }
        self.size = size;
        self.cell = cell;
        self.term.lock().resize(size);
        let _ = self.sender.send(Msg::Resize(window_size(size, cell)));
    }

    pub fn exited(&self) -> bool {
        self.shared.exited.load(Ordering::Relaxed)
    }

    pub fn title(&self) -> Option<String> {
        self.shared.title.lock().unwrap().clone()
    }

    /// Text the program asked to put on the clipboard (OSC 52, e.g. tmux copy-mode).
    pub fn take_clipboard(&self) -> Option<String> {
        self.shared.clipboard.lock().unwrap().take()
    }

    pub fn take_bell(&self) -> bool {
        self.shared.bell.swap(false, Ordering::Relaxed)
    }

    pub fn size(&self) -> GridSize {
        self.size
    }
}

impl Drop for TermSession {
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

fn window_size(size: GridSize, cell: (u16, u16)) -> WindowSize {
    WindowSize { num_lines: size.rows as u16, num_cols: size.cols as u16, cell_width: cell.0, cell_height: cell.1 }
}
