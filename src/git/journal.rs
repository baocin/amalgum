//! Undo/redo journal (§5.18). Every ref-changing operation records the repo state before and
//! after, the forward command plan (for redo), and the inverse plan (for undo; `None` for push,
//! shown greyed as "Cannot be undone from the client"). Stored per repo as JSON, capped at
//! [`CAP`] entries (oldest dropped), surviving restarts.
//!
//! Guard: undo runs only if the repo still matches the entry's `after` snapshot — HEAD, the
//! branch HEAD is on, the refs the entry touched, and the stash list if it recorded one — (and
//! redo only if it matches `before`); otherwise the repo moved underneath us (a terminal or an
//! agent ran git) and the caller shows "Can't undo: `main` has moved since. Open reflog?".
//! An entry is marked undone (or redone) once its plan has run successfully, or when a command
//! of the plan failed but re-reading the repo finds it in the state the plan leads to anyway.
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
    /// Stash commit hashes, newest (`stash@{0}`) first; empty when the operation doesn't depend
    /// on the stash list, and then the guard ignores it.
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
    /// Named ref no longer matches the journal: a touched ref, `HEAD` (its commit or branch), or
    /// `refs/stash` (the stash list).
    Moved(String),
}

/// Why [`Journal::undo`] or [`Journal::redo`] reports an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failed<E> {
    /// Refused before anything ran. The entry keeps its undone state.
    Refused(Refused),
    /// A command of the plan failed and the repo is not in the state the plan leads to. The
    /// entry keeps its undone state, so the same action can be retried; a multi-command plan
    /// may have stopped part-way, in which case the guard refuses the next attempt.
    Run(E),
    /// A command of the plan failed, yet the repo is in the state the plan leads to (and no
    /// longer in the one it started from): `git checkout` exits with a failing `post-checkout`
    /// hook's status after switching, say. The entry was marked like after a success.
    AppliedWithError(E),
}

impl<E> From<Refused> for Failed<E> {
    fn from(refused: Refused) -> Self {
        Failed::Refused(refused)
    }
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
    /// `Mod+Z`: check the guard against `current`, hand the inverse plan of the entry
    /// [`Journal::peek_undo`] names to `run`, and mark that entry undone if `run` succeeds.
    /// When it fails, `reread` snapshots the repo again (the same refs and stash list as
    /// `current`): a command can exit non-zero after its change took effect, so the entry is
    /// still marked, as [`Failed::AppliedWithError`], if the repo now matches its `before` and
    /// no longer its `after`. Otherwise (an agent holding a lock, say) the journal is left as it
    /// was, so the undo can be retried.
    ///
    /// The spec (§5.18) says undo runs "if `before` still matches", but that reads as shorthand
    /// for "the repo is still in the state the operation produced": undo replays the *inverse* of
    /// an operation that already ran, so it is only safe when the repo is still sitting in that
    /// operation's `after` state (nothing else touched the refs since). Comparing against
    /// `before` would let undo fire even after unrelated commits landed on top, silently
    /// discarding them. Redo is symmetrical against `before`, since it is only safe to replay the
    /// forward command from the exact state that preceded it.
    pub fn undo<E>(
        &mut self,
        current: &Snapshot,
        run: impl FnOnce(&Plan) -> Result<(), E>,
        reread: impl FnOnce() -> Snapshot,
    ) -> Result<(), Failed<E>> {
        let idx = self.entries.iter().rposition(|e| !e.undone).ok_or(Refused::NothingToDo)?;
        let entry = &self.entries[idx];
        let Some(inverse) = &entry.inverse else {
            return Err(Refused::Irreversible(entry.description.clone()).into());
        };
        if let Some(name) = mismatch(&entry.after, current) {
            return Err(Refused::Moved(name).into());
        }
        let result = run(inverse);
        self.settle(idx, true, result, reread)
    }
    /// `Mod+Shift+Z`: like [`Journal::undo`], for the entry [`Journal::peek_redo`] names, guarded
    /// against its `before` snapshot and running its forward plan (so a failed run still marks
    /// the entry redone if the repo reached its `after`).
    pub fn redo<E>(
        &mut self,
        current: &Snapshot,
        run: impl FnOnce(&Plan) -> Result<(), E>,
        reread: impl FnOnce() -> Snapshot,
    ) -> Result<(), Failed<E>> {
        let idx = self.redo_run_start().ok_or(Refused::NothingToDo)?;
        let entry = &self.entries[idx];
        if let Some(name) = mismatch(&entry.before, current) {
            return Err(Refused::Moved(name).into());
        }
        let result = run(&entry.forward);
        self.settle(idx, false, result, reread)
    }
    /// Sets entry `idx`'s `undone` flag to `undone` after its plan ran with `result`: always on
    /// success; on failure only if `reread` finds the repo in the snapshot the plan leads to
    /// (`before` for an undo, `after` for a redo) and no longer in the one it started from.
    fn settle<E>(
        &mut self,
        idx: usize,
        undone: bool,
        result: Result<(), E>,
        reread: impl FnOnce() -> Snapshot,
    ) -> Result<(), Failed<E>> {
        let failed = match result {
            Ok(()) => None,
            Err(e) => {
                let entry = &self.entries[idx];
                let (target, start) =
                    if undone { (&entry.before, &entry.after) } else { (&entry.after, &entry.before) };
                let now = reread();
                if mismatch(target, &now).is_some() || mismatch(start, &now).is_none() {
                    return Err(Failed::Run(e));
                }
                Some(Failed::AppliedWithError(e))
            }
        };
        self.entries[idx].undone = undone;
        failed.map_or(Ok(()), Err)
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
/// from `current` counts as a mismatch), else `"HEAD"` if `snapshot.head` is set and disagrees
/// or HEAD is on a different branch (or detached vs. on one) than `snapshot.branch` — inverses
/// like `reset --soft` move whichever branch HEAD is on — else `"refs/stash"` if `snapshot`
/// recorded a stash list and `current`'s differs, else `None`.
fn mismatch(snapshot: &Snapshot, current: &Snapshot) -> Option<String> {
    for (name, hash) in &snapshot.refs {
        if current.refs.get(name) != Some(hash) {
            return Some(name.clone());
        }
    }
    let head_moved = snapshot.head.as_ref().is_some_and(|head| current.head.as_ref() != Some(head));
    if head_moved || snapshot.branch != current.branch {
        return Some("HEAD".to_string());
    }
    if !snapshot.stashes.is_empty() && snapshot.stashes != current.stashes {
        return Some("refs/stash".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

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

    /// Undo with a runner that always succeeds; returns the plan it was handed.
    fn undo_ok(j: &mut Journal, current: &Snapshot) -> Result<Plan, Refused> {
        let mut ran = Plan::new();
        let run = |plan: &Plan| -> Result<(), Infallible> {
            ran = plan.clone();
            Ok(())
        };
        match j.undo(current, run, || unreachable!("the run never fails")) {
            Ok(()) => Ok(ran),
            Err(Failed::Refused(refused)) => Err(refused),
            Err(Failed::Run(never) | Failed::AppliedWithError(never)) => match never {},
        }
    }

    /// Redo with a runner that always succeeds; returns the plan it was handed.
    fn redo_ok(j: &mut Journal, current: &Snapshot) -> Result<Plan, Refused> {
        let mut ran = Plan::new();
        let run = |plan: &Plan| -> Result<(), Infallible> {
            ran = plan.clone();
            Ok(())
        };
        match j.redo(current, run, || unreachable!("the run never fails")) {
            Ok(()) => Ok(ran),
            Err(Failed::Refused(refused)) => Err(refused),
            Err(Failed::Run(never) | Failed::AppliedWithError(never)) => match never {},
        }
    }

    /// Runs `plan` in `repo`, stopping at the first failing command and returning its stderr.
    fn run_in(repo: &crate::testutil::TempRepo, plan: &Plan) -> Result<(), String> {
        for cmd in plan {
            let out = crate::testutil::hermetic_git(repo.path()).args(cmd).output().expect("spawn git");
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).into_owned());
            }
        }
        Ok(())
    }

    #[test]
    fn failed_undo_leaves_the_entry_to_undo_again() {
        let mut j = Journal::default();
        j.record(entry("commit a", "h0", "h1"));

        assert_eq!(
            j.undo(&snap("h1"), |_| Err("index.lock exists"), || snap("h1")),
            Err(Failed::Run("index.lock exists"))
        );
        assert_eq!(j.peek_undo().map(|e| e.description.as_str()), Some("commit a"));
        assert_eq!(j.peek_redo(), None, "nothing was undone, so nothing to redo");

        // The repo is still in `after`, so the retry passes the guard.
        assert!(undo_ok(&mut j, &snap("h1")).is_ok());
    }

    #[test]
    fn failed_redo_leaves_the_entry_to_redo_again() {
        let mut j = Journal::default();
        j.record(entry("commit a", "h0", "h1"));
        undo_ok(&mut j, &snap("h1")).expect("undo succeeds");

        assert_eq!(
            j.redo(&snap("h0"), |_| Err("index.lock exists"), || snap("h0")),
            Err(Failed::Run("index.lock exists"))
        );
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("commit a"));
        assert_eq!(j.peek_undo(), None);

        assert!(redo_ok(&mut j, &snap("h0")).is_ok());
    }

    #[test]
    fn failed_undo_that_reached_before_anyway_marks_the_entry_undone() {
        // `git checkout main` switched, then exited 1 with a failing post-checkout hook.
        let mut j = Journal::default();
        j.record(entry("checkout feat", "h0", "h1"));

        let result = j.undo(&snap("h1"), |_| Err("post-checkout hook failed"), || snap("h0"));
        assert_eq!(result, Err(Failed::AppliedWithError("post-checkout hook failed")));
        assert_eq!(j.peek_undo(), None, "the repo is in `before`: the entry is undone");
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("checkout feat"));

        let result = j.redo(&snap("h0"), |_| Err("post-checkout hook failed"), || snap("h1"));
        assert_eq!(result, Err(Failed::AppliedWithError("post-checkout hook failed")));
        assert_eq!(j.peek_undo().map(|e| e.description.as_str()), Some("checkout feat"));
        assert_eq!(j.peek_redo(), None);
    }

    #[test]
    fn failed_run_is_not_marked_when_before_and_after_look_alike() {
        // A stash pop that failed on a conflict keeps the stash: the repo still matches `after`
        // (and `before`, which recorded no stash list), so nothing shows the undo took effect.
        let mut j = Journal::default();
        let mut e = entry("stash push", "h1", "h1");
        e.after.stashes = vec!["s1".to_string()];
        e.inverse = Some(vec![vec!["stash".to_string(), "pop".to_string()]]);
        j.record(e);
        let mut current = snap("h1");
        current.stashes = vec!["s1".to_string()];

        let result = j.undo(&current, |_| Err("conflict"), || current.clone());
        assert_eq!(result, Err(Failed::Run("conflict")));
        assert_eq!(j.peek_undo().map(|e| e.description.as_str()), Some("stash push"));
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

        let inverse = undo_ok(&mut j, &snap("h1")).expect("undo succeeds against `after`");
        assert_eq!(inverse, vec![vec!["reset".to_string(), "--soft".to_string(), "h0".to_string()]]);
        assert_eq!(j.peek_undo(), None, "nothing left to undo");
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("commit a"));

        let forward = redo_ok(&mut j, &snap("h0")).expect("redo succeeds against `before`");
        assert_eq!(forward, vec![vec!["commit".to_string()]]);
        assert_eq!(j.peek_redo(), None, "nothing left to redo");
        assert_eq!(j.peek_undo().map(|e| e.description.as_str()), Some("commit a"));
    }

    #[test]
    fn undo_and_redo_on_empty_journal_are_nothing_to_do() {
        let mut j = Journal::default();
        assert_eq!(undo_ok(&mut j, &snap("h0")), Err(Refused::NothingToDo));
        assert_eq!(redo_ok(&mut j, &snap("h0")), Err(Refused::NothingToDo));
    }

    #[test]
    fn peek_redo_is_the_oldest_of_the_trailing_undone_run() {
        // E1..E4, each moving one commit further; undo twice from the newest end.
        let mut j = Journal::default();
        j.record(entry("E1", "h0", "h1"));
        j.record(entry("E2", "h1", "h2"));
        j.record(entry("E3", "h2", "h3"));
        j.record(entry("E4", "h3", "h4"));

        undo_ok(&mut j, &snap("h4")).expect("undo E4");
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("E4"));

        undo_ok(&mut j, &snap("h3")).expect("undo E3");
        // Trailing undone run is now [E3, E4]; the oldest of that run (E3) is the one most
        // recently undone, and thus what Mod+Shift+Z brings back first.
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("E3"));
        assert_eq!(j.peek_undo().map(|e| e.description.as_str()), Some("E2"));

        redo_ok(&mut j, &snap("h2")).expect("redo E3");
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("E4"));
    }

    #[test]
    fn record_discards_the_redo_tail() {
        let mut j = Journal::default();
        j.record(entry("E1", "h0", "h1"));
        undo_ok(&mut j, &snap("h1")).expect("undo E1");
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
        assert_eq!(undo_ok(&mut j, &current), Err(Refused::Moved("refs/heads/main".to_string())));
    }

    #[test]
    fn undo_guard_rejects_moved_ref() {
        let mut j = Journal::default();
        j.record(entry("commit a", "h0", "h1"));

        let current = snap("other-hash");
        assert_eq!(undo_ok(&mut j, &current), Err(Refused::Moved("refs/heads/main".to_string())));
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
        assert_eq!(undo_ok(&mut j, &current), Err(Refused::Moved("HEAD".to_string())));
    }

    #[test]
    fn undo_guard_rejects_another_branch_or_detached_head_at_the_same_commit() {
        // `reset --soft h0` moves whatever branch HEAD is on, so the same hash on another
        // branch (or detached) is not the state the entry produced.
        let mut j = Journal::default();
        j.record(entry("commit a", "h0", "h1"));

        let mut on_wip = snap("h1");
        on_wip.branch = Some("wip".to_string());
        assert_eq!(undo_ok(&mut j, &on_wip), Err(Refused::Moved("HEAD".to_string())));

        let mut detached = snap("h1");
        detached.branch = None;
        assert_eq!(undo_ok(&mut j, &detached), Err(Refused::Moved("HEAD".to_string())));
    }

    #[test]
    fn redo_guard_rejects_another_branch_at_the_same_commit() {
        let mut j = Journal::default();
        j.record(entry("commit a", "h0", "h1"));
        undo_ok(&mut j, &snap("h1")).expect("undo succeeds");

        let mut on_wip = snap("h0");
        on_wip.branch = Some("wip".to_string());
        assert_eq!(redo_ok(&mut j, &on_wip), Err(Refused::Moved("HEAD".to_string())));
    }

    #[test]
    fn undo_guard_compares_the_stash_list_when_the_entry_recorded_one() {
        let mut j = Journal::default();
        let mut e = entry("stash push", "h1", "h1");
        e.after.stashes = vec!["s1".to_string()];
        e.inverse = Some(vec![vec!["stash".to_string(), "pop".to_string()]]);
        j.record(e);

        // A stash pushed from a terminal since: `stash pop` would pop that one instead.
        let mut current = snap("h1");
        current.stashes = vec!["s2".to_string(), "s1".to_string()];
        assert_eq!(undo_ok(&mut j, &current), Err(Refused::Moved("refs/stash".to_string())));

        current.stashes = vec!["s1".to_string()];
        assert!(undo_ok(&mut j, &current).is_ok());
    }

    #[test]
    fn guard_ignores_the_stash_list_when_the_entry_recorded_none() {
        let mut j = Journal::default();
        j.record(entry("commit a", "h0", "h1"));
        let mut current = snap("h1");
        current.stashes = vec!["s1".to_string()];
        assert!(undo_ok(&mut j, &current).is_ok());
    }

    #[test]
    fn redo_guard_symmetrical_against_before() {
        let mut j = Journal::default();
        j.record(entry("commit a", "h0", "h1"));
        undo_ok(&mut j, &snap("h1")).expect("undo succeeds");

        // Something else moved main to h9 in between undo and redo.
        assert_eq!(redo_ok(&mut j, &snap("h9")), Err(Refused::Moved("refs/heads/main".to_string())));
        // The entry stays undone: redo did not fire.
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("commit a"));
    }

    #[test]
    fn undo_of_push_is_irreversible_and_leaves_it_undone() {
        let mut j = Journal::default();
        j.record(push_entry("h0", "h1"));

        assert_eq!(undo_ok(&mut j, &snap("h1")), Err(Refused::Irreversible("push origin main".to_string())));
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
        assert_eq!(
            undo_ok(&mut j, &snap("moved")),
            Err(Refused::Irreversible("push origin main".to_string()))
        );
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
        undo_ok(&mut j, &snap("h1")).expect("undo succeeds");
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
        j.undo(&snap(&h1), |plan| run_in(&repo, plan), || unreachable!("the reset succeeds"))
            .expect("undo succeeds against the real post-commit state");
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
        assert_eq!(redo_ok(&mut j, &real_current), Err(Refused::Moved("refs/heads/main".to_string())));

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
        assert_eq!(undo_ok(&mut j2, &real_current), Err(Refused::Moved("refs/heads/main".to_string())));
    }

    /// The repo's HEAD, current branch, and `refs/heads/main`, read from real git the way the UI
    /// builds its guard snapshot.
    fn real_snapshot(repo: &crate::testutil::TempRepo) -> Snapshot {
        let branch = crate::testutil::hermetic_git(repo.path())
            .args(["symbolic-ref", "-q", "HEAD"])
            .output()
            .expect("spawn git");
        let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();
        let branch = branch.strip_prefix("refs/heads/").map(str::to_string).unwrap_or(branch);
        Snapshot {
            head: Some(repo.git(&["rev-parse", "HEAD"])),
            branch: (!branch.is_empty()).then_some(branch),
            refs: BTreeMap::from([(
                "refs/heads/main".to_string(),
                repo.git(&["rev-parse", "refs/heads/main"]),
            )]),
            stashes: vec![],
        }
    }

    /// An agent creates a branch at the commit the app just made. HEAD and `main` still hold
    /// the entry's hashes, but its `reset --soft` inverse would now move `wip`, not `main`.
    #[test]
    fn integration_undo_refuses_once_head_is_on_another_branch_at_the_same_commit() {
        let mut repo = crate::testutil::TempRepo::new();
        let h0 = repo.commit("first");
        let h1 = repo.commit("second");
        let mut j = Journal::default();
        j.record(entry("commit second", &h0, &h1));
        assert_eq!(real_snapshot(&repo), snap(&h1), "the entry's `after` is the real state");

        repo.git(&["switch", "-q", "-c", "wip"]);
        assert_eq!(undo_ok(&mut j, &real_snapshot(&repo)), Err(Refused::Moved("HEAD".to_string())));

        repo.git(&["switch", "-q", "--detach"]);
        assert_eq!(undo_ok(&mut j, &real_snapshot(&repo)), Err(Refused::Moved("HEAD".to_string())));
    }

    /// Another git process holds `main`'s lock when `Mod+Z` runs `reset --soft`: the command
    /// fails, the entry stays undoable, and the retry works once the lock is gone.
    #[test]
    fn integration_failed_undo_can_be_retried_once_the_lock_is_released() {
        let mut repo = crate::testutil::TempRepo::new();
        let h0 = repo.commit("first");
        let h1 = repo.commit("second");
        let mut j = Journal::default();
        j.record(entry("commit second", &h0, &h1));

        let lock = repo.path().join(".git").join("refs").join("heads").join("main.lock");
        std::fs::write(&lock, "").expect("take the lock");
        let result = j.undo(&real_snapshot(&repo), |plan| run_in(&repo, plan), || real_snapshot(&repo));
        assert!(matches!(&result, Err(Failed::Run(stderr)) if stderr.contains("main.lock")), "{result:?}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), h1, "nothing moved");
        assert_eq!(j.peek_undo().map(|e| e.description.as_str()), Some("commit second"));

        std::fs::remove_file(&lock).expect("release the lock");
        j.undo(&real_snapshot(&repo), |plan| run_in(&repo, plan), || real_snapshot(&repo))
            .expect("the retry succeeds");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), h0);
        assert_eq!(j.peek_redo().map(|e| e.description.as_str()), Some("commit second"));
    }
}
