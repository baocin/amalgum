//! The `amalgum` command line (§5.31). The same binary is the app (default features) and the
//! headless remote CLI (`--no-default-features`). `main.rs` parses [`Cli`]; with a subcommand
//! it calls [`run`], otherwise it opens `path` (default `.`) as a workspace in the app.
//!
//! Exit codes: 0 success; 1 query-type command with no app ("Amalgum is not running") or an
//! error response; 2 usage error (clap). Notification-type commands (`notify`, `set-status`,
//! `clear-status`, `agent-event`) always exit 0 so a missing app never blocks an agent: with
//! no app, or no answer within `NOTIFY_TIMEOUT`, they are appended to the offline queue when
//! `queue::should_queue`, else dropped.

use super::protocol::{self, SplitDir, StatusArg};
use super::{queue, socket};
use crate::agent::AgentKind;
use crate::paths;
use clap::{Parser, Subcommand, ValueEnum};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a notification-type command waits for the app's answer. Hooks run it under a 1 s
/// timeout (§5.29), so it must give up, and queue, well before the agent kills it.
const NOTIFY_TIMEOUT: Duration = Duration::from_millis(500);
/// How long a query-type command waits for the app's answer.
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Parser)]
#[command(name = "amalgum", version, about = "Workspace shell for coding agents, with git built in")]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
    /// Folder or `host:path` to open as a workspace (default: current directory).
    pub path: Option<String>,
    /// Internal: `GIT_SEQUENCE_EDITOR` helper for interactive rebase (§5.16).
    #[arg(long, hide = true, value_name = "TODO_FILE")]
    pub sequence_editor: Option<PathBuf>,
    /// Internal: `GIT_EDITOR` helper supplying reword/squash messages (§5.16).
    #[arg(long, hide = true, value_name = "MSG_FILE")]
    pub editor: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Post a notification to the app.
    Notify {
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        tab: Option<String>,
    },
    /// Set this tab's agent status explicitly.
    SetStatus {
        status: StatusArg,
        #[arg(long)]
        message: Option<String>,
    },
    /// Clear an explicitly set status.
    ClearStatus,
    /// Forward an agent hook event (JSON on stdin, or as the last argument for Codex).
    AgentEvent {
        #[arg(long)]
        agent: AgentKind,
        /// Version of the installed hook block; lets `hooks status` detect drift.
        #[arg(long)]
        hook_version: Option<u32>,
        json: Option<String>,
    },
    /// Open or focus a workspace for a folder or `host:path`.
    Open {
        location: String,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        run: Option<String>,
    },
    /// Open a new terminal running a command.
    ///
    /// One argument is a shell command line (`run "npm start"`); several are the command's
    /// words, each kept intact (`run -- git commit -m "fix the bug"`: `--` lets words that start
    /// with `-` through). Flags may come before or after the command.
    Run {
        #[arg(long)]
        split: Option<SplitDir>,
        #[arg(long)]
        tab: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(required = true)]
        command: Vec<String>,
    },
    /// List workspaces, tabs, statuses, cwds, and ports.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Focus a workspace or tab.
    Focus { target: String },
    /// Resume a hibernated agent tab.
    Resume { tab: String },
    /// Hibernate an idle agent tab.
    Hibernate { tab: String },
    /// Install, inspect, or remove agent hooks.
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// Print and clear the offline event queue (run by the app on remote hosts).
    Drain,
}

#[derive(Debug, Subcommand)]
pub enum HooksAction {
    Setup { agent: Option<HookTarget> },
    Status { agent: Option<HookTarget> },
    Remove { agent: Option<HookTarget> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookTarget {
    Claude,
    Codex,
    Opencode,
    Gemini,
    All,
}

impl HookTarget {
    /// `None` and `All` mean every agent.
    pub fn agents(target: Option<Self>) -> Vec<AgentKind> {
        match target {
            None | Some(HookTarget::All) => AgentKind::ALL.to_vec(),
            Some(HookTarget::Claude) => vec![AgentKind::Claude],
            Some(HookTarget::Codex) => vec![AgentKind::Codex],
            Some(HookTarget::Opencode) => vec![AgentKind::Opencode],
            Some(HookTarget::Gemini) => vec![AgentKind::Gemini],
        }
    }
}

/// Process environment, captured once so [`run`] is testable without touching globals.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// `AMALGUM_SOCK`, set in shells the app spawns (reverse-forwarded on remote hosts).
    pub sock: Option<PathBuf>,
    /// Fallback socket when `AMALGUM_SOCK` is unset: `Dirs::discover()?.socket()`.
    pub default_sock: Option<PathBuf>,
    /// `AMALGUM_WORKSPACE` / `AMALGUM_TAB`.
    pub workspace: Option<String>,
    pub tab: Option<String>,
    pub home: Option<PathBuf>,
    pub cwd: PathBuf,
    /// How hooks should invoke this binary: see [`hook_exe`].
    pub exe: String,
    /// This process's parent: for `agent-event`, the agent that ran the hook.
    pub parent_pid: Option<u32>,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            sock: std::env::var_os("AMALGUM_SOCK").map(PathBuf::from),
            default_sock: paths::Dirs::discover().map(|d| d.socket()),
            workspace: std::env::var("AMALGUM_WORKSPACE").ok(),
            tab: std::env::var("AMALGUM_TAB").ok(),
            home: paths::home(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            exe: hook_exe(
                std::env::current_exe().ok(),
                std::env::var_os("APPIMAGE").map(PathBuf::from),
                std::env::var_os("APPDIR").map(PathBuf::from),
            ),
            parent_pid: Some(std::os::unix::process::parent_id()),
        }
    }
}

/// The executable path written into agent hooks. It must outlive this process and not depend on
/// how this binary was launched, since `hooks status` compares it byte for byte:
/// - inside an AppImage, `exe` sits under a per-launch mount (`$APPDIR`) that vanishes on exit,
///   so the AppImage file itself (`$APPIMAGE`) is used;
/// - otherwise `exe` with symlinks resolved (macOS reports the path it was launched by, e.g.
///   the `/usr/local/bin` shim rather than the app bundle);
/// - `amalgum` (found on `$PATH`) when there is no usable path.
fn hook_exe(exe: Option<PathBuf>, appimage: Option<PathBuf>, appdir: Option<PathBuf>) -> String {
    let resolve = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    let exe = exe.map(resolve);
    let path = match (exe, appimage, appdir.map(resolve)) {
        (Some(exe), Some(appimage), Some(appdir)) if exe.starts_with(&appdir) => Some(appimage),
        (exe, ..) => exe,
    };
    path.and_then(|p| p.to_str().map(str::to_string)).unwrap_or_else(|| "amalgum".to_string())
}

/// Execute one subcommand. `stdin` is read only by `agent-event`; human output goes to `out`,
/// diagnostics to stderr. Returns the process exit code.
pub fn run(cmd: Cmd, env: &Env, stdin: &mut dyn Read, out: &mut dyn Write) -> i32 {
    match cmd {
        Cmd::Notify { title, body, tab } => {
            // No `--workspace` flag on this subcommand (§5.31): it always comes from env.
            let command = protocol::Command::Notify {
                title,
                body,
                workspace: env.workspace.clone(),
                tab: tab.or_else(|| env.tab.clone()),
            };
            dispatch(command, env).0
        }
        Cmd::SetStatus { status, message } => {
            let command = protocol::Command::SetStatus {
                status,
                message,
                workspace: env.workspace.clone(),
                tab: env.tab.clone(),
            };
            dispatch(command, env).0
        }
        Cmd::ClearStatus => {
            let command =
                protocol::Command::ClearStatus { workspace: env.workspace.clone(), tab: env.tab.clone() };
            dispatch(command, env).0
        }
        Cmd::AgentEvent { agent, hook_version: _, json } => agent_event(agent, json, env, stdin),
        Cmd::Open { location, remote, name, run } => {
            let location = resolve_open_location(&location, &env.cwd);
            let command = protocol::Command::Open { location, remote, name, run };
            dispatch(command, env).0
        }
        Cmd::Run { split, tab, workspace, command } => {
            let command = protocol::Command::Run {
                command: shell_command_line(&command),
                split,
                workspace: workspace.or_else(|| env.workspace.clone()),
                tab: tab.or_else(|| env.tab.clone()),
            };
            dispatch(command, env).0
        }
        Cmd::List { json } => list(json, env, out),
        Cmd::Focus { target } => dispatch(protocol::Command::Focus { target }, env).0,
        Cmd::Resume { tab } => dispatch(protocol::Command::Resume { tab }, env).0,
        Cmd::Hibernate { tab } => dispatch(protocol::Command::Hibernate { tab }, env).0,
        Cmd::Hooks { action } => handle_hooks(action, env, out),
        Cmd::Drain => drain_cmd(env, out),
    }
}

/// Send `cmd` to the app and turn its outcome into an exit code, per the module doc's exit
/// codes and §5.31 ("notification-type commands always exit 0"). Returns the raw [`Response`]
/// too, for subcommands (`list`) that render its `data`.
fn dispatch(cmd: protocol::Command, env: &Env) -> (i32, Option<protocol::Response>) {
    let Some(sock) = resolve_sock(env) else {
        return (handle_no_app(&cmd, env), None);
    };
    let request = protocol::Request::new(cmd.clone());
    let timeout = if cmd.is_notification() { NOTIFY_TIMEOUT } else { QUERY_TIMEOUT };
    match socket::send(&sock, &request, timeout) {
        Ok(resp) => {
            if resp.ok {
                (0, Some(resp))
            } else {
                if let Some(err) = &resp.error {
                    eprintln!("{err}");
                }
                (if cmd.is_notification() { 0 } else { 1 }, Some(resp))
            }
        }
        Err(e) if is_connect_failure(&e) => (handle_no_app(&cmd, env), None),
        Err(e) => {
            eprintln!("amalgum: {e}");
            // A reverse-forwarded socket accepts even when the app behind it is gone or
            // unreachable (sshd answers the connect itself), so for an event a hang-up or a
            // timeout means "no app" just as much as a refused connect does: queue it rather
            // than lose it. The price is a rare duplicate, if a slow app did handle it.
            if cmd.is_notification() { (handle_no_app(&cmd, env), None) } else { (1, None) }
        }
    }
}

fn resolve_sock(env: &Env) -> Option<PathBuf> {
    env.sock.clone().or_else(|| env.default_sock.clone())
}

/// `ConnectionRefused`/`NotFound` (or no socket path at all, folded in by the caller) mean no
/// app is running. Query-type commands treat every other failure as an error, not "no app".
fn is_connect_failure(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
}

/// What happens when there is no app to talk to: notification-type commands are queued (on a
/// remote host) or silently dropped (locally), always exiting 0; query-type commands report
/// the failure and exit 1.
fn handle_no_app(cmd: &protocol::Command, env: &Env) -> i32 {
    if cmd.is_notification() {
        if let (Some(home), Some(sock)) = (&env.home, resolve_sock(env))
            && queue::should_queue(&sock, home)
        {
            let request = protocol::Request::new(cmd.clone());
            let _ = queue::append(&paths::queue_file(home), &request);
        }
        0
    } else {
        eprintln!("Amalgum is not running");
        1
    }
}

fn agent_event(agent: AgentKind, json: Option<String>, env: &Env, stdin: &mut dyn Read) -> i32 {
    let text = match json {
        Some(s) => s,
        None => {
            let mut buf = String::new();
            let _ = stdin.read_to_string(&mut buf);
            buf
        }
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(mut event) => {
            // No agent's payload carries its pid, but session capture needs it (§5.29): the
            // agent runs this hook (directly, or through a `sh -c` that execs it), so it is
            // this process's parent.
            if let (Some(fields), Some(pid)) = (event.as_object_mut(), env.parent_pid) {
                fields.entry("pid").or_insert(pid.into());
            }
            let command = protocol::Command::AgentEvent {
                agent,
                event,
                workspace: env.workspace.clone(),
                tab: env.tab.clone(),
            };
            dispatch(command, env).0
        }
        Err(e) => {
            // A hook must never block the agent on a malformed payload (invariant §4.8).
            eprintln!("amalgum: invalid agent-event JSON: {e}");
            0
        }
    }
}

/// `run`'s words as the line the app types into a shell: a single word is already a command
/// line; several are quoted one by one so the shell splits them back into the same words.
fn shell_command_line(words: &[String]) -> String {
    match words {
        [line] => line.clone(),
        _ => words.iter().map(|w| crate::ssh::quote(w)).collect::<Vec<_>>().join(" "),
    }
}

/// Resolve a relative local path against `cwd` (the app's cwd differs); `host:path` and
/// absolute paths pass through.
fn resolve_open_location(location: &str, cwd: &Path) -> String {
    match crate::git::Location::parse(location) {
        crate::git::Location::Local { path } if path.is_relative() => {
            cwd.join(path).to_string_lossy().into_owned()
        }
        _ => location.to_string(),
    }
}

/// `list`'s expected `data` shape (produced by the running app; the CLI only renders it):
/// ```json
/// {"workspaces": [{"id": "…", "name": "…", "location": "…", "status": "…", "ports": [8080],
///   "tabs": [{"id": "…", "title": "…", "status": "…", "cwd": "…", "exited": null}]}]}
/// ```
fn list(as_json: bool, env: &Env, out: &mut dyn Write) -> i32 {
    let (code, resp) = dispatch(protocol::Command::List, env);
    if code == 0
        && let Some(resp) = resp
    {
        print_list(resp.data, as_json, out);
    }
    code
}

fn print_list(data: Option<serde_json::Value>, as_json: bool, out: &mut dyn Write) {
    let data = data.unwrap_or(serde_json::Value::Null);
    if as_json {
        if let Ok(s) = serde_json::to_string_pretty(&data) {
            let _ = writeln!(out, "{s}");
        }
        return;
    }
    let Some(workspaces) = data.get("workspaces").and_then(serde_json::Value::as_array) else {
        return;
    };
    for ws in workspaces {
        let name = ws.get("name").and_then(serde_json::Value::as_str).unwrap_or("?");
        let location = ws.get("location").and_then(serde_json::Value::as_str).unwrap_or("");
        let status = ws.get("status").and_then(serde_json::Value::as_str).unwrap_or("");
        let ports: Vec<String> = ws
            .get("ports")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_u64)
            .map(|port| format!(":{port}"))
            .collect();
        if ports.is_empty() {
            let _ = writeln!(out, "{name}\t{location}\t{status}");
        } else {
            let _ = writeln!(out, "{name}\t{location}\t{status}\t{}", ports.join(","));
        }
        let Some(tabs) = ws.get("tabs").and_then(serde_json::Value::as_array) else { continue };
        for tab in tabs {
            let title = tab.get("title").and_then(serde_json::Value::as_str).unwrap_or("?");
            let tstatus = tab.get("status").and_then(serde_json::Value::as_str).unwrap_or("");
            let cwd = tab.get("cwd").and_then(serde_json::Value::as_str).unwrap_or("");
            let _ = writeln!(out, "  {title}\t{tstatus}\t{cwd}");
        }
    }
}

fn hook_config_path(agent: AgentKind, home: &Path) -> PathBuf {
    match agent {
        AgentKind::Claude => crate::agent::adapters::claude::config_path(home),
        AgentKind::Codex => crate::agent::adapters::codex::config_path(home),
        AgentKind::Opencode => crate::agent::adapters::opencode::config_path(home),
        AgentKind::Gemini => crate::agent::adapters::gemini::config_path(home),
    }
}

fn hook_state_label(state: &crate::agent::adapters::HookState) -> String {
    match state {
        crate::agent::adapters::HookState::Installed => "installed".to_string(),
        crate::agent::adapters::HookState::Outdated => "outdated".to_string(),
        crate::agent::adapters::HookState::Missing => "missing".to_string(),
        crate::agent::adapters::HookState::Conflict(what) => format!("conflict ({what})"),
    }
}

/// `hooks setup|status|remove` never touch the socket: they edit the agents' own config files
/// directly, local or (for `hooks setup` at connect time) over ssh by the app itself (§5.29).
fn handle_hooks(action: HooksAction, env: &Env, out: &mut dyn Write) -> i32 {
    let Some(home) = env.home.as_deref() else {
        eprintln!("amalgum: cannot determine home directory");
        return 1;
    };
    let target = match &action {
        HooksAction::Setup { agent } | HooksAction::Status { agent } | HooksAction::Remove { agent } => {
            *agent
        }
    };

    let mut all_ok = true;
    for agent in HookTarget::agents(target) {
        let result = match &action {
            HooksAction::Setup { .. } => {
                // §5.1: list what will be written before writing it.
                let _ =
                    writeln!(out, "{}  will write {}", agent.name(), hook_config_path(agent, home).display());
                crate::agent::adapters::setup(agent, home, &env.exe)
            }
            HooksAction::Status { .. } => crate::agent::adapters::status(agent, home, &env.exe),
            HooksAction::Remove { .. } => crate::agent::adapters::remove(agent, home),
        };
        match result {
            Ok(report) => {
                let _ = writeln!(
                    out,
                    "{}  {}  {}",
                    agent.name(),
                    hook_state_label(&report.state),
                    report.path.display()
                );
            }
            Err(e) => {
                all_ok = false;
                eprintln!("{}: {e}", agent.name());
            }
        }
    }
    if all_ok { 0 } else { 1 }
}

/// Run by the app remotely (over ssh) to collect what queued while disconnected; always
/// exits 0 (§5.28: a queue read failure here must not break reconnect).
fn drain_cmd(env: &Env, out: &mut dyn Write) -> i32 {
    let Some(home) = env.home.as_deref() else {
        eprintln!("amalgum: cannot determine home directory");
        return 0;
    };
    match queue::drain(&paths::queue_file(home)) {
        Ok(lines) => {
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
        }
        Err(e) => eprintln!("amalgum: {e}"),
    }
    0
}

/// Handle `--sequence-editor` / `--editor` if present (§5.16); `None` if neither flag is set.
/// Plans and messages come from files named by `AMALGUM_REBASE_PLAN` / `AMALGUM_REBASE_MSGS`
/// (see `git::rebase`).
pub fn run_rebase_helper(cli: &Cli) -> Option<i32> {
    if let Some(todo_file) = &cli.sequence_editor {
        let Ok(plan_file) = std::env::var("AMALGUM_REBASE_PLAN") else {
            eprintln!("amalgum: AMALGUM_REBASE_PLAN is not set");
            return Some(1);
        };
        return Some(match crate::git::rebase::apply_sequence_editor(Path::new(&plan_file), todo_file) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("amalgum: {e}");
                1
            }
        });
    }
    if let Some(msg_file) = &cli.editor {
        let Ok(messages_file) = std::env::var("AMALGUM_REBASE_MSGS") else {
            eprintln!("amalgum: AMALGUM_REBASE_MSGS is not set");
            return Some(1);
        };
        return Some(match crate::git::rebase::apply_editor(Path::new(&messages_file), msg_file) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("amalgum: {e}");
                1
            }
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctl::protocol::Command;

    fn env(home: Option<&Path>, sock: Option<PathBuf>) -> Env {
        Env {
            sock,
            default_sock: None,
            workspace: None,
            tab: None,
            home: home.map(Path::to_path_buf),
            cwd: PathBuf::from("/cwd"), // portability: allow
            exe: "amalgum".to_string(),
            parent_pid: None,
        }
    }

    /// A fake app socket that records every request and answers `ok`.
    fn recording_server(
        dir: &Path,
    ) -> (socket::ServerHandle, std::sync::Arc<std::sync::Mutex<Vec<Command>>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_bg = std::sync::Arc::clone(&seen);
        let server = socket::serve(&dir.join("app.sock"), move |req| {
            seen_bg.lock().unwrap().push(req.cmd);
            protocol::Response::ok()
        })
        .unwrap();
        (server, seen)
    }

    fn run_cmd(cmd: Cmd, env: &Env) -> (i32, String) {
        let mut stdin: &[u8] = b"";
        let mut out = Vec::new();
        let code = run(cmd, env, &mut stdin, &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    // --- HookTarget::agents ---------------------------------------------------------------

    #[test]
    fn hook_target_agents_none_and_all_mean_every_agent() {
        assert_eq!(HookTarget::agents(None), AgentKind::ALL.to_vec());
        assert_eq!(HookTarget::agents(Some(HookTarget::All)), AgentKind::ALL.to_vec());
    }

    #[test]
    fn hook_target_agents_single() {
        assert_eq!(HookTarget::agents(Some(HookTarget::Claude)), vec![AgentKind::Claude]);
        assert_eq!(HookTarget::agents(Some(HookTarget::Codex)), vec![AgentKind::Codex]);
        assert_eq!(HookTarget::agents(Some(HookTarget::Opencode)), vec![AgentKind::Opencode]);
        assert_eq!(HookTarget::agents(Some(HookTarget::Gemini)), vec![AgentKind::Gemini]);
    }

    // --- open: remote vs local, relative-path resolution -----------------------------------

    #[test]
    fn resolve_open_location_joins_relative_paths_to_cwd() {
        let cwd = Path::new("/work/dir"); // portability: allow
        assert_eq!(resolve_open_location("sub/proj", cwd), "/work/dir/sub/proj"); // portability: allow
    }

    #[test]
    fn resolve_open_location_leaves_absolute_paths_untouched() {
        let cwd = Path::new("/work/dir"); // portability: allow
        assert_eq!(resolve_open_location("/already/absolute", cwd), "/already/absolute"); // portability: allow
    }

    #[test]
    fn resolve_open_location_leaves_host_path_untouched() {
        let cwd = Path::new("/work/dir"); // portability: allow
        assert_eq!(resolve_open_location("gpu-box:~/repo", cwd), "gpu-box:~/repo");
    }

    // --- no app running: notification vs query exit codes ----------------------------------

    #[test]
    fn notify_with_no_app_and_no_queue_exits_0_silently() {
        let dir = tempfile::tempdir().unwrap();
        let e = env(Some(dir.path()), Some(dir.path().join("nobody.sock")));
        let cmd = Cmd::Notify { title: "hi".into(), body: None, tab: None };
        let (code, out) = run_cmd(cmd, &e);
        assert_eq!(code, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn list_with_no_app_exits_1_and_prints_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let e = env(Some(dir.path()), Some(dir.path().join("nobody.sock")));
        let (code, out) = run_cmd(Cmd::List { json: false }, &e);
        assert_eq!(code, 1);
        assert!(out.is_empty(), "no data to render when the app isn't running");
    }

    #[test]
    fn focus_with_no_app_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        let e = env(Some(dir.path()), Some(dir.path().join("nobody.sock")));
        let (code, _) = run_cmd(Cmd::Focus { target: "w1".into() }, &e);
        assert_eq!(code, 1);
    }

    #[test]
    fn no_sock_at_all_is_treated_as_no_app() {
        let dir = tempfile::tempdir().unwrap();
        let e = env(Some(dir.path()), None); // sock and default_sock both None
        assert_eq!(run_cmd(Cmd::Resume { tab: "t1".into() }, &e).0, 1);
        assert_eq!(run_cmd(Cmd::Notify { title: "x".into(), body: None, tab: None }, &e).0, 0);
    }

    // --- offline queueing (§5.28): only when the socket is under home's remote root --------

    #[test]
    fn notify_queues_when_socket_is_under_remote_root() {
        let home = tempfile::tempdir().unwrap();
        let sock = paths::remote_root(home.path()).join("run").join("app.sock");
        let e = env(Some(home.path()), Some(sock));
        let cmd = Cmd::Notify { title: "queued".into(), body: None, tab: None };
        let (code, _) = run_cmd(cmd, &e);
        assert_eq!(code, 0);

        let lines = queue::drain(&paths::queue_file(home.path())).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("queued"));
    }

    /// On a remote host sshd owns the forwarded socket: it accepts every connection and only
    /// then tries to reach the app. With the app gone (the ControlMaster persists after it
    /// quits) it hangs up without an answer, which is "no app" too, not a reason to drop.
    #[test]
    fn notify_queues_when_the_forwarded_socket_hangs_up_without_answering() {
        let home = tempfile::tempdir().unwrap();
        let sock = paths::remote_root(home.path()).join("run").join("app.sock");
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let sshd = std::thread::spawn(move || drop(listener.accept().unwrap()));

        let e = env(Some(home.path()), Some(sock));
        let (code, _) = run_cmd(Cmd::Notify { title: "while away".into(), body: None, tab: None }, &e);
        assert_eq!(code, 0);
        sshd.join().unwrap();

        let lines = queue::drain(&paths::queue_file(home.path())).unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("while away"));
    }

    /// Laptop asleep: sshd accepts and never answers. The CLI must give up and queue inside the
    /// agents' 1 s hook timeout (§5.29), or the agent kills it first and the event is lost.
    #[test]
    fn notify_queues_within_the_hook_budget_when_the_forwarded_socket_never_answers() {
        let home = tempfile::tempdir().unwrap();
        let sock = paths::remote_root(home.path()).join("run").join("app.sock");
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let sshd = std::thread::spawn(move || {
            let conn = listener.accept().unwrap();
            let _ = wait.recv(); // hold the connection open, silent, until the test is done
            drop(conn);
        });

        let e = env(Some(home.path()), Some(sock));
        let started = std::time::Instant::now();
        let (code, _) = run_cmd(Cmd::SetStatus { status: StatusArg::Idle, message: None }, &e);
        let took = started.elapsed();
        drop(done);
        sshd.join().unwrap();

        assert_eq!(code, 0);
        assert!(took < Duration::from_millis(900), "took {took:?}, over the 1 s hook budget");
        assert_eq!(queue::drain(&paths::queue_file(home.path())).unwrap().len(), 1);
    }

    #[test]
    fn query_with_a_silent_forwarded_socket_still_exits_1_and_queues_nothing() {
        let home = tempfile::tempdir().unwrap();
        let sock = paths::remote_root(home.path()).join("run").join("app.sock");
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let sshd = std::thread::spawn(move || drop(listener.accept().unwrap()));

        let e = env(Some(home.path()), Some(sock));
        assert_eq!(run_cmd(Cmd::Focus { target: "w1".into() }, &e).0, 1);
        sshd.join().unwrap();
        assert!(queue::drain(&paths::queue_file(home.path())).unwrap().is_empty());
    }

    #[test]
    fn notify_does_not_queue_when_socket_is_local() {
        let home = tempfile::tempdir().unwrap();
        let sock = home.path().join("run").join("app.sock"); // NOT under <home>/.amalgum
        let e = env(Some(home.path()), Some(sock));
        let (code, _) = run_cmd(Cmd::Notify { title: "dropped".into(), body: None, tab: None }, &e);
        assert_eq!(code, 0);
        assert!(queue::drain(&paths::queue_file(home.path())).unwrap().is_empty());
    }

    // --- agent-event: positional arg vs stdin, invalid JSON -------------------------------

    #[test]
    fn agent_event_reads_json_from_positional_arg() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("app.sock");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_bg = std::sync::Arc::clone(&seen);
        let server = socket::serve(&sock, move |req| {
            seen_bg.lock().unwrap().push(req.cmd);
            protocol::Response::ok()
        })
        .unwrap();

        let e = env(None, Some(server.path().to_path_buf()));
        let cmd = Cmd::AgentEvent {
            agent: AgentKind::Claude,
            hook_version: Some(1),
            json: Some(r#"{"hook_event_name":"Stop"}"#.into()),
        };
        let (code, _) = run_cmd(cmd, &e);
        assert_eq!(code, 0);

        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 1);
        match &got[0] {
            Command::AgentEvent { agent, event, .. } => {
                assert_eq!(*agent, AgentKind::Claude);
                assert_eq!(event["hook_event_name"], "Stop");
            }
            other => panic!("expected AgentEvent, got {other:?}"),
        }
    }

    #[test]
    fn agent_event_reads_json_from_stdin_when_no_arg() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("app.sock");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_bg = std::sync::Arc::clone(&seen);
        let server = socket::serve(&sock, move |req| {
            seen_bg.lock().unwrap().push(req.cmd);
            protocol::Response::ok()
        })
        .unwrap();

        let e = env(None, Some(server.path().to_path_buf()));
        let mut stdin: &[u8] = br#"{"hook_event_name":"SessionStart"}"#;
        let mut out = Vec::new();
        let cmd = Cmd::AgentEvent { agent: AgentKind::Gemini, hook_version: None, json: None };
        let code = run(cmd, &e, &mut stdin, &mut out);
        assert_eq!(code, 0);

        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 1);
        match &got[0] {
            Command::AgentEvent { agent, event, .. } => {
                assert_eq!(*agent, AgentKind::Gemini);
                assert_eq!(event["hook_event_name"], "SessionStart");
            }
            other => panic!("expected AgentEvent, got {other:?}"),
        }
    }

    /// No agent's hook payload carries a pid, yet §5.29 session capture (and §5.30 hibernation)
    /// needs one: the hook's parent is the agent, so the CLI adds it.
    #[test]
    fn agent_event_adds_the_agent_pid_when_the_payload_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let (server, seen) = recording_server(dir.path());
        let mut e = env(None, Some(server.path().to_path_buf()));
        e.parent_pid = Some(4242);

        for (payload, want) in [
            (r#"{"hook_event_name":"SessionStart","session_id":"s1"}"#, 4242),
            (r#"{"hook_event_name":"SessionStart","pid":7}"#, 7),
        ] {
            let cmd =
                Cmd::AgentEvent { agent: AgentKind::Claude, hook_version: None, json: Some(payload.into()) };
            assert_eq!(run_cmd(cmd, &e).0, 0);
            match seen.lock().unwrap().pop() {
                Some(Command::AgentEvent { event, .. }) => assert_eq!(event["pid"], want, "{payload}"),
                other => panic!("expected AgentEvent, got {other:?}"),
            }
        }
    }

    // --- hooks: the command written into agent configs --------------------------------------

    /// Hooks outlive this process, and `hooks status` compares the command byte for byte: the
    /// path must not depend on how this binary happened to be launched.
    #[test]
    fn hook_exe_resolves_a_symlinked_launch_to_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("Amalgum.app").join("amalgum");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, b"binary").unwrap();
        let shim = dir.path().join("amalgum");
        std::os::unix::fs::symlink(&real, &shim).unwrap();

        let want = std::fs::canonicalize(&real).unwrap().to_str().unwrap().to_string();
        assert_eq!(hook_exe(Some(shim), None, None), want);
        assert_eq!(hook_exe(Some(real), None, None), want);
    }

    /// Inside an AppImage the executable lives under a per-launch mount (`$APPDIR`) that is gone
    /// once this process exits; the AppImage file itself (`$APPIMAGE`) is what stays.
    #[test]
    fn hook_exe_inside_an_appimage_is_the_appimage_file() {
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join(".mount_AmalgX");
        let exe = mount.join("usr").join("bin").join("amalgum");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"binary").unwrap();
        let appimage = dir.path().join("Amalgum.AppImage");

        let got = hook_exe(Some(exe.clone()), Some(appimage.clone()), Some(mount));
        assert_eq!(got, appimage.to_str().unwrap());

        // `$APPIMAGE` leaks into every shell the AppImage app spawns; a different binary run
        // from one of them keeps its own path.
        let other = dir.path().join("amalgum");
        std::fs::write(&other, b"binary").unwrap();
        let want = std::fs::canonicalize(&other).unwrap().to_str().unwrap().to_string();
        assert_eq!(hook_exe(Some(other), Some(appimage), Some(dir.path().join(".mount_AmalgX"))), want);
    }

    #[test]
    fn hook_exe_without_a_known_executable_falls_back_to_path_lookup() {
        assert_eq!(hook_exe(None, None, None), "amalgum");
    }

    #[test]
    fn agent_event_invalid_json_exits_0_and_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("app.sock");
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_bg = std::sync::Arc::clone(&count);
        let server = socket::serve(&sock, move |_req| {
            count_bg.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            protocol::Response::ok()
        })
        .unwrap();

        let e = env(None, Some(server.path().to_path_buf()));
        let cmd =
            Cmd::AgentEvent { agent: AgentKind::Codex, hook_version: None, json: Some("not json".into()) };
        let (code, _) = run_cmd(cmd, &e);
        assert_eq!(code, 0);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0, "malformed payload must not be sent");
    }

    // --- dispatch against a fake server: success, and app-reported errors ------------------

    #[test]
    fn ok_response_with_data_is_rendered_by_list() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("app.sock");
        let data = serde_json::json!({"workspaces": [
            {"name": "conduit", "location": "~/w/conduit", "status": "running",
             "tabs": [{"id": "t1", "title": "claude", "status": "needs-input", "cwd": "~/w/conduit", "ports": []}]}
        ]});
        let server = socket::serve(&sock, move |_req| protocol::Response::with_data(data.clone())).unwrap();

        let e = env(None, Some(server.path().to_path_buf()));
        let (code, out) = run_cmd(Cmd::List { json: false }, &e);
        assert_eq!(code, 0);
        assert!(out.contains("conduit"));
        assert!(out.contains("claude"));
        assert!(out.contains("needs-input"));
    }

    /// §5.31: `list` shows ports too. The app reports them per workspace.
    #[test]
    fn list_text_shows_each_workspaces_ports() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("app.sock");
        let data = serde_json::json!({"workspaces": [
            {"name": "conduit", "location": "~/w/conduit", "status": "running", "ports": [3000, 8080],
             "tabs": [{"id": "t1", "title": "vite", "status": "running", "cwd": "~/w/conduit"}]},
            {"name": "quiet", "location": "~/w/quiet", "status": "idle", "ports": null, "tabs": []}
        ]});
        let server = socket::serve(&sock, move |_req| protocol::Response::with_data(data.clone())).unwrap();

        let e = env(None, Some(server.path().to_path_buf()));
        let (code, out) = run_cmd(Cmd::List { json: false }, &e);
        assert_eq!(code, 0);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "conduit\t~/w/conduit\trunning\t:3000,:8080");
        assert_eq!(lines[2], "quiet\t~/w/quiet\tidle");
    }

    #[test]
    fn list_json_flag_prints_pretty_json() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("app.sock");
        let server =
            socket::serve(&sock, |_req| protocol::Response::with_data(serde_json::json!({"workspaces": []})))
                .unwrap();

        let e = env(None, Some(server.path().to_path_buf()));
        let (code, out) = run_cmd(Cmd::List { json: true }, &e);
        assert_eq!(code, 0);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON output");
        assert_eq!(parsed, serde_json::json!({"workspaces": []}));
        assert!(out.contains('\n'), "pretty-printed, not one line");
    }

    #[test]
    fn query_type_error_response_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("app.sock");
        let server = socket::serve(&sock, |_req| protocol::Response::err("unknown tab t9")).unwrap();

        let e = env(None, Some(server.path().to_path_buf()));
        let (code, _) = run_cmd(Cmd::Focus { target: "t9".into() }, &e);
        assert_eq!(code, 1);
    }

    #[test]
    fn notification_type_error_response_still_exits_0() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("app.sock");
        let server = socket::serve(&sock, |_req| protocol::Response::err("boom")).unwrap();

        let e = env(None, Some(server.path().to_path_buf()));
        let cmd = Cmd::Notify { title: "x".into(), body: None, tab: None };
        let (code, _) = run_cmd(cmd, &e);
        assert_eq!(code, 0);
    }

    #[test]
    fn run_command_joins_words_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("app.sock");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen_bg = std::sync::Arc::clone(&seen);
        let server = socket::serve(&sock, move |req| {
            *seen_bg.lock().unwrap() = Some(req.cmd);
            protocol::Response::ok()
        })
        .unwrap();

        let e = env(None, Some(server.path().to_path_buf()));
        let cmd = Cmd::Run {
            split: None,
            tab: None,
            workspace: None,
            command: vec!["cargo".into(), "test".into(), "--lib".into()],
        };
        run_cmd(cmd, &e);
        match seen.lock().unwrap().take() {
            Some(Command::Run { command, .. }) => assert_eq!(command, "cargo test --lib"),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// Several words are an argv: the shell the app types the command into must see exactly
    /// those words, so each is quoted. One word is a shell command line, sent as written.
    #[test]
    fn run_command_keeps_each_word_intact_for_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let (server, seen) = recording_server(dir.path());
        let e = env(None, Some(server.path().to_path_buf()));

        for (words, want) in [
            (vec!["git", "commit", "-m", "fix the bug"], "git commit -m 'fix the bug'"),
            (vec!["pytest", "-k", "a and b"], "pytest -k 'a and b'"),
            (vec!["npm start && open http://localhost:3000"], "npm start && open http://localhost:3000"),
        ] {
            let command = words.iter().map(|w| w.to_string()).collect();
            run_cmd(Cmd::Run { split: None, tab: None, workspace: None, command }, &e);
            match seen.lock().unwrap().pop() {
                Some(Command::Run { command, .. }) => assert_eq!(command, want, "{words:?}"),
                other => panic!("expected Run, got {other:?}"),
            }
        }
    }

    /// §5.31 writes the usage as `run <cmd> [--split right|down] [--tab ID] [--workspace ID]`:
    /// flags after the command are amalgum's, and `--` passes flag-like words to the command.
    #[test]
    fn run_flags_after_the_command_are_parsed_as_flags() {
        let parse = |args: &[&str]| match Cli::try_parse_from(args).expect("parses").command {
            Some(Cmd::Run { split, tab, workspace, command }) => (split, tab, workspace, command),
            other => panic!("expected Run, got {other:?}"),
        };
        let (split, tab, workspace, command) =
            parse(&["amalgum", "run", "npm start", "--split", "right", "--tab", "t1", "--workspace", "w2"]);
        assert_eq!(split, Some(SplitDir::Right));
        assert_eq!(tab.as_deref(), Some("t1"));
        assert_eq!(workspace.as_deref(), Some("w2"));
        assert_eq!(command, ["npm start"]);

        let (split, _, _, command) =
            parse(&["amalgum", "run", "--split", "down", "--", "cargo", "test", "--lib"]);
        assert_eq!(split, Some(SplitDir::Down));
        assert_eq!(command, ["cargo", "test", "--lib"]);
    }

    // --- drain: prints queued lines, doesn't touch the socket ------------------------------

    #[test]
    fn drain_prints_and_clears_the_queue() {
        let home = tempfile::tempdir().unwrap();
        let e = env(Some(home.path()), None);
        queue::append(
            &paths::queue_file(home.path()),
            &protocol::Request::new(Command::Focus { target: "w1".into() }),
        )
        .unwrap();

        let (code, out) = run_cmd(Cmd::Drain, &e);
        assert_eq!(code, 0);
        assert!(out.contains("w1"));
        assert!(queue::drain(&paths::queue_file(home.path())).unwrap().is_empty());
    }

    #[test]
    fn drain_with_no_home_exits_0() {
        let e = env(None, None);
        let (code, _) = run_cmd(Cmd::Drain, &e);
        assert_eq!(code, 0);
    }

    // --- --sequence-editor / --editor: missing env var reports and exits 1 -----------------

    #[test]
    fn run_rebase_helper_is_none_without_either_flag() {
        let cli = Cli { command: None, path: None, sequence_editor: None, editor: None };
        assert_eq!(run_rebase_helper(&cli), None);
    }

    #[test]
    fn run_rebase_helper_sequence_editor_without_env_var_is_an_error() {
        assert!(std::env::var_os("AMALGUM_REBASE_PLAN").is_none(), "test precondition");
        let cli =
            Cli { command: None, path: None, sequence_editor: Some(PathBuf::from("todo")), editor: None };
        assert_eq!(run_rebase_helper(&cli), Some(1));
    }

    #[test]
    fn run_rebase_helper_editor_without_env_var_is_an_error() {
        assert!(std::env::var_os("AMALGUM_REBASE_MSGS").is_none(), "test precondition");
        let cli =
            Cli { command: None, path: None, sequence_editor: None, editor: Some(PathBuf::from("msg")) };
        assert_eq!(run_rebase_helper(&cli), Some(1));
    }
}
