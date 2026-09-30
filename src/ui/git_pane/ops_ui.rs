//! Running the pane's local git operations (§5.9 branches, §5.10 remote management, §5.11 tags,
//! §5.16 cherry-pick / revert / reset, §5.21 worktrees) and every prompt around them: the W19
//! popovers, the fuzzy pickers, the §5.20 confirmations, the "Checkout would overwrite N files"
//! prompt, and the conflict hand-off (§5.17).
//!
//! Every operation is a `git::ops::Op` run by `ops::execute` on a worker thread
//! ([`GitPane::start_ops`]); its journal entry is recorded and saved on the UI thread exactly
//! like a checkout (§5.18). An op that stops on conflicts leaves its `ops::Pending` here; once
//! the repository no longer has an operation in progress, [`GitPane::finish_pending`] journals
//! it. What the prompts say comes from `model::git_menu`; this file only draws and dispatches.

use super::requests::SyncRequest;
use super::worker::Reply;
use super::{GitEvent, GitPane, Selection};
use crate::git::cmd::{Git, GitError};
use crate::git::journal::Entry;
use crate::git::log as gitlog;
use crate::git::ops::{self, Failure, FastForward, Op, OpError, Pending, ResetMode};
use crate::git::refs::RefKind;
use crate::git::worktree_ops::{self, WorktreeOp};
use crate::model::confirm::{Confirm, Decision};
use crate::model::git_menu::{self, CheckoutTarget, RemoteCheckout};
use crate::model::keymap::Preset;
use crate::model::settings::{self, Settings};
use crate::model::state::GitView;
use crate::model::theme::Token;
use crate::ui::chrome::Toast;
use crate::ui::dialogs::{self, FormOutcome, Popover};
use crate::ui::jobs;
use crate::ui::theme::Colors;

/// Settings the handlers need, refreshed from `Settings` every frame.
#[derive(Debug, Clone, Copy)]
struct Prefs {
    ff: FastForward,
    cherry_pick_x: bool,
    /// §5.20 "Don't ask again" for soft reset not ticked.
    confirm_soft_reset: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { ff: FastForward::Ff, cherry_pick_x: true, confirm_soft_reset: true }
    }
}

/// What a confirmed §5.20 dialog goes on to do.
#[derive(Debug, Clone)]
enum AfterConfirm {
    /// `branch -D`; option 0 (if `remote`) also deletes it there.
    DeleteBranch {
        name: String,
        remote: Option<(String, String)>,
    },
    /// Option `i` deletes the tag on `remotes[i]` too.
    DeleteTag {
        name: String,
        remotes: Vec<String>,
    },
    Reset {
        target: String,
        mode: ResetMode,
    },
    UndoRedo {
        is_undo: bool,
    },
    RemoveWorktree {
        path: String,
    },
    RemoveRemote {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PickKind {
    Upstream { branch: String },
    Merge,
    Rebase,
}

/// An open popover (W19 style) or picker.
#[derive(Debug, Clone)]
enum Prompt {
    CreateBranch {
        base: String,
        base_label: String,
        name: String,
        checkout: bool,
    },
    CreateTag {
        target: String,
        target_label: String,
        name: String,
        annotated: bool,
        message: String,
        sign: bool,
        sign_available: bool,
        push: bool,
    },
    RenameBranch {
        from: String,
        to: String,
    },
    RenameRemote {
        from: String,
        to: String,
    },
    RemoteUrl {
        name: String,
        url: String,
    },
    AddRemote {
        name: String,
        url: String,
    },
    /// §5.16 revert of a merge commit: `-m 1` or `-m 2`.
    Mainline {
        commit: String,
        choice: u8,
        choices: Vec<(u8, String)>,
    },
    /// The `m` / `r` / `p` popover stating the exact operation (§5.4); `Enter` runs it.
    Run {
        sentence: String,
        verb: &'static str,
        op: Op,
    },
    Pick {
        kind: PickKind,
        query: String,
        candidates: Vec<String>,
        selected: usize,
        primary: Option<String>,
    },
    /// §5.9 checkout of `origin/feat` while a local `feat` exists: ask what to do.
    RemoteCheckout {
        remote: String,
        branch: String,
        choice: RemoteCheckout,
        /// The new tracking branch's name, for [`RemoteCheckout::NewBranch`].
        name: String,
    },
}

impl Prompt {
    /// Prompts driven by bare `Enter` / arrows / `Esc` rather than a focused text field: they
    /// close when keyboard focus leaves the git pane, so typing in a terminal can't run them.
    fn keyboard_only(&self) -> bool {
        matches!(self, Prompt::Run { .. } | Prompt::Mainline { .. } | Prompt::Pick { .. })
    }
}

/// §5.9 "Checkout would overwrite N files" with **Stash and checkout**, **Force**, **Cancel**.
#[derive(Debug, Clone)]
struct Overwrite {
    target: CheckoutTarget,
    paths: Vec<String>,
}

/// The pane's operation state (one field on `GitPane`).
#[derive(Default)]
pub(super) struct OpsState {
    busy: bool,
    confirm: Option<(Confirm, AfterConfirm)>,
    prompt: Option<(egui::Pos2, Prompt)>,
    overwrite: Option<Overwrite>,
    /// An op stopped on conflicts, journaled once it is continued to the end (§5.17).
    pending: Option<Pending>,
    finishing: bool,
    /// Where keyboard-opened popovers appear: the pane's top-left corner.
    anchor: Option<egui::Pos2>,
    prefs: Prefs,
    /// Events from the public entry points (keymap, palette), which have no `events` list;
    /// handed out on the next `show`.
    queued: Vec<GitEvent>,
    /// The Refs view's selected row (§7.6 keys act on it).
    pub(super) refs_sel: Option<super::refs::RefsSel>,
    /// The app's keyboard focus is on this pane (not a terminal); set every frame by the app.
    pub(super) app_focused: bool,
    /// The remote the workspace is bound to (its sidebar group), from the app.
    bound_remote: Option<String>,
    /// `(commit, HEAD, loaded commits)` → whether the commit is in HEAD's history, so an open
    /// context menu doesn't walk the graph every frame.
    in_head_cache: Option<((String, String, usize), Option<bool>)>,
}

/// Worker results for this module (carried by `Reply::Ops`).
pub(super) enum OpsReply {
    /// Ops run in order until the first failure. `unmerged` lists a refused `branch -d`'s
    /// commits not on HEAD, for the W13 confirmation.
    Ran {
        done: Vec<(Op, Option<Entry>)>,
        failed: Option<(Op, OpError)>,
        unmerged: Vec<(String, String)>,
        then: Vec<SyncRequest>,
    },
    ResetPreview {
        target: String,
        mode: ResetMode,
        result: Result<Vec<(String, String)>, GitError>,
    },
    Finished {
        entry: Option<Entry>,
        in_progress: bool,
    },
    Worktree {
        op: WorktreeOp,
        result: Result<(), GitError>,
    },
    WorktreeDirty {
        path: String,
        result: Result<Vec<String>, GitError>,
    },
    TagSign(bool),
    Opened(Result<(), String>),
}

fn ff_of(ff: settings::FastForward) -> FastForward {
    match ff {
        settings::FastForward::Ff => FastForward::Ff,
        settings::FastForward::NoFf => FastForward::NoFf,
        settings::FastForward::FfOnly => FastForward::FfOnly,
    }
}

pub(super) fn checkout_op(target: &CheckoutTarget) -> Op {
    match target {
        CheckoutTarget::Branch(b) => Op::Checkout { branch: b.clone(), force: false },
        CheckoutTarget::Remote { remote, branch } => {
            Op::CheckoutRemote { remote: remote.clone(), branch: branch.clone() }
        }
        CheckoutTarget::Detached(rev) => Op::CheckoutDetached { rev: rev.clone() },
    }
}

fn checkout_target_of(op: &Op) -> Option<CheckoutTarget> {
    match op {
        Op::Checkout { branch, .. } => Some(CheckoutTarget::Branch(branch.clone())),
        Op::CheckoutRemote { remote, branch } => {
            Some(CheckoutTarget::Remote { remote: remote.clone(), branch: branch.clone() })
        }
        Op::CheckoutDetached { rev } => Some(CheckoutTarget::Detached(rev.clone())),
        _ => None,
    }
}

/// The success toast for a finished op.
fn success_message(op: &Op) -> String {
    let short = |r: &str| ops::short_hash(r).to_string();
    let rev = |r: &str| {
        if r.len() >= 20 && r.chars().all(|c| c.is_ascii_hexdigit()) { short(r) } else { r.to_string() }
    };
    match op {
        Op::CreateBranch { name, checkout: true, .. } => format!("Created and checked out `{name}`"),
        Op::CreateBranch { name, .. } => format!("Created branch `{name}`"),
        Op::Checkout { branch, .. } => format!("Checked out `{branch}`"),
        Op::CheckoutDetached { rev: r } => format!("Checked out {} (detached)", rev(r)),
        Op::CheckoutRemote { remote, branch } => {
            format!("Checked out `{branch}` tracking `{remote}/{branch}`")
        }
        Op::RenameBranch { from, to } => format!("Renamed `{from}` to `{to}`"),
        Op::DeleteBranch { name, .. } => format!("Deleted branch `{name}`"),
        Op::SetUpstream { branch, upstream: Some(u) } => format!("`{branch}` now tracks `{u}`"),
        Op::SetUpstream { branch, upstream: None } => format!("Unset the upstream of `{branch}`"),
        Op::Merge { rev: r, .. } => format!("Merged `{}`", rev(r)),
        Op::Rebase { onto } => format!("Rebased onto `{}`", rev(onto)),
        Op::CreateTag { name, .. } => format!("Created tag `{name}`"),
        Op::DeleteTag { name } => format!("Deleted tag `{name}`"),
        Op::StashPush { .. } => "Stashed changes".to_string(),
        Op::CherryPick { commits, .. } if commits.len() == 1 => {
            format!("Cherry-picked {}", short(&commits[0]))
        }
        Op::CherryPick { commits, .. } => format!("Cherry-picked {} commits", commits.len()),
        Op::Revert { commits, .. } if commits.len() == 1 => format!("Reverted {}", short(&commits[0])),
        Op::Revert { commits, .. } => format!("Reverted {} commits", commits.len()),
        Op::Reset { target, .. } => format!("Reset to {}", short(target)),
        Op::AddRemote { name, .. } => format!("Added remote `{name}`"),
        Op::RemoveRemote { name } => format!("Removed remote `{name}`"),
        Op::RenameRemote { from, to } => format!("Renamed remote `{from}` to `{to}`"),
        Op::SetRemoteUrl { name, .. } => format!("Updated the URL of `{name}`"),
        _ => "Done".to_string(),
    }
}

/// `(id, subject)` of the commits reachable from `include` but not `exclude`, newest first.
fn commits_between(git: &Git, include: &str, exclude: &str) -> Result<Vec<(String, String)>, GitError> {
    let not = format!("^{exclude}");
    let args = gitlog::log_args(&["-n", "500", include, &not, "--"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(git.run(&args).map(|o| gitlog::parse_log(&o))?.into_iter().map(|c| (c.id, c.subject)).collect())
}

fn run_ops(git: &Git, list: Vec<Op>, then: Vec<SyncRequest>, now: u64) -> OpsReply {
    let mut done = Vec::new();
    let mut unmerged = Vec::new();
    for op in list {
        match ops::execute(git, &op, now) {
            Ok(executed) => done.push((op, executed.entry)),
            Err(e) => {
                if let (
                    Op::DeleteBranch { name, .. },
                    OpError::Known { failure: Failure::NotFullyMerged { .. }, .. },
                ) = (&op, &e)
                {
                    unmerged =
                        commits_between(git, &format!("refs/heads/{name}"), "HEAD").unwrap_or_default();
                }
                return OpsReply::Ran { done, failed: Some((op, e)), unmerged, then };
            }
        }
    }
    OpsReply::Ran { done, failed: None, unmerged, then }
}

// ---- starting operations ------------------------------------------------------------------------

impl GitPane {
    fn toast(events: &mut Vec<GitEvent>, t: Toast) {
        events.push(GitEvent::Toast(t));
    }

    /// The app's per-frame context: whether its keyboard focus is on this pane, and the remote
    /// the workspace is bound to (§5.10, the sidebar group it sits in; `""` = none).
    pub fn set_app_context(&mut self, focused: bool, bound_remote: Option<&str>) {
        self.ops.app_focused = focused;
        if self.ops.bound_remote.as_deref() != bound_remote {
            self.ops.bound_remote = bound_remote.filter(|r| !r.is_empty()).map(str::to_string);
        }
    }

    /// The app's keyboard focus is on this pane (not a terminal).
    pub(super) fn app_focused(&self) -> bool {
        self.ops.app_focused
    }

    /// One ref-changing operation at a time (ops, undo / redo, checkout, worktree changes): the
    /// refusal toast while another runs.
    pub(super) fn busy_refusal(&self) -> Option<Toast> {
        (self.ops.busy || self.undo_busy).then(|| Toast::info("Another git operation is still running"))
    }

    /// Like [`Self::busy_refusal`], queueing the toast for entry points without an `events`
    /// list; `true` = refused.
    pub(super) fn refuse_if_busy(&mut self) -> bool {
        match self.busy_refusal() {
            Some(t) => {
                self.ops.queued.push(GitEvent::Toast(t));
                true
            }
            None => false,
        }
    }

    /// Marks the start / end of a ref-changing job that doesn't go through [`Self::start_ops`].
    pub(super) fn set_busy(&mut self, busy: bool) {
        self.ops.busy = busy;
    }

    /// Runs `list` in order on a worker thread, then emits `then` if all succeeded. One
    /// operation at a time: a second request while one runs is refused with a toast.
    pub(super) fn start_ops(
        &mut self,
        ctx: &egui::Context,
        list: Vec<Op>,
        then: Vec<SyncRequest>,
    ) -> Option<Toast> {
        if let Some(t) = self.busy_refusal() {
            return Some(t);
        }
        self.ops.busy = true;
        let git = self.git.clone();
        let now = crate::util::unix_now();
        jobs::spawn(ctx, &self.tx, move || Reply::Ops(run_ops(&git, list, then, now)));
        None
    }

    /// Like [`Self::start_ops`], reporting a refusal through `events`.
    pub(super) fn run_op(&mut self, ctx: &egui::Context, op: Op, events: &mut Vec<GitEvent>) {
        if let Some(t) = self.start_ops(ctx, vec![op], Vec::new()) {
            Self::toast(events, t);
        }
    }

    /// §5.9 Checkout of a branch, remote branch (tracking), commit, or tag. A remote branch
    /// whose local name is taken asks first ("asking only if the local name exists").
    pub(super) fn run_checkout(
        &mut self,
        ctx: &egui::Context,
        target: CheckoutTarget,
        events: &mut Vec<GitEvent>,
    ) {
        if let CheckoutTarget::Remote { remote, branch } = &target {
            let locals = self.names_of(RefKind::Local);
            if locals.contains(branch) {
                let name = git_menu::tracking_name_suggestion(remote, branch, &locals);
                let prompt = Prompt::RemoteCheckout {
                    remote: remote.clone(),
                    branch: branch.clone(),
                    choice: RemoteCheckout::Local,
                    name,
                };
                self.ops.prompt = Some((self.anchor(), prompt));
                return;
            }
        }
        self.run_op(ctx, checkout_op(&target), events);
    }

    /// The branch HEAD is on, for sentences and ops.
    pub(super) fn current_branch(&self) -> Option<String> {
        self.status.branch.head.clone()
    }

    pub(super) fn head_id(&self) -> Option<String> {
        self.status.branch.oid.clone()
    }

    fn names_of(&self, kind: RefKind) -> Vec<String> {
        self.refs_list.iter().filter(|r| r.kind == kind).map(|r| r.short.clone()).collect()
    }

    pub(super) fn remote_names(&self) -> Vec<String> {
        self.remotes.iter().map(|r| r.name.clone()).collect()
    }

    pub(super) fn default_remote(&self) -> Option<String> {
        git_menu::default_remote(
            self.ops.bound_remote.as_deref(),
            self.status.branch.upstream.as_deref(),
            &self.remote_names(),
        )
    }

    /// The fetch URL of `remote`, else of the default remote.
    pub(super) fn remote_url(&self, remote: Option<&str>) -> Option<String> {
        let name = remote.map(str::to_string).or_else(|| self.default_remote())?;
        self.remotes.iter().find(|r| r.name == name).map(|r| r.fetch_url.clone())
    }

    fn anchor(&self) -> egui::Pos2 {
        self.ops.anchor.unwrap_or(egui::pos2(80.0, 80.0))
    }

    /// Where a keyboard-opened popover appears (the pane's top-left corner).
    pub(super) fn ops_anchor(&self) -> egui::Pos2 {
        self.anchor()
    }

    /// Whether `commit` is in `head`'s history as far as the loaded graph knows
    /// (`git_menu::reaches`), cached while HEAD and the loaded log stay the same: an open context
    /// menu asks every frame.
    pub(super) fn in_head(&mut self, head: &str, commit: &str) -> Option<bool> {
        let key = (commit.to_string(), head.to_string(), self.commits.len());
        if let Some((k, v)) = &self.ops.in_head_cache
            && *k == key
        {
            return *v;
        }
        let by_id: std::collections::HashMap<&str, &[String]> =
            self.commits.iter().map(|c| (c.id.as_str(), c.parents.as_slice())).collect();
        let v = git_menu::reaches(head, commit, |id| by_id.get(id).copied());
        self.ops.in_head_cache = Some((key, v));
        v
    }

    /// The selected graph commit, else HEAD: the default base for create branch / tag (§5.9).
    fn selected_or_head(&self) -> Option<String> {
        match &self.selected {
            Selection::Commit(id) => Some(id.clone()),
            Selection::None => self.head_id(),
        }
    }

    fn label_of(&self, rev: &str) -> String {
        if self.commits.iter().any(|c| c.id == rev) || rev.len() == 40 {
            ops::short_hash(rev).to_string()
        } else {
            rev.to_string()
        }
    }

    /// W19: **New branch at 5e6f** (`at` = where the popover opens; `None`: the pane corner).
    pub(super) fn open_create_branch(&mut self, base: Option<String>, at: Option<egui::Pos2>) {
        let Some(base) = base.or_else(|| self.selected_or_head()) else { return };
        let base_label = self.label_of(&base);
        let at = at.unwrap_or_else(|| self.anchor());
        self.ops.prompt =
            Some((at, Prompt::CreateBranch { base, base_label, name: String::new(), checkout: true }));
    }

    /// §5.11 Create tag popover; reads `tag.gpgSign` to offer **Sign**.
    pub(super) fn open_create_tag(
        &mut self,
        ctx: &egui::Context,
        target: Option<String>,
        at: Option<egui::Pos2>,
    ) {
        let Some(target) = target.or_else(|| self.selected_or_head()) else { return };
        let target_label = self.label_of(&target);
        let at = at.unwrap_or_else(|| self.anchor());
        self.ops.prompt = Some((
            at,
            Prompt::CreateTag {
                target,
                target_label,
                name: String::new(),
                annotated: true,
                message: String::new(),
                sign: false,
                sign_available: false,
                push: false,
            },
        ));
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let on = git
                .run(&["config", "--bool", "--get", "tag.gpgSign"])
                .map(|o| String::from_utf8_lossy(&o).trim() == "true")
                .unwrap_or(false);
            Reply::Ops(OpsReply::TagSign(on))
        });
    }

    pub(super) fn open_rename_branch(&mut self, from: String, at: Option<egui::Pos2>) {
        let at = at.unwrap_or_else(|| self.anchor());
        self.ops.prompt = Some((at, Prompt::RenameBranch { to: from.clone(), from }));
    }

    pub(super) fn open_rename_remote(&mut self, from: String, at: Option<egui::Pos2>) {
        let at = at.unwrap_or_else(|| self.anchor());
        self.ops.prompt = Some((at, Prompt::RenameRemote { to: from.clone(), from }));
    }

    pub(super) fn open_remote_url(&mut self, name: String, at: Option<egui::Pos2>) {
        let at = at.unwrap_or_else(|| self.anchor());
        let url = self.remote_url(Some(&name)).unwrap_or_default();
        self.ops.prompt = Some((at, Prompt::RemoteUrl { name, url }));
    }

    pub(super) fn open_add_remote(&mut self, at: Option<egui::Pos2>) {
        let at = at.unwrap_or_else(|| self.anchor());
        self.ops.prompt = Some((at, Prompt::AddRemote { name: String::new(), url: String::new() }));
    }

    /// The anchored `m` / `r` / `p` popover (§5.4).
    pub(super) fn open_run_prompt(&mut self, sentence: String, verb: &'static str, op: Op) {
        let at = self.anchor();
        self.ops.prompt = Some((at, Prompt::Run { sentence, verb, op }));
    }

    /// §5.9 **Set upstream…**: remote branches with a fuzzy filter, the bound remote first.
    pub(super) fn open_upstream_picker(&mut self, branch: String, at: Option<egui::Pos2>) {
        let candidates = self.names_of(RefKind::Remote);
        self.open_picker(PickKind::Upstream { branch }, candidates, at);
    }

    /// Palette **Merge…** / **Rebase…**: every branch but the current one.
    pub(super) fn open_branch_picker(&mut self, merge: bool) {
        let current = self.current_branch();
        let mut candidates: Vec<String> =
            self.names_of(RefKind::Local).into_iter().filter(|b| Some(b) != current.as_ref()).collect();
        candidates.extend(self.names_of(RefKind::Remote));
        let kind = if merge { PickKind::Merge } else { PickKind::Rebase };
        self.open_picker(kind, candidates, None);
    }

    fn open_picker(&mut self, kind: PickKind, candidates: Vec<String>, at: Option<egui::Pos2>) {
        let at = at.unwrap_or_else(|| self.anchor());
        let primary = self.default_remote();
        self.ops.prompt =
            Some((at, Prompt::Pick { kind, query: String::new(), candidates, selected: 0, primary }));
    }

    /// §5.16 Revert: a merge commit asks for the mainline first.
    pub(super) fn start_revert(
        &mut self,
        ctx: &egui::Context,
        id: &str,
        at: Option<egui::Pos2>,
        events: &mut Vec<GitEvent>,
    ) {
        let parents = self.commits.iter().find(|c| c.id == id).map(|c| c.parents.clone()).unwrap_or_default();
        if parents.len() > 1 {
            let described: Vec<(String, Option<String>)> = parents
                .iter()
                .map(|p| (p.clone(), self.commits.iter().find(|c| &c.id == p).map(|c| c.subject.clone())))
                .collect();
            let choices = git_menu::mainline_choices(&described);
            let at = at.unwrap_or_else(|| self.anchor());
            self.ops.prompt = Some((at, Prompt::Mainline { commit: id.to_string(), choice: 1, choices }));
        } else {
            self.run_op(ctx, Op::Revert { commits: vec![id.to_string()], mainline: None }, events);
        }
    }

    pub(super) fn start_cherry_pick(&mut self, ctx: &egui::Context, id: &str, events: &mut Vec<GitEvent>) {
        let op =
            Op::CherryPick { commits: vec![id.to_string()], record_origin: self.ops.prefs.cherry_pick_x };
        self.run_op(ctx, op, events);
    }

    pub(super) fn merge_op(&self, rev: String) -> Op {
        Op::Merge { rev, ff: self.ops.prefs.ff }
    }

    pub(super) fn cherry_pick_op(&self, id: String) -> Op {
        Op::CherryPick { commits: vec![id], record_origin: self.ops.prefs.cherry_pick_x }
    }

    /// §5.16 Reset: list the commits leaving the branch (worker), then confirm (§5.20).
    pub(super) fn start_reset(&mut self, ctx: &egui::Context, target: String, mode: ResetMode) {
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let result = commits_between(&git, "HEAD", &target);
            Reply::Ops(OpsReply::ResetPreview { target, mode, result })
        });
    }

    /// §5.9 Delete: `branch -d`; an unmerged branch comes back as the W13 confirmation.
    pub(super) fn start_delete_branch(
        &mut self,
        ctx: &egui::Context,
        name: String,
        events: &mut Vec<GitEvent>,
    ) {
        self.run_op(ctx, Op::DeleteBranch { name, force: false }, events);
    }

    /// §5.11 Delete tag: always confirms, offering **Also delete on <remote>**.
    pub(super) fn start_delete_tag(&mut self, name: String) {
        let remotes = self.remote_names();
        let confirm = git_menu::delete_tag_confirm(&name, &remotes);
        self.ops.confirm = Some((confirm, AfterConfirm::DeleteTag { name, remotes }));
    }

    /// §5.10 Remove remote: confirm listing its remote-tracking branches.
    pub(super) fn start_remove_remote(&mut self, name: String) {
        let prefix = format!("{name}/");
        let tracking: Vec<String> =
            self.names_of(RefKind::Remote).into_iter().filter(|r| r.starts_with(&prefix)).collect();
        let confirm = git_menu::remove_remote_confirm(&name, &tracking);
        self.ops.confirm = Some((confirm, AfterConfirm::RemoveRemote { name }));
    }

    /// §5.21 Remove: a dirty worktree confirms with its file list first.
    pub(super) fn start_remove_worktree(&mut self, ctx: &egui::Context, path: String) {
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let result = worktree_ops::dirty_files(&git, &path);
            Reply::Ops(OpsReply::WorktreeDirty { path, result })
        });
    }

    pub(super) fn run_worktree_op(&mut self, ctx: &egui::Context, op: WorktreeOp) {
        if self.refuse_if_busy() {
            return;
        }
        self.ops.busy = true;
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let result = op.run(&git);
            Reply::Ops(OpsReply::Worktree { op, result })
        });
    }

    /// Opens a URL in the default browser, off the UI thread.
    pub(super) fn open_url(&mut self, ctx: &egui::Context, url: String) {
        jobs::spawn(ctx, &self.tx, move || {
            Reply::Ops(OpsReply::Opened(
                crate::platform::open_in_default_app(&url).map_err(|e| e.to_string()),
            ))
        });
    }

    /// §5.21 **Reveal in file manager** (local repositories only).
    pub(super) fn reveal(&mut self, ctx: &egui::Context, path: String) {
        jobs::spawn(ctx, &self.tx, move || {
            let p = std::path::PathBuf::from(path);
            Reply::Ops(OpsReply::Opened(
                crate::platform::reveal_in_file_manager(&p).map_err(|e| e.to_string()),
            ))
        });
    }

    /// Before undo/redo: a plan that hard-resets over uncommitted changes confirms first
    /// (`ops::lost_changes`, §5.9 "confirm if working tree dirty"). `true` = a confirmation is
    /// now showing, or another operation is still running (a toast says so), and the caller
    /// must not run the plan yet.
    pub(super) fn confirm_lossy_undo(&mut self, is_undo: bool) -> bool {
        if self.refuse_if_busy() {
            return true;
        }
        let entry = if is_undo { self.journal.peek_undo() } else { self.journal.peek_redo() };
        let Some(entry) = entry else { return false };
        let plan = if is_undo { entry.inverse.clone().unwrap_or_default() } else { entry.forward.clone() };
        let lost = ops::lost_changes(&plan, &self.status);
        if lost.is_empty() {
            return false;
        }
        let confirm = git_menu::lossy_undo_confirm(is_undo, &entry.description, &lost);
        self.ops.confirm = Some((confirm, AfterConfirm::UndoRedo { is_undo }));
        true
    }

    /// The journal's next undo / redo descriptions ("commit ab12cd3").
    pub(super) fn undo_redo_descriptions(&self) -> (Option<String>, Option<String>) {
        (
            self.journal.peek_undo().map(|e| e.description.clone()),
            self.journal.peek_redo().map(|e| e.description.clone()),
        )
    }
}

// ---- entry points for the app: keymap actions and palette commands ----------------------------

impl GitPane {
    /// Keymap / palette actions that act in the git pane (§7.1 `Mod+B`, `Mod+Shift+T`; §7.3
    /// graph actions chosen from the palette). `false`: not one of this pane's actions.
    pub fn run_action(&mut self, ctx: &egui::Context, action: crate::model::keymap::Action) -> bool {
        use crate::model::keymap::Action as A;
        let mut events = Vec::new();
        let selected = match &self.selected {
            Selection::Commit(id) => self.commits.iter().find(|c| &c.id == id).cloned(),
            Selection::None => None,
        };
        match action {
            A::NewBranch | A::CreateBranchAtSelectedCommit => self.open_create_branch(None, None),
            A::NewTag | A::CreateTagAtSelectedCommit => self.open_create_tag(ctx, None, None),
            A::CheckoutSelected
            | A::MergeSelectedIntoCurrent
            | A::RebaseCurrentOntoSelected
            | A::CherryPickSelectedOntoCurrent => {
                let Some(commit) = selected else {
                    self.ops.queued.push(GitEvent::Toast(Toast::info("Select a commit in the graph first")));
                    return true;
                };
                self.selected_commit_action(ctx, &commit, action, &mut events);
            }
            A::InteractiveRebaseFromSelected
            | A::SquashSelectedIntoParent
            | A::FixupSelectedIntoParent
            | A::EditSelectedCommitMessage => self.rebase_action(ctx, action),
            A::SetUpstream => match self.current_branch() {
                Some(b) => self.open_upstream_picker(b, None),
                None => events
                    .push(GitEvent::Toast(Toast::info("HEAD is detached: no branch to set an upstream for"))),
            },
            _ => return false,
        }
        self.ops.queued.extend(events);
        true
    }

    /// Palette **Merge…** / **Rebase…**: pick the branch to merge into / rebase the current one onto.
    pub fn open_merge_picker(&mut self, merge: bool) {
        self.open_branch_picker(merge);
    }

    /// Tag names for the palette's Tags group (§5.19).
    pub fn tags(&self) -> Vec<String> {
        self.names_of(RefKind::Tag)
    }

    /// §5.11 Checkout tag (detached), from the palette.
    pub fn checkout_tag(&mut self, ctx: &egui::Context, name: String) {
        let mut events = Vec::new();
        self.run_checkout(ctx, CheckoutTarget::Detached(name), &mut events);
        self.ops.queued.extend(events);
    }

    pub(super) fn ops_prompt_open(&self) -> bool {
        self.ops.prompt.is_some() || self.ops.confirm.is_some() || self.ops.overwrite.is_some()
    }
}

// ---- replies ------------------------------------------------------------------------------------

impl GitPane {
    pub(super) fn apply_ops_reply(
        &mut self,
        ctx: &egui::Context,
        reply: OpsReply,
        events: &mut Vec<GitEvent>,
    ) {
        match reply {
            OpsReply::Ran { done, failed, unmerged, then } => {
                self.ops.busy = false;
                let mut recorded = false;
                let mut last_message = None;
                for (op, entry) in done {
                    match entry {
                        Some(entry) => {
                            self.journal.record(entry);
                            recorded = true;
                            last_message = Some(Toast::success(success_message(&op)));
                        }
                        None => last_message = Some(Toast::info("Already up to date")),
                    }
                }
                if recorded {
                    let _ = self.journal.save(&self.journal_path);
                }
                match failed {
                    None => {
                        if let Some(t) = last_message {
                            Self::toast(events, t);
                        }
                        events.extend(then.into_iter().map(GitEvent::Sync));
                    }
                    Some((op, e)) => self.op_failed(op, e, unmerged, events),
                }
                self.refresh_now(ctx);
            }
            OpsReply::ResetPreview { target, mode, result } => match result {
                Ok(leaving) => {
                    let lost = ops::lost_changes(
                        &vec![vec!["reset".to_string(), "--hard".to_string()]],
                        &self.status,
                    );
                    let branch = self.current_branch();
                    let confirm = git_menu::reset_confirm(branch.as_deref(), &target, mode, &leaving, &lost);
                    if mode == ResetMode::Soft && !self.ops.prefs.confirm_soft_reset {
                        self.run_op(ctx, Op::Reset { target, mode }, events);
                    } else {
                        self.ops.confirm = Some((confirm, AfterConfirm::Reset { target, mode }));
                    }
                }
                Err(e) => Self::toast(events, Toast::error(e.summary(), e.stderr.clone())),
            },
            OpsReply::Finished { entry, in_progress } => {
                self.ops.finishing = false;
                if in_progress {
                    return;
                }
                // Continued to the end (an entry) or aborted (none): either way it's settled.
                if self.ops.pending.take().is_some()
                    && let Some(entry) = entry
                {
                    let description = entry.description.clone();
                    self.journal.record(entry);
                    let _ = self.journal.save(&self.journal_path);
                    Self::toast(
                        events,
                        Toast::success(format!("Finished: {description} (undo is available)")),
                    );
                }
            }
            OpsReply::Worktree { op, result } => {
                self.ops.busy = false;
                match result {
                    Ok(()) => Self::toast(events, Toast::success(op.done_message())),
                    Err(e) => Self::toast(events, Toast::error(e.summary(), e.stderr.clone())),
                }
                self.refresh_now(ctx);
            }
            OpsReply::WorktreeDirty { path, result } => match result {
                Ok(files) if files.is_empty() => {
                    self.run_worktree_op(ctx, WorktreeOp::Remove { path, force: false });
                }
                Ok(files) => {
                    let confirm = git_menu::remove_worktree_confirm(&path, &files);
                    self.ops.confirm = Some((confirm, AfterConfirm::RemoveWorktree { path }));
                }
                Err(e) => Self::toast(events, Toast::error(e.summary(), e.stderr.clone())),
            },
            OpsReply::TagSign(on) => {
                if let Some((_, Prompt::CreateTag { sign, sign_available, .. })) = &mut self.ops.prompt {
                    *sign_available = on;
                    *sign = on;
                }
            }
            OpsReply::Opened(Err(e)) => Self::toast(events, Toast::error("Could not open", e)),
            OpsReply::Opened(Ok(())) => {}
        }
    }

    fn op_failed(&mut self, op: Op, e: OpError, unmerged: Vec<(String, String)>, events: &mut Vec<GitEvent>) {
        match e {
            OpError::Known { failure: Failure::WouldOverwrite { paths }, error, .. } => {
                match checkout_target_of(&op) {
                    Some(target) => self.ops.overwrite = Some(Overwrite { target, paths }),
                    None => {
                        let e = OpError::Known {
                            action: "Operation",
                            failure: Failure::WouldOverwrite { paths },
                            error,
                        };
                        Self::toast(events, known_toast(&e));
                    }
                }
            }
            OpError::Known { failure: Failure::NotFullyMerged { onto, commits }, error, action } => {
                match op {
                    Op::DeleteBranch { name, .. } => {
                        let remote_branches = self.names_of(RefKind::Remote);
                        let configured = self
                            .refs_list
                            .iter()
                            .find(|r| r.kind == RefKind::Local && r.short == name)
                            .and_then(|r| r.upstream.clone());
                        let upstream = git_menu::remote_branch_for(
                            &name,
                            configured.as_deref(),
                            self.default_remote().as_deref(),
                            &remote_branches,
                        );
                        let remote = upstream
                            .as_deref()
                            .and_then(|u| git_menu::split_remote_branch(u, &self.remote_names()));
                        let confirm = git_menu::delete_branch_confirm(
                            &name,
                            onto.as_deref(),
                            &unmerged,
                            upstream.as_deref(),
                        );
                        self.ops.confirm = Some((confirm, AfterConfirm::DeleteBranch { name, remote }));
                    }
                    _ => {
                        let e = OpError::Known {
                            action,
                            failure: Failure::NotFullyMerged { onto, commits },
                            error,
                        };
                        Self::toast(events, known_toast(&e));
                    }
                }
            }
            OpError::Conflict { error, pending, op: repo_op } => {
                let title =
                    OpError::Conflict { error: error.clone(), pending: None, op: repo_op }.to_string();
                self.ops.pending = pending.map(|p| *p);
                self.view = GitView::Changes;
                Self::toast(
                    events,
                    Toast::warning(format!("{title}: resolve them in Changes"), error.stderr),
                );
            }
            OpError::Plan(p) => Self::toast(events, Toast::error(p.0, String::new())),
            OpError::Git(g) => Self::toast(events, Toast::error(g.summary(), g.stderr.clone())),
            e @ OpError::Known { .. } => Self::toast(events, known_toast(&e)),
        }
    }

    /// After every in-progress-operation probe: once the repo has no operation in progress, a
    /// kept `Pending` is journaled (continued to the end) or dropped (aborted).
    pub(super) fn finish_pending(&mut self, ctx: &egui::Context) {
        if self.ops.finishing || self.op.is_some() {
            return;
        }
        let Some(pending) = self.ops.pending.clone() else { return };
        self.ops.finishing = true;
        let git = self.git.clone();
        let now = crate::util::unix_now();
        jobs::spawn(ctx, &self.tx, move || {
            let in_progress = ops::in_progress(&git).is_some();
            let entry = if in_progress { None } else { pending.finish(&git, now) };
            Reply::Ops(OpsReply::Finished { entry, in_progress })
        });
    }
}

fn known_toast(e: &OpError) -> Toast {
    let stderr = match e {
        OpError::Known { error: Some(g), .. } => g.stderr.clone(),
        _ => String::new(),
    };
    Toast::error(e.to_string(), stderr)
}

// ---- overlays -----------------------------------------------------------------------------------

enum PickOutcome {
    Open,
    Cancelled,
    Chosen(String),
}

/// A fuzzy picker styled like the W19 popover: filter field, up to 12 candidates, `↑/↓/Enter`,
/// `Esc`.
#[allow(clippy::too_many_arguments)]
fn picker(
    ctx: &egui::Context,
    colors: &Colors,
    at: egui::Pos2,
    title: &str,
    query: &mut String,
    candidates: &[String],
    primary: Option<&str>,
    selected: &mut usize,
    app_focused: bool,
) -> PickOutcome {
    let shown = git_menu::rank_candidates(candidates, primary, query);
    let id = egui::Id::new(("amalgum_git_picker", title));
    let field_id = id.with("filter");
    // Keys act only while the filter field has focus (like `Popover::show`), so typing in a
    // terminal never picks; `Esc` also when nothing has focus.
    let owns = app_focused && ctx.memory(|m| m.has_focus(field_id));
    let esc_ok = app_focused && (owns || ctx.memory(|m| m.focused().is_none()));
    let (up, down, enter, esc) = ctx.input_mut(|i| {
        let mut key = |ok: bool, k| ok && i.consume_key(egui::Modifiers::NONE, k);
        (
            key(owns, egui::Key::ArrowUp),
            key(owns, egui::Key::ArrowDown),
            key(owns, egui::Key::Enter),
            key(esc_ok, egui::Key::Escape),
        )
    });
    // Focus the field on the frame the picker opens only, never stealing it back afterwards.
    let pass = ctx.cumulative_pass_nr();
    let opening = ctx.data_mut(|d| {
        let last = d.get_temp::<u64>(id);
        d.insert_temp(id, pass);
        !last.is_some_and(|l| l + 1 >= pass)
    });
    if down && !shown.is_empty() {
        *selected = (*selected + 1).min(shown.len() - 1);
    }
    if up {
        *selected = selected.saturating_sub(1);
    }
    if *selected >= shown.len() {
        *selected = 0;
    }
    let mut outcome = if esc { PickOutcome::Cancelled } else { PickOutcome::Open };
    if enter && let Some(c) = shown.get(*selected) {
        outcome = PickOutcome::Chosen(c.clone());
    }
    egui::Area::new(id).order(egui::Order::Foreground).fixed_pos(at).constrain(true).show(ctx, |ui| {
        egui::Frame::default()
            .fill(colors.get(Token::BgRaised))
            .stroke(egui::Stroke::new(1.0, colors.get(Token::Border)))
            .corner_radius(8)
            .inner_margin(12)
            .show(ui, |ui| {
                ui.set_width(300.0);
                ui.label(egui::RichText::new(title).strong());
                ui.add_space(4.0);
                let field = ui.add(
                    egui::TextEdit::singleline(query)
                        .id(field_id)
                        .hint_text("Filter…")
                        .desired_width(f32::INFINITY),
                );
                if opening {
                    field.request_focus();
                }
                if field.changed() {
                    *selected = 0;
                }
                ui.add_space(4.0);
                if shown.is_empty() {
                    ui.colored_label(colors.get(Token::FgSecondary), "No branches match.");
                }
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for (i, c) in shown.iter().enumerate() {
                        let resp = ui.selectable_label(i == *selected, c);
                        if i == *selected && (up || down) {
                            resp.scroll_to_me(None);
                        }
                        if resp.clicked() {
                            outcome = PickOutcome::Chosen(c.clone());
                        }
                    }
                });
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Cancel").clicked() {
                        outcome = PickOutcome::Cancelled;
                    }
                    ui.colored_label(colors.get(Token::FgSecondary), "↑↓ Enter");
                });
            });
    });
    outcome
}

impl GitPane {
    /// Draws the confirmation, popover, and overwrite prompt this pane has open (called once per
    /// frame from `show`, after the active view).
    pub(super) fn show_ops_overlays(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        settings: &Settings,
        preset: Preset,
        events: &mut Vec<GitEvent>,
    ) {
        let ctx = ui.ctx().clone();
        self.ops.prefs = Prefs {
            ff: ff_of(settings.git.fast_forward),
            cherry_pick_x: settings.git.cherry_pick_x,
            confirm_soft_reset: settings.should_confirm(crate::model::confirm::ConfirmKind::ResetSoft),
        };
        self.ops.anchor = Some(ui.max_rect().left_top() + egui::vec2(24.0, 48.0));
        events.append(&mut self.ops.queued);

        if let Some((confirm, after)) = &mut self.ops.confirm {
            match dialogs::confirm_dialog(&ctx, colors, preset, confirm) {
                Some(Decision::Confirm) => {
                    let (confirm, after) = (confirm.clone(), after.clone());
                    self.ops.confirm = None;
                    if confirm.remember() {
                        events.push(GitEvent::DontAskAgain(confirm.kind));
                    }
                    self.after_confirm(&ctx, &confirm, after, events);
                }
                Some(Decision::Cancel) => self.ops.confirm = None,
                None => {}
            }
        }

        if !self.ops.app_focused && self.ops.prompt.as_ref().is_some_and(|(_, p)| p.keyboard_only()) {
            self.ops.prompt = None; // focus moved to a terminal (or elsewhere): never act on its keys
        }
        if let Some((at, mut prompt)) = self.ops.prompt.take() {
            let keep = self.show_prompt(&ctx, colors, at, &mut prompt, events);
            // A handler may have opened a new prompt; don't overwrite it.
            if keep && self.ops.prompt.is_none() {
                self.ops.prompt = Some((at, prompt));
            }
        }

        if let Some(ow) = self.ops.overwrite.clone() {
            self.show_overwrite(&ctx, colors, ow, events);
        }
    }

    fn after_confirm(
        &mut self,
        ctx: &egui::Context,
        confirm: &Confirm,
        after: AfterConfirm,
        events: &mut Vec<GitEvent>,
    ) {
        let checked = |i: usize| confirm.options.get(i).is_some_and(|o| o.checked);
        let result = match after {
            AfterConfirm::DeleteBranch { name, remote } => {
                let then = match remote {
                    Some((remote, branch)) if checked(0) => {
                        vec![SyncRequest::DeleteRemoteBranch { remote, branch }]
                    }
                    _ => Vec::new(),
                };
                self.start_ops(ctx, vec![Op::DeleteBranch { name, force: true }], then)
            }
            AfterConfirm::DeleteTag { name, remotes } => {
                let then = remotes
                    .into_iter()
                    .enumerate()
                    .filter(|(i, _)| checked(*i))
                    .map(|(_, remote)| SyncRequest::DeleteRemoteTag { remote, tag: name.clone() })
                    .collect();
                self.start_ops(ctx, vec![Op::DeleteTag { name }], then)
            }
            AfterConfirm::Reset { target, mode } => {
                self.start_ops(ctx, vec![Op::Reset { target, mode }], Vec::new())
            }
            AfterConfirm::UndoRedo { is_undo } => match self.busy_refusal() {
                Some(t) => Some(t),
                None => {
                    self.run_undo_redo(ctx, is_undo);
                    None
                }
            },
            AfterConfirm::RemoveWorktree { path } => {
                self.run_worktree_op(ctx, WorktreeOp::Remove { path, force: true });
                None
            }
            AfterConfirm::RemoveRemote { name } => {
                self.start_ops(ctx, vec![Op::RemoveRemote { name }], Vec::new())
            }
        };
        if let Some(t) = result {
            Self::toast(events, t);
        }
    }

    /// One frame of the open prompt; `false` once it closed.
    fn show_prompt(
        &mut self,
        ctx: &egui::Context,
        colors: &Colors,
        at: egui::Pos2,
        prompt: &mut Prompt,
        events: &mut Vec<GitEvent>,
    ) -> bool {
        let branches = self.names_of(RefKind::Local);
        let tags = self.names_of(RefKind::Tag);
        let remotes = self.remote_names();
        let default_remote = self.default_remote();
        let mut run: Option<(Vec<Op>, Vec<SyncRequest>)> = None;
        let outcome = match prompt {
            Prompt::CreateBranch { base, base_label, name, checkout } => {
                let err = git_menu::branch_name_error(name, &branches);
                let outcome = Popover::new("create-branch", format!("New branch at {base_label}"))
                    .primary("Create")
                    .show(ctx, colors, at, |f| {
                        f.text(name, "branch name", err.as_deref())
                            .checkbox(checkout, "Checkout after create");
                    });
                if outcome == FormOutcome::Submitted {
                    let op = Op::CreateBranch { name: name.clone(), base: base.clone(), checkout: *checkout };
                    run = Some((vec![op], Vec::new()));
                }
                outcome
            }
            Prompt::CreateTag {
                target,
                target_label,
                name,
                annotated,
                message,
                sign,
                sign_available,
                push,
            } => {
                let err = git_menu::tag_name_error(name, &tags);
                let push_label = default_remote.as_deref().map(|r| format!("Push to {r}"));
                let outcome = Popover::new("create-tag", format!("New tag at {target_label}"))
                    .primary("Create")
                    .show(ctx, colors, at, |f| {
                        f.text(name, "tag name", err.as_deref()).checkbox(annotated, "Annotated");
                        if *annotated {
                            let msg_err = message.trim().is_empty().then_some("");
                            f.text_area(message, "message", msg_err);
                            if *sign_available {
                                f.checkbox(sign, "Sign");
                            }
                        }
                        if let Some(label) = &push_label {
                            f.checkbox(push, label);
                        }
                    });
                if outcome == FormOutcome::Submitted {
                    let op = Op::CreateTag {
                        name: name.clone(),
                        target: target.clone(),
                        message: annotated.then(|| message.clone()),
                        sign: *annotated && *sign,
                    };
                    let then = if *push && push_label.is_some() {
                        vec![SyncRequest::PushTag { tag: name.clone(), remote: None }]
                    } else {
                        Vec::new()
                    };
                    run = Some((vec![op], then));
                }
                outcome
            }
            Prompt::RenameBranch { from, to } => {
                let others: Vec<String> = branches.iter().filter(|b| *b != from).cloned().collect();
                let err = git_menu::branch_name_error(to, &others).or_else(|| (to == from).then(String::new));
                let outcome = Popover::new("rename-branch", format!("Rename branch `{from}`"))
                    .primary("Rename")
                    .show(ctx, colors, at, |f| {
                        f.text(to, "new name", err.as_deref());
                    });
                if outcome == FormOutcome::Submitted {
                    run = Some((vec![Op::RenameBranch { from: from.clone(), to: to.clone() }], Vec::new()));
                }
                outcome
            }
            Prompt::RenameRemote { from, to } => {
                let others: Vec<String> = remotes.iter().filter(|r| *r != from).cloned().collect();
                let err = git_menu::remote_name_error(to, &others).or_else(|| (to == from).then(String::new));
                let outcome = Popover::new("rename-remote", format!("Rename remote `{from}`"))
                    .primary("Rename")
                    .show(ctx, colors, at, |f| {
                        f.text(to, "new name", err.as_deref());
                    });
                if outcome == FormOutcome::Submitted {
                    run = Some((vec![Op::RenameRemote { from: from.clone(), to: to.clone() }], Vec::new()));
                }
                outcome
            }
            Prompt::RemoteUrl { name, url } => {
                let err = git_menu::url_error(url);
                let outcome = Popover::new("remote-url", format!("Edit URL of `{name}`"))
                    .primary("Save")
                    .show(ctx, colors, at, |f| {
                        f.text(url, "URL", err.as_deref());
                    });
                if outcome == FormOutcome::Submitted {
                    let op = Op::SetRemoteUrl { name: name.clone(), url: url.trim().to_string() };
                    run = Some((vec![op], Vec::new()));
                }
                outcome
            }
            Prompt::AddRemote { name, url } => {
                let name_err = git_menu::remote_name_error(name, &remotes);
                let url_err = git_menu::url_error(url);
                let outcome =
                    Popover::new("add-remote", "Add remote").primary("Add").show(ctx, colors, at, |f| {
                        f.text(name, "name", name_err.as_deref()).text(url, "URL", url_err.as_deref());
                    });
                if outcome == FormOutcome::Submitted {
                    let op = Op::AddRemote { name: name.clone(), url: url.trim().to_string() };
                    run = Some((vec![op], Vec::new()));
                }
                outcome
            }
            Prompt::Mainline { commit, choice, choices } => {
                let options: Vec<(u8, &str)> = choices.iter().map(|(n, l)| (*n, l.as_str())).collect();
                let title = format!("Revert merge {}", ops::short_hash(commit));
                let mut outcome =
                    Popover::new("revert-mainline", title).primary("Revert").show(ctx, colors, at, |f| {
                        f.note("Revert the changes relative to which parent?")
                            .dropdown("Parent", choice, &options);
                    });
                if outcome == FormOutcome::Open {
                    outcome = self.bare_keys(ctx).unwrap_or(outcome);
                }
                if outcome == FormOutcome::Submitted {
                    let op = Op::Revert { commits: vec![commit.clone()], mainline: Some(*choice) };
                    run = Some((vec![op], Vec::new()));
                }
                outcome
            }
            Prompt::Run { sentence, verb, op } => {
                let mut outcome =
                    Popover::new("run-op", sentence.clone()).primary(*verb).show(ctx, colors, at, |f| {
                        f.note("Enter to run · Esc to cancel");
                    });
                if outcome == FormOutcome::Open {
                    outcome = self.bare_keys(ctx).unwrap_or(outcome);
                }
                if outcome == FormOutcome::Submitted {
                    run = Some((vec![op.clone()], Vec::new()));
                }
                outcome
            }
            Prompt::Pick { kind, query, candidates, selected, primary } => {
                let title = match kind {
                    PickKind::Upstream { branch } => format!("Set upstream of `{branch}`"),
                    PickKind::Merge => {
                        format!("Merge into `{}`", self.current_branch().unwrap_or("HEAD".into()))
                    }
                    PickKind::Rebase => {
                        format!("Rebase `{}` onto", self.current_branch().unwrap_or("HEAD".into()))
                    }
                };
                let focused = self.ops.app_focused;
                match picker(
                    ctx,
                    colors,
                    at,
                    &title,
                    query,
                    candidates,
                    primary.as_deref(),
                    selected,
                    focused,
                ) {
                    PickOutcome::Open => FormOutcome::Open,
                    PickOutcome::Cancelled => FormOutcome::Cancelled,
                    PickOutcome::Chosen(c) => {
                        let op = match kind {
                            PickKind::Upstream { branch } => {
                                Op::SetUpstream { branch: branch.clone(), upstream: Some(c) }
                            }
                            PickKind::Merge => self.merge_op(c),
                            PickKind::Rebase => Op::Rebase { onto: c },
                        };
                        run = Some((vec![op], Vec::new()));
                        FormOutcome::Submitted
                    }
                }
            }
            Prompt::RemoteCheckout { remote, branch, choice, name } => {
                let name_err = (*choice == RemoteCheckout::NewBranch)
                    .then(|| git_menu::branch_name_error(name, &branches))
                    .flatten();
                let local = format!("Checkout the local `{branch}`");
                let detached = format!("Checkout `{remote}/{branch}` detached");
                let options = [
                    (RemoteCheckout::Local, local.as_str()),
                    (RemoteCheckout::NewBranch, "New branch tracking it"),
                    (RemoteCheckout::Detached, detached.as_str()),
                ];
                let title = format!("Checkout `{remote}/{branch}`");
                let outcome =
                    Popover::new("remote-checkout", title).primary("Checkout").show(ctx, colors, at, |f| {
                        f.note(&format!("A local branch `{branch}` already exists."))
                            .dropdown("Do", choice, &options);
                        if *choice == RemoteCheckout::NewBranch {
                            f.text(name, "branch name", name_err.as_deref());
                        }
                    });
                if outcome == FormOutcome::Submitted {
                    run = Some((git_menu::remote_checkout_ops(remote, branch, *choice, name), Vec::new()));
                }
                outcome
            }
        };
        if let Some((list, then)) = run
            && let Some(t) = self.start_ops(ctx, list, then)
        {
            Self::toast(events, t);
        }
        outcome == FormOutcome::Open
    }

    fn show_overwrite(
        &mut self,
        ctx: &egui::Context,
        colors: &Colors,
        ow: Overwrite,
        events: &mut Vec<GitEvent>,
    ) {
        let n = ow.paths.len();
        let title = format!("Checkout would overwrite {n} file{}", if n == 1 { "" } else { "s" });
        let at = self.anchor();
        let (mut stash, mut force, mut cancel) = (false, false, false);
        egui::Area::new(egui::Id::new("amalgum_git_overwrite"))
            .order(egui::Order::Foreground)
            .fixed_pos(at)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(colors.get(Token::BgRaised))
                    .stroke(egui::Stroke::new(1.0, colors.get(Token::Border)))
                    .corner_radius(6)
                    .inner_margin(10)
                    .show(ui, |ui| {
                        ui.set_max_width(360.0);
                        ui.horizontal(|ui| {
                            ui.colored_label(colors.get(Token::Warning), "⚠");
                            ui.label(egui::RichText::new(&title).strong());
                        });
                        for p in ow.paths.iter().take(5) {
                            let text = egui::RichText::new(format!("  {p}"))
                                .font(egui::FontId::monospace(12.0))
                                .color(colors.get(Token::FgSecondary));
                            ui.add(egui::Label::new(text).truncate());
                        }
                        if n > 5 {
                            ui.colored_label(colors.get(Token::FgSecondary), format!("  +{} more", n - 5));
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            stash = ui.button("Stash and checkout").clicked();
                            if matches!(ow.target, CheckoutTarget::Branch(_)) {
                                let danger = egui::RichText::new("Force").color(colors.get(Token::Danger));
                                force = ui
                                    .button(danger)
                                    .on_hover_text("Discard the changes (undo restores them)")
                                    .clicked();
                            }
                            cancel = ui.button("Cancel").clicked();
                        });
                    });
            });
        // Buttons only: like a toast, it never takes `Esc` from a terminal.
        let list = if stash {
            let push = Op::StashPush { message: None, include_untracked: true, keep_index: false };
            Some(vec![push, checkout_op(&ow.target)])
        } else if force {
            match &ow.target {
                CheckoutTarget::Branch(b) => Some(vec![Op::Checkout { branch: b.clone(), force: true }]),
                _ => None,
            }
        } else {
            None
        };
        if stash || force || cancel {
            self.ops.overwrite = None;
        }
        if let Some(list) = list
            && let Some(t) = self.start_ops(ctx, list, Vec::new())
        {
            Self::toast(events, t);
        }
    }
}

impl GitPane {
    /// Bare `Enter` / `Esc` for a popover with no text field (the `m` / `r` / `p` prompt, the
    /// revert mainline): consumed only while the app's keyboard focus is on this pane and no
    /// text field has egui focus, so keys typed in a terminal or a field never reach it.
    fn bare_keys(&self, ctx: &egui::Context) -> Option<FormOutcome> {
        if !self.ops.app_focused || ctx.egui_wants_keyboard_input() {
            return None;
        }
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Enter) {
                Some(FormOutcome::Submitted)
            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                Some(FormOutcome::Cancelled)
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Location;
    use crate::testutil::TempRepo;

    // ---- keyboard ownership of the bare-key prompts ----

    fn enter() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// One frame of the pane's overlays with `events`; `field_focused` puts egui focus on a
    /// text field drawn first (a terminal's or sidebar's). Returns the events the pane emitted.
    fn overlay_frame(
        pane: &mut GitPane,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        field_focused: bool,
    ) -> Vec<GitEvent> {
        let colors = Colors::new(crate::model::theme::Mode::Light);
        let settings = Settings::default();
        let mut out = Vec::new();
        let mut text = String::new();
        let mut output = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
            let field = ui.text_edit_singleline(&mut text);
            if field_focused {
                field.request_focus();
            }
            pane.show_ops_overlays(ui, &colors, &settings, Preset::MacOs, &mut out);
        });
        output.textures_delta.clear();
        out
    }

    /// A `Run` prompt (the `m` popover) on a pane whose "running" flag is set, so a submit shows
    /// up as the busy refusal toast instead of spawning git.
    fn pane_with_run_prompt() -> GitPane {
        let mut pane = super::super::tests::new_test_pane();
        pane.ops.busy = true;
        pane.ops.prompt = Some((
            egui::pos2(10.0, 10.0),
            Prompt::Run {
                sentence: "Merge `feat` into `main`".into(),
                verb: "Merge",
                op: Op::Merge { rev: "feat".into(), ff: FastForward::Ff },
            },
        ));
        pane
    }

    fn submitted(events: &[GitEvent]) -> bool {
        events.iter().any(|e| matches!(e, GitEvent::Toast(_)))
    }

    #[test]
    fn enter_runs_the_prompt_while_the_pane_has_the_keyboard() {
        let (mut pane, ctx) = (pane_with_run_prompt(), egui::Context::default());
        pane.set_app_context(true, None);
        overlay_frame(&mut pane, &ctx, vec![], false);
        assert!(submitted(&overlay_frame(&mut pane, &ctx, vec![enter()], false)));
    }

    #[test]
    fn enter_typed_in_a_terminal_never_runs_the_prompt() {
        // App focus moved to a terminal: the prompt closes instead of taking its keys.
        let (mut pane, ctx) = (pane_with_run_prompt(), egui::Context::default());
        pane.set_app_context(false, None);
        assert!(!submitted(&overlay_frame(&mut pane, &ctx, vec![enter()], false)));
        assert!(pane.ops.prompt.is_none());
        // A focused text field elsewhere keeps its Enter too.
        let (mut pane, ctx) = (pane_with_run_prompt(), egui::Context::default());
        pane.set_app_context(true, None);
        overlay_frame(&mut pane, &ctx, vec![], true);
        assert!(!submitted(&overlay_frame(&mut pane, &ctx, vec![enter()], true)));
        assert!(pane.ops.prompt.is_some());
    }

    #[test]
    fn the_picker_takes_enter_only_from_its_own_field() {
        let pick = |pane: &mut GitPane| {
            pane.ops.busy = true;
            pane.ops.prompt = Some((
                egui::pos2(10.0, 10.0),
                Prompt::Pick {
                    kind: PickKind::Rebase,
                    query: String::new(),
                    candidates: vec!["feat".into()],
                    selected: 0,
                    primary: None,
                },
            ));
        };
        // Opened: its filter field has focus, Enter picks.
        let (mut pane, ctx) = (super::super::tests::new_test_pane(), egui::Context::default());
        pane.set_app_context(true, None);
        pick(&mut pane);
        overlay_frame(&mut pane, &ctx, vec![], false);
        assert!(submitted(&overlay_frame(&mut pane, &ctx, vec![enter()], false)));
        // Another field took focus after it opened (a terminal's): Enter is left alone.
        let (mut pane, ctx) = (super::super::tests::new_test_pane(), egui::Context::default());
        pane.set_app_context(true, None);
        pick(&mut pane);
        overlay_frame(&mut pane, &ctx, vec![], false);
        overlay_frame(&mut pane, &ctx, vec![], true);
        assert!(!submitted(&overlay_frame(&mut pane, &ctx, vec![enter()], true)));
    }

    #[test]
    fn default_remote_follows_the_bound_remote() {
        let mut pane = super::super::tests::new_test_pane();
        pane.remotes = ["origin", "fork"]
            .iter()
            .map(|n| crate::git::refs::Remote {
                name: n.to_string(),
                fetch_url: String::new(),
                push_url: String::new(),
            })
            .collect();
        assert_eq!(pane.default_remote().as_deref(), Some("origin"));
        pane.set_app_context(true, Some("fork"));
        assert_eq!(pane.default_remote().as_deref(), Some("fork"));
    }

    fn repo() -> TempRepo {
        let repo = TempRepo::new();
        for (k, v) in [("user.name", "T"), ("user.email", "t@example.com"), ("commit.gpgsign", "false")] {
            repo.git(&["config", k, v]);
        }
        repo
    }

    fn git(repo: &TempRepo) -> Git {
        Git::new(Location::Local { path: repo.path().to_path_buf() })
    }

    #[test]
    fn checkout_ops_round_trip_through_targets() {
        for t in [
            CheckoutTarget::Branch("feat".into()),
            CheckoutTarget::Remote { remote: "origin".into(), branch: "x".into() },
            CheckoutTarget::Detached("abc".into()),
        ] {
            assert_eq!(checkout_target_of(&checkout_op(&t)), Some(t));
        }
        assert_eq!(checkout_target_of(&Op::DeleteTag { name: "v".into() }), None);
    }

    #[test]
    fn success_messages_shorten_hashes_but_not_names() {
        let id = "0123456789abcdef0123456789abcdef01234567".to_string();
        assert_eq!(
            success_message(&Op::CheckoutDetached { rev: id.clone() }),
            "Checked out 0123456 (detached)"
        );
        assert_eq!(
            success_message(&Op::CheckoutDetached { rev: "v1.0".into() }),
            "Checked out v1.0 (detached)"
        );
        assert_eq!(success_message(&Op::Reset { target: id, mode: ResetMode::Hard }), "Reset to 0123456");
    }

    #[test]
    fn unmerged_delete_reports_the_commits_not_on_head() {
        let mut r = repo();
        r.commit_file("a.txt", "a\n", "base");
        r.git(&["checkout", "-q", "-b", "feat"]);
        r.commit_file("b.txt", "b\n", "Refresh tokens");
        r.git(&["checkout", "-q", "main"]);
        let reply =
            run_ops(&git(&r), vec![Op::DeleteBranch { name: "feat".into(), force: false }], Vec::new(), 1);
        let OpsReply::Ran { done, failed, unmerged, .. } = reply else { panic!() };
        assert!(done.is_empty());
        assert!(matches!(failed, Some((_, OpError::Known { failure: Failure::NotFullyMerged { .. }, .. }))));
        assert_eq!(unmerged.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(), ["Refresh tokens"]);
    }

    #[test]
    fn stash_and_checkout_runs_both_and_journals_both() {
        let mut r = repo();
        r.commit_file("f.txt", "1\n", "a");
        r.git(&["checkout", "-q", "-b", "feat"]);
        r.commit_file("f.txt", "feat\n", "c");
        r.git(&["checkout", "-q", "main"]);
        r.write("f.txt", "dirty\n");
        let g = git(&r);
        let OpsReply::Ran { failed, .. } =
            run_ops(&g, vec![Op::Checkout { branch: "feat".into(), force: false }], Vec::new(), 1)
        else {
            panic!()
        };
        let Some((op, OpError::Known { failure: Failure::WouldOverwrite { paths }, .. })) = failed else {
            panic!("{failed:?}")
        };
        assert_eq!(paths, ["f.txt"]);
        let push = Op::StashPush { message: None, include_untracked: true, keep_index: false };
        let target = checkout_target_of(&op).expect("a checkout");
        let OpsReply::Ran { done, failed, .. } = run_ops(&g, vec![push, checkout_op(&target)], Vec::new(), 2)
        else {
            panic!()
        };
        assert!(failed.is_none(), "{failed:?}");
        assert_eq!(done.len(), 2);
        assert!(done.iter().all(|(_, e)| e.is_some()));
        assert_eq!(r.git(&["symbolic-ref", "--short", "HEAD"]), "feat");
    }

    #[test]
    fn conflicted_merge_keeps_a_pending_that_finishes_after_continue() {
        let mut r = repo();
        r.commit_file("f.txt", "1\n", "a");
        r.git(&["checkout", "-q", "-b", "feat"]);
        r.commit_file("f.txt", "feat\n", "c");
        r.git(&["checkout", "-q", "main"]);
        r.commit_file("f.txt", "main\n", "b");
        let g = git(&r);
        let OpsReply::Ran { failed, .. } =
            run_ops(&g, vec![Op::Merge { rev: "feat".into(), ff: FastForward::Ff }], Vec::new(), 1)
        else {
            panic!()
        };
        let Some((_, OpError::Conflict { pending: Some(pending), .. })) = failed else {
            panic!("{failed:?}")
        };
        assert!(ops::in_progress(&g).is_some());
        assert_eq!(pending.clone().finish(&g, 2), None, "still in progress");

        r.write("f.txt", "resolved\n");
        r.git(&["add", "f.txt"]);
        r.git(&["-c", "core.editor=true", "merge", "--continue"]);
        assert!(ops::in_progress(&g).is_none());
        let entry = pending.finish(&g, 3).expect("journaled once continued");
        assert_eq!(entry.description, "merge feat");
    }

    #[test]
    fn commits_between_lists_what_a_reset_drops() {
        let mut r = repo();
        let a = r.commit("a");
        r.commit("b");
        r.commit("c");
        let got = commits_between(&git(&r), "HEAD", &a).expect("log");
        assert_eq!(got.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(), ["c", "b"]);
    }
}
