//! Commit records for the graph (§5.4) and the details area (§5.5).
//!
//! The graph streams `git log` output: records are NUL-separated (`-z`), fields `%x1f`-separated,
//! decorations are full ref names (`--decorate=full`) so a local branch `origin/x` is never
//! confused with a remote-tracking branch. Callers split the stream on NUL and feed each record
//! to [`parse_record`] as it arrives.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoration {
    /// Detached `HEAD`.
    Head,
    /// `HEAD -> refs/heads/<name>`: the current branch.
    CurrentBranch(String),
    Branch(String),
    /// `origin/main`.
    Remote(String),
    Tag(String),
    /// Stash ref or anything else, by full name.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    /// Author time, Unix seconds.
    pub time: u64,
    pub committer: String,
    pub committer_email: String,
    pub commit_time: u64,
    pub refs: Vec<Decoration>,
    pub subject: String,
}

/// `%H %P %an %ae %at %cn %ce %ct %D %s`, `%x1f`-separated.
pub const LOG_FORMAT: &str = "%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%cn%x1f%ce%x1f%ct%x1f%D%x1f%s";

/// `git log` argv for the graph: topo order, `-z`, full decorations, [`LOG_FORMAT`], plus
/// `extra` (revision ranges, `--all`, `-n`, paths).
pub fn log_args(extra: &[&str]) -> Vec<String> {
    todo!()
}

/// Parse one record (no trailing NUL). `None` if it is malformed.
pub fn parse_record(rec: &[u8]) -> Option<Commit> {
    todo!()
}

/// Parse a whole `-z` stream.
pub fn parse_log(out: &[u8]) -> Vec<Commit> {
    todo!()
}

/// Full message for the details area: `git show -s --format=%B <id>` output, split into the
/// subject (first line) and body (rest, leading blank lines trimmed).
pub fn split_message(full: &str) -> (String, String) {
    todo!()
}
