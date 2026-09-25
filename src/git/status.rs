//! `git status --porcelain=v2 -z --branch --show-stash` parsing and in-progress operation
//! detection (§5.7, §5.17).

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Branch {
    /// `None` before the first commit (`# branch.oid (initial)`).
    pub oid: Option<String>,
    /// `None` when detached.
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Ordinary,
    Renamed { from: String },
    Copied { from: String },
    Unmerged,
    Untracked,
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub kind: EntryKind,
    /// Index status (`X`), `.` when unchanged. `?`/`!` for untracked/ignored.
    pub index: char,
    /// Worktree status (`Y`), `.` when unchanged.
    pub worktree: char,
}

impl Entry {
    pub fn is_staged(&self) -> bool {
        self.index != '.'
            && !matches!(self.kind, EntryKind::Untracked | EntryKind::Ignored | EntryKind::Unmerged)
    }
    /// Unstaged list includes untracked files (§5.7).
    pub fn is_unstaged(&self) -> bool {
        matches!(self.kind, EntryKind::Untracked)
            || (self.worktree != '.' && !matches!(self.kind, EntryKind::Unmerged | EntryKind::Ignored))
    }
    pub fn is_conflicted(&self) -> bool {
        matches!(self.kind, EntryKind::Unmerged)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: Branch,
    pub entries: Vec<Entry>,
    pub stash_count: u32,
}

impl Status {
    /// "N changed" in the status bar and sidebar: distinct paths, ignored excluded.
    pub fn changed_count(&self) -> usize {
        self.entries.iter().filter(|e| !matches!(e.kind, EntryKind::Ignored)).count()
    }
}

pub const STATUS_ARGS: &[&str] =
    &["status", "--porcelain=v2", "-z", "--branch", "--show-stash", "--untracked-files=all"];

/// Parse porcelain v2 `-z` output. Unknown line types are skipped (forward compatible).
pub fn parse(out: &[u8]) -> Result<Status, String> {
    let mut status = Status::default();

    // Records are NUL-terminated; a rename/copy record (`2 ...`) is followed by one extra
    // NUL-terminated `origPath` token. Splitting on `\0` never breaks a path in two, since `-z`
    // uses NUL (not newline) as the record separator — a literal newline inside a path survives.
    let mut tokens: Vec<&[u8]> = out.split(|&b| b == b'\0').collect();
    if tokens.last().is_some_and(|t| t.is_empty()) {
        tokens.pop();
    }

    let mut i = 0;
    while i < tokens.len() {
        let raw = tokens[i];
        if raw.is_empty() {
            // A blank record only appears here if a prior `2` record's origPath token was cut
            // off mid-stream: everything after it would misalign, so this is not safe to skip.
            return Err("git status: truncated record".to_string());
        }
        let line = String::from_utf8_lossy(raw);

        if let Some(rest) = line.strip_prefix("# ") {
            parse_header(rest, &mut status);
            i += 1;
        } else if let Some(rest) = line.strip_prefix("1 ") {
            if let Some(entry) = parse_ordinary(rest) {
                status.entries.push(entry);
            }
            i += 1;
        } else if let Some(rest) = line.strip_prefix("2 ") {
            let Some(&orig_raw) = tokens.get(i + 1) else {
                return Err("git status: rename/copy record missing origPath".to_string());
            };
            let orig_path = String::from_utf8_lossy(orig_raw).into_owned();
            if let Some(entry) = parse_rename_or_copy(rest, orig_path) {
                status.entries.push(entry);
            }
            i += 2;
        } else if let Some(rest) = line.strip_prefix("u ") {
            if let Some(entry) = parse_unmerged(rest) {
                status.entries.push(entry);
            }
            i += 1;
        } else if let Some(path) = line.strip_prefix("? ") {
            status.entries.push(Entry {
                path: path.to_string(),
                kind: EntryKind::Untracked,
                index: '?',
                worktree: '?',
            });
            i += 1;
        } else if let Some(path) = line.strip_prefix("! ") {
            status.entries.push(Entry {
                path: path.to_string(),
                kind: EntryKind::Ignored,
                index: '!',
                worktree: '!',
            });
            i += 1;
        } else {
            // Unknown line type: skip for forward compatibility.
            i += 1;
        }
    }

    Ok(status)
}

fn parse_header(rest: &str, status: &mut Status) {
    if let Some(v) = rest.strip_prefix("branch.oid ") {
        status.branch.oid = if v == "(initial)" { None } else { Some(v.to_string()) };
    } else if let Some(v) = rest.strip_prefix("branch.head ") {
        status.branch.head = if v == "(detached)" { None } else { Some(v.to_string()) };
    } else if let Some(v) = rest.strip_prefix("branch.upstream ") {
        status.branch.upstream = Some(v.to_string());
    } else if let Some(v) = rest.strip_prefix("branch.ab ") {
        let mut parts = v.split_whitespace();
        status.branch.ahead =
            parts.next().and_then(|p| p.strip_prefix('+')).and_then(|n| n.parse().ok()).unwrap_or(0);
        status.branch.behind =
            parts.next().and_then(|p| p.strip_prefix('-')).and_then(|n| n.parse().ok()).unwrap_or(0);
    } else if let Some(v) = rest.strip_prefix("stash ") {
        status.stash_count = v.parse().unwrap_or(0);
    }
    // Unknown header keys (e.g. a future `# branch.foo`) are skipped.
}

/// `XY sub mH mI mW hH hI path`.
fn parse_ordinary(rest: &str) -> Option<Entry> {
    let mut f = rest.splitn(8, ' ');
    let xy = f.next()?;
    for _ in 0..5 {
        f.next()?; // sub, mH, mI, mW, hH
    }
    f.next()?; // hI
    let path = f.next()?;
    let mut xy = xy.chars();
    Some(Entry { path: path.to_string(), kind: EntryKind::Ordinary, index: xy.next()?, worktree: xy.next()? })
}

/// `XY sub mH mI mW hH hI Xscore path` (origPath comes from the next `-z` token).
fn parse_rename_or_copy(rest: &str, orig_path: String) -> Option<Entry> {
    let mut f = rest.splitn(9, ' ');
    let xy = f.next()?;
    for _ in 0..5 {
        f.next()?; // sub, mH, mI, mW, hH
    }
    f.next()?; // hI
    let score = f.next()?;
    let path = f.next()?;
    let kind = match score.chars().next()? {
        'R' => EntryKind::Renamed { from: orig_path },
        'C' => EntryKind::Copied { from: orig_path },
        _ => return None,
    };
    let mut xy = xy.chars();
    Some(Entry { path: path.to_string(), kind, index: xy.next()?, worktree: xy.next()? })
}

/// `XY sub m1 m2 m3 mW h1 h2 h3 path`.
fn parse_unmerged(rest: &str) -> Option<Entry> {
    let mut f = rest.splitn(10, ' ');
    let xy = f.next()?;
    for _ in 0..7 {
        f.next()?; // sub, m1, m2, m3, mW, h1, h2
    }
    f.next()?; // h3
    let path = f.next()?;
    let mut xy = xy.chars();
    Some(Entry { path: path.to_string(), kind: EntryKind::Unmerged, index: xy.next()?, worktree: xy.next()? })
}

/// An operation that stopped midway and owns the working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoOp {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl RepoOp {
    /// "Merge", "Rebase", "Cherry-pick", "Revert" — for "Merge in progress · 3 conflicts".
    pub fn name(self) -> &'static str {
        match self {
            RepoOp::Merge => "Merge",
            RepoOp::Rebase => "Rebase",
            RepoOp::CherryPick => "Cherry-pick",
            RepoOp::Revert => "Revert",
        }
    }
    pub fn continue_args(self) -> &'static [&'static str] {
        match self {
            RepoOp::Merge => &["merge", "--continue"],
            RepoOp::Rebase => &["rebase", "--continue"],
            RepoOp::CherryPick => &["cherry-pick", "--continue"],
            RepoOp::Revert => &["revert", "--continue"],
        }
    }
    pub fn abort_args(self) -> &'static [&'static str] {
        match self {
            RepoOp::Merge => &["merge", "--abort"],
            RepoOp::Rebase => &["rebase", "--abort"],
            RepoOp::CherryPick => &["cherry-pick", "--abort"],
            RepoOp::Revert => &["revert", "--abort"],
        }
    }
}

/// Detect an in-progress operation from files in the git dir (`MERGE_HEAD`, `rebase-merge/`,
/// `rebase-apply/`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`). `exists` answers for a path relative
/// to the git dir, so this works locally and over ssh alike.
pub fn detect_op(exists: impl Fn(&str) -> bool) -> Option<RepoOp> {
    if exists("rebase-merge") || exists("rebase-apply") {
        Some(RepoOp::Rebase)
    } else if exists("MERGE_HEAD") {
        Some(RepoOp::Merge)
    } else if exists("CHERRY_PICK_HEAD") {
        Some(RepoOp::CherryPick)
    } else if exists("REVERT_HEAD") {
        Some(RepoOp::Revert)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;

    fn entry<'a>(status: &'a Status, path: &str) -> &'a Entry {
        status.entries.iter().find(|e| e.path == path).unwrap_or_else(|| panic!("no entry for {path}"))
    }

    #[test]
    fn parses_staged_unstaged_untracked() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "hello\n", "init");
        repo.write("a.txt", "hello\nworld\n"); // unstaged
        repo.git(&["add", "-A"]);
        repo.write("a.txt", "hello\nworld\nmore\n"); // now also unstaged on top of staged
        repo.write("notes.txt", "new"); // untracked

        let out = repo.git_raw(STATUS_ARGS);
        let status = parse(&out).expect("parse");

        assert_eq!(status.branch.head.as_deref(), Some("main"));
        assert!(status.branch.oid.is_some());
        assert_eq!(status.branch.ahead, 0);
        assert_eq!(status.branch.behind, 0);

        let a = entry(&status, "a.txt");
        assert!(a.is_staged());
        assert!(a.is_unstaged());
        assert!(!a.is_conflicted());

        let notes = entry(&status, "notes.txt");
        assert_eq!(notes.kind, EntryKind::Untracked);
        assert!(!notes.is_staged());
        assert!(notes.is_unstaged());

        assert_eq!(status.changed_count(), 2);
    }

    #[test]
    fn ignored_files_are_excluded_from_changed_count() {
        let mut repo = TempRepo::new();
        repo.commit_file(".gitignore", "*.log\n", "init");
        repo.write("boom.log", "x");

        let out = repo.git_raw(&[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--ignored",
            "--untracked-files=all",
        ]);
        let status = parse(&out).expect("parse");

        let ignored = entry(&status, "boom.log");
        assert_eq!(ignored.kind, EntryKind::Ignored);
        assert!(!ignored.is_staged());
        assert!(!ignored.is_unstaged());
        assert_eq!(status.changed_count(), 0);
    }

    #[test]
    fn parses_rename_from_real_git_mv() {
        let mut repo = TempRepo::new();
        repo.commit_file("orig.txt", "line1\nline2\n", "init");
        repo.git(&["mv", "orig.txt", "renamed.txt"]);

        let out = repo.git_raw(STATUS_ARGS);
        let status = parse(&out).expect("parse");

        let renamed = entry(&status, "renamed.txt");
        assert_eq!(renamed.kind, EntryKind::Renamed { from: "orig.txt".to_string() });
        assert!(renamed.is_staged());
        assert!(!renamed.is_unstaged());
    }

    #[test]
    fn parses_copy_from_hand_written_fixture() {
        // Copy detection needs `status.renames = copies` or an explicit `-C`/`find-copies`; it's
        // not exercised by a plain TempRepo flow, so this is a hand-written porcelain v2 record.
        let out = b"# branch.oid abc123\0# branch.head main\0\
2 C. N... 100644 100644 100644 aaaa aaaa C100 new.txt\0orig.txt\0";
        let status = parse(out).expect("parse");
        let copied = entry(&status, "new.txt");
        assert_eq!(copied.kind, EntryKind::Copied { from: "orig.txt".to_string() });
        assert_eq!(copied.index, 'C');
        assert_eq!(copied.worktree, '.');
    }

    #[test]
    fn parses_real_merge_conflict() {
        let mut repo = TempRepo::new();
        repo.commit_file("f.txt", "line1\n", "init");
        repo.git(&["checkout", "-q", "-b", "feat"]);
        repo.commit_file("f.txt", "line1-feat\n", "feat change");
        repo.git(&["checkout", "-q", "main"]);
        repo.commit_file("f.txt", "line1-main\n", "main change");
        // Merge is expected to fail (conflict); run hermetic_git directly to ignore the exit code.
        let _ = crate::testutil::hermetic_git(repo.path())
            .args(["-c", "commit.gpgsign=false", "merge", "--no-edit", "feat"])
            .output()
            .expect("spawn git");

        let out = repo.git_raw(STATUS_ARGS);
        let status = parse(&out).expect("parse");

        let f = entry(&status, "f.txt");
        assert_eq!(f.kind, EntryKind::Unmerged);
        assert!(f.is_conflicted());
        assert!(!f.is_staged());
        assert!(!f.is_unstaged());
    }

    #[test]
    fn parses_detached_head() {
        let mut repo = TempRepo::new();
        repo.commit_file("f.txt", "1\n", "c1");
        repo.commit_file("f.txt", "2\n", "c2");
        repo.git(&["checkout", "-q", "HEAD~1"]);

        let out = repo.git_raw(STATUS_ARGS);
        let status = parse(&out).expect("parse");

        assert_eq!(status.branch.head, None);
        assert!(status.branch.oid.is_some());
    }

    #[test]
    fn parses_initial_repo_with_no_commits() {
        let repo = TempRepo::new();
        repo.write("f.txt", "x");
        repo.git(&["add", "-A"]);

        let out = repo.git_raw(STATUS_ARGS);
        let status = parse(&out).expect("parse");

        assert_eq!(status.branch.oid, None);
        assert_eq!(status.branch.head.as_deref(), Some("main"));
        assert!(entry(&status, "f.txt").is_staged());
    }

    #[test]
    fn parses_ahead_behind_upstream() {
        // `TempRepo` only inits non-bare repos, and pushing into a non-bare repo's checked-out
        // branch is refused by default; use a real bare repo as the remote, matching how
        // `git clone` sets up `branch.main.{remote,merge}` (and thus `# branch.upstream`).
        let base = tempfile::tempdir().expect("tempdir");
        let remote = base.path().join("remote.git");
        let out = crate::testutil::hermetic_git(base.path())
            .args(["init", "-q", "-b", "main", "--bare", remote.to_str().expect("utf8 path")])
            .output()
            .expect("spawn git");
        assert!(out.status.success());

        let repo_a = TempRepoLike::clone_from(&remote, base.path().join("a"));
        repo_a.commit("a.txt", "1\n", "c1", 100);
        repo_a.git(&["push", "-q", "-u", "origin", "main"]);

        let repo_b = TempRepoLike::clone_from(&remote, base.path().join("b"));

        // `a` moves ahead and pushes; `b` commits locally, then fetches without merging, so it
        // ends up both ahead (its local commit) and behind (a's pushed commit).
        repo_a.commit("a.txt", "2\n", "c2", 200);
        repo_a.git(&["push", "-q", "origin", "main"]);
        repo_b.commit("b.txt", "x", "local commit", 300);
        repo_b.git(&["fetch", "-q", "origin"]);

        let out = repo_b.git_raw(STATUS_ARGS);
        let status = parse(&out).expect("parse");

        assert_eq!(status.branch.upstream.as_deref(), Some("origin/main"));
        assert_eq!(status.branch.ahead, 1);
        assert_eq!(status.branch.behind, 1);
    }

    #[test]
    fn parses_stash_count_header() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n", "c1");
        repo.write("a.txt", "2\n");
        repo.git(&["stash", "push", "-q", "-m", "s1"]);

        let out = repo.git_raw(STATUS_ARGS);
        let status = parse(&out).expect("parse");
        assert_eq!(status.stash_count, 1);
    }

    #[test]
    fn paths_with_spaces_unicode_and_newlines_survive() {
        let mut repo = TempRepo::new();
        repo.commit_file("seed.txt", "x", "init");
        repo.write("with space.txt", "hi");
        repo.write("unicode-café-🎉.txt", "hi");
        repo.write("weird\nname.txt", "hi");
        repo.git(&["add", "-A"]);

        let out = repo.git_raw(STATUS_ARGS);
        let status = parse(&out).expect("parse");

        assert!(status.entries.iter().any(|e| e.path == "with space.txt"));
        assert!(status.entries.iter().any(|e| e.path == "unicode-café-🎉.txt"));
        assert!(status.entries.iter().any(|e| e.path == "weird\nname.txt"));
    }

    #[test]
    fn unknown_line_types_are_skipped() {
        let out = b"# branch.oid abc\0# branch.head main\0# branch.future-thing hi\0? new.txt\0";
        let status = parse(out).expect("parse");
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].path, "new.txt");
    }

    #[test]
    fn truncated_rename_record_is_an_error() {
        let out = b"# branch.oid abc\0# branch.head main\x002 R. N... 100644 100644 100644 h h R100 new.txt";
        assert!(parse(out).is_err());
    }

    #[test]
    fn empty_output_parses_to_default_status() {
        let status = parse(b"").expect("parse");
        assert_eq!(status, Status::default());
    }

    #[test]
    fn repo_op_command_tables() {
        assert_eq!(RepoOp::Merge.name(), "Merge");
        assert_eq!(RepoOp::Rebase.name(), "Rebase");
        assert_eq!(RepoOp::CherryPick.name(), "Cherry-pick");
        assert_eq!(RepoOp::Revert.name(), "Revert");
        assert_eq!(RepoOp::Merge.continue_args(), &["merge", "--continue"]);
        assert_eq!(RepoOp::Rebase.abort_args(), &["rebase", "--abort"]);
    }

    #[test]
    fn detect_op_precedence() {
        // rebase-merge/rebase-apply > MERGE_HEAD > CHERRY_PICK_HEAD > REVERT_HEAD.
        let all = |present: &[&str]| {
            let present: Vec<String> = present.iter().map(|s| s.to_string()).collect();
            move |p: &str| present.iter().any(|s| s == p)
        };
        assert_eq!(detect_op(all(&["rebase-merge", "MERGE_HEAD"])), Some(RepoOp::Rebase));
        assert_eq!(detect_op(all(&["MERGE_HEAD", "CHERRY_PICK_HEAD"])), Some(RepoOp::Merge));
        assert_eq!(detect_op(all(&["CHERRY_PICK_HEAD", "REVERT_HEAD"])), Some(RepoOp::CherryPick));
        assert_eq!(detect_op(all(&["REVERT_HEAD"])), Some(RepoOp::Revert));
        assert_eq!(detect_op(all(&[])), None);
    }

    #[test]
    fn detect_op_finds_real_mid_merge_repo() {
        let mut repo = TempRepo::new();
        repo.commit_file("f.txt", "line1\n", "init");
        repo.git(&["checkout", "-q", "-b", "feat"]);
        repo.commit_file("f.txt", "line1-feat\n", "feat change");
        repo.git(&["checkout", "-q", "main"]);
        repo.commit_file("f.txt", "line1-main\n", "main change");
        let _ = crate::testutil::hermetic_git(repo.path())
            .args(["-c", "commit.gpgsign=false", "merge", "--no-edit", "feat"])
            .output()
            .expect("spawn git");

        let git_dir = repo.path().join(".git");
        let op = detect_op(|p| git_dir.join(p).exists());
        assert_eq!(op, Some(RepoOp::Merge));
    }

    /// Minimal helper so `parses_ahead_behind_upstream` can drive a clone of a real bare remote
    /// (not the non-bare-only `TempRepo::new`) with the same hermetic-git conveniences.
    struct TempRepoLike {
        dir: std::path::PathBuf,
    }

    impl TempRepoLike {
        fn clone_from(remote: &std::path::Path, dir: std::path::PathBuf) -> Self {
            let out = crate::testutil::hermetic_git(remote)
                .args(["clone", "-q", remote.to_str().expect("utf8 path"), dir.to_str().expect("utf8 path")])
                .output()
                .expect("spawn git");
            assert!(out.status.success(), "clone failed:\n{}", String::from_utf8_lossy(&out.stderr));
            Self { dir }
        }

        fn git(&self, args: &[&str]) -> String {
            let out = crate::testutil::hermetic_git(&self.dir).args(args).output().expect("spawn git");
            assert!(out.status.success(), "git {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim_end().to_string()
        }

        fn git_raw(&self, args: &[&str]) -> Vec<u8> {
            let out = crate::testutil::hermetic_git(&self.dir).args(args).output().expect("spawn git");
            assert!(out.status.success(), "git {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
            out.stdout
        }

        fn commit(&self, rel: &str, contents: &str, message: &str, clock: u64) {
            std::fs::write(self.dir.join(rel), contents).expect("write");
            self.git(&["add", "-A"]);
            let date = format!("@{} +0000", 1_700_000_000 + clock);
            let out = crate::testutil::hermetic_git(&self.dir)
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date)
                .args(["commit", "-q", "-m", message])
                .output()
                .expect("spawn git");
            assert!(out.status.success(), "commit failed:\n{}", String::from_utf8_lossy(&out.stderr));
        }
    }
}
