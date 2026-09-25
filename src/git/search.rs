//! Commit search (§5.8). Plain words match subject, body, author name/email, full and short
//! hash, branch and tag names, and changed paths (case-insensitive). Prefix filters
//! `author: path: before: after: hash: branch: tag: msg:` AND together; quotes make phrases
//! (`msg:"fix login"`). Dates are `YYYY-MM-DD`. An unknown `foo:` prefix is a plain word.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub words: Vec<String>,
    pub author: Vec<String>,
    pub path: Vec<String>,
    pub hash: Vec<String>,
    pub branch: Vec<String>,
    pub tag: Vec<String>,
    pub msg: Vec<String>,
    /// Unix seconds, exclusive upper bound (start of that day).
    pub before: Option<u64>,
    /// Unix seconds, inclusive lower bound.
    pub after: Option<u64>,
}

/// What a commit exposes to search.
#[derive(Debug, Clone, Default)]
pub struct Doc<'a> {
    pub id: &'a str,
    pub subject: &'a str,
    pub body: &'a str,
    pub author: &'a str,
    pub email: &'a str,
    pub time: u64,
    pub branches: &'a [String],
    pub tags: &'a [String],
    pub paths: &'a [String],
}

impl Query {
    pub fn parse(input: &str) -> Self {
        todo!()
    }
    pub fn is_empty(&self) -> bool {
        todo!()
    }
    pub fn matches(&self, doc: &Doc<'_>) -> bool {
        todo!()
    }
    /// Equivalent `git log` filters for remote workspaces (no local index): `--author`,
    /// `--grep` (with `--regexp-ignore-case --all-match`), `--before/--after`, `-- <paths>`.
    /// Filters git cannot express (hash/branch/tag) are left for [`Query::matches`].
    pub fn git_log_args(&self) -> Vec<String> {
        todo!()
    }
}
