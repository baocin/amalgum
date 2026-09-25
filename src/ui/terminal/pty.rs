//! `ScanningPty`: wraps alacritty's `tty::Pty`, delegating registration, resize, and child
//! events, but exposing a reader that feeds each chunk through `agent::osc::Scanner` and sends
//! the resulting events on a channel before alacritty's parser sees the bytes.
//!
//! The reader is a `try_clone` of the PTY master (same open file description, so the
//! non-blocking flag and poller readiness are shared with the registered fd).
