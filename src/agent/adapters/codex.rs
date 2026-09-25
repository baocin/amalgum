//! OpenAI Codex CLI. `~/.codex/config.toml` top-level `notify = ["prog", "arg", …]`; Codex
//! runs it with the event JSON appended as the final argv element (not stdin), e.g.
//! `{"type":"agent-turn-complete","thread-id":"…","last-assistant-message":"…"}`.
//! We edit the TOML as text so comments and formatting survive: our key sits between
//! `# >>> amalgum hooks >>>` and `# <<< amalgum hooks <<<` at the top of the file (a top-level
//! key must precede any `[table]`). A pre-existing `notify` that is not ours is a
//! `HookState::Conflict` and is never overwritten.
//! Mapping: agent-turn-complete → Idle, or NeedsInput if the message ends with `?`.
//! Resume: `codex resume <id>`.

use super::{AgentEvent, AgentKind, EventKind, HookState, Status};
use std::path::{Path, PathBuf};

const BEGIN_MARKER: &str = "# >>> amalgum hooks >>>";
const END_MARKER: &str = "# <<< amalgum hooks <<<";

pub fn adapt(raw: &serde_json::Value) -> Option<AgentEvent> {
    let ty = super::str_field(raw, "type")?;
    if ty != "agent-turn-complete" {
        return None;
    }
    let session_id = super::str_field(raw, "thread-id").or_else(|| super::str_field(raw, "session_id"));
    let pid = super::u32_field(raw, "pid");
    let cwd = super::str_field(raw, "cwd");
    let last = super::str_field(raw, "last-assistant-message");

    let (status, message) = match &last {
        Some(text) if text.trim().ends_with('?') => (Status::NeedsInput, Some(text.trim().to_string())),
        _ => (Status::Idle, None),
    };

    Some(AgentEvent { agent: AgentKind::Codex, kind: EventKind::Stop, status, message, session_id, pid, cwd })
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join(".codex").join("config.toml")
}

pub fn install(text: &str, command: &str) -> Result<String, String> {
    let rest = strip_block(text);
    if has_conflicting_notify(&rest) {
        return Err("notify already set in config.toml".to_string());
    }
    let block = build_block(command);
    let combined = if rest.trim().is_empty() { block } else { format!("{block}\n{rest}") };
    Ok(combined)
}

pub fn uninstall(text: &str) -> Result<String, String> {
    Ok(strip_block(text))
}

pub fn state(text: &str, command: &str) -> HookState {
    let rest = strip_block(text);
    if has_conflicting_notify(&rest) {
        return HookState::Conflict("notify already set in config.toml".to_string());
    }
    if !text.contains(BEGIN_MARKER) {
        return HookState::Missing;
    }
    match install(&rest, command) {
        Ok(expected) if expected == text => HookState::Installed,
        _ => HookState::Outdated,
    }
}

pub fn resume(session_id: &str) -> String {
    format!("codex resume {}", super::quote_word(session_id))
}

/// Build the marker-delimited block: `notify = [...]` as a TOML array of strings, one per argv
/// word of `command` (the shell command [`super::hook_command`] produces).
fn build_block(command: &str) -> String {
    let array = argv(command).iter().map(|s| toml_string(s)).collect::<Vec<_>>().join(", ");
    format!("{BEGIN_MARKER}\nnotify = [{array}]\n{END_MARKER}\n")
}

/// Remove our block (and one blank separator line after it) if present; otherwise return `text`
/// unchanged. Position-independent so it also cleans up a block a user moved.
fn strip_block(text: &str) -> String {
    let Some(start) = text.find(BEGIN_MARKER) else {
        return text.to_string();
    };
    let Some(end_rel) = text[start..].find(END_MARKER) else {
        return text.to_string();
    };
    let end = start + end_rel + END_MARKER.len();
    let mut after = &text[end..];
    after = after.strip_prefix('\n').unwrap_or(after);
    after = after.strip_prefix('\n').unwrap_or(after);
    let before = &text[..start];
    format!("{before}{after}")
}

/// Does `text` (assumed to already have our block stripped) contain a top-level `notify =` line
/// that isn't ours? Top-level means before the first `[table]` header.
fn has_conflicting_notify(text: &str) -> bool {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("notify")
            && rest.trim_start().starts_with('=')
        {
            return true;
        }
    }
    false
}

/// Split a command string produced by [`super::hook_command`] into argv, undoing the POSIX
/// single-quoting [`super::quote_word`] applies. Not a general shell parser — only handles our
/// own output: a run of bare words, and/or one word wrapped in `'...'` with embedded `'`
/// escaped as `'\''`.
fn argv(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut in_squote = false;
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        if in_squote {
            if c == '\'' {
                in_squote = false;
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_squote = true;
                in_word = true;
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                    in_word = true;
                }
            }
            _ => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

/// Escape a string as a TOML basic (double-quoted) string.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("valid test fixture JSON")
    }

    #[test]
    fn adapt_agent_turn_complete_idle_by_default() {
        let ev = adapt(&raw(
            r#"{"type":"agent-turn-complete","thread-id":"t1","last-assistant-message":"Done."}"#,
        ))
        .unwrap();
        assert_eq!(ev.kind, EventKind::Stop);
        assert_eq!(ev.status, Status::Idle);
        assert_eq!(ev.session_id.as_deref(), Some("t1"));
        assert_eq!(ev.message, None);
    }

    #[test]
    fn adapt_agent_turn_complete_needs_input_on_question() {
        let ev =
            adapt(&raw(r#"{"type":"agent-turn-complete","last-assistant-message":"Proceed? "}"#)).unwrap();
        assert_eq!(ev.status, Status::NeedsInput);
        assert_eq!(ev.message.as_deref(), Some("Proceed?"));
        assert!(ev.notifies());
    }

    #[test]
    fn adapt_unknown_type_or_malformed_is_ignored() {
        assert_eq!(adapt(&raw(r#"{"type":"session-configured"}"#)), None);
        assert_eq!(adapt(&raw(r#"{}"#)), None);
        for v in [serde_json::Value::Null, json!(1), json!([1]), json!("x"), json!({"type": 5})] {
            assert_eq!(adapt(&v), None);
        }
    }

    #[test]
    fn config_path_is_under_dot_codex() {
        let home = Path::new("/home/u"); // portability: allow
        assert_eq!(config_path(home), Path::new("/home/u/.codex/config.toml")); // portability: allow
    }

    #[test]
    fn resume_quotes_when_needed() {
        assert_eq!(resume("abc"), "codex resume abc");
        assert_eq!(resume("has space"), "codex resume 'has space'");
    }

    #[test]
    fn argv_round_trips_hook_command_output() {
        let cmd = super::super::hook_command("amalgum", AgentKind::Codex);
        assert_eq!(argv(&cmd), vec!["amalgum", "agent-event", "--agent", "codex", "--hook-version", "1"]);

        let cmd = super::super::hook_command("/opt/has space/amalgum", AgentKind::Codex); // portability: allow
        assert_eq!(
            argv(&cmd),
            vec!["/opt/has space/amalgum", "agent-event", "--agent", "codex", "--hook-version", "1"] // portability: allow
        );

        // A single embedded quote in the exe path.
        let cmd = super::super::hook_command("it's/amalgum", AgentKind::Codex);
        assert_eq!(
            argv(&cmd),
            vec!["it's/amalgum", "agent-event", "--agent", "codex", "--hook-version", "1"]
        );
    }

    #[test]
    fn install_writes_a_notify_array_that_parses_as_toml() {
        let cmd = "'/opt/has space/amalgum' agent-event --agent codex --hook-version 1"; // portability: allow
        let out = install("", cmd).unwrap();
        assert!(out.starts_with(BEGIN_MARKER));
        assert!(out.contains(END_MARKER));

        let parsed: toml::Value = toml::from_str(&out).expect("valid toml");
        let notify = parsed.get("notify").and_then(toml::Value::as_array).expect("notify array");
        let strings: Vec<&str> = notify.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(
            strings,
            vec!["/opt/has space/amalgum", "agent-event", "--agent", "codex", "--hook-version", "1"] // portability: allow
        );
    }

    #[test]
    fn install_is_idempotent() {
        let cmd = "amalgum agent-event --agent codex --hook-version 1";
        let once = install("", cmd).unwrap();
        let twice = install(&once, cmd).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn install_inserts_at_top_and_preserves_rest() {
        let cmd = "amalgum agent-event --agent codex --hook-version 1";
        let rest = "model = \"o1\"\n[history]\npersistence = \"save-all\"\n";
        let out = install(rest, cmd).unwrap();
        assert!(out.starts_with(BEGIN_MARKER));
        assert!(out.ends_with(rest));

        // Uninstalling gives back exactly the original rest, byte for byte.
        assert_eq!(uninstall(&out).unwrap(), rest);
    }

    #[test]
    fn install_rejects_conflicting_notify() {
        let existing = "notify = [\"other-tool\"]\n";
        let err = install(existing, "cmd").unwrap_err();
        assert!(err.contains("notify"));
    }

    #[test]
    fn install_does_not_conflict_with_notify_under_a_table() {
        // Not top-level: shouldn't trip conflict detection.
        let existing = "[some_table]\nnotify = \"nested\"\n";
        assert!(install(existing, "amalgum agent-event --agent codex --hook-version 1").is_ok());
    }

    #[test]
    fn uninstall_of_untouched_config_is_a_noop() {
        let rest = "model = \"o1\"\n";
        assert_eq!(uninstall(rest).unwrap(), rest);
    }

    #[test]
    fn uninstall_of_empty_text_is_empty() {
        assert_eq!(uninstall("").unwrap(), "");
    }

    #[test]
    fn state_missing_then_installed_then_outdated_then_conflict() {
        let cmd = "amalgum agent-event --agent codex --hook-version 1";
        assert_eq!(state("", cmd), HookState::Missing);

        let installed = install("", cmd).unwrap();
        assert_eq!(state(&installed, cmd), HookState::Installed);

        let old_cmd = "amalgum agent-event --agent codex --hook-version 0";
        let outdated = install("", old_cmd).unwrap();
        assert_eq!(state(&outdated, cmd), HookState::Outdated);

        let conflicting = "notify = [\"other-tool\"]\n";
        assert!(matches!(state(conflicting, cmd), HookState::Conflict(_)));
    }
}
