//! Commit details and the diff view below the graph (§5.5, §5.6): header, message, the
//! changed-file list, and the unified diff of whichever file is selected.

use super::worker;
use super::{GitEvent, GitPane};
use crate::git::cmd::GitError;
use crate::git::diff::{self, FileChange, FileDiff, Line, LineKind};
use crate::git::log::Commit;
use crate::model::theme::Token;
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
        self.details = Some(DetailsState::new(commit));
        self.dispatch_commit_body(ctx, id.clone());
        self.dispatch_file_list(ctx, id.clone());
    }

    fn dispatch_commit_body(&self, ctx: &egui::Context, id: String) {
        let git = self.git.clone();
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let result = git
                .run(&["show", "-s", "--format=%B", &id])
                .map(|o| crate::git::log::split_message(&String::from_utf8_lossy(&o)));
            worker::Reply::CommitBody { id, result }
        });
    }

    fn dispatch_file_list(&self, ctx: &egui::Context, id: String) {
        let git = self.git.clone();
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let mut args = vec!["diff-tree", "-r", "--root", "--no-commit-id", &id];
            args.extend_from_slice(diff::RAW_NUMSTAT_ARGS);
            let result = git.run(&args).map(|o| diff::parse_raw_numstat(&o));
            worker::Reply::FileList { id, result }
        });
    }

    pub(super) fn dispatch_details_diff(&mut self, ctx: &egui::Context, path: String) {
        let Some(details_id) = self.details.as_ref().map(|d| d.commit.id.clone()) else { return };
        let req = self.next_req_id();
        if let Some(details) = &mut self.details {
            details.selected_file = Some(path.clone());
            details.diff_loading = true;
            details.diff_req = req;
        }
        let git = self.git.clone();
        crate::ui::jobs::spawn(ctx, &self.tx, move || {
            let mut args = vec!["show", "--format="];
            args.extend_from_slice(diff::DIFF_ARGS);
            args.extend([details_id.as_str(), "--", path.as_str()]);
            let result = git.run(&args).map(|o| diff::parse(&o));
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

        let mut action: Option<DetailAction> = None;

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
                    let clicked = ui
                        .interact(
                            resp.response.rect,
                            ui.id().with(("detail_file", &f.path)),
                            egui::Sense::click(),
                        )
                        .clicked();
                    if clicked {
                        action = Some(DetailAction::SelectFile(f.path.clone()));
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
                render_diff_file(ui, colors, file, limit, &mut want_more);
            }
            if want_more {
                action = Some(DetailAction::LoadMore);
            }
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
}

fn status_color(colors: &Colors, status: char) -> egui::Color32 {
    match status {
        'A' | 'C' => colors.get(Token::DiffAddFg),
        'D' => colors.get(Token::DiffDelFg),
        _ => colors.get(Token::FgSecondary),
    }
}

/// Renders one file's hunks (read-only: used by the commit details view). `budget` is decremented
/// as lines are drawn; once it reaches the file's `limit`, drawing stops and `*want_more` is set
/// if the caller clicks "Load more".
pub(super) fn render_diff_file(
    ui: &mut egui::Ui,
    colors: &Colors,
    file: &FileDiff,
    limit: usize,
    want_more: &mut bool,
) {
    let title = match (&file.old_path, &file.new_path) {
        (Some(a), Some(b)) if a != b => format!("{a} → {b}"),
        (Some(a), _) => a.clone(),
        (None, Some(b)) => b.clone(),
        (None, None) => "(unknown)".to_string(),
    };
    ui.label(egui::RichText::new(title).strong());
    if file.binary {
        ui.weak("Binary file");
        return;
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
                    render_line(ui, colors, &hunk.lines[del], Some(&hunk.lines[add].display()));
                    render_line(ui, colors, &hunk.lines[add], Some(&hunk.lines[del].display()));
                    i += 2;
                    drawn += 2;
                }
                None => {
                    render_line(ui, colors, &hunk.lines[i], None);
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

pub(super) fn render_line(ui: &mut egui::Ui, colors: &Colors, line: &Line, pair_text: Option<&str>) {
    if line.kind == LineKind::NoNewline {
        ui.weak("\\ No newline at end of file");
        return;
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
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::hover());
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

    // ---- status_color mapping is at least stable/distinct ------------------------------------

    #[test]
    fn status_color_distinguishes_add_delete_other() {
        let colors = Colors::new(crate::model::theme::Mode::Dark);
        assert_ne!(status_color(&colors, 'A'), status_color(&colors, 'D'));
        assert_eq!(status_color(&colors, 'A'), status_color(&colors, 'C'));
        assert_eq!(status_color(&colors, 'M'), colors.get(Token::FgSecondary));
    }
}
