//! Persistent app state (§5.22 "Per-workspace state"): repo › remote › workspace › tab › pane,
//! recent locations, and the active workspace, saved to `state.json`. Sidebar order is vector
//! order. Closing the last workspace of a repo keeps the repo group until `forget_repo`.

use crate::agent::AgentKind;
use crate::agent::status::Status;
use crate::git::Location;
use super::layout::{PaneId, Tree};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

pub const RECENTS_CAP: usize = 20;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitView {
    #[default]
    Graph,
    Changes,
    Refs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    pub agent: AgentKind,
    pub session_id: Option<String>,
    pub pid: Option<u32>,
    pub cwd: Option<String>,
    /// Sanitised argv (no env assignments), for resume flags (§5.30).
    pub argv: Vec<String>,
    pub status: Status,
    pub hibernated: bool,
    pub updated: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub id: PaneId,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub agent: Option<AgentSession>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: String,
    /// User-set name (double-click rename); otherwise the UI shows OSC title / process name.
    pub name: Option<String>,
    pub layout: Tree,
    pub panes: Vec<Pane>,
    pub focused: PaneId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub location: Location,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub git_view: GitView,
    pub git_pane_open: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteGroup {
    /// `origin`; empty for a repo with no remotes.
    pub name: String,
    pub url: Option<String>,
    pub collapsed: bool,
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repo {
    /// `git::remote::repo_id`.
    pub id: String,
    pub name: String,
    pub collapsed: bool,
    pub remotes: Vec<RemoteGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    pub location: Location,
    /// `conduit / origin`.
    pub label: String,
    pub opened: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppState {
    pub repos: Vec<Repo>,
    pub recents: Vec<Recent>,
    pub active: Option<String>,
    next_id: u64,
}

impl AppState {
    /// Missing file → default. Unparseable → `Err` (the caller backs it up, never overwrites).
    pub fn load(path: &Path) -> io::Result<Self> {
        todo!()
    }
    /// Atomic write (temp file + rename).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        todo!()
    }
    /// Fresh id with a prefix (`"w"` → `"w7"`), unique within this state forever.
    pub fn new_id(&mut self, prefix: &str) -> String {
        todo!()
    }
    /// Insert `ws` under repo `repo_id` / remote `remote`, creating either group if new.
    pub fn add_workspace(&mut self, repo_id: &str, repo_name: &str, remote: &str, url: Option<&str>, ws: Workspace) {
        todo!()
    }
    pub fn remove_workspace(&mut self, id: &str) -> Option<Workspace> {
        todo!()
    }
    pub fn forget_repo(&mut self, repo_id: &str) {
        todo!()
    }
    pub fn workspace(&self, id: &str) -> Option<&Workspace> {
        todo!()
    }
    pub fn workspace_mut(&mut self, id: &str) -> Option<&mut Workspace> {
        todo!()
    }
    /// All workspaces in sidebar order (for `Mod+1..9` and `Mod+Alt+↑/↓`).
    pub fn workspaces(&self) -> Vec<&Workspace> {
        todo!()
    }
    /// Move a workspace up (`-1`) or down (`+1`) within its remote group.
    pub fn move_workspace(&mut self, id: &str, delta: i32) -> bool {
        todo!()
    }
    /// Record an opened location: dedupe by location, newest first, cap [`RECENTS_CAP`].
    pub fn touch_recent(&mut self, location: Location, label: &str, now: u64) {
        todo!()
    }
}
