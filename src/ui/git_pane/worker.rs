//! Everything that runs on a worker thread and the pure logic around it: the `Reply` enum
//! carried back over the pane's channel, the generation guard that drops stale replies (§1),
//! log pagination bookkeeping (§2 "Streaming"), and the journal-entry builders for commit,
//! checkout, and undo/redo (§6).

use crate::git::cmd::{Git, GitError};
use crate::git::diff::{FileChange, FileDiff};
use crate::git::journal::{self, Entry as JournalEntry, Plan, Refused, Snapshot};
use crate::git::log::Commit;
use crate::git::refs::{Ref, Remote, Stash, Worktree};
use crate::git::status::{RepoOp, Status};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// First page of the graph, streamed immediately (§5.4 "Streaming").
pub(super) const LOG_FIRST_PAGE: usize = 500;
/// Further pages, requested as the user scrolls near the end.
pub(super) const LOG_BATCH: usize = 2000;

/// The next `-n` to pass to `git log`: 500 for the first page, then +2000 per further batch.
pub(super) fn next_log_n(loaded_before: usize) -> usize {
    if loaded_before == 0 { LOG_FIRST_PAGE } else { loaded_before + LOG_BATCH }
}

/// Whether a reply tagged `reply_gen` still belongs to the pane's current generation, or was
/// produced by a job started before the last `refresh()` and should be dropped.
pub(super) fn gen_is_current(reply_gen: u64, current_gen: u64) -> bool {
    reply_gen == current_gen
}

/// The outcome of a successful commit worker job.
pub(super) struct CommitOutcome {
    pub hash: String,
    pub subject: String,
    pub entry: JournalEntry,
}

/// The outcome of a successful checkout worker job.
pub(super) struct CheckoutOutcome {
    pub entry: JournalEntry,
}

/// The outcome of a successful undo/redo worker job: the journal (handed back so the UI thread
/// regains ownership of it) and what happened.
pub(super) struct JournalRunOutcome {
    pub journal: journal::Journal,
    pub result: Result<(), Refused>,
}

/// Replies carried back from worker threads over the pane's channel. Variants carrying a
/// `page_gen` (the pane's generation counter at dispatch time) are dropped by the receiver when
/// it no longer matches the pane's current generation; the rest are one-off action results
/// applied unconditionally.
pub(super) enum Reply {
    Status { page_gen: u64, result: Result<Status, GitError> },
    Refs { page_gen: u64, result: Result<Vec<Ref>, GitError> },
    Stashes { page_gen: u64, result: Result<Vec<Stash>, GitError> },
    Worktrees { page_gen: u64, result: Result<Vec<Worktree>, GitError> },
    Remotes { page_gen: u64, result: Result<Vec<Remote>, GitError> },
    Op { page_gen: u64, git_dir: Option<PathBuf>, op: Option<RepoOp> },
    LogPage { page_gen: u64, requested: usize, result: Result<Vec<Commit>, GitError> },

    CommitBody { id: String, result: Result<(String, String), GitError> },
    FileList { id: String, result: Result<Vec<FileChange>, GitError> },
    DetailsDiff { req: u64, result: Result<Vec<FileDiff>, GitError> },

    ChangesDiff { req: u64, result: Result<Vec<FileDiff>, GitError> },
    LastCommitMessage(Result<(String, String), GitError>),
    ActionDone { req: u64, result: Result<(), GitError> },
    Committed(Result<CommitOutcome, GitError>),
    Checkout { branch: String, result: Result<CheckoutOutcome, GitError> },

    Undo(JournalRunOutcome),
    Redo(JournalRunOutcome),
}

/// `git rev-parse HEAD` and `git symbolic-ref --short -q HEAD`, tolerating an unborn HEAD
/// (`None`) or a detached one (`branch: None`).
pub(super) fn head_and_branch(git: &Git) -> (Option<String>, Option<String>) {
    let head = git.run(&["rev-parse", "HEAD"]).ok().map(|o| String::from_utf8_lossy(&o).trim().to_string());
    let branch = git
        .run(&["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .filter(|b| !b.is_empty());
    (head, branch)
}

/// A short (7-char) hash for descriptions and labels; shorter inputs pass through unchanged.
pub(super) fn short_hash(id: &str) -> &str {
    &id[..id.len().min(7)]
}

/// `git diff --no-index` shows an untracked file "as all-added" (§4 "Changes view"), but unlike
/// ordinary `git diff` it exits 1 when a difference is found (documented `git-diff` behaviour),
/// so `Git::run`'s "non-zero means error, discard stdout" convention would lose the diff. This
/// runs it directly and accepts exit codes 0 and 1 as success.
pub(super) fn diff_no_index(git: &Git, path: &str) -> Result<Vec<u8>, GitError> {
    use std::process::Stdio;
    let args = ["diff", "--no-color", "--no-index", "--", "/dev/null", path]; // portability: allow
    let command_line = format!("git diff --no-index -- /dev/null {path}"); // portability: allow
    let mut cmd = git.command(&args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = cmd.output().map_err(|e| GitError {
        command: command_line.clone(),
        code: None,
        stderr: e.to_string(),
    })?;
    match output.status.code() {
        Some(0) | Some(1) => Ok(output.stdout),
        code => Err(GitError {
            command: command_line,
            code,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }),
    }
}

/// Builds the journal entry for a commit (§6 "forward/inverse"): the same commit object is not
/// reproducible by re-running `git commit`, so `forward` and `inverse` are both `reset --soft`
/// to the after/before hash. `branch_ref` is the full ref name (`refs/heads/main`) if HEAD was
/// on a branch, so the guard can name it in "has moved since" toasts.
pub(super) fn commit_entry(
    now: u64,
    before_head: Option<String>,
    branch_ref: Option<String>,
    after_head: String,
) -> JournalEntry {
    let mut before_refs = BTreeMap::new();
    let mut after_refs = BTreeMap::new();
    if let (Some(name), Some(before)) = (&branch_ref, &before_head) {
        before_refs.insert(name.clone(), before.clone());
    }
    if let Some(name) = &branch_ref {
        after_refs.insert(name.clone(), after_head.clone());
    }
    let branch_short = branch_ref.as_deref().and_then(|r| r.strip_prefix("refs/heads/")).map(str::to_string);
    JournalEntry {
        id: 0,
        time: now,
        description: format!("commit {}", short_hash(&after_head)),
        before: Snapshot {
            head: before_head.clone(),
            branch: branch_short.clone(),
            refs: before_refs,
            stashes: vec![],
        },
        after: Snapshot {
            head: Some(after_head.clone()),
            branch: branch_short,
            refs: after_refs,
            stashes: vec![],
        },
        forward: vec![vec!["reset".to_string(), "--soft".to_string(), after_head]],
        inverse: before_head.map(|h| vec![vec!["reset".to_string(), "--soft".to_string(), h]]),
        undone: false,
    }
}

/// Journal entry for a branch checkout: the guard is HEAD's commit (in `Snapshot::head`), so
/// undo refuses once new commits landed on the checked-out branch. `before_branch` is `None`
/// when the checkout started from a detached HEAD, in which case the inverse checks out
/// `before_head` directly.
pub(super) fn checkout_entry(
    now: u64,
    before_head: String,
    before_branch: Option<String>,
    target: &str,
    after_head: String,
) -> JournalEntry {
    let restore_to = before_branch.clone().unwrap_or_else(|| before_head.clone());
    JournalEntry {
        id: 0,
        time: now,
        description: format!("checkout {target}"),
        before: Snapshot {
            head: Some(before_head),
            branch: before_branch,
            refs: BTreeMap::new(),
            stashes: vec![],
        },
        after: Snapshot {
            head: Some(after_head),
            branch: Some(target.to_string()),
            refs: BTreeMap::new(),
            stashes: vec![],
        },
        forward: vec![vec!["checkout".to_string(), target.to_string()]],
        inverse: Some(vec![vec!["checkout".to_string(), restore_to]]),
        undone: false,
    }
}

/// Runs every command of `plan` in order via `git.run`, stopping at (and returning) the first
/// failure.
pub(super) fn run_plan(git: &Git, plan: &Plan) -> Result<(), GitError> {
    for cmd in plan {
        let args: Vec<&str> = cmd.iter().map(String::as_str).collect();
        git.run(&args)?;
    }
    Ok(())
}

/// The human line for a refused undo/redo (§5.18): `action` is `"undo"` or `"redo"`.
pub(super) fn refusal_message(action: &str, refused: &Refused) -> String {
    match refused {
        Refused::NothingToDo => format!("Nothing to {action}."),
        Refused::Irreversible(desc) => format!("Can't {action}: {desc} cannot be undone from the client."),
        Refused::Moved(name) => format!("Can't {action}: `{}` has moved since.", display_ref_name(name)),
    }
}

fn display_ref_name(name: &str) -> &str {
    name.strip_prefix("refs/heads/").unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- next_log_n ---------------------------------------------------------------------

    #[test]
    fn next_log_n_first_page_then_batches() {
        assert_eq!(next_log_n(0), LOG_FIRST_PAGE);
        assert_eq!(next_log_n(500), 500 + LOG_BATCH);
        assert_eq!(next_log_n(2500), 2500 + LOG_BATCH);
    }

    // ---- gen_is_current -------------------------------------------------------------------

    #[test]
    fn gen_is_current_matches_exactly() {
        assert!(gen_is_current(3, 3));
        assert!(!gen_is_current(2, 3));
        assert!(!gen_is_current(4, 3));
    }

    // ---- short_hash -------------------------------------------------------------------------

    #[test]
    fn short_hash_truncates_to_seven() {
        assert_eq!(short_hash("ab12cd3ef456"), "ab12cd3");
        assert_eq!(short_hash("abc"), "abc");
        assert_eq!(short_hash(""), "");
    }

    // ---- commit_entry -----------------------------------------------------------------------

    #[test]
    fn commit_entry_normal_case_records_reset_soft_forward_and_inverse() {
        let e = commit_entry(
            100,
            Some("old1234".to_string()),
            Some("refs/heads/main".to_string()),
            "new5678".to_string(),
        );
        assert_eq!(e.description, "commit new5678");
        assert_eq!(e.before.head.as_deref(), Some("old1234"));
        assert_eq!(e.before.branch.as_deref(), Some("main"));
        assert_eq!(e.before.refs.get("refs/heads/main").map(String::as_str), Some("old1234"));
        assert_eq!(e.after.head.as_deref(), Some("new5678"));
        assert_eq!(e.after.refs.get("refs/heads/main").map(String::as_str), Some("new5678"));
        assert_eq!(e.forward, vec![vec!["reset".to_string(), "--soft".to_string(), "new5678".to_string()]]);
        assert_eq!(
            e.inverse,
            Some(vec![vec!["reset".to_string(), "--soft".to_string(), "old1234".to_string()]])
        );
        assert!(!e.undone);
    }

    #[test]
    fn commit_entry_initial_commit_has_no_inverse() {
        // No prior HEAD (unborn branch): nothing to reset back to.
        let e = commit_entry(0, None, Some("refs/heads/main".to_string()), "aaa1111".to_string());
        assert_eq!(e.before.head, None);
        assert!(e.before.refs.is_empty(), "no prior commit to record a before-hash for");
        assert_eq!(e.inverse, None);
    }

    #[test]
    fn commit_entry_detached_head_has_no_branch_ref_entries() {
        let e = commit_entry(0, Some("old".to_string()), None, "new".to_string());
        assert_eq!(e.before.branch, None);
        assert!(e.before.refs.is_empty());
        assert!(e.after.refs.is_empty());
        // The HEAD guard alone still protects undo/redo.
        assert_eq!(e.before.head.as_deref(), Some("old"));
        assert_eq!(e.after.head.as_deref(), Some("new"));
    }

    // ---- checkout_entry ---------------------------------------------------------------------

    #[test]
    fn checkout_entry_from_a_branch_restores_that_branch() {
        let e = checkout_entry(0, "h0".to_string(), Some("main".to_string()), "feat", "h1".to_string());
        assert_eq!(e.description, "checkout feat");
        assert_eq!(e.forward, vec![vec!["checkout".to_string(), "feat".to_string()]]);
        assert_eq!(e.inverse, Some(vec![vec!["checkout".to_string(), "main".to_string()]]));
        assert_eq!(e.before.head.as_deref(), Some("h0"));
        assert_eq!(e.after.head.as_deref(), Some("h1"));
        assert_eq!(e.after.branch.as_deref(), Some("feat"));
    }

    #[test]
    fn checkout_entry_from_detached_restores_the_hash() {
        let e = checkout_entry(0, "h0".to_string(), None, "feat", "h1".to_string());
        assert_eq!(e.inverse, Some(vec![vec!["checkout".to_string(), "h0".to_string()]]));
        assert_eq!(e.before.branch, None);
    }

    // ---- refusal_message --------------------------------------------------------------------

    #[test]
    fn refusal_message_nothing_to_do() {
        assert_eq!(refusal_message("undo", &Refused::NothingToDo), "Nothing to undo.");
        assert_eq!(refusal_message("redo", &Refused::NothingToDo), "Nothing to redo.");
    }

    #[test]
    fn refusal_message_irreversible_names_the_entry() {
        assert_eq!(
            refusal_message("undo", &Refused::Irreversible("push origin main".to_string())),
            "Can't undo: push origin main cannot be undone from the client."
        );
    }

    #[test]
    fn refusal_message_moved_strips_refs_heads_prefix() {
        assert_eq!(
            refusal_message("undo", &Refused::Moved("refs/heads/main".to_string())),
            "Can't undo: `main` has moved since."
        );
        assert_eq!(
            refusal_message("redo", &Refused::Moved("HEAD".to_string())),
            "Can't redo: `HEAD` has moved since."
        );
    }

    // ---- run_plan -----------------------------------------------------------------------------

    #[test]
    fn run_plan_against_real_repo() {
        let mut repo = crate::testutil::TempRepo::new();
        let h0 = repo.commit("first");
        let _h1 = repo.commit("second");
        let git = Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });

        let plan: Plan = vec![vec!["reset".to_string(), "--soft".to_string(), h0.clone()]];
        run_plan(&git, &plan).expect("plan runs");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), h0);
    }

    // ---- diff_no_index ------------------------------------------------------------------------

    #[test]
    fn diff_no_index_untracked_file_succeeds_despite_exit_code_1() {
        let repo = crate::testutil::TempRepo::new();
        repo.write("notes.txt", "hello\nworld\n");
        let git = Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });

        let out = diff_no_index(&git, "notes.txt").expect("exit code 1 (differences found) is success here");
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("+hello"), "diff should show the file as all-added: {text}");
        assert!(text.contains("+world"));
    }

    #[test]
    fn diff_no_index_missing_git_binary_is_an_error() {
        let repo = crate::testutil::TempRepo::new();
        repo.write("notes.txt", "hello\n");
        let mut git = Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });
        git.git_bin = "amalgum-git-binary-that-does-not-exist".to_string();
        assert!(diff_no_index(&git, "notes.txt").is_err());
    }

    #[test]
    fn run_plan_stops_at_first_failure() {
        let repo = crate::testutil::TempRepo::new();
        let git = Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });
        let plan: Plan = vec![vec!["not-a-git-command".to_string()]];
        assert!(run_plan(&git, &plan).is_err());
    }
}
