//! §5.15 File history, and the plumbing shared with blame ([`super::blame`]): the file a
//! command acts on ([`FileTarget`]), the right-click **File history** / **Blame** / **Open at
//! this commit** items every file list shows, the "focused file" behind `b` and `Mod+Shift+H`,
//! and the worker replies of both views.
//!
//! File history replaces the commit details below the graph with the file's commits
//! (`git log --follow`, renames noted inline); a row shows the file's diff at that commit. The
//! header has **Open at this commit** (the blob via `git show <rev>:<path>`, written to a temp
//! file and opened in the default app — remote repos too, the bytes come over ssh) and
//! **Blame**. Selecting another commit in the graph leaves the history.

use super::{GitEvent, GitPane, Selection, worker};
use crate::git::cmd::GitError;
use crate::git::diff::FileDiff;
use crate::git::history::{self as githistory, HistoryEntry};
use crate::git::log::{self as gitlog, Commit};
use crate::model::blame::short_rev;
use crate::model::state::GitView;
use crate::model::theme::Token;
use crate::ui::chrome::Toast;
use crate::ui::jobs;
use crate::ui::theme::Colors;
use std::collections::HashMap;

/// A file at a revision: what **File history**, **Blame**, and **Open at this commit** act on.
/// `rev: None` is the working tree; `line` (1-based) is where blame opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileTarget {
    pub rev: Option<String>,
    pub path: String,
    pub line: Option<u32>,
}

impl FileTarget {
    pub fn worktree(path: &str) -> Self {
        Self { rev: None, path: path.to_string(), line: None }
    }

    pub fn at(rev: &str, path: &str) -> Self {
        Self { rev: Some(rev.to_string()), path: path.to_string(), line: None }
    }

    /// A file row of commit `id`'s change list: a deleted file only exists in the parent.
    pub fn in_commit(id: &str, path: &str, status: char) -> Self {
        if status == 'D' { Self::at(&format!("{id}^"), path) } else { Self::at(id, path) }
    }
}

/// What a file row's **Blame** and **File history** act on: `blame` is also what **Open at
/// this commit** opens; `history` is the file's name and the commit its log starts at (`None`
/// rev: `HEAD`). On Changes rows the status decides (see [`changes_file`]); `None` hides the
/// item (and `b` / `Mod+Shift+H` say it's unavailable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileRow {
    pub blame: Option<FileTarget>,
    pub history: Option<FileTarget>,
}

impl From<FileTarget> for FileRow {
    fn from(t: FileTarget) -> Self {
        Self { history: Some(t.clone()), blame: Some(t) }
    }
}

impl FileRow {
    /// A file row of commit `id`'s change list: blamed (and opened) where the file exists —
    /// the parent for a deletion — and its history followed from `id` itself, under the name
    /// it has there, so a later rename doesn't cut it short.
    pub fn in_commit(id: &str, path: &str, status: char) -> Self {
        Self { blame: Some(FileTarget::in_commit(id, path, status)), history: Some(FileTarget::at(id, path)) }
    }
}

/// A Changes row's file items by its status: git can't blame a file that isn't on disk or
/// isn't tracked, and `git log --follow` from `HEAD` knows a file only by its committed name.
/// - untracked / ignored / conflicted: nothing;
/// - deleted (staged or not): blame and history at `HEAD`, where it still exists;
/// - staged rename or copy: blame the working file (git follows it), history of the old name;
///   deleted from the working tree afterwards: blame and history of the old name at `HEAD`;
/// - staged new file: blame (every line uncommitted), no history yet;
/// - otherwise: the working tree, and the file's history.
pub(super) fn changes_file(e: &crate::git::status::Entry) -> FileRow {
    use crate::git::status::EntryKind as K;
    let none = FileRow { blame: None, history: None };
    match &e.kind {
        K::Untracked | K::Ignored | K::Unmerged => none,
        _ if e.worktree == 'D' && e.index == 'A' => none,
        K::Renamed { from } | K::Copied { from } if e.worktree == 'D' => FileTarget::at("HEAD", from).into(),
        _ if e.worktree == 'D' || e.index == 'D' => FileTarget::at("HEAD", &e.path).into(),
        K::Renamed { from } | K::Copied { from } => {
            FileRow { blame: Some(FileTarget::worktree(&e.path)), history: Some(FileTarget::worktree(from)) }
        }
        _ if e.index == 'A' => FileRow { blame: Some(FileTarget::worktree(&e.path)), history: None },
        K::Ordinary => FileTarget::worktree(&e.path).into(),
    }
}

/// A Changes row's context-menu items (see [`changes_file`]).
pub(super) fn changes_menu_items(ui: &mut egui::Ui, row: &FileRow) -> Option<FileViewRequest> {
    let mut chosen = None;
    if let Some(t) = &row.history
        && ui.button("File history").clicked()
    {
        chosen = Some(FileViewRequest::History(t.clone()));
    }
    if let Some(t) = &row.blame
        && ui.button("Blame").clicked()
    {
        chosen = Some(FileViewRequest::Blame(t.clone()));
    }
    if chosen.is_some() {
        ui.close();
    }
    chosen
}

/// A right-click / key choice on a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FileViewRequest {
    History(FileTarget),
    Blame(FileTarget),
    OpenAt(FileTarget),
}

/// A diff line chosen with **Blame this line** (§5.6): its number on the new side, or — for a
/// removed line — on the old side (blamed in the parent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiffLine {
    New(u32),
    Old(u32),
}

/// The blame target of `line` in `file`'s diff at commit `rev`.
pub(super) fn line_target(rev: &str, file: &FileDiff, line: DiffLine) -> Option<FileTarget> {
    let (rev, path, n) = match line {
        DiffLine::New(n) => (rev.to_string(), file.new_path.clone()?, n),
        DiffLine::Old(n) => (format!("{rev}^"), file.old_path.clone()?, n),
    };
    Some(FileTarget { rev: Some(rev), path, line: Some(n) })
}

/// Replies of the file-history and blame jobs, carried in `Reply::FileView`.
pub(super) enum FileViewReply {
    History {
        req: u64,
        result: Result<Vec<HistoryEntry>, GitError>,
    },
    Diff {
        req: u64,
        result: Result<Vec<FileDiff>, GitError>,
    },
    Blame {
        req: u64,
        result: Result<crate::git::blame::Blame, GitError>,
    },
    /// A commit's full message, for the blame gutter's hover.
    Message {
        id: String,
        result: Result<(String, String), GitError>,
    },
    /// A commit further back than the graph has loaded, selected from blame.
    Commit {
        id: String,
        result: Result<Vec<Commit>, GitError>,
    },
    Opened(Result<(), String>),
}

pub(super) struct HistoryState {
    pub path: String,
    /// The commit the log starts at; `None` is `HEAD`.
    pub rev: Option<String>,
    pub entries: Option<Vec<HistoryEntry>>,
    pub req: u64,
    pub selected: Option<usize>,
    pub diff: Vec<FileDiff>,
    pub diff_loading: bool,
    pub diff_req: u64,
    pub diff_line_limit: usize,
    /// The graph selection when the history opened: selecting another commit leaves it.
    pub opened_on: Selection,
}

/// A cached blame's (rev, path); `None` rev is the working tree.
pub(super) type BlameKey = (Option<String>, String);

/// File history, blame, and the focused file.
#[derive(Default)]
pub(super) struct FileViews {
    pub history: Option<HistoryState>,
    pub blame: Option<super::blame::BlameState>,
    /// The file row that has keyboard focus (`b`, `Mod+Shift+H`), by its widget id.
    pub focus: Option<(egui::Id, FileRow)>,
    /// Full commit messages for the blame hover; `None` while loading.
    pub messages: HashMap<String, Option<String>>,
    /// Recent blames by (rev, path), newest last (§8 "cached per (commit, path)").
    pub cache: Vec<(BlameKey, std::sync::Arc<crate::git::blame::Blame>)>,
    /// Events raised outside `show` (palette actions), handed out on the next frame.
    pub pending: Vec<GitEvent>,
}

/// Draws **File history**, **Blame**, and (at a commit) **Open at this commit** into an open
/// context menu; returns the chosen one.
pub(super) fn file_menu_items(ui: &mut egui::Ui, row: &FileRow) -> Option<FileViewRequest> {
    let mut chosen = changes_menu_items(ui, row);
    if let Some(t) = row.blame.as_ref().filter(|t| t.rev.is_some())
        && ui.button("Open at this commit").clicked()
    {
        chosen = Some(FileViewRequest::OpenAt(t.clone()));
        ui.close();
    }
    chosen
}

/// The row's right-click menu with just the file items.
pub(super) fn file_menu(resp: &egui::Response, row: &FileRow) -> Option<FileViewRequest> {
    let mut chosen = None;
    resp.context_menu(|ui| chosen = file_menu_items(ui, row));
    chosen
}

impl GitPane {
    /// Runs a file request from a menu, a key, or the palette.
    pub(super) fn open_file_view(&mut self, ctx: &egui::Context, request: FileViewRequest) {
        match request {
            FileViewRequest::History(t) => self.open_history(ctx, t),
            FileViewRequest::Blame(t) => self.open_blame(ctx, t),
            FileViewRequest::OpenAt(t) => self.open_at_commit(ctx, t),
        }
    }

    /// A file row drew this frame: a click gives it keyboard focus and makes it the file `b`
    /// and `Mod+Shift+H` act on.
    pub(super) fn note_file_focus(&mut self, resp: &egui::Response, target: impl Into<FileRow>) {
        if resp.clicked() || resp.secondary_clicked() {
            resp.request_focus();
            self.file_views.focus = Some((resp.id, target.into()));
        }
    }

    /// The file `Mod+Shift+H` / palette **File History** / **Blame File** act on: the focused
    /// row, else what the current view has selected.
    fn focused_file(&self) -> Option<FileRow> {
        if let Some((id, target)) = &self.file_views.focus
            && self.ctx.memory(|m| m.has_focus(*id))
        {
            return Some(target.clone());
        }
        if let Some(b) = &self.file_views.blame {
            let f = b.stack.current();
            return Some(FileTarget { rev: f.rev.clone(), path: f.path.clone(), line: None }.into());
        }
        match self.view {
            GitView::Changes => {
                let (_, path) = self.changes.selected.as_ref()?;
                self.status.entries.iter().find(|e| &e.path == path).map(changes_file)
            }
            _ => {
                if let Some(h) = &self.file_views.history
                    && let Some(e) = h.selected.and_then(|i| h.entries.as_ref()?.get(i))
                {
                    return Some(entry_row(e));
                }
                let d = self.details.as_ref().filter(|d| d.stash.is_none())?;
                let path = d.selected_file.as_ref()?;
                let status =
                    d.files.as_ref().and_then(|fs| fs.iter().find(|f| &f.path == path)).map(|f| f.status);
                Some(FileRow::in_commit(&d.commit.id, path, status.unwrap_or('M')))
            }
        }
    }

    /// Palette / `Mod+Shift+H`: **File History** or **Blame File** on the focused file.
    /// Returns whether the action was one of these.
    pub fn file_action(&mut self, ctx: &egui::Context, action: crate::model::keymap::Action) -> bool {
        use crate::model::keymap::Action as A;
        if !matches!(action, A::FileHistoryOfFocusedFile | A::BlameFocusedFile) {
            return false;
        }
        let Some(row) = self.focused_file() else {
            let hint = "Select a file first (Changes, or a commit's file list)";
            self.file_views.pending.push(GitEvent::Toast(Toast::info(hint)));
            return true;
        };
        match (action == A::FileHistoryOfFocusedFile, row) {
            (true, FileRow { history: Some(t), .. }) => self.open_history(ctx, t),
            (false, FileRow { blame: Some(t), .. }) => self.open_blame(ctx, t),
            (history, _) => {
                let what = if history { "File history" } else { "Blame" };
                let msg = format!("{what} isn't available for this file (untracked, or not committed yet)");
                self.file_views.pending.push(GitEvent::Toast(Toast::info(msg)));
            }
        }
        true
    }

    /// Once per frame after the views: `b` on a focused file row (§7.5), and queued events.
    pub(super) fn file_view_frame(&mut self, ui: &egui::Ui, events: &mut Vec<GitEvent>) {
        events.append(&mut self.file_views.pending);
        let Some((id, FileRow { blame: Some(target), .. })) = self.file_views.focus.clone() else { return };
        if !ui.memory(|m| m.has_focus(id)) {
            return;
        }
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::B)) {
            let ctx = ui.ctx().clone();
            self.open_blame(&ctx, target);
        }
    }

    // ---- file history -----------------------------------------------------------------------

    /// **File history** of `target.path` from `target.rev` (`HEAD` when `None`): the details
    /// below the graph become its commit list.
    pub(super) fn open_history(&mut self, ctx: &egui::Context, target: FileTarget) {
        self.close_blame();
        self.view = GitView::Graph;
        let req = self.next_req_id();
        let FileTarget { rev, path, .. } = target;
        self.file_views.history = Some(HistoryState {
            path: path.clone(),
            rev: rev.clone(),
            entries: None,
            req,
            selected: None,
            diff: Vec::new(),
            diff_loading: false,
            diff_req: 0,
            diff_line_limit: super::details::DIFF_LINE_PAGE,
            opened_on: self.selected.clone(),
        });
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let result = githistory::run(&git, rev.as_deref(), &path, githistory::HISTORY_LIMIT);
            worker::Reply::FileView(FileViewReply::History { req, result })
        });
    }

    fn select_history_row(&mut self, ctx: &egui::Context, index: usize) {
        let req = self.next_req_id();
        let Some(h) = &mut self.file_views.history else { return };
        let Some(entry) = h.entries.as_ref().and_then(|e| e.get(index)).cloned() else { return };
        h.selected = Some(index);
        h.diff_loading = true;
        h.diff_req = req;
        h.diff_line_limit = super::details::DIFF_LINE_PAGE;
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let result = githistory::entry_diff(&git, &entry);
            worker::Reply::FileView(FileViewReply::Diff { req, result })
        });
    }

    /// The Graph view's details area: a resizable panel at the bottom of the pane, the graph
    /// above it, holding the selected commit's details — or, while one is open, a file's history.
    pub(super) fn show_graph_bottom_panel(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        now: u64,
        ctx: &egui::Context,
    ) {
        if self.view != GitView::Graph
            || (self.details.is_none() && self.file_views.history.is_none() && !self.selection_mode_active())
        {
            return;
        }
        let height = ui.available_height();
        egui::Panel::bottom(egui::Id::new("amalgum_graph_details"))
            .resizable(true)
            .default_size(height * 0.55)
            .min_size(120.0)
            .max_size(height * 0.9)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("graph_details_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if !self.show_file_history(ui, colors, now, ctx)
                            && !self.show_selection_details(ui, colors, now)
                        {
                            self.draw_details(ui, colors, now, ctx);
                        }
                    });
            });
    }

    /// Draws the history; `false` when no history is open (or the graph selection moved on,
    /// which closes it).
    pub(super) fn show_file_history(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        now: u64,
        ctx: &egui::Context,
    ) -> bool {
        let Some(h) = &self.file_views.history else { return false };
        if h.opened_on != self.selected {
            self.file_views.history = None;
            return false;
        }
        let title = match &h.rev {
            Some(rev) => format!("History of {} from {}", h.path, worker::short_hash(rev)),
            None => format!("History of {}", h.path),
        };
        // Moved out for drawing (not cloned: a history holds up to 2000 rows and a diff can be
        // a whole large file) and put back before acting.
        let Some(h) = &mut self.file_views.history else { return false };
        let entries = h.entries.take();
        let diff = std::mem::take(&mut h.diff);
        let selected = h.selected;
        let (diff_loading, limit) = (h.diff_loading, h.diff_line_limit);
        let current_entry =
            selected.and_then(|i| entries.as_ref()?.get(i)).or(entries.as_ref().and_then(|e| e.first()));
        let current = current_entry.map(entry_target);

        enum Act {
            Close,
            Row(usize),
            File(FileViewRequest),
            LoadMore,
        }
        let mut act = None;
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(title).strong());
            if let Some(n) = entries.as_ref().map(Vec::len) {
                let more = if n >= githistory::HISTORY_LIMIT { "+" } else { "" };
                ui.weak(format!("{n}{more} commits"));
            }
            let hint = "the selected row's commit (the newest when none is selected)";
            if ui
                .add_enabled(current.is_some(), egui::Button::new("Open at this commit"))
                .on_hover_text(format!("Open the file as of {hint}"))
                .clicked()
                && let Some(t) = &current
            {
                act = Some(Act::File(FileViewRequest::OpenAt(t.clone())));
            }
            if ui
                .add_enabled(current.is_some(), egui::Button::new("Blame"))
                .on_hover_text(format!("Blame the file at {hint}"))
                .clicked()
                && let Some(t) = &current
            {
                act = Some(Act::File(FileViewRequest::Blame(t.clone())));
            }
            if ui.button("✕").on_hover_text("Close file history").clicked() {
                act = Some(Act::Close);
            }
        });
        ui.separator();
        match &entries {
            None => {
                ui.weak("Loading history…");
            }
            Some(list) if list.is_empty() => {
                ui.weak("No commits touch this file.");
            }
            Some(list) => {
                let row_h = 20.0;
                egui::ScrollArea::vertical().id_salt("file_history_rows").max_height(220.0).show_rows(
                    ui,
                    row_h,
                    list.len(),
                    |ui, range| {
                        for i in range {
                            let e = &list[i];
                            let resp = history_row(ui, colors, e, selected == Some(i), now, row_h);
                            if resp.clicked() {
                                act = Some(Act::Row(i));
                            }
                            if let Some(r) = file_menu(&resp, &entry_row(e)) {
                                act = Some(Act::File(r));
                            }
                        }
                    },
                );
            }
        }
        ui.separator();
        if diff_loading {
            ui.weak("Loading diff…");
        } else if selected.is_none() {
            ui.weak("Select a commit to see this file's diff at that commit.");
        } else {
            let mut want_more = false;
            for file in &diff {
                if let Some(line) = super::details::render_diff_file(ui, colors, file, limit, &mut want_more)
                    && let Some(t) = current_entry.and_then(|e| entry_line_target(e, file, line))
                {
                    act = Some(Act::File(FileViewRequest::Blame(t)));
                }
                if file.hunks.is_empty() && !file.binary {
                    ui.weak("No content changes.");
                }
            }
            if diff.is_empty() {
                ui.weak("No changes to this file in this commit (a merge that kept one side).");
            }
            if want_more {
                act = Some(Act::LoadMore);
            }
        }

        if let Some(h) = &mut self.file_views.history {
            h.entries = entries;
            h.diff = diff;
        }
        match act {
            Some(Act::Close) => self.file_views.history = None,
            Some(Act::Row(i)) => self.select_history_row(ctx, i),
            Some(Act::File(r)) => self.open_file_view(ctx, r),
            Some(Act::LoadMore) => {
                if let Some(h) = &mut self.file_views.history {
                    h.diff_line_limit += super::details::DIFF_LINE_PAGE;
                }
            }
            None => {}
        }
        true
    }

    // ---- open at this commit ----------------------------------------------------------------

    /// **Open at this commit**: `git show <rev>:<path>` on a worker (over ssh for a remote repo),
    /// written to `<cache>/blobs/<short hash>/<file name>` (the app's per-user cache directory,
    /// not the shared temp dir) and opened in the default app.
    pub(super) fn open_at_commit(&mut self, ctx: &egui::Context, target: FileTarget) {
        let Some(rev) = target.rev else {
            self.file_views.pending.push(GitEvent::Toast(Toast::info("Pick a commit to open the file at")));
            return;
        };
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let args = githistory::show_blob_args(&rev, &target.path);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let result = git.run(&args).map_err(|e| e.to_string()).and_then(|bytes| {
                let (dir, name) = githistory::blob_file_name(&rev, &target.path);
                let file = write_blob(&dir, &name, &bytes)?;
                crate::platform::open_in_default_app(&file.display().to_string()).map_err(|e| e.to_string())
            });
            worker::Reply::FileView(FileViewReply::Opened(result))
        });
    }

    // ---- selecting a commit from blame ---------------------------------------------------------

    /// Blame's gutter click (§5.15 "Click selects that commit in the graph"): back to the graph
    /// with the commit selected and scrolled to, its details below.
    pub(super) fn select_commit_in_graph(&mut self, ctx: &egui::Context, id: String) {
        self.close_blame();
        self.file_views.history = None;
        self.view = GitView::Graph;
        self.selected = Selection::Commit(id.clone());
        if let Some(row) = self.commits.iter().position(|c| c.id == id) {
            self.search.scroll_to = Some(row);
            self.load_details_for_selected(ctx);
            return;
        }
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let args = gitlog::log_args(&["-1", &id, "--"]);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let result = git.run(&args).map(|o| gitlog::parse_log(&o));
            worker::Reply::FileView(FileViewReply::Commit { id, result })
        });
    }

    /// Fetch a commit's full message for the blame hover, once.
    pub(super) fn request_message(&mut self, ctx: &egui::Context, id: &str) {
        if self.file_views.messages.contains_key(id) {
            return;
        }
        self.file_views.messages.insert(id.to_string(), None);
        let (git, id) = (self.git.clone(), id.to_string());
        jobs::spawn(ctx, &self.tx, move || {
            let result = git
                .run(&gitlog::message_args(&id))
                .map(|o| gitlog::split_message(&String::from_utf8_lossy(&o)));
            worker::Reply::FileView(FileViewReply::Message { id, result })
        });
    }

    // ---- replies --------------------------------------------------------------------------

    pub(super) fn apply_file_view_reply(
        &mut self,
        ctx: &egui::Context,
        reply: FileViewReply,
        events: &mut Vec<GitEvent>,
    ) {
        match reply {
            FileViewReply::History { req, result } => {
                let Some(h) = &mut self.file_views.history else { return };
                if h.req != req {
                    return;
                }
                match result {
                    Ok(entries) => {
                        let first = !entries.is_empty();
                        h.entries = Some(entries);
                        if first {
                            self.select_history_row(ctx, 0);
                        }
                    }
                    Err(e) => {
                        h.entries = Some(Vec::new());
                        events.push(GitEvent::Toast(Toast::error(
                            "Could not read file history",
                            e.to_string(),
                        )));
                    }
                }
            }
            FileViewReply::Diff { req, result } => {
                let Some(h) = &mut self.file_views.history else { return };
                if h.diff_req != req {
                    return;
                }
                h.diff_loading = false;
                match result {
                    Ok(diff) => h.diff = diff,
                    Err(e) => {
                        events.push(GitEvent::Toast(Toast::error("Could not read diff", e.to_string())))
                    }
                }
            }
            FileViewReply::Blame { req, result } => self.apply_blame(req, result, events),
            FileViewReply::Message { id, result } => {
                let message = match result {
                    Ok((subject, body)) if body.is_empty() => subject,
                    Ok((subject, body)) => format!("{subject}\n\n{body}"),
                    Err(e) => e.summary(),
                };
                self.file_views.messages.insert(id, Some(message));
            }
            FileViewReply::Commit { id, result } => {
                if self.selected != Selection::Commit(id.clone()) {
                    return;
                }
                match result.map(|c| c.into_iter().next()) {
                    Ok(Some(commit)) => self.show_commit_details(ctx, commit),
                    Ok(None) => events.push(GitEvent::Toast(Toast::info(format!(
                        "Commit {} is not in this repository",
                        short_rev(&id)
                    )))),
                    Err(e) => {
                        events.push(GitEvent::Toast(Toast::error("Could not read commit", e.to_string())))
                    }
                }
            }
            FileViewReply::Opened(Ok(())) => {}
            FileViewReply::Opened(Err(e)) => {
                events.push(GitEvent::Toast(Toast::error("Could not open file", e)))
            }
        }
    }
}

/// One history row: short hash, subject, rename note, author, relative date.
fn history_row(
    ui: &mut egui::Ui,
    colors: &Colors,
    e: &HistoryEntry,
    selected: bool,
    now: u64,
    row_h: f32,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
    let painter = ui.painter_at(rect);
    if selected {
        painter.rect_filled(rect, 0.0, colors.get(Token::BgSelected));
    } else if resp.hovered() {
        painter.rect_filled(rect, 0.0, colors.get(Token::BgHover));
    }
    let y = rect.center().y;
    let small = egui::FontId::proportional(11.0);
    painter.text(
        egui::pos2(rect.left() + 4.0, y),
        egui::Align2::LEFT_CENTER,
        worker::short_hash(&e.commit.id),
        egui::FontId::monospace(11.0),
        colors.get(Token::FgSecondary),
    );
    let right_w = (rect.width() * 0.25).clamp(60.0, 160.0);
    let text_right = (rect.right() - right_w).max(rect.left() + 70.0);
    let mut job = egui::text::LayoutJob::default();
    // The note goes first so a narrow pane truncates the subject, never the rename.
    if let Some(old) = e.renamed_from() {
        let note = egui::TextFormat::simple(small.clone(), colors.get(Token::Warning));
        let verb = if e.change.as_ref().is_some_and(|c| c.status == 'C') { "copied" } else { "renamed" };
        job.append(&format!("{verb} from {old}"), 0.0, note);
    } else if let Some(c) = &e.change
        && matches!(c.status, 'A' | 'D')
    {
        let label = if c.status == 'A' { "added" } else { "deleted" };
        job.append(label, 0.0, egui::TextFormat::simple(small.clone(), colors.get(Token::FgSecondary)));
    }
    let body = egui::TextFormat::simple(egui::FontId::proportional(13.0), colors.get(Token::FgPrimary));
    let gap = if job.sections.is_empty() { 0.0 } else { 8.0 };
    job.append(&e.commit.subject, gap, body);
    job.wrap = egui::text::TextWrapping::truncate_at_width((text_right - rect.left() - 74.0).max(0.0));
    let galley = painter.layout_job(job);
    painter.galley(
        egui::pos2(rect.left() + 70.0, y - galley.size().y / 2.0),
        galley,
        colors.get(Token::FgPrimary),
    );
    let who = format!("{} · {}", e.commit.author, crate::util::relative_time(now, e.commit.time));
    let mut job = egui::text::LayoutJob::single_section(
        who,
        egui::TextFormat::simple(small, colors.get(Token::FgSecondary)),
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(right_w - 8.0);
    let galley = painter.layout_job(job);
    painter.galley(
        egui::pos2(rect.right() - galley.size().x - 4.0, y - galley.size().y / 2.0),
        galley,
        colors.get(Token::FgSecondary),
    );
    let hover = match e.renamed_from() {
        Some(old) => format!("{} → {}", old, e.path),
        None => e.path.clone(),
    };
    resp.on_hover_text(format!("{}\n{}", e.commit.subject, hover))
}

/// The file as a history row shows it: at the row's commit, or at its parent when the row is
/// the commit that deleted the file (it no longer exists there).
fn entry_target(e: &HistoryEntry) -> FileTarget {
    FileTarget::in_commit(&e.commit.id, &e.path, entry_status(e))
}

/// A history row's file items (see [`FileRow::in_commit`]).
fn entry_row(e: &HistoryEntry) -> FileRow {
    FileRow::in_commit(&e.commit.id, &e.path, entry_status(e))
}

fn entry_status(e: &HistoryEntry) -> char {
    e.change.as_ref().map_or('M', |c| c.status)
}

/// **Blame this line** in a history row's diff: the diff is the row's commit against its
/// parent, so it is the commit id — not [`entry_target`], which is already the parent for a
/// deletion — that a removed line is blamed in the parent of.
fn entry_line_target(e: &HistoryEntry, file: &FileDiff, line: DiffLine) -> Option<FileTarget> {
    line_target(&e.commit.id, file, line)
}

/// Write an **Open at this commit** blob under the app's per-user cache directory. A file left
/// there earlier (or anything planted in its place, a symlink included) is removed first and the
/// new one created exclusively, so the write never follows a link.
fn write_blob(dir: &str, name: &str, bytes: &[u8]) -> Result<std::path::PathBuf, String> {
    use std::io::Write as _;
    let dirs = crate::paths::Dirs::discover().ok_or("No cache directory for this user")?;
    let dir = dirs.cache.join("blobs").join(dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let file = dir.join(name);
    match std::fs::remove_file(&file) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", file.display())),
    }
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file)
        .map_err(|e| format!("{}: {e}", file.display()))?;
    out.write_all(bytes).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_rows_target_what_git_can_blame_and_follow() {
        use crate::git::status::{Entry, EntryKind as K};
        let e = |kind, index, worktree| changes_file(&Entry { path: "p".into(), kind, index, worktree });
        let none = FileRow { blame: None, history: None };
        assert_eq!(e(K::Ordinary, '.', 'M'), FileTarget::worktree("p").into());
        assert_eq!(e(K::Untracked, '?', '?'), none);
        assert_eq!(e(K::Ordinary, 'A', 'D'), none);
        let deleted = FileTarget::at("HEAD", "p").into();
        assert_eq!(e(K::Ordinary, '.', 'D'), deleted);
        assert_eq!(e(K::Ordinary, 'D', '.'), deleted);
        assert_eq!(
            e(K::Renamed { from: "old".into() }, 'R', '.'),
            FileRow { blame: Some(FileTarget::worktree("p")), history: Some(FileTarget::worktree("old")) }
        );
        // Renamed in the index, then deleted from the working tree: only the old name is
        // anywhere git can read it.
        assert_eq!(e(K::Renamed { from: "old".into() }, 'R', 'D'), FileTarget::at("HEAD", "old").into());
        assert_eq!(
            e(K::Ordinary, 'A', '.'),
            FileRow { blame: Some(FileTarget::worktree("p")), history: None }
        );
    }

    #[test]
    fn a_deleting_history_row_targets_the_parent() {
        let mut repo = crate::testutil::TempRepo::new();
        repo.commit_file("f", "x\n", "add");
        repo.git(&["rm", "-q", "f"]);
        let del = repo.commit("delete");
        let git = crate::git::Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });
        let entries = githistory::run(&git, None, "f", 10).unwrap();
        assert_eq!(entry_target(&entries[0]), FileTarget::at(&format!("{del}^"), "f"));
        // Its history starts at the deleting commit itself, so the deletion stays listed.
        assert_eq!(entry_row(&entries[0]).history, Some(FileTarget::at(&del, "f")));
    }

    #[test]
    fn blame_this_line_in_a_deleting_rows_diff_blames_the_commits_parent() {
        let mut repo = crate::testutil::TempRepo::new();
        repo.commit_file("base", "b\n", "root");
        let add = repo.commit_file("f", "one\ntwo\n", "add");
        repo.git(&["rm", "-q", "f"]);
        let del = repo.commit("delete");
        let git = crate::git::Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });
        let entries = githistory::run(&git, None, "f", 10).unwrap();
        let diff = githistory::entry_diff(&git, &entries[0]).unwrap();
        assert_eq!(diff.len(), 1);
        let t = entry_line_target(&entries[0], &diff[0], DiffLine::Old(2)).expect("old side");
        assert_eq!(t, FileTarget { rev: Some(format!("{del}^")), path: "f".into(), line: Some(2) });
        // That revision has the line, and it comes from the commit that added it.
        let blame = crate::git::blame::run(&git, t.rev.as_deref(), &t.path).expect("blame");
        assert_eq!(blame.commit_of(1).map(|c| c.id.as_str()), Some(add.as_str()));
    }

    fn diff_file(old: Option<&str>, new: Option<&str>) -> FileDiff {
        FileDiff {
            old_path: old.map(str::to_string),
            new_path: new.map(str::to_string),
            header: Vec::new(),
            binary: false,
            hunks: Vec::new(),
        }
    }

    #[test]
    fn diff_lines_blame_at_the_commit_or_its_parent() {
        let f = diff_file(Some("old.rs"), Some("new.rs"));
        assert_eq!(
            line_target("abc", &f, DiffLine::New(4)),
            Some(FileTarget { rev: Some("abc".into()), path: "new.rs".into(), line: Some(4) })
        );
        assert_eq!(
            line_target("abc", &f, DiffLine::Old(2)),
            Some(FileTarget { rev: Some("abc^".into()), path: "old.rs".into(), line: Some(2) })
        );
        assert_eq!(
            line_target("abc", &diff_file(None, Some("n")), DiffLine::Old(1)),
            None,
            "an added file has no old side"
        );
    }

    #[test]
    fn deleted_files_are_targeted_in_the_parent() {
        assert_eq!(FileTarget::in_commit("abc", "a", 'D').rev.as_deref(), Some("abc^"));
        assert_eq!(FileTarget::in_commit("abc", "a", 'M').rev.as_deref(), Some("abc"));
        assert_eq!(FileTarget::worktree("a").rev, None);
    }

    #[test]
    fn a_history_closes_when_the_graph_selection_moves() {
        let mut pane = super::super::tests::new_test_pane();
        let ctx = egui::Context::default();
        pane.file_views.history = Some(HistoryState {
            path: "f".into(),
            rev: None,
            entries: Some(Vec::new()),
            req: 1,
            selected: None,
            diff: Vec::new(),
            diff_loading: false,
            diff_req: 0,
            diff_line_limit: 10,
            opened_on: Selection::None,
        });
        let colors = Colors::new(crate::model::theme::Mode::Dark);
        let mut shown = Vec::new();
        for moved in [false, true] {
            if moved {
                pane.selected = Selection::Commit("x".into());
            }
            let mut output =
                ctx.run_ui(Default::default(), |ui| shown.push(pane.show_file_history(ui, &colors, 0, &ctx)));
            output.textures_delta.clear(); // no renderer to apply the font atlas upload
        }
        assert_eq!(shown, [true, false]);
        assert!(pane.file_views.history.is_none());
    }

    #[test]
    fn palette_file_actions_need_a_file() {
        use crate::model::keymap::Action;
        let mut pane = super::super::tests::new_test_pane();
        let ctx = egui::Context::default();
        assert!(!pane.file_action(&ctx, Action::Push));
        assert!(pane.file_action(&ctx, Action::BlameFocusedFile));
        assert!(matches!(pane.file_views.pending.as_slice(), [GitEvent::Toast(_)]));
    }

    #[test]
    fn mod_shift_h_opens_the_history_of_the_selected_changes_file_in_the_graph_view() {
        use crate::model::keymap::Action;
        let mut pane = super::super::tests::new_test_pane();
        let ctx = egui::Context::default();
        pane.view = GitView::Changes;
        pane.changes.selected = Some((super::super::changes::Side::Unstaged, "src/a b.rs".into()));
        pane.status.entries.push(crate::git::status::Entry {
            path: "src/a b.rs".into(),
            kind: crate::git::status::EntryKind::Ordinary,
            index: '.',
            worktree: 'M',
        });
        assert!(pane.file_action(&ctx, Action::FileHistoryOfFocusedFile));
        assert_eq!(pane.view, GitView::Graph);
        assert_eq!(pane.file_views.history.as_ref().map(|h| h.path.as_str()), Some("src/a b.rs"));

        // Blame File on the same selection opens blame on the working tree and maximizes.
        pane.view = GitView::Changes;
        assert!(pane.file_action(&ctx, Action::BlameFocusedFile));
        let frame = pane.file_views.blame.as_ref().expect("blame open").stack.current().clone();
        assert_eq!((frame.rev, frame.path.as_str()), (None, "src/a b.rs"));
        assert_eq!(pane.file_views.pending, [GitEvent::MaximizePane(true)]);
    }
}
