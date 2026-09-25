//! macOS implementations. Native menu bar, vibrancy, and UNUserNotification can land here
//! later; today every function shells out to a stock macOS tool.

use super::{Decorations, capture, spawn_detached};
use crate::ports;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const PRIMARY_MODIFIER: &str = "⌘";

pub fn open_in_default_app(target: &str) -> io::Result<()> {
    spawn_detached(Command::new("open").arg(target))
}

pub fn reveal_in_file_manager(path: &Path) -> io::Result<()> {
    spawn_detached(Command::new("open").arg("-R").arg(path))
}

pub fn open_terminal_at(path: &Path) -> io::Result<()> {
    spawn_detached(Command::new("open").args(["-a", "Terminal"]).arg(path))
}

/// `/usr/local/bin` is root-owned, so the link is made through an admin prompt.
pub fn install_cli_shim(exe: &Path) -> io::Result<PathBuf> {
    let link = PathBuf::from("/usr/local/bin/amalgum");
    let script = format!(
        "do shell script \"mkdir -p /usr/local/bin && ln -sf \" & quoted form of \"{}\" & \" {}\" \
         with administrator privileges",
        applescript_escape(&exe.to_string_lossy()),
        link.display()
    );
    let status = Command::new("osascript").args(["-e", &script]).status()?;
    if status.success() { Ok(link) } else { Err(io::Error::other("administrator prompt was cancelled")) }
}

pub fn listening_ports(pids: &[u32]) -> Vec<(u32, u16)> {
    let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let out = capture(Command::new("lsof").args(["-a", "-iTCP", "-sTCP:LISTEN", "-P", "-n", "-F", "pn", "-p", &list]));
    ports::parse_lsof(&out, pids)
}

pub fn window_decorations() -> Decorations {
    Decorations { fullsize_content: true }
}

pub fn notify(title: &str, body: &str) -> io::Result<()> {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        applescript_escape(body),
        applescript_escape(title)
    );
    spawn_detached(Command::new("osascript").args(["-e", &script]))
}

fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
