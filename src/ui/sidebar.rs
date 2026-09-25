//! The sidebar (§3, §5.22, W2, W3): Repo › Remote › Workspace tree. Workspace rows show a
//! status dot (shape + color, §4), the name, a subtitle with location · branch (when it differs
//! from the name) · ↑↓ · dirty count, listening ports (clickable), and the last notification
//! line with its age. `⋯` on hover opens the same menu as right-click. Empty state: W20.

use super::chrome::status_token;
use super::theme::Colors;
use crate::agent::status::Status;
use crate::model::state::{AppState, RemoteGroup, Repo, Workspace};
use crate::model::theme::Token;
use crate::util::relative_time;
use std::collections::HashMap;

/// Live, non-persisted facts for one workspace row.
#[derive(Debug, Clone, Default)]
pub struct RowInfo {
    pub status: Status,
    /// Heuristic-only status: draw the dot hollow.
    pub low_confidence: bool,
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub dirty: usize,
    pub ports: Vec<u16>,
    /// Last notification text and its Unix time.
    pub last_message: Option<(String, u64)>,
    /// "reconnecting 12s", "hibernated", …
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarAction {
    Select(String),
    NewWorkspace,
    Close(String),
    Rename(String, String),
    MoveUp(String),
    MoveDown(String),
    OpenPort(u16),
    RevealInFileManager(String),
    CopyPath(String),
    OpenInExternalTerminal(String),
    ToggleRepo(String),
    ForgetRepo(String),
}

/// Ephemeral, per-session UI state that does not belong in `AppState`: the search query and
/// which row (if any) is mid-rename. Lives in egui memory, keyed off the sidebar's own `Id`.
#[derive(Clone, Default)]
struct UiState {
    search: String,
    renaming: Option<String>,
    rename_buf: String,
}

/// Case-insensitive substring match; an empty query matches everything (§3 "search field").
fn matches_search(name: &str, query_lower: &str) -> bool {
    query_lower.is_empty() || name.to_lowercase().contains(query_lower)
}

/// The workspace row subtitle (W3): `location · branch · ↑a ↓b · dirty`, or — when the row
/// carries a `note` such as "hibernated" or "reconnecting 12s" — `location · note` in its
/// place (the remote-workspace variant of W3). `branch` is omitted when it equals `name`
/// (spec: "branch when it differs from the name").
fn subtitle_line(
    location: &str,
    name: &str,
    branch: Option<&str>,
    ahead: u32,
    behind: u32,
    dirty: usize,
    note: Option<&str>,
) -> String {
    if let Some(note) = note {
        return format!("{location} · {note}");
    }
    let mut parts = vec![location.to_string()];
    if let Some(b) = branch
        && b != name
    {
        parts.push(b.to_string());
    }
    parts.push(format!("↑{ahead} ↓{behind}"));
    parts.push(dirty.to_string());
    parts.join(" · ")
}

fn repo_workspace_count(repo: &Repo) -> usize {
    repo.remotes.iter().map(|g| g.workspaces.len()).sum()
}

pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    rows: &HashMap<String, RowInfo>,
    colors: &Colors,
    now: u64,
) -> Option<SidebarAction> {
    let mem_id = ui.id().with("sidebar-ui-state");
    let mut mem: UiState = ui.data_mut(|d| d.get_temp(mem_id)).unwrap_or_default();
    let mut action = None;

    ui.horizontal(|ui| {
        ui.colored_label(colors.get(Token::FgSecondary), "🔍");
        ui.add(
            egui::TextEdit::singleline(&mut mem.search)
                .hint_text("Search workspaces")
                .desired_width(f32::INFINITY)
                .font(egui::FontId::proportional(13.0)),
        );
    });
    ui.separator();

    let home = crate::paths::home();
    let query = mem.search.trim().to_lowercase();

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if state.repos.is_empty() {
            ui.add_space(8.0);
            ui.colored_label(colors.get(Token::FgSecondary), "No workspaces yet.");
            if ui.link("New workspace").clicked() {
                action = Some(SidebarAction::NewWorkspace);
            }
            return;
        }
        for repo in &state.repos {
            if let Some(a) =
                repo_section(ui, repo, rows, colors, now, home.as_deref(), &query, &mut mem, state)
            {
                action = Some(a);
            }
        }
    });

    ui.separator();
    if ui.link("+ New workspace").clicked() {
        action = Some(SidebarAction::NewWorkspace);
    }

    ui.data_mut(|d| d.insert_temp(mem_id, mem));
    action
}

#[allow(clippy::too_many_arguments)]
fn repo_section(
    ui: &mut egui::Ui,
    repo: &Repo,
    rows: &HashMap<String, RowInfo>,
    colors: &Colors,
    now: u64,
    home: Option<&std::path::Path>,
    query: &str,
    mem: &mut UiState,
    state: &AppState,
) -> Option<SidebarAction> {
    let mut action = None;

    let any_match =
        repo.remotes.iter().flat_map(|g| g.workspaces.iter()).any(|w| matches_search(&w.name, query));
    if !query.is_empty() && !any_match {
        return None;
    }

    let glyph = if repo.collapsed { "▸" } else { "▾" };
    let header = ui.add(egui::Button::new(format!("{glyph} {}", repo.name)).frame(false));
    if header.clicked() {
        action = Some(SidebarAction::ToggleRepo(repo.id.clone()));
    }
    header.context_menu(|ui| {
        if ui.button("Forget repo").clicked() {
            action = Some(SidebarAction::ForgetRepo(repo.id.clone()));
        }
    });

    // Searching force-expands a collapsed repo so matches stay visible.
    if repo.collapsed && query.is_empty() {
        return action;
    }

    if repo_workspace_count(repo) == 0 {
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.colored_label(colors.get(Token::FgSecondary), format!("No workspaces for {}.", repo.name));
        });
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            if ui.link("New workspace").clicked() {
                action = Some(SidebarAction::NewWorkspace);
            }
        });
        return action;
    }

    for group in &repo.remotes {
        let group_workspaces: Vec<&Workspace> =
            group.workspaces.iter().filter(|w| matches_search(&w.name, query)).collect();
        if group_workspaces.is_empty() {
            continue;
        }
        remote_header(ui, group, colors);
        for ws in group_workspaces {
            let selected = state.active.as_deref() == Some(ws.id.as_str());
            if let Some(a) = workspace_row(ui, ws, rows.get(&ws.id), colors, now, home, selected, mem) {
                action = Some(a);
            }
        }
    }

    action
}

fn remote_header(ui: &mut egui::Ui, group: &RemoteGroup, colors: &Colors) {
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        ui.label(format!("▾ {}", group.name));
        if let Some(url) = &group.url {
            ui.colored_label(colors.get(Token::FgSecondary), url);
        }
    });
}

fn row_menu_items(ui: &mut egui::Ui, ws: &Workspace, mem: &mut UiState) -> Option<SidebarAction> {
    let mut result = None;
    if ui.button("Rename").clicked() {
        mem.renaming = Some(ws.id.clone());
        mem.rename_buf = ws.name.clone();
    }
    if ui.button("Move up").clicked() {
        result = Some(SidebarAction::MoveUp(ws.id.clone()));
    }
    if ui.button("Move down").clicked() {
        result = Some(SidebarAction::MoveDown(ws.id.clone()));
    }
    if ui.button("Open in external terminal").clicked() {
        result = Some(SidebarAction::OpenInExternalTerminal(ws.id.clone()));
    }
    if ui.button("Reveal in file manager").clicked() {
        result = Some(SidebarAction::RevealInFileManager(ws.id.clone()));
    }
    if ui.button("Copy path").clicked() {
        result = Some(SidebarAction::CopyPath(ws.id.clone()));
    }
    ui.separator();
    if ui.button("Close").clicked() {
        result = Some(SidebarAction::Close(ws.id.clone()));
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn workspace_row(
    ui: &mut egui::Ui,
    ws: &Workspace,
    info: Option<&RowInfo>,
    colors: &Colors,
    now: u64,
    home: Option<&std::path::Path>,
    selected: bool,
    mem: &mut UiState,
) -> Option<SidebarAction> {
    let default_info = RowInfo::default();
    let info = info.unwrap_or(&default_info);
    let mut action = None;
    let is_renaming = mem.renaming.as_deref() == Some(ws.id.as_str());

    let bg = if selected { colors.get(Token::BgSelected) } else { egui::Color32::TRANSPARENT };
    let frame = egui::Frame::default().fill(bg).inner_margin(6).corner_radius(4).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let glyph = info.status.glyph();
            if !glyph.is_empty() {
                let token = status_token(info.status);
                let color = if info.low_confidence { colors.faded(token, 0.5) } else { colors.get(token) };
                ui.colored_label(color, glyph);
            }
            if is_renaming {
                let field = ui.add(egui::TextEdit::singleline(&mut mem.rename_buf).desired_width(140.0));
                field.request_focus();
                if field.lost_focus() {
                    let committed =
                        ui.input(|i| i.key_pressed(egui::Key::Enter)) && !mem.rename_buf.trim().is_empty();
                    if committed {
                        action =
                            Some(SidebarAction::Rename(ws.id.clone(), mem.rename_buf.trim().to_string()));
                    }
                    mem.renaming = None;
                }
            } else {
                ui.label(&ws.name);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let dots = ui.menu_button("⋯", |ui| row_menu_items(ui, ws, mem));
                if let Some(Some(a)) = dots.inner {
                    action = Some(a);
                }
            });
        });

        let subtitle = subtitle_line(
            &ws.location.display(home),
            &ws.name,
            info.branch.as_deref(),
            info.ahead,
            info.behind,
            info.dirty,
            info.note.as_deref(),
        );
        ui.colored_label(colors.get(Token::FgSecondary), subtitle);

        if !info.ports.is_empty() {
            ui.horizontal(|ui| {
                for &port in &info.ports {
                    if ui.link(format!(":{port}")).clicked() {
                        action = Some(SidebarAction::OpenPort(port));
                    }
                }
            });
        }

        if let Some((text, at)) = &info.last_message {
            ui.horizontal(|ui| {
                // Age first, right-aligned; the message takes the rest, elided to one line (W3).
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.colored_label(colors.get(Token::FgSecondary), relative_time(now, *at));
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        let label =
                            egui::RichText::new(format!("\"{text}\"")).color(colors.get(Token::FgSecondary));
                        ui.add(egui::Label::new(label).truncate());
                    });
                });
            });
        }
    });

    // A row-wide click selects the workspace, but only once no inner widget (port link,
    // `⋯` menu, rename field) already claimed this click.
    let row_id = ui.id().with(("sidebar-row", ws.id.as_str()));
    let row_resp = ui.interact(frame.response.rect, row_id, egui::Sense::click());
    row_resp.context_menu(|ui| {
        if let Some(a) = row_menu_items(ui, ws, mem) {
            action = Some(a);
        }
    });
    if action.is_none() && !is_renaming && row_resp.clicked() {
        action = Some(SidebarAction::Select(ws.id.clone()));
    }

    action
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- matches_search ----

    #[test]
    fn matches_search_is_case_insensitive_substring() {
        assert!(matches_search("Fix-Crash", "crash"));
        assert!(matches_search("fix-crash", ""));
        assert!(!matches_search("fix-crash", "nope"));
    }

    // ---- subtitle_line (W3 row anatomy) ----

    #[test]
    fn subtitle_matches_w3_example() {
        assert_eq!(
            subtitle_line("~/w/conduit", "fix-crash", Some("main"), 0, 3, 2, None),
            "~/w/conduit · main · ↑0 ↓3 · 2"
        );
    }

    #[test]
    fn subtitle_omits_branch_when_it_equals_the_name() {
        assert_eq!(
            subtitle_line("~/w/c-oauth", "feat-oauth", Some("feat-oauth"), 2, 0, 0, None),
            "~/w/c-oauth · ↑2 ↓0 · 0"
        );
    }

    #[test]
    fn subtitle_uses_note_instead_of_branch_and_counts_for_remote_variant() {
        assert_eq!(
            subtitle_line("gpu-box:~/amalgum", "main", None, 0, 0, 0, Some("reconnecting 12s")),
            "gpu-box:~/amalgum · reconnecting 12s"
        );
        assert_eq!(
            subtitle_line("gpu-box:~/g", "main", Some("main"), 0, 0, 0, Some("hibernated")),
            "gpu-box:~/g · hibernated"
        );
    }

    #[test]
    fn subtitle_omits_branch_segment_when_none() {
        assert_eq!(subtitle_line("~/repo", "main", None, 1, 0, 0, None), "~/repo · ↑1 ↓0 · 0");
    }

    // ---- repo_workspace_count ----

    #[test]
    fn repo_workspace_count_sums_across_remotes() {
        let repo = Repo {
            id: "r1".into(),
            name: "conduit".into(),
            collapsed: false,
            remotes: vec![
                RemoteGroup {
                    name: "origin".into(),
                    url: None,
                    collapsed: false,
                    workspaces: vec![ws("w0"), ws("w1")],
                },
                RemoteGroup {
                    name: "upstream".into(),
                    url: None,
                    collapsed: false,
                    workspaces: vec![ws("w2")],
                },
            ],
        };
        assert_eq!(repo_workspace_count(&repo), 3);
        let empty = Repo { id: "r2".into(), name: "amalgum".into(), collapsed: false, remotes: vec![] };
        assert_eq!(repo_workspace_count(&empty), 0);
    }

    fn ws(id: &str) -> Workspace {
        Workspace {
            id: id.to_string(),
            name: id.to_string(),
            location: crate::git::Location::Local { path: std::path::PathBuf::from("/tmp/x") }, // portability: allow
            tabs: Vec::new(),
            active_tab: 0,
            git_view: crate::model::state::GitView::default(),
            git_pane_open: false,
        }
    }
}
