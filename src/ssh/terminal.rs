//! The argv a remote terminal tab spawns in its PTY (§5.28 "Terminal spawn"): `ssh -t` through
//! the host's ControlMaster, into tmux (`new-session -A`, so a reconnect reattaches with full
//! scrollback) when keepalive is on and the host has tmux, else straight into a login shell.

use super::Conn;

/// One tab's identity and where it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabSpawn<'a> {
    pub workspace_id: &'a str,
    pub tab_id: &'a str,
    /// Remote directory; `~/…` is expanded by the remote shell.
    pub cwd: &'a str,
}

/// tmux turns `.` and `:` in a session name into `_` (they separate window and pane in a
/// target), so `new-session -A` would never find the session again: use its spelling up front.
pub fn tmux_session_name(tab_id: &str) -> String {
    tab_id.replace(['.', ':'], "_")
}

/// The full argv (`ssh_bin` first). `remote_socket` is the absolute reverse-forwarded socket
/// path (`AMALGUM_SOCK`), set even in queue mode so the remote CLI queues there.
pub fn terminal_argv(
    ssh_bin: &str,
    conn: &Conn,
    remote_socket: &str,
    tab: &TabSpawn,
    use_tmux: bool,
) -> Vec<String> {
    let env = [
        ("AMALGUM_SOCK", remote_socket),
        ("AMALGUM_WORKSPACE", tab.workspace_id),
        ("AMALGUM_TAB", tab.tab_id),
    ];
    let mut argv = vec![ssh_bin.to_string()];
    argv.extend(if use_tmux {
        conn.tmux_args(&tmux_session_name(tab.tab_id), tab.cwd, &env)
    } else {
        conn.shell_args(tab.cwd, &env)
    });
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAB: TabSpawn = TabSpawn { workspace_id: "ws1", tab_id: "tab.7", cwd: "~/w/app" };
    const SOCK: &str = "/home/u/.amalgum/run/app1.sock"; // portability: allow

    #[test]
    fn with_tmux_attaches_or_creates_the_tabs_session() {
        let c = Conn::new("gpu-box", "run/ssh");
        let argv = terminal_argv("ssh", &c, SOCK, &TAB, true);
        assert_eq!(argv[0], "ssh");
        assert_eq!(
            argv[1..],
            c.tmux_args(
                "tab_7",
                "~/w/app",
                &[("AMALGUM_SOCK", SOCK), ("AMALGUM_WORKSPACE", "ws1"), ("AMALGUM_TAB", "tab.7"),]
            )[..]
        );
        assert!(argv.contains(&"-t".to_string()) && argv.iter().any(|a| a.starts_with("ControlPath=")));
        assert_eq!(
            argv.last().unwrap(),
            "tmux -L amalgum -f ~/.amalgum/tmux.conf new-session -A -s tab_7 -c ~/w/app \
             -e AMALGUM_SOCK=/home/u/.amalgum/run/app1.sock -e AMALGUM_WORKSPACE=ws1 -e AMALGUM_TAB=tab.7" // portability: allow
        );
    }

    #[test]
    fn without_tmux_execs_a_login_shell_with_the_env_prefixed() {
        let c = Conn::new("gpu-box", "run/ssh").with_config_file(Some("cfg".into()));
        let argv = terminal_argv("ssh", &c, SOCK, &TAB, false);
        assert_eq!(argv[..3], ["ssh", "-F", "cfg"]);
        assert_eq!(
            argv.last().unwrap(),
            "cd ~/w/app && exec env AMALGUM_SOCK=/home/u/.amalgum/run/app1.sock AMALGUM_WORKSPACE=ws1 \
             AMALGUM_TAB=tab.7 \"$SHELL\" -l" // portability: allow
        );
    }

    #[test]
    fn hostile_ids_stay_quoted() {
        let c = Conn::new("gpu-box", "run/ssh");
        let tab = TabSpawn { workspace_id: "$(id)", tab_id: "a b", cwd: "/srv/x;y" }; // portability: allow
        let cmd = terminal_argv("ssh", &c, SOCK, &tab, false).pop().unwrap();
        assert!(
            cmd.contains("'/srv/x;y'")
                && cmd.contains("'AMALGUM_WORKSPACE=$(id)'")
                && cmd.contains("'AMALGUM_TAB=a b'"),
            "{cmd}"
        ); // portability: allow
    }

    #[test]
    fn session_names() {
        assert_eq!(tmux_session_name("t-1"), "t-1");
        assert_eq!(tmux_session_name("a.b:c"), "a_b_c");
    }
}
