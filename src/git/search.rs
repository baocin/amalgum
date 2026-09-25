//! Commit search (§5.8). Plain words match subject, body, author name, author email, full and
//! short hash, branch and tag names, and changed file paths (case-insensitive). Prefix filters
//! `author: path: before: after: hash: branch: tag: msg:` AND together; quotes make phrases
//! (`msg:"fix login"`). Dates are `YYYY-MM-DD`. An unknown `foo:` prefix is a plain word.

use crate::util;

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

/// Recognised prefix keywords, matched case-insensitively.
const PREFIXES: &[&str] = &["author", "path", "before", "after", "hash", "branch", "tag", "msg"];

/// Splits the input into tokens on whitespace, treating a `"..."` run (optionally preceded by
/// a `prefix:`) as one token that keeps its embedded spaces.
fn tokenize(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let mut tok = String::new();
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '"' {
            tok.push(chars[i]);
            i += 1;
        }
        if i < chars.len() && chars[i] == '"' {
            i += 1; // opening quote
            while i < chars.len() && chars[i] != '"' {
                tok.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                i += 1; // closing quote
            }
        }
        tokens.push(tok);
    }
    tokens
}

/// If `tok` starts with a recognised `prefix:`, returns the lowercased prefix and the value
/// after the colon (which may be empty).
fn split_prefix(tok: &str) -> Option<(&'static str, &str)> {
    let colon = tok.find(':')?;
    let key = &tok[..colon];
    let key_lower = key.to_ascii_lowercase();
    let prefix = PREFIXES.iter().find(|p| **p == key_lower)?;
    Some((prefix, &tok[colon + 1..]))
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

impl Query {
    pub fn parse(input: &str) -> Self {
        let mut q = Query::default();
        for tok in tokenize(input) {
            let Some((prefix, value)) = split_prefix(&tok) else {
                q.words.push(tok);
                continue;
            };
            if value.is_empty() {
                continue; // empty values are ignored entirely
            }
            match prefix {
                "author" => q.author.push(value.to_string()),
                "path" => q.path.push(value.to_string()),
                "hash" => q.hash.push(value.to_string()),
                "branch" => q.branch.push(value.to_string()),
                "tag" => q.tag.push(value.to_string()),
                "msg" => q.msg.push(value.to_string()),
                "before" => match util::parse_date(value) {
                    Some(secs) => q.before = Some(secs),
                    None => q.words.push(tok),
                },
                "after" => match util::parse_date(value) {
                    Some(secs) => q.after = Some(secs),
                    None => q.words.push(tok),
                },
                _ => unreachable!("split_prefix only returns known prefixes"),
            }
        }
        q
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
            && self.author.is_empty()
            && self.path.is_empty()
            && self.hash.is_empty()
            && self.branch.is_empty()
            && self.tag.is_empty()
            && self.msg.is_empty()
            && self.before.is_none()
            && self.after.is_none()
    }

    /// A single plain word must match at least one searchable field.
    fn word_matches(doc: &Doc<'_>, word: &str) -> bool {
        if contains_ci(doc.subject, word) || contains_ci(doc.body, word) {
            return true;
        }
        if contains_ci(doc.author, word) || contains_ci(doc.email, word) {
            return true;
        }
        // A short hash prefix must be at least 4 chars, or nearly every word would match.
        if word.chars().count() >= 4 && doc.id.to_lowercase().starts_with(&word.to_lowercase()) {
            return true;
        }
        doc.branches.iter().any(|b| contains_ci(b, word))
            || doc.tags.iter().any(|t| contains_ci(t, word))
            || doc.paths.iter().any(|p| contains_ci(p, word))
    }

    pub fn matches(&self, doc: &Doc<'_>) -> bool {
        if !self.words.iter().all(|w| Self::word_matches(doc, w)) {
            return false;
        }
        if !self.author.iter().all(|a| contains_ci(doc.author, a) || contains_ci(doc.email, a)) {
            return false;
        }
        if !self.path.iter().all(|p| doc.paths.iter().any(|dp| contains_ci(dp, p))) {
            return false;
        }
        if !self.hash.iter().all(|h| doc.id.to_lowercase().starts_with(&h.to_lowercase())) {
            return false;
        }
        if !self.branch.iter().all(|b| doc.branches.iter().any(|db| contains_ci(db, b))) {
            return false;
        }
        if !self.tag.iter().all(|t| doc.tags.iter().any(|dt| contains_ci(dt, t))) {
            return false;
        }
        if !self.msg.iter().all(|m| contains_ci(doc.subject, m) || contains_ci(doc.body, m)) {
            return false;
        }
        if let Some(before) = self.before
            && doc.time >= before
        {
            return false;
        }
        if let Some(after) = self.after
            && doc.time < after
        {
            return false;
        }
        true
    }

    /// Equivalent `git log` filters for remote workspaces (no local index): `--author`,
    /// `--grep` (with `--regexp-ignore-case --all-match`), `--before/--after`, `-- <paths>`.
    /// Filters git cannot express (hash/branch/tag) are left for [`Query::matches`].
    pub fn git_log_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for a in &self.author {
            args.push(format!("--author={a}"));
        }

        let grep_terms: Vec<&str> = self.words.iter().chain(&self.msg).map(String::as_str).collect();
        if !grep_terms.is_empty() {
            args.push("--regexp-ignore-case".to_string());
            if grep_terms.len() > 1 {
                args.push("--all-match".to_string());
            }
            for term in grep_terms {
                args.push(format!("--grep={term}"));
            }
        }

        if let Some(before) = self.before {
            args.push(format!("--before={}", format_ymd(before)));
        }
        if let Some(after) = self.after {
            args.push(format!("--after={}", format_ymd(after)));
        }

        if !self.path.is_empty() {
            args.push("--".to_string());
            args.extend(self.path.iter().cloned());
        }
        args
    }
}

/// Inverse of `util::parse_date`: Unix seconds (UTC midnight) → `YYYY-MM-DD`.
fn format_ymd(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 → proleptic Gregorian date (Howard Hinnant's algorithm, the inverse of
/// `util::days_from_civil`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1); // [1, 31]
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1); // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_words_and_phrase() {
        let q = Query::parse(r#"fix "two words" login"#);
        assert_eq!(q.words, vec!["fix", "two words", "login"]);
        assert!(q.author.is_empty());
    }

    #[test]
    fn parse_prefix_filters() {
        let q = Query::parse("author:mp path:src/auth");
        assert_eq!(q.author, vec!["mp"]);
        assert_eq!(q.path, vec!["src/auth"]);
    }

    #[test]
    fn parse_prefix_is_case_insensitive() {
        let q = Query::parse("AUTHOR:mp Path:src/auth MsG:oops");
        assert_eq!(q.author, vec!["mp"]);
        assert_eq!(q.path, vec!["src/auth"]);
        assert_eq!(q.msg, vec!["oops"]);
    }

    #[test]
    fn parse_quoted_phrase_after_prefix() {
        let q = Query::parse(r#"msg:"fix login""#);
        assert_eq!(q.msg, vec!["fix login"]);
        assert!(q.words.is_empty());
    }

    #[test]
    fn parse_unknown_prefix_is_a_word() {
        let q = Query::parse("foo:bar");
        assert_eq!(q.words, vec!["foo:bar"]);
        assert!(q.author.is_empty());
    }

    #[test]
    fn parse_empty_value_is_ignored() {
        let q = Query::parse("author: path:src");
        assert!(q.author.is_empty());
        assert_eq!(q.path, vec!["src"]);
    }

    #[test]
    fn parse_dates() {
        let q = Query::parse("before:2024-06-01 after:2024-01-15");
        assert_eq!(q.before, util::parse_date("2024-06-01"));
        assert_eq!(q.after, util::parse_date("2024-01-15"));
    }

    #[test]
    fn parse_invalid_date_is_a_plain_word() {
        let q = Query::parse("before:not-a-date");
        assert!(q.before.is_none());
        assert_eq!(q.words, vec!["before:not-a-date"]);
    }

    #[test]
    fn is_empty_reflects_all_fields() {
        assert!(Query::parse("").is_empty());
        assert!(Query::parse("   ").is_empty());
        assert!(!Query::parse("word").is_empty());
        assert!(!Query::parse("before:2024-01-01").is_empty());
    }

    #[test]
    fn matches_word_across_fields() {
        let branches = vec!["feat/login".to_string()];
        let tags = vec!["v1.0".to_string()];
        let paths = vec!["src/auth/login.rs".to_string()];
        let d = Doc {
            id: "ab12cd34ef",
            subject: "Fix login timeout",
            body: "Detailed body",
            author: "Mary Poppins",
            email: "mary@example.com",
            time: 1000,
            branches: &branches,
            tags: &tags,
            paths: &paths,
        };
        assert!(Query::parse("login").matches(&d), "subject word");
        assert!(Query::parse("Detailed").matches(&d), "body word, case-insensitive");
        assert!(Query::parse("mary").matches(&d), "author name word");
        assert!(Query::parse("example.com").matches(&d), "author email word");
        assert!(Query::parse("ab12").matches(&d), "short hash prefix word (>=4 chars)");
        assert!(!Query::parse("ab1").matches(&d), "hash prefix word too short (<4 chars)");
        assert!(Query::parse("feat").matches(&d), "branch word");
        assert!(Query::parse("v1.0").matches(&d), "tag word");
        assert!(Query::parse("auth/login").matches(&d), "path word");
        assert!(!Query::parse("nomatch").matches(&d));
    }

    #[test]
    fn matches_filters_and_together() {
        let branches = vec!["feat/login".to_string()];
        let tags: Vec<String> = vec![];
        let paths = vec!["src/auth/login.rs".to_string(), "README.md".to_string()];
        let d = Doc {
            id: "ab12cd34ef",
            subject: "Fix login timeout",
            body: "",
            author: "Mary Poppins",
            email: "mary@example.com",
            time: util::parse_date("2024-06-15").unwrap(),
            branches: &branches,
            tags: &tags,
            paths: &paths,
        };

        assert!(Query::parse("author:mp path:src/auth").matches(&d), "W7 example");
        assert!(!Query::parse("author:mp path:nomatch").matches(&d), "one filter fails => AND fails");
        assert!(Query::parse("hash:ab12").matches(&d));
        assert!(!Query::parse("hash:zz").matches(&d));
        assert!(Query::parse("branch:login").matches(&d));
        assert!(Query::parse("msg:timeout").matches(&d));
        assert!(Query::parse("author:MARY").matches(&d), "author matches case-insensitively");
        assert!(Query::parse("author:example.com").matches(&d), "author filter also checks email");
    }

    #[test]
    fn matches_before_after_bounds() {
        let branches: Vec<String> = vec![];
        let tags: Vec<String> = vec![];
        let paths: Vec<String> = vec![];
        let d = Doc {
            id: "abc",
            subject: "s",
            body: "",
            author: "a",
            email: "a@e",
            time: util::parse_date("2024-06-15").unwrap(),
            branches: &branches,
            tags: &tags,
            paths: &paths,
        };
        assert!(Query::parse("after:2024-06-15").matches(&d), "on the day passes after");
        assert!(!Query::parse("before:2024-06-15").matches(&d), "same day fails strict before");
        assert!(Query::parse("before:2024-06-16").matches(&d), "next day passes before");
        assert!(!Query::parse("after:2024-06-16").matches(&d));
    }

    #[test]
    fn git_log_args_basic() {
        let q = Query::parse("author:mp path:src/auth");
        assert_eq!(q.git_log_args(), vec!["--author=mp", "--", "src/auth"]);
    }

    #[test]
    fn git_log_args_grep_words_and_msg() {
        let q = Query::parse(r#"fix msg:"login bug""#);
        assert_eq!(
            q.git_log_args(),
            vec!["--regexp-ignore-case", "--all-match", "--grep=fix", "--grep=login bug"]
        );
    }

    #[test]
    fn git_log_args_single_grep_has_no_all_match() {
        let q = Query::parse("fix");
        assert_eq!(q.git_log_args(), vec!["--regexp-ignore-case", "--grep=fix"]);
    }

    #[test]
    fn git_log_args_dates_and_paths_last() {
        let q = Query::parse("before:2024-06-01 after:2024-01-15 path:src");
        assert_eq!(q.git_log_args(), vec!["--before=2024-06-01", "--after=2024-01-15", "--", "src"]);
    }

    #[test]
    fn git_log_args_omits_hash_branch_tag() {
        // These filters cannot be expressed by `git log`; left entirely to `Query::matches`.
        let q = Query::parse("hash:ab12 branch:feat tag:v1");
        assert!(q.git_log_args().is_empty());
    }

    #[test]
    fn format_ymd_round_trips_parse_date() {
        for date in ["1970-01-01", "2000-03-01", "2024-06-15", "2024-12-31", "1999-02-28"] {
            let secs = util::parse_date(date).unwrap();
            assert_eq!(format_ymd(secs), date);
        }
    }
}
