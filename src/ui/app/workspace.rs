//! Opening locations (§5.2, §5.22) and the terminal/git runtime of each workspace (§5.27).

use super::{App, Focus, Live, Msg};
use crate::git::refs::{REMOTE_ARGS, parse_remotes};
use crate::git::{Git, Location, remote};
use crate::model::layout::{Axis, Closed, PaneId, Tree};
use crate::model::state::{GitView, Pane, Tab, Workspace};
use crate::ui::chrome::Toast;
use crate::ui::git_pane::GitPane;
use crate::ui::jobs;
use crate::ui::terminal::{Spawn, TerminalPane};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A location resolved to its repository (worker thread result).
pub(super) struct Opened {
    pub root: PathBuf,
    pub repo_id: String,
    pub repo_name: String,
    pub remote: Option<(String, String)>,
    pub branch: Option<String>,
    pub name: Option<String>,
    pub run: Option<String>,
}

/// `AMALGUM_TAB` for one terminal: unique per pane, stable across restarts.
pub(super) fn pane_key(tab: &str, pane: PaneId) -> String {
    format!("{tab}.{pane}")
}

/// Subtitle form of a location (`~/w/conduit`, `gpu-box:~/g`).
pub(super) fn location_label(loc: &Location, home: Option<&Path>) -> String {
    loc.display(home)
}

/// A path typed or pasted by the user: `host:path` stays remote, local paths expand `~`.
pub(super) fn parse_user_location(text: &str) -> Location {
    match Location::parse(text) {
        Location::Local { path } => {
            let path = match (path.strip_prefix("~"), crate::paths::home()) {
                (Ok(rest), Some(home)) => home.join(rest),
                _ => path,
            };
            Location::Local { path }
        }
        remote => remote,
    }
}

/// Worker: find the repo root, its primary remote, and the current branch (§5.2 steps 1–3).
fn resolve(loc: Location, name: Option<String>, run: Option<String>) -> Result<Opened, String> {
    let path = match loc {
        Location::Local { path } => path,
        Location::Remote { host, path } => {
            return Err(format!(
                "Remote workspaces are not available in this build yet ({host}:{path}).\nSSH workspaces (SPEC §5.28) are the next milestone."
            ));
        }
    };
    let top = Git::new(Location::Local { path: path.clone() })
        .run(&["rev-parse", "--show-toplevel"])
        .map_err(|e| format!("{}: {}\n{}", e.summary(), path.display(), e.stderr))?;
    let root = PathBuf::from(String::from_utf8_lossy(&top).trim());
    let git = Git::new(Location::Local { path: root.clone() });
    let remotes =
        git.run(REMOTE_ARGS).map(|o| parse_remotes(&String::from_utf8_lossy(&o))).unwrap_or_default();
    let primary = remotes.iter().find(|r| r.name == "origin").or(remotes.first());
    let remote = primary.map(|r| (r.name.clone(), r.fetch_url.clone()));
    let branch = git
        .run(&["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .filter(|b| !b.is_empty());
    let root_str = root.display().to_string();
    let repo_name = primary
        .and_then(|r| remote::normalize(&r.fetch_url))
        .and_then(|n| n.rsplit('/').next().map(str::to_string))
        .or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| root_str.clone());
    let repo_id = remote::repo_id(primary.map(|r| r.fetch_url.as_str()), &root_str);
    Ok(Opened { root, repo_id, repo_name, remote, branch, name, run })
}

impl App {
    /// Open or focus a workspace for `loc` (§5.2). Resolution runs on a worker.
    pub(super) fn open(
        &mut self,
        ctx: &egui::Context,
        loc: Location,
        name: Option<String>,
        run: Option<String>,
    ) {
        if let Some(existing) = self.state.workspaces().iter().find(|w| same_location(&w.location, &loc)) {
            self.state.active = Some(existing.id.clone());
            self.dirty = true;
            return;
        }
        jobs::spawn(ctx, &self.tx, move || Msg::Opened(resolve(loc, name, run)));
    }

    pub(super) fn on_opened(&mut self, ctx: &egui::Context, o: Opened) {
        let location = Location::Local { path: o.root.clone() };
        if let Some(existing) = self.state.workspaces().iter().find(|w| w.location == location) {
            self.state.active = Some(existing.id.clone());
            self.dirty = true;
            return;
        }
        let id = self.state.new_id("w");
        let tab_id = self.state.new_id("t");
        let pane_id = self.next_pane_id();
        let ws = Workspace {
            name: o.name.clone().or(o.branch.clone()).unwrap_or_else(|| o.repo_name.clone()),
            id: id.clone(),
            location: location.clone(),
            tabs: vec![Tab {
                id: tab_id,
                name: None,
                layout: Tree::Leaf(pane_id),
                panes: vec![Pane { id: pane_id, cwd: None, title: None, agent: None }],
                focused: pane_id,
            }],
            active_tab: 0,
            git_view: GitView::Graph,
            git_pane_open: true,
        };
        let (remote_name, url) = o.remote.clone().unzip();
        self.state.add_workspace(
            &o.repo_id,
            &o.repo_name,
            remote_name.as_deref().unwrap_or(""),
            url.as_deref(),
            ws,
        );
        let label = format!("{} / {}", o.repo_name, remote_name.as_deref().unwrap_or("local"));
        self.state.touch_recent(location, &label, crate::util::unix_now());
        self.state.active = Some(id.clone());
        self.dirty = true;
        self.ensure_live(ctx, &id);
        if let Some(cmd) = o.run {
            self.write_to_focused(format!("{cmd}\r").into_bytes());
        }
        self.focus = Focus::Terminal;
    }

    fn next_pane_id(&self) -> PaneId {
        let used = self
            .state
            .workspaces()
            .into_iter()
            .flat_map(|w| w.tabs.iter())
            .flat_map(|t| t.panes.iter().map(|p| p.id));
        used.max().unwrap_or(0) + 1
    }

    /// Create the runtime (terminals + git pane) for a persisted workspace, once.
    pub(super) fn ensure_live(&mut self, ctx: &egui::Context, id: &str) {
        if self.live.contains_key(id) {
            return;
        }
        let Some(ws) = self.state.workspace(id).cloned() else { return };
        let repo_id = self
            .state
            .repos
            .iter()
            .find(|r| r.remotes.iter().any(|g| g.workspaces.iter().any(|w| w.id == id)))
            .map(|r| r.id.clone());
        let git = match (&ws.location, repo_id) {
            (Location::Local { .. }, Some(repo_id)) => {
                let mut pane = GitPane::new(ws.location.clone(), self.dirs.journal(&repo_id), ctx);
                pane.set_view(ws.git_view);
                Some(pane)
            }
            _ => None,
        };
        let mut live = Live { panes: HashMap::new(), trackers: HashMap::new(), git, last_message: None };
        for tab in &ws.tabs {
            for pane in &tab.panes {
                match self.spawn_terminal(ctx, &ws, &tab.id, pane) {
                    Ok(term) => {
                        live.panes.insert(pane.id, term);
                    }
                    Err(e) => self.toast(Toast::error("Could not start a terminal", e)),
                }
            }
        }
        self.live.insert(id.to_string(), live);
    }

    fn spawn_terminal(
        &self,
        ctx: &egui::Context,
        ws: &Workspace,
        tab: &str,
        pane: &Pane,
    ) -> Result<TerminalPane, String> {
        let Location::Local { path: root } = &ws.location else {
            return Err("remote terminals are not available in this build yet".into());
        };
        let cwd = pane.cwd.as_ref().map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or_else(|| root.clone());
        let env = vec![
            ("AMALGUM_SOCK".into(), self.dirs.socket().display().to_string()),
            ("AMALGUM_WORKSPACE".into(), ws.id.clone()),
            ("AMALGUM_TAB".into(), pane_key(tab, pane.id)),
        ];
        let program = self.settings.terminal.shell.clone().map(|s| (s, vec!["-l".to_string()]));
        let spec = Spawn { cwd, env, program, scrollback: self.settings.terminal.scrollback as usize };
        TerminalPane::spawn(spec, ctx).map_err(|e| e.to_string())
    }

    /// `Mod+T`: a new tab in the active workspace (§5.27).
    pub(super) fn new_tab(&mut self, ctx: &egui::Context) {
        let Some(id) = self.state.active.clone() else { return };
        let tab_id = self.state.new_id("t");
        let pane_id = self.next_pane_id();
        let pane = Pane { id: pane_id, cwd: self.focused_cwd(), title: None, agent: None };
        let Some(ws) = self.state.workspace_mut(&id) else { return };
        ws.tabs.push(Tab {
            id: tab_id.clone(),
            name: None,
            layout: Tree::Leaf(pane_id),
            panes: vec![pane.clone()],
            focused: pane_id,
        });
        ws.active_tab = ws.tabs.len() - 1;
        let ws = ws.clone();
        self.dirty = true;
        self.add_terminal(ctx, &ws, &tab_id, &pane);
    }

    /// `Mod+D` / `Mod+Shift+D`: split the focused pane (§5.27).
    pub(super) fn split(&mut self, ctx: &egui::Context, axis: Axis) {
        let Some(id) = self.state.active.clone() else { return };
        let pane_id = self.next_pane_id();
        let pane = Pane { id: pane_id, cwd: self.focused_cwd(), title: None, agent: None };
        let Some(ws) = self.state.workspace_mut(&id) else { return };
        let Some(tab) = ws.tabs.get_mut(ws.active_tab) else { return };
        if !tab.layout.split(tab.focused, axis, pane_id) {
            return;
        }
        tab.panes.push(pane.clone());
        tab.focused = pane_id;
        let tab_id = tab.id.clone();
        let ws = ws.clone();
        self.dirty = true;
        self.add_terminal(ctx, &ws, &tab_id, &pane);
    }

    fn add_terminal(&mut self, ctx: &egui::Context, ws: &Workspace, tab_id: &str, pane: &Pane) {
        match self.spawn_terminal(ctx, ws, tab_id, pane) {
            Ok(term) => {
                if let Some(live) = self.live.get_mut(&ws.id) {
                    live.panes.insert(pane.id, term);
                }
                self.focus = super::Focus::Terminal;
            }
            Err(e) => self.toast(Toast::error("Could not start a terminal", e)),
        }
    }

    /// `Mod+W`: close the focused pane, the tab when it was the last pane (§5.27).
    pub(super) fn close_pane(&mut self) {
        let Some(id) = self.state.active.clone() else { return };
        let Some(ws) = self.state.workspace_mut(&id) else { return };
        let Some(tab) = ws.tabs.get_mut(ws.active_tab) else { return };
        let target = tab.focused;
        let key = pane_key(&tab.id, target);
        match tab.layout.close(target) {
            Closed::NotFound => return,
            Closed::Focus(next) => {
                tab.panes.retain(|p| p.id != target);
                tab.focused = next;
            }
            Closed::LastPane => {
                ws.tabs.remove(ws.active_tab);
                ws.active_tab = ws.active_tab.min(ws.tabs.len().saturating_sub(1));
            }
        }
        self.dirty = true;
        if let Some(live) = self.live.get_mut(&id) {
            live.panes.remove(&target);
            live.trackers.remove(&key);
        }
    }

    pub(super) fn close_workspace(&mut self, id: &str) {
        self.live.remove(id);
        self.state.remove_workspace(id);
        self.dirty = true;
    }

    fn focused_cwd(&self) -> Option<String> {
        let ws = self.state.workspace(self.state.active.as_deref()?)?;
        let tab = ws.tabs.get(ws.active_tab)?;
        let term = self.live.get(&ws.id)?.panes.get(&tab.focused)?;
        term.cwd().map(str::to_string)
    }

    /// Type into the focused terminal (without Enter unless `bytes` contains it).
    pub(super) fn write_to_focused(&mut self, bytes: Vec<u8>) {
        let Some(ws) = self.state.active.as_deref().and_then(|id| self.state.workspace(id)) else { return };
        let Some(tab) = ws.tabs.get(ws.active_tab) else { return };
        if let Some(term) = self.live.get(&ws.id).and_then(|l| l.panes.get(&tab.focused)) {
            term.write(bytes);
        }
    }
}

fn same_location(a: &Location, b: &Location) -> bool {
    match (a, b) {
        (Location::Local { path: pa }, Location::Local { path: pb }) => {
            let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            canon(pa) == canon(pb)
        }
        _ => a == b,
    }
}
