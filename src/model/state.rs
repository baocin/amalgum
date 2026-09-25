//! Persistent app state (§5.22 "Per-workspace state"): repo › remote › workspace › tab › pane,
//! recent locations, and the active workspace, saved to `state.json`. Sidebar order is vector
//! order. Closing the last workspace of a repo keeps the repo group until `forget_repo`.

use super::layout::{PaneId, Tree};
use crate::agent::AgentKind;
use crate::agent::status::Status;
use crate::git::Location;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

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
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid state.json: {e}")))
    }

    /// Atomic write (temp file + rename): a crash or concurrent read never sees a half-written
    /// `state.json`. Creates the parent directory if it doesn't exist yet.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let tmp = tmp_path_for(path);
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Fresh id with a prefix (`"w"` → `"w7"`), unique within this state forever: backed by a
    /// persisted, monotonically increasing counter, so ids never repeat even after removals.
    pub fn new_id(&mut self, prefix: &str) -> String {
        let n = self.next_id;
        self.next_id += 1;
        format!("{prefix}{n}")
    }

    /// Insert `ws` under repo `repo_id` / remote `remote`, creating either group if new.
    pub fn add_workspace(
        &mut self,
        repo_id: &str,
        repo_name: &str,
        remote: &str,
        url: Option<&str>,
        ws: Workspace,
    ) {
        let repo_idx = match self.repos.iter().position(|r| r.id == repo_id) {
            Some(i) => i,
            None => {
                self.repos.push(Repo {
                    id: repo_id.to_string(),
                    name: repo_name.to_string(),
                    collapsed: false,
                    remotes: Vec::new(),
                });
                self.repos.len() - 1
            }
        };
        let repo = &mut self.repos[repo_idx];
        let group_idx = match repo.remotes.iter().position(|g| g.name == remote) {
            Some(i) => i,
            None => {
                repo.remotes.push(RemoteGroup {
                    name: remote.to_string(),
                    url: url.map(str::to_string),
                    collapsed: false,
                    workspaces: Vec::new(),
                });
                repo.remotes.len() - 1
            }
        };
        let id = ws.id.clone();
        repo.remotes[group_idx].workspaces.push(ws);
        if self.active.is_none() {
            self.active = Some(id);
        }
    }

    /// Removes `id` from its remote group (the group and its repo survive empty, per §5.22
    /// "Closing the last workspace of a repo keeps the repo group until Forget repo"). If `id`
    /// was active, the new active workspace is the next one in sidebar order, or the previous
    /// one if `id` was last, or `None` if it was the only workspace.
    pub fn remove_workspace(&mut self, id: &str) -> Option<Workspace> {
        let order = self.workspaces();
        let idx = order.iter().position(|w| w.id == id)?;
        let was_active = self.active.as_deref() == Some(id);
        let next_active = if order.len() == 1 {
            None
        } else if idx + 1 < order.len() {
            Some(order[idx + 1].id.clone())
        } else {
            Some(order[idx - 1].id.clone())
        };

        let mut removed = None;
        for repo in &mut self.repos {
            for group in &mut repo.remotes {
                if let Some(pos) = group.workspaces.iter().position(|w| w.id == id) {
                    removed = Some(group.workspaces.remove(pos));
                    break;
                }
            }
            if removed.is_some() {
                break;
            }
        }

        if was_active {
            self.active = next_active;
        }
        removed
    }

    /// Removes the repo group entirely (all its remotes and workspaces). Clears `active` if it
    /// pointed inside the removed repo.
    pub fn forget_repo(&mut self, repo_id: &str) {
        let Some(pos) = self.repos.iter().position(|r| r.id == repo_id) else { return };
        let repo = self.repos.remove(pos);
        let active_was_inside = self
            .active
            .as_deref()
            .is_some_and(|active| repo.remotes.iter().any(|g| g.workspaces.iter().any(|w| w.id == active)));
        if active_was_inside {
            self.active = None;
        }
    }

    pub fn workspace(&self, id: &str) -> Option<&Workspace> {
        self.repos
            .iter()
            .flat_map(|r| r.remotes.iter())
            .flat_map(|g| g.workspaces.iter())
            .find(|w| w.id == id)
    }

    pub fn workspace_mut(&mut self, id: &str) -> Option<&mut Workspace> {
        self.repos
            .iter_mut()
            .flat_map(|r| r.remotes.iter_mut())
            .flat_map(|g| g.workspaces.iter_mut())
            .find(|w| w.id == id)
    }

    /// All workspaces in sidebar order (for `Mod+1..9` and `Mod+Alt+↑/↓`): repos, then remotes,
    /// then workspaces, all in vector (sidebar) order.
    pub fn workspaces(&self) -> Vec<&Workspace> {
        self.repos.iter().flat_map(|r| r.remotes.iter()).flat_map(|g| g.workspaces.iter()).collect()
    }

    /// Move a workspace up (`-1`) or down (`+1`) within its remote group. False if `id` isn't
    /// found, or the move would go past either edge of the group.
    pub fn move_workspace(&mut self, id: &str, delta: i32) -> bool {
        for repo in &mut self.repos {
            for group in &mut repo.remotes {
                if let Some(pos) = group.workspaces.iter().position(|w| w.id == id) {
                    let Some(new_pos) = pos.checked_add_signed(delta as isize) else { return false };
                    if new_pos >= group.workspaces.len() {
                        return false;
                    }
                    group.workspaces.swap(pos, new_pos);
                    return true;
                }
            }
        }
        false
    }

    /// Record an opened location: dedupe by location, newest first, cap [`RECENTS_CAP`].
    pub fn touch_recent(&mut self, location: Location, label: &str, now: u64) {
        self.recents.retain(|r| r.location != location);
        self.recents.insert(0, Recent { location, label: label.to_string(), opened: now });
        self.recents.truncate(RECENTS_CAP);
    }

    /// The pane `pane` within `workspace` (searching every tab), if it exists.
    pub fn find_pane(&self, workspace: &str, pane: PaneId) -> Option<&Pane> {
        self.workspace(workspace)?.tabs.iter().flat_map(|t| t.panes.iter()).find(|p| p.id == pane)
    }

    /// Mutable access to tab `tab` within `workspace`.
    pub fn tab_mut(&mut self, workspace: &str, tab: &str) -> Option<&mut Tab> {
        self.workspace_mut(workspace)?.tabs.iter_mut().find(|t| t.id == tab)
    }
}

/// A same-directory temp path for [`AppState::save`]'s write-then-rename, unique per process so
/// concurrent saves (unexpected, but cheap to guard) never collide.
fn tmp_path_for(path: &Path) -> PathBuf {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state.json");
    path.with_file_name(format!(".{file_name}.tmp.{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::status::Status;
    use crate::model::layout::{Axis, Tree};

    /// A minimal, otherwise-default workspace for tests that only care about ids and ordering.
    fn ws(id: &str) -> Workspace {
        Workspace {
            id: id.to_string(),
            name: id.to_string(),
            location: Location::Local { path: PathBuf::from(format!("/tmp/{id}")) }, // portability: allow
            tabs: Vec::new(),
            active_tab: 0,
            git_view: GitView::default(),
            git_pane_open: true,
        }
    }

    // --- load / save -----------------------------------------------------

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let loaded = AppState::load(&path).expect("missing file should load as default");
        assert_eq!(loaded, AppState::default());
    }

    #[test]
    fn load_unparseable_file_is_invalid_data_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"not json at all {{{").expect("write");
        let err = AppState::load(&path).expect_err("garbage should fail to parse");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn save_then_load_round_trips_and_no_temp_file_is_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("state.json");

        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", Some("git@example.com:r1"), ws("w0"));

        state.save(&path).expect("save");
        let loaded = AppState::load(&path).expect("load");
        assert_eq!(loaded, state);

        // Atomic write: parent dir created, and only the final file remains (no leftover .tmp).
        let entries: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name(), "state.json");
    }

    #[test]
    fn save_produces_pretty_printed_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        AppState::default().save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains('\n'), "expected pretty-printed (multi-line) JSON");
    }

    /// A realistic snapshot: 2 repos, local and remote locations, a tab with a split layout and
    /// an agent session, round-tripped through JSON exactly like `state.json` on disk.
    #[test]
    fn round_trip_realistic_state_with_splits_and_agent_sessions() {
        let mut state = AppState::default();

        let mut local_ws = ws("w0");
        local_ws.location = Location::Local { path: PathBuf::from("/home/ada/work/conduit") }; // portability: allow
        local_ws.tabs.push(Tab {
            id: "t0".to_string(),
            name: None,
            layout: Tree::Split {
                axis: Axis::Horizontal,
                ratio: 0.5,
                first: Box::new(Tree::Leaf(1)),
                second: Box::new(Tree::Leaf(2)),
            },
            panes: vec![
                Pane {
                    id: 1,
                    cwd: Some("/home/ada/work/conduit".to_string()), // portability: allow
                    title: Some("claude".to_string()),
                    agent: Some(AgentSession {
                        agent: AgentKind::Claude,
                        session_id: Some("sess-abc".to_string()),
                        pid: Some(4242),
                        cwd: Some("/home/ada/work/conduit".to_string()), // portability: allow
                        argv: vec!["claude".to_string(), "--resume".to_string(), "sess-abc".to_string()],
                        status: Status::Running,
                        hibernated: false,
                        updated: 1_700_000_000,
                    }),
                },
                Pane { id: 2, cwd: None, title: Some("zsh".to_string()), agent: None },
            ],
            focused: 1,
        });
        state.add_workspace("r1", "conduit", "origin", Some("git@example.com:acme/conduit.git"), local_ws);

        let mut remote_ws = ws("w1");
        remote_ws.location = Location::Remote { host: "gpu-box".to_string(), path: "~/amalgum".to_string() };
        state.add_workspace("r2", "amalgum", "origin", Some("https://example.com/acme/amalgum"), remote_ws);

        state.touch_recent(
            Location::Local { path: PathBuf::from("/home/ada/work/conduit") }, // portability: allow
            "conduit / origin",
            1_700_000_100,
        );
        state.touch_recent(
            Location::Remote { host: "gpu-box".to_string(), path: "~/amalgum".to_string() },
            "amalgum / origin",
            1_700_000_200,
        );

        let json = serde_json::to_string_pretty(&state).expect("serialize");
        let back: AppState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);
    }

    // --- new_id ------------------------------------------------------------

    #[test]
    fn new_id_is_prefixed_and_unique_forever_even_after_removal() {
        let mut state = AppState::default();
        let a = state.new_id("w");
        let b = state.new_id("w");
        assert_ne!(a, b);
        assert!(a.starts_with('w'));
        assert!(b.starts_with('w'));

        // Persisted counter survives a save/load round trip and a removal: no id repeats.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        state.add_workspace("r1", "repo", "origin", None, ws(&a));
        state.save(&path).expect("save");
        let mut reloaded = AppState::load(&path).expect("load");
        reloaded.remove_workspace(&a);
        let c = reloaded.new_id("w");
        assert_ne!(c, a);
        assert_ne!(c, b);
    }

    // --- add_workspace / workspace / workspace_mut --------------------------

    #[test]
    fn add_workspace_creates_repo_and_remote_group_when_new() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", Some("url"), ws("w0"));

        assert_eq!(state.repos.len(), 1);
        assert!(!state.repos[0].collapsed);
        assert_eq!(state.repos[0].remotes.len(), 1);
        assert_eq!(state.repos[0].remotes[0].name, "origin");
        assert_eq!(state.repos[0].remotes[0].url.as_deref(), Some("url"));
        assert_eq!(state.workspace("w0").map(|w| w.id.as_str()), Some("w0"));
    }

    #[test]
    fn add_workspace_reuses_existing_repo_and_remote_group() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", Some("url"), ws("w0"));
        state.add_workspace("r1", "conduit", "origin", Some("url"), ws("w1"));

        assert_eq!(state.repos.len(), 1);
        assert_eq!(state.repos[0].remotes.len(), 1);
        assert_eq!(state.repos[0].remotes[0].workspaces.len(), 2);
    }

    #[test]
    fn add_workspace_new_remote_group_within_existing_repo() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", Some("url"), ws("w0"));
        state.add_workspace("r1", "conduit", "upstream", Some("url2"), ws("w1"));

        assert_eq!(state.repos.len(), 1);
        assert_eq!(state.repos[0].remotes.len(), 2);
    }

    #[test]
    fn add_workspace_sets_active_only_when_none() {
        let mut state = AppState::default();
        assert_eq!(state.active, None);
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        assert_eq!(state.active.as_deref(), Some("w0"));
        state.add_workspace("r1", "conduit", "origin", None, ws("w1"));
        // Still the first one: adding more workspaces never steals focus.
        assert_eq!(state.active.as_deref(), Some("w0"));
    }

    #[test]
    fn workspace_mut_allows_editing_in_place() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        state.workspace_mut("w0").expect("exists").name = "renamed".to_string();
        assert_eq!(state.workspace("w0").expect("exists").name, "renamed");
    }

    // --- workspaces (sidebar order) ------------------------------------------

    #[test]
    fn workspaces_are_in_sidebar_order_repos_then_remotes_then_workspaces() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        state.add_workspace("r1", "conduit", "upstream", None, ws("w1"));
        state.add_workspace("r2", "amalgum", "origin", None, ws("w2"));

        let ids: Vec<_> = state.workspaces().iter().map(|w| w.id.clone()).collect();
        assert_eq!(ids, vec!["w0", "w1", "w2"]);
    }

    // --- remove_workspace ----------------------------------------------------

    #[test]
    fn remove_workspace_keeps_empty_group_and_repo() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", Some("url"), ws("w0"));

        let removed = state.remove_workspace("w0").expect("was present");
        assert_eq!(removed.id, "w0");
        assert_eq!(state.repos.len(), 1, "repo group survives with no workspaces");
        assert_eq!(state.repos[0].remotes.len(), 1, "remote group survives empty");
        assert!(state.repos[0].remotes[0].workspaces.is_empty());
    }

    #[test]
    fn remove_workspace_missing_id_is_none() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        assert_eq!(state.remove_workspace("nope"), None);
        assert_eq!(state.repos[0].remotes[0].workspaces.len(), 1);
    }

    #[test]
    fn remove_workspace_active_moves_to_next_in_sidebar_order() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        state.add_workspace("r1", "conduit", "origin", None, ws("w1"));
        state.add_workspace("r1", "conduit", "origin", None, ws("w2"));
        state.active = Some("w1".to_string());

        state.remove_workspace("w1");
        assert_eq!(state.active.as_deref(), Some("w2"));
    }

    #[test]
    fn remove_workspace_active_last_moves_to_previous() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        state.add_workspace("r1", "conduit", "origin", None, ws("w1"));
        state.active = Some("w1".to_string());

        state.remove_workspace("w1");
        assert_eq!(state.active.as_deref(), Some("w0"));
    }

    #[test]
    fn remove_workspace_active_only_one_becomes_none() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        state.active = Some("w0".to_string());

        state.remove_workspace("w0");
        assert_eq!(state.active, None);
    }

    #[test]
    fn remove_workspace_non_active_leaves_active_untouched() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        state.add_workspace("r1", "conduit", "origin", None, ws("w1"));
        state.active = Some("w0".to_string());

        state.remove_workspace("w1");
        assert_eq!(state.active.as_deref(), Some("w0"));
    }

    // --- forget_repo -----------------------------------------------------

    #[test]
    fn forget_repo_removes_repo_and_clears_active_if_inside() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        state.add_workspace("r2", "amalgum", "origin", None, ws("w1"));
        state.active = Some("w0".to_string());

        state.forget_repo("r1");
        assert_eq!(state.repos.len(), 1);
        assert_eq!(state.repos[0].id, "r2");
        assert_eq!(state.active, None);
    }

    #[test]
    fn forget_repo_leaves_active_untouched_when_elsewhere() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        state.add_workspace("r2", "amalgum", "origin", None, ws("w1"));
        state.active = Some("w1".to_string());

        state.forget_repo("r1");
        assert_eq!(state.active.as_deref(), Some("w1"));
    }

    #[test]
    fn forget_repo_unknown_id_is_a_no_op() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        state.forget_repo("nope");
        assert_eq!(state.repos.len(), 1);
    }

    // --- move_workspace ----------------------------------------------------

    #[test]
    fn move_workspace_swaps_within_its_group() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        state.add_workspace("r1", "conduit", "origin", None, ws("w1"));

        assert!(state.move_workspace("w1", -1));
        let ids: Vec<_> = state.repos[0].remotes[0].workspaces.iter().map(|w| w.id.clone()).collect();
        assert_eq!(ids, vec!["w1", "w0"]);
    }

    #[test]
    fn move_workspace_false_at_edges() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        state.add_workspace("r1", "conduit", "origin", None, ws("w1"));

        assert!(!state.move_workspace("w0", -1));
        assert!(!state.move_workspace("w1", 1));
    }

    #[test]
    fn move_workspace_missing_id_is_false() {
        let mut state = AppState::default();
        state.add_workspace("r1", "conduit", "origin", None, ws("w0"));
        assert!(!state.move_workspace("nope", 1));
    }

    // --- touch_recent ------------------------------------------------------

    #[test]
    fn touch_recent_dedupes_by_location_and_moves_to_front() {
        let mut state = AppState::default();
        let loc_a = Location::Local { path: PathBuf::from("/a") }; // portability: allow
        let loc_b = Location::Local { path: PathBuf::from("/b") }; // portability: allow

        state.touch_recent(loc_a.clone(), "a", 1);
        state.touch_recent(loc_b.clone(), "b", 2);
        state.touch_recent(loc_a.clone(), "a again", 3);

        assert_eq!(state.recents.len(), 2);
        assert_eq!(state.recents[0].location, loc_a);
        assert_eq!(state.recents[0].label, "a again");
        assert_eq!(state.recents[1].location, loc_b);
    }

    #[test]
    fn touch_recent_caps_at_recents_cap() {
        let mut state = AppState::default();
        for i in 0..(RECENTS_CAP + 5) {
            state.touch_recent(
                Location::Local { path: PathBuf::from(format!("/p{i}")) }, // portability: allow
                "label",
                i as u64,
            );
        }
        assert_eq!(state.recents.len(), RECENTS_CAP);
        // Most recently touched stays at the front.
        assert_eq!(state.recents[0].location, Location::Local { path: PathBuf::from("/p24") }); // portability: allow
    }

    // --- find_pane / tab_mut -------------------------------------------------

    #[test]
    fn find_pane_locates_pane_across_tabs() {
        let mut state = AppState::default();
        let mut w = ws("w0");
        w.tabs.push(Tab {
            id: "t0".to_string(),
            name: None,
            layout: Tree::Leaf(1),
            panes: vec![Pane { id: 1, cwd: None, title: None, agent: None }],
            focused: 1,
        });
        w.tabs.push(Tab {
            id: "t1".to_string(),
            name: None,
            layout: Tree::Leaf(2),
            panes: vec![Pane { id: 2, cwd: None, title: Some("found me".to_string()), agent: None }],
            focused: 2,
        });
        state.add_workspace("r1", "conduit", "origin", None, w);

        let pane = state.find_pane("w0", 2).expect("pane 2 exists in tab t1");
        assert_eq!(pane.title.as_deref(), Some("found me"));
        assert!(state.find_pane("w0", 99).is_none());
        assert!(state.find_pane("nope", 1).is_none());
    }

    #[test]
    fn tab_mut_allows_editing_a_specific_tab() {
        let mut state = AppState::default();
        let mut w = ws("w0");
        w.tabs.push(Tab {
            id: "t0".to_string(),
            name: None,
            layout: Tree::Leaf(1),
            panes: Vec::new(),
            focused: 1,
        });
        state.add_workspace("r1", "conduit", "origin", None, w);

        state.tab_mut("w0", "t0").expect("exists").name = Some("renamed".to_string());
        assert_eq!(state.workspace("w0").expect("exists").tabs[0].name.as_deref(), Some("renamed"));
        assert!(state.tab_mut("w0", "nope").is_none());
    }
}
