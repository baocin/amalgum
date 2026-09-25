//! Undo/redo journal (§5.18). Every ref-changing operation records the repo state before and
//! after, the forward command plan (for redo), and the inverse plan (for undo; `None` for push,
//! shown greyed as "Cannot be undone from the client"). Stored per repo as JSON, capped at
//! [`CAP`] entries (oldest dropped), surviving restarts.
//!
//! Guard: undo runs only if the refs the entry touched still equal its `after` snapshot (and
//! redo only if they equal `before`); otherwise the repo moved underneath us (a terminal or an
//! agent ran git) and the caller shows "Can't undo: `main` has moved since. Open reflog?".
//! Recording a new operation discards any undone entries (the redo tail).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

pub const CAP: usize = 200;

/// A git argv (without the leading `git`), e.g. `["reset", "--soft", "HEAD~1"]`.
pub type Plan = Vec<Vec<String>>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub head: Option<String>,
    /// Current branch, `None` when detached.
    pub branch: Option<String>,
    /// Full ref name → hash, only for refs the operation touched.
    pub refs: BTreeMap<String, String>,
    pub stashes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub time: u64,
    /// "commit ab12cd3", used as "Undo: commit ab12cd3".
    pub description: String,
    pub before: Snapshot,
    pub after: Snapshot,
    pub forward: Plan,
    pub inverse: Option<Plan>,
    pub undone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    NothingToDo,
    /// The entry cannot be undone (push).
    Irreversible(String),
    /// Named ref (or `HEAD`) no longer matches the journal.
    Moved(String),
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Journal {
    entries: Vec<Entry>,
    next_id: u64,
}

impl Journal {
    /// Record an operation (assigns `id`, clears `undone`). Returns the id.
    pub fn record(&mut self, entry: Entry) -> u64 {
        todo!()
    }
    /// The entry `Mod+Z` would undo (newest not undone), for "Undo: …" labels.
    pub fn peek_undo(&self) -> Option<&Entry> {
        todo!()
    }
    pub fn peek_redo(&self) -> Option<&Entry> {
        todo!()
    }
    /// Check the guard against `current` and return the inverse plan; marks it undone.
    pub fn undo(&mut self, current: &Snapshot) -> Result<Plan, Refused> {
        todo!()
    }
    pub fn redo(&mut self, current: &Snapshot) -> Result<Plan, Refused> {
        todo!()
    }
    /// Newest first, for the Undo panel.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().rev()
    }
    pub fn load(path: &Path) -> io::Result<Self> {
        todo!()
    }
    /// Atomic write (temp file + rename).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        todo!()
    }
}
