//! Where every file lives (§1: "No absolute paths. Every path comes from `directories` or git
//! config"). Set `AMALGUM_HOME` to relocate everything under one directory — tests, dev runs,
//! and agents use it so they never touch the real user's state.

use std::path::{Path, PathBuf};

/// Directory, relative to a remote host's `$HOME`, holding everything Amalgum puts there (§9).
pub const REMOTE_ROOT: &str = ".amalgum";

/// The app's local directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// `settings.toml`, per-repo overrides, journal.
    pub config: PathBuf,
    /// `state.json`, notifications, scrollback, logs.
    pub data: PathBuf,
    /// Commit-metadata cache and search index.
    pub cache: PathBuf,
    /// Control socket and ssh ControlMaster sockets.
    pub runtime: PathBuf,
}

impl Dirs {
    /// Resolve from `AMALGUM_HOME` if set, else from the OS conventions
    /// (`~/Library/Application Support/amalgum`, `$XDG_CONFIG_HOME/amalgum`, …).
    pub fn discover() -> Option<Self> {
        if let Some(root) = std::env::var_os("AMALGUM_HOME").filter(|v| !v.is_empty()) {
            return Some(Self::under(Path::new(&root)));
        }
        let p = directories::ProjectDirs::from("", "", "amalgum")?;
        let runtime = p.runtime_dir().map_or_else(|| p.data_dir().join("run"), |r| r.to_path_buf());
        Some(Self {
            config: p.config_dir().to_path_buf(),
            data: p.data_dir().to_path_buf(),
            cache: p.cache_dir().to_path_buf(),
            runtime,
        })
    }

    /// Everything under one root. Used by `AMALGUM_HOME` and tests.
    pub fn under(root: &Path) -> Self {
        Self {
            config: root.join("config"),
            data: root.join("data"),
            cache: root.join("cache"),
            runtime: root.join("run"),
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime.join("app.sock")
    }
    pub fn ssh_control_dir(&self) -> PathBuf {
        self.runtime.join("ssh")
    }
    pub fn settings(&self) -> PathBuf {
        self.config.join("settings.toml")
    }
    pub fn repo_settings(&self, repo_id: &str) -> PathBuf {
        self.config.join("repos").join(format!("{repo_id}.toml"))
    }
    pub fn journal(&self, repo_id: &str) -> PathBuf {
        self.config.join("journal").join(format!("{repo_id}.json"))
    }
    pub fn state(&self) -> PathBuf {
        self.data.join("state.json")
    }
    pub fn notifications(&self) -> PathBuf {
        self.data.join("notifications.jsonl")
    }
    pub fn scrollback(&self, tab_id: &str) -> PathBuf {
        self.data.join("scrollback").join(format!("{tab_id}.txt"))
    }
    pub fn logs(&self) -> PathBuf {
        self.data.join("logs")
    }
    pub fn commit_cache(&self, repo_id: &str) -> PathBuf {
        self.cache.join(repo_id).join("commits.bin")
    }
}

/// The user's home directory (`$HOME`), used for agent config files and the remote root.
pub fn home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// `~/.amalgum` on this machine; on a remote host this is where the CLI, sockets, and the
/// offline queue live (§9 "Remote footprint").
pub fn remote_root(home: &Path) -> PathBuf {
    home.join(REMOTE_ROOT)
}

/// Offline hook-event queue drained by `amalgum drain` (§5.28).
pub fn queue_file(home: &Path) -> PathBuf {
    remote_root(home).join("queue.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_root_layout() {
        let d = Dirs::under(Path::new("r"));
        assert_eq!(d.socket(), Path::new("r/run/app.sock"));
        assert_eq!(d.journal("abc"), Path::new("r/config/journal/abc.json"));
        assert_eq!(d.state(), Path::new("r/data/state.json"));
        assert_eq!(queue_file(Path::new("h")), Path::new("h/.amalgum/queue.jsonl"));
    }
}
