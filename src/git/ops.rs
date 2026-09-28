//! Local ref-changing operations: branches (§5.9), remote management (§5.10), tags (§5.11),
//! stashes (§5.12), and history rewriting (§5.16). Each is an [`Op`] carrying exactly the user's
//! choices; [`Op::needs`] names the repo facts its plan depends on, the pure [`plan`] turns op +
//! facts into a description, the forward argv, and the inverse the spec's "Undo:" line asks for
//! (§5.18), and [`execute`] reads the facts, runs the plan, snapshots the repo on both sides,
//! and returns the journal [`Entry`].
//!
//! Redo replays a plan that reproduces the *same* objects, not the original command: an op that
//! writes new commits (merge, rebase, cherry-pick, revert) redoes as `reset --keep` to the
//! journaled result, an annotated tag as `update-ref` to the journaled tag object, and a stash
//! push as `stash store` of the journaled stash commit.

use super::cmd::overwritten_files;
use super::journal::{Entry, Plan, Snapshot};
use super::refs::{STASH_ARGS, parse_stashes};
use super::remote::validate_branch_name;
use super::status::{self, RepoOp, STATUS_ARGS, Status};
use super::{Git, GitError};
use std::collections::BTreeMap;
use std::fmt;

/// `git merge` fast-forward setting (§5.9 "Merge into current").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FastForward {
    #[default]
    Ff,
    NoFf,
    FfOnly,
}

/// `git reset` mode (§5.16 "Reset").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

impl ResetMode {
    fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        }
    }
}

/// One local ref-changing operation. Revisions (`base`, `rev`, `commits`, …) are anything
/// `git rev-parse` accepts; the plan pins them to hashes where redo needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// §5.9 Create: `git branch <name> <base>`, then optionally checkout.
    CreateBranch { name: String, base: String, checkout: bool },
    /// §5.9 Checkout of a local branch. `force` is the **Force** answer to "Checkout would
    /// overwrite N files": `checkout -f`, with the discarded changes saved as a hidden stash
    /// that undo restores. (**Stash and checkout** is [`Op::StashPush`] then this, two entries.)
    Checkout { branch: String, force: bool },
    /// §5.9 Checkout commit / §5.11 Checkout tag: detached HEAD.
    CheckoutDetached { rev: String },
    /// §5.9 Checkout of a remote branch: a tracking local branch of the same name.
    CheckoutRemote { remote: String, branch: String },
    /// §5.9 Rename: `git branch -m`.
    RenameBranch { from: String, to: String },
    /// §5.9 Delete: `git branch -d`, or `-D` once the user confirmed losing unmerged commits.
    DeleteBranch { name: String, force: bool },
    /// §5.9 Set upstream (`Some("origin/feat")`) or unset it (`None`).
    SetUpstream { branch: String, upstream: Option<String> },
    /// §5.9 Merge into current: `git merge --no-edit <ff> <rev>`.
    Merge { rev: String, ff: FastForward },
    /// §5.9 Rebase current onto `onto`.
    Rebase { onto: String },
    /// §5.11 Create: lightweight when `message` is `None`, else annotated, GPG-signed if
    /// `sign` (the **Sign** option). Never signed otherwise, whatever `tag.gpgSign` says: a
    /// signature needs a message, and git would open an editor for one.
    CreateTag { name: String, target: String, message: Option<String>, sign: bool },
    /// §5.11 Delete.
    DeleteTag { name: String },
    /// §5.12 Stash: `git stash push [-u] [-k] [-m <msg>]` (`None`: git's default message).
    StashPush { message: Option<String>, include_untracked: bool, keep_index: bool },
    /// §5.12 Apply `stash@{index}`.
    StashApply { index: usize },
    /// §5.12 Pop `stash@{index}`.
    StashPop { index: usize },
    /// §5.12 Drop `stash@{index}`.
    StashDrop { index: usize },
    /// §5.12 Branch from stash: `git stash branch <name> stash@{index}`.
    StashBranch { name: String, index: usize },
    /// §5.16 Cherry-pick, oldest first; `record_origin` adds `-x`.
    CherryPick { commits: Vec<String>, record_origin: bool },
    /// §5.16 Revert, one commit or a selection ("Revert 5 commits", §5.14), in the order given
    /// (newest first reverts cleanly); `mainline` is `-m N` for a merge commit.
    Revert { commits: Vec<String>, mainline: Option<u8> },
    /// §5.16 Reset the current branch (or detached HEAD) to `target`.
    Reset { target: String, mode: ResetMode },
    /// §5.10 Add remote.
    AddRemote { name: String, url: String },
    /// §5.10 Remove remote.
    RemoveRemote { name: String },
    /// §5.10 Rename remote.
    RenameRemote { from: String, to: String },
    /// §5.10 Edit URL.
    SetRemoteUrl { name: String, url: String },
}

/// The facts a plan depends on, besides HEAD and its branch (always read).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Needs {
    /// Revisions to resolve with `rev-parse --verify` (see [`commit_of`] for peeled ones).
    pub revs: Vec<String>,
    /// Branch whose upstream (`branch.<b>.remote`/`.merge`) to read.
    pub upstream_of: Option<String>,
    /// `stash@{n}` whose commit, message, and paths to read.
    pub stash: Option<usize>,
    /// Remote whose URLs, remote-tracking refs, and tracking branches to read; also reads the
    /// list of remote names.
    pub remote: Option<String>,
    /// Save the index and working tree as a stash commit first (`git stash create`): the
    /// backup of a hard reset or forced checkout, the index a mixed reset drops, and what a
    /// stash apply/pop's paths looked like before it.
    pub save_worktree: bool,
}

/// What [`execute`] read before planning. Plain data so [`plan`] can be tested without git.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub head: Option<String>,
    /// Current branch, `None` when detached.
    pub branch: Option<String>,
    /// Each resolvable revision of [`Needs::revs`] → its object id.
    pub oids: BTreeMap<String, String>,
    pub upstream: Option<Upstream>,
    pub stash: Option<StashFacts>,
    /// All remote names (read when [`Needs::remote`] is set).
    pub remotes: Vec<String>,
    pub remote: Option<RemoteFacts>,
    /// The `amalgum-undo-<timestamp>` stash commit of the index and working tree, `None` when
    /// clean — or when the index has unmerged entries, which `git stash create` can't save.
    pub saved_worktree: Option<String>,
    /// Unix seconds, for the hidden stash ref's name.
    pub now: u64,
}

/// A branch's upstream as git stores it: `branch.<b>.remote` and `branch.<b>.merge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub branch: String,
    pub remote: String,
    pub merge: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StashFacts {
    pub hash: String,
    /// The full reflog subject (`On main: msg`), as `git stash store -m` takes it back.
    pub subject: String,
    /// Tracked paths the stash changes (worktree or index).
    pub tracked: Vec<String>,
    /// Untracked files it saved (`-u`).
    pub untracked: Vec<String>,
    /// `(hash, subject)` of the stashes above it (`stash@{0}` first).
    pub newer: Vec<(String, String)>,
    /// The `tracked` paths that exist in the stash's base commit (`stash^1`).
    pub in_base: Vec<String>,
    /// The `tracked` paths that exist in [`Facts::saved_worktree`], or in HEAD when clean.
    pub in_current: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteFacts {
    pub url: String,
    pub push_url: Option<String>,
    /// Remote-tracking refs under `refs/remotes/<name>/`.
    pub refs: Vec<RemoteRef>,
    /// Local branches whose upstream is on this remote.
    pub tracking: Vec<Upstream>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    pub name: String,
    pub oid: String,
    /// Target of a symbolic ref (`refs/remotes/origin/HEAD`).
    pub symref: Option<String>,
}

/// How the journal entry's redo plan is built once the forward plan has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redo {
    /// Replay the forward plan: it reproduces the same state.
    Forward,
    /// Known up front.
    Plan(Plan),
    /// `reset --keep` to the HEAD the operation produced: the same commits, not new ones.
    KeepResetToAfter,
    /// `update-ref` of this ref to the object the operation left it at.
    RefToAfter(String),
    /// Store the stash commit the push produced and take its changes out of the working tree
    /// again, as the push did — after checking they are still exactly what it stashed.
    Restash { keep_index: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpPlan {
    /// "delete branch feat", shown as "Undo: delete branch feat".
    pub description: String,
    pub forward: Plan,
    /// Piped to the last forward command (an annotated tag's message, §5.7 "on stdin").
    pub stdin: Option<String>,
    pub inverse: Option<Plan>,
    pub redo: Redo,
    /// Full ref names the guard snapshots must cover.
    pub refs: Vec<String>,
    /// Whether the guard snapshots record the stash list.
    pub stashes: bool,
}

/// A validation failure, with a user-facing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError(pub String);

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Failures the UI reacts to specially, recognised where git has no machine-readable form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// §5.9 "Checkout would overwrite 3 files": offer Stash and checkout / Force / Cancel.
    WouldOverwrite { paths: Vec<String> },
    /// §5.12 "Nothing to stash" toast.
    NothingToStash,
    /// §5.9 `branch -d` of an unmerged branch: confirm with "Branch has N commits not on
    /// `main`". `onto` is the current branch (`None` when detached).
    NotFullyMerged { commits: Option<u32>, onto: Option<String> },
}

/// Classifies a failed command's stderr (git runs with `LC_ALL=C`). Nothing to stash is not
/// here: `git stash push` exits 0 then, so [`execute`] detects it from the stash list.
pub fn classify(stderr: &str) -> Option<Failure> {
    if let Some(paths) = overwritten_files(stderr) {
        return Some(Failure::WouldOverwrite { paths });
    }
    if stderr.to_ascii_lowercase().contains("is not fully merged") {
        return Some(Failure::NotFullyMerged { commits: None, onto: None });
    }
    None
}

/// An operation that stopped on conflicts, kept so that once the user continues it to the end
/// (§5.17) [`Pending::finish`] can journal it like any other (§5.18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub description: String,
    pub before: Snapshot,
    pub inverse: Option<Plan>,
    pub refs: Vec<String>,
}

impl Pending {
    /// The journal entry for the finished operation; `None` while it is still in progress or
    /// once it was aborted (nothing changed).
    pub fn finish(self, git: &Git, now: u64) -> Option<Entry> {
        if in_progress(git).is_some() {
            return None;
        }
        let after = read_snapshot(git, &self.refs, false);
        if after == self.before {
            return None;
        }
        Some(Entry {
            id: 0,
            time: now,
            description: self.description,
            forward: vec![reset("--keep", after.head.as_deref()?)],
            before: self.before,
            after,
            inverse: self.inverse,
            undone: false,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpError {
    Plan(PlanError),
    Git(GitError),
    /// A recognised failure of `action` ("Checkout", "Merge", …); `error` is `None` when git
    /// itself exited 0 (nothing to stash).
    Known {
        action: &'static str,
        failure: Failure,
        error: Option<GitError>,
    },
    /// Stopped on conflicts (§5.17). `op` is the in-progress operation to continue or abort;
    /// `None` for a stash apply/pop/branch, which leaves conflicts but no operation. `pending`
    /// journals a merge, rebase, cherry-pick, or revert once it is continued.
    Conflict {
        op: Option<RepoOp>,
        error: GitError,
        pending: Option<Box<Pending>>,
    },
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpError::Plan(e) => e.fmt(f),
            OpError::Git(e) => e.fmt(f),
            OpError::Known { action, failure: Failure::WouldOverwrite { paths }, .. } => {
                let n = paths.len();
                write!(f, "{action} would overwrite {n} file{}", if n == 1 { "" } else { "s" })
            }
            OpError::Known { failure: Failure::NothingToStash, .. } => f.write_str("Nothing to stash"),
            OpError::Known { failure: Failure::NotFullyMerged { commits, onto }, .. } => {
                let onto = onto.as_deref().map_or("HEAD".to_string(), |b| format!("`{b}`"));
                match commits {
                    Some(1) => write!(f, "Branch has 1 commit not on {onto}"),
                    Some(n) => write!(f, "Branch has {n} commits not on {onto}"),
                    None => write!(f, "Branch is not fully merged into {onto}"),
                }
            }
            OpError::Conflict { op: Some(op), .. } => write!(f, "{} stopped on conflicts", op.name()),
            OpError::Conflict { op: None, .. } => f.write_str("Stash applied with conflicts"),
        }
    }
}

impl std::error::Error for OpError {}

impl From<PlanError> for OpError {
    fn from(e: PlanError) -> Self {
        OpError::Plan(e)
    }
}

impl From<GitError> for OpError {
    fn from(e: GitError) -> Self {
        OpError::Git(e)
    }
}

/// A completed operation: its journal entry (`None` when it changed nothing, e.g. a merge that
/// was already up to date) and git's stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executed {
    pub entry: Option<Entry>,
    pub stdout: String,
}

/// The revision string that resolves `rev` to its commit (`<rev>^{commit}`), as a key of
/// [`Needs::revs`] / [`Facts::oids`].
pub fn commit_of(rev: &str) -> String {
    format!("{rev}^{{commit}}")
}

fn heads(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

fn tags(name: &str) -> String {
    format!("refs/tags/{name}")
}

fn stash_ref(index: usize) -> String {
    format!("stash@{{{index}}}")
}

impl Op {
    /// Which facts [`plan`] needs for this op.
    pub fn needs(&self) -> Needs {
        let revs = |revs: Vec<String>| Needs { revs, ..Needs::default() };
        let stash = |index: usize| Needs { stash: Some(index), ..Needs::default() };
        let remote = |name: &str| Needs { remote: Some(name.to_string()), ..Needs::default() };
        match self {
            Op::CreateBranch { name, base, .. } => revs(vec![commit_of(base), heads(name)]),
            Op::Checkout { branch, force } => Needs { save_worktree: *force, ..revs(vec![heads(branch)]) },
            Op::CheckoutDetached { rev } => revs(vec![commit_of(rev)]),
            Op::CheckoutRemote { remote, branch } => {
                revs(vec![format!("refs/remotes/{remote}/{branch}"), heads(branch)])
            }
            Op::RenameBranch { from, to } => revs(vec![heads(from), heads(to)]),
            Op::DeleteBranch { name, .. } => {
                Needs { upstream_of: Some(name.clone()), ..revs(vec![heads(name)]) }
            }
            Op::SetUpstream { branch, upstream } => {
                let mut r = vec![heads(branch)];
                r.extend(upstream.iter().map(|u| format!("refs/remotes/{u}")));
                Needs { upstream_of: Some(branch.clone()), ..revs(r) }
            }
            Op::Merge { rev, .. } => revs(vec![commit_of(rev)]),
            Op::Rebase { onto } => revs(vec![commit_of(onto)]),
            Op::CreateTag { name, target, .. } => revs(vec![commit_of(target), tags(name)]),
            Op::DeleteTag { name } => revs(vec![tags(name)]),
            Op::StashPush { .. } => Needs::default(),
            Op::StashApply { index } | Op::StashPop { index } => {
                Needs { save_worktree: true, ..stash(*index) }
            }
            Op::StashDrop { index } => stash(*index),
            Op::StashBranch { name, index } => Needs { revs: vec![heads(name)], ..stash(*index) },
            Op::CherryPick { commits, .. } => revs(commits.iter().map(|c| commit_of(c)).collect()),
            Op::Revert { commits, .. } => revs(commits.iter().map(|c| commit_of(c)).collect()),
            Op::Reset { target, mode } => {
                Needs { save_worktree: *mode != ResetMode::Soft, ..revs(vec![commit_of(target)]) }
            }
            Op::AddRemote { name, .. } | Op::RemoveRemote { name } | Op::SetRemoteUrl { name, .. } => {
                remote(name)
            }
            Op::RenameRemote { from, .. } => remote(from),
        }
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn fail<T>(message: impl Into<String>) -> Result<T, PlanError> {
    Err(PlanError(message.into()))
}

/// Rejects what git would read as an option.
fn check_arg(what: &str, value: &str) -> Result<(), PlanError> {
    if value.is_empty() || value.starts_with('-') {
        return fail(format!("Invalid {what} `{value}`"));
    }
    Ok(())
}

fn check_name(what: &str, name: &str) -> Result<(), PlanError> {
    validate_branch_name(name).or_else(|why| fail(format!("Invalid {what} name `{name}`: {why}")))
}

/// A 7-char hash for descriptions; shorter inputs pass through unchanged.
pub fn short_hash(id: &str) -> &str {
    &id[..id.len().min(7)]
}

impl Facts {
    fn oid(&self, rev: &str) -> Option<&str> {
        self.oids.get(rev).map(String::as_str)
    }
    fn commit(&self, rev: &str) -> Result<&str, PlanError> {
        check_arg("revision", rev)?;
        self.oid(&commit_of(rev)).ok_or_else(|| PlanError(format!("Unknown revision `{rev}`")))
    }
    fn branch_exists(&self, name: &str) -> bool {
        self.oid(&heads(name)).is_some()
    }
    fn existing_branch(&self, name: &str) -> Result<&str, PlanError> {
        self.oid(&heads(name)).ok_or_else(|| PlanError(format!("No branch `{name}`")))
    }
    fn head(&self) -> Result<&str, PlanError> {
        self.head.as_deref().ok_or_else(|| PlanError("No commits yet".to_string()))
    }
    /// The commands that check out what HEAD is on now: its branch, else its commit. An unborn
    /// branch can't be checked out: HEAD is pointed back at it and the files the checkout
    /// brought are removed again (`rm` refuses, failing the undo, if they were edited since).
    fn return_to(&self) -> Result<Plan, PlanError> {
        Ok(match (&self.branch, &self.head) {
            (Some(branch), Some(_)) => vec![checkout(branch)],
            (Some(branch), None) => vec![
                argv(&["rm", "-r", "-q", "--ignore-unmatch", "--", ":/"]),
                argv(&["symbolic-ref", "HEAD", &heads(branch)]),
            ],
            (None, _) => vec![checkout(self.head()?)],
        })
    }
    /// The current branch's full ref, for guards (none when detached).
    fn branch_refs(&self) -> Vec<String> {
        self.branch.iter().map(|b| heads(b)).collect()
    }
    fn stash(&self, index: usize) -> Result<&StashFacts, PlanError> {
        self.stash.as_ref().ok_or_else(|| PlanError(format!("No {}", stash_ref(index))))
    }
    fn remote(&self, name: &str) -> Result<&RemoteFacts, PlanError> {
        self.remote.as_ref().ok_or_else(|| PlanError(format!("No remote `{name}`")))
    }
}

fn checkout(target: &str) -> Vec<String> {
    argv(&["checkout", target, "--"])
}

fn reset(mode: &str, target: &str) -> Vec<String> {
    argv(&["reset", mode, target])
}

/// `:(literal)` pathspecs, so paths with `*`, `?`, or a leading `:` mean themselves.
fn pathspecs(paths: &[String]) -> impl Iterator<Item = String> + '_ {
    paths.iter().map(|p| format!(":(literal){p}"))
}

/// Puts the stash's `tracked` paths back as they are in the commit `worktree` (whose tree is
/// the working tree) and the tree-ish `index`, and removes its untracked files. `present` are
/// the tracked paths that exist in `worktree`. Unlike `restore`/`checkout`, which fail on a
/// pathspec that matches nothing, each step accepts paths that exist on only one side.
fn restore_paths(stash: &StashFacts, present: &[String], worktree: &str, index: &str) -> Plan {
    let mut plan = Plan::new();
    if !stash.tracked.is_empty() {
        let with = |mut cmd: Vec<String>| {
            cmd.extend(pathspecs(&stash.tracked));
            cmd
        };
        plan.push(with(argv(&["reset", "-q", worktree, "--"])));
        // Paths not in `worktree` are untracked now; remove them from the working tree.
        plan.push(with(argv(&["clean", "-f", "-q", "--"])));
        if !present.is_empty() {
            let mut cmd = argv(&["checkout-index", "-f", "-q", "--"]);
            cmd.extend(present.iter().cloned());
            plan.push(cmd);
        }
        if index != worktree {
            plan.push(with(argv(&["reset", "-q", index, "--"])));
        }
    }
    if !stash.untracked.is_empty() {
        let mut cmd = argv(&["clean", "-f", "-q", "--"]);
        cmd.extend(pathspecs(&stash.untracked));
        plan.push(cmd);
    }
    plan
}

/// Fails (`git diff` exits 1) unless the stash's tracked paths hold exactly what the stash
/// saved, in both the index and the working tree: [`Redo::Restash`] must not throw away edits
/// made after the undo.
fn check_unchanged(stash: &StashFacts) -> Plan {
    if stash.tracked.is_empty() {
        return Plan::new();
    }
    let index = format!("{}^2", stash.hash);
    [argv(&["diff", "--quiet", &stash.hash, "--"]), argv(&["diff", "--quiet", "--cached", &index, "--"])]
        .into_iter()
        .map(|mut cmd| {
            cmd.extend(pathspecs(&stash.tracked));
            cmd
        })
        .collect()
}

fn store(hash: &str, subject: &str) -> Vec<String> {
    argv(&["stash", "store", "-m", subject, hash])
}

/// Puts a popped or dropped stash back at its old position (§5.12: "`git stash store` restores
/// it exactly"). `store` always pushes on top, so the stashes that were above it come off first
/// and go back on after it, by their journaled hashes.
fn restore_stash(stash: &StashFacts) -> Plan {
    let mut plan: Plan = stash.newer.iter().map(|_| argv(&["stash", "drop", "stash@{0}"])).collect();
    plan.push(store(&stash.hash, &stash.subject));
    plan.extend(stash.newer.iter().rev().map(|(hash, subject)| store(hash, subject)));
    plan
}

/// The commands that give `branch` back its upstream as git stored it.
fn set_upstream_config(up: &Upstream) -> Plan {
    vec![
        argv(&["config", &format!("branch.{}.remote", up.branch), &up.remote]),
        argv(&["config", &format!("branch.{}.merge", up.branch), &up.merge]),
    ]
}

/// Where the hidden backups live (§5.16 "hidden stash"): outside `refs/stash`, so the stash
/// list doesn't show them, and excluded from the graph's `--all` (see [`super::log::log_args`]).
pub const HIDDEN_REFS: &str = "refs/amalgum/";

/// [`Facts::saved_worktree`] kept alive by a hidden ref while undo may need it: `create` makes
/// the ref, `drop` deletes it once undo has restored the stash.
struct Backup {
    name: String,
    saved: String,
    create: Vec<String>,
    drop: Vec<String>,
}

fn backup(facts: &Facts) -> Option<Backup> {
    let saved = facts.saved_worktree.clone()?;
    let name = format!("{HIDDEN_REFS}amalgum-undo-{}-{}", facts.now, short_hash(&saved));
    Some(Backup {
        create: argv(&["update-ref", &name, &saved]),
        drop: argv(&["update-ref", "-d", &name, &saved]),
        name,
        saved,
    })
}

/// Runs `cmd` behind a hidden backup when there is one; `restore` is the inverse up to the
/// point where the backup goes back.
fn with_backup(
    p: &mut OpPlan,
    facts: &Facts,
    cmd: Vec<String>,
    mut inverse: Plan,
    restore: impl Fn(&str) -> Plan,
) {
    match backup(facts) {
        Some(b) => {
            p.forward = vec![b.create.clone(), cmd.clone()];
            p.redo = Redo::Plan(vec![b.create, cmd]);
            inverse.extend(restore(&b.saved));
            inverse.push(b.drop);
            p.refs.push(b.name);
        }
        None => p.forward = vec![cmd],
    }
    p.inverse = Some(inverse);
}

/// Plans `op` against `facts`. Pure: validation, the forward plan, and its inverse.
pub fn plan(op: &Op, facts: &Facts) -> Result<OpPlan, PlanError> {
    let mut p = OpPlan {
        description: String::new(),
        forward: Plan::new(),
        stdin: None,
        inverse: None,
        redo: Redo::Forward,
        refs: Vec::new(),
        stashes: false,
    };
    // Ops that write new commits: undo is `reset --hard` to the journaled pre-op hash (§5.9,
    // §5.16), redo moves back to the journaled result.
    let rewrite = |p: &mut OpPlan, facts: &Facts| -> Result<(), PlanError> {
        p.inverse = Some(vec![reset("--hard", facts.head()?)]);
        p.redo = Redo::KeepResetToAfter;
        p.refs = facts.branch_refs();
        Ok(())
    };
    match op {
        Op::CreateBranch { name, base, checkout: then_checkout } => {
            check_name("branch", name)?;
            if facts.branch_exists(name) {
                return fail(format!("Branch `{name}` already exists"));
            }
            let base = facts.commit(base)?;
            p.description = format!("create branch {name}");
            p.forward.push(argv(&["branch", "--no-track", name, base]));
            let mut inverse = Plan::new();
            if *then_checkout {
                p.forward.push(checkout(name));
                inverse.extend(facts.return_to()?);
            }
            inverse.push(argv(&["branch", "-D", name]));
            p.inverse = Some(inverse);
            p.refs = vec![heads(name)];
        }
        Op::Checkout { branch, force } => {
            facts.existing_branch(branch)?;
            if facts.branch.as_deref() == Some(branch.as_str()) {
                return fail(format!("Already on `{branch}`"));
            }
            p.description = format!("checkout {branch}");
            if *force {
                // §5.18: undo checks the previous branch out and restores the discarded
                // changes from the hidden stash.
                let cmd = argv(&["checkout", "-f", branch, "--"]);
                with_backup(&mut p, facts, cmd, facts.return_to()?, |saved| {
                    vec![argv(&["stash", "apply", "--index", saved])]
                });
            } else {
                p.forward.push(checkout(branch));
                p.inverse = Some(facts.return_to()?);
            }
        }
        Op::CheckoutDetached { rev } => {
            let oid = facts.commit(rev)?;
            let shown = if rev == oid { short_hash(rev) } else { rev.as_str() };
            p.description = format!("checkout {shown}");
            p.forward.push(argv(&["checkout", "--detach", oid]));
            p.inverse = Some(facts.return_to()?);
        }
        Op::CheckoutRemote { remote, branch } => {
            check_name("branch", branch)?;
            let tracking = format!("{remote}/{branch}");
            if facts.oid(&format!("refs/remotes/{tracking}")).is_none() {
                return fail(format!("No remote branch `{tracking}`"));
            }
            if facts.branch_exists(branch) {
                return fail(format!("A local branch `{branch}` already exists"));
            }
            p.description = format!("checkout {tracking}");
            p.forward.push(argv(&["checkout", "-b", branch, "--track", &tracking]));
            let mut inverse = facts.return_to()?;
            inverse.push(argv(&["branch", "-D", branch]));
            p.inverse = Some(inverse);
            p.refs = vec![heads(branch)];
        }
        Op::RenameBranch { from, to } => {
            check_name("branch", to)?;
            facts.existing_branch(from)?;
            if facts.branch_exists(to) {
                return fail(format!("Branch `{to}` already exists"));
            }
            p.description = format!("rename branch {from} to {to}");
            p.forward.push(argv(&["branch", "-m", from, to]));
            p.inverse = Some(vec![argv(&["branch", "-m", to, from])]);
            p.refs = vec![heads(from), heads(to)];
        }
        Op::DeleteBranch { name, force } => {
            let oid = facts.existing_branch(name)?;
            if facts.branch.as_deref() == Some(name.as_str()) {
                return fail(format!("Can't delete `{name}`: it is checked out"));
            }
            p.description = format!("delete branch {name}");
            p.forward.push(argv(&["branch", if *force { "-D" } else { "-d" }, name]));
            let mut inverse = vec![argv(&["branch", "--no-track", name, oid])];
            inverse.extend(facts.upstream.iter().flat_map(set_upstream_config));
            p.inverse = Some(inverse);
            p.refs = vec![heads(name)];
        }
        Op::SetUpstream { branch, upstream } => {
            facts.existing_branch(branch)?;
            match upstream {
                Some(up) => {
                    check_arg("upstream", up)?;
                    let full = format!("refs/remotes/{up}");
                    if facts.oid(&full).is_none() {
                        return fail(format!("No remote branch `{up}`"));
                    }
                    p.description = format!("set upstream of {branch} to {up}");
                    p.forward.push(argv(&["branch", &format!("--set-upstream-to={up}"), branch]));
                    // Not the remote-tracking ref: the op doesn't move it, and a background
                    // fetch that does must not block undo.
                    p.refs = vec![heads(branch)];
                }
                None => {
                    if facts.upstream.is_none() {
                        return fail(format!("`{branch}` has no upstream"));
                    }
                    p.description = format!("unset upstream of {branch}");
                    p.forward.push(argv(&["branch", "--unset-upstream", branch]));
                    p.refs = vec![heads(branch)];
                }
            }
            p.inverse = Some(match &facts.upstream {
                Some(up) => set_upstream_config(up),
                None => vec![argv(&["branch", "--unset-upstream", branch])],
            });
        }
        Op::Merge { rev, ff } => {
            facts.commit(rev)?;
            let flag = match ff {
                FastForward::Ff => "--ff",
                FastForward::NoFf => "--no-ff",
                FastForward::FfOnly => "--ff-only",
            };
            p.description = format!("merge {rev}");
            p.forward.push(argv(&["merge", "--no-edit", flag, rev]));
            rewrite(&mut p, facts)?;
        }
        Op::Rebase { onto } => {
            facts.commit(onto)?;
            p.description = match &facts.branch {
                Some(b) => format!("rebase {b} onto {onto}"),
                None => format!("rebase onto {onto}"),
            };
            p.forward.push(argv(&["rebase", onto]));
            rewrite(&mut p, facts)?;
        }
        Op::CreateTag { name, target, message, sign } => {
            check_name("tag", name)?;
            if facts.oid(&tags(name)).is_some() {
                return fail(format!("Tag `{name}` already exists"));
            }
            let oid = facts.commit(target)?;
            p.description = format!("create tag {name}");
            p.forward.push(match message {
                Some(_) => argv(&["tag", "-a", if *sign { "-s" } else { "--no-sign" }, "-F", "-", name, oid]),
                None if *sign => return fail("A signed tag needs a message"),
                None => argv(&["tag", "--no-sign", name, oid]),
            });
            p.stdin = message.clone();
            p.inverse = Some(vec![argv(&["tag", "-d", name])]);
            p.redo = Redo::RefToAfter(tags(name));
            p.refs = vec![tags(name)];
        }
        Op::DeleteTag { name } => {
            let oid = facts.oid(&tags(name)).ok_or_else(|| PlanError(format!("No tag `{name}`")))?;
            p.description = format!("delete tag {name}");
            p.forward.push(argv(&["tag", "-d", name]));
            p.inverse = Some(vec![argv(&["update-ref", &tags(name), oid])]);
            p.refs = vec![tags(name)];
        }
        Op::StashPush { message, include_untracked, keep_index } => {
            let mut cmd = argv(&["stash", "push"]);
            if *include_untracked {
                cmd.push("-u".to_string());
            }
            if *keep_index {
                cmd.push("-k".to_string());
            }
            if let Some(m) = message {
                cmd.extend(["-m".to_string(), m.clone()]);
            }
            p.description = "stash changes".to_string();
            p.forward.push(cmd);
            // `--index` restores what was staged. With `-k` the staged changes are still in the
            // index and working tree, and popping onto them would conflict: [`execute`] puts
            // the stashed paths back to HEAD first, once the stash exists to name them.
            p.inverse = Some(vec![argv(&["stash", "pop", "--index"])]);
            p.redo = Redo::Restash { keep_index: *keep_index };
        }
        Op::StashApply { index } => {
            let stash = facts.stash(*index)?;
            p.description = format!("apply {}", stash_ref(*index));
            p.forward.push(argv(&["stash", "apply", &stash.hash]));
            p.inverse = Some(unapply(stash, facts)?);
        }
        Op::StashPop { index } => {
            let stash = facts.stash(*index)?;
            p.description = format!("pop {}", stash_ref(*index));
            p.forward.push(argv(&["stash", "pop", &stash_ref(*index)]));
            let mut inverse = restore_stash(stash);
            inverse.extend(unapply(stash, facts)?);
            p.inverse = Some(inverse);
        }
        Op::StashDrop { index } => {
            let stash = facts.stash(*index)?;
            p.description = format!("drop {}", stash_ref(*index));
            p.forward.push(argv(&["stash", "drop", &stash_ref(*index)]));
            p.inverse = Some(restore_stash(stash));
        }
        Op::StashBranch { name, index } => {
            check_name("branch", name)?;
            if facts.branch_exists(name) {
                return fail(format!("Branch `{name}` already exists"));
            }
            let stash = facts.stash(*index)?;
            p.description = format!("branch {name} from {}", stash_ref(*index));
            p.forward.push(argv(&["stash", "branch", name, &stash_ref(*index)]));
            let mut inverse = restore_stash(stash);
            let base = format!("{}^1", stash.hash);
            inverse.extend(restore_paths(stash, &stash.in_base, &base, &base));
            inverse.extend(facts.return_to()?);
            inverse.push(argv(&["branch", "-D", name]));
            p.inverse = Some(inverse);
            p.refs = vec![heads(name)];
        }
        Op::CherryPick { commits, record_origin } => {
            let oids = commits.iter().map(|c| facts.commit(c)).collect::<Result<Vec<_>, _>>()?;
            p.description = match oids.as_slice() {
                [] => return fail("No commits to cherry-pick"),
                [one] => format!("cherry-pick {}", short_hash(one)),
                many => format!("cherry-pick {} commits", many.len()),
            };
            let mut cmd = argv(&["cherry-pick"]);
            if *record_origin {
                cmd.push("-x".to_string());
            }
            cmd.extend(oids.iter().map(|o| o.to_string()));
            p.forward.push(cmd);
            rewrite(&mut p, facts)?;
        }
        Op::Revert { commits, mainline } => {
            let oids = commits.iter().map(|c| facts.commit(c)).collect::<Result<Vec<_>, _>>()?;
            p.description = match oids.as_slice() {
                [] => return fail("No commits to revert"),
                [one] => format!("revert {}", short_hash(one)),
                many => format!("revert {} commits", many.len()),
            };
            let mut cmd = argv(&["revert", "--no-edit"]);
            if let Some(n) = mainline {
                cmd.extend(["-m".to_string(), n.to_string()]);
            }
            cmd.extend(oids.iter().map(|o| o.to_string()));
            p.forward.push(cmd);
            rewrite(&mut p, facts)?;
        }
        Op::Reset { target, mode } => {
            let oid = facts.commit(target)?;
            let before = facts.head()?;
            p.description = format!(
                "reset {} to {} ({})",
                facts.branch.as_deref().unwrap_or("HEAD"),
                short_hash(oid),
                &mode.flag()[2..]
            );
            p.refs = facts.branch_refs();
            let cmd = reset(mode.flag(), oid);
            match (mode, &facts.saved_worktree) {
                // §5.16: the pre-reset working tree comes back from the hidden stash.
                (ResetMode::Hard, _) => {
                    with_backup(&mut p, facts, cmd, vec![reset("--hard", before)], |saved| {
                        vec![argv(&["stash", "apply", "--index", saved])]
                    })
                }
                // The index the reset replaced is the stash's index commit (§5.18 journals the
                // index tree); the working tree was never touched.
                (ResetMode::Mixed, Some(_)) => {
                    with_backup(&mut p, facts, cmd, vec![reset("--soft", before)], |saved| {
                        vec![argv(&["read-tree", &format!("{saved}^2")])]
                    })
                }
                _ => {
                    p.forward = vec![cmd];
                    p.inverse = Some(vec![reset(mode.flag(), before)]);
                }
            }
        }
        Op::AddRemote { name, url } => {
            check_name("remote", name)?;
            check_arg("URL", url)?;
            if facts.remotes.contains(name) {
                return fail(format!("Remote `{name}` already exists"));
            }
            p.description = format!("add remote {name}");
            p.forward.push(argv(&["remote", "add", name, url]));
            p.inverse = Some(vec![argv(&["remote", "remove", name])]);
        }
        Op::RemoveRemote { name } => {
            let remote = facts.remote(name)?;
            p.description = format!("remove remote {name}");
            p.forward.push(argv(&["remote", "remove", name]));
            let mut inverse = vec![argv(&["remote", "add", name, &remote.url])];
            if let Some(push) = &remote.push_url {
                inverse.push(argv(&["config", &format!("remote.{name}.pushurl"), push]));
            }
            // Plain refs first: a symbolic ref's target must exist.
            let (sym, plain): (Vec<_>, Vec<_>) = remote.refs.iter().partition(|r| r.symref.is_some());
            inverse.extend(plain.iter().map(|r| argv(&["update-ref", &r.name, &r.oid])));
            inverse.extend(
                sym.iter().flat_map(|r| r.symref.as_deref().map(|t| argv(&["symbolic-ref", &r.name, t]))),
            );
            inverse.extend(remote.tracking.iter().flat_map(set_upstream_config));
            p.inverse = Some(inverse);
        }
        Op::RenameRemote { from, to } => {
            facts.remote(from)?;
            check_name("remote", to)?;
            if facts.remotes.contains(to) {
                return fail(format!("Remote `{to}` already exists"));
            }
            p.description = format!("rename remote {from} to {to}");
            p.forward.push(argv(&["remote", "rename", from, to]));
            p.inverse = Some(vec![argv(&["remote", "rename", to, from])]);
        }
        Op::SetRemoteUrl { name, url } => {
            let remote = facts.remote(name)?;
            check_arg("URL", url)?;
            p.description = format!("set URL of remote {name}");
            p.forward.push(argv(&["remote", "set-url", name, url]));
            p.inverse = Some(vec![argv(&["remote", "set-url", name, &remote.url])]);
        }
    }
    p.stashes = matches!(
        op,
        Op::StashPush { .. }
            | Op::StashApply { .. }
            | Op::StashPop { .. }
            | Op::StashDrop { .. }
            | Op::StashBranch { .. }
    );
    Ok(p)
}

/// The inverse of applying `stash`: its paths back to how they were before (the saved index and
/// working tree, or HEAD when the tree was clean) — so changes the user had staged on those
/// paths survive — and its untracked files removed.
fn unapply(stash: &StashFacts, facts: &Facts) -> Result<Plan, PlanError> {
    let (worktree, index) = match &facts.saved_worktree {
        Some(saved) => (saved.clone(), format!("{saved}^2")),
        None => (facts.head()?.to_string(), facts.head()?.to_string()),
    };
    Ok(restore_paths(stash, &stash.in_current, &worktree, &index))
}

/// The tracked paths a `reset --hard` in `plan` would discard (§5.9 merge undo: "confirm if
/// working tree dirty"). Empty when the plan has no hard reset or the tree is clean, so the
/// caller confirms before undo/redo only when this is non-empty.
pub fn lost_changes(plan: &Plan, status: &Status) -> Vec<String> {
    let hard =
        plan.iter().any(|cmd| cmd.first().is_some_and(|c| c == "reset") && cmd.iter().any(|a| a == "--hard"));
    if !hard {
        return Vec::new();
    }
    status
        .entries
        .iter()
        .filter(|e| !matches!(e.kind, status::EntryKind::Untracked | status::EntryKind::Ignored))
        .map(|e| e.path.clone())
        .collect()
}

// ---- I/O ----------------------------------------------------------------------------------

fn text(out: Vec<u8>) -> String {
    String::from_utf8_lossy(&out).trim().to_string()
}

/// `git rev-parse HEAD` and the branch name from `git symbolic-ref -q HEAD`, tolerating an
/// unborn HEAD (`None`) or a detached one (`branch: None`). The name is the full ref minus
/// `refs/heads/`, as `git checkout` takes it and as entries record it: `--short` would print
/// `heads/v1` for a branch `v1` when a tag `v1` also exists.
pub fn head_and_branch(git: &Git) -> (Option<String>, Option<String>) {
    let head = git.run(&["rev-parse", "HEAD"]).ok().map(text);
    let branch = git
        .run(&["symbolic-ref", "-q", "HEAD"])
        .ok()
        .map(text)
        .map(|r| r.strip_prefix("refs/heads/").map(str::to_string).unwrap_or(r))
        .filter(|b| !b.is_empty());
    (head, branch)
}

/// The repo's state for the journal guard (§5.18): HEAD and its branch, the hash of each ref in
/// `refs` (a ref that doesn't resolve is left out), and the stash list if `stashes`.
pub fn read_snapshot(git: &Git, refs: &[String], stashes: bool) -> Snapshot {
    let (head, branch) = head_and_branch(git);
    let refs = refs
        .iter()
        .filter_map(|name| Some((name.clone(), text(git.run(&["rev-parse", "--verify", "-q", name]).ok()?))))
        .collect();
    let stashes = if stashes {
        git.run(STASH_ARGS)
            .map(|o| parse_stashes(&o).into_iter().map(|s| s.oid).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Snapshot { head, branch, refs, stashes }
}

/// Runs every command of `plan` in order, stopping at (and returning) the first failure.
pub fn run_plan(git: &Git, plan: &Plan) -> Result<(), GitError> {
    for cmd in plan {
        let args: Vec<&str> = cmd.iter().map(String::as_str).collect();
        git.run(&args)?;
    }
    Ok(())
}

/// `stash list` with each entry's full reflog subject, for `stash store -m`.
pub const STASH_SUBJECT_ARGS: &[&str] = &["stash", "list", "-z", "--format=%H%x1f%gs"];

/// `(hash, subject)` of each stash, newest first, from [`STASH_SUBJECT_ARGS`] output.
pub fn parse_stash_subjects(out: &[u8]) -> Vec<(String, String)> {
    out.split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let line = String::from_utf8_lossy(r);
            let (hash, subject) = line.split_once('\x1f')?;
            Some((hash.to_string(), subject.to_string()))
        })
        .collect()
}

/// `-z` path lists (`diff --name-only -z`, `ls-tree --name-only -z`).
fn parse_paths(out: &[u8]) -> Vec<String> {
    out.split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

/// `config -z --get-regexp` output: `key\nvalue\0` records.
fn parse_config(out: &[u8]) -> Vec<(String, String)> {
    out.split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let r = String::from_utf8_lossy(r);
            let (k, v) = r.split_once('\n')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

pub const BRANCH_UPSTREAM_ARGS: &[&str] = &["config", "-z", "--get-regexp", r"^branch\..*\.(remote|merge)$"];

/// Every branch with both `branch.<b>.remote` and `branch.<b>.merge` set, from
/// [`BRANCH_UPSTREAM_ARGS`] output. Branch names may contain dots.
pub fn parse_upstreams(out: &[u8]) -> Vec<Upstream> {
    let mut by_branch: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    for (key, value) in parse_config(out) {
        let Some(rest) = key.strip_prefix("branch.") else { continue };
        if let Some(b) = rest.strip_suffix(".remote") {
            by_branch.entry(b.to_string()).or_default().0 = Some(value);
        } else if let Some(b) = rest.strip_suffix(".merge") {
            by_branch.entry(b.to_string()).or_default().1 = Some(value);
        }
    }
    by_branch
        .into_iter()
        .filter_map(|(branch, (remote, merge))| Some(Upstream { branch, remote: remote?, merge: merge? }))
        .collect()
}

pub const REMOTE_REFS_FORMAT: &str = "--format=%(refname)%00%(objectname)%00%(symref)";

/// `for-each-ref` output in [`REMOTE_REFS_FORMAT`].
pub fn parse_remote_refs(out: &[u8]) -> Vec<RemoteRef> {
    out.split(|&b| b == b'\n')
        .filter_map(|line| {
            let line = String::from_utf8_lossy(line);
            let mut f = line.split('\0');
            let (name, oid, symref) = (f.next()?, f.next()?, f.next()?);
            (!name.is_empty()).then(|| RemoteRef {
                name: name.to_string(),
                oid: oid.to_string(),
                symref: (!symref.is_empty()).then(|| symref.to_string()),
            })
        })
        .collect()
}

fn read_stash(git: &Git, index: usize) -> Option<StashFacts> {
    let mut newer = parse_stash_subjects(&git.run(STASH_SUBJECT_ARGS).ok()?);
    if index >= newer.len() {
        return None;
    }
    newer.truncate(index + 1);
    let (hash, subject) = newer.pop()?;
    let diff = |a: &str, b: &str| {
        git.run(&["diff", "--name-only", "-z", "--no-renames", a, b])
            .map(|o| parse_paths(&o))
            .unwrap_or_default()
    };
    let (base, index_commit) = (format!("{hash}^1"), format!("{hash}^2"));
    let mut tracked = diff(&base, &hash);
    tracked.extend(diff(&base, &index_commit));
    tracked.sort();
    tracked.dedup();
    let untracked_commit = format!("{hash}^3");
    let untracked = match git.run(&["rev-parse", "-q", "--verify", &untracked_commit]) {
        Ok(_) => git
            .run(&["ls-tree", "-r", "-z", "--name-only", &untracked_commit])
            .map(|o| parse_paths(&o))
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let in_base = paths_in(git, &base, &tracked);
    Some(StashFacts { hash, subject, tracked, untracked, newer, in_base, in_current: Vec::new() })
}

/// Which of `paths` exist in the tree of `rev`. Lists the whole tree: `ls-tree`'s path
/// arguments are patterns, not literal names.
fn paths_in(git: &Git, rev: &str, paths: &[String]) -> Vec<String> {
    if paths.is_empty() {
        return Vec::new();
    }
    let tree: std::collections::BTreeSet<String> = git
        .run(&["ls-tree", "-r", "-z", "--name-only", rev])
        .map(|o| parse_paths(&o))
        .unwrap_or_default()
        .into_iter()
        .collect();
    paths.iter().filter(|p| tree.contains(*p)).cloned().collect()
}

fn read_remote(git: &Git, name: &str) -> Option<RemoteFacts> {
    let config = |key: String| git.run(&["config", "--get", &key]).ok().map(text);
    let url = config(format!("remote.{name}.url"))?;
    let refs = git
        .run(&["for-each-ref", REMOTE_REFS_FORMAT, &format!("refs/remotes/{name}/")])
        .map(|o| parse_remote_refs(&o))
        .unwrap_or_default();
    let tracking = git
        .run(BRANCH_UPSTREAM_ARGS)
        .map(|o| parse_upstreams(&o))
        .unwrap_or_default()
        .into_iter()
        .filter(|u| u.remote == name)
        .collect();
    Some(RemoteFacts { url, push_url: config(format!("remote.{name}.pushurl")), refs, tracking })
}

/// Reads what `needs` asks for. Only saving the working tree can fail: a hard reset must not
/// run without its backup. The exception is an index with unmerged entries, which
/// `git stash create` refuses to save ("needs merge"): the op runs without a backup then, and
/// the reset's confirmation (which lists the files it loses) is all that protects them.
pub fn read_facts(git: &Git, needs: &Needs, now: u64) -> Result<Facts, GitError> {
    let (head, branch) = head_and_branch(git);
    let oids = needs
        .revs
        .iter()
        .filter(|rev| !rev.starts_with('-'))
        .filter_map(|rev| Some((rev.clone(), text(git.run(&["rev-parse", "--verify", "-q", rev]).ok()?))))
        .collect();
    let upstream = needs.upstream_of.as_ref().and_then(|b| {
        let all = git.run(BRANCH_UPSTREAM_ARGS).map(|o| parse_upstreams(&o)).unwrap_or_default();
        all.into_iter().find(|u| &u.branch == b)
    });
    let remotes = match &needs.remote {
        Some(_) => {
            git.run(&["remote"]).map(|o| text(o).lines().map(str::to_string).collect()).unwrap_or_default()
        }
        None => Vec::new(),
    };
    let saved_worktree = if needs.save_worktree {
        match git.run(&["stash", "create", &format!("amalgum-undo-{now}")]) {
            Ok(out) => Some(text(out)).filter(|h| !h.is_empty()),
            Err(_) if has_conflicts(git) => None,
            Err(e) => return Err(e),
        }
    } else {
        None
    };
    let stash = needs.stash.and_then(|i| read_stash(git, i)).map(|mut s| {
        if let Some(current) = saved_worktree.as_deref().or(head.as_deref()) {
            s.in_current = paths_in(git, current, &s.tracked);
        }
        s
    });
    Ok(Facts {
        head,
        branch,
        oids,
        upstream,
        stash,
        remotes,
        remote: needs.remote.as_deref().and_then(|n| read_remote(git, n)),
        saved_worktree,
        now,
    })
}

/// The operation git left in progress, asked through its pseudo-refs so it works over ssh too
/// (`REBASE_HEAD` marks a rebase stopped on a conflict).
fn in_progress(git: &Git) -> Option<RepoOp> {
    let exists = |r: &str| git.run(&["rev-parse", "-q", "--verify", r]).is_ok();
    status::detect_op(|marker| match marker {
        "rebase-merge" | "rebase-apply" => exists("REBASE_HEAD"),
        "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD" => exists(marker),
        _ => false,
    })
}

fn has_conflicts(git: &Git) -> bool {
    git.run(STATUS_ARGS)
        .ok()
        .and_then(|o| status::parse(&o).ok())
        .is_some_and(|s| s.entries.iter().any(|e| e.is_conflicted()))
}

/// What [`OpError::Known::action`] calls `op`.
fn action(op: &Op) -> &'static str {
    match op {
        Op::Checkout { .. } | Op::CheckoutDetached { .. } | Op::CheckoutRemote { .. } => "Checkout",
        Op::CreateBranch { checkout: true, .. } => "Checkout",
        Op::Merge { .. } => "Merge",
        Op::Rebase { .. } => "Rebase",
        Op::CherryPick { .. } => "Cherry-pick",
        Op::Revert { .. } => "Revert",
        Op::Reset { .. } => "Reset",
        Op::StashPush { .. } => "Stash",
        Op::StashApply { .. } | Op::StashPop { .. } | Op::StashBranch { .. } => "Stash apply",
        Op::DeleteBranch { .. } => "Delete",
        _ => "Operation",
    }
}

/// Turns a failed forward command into what the UI reacts to.
fn failed(git: &Git, op: &Op, plan: &OpPlan, before: &Snapshot, error: GitError) -> OpError {
    match op {
        Op::Merge { .. } | Op::Rebase { .. } | Op::CherryPick { .. } | Op::Revert { .. } => {
            if let Some(repo_op) = in_progress(git) {
                let pending = Pending {
                    description: plan.description.clone(),
                    before: before.clone(),
                    inverse: plan.inverse.clone(),
                    refs: plan.refs.clone(),
                };
                return OpError::Conflict { op: Some(repo_op), error, pending: Some(Box::new(pending)) };
            }
        }
        Op::StashApply { .. } | Op::StashPop { .. } | Op::StashBranch { .. } if has_conflicts(git) => {
            return OpError::Conflict { op: None, error, pending: None };
        }
        _ => {}
    }
    let action = action(op);
    match classify(&error.stderr) {
        Some(Failure::NotFullyMerged { .. }) => {
            let commits = match op {
                Op::DeleteBranch { name, .. } => git
                    .run(&["rev-list", "--count", &format!("HEAD..{}", heads(name))])
                    .ok()
                    .and_then(|o| text(o).parse().ok()),
                _ => None,
            };
            let onto = head_and_branch(git).1;
            OpError::Known { action, failure: Failure::NotFullyMerged { commits, onto }, error: Some(error) }
        }
        Some(failure) => OpError::Known { action, failure, error: Some(error) },
        None => OpError::Git(error),
    }
}

/// Reads the facts `op` needs, plans it, runs it, and returns its journal entry (§5.18).
pub fn execute(git: &Git, op: &Op, now: u64) -> Result<Executed, OpError> {
    let facts = read_facts(git, &op.needs(), now)?;
    let plan = plan(op, &facts)?;
    let before = read_snapshot(git, &plan.refs, plan.stashes);
    let mut stdout = String::new();
    for (i, cmd) in plan.forward.iter().enumerate() {
        let args: Vec<&str> = cmd.iter().map(String::as_str).collect();
        let out = match (&plan.stdin, i + 1 == plan.forward.len()) {
            (Some(input), true) => git.run_with_stdin(&args, input.as_bytes()),
            _ => git.run(&args),
        };
        stdout.push_str(&String::from_utf8_lossy(&out.map_err(|e| failed(git, op, &plan, &before, e))?));
    }
    let after = read_snapshot(git, &plan.refs, plan.stashes);
    if let Op::StashPush { .. } = op
        && after.stashes.first() == before.stashes.first()
    {
        return Err(OpError::Known { action: "Stash", failure: Failure::NothingToStash, error: None });
    }
    if matches!(op, Op::Merge { .. } | Op::Rebase { .. }) && before == after {
        return Ok(Executed { entry: None, stdout }); // already up to date
    }
    let mut inverse = plan.inverse;
    let forward = match plan.redo {
        Redo::Forward => plan.forward,
        Redo::Plan(p) => p,
        Redo::KeepResetToAfter => vec![reset("--keep", after.head.as_deref().unwrap_or_default())],
        Redo::RefToAfter(name) => match after.refs.get(&name) {
            Some(oid) => vec![argv(&["update-ref", &name, oid])],
            None => plan.forward,
        },
        Redo::Restash { keep_index } => match (read_stash(git, 0), after.head.as_deref()) {
            (Some(stash), Some(head)) => {
                if keep_index {
                    // Undo: the stashed paths back to HEAD, then pop (see `plan`).
                    let in_head = paths_in(git, head, &stash.tracked);
                    let mut undo = restore_paths(
                        &StashFacts { untracked: Vec::new(), ..stash.clone() },
                        &in_head,
                        head,
                        head,
                    );
                    undo.extend(inverse.take().unwrap_or_default());
                    inverse = Some(undo);
                }
                // What the push left: the stash's index (`-k`) or HEAD.
                let left = if keep_index { format!("{}^2", stash.hash) } else { head.to_string() };
                let present = paths_in(git, &left, &stash.tracked);
                let mut p = check_unchanged(&stash);
                p.extend(restore_stash(&stash));
                p.extend(restore_paths(&stash, &present, &left, &left));
                p
            }
            _ => plan.forward,
        },
    };
    let entry = Entry {
        id: 0,
        time: now,
        description: plan.description,
        before,
        after,
        forward,
        inverse,
        undone: false,
    };
    Ok(Executed { entry: Some(entry), stdout })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Location;
    use crate::git::journal::{Failed, Journal};
    use crate::testutil::{TempRepo, hermetic_git};

    const NOW: u64 = 1_700_000_000;

    /// A TempRepo whose own config supplies what `Git::run` (not hermetic) needs: an identity
    /// and no signing, overriding whatever the machine's global config says.
    fn repo() -> TempRepo {
        let repo = TempRepo::new();
        for (key, value) in [
            ("user.name", "Ada Tester"),
            ("user.email", "ada@example.com"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
        ] {
            repo.git(&["config", key, value]);
        }
        repo
    }

    fn git(repo: &TempRepo) -> Git {
        Git::new(Location::Local { path: repo.path().to_path_buf() })
    }

    /// Output of a git command that may fail (empty then).
    fn out(repo: &TempRepo, args: &[&str]) -> String {
        let o = hermetic_git(repo.path()).args(args).output().expect("spawn git");
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    /// Everything an operation or its undo may touch: HEAD, refs, the index's and the working
    /// tree's contents, the stash list, and branch/remote config.
    fn state(repo: &TempRepo) -> String {
        let head = match out(repo, &["symbolic-ref", "-q", "HEAD"]) {
            s if s.is_empty() => out(repo, &["rev-parse", "HEAD"]),
            s => s,
        };
        let mut config: Vec<String> = out(repo, &["config", "--get-regexp", r"^(remote|branch)\."])
            .lines()
            .map(str::to_string)
            .collect();
        config.sort();
        [
            head,
            out(
                repo,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/heads",
                    "refs/tags",
                    "refs/remotes",
                ],
            ),
            out(repo, &["status", "--porcelain=v1", "-uall"]),
            out(repo, &["diff", "--binary"]),
            out(repo, &["diff", "--cached", "--binary"]),
            out(repo, &["stash", "list", "--format=%H %gs"]),
            config.join("\n"),
        ]
        .join("--\n")
    }

    /// One `Mod+Z` (`undo`) or `Mod+Shift+Z` through the journal, guarded like the pane does it.
    fn step(git: &Git, j: &mut Journal, undo: bool) -> Result<(), Failed<GitError>> {
        let entry = if undo { j.peek_undo() } else { j.peek_redo() }.expect("an entry").clone();
        let (refs, stashes) = entry.guard_scope();
        let snapshot = || read_snapshot(git, &refs, stashes);
        let run = |plan: &Plan| run_plan(git, plan);
        if undo { j.undo(&snapshot(), run, snapshot) } else { j.redo(&snapshot(), run, snapshot) }
    }

    /// Executes `op`, then undo → state before, redo → state after, and once more each way
    /// (so the redone entry's guard still holds). Leaves the repo in the after state.
    fn round_trip(repo: &TempRepo, op: Op) -> Executed {
        let git = git(repo);
        let before = state(repo);
        let executed = execute(&git, &op, NOW).unwrap_or_else(|e| panic!("{op:?}: {e:?}"));
        let after = state(repo);
        assert_ne!(before, after, "{op:?} changed nothing");
        let mut j = Journal::default();
        j.record(executed.entry.clone().expect("journaled"));
        for _ in 0..2 {
            step(&git, &mut j, true).unwrap_or_else(|e| panic!("undo {op:?}: {e:?}"));
            assert_eq!(state(repo), before, "undo {op:?}");
            step(&git, &mut j, false).unwrap_or_else(|e| panic!("redo {op:?}: {e:?}"));
            assert_eq!(state(repo), after, "redo {op:?}");
        }
        executed
    }

    fn head(repo: &TempRepo) -> String {
        repo.git(&["rev-parse", "HEAD"])
    }

    fn current_branch(repo: &TempRepo) -> String {
        out(repo, &["symbolic-ref", "-q", "--short", "HEAD"]).trim().to_string()
    }

    /// main: a — b; feat (from a): c touching another file.
    fn forked() -> (TempRepo, String) {
        let mut r = repo();
        let a = r.commit_file("f.txt", "1\n", "a");
        r.git(&["checkout", "-q", "-b", "feat"]);
        r.commit_file("g.txt", "feat\n", "c");
        r.git(&["checkout", "-q", "main"]);
        r.commit_file("h.txt", "main\n", "b");
        (r, a)
    }

    /// Like [`forked`] but feat and main both change f.txt.
    fn conflicting() -> TempRepo {
        let mut r = repo();
        r.commit_file("f.txt", "1\n", "a");
        r.git(&["checkout", "-q", "-b", "feat"]);
        r.commit_file("f.txt", "feat\n", "c");
        r.git(&["checkout", "-q", "main"]);
        r.commit_file("f.txt", "main\n", "b");
        r
    }

    fn remote_tracking(r: &TempRepo, remote: &str, branch: &str, at: &str) {
        r.git(&["remote", "add", remote, "nowhere.invalid:repo.git"]);
        r.git(&["update-ref", &format!("refs/remotes/{remote}/{branch}"), at]);
    }

    // ---- §5.9 branches ----------------------------------------------------------------------

    #[test]
    fn create_branch_and_check_it_out() {
        let (r, a) = forked();
        round_trip(&r, Op::CreateBranch { name: "fix/x".into(), base: a.clone(), checkout: true });
        assert_eq!(current_branch(&r), "fix/x");
        assert_eq!(r.git(&["rev-parse", "fix/x"]), a);
    }

    #[test]
    fn create_branch_without_checkout() {
        let (r, _) = forked();
        let e = round_trip(&r, Op::CreateBranch { name: "v2".into(), base: "feat".into(), checkout: false });
        assert_eq!(current_branch(&r), "main");
        assert_eq!(r.git(&["rev-parse", "v2"]), r.git(&["rev-parse", "feat"]));
        assert_eq!(e.entry.expect("entry").description, "create branch v2");
    }

    #[test]
    fn checkout_branch() {
        let (r, _) = forked();
        round_trip(&r, Op::Checkout { branch: "feat".into(), force: false });
        assert_eq!(current_branch(&r), "feat");
    }

    #[test]
    fn checkout_commit_detaches() {
        let (r, a) = forked();
        round_trip(&r, Op::CheckoutDetached { rev: a.clone() });
        assert_eq!(current_branch(&r), "");
        assert_eq!(head(&r), a);
    }

    #[test]
    fn checkout_remote_branch_creates_a_tracking_branch() {
        let (r, a) = forked();
        remote_tracking(&r, "origin", "topic", &a);
        round_trip(&r, Op::CheckoutRemote { remote: "origin".into(), branch: "topic".into() });
        assert_eq!(current_branch(&r), "topic");
        assert_eq!(r.git(&["rev-parse", "--abbrev-ref", "topic@{upstream}"]), "origin/topic");
    }

    #[test]
    fn rename_current_and_other_branch() {
        let (r, _) = forked();
        round_trip(&r, Op::RenameBranch { from: "main".into(), to: "trunk".into() });
        assert_eq!(current_branch(&r), "trunk");
        round_trip(&r, Op::RenameBranch { from: "feat".into(), to: "feature".into() });
        assert!(out(&r, &["rev-parse", "--verify", "-q", "feat"]).is_empty());
    }

    #[test]
    fn delete_branch_restores_hash_and_upstream_on_undo() {
        let (r, a) = forked();
        remote_tracking(&r, "origin", "old", &a);
        r.git(&["branch", "old", &a]);
        r.git(&["branch", "--set-upstream-to=origin/old", "old"]);
        round_trip(&r, Op::DeleteBranch { name: "old".into(), force: false });
        assert!(out(&r, &["rev-parse", "--verify", "-q", "refs/heads/old"]).is_empty());
    }

    #[test]
    fn delete_unmerged_branch_reports_its_commit_count_then_force_deletes() {
        let (r, _) = forked();
        let err =
            execute(&git(&r), &Op::DeleteBranch { name: "feat".into(), force: false }, NOW).unwrap_err();
        assert!(
            matches!(
                &err,
                OpError::Known {
                    failure: Failure::NotFullyMerged { commits: Some(1), onto: Some(_) },
                    error: Some(_),
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(err.to_string(), "Branch has 1 commit not on `main`");
        round_trip(&r, Op::DeleteBranch { name: "feat".into(), force: true });
    }

    #[test]
    fn set_and_unset_upstream() {
        let (r, a) = forked();
        remote_tracking(&r, "origin", "feat", &a);
        r.git(&["update-ref", "refs/remotes/origin/other", &a]);
        round_trip(&r, Op::SetUpstream { branch: "feat".into(), upstream: Some("origin/feat".into()) });
        assert_eq!(r.git(&["rev-parse", "--abbrev-ref", "feat@{upstream}"]), "origin/feat");
        // Changing an existing upstream: undo restores the previous one, not "unset".
        round_trip(&r, Op::SetUpstream { branch: "feat".into(), upstream: Some("origin/other".into()) });
        round_trip(&r, Op::SetUpstream { branch: "feat".into(), upstream: None });
        assert!(out(&r, &["rev-parse", "--abbrev-ref", "feat@{upstream}"]).is_empty());
    }

    #[test]
    fn merge_no_ff_and_ff() {
        let (r, _) = forked();
        round_trip(&r, Op::Merge { rev: "feat".into(), ff: FastForward::NoFf });
        assert_eq!(r.git(&["rev-list", "--count", "--merges", "HEAD"]), "1");

        let (r, _) = forked();
        r.git(&["checkout", "-q", "feat"]);
        r.git(&["reset", "-q", "--hard", "main~1"]);
        round_trip(&r, Op::Merge { rev: "main".into(), ff: FastForward::FfOnly });
        assert_eq!(r.git(&["rev-parse", "feat"]), r.git(&["rev-parse", "main"]));
    }

    #[test]
    fn merge_already_up_to_date_journals_nothing() {
        let (r, _) = forked();
        let e =
            execute(&git(&r), &Op::Merge { rev: "main~1".into(), ff: FastForward::Ff }, NOW).expect("merge");
        assert_eq!(e.entry, None);
    }

    #[test]
    fn merge_conflict_is_reported_as_merge_in_progress() {
        let r = conflicting();
        let err = execute(&git(&r), &Op::Merge { rev: "feat".into(), ff: FastForward::Ff }, NOW).unwrap_err();
        assert!(matches!(err, OpError::Conflict { op: Some(RepoOp::Merge), .. }), "{err:?}");
    }

    #[test]
    fn merge_undo_on_a_dirty_tree_lists_what_it_would_discard() {
        let (r, _) = forked();
        let e =
            execute(&git(&r), &Op::Merge { rev: "feat".into(), ff: FastForward::NoFf }, NOW).expect("merge");
        let inverse = e.entry.expect("entry").inverse.expect("inverse");
        let status = || status::parse(&r.git_raw(STATUS_ARGS)).expect("status");
        assert!(lost_changes(&inverse, &status()).is_empty(), "clean tree: nothing to confirm");
        r.write("f.txt", "edited\n");
        r.write("new.txt", "untracked survives a hard reset\n");
        assert_eq!(lost_changes(&inverse, &status()), vec!["f.txt".to_string()]);
        assert!(lost_changes(&vec![argv(&["reset", "--soft", "HEAD~1"])], &status()).is_empty());
    }

    #[test]
    fn rebase_onto() {
        let (r, _) = forked();
        r.git(&["checkout", "-q", "feat"]);
        round_trip(&r, Op::Rebase { onto: "main".into() });
        assert_eq!(r.git(&["rev-parse", "feat~1"]), r.git(&["rev-parse", "main"]));
    }

    #[test]
    fn rebase_conflict_is_reported_as_rebase_in_progress() {
        let r = conflicting();
        r.git(&["checkout", "-q", "feat"]);
        let err = execute(&git(&r), &Op::Rebase { onto: "main".into() }, NOW).unwrap_err();
        assert!(matches!(err, OpError::Conflict { op: Some(RepoOp::Rebase), .. }), "{err:?}");
    }

    #[test]
    fn checkout_that_would_overwrite_lists_the_files() {
        let r = conflicting();
        r.write("f.txt", "local edit\n");
        let err = execute(&git(&r), &Op::Checkout { branch: "feat".into(), force: false }, NOW).unwrap_err();
        match err {
            ref e @ OpError::Known {
                failure: Failure::WouldOverwrite { ref paths }, error: Some(_), ..
            } => {
                assert_eq!(paths, &vec!["f.txt".to_string()]);
                assert_eq!(e.to_string(), "Checkout would overwrite 1 file");
            }
            other => panic!("{other:?}"),
        }
    }

    // ---- §5.11 tags ----------------------------------------------------------------------------

    #[test]
    fn create_lightweight_and_annotated_tags() {
        let (r, a) = forked();
        round_trip(&r, Op::CreateTag { name: "v1".into(), target: a.clone(), message: None, sign: false });
        assert_eq!(r.git(&["cat-file", "-t", "refs/tags/v1"]), "commit");
        round_trip(
            &r,
            Op::CreateTag {
                name: "v2".into(),
                target: "HEAD".into(),
                message: Some("Release 2\n".into()),
                sign: false,
            },
        );
        assert_eq!(r.git(&["cat-file", "-t", "refs/tags/v2"]), "tag");
        assert_eq!(r.git(&["tag", "-l", "--format=%(contents:subject)", "v2"]), "Release 2");
    }

    #[test]
    fn delete_annotated_tag_restores_the_tag_object() {
        let (r, _) = forked();
        r.git(&["tag", "-a", "-m", "one", "v1"]);
        round_trip(&r, Op::DeleteTag { name: "v1".into() });
    }

    #[test]
    fn checkout_tag_detaches_and_branch_from_tag() {
        let (r, a) = forked();
        r.git(&["tag", "-a", "-m", "one", "v1", &a]);
        round_trip(&r, Op::CheckoutDetached { rev: "v1".into() });
        assert_eq!(head(&r), a);
        r.git(&["checkout", "-q", "main"]);
        round_trip(&r, Op::CreateBranch { name: "from-tag".into(), base: "v1".into(), checkout: true });
        assert_eq!(head(&r), a);
    }

    // ---- §5.12 stashes -------------------------------------------------------------------------

    /// forked() with a partially staged file (`MM`), an unstaged change, and an untracked file.
    fn dirty() -> TempRepo {
        let (r, _) = forked();
        r.write("f.txt", "staged\n");
        r.git(&["add", "f.txt"]);
        r.write("f.txt", "staged\nunstaged\n");
        r.write("h.txt", "unstaged\n");
        r.write("notes/new file.txt", "untracked\n");
        r
    }

    #[test]
    fn stash_push_with_untracked() {
        let r = dirty();
        round_trip(
            &r,
            Op::StashPush { message: Some("wip".into()), include_untracked: true, keep_index: false },
        );
        assert_eq!(out(&r, &["status", "--porcelain"]), "");
    }

    #[test]
    fn stash_push_keep_index() {
        let r = dirty();
        round_trip(&r, Op::StashPush { message: None, include_untracked: false, keep_index: true });
        assert_eq!(out(&r, &["status", "--porcelain"]), "M  f.txt\n?? notes/\n");
        assert_eq!(std::fs::read_to_string(r.path().join("f.txt")).expect("f.txt"), "staged\n");
    }

    #[test]
    fn nothing_to_stash() {
        let (r, _) = forked();
        let op = Op::StashPush { message: None, include_untracked: true, keep_index: false };
        let err = execute(&git(&r), &op, NOW).unwrap_err();
        assert_eq!(err, OpError::Known { action: "Stash", failure: Failure::NothingToStash, error: None });
    }

    /// dirty() stashed with `-u` (stash@{1}), plus a second stash of one file (stash@{0}).
    fn stashed() -> TempRepo {
        let r = dirty();
        r.git(&["stash", "push", "-u", "-m", "first"]);
        r.write("g.txt", "second\n");
        r.git(&["stash", "push", "-u", "-m", "second"]);
        r
    }

    #[test]
    fn stash_apply() {
        let r = stashed();
        round_trip(&r, Op::StashApply { index: 1 });
        assert!(r.path().join("notes/new file.txt").exists());
    }

    #[test]
    fn stash_pop() {
        let r = stashed();
        round_trip(&r, Op::StashPop { index: 0 });
        assert_eq!(out(&r, &["stash", "list", "--format=%gs"]), "On main: first\n");
        // Below the top: undo puts it back at its old position, not on top.
        let r = stashed();
        round_trip(&r, Op::StashPop { index: 1 });
        assert_eq!(out(&r, &["stash", "list", "--format=%gs"]), "On main: second\n");
    }

    #[test]
    fn stash_drop() {
        let r = stashed();
        round_trip(&r, Op::StashDrop { index: 1 });
    }

    #[test]
    fn stash_branch() {
        let r = stashed();
        round_trip(&r, Op::StashBranch { name: "from-stash".into(), index: 1 });
        assert_eq!(current_branch(&r), "from-stash");
    }

    #[test]
    fn stash_apply_conflict() {
        let mut r = repo();
        r.commit_file("f.txt", "1\n", "a");
        r.write("f.txt", "stashed\n");
        r.git(&["stash", "push", "-m", "s"]);
        r.commit_file("f.txt", "committed\n", "b");
        let err = execute(&git(&r), &Op::StashApply { index: 0 }, NOW).unwrap_err();
        assert!(matches!(err, OpError::Conflict { op: None, .. }), "{err:?}");
    }

    // ---- §5.16 history rewriting ---------------------------------------------------------------

    #[test]
    fn cherry_pick_one_and_several() {
        let (mut r, _) = forked();
        r.git(&["checkout", "-q", "feat"]);
        let d = r.commit_file("d.txt", "d\n", "d");
        r.git(&["checkout", "-q", "main"]);
        let e = round_trip(&r, Op::CherryPick { commits: vec!["feat~1".into(), d], record_origin: true });
        assert_eq!(e.entry.expect("entry").description, "cherry-pick 2 commits");
        assert!(r.git(&["log", "-1", "--format=%B"]).contains("(cherry picked from commit"));
        assert!(r.path().join("g.txt").exists() && r.path().join("d.txt").exists());
    }

    #[test]
    fn cherry_pick_conflict() {
        let r = conflicting();
        let err =
            execute(&git(&r), &Op::CherryPick { commits: vec!["feat".into()], record_origin: false }, NOW)
                .unwrap_err();
        assert!(matches!(err, OpError::Conflict { op: Some(RepoOp::CherryPick), .. }), "{err:?}");
    }

    #[test]
    fn revert_commit_and_merge_commit() {
        let (r, _) = forked();
        round_trip(&r, Op::Revert { commits: vec!["HEAD".into()], mainline: None });
        assert!(!r.path().join("h.txt").exists());

        let (r, _) = forked();
        r.git(&["merge", "-q", "--no-ff", "--no-edit", "feat"]);
        round_trip(&r, Op::Revert { commits: vec!["HEAD".into()], mainline: Some(1) });
        assert!(!r.path().join("g.txt").exists());
    }

    #[test]
    fn reset_soft_and_mixed() {
        let (r, a) = forked();
        round_trip(&r, Op::Reset { target: a.clone(), mode: ResetMode::Soft });
        assert_eq!(out(&r, &["status", "--porcelain"]), "A  h.txt\n");
        let (r, a) = forked();
        round_trip(&r, Op::Reset { target: a, mode: ResetMode::Mixed });
        assert_eq!(out(&r, &["status", "--porcelain"]), "?? h.txt\n");
    }

    #[test]
    fn reset_hard_saves_and_restores_the_working_tree() {
        let r = dirty();
        let a = r.git(&["rev-parse", "main~1"]);
        let e = round_trip(&r, Op::Reset { target: a.clone(), mode: ResetMode::Hard });
        assert_eq!(head(&r), a);
        let entry = e.entry.expect("entry");
        assert_eq!(entry.description, format!("reset main to {} (hard)", short_hash(&a)));
        let hidden = r.git(&["for-each-ref", "--format=%(refname)", "refs/amalgum/"]);
        assert!(hidden.starts_with(&format!("refs/amalgum/amalgum-undo-{NOW}-")), "{hidden}");
        assert!(entry.after.refs.contains_key(&hidden), "the guard covers the hidden stash ref");
    }

    #[test]
    fn reset_hard_on_a_clean_tree_saves_nothing() {
        let (r, a) = forked();
        round_trip(&r, Op::Reset { target: a, mode: ResetMode::Hard });
        assert_eq!(r.git(&["for-each-ref", "refs/amalgum/"]), "");
    }

    // ---- §5.10 remotes -------------------------------------------------------------------------

    #[test]
    fn add_rename_and_set_url_of_a_remote() {
        let (r, a) = forked();
        round_trip(&r, Op::AddRemote { name: "upstream".into(), url: "https://example.com/x.git".into() });
        r.git(&["update-ref", "refs/remotes/upstream/main", &a]);
        round_trip(&r, Op::RenameRemote { from: "upstream".into(), to: "fork".into() });
        assert_eq!(r.git(&["rev-parse", "refs/remotes/fork/main"]), a);
        round_trip(&r, Op::SetRemoteUrl { name: "fork".into(), url: "git@example.com:y.git".into() });
        assert_eq!(r.git(&["remote", "get-url", "fork"]), "git@example.com:y.git");
    }

    #[test]
    fn remove_remote_restores_refs_urls_and_tracking_branches() {
        let (r, a) = forked();
        remote_tracking(&r, "origin", "main", &a);
        r.git(&["update-ref", "refs/remotes/origin/feat", "feat"]);
        r.git(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"]);
        r.git(&["remote", "set-url", "--push", "origin", "push.invalid:repo.git"]);
        r.git(&["branch", "--set-upstream-to=origin/feat", "feat"]);
        round_trip(&r, Op::RemoveRemote { name: "origin".into() });
        assert_eq!(r.git(&["remote"]), "");
    }

    // ---- review regressions -------------------------------------------------------------------

    /// f.txt with seven lines on main; a stash that changes line 7; then line 1 staged.
    fn stash_under_staged_change() -> TempRepo {
        let mut r = repo();
        r.commit_file("f.txt", "1\n2\n3\n4\n5\n6\n7\n", "a");
        r.write("f.txt", "1\n2\n3\n4\n5\n6\nSTASHED\n");
        r.git(&["stash", "push", "-m", "s"]);
        r.write("f.txt", "MINE-STAGED\n2\n3\n4\n5\n6\n7\n");
        r.git(&["add", "f.txt"]);
        r
    }

    #[test]
    fn stash_apply_and_pop_undo_keep_changes_staged_before() {
        let r = stash_under_staged_change();
        round_trip(&r, Op::StashApply { index: 0 });
        assert!(out(&r, &["diff", "--cached"]).contains("+MINE-STAGED"));
        let r = stash_under_staged_change();
        round_trip(&r, Op::StashPop { index: 0 });
    }

    #[test]
    fn stash_push_redo_refuses_to_discard_edits_made_after_undo() {
        let r = dirty();
        let git = git(&r);
        let op = Op::StashPush { message: None, include_untracked: false, keep_index: false };
        let mut j = Journal::default();
        j.record(execute(&git, &op, NOW).expect("stash").entry.expect("entry"));
        step(&git, &mut j, true).expect("undo");
        r.write("h.txt", "edited after undo\n");
        assert!(step(&git, &mut j, false).is_err(), "redo must not run over the edit");
        assert_eq!(std::fs::read_to_string(r.path().join("h.txt")).expect("h.txt"), "edited after undo\n");
    }

    #[test]
    fn set_upstream_undo_survives_a_fetch_moving_the_remote_branch() {
        let (r, a) = forked();
        remote_tracking(&r, "origin", "feat", &a);
        let git = git(&r);
        let op = Op::SetUpstream { branch: "feat".into(), upstream: Some("origin/feat".into()) };
        let mut j = Journal::default();
        j.record(execute(&git, &op, NOW).expect("set upstream").entry.expect("entry"));
        r.git(&["update-ref", "refs/remotes/origin/feat", "main"]);
        step(&git, &mut j, true).expect("undo");
        assert!(out(&r, &["rev-parse", "--abbrev-ref", "feat@{upstream}"]).is_empty());
    }

    /// An unborn branch `fresh` with an empty index, next to `main` (one commit).
    fn unborn() -> (TempRepo, String) {
        let mut r = repo();
        let a = r.commit_file("f.txt", "1\n", "a");
        r.git(&["checkout", "-q", "--orphan", "fresh"]);
        r.git(&["rm", "-r", "-q", "-f", "."]);
        (r, a)
    }

    #[test]
    fn checkout_from_an_unborn_branch_undoes_back_to_it() {
        let (r, a) = unborn();
        remote_tracking(&r, "origin", "topic", &a);
        round_trip(&r, Op::CheckoutRemote { remote: "origin".into(), branch: "topic".into() });
        assert_eq!(current_branch(&r), "topic");
        let (r, _) = unborn();
        round_trip(&r, Op::Checkout { branch: "main".into(), force: false });
    }

    #[test]
    fn lightweight_tag_ignores_tag_gpgsign() {
        let (r, a) = forked();
        r.git(&["config", "tag.gpgSign", "true"]);
        r.git(&["config", "core.editor", "false"]);
        round_trip(&r, Op::CreateTag { name: "v1".into(), target: a, message: None, sign: false });
        let op = Op::CreateTag {
            name: "v2".into(),
            target: "HEAD".into(),
            message: Some("m".into()),
            sign: false,
        };
        round_trip(&r, op);
    }

    #[test]
    fn merge_continued_after_a_conflict_is_journaled() {
        let r = conflicting();
        let git = git(&r);
        let before = state(&r);
        let err = execute(&git, &Op::Merge { rev: "feat".into(), ff: FastForward::Ff }, NOW).unwrap_err();
        let OpError::Conflict { pending: Some(pending), .. } = err else { panic!("{err:?}") };
        assert_eq!(pending.clone().finish(&git, NOW), None, "still in progress");
        r.write("f.txt", "resolved\n");
        r.git(&["add", "f.txt"]);
        r.git(&["commit", "-q", "--no-edit"]);
        let after = state(&r);
        let mut j = Journal::default();
        j.record(pending.finish(&git, NOW).expect("entry"));
        step(&git, &mut j, true).expect("undo");
        assert_eq!(state(&r), before);
        step(&git, &mut j, false).expect("redo");
        assert_eq!(state(&r), after);
    }

    #[test]
    fn reset_hard_while_unmerged_runs_without_a_backup() {
        let mut r = repo();
        r.commit_file("f.txt", "1\n", "a");
        r.write("f.txt", "stashed\n");
        r.git(&["stash", "push", "-m", "s"]);
        r.commit_file("f.txt", "committed\n", "b");
        assert!(matches!(
            execute(&git(&r), &Op::StashApply { index: 0 }, NOW),
            Err(OpError::Conflict { .. })
        ));
        let e = execute(&git(&r), &Op::Reset { target: "HEAD".into(), mode: ResetMode::Hard }, NOW)
            .expect("reset");
        assert!(e.entry.is_some());
        assert_eq!(out(&r, &["status", "--porcelain"]), "");
    }

    #[test]
    fn reset_mixed_undo_restores_the_staged_changes() {
        let r = dirty();
        let a = r.git(&["rev-parse", "main~1"]);
        round_trip(&r, Op::Reset { target: a, mode: ResetMode::Mixed });
    }

    #[test]
    fn undoing_a_hard_reset_drops_its_hidden_ref() {
        let r = dirty();
        let git = git(&r);
        let op = Op::Reset { target: "main~1".into(), mode: ResetMode::Hard };
        let mut j = Journal::default();
        j.record(execute(&git, &op, NOW).expect("reset").entry.expect("entry"));
        assert_ne!(r.git(&["for-each-ref", HIDDEN_REFS]), "");
        step(&git, &mut j, true).expect("undo");
        assert_eq!(r.git(&["for-each-ref", HIDDEN_REFS]), "");
    }

    #[test]
    fn force_checkout_saves_and_restores_the_discarded_changes() {
        let r = conflicting();
        r.write("f.txt", "local edit\n");
        round_trip(&r, Op::Checkout { branch: "feat".into(), force: true });
        assert_eq!(current_branch(&r), "feat");
        assert_eq!(std::fs::read_to_string(r.path().join("f.txt")).expect("f.txt"), "feat\n");
    }

    #[test]
    fn revert_several_commits() {
        let (mut r, _) = forked();
        r.commit_file("i.txt", "i\n", "i");
        let e = round_trip(&r, Op::Revert { commits: vec!["HEAD".into(), "HEAD~1".into()], mainline: None });
        assert_eq!(e.entry.expect("entry").description, "revert 2 commits");
        assert!(!r.path().join("h.txt").exists() && !r.path().join("i.txt").exists());
    }

    // ---- plan() validation ---------------------------------------------------------------------

    fn facts(oids: &[(&str, &str)]) -> Facts {
        Facts {
            head: Some("h0".into()),
            branch: Some("main".into()),
            oids: oids.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Facts::default()
        }
    }

    fn plan_err(op: Op, facts: &Facts) -> String {
        plan(&op, facts).expect_err("plan refuses").0
    }

    #[test]
    fn plan_rejects_bad_names_and_existing_refs() {
        let f = facts(&[("HEAD^{commit}", "h0"), ("refs/heads/feat", "h1"), ("refs/tags/v1", "t1")]);
        let create =
            |name: &str| Op::CreateBranch { name: name.into(), base: "HEAD".into(), checkout: false };
        assert!(plan_err(create("bad name"), &f).starts_with("Invalid branch name `bad name`"));
        assert!(plan_err(create("x.lock"), &f).contains(".lock"));
        assert_eq!(plan_err(create("feat"), &f), "Branch `feat` already exists");
        assert_eq!(
            plan_err(Op::RenameBranch { from: "feat".into(), to: "main..x".into() }, &f),
            "Invalid branch name `main..x`: ref name may not contain '..'"
        );
        assert_eq!(
            plan_err(
                Op::CreateTag { name: "v1".into(), target: "HEAD".into(), message: None, sign: false },
                &f
            ),
            "Tag `v1` already exists"
        );
        assert_eq!(
            plan_err(Op::AddRemote { name: "o".into(), url: "--upload-pack=x".into() }, &f),
            "Invalid URL `--upload-pack=x`"
        );
    }

    #[test]
    fn plan_refuses_deleting_the_checked_out_branch() {
        let f = facts(&[("refs/heads/main", "h0")]);
        assert_eq!(
            plan_err(Op::DeleteBranch { name: "main".into(), force: true }, &f),
            "Can't delete `main`: it is checked out"
        );
    }

    #[test]
    fn plan_refuses_unknown_or_option_like_revisions() {
        let f = facts(&[]);
        assert_eq!(
            plan_err(Op::Merge { rev: "nope".into(), ff: FastForward::Ff }, &f),
            "Unknown revision `nope`"
        );
        assert_eq!(plan_err(Op::Rebase { onto: "--exec=x".into() }, &f), "Invalid revision `--exec=x`");
        assert_eq!(
            plan_err(Op::CherryPick { commits: vec![], record_origin: true }, &f),
            "No commits to cherry-pick"
        );
        assert_eq!(plan_err(Op::StashPop { index: 3 }, &f), "No stash@{3}");
        assert_eq!(plan_err(Op::Checkout { branch: "gone".into(), force: false }, &f), "No branch `gone`");
    }

    #[test]
    fn plan_asks_before_reusing_an_existing_local_branch_for_a_remote_checkout() {
        let f = facts(&[("refs/remotes/origin/feat", "h1"), ("refs/heads/feat", "h2")]);
        assert_eq!(
            plan_err(Op::CheckoutRemote { remote: "origin".into(), branch: "feat".into() }, &f),
            "A local branch `feat` already exists"
        );
    }

    #[test]
    fn plan_of_delete_restores_the_upstream_config() {
        let mut f = facts(&[("refs/heads/feat", "h1")]);
        f.upstream = Some(Upstream {
            branch: "feat".into(),
            remote: "origin".into(),
            merge: "refs/heads/feat".into(),
        });
        let p = plan(&Op::DeleteBranch { name: "feat".into(), force: false }, &f).expect("plan");
        assert_eq!(p.forward, vec![argv(&["branch", "-d", "feat"])]);
        assert_eq!(
            p.inverse,
            Some(vec![
                argv(&["branch", "--no-track", "feat", "h1"]),
                argv(&["config", "branch.feat.remote", "origin"]),
                argv(&["config", "branch.feat.merge", "refs/heads/feat"]),
            ])
        );
        assert_eq!(p.refs, vec!["refs/heads/feat".to_string()]);
    }

    #[test]
    fn plan_of_merge_undoes_with_reset_hard_to_the_journaled_hash() {
        let f = facts(&[("feat^{commit}", "h1")]);
        let p = plan(&Op::Merge { rev: "feat".into(), ff: FastForward::NoFf }, &f).expect("plan");
        assert_eq!(p.forward, vec![argv(&["merge", "--no-edit", "--no-ff", "feat"])]);
        assert_eq!(p.inverse, Some(vec![argv(&["reset", "--hard", "h0"])]));
        assert_eq!(p.redo, Redo::KeepResetToAfter);
        assert_eq!(p.refs, vec!["refs/heads/main".to_string()]);
    }

    #[test]
    fn stash_paths_are_literal_pathspecs() {
        let stash = StashFacts {
            hash: "s".into(),
            subject: "On main: x".into(),
            tracked: vec!["a*.txt".into(), "gone".into()],
            untracked: vec![":odd".into()],
            in_current: vec!["a*.txt".into()],
            ..StashFacts::default()
        };
        assert_eq!(
            restore_paths(&stash, &stash.in_current, "w", "i"),
            vec![
                argv(&["reset", "-q", "w", "--", ":(literal)a*.txt", ":(literal)gone"]),
                argv(&["clean", "-f", "-q", "--", ":(literal)a*.txt", ":(literal)gone"]),
                argv(&["checkout-index", "-f", "-q", "--", "a*.txt"]),
                argv(&["reset", "-q", "i", "--", ":(literal)a*.txt", ":(literal)gone"]),
                argv(&["clean", "-f", "-q", "--", ":(literal):odd"]),
            ]
        );
    }

    #[test]
    fn plan_of_set_upstream_guards_only_the_branch() {
        let f = facts(&[("refs/heads/feat", "h1"), ("refs/remotes/origin/feat", "r1")]);
        let op = Op::SetUpstream { branch: "feat".into(), upstream: Some("origin/feat".into()) };
        assert_eq!(plan(&op, &f).expect("plan").refs, vec!["refs/heads/feat".to_string()]);
    }

    #[test]
    fn plan_of_tags_never_signs_unless_asked() {
        let f = facts(&[("HEAD^{commit}", "h0")]);
        let tag = |message: Option<&str>, sign| Op::CreateTag {
            name: "v1".into(),
            target: "HEAD".into(),
            message: message.map(str::to_string),
            sign,
        };
        assert_eq!(
            plan(&tag(None, false), &f).expect("plan").forward,
            vec![argv(&["tag", "--no-sign", "v1", "h0"])]
        );
        assert_eq!(
            plan(&tag(Some("m"), true), &f).expect("plan").forward,
            vec![argv(&["tag", "-a", "-s", "-F", "-", "v1", "h0"])]
        );
        assert_eq!(plan_err(tag(None, true), &f), "A signed tag needs a message");
    }

    #[test]
    fn error_messages_name_the_action_and_the_branch() {
        let known = |action, failure| OpError::Known { action, failure, error: None };
        let overwrite = Failure::WouldOverwrite { paths: vec!["a".into(), "b".into()] };
        assert_eq!(known("Merge", overwrite).to_string(), "Merge would overwrite 2 files");
        let unmerged = Failure::NotFullyMerged { commits: Some(4), onto: None };
        assert_eq!(known("Delete", unmerged).to_string(), "Branch has 4 commits not on HEAD");
    }

    // ---- parsers and classifier ----------------------------------------------------------------

    #[test]
    fn parses_upstreams_with_dotted_branch_names() {
        let out = b"branch.release.1.remote\norigin\0branch.release.1.merge\nrefs/heads/release.1\0branch.half.remote\norigin\0";
        assert_eq!(
            parse_upstreams(out),
            vec![Upstream {
                branch: "release.1".into(),
                remote: "origin".into(),
                merge: "refs/heads/release.1".into()
            }]
        );
    }

    #[test]
    fn parses_remote_refs_from_real_git() {
        let (r, a) = forked();
        remote_tracking(&r, "origin", "main", &a);
        r.git(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"]);
        let refs =
            parse_remote_refs(&r.git_raw(&["for-each-ref", REMOTE_REFS_FORMAT, "refs/remotes/origin/"]));
        assert_eq!(
            refs,
            vec![
                RemoteRef {
                    name: "refs/remotes/origin/HEAD".into(),
                    oid: a.clone(),
                    symref: Some("refs/remotes/origin/main".into())
                },
                RemoteRef { name: "refs/remotes/origin/main".into(), oid: a, symref: None },
            ]
        );
    }

    #[test]
    fn parses_stash_subjects_from_real_git() {
        let r = stashed();
        let subjects = parse_stash_subjects(&r.git_raw(STASH_SUBJECT_ARGS));
        let subjects: Vec<&str> = subjects.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(subjects, vec!["On main: second", "On main: first"]);
    }

    #[test]
    fn classifies_real_git_failures() {
        let (r, _) = forked();
        let stderr = |args: &[&str]| {
            let o = hermetic_git(r.path()).args(args).output().expect("spawn git");
            assert!(!o.status.success());
            String::from_utf8_lossy(&o.stderr).into_owned()
        };
        assert_eq!(
            classify(&stderr(&["branch", "-d", "feat"])),
            Some(Failure::NotFullyMerged { commits: None, onto: None })
        );

        r.write("g.txt", "untracked in the way\n");
        assert_eq!(
            classify(&stderr(&["checkout", "feat"])),
            Some(Failure::WouldOverwrite { paths: vec!["g.txt".into()] })
        );
        assert_eq!(classify("fatal: something else\n"), None);
    }
}
