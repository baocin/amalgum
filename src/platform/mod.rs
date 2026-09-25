//! The only module that may call OS-specific APIs or tools (§1 portability rules).
//!
//! Its surface is fixed by the spec: [`open_in_default_app`], [`reveal_in_file_manager`],
//! [`open_terminal_at`], [`install_cli_shim`], [`listening_ports`], [`primary_modifier_name`],
//! [`window_decorations`], plus [`notify`] for OS notifications (§5.29 routes them "via
//! platform"). Do not add `#[cfg(target_os)]` anywhere else; `scripts/check-portability`
//! fails the build if you do. PTYs, sockets, signals, ssh, and tmux are POSIX and do not
//! belong here.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("Amalgum targets macOS and Linux only (SPEC §11).");

/// Cosmetic window chrome the UI may apply. No feature depends on it (§1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Decorations {
    /// Draw content under a transparent title bar (macOS traffic lights stay).
    pub fullsize_content: bool,
}

/// Open a file, folder, or URL with the user's default handler.
pub fn open_in_default_app(path_or_url: &str) -> io::Result<()> {
    os::open_in_default_app(path_or_url)
}

/// Show `path` selected in Finder / the desktop file manager.
pub fn reveal_in_file_manager(path: &Path) -> io::Result<()> {
    os::reveal_in_file_manager(path)
}

/// Open the user's external terminal app at `path`.
pub fn open_terminal_at(path: &Path) -> io::Result<()> {
    os::open_terminal_at(path)
}

/// Link the running executable onto `$PATH` as `amalgum` (§5.31). Returns the link path.
pub fn install_cli_shim() -> io::Result<PathBuf> {
    os::install_cli_shim(&std::env::current_exe()?)
}

/// TCP ports in LISTEN state owned by any of `pids`, as `(pid, port)` pairs.
pub fn listening_ports(pids: &[u32]) -> Vec<(u32, u16)> {
    if pids.is_empty() {
        return Vec::new();
    }
    os::listening_ports(pids)
}

/// Human name of the `Mod` key: `⌘` on macOS, `Ctrl+Shift` on Linux (§7).
pub fn primary_modifier_name() -> &'static str {
    os::PRIMARY_MODIFIER
}

pub fn window_decorations() -> Decorations {
    os::window_decorations()
}

/// Raise an OS notification. Best effort: failures are ignored by callers.
pub fn notify(title: &str, body: &str) -> io::Result<()> {
    os::notify(title, body)
}

/// Run a helper tool detached from the app: no stdio, not waited on.
fn spawn_detached(cmd: &mut Command) -> io::Result<()> {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(drop)
}

/// Run a helper tool to completion and return its stdout ("" if it is missing or fails).
fn capture(cmd: &mut Command) -> String {
    cmd.stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}
