//! One file per agent (§12 risk 3: "one adapter file per agent, versioned"). Everything that
//! depends on an agent's hook schema, config format, or CLI flags lives in that agent's file:
//!
//! - `adapt(raw)`: hook payload → [`AgentEvent`] (or `None` for events we ignore)
//! - `config_path(home)`: the agent's config file we merge into
//! - `install(text, command)`: pure text transform adding our marker-keyed hook block
//!   (idempotent: re-running replaces the block in place; the user's other content survives)
//! - `uninstall(text)`: pure text transform removing exactly our block
//! - `state(text, command)`: [`HookState`] for `amalgum hooks status`
//! - `resume(session_id)`: the shell command that resumes a session (§5.30)
//! - `CLI_OPTIONS`: its long options, what each takes, and which a resume never replays (§5.30)
//!
//! This module does the file I/O around those pure functions: read (missing file = empty),
//! write atomically (temp file + rename, preserving permissions, through symlinks), create
//! parent dirs. Tests must pass a temp `home`; nothing here may touch the real `$HOME` under test.

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;

use super::AgentKind;
use super::status::Status;
use serde::Serialize;
use serde_json::{Map, Value};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Bump when the installed hook block changes; older blocks then report `Outdated`.
pub const HOOK_VERSION: u32 = 1;

/// Hook events in agent-neutral terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    SessionStart,
    PromptSubmit,
    Notification,
    PermissionRequest,
    Stop,
    SessionEnd,
}

/// What one hook payload means for its tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEvent {
    pub agent: AgentKind,
    pub kind: EventKind,
    /// `Status::None` for `SessionEnd` (no agent any more).
    pub status: Status,
    /// The agent's own text, for the sidebar line and notification row.
    pub message: Option<String>,
    pub session_id: Option<String>,
    pub pid: Option<u32>,
    pub cwd: Option<String>,
}

impl AgentEvent {
    /// Whether this event creates a notification row (§5.29: needs-input transitions and
    /// every hook `Notification`).
    pub fn notifies(&self) -> bool {
        matches!(self.kind, EventKind::Notification | EventKind::PermissionRequest)
            || self.status == Status::NeedsInput
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookState {
    Installed,
    /// Our block is present but differs from what `install` would write now.
    Outdated,
    /// The agent's config exists (or the agent is on this machine) but has no block of ours.
    Missing,
    /// Something else already occupies the slot we need (e.g. Codex `notify`); left untouched.
    Conflict(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReport {
    pub agent: AgentKind,
    pub state: HookState,
    /// The agent's config file. For `setup` and `remove` that is the file they write: through
    /// a symlinked config, the file the link points at.
    pub path: PathBuf,
}

/// Map one raw hook payload. Must not panic on any JSON value.
pub fn adapt(agent: AgentKind, raw: &serde_json::Value) -> Option<AgentEvent> {
    match agent {
        AgentKind::Claude => claude::adapt(raw),
        AgentKind::Codex => codex::adapt(raw),
        AgentKind::Opencode => opencode::adapt(raw),
        AgentKind::Gemini => gemini::adapt(raw),
    }
}

/// The command each hook runs: `<exe> agent-event --agent <name> --hook-version <N>`.
/// `exe` is shell-quoted if it contains anything but `[A-Za-z0-9_./~-]`.
pub fn hook_command(exe: &str, agent: AgentKind) -> String {
    format!("{} agent-event --agent {} --hook-version {}", crate::ssh::quote(exe), agent.name(), HOOK_VERSION)
}

/// Install or update our hook block for `agent` under `home`. A config already as it would be
/// written is left alone.
pub fn setup(agent: AgentKind, home: &Path, exe: &str) -> io::Result<HookReport> {
    let path = resolve_symlinks(&config_path_for(agent, home))?;
    let text = read_to_string_opt(&path)?.unwrap_or_default();
    let command = hook_command(exe, agent);

    // A pre-existing conflict (e.g. Codex `notify` we don't own) is reported, not overwritten.
    if let HookState::Conflict(reason) = state_for(agent, &text, &command) {
        return Ok(HookReport { agent, state: HookState::Conflict(reason), path });
    }

    let new_text =
        install_for(agent, &text, &command).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if new_text != text {
        write_atomic(&path, &new_text)?;
    }

    let state = state_for(agent, &new_text, &command);
    Ok(HookReport { agent, state, path })
}

pub fn status(agent: AgentKind, home: &Path, exe: &str) -> io::Result<HookReport> {
    let path = config_path_for(agent, home);
    let text = read_to_string_opt(&path)?.unwrap_or_default();
    let command = hook_command(exe, agent);
    Ok(HookReport { agent, state: state_for(agent, &text, &command), path })
}

/// Remove our block; leaves the rest of the file intact. JSON configs keep their key order and
/// indentation, though other formatting (an inline array, say) comes back pretty-printed; a
/// config with nothing of ours in it is not rewritten at all.
pub fn remove(agent: AgentKind, home: &Path) -> io::Result<HookReport> {
    let path = config_path_for(agent, home);

    // OpenCode: we own the whole file, so removal deletes it outright.
    if agent == AgentKind::Opencode {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        return Ok(HookReport { agent, state: HookState::Missing, path });
    }

    let path = resolve_symlinks(&path)?;
    let Some(text) = read_to_string_opt(&path)? else {
        return Ok(HookReport { agent, state: HookState::Missing, path });
    };

    let new_text = uninstall_for(agent, &text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if new_text != text {
        write_atomic(&path, &new_text)?;
    }

    // No `exe` here to rebuild the exact expected command; conflict/missing detection doesn't
    // need it, and after a clean uninstall we never land on `Installed`/`Outdated` anyway.
    let state = state_for(agent, &new_text, "");
    Ok(HookReport { agent, state, path })
}

/// Shell command that resumes `session_id` (§5.30), e.g. `claude --resume <id>`.
pub fn resume_command(agent: AgentKind, session_id: &str) -> String {
    match agent {
        AgentKind::Claude => claude::resume(session_id),
        AgentKind::Codex => codex::resume(session_id),
        AgentKind::Opencode => opencode::resume(session_id),
        AgentKind::Gemini => gemini::resume(session_id),
    }
}

/// An agent's CLI long options, for replaying its original argv on resume (§5.30).
#[derive(Debug)]
pub struct CliOptions {
    /// The long options of the agent's interactive command, as its `--help` lists them, with the
    /// words each takes. Only these are replayed: an option missing here (a newer CLI's, a typo)
    /// is dropped, and so is the word after it unless that is an option, since without knowing
    /// whether it takes a value, replaying it could leave it without one and the agent would
    /// reject the whole line.
    pub options: &'static [(&'static str, Takes)],
    /// Listed options never replayed, value and all: they pick or start the session (the resume
    /// command picks its own), or submit a prompt the session has already run.
    pub not_replayed: &'static [&'static str],
}

/// The argv words a long option takes after it (`--name=value` carries its value inline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Takes {
    /// None: a switch (`--verbose`).
    Nothing,
    /// The next word, whatever it looks like (`--model opus`, `--append-system-prompt "-x"`).
    One,
    /// The next word unless it is an option (`--debug api`, or a bare `--debug`).
    Optional,
    /// The next word, then every word after it up to the next option (`--add-dir a b`).
    Many,
}

impl CliOptions {
    /// What `option` (a `--name`, without any `=value`) takes; `None` for an unlisted option.
    pub fn takes(&self, option: &str) -> Option<Takes> {
        self.options.iter().find(|(name, _)| *name == option).map(|&(_, takes)| takes)
    }
}

/// `agent`'s [`CliOptions`], from its adapter file.
pub fn cli_options(agent: AgentKind) -> &'static CliOptions {
    match agent {
        AgentKind::Claude => &claude::CLI_OPTIONS,
        AgentKind::Codex => &codex::CLI_OPTIONS,
        AgentKind::Opencode => &opencode::CLI_OPTIONS,
        AgentKind::Gemini => &gemini::CLI_OPTIONS,
    }
}

// --- per-agent dispatch -----------------------------------------------------------------

fn config_path_for(agent: AgentKind, home: &Path) -> PathBuf {
    match agent {
        AgentKind::Claude => claude::config_path(home),
        AgentKind::Codex => codex::config_path(home),
        AgentKind::Opencode => opencode::config_path(home),
        AgentKind::Gemini => gemini::config_path(home),
    }
}

fn install_for(agent: AgentKind, text: &str, command: &str) -> Result<String, String> {
    match agent {
        AgentKind::Claude => claude::install(text, command),
        AgentKind::Codex => codex::install(text, command),
        AgentKind::Opencode => opencode::install(text, command),
        AgentKind::Gemini => gemini::install(text, command),
    }
}

fn uninstall_for(agent: AgentKind, text: &str) -> Result<String, String> {
    match agent {
        AgentKind::Claude => claude::uninstall(text),
        AgentKind::Codex => codex::uninstall(text),
        AgentKind::Opencode => opencode::uninstall(text),
        AgentKind::Gemini => gemini::uninstall(text),
    }
}

fn state_for(agent: AgentKind, text: &str, command: &str) -> HookState {
    match agent {
        AgentKind::Claude => claude::state(text, command),
        AgentKind::Codex => codex::state(text, command),
        AgentKind::Opencode => opencode::state(text, command),
        AgentKind::Gemini => gemini::state(text, command),
    }
}

// --- I/O helpers -------------------------------------------------------------------------

/// Read a file to a string; a missing file is `None` rather than an error (callers that just
/// want "missing = empty" use `.unwrap_or_default()`; `remove` needs to tell the two apart).
fn read_to_string_opt(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write `contents` atomically: a temp file in the same directory, then rename over `path`.
/// Creates parent directories if needed and preserves the target's existing permissions.
/// A symlinked `path` (dotfiles managers link agent configs into a repo) is written through:
/// the link stays, and the file it points at is the one replaced. The temp file is never more
/// readable than the target, not even before its permissions are copied (a 0600 config may
/// hold API keys), and it is removed again if anything fails. An error names the file being
/// replaced: through a link, its target (home-manager's, say, in the read-only Nix store).
fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let path = resolve_symlinks(path)?;
    replace_file(&path, contents).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

/// [`write_atomic`] once `path` is no symlink.
fn replace_file(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "config path has no file name"))?;
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(format!(".amalgum-tmp-{}", std::process::id()));
    let tmp_path = path.with_file_name(tmp_name);

    let perms = fs::metadata(path).ok().map(|meta| meta.permissions());
    // A new file gets the usual 0666 less the umask.
    let mode = perms.as_ref().map_or(0o666, |p| p.mode() & 0o777);
    // A temp file left by a killed run with our pid would make `create_new` fail.
    let _ = fs::remove_file(&tmp_path);
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp_path)?;
    let result = file
        .write_all(contents.as_bytes())
        // Exact bits: the umask may have cleared some of `mode`.
        .and_then(|()| perms.map_or(Ok(()), |perms| file.set_permissions(perms)))
        .and_then(|()| fs::rename(&tmp_path, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

/// Follow `path` through any symlinks to the file they finally name, which need not exist yet
/// (a dangling link names the file to create). Anything that isn't a link is returned as is.
fn resolve_symlinks(path: &Path) -> io::Result<PathBuf> {
    let mut path = path.to_path_buf();
    // Linux's MAXSYMLINKS: past it, a link loop.
    for _ in 0..40 {
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&path)?;
                // A relative target is relative to the link's own directory.
                path = match path.parent() {
                    Some(dir) => dir.join(target),
                    None => target,
                };
            }
            _ => return Ok(path),
        }
    }
    Err(io::Error::other(format!("{}: too many levels of symbolic links", path.display())))
}

// --- shared JSON hook-block plumbing (Claude/Gemini share this shape) --------------------

/// Marker substring identifying our hook entries for `agent_name` (present in every command
/// we generate, quoted or not: only the executable is ever quoted, never the flags).
fn marker(agent_name: &str) -> String {
    format!("agent-event --agent {agent_name}")
}

/// Remove any hook objects (inside each matcher-group of one event's array) whose `command`
/// contains `marker`; a group left with no hooks is dropped entirely, others are kept as-is.
fn strip_marker_groups(arr: &[Value], marker: &str) -> Vec<Value> {
    arr.iter()
        .cloned()
        .filter_map(|mut group| {
            let Some(Value::Array(inner)) = group.get("hooks").cloned() else {
                return Some(group); // not our shape; leave it alone
            };
            let kept: Vec<Value> = inner
                .into_iter()
                .filter(|h| !h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains(marker)))
                .collect();
            if kept.is_empty() {
                return None;
            }
            if let Value::Object(map) = &mut group {
                map.insert("hooks".to_string(), Value::Array(kept));
            }
            Some(group)
        })
        .collect()
}

/// Shared installer for Claude/Gemini-shaped hook configs:
/// `{"hooks":{"<Event>":[{"matcher":"","hooks":[{"type":"command","command":…,"timeout":…}]}]}}`.
fn json_install(
    text: &str,
    command: &str,
    agent_name: &str,
    events: &[&str],
    timeout: u64,
) -> Result<String, String> {
    let trimmed = text.trim();
    let mut root: Value = if trimmed.is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?
    };
    let Value::Object(root_map) = &mut root else {
        return Err("config root is not a JSON object".to_string());
    };
    let hooks_val = root_map.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(hooks_map) = hooks_val else {
        return Err("\"hooks\" is not a JSON object".to_string());
    };

    let marker = marker(agent_name);
    let entry = serde_json::json!({
        "matcher": "",
        "hooks": [{"type": "command", "command": command, "timeout": timeout}],
    });

    for &event in events {
        let existing = hooks_map.get(event).and_then(Value::as_array).cloned().unwrap_or_default();
        let mut filtered = strip_marker_groups(&existing, &marker);
        filtered.push(entry.clone());
        hooks_map.insert(event.to_string(), Value::Array(filtered));
    }

    to_json_like(&root, text)
}

/// Shared uninstaller matching [`json_install`]'s shape.
fn json_uninstall(text: &str, agent_name: &str, events: &[&str]) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(text.to_string());
    }
    let mut root: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let Value::Object(root_map) = &mut root else {
        return Err("config root is not a JSON object".to_string());
    };
    let marker = marker(agent_name);

    let mut removed = false;
    if let Some(Value::Object(hooks_map)) = root_map.get_mut("hooks") {
        let mut remove_keys = Vec::new();
        for &event in events {
            let Some(Value::Array(existing)) = hooks_map.get(event) else { continue };
            let filtered = strip_marker_groups(existing, &marker);
            removed |= filtered != *existing;
            if filtered.is_empty() {
                remove_keys.push(event.to_string());
            } else {
                hooks_map.insert(event.to_string(), Value::Array(filtered));
            }
        }
        for k in remove_keys {
            hooks_map.shift_remove(&k);
        }
        if hooks_map.is_empty() {
            root_map.shift_remove("hooks");
        }
    }

    // Nothing of ours: the file stays exactly as it is, formatting and all.
    if !removed {
        return Ok(text.to_string());
    }
    to_json_like(&root, text)
}

/// Pretty-print `root` (keys in the order read, serde_json's `preserve_order`) indented like
/// `original`: by its first indented line's leading whitespace, or two spaces (the agents' own
/// style) for a new or one-line file. Key order and indentation are kept; the rest is
/// serde_json's layout (an inline array or object is spread over lines, a final newline added,
/// an escape such as `\/` written plainly), so only a file already in it comes back byte for byte.
fn to_json_like(root: &Value, original: &str) -> Result<String, String> {
    let indent = original
        .lines()
        .skip(1)
        .map(|line| &line[..line.len() - line.trim_start().len()])
        .find(|ws| !ws.is_empty())
        .unwrap_or("  ");
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    root.serialize(&mut serde_json::Serializer::with_formatter(&mut out, formatter))
        .map_err(|e| e.to_string())?;
    out.push(b'\n');
    String::from_utf8(out).map_err(|e| e.to_string())
}

/// Shared state check matching [`json_install`]'s shape: installed only when every owned event
/// carries exactly one of our hook entries and it equals `command` verbatim.
fn json_state(text: &str, command: &str, agent_name: &str, events: &[&str]) -> HookState {
    let marker = marker(agent_name);
    if !text.contains(&marker) {
        return HookState::Missing;
    }
    let Ok(root) = serde_json::from_str::<Value>(text) else {
        return HookState::Outdated;
    };
    let Some(hooks_map) = root.get("hooks").and_then(Value::as_object) else {
        return HookState::Outdated;
    };
    let all_match = events.iter().all(|event| {
        let cmds: Vec<&str> = hooks_map
            .get(*event)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|group| group.get("hooks"))
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|h| h.get("command"))
            .filter_map(Value::as_str)
            .filter(|c| c.contains(&marker))
            .collect();
        cmds.len() == 1 && cmds[0] == command
    });
    if all_match { HookState::Installed } else { HookState::Outdated }
}

// --- shared value/text helpers ------------------------------------------------------------

/// Best-effort string field: `None` for anything but a JSON string (never panics on the wrong
/// shape — arrays, numbers, `null`, or a non-object `v`).
fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(str::to_string)
}

/// Best-effort `u32` field: `None` for anything but a non-negative integer that fits.
fn u32_field(v: &serde_json::Value, key: &str) -> Option<u32> {
    v.get(key)?.as_u64().and_then(|n| u32::try_from(n).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn kind(agent: AgentKind, k: EventKind, status: Status) -> AgentEvent {
        AgentEvent { agent, kind: k, status, message: None, session_id: None, pid: None, cwd: None }
    }

    #[test]
    fn notifies_true_for_notification_and_permission_kinds() {
        assert!(kind(AgentKind::Claude, EventKind::Notification, Status::NeedsInput).notifies());
        assert!(kind(AgentKind::Claude, EventKind::PermissionRequest, Status::NeedsInput).notifies());
    }

    #[test]
    fn notifies_true_whenever_status_needs_input_regardless_of_kind() {
        assert!(kind(AgentKind::Claude, EventKind::Stop, Status::NeedsInput).notifies());
    }

    #[test]
    fn notifies_false_for_running_and_idle_non_notification_kinds() {
        assert!(!kind(AgentKind::Claude, EventKind::SessionStart, Status::Running).notifies());
        assert!(!kind(AgentKind::Claude, EventKind::Stop, Status::Idle).notifies());
        assert!(!kind(AgentKind::Claude, EventKind::SessionEnd, Status::None).notifies());
    }

    #[test]
    fn hook_command_format() {
        assert_eq!(
            hook_command("amalgum", AgentKind::Claude),
            "amalgum agent-event --agent claude --hook-version 1"
        );
        let exe = "/opt/has space/amalgum"; // portability: allow
        assert_eq!(
            hook_command(exe, AgentKind::Gemini),
            "'/opt/has space/amalgum' agent-event --agent gemini --hook-version 1" // portability: allow
        );
    }

    #[test]
    fn adapt_dispatches_by_agent() {
        let raw: serde_json::Value = serde_json::from_str(r#"{"hook_event_name":"SessionEnd"}"#).unwrap();
        let ev = adapt(AgentKind::Claude, &raw).expect("SessionEnd is a recognised Claude event");
        assert_eq!(ev.agent, AgentKind::Claude);
        assert_eq!(ev.kind, EventKind::SessionEnd);
    }

    #[test]
    fn adapt_never_panics_on_malformed_json() {
        for raw in
            [Value::Null, serde_json::json!(42), serde_json::json!([1, 2, 3]), serde_json::json!("oops")]
        {
            for agent in AgentKind::ALL {
                assert_eq!(adapt(agent, &raw), None);
            }
        }
    }

    #[test]
    fn resume_command_dispatches_by_agent() {
        assert_eq!(resume_command(AgentKind::Claude, "abc"), "claude --resume abc");
        assert_eq!(resume_command(AgentKind::Codex, "abc"), "codex resume abc");
        assert_eq!(resume_command(AgentKind::Opencode, "abc"), "opencode --session abc");
        assert_eq!(resume_command(AgentKind::Gemini, "abc"), "gemini --resume abc");
    }

    fn setup_status_remove_roundtrip(agent: AgentKind) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        let exe = "amalgum";

        let report = setup(agent, home, exe).expect("setup");
        assert_eq!(report.state, HookState::Installed, "{agent:?} should install cleanly into an empty home");

        let st = status(agent, home, exe).expect("status");
        assert_eq!(st.state, HookState::Installed);

        // Idempotent: installed bytes/content don't change on a second run.
        let path = st.path.clone();
        let before = fs::read_to_string(&path).ok();
        setup(agent, home, exe).expect("setup again");
        let after = fs::read_to_string(&path).ok();
        assert_eq!(before, after, "{agent:?} setup should be idempotent");

        remove(agent, home).expect("remove");
        let st = status(agent, home, exe).expect("status after remove");
        assert_eq!(st.state, HookState::Missing);
    }

    #[test]
    fn setup_status_remove_roundtrip_all_agents() {
        for agent in AgentKind::ALL {
            setup_status_remove_roundtrip(agent);
        }
    }

    #[test]
    fn status_on_missing_home_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        for agent in AgentKind::ALL {
            let report = status(agent, dir.path(), "amalgum").expect("status");
            assert_eq!(report.state, HookState::Missing);
        }
    }

    #[test]
    fn remove_on_missing_file_is_a_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        for agent in AgentKind::ALL {
            let report = remove(agent, dir.path()).expect("remove");
            assert_eq!(report.state, HookState::Missing);
            assert!(!report.path.exists());
        }
    }

    #[test]
    fn setup_preserves_unrelated_json_content_semantically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        let config_dir = home.join(".claude");
        fs::create_dir_all(&config_dir).expect("mkdir");
        fs::write(config_dir.join("settings.json"), r#"{"model":"opus","other":[1,2,3]}"#).expect("write");

        setup(AgentKind::Claude, home, "amalgum").expect("setup");
        remove(AgentKind::Claude, home).expect("remove");

        let text = fs::read_to_string(config_dir.join("settings.json")).expect("read back");
        let value: Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value.get("model").and_then(Value::as_str), Some("opus"));
        assert_eq!(value.get("other"), Some(&serde_json::json!([1, 2, 3])));
        assert!(value.get("hooks").is_none());
    }

    #[test]
    fn setup_reports_codex_conflict_without_overwriting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        let config_dir = home.join(".codex");
        fs::create_dir_all(&config_dir).expect("mkdir");
        let original = "notify = [\"other-tool\"]\n";
        fs::write(config_dir.join("config.toml"), original).expect("write");

        let report = setup(AgentKind::Codex, home, "amalgum").expect("setup");
        assert!(matches!(report.state, HookState::Conflict(_)));

        let text = fs::read_to_string(config_dir.join("config.toml")).expect("read back");
        assert_eq!(text, original, "a conflicting file must be left untouched");
    }

    #[test]
    fn codex_setup_and_remove_keep_what_codex_wrote_inside_an_older_block() {
        // An older install into an empty config.toml, then Codex's own toml_edit writes (its
        // model and a folder trust), which landed before our END marker (recorded output).
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        let path = config_path_for(AgentKind::Codex, home);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let codex_wrote = "model = \"gpt-5\"\n\n[projects.\"/work/proj\"]\ntrust_level = \"trusted\"\n";
        let notify = r#"notify = ["amalgum", "agent-event", "--agent", "codex", "--hook-version", "1"]"#;
        fs::write(
            &path,
            format!("# >>> amalgum hooks >>>\n{notify}\n{codex_wrote}# <<< amalgum hooks <<<\n"),
        )
        .expect("write");
        assert_eq!(status(AgentKind::Codex, home, "amalgum").expect("status").state, HookState::Outdated);

        assert_eq!(setup(AgentKind::Codex, home, "amalgum").expect("setup").state, HookState::Installed);
        let parsed: toml::Value = toml::from_str(&fs::read_to_string(&path).expect("read")).expect("toml");
        assert_eq!(parsed.get("model").and_then(toml::Value::as_str), Some("gpt-5"));
        let trust =
            parsed.get("projects").and_then(|p| p.get("/work/proj")).and_then(|p| p.get("trust_level"));
        assert_eq!(trust.and_then(toml::Value::as_str), Some("trusted"));

        remove(AgentKind::Codex, home).expect("remove");
        assert_eq!(fs::read_to_string(&path).expect("read"), codex_wrote);
    }

    #[test]
    fn write_atomic_preserves_permissions_and_creates_parent_dirs() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("file.json");
        write_atomic(&path, "{}\n").expect("first write creates parents");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("chmod");

        write_atomic(&path, "{\"a\":1}\n").expect("second write");
        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "existing permissions must survive an atomic rewrite");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\":1}\n");
    }

    #[test]
    fn setup_and_remove_write_through_a_symlinked_config() {
        // Dotfiles managers (stow, home-manager) link the agent's config into a repo; the link
        // must survive, and the repo's file must be the one that changes.
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let repo = dir.path().join("dotfiles");
        fs::create_dir_all(home.join(".claude")).expect("mkdir");
        fs::create_dir_all(repo.join("claude")).expect("mkdir");
        let target = repo.join("claude").join("settings.json");
        fs::write(&target, "{\n  \"model\": \"opus\"\n}\n").expect("write");
        let link = home.join(".claude").join("settings.json");
        std::os::unix::fs::symlink("../../dotfiles/claude/settings.json", &link).expect("symlink");

        let report = setup(AgentKind::Claude, &home, "amalgum").expect("setup");
        assert_eq!(report.state, HookState::Installed);
        assert!(fs::symlink_metadata(&link).expect("lstat").file_type().is_symlink(), "link replaced");
        // The report names the file written, not the link: the output says where it went.
        assert_eq!(fs::canonicalize(&report.path).ok(), fs::canonicalize(&target).ok());
        assert!(!fs::symlink_metadata(&report.path).expect("lstat").file_type().is_symlink());
        let installed = fs::read_to_string(&target).expect("read target");
        assert_eq!(
            claude::state(&installed, &hook_command("amalgum", AgentKind::Claude)),
            HookState::Installed
        );

        let report = remove(AgentKind::Claude, &home).expect("remove");
        assert_eq!(fs::canonicalize(&report.path).ok(), fs::canonicalize(&target).ok());
        assert!(fs::symlink_metadata(&link).expect("lstat").file_type().is_symlink(), "link replaced");
        assert_eq!(fs::read_to_string(&target).expect("read target"), "{\n  \"model\": \"opus\"\n}\n");
        let leftovers: Vec<_> = fs::read_dir(home.join(".claude")).expect("ls").collect();
        assert_eq!(leftovers.len(), 1, "only the link: {leftovers:?}");
    }

    #[test]
    fn a_failed_write_through_a_symlink_names_the_file_it_tried_to_replace() {
        // home-manager links into the read-only Nix store: the bare "Read-only file system"
        // must say which file. (A directory in the way fails the rename even for root.)
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("store").join("settings.json");
        fs::create_dir_all(&target).expect("mkdir");
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let err = write_atomic(&link, "{}\n").expect_err("a directory is in the way");
        assert!(err.to_string().contains(&target.display().to_string()), "{err}");
    }

    #[test]
    fn remove_leaves_a_json_config_with_nothing_of_ours_byte_for_byte() {
        // Hand-edited settings: inline arrays and objects, an escape, no final newline.
        let original = "{\n  \"permissions\": {\"allow\": [\"Bash(ls)\", \"Read\"]},\n  \"env\": {\"URL\": \"https:\\/\\/x\"}\n}";
        for agent in [AgentKind::Claude, AgentKind::Gemini] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = config_path_for(agent, dir.path());
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(&path, original).expect("write");
            assert_eq!(remove(agent, dir.path()).expect("remove").state, HookState::Missing);
            assert_eq!(fs::read_to_string(&path).expect("read"), original, "{agent:?}");
        }
    }

    #[test]
    fn setup_through_a_dangling_symlink_creates_its_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        fs::create_dir_all(home.join(".codex")).expect("mkdir");
        let target = home.join("config.toml");
        let link = home.join(".codex").join("config.toml");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        setup(AgentKind::Codex, home, "amalgum").expect("setup");
        assert!(fs::symlink_metadata(&link).expect("lstat").file_type().is_symlink(), "link replaced");
        assert!(fs::read_to_string(&target).expect("read target").contains("notify"));
    }

    #[test]
    fn write_atomic_through_a_symlink_loop_fails_and_keeps_the_link() {
        let dir = tempfile::tempdir().expect("tempdir");
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink("settings.json", &link).expect("symlink");
        assert!(write_atomic(&link, "{}\n").is_err());
        assert!(fs::symlink_metadata(&link).expect("lstat").file_type().is_symlink(), "link replaced");
    }

    #[test]
    fn write_atomic_leaves_no_temp_copy_behind_when_it_fails() {
        // A directory where the file should be makes the final rename fail; the temp copy (with
        // the whole config in it) must not stay behind.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::create_dir(&path).expect("mkdir");
        assert!(write_atomic(&path, "{\"env\":{\"API_KEY\":\"secret\"}}\n").is_err());
        let names: Vec<_> =
            fs::read_dir(dir.path()).expect("ls").map(|e| e.expect("entry").file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("settings.json")]);
    }

    #[test]
    fn write_atomic_replaces_a_stale_temp_file_from_an_earlier_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let stale = dir.path().join(format!("config.toml.amalgum-tmp-{}", std::process::id()));
        fs::write(&stale, "stale").expect("write");
        write_atomic(&path, "model = \"o3\"\n").expect("write_atomic");
        assert_eq!(fs::read_to_string(&path).expect("read"), "model = \"o3\"\n");
        assert!(!stale.exists());
    }

    #[test]
    fn setup_then_remove_restores_an_untouched_json_config_byte_for_byte() {
        // Key order and indentation are the user's (tracked in a dotfiles repo, a reordered file
        // is a whole-file diff); only our hook entries come and go.
        let two_spaces = "{\n  \"permissions\": {\n    \"deny\": [],\n    \"allow\": [\n      \"Bash(ls)\"\n    ]\n  },\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"PreToolUse\": [\n      {\n        \"matcher\": \"Bash\",\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"lint\"\n          }\n        ]\n      }\n    ]\n  },\n  \"env\": {\n    \"Z\": \"1\",\n    \"A\": \"2\"\n  }\n}\n";
        let four_spaces = "{\n    \"theme\": \"dark\",\n    \"env\": {\n        \"Z\": \"1\",\n        \"A\": \"2\"\n    }\n}\n";
        for agent in [AgentKind::Claude, AgentKind::Gemini] {
            for original in [two_spaces, four_spaces] {
                let dir = tempfile::tempdir().expect("tempdir");
                let path = config_path_for(agent, dir.path());
                fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
                fs::write(&path, original).expect("write");

                setup(agent, dir.path(), "amalgum").expect("setup");
                let installed = fs::read_to_string(&path).expect("read");
                let first_key = original.lines().nth(1).expect("a key line");
                assert_eq!(installed.lines().nth(1), Some(first_key), "{agent:?}: order or indent changed");

                remove(agent, dir.path()).expect("remove");
                assert_eq!(fs::read_to_string(&path).expect("read"), original, "{agent:?}");
            }
        }
    }

    #[test]
    fn cli_options_list_each_option_once_and_skip_only_listed_ones() {
        for agent in [AgentKind::Claude, AgentKind::Codex, AgentKind::Opencode, AgentKind::Gemini] {
            let table = cli_options(agent);
            for (i, (name, _)) in table.options.iter().enumerate() {
                assert!(name.starts_with("--") && !name.contains('='), "{agent:?}: {name}");
                assert!(
                    table.options[i + 1..].iter().all(|(other, _)| other != name),
                    "{agent:?}: {name} twice"
                );
            }
            for name in table.not_replayed {
                assert!(
                    table.takes(name).is_some(),
                    "{agent:?}: {name} is not listed, so its words are unknown"
                );
            }
        }
    }

    #[test]
    fn str_field_and_u32_field_never_panic_on_wrong_shapes() {
        for v in [Value::Null, serde_json::json!(1), serde_json::json!([1]), serde_json::json!("x")] {
            assert_eq!(str_field(&v, "k"), None);
            assert_eq!(u32_field(&v, "k"), None);
        }
        let obj = serde_json::json!({"s": "hi", "n": 5, "neg": -1, "arr": [1], "big": 9999999999u64});
        assert_eq!(str_field(&obj, "s"), Some("hi".to_string()));
        assert_eq!(str_field(&obj, "n"), None);
        assert_eq!(u32_field(&obj, "n"), Some(5));
        assert_eq!(u32_field(&obj, "neg"), None);
        assert_eq!(u32_field(&obj, "arr"), None);
        assert_eq!(u32_field(&obj, "big"), None);
        assert_eq!(u32_field(&obj, "missing"), None);
    }
}
