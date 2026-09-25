//! Per-tab agent status (§5.29 "Status model" and "Detection sources").
//!
//! Sources, most confident first: hook events > OSC 9/99/777 or BEL > OSC 133 shell
//! integration > process heuristic. A more (or equally) confident source always wins; a less
//! confident one may override only after the current observation is older than
//! [`STALE_AFTER_SECS`]. A tab's status rolls up to its sidebar row as the most urgent one.

use serde::{Deserialize, Serialize};

/// Ordered by urgency: `NeedsInput` is the maximum, so `max()` picks the most urgent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// No agent and no foreground command: draw no dot.
    #[default]
    None,
    /// Remote tab whose connection is down. `⊘`, danger.
    Disconnected,
    /// `◌`, status.hibernated.
    Hibernated,
    /// `○`, status.idle.
    Idle,
    /// `●`, success.
    Running,
    /// `◐`, accent, ring on the pane.
    NeedsInput,
}

impl Status {
    /// Status dot glyph; shape carries meaning as well as color (§4, §8). `""` for `None`.
    pub fn glyph(self) -> &'static str {
        todo!()
    }
}

/// Where an observation came from, ordered by confidence (`Hook` is the maximum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Source {
    Heuristic,
    ShellIntegration,
    Osc,
    Hook,
}

pub const STALE_AFTER_SECS: u64 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub status: Status,
    pub source: Source,
    /// The agent's own words ("Approve edit to src/db.rs?"), never a generic placeholder.
    pub message: Option<String>,
    /// Unix seconds.
    pub at: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Tracker {
    current: Option<Observation>,
}

impl Tracker {
    /// Apply the arbitration rule. Returns true if the visible status or message changed.
    pub fn observe(&mut self, obs: Observation) -> bool {
        todo!()
    }
    pub fn status(&self) -> Status {
        todo!()
    }
    pub fn message(&self) -> Option<&str> {
        todo!()
    }
    /// Heuristic-only status is drawn with a hollow dot.
    pub fn low_confidence(&self) -> bool {
        todo!()
    }
    pub fn clear(&mut self) {
        todo!()
    }
}

/// The most urgent status among a workspace's tabs (`None` for no tabs).
pub fn rollup(statuses: impl IntoIterator<Item = Status>) -> Status {
    todo!()
}
