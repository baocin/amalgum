//! The connect steps of §5.28, one function each, plus the operations on a live connection
//! (forwards, drain, kill sessions). Each runs ssh through a [`Runner`] and returns a typed
//! outcome; every failure is a [`GitError`] carrying the command and ssh's stderr verbatim.
//! [`connect`] chains the steps for one attempt and reports progress for the
//! [`state`](super::state) machine. Blocking: call from a worker thread.
//!
//! Remote commands are argv arrays quoted by [`Conn::exec_args`]; the few that need a shell
//! (`sh -c`) use constant scripts and pass every variable part as a quoted positional argument.

use std::borrow::Cow;
use std::path::Path;

use super::runner::{Output, Runner, run_ok};
use super::state::{Step, Warning};
use super::{Conn, HostKeyPrompt, MasterOptions};
use crate::ctl::protocol::Request;
use crate::git::refs::{self, REMOTE_ARGS};
use crate::git::{Git, GitError, Location};

/// Returns the CLI binary for a Rust target triple, if there is one to upload.
pub type CliSource = dyn Fn(&str) -> Option<Cow<'static, [u8]>> + Send + Sync;

/// The binaries this build embedded ([`super::embedded_cli`]); empty outside release builds.
pub fn embedded_cli_source(target: &str) -> Option<Cow<'static, [u8]>> {
    super::embedded_cli(target).map(Cow::Borrowed)
}

/// `$HOME` and `uname -sm` in one round trip.
const INFO_SCRIPT: &str = r#"printf '%s\n' "$HOME"; uname -sm"#;
/// `<digest>  <file>` from whichever tool the host has.
const SHA256_SCRIPT: &str =
    r#"if command -v sha256sum >/dev/null 2>&1; then sha256sum -- "$1"; else shasum -a 256 -- "$1"; fi"#;
/// Make the uploaded binary executable and move it into place atomically.
const INSTALL_SCRIPT: &str = r#"chmod 755 -- "$1" && mv -f -- "$1" "$2""#;
/// Write stdin to `~/.amalgum/tmux.conf` atomically.
const TMUX_CONF_SCRIPT: &str = r#"d="$HOME/.amalgum" && mkdir -p "$d" && cat > "$d/tmux.conf.tmp" && mv -f "$d/tmux.conf.tmp" "$d/tmux.conf""#;
/// The socket directory (0700), minus a stale socket a dead connection left at `$1`: sshd
/// would refuse to bind over it, since its own `StreamLocalBindUnlink` defaults to no.
const SOCKET_DIR_SCRIPT: &str =
    r#"d="$HOME/.amalgum/run" && mkdir -p "$d" && chmod 700 "$d" && rm -f -- "$1""#;

/// One host's connection as the steps see it.
#[derive(Clone, Copy)]
pub struct Link<'a> {
    pub runner: &'a dyn Runner,
    pub conn: &'a Conn,
    pub ssh_bin: &'a str,
    pub scp_bin: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MasterOutcome {
    /// Opened now, or already running.
    Open,
    /// ssh stopped at an unknown host key: show the W16 dialog. `ssh_said` is ssh's stderr.
    HostKeyUnknown { prompt: HostKeyPrompt, ssh_said: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInfo {
    /// Absolute `$HOME` on the host (environment values are passed literally, so paths given
    /// to terminals are absolute).
    pub home: String,
    /// `uname -sm`, e.g. `Linux x86_64`.
    pub uname: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliStatus {
    /// `~/.amalgum/bin/<version>/amalgum --version` already answered with our version.
    Present,
    /// Uploaded, checksum verified, and answering now.
    Installed,
    /// Nothing to upload (see [`Warning::CliUnavailable`]); the connection continues degraded.
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tools {
    /// `git version 2.43.0`.
    pub git: String,
    /// `tmux 3.4`, or `None` when the host has no tmux.
    pub tmux: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketStatus {
    Forwarded,
    /// sshd refused it (`AllowStreamLocalForwarding no`): queue mode.
    Refused {
        ssh_said: String,
    },
}

/// What `amalgum drain` printed: requests in queue order, and lines that did not parse.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Drained {
    pub events: Vec<Request>,
    pub invalid: Vec<(String, String)>,
}

/// `~/.amalgum/bin/<version>/amalgum`, `~` left for the remote shell.
pub fn remote_cli(version: &str) -> String {
    format!("~/{}/bin/{version}/amalgum", crate::paths::REMOTE_ROOT)
}

/// `<home>/.amalgum/run/<app_id>.sock`: where agents on the host reach the app (§5.28 step 4).
pub fn remote_socket(home: &str, app_id: &str) -> String {
    format!("{}/{}/run/{app_id}.sock", home.trim_end_matches('/'), crate::paths::REMOTE_ROOT)
}

/// `amalgum 0.4.1` → `0.4.1`.
pub fn parse_cli_version(stdout: &str) -> Option<&str> {
    let mut words = stdout.split_whitespace();
    (words.next()? == "amalgum").then(|| words.next()).flatten()
}

/// `$HOME` then `uname -sm`, one per line.
pub fn parse_remote_info(stdout: &str) -> Option<RemoteInfo> {
    let mut lines = stdout.lines();
    let home = lines.next()?.trim_end();
    let uname = lines.next()?.trim();
    (home.starts_with('/') && !uname.is_empty())
        .then(|| RemoteInfo { home: home.to_string(), uname: uname.to_string() })
}

/// The hex digest at the start of `sha256sum`/`shasum` output.
pub fn parse_sha256(stdout: &str) -> Option<&str> {
    let digest = stdout.split_whitespace().next()?;
    (digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())).then_some(digest)
}

pub fn parse_drain(stdout: &str) -> Drained {
    let mut drained = Drained::default();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        match Request::from_line(line) {
            Ok(req) => drained.events.push(req),
            Err(e) => drained.invalid.push((line.to_string(), e)),
        }
    }
    drained
}

/// sshd said no to a forward (rather than the connection failing).
fn forward_refused(stderr: &str) -> bool {
    stderr.contains("forwarding failed") || stderr.contains("forwarding request failed")
}

fn local_error(command: &str, stderr: String) -> GitError {
    GitError { command: command.to_string(), code: None, stderr }
}

impl Link<'_> {
    fn ssh(&self, args: Vec<String>) -> Vec<String> {
        let mut argv = vec![self.ssh_bin.to_string()];
        argv.extend(args);
        argv
    }

    /// Run `remote_argv` on the host through the master (every word quoted).
    pub fn exec(&self, remote_argv: &[&str], stdin: Option<&[u8]>) -> Result<Output, GitError> {
        run_ok(self.runner, &self.ssh(self.conn.exec_args(remote_argv)), stdin)
    }

    /// A git runner for `path` on this host, through the master.
    pub fn git(&self, path: &str) -> Git {
        let mut git = Git::new(Location::Remote { host: self.conn.host.clone(), path: path.to_string() });
        git.ssh_bin = self.ssh_bin.to_string();
        git.control_dir = Some(self.conn.control_dir.clone());
        git.ssh_config = self.conn.config_file.clone();
        git
    }

    /// `ssh -O check`: the liveness probe. Cheap (a local socket round trip), but it still
    /// spawns ssh, so the UI calls it off-thread.
    pub fn master_alive(&self) -> bool {
        self.runner.run(&self.ssh(self.conn.check_args()), None).is_ok_and(|out| out.success())
    }

    /// The master's pid, from `ssh -O check`.
    pub fn master_pid(&self) -> Option<u32> {
        let out = self.runner.run(&self.ssh(self.conn.check_args()), None).ok()?;
        super::parse_master_pid(&format!("{}{}", out.stderr_str(), out.stdout_str()))
    }

    /// Step 1: open the ControlMaster, or reuse a live one. `opts.keepalive` is dropped when the
    /// user's ssh config sets `ServerAliveInterval` (`ssh -G`). `trust` accepts a new host key
    /// for this one connection (`StrictHostKeyChecking=accept-new`); an unknown key otherwise
    /// comes back as [`MasterOutcome::HostKeyUnknown`] with its fingerprint.
    pub fn open_master(&self, opts: &MasterOptions, trust: bool) -> Result<MasterOutcome, GitError> {
        if self.master_alive() {
            return Ok(MasterOutcome::Open);
        }
        let ssh_g = run_ok(self.runner, &self.ssh(self.conn.query_config_args()), None)?.stdout_str();
        let mut opts = opts.clone();
        if super::user_sets_keepalive(&ssh_g) {
            opts.keepalive = None;
        }
        let argv =
            self.ssh(self.conn.master_args(&opts, trust).map_err(|e| local_error("ssh", e.to_string()))?);
        create_private_dir(&self.conn.control_dir)
            .map_err(|e| local_error(&format!("mkdir {}", self.conn.control_dir.display()), e.to_string()))?;
        let out = self.runner.run(&argv, None).map_err(|e| local_error(&argv[0], e.to_string()))?;
        if out.success() {
            return Ok(MasterOutcome::Open);
        }
        let ssh_said = out.stderr_str();
        if let Some(prompt) = super::parse_host_key_prompt(&ssh_said) {
            return Ok(MasterOutcome::HostKeyUnknown { prompt, ssh_said });
        }
        // A ProxyJump/ProxyCommand that failed shows only as a closed connection; it may have
        // been the jump host's key.
        let proxy_failed = ssh_said.contains("UNKNOWN port 65535");
        if !super::host_key_unknown(&ssh_said) && !proxy_failed {
            return Err(out.error(&argv));
        }
        // Without a terminal ssh prints no fingerprint; a verbose probe shows the key. Its log
        // always carries the changed-key banner, which the user's LogLevel may hide above.
        let probe_said = match self.runner.run(&self.ssh(self.conn.host_key_probe_args()), None) {
            Ok(probe) => probe.stderr_str(),
            Err(e) => e.to_string(),
        };
        let fail = |why: &str| GitError {
            command: super::runner::command_line(&argv),
            code: out.code,
            stderr: format!("{ssh_said}{why}\n{probe_said}"),
        };
        if super::host_key_changed(&probe_said) {
            return Err(fail("The host key changed. Verbose probe:"));
        }
        if !super::host_key_unknown(&ssh_said) && !super::host_key_unknown(&probe_said) {
            // The proxy failed for another reason (the jump host is down): retry as usual.
            return Err(out.error(&argv));
        }
        match super::ssh_g_target(&ssh_g)
            .and_then(|target| super::parse_server_host_key(&self.conn.host, &target, &probe_said))
        {
            Some(prompt) => Ok(MasterOutcome::HostKeyUnknown { prompt, ssh_said }),
            // Not the target's key (a ProxyJump host's is unknown, say): no Trust can fix that.
            None => Err(fail(
                "Could not read the host's key to confirm it; if it connects through a jump host, \
                 add that host's key to known_hosts. Verbose probe:",
            )),
        }
    }

    /// `$HOME` and `uname -sm`.
    pub fn remote_info(&self) -> Result<RemoteInfo, GitError> {
        let out = self.exec(&["sh", "-c", INFO_SCRIPT], None)?;
        parse_remote_info(&out.stdout_str()).ok_or_else(|| GitError {
            command: "sh -c <remote info>".into(),
            code: out.code,
            stderr: format!("unexpected output: {:?}", out.stdout_str()),
        })
    }

    /// Step 2: make sure `~/.amalgum/bin/<version>/amalgum` answers with `version` (and, when
    /// this build embeds a binary for the host, is that binary); otherwise upload the binary for
    /// the host's `uname -sm` from `source` with scp to a temp name, verify its sha256 on the
    /// host, chmod, and move it into place. The CLI is optional: anything but a lost connection
    /// (ssh's 255) leaves it [`CliStatus::Unavailable`] and the connection continues degraded.
    pub fn ensure_cli(
        &self,
        info: &RemoteInfo,
        version: &str,
        source: &CliSource,
    ) -> Result<CliStatus, GitError> {
        let target = super::target_for_uname(&info.uname);
        let binary = target.and_then(source);
        let dir = format!("{}/{}/bin/{version}", info.home.trim_end_matches('/'), crate::paths::REMOTE_ROOT);
        let installed = format!("{dir}/amalgum");
        if self.cli_version(&remote_cli(version)).as_deref() == Some(version) {
            let Some(binary) = &binary else { return Ok(CliStatus::Present) };
            // Same version string, but is it our build? A stale or foreign one is replaced.
            if self.remote_sha256(&installed).is_some_and(|got| got == super::sha256::hex(binary)) {
                return Ok(CliStatus::Present);
            }
        }
        let Some(target) = target else {
            return Ok(CliStatus::Unavailable { reason: format!("no build for {}", info.uname) });
        };
        let Some(binary) = binary else {
            return Ok(CliStatus::Unavailable { reason: format!("this build embeds no {target} CLI") });
        };
        match self.install_cli(&dir, version, &binary) {
            Ok(()) => Ok(CliStatus::Installed),
            Err(e) if e.code == Some(255) => Err(e),
            Err(e) => Ok(CliStatus::Unavailable { reason: format!("installing it failed: {}", e.summary()) }),
        }
    }

    fn install_cli(&self, dir: &str, version: &str, binary: &[u8]) -> Result<(), GitError> {
        let part = format!("{dir}/amalgum.part-{}", unique_suffix());
        self.exec(&["mkdir", "-p", dir], None)?;
        self.upload(binary, &part)?;
        let want = super::sha256::hex(binary);
        let verified = match self.remote_sha256(&part) {
            Some(got) if got == want => Ok(()),
            got => Err(GitError {
                command: format!("sha256sum {part}"),
                code: None,
                stderr: format!("checksum mismatch after upload: expected {want}, host computed {got:?}"),
            }),
        };
        if let Err(e) = verified.and_then(|()| {
            self.exec(&["sh", "-c", INSTALL_SCRIPT, "sh", &part, &format!("{dir}/amalgum")], None).map(drop)
        }) {
            let _ = self.exec(&["rm", "-f", "--", &part], None);
            return Err(e);
        }
        let cli = remote_cli(version);
        match self.cli_version(&cli) {
            Some(v) if v == version => Ok(()),
            other => Err(GitError {
                command: format!("{cli} --version"),
                code: None,
                stderr: format!("installed CLI reports {other:?}, expected {version}"),
            }),
        }
    }

    /// The lowercase sha256 of a file on the host, by `sha256sum` or `shasum -a 256`.
    fn remote_sha256(&self, path: &str) -> Option<String> {
        let out = self.exec(&["sh", "-c", SHA256_SCRIPT, "sh", path], None).ok()?;
        parse_sha256(&out.stdout_str()).map(str::to_ascii_lowercase)
    }

    fn cli_version(&self, cli: &str) -> Option<String> {
        let out = self.exec(&[cli, "--version"], None).ok()?;
        parse_cli_version(&out.stdout_str()).map(str::to_string)
    }

    /// scp `data` to the absolute, shell-safe `remote` path, via a private local temp file.
    fn upload(&self, data: &[u8], remote: &str) -> Result<(), GitError> {
        let local = self.conn.control_dir.join(format!("upload-{}", unique_suffix()));
        let args = self.conn.scp_upload_args(&local, remote).ok_or_else(|| {
            local_error("scp", format!("remote path {remote:?} needs quoting; refusing to upload there"))
        })?;
        write_private(&local, data)
            .map_err(|e| local_error(&format!("write {}", local.display()), e.to_string()))?;
        let mut argv = vec![self.scp_bin.to_string()];
        argv.extend(args);
        // scp speaks SFTP by default; a host without an sftp subsystem needs the legacy
        // protocol (`-O`), which is safe here because `remote` needs no quoting.
        let result = run_ok(self.runner, &argv, None).or_else(|sftp| {
            let mut legacy = argv.clone();
            legacy.insert(legacy.len() - 3, "-O".to_string());
            run_ok(self.runner, &legacy, None)
                .map_err(|e| GitError { stderr: format!("{}{}", sftp.stderr, e.stderr), ..e })
        });
        let _ = std::fs::remove_file(&local);
        result.map(drop)
    }

    /// Upload the bundled tmux.conf to `~/.amalgum/tmux.conf`.
    pub fn upload_tmux_conf(&self) -> Result<(), GitError> {
        self.exec(&["sh", "-c", TMUX_CONF_SCRIPT], Some(super::TMUX_CONF.as_bytes())).map(drop)
    }

    /// Step 3: `git --version` (missing is an error) and `tmux -V` (missing is `None`). A lost
    /// connection (ssh's 255) is an error for either.
    pub fn check_tools(&self) -> Result<Tools, GitError> {
        let git = self.exec(&["git", "--version"], None)?.stdout_str().trim().to_string();
        let tmux = match self.exec(&["tmux", "-V"], None) {
            Ok(out) => Some(out.stdout_str().trim().to_string()),
            Err(e) if e.code == Some(255) => return Err(e),
            Err(_) => None,
        };
        Ok(Tools { git, tmux })
    }

    /// Step 4: reverse-forward the app socket to `remote_sock` on the master.
    pub fn forward_socket(&self, remote_sock: &str, local_sock: &Path) -> Result<SocketStatus, GitError> {
        if remote_sock.contains(':') {
            // `-R <remote>:<local>` has no escape for it; the CLI queues instead.
            let ssh_said = format!("cannot forward to {remote_sock:?}: ssh cannot name a path with ':'");
            return Ok(SocketStatus::Refused { ssh_said });
        }
        self.exec(&["sh", "-c", SOCKET_DIR_SCRIPT, "sh", remote_sock], None)?;
        let argv = self.ssh(self.conn.forward_socket_args(remote_sock, local_sock));
        match run_ok(self.runner, &argv, None) {
            Ok(_) => Ok(SocketStatus::Forwarded),
            Err(e) if forward_refused(&e.stderr) => Ok(SocketStatus::Refused { ssh_said: e.stderr }),
            Err(e) => Err(e),
        }
    }

    pub fn cancel_socket(&self, remote_sock: &str) -> Result<(), GitError> {
        run_ok(self.runner, &self.ssh(self.conn.cancel_socket_args(remote_sock)), None).map(drop)
    }

    /// Step 5: `git -C <path> remote -v`, to file the workspace under its repo group.
    pub fn repo_identity(&self, path: &str) -> Result<Vec<refs::Remote>, GitError> {
        let out = run_ok(self.runner, &self.git(path).argv(REMOTE_ARGS), None)?;
        Ok(refs::parse_remotes(&out.stdout_str()))
    }

    /// `amalgum drain` on the host: hook events queued while the socket was unreachable.
    pub fn drain(&self, version: &str) -> Result<Drained, GitError> {
        Ok(parse_drain(&self.exec(&[&remote_cli(version), "drain"], None)?.stdout_str()))
    }

    /// `ssh -O forward -L <local>:localhost:<remote>` on the master.
    pub fn forward_port(&self, local_port: u16, remote_port: u16) -> Result<(), GitError> {
        run_ok(self.runner, &self.ssh(self.conn.forward_port_args(local_port, remote_port)), None).map(drop)
    }

    pub fn cancel_port(&self, local_port: u16, remote_port: u16) -> Result<(), GitError> {
        run_ok(self.runner, &self.ssh(self.conn.cancel_port_args(local_port, remote_port)), None).map(drop)
    }

    /// **Kill sessions**: `tmux -L amalgum kill-server` (after the §5.20 confirm). No server
    /// running is success.
    pub fn kill_sessions(&self) -> Result<(), GitError> {
        match self.exec(&["tmux", "-L", super::TMUX_SERVER, "kill-server"], None) {
            Err(e)
                if e.code != Some(255)
                    && (e.stderr.contains("no server running") || e.stderr.contains("error connecting")) =>
            {
                Ok(())
            }
            other => other.map(drop),
        }
    }
}

/// A free TCP port on 127.0.0.1 for a `-L` forward.
pub fn free_local_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

fn unique_suffix() -> String {
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

/// The control dir, 0700 even when an older build created it with looser permissions.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// A new 0600 file holding `data`; nothing is left behind when writing fails (disk full).
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(data).inspect_err(|_| {
        let _ = std::fs::remove_file(path);
    })
}

/// What one attempt needs besides the [`Link`].
pub struct ConnectParams<'a> {
    pub master: MasterOptions,
    pub trust: bool,
    /// The CLI version to verify or install (the app's own).
    pub cli_version: &'a str,
    pub cli_source: &'a CliSource,
    /// Session survival requested (Settings → SSH): tmux is used when present.
    pub keepalive: bool,
    pub app_id: &'a str,
    pub local_socket: &'a Path,
}

/// What a successful attempt learned about the host.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub info: RemoteInfo,
    pub cli: CliStatus,
    pub tools: Tools,
    /// Absolute path of the reverse-forwarded socket (`AMALGUM_SOCK` on the host), set even in
    /// queue mode: the remote CLI queues when nothing answers there.
    pub remote_socket: String,
    pub socket: SocketStatus,
    pub drained: Drained,
}

impl Session {
    /// Terminals run inside tmux (§5.28 "Terminal spawn").
    pub fn use_tmux(&self, keepalive: bool) -> bool {
        keepalive && self.tools.tmux.as_deref().is_some_and(super::tmux_supports_env)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Step(Step),
    Warning(Warning),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Connected(Box<Session>),
    HostKeyUnknown { prompt: HostKeyPrompt, ssh_said: String },
    Failed { step: Step, error: GitError, fatal: bool },
}

/// Is a failure worth retrying automatically? Before the master exists, anything but a host
/// key problem, rejected auth, or a local problem is (offline, host rebooting). An unknown key
/// reaching here could not be offered for Trust (see [`Link::open_master`]).
/// After it, only ssh's own 255 is: any other exit is the host's answer and will not change.
fn retryable(step: Step, e: &GitError) -> bool {
    match step {
        Step::Master => {
            e.code.is_some()
                && !super::host_key_changed(&e.stderr)
                && !super::host_key_unknown(&e.stderr)
                && !e.stderr.contains("Permission denied")
        }
        _ => e.code == Some(255),
    }
}

/// One connect attempt: every step in order, reporting each as it starts and each warning as
/// it happens. The master is left running on failure (ControlPersist reaps it).
pub fn connect(link: &Link, p: &ConnectParams, report: &mut dyn FnMut(Progress)) -> Outcome {
    let fail = |step, error: GitError| Outcome::Failed { step, fatal: !retryable(step, &error), error };

    report(Progress::Step(Step::Master));
    match link.open_master(&p.master, p.trust) {
        Ok(MasterOutcome::Open) => {}
        Ok(MasterOutcome::HostKeyUnknown { prompt, ssh_said }) => {
            return Outcome::HostKeyUnknown { prompt, ssh_said };
        }
        Err(e) => return fail(Step::Master, e),
    }

    report(Progress::Step(Step::Cli));
    let info = match link.remote_info() {
        Ok(info) => info,
        Err(e) => return fail(Step::Cli, e),
    };
    let cli = match link.ensure_cli(&info, p.cli_version, p.cli_source) {
        Ok(cli) => cli,
        Err(e) => return fail(Step::Cli, e),
    };
    if let CliStatus::Unavailable { reason } = &cli {
        report(Progress::Warning(Warning::CliUnavailable { reason: reason.clone() }));
    }

    report(Progress::Step(Step::Tools));
    let tools = match link.check_tools() {
        Ok(tools) => tools,
        Err(e) => return fail(Step::Tools, e),
    };
    let usable_tmux = match (&tools.tmux, p.keepalive) {
        (_, false) => false,
        (None, true) => {
            report(Progress::Warning(Warning::NoTmux));
            false
        }
        (Some(version), true) if !super::tmux_supports_env(version) => {
            report(Progress::Warning(Warning::OldTmux { version: version.clone() }));
            false
        }
        (Some(_), true) => true,
    };
    if usable_tmux {
        match link.upload_tmux_conf() {
            Err(e) if e.code == Some(255) => return fail(Step::Tools, e),
            Err(error) => {
                report(Progress::Warning(Warning::StepFailed { what: "Uploading tmux.conf".into(), error }))
            }
            Ok(()) => {}
        }
    }

    report(Progress::Step(Step::Socket));
    let remote_socket = remote_socket(&info.home, p.app_id);
    let socket = match link.forward_socket(&remote_socket, p.local_socket) {
        Ok(socket) => socket,
        Err(e) => return fail(Step::Socket, e),
    };
    if let SocketStatus::Refused { ssh_said } = &socket {
        report(Progress::Warning(Warning::QueueMode { ssh_said: ssh_said.clone() }));
    }

    report(Progress::Step(Step::Drain));
    let drained = match &cli {
        CliStatus::Unavailable { .. } => Drained::default(),
        _ => match link.drain(p.cli_version) {
            Ok(drained) => drained,
            Err(e) if e.code == Some(255) => return fail(Step::Drain, e),
            Err(error) => {
                report(Progress::Warning(Warning::StepFailed {
                    what: "Draining queued events".into(),
                    error,
                }));
                Drained::default()
            }
        },
    };

    Outcome::Connected(Box::new(Session { info, cli, tools, remote_socket, socket, drained }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ctl::protocol::Command;
    use std::sync::Mutex;

    /// One recorded run: argv and stdin.
    pub(crate) type Call = (Vec<String>, Option<Vec<u8>>);

    /// Answers each command with the latest rule whose pattern occurs in the joined argv
    /// (default: exit 0, no output), and records every call.
    #[derive(Default)]
    pub(crate) struct FakeRunner {
        rules: Mutex<Vec<(String, Output)>>,
        pub(crate) calls: Mutex<Vec<Call>>,
    }

    pub(crate) fn out(code: i32, stdout: &str, stderr: &str) -> Output {
        Output { code: Some(code), stdout: stdout.as_bytes().to_vec(), stderr: stderr.as_bytes().to_vec() }
    }

    impl FakeRunner {
        pub(crate) fn on(self, pattern: &str, output: Output) -> Self {
            self.rules.lock().unwrap().insert(0, (pattern.to_string(), output));
            self
        }

        pub(crate) fn commands(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().map(|(argv, _)| argv.join(" ")).collect()
        }

        pub(crate) fn ran(&self, pattern: &str) -> bool {
            self.commands().iter().any(|c| c.contains(pattern))
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, argv: &[String], stdin: Option<&[u8]>) -> std::io::Result<Output> {
            self.calls.lock().unwrap().push((argv.to_vec(), stdin.map(<[u8]>::to_vec)));
            let line = argv.join(" ");
            let rules = self.rules.lock().unwrap();
            Ok(rules
                .iter()
                .find(|(p, _)| line.contains(p.as_str()))
                .map_or_else(|| out(0, "", ""), |(_, o)| o.clone()))
        }
    }

    fn conn(dir: &Path) -> Conn {
        Conn::new("gpu-box", dir.join("ssh"))
    }

    fn link<'a>(runner: &'a FakeRunner, conn: &'a Conn) -> Link<'a> {
        Link { runner, conn, ssh_bin: "ssh", scp_bin: "scp" }
    }

    const DEAD: &str = "-O check";
    const INFO: &str = "printf";
    const CLI_VERSION: &str = "amalgum --version";
    const SSH_G: &str = " -G ";
    const G_OUT: &str = "hostname 10.0.0.5\nport 22\n";
    /// The verbose probe's log for a server offering `key`.
    fn probed(key: &str) -> String {
        format!("debug1: Authenticating to 10.0.0.5:22 as 'u'\ndebug1: Server host key: {key}\n")
    }
    const SHA256: &str = "sha256sum";
    fn digest_of(data: &[u8]) -> Output {
        out(0, &format!("{}  file\n", crate::ssh::sha256::hex(data)), "")
    }

    fn info() -> RemoteInfo {
        RemoteInfo { home: "/home/u".into(), uname: "Linux x86_64".into() } // portability: allow
    }

    fn no_cli(_: &str) -> Option<Cow<'static, [u8]>> {
        None
    }

    fn fake_cli(target: &str) -> Option<Cow<'static, [u8]>> {
        (target == "x86_64-unknown-linux-musl").then_some(Cow::Borrowed(b"BINARY"))
    }

    // --- parsers ---------------------------------------------------------------------------

    #[test]
    fn parsers() {
        assert_eq!(parse_cli_version("amalgum 0.4.1\n"), Some("0.4.1"));
        assert_eq!(parse_cli_version("bash: amalgum: command not found"), None);
        assert_eq!(parse_cli_version(""), None);
        assert_eq!(parse_remote_info("/home/u\nLinux x86_64\n"), Some(info())); // portability: allow
        assert_eq!(parse_remote_info("\nLinux x86_64\n"), None, "home must be absolute");
        assert_eq!(parse_remote_info("/h\n"), None);
        let digest = "a".repeat(64);
        assert_eq!(parse_sha256(&format!("{digest}  /h/x\n")), Some(digest.as_str()));
        assert_eq!(parse_sha256("sha256sum: /h/x: No such file"), None);
        assert_eq!(remote_socket("/home/u/", "app1"), "/home/u/.amalgum/run/app1.sock"); // portability: allow
        assert_eq!(remote_cli("0.4.1"), "~/.amalgum/bin/0.4.1/amalgum");
    }

    #[test]
    fn parse_drain_keeps_order_and_reports_bad_lines() {
        let a = Request::new(Command::Focus { target: "a".into() });
        let b = Request::new(Command::Focus { target: "b".into() });
        let text = format!("{}not json\n\n{}", a.to_line(), b.to_line());
        let drained = parse_drain(&text);
        assert_eq!(drained.events, vec![a, b]);
        assert_eq!(drained.invalid.len(), 1);
        assert_eq!(drained.invalid[0].0, "not json");
    }

    // --- open_master -----------------------------------------------------------------------

    #[test]
    fn open_master_reuses_a_live_master() {
        let dir = tempfile::tempdir().unwrap();
        let (r, c) = (FakeRunner::default(), conn(dir.path()));
        assert_eq!(link(&r, &c).open_master(&MasterOptions::default(), false), Ok(MasterOutcome::Open));
        assert_eq!(r.commands().len(), 1, "{:?}", r.commands());
    }

    #[test]
    fn open_master_respects_the_users_keepalive_and_creates_the_control_dir() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let r = FakeRunner::default()
            .on(DEAD, out(255, "", "No such file"))
            .on(" -G ", out(0, "serveraliveinterval 30\n", ""));
        assert_eq!(link(&r, &c).open_master(&MasterOptions::default(), false), Ok(MasterOutcome::Open));
        let master = r.commands().into_iter().find(|c| c.contains("-N -f")).expect("master started");
        assert!(!master.contains("ServerAlive"), "{master}");
        assert!(master.contains("StrictHostKeyChecking=yes"), "{master}");
        assert!(c.control_dir.is_dir());

        let r = FakeRunner::default()
            .on(DEAD, out(255, "", ""))
            .on(" -G ", out(0, "serveraliveinterval 0\n", ""));
        link(&r, &c).open_master(&MasterOptions::default(), true).unwrap();
        let master = r.commands().into_iter().find(|c| c.contains("-N -f")).unwrap();
        assert!(
            master.contains("ServerAliveInterval=20") && master.contains("StrictHostKeyChecking=accept-new")
        );
    }

    #[test]
    fn open_master_reports_an_unknown_key_with_the_probed_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let r = FakeRunner::default()
            .on(DEAD, out(255, "", ""))
            .on(SSH_G, out(0, G_OUT, ""))
            .on("BatchMode=yes", out(255, "", &probed("ssh-ed25519 SHA256:k3s9Qz")))
            .on("-N -f", out(255, "", "Host key verification failed.\n"));
        let got = link(&r, &c).open_master(&MasterOptions::default(), false).unwrap();
        assert_eq!(
            got,
            MasterOutcome::HostKeyUnknown {
                prompt: HostKeyPrompt {
                    host: "gpu-box".into(),
                    key_type: "ED25519".into(),
                    fingerprint: "SHA256:k3s9Qz".into()
                },
                ssh_said: "Host key verification failed.\n".into(),
            }
        );
        assert!(!r.commands().iter().any(|c| c.contains("StrictHostKeyChecking=no")));
    }

    #[test]
    fn open_master_unreadable_or_foreign_key_fails_instead_of_looping() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let jump_only = "debug1: Authenticating to jump:22 as 'u'\n\
            debug1: Server host key: ssh-ed25519 SHA256:JUMP\nHost key verification failed.\n";
        for probe in [jump_only, "ssh: Could not resolve hostname\n"] {
            let r = FakeRunner::default()
                .on(DEAD, out(255, "", ""))
                .on(SSH_G, out(0, G_OUT, ""))
                .on("BatchMode=yes", out(255, "", probe))
                .on("-N -f", out(255, "", "Host key verification failed.\n"));
            let e = link(&r, &c).open_master(&MasterOptions::default(), true).unwrap_err();
            assert!(e.stderr.contains(probe) && e.stderr.contains("jump host"), "{e:?}");
            assert!(!retryable(Step::Master, &e), "no Trust or retry can fix it: {e:?}");
        }
    }

    #[test]
    fn open_master_proxy_failure_without_a_key_problem_stays_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let closed = "Connection closed by UNKNOWN port 65535\r\n";
        let r = FakeRunner::default()
            .on(DEAD, out(255, "", ""))
            .on(SSH_G, out(0, G_OUT, ""))
            .on("BatchMode=yes", out(255, "", "ssh: connect to host jump port 22: Connection refused\n"))
            .on("-N -f", out(255, "", closed));
        let e = link(&r, &c).open_master(&MasterOptions::default(), false).unwrap_err();
        assert_eq!(e.stderr, closed);
        assert!(retryable(Step::Master, &e), "the jump host may come back");
        assert!(r.ran("BatchMode=yes"), "a failed proxy is probed for an unknown jump key");
    }

    /// A LogLevel that hides ssh's banner leaves only "verification failed"; the probe shows it.
    #[test]
    fn open_master_changed_key_seen_only_by_the_probe_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let banner = "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\n";
        let r = FakeRunner::default()
            .on(DEAD, out(255, "", ""))
            .on(SSH_G, out(0, G_OUT, ""))
            .on("BatchMode=yes", out(255, "", &format!("{}{banner}", probed("ssh-ed25519 SHA256:x"))))
            .on("-N -f", out(255, "", "Host key verification failed.\n"));
        let e = link(&r, &c).open_master(&MasterOptions::default(), false).unwrap_err();
        assert!(e.stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED"), "{e:?}");
        assert!(!retryable(Step::Master, &e));
    }

    #[test]
    fn open_master_changed_key_is_an_error_not_a_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let changed = "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!\nHost key verification failed.\n";
        let r = FakeRunner::default().on(DEAD, out(255, "", "")).on("-N -f", out(255, "", changed));
        let e = link(&r, &c).open_master(&MasterOptions::default(), false).unwrap_err();
        assert_eq!((e.code, e.stderr.as_str()), (Some(255), changed));
        assert!(!r.ran("BatchMode=yes"), "no probe for a changed key");
        assert!(!retryable(Step::Master, &e));
    }

    #[test]
    fn open_master_other_failures_keep_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let r = FakeRunner::default()
            .on(DEAD, out(255, "", ""))
            .on("-N -f", out(255, "", "ssh: connect to host gpu-box port 22: Connection refused\n"));
        let e = link(&r, &c).open_master(&MasterOptions::default(), false).unwrap_err();
        assert!(e.stderr.contains("Connection refused") && e.command.starts_with("ssh "));
        assert!(retryable(Step::Master, &e));
    }

    // --- ensure_cli ------------------------------------------------------------------------

    #[test]
    fn ensure_cli_present_uploads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let r = FakeRunner::default()
            .on(CLI_VERSION, out(0, "amalgum 0.4.1\n", ""))
            .on(SHA256, digest_of(b"BINARY"));
        assert_eq!(link(&r, &c).ensure_cli(&info(), "0.4.1", &fake_cli), Ok(CliStatus::Present));
        assert!(!r.ran("scp"));
        assert!(r.ran("~/.amalgum/bin/0.4.1/amalgum --version"), "{:?}", r.commands());
        assert!(r.ran("sh /home/u/.amalgum/bin/0.4.1/amalgum"), "checksummed: {:?}", r.commands()); // portability: allow

        // Nothing embedded to compare with: the version is all there is to check.
        let r = FakeRunner::default().on(CLI_VERSION, out(0, "amalgum 0.4.1\n", ""));
        assert_eq!(link(&r, &c).ensure_cli(&info(), "0.4.1", &no_cli), Ok(CliStatus::Present));
        assert!(!r.ran(SHA256));
    }

    #[test]
    fn ensure_cli_replaces_a_foreign_binary_with_the_same_version() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        std::fs::create_dir_all(&c.control_dir).unwrap();
        let r = FakeRunner::default()
            .on(CLI_VERSION, out(0, "amalgum 0.4.1\n", ""))
            .on(SHA256, digest_of(b"SOMETHING ELSE"))
            .on("amalgum.part-", digest_of(b"BINARY"));
        assert_eq!(link(&r, &c).ensure_cli(&info(), "0.4.1", &fake_cli), Ok(CliStatus::Installed));
        assert!(r.ran("scp ") && r.ran("mv -f"), "{:?}", r.commands());
    }

    #[test]
    fn ensure_cli_without_a_binary_is_unavailable_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let r = FakeRunner::default().on(CLI_VERSION, out(127, "", "sh: amalgum: not found\n"));
        let got = link(&r, &c).ensure_cli(&info(), "0.4.1", &no_cli).unwrap();
        assert!(
            matches!(got, CliStatus::Unavailable { ref reason } if reason.contains("x86_64-unknown-linux-musl")),
            "{got:?}"
        );
        let odd = RemoteInfo { uname: "Plan9 mips".into(), ..info() };
        let got = link(&r, &c).ensure_cli(&odd, "0.4.1", &fake_cli).unwrap();
        assert!(
            matches!(got, CliStatus::Unavailable { ref reason } if reason.contains("Plan9 mips")),
            "{got:?}"
        );
    }

    /// A runner whose `--version` starts answering once the install `mv` ran.
    struct Installing {
        inner: FakeRunner,
        digest: String,
    }

    impl Runner for Installing {
        fn run(&self, argv: &[String], stdin: Option<&[u8]>) -> std::io::Result<Output> {
            let line = argv.join(" ");
            let installed = self.inner.ran("mv -f");
            let result = self.inner.run(argv, stdin)?;
            Ok(if line.contains(CLI_VERSION) && installed {
                out(0, "amalgum 0.4.1\n", "")
            } else if line.contains("sha256sum") {
                out(0, &format!("{}  file\n", self.digest), "")
            } else {
                result
            })
        }
    }

    #[test]
    fn ensure_cli_uploads_verifies_and_installs_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        std::fs::create_dir_all(&c.control_dir).unwrap();
        let r = Installing {
            inner: FakeRunner::default().on(CLI_VERSION, out(127, "", "not found")),
            digest: crate::ssh::sha256::hex(b"BINARY"),
        };
        let l = Link { runner: &r, conn: &c, ssh_bin: "ssh", scp_bin: "scp" };
        assert_eq!(l.ensure_cli(&info(), "0.4.1", &fake_cli), Ok(CliStatus::Installed));
        let cmds = r.inner.commands();
        let pos = |p: &str| {
            cmds.iter().position(|c| c.contains(p)).unwrap_or_else(|| panic!("{p} not run: {cmds:#?}"))
        };
        assert!(pos("mkdir -p /home/u/.amalgum/bin/0.4.1") < pos("scp ")); // portability: allow
        assert!(pos("scp ") < pos("sha256sum") && pos("sha256sum") < pos("mv -f"));
        let scp = &cmds[pos("scp ")];
        assert!(scp.contains("gpu-box:/home/u/.amalgum/bin/0.4.1/amalgum.part-"), "{scp}"); // portability: allow
        assert!(std::fs::read_dir(&c.control_dir).unwrap().next().is_none(), "local temp file removed");
    }

    #[test]
    fn ensure_cli_checksum_mismatch_removes_the_upload() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        std::fs::create_dir_all(&c.control_dir).unwrap();
        let r = Installing {
            inner: FakeRunner::default().on(CLI_VERSION, out(127, "", "")),
            digest: "0".repeat(64),
        };
        let l = Link { runner: &r, conn: &c, ssh_bin: "ssh", scp_bin: "scp" };
        let got = l.ensure_cli(&info(), "0.4.1", &fake_cli).unwrap();
        assert!(
            matches!(got, CliStatus::Unavailable { ref reason } if reason.contains("checksum mismatch")),
            "the CLI is optional: {got:?}"
        );
        assert!(!r.inner.ran("mv -f"));
        assert!(r.inner.ran("rm -f -- /home/u/.amalgum/bin/0.4.1/amalgum.part-"), "{:?}", r.inner.commands()); // portability: allow
    }

    #[test]
    fn ensure_cli_install_problems_degrade_but_a_lost_connection_fails() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        std::fs::create_dir_all(&c.control_dir).unwrap();
        let r = FakeRunner::default().on(CLI_VERSION, out(127, "", ""));
        let spaced = RemoteInfo { home: "/home/a b".into(), ..info() }; // portability: allow
        let got = link(&r, &c).ensure_cli(&spaced, "0.4.1", &fake_cli).unwrap();
        assert!(
            matches!(got, CliStatus::Unavailable { ref reason } if reason.contains("quoting")),
            "{got:?}"
        );

        let r = FakeRunner::default()
            .on(CLI_VERSION, out(127, "", ""))
            .on("mkdir", out(1, "", "mkdir: Read-only file system\n"));
        let got = link(&r, &c).ensure_cli(&info(), "0.4.1", &fake_cli).unwrap();
        assert!(
            matches!(got, CliStatus::Unavailable { ref reason } if reason.contains("Read-only")),
            "{got:?}"
        );

        let r = FakeRunner::default()
            .on(CLI_VERSION, out(127, "", ""))
            .on("mkdir", out(255, "", "Connection closed\n"));
        assert_eq!(link(&r, &c).ensure_cli(&info(), "0.4.1", &fake_cli).unwrap_err().code, Some(255));
    }

    #[test]
    fn write_private_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        write_private(&path, b"x").unwrap();
        assert!(write_private(&path, b"y").is_err(), "never overwrites");
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }

    #[test]
    fn create_private_dir_tightens_an_existing_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ctl = dir.path().join("ctl");
        std::fs::create_dir(&ctl).unwrap();
        std::fs::set_permissions(&ctl, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&ctl).unwrap();
        assert_eq!(std::fs::metadata(&ctl).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn upload_falls_back_to_legacy_scp_and_reports_both_failures() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        std::fs::create_dir_all(&c.control_dir).unwrap();
        let r = FakeRunner::default()
            .on("scp ", out(255, "", "scp: Connection closed\n"))
            .on("-q -O --", out(0, "", ""));
        link(&r, &c).upload(b"x", "/h/x").unwrap(); // portability: allow
        assert_eq!(r.commands().len(), 2);

        let r = FakeRunner::default().on("scp ", out(255, "", "sftp said no\n"));
        let e = link(&r, &c).upload(b"x", "/h/x").unwrap_err(); // portability: allow
        assert_eq!(e.stderr, "sftp said no\nsftp said no\n");
        assert!(e.command.contains(" -O "), "{}", e.command);
        assert!(link(&r, &c).upload(b"x", "/h/a b").is_err(), "needs quoting"); // portability: allow
    }

    // --- tools, socket, kill -------------------------------------------------------------------

    #[test]
    fn check_tools_cases() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let r = FakeRunner::default()
            .on("git --version", out(0, "git version 2.43.0\n", ""))
            .on("tmux -V", out(0, "tmux 3.4\n", ""));
        assert_eq!(
            link(&r, &c).check_tools(),
            Ok(Tools { git: "git version 2.43.0".into(), tmux: Some("tmux 3.4".into()) })
        );

        let r = FakeRunner::default().on("tmux -V", out(127, "", "sh: tmux: not found"));
        assert_eq!(link(&r, &c).check_tools().unwrap().tmux, None);

        let r = FakeRunner::default().on("git --version", out(127, "", "sh: git: not found\n"));
        let e = link(&r, &c).check_tools().unwrap_err();
        assert_eq!(e.stderr, "sh: git: not found\n");
        assert!(!retryable(Step::Tools, &e), "missing git is fatal");

        let r = FakeRunner::default().on("tmux -V", out(255, "", "Connection reset"));
        assert!(retryable(Step::Tools, &link(&r, &c).check_tools().unwrap_err()));
    }

    #[test]
    fn forward_socket_prepares_the_dir_and_detects_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let local = dir.path().join("app.sock");
        let r = FakeRunner::default();
        assert_eq!(
            link(&r, &c).forward_socket("/h/.amalgum/run/a.sock", &local),
            Ok(SocketStatus::Forwarded)
        ); // portability: allow
        assert!(
            r.commands()[0].contains("chmod 700") && r.commands()[0].ends_with("sh /h/.amalgum/run/a.sock")
        ); // portability: allow

        let refused = "mux_client_forward: forwarding request failed: remote port forwarding failed for listen path /h/x\n"; // portability: allow
        let r = FakeRunner::default().on("-O forward", out(255, "", refused));
        assert_eq!(
            link(&r, &c).forward_socket("/h/x", &local),
            Ok(SocketStatus::Refused { ssh_said: refused.into() })
        ); // portability: allow

        let colon = link(&r, &c).forward_socket("/h/a:b/.amalgum/run/a.sock", &local).unwrap(); // portability: allow
        assert!(
            matches!(colon, SocketStatus::Refused { ref ssh_said } if ssh_said.contains("':'")),
            "{colon:?}"
        );

        let r =
            FakeRunner::default().on("-O forward", out(255, "", "Control socket connect: No such file\n"));
        assert!(link(&r, &c).forward_socket("/h/x", &local).is_err()); // portability: allow
    }

    #[test]
    fn kill_sessions_without_a_server_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let r =
            FakeRunner::default().on("kill-server", out(1, "", "no server running on /tmp/tmux-0/amalgum\n")); // portability: allow
        assert_eq!(link(&r, &c).kill_sessions(), Ok(()));
        assert!(r.ran("tmux -L amalgum kill-server"));
        let r = FakeRunner::default().on("kill-server", out(255, "", "Connection closed\n"));
        assert!(link(&r, &c).kill_sessions().is_err());
    }

    #[test]
    fn repo_identity_parses_remote_v_through_the_master() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path()).with_config_file(Some(dir.path().join("cfg")));
        let r = FakeRunner::default().on(
            "remote -v",
            out(0, "origin\tgit@github.com:a/b.git (fetch)\norigin\tgit@github.com:a/b.git (push)\n", ""),
        );
        let remotes = link(&r, &c).repo_identity("~/w/a b").unwrap();
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].fetch_url, "git@github.com:a/b.git");
        let cmd = &r.calls.lock().unwrap()[0].0;
        assert_eq!(cmd[..3], ["ssh".to_string(), "-F".into(), dir.path().join("cfg").display().to_string()]);
        assert!(cmd.last().unwrap().ends_with("git -C ~/'w/a b' remote -v"), "{cmd:?}");
    }

    // --- connect ---------------------------------------------------------------------------

    fn params<'a>(source: &'a CliSource, local: &'a Path, keepalive: bool) -> ConnectParams<'a> {
        ConnectParams {
            master: MasterOptions::default(),
            trust: false,
            cli_version: "0.4.1",
            cli_source: source,
            keepalive,
            app_id: "app1",
            local_socket: local,
        }
    }

    fn healthy() -> FakeRunner {
        FakeRunner::default()
            .on(SHA256, digest_of(b"BINARY"))
            .on(SSH_G, out(0, G_OUT, ""))
            .on(INFO, out(0, "/home/u\nLinux x86_64\n", "")) // portability: allow
            .on(CLI_VERSION, out(0, "amalgum 0.4.1\n", ""))
            .on("git --version", out(0, "git version 2.43.0\n", ""))
            .on("tmux -V", out(0, "tmux 3.4\n", ""))
    }

    fn run_connect(r: &FakeRunner, keepalive: bool) -> (Outcome, Vec<Progress>) {
        let dir = tempfile::tempdir().unwrap();
        let c = conn(dir.path());
        let local = dir.path().join("app.sock");
        let mut seen = Vec::new();
        let outcome = connect(&link(r, &c), &params(&fake_cli, &local, keepalive), &mut |p| seen.push(p));
        (outcome, seen)
    }

    #[test]
    fn connect_runs_every_step_in_order() {
        let queued = Request::new(Command::Focus { target: "t".into() });
        let r = healthy().on("amalgum drain", out(0, &queued.to_line(), ""));
        let (outcome, seen) = run_connect(&r, true);
        assert_eq!(seen, Step::ALL.map(Progress::Step).to_vec());
        let Outcome::Connected(session) = outcome else { panic!("{outcome:?}") };
        assert_eq!(session.remote_socket, "/home/u/.amalgum/run/app1.sock"); // portability: allow
        assert_eq!(session.socket, SocketStatus::Forwarded);
        assert_eq!(session.drained.events, vec![queued]);
        assert!(session.use_tmux(true) && !session.use_tmux(false));
        let tmux_conf =
            r.calls.lock().unwrap().iter().find(|(a, _)| a.join(" ").contains("tmux.conf.tmp")).cloned();
        assert_eq!(tmux_conf.and_then(|(_, stdin)| stdin), Some(crate::ssh::TMUX_CONF.as_bytes().to_vec()));
    }

    #[test]
    fn connect_degrades_with_warnings() {
        let refused = "remote port forwarding failed for listen path x\n";
        let r = FakeRunner::default()
            .on(INFO, out(0, "/home/u\nPlan9 mips\n", "")) // portability: allow
            .on(CLI_VERSION, out(127, "", ""))
            .on("tmux -V", out(127, "", ""))
            .on("-O forward", out(255, "", refused));
        let (outcome, seen) = run_connect(&r, true);
        assert!(
            matches!(outcome, Outcome::Connected(ref s) if s.socket == SocketStatus::Refused { ssh_said: refused.into() })
        );
        let warnings: Vec<_> = seen
            .into_iter()
            .filter_map(|p| if let Progress::Warning(w) = p { Some(w) } else { None })
            .collect();
        assert_eq!(
            warnings,
            vec![
                Warning::CliUnavailable { reason: "no build for Plan9 mips".into() },
                Warning::NoTmux,
                Warning::QueueMode { ssh_said: refused.into() },
            ]
        );
        assert!(!r.ran("drain"), "no CLI to drain with");
        assert!(!r.ran("tmux.conf"));
    }

    #[test]
    fn connect_with_a_tmux_older_than_3_2_warns_and_does_without() {
        let r = healthy().on("tmux -V", out(0, "tmux 3.0a\n", ""));
        let (outcome, seen) = run_connect(&r, true);
        assert!(seen.contains(&Progress::Warning(Warning::OldTmux { version: "tmux 3.0a".into() })));
        let Outcome::Connected(session) = outcome else { panic!("{outcome:?}") };
        assert!(!session.use_tmux(true));
        assert!(!r.ran("tmux.conf"));
    }

    #[test]
    fn connect_without_keepalive_does_not_warn_about_tmux() {
        let r = healthy().on("tmux -V", out(127, "", ""));
        let (_, seen) = run_connect(&r, false);
        assert!(!seen.contains(&Progress::Warning(Warning::NoTmux)));
    }

    #[test]
    fn connect_missing_git_is_fatal_and_stops() {
        let r = healthy().on("git --version", out(127, "", "sh: git: not found\n"));
        let (outcome, seen) = run_connect(&r, true);
        assert!(
            matches!(outcome, Outcome::Failed { step: Step::Tools, fatal: true, ref error } if error.stderr.contains("git: not found"))
        );
        assert_eq!(seen.last(), Some(&Progress::Step(Step::Tools)));
        assert!(!r.ran("-O forward"));
    }

    #[test]
    fn connect_dropped_mid_way_is_retryable() {
        let r = healthy().on("-O forward", out(255, "", "Control socket connect(x): Connection refused\n"));
        let (outcome, _) = run_connect(&r, true);
        assert!(matches!(outcome, Outcome::Failed { step: Step::Socket, fatal: false, .. }), "{outcome:?}");
    }

    #[test]
    fn connect_stops_at_an_unknown_host_key() {
        let r = healthy()
            .on(DEAD, out(255, "", ""))
            .on("BatchMode=yes", out(255, "", &probed("ssh-ed25519 SHA256:x")))
            .on("-N -f", out(255, "", "Host key verification failed.\n"));
        let (outcome, seen) = run_connect(&r, true);
        assert!(matches!(outcome, Outcome::HostKeyUnknown { .. }), "{outcome:?}");
        assert_eq!(seen, vec![Progress::Step(Step::Master)]);
    }
}
