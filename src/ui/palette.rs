//! Command palette (§5.19, W8): centered overlay, fuzzy input (`model::fuzzy`), results grouped
//! Actions, Workspaces, Branches, Tags, Stashes, Worktrees, Recent locations, Commits; group
//! prefixes `> @ # $ ~ !`; ↑/↓/Enter/Esc; shortcuts right-aligned.

use super::theme::Colors;
use crate::git::Location;
use crate::model::fuzzy::{self, Group, Match};
use crate::model::keymap::Action;
use crate::model::theme::Token;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Action(Action),
    Workspace(String),
    Branch(String),
    Location(Location),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub group: Group,
    pub label: String,
    /// Right-hand detail: shortcut label, status text, or location.
    pub detail: String,
    pub target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Open,
    Cancelled,
    Chosen(Target),
}

/// At most this many results are shown at once (§5.19).
const MAX_RESULTS: usize = 50;

#[derive(Debug)]
pub struct Palette {
    query: String,
    selected: usize,
    /// Set on construction; consumed the first frame `show` runs, to request keyboard focus
    /// on the input field exactly once.
    just_opened: bool,
}

impl Default for Palette {
    fn default() -> Self {
        Self { query: String::new(), selected: 0, just_opened: true }
    }
}

/// A candidate belongs to the current search unless a group prefix (`>`, `@`, …) names a
/// different group, or it is the Commits group appearing unprefixed with a query that doesn't
/// look like a hash (§5.19: "Commits … only when the query is 7+ hex chars or prefixed `#`").
fn matches_group_filter(group: Group, filter: Option<Group>, rest: &str) -> bool {
    match filter {
        Some(g) => group == g,
        None => group != Group::Commits || fuzzy::looks_like_hash(rest),
    }
}

/// Filter `items` by the query's group prefix, fuzzy-rank each surviving group by the rest of
/// the query, then flatten in [`Group`] (spec) order and cap at [`MAX_RESULTS`].
fn build_results<'a>(query: &str, items: &'a [Item]) -> Vec<(&'a Item, Match)> {
    let (filter, rest) = fuzzy::split_prefix(query);
    let mut by_group: BTreeMap<Group, Vec<&'a Item>> = BTreeMap::new();
    for item in items {
        if matches_group_filter(item.group, filter, rest) {
            by_group.entry(item.group).or_default().push(item);
        }
    }
    let mut out = Vec::new();
    for (_, group_items) in by_group {
        let labels: Vec<&str> = group_items.iter().map(|i| i.label.as_str()).collect();
        for (idx, m) in fuzzy::rank(rest, &labels) {
            out.push((group_items[idx], m));
        }
    }
    out.truncate(MAX_RESULTS);
    out
}

fn group_label(group: Group) -> &'static str {
    match group {
        Group::Actions => "Actions",
        Group::Workspaces => "Workspaces",
        Group::Branches => "Branches",
        Group::Tags => "Tags",
        Group::Stashes => "Stashes",
        Group::Worktrees => "Worktrees",
        Group::Locations => "Recent locations",
        Group::Commits => "Commits",
    }
}

fn wrap_next(selected: usize, len: usize) -> usize {
    if len == 0 { 0 } else { (selected + 1) % len }
}

fn wrap_prev(selected: usize, len: usize) -> usize {
    if len == 0 { 0 } else { (selected + len - 1) % len }
}

fn highlighted_label(label: &str, positions: &[usize], colors: &Colors) -> egui::text::LayoutJob {
    let matched_at: std::collections::HashSet<usize> = positions.iter().copied().collect();
    let font = egui::FontId::proportional(13.0);
    let normal = egui::text::TextFormat::simple(font.clone(), colors.get(Token::FgPrimary));
    let matched = egui::text::TextFormat::simple(font, colors.get(Token::Accent));
    let mut job = egui::text::LayoutJob::default();
    for (i, ch) in label.chars().enumerate() {
        let format = if matched_at.contains(&i) { matched.clone() } else { normal.clone() };
        job.append(&ch.to_string(), 0.0, format);
    }
    job
}

impl Palette {
    pub fn show(&mut self, ctx: &egui::Context, items: &[Item], colors: &Colors) -> Outcome {
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let up = ctx.input(|i| i.key_pressed(egui::Key::ArrowUp));
        let down = ctx.input(|i| i.key_pressed(egui::Key::ArrowDown));
        let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));

        let results = build_results(&self.query, items);
        if results.is_empty() {
            self.selected = 0;
        } else {
            if down {
                self.selected = wrap_next(self.selected, results.len());
            }
            if up {
                self.selected = wrap_prev(self.selected, results.len());
            }
            if self.selected >= results.len() {
                self.selected = 0;
            }
        }

        let mut outcome = Outcome::Open;
        let screen = ctx.content_rect();

        let area = egui::Area::new(egui::Id::new("amalgum_command_palette"))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, screen.height() * 0.2))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(colors.get(Token::BgRaised))
                    .stroke(egui::Stroke::new(1.0, colors.get(Token::Border)))
                    .corner_radius(8)
                    .inner_margin(10)
                    .show(ui, |ui| {
                        ui.set_width(560.0);
                        let field = ui.add(
                            egui::TextEdit::singleline(&mut self.query)
                                .hint_text("Type a command…")
                                .font(egui::FontId::proportional(13.0))
                                .desired_width(f32::INFINITY),
                        );
                        if std::mem::take(&mut self.just_opened) {
                            field.request_focus();
                        }
                        if field.changed() {
                            self.selected = 0;
                        }
                        ui.separator();

                        let mut clicked = None;
                        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                            if results.is_empty() {
                                ui.add_space(8.0);
                                ui.colored_label(colors.get(Token::FgSecondary), "No matches.");
                                return;
                            }
                            let mut last_group = None;
                            for (i, (item, m)) in results.iter().enumerate() {
                                if last_group != Some(item.group) {
                                    last_group = Some(item.group);
                                    ui.add_space(4.0);
                                    ui.colored_label(colors.get(Token::FgSecondary), group_label(item.group));
                                }
                                let selected = i == self.selected;
                                let job = highlighted_label(&item.label, &m.positions, colors);
                                let bg = if selected {
                                    colors.get(Token::BgSelected)
                                } else {
                                    egui::Color32::TRANSPARENT
                                };
                                let row = egui::Frame::default()
                                    .fill(bg)
                                    .inner_margin(6)
                                    .corner_radius(4)
                                    .show(ui, |ui| {
                                        egui::Sides::new().show(
                                            ui,
                                            |ui| {
                                                ui.add(egui::Label::new(job).truncate());
                                            },
                                            |ui| {
                                                ui.colored_label(
                                                    colors.get(Token::FgSecondary),
                                                    &item.detail,
                                                );
                                            },
                                        );
                                    });
                                let resp = ui.interact(
                                    row.response.rect,
                                    ui.id().with(("palette-row", i)),
                                    egui::Sense::click(),
                                );
                                if selected {
                                    resp.scroll_to_me(None);
                                }
                                if resp.clicked() {
                                    clicked = Some(item.target.clone());
                                }
                            }
                        });
                        if let Some(target) = clicked {
                            outcome = Outcome::Chosen(target);
                        }
                    });
            });

        if esc {
            outcome = Outcome::Cancelled;
        } else if matches!(outcome, Outcome::Open) && enter && !results.is_empty() {
            outcome = Outcome::Chosen(results[self.selected].0.target.clone());
        } else if matches!(outcome, Outcome::Open) {
            let clicked_outside = ctx.input(|i| i.pointer.primary_clicked())
                && ctx.input(|i| i.pointer.interact_pos()).is_some_and(|p| !area.response.rect.contains(p));
            if clicked_outside {
                outcome = Outcome::Cancelled;
            }
        }

        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action_item(label: &str) -> Item {
        Item {
            group: Group::Actions,
            label: label.to_string(),
            detail: String::new(),
            target: Target::Action(Action::Push),
        }
    }
    fn workspace_item(label: &str) -> Item {
        Item {
            group: Group::Workspaces,
            label: label.to_string(),
            detail: String::new(),
            target: Target::Workspace(label.to_string()),
        }
    }
    fn commit_item(label: &str) -> Item {
        Item {
            group: Group::Commits,
            label: label.to_string(),
            detail: String::new(),
            target: Target::Branch(label.to_string()),
        }
    }

    // ---- build_results grouping and ranking ----

    #[test]
    fn groups_appear_in_spec_order() {
        let items = vec![workspace_item("conduit"), action_item("Push to origin")];
        let results = build_results("", &items);
        assert_eq!(results[0].0.group, Group::Actions, "Actions sorts before Workspaces");
        assert_eq!(results[1].0.group, Group::Workspaces);
    }

    #[test]
    fn ranks_within_a_group_by_fuzzy_score() {
        let items = vec![action_item("Push all tags"), action_item("Push to origin")];
        let results = build_results("push to", &items);
        assert_eq!(results[0].0.label, "Push to origin");
    }

    #[test]
    fn empty_query_returns_everything_up_to_the_cap() {
        let items: Vec<Item> = (0..60).map(|i| action_item(&format!("Action {i}"))).collect();
        let results = build_results("", &items);
        assert_eq!(results.len(), MAX_RESULTS);
    }

    #[test]
    fn non_matching_query_excludes_the_item() {
        let items = vec![action_item("Push to origin")];
        assert!(build_results("zzz", &items).is_empty());
    }

    // ---- group prefixes (§5.19) ----

    #[test]
    fn prefix_restricts_to_one_group() {
        let items = vec![action_item("push"), workspace_item("push-thing")];
        let results = build_results(">push", &items);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.group, Group::Actions);
    }

    #[test]
    fn workspace_prefix_selects_workspaces_group() {
        let items = vec![action_item("conduit"), workspace_item("conduit")];
        let results = build_results("!conduit", &items);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.group, Group::Workspaces);
    }

    // ---- Commits group gating (§5.19) ----

    #[test]
    fn commits_hidden_unprefixed_unless_query_looks_like_a_hash() {
        let items = vec![action_item("push"), commit_item("ab12cd3")];
        // Short, non-hash-length query: Commits excluded even if it happened to match.
        let results = build_results("ab", &items);
        assert!(results.iter().all(|(i, _)| i.group != Group::Commits));

        // 7+ hex chars unprefixed: Commits included.
        let results = build_results("ab12cd3", &items);
        assert!(results.iter().any(|(i, _)| i.group == Group::Commits));
    }

    #[test]
    fn hash_prefix_always_shows_commits() {
        let items = vec![commit_item("ab12cd3")];
        let results = build_results("#ab", &items);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.group, Group::Commits);
    }

    // ---- wrap_next / wrap_prev ----

    #[test]
    fn selection_wraps_both_directions() {
        assert_eq!(wrap_next(2, 3), 0);
        assert_eq!(wrap_next(0, 3), 1);
        assert_eq!(wrap_prev(0, 3), 2);
        assert_eq!(wrap_prev(1, 3), 0);
    }

    #[test]
    fn wrap_on_empty_list_is_always_zero() {
        assert_eq!(wrap_next(0, 0), 0);
        assert_eq!(wrap_prev(0, 0), 0);
    }

    // ---- Default ----

    #[test]
    fn default_palette_requests_focus_once() {
        let p = Palette::default();
        assert!(p.just_opened);
        assert_eq!(p.query, "");
        assert_eq!(p.selected, 0);
    }
}
