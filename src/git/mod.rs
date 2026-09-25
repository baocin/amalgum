//! Git, via the system `git` binary (§1): one backend for local and `ssh host:path`
//! workspaces. Every command goes through [`cmd::Git`], which prefixes remote commands with
//! `ssh <host> -- git -C <path>`. Output is always machine format (`--porcelain`, `-z`,
//! `--format`) and parsed by the pure functions in the submodules, which never spawn anything.
//!
//! - [`cmd`]      runner, [`Location`], [`GitError`] with a human summary for toasts
//! - [`status`]   `status --porcelain=v2 -z`, in-progress operation detection
//! - [`refs`]     branches/tags (`for-each-ref`), stashes, worktrees, remotes
//! - [`log`]      commit records for the graph and details
//! - [`graph`]    incremental lane layout; rows never move once laid out
//! - [`diff`]     unified diff parsing, file-change lists, hunk/line patch synthesis, word diff
//! - [`remote`]   URL normalisation, repo identity, web links, ref-name validation
//! - [`search`]   commit search query language (§5.8)
//! - [`journal`]  undo/redo journal (§5.18)
//! - [`rebase`]   interactive rebase plans and the editor helpers (§5.16)
//! - [`message`]  commit message editor rules (§5.7)

pub mod cmd;
pub mod diff;
pub mod graph;
pub mod journal;
pub mod log;
pub mod message;
pub mod rebase;
pub mod refs;
pub mod remote;
pub mod search;
pub mod status;

pub use cmd::{Git, GitError, Location};
