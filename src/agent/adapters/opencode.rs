//! OpenCode. We own a whole plugin file, `~/.config/opencode/plugin/amalgum.js`, which pipes
//! each bus event as JSON into `<command>` on stdin (`$` is Bun's shell; `.nothrow().quiet()`
//! so a missing app never blocks OpenCode). The file's first line is the marker
//! `// amalgum hooks v<HOOK_VERSION> — generated; edit via amalgum hooks setup`.
//! Payload: `{"type":"session.idle","properties":{"sessionID":"…"}}` etc.
//! Mapping: `session.status` busy / `message.updated` from the user → Running;
//! `permission.updated` / `permission.asked` → NeedsInput; `session.idle` → Idle;
//! `session.deleted` → Status::None. Resume: `opencode --session <id>`.
//! `install` returns the whole file; `uninstall` returns "" (the caller deletes the file).

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
