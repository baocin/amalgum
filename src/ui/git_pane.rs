//! The git pane (§3, §5.4–5.21) bound to one workspace location: a segmented control with
//! **Graph** (lanes + rows, details and diff of the selected commit below), **Changes** (Unstaged /
//! Staged lists with checkboxes, the commit editor, the diff of the focused file) and **Refs**
//! (branches, remotes, tags, stashes, worktrees).
//!
//! All git runs on worker threads via `ui::jobs` into this pane's own channel; `show` drains it
//! each frame, so the pane is self-contained and never blocks. Local repos refresh from a
//! `notify` watcher on `.git/` and the work tree (debounced 200 ms, §5.25); remote repos refresh
//! on `refresh()` calls and every `Settings::git.remote_refresh_secs` while visible.
//!
//! Ref-changing operations (commit, checkout, …) record a `git::journal` entry, so `undo`/`redo`
//! work across restarts (§5.18).

use super::chrome::Toast;
use super::theme::Colors;
use crate::git::Location;
use crate::git::status::RepoOp;
use crate::model::settings::Settings;
use crate::model::state::GitView;
use std::path::PathBuf;

/// Enough repo state for the status bar and the sidebar row (W2, W3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoSummary {
    pub branch: Option<String>,
    /// Short hash when detached.
    pub detached: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changed: usize,
    pub op: Option<RepoOp>,
    pub conflicts: usize,
    pub loading: bool,
}

/// Things the pane asks the app to do.
#[derive(Debug, Clone, PartialEq)]
pub enum GitEvent {
    Toast(Toast),
    /// Type text into the focused terminal without Enter (§5.6 "Send path:line to terminal").
    SendToTerminal(String),
    /// The pane's summary changed (branch, counts, operation) — re-render sidebar/status bar.
    SummaryChanged,
}

pub struct GitPane {
    // private: location, git runner, journal (+ path), view, graph rows + commits, selection,
    // status, refs, commit editor draft, rx/tx for job results, watcher, last refresh
}

impl GitPane {
    /// Start loading status, refs, and the first 500 commits in the background.
    pub fn new(location: Location, journal_path: PathBuf, ctx: &egui::Context) -> Self {
        todo!()
    }
    pub fn view(&self) -> GitView {
        todo!()
    }
    pub fn set_view(&mut self, view: GitView) {
        todo!()
    }
    /// Re-read status, refs, and the head of the log (after hook events, focus, fetch).
    pub fn refresh(&mut self) {
        todo!()
    }
    pub fn summary(&self) -> RepoSummary {
        todo!()
    }
    /// `Mod+Z` / `Mod+Shift+Z` in the git pane (§5.18). Refusals become toasts.
    pub fn undo(&mut self) {
        todo!()
    }
    pub fn redo(&mut self) {
        todo!()
    }
    /// Draw the pane; returns requests for the app.
    pub fn show(&mut self, ui: &mut egui::Ui, colors: &Colors, settings: &Settings, now: u64) -> Vec<GitEvent> {
        todo!()
    }
}
