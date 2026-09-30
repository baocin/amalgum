//! Commit details and the diff view below the graph (§5.5, §5.6): header, message, the
//! changed-file list, and the unified diff of whichever file is selected. A selected stash
//! (§5.12) shows here exactly like a commit, with **Apply** / **Pop** / **Drop** / **Branch from
//! stash**; the stash popover, the branch-name prompt, and the drop confirmation live here too.
//! Every stash operation runs through `git::ops` on a worker and is journaled (§5.18).

use super::worker;
use super::{GitEvent, GitPane};
use crate::git::cmd::{Git, GitError};
use crate::git::diff::{self, FileChange, FileDiff, Line, LineKind};
use crate::git::log::Commit;
use crate::git::ops::{self, Executed, Failure, Op, OpError, PlanError};
use crate::git::refs::Stash;
use crate::model::confirm::{Confirm, ConfirmKind, Decision};
use crate::model::keymap::Preset;
use crate::model::settings::Settings;
use crate::model::theme::Token;
use crate::ui::chrome::Toast;
use crate::ui::dialogs::{self, FormOutcome, Popover};
use crate::ui::theme::Colors;
use std::ops::Range;

/// Files over this many lines render only the first page, with a "Load more" button (§5.6).
pub(super) const DIFF_LINE_PAGE: usize = 2000;

pub(super) struct DetailsState {
    pub commit: Commit,
    pub body: Option<String>,
    pub files: Option<Vec<FileChange>>,
    pub selected_file: Option<String>,
    pub diff: Vec<FileDiff>,
    pub diff_loading: bool,
    pub diff_line_limit: usize,
    pub diff_req: u64,
    /// Set when the details show a stash (§5.12) rather than a commit.
    pub stash: Option<StashRef>,
}

/// The stash the details show: its diff is against its base commit (`<oid>^1`), plus the
/// untracked files it saved (`<oid>^3`, `-u`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StashRef {
    pub index: usize,
    pub oid: String,
    pub message: String,
}

impl DetailsState {
    fn new(commit: Commit) -> Self {
        Self {
            commit,
            body: None,
            files: None,
            selected_file: None,
            diff: Vec::new(),
            diff_loading: false,
            diff_line_limit: DIFF_LINE_PAGE,
            diff_req: 0,
            stash: None,
        }
    }
}

/// The inverse of `util::days_from_civil` (Howard Hinnant's `civil_from_days`), kept local since
/// `util.rs` only exposes the forward direction. Correct for any non-negative day count, which
/// covers every commit timestamp this pane will ever format.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Absolute date for the commit header (§5.5 "date absolute + relative"), UTC.
pub(super) fn absolute_time(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs_of_day = unix % 86_400;
    let (y, m, d) = civil_from_days(days);
    let (h, mi) = (secs_of_day / 3600, (secs_of_day % 3600) / 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")
}

/// How many diff lines to show given a per-file limit: `(shown, has_more)`.
pub(super) fn visible_line_count(total: usize, limit: usize) -> (usize, bool) {
    (total.min(limit), total > limit)
}

impl GitPane {
    /// Loads the currently selected commit's header (instant, from the already-fetched `Commit`
    /// record), then dispatches the body and file-list jobs.
    pub(super) fn load_details_for_selected(&mut self, ctx: &egui::Context) {
        let super::Selection::Commit(id) = &self.selected else {
            self.details = None;
            return;
        };
        let Some(commit) = self.commits.iter().find(|c| &c.id == id).cloned() else {
            self.details = None;
            return;
        };
        self.show_commit_details(ctx, commit);
    }

    /// Details for `commit`, which need not be among the loaded graph rows (a remote search
    /// result further back than the graph has read).
    pub(super) fn show_commit_details(&mut self, ctx: &egui::Context, commit: Commit) {
        self.details_replaced(); // §5.13: a new selection ends compare mode
        let id = commit.id.clone();
        self.details = Some(DetailsState::new(commit));
        self.dispatch_commit_body(ctx, id.clone());
        self.dispatch_file_list(ctx, id, false);
    }

    /// §5.12 "Select a stash: details shows its diff exactly like a commit". The header comes
    /// from the stash list at once and is completed from `git log -1` on a worker.
    pub(super) fn select_stash(&mut self, ctx: &egui::Context, stash: &Stash) {
        self.details_replaced(); // §5.13: a new selection ends compare mode
        self.selected = super::Selection::None;
        let commit = Commit {
            id: stash.oid.clone(),
            parents: Vec::new(),
            author: String::new(),
            email: String::new(),
            time: stash.time,
            committer: String::new(),
            committer_email: String::new(),
            commit_time: stash.time,
            refs: Vec::new(),
            subject: stash.message.clone(),
        };
        let mut details = DetailsState::new(commit);
        details.stash =
            Some(StashRef { index: stash.index, oid: stash.oid.clone(), message: stash.message.clone() });
        self.details = Some(details);
        let (git, oid) = (self.git.clone(), stash.oid.clone());
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let args = crate::git::log::log_args(&["-1", &oid, "--"]);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let result = git.run(&args).map(|o| crate::git::log::parse_log(&o));
            worker::Reply::Stash(StashReply::Header { oid, result })
        });
        self.dispatch_commit_body(ctx, stash.oid.clone());
        self.dispatch_file_list(ctx, stash.oid.clone(), true);
    }

    /// The selected stash, if the details show one.
    pub(super) fn selected_stash(&self) -> Option<&StashRef> {
        self.details.as_ref()?.stash.as_ref()
    }

    fn dispatch_commit_body(&self, ctx: &egui::Context, id: String) {
        let git = self.git.clone();
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let result = git
                .run(&crate::git::log::message_args(&id))
                .map(|o| crate::git::log::split_message(&String::from_utf8_lossy(&o)));
            worker::Reply::CommitBody { id, result }
        });
    }

    fn dispatch_file_list(&self, ctx: &egui::Context, id: String, stash: bool) {
        let git = self.git.clone();
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let result = if stash {
                stash_file_list(&git, &id)
            } else {
                let mut args = vec!["diff-tree", "-r", "--root", "--no-commit-id", &id];
                args.extend_from_slice(diff::RAW_NUMSTAT_ARGS);
                git.run(&args).map(|o| diff::parse_raw_numstat(&o))
            };
            worker::Reply::FileList { id, result }
        });
    }

    pub(super) fn dispatch_details_diff(&mut self, ctx: &egui::Context, path: String) {
        let Some(details_id) = self.details.as_ref().map(|d| d.commit.id.clone()) else { return };
        let is_stash = self.selected_stash().is_some();
        let req = self.next_req_id();
        if let Some(details) = &mut self.details {
            details.selected_file = Some(path.clone());
            details.diff_loading = true;
            details.diff_req = req;
        }
        let git = self.git.clone();
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let result = if is_stash {
                stash_file_diff(&git, &details_id, &path)
            } else {
                let mut args = vec!["show", "--format="];
                args.extend_from_slice(diff::DIFF_ARGS);
                args.extend([details_id.as_str(), "--", path.as_str()]);
                git.run(&args).map(|o| diff::parse(&o))
            };
            worker::Reply::DetailsDiff { req, result }
        });
    }

    pub(super) fn apply_commit_body(&mut self, id: String, result: Result<(String, String), GitError>) {
        if self.details.as_ref().map(|d| d.commit.id.as_str()) != Some(id.as_str()) {
            return;
        }
        if let (Some(details), Ok((_subject, body))) = (&mut self.details, result) {
            details.body = Some(body);
        }
    }

    pub(super) fn apply_file_list(
        &mut self,
        id: String,
        result: Result<Vec<FileChange>, GitError>,
        events: &mut Vec<GitEvent>,
    ) {
        if self.details.as_ref().map(|d| d.commit.id.as_str()) != Some(id.as_str()) {
            return;
        }
        match result {
            Ok(files) => {
                if let Some(details) = &mut self.details {
                    details.files = Some(files);
                }
            }
            Err(e) => events.push(GitEvent::Toast(crate::ui::chrome::Toast::error(
                "Could not read changed files",
                e.to_string(),
            ))),
        }
    }

    pub(super) fn apply_details_diff(
        &mut self,
        req: u64,
        result: Result<Vec<FileDiff>, GitError>,
        events: &mut Vec<GitEvent>,
    ) {
        let Some(details) = &mut self.details else { return };
        if details.diff_req != req {
            return; // superseded by a later click
        }
        details.diff_loading = false;
        match result {
            Ok(files) => details.diff = files,
            Err(e) => events
                .push(GitEvent::Toast(crate::ui::chrome::Toast::error("Could not read diff", e.to_string()))),
        }
    }

    pub(super) fn show_details(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        now: u64,
        _events: &mut Vec<GitEvent>,
        ctx: &egui::Context,
    ) {
        if self.view == crate::model::state::GitView::Graph {
            return; // drawn in the Graph view's bottom panel (`show_graph_bottom_panel`)
        }
        // §5.13 compare mode and §5.14 multi-selection take the details area over.
        if self.show_selection_details(ui, colors, now) {
            return;
        }
        self.draw_details(ui, colors, now, ctx);
    }

    /// The selected commit's (or stash's) details: message, files, and the selected file's diff.
    pub(super) fn draw_details(&mut self, ui: &mut egui::Ui, colors: &Colors, now: u64, ctx: &egui::Context) {
        // Snapshot everything needed for rendering up front: every field below borrows `self`
        // immutably, and several widgets below (parent hashes, file rows, "Load more") need a
        // `&mut self` call when clicked, so no borrow of `self.details` may still be alive by
        // then. `action` collects the one thing the user did this frame, applied at the end.
        let Some(details) = &self.details else {
            ui.label("Select a commit to see its details.");
            return;
        };
        let commit = details.commit.clone();
        let body = details.body.clone();
        let files = details.files.clone();
        let selected_file = details.selected_file.clone();
        let diff = details.diff.clone();
        let diff_loading = details.diff_loading;
        let limit = details.diff_line_limit;

        let stash = details.stash.clone();
        let mut action: Option<DetailAction> = None;
        let mut file_view: Option<super::history::FileViewRequest> = None;

        if let Some(stash) = &stash {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("stash@{{{}}}", stash.index)).strong());
                let busy = self.stash_ui.busy;
                for (label, act) in [
                    ("Apply", StashAction::Apply),
                    ("Pop", StashAction::Pop),
                    ("Drop…", StashAction::Drop),
                    ("Branch from stash…", StashAction::Branch),
                ] {
                    if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                        action = Some(DetailAction::Stash(act));
                    }
                }
            });
            ui.separator();
        }

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&commit.id).monospace());
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(commit.id.clone());
            }
        });
        ui.label(format!("{} <{}>", commit.author, commit.email));
        if commit.author != commit.committer {
            ui.label(format!("committed by {} <{}>", commit.committer, commit.committer_email));
        }
        ui.label(format!(
            "{} · {}",
            absolute_time(commit.time),
            crate::util::relative_time(now, commit.time)
        ));
        if !commit.parents.is_empty() {
            ui.horizontal(|ui| {
                ui.label("Parents:");
                for p in &commit.parents {
                    if ui.button(worker::short_hash(p)).clicked() {
                        action = Some(DetailAction::SelectParent(p.clone()));
                    }
                }
            });
        }
        if !commit.refs.is_empty() {
            let names: Vec<String> = commit.refs.iter().filter_map(super::graph::chip_text).collect();
            if !names.is_empty() {
                ui.label(format!("Refs: {}", names.join(", ")));
            }
        }

        ui.separator();
        ui.label(egui::RichText::new(&commit.subject).strong());
        match &body {
            Some(body) if !body.is_empty() => {
                ui.label(egui::RichText::new(body).monospace());
            }
            Some(_) => {}
            None => {
                ui.weak("Loading…");
            }
        }

        ui.separator();
        match &files {
            None => {
                ui.weak("Loading changed files…");
            }
            Some(files) if files.is_empty() => {
                ui.weak("No file changes (empty commit).");
            }
            Some(files) => {
                for f in files {
                    let label = match &f.old_path {
                        Some(old) => format!("{} {old} → {}", f.status, f.path),
                        None => format!("{} {}", f.status, f.path),
                    };
                    let counts = match (f.added, f.deleted) {
                        (Some(a), Some(d)) => format!("+{a} −{d}"),
                        _ => "binary".to_string(),
                    };
                    let selected = selected_file.as_deref() == Some(f.path.as_str());
                    let color = status_color(colors, f.status);
                    let resp = ui.horizontal(|ui| {
                        if selected {
                            ui.painter().rect_filled(
                                ui.available_rect_before_wrap(),
                                0.0,
                                colors.get(Token::BgSelected),
                            );
                        }
                        ui.colored_label(color, label);
                        ui.weak(counts);
                    });
                    let row = ui.interact(
                        resp.response.rect,
                        ui.id().with(("detail_file", &f.path)),
                        egui::Sense::click(),
                    );
                    if row.clicked() {
                        action = Some(DetailAction::SelectFile(f.path.clone()));
                    }
                    if stash.is_none() {
                        // §5.15: right-click → File history / Blame; `b` on the focused row.
                        let target = super::history::FileRow::in_commit(&commit.id, &f.path, f.status);
                        self.note_file_focus(&row, target.clone());
                        file_view = super::history::file_menu(&row, &target).or(file_view);
                    }
                }
            }
        }

        ui.separator();
        if diff_loading {
            ui.weak("Loading diff…");
        } else {
            let mut want_more = false;
            for file in &diff {
                let line = render_diff_file(ui, colors, file, limit, &mut want_more);
                if let Some(line) = line.filter(|_| stash.is_none()) {
                    let target = super::history::line_target(&commit.id, file, line);
                    file_view = target.map(super::history::FileViewRequest::Blame).or(file_view);
                }
            }
            if want_more {
                action = Some(DetailAction::LoadMore);
            }
        }

        if let Some(request) = file_view {
            self.open_file_view(ctx, request);
            return;
        }
        match action {
            Some(DetailAction::SelectParent(hash)) => {
                self.selected = super::Selection::Commit(hash);
                self.load_details_for_selected(ctx);
            }
            Some(DetailAction::SelectFile(path)) => self.dispatch_details_diff(ctx, path),
            Some(DetailAction::LoadMore) => {
                if let Some(d) = &mut self.details {
                    d.diff_line_limit += DIFF_LINE_PAGE;
                }
            }
            Some(DetailAction::Stash(act)) => {
                if let Some(stash) = stash {
                    self.stash_action(ctx, act, stash, files.unwrap_or_default());
                }
            }
            None => {}
        }
    }
}

/// The single user action `show_details` may take in a frame, applied after every borrow of
/// `self.details` used for rendering has ended (see the comment in `show_details`).
enum DetailAction {
    SelectParent(String),
    SelectFile(String),
    LoadMore,
    Stash(StashAction),
}

// ---- §5.12 stashes ---------------------------------------------------------------------------

/// The buttons on a selected stash's details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StashAction {
    Apply,
    Pop,
    Drop,
    Branch,
}

/// Replies of the stash jobs, carried in `Reply::Stash`.
pub(super) enum StashReply {
    /// The stash commit's full record (`git log -1`), completing the details header.
    Header { oid: String, result: Result<Vec<Commit>, GitError> },
    /// A stash operation finished; `oid` is the stash it acted on (`None` for a push).
    Done { op: Op, oid: Option<String>, result: Box<Result<Executed, OpError>> },
}

/// The stash popover (§5.12 "Popover: message, Include untracked, Keep staged").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PushForm {
    pub message: String,
    pub include_untracked: bool,
    pub keep_index: bool,
}

impl Default for PushForm {
    fn default() -> Self {
        Self { message: String::new(), include_untracked: true, keep_index: false }
    }
}

impl PushForm {
    /// An empty message leaves git's default ("WIP on main: …").
    pub fn op(&self) -> Op {
        let message = self.message.trim();
        Op::StashPush {
            message: (!message.is_empty()).then(|| message.to_string()),
            include_untracked: self.include_untracked,
            keep_index: self.keep_index,
        }
    }
}

/// **Branch from stash**: the name prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BranchForm {
    pub stash: StashRef,
    pub name: String,
}

/// **Drop** waiting on its §5.20 confirmation.
#[derive(Debug, Clone)]
pub(super) struct PendingDrop {
    pub stash: StashRef,
    pub confirm: Confirm,
}

/// Stash popovers and the one job in flight.
#[derive(Debug, Clone, Default)]
pub(super) struct StashUi {
    pub push: Option<PushForm>,
    pub branch: Option<BranchForm>,
    pub drop: Option<PendingDrop>,
    pub busy: bool,
}

/// The live validation message for a new branch's name (`None` = valid).
pub(super) fn branch_name_error(name: &str, existing: &[String]) -> Option<String> {
    if let Err(e) = crate::git::remote::validate_branch_name(name) {
        return Some(e.to_string());
    }
    existing.iter().any(|b| b == name).then(|| format!("A branch named `{name}` already exists"))
}

/// The §5.20 confirmation for dropping `stash`; `files` are what it changes.
pub(super) fn drop_confirm(stash: &StashRef, files: &[FileChange]) -> Confirm {
    let detail = if files.is_empty() {
        format!("“{}” will be deleted.", stash.message)
    } else {
        format!("“{}” and its changes to these files will be deleted:", stash.message)
    };
    Confirm::new(ConfirmKind::DropStash, format!("Drop stash@{{{}}}?", stash.index))
        .detail(detail)
        .lost(files.iter().map(|f| f.path.clone()))
}

/// The success toast's title for a finished stash operation.
pub(super) fn done_title(op: &Op) -> String {
    match op {
        Op::StashPush { .. } => "Stashed changes".to_string(),
        Op::StashApply { index } => format!("Applied stash@{{{index}}}"),
        Op::StashPop { index } => format!("Popped stash@{{{index}}}"),
        Op::StashDrop { index } => format!("Dropped stash@{{{index}}}"),
        Op::StashBranch { name, .. } => format!("Created `{name}` from the stash"),
        _ => "Done".to_string(),
    }
}

/// §5.24: the toast's details are the full command line and git's stderr.
pub(super) fn error_details(e: &GitError) -> String {
    format!("$ {}\n{}", e.command, e.stderr)
}

/// The toast for a failed stash operation (§5.12 "Nothing to stash: toast", §5.24).
pub(super) fn failure_toast(err: &OpError) -> Toast {
    match err {
        OpError::Known { failure: Failure::NothingToStash, .. } => Toast::info("Nothing to stash"),
        OpError::Conflict { error, .. } => Toast::warning(err.to_string(), error_details(error)),
        OpError::Git(e) => Toast::error(e.summary(), error_details(e)),
        OpError::Known { error: Some(e), .. } => Toast::error(err.to_string(), error_details(e)),
        OpError::Known { error: None, .. } | OpError::Plan(_) => Toast::error(err.to_string(), String::new()),
    }
}

/// The file list of stash `oid`: its tracked changes against its base (`oid^1`), then the
/// untracked files it saved (`oid^3`, when stashed with `-u`).
fn stash_file_list(git: &Git, oid: &str) -> Result<Vec<FileChange>, GitError> {
    let base = format!("{oid}^1");
    let mut args = vec!["diff"];
    args.extend_from_slice(diff::RAW_NUMSTAT_ARGS);
    args.extend([base.as_str(), oid, "--"]);
    let mut files = diff::parse_raw_numstat(&git.run(&args)?);
    if let Some(untracked) = untracked_commit(git, oid) {
        let mut args = vec!["diff-tree", "-r", "--root", "--no-commit-id"];
        args.extend_from_slice(diff::RAW_NUMSTAT_ARGS);
        args.extend([untracked.as_str(), "--"]);
        files.extend(diff::parse_raw_numstat(&git.run(&args)?));
    }
    Ok(files)
}

/// One file's diff in stash `oid`, from its tracked changes or else its untracked files.
fn stash_file_diff(git: &Git, oid: &str, path: &str) -> Result<Vec<FileDiff>, GitError> {
    let base = format!("{oid}^1");
    let mut args = vec!["diff"];
    args.extend_from_slice(diff::DIFF_ARGS);
    args.extend([base.as_str(), oid, "--", path]);
    let files = diff::parse(&git.run(&args)?);
    if !files.is_empty() {
        return Ok(files);
    }
    let Some(untracked) = untracked_commit(git, oid) else { return Ok(files) };
    let mut args = vec!["show", "--format="];
    args.extend_from_slice(diff::DIFF_ARGS);
    args.extend([untracked.as_str(), "--", path]);
    Ok(diff::parse(&git.run(&args)?))
}

/// `oid^3`, the untracked-files commit of a stash made with `-u`, if it has one.
fn untracked_commit(git: &Git, oid: &str) -> Option<String> {
    let rev = format!("{oid}^3");
    git.run(&["rev-parse", "-q", "--verify", &format!("{rev}^{{commit}}")]).ok().map(|_| rev)
}

impl GitPane {
    /// `Mod+Shift+S`, palette **Stash**: open the popover, or toast when there is nothing to
    /// stash (§5.12).
    /// Returns the toast for the app to show instead, if any.
    pub fn open_stash(&mut self) -> Option<Toast> {
        if !self.initial_loading && self.status.changed_count() == 0 {
            return Some(Toast::info("Nothing to stash"));
        }
        self.stash_ui.push.get_or_insert_with(PushForm::default);
        None
    }

    fn stash_action(
        &mut self,
        ctx: &egui::Context,
        act: StashAction,
        stash: StashRef,
        files: Vec<FileChange>,
    ) {
        match act {
            StashAction::Apply => {
                self.dispatch_stash_op(ctx, Op::StashApply { index: stash.index }, Some(stash))
            }
            StashAction::Pop => self.dispatch_stash_op(ctx, Op::StashPop { index: stash.index }, Some(stash)),
            StashAction::Drop => {
                let confirm = drop_confirm(&stash, &files);
                self.stash_ui.drop = Some(PendingDrop { stash, confirm });
            }
            StashAction::Branch => self.stash_ui.branch = Some(BranchForm { stash, name: String::new() }),
        }
    }

    /// Runs `op` through `git::ops` on a worker. For an op on an existing stash, `stash` pins
    /// the stash the user saw: if `stash@{n}` no longer is that commit (a push or drop since),
    /// nothing runs.
    fn dispatch_stash_op(&mut self, ctx: &egui::Context, op: Op, stash: Option<StashRef>) {
        if self.stash_ui.busy {
            return;
        }
        self.stash_ui.busy = true;
        let git = self.git.clone();
        let now = crate::util::unix_now();
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let oid = stash.as_ref().map(|s| s.oid.clone());
            if let Some(s) = &stash {
                let rev = format!("refs/stash@{{{}}}", s.index);
                let current = git.run(&["rev-parse", "-q", "--verify", &rev]).ok();
                let current = current.map(|o| String::from_utf8_lossy(&o).trim().to_string());
                if current.as_deref() != Some(s.oid.as_str()) {
                    let msg =
                        format!("stash@{{{}}} has changed since it was selected; select it again", s.index);
                    let result = Err(OpError::Plan(PlanError(msg)));
                    return worker::Reply::Stash(StashReply::Done { op, oid, result: Box::new(result) });
                }
            }
            let result = ops::execute(&git, &op, now);
            worker::Reply::Stash(StashReply::Done { op, oid, result: Box::new(result) })
        });
    }

    pub(super) fn apply_stash_reply(
        &mut self,
        ctx: &egui::Context,
        reply: StashReply,
        events: &mut Vec<GitEvent>,
    ) {
        match reply {
            StashReply::Header { oid, result } => {
                let Some(details) = &mut self.details else { return };
                if details.commit.id != oid {
                    return;
                }
                if let Some(commit) = result.ok().and_then(|c| c.into_iter().next()) {
                    // Keep the stash list's message: the record's subject is `On main: msg`.
                    let subject = std::mem::take(&mut details.commit.subject);
                    details.commit = Commit { subject, ..commit };
                }
            }
            StashReply::Done { op, oid, result } => {
                self.stash_ui.busy = false;
                match *result {
                    Ok(executed) => {
                        if let Some(entry) = executed.entry {
                            self.journal.record(entry);
                            let _ = self.journal.save(&self.journal_path);
                        }
                        events.push(GitEvent::Toast(Toast::success(done_title(&op))));
                        let gone = !matches!(op, Op::StashApply { .. } | Op::StashPush { .. });
                        if gone && oid.is_some() && self.selected_stash().map(|s| &s.oid) == oid.as_ref() {
                            self.details = None;
                        }
                    }
                    Err(err) => {
                        if matches!(err, OpError::Conflict { .. }) {
                            self.view = crate::model::state::GitView::Changes; // §5.17
                        }
                        events.push(GitEvent::Toast(failure_toast(&err)));
                    }
                }
                self.refresh_now(ctx);
            }
        }
    }

    /// The stash popovers and the drop confirmation, drawn over the pane whatever its view.
    pub(super) fn show_stash_overlays(
        &mut self,
        ui: &egui::Ui,
        colors: &Colors,
        settings: &Settings,
        preset: Preset,
        events: &mut Vec<GitEvent>,
    ) {
        let ctx = ui.ctx().clone();
        let at = ui.max_rect().left_top() + egui::vec2(12.0, 36.0);
        if let Some(form) = &mut self.stash_ui.push {
            let outcome =
                Popover::new("stash-push", "Stash changes").primary("Stash").show(&ctx, colors, at, |f| {
                    f.text(&mut form.message, "Message (default from git)", None)
                        .checkbox(&mut form.include_untracked, "Include untracked")
                        .checkbox(&mut form.keep_index, "Keep staged");
                });
            match outcome {
                FormOutcome::Submitted => {
                    let op = form.op();
                    self.stash_ui.push = None;
                    self.dispatch_stash_op(&ctx, op, None);
                }
                FormOutcome::Cancelled => self.stash_ui.push = None,
                FormOutcome::Open => {}
            }
        }
        if let Some(form) = &mut self.stash_ui.branch {
            let existing: Vec<String> = self
                .refs_list
                .iter()
                .filter(|r| r.kind == crate::git::refs::RefKind::Local)
                .map(|r| r.short.clone())
                .collect();
            let title = format!("Branch from stash@{{{}}}", form.stash.index);
            let error = branch_name_error(&form.name, &existing);
            let outcome = Popover::new("stash-branch", title).primary("Create").show(&ctx, colors, at, |f| {
                f.text(&mut form.name, "branch name", error.as_deref())
                    .note("Checks out the stash's base commit on the new branch and pops the stash there.");
            });
            match outcome {
                FormOutcome::Submitted => {
                    let (name, stash) = (form.name.clone(), form.stash.clone());
                    self.stash_ui.branch = None;
                    self.dispatch_stash_op(&ctx, Op::StashBranch { name, index: stash.index }, Some(stash));
                }
                FormOutcome::Cancelled => self.stash_ui.branch = None,
                FormOutcome::Open => {}
            }
        }
        if let Some(pending) = &mut self.stash_ui.drop {
            let decision = if settings.should_confirm(pending.confirm.kind) {
                dialogs::confirm_dialog(&ctx, colors, preset, &mut pending.confirm)
            } else {
                Some(Decision::Confirm)
            };
            match decision {
                Some(Decision::Confirm) => {
                    if pending.confirm.remember() {
                        events.push(GitEvent::DontAskAgain(pending.confirm.kind));
                    }
                    let stash = pending.stash.clone();
                    self.stash_ui.drop = None;
                    self.dispatch_stash_op(&ctx, Op::StashDrop { index: stash.index }, Some(stash));
                }
                Some(Decision::Cancel) => self.stash_ui.drop = None,
                None => {}
            }
        }
    }

    /// The Refs view shows a selected stash's details below its lists (§5.12).
    pub(super) fn show_refs_details(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        now: u64,
        events: &mut Vec<GitEvent>,
        ctx: &egui::Context,
    ) {
        if self.selected_stash().is_none() {
            return;
        }
        ui.separator();
        egui::ScrollArea::vertical().id_salt("refs_stash_details").auto_shrink([false, false]).show(
            ui,
            |ui| {
                self.show_details(ui, colors, now, events, ctx);
            },
        );
    }
}

pub(super) fn status_color(colors: &Colors, status: char) -> egui::Color32 {
    match status {
        'A' | 'C' => colors.get(Token::DiffAddFg),
        'D' => colors.get(Token::DiffDelFg),
        _ => colors.get(Token::FgSecondary),
    }
}

/// Renders one file's hunks (read-only: used by the commit details view). `budget` is decremented
/// as lines are drawn; once it reaches the file's `limit`, drawing stops and `*want_more` is set
/// if the caller clicks "Load more". Returns the line chosen with **Blame this line** (§5.15).
pub(super) fn render_diff_file(
    ui: &mut egui::Ui,
    colors: &Colors,
    file: &FileDiff,
    limit: usize,
    want_more: &mut bool,
) -> Option<super::history::DiffLine> {
    let mut blame_line = None;
    let title = match (&file.old_path, &file.new_path) {
        (Some(a), Some(b)) if a != b => format!("{a} → {b}"),
        (Some(a), _) => a.clone(),
        (None, Some(b)) => b.clone(),
        (None, None) => "(unknown)".to_string(),
    };
    ui.label(egui::RichText::new(title).strong());
    if file.binary {
        ui.weak("Binary file");
        return None;
    }
    let total: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
    let (shown, truncated) = visible_line_count(total, limit);
    let mut drawn = 0usize;
    'hunks: for hunk in &file.hunks {
        ui.horizontal(|ui| {
            let bg = colors.get(Token::DiffHunkBg);
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 18.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 0.0, bg);
            ui.painter().text(
                rect.left_center() + egui::vec2(4.0, 0.0),
                egui::Align2::LEFT_CENTER,
                format!(
                    "@@ -{},{} +{},{} @@ {}",
                    hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len, hunk.section
                ),
                egui::FontId::monospace(11.0),
                colors.get(Token::FgSecondary),
            );
        });
        let mut i = 0;
        while i < hunk.lines.len() {
            if drawn >= shown {
                break 'hunks;
            }
            let paired = pair_for_word_diff(&hunk.lines, i);
            match paired {
                Some((del, add)) => {
                    let r = render_line(ui, colors, &hunk.lines[del], Some(&hunk.lines[add].display()));
                    line_menu(&r, &hunk.lines[del], &mut blame_line);
                    let r = render_line(ui, colors, &hunk.lines[add], Some(&hunk.lines[del].display()));
                    line_menu(&r, &hunk.lines[add], &mut blame_line);
                    i += 2;
                    drawn += 2;
                }
                None => {
                    let r = render_line(ui, colors, &hunk.lines[i], None);
                    line_menu(&r, &hunk.lines[i], &mut blame_line);
                    i += 1;
                    drawn += 1;
                }
            }
        }
    }
    if truncated {
        ui.label(format!("Showing {shown} of {total} lines."));
        if ui.button("Load more").clicked() {
            *want_more = true;
        }
    }
    blame_line
}

/// The diff line menu's **Blame this line** (§5.6, §5.15): a removed line is blamed on the old
/// side, every other line on the new side.
fn line_menu(resp: &egui::Response, line: &Line, chosen: &mut Option<super::history::DiffLine>) {
    use super::history::DiffLine;
    let target = match line.kind {
        LineKind::Del => line.old_no.map(DiffLine::Old),
        _ => line.new_no.map(DiffLine::New),
    };
    let Some(target) = target else { return };
    resp.context_menu(|ui| {
        if ui.button("Blame this line").clicked() {
            *chosen = Some(target);
            ui.close();
        }
    });
}

/// If `lines[i]` is a `Del` immediately followed by an `Add` (a replace pair, so word-diff
/// applies), returns their indices.
pub(super) fn pair_for_word_diff(lines: &[Line], i: usize) -> Option<(usize, usize)> {
    if lines.get(i).map(|l| l.kind) == Some(LineKind::Del)
        && lines.get(i + 1).map(|l| l.kind) == Some(LineKind::Add)
    {
        Some((i, i + 1))
    } else {
        None
    }
}

pub(super) fn render_line(
    ui: &mut egui::Ui,
    colors: &Colors,
    line: &Line,
    pair_text: Option<&str>,
) -> egui::Response {
    if line.kind == LineKind::NoNewline {
        return ui.weak("\\ No newline at end of file");
    }
    let (bg, fg, word_bg) = match line.kind {
        LineKind::Add => {
            (Some(colors.get(Token::DiffAddBg)), colors.get(Token::DiffAddFg), colors.get(Token::DiffAddWord))
        }
        LineKind::Del => {
            (Some(colors.get(Token::DiffDelBg)), colors.get(Token::DiffDelFg), colors.get(Token::DiffDelWord))
        }
        _ => (None, colors.get(Token::FgPrimary), colors.get(Token::BgHover)),
    };
    let text = line.display();
    let highlight: Vec<Range<usize>> = match pair_text {
        Some(other) if line.kind == LineKind::Del => diff::word_diff(&text, other).0,
        Some(other) if line.kind == LineKind::Add => diff::word_diff(other, &text).1,
        _ => Vec::new(),
    };

    let row_h = 16.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
    if let Some(bg) = bg {
        ui.painter().rect_filled(rect, 0.0, bg);
    }
    let gutter = colors.get(Token::DiffGutterFg);
    let old_no = line.old_no.map(|n| n.to_string()).unwrap_or_default();
    let new_no = line.new_no.map(|n| n.to_string()).unwrap_or_default();
    let painter = ui.painter_at(rect);
    painter.text(
        rect.left_center() + egui::vec2(4.0, 0.0),
        egui::Align2::LEFT_CENTER,
        format!("{old_no:>5} {new_no:>5}"),
        egui::FontId::monospace(11.0),
        gutter,
    );
    let job = line_job(&text, fg, &highlight, word_bg);
    let galley = painter.layout_job(job);
    painter.galley(rect.left_center() + egui::vec2(80.0, -galley.size().y / 2.0), galley, fg);
    resp
}

pub(super) fn line_job(
    text: &str,
    color: egui::Color32,
    highlight: &[Range<usize>],
    highlight_bg: egui::Color32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let font = egui::FontId::monospace(12.0);
    let mut ranges: Vec<Range<usize>> = highlight.to_vec();
    ranges.sort_by_key(|r| r.start);
    let mut pos = 0usize;
    for r in ranges {
        if r.start > text.len() || r.end > text.len() || r.start >= r.end {
            continue;
        }
        if r.start > pos {
            job.append(
                &text[pos..r.start],
                0.0,
                egui::TextFormat { font_id: font.clone(), color, ..Default::default() },
            );
        }
        job.append(
            &text[r.start..r.end],
            0.0,
            egui::TextFormat { font_id: font.clone(), color, background: highlight_bg, ..Default::default() },
        );
        pos = r.end;
    }
    if pos < text.len() {
        job.append(&text[pos..], 0.0, egui::TextFormat { font_id: font, color, ..Default::default() });
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- absolute_time -------------------------------------------------------------------

    #[test]
    fn absolute_time_epoch() {
        assert_eq!(absolute_time(0), "1970-01-01 00:00");
    }

    #[test]
    fn absolute_time_matches_util_parse_date_fixture() {
        // util::parse_date("2000-03-01") == Some(951_868_800) (see src/util.rs tests).
        assert_eq!(absolute_time(951_868_800), "2000-03-01 00:00");
    }

    #[test]
    fn absolute_time_includes_time_of_day() {
        // 1970-01-01 01:02:03 UTC.
        assert_eq!(absolute_time(3723), "1970-01-01 01:02");
    }

    #[test]
    fn absolute_time_round_trips_against_util_parse_date_for_many_dates() {
        for date in ["1999-12-31", "2024-02-29", "2038-01-19", "2100-06-15"] {
            let unix = crate::util::parse_date(date).expect("valid date");
            assert_eq!(absolute_time(unix), format!("{date} 00:00"), "date {date}");
        }
    }

    // ---- visible_line_count -----------------------------------------------------------------

    #[test]
    fn visible_line_count_under_limit_shows_all() {
        assert_eq!(visible_line_count(100, 2000), (100, false));
    }

    #[test]
    fn visible_line_count_over_limit_truncates() {
        assert_eq!(visible_line_count(3000, 2000), (2000, true));
    }

    #[test]
    fn visible_line_count_exactly_at_limit_is_not_truncated() {
        assert_eq!(visible_line_count(2000, 2000), (2000, false));
    }

    // ---- pair_for_word_diff -----------------------------------------------------------------

    fn line(kind: LineKind, text: &str) -> Line {
        Line { kind, old_no: None, new_no: None, text: text.into() }
    }

    #[test]
    fn pair_for_word_diff_detects_del_then_add() {
        let lines = vec![line(LineKind::Context, "a"), line(LineKind::Del, "b"), line(LineKind::Add, "c")];
        assert_eq!(pair_for_word_diff(&lines, 1), Some((1, 2)));
        assert_eq!(pair_for_word_diff(&lines, 0), None);
    }

    #[test]
    fn pair_for_word_diff_none_when_not_adjacent_or_out_of_order() {
        let lines = vec![line(LineKind::Add, "a"), line(LineKind::Del, "b")];
        assert_eq!(pair_for_word_diff(&lines, 0), None);
        let lines2 = vec![line(LineKind::Del, "a")];
        assert_eq!(pair_for_word_diff(&lines2, 0), None, "no following line");
    }

    // ---- stashes (§5.12) ----------------------------------------------------------------------

    fn stashed_repo() -> (crate::testutil::TempRepo, Git, String) {
        let mut repo = crate::testutil::TempRepo::new();
        repo.commit_file("tracked.txt", "one\n", "first");
        repo.write("tracked.txt", "one\ntwo\n");
        repo.write("new file.txt", "fresh\n");
        repo.git(&["stash", "push", "-u", "-m", "wip"]);
        let oid = repo.git(&["rev-parse", "refs/stash"]);
        let git = Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });
        (repo, git, oid)
    }

    #[test]
    fn stash_file_list_has_tracked_changes_and_untracked_files() {
        let (_repo, git, oid) = stashed_repo();
        let files = stash_file_list(&git, &oid).expect("file list");
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["tracked.txt", "new file.txt"]);
        assert_eq!((files[0].added, files[0].deleted), (Some(1), Some(0)));
        assert_eq!(files[1].status, 'A');
    }

    #[test]
    fn stash_file_diff_reads_tracked_and_untracked_paths() {
        let (_repo, git, oid) = stashed_repo();
        let tracked = stash_file_diff(&git, &oid, "tracked.txt").expect("diff");
        assert_eq!(tracked.len(), 1);
        assert!(tracked[0].hunks[0].lines.iter().any(|l| l.kind == LineKind::Add && l.display() == "two"));
        let untracked = stash_file_diff(&git, &oid, "new file.txt").expect("diff");
        assert_eq!(untracked.len(), 1);
        assert!(
            untracked[0].hunks[0].lines.iter().any(|l| l.kind == LineKind::Add && l.display() == "fresh")
        );
    }

    #[test]
    fn stash_file_list_without_untracked_files() {
        let mut repo = crate::testutil::TempRepo::new();
        repo.commit_file("a.txt", "a\n", "first");
        repo.write("a.txt", "b\n");
        repo.git(&["stash", "push"]);
        let git = Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });
        let oid = repo.git(&["rev-parse", "refs/stash"]);
        let files = stash_file_list(&git, &oid).expect("file list");
        assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["a.txt"]);
        assert!(stash_file_diff(&git, &oid, "missing.txt").expect("diff").is_empty());
    }

    #[test]
    fn push_form_defaults_include_untracked_and_leave_the_message_to_git() {
        let form = PushForm::default();
        assert_eq!(form.op(), Op::StashPush { message: None, include_untracked: true, keep_index: false });
        let form = PushForm { message: "  wip auth  ".into(), include_untracked: false, keep_index: true };
        assert_eq!(
            form.op(),
            Op::StashPush { message: Some("wip auth".into()), include_untracked: false, keep_index: true }
        );
    }

    #[test]
    fn stash_push_through_ops_is_journaled_and_nothing_to_stash_is_an_info_toast() {
        let mut repo = crate::testutil::TempRepo::new();
        repo.commit_file("a.txt", "a\n", "first");
        let git = Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });
        let err = ops::execute(&git, &PushForm::default().op(), 0).expect_err("clean tree");
        let toast = failure_toast(&err);
        assert_eq!(
            (toast.level, toast.title.as_str()),
            (crate::ui::chrome::ToastLevel::Info, "Nothing to stash")
        );

        repo.write("b.txt", "untracked\n");
        let done = ops::execute(&git, &PushForm::default().op(), 0).expect("stashed");
        assert!(done.entry.expect("journal entry").inverse.is_some(), "undo: pop it");
        assert!(!repo.path().join("b.txt").exists(), "untracked files are included by default");
    }

    #[test]
    fn git_failures_toast_the_command_and_stderr() {
        let e = GitError {
            command: "git stash apply stash@{0}".into(),
            code: Some(1),
            stderr: "error: boom".into(),
        };
        let toast = failure_toast(&OpError::Git(e));
        assert_eq!(toast.title, "error: boom");
        assert_eq!(toast.details.as_deref(), Some("$ git stash apply stash@{0}\nerror: boom"));
        let plan = failure_toast(&OpError::Plan(PlanError("No stash@{3}".into())));
        assert_eq!(plan.title, "No stash@{3}");
    }

    #[test]
    fn branch_name_validation() {
        let existing = vec!["main".to_string()];
        assert!(branch_name_error("", &existing).is_some());
        assert!(branch_name_error("has space", &existing).is_some());
        assert_eq!(
            branch_name_error("main", &existing).as_deref(),
            Some("A branch named `main` already exists")
        );
        assert_eq!(branch_name_error("from-stash", &existing), None);
    }

    #[test]
    fn drop_confirmation_names_the_stash_and_lists_its_files() {
        let stash = StashRef { index: 2, oid: "abc".into(), message: "wip auth".into() };
        let files = vec![FileChange {
            path: "src/a.rs".into(),
            old_path: None,
            status: 'M',
            added: Some(1),
            deleted: Some(0),
        }];
        let c = drop_confirm(&stash, &files);
        assert_eq!(c.kind, ConfirmKind::DropStash);
        assert_eq!(c.title, "Drop stash@{2}?");
        assert_eq!(c.verb, "Drop");
        assert_eq!(c.lost, vec!["src/a.rs"]);
    }

    #[test]
    fn done_titles() {
        assert_eq!(done_title(&Op::StashPop { index: 1 }), "Popped stash@{1}");
        assert_eq!(done_title(&Op::StashBranch { name: "x".into(), index: 0 }), "Created `x` from the stash");
    }

    // ---- status_color mapping is at least stable/distinct ------------------------------------

    #[test]
    fn status_color_distinguishes_add_delete_other() {
        let colors = Colors::new(crate::model::theme::Mode::Dark);
        assert_ne!(status_color(&colors, 'A'), status_color(&colors, 'D'));
        assert_eq!(status_color(&colors, 'A'), status_color(&colors, 'C'));
        assert_eq!(status_color(&colors, 'M'), colors.get(Token::FgSecondary));
    }
}
