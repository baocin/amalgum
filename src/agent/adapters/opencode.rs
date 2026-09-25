//! OpenCode. We own a whole plugin file, `~/.config/opencode/plugin/amalgum.js`, which pipes
//! each bus event as JSON into `<command>` on stdin (`$` is Bun's shell; `.nothrow().quiet()`
//! so a missing app never blocks OpenCode). The file's first line is the marker
//! `// amalgum hooks v<HOOK_VERSION> — generated; edit via amalgum hooks setup`.
//! Payload: `{"type":"session.idle","properties":{"sessionID":"…"}}` etc.
//! Mapping: `session.status` busy / `message.updated` from the user → Running;
//! `permission.updated` / `permission.asked` → NeedsInput; `session.idle` → Idle;
//! `session.deleted` → Status::None. Resume: `opencode --session <id>`.
//! `install` returns the whole file; `uninstall` returns "" (the caller deletes the file).

use super::{AgentEvent, AgentKind, EventKind, HookState, Status};
use std::path::{Path, PathBuf};

const MARKER_PREFIX: &str = "// amalgum hooks v";

pub fn adapt(raw: &serde_json::Value) -> Option<AgentEvent> {
    let event_type = super::str_field(raw, "type")?;
    let properties = raw.get("properties");
    let session_id = properties
        .and_then(|p| super::str_field(p, "sessionID"))
        .or_else(|| properties.and_then(|p| p.get("info")).and_then(|info| super::str_field(info, "id")));
    let pid = super::u32_field(raw, "pid");
    let cwd = super::str_field(raw, "cwd");

    let (kind, status) = match event_type.as_str() {
        "session.status" => {
            let busy = properties.and_then(|p| super::str_field(p, "status")).is_some_and(|s| s == "busy");
            if !busy {
                return None;
            }
            (EventKind::PromptSubmit, Status::Running)
        }
        "message.updated" => {
            let from_user = properties
                .and_then(|p| p.get("info"))
                .and_then(|info| super::str_field(info, "role"))
                .is_some_and(|r| r == "user");
            if !from_user {
                return None;
            }
            (EventKind::PromptSubmit, Status::Running)
        }
        "permission.updated" | "permission.asked" => (EventKind::PermissionRequest, Status::NeedsInput),
        "session.idle" => (EventKind::Stop, Status::Idle),
        "session.deleted" => (EventKind::SessionEnd, Status::None),
        _ => return None,
    };

    let message = match kind {
        EventKind::PermissionRequest => {
            properties.and_then(|p| super::str_field(p, "title").or_else(|| super::str_field(p, "message")))
        }
        _ => None,
    };

    Some(AgentEvent { agent: AgentKind::Opencode, kind, status, message, session_id, pid, cwd })
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join(".config").join("opencode").join("plugin").join("amalgum.js")
}

/// Ignores `text`: we own the whole file, so install always regenerates it from scratch.
pub fn install(_text: &str, command: &str) -> Result<String, String> {
    let marker = format!("{MARKER_PREFIX}{} — generated; edit via amalgum hooks setup", super::HOOK_VERSION);
    // Only the JSON payload is data (interpolated with `${}`, escaped by Bun); `command` is our
    // own pre-quoted shell command and goes straight into the template as plain shell syntax.
    let body = format!(
        "export const Amalgum = async ({{ $ }}) => ({{\n  event: async ({{ event }}) => {{\n    await $`echo ${{JSON.stringify(event)}} | {command}`.nothrow().quiet()\n  }},\n}})\n"
    );
    Ok(format!("{marker}\n{body}"))
}

pub fn uninstall(_text: &str) -> Result<String, String> {
    Ok(String::new())
}

pub fn state(text: &str, command: &str) -> HookState {
    if !text.starts_with(MARKER_PREFIX) {
        return HookState::Missing;
    }
    match install(text, command) {
        Ok(expected) if expected == text => HookState::Installed,
        _ => HookState::Outdated,
    }
}

pub fn resume(session_id: &str) -> String {
    format!("opencode --session {}", crate::ssh::quote(session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("valid test fixture JSON")
    }

    #[test]
    fn adapt_session_status_busy_is_running() {
        let ev = adapt(&raw(r#"{"type":"session.status","properties":{"status":"busy","sessionID":"s1"}}"#))
            .unwrap();
        assert_eq!(ev.kind, EventKind::PromptSubmit);
        assert_eq!(ev.status, Status::Running);
        assert_eq!(ev.session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn adapt_session_status_non_busy_is_ignored() {
        assert_eq!(adapt(&raw(r#"{"type":"session.status","properties":{"status":"idle"}}"#)), None);
        assert_eq!(adapt(&raw(r#"{"type":"session.status","properties":{}}"#)), None);
    }

    #[test]
    fn adapt_message_updated_from_user_is_running() {
        let ev = adapt(&raw(r#"{"type":"message.updated","properties":{"info":{"role":"user","id":"s2"}}}"#))
            .unwrap();
        assert_eq!(ev.status, Status::Running);
        assert_eq!(ev.session_id.as_deref(), Some("s2"));
    }

    #[test]
    fn adapt_message_updated_from_assistant_is_ignored() {
        assert_eq!(
            adapt(&raw(r#"{"type":"message.updated","properties":{"info":{"role":"assistant"}}}"#)),
            None
        );
    }

    #[test]
    fn adapt_session_id_falls_back_to_properties_info_id() {
        let ev = adapt(&raw(r#"{"type":"session.idle","properties":{"info":{"id":"s3"}}}"#)).unwrap();
        assert_eq!(ev.session_id.as_deref(), Some("s3"));
    }

    #[test]
    fn adapt_permission_events_need_input() {
        let ev = adapt(&raw(r#"{"type":"permission.asked","properties":{"title":"Run rm -rf?"}}"#)).unwrap();
        assert_eq!(ev.kind, EventKind::PermissionRequest);
        assert_eq!(ev.status, Status::NeedsInput);
        assert_eq!(ev.message.as_deref(), Some("Run rm -rf?"));
        assert!(ev.notifies());

        let ev = adapt(&raw(r#"{"type":"permission.updated","properties":{}}"#)).unwrap();
        assert_eq!(ev.message, None);
    }

    #[test]
    fn adapt_session_idle_and_deleted() {
        let ev = adapt(&raw(r#"{"type":"session.idle","properties":{"sessionID":"s4"}}"#)).unwrap();
        assert_eq!(ev.kind, EventKind::Stop);
        assert_eq!(ev.status, Status::Idle);

        let ev = adapt(&raw(r#"{"type":"session.deleted","properties":{"sessionID":"s4"}}"#)).unwrap();
        assert_eq!(ev.kind, EventKind::SessionEnd);
        assert_eq!(ev.status, Status::None);
    }

    #[test]
    fn adapt_unknown_or_malformed_is_ignored() {
        assert_eq!(adapt(&raw(r#"{"type":"something.else"}"#)), None);
        for v in [serde_json::Value::Null, json!(1), json!([1]), json!("x"), json!({"type": 5})] {
            assert_eq!(adapt(&v), None);
        }
    }

    #[test]
    fn config_path_is_the_opencode_plugin_file() {
        let home = Path::new("/home/u"); // portability: allow
        assert_eq!(config_path(home), Path::new("/home/u/.config/opencode/plugin/amalgum.js")); // portability: allow
    }

    #[test]
    fn resume_quotes_when_needed() {
        assert_eq!(resume("abc"), "opencode --session abc");
        assert_eq!(resume("has space"), "opencode --session 'has space'");
    }

    #[test]
    fn install_embeds_the_marker_and_command_and_is_plain_js() {
        let cmd = "'/opt/has space/amalgum' agent-event --agent opencode --hook-version 1";
        let out = install("", cmd).unwrap();
        assert!(out.starts_with("// amalgum hooks v1 — generated"));
        assert!(out.contains("export const Amalgum"));
        assert!(out.contains("${JSON.stringify(event)}"));
        assert!(out.contains(cmd));
        assert!(out.contains(".nothrow().quiet()"));
        // No stray unmatched braces (a cheap sanity check the template is well formed).
        assert_eq!(out.matches('{').count(), out.matches('}').count());
    }

    #[test]
    fn install_ignores_prior_text_and_is_idempotent() {
        let cmd = "amalgum agent-event --agent opencode --hook-version 1";
        let once = install("garbage", cmd).unwrap();
        let twice = install(&once, cmd).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn uninstall_returns_empty_string() {
        assert_eq!(uninstall("anything").unwrap(), "");
    }

    #[test]
    fn state_transitions() {
        let cmd = "amalgum agent-event --agent opencode --hook-version 1";
        assert_eq!(state("", cmd), HookState::Missing);
        assert_eq!(state("// not ours\n", cmd), HookState::Missing);

        let installed = install("", cmd).unwrap();
        assert_eq!(state(&installed, cmd), HookState::Installed);

        let other_cmd = "amalgum agent-event --agent opencode --hook-version 2";
        assert_eq!(state(&installed, other_cmd), HookState::Outdated);

        // Marker present but hand-edited body: outdated.
        let mut edited = installed.clone();
        edited.push_str("// tampered\n");
        assert_eq!(state(&edited, cmd), HookState::Outdated);
    }
}
