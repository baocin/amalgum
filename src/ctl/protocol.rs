//! Wire protocol between the CLI (and agent hooks) and the running app (§5.31).
//!
//! One connection per request: one line of JSON in, one line of JSON out, no ids, no auth
//! beyond socket permissions. Every request carries `v` (protocol version) and `ts` (Unix
//! seconds when the event happened, so queued remote events keep their real time).
//!
//! Wire examples (these exact strings are test vectors):
//! ```text
//! → {"v":1,"ts":1700000000,"cmd":"notify","title":"Done","body":"12 tests pass","workspace":"w1","tab":"t1"}
//! ← {"ok":true}
//! ← {"ok":false,"error":"unknown tab t9"}
//! ```
//! Optional fields are omitted when `None`. Unknown fields are ignored (forward compatible).
//! The app accepts any request with `v <= VERSION` and rejects newer ones with an error
//! response naming both versions.

use crate::agent::AgentKind;
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;

/// Status a script may set explicitly with `amalgum set-status` (§5.29 "Explicit status").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum StatusArg {
    Running,
    NeedsInput,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SplitDir {
    Right,
    Down,
}

/// One control command. Serialized with `"cmd": "<kebab-case variant>"` beside the fields.
/// `workspace`/`tab` default from `AMALGUM_WORKSPACE`/`AMALGUM_TAB` in the CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Command {
    Notify { title: String, body: Option<String>, workspace: Option<String>, tab: Option<String> },
    SetStatus { status: StatusArg, message: Option<String>, workspace: Option<String>, tab: Option<String> },
    ClearStatus { workspace: Option<String>, tab: Option<String> },
    /// Raw hook payload; the app's per-agent adapter (`agent::adapters`) interprets it.
    AgentEvent { agent: AgentKind, event: serde_json::Value, workspace: Option<String>, tab: Option<String> },
    Open { location: String, remote: Option<String>, name: Option<String>, run: Option<String> },
    Run { command: String, split: Option<SplitDir>, workspace: Option<String>, tab: Option<String> },
    List,
    Focus { target: String },
    Resume { tab: String },
    Hibernate { tab: String },
}

impl Command {
    /// Notification-type commands never fail loudly: with no app they exit 0 (and are queued
    /// on remote hosts). Everything else is query-type and exits 1 "Amalgum is not running".
    pub fn is_notification(&self) -> bool {
        todo!()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub ts: u64,
    #[serde(flatten)]
    pub cmd: Command,
}

impl Request {
    /// Stamp `cmd` with the current protocol version and time.
    pub fn new(cmd: Command) -> Self {
        todo!()
    }
    /// JSON plus a trailing `\n`.
    pub fn to_line(&self) -> String {
        todo!()
    }
    /// Parse one line (trailing newline optional). Rejects `v > VERSION`.
    pub fn from_line(line: &str) -> Result<Self, String> {
        todo!()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn ok() -> Self {
        todo!()
    }
    pub fn with_data(data: serde_json::Value) -> Self {
        todo!()
    }
    pub fn err(msg: impl Into<String>) -> Self {
        todo!()
    }
    pub fn to_line(&self) -> String {
        todo!()
    }
    pub fn from_line(line: &str) -> Result<Self, String> {
        todo!()
    }
}
