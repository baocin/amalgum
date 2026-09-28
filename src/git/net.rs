//! Network git: clone (§5.3), fetch, pull, push (§5.10), tag push and remote deletes (§5.11),
//! plain force push (§5.20). Pure argv builders, a progress parser for the status-bar pill, the
//! protected-branch rule, clone-sheet helpers, and a classifier that turns stderr into the
//! spec's toast wording, and the background-fetch rules ([`BackgroundFetch`]); then the headless
//! runners the UI calls from a worker thread, each taking a progress callback and a cancel flag
//! ([`Git::run_streaming`]).
//!
//! Journal (§5.18): pull records an entry whose inverse resets to the pre-pull HEAD; push
//! records one with no inverse ("Cannot be undone from the client"); fetch never moves a local
//! branch and clone has no repo yet, so neither records anything.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::git::cmd::CANCELLED;
use crate::git::journal::{Entry, Plan, Snapshot};
use crate::git::remote;
use crate::git::{Git, GitError, Location};
use crate::ssh::{self, HostKeyPrompt};
use crate::util::relative_time;

// ---- argv builders ----------------------------------------------------------------------------

/// What **Fetch** fetches (§5.10): the bound (or a chosen) remote, or every remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    Remote(String),
    All,
}

/// `fetch <remote> --prune --progress`, or `fetch --all --prune --progress` for **Fetch all**.
pub fn fetch_args(target: &Fetch) -> Vec<String> {
    let target = match target {
        Fetch::Remote(remote) => remote.as_str(),
        Fetch::All => "--all",
    };
    ["fetch", target, "--prune", "--progress"].map(String::from).to_vec()
}

/// The **Pull** split button's choices (§5.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullMode {
    Merge,
    Rebase,
    FastForwardOnly,
}

impl PullMode {
    /// The split button's default, from `git config pull.rebase` (unset means merge): git ≥ 2.33
    /// refuses a divergent pull without an explicit mode, so the app always passes one.
    pub fn from_config(pull_rebase: Option<&str>) -> Self {
        match pull_rebase.map(str::trim) {
            Some("true" | "merges" | "interactive" | "i" | "m") => Self::Rebase,
            _ => Self::Merge,
        }
    }
}

/// `pull (--no-rebase|--rebase|--ff-only) [--autostash] --progress`. `autostash` is the dirty-tree
/// toast's **Stash, pull, pop**.
pub fn pull_args(mode: PullMode, autostash: bool) -> Vec<String> {
    let flag = match mode {
        PullMode::Merge => "--no-rebase",
        PullMode::Rebase => "--rebase",
        PullMode::FastForwardOnly => "--ff-only",
    };
    let mut v = vec!["pull".to_string(), flag.to_string()];
    if autostash {
        v.push("--autostash".to_string());
    }
    v.push("--progress".to_string());
    v
}

/// How hard a branch push overwrites the remote (§5.10 **Force push**, §5.20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Force {
    No,
    /// `--force-with-lease`: the dropdown's default.
    WithLease,
    /// Plain `--force`: only with `Alt` held, always behind the §5.20 confirmation.
    Plain,
}

/// One push (§5.10, §5.11). A branch is named short or as `refs/heads/…`; `Delete` takes a full
/// ref name (`refs/heads/x`, `refs/tags/v1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Push {
    /// `set_upstream` adds `-u` (a branch's first push, §5.10).
    Branch {
        remote: String,
        branch: String,
        set_upstream: bool,
        force: Force,
    },
    Tag {
        remote: String,
        tag: String,
    },
    AllTags {
        remote: String,
    },
    Delete {
        remote: String,
        refname: String,
    },
}

impl Push {
    /// `push --progress [-u] [--force-with-lease|--force] <remote> refs/heads/<branch>`,
    /// `push --progress <remote> refs/tags/<tag>`, `push --progress <remote> --tags`,
    /// `push --progress <remote> --delete <ref>`. Full ref names keep a branch and a tag of the
    /// same name apart.
    pub fn args(&self) -> Vec<String> {
        let mut v = vec!["push".to_string(), "--progress".to_string()];
        match self {
            Self::Branch { remote, branch, set_upstream, force } => {
                if *set_upstream {
                    v.push("-u".to_string());
                }
                match force {
                    Force::No => {}
                    Force::WithLease => v.push("--force-with-lease".to_string()),
                    Force::Plain => v.push("--force".to_string()),
                }
                v.extend([remote.clone(), format!("refs/heads/{}", short_branch(branch))]);
            }
            Self::Tag { remote, tag } => v.extend([remote.clone(), format!("refs/tags/{tag}")]),
            Self::AllTags { remote } => v.extend([remote.clone(), "--tags".to_string()]),
            Self::Delete { remote, refname } => {
                v.extend([remote.clone(), "--delete".to_string(), refname.clone()])
            }
        }
        v
    }

    /// The journal and progress-pill label: "push origin feat", "push origin tag v1".
    pub fn description(&self) -> String {
        match self {
            Self::Branch { remote, branch, force, .. } => {
                let force = match force {
                    Force::No => "",
                    Force::WithLease => " (force with lease)",
                    Force::Plain => " (force)",
                };
                format!("push {remote} {}{force}", short_branch(branch))
            }
            Self::Tag { remote, tag } => format!("push {remote} tag {tag}"),
            Self::AllTags { remote } => format!("push {remote} --tags"),
            Self::Delete { remote, refname } => format!("delete {} on {remote}", short_ref(refname)),
        }
    }
}

fn short_branch(branch: &str) -> &str {
    branch.strip_prefix("refs/heads/").unwrap_or(branch)
}

fn short_ref(refname: &str) -> &str {
    refname.strip_prefix("refs/heads/").or_else(|| refname.strip_prefix("refs/tags/")).unwrap_or(refname)
}

/// `clone --progress [--depth N] -- <url> <dest>` (§5.3); `--` so a URL can't read as an option.
pub fn clone_args(url: &str, dest: &str, depth: Option<u32>) -> Vec<String> {
    let mut v = vec!["clone".to_string(), "--progress".to_string()];
    if let Some(n) = depth {
        v.extend(["--depth".to_string(), n.to_string()]);
    }
    v.extend(["--".to_string(), url.to_string(), dest.to_string()]);
    v
}

// ---- progress -----------------------------------------------------------------------------------

/// One progress redraw: `Receiving objects:  45% (45/100), 1.2 MiB | …` →
/// `{ phase: "Receiving objects", percent: Some(45), done: 45, total: Some(100) }`;
/// `remote: Enumerating objects: 5, done.` → `{ "Enumerating objects", None, 5, None }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub phase: String,
    pub percent: Option<u8>,
    pub done: u64,
    pub total: Option<u64>,
}

/// Parses one stderr segment ([`Git::run_streaming`] already split on `\r` and `\n`). Anything
/// else git prints (`From …`, ref updates, hints, errors) is `None`.
pub fn parse_progress(segment: &str) -> Option<Progress> {
    let line = segment.trim();
    let line = line.strip_prefix("remote:").map_or(line, str::trim_start);
    let (phase, rest) = line.split_once(": ")?;
    let phase_ok = phase.starts_with(|c: char| c.is_ascii_uppercase())
        && phase.chars().all(|c| c.is_ascii_alphabetic() || c == ' ');
    if !phase_ok {
        return None;
    }
    let rest = rest.trim_start();
    let digits = |s: &str| s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n = digits(rest);
    if n == 0 {
        return None;
    }
    let number: u64 = rest[..n].parse().ok()?;
    let after = &rest[n..];
    let Some(after) = after.strip_prefix('%') else {
        // A counter without a total: `Enumerating objects: 5, done.` or `Counting: 12`.
        return (after.is_empty() || after.starts_with(',')).then(|| Progress {
            phase: phase.to_string(),
            percent: None,
            done: number,
            total: None,
        });
    };
    let percent = u8::try_from(number.min(100)).ok()?;
    let counts =
        after.trim_start().strip_prefix('(').and_then(|s| s.split_once(')')).and_then(|(inner, _)| {
            let (done, total) = inner.split_once('/')?;
            Some((done.parse().ok()?, total.parse().ok()?))
        });
    let (done, total) = counts.map_or((0, None), |(d, t)| (d, Some(t)));
    Some(Progress { phase: phase.to_string(), percent: Some(percent), done, total })
}

// ---- protected branches ---------------------------------------------------------------------

/// Does `branch` (short or `refs/heads/…`) match a **Protected branches** pattern (§5.10,
/// §5.23)? A pattern is an exact name or `prefix/*`, which matches any branch strictly under
/// `prefix/` (`release/*` matches `release/1.2`, not `release`).
pub fn is_protected(patterns: &[impl AsRef<str>], branch: &str) -> bool {
    let branch = short_branch(branch);
    patterns.iter().any(|pattern| match pattern.as_ref().strip_suffix("/*") {
        Some(prefix) => {
            branch.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')).is_some_and(|r| !r.is_empty())
        }
        None => branch == pattern.as_ref(),
    })
}

/// Protected branches are never force-pushed, with or without a lease (§5.10), nor deleted on
/// the remote: the spec names only force push, but a delete overwrites strictly more. Ordinary
/// pushes, tags, and other deletes pass.
pub fn check_push_allowed(push: &Push, protected: &[impl AsRef<str>]) -> Result<(), NetErrorKind> {
    match push {
        Push::Branch { branch, force: Force::WithLease | Force::Plain, .. }
            if is_protected(protected, branch) =>
        {
            Err(NetErrorKind::ProtectedBranch(short_branch(branch).to_string()))
        }
        // A name outside `refs/` is resolved by git, a branch first.
        Push::Delete { refname, .. }
            if (refname.starts_with("refs/heads/") || !refname.starts_with("refs/"))
                && is_protected(protected, refname) =>
        {
            Err(NetErrorKind::ProtectedBranchDelete(short_branch(refname).to_string()))
        }
        _ => Ok(()),
    }
}

// ---- clone sheet --------------------------------------------------------------------------------

/// The clone sheet's auto-derived folder name (§5.3), as git derives it:
/// `git@github.com:acme/conduit.git`, `https://host/acme/conduit/`, `/srv/conduit/.git` →
/// `conduit`. `None` when nothing usable is left.
pub fn clone_folder_name(url: &str) -> Option<String> {
    let s = url.trim().trim_end_matches('/');
    let s = s.strip_suffix("/.git").unwrap_or(s);
    let last = s.rsplit(['/', ':']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_string())
}

/// Whether clipboard text should auto-fill the clone sheet's URL (§5.3): one token that is an
/// `ssh://`, `git://`, or `file://` URL, an scp-like `user@host:path` or `host:path.git`, or an
/// http(s) URL ending in `.git` or naming a forge's `host/owner/repo`. Ordinary web links
/// (`https://example.com/docs`) and prose are not.
pub fn looks_like_git_url(text: &str) -> bool {
    let s = text.trim();
    if s.is_empty() || s.chars().any(char::is_whitespace) {
        return false;
    }
    if let Some(path) = s.strip_prefix("file://") {
        return path.len() > 1;
    }
    let Some(normalized) = remote::normalize(s) else { return false };
    let ends_in_git = s.trim_end_matches('/').ends_with(".git");
    if s.starts_with("ssh://") || s.starts_with("git://") {
        true
    } else if s.starts_with("http://") || s.starts_with("https://") {
        ends_in_git || (remote::forge(&normalized).is_some() && normalized.split('/').count() == 3)
    } else {
        s.contains('@') || ends_in_git
    }
}

// ---- errors ---------------------------------------------------------------------------------

/// What went wrong, for the toast's action buttons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetErrorKind {
    Cancelled,
    AuthFailed,
    /// `Some` for an unknown key (the W16 dialog, never auto-accepted); `None` when ssh only
    /// said verification failed (a changed key).
    HostKey(Option<Box<HostKeyPrompt>>),
    DestinationExists,
    /// Pull without an upstream; the branch when known (toast offers **Set upstream…**).
    NoUpstream(Option<String>),
    /// Non-fast-forward (or stale lease) push rejection.
    Rejected,
    /// `--ff-only` on a diverged branch.
    CannotFastForward,
    /// Local changes block the pull (toast offers **Stash, pull, pop**).
    DirtyWorktree,
    /// The pull stopped on conflicts (§5.17 takes over).
    Conflicts,
    Offline,
    /// Force push refused before running anything.
    ProtectedBranch(String),
    /// Deleting a protected branch on the remote, refused before running anything.
    ProtectedBranchDelete(String),
    /// Pull refused up front: HEAD is detached, so there is no branch to pull into.
    DetachedHead,
    Other,
}

/// A failed network operation: the kind, plus the git failure (`None` when refused before git
/// ran), whose command and stderr the toast's **Details** shows (§5.24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetError {
    pub kind: NetErrorKind,
    pub git: Option<Box<GitError>>,
}

impl NetError {
    fn from_git(err: GitError) -> Self {
        Self { kind: classify(&err.stderr), git: Some(Box::new(err)) }
    }

    /// The toast line, in the spec's words where it has them.
    pub fn message(&self) -> String {
        match &self.kind {
            NetErrorKind::Cancelled => "Cancelled".to_string(),
            NetErrorKind::AuthFailed => {
                "Authentication failed. Check your SSH agent or credential helper.".to_string()
            }
            NetErrorKind::HostKey(_) => "Unknown or changed host key".to_string(),
            NetErrorKind::DestinationExists => "Folder already exists".to_string(),
            NetErrorKind::NoUpstream(Some(branch)) => format!("No upstream for `{branch}`"),
            NetErrorKind::NoUpstream(None) => "No upstream for the current branch".to_string(),
            NetErrorKind::Rejected => "Push rejected: remote has commits you don't have".to_string(),
            NetErrorKind::CannotFastForward => "Can't fast-forward: the branches have diverged".to_string(),
            NetErrorKind::DirtyWorktree => "Local changes block the pull".to_string(),
            NetErrorKind::Conflicts => "Pull stopped on conflicts".to_string(),
            NetErrorKind::Offline => "Offline".to_string(),
            NetErrorKind::ProtectedBranch(branch) => {
                format!("`{branch}` is protected: force push is blocked")
            }
            NetErrorKind::ProtectedBranchDelete(branch) => {
                format!("`{branch}` is protected: deleting it on the remote is blocked")
            }
            NetErrorKind::DetachedHead => "Can't pull: HEAD is detached, not on a branch".to_string(),
            NetErrorKind::Other => {
                self.git.as_ref().map_or_else(|| "git failed".to_string(), |e| e.summary())
            }
        }
    }
}

impl From<NetErrorKind> for NetError {
    fn from(kind: NetErrorKind) -> Self {
        Self { kind, git: None }
    }
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for NetError {}

/// Classifies a network command's stderr (`LC_ALL=C`, so git's English wording).
pub fn classify(stderr: &str) -> NetErrorKind {
    let has = |needles: &[&str]| needles.iter().any(|n| stderr.contains(n));
    if stderr.lines().map(str::trim).rfind(|l| !l.is_empty()) == Some(CANCELLED) {
        return NetErrorKind::Cancelled;
    }
    if let Some(prompt) = ssh::parse_host_key_prompt(stderr) {
        return NetErrorKind::HostKey(Some(Box::new(prompt)));
    }
    if has(&["Host key verification failed", "REMOTE HOST IDENTIFICATION HAS CHANGED"]) {
        return NetErrorKind::HostKey(None);
    }
    if has(&[
        "Permission denied (publickey",
        "Authentication failed",
        "could not read Username",
        "could not read Password",
    ]) {
        return NetErrorKind::AuthFailed;
    }
    if has(&["already exists and is not an empty directory"]) {
        return NetErrorKind::DestinationExists;
    }
    if has(&["There is no tracking information for the current branch"]) {
        return NetErrorKind::NoUpstream(None);
    }
    if let Some(rest) = stderr.split("The current branch ").nth(1)
        && let Some((branch, _)) = rest.split_once(" has no upstream branch")
    {
        return NetErrorKind::NoUpstream(Some(branch.to_string()));
    }
    if has(&["Not possible to fast-forward"]) {
        return NetErrorKind::CannotFastForward;
    }
    if has(&["(non-fast-forward)", "(fetch first)", "(stale info)", "Updates were rejected"]) {
        return NetErrorKind::Rejected;
    }
    if has(&["cannot pull with rebase", "cannot rebase: You have", "would be overwritten by merge"]) {
        return NetErrorKind::DirtyWorktree;
    }
    if has(&["CONFLICT (", "Automatic merge failed", "could not apply", "Resolve all conflicts"]) {
        return NetErrorKind::Conflicts;
    }
    if has(&[
        "Could not resolve host",
        "Could not resolve hostname",
        "Connection refused",
        "Connection timed out",
        "Operation timed out",
        "Network is unreachable",
        "No route to host",
    ]) {
        return NetErrorKind::Offline;
    }
    NetErrorKind::Other
}

// ---- background fetch ---------------------------------------------------------------------

/// Below this charge, on battery, background fetch waits (§5.10).
pub const LOW_BATTERY_PERCENT: u8 = 20;

/// The machine's battery, when it has one and the caller could read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// Running on battery, not on mains power.
    pub discharging: bool,
    pub percent: u8,
}

/// What [`BackgroundFetch::due`] looks at, gathered by the caller on each tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchConditions<'a> {
    /// Settings → **Fetch every**, in minutes; `0` is off.
    pub interval_min: u32,
    /// The workspace's bound remote (§5.22); background fetch never fetches another.
    pub bound_remote: Option<&'a str>,
    pub now: u64,
    pub online: bool,
    /// `false` while a remote workspace is disconnected or reconnecting (§5.28).
    pub connected: bool,
    /// A fetch (manual or background) is already running for this workspace.
    pub in_progress: bool,
    pub battery: Option<Battery>,
    /// Settings → **battery skip**.
    pub skip_on_low_battery: bool,
}

/// Why a background fetch did not start this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Off,
    NoRemote,
    NotDue,
    Offline,
    InProgress,
    Disconnected,
    LowBattery,
}

/// How a finished fetch's failure reaches the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// Nothing to show: a success, a cancel, or a background fetch that found the network
    /// down (it pauses silently until a fetch succeeds, §5.25).
    None,
    Toast,
    /// Only the status bar's "Fetch failed · 3m" ([`BackgroundFetch::status_label`]).
    StatusBar,
}

/// One workspace's background-fetch state (§5.10 **Background fetch**).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackgroundFetch {
    /// When the last fetch of the bound remote started, manual or background.
    pub last_started: Option<u64>,
    /// Background failures since the last successful fetch.
    pub failures: u32,
    /// The latest background failure and when it happened: the status bar's click-to-see-stderr.
    pub last_failure: Option<(u64, NetError)>,
}

impl BackgroundFetch {
    /// Whether to start a background fetch now, and of what: only the bound remote, every
    /// `interval_min` minutes counted from the last fetch start; never while offline,
    /// disconnected, already fetching, or on battery below 20 % (when that setting is on).
    pub fn due(&self, c: &FetchConditions) -> Result<Fetch, Skip> {
        if c.interval_min == 0 {
            return Err(Skip::Off);
        }
        let remote = c.bound_remote.ok_or(Skip::NoRemote)?;
        let low_battery = c.battery.is_some_and(|b| b.discharging && b.percent < LOW_BATTERY_PERCENT);
        let not_due =
            self.last_started.is_some_and(|t| c.now < t.saturating_add(u64::from(c.interval_min) * 60));
        let skip = [
            (!c.connected, Skip::Disconnected),
            (!c.online, Skip::Offline),
            (c.in_progress, Skip::InProgress),
            (c.skip_on_low_battery && low_battery, Skip::LowBattery),
            (not_due, Skip::NotDue),
        ];
        match skip.into_iter().find(|(hit, _)| *hit) {
            Some((_, why)) => Err(why),
            None => Ok(Fetch::Remote(remote.to_string())),
        }
    }

    /// Call when a fetch of the bound remote starts (manual ones too: they reset the interval).
    pub fn started(&mut self, now: u64) {
        self.last_started = Some(now);
    }

    /// Call when a fetch of the bound remote finishes; returns how to report it. Any success
    /// clears the failure state. A manual failure is always a toast. A background failure is a
    /// toast the first time and only the status bar after that, to avoid nagging; being offline
    /// or cancelled shows nothing.
    pub fn finished(&mut self, now: u64, background: bool, result: &Result<(), NetError>) -> Notice {
        let Err(err) = result else {
            self.failures = 0;
            self.last_failure = None;
            return Notice::None;
        };
        if !background {
            return Notice::Toast;
        }
        if matches!(err.kind, NetErrorKind::Offline | NetErrorKind::Cancelled) {
            return Notice::None;
        }
        self.failures += 1;
        self.last_failure = Some((now, err.clone()));
        if self.failures == 1 { Notice::Toast } else { Notice::StatusBar }
    }

    /// The status bar's "Fetch failed · 3m" while background fetches keep failing.
    pub fn status_label(&self, now: u64) -> Option<String> {
        let (at, _) = self.last_failure.as_ref()?;
        Some(format!("Fetch failed · {}", relative_time(now, *at)))
    }
}

// ---- execution (worker thread) --------------------------------------------------------------

/// Runs `args` via [`Git::run_streaming`], handing each progress redraw to `on_progress`.
fn run(
    git: &Git,
    args: &[String],
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<Vec<u8>, NetError> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git.run_streaming(&args, cancel, &mut |segment| {
        if let Some(p) = parse_progress(segment) {
            on_progress(&p);
        }
    })
    .map_err(NetError::from_git)
}

/// HEAD's commit and branch (`None` when unborn / detached), plus the hash of each ref in `refs`
/// that resolves.
fn read_snapshot(git: &Git, refs: &[String]) -> Snapshot {
    let line = |args: &[&str]| {
        let out = git.run(args).ok()?;
        Some(String::from_utf8_lossy(&out).trim().to_string()).filter(|s| !s.is_empty())
    };
    let refs = refs
        .iter()
        .filter_map(|name| Some((name.clone(), line(&["rev-parse", "--verify", "-q", name])?)))
        .collect();
    Snapshot {
        head: line(&["rev-parse", "--verify", "-q", "HEAD"]),
        branch: line(&["symbolic-ref", "-q", "--short", "HEAD"]),
        refs,
        stashes: Vec::new(),
    }
}

/// **Fetch** / **Fetch all** (§5.10). Never touches local branches; records no journal entry.
pub fn fetch(
    git: &Git,
    target: &Fetch,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<(), NetError> {
    run(git, &fetch_args(target), cancel, on_progress).map(drop)
}

/// What a successful [`pull`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pulled {
    /// The journal entry to record; `None` when HEAD did not move ("Already up to date").
    pub entry: Option<Entry>,
    /// The pull landed but popping the autostash left conflicts (git exits 0 and says "Applying
    /// autostash resulted in conflicts"): §5.17 takes over, and undo refuses until they are
    /// resolved, since `reset --keep` can't run on an unmerged index.
    pub autostash_conflicts: bool,
}

/// **Pull** (§5.10) into the current branch; a detached HEAD is refused before git runs. Undo
/// resets (`--keep`, so uncommitted changes survive or the reset refuses) the branch to its
/// pre-pull commit; redo resets it to the pulled one. On an unborn branch, undo deletes the
/// branch ref again and redo recreates it. The snapshots name the branch's ref, which both plans
/// move. Confirming when the pull's merge commits were since pushed is the caller's. A pull that
/// stops on conflicts — git prints a merge's `CONFLICT` lines on stdout, so they are found in the
/// index — fails with [`NetErrorKind::Conflicts`].
pub fn pull(
    git: &Git,
    mode: PullMode,
    autostash: bool,
    now: u64,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<Pulled, NetError> {
    let Some(branch) = read_snapshot(git, &[]).branch else {
        return Err(NetErrorKind::DetachedHead.into());
    };
    let refname = format!("refs/heads/{branch}");
    let refs = [refname.clone()];
    let before = read_snapshot(git, &refs);
    if let Err(mut err) = run(git, &pull_args(mode, autostash), cancel, on_progress) {
        match err.kind {
            NetErrorKind::NoUpstream(None) => err.kind = NetErrorKind::NoUpstream(Some(branch)),
            NetErrorKind::Other if has_unmerged(git) => err.kind = NetErrorKind::Conflicts,
            _ => {}
        }
        return Err(err);
    }
    let after = read_snapshot(git, &refs);
    let autostash_conflicts = autostash && has_unmerged(git);
    let Some(to) = after.head.clone().filter(|to| before.head.as_ref() != Some(to)) else {
        return Ok(Pulled { entry: None, autostash_conflicts });
    };
    let (forward, inverse) = match &before.head {
        Some(from) => (reset_keep(&to), reset_keep(from)),
        None => (
            vec![["update-ref", &refname, &to, ""].map(String::from).to_vec()],
            vec![["update-ref", "-d", &refname, &to].map(String::from).to_vec()],
        ),
    };
    let entry = Entry {
        id: 0,
        time: now,
        description: format!("pull {branch}"),
        forward,
        inverse: Some(inverse),
        before,
        after,
        undone: false,
    };
    Ok(Pulled { entry: Some(entry), autostash_conflicts })
}

fn reset_keep(to: &str) -> Plan {
    vec![["reset", "--keep", to].map(String::from).to_vec()]
}

/// Whether the index has unmerged paths (a pull or autostash pop stopped on conflicts).
fn has_unmerged(git: &Git) -> bool {
    git.run(&["ls-files", "--unmerged", "-z"]).is_ok_and(|out| !out.is_empty())
}

/// **Push** (§5.10, §5.11). Refuses a force push of a protected branch before running git.
/// Returns the journal entry to record: no inverse, since a push cannot be undone from the
/// client (§5.18); a branch push's snapshots name its remote-tracking ref.
pub fn push(
    git: &Git,
    push: &Push,
    protected: &[impl AsRef<str>],
    now: u64,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<Entry, NetError> {
    check_push_allowed(push, protected)?;
    let refs = match push {
        Push::Branch { remote, branch, .. } => {
            vec![format!("refs/remotes/{remote}/{}", short_branch(branch))]
        }
        _ => Vec::new(),
    };
    let before = read_snapshot(git, &refs);
    let args = push.args();
    run(git, &args, cancel, on_progress)?;
    Ok(Entry {
        id: 0,
        time: now,
        description: push.description(),
        before,
        after: read_snapshot(git, &refs),
        forward: vec![args],
        inverse: None,
        undone: false,
    })
}

/// **Clone** (§5.3) into `<git.location>/<name>`: `git` is bound to the destination's parent
/// folder, local or on a remote host ("Clone on remote host"), and a missing local parent is
/// created. Returns the new repository's location. An existing non-empty local destination is
/// refused up front. On cancel or failure a local destination that did not exist before is
/// deleted; a pre-existing one was empty, so it is emptied again (git never got to run its own
/// cleanup after a kill) and left in place. Remotely, git removes its own partial folder when it
/// fails, or on a cancel once the killed ssh session breaks its pipes (at its next write).
pub fn clone(
    git: &Git,
    url: &str,
    name: &str,
    depth: Option<u32>,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<Location, NetError> {
    let dest = match &git.location {
        Location::Local { path } => {
            let dest = path.join(name);
            let existed = dest.exists();
            if existed && !is_empty_dir(&dest) {
                return Err(NetErrorKind::DestinationExists.into());
            }
            if let Err(e) = std::fs::create_dir_all(path) {
                let command = format!("create folder {}", path.display());
                return Err(NetError::from_git(GitError { command, code: None, stderr: e.to_string() }));
            }
            let result = run(git, &clone_args(url, name, depth), cancel, on_progress);
            match result {
                Err(_) if existed => empty_dir(&dest),
                Err(_) => {
                    let _ = std::fs::remove_dir_all(&dest);
                }
                Ok(_) => {}
            }
            result?;
            return Ok(Location::Local { path: dest });
        }
        Location::Remote { host, path } => {
            Location::Remote { host: host.clone(), path: format!("{}/{name}", path.trim_end_matches('/')) }
        }
    };
    run(git, &clone_args(url, name, depth), cancel, on_progress)?;
    Ok(dest)
}

fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
}

/// Removes everything inside `dir` (best effort), keeping `dir` itself.
fn empty_dir(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let _ = if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::journal::{Journal, Plan};
    use crate::testutil::{TempRepo, hermetic_git};
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    // ---- argv builders ------------------------------------------------------------------------

    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    fn branch_push(branch: &str, set_upstream: bool, force: Force) -> Push {
        Push::Branch { remote: "origin".into(), branch: branch.into(), set_upstream, force }
    }

    #[test]
    fn fetch_args_match_the_spec() {
        assert_eq!(
            strs(&fetch_args(&Fetch::Remote("origin".into()))),
            ["fetch", "origin", "--prune", "--progress"]
        );
        assert_eq!(strs(&fetch_args(&Fetch::All)), ["fetch", "--all", "--prune", "--progress"]);
    }

    #[test]
    fn pull_args_per_mode() {
        assert_eq!(strs(&pull_args(PullMode::Merge, false)), ["pull", "--no-rebase", "--progress"]);
        assert_eq!(
            strs(&pull_args(PullMode::Rebase, true)),
            ["pull", "--rebase", "--autostash", "--progress"]
        );
        assert_eq!(strs(&pull_args(PullMode::FastForwardOnly, false)), ["pull", "--ff-only", "--progress"]);
    }

    #[test]
    fn pull_mode_default_follows_pull_rebase() {
        assert_eq!(PullMode::from_config(None), PullMode::Merge);
        assert_eq!(PullMode::from_config(Some("false")), PullMode::Merge);
        assert_eq!(PullMode::from_config(Some("true\n")), PullMode::Rebase);
        assert_eq!(PullMode::from_config(Some("merges")), PullMode::Rebase);
    }

    #[test]
    fn push_args_per_kind() {
        let args = |p: Push| p.args();
        assert_eq!(
            strs(&args(branch_push("feat", false, Force::No))),
            ["push", "--progress", "origin", "refs/heads/feat"]
        );
        assert_eq!(
            strs(&args(branch_push("feat", true, Force::WithLease))),
            ["push", "--progress", "-u", "--force-with-lease", "origin", "refs/heads/feat"]
        );
        assert_eq!(
            strs(&args(branch_push("feat", false, Force::Plain))),
            ["push", "--progress", "--force", "origin", "refs/heads/feat"]
        );
        let tag = Push::Tag { remote: "origin".into(), tag: "v1".into() };
        assert_eq!(strs(&tag.args()), ["push", "--progress", "origin", "refs/tags/v1"]);
        let all = Push::AllTags { remote: "origin".into() };
        assert_eq!(strs(&all.args()), ["push", "--progress", "origin", "--tags"]);
        let del = Push::Delete { remote: "origin".into(), refname: "refs/heads/old".into() };
        assert_eq!(strs(&del.args()), ["push", "--progress", "origin", "--delete", "refs/heads/old"]);
        assert_eq!(del.description(), "delete old on origin");
        assert_eq!(
            branch_push("feat", false, Force::WithLease).description(),
            "push origin feat (force with lease)"
        );
    }

    #[test]
    fn clone_args_with_and_without_depth() {
        assert_eq!(strs(&clone_args("u", "d", None)), ["clone", "--progress", "--", "u", "d"]);
        assert_eq!(
            strs(&clone_args("u", "d", Some(1))),
            ["clone", "--progress", "--depth", "1", "--", "u", "d"]
        );
    }

    // ---- progress -------------------------------------------------------------------------------

    #[test]
    fn parse_progress_phases() {
        let p = |phase: &str, percent, done, total| Progress { phase: phase.into(), percent, done, total };
        let cases = [
            (
                "Receiving objects:  45% (45/100), 1.20 MiB | 2.00 MiB/s",
                p("Receiving objects", Some(45), 45, Some(100)),
            ),
            ("Resolving deltas: 100% (3/3), done.", p("Resolving deltas", Some(100), 3, Some(3))),
            ("remote: Counting objects:  50% (1/2)", p("Counting objects", Some(50), 1, Some(2))),
            (
                "remote: Compressing objects: 100% (2/2), done.   ",
                p("Compressing objects", Some(100), 2, Some(2)),
            ),
            ("remote: Enumerating objects: 5, done.", p("Enumerating objects", None, 5, None)),
            (
                "Writing objects: 100% (3/3), 250 bytes | 250.00 KiB/s, done.",
                p("Writing objects", Some(100), 3, Some(3)),
            ),
            ("Updating files:   7% (7/100)", p("Updating files", Some(7), 7, Some(100))),
        ];
        for (input, want) in cases {
            assert_eq!(parse_progress(input), Some(want), "{input:?}");
        }
    }

    #[test]
    fn parse_progress_ignores_everything_else() {
        for input in [
            "Cloning into 'w'...",
            "From ../r",
            " * [new branch]      feat       -> origin/feat",
            "remote: Total 3 (delta 0), reused 0 (delta 0), pack-reused 0",
            "fatal: repository 'x' does not exist",
            "hint: Updates were rejected because the tip of your current branch is behind",
            "error: failed to push some refs to 'r'",
            "",
        ] {
            assert_eq!(parse_progress(input), None, "{input:?}");
        }
    }

    // ---- protected branches -------------------------------------------------------------------

    const DEFAULT_PROTECTED: [&str; 4] = ["main", "master", "develop", "release/*"];
    const NONE_PROTECTED: [&str; 0] = [];

    #[test]
    fn protected_patterns_exact_and_glob() {
        for b in ["main", "master", "develop", "release/1.2", "release/a/b", "refs/heads/main"] {
            assert!(is_protected(&DEFAULT_PROTECTED, b), "{b}");
        }
        for b in ["feat", "release", "mainline", "release/", "develop2"] {
            assert!(!is_protected(&DEFAULT_PROTECTED, b), "{b}");
        }
    }

    #[test]
    fn protected_branches_refuse_every_force_push_only() {
        for force in [Force::WithLease, Force::Plain] {
            assert_eq!(
                check_push_allowed(&branch_push("release/2", false, force), &DEFAULT_PROTECTED),
                Err(NetErrorKind::ProtectedBranch("release/2".into()))
            );
            assert_eq!(check_push_allowed(&branch_push("feat", false, force), &DEFAULT_PROTECTED), Ok(()));
        }
        assert_eq!(check_push_allowed(&branch_push("main", false, Force::No), &DEFAULT_PROTECTED), Ok(()));
        let err = NetError::from(NetErrorKind::ProtectedBranch("main".into()));
        assert_eq!(err.message(), "`main` is protected: force push is blocked");
    }

    // ---- clone sheet --------------------------------------------------------------------------

    #[test]
    fn clone_folder_name_derivation() {
        let cases = [
            ("git@github.com:acme/conduit.git", Some("conduit")),
            ("https://github.com/acme/conduit", Some("conduit")),
            ("https://github.com/acme/conduit.git/", Some("conduit")),
            ("ssh://git@host:22/srv/tools.git", Some("tools")),
            ("host:repo.git", Some("repo")),
            ("../src/.git", Some("src")),
            ("file:///srv/bare/lib.git", Some("lib")), // portability: allow
            ("  https://gitlab.com/g/sub/proj  ", Some("proj")),
            ("", None),
            ("https://", None),
            ("..", None),
        ];
        for (url, want) in cases {
            assert_eq!(clone_folder_name(url).as_deref(), want, "{url:?}");
        }
    }

    #[test]
    fn git_url_detection_for_clipboard_autofill() {
        for yes in [
            "git@github.com:acme/conduit.git",
            "git@github.com:acme/conduit",
            "https://github.com/acme/conduit",
            "https://github.com/acme/conduit.git",
            "https://git.example.com/team/app.git",
            "ssh://git@host:2222/srv/app",
            "git://host/app",
            "file:///srv/app.git", // portability: allow
            "host:path/app.git",
            "  git@github.com:acme/conduit.git\n",
        ] {
            assert!(looks_like_git_url(yes), "{yes:?}");
        }
        for no in [
            "",
            "hello world",
            "https://example.com/docs/page",
            "https://github.com/acme/conduit/pulls",
            "note:remember",
            "C:\\repos\\x",
            "../local/path",
            "git@github.com:acme/conduit.git and more",
        ] {
            assert!(!looks_like_git_url(no), "{no:?}");
        }
    }

    // ---- classifier -----------------------------------------------------------------------------

    #[test]
    fn classify_known_failures() {
        let unknown_host = "The authenticity of host 'gh (1.2.3.4)' can't be established.\n\
            ED25519 key fingerprint is SHA256:abc123.\n\
            Host key verification failed.\nfatal: Could not read from remote repository.\n";
        let prompt = HostKeyPrompt {
            host: "gh".into(),
            key_type: "ED25519".into(),
            fingerprint: "SHA256:abc123".into(),
        };
        let cases: Vec<(&str, NetErrorKind)> = vec![
            ("remote: x\nCancelled\n", NetErrorKind::Cancelled),
            (unknown_host, NetErrorKind::HostKey(Some(Box::new(prompt)))),
            ("Host key verification failed.\n", NetErrorKind::HostKey(None)),
            ("git@github.com: Permission denied (publickey).\n", NetErrorKind::AuthFailed),
            (
                "fatal: could not read Username for 'https://x': terminal prompts disabled\n",
                NetErrorKind::AuthFailed,
            ),
            (
                "fatal: destination path 'w' already exists and is not an empty directory.\n",
                NetErrorKind::DestinationExists,
            ),
            (
                "There is no tracking information for the current branch.\nPlease specify which branch\n",
                NetErrorKind::NoUpstream(None),
            ),
            (
                "fatal: The current branch feat has no upstream branch.\n",
                NetErrorKind::NoUpstream(Some("feat".into())),
            ),
            ("fatal: Not possible to fast-forward, aborting.\n", NetErrorKind::CannotFastForward),
            (" ! [rejected]        main -> main (fetch first)\n", NetErrorKind::Rejected),
            (" ! [rejected]        main -> main (stale info)\n", NetErrorKind::Rejected),
            ("error: cannot pull with rebase: You have unstaged changes.\n", NetErrorKind::DirtyWorktree),
            (
                "CONFLICT (content): Merge conflict in a.txt\nAutomatic merge failed\n",
                NetErrorKind::Conflicts,
            ),
            ("ssh: connect to host h port 22: Network is unreachable\n", NetErrorKind::Offline),
            ("fatal: unable to access 'https://x/': Could not resolve host: x\n", NetErrorKind::Offline),
            ("fatal: something new\n", NetErrorKind::Other),
        ];
        for (stderr, want) in cases {
            assert_eq!(classify(stderr), want, "{stderr:?}");
        }
    }

    #[test]
    fn messages_use_the_spec_wording() {
        let msg = |kind| NetError::from(kind).message();
        assert_eq!(
            msg(NetErrorKind::AuthFailed),
            "Authentication failed. Check your SSH agent or credential helper."
        );
        assert_eq!(msg(NetErrorKind::DestinationExists), "Folder already exists");
        assert_eq!(msg(NetErrorKind::NoUpstream(Some("feat".into()))), "No upstream for `feat`");
        assert_eq!(msg(NetErrorKind::Rejected), "Push rejected: remote has commits you don't have");
        assert_eq!(msg(NetErrorKind::Offline), "Offline");
        let git = GitError { command: "git x".into(), code: Some(1), stderr: "fatal: odd\n".into() };
        assert_eq!(NetError::from_git(git).message(), "fatal: odd");
    }

    // ---- end to end against a bare "remote" -----------------------------------------------------

    /// `remote.git` (bare, `main` with one commit) and two clones of it: `work`, driven through
    /// [`Git`], and `other`, standing in for a teammate.
    struct Fx {
        dir: tempfile::TempDir,
    }

    impl Fx {
        fn new() -> Self {
            let mut seed = TempRepo::new();
            seed.commit_file("a.txt", "1\n", "first");
            let fx = Self { dir: tempfile::tempdir().expect("tempdir") };
            let seed_path = seed.path().display().to_string();
            fx.sh(fx.dir.path(), &["clone", "-q", "--bare", &seed_path, "remote.git"]);
            for name in ["work", "other"] {
                fx.sh(fx.dir.path(), &["clone", "-q", "remote.git", name]);
                // `Git::run` doesn't get hermetic_git's identity: give the clone its own.
                for (k, v) in
                    [("user.name", "Ada"), ("user.email", "ada@example.com"), ("commit.gpgsign", "false")]
                {
                    fx.in_(name, &["config", k, v]);
                }
            }
            fx
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn git(&self, name: &str) -> Git {
            Git::new(Location::Local { path: self.path(name) })
        }

        fn sh(&self, dir: &Path, args: &[&str]) -> String {
            let out = hermetic_git(dir).args(args).output().expect("spawn git");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn in_(&self, name: &str, args: &[&str]) -> String {
            self.sh(&self.path(name), args)
        }

        fn commit(&self, name: &str, file: &str, msg: &str) -> String {
            std::fs::write(self.path(name).join(file), msg).expect("write");
            self.in_(name, &["add", "-A"]);
            self.in_(name, &["commit", "-q", "-m", msg]);
            self.in_(name, &["rev-parse", "HEAD"])
        }

        fn rev(&self, name: &str, rev: &str) -> String {
            self.in_(name, &["rev-parse", rev])
        }
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn run_plan(git: &Git, plan: &Plan) -> Result<(), GitError> {
        plan.iter().try_for_each(|cmd| git.run(&strs(cmd)).map(drop))
    }

    #[test]
    fn fetch_picks_up_new_commits_and_prunes() {
        let fx = Fx::new();
        fx.in_("other", &["push", "-q", "origin", "HEAD:refs/heads/gone"]);
        fetch(&fx.git("work"), &Fetch::Remote("origin".into()), &no_cancel(), &mut |_| {}).expect("fetch");
        assert!(fx.in_("work", &["branch", "-r"]).contains("origin/gone"));

        let new = fx.commit("other", "b.txt", "second");
        fx.in_("other", &["push", "-q", "origin", "HEAD:refs/heads/main", ":refs/heads/gone"]);
        let local_main = fx.rev("work", "main");
        fetch(&fx.git("work"), &Fetch::All, &no_cancel(), &mut |_| {}).expect("fetch all");
        assert_eq!(fx.rev("work", "origin/main"), new);
        assert!(!fx.in_("work", &["branch", "-r"]).contains("origin/gone"), "pruned");
        assert_eq!(fx.rev("work", "main"), local_main, "fetch never moves local branches");
    }

    #[test]
    fn pull_fast_forward_records_an_undoable_entry() {
        let fx = Fx::new();
        let before = fx.rev("work", "HEAD");
        let new = fx.commit("other", "b.txt", "second");
        fx.in_("other", &["push", "-q", "origin", "main"]);
        let git = fx.git("work");

        let entry = pull(&git, PullMode::Merge, false, 7, &no_cancel(), &mut |_| {})
            .expect("pull")
            .entry
            .expect("moved");
        assert_eq!(fx.rev("work", "HEAD"), new);
        assert_eq!(entry.description, "pull main");
        assert_eq!(entry.time, 7);
        assert_eq!(entry.before.refs.get("refs/heads/main"), Some(&before));
        assert_eq!(entry.after.refs.get("refs/heads/main"), Some(&new));

        let mut journal = Journal::default();
        journal.record(entry);
        let refs = ["refs/heads/main".to_string()];
        let snap = || read_snapshot(&git, &refs);
        journal.undo(&snap(), |p| run_plan(&git, p), snap).expect("undo");
        assert_eq!(fx.rev("work", "HEAD"), before, "undo resets to the pre-pull HEAD");
        journal.redo(&snap(), |p| run_plan(&git, p), snap).expect("redo");
        assert_eq!(fx.rev("work", "HEAD"), new);

        let again = pull(&git, PullMode::Merge, false, 8, &no_cancel(), &mut |_| {}).expect("pull");
        assert_eq!(again, Pulled { entry: None, autostash_conflicts: false }, "up to date");
    }

    /// `work` and `other` each commit on top of `main`; `other` pushes. Returns (ours, theirs).
    fn diverged() -> (Fx, String, String) {
        let fx = Fx::new();
        let theirs = fx.commit("other", "theirs.txt", "theirs");
        fx.in_("other", &["push", "-q", "origin", "main"]);
        let ours = fx.commit("work", "ours.txt", "ours");
        (fx, ours, theirs)
    }

    #[test]
    fn pull_merge_creates_a_merge_commit() {
        let (fx, ours, theirs) = diverged();
        let entry = pull(&fx.git("work"), PullMode::Merge, false, 0, &no_cancel(), &mut |_| {})
            .expect("pull")
            .entry
            .expect("moved");
        assert_eq!(fx.in_("work", &["show", "-s", "--format=%P", "HEAD"]), format!("{ours} {theirs}"));
        assert_eq!(entry.inverse, Some(vec![vec!["reset".into(), "--keep".into(), ours]]));
    }

    #[test]
    fn pull_rebase_replays_local_commits() {
        let (fx, ours, theirs) = diverged();
        pull(&fx.git("work"), PullMode::Rebase, false, 0, &no_cancel(), &mut |_| {})
            .expect("pull")
            .entry
            .expect("moved");
        assert_eq!(fx.rev("work", "HEAD~1"), theirs);
        assert_ne!(fx.rev("work", "HEAD"), ours);
        assert_eq!(fx.in_("work", &["log", "-1", "--format=%s"]), "ours");
    }

    #[test]
    fn pull_ff_only_refuses_diverged_branches() {
        let (fx, ours, _) = diverged();
        let err = pull(&fx.git("work"), PullMode::FastForwardOnly, false, 0, &no_cancel(), &mut |_| {})
            .expect_err("diverged");
        assert_eq!(err.kind, NetErrorKind::CannotFastForward);
        assert_eq!(fx.rev("work", "HEAD"), ours);
    }

    #[test]
    fn pull_rebase_with_a_dirty_tree_needs_autostash() {
        let (fx, _, theirs) = diverged();
        std::fs::write(fx.path("work").join("a.txt"), "dirty\n").expect("write");
        let git = fx.git("work");
        let err = pull(&git, PullMode::Rebase, false, 0, &no_cancel(), &mut |_| {}).expect_err("dirty");
        assert_eq!(err.kind, NetErrorKind::DirtyWorktree);
        let pulled = pull(&git, PullMode::Rebase, true, 0, &no_cancel(), &mut |_| {}).expect("autostash");
        assert!(pulled.entry.is_some() && !pulled.autostash_conflicts, "{pulled:?}");
        assert_eq!(fx.rev("work", "HEAD~1"), theirs);
        assert_eq!(std::fs::read_to_string(fx.path("work").join("a.txt")).expect("read"), "dirty\n");
    }

    #[test]
    fn pull_without_upstream_names_the_branch() {
        let fx = Fx::new();
        fx.in_("work", &["switch", "-q", "-c", "feat"]);
        let err = pull(&fx.git("work"), PullMode::Merge, false, 0, &no_cancel(), &mut |_| {})
            .expect_err("no upstream");
        assert_eq!(err.kind, NetErrorKind::NoUpstream(Some("feat".into())));
        assert_eq!(err.message(), "No upstream for `feat`");
    }

    #[test]
    fn push_new_branch_sets_upstream_and_journals_without_inverse() {
        let fx = Fx::new();
        fx.in_("work", &["switch", "-q", "-c", "feat"]);
        let head = fx.commit("work", "f.txt", "feature");
        let mut phases = Vec::new();
        let p = branch_push("feat", true, Force::No);
        let entry = push(&fx.git("work"), &p, &DEFAULT_PROTECTED, 0, &no_cancel(), &mut |pr| {
            phases.push(pr.phase.clone())
        })
        .expect("push");
        assert_eq!(fx.in_("work", &["rev-parse", "--abbrev-ref", "feat@{upstream}"]), "origin/feat");
        assert_eq!(fx.sh(&fx.path("remote.git"), &["rev-parse", "refs/heads/feat"]), head);
        assert_eq!(entry.inverse, None);
        assert_eq!(entry.description, "push origin feat");
        assert_eq!(entry.after.refs.get("refs/remotes/origin/feat"), Some(&head));
        assert!(phases.iter().any(|ph| ph == "Writing objects"), "{phases:?}");
    }

    #[test]
    fn push_rejected_non_fast_forward_is_classified() {
        let (fx, _, _) = diverged();
        let p = branch_push("main", false, Force::No);
        let err = push(&fx.git("work"), &p, &DEFAULT_PROTECTED, 0, &no_cancel(), &mut |_| {})
            .expect_err("rejected");
        assert_eq!(err.kind, NetErrorKind::Rejected);
        assert_eq!(err.message(), "Push rejected: remote has commits you don't have");
        assert!(err.git.expect("git ran").stderr.contains("[rejected]"));
    }

    #[test]
    fn lease_push_succeeds_or_fails_as_git_decides() {
        let (fx, ours, _) = diverged();
        let git = fx.git("work");
        let lease = branch_push("main", false, Force::WithLease);
        // Our origin/main is stale (other pushed after we cloned): the lease refuses.
        let err = push(&git, &lease, &NONE_PROTECTED, 0, &no_cancel(), &mut |_| {}).expect_err("stale lease");
        assert_eq!(err.kind, NetErrorKind::Rejected);
        // After a fetch the lease matches the remote and the force push goes through.
        fetch(&git, &Fetch::Remote("origin".into()), &no_cancel(), &mut |_| {}).expect("fetch");
        push(&git, &lease, &NONE_PROTECTED, 0, &no_cancel(), &mut |_| {}).expect("lease holds");
        assert_eq!(fx.sh(&fx.path("remote.git"), &["rev-parse", "refs/heads/main"]), ours);
    }

    #[test]
    fn force_push_to_a_protected_branch_never_runs() {
        let (fx, _, theirs) = diverged();
        for force in [Force::WithLease, Force::Plain] {
            let p = branch_push("main", false, force);
            let err =
                push(&fx.git("work"), &p, &DEFAULT_PROTECTED, 0, &no_cancel(), &mut |_| panic!("ran git"))
                    .expect_err("protected");
            assert_eq!(err, NetError::from(NetErrorKind::ProtectedBranch("main".into())));
        }
        assert_eq!(fx.sh(&fx.path("remote.git"), &["rev-parse", "refs/heads/main"]), theirs);
    }

    #[test]
    fn push_tags_and_delete_remote_refs() {
        let fx = Fx::new();
        let git = fx.git("work");
        let remote_tags = || fx.sh(&fx.path("remote.git"), &["tag"]);
        let run = |p: Push| push(&git, &p, &DEFAULT_PROTECTED, 0, &no_cancel(), &mut |_| {});
        fx.in_("work", &["tag", "v1"]);
        fx.in_("work", &["tag", "v2"]);
        run(Push::Tag { remote: "origin".into(), tag: "v1".into() }).expect("push tag");
        assert_eq!(remote_tags(), "v1");
        run(Push::AllTags { remote: "origin".into() }).expect("push --tags");
        assert_eq!(remote_tags(), "v1\nv2");
        let entry =
            run(Push::Delete { remote: "origin".into(), refname: "refs/tags/v1".into() }).expect("delete");
        assert_eq!(remote_tags(), "v2");
        assert_eq!(entry.inverse, None);
    }

    #[test]
    fn clone_reports_progress_parsed_from_real_stderr() {
        let fx = Fx::new();
        for i in 0..30 {
            fx.commit("other", &format!("f{i}.txt"), &"line\n".repeat(i + 1));
        }
        fx.in_("other", &["push", "-q", "origin", "main"]);
        let url = format!("file://{}", fx.path("remote.git").display());
        let mut seen: Vec<Progress> = Vec::new();
        let loc = clone(&fx.git("clones"), &url, "c", Some(5), &no_cancel(), &mut |p| seen.push(p.clone()))
            .expect("clone into a parent that doesn't exist yet");
        assert_eq!(loc, Location::Local { path: fx.path("clones").join("c") });
        assert_eq!(fx.sh(&fx.path("clones").join("c"), &["rev-list", "--count", "HEAD"]), "5", "shallow");
        let receiving: Vec<&Progress> = seen.iter().filter(|p| p.phase == "Receiving objects").collect();
        assert!(!receiving.is_empty(), "{seen:?}");
        assert!(receiving.windows(2).all(|w| w[0].percent <= w[1].percent), "{receiving:?}");
        assert_eq!(receiving.last().and_then(|p| p.percent), Some(100));
    }

    #[test]
    fn clone_into_existing_folder_is_refused_and_left_alone() {
        let fx = Fx::new();
        std::fs::write(fx.path("work").join("keep.txt"), "mine").expect("write");
        let git = Git::new(Location::Local { path: fx.dir.path().to_path_buf() });
        let err = clone(&git, "remote.git", "work", None, &no_cancel(), &mut |_| {}).expect_err("exists");
        assert_eq!(err.kind, NetErrorKind::DestinationExists);
        assert_eq!(err.message(), "Folder already exists");
        assert!(fx.path("work").join("keep.txt").exists());
    }

    #[test]
    fn clone_cancelled_before_start_creates_nothing() {
        let fx = Fx::new();
        let git = Git::new(Location::Local { path: fx.dir.path().to_path_buf() });
        let err =
            clone(&git, "remote.git", "c", None, &AtomicBool::new(true), &mut |_| {}).expect_err("cancelled");
        assert_eq!(err.kind, NetErrorKind::Cancelled);
        assert!(!fx.path("c").exists());
    }

    /// Cancel mid-run, deterministically: the first progress redraw sets the flag, so the runner
    /// kills git before it finishes, and the partial folder must go — unless it was there before.
    #[test]
    fn clone_cancelled_mid_run_deletes_the_partial_folder_but_not_a_preexisting_one() {
        let fx = Fx::new();
        let url = format!("file://{}", fx.path("remote.git").display());
        let git = Git::new(Location::Local { path: fx.dir.path().to_path_buf() });
        for preexisting in [false, true] {
            let dest = fx.path("c");
            if preexisting {
                std::fs::create_dir(&dest).expect("mkdir");
            }
            let cancel = AtomicBool::new(false);
            let err = clone(&git, &url, "c", None, &cancel, &mut |_| cancel.store(true, Ordering::Relaxed))
                .expect_err("cancelled");
            assert_eq!(err.kind, NetErrorKind::Cancelled);
            assert_eq!(dest.exists(), preexisting, "preexisting: {preexisting}");
            if preexisting {
                assert!(is_empty_dir(&dest), "git's partial .git is gone");
            }
        }
        // The emptied folder takes a retry.
        clone(&git, &url, "c", None, &no_cancel(), &mut |_| {}).expect("retry");
        assert!(fx.path("c").join("a.txt").exists());
    }

    // ---- regressions ----------------------------------------------------------------------------

    #[test]
    fn push_normalizes_full_branch_names_and_refuses_protected_deletes() {
        let full = branch_push("refs/heads/feat", false, Force::No);
        assert_eq!(strs(&full.args()), ["push", "--progress", "origin", "refs/heads/feat"]);
        assert_eq!(full.description(), "push origin feat");
        assert_eq!(
            check_push_allowed(&branch_push("refs/heads/main", false, Force::Plain), &DEFAULT_PROTECTED),
            Err(NetErrorKind::ProtectedBranch("main".into()))
        );
        let delete = |refname: &str| Push::Delete { remote: "origin".into(), refname: refname.into() };
        for protected in ["refs/heads/main", "main", "refs/heads/release/1"] {
            assert!(
                matches!(
                    check_push_allowed(&delete(protected), &DEFAULT_PROTECTED),
                    Err(NetErrorKind::ProtectedBranchDelete(_))
                ),
                "{protected}"
            );
        }
        for allowed in ["refs/heads/feat", "refs/tags/main"] {
            assert_eq!(check_push_allowed(&delete(allowed), &DEFAULT_PROTECTED), Ok(()), "{allowed}");
        }
        let err = NetError::from(NetErrorKind::ProtectedBranchDelete("main".into()));
        assert_eq!(err.message(), "`main` is protected: deleting it on the remote is blocked");
    }

    #[test]
    fn deleting_a_protected_branch_on_the_remote_never_runs() {
        let fx = Fx::new();
        let p = Push::Delete { remote: "origin".into(), refname: "refs/heads/main".into() };
        let err = push(&fx.git("work"), &p, &DEFAULT_PROTECTED, 0, &no_cancel(), &mut |_| panic!("ran git"))
            .expect_err("protected");
        assert_eq!(err.kind, NetErrorKind::ProtectedBranchDelete("main".into()));
        fx.sh(&fx.path("remote.git"), &["rev-parse", "--verify", "refs/heads/main"]);
    }

    /// `git merge` prints `CONFLICT (content): …` on stdout, not stderr: the conflict must still
    /// be recognised (§5.10 "Conflicts → 5.17").
    #[test]
    fn pull_merge_that_stops_on_conflicts_is_classified_conflicts() {
        let fx = Fx::new();
        fx.commit("other", "a.txt", "theirs");
        fx.in_("other", &["push", "-q", "origin", "main"]);
        let ours = fx.commit("work", "a.txt", "ours");
        let err = pull(&fx.git("work"), PullMode::Merge, false, 0, &no_cancel(), &mut |_| {})
            .expect_err("conflicts");
        assert_eq!(err.kind, NetErrorKind::Conflicts);
        assert_eq!(err.message(), "Pull stopped on conflicts");
        assert_eq!(fx.rev("work", "HEAD"), ours);
        assert!(fx.in_("work", &["status", "--porcelain"]).contains("UU a.txt"));
    }

    #[test]
    fn pull_rebase_that_stops_on_conflicts_is_classified_conflicts() {
        let fx = Fx::new();
        fx.commit("other", "a.txt", "theirs");
        fx.in_("other", &["push", "-q", "origin", "main"]);
        fx.commit("work", "a.txt", "ours");
        let err = pull(&fx.git("work"), PullMode::Rebase, false, 0, &no_cancel(), &mut |_| {})
            .expect_err("conflicts");
        assert_eq!(err.kind, NetErrorKind::Conflicts);
    }

    /// git exits 0 when popping the autostash conflicts; the pull landed (and is journaled) but
    /// the caller must hand over to §5.17.
    #[test]
    fn pull_whose_autostash_pop_conflicts_reports_it() {
        let fx = Fx::new();
        let theirs = fx.commit("other", "a.txt", "theirs");
        fx.in_("other", &["push", "-q", "origin", "main"]);
        std::fs::write(fx.path("work").join("a.txt"), "dirty\n").expect("write");
        let pulled =
            pull(&fx.git("work"), PullMode::Rebase, true, 0, &no_cancel(), &mut |_| {}).expect("pull landed");
        assert!(pulled.autostash_conflicts, "{pulled:?}");
        assert_eq!(pulled.entry.expect("journaled").after.head, Some(theirs));
        assert!(fx.in_("work", &["status", "--porcelain"]).contains("UU a.txt"));
    }

    /// Pulling into an unborn branch creates the branch ref: journaled, undo deletes it again.
    #[test]
    fn pull_into_an_unborn_branch_is_journaled() {
        let fx = Fx::new();
        let remote_main = fx.rev("work", "origin/main");
        fx.in_("work", &["switch", "-q", "--orphan", "fresh"]);
        fx.in_("work", &["config", "branch.fresh.remote", "origin"]);
        fx.in_("work", &["config", "branch.fresh.merge", "refs/heads/main"]);
        let git = fx.git("work");
        let entry = pull(&git, PullMode::Merge, false, 0, &no_cancel(), &mut |_| {})
            .expect("pull")
            .entry
            .expect("the branch was created");
        assert_eq!(fx.rev("work", "HEAD"), remote_main);
        assert_eq!(entry.before.head, None);

        let mut journal = Journal::default();
        journal.record(entry);
        let refs = ["refs/heads/fresh".to_string()];
        let snap = || read_snapshot(&git, &refs);
        journal.undo(&snap(), |p| run_plan(&git, p), snap).expect("undo");
        assert_eq!(snap().head, None, "unborn again");
        assert_eq!(snap().branch.as_deref(), Some("fresh"));
        journal.redo(&snap(), |p| run_plan(&git, p), snap).expect("redo");
        assert_eq!(fx.rev("work", "HEAD"), remote_main);
    }

    #[test]
    fn pull_on_a_detached_head_is_refused_before_git_runs() {
        let fx = Fx::new();
        fx.in_("work", &["checkout", "-q", "--detach"]);
        let err = pull(&fx.git("work"), PullMode::Merge, false, 0, &AtomicBool::new(false), &mut |_| {
            panic!("ran git")
        })
        .expect_err("detached");
        assert_eq!(err, NetError::from(NetErrorKind::DetachedHead));
        assert_eq!(err.message(), "Can't pull: HEAD is detached, not on a branch");
    }

    /// A stand-in ssh host (run with `ssh_bin = "sh"`): runs the remote command locally.
    fn fake_host(dir: &Path) -> String {
        let path = dir.join("fake-host.sh");
        let script = format!(
            "while [ \"$1\" != -- ]; do shift; done\n\
             exec env -i PATH=\"$PATH\" HOME={} GIT_CONFIG_NOSYSTEM=1 sh -c \"$2\"\n",
            ssh::quote(&dir.display().to_string())
        );
        std::fs::write(&path, script).expect("write fake host");
        path.display().to_string()
    }

    #[test]
    fn clone_on_a_remote_host_returns_the_remote_location_and_leaves_nothing_on_failure() {
        let fx = Fx::new();
        let host_dir = tempfile::tempdir().expect("tempdir");
        let host = fake_host(host_dir.path());
        let parent = fx.dir.path().display().to_string();
        let git = Git {
            location: Location::Remote { host: host.clone(), path: parent.clone() },
            git_bin: "git".into(),
            ssh_bin: "sh".into(),
            control_dir: None,
        };
        let loc = clone(&git, "remote.git", "c", None, &no_cancel(), &mut |_| {}).expect("remote clone");
        assert_eq!(loc, Location::Remote { host, path: format!("{parent}/c") });
        assert!(fx.path("c").join("a.txt").exists());

        let err = clone(&git, "missing.git", "d", None, &no_cancel(), &mut |_| {}).expect_err("no repo");
        assert_eq!(err.kind, NetErrorKind::Other);
        assert!(!fx.path("d").exists());
        let err = clone(&git, "remote.git", "c", None, &no_cancel(), &mut |_| {}).expect_err("exists");
        assert_eq!(err.kind, NetErrorKind::DestinationExists);
    }

    // ---- background fetch -----------------------------------------------------------------------

    fn conditions(now: u64) -> FetchConditions<'static> {
        FetchConditions {
            interval_min: 5,
            bound_remote: Some("origin"),
            now,
            online: true,
            connected: true,
            in_progress: false,
            battery: None,
            skip_on_low_battery: true,
        }
    }

    #[test]
    fn background_fetch_runs_the_bound_remote_on_the_interval() {
        let mut bg = BackgroundFetch::default();
        assert_eq!(bg.due(&conditions(1_000)), Ok(Fetch::Remote("origin".into())), "never fetched");
        bg.started(1_000);
        assert_eq!(bg.due(&conditions(1_000 + 299)), Err(Skip::NotDue));
        assert_eq!(bg.due(&conditions(1_000 + 300)), Ok(Fetch::Remote("origin".into())));
        let off = FetchConditions { interval_min: 0, ..conditions(5_000) };
        assert_eq!(bg.due(&off), Err(Skip::Off));
        let unbound = FetchConditions { bound_remote: None, ..conditions(5_000) };
        assert_eq!(bg.due(&unbound), Err(Skip::NoRemote));
    }

    #[test]
    fn background_fetch_skip_rules() {
        let bg = BackgroundFetch::default();
        let c = conditions(0);
        let battery = |discharging, percent| Some(Battery { discharging, percent });
        let cases = [
            (FetchConditions { online: false, ..c.clone() }, Err(Skip::Offline)),
            (FetchConditions { connected: false, ..c.clone() }, Err(Skip::Disconnected)),
            (FetchConditions { in_progress: true, ..c.clone() }, Err(Skip::InProgress)),
            (FetchConditions { battery: battery(true, 19), ..c.clone() }, Err(Skip::LowBattery)),
            (FetchConditions { battery: battery(true, 20), ..c.clone() }, Ok(Fetch::Remote("origin".into()))),
            (FetchConditions { battery: battery(false, 5), ..c.clone() }, Ok(Fetch::Remote("origin".into()))),
            (
                FetchConditions { battery: battery(true, 5), skip_on_low_battery: false, ..c.clone() },
                Ok(Fetch::Remote("origin".into())),
            ),
        ];
        for (conditions, want) in cases {
            assert_eq!(bg.due(&conditions), want, "{conditions:?}");
        }
    }

    #[test]
    fn background_fetch_failures_toast_once_then_only_the_status_bar() {
        let mut bg = BackgroundFetch::default();
        let failed = || Err(NetError::from(NetErrorKind::AuthFailed));
        assert_eq!(bg.finished(100, true, &failed()), Notice::Toast);
        assert_eq!(bg.finished(400, true, &failed()), Notice::StatusBar);
        assert_eq!(bg.status_label(400 + 180).as_deref(), Some("Fetch failed · 3m"));
        assert_eq!(bg.finished(500, false, &failed()), Notice::Toast, "a manual fetch always toasts");
        assert_eq!(bg.finished(700, true, &Err(NetErrorKind::Offline.into())), Notice::None, "silent");
        assert_eq!(bg.finished(900, false, &Ok(())), Notice::None);
        assert_eq!((bg.failures, bg.status_label(900)), (0, None), "a success clears it");
        assert_eq!(bg.finished(1_000, true, &failed()), Notice::Toast, "the first failure again");
    }
}
