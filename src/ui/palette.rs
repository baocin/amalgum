//! Command palette (§5.19, W8): centered overlay, fuzzy input (`model::fuzzy`), results grouped
//! Actions, Workspaces, Branches, Tags, Stashes, Worktrees, Recent locations, Commits; group
//! prefixes `> @ # $ ~ !`; ↑/↓/Enter/Esc; shortcuts right-aligned.

use super::theme::Colors;
use crate::git::Location;
use crate::model::fuzzy::Group;
use crate::model::keymap::Action;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Action(Action),
    Workspace(String),
    Branch(String),
    Location(Location),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub group: Group,
    pub label: String,
    /// Right-hand detail: shortcut label, status text, or location.
    pub detail: String,
    pub target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Open,
    Cancelled,
    Chosen(Target),
}

#[derive(Debug, Default)]
pub struct Palette {
    // private: query, selection index, focus-requested flag
}

impl Palette {
    pub fn show(&mut self, ctx: &egui::Context, items: &[Item], colors: &Colors) -> Outcome {
        todo!()
    }
}
