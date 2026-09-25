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
    ///
    /// Recording discards the redo tail: any entries already undone are dropped first, since a
    /// fresh operation invalidates them (their `before`/`after` snapshots no longer describe a
    /// reachable state once new history has been written). The oldest entries beyond [`CAP`] are
    /// then dropped so the journal never grows without bound.
    pub fn record(&mut self, mut entry: Entry) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        entry.id = id;
        entry.undone = false;
        self.entries.retain(|e| !e.undone);
        self.entries.push(entry);
        if self.entries.len() > CAP {
            let excess = self.entries.len() - CAP;
            self.entries.drain(..excess);
        }
        id
    }
    /// The entry `Mod+Z` would undo (newest not undone), for "Undo: …" labels.
    pub fn peek_undo(&self) -> Option<&Entry> {
        self.entries.iter().rev().find(|e| !e.undone)
    }
    /// The entry `Mod+Shift+Z` would redo: the oldest of the contiguous run of undone entries at
    /// the tail, i.e. the one most recently undone (undo always undoes newest-first, so that run
    /// grows leftward and its earliest member is always the last one undone).
    pub fn peek_redo(&self) -> Option<&Entry> {
        self.entries.get(self.redo_run_start()?)
    }
    /// Index of the first entry in the trailing run of undone entries, or `None` if the journal
    /// is empty or its newest entry is not undone.
    fn redo_run_start(&self) -> Option<usize> {
        let last = self.entries.len().checked_sub(1)?;
        if !self.entries[last].undone {
            return None;
        }
        let mut start = last;
        while start > 0 && self.entries[start - 1].undone {
            start -= 1;
        }
        Some(start)
    }
    /// Check the guard against `current` and return the inverse plan; marks it undone.
    ///
    /// The spec (§5.18) says undo runs "if `before` still matches", but that reads as shorthand
    /// for "the repo is still in the state the operation produced": undo replays the *inverse* of
    /// an operation that already ran, so it is only safe when the repo is still sitting in that
    /// operation's `after` state (nothing else touched the refs since). Comparing against
    /// `before` would let undo fire even after unrelated commits landed on top, silently
    /// discarding them. Redo is symmetrical against `before`, since it is only safe to replay the
    /// forward command from the exact state that preceded it.
    pub fn undo(&mut self, current: &Snapshot) -> Result<Plan, Refused> {
        let idx = self.entries.iter().rposition(|e| !e.undone).ok_or(Refused::NothingToDo)?;
        let entry = &self.entries[idx];
        let Some(inverse) = &entry.inverse else {
            return Err(Refused::Irreversible(entry.description.clone()));
        };
        if let Some(name) = mismatch(&entry.after, current) {
            return Err(Refused::Moved(name));
        }
        let inverse = inverse.clone();
        self.entries[idx].undone = true;
        Ok(inverse)
    }
    pub fn redo(&mut self, current: &Snapshot) -> Result<Plan, Refused> {
        let idx = self.redo_run_start().ok_or(Refused::NothingToDo)?;
        let entry = &self.entries[idx];
        if let Some(name) = mismatch(&entry.before, current) {
            return Err(Refused::Moved(name));
        }
        let forward = entry.forward.clone();
        self.entries[idx].undone = false;
        Ok(forward)
    }
    /// Newest first, for the Undo panel.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().rev()
    }
    /// Missing file loads as an empty journal.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }
    /// Atomic write (temp file in the same directory + rename), creating parent directories.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let mut tmp_name = path.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp_path = std::path::PathBuf::from(tmp_name);
        std::fs::write(&tmp_path, json)?;
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }
}

/// The first ref name in `snapshot.refs` whose value `current` disagrees with (a ref missing
/// from `current` counts as a mismatch), else `"HEAD"` if `snapshot.head` is set and disagrees,
/// else `None`.
fn mismatch(snapshot: &Snapshot, current: &Snapshot) -> Option<String> {
    for (name, hash) in &snapshot.refs {
        if current.refs.get(name) != Some(hash) {
            return Some(name.clone());
        }
    }
    if let Some(head) = &snapshot.head
        && current.head.as_ref() != Some(head)
    {
        return Some("HEAD".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot with `HEAD` and `refs/heads/main` both at `hash`.
    fn snap(hash: &str) -> Snapshot {
        Snapshot {
            head: Some(hash.to_string()),
            branch: Some("main".to_string()),
            refs: BTreeMap::from([("refs/heads/main".to_string(), hash.to_string())]),
            stashes: vec![],
        }
    }

    /// A reversible entry moving `refs/heads/main` (and HEAD) from `before` to `after`.
    fn entry(description: &str, before: &str, after: &str) -> Entry {
        Entry {
            id: 0,
            time: 0,
            description: description.to_string(),
            before: snap(before),
            after: snap(after),
            forward: vec![vec!["commit".to_string()]],
            inverse: Some(vec![vec!["reset".to_string(), "--soft".to_string(), before.to_string()]]),
            undone: false,
        }
    }

    /// An entry with no inverse, as recorded for a push.
    fn push_entry(before: &str, after: &str) -> Entry {
        Entry {
            id: 0,
            time: 0,
            description: "push origin main".to_string(),
            before: snap(before),
            after: snap(after),
            forward: vec![vec!["push".to_string()]],
            inverse: None,
            undone: false,
        }
    }

    #[test]
    fn record_assigns_increasing_ids_and_clears_undone() {
        let mut j = Journal::default();
        let mut e = entry("commit a", "h0", "h1");
        e.undone = true; // record() must clear this regardless of what's passed in
        let id0 = j.record(e);
        let id1 = j.record(entry("commit b", "h1", "h2"));
        assert_eq!((id0, id1), (0, 1));
        assert!(!j.peek_undo().expect("has entry").undone);
    }

    #[test]
    fn undo_then_redo_round_trip() {
        let mut j = Journal::default();
        j.record(entry("commit a", "h0", "h1"));

        let inverse = j.undo(&snap("h1")).expect("undo succeeds against `after`");
        assert_eq!(inverse, vec![vec!["reset".to_string(), "--soft".to_string(), "h0".to_string()]]);
        assert_eq!(j.peek_undo(), None, "nothing left to undo");
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("commit a"));

        let forward = j.redo(&snap("h0")).expect("redo succeeds against `before`");
        assert_eq!(forward, vec![vec!["commit".to_string()]]);
        assert_eq!(j.peek_redo(), None, "nothing left to redo");
        assert_eq!(j.peek_undo().map(|e| e.description.as_str()), Some("commit a"));
    }

    #[test]
    fn undo_and_redo_on_empty_journal_are_nothing_to_do() {
        let mut j = Journal::default();
        assert_eq!(j.undo(&snap("h0")), Err(Refused::NothingToDo));
        assert_eq!(j.redo(&snap("h0")), Err(Refused::NothingToDo));
    }

    #[test]
    fn peek_redo_is_the_oldest_of_the_trailing_undone_run() {
        // E1..E4, each moving one commit further; undo twice from the newest end.
        let mut j = Journal::default();
        j.record(entry("E1", "h0", "h1"));
        j.record(entry("E2", "h1", "h2"));
        j.record(entry("E3", "h2", "h3"));
        j.record(entry("E4", "h3", "h4"));

        j.undo(&snap("h4")).expect("undo E4");
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("E4"));

        j.undo(&snap("h3")).expect("undo E3");
        // Trailing undone run is now [E3, E4]; the oldest of that run (E3) is the one most
        // recently undone, and thus what Mod+Shift+Z brings back first.
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("E3"));
        assert_eq!(j.peek_undo().map(|e| e.description.as_str()), Some("E2"));

        j.redo(&snap("h2")).expect("redo E3");
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("E4"));
    }

    #[test]
    fn record_discards_the_redo_tail() {
        let mut j = Journal::default();
        j.record(entry("E1", "h0", "h1"));
        j.undo(&snap("h1")).expect("undo E1");
        assert!(j.peek_redo().is_some());

        j.record(entry("E2", "h0", "h2"));
        assert_eq!(j.peek_redo(), None, "E1 was dropped as the redo tail");
        assert_eq!(j.entries().count(), 1);
    }

    #[test]
    fn cap_keeps_the_newest_entries() {
        let mut j = Journal::default();
        for i in 0..(CAP + 50) {
            j.record(entry(&format!("commit {i}"), "h0", "h1"));
        }
        let descriptions: Vec<&str> = j.entries().map(|e| e.description.as_str()).collect();
        assert_eq!(descriptions.len(), CAP);
        // Newest first; the newest is the last one recorded, the oldest kept is #50.
        assert_eq!(descriptions[0], format!("commit {}", CAP + 49));
        assert_eq!(descriptions[CAP - 1], "commit 50");
    }

    #[test]
    fn undo_guard_rejects_missing_ref() {
        let mut j = Journal::default();
        j.record(entry("commit a", "h0", "h1"));

        let mut current = snap("h1");
        current.refs.remove("refs/heads/main");
        assert_eq!(j.undo(&current), Err(Refused::Moved("refs/heads/main".to_string())));
    }

    #[test]
    fn undo_guard_rejects_moved_ref() {
        let mut j = Journal::default();
        j.record(entry("commit a", "h0", "h1"));

        let current = snap("other-hash");
        assert_eq!(j.undo(&current), Err(Refused::Moved("refs/heads/main".to_string())));
    }

    #[test]
    fn undo_guard_rejects_moved_head_when_refs_match() {
        let mut j = Journal::default();
        let mut e = entry("commit a", "h0", "h1");
        // Refs already agree; only HEAD (e.g. detached) disagrees.
        let mut current = snap("h1");
        current.head = Some("h1-detached".to_string());
        e.after.refs = current.refs.clone();
        j.record(e);
        assert_eq!(j.undo(&current), Err(Refused::Moved("HEAD".to_string())));
    }

    #[test]
    fn redo_guard_symmetrical_against_before() {
        let mut j = Journal::default();
        j.record(entry("commit a", "h0", "h1"));
        j.undo(&snap("h1")).expect("undo succeeds");

        // Something else moved main to h9 in between undo and redo.
        assert_eq!(j.redo(&snap("h9")), Err(Refused::Moved("refs/heads/main".to_string())));
        // The entry stays undone: redo did not fire.
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("commit a"));
    }

    #[test]
    fn undo_of_push_is_irreversible_and_leaves_it_undone() {
        let mut j = Journal::default();
        j.record(push_entry("h0", "h1"));

        assert_eq!(j.undo(&snap("h1")), Err(Refused::Irreversible("push origin main".to_string())));
        // Not marked undone, so it is still what Mod+Z would report, and it can never reach the
        // redo tail (redo of an undone push can't happen: push is never marked undone).
        assert_eq!(j.peek_undo().map(|e| e.description.as_str()), Some("push origin main"));
        assert_eq!(j.peek_redo(), None);
    }

    #[test]
    fn irreversible_check_runs_before_the_move_guard() {
        // Even when the guard would also fail, an irreversible entry reports Irreversible.
        let mut j = Journal::default();
        j.record(push_entry("h0", "h1"));
        assert_eq!(j.undo(&snap("moved")), Err(Refused::Irreversible("push origin main".to_string())));
    }

    #[test]
    fn load_missing_file_is_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.json");
        let j = Journal::load(&path).expect("missing file loads as default");
        assert_eq!(j.entries().count(), 0);
    }

    #[test]
    fn save_then_load_round_trips_and_creates_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("journal.json");

        let mut j = Journal::default();
        j.record(entry("commit a", "h0", "h1"));
        j.undo(&snap("h1")).expect("undo succeeds");
        j.save(&path).expect("save");

        // The atomic-rename temp file is not left behind.
        let tmp = dir.path().join("nested").join("journal.json.tmp");
        assert!(!tmp.exists());

        let loaded = Journal::load(&path).expect("load");
        assert_eq!(loaded.entries().count(), 1);
        assert_eq!(loaded.peek_redo().map(|e| e.description.as_str()), Some("commit a"));

        // A further record() on the loaded journal keeps assigning fresh, increasing ids.
        let mut loaded = loaded;
        let id = loaded.record(entry("commit b", "h1", "h2"));
        assert_eq!(id, 1);
    }

    /// End to end against a real repo: an entry's inverse actually undoes the commit, and once
    /// the branch moves out from under the journal (a terminal or agent ran git), both undo and
    /// redo refuse with `Moved` rather than silently discarding the out-of-band work.
    #[test]
    fn integration_undo_runs_against_real_git_and_guards_out_of_band_moves() {
        let mut repo = crate::testutil::TempRepo::new();
        let h0 = repo.commit("first");
        let h1 = repo.commit("second");

        let mut j = Journal::default();
        j.record(Entry {
            id: 0,
            time: 0,
            description: format!("commit {h1}"),
            before: snap(&h0),
            after: snap(&h1),
            forward: vec![vec!["commit".to_string(), "--allow-empty".to_string()]],
            inverse: Some(vec![vec!["reset".to_string(), "--soft".to_string(), h0.clone()]]),
            undone: false,
        });

        // The UI's current snapshot matches `after`: undo is allowed.
        let plan = j.undo(&snap(&h1)).expect("undo succeeds against the real post-commit state");
        for cmd in &plan {
            let args: Vec<&str> = cmd.iter().map(String::as_str).collect();
            let status = crate::testutil::hermetic_git(repo.path()).args(&args).status().expect("spawn git");
            assert!(status.success(), "inverse plan {cmd:?} failed");
        }
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), h0, "HEAD moved back to the pre-commit hash");

        // A terminal or agent now commits without going through the journal.
        let h_oob = repo.commit("out of band");
        let real_current = Snapshot {
            head: Some(h_oob.clone()),
            branch: Some("main".to_string()),
            refs: BTreeMap::from([("refs/heads/main".to_string(), h_oob.clone())]),
            stashes: vec![],
        };

        // Redo of the entry we just undid: `before` no longer matches the real repo.
        assert_eq!(j.redo(&real_current), Err(Refused::Moved("refs/heads/main".to_string())));

        // A second, not-yet-undone entry whose `after` was invalidated by the same out-of-band
        // commit: undo refuses too, rather than resetting past work it never journaled.
        let mut j2 = Journal::default();
        j2.record(Entry {
            id: 0,
            time: 0,
            description: "commit unrelated".to_string(),
            before: snap(&h0),
            after: snap(&h1),
            forward: vec![vec!["commit".to_string()]],
            inverse: Some(vec![vec!["reset".to_string(), "--soft".to_string(), h0.clone()]]),
            undone: false,
        });
        assert_eq!(j2.undo(&real_current), Err(Refused::Moved("refs/heads/main".to_string())));
    }
}
