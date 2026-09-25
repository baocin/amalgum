//! Offline hook-event queue on remote hosts (§5.28 "Hooks from remote agents").
//!
//! While the reverse-forwarded socket is down, notification-type requests are appended to
//! `~/.amalgum/queue.jsonl`. On reconnect the app runs `amalgum drain` remotely, which prints
//! and removes the queued lines. Draining is atomic with respect to concurrent appends: an
//! advisory lock on a stable `<queue>.lock` file serializes "open the queue and write a line"
//! against "rename the queue away", so a rename can never land between a writer's open and its
//! write (which would otherwise orphan that line in an unlinked, never-read inode). The lock is
//! held only around the rename itself, not around reading the renamed-away copy, so drainer and
//! writers never wait on file I/O, only on each other's open+write.

use super::protocol::Request;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Append one request as a JSON line (creating the file 0600 and its directory 0700).
pub fn append(queue: &Path, req: &Request) -> io::Result<()> {
    let _lock = lock(queue)?;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(queue)?;
    file.write_all(req.to_line().as_bytes())
}

/// Take every queued line, oldest first. A missing queue is empty, not an error.
pub fn drain(queue: &Path) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();

    // A crash mid-drain can leave `<queue>.draining.<pid>.<nanos>` files behind; fold their
    // contents in first, oldest first, before the current queue's own contents.
    for leftover in leftover_draining_files(queue) {
        lines.extend(take_lines(&leftover)?);
    }

    let draining = draining_path(queue);
    {
        let _lock = lock(queue)?;
        match std::fs::rename(queue, &draining) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(lines),
            Err(e) => return Err(e),
        }
    } // lock released: reading the (now private) renamed-away copy needs no coordination.
    lines.extend(take_lines(&draining)?);
    Ok(lines)
}

/// Queue only when talking to a reverse-forwarded socket, i.e. `sock` lies inside
/// `~/.amalgum/` on this host. Locally, a missing app just means drop the event.
pub fn should_queue(sock: &Path, home: &Path) -> bool {
    sock.starts_with(crate::paths::remote_root(home))
}

/// Take an exclusive, blocking advisory lock on `<queue>.lock` (creating the queue's directory
/// and the lock file if needed). Held by the returned guard; dropping it releases the lock.
fn lock(queue: &Path) -> io::Result<File> {
    if let Some(parent) = queue.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    let stem = queue.file_name().and_then(|n| n.to_str()).unwrap_or("queue.jsonl");
    let path = queue.with_file_name(format!("{stem}.lock"));
    // Only ever used for `flock`; its content is never read or written.
    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).mode(0o600).open(path)?;
    file.lock()?;
    Ok(file)
}

/// A fresh, never-yet-used name for the file `drain` swaps the queue out to.
fn draining_path(queue: &Path) -> PathBuf {
    let stem = queue.file_name().and_then(|n| n.to_str()).unwrap_or("queue.jsonl");
    let pid = std::process::id();
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    queue.with_file_name(format!("{stem}.draining.{pid}.{nanos}"))
}

/// Leftover `.draining.*` files beside `queue`, oldest first (by filesystem mtime).
fn leftover_draining_files(queue: &Path) -> Vec<PathBuf> {
    let Some(dir) = queue.parent() else { return Vec::new() };
    let Some(stem) = queue.file_name().and_then(|n| n.to_str()) else { return Vec::new() };
    let prefix = format!("{stem}.draining.");

    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix)))
        .map(|e| {
            let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            (mtime, e.path())
        })
        .collect();
    found.sort_by_key(|(mtime, _)| *mtime);
    found.into_iter().map(|(_, path)| path).collect()
}

/// Read `path`'s non-empty lines and delete it. The file must already exist. Bytes that are
/// not UTF-8 (a write torn mid-character) are decoded lossily rather than failing, so the file
/// is always consumed; a damaged line just no longer parses as a request.
fn take_lines(path: &Path) -> io::Result<Vec<String>> {
    let bytes = std::fs::read(path)?;
    let content = String::from_utf8_lossy(&bytes);
    let lines =
        content.lines().map(|l| l.trim_end_matches('\r').to_string()).filter(|l| !l.is_empty()).collect();
    std::fs::remove_file(path)?;
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctl::protocol::Command;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn req(target: &str) -> Request {
        Request::new(Command::Focus { target: target.into() })
    }

    #[test]
    fn missing_queue_drains_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("nope").join("queue.jsonl");
        assert_eq!(drain(&queue).expect("drain"), Vec::<String>::new());
    }

    #[test]
    fn append_creates_dir_0700_and_file_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("remote").join("queue.jsonl");
        append(&queue, &req("t1")).expect("append");

        let dir_mode = std::fs::metadata(queue.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let file_mode = std::fs::metadata(&queue).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn appended_lines_drain_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("queue.jsonl");
        append(&queue, &req("t1")).expect("append 1");
        append(&queue, &req("t2")).expect("append 2");
        append(&queue, &req("t3")).expect("append 3");

        let lines = drain(&queue).expect("drain");
        assert_eq!(lines.len(), 3);
        let targets: Vec<_> = lines
            .iter()
            .map(|l| {
                let r = Request::from_line(l).expect("valid line");
                match r.cmd {
                    Command::Focus { target } => target,
                    other => panic!("unexpected {other:?}"),
                }
            })
            .collect();
        assert_eq!(targets, vec!["t1", "t2", "t3"]);
    }

    #[test]
    fn drain_removes_the_queue_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("queue.jsonl");
        append(&queue, &req("t1")).expect("append");
        drain(&queue).expect("drain");
        assert!(!queue.exists());
    }

    #[test]
    fn second_drain_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("queue.jsonl");
        append(&queue, &req("t1")).expect("append");
        assert_eq!(drain(&queue).expect("first drain").len(), 1);
        assert_eq!(drain(&queue).expect("second drain"), Vec::<String>::new());
    }

    #[test]
    fn append_after_drain_starts_a_fresh_queue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("queue.jsonl");
        append(&queue, &req("t1")).expect("append 1");
        drain(&queue).expect("drain");
        append(&queue, &req("t2")).expect("append 2");
        let lines = drain(&queue).expect("drain 2");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn leftover_draining_file_from_a_crash_is_picked_up_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("queue.jsonl");
        // Simulate a drain that renamed the queue but crashed before reading it.
        let crashed = queue.with_file_name("queue.jsonl.draining.111.222");
        std::fs::write(&crashed, req("stale").to_line()).expect("write leftover");
        append(&queue, &req("fresh")).expect("append current");

        let lines = drain(&queue).expect("drain");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("stale"));
        assert!(lines[1].contains("fresh"));
        assert!(!crashed.exists());
    }

    /// A write cut off mid-character (e.g. on ENOSPC) leaves bytes that are not UTF-8. The file
    /// must still be consumed: every drain reads leftovers first, so failing on it would stop
    /// the live queue from ever being delivered again.
    #[test]
    fn leftover_with_invalid_utf8_is_consumed_not_a_permanent_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("queue.jsonl");
        let crashed = queue.with_file_name("queue.jsonl.draining.111.222");
        let mut bytes = req("stale").to_line().into_bytes();
        bytes.extend_from_slice(b"{\"v\":1,\"cmd\":\"notify\",\"title\":\"caf\xC3");
        std::fs::write(&crashed, bytes).expect("write leftover");
        append(&queue, &req("fresh")).expect("append current");

        let lines = drain(&queue).expect("drain despite the torn write");
        assert!(lines[0].contains("stale"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("fresh")), "{lines:?}");
        assert!(!crashed.exists());

        append(&queue, &req("later")).expect("append after");
        let lines = drain(&queue).expect("next drain");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("later"));
    }

    #[test]
    fn should_queue_only_for_sockets_under_remote_root() {
        let home = Path::new("/home/u"); // portability: allow
        assert!(should_queue(Path::new("/home/u/.amalgum/run/app-1.sock"), home)); // portability: allow
        assert!(!should_queue(Path::new("/run/user/1000/amalgum/app.sock"), home)); // portability: allow
    }

    /// Several threads append concurrently while another drains in a loop; every appended
    /// line must be drained exactly once (never lost, never duplicated).
    #[test]
    fn concurrent_append_and_drain_loses_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("queue.jsonl");
        let writer_count = 8;
        let per_writer = 25;
        let total = writer_count * per_writer;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let writers: Vec<_> = (0..writer_count)
            .map(|w| {
                let queue = queue.clone();
                thread::spawn(move || {
                    for i in 0..per_writer {
                        append(&queue, &req(&format!("w{w}-{i}"))).expect("append");
                    }
                })
            })
            .collect();

        let drainer = {
            let queue = queue.clone();
            let stop = Arc::clone(&stop);
            let collected = Arc::clone(&collected);
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let lines = drain(&queue).unwrap_or_default();
                    collected.lock().unwrap().extend(lines);
                    thread::yield_now();
                }
                // Final sweep after writers are known to be done.
                let lines = drain(&queue).unwrap_or_default();
                collected.lock().unwrap().extend(lines);
            })
        };

        for w in writers {
            w.join().expect("writer thread");
        }
        stop.store(true, Ordering::SeqCst);
        drainer.join().expect("drainer thread");

        let lines = collected.lock().unwrap();
        assert_eq!(lines.len(), total, "every line drained exactly once, none lost or duplicated");

        let mut seen = std::collections::HashSet::new();
        for line in lines.iter() {
            let r = Request::from_line(line).expect("valid line");
            let target = match r.cmd {
                Command::Focus { target } => target,
                other => panic!("unexpected {other:?}"),
            };
            assert!(seen.insert(target), "duplicate line drained");
        }
    }
}
