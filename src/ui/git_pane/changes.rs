//! The Changes view (§5.7, W5): Unstaged / Staged / Conflicts lists with checkboxes, the commit
//! editor, and the interactive diff (stage/unstage/discard per hunk) of the focused file.

use super::worker;
use super::{GitEvent, GitPane};
use crate::git::cmd::GitError;
use crate::git::diff::{self, FileDiff, Selection as HunkSelection};
use crate::git::log::{message_args, split_message};
use crate::git::message::{self, SubjectLen};
use crate::git::status::{Entry, EntryKind, Status};
use crate::model::settings::Settings;
use crate::model::theme::Token;
use crate::ui::chrome::Toast;
use crate::ui::theme::Colors;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Unstaged,
    Staged,
    Conflict,
}

#[derive(Debug, Clone)]
pub(super) struct PendingDiscard {
    pub path: String,
    pub hunk: usize,
}

#[derive(Default)]
pub(super) struct ChangesState {
    pub selected: Option<(Side, String)>,
    pub diff: Vec<FileDiff>,
    pub diff_loading: bool,
    pub diff_req: u64,
    pub subject: String,
    pub body: String,
    pub amend: bool,
    pub committing: bool,
    pub pending_discard: Option<PendingDiscard>,
}

/// Splits `status.entries` into (conflicts, unstaged, staged) for the three list sections
/// (§5.7, §5.17 "Conflicted files sit in a Conflicts section above Unstaged").
pub(super) fn split_status(status: &Status) -> (Vec<&Entry>, Vec<&Entry>, Vec<&Entry>) {
    let mut conflicts = Vec::new();
    let mut unstaged = Vec::new();
    let mut staged = Vec::new();
    for e in &status.entries {
        if e.is_conflicted() {
            conflicts.push(e);
        }
        if e.is_unstaged() {
            unstaged.push(e);
        }
        if e.is_staged() {
            staged.push(e);
        }
    }
    (conflicts, unstaged, staged)
}

/// Title and body for the "Discard hunk" confirmation (§5.20).
pub(super) fn discard_confirm_text(path: &str) -> (String, String) {
    (
        format!("Discard changes to `{path}`?"),
        "This hunk's changes will be lost. This cannot be undone.".to_string(),
    )
}

enum ChangesAction {
    Stage(String),
    Unstage(String),
    StageAll,
    UnstageAll,
    SelectFile(Side, String),
    /// §5.7 file menu: type the path into the focused terminal (for handing it to an agent).
    SendPath(String),
    StageHunk(usize),
    UnstageHunk(usize),
    RequestDiscardHunk(String, usize),
    ConfirmDiscard,
    CancelDiscard,
    Commit,
}

impl GitPane {
    pub(super) fn show_changes(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        _settings: &Settings,
        _now: u64,
        events: &mut Vec<GitEvent>,
        ctx: &egui::Context,
    ) {
        let (conflicts, unstaged, staged): (Vec<Entry>, Vec<Entry>, Vec<Entry>) = {
            let (c, u, s) = split_status(&self.status);
            (
                c.into_iter().cloned().collect(),
                u.into_iter().cloned().collect(),
                s.into_iter().cloned().collect(),
            )
        };
        let selected = self.changes.selected.clone();
        let mut action: Option<ChangesAction> = None;

        if conflicts.is_empty() && unstaged.is_empty() && staged.is_empty() {
            ui.horizontal(|ui| {
                ui.colored_label(colors.get(Token::Success), "✓");
                ui.label("Nothing to commit. Working tree clean.");
            });
        }

        if !conflicts.is_empty() {
            ui.label(format!("Conflicts ({})", conflicts.len()));
            for e in &conflicts {
                let selected_now = selected.as_ref() == Some(&(Side::Conflict, e.path.clone()));
                let resp = row_label(ui, colors, selected_now, &format!("! {}", e.path));
                if resp.clicked() {
                    action = Some(ChangesAction::SelectFile(Side::Conflict, e.path.clone()));
                }
            }
            ui.separator();
        }

        ui.horizontal(|ui| {
            ui.label(format!("Unstaged ({})", unstaged.len()));
            if !unstaged.is_empty() && ui.button("Stage all").clicked() {
                action = Some(ChangesAction::StageAll);
            }
        });
        for e in &unstaged {
            let mut checked = false;
            let resp = ui.horizontal(|ui| {
                let cb = ui.checkbox(&mut checked, "");
                let label_resp = row_label(
                    ui,
                    colors,
                    selected.as_ref() == Some(&(Side::Unstaged, e.path.clone())),
                    &entry_label(e, false),
                );
                (cb, label_resp)
            });
            resp.inner.1.context_menu(|ui| {
                if ui.button("Send path to terminal").clicked() {
                    action = Some(ChangesAction::SendPath(e.path.clone()));
                }
            });
            if resp.inner.0.changed() {
                action = Some(ChangesAction::Stage(e.path.clone()));
            } else if resp.inner.1.clicked() {
                action = Some(ChangesAction::SelectFile(Side::Unstaged, e.path.clone()));
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!("Staged ({})", staged.len()));
            if !staged.is_empty() && ui.button("Unstage all").clicked() {
                action = Some(ChangesAction::UnstageAll);
            }
        });
        for e in &staged {
            let mut checked = true;
            let resp = ui.horizontal(|ui| {
                let cb = ui.checkbox(&mut checked, "");
                let label_resp = row_label(
                    ui,
                    colors,
                    selected.as_ref() == Some(&(Side::Staged, e.path.clone())),
                    &entry_label(e, true),
                );
                (cb, label_resp)
            });
            resp.inner.1.context_menu(|ui| {
                if ui.button("Send path to terminal").clicked() {
                    action = Some(ChangesAction::SendPath(e.path.clone()));
                }
            });
            if resp.inner.0.changed() {
                action = Some(ChangesAction::Unstage(e.path.clone()));
            } else if resp.inner.1.clicked() {
                action = Some(ChangesAction::SelectFile(Side::Staged, e.path.clone()));
            }
        }

        ui.separator();
        let subject_len = message::subject_len(&self.changes.subject);
        let subject_color = subject_color(colors, subject_len);
        ui.add(
            egui::TextEdit::singleline(&mut self.changes.subject)
                .hint_text("Subject")
                .text_color(subject_color)
                .desired_width(f32::INFINITY),
        );
        ui.label(format!("{}/50", self.changes.subject.chars().count()));
        ui.add(
            egui::TextEdit::multiline(&mut self.changes.body)
                .hint_text("Body")
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );
        let mut amend = self.changes.amend;
        if ui.checkbox(&mut amend, "Amend").changed() {
            self.changes.amend = amend;
            if amend {
                self.dispatch_last_commit_message(ctx);
            }
        }
        let can_commit = message::can_commit(&self.changes.subject, staged.len(), self.changes.amend);
        let commit_label = if self.changes.amend { "Amend" } else { "Commit" };
        if ui.add_enabled(can_commit && !self.changes.committing, egui::Button::new(commit_label)).clicked() {
            action = Some(ChangesAction::Commit);
        }

        ui.separator();
        let diff = self.changes.diff.clone();
        let diff_loading = self.changes.diff_loading;
        let pending = self.changes.pending_discard.clone();
        if diff_loading {
            ui.weak("Loading diff…");
        } else if let Some((side, path)) = &selected {
            for file in &diff {
                let hunk_action = render_changes_diff_file(ui, colors, file, *side, &path.clone());
                match hunk_action {
                    Some(HunkAction::Stage(idx)) => action = Some(ChangesAction::StageHunk(idx)),
                    Some(HunkAction::Unstage(idx)) => action = Some(ChangesAction::UnstageHunk(idx)),
                    Some(HunkAction::Discard(idx)) => {
                        action = Some(ChangesAction::RequestDiscardHunk(path.clone(), idx))
                    }
                    None => {}
                }
            }
        }

        if let Some(p) = &pending {
            let (title, body) = discard_confirm_text(&p.path);
            let modal = egui::Modal::new(egui::Id::new("gitpane_discard_hunk")).show(ui.ctx(), |ui| {
                ui.label(egui::RichText::new(title).strong());
                ui.label(body);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        action = Some(ChangesAction::CancelDiscard);
                    }
                    if ui
                        .button(egui::RichText::new("Discard").color(colors.get(Token::FgOnAccent)))
                        .clicked()
                    {
                        action = Some(ChangesAction::ConfirmDiscard);
                    }
                });
            });
            if modal.backdrop_response.clicked() {
                action = Some(ChangesAction::CancelDiscard);
            }
        }

        if let Some(action) = action {
            self.apply_changes_action(ctx, action, events);
        }
    }

    fn apply_changes_action(
        &mut self,
        ctx: &egui::Context,
        action: ChangesAction,
        events: &mut Vec<GitEvent>,
    ) {
        match action {
            ChangesAction::Stage(path) => self.dispatch_stage_paths(ctx, vec![path], true),
            ChangesAction::Unstage(path) => self.dispatch_stage_paths(ctx, vec![path], false),
            ChangesAction::StageAll => {
                let paths: Vec<String> =
                    split_status(&self.status).1.into_iter().map(|e| e.path.clone()).collect();
                self.dispatch_stage_paths(ctx, paths, true);
            }
            ChangesAction::UnstageAll => {
                let paths: Vec<String> =
                    split_status(&self.status).2.into_iter().map(|e| e.path.clone()).collect();
                self.dispatch_stage_paths(ctx, paths, false);
            }
            ChangesAction::SelectFile(side, path) => self.dispatch_changes_diff(ctx, side, path),
            ChangesAction::StageHunk(idx) => self.dispatch_hunk(ctx, idx, HunkOp::Stage),
            ChangesAction::UnstageHunk(idx) => self.dispatch_hunk(ctx, idx, HunkOp::Unstage),
            ChangesAction::RequestDiscardHunk(path, idx) => {
                self.changes.pending_discard = Some(PendingDiscard { path, hunk: idx });
            }
            ChangesAction::ConfirmDiscard => {
                if let Some(p) = self.changes.pending_discard.clone() {
                    self.dispatch_hunk(ctx, p.hunk, HunkOp::Discard);
                }
            }
            ChangesAction::CancelDiscard => self.changes.pending_discard = None,
            ChangesAction::Commit => self.dispatch_commit(ctx),
            ChangesAction::SendPath(path) => events.push(GitEvent::SendToTerminal(path)),
        }
    }

    fn dispatch_stage_paths(&mut self, ctx: &egui::Context, paths: Vec<String>, stage: bool) {
        if paths.is_empty() {
            return;
        }
        let head_exists = self.status.branch.oid.is_some();
        let git = self.git.clone();
        let req = self.next_req_id();
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let mut result = Ok(());
            for path in &paths {
                let args: Vec<&str> = if stage {
                    vec!["add", "--", path]
                } else if head_exists {
                    vec!["restore", "--staged", "--", path]
                } else {
                    vec!["rm", "--cached", "--", path]
                };
                if let Err(e) = git.run(&args) {
                    result = Err(e);
                    break;
                }
            }
            worker::Reply::ActionDone { req, result }
        });
    }

    pub(super) fn dispatch_changes_diff(&mut self, ctx: &egui::Context, side: Side, path: String) {
        self.changes.selected = Some((side, path.clone()));
        self.changes.diff_loading = true;
        let req = self.next_req_id();
        self.changes.diff_req = req;
        let is_untracked = side == Side::Unstaged
            && self.status.entries.iter().any(|e| e.path == path && e.kind == EntryKind::Untracked);
        let git = self.git.clone();
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let result = if is_untracked {
                worker::diff_no_index(&git, &path).map(|o| diff::parse(&o))
            } else {
                let mut args = vec!["diff"];
                args.extend_from_slice(diff::DIFF_ARGS);
                if side == Side::Staged {
                    args.push("--cached");
                }
                args.push("--");
                args.push(&path);
                git.run(&args).map(|o| diff::parse(&o))
            };
            worker::Reply::ChangesDiff { req, result }
        });
    }

    fn dispatch_hunk(&mut self, ctx: &egui::Context, hunk_idx: usize, op: HunkOp) {
        let Some(file) = self.changes.diff.first().cloned() else { return };
        let reverse = op != HunkOp::Stage;
        let Some(patch) = diff::patch_for(&file, hunk_idx, &HunkSelection::WholeHunk, reverse) else {
            return;
        };
        let args: &[&str] = match op {
            HunkOp::Stage => &["apply", "--cached"],
            HunkOp::Unstage => &["apply", "--cached", "-R"],
            HunkOp::Discard => &["apply", "-R"],
        };
        let git = self.git.clone();
        let req = self.next_req_id();
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let result = git.run_with_stdin(args, &patch).map(|_| ());
            worker::Reply::ActionDone { req, result }
        });
    }

    fn dispatch_last_commit_message(&self, ctx: &egui::Context) {
        let git = self.git.clone();
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let result = git.run(&message_args("HEAD")).map(|o| split_message(&String::from_utf8_lossy(&o)));
            worker::Reply::LastCommitMessage(result)
        });
    }

    pub(super) fn dispatch_commit(&mut self, ctx: &egui::Context) {
        if self.changes.committing {
            return;
        }
        let subject = self.changes.subject.clone();
        let body = self.changes.body.clone();
        let amend = self.changes.amend;
        self.changes.committing = true;
        let git = self.git.clone();
        let now = crate::util::unix_now();
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let (before_head, before_branch) = worker::head_and_branch(&git);
            let message = message::compose(&subject, &body);
            let mut args: Vec<&str> = vec!["commit"];
            if amend {
                args.push("--amend");
            }
            args.extend(["-F", "-"]);
            let result = git.run_with_stdin(&args, message.as_bytes()).map(|_| {
                let (after_head, _) = worker::head_and_branch(&git);
                let after_head = after_head.unwrap_or_default();
                let branch_ref = before_branch.map(|b| format!("refs/heads/{b}"));
                let entry = worker::commit_entry(now, before_head, branch_ref, after_head.clone());
                worker::CommitOutcome { hash: after_head, subject: subject.clone(), entry }
            });
            worker::Reply::Committed(result)
        });
    }

    pub(super) fn apply_action_done(
        &mut self,
        ctx: &egui::Context,
        _req: u64,
        result: Result<(), GitError>,
        events: &mut Vec<GitEvent>,
    ) {
        match result {
            Ok(()) => {
                self.changes.pending_discard = None;
                let reselect = self.changes.selected.clone();
                self.refresh_now(ctx);
                if let Some((side, path)) = reselect {
                    self.dispatch_changes_diff(ctx, side, path);
                }
            }
            Err(e) => events.push(GitEvent::Toast(Toast::error(e.summary(), e.stderr.clone()))),
        }
    }

    pub(super) fn apply_changes_diff(
        &mut self,
        req: u64,
        result: Result<Vec<FileDiff>, GitError>,
        events: &mut Vec<GitEvent>,
    ) {
        if self.changes.diff_req != req {
            return;
        }
        self.changes.diff_loading = false;
        match result {
            Ok(files) => self.changes.diff = files,
            Err(e) => events.push(GitEvent::Toast(Toast::error("Could not read diff", e.to_string()))),
        }
    }

    pub(super) fn apply_last_commit_message(
        &mut self,
        result: Result<(String, String), GitError>,
        events: &mut Vec<GitEvent>,
    ) {
        match result {
            Ok((subject, body)) => {
                self.changes.subject = subject;
                self.changes.body = body;
            }
            Err(e) => events
                .push(GitEvent::Toast(Toast::error("Could not read the last commit message", e.to_string()))),
        }
    }

    pub(super) fn apply_committed(
        &mut self,
        ctx: &egui::Context,
        result: Result<worker::CommitOutcome, GitError>,
        events: &mut Vec<GitEvent>,
    ) {
        self.changes.committing = false;
        match result {
            Ok(outcome) => {
                self.journal.record(outcome.entry);
                let _ = self.journal.save(&self.journal_path);
                events.push(GitEvent::Toast(Toast::success(format!(
                    "Committed {} · {}",
                    worker::short_hash(&outcome.hash),
                    outcome.subject
                ))));
                self.changes.subject.clear();
                self.changes.body.clear();
                self.changes.amend = false;
                self.selected = super::Selection::Commit(outcome.hash.clone());
                self.view = crate::model::state::GitView::Graph;
                self.refresh_now(ctx);
            }
            Err(e) => events.push(GitEvent::Toast(Toast::error(e.summary(), e.stderr.clone()))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HunkOp {
    Stage,
    Unstage,
    Discard,
}

enum HunkAction {
    Stage(usize),
    Unstage(usize),
    Discard(usize),
}

/// The status glyph for one row: the worktree status in the Unstaged list (what changed there),
/// the index status in the Staged list (what's about to be committed).
fn entry_label(e: &Entry, staged: bool) -> String {
    let glyph = if staged { e.index } else { e.worktree };
    match &e.kind {
        EntryKind::Renamed { from } | EntryKind::Copied { from } => format!("{glyph} {from} → {}", e.path),
        EntryKind::Untracked => format!("? {}", e.path),
        _ => format!("{glyph} {}", e.path),
    }
}

fn row_label(ui: &mut egui::Ui, colors: &Colors, selected: bool, text: &str) -> egui::Response {
    let resp = ui.horizontal(|ui| {
        if selected {
            ui.painter().rect_filled(ui.available_rect_before_wrap(), 0.0, colors.get(Token::BgSelected));
        }
        ui.label(text);
    });
    ui.interact(resp.response.rect, ui.id().with(("changes_row", text)), egui::Sense::click())
}

fn subject_color(colors: &Colors, len: SubjectLen) -> egui::Color32 {
    match len {
        SubjectLen::Ok => colors.get(Token::FgPrimary),
        SubjectLen::Warning => colors.get(Token::Warning),
        SubjectLen::Danger => colors.get(Token::Danger),
    }
}

/// Renders one file's hunks with **Stage hunk** / **Unstage hunk** / **Discard hunk** buttons
/// (§5.7), and returns the one button the user clicked, if any.
fn render_changes_diff_file(
    ui: &mut egui::Ui,
    colors: &Colors,
    file: &FileDiff,
    side: Side,
    _path: &str,
) -> Option<HunkAction> {
    let title = file.new_path.clone().or_else(|| file.old_path.clone()).unwrap_or_default();
    ui.label(egui::RichText::new(title).strong());
    if file.binary {
        ui.weak("Binary file");
        return None;
    }
    let mut clicked = None;
    for (idx, hunk) in file.hunks.iter().enumerate() {
        ui.horizontal(|ui| {
            let bg = colors.get(Token::DiffHunkBg);
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width() * 0.5, 18.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 0.0, bg);
            ui.painter().text(
                rect.left_center() + egui::vec2(4.0, 0.0),
                egui::Align2::LEFT_CENTER,
                format!("@@ -{},{} +{},{} @@", hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len),
                egui::FontId::monospace(11.0),
                colors.get(Token::FgSecondary),
            );
            match side {
                Side::Unstaged => {
                    if ui.button("Stage hunk").clicked() {
                        clicked = Some(HunkAction::Stage(idx));
                    }
                    if ui.button("Discard hunk").clicked() {
                        clicked = Some(HunkAction::Discard(idx));
                    }
                }
                Side::Staged => {
                    if ui.button("Unstage hunk").clicked() {
                        clicked = Some(HunkAction::Unstage(idx));
                    }
                }
                Side::Conflict => {}
            }
        });
        let mut i = 0;
        while i < hunk.lines.len() {
            match super::details::pair_for_word_diff(&hunk.lines, i) {
                Some((d, a)) => {
                    super::details::render_line(ui, colors, &hunk.lines[d], Some(&hunk.lines[a].display()));
                    super::details::render_line(ui, colors, &hunk.lines[a], Some(&hunk.lines[d].display()));
                    i += 2;
                }
                None => {
                    super::details::render_line(ui, colors, &hunk.lines[i], None);
                    i += 1;
                }
            }
        }
    }
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: EntryKind, index: char, worktree: char) -> Entry {
        Entry { path: path.to_string(), kind, index, worktree }
    }

    // ---- split_status ---------------------------------------------------------------------

    #[test]
    fn split_status_places_files_in_expected_lists() {
        let mut status = Status::default();
        status.entries.push(entry("staged.txt", EntryKind::Ordinary, 'M', '.'));
        status.entries.push(entry("unstaged.txt", EntryKind::Ordinary, '.', 'M'));
        status.entries.push(entry("both.txt", EntryKind::Ordinary, 'M', 'M'));
        status.entries.push(entry("new.txt", EntryKind::Untracked, '?', '?'));
        status.entries.push(entry("conflict.txt", EntryKind::Unmerged, 'U', 'U'));
        status.entries.push(entry("ignored.txt", EntryKind::Ignored, '!', '!'));

        let (conflicts, unstaged, staged) = split_status(&status);
        fn names<'a>(v: &[&'a Entry]) -> Vec<&'a str> {
            v.iter().map(|e| e.path.as_str()).collect()
        }

        assert_eq!(names(&conflicts), vec!["conflict.txt"]);
        assert!(names(&unstaged).contains(&"unstaged.txt"));
        assert!(names(&unstaged).contains(&"both.txt"));
        assert!(names(&unstaged).contains(&"new.txt"));
        assert!(!names(&unstaged).contains(&"staged.txt"));
        assert!(!names(&unstaged).contains(&"ignored.txt"));
        assert!(names(&staged).contains(&"staged.txt"));
        assert!(names(&staged).contains(&"both.txt"));
        assert!(!names(&staged).contains(&"new.txt"), "untracked files are never staged");
    }

    #[test]
    fn split_status_empty_repo_is_all_empty() {
        let status = Status::default();
        let (c, u, s) = split_status(&status);
        assert!(c.is_empty() && u.is_empty() && s.is_empty());
    }

    // ---- discard_confirm_text ---------------------------------------------------------------

    #[test]
    fn discard_confirm_text_names_the_file() {
        let (title, body) = discard_confirm_text("src/main.rs");
        assert_eq!(title, "Discard changes to `src/main.rs`?");
        assert!(body.contains("cannot be undone"));
    }

    // ---- entry_label --------------------------------------------------------------------------

    #[test]
    fn entry_label_shows_rename_arrow() {
        let e = entry("new.txt", EntryKind::Renamed { from: "old.txt".to_string() }, 'R', '.');
        assert_eq!(entry_label(&e, true), "R old.txt → new.txt");
    }

    #[test]
    fn entry_label_untracked_uses_question_mark() {
        let e = entry("notes.txt", EntryKind::Untracked, '?', '?');
        assert_eq!(entry_label(&e, false), "? notes.txt");
    }

    #[test]
    fn entry_label_glyph_picks_index_or_worktree_status() {
        // Staged as Modified but further modified in the worktree (Added there too), matching
        // §5.7's checkbox rows: the Unstaged row shows the worktree change, the Staged row shows
        // what's actually about to be committed.
        let e = entry("both.txt", EntryKind::Ordinary, 'M', 'A');
        assert_eq!(entry_label(&e, false), "A both.txt", "unstaged row shows the worktree glyph");
        assert_eq!(entry_label(&e, true), "M both.txt", "staged row shows the index glyph");
    }

    // ---- subject_color --------------------------------------------------------------------

    #[test]
    fn subject_color_matches_severity() {
        let colors = Colors::new(crate::model::theme::Mode::Dark);
        assert_eq!(subject_color(&colors, SubjectLen::Ok), colors.get(Token::FgPrimary));
        assert_eq!(subject_color(&colors, SubjectLen::Warning), colors.get(Token::Warning));
        assert_eq!(subject_color(&colors, SubjectLen::Danger), colors.get(Token::Danger));
    }
}
