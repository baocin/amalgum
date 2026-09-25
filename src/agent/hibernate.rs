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

/// Check every rule; the first failing one is returned. Order (deliberate, not the
/// [`Ineligible`] declaration order): the "Never" rules first (needs-input, no session id,
/// unsaved input), then the hook-idle/I/O/foreground/focus checks, then the threshold, which
/// manual "Hibernate now" skips.
pub fn eligible(f: &TabFacts, now: u64, threshold_secs: u64, manual: bool) -> Result<(), Ineligible> {
    if f.status == Status::NeedsInput {
        return Err(Ineligible::NeedsInput);
    }
    if f.session_id.is_none() {
        return Err(Ineligible::NoSessionId);
    }
    if f.prompt_has_input {
        return Err(Ineligible::UnsavedInput);
    }
    if !f.idle_from_hook {
        return Err(Ineligible::NotIdleByHook);
    }
    if f.last_io > f.idle_since {
        return Err(Ineligible::IoSinceIdle);
    }
    if !f.agent_is_foreground {
        return Err(Ineligible::AgentNotForeground);
    }
    if f.focused {
        return Err(Ineligible::Focused);
    }
    if !manual && now.saturating_sub(f.idle_since) < threshold_secs {
        return Err(Ineligible::BelowThreshold);
    }
    Ok(())
}

/// Live-terminal limit: given `(tab id, agent start time, eligible)` for every tab running an
/// agent, return the tabs to hibernate so at most `limit` remain, oldest eligible first.
pub fn over_limit<'a>(running: &[(&'a str, u64, bool)], limit: usize) -> Vec<&'a str> {
    if running.len() <= limit {
        return Vec::new();
    }
    let mut eligible: Vec<(&'a str, u64)> =
        running.iter().filter(|(_, _, e)| *e).map(|(id, started, _)| (*id, *started)).collect();
    eligible.sort_by_key(|(_, started)| *started);
    let excess = running.len() - limit;
    eligible.into_iter().take(excess).map(|(id, _)| id).collect()
}

/// Keep only the `--flag` / `--flag=value` long-option arguments from `argv` (dropping
/// `argv[0]`, short flags, `VAR=value` env assignments, and positional/prompt text): the parts
/// of the original invocation worth replaying on resume, e.g.
/// `["claude", "--dangerously-skip-permissions", "FOO=bar", "fix the bug"]` →
/// `["--dangerously-skip-permissions"]`.
pub fn resume_flags(argv: &[String]) -> Vec<String> {
    argv.iter().skip(1).filter(|a| is_long_flag(a)).cloned().collect()
}

fn is_long_flag(a: &str) -> bool {
    a.starts_with("--") && a.len() > 2
}

/// The command typed into the shell to resume: the agent's resume command plus any `--flag`
/// arguments from the original argv (e.g. `--dangerously-skip-permissions`), never env
/// assignments or positional prompt text.
pub fn resume_line(agent: AgentKind, session_id: &str, original_argv: &[String]) -> String {
    let mut line = crate::agent::adapters::resume_command(agent, session_id);
    for flag in resume_flags(original_argv) {
        line.push(' ');
        line.push_str(&crate::ssh::quote(&flag));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tab that satisfies every rule at `now == 1_000`, `idle_since == 0`, threshold 300s.
    fn eligible_facts() -> TabFacts {
        TabFacts {
            status: Status::Idle,
            idle_from_hook: true,
            idle_since: 0,
            last_io: 0,
            agent_is_foreground: true,
            focused: false,
            session_id: Some("sess-1".into()),
            prompt_has_input: false,
        }
    }

    #[test]
    fn baseline_facts_are_eligible() {
        assert_eq!(eligible(&eligible_facts(), 1_000, 300, false), Ok(()));
    }

    /// Flip exactly one field away from the eligible baseline and check the reported reason,
    /// in the order the implementation is documented to check them.
    #[test]
    fn each_rule_reports_its_own_reason() {
        struct Case {
            name: &'static str,
            make: fn(TabFacts) -> TabFacts,
            want: Ineligible,
        }
        let cases = [
            Case {
                name: "needs input",
                make: |mut f| {
                    f.status = Status::NeedsInput;
                    f
                },
                want: Ineligible::NeedsInput,
            },
            Case {
                name: "no session id",
                make: |mut f| {
                    f.session_id = None;
                    f
                },
                want: Ineligible::NoSessionId,
            },
            Case {
                name: "unsaved prompt input",
                make: |mut f| {
                    f.prompt_has_input = true;
                    f
                },
                want: Ineligible::UnsavedInput,
            },
            Case {
                name: "last status was not from a hook",
                make: |mut f| {
                    f.idle_from_hook = false;
                    f
                },
                want: Ineligible::NotIdleByHook,
            },
            Case {
                name: "PTY I/O happened after idle_since",
                make: |mut f| {
                    f.idle_since = 10;
                    f.last_io = 11;
                    f
                },
                want: Ineligible::IoSinceIdle,
            },
            Case {
                name: "agent no longer the foreground process",
                make: |mut f| {
                    f.agent_is_foreground = false;
                    f
                },
                want: Ineligible::AgentNotForeground,
            },
            Case {
                name: "tab is focused",
                make: |mut f| {
                    f.focused = true;
                    f
                },
                want: Ineligible::Focused,
            },
            Case {
                name: "idle time below threshold",
                make: |mut f| {
                    f.idle_since = 999;
                    f
                },
                want: Ineligible::BelowThreshold,
            },
        ];
        for c in cases {
            let facts = (c.make)(eligible_facts());
            assert_eq!(eligible(&facts, 1_000, 300, false), Err(c.want), "{}", c.name);
        }
    }

    #[test]
    fn io_exactly_at_idle_since_does_not_disqualify() {
        // last_io > idle_since disqualifies; equal does not (the hook event itself is I/O).
        let mut f = eligible_facts();
        f.idle_since = 50;
        f.last_io = 50;
        assert_eq!(eligible(&f, 1_000, 300, false), Ok(()));
    }

    #[test]
    fn threshold_boundary_is_inclusive() {
        let mut f = eligible_facts();
        f.idle_since = 700;
        // now - idle_since == threshold exactly: not below, so eligible.
        assert_eq!(eligible(&f, 1_000, 300, false), Ok(()));
        // One second short of the threshold: still below.
        f.idle_since = 701;
        assert_eq!(eligible(&f, 1_000, 300, false), Err(Ineligible::BelowThreshold));
    }

    #[test]
    fn manual_hibernate_skips_only_the_threshold_check() {
        let mut f = eligible_facts();
        f.idle_since = 999; // 1s idle, threshold 300s: fails automatically.
        assert_eq!(eligible(&f, 1_000, 300, false), Err(Ineligible::BelowThreshold));
        assert_eq!(eligible(&f, 1_000, 300, true), Ok(()));

        // Manual does not skip the "Never" rules or the hook/foreground/focus checks.
        f.status = Status::NeedsInput;
        assert_eq!(eligible(&f, 1_000, 300, true), Err(Ineligible::NeedsInput));
    }

    #[test]
    fn precedence_returns_the_first_failing_rule() {
        // Both NeedsInput and NoSessionId apply; NeedsInput is checked first.
        let mut f = eligible_facts();
        f.status = Status::NeedsInput;
        f.session_id = None;
        assert_eq!(eligible(&f, 1_000, 300, false), Err(Ineligible::NeedsInput));

        // NoSessionId and UnsavedInput both apply; NoSessionId is checked first.
        let mut f = eligible_facts();
        f.session_id = None;
        f.prompt_has_input = true;
        assert_eq!(eligible(&f, 1_000, 300, false), Err(Ineligible::NoSessionId));

        // IoSinceIdle and AgentNotForeground both apply; IoSinceIdle is checked first.
        let mut f = eligible_facts();
        f.idle_since = 10;
        f.last_io = 20;
        f.agent_is_foreground = false;
        assert_eq!(eligible(&f, 1_000, 300, false), Err(Ineligible::IoSinceIdle));
    }

    // -- over_limit -------------------------------------------------------------------------

    #[test]
    fn over_limit_empty_when_at_or_under_the_cap() {
        let running = [("a", 1, true), ("b", 2, true)];
        assert!(over_limit(&running, 2).is_empty());
        assert!(over_limit(&running, 5).is_empty());
    }

    #[test]
    fn over_limit_picks_oldest_eligible_tabs_first() {
        // 4 running, limit 2 => hibernate 2, oldest eligible first, ignoring ineligible ones
        // even when they are older.
        let running = [
            ("oldest-ineligible", 1, false),
            ("second-oldest", 2, true),
            ("third", 3, true),
            ("newest", 4, true),
        ];
        assert_eq!(over_limit(&running, 2), vec!["second-oldest", "third"]);
    }

    #[test]
    fn over_limit_returns_fewer_than_excess_when_not_enough_eligible() {
        let running = [("a", 1, false), ("b", 2, true), ("c", 3, false), ("d", 4, false)];
        // Excess is 3, but only one tab is eligible.
        assert_eq!(over_limit(&running, 1), vec!["b"]);
    }

    #[test]
    fn over_limit_zero_hibernates_all_eligible() {
        let running = [("a", 2, true), ("b", 1, true)];
        assert_eq!(over_limit(&running, 0), vec!["b", "a"]);
    }

    // -- resume_flags -------------------------------------------------------------------------

    #[test]
    fn resume_flags_drops_argv0_env_assignments_short_flags_and_positionals() {
        let argv = vec![
            "claude".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "-x".to_string(),
            "FOO=bar".to_string(),
            "fix the bug".to_string(),
            "--model=opus".to_string(),
        ];
        assert_eq!(
            resume_flags(&argv),
            vec!["--dangerously-skip-permissions".to_string(), "--model=opus".to_string()]
        );
    }

    #[test]
    fn resume_flags_empty_argv_and_program_name_only() {
        assert_eq!(resume_flags(&[]), Vec::<String>::new());
        assert_eq!(resume_flags(&["claude".to_string()]), Vec::<String>::new());
    }

    #[test]
    fn resume_flags_ignores_bare_double_dash() {
        let argv = vec!["claude".to_string(), "--".to_string(), "--resume".to_string()];
        assert_eq!(resume_flags(&argv), vec!["--resume".to_string()]);
    }

    #[test]
    fn resume_line_is_the_agents_command_plus_quoted_long_flags() {
        let argv: Vec<String> =
            ["claude", "--dangerously-skip-permissions", "--msg=hello world", "fix the bug", "-v"]
                .map(String::from)
                .into();
        assert_eq!(
            resume_line(AgentKind::Claude, "7f3a", &argv),
            "claude --resume 7f3a --dangerously-skip-permissions '--msg=hello world'"
        );
        assert_eq!(resume_line(AgentKind::Codex, "id with space", &[]), "codex resume 'id with space'");
    }
}
