//! Refs view data (§5.9–5.12, §5.21): branches and tags, stashes, worktrees, remotes.
//! Each `*_ARGS` constant is the exact git invocation its parser expects.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    Local,
    Remote,
    Tag,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    /// Full name, `refs/heads/feat/x`.
    pub name: String,
    /// Display name: `feat/x`, `origin/main`, `v1.0`.
    pub short: String,
    pub kind: RefKind,
    /// Object the ref points at.
    pub target: String,
    /// Commit an annotated tag peels to.
    pub peeled: Option<String>,
    /// Short upstream name (`origin/main`).
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Upstream configured but deleted on the remote.
    pub gone: bool,
    /// Checked out in this worktree.
    pub is_head: bool,
    pub time: u64,
    pub subject: String,
}

/// `git for-each-ref` with a `%1f`-separated format, one ref per line.
pub const FOR_EACH_REF_ARGS: &[&str] = &[
    "for-each-ref",
    "--format=%(refname)%1f%(objectname)%1f%(*objectname)%1f%(upstream:short)%1f%(upstream:track,nobracket)%1f%(HEAD)%1f%(creatordate:unix)%1f%(contents:subject)",
    "refs/heads",
    "refs/remotes",
    "refs/tags",
];

/// Remote `HEAD` symrefs (`refs/remotes/origin/HEAD`) are skipped.
pub fn parse_refs(out: &str) -> Vec<Ref> {
    todo!()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    pub index: usize,
    pub oid: String,
    pub time: u64,
    /// Message without the `On <branch>: ` / `WIP on <branch>: ` prefix.
    pub message: String,
    pub branch: Option<String>,
}

pub const STASH_ARGS: &[&str] = &["stash", "list", "-z", "--format=%gd%x1f%H%x1f%ct%x1f%gs"];

pub fn parse_stashes(out: &[u8]) -> Vec<Stash> {
    todo!()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: String,
    pub head: Option<String>,
    /// Short branch name; `None` when detached or bare.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: bool,
    pub prunable: bool,
}

pub const WORKTREE_ARGS: &[&str] = &["worktree", "list", "--porcelain", "-z"];

pub fn parse_worktrees(out: &[u8]) -> Vec<Worktree> {
    todo!()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub fetch_url: String,
    pub push_url: String,
}

pub const REMOTE_ARGS: &[&str] = &["remote", "-v"];

/// Parse `git remote -v` (one fetch and one push line per remote), in first-seen order.
pub fn parse_remotes(out: &str) -> Vec<Remote> {
    todo!()
}
