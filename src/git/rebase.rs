//! Interactive rebase without a terminal editor (§5.16). The UI builds a [`Plan`]; the app runs
//! `git rebase -i <base>` with `GIT_SEQUENCE_EDITOR="<exe> --sequence-editor"` and
//! `GIT_EDITOR="<exe> --editor"`, and env `AMALGUM_REBASE_PLAN` / `AMALGUM_REBASE_MSGS` naming
//! JSON files written by [`write_plan_files`]. The helpers run in the CLI (also on remote
//! hosts): `--sequence-editor <todo>` overwrites git's todo file with [`todo_text`];
//! `--editor <msgfile>` overwrites the message file with the next queued message (reword and
//! squash steps, in plan order; a counter file beside the messages file tracks progress). If
//! the queue is exhausted, the editor helper leaves git's message untouched.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
}

impl Action {
    /// Single-key shortcuts from W9: p r e s f x.
    pub fn from_key(c: char) -> Option<Self> {
        todo!()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub action: Action,
    pub hash: String,
    pub subject: String,
    /// New message for `reword` / combined message for `squash`.
    pub message: Option<String>,
}

/// Oldest commit first, as git's todo list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<Step>,
}

impl Plan {
    /// `pick` for every commit, oldest first.
    pub fn from_commits(commits: &[(String, String)]) -> Self {
        todo!()
    }
    /// Move step `from` to index `to` (drag or `Mod+↑/↓`).
    pub fn move_step(&mut self, from: usize, to: usize) {
        todo!()
    }
    /// Plan problems shown before Start: first non-dropped step is squash/fixup (nothing to
    /// meld into), or every step dropped.
    pub fn validate(&self) -> Result<(), String> {
        todo!()
    }
    /// Messages the editor helper will supply, in the order git asks for them.
    pub fn messages(&self) -> Vec<String> {
        todo!()
    }
}

/// git's todo format: `<action> <hash> <subject>` per line.
pub fn todo_text(plan: &Plan) -> String {
    todo!()
}

/// Write `plan.json` and `messages.json` into `dir`; returns their paths.
pub fn write_plan_files(plan: &Plan, dir: &Path) -> io::Result<(std::path::PathBuf, std::path::PathBuf)> {
    todo!()
}

/// `--sequence-editor` helper body.
pub fn apply_sequence_editor(plan_file: &Path, todo_file: &Path) -> io::Result<()> {
    todo!()
}

/// `--editor` helper body.
pub fn apply_editor(messages_file: &Path, message_file: &Path) -> io::Result<()> {
    todo!()
}
