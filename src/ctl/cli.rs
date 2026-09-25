//! The `amalgum` command line (§5.31). The same binary is the app (default features) and the
//! headless remote CLI (`--no-default-features`). `main.rs` parses [`Cli`]; with a subcommand
//! it calls [`run`], otherwise it opens `path` (default `.`) as a workspace in the app.
//!
//! Exit codes: 0 success; 1 query-type command with no app ("Amalgum is not running") or an
//! error response; 2 usage error (clap). Notification-type commands (`notify`, `set-status`,
//! `clear-status`, `agent-event`) always exit 0 so a missing app never blocks an agent: with
//! no app they are appended to the offline queue when `queue::should_queue`, else dropped.

use super::protocol::{SplitDir, StatusArg};
use crate::agent::AgentKind;
use clap::{Parser, Subcommand, ValueEnum};
use std::io::{Read, Write};
use std::path::PathBuf;

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
        todo!()
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
        todo!()
    }
}

/// Execute one subcommand. `stdin` is read only by `agent-event`; human output goes to `out`,
/// diagnostics to stderr. Returns the process exit code.
pub fn run(cmd: Cmd, env: &Env, stdin: &mut dyn Read, out: &mut dyn Write) -> i32 {
    todo!()
}

/// Handle `--sequence-editor` / `--editor` if present (§5.16); `None` if neither flag is set.
/// Plans and messages come from files named by `AMALGUM_REBASE_PLAN` / `AMALGUM_REBASE_MSGS`
/// (see `git::rebase`).
pub fn run_rebase_helper(cli: &Cli) -> Option<i32> {
    todo!()
}
