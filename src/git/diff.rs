//! Diffs (§5.5–5.7): parse `git diff`/`git show` unified output (run with [`DIFF_ARGS`]), list
//! changed files, synthesise patches that stage/unstage/discard a hunk or a line range
//! (`git apply --cached [-R]`), and compute word-level intra-line highlights.

use std::borrow::Cow;
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
    /// The line's bytes without the leading ` `/`+`/`-` and the `\n`, verbatim — a CRLF line
    /// keeps its `\r`, non-UTF-8 text is untouched — so [`patch_for`] reproduces it exactly.
    /// Shown through [`Line::display`].
    pub text: Vec<u8>,
}

impl Line {
    /// `text` for display: decoded lossily, without a CRLF line's `\r`.
    pub fn display(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(self.text.strip_suffix(b"\r").unwrap_or(&self.text))
    }
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
    pub header: Vec<u8>,
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
/// other path line (mode-only changes, binary files). For quoted names the whole `"a/X" "b/Y"`
/// pair is authoritative; for unquoted names, `X`/`Y` are found by the only split of `X b/Y`
/// where the two halves are equal (true whenever the path is not itself being renamed).
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
    let section = after[close + 3..].trim().to_string();
    let (old, new) = ranges.split_once(' ')?;
    let (old_start, old_len) = parse_range(old)?;
    let (new_start, new_len) = parse_range(new)?;
    Some((old_start, old_len, new_start, new_len, section))
}

/// Flags every `git diff`/`git show`/`git diff --no-index` whose output goes to [`parse`] must
/// pass. They override the user config that changes what [`parse`] and [`patch_for`] rely on:
/// color (`color.ui`), an external diff tool (`diff.external`) or textconv filter instead of
/// git's own unified diff, and path prefixes other than `a/`/`b/` (`diff.noprefix`,
/// `diff.mnemonicPrefix`).
pub const DIFF_ARGS: &[&str] =
    &["--no-color", "--no-ext-diff", "--no-textconv", "--src-prefix=a/", "--dst-prefix=b/"];

/// Parse unified diff output (possibly several files) from a command run with [`DIFF_ARGS`].
/// Handles renames, mode changes, binary files, `\ No newline at end of file`, and quoted
/// paths with escapes. Content lines are kept as bytes: CRLF endings and non-UTF-8 text survive.
pub fn parse(out: &[u8]) -> Vec<FileDiff> {
    // Split on `\n` alone: `str::lines` would also strip the `\r` of a CRLF file's lines.
    let mut lines: Vec<&[u8]> = out.split(|&b| b == b'\n').collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let mut files = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].starts_with(b"diff --git ") {
            i += 1;
            continue;
        }
        let start = i;
        let (mut old_path, mut new_path) = match split_diff_git_line(&String::from_utf8_lossy(lines[i])) {
            Some((a, b)) => (Some(a), Some(b)),
            None => (None, None),
        };
        let mut binary = false;
        i += 1;
        while i < lines.len() && !lines[i].starts_with(b"@@ ") && !lines[i].starts_with(b"diff --git ") {
            let l = String::from_utf8_lossy(lines[i]);
            let l = l.as_ref();
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
                // Only the flag: the paths come from the lines above (`new file mode` and
                // `deleted file mode` mark the `/dev/null` side). This line can't name them
                // itself — an unquoted name may contain the " and " separating the two.
                binary = true;
            }
            i += 1;
        }
        let mut header = lines[start..i].join(&b'\n');
        header.push(b'\n');

        let mut hunks = Vec::new();
        while i < lines.len() && lines[i].starts_with(b"@@ ") {
            let Some((old_start, old_len, new_start, new_len, section)) =
                parse_hunk_header(&String::from_utf8_lossy(lines[i]))
            else {
                break;
            };
            i += 1;
            let mut hunk_lines = Vec::new();
            let mut old_no = old_start;
            let mut new_no = new_start;
            while i < lines.len() && !lines[i].starts_with(b"@@ ") && !lines[i].starts_with(b"diff --git ") {
                let l = lines[i];
                if l == b"\\ No newline at end of file" {
                    hunk_lines.push(Line {
                        kind: LineKind::NoNewline,
                        old_no: None,
                        new_no: None,
                        text: Vec::new(),
                    });
                } else if let Some(t) = l.strip_prefix(b"+") {
                    hunk_lines.push(Line {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(new_no),
                        text: t.to_vec(),
                    });
                    new_no += 1;
                } else if let Some(t) = l.strip_prefix(b"-") {
                    hunk_lines.push(Line {
                        kind: LineKind::Del,
                        old_no: Some(old_no),
                        new_no: None,
                        text: t.to_vec(),
                    });
                    old_no += 1;
                } else if let Some(t) = l.strip_prefix(b" ") {
                    hunk_lines.push(Line {
                        kind: LineKind::Context,
                        old_no: Some(old_no),
                        new_no: Some(new_no),
                        text: t.to_vec(),
                    });
                    old_no += 1;
                    new_no += 1;
                } else if l.is_empty() {
                    hunk_lines.push(Line {
                        kind: LineKind::Context,
                        old_no: Some(old_no),
                        new_no: Some(new_no),
                        text: Vec::new(),
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

/// One line of a [`patch_for`] hunk: the sides of the patch it is on (both for context), and
/// whether the diff marked it `\ No newline at end of file`.
struct PatchLine<'a> {
    old: bool,
    new: bool,
    text: &'a [u8],
    no_newline: bool,
}

fn push_patch_line(out: &mut Vec<u8>, sign: u8, text: &[u8], no_newline: bool) {
    out.push(sign);
    out.extend_from_slice(text);
    out.push(b'\n');
    if no_newline {
        out.extend_from_slice(b"\\ No newline at end of file\n");
    }
}

/// Build a patch applying only `sel` of `file.hunks[hunk]`, suitable for
/// `git apply --cached` (stage), `git apply --cached -R` (unstage), or `git apply -R`
/// (discard). Unselected `+` lines are dropped; unselected `-` lines become context; hunk header
/// counts are recomputed. `reverse` means the patch will be applied with `-R` (so unselected
/// `+` lines become context instead). The patch changes the file's content only: a rename, copy,
/// or mode change is left to file-level actions, and part of a new or deleted file is patched
/// in place (`patch_header`). Returns `None` if the selection contains no change, or if the
/// header names no `a/`/`b/` path (`file` did not come from [`DIFF_ARGS`] output).
pub fn patch_for(file: &FileDiff, hunk: usize, sel: &Selection, reverse: bool) -> Option<Vec<u8>> {
    let h = file.hunks.get(hunk)?;
    let selected = |k: usize| match sel {
        Selection::WholeHunk => true,
        Selection::Lines(r) => r.contains(&k),
    };
    let is_change = |k: usize| matches!(h.lines[k].kind, LineKind::Add | LineKind::Del);
    if !(0..h.lines.len()).any(|k| selected(k) && is_change(k)) {
        return None;
    }
    let whole_file = file.hunks.len() == 1 && (0..h.lines.len()).all(|k| selected(k) || !is_change(k));

    let mut kept: Vec<PatchLine> = Vec::new();
    let mut prev_kept = false;
    for (k, line) in h.lines.iter().enumerate() {
        let (old, new) = match line.kind {
            LineKind::NoNewline => {
                if prev_kept && let Some(prev) = kept.last_mut() {
                    prev.no_newline = true;
                }
                continue;
            }
            LineKind::Context => (true, true),
            LineKind::Add if selected(k) => (false, true),
            LineKind::Del if selected(k) => (true, false),
            // Unselected `+` becomes context: the `-R` apply must leave it untouched.
            LineKind::Add if reverse => (true, true),
            // Unselected `-` becomes context: the forward apply must leave it untouched.
            LineKind::Del if !reverse => (true, true),
            LineKind::Add | LineKind::Del => {
                prev_kept = false;
                continue;
            }
        };
        kept.push(PatchLine { old, new, text: &line.text, no_newline: false });
        prev_kept = true;
    }

    let (mut old_len, mut new_len) = (0u32, 0u32);
    let mut body = Vec::new();
    for (idx, line) in kept.iter().enumerate() {
        old_len += u32::from(line.old);
        new_len += u32::from(line.new);
        // `\ No newline` holds on a side only while the line is that side's last line in this
        // patch. A `-b` the selection turned into context can be followed by kept `+` lines;
        // `b` then needs its newline on the new side, or git glues the next line onto it.
        let later = &kept[idx + 1..];
        let old_eof = line.no_newline && line.old && !later.iter().any(|l| l.old);
        let new_eof = line.no_newline && line.new && !later.iter().any(|l| l.new);
        match (line.old, line.new) {
            (true, true) if old_eof == new_eof => push_patch_line(&mut body, b' ', line.text, old_eof),
            (true, true) => {
                push_patch_line(&mut body, b'-', line.text, old_eof);
                push_patch_line(&mut body, b'+', line.text, new_eof);
            }
            (true, false) => push_patch_line(&mut body, b'-', line.text, old_eof),
            _ => push_patch_line(&mut body, b'+', line.text, new_eof),
        }
    }

    // The patch's two sides differ by this hunk alone, so both start where the hunk sits in
    // the file being patched: at the diff's old side for a forward apply (the index, which has
    // none of the diff's hunks), at its new side under `-R` (the worktree or index, which has
    // all of them). git apply starts searching there and takes the nearest match, so starting
    // on the wrong side can land on an identical block elsewhere. As in git's own output, an
    // empty side is numbered by the line before it.
    let (start, len) = if reverse { (h.new_start, h.new_len) } else { (h.old_start, h.old_len) };
    let first = if len == 0 { start.saturating_add(1) } else { start };
    let range =
        |n: u32| if n == 0 { format!("{},0", first.saturating_sub(1)) } else { format!("{first},{n}") };

    let mut patch = patch_header(file, reverse, whole_file)?;
    patch.extend_from_slice(format!("@@ -{} +{} @@", range(old_len), range(new_len)).as_bytes());
    if !h.section.is_empty() {
        patch.push(b' ');
        patch.extend_from_slice(h.section.as_bytes());
    }
    patch.push(b'\n');
    patch.extend_from_slice(&body);
    Some(patch)
}

/// The file header of a [`patch_for`] patch. The diff's own header is kept only when the patch
/// must create or delete the file: when the index or worktree being patched lacks the file
/// (staging part of a new file, restoring part of a deleted one), and when the selection is the
/// whole change of a new or deleted file in the other direction. Otherwise the patch edits the
/// file in place, under its name in the index or worktree being patched (the diff's old side
/// forward, its new side under `-R`), with a plain `a/P b/P` header. A rename, copy, or mode
/// change belongs to the whole file, not to a hunk or line range, and a partial selection of a
/// new or deleted file keeps the file. `None` if that name is missing or lacks its `a/`/`b/`
/// prefix.
fn patch_header(file: &FileDiff, reverse: bool, whole_file: bool) -> Option<Vec<u8>> {
    let (is_new, is_deleted) = (file.old_path.is_none(), file.new_path.is_none());
    let (target_lacks_file, removes_file) = if reverse { (is_deleted, is_new) } else { (is_new, is_deleted) };
    if target_lacks_file || (removes_file && whole_file) {
        return Some(file.header.clone());
    }
    let marker: &[u8] = if reverse { b"+++ " } else { b"--- " };
    let name = file.header.split(|&b| b == b'\n').find_map(|l| l.strip_prefix(marker))?;
    // git ends a `---`/`+++` name that contains a space with a tab.
    let name = name.strip_suffix(b"\t").unwrap_or(name);
    let (a, b) = (with_side_prefix(name, b'a')?, with_side_prefix(name, b'b')?);
    Some([&b"diff --git "[..], &a, b" ", &b, b"\n--- ", &a, b"\n+++ ", &b, b"\n"].concat())
}

/// A header name as git prints it (`a/X`, `b/X`, or quoted `"a/X"`) with its side set to `side`.
fn with_side_prefix(name: &[u8], side: u8) -> Option<Vec<u8>> {
    let (quote, unquoted) = match name.strip_prefix(b"\"") {
        Some(rest) => (&b"\""[..], rest),
        None => (&b""[..], name),
    };
    let path = unquoted.strip_prefix(b"a/").or_else(|| unquoted.strip_prefix(b"b/"))?;
    Some([quote, &[side, b'/'], path].concat())
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
        let files = parse(out.as_bytes());
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.old_path.as_deref(), Some("a.txt"));
        assert_eq!(f.new_path.as_deref(), Some("a.txt"));
        assert!(!f.binary);
        assert!(f.header.starts_with(b"diff --git a/a.txt b/a.txt\n"));
        assert!(f.header.ends_with(b"+++ b/a.txt\n"));
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
        let files = parse(out.as_bytes());
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
        let files = parse(out.as_bytes());
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
        let files = parse(out.as_bytes());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path.as_deref(), Some("orig.txt"));
        assert_eq!(files[0].new_path.as_deref(), Some("renamed.txt"));
        assert_eq!(files[0].hunks.len(), 1);
        assert!(files[0].hunks[0].lines.iter().any(|l| l.kind == LineKind::Del && l.text == b"5"));
        assert!(files[0].hunks[0].lines.iter().any(|l| l.kind == LineKind::Add && l.text == b"EDITED"));
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
        let files = parse(out.as_bytes());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path.as_deref(), Some("script.sh"));
        assert_eq!(files[0].new_path.as_deref(), Some("script.sh"));
        assert!(files[0].hunks.is_empty());
        assert!(!files[0].binary);
        assert!(String::from_utf8_lossy(&files[0].header).contains("old mode 100644"));
        assert!(String::from_utf8_lossy(&files[0].header).contains("new mode 100755"));
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
        let files = parse(out.as_bytes());
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
        let files = parse(out.as_bytes());
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
        let files = parse(out.as_bytes());
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
        let files = parse(out.as_bytes());
        assert_eq!(files[0].hunks.len(), 2);
        assert_eq!(files[0].hunks[0].old_start, 1);
        assert_eq!(files[0].hunks[1].old_start, 25);
    }

    #[test]
    fn parse_hunk_header_single_line_counts() {
        // Hand-written: git omits `,1` when a side's length is 1.
        let diff =
            "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -5 +5,2 @@\n context\n+added\n";
        let files = parse(diff.as_bytes());
        let h = &files[0].hunks[0];
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (5, 1, 5, 2));
    }

    #[test]
    fn parse_copy_header() {
        // Hand-written: `-C` output isn't produced by our default flags, but the format is fixed.
        let diff =
            "diff --git a/orig.txt b/copy.txt\ncopy from orig.txt\ncopy to copy.txt\nindex 1..1 100644\n";
        let files = parse(diff.as_bytes());
        assert_eq!(files[0].old_path.as_deref(), Some("orig.txt"));
        assert_eq!(files[0].new_path.as_deref(), Some("copy.txt"));
        assert!(files[0].hunks.is_empty());
    }

    #[test]
    fn parse_hunk_section_text() {
        let diff = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@ fn foo() {\n context\n-old\n+new\n";
        let files = parse(diff.as_bytes());
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
        let files = parse(out.as_bytes());
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path.as_deref(), Some("a.txt"));
        assert_eq!(files[1].new_path.as_deref(), Some("b.txt"));
    }

    // ---- patch_for() --------------------------------------------------------------------

    /// Feed `patch` to `git <args>` on stdin; panics with stderr on failure.
    fn run_apply(repo: &TempRepo, args: &[&str], patch: &[u8]) {
        let mut child = hermetic_git(repo.path())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git apply");
        child.stdin.take().expect("stdin").write_all(patch).expect("write patch");
        let out = child.wait_with_output().expect("wait");
        assert!(out.status.success(), "git {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }

    fn stage(repo: &TempRepo, patch: &[u8]) {
        run_apply(repo, &["apply", "--cached", "--check"], patch);
        run_apply(repo, &["apply", "--cached"], patch);
    }

    fn unstage(repo: &TempRepo, patch: &[u8]) {
        run_apply(repo, &["apply", "--cached", "-R", "--check"], patch);
        run_apply(repo, &["apply", "--cached", "-R"], patch);
    }

    fn discard(repo: &TempRepo, patch: &[u8]) {
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
        let files = parse(out.as_bytes());
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
        let files = parse(out.as_bytes());
        let h = &files[0].hunks[0];
        // Select only the `+FOUR` line: its paired `-4` is untouched (kept as context, i.e. `4`
        // stays), so the index ends up with both `4` and `FOUR` — matching `git add -p`.
        let idx = h.lines.iter().position(|l| l.kind == LineKind::Add && l.text == b"FOUR").expect("found");
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
        let files = parse(out.as_bytes());
        let h = &files[0].hunks[0];
        let idx = h.lines.iter().position(|l| l.kind == LineKind::Del && l.text == b"2").expect("found");
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
        let files = parse(out.as_bytes());
        let h = &files[0].hunks[0];
        // Select the `-2`, `+TWO`, `-4` run (everything about the "2"/"4" edits, not "SIX"/"FOUR").
        let start = h.lines.iter().position(|l| l.text == b"2" && l.kind == LineKind::Del).expect("s");
        let end = h.lines.iter().position(|l| l.text == b"4" && l.kind == LineKind::Del).expect("e");
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
        let files = parse(out.as_bytes());
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
        let files = parse(out.as_bytes());
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
        let files = parse(out.as_bytes());
        let h = &files[0].hunks[0];
        let idx = h.lines.iter().position(|l| l.kind == LineKind::Add && l.text == b"TWO").expect("found");
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
        let files = parse(out.as_bytes());
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
        let files = parse(out.as_bytes());
        let h = &files[0].hunks[0];
        let idx = h.lines.iter().position(|l| l.kind == LineKind::Add && l.text == b"TWO").expect("found");
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
        let files = parse(out.as_bytes());
        let h = &files[0].hunks[0];
        let idx = h.lines.iter().position(|l| l.text == b"beta").expect("found");
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
        let files = parse(out.as_bytes());
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, false).expect("patch");
        assert!(patch.ends_with(b"\n\\ No newline at end of file\n"));
        stage(&repo, &patch);
        let staged = repo.git(&["show", ":nn.txt"]);
        assert_eq!(staged, "1\n2\nnew");
    }

    #[test]
    fn patch_for_returns_none_without_change_lines() {
        let diff =
            "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let files = parse(diff.as_bytes());
        // Select only the context lines (indices 0 and 3): no +/- present.
        assert_eq!(patch_for(&files[0], 0, &Selection::Lines(0..=0), false), None);
    }

    #[test]
    fn patch_for_unknown_hunk_index_returns_none() {
        let diff = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-a\n+b\n";
        let files = parse(diff.as_bytes());
        assert_eq!(patch_for(&files[0], 5, &Selection::WholeHunk, false), None);
    }

    // ---- patch_for(): positions, line endings, encodings, file headers ----------------------

    /// `git <cmd> <DIFF_ARGS> <rest>`, parsed: the argv every caller of `parse` pins.
    fn diff_of(repo: &TempRepo, cmd: &str, rest: &[&str]) -> Vec<FileDiff> {
        let mut args = vec![cmd];
        args.extend_from_slice(DIFF_ARGS);
        args.extend_from_slice(rest);
        parse(&repo.git_raw(&args))
    }

    fn staged_bytes(repo: &TempRepo, path: &str) -> Vec<u8> {
        repo.git_raw(&["show", &format!(":{path}")])
    }

    fn worktree_bytes(repo: &TempRepo, path: &str) -> Vec<u8> {
        std::fs::read(repo.path().join(path)).expect("read")
    }

    /// `c1 c2 c3 <mid> c4 c5 c6`, one per line.
    fn block(mid: &str) -> String {
        format!("c1\nc2\nc3\n{mid}\nc4\nc5\nc6\n")
    }

    /// Two identical blocks, with the second one's `KEEP` changed to `NEW` and 30 lines inserted
    /// above both: hunk 1 is `@@ -17,7 +47,7 @@`, and in the patched file the first block (at
    /// 32) sits nearer the hunk's old line (17) than the hunk itself does (47).
    fn shifted_repeated_blocks() -> (String, String, String) {
        let filler: String = (1..=8).map(|n| format!("f{n}\n")).collect();
        let inserted: String = (1..=30).map(|n| format!("ins{n}\n")).collect();
        let old = format!("top\n{}{filler}{}", block("NEW"), block("KEEP"));
        let new = format!("top\n{inserted}{}{filler}{}", block("NEW"), block("NEW"));
        let new_without_hunk_1 = format!("top\n{inserted}{}{filler}{}", block("NEW"), block("KEEP"));
        (old, new, new_without_hunk_1)
    }

    #[test]
    fn patch_for_discard_hunk_after_line_shifting_hunk_reverts_that_hunk() {
        let (old, new, expected) = shifted_repeated_blocks();
        let mut repo = TempRepo::new();
        repo.commit_file("f.txt", &old, "init");
        repo.write("f.txt", &new);
        let files = diff_of(&repo, "diff", &[]);
        assert_eq!((files[0].hunks[1].old_start, files[0].hunks[1].new_start), (17, 47));
        let patch = patch_for(&files[0], 1, &Selection::WholeHunk, true).expect("patch");
        discard(&repo, &patch);
        assert_eq!(String::from_utf8(worktree_bytes(&repo, "f.txt")).expect("utf8"), expected);
    }

    #[test]
    fn patch_for_unstage_hunk_after_line_shifting_hunk_reverts_that_hunk() {
        let (old, new, expected) = shifted_repeated_blocks();
        let mut repo = TempRepo::new();
        repo.commit_file("f.txt", &old, "init");
        repo.write("f.txt", &new);
        repo.git(&["add", "-A"]);
        let files = diff_of(&repo, "diff", &["--cached"]);
        let patch = patch_for(&files[0], 1, &Selection::WholeHunk, true).expect("patch");
        unstage(&repo, &patch);
        assert_eq!(String::from_utf8(staged_bytes(&repo, "f.txt")).expect("utf8"), expected);
    }

    /// Index `a\nb` (no final newline), worktree `a\nb\nc\n`: ` a`, `-b`, `\`, `+b`, `+c`.
    fn appended_after_missing_newline() -> (TempRepo, FileDiff) {
        let mut repo = TempRepo::new();
        std::fs::write(repo.path().join("nn.txt"), "a\nb").expect("write");
        repo.git(&["add", "-A"]);
        repo.commit("init");
        repo.write("nn.txt", "a\nb\nc\n");
        let file = diff_of(&repo, "diff", &[]).remove(0);
        let kinds: Vec<LineKind> = file.hunks[0].lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [LineKind::Context, LineKind::Del, LineKind::NoNewline, LineKind::Add, LineKind::Add]
        );
        (repo, file)
    }

    #[test]
    fn patch_for_stage_line_appended_after_last_line_without_newline() {
        let (repo, file) = appended_after_missing_newline();
        // `+c` alone: `b` stays, so it needs its newline before `c` can follow it.
        let patch = patch_for(&file, 0, &Selection::Lines(4..=4), false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_bytes(&repo, "nn.txt"), b"a\nb\nc\n");
    }

    #[test]
    fn patch_for_stage_added_lines_but_not_deleted_last_line_without_newline() {
        let (repo, file) = appended_after_missing_newline();
        // `+b`, `+c` without `-b`: the old `b` is kept and the two lines follow it.
        let patch = patch_for(&file, 0, &Selection::Lines(3..=4), false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_bytes(&repo, "nn.txt"), b"a\nb\nb\nc\n");
    }

    #[test]
    fn patch_for_discard_deleted_last_line_without_newline_before_kept_lines() {
        let (repo, file) = appended_after_missing_newline();
        // Discarding only `-b` restores the old `b`; the kept `b`, `c` follow it, so it is no
        // longer the last line and takes a newline.
        let patch = patch_for(&file, 0, &Selection::Lines(1..=1), true).expect("patch");
        discard(&repo, &patch);
        assert_eq!(worktree_bytes(&repo, "nn.txt"), b"a\nb\nb\nc\n");
    }

    #[test]
    fn parse_keeps_line_bytes_and_display_decodes_them() {
        let diff = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n same\r\n-caf\xe9\r\n+cafe\r\n";
        let lines = &parse(diff)[0].hunks[0].lines;
        assert_eq!(lines[0].text, b"same\r");
        assert_eq!(lines[1].text, b"caf\xe9\r");
        assert_eq!(lines[0].display(), "same");
        assert_eq!(lines[1].display(), "caf\u{FFFD}");
        assert_eq!(lines[2].display(), "cafe");
    }

    #[test]
    fn patch_for_crlf_file_stages_hunk_and_line_with_line_endings_intact() {
        let mut repo = TempRepo::new();
        repo.commit_file("f.txt", "one\r\ntwo\r\nthree\r\nfour\r\n", "init");
        repo.write("f.txt", "one\r\nTWO\r\nthree\r\nFOUR\r\n");
        let files = diff_of(&repo, "diff", &[]);
        let h = &files[0].hunks[0];
        let idx = h.lines.iter().position(|l| l.kind == LineKind::Add).expect("add");
        let patch = patch_for(&files[0], 0, &Selection::Lines(idx - 1..=idx), false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_bytes(&repo, "f.txt"), b"one\r\nTWO\r\nthree\r\nfour\r\n");
        let files = diff_of(&repo, "diff", &[]);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_bytes(&repo, "f.txt"), b"one\r\nTWO\r\nthree\r\nFOUR\r\n");
    }

    #[test]
    fn patch_for_crlf_new_file_keeps_line_endings() {
        let mut repo = TempRepo::new();
        repo.commit_file("keep.txt", "x\n", "init");
        repo.write("n.txt", "alpha\r\nbeta\r\n");
        repo.git(&["add", "-N", "n.txt"]);
        let files = diff_of(&repo, "diff", &[]);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_bytes(&repo, "n.txt"), b"alpha\r\nbeta\r\n");
    }

    #[test]
    fn patch_for_non_utf8_lines_are_staged_byte_exact() {
        let mut repo = TempRepo::new();
        std::fs::write(repo.path().join("l1.txt"), b"caf\xe9\nsame\nold\n").expect("write");
        repo.git(&["add", "-A"]);
        repo.commit("init");
        // Latin-1 in a context line and in the added line.
        std::fs::write(repo.path().join("l1.txt"), b"caf\xe9\nsame\nna\xefve\n").expect("write");
        let files = diff_of(&repo, "diff", &[]);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_bytes(&repo, "l1.txt"), b"caf\xe9\nsame\nna\xefve\n");
    }

    #[test]
    fn patch_for_non_utf8_new_file_is_staged_byte_exact() {
        let mut repo = TempRepo::new();
        repo.commit_file("keep.txt", "x\n", "init");
        std::fs::write(repo.path().join("new.txt"), b"caf\xe9\n").expect("write");
        repo.git(&["add", "-N", "new.txt"]);
        let files = diff_of(&repo, "diff", &[]);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_bytes(&repo, "new.txt"), b"caf\xe9\n");
    }

    fn forty_lines(edit: &[u32]) -> String {
        (1..=40).map(|n| if edit.contains(&n) { format!("EDIT{n}\n") } else { format!("{n}\n") }).collect()
    }

    #[test]
    fn patch_for_unstage_hunk_of_staged_rename_keeps_the_rename() {
        let mut repo = TempRepo::new();
        repo.commit_file("orig.txt", &forty_lines(&[]), "init");
        repo.git(&["mv", "orig.txt", "renamed.txt"]);
        repo.write("renamed.txt", &forty_lines(&[3, 37]));
        repo.git(&["add", "-A"]);
        let files = diff_of(&repo, "diff", &["--cached"]);
        assert_eq!(files[0].old_path.as_deref(), Some("orig.txt"));
        assert_eq!(files[0].hunks.len(), 2);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, true).expect("patch");
        unstage(&repo, &patch);
        let status = repo.git(&["diff", "--cached", "--name-status", "-M"]);
        assert!(status.starts_with('R') && status.ends_with("\torig.txt\trenamed.txt"), "{status}");
        assert_eq!(String::from_utf8(staged_bytes(&repo, "renamed.txt")).expect("utf8"), forty_lines(&[37]));
    }

    #[test]
    fn patch_for_hunk_of_rename_with_space_and_quoted_names() {
        let mut repo = TempRepo::new();
        repo.commit_file("old name.txt", &forty_lines(&[]), "init");
        let new_name = "späce ñew.txt";
        repo.git(&["mv", "old name.txt", new_name]);
        repo.write(new_name, &forty_lines(&[3, 37]));
        repo.git(&["add", "-A"]);
        let files = diff_of(&repo, "diff", &["--cached"]);
        assert_eq!(files[0].old_path.as_deref(), Some("old name.txt"));
        assert_eq!(files[0].new_path.as_deref(), Some(new_name));
        let patch = patch_for(&files[0], 1, &Selection::WholeHunk, true).expect("patch");
        unstage(&repo, &patch);
        assert_eq!(String::from_utf8(staged_bytes(&repo, new_name)).expect("utf8"), forty_lines(&[3]));
        // A plain modification of a name with a space (git ends its `---`/`+++` with a tab).
        let mut repo = TempRepo::new();
        repo.commit_file("my file.txt", &forty_lines(&[]), "init");
        repo.write("my file.txt", &forty_lines(&[3, 37]));
        let files = diff_of(&repo, "diff", &[]);
        assert!(files[0].header.ends_with(b"\n+++ b/my file.txt\t\n"));
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(String::from_utf8(staged_bytes(&repo, "my file.txt")).expect("utf8"), forty_lines(&[3]));
    }

    #[test]
    fn patch_for_stage_hunk_of_intent_to_add_rename_patches_the_old_name() {
        let mut repo = TempRepo::new();
        repo.commit_file("orig.txt", &forty_lines(&[]), "init");
        std::fs::rename(repo.path().join("orig.txt"), repo.path().join("renamed.txt")).expect("mv");
        repo.write("renamed.txt", &forty_lines(&[3, 37]));
        repo.git(&["add", "-N", "renamed.txt"]);
        let files = diff_of(&repo, "diff", &[]);
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].old_path.as_deref(), Some("orig.txt"));
        assert_eq!(files[0].new_path.as_deref(), Some("renamed.txt"));
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(String::from_utf8(staged_bytes(&repo, "orig.txt")).expect("utf8"), forty_lines(&[3]));
    }

    #[test]
    fn patch_for_unstage_submodule_pointer_hunk() {
        // The rewritten header drops the `index … 160000` line; git takes the gitlink mode from
        // the index entry instead.
        let mut repo = TempRepo::new();
        let a = repo.commit_file("keep.txt", "x\n", "init");
        let b = repo.commit_file("keep.txt", "y\n", "second");
        repo.git(&["update-index", "--add", "--cacheinfo", &format!("160000,{a},sub")]);
        repo.commit("sub");
        repo.git(&["update-index", "--cacheinfo", &format!("160000,{b},sub")]);
        let files = diff_of(&repo, "diff", &["--cached", "--", "sub"]);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, true).expect("patch");
        unstage(&repo, &patch);
        assert_eq!(repo.git(&["ls-files", "-s", "sub"]), format!("160000 {a} 0\tsub"));
    }

    #[test]
    fn patch_for_refuses_a_header_without_side_prefixes() {
        // `diff.noprefix` output (what DIFF_ARGS prevents): `git apply` would strip `src/` and
        // patch a top-level `x.rs` instead.
        let diff = "diff --git src/x.rs src/x.rs\nindex 1..2 100644\n--- src/x.rs\n+++ src/x.rs\n\
                    @@ -1,1 +1,1 @@\n-a\n+b\n";
        let files = parse(diff.as_bytes());
        assert_eq!(patch_for(&files[0], 0, &Selection::WholeHunk, false), None);
    }

    #[test]
    fn patch_for_hunk_actions_leave_the_mode_change_alone() {
        let mut repo = TempRepo::new();
        repo.commit_file("f.sh", &forty_lines(&[]), "init");
        repo.write("f.sh", &forty_lines(&[3, 37]));
        repo.git(&["add", "-A"]);
        repo.git(&["update-index", "--chmod=+x", "f.sh"]);
        // Staged: mode 100644 → 100755 and two hunks. Unstaging one hunk keeps the mode staged.
        let files = diff_of(&repo, "diff", &["--cached"]);
        assert!(String::from_utf8_lossy(&files[0].header).contains("new mode 100755"));
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, true).expect("patch");
        unstage(&repo, &patch);
        assert!(repo.git(&["ls-files", "-s", "f.sh"]).starts_with("100755 "));
        assert_eq!(String::from_utf8(staged_bytes(&repo, "f.sh")).expect("utf8"), forty_lines(&[37]));
        // Unstaged: the worktree's 100644 against the index's 100755. Staging a hunk keeps 100755.
        let files = diff_of(&repo, "diff", &[]);
        assert!(String::from_utf8_lossy(&files[0].header).contains("new mode 100644"));
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, false).expect("patch");
        stage(&repo, &patch);
        assert!(repo.git(&["ls-files", "-s", "f.sh"]).starts_with("100755 "));
        assert_eq!(String::from_utf8(staged_bytes(&repo, "f.sh")).expect("utf8"), forty_lines(&[3, 37]));
    }

    #[test]
    fn parse_binary_paths_containing_and() {
        let mut repo = TempRepo::new();
        std::fs::write(repo.path().join("Terms and Conditions.pdf"), [0u8, 1, 2]).expect("write");
        std::fs::write(repo.path().join("x and y.bin"), [0u8, 1, 2]).expect("write");
        repo.git(&["add", "-A"]);
        repo.commit("init");
        std::fs::write(repo.path().join("Terms and Conditions.pdf"), [0u8, 9, 9]).expect("write");
        std::fs::remove_file(repo.path().join("x and y.bin")).expect("rm");
        let files = diff_of(&repo, "diff", &[]);
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.binary));
        assert_eq!(files[0].old_path.as_deref(), Some("Terms and Conditions.pdf"));
        assert_eq!(files[0].new_path.as_deref(), Some("Terms and Conditions.pdf"));
        assert_eq!(files[1].old_path.as_deref(), Some("x and y.bin"));
        assert_eq!(files[1].new_path, None);
    }

    #[test]
    fn patch_for_stage_some_lines_of_deleted_file() {
        let mut repo = TempRepo::new();
        repo.commit_file("gone.txt", "a\nb\nc\n", "init");
        std::fs::remove_file(repo.path().join("gone.txt")).expect("rm");
        let files = diff_of(&repo, "diff", &[]);
        let patch = patch_for(&files[0], 0, &Selection::Lines(1..=1), false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_bytes(&repo, "gone.txt"), b"a\nc\n");
        // The rest, as a whole hunk, stages the deletion itself.
        let files = diff_of(&repo, "diff", &[]);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(repo.git(&["ls-files", "--", "gone.txt"]), "");
    }

    #[test]
    fn patch_for_discard_some_lines_of_deleted_file_restores_them() {
        let mut repo = TempRepo::new();
        repo.commit_file("gone.txt", "a\nb\nc\n", "init");
        std::fs::remove_file(repo.path().join("gone.txt")).expect("rm");
        let files = diff_of(&repo, "diff", &[]);
        let patch = patch_for(&files[0], 0, &Selection::Lines(1..=1), true).expect("patch");
        discard(&repo, &patch);
        assert_eq!(worktree_bytes(&repo, "gone.txt"), b"b\n");
    }

    #[test]
    fn patch_for_discard_some_lines_of_new_file() {
        let mut repo = TempRepo::new();
        repo.commit_file("keep.txt", "x\n", "init");
        repo.write("new.txt", "alpha\nbeta\ngamma\n");
        repo.git(&["add", "-N", "new.txt"]);
        let files = diff_of(&repo, "diff", &[]);
        let patch = patch_for(&files[0], 0, &Selection::Lines(1..=1), true).expect("patch");
        discard(&repo, &patch);
        assert_eq!(worktree_bytes(&repo, "new.txt"), b"alpha\ngamma\n");
        // The rest, as a whole hunk, discards the file itself.
        let files = diff_of(&repo, "diff", &[]);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, true).expect("patch");
        discard(&repo, &patch);
        assert!(!repo.path().join("new.txt").exists());
    }

    #[test]
    fn patch_for_unstage_some_lines_of_staged_new_file() {
        let mut repo = TempRepo::new();
        repo.commit_file("keep.txt", "x\n", "init");
        repo.write("new.txt", "alpha\nbeta\ngamma\n");
        repo.git(&["add", "-A"]);
        let files = diff_of(&repo, "diff", &["--cached"]);
        let patch = patch_for(&files[0], 0, &Selection::Lines(1..=1), true).expect("patch");
        unstage(&repo, &patch);
        assert_eq!(staged_bytes(&repo, "new.txt"), b"alpha\ngamma\n");
        // The rest, as a whole hunk, unstages the file itself.
        let files = diff_of(&repo, "diff", &["--cached"]);
        let patch = patch_for(&files[0], 0, &Selection::WholeHunk, true).expect("patch");
        unstage(&repo, &patch);
        assert_eq!(repo.git(&["ls-files", "--", "new.txt"]), "");
        assert_eq!(worktree_bytes(&repo, "new.txt"), b"alpha\nbeta\ngamma\n");
    }

    #[test]
    fn patch_for_untracked_file_from_no_index_diff() {
        let mut repo = TempRepo::new();
        repo.commit_file("keep.txt", "x\n", "init");
        repo.write("u.txt", "alpha\nbeta\ngamma\n");
        // `diff --no-index` exits 1 when the files differ, so it can't go through `repo.git`.
        let mut args = vec!["diff", "--no-index"];
        args.extend_from_slice(DIFF_ARGS);
        args.extend_from_slice(&["--", "/dev/null", "u.txt"]); // portability: allow
        let out = hermetic_git(repo.path()).args(&args).output().expect("spawn git");
        assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
        let files = parse(&out.stdout);
        assert_eq!(files[0].new_path.as_deref(), Some("u.txt"));
        let patch = patch_for(&files[0], 0, &Selection::Lines(1..=1), false).expect("patch");
        stage(&repo, &patch);
        assert_eq!(staged_bytes(&repo, "u.txt"), b"beta\n");
    }

    #[test]
    fn diff_args_override_user_diff_config() {
        for config in [["diff.noprefix", "true"], ["diff.mnemonicPrefix", "true"]] {
            let mut repo = TempRepo::new();
            repo.write(".gitattributes", "*.rs diff=conv\n");
            repo.write("x.rs", "top level\n");
            repo.commit_file("src/x.rs", "one\ntwo\nthree\n", "init");
            repo.git(&["config", config[0], config[1]]);
            repo.git(&["config", "color.ui", "always"]);
            repo.git(&["config", "diff.external", "amalgum-no-such-diff-tool"]);
            repo.git(&["config", "diff.conv.textconv", "amalgum-no-such-textconv"]);
            repo.write("src/x.rs", "one\nTWO\nthree\n");

            let files = diff_of(&repo, "diff", &[]);
            assert_eq!(files.len(), 1, "{config:?}");
            assert_eq!(files[0].old_path.as_deref(), Some("src/x.rs"), "{config:?}");
            assert_eq!(files[0].new_path.as_deref(), Some("src/x.rs"), "{config:?}");
            let patch = patch_for(&files[0], 0, &Selection::WholeHunk, false).expect("patch");
            stage(&repo, &patch);
            assert_eq!(staged_bytes(&repo, "src/x.rs"), b"one\nTWO\nthree\n", "{config:?}");
            assert_eq!(staged_bytes(&repo, "x.rs"), b"top level\n", "{config:?}");

            let commit = repo.commit("edit");
            let files = diff_of(&repo, "show", &["--format=", &commit, "--", "src/x.rs"]);
            assert_eq!(files.len(), 1, "{config:?}");
            assert_eq!(files[0].new_path.as_deref(), Some("src/x.rs"), "{config:?}");
            assert_eq!(files[0].hunks.len(), 1, "{config:?}");
        }
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
        assert!(old.is_empty());
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
