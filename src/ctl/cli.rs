//! The `amalgum` command line (§5.31). The same binary is the app (default features) and the
//! headless remote CLI (`--no-default-features`). `main.rs` parses [`Cli`]; with a subcommand
//! it calls [`run`], otherwise it opens `path` (default `.`) as a workspace in the app.
//!
//! Exit codes: 0 success; 1 query-type command with no app ("Amalgum is not running") or an
//! error response; 2 usage error (clap). Notification-type commands (`notify`, `set-status`,
//! `clear-status`, `agent-event`) always exit 0 so a missing app never blocks an agent: with
//! no app they are appended to the offline queue when `queue::should_queue`, else dropped.

use super::protocol::{self, SplitDir, StatusArg};
use super::{queue, socket};
use crate::agent::AgentKind;
use crate::paths;
use clap::{Parser, Subcommand, ValueEnum};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    Run {
        #[arg(long)]
        split: Option<SplitDir>,
        #[arg(long)]
        tab: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(trailing_var_arg = true, required = true)]
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
    /// How hooks should invoke this binary (the current executable path).
    pub exe: String,
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
            exe: std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(str::to_string))
                .unwrap_or_else(|| "amalgum".to_string()),
        }
    }
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
                command: command.join(" "),
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
    match socket::send(&sock, &request, Duration::from_secs(2)) {
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
            (if cmd.is_notification() { 0 } else { 1 }, None)
        }
    }
}

fn resolve_sock(env: &Env) -> Option<PathBuf> {
    env.sock.clone().or_else(|| env.default_sock.clone())
}

/// `ConnectionRefused`/`NotFound` (or no socket path at all, folded in by the caller) mean no
/// app is running.
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
        Ok(event) => {
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

/// Resolve a relative local path against `cwd` (the app's cwd differs); `host:path` and
/// absolute paths pass through.
fn resolve_open_location(location: &str, cwd: &Path) -> String {
    match crate::git::Location::parse(location) {
        crate::git::Location::Local { path } if path.is_relative() => cwd.join(path).to_string_lossy().into_owned(),
        _ => location.to_string(),
    }
}

/// `list`'s expected `data` shape (produced by the running app; the CLI only renders it):
/// ```json
/// {"workspaces": [{"id": "…", "name": "…", "location": "…", "status": "…",
///   "tabs": [{"id": "…", "title": "…", "status": "…", "cwd": "…", "ports": [8080]}]}]}
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
        let _ = writeln!(out, "{name}\t{location}\t{status}");
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
        }
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
