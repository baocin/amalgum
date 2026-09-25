//! Unix-socket transport for the control protocol (§5.31).
//!
//! The socket lives at `Dirs::socket()`, mode 0600 in a 0700 directory; filesystem permissions
//! are the only auth. One app instance per user: binding over a socket that a live app answers
//! fails with `ErrorKind::AddrInUse` (the caller then forwards its args and exits); a stale
//! socket file with nobody listening is removed and re-bound. Launches take an advisory lock on
//! `<socket>.lock` around that check-and-rebind, so racing launches cannot both win.

use super::protocol::{Request, Response};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// One request line is capped at 1 MiB; anything longer gets an error response instead of
/// growing memory unbounded.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Per-connection read/write timeout, so one slow or silent client can't wedge a handler
/// thread forever.
const CONN_TIMEOUT: Duration = Duration::from_secs(2);

/// `sockaddr_un.sun_path` is 104 bytes on macOS (108 on Linux); we cap to the smaller of the
/// two so a socket bound on Linux isn't unusable if the same path is ever used on macOS.
const MAX_SOCKET_PATH_BYTES: usize = 104;

type Handler = dyn Fn(Request) -> Response + Send + Sync;

/// A running server. Dropping it stops accepting and removes the socket file.
#[derive(Debug)]
pub struct ServerHandle {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// `(dev, ino)` captured right after bind, so `Drop` never deletes a *different* socket
    /// that some later process bound at the same path after this one was replaced.
    identity: (u64, u64),
}

impl ServerHandle {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // The accept loop blocks in `accept()`; connecting to our own socket unblocks it so
        // it can observe `stop` and exit instead of waiting for a real client forever.
        let _ = UnixStream::connect(&self.path);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        if let Ok(meta) = std::fs::metadata(&self.path)
            && (meta.dev(), meta.ino()) == self.identity
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Bind at `path` and serve on a background thread. Each connection: read one line, call
/// `handler`, write one response line, close. A malformed line gets
/// `{"ok":false,"error":"bad request: …"}`; a slow client times out after 2 s.
pub fn serve<F>(path: &Path, handler: F) -> io::Result<ServerHandle>
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    if path.as_os_str().len() >= MAX_SOCKET_PATH_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path too long ({} bytes, max {MAX_SOCKET_PATH_BYTES}): {}",
                path.as_os_str().len(),
                path.display()
            ),
        ));
    }

    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }

    // Launches serialize check-unlink-bind: otherwise two of them that both found the same
    // stale file could each unlink it, the second removing the first's fresh socket, leaving
    // two apps, one listening where nothing can reach it.
    let launch_lock = lock_beside(path)?;
    if path.exists() {
        match UnixStream::connect(path) {
            // A live server answered: this is a real second instance, not a stale file.
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{} is already in use by a running app", path.display()),
                ));
            }
            // Nobody home: leftover from a crash or unclean shutdown.
            Err(_) => {
                if let Err(e) = std::fs::remove_file(path)
                    && e.kind() != io::ErrorKind::NotFound
                {
                    return Err(e);
                }
            }
        }
    }

    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let meta = std::fs::metadata(path)?;
    let identity = (meta.dev(), meta.ino());
    drop(launch_lock); // bound and listening: the next launch's connect now succeeds.

    let stop = Arc::new(AtomicBool::new(false));
    let handler: Arc<Handler> = Arc::new(handler);
    let stop_bg = Arc::clone(&stop);
    let join = thread::spawn(move || accept_loop(listener, &stop_bg, &handler));

    Ok(ServerHandle { path: path.to_path_buf(), stop, join: Some(join), identity })
}

/// An exclusive advisory lock on `<socket>.lock` (0600, created if needed, never removed so
/// every launch locks the same inode), released when the returned file is dropped.
fn lock_beside(socket: &Path) -> io::Result<std::fs::File> {
    let mut name = socket.as_os_str().to_owned();
    name.push(".lock");
    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).mode(0o600).open(name)?;
    file.lock()?;
    Ok(file)
}

fn accept_loop(listener: UnixListener, stop: &AtomicBool, handler: &Arc<Handler>) {
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                // `Drop` connects to unblock a pending `accept()`; check *before* spawning a
                // handler so that wake-up connection is never treated as a real request.
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let handler = Arc::clone(handler);
                thread::spawn(move || handle_connection(stream, handler.as_ref()));
            }
            Err(_) if stop.load(Ordering::SeqCst) => return,
            Err(_) => {
                // Transient accept error (e.g. too many open files): don't spin hot.
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn handle_connection(stream: UnixStream, handler: &Handler) {
    let _ = handle_connection_inner(stream, handler);
}

fn handle_connection_inner(stream: UnixStream, handler: &Handler) -> io::Result<()> {
    stream.set_read_timeout(Some(CONN_TIMEOUT))?;
    stream.set_write_timeout(Some(CONN_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let mut buf = Vec::new();
    // Cap to MAX_LINE_BYTES+1 so we can tell "exactly at the cap, with a newline" apart from
    // "over the cap" without reading unboundedly.
    let n = reader.by_ref().take((MAX_LINE_BYTES + 1) as u64).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(()); // peer closed without sending anything; nothing to answer.
    }

    let response = if buf.len() > MAX_LINE_BYTES {
        Response::err("bad request: line too long")
    } else {
        match std::str::from_utf8(&buf) {
            Ok(line) => match Request::from_line(line) {
                Ok(req) => handler(req),
                Err(e) => Response::err(format!("bad request: {e}")),
            },
            Err(_) => Response::err("bad request: invalid utf-8"),
        }
    };

    writer.write_all(response.to_line().as_bytes())?;
    writer.flush()
}

/// Send one request and wait for its response. `ConnectionRefused`/`NotFound` mean no app.
pub fn send(path: &Path, req: &Request, timeout: Duration) -> io::Result<Response> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(req.to_line().as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut buf = Vec::new();
    BufReader::new(stream).read_until(b'\n', &mut buf)?;
    let line = String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Response::from_line(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctl::protocol::Command;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    fn echo_handler(req: Request) -> Response {
        Response::with_data(serde_json::json!({"seen": req.cmd}))
    }

    #[test]
    fn round_trips_a_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("app.sock");
        let server = serve(&sock, echo_handler).expect("serve");

        let req = Request::new(Command::List);
        let resp = send(server.path(), &req, Duration::from_secs(2)).expect("send");
        assert!(resp.ok);
        assert_eq!(resp.data.unwrap()["seen"]["cmd"], "list");
    }

    #[test]
    fn socket_is_created_with_0600_in_a_0700_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("run").join("app.sock");
        let server = serve(&sock, |_| Response::ok()).expect("serve");

        let sock_mode = std::fs::metadata(server.path()).expect("stat sock").permissions().mode() & 0o777;
        assert_eq!(sock_mode, 0o600);
        let dir_mode =
            std::fs::metadata(sock.parent().unwrap()).expect("stat dir").permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn stale_socket_file_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("app.sock");

        // A file at the path with nobody listening (simulating a crash that left it behind).
        std::fs::write(&sock, b"not a socket").expect("write stale file");
        let server = serve(&sock, |_| Response::ok()).expect("serve replaces stale file");

        let req = Request::new(Command::List);
        let resp = send(server.path(), &req, Duration::from_secs(2)).expect("send");
        assert!(resp.ok);
    }

    /// Launches racing over a socket left by a crash: exactly one may serve, and it must be the
    /// one reachable at the path, not one listening on an inode another launch unlinked.
    #[test]
    fn racing_launches_over_a_stale_socket_leave_exactly_one_reachable_server() {
        for _ in 0..100 {
            let dir = tempfile::tempdir().expect("tempdir");
            let sock = dir.path().join("app.sock");
            drop(UnixListener::bind(&sock).expect("bind")); // the file stays, nobody listens
            let racers = 4;
            let barrier = Arc::new(std::sync::Barrier::new(racers));
            let results: Vec<io::Result<ServerHandle>> = (0..racers)
                .map(|_| {
                    let (sock, barrier) = (sock.clone(), Arc::clone(&barrier));
                    thread::spawn(move || {
                        barrier.wait();
                        serve(&sock, |_| Response::ok())
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().expect("racer thread"))
                .collect();

            let outcome: Vec<Option<io::ErrorKind>> =
                results.iter().map(|r| r.as_ref().err().map(io::Error::kind)).collect();
            let winners = outcome.iter().filter(|k| k.is_none()).count();
            let reachable = send(&sock, &Request::new(Command::List), Duration::from_secs(2)).is_ok();
            let losers_saw_a_live_app = outcome.iter().flatten().all(|k| *k == io::ErrorKind::AddrInUse);
            if winners != 1 || !reachable || !losers_saw_a_live_app {
                // An orphaned server's `Drop` would wait forever on an accept() nobody can reach.
                std::mem::forget(results);
                panic!("racing launches: {outcome:?}, reachable at path: {reachable}");
            }
        }
    }

    #[test]
    fn live_socket_refuses_a_second_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("app.sock");
        let _first = serve(&sock, |_| Response::ok()).expect("first serve");

        let err = serve(&sock, |_| Response::ok()).expect_err("second serve must fail");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    #[test]
    fn malformed_request_gets_a_bad_request_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("app.sock");
        let server = serve(&sock, |_| Response::ok()).expect("serve");

        let mut stream = UnixStream::connect(server.path()).expect("connect");
        stream.write_all(b"not json at all\n").expect("write");
        stream.shutdown(std::net::Shutdown::Write).expect("shutdown write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read");
        let resp = Response::from_line(std::str::from_utf8(&buf).unwrap()).expect("parses as a response");
        assert!(!resp.ok);
        assert!(resp.error.unwrap().starts_with("bad request:"));
    }

    #[test]
    fn oversized_line_gets_an_error_not_a_hang() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("app.sock");
        let server = serve(&sock, |_| Response::ok()).expect("serve");

        let mut stream = UnixStream::connect(server.path()).expect("connect");
        stream.set_write_timeout(Some(Duration::from_secs(5))).expect("timeout");
        let big = vec![b'a'; MAX_LINE_BYTES + 10];
        stream.write_all(&big).expect("write");
        stream.write_all(b"\n").expect("write newline");
        stream.shutdown(std::net::Shutdown::Write).expect("shutdown write");

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read");
        let resp = Response::from_line(std::str::from_utf8(&buf).unwrap()).expect("parses as a response");
        assert!(!resp.ok);
    }

    #[test]
    fn many_concurrent_clients_all_get_answered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("app.sock");
        let count = Arc::new(AtomicUsize::new(0));
        let count_bg = Arc::clone(&count);
        let server = serve(&sock, move |_req| {
            count_bg.fetch_add(1, Ordering::SeqCst);
            Response::ok()
        })
        .expect("serve");

        let path = server.path().to_path_buf();
        let handles: Vec<_> = (0..20)
            .map(|_| {
                let path = path.clone();
                thread::spawn(move || {
                    let req = Request::new(Command::List);
                    send(&path, &req, Duration::from_secs(2)).expect("send").ok
                })
            })
            .collect();

        for h in handles {
            assert!(h.join().expect("client thread"));
        }
        assert_eq!(count.load(Ordering::SeqCst), 20);
    }

    #[test]
    fn drop_removes_the_socket_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("app.sock");
        let server = serve(&sock, |_| Response::ok()).expect("serve");
        assert!(sock.exists());
        drop(server);
        assert!(!sock.exists());
    }

    #[test]
    fn overlong_path_is_a_clear_error_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let long_name = "x".repeat(200);
        let sock = dir.path().join(long_name);
        let err = serve(&sock, |_| Response::ok()).expect_err("over-long path must error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn send_with_no_server_is_not_found_or_connection_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("nobody-here.sock");
        let req = Request::new(Command::List);
        let err = send(&sock, &req, Duration::from_millis(200)).expect_err("no server listening");
        assert!(matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused));
    }

    /// Guards against a lock being needed only in this file's test setup, not production code.
    #[test]
    fn handler_runs_with_shared_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("app.sock");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_bg = Arc::clone(&seen);
        let server = serve(&sock, move |req| {
            seen_bg.lock().unwrap().push(req.cmd);
            Response::ok()
        })
        .expect("serve");

        let req = Request::new(Command::Focus { target: "w1".into() });
        send(server.path(), &req, Duration::from_secs(2)).expect("send");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
