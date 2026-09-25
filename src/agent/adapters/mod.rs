//! One file per agent (§12 risk 3: "one adapter file per agent, versioned"). Everything that
//! depends on an agent's hook schema, config format, or CLI flags lives in that agent's file:
//!
//! - `adapt(raw)`: hook payload → [`AgentEvent`] (or `None` for events we ignore)
//! - `config_path(home)`: the agent's config file we merge into
//! - `install(text, command)`: pure text transform adding our marker-keyed hook block
//!   (idempotent: re-running replaces the block in place; the user's other content survives)
//! - `uninstall(text)`: pure text transform removing exactly our block
//! - `state(text, command)`: [`HookState`] for `amalgum hooks status`
//! - `resume(session_id)`: the shell command that resumes a session (§5.30)
//!
//! This module does the file I/O around those pure functions: read (missing file = empty),
//! write atomically (temp file + rename, preserving permissions), create parent dirs.
//! Tests must pass a temp `home`; nothing here may touch the real `$HOME` under test.

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;

use super::AgentKind;
use super::status::Status;
use std::io;
use std::path::{Path, PathBuf};

/// Bump when the installed hook block changes; older blocks then report `Outdated`.
pub const HOOK_VERSION: u32 = 1;

/// Hook events in agent-neutral terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    SessionStart,
    PromptSubmit,
    Notification,
    PermissionRequest,
    Stop,
    SessionEnd,
}

/// What one hook payload means for its tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEvent {
    pub agent: AgentKind,
    pub kind: EventKind,
    /// `Status::None` for `SessionEnd` (no agent any more).
    pub status: Status,
    /// The agent's own text, for the sidebar line and notification row.
    pub message: Option<String>,
    pub session_id: Option<String>,
    pub pid: Option<u32>,
    pub cwd: Option<String>,
}

impl AgentEvent {
    /// Whether this event creates a notification row (§5.29: needs-input transitions and
    /// every hook `Notification`).
    pub fn notifies(&self) -> bool {
        todo!()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookState {
    Installed,
    /// Our block is present but differs from what `install` would write now.
    Outdated,
    /// The agent's config exists (or the agent is on this machine) but has no block of ours.
    Missing,
    /// Something else already occupies the slot we need (e.g. Codex `notify`); left untouched.
    Conflict(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReport {
    pub agent: AgentKind,
    pub state: HookState,
    pub path: PathBuf,
}

/// Map one raw hook payload. Must not panic on any JSON value.
pub fn adapt(agent: AgentKind, raw: &serde_json::Value) -> Option<AgentEvent> {
    todo!()
}

/// The command each hook runs: `<exe> agent-event --agent <name> --hook-version <N>`.
/// `exe` is shell-quoted if it contains anything but `[A-Za-z0-9_./~-]`.
pub fn hook_command(exe: &str, agent: AgentKind) -> String {
    todo!()
}

/// Install or update our hook block for `agent` under `home`.
pub fn setup(agent: AgentKind, home: &Path, exe: &str) -> io::Result<HookReport> {
    todo!()
}

pub fn status(agent: AgentKind, home: &Path, exe: &str) -> io::Result<HookReport> {
    todo!()
}

/// Remove our block; leaves the rest of the file byte-for-byte intact.
pub fn remove(agent: AgentKind, home: &Path) -> io::Result<HookReport> {
    todo!()
}

/// Shell command that resumes `session_id` (§5.30), e.g. `claude --resume <id>`.
pub fn resume_command(agent: AgentKind, session_id: &str) -> String {
    todo!()
}
