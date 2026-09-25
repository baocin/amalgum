//! `git status --porcelain=v2 -z --branch --show-stash` parsing and in-progress operation
//! detection (§5.7, §5.17).

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Branch {
    /// `None` before the first commit (`# branch.oid (initial)`).
    pub oid: Option<String>,
    /// `None` when detached.
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Ordinary,
    Renamed { from: String },
    Copied { from: String },
    Unmerged,
    Untracked,
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub kind: EntryKind,
    /// Index status (`X`), `.` when unchanged. `?`/`!` for untracked/ignored.
    pub index: char,
    /// Worktree status (`Y`), `.` when unchanged.
    pub worktree: char,
}

impl Entry {
    pub fn is_staged(&self) -> bool {
        todo!()
    }
    /// Unstaged list includes untracked files (§5.7).
    pub fn is_unstaged(&self) -> bool {
        todo!()
    }
    pub fn is_conflicted(&self) -> bool {
        todo!()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: Branch,
    pub entries: Vec<Entry>,
    pub stash_count: u32,
}

impl Status {
    /// "N changed" in the status bar and sidebar: distinct paths, ignored excluded.
    pub fn changed_count(&self) -> usize {
        todo!()
    }
}

pub const STATUS_ARGS: &[&str] =
    &["status", "--porcelain=v2", "-z", "--branch", "--show-stash", "--untracked-files=all"];

/// Parse porcelain v2 `-z` output. Unknown line types are skipped (forward compatible).
pub fn parse(out: &[u8]) -> Result<Status, String> {
    todo!()
}

/// An operation that stopped midway and owns the working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoOp {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl RepoOp {
    /// "Merge", "Rebase", "Cherry-pick", "Revert" — for "Merge in progress · 3 conflicts".
    pub fn name(self) -> &'static str {
        todo!()
    }
    pub fn continue_args(self) -> &'static [&'static str] {
        todo!()
    }
    pub fn abort_args(self) -> &'static [&'static str] {
        todo!()
    }
}

/// Detect an in-progress operation from files in the git dir (`MERGE_HEAD`, `rebase-merge/`,
/// `rebase-apply/`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`). `exists` answers for a path relative
/// to the git dir, so this works locally and over ssh alike.
pub fn detect_op(exists: impl Fn(&str) -> bool) -> Option<RepoOp> {
    todo!()
}
