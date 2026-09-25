//! Unix-socket transport for the control protocol (§5.31).
//!
//! The socket lives at `Dirs::socket()`, mode 0600 in a 0700 directory; filesystem permissions
//! are the only auth. One app instance per user: binding over a socket that a live app answers
//! fails with `ErrorKind::AddrInUse` (the caller then forwards its args and exits); a stale
//! socket file with nobody listening is removed and re-bound.

use super::protocol::{Request, Response};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A running server. Dropping it stops accepting and removes the socket file.
pub struct ServerHandle {
    path: PathBuf,
}

impl ServerHandle {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Bind at `path` and serve on a background thread. Each connection: read one line, call
/// `handler`, write one response line, close. A malformed line gets
/// `{"ok":false,"error":"bad request: …"}`; a slow client times out after 2 s.
pub fn serve<F>(path: &Path, handler: F) -> io::Result<ServerHandle>
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    todo!()
}

/// Send one request and wait for its response. `ConnectionRefused`/`NotFound` mean no app.
pub fn send(path: &Path, req: &Request, timeout: Duration) -> io::Result<Response> {
    todo!()
}
