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
use std::process::{Child, Command, Stdio};

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

/// Link the running executable (inside an AppImage, the AppImage file, which outlives the mount
/// the executable runs from) onto `$PATH` as `amalgum` (§5.31). Returns the link path.
/// Only an older symlink is replaced: a path that already reaches the executable is left as it
/// is, and any other file there is an `AlreadyExists` error.
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

/// Before `install_cli_shim` (re)points `link` at `exe`: `Ok(true)` when `link` already reaches
/// the same file as `exe` (the binary itself was installed at `link`, or `$HOME` is reached
/// through a symlink), so there is nothing to do and replacing it would delete the binary;
/// `Ok(false)` when nothing is there or it is a symlink that may be replaced. Any other file
/// belongs to the user and is reported as `AlreadyExists`, never deleted.
fn shim_is_current(exe: &Path, link: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let found = match link.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let same_file = match (std::fs::metadata(link), std::fs::metadata(exe)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    };
    if same_file {
        return Ok(true);
    }
    if found.is_symlink() {
        return Ok(false);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{} already exists and is not a link to Amalgum; move it aside first", link.display()),
    ))
}

/// Run a helper tool detached from the app: no stdio, reaped in the background.
fn spawn_detached(cmd: &mut Command) -> io::Result<()> {
    let child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    reap_in_background(child);
    Ok(())
}

/// Wait for `child` on a throwaway thread, so it never lingers as a zombie holding a slot of
/// the per-user process limit for the rest of the app's life.
fn reap_in_background(mut child: Child) {
    let _ = std::thread::Builder::new().name("reap-helper".into()).spawn(move || child.wait());
}

/// Run a helper tool to completion and return its stdout ("" if it is missing or fails).
fn capture(cmd: &mut Command) -> String {
    cmd.stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Shared by both OSes' `install_cli_shim` (the Linux one is also tested end to end).
    #[test]
    fn shim_is_current_only_lets_an_older_symlink_be_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("exe");
        std::fs::write(&exe, b"binary").expect("write exe");
        let link = dir.path().join("amalgum");

        assert!(!shim_is_current(&exe, &link).expect("nothing there yet"));
        assert!(shim_is_current(&exe, &exe).expect("the binary itself"));

        std::os::unix::fs::symlink(dir.path().join("old"), &link).expect("old link");
        assert!(!shim_is_current(&exe, &link).expect("an older symlink is replaceable"));
        std::fs::remove_file(&link).expect("rm");
        std::os::unix::fs::symlink(&exe, &link).expect("current link");
        assert!(shim_is_current(&exe, &link).expect("already pointing at exe"));
        std::fs::remove_file(&link).expect("rm");

        std::fs::write(&link, b"user's own file").expect("regular file");
        let err = shim_is_current(&exe, &link).expect_err("never delete the user's file");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    /// A detached helper (notify-send, xdg-open, osascript…) that exits must not stay behind as
    /// a zombie for the app's whole lifetime: each one holds a slot of the per-user process limit.
    #[test]
    fn detached_helpers_are_reaped_when_they_exit() {
        let child = Command::new("true").stdin(Stdio::null()).spawn().expect("spawn true");
        let pid = child.id().to_string();
        reap_in_background(child);

        // `ps` lists the pid (state `Z` once it has exited) until its parent waits for it.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let listed = capture(Command::new("ps").args(["-o", "stat=", "-p", &pid]));
            if listed.trim().is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "pid {pid} never reaped (ps state {:?})", listed.trim());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
