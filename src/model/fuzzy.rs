//! Command-palette matching (§5.19). Case-insensitive subsequence match with bonuses for
//! consecutive characters, word starts (after space, `/`, `-`, `_`, `.`, or a lowercase→upper
//! transition), and a prefix match; shorter candidates win ties.

/// Palette result groups, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    Actions,
    Workspaces,
    Branches,
    Tags,
    Stashes,
    Worktrees,
    Locations,
    Commits,
}

/// Split a leading group prefix: `>` actions, `@` branches, `#` commits, `$` stashes,
/// `~` locations, `!` workspaces. Returns the group (if any) and the remaining query, trimmed.
pub fn split_prefix(input: &str) -> (Option<Group>, &str) {
    todo!()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub score: i64,
    /// Char indices of matched characters in the candidate, for highlighting.
    pub positions: Vec<usize>,
}

/// `None` if `query` is not a subsequence of `candidate`. Empty query matches with score 0.
pub fn score(query: &str, candidate: &str) -> Option<Match> {
    todo!()
}

/// Indices of `candidates` that match, best first (stable for equal scores).
pub fn rank(query: &str, candidates: &[&str]) -> Vec<(usize, Match)> {
    todo!()
}

/// The Commits group appears only for 7+ hex chars or a `#` prefix.
pub fn looks_like_hash(query: &str) -> bool {
    todo!()
}
