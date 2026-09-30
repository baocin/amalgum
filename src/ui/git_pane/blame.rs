//! §5.15 Blame, W12: the git pane maximized to the file's content with a gutter per line —
//! short hash, author, relative date — on a `blame.hot` (newest) → `blame.cold` (oldest)
//! background. Hovering a gutter cell shows the commit's full message; clicking it selects the
//! commit in the graph. `Mod+←` re-blames at the parent of the focused line's commit ("blame
//! back"), `Mod+→` steps forward, the breadcrumb shows the stack. Rows are virtualised
//! (`show_rows`), so a large file costs only the visible lines per frame.
//!
//! `git blame -p` runs on a worker through `git::blame::run` (local or over ssh), honouring
//! `blame.ignoreRevsFile`; results are cached per (commit, path) — only for full commit ids,
//! whose blame never changes. The working tree and symbolic revisions (`HEAD`, `id^`) are
//! re-blamed every time, so an edit or a new commit is never hidden by a stale entry.

use super::history::{FileTarget, FileViewReply};
use super::{GitEvent, GitPane, worker};
use crate::git::blame::Blame;
use crate::git::cmd::GitError;
use crate::model::blame::{self as model, BlameStack, Frame};
use crate::model::theme::Token;
use crate::ui::chrome::Toast;
use crate::ui::jobs;
use crate::ui::theme::Colors;
use std::sync::Arc;

/// Blames kept for back / forward and re-opening (§8 "cached per (commit, path)").
const CACHE_SIZE: usize = 16;
const ROW_HEIGHT: f32 = 17.0;
const GUTTER_WIDTH: f32 = 236.0;
const NUMBER_WIDTH: f32 = 48.0;

pub(super) struct BlameState {
    pub stack: BlameStack,
    pub blame: Option<Arc<Blame>>,
    /// Per commit of `blame`, see `model::blame::heat`.
    pub heat: Vec<f32>,
    /// The longest line in characters, measured once per blame (the horizontal extent).
    pub widest: usize,
    pub loading: bool,
    pub req: u64,
    pub error: Option<String>,
    /// Scroll the focused line into view on the next frame.
    pub scroll_to_focus: bool,
}

enum Act {
    Close,
    Back,
    BackAt(usize),
    Forward,
    Jump(usize),
    Focus(usize),
    Move(isize),
    SelectCommit(String),
    History,
    CopyHash(String),
}

impl GitPane {
    /// **Blame** (§5.15): open the full-pane view on `target` (the git pane maximizes).
    pub(super) fn open_blame(&mut self, ctx: &egui::Context, target: FileTarget) {
        let line = target.line.map_or(0, |n| n.saturating_sub(1) as usize);
        let frame = Frame::new(target.rev, target.path, line);
        let was_open = self.file_views.blame.is_some();
        self.file_views.blame = Some(BlameState {
            stack: BlameStack::new(frame),
            blame: None,
            heat: Vec::new(),
            widest: 0,
            loading: false,
            req: 0,
            error: None,
            scroll_to_focus: true,
        });
        if !was_open {
            self.file_views.pending.push(GitEvent::MaximizePane(true));
        }
        self.load_blame_frame(ctx);
    }

    /// Leave the blame view, restoring the pane's size.
    pub(super) fn close_blame(&mut self) {
        if self.file_views.blame.take().is_some() {
            self.file_views.pending.push(GitEvent::MaximizePane(false));
        }
    }

    /// Show the stack's current frame: from the cache, or `git blame -p` on a worker.
    fn load_blame_frame(&mut self, ctx: &egui::Context) {
        let req = self.next_req_id();
        let Some(state) = &mut self.file_views.blame else { return };
        let frame = state.stack.current().clone();
        state.scroll_to_focus = true;
        state.error = None;
        let key = (frame.rev.clone(), frame.path.clone());
        let hit = cacheable(&key).then(|| self.file_views.cache.iter().find(|(k, _)| *k == key)).flatten();
        if let Some((_, hit)) = hit {
            state.heat = model::heat(hit);
            state.widest = hit.max_line_chars();
            state.blame = Some(Arc::clone(hit));
            state.loading = false;
            return;
        }
        // The previous frame's blame must not answer actions aimed at this one.
        state.blame = None;
        state.heat.clear();
        state.widest = 0;
        state.loading = true;
        state.req = req;
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let result = crate::git::blame::run(&git, frame.rev.as_deref(), &frame.path);
            worker::Reply::FileView(FileViewReply::Blame { req, result })
        });
    }

    pub(super) fn apply_blame(
        &mut self,
        req: u64,
        result: Result<Blame, GitError>,
        events: &mut Vec<GitEvent>,
    ) {
        let Some(state) = &mut self.file_views.blame else { return };
        if state.req != req || !state.loading {
            return;
        }
        state.loading = false;
        match result {
            Ok(blame) => {
                let blame = Arc::new(blame);
                let frame = state.stack.current_mut();
                frame.line = frame.line.min(blame.lines.len().saturating_sub(1));
                let key = (frame.rev.clone(), frame.path.clone());
                state.heat = model::heat(&blame);
                state.widest = blame.max_line_chars();
                state.blame = Some(Arc::clone(&blame));
                state.scroll_to_focus = true;
                if !cacheable(&key) {
                    return;
                }
                let cache = &mut self.file_views.cache;
                cache.retain(|(k, _)| *k != key);
                cache.push((key, blame));
                if cache.len() > CACHE_SIZE {
                    cache.remove(0);
                }
            }
            Err(e) => {
                state.blame = None;
                state.error = Some(e.summary());
                events.push(GitEvent::Toast(Toast::error("Could not blame the file", e.to_string())));
            }
        }
    }

    /// Draws the blame view when it is open (in place of the whole pane); `false` otherwise.
    pub(super) fn show_blame_view(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        now: u64,
        events: &mut Vec<GitEvent>,
        ctx: &egui::Context,
    ) -> bool {
        events.append(&mut self.file_views.pending);
        let Some(state) = &mut self.file_views.blame else { return false };
        let frame = state.stack.current().clone();
        let crumbs = state.stack.breadcrumb();
        let (blame, heat, widest, loading, error) =
            (state.blame.clone(), state.heat.clone(), state.widest, state.loading, state.error.clone());
        let scroll_to_focus = std::mem::take(&mut state.scroll_to_focus);
        let mut act: Option<Act> = None;

        // Keys only while the app's keyboard focus is on this pane: a terminal beside it (the
        // pane un-maximized) must get its arrows, PageUp/Down, word motion and Esc.
        if self.ops.app_focused && !ctx.memory(|m| m.top_modal_layer().is_some()) && !ctx.text_edit_focused()
        {
            ui.input_mut(|i| {
                let cmd = egui::Modifiers::COMMAND;
                if i.consume_key(cmd, egui::Key::ArrowLeft) {
                    act = Some(Act::Back);
                } else if i.consume_key(cmd, egui::Key::ArrowRight) {
                    act = Some(Act::Forward);
                } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                    act = Some(Act::Close);
                } else if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::J)
                {
                    act = Some(Act::Move(1));
                } else if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::K)
                {
                    act = Some(Act::Move(-1));
                } else if i.consume_key(egui::Modifiers::NONE, egui::Key::PageDown) {
                    act = Some(Act::Move(30));
                } else if i.consume_key(egui::Modifiers::NONE, egui::Key::PageUp) {
                    act = Some(Act::Move(-30));
                }
            });
        }

        let mod_label = crate::platform::primary_modifier_name();
        ui.horizontal(|ui| {
            if ui.button("← Back").on_hover_text("Esc").clicked() {
                act = Some(Act::Close);
            }
            ui.label(egui::RichText::new(format!("Blame {} @ {}", frame.path, frame.label())).strong());
            ui.add_space(12.0);
            for (i, (label, current)) in crumbs.iter().enumerate() {
                if i > 0 {
                    ui.weak("›");
                }
                if ui.selectable_label(*current, label).clicked() && !current {
                    act = Some(Act::Jump(i));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("History").on_hover_text("File history of this file").clicked() {
                    act = Some(Act::History);
                }
                let can_forward = crumbs.first().is_some_and(|(_, current)| !current);
                if ui
                    .add_enabled(can_forward, egui::Button::new("▶"))
                    .on_hover_text(format!("Step forward ({mod_label}+→)"))
                    .clicked()
                {
                    act = Some(Act::Forward);
                }
                if ui
                    .add_enabled(blame.is_some(), egui::Button::new("◀"))
                    .on_hover_text(format!("Blame the parent of this line's commit ({mod_label}+←)"))
                    .clicked()
                {
                    act = Some(Act::Back);
                }
            });
        });
        ui.separator();

        match (&blame, loading, &error) {
            (_, true, _) => {
                ui.weak("Blaming…");
            }
            (None, false, Some(e)) => {
                ui.colored_label(colors.get(Token::Danger), e);
            }
            (Some(blame), false, _) if blame.lines.is_empty() => {
                ui.weak("The file is empty.");
            }
            (Some(blame), false, _) => {
                if let Some(a) = draw_rows(
                    ui,
                    colors,
                    blame,
                    (&heat, widest),
                    frame.line,
                    scroll_to_focus,
                    now,
                    &self.file_views.messages,
                ) {
                    act = Some(a);
                }
                let hovered: Vec<String> = ui.ctx().data(|d| d.get_temp(hover_id())).unwrap_or_default();
                for id in hovered {
                    self.request_message(ctx, &id);
                }
            }
            (None, false, None) => {}
        }

        if let Some(act) = act {
            self.apply_blame_act(ctx, act, events);
        }
        true
    }

    fn apply_blame_act(&mut self, ctx: &egui::Context, act: Act, events: &mut Vec<GitEvent>) {
        let Some(state) = &mut self.file_views.blame else { return };
        if state.loading && matches!(act, Act::Back | Act::BackAt(_) | Act::Move(_) | Act::Focus(_)) {
            return; // they act on the current frame's blame, which is not here yet
        }
        match act {
            Act::Close => {
                self.close_blame();
                events.append(&mut self.file_views.pending);
            }
            Act::Back | Act::BackAt(_) => {
                let Some(blame) = state.blame.clone() else { return };
                if let Act::BackAt(line) = act {
                    state.stack.current_mut().line = line;
                }
                match model::back_target(&blame, state.stack.current().line) {
                    Ok(frame) => {
                        state.stack.push(frame);
                        self.load_blame_frame(ctx);
                    }
                    Err(refusal) => events.push(GitEvent::Toast(Toast::info(refusal.message()))),
                }
            }
            Act::Forward => {
                if state.stack.forward() {
                    self.load_blame_frame(ctx);
                }
            }
            Act::Jump(i) => {
                if state.stack.jump(i) {
                    self.load_blame_frame(ctx);
                }
            }
            Act::Focus(line) => state.stack.current_mut().line = line,
            Act::Move(delta) => {
                let len = state.blame.as_ref().map_or(0, |b| b.lines.len());
                let frame = state.stack.current_mut();
                frame.line = frame.line.saturating_add_signed(delta).min(len.saturating_sub(1));
                state.scroll_to_focus = true;
            }
            Act::SelectCommit(id) => {
                if id == crate::git::blame::UNCOMMITTED {
                    events.push(GitEvent::Toast(Toast::info("This line is not committed yet")));
                } else {
                    self.select_commit_in_graph(ctx, id);
                    events.append(&mut self.file_views.pending);
                }
            }
            Act::History => {
                let f = state.stack.current();
                let target = FileTarget { rev: f.rev.clone(), path: f.path.clone(), line: None };
                self.open_history(ctx, target);
                events.append(&mut self.file_views.pending);
            }
            Act::CopyHash(id) => ctx.copy_text(id),
        }
    }
}

/// Whether a blame of `(rev, path)` can be served from the cache: only at a full commit id.
/// The working tree changes under us, and `HEAD` / `id^` name different commits over time.
fn cacheable((rev, _): &(Option<String>, String)) -> bool {
    rev.as_deref().is_some_and(|r| matches!(r.len(), 40 | 64) && r.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Where `draw_rows` leaves the ids of commits whose gutter is hovered (their full message is
/// fetched once).
fn hover_id() -> egui::Id {
    egui::Id::new("amalgum_blame_hovered")
}

/// The virtualised rows; returns the one action taken. `(heat, widest)` are the state's per-commit
/// heat and longest line.
#[allow(clippy::too_many_arguments)]
fn draw_rows(
    ui: &mut egui::Ui,
    colors: &Colors,
    blame: &Blame,
    (heat, widest): (&[f32], usize),
    focus: usize,
    scroll_to_focus: bool,
    now: u64,
    messages: &std::collections::HashMap<String, Option<String>>,
) -> Option<Act> {
    let mut act = None;
    let mut hovered: Vec<String> = Vec::new();
    let mono = egui::FontId::monospace(12.0);
    let small = egui::FontId::proportional(11.0);
    let char_w = ui.painter().layout_no_wrap("M".into(), mono.clone(), colors.get(Token::FgPrimary)).size().x;
    let text_w = widest as f32 * char_w + 24.0;
    let step = ROW_HEIGHT + ui.spacing().item_spacing.y;
    let mut area = egui::ScrollArea::both().id_salt("blame_rows").auto_shrink([false, false]);
    if scroll_to_focus {
        let visible_rows = (ui.available_height() / step).max(1.0) as usize;
        let top = focus.saturating_sub(visible_rows / 3);
        area = area.vertical_scroll_offset(top as f32 * step);
    }
    area.show_rows(ui, ROW_HEIGHT, blame.lines.len(), |ui, range| {
        let width = (GUTTER_WIDTH + NUMBER_WIDTH + text_w).max(ui.available_width());
        for i in range {
            let line = &blame.lines[i];
            let commit = &blame.commits[line.commit];
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, ROW_HEIGHT), egui::Sense::click());
            let painter = ui.painter_at(rect);
            let gutter_rect = egui::Rect::from_min_size(rect.min, egui::vec2(GUTTER_WIDTH, ROW_HEIGHT));
            let heat = heat.get(line.commit).copied().unwrap_or(0.0);
            painter.rect_filled(gutter_rect, 0.0, colors.blame_heat(heat));
            if i == focus {
                let body = egui::Rect::from_min_max(egui::pos2(gutter_rect.right(), rect.top()), rect.max);
                painter.rect_filled(body, 0.0, colors.get(Token::BgSelected));
            }
            let y = rect.center().y;
            let cell = model::gutter(commit, now);
            let fg = colors.get(Token::FgPrimary);
            let dim = colors.get(Token::FgSecondary);
            painter.text(
                egui::pos2(rect.left() + 6.0, y),
                egui::Align2::LEFT_CENTER,
                &cell.hash,
                mono.clone(),
                fg,
            );
            let gutter_painter = painter.with_clip_rect(gutter_rect.shrink2(egui::vec2(4.0, 0.0)));
            gutter_painter.text(
                egui::pos2(rect.left() + 70.0, y),
                egui::Align2::LEFT_CENTER,
                &cell.author,
                small.clone(),
                fg,
            );
            gutter_painter.text(
                egui::pos2(gutter_rect.right() - 6.0, y),
                egui::Align2::RIGHT_CENTER,
                &cell.when,
                small.clone(),
                dim,
            );
            painter.text(
                egui::pos2(gutter_rect.right() + NUMBER_WIDTH - 8.0, y),
                egui::Align2::RIGHT_CENTER,
                line.final_line.to_string(),
                mono.clone(),
                colors.get(Token::DiffGutterFg),
            );
            painter.text(
                egui::pos2(gutter_rect.right() + NUMBER_WIDTH, y),
                egui::Align2::LEFT_CENTER,
                crate::git::blame::expand_tabs(&line.text),
                mono.clone(),
                fg,
            );

            let gutter = ui.interact(gutter_rect, ui.id().with(("blame_gutter", i)), egui::Sense::click());
            if gutter.hovered() && !commit.is_uncommitted() {
                hovered.push(commit.id.clone());
            }
            let message = messages.get(&commit.id).cloned().flatten();
            let gutter = gutter.on_hover_text(model::hover_text(commit, message.as_deref(), now));
            if gutter.clicked() {
                act = Some(Act::SelectCommit(commit.id.clone()));
            }
            if resp.clicked() {
                act = Some(Act::Focus(i));
            }
            let mod_label = crate::platform::primary_modifier_name();
            for r in [&resp, &gutter] {
                r.context_menu(|ui| {
                    if ui.button(format!("Blame parent of this line's commit  {mod_label}+←")).clicked() {
                        act = Some(Act::BackAt(i));
                        ui.close();
                    }
                    if !commit.is_uncommitted() {
                        if ui.button("Select commit in graph").clicked() {
                            act = Some(Act::SelectCommit(commit.id.clone()));
                            ui.close();
                        }
                        if ui.button("Copy hash").clicked() {
                            act = Some(Act::CopyHash(commit.id.clone()));
                            ui.close();
                        }
                    }
                });
            }
        }
    });
    ui.ctx().data_mut(|d| d.insert_temp(hover_id(), hovered));
    act
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::blame::{BlameCommit, BlameLine};

    const TIP: &str = "0123456789abcdef0123456789abcdef01234567";

    fn blame_with_parent() -> Blame {
        let parent = "p".repeat(40);
        let child = BlameCommit {
            id: "c".repeat(40),
            previous: Some((parent, "f".into())),
            author_time: 2,
            ..BlameCommit::default()
        };
        let root = BlameCommit { id: "r".repeat(40), author_time: 1, ..BlameCommit::default() };
        Blame {
            commits: vec![child, root],
            lines: vec![
                BlameLine { commit: 0, orig_line: 3, final_line: 1, text: "a".into() },
                BlameLine { commit: 1, orig_line: 1, final_line: 2, text: "b".into() },
            ],
        }
    }

    fn pane_with(blame: Blame) -> GitPane {
        let mut pane = super::super::tests::new_test_pane();
        let blame = Arc::new(blame);
        pane.file_views.blame = Some(BlameState {
            stack: BlameStack::new(Frame::new(Some(TIP.into()), "f", 0)),
            heat: model::heat(&blame),
            widest: 0,
            blame: Some(blame),
            loading: false,
            req: 0,
            error: None,
            scroll_to_focus: false,
        });
        pane
    }

    #[test]
    fn blame_back_pushes_the_parent_and_forward_returns_from_the_cache() {
        let mut pane = pane_with(blame_with_parent());
        let ctx = egui::Context::default();
        let mut events = Vec::new();
        pane.file_views.cache.push((
            (Some(TIP.into()), "f".into()),
            pane.file_views.blame.as_ref().unwrap().blame.clone().unwrap(),
        ));
        pane.apply_blame_act(&ctx, Act::Back, &mut events);
        let state = pane.file_views.blame.as_ref().unwrap();
        assert_eq!(state.stack.pos(), 1);
        assert_eq!(state.stack.current(), &Frame::new(Some("p".repeat(40)), "f", 2));
        assert!(state.loading, "the parent is not cached: git blame runs");
        pane.apply_blame_act(&ctx, Act::Forward, &mut events);
        let state = pane.file_views.blame.as_ref().unwrap();
        assert_eq!(state.stack.pos(), 0);
        assert!(!state.loading && state.blame.is_some(), "served from the cache");
        assert!(events.is_empty());
    }

    #[test]
    fn blame_back_on_the_introducing_commit_is_a_toast() {
        let mut pane = pane_with(blame_with_parent());
        let mut events = Vec::new();
        pane.apply_blame_act(&egui::Context::default(), Act::BackAt(1), &mut events);
        assert_eq!(pane.file_views.blame.as_ref().unwrap().stack.pos(), 0);
        assert!(matches!(&events[..], [GitEvent::Toast(t)] if t.title.contains("rrrrrrr")));
    }

    #[test]
    fn opening_maximizes_and_closing_restores() {
        let mut pane = super::super::tests::new_test_pane();
        let ctx = egui::Context::default();
        pane.open_blame(&ctx, FileTarget { rev: Some("HEAD".into()), path: "f".into(), line: Some(5) });
        assert_eq!(pane.file_views.blame.as_ref().unwrap().stack.current().line, 4);
        assert_eq!(pane.file_views.pending, [GitEvent::MaximizePane(true)]);
        pane.file_views.pending.clear();
        let mut events = Vec::new();
        pane.apply_blame_act(&ctx, Act::Close, &mut events);
        assert!(pane.file_views.blame.is_none());
        assert_eq!(events, [GitEvent::MaximizePane(false)]);
    }

    #[test]
    fn the_working_tree_is_never_served_from_or_kept_in_the_cache() {
        let mut pane = super::super::tests::new_test_pane();
        let ctx = egui::Context::default();
        pane.file_views.cache.push(((None, "f".into()), Arc::new(blame_with_parent())));
        pane.open_blame(&ctx, FileTarget::worktree("f"));
        let state = pane.file_views.blame.as_ref().unwrap();
        assert!(state.loading && state.blame.is_none(), "an edit since the last blame must show");
        let req = state.req;
        pane.file_views.cache.clear();
        let mut events = Vec::new();
        pane.apply_blame(req, Ok(blame_with_parent()), &mut events);
        assert!(pane.file_views.blame.as_ref().unwrap().blame.is_some());
        assert!(pane.file_views.cache.is_empty());
    }

    #[test]
    fn actions_on_the_current_line_wait_for_its_blame() {
        let mut pane = pane_with(blame_with_parent());
        let ctx = egui::Context::default();
        let mut events = Vec::new();
        pane.apply_blame_act(&ctx, Act::Back, &mut events);
        let state = pane.file_views.blame.as_ref().unwrap();
        assert!(state.loading && state.blame.is_none(), "the old frame's blame is dropped");
        pane.apply_blame_act(&ctx, Act::Back, &mut events);
        pane.apply_blame_act(&ctx, Act::Move(1), &mut events);
        let state = pane.file_views.blame.as_ref().unwrap();
        assert_eq!((state.stack.pos(), state.stack.current().line), (1, 2), "ignored while loading");
    }

    #[test]
    fn moving_the_focus_stays_in_the_file() {
        let mut pane = pane_with(blame_with_parent());
        let ctx = egui::Context::default();
        let mut events = Vec::new();
        pane.apply_blame_act(&ctx, Act::Move(30), &mut events);
        assert_eq!(pane.file_views.blame.as_ref().unwrap().stack.current().line, 1);
        pane.apply_blame_act(&ctx, Act::Move(-30), &mut events);
        assert_eq!(pane.file_views.blame.as_ref().unwrap().stack.current().line, 0);
    }
}
