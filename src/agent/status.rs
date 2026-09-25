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
        match self {
            Status::None => "",
            Status::Disconnected => "⊘",
            Status::Hibernated => "◌",
            Status::Idle => "○",
            Status::Running => "●",
            Status::NeedsInput => "◐",
        }
    }

    /// Words for the status bar and tooltips ("needs input").
    pub fn label(self) -> &'static str {
        match self {
            Status::None => "",
            Status::Disconnected => "disconnected",
            Status::Hibernated => "hibernated",
            Status::Idle => "idle",
            Status::Running => "running",
            Status::NeedsInput => "needs input",
        }
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
    /// Apply the arbitration rule: accept `obs` when there is no current observation, when
    /// `obs` is at least as confident as the current source, or when the current observation
    /// has gone stale ([`STALE_AFTER_SECS`] old or more), letting a less confident source
    /// override. Returns true if the visible status or message changed.
    pub fn observe(&mut self, obs: Observation) -> bool {
        let accept = match &self.current {
            None => true,
            Some(cur) => obs.source >= cur.source || obs.at >= cur.at + STALE_AFTER_SECS,
        };
        if !accept {
            return false;
        }
        let changed = match &self.current {
            None => true,
            Some(cur) => cur.status != obs.status || cur.message != obs.message,
        };
        self.current = Some(obs);
        changed
    }

    pub fn status(&self) -> Status {
        self.current.as_ref().map(|o| o.status).unwrap_or_default()
    }

    pub fn message(&self) -> Option<&str> {
        self.current.as_ref().and_then(|o| o.message.as_deref())
    }

    /// Heuristic-only status is drawn with a hollow dot.
    pub fn low_confidence(&self) -> bool {
        matches!(&self.current, Some(o) if o.source == Source::Heuristic)
    }

    pub fn clear(&mut self) {
        self.current = None;
    }
}

/// The most urgent status among a workspace's tabs (`None` for no tabs).
pub fn rollup(statuses: impl IntoIterator<Item = Status>) -> Status {
    statuses.into_iter().max().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(status: Status, source: Source, at: u64) -> Observation {
        Observation { status, source, message: None, at }
    }

    fn obs_msg(status: Status, source: Source, at: u64, msg: &str) -> Observation {
        Observation { status, source, message: Some(msg.to_string()), at }
    }

    #[test]
    fn glyphs_match_spec_dot_shapes() {
        let cases = [
            (Status::None, ""),
            (Status::Disconnected, "⊘"),
            (Status::Hibernated, "◌"),
            (Status::Idle, "○"),
            (Status::Running, "●"),
            (Status::NeedsInput, "◐"),
        ];
        for (status, glyph) in cases {
            assert_eq!(status.glyph(), glyph, "{status:?}");
        }
    }

    #[test]
    fn source_confidence_ordering() {
        assert!(Source::Heuristic < Source::ShellIntegration);
        assert!(Source::ShellIntegration < Source::Osc);
        assert!(Source::Osc < Source::Hook);
    }

    #[test]
    fn fresh_tracker_has_no_status_or_message() {
        let t = Tracker::default();
        assert_eq!(t.status(), Status::None);
        assert_eq!(t.message(), None);
        assert!(!t.low_confidence());
    }

    #[test]
    fn first_observation_is_always_accepted() {
        let mut t = Tracker::default();
        let changed = t.observe(obs(Status::Running, Source::Heuristic, 100));
        assert!(changed);
        assert_eq!(t.status(), Status::Running);
    }

    /// Table of (current, incoming) pairs and whether arbitration accepts the incoming
    /// observation, covering higher/lower/equal confidence and stale overrides at the boundary.
    #[test]
    fn arbitration_table() {
        struct Case {
            name: &'static str,
            current: (Status, Source, u64),
            incoming: (Status, Source, u64),
            accepted: bool,
        }
        let cases = [
            Case {
                name: "higher confidence always wins, even if older",
                current: (Status::Idle, Source::Heuristic, 100),
                incoming: (Status::Running, Source::Hook, 50),
                accepted: true,
            },
            Case {
                name: "equal source replaces the current observation",
                current: (Status::Idle, Source::Osc, 100),
                incoming: (Status::Running, Source::Osc, 101),
                accepted: true,
            },
            Case {
                name: "lower confidence rejected while current is fresh",
                current: (Status::NeedsInput, Source::Hook, 100),
                incoming: (Status::Idle, Source::Heuristic, 105),
                accepted: false,
            },
            Case {
                name: "lower confidence rejected 1s before the stale boundary",
                current: (Status::NeedsInput, Source::Hook, 100),
                incoming: (Status::Idle, Source::Heuristic, 109),
                accepted: false,
            },
            Case {
                name: "lower confidence accepted at exactly the 10s stale boundary",
                current: (Status::NeedsInput, Source::Hook, 100),
                incoming: (Status::Idle, Source::Heuristic, 110),
                accepted: true,
            },
            Case {
                name: "lower confidence accepted well past the stale boundary",
                current: (Status::NeedsInput, Source::Hook, 100),
                incoming: (Status::Idle, Source::Heuristic, 500),
                accepted: true,
            },
            Case {
                name: "a heuristic never overrides a fresh hook",
                current: (Status::Running, Source::Hook, 1_000),
                incoming: (Status::NeedsInput, Source::Heuristic, 1_009),
                accepted: false,
            },
        ];
        for c in cases {
            let mut t = Tracker::default();
            t.observe(obs(c.current.0, c.current.1, c.current.2));
            let before = t.status();
            let changed = t.observe(obs(c.incoming.0, c.incoming.1, c.incoming.2));
            if c.accepted {
                assert_eq!(t.status(), c.incoming.0, "{}: expected acceptance", c.name);
                assert_eq!(changed, c.incoming.0 != before, "{}: changed flag", c.name);
            } else {
                assert_eq!(t.status(), c.current.0, "{}: expected rejection", c.name);
                assert!(!changed, "{}: rejected observation must report unchanged", c.name);
            }
        }
    }

    #[test]
    fn observe_reports_changed_only_when_status_or_message_differ() {
        let mut t = Tracker::default();
        assert!(t.observe(obs_msg(Status::NeedsInput, Source::Hook, 0, "Approve edit?")));
        // Same source, same status and message, later timestamp: no visible change.
        assert!(!t.observe(obs_msg(Status::NeedsInput, Source::Hook, 1, "Approve edit?")));
        // Same status, different message: visible change even though status is identical.
        assert!(t.observe(obs_msg(Status::NeedsInput, Source::Hook, 2, "Run the tests?")));
        assert_eq!(t.message(), Some("Run the tests?"));
    }

    #[test]
    fn low_confidence_true_only_for_heuristic_source() {
        let mut t = Tracker::default();
        t.observe(obs(Status::NeedsInput, Source::Heuristic, 0));
        assert!(t.low_confidence());
        t.observe(obs(Status::Running, Source::Osc, 1));
        assert!(!t.low_confidence());
    }

    #[test]
    fn clear_resets_to_no_status() {
        let mut t = Tracker::default();
        t.observe(obs_msg(Status::Running, Source::Hook, 0, "hi"));
        t.clear();
        assert_eq!(t.status(), Status::None);
        assert_eq!(t.message(), None);
        assert!(!t.low_confidence());
        // A clear reopens the door to any source again.
        assert!(t.observe(obs(Status::Idle, Source::Heuristic, 1)));
    }

    #[test]
    fn rollup_picks_most_urgent_status() {
        assert_eq!(rollup([]), Status::None);
        assert_eq!(rollup([Status::Idle]), Status::Idle);
        assert_eq!(rollup([Status::Idle, Status::NeedsInput, Status::Running]), Status::NeedsInput);
        // §5.29 urgency chain: needs input > running > idle > hibernated > disconnected.
        assert_eq!(rollup([Status::Hibernated, Status::Disconnected]), Status::Hibernated);
        assert_eq!(rollup([Status::None, Status::None]), Status::None);
    }
}
