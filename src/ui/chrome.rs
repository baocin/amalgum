//! Window chrome (§3, §5.24, §5.26, §5.29): the in-window menu bar generated from the keymap's
//! action table, the status bar, toasts (bottom-left over the sidebar, never over a terminal
//! cursor or the commit button), and the notification panel (W14).

use super::theme::Colors;
use crate::agent::notifications::{Level, Store};
use crate::agent::status::Status;
use crate::git::status::RepoOp;
use crate::model::keymap::{self, Action, Keymap, Menu, Preset};
use crate::model::theme::Token;
use crate::util::relative_time;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    /// `danger` toasts never auto-dismiss (§5.24).
    Error,
}

/// A toast: one human line, and the exact command + stderr one click away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub level: ToastLevel,
    pub title: String,
    pub details: Option<String>,
}

impl Toast {
    pub fn error(title: impl Into<String>, details: impl Into<String>) -> Self {
        Self { level: ToastLevel::Error, title: title.into(), details: Some(details.into()) }
    }
    pub fn success(title: impl Into<String>) -> Self {
        Self { level: ToastLevel::Success, title: title.into(), details: None }
    }
    pub fn warning(title: impl Into<String>, details: impl Into<String>) -> Self {
        Self { level: ToastLevel::Warning, title: title.into(), details: Some(details.into()) }
    }
    pub fn info(title: impl Into<String>) -> Self {
        Self { level: ToastLevel::Info, title: title.into(), details: None }
    }
}

fn toast_glyph(level: ToastLevel) -> &'static str {
    match level {
        ToastLevel::Info => "ℹ",
        ToastLevel::Success => "✓",
        ToastLevel::Warning => "⚠",
        ToastLevel::Error => "✗",
    }
}

fn toast_token(level: ToastLevel) -> Token {
    match level {
        ToastLevel::Info => Token::Accent,
        ToastLevel::Success => Token::Success,
        ToastLevel::Warning => Token::Warning,
        ToastLevel::Error => Token::Danger,
    }
}

/// Non-error toasts auto-dismiss after this long unless hovered or expanded (§5.24).
const AUTO_DISMISS_SECS: f64 = 8.0;
/// At most this many toasts are ever shown at once, newest at the bottom (§3.1 W14).
const MAX_VISIBLE_TOASTS: usize = 5;

/// Pure arbitration for one toast: never for `danger`, never while hovered or expanded,
/// otherwise once `elapsed` (seconds since it was pushed) reaches [`AUTO_DISMISS_SECS`].
fn should_auto_dismiss(level: ToastLevel, elapsed: f64, hovered: bool, expanded: bool) -> bool {
    if level == ToastLevel::Error {
        return false;
    }
    if hovered || expanded {
        return false;
    }
    elapsed >= AUTO_DISMISS_SECS
}

#[derive(Debug)]
struct ToastEntry {
    toast: Toast,
    created: f64,
    expanded: bool,
}

/// Toast stack. Non-error toasts auto-dismiss after 8 s unless hovered or expanded.
#[derive(Debug, Default)]
pub struct Toasts {
    items: Vec<ToastEntry>,
}

impl Toasts {
    pub fn push(&mut self, toast: Toast, now: f64) {
        self.items.push(ToastEntry { toast, created: now, expanded: false });
    }

    /// Draw the stack anchored bottom-left; `now` is `ctx.input(|i| i.time)`.
    pub fn show(&mut self, ctx: &egui::Context, colors: &Colors, now: f64) {
        if self.items.is_empty() {
            return;
        }
        let start = self.items.len().saturating_sub(MAX_VISIBLE_TOASTS);
        let mut remove: Vec<usize> = Vec::new();
        let mut soonest_deadline: Option<f64> = None;

        egui::Area::new(egui::Id::new("amalgum_toasts"))
            .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(12.0, -40.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    for i in start..self.items.len() {
                        let entry = &mut self.items[i];
                        let elapsed = (now - entry.created).max(0.0);
                        let level = entry.toast.level;

                        let frame_resp = egui::Frame::default()
                            .fill(colors.get(Token::BgRaised))
                            .stroke(egui::Stroke::new(1.0, colors.get(Token::Border)))
                            .corner_radius(6)
                            .inner_margin(10)
                            .show(ui, |ui| {
                                ui.set_max_width(360.0);
                                ui.horizontal(|ui| {
                                    ui.colored_label(colors.get(toast_token(level)), toast_glyph(level));
                                    ui.label(&entry.toast.title);
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.small_button("×").clicked() {
                                            remove.push(i);
                                        }
                                    });
                                });
                                if let Some(details) = entry.toast.details.clone() {
                                    let label = if entry.expanded { "Details ▴" } else { "Details ▾" };
                                    if ui.small_button(label).clicked() {
                                        entry.expanded = !entry.expanded;
                                    }
                                    if entry.expanded {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&details)
                                                    .font(egui::FontId::monospace(12.0))
                                                    .color(colors.get(Token::FgSecondary)),
                                            )
                                            .wrap(),
                                        );
                                        if ui.button("Copy").clicked() {
                                            ui.ctx().copy_text(details);
                                        }
                                    }
                                }
                            });
                        let hovered = frame_resp.response.hovered();

                        if should_auto_dismiss(level, elapsed, hovered, entry.expanded) {
                            remove.push(i);
                        } else if level != ToastLevel::Error {
                            let deadline = entry.created + AUTO_DISMISS_SECS;
                            soonest_deadline =
                                Some(soonest_deadline.map_or(deadline, |d: f64| d.min(deadline)));
                        }
                    }
                });
            });

        remove.sort_unstable();
        remove.dedup();
        for i in remove.into_iter().rev() {
            self.items.remove(i);
        }

        if let Some(deadline) = soonest_deadline {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64((deadline - now).max(0.0)));
        }
    }
}

fn menu_label(menu: Menu) -> &'static str {
    match menu {
        Menu::App => "Amalgum",
        Menu::File => "File",
        Menu::Edit => "Edit",
        Menu::View => "View",
        Menu::Workspace => "Workspace",
        Menu::Repository => "Repository",
        Menu::Window => "Window",
        Menu::Help => "Help",
    }
}

fn actions_for(menu: Menu) -> Vec<Action> {
    keymap::table().iter().filter(|d| d.menu == Some(menu)).map(|d| d.action).collect()
}

/// Top-level menus and their items, in §5.26 order, items within each in table order. On macOS
/// the App menu (Settings, About, Quit) is its own top-level "Amalgum" menu; elsewhere there is
/// no OS-level app menu, so those items fold into File (Settings, Quit) and Help (About), per
/// §5.26's "Settings on Linux" / "Quit on Linux" / "About on Linux".
fn grouped_menus(preset: Preset) -> Vec<(&'static str, Vec<Action>)> {
    let is_mac = preset == Preset::MacOs;
    let mut out = Vec::new();
    if is_mac {
        out.push((menu_label(Menu::App), actions_for(Menu::App)));
    }
    let mut file = actions_for(Menu::File);
    let mut help = actions_for(Menu::Help);
    if !is_mac {
        file.push(Action::Settings);
        file.push(Action::Quit);
        help.push(Action::About);
    }
    out.push((menu_label(Menu::File), file));
    out.push((menu_label(Menu::Edit), actions_for(Menu::Edit)));
    out.push((menu_label(Menu::View), actions_for(Menu::View)));
    out.push((menu_label(Menu::Workspace), actions_for(Menu::Workspace)));
    out.push((menu_label(Menu::Repository), actions_for(Menu::Repository)));
    out.push((menu_label(Menu::Window), actions_for(Menu::Window)));
    out.push((menu_label(Menu::Help), help));
    out
}

/// Menu bar from `keymap::table()` grouped by `Menu`, each item labelled with its shortcut.
pub fn menu_bar(ui: &mut egui::Ui, keymap: &Keymap) -> Option<Action> {
    let mut clicked = None;
    egui::MenuBar::new().ui(ui, |ui| {
        for (label, actions) in grouped_menus(keymap.preset) {
            ui.menu_button(label, |ui| {
                for action in actions {
                    let def = keymap::def(action);
                    let shortcut =
                        keymap.bindings(action).first().map(|c| keymap.preset.label(c)).unwrap_or_default();
                    let button = egui::Button::new(def.label).shortcut_text(shortcut);
                    if ui.add(button).clicked() {
                        clicked = Some(action);
                    }
                }
            });
        }
    });
    clicked
}

/// Everything the status bar shows (W2 bottom row).
#[derive(Debug, Clone, Default)]
pub struct StatusInfo {
    pub branch: Option<String>,
    pub detached: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changed: usize,
    pub op: Option<(RepoOp, usize)>,
    /// Focused tab's agent status and label ("claude running 4m").
    pub agent: Option<(Status, String)>,
    pub ports: Vec<u16>,
    pub unread: usize,
    pub remote_state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusAction {
    OpenChanges,
    ToggleNotifications,
    OpenPort(u16),
    ContinueOp,
    AbortOp,
    CreateBranch,
}

/// Status dot color token (§5.29 urgency chain). Shared with the sidebar (§5.22).
pub(crate) fn status_token(status: Status) -> Token {
    match status {
        Status::None => Token::FgSecondary,
        Status::Disconnected => Token::Danger,
        Status::Hibernated => Token::StatusHibernated,
        Status::Idle => Token::StatusIdle,
        Status::Running => Token::Success,
        Status::NeedsInput => Token::Accent,
    }
}

/// "Merge in progress" / "Merge in progress · 3 conflicts".
fn op_label(op: RepoOp, conflicts: usize) -> String {
    if conflicts > 0 {
        format!("{} in progress · {conflicts} conflict{}", op.name(), if conflicts == 1 { "" } else { "s" })
    } else {
        format!("{} in progress", op.name())
    }
}

pub fn status_bar(ui: &mut egui::Ui, info: &StatusInfo, colors: &Colors) -> Option<StatusAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;

        // ---- Left: in-progress op, detached HEAD, or branch + ahead/behind/dirty ----
        if let Some((op, conflicts)) = &info.op {
            ui.colored_label(colors.get(Token::Danger), op_label(*op, *conflicts));
            if ui.small_button("Continue").clicked() {
                action = Some(StatusAction::ContinueOp);
            }
            if ui.small_button("Abort").clicked() {
                action = Some(StatusAction::AbortOp);
            }
        } else if let Some(name) = &info.detached {
            ui.colored_label(
                colors.get(Token::Warning),
                format!("{} {name} (detached)", super::fonts::BRANCH),
            );
            if ui.small_button("Create branch").clicked() {
                action = Some(StatusAction::CreateBranch);
            }
        } else if let Some(name) = &info.branch {
            ui.label(format!("{} {name}", super::fonts::BRANCH));
            ui.colored_label(colors.get(Token::Success), format!("↑{}", info.ahead));
            ui.colored_label(colors.get(Token::Warning), format!("↓{}", info.behind));
            if info.changed > 0 && ui.link(format!("{} changed", info.changed)).clicked() {
                action = Some(StatusAction::OpenChanges);
            }
        }

        ui.separator();

        // ---- Center: focused tab's agent status + this workspace's ports ----
        if let Some((status, label)) = &info.agent {
            let glyph = status.glyph();
            if !glyph.is_empty() {
                ui.colored_label(colors.get(status_token(*status)), glyph);
            }
            ui.label(label);
        }
        for &port in &info.ports {
            if ui.link(format!(":{port}")).clicked() {
                action = Some(StatusAction::OpenPort(port));
            }
        }

        // ---- Right: remote state + notification bell ----
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let bell = if info.unread > 0 { format!("🔔 {}", info.unread) } else { "🔔".to_string() };
            if ui.button(bell).clicked() {
                action = Some(StatusAction::ToggleNotifications);
            }
            if let Some(remote) = &info.remote_state {
                ui.colored_label(colors.get(Token::FgSecondary), remote);
            }
        });
    });
    action
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelAction {
    Focus { workspace: Option<String>, tab: Option<String> },
    Close,
}

fn level_glyph(level: Level) -> &'static str {
    match level {
        Level::Info => "ℹ",
        Level::Success => "✓",
        Level::Error => "✗",
        Level::NeedsInput => "◐",
    }
}

fn level_token(level: Level) -> Token {
    match level {
        Level::Info => Token::FgSecondary,
        Level::Success => Token::Success,
        Level::Error => Token::Danger,
        Level::NeedsInput => Token::Accent,
    }
}

/// "workspace / tab" row title (§3.1 W14): whichever of the two is present, joined; a bare
/// "Amalgum" when the notification names neither (e.g. an app-level toast).
fn notification_title(workspace: Option<&str>, tab: Option<&str>) -> String {
    match (workspace, tab) {
        (Some(w), Some(t)) => format!("{w} / {t}"),
        (Some(w), None) => w.to_string(),
        (None, Some(t)) => t.to_string(),
        (None, None) => "Amalgum".to_string(),
    }
}

struct NotifRow {
    level: Level,
    workspace: Option<String>,
    tab: Option<String>,
    title: String,
    body: String,
    time: u64,
    read: bool,
}

fn notification_row(
    ui: &mut egui::Ui,
    idx: usize,
    row: &NotifRow,
    colors: &Colors,
    now: u64,
) -> egui::Response {
    let bg = if row.read { egui::Color32::TRANSPARENT } else { colors.faded(Token::BgSelected, 0.6) };
    let inner = egui::Frame::default().fill(bg).corner_radius(4).inner_margin(8).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.colored_label(colors.get(level_token(row.level)), level_glyph(row.level));
            let title = egui::RichText::new(&row.title);
            ui.label(if row.read { title } else { title.strong() });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.colored_label(colors.get(Token::FgSecondary), relative_time(now, row.time));
            });
        });
        if !row.body.is_empty() {
            ui.colored_label(colors.get(Token::FgSecondary), &row.body);
        }
    });
    ui.interact(inner.response.rect, ui.id().with(("notif-row", idx)), egui::Sense::click())
}

/// The notification panel sliding in from the right (`Mod+Shift+I`, W14). Clicking a row
/// focuses its workspace/tab; **Clear all** empties the store; unread rows are bold.
pub fn notification_panel(
    ctx: &egui::Context,
    store: &mut Store,
    colors: &Colors,
    now: u64,
) -> Option<PanelAction> {
    let rows: Vec<NotifRow> = store
        .iter()
        .map(|n| NotifRow {
            level: n.level,
            workspace: n.workspace.clone(),
            tab: n.tab.clone(),
            title: notification_title(n.workspace.as_deref(), n.tab.as_deref()),
            body: n.body.clone(),
            time: n.time,
            read: n.read,
        })
        .collect();

    let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    let mut clear_all = false;
    let mut focus: Option<(Option<String>, Option<String>)> = None;

    egui::Area::new(egui::Id::new("amalgum_notification_panel"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 34.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::default()
                .fill(colors.get(Token::BgRaised))
                .stroke(egui::Stroke::new(1.0, colors.get(Token::Border)))
                .corner_radius(8)
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.set_width(360.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Notifications").size(15.0).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("×").clicked() {
                                close = true;
                            }
                            if ui.button("Clear all").clicked() {
                                clear_all = true;
                            }
                        });
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(480.0).show(ui, |ui| {
                        if rows.is_empty() {
                            ui.add_space(8.0);
                            ui.colored_label(
                                colors.get(Token::FgSecondary),
                                "Nothing yet. Run `amalgum hooks setup` so agents can notify you.",
                            );
                            if ui.button("Set up hooks").clicked() {
                                ui.ctx().copy_text("amalgum hooks setup".to_string());
                            }
                        } else {
                            for (i, row) in rows.iter().enumerate() {
                                if notification_row(ui, i, row, colors, now).clicked() {
                                    focus = Some((row.workspace.clone(), row.tab.clone()));
                                }
                            }
                        }
                    });
                });
        });

    if clear_all {
        store.clear();
    }
    if let Some((_, Some(tab))) = &focus {
        store.mark_tab_read(tab);
    }

    match focus {
        Some((workspace, tab)) => Some(PanelAction::Focus { workspace, tab }),
        None if close => Some(PanelAction::Close),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- should_auto_dismiss (§5.24) ----

    #[test]
    fn error_toasts_never_auto_dismiss() {
        assert!(!should_auto_dismiss(ToastLevel::Error, 1000.0, false, false));
    }

    #[test]
    fn non_error_dismisses_at_the_boundary() {
        assert!(!should_auto_dismiss(ToastLevel::Info, 7.999, false, false));
        assert!(should_auto_dismiss(ToastLevel::Info, 8.0, false, false));
        assert!(should_auto_dismiss(ToastLevel::Success, 30.0, false, false));
    }

    #[test]
    fn hovered_or_expanded_blocks_dismissal_even_when_overdue() {
        assert!(!should_auto_dismiss(ToastLevel::Warning, 100.0, true, false));
        assert!(!should_auto_dismiss(ToastLevel::Warning, 100.0, false, true));
        assert!(!should_auto_dismiss(ToastLevel::Warning, 100.0, true, true));
    }

    #[test]
    fn fresh_toast_is_not_dismissed() {
        assert!(!should_auto_dismiss(ToastLevel::Info, 0.0, false, false));
    }

    // ---- grouped_menus (§5.26) ----

    #[test]
    fn mac_has_a_dedicated_app_menu_named_amalgum() {
        let menus = grouped_menus(Preset::MacOs);
        assert_eq!(menus[0].0, "Amalgum");
        assert_eq!(menus[0].1, vec![Action::Settings, Action::About, Action::Quit]);
    }

    #[test]
    fn top_level_menu_order_matches_5_26() {
        let menus = grouped_menus(Preset::MacOs);
        let labels: Vec<&str> = menus.iter().map(|(l, _)| *l).collect();
        assert_eq!(
            labels,
            vec!["Amalgum", "File", "Edit", "View", "Workspace", "Repository", "Window", "Help"]
        );

        let linux = grouped_menus(Preset::LinuxCtrlShift);
        let linux_labels: Vec<&str> = linux.iter().map(|(l, _)| *l).collect();
        assert_eq!(linux_labels, vec!["File", "Edit", "View", "Workspace", "Repository", "Window", "Help"]);
    }

    #[test]
    fn linux_folds_settings_and_quit_into_file_and_about_into_help() {
        let menus = grouped_menus(Preset::LinuxCtrlShift);
        let file = &menus.iter().find(|(l, _)| *l == "File").unwrap().1;
        assert!(file.ends_with(&[Action::Settings, Action::Quit]));
        let help = &menus.iter().find(|(l, _)| *l == "Help").unwrap().1;
        assert!(help.ends_with(&[Action::About]));
    }

    #[test]
    fn items_within_a_menu_follow_table_order() {
        let menus = grouped_menus(Preset::MacOs);
        let file = &menus.iter().find(|(l, _)| *l == "File").unwrap().1;
        // Table order (§ actions defined before others in the same menu).
        assert_eq!(file[0], Action::OpenFolder);
        assert_eq!(file[1], Action::CloneRepository);
    }

    #[test]
    fn every_menu_tagged_action_appears_exactly_once_across_menus() {
        let menus = grouped_menus(Preset::MacOs);
        let mut all: Vec<Action> = menus.iter().flat_map(|(_, a)| a.iter().copied()).collect();
        let mut expected: Vec<Action> =
            keymap::table().iter().filter_map(|d| d.menu.map(|_| d.action)).collect();
        all.sort_by_key(|a| format!("{a:?}"));
        expected.sort_by_key(|a| format!("{a:?}"));
        assert_eq!(all, expected);
    }

    // ---- status_token (§5.29 urgency chain) ----

    #[test]
    fn status_token_matches_spec_colors() {
        assert_eq!(status_token(Status::Running), Token::Success);
        assert_eq!(status_token(Status::NeedsInput), Token::Accent);
        assert_eq!(status_token(Status::Idle), Token::StatusIdle);
        assert_eq!(status_token(Status::Hibernated), Token::StatusHibernated);
        assert_eq!(status_token(Status::Disconnected), Token::Danger);
    }

    // ---- op_label ----

    #[test]
    fn op_label_names_the_op_and_conflict_count() {
        assert_eq!(op_label(RepoOp::Merge, 0), "Merge in progress");
        assert_eq!(op_label(RepoOp::Rebase, 1), "Rebase in progress · 1 conflict");
        assert_eq!(op_label(RepoOp::CherryPick, 3), "Cherry-pick in progress · 3 conflicts");
    }

    // ---- notification_title (§3.1 W14) ----

    #[test]
    fn notification_title_joins_workspace_and_tab() {
        assert_eq!(notification_title(Some("conduit"), Some("fix-crash")), "conduit / fix-crash");
        assert_eq!(notification_title(Some("conduit"), None), "conduit");
        assert_eq!(notification_title(None, Some("fix-crash")), "fix-crash");
        assert_eq!(notification_title(None, None), "Amalgum");
    }

    // ---- level_glyph / level_token cover every Level ----

    #[test]
    fn every_level_has_a_distinct_glyph() {
        let levels = [Level::Info, Level::Success, Level::Error, Level::NeedsInput];
        let glyphs: std::collections::HashSet<&str> = levels.iter().map(|l| level_glyph(*l)).collect();
        assert_eq!(glyphs.len(), levels.len());
    }
}
