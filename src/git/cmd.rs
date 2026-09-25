//! Running git. Local: `git -C <path> <args>`. Remote: `ssh -o ControlPath=<dir>/%C <host> --
//! git -C <path> <args>` over the shared ControlMaster (§5.28), each argument shell-quoted with
//! `ssh::quote` because ssh hands the remote shell one string.
//!
//! Every invocation sets `GIT_TERMINAL_PROMPT=0` (the app has no TTY), `LC_ALL=C` (stable
//! messages), and `GIT_OPTIONAL_LOCKS=0` (status polling must not fight the user's git).

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a workspace's repository lives.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Location {
    Local { path: PathBuf },
    Remote { host: String, path: String },
}

impl Location {
    /// `host:path` (host has no `/`, path non-empty) is remote; anything else is a local path.
    /// `./a:b`, `/x:y`, and `~/a:b` are local.
    pub fn parse(s: &str) -> Self {
        todo!()
    }
    /// Short form for subtitles: `~/w/conduit` (home abbreviated) or `gpu-box:~/g`.
    pub fn display(&self, home: Option<&Path>) -> String {
        todo!()
    }
}

/// A failed git (or ssh) invocation. The toast shows [`GitError::summary`]; **Details** shows
/// `command` and `stderr` verbatim (§5.24: the app never swallows stderr).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError {
    pub command: String,
    pub code: Option<i32>,
    pub stderr: String,
}

impl GitError {
    /// One human line for known failures, else the last non-empty stderr line:
    /// - auth failure → "Authentication failed. Check your SSH agent or credential helper."
    /// - non-fast-forward rejection → "Push rejected: remote has commits you don't have"
    /// - not a repository → "Not a git repository"
    /// - host key verification failed → "Unknown or changed host key"
    /// - network ("Could not resolve host", "Connection refused/timed out") → "Offline"
    /// - local changes would be overwritten → "Checkout would overwrite N files"
    pub fn summary(&self) -> String {
        todo!()
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary())
    }
}

impl std::error::Error for GitError {}

/// A git runner bound to one location.
#[derive(Debug, Clone)]
pub struct Git {
    pub location: Location,
    /// `git` binary (Settings → Git → path). Default `"git"`.
    pub git_bin: String,
    /// `ssh` binary for remote locations. Default `"ssh"`.
    pub ssh_bin: String,
    /// ControlMaster socket directory (`Dirs::ssh_control_dir`); remote only.
    pub control_dir: Option<PathBuf>,
}

impl Git {
    pub fn new(location: Location) -> Self {
        todo!()
    }
    /// The full argv that would run, for logs and the error details pane.
    pub fn argv(&self, args: &[&str]) -> Vec<String> {
        todo!()
    }
    /// A ready-to-spawn command (env set, stdin null unless the caller changes it).
    pub fn command(&self, args: &[&str]) -> Command {
        todo!()
    }
    /// Run to completion; stdout on success.
    pub fn run(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
        todo!()
    }
    /// Run with `input` piped to stdin (commit messages, patches for `apply --cached`).
    pub fn run_with_stdin(&self, args: &[&str], input: &[u8]) -> Result<Vec<u8>, GitError> {
        todo!()
    }
}
