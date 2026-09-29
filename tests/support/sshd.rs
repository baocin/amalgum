//! A private sshd for end-to-end tests of the ssh engine (§5.28): runs as the current user on
//! 127.0.0.1:<free port> with its own host key, authorized key, and config in a temp dir, and
//! writes a client config with a `Host` alias for it, so tests never read or touch `~/.ssh`.
//!
//! The "remote" side is this machine, so sshd gives sessions a private `$HOME` and
//! `TMUX_TMPDIR` inside the temp dir (`SetEnv`): nothing lands in the real home or tmux server.
//! Host keys are trusted by writing `known_hosts` up front ([`Options::trusted`]) or through
//! `accept-new` in the test itself, never `StrictHostKeyChecking=no`.
//!
//! [`Sshd::start`] returns `Err(reason)` when sshd is not installed or cannot start here; tests
//! print the reason and pass (see [`start_or_skip`]).

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const ALIAS: &str = "testbox";

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Write the host key into the client's `known_hosts`.
    pub trusted: bool,
    /// `AllowStreamLocalForwarding` (reverse socket forwards).
    pub stream_local: bool,
    /// An sftp subsystem (scp's default protocol); without it scp needs `-O`.
    pub sftp: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { trusted: true, stream_local: true, sftp: true }
    }
}

pub struct Sshd {
    dir: tempfile::TempDir,
    child: Child,
    port: u16,
}

/// Start an sshd, or print why not and return `None`.
pub fn start_or_skip(test: &str, opts: Options) -> Option<Sshd> {
    match Sshd::start(opts) {
        Ok(sshd) => Some(sshd),
        Err(why) => {
            eprintln!("skipped {test}: {why}");
            None
        }
    }
}

fn find_sshd() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/usr/local/sbin", "/sbin"].map(PathBuf::from)) // portability: allow
        .map(|dir| dir.join("sshd"))
        .find(|p| p.is_file())
}

fn run(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd:?} failed: {}", String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    Ok(listener.local_addr().map_err(|e| e.to_string())?.port())
}

impl Sshd {
    pub fn start(opts: Options) -> Result<Self, String> {
        let sshd = find_sshd().ok_or("sshd is not installed")?;
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let d = dir.path();
        for sub in ["home", "tmux", "ctl"] {
            std::fs::create_dir(d.join(sub)).map_err(|e| e.to_string())?;
        }
        for key in ["host_key", "client_key"] {
            run(Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "", "-f"])
                .arg(d.join(key)))?;
        }
        std::fs::copy(d.join("client_key.pub"), d.join("authorized_keys")).map_err(|e| e.to_string())?;

        let mut last_error = String::new();
        // A free port can be taken between probing and binding; try a few.
        for _ in 0..3 {
            let port = free_port()?;
            std::fs::write(d.join("sshd_config"), sshd_config(d, port, opts)).map_err(|e| e.to_string())?;
            std::fs::write(d.join("ssh_config"), host_block(ALIAS, d, port)).map_err(|e| e.to_string())?;
            let log = d.join("sshd.log");
            let mut child = Command::new(&sshd)
                .arg("-D")
                .arg("-f")
                .arg(d.join("sshd_config"))
                .arg("-E")
                .arg(&log)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("spawn {}: {e}", sshd.display()))?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(Some(status)) = child.try_wait() {
                    let log = std::fs::read_to_string(&log).unwrap_or_default();
                    last_error = format!("sshd exited ({status}): {}", log.trim());
                    break;
                }
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    let sshd = Self { dir, child, port };
                    if opts.trusted {
                        sshd.trust_host_key()?;
                    }
                    return Ok(sshd);
                }
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("sshd did not start listening within 5 s".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Err(last_error)
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// `ssh -F` this for the [`ALIAS`] host.
    pub fn client_config(&self) -> PathBuf {
        self.dir().join("ssh_config")
    }

    /// This server's client config under another `Host` alias, to combine several servers in
    /// one config file (ProxyJump). Its key is still looked up as [`ALIAS`] in [`Self::known_hosts`].
    pub fn host_block(&self, alias: &str) -> String {
        host_block(alias, self.dir(), self.port)
    }

    /// `$HOME` of every session on this "remote".
    pub fn remote_home(&self) -> PathBuf {
        self.dir().join("home")
    }

    pub fn control_dir(&self) -> PathBuf {
        self.dir().join("ctl")
    }

    pub fn known_hosts(&self) -> PathBuf {
        self.dir().join("known_hosts")
    }

    /// `SHA256:…` of the server's host key, as ssh shows it.
    pub fn host_fingerprint(&self) -> String {
        let out = run(Command::new("ssh-keygen").arg("-lf").arg(self.dir().join("host_key.pub")))
            .expect("ssh-keygen -l");
        out.split_whitespace().nth(1).expect("fingerprint").to_string()
    }

    fn trust_host_key(&self) -> Result<(), String> {
        let key = std::fs::read_to_string(self.dir().join("host_key.pub")).map_err(|e| e.to_string())?;
        std::fs::write(self.known_hosts(), format!("{ALIAS} {key}")).map_err(|e| e.to_string())
    }

    /// Ask every master under [`Self::control_dir`] to exit.
    fn stop_masters(&self) {
        let Ok(entries) = std::fs::read_dir(self.control_dir()) else { return };
        for entry in entries.flatten() {
            let _ = Command::new("ssh")
                .arg("-F")
                .arg(self.client_config())
                .arg("-o")
                .arg(format!("ControlPath={}", entry.path().display()))
                .args(["-O", "exit", ALIAS])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        self.stop_masters();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn sshd_config(d: &Path, port: u16, opts: Options) -> String {
    let d = d.display();
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    format!(
        "Port {port}\n\
         ListenAddress 127.0.0.1\n\
         HostKey {d}/host_key\n\
         AuthorizedKeysFile {d}/authorized_keys\n\
         PidFile none\n\
         StrictModes no\n\
         UsePAM no\n\
         PasswordAuthentication no\n\
         KbdInteractiveAuthentication no\n\
         PubkeyAuthentication yes\n\
         PermitRootLogin prohibit-password\n\
         AllowTcpForwarding yes\n\
         AllowStreamLocalForwarding {}\n\
         SetEnv HOME={d}/home TMUX_TMPDIR={d}/tmux\n\
         LogLevel ERROR\n\
         {}",
        yes_no(opts.stream_local),
        if opts.sftp { "Subsystem sftp internal-sftp\n" } else { "" }
    )
}

fn host_block(alias: &str, d: &Path, port: u16) -> String {
    let d = d.display();
    format!(
        "Host {alias}\n\
         \x20 HostName 127.0.0.1\n\
         \x20 Port {port}\n\
         \x20 HostKeyAlias {ALIAS}\n\
         \x20 IdentityFile {d}/client_key\n\
         \x20 IdentitiesOnly yes\n\
         \x20 IdentityAgent none\n\
         \x20 UserKnownHostsFile {d}/known_hosts\n\
         \x20 GlobalKnownHostsFile /dev/null\n\
         \x20 PasswordAuthentication no\n\
         \x20 KbdInteractiveAuthentication no\n"
    )
}
