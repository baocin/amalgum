//! Commit message editor rules (§5.7).

/// Subject length state: 72-char ruler, `warning` past 50, `danger` past 72 (chars, not bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectLen {
    Ok,
    Warning,
    Danger,
}

pub fn subject_len(subject: &str) -> SubjectLen {
    match subject.chars().count() {
        0..=50 => SubjectLen::Ok,
        51..=72 => SubjectLen::Warning,
        _ => SubjectLen::Danger,
    }
}

/// Pasting multi-line text into the subject: first line → subject, the rest → body (leading
/// blank lines dropped). CRLF normalised.
pub fn split_paste(text: &str) -> (String, String) {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = normalized.strip_suffix('\n').unwrap_or(&normalized);
    let mut lines = trimmed.split('\n');
    let subject = lines.next().unwrap_or("").to_string();
    let body = lines.skip_while(|l| l.is_empty()).collect::<Vec<_>>().join("\n");
    (subject, body)
}

/// The text given to `git commit -F`: subject, blank line, body (if any), trailing newline.
/// Lines starting with `#` are kept (git's cleanup mode handles them).
pub fn compose(subject: &str, body: &str) -> String {
    let subject = subject.trim_end();
    if body.trim().is_empty() { format!("{subject}\n") } else { format!("{subject}\n\n{body}\n") }
}

/// Commit is allowed when something is staged (or amending) and the subject is not blank.
pub fn can_commit(subject: &str, staged: usize, amend: bool) -> bool {
    (staged > 0 || amend) && !subject.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_len_thresholds() {
        assert_eq!(subject_len(""), SubjectLen::Ok);
        assert_eq!(subject_len(&"a".repeat(50)), SubjectLen::Ok);
        assert_eq!(subject_len(&"a".repeat(51)), SubjectLen::Warning);
        assert_eq!(subject_len(&"a".repeat(72)), SubjectLen::Warning);
        assert_eq!(subject_len(&"a".repeat(73)), SubjectLen::Danger);
    }

    #[test]
    fn subject_len_counts_unicode_chars_not_bytes() {
        // 51 multi-byte chars: over the byte-derived 50 threshold if bytes were counted, but
        // exactly 51 chars, so it must land in Warning, not Ok or something byte-skewed.
        let subject = "é".repeat(51);
        assert!(subject.len() > 51, "sanity: multi-byte chars take more than 1 byte each");
        assert_eq!(subject_len(&subject), SubjectLen::Warning);
    }

    #[test]
    fn split_paste_first_line_is_subject_rest_is_body() {
        let (subject, body) = split_paste("Fix login bug\n\nDetails here.\nMore details.\n");
        assert_eq!(subject, "Fix login bug");
        assert_eq!(body, "Details here.\nMore details.");
    }

    #[test]
    fn split_paste_single_line_has_no_body() {
        let (subject, body) = split_paste("Just a subject");
        assert_eq!(subject, "Just a subject");
        assert_eq!(body, "");
    }

    #[test]
    fn split_paste_normalizes_crlf() {
        let (subject, body) = split_paste("Subject\r\n\r\nBody line\r\n");
        assert_eq!(subject, "Subject");
        assert_eq!(body, "Body line");
    }

    #[test]
    fn split_paste_drops_leading_blank_body_lines_only() {
        let (subject, body) = split_paste("Subject\n\n\nBody\n\nmore\n");
        assert_eq!(subject, "Subject");
        assert_eq!(body, "Body\n\nmore", "only leading blank lines are dropped");
    }

    #[test]
    fn split_paste_trims_exactly_one_trailing_newline() {
        let (subject, body) = split_paste("Subject\n\nBody\n\n\n");
        assert_eq!(subject, "Subject");
        assert_eq!(body, "Body\n\n", "only one trailing newline is trimmed off the whole text");
    }

    #[test]
    fn split_paste_empty_input() {
        assert_eq!(split_paste(""), (String::new(), String::new()));
    }

    #[test]
    fn compose_with_and_without_body() {
        assert_eq!(compose("Fix login bug", "Details here."), "Fix login bug\n\nDetails here.\n");
        assert_eq!(compose("Fix login bug", ""), "Fix login bug\n");
        assert_eq!(compose("Fix login bug", "   "), "Fix login bug\n", "blank body is dropped");
    }

    #[test]
    fn compose_trims_trailing_whitespace_on_subject_only() {
        assert_eq!(compose("Fix login bug   ", "Details.  "), "Fix login bug\n\nDetails.  \n");
    }

    #[test]
    fn compose_keeps_hash_comment_lines() {
        assert_eq!(
            compose("Subject", "# not a comment marker for us"),
            "Subject\n\n# not a comment marker for us\n"
        );
    }

    #[test]
    fn can_commit_rules() {
        assert!(can_commit("Fix bug", 1, false));
        assert!(!can_commit("Fix bug", 0, false), "nothing staged and not amending");
        assert!(can_commit("Fix bug", 0, true), "amending needs no staged files");
        assert!(!can_commit("", 1, false), "blank subject");
        assert!(!can_commit("   ", 1, false), "whitespace-only subject");
    }
}
