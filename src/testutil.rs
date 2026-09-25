//! Hermetic git fixtures for tests. Every repo lives in a temp dir, ignores the user's global
//! and system git config, and uses fixed identities and dates so hashes are reproducible.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct TempRepo {
    dir: tempfile::TempDir,
    clock: u64,
}

impl TempRepo {
    /// `git init` on branch `main` with a fixed identity.
    pub fn new() -> Self {
        let repo = Self { dir: tempfile::tempdir().expect("tempdir"), clock: 1_700_000_000 };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Run git in the repo; panics with stderr on failure. Returns trimmed stdout.
    pub fn git(&self, args: &[&str]) -> String {
        let out = hermetic_git(self.path()).args(args).output().expect("spawn git");
        assert!(out.status.success(), "git {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    /// Run git and return raw stdout bytes (for `-z` parsers). Panics on failure.
    pub fn git_raw(&self, args: &[&str]) -> Vec<u8> {
        let out = hermetic_git(self.path()).args(args).output().expect("spawn git");
        assert!(out.status.success(), "git {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }

    pub fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&p, contents).expect("write");
        p
    }

    /// Write `rel`, stage everything, and commit with a deterministic timestamp. Returns the hash.
    pub fn commit_file(&mut self, rel: &str, contents: &str, message: &str) -> String {
        self.write(rel, contents);
        self.git(&["add", "-A"]);
        self.commit(message)
    }

    /// Commit whatever is staged (allowing empty). Returns the new HEAD hash.
    pub fn commit(&mut self, message: &str) -> String {
        self.clock += 60;
        let date = format!("@{} +0000", self.clock);
        let out = hermetic_git(self.path())
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .args(["commit", "-q", "--allow-empty", "-m", message])
            .output()
            .expect("spawn git");
        assert!(out.status.success(), "commit failed:\n{}", String::from_utf8_lossy(&out.stderr));
        self.git(&["rev-parse", "HEAD"])
    }
}

/// A `git` command isolated from the machine's configuration — and from any repository an
/// enclosing git hook points at (the pre-commit gate runs these tests with GIT_DIR set).
pub fn hermetic_git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    for var in crate::git::cmd::REPO_ENV_VARS {
        c.env_remove(var);
    }
    c.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada Tester")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada Tester")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false", "-c", "core.autocrlf=false"]);
    c
}
