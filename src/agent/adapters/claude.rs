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

use super::{AgentEvent, HookState};
use std::path::{Path, PathBuf};

pub fn adapt(raw: &serde_json::Value) -> Option<AgentEvent> {
    todo!()
}
pub fn config_path(home: &Path) -> PathBuf {
    todo!()
}
/// Invalid existing JSON is an error (never clobber a file we cannot parse).
pub fn install(text: &str, command: &str) -> Result<String, String> {
    todo!()
}
pub fn uninstall(text: &str) -> Result<String, String> {
    todo!()
}
pub fn state(text: &str, command: &str) -> HookState {
    todo!()
}
pub fn resume(session_id: &str) -> String {
    todo!()
}
