//! Fetch, pull, and push (§5.10) in the git pane: the header toolbar (Fetch ▾, Pull split
//! button, Push split button with **Push to…** and force push), the progress pill with
//! **Cancel**, the action toasts ("No upstream for `feat`", "Push `feat` to origin and set
//! upstream?", "Push rejected…", **Stash, pull, pop**), the plain `--force` confirmation (§5.20),
//! background fetch of the bound remote, and the status bar's sync part.
//!
//! Every network command runs `git::net` on a worker (`ui::jobs`) and reports back over this
//! module's own channel, drained by [`GitPane::sync_tick`] — which the app calls every frame for
//! every workspace, so background fetches of hidden workspaces land too. The decisions (which
//! remote, when to ask, which toast follows a failure) live headless in `model::sync`.
//!
//! The action toasts render as a toast-styled strip under the toolbar rather than in the app's
//! toast stack: `chrome::Toast` has no buttons.

use super::{GitEvent, GitPane};
use crate::git::journal::Entry;
use crate::git::net::{self, BackgroundFetch, Fetch, NetError, NetErrorKind, Notice, Pulled, Push};
pub use crate::git::net::{Force, PullMode};
use crate::git::ops::{self, Executed, Op, OpError};
use crate::git::refs::RefKind;
use crate::git::status::EntryKind;
use crate::git::{Git, GitError};
use crate::model::confirm::{Confirm, ConfirmKind, Decision};
use crate::model::keymap::Preset;
use crate::model::settings::Settings;
use crate::model::sync::{self as plan, Choice, NewBranchAsks, Prompt, PullPlan, PushFacts, PushPlan};
use crate::model::theme::Token;
use crate::ui::chrome::{SyncStatus, Toast};
use crate::ui::dialogs::{self, FormOutcome, Popover};
use crate::ui::jobs;
use crate::ui::theme::Colors;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

/// A fetch / pull / push asked for from outside the toolbar: context menus, the palette,
/// shortcuts. `None` fields mean the default (the bound remote, the current branch, the
/// `pull.rebase` mode).
#[derive(Debug, Clone, PartialEq, Eq)]
// The tag and remote-delete variants are built by the Refs / graph context menus (§5.9, §5.11).
#[allow(dead_code)]
pub enum SyncRequest {
    /// `all` fetches every remote (`remote` is then ignored); `prune: false` drops `--prune`.
    Fetch {
        remote: Option<String>,
        all: bool,
        prune: bool,
    },
    Pull {
        mode: Option<PullMode>,
    },
    /// `Force::Plain` always confirms here (§5.20); protected branches are refused here.
    Push {
        remote: Option<String>,
        branch: Option<String>,
        force: Force,
    },
    PushTag {
        remote: String,
        tag: String,
    },
    PushAllTags {
        remote: String,
    },
    /// `refname` is a full ref (`refs/heads/x`, `refs/tags/v1`). Deleting a tag on the remote
    /// shows the §5.20 **Delete remote tag** confirmation here, so callers don't confirm again;
    /// deleting a remote branch runs as asked (its confirmation belongs to the delete-branch
    /// dialog). Protected branches are refused.
    DeleteRemote {
        remote: String,
        refname: String,
    },
}

enum Job {
    Fetch { target: Fetch, prune: bool, background: bool },
    Pull { mode: PullMode, autostash: bool },
    Push(Push),
}

impl Job {
    /// The progress pill's text until git reports a phase.
    fn label(&self) -> String {
        match self {
            Job::Fetch { target: Fetch::All, .. } => "Fetching all remotes".to_string(),
            Job::Fetch { target: Fetch::Remote(r), .. } => format!("Fetching {r}"),
            Job::Pull { .. } => "Pulling".to_string(),
            Job::Push(Push::Branch { remote, .. }) => format!("Pushing to {remote}"),
            Job::Push(Push::Delete { remote, .. }) => format!("Deleting on {remote}"),
            Job::Push(Push::Tag { remote, .. } | Push::AllTags { remote }) => {
                format!("Pushing tags to {remote}")
            }
        }
    }
}

enum SyncReply {
    Progress(net::Progress),
    Fetched {
        target: Fetch,
        background: bool,
        result: Result<(), NetError>,
    },
    Pulled {
        mode: PullMode,
        result: Result<Pulled, NetError>,
    },
    Pushed {
        push: Push,
        result: Result<Entry, NetError>,
    },
    PullConfig(Option<String>),
    UpstreamSet {
        branch: String,
        upstream: String,
        retry: Option<PullMode>,
        result: Result<Executed, OpError>,
    },
    /// The remote commits a plain force push would drop, for the §5.20 body.
    ForceLost {
        push: Push,
        lost: Vec<String>,
    },
}

struct Running {
    label: String,
    progress: Option<net::Progress>,
    cancel: Arc<AtomicBool>,
}

/// An open W19 popover.
enum SyncForm {
    SetUpstream { branch: String, value: String, retry: Option<PullMode> },
    ChooseRemote { push: Push, choice: usize },
}

/// The pane's fetch/pull/push state.
pub(super) struct SyncState {
    tx: Sender<SyncReply>,
    rx: Receiver<SyncReply>,
    running: Option<Running>,
    background: BackgroundFetch,
    /// When the last fetch of anything succeeded ("Last fetch Xm").
    last_fetch: Option<u64>,
    /// `git config pull.rebase`; `None` until read.
    pull_rebase: Option<Option<String>>,
    config_requested: bool,
    prompt: Option<Prompt>,
    confirm: Option<(Confirm, Push)>,
    form: Option<SyncForm>,
    asks: NewBranchAsks,
    /// **Pull then push**: the push to run once the pull succeeds.
    then_push: Option<Push>,
    // -- from the app / settings, refreshed every tick --
    bound_remote: Option<String>,
    protected: Vec<String>,
    always_push: bool,
    /// Toasts raised outside `show` (requests, replies), handed out by the next tick.
    outbox: Vec<GitEvent>,
    /// A worker reply raised a prompt or confirmation the pane must show; the app opens the
    /// pane (it may be closed: `Mod+P` works without it) via [`GitPane::take_sync_attention`].
    attention: bool,
    /// Journal entries of pulls/pushes that finished while an undo/redo held the journal on
    /// its worker; recorded once it is back.
    pending_entries: Vec<crate::git::journal::Entry>,
    anchor: egui::Pos2,
}

impl Default for SyncState {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            running: None,
            background: BackgroundFetch::default(),
            last_fetch: None,
            pull_rebase: None,
            config_requested: false,
            prompt: None,
            confirm: None,
            form: None,
            asks: NewBranchAsks::default(),
            then_push: None,
            bound_remote: None,
            protected: Settings::default().git.protected_branches,
            always_push: false,
            outbox: Vec::new(),
            attention: false,
            pending_entries: Vec::new(),
            anchor: egui::pos2(16.0, 64.0),
        }
    }
}

/// A failed network command as a toast: the spec's line, then command + stderr (§5.24).
fn net_toast(title: String, err: &NetError) -> Toast {
    let details =
        err.git.as_ref().map_or_else(String::new, |g| format!("{}\n{}", g.command, g.stderr.trim_end()));
    Toast::error(title, details)
}

fn git_toast(e: &GitError) -> Toast {
    Toast::error(e.summary(), format!("{}\n{}", e.command, e.stderr.trim_end()))
}

/// `git fetch` without `--prune` when a caller asks for that; otherwise `net::fetch`.
fn fetch(
    git: &Git,
    target: &Fetch,
    prune: bool,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&net::Progress),
) -> Result<(), NetError> {
    if prune {
        return net::fetch(git, target, cancel, on_progress);
    }
    let args: Vec<String> = net::fetch_args(target).into_iter().filter(|a| a != "--prune").collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git.run_streaming(&args, cancel, &mut |segment| {
        if let Some(p) = net::parse_progress(segment) {
            on_progress(&p);
        }
    })
    .map(drop)
    .map_err(|e| NetError { kind: net::classify(&e.stderr), git: Some(Box::new(e)) })
}

// ---- requests ------------------------------------------------------------------------------

impl GitPane {
    /// Run a fetch / pull / push from a menu, the palette, or a shortcut (§5.10). Never blocks:
    /// git runs on a worker; refusals and results arrive as toasts through [`Self::sync_tick`].
    pub fn sync(&mut self, ctx: &egui::Context, request: SyncRequest) {
        match request {
            SyncRequest::Fetch { remote, all, prune } => {
                let target = if all {
                    Fetch::All
                } else {
                    match remote.or_else(|| self.default_remote()) {
                        Some(r) => Fetch::Remote(r),
                        None => {
                            return self
                                .notify(Toast::info("Nothing to fetch: this repository has no remote"));
                        }
                    }
                };
                self.start_job(ctx, Job::Fetch { target, prune, background: false });
            }
            SyncRequest::Pull { mode } => self.request_pull(ctx, mode),
            SyncRequest::Push { remote, branch, force } => self.request_push(ctx, remote, branch, force),
            SyncRequest::PushTag { remote, tag } => {
                // An empty remote (a menu's "Push to <bound remote>") means the default remote.
                let remote = Some(remote).filter(|r| !r.is_empty()).or_else(|| self.default_remote());
                match remote {
                    Some(remote) => self.start_job(ctx, Job::Push(Push::Tag { remote, tag })),
                    None => self.notify(Toast::info("This repository has no remote")),
                }
            }
            SyncRequest::PushAllTags { remote } => self.start_job(ctx, Job::Push(Push::AllTags { remote })),
            SyncRequest::DeleteRemote { remote, refname } => {
                let push = Push::Delete { remote: remote.clone(), refname: refname.clone() };
                match refname.strip_prefix("refs/tags/") {
                    Some(tag) => {
                        let confirm = Confirm::new(ConfirmKind::DeleteRemoteTag, format!("Delete tag `{tag}` on {remote}?"))
                            .detail("The tag is removed from the remote for everyone who fetches; your local tag stays.");
                        self.sync.confirm = Some((confirm, push));
                    }
                    None => self.start_job(ctx, Job::Push(push)),
                }
            }
        }
    }

    /// Whether a toast, confirmation, or popover is waiting in the pane's header, so the app
    /// can open the pane after a shortcut.
    pub fn sync_needs_attention(&self) -> bool {
        self.sync.prompt.is_some() || self.sync.confirm.is_some() || self.sync.form.is_some()
    }

    /// Whether a fetch/pull/push job is running; undo/redo waits for it (both move refs).
    pub(super) fn sync_busy(&self) -> bool {
        self.sync.running.is_some()
    }

    /// Once per raised prompt: its title, when a worker reply left a toast or confirmation in
    /// the header that the user has not seen. The app opens the pane for it.
    pub fn take_sync_attention(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.sync.attention) {
            return None;
        }
        match (&self.sync.prompt, &self.sync.confirm) {
            (Some(prompt), _) => Some(prompt.title()),
            (None, Some((confirm, _))) => Some(confirm.title.clone()),
            (None, None) => None,
        }
    }

    /// Records a journal entry and saves it, like `apply_checkout`. While an undo/redo has the
    /// journal on its worker the entry waits in `pending_entries` (the worker's journal would
    /// otherwise replace it).
    fn record_journal(&mut self, entry: crate::git::journal::Entry) {
        self.sync.pending_entries.push(entry);
        self.flush_journal();
    }

    fn flush_journal(&mut self) {
        if self.undo_busy || self.sync.pending_entries.is_empty() {
            return;
        }
        for entry in std::mem::take(&mut self.sync.pending_entries) {
            self.journal.record(entry);
        }
        if let Err(e) = self.journal.save(&self.journal_path) {
            self.notify(Toast::warning("Could not save the undo history", e.to_string()));
        }
    }

    fn notify(&mut self, toast: Toast) {
        self.sync.outbox.push(GitEvent::Toast(toast));
    }

    fn default_pull_mode(&self) -> PullMode {
        PullMode::from_config(self.sync.pull_rebase.as_ref().and_then(|c| c.as_deref()))
    }

    fn request_pull(&mut self, ctx: &egui::Context, mode: Option<PullMode>) {
        let mode = mode.unwrap_or_else(|| self.default_pull_mode());
        let dirty =
            self.status.entries.iter().any(|e| !matches!(e.kind, EntryKind::Untracked | EntryKind::Ignored));
        let branch = self.status.branch.head.clone();
        let upstream = self.status.branch.upstream.clone();
        self.run_pull_plan(ctx, plan::plan_pull(branch.as_deref(), upstream.as_deref(), mode, dirty));
    }

    fn run_pull_plan(&mut self, ctx: &egui::Context, pull: PullPlan) {
        match pull {
            PullPlan::Run { mode, autostash } => self.start_job(ctx, Job::Pull { mode, autostash }),
            PullPlan::AskStash(mode) => self.sync.prompt = Some(Prompt::StashPull(mode)),
            PullPlan::NoUpstream(branch) => self.sync.prompt = Some(Prompt::SetUpstream { branch }),
            PullPlan::Detached => {
                self.notify(Toast::info(NetError::from(NetErrorKind::DetachedHead).message()));
            }
        }
        if !matches!(self.sync.prompt, Some(Prompt::StashPull(_))) && self.sync.running.is_none() {
            self.sync.then_push = None;
        }
    }

    fn request_push(
        &mut self,
        ctx: &egui::Context,
        remote: Option<String>,
        branch: Option<String>,
        force: Force,
    ) {
        let current = self.status.branch.head.clone();
        let branch = branch.or(current.clone());
        let upstream = match (&branch, &current) {
            (Some(b), Some(c)) if b == c => self.status.branch.upstream.clone(),
            (Some(b), _) => self
                .refs_list
                .iter()
                .find(|r| r.kind == RefKind::Local && r.short == *b)
                .and_then(|r| r.upstream.clone()),
            (None, _) => None,
        };
        let remotes = self.remote_names();
        let facts = PushFacts {
            branch: branch.as_deref(),
            upstream: upstream.as_deref(),
            remote: remote.as_deref(),
            bound_remote: self.sync.bound_remote.as_deref(),
            remotes: &remotes,
            force,
            protected: &self.sync.protected,
            ask_new_branch: self.sync.asks.should_ask(branch.as_deref(), self.sync.always_push),
        };
        match plan::plan_push(&facts) {
            PushPlan::Run(push) => self.start_job(ctx, Job::Push(push)),
            PushPlan::AskSetUpstream(push) => self.sync.prompt = Some(Prompt::PushNewBranch(push)),
            PushPlan::Confirm(push) => self.dispatch_force_lost(ctx, push),
            PushPlan::Refuse(why) => self.notify(Toast::error(why.message(), String::new())),
        }
    }

    /// Reads the remote commits a plain `--force` would drop, then shows the §5.20 dialog.
    fn dispatch_force_lost(&self, ctx: &egui::Context, push: Push) {
        let git = self.git.clone();
        jobs::spawn(ctx, &self.sync.tx, move || {
            let lost = match &push {
                Push::Branch { remote, branch, .. } => {
                    let range = format!("refs/heads/{branch}..refs/remotes/{remote}/{branch}");
                    git.run(&["log", "--format=%h %s", "-n", "200", &range, "--"])
                        .map(|out| String::from_utf8_lossy(&out).lines().map(str::to_string).collect())
                        .unwrap_or_default()
                }
                _ => Vec::new(),
            };
            SyncReply::ForceLost { push, lost }
        });
    }

    fn start_job(&mut self, ctx: &egui::Context, job: Job) {
        if let Some(running) = &self.sync.running {
            let busy = format!("Wait for “{}” to finish", running.label);
            return self.notify(Toast::info(busy));
        }
        if self.undo_busy {
            if matches!(job, Job::Fetch { background: true, .. }) {
                return; // the next tick tries again
            }
            return self.notify(Toast::info("Wait for undo to finish"));
        }
        let now = crate::util::unix_now();
        if let Job::Fetch { target, .. } = &job
            && self.fetches_bound(target)
        {
            self.sync.background.started(now);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.sync.running = Some(Running { label: job.label(), progress: None, cancel: Arc::clone(&cancel) });
        let (git, protected) = (self.git.clone(), self.sync.protected.clone());
        let (progress_tx, wake) = (self.sync.tx.clone(), ctx.clone());
        jobs::spawn(ctx, &self.sync.tx, move || {
            let mut on_progress = |p: &net::Progress| {
                if progress_tx.send(SyncReply::Progress(p.clone())).is_ok() {
                    wake.request_repaint();
                }
            };
            match job {
                Job::Fetch { target, prune, background } => {
                    let result = fetch(&git, &target, prune, &cancel, &mut on_progress);
                    SyncReply::Fetched { target, background, result }
                }
                Job::Pull { mode, autostash } => {
                    let result = net::pull(&git, mode, autostash, now, &cancel, &mut on_progress);
                    SyncReply::Pulled { mode, result }
                }
                Job::Push(push) => {
                    let result = net::push(&git, &push, &protected, now, &cancel, &mut on_progress);
                    SyncReply::Pushed { push, result }
                }
            }
        });
    }

    /// Whether `target` includes the bound remote, whose fetches reset the background timer.
    fn fetches_bound(&self, target: &Fetch) -> bool {
        match target {
            Fetch::All => self.sync.bound_remote.is_some(),
            Fetch::Remote(r) => self.sync.bound_remote.as_deref() == Some(r.as_str()),
        }
    }

    fn set_upstream(&self, ctx: &egui::Context, branch: String, upstream: String, retry: Option<PullMode>) {
        let git = self.git.clone();
        jobs::spawn(ctx, &self.sync.tx, move || {
            let op = Op::SetUpstream { branch: branch.clone(), upstream: Some(upstream.clone()) };
            let result = ops::execute(&git, &op, crate::util::unix_now());
            SyncReply::UpstreamSet { branch, upstream, retry, result }
        });
    }
}

// ---- tick: replies, background fetch -------------------------------------------------------

impl GitPane {
    /// Every frame, for every workspace (visible or not): apply finished jobs, start a due
    /// background fetch of `bound_remote` (§5.10), and return the toasts raised since the last
    /// tick. There is no battery reading yet (`platform` has none), so the low-battery skip
    /// never triggers; network state is learned from the fetch itself (offline failures stay
    /// silent after the first, per `BackgroundFetch::finished`).
    pub fn sync_tick(
        &mut self,
        ctx: &egui::Context,
        settings: &Settings,
        bound_remote: Option<&str>,
        now: u64,
    ) -> Vec<GitEvent> {
        self.sync.bound_remote = bound_remote.filter(|r| !r.is_empty()).map(str::to_string);
        if self.sync.protected != settings.git.protected_branches {
            self.sync.protected = settings.git.protected_branches.clone();
        }
        self.sync.always_push = settings.git.push_new_branches_without_asking;
        while let Ok(reply) = self.sync.rx.try_recv() {
            self.receive_sync_reply(ctx, reply, now);
        }
        self.flush_journal();
        if !self.sync.config_requested {
            self.sync.config_requested = true;
            let git = self.git.clone();
            jobs::spawn(ctx, &self.sync.tx, move || {
                let value = git.run(&["config", "--get", "pull.rebase"]).ok();
                SyncReply::PullConfig(value.map(|v| String::from_utf8_lossy(&v).trim().to_string()))
            });
        }
        let interval = settings.general.fetch_interval_min;
        let conditions = net::FetchConditions {
            interval_min: interval,
            bound_remote: self.sync.bound_remote.as_deref(),
            now,
            online: true,
            connected: true,
            in_progress: self.sync.running.is_some(),
            battery: None,
            skip_on_low_battery: settings.general.skip_fetch_on_low_battery,
        };
        if !self.initial_loading
            && let Ok(target) = self.sync.background.due(&conditions)
        {
            self.start_job(ctx, Job::Fetch { target, prune: true, background: true });
        }
        if self.sync.bound_remote.is_some()
            && let Some(wait) = plan::background_fetch_wait(self.sync.background.last_started, interval, now)
        {
            ctx.request_repaint_after(std::time::Duration::from_secs(wait.max(1)));
        }
        std::mem::take(&mut self.sync.outbox)
    }

    /// The status bar's sync part (§5.10).
    pub fn sync_status(&self, now: u64) -> SyncStatus {
        SyncStatus {
            syncing: self.sync.running.is_some(),
            last_fetch: self.sync.last_fetch.map(|at| plan::last_fetch_label(now, at)),
            fetch_failed: self.sync.background.status_label(now),
        }
    }

    /// "Fetch failed · 3m" clicked: the failure with its stderr.
    pub fn fetch_error_toast(&self) -> Option<Toast> {
        let (_, err) = self.sync.background.last_failure.as_ref()?;
        Some(net_toast(format!("Background fetch failed: {}", err.message()), err))
    }

    /// Applies `reply`, flagging a prompt or confirmation it raised for the app's attention.
    fn receive_sync_reply(&mut self, ctx: &egui::Context, reply: SyncReply, now: u64) {
        let (prompt, had_confirm) = (self.sync.prompt.clone(), self.sync.confirm.is_some());
        self.apply_sync_reply(ctx, reply, now);
        let new_prompt = self.sync.prompt.is_some() && self.sync.prompt != prompt;
        if new_prompt || (!had_confirm && self.sync.confirm.is_some()) {
            self.sync.attention = true;
        }
    }

    fn apply_sync_reply(&mut self, ctx: &egui::Context, reply: SyncReply, now: u64) {
        match reply {
            SyncReply::Progress(p) => {
                if let Some(running) = &mut self.sync.running {
                    running.progress = Some(p);
                }
            }
            SyncReply::PullConfig(value) => self.sync.pull_rebase = Some(value),
            SyncReply::Fetched { target, background, result } => {
                self.sync.running = None;
                if result.is_ok() {
                    self.sync.last_fetch = Some(now);
                    self.refresh_now(ctx);
                }
                let notice = if self.fetches_bound(&target) {
                    self.sync.background.finished(now, background, &result)
                } else if result.is_err() {
                    Notice::Toast
                } else {
                    Notice::None
                };
                if let (Notice::Toast, Err(err)) = (notice, &result)
                    && err.kind != NetErrorKind::Cancelled
                {
                    let title = format!("Fetch failed: {}", err.message());
                    self.notify(net_toast(title, err));
                }
            }
            SyncReply::Pulled { mode, result } => {
                self.sync.running = None;
                self.sync.config_requested = false; // re-read pull.rebase now and then
                self.apply_pulled(ctx, mode, result);
            }
            SyncReply::Pushed { push, result } => {
                self.sync.running = None;
                match result {
                    Ok(entry) => {
                        self.record_journal(entry);
                        self.notify(Toast::success(plan::pushed_message(&push)));
                        self.refresh_now(ctx);
                    }
                    Err(err) if err.kind == NetErrorKind::Cancelled => {}
                    Err(err) => match plan::push_failure_prompt(&err.kind, &push, &self.sync.protected) {
                        Some(prompt) => self.sync.prompt = Some(prompt),
                        None => self.notify(net_toast(err.message(), &err)),
                    },
                }
            }
            SyncReply::UpstreamSet { branch, upstream, retry, result } => match result {
                Ok(executed) => {
                    if let Some(entry) = executed.entry {
                        self.record_journal(entry);
                    }
                    self.notify(Toast::success(format!("`{branch}` now tracks `{upstream}`")));
                    self.refresh_now(ctx);
                    if let Some(mode) = retry {
                        let dirty = self
                            .status
                            .entries
                            .iter()
                            .any(|e| !matches!(e.kind, EntryKind::Untracked | EntryKind::Ignored));
                        let pull = plan::plan_pull(Some(&branch), Some(&upstream), mode, dirty);
                        self.run_pull_plan(ctx, pull);
                    }
                }
                Err(OpError::Git(e)) => self.notify(git_toast(&e)),
                Err(e) => self.notify(Toast::error(format!("Could not set upstream: {e}"), String::new())),
            },
            SyncReply::ForceLost { push, lost } => {
                let (remote, branch) = match &push {
                    Push::Branch { remote, branch, .. } => (remote.clone(), branch.clone()),
                    _ => return,
                };
                let detail = if lost.is_empty() {
                    format!(
                        "Plain --force overwrites `{branch}` on {remote} even if someone pushed since your last fetch. Force push with lease is the safe choice."
                    )
                } else {
                    let n = lost.len();
                    format!(
                        "{n} commit{} on {remote}/{branch} {} not in your branch and will be lost:",
                        if n == 1 { "" } else { "s" },
                        if n == 1 { "is" } else { "are" }
                    )
                };
                let confirm =
                    Confirm::new(ConfirmKind::ForcePush, format!("Force push `{branch}` to {remote}?"))
                        .detail(detail)
                        .lost(lost);
                self.sync.confirm = Some((confirm, push));
            }
        }
    }

    fn apply_pulled(&mut self, ctx: &egui::Context, mode: PullMode, result: Result<Pulled, NetError>) {
        match result {
            Ok(pulled) => {
                match pulled.entry {
                    Some(entry) => {
                        let branch =
                            entry.description.strip_prefix("pull ").unwrap_or(&entry.description).to_string();
                        self.record_journal(entry);
                        self.notify(Toast::success(format!("Pulled into `{branch}`")));
                    }
                    None => self.notify(Toast::info("Already up to date")),
                }
                if pulled.autostash_conflicts {
                    self.notify(Toast::warning(
                        "Pulled, but restoring your stashed changes hit conflicts",
                        "Resolve them in Changes; git keeps the stash until they are resolved.",
                    ));
                }
                self.refresh_now(ctx);
                if let Some(push) = self.sync.then_push.take() {
                    self.start_job(ctx, Job::Push(push));
                }
            }
            Err(err) => {
                self.sync.then_push = None;
                match err.kind {
                    NetErrorKind::Cancelled => {}
                    NetErrorKind::Conflicts => {
                        self.notify(net_toast(err.message(), &err));
                        self.refresh_now(ctx);
                    }
                    _ => match plan::pull_failure_prompt(&err.kind, mode) {
                        Some(prompt) => self.sync.prompt = Some(prompt),
                        None => {
                            self.notify(net_toast(err.message(), &err));
                            self.refresh_now(ctx);
                        }
                    },
                }
            }
        }
    }
}

// ---- drawing ---------------------------------------------------------------------------------

impl GitPane {
    /// The header toolbar, progress pill, action toast, confirmation, and popovers.
    pub(super) fn show_sync_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        preset: Preset,
        events: &mut Vec<GitEvent>,
    ) {
        let ctx = ui.ctx().clone();
        let remotes = self.remote_names();
        let default_remote = self.default_remote();
        let pull_default = self.default_pull_mode();
        let busy = self.sync.running.is_some();
        let mut request = None;
        let mut cancel = false;
        let bar = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let fetch_hint =
                default_remote.as_deref().map_or("No remote".to_string(), |r| format!("Fetch {r}"));
            if ui.add_enabled(!busy, egui::Button::new("⟳ Fetch")).on_hover_text(fetch_hint).clicked() {
                request = Some(SyncRequest::Fetch { remote: None, all: false, prune: true });
            }
            ui.add_enabled_ui(!busy, |ui| {
                ui.menu_button("▾", |ui| {
                    if ui.button("Fetch all").clicked() {
                        request = Some(SyncRequest::Fetch { remote: None, all: true, prune: true });
                    }
                    ui.menu_button("Fetch…", |ui| {
                        for r in &remotes {
                            if ui.button(r).clicked() {
                                request = Some(SyncRequest::Fetch {
                                    remote: Some(r.clone()),
                                    all: false,
                                    prune: true,
                                });
                            }
                        }
                    });
                });
            });
            ui.add_space(8.0);

            let pull = ui
                .add_enabled(!busy, egui::Button::new("Pull"))
                .on_hover_text(plan::pull_mode_label(pull_default));
            if pull.clicked() {
                request = Some(SyncRequest::Pull { mode: None });
            }
            ui.add_enabled_ui(!busy, |ui| {
                ui.menu_button("▾", |ui| {
                    for mode in [PullMode::Merge, PullMode::Rebase, PullMode::FastForwardOnly] {
                        let label = plan::pull_mode_label(mode);
                        let button = egui::Button::new(label).selected(mode == pull_default);
                        if ui.add(button).clicked() {
                            request = Some(SyncRequest::Pull { mode: Some(mode) });
                        }
                    }
                });
            });
            ui.add_space(8.0);

            let push = ui.add_enabled(!busy, egui::Button::new(plan::push_label(default_remote.as_deref())));
            if push.clicked() {
                request = Some(SyncRequest::Push { remote: None, branch: None, force: Force::No });
            }
            ui.add_enabled_ui(!busy, |ui| {
                ui.menu_button("▾", |ui| {
                    ui.menu_button("Push to…", |ui| {
                        for r in &remotes {
                            if ui.button(r).clicked() {
                                request = Some(SyncRequest::Push {
                                    remote: Some(r.clone()),
                                    branch: None,
                                    force: Force::No,
                                });
                            }
                        }
                    });
                    if ui.button("Force push (with lease)").clicked() {
                        request =
                            Some(SyncRequest::Push { remote: None, branch: None, force: Force::WithLease });
                    }
                    // Plain --force only while Alt is held (§5.10), styled danger, always confirmed.
                    if ui.input(|i| i.modifiers.alt) {
                        let text =
                            egui::RichText::new("Force push (--force)…").color(colors.get(Token::Danger));
                        if ui.button(text).clicked() {
                            request =
                                Some(SyncRequest::Push { remote: None, branch: None, force: Force::Plain });
                        }
                    }
                });
            });
        });
        self.sync.anchor = bar.response.rect.left_bottom() + egui::vec2(0.0, 4.0);
        // Its own row, so a long phase never widens the pane.
        if let Some(running) = &self.sync.running {
            let progress = match &running.progress {
                Some(p) => dialogs::Progress { phase: p.phase.clone(), percent: p.percent },
                None => dialogs::Progress { phase: running.label.clone(), percent: None },
            };
            ui.horizontal(|ui| cancel = dialogs::progress_pill(ui, colors, &progress, true));
        }
        if cancel && let Some(running) = &self.sync.running {
            running.cancel.store(true, Ordering::Relaxed);
        }

        self.show_sync_prompt(ui, colors, &ctx);
        self.show_sync_confirm(&ctx, colors, preset);
        self.show_sync_form(&ctx, colors);
        if let Some(request) = request {
            self.sync(&ctx, request);
        }
        events.append(&mut self.sync.outbox);
    }

    /// The action toast under the toolbar.
    fn show_sync_prompt(&mut self, ui: &mut egui::Ui, colors: &Colors, ctx: &egui::Context) {
        let Some(prompt) = self.sync.prompt.clone() else { return };
        let mut chosen = None;
        egui::Frame::default()
            .fill(colors.get(Token::BgRaised))
            .stroke(egui::Stroke::new(1.0, colors.get(Token::Border)))
            .corner_radius(6)
            .inner_margin(8)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let token = if matches!(prompt, Prompt::Rejected { .. }) {
                        Token::Danger
                    } else {
                        Token::Warning
                    };
                    ui.colored_label(colors.get(token), "⚠");
                    ui.label(prompt.title());
                });
                ui.horizontal_wrapped(|ui| {
                    for (label, choice) in prompt.choices() {
                        if ui.small_button(label).clicked() {
                            chosen = Some(choice);
                        }
                    }
                });
            });
        if let Some(choice) = chosen {
            self.sync.prompt = None;
            self.on_prompt_choice(ctx, prompt, choice);
        }
    }

    fn on_prompt_choice(&mut self, ctx: &egui::Context, prompt: Prompt, choice: Choice) {
        match (prompt, choice) {
            (Prompt::SetUpstream { branch }, Choice::SetUpstream) => {
                let remote = self.default_remote().unwrap_or_else(|| "origin".to_string());
                let value = format!("{remote}/{branch}");
                let retry = Some(self.default_pull_mode());
                self.sync.form = Some(SyncForm::SetUpstream { branch, value, retry });
            }
            (Prompt::PushNewBranch(push), Choice::Push) => {
                self.mark_asked(&push);
                self.start_job(ctx, Job::Push(push));
            }
            (Prompt::PushNewBranch(push), Choice::ChooseRemote) => {
                self.mark_asked(&push);
                let remotes = self.remote_names();
                let current = match &push {
                    Push::Branch { remote, .. } => remotes.iter().position(|r| r == remote).unwrap_or(0),
                    _ => 0,
                };
                self.sync.form = Some(SyncForm::ChooseRemote { push, choice: current });
            }
            (Prompt::Rejected { push, .. }, Choice::PullThenPush) => {
                self.sync.then_push = Some(push);
                self.request_pull(ctx, None);
            }
            (Prompt::Rejected { push, lease_allowed: true }, Choice::ForceWithLease) => {
                self.start_job(ctx, Job::Push(plan::with_lease(&push)));
            }
            (Prompt::StashPull(mode), Choice::StashPullPop) => {
                self.start_job(ctx, Job::Pull { mode, autostash: true });
            }
            _ => self.sync.then_push = None,
        }
    }

    fn mark_asked(&mut self, push: &Push) {
        if let Push::Branch { branch, .. } = push {
            self.sync.asks.mark(branch);
        }
    }

    fn show_sync_confirm(&mut self, ctx: &egui::Context, colors: &Colors, preset: Preset) {
        let Some((confirm, _)) = &mut self.sync.confirm else { return };
        match dialogs::confirm_dialog(ctx, colors, preset, confirm) {
            Some(Decision::Confirm) => {
                if let Some((_, push)) = self.sync.confirm.take() {
                    self.start_job(ctx, Job::Push(push));
                }
            }
            Some(Decision::Cancel) => self.sync.confirm = None,
            None => {}
        }
    }

    fn show_sync_form(&mut self, ctx: &egui::Context, colors: &Colors) {
        let anchor = self.sync.anchor;
        let remote_refs: Vec<String> =
            self.refs_list.iter().filter(|r| r.kind == RefKind::Remote).map(|r| r.short.clone()).collect();
        let remotes = self.remote_names();
        let Some(form) = &mut self.sync.form else { return };
        match form {
            SyncForm::SetUpstream { branch, value, retry } => {
                let error = upstream_error(value, &remote_refs);
                let title = format!("Set upstream for `{branch}`");
                let outcome = Popover::new("sync-set-upstream", title).primary("Set upstream").show(
                    ctx,
                    colors,
                    anchor,
                    |f| {
                        f.text(value, "origin/branch", error.as_deref());
                        if let Some(mode) = retry {
                            f.note(&format!("Then {}", plan::pull_mode_label(*mode).to_lowercase()));
                        }
                    },
                );
                match outcome {
                    FormOutcome::Submitted => {
                        let (branch, upstream, retry) = (branch.clone(), value.trim().to_string(), *retry);
                        self.sync.form = None;
                        self.set_upstream(ctx, branch, upstream, retry);
                    }
                    FormOutcome::Cancelled => self.sync.form = None,
                    FormOutcome::Open => {}
                }
            }
            SyncForm::ChooseRemote { push, choice } => {
                let options: Vec<(usize, &str)> = remotes.iter().map(String::as_str).enumerate().collect();
                let title = match &*push {
                    Push::Branch { branch, .. } => format!("Push `{branch}` and set upstream"),
                    other => other.description(),
                };
                let outcome = Popover::new("sync-choose-remote", title).primary("Push").show(
                    ctx,
                    colors,
                    anchor,
                    |f| {
                        f.dropdown("Remote", choice, &options);
                    },
                );
                match outcome {
                    FormOutcome::Submitted => {
                        let push = match (push.clone(), remotes.get(*choice)) {
                            (Push::Branch { branch, force, .. }, Some(remote)) => {
                                Push::Branch { remote: remote.clone(), branch, set_upstream: true, force }
                            }
                            (push, _) => push,
                        };
                        self.sync.form = None;
                        self.start_job(ctx, Job::Push(push));
                    }
                    FormOutcome::Cancelled => self.sync.form = None,
                    FormOutcome::Open => {}
                }
            }
        }
    }
}

/// The **Set upstream…** field's live validation: an existing remote-tracking branch.
fn upstream_error(value: &str, remote_refs: &[String]) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        Some("Name a remote branch, like origin/main".to_string())
    } else if !remote_refs.iter().any(|r| r == value) {
        Some(format!("No remote branch `{value}`; fetch first?"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_must_name_a_known_remote_branch() {
        let refs = vec!["origin/main".to_string(), "origin/feat".to_string()];
        assert_eq!(upstream_error(" origin/feat ", &refs), None);
        assert!(upstream_error("", &refs).is_some());
        assert_eq!(
            upstream_error("origin/nope", &refs).as_deref(),
            Some("No remote branch `origin/nope`; fetch first?")
        );
    }

    #[test]
    fn fetch_without_prune_drops_only_that_flag() {
        let repo = crate::testutil::TempRepo::new();
        let git = Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });
        let cancel = AtomicBool::new(false);
        // No remote named `nope`: git fails, and the failure carries the argv it ran.
        let err = fetch(&git, &Fetch::Remote("nope".into()), false, &cancel, &mut |_| {}).unwrap_err();
        let command = err.git.map(|g| g.command).unwrap_or_default();
        assert!(command.contains("fetch nope --progress"), "{command}");
        assert!(!command.contains("--prune"), "{command}");
    }

    #[test]
    fn a_request_while_busy_is_refused_with_a_toast() {
        let mut pane = super::super::tests::new_test_pane();
        pane.sync.running =
            Some(Running { label: "Fetching origin".into(), progress: None, cancel: Arc::default() });
        pane.sync(&egui::Context::default(), SyncRequest::PushAllTags { remote: "origin".into() });
        let toasts: Vec<_> = std::mem::take(&mut pane.sync.outbox);
        assert!(
            matches!(&toasts[..], [GitEvent::Toast(t)] if t.title.contains("Fetching origin")),
            "{toasts:?}"
        );
    }

    #[test]
    fn protected_force_push_is_refused_before_anything_runs() {
        let mut pane = super::super::tests::new_test_pane();
        pane.status.branch.head = Some("main".into());
        pane.status.branch.upstream = Some("origin/main".into());
        pane.sync.bound_remote = Some("origin".into());
        let ctx = egui::Context::default();
        pane.sync(&ctx, SyncRequest::Push { remote: None, branch: None, force: Force::Plain });
        assert!(pane.sync.running.is_none() && pane.sync.confirm.is_none());
        let toasts = std::mem::take(&mut pane.sync.outbox);
        assert!(matches!(&toasts[..], [GitEvent::Toast(t)] if t.title.contains("protected")), "{toasts:?}");
    }

    #[test]
    fn deleting_a_remote_tag_confirms_first() {
        let mut pane = super::super::tests::new_test_pane();
        let ctx = egui::Context::default();
        pane.sync(
            &ctx,
            SyncRequest::DeleteRemote { remote: "origin".into(), refname: "refs/tags/v1".into() },
        );
        let (confirm, push) = pane.sync.confirm.clone().expect("confirmation");
        assert_eq!(confirm.kind, ConfirmKind::DeleteRemoteTag);
        assert_eq!(push, Push::Delete { remote: "origin".into(), refname: "refs/tags/v1".into() });
        assert!(pane.sync.running.is_none());
    }

    #[test]
    fn first_push_of_a_new_branch_asks_once() {
        let mut pane = super::super::tests::new_test_pane();
        pane.status.branch.head = Some("feat".into());
        pane.sync.bound_remote = Some("origin".into());
        let ctx = egui::Context::default();
        pane.sync(&ctx, SyncRequest::Push { remote: None, branch: None, force: Force::No });
        let Some(Prompt::PushNewBranch(push)) = pane.sync.prompt.take() else { panic!("no prompt") };
        pane.mark_asked(&push);
        assert!(!pane.sync.asks.should_ask(Some("feat"), false));
    }

    // ---- end to end against a bare "remote" -----------------------------------------------------

    fn sh(dir: &std::path::Path, args: &[&str]) {
        let out = crate::testutil::hermetic_git(dir).args(args).output().expect("spawn git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// Apply replies until `done` holds (the worker threads send them).
    fn pump(pane: &mut GitPane, ctx: &egui::Context, done: impl Fn(&GitPane) -> bool) {
        while !done(pane) {
            let reply = pane.sync.rx.recv_timeout(std::time::Duration::from_secs(60)).expect("a reply");
            pane.receive_sync_reply(ctx, reply, crate::util::unix_now());
        }
    }

    #[test]
    fn rejected_push_then_pull_then_push_journals_both() {
        let mut seed = crate::testutil::TempRepo::new();
        seed.commit_file("a.txt", "1\n", "first");
        let dir = tempfile::tempdir().expect("tempdir");
        sh(dir.path(), &["clone", "-q", "--bare", &seed.path().display().to_string(), "remote.git"]);
        for name in ["work", "other"] {
            sh(dir.path(), &["clone", "-q", "remote.git", name]);
            // `Git::run` doesn't get hermetic_git's identity: give the clone its own.
            for (k, v) in
                [("user.name", "Ada"), ("user.email", "ada@example.com"), ("commit.gpgsign", "false")]
            {
                sh(&dir.path().join(name), &["config", k, v]);
            }
        }
        for (name, file) in [("other", "b.txt"), ("work", "c.txt")] {
            let path = dir.path().join(name);
            std::fs::write(path.join(file), "x\n").expect("write");
            sh(&path, &["add", "-A"]);
            sh(&path, &["commit", "-q", "-m", file]);
        }
        sh(&dir.path().join("other"), &["push", "-q", "origin", "HEAD"]);

        let mut pane = super::super::tests::new_test_pane();
        pane.git = Git::new(crate::git::Location::Local { path: dir.path().join("work") });
        pane.journal_path = dir.path().join("journal.json");
        pane.status.branch.head = Some("main".into());
        pane.status.branch.upstream = Some("origin/main".into());
        pane.remotes = vec![crate::git::refs::Remote {
            name: "origin".into(),
            fetch_url: String::new(),
            push_url: String::new(),
        }];
        pane.sync.bound_remote = Some("origin".into());
        let ctx = egui::Context::default();

        pane.sync(&ctx, SyncRequest::Push { remote: None, branch: None, force: Force::No });
        pump(&mut pane, &ctx, |p| p.sync.running.is_none());
        assert_eq!(
            pane.take_sync_attention().as_deref(),
            Some("Push rejected: remote has commits you don't have"),
            "a rejected push asks the app to open the pane, even a closed one"
        );
        assert_eq!(pane.take_sync_attention(), None, "once per prompt");
        let prompt = pane.sync.prompt.take().expect("rejected toast");
        assert!(matches!(prompt, Prompt::Rejected { .. }), "{prompt:?}");

        pane.on_prompt_choice(&ctx, prompt, Choice::PullThenPush);
        pump(&mut pane, &ctx, |p| p.sync.running.is_none() && p.sync.then_push.is_none());
        pump(&mut pane, &ctx, |p| p.sync.running.is_none());
        let toasts: Vec<String> = pane
            .sync
            .outbox
            .iter()
            .filter_map(|e| match e {
                GitEvent::Toast(t) => Some(t.title.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            toasts,
            ["Pulled into `main`", "Pushed `main` to origin · Undo is not available for push"]
        );

        let pushed = pane.journal.peek_undo().expect("push entry");
        assert!(pushed.inverse.is_none(), "push has no inverse");
        let saved = crate::git::journal::Journal::load(&pane.journal_path).expect("journal saved");
        let entries: Vec<(&str, bool)> =
            saved.entries().map(|e| (e.description.as_str(), e.inverse.is_some())).collect();
        assert_eq!(entries, [("push origin main", false), ("pull main", true)], "newest first");
    }
}
