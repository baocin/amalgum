//! ssh and tmux argv builders (§5.28). The app never talks SSH itself: it runs the system
//! `ssh` over one ControlMaster per host, so `~/.ssh/config`, agents, ProxyJump, and hardware
//! keys work for free. This module only builds argument vectors and parses ssh's output.
//!
//! Remote commands reach the remote shell as a single string, so every argument is quoted with
//! [`quote`]. A leading `~/` (or a bare `~`) is left unquoted so the remote shell expands it.
//!
//! ControlPath: `<control_dir>/<16 hex of fnv1a64(host)>`. Unix socket paths are limited to 104
//! bytes on macOS and ssh appends a 17-byte temporary suffix while creating the socket, so the
//! name is kept short instead of using `%C`.
//! Host keys are only ever accepted through the W16 dialog: [`Conn::master_args`] with
//! `trust_new_host_key` adds `StrictHostKeyChecking=accept-new` for that one connection. Never
//! `StrictHostKeyChecking=no`.

use std::path::{Path, PathBuf};

/// Private tmux server name (`tmux -L amalgum`).
pub const TMUX_SERVER: &str = "amalgum";
/// Uploaded to `~/.amalgum/tmux.conf`: tmux stays invisible (§5.28).
pub const TMUX_CONF: &str = include_str!("../assets/tmux.conf");

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_cli.rs"));
}

/// The CLI binary for a remote target, if this build embedded one (release builds only; see
/// `build.rs`). Uploaded to `~/.amalgum/bin/<version>/amalgum` on connect.
pub fn embedded_cli(target: &str) -> Option<&'static [u8]> {
    embedded::EMBEDDED_CLI.iter().find(|(t, _)| *t == target).map(|(_, bin)| *bin)
}

/// POSIX-shell quote one argument for a remote command line.
pub fn quote(arg: &str) -> String {
    todo!()
}

/// Quote and join an argv into one remote command string.
pub fn remote_command(argv: &[&str]) -> String {
    todo!()
}

#[derive(Debug, Clone)]
pub struct MasterOptions {
    pub persist: String,
    /// `(ServerAliveInterval, ServerAliveCountMax)`. `None` when the user's ssh config already
    /// sets them (see [`user_sets_keepalive`]): `-o` on the command line would override it.
    pub keepalive: Option<(u32, u32)>,
}

impl Default for MasterOptions {
    fn default() -> Self {
        Self { persist: "10m".into(), keepalive: Some((20, 2)) }
    }
}

/// Given `ssh -G <host>` output, does the user's config set `serveraliveinterval` (non-zero)?
pub fn user_sets_keepalive(ssh_g: &str) -> bool {
    todo!()
}

/// One host's connection parameters.
#[derive(Debug, Clone)]
pub struct Conn {
    pub host: String,
    pub control_dir: PathBuf,
}

impl Conn {
    pub fn control_path(&self) -> PathBuf {
        todo!()
    }
    /// `-o ControlMaster=auto -o ControlPath=… -o ControlPersist=… [-o ServerAliveInterval=…
    /// -o ServerAliveCountMax=…] [-o StrictHostKeyChecking=accept-new] -N -f <host>`.
    pub fn master_args(&self, opts: &MasterOptions, trust_new_host_key: bool) -> Vec<String> {
        todo!()
    }
    /// `-O check <host>`: is the master alive?
    pub fn check_args(&self) -> Vec<String> {
        todo!()
    }
    /// `-O exit <host>`.
    pub fn exit_args(&self) -> Vec<String> {
        todo!()
    }
    /// Run a command on the master: `-o ControlPath=… <host> -- <quoted argv>`.
    pub fn exec_args(&self, remote_argv: &[&str]) -> Vec<String> {
        todo!()
    }
    /// Reverse-forward the app socket: `-O forward -R <remote_sock>:<local_sock>
    /// -o StreamLocalBindUnlink=yes <host>`.
    pub fn forward_socket_args(&self, remote_sock: &str, local_sock: &Path) -> Vec<String> {
        todo!()
    }
    /// Open a remote port locally: `-O forward -L <local>:localhost:<remote> <host>`.
    pub fn forward_port_args(&self, local_port: u16, remote_port: u16) -> Vec<String> {
        todo!()
    }
    pub fn cancel_port_args(&self, local_port: u16, remote_port: u16) -> Vec<String> {
        todo!()
    }
    /// Terminal with session survival: `-t <host> -- tmux -L amalgum -f ~/.amalgum/tmux.conf
    /// new-session -A -s <session> -c <cwd> -e K=V …`.
    pub fn tmux_args(&self, session: &str, cwd: &str, env: &[(&str, &str)]) -> Vec<String> {
        todo!()
    }
    /// Terminal without tmux: `-t <host> -- 'cd <cwd> && exec env K=V … "$SHELL" -l'`.
    pub fn shell_args(&self, cwd: &str, env: &[(&str, &str)]) -> Vec<String> {
        todo!()
    }
}

/// Reconnect delays in seconds: 3, 6, 12, 24, 48, then `cap` (default 60) forever.
#[derive(Debug, Clone)]
pub struct Backoff {
    next: u64,
    cap: u64,
}

impl Backoff {
    pub fn new(cap: u64) -> Self {
        Self { next: 3, cap }
    }
    pub fn reset(&mut self) {
        self.next = 3;
    }
}

impl Iterator for Backoff {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        todo!()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyPrompt {
    pub host: String,
    /// `ED25519`.
    pub key_type: String,
    /// `SHA256:…`.
    pub fingerprint: String,
}

/// Extract the unknown-host-key details from ssh's stderr ("The authenticity of host … can't be
/// established. ED25519 key fingerprint is SHA256:…").
pub fn parse_host_key_prompt(stderr: &str) -> Option<HostKeyPrompt> {
    todo!()
}

/// Rust target of the CLI binary to upload for `uname -sm` output (§5.31 "Remote build").
pub fn target_for_uname(uname_sm: &str) -> Option<&'static str> {
    todo!()
}

/// Concrete `Host` aliases from an ssh config (no wildcards or negations), for W15.
pub fn config_hosts(config: &str) -> Vec<String> {
    todo!()
}
