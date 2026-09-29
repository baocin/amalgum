//! Remote workspaces in the app (§5.28, W16): the app owns one ssh [`Manager`] (one
//! ControlMaster per host, every ssh command on a worker), polls it each frame — its `wake`
//! callback repaints — and turns its [`HostEvent`]s into terminals, git panes, toasts, and the
//! W16 progress line, banner, and host-key dialog. A remote workspace is created at once in a
//! connecting state under a placeholder repo group, then moved under its real repo once the
//! host reports the repo's remotes (§5.28 step 5).

use super::workspace::identity;
use super::{App, Live};
use crate::agent::status::Status;
use crate::git::{GitError, Location};
use crate::model::confirm::{Confirm, ConfirmKind, Decision};
use crate::model::theme::Token;
use crate::ssh::manager::{HostEvent, Manager};
use crate::ssh::runner::SystemRunner;
use crate::ssh::state::{Machine, Phase, Warning};
use crate::ssh::steps::Drained;
use crate::ui::chrome::Toast;
use crate::ui::dialogs::{self, HostKeyChoice};
use crate::ui::git_pane::GitPane;
use crate::ui::sidebar::RemoteLink;
use std::sync::Arc;

/// A remote workspace's runtime beside its terminals.
pub(super) struct RemoteLive {
    pub host: String,
    /// The connect attempt (machine epoch) the terminals run on; a newer one re-runs them.
    pub spawned: Option<u64>,
    /// `amalgum open --run`: typed into the first terminal once it starts.
    pub run: Option<String>,
}

/// A remote port opened on a local one with `ssh -L` (§5.28 "Ports", W18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Forward {
    pub workspace: String,
    pub host: String,
    pub remote_port: u16,
    pub local_port: u16,
}

/// The app's ssh side: the manager and what the remote UI keeps between frames.
pub(super) struct Remote {
    pub ssh: Manager,
    pub forwards: Vec<Forward>,
    /// Forwards asked for and not answered yet: `(host, remote port, workspace)`.
    pending: Vec<(String, u16, String)>,
    /// W18 for this workspace, with the "remote port" field.
    ports: Option<(String, String)>,
    /// Closing a remote workspace whose terminals live in tmux: keep or kill them (§5.22).
    closing: Option<String>,
    /// §5.20 **Kill sessions** on a host, and the workspace to close afterwards.
    kill: Option<(String, Option<String>, Confirm)>,
}

impl Remote {
    pub fn new(
        ctx: &egui::Context,
        dirs: &crate::paths::Dirs,
        settings: &crate::model::settings::Settings,
    ) -> Self {
        // Names this app's reverse socket on hosts: stable per data directory.
        let app_id = format!("{:016x}", crate::util::fnv1a64(dirs.socket().display().to_string().as_bytes()));
        let wake_ctx = ctx.clone();
        let ssh = Manager::new(
            settings.ssh.manager_settings(dirs, &app_id),
            Arc::new(SystemRunner),
            Arc::new(crate::ssh::steps::embedded_cli_source),
            Arc::new(move || wake_ctx.request_repaint()),
        );
        Self { ssh, forwards: Vec::new(), pending: Vec::new(), ports: None, closing: None, kill: None }
    }
}

/// The sidebar subtitle's note for a remote row (W3): `None` once connected.
pub(super) fn remote_note(phase: &Phase, now: f64) -> Option<String> {
    Some(match phase {
        Phase::Connected { .. } => return None,
        Phase::Disconnected => "disconnected".into(),
        Phase::Connecting { .. } => "connecting…".into(),
        Phase::HostKeyUnknown { .. } => "unknown host key".into(),
        Phase::Failed { .. } => "can't connect".into(),
        Phase::Lost { outage, .. } | Phase::Retrying { outage, .. } if outage.ever_connected => {
            format!("reconnecting {}s", (now - outage.since).max(0.0) as u64)
        }
        Phase::Lost { .. } | Phase::Retrying { .. } => "unreachable · retrying".into(),
    })
}

/// A warning toast worth showing: the tmux ones only when some workspace on the host asked for
/// tmux (W15 **Keep sessions alive**), since hosts are always checked for it.
pub(super) fn shows_warning(warning: &Warning, host_wants_tmux: bool) -> bool {
    host_wants_tmux || !matches!(warning, Warning::NoTmux | Warning::OldTmux { .. })
}

/// Connecting, lost or waiting to retry: a countdown to animate and a loop Disconnect stops.
pub(super) fn retrying(phase: &Phase) -> bool {
    matches!(phase, Phase::Connecting { .. } | Phase::Lost { .. } | Phase::Retrying { .. })
}

/// Queued requests the app acts on: notification-type commands only (§5.29). The rest is
/// reported as unreadable instead of opening, focusing or running anything (invariant 10).
pub(super) fn notifications_only(mut drained: Drained) -> Drained {
    let (keep, reject): (Vec<_>, Vec<_>) = drained.events.into_iter().partition(|r| r.cmd.is_notification());
    drained.events = keep;
    drained.invalid.extend(
        reject.into_iter().map(|r| (r.to_line().trim_end().to_string(), "not a notification".into())),
    );
    drained
}

/// `~/w/amalgum` → `amalgum`; the placeholder repo name before the host answers.
pub(super) fn dir_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(path)
}

fn error_toast(title: impl Into<String>, e: &GitError) -> Toast {
    Toast::error(title, format!("{}\n{}", e.command, e.stderr))
}

impl App {
    /// `amalgum open host:path`, W15 **Remote host**, a remote recent or clone: create the
    /// workspace now, connecting (§5.28 "Connect"); its repo group follows once identified.
    pub(super) fn open_remote(
        &mut self,
        ctx: &egui::Context,
        location: Location,
        name: Option<String>,
        run: Option<String>,
        keep_sessions: bool,
    ) {
        let Location::Remote { host, path } = location.clone() else { return };
        if let Some(existing) = self.state.workspaces().iter().find(|w| w.location == location) {
            let id = existing.id.clone();
            return self.activate(ctx, &id);
        }
        if !crate::ssh::valid_host(&host) {
            return self.toast(Toast::info(format!("{host:?} is not a usable ssh host")));
        }
        let label = location.display(None);
        let repo_name = dir_name(&path).to_string();
        let mut ws =
            self.new_workspace_state(location.clone(), name.unwrap_or_else(|| format!("{host}:{repo_name}")));
        ws.keep_sessions = keep_sessions;
        let id = ws.id.clone();
        self.state.add_workspace(&crate::git::remote::repo_id(None, &label), &repo_name, "", None, ws);
        self.state.touch_recent(location, &format!("{repo_name} / {host}"), crate::util::unix_now());
        self.state.active = Some(id.clone());
        self.dirty = true;
        self.ensure_live(ctx, &id);
        self.run_when_spawned(&id, run);
        self.focus = super::Focus::Terminal;
    }

    /// `ensure_live` for a remote workspace: take a hold on the host (connecting it if needed).
    /// Terminals start once connected; the git pane once the repo is identified.
    pub(super) fn remote_live(&mut self, id: &str, host: &str, path: &str) -> Live {
        if let Err(e) = self.remote.ssh.acquire(host, id, path, self.time) {
            self.toast(Toast::info(e));
        }
        let remote = RemoteLive { host: host.to_string(), spawned: None, run: None };
        Live { remote: Some(remote), ..Live::default() }
    }

    /// The host of a remote workspace, and its connection.
    pub(super) fn machine_of(&self, workspace: &str) -> Option<(&str, &Machine)> {
        let host = self.live.get(workspace)?.remote.as_ref()?.host.as_str();
        Some((host, self.remote.ssh.machine(host)?))
    }

    /// A remote workspace whose host is not connected: terminals grey out, git pauses.
    pub(super) fn remote_down(&self, workspace: &str) -> bool {
        self.live
            .get(workspace)
            .and_then(|l| l.remote.as_ref())
            .is_some_and(|r| self.remote.ssh.machine(&r.host).is_none_or(Machine::is_down))
    }

    fn workspaces_on(&self, host: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .live
            .iter()
            .filter(|(_, l)| l.remote.as_ref().is_some_and(|r| r.host == host))
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Every frame: apply the manager's results and keep the clock moving for the banner's
    /// countdown and the liveness probe.
    pub(super) fn poll_ssh(&mut self, ctx: &egui::Context) {
        for event in self.remote.ssh.poll(self.time) {
            self.on_host_event(ctx, event);
        }
        let phases: Vec<&Phase> =
            self.remote.ssh.hosts().filter_map(|h| self.remote.ssh.machine(h)).map(Machine::phase).collect();
        if phases.iter().any(|p| retrying(p)) {
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        } else if phases.iter().any(|p| matches!(p, Phase::Connected { .. })) {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(
                self.remote.ssh.settings().probe_interval,
            ));
        }
    }

    fn on_host_event(&mut self, ctx: &egui::Context, event: HostEvent) {
        match event {
            HostEvent::Changed { host } => self.host_changed(ctx, &host),
            HostEvent::Warning { host, warning } => {
                let wants_tmux = self
                    .workspaces_on(&host)
                    .iter()
                    .any(|id| self.state.workspace(id).is_some_and(|w| w.keep_sessions));
                if shows_warning(&warning, wants_tmux) {
                    let details = match &warning {
                        Warning::QueueMode { ssh_said } => ssh_said.clone(),
                        Warning::StepFailed { error, .. } => format!("{}\n{}", error.command, error.stderr),
                        Warning::CliUnavailable { reason } => reason.clone(),
                        Warning::NoTmux | Warning::OldTmux { .. } => String::new(),
                    };
                    self.toast(Toast::warning(warning.text(&host), details));
                }
            }
            HostEvent::Drained { host, drained } => {
                // Queued hook events take the socket path, labelled with their real time.
                // Only notification-type commands are ever queued (§5.29); anything else in the
                // file is host text and must not open, focus or run anything here (invariant 10).
                let now = crate::util::unix_now();
                let drained = notifications_only(drained);
                for request in drained.events {
                    self.on_request(ctx, request, now);
                }
                if !drained.invalid.is_empty() {
                    let details: Vec<String> =
                        drained.invalid.iter().map(|(line, why)| format!("{why}: {line}")).collect();
                    self.toast(Toast::warning(
                        format!(
                            "{} queued notification(s) from {host} could not be read",
                            drained.invalid.len()
                        ),
                        details.join("\n"),
                    ));
                }
            }
            HostEvent::RepoIdentity { host, workspace, result } => {
                self.on_identity(ctx, &host, &workspace, result)
            }
            HostEvent::PortForwarded { host, remote_port, result } => {
                let Some(i) =
                    self.remote.pending.iter().position(|(h, p, _)| *h == host && *p == remote_port)
                else {
                    return;
                };
                let (_, _, workspace) = self.remote.pending.remove(i);
                let connected = self.remote.ssh.machine(&host).is_some_and(Machine::is_connected);
                match result {
                    // Disconnected meanwhile: the engine already closed it again.
                    Ok(_) if !connected => {}
                    Ok(local_port) => {
                        self.remote.forwards.push(Forward { workspace, host, remote_port, local_port });
                        let _ =
                            crate::platform::open_in_default_app(&format!("http://localhost:{local_port}"));
                    }
                    Err(e) => {
                        self.toast(error_toast(format!("Could not forward :{remote_port} from {host}"), &e))
                    }
                }
            }
            HostEvent::PortClosed { host, local_port, result } => {
                let closed =
                    self.remote.forwards.iter().position(|f| f.host == host && f.local_port == local_port);
                let Some(f) = closed.map(|i| self.remote.forwards.remove(i)) else { return };
                if let Err(e) = result {
                    self.toast(Toast::warning(
                        format!("Forward of :{} from {host} closed", f.remote_port),
                        e.stderr,
                    ));
                }
            }
            HostEvent::SessionEnded { host, result: Err(e) } => {
                self.toast(error_toast(format!("Could not end a closed tab's tmux session on {host}"), &e))
            }
            HostEvent::SessionEnded { result: Ok(()), .. } => {}
            HostEvent::SessionsKilled { host, result } => match result {
                Ok(()) => self.toast(Toast::success(format!("Killed the tmux sessions on {host}"))),
                Err(e) => self.toast(error_toast(format!("Could not kill the tmux sessions on {host}"), &e)),
            },
        }
    }

    /// The host's phase changed: start or re-run terminals once connected, pause git while not.
    fn host_changed(&mut self, ctx: &egui::Context, host: &str) {
        let Some(machine) = self.remote.ssh.machine(host) else { return };
        let (connected, epoch, phase) = (machine.is_connected(), machine.epoch(), machine.phase().clone());
        if let Phase::Failed { error, .. } = &phase {
            self.toast(error_toast(format!("Can't connect to {host}: {}", error.summary()), error));
        }
        if !connected {
            // `-L` forwards die with the master and Disconnect closes them; a reconnect does
            // not reopen them (§5.28), so none of them is usable any more.
            self.remote.forwards.retain(|f| f.host != host);
            self.remote.pending.retain(|(h, _, _)| h != host);
        }
        for id in self.workspaces_on(host) {
            let Some(live) = self.live.get_mut(&id) else { continue };
            if let Some(git) = live.git.as_mut() {
                git.set_online(connected);
            }
            if phase == Phase::Disconnected {
                // Disconnect closes the terminals here; tmux keeps them on the host (§5.28).
                live.panes.clear();
                if let Some(r) = live.remote.as_mut() {
                    r.spawned = None;
                }
            }
            let stale = live.remote.as_ref().is_some_and(|r| r.spawned != Some(epoch));
            if connected && stale {
                self.spawn_remote_terminals(ctx, &id, epoch);
            }
        }
    }

    /// (Re)run every terminal of a remote workspace on the connection `epoch`: tmux reattaches
    /// with its scrollback; without tmux a fresh shell starts at the pane's last cwd.
    pub(super) fn spawn_remote_terminals(&mut self, ctx: &egui::Context, id: &str, epoch: u64) {
        let Some(ws) = self.state.workspace(id).cloned() else { return };
        let mut started = Vec::new();
        for tab in &ws.tabs {
            for pane in &tab.panes {
                match self.spawn_terminal(ctx, &ws, &tab.id, pane) {
                    Ok(term) => started.push((pane.id, term)),
                    Err(e) => self.toast(Toast::error("Could not start a terminal", e)),
                }
            }
        }
        let Some(live) = self.live.get_mut(id) else { return };
        live.panes.extend(started);
        let run = live.remote.as_mut().and_then(|r| {
            r.spawned = Some(epoch);
            r.run.take()
        });
        self.run_when_spawned(id, run);
    }

    /// `--run`: type `cmd` into the focused terminal now if the terminals run (the host was
    /// already connected), else once they start.
    fn run_when_spawned(&mut self, id: &str, run: Option<String>) {
        let Some(cmd) = run else { return };
        let focused = self.state.workspace(id).and_then(|w| w.tabs.get(w.active_tab)).map(|t| t.focused);
        let Some(live) = self.live.get_mut(id) else { return };
        let Some(remote) = live.remote.as_mut() else { return };
        match (remote.spawned, focused.and_then(|p| live.panes.get(&p))) {
            (Some(_), Some(term)) => term.write(format!("{cmd}\r").into_bytes()),
            _ => remote.run = Some(cmd),
        }
    }

    /// §5.28 step 5: nest the workspace under its repo and start (or resume) its git pane.
    fn on_identity(
        &mut self,
        ctx: &egui::Context,
        host: &str,
        workspace: &str,
        result: Result<Vec<crate::git::refs::Remote>, GitError>,
    ) {
        let Some(ws) = self.state.workspace(workspace).cloned() else { return };
        let Location::Remote { path, .. } = &ws.location else { return };
        let remotes = match result {
            Ok(remotes) => remotes,
            Err(e) => {
                let title = format!("No git repository at {}: {}", ws.location.display(None), e.summary());
                return self.toast(error_toast(title, &e));
            }
        };
        let id = identity(&remotes, &ws.location.display(None), Some(dir_name(path).to_string()));
        let (remote_name, url) = id.remote.clone().unzip();
        self.dirty |= self.state.rehome_workspace(
            workspace,
            &id.repo_id,
            &id.repo_name,
            remote_name.as_deref().unwrap_or(""),
            url.as_deref(),
        );
        let journal = self.dirs.journal(&id.repo_id);
        let git = self.remote.ssh.git(host, path);
        // The reply may land after the host dropped again: no polling a dead master.
        let online = self.remote.ssh.machine(host).is_some_and(Machine::is_connected);
        let Some(live) = self.live.get_mut(workspace) else { return };
        match (&mut live.git, git) {
            (Some(pane), _) => pane.set_online(online),
            (None, Some(git)) => {
                let mut pane = GitPane::with_git(git, journal, ctx);
                pane.set_view(ws.git_view);
                pane.set_online(online);
                live.git = Some(pane);
            }
            (None, None) => {}
        }
    }

    /// Closing a workspace: a remote one closes its forwards and lets go of its host.
    pub(super) fn release_remote(&mut self, id: &str) {
        let Some(host) = self.live.get(id).and_then(|l| l.remote.as_ref()).map(|r| r.host.clone()) else {
            return;
        };
        for f in self.remote.forwards.iter().filter(|f| f.workspace == id) {
            self.remote.ssh.cancel_port(&f.host, f.local_port);
        }
        self.remote.ssh.release(&host, id, self.time);
    }

    /// Close, asking first for a workspace whose terminals live in tmux on the host (§5.22:
    /// keep the sessions, the default, or kill them).
    pub(super) fn close_remote_or_ask(&mut self, id: &str) -> bool {
        let keeps = self.live.get(id).is_some_and(|l| l.remote.is_some())
            && self.state.workspace(id).is_some_and(|w| w.keep_sessions)
            && !self.remote_down(id);
        if keeps {
            self.remote.closing = Some(id.to_string());
        }
        keeps
    }

    /// Row menu **Disconnect** / **Reconnect** / **Kill sessions…** / W18.
    pub(super) fn on_remote_row(&mut self, id: &str, action: RowAction) {
        let Some(host) = self.live.get(id).and_then(|l| l.remote.as_ref()).map(|r| r.host.clone()) else {
            return;
        };
        match action {
            RowAction::Disconnect => self.remote.ssh.disconnect(&host, self.time),
            RowAction::Reconnect => self.remote.ssh.retry_now(&host, self.time),
            RowAction::KillSessions => self.ask_kill_sessions(&host, None),
            RowAction::Ports => self.remote.ports = Some((id.to_string(), String::new())),
        }
    }

    fn ask_kill_sessions(&mut self, host: &str, then_close: Option<String>) {
        let sessions: Vec<String> = self
            .workspaces_on(host)
            .iter()
            .filter_map(|id| self.state.workspace(id))
            .flat_map(|w| {
                w.tabs.iter().flat_map(move |t| {
                    t.panes
                        .iter()
                        .map(move |p| format!("{} · {}", w.name, p.title.as_deref().unwrap_or("shell")))
                })
            })
            .collect();
        let confirm =
            Confirm::new(ConfirmKind::KillRemoteSessions, format!("Kill the tmux sessions on {host}?"))
                .detail("Everything running in these terminals on the host ends:")
                .lost(sessions);
        self.remote.kill = Some((host.to_string(), then_close, confirm));
    }

    /// Ports shown on the row and status bar: a remote workspace's forwarded ports.
    pub(super) fn workspace_ports(&self, workspace: &str) -> Vec<u16> {
        if self.live.get(workspace).is_some_and(|l| l.remote.is_some()) {
            let ports = self.remote.forwards.iter().filter(|f| f.workspace == workspace);
            return ports.map(|f| f.remote_port).collect();
        }
        self.ports.get(workspace).cloned().unwrap_or_default()
    }

    /// Open a port listed for `workspace`: through its `ssh -L` forward when remote.
    pub(super) fn open_port(&self, workspace: &str, port: u16) {
        let local =
            match self.remote.forwards.iter().find(|f| f.workspace == workspace && f.remote_port == port) {
                Some(f) => f.local_port,
                None if self.live.get(workspace).is_some_and(|l| l.remote.is_some()) => return,
                None => port,
            };
        let _ = crate::platform::open_in_default_app(&format!("http://localhost:{local}"));
    }

    /// Status bar: "gpu-box · connected".
    pub(super) fn remote_state(&self, workspace: &str) -> Option<String> {
        let (host, machine) = self.machine_of(workspace)?;
        let state = remote_note(machine.phase(), self.time).unwrap_or_else(|| "connected".into());
        Some(format!("{host} · {state}"))
    }

    /// Sidebar row facts for a remote workspace: ⊘ while disconnected and the W3 note.
    pub(super) fn remote_row(&self, workspace: &str, info: &mut crate::ui::sidebar::RowInfo) {
        let Some(remote) = self.live.get(workspace).and_then(|l| l.remote.as_ref()) else { return };
        let machine = self.remote.ssh.machine(&remote.host);
        let connected = machine.is_some_and(Machine::is_connected);
        info.remote = Some(match machine.map(Machine::phase) {
            Some(Phase::Connected { .. }) => RemoteLink::Connected,
            Some(p) if retrying(p) => RemoteLink::Reconnecting,
            _ => RemoteLink::Down,
        });
        if !connected {
            info.status = Status::Disconnected;
            info.low_confidence = false;
            info.note = Some(
                machine.and_then(|m| remote_note(m.phase(), self.time)).unwrap_or("disconnected".into()),
            );
        }
    }

    /// W16 above (progress line) and below (banner) a remote workspace's terminals. Returns
    /// the terminal area left over, and whether the terminals are greyed out.
    pub(super) fn remote_chrome(
        &mut self,
        ui: &mut egui::Ui,
        workspace: &str,
        area: egui::Rect,
    ) -> (egui::Rect, bool) {
        let Some((host, machine)) = self.machine_of(workspace) else {
            return (area, self.remote_down(workspace));
        };
        if machine.is_connected() {
            return (area, false);
        }
        let host = host.to_string();
        let (phase, now) = (machine.phase().clone(), self.time);
        let line = match &phase {
            Phase::Disconnected => Some(format!("Disconnected from {host}")),
            Phase::HostKeyUnknown { .. } => Some(format!("Waiting for you to trust {host}'s host key")),
            _ => machine.progress_line(),
        };
        let progress = machine.progress();
        let banner = machine.banner(now);
        let error = machine.last_error().cloned();
        let row = 30.0;
        let mut area = area;
        let mut clicked = None;
        let colors = self.colors;
        if let Some(line) = line {
            let rect = egui::Rect::from_min_size(area.min, egui::vec2(area.width(), row));
            area.min.y += row;
            let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(8.0, 4.0))));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let button = match phase {
                    Phase::Disconnected => Some(("Reconnect", RowAction::Reconnect)),
                    Phase::Connecting { .. } | Phase::Lost { .. } | Phase::Retrying { .. } => {
                        Some(("Cancel", RowAction::Disconnect))
                    }
                    _ => None,
                };
                if let Some((label, action)) = button
                    && ui.button(label).clicked()
                {
                    clicked = Some(action);
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if matches!(phase, Phase::Connecting { .. } | Phase::Lost { .. }) {
                        ui.add(egui::Spinner::new().size(12.0).color(colors.get(Token::Accent)));
                    }
                    if let Some((done, total)) = progress {
                        let bar: String = (0..total).map(|i| if i <= done { '▮' } else { '▯' }).collect();
                        ui.colored_label(colors.get(Token::Accent), bar);
                    }
                    ui.add(egui::Label::new(line).truncate());
                });
            });
        }
        let mut details = false;
        if let Some(banner) = banner {
            let rect = egui::Rect::from_min_max(egui::pos2(area.min.x, area.max.y - row), area.max);
            area.max.y -= row;
            ui.painter().rect_filled(rect, 0.0, colors.get(Token::BgRaised));
            let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(8.0, 4.0))));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if error.is_some() && ui.button("Details").clicked() {
                    details = true;
                }
                let retry = if matches!(phase, Phase::Failed { .. }) { "Retry" } else { "Retry now" };
                if !matches!(phase, Phase::Lost { .. }) && ui.button(retry).clicked() {
                    clicked = Some(RowAction::Reconnect);
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let text = egui::RichText::new(format!("⚠ {banner}")).color(colors.get(Token::Warning));
                    ui.add(egui::Label::new(text).truncate());
                });
            });
        }
        if details && let Some(e) = &error {
            self.toast(error_toast(e.summary(), e));
        }
        match clicked {
            Some(RowAction::Disconnect) => self.remote.ssh.cancel(&host, now),
            Some(RowAction::Reconnect) => self.remote.ssh.retry_now(&host, now),
            _ => {}
        }
        (area, true)
    }

    /// The W16 host-key dialog, the close/kill prompts, and the W18 ports popover.
    pub(super) fn remote_overlays(&mut self, ctx: &egui::Context) {
        let asking = self.remote.ssh.hosts().find_map(|h| match self.remote.ssh.machine(h)?.phase() {
            Phase::HostKeyUnknown { prompt, ssh_said } => {
                Some((h.to_string(), prompt.clone(), ssh_said.clone()))
            }
            _ => None,
        });
        if let Some((host, prompt, ssh_said)) = asking {
            match dialogs::host_key_dialog(ctx, &self.colors, self.keymap.preset, &prompt, &ssh_said) {
                Some(HostKeyChoice::CopyFingerprint) => ctx.copy_text(prompt.fingerprint.clone()),
                Some(HostKeyChoice::Trust) => self.remote.ssh.trust(&host, self.time),
                Some(HostKeyChoice::Cancel) => self.remote.ssh.cancel(&host, self.time),
                None => {}
            }
            return;
        }
        if let Some((host, then_close, mut confirm)) = self.remote.kill.take() {
            match dialogs::confirm_dialog(ctx, &self.colors, self.keymap.preset, &mut confirm) {
                Some(Decision::Confirm) => {
                    self.remote.ssh.kill_sessions(&host);
                    if let Some(id) = then_close {
                        self.finish_close(&id);
                    }
                }
                Some(Decision::Cancel) => {}
                None => self.remote.kill = Some((host, then_close, confirm)),
            }
            return;
        }
        if let Some(id) = self.remote.closing.take() {
            self.close_prompt(ctx, id);
        }
        if let Some((id, field)) = self.remote.ports.take() {
            self.ports_popover(ctx, id, field);
        }
    }

    fn close_prompt(&mut self, ctx: &egui::Context, id: String) {
        let Some(ws) = self.state.workspace(&id) else { return };
        let (title, host) = (
            format!("Close {}?", ws.name),
            self.live.get(&id).and_then(|l| l.remote.as_ref()).map(|r| r.host.clone()),
        );
        let Some(host) = host else { return };
        let mut choice = None;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("Its terminals run in tmux on {host}."));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        choice = Some(None);
                    }
                    if ui.button("Kill sessions…").clicked() {
                        choice = Some(Some(true));
                    }
                    let keep = ui.button("Keep tmux sessions on host");
                    keep.request_focus();
                    if keep.clicked() {
                        choice = Some(Some(false));
                    }
                });
            });
        match choice {
            None => self.remote.closing = Some(id),
            Some(None) => {}
            Some(Some(false)) => self.finish_close(&id),
            Some(Some(true)) => self.ask_kill_sessions(&host, Some(id)),
        }
    }

    /// W18 for a remote workspace: its `ssh -L` forwards with **Open** and **Stop**, and a
    /// field to forward another port (listening remote ports are not discovered yet).
    fn ports_popover(&mut self, ctx: &egui::Context, id: String, mut field: String) {
        let Some(host) = self.live.get(&id).and_then(|l| l.remote.as_ref()).map(|r| r.host.clone()) else {
            return;
        };
        let forwards: Vec<Forward> =
            self.remote.forwards.iter().filter(|f| f.workspace == id).cloned().collect();
        let (mut open, mut stop, mut forward, mut close) = (None, None, None, false);
        let secondary = self.colors.get(Token::FgSecondary);
        egui::Window::new("Ports")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Forwarded from this workspace's host").strong());
                if forwards.is_empty() {
                    ui.colored_label(secondary, "Nothing forwarded yet.");
                }
                for f in &forwards {
                    ui.horizontal(|ui| {
                        ui.monospace(format!(":{}  → localhost:{}", f.remote_port, f.local_port));
                        if ui.button("Open ↗").clicked() {
                            open = Some(f.local_port);
                        }
                        if ui.button("Stop").clicked() {
                            stop = Some(f.local_port);
                        }
                    });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Remote port");
                    let edit =
                        ui.add(egui::TextEdit::singleline(&mut field).desired_width(80.0).hint_text("3000"));
                    let port = field.trim().parse::<u16>().ok().filter(|p| *p > 0);
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.add_enabled(port.is_some(), egui::Button::new("Forward")).clicked() || enter)
                        && let Some(p) = port
                    {
                        forward = Some(p);
                    }
                });
                ui.colored_label(secondary, format!("Remote: opens via ssh -L on {host}"));
                if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        if let Some(local) = open {
            let _ = crate::platform::open_in_default_app(&format!("http://localhost:{local}"));
        }
        if let Some(local) = stop {
            self.remote.ssh.cancel_port(&host, local);
        }
        if let Some(port) = forward {
            self.remote.ssh.forward_port(&host, port);
            self.remote.pending.push((host, port, id.clone()));
            field.clear();
        }
        if !close {
            self.remote.ports = Some((id, field));
        }
    }

    /// A remote git command failed with ssh's 255: check the active workspace's master now.
    pub(super) fn probe_active_host(&mut self) {
        let host =
            self.state.active.as_deref().and_then(|id| self.machine_of(id)).map(|(h, _)| h.to_string());
        if let Some(host) = host {
            self.remote.ssh.probe(&host);
        }
    }

    /// W18 from the status bar: the active remote workspace's ports.
    pub(super) fn show_remote_ports(&mut self) {
        if let Some(id) =
            self.state.active.clone().filter(|id| self.live.get(id).is_some_and(|l| l.remote.is_some()))
        {
            self.remote.ports = Some((id, String::new()));
        }
    }
}

/// Remote row actions (sidebar menu, W16 buttons).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowAction {
    Disconnect,
    Reconnect,
    KillSessions,
    Ports,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::state::{Outage, Step};

    #[test]
    fn row_notes_follow_the_connection() {
        assert_eq!(remote_note(&Phase::Connected { warnings: vec![] }, 5.0), None);
        assert_eq!(
            remote_note(&Phase::Connecting { step: Step::Cli, trust: false }, 0.0).unwrap(),
            "connecting…"
        );
        let outage = Outage { since: 10.0, attempt: 2, ever_connected: true };
        assert_eq!(
            remote_note(&Phase::Retrying { outage, next_at: 30.0 }, 22.4).unwrap(),
            "reconnecting 12s"
        );
        let never = Outage { ever_connected: false, ..outage };
        assert_eq!(
            remote_note(&Phase::Lost { outage: never, step: Step::Master }, 22.0).unwrap(),
            "unreachable · retrying"
        );
        assert_eq!(remote_note(&Phase::Disconnected, 0.0).unwrap(), "disconnected");
    }

    #[test]
    fn tmux_warnings_only_matter_to_workspaces_that_asked_for_tmux() {
        assert!(!shows_warning(&Warning::NoTmux, false));
        assert!(!shows_warning(&Warning::OldTmux { version: "tmux 3.0".into() }, false));
        assert!(shows_warning(&Warning::NoTmux, true));
        assert!(shows_warning(&Warning::QueueMode { ssh_said: String::new() }, false));
        assert!(shows_warning(&Warning::CliUnavailable { reason: "no build".into() }, false));
    }

    #[test]
    fn only_notifications_survive_a_drain() {
        use crate::ctl::protocol::{Command, Request};
        let note = Request::new(Command::ClearStatus { workspace: None, tab: None });
        let run = Request::new(Command::Run {
            command: "rm -rf ~".into(),
            split: None,
            workspace: None,
            tab: None,
        });
        let drained = notifications_only(Drained { events: vec![note.clone(), run], invalid: vec![] });
        assert_eq!(drained.events, vec![note]);
        assert_eq!(drained.invalid.len(), 1);
        assert!(drained.invalid[0].0.contains("rm -rf"));
    }

    #[test]
    fn only_live_attempts_keep_the_clock_running() {
        let outage = Outage { since: 0.0, attempt: 1, ever_connected: true };
        assert!(retrying(&Phase::Retrying { outage, next_at: 5.0 }));
        assert!(retrying(&Phase::Lost { outage, step: Step::Master }));
        assert!(!retrying(&Phase::Disconnected));
        assert!(!retrying(&Phase::Connected { warnings: vec![] }));
    }

    #[test]
    fn placeholder_names_come_from_the_last_path_component() {
        assert_eq!(dir_name("~/w/amalgum"), "amalgum");
        assert_eq!(dir_name("~/w/amalgum/"), "amalgum");
        assert_eq!(dir_name("~"), "~");
        assert_eq!(dir_name("/"), "/"); // portability: allow
    }
}
