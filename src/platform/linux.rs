//! Linux implementations, via freedesktop tools that ship with every desktop.

use super::{Decorations, capture, shim_is_current, spawn_detached};
use crate::ports;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const PRIMARY_MODIFIER: &str = "Ctrl+Shift";

pub fn open_in_default_app(target: &str) -> io::Result<()> {
    spawn_detached(Command::new("xdg-open").arg(target))
}

/// Ask the file manager over D-Bus to select the item; fall back to opening the parent.
pub fn reveal_in_file_manager(path: &Path) -> io::Result<()> {
    let uri = format!("array:string:file://{}", path.display());
    let shown = Command::new("dbus-send")
        .args(["--session", "--dest=org.freedesktop.FileManager1", "--type=method_call"])
        .args(["/org/freedesktop/FileManager1", "org.freedesktop.FileManager1.ShowItems", &uri, "string:"])
        .status()
        .is_ok_and(|s| s.success());
    if shown {
        return Ok(());
    }
    let dir = path.parent().unwrap_or(path);
    spawn_detached(Command::new("xdg-open").arg(dir))
}

/// `$TERMINAL`, then the Debian alternative, then common emulators, started in `path`.
pub fn open_terminal_at(path: &Path) -> io::Result<()> {
    let preferred = std::env::var("TERMINAL").ok().filter(|t| !t.is_empty());
    let candidates = preferred.into_iter().chain(
        ["x-terminal-emulator", "gnome-terminal", "konsole", "xfce4-terminal", "alacritty", "kitty", "xterm"]
            .map(String::from),
    );
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no terminal emulator found");
    for term in candidates {
        match spawn_detached(Command::new(&term).current_dir(path)) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// `~/.local/bin` needs no privileges (§5.31).
pub fn install_cli_shim(exe: &Path) -> io::Result<PathBuf> {
    let home = crate::paths::home().ok_or_else(|| io::Error::other("no home directory"))?;
    link_shim(exe, &home.join(".local/bin"))
}

/// Point `<bin>/amalgum` at `exe`, replacing only an older symlink (see `shim_is_current`).
fn link_shim(exe: &Path, bin: &Path) -> io::Result<PathBuf> {
    std::fs::create_dir_all(bin)?;
    let link = bin.join("amalgum");
    if shim_is_current(exe, &link)? {
        return Ok(link);
    }
    if let Err(e) = std::fs::remove_file(&link)
        && e.kind() != io::ErrorKind::NotFound
    {
        return Err(e);
    }
    std::os::unix::fs::symlink(exe, &link)?;
    Ok(link)
}

pub fn listening_ports(pids: &[u32]) -> Vec<(u32, u16)> {
    ports::parse_ss(&capture(Command::new("ss").args(["-H", "-ltnp"])), pids)
}

pub fn window_decorations() -> Decorations {
    Decorations::default()
}

pub fn notify(title: &str, body: &str) -> io::Result<()> {
    spawn_detached(Command::new("notify-send").args(notify_send_args(title, body)))
}

/// `--` first: title and body are agent text, and notify-send reads option-like words anywhere.
fn notify_send_args<'a>(title: &'a str, body: &'a str) -> [&'a str; 4] {
    ["--app-name=Amalgum", "--", title, body]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    // --- install_cli_shim ----------------------------------------------------------------

    #[test]
    fn shim_creates_the_bin_dir_and_links_to_the_exe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("amalgum");
        std::fs::write(&exe, b"binary").expect("write exe");
        let bin = dir.path().join(".local").join("bin");

        let link = link_shim(&exe, &bin).expect("install");
        assert_eq!(link, bin.join("amalgum"));
        assert_eq!(std::fs::read_link(&link).expect("a symlink"), exe);
    }

    #[test]
    fn shim_repoints_an_existing_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        symlink(dir.path().join("old-install"), bin.join("amalgum")).expect("old link");
        let exe = dir.path().join("amalgum");
        std::fs::write(&exe, b"binary").expect("write exe");

        let link = link_shim(&exe, &bin).expect("install");
        assert_eq!(std::fs::read_link(&link).expect("a symlink"), exe);
    }

    /// The Linux tarball's bare binary is usually dropped into `~/.local/bin` itself, so the
    /// running executable *is* the link path: replacing it would unlink the install.
    #[test]
    fn shim_leaves_the_binary_alone_when_it_already_lives_at_the_link() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join(".local").join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let exe = bin.join("amalgum");
        std::fs::write(&exe, b"binary").expect("write exe");

        assert_eq!(link_shim(&exe, &bin).expect("already installed"), exe);
        assert!(!exe.symlink_metadata().expect("still there").is_symlink(), "binary replaced by a link");
        assert_eq!(std::fs::read(&exe).expect("binary readable"), b"binary");
    }

    /// The same file under another spelling: `$HOME` reached through a symlinked directory.
    #[test]
    fn shim_leaves_the_binary_alone_when_home_is_reached_through_a_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real_bin = dir.path().join("real").join(".local").join("bin");
        std::fs::create_dir_all(&real_bin).expect("mkdir");
        let exe = real_bin.join("amalgum");
        std::fs::write(&exe, b"binary").expect("write exe");
        symlink(dir.path().join("real"), dir.path().join("alias")).expect("home alias");

        link_shim(&exe, &dir.path().join("alias").join(".local").join("bin")).expect("already installed");
        assert!(!exe.symlink_metadata().expect("still there").is_symlink(), "binary replaced by a link");
        assert_eq!(std::fs::read(&exe).expect("binary readable"), b"binary");
    }

    #[test]
    fn shim_refuses_to_delete_a_regular_file_it_does_not_own() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        std::fs::write(bin.join("amalgum"), b"someone else's").expect("write other file");
        let exe = dir.path().join("amalgum");
        std::fs::write(&exe, b"binary").expect("write exe");

        let err = link_shim(&exe, &bin).expect_err("a regular file is not ours to replace");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(bin.join("amalgum")).expect("untouched"), b"someone else's");
    }

    // --- notify ----------------------------------------------------------------------------

    /// notify-send's option parser reads option-like words anywhere in argv, so agent text that
    /// starts with `-` must follow `--`: otherwise the notification is lost or sets flags.
    #[test]
    fn notify_send_passes_agent_text_as_operands() {
        let title = "--urgency=critical";
        let body = "- Fixed the bug\n- Added tests\nShould I commit?";
        let args = notify_send_args(title, body);
        let end = args.iter().position(|a| *a == "--").expect("`--` ends the options");
        assert_eq!(args[end + 1..], [title, body]);
        assert_eq!(args[..end], ["--app-name=Amalgum"]);
    }
}
