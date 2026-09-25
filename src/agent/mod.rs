//! Agent awareness (§5.27–5.30): which tab needs you and why.
//!
//! - [`status`]        per-tab status model and confidence arbitration between sources
//! - [`adapters`]      one file per agent: hook payload → [`adapters::AgentEvent`], hook installer, resume command
//! - [`osc`]           streaming scanner for OSC 7/0/2/9/99/777/133 and BEL in PTY output
//! - [`hibernate`]     eligibility rules for freeing idle agents (§5.30)
//! - [`notifications`] the notification panel's store (§5.29)

pub mod adapters;
pub mod hibernate;
pub mod notifications;
pub mod osc;
pub mod status;

use serde::{Deserialize, Serialize};

/// Agents with hook integrations. Serialized lowercase (`"opencode"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
    Opencode,
    Gemini,
}

impl AgentKind {
    pub const ALL: [Self; 4] = [Self::Claude, Self::Codex, Self::Opencode, Self::Gemini];

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Gemini => "gemini",
        }
    }
}

/// Foreground process names treated as agents by the low-confidence heuristic (§5.29 source 4).
pub const KNOWN_AGENT_BINARIES: &[&str] = &["claude", "codex", "opencode", "gemini", "aider", "kiro"];
