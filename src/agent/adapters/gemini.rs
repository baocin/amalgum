//! Gemini CLI. `~/.gemini/settings.json` `"hooks"` with the same shape as Claude Code's
//! (`{"hooks":{"<Event>":[{"matcher":"","hooks":[{"type":"command","command":"…"}]}]}}`).
//! Payloads carry `hook_event_name`, `session_id`, `cwd`. Events we install: SessionStart,
//! BeforeAgent (→ Running), Notification (→ NeedsInput), AfterAgent (→ Idle, NeedsInput if the
//! response ends with `?`), SessionEnd (→ Status::None). Marker: command contains
//! `agent-event --agent gemini`. Resume: `gemini --resume <id>`.
//! Gemini's hook schema is young; keep this file the only place that knows it.

use super::{AgentEvent, HookState};
use std::path::{Path, PathBuf};

pub fn adapt(raw: &serde_json::Value) -> Option<AgentEvent> {
    todo!()
}
pub fn config_path(home: &Path) -> PathBuf {
    todo!()
}
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
