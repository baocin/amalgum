//! `ScanningPty`: wraps alacritty's `tty::Pty`, delegating registration, resize, and child
//! events, but exposing a reader that feeds each chunk through `agent::osc::Scanner` before the
//! bytes reach the VT parser, so OSC 7/9/99/777/133 and BEL become [`TermEvent`]s without a
//! second reader.
//!
//! The reader is a `try_clone` of the PTY master (same open file description, so the
//! non-blocking flag and poller readiness are shared with the registered fd).

use std::fs::File;
use std::io::{self, Read};
use std::process::Child;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{ChildEvent, EventedPty, EventedReadWrite, Pty};
use polling::{Event, PollMode, Poller};

use crate::agent::osc::Scanner;

use super::TermEvent;

/// A PTY reader that tees every chunk through the OSC pre-scanner before returning it to
/// alacritty's parser. Reads from its own clone of the master fd, so it shares readiness with
/// the fd `ScanningPty::register` hands to the poller.
pub struct ScanReader {
    file: File,
    scanner: Scanner,
    tx: Sender<TermEvent>,
    ctx: egui::Context,
}

impl Read for ScanReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        if n > 0 {
            let mut events = Vec::new();
            self.scanner.feed(&buf[..n], &mut events);
            if !events.is_empty() {
                for ev in events {
                    // The receiving end (the app's poll loop) may have been dropped already if
                    // the pane is closing concurrently; a dropped receiver is not our problem.
                    let _ = self.tx.send(TermEvent::Osc(ev));
                }
                self.ctx.request_repaint();
            }
        }
        Ok(n)
    }
}

/// Wraps alacritty's `tty::Pty` so its reader runs through [`ScanReader`] first. Every other
/// operation (registration, resize, child exit) delegates straight to `inner`.
pub struct ScanningPty {
    inner: Pty,
    reader: ScanReader,
}

impl ScanningPty {
    /// `inner`'s master fd is cloned for the scanning reader; `inner` keeps the fd it registers
    /// with the poller.
    pub fn new(inner: Pty, tx: Sender<TermEvent>, ctx: egui::Context) -> io::Result<Self> {
        let file = inner.file().try_clone()?;
        Ok(Self { inner, reader: ScanReader { file, scanner: Scanner::default(), tx, ctx } })
    }

    pub fn child(&self) -> &Child {
        self.inner.child()
    }
}

impl EventedReadWrite for ScanningPty {
    type Reader = ScanReader;
    type Writer = File;

    unsafe fn register(&mut self, poll: &Arc<Poller>, interest: Event, mode: PollMode) -> io::Result<()> {
        // SAFETY: the sources registered here (the PTY master fd and its SIGCHLD pipe) are
        // owned by `self.inner`, which lives at least as long as this registration — it is a
        // field of `self`, and dropping `self` deregisters before it can drop `inner`'s fds
        // (alacritty's event loop always calls `deregister` before the pty is dropped; see
        // `EventLoop::spawn`'s final `let _ = self.pty.deregister(&self.poll);`).
        unsafe { self.inner.register(poll, interest, mode) }
    }

    fn reregister(&mut self, poll: &Arc<Poller>, interest: Event, mode: PollMode) -> io::Result<()> {
        self.inner.reregister(poll, interest, mode)
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.inner.deregister(poll)
    }

    fn reader(&mut self) -> &mut ScanReader {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut File {
        self.inner.writer()
    }
}

impl EventedPty for ScanningPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.inner.next_child_event()
    }
}

impl OnResize for ScanningPty {
    fn on_resize(&mut self, window_size: WindowSize) {
        self.inner.on_resize(window_size);
    }
}
