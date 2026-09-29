//! Worktree maintenance from the Refs view (§5.21): **Remove** (`--force` only after the dirty
//! confirmation), **Prune**, **Lock** / **Unlock**. None of these moves a ref, so none is
//! journaled (§5.18 journals ref-changing operations; the branch a removed worktree had checked
//! out stays where it is).

use super::status::{self, EntryKind, STATUS_ARGS};
use super::{Git, GitError, Location};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeOp {
    /// `worktree remove [--force] -- <path>`.
    Remove {
        path: String,
        force: bool,
    },
    Prune,
    Lock {
        path: String,
    },
    Unlock {
        path: String,
    },
}

impl WorktreeOp {
    pub fn args(&self) -> Vec<String> {
        let v = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match self {
            WorktreeOp::Remove { path, force: false } => v(&["worktree", "remove", "--", path]),
            WorktreeOp::Remove { path, force: true } => v(&["worktree", "remove", "--force", "--", path]),
            WorktreeOp::Prune => v(&["worktree", "prune"]),
            WorktreeOp::Lock { path } => v(&["worktree", "lock", "--", path]),
            WorktreeOp::Unlock { path } => v(&["worktree", "unlock", "--", path]),
        }
    }

    /// The success toast: "Removed worktree `../wt`".
    pub fn done_message(&self) -> String {
        match self {
            WorktreeOp::Remove { path, .. } => format!("Removed worktree `{path}`"),
            WorktreeOp::Prune => "Pruned stale worktrees".to_string(),
            WorktreeOp::Lock { path } => format!("Locked worktree `{path}`"),
            WorktreeOp::Unlock { path } => format!("Unlocked worktree `{path}`"),
        }
    }

    pub fn run(&self, git: &Git) -> Result<(), GitError> {
        let args = self.args();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        git.run(&args).map(|_| ())
    }
}

/// A runner for another worktree of the same repository: same host, same binaries, `path`.
pub fn worktree_git(base: &Git, path: &str) -> Git {
    let location = match &base.location {
        Location::Local { .. } => Location::Local { path: path.into() },
        Location::Remote { host, .. } => Location::Remote { host: host.clone(), path: path.to_string() },
    };
    Git { location, ..base.clone() }
}

/// The tracked and untracked files `git worktree remove` refuses to discard without `--force`,
/// from `status` run inside that worktree.
pub fn dirty_files(git: &Git, path: &str) -> Result<Vec<String>, GitError> {
    let out = worktree_git(git, path).run(STATUS_ARGS)?;
    let parsed = status::parse(&out).map_err(|e| GitError {
        command: format!("git -C {path} status"),
        code: None,
        stderr: e,
    })?;
    Ok(parsed.entries.into_iter().filter(|e| e.kind != EntryKind::Ignored).map(|e| e.path).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;

    fn git(repo: &TempRepo) -> Git {
        Git::new(Location::Local { path: repo.path().to_path_buf() })
    }

    fn add_worktree(repo: &TempRepo, name: &str) -> String {
        let path = repo.path().join(name);
        repo.git(&["worktree", "add", "-q", "-b", name, &path.display().to_string()]);
        path.display().to_string()
    }

    fn listed(repo: &TempRepo) -> Vec<crate::git::refs::Worktree> {
        crate::git::refs::parse_worktrees(&repo.git_raw(crate::git::refs::WORKTREE_ARGS))
    }

    #[test]
    fn args_put_the_path_after_double_dash() {
        assert_eq!(
            WorktreeOp::Remove { path: "-odd".into(), force: true }.args(),
            ["worktree", "remove", "--force", "--", "-odd"]
        );
        assert_eq!(WorktreeOp::Prune.args(), ["worktree", "prune"]);
    }

    #[test]
    fn lock_unlock_and_remove_a_clean_worktree() {
        let mut repo = TempRepo::new();
        repo.commit("first");
        let wt = add_worktree(&repo, "wt-clean");
        let g = git(&repo);

        WorktreeOp::Lock { path: wt.clone() }.run(&g).expect("lock");
        assert!(listed(&repo).iter().any(|w| w.locked));
        WorktreeOp::Unlock { path: wt.clone() }.run(&g).expect("unlock");
        assert!(!listed(&repo).iter().any(|w| w.locked));

        assert_eq!(dirty_files(&g, &wt).expect("status"), Vec::<String>::new());
        WorktreeOp::Remove { path: wt.clone(), force: false }.run(&g).expect("remove");
        assert_eq!(listed(&repo).len(), 1);
        // The branch it had checked out stays.
        assert!(!repo.git(&["rev-parse", "--verify", "refs/heads/wt-clean"]).is_empty());
    }

    #[test]
    fn a_dirty_worktree_lists_its_files_and_needs_force() {
        let mut repo = TempRepo::new();
        repo.commit("first");
        let wt = add_worktree(&repo, "wt-dirty");
        std::fs::write(std::path::Path::new(&wt).join("new file.txt"), "x").expect("write");
        let g = git(&repo);

        assert_eq!(dirty_files(&g, &wt).expect("status"), ["new file.txt"]);
        let err = WorktreeOp::Remove { path: wt.clone(), force: false }.run(&g).expect_err("refuses");
        assert!(err.stderr.contains("untracked") || err.stderr.contains("modified"), "{}", err.stderr);
        WorktreeOp::Remove { path: wt, force: true }.run(&g).expect("forced");
        assert_eq!(listed(&repo).len(), 1);
    }

    #[test]
    fn prune_forgets_a_deleted_worktree_directory() {
        let mut repo = TempRepo::new();
        repo.commit("first");
        let wt = add_worktree(&repo, "wt-gone");
        std::fs::remove_dir_all(&wt).expect("rm");
        assert!(listed(&repo).iter().any(|w| w.prunable));
        WorktreeOp::Prune.run(&git(&repo)).expect("prune");
        assert_eq!(listed(&repo).len(), 1);
    }

    #[test]
    fn worktree_git_keeps_host_and_binaries() {
        let mut base = Git::new(Location::Remote { host: "box".into(), path: "~/repo".into() });
        base.git_bin = "git2".into();
        let g = worktree_git(&base, "~/repo-wt");
        assert_eq!(g.location, Location::Remote { host: "box".into(), path: "~/repo-wt".into() });
        assert_eq!(g.git_bin, "git2");
    }
}
