//! Opening locations (§5.2, §5.22) and the terminal/git runtime of each workspace (§5.27).

use super::{App, Focus, Live, Msg};
use crate::git::refs::{REMOTE_ARGS, Remote, parse_remotes};
use crate::git::{Git, Location, remote};
use crate::model::layout::{Axis, Closed, PaneId, Tree};
use crate::model::state::{GitView, Pane, Tab, Workspace};
use crate::ssh::terminal::TabSpawn;
use crate::ui::chrome::Toast;
use crate::ui::git_pane::GitPane;
use crate::ui::jobs;
use crate::ui::terminal::{Spawn, TerminalPane};
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

/// Which repo group a repository belongs to (§5.2, §5.28 step 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Identity {
    pub repo_id: String,
    pub repo_name: String,
    /// The primary remote (`origin`, else the first): name and fetch URL.
    pub remote: Option<(String, String)>,
}

/// Identify a repo from its remotes: the primary remote's normalised URL, so local and remote
/// clones of one repo nest together; without remotes, its location (`root`) and `dir_name`.
pub(super) fn identity(remotes: &[Remote], root: &str, dir_name: Option<String>) -> Identity {
    let primary = remotes.iter().find(|r| r.name == "origin").or(remotes.first());
    let repo_name = primary
        .and_then(|r| remote::normalize(&r.fetch_url))
        .and_then(|n| n.rsplit('/').next().map(str::to_string))
        .or(dir_name)
        .unwrap_or_else(|| root.to_string());
    Identity {
        repo_id: remote::repo_id(primary.map(|r| r.fetch_url.as_str()), root),
        repo_name,
        remote: primary.map(|r| (r.name.clone(), r.fetch_url.clone())),
    }
}

/// Worker: find the repo root, its primary remote, and the current branch (§5.2 steps 1–3).
fn resolve(path: PathBuf, name: Option<String>, run: Option<String>) -> Result<Opened, String> {
    let top = Git::new(Location::Local { path: path.clone() })
        .run(&["rev-parse", "--show-toplevel"])
        .map_err(|e| format!("{}: {}\n{}", e.summary(), path.display(), e.stderr))?;
    let root = PathBuf::from(String::from_utf8_lossy(&top).trim());
    let git = Git::new(Location::Local { path: root.clone() });
    let remotes =
        git.run(REMOTE_ARGS).map(|o| parse_remotes(&String::from_utf8_lossy(&o))).unwrap_or_default();
    let branch = git
        .run(&["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .filter(|b| !b.is_empty());
    let dir_name = root.file_name().map(|n| n.to_string_lossy().into_owned());
    let Identity { repo_id, repo_name, remote } = identity(&remotes, &root.display().to_string(), dir_name);
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
            let id = existing.id.clone();
            return self.activate(ctx, &id);
        }
        match loc {
            Location::Local { path } => {
                jobs::spawn(ctx, &self.tx, move || Msg::Opened(resolve(path, name, run)))
            }
            remote => {
                let keep = self.settings.ssh.tmux_default;
                self.open_remote(ctx, remote, name, run, keep);
            }
        }
    }

    /// A new workspace's persisted state: one tab with one terminal.
    pub(super) fn new_workspace_state(&mut self, location: Location, name: String) -> Workspace {
        let id = self.state.new_id("w");
        let tab_id = self.state.new_id("t");
        let pane_id = self.next_pane_id();
        Workspace {
            name,
            id,
            location,
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
            keep_sessions: true,
        }
    }

    pub(super) fn on_opened(&mut self, ctx: &egui::Context, o: Opened) {
        let location = Location::Local { path: o.root.clone() };
        if let Some(existing) = self.state.workspaces().iter().find(|w| w.location == location) {
            self.state.active = Some(existing.id.clone());
            self.dirty = true;
            return;
        }
        let name = o.name.clone().or(o.branch.clone()).unwrap_or_else(|| o.repo_name.clone());
        let ws = self.new_workspace_state(location.clone(), name);
        let id = ws.id.clone();
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
        if let Location::Remote { host, path } = &ws.location {
            let live = self.remote_live(id, host, path);
            self.live.insert(id.to_string(), live);
            // A host another workspace already connected: start the terminals now.
            if let Some(epoch) = self.remote.ssh.machine(host).filter(|m| m.is_connected()).map(|m| m.epoch())
            {
                self.spawn_remote_terminals(ctx, id, epoch);
            }
            return;
        }
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
        let mut live = Live { git, ..Live::default() };
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

    pub(super) fn spawn_terminal(
        &self,
        ctx: &egui::Context,
        ws: &Workspace,
        tab: &str,
        pane: &Pane,
    ) -> Result<TerminalPane, String> {
        let root = match &ws.location {
            Location::Local { path } => path,
            // §5.28 "Terminal spawn": ssh through the host's master, into tmux when asked for.
            Location::Remote { host, path } => {
                let (key, cwd) = (pane_key(tab, pane.id), pane.cwd.clone().unwrap_or_else(|| path.clone()));
                let spawn = TabSpawn { workspace_id: &ws.id, tab_id: &key, cwd: &cwd };
                let argv = self
                    .remote
                    .ssh
                    .terminal_argv_with(host, &spawn, ws.keep_sessions)
                    .ok_or_else(|| format!("{host} is not connected"))?;
                let (program, args) = argv.split_first().ok_or("empty ssh command")?;
                let spec = Spawn {
                    cwd: crate::paths::home().unwrap_or_else(std::env::temp_dir),
                    env: Vec::new(),
                    program: Some((program.clone(), args.to_vec())),
                    scrollback: self.settings.terminal.scrollback as usize,
                };
                return TerminalPane::spawn(spec, ctx).map_err(|e| e.to_string());
            }
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
        if self.remote_down(&ws.id) {
            return; // it starts with the others once the host is back (`remote.rs`)
        }
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
        // A remote pane's tmux session would live on, detached, on the host.
        if let Some(host) = self.live.get(&id).and_then(|l| l.remote.as_ref()).map(|r| r.host.clone()) {
            let tmux = self.state.workspace(&id).is_some_and(|w| w.keep_sessions);
            self.remote.ssh.end_session(&host, &key, tmux);
        }
    }

    /// `Mod+Shift+W` / row menu **Close**; a remote workspace in tmux asks keep or kill first.
    pub(super) fn close_workspace(&mut self, id: &str) {
        if !self.close_remote_or_ask(id) {
            self.finish_close(id);
        }
    }

    pub(super) fn finish_close(&mut self, id: &str) {
        self.drop_live(id);
        self.state.remove_workspace(id);
        self.dirty = true;
    }

    /// Stop a workspace's runtime; a remote one lets go of its host and forwards.
    pub(super) fn drop_live(&mut self, id: &str) {
        self.release_remote(id);
        self.live.remove(id);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(name: &str, url: &str) -> Remote {
        Remote { name: name.into(), fetch_url: url.into(), push_url: url.into() }
    }

    #[test]
    fn local_and_remote_clones_of_one_repo_share_an_identity() {
        let url = "https://github.com/acme/conduit.git";
        let local = identity(&[remote("origin", url)], "/home/a/w/conduit", Some("conduit".into())); // portability: allow
        let over_ssh = identity(&[remote("origin", url)], "gpu-box:~/conduit-2", Some("conduit-2".into()));
        assert_eq!(local, over_ssh);
        assert_eq!(local.repo_name, "conduit");
        assert_eq!(local.remote, Some(("origin".into(), url.into())));
    }

    #[test]
    fn origin_wins_and_no_remote_falls_back_to_the_location() {
        let id = identity(
            &[remote("fork", "https://x.org/me/a.git"), remote("origin", "https://x.org/u/b.git")],
            "r",
            None,
        );
        assert_eq!(id.repo_name, "b");
        let bare = identity(&[], "gpu-box:~/scratch", Some("scratch".into()));
        assert_eq!((bare.repo_name.as_str(), bare.remote), ("scratch", None));
        assert_eq!(bare.repo_id, remote::repo_id(None, "gpu-box:~/scratch"), "the placeholder group's id");
    }
}
