//! Fetch / pull / push decisions for the git pane toolbar (§5.10), headless: which remote a push
//! goes to, when to ask before a branch's first push, when a pull needs **Stash, pull, pop**,
//! which action toast follows a failure, and the status bar's sync labels. The UI
//! (`ui::git_pane::sync`) gathers the facts, asks these functions, and runs `git::net` on a
//! worker.

use crate::git::net::{Force, NetErrorKind, PullMode, Push, check_push_allowed};
use std::collections::BTreeSet;

/// The remote an upstream such as `origin/feat` lives on: the longest known remote name that
/// prefixes it followed by `/` (remote names may themselves contain `/`).
pub fn upstream_remote<'a>(upstream: &str, remotes: &'a [String]) -> Option<&'a str> {
    remotes
        .iter()
        .filter(|r| upstream.strip_prefix(r.as_str()).is_some_and(|rest| rest.starts_with('/')))
        .max_by_key(|r| r.len())
        .map(String::as_str)
}

/// Where a push, pull, or fetch goes when the user named no remote (§5.10): the workspace's
/// bound remote, else the branch upstream's remote, else the first remote.
pub fn default_remote(bound: Option<&str>, upstream: Option<&str>, remotes: &[String]) -> Option<String> {
    bound
        .filter(|b| !b.is_empty())
        .map(str::to_string)
        .or_else(|| upstream.and_then(|u| upstream_remote(u, remotes)).map(str::to_string))
        .or_else(|| remotes.first().cloned())
}

/// Everything [`plan_push`] looks at.
#[derive(Debug, Clone, Copy)]
pub struct PushFacts<'a> {
    /// The branch to push; `None` on a detached HEAD.
    pub branch: Option<&'a str>,
    /// Its upstream (`origin/feat`), if any.
    pub upstream: Option<&'a str>,
    /// A remote the user picked (**Push to…**, a context menu); `None` means the default.
    pub remote: Option<&'a str>,
    pub bound_remote: Option<&'a str>,
    pub remotes: &'a [String],
    pub force: Force,
    /// Settings → **Protected branches**.
    pub protected: &'a [String],
    /// Whether a branch without an upstream still gets the "set upstream?" toast: the setting
    /// **Always push new branches to bound remote without asking** is off and this branch was not
    /// asked about yet ([`NewBranchAsks`]).
    pub ask_new_branch: bool,
}

/// Why a push does not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Detached,
    NoRemote,
    /// Force push of a protected branch (§5.10): blocked entirely, with a toast.
    Protected(String),
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::Detached => "Can't push: HEAD is detached, not on a branch".to_string(),
            Refusal::NoRemote => "Can't push: this repository has no remote".to_string(),
            Refusal::Protected(branch) => format!("`{branch}` is protected: force push is blocked"),
        }
    }
}

/// What to do with a push request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushPlan {
    Run(Push),
    /// First push of a branch to its default remote: toast "Push `feat` to origin and set
    /// upstream?" with **Push** / **Choose remote…** first (§5.10), once per branch.
    AskSetUpstream(Push),
    /// Plain `--force`: the §5.20 confirmation first, every time.
    Confirm(Push),
    Refuse(Refusal),
}

/// §5.10 **Push**: a branch without an upstream is pushed with `-u`; protected branches are never
/// force-pushed (checked before any confirmation, so a doomed force push never asks).
pub fn plan_push(f: &PushFacts) -> PushPlan {
    let Some(branch) = f.branch else { return PushPlan::Refuse(Refusal::Detached) };
    let explicit = f.remote.is_some();
    let remote = match f.remote {
        Some(r) => r.to_string(),
        None => match default_remote(f.bound_remote, f.upstream, f.remotes) {
            Some(r) => r,
            None => return PushPlan::Refuse(Refusal::NoRemote),
        },
    };
    let set_upstream = f.upstream.is_none();
    let push = Push::Branch { remote, branch: branch.to_string(), set_upstream, force: f.force };
    if check_push_allowed(&push, f.protected).is_err() {
        return PushPlan::Refuse(Refusal::Protected(branch.to_string()));
    }
    if f.force == Force::Plain {
        PushPlan::Confirm(push)
    } else if set_upstream && !explicit && f.ask_new_branch {
        PushPlan::AskSetUpstream(push)
    } else {
        PushPlan::Run(push)
    }
}

/// The branches already asked "Push `feat` to origin and set upstream?" this session (§5.10:
/// "once per branch").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewBranchAsks {
    asked: BTreeSet<String>,
}

impl NewBranchAsks {
    /// Whether `branch`'s first push should still ask. `always_push` is the setting **Always
    /// push new branches to bound remote without asking**.
    pub fn should_ask(&self, branch: Option<&str>, always_push: bool) -> bool {
        !always_push && branch.is_some_and(|b| !self.asked.contains(b))
    }

    pub fn mark(&mut self, branch: &str) {
        self.asked.insert(branch.to_string());
    }
}

/// What to do with a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullPlan {
    Run {
        mode: PullMode,
        autostash: bool,
    },
    /// A rebase pull over uncommitted changes: toast **Stash, pull, pop** / **Cancel** (§5.10).
    AskStash(PullMode),
    /// Toast "No upstream for `feat`" with **Set upstream…**.
    NoUpstream(String),
    Detached,
}

/// §5.10 **Pull**. `dirty` counts tracked changes only: untracked files never block a rebase.
pub fn plan_pull(branch: Option<&str>, upstream: Option<&str>, mode: PullMode, dirty: bool) -> PullPlan {
    let Some(branch) = branch else { return PullPlan::Detached };
    if upstream.is_none() {
        PullPlan::NoUpstream(branch.to_string())
    } else if mode == PullMode::Rebase && dirty {
        PullPlan::AskStash(mode)
    } else {
        PullPlan::Run { mode, autostash: false }
    }
}

/// An action toast (§5.10): one line and its buttons. The last button always dismisses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    SetUpstream {
        branch: String,
    },
    PushNewBranch(Push),
    /// A non-fast-forward rejection. `lease_allowed` is false for a protected branch (§5.10: no
    /// force push at all) and for a push that already carried a lease.
    Rejected {
        push: Push,
        lease_allowed: bool,
    },
    StashPull(PullMode),
}

/// A button on a [`Prompt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    SetUpstream,
    Push,
    ChooseRemote,
    PullThenPush,
    ForceWithLease,
    StashPullPop,
    Cancel,
}

impl Prompt {
    /// The toast line, in the spec's words.
    pub fn title(&self) -> String {
        match self {
            Prompt::SetUpstream { branch } => format!("No upstream for `{branch}`"),
            Prompt::PushNewBranch(Push::Branch { remote, branch, .. }) => {
                format!("Push `{branch}` to {remote} and set upstream?")
            }
            Prompt::PushNewBranch(push) => push.description(),
            Prompt::Rejected { .. } => "Push rejected: remote has commits you don't have".to_string(),
            Prompt::StashPull(_) => "Uncommitted changes block a rebase pull".to_string(),
        }
    }

    pub fn choices(&self) -> Vec<(&'static str, Choice)> {
        match self {
            Prompt::SetUpstream { .. } => {
                vec![("Set upstream…", Choice::SetUpstream), ("Dismiss", Choice::Cancel)]
            }
            Prompt::PushNewBranch(_) => vec![
                ("Push", Choice::Push),
                ("Choose remote…", Choice::ChooseRemote),
                ("Cancel", Choice::Cancel),
            ],
            Prompt::Rejected { lease_allowed: true, .. } => vec![
                ("Pull then push", Choice::PullThenPush),
                ("Force push with lease", Choice::ForceWithLease),
                ("Cancel", Choice::Cancel),
            ],
            Prompt::Rejected { lease_allowed: false, .. } => {
                vec![("Pull then push", Choice::PullThenPush), ("Cancel", Choice::Cancel)]
            }
            Prompt::StashPull(_) => {
                vec![("Stash, pull, pop", Choice::StashPullPop), ("Cancel", Choice::Cancel)]
            }
        }
    }
}

/// The action toast that follows a failed pull, if the failure has one.
pub fn pull_failure_prompt(kind: &NetErrorKind, mode: PullMode) -> Option<Prompt> {
    match kind {
        NetErrorKind::NoUpstream(Some(branch)) => Some(Prompt::SetUpstream { branch: branch.clone() }),
        NetErrorKind::DirtyWorktree => Some(Prompt::StashPull(mode)),
        _ => None,
    }
}

/// The action toast that follows a failed push: only a rejected branch push has one. It offers
/// **Force push with lease** only when that push would be allowed: never on a protected branch,
/// and not again after a lease went stale (the same lease can't help).
pub fn push_failure_prompt(kind: &NetErrorKind, push: &Push, protected: &[String]) -> Option<Prompt> {
    match (kind, push) {
        (NetErrorKind::Rejected, Push::Branch { force, .. }) => Some(Prompt::Rejected {
            push: push.clone(),
            lease_allowed: *force == Force::No && check_push_allowed(&with_lease(push), protected).is_ok(),
        }),
        _ => None,
    }
}

/// The same branch push, forced with a lease (the rejected toast's **Force push with lease**).
pub fn with_lease(push: &Push) -> Push {
    match push {
        Push::Branch { remote, branch, set_upstream, .. } => Push::Branch {
            remote: remote.clone(),
            branch: branch.clone(),
            set_upstream: *set_upstream,
            force: Force::WithLease,
        },
        other => other.clone(),
    }
}

/// The success toast after a push; push has no undo, and the toast says so (§5.10).
pub fn pushed_message(push: &Push) -> String {
    let what = match push {
        Push::Branch { remote, branch, .. } => format!("Pushed `{branch}` to {remote}"),
        Push::Tag { remote, tag } => format!("Pushed tag `{tag}` to {remote}"),
        Push::AllTags { remote } => format!("Pushed all tags to {remote}"),
        Push::Delete { remote, refname } => {
            let short = refname
                .strip_prefix("refs/heads/")
                .or_else(|| refname.strip_prefix("refs/tags/"))
                .unwrap_or(refname);
            format!("Deleted `{short}` on {remote}")
        }
    };
    format!("{what} · Undo is not available for push")
}

/// The toolbar's push button: "Push → origin" (§5.10 "labels name it").
pub fn push_label(remote: Option<&str>) -> String {
    match remote {
        Some(r) => format!("Push → {r}"),
        None => "Push".to_string(),
    }
}

pub fn pull_mode_label(mode: PullMode) -> &'static str {
    match mode {
        PullMode::Merge => "Pull (merge)",
        PullMode::Rebase => "Pull (rebase)",
        PullMode::FastForwardOnly => "Pull (fast-forward only)",
    }
}

/// The status bar's "Last fetch 0s" / "Last fetch 42s" / "Last fetch 3m".
pub fn last_fetch_label(now: u64, at: u64) -> String {
    let secs = now.saturating_sub(at);
    let ago = if secs < 60 { format!("{secs}s") } else { crate::util::relative_time(now, at) };
    format!("Last fetch {ago}")
}

/// Seconds until the next background fetch is due, for scheduling a repaint while the app is
/// idle: `None` when **Fetch every** is off, `0` when none ran yet or one is overdue.
pub fn background_fetch_wait(last_started: Option<u64>, interval_min: u32, now: u64) -> Option<u64> {
    if interval_min == 0 {
        return None;
    }
    let due = last_started.map_or(0, |t| t.saturating_add(u64::from(interval_min) * 60));
    Some(due.saturating_sub(now))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn facts<'a>(remotes: &'a [String], protected: &'a [String]) -> PushFacts<'a> {
        PushFacts {
            branch: Some("feat"),
            upstream: Some("origin/feat"),
            remote: None,
            bound_remote: Some("origin"),
            remotes,
            force: Force::No,
            protected,
            ask_new_branch: true,
        }
    }

    fn branch_push(remote: &str, branch: &str, set_upstream: bool, force: Force) -> Push {
        Push::Branch { remote: remote.into(), branch: branch.into(), set_upstream, force }
    }

    #[test]
    fn upstream_remote_prefers_the_longest_matching_remote() {
        let remotes = names(&["origin", "or", "team/fork"]);
        assert_eq!(upstream_remote("origin/feat", &remotes), Some("origin"));
        assert_eq!(upstream_remote("team/fork/feat/x", &remotes), Some("team/fork"));
        assert_eq!(upstream_remote("originx/feat", &remotes), None);
        assert_eq!(upstream_remote("origin", &remotes), None, "a bare remote name is no upstream");
    }

    #[test]
    fn default_remote_is_bound_then_upstream_then_first() {
        let remotes = names(&["fork", "origin"]);
        assert_eq!(default_remote(Some("origin"), Some("fork/x"), &remotes).as_deref(), Some("origin"));
        assert_eq!(default_remote(None, Some("origin/x"), &remotes).as_deref(), Some("origin"));
        assert_eq!(default_remote(Some(""), None, &remotes).as_deref(), Some("fork"));
        assert_eq!(default_remote(None, None, &[]), None);
    }

    #[test]
    fn push_with_upstream_runs_on_the_bound_remote() {
        let (remotes, protected) = (names(&["origin"]), names(&["main"]));
        assert_eq!(
            plan_push(&facts(&remotes, &protected)),
            PushPlan::Run(branch_push("origin", "feat", false, Force::No))
        );
    }

    #[test]
    fn first_push_asks_once_then_sets_upstream() {
        let (remotes, protected) = (names(&["origin"]), names(&[]));
        let mut f = facts(&remotes, &protected);
        f.upstream = None;
        let push = branch_push("origin", "feat", true, Force::No);
        assert_eq!(plan_push(&f), PushPlan::AskSetUpstream(push.clone()));
        f.ask_new_branch = false;
        assert_eq!(plan_push(&f), PushPlan::Run(push));
    }

    #[test]
    fn a_chosen_remote_never_asks_but_still_sets_upstream() {
        let (remotes, protected) = (names(&["origin", "fork"]), names(&[]));
        let mut f = facts(&remotes, &protected);
        f.upstream = None;
        f.remote = Some("fork");
        assert_eq!(plan_push(&f), PushPlan::Run(branch_push("fork", "feat", true, Force::No)));
    }

    #[test]
    fn plain_force_always_confirms_and_lease_does_not() {
        let (remotes, protected) = (names(&["origin"]), names(&["main"]));
        let mut f = facts(&remotes, &protected);
        f.force = Force::Plain;
        assert_eq!(plan_push(&f), PushPlan::Confirm(branch_push("origin", "feat", false, Force::Plain)));
        f.force = Force::WithLease;
        assert_eq!(plan_push(&f), PushPlan::Run(branch_push("origin", "feat", false, Force::WithLease)));
    }

    #[test]
    fn protected_branches_are_never_force_pushed_but_push_normally() {
        let (remotes, protected) = (names(&["origin"]), names(&["main", "release/*"]));
        let mut f = facts(&remotes, &protected);
        for branch in ["main", "release/1.2"] {
            f.branch = Some(branch);
            for force in [Force::WithLease, Force::Plain] {
                f.force = force;
                assert_eq!(
                    plan_push(&f),
                    PushPlan::Refuse(Refusal::Protected(branch.into())),
                    "{branch} {force:?}"
                );
            }
            f.force = Force::No;
            assert!(matches!(plan_push(&f), PushPlan::Run(_)));
        }
        assert_eq!(Refusal::Protected("main".into()).message(), "`main` is protected: force push is blocked");
    }

    #[test]
    fn detached_or_remoteless_pushes_are_refused() {
        let (remotes, protected) = (names(&[]), names(&[]));
        let mut f = facts(&remotes, &protected);
        f.bound_remote = None;
        f.upstream = None;
        assert_eq!(plan_push(&f), PushPlan::Refuse(Refusal::NoRemote));
        f.branch = None;
        assert_eq!(plan_push(&f), PushPlan::Refuse(Refusal::Detached));
    }

    #[test]
    fn new_branch_asks_once_per_branch_unless_the_setting_is_on() {
        let mut asks = NewBranchAsks::default();
        assert!(asks.should_ask(Some("feat"), false));
        assert!(!asks.should_ask(Some("feat"), true));
        asks.mark("feat");
        assert!(!asks.should_ask(Some("feat"), false));
        assert!(asks.should_ask(Some("other"), false));
        assert!(!asks.should_ask(None, false));
    }

    #[test]
    fn pull_plans() {
        use PullMode::*;
        assert_eq!(plan_pull(None, None, Merge, false), PullPlan::Detached);
        assert_eq!(plan_pull(Some("feat"), None, Merge, false), PullPlan::NoUpstream("feat".into()));
        assert_eq!(plan_pull(Some("feat"), Some("origin/feat"), Rebase, true), PullPlan::AskStash(Rebase));
        assert_eq!(
            plan_pull(Some("feat"), Some("origin/feat"), Merge, true),
            PullPlan::Run { mode: Merge, autostash: false },
            "a merge pull over local changes is git's call"
        );
        assert_eq!(
            plan_pull(Some("feat"), Some("origin/feat"), Rebase, false),
            PullPlan::Run { mode: Rebase, autostash: false }
        );
    }

    #[test]
    fn prompts_use_the_spec_wording_and_buttons() {
        let push = branch_push("origin", "feat", true, Force::No);
        let ask = Prompt::PushNewBranch(push.clone());
        assert_eq!(ask.title(), "Push `feat` to origin and set upstream?");
        let labels = |p: &Prompt| p.choices().into_iter().map(|(l, _)| l).collect::<Vec<_>>();
        assert_eq!(labels(&ask), ["Push", "Choose remote…", "Cancel"]);

        let rejected = Prompt::Rejected { push: push.clone(), lease_allowed: true };
        assert_eq!(rejected.title(), "Push rejected: remote has commits you don't have");
        assert_eq!(labels(&rejected), ["Pull then push", "Force push with lease", "Cancel"]);
        let no_force = Prompt::Rejected { push: push.clone(), lease_allowed: false };
        assert_eq!(labels(&no_force), ["Pull then push", "Cancel"]);

        let up = Prompt::SetUpstream { branch: "feat".into() };
        assert_eq!(up.title(), "No upstream for `feat`");
        assert_eq!(labels(&up)[0], "Set upstream…");
        assert_eq!(labels(&Prompt::StashPull(PullMode::Rebase)), ["Stash, pull, pop", "Cancel"]);
        for p in [ask, rejected, up] {
            assert_eq!(p.choices().last().map(|c| c.1), Some(Choice::Cancel));
        }
    }

    #[test]
    fn failures_map_to_their_action_toasts() {
        let push = branch_push("origin", "feat", false, Force::No);
        let none: Vec<String> = Vec::new();
        assert_eq!(
            push_failure_prompt(&NetErrorKind::Rejected, &push, &none),
            Some(Prompt::Rejected { push: push.clone(), lease_allowed: true })
        );
        assert_eq!(push_failure_prompt(&NetErrorKind::AuthFailed, &push, &none), None);
        // A lease that went stale again gets no second lease.
        let leased = with_lease(&push);
        assert_eq!(
            push_failure_prompt(&NetErrorKind::Rejected, &leased, &none),
            Some(Prompt::Rejected { push: leased.clone(), lease_allowed: false })
        );
        // §5.10: protected branches are never force-pushed, so the button is not offered.
        let main = branch_push("origin", "main", false, Force::No);
        let protected = names(&["main", "release/*"]);
        assert_eq!(
            push_failure_prompt(&NetErrorKind::Rejected, &main, &protected),
            Some(Prompt::Rejected { push: main.clone(), lease_allowed: false })
        );
        let tag = Push::Tag { remote: "origin".into(), tag: "v1".into() };
        assert_eq!(push_failure_prompt(&NetErrorKind::Rejected, &tag, &none), None);

        let kind = NetErrorKind::NoUpstream(Some("feat".into()));
        assert_eq!(
            pull_failure_prompt(&kind, PullMode::Merge),
            Some(Prompt::SetUpstream { branch: "feat".into() })
        );
        assert_eq!(
            pull_failure_prompt(&NetErrorKind::DirtyWorktree, PullMode::Rebase),
            Some(Prompt::StashPull(PullMode::Rebase))
        );
        assert_eq!(pull_failure_prompt(&NetErrorKind::Conflicts, PullMode::Merge), None);
    }

    #[test]
    fn with_lease_keeps_the_target() {
        let push = branch_push("origin", "feat", true, Force::No);
        assert_eq!(with_lease(&push), branch_push("origin", "feat", true, Force::WithLease));
    }

    #[test]
    fn push_toast_says_undo_is_not_available() {
        let push = branch_push("origin", "feat", false, Force::No);
        assert_eq!(pushed_message(&push), "Pushed `feat` to origin · Undo is not available for push");
        let del = Push::Delete { remote: "origin".into(), refname: "refs/tags/v1".into() };
        assert!(pushed_message(&del).starts_with("Deleted `v1` on origin"));
    }

    #[test]
    fn labels() {
        assert_eq!(push_label(Some("origin")), "Push → origin");
        assert_eq!(push_label(None), "Push");
        assert_eq!(pull_mode_label(PullMode::FastForwardOnly), "Pull (fast-forward only)");
        assert_eq!(last_fetch_label(100, 100), "Last fetch 0s");
        assert_eq!(last_fetch_label(142, 100), "Last fetch 42s");
        assert_eq!(last_fetch_label(100 + 180, 100), "Last fetch 3m");
    }

    #[test]
    fn background_fetch_wait_counts_down_from_the_last_start() {
        assert_eq!(background_fetch_wait(None, 5, 1000), Some(0));
        assert_eq!(background_fetch_wait(Some(1000), 5, 1060), Some(240));
        assert_eq!(background_fetch_wait(Some(1000), 5, 2000), Some(0));
        assert_eq!(background_fetch_wait(Some(1000), 0, 1000), None);
    }
}
