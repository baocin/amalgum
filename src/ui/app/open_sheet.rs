//! The "Open folder / Connect to host" sheet: the Existing folder and Remote host parts of W15
//! (§5.22). Remote host: a host from the ssh config's `Host` aliases (read on a worker when the
//! sheet opens) or typed, a path on it, and **Keep sessions alive with tmux on the host**.

use super::{App, Msg, workspace};
use crate::git::Location;
use crate::ui::jobs;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct OpenSheet {
    pub remote: bool,
    /// Existing folder: a local path, or `host:path`.
    pub folder: String,
    pub host: String,
    pub path: String,
    pub keep_sessions: bool,
    /// `Host` aliases from the ssh config.
    pub hosts: Vec<String>,
}

impl OpenSheet {
    pub fn folder(folder: String) -> Self {
        Self { folder, path: "~".into(), keep_sessions: true, ..Self::default() }
    }

    pub fn host(keep_sessions: bool) -> Self {
        Self { remote: true, keep_sessions, ..Self::folder(String::new()) }
    }

    /// What **Open** opens, or why it can't yet.
    pub fn target(&self) -> Result<Location, String> {
        if !self.remote {
            let folder = self.folder.trim();
            return match folder.is_empty() {
                true => Err("Type a folder".into()),
                false => Ok(workspace::parse_user_location(folder)),
            };
        }
        let (host, path) = (self.host.trim(), self.path.trim());
        if !crate::ssh::valid_host(host) {
            return Err("Choose a host".into());
        }
        if path.is_empty() {
            return Err("Type a path on the host".into());
        }
        Ok(Location::Remote { host: host.into(), path: path.into() })
    }
}

impl App {
    /// Open the sheet; the host list arrives from a worker (the ssh config is a file).
    pub(super) fn open_sheet(&mut self, ctx: &egui::Context, sheet: OpenSheet) {
        let config = self.settings.ssh.config_path(crate::paths::home().as_deref());
        jobs::spawn(ctx, &self.tx, move || {
            let text = config.and_then(|c| std::fs::read_to_string(c).ok()).unwrap_or_default();
            Msg::SshHosts(crate::ssh::config_hosts(&text))
        });
        self.open_sheet = Some(sheet);
    }

    pub(super) fn show_open_sheet(&mut self, ctx: &egui::Context, mut sheet: OpenSheet) -> Option<OpenSheet> {
        let (mut cancel, mut submit) = (false, false);
        let title = if sheet.remote { "Connect to host" } else { "Open folder" };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Location");
                    ui.radio_value(&mut sheet.remote, false, "Existing folder");
                    ui.radio_value(&mut sheet.remote, true, "Remote host");
                });
                ui.add_space(4.0);
                let enter = |ui: &egui::Ui, r: &egui::Response| {
                    r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                };
                if sheet.remote {
                    egui::Grid::new("open_sheet_remote").num_columns(2).show(ui, |ui| {
                        ui.label("Host");
                        ui.horizontal(|ui| {
                            let field = ui.add(
                                egui::TextEdit::singleline(&mut sheet.host)
                                    .desired_width(240.0)
                                    .hint_text("gpu-box"),
                            );
                            if sheet.host.is_empty()
                                && !field.has_focus()
                                && ui.memory(|m| m.focused().is_none())
                            {
                                field.request_focus();
                            }
                            submit |= enter(ui, &field);
                            ui.add_enabled_ui(!sheet.hosts.is_empty(), |ui| {
                                ui.menu_button("▾", |ui| {
                                    for h in &sheet.hosts {
                                        if ui.button(h).clicked() {
                                            sheet.host = h.clone();
                                            ui.close();
                                        }
                                    }
                                })
                                .response
                                .on_hover_text("Hosts from ssh config");
                            });
                        });
                        ui.end_row();
                        ui.label("Path");
                        let field = ui.add(egui::TextEdit::singleline(&mut sheet.path).desired_width(240.0));
                        submit |= enter(ui, &field);
                        ui.end_row();
                    });
                    ui.checkbox(&mut sheet.keep_sessions, "Keep sessions alive with tmux on the host");
                } else {
                    ui.label("Folder or host:path");
                    let field = ui.add(egui::TextEdit::singleline(&mut sheet.folder).desired_width(360.0));
                    field.request_focus();
                    submit |= enter(ui, &field);
                }
                let target = sheet.target();
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        cancel = true;
                    }
                    let open = ui.add_enabled(target.is_ok(), egui::Button::new("Open"));
                    submit |= open.clicked();
                    if let Err(why) = &target {
                        open.on_disabled_hover_text(why);
                    }
                });
            });
        if cancel {
            return None;
        }
        match sheet.target() {
            Ok(Location::Remote { host, path }) if submit && sheet.remote => {
                let keep = sheet.keep_sessions;
                self.open_remote(ctx, Location::Remote { host, path }, None, None, keep);
                None
            }
            Ok(location) if submit => {
                self.open(ctx, location, None, None);
                None
            }
            _ => Some(sheet),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_target_needs_a_usable_host_and_a_path() {
        let mut sheet = OpenSheet::host(true);
        assert_eq!(sheet.path, "~");
        assert!(sheet.target().is_err());
        sheet.host = "-oProxyCommand=x".into();
        assert!(sheet.target().is_err(), "never an ssh option");
        sheet.host = " gpu-box ".into();
        assert_eq!(sheet.target(), Ok(Location::Remote { host: "gpu-box".into(), path: "~".into() }));
        sheet.path = " ".into();
        assert!(sheet.target().is_err());
    }

    #[test]
    fn the_folder_field_takes_host_colon_path_too() {
        let mut sheet = OpenSheet::folder(String::new());
        assert!(sheet.target().is_err());
        sheet.folder = "gpu-box:~/amalgum".into();
        assert_eq!(sheet.target(), Ok(Location::Remote { host: "gpu-box".into(), path: "~/amalgum".into() }));
    }
}
