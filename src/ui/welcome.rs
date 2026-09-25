//! Welcome screen (§5.1, W1): Open Folder / Clone Repository / Connect to Host cards with their
//! shortcuts, Recent locations (Enter/double-click opens, Backspace removes, missing local paths
//! greyed with "Not found" + Remove), the drop hint, and the first-launch setup banner.

use super::theme::Colors;
use crate::git::Location;
use crate::model::keymap::{Action, Keymap};
use crate::model::state::Recent;
use crate::model::theme::Token;
use crate::util::relative_time;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WelcomeAction {
    OpenFolder,
    CloneRepository,
    ConnectToHost,
    OpenRecent(Location),
    RemoveRecent(Location),
    ClearRecents,
    SetUpHooks,
    SkipSetUp,
}

/// Ephemeral selection within the recents list, so `Enter`/`Backspace` have something to act
/// on after a click. Not persisted: this screen only exists while there are no workspaces.
#[derive(Clone, Default)]
struct UiState {
    selected: Option<usize>,
}

/// A local path that no longer exists shows greyed with "Not found" (§5.1.3); remote entries
/// are never checked (there is no cheap local test for host reachability).
fn location_missing(loc: &Location) -> bool {
    match loc {
        Location::Local { path } => !path.exists(),
        Location::Remote { .. } => false,
    }
}

fn shortcut_label(keymap: &Keymap, action: Action) -> String {
    keymap.bindings(action).first().map(|c| keymap.preset.label(c)).unwrap_or_default()
}

pub fn show(
    ui: &mut egui::Ui,
    recents: &[Recent],
    show_setup_banner: bool,
    keymap: &Keymap,
    colors: &Colors,
    now: u64,
) -> Option<WelcomeAction> {
    let mem_id = ui.id().with("welcome-ui-state");
    let mut mem: UiState = ui.data_mut(|d| d.get_temp(mem_id)).unwrap_or_default();
    let mut action = None;

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(48.0);
            ui.label(egui::RichText::new("Amalgum").size(20.0).strong());
            ui.add_space(24.0);

            ui.horizontal(|ui| {
                let card_width = 168.0;
                let total = card_width * 3.0 + 16.0 * 2.0;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                if card(ui, colors, keymap, Action::OpenFolder, "Open Folder", card_width) {
                    action = Some(WelcomeAction::OpenFolder);
                }
                ui.add_space(16.0);
                if card(ui, colors, keymap, Action::CloneRepository, "Clone Repository", card_width) {
                    action = Some(WelcomeAction::CloneRepository);
                }
                ui.add_space(16.0);
                if card(ui, colors, keymap, Action::ConnectToHost, "Connect to Host", card_width) {
                    action = Some(WelcomeAction::ConnectToHost);
                }
            });

            ui.add_space(28.0);

            ui.horizontal(|ui| {
                ui.set_width(640.0);
                ui.label(egui::RichText::new("Recent locations").size(15.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !recents.is_empty() && ui.link("Clear").clicked() {
                        action = Some(WelcomeAction::ClearRecents);
                    }
                });
            });
            ui.add_space(4.0);

            if recents.is_empty() {
                ui.colored_label(colors.get(Token::FgSecondary), "No recent locations yet.");
            } else {
                ui.vertical(|ui| {
                    ui.set_width(640.0);
                    for (i, recent) in recents.iter().enumerate() {
                        if let Some(a) = recent_row(ui, i, recent, colors, now, &mut mem) {
                            action = Some(a);
                        }
                    }
                });
            }

            ui.add_space(20.0);
            ui.colored_label(
                colors.get(Token::FgSecondary),
                "Drop a folder anywhere in this window to open it as a workspace.",
            );

            if show_setup_banner {
                ui.add_space(16.0);
                egui::Frame::default()
                    .fill(colors.get(Token::BgRaised))
                    .inner_margin(10)
                    .corner_radius(6)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                colors.get(Token::FgSecondary),
                                "ⓘ Install the `amalgum` command line tool and agent hooks",
                            );
                            if ui.button("Set up").clicked() {
                                action = Some(WelcomeAction::SetUpHooks);
                            }
                            if ui.button("Skip").clicked() {
                                action = Some(WelcomeAction::SkipSetUp);
                            }
                        });
                    });
            }
        });
    });

    ui.data_mut(|d| d.insert_temp(mem_id, mem));
    action
}

fn card(
    ui: &mut egui::Ui,
    colors: &Colors,
    keymap: &Keymap,
    action: Action,
    title: &str,
    width: f32,
) -> bool {
    let shortcut = shortcut_label(keymap, action);
    let inner = egui::Frame::default()
        .fill(colors.get(Token::BgRaised))
        .stroke(egui::Stroke::new(1.0, colors.get(Token::Border)))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 16))
        .show(ui, |ui| {
            ui.set_width(width);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(title).size(13.0));
                ui.add_space(6.0);
                ui.colored_label(colors.get(Token::FgSecondary), shortcut);
            });
        });
    let resp = ui.interact(inner.response.rect, ui.id().with(("welcome-card", title)), egui::Sense::click());
    resp.clicked()
}

fn recent_row(
    ui: &mut egui::Ui,
    idx: usize,
    recent: &Recent,
    colors: &Colors,
    now: u64,
    mem: &mut UiState,
) -> Option<WelcomeAction> {
    let missing = location_missing(&recent.location);
    let selected = mem.selected == Some(idx);
    let mut action = None;

    let bg = if selected { colors.get(Token::BgSelected) } else { egui::Color32::TRANSPARENT };
    let name_color = if missing { colors.get(Token::FgDisabled) } else { colors.get(Token::FgPrimary) };
    let inner = egui::Frame::default().fill(bg).inner_margin(6).corner_radius(4).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.colored_label(name_color, &recent.label);
            ui.colored_label(
                colors.get(Token::FgSecondary),
                recent.location.display(crate::paths::home().as_deref()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if missing {
                    if ui.button("Remove").clicked() {
                        action = Some(WelcomeAction::RemoveRecent(recent.location.clone()));
                    }
                    ui.colored_label(colors.get(Token::Warning), "Not found");
                } else {
                    ui.colored_label(colors.get(Token::FgSecondary), relative_time(now, recent.opened));
                }
            });
        });
    });

    let row_id = ui.id().with(("welcome-recent", idx));
    let resp = ui.interact(inner.response.rect, row_id, egui::Sense::click());
    if action.is_none() {
        if resp.double_clicked() {
            action = Some(WelcomeAction::OpenRecent(recent.location.clone()));
        } else if resp.clicked() {
            mem.selected = Some(idx);
        }
    }
    if action.is_none() && mem.selected == Some(idx) {
        if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            action = Some(WelcomeAction::OpenRecent(recent.location.clone()));
        } else if ui.input(|i| i.key_pressed(egui::Key::Backspace)) {
            action = Some(WelcomeAction::RemoveRecent(recent.location.clone()));
        }
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_flags_a_nonexistent_local_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!location_missing(&Location::Local { path: dir.path().to_path_buf() }));
        assert!(location_missing(&Location::Local { path: dir.path().join("does-not-exist") }));
    }

    #[test]
    fn remote_locations_are_never_flagged_missing() {
        assert!(!location_missing(&Location::Remote { host: "gpu-box".into(), path: "~/amalgum".into() }));
    }
}
