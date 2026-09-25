//! Actions from every entry point — shortcuts, menu bar, palette, sidebar, welcome screen —
//! funnel through here (§2 "every action is reachable three ways").

use super::{App, Focus};
use crate::git::Location;
use crate::model::fuzzy::Group;
use crate::model::keymap::{self, Action, Context};
use crate::model::layout::{Axis, Dir, Rect};
use crate::model::state::GitView;
use crate::model::theme::Mode;
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
        use Action as A;
        let nth = |a: Action| match a {
            A::Workspace1 => Some(0),
            A::Workspace2 => Some(1),
            A::Workspace3 => Some(2),
            A::Workspace4 => Some(3),
            A::Workspace5 => Some(4),
            A::Workspace6 => Some(5),
            A::Workspace7 => Some(6),
            A::Workspace8 => Some(7),
            A::Workspace9 => Some(8),
            _ => None,
        };
        if let Some(n) = nth(action) {
            if let Some(id) = self.state.workspaces().get(n).map(|w| w.id.clone()) {
                self.activate(ctx, &id);
            }
            return;
        }
        match action {
            A::PreviousWorkspace => self.cycle_workspace(ctx, -1),
            A::NextWorkspace => self.cycle_workspace(ctx, 1),
            A::NewTerminalTab => self.new_tab(ctx),
            A::CloseTabOrPane | A::ClosePane => self.close_pane(),
            A::PreviousTerminalTab => self.cycle_tab(-1),
            A::NextTerminalTab => self.cycle_tab(1),
            A::SplitRight => self.split(ctx, Axis::Horizontal),
            A::SplitDown => self.split(ctx, Axis::Vertical),
            A::FocusPaneLeft => self.move_focus(Dir::Left),
            A::FocusPaneRight => self.move_focus(Dir::Right),
            A::FocusPaneUp => self.move_focus(Dir::Up),
            A::FocusPaneDown => self.move_focus(Dir::Down),
            A::ZoomPane => self.zoomed = !self.zoomed,
            A::MaximizePane => match self.focus {
                Focus::Git => self.git_maximized = !self.git_maximized,
                Focus::Terminal => self.zoomed = !self.zoomed,
            },
            A::ToggleGitPane => {
                if let Some(ws) = self.state.active.clone().and_then(|id| self.state.workspace_mut(&id)) {
                    ws.git_pane_open = !ws.git_pane_open;
                    self.dirty = true;
                }
            }
            A::ToggleSidebar => self.show_sidebar = !self.show_sidebar,
            A::NotificationPanel => self.show_notifications = !self.show_notifications,
            A::FocusGraph => self.set_git_view(GitView::Graph),
            A::GitPaneChangesView => self.set_git_view(GitView::Changes),
            A::GitPaneRefsView => self.set_git_view(GitView::Refs),
            A::FocusTerminalFromGitPane => self.focus = Focus::Terminal,
            A::Undo | A::Redo => {
                if let Some(git) = self.active_live().and_then(|l| l.git.as_mut()) {
                    if action == A::Undo { git.undo() } else { git.redo() }
                }
            }
            A::ToggleTheme => {
                let flipped = match self.colors.mode {
                    Mode::Dark => Mode::Light,
                    Mode::Light => Mode::Dark,
                };
                self.theme_override = Some(flipped);
            }
            A::ZoomIn | A::ZoomOut | A::ZoomReset => {
                let size = &mut self.settings.terminal.font_size;
                *size = match action {
                    A::ZoomIn => (*size + 1.0).min(32.0),
                    A::ZoomOut => (*size - 1.0).max(8.0),
                    _ => 12.0,
                };
            }
            A::TerminalCopy | A::Copy => {
                if let Some(text) = self.focused_terminal().and_then(|t| t.selection_text()) {
                    ctx.copy_text(text);
                }
            }
            A::TerminalPaste | A::Paste => {
                let text = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
                if let Some(t) = self.focused_terminal() {
                    t.paste(&text);
                }
            }
            A::Settings => self.open_settings_file(),
            A::OpenLogFolder => {
                let _ = std::fs::create_dir_all(self.dirs.logs());
                let _ = crate::platform::open_in_default_app(&self.dirs.logs().display().to_string());
            }
            A::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            A::MinimizeWindow => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
            A::ZoomWindow => {
                let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
            A::ShortcutOverlay => self.palette = Some(Palette::default()),
            other => {
                self.toast(Toast::info(format!(
                    "{} is not available in this build yet",
                    keymap::def(other).label
                )));
            }
        }
    }

    fn cycle_workspace(&mut self, ctx: &egui::Context, delta: isize) {
        let ids: Vec<String> = self.state.workspaces().iter().map(|w| w.id.clone()).collect();
        let Some(pos) = self.state.active.as_ref().and_then(|a| ids.iter().position(|i| i == a)) else {
            return;
        };
        let next = (pos as isize + delta).rem_euclid(ids.len() as isize) as usize;
        self.activate(ctx, &ids[next]);
    }

    fn cycle_tab(&mut self, delta: isize) {
        let Some(ws) = self.state.active.clone().and_then(|id| self.state.workspace_mut(&id)) else { return };
        if ws.tabs.is_empty() {
            return;
        }
        ws.active_tab = (ws.active_tab as isize + delta).rem_euclid(ws.tabs.len() as isize) as usize;
        self.dirty = true;
    }

    /// `Mod+Alt+arrows`: geometric neighbour in the active tab's split tree (§5.27).
    fn move_focus(&mut self, dir: Dir) {
        let Some(id) = self.state.active.clone() else { return };
        let Some(ws) = self.state.workspace(&id) else { return };
        let Some(tab) = ws.tabs.get(ws.active_tab) else { return };
        // Layout is proportional, so any area gives the same neighbours.
        let area = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 1000.0 };
        if let Some(pane) = tab.layout.neighbor(area, tab.focused, dir) {
            self.focus_pane(&id, pane);
        }
    }

    fn focused_terminal(&self) -> Option<&crate::ui::terminal::TerminalPane> {
        let ws = self.state.workspace(self.state.active.as_deref()?)?;
        let tab = ws.tabs.get(ws.active_tab)?;
        self.live.get(&ws.id)?.panes.get(&tab.focused)
    }

    /// Settings has no window yet: open `settings.toml` (written with defaults if missing).
    fn open_settings_file(&mut self) {
        let path = self.dirs.settings();
        if !path.exists()
            && let Err(e) = self.settings.save(&path)
        {
            self.toast(Toast::error("Could not write settings.toml", e.to_string()));
            return;
        }
        let _ = crate::platform::open_in_default_app(&path.display().to_string());
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
