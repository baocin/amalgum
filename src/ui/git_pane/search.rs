//! Commit search above the graph (§5.8, W7): the field, the flat result list that replaces the
//! graph rows with "N matches", and the jobs that run a query. The state machine (history,
//! debounce, stale answers, `Esc`/`Enter`) is `model::search`; this file only draws it and runs
//! its jobs on workers.

use super::worker;
use super::{GitEvent, GitPane, Selection};
use crate::git::cmd::{GitError, Location};
use crate::git::log::Commit;
use crate::git::search::{parse_search_log, search_log_args};
use crate::model::search::{self, Job, Mode, Record, Search};
use crate::model::theme::Token;
use crate::ui::chrome::Toast;
use crate::ui::theme::Colors;

/// Replies of the search jobs, carried in `Reply::Search`.
pub(super) enum SearchReply {
    Corpus { key: u64, result: Result<Vec<Record>, GitError> },
    Hits { req: u64, result: Result<Vec<Commit>, GitError> },
}

/// The pane's search: the headless state plus where to scroll the graph once it closes.
pub(super) struct SearchUi {
    pub state: Search,
    /// Graph row to bring into view on the next frame (after `Enter` or `Esc`).
    pub scroll_to: Option<usize>,
}

impl Default for SearchUi {
    fn default() -> Self {
        Self { state: Search::new(Mode::Local), scroll_to: None }
    }
}

impl SearchUi {
    pub fn new(location: &Location) -> Self {
        let mode = match location {
            Location::Local { .. } => Mode::Local,
            Location::Remote { .. } => Mode::Remote,
        };
        Self { state: Search::new(mode), scroll_to: None }
    }
}

/// Reads the loaded part of history with message bodies and changed paths (the local corpus).
fn load_corpus(git: &crate::git::Git, n: usize) -> Result<Vec<Record>, GitError> {
    let args = search_log_args(&["--all".to_string(), "-n".to_string(), n.to_string()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(parse_search_log(&git.run(&args)?).into_iter().map(Record::new).collect())
}

/// A remote search: `git log` narrowed by the query's filters, refined by `Query::matches`.
fn remote_search(git: &crate::git::Git, query: &crate::git::search::Query) -> Result<Vec<Commit>, GitError> {
    let mut extra = vec!["--all".to_string(), "-n".to_string(), search::REMOTE_LIMIT.to_string()];
    extra.extend(query.git_log_args());
    let args = search_log_args(&extra);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let records: Vec<Record> = parse_search_log(&git.run(&args)?).into_iter().map(Record::new).collect();
    Ok(search::filter(query, &records))
}

impl GitPane {
    /// Palette "Search commits" (and `/` / `Mod+F` with the graph focused): open the field.
    pub fn open_search(&mut self) {
        self.view = crate::model::state::GitView::Graph;
        let selected = match &self.selected {
            Selection::Commit(id) => Some(id.clone()),
            Selection::None => None,
        };
        self.search.state.open(selected);
    }

    /// What the local corpus must cover: the graph's current log window.
    fn corpus_key(&self) -> u64 {
        (self.log_refetch_dispatched_gen << 32) ^ self.log_loaded as u64
    }

    fn run_search_job(&mut self, ctx: &egui::Context, job: Job) {
        let git = self.git.clone();
        match job {
            Job::LoadCorpus { key } => {
                let n = self.log_loaded.max(worker::LOG_FIRST_PAGE);
                crate::ui::jobs::spawn(ctx, &self.tx, move || {
                    worker::Reply::Search(SearchReply::Corpus { key, result: load_corpus(&git, n) })
                });
            }
            Job::Local { req, query, corpus } => crate::ui::jobs::spawn(ctx, &self.tx, move || {
                worker::Reply::Search(SearchReply::Hits { req, result: Ok(search::filter(&query, &corpus)) })
            }),
            Job::Remote { req, query } => crate::ui::jobs::spawn(ctx, &self.tx, move || {
                worker::Reply::Search(SearchReply::Hits { req, result: remote_search(&git, &query) })
            }),
        }
    }

    pub(super) fn apply_search_reply(&mut self, reply: SearchReply, events: &mut Vec<GitEvent>) {
        match reply {
            SearchReply::Corpus { key, result } => match result {
                Ok(records) => self.search.state.set_corpus(key, records),
                Err(e) => {
                    self.search.state.corpus_failed(key);
                    events.push(GitEvent::Toast(Toast::error("Could not search history", e.to_string())));
                }
            },
            SearchReply::Hits { req, result } => match result {
                Ok(hits) => {
                    if self.search.state.accept(req, hits) {
                        self.show_search_cursor();
                    }
                }
                Err(e) => {
                    self.search.state.failed(req);
                    events.push(GitEvent::Toast(Toast::error("Search failed", e.to_string())));
                }
            },
        }
    }

    /// Details follow the result cursor, as they follow the graph selection.
    fn show_search_cursor(&mut self) {
        let Some(commit) = self.search.state.current().cloned() else { return };
        if self.selected == Selection::Commit(commit.id.clone()) {
            return;
        }
        self.selected = Selection::Commit(commit.id.clone());
        let ctx = self.ctx.clone();
        self.show_commit_details(&ctx, commit);
    }

    /// Selects `id` in the graph, loads its details, and scrolls the graph to it.
    fn position_graph(&mut self, commit: Option<Commit>, id: Option<String>) {
        let ctx = self.ctx.clone();
        let Some(id) = id else {
            self.selected = Selection::None;
            self.details = None;
            return;
        };
        self.selected = Selection::Commit(id.clone());
        match self.commits.iter().position(|c| c.id == id) {
            Some(row) => {
                self.search.scroll_to = Some(row);
                self.load_details_for_selected(&ctx);
            }
            // Further back than the graph has loaded (a remote result): details only.
            None => match commit {
                Some(c) => self.show_commit_details(&ctx, c),
                None => self.details = None,
            },
        }
    }

    /// The scroll offset that brings [`SearchUi::scroll_to`] into view, a few rows from the top.
    pub(super) fn take_search_scroll(&mut self, ui: &egui::Ui, row_height: f32) -> Option<f32> {
        let row = self.search.scroll_to.take()?;
        let step = row_height + ui.spacing().item_spacing.y;
        Some((row.saturating_sub(3) as f32) * step)
    }

    /// Draws the field when search is open and, while it holds a query, the results in place of
    /// the graph rows. Returns `true` when it drew the results (the caller skips the graph).
    /// `graph_focus` is the graph's focus id, for the `/` and `Mod+F` shortcuts.
    pub(super) fn show_search(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        now: u64,
        row_height: f32,
        graph_focus: egui::Id,
    ) -> bool {
        let frame_time = ui.input(|i| i.time);
        let graph_focused = ui.memory(|m| m.has_focus(graph_focus));
        if graph_focused {
            let open = ui.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Slash)
                    | i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)
            });
            if open {
                self.open_search();
            }
        }

        if let Some(job) = self.search.state.poll(frame_time, self.corpus_key()) {
            let ctx = ui.ctx().clone();
            self.run_search_job(&ctx, job);
        }
        if let Some(due) = self.search.state.due() {
            ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64((due - frame_time).max(0.0)));
        }
        if !self.search.state.is_open() {
            return false;
        }

        // -- the field ------------------------------------------------------------------------
        let field_id = ui.id().with("commit_search_field");
        let (mut up, mut down, mut enter, mut esc) = (false, false, false, false);
        if ui.memory(|m| m.has_focus(field_id)) {
            ui.input_mut(|i| {
                up = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp);
                down = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown);
                enter = i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
                esc = i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
            });
        } else if graph_focused || ui.memory(|m| m.focused().is_none()) {
            esc = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        }
        let mut clear = false;
        ui.horizontal(|ui| {
            let edit = egui::TextEdit::singleline(&mut self.search.state.text)
                .id(field_id)
                .hint_text("Search commits   author: path: before: after: hash: branch: tag: msg:")
                .desired_width(ui.available_width() - 28.0);
            let resp = ui.add(edit);
            if std::mem::take(&mut self.search.state.focus_field) {
                resp.request_focus();
            }
            if resp.changed() {
                self.search.state.edited(frame_time);
            }
            clear = ui.small_button("×").on_hover_text("Close search (Esc)").clicked();
        });
        let label = self.search.state.count_label();
        if !label.is_empty() {
            ui.colored_label(colors.get(Token::FgSecondary), label);
        }
        ui.separator();

        if up {
            self.search.state.up(frame_time);
            self.show_search_cursor();
        }
        if down {
            self.search.state.down(frame_time);
            self.show_search_cursor();
        }
        if esc || clear {
            let prev = self.search.state.escape();
            self.position_graph(None, prev);
            ui.memory_mut(|m| m.request_focus(graph_focus));
            return false;
        }
        if enter && let Some(hit) = self.search.state.enter() {
            let id = hit.id.clone();
            self.position_graph(Some(hit), Some(id));
            ui.memory_mut(|m| m.request_focus(graph_focus));
            return false;
        }

        if !self.search.state.showing_results() {
            return false;
        }

        // -- the results, flat, in place of the graph rows (W7) -------------------------------
        let mut clicked: Option<(usize, bool)> = None;
        let results = self.search.state.results().unwrap_or_default();
        let cursor = self.search.state.cursor();
        // Leave the lower part of the pane to the selected result's details.
        let max_height = if self.details.is_some() { ui.available_height() * 0.5 } else { f32::INFINITY };
        let area = egui::ScrollArea::vertical().id_salt("commit_search_results").max_height(max_height);
        area.auto_shrink([false, false]).show_rows(ui, row_height, results.len(), |ui, range| {
            for i in range {
                let commit = &results[i];
                let (rect, resp) = ui
                    .allocate_exact_size(egui::vec2(ui.available_width(), row_height), egui::Sense::click());
                if i == cursor {
                    ui.painter().rect_filled(rect, 0.0, colors.get(Token::BgSelected));
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 0.0, colors.get(Token::BgHover));
                }
                draw_result_row(ui, colors, now, rect, commit);
                if resp.double_clicked() {
                    clicked = Some((i, true));
                } else if resp.clicked() {
                    clicked = Some((i, false));
                }
            }
        });
        if let Some((i, accept)) = clicked {
            self.search.state.set_cursor(i);
            self.show_search_cursor();
            if accept && let Some(hit) = self.search.state.enter() {
                let id = hit.id.clone();
                self.position_graph(Some(hit), Some(id));
                ui.memory_mut(|m| m.request_focus(graph_focus));
            }
        }
        true
    }
}

/// One result row: subject, relative time, short hash (W7), no lanes.
fn draw_result_row(ui: &egui::Ui, colors: &Colors, now: u64, rect: egui::Rect, commit: &Commit) {
    let painter = ui.painter_at(rect);
    let (hash_w, time_w) = (64.0, 48.0);
    let mid_y = rect.center().y;
    let left = rect.left() + 8.0;
    let text_right = rect.right() - hash_w - time_w - 8.0;
    let mut job = egui::text::LayoutJob::single_section(
        commit.subject.clone(),
        egui::TextFormat::simple(egui::FontId::proportional(13.0), colors.get(Token::FgPrimary)),
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width((text_right - left).max(0.0));
    let galley = painter.layout_job(job);
    painter.galley(egui::pos2(left, mid_y - galley.size().y / 2.0), galley, colors.get(Token::FgPrimary));
    painter.text(
        egui::pos2(rect.right() - hash_w - time_w, mid_y),
        egui::Align2::LEFT_CENTER,
        crate::util::relative_time(now, commit.time),
        egui::FontId::proportional(11.0),
        colors.get(Token::FgSecondary),
    );
    painter.text(
        egui::pos2(rect.right() - hash_w, mid_y),
        egui::Align2::LEFT_CENTER,
        worker::short_hash(&commit.id),
        egui::FontId::monospace(11.0),
        colors.get(Token::FgSecondary),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::search::Query;
    use crate::testutil::TempRepo;

    fn git(repo: &TempRepo) -> crate::git::Git {
        crate::git::Git::new(Location::Local { path: repo.path().to_path_buf() })
    }

    #[test]
    fn local_corpus_covers_paths_and_bodies() {
        let mut repo = TempRepo::new();
        repo.commit_file("src/auth/login.rs", "a", "Fix login timeout");
        repo.commit_file("README.md", "b", "Docs");
        let records = load_corpus(&git(&repo), 500).expect("corpus");
        let hits = search::filter(&Query::parse("path:auth"), &records);
        assert_eq!(hits.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), vec!["Fix login timeout"]);
    }

    #[test]
    fn remote_search_refines_what_git_returns() {
        let mut repo = TempRepo::new();
        repo.commit_file("src/auth/login.rs", "a", "Fix login timeout");
        let docs = repo.commit_file("README.md", "b", "Docs");
        repo.git(&["tag", "v2"]);
        // `tag:` cannot be sent to git; `Query::matches` narrows git's answer.
        let hits = remote_search(&git(&repo), &Query::parse("tag:v2")).expect("search");
        assert_eq!(hits.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec![docs.as_str()]);
        let hits = remote_search(&git(&repo), &Query::parse("author:tester path:auth")).expect("search");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn enter_positions_the_graph_on_the_hit() {
        let mut pane = super::super::tests::new_test_pane();
        let commit = |id: &str| Commit {
            id: id.into(),
            parents: vec![],
            author: "A".into(),
            email: "a@x".into(),
            time: 0,
            committer: "A".into(),
            committer_email: "a@x".into(),
            commit_time: 0,
            refs: vec![],
            subject: id.into(),
        };
        pane.apply_log_page((0..10).map(|i| commit(&format!("c{i}"))).collect(), 500);
        pane.position_graph(None, Some("c7".into()));
        assert_eq!(pane.selected, Selection::Commit("c7".into()));
        assert_eq!(pane.search.scroll_to, Some(7));
        // A hit beyond the loaded rows still shows its details.
        pane.position_graph(Some(commit("far")), Some("far".into()));
        assert_eq!(pane.details.as_ref().map(|d| d.commit.id.as_str()), Some("far"));
        pane.position_graph(None, None);
        assert_eq!(pane.selected, Selection::None);
    }
}
