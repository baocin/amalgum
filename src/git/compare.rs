//! Compare two refs (§5.13, W11) and the multi-commit selection's combined diff (§5.14): argv
//! builders, pure parsers, and the small loaders that chain them on a worker thread. Every
//! command goes through [`Git::run`], so a remote workspace compares over ssh like a local one.
//!
//! - Compare `A`…`B`: **Files** is `git diff A...B` (three-dot, changes on `B` since the merge
//!   base) or `git diff A..B` (two-dot, the trees themselves); **Commits** is `git log A..B`,
//!   the commits in `B` that are not in `A`.
//! - A contiguous selection's combined diff is `git diff <oldest>~1 <newest>`; when the oldest
//!   commit is a root commit, `<oldest>~1` does not exist and the empty tree stands in for it.

use super::cmd::{Git, GitError};
use super::diff::{self, FileChange, FileDiff};
use super::log::{self, Commit};
use super::rebase::{Action, Plan};

/// The empty tree of a SHA-1 repository: the base of a root commit's diff.
pub const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// The empty tree of a SHA-256 repository.
pub const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

/// At most this many commits are listed on the Commits tab; the header's count is exact.
pub const COMMIT_LIST_LIMIT: usize = 1000;

/// §5.13 "three-dot by default, toggle to two-dot".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiffMode {
    /// `A...B`: what `B` changed since it forked from `A`.
    #[default]
    ThreeDot,
    /// `A..B`: `A`'s tree against `B`'s.
    TwoDot,
}

impl DiffMode {
    pub fn range(self, a: &str, b: &str) -> String {
        match self {
            DiffMode::ThreeDot => format!("{a}...{b}"),
            DiffMode::TwoDot => format!("{a}..{b}"),
        }
    }
}

/// Whether `rev` can be handed to git as a revision: anything `rev-parse` accepts, except text
/// git would read as an option (a leading `-`) or that would change the range being built (a
/// `..`), and nothing with whitespace or control characters. The picker's live error.
pub fn rev_error(rev: &str) -> Option<String> {
    let rev = rev.trim();
    if rev.is_empty() {
        return Some("Enter a branch, tag, or commit".into());
    }
    if rev.starts_with('-') {
        return Some("A revision can't start with `-`".into());
    }
    if rev.contains("..") {
        return Some("Enter one revision, not a range".into());
    }
    if rev.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Some("A revision can't contain spaces".into());
    }
    None
}

/// `git rev-parse` resolving `rev` to the commit it names (tags peeled).
pub fn resolve_args(rev: &str) -> Vec<String> {
    vec!["rev-parse".into(), "--verify".into(), "--quiet".into(), format!("{rev}^{{commit}}")]
}

/// The object id [`resolve_args`] printed, if it is one.
pub fn parse_resolved(out: &[u8]) -> Option<String> {
    let id = String::from_utf8_lossy(out).trim().to_string();
    (matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

/// The changed files between `a` and `b` (`--raw --numstat`, for [`diff::parse_raw_numstat`]).
pub fn files_args(a: &str, b: &str, mode: DiffMode) -> Vec<String> {
    let mut args = vec!["diff".to_string()];
    args.extend(diff::RAW_NUMSTAT_ARGS.iter().map(|s| s.to_string()));
    args.push(mode.range(a, b));
    args.push("--".into());
    args
}

/// The pathspec of one file's diff: a rename's old path too, or git can't pair the two and
/// shows the new path as an added file.
fn file_pathspec(args: &mut Vec<String>, path: &str, old_path: Option<&str>) {
    args.extend(["-M".to_string(), "--".into(), path.to_string()]);
    args.extend(old_path.filter(|o| *o != path).map(str::to_string));
}

/// One file's diff between `a` and `b` (for [`diff::parse`]); `old_path` is a rename's source.
pub fn file_diff_args(a: &str, b: &str, mode: DiffMode, path: &str, old_path: Option<&str>) -> Vec<String> {
    let mut args = vec!["diff".to_string()];
    args.extend(diff::DIFF_ARGS.iter().map(|s| s.to_string()));
    args.push(mode.range(a, b));
    file_pathspec(&mut args, path, old_path);
    args
}

/// The commits in `b` that are not in `a`, newest first, at most `limit` (for [`log::parse_log`]).
pub fn commits_args(a: &str, b: &str, limit: usize) -> Vec<String> {
    let range = format!("{a}..{b}");
    log::log_args(&["-n", &limit.to_string(), &range, "--"])
}

/// How many commits are in `b` but not in `a`.
pub fn count_args(a: &str, b: &str) -> Vec<String> {
    vec!["rev-list".into(), "--count".into(), format!("{a}..{b}"), "--".into()]
}

/// The number [`count_args`] printed.
pub fn parse_count(out: &[u8]) -> Option<usize> {
    String::from_utf8_lossy(out).trim().parse().ok()
}

/// The base of a contiguous selection's combined diff (§5.14 `git diff <oldest>~1..<newest>`):
/// `<oldest>~1`, or the empty tree when `oldest` is a root commit (the object format follows
/// from the id's length).
pub fn range_base(oldest: &str, oldest_is_root: bool) -> String {
    match (oldest_is_root, oldest.len()) {
        (false, _) => format!("{oldest}~1"),
        (true, 64) => EMPTY_TREE_SHA256.to_string(),
        (true, _) => EMPTY_TREE_SHA1.to_string(),
    }
}

/// The files a contiguous range changed, `base` from [`range_base`].
pub fn range_files_args(base: &str, newest: &str) -> Vec<String> {
    let mut args = vec!["diff".to_string()];
    args.extend(diff::RAW_NUMSTAT_ARGS.iter().map(|s| s.to_string()));
    args.extend([base.to_string(), newest.to_string(), "--".into()]);
    args
}

/// One file's combined diff over a contiguous range.
pub fn range_file_diff_args(base: &str, newest: &str, path: &str, old_path: Option<&str>) -> Vec<String> {
    let mut args = vec!["diff".to_string()];
    args.extend(diff::DIFF_ARGS.iter().map(|s| s.to_string()));
    args.extend([base.to_string(), newest.to_string()]);
    file_pathspec(&mut args, path, old_path);
    args
}

/// A merge commit's changes against its first parent: plain `diff-tree` prints nothing for a
/// merge and `-m` alone one diff per parent. `-m --first-parent` on `show` means this in every
/// git (`--diff-merges=first-parent` would need 2.31; `diff-tree` ignores `--first-parent`).
const FIRST_PARENT: [&str; 2] = ["-m", "--first-parent"];

/// The files one commit changed (against its first parent, merges included; everything for a
/// root commit).
pub fn commit_files_args(id: &str) -> Vec<String> {
    let mut args: Vec<String> = ["show", "--format=", "--no-abbrev"].map(String::from).to_vec();
    args.extend(FIRST_PARENT.map(String::from));
    args.extend(diff::RAW_NUMSTAT_ARGS.iter().map(|s| s.to_string()));
    args.extend([id.to_string(), "--".into()]);
    args
}

/// One file's diff in one commit, against its first parent like [`commit_files_args`].
pub fn commit_file_diff_args(id: &str, path: &str, old_path: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec!["show".into(), "--format=".into()];
    args.extend(FIRST_PARENT.map(String::from));
    args.extend(diff::DIFF_ARGS.iter().map(|s| s.to_string()));
    args.push(id.to_string());
    file_pathspec(&mut args, path, old_path);
    args
}

/// Whether `commits` (newest first, `(id, parents)`) are one unbroken first-parent line of
/// history: every commit but the oldest has exactly one parent, the next one in the list. Only
/// then is `<oldest>~1..<newest>` their combined change.
pub fn is_linear_chain(commits: &[(&str, &[String])]) -> bool {
    !commits.is_empty() && commits.windows(2).all(|w| w[0].1.len() == 1 && w[0].1[0] == w[1].0)
}

/// Whether §5.14 **Squash into one** may be offered for `commits` (newest first,
/// `(id, parents)`) with HEAD at `head`: one first-parent line of non-merge commits whose newest
/// *is* HEAD. [`squash_plan`] lists only the selection, and the rebase replaces git's whole todo
/// with it, so any commit between the newest selected one and HEAD would be dropped.
pub fn can_squash(commits: &[(&str, &[String])], head: Option<&str>) -> bool {
    commits.len() > 1
        && is_linear_chain(commits)
        && commits.iter().all(|(_, parents)| parents.len() <= 1)
        && head.is_some_and(|h| commits[0].0 == h)
}

/// §5.14 **Squash into one**: an interactive rebase plan over `commits` (oldest first,
/// `(id, subject)`) with all but the first marked `squash`.
pub fn squash_plan(commits: &[(String, String)]) -> Plan {
    let mut plan = Plan::from_commits(commits);
    for step in plan.steps.iter_mut().skip(1) {
        step.action = Action::Squash;
    }
    plan
}

/// The upstream a squash's `git rebase -i` starts from: `<oldest>~1`, or `None` for `--root`.
pub fn squash_base(oldest: &str, oldest_is_root: bool) -> Option<String> {
    (!oldest_is_root).then(|| format!("{oldest}~1"))
}

/// Paths changed by any of `groups`, each counted once (the "18 files" of §5.14).
pub fn distinct_paths<'a>(groups: impl IntoIterator<Item = &'a [FileChange]>) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    for files in groups {
        for f in files {
            seen.insert(f.path.as_str());
        }
    }
    seen.len()
}

// ---- loaders (worker thread) -------------------------------------------------------------------

/// Everything the compare view shows for `A`…`B`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub a_id: String,
    pub b_id: String,
    /// The Files tab; an error of its own, so unrelated histories (no merge base for three-dot)
    /// still show the commits and their count.
    pub files: Result<Vec<FileChange>, GitError>,
    /// Newest first, at most [`COMMIT_LIST_LIMIT`].
    pub commits: Vec<Commit>,
    /// All commits in `B` not in `A`.
    pub commit_count: usize,
}

fn run(git: &Git, args: &[String]) -> Result<Vec<u8>, GitError> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git.run(&args)
}

/// Resolve `rev` to a commit id; an unknown revision is an error naming it.
pub fn resolve(git: &Git, rev: &str) -> Result<String, GitError> {
    let unknown = |code| GitError {
        command: format!("git rev-parse --verify {rev}^{{commit}}"),
        code,
        stderr: format!("`{rev}` is not a branch, tag, or commit in this repository"),
    };
    if let Some(e) = rev_error(rev) {
        return Err(GitError { stderr: e, ..unknown(None) });
    }
    match run(git, &resolve_args(rev.trim())) {
        Ok(out) => parse_resolved(&out).ok_or_else(|| unknown(None)),
        Err(e) if e.code == Some(1) || e.code == Some(128) => Err(unknown(e.code)),
        Err(e) => Err(e),
    }
}

/// Resolve both sides, then read the file list and the commits of `A`…`B`. The ids are pinned
/// first, so every later command sees the same two commits even if a ref moves meanwhile.
pub fn load(git: &Git, a: &str, b: &str, mode: DiffMode) -> Result<Comparison, GitError> {
    let a_id = resolve(git, a)?;
    let b_id = resolve(git, b)?;
    let commits = log::parse_log(&run(git, &commits_args(&a_id, &b_id, COMMIT_LIST_LIMIT))?);
    let commit_count = parse_count(&run(git, &count_args(&a_id, &b_id))?).unwrap_or(commits.len());
    let files = run(git, &files_args(&a_id, &b_id, mode)).map(|out| diff::parse_raw_numstat(&out));
    Ok(Comparison { a_id, b_id, files, commits, commit_count })
}

/// One file's diff in the comparison of the (resolved) ids `a` and `b`.
pub fn load_file_diff(
    git: &Git,
    a: &str,
    b: &str,
    mode: DiffMode,
    path: &str,
    old_path: Option<&str>,
) -> Result<Vec<FileDiff>, GitError> {
    Ok(diff::parse(&run(git, &file_diff_args(a, b, mode, path, old_path))?))
}

/// What the Files tab of a multi-commit selection lists (§5.14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionFiles {
    /// A contiguous range: the combined change `base`..`newest`.
    Combined { base: String, newest: String, files: Vec<FileChange> },
    /// Anything else: each commit's own files, newest first, `(commit id, files)`.
    PerCommit(Vec<(String, Vec<FileChange>)>),
}

impl SelectionFiles {
    /// Distinct paths across the selection.
    pub fn file_count(&self) -> usize {
        match self {
            SelectionFiles::Combined { files, .. } => files.len(),
            SelectionFiles::PerCommit(groups) => distinct_paths(groups.iter().map(|(_, f)| f.as_slice())),
        }
    }
}

/// The combined file list of the contiguous range `oldest`..=`newest`.
pub fn load_range_files(
    git: &Git,
    oldest: &str,
    oldest_is_root: bool,
    newest: &str,
) -> Result<SelectionFiles, GitError> {
    let base = range_base(oldest, oldest_is_root);
    let files = diff::parse_raw_numstat(&run(git, &range_files_args(&base, newest))?);
    Ok(SelectionFiles::Combined { base, newest: newest.to_string(), files })
}

/// Each of `ids`' own file lists, in the order given.
pub fn load_per_commit_files(git: &Git, ids: &[String]) -> Result<SelectionFiles, GitError> {
    let mut groups = Vec::with_capacity(ids.len());
    for id in ids {
        groups.push((id.clone(), diff::parse_raw_numstat(&run(git, &commit_files_args(id))?)));
    }
    Ok(SelectionFiles::PerCommit(groups))
}

/// One file's combined diff over a contiguous range.
pub fn load_range_file_diff(
    git: &Git,
    base: &str,
    newest: &str,
    path: &str,
    old_path: Option<&str>,
) -> Result<Vec<FileDiff>, GitError> {
    Ok(diff::parse(&run(git, &range_file_diff_args(base, newest, path, old_path))?))
}

/// One file's diff in commit `id` (a group of a non-contiguous selection).
pub fn load_commit_file_diff(
    git: &Git,
    id: &str,
    path: &str,
    old_path: Option<&str>,
) -> Result<Vec<FileDiff>, GitError> {
    Ok(diff::parse(&run(git, &commit_file_diff_args(id, path, old_path))?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Location;
    use crate::testutil::TempRepo;

    fn git_for(repo: &TempRepo) -> Git {
        Git::new(Location::Local { path: repo.path().to_path_buf() })
    }

    /// main: m1 ─ m2 ─ m3(hotfix)
    ///               └ f1 ─ f2 ─ f3   (feat)
    fn forked() -> (TempRepo, Vec<String>) {
        let mut repo = TempRepo::new();
        let m1 = repo.commit_file("README.md", "one\n", "m1");
        let m2 = repo.commit_file("README.md", "one\ntwo\n", "m2");
        repo.git(&["checkout", "-q", "-b", "feat"]);
        let f1 = repo.commit_file("src/auth/oauth.rs", "fn oauth() {}\n", "f1");
        let f2 = repo.commit_file("src/auth/mod.rs", "ttl\n", "f2");
        let f3 = repo.commit_file("tests/with space.rs", "t\n", "f3");
        repo.git(&["checkout", "-q", "main"]);
        let m3 = repo.commit_file("hotfix.txt", "fix\n", "m3");
        repo.git(&["tag", "-a", "v1", "-m", "v1", &m3]);
        (repo, vec![m1, m2, m3, f1, f2, f3])
    }

    fn paths(files: &[FileChange]) -> Vec<&str> {
        files.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn three_dot_lists_only_what_b_changed_since_the_fork() {
        let (repo, _) = forked();
        let c = load(&git_for(&repo), "main", "feat", DiffMode::ThreeDot).expect("compare");
        assert_eq!(
            paths(c.files.as_ref().expect("files")),
            vec!["src/auth/mod.rs", "src/auth/oauth.rs", "tests/with space.rs"]
        );
        assert!(c.files.as_ref().expect("files").iter().all(|f| f.status == 'A'));
    }

    #[test]
    fn two_dot_also_shows_what_a_has_that_b_lacks() {
        let (repo, _) = forked();
        let c = load(&git_for(&repo), "main", "feat", DiffMode::TwoDot).expect("compare");
        let hotfix = c
            .files
            .as_ref()
            .expect("files")
            .iter()
            .find(|f| f.path == "hotfix.txt")
            .expect("main's hotfix shows as deleted");
        assert_eq!(hotfix.status, 'D');
        assert_eq!(c.files.as_ref().expect("files").len(), 4);
    }

    #[test]
    fn commits_are_those_in_b_not_in_a_newest_first() {
        let (repo, ids) = forked();
        let c = load(&git_for(&repo), "main", "feat", DiffMode::ThreeDot).expect("compare");
        let listed: Vec<&str> = c.commits.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(listed, vec![ids[5].as_str(), ids[4].as_str(), ids[3].as_str()]);
        assert_eq!(c.commit_count, 3);
        // And the other way round: only the hotfix.
        let back = load(&git_for(&repo), "feat", "main", DiffMode::ThreeDot).expect("compare");
        assert_eq!(back.commit_count, 1);
        assert_eq!(back.commits[0].subject, "m3");
    }

    #[test]
    fn any_ref_or_hash_resolves_and_tags_are_peeled() {
        let (repo, ids) = forked();
        let git = git_for(&repo);
        assert_eq!(resolve(&git, "v1").expect("tag"), ids[2]);
        assert_eq!(resolve(&git, &ids[3][..7]).expect("short hash"), ids[3]);
        assert_eq!(resolve(&git, "feat~1").expect("expression"), ids[4]);
        let err = resolve(&git, "nope").expect_err("unknown");
        assert!(err.stderr.contains("`nope`"), "{}", err.stderr);
        let err = resolve(&git, "--all").expect_err("option-like");
        assert!(err.stderr.contains("`-`"), "{}", err.stderr);
    }

    #[test]
    fn file_diff_follows_the_mode() {
        let (repo, ids) = forked();
        let git = git_for(&repo);
        let three =
            load_file_diff(&git, &ids[2], &ids[5], DiffMode::ThreeDot, "hotfix.txt", None).expect("diff");
        assert!(three.is_empty(), "the hotfix is not a change on feat");
        let two = load_file_diff(&git, &ids[2], &ids[5], DiffMode::TwoDot, "hotfix.txt", None).expect("diff");
        assert_eq!(two.len(), 1);
        let spaced = load_file_diff(&git, &ids[2], &ids[5], DiffMode::ThreeDot, "tests/with space.rs", None)
            .expect("diff");
        assert_eq!(spaced[0].new_path.as_deref(), Some("tests/with space.rs"));
    }

    #[test]
    fn unrelated_histories_fail_three_dot_with_gits_message() {
        let (repo, _) = forked();
        repo.git(&["checkout", "-q", "--orphan", "other"]);
        repo.git(&["rm", "-rq", "--cached", "."]);
        let mut repo = repo;
        repo.commit_file("x", "x\n", "orphan");
        let c =
            load(&git_for(&repo), "main", "other", DiffMode::ThreeDot).expect("commits need no merge base");
        assert!(!c.files.expect_err("no merge base").stderr.is_empty());
        assert_eq!(c.commit_count, 1, "the orphan commit");
        assert!(load(&git_for(&repo), "main", "other", DiffMode::TwoDot).expect("two-dot").files.is_ok());
    }

    #[test]
    fn contiguous_range_combines_changes_and_is_root_safe() {
        let (repo, ids) = forked();
        let git = git_for(&repo);
        // f1..=f3 (feat): three new files.
        let combined = load_range_files(&git, &ids[3], false, &ids[5]).expect("range");
        assert_eq!(combined.file_count(), 3);
        let SelectionFiles::Combined { base, newest, files } = &combined else { panic!("combined") };
        assert_eq!(base, &format!("{}~1", ids[3]));
        assert_eq!(paths(files), vec!["src/auth/mod.rs", "src/auth/oauth.rs", "tests/with space.rs"]);
        let d = load_range_file_diff(&git, base, newest, "src/auth/oauth.rs", None).expect("diff");
        assert_eq!(d.len(), 1);
        // m1..=m2 from the root commit: the empty tree stands in for m1~1.
        let root = load_range_files(&git, &ids[0], true, &ids[1]).expect("root range");
        let SelectionFiles::Combined { base, files, .. } = &root else { panic!("combined") };
        assert_eq!(base, EMPTY_TREE_SHA1);
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].status, files[0].added), ('A', Some(2)));
    }

    #[test]
    fn per_commit_groups_keep_each_commits_files() {
        let (repo, ids) = forked();
        let sel = load_per_commit_files(&git_for(&repo), &[ids[5].clone(), ids[2].clone(), ids[0].clone()])
            .expect("groups");
        let SelectionFiles::PerCommit(groups) = &sel else { panic!("per commit") };
        assert_eq!(groups.len(), 3);
        assert_eq!(paths(&groups[0].1), vec!["tests/with space.rs"]);
        assert_eq!(paths(&groups[1].1), vec!["hotfix.txt"]);
        assert_eq!(paths(&groups[2].1), vec!["README.md"], "a root commit lists its files too");
        assert_eq!(sel.file_count(), 3);
    }

    /// The rename's diff pairs both paths, in a compare, a range, and one commit.
    #[test]
    fn a_renamed_file_diffs_as_a_rename_not_an_addition() {
        let (mut repo, ids) = forked();
        repo.git(&["checkout", "-q", "feat"]);
        repo.git(&["mv", "src/auth/oauth.rs", "src/auth/login.rs"]);
        repo.git(&["commit", "-q", "-m", "mv"]);
        let mv = repo.commit_file("other.txt", "o\n", "after");
        let git = git_for(&repo);
        let c = load(&git, &ids[2], "feat", DiffMode::ThreeDot).expect("compare");
        let _ = &mut repo;
        let r =
            c.files.as_ref().expect("files").iter().find(|f| f.path == "src/auth/login.rs").expect("listed");
        assert_eq!((r.status, r.old_path.as_deref()), ('A', None), "added on feat since the fork");
        let c = load(&git, &ids[5], "feat", DiffMode::ThreeDot).expect("compare");
        let r =
            c.files.as_ref().expect("files").iter().find(|f| f.path == "src/auth/login.rs").expect("listed");
        assert_eq!((r.status, r.old_path.as_deref()), ('R', Some("src/auth/oauth.rs")));
        let old = r.old_path.as_deref();
        for d in [
            load_file_diff(&git, &c.a_id, &c.b_id, DiffMode::ThreeDot, &r.path, old).expect("diff"),
            load_range_file_diff(&git, &format!("{}~1", ids[5]), &mv, &r.path, old).expect("diff"),
            load_commit_file_diff(&git, &format!("{mv}~1"), &r.path, old).expect("diff"),
        ] {
            assert_eq!(d.len(), 1, "{d:?}");
            assert_eq!(d[0].old_path.as_deref(), Some("src/auth/oauth.rs"), "{d:?}");
            assert_eq!(d[0].new_path.as_deref(), Some("src/auth/login.rs"));
        }
    }

    /// A merge in a non-contiguous selection lists (and diffs) its changes against the first
    /// parent, not nothing and not one diff per parent.
    #[test]
    fn a_merge_commit_lists_its_first_parent_changes() {
        let (repo, ids) = forked();
        repo.git(&["merge", "-q", "--no-ff", "--no-edit", "feat"]);
        let merge = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        let git = git_for(&repo);
        let sel = load_per_commit_files(&git, std::slice::from_ref(&merge)).expect("groups");
        let SelectionFiles::PerCommit(groups) = &sel else { panic!("per commit") };
        assert_eq!(paths(&groups[0].1), vec!["src/auth/mod.rs", "src/auth/oauth.rs", "tests/with space.rs"]);
        assert!(!paths(&groups[0].1).contains(&"hotfix.txt"), "not the diff against feat");
        let d = load_commit_file_diff(&git, &merge, "src/auth/mod.rs", None).expect("diff");
        assert_eq!(d.len(), 1);
        let _ = ids;
    }

    /// Squashing c2..c3 under a tip c5 would drop c4 and c5 (the plan lists only the selection):
    /// it is offered only when the newest selected commit is HEAD.
    #[test]
    fn squash_is_offered_only_when_the_newest_selected_commit_is_head() {
        let p = |s: &str| vec![s.to_string()];
        let (c1, c2, c3) = (p("c1"), p("c2"), p("c3"));
        let sel: Vec<(&str, &[String])> = vec![("c3", &c2), ("c2", &c1)];
        assert!(!can_squash(&sel, Some("c5")), "c4 and c5 would be dropped");
        assert!(can_squash(&sel, Some("c3")));
        assert!(!can_squash(&sel, None), "detached or unknown HEAD");
        assert!(!can_squash(&sel[..1], Some("c3")), "one commit is nothing to squash");
        let gap: Vec<(&str, &[String])> = vec![("c4", &c3), ("c2", &c1)];
        assert!(!can_squash(&gap, Some("c4")), "not contiguous");
        let merge = vec!["c2".to_string(), "x".to_string()];
        let with_merge: Vec<(&str, &[String])> = vec![("c3", &merge), ("c2", &c1)];
        assert!(!can_squash(&with_merge, Some("c3")));
    }

    #[test]
    fn linear_chain_needs_single_first_parent_links() {
        let p = |s: &str| vec![s.to_string()];
        let (c, b, a) = (p("b"), p("a"), Vec::<String>::new());
        assert!(is_linear_chain(&[("c", &c), ("b", &b), ("a", &a)]));
        assert!(is_linear_chain(&[("c", &c)]));
        assert!(!is_linear_chain(&[]));
        let skip = p("x");
        assert!(!is_linear_chain(&[("c", &skip), ("b", &b)]), "c's parent is not b");
        let merge = vec!["b".to_string(), "z".to_string()];
        assert!(!is_linear_chain(&[("c", &merge), ("b", &b)]), "a merge breaks the chain");
    }

    #[test]
    fn squash_plan_picks_the_first_and_squashes_the_rest() {
        let commits = vec![
            ("a".to_string(), "one".to_string()),
            ("b".into(), "two".into()),
            ("c".into(), "three".into()),
        ];
        let plan = squash_plan(&commits);
        let actions: Vec<Action> = plan.steps.iter().map(|s| s.action).collect();
        assert_eq!(actions, vec![Action::Pick, Action::Squash, Action::Squash]);
        assert_eq!(plan.steps[0].hash, "a");
        assert!(plan.validate().is_ok());
        assert_eq!(squash_base("a", false).as_deref(), Some("a~1"));
        assert_eq!(squash_base("a", true), None);
    }

    #[test]
    fn rev_validation() {
        assert!(rev_error("").is_some());
        assert!(rev_error("-x").is_some());
        assert!(rev_error("a..b").is_some());
        assert!(rev_error("a b").is_some());
        assert_eq!(rev_error("origin/feat~2"), None);
        assert_eq!(rev_error("v1.0^{}"), None);
    }

    #[test]
    fn argv_shapes() {
        assert_eq!(DiffMode::default(), DiffMode::ThreeDot);
        assert_eq!(files_args("a", "b", DiffMode::TwoDot).last().map(String::as_str), Some("--"));
        assert!(files_args("a", "b", DiffMode::ThreeDot).contains(&"a...b".to_string()));
        assert_eq!(parse_count(b"12\n"), Some(12));
        assert_eq!(parse_resolved(b"nothex\n"), None);
        assert_eq!(range_base(&"f".repeat(64), true), EMPTY_TREE_SHA256);
    }
}
