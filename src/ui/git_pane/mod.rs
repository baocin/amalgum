//! The git pane (§3, §5.4–5.21) bound to one workspace location: a segmented control with
//! **Graph** (lanes + rows, details and diff of the selected commit below), **Changes** (Unstaged /
//! Staged lists with checkboxes, the commit editor, the diff of the focused file) and **Refs**
//! (branches, remotes, tags, stashes, worktrees).
//!
//! All git runs on worker threads via `ui::jobs` into this pane's own channel; `show` drains it
//! each frame, so the pane is self-contained and never blocks. Local repos refresh from a
//! `notify` watcher on `.git/` and the work tree (debounced 200 ms, §5.25); remote repos refresh
//! on `refresh()` calls and every `Settings::git.remote_refresh_secs` while visible.
//!
//! Ref-changing operations (commit, checkout, …) record a `git::journal` entry, so `undo`/`redo`
//! work across restarts (§5.18).
//!
//! Module layout: [`worker`] is the `Reply` enum and every pure helper around it (pagination,
//! generation guard, journal-entry builders); [`graph`] the Graph view; [`details`] the commit
//! details and diff below it; [`changes`] the Changes view (staging + commit editor); [`refs`]
//! the Refs view.

mod changes;
mod details;
mod graph;
mod refs;
mod worker;

use super::chrome::Toast;
use super::jobs;
use super::theme::Colors;
use crate::git::cmd::{Git, GitError, Location};
use crate::git::journal::{Failed, Journal};
use crate::git::log::Commit;
use crate::git::refs::{
    FOR_EACH_REF_ARGS, REMOTE_ARGS, Ref, Remote, STASH_ARGS, Stash, WORKTREE_ARGS, Worktree,
};
use crate::git::status::{RepoOp, STATUS_ARGS, Status};
use crate::git::{graph as gitgraph, log as gitlog, refs as gitrefs, status as gitstatus};
use crate::model::settings::Settings;
use crate::model::state::GitView;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use worker::Reply;

/// Enough repo state for the status bar and the sidebar row (W2, W3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoSummary {
    pub branch: Option<String>,
    /// Short hash when detached.
    pub detached: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changed: usize,
    pub op: Option<RepoOp>,
    pub conflicts: usize,
    pub loading: bool,
}

/// Things the pane asks the app to do.
#[derive(Debug, Clone, PartialEq)]
pub enum GitEvent {
    Toast(Toast),
    /// Type text into the focused terminal without Enter (§5.6 "Send path:line to terminal").
    SendToTerminal(String),
    /// The pane's summary changed (branch, counts, operation) — re-render sidebar/status bar.
    SummaryChanged,
}

/// The graph's current selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Selection {
    None,
    Commit(String),
}

/// A background watcher's debounce state, or the poll fallback (§5.25).
enum Live {
    Watcher { _watcher: notify::RecommendedWatcher, dirty: Arc<AtomicBool>, debounce_until: Option<f64> },
    Poll { last_poll: f64 },
}

pub struct GitPane {
    /// A clone of the app's `egui::Context`, refreshed on every `show()` call. `refresh`,
    /// `undo`, and `redo` keep the fixed contract signature (no `ctx` parameter) but still need
    /// one to spawn worker jobs and wake the UI when a reply lands; egui's `Context` is a cheap
    /// `Clone` handle for exactly this reason.
    ctx: egui::Context,
    location: Location,
    git: Git,
    journal_path: PathBuf,
    journal: Journal,
    view: GitView,

    // -- latest known repo state, refreshed as replies arrive --
    status: Status,
    refs_list: Vec<Ref>,
    ref_targets: BTreeMap<String, String>,
    refs_seen: bool,
    stashes: Vec<Stash>,
    worktrees: Vec<Worktree>,
    remotes: Vec<Remote>,
    op: Option<RepoOp>,
    git_dir: Option<PathBuf>,

    // -- graph --
    pub(super) layout: gitgraph::Layout,
    pub(super) commits: Vec<Commit>,
    pub(super) rows: Vec<gitgraph::Row>,
    pub(super) log_loaded: usize,
    pub(super) log_has_more: bool,
    pub(super) log_loading: bool,
    pub(super) selected: Selection,

    // -- details (below the graph) --
    details: Option<details::DetailsState>,

    // -- changes view --
    changes: changes::ChangesState,

    // -- worker plumbing --
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    generation: u64,
    next_req: u64,
    log_refetch_dispatched_gen: u64,
    undo_busy: bool,

    live: Live,
    last_summary: RepoSummary,
    initial_loading: bool,
}

impl GitPane {
    /// Start loading status, refs, and the first 500 commits in the background.
    pub fn new(location: Location, journal_path: PathBuf, ctx: &egui::Context) -> Self {
        let git = Git::new(location.clone());
        let journal = Journal::load(&journal_path).unwrap_or_default();
        let (tx, rx) = channel();

        let live = match &location {
            Location::Local { path } => match setup_watcher(path, ctx) {
                Some(w) => w,
                None => Live::Poll { last_poll: 0.0 },
            },
            Location::Remote { .. } => Live::Poll { last_poll: 0.0 },
        };

        let mut pane = Self {
            ctx: ctx.clone(),
            location,
            git,
            journal_path,
            journal,
            view: GitView::Graph,
            status: Status::default(),
            refs_list: Vec::new(),
            ref_targets: BTreeMap::new(),
            refs_seen: false,
            stashes: Vec::new(),
            worktrees: Vec::new(),
            remotes: Vec::new(),
            op: None,
            git_dir: None,
            layout: gitgraph::Layout::default(),
            commits: Vec::new(),
            rows: Vec::new(),
            log_loaded: 0,
            log_has_more: true,
            log_loading: false,
            selected: Selection::None,
            details: None,
            changes: changes::ChangesState::default(),
            tx,
            rx,
            generation: 1,
            next_req: 1,
            log_refetch_dispatched_gen: 0,
            undo_busy: false,
            live,
            last_summary: RepoSummary::default(),
            initial_loading: true,
        };
        pane.dispatch_bootstrap(ctx);
        pane
    }

    pub fn set_view(&mut self, view: GitView) {
        self.view = view;
    }

    /// Re-read status, refs, and the head of the log (after hook events, focus, fetch).
    pub fn refresh(&mut self) {
        self.generation += 1;
        let ctx = self.ctx.clone();
        self.dispatch_refresh_jobs(&ctx);
    }

    pub fn summary(&self) -> RepoSummary {
        self.compute_summary()
    }

    /// `Mod+Z` / `Mod+Shift+Z` in the git pane (§5.18). Refusals become toasts.
    pub fn undo(&mut self) {
        let ctx = self.ctx.clone();
        self.run_undo_redo(&ctx, true);
    }

    pub fn redo(&mut self) {
        let ctx = self.ctx.clone();
        self.run_undo_redo(&ctx, false);
    }

    /// Draw the pane; returns requests for the app.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        settings: &Settings,
        now: u64,
    ) -> Vec<GitEvent> {
        self.ctx = ui.ctx().clone();
        let ctx = self.ctx.clone();
        let frame_time = ctx.input(|i| i.time);
        let mut events = self.drain_replies(&ctx);

        self.poll_watch(&ctx, frame_time);

        ui.horizontal(|ui| {
            for (label, v) in
                [("Graph", GitView::Graph), ("Changes", GitView::Changes), ("Refs", GitView::Refs)]
            {
                let text = if v == GitView::Changes && self.status.changed_count() > 0 {
                    format!("{label} ({})", self.status.changed_count())
                } else {
                    label.to_string()
                };
                if ui.selectable_label(self.view == v, text).clicked() {
                    self.view = v;
                }
            }
        });
        ui.separator();

        match self.view {
            GitView::Graph => self.show_graph(ui, colors, settings, now, &mut events, &ctx),
            GitView::Changes => self.show_changes(ui, colors, settings, now, &mut events, &ctx),
            GitView::Refs => self.show_refs(ui, colors, now, &mut events),
        }

        let summary = self.compute_summary();
        if summary != self.last_summary {
            self.last_summary = summary;
            events.push(GitEvent::SummaryChanged);
        }
        events
    }
}

// -- construction / bootstrap ------------------------------------------------------------------

impl GitPane {
    /// `new()` only: the first load, including the graph's first page. `refresh()` uses
    /// [`Self::dispatch_refresh_jobs`] instead, which leaves the log alone unless a Status/Refs
    /// reply detects that HEAD or the refs actually moved (`maybe_refetch_log`) — otherwise every
    /// refresh would reset an already-paginated graph back to its first 500 rows.
    fn dispatch_bootstrap(&mut self, ctx: &egui::Context) {
        self.dispatch_refresh_jobs(ctx);
        self.dispatch_log_page(ctx, worker::next_log_n(0));
    }

    fn dispatch_refresh_jobs(&mut self, ctx: &egui::Context) {
        self.dispatch_status(ctx);
        self.dispatch_refs(ctx);
        self.dispatch_stashes(ctx);
        self.dispatch_worktrees(ctx);
        self.dispatch_remotes(ctx);
        self.dispatch_op(ctx);
    }

    fn dispatch_status(&self, ctx: &egui::Context) {
        let page_gen = self.generation;
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let result = git.run(STATUS_ARGS).and_then(|out| {
                gitstatus::parse(&out).map_err(|e| GitError {
                    command: "git status".into(),
                    code: None,
                    stderr: e,
                })
            });
            Reply::Status { page_gen, result }
        });
    }

    fn dispatch_refs(&self, ctx: &egui::Context) {
        let page_gen = self.generation;
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let result =
                git.run(FOR_EACH_REF_ARGS).map(|out| gitrefs::parse_refs(&String::from_utf8_lossy(&out)));
            Reply::Refs { page_gen, result }
        });
    }

    fn dispatch_stashes(&self, ctx: &egui::Context) {
        let page_gen = self.generation;
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || Reply::Stashes {
            page_gen,
            result: git.run(STASH_ARGS).map(|o| gitrefs::parse_stashes(&o)),
        });
    }

    fn dispatch_worktrees(&self, ctx: &egui::Context) {
        let page_gen = self.generation;
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || Reply::Worktrees {
            page_gen,
            result: git.run(WORKTREE_ARGS).map(|o| gitrefs::parse_worktrees(&o)),
        });
    }

    fn dispatch_remotes(&self, ctx: &egui::Context) {
        let page_gen = self.generation;
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let result = git.run(REMOTE_ARGS).map(|o| gitrefs::parse_remotes(&String::from_utf8_lossy(&o)));
            Reply::Remotes { page_gen, result }
        });
    }

    /// Detects an in-progress operation (§5.17). Only implemented for local repositories: a
    /// remote repo's git-dir cannot be probed for file existence from this machine without an
    /// extra ssh round trip per file, which this milestone does not add.
    fn dispatch_op(&self, ctx: &egui::Context) {
        let page_gen = self.generation;
        let git = self.git.clone();
        let is_local = matches!(self.location, Location::Local { .. });
        jobs::spawn(ctx, &self.tx, move || {
            if !is_local {
                return Reply::Op { page_gen, git_dir: None, op: None };
            }
            let Ok(out) = git.run(&["rev-parse", "--absolute-git-dir"]) else {
                return Reply::Op { page_gen, git_dir: None, op: None };
            };
            let dir = PathBuf::from(String::from_utf8_lossy(&out).trim());
            let op = gitstatus::detect_op(|p| dir.join(p).exists());
            Reply::Op { page_gen, git_dir: Some(dir), op }
        });
    }

    pub(super) fn dispatch_log_page(&mut self, ctx: &egui::Context, n: usize) {
        self.log_loading = true;
        let page_gen = self.generation;
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let args = gitlog::log_args(&["--all", "-n", &n.to_string()]);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let result = git.run(&args).map(|o| gitlog::parse_log(&o));
            Reply::LogPage { page_gen, requested: n, result }
        });
    }

    pub(super) fn next_req_id(&mut self) -> u64 {
        self.next_req += 1;
        self.next_req
    }
}

// -- reply draining --------------------------------------------------------------------------

impl GitPane {
    fn drain_replies(&mut self, ctx: &egui::Context) -> Vec<GitEvent> {
        let mut events = Vec::new();
        while let Ok(reply) = self.rx.try_recv() {
            self.apply_reply(ctx, reply, &mut events);
        }
        events
    }

    fn apply_reply(&mut self, ctx: &egui::Context, reply: Reply, events: &mut Vec<GitEvent>) {
        match reply {
            Reply::Status { page_gen, result } => {
                if !worker::gen_is_current(page_gen, self.generation) {
                    return;
                }
                match result {
                    Ok(status) => {
                        let was_loaded = !self.initial_loading;
                        let head_changed = was_loaded && status.branch.oid != self.status.branch.oid;
                        self.status = status;
                        self.initial_loading = false;
                        if head_changed {
                            self.maybe_refetch_log(ctx);
                        }
                    }
                    Err(e) => {
                        events.push(GitEvent::Toast(Toast::error("Could not read status", e.to_string())))
                    }
                }
            }
            Reply::Refs { page_gen, result } => {
                if !worker::gen_is_current(page_gen, self.generation) {
                    return;
                }
                match result {
                    Ok(refs) => {
                        let new_targets = target_map(&refs);
                        let changed = self.refs_seen && refs_changed(&self.ref_targets, &new_targets);
                        self.refs_list = refs;
                        self.ref_targets = new_targets;
                        self.refs_seen = true;
                        if changed {
                            self.maybe_refetch_log(ctx);
                        }
                    }
                    Err(e) => {
                        events.push(GitEvent::Toast(Toast::error("Could not read refs", e.to_string())))
                    }
                }
            }
            Reply::Stashes { page_gen, result } => {
                if worker::gen_is_current(page_gen, self.generation) {
                    match result {
                        Ok(s) => self.stashes = s,
                        Err(e) => events
                            .push(GitEvent::Toast(Toast::error("Could not read stashes", e.to_string()))),
                    }
                }
            }
            Reply::Worktrees { page_gen, result } => {
                if worker::gen_is_current(page_gen, self.generation) {
                    match result {
                        Ok(w) => self.worktrees = w,
                        Err(e) => events
                            .push(GitEvent::Toast(Toast::error("Could not read worktrees", e.to_string()))),
                    }
                }
            }
            Reply::Remotes { page_gen, result } => {
                if worker::gen_is_current(page_gen, self.generation) {
                    match result {
                        Ok(r) => self.remotes = r,
                        Err(e) => events
                            .push(GitEvent::Toast(Toast::error("Could not read remotes", e.to_string()))),
                    }
                }
            }
            Reply::Op { page_gen, git_dir, op } => {
                if worker::gen_is_current(page_gen, self.generation) {
                    self.git_dir = git_dir;
                    self.op = op;
                }
            }
            Reply::LogPage { page_gen, requested, result } => {
                self.log_loading = false;
                if !worker::gen_is_current(page_gen, self.generation) {
                    return;
                }
                match result {
                    Ok(commits) => self.apply_log_page(commits, requested),
                    Err(e) => {
                        events.push(GitEvent::Toast(Toast::error("Could not read history", e.to_string())))
                    }
                }
            }
            Reply::CommitBody { id, result } => self.apply_commit_body(id, result),
            Reply::FileList { id, result } => self.apply_file_list(id, result, events),
            Reply::DetailsDiff { req, result } => self.apply_details_diff(req, result, events),
            Reply::ChangesDiff { req, result } => self.apply_changes_diff(req, result, events),
            Reply::LastCommitMessage(result) => self.apply_last_commit_message(result, events),
            Reply::ActionDone { req, result } => self.apply_action_done(ctx, req, result, events),
            Reply::Committed(result) => self.apply_committed(ctx, result, events),
            Reply::Checkout { branch, result } => self.apply_checkout(ctx, branch, result, events),
            Reply::Undo(outcome) => self.apply_undo_redo(ctx, "undo", outcome, events),
            Reply::Redo(outcome) => self.apply_undo_redo(ctx, "redo", outcome, events),
        }
    }

    /// After a status or refs change that implies the log's head may have moved, refetch the
    /// currently loaded window fresh (at most once per generation) and rebuild the layout.
    fn maybe_refetch_log(&mut self, ctx: &egui::Context) {
        if self.log_refetch_dispatched_gen == self.generation {
            return;
        }
        self.log_refetch_dispatched_gen = self.generation;
        let n = self.log_loaded.max(worker::next_log_n(0));
        self.layout = gitgraph::Layout::default();
        self.commits.clear();
        self.rows.clear();
        self.log_loaded = 0;
        self.log_has_more = true;
        self.dispatch_log_page(ctx, n);
    }
}

// -- summary --------------------------------------------------------------------------------

impl GitPane {
    fn compute_summary(&self) -> RepoSummary {
        let branch = self.status.branch.head.clone();
        let detached = (branch.is_none() && self.status.branch.oid.is_some())
            .then(|| worker::short_hash(self.status.branch.oid.as_deref().unwrap_or_default()).to_string());
        let conflicts = self.status.entries.iter().filter(|e| e.is_conflicted()).count();
        RepoSummary {
            branch,
            detached,
            upstream: self.status.branch.upstream.clone(),
            ahead: self.status.branch.ahead,
            behind: self.status.branch.behind,
            changed: self.status.changed_count(),
            op: self.op,
            conflicts,
            loading: self.initial_loading,
        }
    }
}

// -- undo / redo ------------------------------------------------------------------------------

impl GitPane {
    fn run_undo_redo(&mut self, ctx: &egui::Context, is_undo: bool) {
        if self.undo_busy {
            return;
        }
        let peeked = if is_undo { self.journal.peek_undo() } else { self.journal.peek_redo() };
        let Some(entry) = peeked else {
            return; // nothing recorded yet; `journal.undo/redo` would say NothingToDo anyway
        };
        // What both snapshots depend on: the guard checks one, and after a failed run the journal
        // re-reads the repo to check whether it reached the other anyway.
        let mut touched: Vec<String> =
            entry.before.refs.keys().chain(entry.after.refs.keys()).cloned().collect();
        touched.sort();
        touched.dedup();
        let want_stashes = !entry.before.stashes.is_empty() || !entry.after.stashes.is_empty();
        self.undo_busy = true;
        let journal = std::mem::take(&mut self.journal);
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let snapshot = || worker::read_snapshot(&git, &touched, want_stashes);
            let current = snapshot();
            let mut journal = journal;
            let run = |plan: &crate::git::journal::Plan| worker::run_plan(&git, plan);
            let result = if is_undo {
                journal.undo(&current, run, snapshot)
            } else {
                journal.redo(&current, run, snapshot)
            };
            let outcome = worker::JournalRunOutcome { journal, result };
            if is_undo { Reply::Undo(outcome) } else { Reply::Redo(outcome) }
        });
    }

    fn apply_undo_redo(
        &mut self,
        ctx: &egui::Context,
        action: &str,
        outcome: worker::JournalRunOutcome,
        events: &mut Vec<GitEvent>,
    ) {
        self.undo_busy = false;
        self.journal = outcome.journal;
        let _ = self.journal.save(&self.journal_path);
        match outcome.result {
            Ok(()) => self.refresh_now(ctx),
            Err(Failed::Refused(refused)) => events.push(GitEvent::Toast(Toast::error(
                worker::refusal_message(action, &refused),
                String::new(),
            ))),
            Err(Failed::Run(e)) => {
                events.push(GitEvent::Toast(Toast::error(
                    format!("Can't {action}: {}", e.summary()),
                    e.stderr.clone(),
                )));
                self.refresh_now(ctx); // a plan of several commands may have stopped part-way
            }
            Err(Failed::AppliedWithError(e)) => {
                events.push(GitEvent::Toast(Toast::warning(
                    format!("The {action} took effect, but git reported: {}", e.summary()),
                    e.stderr.clone(),
                )));
                self.refresh_now(ctx);
            }
        }
    }

    /// `refresh()` plus immediately re-dispatching the jobs (the fixed `refresh()` signature has
    /// no `ctx`; callers that have one — including this pane after its own actions — use this).
    fn refresh_now(&mut self, ctx: &egui::Context) {
        self.generation += 1;
        self.dispatch_refresh_jobs(ctx);
    }
}

// -- checkout ---------------------------------------------------------------------------------

impl GitPane {
    /// Local branch names for the palette's Branches group (§5.19).
    pub fn local_branches(&self) -> Vec<String> {
        self.refs_list.iter().filter(|r| r.kind == gitrefs::RefKind::Local).map(|r| r.short.clone()).collect()
    }

    /// Checkout chosen from the palette (§5.9); journaled like a double-click in Refs.
    pub fn checkout(&mut self, ctx: &egui::Context, name: String) {
        self.start_checkout(ctx, name);
    }

    /// §5.9 "Checkout": `git checkout <target>`, journaled with an inverse that checks out the
    /// previous branch (or the previous detached hash).
    pub(super) fn start_checkout(&mut self, ctx: &egui::Context, target: String) {
        let git = self.git.clone();
        let now = crate::util::unix_now();
        jobs::spawn(ctx, &self.tx, move || {
            let (before_head, before_branch) = worker::head_and_branch(&git);
            let Some(before_head) = before_head else {
                return Reply::Checkout {
                    branch: target,
                    result: Err(GitError {
                        command: "git checkout".into(),
                        code: None,
                        stderr: "no HEAD yet".into(),
                    }),
                };
            };
            let result = git.run(&["checkout", &target]).map(|_| {
                let (after_head, _) = worker::head_and_branch(&git);
                let after_head = after_head.unwrap_or_else(|| before_head.clone());
                worker::CheckoutOutcome {
                    entry: worker::checkout_entry(now, before_head, before_branch, &target, after_head),
                }
            });
            Reply::Checkout { branch: target, result }
        });
    }

    fn apply_checkout(
        &mut self,
        ctx: &egui::Context,
        branch: String,
        result: Result<worker::CheckoutOutcome, GitError>,
        events: &mut Vec<GitEvent>,
    ) {
        match result {
            Ok(outcome) => {
                self.journal.record(outcome.entry);
                let _ = self.journal.save(&self.journal_path);
                events.push(GitEvent::Toast(Toast::success(format!("Checked out `{branch}`"))));
                self.refresh_now(ctx);
            }
            Err(e) => events.push(GitEvent::Toast(Toast::error(e.summary(), e.stderr.clone()))),
        }
    }
}

fn target_map(refs: &[Ref]) -> BTreeMap<String, String> {
    refs.iter().map(|r| (r.name.clone(), r.target.clone())).collect()
}

/// Whether the set of ref → target pairs changed (§7 "compare `for-each-ref` output").
fn refs_changed(old: &BTreeMap<String, String>, new: &BTreeMap<String, String>) -> bool {
    old != new
}

fn setup_watcher(path: &std::path::Path, ctx: &egui::Context) -> Option<Live> {
    use notify::{RecommendedWatcher, RecursiveMode, Watcher};
    let dirty = Arc::new(AtomicBool::new(false));
    let (flag, wake_ctx) = (Arc::clone(&dirty), ctx.clone());
    let mut watcher: RecommendedWatcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if event.paths.is_empty() || event.paths.iter().any(|p| !is_excluded(p)) {
                flag.store(true, Ordering::Relaxed);
                wake_ctx.request_repaint();
            }
        })
        .ok()?;
    let git_dir = path.join(".git");
    let watch_git = watcher.watch(&git_dir, RecursiveMode::Recursive).is_ok();
    let watch_tree = watcher.watch(path, RecursiveMode::Recursive).is_ok();
    if !watch_git && !watch_tree {
        return None;
    }
    Some(Live::Watcher { _watcher: watcher, dirty, debounce_until: None })
}

/// Paths under `.git/objects`, `target/`, or `node_modules/` are noise (packfile writes, build
/// output) and never warrant a refresh.
fn is_excluded(path: &std::path::Path) -> bool {
    let mut prev: Option<String> = None;
    for c in path.components() {
        let s = c.as_os_str().to_string_lossy();
        if s == "objects" && prev.as_deref() == Some(".git") {
            return true;
        }
        if s == "target" || s == "node_modules" {
            return true;
        }
        prev = Some(s.into_owned());
    }
    false
}

impl GitPane {
    fn poll_watch(&mut self, ctx: &egui::Context, frame_time: f64) {
        match &mut self.live {
            Live::Watcher { dirty, debounce_until, .. } => {
                if dirty.swap(false, Ordering::Relaxed) && debounce_until.is_none() {
                    *debounce_until = Some(frame_time + 0.2);
                }
                if let Some(t) = *debounce_until
                    && frame_time >= t
                {
                    if let Live::Watcher { debounce_until, .. } = &mut self.live {
                        *debounce_until = None;
                    }
                    self.refresh_now(ctx);
                }
            }
            Live::Poll { last_poll } => {
                if frame_time - *last_poll >= 2.0 {
                    *last_poll = frame_time;
                    self.dispatch_status(ctx);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- refs_changed / target_map -----------------------------------------------------------

    fn r(name: &str, target: &str) -> Ref {
        Ref {
            name: name.to_string(),
            short: name.to_string(),
            kind: crate::git::refs::RefKind::Local,
            target: target.to_string(),
            peeled: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            gone: false,
            is_head: false,
            time: 0,
            subject: String::new(),
        }
    }

    #[test]
    fn target_map_collects_name_to_target() {
        let refs = vec![r("refs/heads/main", "aaa"), r("refs/heads/feat", "bbb")];
        let m = target_map(&refs);
        assert_eq!(m.get("refs/heads/main").map(String::as_str), Some("aaa"));
        assert_eq!(m.get("refs/heads/feat").map(String::as_str), Some("bbb"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn refs_changed_detects_moved_added_removed() {
        let a = target_map(&[r("refs/heads/main", "aaa")]);
        let b = target_map(&[r("refs/heads/main", "bbb")]);
        assert!(refs_changed(&a, &b), "moved ref");

        let c = target_map(&[r("refs/heads/main", "aaa"), r("refs/heads/feat", "ccc")]);
        assert!(refs_changed(&a, &c), "added ref");

        let same_a = target_map(&[r("refs/heads/main", "aaa")]);
        assert!(!refs_changed(&a, &same_a), "identical sets are unchanged");
    }

    // ---- is_excluded --------------------------------------------------------------------------

    #[test]
    fn is_excluded_git_objects_and_build_dirs() {
        assert!(is_excluded(std::path::Path::new("/repo/.git/objects/aa/bb"))); // portability: allow
        assert!(is_excluded(std::path::Path::new("/repo/target/debug/foo"))); // portability: allow
        assert!(is_excluded(std::path::Path::new("/repo/node_modules/x"))); // portability: allow
        assert!(!is_excluded(std::path::Path::new("/repo/.git/HEAD"))); // portability: allow
        assert!(!is_excluded(std::path::Path::new("/repo/src/main.rs"))); // portability: allow
    }

    // ---- compute_summary ----------------------------------------------------------------------

    #[test]
    fn compute_summary_detached_shows_short_hash() {
        let mut pane = new_test_pane();
        pane.status.branch.head = None;
        pane.status.branch.oid = Some("abcdef1234567".to_string());
        let s = pane.compute_summary();
        assert_eq!(s.branch, None);
        assert_eq!(s.detached.as_deref(), Some("abcdef1"));
    }

    #[test]
    fn compute_summary_on_branch_has_no_detached() {
        let mut pane = new_test_pane();
        pane.status.branch.head = Some("main".to_string());
        pane.status.branch.oid = Some("abcdef1234567".to_string());
        let s = pane.compute_summary();
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.detached, None);
    }

    #[test]
    fn compute_summary_counts_conflicts() {
        use crate::git::status::{Entry, EntryKind};
        let mut pane = new_test_pane();
        pane.status.entries.push(Entry {
            path: "a".into(),
            kind: EntryKind::Unmerged,
            index: 'U',
            worktree: 'U',
        });
        pane.status.entries.push(Entry {
            path: "b".into(),
            kind: EntryKind::Ordinary,
            index: 'M',
            worktree: '.',
        });
        let s = pane.compute_summary();
        assert_eq!(s.conflicts, 1);
        assert_eq!(s.changed, 2);
    }

    /// A pane with no worker threads doing anything, for pure-logic tests that touch a few
    /// fields directly (not spawning `new()`'s bootstrap jobs, which need a live `egui::Context`
    /// and would leak threads that never get drained in a unit test).
    pub(super) fn new_test_pane() -> GitPane {
        let (tx, rx) = channel();
        GitPane {
            ctx: egui::Context::default(),
            location: Location::Local { path: PathBuf::from("/tmp/repo") }, // portability: allow
            git: Git::new(Location::Local { path: PathBuf::from("/tmp/repo") }), // portability: allow
            journal_path: PathBuf::from("/tmp/journal.json"),               // portability: allow
            journal: Journal::default(),
            view: GitView::Graph,
            status: Status::default(),
            refs_list: Vec::new(),
            ref_targets: BTreeMap::new(),
            refs_seen: false,
            stashes: Vec::new(),
            worktrees: Vec::new(),
            remotes: Vec::new(),
            op: None,
            git_dir: None,
            layout: gitgraph::Layout::default(),
            commits: Vec::new(),
            rows: Vec::new(),
            log_loaded: 0,
            log_has_more: true,
            log_loading: false,
            selected: Selection::None,
            details: None,
            changes: changes::ChangesState::default(),
            tx,
            rx,
            generation: 1,
            next_req: 1,
            log_refetch_dispatched_gen: 0,
            undo_busy: false,
            live: Live::Poll { last_poll: 0.0 },
            last_summary: RepoSummary::default(),
            initial_loading: true,
        }
    }
}
