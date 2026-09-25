//! Listening-port detection output parsers (§5.27 "Port detection"). `platform` runs the tool
//! (`lsof` on macOS, `ss` on Linux, either over ssh for remote workspaces); these pure
//! functions turn its output into `(pid, port)` pairs owned by `pids`, deduplicated and sorted
//! by port then pid.

/// `lsof -a -iTCP -sTCP:LISTEN -P -n -F pn -p <pids>`: `p<pid>` lines followed by
/// `n<addr>:<port>` lines (`n*:3000`, `n127.0.0.1:5173`, `n[::1]:8080`).
pub fn parse_lsof(out: &str, pids: &[u32]) -> Vec<(u32, u16)> {
    todo!()
}

/// `ss -H -ltnp`: `LISTEN 0 511 *:3000 *:* users:(("node",pid=4412,fd=21))`; local address
/// forms `0.0.0.0:80`, `[::]:3000`, `*:5173`, `127.0.0.1%lo:53`; several processes may share
/// one socket.
pub fn parse_ss(out: &str, pids: &[u32]) -> Vec<(u32, u16)> {
    todo!()
}
