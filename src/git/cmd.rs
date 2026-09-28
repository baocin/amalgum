//! Running git. Local: `git -C <path> <args>`. Remote: `ssh -o ControlPath=… <host> -- env
//! <K=V…> git -C <path> <args>` over the shared ControlMaster (§5.28), each argument
//! shell-quoted with `ssh::quote` because ssh hands the remote shell one string.
//!
//! Every invocation runs git with `GIT_TERMINAL_PROMPT=0` (the app has no TTY), `LC_ALL=C`
//! (stable messages), and `GIT_OPTIONAL_LOCKS=0` (status polling must not fight the user's
//! git). Locally they are set on the process; ssh forwards none of them, so remotely they are
//! part of the command itself (`env K=V…` above).

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use crate::ssh::{self, Conn};

/// The environment every git we run gets (see the module doc).
const GIT_ENV: &[(&str, &str)] =
    &[("GIT_TERMINAL_PROMPT", "0"), ("LC_ALL", "C"), ("GIT_OPTIONAL_LOCKS", "0")];

/// Environment variables that redirect git to a different repository, index, or object store
/// (`git rev-parse --local-env-vars`). Git exports several of them to hooks, so anything run
/// from a hook (the pre-commit gate's tests, an agent) would otherwise act on the *hook's*
/// repository instead of the `-C <path>` it asked for. Every git we spawn clears them.
pub const REPO_ENV_VARS: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// Subcommands that accept `--progress` (forcing progress output onto a non-TTY stderr).
const PROGRESS_SUBCOMMANDS: &[&str] = &["clone", "fetch", "pull", "push"];

/// How often [`Git::run_streaming`] checks its cancel flag while git is silent.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// The last stderr line of the [`GitError`] a cancelled [`Git::run_streaming`] returns.
pub const CANCELLED: &str = "Cancelled";

/// Where a workspace's repository lives.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Location {
    Local { path: PathBuf },
    Remote { host: String, path: String },
}

impl Location {
    /// `host:path` (host has no `/`, path non-empty) is remote; anything else is a local path.
    /// `./a:b`, `/x:y`, and `~/a:b` are local.
    pub fn parse(s: &str) -> Self {
        if let Some(idx) = s.find(':') {
            let host = &s[..idx];
            let path = &s[idx + 1..];
            let looks_local =
                host.is_empty() || host.contains('/') || host.starts_with('.') || host.starts_with('~');
            if !looks_local && !path.is_empty() {
                return Self::Remote { host: host.to_string(), path: path.to_string() };
            }
        }
        Self::Local { path: PathBuf::from(s) }
    }

    /// Short form for subtitles: `~/w/conduit` (home abbreviated) or `gpu-box:~/g`.
    pub fn display(&self, home: Option<&Path>) -> String {
        match self {
            Self::Local { path } => match home.and_then(|h| path.strip_prefix(h).ok()) {
                Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
                Some(rest) => format!("~/{}", rest.display()),
                None => path.display().to_string(),
            },
            // The remote path is whatever the user gave (often already `~/...`); there is no
            // local knowledge of the remote host's home directory to abbreviate against.
            Self::Remote { host, path } => format!("{host}:{path}"),
        }
    }
}

/// A failed git (or ssh) invocation. The toast shows [`GitError::summary`]; **Details** shows
/// `command` and `stderr` verbatim (§5.24: the app never swallows stderr).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError {
    pub command: String,
    pub code: Option<i32>,
    pub stderr: String,
}

impl GitError {
    /// One human line for known failures, else the last non-empty stderr line:
    /// - auth failure → "Authentication failed. Check your SSH agent or credential helper."
    /// - non-fast-forward rejection → "Push rejected: remote has commits you don't have"
    /// - not a repository → "Not a git repository"
    /// - host key verification failed → "Unknown or changed host key"
    /// - network ("Could not resolve host", "Connection refused/timed out") → "Offline"
    /// - local changes would be overwritten → "Checkout would overwrite N files"
    pub fn summary(&self) -> String {
        if let Some(known) = known_summary(&self.stderr) {
            return known;
        }
        if let Some(last) = self.stderr.lines().map(str::trim).rfind(|l| !l.is_empty()) {
            return last.to_string();
        }
        match self.code {
            Some(n) => format!("git exited with code {n}"),
            None => "git failed to run".to_string(),
        }
    }
}

fn known_summary(stderr: &str) -> Option<String> {
    if stderr.contains("Permission denied (publickey")
        || stderr.contains("Authentication failed")
        || stderr.contains("could not read Username")
        || stderr.contains("could not read Password")
    {
        return Some("Authentication failed. Check your SSH agent or credential helper.".to_string());
    }
    if stderr.contains("non-fast-forward") || stderr.contains("Updates were rejected") {
        return Some("Push rejected: remote has commits you don't have".to_string());
    }
    if stderr.contains("not a git repository") {
        return Some("Not a git repository".to_string());
    }
    if stderr.contains("Host key verification failed") {
        return Some("Unknown or changed host key".to_string());
    }
    if stderr.contains("Could not resolve host")
        || stderr.contains("Could not resolve hostname")
        || stderr.contains("Connection refused")
        || stderr.contains("Connection timed out")
        || stderr.contains("Operation timed out")
    {
        return Some("Offline".to_string());
    }
    if let Some(files) = overwritten_files(stderr) {
        return Some(format!("Checkout would overwrite {} files", files.len()));
    }
    None
}

/// The indented file names git lists after "...would be overwritten by
/// checkout/merge/rebase/...:" (local changes and untracked files alike).
pub(crate) fn overwritten_files(stderr: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = stderr.lines().collect();
    let start = lines.iter().position(|l| l.contains("would be overwritten by"))?;
    let files: Vec<String> = lines[start + 1..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .take_while(|l| l.starts_with(char::is_whitespace))
        .map(|l| l.trim().to_string())
        .collect();
    (!files.is_empty()).then_some(files)
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary())
    }
}

impl std::error::Error for GitError {}

/// A git runner bound to one location.
#[derive(Debug, Clone)]
pub struct Git {
    pub location: Location,
    /// `git` binary (Settings → Git → path). Default `"git"`.
    pub git_bin: String,
    /// `ssh` binary for remote locations. Default `"ssh"`.
    pub ssh_bin: String,
    /// ControlMaster socket directory (`Dirs::ssh_control_dir`); remote only.
    pub control_dir: Option<PathBuf>,
}

impl Git {
    pub fn new(location: Location) -> Self {
        Self { location, git_bin: "git".to_string(), ssh_bin: "ssh".to_string(), control_dir: None }
    }

    /// The full argv that would run, for logs and the error details pane.
    pub fn argv(&self, args: &[&str]) -> Vec<String> {
        match &self.location {
            Location::Local { path } => {
                let mut v = vec![self.git_bin.clone(), "-C".to_string(), path.display().to_string()];
                v.extend(args.iter().map(|a| a.to_string()));
                v
            }
            Location::Remote { host, path } => {
                // sshd does not pass our environment through: `env K=V … git …` on the host.
                let mut remote_argv: Vec<String> = vec!["env".to_string()];
                remote_argv.extend(GIT_ENV.iter().map(|(k, v)| format!("{k}={v}")));
                remote_argv.extend(["git".to_string(), "-C".to_string(), path.clone()]);
                remote_argv.extend(args.iter().map(|a| a.to_string()));
                let remote_refs: Vec<&str> = remote_argv.iter().map(String::as_str).collect();

                let mut v = vec![self.ssh_bin.clone()];
                match &self.control_dir {
                    Some(control_dir) => {
                        let conn = Conn { host: host.clone(), control_dir: control_dir.clone() };
                        v.extend(conn.exec_args(&remote_refs));
                    }
                    // No control directory configured: drop the ControlPath/ControlMaster
                    // options that `Conn::exec_args` would add and just address the host.
                    None => {
                        v.push(host.clone());
                        v.push("--".to_string());
                        v.push(ssh::remote_command(&remote_refs));
                    }
                }
                v
            }
        }
    }

    /// A ready-to-spawn command (env set, stdin null unless the caller changes it).
    pub fn command(&self, args: &[&str]) -> Command {
        let argv = self.argv(args);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        for var in REPO_ENV_VARS {
            cmd.env_remove(var);
        }
        cmd.envs(GIT_ENV.iter().copied());
        cmd.stdin(Stdio::null());
        cmd
    }

    /// Run to completion; stdout on success.
    pub fn run(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
        self.run_inner(args, None)
    }

    /// Run with `input` piped to stdin (commit messages, patches for `apply --cached`).
    pub fn run_with_stdin(&self, args: &[&str], input: &[u8]) -> Result<Vec<u8>, GitError> {
        self.run_inner(args, Some(input))
    }

    /// Run a long network command (§5.3, §5.10): `--progress` is added to `clone`, `fetch`,
    /// `pull`, and `push` if missing, and `on_stderr` gets each stderr segment as it arrives —
    /// split on `\n` *and* `\r`, since git redraws progress in place with `\r` — on the calling
    /// thread. Setting `cancel` kills git and every process it started (remotely: the ssh client,
    /// which closes the remote git's pipes) and returns a [`GitError`] whose stderr ends with
    /// [`CANCELLED`] — unless git had already exited, whose own result is then returned. A flag
    /// already set spawns nothing. The error's stderr keeps whole lines, not progress redraws.
    pub fn run_streaming(
        &self,
        args: &[&str],
        cancel: &AtomicBool,
        on_stderr: &mut dyn FnMut(&str),
    ) -> Result<Vec<u8>, GitError> {
        let args = with_progress(args);
        let command_line = quoted_command_line(&self.argv(&args));
        let error = |code, stderr: String| GitError { command: command_line.clone(), code, stderr };
        let cancelled = |log: String| error(None, format!("{log}{CANCELLED}\n"));
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled(String::new()));
        }

        let mut cmd = self.command(&args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        // Its own session (so its own process group): cancel also kills the helpers git spawned
        // (index-pack, upload-pack, ssh), which would otherwise keep writing into a folder being
        // deleted. A new session, not just a group, detaches it from the app's controlling
        // terminal: an ssh wanting a passphrase or a host-key answer then fails fast (or uses
        // SSH_ASKPASS) instead of stopping on SIGTTIN/SIGTTOU as a background job would.
        // SAFETY: the closure runs between fork and exec and only calls setsid(2), which is
        // async-signal-safe and touches no memory.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
                if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
            });
        }
        let mut child = cmd.spawn().map_err(|e| error(None, e.to_string()))?;
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let stdout_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            split_segments(stderr, |segment, is_line| tx.send((segment, is_line)).is_ok())
        });

        let mut log = String::new();
        // Set once git has exited on its own: a cancel arriving after that reports what git did
        // (a push that landed must still be journaled), so only stderr is left to drain.
        let mut exited = false;
        loop {
            match rx.recv_timeout(CANCEL_POLL) {
                Ok((segment, is_line)) => {
                    on_stderr(&segment);
                    if is_line {
                        log.push_str(&segment);
                        log.push('\n');
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if exited || !cancel.load(Ordering::Relaxed) {
                continue;
            }
            if matches!(child.try_wait(), Ok(Some(_))) {
                exited = true;
                continue;
            }
            // No waiting for stderr's EOF: a killed group's pipes may close late.
            if let Ok(pgid) = libc::pid_t::try_from(child.id()) {
                // SAFETY: kill(2) takes no pointers; -pgid names the group spawned above.
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
            }
            let _ = child.kill();
            // Git may have exited successfully between `try_wait` and the kill.
            if !child.wait().is_ok_and(|status| status.success()) {
                return Err(cancelled(log));
            }
            exited = true;
        }

        let status = child.wait().map_err(|e| error(None, e.to_string()))?;
        let stdout = stdout_reader.join().unwrap_or_default();
        if status.success() { Ok(stdout) } else { Err(error(status.code(), log)) }
    }

    fn run_inner(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, GitError> {
        let argv = self.argv(args);
        let command_line = quoted_command_line(&argv);

        let mut cmd = self.command(args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        if input.is_some() {
            cmd.stdin(Stdio::piped());
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return Err(GitError { command: command_line, code: None, stderr: e.to_string() }),
        };

        if let Some(data) = input {
            // Write on a separate thread so a large payload can't deadlock against a child that
            // fills its stdout/stderr pipes before we get to `wait_with_output`.
            if let Some(mut stdin) = child.stdin.take() {
                let data = data.to_vec();
                let _ = std::thread::spawn(move || stdin.write_all(&data)).join();
            }
        }

        let output = match child.wait_with_output() {
            Ok(output) => output,
            Err(e) => return Err(GitError { command: command_line, code: None, stderr: e.to_string() }),
        };

        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(GitError {
                command: command_line,
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }
}

/// `args` with `--progress` inserted after a [`PROGRESS_SUBCOMMANDS`] subcommand lacking it.
fn with_progress<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut v = args.to_vec();
    if v.first().is_some_and(|sub| PROGRESS_SUBCOMMANDS.contains(sub)) && !v.contains(&"--progress") {
        v.insert(1, "--progress");
    }
    v
}

/// Reads `reader` to EOF, handing each non-empty segment to `emit` with `true` for a line (ended
/// by `\n`, `\r\n`, or EOF) and `false` for a progress redraw (ended by a lone `\r`). Stops early
/// when `emit` returns `false`.
fn split_segments(reader: impl Read, mut emit: impl FnMut(String, bool) -> bool) {
    let mut reader = reader;
    let mut segment = Vec::new();
    let mut after_cr = false;
    let mut flush = |segment: &mut Vec<u8>, is_line| {
        let keep_going = segment.is_empty() || emit(String::from_utf8_lossy(segment).into_owned(), is_line);
        segment.clear();
        keep_going
    };
    let mut chunk = [0u8; 4096];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &byte in &chunk[..n] {
            // A `\r` ends a progress redraw only once we know no `\n` follows it.
            if std::mem::take(&mut after_cr) {
                if !flush(&mut segment, byte == b'\n') {
                    return;
                }
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => after_cr = true,
                b'\n' if !flush(&mut segment, true) => return,
                b'\n' => {}
                _ => segment.push(byte),
            }
        }
    }
    flush(&mut segment, !after_cr);
}

/// The argv joined into one shell-quoted line, for [`GitError::command`] and logs.
fn quoted_command_line(argv: &[String]) -> String {
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    ssh::remote_command(&refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempRepo, hermetic_git};

    // --- Location::parse ------------------------------------------------------------------

    #[test]
    fn parse_remote() {
        assert_eq!(
            Location::parse("gpu-box:~/g"),
            Location::Remote { host: "gpu-box".into(), path: "~/g".into() }
        );
        assert_eq!(
            Location::parse("user@host:/srv/app"),
            Location::Remote { host: "user@host".into(), path: "/srv/app".into() }
        );
    }

    #[test]
    fn parse_local_cases() {
        assert_eq!(Location::parse("./a:b"), Location::Local { path: PathBuf::from("./a:b") });
        assert_eq!(Location::parse("/x:y"), Location::Local { path: PathBuf::from("/x:y") }); // portability: allow
        assert_eq!(Location::parse("~/a:b"), Location::Local { path: PathBuf::from("~/a:b") });
        assert_eq!(Location::parse("/plain/path"), Location::Local { path: PathBuf::from("/plain/path") }); // portability: allow
        assert_eq!(
            Location::parse("relative/path"),
            Location::Local { path: PathBuf::from("relative/path") }
        );
    }

    #[test]
    fn parse_rejects_empty_host_or_path() {
        assert_eq!(Location::parse(":path"), Location::Local { path: PathBuf::from(":path") });
        assert_eq!(Location::parse("host:"), Location::Local { path: PathBuf::from("host:") });
    }

    // --- Location::display -----------------------------------------------------------------

    #[test]
    fn display_local_abbreviates_home() {
        let home = Path::new("/Users/steele"); // portability: allow
        let loc = Location::Local { path: PathBuf::from("/Users/steele/w/conduit") }; // portability: allow
        assert_eq!(loc.display(Some(home)), "~/w/conduit");
    }

    #[test]
    fn display_local_exact_home_is_bare_tilde() {
        let home = Path::new("/Users/steele"); // portability: allow
        let loc = Location::Local { path: PathBuf::from("/Users/steele") }; // portability: allow
        assert_eq!(loc.display(Some(home)), "~");
    }

    #[test]
    fn display_local_outside_home_or_no_home() {
        let loc = Location::Local { path: PathBuf::from("/opt/other") }; // portability: allow
        assert_eq!(loc.display(Some(Path::new("/Users/steele"))), "/opt/other"); // portability: allow
        assert_eq!(loc.display(None), "/opt/other"); // portability: allow
    }

    #[test]
    fn display_remote_shows_host_colon_path_verbatim() {
        let loc = Location::Remote { host: "gpu-box".into(), path: "~/g".into() };
        assert_eq!(loc.display(None), "gpu-box:~/g");
        assert_eq!(loc.display(Some(Path::new("/Users/steele"))), "gpu-box:~/g"); // portability: allow
    }

    // --- Git::new / argv --------------------------------------------------------------------

    #[test]
    fn new_has_expected_defaults() {
        let git = Git::new(Location::Local { path: PathBuf::from("/repo") }); // portability: allow
        assert_eq!(git.git_bin, "git");
        assert_eq!(git.ssh_bin, "ssh");
        assert_eq!(git.control_dir, None);
    }

    #[test]
    fn argv_local() {
        let git = Git::new(Location::Local { path: PathBuf::from("/repo") }); // portability: allow
        assert_eq!(
            git.argv(&["status", "--porcelain=v2"]),
            vec!["git", "-C", "/repo", "status", "--porcelain=v2"]
        ); // portability: allow
    }

    #[test]
    fn argv_remote_without_control_dir_omits_control_options() {
        let git = Git {
            location: Location::Remote { host: "gpu-box".into(), path: "~/g".into() },
            git_bin: "git".into(),
            ssh_bin: "ssh".into(),
            control_dir: None,
        };
        assert_eq!(
            git.argv(&["status"]),
            vec![
                "ssh",
                "gpu-box",
                "--",
                "env GIT_TERMINAL_PROMPT=0 LC_ALL=C GIT_OPTIONAL_LOCKS=0 git -C ~/g status"
            ]
        );
    }

    #[test]
    fn argv_remote_with_control_dir_uses_exec_args() {
        let git = Git {
            location: Location::Remote { host: "gpu-box".into(), path: "/srv/app".into() }, // portability: allow
            git_bin: "git".into(),
            ssh_bin: "ssh".into(),
            control_dir: Some(PathBuf::from("/run/ssh")), // portability: allow
        };
        let argv = git.argv(&["log", "-1"]);
        assert_eq!(argv[0], "ssh");
        assert_eq!(argv[1], "-o");
        assert!(argv[2].starts_with("ControlPath="), "{argv:?}");
        assert_eq!(argv[3], "-o");
        assert_eq!(argv[4], "ControlMaster=no");
        assert_eq!(argv[5], "gpu-box");
        assert_eq!(argv[6], "--");
        assert_eq!(argv[7], "env GIT_TERMINAL_PROMPT=0 LC_ALL=C GIT_OPTIONAL_LOCKS=0 git -C /srv/app log -1"); // portability: allow
        assert_eq!(argv.len(), 8);
    }

    /// A remote host for [`Git::argv`]'s `<ssh_bin> <host> -- <command>` with `ssh_bin = "sh"`
    /// and the returned script as `<host>`: it runs `<command>` in a fresh environment, as sshd
    /// does (it forwards none of our variables by default). `sh` only reads the script, so it is
    /// never exec'd right after being written (ETXTBSY while other test threads fork).
    fn fake_host(dir: &Path) -> String {
        let path = dir.join("fake-host.sh");
        let script = format!(
            "while [ \"$1\" != -- ]; do shift; done\n\
             exec env -i PATH=\"$PATH\" HOME={} GIT_CONFIG_NOSYSTEM=1 sh -c \"$2\"\n",
            ssh::quote(&dir.display().to_string())
        );
        std::fs::write(&path, script).expect("write fake host");
        path.display().to_string()
    }

    /// The environment the module doc promises must reach the *remote* git, not just the local
    /// ssh process. Observed through its effect: without `GIT_OPTIONAL_LOCKS=0`, `status` on a
    /// stat-dirty index takes index.lock and rewrites the index, colliding with agents' git.
    #[test]
    fn remote_git_runs_with_the_same_environment_as_local_git() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "hello\n", "first");
        let file = std::fs::File::options().write(true).open(repo.path().join("a.txt")).expect("open");
        let an_hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        file.set_modified(an_hour_ago).expect("touch a.txt: same content, stale index stat");
        let index = repo.path().join(".git").join("index");
        let before = std::fs::read(&index).expect("read index");

        let host_dir = tempfile::tempdir().expect("tempdir");
        let git = Git {
            location: Location::Remote {
                host: fake_host(host_dir.path()),
                path: repo.path().display().to_string(),
            },
            git_bin: "git".into(),
            ssh_bin: "sh".into(),
            control_dir: None,
        };
        let out = git.run(&["status", "--porcelain=v2"]).expect("remote status");
        assert!(out.is_empty(), "clean tree: {:?}", String::from_utf8_lossy(&out));
        assert!(std::fs::read(&index).expect("read index") == before, "remote status rewrote the index");
    }

    // --- Git::command ----------------------------------------------------------------------

    #[test]
    fn command_sets_expected_env() {
        let git = Git::new(Location::Local { path: PathBuf::from("/repo") }); // portability: allow
        let cmd = git.command(&["status"]);
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned())))
            .collect();
        assert!(envs.contains(&("GIT_TERMINAL_PROMPT".to_string(), Some("0".to_string()))));
        assert!(envs.contains(&("LC_ALL".to_string(), Some("C".to_string()))));
        assert!(envs.contains(&("GIT_OPTIONAL_LOCKS".to_string(), Some("0".to_string()))));
    }

    // --- Git::run against a real repo -------------------------------------------------------

    #[test]
    fn run_rev_parse_matches_committed_hash() {
        let mut repo = TempRepo::new();
        let hash = repo.commit_file("a.txt", "hello\n", "first");
        let git = Git::new(Location::Local { path: repo.path().to_path_buf() });

        let out = git.run(&["rev-parse", "HEAD"]).expect("rev-parse should succeed");
        assert_eq!(String::from_utf8(out).expect("utf8").trim(), hash);
    }

    #[test]
    fn run_status_on_clean_repo_is_empty() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "hello\n", "first");
        let git = Git::new(Location::Local { path: repo.path().to_path_buf() });

        let out = git.run(&["status", "--porcelain=v2"]).expect("status should succeed");
        assert!(out.is_empty(), "clean tree should report no changes: {:?}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn run_log_lists_commits_oldest_last() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n", "first");
        let second = repo.commit_file("a.txt", "2\n", "second");
        let git = Git::new(Location::Local { path: repo.path().to_path_buf() });

        let out = git.run(&["log", "--format=%H"]).expect("log should succeed");
        let first_line =
            String::from_utf8(out).expect("utf8").lines().next().expect("at least one line").to_string();
        assert_eq!(first_line, second, "log lists newest commit first");
    }

    #[test]
    fn run_against_non_repo_fails_with_128_and_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = Git::new(Location::Local { path: dir.path().to_path_buf() });

        let err = git.run(&["status"]).expect_err("not a git repository");
        assert_eq!(err.code, Some(128));
        assert!(err.stderr.contains("not a git repository"), "stderr: {}", err.stderr);
    }

    #[test]
    fn run_spawn_failure_has_no_code_and_io_error_in_stderr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = Git::new(Location::Local { path: dir.path().to_path_buf() });
        let mut git = git;
        git.git_bin = "amalgum-git-binary-that-does-not-exist".to_string();

        let err = git.run(&["status"]).expect_err("spawn should fail");
        assert_eq!(err.code, None);
        assert!(!err.stderr.is_empty(), "spawn failure stderr should carry the io error text");
    }

    #[test]
    fn run_with_stdin_hash_object_matches_oracle() {
        let repo = TempRepo::new();
        let git = Git::new(Location::Local { path: repo.path().to_path_buf() });
        let content = b"hello amalgum\n";

        let out =
            git.run_with_stdin(&["hash-object", "--stdin"], content).expect("hash-object should succeed");
        let got = String::from_utf8(out).expect("utf8").trim().to_string();

        // Oracle: hash the same bytes with a directly hermetic git invocation.
        let mut child = hermetic_git(repo.path())
            .args(["hash-object", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn git");
        child.stdin.take().expect("stdin").write_all(content).expect("write stdin");
        let oracle = child.wait_with_output().expect("wait for git");
        let want = String::from_utf8_lossy(&oracle.stdout).trim().to_string();

        assert_eq!(got, want);
        assert_eq!(got.len(), 40, "sha1 hex digest");
    }

    // --- GitError::summary ------------------------------------------------------------------

    #[test]
    fn summary_known_cases() {
        let cases: &[(&str, &str)] = &[
            (
                "git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.\n",
                "Authentication failed. Check your SSH agent or credential helper.",
            ),
            (
                "remote: Support for password authentication was removed.\nfatal: Authentication failed for 'https://github.com/x/y.git/'\n",
                "Authentication failed. Check your SSH agent or credential helper.",
            ),
            (
                "fatal: could not read Username for 'https://github.com': terminal prompts disabled\n",
                "Authentication failed. Check your SSH agent or credential helper.",
            ),
            (
                "To github.com:user/repo.git\n ! [rejected]        main -> main (non-fast-forward)\nerror: failed to push some refs to 'github.com:user/repo.git'\nhint: Updates were rejected because the tip of your current branch is behind\n",
                "Push rejected: remote has commits you don't have",
            ),
            (
                "fatal: not a git repository (or any of the parent directories): .git\n",
                "Not a git repository",
            ),
            (
                "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\nHost key verification failed.\nfatal: Could not read from remote repository.\n",
                "Unknown or changed host key",
            ),
            (
                "ssh: Could not resolve hostname gpu-box: Name or service not known\nfatal: Could not read from remote repository.\n",
                "Offline",
            ),
            ("ssh: connect to host gpu-box port 22: Connection refused\n", "Offline"),
            ("ssh: connect to host gpu-box port 22: Connection timed out\n", "Offline"),
            (
                "fatal: unable to access 'https://github.com/x/y.git/': Could not resolve host: github.com\n",
                "Offline",
            ),
            (
                "error: Your local changes to the following files would be overwritten by checkout:\n\tsrc/main.rs\n\tCargo.toml\nPlease commit your changes or stash them before you switch branches.\nAborting\n",
                "Checkout would overwrite 2 files",
            ),
            (
                "error: Your local changes to the following files would be overwritten by merge:\n\tREADME.md\nPlease commit your changes or stash them before you merge.\n",
                "Checkout would overwrite 1 files",
            ),
        ];
        for (stderr, want) in cases {
            let err = GitError { command: "git x".into(), code: Some(1), stderr: (*stderr).to_string() };
            assert_eq!(err.summary(), *want, "stderr: {stderr:?}");
        }
    }

    #[test]
    fn summary_falls_back_to_last_non_empty_stderr_line() {
        let err = GitError {
            command: "git x".into(),
            code: Some(1),
            stderr: "warning: something\nerror: final failure line\n\n".to_string(),
        };
        assert_eq!(err.summary(), "error: final failure line");
    }

    #[test]
    fn summary_empty_stderr_shows_exit_code() {
        let err = GitError { command: "git x".into(), code: Some(128), stderr: String::new() };
        assert_eq!(err.summary(), "git exited with code 128");
    }

    #[test]
    fn display_uses_summary() {
        let err = GitError {
            command: "git x".into(),
            code: Some(128),
            stderr: "fatal: not a git repository\n".into(),
        };
        assert_eq!(err.to_string(), "Not a git repository");
    }

    #[test]
    fn command_never_inherits_a_hooks_repository() {
        // Inside a git hook, GIT_DIR/GIT_INDEX_FILE point at the hook's repo; they must not leak.
        let cmd = Git::new(Location::Local { path: PathBuf::from("repo") }).command(&["status"]);
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY"] {
            assert!(removed.iter().any(|r| r == var), "{var} not cleared: {removed:?}");
        }
    }

    // --- Git::run_streaming -----------------------------------------------------------------

    #[test]
    fn with_progress_adds_the_flag_to_network_commands_once() {
        assert_eq!(with_progress(&["fetch", "origin"]), vec!["fetch", "--progress", "origin"]);
        assert_eq!(with_progress(&["push", "--progress", "o"]), vec!["push", "--progress", "o"]);
        assert_eq!(with_progress(&["status"]), vec!["status"]);
        assert_eq!(with_progress(&[]), Vec::<&str>::new());
    }

    fn segments(input: &[u8]) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        split_segments(input, |s, line| {
            out.push((s, line));
            true
        });
        out
    }

    #[test]
    fn split_segments_separates_redraws_from_lines() {
        let got = segments(b"Receiving:  1%\rReceiving: 50%\rReceiving: 100%, done.\r\nnext\n\nlast");
        let want = [("Receiving:  1%", false), ("Receiving: 50%", false), ("Receiving: 100%, done.", true)];
        let mut want: Vec<(String, bool)> = want.iter().map(|(s, l)| (s.to_string(), *l)).collect();
        want.extend([("next".to_string(), true), ("last".to_string(), true)]);
        assert_eq!(got, want);
        assert_eq!(segments(b"tail\r"), vec![("tail".to_string(), false)]);
        assert_eq!(segments(b""), vec![]);
    }

    /// A repo with one commit and a bare clone of it; returns (repo, bare path).
    fn repo_with_bare_remote() -> (TempRepo, tempfile::TempDir) {
        let mut repo = TempRepo::new();
        for i in 0..20 {
            repo.commit_file(&format!("f{i}.txt"), &"x\n".repeat(i + 1), &format!("c{i}"));
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let src = repo.path().display().to_string();
        repo.git(&["clone", "-q", "--bare", &src, &dir.path().join("r.git").display().to_string()]);
        (repo, dir)
    }

    #[test]
    fn run_streaming_reports_progress_and_succeeds() {
        let (_repo, dir) = repo_with_bare_remote();
        let url = format!("file://{}", dir.path().join("r.git").display());
        let git = Git::new(Location::Local { path: dir.path().to_path_buf() });
        let mut seen = Vec::new();
        let cancel = AtomicBool::new(false);
        git.run_streaming(&["clone", &url, "w"], &cancel, &mut |s| seen.push(s.to_string()))
            .expect("clone succeeds");
        assert!(dir.path().join("w").join("f0.txt").exists());
        assert!(seen.iter().any(|s| s.starts_with("Cloning into")), "{seen:?}");
        assert!(seen.iter().any(|s| s.starts_with("Receiving objects:")), "--progress was passed: {seen:?}");
    }

    #[test]
    fn run_streaming_failure_keeps_stderr_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = Git::new(Location::Local { path: dir.path().to_path_buf() });
        let missing = dir.path().join("missing.git").display().to_string();
        let err = git
            .run_streaming(&["clone", &missing, "w"], &AtomicBool::new(false), &mut |_| {})
            .expect_err("no such repository");
        assert_eq!(err.code, Some(128));
        assert!(err.stderr.contains("does not exist"), "{}", err.stderr);
        assert!(err.command.contains("--progress"), "{}", err.command);
    }

    #[test]
    fn run_streaming_with_cancel_already_set_spawns_nothing() {
        let (_repo, dir) = repo_with_bare_remote();
        let git = Git::new(Location::Local { path: dir.path().to_path_buf() });
        let err = git
            .run_streaming(&["clone", "r.git", "w"], &AtomicBool::new(true), &mut |_| panic!("no output"))
            .expect_err("cancelled");
        assert_eq!(err.code, None);
        assert!(err.stderr.ends_with(&format!("{CANCELLED}\n")), "{}", err.stderr);
        assert!(!dir.path().join("w").exists());
    }

    /// A cancel that arrives after git already finished reports git's result, not a cancel: the
    /// operation happened (a push landed) and the caller must journal it.
    #[test]
    fn run_streaming_cancel_after_git_exited_reports_success() {
        let host_dir = tempfile::tempdir().expect("tempdir");
        let host = host_dir.path().join("quick-host.sh");
        std::fs::write(&host, "echo 'Writing objects: 100% (1/1), done.' >&2\necho pushed\n")
            .expect("write host");
        let git = Git {
            location: Location::Remote { host: host.display().to_string(), path: "r".into() },
            git_bin: "git".into(),
            ssh_bin: "sh".into(),
            control_dir: None,
        };
        let cancel = AtomicBool::new(false);
        let out = git
            .run_streaming(&["push", "origin"], &cancel, &mut |_| {
                cancel.store(true, Ordering::Relaxed);
                // Let the host exit before the runner looks at the flag.
                std::thread::sleep(Duration::from_millis(300));
            })
            .expect("git finished before the cancel");
        assert_eq!(out, b"pushed\n");
    }

    /// Cancelling kills the process instead of waiting for it: here a "remote" that would take
    /// 30 s, through the fake-ssh `sh` host.
    #[test]
    fn run_streaming_cancel_kills_a_running_command() {
        let host_dir = tempfile::tempdir().expect("tempdir");
        let host = host_dir.path().join("slow-host.sh");
        std::fs::write(&host, "echo 'Receiving objects:   1% (1/100)' >&2\nexec sleep 30\n")
            .expect("write host");
        let git = Git {
            location: Location::Remote { host: host.display().to_string(), path: "r".into() },
            git_bin: "git".into(),
            ssh_bin: "sh".into(),
            control_dir: None,
        };
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let started = std::time::Instant::now();
        let mut seen = Vec::new();
        let err = git
            .run_streaming(&["fetch", "origin"], &cancel, &mut |s| {
                seen.push(s.to_string());
                cancel.store(true, Ordering::Relaxed);
            })
            .expect_err("cancelled");
        assert!(started.elapsed() < Duration::from_secs(10), "waited for the child: {:?}", started.elapsed());
        assert_eq!(seen, vec!["Receiving objects:   1% (1/100)"]);
        assert!(err.stderr.ends_with(&format!("{CANCELLED}\n")), "{}", err.stderr);
    }

    #[test]
    fn run_streaming_works_for_a_remote_location() {
        let (_repo, dir) = repo_with_bare_remote();
        let host_dir = tempfile::tempdir().expect("tempdir");
        let git = Git {
            location: Location::Remote {
                host: fake_host(host_dir.path()),
                path: dir.path().display().to_string(),
            },
            git_bin: "git".into(),
            ssh_bin: "sh".into(),
            control_dir: None,
        };
        let mut seen = Vec::new();
        git.run_streaming(&["clone", "r.git", "w"], &AtomicBool::new(false), &mut |s| {
            seen.push(s.to_string())
        })
        .expect("remote clone");
        assert!(dir.path().join("w").join("f0.txt").exists());
        assert!(seen.iter().any(|s| s.starts_with("Cloning into")), "{seen:?}");
    }
}
