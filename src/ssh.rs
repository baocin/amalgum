//! ssh and tmux argv builders (§5.28). The app never talks SSH itself: it runs the system
//! `ssh` over one ControlMaster per host, so `~/.ssh/config`, agents, ProxyJump, and hardware
//! keys work for free. This module only builds argument vectors and parses ssh's output.
//!
//! Remote commands reach the remote shell as a single string, so every argument is quoted with
//! [`quote`]. A leading `~/` (or a bare `~`) is left unquoted so the remote shell expands it.
//!
//! ControlPath: `<control_dir>/<16 hex of fnv1a64(host)>`. Unix socket paths are limited to 104
//! bytes on macOS and ssh appends a 17-byte temporary suffix while creating the socket, so the
//! name is kept short instead of using `%C`, and [`Conn::master_args`] refuses a control dir too
//! long for even that ([`MAX_CONTROL_PATH_BYTES`]). The `-o ControlPath=…` value is quoted for
//! ssh's own option parser, since the macOS control dir contains a space.
//! Host keys are only ever accepted through the W16 dialog: [`Conn::master_args`] with
//! `trust_new_host_key` adds `StrictHostKeyChecking=accept-new` for that one connection. Never
//! `StrictHostKeyChecking=no`.

use std::io;
use std::path::{Path, PathBuf};

/// Private tmux server name (`tmux -L amalgum`).
pub const TMUX_SERVER: &str = "amalgum";
/// Uploaded to `~/.amalgum/tmux.conf`: tmux stays invisible (§5.28).
pub const TMUX_CONF: &str = include_str!("../assets/tmux.conf");

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_cli.rs"));
}

/// The CLI binary for a remote target, if this build embedded one (release builds only; see
/// `build.rs`). Uploaded to `~/.amalgum/bin/<version>/amalgum` on connect.
pub fn embedded_cli(target: &str) -> Option<&'static [u8]> {
    embedded::EMBEDDED_CLI.iter().find(|(t, _)| *t == target).map(|(_, bin)| *bin)
}

/// Characters that need no quoting in a shell word: safe outside quotes in any position,
/// including as the first character (so a lone `-oProxyCommand=x`-shaped argument stays inert:
/// the shell never treats it as anything but a literal string). The one exception is a leading
/// `=`, which zsh (a common login shell, so what sshd often runs the command with) replaces with
/// the path of the command it names; [`quote_literal`] quotes that case.
fn is_shell_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-')
}

/// POSIX-shell quote one argument for a remote command line.
///
/// Characters outside `[A-Za-z0-9_@%+=:,./-]`, or a leading `=`, force single-quoting, with
/// embedded `'` escaped as `'\''`. An empty string becomes `''`. A bare `~` or a leading `~/` is
/// left unquoted (the rest of a `~/...` path is still quoted) so the remote shell performs tilde
/// expansion against its own `$HOME`; any other tilde form (`~user`, a `~` not at the start) is
/// quoted like any other character and stays inert.
pub fn quote(arg: &str) -> String {
    if arg == "~" {
        return "~".to_string();
    }
    if let Some(rest) = arg.strip_prefix("~/") {
        return format!("~/{}", quote(rest));
    }
    quote_literal(arg)
}

/// Like [`quote`], but a leading `~` is quoted too, so the shell expands nothing: for words that
/// are the user's own (`amalgum run -- ls '~/x'`), not paths on a remote host.
pub fn quote_literal(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    if !arg.starts_with('=') && arg.chars().all(is_shell_safe) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Quote and join an argv into one remote command string.
pub fn remote_command(argv: &[&str]) -> String {
    argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// True when `host` is safe to place as a positional ssh argument: non-empty, does not start
/// with `-` (which ssh's own argument parser would read as an option), and has no whitespace or
/// control characters. [`Conn`]'s argv builders do not re-check this — validating a host before
/// constructing a `Conn` is the caller's responsibility (there is no way for an argv builder to
/// make a leading-`-` host inert after the fact, since ssh parses its own options before it ever
/// sees a `--`).
pub fn valid_host(host: &str) -> bool {
    !host.is_empty() && !host.starts_with('-') && host.chars().all(|c| !c.is_whitespace() && !c.is_control())
}

#[derive(Debug, Clone)]
pub struct MasterOptions {
    pub persist: String,
    /// `(ServerAliveInterval, ServerAliveCountMax)`. `None` when the user's ssh config already
    /// sets them (see [`user_sets_keepalive`]): `-o` on the command line would override it.
    pub keepalive: Option<(u32, u32)>,
}

impl Default for MasterOptions {
    fn default() -> Self {
        Self { persist: "10m".into(), keepalive: Some((20, 2)) }
    }
}

/// Given `ssh -G <host>` output, does the user's config set `serveraliveinterval` (non-zero)?
pub fn user_sets_keepalive(ssh_g: &str) -> bool {
    ssh_g
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            let key = words.next()?;
            key.eq_ignore_ascii_case("serveraliveinterval")
                .then(|| words.next())
                .flatten()?
                .parse::<u32>()
                .ok()
        })
        .is_some_and(|interval| interval != 0)
}

/// The longest ControlPath ssh can create a master socket at. `sockaddr_un.sun_path` holds 104
/// bytes on macOS (108 on Linux; the smaller counts everywhere, as for the app socket) including
/// the NUL, and ssh first binds `<ControlPath>.<16 random chars>` before renaming it into place.
pub const MAX_CONTROL_PATH_BYTES: usize = 104 - 1 - 17;

/// One host's connection parameters.
#[derive(Debug, Clone)]
pub struct Conn {
    pub host: String,
    pub control_dir: PathBuf,
}

impl Conn {
    /// `<control_dir>/<16 lowercase hex chars of fnv1a64(host)>`. Always exactly
    /// `control_dir`'s length plus 17 bytes (one separator, 16 hex digits), regardless of host.
    pub fn control_path(&self) -> PathBuf {
        let hash = crate::util::fnv1a64(self.host.as_bytes());
        self.control_dir.join(format!("{hash:016x}"))
    }

    /// `ControlPath="<control_path>"`. ssh re-splits every `-o` value on whitespace like a config
    /// line and expands `%` tokens in this one, and the default macOS control dir has a space
    /// in it (`~/Library/Application Support/…`): the path is double-quoted, with `\` and `"`
    /// backslash-escaped and `%` doubled, so ssh reads it back verbatim.
    fn control_path_option(&self) -> String {
        let path = self.control_path().display().to_string();
        let escaped = path.replace('\\', r"\\").replace('"', r#"\""#).replace('%', "%%");
        format!("ControlPath=\"{escaped}\"")
    }

    /// `-o ControlMaster=auto -o ControlPath=… -o ControlPersist=… [-o ServerAliveInterval=…
    /// -o ServerAliveCountMax=…] [-o StrictHostKeyChecking=accept-new] -N -f <host>`.
    ///
    /// `InvalidInput` when the control path is over [`MAX_CONTROL_PATH_BYTES`]: ssh could not
    /// create the master's socket there (with the default macOS control dir, a user name over
    /// 18 bytes), and would only fail later with a less clear message.
    pub fn master_args(&self, opts: &MasterOptions, trust_new_host_key: bool) -> io::Result<Vec<String>> {
        let path = self.control_path();
        if path.as_os_str().len() > MAX_CONTROL_PATH_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "ssh control socket path too long ({} bytes, max {MAX_CONTROL_PATH_BYTES}): {}",
                    path.as_os_str().len(),
                    path.display()
                ),
            ));
        }
        let mut args = vec![
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            self.control_path_option(),
            "-o".to_string(),
            format!("ControlPersist={}", opts.persist),
        ];
        if let Some((interval, count)) = opts.keepalive {
            args.push("-o".into());
            args.push(format!("ServerAliveInterval={interval}"));
            args.push("-o".into());
            args.push(format!("ServerAliveCountMax={count}"));
        }
        if trust_new_host_key {
            args.push("-o".into());
            args.push("StrictHostKeyChecking=accept-new".into());
        }
        args.push("-N".into());
        args.push("-f".into());
        args.push(self.host.clone());
        Ok(args)
    }

    /// `-o ControlPath=<path>`: every command after `master_args` must name the master's
    /// socket, or ssh looks for the user's default ControlPath and misses ours.
    fn via_master(&self, rest: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut args = vec!["-o".to_string(), self.control_path_option()];
        args.extend(rest);
        args
    }

    /// `-o ControlPath=… -O check <host>`: is the master alive?
    pub fn check_args(&self) -> Vec<String> {
        self.via_master(["-O".into(), "check".into(), self.host.clone()])
    }

    /// `-o ControlPath=… -O exit <host>`.
    pub fn exit_args(&self) -> Vec<String> {
        self.via_master(["-O".into(), "exit".into(), self.host.clone()])
    }

    /// Run a command on the master: `-o ControlPath=… -o ControlMaster=no <host> -- <quoted
    /// argv>`. `ControlMaster=no` means this fails cleanly instead of starting a new master when
    /// none is running.
    pub fn exec_args(&self, remote_argv: &[&str]) -> Vec<String> {
        self.via_master([
            "-o".into(),
            "ControlMaster=no".into(),
            self.host.clone(),
            "--".into(),
            remote_command(remote_argv),
        ])
    }

    /// Reverse-forward the app socket: `-o ControlPath=… -O forward -R <remote_sock>:<local_sock>
    /// -o StreamLocalBindUnlink=yes <host>`.
    pub fn forward_socket_args(&self, remote_sock: &str, local_sock: &Path) -> Vec<String> {
        self.via_master([
            "-O".into(),
            "forward".into(),
            "-R".into(),
            format!("{remote_sock}:{}", local_sock.display()),
            "-o".into(),
            "StreamLocalBindUnlink=yes".into(),
            self.host.clone(),
        ])
    }

    /// Open a remote port locally: `-o ControlPath=… -O forward -L <local>:localhost:<remote> <host>`.
    pub fn forward_port_args(&self, local_port: u16, remote_port: u16) -> Vec<String> {
        self.via_master([
            "-O".into(),
            "forward".into(),
            "-L".into(),
            format!("{local_port}:localhost:{remote_port}"),
            self.host.clone(),
        ])
    }

    /// `-o ControlPath=… -O cancel -L <local>:localhost:<remote> <host>`: undo a forward opened with
    /// [`forward_port_args`](Self::forward_port_args).
    pub fn cancel_port_args(&self, local_port: u16, remote_port: u16) -> Vec<String> {
        self.via_master([
            "-O".into(),
            "cancel".into(),
            "-L".into(),
            format!("{local_port}:localhost:{remote_port}"),
            self.host.clone(),
        ])
    }

    /// Environment pairs are quoted whole, so values are literal (no `~` or `$` expansion):
    /// pass absolute remote paths.
    ///
    /// Terminal with session survival: `-o ControlPath=… -t <host> -- tmux -L amalgum -f ~/.amalgum/tmux.conf
    /// new-session -A -s <session> -c <cwd> -e K=V …`.
    pub fn tmux_args(&self, session: &str, cwd: &str, env: &[(&str, &str)]) -> Vec<String> {
        let conf = format!("~/{}/tmux.conf", crate::paths::REMOTE_ROOT);
        let mut cmd = format!(
            "tmux -L {} -f {} new-session -A -s {} -c {}",
            quote(TMUX_SERVER),
            quote(&conf),
            quote(session),
            quote(cwd),
        );
        for (k, v) in env {
            cmd.push_str(&format!(" -e {}", quote(&format!("{k}={v}"))));
        }
        self.via_master(["-t".into(), self.host.clone(), "--".into(), cmd])
    }

    /// Terminal without tmux: `-o ControlPath=… -t <host> -- 'cd <cwd> && exec env K=V … "$SHELL" -l'`.
    pub fn shell_args(&self, cwd: &str, env: &[(&str, &str)]) -> Vec<String> {
        let mut cmd = format!("cd {} && exec env", quote(cwd));
        for (k, v) in env {
            cmd.push_str(&format!(" {}", quote(&format!("{k}={v}"))));
        }
        cmd.push_str(" \"$SHELL\" -l");
        self.via_master(["-t".into(), self.host.clone(), "--".into(), cmd])
    }
}

/// Reconnect delays in seconds: 3, 6, 12, 24, 48, then `cap` (default 60) forever.
#[derive(Debug, Clone)]
pub struct Backoff {
    next: u64,
    cap: u64,
}

impl Backoff {
    pub fn new(cap: u64) -> Self {
        Self { next: 3, cap }
    }
    pub fn reset(&mut self) {
        self.next = 3;
    }
}

impl Iterator for Backoff {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        let value = self.next.min(self.cap);
        self.next = self.next.saturating_mul(2);
        Some(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyPrompt {
    pub host: String,
    /// `ED25519`.
    pub key_type: String,
    /// `SHA256:…`.
    pub fingerprint: String,
}

/// Extract the unknown-host-key details from ssh's stderr ("The authenticity of host … can't be
/// established. ED25519 key fingerprint is SHA256:…").
pub fn parse_host_key_prompt(stderr: &str) -> Option<HostKeyPrompt> {
    let host_line = stderr.lines().find(|l| l.contains("The authenticity of host"))?;
    let after_quote = &host_line[host_line.find('\'')? + 1..];
    let quoted = &after_quote[..after_quote.find('\'')?];
    // `host` or `host (1.2.3.4)`: only the alias itself is wanted.
    let host = quoted.split_whitespace().next()?.to_string();

    let fp_line = stderr.lines().find(|l| l.contains("key fingerprint is"))?;
    let key_type = fp_line.split_whitespace().next()?.to_string();
    let fp_start = fp_line.find("SHA256:")?;
    let fingerprint = fp_line[fp_start..].split_whitespace().next()?.trim_end_matches('.').to_string();

    Some(HostKeyPrompt { host, key_type, fingerprint })
}

/// Rust target of the CLI binary to upload for `uname -sm` output (§5.31 "Remote build").
pub fn target_for_uname(uname_sm: &str) -> Option<&'static str> {
    let mut parts = uname_sm.split_whitespace();
    let os = parts.next()?;
    let arch = parts.next()?;
    match (os, arch) {
        ("Linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("Linux", "aarch64" | "arm64") => Some("aarch64-unknown-linux-musl"),
        ("Darwin", "arm64") => Some("aarch64-apple-darwin"),
        ("Darwin", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

/// Concrete `Host` aliases from an ssh config (no wildcards or negations), for W15.
pub fn config_hosts(config: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for raw_line in config.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, rest) = split_directive(line);
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        for tok in rest.split_whitespace() {
            if tok.starts_with('!') || tok.contains('*') || tok.contains('?') {
                continue;
            }
            if !hosts.iter().any(|h| h == tok) {
                hosts.push(tok.to_string());
            }
        }
    }
    hosts
}

/// Split an `ssh_config` directive line into `(keyword, rest)`, accepting both the usual
/// `Keyword value` form and the `Keyword=value` form.
fn split_directive(line: &str) -> (&str, &str) {
    let end = line.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(line.len());
    let keyword = &line[..end];
    let rest = line[end..].trim_start_matches([' ', '\t', '=']);
    (keyword, rest.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    // --- quote --------------------------------------------------------------------------

    #[test]
    fn quote_table() {
        let cases: &[(&str, &str)] = &[
            ("", "''"),
            ("abc123", "abc123"),
            ("a/b-c.d_e@f%25+=:,", "a/b-c.d_e@f%25+=:,"),
            ("-rf", "-rf"),
            ("~", "~"),
            ("~/foo", "~/foo"),
            ("~/foo bar", "~/'foo bar'"),
            ("~user", "'~user'"),
            ("$HOME", "'$HOME'"),
            ("`echo hi`", "'`echo hi`'"),
            ("$(echo hi)", "'$(echo hi)'"),
            ("with space", "'with space'"),
            ("*.rs", "'*.rs'"),
            ("new\nline", "'new\nline'"),
            ("a'b", "'a'\\''b'"),
            // zsh's EQUALS expansion turns a leading `=word` into the path of command `word`.
            ("=notes.md", "'=notes.md'"),
            ("=", "'='"),
        ];
        for (input, want) in cases {
            assert_eq!(&quote(input), want, "quote({input:?})");
        }
    }

    #[test]
    fn quote_literal_quotes_a_leading_tilde_too() {
        let cases: &[(&str, &str)] = &[
            ("~", "'~'"),
            ("~/x", "'~/x'"),
            ("~/foo bar", "'~/foo bar'"),
            ("=notes.md", "'=notes.md'"),
            ("plain", "plain"),
            ("", "''"),
        ];
        for (input, want) in cases {
            assert_eq!(&quote_literal(input), want, "quote_literal({input:?})");
        }
    }

    /// Arguments that must reach a remote shell unchanged: substitutions, globs, separators,
    /// quotes, newlines, tilde forms other than `~` and `~/`, and a leading `=` (zsh).
    const HOSTILE: &[&str] = &[
        "simple",
        "with space",
        "$(rm -rf /)",
        "`echo hi`",
        "new\nline",
        "*.rs",
        "-rf",
        "--flag=value",
        "a'b",
        "''",
        "",
        "~user",
        "~alice/foo",
        "$HOME",
        "glob[abc]",
        "semi;colon|pipe&amp",
        "trailing-newline\n",
        "=notes.md",
        "=ls",
        "%1",
        "a=b",
    ];

    /// What `<shell> -c "printf '%s\n' <quoted>"` prints, minus the one newline `printf` adds (so
    /// an argument that itself ends in `\n` still round-trips exactly); `None` when the shell is
    /// not installed.
    fn printed_by(shell: &[&str], quoted: &str) -> Option<String> {
        let script = format!("printf '%s\\n' {quoted}");
        let out = match Command::new(shell[0]).args(&shell[1..]).arg("-c").arg(&script).output() {
            Ok(out) => out,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => panic!("spawn {shell:?}: {e}"),
        };
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{shell:?} failed on {script:?}: {stderr}");
        let got = String::from_utf8_lossy(&out.stdout);
        Some(got.strip_suffix('\n').unwrap_or(&got).to_string())
    }

    /// Round-trip the hostile table through a real POSIX shell and check it reproduces the
    /// original text exactly.
    #[test]
    fn quote_round_trips_through_sh() {
        for &arg in HOSTILE {
            let quoted = quote(arg);
            assert_eq!(printed_by(&["sh"], &quoted).expect("sh"), arg, "round trip via {quoted:?}");
        }
    }

    /// zsh, the macOS default login shell and so often the one sshd runs a remote command with,
    /// expands more than POSIX sh (a leading `=`). Skipped where no `zsh` is installed; `-f`
    /// keeps the developer's rc files out.
    #[test]
    fn quote_round_trips_through_zsh() {
        for &arg in HOSTILE {
            let quoted = quote(arg);
            let Some(got) = printed_by(&["zsh", "-f"], &quoted) else {
                eprintln!("skipped: no zsh on PATH");
                return;
            };
            assert_eq!(got, arg, "round trip via {quoted:?}");
        }
    }

    /// `quote_literal` keeps even `~` and `~/…` from being expanded.
    #[test]
    fn quote_literal_round_trips_through_sh() {
        for &arg in HOSTILE.iter().chain(&["~", "~/x", "~/foo bar"]) {
            let quoted = quote_literal(arg);
            assert_eq!(printed_by(&["sh"], &quoted).expect("sh"), arg, "round trip via {quoted:?}");
        }
    }

    /// `~` and `~/...` must actually expand against the shell's `$HOME`, not be treated as a
    /// literal tilde.
    #[test]
    fn quote_tilde_expands_home_in_sh() {
        let home = std::env::var("HOME").expect("HOME must be set to test tilde expansion");
        let cases: &[&str] = &["~", "~/foo", "~/foo bar", "~/.amalgum/bin", "~/a/b c/d"];
        for &arg in cases {
            let quoted = quote(arg);
            let script = format!("printf '%s\\n' {quoted}");
            let out = Command::new("sh").arg("-c").arg(&script).output().expect("spawn sh");
            assert!(out.status.success());
            let got = String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string();
            let want = if arg == "~" { home.clone() } else { format!("{home}{}", &arg[1..]) };
            assert_eq!(got, want, "tilde expansion for {arg:?}");
        }
    }

    #[test]
    fn quote_hostile_inputs_stay_inert_when_run() {
        // Each of these, if unquoted, would run a command, expand a variable, read another
        // user's home, or glob-expand. Round-tripping through sh above already proves they come
        // back byte-identical; this only pins the "these must be single-quoted" property that
        // makes that possible.
        for hostile in ["`id`", "$(id)", "$IFS", "~root", "a b", "*"] {
            let q = quote(hostile);
            assert!(q.starts_with('\'') && q.ends_with('\''), "{hostile:?} -> {q:?} must be quoted");
        }
    }

    #[test]
    fn remote_command_joins_quoted_args() {
        assert_eq!(remote_command(&["git", "-C", "~/repo", "log", "a b"]), "git -C ~/repo log 'a b'");
        assert_eq!(remote_command(&[]), "");
    }

    // --- valid_host -----------------------------------------------------------------------

    #[test]
    fn valid_host_cases() {
        for ok in ["gpu-box", "10.0.0.5", "user@gpu-box", "box.internal.example.com"] {
            assert!(valid_host(ok), "{ok:?} should be valid");
        }
        for bad in ["-oProxyCommand=x", "-", "", "has space", "tab\there", "new\nline", "\u{7}bell"] {
            assert!(!valid_host(bad), "{bad:?} should be rejected");
        }
    }

    // --- Conn -------------------------------------------------------------------------------

    fn conn(host: &str, dir: &str) -> Conn {
        Conn { host: host.to_string(), control_dir: PathBuf::from(dir) }
    }

    #[test]
    fn control_path_is_stable_and_short() {
        let a = conn("gpu-box", "run/ssh");
        let b = conn("gpu-box", "run/ssh");
        assert_eq!(a.control_path(), b.control_path(), "same host must hash the same way");
        assert_ne!(a.control_path(), conn("other-box", "run/ssh").control_path());

        let name = a.control_path().file_name().expect("filename").to_string_lossy().into_owned();
        assert_eq!(name.len(), 16, "control_path uses exactly 16 hex chars");
        assert!(name.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    /// docs/SPEC.md §1 portability + CLAUDE.md: macOS caps `sockaddr_un.sun_path` at 104 bytes
    /// including the terminating NUL, and ssh first binds `<ControlPath>.<16 random chars>` (17
    /// more bytes) before renaming it into place. `control_path` always adds exactly one
    /// separator plus 16 hex chars (17 bytes) to `control_dir`, regardless of host. With the
    /// default macOS control dir that leaves room for a user name of up to 18 bytes; past that
    /// ssh could not create the master's socket, and `master_args` says so up front.
    #[test]
    fn master_args_refuse_a_control_path_ssh_cannot_bind_on_macos() {
        let control_dir = |user: &str| format!("/Users/{user}/Library/Application Support/amalgum/run/ssh"); // portability: allow
        let longest = conn("gpu-box.example.internal.corp", &control_dir("abcdefghijklmnopqr"));
        let path_len = longest.control_path().as_os_str().len();
        assert_eq!(path_len, control_dir("abcdefghijklmnopqr").len() + 17, "always exactly 17 bytes more");
        assert_eq!(path_len + 17 + 1, 104, "ssh's temporary suffix and the NUL fill sun_path exactly");
        assert!(longest.master_args(&MasterOptions::default(), false).is_ok());

        let too_long = conn("gpu-box", &control_dir("christopher.johnson"));
        let err = too_long.master_args(&MasterOptions::default(), false).expect_err("ssh can't bind it");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains(&too_long.control_path().display().to_string()), "{err}");
    }

    /// ssh re-splits every `-o` value like a config line and expands `%` tokens in ControlPath,
    /// so the default macOS control dir (`~/Library/Application Support/…`) must be quoted.
    #[test]
    fn control_path_option_is_one_literal_token_for_ssh() {
        let c = conn("gpu-box", "/Users/u/Library/Application Support/100%/a\"b\\c/ssh"); // portability: allow
        let hash = c.control_path().file_name().expect("file name").to_string_lossy().into_owned();
        let want = format!(r#"ControlPath="/Users/u/Library/Application Support/100%%/a\"b\\c/ssh/{hash}""#); // portability: allow
        assert_eq!(c.master_args(&MasterOptions::default(), false).unwrap()[3], want);
        assert_eq!(c.check_args()[1], want);
    }

    /// `ssh -G` parses the options and prints the resulting config without connecting or touching
    /// the filesystem, so a short relative dir keeps the space and `%` without tripping the macOS
    /// length limit ($TMPDIR alone is ~50 bytes there). Skipped where no `ssh` is installed.
    #[test]
    fn control_path_option_survives_ssh_option_parsing() {
        let control_dir = PathBuf::from("Application Support/100%/ssh");
        let c = Conn { host: "gpu-box".into(), control_dir };
        for args in [c.master_args(&MasterOptions::default(), false).unwrap(), c.exec_args(&["true"])] {
            let out = match Command::new("ssh").args(["-F", "none", "-G"]).args(&args).output() {
                Ok(out) => out,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("skipped: no ssh on PATH");
                    return;
                }
                Err(e) => panic!("spawn ssh: {e}"),
            };
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "ssh -G rejected {args:?}: {stderr}");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let parsed = stdout.lines().find_map(|l| l.strip_prefix("controlpath ")).expect("controlpath");
            assert_eq!(Path::new(parsed), c.control_path(), "{args:?}");
        }
    }

    #[test]
    fn master_args_default_options() {
        let c = conn("gpu-box", "run/ssh");
        let opts = MasterOptions::default();
        let args = c.master_args(&opts, false).unwrap();
        assert_eq!(
            args,
            vec![
                "-o",
                "ControlMaster=auto",
                "-o",
                &format!("ControlPath=\"{}\"", c.control_path().display()),
                "-o",
                "ControlPersist=10m",
                "-o",
                "ServerAliveInterval=20",
                "-o",
                "ServerAliveCountMax=2",
                "-N",
                "-f",
                "gpu-box",
            ]
        );
    }

    #[test]
    fn master_args_without_keepalive() {
        let c = conn("gpu-box", "run/ssh");
        let opts = MasterOptions { persist: "10m".into(), keepalive: None };
        let args = c.master_args(&opts, false).unwrap();
        assert!(!args.iter().any(|a| a.starts_with("ServerAlive")), "keepalive omitted: {args:?}");
    }

    #[test]
    fn master_args_trust_new_host_key_uses_accept_new_never_no() {
        let c = conn("gpu-box", "run/ssh");
        let opts = MasterOptions::default();

        let trusted = c.master_args(&opts, true).unwrap();
        assert!(trusted.iter().any(|a| a == "StrictHostKeyChecking=accept-new"));

        let untrusted = c.master_args(&opts, false).unwrap();
        assert!(!untrusted.iter().any(|a| a.starts_with("StrictHostKeyChecking")));

        for args in [&trusted, &untrusted] {
            assert!(
                !args.iter().any(|a| a == "StrictHostKeyChecking=no"),
                "must never emit StrictHostKeyChecking=no"
            );
        }
    }

    #[test]
    fn exec_args_shape() {
        let c = conn("gpu-box", "run/ssh");
        let args = c.exec_args(&["git", "-C", "~/repo", "status"]);
        assert_eq!(
            args,
            vec![
                "-o".to_string(),
                format!("ControlPath=\"{}\"", c.control_path().display()),
                "-o".to_string(),
                "ControlMaster=no".to_string(),
                "gpu-box".to_string(),
                "--".to_string(),
                "git -C ~/repo status".to_string(),
            ]
        );
    }

    /// The `-o ControlPath="…"` pair every post-master command starts with.
    fn via(c: &Conn) -> Vec<String> {
        vec!["-o".into(), format!("ControlPath=\"{}\"", c.control_path().display())]
    }

    fn with_via(c: &Conn, rest: &[&str]) -> Vec<String> {
        via(c).into_iter().chain(rest.iter().map(|s| s.to_string())).collect()
    }

    #[test]
    fn tmux_args_shape() {
        let c = conn("gpu-box", "run/ssh");
        let args = c.tmux_args("tab-1", "/srv/app", &[("AMALGUM_SOCK", "~/.amalgum/run/app.sock")]); // portability: allow
        let cmd = "tmux -L amalgum -f ~/.amalgum/tmux.conf new-session -A -s tab-1 -c /srv/app -e 'AMALGUM_SOCK=~/.amalgum/run/app.sock'"; // portability: allow
        assert_eq!(args, with_via(&c, &["-t", "gpu-box", "--", cmd]));
    }

    #[test]
    fn tmux_args_quote_cwd_and_whole_env_pairs() {
        let c = conn("gpu-box", "run/ssh");
        let args = c.tmux_args("t", "/a b", &[("K", "v v"), ("X;rm", "1")]); // portability: allow
        assert_eq!(
            args.last().map(String::as_str),
            Some(
                "tmux -L amalgum -f ~/.amalgum/tmux.conf new-session -A -s t -c '/a b' -e 'K=v v' -e 'X;rm=1'"
            ) // portability: allow
        );
    }

    #[test]
    fn shell_args_shape() {
        let c = conn("gpu-box", "run/ssh");
        let args = c.shell_args("/srv/app", &[("AMALGUM_TAB", "t1")]); // portability: allow
        let cmd = "cd /srv/app && exec env AMALGUM_TAB=t1 \"$SHELL\" -l"; // portability: allow
        assert_eq!(args, with_via(&c, &["-t", "gpu-box", "--", cmd]));
    }

    #[test]
    fn shell_args_quotes_cwd() {
        let c = conn("gpu-box", "run/ssh");
        let args = c.shell_args("/a b", &[]); // portability: allow
        assert_eq!(args.last().map(String::as_str), Some("cd '/a b' && exec env \"$SHELL\" -l")); // portability: allow
    }

    #[test]
    fn every_post_master_command_names_the_control_path() {
        let c = conn("gpu-box", "run/ssh");
        let local = PathBuf::from("app.sock");
        for args in [
            c.check_args(),
            c.exit_args(),
            c.exec_args(&["true"]),
            c.forward_socket_args("r.sock", &local),
            c.forward_port_args(1, 2),
            c.cancel_port_args(1, 2),
            c.tmux_args("t", "d", &[]),
            c.shell_args("d", &[]),
        ] {
            assert_eq!(args[..2], via(&c)[..], "{args:?}");
        }
    }

    #[test]
    fn check_and_exit_args() {
        let c = conn("gpu-box", "run/ssh");
        assert_eq!(c.check_args(), with_via(&c, &["-O", "check", "gpu-box"]));
        assert_eq!(c.exit_args(), with_via(&c, &["-O", "exit", "gpu-box"]));
    }

    #[test]
    fn forward_socket_args_shape() {
        let c = conn("gpu-box", "run/ssh");
        let local = PathBuf::from("/tmp/app.sock"); // portability: allow
        let args = c.forward_socket_args("~/.amalgum/run/app.sock", &local); // portability: allow
        let spec = "~/.amalgum/run/app.sock:/tmp/app.sock"; // portability: allow
        assert_eq!(
            args,
            with_via(&c, &["-O", "forward", "-R", spec, "-o", "StreamLocalBindUnlink=yes", "gpu-box"])
        );
    }

    #[test]
    fn forward_and_cancel_port_args() {
        let c = conn("gpu-box", "run/ssh");
        assert_eq!(
            c.forward_port_args(3000, 8080),
            with_via(&c, &["-O", "forward", "-L", "3000:localhost:8080", "gpu-box"])
        );
        assert_eq!(
            c.cancel_port_args(3000, 8080),
            with_via(&c, &["-O", "cancel", "-L", "3000:localhost:8080", "gpu-box"])
        );
    }

    // --- Backoff ------------------------------------------------------------------------------

    #[test]
    fn backoff_sequence_default_cap() {
        let b = Backoff::new(60);
        let seq: Vec<u64> = b.take(8).collect();
        assert_eq!(seq, vec![3, 6, 12, 24, 48, 60, 60, 60]);
    }

    #[test]
    fn backoff_cap_smaller_than_48_caps_earlier() {
        let b = Backoff::new(10);
        let seq: Vec<u64> = b.take(6).collect();
        assert_eq!(seq, vec![3, 6, 10, 10, 10, 10]);
    }

    #[test]
    fn backoff_reset_restarts() {
        let mut b = Backoff::new(60);
        assert_eq!(b.next(), Some(3));
        assert_eq!(b.next(), Some(6));
        b.reset();
        assert_eq!(b.next(), Some(3));
    }

    // --- parse_host_key_prompt ----------------------------------------------------------------

    #[test]
    fn parse_host_key_prompt_ed25519_with_ip() {
        let stderr = "The authenticity of host 'gpu-box (10.0.0.5)' can't be established.\n\
             ED25519 key fingerprint is SHA256:abcd1234efgh5678ijkl9012mnop3456qrst7890uvw.\n\
             This host key is known by the following other names/addresses:\n\
             Are you sure you want to continue connecting (yes/no/[fingerprint])? ";
        let got = parse_host_key_prompt(stderr).expect("should parse");
        assert_eq!(
            got,
            HostKeyPrompt {
                host: "gpu-box".into(),
                key_type: "ED25519".into(),
                fingerprint: "SHA256:abcd1234efgh5678ijkl9012mnop3456qrst7890uvw".into(),
            }
        );
    }

    #[test]
    fn parse_host_key_prompt_rsa_no_ip() {
        let stderr = "The authenticity of host 'example.com' can't be established.\n\
             RSA key fingerprint is SHA256:zzzz9999.\n";
        let got = parse_host_key_prompt(stderr).expect("should parse");
        assert_eq!(got.host, "example.com");
        assert_eq!(got.key_type, "RSA");
        assert_eq!(got.fingerprint, "SHA256:zzzz9999");
    }

    #[test]
    fn parse_host_key_prompt_ecdsa() {
        let stderr = "The authenticity of host 'box (1.2.3.4)' can't be established.\n\
             ECDSA key fingerprint is SHA256:ffff0000.\n";
        let got = parse_host_key_prompt(stderr).expect("should parse");
        assert_eq!(got.key_type, "ECDSA");
    }

    #[test]
    fn parse_host_key_prompt_unrelated_stderr_is_none() {
        for stderr in [
            "ssh: Could not resolve hostname gpu-box: Name or service not known\n",
            "Permission denied (publickey).\n",
            "",
            "fatal: not a git repository\n",
        ] {
            assert_eq!(parse_host_key_prompt(stderr), None, "should not parse: {stderr:?}");
        }
    }

    // --- user_sets_keepalive ------------------------------------------------------------------

    #[test]
    fn user_sets_keepalive_cases() {
        assert!(!user_sets_keepalive("serveraliveinterval 0\nservealivecountmax 3\n"));
        assert!(user_sets_keepalive("hostname gpu-box\nserveraliveinterval 15\n"));
        assert!(user_sets_keepalive("ServerAliveInterval 30\n"), "keys are case-insensitive");
        assert!(!user_sets_keepalive("hostname gpu-box\n"), "absent means not set");
        assert!(!user_sets_keepalive(""));
    }

    // --- target_for_uname ----------------------------------------------------------------------

    #[test]
    fn target_for_uname_cases() {
        assert_eq!(target_for_uname("Linux x86_64"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(target_for_uname("Linux aarch64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(target_for_uname("Linux arm64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(target_for_uname("Darwin arm64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_for_uname("Darwin x86_64"), Some("x86_64-apple-darwin"));
        assert_eq!(target_for_uname("Linux riscv64"), None);
        assert_eq!(target_for_uname("FreeBSD x86_64"), None);
        assert_eq!(target_for_uname(""), None);
        assert_eq!(target_for_uname("Linux"), None);
    }

    // --- config_hosts --------------------------------------------------------------------------

    #[test]
    fn config_hosts_basic_filters_wildcards_and_negations() {
        assert_eq!(config_hosts("Host a b *.c !d\n"), vec!["a", "b"]);
    }

    #[test]
    fn config_hosts_dedups_keeping_first_order() {
        assert_eq!(config_hosts("Host a\nHost b a c\n"), vec!["a", "b", "c"]);
    }

    #[test]
    fn config_hosts_case_insensitive_keyword() {
        assert_eq!(config_hosts("host foo\nHOST bar\nHoSt baz\n"), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn config_hosts_equals_form() {
        assert_eq!(config_hosts("Host=a b\n"), vec!["a", "b"]);
    }

    #[test]
    fn config_hosts_ignores_match_blocks() {
        let config =
            "Match host special.example.com\n    HostName 10.0.0.5\n\nHost real\n    HostName 10.0.0.6\n";
        assert_eq!(config_hosts(config), vec!["real"]);
    }

    #[test]
    fn config_hosts_ignores_comments_and_blank_lines() {
        let config = "# a comment\n\nHost foo\n# Host commented-out\n\nHost bar\n";
        assert_eq!(config_hosts(config), vec!["foo", "bar"]);
    }

    #[test]
    fn config_hosts_empty_input() {
        assert_eq!(config_hosts(""), Vec::<String>::new());
    }
}
