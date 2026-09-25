//! One terminal pane (§5.27): a PTY running the user's shell, the `alacritty_terminal` VT state
//! machine, and our own egui renderer.
//!
//! Threads: alacritty's `EventLoop` owns the PTY on its own thread. It reads through
//! [`pty::ScanningPty`], which tees every chunk through `agent::osc::Scanner` before the bytes
//! reach the VT parser, so OSC 7/9/99/777/133 and BEL become [`TermEvent`]s without a second
//! reader. Terminal events (title, bell, child exit, PTY write-backs) arrive through an
//! `EventListener` proxy. Both paths push onto one channel and call `request_repaint`; idle
//! terminals cost zero frames.
//!
//! Environment of the spawned shell: `AMALGUM_SOCK`, `AMALGUM_WORKSPACE`, `AMALGUM_TAB`,
//! `TERM=xterm-256color`, `COLORTERM=truecolor` (§5.27 "Model").

mod input;
mod pty;
mod render;

use super::theme::Colors;
use crate::agent::osc::OscEvent;
use crate::model::keymap::Keymap;
use std::io;
use std::path::PathBuf;

/// What to start in a new pane.
#[derive(Debug, Clone, Default)]
pub struct Spawn {
    pub cwd: PathBuf,
    /// Extra environment on top of the TERM/COLORTERM defaults.
    pub env: Vec<(String, String)>,
    /// Program and args; `None` = the login shell (`$SHELL`, as alacritty resolves it).
    pub program: Option<(String, Vec<String>)>,
    pub scrollback: usize,
}

/// Something the app must react to. Drained once per frame with [`TerminalPane::poll`].
#[derive(Debug, Clone, PartialEq)]
pub enum TermEvent {
    Osc(OscEvent),
    /// OSC 0/2 via alacritty (also reset to `None`).
    Title(Option<String>),
    Bell,
    /// The shell exited; the pane shows "[Process exited with code N]".
    Exited(Option<i32>),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Response {
    /// The user clicked into the pane; the app should focus it.
    pub focus_requested: bool,
}

pub struct TerminalPane {
    // private: term: Arc<FairMutex<Term<Proxy>>>, notifier: Notifier, events: Receiver<TermEvent>,
    // title, cwd, exit code, cell size, grid size, pid, scroll state
}

impl TerminalPane {
    /// Spawn the PTY + event loop. Fails if the PTY or shell cannot be created.
    pub fn spawn(spec: Spawn, ctx: &egui::Context) -> io::Result<Self> {
        todo!()
    }

    /// Paint the grid into the available rect, resize the PTY when the rect's cell dimensions
    /// change, and — when `focused` — translate keyboard/paste/IME events into PTY input.
    /// Chords containing `Mod` are left for the app (`keymap`), everything else goes to the PTY.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        focused: bool,
        keymap: &Keymap,
        colors: &Colors,
        font_size: f32,
    ) -> Response {
        todo!()
    }

    /// Events since the last call, in order.
    pub fn poll(&mut self) -> Vec<TermEvent> {
        todo!()
    }

    /// Send bytes to the PTY as if typed (run a command, paste a path for an agent).
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        todo!()
    }

    pub fn title(&self) -> Option<&str> {
        todo!()
    }
    /// Last cwd reported by OSC 7.
    pub fn cwd(&self) -> Option<&str> {
        todo!()
    }
    pub fn exit_code(&self) -> Option<Option<i32>> {
        todo!()
    }
    /// Shell pid (for port detection and agent heuristics).
    pub fn pid(&self) -> u32 {
        todo!()
    }
    /// The selected text, if any (Edit → Copy, `Mod+C`).
    pub fn selection_text(&self) -> Option<String> {
        todo!()
    }
    /// Paste as typed input, bracketed when the application enabled bracketed paste.
    pub fn paste(&self, text: &str) {
        todo!()
    }
    /// Visible screen as text (scrollback persistence, accessibility, tests).
    pub fn screen_text(&self) -> String {
        todo!()
    }
}

impl Drop for TerminalPane {
    /// Shut the event loop down and hang up the PTY.
    fn drop(&mut self) {}
}
