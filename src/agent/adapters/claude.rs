//! Claude Code. Hooks live in `~/.claude/settings.json` under `"hooks"`:
//! `{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"…","timeout":1}]}]}}`.
//! Payloads arrive on stdin with `hook_event_name`, `session_id`, `cwd`, and per-event fields
//! (`message` on Notification, `tool_name` on PermissionRequest).
//! Events we install: SessionStart, UserPromptSubmit, Notification, PermissionRequest, Stop,
//! SessionEnd (§5.29). Our entries are recognised by their command containing
//! `agent-event --agent claude` — that substring is the marker.
//! Mapping: SessionStart/UserPromptSubmit → Running; Notification/PermissionRequest →
//! NeedsInput; Stop → Idle, or NeedsInput when the last assistant message ends with `?`;
//! SessionEnd → Status::None. Resume: `claude --resume <id>`.

use super::Takes::{Many, Nothing, One, Optional};
use super::{AgentEvent, AgentKind, EventKind, HookState, Status};
use std::path::{Path, PathBuf};

/// Events we own in this config (installed, recognised for removal/state).
const EVENTS: &[&str] =
    &["SessionStart", "UserPromptSubmit", "Notification", "PermissionRequest", "Stop", "SessionEnd"];

pub fn adapt(raw: &serde_json::Value) -> Option<AgentEvent> {
    let event_name = super::str_field(raw, "hook_event_name")?;
    let session_id = super::str_field(raw, "session_id");
    let pid = super::u32_field(raw, "pid");
    let cwd = super::str_field(raw, "cwd");

    let (kind, status, message) = match event_name.as_str() {
        "SessionStart" => (EventKind::SessionStart, Status::Running, None),
        "UserPromptSubmit" => (EventKind::PromptSubmit, Status::Running, None),
        "Notification" => (EventKind::Notification, Status::NeedsInput, super::str_field(raw, "message")),
        "PermissionRequest" => {
            let message = super::str_field(raw, "message")
                .or_else(|| super::str_field(raw, "tool_name").map(|t| format!("Permission requested: {t}")));
            (EventKind::PermissionRequest, Status::NeedsInput, message)
        }
        "Stop" => {
            let last =
                super::str_field(raw, "last_assistant_message").or_else(|| super::str_field(raw, "message"));
            match last {
                Some(text) if text.trim().ends_with('?') => {
                    (EventKind::Stop, Status::NeedsInput, Some(text.trim().to_string()))
                }
                _ => (EventKind::Stop, Status::Idle, None),
            }
        }
        "SessionEnd" => (EventKind::SessionEnd, Status::None, None),
        _ => return None,
    };

    Some(AgentEvent { agent: AgentKind::Claude, kind, status, message, session_id, pid, cwd })
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

/// Invalid existing JSON is an error (never clobber a file we cannot parse).
pub fn install(text: &str, command: &str) -> Result<String, String> {
    super::json_install(text, command, "claude", EVENTS, 1)
}

pub fn uninstall(text: &str) -> Result<String, String> {
    super::json_uninstall(text, "claude", EVENTS)
}

pub fn state(text: &str, command: &str) -> HookState {
    super::json_state(text, command, "claude", EVENTS)
}

pub fn resume(session_id: &str) -> String {
    format!("claude --resume {}", crate::ssh::quote(session_id))
}

/// `claude --help` (2.1), plus the `--*-file` prompt options and `--permission-prompt-tool` it
/// names in passing.
pub const CLI_OPTIONS: super::CliOptions = super::CliOptions {
    options: &[
        ("--add-dir", Many),
        ("--agent", One),
        ("--agents", One),
        ("--allow-dangerously-skip-permissions", Nothing),
        ("--allowed-tools", Many),
        ("--allowedTools", Many),
        ("--append-system-prompt", One),
        ("--append-system-prompt-file", One),
        ("--autocompact", One),
        ("--ax-screen-reader", Nothing),
        ("--background", Nothing),
        ("--bare", Nothing),
        ("--betas", Many),
        ("--bg", Nothing),
        ("--brief", Nothing),
        ("--chrome", Nothing),
        ("--cloud", Optional),
        ("--continue", Nothing),
        ("--dangerously-skip-permissions", Nothing),
        ("--debug", Optional),
        ("--debug-file", One),
        ("--disable-slash-commands", Nothing),
        ("--disallowed-tools", Many),
        ("--disallowedTools", Many),
        ("--effort", One),
        ("--environment", One),
        ("--exclude-dynamic-system-prompt-sections", Nothing),
        ("--fallback-model", One),
        ("--file", Many),
        ("--fork-session", Nothing),
        ("--forward-subagent-text", Nothing),
        ("--from-pr", Optional),
        ("--ide", Nothing),
        ("--include-hook-events", Nothing),
        ("--include-partial-messages", Nothing),
        ("--input-format", One),
        ("--json-schema", One),
        ("--max-budget-usd", One),
        ("--mcp-config", Many),
        ("--model", One),
        ("--name", One),
        ("--no-chrome", Nothing),
        ("--no-session-persistence", Nothing),
        ("--output-format", One),
        ("--permission-mode", One),
        ("--permission-prompt-tool", One),
        ("--permission-prompts", One),
        ("--plugin-dir", One),
        ("--plugin-url", One),
        ("--print", Nothing),
        ("--prompt-suggestions", Optional),
        ("--remote-control", Optional),
        ("--remote-control-session-name-prefix", One),
        ("--replay-user-messages", Nothing),
        ("--restricted", Nothing),
        ("--resume", Optional),
        ("--safe-mode", Nothing),
        ("--session-id", One),
        ("--setting-sources", One),
        ("--settings", One),
        ("--strict-mcp-config", Nothing),
        ("--system-prompt", One),
        ("--system-prompt-file", One),
        ("--system-prompt-snapshot", One),
        ("--teleport", Optional),
        ("--tmux", Nothing),
        ("--tools", Many),
        ("--verbose", Nothing),
        ("--worktree", Optional),
    ],
    // `--worktree <name>` is replayed: an existing worktree of that name is reused, and the
    // session lives there. `--bg` would take the resumed session out of the tab.
    not_replayed: &[
        "--background",
        "--bg",
        "--cloud",
        "--continue",
        "--environment",
        "--fork-session",
        "--from-pr",
        "--print",
        "--resume",
        "--session-id",
        "--teleport",
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("valid test fixture JSON")
    }

    #[test]
    fn adapt_session_start_and_prompt_submit_are_running() {
        let ev =
            adapt(&raw(r#"{"hook_event_name":"SessionStart","session_id":"s1","cwd":"/repo"}"#)).unwrap();
        assert_eq!(ev.kind, EventKind::SessionStart);
        assert_eq!(ev.status, Status::Running);
        assert_eq!(ev.session_id.as_deref(), Some("s1"));
        assert_eq!(ev.cwd.as_deref(), Some("/repo"));

        let ev = adapt(&raw(r#"{"hook_event_name":"UserPromptSubmit"}"#)).unwrap();
        assert_eq!(ev.kind, EventKind::PromptSubmit);
        assert_eq!(ev.status, Status::Running);
    }

    #[test]
    fn adapt_notification_carries_message() {
        let ev =
            adapt(&raw(r#"{"hook_event_name":"Notification","message":"Waiting for approval"}"#)).unwrap();
        assert_eq!(ev.kind, EventKind::Notification);
        assert_eq!(ev.status, Status::NeedsInput);
        assert_eq!(ev.message.as_deref(), Some("Waiting for approval"));
        assert!(ev.notifies());
    }

    #[test]
    fn adapt_permission_request_falls_back_to_tool_name() {
        let ev = adapt(&raw(r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash"}"#)).unwrap();
        assert_eq!(ev.status, Status::NeedsInput);
        assert_eq!(ev.message.as_deref(), Some("Permission requested: Bash"));

        let ev = adapt(&raw(
            r#"{"hook_event_name":"PermissionRequest","message":"custom text","tool_name":"Bash"}"#,
        ))
        .unwrap();
        assert_eq!(ev.message.as_deref(), Some("custom text"));

        let ev = adapt(&raw(r#"{"hook_event_name":"PermissionRequest"}"#)).unwrap();
        assert_eq!(ev.message, None);
    }

    #[test]
    fn adapt_stop_is_idle_unless_a_question() {
        let ev = adapt(&raw(r#"{"hook_event_name":"Stop","last_assistant_message":"Done."}"#)).unwrap();
        assert_eq!(ev.status, Status::Idle);
        assert_eq!(ev.message, None);

        let ev = adapt(&raw(r#"{"hook_event_name":"Stop","last_assistant_message":"Should I continue? "}"#))
            .unwrap();
        assert_eq!(ev.status, Status::NeedsInput);
        assert_eq!(ev.message.as_deref(), Some("Should I continue?"));

        // `message` fallback when `last_assistant_message` is absent.
        let ev = adapt(&raw(r#"{"hook_event_name":"Stop","message":"Ready for the next step?"}"#)).unwrap();
        assert_eq!(ev.status, Status::NeedsInput);
    }

    #[test]
    fn adapt_session_end_clears_status() {
        let ev = adapt(&raw(r#"{"hook_event_name":"SessionEnd"}"#)).unwrap();
        assert_eq!(ev.kind, EventKind::SessionEnd);
        assert_eq!(ev.status, Status::None);
    }

    #[test]
    fn adapt_unknown_event_is_ignored() {
        assert_eq!(adapt(&raw(r#"{"hook_event_name":"SomethingElse"}"#)), None);
        assert_eq!(adapt(&raw(r#"{}"#)), None);
    }

    #[test]
    fn adapt_never_panics_on_malformed_json() {
        for v in [
            serde_json::Value::Null,
            json!(42),
            json!([1, 2, 3]),
            json!("oops"),
            json!({"hook_event_name": 5}),
        ] {
            assert_eq!(adapt(&v), None);
        }
    }

    #[test]
    fn config_path_is_under_dot_claude() {
        let home = Path::new("/home/u"); // portability: allow
        assert_eq!(config_path(home), Path::new("/home/u/.claude/settings.json")); // portability: allow
    }

    #[test]
    fn resume_quotes_when_needed() {
        assert_eq!(resume("abc123"), "claude --resume abc123");
        assert_eq!(resume("has space"), "claude --resume 'has space'");
    }

    #[test]
    fn install_creates_hooks_for_every_owned_event() {
        let out = install("", "amalgum agent-event --agent claude --hook-version 1").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        for event in EVENTS {
            let hooks = v["hooks"][event].as_array().expect("array");
            assert_eq!(hooks.len(), 1);
            assert_eq!(
                hooks[0]["hooks"][0]["command"],
                "amalgum agent-event --agent claude --hook-version 1"
            );
            assert_eq!(hooks[0]["hooks"][0]["timeout"], 1);
        }
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn install_is_idempotent() {
        let cmd = "amalgum agent-event --agent claude --hook-version 1";
        let once = install("", cmd).unwrap();
        let twice = install(&once, cmd).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn install_replaces_old_command_and_preserves_other_hooks() {
        let existing = json!({
            "hooks": {
                "Stop": [
                    {"matcher": "", "hooks": [{"type": "command", "command": "amalgum agent-event --agent claude --hook-version 0", "timeout": 1}]},
                    {"matcher": "foo", "hooks": [{"type": "command", "command": "some-other-tool", "timeout": 5}]}
                ]
            }
        });
        let out =
            install(&existing.to_string(), "amalgum agent-event --agent claude --hook-version 1").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        // The unrelated group survives; the old marker group was replaced by the new one.
        assert_eq!(stop.len(), 2);
        assert!(stop.iter().any(|g| g["hooks"][0]["command"] == "some-other-tool"));
        assert!(
            stop.iter()
                .any(|g| g["hooks"][0]["command"] == "amalgum agent-event --agent claude --hook-version 1")
        );
        assert!(
            !stop
                .iter()
                .any(|g| g["hooks"][0]["command"] == "amalgum agent-event --agent claude --hook-version 0")
        );
    }

    #[test]
    fn install_preserves_unrelated_top_level_keys() {
        let out = install(r#"{"model":"opus"}"#, "cmd").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["model"], "opus");
    }

    #[test]
    fn install_rejects_invalid_json() {
        assert!(install("not json", "cmd").is_err());
    }

    #[test]
    fn install_rejects_non_object_root() {
        assert!(install("[1,2,3]", "cmd").is_err());
    }

    #[test]
    fn uninstall_removes_only_our_entries() {
        let existing = json!({
            "hooks": {
                "Stop": [
                    {"matcher": "", "hooks": [{"type": "command", "command": "amalgum agent-event --agent claude --hook-version 1", "timeout": 1}]},
                    {"matcher": "foo", "hooks": [{"type": "command", "command": "some-other-tool", "timeout": 5}]}
                ],
                "Notification": [
                    {"matcher": "", "hooks": [{"type": "command", "command": "amalgum agent-event --agent claude --hook-version 1", "timeout": 1}]}
                ]
            }
        });
        let out = uninstall(&existing.to_string()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["hooks"][0]["command"], "some-other-tool");
        assert!(v["hooks"].get("Notification").is_none(), "event array emptied by removal is dropped");
    }

    #[test]
    fn uninstall_drops_hooks_key_when_empty() {
        let existing = json!({
            "model": "opus",
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [{"type": "command", "command": "amalgum agent-event --agent claude --hook-version 1", "timeout": 1}]}]
            }
        });
        let out = uninstall(&existing.to_string()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("hooks").is_none());
        assert_eq!(v["model"], "opus");
    }

    #[test]
    fn uninstall_of_untouched_config_is_a_noop() {
        let out = uninstall(r#"{"model":"opus"}"#).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["model"], "opus");
    }

    #[test]
    fn uninstall_of_empty_text_is_empty() {
        assert_eq!(uninstall("").unwrap(), "");
    }

    #[test]
    fn uninstall_rejects_invalid_json() {
        assert!(uninstall("not json").is_err());
    }

    #[test]
    fn state_missing_when_no_marker() {
        assert_eq!(state("", "cmd"), HookState::Missing);
        assert_eq!(state(r#"{"model":"opus"}"#, "cmd"), HookState::Missing);
    }

    #[test]
    fn state_installed_after_install() {
        let cmd = "amalgum agent-event --agent claude --hook-version 1";
        let out = install("", cmd).unwrap();
        assert_eq!(state(&out, cmd), HookState::Installed);
    }

    #[test]
    fn state_outdated_when_command_differs_or_event_missing() {
        let old_cmd = "amalgum agent-event --agent claude --hook-version 0";
        let new_cmd = "amalgum agent-event --agent claude --hook-version 1";
        let out = install("", old_cmd).unwrap();
        assert_eq!(state(&out, new_cmd), HookState::Outdated);

        // Marker present for some events but not all (simulate a partial/older install).
        let mut v: serde_json::Value = serde_json::from_str(&out).unwrap();
        v["hooks"].as_object_mut().unwrap().remove("SessionEnd");
        assert_eq!(state(&v.to_string(), old_cmd), HookState::Outdated);
    }
}
