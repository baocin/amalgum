//! OpenAI Codex CLI. `~/.codex/config.toml` top-level `notify = ["prog", "arg", …]`; Codex
//! runs it with the event JSON appended as the final argv element (not stdin), e.g.
//! `{"type":"agent-turn-complete","thread-id":"…","last-assistant-message":"…"}`.
//! We edit the TOML as text so comments and formatting survive: our key sits between
//! `# >>> amalgum hooks >>>` and `# <<< amalgum hooks <<<` at the top of the file (a top-level
//! key must precede any `[table]`). A pre-existing `notify` that is not ours is a
//! `HookState::Conflict` and is never overwritten.
//! Mapping: agent-turn-complete → Idle, or NeedsInput if the message ends with `?`.
//! Resume: `codex resume <id>`.

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
