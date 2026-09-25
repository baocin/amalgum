//! Integration test of the built `amalgum` binary (§5.31), exercising it exactly as a shell or
//! an agent hook would: as a subprocess talking to a real (or absent) control socket.
//!
//! Run with `cargo test --no-default-features --test cli`.

use amalgum::ctl::protocol::{Command, Response};
use amalgum::ctl::socket;
use std::process::Command as Proc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_amalgum")
}

#[test]
fn version_flag_prints_and_exits_0() {
    let output = Proc::new(bin()).arg("--version").output().expect("run amalgum --version");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.to_lowercase().contains("amalgum"), "stdout: {stdout}");
}

#[test]
fn notify_delivers_a_request_and_exits_0() {
    let sock_dir = tempfile::tempdir().expect("sock tempdir");
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let sock = sock_dir.path().join("app.sock");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_bg = Arc::clone(&seen);
    let server = socket::serve(&sock, move |req| {
        seen_bg.lock().unwrap().push(req.cmd);
        Response::ok()
    })
    .expect("start fake server");

    let output = Proc::new(bin())
        .args(["notify", "--title", "T", "--body", "12 tests pass"])
        .env("AMALGUM_SOCK", server.path())
        .env("HOME", home_dir.path())
        .output()
        .expect("run amalgum notify");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let got = seen.lock().unwrap();
    assert_eq!(got.len(), 1);
    match &got[0] {
        Command::Notify { title, body, .. } => {
            assert_eq!(title, "T");
            assert_eq!(body.as_deref(), Some("12 tests pass"));
        }
        other => panic!("expected Notify, got {other:?}"),
    }
}

#[test]
fn list_with_no_server_exits_1_with_message() {
    let amalgum_home = tempfile::tempdir().expect("amalgum home tempdir");
    let home_dir = tempfile::tempdir().expect("home tempdir");

    let output = Proc::new(bin())
        .arg("list")
        .env_remove("AMALGUM_SOCK")
        .env("AMALGUM_HOME", amalgum_home.path())
        .env("HOME", home_dir.path())
        .output()
        .expect("run amalgum list");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Amalgum is not running"), "stderr: {stderr}");
    assert!(output.stdout.is_empty(), "no data to print when the app isn't running");
}

#[test]
fn notify_with_no_app_and_no_queue_exits_0() {
    // `AMALGUM_HOME` (where the default socket would live) and `HOME` (where the offline
    // queue would live) are unrelated tempdirs, so the default socket path isn't under
    // `<HOME>/.amalgum` and nothing gets queued (§5.28 `should_queue`) — it's just dropped.
    let amalgum_home = tempfile::tempdir().expect("amalgum home tempdir");
    let home_dir = tempfile::tempdir().expect("home tempdir");

    let output = Proc::new(bin())
        .args(["notify", "--title", "dropped"])
        .env_remove("AMALGUM_SOCK")
        .env("AMALGUM_HOME", amalgum_home.path())
        .env("HOME", home_dir.path())
        .output()
        .expect("run amalgum notify");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(!home_dir.path().join(".amalgum").join("queue.jsonl").exists());
}

#[test]
fn set_status_and_clear_status_round_trip_through_the_socket() {
    let sock_dir = tempfile::tempdir().expect("sock tempdir");
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let sock = sock_dir.path().join("app.sock");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_bg = Arc::clone(&seen);
    let server = socket::serve(&sock, move |req| {
        seen_bg.lock().unwrap().push(req.cmd);
        Response::ok()
    })
    .expect("start fake server");

    let set = Proc::new(bin())
        .args(["set-status", "needs-input", "--message", "waiting"])
        .env("AMALGUM_SOCK", server.path())
        .env("HOME", home_dir.path())
        .output()
        .expect("run amalgum set-status");
    assert!(set.status.success());

    let clear = Proc::new(bin())
        .arg("clear-status")
        .env("AMALGUM_SOCK", server.path())
        .env("HOME", home_dir.path())
        .output()
        .expect("run amalgum clear-status");
    assert!(clear.status.success());

    let got = seen.lock().unwrap();
    assert_eq!(got.len(), 2);
    assert!(matches!(&got[0], Command::SetStatus { .. }));
    assert!(matches!(&got[1], Command::ClearStatus { .. }));
}

#[test]
fn query_error_response_exits_1_with_the_apps_message() {
    let sock_dir = tempfile::tempdir().expect("sock tempdir");
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let sock = sock_dir.path().join("app.sock");
    let server = socket::serve(&sock, |_req| Response::err("unknown tab t9")).expect("start fake server");

    let output = Proc::new(bin())
        .args(["focus", "t9"])
        .env("AMALGUM_SOCK", server.path())
        .env("HOME", home_dir.path())
        .output()
        .expect("run amalgum focus");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown tab t9"), "stderr: {stderr}");
}

#[test]
fn drain_prints_queued_lines_and_empties_the_queue() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    let queue = amalgum::paths::queue_file(home_dir.path());
    let req = amalgum::ctl::protocol::Request::new(Command::Focus { target: "w1".into() });
    amalgum::ctl::queue::append(&queue, &req).expect("seed queue");

    let output = Proc::new(bin())
        .arg("drain")
        .env_remove("AMALGUM_SOCK")
        .env("HOME", home_dir.path())
        .output()
        .expect("run amalgum drain");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("w1"), "stdout: {stdout}");
    assert!(!queue.exists());

    // A second drain has nothing left to print.
    let output2 = Proc::new(bin())
        .arg("drain")
        .env_remove("AMALGUM_SOCK")
        .env("HOME", home_dir.path())
        .output()
        .expect("run amalgum drain (2nd)");
    assert!(output2.status.success());
    assert!(output2.stdout.is_empty());
}

/// Sanity check that the fake server used above actually answers within the process's own
/// lifetime (guards against a flaky setup masking every other test in this file as false
/// positives if `serve`/`send` ever regress together).
#[test]
fn fake_server_smoke_test() {
    let sock_dir = tempfile::tempdir().expect("sock tempdir");
    let sock = sock_dir.path().join("app.sock");
    let server = socket::serve(&sock, |_req| Response::ok()).expect("serve");
    let req = amalgum::ctl::protocol::Request::new(Command::List);
    let resp = socket::send(server.path(), &req, Duration::from_secs(2)).expect("send");
    assert!(resp.ok);
}
