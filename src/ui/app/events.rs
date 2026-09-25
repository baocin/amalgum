//! Agent awareness plumbing (§5.29): control-socket requests and terminal escape sequences
//! become tracker observations, sidebar messages, and notification rows.

use super::{App, workspace::pane_key};
use crate::agent::adapters;
use crate::agent::notifications::{Level, Notification};
use crate::agent::osc::{OscEvent, PromptMark};
use crate::agent::status::{Observation, Source, Status};
use crate::ctl::protocol::{Command, Request, StatusArg};
use crate::ui::chrome::Toast;
use crate::ui::terminal::TermEvent;

/// A notification about to be stored, before the store assigns id/read.
struct Note {
    level: Level,
    title: String,
    body: String,
    workspace: Option<String>,
    tab: Option<String>,
    agent: Option<crate::agent::AgentKind>,
}

impl App {
    pub(super) fn drain_control(&mut self, ctx: &egui::Context, now: u64) {
        let requests = self.control.as_ref().map(|c| c.drain()).unwrap_or_default();
        for req in requests {
            self.on_request(ctx, req, now);
        }
    }

    fn on_request(&mut self, ctx: &egui::Context, req: Request, now: u64) {
        let at = req.ts.min(now);
        match req.cmd {
            Command::Notify { title, body, workspace, tab } => {
                self.notify(
                    Note {
                        level: Level::Info,
                        title,
                        body: body.unwrap_or_default(),
                        workspace,
                        tab,
                        agent: None,
                    },
                    at,
                );
            }
            Command::SetStatus { status, message, workspace, tab } => {
                let status = match status {
                    StatusArg::Running => Status::Running,
                    StatusArg::NeedsInput => Status::NeedsInput,
                    StatusArg::Idle => Status::Idle,
                };
                self.observe(workspace.as_deref(), tab.as_deref(), status, Source::Hook, message, at);
            }
            Command::ClearStatus { workspace, tab } => {
                if let Some(t) = self.tracker(workspace.as_deref(), tab.as_deref()) {
                    t.clear();
                }
            }
            Command::AgentEvent { agent, event, workspace, tab } => {
                let Some(ev) = adapters::adapt(agent, &event) else { return };
                self.observe(
                    workspace.as_deref(),
                    tab.as_deref(),
                    ev.status,
                    Source::Hook,
                    ev.message.clone(),
                    at,
                );
                if ev.notifies() {
                    let body = ev.message.clone().unwrap_or_default();
                    let note = Note {
                        level: Level::NeedsInput,
                        title: agent.name().into(),
                        body,
                        workspace: workspace.clone(),
                        tab,
                        agent: Some(agent),
                    };
                    self.notify(note, at);
                }
                // An agent turn may have committed or staged: refresh that workspace's git pane.
                if let Some(git) =
                    workspace.as_deref().and_then(|w| self.live.get_mut(w)).and_then(|l| l.git.as_mut())
                {
                    git.refresh();
                }
            }
            Command::Open { location, name, run, remote: _ } => {
                let loc = super::workspace::parse_user_location(&location);
                self.open(ctx, loc, name, run);
            }
            Command::Run { command, workspace, .. } => {
                if let Some(ws) = workspace.filter(|w| self.state.workspace(w).is_some()) {
                    self.state.active = Some(ws);
                }
                self.new_tab(ctx);
                self.write_to_focused(format!("{command}\r").into_bytes());
            }
            Command::Focus { target } => {
                let found = self
                    .state
                    .workspaces()
                    .iter()
                    .find(|w| w.id == target || w.name == target)
                    .map(|w| w.id.clone());
                match found {
                    Some(id) => {
                        self.state.active = Some(id);
                        self.dirty = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    None => self.toast(Toast::info(format!("amalgum focus: no workspace named {target}"))),
                }
            }
            Command::Resume { .. } | Command::Hibernate { .. } => {
                self.toast(Toast::info("Hibernation is not available in this build yet (SPEC §5.30)"));
            }
            Command::List => {}
        }
    }

    /// The tracker for a control request's target, if that pane exists.
    fn tracker(
        &mut self,
        workspace: Option<&str>,
        tab: Option<&str>,
    ) -> Option<&mut crate::agent::status::Tracker> {
        let (ws, tab) = (workspace?, tab?);
        let live = self.live.get_mut(ws)?;
        Some(live.trackers.entry(tab.to_string()).or_default())
    }

    fn observe(
        &mut self,
        ws: Option<&str>,
        tab: Option<&str>,
        status: Status,
        source: Source,
        message: Option<String>,
        at: u64,
    ) {
        if let (Some(ws), Some(msg)) = (ws, message.as_ref())
            && let Some(live) = self.live.get_mut(ws)
        {
            live.last_message = Some((msg.clone(), at));
        }
        if let Some(t) = self.tracker(ws, tab) {
            t.observe(Observation { status, source, message, at });
        }
    }

    fn notify(&mut self, n: Note, at: u64) {
        let focused_window = self.focused_window;
        let body_for_os = n.body.clone();
        let title_for_os = n.title.clone();
        let row = Notification {
            id: 0,
            time: at,
            level: n.level,
            title: n.title,
            body: n.body,
            workspace: n.workspace,
            tab: n.tab,
            agent: n.agent,
            commit: None,
            read: false,
        };
        if self.notifications.push(row).is_some() && !focused_window && self.settings.agents.os_notifications
        {
            let _ = crate::platform::notify(&title_for_os, &body_for_os);
        }
    }

    /// Drain every terminal's events: OSC status sources, titles, cwd, bells (§5.27, §5.29).
    pub(super) fn poll_terminals(&mut self, _ctx: &egui::Context, now: u64) {
        let mut observations = Vec::new();
        let mut state_changes = Vec::new();
        for ws in self.state.workspaces() {
            let Some(live) = self.live.get_mut(&ws.id) else { continue };
            for (tab_index, tab) in ws.tabs.iter().enumerate() {
                for pane in &tab.panes {
                    let Some(term) = live.panes.get_mut(&pane.id) else { continue };
                    let key = pane_key(&tab.id, pane.id);
                    let visible =
                        self.state.active.as_deref() == Some(ws.id.as_str()) && ws.active_tab == tab_index;
                    for ev in term.poll() {
                        match ev {
                            TermEvent::Osc(OscEvent::Notify { title, body }) => {
                                observations.push((
                                    ws.id.clone(),
                                    key.clone(),
                                    Status::NeedsInput,
                                    Source::Osc,
                                    Some(body.clone()),
                                ));
                                let title = title.unwrap_or_else(|| ws.name.clone());
                                state_changes.push(Change::Notify(ws.id.clone(), key.clone(), title, body));
                            }
                            TermEvent::Osc(OscEvent::Prompt(mark)) => {
                                let status = match mark {
                                    PromptMark::PromptStart | PromptMark::CommandFinished { .. } => {
                                        Status::Idle
                                    }
                                    PromptMark::CommandStart => continue,
                                    PromptMark::CommandExecuted => Status::Running,
                                };
                                observations.push((
                                    ws.id.clone(),
                                    key.clone(),
                                    status,
                                    Source::ShellIntegration,
                                    None,
                                ));
                            }
                            TermEvent::Osc(OscEvent::Cwd { path, .. }) => {
                                state_changes.push(Change::Cwd(ws.id.clone(), tab.id.clone(), pane.id, path));
                            }
                            TermEvent::Osc(OscEvent::Title(t)) | TermEvent::Title(Some(t)) => {
                                state_changes.push(Change::Title(
                                    ws.id.clone(),
                                    tab.id.clone(),
                                    pane.id,
                                    Some(t),
                                ));
                            }
                            TermEvent::Title(None) => state_changes.push(Change::Title(
                                ws.id.clone(),
                                tab.id.clone(),
                                pane.id,
                                None,
                            )),
                            TermEvent::Osc(OscEvent::Bell) | TermEvent::Bell if !visible => {
                                observations.push((
                                    ws.id.clone(),
                                    key.clone(),
                                    Status::NeedsInput,
                                    Source::Osc,
                                    None,
                                ));
                            }
                            TermEvent::Osc(OscEvent::Bell) | TermEvent::Bell | TermEvent::Exited(_) => {}
                        }
                    }
                }
            }
        }
        for (ws, key, status, source, message) in observations {
            self.observe(Some(&ws), Some(&key), status, source, message, now);
        }
        for change in state_changes {
            self.apply(change, now);
        }
    }

    fn apply(&mut self, change: Change, now: u64) {
        match change {
            Change::Notify(ws, key, title, body) => {
                self.notify(
                    Note {
                        level: Level::NeedsInput,
                        title,
                        body,
                        workspace: Some(ws),
                        tab: Some(key),
                        agent: None,
                    },
                    now,
                );
            }
            Change::Cwd(ws, tab, pane, path) => {
                if let Some(p) =
                    self.state.tab_mut(&ws, &tab).and_then(|t| t.panes.iter_mut().find(|p| p.id == pane))
                    && p.cwd.as_deref() != Some(path.as_str())
                {
                    p.cwd = Some(path);
                    self.dirty = true;
                }
            }
            Change::Title(ws, tab, pane, title) => {
                if let Some(p) =
                    self.state.tab_mut(&ws, &tab).and_then(|t| t.panes.iter_mut().find(|p| p.id == pane))
                {
                    p.title = title;
                }
            }
        }
    }
}

enum Change {
    Notify(String, String, String, String),
    Cwd(String, String, crate::model::layout::PaneId, String),
    Title(String, String, crate::model::layout::PaneId, Option<String>),
}
