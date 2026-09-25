//! One terminal pane (§5.27): a PTY running the user's shell, the `alacritty_terminal` VT state
//! machine, and our own egui renderer.
//!
//! Threads: alacritty's `EventLoop` owns the PTY on its own thread. It reads through
//! [`pty::ScanningPty`], which tees every chunk through `agent::osc::Scanner` before the bytes
//! reach the VT parser, so OSC 7/9/99/777/133 and BEL become [`TermEvent`]s without a second
//! reader. Terminal events (title, bell, child exit, PTY write-backs) arrive through an
//! `EventListener` proxy ([`Proxy`]). Both paths push onto one channel and call
//! `request_repaint`; idle terminals cost zero frames (§6 "zero repaints and zero CPU for idle
//! terminals").
//!
//! Environment of the spawned shell: `AMALGUM_SOCK`, `AMALGUM_WORKSPACE`, `AMALGUM_TAB` (set by
//! the caller in `spec.env` — this module only knows about `TERM`/`COLORTERM`), plus
//! `TERM=xterm-256color`, `COLORTERM=truecolor` (§5.27 "Model"). We do **not** call alacritty's
//! `tty::setup_env()`: it calls `std::env::set_var` on the *whole process* environment, which
//! races with any other thread reading env (and is `unsafe` as of edition 2024 for exactly that
//! reason) — with several terminal panes spawning concurrently that is a real hazard, not a
//! theoretical one. `tty::Options::env` already gives each child the two variables it needs
//! without touching global process state.
//!
//! Locking: the `Term` is behind alacritty's `FairMutex`. [`TerminalPane::show`] takes the lock
//! only for short, pure operations (resize, snapshotting the visible grid, reading a mode flag)
//! and never while painting or laying out text (§6 "never hold the term lock while doing
//! anything slow").

mod input;
mod pty;
mod render;

use super::theme::Colors;
use crate::agent::osc::OscEvent;
use crate::model::keymap::Keymap;
use crate::model::theme::Token;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event as AlacrittyEvent, EventListener, Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{self, Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty;

use pty::ScanningPty;

/// Minimum pane size (§5.27 "Splits": "Minimum pane 20 columns × 5 rows").
const MIN_COLS: usize = 20;
const MIN_LINES: usize = 5;
/// Terminal line height (§4 "Typography": "Line height ... 1.2 in terminals").
const LINE_HEIGHT: f32 = 1.2;

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

/// Our own tiny `Dimensions` for `Term::new`/`Term::resize` — just columns and visible rows;
/// scrollback is tracked internally by alacritty's `Grid` from `Config::scrolling_history`, not
/// from this `Dimensions` impl (see `Term::new`, which only reads `columns()`/`screen_lines()`).
#[derive(Debug, Clone, Copy)]
struct GridSize {
    cols: usize,
    lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

fn clamp_grid(cols: usize, lines: usize) -> (usize, usize) {
    (cols.max(MIN_COLS), lines.max(MIN_LINES))
}

/// `Term`'s `EventListener`: translates alacritty's internal events into [`TermEvent`]s on
/// `tx`, repaints, and writes PTY-bound replies back through `sender` once the event loop
/// exists (see module docs on the `Arc<OnceLock<..>>` two-step below).
#[derive(Clone)]
struct Proxy {
    tx: Sender<TermEvent>,
    ctx: egui::Context,
    /// Filled with the event loop's sender right after `EventLoop::new` — the loop needs a
    /// `Term` (and therefore a `Proxy`) to exist before it can be built, so the proxy cannot be
    /// constructed with this already in hand (mirrors how alacritty itself wires this up).
    sender: Arc<OnceLock<EventLoopSender>>,
    /// The last size the PTY was resized to, shared with `TerminalPane::show` so a
    /// `TextAreaSizeRequest` can be answered without reaching back into `show`'s caller.
    size: Arc<Mutex<WindowSize>>,
}

impl Proxy {
    fn write_pty(&self, bytes: Vec<u8>) {
        if let Some(sender) = self.sender.get() {
            let _ = sender.send(Msg::Input(bytes.into()));
        }
    }
}

impl EventListener for Proxy {
    fn send_event(&self, event: AlacrittyEvent) {
        match event {
            AlacrittyEvent::Wakeup
            | AlacrittyEvent::CursorBlinkingChange
            | AlacrittyEvent::MouseCursorDirty => {
                self.ctx.request_repaint();
            }
            AlacrittyEvent::Title(t) => {
                let _ = self.tx.send(TermEvent::Title(Some(t)));
                self.ctx.request_repaint();
            }
            AlacrittyEvent::ResetTitle => {
                let _ = self.tx.send(TermEvent::Title(None));
                self.ctx.request_repaint();
            }
            AlacrittyEvent::Bell => {
                let _ = self.tx.send(TermEvent::Bell);
                self.ctx.request_repaint();
            }
            AlacrittyEvent::ChildExit(status) => {
                let _ = self.tx.send(TermEvent::Exited(exit_code(status)));
                self.ctx.request_repaint();
            }
            AlacrittyEvent::Exit => {}
            AlacrittyEvent::PtyWrite(text) => self.write_pty(text.into_bytes()),
            AlacrittyEvent::ClipboardStore(_, text) => self.ctx.copy_text(text),
            AlacrittyEvent::ClipboardLoad(_, format) => {
                // No clipboard reader is reachable from this thread (see module docs); answer
                // with empty content rather than blocking the PTY reader thread on a platform
                // clipboard call. A program polling OSC 52 gets "" instead of hanging.
                self.write_pty(format("").into_bytes());
            }
            AlacrittyEvent::ColorRequest(_, _format) => {
                // Answering correctly needs the active theme, which isn't reachable from this
                // thread; ignoring is safe — the requesting program just doesn't learn the
                // color (deferred, see final report).
            }
            AlacrittyEvent::TextAreaSizeRequest(format) => {
                let size = read_size(&self.size);
                self.write_pty(format(size).into_bytes());
            }
        }
    }
}

fn exit_code(status: ExitStatus) -> Option<i32> {
    status.code()
}

fn read_size(size: &Mutex<WindowSize>) -> WindowSize {
    match size.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

fn write_size(size: &Mutex<WindowSize>, value: WindowSize) {
    match size.lock() {
        Ok(mut guard) => *guard = value,
        Err(poisoned) => *poisoned.into_inner() = value,
    }
}

pub struct TerminalPane {
    term: Arc<FairMutex<Term<Proxy>>>,
    notifier: Notifier,
    events: Receiver<TermEvent>,
    /// Keeps the PTY reader thread alive; never read (we shut it down with `Msg::Shutdown` in
    /// `Drop` and let the thread detach rather than joining it, which could block the UI
    /// thread — §6 "never block").
    #[allow(dead_code)]
    event_loop: JoinHandle<(EventLoop<ScanningPty, Proxy>, alacritty_terminal::event_loop::State)>,
    pid: u32,
    title: Option<String>,
    cwd: Option<String>,
    exit_code: Option<Option<i32>>,
    /// Shared with `Proxy` so `TextAreaSizeRequest` sees the latest size without a callback.
    size: Arc<Mutex<WindowSize>>,
    /// Last computed `(width, height)` cell size in points, from `show`.
    cell: (f32, f32),
    cols: usize,
    lines: usize,
}

impl TerminalPane {
    /// Spawn the PTY + event loop. Fails if the PTY or shell cannot be created.
    pub fn spawn(spec: Spawn, ctx: &egui::Context) -> io::Result<Self> {
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());
        for (k, v) in spec.env {
            env.insert(k, v);
        }

        let options = tty::Options {
            shell: spec.program.map(|(p, a)| tty::Shell::new(p, a)),
            working_directory: Some(spec.cwd),
            drain_on_exit: true,
            env,
        };

        // 80x24 with 1x1 "pixel" cells: a placeholder the kernel needs immediately; `show`
        // replaces it with the real cell metrics and grid size on the first frame.
        let initial_size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 1, cell_height: 1 };
        let pty = tty::new(&options, initial_size, 0)?;

        let (tx, rx) = channel::<TermEvent>();
        let scanning = ScanningPty::new(pty, tx.clone(), ctx.clone())?;
        let pid = scanning.child().id();

        let shared_size = Arc::new(Mutex::new(initial_size));
        let proxy =
            Proxy { tx, ctx: ctx.clone(), sender: Arc::new(OnceLock::new()), size: shared_size.clone() };

        let size = GridSize { cols: 80, lines: 24 };
        let term_config = TermConfig { scrolling_history: spec.scrollback, ..Default::default() };
        let term = Term::new(term_config, &size, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let event_loop = EventLoop::new(term.clone(), proxy.clone(), scanning, true, false)?;
        let sender = event_loop.channel();
        // Filling this after `EventLoop::new` (not before) is deliberate — see the `Proxy::sender`
        // doc comment.
        let _ = proxy.sender.set(sender.clone());
        let notifier = Notifier(sender);
        let handle = event_loop.spawn();

        Ok(Self {
            term,
            notifier,
            events: rx,
            event_loop: handle,
            pid,
            title: None,
            cwd: None,
            exit_code: None,
            size: shared_size,
            cell: (1.0, 1.0),
            cols: 80,
            lines: 24,
        })
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
        let mut response = Response::default();
        let rect = ui.available_rect_before_wrap();
        let interact = ui.interact(rect, ui.id().with("terminal"), egui::Sense::click_and_drag());

        if interact.clicked() {
            response.focus_requested = true;
        }

        // -- cell metrics + resize ----------------------------------------------------------
        let font_id = egui::FontId::monospace(font_size);
        let (cell_w, cell_h) = ui.ctx().fonts_mut(|f| {
            let w = f.glyph_width(&font_id, 'M').max(1.0);
            let h = (f.row_height(&font_id) * LINE_HEIGHT).max(1.0);
            (w, h)
        });
        let (cols, lines) =
            clamp_grid((rect.width() / cell_w).floor() as usize, (rect.height() / cell_h).floor() as usize);
        if (cols, lines) != (self.cols, self.lines) {
            self.cols = cols;
            self.lines = lines;
            self.term.lock().resize(GridSize { cols, lines });
            let window_size = WindowSize {
                num_lines: lines as u16,
                num_cols: cols as u16,
                cell_width: cell_w.round().max(1.0) as u16,
                cell_height: cell_h.round().max(1.0) as u16,
            };
            write_size(&self.size, window_size);
            let _ = self.notifier.0.send(Msg::Resize(window_size));
        }
        self.cell = (cell_w, cell_h);

        // -- mouse: wheel scroll, selection drag ---------------------------------------------
        if interact.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            let delta_lines = (dy / cell_h).round() as i32;
            if delta_lines != 0 {
                self.term.lock().scroll_display(Scroll::Delta(delta_lines));
            }
        }
        if let Some(pos) = interact.interact_pointer_pos() {
            let (point, side) = self.viewport_point(pos, rect);
            if interact.triple_clicked() {
                self.term.lock().selection = Some(Selection::new(SelectionType::Lines, point, side));
            } else if interact.double_clicked() {
                self.term.lock().selection = Some(Selection::new(SelectionType::Semantic, point, side));
            } else if interact.drag_started() {
                self.term.lock().selection = Some(Selection::new(SelectionType::Simple, point, side));
            } else if interact.dragged()
                && let Some(sel) = self.term.lock().selection.as_mut()
            {
                sel.update(point, side);
            }
        }

        // -- keyboard/text/paste/IME, only when this pane is the app's focused terminal -----
        if focused {
            interact.request_focus();
            let app_cursor = self.term.lock().mode().contains(TermMode::APP_CURSOR);
            let (events, mods) = ui.input(|i| (i.events.clone(), i.modifiers));
            let mut jump_to_bottom = false;
            for ev in events {
                match ev {
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {
                        if self.is_mod_chord(key, modifiers, keymap) {
                            continue; // belongs to the app (§5.27 "Input mapping").
                        }
                        if let Some(bytes) = input::key_bytes(key, modifiers, app_cursor) {
                            self.write_bytes(bytes);
                            jump_to_bottom = true;
                        }
                    }
                    egui::Event::Text(text) => {
                        self.write_bytes(input::text_bytes(&text));
                        jump_to_bottom = true;
                    }
                    egui::Event::Copy => {
                        self.handle_clipboard(
                            ui,
                            input::copy_event_action(self.is_mod_chord(egui::Key::C, mods, keymap)),
                        );
                    }
                    egui::Event::Cut => {
                        self.handle_clipboard(
                            ui,
                            input::cut_event_action(self.is_mod_chord(egui::Key::X, mods, keymap)),
                        );
                    }
                    egui::Event::Paste(text) => {
                        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
                        let mod_chord = self.is_mod_chord(egui::Key::V, mods, keymap);
                        self.handle_clipboard(ui, input::paste_event_action(mod_chord, &text, bracketed));
                        jump_to_bottom = true;
                    }
                    egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                        self.write_bytes(input::text_bytes(&text));
                        jump_to_bottom = true;
                    }
                    _ => {}
                }
            }
            if jump_to_bottom {
                self.term.lock().scroll_display(Scroll::Bottom);
            }
        }

        // -- paint -----------------------------------------------------------------------------
        let snapshot = self.build_snapshot(colors, focused);
        render::paint(ui, rect, &snapshot, self.cell, &font_id, colors);

        if let Some(code) = self.exit_code {
            let text = match code {
                Some(n) => format!("[Process exited with code {n}] · Enter to restart · Mod+W to close"),
                None => "[Process exited] · Enter to restart · Mod+W to close".to_string(),
            };
            ui.painter().text(
                egui::pos2(rect.min.x + 8.0, rect.max.y - 4.0),
                egui::Align2::LEFT_BOTTOM,
                text,
                egui::FontId::proportional((font_size * 0.85).max(9.0)),
                colors.get(Token::FgSecondary),
            );
        }

        response
    }

    /// Whether `key`+`modifiers` resolve to the keymap's `Mod` chord — these are left for the
    /// app rather than sent to the PTY (§5.27 "Input mapping").
    fn is_mod_chord(&self, key: egui::Key, modifiers: egui::Modifiers, keymap: &Keymap) -> bool {
        crate::ui::shortcuts::chord(key, modifiers, keymap.preset).is_some_and(|c| c.mods.primary)
    }

    fn handle_clipboard(&self, ui: &egui::Ui, action: input::ClipboardAction) {
        match action {
            input::ClipboardAction::CopySelection => {
                if let Some(text) = self.selection_text() {
                    ui.ctx().copy_text(text);
                }
            }
            input::ClipboardAction::Bytes(bytes) => self.write_bytes(bytes),
        }
    }

    /// The terminal-space point and cell side under `pos` (for selection start/update).
    fn viewport_point(&self, pos: egui::Pos2, rect: egui::Rect) -> (Point, alacritty_terminal::index::Side) {
        use alacritty_terminal::index::Side;
        let (cw, ch) = self.cell;
        let x = ((pos.x - rect.min.x) / cw).max(0.0);
        let col = (x.floor() as usize).min(self.cols.saturating_sub(1));
        let side = if x.fract() < 0.5 { Side::Left } else { Side::Right };
        let row = (((pos.y - rect.min.y) / ch).floor().max(0.0) as usize).min(self.lines.saturating_sub(1));
        let display_offset = self.term.lock().grid().display_offset();
        let point = term::viewport_to_point(display_offset, Point::new(row, Column(col)));
        (point, side)
    }

    /// Build one frame's paint-ready snapshot while the term lock is held, then release it —
    /// see module docs on locking.
    fn build_snapshot(&self, colors: &Colors, focused: bool) -> render::Snapshot {
        let term = self.term.lock();
        let cols = term.columns();
        let lines = term.screen_lines();
        let content = term.renderable_content();
        let cursor_shape = content.cursor.shape;
        let display_offset = content.display_offset;
        let selection = content.selection;

        let mut rows: Vec<Vec<render::ResolvedCell>> = (0..lines).map(|_| Vec::with_capacity(cols)).collect();
        for indexed in content.display_iter {
            let Some(viewport) = term::point_to_viewport(display_offset, indexed.point) else { continue };
            if viewport.line >= lines {
                continue;
            }
            let cell = indexed.cell;
            let (fg, mut bg) = render::cell_colors(cell.fg, cell.bg, cell.flags, content.colors, colors);
            if selection.is_some_and(|s| s.contains_cell(&indexed, indexed.point, cursor_shape)) {
                bg = colors.get(Token::BgSelected);
            }
            rows[viewport.line].push(render::ResolvedCell {
                c: cell.c,
                fg,
                bg,
                italic: cell.flags.contains(Flags::ITALIC),
                underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                strikeout: cell.flags.contains(Flags::STRIKEOUT),
                skip: cell.flags.contains(Flags::WIDE_CHAR_SPACER),
            });
        }

        let cursor = term::point_to_viewport(display_offset, content.cursor.point)
            .filter(|p| p.line < lines)
            .map(|p| render::CursorInfo {
                row: p.line,
                col: p.column.0,
                paint: render::cursor_paint(cursor_shape, focused),
            });

        render::Snapshot { rows, cursor }
    }

    /// Events since the last call, in order.
    pub fn poll(&mut self) -> Vec<TermEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            match &ev {
                TermEvent::Title(t) => self.title = t.clone(),
                TermEvent::Osc(OscEvent::Title(t)) => self.title = Some(t.clone()),
                TermEvent::Osc(OscEvent::Cwd { path, .. }) => self.cwd = Some(path.clone()),
                TermEvent::Exited(code) => self.exit_code = Some(*code),
                TermEvent::Osc(_) | TermEvent::Bell => {}
            }
            out.push(ev);
        }
        out
    }

    /// Send bytes to the PTY as if typed (run a command, paste a path for an agent).
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        self.write_bytes(bytes.into());
    }

    fn write_bytes(&self, bytes: Vec<u8>) {
        self.notifier.notify(bytes);
    }

    /// The terminal's current selection as plain text, or `None` when nothing is selected
    /// (`Mod+C`, and the app's Edit ▸ Copy).
    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    /// Send `text` to the PTY as a paste, bracketed when the terminal enabled that mode (the
    /// app's Edit ▸ Paste, and dropping a path onto a pane).
    pub fn paste(&self, text: &str) {
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        self.write_bytes(input::paste_bytes(text, bracketed));
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
    /// Last cwd reported by OSC 7.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }
    pub fn exit_code(&self) -> Option<Option<i32>> {
        self.exit_code
    }
    /// Shell pid (for port detection and agent heuristics).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Visible screen as text (scrollback persistence, accessibility, tests).
    // Not wired into the app yet: scrollback persistence (§5.22), `read-screen` (§5.31), §8.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn screen_text(&self) -> String {
        let term = self.term.lock();
        let lines = term.screen_lines();
        let content = term.renderable_content();
        let display_offset = content.display_offset;
        let mut rows = vec![String::new(); lines];
        for indexed in content.display_iter {
            if indexed.cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            if let Some(viewport) = term::point_to_viewport(display_offset, indexed.point)
                && viewport.line < lines
            {
                rows[viewport.line].push(indexed.cell.c);
            }
        }
        rows.iter().map(|r| r.trim_end()).collect::<Vec<_>>().join("\n")
    }
}

impl Drop for TerminalPane {
    /// Shut the event loop down and hang up the PTY. We do not join the reader thread — see
    /// the `event_loop` field doc comment.
    fn drop(&mut self) {
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Poll `pane` for up to 5s until some accumulated event matches `pred`, or panic.
    ///
    /// `seen` accumulates every event across calls (rather than each call draining and
    /// discarding whatever didn't match) so that a batch containing two events the caller wants
    /// to check separately — as happens here, since `printf ...; exit 3` produces a title *and*
    /// an exit within the same poll — doesn't lose the second one: a naive version that drains
    /// and returns on the first match would silently discard it, and a second `wait_for` call
    /// for that event would then wait out its full deadline for an event that will never repeat.
    fn wait_for(pane: &mut TerminalPane, seen: &mut Vec<TermEvent>, pred: impl Fn(&TermEvent) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            seen.extend(pane.poll());
            if seen.iter().any(&pred) {
                return;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for the expected terminal event; saw: {seen:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn spawned_shell_prints_sets_title_and_reports_exit() {
        let ctx = egui::Context::default();
        let spec = Spawn {
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            // portability: allow — a fixed shell path for a hermetic, deterministic test.
            program: Some((
                "/bin/sh".to_string(), // portability: allow
                vec!["-c".to_string(), "printf 'hello\\033]0;T\\007'; exit 3".to_string()],
            )),
            scrollback: 1000,
        };
        let mut pane = TerminalPane::spawn(spec, &ctx).expect("spawn /bin/sh");
        let mut seen = Vec::new();

        wait_for(&mut pane, &mut seen, |ev| matches!(ev, TermEvent::Title(Some(t)) if t == "T"));
        wait_for(&mut pane, &mut seen, |ev| matches!(ev, TermEvent::Exited(Some(3))));

        assert!(pane.screen_text().contains("hello"), "screen was: {:?}", pane.screen_text());
        assert_eq!(pane.title(), Some("T"));
        assert_eq!(pane.exit_code(), Some(Some(3)));
    }

    #[test]
    fn osc7_cwd_arrives_as_osc_event() {
        let ctx = egui::Context::default();
        let spec = Spawn {
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            program: Some((
                "/bin/sh".to_string(), // portability: allow
                vec!["-c".to_string(), "printf '\\033]7;file:///tmp/osc7-test\\007'; sleep 5".to_string()],
            )),
            scrollback: 1000,
        };
        let mut pane = TerminalPane::spawn(spec, &ctx).expect("spawn /bin/sh");
        let mut seen = Vec::new();

        wait_for(&mut pane, &mut seen, |ev| {
            matches!(ev, TermEvent::Osc(OscEvent::Cwd { path, .. }) if path == "/tmp/osc7-test") // portability: allow
        });
        assert_eq!(pane.cwd(), Some("/tmp/osc7-test")); // portability: allow
    }
}
