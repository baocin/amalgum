//! End-to-end tests of the ssh engine (§5.28) against a private sshd on this machine
//! (`support/sshd.rs`). Each test skips with a printed reason where sshd cannot run.
//!
//! Run with `cargo test --no-default-features --test ssh`.
//!
//! Headless build only: the tests upload this build's `amalgum` as the remote CLI, and the app
//! build's debug binary is ~0.5 GB (the upload alone outlasts the reconnect deadline). The ssh
//! engine is headless code, so the headless leg of the gate covers all of it.
#![cfg(not(feature = "gui"))]

#[path = "support/sshd.rs"]
mod sshd;

use std::borrow::Cow;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use amalgum::ctl::protocol::{Command, Request, Response};
use amalgum::ctl::{queue, socket};
use amalgum::git::{Git, Location};
use amalgum::ssh::manager::{HostEvent, Manager, Settings};
use amalgum::ssh::runner::{Runner, SystemRunner};
use amalgum::ssh::state::Warning;
use amalgum::ssh::steps::{
    self, CliSource, CliStatus, ConnectParams, Link, MasterOutcome, Outcome, SocketStatus,
};
use amalgum::ssh::{Conn, MasterOptions, TMUX_SERVER};
use sshd::{ALIAS, Options, Sshd, start_or_skip};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const APP_ID: &str = "e2e";

fn conn(sshd: &Sshd) -> Conn {
    Conn::new(ALIAS, sshd.control_dir()).with_config_file(Some(sshd.client_config()))
}

fn link<'a>(conn: &'a Conn) -> Link<'a> {
    Link { runner: &SystemRunner, conn, ssh_bin: "ssh", scp_bin: "scp" }
}

/// The CLI this test build produced stands in for the embedded one: it runs here, and "here"
/// is the remote host.
fn built_cli(_target: &str) -> Option<Cow<'static, [u8]>> {
    std::fs::read(env!("CARGO_BIN_EXE_amalgum")).ok().map(Cow::Owned)
}

fn short_master() -> MasterOptions {
    MasterOptions { persist: "60s".into(), keepalive: Some((20, 2)) }
}

fn params<'a>(source: &'a CliSource, local_socket: &'a Path) -> ConnectParams<'a> {
    ConnectParams {
        master: short_master(),
        trust: false,
        cli_version: VERSION,
        cli_source: source,
        keepalive: true,
        app_id: APP_ID,
        local_socket,
    }
}

fn connect(sshd: &Sshd, source: &CliSource) -> steps::Session {
    let c = conn(sshd);
    let local = sshd.dir().join("app.sock");
    match steps::connect(&link(&c), &params(source, &local), &mut |_| {}) {
        Outcome::Connected(session) => *session,
        other => panic!("connect failed: {other:?}"),
    }
}

/// Start a fake app on `path`; received requests land in the returned list.
fn fake_app(path: &Path) -> (socket::ServerHandle, Arc<Mutex<Vec<Command>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let server = socket::serve(path, move |req| {
        sink.lock().expect("app lock").push(req.cmd);
        Response::ok()
    })
    .expect("serve the app socket");
    (server, seen)
}

fn notify_from_host(c: &Conn, remote_socket: &str, title: &str) {
    let cli = steps::remote_cli(VERSION);
    let sock = format!("AMALGUM_SOCK={remote_socket}");
    link(c).exec(&["env", &sock, &cli, "notify", "--title", title], None).expect("remote amalgum notify");
}

fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn master_opens_and_answers_check() {
    let Some(sshd) = start_or_skip("master_opens_and_answers_check", Options::default()) else { return };
    let c = conn(&sshd);
    let l = link(&c);
    assert!(!l.master_alive());
    assert_eq!(l.open_master(&short_master(), false), Ok(MasterOutcome::Open));
    assert!(l.master_alive());
    assert!(l.master_pid().is_some());
    assert_eq!(l.open_master(&short_master(), false), Ok(MasterOutcome::Open), "a live master is reused");
    let info = l.remote_info().unwrap();
    assert_eq!(Path::new(&info.home), sshd.remote_home());
    assert!(amalgum::ssh::target_for_uname(&info.uname).is_some() || !info.uname.is_empty());
}

#[test]
fn unknown_host_key_is_reported_then_trusted_with_accept_new() {
    let opts = Options { trusted: false, ..Options::default() };
    let Some(sshd) = start_or_skip("unknown_host_key_is_reported_then_trusted_with_accept_new", opts) else {
        return;
    };
    let c = conn(&sshd);
    let l = link(&c);
    let Ok(MasterOutcome::HostKeyUnknown { prompt, ssh_said }) = l.open_master(&short_master(), false) else {
        panic!("expected the unknown-key prompt")
    };
    assert_eq!(prompt.host, ALIAS);
    assert_eq!(prompt.key_type, "ED25519");
    assert_eq!(prompt.fingerprint, sshd.host_fingerprint());
    assert!(ssh_said.contains("Host key verification failed"), "{ssh_said}");
    assert!(!l.master_alive());
    assert!(!sshd.known_hosts().exists(), "nothing accepted without Trust");

    assert_eq!(l.open_master(&short_master(), true), Ok(MasterOutcome::Open));
    assert!(std::fs::read_to_string(sshd.known_hosts()).unwrap().starts_with(ALIAS));
}

/// Regression: without a terminal, ssh asks `SSH_ASKPASS` to confirm an unknown host key when
/// one is configured (common on Linux desktops). The untrusted master must not let it.
#[test]
fn unknown_host_key_is_never_accepted_through_askpass() {
    let opts = Options { trusted: false, ..Options::default() };
    let Some(sshd) = start_or_skip("unknown_host_key_is_never_accepted_through_askpass", opts) else {
        return;
    };
    let d = sshd.dir();
    let (askpass, wrapper, asked) = (d.join("askpass"), d.join("ssh-askpass-env"), d.join("asked"));
    let script = |path: &Path, body: String| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    script(&askpass, format!("printf '%s\\n' \"$*\" >> '{}'\necho yes", asked.display()));
    script(
        &wrapper,
        format!(
            "exec env DISPLAY=:99 SSH_ASKPASS='{}' SSH_ASKPASS_REQUIRE=force ssh \"$@\"",
            askpass.display()
        ),
    );
    let c = conn(&sshd);
    let ssh_bin = wrapper.display().to_string();
    let l = Link { runner: &SystemRunner, conn: &c, ssh_bin: &ssh_bin, scp_bin: "scp" };
    let got = l.open_master(&short_master(), false);
    assert!(matches!(got, Ok(MasterOutcome::HostKeyUnknown { .. })), "{got:?}");
    assert!(!sshd.known_hosts().exists(), "accepted outside W16");
    let asked = std::fs::read_to_string(&asked).unwrap_or_default();
    assert!(!asked.contains("continue connecting"), "askpass was asked: {asked}");
}

/// With ProxyJump, ssh logs the jump host's key first; W16 must show the target's.
#[test]
fn proxy_jump_shows_the_targets_key_and_an_unknown_jump_key_fails() {
    let name = "proxy_jump_shows_the_targets_key_and_an_unknown_jump_key_fails";
    let Some(jump) = start_or_skip(name, Options::default()) else { return };
    let Some(target) = start_or_skip(name, Options { trusted: false, ..Options::default() }) else { return };
    let config = target.dir().join("jump_config");
    let blocks = format!("{}{}  ProxyJump jump\n", jump.host_block("jump"), target.host_block("target"));
    std::fs::write(&config, blocks).unwrap();
    let c = Conn::new("target", target.control_dir()).with_config_file(Some(config.clone()));
    let l = link(&c);
    let got = l.open_master(&short_master(), false);
    let Ok(MasterOutcome::HostKeyUnknown { prompt, .. }) = got else { panic!("{got:?}") };
    assert_eq!((prompt.host.as_str(), prompt.fingerprint), ("target", target.host_fingerprint()));
    assert_ne!(target.host_fingerprint(), jump.host_fingerprint());
    assert_eq!(l.open_master(&short_master(), true), Ok(MasterOutcome::Open));

    // Now the jump host's key is the unknown one: no Trust for the target can fix that.
    std::fs::remove_file(jump.known_hosts()).unwrap();
    let c = Conn::new("target", jump.control_dir()).with_config_file(Some(config));
    let local = jump.dir().join("app.sock");
    for trust in [false, true] {
        let p = ConnectParams { trust, ..params(&built_cli, &local) };
        let outcome = steps::connect(&link(&c), &p, &mut |_| {});
        assert!(
            matches!(outcome, Outcome::Failed { fatal: true, ref error, .. } if error.stderr.contains("jump host")),
            "fails for good instead of retrying or prompting: {outcome:?}"
        );
    }
}

#[test]
fn full_connect_installs_the_cli_and_forwards_the_app_socket() {
    let Some(sshd) =
        start_or_skip("full_connect_installs_the_cli_and_forwards_the_app_socket", Options::default())
    else {
        return;
    };
    let (_app, seen) = fake_app(&sshd.dir().join("app.sock"));
    let mut progress = Vec::new();
    let c = conn(&sshd);
    let local = sshd.dir().join("app.sock");
    let Outcome::Connected(session) =
        steps::connect(&link(&c), &params(&built_cli, &local), &mut |p| progress.push(p))
    else {
        panic!("connect failed")
    };
    assert_eq!(session.cli, CliStatus::Installed);
    assert!(session.tools.git.starts_with("git version"), "{:?}", session.tools);
    assert_eq!(session.socket, SocketStatus::Forwarded);
    let installed = sshd.remote_home().join(format!(".amalgum/bin/{VERSION}/amalgum"));
    assert!(installed.is_file());
    assert!(
        !std::fs::read_dir(installed.parent().unwrap())
            .unwrap()
            .flatten()
            .any(|e| e.file_name() != "amalgum"),
        "no upload leftovers"
    );
    if session.tools.tmux.is_some() {
        assert_eq!(
            std::fs::read_to_string(sshd.remote_home().join(".amalgum/tmux.conf")).unwrap(),
            amalgum::ssh::TMUX_CONF
        );
    }

    // §5.28 Security: only the user can reach the forwarded socket on the host.
    let mode = |p: &Path| {
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(p).unwrap().permissions()) & 0o777
    };
    let remote_socket = Path::new(&session.remote_socket);
    assert_eq!(mode(remote_socket), 0o600, "socket mode");
    assert_eq!(mode(remote_socket.parent().unwrap()), 0o700, "run dir mode");

    // An agent on the host reaches the app through the reverse-forwarded socket.
    notify_from_host(&c, &session.remote_socket, "from the host");
    wait_until("the forwarded notify", || !seen.lock().unwrap().is_empty());
    assert!(matches!(&seen.lock().unwrap()[0], Command::Notify { title, .. } if title == "from the host"));

    // A second connect finds the CLI in place.
    assert_eq!(connect(&sshd, &built_cli).cli, CliStatus::Present);
}

#[test]
fn no_embedded_cli_connects_degraded() {
    let Some(sshd) = start_or_skip("no_embedded_cli_connects_degraded", Options::default()) else { return };
    let c = conn(&sshd);
    let local = sshd.dir().join("app.sock");
    let mut warnings = Vec::new();
    let outcome = steps::connect(&link(&c), &params(&steps::embedded_cli_source, &local), &mut |p| {
        if let steps::Progress::Warning(w) = p {
            warnings.push(w)
        }
    });
    let Outcome::Connected(session) = outcome else { panic!("{outcome:?}") };
    if amalgum::ssh::embedded_cli("x86_64-unknown-linux-musl").is_none() {
        assert!(matches!(session.cli, CliStatus::Unavailable { .. }), "{:?}", session.cli);
        assert!(warnings.iter().any(|w| matches!(w, Warning::CliUnavailable { .. })), "{warnings:?}");
    }
}

#[test]
fn queued_events_are_drained() {
    // No sftp subsystem: the CLI upload falls back to legacy scp.
    let opts = Options { sftp: false, ..Options::default() };
    let Some(sshd) = start_or_skip("queued_events_are_drained", opts) else { return };
    connect(&sshd, &built_cli);
    let req = Request::new(Command::Notify { title: "late".into(), body: None, workspace: None, tab: None });
    queue::append(&amalgum::paths::queue_file(&sshd.remote_home()), &req).unwrap();
    let c = conn(&sshd);
    let drained = link(&c).drain(VERSION).unwrap();
    assert_eq!(drained.events, vec![req]);
    assert!(link(&c).drain(VERSION).unwrap().events.is_empty(), "drain empties the queue");
}

#[test]
fn refused_socket_forward_falls_back_to_queue_mode() {
    let opts = Options { stream_local: false, ..Options::default() };
    let Some(sshd) = start_or_skip("refused_socket_forward_falls_back_to_queue_mode", opts) else { return };
    let c = conn(&sshd);
    let local = sshd.dir().join("app.sock");
    let (_app, seen) = fake_app(&local);
    let mut warnings = Vec::new();
    let outcome = steps::connect(&link(&c), &params(&built_cli, &local), &mut |p| {
        if let steps::Progress::Warning(w) = p {
            warnings.push(w)
        }
    });
    let Outcome::Connected(session) = outcome else { panic!("{outcome:?}") };
    assert!(
        matches!(session.socket, SocketStatus::Refused { ref ssh_said } if ssh_said.contains("forwarding"))
    );
    assert!(warnings.iter().any(|w| matches!(w, Warning::QueueMode { .. })), "{warnings:?}");

    // The hook still exits 0 and its event waits in the queue for the next drain.
    notify_from_host(&c, &session.remote_socket, "queued");
    let drained = link(&c).drain(VERSION).unwrap();
    assert!(
        matches!(&drained.events[..], [Request { cmd: Command::Notify { title, .. }, .. }] if title == "queued")
    );
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn tmux_sessions_use_the_uploaded_conf_and_can_be_killed() {
    let Some(sshd) =
        start_or_skip("tmux_sessions_use_the_uploaded_conf_and_can_be_killed", Options::default())
    else {
        return;
    };
    let session = connect(&sshd, &built_cli);
    let Some(tmux) = session.tools.tmux else {
        eprintln!("skipped: no tmux on this host");
        return;
    };
    assert!(tmux.starts_with("tmux"), "{tmux}");
    let c = conn(&sshd);
    let l = link(&c);
    let conf = "~/.amalgum/tmux.conf";
    l.exec(&["tmux", "-L", TMUX_SERVER, "-f", conf, "new-session", "-d", "-s", "tab-1"], None)
        .expect("tmux starts");
    let has = |l: &Link| l.exec(&["tmux", "-L", TMUX_SERVER, "has-session", "-t", "tab-1"], None).is_ok();
    assert!(has(&l));
    let status = l.exec(&["tmux", "-L", TMUX_SERVER, "show-options", "-g", "status"], None).unwrap();
    assert_eq!(status.stdout_str().trim(), "status off", "the bundled tmux.conf is loaded");
    l.kill_sessions().unwrap();
    assert!(!has(&l));
    l.kill_sessions().expect("killing no sessions is fine");
}

#[test]
fn remote_git_runs_through_the_master_with_the_config_file() {
    let Some(sshd) =
        start_or_skip("remote_git_runs_through_the_master_with_the_config_file", Options::default())
    else {
        return;
    };
    let c = conn(&sshd);
    assert_eq!(link(&c).open_master(&short_master(), false), Ok(MasterOutcome::Open));
    let repo = sshd.remote_home().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut git = Git::new(Location::Remote { host: ALIAS.into(), path: "~/repo".into() });
    git.control_dir = Some(sshd.control_dir());
    git.ssh_config = Some(sshd.client_config());
    // The remote HOME is private, so no global git config applies there.
    git.run(&["init", "-q", "-b", "main"]).expect("remote git init");
    git.run(&["remote", "add", "origin", "git@github.com:acme/conduit.git"]).unwrap();
    let ident = ["-c", "user.name=Ada", "-c", "user.email=ada@example.com", "-c", "commit.gpgsign=false"];
    let mut commit = ident.to_vec();
    commit.extend(["commit", "-q", "--allow-empty", "--no-verify", "-m", "first 'quoted' $(message)"]);
    git.run(&commit).expect("remote commit");
    let subject = git.run(&["log", "-1", "--format=%s"]).unwrap();
    assert_eq!(String::from_utf8_lossy(&subject).trim(), "first 'quoted' $(message)");
    assert!(repo.join(".git").is_dir(), "ran in the remote path");

    let remotes = link(&c).repo_identity("~/repo").unwrap();
    assert_eq!(remotes[0].fetch_url, "git@github.com:acme/conduit.git");
}

#[test]
fn port_forward_reaches_a_port_on_the_host() {
    let Some(sshd) = start_or_skip("port_forward_reaches_a_port_on_the_host", Options::default()) else {
        return;
    };
    let c = conn(&sshd);
    let l = link(&c);
    l.open_master(&short_master(), false).unwrap();

    // "Remote" service: an echo of one line.
    let service = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let remote_port = service.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = service.accept() {
            let mut line = String::new();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let _ = reader.read_line(&mut line);
            let _ = (&stream).write_all(format!("echo {line}").as_bytes());
        }
    });

    let local_port = steps::free_local_port().unwrap();
    l.forward_port(local_port, remote_port).expect("ssh -O forward -L");
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", local_port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(b"ping\n").unwrap();
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply).unwrap();
    assert_eq!(reply, "echo ping\n");

    l.cancel_port(local_port, remote_port).expect("ssh -O cancel -L");
    assert!(std::net::TcpStream::connect(("127.0.0.1", local_port)).is_err(), "forward closed");
}

/// Kill the master behind the manager's back: the probe notices, the host goes Lost, and the
/// reconnect brings back the master and the reverse socket.
#[test]
fn manager_reconnects_after_the_master_dies() {
    let Some(sshd) = start_or_skip("manager_reconnects_after_the_master_dies", Options::default()) else {
        return;
    };
    let local = sshd.dir().join("app.sock");
    let (_app, seen) = fake_app(&local);
    let mut settings = Settings::new(&amalgum::paths::Dirs::under(sshd.dir()), APP_ID);
    settings.control_dir = sshd.control_dir();
    settings.ssh_config = Some(sshd.client_config());
    settings.local_socket = local;
    settings.master = short_master();
    settings.probe_interval = 0.1;
    let runner: Arc<dyn Runner> = Arc::new(SystemRunner);
    let mut m = Manager::new(settings, runner, Arc::new(built_cli), Arc::new(|| {}));

    let start = Instant::now();
    let now = || start.elapsed().as_secs_f64();
    let mut events = Vec::new();
    let mut pump = |m: &mut Manager, what: &str, done: &dyn Fn(&Manager) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(m) {
            events.extend(m.poll(now()));
            let phase = m.machine(ALIAS).map(|x| x.phase().clone());
            assert!(Instant::now() < deadline, "timed out waiting for {what}; now {phase:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    m.acquire(ALIAS, "ws1", "~", now()).unwrap();
    pump(&mut m, "connect", &|m| m.machine(ALIAS).unwrap().is_connected());
    let c = conn(&sshd);
    let first_pid = link(&c).master_pid().expect("master pid");
    let remote_socket = m.session(ALIAS).unwrap().remote_socket.clone();

    // SAFETY: kill(2) takes no pointers; the pid is the master this test's manager started.
    unsafe { libc::kill(first_pid as libc::pid_t, libc::SIGKILL) };
    pump(&mut m, "the drop to be noticed", &|m| !m.machine(ALIAS).unwrap().is_connected());
    pump(&mut m, "reconnect", &|m| m.machine(ALIAS).unwrap().is_connected());
    let second_pid = link(&c).master_pid().expect("a new master");
    assert_ne!(first_pid, second_pid);
    assert!(events.iter().any(|e| matches!(e, HostEvent::RepoIdentity { .. })));

    notify_from_host(&c, &remote_socket, "after reconnect");
    wait_until("the notify through the new forward", || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|cmd| matches!(cmd, Command::Notify { title, .. } if title == "after reconnect"))
    });

    let argv = m.terminal_argv(
        ALIAS,
        &amalgum::ssh::terminal::TabSpawn { workspace_id: "ws1", tab_id: "t1", cwd: "~" },
    );
    assert!(argv.is_some_and(|a| a.iter().any(|w| w == "-t")));
    m.release(ALIAS, "ws1", now());
}
