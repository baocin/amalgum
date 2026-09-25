//! Gemini CLI. `~/.gemini/settings.json` `"hooks"` with the same shape as Claude Code's
//! (`{"hooks":{"<Event>":[{"matcher":"","hooks":[{"type":"command","command":"…"}]}]}}`).
//! Payloads carry `hook_event_name`, `session_id`, `cwd`. Events we install: SessionStart,
//! BeforeAgent (→ Running), Notification (→ NeedsInput), AfterAgent (→ Idle, NeedsInput if the
//! response ends with `?`), SessionEnd (→ Status::None). Marker: command contains
//! `agent-event --agent gemini`. Resume: `gemini --resume <id>`.
//! Gemini's hook schema is young; keep this file the only place that knows it.

use super::{AgentEvent, AgentKind, EventKind, HookState, Status};
use std::path::{Path, PathBuf};

/// Events we own in this config.
const EVENTS: &[&str] = &["SessionStart", "BeforeAgent", "Notification", "AfterAgent", "SessionEnd"];

/// Gemini's hook `timeout` is documented in milliseconds (unlike Claude Code's seconds); 1000 ms
/// gives the same one-second budget as Claude's `timeout: 1` (§5.29: "always exit 0 ... 1 s
/// timeout"). Decision made here since the module doc's example omits the field.
const TIMEOUT_MS: u64 = 1000;

pub fn adapt(raw: &serde_json::Value) -> Option<AgentEvent> {
    let event_name = super::str_field(raw, "hook_event_name")?;
    let session_id = super::str_field(raw, "session_id");
    let pid = super::u32_field(raw, "pid");
    let cwd = super::str_field(raw, "cwd");

    // Gemini's own text: `prompt_response` first, `message` as a fallback.
    let response = || super::str_field(raw, "prompt_response").or_else(|| super::str_field(raw, "message"));

    let (kind, status, message) = match event_name.as_str() {
        "SessionStart" => (EventKind::SessionStart, Status::Running, None),
        "BeforeAgent" => (EventKind::PromptSubmit, Status::Running, None),
        "Notification" => (EventKind::Notification, Status::NeedsInput, response()),
        "AfterAgent" => match response() {
            Some(text) if text.trim().ends_with('?') => {
                (EventKind::Stop, Status::NeedsInput, Some(text.trim().to_string()))
            }
            _ => (EventKind::Stop, Status::Idle, None),
        },
        "SessionEnd" => (EventKind::SessionEnd, Status::None, None),
        _ => return None,
    };

    Some(AgentEvent { agent: AgentKind::Gemini, kind, status, message, session_id, pid, cwd })
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join(".gemini").join("settings.json")
}

pub fn install(text: &str, command: &str) -> Result<String, String> {
    super::json_install(text, command, "gemini", EVENTS, TIMEOUT_MS)
}

pub fn uninstall(text: &str) -> Result<String, String> {
    super::json_uninstall(text, "gemini", EVENTS)
}

pub fn state(text: &str, command: &str) -> HookState {
    super::json_state(text, command, "gemini", EVENTS)
}

pub fn resume(session_id: &str) -> String {
    format!("gemini --resume {}", crate::ssh::quote(session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("valid test fixture JSON")
    }

    #[test]
    fn adapt_session_start_and_before_agent_are_running() {
        let ev =
            adapt(&raw(r#"{"hook_event_name":"SessionStart","session_id":"s1","cwd":"/repo"}"#)).unwrap();
        assert_eq!(ev.kind, EventKind::SessionStart);
        assert_eq!(ev.status, Status::Running);

        let ev = adapt(&raw(r#"{"hook_event_name":"BeforeAgent"}"#)).unwrap();
        assert_eq!(ev.kind, EventKind::PromptSubmit);
        assert_eq!(ev.status, Status::Running);
    }

    #[test]
    fn adapt_notification_prefers_prompt_response_then_message() {
        let ev =
            adapt(&raw(r#"{"hook_event_name":"Notification","prompt_response":"A","message":"B"}"#)).unwrap();
        assert_eq!(ev.message.as_deref(), Some("A"));

        let ev = adapt(&raw(r#"{"hook_event_name":"Notification","message":"B"}"#)).unwrap();
        assert_eq!(ev.message.as_deref(), Some("B"));
        assert!(ev.notifies());
    }

    #[test]
    fn adapt_after_agent_is_idle_unless_a_question() {
        let ev = adapt(&raw(r#"{"hook_event_name":"AfterAgent","prompt_response":"All done."}"#)).unwrap();
        assert_eq!(ev.status, Status::Idle);
        assert_eq!(ev.message, None);

        let ev = adapt(&raw(r#"{"hook_event_name":"AfterAgent","prompt_response":"Continue? "}"#)).unwrap();
        assert_eq!(ev.status, Status::NeedsInput);
        assert_eq!(ev.message.as_deref(), Some("Continue?"));
    }

    #[test]
    fn adapt_session_end_clears_status() {
        let ev = adapt(&raw(r#"{"hook_event_name":"SessionEnd"}"#)).unwrap();
        assert_eq!(ev.status, Status::None);
    }

    #[test]
    fn adapt_unknown_or_malformed_is_ignored() {
        assert_eq!(adapt(&raw(r#"{"hook_event_name":"Other"}"#)), None);
        for v in [serde_json::Value::Null, json!(1), json!([1]), json!("x")] {
            assert_eq!(adapt(&v), None);
        }
    }

    #[test]
    fn config_path_is_under_dot_gemini() {
        let home = Path::new("/home/u"); // portability: allow
        assert_eq!(config_path(home), Path::new("/home/u/.gemini/settings.json")); // portability: allow
    }

    #[test]
    fn resume_quotes_when_needed() {
        assert_eq!(resume("abc"), "gemini --resume abc");
        assert_eq!(resume("has space"), "gemini --resume 'has space'");
    }

    #[test]
    fn install_uses_millisecond_timeout_and_every_owned_event() {
        let out = install("", "cmd agent-event --agent gemini --hook-version 1").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        for event in EVENTS {
            assert_eq!(v["hooks"][event][0]["hooks"][0]["timeout"], 1000);
        }
    }

    #[test]
    fn install_is_idempotent() {
        let cmd = "amalgum agent-event --agent gemini --hook-version 1";
        let once = install("", cmd).unwrap();
        let twice = install(&once, cmd).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn uninstall_removes_only_our_entries() {
        let existing = json!({
            "hooks": {
                "AfterAgent": [
                    {"matcher": "", "hooks": [{"type": "command", "command": "amalgum agent-event --agent gemini --hook-version 1", "timeout": 1000}]},
                    {"matcher": "", "hooks": [{"type": "command", "command": "unrelated", "timeout": 500}]}
                ]
            }
        });
        let out = uninstall(&existing.to_string()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let after_agent = v["hooks"]["AfterAgent"].as_array().unwrap();
        assert_eq!(after_agent.len(), 1);
        assert_eq!(after_agent[0]["hooks"][0]["command"], "unrelated");
    }

    #[test]
    fn state_transitions() {
        let cmd = "amalgum agent-event --agent gemini --hook-version 1";
        assert_eq!(state("", cmd), HookState::Missing);

        let installed = install("", cmd).unwrap();
        assert_eq!(state(&installed, cmd), HookState::Installed);

        let old_cmd = "amalgum agent-event --agent gemini --hook-version 0";
        let outdated = install("", old_cmd).unwrap();
        assert_eq!(state(&outdated, cmd), HookState::Outdated);
    }
}
