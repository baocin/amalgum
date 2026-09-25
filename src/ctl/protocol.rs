//! Wire protocol between the CLI (and agent hooks) and the running app (§5.31).
//!
//! One connection per request: one line of JSON in, one line of JSON out, no ids, no auth
//! beyond socket permissions. Every request carries `v` (protocol version) and `ts` (Unix
//! seconds when the event happened, so queued remote events keep their real time).
//!
//! Wire examples (these exact strings are test vectors):
//! ```text
//! → {"v":1,"ts":1700000000,"cmd":"notify","title":"Done","body":"12 tests pass","workspace":"w1","tab":"t1"}
//! ← {"ok":true}
//! ← {"ok":false,"error":"unknown tab t9"}
//! ```
//! Optional fields are omitted when `None`. Unknown fields are ignored (forward compatible).
//! The app accepts any request with `v <= VERSION` and rejects newer ones with an error
//! response naming both versions.

use crate::agent::AgentKind;
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;

/// Status a script may set explicitly with `amalgum set-status` (§5.29 "Explicit status").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum StatusArg {
    Running,
    NeedsInput,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SplitDir {
    Right,
    Down,
}

/// One control command. Serialized with `"cmd": "<kebab-case variant>"` beside the fields.
/// `workspace`/`tab` default from `AMALGUM_WORKSPACE`/`AMALGUM_TAB` in the CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Command {
    Notify {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<String>,
    },
    SetStatus {
        status: StatusArg,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<String>,
    },
    ClearStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<String>,
    },
    /// Raw hook payload; the app's per-agent adapter (`agent::adapters`) interprets it.
    AgentEvent {
        agent: AgentKind,
        event: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<String>,
    },
    Open {
        location: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
    },
    Run {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        split: Option<SplitDir>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<String>,
    },
    List,
    Focus {
        target: String,
    },
    Resume {
        tab: String,
    },
    Hibernate {
        tab: String,
    },
}

impl Command {
    /// Notification-type commands never fail loudly: with no app they exit 0 (and are queued
    /// on remote hosts). Everything else is query-type and exits 1 "Amalgum is not running".
    pub fn is_notification(&self) -> bool {
        matches!(
            self,
            Command::Notify { .. }
                | Command::SetStatus { .. }
                | Command::ClearStatus { .. }
                | Command::AgentEvent { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub ts: u64,
    #[serde(flatten)]
    pub cmd: Command,
}

impl Request {
    /// Stamp `cmd` with the current protocol version and time.
    pub fn new(cmd: Command) -> Self {
        Self { v: VERSION, ts: unix_now(), cmd }
    }
    /// JSON plus a trailing `\n`.
    pub fn to_line(&self) -> String {
        to_line(self)
    }
    /// Parse one line (trailing newline optional). Rejects `v > VERSION`.
    pub fn from_line(line: &str) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(line.trim_end_matches(['\n', '\r']))
            .map_err(|e| format!("invalid JSON: {e}"))?;
        let v = value.get("v").and_then(serde_json::Value::as_u64).ok_or("missing field `v`")?;
        if v > u64::from(VERSION) {
            return Err(format!("request uses protocol v{v}, this app supports up to v{VERSION}"));
        }
        serde_json::from_value(value).map_err(|e| e.to_string())
    }
}

/// Seconds since the Unix epoch, `0` if the clock is somehow before it.
fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Serialize `v` and append the trailing newline; serialization of these types cannot
/// practically fail, so a failure degrades to an empty line rather than panicking.
fn to_line<T: Serialize>(v: &T) -> String {
    let mut s = serde_json::to_string(v).unwrap_or_default();
    s.push('\n');
    s
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn ok() -> Self {
        Self { ok: true, error: None, data: None }
    }
    pub fn with_data(data: serde_json::Value) -> Self {
        Self { ok: true, error: None, data: Some(data) }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, error: Some(msg.into()), data: None }
    }
    pub fn to_line(&self) -> String {
        to_line(self)
    }
    pub fn from_line(line: &str) -> Result<Self, String> {
        serde_json::from_str(line.trim_end_matches(['\n', '\r']))
            .map_err(|e| format!("invalid response: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The module doc's request line is a wire test vector: field order must be exactly
    /// `v`, `ts`, `cmd`, then the variant's own fields in declaration order.
    #[test]
    fn notify_request_matches_doc_example() {
        let req = Request {
            v: 1,
            ts: 1_700_000_000,
            cmd: Command::Notify {
                title: "Done".into(),
                body: Some("12 tests pass".into()),
                workspace: Some("w1".into()),
                tab: Some("t1".into()),
            },
        };
        assert_eq!(
            req.to_line(),
            "{\"v\":1,\"ts\":1700000000,\"cmd\":\"notify\",\"title\":\"Done\",\"body\":\"12 tests pass\",\"workspace\":\"w1\",\"tab\":\"t1\"}\n"
        );
    }

    #[test]
    fn ok_response_matches_doc_example() {
        assert_eq!(Response::ok().to_line(), "{\"ok\":true}\n");
    }

    #[test]
    fn err_response_matches_doc_example() {
        assert_eq!(
            Response::err("unknown tab t9").to_line(),
            "{\"ok\":false,\"error\":\"unknown tab t9\"}\n"
        );
    }

    #[test]
    fn none_optional_fields_are_omitted() {
        let cmd = Command::Notify { title: "Hi".into(), body: None, workspace: None, tab: None };
        let line = Request::new(cmd).to_line();
        assert!(!line.contains("body"));
        assert!(!line.contains("workspace"));
        assert!(!line.contains("tab"));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let line = r#"{"v":1,"ts":1,"cmd":"list","surprise":"field","nested":{"a":1}}"#;
        let req = Request::from_line(line).expect("known version parses");
        assert_eq!(req.cmd, Command::List);
    }

    #[test]
    fn v_equal_to_version_is_accepted() {
        let line = format!(r#"{{"v":{VERSION},"ts":1,"cmd":"list"}}"#);
        assert!(Request::from_line(&line).is_ok());
    }

    #[test]
    fn v_greater_than_version_names_both_versions() {
        let line = format!(r#"{{"v":{},"ts":1,"cmd":"list"}}"#, VERSION + 1);
        let err = Request::from_line(&line).expect_err("newer version is rejected");
        assert!(err.contains(&(VERSION + 1).to_string()), "error should name the request's version: {err}");
        assert!(err.contains(&VERSION.to_string()), "error should name the supported version: {err}");
    }

    #[test]
    fn missing_v_is_an_error() {
        assert!(Request::from_line(r#"{"ts":1,"cmd":"list"}"#).is_err());
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(Request::from_line("not json").is_err());
        assert!(Response::from_line("not json").is_err());
    }

    #[test]
    fn from_line_accepts_trailing_newline_or_not() {
        let with_nl = "{\"v\":1,\"ts\":1,\"cmd\":\"list\"}\n";
        let without_nl = "{\"v\":1,\"ts\":1,\"cmd\":\"list\"}";
        assert_eq!(Request::from_line(with_nl).unwrap(), Request::from_line(without_nl).unwrap());
    }

    #[test]
    fn agent_event_carries_arbitrary_json() {
        let event = serde_json::json!({"hook_event_name": "Stop", "nested": {"a": [1, 2, 3]}, "n": null});
        let cmd = Command::AgentEvent {
            agent: AgentKind::Claude,
            event: event.clone(),
            workspace: None,
            tab: None,
        };
        let req = Request::new(cmd);
        let round = Request::from_line(&req.to_line()).expect("round-trips");
        match round.cmd {
            Command::AgentEvent { event: got, .. } => assert_eq!(got, event),
            other => panic!("expected AgentEvent, got {other:?}"),
        }
    }

    #[test]
    fn is_notification_classifies_commands() {
        let notify = Command::Notify { title: "t".into(), body: None, workspace: None, tab: None };
        let set_status =
            Command::SetStatus { status: StatusArg::Idle, message: None, workspace: None, tab: None };
        let clear_status = Command::ClearStatus { workspace: None, tab: None };
        let agent_event = Command::AgentEvent {
            agent: AgentKind::Codex,
            event: serde_json::Value::Null,
            workspace: None,
            tab: None,
        };
        for cmd in [&notify, &set_status, &clear_status, &agent_event] {
            assert!(cmd.is_notification(), "{cmd:?} should be notification-type");
        }

        let open = Command::Open { location: "/x".into(), remote: None, name: None, run: None };
        let run = Command::Run { command: "echo hi".into(), split: None, workspace: None, tab: None };
        let list = Command::List;
        let focus = Command::Focus { target: "w1".into() };
        let resume = Command::Resume { tab: "t1".into() };
        let hibernate = Command::Hibernate { tab: "t1".into() };
        for cmd in [&open, &run, &list, &focus, &resume, &hibernate] {
            assert!(!cmd.is_notification(), "{cmd:?} should be query-type");
        }
    }

    /// Every variant round-trips through `to_line` / `from_line` with all fields populated.
    #[test]
    fn round_trips_every_command_variant() {
        let variants = vec![
            Command::Notify {
                title: "t".into(),
                body: Some("b".into()),
                workspace: Some("w".into()),
                tab: Some("tab".into()),
            },
            Command::SetStatus {
                status: StatusArg::NeedsInput,
                message: Some("m".into()),
                workspace: Some("w".into()),
                tab: Some("tab".into()),
            },
            Command::ClearStatus { workspace: Some("w".into()), tab: Some("tab".into()) },
            Command::AgentEvent {
                agent: AgentKind::Opencode,
                event: serde_json::json!({"k": "v"}),
                workspace: Some("w".into()),
                tab: Some("tab".into()),
            },
            Command::Open {
                location: "host:path".into(),
                remote: Some("origin".into()),
                name: Some("n".into()),
                run: Some("cmd".into()),
            },
            Command::Run {
                command: "cargo test".into(),
                split: Some(SplitDir::Right),
                workspace: Some("w".into()),
                tab: Some("tab".into()),
            },
            Command::List,
            Command::Focus { target: "w1".into() },
            Command::Resume { tab: "t1".into() },
            Command::Hibernate { tab: "t1".into() },
        ];
        for cmd in variants {
            let req = Request::new(cmd.clone());
            let line = req.to_line();
            assert!(line.ends_with('\n'));
            let round =
                Request::from_line(&line).unwrap_or_else(|e| panic!("{cmd:?} failed to round-trip: {e}"));
            assert_eq!(round, req);
        }
    }

    #[test]
    fn response_round_trips_with_and_without_data() {
        let with_data = Response::with_data(serde_json::json!({"workspaces": []}));
        assert_eq!(Response::from_line(&with_data.to_line()).unwrap(), with_data);
        let plain_ok = Response::ok();
        assert_eq!(Response::from_line(&plain_ok.to_line()).unwrap(), plain_ok);
        let error = Response::err("boom");
        assert_eq!(Response::from_line(&error.to_line()).unwrap(), error);
    }
}
