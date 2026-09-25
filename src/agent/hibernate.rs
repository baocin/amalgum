//! Hibernation rules (§5.30). Pure decisions; the app performs the SIGTERM/SIGKILL and UI.
//!
//! A tab is eligible only when *all* hold: the last hook event was `Stop` or idle (never on
//! heuristics alone), no PTY I/O since that event, the agent process is still the foreground
//! process, the tab is not focused, idle time exceeds the threshold (manual "Hibernate now"
//! skips only this check), and none of the "Never" rules apply: not needs-input, the agent has
//! a captured session id, and no unsaved input on the prompt line.

use super::AgentKind;
use super::status::Status;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabFacts {
    pub status: Status,
    /// True when the latest status came from a hook `Stop`/idle event.
    pub idle_from_hook: bool,
    /// Unix seconds of that hook event.
    pub idle_since: u64,
    /// Unix seconds of the last PTY read or write.
    pub last_io: u64,
    pub agent_is_foreground: bool,
    pub focused: bool,
    pub session_id: Option<String>,
    pub prompt_has_input: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ineligible {
    NotIdleByHook,
    IoSinceIdle,
    AgentNotForeground,
    Focused,
    BelowThreshold,
    NeedsInput,
    NoSessionId,
    UnsavedInput,
}

/// Check every rule; the first failing one is returned.
pub fn eligible(f: &TabFacts, now: u64, threshold_secs: u64, manual: bool) -> Result<(), Ineligible> {
    todo!()
}

/// Live-terminal limit: given `(tab id, agent start time, eligible)` for every tab running an
/// agent, return the tabs to hibernate so at most `limit` remain, oldest eligible first.
pub fn over_limit<'a>(running: &[(&'a str, u64, bool)], limit: usize) -> Vec<&'a str> {
    todo!()
}

/// The command typed into the shell to resume: the agent's resume command plus any `--flag`
/// arguments from the original argv (e.g. `--dangerously-skip-permissions`), never env
/// assignments or positional prompt text.
pub fn resume_line(agent: AgentKind, session_id: &str, original_argv: &[String]) -> String {
    todo!()
}
