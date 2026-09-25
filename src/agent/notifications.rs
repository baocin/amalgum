//! Notification panel store (§5.29, W14): newest first, capped at [`CAP`], unread counts,
//! per-workspace mute, persisted as JSON lines. Loading tolerates corrupt lines (skips them).

use super::AgentKind;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::io;
use std::path::Path;

pub const CAP: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Level {
    Info,
    Success,
    Error,
    NeedsInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub id: u64,
    pub time: u64,
    pub level: Level,
    pub title: String,
    pub body: String,
    pub workspace: Option<String>,
    pub tab: Option<String>,
    pub agent: Option<AgentKind>,
    /// Commit an agent made during this turn (§5.29 "Agent commit linking").
    pub commit: Option<String>,
    pub read: bool,
}

#[derive(Debug, Default)]
pub struct Store {
    items: VecDeque<Notification>,
    next_id: u64,
    muted: BTreeSet<String>,
}

impl Store {
    /// Add a notification (its `id` and `read` are assigned here). Returns the id, or `None`
    /// if its workspace is muted. Drops the oldest beyond [`CAP`].
    pub fn push(&mut self, n: Notification) -> Option<u64> {
        todo!()
    }
    /// Newest first.
    pub fn iter(&self) -> impl Iterator<Item = &Notification> {
        self.items.iter()
    }
    pub fn unread(&self) -> usize {
        todo!()
    }
    /// Focusing a tab marks its rows read.
    pub fn mark_tab_read(&mut self, tab: &str) {
        todo!()
    }
    pub fn mark_all_read(&mut self) {
        todo!()
    }
    pub fn clear(&mut self) {
        todo!()
    }
    pub fn set_muted(&mut self, workspace: &str, muted: bool) {
        todo!()
    }
    pub fn load(path: &Path) -> io::Result<Self> {
        todo!()
    }
    /// Atomic write (temp file + rename).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        todo!()
    }
}
