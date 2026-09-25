//! App state and the per-frame composition (§3, W2). Components draw themselves and return
//! actions; this module owns the state those actions change.
//!
//! - `app/workspace.rs` opening locations; spawning terminals and git panes; tabs and splits
//! - `app/events.rs`    control-socket requests and terminal events → agent status, notifications
//! - `app/actions.rs`   actions from the palette, menu bar, and shortcuts
//! - `app/surface.rs`   the center area: tab strip and split terminal panes

mod actions;
mod events;
mod ports;
mod surface;
mod workspace;

use super::chrome::{self, PanelAction, StatusAction, StatusInfo, Toast, Toasts};
use super::control::Control;
use super::git_pane::{GitEvent, GitPane, RepoSummary};
use super::palette::Palette;
use super::sidebar::{self, RowInfo};
use super::terminal::TerminalPane;
use super::theme::Colors;
use super::welcome;
use crate::agent::notifications::Store;
use crate::agent::status::{Status, Tracker, rollup};
use crate::git::Location;
use crate::model::keymap::{Keymap, Preset};
use crate::model::layout::PaneId;
use crate::model::settings::{Settings, ThemeMode};
use crate::model::state::AppState;
use crate::model::theme::Mode;
use crate::paths::Dirs;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};

/// Where keyboard focus is, which decides the keymap context (§5.27 "Input mapping").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Terminal,
    Git,
}

/// Runtime objects for one workspace. Persistent facts live in `AppState`.
struct Live {
    panes: HashMap<PaneId, TerminalPane>,
    /// Agent status per terminal, keyed by the pane's `AMALGUM_TAB` id (`<tab>.<pane>`).
    trackers: HashMap<String, Tracker>,
    git: Option<GitPane>,
    last_message: Option<(String, u64)>,
}

/// Results coming back from worker threads.
enum Msg {
    Opened(Result<workspace::Opened, String>),
    /// Listening ports per workspace (`app/ports.rs`).
    Ports(HashMap<String, Vec<u16>>),
}

pub struct App {
    dirs: Dirs,
    settings: Settings,
    state: AppState,
    keymap: Keymap,
    colors: Colors,
    /// `Mod+Alt+D` flips light/dark until relaunch (§7.1).
    theme_override: Option<Mode>,
    notifications: Store,
    control: Option<Control>,
    live: HashMap<String, Live>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    saver: Sender<AppState>,
    toasts: Toasts,
    palette: Option<Palette>,
    open_sheet: Option<String>,
    show_notifications: bool,
    show_sidebar: bool,
    /// `Mod+Shift+Enter`: the focused terminal fills its tab.
    zoomed: bool,
    /// `Mod+\` with the git pane focused: the git pane fills the window (§3).
    git_maximized: bool,
    focus: Focus,
    dirty: bool,
    /// `ctx.input(|i| i.time)` of the current frame, for toasts.
    time: f64,
    /// OS notifications are raised only when the window is not focused (§5.29).
    focused_window: bool,
    screenshot: super::screenshot::Screenshot,
    ports: HashMap<String, Vec<u16>>,
    last_port_scan: f64,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, dirs: Dirs, initial: Option<Location>) -> Self {
        let ctx = &cc.egui_ctx;
        let mut toasts = Toasts::default();
        let settings = Settings::load(&dirs.settings()).unwrap_or_else(|e| {
            toasts.push(Toast::error("settings.toml could not be read; using defaults", e), 0.0);
            Settings::default()
        });
        let state = AppState::load(&dirs.state()).unwrap_or_else(|e| {
            // Keep the unreadable file for the user; never overwrite it.
            let backup = dirs.state().with_extension(format!("json.unreadable-{}", crate::util::unix_now()));
            let _ = std::fs::rename(dirs.state(), &backup);
            toasts.push(
                Toast::error(
                    "Saved workspaces could not be restored",
                    format!("{e}\nkept as {}", backup.display()),
                ),
                0.0,
            );
            AppState::default()
        });
        let notifications = Store::load(&dirs.notifications()).unwrap_or_default();
        let control = Control::start(&dirs, ctx)
            .map_err(|e| {
                toasts.push(
                    Toast::warning(
                        "Control socket unavailable; `amalgum` commands won't reach this window",
                        e.to_string(),
                    ),
                    0.0,
                )
            })
            .ok();
        let (tx, rx) = channel();
        let mut app = Self {
            saver: spawn_saver(dirs.state()),
            dirs,
            keymap: Keymap::new(Preset::native()),
            colors: Colors::new(Mode::Dark),
            theme_override: None,
            notifications,
            control,
            live: HashMap::new(),
            tx,
            rx,
            toasts,
            palette: None,
            open_sheet: None,
            show_notifications: false,
            show_sidebar: true,
            zoomed: false,
            git_maximized: false,
            focus: Focus::Terminal,
            dirty: false,
            time: 0.0,
            focused_window: true,
            screenshot: super::screenshot::Screenshot::from_env(),
            ports: HashMap::new(),
            last_port_scan: f64::NEG_INFINITY,
            settings,
            state,
        };
        super::fonts::install(ctx);
        app.apply_theme(ctx);
        if app.settings.general.restore_workspaces {
            let ids: Vec<String> = app.state.workspaces().iter().map(|w| w.id.clone()).collect();
            for id in ids {
                app.ensure_live(ctx, &id);
            }
        }
        if let Some(loc) = initial {
            app.open(ctx, loc, None, None);
        }
        app
    }

    fn mode(&self, ctx: &egui::Context) -> Mode {
        self.theme_override.unwrap_or(match self.settings.appearance.theme {
            ThemeMode::Light => Mode::Light,
            ThemeMode::Dark => Mode::Dark,
            ThemeMode::System => match ctx.system_theme() {
                Some(egui::Theme::Light) => Mode::Light,
                _ => Mode::Dark,
            },
        })
    }

    /// Follow the OS theme live (§4): re-derive visuals only when the mode actually changes.
    fn apply_theme(&mut self, ctx: &egui::Context) {
        let mode = self.mode(ctx);
        if mode != self.colors.mode || self.time == 0.0 {
            self.colors = Colors::new(mode);
            ctx.set_visuals(self.colors.visuals());
        }
    }

    fn toast(&mut self, toast: Toast) {
        self.toasts.push(toast, self.time);
    }

    fn active_live(&mut self) -> Option<&mut Live> {
        let id = self.state.active.clone()?;
        self.live.get_mut(&id)
    }

    fn summary(&self, workspace: &str) -> RepoSummary {
        self.live.get(workspace).and_then(|l| l.git.as_ref()).map(GitPane::summary).unwrap_or_default()
    }

    fn workspace_status(&self, workspace: &str) -> (Status, bool) {
        let Some(live) = self.live.get(workspace) else { return (Status::None, false) };
        let status = rollup(live.trackers.values().map(Tracker::status));
        let low = live.trackers.values().any(|t| t.status() == status && t.low_confidence());
        (status, low)
    }

    fn row_infos(&self) -> HashMap<String, RowInfo> {
        self.state
            .workspaces()
            .into_iter()
            .map(|ws| {
                let s = self.summary(&ws.id);
                let (status, low_confidence) = self.workspace_status(&ws.id);
                let info = RowInfo {
                    status,
                    low_confidence,
                    branch: s.branch.or(s.detached),
                    ahead: s.ahead,
                    behind: s.behind,
                    dirty: s.changed,
                    ports: self.ports.get(&ws.id).cloned().unwrap_or_default(),
                    last_message: self.live.get(&ws.id).and_then(|l| l.last_message.clone()),
                    note: None,
                };
                (ws.id.clone(), info)
            })
            .collect()
    }

    fn status_info(&self) -> StatusInfo {
        let Some(ws) = self.state.active.as_deref() else { return StatusInfo::default() };
        let s = self.summary(ws);
        let (status, _) = self.workspace_status(ws);
        let agent = (status != Status::None).then(|| (status, format!("{status:?}").to_lowercase()));
        StatusInfo {
            branch: s.branch,
            detached: s.detached,
            ahead: s.ahead,
            behind: s.behind,
            changed: s.changed,
            op: s.op.map(|op| (op, s.conflicts)),
            agent,
            ports: self.ports.get(ws).cloned().unwrap_or_default(),
            unread: self.notifications.unread(),
            remote_state: None,
        }
    }

    /// What `amalgum list` returns (shape documented in `ctl::cli`).
    fn snapshot(&self) -> serde_json::Value {
        let workspaces: Vec<_> = self
            .state
            .workspaces()
            .into_iter()
            .map(|ws| {
                let live = self.live.get(&ws.id);
                let tabs: Vec<_> = ws
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.panes.iter().map(move |p| (tab, p)))
                    .map(|(tab, pane)| {
                        let key = workspace::pane_key(&tab.id, pane.id);
                        let status = live.and_then(|l| l.trackers.get(&key)).map_or(Status::None, Tracker::status);
                        let term = live.and_then(|l| l.panes.get(&pane.id));
                        let exited = term.and_then(|t| t.exit_code()).map(|code| code.unwrap_or(-1));
                        serde_json::json!({ "id": key, "title": pane.title, "status": status, "cwd": pane.cwd, "exited": exited })
                    })
                    .collect();
                serde_json::json!({
                    "id": ws.id, "name": ws.name, "ports": self.ports.get(&ws.id), "location": workspace::location_label(&ws.location, None),
                    "status": self.workspace_status(&ws.id).0, "tabs": tabs,
                })
            })
            .collect();
        serde_json::json!({ "workspaces": workspaces })
    }

    fn persist(&mut self) {
        if std::mem::take(&mut self.dirty) {
            let _ = self.saver.send(self.state.clone());
        }
    }

    fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Opened(Ok(opened)) => self.on_opened(ctx, opened),
                Msg::Ports(found) => self.on_ports(found),
                Msg::Opened(Err(e)) => {
                    self.toast(Toast::error(e.lines().next().unwrap_or("Could not open"), e.clone()))
                }
            }
        }
    }

    fn on_status_action(&mut self, action: StatusAction) {
        match action {
            StatusAction::OpenChanges => self.set_git_view(crate::model::state::GitView::Changes),
            StatusAction::ToggleNotifications => self.show_notifications = !self.show_notifications,
            StatusAction::OpenPort(port) => {
                let _ = crate::platform::open_in_default_app(&format!("http://localhost:{port}"));
            }
            StatusAction::ContinueOp | StatusAction::AbortOp | StatusAction::CreateBranch => {
                self.set_git_view(crate::model::state::GitView::Changes);
            }
        }
    }

    fn set_git_view(&mut self, view: crate::model::state::GitView) {
        if let Some(id) = self.state.active.clone() {
            if let Some(ws) = self.state.workspace_mut(&id) {
                ws.git_view = view;
                ws.git_pane_open = true;
            }
            if let Some(git) = self.live.get_mut(&id).and_then(|l| l.git.as_mut()) {
                git.set_view(view);
            }
            self.focus = Focus::Git;
            self.dirty = true;
        }
    }

    fn on_git_events(&mut self, events: Vec<GitEvent>) {
        for ev in events {
            match ev {
                GitEvent::Toast(t) => self.toast(t),
                GitEvent::SendToTerminal(text) => self.write_to_focused(text.into_bytes()),
                GitEvent::SummaryChanged => {}
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.time = ctx.input(|i| i.time).max(f64::MIN_POSITIVE);
        self.focused_window = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        let now = crate::util::unix_now();
        self.apply_theme(&ctx);
        self.drain_messages(&ctx);
        self.drain_control(&ctx, now);
        self.poll_terminals(&ctx, now);
        self.handle_shortcuts(&ctx);
        self.handle_dropped_files(&ctx);
        self.scan_ports(&ctx);

        let menu = egui::Panel::top("menu").show(ui, |ui| chrome::menu_bar(ui, &self.keymap)).inner;
        if let Some(action) = menu {
            self.dispatch(&ctx, action);
        }
        let info = self.status_info();
        let status =
            egui::Panel::bottom("status").show(ui, |ui| chrome::status_bar(ui, &info, &self.colors)).inner;
        if let Some(action) = status {
            self.on_status_action(action);
        }
        if self.show_sidebar {
            let rows = self.row_infos();
            let action = egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(240.0)
                .show(ui, |ui| sidebar::show(ui, &self.state, &rows, &self.colors, now))
                .inner;
            if let Some(action) = action {
                self.on_sidebar(&ctx, action);
            }
        }
        let git_open = self
            .state
            .active
            .as_deref()
            .and_then(|id| self.state.workspace(id))
            .is_some_and(|w| w.git_pane_open);
        let maximized = git_open && self.git_maximized;
        if git_open && !maximized {
            egui::Panel::right("git")
                .resizable(true)
                .default_size(420.0)
                .show(ui, |ui| self.git_pane(ui, now));
        }
        let base = egui::Frame::new().fill(self.colors.get(crate::model::theme::Token::BgBase));
        egui::CentralPanel::no_frame().frame(base).show(ui, |ui| {
            if maximized {
                self.git_pane(ui, now);
            } else if self.state.workspaces().is_empty() {
                let banner = false;
                if let Some(action) =
                    welcome::show(ui, &self.state.recents, banner, &self.keymap, &self.colors, now)
                {
                    self.on_welcome(&ctx, action);
                }
            } else {
                self.surface(ui);
            }
        });

        self.overlays(&ctx, now);
        if let Some(control) = &self.control {
            control.publish(self.snapshot());
        }
        self.persist();
        self.screenshot.tick(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.state.save(&self.dirs.state());
        let _ = self.notifications.save(&self.dirs.notifications());
    }
}

impl App {
    /// The active workspace's git pane; a click inside moves keyboard focus to it.
    fn git_pane(&mut self, ui: &mut egui::Ui, now: u64) {
        if ui.ui_contains_pointer() && ui.input(|i| i.pointer.any_pressed()) {
            self.focus = Focus::Git;
        }
        let (colors, settings) = (self.colors, self.settings.clone());
        let events =
            self.active_live().and_then(|l| l.git.as_mut()).map(|g| g.show(ui, &colors, &settings, now));
        self.on_git_events(events.unwrap_or_default());
    }

    fn overlays(&mut self, ctx: &egui::Context, now: u64) {
        if let Some(mut palette) = self.palette.take() {
            let items = self.palette_items();
            match palette.show(ctx, &items, &self.colors) {
                super::palette::Outcome::Open => self.palette = Some(palette),
                super::palette::Outcome::Cancelled => {}
                super::palette::Outcome::Chosen(target) => self.on_palette(ctx, target),
            }
        }
        if self.show_notifications {
            match chrome::notification_panel(ctx, &mut self.notifications, &self.colors, now) {
                Some(PanelAction::Close) => self.show_notifications = false,
                Some(PanelAction::Focus { workspace, tab: _ }) => {
                    if let Some(ws) = workspace.filter(|w| self.state.workspace(w).is_some()) {
                        self.state.active = Some(ws);
                        self.dirty = true;
                    }
                    self.show_notifications = false;
                }
                None => {}
            }
        }
        if let Some(path) = self.open_sheet.take() {
            self.open_sheet = self.show_open_sheet(ctx, path);
        }
        self.toasts.show(ctx, &self.colors, self.time);
    }

    /// The minimal "Open Folder" sheet: a path field (a native picker is a later step).
    fn show_open_sheet(&mut self, ctx: &egui::Context, mut path: String) -> Option<String> {
        let mut result = Some(());
        let mut submit = false;
        egui::Window::new("Open folder")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Folder or host:path");
                let field = ui.add(egui::TextEdit::singleline(&mut path).desired_width(360.0));
                field.request_focus();
                submit = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        result = None;
                    }
                    submit |= ui.button("Open").clicked();
                });
            });
        if submit && !path.trim().is_empty() {
            let loc = workspace::parse_user_location(path.trim());
            self.open(ctx, loc, None, None);
            return None;
        }
        result.map(|()| path)
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<_> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        for path in dropped.into_iter().filter(|p| p.is_dir()) {
            self.open(ctx, Location::Local { path }, None, None);
        }
    }
}

/// One writer thread for `state.json`, always saving the newest state it has been sent.
fn spawn_saver(path: std::path::PathBuf) -> Sender<AppState> {
    let (tx, rx) = channel::<AppState>();
    std::thread::spawn(move || {
        while let Ok(mut state) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                state = newer;
            }
            let _ = state.save(&path);
        }
    });
    tx
}
