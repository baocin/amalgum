//! Per-file history (§5.15 "File history"): `git log --follow --name-status -z` parsed into one
//! entry per commit with the file's name at that commit and its change (renames noted), plus
//! the argv for that file's diff at one commit and for its blob (**Open at this commit**).
//!
//! Records are the graph's [`log::LOG_FORMAT`] prefixed with `%x1e` so each record can be found
//! without guessing where the previous one's name-status ended: `\x1e<record>\0\n<status>\0
//! <path>\0[<new path>\0]`. A merge commit carries no name-status.

use super::cmd::{Git, GitError};
use super::diff::{self, FileDiff};
use super::log::{self, Commit};

/// The most commits one history view reads (§2 "Streaming": diffs and lists are capped).
pub const HISTORY_LIMIT: usize = 2000;

/// What happened to the file in one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatus {
    /// `A`, `M`, `D`, `R`, `C`, `T`.
    pub status: char,
    /// Rename / copy similarity (`R087` → 87).
    pub score: Option<u8>,
    /// The name before a rename or copy.
    pub old_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub commit: Commit,
    /// The file's name at this commit (after the commit's rename, if any).
    pub path: String,
    /// `None` for a merge commit (git prints no name-status for merges).
    pub change: Option<FileStatus>,
}

impl HistoryEntry {
    /// `renamed from old` note for a rename (§5.15 "Renames noted inline").
    pub fn renamed_from(&self) -> Option<&str> {
        let change = self.change.as_ref()?;
        matches!(change.status, 'R' | 'C').then_some(change.old_path.as_deref()).flatten()
    }
}

/// `git log --follow` argv for `path`'s history, newest first, starting at `rev` (`HEAD` when
/// `None`). `path` is the file's name at `rev`: starting there keeps the whole history of a file
/// that was renamed (or deleted) later.
pub fn history_args(rev: Option<&str>, path: &str, limit: usize) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "log".into(),
        "--follow".into(),
        "-M".into(),
        "--name-status".into(),
        "-z".into(),
        "--decorate=full".into(),
        "--no-show-signature".into(),
        "--no-color".into(),
        format!("--format=%x1e{}", log::LOG_FORMAT),
        "-n".into(),
        limit.to_string(),
    ];
    args.extend(rev.map(str::to_string));
    args.extend(["--".to_string(), path.to_string()]);
    args
}

/// Parse [`history_args`] output. `path` is the name asked for (the newest name): entries
/// without a name-status (merges) carry the name the file had at the next newer rename.
pub fn parse_history(out: &[u8], path: &str) -> Vec<HistoryEntry> {
    let mut entries = Vec::new();
    let mut current = path.to_string();
    for chunk in out.split(|&b| b == 0x1e).filter(|c| !c.is_empty()) {
        let mut tokens = chunk.split(|&b| b == 0);
        let Some(commit) = tokens.next().and_then(log::parse_record) else { continue };
        let rest: Vec<String> = tokens
            .map(|t| t.strip_prefix(b"\n").unwrap_or(t))
            .filter(|t| !t.is_empty())
            .map(|t| String::from_utf8_lossy(t).into_owned())
            .collect();
        let change = rest.first().and_then(|status| {
            let mut chars = status.chars();
            let letter = chars.next()?;
            let score = chars.as_str().parse().ok();
            match letter {
                'R' | 'C' => Some((
                    FileStatus { status: letter, score, old_path: rest.get(1).cloned() },
                    rest.get(2).cloned(),
                )),
                _ => Some((FileStatus { status: letter, score, old_path: None }, rest.get(1).cloned())),
            }
        });
        let (change, name) = match change {
            Some((c, name)) => (Some(c), name),
            None => (None, None),
        };
        let at = name.unwrap_or_else(|| current.clone());
        if let Some(old) = change.as_ref().and_then(|c| c.old_path.clone()) {
            current = old;
        } else {
            current = at.clone();
        }
        entries.push(HistoryEntry { commit, path: at, change });
    }
    entries
}

/// Run [`history_args`] and parse it.
pub fn run(git: &Git, rev: Option<&str>, path: &str, limit: usize) -> Result<Vec<HistoryEntry>, GitError> {
    let args = history_args(rev, path, limit);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git.run(&args).map(|out| parse_history(&out, path))
}

/// The argv for `entry`'s file diff at its commit: against the first parent (so a merge shows
/// what it brought to this file, and a rename shows as one), or the root commit's whole file.
pub fn entry_diff_args(entry: &HistoryEntry) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    match entry.commit.parents.first() {
        Some(parent) => {
            args.extend(["diff".to_string(), "-M".to_string()]);
            args.extend(diff::DIFF_ARGS.iter().map(|s| s.to_string()));
            args.extend([parent.clone(), entry.commit.id.clone()]);
        }
        None => {
            args.extend(["show".to_string(), "--format=".to_string(), "-M".to_string()]);
            args.extend(diff::DIFF_ARGS.iter().map(|s| s.to_string()));
            args.push(entry.commit.id.clone());
        }
    }
    args.push("--".into());
    if let Some(old) = entry.renamed_from() {
        args.push(old.to_string());
    }
    args.push(entry.path.clone());
    args
}

/// `entry`'s diff, parsed.
pub fn entry_diff(git: &Git, entry: &HistoryEntry) -> Result<Vec<FileDiff>, GitError> {
    let args = entry_diff_args(entry);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git.run(&args).map(|out| diff::parse(&out))
}

/// `git show <rev>:<path>`: the file's bytes at `rev` (**Open at this commit**). `--no-textconv`
/// keeps them the bytes git stores.
pub fn show_blob_args(rev: &str, path: &str) -> Vec<String> {
    vec!["show".into(), "--no-textconv".into(), format!("{rev}:{path}")]
}

/// The name to save `path` at `rev` under, for opening in the default app: the file's own name
/// (so the right app opens it) inside a directory named after the short hash, keeping any
/// suffix such as `^` (the parent) so `id` and `id^` don't share a file. Both parts are single
/// path components: separators and dots-only names can't climb out of the directory.
pub fn blob_file_name(rev: &str, path: &str) -> (String, String) {
    let hex = rev.bytes().take_while(u8::is_ascii_hexdigit).count();
    let (id, suffix) = rev.split_at(hex);
    let short = if hex >= 40 { format!("{}{suffix}", &id[..7]) } else { rev.to_string() };
    let short: String = short
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "^~-_".contains(c) { c } else { '_' })
        .collect();
    let short = if short.is_empty() { "rev".to_string() } else { short };
    let base =
        path.rsplit('/').next().filter(|b| !b.is_empty() && b.chars().any(|c| c != '.')).unwrap_or("file");
    (short, base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;

    fn git_of(repo: &TempRepo) -> Git {
        Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() })
    }

    /// a.txt → "b dir/b file.txt" (rename, then an edit), with an unrelated commit between.
    fn renamed_repo() -> (TempRepo, Vec<String>) {
        let mut repo = TempRepo::new();
        let c1 = repo.commit_file("a.txt", "one\ntwo\nthree\nfour\n", "add a");
        let c2 = repo.commit_file("a.txt", "one\nTWO\nthree\nfour\n", "edit a");
        repo.commit_file("other.txt", "x\n", "unrelated");
        std::fs::create_dir_all(repo.path().join("b dir")).expect("mkdir");
        repo.git(&["mv", "a.txt", "b dir/b file.txt"]);
        let c4 = repo.commit("move a");
        let c5 = repo.commit_file("b dir/b file.txt", "one\nTWO\nthree\nfour\nfive\n", "edit b");
        (repo, vec![c1, c2, c4, c5])
    }

    #[test]
    fn follows_renames_and_notes_them() {
        let (repo, ids) = renamed_repo();
        let h = run(&git_of(&repo), None, "b dir/b file.txt", HISTORY_LIMIT).expect("history");
        let got: Vec<(&str, &str, char)> = h
            .iter()
            .map(|e| (e.commit.id.as_str(), e.path.as_str(), e.change.as_ref().unwrap().status))
            .collect();
        assert_eq!(
            got,
            [
                (ids[3].as_str(), "b dir/b file.txt", 'M'),
                (ids[2].as_str(), "b dir/b file.txt", 'R'),
                (ids[1].as_str(), "a.txt", 'M'),
                (ids[0].as_str(), "a.txt", 'A'),
            ]
        );
        assert_eq!(h[1].renamed_from(), Some("a.txt"));
        assert_eq!(h[1].change.as_ref().unwrap().score, Some(100));
        assert_eq!(h[0].renamed_from(), None);
        assert_eq!(h[0].commit.subject, "edit b");
    }

    #[test]
    fn entry_diffs_show_the_file_at_each_commit() {
        let (repo, _) = renamed_repo();
        let git = git_of(&repo);
        let h = run(&git, None, "b dir/b file.txt", HISTORY_LIMIT).expect("history");
        let edit = entry_diff(&git, &h[0]).expect("diff");
        assert_eq!(edit.len(), 1);
        assert!(edit[0].hunks[0].lines.iter().any(|l| l.display() == "five"));
        let rename = entry_diff(&git, &h[1]).expect("diff");
        assert_eq!(rename.len(), 1, "one file, shown as a rename: {rename:?}");
        assert_eq!(rename[0].old_path.as_deref(), Some("a.txt"));
        assert_eq!(rename[0].new_path.as_deref(), Some("b dir/b file.txt"));
        let root = entry_diff(&git, &h[3]).expect("diff");
        assert_eq!(root[0].hunks[0].lines.len(), 4, "the root commit adds the whole file");
    }

    #[test]
    fn merges_have_no_name_status_and_keep_the_current_name() {
        let mut repo = TempRepo::new();
        repo.commit_file("f.txt", "1\n2\n3\n", "base");
        repo.git(&["checkout", "-q", "-b", "side"]);
        repo.commit_file("f.txt", "1\n2\n3 side\n", "side edit");
        repo.git(&["checkout", "-q", "main"]);
        repo.commit_file("f.txt", "1 main\n2\n3\n", "main edit");
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
        let h = run(&git_of(&repo), None, "f.txt", HISTORY_LIMIT).expect("history");
        let subjects: Vec<&str> = h.iter().map(|e| e.commit.subject.as_str()).collect();
        assert!(subjects.contains(&"side edit") && subjects.contains(&"main edit"), "{subjects:?}");
        assert!(h.iter().all(|e| e.path == "f.txt"));
        // A hand-made merge record without name-status parses too.
        let mut out = b"\x1e".to_vec();
        out.extend(b"m\x1fp1 p2\x1fA\x1fa@x\x1f1\x1fA\x1fa@x\x1f1\x1f\x1fmerge");
        out.push(0);
        let parsed = parse_history(&out, "f.txt");
        assert_eq!(parsed.len(), 1);
        assert_eq!((parsed[0].path.as_str(), parsed[0].change.clone()), ("f.txt", None));
    }

    #[test]
    fn deleted_files_and_the_limit() {
        let mut repo = TempRepo::new();
        repo.commit_file("gone.txt", "x\n", "add");
        repo.commit_file("gone.txt", "y\n", "edit");
        repo.git(&["rm", "-q", "gone.txt"]);
        repo.commit("delete");
        let h = run(&git_of(&repo), None, "gone.txt", HISTORY_LIMIT).expect("history");
        assert_eq!(h.iter().map(|e| e.change.as_ref().unwrap().status).collect::<String>(), "DMA");
        assert_eq!(run(&git_of(&repo), None, "gone.txt", 2).unwrap().len(), 2);
    }

    #[test]
    fn starting_at_an_older_commit_keeps_the_history_of_a_later_renamed_name() {
        let (repo, ids) = renamed_repo();
        let git = git_of(&repo);
        // From HEAD the old name only shows the rename away from it.
        let from_head = run(&git, None, "a.txt", HISTORY_LIMIT).expect("history");
        assert_eq!(from_head.first().map(|e| e.change.as_ref().unwrap().status), Some('D'));
        let h = run(&git, Some(&ids[1]), "a.txt", HISTORY_LIMIT).expect("history");
        let got: Vec<&str> = h.iter().map(|e| e.commit.id.as_str()).collect();
        assert_eq!(got, [ids[1].as_str(), ids[0].as_str()]);
    }

    #[test]
    fn open_at_commit_reads_the_blob_at_that_revision() {
        let (repo, ids) = renamed_repo();
        let git = git_of(&repo);
        let args = show_blob_args(&ids[1], "a.txt");
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_eq!(git.run(&args).unwrap(), b"one\nTWO\nthree\nfour\n");
        assert_eq!(
            blob_file_name(&ids[1], "b dir/b file.txt"),
            (ids[1][..7].to_string(), "b file.txt".into())
        );
        assert_eq!(blob_file_name("abc", "").1, "file");
        assert_eq!(blob_file_name(&format!("{}^", ids[1]), "x").0, format!("{}^", &ids[1][..7]));
        assert_eq!(blob_file_name("../origin/main", "..").0, "___origin_main");
        assert_eq!(blob_file_name("HEAD", "a/..").1, "file");
    }

    #[test]
    fn empty_output_is_empty_history() {
        assert!(parse_history(b"", "x").is_empty());
        let repo = TempRepo::new();
        // No commits: git log fails on an unborn HEAD; the caller shows the error.
        assert!(run(&git_of(&repo), None, "x", 10).is_err());
    }
}
