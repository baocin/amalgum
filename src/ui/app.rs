//! App state and the per-frame composition (§3, W2). Components draw themselves and return
//! actions; this module owns the state those actions change.
//!
//! - `app/workspace.rs` opening locations; spawning terminals and git panes; tabs and splits
//! - `app/events.rs`    control-socket requests and terminal events → agent status, notifications
//! - `app/actions.rs`   actions from the palette, menu bar, and shortcuts
//! - `app/surface.rs`   the center area: tab strip and split terminal panes
//! - `app/remote.rs`    remote workspaces: the ssh manager, W16, W18 forwards (§5.28)
//! - `app/open_sheet.rs` Open folder / Connect to host (W15's location part)

mod actions;
mod events;
mod open_sheet;
mod ports;
mod remote;
mod surface;
mod sync;
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
#[derive(Default)]
struct Live {
    panes: HashMap<PaneId, TerminalPane>,
    /// Agent status per terminal, keyed by the pane's `AMALGUM_TAB` id (`<tab>.<pane>`).
    trackers: HashMap<String, Tracker>,
    git: Option<GitPane>,
    last_message: Option<(String, u64)>,
    /// Remote workspaces: the host and the connection the terminals run on.
    remote: Option<remote::RemoteLive>,
}

/// Results coming back from worker threads.
enum Msg {
    Opened(Result<workspace::Opened, String>),
    /// Listening ports per workspace (`app/ports.rs`).
    Ports(HashMap<String, Vec<u16>>),
    SettingsSaved(Result<(), String>),
    /// `Host` aliases for the open sheet (§5.22 W15).
    SshHosts(Vec<String>),
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
    /// §5.3 clone sheet and its running clone.
    clone: super::clone::CloneSheet,
    open_sheet: Option<open_sheet::OpenSheet>,
    /// SSH workspaces (§5.28).
    remote: remote::Remote,
    show_notifications: bool,
    show_sidebar: bool,
    /// `Mod+Shift+Enter`: the focused terminal fills its tab.
    zoomed: bool,
    /// `Mod+\` with the git pane focused: the git pane fills the window (§3).
    git_maximized: bool,
    /// `git_maximized` before a view maximized the pane (§5.15 blame), restored when it closes,
    /// with the workspace it was saved in: switching workspace (or `Mod+\\`) drops it.
    git_maximized_before: Option<(Option<String>, bool)>,
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
        let remote = remote::Remote::new(ctx, &dirs, &settings);
        let mut app = Self {
            remote,
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
            clone: Default::default(),
            open_sheet: None,
            show_notifications: false,
            show_sidebar: true,
            zoomed: false,
            git_maximized: false,
            git_maximized_before: None,
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
                let mut info = RowInfo {
                    status,
                    low_confidence,
                    branch: s.branch.or(s.detached),
                    ahead: s.ahead,
                    behind: s.behind,
                    dirty: s.changed,
                    ports: self.workspace_ports(&ws.id),
                    last_message: self.live.get(&ws.id).and_then(|l| l.last_message.clone()),
                    note: None,
                    remote: None,
                };
                self.remote_row(&ws.id, &mut info);
                (ws.id.clone(), info)
            })
            .collect()
    }

    fn status_info(&self) -> StatusInfo {
        let Some(ws) = self.state.active.as_deref() else { return StatusInfo::default() };
        let s = self.summary(ws);
        let (status, _) = self.workspace_status(ws);
        let agent = (status != Status::None).then(|| (status, status.label().to_string()));
        StatusInfo {
            branch: s.branch,
            detached: s.detached,
            ahead: s.ahead,
            behind: s.behind,
            changed: s.changed,
            op: s.op.map(|op| (op, s.conflicts)),
            rebase: s
                .rebase_paused
                .as_deref()
                .map(|at| (super::git_pane::paused_label(at, s.conflicts), s.conflicts == 0)),
            agent,
            ports: self.workspace_ports(ws),
            unread: self.notifications.unread(),
            remote_state: self.remote_state(ws),
            sync: self.sync_status(ws),
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
                    "id": ws.id, "name": ws.name, "ports": self.workspace_ports(&ws.id), "location": workspace::location_label(&ws.location, None),
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
                Msg::SettingsSaved(Ok(())) => {}
                Msg::SshHosts(hosts) => {
                    if let Some(sheet) = self.open_sheet.as_mut() {
                        sheet.hosts = hosts;
                    }
                }
                Msg::SettingsSaved(Err(e)) => {
                    self.toast(Toast::error("Could not save \"Don't ask again\" to settings.toml", e))
                }
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
                if let Some(ws) = self.state.active.clone() {
                    self.open_port(&ws, port);
                }
            }
            StatusAction::RemotePorts => self.show_remote_ports(),
            StatusAction::ShowFetchError => self.show_fetch_error(),
            StatusAction::ContinueRebase | StatusAction::AbortRebase => {
                if let Some(pane) = self.active_live().and_then(|l| l.git.as_mut()) {
                    match action {
                        StatusAction::ContinueRebase => pane.rebase_continue(),
                        _ => pane.rebase_abort(),
                    }
                }
            }
            StatusAction::ContinueOp | StatusAction::AbortOp | StatusAction::CreateBranch => {
                self.set_git_view(crate::model::state::GitView::Changes);
            }
        }
    }

    /// §5.20 "Don't ask again": update the in-memory settings now; on a worker thread (§2
    /// "Never block"), add just that key to the file as it is on disk — never the startup
    /// snapshot. A failed write or an unparsable file becomes a toast.
    fn dont_ask_again(&mut self, ctx: &egui::Context, kind: crate::model::confirm::ConfirmKind) {
        /// Read-modify-write jobs run one at a time, so two quick saves never drop one.
        static WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        if self.settings.set_dont_ask(kind) {
            let path = self.dirs.settings();
            super::jobs::spawn(ctx, &self.tx, move || {
                let _guard = WRITE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                Msg::SettingsSaved(Settings::persist_dont_ask(&path, kind))
            });
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

    fn on_git_events(&mut self, ctx: &egui::Context, events: Vec<GitEvent>) {
        for ev in events {
            match ev {
                GitEvent::Toast(t) => self.toast(t),
                GitEvent::SendToTerminal(text) => self.write_to_focused(text.into_bytes()),
                GitEvent::SummaryChanged => {}
                GitEvent::DontAskAgain(kind) => self.dont_ask_again(ctx, kind),
                GitEvent::Sync(request) => self.on_sync_request(ctx, request),
                GitEvent::BindRemote(remote) => self.bind_active_to_remote(&remote),
                GitEvent::HostUnreachable => self.probe_active_host(),
                GitEvent::MaximizePane(true) => {
                    self.git_maximized_before.get_or_insert((self.state.active.clone(), self.git_maximized));
                    self.git_maximized = true;
                    self.focus = Focus::Git;
                }
                GitEvent::MaximizePane(false) => {
                    if let Some((_, before)) = self.git_maximized_before.take() {
                        self.git_maximized = before;
                    }
                }
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.time = ctx.input(|i| i.time).max(f64::MIN_POSITIVE);
        let was_focused =
            std::mem::replace(&mut self.focused_window, ctx.input(|i| i.viewport().focused.unwrap_or(true)));
        if self.focused_window && !was_focused {
            self.refresh_active_remote_git(); // §5.25: remote repos refresh on focus
        }
        let now = crate::util::unix_now();
        self.apply_theme(&ctx);
        self.drain_messages(&ctx);
        self.poll_ssh(&ctx);
        self.drain_control(&ctx, now);
        self.poll_terminals(&ctx, now);
        self.handle_shortcuts(&ctx);
        self.handle_dropped_files(&ctx);
        self.scan_ports(&ctx);
        self.tick_sync(&ctx, now);

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
        // A view's maximize belongs to the workspace it was opened in.
        if self.git_maximized_before.as_ref().is_some_and(|(ws, _)| *ws != self.state.active)
            && let Some((_, before)) = self.git_maximized_before.take()
        {
            self.git_maximized = before;
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
        // `max_rect`, not `ui_contains_pointer`: nothing is laid out yet, so `min_rect` is empty.
        if ui.rect_contains_pointer(ui.max_rect()) && ui.input(|i| i.pointer.any_pressed()) {
            self.focus = Focus::Git;
        }
        let (colors, settings, preset) = (self.colors, self.settings.clone(), self.keymap.preset);
        let focused = self.focus == Focus::Git;
        let active = self.state.active.clone().unwrap_or_default();
        let bound = self
            .state
            .repos
            .iter()
            .flat_map(|r| r.remotes.iter())
            .find_map(|g| g.workspaces.iter().any(|w| w.id == active).then(|| g.name.clone()));
        let rebase_helper = self.remote_rebase_helper(&active);
        let events = self.active_live().and_then(|l| l.git.as_mut()).map(|g| {
            g.set_app_context(focused, bound.as_deref());
            if let Some(helper) = rebase_helper {
                g.set_rebase_helper(helper);
            }
            g.show(ui, &colors, &settings, preset, now)
        });
        if events.is_none()
            && let Some(state) = self.remote_state(&active)
        {
            // A remote repo loads once its host answers (§5.28 step 5).
            ui.weak(format!("Repository not loaded yet ({state})"));
        }
        self.on_git_events(ui.ctx(), events.unwrap_or_default());
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
        if let Some(sheet) = self.open_sheet.take() {
            self.open_sheet = self.show_open_sheet(ctx, sheet);
        }
        self.remote_overlays(ctx);
        self.clone_sheet(ctx);
        self.toasts.show(ctx, &self.colors, self.time);
    }

    /// §5.3: draw the clone sheet / progress pill; a finished clone joins the recents and opens
    /// as a workspace (§5.2) when asked.
    fn clone_sheet(&mut self, ctx: &egui::Context) {
        for event in self.clone.show(ctx, &self.colors, &self.settings) {
            match event {
                super::clone::CloneEvent::Toast(t) => self.toast(t),
                super::clone::CloneEvent::Cloned { location, name, open } => {
                    self.state.touch_recent(location.clone(), &name, crate::util::unix_now());
                    self.dirty = true;
                    if open {
                        self.open(ctx, location, None, None);
                    }
                }
            }
        }
    }

    /// §5.25: a remote repo has no watcher; it refreshes when its workspace gains focus.
    fn refresh_active_remote_git(&mut self) {
        let Some(id) = self.state.active.clone() else { return };
        if !self.remote_down(&id)
            && let Some(live) = self.live.get_mut(&id).filter(|l| l.remote.is_some())
            && let Some(git) = live.git.as_mut()
        {
            git.refresh();
        }
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
