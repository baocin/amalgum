//! The one place the ssh engine spawns processes. [`steps`](super::steps) and the
//! [`manager`](super::manager) only ever see a [`Runner`], so their logic is unit-tested with a
//! scripted fake and the real [`SystemRunner`] is exercised by `tests/ssh.rs` against a private
//! sshd.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};

use crate::git::GitError;

/// What a finished process left behind. `code` is `None` when it died from a signal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// The failure as a [`GitError`] carrying the command line and ssh's stderr verbatim.
    pub fn error(&self, argv: &[String]) -> GitError {
        GitError { command: command_line(argv), code: self.code, stderr: self.stderr_str() }
    }
}

/// Runs one argv (`argv[0]` is the program) to completion, optionally feeding `stdin`.
pub trait Runner: Send + Sync {
    fn run(&self, argv: &[String], stdin: Option<&[u8]>) -> io::Result<Output>;
}

/// Run and turn a non-zero exit or a spawn failure into a [`GitError`]; stdout on success.
pub fn run_ok(runner: &dyn Runner, argv: &[String], stdin: Option<&[u8]>) -> Result<Output, GitError> {
    match runner.run(argv, stdin) {
        Ok(out) if out.success() => Ok(out),
        Ok(out) => Err(out.error(argv)),
        Err(e) => Err(GitError { command: command_line(argv), code: None, stderr: e.to_string() }),
    }
}

/// The argv as one shell-quoted line, for error details and logs.
pub fn command_line(argv: &[String]) -> String {
    argv.iter().map(|a| super::quote_literal(a)).collect::<Vec<_>>().join(" ")
}

/// Spawns real processes. Each runs in its own session, detached from any controlling terminal,
/// so ssh never stops to ask on `/dev/tty` (a host-key question or a passphrase): it fails fast
/// with a message the steps can classify, or uses `SSH_ASKPASS` if the user configured one.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, argv: &[String], stdin: Option<&[u8]>) -> io::Result<Output> {
        let (program, args) =
            argv.split_first().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: runs between fork and exec and only calls setsid(2), which is
        // async-signal-safe and touches no memory.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
                if libc::setsid() == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
            });
        }
        let mut child = cmd.spawn()?;
        let writer = match (stdin, child.stdin.take()) {
            (Some(data), Some(mut pipe)) => {
                let data = data.to_vec();
                // A separate thread, so a large input cannot deadlock against full output pipes.
                Some(std::thread::spawn(move || pipe.write_all(&data)))
            }
            _ => None,
        };
        let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
        let stderr_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf);
            buf
        });
        let mut stdout = Vec::new();
        child.stdout.take().expect("stdout is piped").read_to_end(&mut stdout)?;
        let status = child.wait()?;
        let stderr = stderr_reader.join().unwrap_or_default();
        if let Some(writer) = writer {
            // A remote command that exits without reading its input is not our failure.
            let _ = writer.join();
        }
        Ok(Output { code: status.code(), stdout, stderr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn system_runner_captures_output_code_and_stdin() {
        let out = SystemRunner
            .run(&argv(&["sh", "-c", "cat; echo err >&2; exit 3"]), Some(b"hello"))
            .expect("spawn sh");
        assert_eq!(out, Output { code: Some(3), stdout: b"hello".to_vec(), stderr: b"err\n".to_vec() });
    }

    #[test]
    fn system_runner_detaches_from_the_terminal() {
        let out =
            SystemRunner.run(&argv(&["sh", "-c", "exec 3</dev/tty && echo has-tty"]), None).expect("spawn");
        assert!(!out.success(), "a child must not reach a controlling terminal: {out:?}");
    }

    #[test]
    fn run_ok_keeps_stderr_and_the_quoted_command() {
        let err = run_ok(&SystemRunner, &argv(&["sh", "-c", "echo boom >&2; exit 2"]), None).unwrap_err();
        assert_eq!(err.code, Some(2));
        assert_eq!(err.stderr, "boom\n");
        assert_eq!(err.command, "sh -c 'echo boom >&2; exit 2'");

        let missing = run_ok(&SystemRunner, &argv(&["amalgum-no-such-program"]), None).unwrap_err();
        assert_eq!(missing.code, None);
        assert!(!missing.stderr.is_empty(), "spawn errors are reported, not swallowed");
    }
}
