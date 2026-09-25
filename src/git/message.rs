//! Commit message editor rules (§5.7).

/// Subject length state: 72-char ruler, `warning` past 50, `danger` past 72 (chars, not bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectLen {
    Ok,
    Warning,
    Danger,
}

pub fn subject_len(subject: &str) -> SubjectLen {
    todo!()
}

/// Pasting multi-line text into the subject: first line → subject, the rest → body (leading
/// blank lines dropped). CRLF normalised.
pub fn split_paste(text: &str) -> (String, String) {
    todo!()
}

/// The text given to `git commit -F`: subject, blank line, body (if any), trailing newline.
/// Lines starting with `#` are kept (git's cleanup mode handles them).
pub fn compose(subject: &str, body: &str) -> String {
    todo!()
}

/// Commit is allowed when something is staged (or amending) and the subject is not blank.
pub fn can_commit(subject: &str, staged: usize, amend: bool) -> bool {
    todo!()
}
