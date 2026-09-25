//! Streaming pre-scanner for the escape sequences Amalgum honours beyond alacritty's set
//! (§5.27). The PTY reader feeds every chunk through [`Scanner::feed`] *and* on to the
//! terminal parser unchanged; the scanner only observes.
//!
//! Recognised (terminated by BEL `\x07` or ST `ESC \`; sequences may be split across chunks):
//! - OSC 7 `file://host/path` → [`OscEvent::Cwd`] (path percent-decoded)
//! - OSC 0 / OSC 2 → [`OscEvent::Title`]
//! - OSC 9 `<body>` → Notify without title. `9;4;…` is ConEmu progress: ignored.
//! - OSC 99 `<metadata>;<body>` (kitty) → Notify with the payload as body
//! - OSC 777 `notify;<title>;<body>` → Notify
//! - OSC 133 `A` / `B` / `C` / `D[;exit]` → [`OscEvent::Prompt`]
//! - a bare BEL outside any sequence → [`OscEvent::Bell`]
//!
//! Anything longer than [`MAX_OSC`] bytes is discarded (never buffered without bound).
//! Invalid UTF-8 is replaced lossily.

pub const MAX_OSC: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    Cwd { host: Option<String>, path: String },
    Title(String),
    Notify { title: Option<String>, body: String },
    Prompt(PromptMark),
    Bell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMark {
    /// `A`: prompt shown → idle.
    PromptStart,
    /// `B`: user is typing a command.
    CommandStart,
    /// `C`: command running.
    CommandExecuted,
    /// `D[;exit]`: command finished; non-zero exit shows a one-shot warning dot.
    CommandFinished { exit: Option<i32> },
}

#[derive(Debug, Default)]
pub struct Scanner {
    // private state machine; see module docs
}

impl Scanner {
    /// Scan `bytes`, appending recognised events to `out` in stream order.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<OscEvent>) {
        todo!()
    }
}
