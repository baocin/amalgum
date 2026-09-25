//! Listening-port detection output parsers (§5.27 "Port detection"). `platform` runs the tool
//! (`lsof` on macOS, `ss` on Linux, either over ssh for remote workspaces); these pure
//! functions turn its output into `(pid, port)` pairs owned by `pids`, deduplicated and sorted
//! by port then pid.

use std::collections::HashSet;

/// Extract the trailing `:<port>` from an address that may be `*:3000`, `127.0.0.1:5173`,
/// `[::1]:8080` (brackets around an IPv6 address), `localhost:3000`, or carry a `%iface`
/// scope id (`127.0.0.1%lo:53`, `[fe80::1%eth0]:8080`) — the scope id sits before the port
/// separator either way, so it never affects the extracted port.
fn extract_port(addr: &str) -> Option<u16> {
    if let Some(close) = addr.rfind(']') {
        return addr[close + 1..].strip_prefix(':')?.parse().ok();
    }
    let sep = addr.rfind(':')?;
    addr[sep + 1..].parse().ok()
}

fn sort_and_dedupe(mut v: Vec<(u32, u16)>) -> Vec<(u32, u16)> {
    v.sort_by_key(|&(pid, port)| (port, pid));
    v.dedup();
    v
}

/// `lsof -a -iTCP -sTCP:LISTEN -P -n -F pn -p <pids>`: `p<pid>` lines followed by
/// `n<addr>:<port>` lines (`n*:3000`, `n127.0.0.1:5173`, `n[::1]:8080`). Field lines other
/// than `p`/`n` (and `n` lines before any `p` line) are ignored.
pub fn parse_lsof(out: &str, pids: &[u32]) -> Vec<(u32, u16)> {
    let allowed: HashSet<u32> = pids.iter().copied().collect();
    let mut current_pid: Option<u32> = None;
    let mut result = Vec::new();
    for line in out.lines() {
        let line = line.trim_end();
        let Some((tag, rest)) = line.split_at_checked(1) else { continue };
        match tag {
            "p" => current_pid = rest.parse().ok(),
            "n" => {
                if let Some(pid) = current_pid
                    && allowed.contains(&pid)
                    && let Some(port) = extract_port(rest)
                {
                    result.push((pid, port));
                }
            }
            _ => {} // other field lines (e.g. `f`, `c`) ignored
        }
    }
    sort_and_dedupe(result)
}

/// `ss -H -ltnp`: `LISTEN 0 511 *:3000 *:* users:(("node",pid=4412,fd=21))`; local address
/// forms `0.0.0.0:80`, `[::]:3000`, `*:5173`, `127.0.0.1%lo:53`; several processes may share
/// one socket (`users:(("node",pid=1,fd=1),("node",pid=2,fd=2))`). Lines whose process column
/// has no `users:((` (no `-p` info, or process died) are skipped.
pub fn parse_ss(out: &str, pids: &[u32]) -> Vec<(u32, u16)> {
    let allowed: HashSet<u32> = pids.iter().copied().collect();
    let mut result = Vec::new();
    for line in out.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Columns: State Recv-Q Send-Q Local:Port Peer:Port Process...
        let Some(local) = fields.get(3) else { continue };
        let process = fields.get(5..).map(|s| s.join(" ")).unwrap_or_default();
        if !process.contains("users:((") {
            continue;
        }
        let Some(port) = extract_port(local) else { continue };
        for pid in extract_pids(&process) {
            if allowed.contains(&pid) {
                result.push((pid, port));
            }
        }
    }
    sort_and_dedupe(result)
}

/// Every `pid=<N>` occurrence in an `ss` process column, e.g. two from
/// `users:(("node",pid=4412,fd=21),("node",pid=4413,fd=23))`.
fn extract_pids(process: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut rest = process;
    while let Some(idx) = rest.find("pid=") {
        rest = &rest[idx + 4..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(pid) = digits.parse() {
            pids.push(pid);
        }
    }
    pids
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_lsof, using realistic `lsof -F pn` fixtures ------------------------------------

    #[test]
    fn lsof_groups_n_lines_under_the_preceding_p_line() {
        let out = "\
p4412
n*:3000
p4420
n127.0.0.1:5173
n[::1]:8080
";
        assert_eq!(parse_lsof(out, &[4412, 4420]), vec![(4412, 3000), (4420, 5173), (4420, 8080)]);
    }

    #[test]
    fn lsof_keeps_only_pids_in_the_filter_list() {
        let out = "\
p4412
n*:3000
p9999
n*:4000
";
        assert_eq!(parse_lsof(out, &[4412]), vec![(4412, 3000)]);
    }

    #[test]
    fn lsof_accepts_localhost_address_form() {
        let out = "p1\nnlocalhost:3000\n";
        assert_eq!(parse_lsof(out, &[1]), vec![(1, 3000)]);
    }

    #[test]
    fn lsof_ignores_other_field_lines_and_n_lines_before_any_p_line() {
        let out = "\
nphantom:1111
p1234
cnode
f21
n*:8000
";
        assert_eq!(parse_lsof(out, &[1234]), vec![(1234, 8000)]);
    }

    #[test]
    fn lsof_dedupes_repeated_pid_port_pairs() {
        let out = "p1\nn*:3000\nn0.0.0.0:3000\n";
        // Both `n` lines resolve to the same (pid, port); only one survives, but sorting must
        // still work when a later duplicate isn't byte-identical to the first.
        let got = parse_lsof(out, &[1]);
        assert_eq!(got.iter().filter(|&&(pid, port)| pid == 1 && port == 3000).count(), 1);
    }

    #[test]
    fn lsof_sorts_by_port_then_pid() {
        let out = "\
p20
n*:9000
p10
n*:3000
p5
n*:3000
";
        assert_eq!(parse_lsof(out, &[5, 10, 20]), vec![(5, 3000), (10, 3000), (20, 9000)]);
    }

    #[test]
    fn lsof_empty_input_and_no_matching_pids() {
        assert_eq!(parse_lsof("", &[1, 2]), vec![]);
        assert_eq!(parse_lsof("p1\nn*:80\n", &[]), vec![]);
    }

    // -- parse_ss, using realistic `ss -H -ltnp` fixtures --------------------------------------

    /// A representative capture: IPv4 wildcard with two sharing processes, IPv4 loopback,
    /// IPv6 wildcard, an interface-scoped loopback address, and one line lacking process info
    /// (no `-p` permission) that must be skipped entirely.
    fn ss_fixture() -> &'static str {
        "LISTEN 0      511          0.0.0.0:80         0.0.0.0:*    users:((\"nginx\",pid=1234,fd=6),(\"nginx\",pid=1235,fd=6))\n\
         LISTEN 0      128                *:22               *:*    users:((\"sshd\",pid=998,fd=3))\n\
         LISTEN 0      4096         127.0.0.1:6379       0.0.0.0:*    users:((\"redis-server\",pid=1500,fd=6))\n\
         LISTEN 0      128            [::]:22            [::]:*      users:((\"sshd\",pid=998,fd=4))\n\
         LISTEN 0      511      127.0.0.1%lo:53          0.0.0.0:*    users:((\"systemd-resolve\",pid=700,fd=13))\n\
         LISTEN 0      128               *:9000              *:*\n"
    }

    #[test]
    fn ss_extracts_port_from_local_address_column() {
        let got = parse_ss(ss_fixture(), &[1234, 1235, 998, 1500, 700]);
        assert!(got.contains(&(1234, 80)));
        assert!(got.contains(&(1235, 80)));
        assert!(got.contains(&(998, 22)));
        assert!(got.contains(&(1500, 6379)));
        assert!(got.contains(&(700, 53)));
    }

    #[test]
    fn ss_line_without_users_paren_is_skipped() {
        // Port 9000 has no `users:((...))`, so it must not appear even if its pid were allowed.
        let got = parse_ss(ss_fixture(), &(1..=9999).collect::<Vec<u32>>());
        assert!(!got.iter().any(|&(_, port)| port == 9000));
    }

    #[test]
    fn ss_several_processes_sharing_one_socket_both_kept_when_both_allowed() {
        let got = parse_ss(ss_fixture(), &[1234, 1235]);
        assert_eq!(got, vec![(1234, 80), (1235, 80)]);
    }

    #[test]
    fn ss_keeps_only_pids_in_the_filter_list() {
        let got = parse_ss(ss_fixture(), &[1234]);
        assert_eq!(got, vec![(1234, 80)]);
    }

    #[test]
    fn ss_handles_ipv6_bracketed_address() {
        let got = parse_ss(ss_fixture(), &[998]);
        assert!(got.contains(&(998, 22)));
    }

    #[test]
    fn ss_strips_iface_scope_id_before_the_port() {
        let got = parse_ss(ss_fixture(), &[700]);
        assert_eq!(got, vec![(700, 53)]);
    }

    #[test]
    fn ss_handles_ipv6_bracket_with_iface_scope() {
        let line = "LISTEN 0 128 [fe80::1%eth0]:8080 [::]:* users:((\"myapp\",pid=42,fd=9))\n";
        assert_eq!(parse_ss(line, &[42]), vec![(42, 8080)]);
    }

    #[test]
    fn ss_dedupes_and_sorts_by_port_then_pid() {
        let out = "\
LISTEN 0 128 *:9000 *:* users:((\"b\",pid=20,fd=1))\n\
LISTEN 0 128 *:3000 *:* users:((\"a\",pid=10,fd=1))\n\
LISTEN 0 128 *:3000 *:* users:((\"a\",pid=5,fd=1))\n\
LISTEN 0 128 *:3000 *:* users:((\"a\",pid=5,fd=1))\n";
        assert_eq!(parse_ss(out, &[5, 10, 20]), vec![(5, 3000), (10, 3000), (20, 9000)]);
    }

    #[test]
    fn ss_empty_input_and_blank_lines() {
        assert_eq!(parse_ss("", &[1]), vec![]);
        assert_eq!(parse_ss("\n\n", &[1]), vec![]);
    }
}
