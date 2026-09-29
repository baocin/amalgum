//! One ControlMaster per host, shared by every workspace on it (§5.28 "Multiple hosts").
//!
//! The UI owns a [`Manager`] and never blocks on it: every ssh command runs on a worker thread
//! and reports back over a channel; [`Manager::poll`] (call it each frame, or whenever the
//! `wake` callback fires) feeds the results to each host's [`Machine`], runs its actions,
//! probes live masters every [`Settings::probe_interval`] seconds (`ssh -O check`, off-thread),
//! and returns the [`HostEvent`]s the UI shows. Hosts are refcounted by workspace: the first
//! [`acquire`](Manager::acquire) connects, the last [`release`](Manager::release) disconnects
//! (closing forwards, keeping the master until ControlPersist and tmux sessions on the host).
//! A released host whose connect attempt is still running stays (hidden) until that attempt
//! reports, so what it forwarded is closed and what it drained is delivered.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use super::runner::Runner;
use super::state::{Action, Event, Machine, Phase, Warning};
use super::steps::{self, CliSource, ConnectParams, Drained, Link, Outcome, Progress, Session, SocketStatus};
use super::terminal::{self, TabSpawn};
use super::{Conn, MasterOptions};
use crate::git::refs::Remote;
use crate::git::{Git, GitError};

/// Host settings (Settings → SSH) and where things live locally.
#[derive(Debug, Clone)]
pub struct Settings {
    pub ssh_bin: String,
    pub scp_bin: String,
    /// `Dirs::ssh_control_dir`.
    pub control_dir: PathBuf,
    /// `ssh -F <file>`; `None` uses `~/.ssh/config`.
    pub ssh_config: Option<PathBuf>,
    pub master: MasterOptions,
    /// Session survival through tmux when the host has it.
    pub keepalive: bool,
    /// Names the reverse socket on hosts: `~/.amalgum/run/<app_id>.sock`.
    pub app_id: String,
    /// The app's control socket (`Dirs::socket`).
    pub local_socket: PathBuf,
    /// The CLI version hosts must run; the app's own.
    pub cli_version: String,
    /// Seconds between liveness probes of a connected master.
    pub probe_interval: f64,
    /// Longest reconnect delay, seconds.
    pub backoff_cap: u64,
}

impl Settings {
    pub fn new(dirs: &crate::paths::Dirs, app_id: impl Into<String>) -> Self {
        Self {
            ssh_bin: "ssh".into(),
            scp_bin: "scp".into(),
            control_dir: dirs.ssh_control_dir(),
            ssh_config: None,
            master: MasterOptions::default(),
            keepalive: true,
            app_id: app_id.into(),
            local_socket: dirs.socket(),
            cli_version: env!("CARGO_PKG_VERSION").into(),
            probe_interval: 5.0,
            backoff_cap: 60,
        }
    }
}

/// What the UI needs to hear about. After [`HostEvent::Changed`], read
/// [`Manager::machine`] for the progress line, banner, dialog, or error.
#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    Changed {
        host: String,
    },
    /// Show once as a toast (a repeat of the same warning for the same host is not sent).
    Warning {
        host: String,
        warning: Warning,
    },
    /// Hook events that queued on the host while its socket was unreachable, oldest first.
    Drained {
        host: String,
        drained: Drained,
    },
    RepoIdentity {
        host: String,
        workspace: String,
        result: Result<Vec<Remote>, GitError>,
    },
    /// `result` is the local port the remote port now opens on.
    PortForwarded {
        host: String,
        remote_port: u16,
        result: Result<u16, GitError>,
    },
    PortClosed {
        host: String,
        local_port: u16,
        result: Result<(), GitError>,
    },
    SessionsKilled {
        host: String,
        result: Result<(), GitError>,
    },
}

enum Msg {
    Progress { host: String, epoch: u64, progress: Progress },
    Done { host: String, epoch: u64, outcome: Outcome },
    Probe { host: String, epoch: u64, alive: bool },
    Forwarded { host: String, local_port: u16, remote_port: u16, result: Result<(), GitError> },
    Event(HostEvent),
}

struct Host {
    machine: Machine,
    conn: Conn,
    /// Workspace id → remote repo path.
    workspaces: BTreeMap<String, String>,
    session: Option<Session>,
    /// `(local, remote)` port forwards open on the master.
    forwards: Vec<(u16, u16)>,
    warned: HashSet<String>,
    probing: bool,
    next_probe: f64,
    /// Epoch of the connect attempt running on a worker.
    in_flight: Option<u64>,
}

impl Host {
    /// Still held by a workspace (else only kept until `in_flight` reports).
    fn held(&self) -> bool {
        !self.workspaces.is_empty()
    }
}

pub struct Manager {
    settings: Arc<Settings>,
    runner: Arc<dyn Runner>,
    cli_source: Arc<CliSource>,
    wake: Arc<dyn Fn() + Send + Sync>,
    hosts: BTreeMap<String, Host>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Manager {
    /// `wake` is called from worker threads whenever a result is waiting (egui:
    /// `request_repaint`).
    pub fn new(
        settings: Settings,
        runner: Arc<dyn Runner>,
        cli_source: Arc<CliSource>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        Self { settings: Arc::new(settings), runner, cli_source, wake, hosts: BTreeMap::new(), tx, rx }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn machine(&self, host: &str) -> Option<&Machine> {
        self.held(host).map(|h| &h.machine)
    }

    /// What the last successful connect learned (home, tools, socket, CLI).
    pub fn session(&self, host: &str) -> Option<&Session> {
        self.held(host).and_then(|h| h.session.as_ref())
    }

    pub fn hosts(&self) -> impl Iterator<Item = &str> {
        self.hosts.iter().filter(|(_, h)| h.held()).map(|(k, _)| k.as_str())
    }

    fn held(&self, host: &str) -> Option<&Host> {
        self.hosts.get(host).filter(|h| h.held())
    }

    /// Open (or share) the host's connection for a workspace at `path` on it. Its repo
    /// identity arrives as [`HostEvent::RepoIdentity`] once connected.
    pub fn acquire(&mut self, host: &str, workspace: &str, path: &str, now: f64) -> Result<(), String> {
        if !super::valid_host(host) {
            return Err(format!("{host:?} is not a usable ssh host"));
        }
        let settings = Arc::clone(&self.settings);
        let entry = self.hosts.entry(host.to_string()).or_insert_with(|| Host {
            machine: Machine::new(host, settings.cli_version.clone(), settings.backoff_cap),
            conn: Conn::new(host, settings.control_dir.clone()).with_config_file(settings.ssh_config.clone()),
            workspaces: BTreeMap::new(),
            session: None,
            forwards: Vec::new(),
            warned: HashSet::new(),
            probing: false,
            next_probe: 0.0,
            in_flight: None,
        });
        entry.workspaces.insert(workspace.to_string(), path.to_string());
        if entry.machine.is_connected() {
            self.identify(host, workspace);
        } else {
            self.send(host, Event::Connect, now);
        }
        Ok(())
    }

    /// Drop a workspace's hold on the host; the last one disconnects it.
    pub fn release(&mut self, host: &str, workspace: &str, now: f64) {
        let Some(entry) = self.hosts.get_mut(host) else { return };
        entry.workspaces.remove(workspace);
        if entry.workspaces.is_empty() {
            self.send(host, Event::Disconnect, now);
            self.reap();
        }
    }

    /// **Trust** in the W16 host-key dialog.
    pub fn trust(&mut self, host: &str, now: f64) {
        self.send(host, Event::Trust, now);
    }

    /// **Cancel** on the progress line or host-key dialog.
    pub fn cancel(&mut self, host: &str, now: f64) {
        self.send(host, Event::Cancel, now);
    }

    /// **Retry now** / **Retry** / **Reconnect**.
    pub fn retry_now(&mut self, host: &str, now: f64) {
        self.send(host, Event::RetryNow, now);
    }

    /// **Disconnect** in the row menu: every workspace on the host goes offline.
    pub fn disconnect(&mut self, host: &str, now: f64) {
        self.send(host, Event::Disconnect, now);
    }

    /// Check the master now (e.g. after a remote git command failed with ssh's 255).
    pub fn probe(&mut self, host: &str) {
        let Some(entry) = self.hosts.get_mut(host) else { return };
        if entry.probing || !entry.machine.is_connected() {
            return;
        }
        entry.probing = true;
        let (conn, epoch) = (entry.conn.clone(), entry.machine.epoch());
        let queue_mode =
            entry.session.as_ref().is_some_and(|s| matches!(s.socket, SocketStatus::Refused { .. }));
        let host = host.to_string();
        self.work(move |w| {
            let alive = w.link(&conn).master_alive();
            // In queue mode nothing reaches the app live: collect the queue on every probe.
            if alive
                && queue_mode
                && let Ok(drained) = w.link(&conn).drain(&w.settings.cli_version)
                && (!drained.events.is_empty() || !drained.invalid.is_empty())
            {
                w.send(Msg::Event(HostEvent::Drained { host: host.clone(), drained }));
            }
            w.send(Msg::Probe { host, epoch, alive });
        });
    }

    /// Apply worker results and due timers. Returns what the UI should show.
    pub fn poll(&mut self, now: f64) -> Vec<HostEvent> {
        let mut out = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            self.apply(msg, now, &mut out);
        }
        self.reap();
        let hosts: Vec<String> = self.hosts.keys().cloned().collect();
        for host in hosts {
            if let Some(action) = self.hosts.get_mut(&host).and_then(|h| h.machine.handle(Event::Tick, now)) {
                self.perform(&host, action);
                out.push(HostEvent::Changed { host: host.clone() });
            }
            let due = self.hosts.get(&host).is_some_and(|h| h.machine.is_connected() && now >= h.next_probe);
            if due {
                if let Some(h) = self.hosts.get_mut(&host) {
                    h.next_probe = now + self.settings.probe_interval;
                }
                self.probe(&host);
            }
        }
        out
    }

    /// The argv for a terminal tab on `host`, or `None` until the host is connected.
    pub fn terminal_argv(&self, host: &str, tab: &TabSpawn) -> Option<Vec<String>> {
        let entry = self.hosts.get(host).filter(|h| h.machine.is_connected())?;
        let session = entry.session.as_ref()?;
        let tmux = session.use_tmux(self.settings.keepalive);
        Some(terminal::terminal_argv(&self.settings.ssh_bin, &entry.conn, &session.remote_socket, tab, tmux))
    }

    /// A git runner for `path` on `host`, through its master.
    pub fn git(&self, host: &str, path: &str) -> Option<Git> {
        let entry = self.hosts.get(host)?;
        Some(
            Link { runner: &*self.runner, conn: &entry.conn, ssh_bin: &self.settings.ssh_bin, scp_bin: "" }
                .git(path),
        )
    }

    /// Open `remote_port` on a free local port (W18). Answers with [`HostEvent::PortForwarded`].
    pub fn forward_port(&mut self, host: &str, remote_port: u16) {
        let Some(conn) = self.hosts.get(host).map(|h| h.conn.clone()) else { return };
        let host = host.to_string();
        self.work(move |w| {
            let (local_port, result) = match steps::free_local_port() {
                Ok(port) => (port, w.link(&conn).forward_port(port, remote_port)),
                Err(e) => (
                    0,
                    Err(GitError { command: "bind 127.0.0.1:0".into(), code: None, stderr: e.to_string() }),
                ),
            };
            w.send(Msg::Forwarded { host, local_port, remote_port, result });
        });
    }

    /// **Stop** in the ports popover.
    pub fn cancel_port(&mut self, host: &str, local_port: u16) {
        let Some(entry) = self.hosts.get_mut(host) else { return };
        let Some(i) = entry.forwards.iter().position(|(l, _)| *l == local_port) else { return };
        let (local, remote) = entry.forwards.remove(i);
        let (conn, host) = (entry.conn.clone(), host.to_string());
        self.work(move |w| {
            let result = w.link(&conn).cancel_port(local, remote);
            w.send(Msg::Event(HostEvent::PortClosed { host, local_port: local, result }));
        });
    }

    /// **Kill sessions** (after the §5.20 confirm): `tmux -L amalgum kill-server` on the host.
    pub fn kill_sessions(&mut self, host: &str) {
        let Some(conn) = self.hosts.get(host).map(|h| h.conn.clone()) else { return };
        let host = host.to_string();
        self.work(move |w| {
            let result = w.link(&conn).kill_sessions();
            w.send(Msg::Event(HostEvent::SessionsKilled { host, result }));
        });
    }

    /// Forget released hosts once no attempt of theirs is still running.
    fn reap(&mut self) {
        self.hosts.retain(|_, h| h.held() || h.in_flight.is_some());
    }

    fn send(&mut self, host: &str, event: Event, now: f64) {
        let Some(entry) = self.hosts.get_mut(host) else { return };
        if let Some(action) = entry.machine.handle(event, now) {
            self.perform(host, action);
        }
    }

    fn perform(&mut self, host: &str, action: Action) {
        let Some(entry) = self.hosts.get_mut(host) else { return };
        let conn = entry.conn.clone();
        match action {
            Action::Start { epoch, trust } => {
                entry.in_flight = Some(epoch);
                let (host, source) = (host.to_string(), Arc::clone(&self.cli_source));
                self.work(move |w| {
                    let settings = &w.settings;
                    let params = ConnectParams {
                        master: settings.master.clone(),
                        trust,
                        cli_version: &settings.cli_version,
                        cli_source: &*source,
                        keepalive: settings.keepalive,
                        app_id: &settings.app_id,
                        local_socket: &settings.local_socket,
                    };
                    let outcome = steps::connect(&w.link(&conn), &params, &mut |progress| {
                        w.send(Msg::Progress { host: host.clone(), epoch, progress });
                    });
                    w.send(Msg::Done { host, epoch, outcome });
                });
            }
            Action::CloseForwards => {
                let forwards = std::mem::take(&mut entry.forwards);
                let socket = entry.session.take().map(|s| s.remote_socket);
                // Failures are expected here (the master may be gone) and change nothing.
                self.work(move |w| {
                    if let Some(socket) = socket {
                        let _ = w.link(&conn).cancel_socket(&socket);
                    }
                    for (local, remote) in forwards {
                        let _ = w.link(&conn).cancel_port(local, remote);
                    }
                });
            }
        }
    }

    fn identify(&self, host: &str, workspace: &str) {
        let Some(entry) = self.hosts.get(host) else { return };
        let Some(path) = entry.workspaces.get(workspace).cloned() else { return };
        let (conn, host, workspace) = (entry.conn.clone(), host.to_string(), workspace.to_string());
        self.work(move |w| {
            let result = w.link(&conn).repo_identity(&path);
            w.send(Msg::Event(HostEvent::RepoIdentity { host, workspace, result }));
        });
    }

    fn apply(&mut self, msg: Msg, now: f64, out: &mut Vec<HostEvent>) {
        let (host, event) = match msg {
            Msg::Event(event) => return out.push(event),
            Msg::Progress { host, epoch, progress: Progress::Step(step) } => {
                (host, Event::StepStarted { epoch, step })
            }
            Msg::Progress { host, epoch, progress: Progress::Warning(warning) } => {
                if let Some(entry) = self.hosts.get_mut(&host)
                    && epoch == entry.machine.epoch()
                    && entry.warned.insert(warning.text(&host))
                {
                    out.push(HostEvent::Warning { host: host.clone(), warning: warning.clone() });
                }
                (host, Event::Warning { epoch, warning })
            }
            Msg::Done { host, epoch, outcome } => {
                if let Some(entry) = self.hosts.get_mut(&host)
                    && entry.in_flight == Some(epoch)
                {
                    entry.in_flight = None;
                }
                match outcome {
                    Outcome::Connected(mut session) => {
                        // A cancelled attempt's session is kept too, so the CloseForwards its late
                        // success triggers knows which socket to close; what it drained is real.
                        if let Some(entry) = self.hosts.get_mut(&host)
                            && (epoch == entry.machine.epoch()
                                || *entry.machine.phase() == Phase::Disconnected)
                        {
                            let drained = std::mem::take(&mut session.drained);
                            entry.session = Some(*session);
                            entry.next_probe = now + self.settings.probe_interval;
                            if !drained.events.is_empty() || !drained.invalid.is_empty() {
                                out.push(HostEvent::Drained { host: host.clone(), drained });
                            }
                        }
                        (host, Event::Connected { epoch })
                    }
                    Outcome::HostKeyUnknown { prompt, ssh_said } => {
                        (host, Event::HostKeyUnknown { epoch, prompt, ssh_said })
                    }
                    Outcome::Failed { step, error, fatal } => {
                        (host, Event::Failed { epoch, step, error, fatal })
                    }
                }
            }
            Msg::Probe { host, epoch, alive } => {
                let Some(entry) = self.hosts.get_mut(&host) else { return };
                entry.probing = false;
                if alive || epoch != entry.machine.epoch() || !entry.machine.is_connected() {
                    return;
                }
                // `-L` forwards died with the master; the reconnect does not reopen them.
                for (local_port, _) in std::mem::take(&mut entry.forwards) {
                    out.push(HostEvent::PortClosed {
                        host: host.clone(),
                        local_port,
                        result: Err(GitError {
                            command: "ssh -O check".into(),
                            code: None,
                            stderr: format!("the connection to {host} was lost"),
                        }),
                    });
                }
                (host, Event::MasterDied)
            }
            Msg::Forwarded { host, local_port, remote_port, result } => {
                if result.is_ok() {
                    match self.hosts.get_mut(&host).filter(|h| h.machine.is_connected()) {
                        Some(entry) => entry.forwards.push((local_port, remote_port)),
                        // Disconnected meanwhile: nobody would ever close it.
                        None => {
                            let conn = Conn::new(host.as_str(), self.settings.control_dir.clone())
                                .with_config_file(self.settings.ssh_config.clone());
                            self.work(move |w| {
                                let _ = w.link(&conn).cancel_port(local_port, remote_port);
                            });
                        }
                    }
                }
                return out.push(HostEvent::PortForwarded {
                    host,
                    remote_port,
                    result: result.map(|()| local_port),
                });
            }
        };
        let Some(entry) = self.hosts.get_mut(&host) else { return };
        let was_connected = entry.machine.is_connected();
        let before = entry.machine.phase().clone();
        let action = entry.machine.handle(event, now);
        let connected_now = !was_connected && entry.machine.is_connected();
        let changed = *entry.machine.phase() != before;
        if connected_now {
            let workspaces: Vec<String> = entry.workspaces.keys().cloned().collect();
            for workspace in workspaces {
                self.identify(&host, &workspace);
            }
        }
        if let Some(action) = action {
            self.perform(&host, action);
        }
        if changed {
            out.push(HostEvent::Changed { host });
        }
    }

    /// Run `job` on a worker thread; `wake` fires with every message it sends.
    fn work(&self, job: impl FnOnce(&Worker) + Send + 'static) {
        let worker = Worker {
            runner: Arc::clone(&self.runner),
            settings: Arc::clone(&self.settings),
            tx: self.tx.clone(),
            wake: Arc::clone(&self.wake),
        };
        std::thread::spawn(move || job(&worker));
    }
}

/// What a worker thread holds.
struct Worker {
    runner: Arc<dyn Runner>,
    settings: Arc<Settings>,
    tx: Sender<Msg>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Worker {
    fn link<'a>(&'a self, conn: &'a Conn) -> Link<'a> {
        Link { runner: &*self.runner, conn, ssh_bin: &self.settings.ssh_bin, scp_bin: &self.settings.scp_bin }
    }

    fn send(&self, msg: Msg) {
        // The manager is gone (the app is quitting): nobody is left to tell.
        if self.tx.send(msg).is_ok() {
            (self.wake)();
        }
    }
}

impl Drop for Manager {
    /// The app is quitting: close forwards (the masters persist on their own, §5.28).
    fn drop(&mut self) {
        let hosts: Vec<String> = self.hosts.keys().cloned().collect();
        for host in hosts {
            self.send(&host, Event::Disconnect, 0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::runner::Output;
    use crate::ssh::state::Step;
    use crate::ssh::steps::tests::{FakeRunner, out};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// A host whose master is alive only after `-N -f` ran (until a test clears `alive`), and
    /// whose host key is unknown until a connection uses accept-new when `known` starts false.
    struct FakeHost {
        inner: FakeRunner,
        alive: AtomicBool,
        known: AtomicBool,
        /// While set, the reverse socket forward waits (an attempt stays in flight).
        hold: AtomicBool,
    }

    impl FakeHost {
        fn new(known: bool) -> Arc<Self> {
            Self::with(known, |r| r)
        }

        fn with(known: bool, extra: impl FnOnce(FakeRunner) -> FakeRunner) -> Arc<Self> {
            let inner = FakeRunner::default()
                .on("printf", out(0, "/home/u\nLinux x86_64\n", "")) // portability: allow
                .on("amalgum --version", out(0, "amalgum 0.4.1\n", ""))
                .on("git --version", out(0, "git version 2.43.0\n", ""))
                .on("tmux -V", out(127, "", "sh: tmux: not found\n"))
                .on(
                    "remote -v",
                    out(
                        0,
                        "origin\tgit@github.com:a/b.git (fetch)\norigin\tgit@github.com:a/b.git (push)\n",
                        "",
                    ),
                );
            let inner = extra(inner);
            Arc::new(Self {
                inner,
                alive: AtomicBool::new(false),
                known: AtomicBool::new(known),
                hold: AtomicBool::new(false),
            })
        }

        fn count(&self, pattern: &str) -> usize {
            self.inner.commands().iter().filter(|c| c.contains(pattern)).count()
        }
    }

    impl Runner for FakeHost {
        fn run(&self, argv: &[String], stdin: Option<&[u8]>) -> std::io::Result<Output> {
            let line = argv.join(" ");
            while line.contains("-O forward -R") && self.hold.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
            let result = self.inner.run(argv, stdin)?;
            let alive = self.alive.load(Ordering::SeqCst);
            Ok(if line.contains("-O check") {
                if alive { out(0, "", "Master running (pid=1)\n") } else { out(255, "", "No such file\n") }
            } else if line.contains("-N -f") {
                if line.contains("accept-new") {
                    self.known.store(true, Ordering::SeqCst);
                }
                if !self.known.load(Ordering::SeqCst) {
                    return Ok(out(255, "", "Host key verification failed.\n"));
                }
                self.alive.store(true, Ordering::SeqCst);
                out(0, "", "")
            } else if line.contains(" -G ") {
                out(0, "hostname 10.0.0.5\nport 22\n", "")
            } else if line.contains("BatchMode=yes") {
                out(
                    255,
                    "",
                    "debug1: Authenticating to 10.0.0.5:22 as 'u'\ndebug1: Server host key: ssh-ed25519 SHA256:abc\n",
                )
            } else if !alive && line.contains("ControlPath=") {
                out(255, "", "Control socket connect: No such file\n")
            } else {
                result
            })
        }
    }

    fn manager(host: &Arc<FakeHost>) -> (Manager, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::new(&crate::paths::Dirs::under(dir.path()), "app1");
        settings.cli_version = "0.4.1".into();
        settings.probe_interval = 0.0;
        let runner: Arc<dyn Runner> = Arc::clone(host) as Arc<dyn Runner>;
        let source: Arc<CliSource> = Arc::new(steps::embedded_cli_source);
        (Manager::new(settings, runner, source, Arc::new(|| {})), dir)
    }

    /// Poll until `done` holds (at most 5 s), collecting events; the fake clock gains 1 s a round.
    fn pump(m: &mut Manager, now: &mut f64, done: impl Fn(&Manager, &[HostEvent]) -> bool) -> Vec<HostEvent> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut events = Vec::new();
        loop {
            events.extend(m.poll(*now));
            if done(m, &events) {
                return events;
            }
            let phase = m.machine("gpu-box").map(|x| x.phase().clone());
            assert!(Instant::now() < deadline, "timed out in {phase:?}; events {events:#?}");
            std::thread::sleep(Duration::from_millis(5));
            *now += 1.0;
        }
    }

    fn connected(m: &Manager, _: &[HostEvent]) -> bool {
        m.machine("gpu-box").is_some_and(Machine::is_connected)
    }

    fn identities(events: &[HostEvent]) -> Vec<String> {
        let mut ws: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                HostEvent::RepoIdentity { workspace, result: Ok(remotes), .. } if remotes.len() == 1 => {
                    Some(workspace.clone())
                }
                _ => None,
            })
            .collect();
        ws.sort();
        ws
    }

    fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn workspaces_on_one_host_share_one_master() {
        let host = FakeHost::new(true);
        let (mut m, _dir) = manager(&host);
        let mut now = 0.0;
        m.acquire("gpu-box", "ws1", "~/a", now).unwrap();
        m.acquire("gpu-box", "ws2", "~/b", now).unwrap();
        let events = pump(&mut m, &mut now, |m, ev| connected(m, ev) && identities(ev).len() == 2);
        assert_eq!(identities(&events), ["ws1", "ws2"]);
        assert_eq!(host.count("-N -f"), 1, "{:#?}", host.inner.commands());
        assert!(events.contains(&HostEvent::Changed { host: "gpu-box".into() }));

        m.acquire("gpu-box", "ws3", "~/c", now).unwrap();
        let events = pump(&mut m, &mut now, |_, ev| identities(ev) == ["ws3"]);
        assert_eq!(identities(&events), ["ws3"], "a late workspace is identified at once");
        assert_eq!(host.count("-N -f"), 1);

        m.release("gpu-box", "ws1", now);
        m.release("gpu-box", "ws2", now);
        assert!(connected(&m, &[]), "still held by ws3");
        m.release("gpu-box", "ws3", now);
        assert!(m.machine("gpu-box").is_none());
        let cancel = "-O cancel -R /home/u/.amalgum/run/app1.sock"; // portability: allow
        wait_for("the reverse forward to close", || host.count(cancel) == 1);
        assert_eq!(host.count("-O exit"), 0, "Disconnect keeps the master until ControlPersist");
    }

    #[test]
    fn a_dead_master_is_detected_and_reconnected() {
        let host = FakeHost::new(true);
        let (mut m, _dir) = manager(&host);
        let mut now = 0.0;
        m.acquire("gpu-box", "ws1", "~/a", now).unwrap();
        let events = pump(&mut m, &mut now, connected);
        let warnings = events.iter().filter(|e| matches!(e, HostEvent::Warning { .. })).count();
        assert_eq!(warnings, 1, "keepalive on a host without tmux: {events:#?}");

        host.alive.store(false, Ordering::SeqCst);
        let events = pump(&mut m, &mut now, |m, _| !m.machine("gpu-box").unwrap().is_connected());
        assert!(events.contains(&HostEvent::Changed { host: "gpu-box".into() }));
        let events = pump(&mut m, &mut now, connected);
        assert_eq!(host.count("-N -f"), 2);
        assert!(!events.iter().any(|e| matches!(e, HostEvent::Warning { .. })), "shown once: {events:#?}");
    }

    #[test]
    fn unknown_host_key_waits_for_trust() {
        let host = FakeHost::new(false);
        let (mut m, _dir) = manager(&host);
        let mut now = 0.0;
        m.acquire("gpu-box", "ws1", "~/a", now).unwrap();
        pump(&mut m, &mut now, |m, _| {
            matches!(m.machine("gpu-box").unwrap().phase(), Phase::HostKeyUnknown { .. })
        });
        let Phase::HostKeyUnknown { prompt, .. } = m.machine("gpu-box").unwrap().phase().clone() else {
            unreachable!()
        };
        assert_eq!(prompt.fingerprint, "SHA256:abc");
        assert!(
            m.terminal_argv("gpu-box", &TabSpawn { workspace_id: "ws1", tab_id: "t", cwd: "~/a" }).is_none()
        );
        m.trust("gpu-box", now);
        pump(&mut m, &mut now, connected);
        assert_eq!(host.count("StrictHostKeyChecking=accept-new"), 1);
        assert_eq!(host.count("StrictHostKeyChecking=no"), 0);
    }

    #[test]
    fn terminals_ports_and_sessions_once_connected() {
        let host = FakeHost::new(true);
        let (mut m, _dir) = manager(&host);
        let mut now = 0.0;
        m.acquire("gpu-box", "ws1", "~/a", now).unwrap();
        pump(&mut m, &mut now, connected);

        let argv =
            m.terminal_argv("gpu-box", &TabSpawn { workspace_id: "ws1", tab_id: "t1", cwd: "~/a" }).unwrap();
        let want = "cd ~/a && exec env AMALGUM_SOCK=/home/u/.amalgum/run/app1.sock"; // portability: allow
        assert!(argv.last().unwrap().starts_with(want), "no tmux on this host: {argv:?}");
        let git = m.git("gpu-box", "~/a").unwrap();
        assert!(git.argv(&["status"]).iter().any(|a| a.starts_with("ControlPath=")));

        m.forward_port("gpu-box", 3000);
        let events =
            pump(&mut m, &mut now, |_, ev| ev.iter().any(|e| matches!(e, HostEvent::PortForwarded { .. })));
        let forwarded = events.iter().find(|e| matches!(e, HostEvent::PortForwarded { .. }));
        let Some(HostEvent::PortForwarded { remote_port: 3000, result: Ok(local), .. }) = forwarded.cloned()
        else {
            panic!("{events:#?}")
        };
        assert_eq!(host.count(&format!("-O forward -L {local}:localhost:3000")), 1);
        m.cancel_port("gpu-box", local);
        pump(&mut m, &mut now, |_, ev| {
            ev.iter().any(|e| matches!(e, HostEvent::PortClosed { result: Ok(()), .. }))
        });

        m.kill_sessions("gpu-box");
        pump(&mut m, &mut now, |_, ev| {
            ev.iter().any(|e| matches!(e, HostEvent::SessionsKilled { result: Ok(()), .. }))
        });
    }

    #[test]
    fn releasing_during_an_attempt_still_closes_and_delivers_what_it_opened() {
        let queued =
            crate::ctl::protocol::Request::new(crate::ctl::protocol::Command::Focus { target: "t".into() });
        let host = FakeHost::with(true, |r| r.on("amalgum drain", out(0, &queued.to_line(), "")));
        host.hold.store(true, Ordering::SeqCst);
        let (mut m, _dir) = manager(&host);
        let mut now = 0.0;
        m.acquire("gpu-box", "ws1", "~/a", now).unwrap();
        pump(&mut m, &mut now, |m, _| {
            matches!(m.machine("gpu-box").unwrap().phase(), Phase::Connecting { step: Step::Socket, .. })
        });
        m.release("gpu-box", "ws1", now);
        assert!(m.machine("gpu-box").is_none() && m.hosts().count() == 0, "hidden once released");

        host.hold.store(false, Ordering::SeqCst);
        let mut events = Vec::new();
        wait_for("the late attempt to report", || {
            events.extend(m.poll(now));
            events.iter().any(|e| matches!(e, HostEvent::Drained { .. }))
        });
        let cancel = "-O cancel -R /home/u/.amalgum/run/app1.sock"; // portability: allow
        wait_for("its reverse forward to close", || host.count(cancel) == 1);
        m.poll(now);
        assert!(m.hosts.is_empty(), "forgotten once the attempt reported");

        m.acquire("gpu-box", "ws1", "~/a", now).unwrap();
        pump(&mut m, &mut now, connected);
    }

    #[test]
    fn port_forwards_close_when_the_master_dies() {
        let host = FakeHost::new(true);
        let (mut m, _dir) = manager(&host);
        let mut now = 0.0;
        m.acquire("gpu-box", "ws1", "~/a", now).unwrap();
        pump(&mut m, &mut now, connected);
        m.forward_port("gpu-box", 3000);
        let events =
            pump(&mut m, &mut now, |_, ev| ev.iter().any(|e| matches!(e, HostEvent::PortForwarded { .. })));
        let Some(HostEvent::PortForwarded { result: Ok(local), .. }) =
            events.iter().find(|e| matches!(e, HostEvent::PortForwarded { .. })).cloned()
        else {
            panic!("{events:#?}")
        };

        host.alive.store(false, Ordering::SeqCst);
        let events =
            pump(&mut m, &mut now, |_, ev| ev.iter().any(|e| matches!(e, HostEvent::PortClosed { .. })));
        assert!(
            events.iter().any(|e| matches!(e, HostEvent::PortClosed { local_port, result: Err(_), .. } if *local_port == local)),
            "{events:#?}"
        );
        pump(&mut m, &mut now, connected);
        m.cancel_port("gpu-box", local);
        assert_eq!(host.count("-O cancel -L"), 0, "nothing left to cancel");
    }

    #[test]
    fn invalid_hosts_are_refused() {
        let host = FakeHost::new(true);
        let (mut m, _dir) = manager(&host);
        assert!(m.acquire("-oProxyCommand=x", "ws", "~", 0.0).is_err());
        assert_eq!(m.hosts().count(), 0);
    }
}
