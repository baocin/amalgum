//! Linux implementations, via freedesktop tools that ship with every desktop.

use super::{Decorations, capture, shim_is_current, spawn_detached};
use crate::ports;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const PRIMARY_MODIFIER: &str = "Ctrl+Shift";

pub fn open_in_default_app(target: &str) -> io::Result<()> {
    spawn_detached(Command::new("xdg-open").arg(target))
}

/// Ask the file manager over D-Bus to select the item; fall back to opening the parent when
/// nothing answers. On a helper thread, since waiting for that answer must not stall the UI.
pub fn reveal_in_file_manager(path: &Path) -> io::Result<()> {
    let path = path.to_path_buf();
    std::thread::Builder::new().name("reveal".into()).spawn(move || {
        let shown = Command::new("dbus-send")
            .args(show_items_args(&path))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !shown {
            let _ = spawn_detached(Command::new("xdg-open").arg(path.parent().unwrap_or(&path)));
        }
    })?;
    Ok(())
}

/// `dbus-send` arguments for `FileManager1.ShowItems([<uri of path>], "")`. `--print-reply` waits
/// for the answer, so the exit status says whether a file manager showed the item: without it
/// `dbus-send` exits 0 even on a bus where nothing provides FileManager1.
fn show_items_args(path: &Path) -> Vec<String> {
    [
        "--session",
        "--print-reply",
        "--dest=org.freedesktop.FileManager1",
        "--type=method_call",
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1.ShowItems",
        &format!("array:string:{}", file_uri(path)),
        "string:",
    ]
    .map(String::from)
    .to_vec()
}

/// `file://` URI with every byte of `path` other than `/` and RFC 3986's unreserved characters
/// percent-encoded: `dbus-send` splits an `array:` value on every comma, with no escape, and a
/// file manager's URI parser would cut at `#` or decode a literal `%20`.
fn file_uri(path: &Path) -> String {
    use std::fmt::Write as _;
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(b));
        } else {
            let _ = write!(uri, "%{b:02X}");
        }
    }
    uri
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
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
    let appdir = std::env::var_os("APPDIR").map(PathBuf::from);
    link_shim(&shim_target(exe, appimage, appdir), &home.join(".local/bin"))
}

/// What the shim points at: `exe`, except inside an AppImage, where `exe` sits under a
/// per-launch mount (`$APPDIR`) that vanishes when the app exits; there it is the AppImage file
/// itself (`$APPIMAGE`), the same rule the hooks' command follows (`ctl::cli::hook_exe`).
fn shim_target(exe: &Path, appimage: Option<PathBuf>, appdir: Option<PathBuf>) -> PathBuf {
    let resolve = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    match (appimage, appdir) {
        (Some(appimage), Some(appdir)) if resolve(exe).starts_with(resolve(&appdir)) => appimage,
        _ => exe.to_path_buf(),
    }
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

    /// Inside an AppImage the executable lives under a per-launch mount (`$APPDIR`) that is gone
    /// once the app exits: a shim pointing there would be dead. The AppImage file (`$APPIMAGE`)
    /// is what stays, and what the hooks run too.
    #[test]
    fn shim_inside_an_appimage_links_to_the_appimage_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mount = dir.path().join(".mount_AmalgX");
        let exe = mount.join("usr").join("bin").join("amalgum");
        std::fs::create_dir_all(exe.parent().expect("parent")).expect("mkdir");
        std::fs::write(&exe, b"binary").expect("write exe");
        let appimage = dir.path().join("Amalgum.AppImage");
        std::fs::write(&appimage, b"appimage").expect("write appimage");

        let target = shim_target(&exe, Some(appimage.clone()), Some(mount.clone()));
        let link = link_shim(&target, &dir.path().join("bin")).expect("install");
        assert_eq!(std::fs::read_link(&link).expect("a symlink"), appimage);

        // `$APPIMAGE` leaks into every shell the AppImage app spawns; a different binary run
        // from one of them links to itself.
        let other = dir.path().join("amalgum");
        assert_eq!(shim_target(&other, Some(appimage), Some(mount)), other);
        assert_eq!(shim_target(&exe, None, None), exe);
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

    // --- reveal_in_file_manager --------------------------------------------------------------

    /// `dbus-send` splits an `array:string:` value on every comma, with no escape, and the file
    /// manager's URI parser cuts at `#` and decodes `%XX`: the path must go over as one element,
    /// percent-encoded.
    #[test]
    fn show_items_sends_the_path_as_one_percent_encoded_uri() {
        let args = show_items_args(Path::new("/home/u/a,b #1/100% ü.txt")); // portability: allow
        let items: Vec<&str> = args.iter().filter_map(|a| a.strip_prefix("array:string:")).collect();
        assert_eq!(items, ["file:///home/u/a%2Cb%20%231/100%25%20%C3%BC.txt"]); // portability: allow
    }

    /// A session bus with no FileManager1 service (common under tiling window managers) must
    /// make `dbus-send` fail, or "Reveal" never falls back to opening the folder. Runs on a
    /// private bus that can start no services; skipped where no such bus can be run.
    #[test]
    fn show_items_fails_when_no_file_manager_answers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("bus.conf");
        // No <servicedir>: the bus can activate nothing, so no real file manager ever starts.
        let bus = format!(
            "<busconfig><type>session</type><listen>unix:dir={}</listen><policy context=\"default\">\
             <allow send_destination=\"*\" eavesdrop=\"true\"/><allow eavesdrop=\"true\"/>\
             <allow own=\"*\"/></policy></busconfig>",
            dir.path().display()
        );
        std::fs::write(&config, bus).expect("write bus config");
        let dbus_send = |args: &[&str]| {
            Command::new("dbus-run-session")
                .arg(format!("--config-file={}", config.display()))
                .args(["--", "dbus-send", "--reply-timeout=5000"])
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        };

        let list_names = ["--session", "--print-reply", "--dest=org.freedesktop.DBus"]
            .into_iter()
            .chain(["/org/freedesktop/DBus", "org.freedesktop.DBus.ListNames"])
            .collect::<Vec<_>>();
        if !dbus_send(&list_names).is_ok_and(|s| s.success()) {
            eprintln!("skipped: cannot run a private D-Bus session here");
            return;
        }
        let args = show_items_args(dir.path());
        let status = dbus_send(&args.iter().map(String::as_str).collect::<Vec<_>>()).expect("spawn");
        assert!(!status.success(), "dbus-send claimed a file manager showed the item");
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
