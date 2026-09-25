//! OpenAI Codex CLI. `~/.codex/config.toml` top-level `notify = ["prog", "arg", …]`; Codex
//! runs it with the event JSON appended as the final argv element (not stdin), e.g.
//! `{"type":"agent-turn-complete","thread-id":"…","last-assistant-message":"…"}`.
//! We edit the TOML as text so comments and formatting survive: our key sits at the top of the
//! file (a top-level key must precede any `[table]`), after a `# >>> amalgum hooks >>>` line
//! and carrying `# <<< amalgum hooks <<<` as its own trailing comment. The END marker must not
//! be a line of its own: Codex writes this file with toml_edit, which keeps a file's last
//! comment last, so what Codex adds to a file holding only our block would land inside it.
//! Removal takes out only the markers and our `notify` (whole, however many lines a formatter
//! spread it over), never other text found between them. A pre-existing `notify` that is not
//! ours is a `HookState::Conflict` and is never overwritten.
//! Mapping: agent-turn-complete → Idle, or NeedsInput if the message ends with `?`.
//! Resume: `codex resume <id>`.

use super::Takes::{Many, Nothing, One};
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
    // Current, at the very top (so `notify` is top-level), and the only block. What follows it
    // needs no blank separator: Codex appends its own keys straight after our line.
    match text.strip_prefix(build_block(command).as_str()) {
        Some(after) if !after.contains(BEGIN_MARKER) => HookState::Installed,
        _ => HookState::Outdated,
    }
}

pub fn resume(session_id: &str) -> String {
    format!("codex resume {}", crate::ssh::quote(session_id))
}

/// `codex --help` and `codex resume --help` (clap). The session is the `resume <id>`
/// subcommand (a positional, never replayed) or `resume --last`.
pub const CLI_OPTIONS: super::CliOptions = super::CliOptions {
    options: &[
        ("--add-dir", One),
        ("--all", Nothing),
        ("--ask-for-approval", One),
        ("--cd", One),
        ("--config", One),
        ("--dangerously-bypass-approvals-and-sandbox", Nothing),
        ("--disable", One),
        ("--enable", One),
        ("--full-auto", Nothing),
        ("--image", Many),
        ("--last", Nothing),
        ("--local-provider", One),
        ("--model", One),
        ("--no-alt-screen", Nothing),
        ("--oss", Nothing),
        ("--profile", One),
        ("--sandbox", One),
        ("--search", Nothing),
        ("--yolo", Nothing),
    ],
    // `--image` attaches files to the first prompt, which the session has already run.
    not_replayed: &["--all", "--image", "--last"],
};

/// Build the marker-delimited block: `notify = [...]` as a TOML array of strings, one per argv
/// word of `command` (the shell command [`super::hook_command`] produces).
fn build_block(command: &str) -> String {
    let array = argv(command).iter().map(|s| toml_string(s)).collect::<Vec<_>>().join(", ");
    format!("{BEGIN_MARKER}\nnotify = [{array}] {END_MARKER}\n")
}

/// Remove our block (and one blank separator line after it) if present; otherwise return `text`
/// unchanged. Only what is ours goes: the markers and our `notify` key-value, whole however many
/// lines a formatter or a hand edit spread it over. Anything else between the markers stays
/// where it is (the older layout, with the END marker on a line of its own, collected whatever
/// Codex added to the file). Position-independent so it also cleans up a block a user moved.
fn strip_block(text: &str) -> String {
    let Some(start) = text.find(BEGIN_MARKER) else {
        return text.to_string();
    };
    let Some(end_rel) = text[start..].find(END_MARKER) else {
        return text.to_string();
    };
    let end = start + end_rel + END_MARKER.len();
    let inside = &text[start + BEGIN_MARKER.len()..start + end_rel];
    let kept = without_notify(inside.strip_prefix('\n').unwrap_or(inside));
    let mut after = &text[end..];
    after = after.strip_prefix('\n').unwrap_or(after);
    after = after.strip_prefix('\n').unwrap_or(after);
    let before = &text[..start];
    format!("{before}{kept}{after}")
}

/// `text` without its first `notify = …` key-value: from the start of the key's line through
/// the end of the line its value ends on.
fn without_notify(text: &str) -> String {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if is_notify_line(line) {
            let len = key_value_len(&text[offset..]);
            return format!("{}{}", &text[..offset], &text[offset + len..]);
        }
        offset += line.len();
    }
    text.to_string()
}

/// The length of the `key = value` that starts `text`, through the newline ending the line its
/// value ends on (or all of `text`). A value may span lines, as an array a formatter wrapped
/// does: brackets are counted outside strings and comments.
fn key_value_len(text: &str) -> usize {
    let mut depth = 0usize;
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        match c {
            '\n' if depth == 0 => return i + 1,
            '[' | '{' => depth += 1,
            ']' | '}' => depth = depth.saturating_sub(1),
            '#' => {
                i += text[i..].find('\n').unwrap_or(text.len() - i);
                continue;
            }
            '"' | '\'' => {
                i += string_len(&text[i..]);
                continue;
            }
            _ => {}
        }
        i += c.len_utf8();
    }
    text.len()
}

/// The length of the TOML string that starts `text` at its opening quote, through its closing
/// quote. An unclosed one-line string ends before the newline, a multi-line one at the end.
fn string_len(text: &str) -> usize {
    let delim = [r#"""""#, "'''", "\"", "'"].into_iter().find(|d| text.starts_with(d)).unwrap_or("\"");
    let (basic, one_line) = (delim.starts_with('"'), delim.len() == 1);
    let mut i = delim.len();
    while let Some(c) = text[i..].chars().next() {
        if text[i..].starts_with(delim) {
            return i + delim.len();
        }
        if one_line && c == '\n' {
            return i;
        }
        i += c.len_utf8();
        // A basic string's backslash escapes the character after it (`\"`, `\\`).
        if basic && c == '\\' {
            i += text[i..].chars().next().map_or(0, char::len_utf8);
        }
    }
    text.len()
}

/// Does `text` (assumed to already have our block stripped) contain a top-level `notify =` line
/// that isn't ours? Top-level means before the first `[table]` header.
fn has_conflicting_notify(text: &str) -> bool {
    text.lines().take_while(|line| !line.trim_start().starts_with('[')).any(is_notify_line)
}

/// Is `line` a `notify = …` key?
fn is_notify_line(line: &str) -> bool {
    line.trim_start().strip_prefix("notify").is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// Split a command string produced by [`super::hook_command`] into argv, undoing the POSIX
/// single-quoting [`crate::ssh::quote`] applies. Not a general shell parser — only handles our
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

    const CMD: &str = "amalgum agent-event --agent codex --hook-version 1";
    /// Keys and tables Codex adds to its config through toml_edit: `/model` and a folder trust.
    const CODEX_ADDED: &str = "model = \"gpt-5\"\n\n[projects.\"/work/proj\"]\ntrust_level = \"trusted\"\n";

    #[test]
    fn what_codex_wrote_inside_an_older_block_survives_setup_and_remove() {
        // The older layout ended a file holding only our block with the END marker on a line of
        // its own. toml_edit keeps a file's last comment last, so what Codex added landed between
        // the markers (recorded from toml_edit 0.25).
        let notify = r#"notify = ["amalgum", "agent-event", "--agent", "codex", "--hook-version", "1"]"#;
        let text = format!("{BEGIN_MARKER}\n{notify}\n{CODEX_ADDED}{END_MARKER}\n");
        assert_eq!(uninstall(&text).unwrap(), CODEX_ADDED);

        let reinstalled = install(&text, CMD).unwrap();
        assert_eq!(reinstalled, format!("{}\n{CODEX_ADDED}", build_block(CMD)));
        assert_eq!(state(&reinstalled, CMD), HookState::Installed);
    }

    #[test]
    fn what_codex_adds_after_install_stays_outside_the_block() {
        let installed = install("", CMD).unwrap();
        let last_line = installed.lines().last().unwrap_or_default();
        assert!(
            last_line.starts_with("notify") && last_line.ends_with(END_MARKER),
            "the END marker must ride on the notify line, not be a trailing comment: {installed:?}"
        );
        // toml_edit adds keys after the last one and tables at the end (recorded from toml_edit
        // 0.25), which for this layout is after our block.
        let edited = format!("{installed}{CODEX_ADDED}");
        assert_eq!(state(&edited, CMD), HookState::Installed);
        assert_eq!(uninstall(&edited).unwrap(), CODEX_ADDED);
        let parsed: toml::Value = toml::from_str(&edited).expect("valid toml");
        assert_eq!(parsed.get("model").and_then(toml::Value::as_str), Some("gpt-5"));
        assert!(parsed.get("notify").is_some_and(toml::Value::is_array));
    }

    /// Our `notify` as a TOML formatter (taplo, format-on-save) writes an array wider than its
    /// column limit: one element per line. `exe` goes in as a TOML basic string.
    fn spread_notify(exe: &str) -> String {
        let words = ["agent-event", "--agent", "codex", "--hook-version", "1"];
        let elements: String =
            std::iter::once(exe).chain(words).map(|w| format!("  {},\n", toml_string(w))).collect();
        format!("notify = [\n{elements}]")
    }

    #[test]
    fn a_notify_spread_over_lines_goes_whole_and_leaves_valid_toml() {
        let rest = "model = \"o3\"\n";
        let notify = spread_notify("/home/u/.local/bin/amalgum"); // portability: allow
        let text = format!("{BEGIN_MARKER}\n{notify} {END_MARKER}\n\n{rest}");
        toml::from_str::<toml::Value>(&text).expect("the formatted file is valid toml");

        assert_eq!(uninstall(&text).unwrap(), rest);
        assert_eq!(state(&text, CMD), HookState::Outdated);
        let updated = install(&text, CMD).unwrap();
        assert_eq!(updated, format!("{}\n{rest}", build_block(CMD)));
        toml::from_str::<toml::Value>(&updated).expect("valid toml after update");
        assert_eq!(state(&updated, CMD), HookState::Installed);
    }

    #[test]
    fn a_spread_notify_in_an_older_block_goes_whole_and_codexs_keys_stay() {
        // Brackets, a `#` and an escaped quote inside the strings are not structure.
        let notify = spread_notify("/opt/a]b [#1] \"x\"/amalgum"); // portability: allow
        let text = format!("{BEGIN_MARKER}\n{notify}\n{CODEX_ADDED}{END_MARKER}\n");
        toml::from_str::<toml::Value>(&text).expect("the formatted file is valid toml");

        assert_eq!(uninstall(&text).unwrap(), CODEX_ADDED);
        let updated = install(&text, CMD).unwrap();
        assert_eq!(updated, format!("{}\n{CODEX_ADDED}", build_block(CMD)));
        toml::from_str::<toml::Value>(&updated).expect("valid toml after update");
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
