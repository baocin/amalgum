//! `git blame -p` (§5.15 "Blame"): argv, the porcelain parser, and the `blame.ignoreRevsFile`
//! lookup. Pure except [`run`], which calls the runner (works local and over ssh).
//!
//! Porcelain output is one group per line: a header `<sha> <orig-line> <final-line>
//! [<group-size>]`, then — only the first time a commit appears — its `author …` / `summary` /
//! `previous` / `boundary` / `filename` lines, then the content line prefixed with a TAB. A
//! commit's metadata is therefore collected once and referenced by index from every line.

use super::cmd::{Git, GitError};

/// The all-zero id git gives lines that are not committed yet (working-tree blame).
pub const UNCOMMITTED: &str = "0000000000000000000000000000000000000000";

/// `git config --get-all --type=path blame.ignoreRevsFile`: the configured ignore lists
/// (`--type=path` expands `~`). Exit 1 means none is set.
pub const IGNORE_REVS_CONFIG_ARGS: &[&str] = &["config", "--get-all", "--type=path", "blame.ignoreRevsFile"];

/// A commit as blame reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlameCommit {
    pub id: String,
    pub author: String,
    pub author_mail: String,
    /// Author time, Unix seconds.
    pub author_time: u64,
    pub summary: String,
    /// The commit is a boundary (the root, or the edge of a shallow clone / range).
    pub boundary: bool,
    /// `previous <sha> <path>`: the parent the line came through and the file's name there —
    /// where "blame back" (§5.15 `Mod+←`) re-blames. `None` when the commit introduced the file.
    pub previous: Option<(String, String)>,
    /// The file's name in this commit (differs from the blamed path across renames).
    pub filename: String,
}

impl BlameCommit {
    /// Lines not committed yet (`0000000…`), from a working-tree blame.
    pub fn is_uncommitted(&self) -> bool {
        self.id == UNCOMMITTED
    }
}

/// One line of the blamed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// Index into [`Blame::commits`].
    pub commit: usize,
    /// Line number in `commit`'s version of the file (1-based).
    pub orig_line: u32,
    /// Line number in the blamed version (1-based).
    pub final_line: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Blame {
    /// Distinct commits, in order of first appearance.
    pub commits: Vec<BlameCommit>,
    pub lines: Vec<BlameLine>,
}

impl Blame {
    pub fn commit_of(&self, line: usize) -> Option<&BlameCommit> {
        self.lines.get(line).and_then(|l| self.commits.get(l.commit))
    }

    /// The longest line in monospace columns after tab expansion (the view's horizontal extent).
    pub fn max_line_chars(&self) -> usize {
        self.lines.iter().map(|l| display_columns(&l.text)).max().unwrap_or(0)
    }
}

/// Columns a tab takes when a blame line is displayed (see [`expand_tabs`]).
pub const TAB_WIDTH: usize = 4;

/// A blame line as displayed: each tab becomes [`TAB_WIDTH`] spaces.
pub fn expand_tabs(text: &str) -> String {
    text.replace('\t', &" ".repeat(TAB_WIDTH))
}

/// Monospace columns `text` takes once displayed: tabs expanded, East Asian wide and emoji
/// characters counted as two.
pub fn display_columns(text: &str) -> usize {
    text.chars()
        .map(|c| match c {
            '\t' => TAB_WIDTH,
            '\u{1100}'..='\u{115F}'
            | '\u{2E80}'..='\u{A4CF}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE30}'..='\u{FE4F}'
            | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}'
            | '\u{1F300}'..='\u{1F64F}'
            | '\u{1F900}'..='\u{1F9FF}'
            | '\u{20000}'..='\u{3FFFD}' => 2,
            _ => 1,
        })
        .sum()
}

/// `git blame -p [--no-ignore-revs-file --ignore-revs-file <f>…] [<rev>] -- <path>`.
///
/// `rev: None` blames the working tree (uncommitted lines come back as [`UNCOMMITTED`]).
/// `ignore_revs` is `None` when `blame.ignoreRevsFile` is not configured (git's defaults stand);
/// otherwise the configured list is cleared (`--no-ignore-revs-file` drops the names read from
/// config) and the configured files that exist — see [`existing_ignore_revs`] — are passed
/// back explicitly: git dies on a configured file that is missing, which is common for a
/// global setting.
pub fn blame_args(rev: Option<&str>, path: &str, ignore_revs: Option<&[String]>) -> Vec<String> {
    let mut args: Vec<String> = vec!["blame".into(), "-p".into()];
    if let Some(files) = ignore_revs {
        args.push("--no-ignore-revs-file".into());
        for file in files {
            args.push("--ignore-revs-file".into());
            args.push(file.clone());
        }
    }
    if let Some(rev) = rev {
        args.push(rev.to_string());
    }
    args.push("--".into());
    args.push(path.to_string());
    args
}

/// `git config --get-all` output → one value per line, blanks dropped.
pub fn parse_config_values(out: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(out);
    let mut values = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            // An empty value resets the list built from earlier config scopes (git's rule).
            values.clear();
        } else {
            values.push(line.to_string());
        }
    }
    values
}

/// A `filename` / `previous` path: git C-quotes names with `"`, `\`, control or (with
/// `core.quotePath`) non-ASCII bytes.
fn unquote(name: &str) -> String {
    match name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
        Some(body) => super::diff::decode_c_quoted(body),
        None => name.to_string(),
    }
}

/// Parse `git blame -p` (or `--line-porcelain`, whose repeated headers are merged) output.
/// Tolerates CRLF content and non-UTF-8 bytes (replaced with U+FFFD).
pub fn parse_porcelain(out: &[u8]) -> Blame {
    let mut blame = Blame::default();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // The commit and line numbers of the group being read, set by each header line.
    let mut current: Option<(usize, u32, u32)> = None;
    for raw in out.split(|&b| b == b'\n') {
        if let Some(content) = raw.strip_prefix(b"\t") {
            if let Some((commit, orig_line, final_line)) = current.take() {
                let text = String::from_utf8_lossy(content);
                let text = text.strip_suffix('\r').unwrap_or(&text).to_string();
                blame.lines.push(BlameLine { commit, orig_line, final_line, text });
            }
            continue;
        }
        let line = String::from_utf8_lossy(raw);
        if line.is_empty() {
            continue;
        }
        if let Some((commit, orig, fin)) = parse_header(&line) {
            let idx = *index.entry(commit.to_string()).or_insert_with(|| {
                blame.commits.push(BlameCommit { id: commit.to_string(), ..BlameCommit::default() });
                blame.commits.len() - 1
            });
            current = Some((idx, orig, fin));
            continue;
        }
        let Some((idx, _, _)) = current else { continue };
        let c = &mut blame.commits[idx];
        let (key, value) = line.split_once(' ').unwrap_or((&line, ""));
        match key {
            "author" => c.author = value.to_string(),
            "author-mail" => {
                c.author_mail = value.trim_start_matches('<').trim_end_matches('>').to_string();
            }
            "author-time" => c.author_time = value.parse().unwrap_or(0),
            "summary" => c.summary = value.to_string(),
            "boundary" => c.boundary = true,
            "filename" => c.filename = unquote(value),
            "previous" => {
                if let Some((sha, name)) = value.split_once(' ') {
                    c.previous = Some((sha.to_string(), unquote(name)));
                }
            }
            _ => {}
        }
    }
    blame
}

/// `<40 or 64 hex> <orig> <final> [<count>]`.
fn parse_header(line: &str) -> Option<(&str, u32, u32)> {
    let mut parts = line.split(' ');
    let sha = parts.next()?;
    if !(sha.len() == 40 || sha.len() == 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let orig = parts.next()?.parse().ok()?;
    let fin = parts.next()?.parse().ok()?;
    match (parts.next(), parts.next()) {
        (None, None) => {}
        (Some(n), None) if n.parse::<u32>().is_ok() => {}
        _ => return None,
    }
    Some((sha, orig, fin))
}

/// The configured `blame.ignoreRevsFile` values that exist (checked with `git hash-object`,
/// which reads the file relative to the same directory blame does — locally or on the host);
/// `None` when none is configured.
pub fn existing_ignore_revs(git: &Git) -> Option<Vec<String>> {
    let configured = parse_config_values(&git.run(IGNORE_REVS_CONFIG_ARGS).ok()?);
    if configured.is_empty() {
        return None;
    }
    Some(configured.into_iter().filter(|file| git.run(&["hash-object", "--", file]).is_ok()).collect())
}

/// Blame `path` at `rev` (`None`: the working tree), honouring `blame.ignoreRevsFile`.
pub fn run(git: &Git, rev: Option<&str>, path: &str) -> Result<Blame, GitError> {
    let ignore = existing_ignore_revs(git);
    let args = blame_args(rev, path, ignore.as_deref());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git.run(&args).map(|out| parse_porcelain(&out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;

    fn git_of(repo: &TempRepo) -> Git {
        Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() })
    }

    fn texts(b: &Blame) -> Vec<&str> {
        b.lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn repeated_commits_share_one_record_and_lines_keep_their_order() {
        let mut repo = TempRepo::new();
        let first = repo.commit_file("a.txt", "one\ntwo\nthree\n", "first");
        let second = repo.commit_file("a.txt", "one\nTWO\nthree\nfour\n", "second");
        let b = parse_porcelain(
            &repo.git_raw(&blame_args(None, "a.txt", None).iter().map(String::as_str).collect::<Vec<_>>()),
        );
        assert_eq!(texts(&b), ["one", "TWO", "three", "four"]);
        assert_eq!(b.commits.len(), 2, "each commit once: {:?}", b.commits);
        let ids: Vec<&str> = b.lines.iter().map(|l| b.commits[l.commit].id.as_str()).collect();
        assert_eq!(ids, [first.as_str(), second.as_str(), first.as_str(), second.as_str()]);
        assert_eq!(b.lines.iter().map(|l| l.final_line).collect::<Vec<_>>(), [1, 2, 3, 4]);
        let c = b.commit_of(1).unwrap();
        assert_eq!((c.author.as_str(), c.author_mail.as_str()), ("Ada Tester", "ada@example.com"));
        assert_eq!(c.summary, "second");
        assert_eq!(c.author_time, 1_700_000_120);
        assert_eq!(c.previous, Some((first.clone(), "a.txt".to_string())));
        let root = b.commit_of(0).unwrap();
        assert!(root.boundary, "the root commit is a boundary without --root");
        assert_eq!(root.previous, None);
    }

    #[test]
    fn uncommitted_lines_come_back_as_zeros_in_a_working_tree_blame() {
        let mut repo = TempRepo::new();
        let head = repo.commit_file("a.txt", "one\n", "first");
        repo.write("a.txt", "one\nlocal edit\n");
        let b = run(&git_of(&repo), None, "a.txt").expect("blame");
        assert_eq!(texts(&b), ["one", "local edit"]);
        let c = b.commit_of(1).unwrap();
        assert!(c.is_uncommitted());
        assert_eq!(c.previous.as_ref().map(|p| p.0.as_str()), Some(head.as_str()), "blame back goes to HEAD");
        assert!(!b.commit_of(0).unwrap().is_uncommitted());
        // At a revision, nothing is uncommitted.
        let at_head = run(&git_of(&repo), Some("HEAD"), "a.txt").expect("blame");
        assert_eq!(texts(&at_head), ["one"]);
    }

    #[test]
    fn filenames_with_spaces_quotes_and_renames() {
        let mut repo = TempRepo::new();
        let first = repo.commit_file("my file.txt", "keep\n", "first");
        repo.git(&["mv", "my file.txt", "say \"hi\" é.txt"]);
        repo.commit("rename");
        repo.write("say \"hi\" é.txt", "keep\nadded\n");
        repo.git(&["add", "-A"]);
        let third = repo.commit("third");
        let b = run(&git_of(&repo), Some(&third), "say \"hi\" é.txt").expect("blame");
        assert_eq!(texts(&b), ["keep", "added"]);
        assert_eq!(b.commit_of(0).unwrap().id, first);
        assert_eq!(b.commit_of(0).unwrap().filename, "my file.txt", "followed across the rename");
        let added = b.commit_of(1).unwrap();
        assert_eq!(added.filename, "say \"hi\" é.txt", "C-quoted name decoded");
        let (_, prev_name) = added.previous.clone().expect("previous");
        assert_eq!(prev_name, "say \"hi\" é.txt");
    }

    #[test]
    fn line_porcelain_repeats_headers_and_parses_the_same() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "x\ny\n", "first");
        let porcelain = parse_porcelain(&repo.git_raw(&["blame", "-p", "--", "a.txt"]));
        let line = parse_porcelain(&repo.git_raw(&["blame", "--line-porcelain", "--", "a.txt"]));
        assert_eq!(porcelain, line);
    }

    #[test]
    fn crlf_content_tabs_and_non_utf8_are_kept() {
        let out = b"1111111111111111111111111111111111111111 1 1 2\nauthor A\nauthor-time 5\nsummary s\nfilename f\n\tone\r\n1111111111111111111111111111111111111111 2 2\n\t\ttabbed \xff\n";
        let b = parse_porcelain(out);
        assert_eq!(b.commits.len(), 1);
        assert_eq!(texts(&b), ["one", "\ttabbed \u{fffd}"]);
        assert_eq!(b.commits[0].author_time, 5);
    }

    #[test]
    fn header_parsing_rejects_non_headers() {
        assert!(parse_header("author 1 2").is_none());
        assert!(parse_header("summary abc").is_none());
        assert!(parse_header(&format!("{UNCOMMITTED} 3 4 1")).is_some());
        assert!(parse_header(&format!("{UNCOMMITTED} 3 4 1 9")).is_none());
        assert!(parse_porcelain(b"").lines.is_empty());
    }

    #[test]
    fn ignore_revs_file_is_honoured_only_when_configured_and_present() {
        let mut repo = TempRepo::new();
        let first = repo.commit_file("a.txt", "fn  main() {}\n", "first");
        let reformat = repo.commit_file("a.txt", "fn main() {}\n", "reformat");
        let git = git_of(&repo);
        assert_eq!(run(&git, None, "a.txt").unwrap().commit_of(0).unwrap().id, reformat);

        // Configured but missing: git itself would die; we skip it and blame normally.
        repo.git(&["config", "blame.ignoreRevsFile", ".git-blame-ignore-revs"]);
        assert_eq!(existing_ignore_revs(&git), Some(vec![]));
        assert_eq!(run(&git, None, "a.txt").unwrap().commit_of(0).unwrap().id, reformat);

        // Present: the reformat commit is skipped.
        repo.write(".git-blame-ignore-revs", &format!("# formatting\n{reformat}\n"));
        assert_eq!(existing_ignore_revs(&git), Some(vec![".git-blame-ignore-revs".to_string()]));
        assert_eq!(run(&git, None, "a.txt").unwrap().commit_of(0).unwrap().id, first);
    }

    #[test]
    fn args_put_the_rev_before_the_path_separator() {
        assert_eq!(
            blame_args(Some("abc"), "-weird", Some(&["ign".into()])),
            ["blame", "-p", "--no-ignore-revs-file", "--ignore-revs-file", "ign", "abc", "--", "-weird"]
        );
        assert_eq!(blame_args(None, "a", None), ["blame", "-p", "--", "a"]);
        assert_eq!(blame_args(None, "a", Some(&[]))[2], "--no-ignore-revs-file", "configured, all missing");
        assert_eq!(parse_config_values(b"a\n b \n"), ["a", "b"]);
        assert_eq!(parse_config_values(b"a\n\n b \n"), ["b"], "an empty value resets the list");
        assert!(parse_config_values(b"a\n\n").is_empty());
    }

    #[test]
    fn display_columns_expand_tabs_and_count_wide_glyphs() {
        assert_eq!(display_columns("\tab"), TAB_WIDTH + 2);
        assert_eq!(display_columns("日本"), 4);
        assert_eq!(expand_tabs("\tx").len(), TAB_WIDTH + 1);
    }

    #[test]
    fn a_missing_path_is_an_error_with_stderr() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "x\n", "first");
        let err = run(&git_of(&repo), Some("HEAD"), "nope.txt").expect_err("no such path");
        assert!(err.stderr.contains("nope.txt"), "{err:?}");
    }
}
