//! Offline hook-event queue on remote hosts (§5.28 "Hooks from remote agents").
//!
//! While the reverse-forwarded socket is down, notification-type requests are appended to
//! `~/.amalgum/queue.jsonl`. On reconnect the app runs `amalgum drain` remotely, which prints
//! and removes the queued lines. Draining is atomic with respect to concurrent appends: take
//! the file by renaming it, then read the renamed file, so no event is lost or printed twice.

use super::protocol::Request;
use std::io;
use std::path::Path;

/// Append one request as a JSON line (creating the file 0600 and its directory 0700).
pub fn append(queue: &Path, req: &Request) -> io::Result<()> {
    todo!()
}

/// Take every queued line, oldest first. A missing queue is empty, not an error.
pub fn drain(queue: &Path) -> io::Result<Vec<String>> {
    todo!()
}

/// Queue only when talking to a reverse-forwarded socket, i.e. `sock` lies inside
/// `~/.amalgum/` on this host. Locally, a missing app just means drop the event.
pub fn should_queue(sock: &Path, home: &Path) -> bool {
    todo!()
}
