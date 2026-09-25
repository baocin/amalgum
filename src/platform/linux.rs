//! Linux implementations, via freedesktop tools that ship with every desktop.

use super::{Decorations, capture, spawn_detached};
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
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin)?;
    let link = bin.join("amalgum");
    if link.symlink_metadata().is_ok() {
        std::fs::remove_file(&link)?;
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
    spawn_detached(Command::new("notify-send").args(["--app-name=Amalgum", title, body]))
}
