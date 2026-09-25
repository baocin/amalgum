//! Diffs (§5.5–5.7): parse `git diff`/`git show` unified output, list changed files, synthesise
//! patches that stage/unstage/discard a hunk or a line range (`git apply --cached [-R]`), and
//! compute word-level intra-line highlights.

use std::collections::HashMap;
use std::ops::Range;

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

/// Decode a git C-quoted path body (the bytes between the surrounding `"` `"`, which are always
/// pure ASCII: non-ASCII bytes are escaped as `\NNN` octal by git before quoting).
fn decode_c_quoted(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' || i + 1 >= bytes.len() {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes[i + 1] {
            b'n' => {
                out.push(b'\n');
                i += 2;
            }
            b't' => {
                out.push(b'\t');
                i += 2;
            }
            b'r' => {
                out.push(b'\r');
                i += 2;
            }
            b'a' => {
                out.push(0x07);
                i += 2;
            }
            b'b' => {
                out.push(0x08);
                i += 2;
            }
            b'f' => {
                out.push(0x0c);
                i += 2;
            }
            b'v' => {
                out.push(0x0b);
                i += 2;
            }
            b'\\' => {
                out.push(b'\\');
                i += 2;
            }
            b'"' => {
                out.push(b'"');
                i += 2;
            }
            b'0'..=b'7' => {
                let mut val = 0u32;
                let mut n = 0;
                let mut j = i + 1;
                while n < 3 && j < bytes.len() && (b'0'..=b'7').contains(&bytes[j]) {
                    val = val * 8 + (bytes[j] - b'0') as u32;
                    j += 1;
                    n += 1;
                }
                out.push(val as u8);
                i = j;
            }
            other => {
                out.push(other);
                i += 2;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Strip surrounding `"…"` and decode C-style escapes if quoted; otherwise the path verbatim.
fn dequote_path(raw: &str) -> String {
    let raw = raw.trim();
    match raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => decode_c_quoted(inner),
        None => raw.to_string(),
    }
}

/// Parse a `--- `/`+++ `/binary-line path field with its `a/`/`b/` prefix: `/dev/null` → `None`,
/// otherwise the dequoted path with `prefix` stripped if present.
fn parse_prefixed_path(raw: &str, prefix: &str) -> Option<String> {
    let raw = raw.trim();
    if raw == "/dev/null" {
        return None;
    }
    let decoded = dequote_path(raw);
    Some(decoded.strip_prefix(prefix).map(str::to_string).unwrap_or(decoded))
}

/// Consume a leading `"…"` (with `\"` escapes) from `s`, returning the inner body (still
/// escaped) and the remainder after the closing quote.
fn take_quoted(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut i = 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'"' {
            return Some((&s[1..i], &s[i + 1..]));
        }
        i += 1;
    }
    None
}

/// Best-effort path pair from `diff --git a/X b/Y`, used as a fallback for diffs that carry no
/// other path line (mode-only changes). For quoted names the whole `"a/X" "b/Y"` pair is
/// authoritative; for unquoted names, `X`/`Y` are found by the only split of `X b/Y` where the
/// two halves are equal (true whenever the path is not itself being renamed).
fn split_diff_git_line(l: &str) -> Option<(String, String)> {
    let rest = l.strip_prefix("diff --git ")?;
    if rest.starts_with('"') {
        let (a, after) = take_quoted(rest)?;
        let after = after.trim_start();
        let b = if after.starts_with('"') { take_quoted(after)?.0 } else { after };
        let a = decode_c_quoted(a);
        let b = decode_c_quoted(b);
        let a = a.strip_prefix("a/").map(str::to_string).unwrap_or(a);
        let b = b.strip_prefix("b/").map(str::to_string).unwrap_or(b);
        return Some((a, b));
    }
    let no_a = rest.strip_prefix("a/")?;
    let mut from = 0;
    while let Some(pos) = no_a[from..].find(" b/") {
        let abs = from + pos;
        let (left, right) = (&no_a[..abs], &no_a[abs + 3..]);
        if left == right {
            return Some((left.to_string(), right.to_string()));
        }
        from = abs + 1;
    }
    None
}

/// `Binary files A and B differ` → the two (optionally quoted, optionally `/dev/null`) paths.
fn parse_binary_paths(l: &str) -> Option<(Option<String>, Option<String>)> {
    let rest = l.strip_prefix("Binary files ")?;
    let rest = rest.strip_suffix(" differ")?;
    let (a_raw, b_raw): (String, String) = if rest.starts_with('"') {
        let (a, after) = take_quoted(rest)?;
        let after = after.strip_prefix(" and ")?;
        (format!("\"{a}\""), after.to_string())
    } else {
        let (a, b) = rest.split_once(" and ")?;
        (a.to_string(), b.to_string())
    };
    Some((parse_prefixed_path(&a_raw, "a/"), parse_prefixed_path(&b_raw, "b/")))
}

/// `-a,b` / `+c,d` → `(start, len)`; a missing `,len` means length 1.
fn parse_range(s: &str) -> Option<(u32, u32)> {
    let s = &s[1..];
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// `@@ -a,b +c,d @@ section` → `(old_start, old_len, new_start, new_len, section)`.
fn parse_hunk_header(l: &str) -> Option<(u32, u32, u32, u32, String)> {
    let after = l.strip_prefix("@@ ")?;
    let close = after.find(" @@")?;
    let ranges = &after[..close];
    let section = after[close + 3..].trim_start().to_string();
    let (old, new) = ranges.split_once(' ')?;
    let (old_start, old_len) = parse_range(old)?;
    let (new_start, new_len) = parse_range(new)?;
    Some((old_start, old_len, new_start, new_len, section))
}

/// Parse unified diff output (possibly several files). Handles renames, mode changes, binary
/// files, `\ No newline at end of file`, and quoted paths with escapes.
pub fn parse(out: &str) -> Vec<FileDiff> {
    let lines: Vec<&str> = out.lines().collect();
    let mut files = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].starts_with("diff --git ") {
            i += 1;
            continue;
        }
        let start = i;
        let (mut old_path, mut new_path) = match split_diff_git_line(lines[i]) {
            Some((a, b)) => (Some(a), Some(b)),
            None => (None, None),
        };
        let mut binary = false;
        i += 1;
        while i < lines.len() && !lines[i].starts_with("@@ ") && !lines[i].starts_with("diff --git ") {
            let l = lines[i];
            if let Some(rest) = l.strip_prefix("rename from ") {
                old_path = Some(dequote_path(rest));
            } else if let Some(rest) = l.strip_prefix("rename to ") {
                new_path = Some(dequote_path(rest));
            } else if let Some(rest) = l.strip_prefix("copy from ") {
                old_path = Some(dequote_path(rest));
            } else if let Some(rest) = l.strip_prefix("copy to ") {
                new_path = Some(dequote_path(rest));
            } else if l.starts_with("new file mode ") {
                old_path = None;
            } else if l.starts_with("deleted file mode ") {
                new_path = None;
            } else if let Some(rest) = l.strip_prefix("--- ") {
                old_path = parse_prefixed_path(rest, "a/");
            } else if let Some(rest) = l.strip_prefix("+++ ") {
                new_path = parse_prefixed_path(rest, "b/");
            } else if l.starts_with("Binary files ") && l.ends_with(" differ") {
                binary = true;
                if let Some((a, b)) = parse_binary_paths(l) {
                    old_path = a;
                    new_path = b;
                }
            }
            i += 1;
        }
        let header_end = i;
        let header = lines[start..header_end].join("\n") + "\n";

        let mut hunks = Vec::new();
        while i < lines.len() && lines[i].starts_with("@@ ") {
            let Some((old_start, old_len, new_start, new_len, section)) = parse_hunk_header(lines[i]) else {
                break;
            };
            i += 1;
            let mut hunk_lines = Vec::new();
            let mut old_no = old_start;
            let mut new_no = new_start;
            while i < lines.len() && !lines[i].starts_with("@@ ") && !lines[i].starts_with("diff --git ") {
                let l = lines[i];
                if l == "\\ No newline at end of file" {
                    hunk_lines.push(Line {
                        kind: LineKind::NoNewline,
                        old_no: None,
                        new_no: None,
                        text: String::new(),
                    });
                } else if let Some(t) = l.strip_prefix('+') {
                    hunk_lines.push(Line {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(new_no),
                        text: t.to_string(),
                    });
                    new_no += 1;
                } else if let Some(t) = l.strip_prefix('-') {
                    hunk_lines.push(Line {
                        kind: LineKind::Del,
                        old_no: Some(old_no),
                        new_no: None,
                        text: t.to_string(),
                    });
                    old_no += 1;
                } else if let Some(t) = l.strip_prefix(' ') {
                    hunk_lines.push(Line {
                        kind: LineKind::Context,
                        old_no: Some(old_no),
                        new_no: Some(new_no),
                        text: t.to_string(),
                    });
                    old_no += 1;
                    new_no += 1;
                } else if l.is_empty() {
                    hunk_lines.push(Line {
                        kind: LineKind::Context,
                        old_no: Some(old_no),
                        new_no: Some(new_no),
                        text: String::new(),
                    });
                    old_no += 1;
                    new_no += 1;
                } else {
                    break;
                }
                i += 1;
            }
            hunks.push(Hunk { old_start, old_len, new_start, new_len, section, lines: hunk_lines });
        }

        files.push(FileDiff { old_path, new_path, header, binary, hunks });
    }
    files
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
    let h = file.hunks.get(hunk)?;
    let selected = |k: usize| match sel {
        Selection::WholeHunk => true,
        Selection::Lines(r) => r.contains(&k),
    };
    let has_change =
        (0..h.lines.len()).any(|k| selected(k) && matches!(h.lines[k].kind, LineKind::Add | LineKind::Del));
    if !has_change {
        return None;
    }

    let mut old_len = 0u32;
    let mut new_len = 0u32;
    let mut body = String::new();
    let mut prev_emitted = false;
    for (k, line) in h.lines.iter().enumerate() {
        match line.kind {
            LineKind::NoNewline => {
                if prev_emitted {
                    body.push_str("\\ No newline at end of file\n");
                }
            }
            LineKind::Context => {
                body.push(' ');
                body.push_str(&line.text);
                body.push('\n');
                old_len += 1;
                new_len += 1;
                prev_emitted = true;
            }
            LineKind::Add => {
                let keep_sign = selected(k);
                if keep_sign {
                    body.push('+');
                    body.push_str(&line.text);
                    body.push('\n');
                    new_len += 1;
                    prev_emitted = true;
                } else if reverse {
                    // Unselected `+` becomes context: the `-R` apply must leave it untouched.
                    body.push(' ');
                    body.push_str(&line.text);
                    body.push('\n');
                    old_len += 1;
                    new_len += 1;
                    prev_emitted = true;
                } else {
                    prev_emitted = false;
                }
            }
            LineKind::Del => {
                let keep_sign = selected(k);
                if keep_sign {
                    body.push('-');
                    body.push_str(&line.text);
                    body.push('\n');
                    old_len += 1;
                    prev_emitted = true;
                } else if !reverse {
                    // Unselected `-` becomes context: the forward apply must leave it untouched.
                    body.push(' ');
                    body.push_str(&line.text);
                    body.push('\n');
                    old_len += 1;
                    new_len += 1;
                    prev_emitted = true;
                } else {
                    prev_emitted = false;
                }
            }
        }
    }

    // Isolated single-hunk patch: nothing before this hunk changed, so new_start == old_start.
    // git apply locates the hunk by context, tolerating the (possibly stale) offset.
    let mut patch = file.header.clone();
    patch.push_str(&format!("@@ -{},{} +{},{} @@", h.old_start, old_len, h.old_start, new_len));
    if !h.section.is_empty() {
        patch.push(' ');
        patch.push_str(&h.section);
    }
    patch.push('\n');
    patch.push_str(&body);
    Some(patch)
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

/// Split on NUL, dropping empty tokens (a trailing terminator produces one).
fn split_nul(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split(|&b| b == 0).filter(|s| !s.is_empty()).collect()
}

struct RawEntry {
    status: char,
    old_path: Option<String>,
}

/// Parse `--raw --numstat -z` output into one entry per file with status and counts.
pub fn parse_raw_numstat(out: &[u8]) -> Vec<FileChange> {
    let tokens = split_nul(out);
    let mut i = 0;
    let mut raw: HashMap<String, RawEntry> = HashMap::new();
    // Raw block: `:oldmode newmode oldsha newsha STATUS` then one path, or two for R/C.
    while i < tokens.len() {
        let Ok(t) = std::str::from_utf8(tokens[i]) else {
            i += 1;
            continue;
        };
        if !t.starts_with(':') {
            if t.contains('\t') {
                break; // numstat block starts here
            }
            i += 1; // commit hash line (`--root`) or other noise
            continue;
        }
        let status = t[1..].split(' ').nth(4).and_then(|f| f.chars().next()).unwrap_or('M');
        i += 1;
        if status == 'R' || status == 'C' {
            let (Some(old), Some(new)) = (tokens.get(i), tokens.get(i + 1)) else { break };
            let path = String::from_utf8_lossy(new).into_owned();
            let old_path = String::from_utf8_lossy(old).into_owned();
            raw.insert(path, RawEntry { status, old_path: Some(old_path) });
            i += 2;
        } else {
            let Some(path) = tokens.get(i) else { break };
            let path = String::from_utf8_lossy(path).into_owned();
            raw.insert(path, RawEntry { status, old_path: None });
            i += 1;
        }
    }

    // Numstat block: `added\tdeleted\tpath`, or `added\tdeleted\t` + two paths for renames.
    let mut out = Vec::new();
    while i < tokens.len() {
        let Ok(t) = std::str::from_utf8(tokens[i]) else {
            i += 1;
            continue;
        };
        i += 1;
        if !t.contains('\t') {
            continue;
        }
        let mut parts = t.splitn(3, '\t');
        let added = parts.next().unwrap_or("");
        let deleted = parts.next().unwrap_or("");
        let path_field = parts.next().unwrap_or("");
        let path = if path_field.is_empty() {
            let (Some(_old), Some(new)) = (tokens.get(i), tokens.get(i + 1)) else { break };
            let new = String::from_utf8_lossy(new).into_owned();
            i += 2;
            new
        } else {
            path_field.to_string()
        };
        let (added, deleted) = match (added.parse::<u32>().ok(), deleted.parse::<u32>().ok()) {
            (Some(a), Some(d)) => (Some(a), Some(d)),
            _ => (None, None),
        };
        let entry = raw.get(&path);
        out.push(FileChange {
            old_path: entry.and_then(|e| e.old_path.clone()),
            status: entry.map(|e| e.status).unwrap_or('M'),
            path,
            added,
            deleted,
        });
    }
    out
}

/// A run of word chars, a run of whitespace, or a single punctuation char.
fn tokenize(s: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        if is_word(c) || c.is_whitespace() {
            let mut end = start + c.len_utf8();
            chars.next();
            while let Some(&(idx, c2)) = chars.peek() {
                if is_word(c2) == is_word(c) && c2.is_whitespace() == c.is_whitespace() {
                    end = idx + c2.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            ranges.push(start..end);
        } else {
            chars.next();
            ranges.push(start..start + c.len_utf8());
        }
    }
    ranges
}

/// Merge consecutive changed tokens (they cover the string contiguously) into ranges.
fn coalesce(ranges: &[Range<usize>], changed: &[bool]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut cur: Option<Range<usize>> = None;
    for (r, &is_changed) in ranges.iter().zip(changed) {
        if is_changed {
            match &mut cur {
                Some(c) => c.end = r.end,
                None => cur = Some(r.clone()),
            }
        } else if let Some(c) = cur.take() {
            out.push(c);
        }
    }
    if let Some(c) = cur {
        out.push(c);
    }
    out
}

/// Word-level highlight ranges (byte ranges, on char boundaries) for a deleted/added line
/// pair: the spans that differ, after tokenising into words, whitespace runs, and punctuation.
pub fn word_diff(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if old == new {
        return (Vec::new(), Vec::new());
    }
    let old_ranges = tokenize(old);
    let new_ranges = tokenize(new);
    if old_ranges.len() > 500 || new_ranges.len() > 500 {
        // Not `vec![range]`: that reads to clippy as a range-of-indices literal, not our
        // intended single-element `Vec<Range<usize>>`.
        let whole = |s: &str| -> Vec<Range<usize>> {
            let mut v = Vec::new();
            if !s.is_empty() {
                v.push(0..s.len());
            }
            v
        };
        return (whole(old), whole(new));
    }
    let old_tok: Vec<&str> = old_ranges.iter().map(|r| &old[r.clone()]).collect();
    let new_tok: Vec<&str> = new_ranges.iter().map(|r| &new[r.clone()]).collect();
    let n = old_tok.len();
    let m = new_tok.len();

    // LCS table, computed backwards so a forward scan can greedily follow the longest path.
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] =
                if old_tok[i] == new_tok[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
        }
    }

    let mut old_changed = vec![false; n];
    let mut new_changed = vec![false; m];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old_tok[i] == new_tok[j] {
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            old_changed[i] = true;
            i += 1;
        } else {
            new_changed[j] = true;
            j += 1;
        }
    }
    old_changed[i..].fill(true);
    new_changed[j..].fill(true);

    (coalesce(&old_ranges, &old_changed), coalesce(&new_ranges, &new_changed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempRepo, hermetic_git};
    use std::io::Write;
    use std::process::Stdio;

    // ---- parse() -----------------------------------------------------------------------

    #[test]
    fn parse_simple_modification() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "one\ntwo\nthree\n", "init");
        repo.write("a.txt", "one\nCHANGED\nthree\n");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.old_path.as_deref(), Some("a.txt"));
        assert_eq!(f.new_path.as_deref(), Some("a.txt"));
        assert!(!f.binary);
        assert!(f.header.starts_with("diff --git a/a.txt b/a.txt\n"));
        assert!(f.header.ends_with("+++ b/a.txt\n"));
        assert_eq!(f.hunks.len(), 1);
        let h = &f.hunks[0];
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (1, 3, 1, 3));
        assert_eq!(h.lines.len(), 4);
        assert_eq!(
            h.lines[0],
            Line { kind: LineKind::Context, old_no: Some(1), new_no: Some(1), text: "one".into() }
        );
        assert_eq!(
            h.lines[1],
            Line { kind: LineKind::Del, old_no: Some(2), new_no: None, text: "two".into() }
        );
        assert_eq!(
            h.lines[2],
            Line { kind: LineKind::Add, old_no: None, new_no: Some(2), text: "CHANGED".into() }
        );
        assert_eq!(
            h.lines[3],
            Line { kind: LineKind::Context, old_no: Some(3), new_no: Some(3), text: "three".into() }
        );
    }

    #[test]
    fn parse_added_file() {
        let mut repo = TempRepo::new();
        repo.commit_file("keep.txt", "x\n", "init");
        repo.write("new.txt", "hello\nworld\n");
        repo.git(&["add", "-A"]);
        let out = repo.git(&["diff", "--cached", "--", "new.txt"]);
        let files = parse(&out);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.old_path, None);
        assert_eq!(f.new_path.as_deref(), Some("new.txt"));
        assert_eq!(f.hunks[0].old_start, 0);
        assert_eq!(f.hunks[0].old_len, 0);
        assert!(f.hunks[0].lines.iter().all(|l| l.kind == LineKind::Add));
    }

    #[test]
    fn parse_deleted_file() {
        let mut repo = TempRepo::new();
        repo.commit_file("gone.txt", "bye\n", "init");
        std::fs::remove_file(repo.path().join("gone.txt")).expect("rm");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path.as_deref(), Some("gone.txt"));
        assert_eq!(files[0].new_path, None);
        assert!(files[0].hunks[0].lines.iter().all(|l| l.kind == LineKind::Del));
    }

    #[test]
    fn parse_rename_with_edit() {
        let mut repo = TempRepo::new();
        repo.commit_file("orig.txt", &(1..=20).map(|n| format!("{n}\n")).collect::<String>(), "init");
        repo.git(&["mv", "orig.txt", "renamed.txt"]);
        repo.write(
            "renamed.txt",
            &(1..=20)
                .map(|n| if n == 5 { "EDITED\n".to_string() } else { format!("{n}\n") })
                .collect::<String>(),
        );
        repo.git(&["add", "-A"]);
        let out = repo.git(&["diff", "--cached", "-M"]);
        let files = parse(&out);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path.as_deref(), Some("orig.txt"));
        assert_eq!(files[0].new_path.as_deref(), Some("renamed.txt"));
        assert_eq!(files[0].hunks.len(), 1);
        assert!(files[0].hunks[0].lines.iter().any(|l| l.kind == LineKind::Del && l.text == "5"));
        assert!(files[0].hunks[0].lines.iter().any(|l| l.kind == LineKind::Add && l.text == "EDITED"));
    }

    #[test]
    fn parse_mode_change_only() {
        let mut repo = TempRepo::new();
        let p = repo.commit_file("script.sh", "echo hi\n", "init");
        let _ = p;
        let path = repo.path().join("script.sh");
        let mut perm = std::fs::metadata(&path).expect("meta").permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perm.set_mode(0o755);
        }
        std::fs::set_permissions(&path, perm).expect("chmod");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path.as_deref(), Some("script.sh"));
        assert_eq!(files[0].new_path.as_deref(), Some("script.sh"));
        assert!(files[0].hunks.is_empty());
        assert!(!files[0].binary);
        assert!(files[0].header.contains("old mode 100644"));
        assert!(files[0].header.contains("new mode 100755"));
    }

    #[test]
    fn parse_binary_file() {
        let mut repo = TempRepo::new();
        repo.write("img.png", "");
        std::fs::write(repo.path().join("img.png"), [0x89, b'P', b'N', b'G', 0, 1, 2]).expect("write");
        repo.git(&["add", "-A"]);
        repo.commit("init");
        std::fs::write(repo.path().join("img.png"), [0x89, b'P', b'N', b'G', 9, 9, 9]).expect("write");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        assert_eq!(files.len(), 1);
        assert!(files[0].binary);
        assert!(files[0].hunks.is_empty());
        assert_eq!(files[0].old_path.as_deref(), Some("img.png"));
        assert_eq!(files[0].new_path.as_deref(), Some("img.png"));
    }

    #[test]
    fn parse_no_trailing_newline() {
        let mut repo = TempRepo::new();
        std::fs::write(repo.path().join("nn.txt"), "before").expect("write");
        repo.git(&["add", "-A"]);
        repo.commit("init");
        std::fs::write(repo.path().join("nn.txt"), "after").expect("write");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        let h = &files[0].hunks[0];
        let kinds: Vec<LineKind> = h.lines.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, vec![LineKind::Del, LineKind::NoNewline, LineKind::Add, LineKind::NoNewline]);
    }

    #[test]
    fn parse_quoted_unicode_path() {
        let mut repo = TempRepo::new();
        repo.commit_file("keep.txt", "x\n", "init");
        let name = "späce \"q\".txt";
        std::fs::write(repo.path().join(name), "hello\n").expect("write");
        repo.git(&["add", "-A"]);
        let out = repo.git(&["diff", "--cached"]);
        let files = parse(&out);
        let f = files.iter().find(|f| f.new_path.as_deref() != Some("keep.txt")).expect("found");
        assert_eq!(f.new_path.as_deref(), Some(name));
        assert_eq!(f.old_path, None);
    }

    #[test]
    fn parse_multi_hunk() {
        let mut repo = TempRepo::new();
        repo.commit_file("multi.txt", &(1..=30).map(|n| format!("{n}\n")).collect::<String>(), "init");
        let content: String = (1..=30)
            .map(|n| match n {
                2 => "CHANGED2\n".to_string(),
                28 => "CHANGED28\n".to_string(),
                _ => format!("{n}\n"),
            })
            .collect();
        repo.write("multi.txt", &content);
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        assert_eq!(files[0].hunks.len(), 2);
        assert_eq!(files[0].hunks[0].old_start, 1);
        assert_eq!(files[0].hunks[1].old_start, 25);
    }

    #[test]
    fn parse_hunk_header_single_line_counts() {
        // Hand-written: git omits `,1` when a side's length is 1.
        let diff =
            "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -5 +5,2 @@\n context\n+added\n";
        let files = parse(diff);
        let h = &files[0].hunks[0];
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (5, 1, 5, 2));
    }

    #[test]
    fn parse_copy_header() {
        // Hand-written: `-C` output isn't produced by our default flags, but the format is fixed.
        let diff =
            "diff --git a/orig.txt b/copy.txt\ncopy from orig.txt\ncopy to copy.txt\nindex 1..1 100644\n";
        let files = parse(diff);
        assert_eq!(files[0].old_path.as_deref(), Some("orig.txt"));
        assert_eq!(files[0].new_path.as_deref(), Some("copy.txt"));
        assert!(files[0].hunks.is_empty());
    }

    #[test]
    fn parse_hunk_section_text() {
        let diff = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@ fn foo() {\n context\n-old\n+new\n";
        let files = parse(diff);
        assert_eq!(files[0].hunks[0].section, "fn foo() {");
    }

    #[test]
    fn parse_multiple_files_in_one_output() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "a\n", "init");
        repo.commit_file("b.txt", "b\n", "add b");
        repo.write("a.txt", "A\n");
        repo.write("b.txt", "B\n");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path.as_deref(), Some("a.txt"));
        assert_eq!(files[1].new_path.as_deref(), Some("b.txt"));
    }

    // ---- patch_for() --------------------------------------------------------------------

    /// Feed `patch` to `git <args>` on stdin; panics with stderr on failure.
    fn run_apply(repo: &TempRepo, args: &[&str], patch: &str) {
        let mut child = hermetic_git(repo.path())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git apply");
        child.stdin.take().expect("stdin").write_all(patch.as_bytes()).expect("write patch");
        let out = child.wait_with_output().expect("wait");
        assert!(out.status.success(), "git {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }

    fn stage(repo: &TempRepo, patch: &str) {
        run_apply(repo, &["apply", "--cached", "--check"], patch);
        run_apply(repo, &["apply", "--cached"], patch);
    }

    fn unstage(repo: &TempRepo, patch: &str) {
        run_apply(repo, &["apply", "--cached", "-R", "--check"], patch);
        run_apply(repo, &["apply", "--cached", "-R"], patch);
    }

    fn discard(repo: &TempRepo, patch: &str) {
        run_apply(repo, &["apply", "-R", "--check"], patch);
        run_apply(repo, &["apply", "-R"], patch);
    }

    fn staged_content(repo: &TempRepo, path: &str) -> String {
        repo.git(&["show", &format!(":{path}")]) + "\n"
    }

    #[test]
    fn patch_for_whole_hunk_stages_everything() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "one\ntwo\nthree\n", "init");
        repo.write("a.txt", "one\nCHANGED\nthree\n");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_content(&repo, "a.txt"), "one\nCHANGED\nthree\n");
    }

    #[test]
    fn patch_for_single_added_line_in_middle_of_mixed_hunk() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n2\n3\n4\n5\n", "init");
        repo.write("a.txt", "1\nTWO\n3\nFOUR\n5\nSIX\n");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        let h = &files[0].hunks[0];
        // Select only the `+FOUR` line: its paired `-4` is untouched (kept as context, i.e. `4`
        // stays), so the index ends up with both `4` and `FOUR` — matching `git add -p`.
        let idx = h.lines.iter().position(|l| l.kind == LineKind::Add && l.text == "FOUR").expect("found");
        let patch = patch_for(&files[0], 0, &Selection::Lines(idx..=idx), false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_content(&repo, "a.txt"), "1\n2\n3\n4\nFOUR\n5\n");
    }

    #[test]
    fn patch_for_single_deleted_line() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n2\n3\n4\n5\n", "init");
        repo.write("a.txt", "1\nTWO\n3\nFOUR\n5\nSIX\n");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        let h = &files[0].hunks[0];
        let idx = h.lines.iter().position(|l| l.kind == LineKind::Del && l.text == "2").expect("found");
        let patch = patch_for(&files[0], 0, &Selection::Lines(idx..=idx), false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_content(&repo, "a.txt"), "1\n3\n4\n5\n");
    }

    #[test]
    fn patch_for_mixed_range() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n2\n3\n4\n5\n", "init");
        repo.write("a.txt", "1\nTWO\n3\nFOUR\n5\nSIX\n");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        let h = &files[0].hunks[0];
        // Select the `-2`, `+TWO`, `-4` run (everything about the "2"/"4" edits, not "SIX"/"FOUR").
        let start = h.lines.iter().position(|l| l.text == "2" && l.kind == LineKind::Del).expect("s");
        let end = h.lines.iter().position(|l| l.text == "4" && l.kind == LineKind::Del).expect("e");
        let patch = patch_for(&files[0], 0, &Selection::Lines(start..=end), false).expect("patch");
        stage(&repo, &patch);
        // "4" is removed (its `-4` was selected) and "FOUR" is dropped (its `+FOUR` was not).
        assert_eq!(staged_content(&repo, "a.txt"), "1\nTWO\n3\n5\n");
    }

    #[test]
    fn patch_for_second_hunk_of_multi_hunk_file() {
        let mut repo = TempRepo::new();
        repo.commit_file("multi.txt", &(1..=30).map(|n| format!("{n}\n")).collect::<String>(), "init");
        let content: String = (1..=30)
            .map(|n| match n {
                2 => "CHANGED2\n".to_string(),
                28 => "CHANGED28\n".to_string(),
                _ => format!("{n}\n"),
            })
            .collect();
        repo.write("multi.txt", &content);
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        assert_eq!(files[0].hunks.len(), 2);
        let patch = patch_for(&files[0], 1, &Selection::WholeHunk, false).expect("patch");
        stage(&repo, &patch);
        let staged = staged_content(&repo, "multi.txt");
        let expected: String =
            (1..=30).map(|n| if n == 28 { "CHANGED28\n".to_string() } else { format!("{n}\n") }).collect();
        assert_eq!(staged, expected);
    }

    #[test]
    fn patch_for_unstage_direction() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n2\n3\n", "init");
        repo.write("a.txt", "1\nTWO\n3\n");
        repo.git(&["add", "-A"]);
        let out = repo.git(&["diff", "--cached"]);
        let files = parse(&out);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, true).expect("patch");
        unstage(&repo, &patch);
        assert_eq!(staged_content(&repo, "a.txt"), "1\n2\n3\n");
        // Worktree is untouched by an index-only unstage.
        let wt = std::fs::read_to_string(repo.path().join("a.txt")).expect("read");
        assert_eq!(wt, "1\nTWO\n3\n");
    }

    #[test]
    fn patch_for_unstage_partial_selection_leaves_rest_staged() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n2\n3\n4\n5\n", "init");
        repo.write("a.txt", "1\nTWO\n3\nFOUR\n5\n");
        repo.git(&["add", "-A"]);
        let out = repo.git(&["diff", "--cached"]);
        let files = parse(&out);
        let h = &files[0].hunks[0];
        let idx = h.lines.iter().position(|l| l.kind == LineKind::Add && l.text == "TWO").expect("found");
        let patch = patch_for(&files[0], 0, &Selection::Lines(idx..=idx), true).expect("patch");
        unstage(&repo, &patch);
        // "TWO" is removed from the index; its paired `-2` wasn't selected so it stays dropped
        // (matches `git add -p`: unstaging one side of a replace doesn't restore the other).
        // "FOUR" stays staged since its hunk region wasn't touched.
        assert_eq!(staged_content(&repo, "a.txt"), "1\n3\nFOUR\n5\n");
    }

    #[test]
    fn patch_for_discard_direction() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n2\n3\n", "init");
        repo.write("a.txt", "1\nTWO\n3\n");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, true).expect("patch");
        discard(&repo, &patch);
        let wt = std::fs::read_to_string(repo.path().join("a.txt")).expect("read");
        assert_eq!(wt, "1\n2\n3\n");
    }

    #[test]
    fn patch_for_discard_partial_selection() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n2\n3\n4\n5\n", "init");
        repo.write("a.txt", "1\nTWO\n3\nFOUR\n5\n");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        let h = &files[0].hunks[0];
        let idx = h.lines.iter().position(|l| l.kind == LineKind::Add && l.text == "TWO").expect("found");
        let patch = patch_for(&files[0], 0, &Selection::Lines(idx..=idx), true).expect("patch");
        discard(&repo, &patch);
        // "TWO" is discarded from the worktree; its paired `-2` wasn't selected so "2" is not
        // restored (matches `git add -p`). "FOUR" is untouched.
        let wt = std::fs::read_to_string(repo.path().join("a.txt")).expect("read");
        assert_eq!(wt, "1\n3\nFOUR\n5\n");
    }

    #[test]
    fn patch_for_new_file_partial_selection() {
        let mut repo = TempRepo::new();
        repo.commit_file("keep.txt", "x\n", "init");
        repo.write("new.txt", "alpha\nbeta\ngamma\n");
        // `add -N` (intent-to-add) puts a placeholder in the index so the new file shows up
        // in a plain `git diff` as a normal `--- /dev/null` addition, without staging content.
        repo.git(&["add", "-N", "new.txt"]);
        let out = repo.git(&["diff", "--", "new.txt"]);
        let files = parse(&out);
        let h = &files[0].hunks[0];
        let idx = h.lines.iter().position(|l| l.text == "beta").expect("found");
        let patch = patch_for(&files[0], 0, &Selection::Lines(idx..=idx), false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_content(&repo, "new.txt"), "beta\n");
    }

    #[test]
    fn patch_for_no_newline_at_end_of_selection() {
        let mut repo = TempRepo::new();
        std::fs::write(repo.path().join("nn.txt"), "1\n2\nold").expect("write");
        repo.git(&["add", "-A"]);
        repo.commit("init");
        std::fs::write(repo.path().join("nn.txt"), "1\n2\nnew").expect("write");
        let out = repo.git(&["diff"]);
        let files = parse(&out);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, false).expect("patch");
        assert!(patch.trim_end_matches('\n').ends_with("No newline at end of file"));
        stage(&repo, &patch);
        let staged = repo.git(&["show", ":nn.txt"]);
        assert_eq!(staged, "1\n2\nnew");
    }

    #[test]
    fn patch_for_returns_none_without_change_lines() {
        let diff =
            "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let files = parse(diff);
        // Select only the context lines (indices 0 and 3): no +/- present.
        assert_eq!(patch_for(&files[0], 0, &Selection::Lines(0..=0), false), None);
    }

    #[test]
    fn patch_for_unknown_hunk_index_returns_none() {
        let diff = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-a\n+b\n";
        let files = parse(diff);
        assert_eq!(patch_for(&files[0], 5, &Selection::WholeHunk, false), None);
    }

    // ---- parse_raw_numstat() -------------------------------------------------------------

    #[test]
    fn raw_numstat_modify_add_delete() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "one\n", "init");
        repo.write("a.txt", "one\ntwo\n");
        repo.write("new.txt", "hello\n");
        repo.git(&["add", "-A"]);
        let commit = repo.commit("second");
        let out = repo.git_raw(&["show", "--format=", "--raw", "--numstat", "-z", "-M", &commit]);
        let mut changes = parse_raw_numstat(&out);
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "a.txt");
        assert_eq!(changes[0].status, 'M');
        assert_eq!(changes[0].added, Some(1));
        assert_eq!(changes[0].deleted, Some(0));
        assert_eq!(changes[1].path, "new.txt");
        assert_eq!(changes[1].status, 'A');
    }

    #[test]
    fn raw_numstat_rename_and_binary() {
        let mut repo = TempRepo::new();
        repo.write("orig.txt", &(1..=50).map(|n| format!("{n}\n")).collect::<String>());
        std::fs::write(repo.path().join("img.png"), [0x89, b'P', b'N', b'G', 0, 0, 0]).expect("write");
        repo.git(&["add", "-A"]);
        repo.commit("init");
        repo.git(&["mv", "orig.txt", "renamed.txt"]);
        repo.write(
            "renamed.txt",
            &(1..=50).map(|n| if n == 1 { "EDITED\n".into() } else { format!("{n}\n") }).collect::<String>(),
        );
        std::fs::write(repo.path().join("img.png"), [0x89, b'P', b'N', b'G', 9, 9, 9]).expect("write");
        repo.git(&["add", "-A"]);
        let commit = repo.commit("second");
        let out = repo.git_raw(&["diff-tree", "-r", "--root", "--raw", "--numstat", "-z", "-M", &commit]);
        let mut changes = parse_raw_numstat(&out);
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(changes.len(), 2);
        let img = changes.iter().find(|c| c.path == "img.png").expect("img");
        assert_eq!(img.status, 'M');
        assert_eq!(img.added, None);
        assert_eq!(img.deleted, None);
        let renamed = changes.iter().find(|c| c.path == "renamed.txt").expect("renamed");
        assert_eq!(renamed.status, 'R');
        assert_eq!(renamed.old_path.as_deref(), Some("orig.txt"));
        assert_eq!(renamed.added, Some(1));
        assert_eq!(renamed.deleted, Some(1));
    }

    #[test]
    fn raw_numstat_skips_leading_commit_hash() {
        // `git diff-tree -r --root <commit>` (without --no-commit-id) prints the commit hash
        // as its own NUL-terminated record before the raw block.
        let raw =
            b"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\0:100644 100644 aaa bbb M\0a.txt\x001\t0\ta.txt\0";
        let changes = parse_raw_numstat(raw);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "a.txt");
        assert_eq!(changes[0].status, 'M');
    }

    #[test]
    fn raw_numstat_empty_input() {
        assert_eq!(parse_raw_numstat(b""), vec![]);
    }

    // ---- word_diff() ----------------------------------------------------------------------

    #[test]
    fn word_diff_identical_lines_are_empty() {
        assert_eq!(word_diff("same text", "same text"), (vec![], vec![]));
        assert_eq!(word_diff("", ""), (vec![], vec![]));
    }

    #[test]
    fn word_diff_single_word_change() {
        let (old, new) = word_diff("the quick fox", "the slow fox");
        // "quick" -> "slow" only; "the ", " fox" stay unhighlighted.
        assert_eq!(old, vec![4..9]);
        assert_eq!(new, vec![4..8]);
        assert_eq!(&"the quick fox"[old[0].clone()], "quick");
        assert_eq!(&"the slow fox"[new[0].clone()], "slow");
    }

    #[test]
    fn word_diff_coalesces_adjacent_changes() {
        // No shared token between "ab"/"!" and "xy"/"?", so both are changed and merge into
        // one range; the trailing "cd" is common and stays unhighlighted.
        let (old, new) = word_diff("ab!cd", "xy?cd");
        assert_eq!(old, vec![0..3]);
        assert_eq!(new, vec![0..3]);
        assert_eq!(&"ab!cd"[old[0].clone()], "ab!");
        assert_eq!(&"xy?cd"[new[0].clone()], "xy?");
    }

    #[test]
    fn word_diff_pure_addition() {
        let (old, new) = word_diff("hello", "hello world");
        assert_eq!(old, vec![]);
        assert_eq!(&"hello world"[new[0].clone()], " world");
    }

    #[test]
    fn word_diff_multibyte_char_boundaries() {
        let (old, new) = word_diff("café old", "café new");
        // "café " unchanged (4-byte char é is 2 bytes in UTF-8: 'c','a','f','é'(2 bytes),' ').
        let old_str = "café old";
        let new_str = "café new";
        for r in &old {
            assert!(old_str.is_char_boundary(r.start) && old_str.is_char_boundary(r.end));
        }
        for r in &new {
            assert!(new_str.is_char_boundary(r.start) && new_str.is_char_boundary(r.end));
        }
        assert_eq!(&old_str[old[0].clone()], "old");
        assert_eq!(&new_str[new[0].clone()], "new");
    }

    #[test]
    fn word_diff_punctuation_tokens() {
        let (old, new) = word_diff("a, b", "a; b");
        assert_eq!(&"a, b"[old[0].clone()], ",");
        assert_eq!(&"a; b"[new[0].clone()], ";");
    }

    #[test]
    fn word_diff_over_token_cap_returns_whole_lines() {
        let old: String = (0..600).map(|n| format!("w{n} ")).collect();
        let new: String = (0..600).map(|n| format!("v{n} ")).collect();
        let (o, n) = word_diff(&old, &new);
        assert_eq!(o, vec![0..old.len()]);
        assert_eq!(n, vec![0..new.len()]);
    }
}
