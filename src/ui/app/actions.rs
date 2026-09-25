//! Actions from every entry point — shortcuts, menu bar, palette, sidebar, welcome screen —
//! funnel through here (§2 "every action is reachable three ways").

use super::{App, Focus};
use crate::git::Location;
use crate::model::fuzzy::Group;
use crate::model::keymap::{self, Action, Context};
use crate::model::layout::Axis;
use crate::model::state::GitView;
use crate::ui::chrome::Toast;
use crate::ui::palette::{Item, Palette, Target};
use crate::ui::shortcuts;
use crate::ui::sidebar::SidebarAction;
use crate::ui::welcome::WelcomeAction;

impl App {
    /// Intercept bound chords before any widget sees them. In a terminal, or while a text field
    /// has focus, only chords containing `Mod` are intercepted (§5.27 "Input mapping").
    pub(super) fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.palette.is_some() || self.open_sheet.is_some() {
            return;
        }
        let context = match self.focus {
            Focus::Terminal => Context::Terminal,
            Focus::Git => Context::GitPane,
        };
        let typing = ctx.egui_wants_keyboard_input();
        let (keymap, mut hits) = (&self.keymap, Vec::new());
        ctx.input_mut(|i| {
            i.events.retain(|e| {
                let egui::Event::Key { key, pressed: true, modifiers, .. } = e else { return true };
                let Some(chord) = shortcuts::chord(*key, *modifiers, keymap.preset) else { return true };
                if typing && !chord.mods.primary {
                    return true;
                }
                match keymap.lookup(context, &chord) {
                    Some(action) => {
                        hits.push(action);
                        false
                    }
                    None => true,
                }
            });
        });
        for action in hits {
            self.dispatch(ctx, action);
        }
    }

    pub(super) fn dispatch(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::CommandPalette => self.palette = Some(Palette::default()),
            Action::OpenFolder | Action::NewWorkspace => self.open_sheet = Some(default_open_path()),
            Action::CloseWorkspace => {
                if let Some(id) = self.state.active.clone() {
                    self.close_workspace(&id);
                }
            }
            other => self.dispatch_more(ctx, other),
        }
    }

    fn dispatch_more(&mut self, ctx: &egui::Context, action: Action) {
        let _ = (ctx, Axis::Horizontal, GitView::Graph);
        self.toast(Toast::info(format!("{} is not available in this build yet", keymap::def(action).label)));
    }

    pub(super) fn palette_items(&self) -> Vec<Item> {
        let preset = self.keymap.preset;
        let actions = keymap::table().iter().map(|d| Item {
            group: Group::Actions,
            label: d.label.to_string(),
            detail: self.keymap.bindings(d.action).first().map(|c| preset.label(c)).unwrap_or_default(),
            target: Target::Action(d.action),
        });
        let workspaces = self.state.workspaces().into_iter().map(|w| Item {
            group: Group::Workspaces,
            label: format!("{} {}", self.workspace_status(&w.id).0.glyph(), w.name).trim().to_string(),
            detail: w.location.display(crate::paths::home().as_deref()),
            target: Target::Workspace(w.id.clone()),
        });
        let recents = self.state.recents.iter().map(|r| Item {
            group: Group::Locations,
            label: r.label.clone(),
            detail: r.location.display(crate::paths::home().as_deref()),
            target: Target::Location(r.location.clone()),
        });
        actions.chain(workspaces).chain(recents).collect()
    }

    pub(super) fn on_palette(&mut self, ctx: &egui::Context, target: Target) {
        match target {
            Target::Action(a) => self.dispatch(ctx, a),
            Target::Workspace(id) => self.activate(ctx, &id),
            Target::Location(loc) => self.open(ctx, loc, None, None),
            Target::Branch(name) => {
                self.toast(Toast::info(format!("Checkout from the palette is not wired yet ({name})")))
            }
        }
    }

    pub(super) fn activate(&mut self, ctx: &egui::Context, id: &str) {
        if self.state.workspace(id).is_some() {
            self.ensure_live(ctx, id);
            self.state.active = Some(id.to_string());
            self.focus = Focus::Terminal;
            self.dirty = true;
        }
    }

    pub(super) fn on_sidebar(&mut self, ctx: &egui::Context, action: SidebarAction) {
        let path_of = |app: &Self, id: &str| match app.state.workspace(id).map(|w| &w.location) {
            Some(Location::Local { path }) => Some(path.clone()),
            _ => None,
        };
        match action {
            SidebarAction::Select(id) => self.activate(ctx, &id),
            SidebarAction::NewWorkspace => self.open_sheet = Some(default_open_path()),
            SidebarAction::Close(id) => self.close_workspace(&id),
            SidebarAction::Rename(id, name) => {
                if let Some(w) = self.state.workspace_mut(&id) {
                    w.name = name;
                    self.dirty = true;
                }
            }
            SidebarAction::MoveUp(id) => self.dirty |= self.state.move_workspace(&id, -1),
            SidebarAction::MoveDown(id) => self.dirty |= self.state.move_workspace(&id, 1),
            SidebarAction::OpenPort(port) => {
                let _ = crate::platform::open_in_default_app(&format!("http://localhost:{port}"));
            }
            SidebarAction::RevealInFileManager(id) => {
                if let Some(p) = path_of(self, &id) {
                    let _ = crate::platform::reveal_in_file_manager(&p);
                }
            }
            SidebarAction::OpenInExternalTerminal(id) => {
                if let Some(p) = path_of(self, &id) {
                    let _ = crate::platform::open_terminal_at(&p);
                }
            }
            SidebarAction::CopyPath(id) => {
                if let Some(w) = self.state.workspace(&id) {
                    ctx.copy_text(w.location.display(None));
                }
            }
            SidebarAction::ToggleRepo(repo) => {
                if let Some(r) = self.state.repos.iter_mut().find(|r| r.id == repo) {
                    r.collapsed = !r.collapsed;
                    self.dirty = true;
                }
            }
            SidebarAction::ForgetRepo(repo) => {
                let doomed: Vec<String> = self
                    .state
                    .repos
                    .iter()
                    .filter(|r| r.id == repo)
                    .flat_map(|r| r.remotes.iter().flat_map(|g| g.workspaces.iter().map(|w| w.id.clone())))
                    .collect();
                for id in doomed {
                    self.live.remove(&id);
                }
                self.state.forget_repo(&repo);
                self.dirty = true;
            }
        }
    }

    pub(super) fn on_welcome(&mut self, ctx: &egui::Context, action: WelcomeAction) {
        match action {
            WelcomeAction::OpenFolder => self.open_sheet = Some(default_open_path()),
            WelcomeAction::OpenRecent(loc) => self.open(ctx, loc, None, None),
            WelcomeAction::RemoveRecent(loc) => {
                self.state.recents.retain(|r| r.location != loc);
                self.dirty = true;
            }
            WelcomeAction::ClearRecents => {
                self.state.recents.clear();
                self.dirty = true;
            }
            WelcomeAction::CloneRepository | WelcomeAction::ConnectToHost => {
                self.toast(Toast::info(
                    "Clone and SSH workspaces are the next milestone; open a local folder for now",
                ));
            }
            WelcomeAction::SetUpHooks => {
                self.toast(Toast::info(
                    "Run `amalgum hooks setup` in a terminal; it lists what it writes before writing",
                ));
            }
            WelcomeAction::SkipSetUp => {}
        }
    }
}

fn default_open_path() -> String {
    crate::paths::home().map(|h| format!("{}/", h.display())).unwrap_or_default()
}
