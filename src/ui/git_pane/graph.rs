//! The Graph view (§5.4): streamed `git log`, incrementally laid out with `git::graph::Layout`,
//! rendered with virtual scrolling (`egui::ScrollArea::show_rows`).

use super::worker;
use super::{GitEvent, GitPane, Selection};
use crate::git::graph::Half;
use crate::git::log::{Commit, Decoration};
use crate::model::settings::Settings;
use crate::ui::theme::Colors;

/// How many rows from the end of what's loaded triggers the next batch (§5.4 "Streaming":
/// "Scrolling near the end triggers the next batch").
const PAGINATE_MARGIN: usize = 50;

/// Whether the visible range's end is close enough to the loaded total to fetch more.
pub(super) fn near_end(range_end: usize, total_loaded: usize) -> bool {
    total_loaded.saturating_sub(range_end) <= PAGINATE_MARGIN
}

/// The x coordinate of a lane's center (module doc: "lane x = left + lane*lane_width +
/// lane_width/2").
pub(super) fn lane_x(left: f32, lane: usize, lane_width: f32) -> f32 {
    left + lane as f32 * lane_width + lane_width / 2.0
}

/// The label shown on a ref chip.
pub(super) fn chip_text(dec: &Decoration) -> Option<String> {
    match dec {
        Decoration::CurrentBranch(name) => Some(name.clone()),
        Decoration::Branch(name) => Some(name.clone()),
        Decoration::Remote(name) => Some(name.clone()),
        Decoration::Tag(name) => Some(name.clone()),
        Decoration::Head | Decoration::Other(_) => None,
    }
}

/// The branch name to check out, for a chip that names a branch.
pub(super) fn checkout_target(dec: &Decoration) -> Option<&str> {
    match dec {
        Decoration::CurrentBranch(name) | Decoration::Branch(name) => Some(name.as_str()),
        _ => None,
    }
}

impl GitPane {
    /// A fresh `LogPage` reply: append to `commits`/`rows`, running the incremental layout over
    /// just the newly-arrived commits (never revisiting earlier rows, per `graph::Layout`'s
    /// contract). `requested` was the `-n` asked for; fewer rows back than that means history is
    /// exhausted.
    pub(super) fn apply_log_page(&mut self, commits: Vec<Commit>, requested: usize) {
        self.log_has_more = commits.len() >= requested;
        // `commits` is the *whole* window from the start (git log -n), not just the new tail:
        // only push commits past what's already laid out.
        for commit in commits.into_iter().skip(self.commits.len()) {
            let branch = commit.refs.iter().find_map(|d| match d {
                Decoration::Branch(n) | Decoration::CurrentBranch(n) => Some(n.as_str()),
                _ => None,
            });
            let row = self.layout.push(&commit.id, &commit.parents, branch);
            self.rows.push(row);
            self.commits.push(commit);
        }
        self.log_loaded = self.commits.len();
    }

    fn selected_index(&self) -> Option<usize> {
        match &self.selected {
            Selection::Commit(id) => self.commits.iter().position(|c| &c.id == id),
            Selection::None => None,
        }
    }

    fn select_index(&mut self, idx: usize) {
        let Some(commit) = self.commits.get(idx) else { return };
        let id = commit.id.clone();
        self.selected = Selection::Commit(id);
        let ctx = self.ctx.clone();
        self.load_details_for_selected(&ctx);
    }

    fn handle_keys(&mut self, ui: &egui::Ui, has_focus: bool) {
        if !has_focus || self.commits.is_empty() {
            return;
        }
        let cur = self.selected_index();
        let (down, up, home, end) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::J) || i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::K) || i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::Home),
                i.key_pressed(egui::Key::End),
            )
        });
        let next = if home {
            Some(0)
        } else if end {
            Some(self.commits.len() - 1)
        } else if down {
            Some(cur.map_or(0, |i| (i + 1).min(self.commits.len() - 1)))
        } else if up {
            Some(cur.map_or(0, |i| i.saturating_sub(1)))
        } else {
            None
        };
        if let Some(idx) = next {
            self.select_index(idx);
        }
    }

    pub(super) fn show_graph(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        settings: &Settings,
        now: u64,
        events: &mut Vec<GitEvent>,
        ctx: &egui::Context,
    ) {
        if self.commits.is_empty() && !self.log_loading && self.status.branch.oid.is_none() {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.colored_label(
                    colors.get(crate::model::theme::Token::FgSecondary),
                    "No commits yet. Stage files and make the first commit.",
                );
                if ui.button("Go to Changes").clicked() {
                    self.view = crate::model::state::GitView::Changes;
                }
            });
            return;
        }

        if self.status.changed_count() > 0 {
            let warn = colors.get(crate::model::theme::Token::Warning);
            let resp = ui.horizontal(|ui| {
                ui.colored_label(warn, format!("⚠ Working tree · {} changed", self.status.changed_count()))
            });
            if resp.response.interact(egui::Sense::click()).clicked() {
                self.view = crate::model::state::GitView::Changes;
            }
            ui.separator();
        }

        let row_height = settings.appearance.row_height as f32;
        let lane_width = (row_height * 0.7).max(10.0);

        let focus_id = ui.id().with("graph_focus_bg");
        let bg_rect = ui.available_rect_before_wrap();
        let bg = ui.interact(bg_rect, focus_id, egui::Sense::click());
        if bg.clicked() {
            bg.request_focus();
        }
        let has_focus = bg.has_focus();
        self.handle_keys(ui, has_focus);

        let total = self.rows.len();
        let mut trigger_more = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(
            ui,
            row_height,
            total,
            |ui, range| {
                if near_end(range.end, self.log_loaded) && self.log_has_more && !self.log_loading {
                    trigger_more = true;
                }
                for i in range.clone() {
                    self.draw_row(ui, colors, now, i, row_height, lane_width, events);
                }
            },
        );
        if trigger_more {
            let n = worker::next_log_n(self.log_loaded);
            self.dispatch_log_page(ctx, n);
        }

        ui.separator();
        self.show_details(ui, colors, now, events, ctx);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_row(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        now: u64,
        i: usize,
        row_height: f32,
        lane_width: f32,
        events: &mut Vec<GitEvent>,
    ) {
        let commit = self.commits[i].clone();
        let row = self.rows[i].clone();
        let is_selected = matches!(&self.selected, Selection::Commit(id) if id == &commit.id);

        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), row_height), egui::Sense::click());
        if is_selected {
            ui.painter().rect_filled(rect, 0.0, colors.get(crate::model::theme::Token::BgSelected));
        }
        let lane_area_width = (row.width.max(1) as f32) * lane_width + lane_width;
        let painter = ui.painter_at(rect);
        let mid_y = rect.top() + row_height / 2.0;
        for seg in &row.segments {
            let x0 = lane_x(rect.left(), seg.from, lane_width);
            let x1 = lane_x(rect.left(), seg.to, lane_width);
            let color = colors.lane(seg.color);
            let (y0, y1) = match seg.half {
                Half::Top => (rect.top(), mid_y),
                Half::Bottom => (mid_y, rect.bottom()),
            };
            painter.line_segment([egui::pos2(x0, y0), egui::pos2(x1, y1)], egui::Stroke::new(2.0, color));
        }
        let node_x = lane_x(rect.left(), row.node, lane_width);
        painter.circle_filled(egui::pos2(node_x, mid_y), lane_width * 0.28, colors.lane(row.color));

        let content_left = rect.left() + lane_area_width;
        let mut cursor = content_left;
        // Right-hand columns are fixed; chips and the subject are clipped to what is left.
        let (hash_w, time_w) = (64.0, 48.0);
        let text_right = rect.right() - hash_w - time_w - 8.0;
        let chips_painter =
            painter.with_clip_rect(egui::Rect::from_x_y_ranges(content_left..=text_right, rect.y_range()));
        for dec in &commit.refs {
            let Some(text) = chip_text(dec) else { continue };
            let bold = matches!(dec, Decoration::CurrentBranch(_));
            let outlined = matches!(dec, Decoration::Tag(_));
            let label = if bold { format!("✓ {text}") } else { text.clone() };
            let chip_rect = egui::Rect::from_min_size(
                egui::pos2(cursor, rect.top() + 2.0),
                egui::vec2(
                    painter
                        .layout_no_wrap(
                            label.clone(),
                            egui::FontId::proportional(11.0),
                            colors.get(crate::model::theme::Token::FgPrimary),
                        )
                        .size()
                        .x
                        + 10.0,
                    row_height - 4.0,
                ),
            );
            if outlined {
                chips_painter.rect_stroke(
                    chip_rect,
                    3.0,
                    egui::Stroke::new(1.0, colors.get(crate::model::theme::Token::Border)),
                    egui::StrokeKind::Inside,
                );
            } else {
                chips_painter.rect_filled(chip_rect, 3.0, colors.get(crate::model::theme::Token::BgHover));
            }
            chips_painter.text(
                chip_rect.center(),
                egui::Align2::CENTER_CENTER,
                &label,
                egui::FontId::proportional(11.0),
                colors.get(crate::model::theme::Token::FgPrimary),
            );
            let chip_resp =
                ui.interact(chip_rect, ui.id().with(("chip", i, dec_key(dec))), egui::Sense::click());
            if chip_resp.clicked() {
                self.select_index(i);
            }
            if chip_resp.double_clicked()
                && let Some(target) = checkout_target(dec)
            {
                let ctx = self.ctx.clone();
                self.start_checkout(&ctx, target.to_string());
            }
            cursor = chip_rect.right() + 4.0;
        }

        let subject_left = (cursor + 4.0).min(text_right);
        let mut job = egui::text::LayoutJob::single_section(
            commit.subject.clone(),
            egui::TextFormat::simple(
                egui::FontId::proportional(13.0),
                colors.get(crate::model::theme::Token::FgPrimary),
            ),
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width((text_right - subject_left).max(0.0));
        let galley = painter.layout_job(job);
        painter.galley(
            egui::pos2(subject_left, mid_y - galley.size().y / 2.0),
            galley,
            colors.get(crate::model::theme::Token::FgPrimary),
        );
        painter.text(
            egui::pos2(rect.right() - hash_w - time_w, mid_y),
            egui::Align2::LEFT_CENTER,
            crate::util::relative_time(now, commit.time),
            egui::FontId::proportional(11.0),
            colors.get(crate::model::theme::Token::FgSecondary),
        );
        painter.text(
            egui::pos2(rect.right() - hash_w, mid_y),
            egui::Align2::LEFT_CENTER,
            worker::short_hash(&commit.id),
            egui::FontId::monospace(11.0),
            colors.get(crate::model::theme::Token::FgSecondary),
        );

        if resp.clicked() {
            self.select_index(i);
        }
        let _ = events;
    }
}

fn dec_key(dec: &Decoration) -> String {
    match dec {
        Decoration::Head => "head".to_string(),
        Decoration::CurrentBranch(n) => format!("cb:{n}"),
        Decoration::Branch(n) => format!("b:{n}"),
        Decoration::Remote(n) => format!("r:{n}"),
        Decoration::Tag(n) => format!("t:{n}"),
        Decoration::Other(n) => format!("o:{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::log::Decoration;

    // ---- near_end ------------------------------------------------------------------------

    #[test]
    fn near_end_triggers_within_margin() {
        assert!(near_end(450, 500));
        assert!(near_end(500, 500));
        assert!(!near_end(0, 500));
        assert!(!near_end(400, 500));
    }

    #[test]
    fn near_end_when_total_smaller_than_range_end() {
        assert!(near_end(10, 5));
    }

    // ---- lane_x --------------------------------------------------------------------------

    #[test]
    fn lane_x_centers_within_its_slot() {
        assert_eq!(lane_x(0.0, 0, 20.0), 10.0);
        assert_eq!(lane_x(0.0, 1, 20.0), 30.0);
        assert_eq!(lane_x(100.0, 2, 10.0), 125.0);
    }

    // ---- chip_text / checkout_target ------------------------------------------------------

    #[test]
    fn chip_text_known_decorations() {
        assert_eq!(chip_text(&Decoration::CurrentBranch("main".into())).as_deref(), Some("main"));
        assert_eq!(chip_text(&Decoration::Branch("feat".into())).as_deref(), Some("feat"));
        assert_eq!(chip_text(&Decoration::Remote("origin/main".into())).as_deref(), Some("origin/main"));
        assert_eq!(chip_text(&Decoration::Tag("v1".into())).as_deref(), Some("v1"));
        assert_eq!(chip_text(&Decoration::Head), None);
        assert_eq!(chip_text(&Decoration::Other("refs/stash".into())), None);
    }

    #[test]
    fn checkout_target_only_for_branches() {
        assert_eq!(checkout_target(&Decoration::CurrentBranch("main".into())), Some("main"));
        assert_eq!(checkout_target(&Decoration::Branch("feat".into())), Some("feat"));
        assert_eq!(checkout_target(&Decoration::Tag("v1".into())), None);
        assert_eq!(checkout_target(&Decoration::Remote("origin/main".into())), None);
    }

    // ---- apply_log_page (pagination bookkeeping + incremental layout) --------------------

    fn commit(id: &str, parents: &[&str]) -> Commit {
        Commit {
            id: id.to_string(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            author: "A".into(),
            email: "a@x".into(),
            time: 0,
            committer: "A".into(),
            committer_email: "a@x".into(),
            commit_time: 0,
            refs: Vec::new(),
            subject: id.to_string(),
        }
    }

    #[test]
    fn apply_log_page_appends_and_lays_out_rows() {
        let mut pane = super::super::tests::new_test_pane();
        pane.apply_log_page(vec![commit("a", &["b"]), commit("b", &[])], 500);
        assert_eq!(pane.commits.len(), 2);
        assert_eq!(pane.rows.len(), 2);
        assert_eq!(pane.log_loaded, 2);
        assert!(!pane.log_has_more, "fewer than requested: history exhausted");
    }

    #[test]
    fn apply_log_page_second_page_only_appends_new_tail() {
        let mut pane = super::super::tests::new_test_pane();
        pane.apply_log_page(vec![commit("a", &["b"]), commit("b", &[])], 500);
        let first_two_rows = pane.rows.clone();
        // A second page re-sends the whole window (a, b, c): only "c" is new.
        pane.apply_log_page(vec![commit("a", &["b"]), commit("b", &["c"]), commit("c", &[])], 2500);
        assert_eq!(pane.commits.len(), 3);
        assert_eq!(pane.rows[..2], first_two_rows[..], "earlier rows are never recomputed");
        assert!(!pane.log_has_more, "only 3 commits exist for a 2500 request: history is exhausted");
    }

    #[test]
    fn apply_log_page_has_more_when_full_page_returned() {
        let mut pane = super::super::tests::new_test_pane();
        let commits: Vec<Commit> = (0..500).map(|i| commit(&format!("c{i}"), &[])).collect();
        pane.apply_log_page(commits, 500);
        assert!(pane.log_has_more);
    }
}
