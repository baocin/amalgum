//! Fetch / pull / push glue (§5.10): each workspace's bound remote, the per-frame sync tick of
//! every git pane (background fetch runs for hidden workspaces too), keyboard and palette
//! entry points, and the status bar's sync part.

use super::App;
use crate::git::net::{Force, PullMode};
use crate::model::fuzzy::Group;
use crate::model::keymap::Action;
use crate::model::sync::pull_mode_label;
use crate::ui::chrome::SyncStatus;
use crate::ui::git_pane::SyncRequest;
use crate::ui::palette::{Item, Target};

impl App {
    /// The remote a workspace is bound to (§5.22): the name of the sidebar group it sits in.
    fn bound_remote(&self, workspace: &str) -> Option<String> {
        self.state
            .repos
            .iter()
            .flat_map(|r| r.remotes.iter())
            .find(|g| g.workspaces.iter().any(|w| w.id == workspace))
            .map(|g| g.name.clone())
            .filter(|name| !name.is_empty())
    }

    /// Every frame: let each git pane apply finished fetch/pull/push jobs and start a due
    /// background fetch of its bound remote.
    pub(super) fn tick_sync(&mut self, ctx: &egui::Context, now: u64) {
        let ids: Vec<String> = self.live.keys().cloned().collect();
        let mut events = Vec::new();
        let mut attention = Vec::new();
        for id in ids {
            if self.remote_down(&id) {
                continue; // a remote repo waits for its host (§5.28)
            }
            let bound = self.bound_remote(&id);
            if let Some(git) = self.live.get_mut(&id).and_then(|l| l.git.as_mut()) {
                events.extend(git.sync_tick(ctx, &self.settings, bound.as_deref(), now));
                if let Some(title) = git.take_sync_attention() {
                    attention.push((id, title));
                }
            }
        }
        self.on_git_events(ctx, events);
        // A reply raised an action toast or confirmation (rejected push, stash-then-pull,
        // force-push dialog): it lives in the pane's header, so open the pane; a background
        // workspace also gets a plain toast pointing there.
        for (id, title) in attention {
            let active = self.state.active.as_deref() == Some(id.as_str());
            if let Some(ws) = self.state.workspace_mut(&id)
                && !ws.git_pane_open
            {
                ws.git_pane_open = true;
                self.dirty = true;
            }
            if !active {
                self.toast(crate::ui::chrome::Toast::info(format!("{title} · see its git pane")));
            }
        }
    }

    /// `Mod+Shift+F`, `Mod+Shift+L`, `Mod+P`, and **Force Push with Lease** from the menu.
    pub(super) fn sync_action(&mut self, ctx: &egui::Context, action: Action) {
        let request = match action {
            Action::FetchBoundRemote => SyncRequest::Fetch { remote: None, all: false, prune: true },
            Action::Pull => SyncRequest::Pull { mode: None },
            Action::ForcePushWithLease => {
                SyncRequest::Push { remote: None, branch: None, force: Force::WithLease }
            }
            _ => SyncRequest::Push { remote: None, branch: None, force: Force::No },
        };
        self.sync_request(ctx, request);
    }

    /// Hand `request` to the active workspace's git pane; open the pane when it needs an answer
    /// (an action toast, a confirmation, a popover).
    pub(super) fn sync_request(&mut self, ctx: &egui::Context, request: SyncRequest) {
        let Some(id) = self.state.active.clone() else { return };
        let Some(git) = self.live.get_mut(&id).and_then(|l| l.git.as_mut()) else {
            self.toast(crate::ui::chrome::Toast::info("This workspace has no git repository"));
            return;
        };
        git.sync(ctx, request);
        if git.sync_needs_attention()
            && let Some(ws) = self.state.workspace_mut(&id)
            && !ws.git_pane_open
        {
            ws.git_pane_open = true;
            self.dirty = true;
        }
    }

    /// Palette entries beyond the keymap's Fetch / Pull / Push (§5.19): Fetch all and each
    /// pull mode.
    pub(super) fn sync_palette_items(&self) -> Vec<Item> {
        let has_repo =
            self.state.active.as_deref().and_then(|id| self.live.get(id)).is_some_and(|l| l.git.is_some());
        if !has_repo {
            return Vec::new();
        }
        let fetch_all = Item {
            group: Group::Actions,
            label: "Fetch All Remotes".into(),
            detail: String::new(),
            target: Target::Sync(SyncRequest::Fetch { remote: None, all: true, prune: true }),
        };
        let pulls = [PullMode::Merge, PullMode::Rebase, PullMode::FastForwardOnly].map(|mode| Item {
            group: Group::Actions,
            label: pull_mode_label(mode).into(),
            detail: String::new(),
            target: Target::Sync(SyncRequest::Pull { mode: Some(mode) }),
        });
        std::iter::once(fetch_all).chain(pulls).collect()
    }

    /// "Fetch failed · 3m" clicked: the failure's stderr as a toast.
    pub(super) fn show_fetch_error(&mut self) {
        let toast = self.active_live().and_then(|l| l.git.as_ref()).and_then(|g| g.fetch_error_toast());
        if let Some(toast) = toast {
            self.toast(toast);
        }
    }

    pub(super) fn sync_status(&self, workspace: &str) -> SyncStatus {
        let now = crate::util::unix_now();
        self.live.get(workspace).and_then(|l| l.git.as_ref()).map(|g| g.sync_status(now)).unwrap_or_default()
    }
}
