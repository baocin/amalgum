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
    pub fn push(&mut self, mut n: Notification) -> Option<u64> {
        if let Some(ws) = &n.workspace
            && self.muted.contains(ws)
        {
            return None;
        }
        // next_id starts at 0 by derive(Default); the first id assigned is 1.
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        n.id = id;
        n.read = false;
        self.items.push_front(n);
        while self.items.len() > CAP {
            self.items.pop_back();
        }
        Some(id)
    }
    /// Newest first.
    pub fn iter(&self) -> impl Iterator<Item = &Notification> {
        self.items.iter()
    }
    pub fn unread(&self) -> usize {
        self.items.iter().filter(|n| !n.read).count()
    }
    /// Focusing a tab marks its rows read.
    pub fn mark_tab_read(&mut self, tab: &str) {
        for n in self.items.iter_mut() {
            if n.tab.as_deref() == Some(tab) {
                n.read = true;
            }
        }
    }
    pub fn mark_all_read(&mut self) {
        for n in self.items.iter_mut() {
            n.read = true;
        }
    }
    pub fn clear(&mut self) {
        self.items.clear();
    }
    pub fn set_muted(&mut self, workspace: &str, muted: bool) {
        if muted {
            self.muted.insert(workspace.to_string());
        } else {
            self.muted.remove(workspace);
        }
    }
    /// Missing file is an empty store, not an error.
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut store = Store::default();
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(store),
            Err(e) => return Err(e),
        };
        // Lines are newest-first, as `save` wrote them; pushing to the back in that order
        // reproduces the same front-to-back (newest-first) iteration order.
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(n) = serde_json::from_str::<Notification>(line) {
                store.items.push_back(n);
            }
            // Corrupt lines are skipped silently.
        }
        store.next_id = store.items.iter().map(|n| n.id).max().unwrap_or(0) + 1;
        Ok(store)
    }
    /// Atomic write (temp file + rename). The muted set is in-memory app state, not persisted
    /// here.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut buf = String::new();
        for n in self.items.iter() {
            let line = serde_json::to_string(n).map_err(io::Error::other)?;
            buf.push_str(&line);
            buf.push('\n');
        }
        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "notifications path has no file name")
        })?;
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(".tmp");
        let tmp_path = path.with_file_name(tmp_name);
        std::fs::write(&tmp_path, buf)?;
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notif(workspace: Option<&str>, tab: Option<&str>, title: &str) -> Notification {
        Notification {
            id: 0,
            time: 0,
            level: Level::Info,
            title: title.to_string(),
            body: String::new(),
            workspace: workspace.map(str::to_string),
            tab: tab.map(str::to_string),
            agent: None,
            commit: None,
            read: true, // push() must overwrite this to false
        }
    }

    // -- push -------------------------------------------------------------------------------

    #[test]
    fn push_assigns_monotonic_ids_starting_at_one_and_marks_unread() {
        let mut store = Store::default();
        let id1 = store.push(notif(None, None, "first")).unwrap();
        let id2 = store.push(notif(None, None, "second")).unwrap();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert!(store.iter().all(|n| !n.read));
    }

    #[test]
    fn push_inserts_at_the_front_newest_first() {
        let mut store = Store::default();
        store.push(notif(None, None, "first"));
        store.push(notif(None, None, "second"));
        let titles: Vec<&str> = store.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, vec!["second", "first"]);
    }

    #[test]
    fn push_drops_oldest_beyond_cap() {
        let mut store = Store::default();
        for i in 0..(CAP + 5) {
            store.push(notif(None, None, &i.to_string()));
        }
        assert_eq!(store.iter().count(), CAP);
        // The newest CAP items survive; the oldest 5 (titles "0".."4") are gone.
        let newest_title: &str = store.iter().next().unwrap().title.as_str();
        assert_eq!(newest_title, (CAP + 4).to_string());
        assert!(store.iter().all(|n| n.title.parse::<usize>().unwrap() >= 5));
    }

    #[test]
    fn push_to_muted_workspace_is_dropped_and_not_stored() {
        let mut store = Store::default();
        store.set_muted("ws1", true);
        assert_eq!(store.push(notif(Some("ws1"), None, "hushed")), None);
        assert_eq!(store.iter().count(), 0);

        // A different, unmuted workspace still stores normally.
        assert!(store.push(notif(Some("ws2"), None, "audible")).is_some());
        assert_eq!(store.iter().count(), 1);
    }

    #[test]
    fn push_without_workspace_is_never_muted() {
        let mut store = Store::default();
        store.set_muted("ws1", true);
        assert!(store.push(notif(None, None, "n/a workspace")).is_some());
    }

    #[test]
    fn unmuting_lets_future_pushes_through_again() {
        let mut store = Store::default();
        store.set_muted("ws1", true);
        assert!(store.push(notif(Some("ws1"), None, "a")).is_none());
        store.set_muted("ws1", false);
        assert!(store.push(notif(Some("ws1"), None, "b")).is_some());
    }

    // -- unread / mark_read -------------------------------------------------------------------

    #[test]
    fn unread_counts_only_unread_rows() {
        let mut store = Store::default();
        store.push(notif(None, Some("tab-a"), "a"));
        store.push(notif(None, Some("tab-b"), "b"));
        assert_eq!(store.unread(), 2);
        store.mark_tab_read("tab-a");
        assert_eq!(store.unread(), 1);
        store.mark_all_read();
        assert_eq!(store.unread(), 0);
    }

    #[test]
    fn mark_tab_read_only_affects_that_tab() {
        let mut store = Store::default();
        store.push(notif(None, Some("tab-a"), "a1"));
        store.push(notif(None, Some("tab-a"), "a2"));
        store.push(notif(None, Some("tab-b"), "b1"));
        store.mark_tab_read("tab-a");
        let unread_titles: Vec<&str> = store.iter().filter(|n| !n.read).map(|n| n.title.as_str()).collect();
        assert_eq!(unread_titles, vec!["b1"]);
    }

    #[test]
    fn clear_empties_the_store() {
        let mut store = Store::default();
        store.push(notif(None, None, "a"));
        store.clear();
        assert_eq!(store.iter().count(), 0);
        assert_eq!(store.unread(), 0);
    }

    // -- load / save --------------------------------------------------------------------------

    #[test]
    fn load_missing_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notifications.jsonl");
        let store = Store::load(&path).unwrap();
        assert_eq!(store.iter().count(), 0);
        // A fresh load still starts numbering at 1.
        let mut store = store;
        assert_eq!(store.push(notif(None, None, "x")), Some(1));
    }

    #[test]
    fn save_then_load_round_trips_newest_first_order_and_next_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("notifications.jsonl");
        let mut store = Store::default();
        store.push(notif(None, None, "first"));
        store.push(notif(None, None, "second"));
        store.push(notif(None, None, "third"));
        store.save(&path).unwrap();
        assert!(path.exists(), "save must create the parent dir");

        let loaded = Store::load(&path).unwrap();
        let titles: Vec<&str> = loaded.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, vec!["third", "second", "first"]);

        // next_id continues past the highest id in the file.
        let mut loaded = loaded;
        assert_eq!(loaded.push(notif(None, None, "fourth")), Some(4));
    }

    #[test]
    fn load_skips_corrupt_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notifications.jsonl");
        let good = notif(None, None, "good");
        let mut good_with_id = good.clone();
        good_with_id.id = 7;
        let line = serde_json::to_string(&good_with_id).unwrap();
        std::fs::write(&path, format!("{line}\nnot json at all\n\n{{\"broken\":\n")).unwrap();

        let store = Store::load(&path).unwrap();
        let titles: Vec<&str> = store.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, vec!["good"]);
        // next_id restored from the one valid line's id (7), so the next push is 8.
        let mut store = store;
        assert_eq!(store.push(notif(None, None, "next")), Some(8));
    }

    #[test]
    fn save_is_atomic_and_does_not_persist_the_muted_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notifications.jsonl");
        let mut store = Store::default();
        store.set_muted("ws1", true);
        store.push(notif(Some("ws2"), None, "kept"));
        store.save(&path).unwrap();

        // No leftover temp file after a successful save.
        let tmp_path = dir.path().join("notifications.jsonl.tmp");
        assert!(!tmp_path.exists());

        // Reloading does not know ws1 was muted: a load-only Store starts unmuted.
        let mut loaded = Store::load(&path).unwrap();
        assert!(loaded.push(notif(Some("ws1"), None, "unmuted after reload")).is_some());
    }
}
