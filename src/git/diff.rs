//! Diffs (§5.5–5.7): parse `git diff`/`git show` unified output, list changed files, synthesise
//! patches that stage/unstage/discard a hunk or a line range (`git apply --cached [-R]`), and
//! compute word-level intra-line highlights.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
    /// `\ No newline at end of file` — attaches to the line before it.
    NoNewline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    /// Without the leading ` `/`+`/`-` and without the newline.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// Text after the second `@@` (function context), trimmed.
    pub section: String,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// `None` for added files.
    pub old_path: Option<String>,
    /// `None` for deleted files.
    pub new_path: Option<String>,
    /// The `diff --git` header through the `+++` line, verbatim, for patch synthesis.
    pub header: String,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

/// Parse unified diff output (possibly several files). Handles renames, mode changes, binary
/// files, `\ No newline at end of file`, and quoted paths with escapes.
pub fn parse(out: &str) -> Vec<FileDiff> {
    todo!()
}

/// Which lines of one hunk to include, as indices into `Hunk::lines`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    WholeHunk,
    Lines(std::ops::RangeInclusive<usize>),
}

/// Build a patch applying only `sel` of `file.hunks[hunk]`, suitable for
/// `git apply --cached` (stage), `git apply --cached -R` (unstage), or `git apply -R`
/// (discard). Unselected `+` lines are dropped; unselected `-` lines become context; hunk header
/// counts are recomputed. `reverse` means the patch will be applied with `-R` (so unselected
/// `+` lines become context instead). Returns `None` if the selection contains no change.
pub fn patch_for(file: &FileDiff, hunk: usize, sel: &Selection, reverse: bool) -> Option<String> {
    todo!()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub old_path: Option<String>,
    /// `A`, `M`, `D`, `R`, `C`, `T`.
    pub status: char,
    /// `None` for binary files.
    pub added: Option<u32>,
    pub deleted: Option<u32>,
}

/// `git diff-tree`/`git diff` flags producing what [`parse_raw_numstat`] reads.
pub const RAW_NUMSTAT_ARGS: &[&str] = &["--raw", "--numstat", "-z", "-M"];

/// Parse `--raw --numstat -z` output into one entry per file with status and counts.
pub fn parse_raw_numstat(out: &[u8]) -> Vec<FileChange> {
    todo!()
}

/// Word-level highlight ranges (byte ranges, on char boundaries) for a deleted/added line
/// pair: the spans that differ, after tokenising into words, whitespace runs, and punctuation.
pub fn word_diff(old: &str, new: &str) -> (Vec<std::ops::Range<usize>>, Vec<std::ops::Range<usize>>) {
    todo!()
}
