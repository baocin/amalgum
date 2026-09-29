//! The W16 connection state machine for one host (§5.28 "Connect", "Keepalive and reconnect").
//! Pure: it is driven by [`Event`]s and a clock value (`now`, seconds on any monotonic scale —
//! the UI passes egui's frame time) and answers with the [`Action`] the caller must perform.
//! It renders the W16 progress line and the "Connection lost" banner.
//!
//! ```text
//! Disconnected ─Connect─▶ Connecting{step} ─Connected─▶ Connected{warnings}
//!                   │  ▲         │ HostKeyUnknown          │ MasterDied
//!                   │  └─Trust── HostKeyUnknown            ▼
//!                   │            │ retryable failure   Lost{since, step} (attempt in flight)
//!                   │            ▼                          │ retryable failure ▲ Tick/RetryNow
//!                   │         Retrying{attempt, next_at} ◀──┘───────────────────┘
//!                   └── fatal failure ─▶ Failed{error}      (Cancel/Disconnect → Disconnected)
//! ```
//!
//! Every attempt gets a fresh epoch number; results from an older attempt (one the user
//! cancelled, or that raced a newer one) are ignored.

use super::{Backoff, HostKeyPrompt};
use crate::git::GitError;

/// One connect step, in order (§5.28 "Connect" 1–4, plus draining queued hook events).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Step {
    Master,
    Cli,
    Tools,
    Socket,
    Drain,
}

impl Step {
    pub const ALL: [Step; 5] = [Step::Master, Step::Cli, Step::Tools, Step::Socket, Step::Drain];

    /// 1-based position, for the W16 progress bar.
    pub fn number(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).map_or(0, |i| i + 1)
    }

    /// W16 progress-line text: "verifying amalgum CLI 0.4.1".
    pub fn label(self, cli_version: &str) -> String {
        match self {
            Step::Master => "opening connection".to_string(),
            Step::Cli => format!("verifying amalgum CLI {cli_version}"),
            Step::Tools => "checking git and tmux".to_string(),
            Step::Socket => "forwarding the control socket".to_string(),
            Step::Drain => "collecting queued notifications".to_string(),
        }
    }
}

/// Something that works but less well than asked; shown once as a toast and kept on the
/// connected state for the host's Details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// No CLI for the host (none embedded in this build, or an unsupported `uname -sm`): the
    /// connection works, but remote agent hooks cannot reach the app.
    CliUnavailable { reason: String },
    /// Keepalive was asked for but the host has no tmux (§5.28 step 3).
    NoTmux,
    /// Keepalive was asked for but the host's tmux predates 3.2 (`new-session -e`): terminals
    /// run without it.
    OldTmux { version: String },
    /// sshd refused the reverse socket (`AllowStreamLocalForwarding no`): hook events queue on
    /// the host and are drained instead (§5.28 step 4).
    QueueMode { ssh_said: String },
    /// A non-essential remote command failed (uploading tmux.conf, draining the queue).
    StepFailed { what: String, error: GitError },
}

impl Warning {
    pub fn text(&self, host: &str) -> String {
        match self {
            Warning::CliUnavailable { reason } => {
                format!(
                    "amalgum CLI unavailable on {host} ({reason}); agent notifications from it will not arrive"
                )
            }
            Warning::NoTmux => format!("tmux not found on {host}; sessions will not survive disconnects"),
            Warning::OldTmux { version } => {
                format!("{version} on {host} is older than 3.2; sessions will not survive disconnects")
            }
            Warning::QueueMode { .. } => format!(
                "{host} does not allow socket forwarding; agent notifications arrive when the app next reaches it"
            ),
            Warning::StepFailed { what, error } => format!("{what} on {host} failed: {}", error.summary()),
        }
    }
}

/// An automatic reconnect in progress: when the connection went away and how many attempts
/// have failed since. `ever_connected` is false while the very first connect keeps failing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outage {
    pub since: f64,
    pub attempt: u32,
    pub ever_connected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    Disconnected,
    /// A user-initiated attempt (first connect, Retry after a failure, Trust).
    Connecting {
        step: Step,
        trust: bool,
    },
    /// Paused on the W16 unknown-host-key dialog until [`Event::Trust`] or [`Event::Cancel`].
    HostKeyUnknown {
        prompt: HostKeyPrompt,
        ssh_said: String,
    },
    Connected {
        warnings: Vec<Warning>,
    },
    /// The connection dropped and an automatic reconnect attempt is running.
    Lost {
        outage: Outage,
        step: Step,
    },
    /// Waiting for the next automatic attempt.
    Retrying {
        outage: Outage,
        next_at: f64,
    },
    /// Needs the user: missing git, a changed host key, rejected auth, …
    Failed {
        step: Step,
        error: GitError,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Open the connection (W15, `amalgum open host:path`, the first workspace on the host).
    Connect,
    /// **Retry now** (banner) or **Retry** (failure).
    RetryNow,
    /// **Trust** in the host-key dialog.
    Trust,
    /// **Cancel** on the progress line or the host-key dialog.
    Cancel,
    /// **Disconnect** in the row menu, or the last workspace on the host closed.
    Disconnect,
    /// Clock advanced; starts a due retry.
    Tick,
    /// The liveness probe (`ssh -O check`) failed, or a remote command found the master gone.
    MasterDied,
    // --- results of an attempt, tagged with its epoch ---
    StepStarted {
        epoch: u64,
        step: Step,
    },
    Warning {
        epoch: u64,
        warning: Warning,
    },
    HostKeyUnknown {
        epoch: u64,
        prompt: HostKeyPrompt,
        ssh_said: String,
    },
    Connected {
        epoch: u64,
    },
    Failed {
        epoch: u64,
        step: Step,
        error: GitError,
        fatal: bool,
    },
}

/// What the caller must do after [`Machine::handle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Run the connect steps on a worker, tagging every result with `epoch`. `trust` accepts a
    /// new host key for this one attempt (`StrictHostKeyChecking=accept-new`).
    Start { epoch: u64, trust: bool },
    /// Close the reverse socket and port forwards. The ControlMaster stays until ControlPersist
    /// expires, and tmux sessions stay on the host (§5.28 "Disconnect").
    CloseForwards,
}

#[derive(Debug, Clone)]
pub struct Machine {
    pub host: String,
    pub cli_version: String,
    phase: Phase,
    epoch: u64,
    backoff: Backoff,
    /// Warnings of the attempt in flight, moved into `Connected`.
    pending: Vec<Warning>,
}

impl Machine {
    pub fn new(host: impl Into<String>, cli_version: impl Into<String>, backoff_cap: u64) -> Self {
        Self {
            host: host.into(),
            cli_version: cli_version.into(),
            phase: Phase::Disconnected,
            epoch: 0,
            backoff: Backoff::new(backoff_cap),
            pending: Vec::new(),
        }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// Epoch of the current (or last) attempt.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.phase, Phase::Connected { .. })
    }

    /// Terminals grey out with their last screen (W16) whenever the host is not connected.
    pub fn is_down(&self) -> bool {
        !self.is_connected()
    }

    pub fn handle(&mut self, event: Event, now: f64) -> Option<Action> {
        use Phase as P;
        match (event, &self.phase) {
            (Event::Connect | Event::RetryNow, P::Disconnected | P::Failed { .. }) => {
                Some(self.start(false, None))
            }
            (Event::RetryNow, P::Retrying { outage, .. }) => {
                let outage = *outage;
                Some(self.start(false, Some(outage)))
            }
            (Event::Tick, P::Retrying { outage, next_at }) if now >= *next_at => {
                let outage = *outage;
                Some(self.start(false, Some(outage)))
            }
            (Event::Trust, P::HostKeyUnknown { .. }) => Some(self.start(true, None)),
            (Event::Cancel | Event::Disconnect, phase) => {
                let had_forwards = matches!(phase, P::Connected { .. } | P::Lost { .. } | P::Retrying { .. });
                self.phase = P::Disconnected;
                self.epoch += 1; // drop whatever the cancelled attempt still reports
                self.pending.clear();
                had_forwards.then_some(Action::CloseForwards)
            }
            (Event::MasterDied, P::Connected { .. }) => {
                Some(self.start(false, Some(Outage { since: now, attempt: 0, ever_connected: true })))
            }
            (Event::StepStarted { epoch, step }, P::Connecting { trust, .. }) if epoch == self.epoch => {
                self.phase = P::Connecting { step, trust: *trust };
                None
            }
            (Event::StepStarted { epoch, step }, P::Lost { outage, .. }) if epoch == self.epoch => {
                self.phase = P::Lost { outage: *outage, step };
                None
            }
            (Event::Warning { epoch, warning }, P::Connecting { .. } | P::Lost { .. })
                if epoch == self.epoch =>
            {
                if !self.pending.contains(&warning) {
                    self.pending.push(warning);
                }
                None
            }
            (Event::HostKeyUnknown { epoch, prompt, ssh_said }, P::Connecting { .. } | P::Lost { .. })
                if epoch == self.epoch =>
            {
                self.phase = P::HostKeyUnknown { prompt, ssh_said };
                self.pending.clear();
                None
            }
            (Event::Connected { epoch }, P::Connecting { .. } | P::Lost { .. }) if epoch == self.epoch => {
                self.phase = P::Connected { warnings: std::mem::take(&mut self.pending) };
                self.backoff.reset();
                None
            }
            // An attempt the user cancelled (however many cancels ago) still opened forwards:
            // close them again.
            (Event::Connected { epoch }, P::Disconnected) if epoch < self.epoch => {
                Some(Action::CloseForwards)
            }
            (Event::Failed { epoch, step, error, fatal }, P::Connecting { .. } | P::Lost { .. })
                if epoch == self.epoch =>
            {
                self.pending.clear();
                if fatal {
                    self.phase = P::Failed { step, error };
                    return None;
                }
                let outage = match &self.phase {
                    P::Lost { outage, .. } => Outage { attempt: outage.attempt + 1, ..*outage },
                    _ => Outage { since: now, attempt: 1, ever_connected: false },
                };
                let delay = self.backoff.next().unwrap_or(60) as f64;
                self.phase = P::Retrying { outage, next_at: now + delay };
                None
            }
            _ => None,
        }
    }

    fn start(&mut self, trust: bool, outage: Option<Outage>) -> Action {
        self.epoch += 1;
        self.pending.clear();
        self.phase = match outage {
            Some(outage) => Phase::Lost { outage, step: Step::Master },
            None => Phase::Connecting { step: Step::Master, trust },
        };
        if outage.is_none() {
            self.backoff.reset();
        }
        Action::Start { epoch: self.epoch, trust }
    }

    /// `(done, total)` steps for the W16 progress bar while an attempt runs.
    pub fn progress(&self) -> Option<(usize, usize)> {
        match self.phase {
            Phase::Connecting { step, .. } | Phase::Lost { step, .. } => {
                Some((step.number() - 1, Step::ALL.len()))
            }
            _ => None,
        }
    }

    /// W16 progress line: "Connecting to gpu-box… verifying amalgum CLI 0.4.1".
    pub fn progress_line(&self) -> Option<String> {
        let (verb, step) = match self.phase {
            Phase::Connecting { step, .. } => ("Connecting to", step),
            Phase::Lost { step, .. } => ("Reconnecting to", step),
            _ => return None,
        };
        Some(format!("{verb} {}…  {}", self.host, step.label(&self.cli_version)))
    }

    /// W16 banner: "Connection lost 12 s ago · retrying in 6 s".
    pub fn banner(&self, now: f64) -> Option<String> {
        let (outage, tail) = match &self.phase {
            Phase::Lost { outage, .. } => (outage, "reconnecting…".to_string()),
            Phase::Retrying { outage, next_at } => {
                (outage, format!("retrying in {}", duration((next_at - now).max(0.0).ceil().max(1.0))))
            }
            Phase::Failed { error, .. } => {
                return Some(format!("Can't connect to {}: {}", self.host, error.summary()));
            }
            _ => return None,
        };
        let head = if outage.ever_connected {
            format!("Connection lost {} ago", duration((now - outage.since).max(0.0).floor()))
        } else {
            format!("Can't reach {}", self.host)
        };
        Some(format!("{head} · {tail}"))
    }
}

/// `12 s`, `3 min`, `2 h`.
fn duration(secs: f64) -> String {
    let secs = secs as u64;
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min", secs / 60),
        _ => format!("{} h", secs / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> Machine {
        Machine::new("gpu-box", "0.4.1", 60)
    }

    fn err(code: i32, stderr: &str) -> GitError {
        GitError { command: "ssh gpu-box".into(), code: Some(code), stderr: stderr.into() }
    }

    fn prompt() -> HostKeyPrompt {
        HostKeyPrompt {
            host: "gpu-box".into(),
            key_type: "ED25519".into(),
            fingerprint: "SHA256:k3s9".into(),
        }
    }

    /// Connect and run every step to success at `now`.
    fn connected(m: &mut Machine, now: f64) {
        let Some(Action::Start { epoch, .. }) = m.handle(Event::Connect, now) else {
            panic!("{:?}", m.phase())
        };
        assert_eq!(m.handle(Event::Connected { epoch }, now), None);
        assert!(m.is_connected());
    }

    fn epoch_of(action: Option<Action>) -> u64 {
        match action {
            Some(Action::Start { epoch, .. }) => epoch,
            other => panic!("expected Start, got {other:?}"),
        }
    }

    #[test]
    fn connect_walks_the_steps_with_progress_line() {
        let mut m = machine();
        assert_eq!(m.progress_line(), None);
        let epoch = epoch_of(m.handle(Event::Connect, 0.0));
        assert_eq!(m.progress_line().unwrap(), "Connecting to gpu-box…  opening connection");
        assert_eq!(m.progress(), Some((0, 5)));
        m.handle(Event::StepStarted { epoch, step: Step::Cli }, 0.1);
        assert_eq!(m.progress_line().unwrap(), "Connecting to gpu-box…  verifying amalgum CLI 0.4.1");
        assert_eq!(m.progress(), Some((1, 5)));
        m.handle(Event::StepStarted { epoch, step: Step::Drain }, 0.2);
        assert_eq!(m.progress(), Some((4, 5)));
        m.handle(Event::Connected { epoch }, 0.3);
        assert_eq!(m.phase(), &Phase::Connected { warnings: vec![] });
        assert_eq!((m.progress(), m.progress_line(), m.banner(0.3)), (None, None, None));
        assert!(!m.is_down());
    }

    #[test]
    fn connect_is_ignored_while_busy_or_connected() {
        let mut m = machine();
        epoch_of(m.handle(Event::Connect, 0.0));
        assert_eq!(m.handle(Event::Connect, 0.1), None);
        let mut c = machine();
        connected(&mut c, 0.0);
        assert_eq!(c.handle(Event::Connect, 1.0), None);
        assert_eq!(c.handle(Event::RetryNow, 1.0), None);
        assert!(c.is_connected());
    }

    #[test]
    fn warnings_collect_once_and_land_on_connected() {
        let mut m = machine();
        let epoch = epoch_of(m.handle(Event::Connect, 0.0));
        let queue = Warning::QueueMode { ssh_said: "refused".into() };
        m.handle(Event::Warning { epoch, warning: Warning::NoTmux }, 0.1);
        m.handle(Event::Warning { epoch, warning: queue.clone() }, 0.1);
        m.handle(Event::Warning { epoch, warning: Warning::NoTmux }, 0.1);
        m.handle(Event::Connected { epoch }, 0.2);
        assert_eq!(m.phase(), &Phase::Connected { warnings: vec![Warning::NoTmux, queue] });
    }

    #[test]
    fn warning_texts() {
        assert_eq!(
            Warning::NoTmux.text("gpu-box"),
            "tmux not found on gpu-box; sessions will not survive disconnects"
        );
        let cli = Warning::CliUnavailable { reason: "not embedded in this build".into() };
        assert!(
            cli.text("gpu-box").contains("amalgum CLI unavailable on gpu-box (not embedded in this build)")
        );
        assert!(
            Warning::QueueMode { ssh_said: String::new() }
                .text("h")
                .contains("does not allow socket forwarding")
        );
        let failed = Warning::StepFailed { what: "Draining queued events".into(), error: err(1, "boom\n") };
        assert_eq!(failed.text("h"), "Draining queued events on h failed: boom");
    }

    #[test]
    fn unknown_host_key_pauses_then_trust_restarts_with_accept_new() {
        let mut m = machine();
        let epoch = epoch_of(m.handle(Event::Connect, 0.0));
        m.handle(
            Event::HostKeyUnknown {
                epoch,
                prompt: prompt(),
                ssh_said: "Host key verification failed.".into(),
            },
            0.1,
        );
        assert!(matches!(m.phase(), Phase::HostKeyUnknown { prompt: p, .. } if *p == prompt()));
        assert_eq!(m.progress_line(), None);
        assert_eq!(m.handle(Event::Tick, 100.0), None, "the dialog waits for the user");

        let Some(Action::Start { epoch: epoch2, trust: true }) = m.handle(Event::Trust, 5.0) else {
            panic!()
        };
        assert!(epoch2 > epoch);
        assert!(matches!(m.phase(), Phase::Connecting { trust: true, .. }));
        m.handle(Event::StepStarted { epoch: epoch2, step: Step::Tools }, 5.1);
        assert!(
            matches!(m.phase(), Phase::Connecting { trust: true, step: Step::Tools }),
            "trust kept per attempt"
        );
        m.handle(Event::Connected { epoch: epoch2 }, 5.2);
        assert!(m.is_connected());
    }

    #[test]
    fn cancel_on_the_host_key_dialog_disconnects_without_closing_anything() {
        let mut m = machine();
        let epoch = epoch_of(m.handle(Event::Connect, 0.0));
        m.handle(Event::HostKeyUnknown { epoch, prompt: prompt(), ssh_said: String::new() }, 0.1);
        assert_eq!(m.handle(Event::Cancel, 0.2), None);
        assert_eq!(m.phase(), &Phase::Disconnected);
        assert_eq!(m.handle(Event::Trust, 0.3), None, "trust without a dialog does nothing");
    }

    #[test]
    fn fatal_failure_needs_the_user() {
        let mut m = machine();
        let epoch = epoch_of(m.handle(Event::Connect, 0.0));
        let e = err(127, "sh: git: not found\n");
        m.handle(Event::Failed { epoch, step: Step::Tools, error: e.clone(), fatal: true }, 1.0);
        assert_eq!(m.phase(), &Phase::Failed { step: Step::Tools, error: e });
        assert_eq!(m.handle(Event::Tick, 1000.0), None, "no automatic retry");
        assert_eq!(m.banner(2.0).unwrap(), "Can't connect to gpu-box: sh: git: not found");
        let epoch2 = epoch_of(m.handle(Event::RetryNow, 3.0));
        assert!(epoch2 > epoch);
        assert!(matches!(m.phase(), Phase::Connecting { step: Step::Master, trust: false }));
    }

    #[test]
    fn first_connect_failing_retries_with_backoff() {
        let mut m = machine();
        let epoch = epoch_of(m.handle(Event::Connect, 0.0));
        m.handle(
            Event::Failed { epoch, step: Step::Master, error: err(255, "Connection refused"), fatal: false },
            2.0,
        );
        assert_eq!(
            m.phase(),
            &Phase::Retrying {
                outage: Outage { since: 2.0, attempt: 1, ever_connected: false },
                next_at: 5.0
            }
        );
        assert_eq!(m.banner(2.0).unwrap(), "Can't reach gpu-box · retrying in 3 s");
        assert!(m.is_down());
    }

    #[test]
    fn master_death_reconnects_at_once_then_backs_off_3_6_12_24_48_60_60() {
        let mut m = machine();
        connected(&mut m, 0.0);
        let mut epoch = epoch_of(m.handle(Event::MasterDied, 100.0));
        assert!(matches!(m.phase(), Phase::Lost { step: Step::Master, .. }));
        assert_eq!(m.banner(112.0).unwrap(), "Connection lost 12 s ago · reconnecting…");
        assert_eq!(m.progress_line().unwrap(), "Reconnecting to gpu-box…  opening connection");

        let mut now = 100.0;
        let mut delays = Vec::new();
        for attempt in 1..=7u32 {
            m.handle(
                Event::Failed { epoch, step: Step::Master, error: err(255, "timeout"), fatal: false },
                now,
            );
            let Phase::Retrying { outage, next_at } = m.phase().clone() else { panic!("{:?}", m.phase()) };
            assert_eq!(outage, Outage { since: 100.0, attempt, ever_connected: true });
            delays.push(next_at - now);
            assert_eq!(m.handle(Event::Tick, next_at - 0.5), None, "not due yet");
            now = next_at;
            epoch = epoch_of(m.handle(Event::Tick, now));
            assert!(matches!(m.phase(), Phase::Lost { outage: o, .. } if o.attempt == attempt));
        }
        assert_eq!(delays, [3.0, 6.0, 12.0, 24.0, 48.0, 60.0, 60.0]);

        m.handle(Event::Connected { epoch }, now + 1.0);
        assert!(m.is_connected());
        // A later drop starts the backoff from 3 s again.
        let epoch = epoch_of(m.handle(Event::MasterDied, 1000.0));
        m.handle(Event::Failed { epoch, step: Step::Master, error: err(255, "x"), fatal: false }, 1000.0);
        assert!(matches!(m.phase(), Phase::Retrying { next_at, .. } if *next_at == 1003.0));
    }

    #[test]
    fn banner_counts_since_the_drop_and_down_to_the_retry() {
        let mut m = machine();
        connected(&mut m, 0.0);
        let epoch = epoch_of(m.handle(Event::MasterDied, 10.0));
        m.handle(Event::Failed { epoch, step: Step::Master, error: err(255, "x"), fatal: false }, 10.0);
        let epoch = epoch_of(m.handle(Event::Tick, 13.0));
        m.handle(Event::Failed { epoch, step: Step::Socket, error: err(255, "x"), fatal: false }, 16.0);
        // next_at = 16 + 6 = 22
        assert_eq!(m.banner(22.0 - 6.0).unwrap(), "Connection lost 6 s ago · retrying in 6 s");
        assert_eq!(m.banner(21.7).unwrap(), "Connection lost 11 s ago · retrying in 1 s");
        assert_eq!(m.banner(22.0).unwrap(), "Connection lost 12 s ago · retrying in 1 s", "never 0 s");
        assert_eq!(m.banner(10.0 + 185.0).unwrap(), "Connection lost 3 min ago · retrying in 1 s");
    }

    #[test]
    fn retry_now_skips_the_wait() {
        let mut m = machine();
        connected(&mut m, 0.0);
        let epoch = epoch_of(m.handle(Event::MasterDied, 10.0));
        m.handle(Event::Failed { epoch, step: Step::Master, error: err(255, "x"), fatal: false }, 10.0);
        let epoch2 = epoch_of(m.handle(Event::RetryNow, 10.5));
        assert!(epoch2 > epoch);
        assert!(matches!(m.phase(), Phase::Lost { .. }));
        assert_eq!(m.handle(Event::RetryNow, 10.6), None, "already attempting");
    }

    #[test]
    fn disconnect_closes_forwards_and_drops_late_results() {
        let mut m = machine();
        connected(&mut m, 0.0);
        assert_eq!(m.handle(Event::Disconnect, 1.0), Some(Action::CloseForwards));
        assert_eq!(m.phase(), &Phase::Disconnected);
        assert_eq!(m.handle(Event::MasterDied, 2.0), None, "a disconnected host does not reconnect");
        assert_eq!(m.handle(Event::Tick, 100.0), None);
        assert_eq!(m.handle(Event::Disconnect, 3.0), None, "idempotent");

        // Disconnect while waiting to retry: forwards may still exist on a live master.
        let mut r = machine();
        connected(&mut r, 0.0);
        let epoch = epoch_of(r.handle(Event::MasterDied, 1.0));
        r.handle(Event::Failed { epoch, step: Step::Master, error: err(255, "x"), fatal: false }, 1.0);
        assert_eq!(r.handle(Event::Disconnect, 2.0), Some(Action::CloseForwards));
    }

    #[test]
    fn cancel_during_connect_ignores_the_attempts_later_results() {
        let mut m = machine();
        let epoch = epoch_of(m.handle(Event::Connect, 0.0));
        assert_eq!(m.handle(Event::Cancel, 0.5), None, "nothing was forwarded yet");
        assert_eq!(m.handle(Event::StepStarted { epoch, step: Step::Cli }, 0.6), None);
        assert_eq!(m.phase(), &Phase::Disconnected);
        assert_eq!(
            m.handle(Event::Connected { epoch }, 1.0),
            Some(Action::CloseForwards),
            "the cancelled attempt may have forwarded the socket"
        );
        assert_eq!(m.phase(), &Phase::Disconnected);
        m.handle(Event::Failed { epoch, step: Step::Master, error: err(1, "x"), fatal: true }, 1.0);
        assert_eq!(m.phase(), &Phase::Disconnected);
    }

    #[test]
    fn a_late_success_after_several_cancels_still_closes_its_forwards() {
        let mut m = machine();
        let first = epoch_of(m.handle(Event::Connect, 0.0));
        m.handle(Event::Cancel, 0.1);
        epoch_of(m.handle(Event::Connect, 0.2));
        m.handle(Event::Cancel, 0.3);
        m.handle(Event::Disconnect, 0.4);
        assert_eq!(m.handle(Event::Connected { epoch: first }, 1.0), Some(Action::CloseForwards));
        assert_eq!(m.phase(), &Phase::Disconnected);
    }

    #[test]
    fn results_from_a_superseded_attempt_are_ignored() {
        let mut m = machine();
        let old = epoch_of(m.handle(Event::Connect, 0.0));
        m.handle(Event::Failed { epoch: old, step: Step::Master, error: err(1, "x"), fatal: true }, 0.1);
        let new = epoch_of(m.handle(Event::RetryNow, 0.2));
        m.handle(Event::Connected { epoch: old }, 0.3);
        m.handle(Event::Warning { epoch: old, warning: Warning::NoTmux }, 0.3);
        m.handle(Event::HostKeyUnknown { epoch: old, prompt: prompt(), ssh_said: String::new() }, 0.3);
        assert!(matches!(m.phase(), Phase::Connecting { .. }));
        m.handle(Event::Connected { epoch: new }, 0.4);
        assert_eq!(m.phase(), &Phase::Connected { warnings: vec![] });
    }

    #[test]
    fn master_died_only_matters_while_connected() {
        for setup in [0, 1, 2] {
            let mut m = machine();
            match setup {
                0 => {}
                1 => {
                    epoch_of(m.handle(Event::Connect, 0.0));
                }
                _ => {
                    let epoch = epoch_of(m.handle(Event::Connect, 0.0));
                    m.handle(
                        Event::Failed { epoch, step: Step::Master, error: err(1, "x"), fatal: true },
                        0.0,
                    );
                }
            }
            let before = m.phase().clone();
            assert_eq!(m.handle(Event::MasterDied, 1.0), None);
            assert_eq!(m.phase(), &before);
        }
    }

    #[test]
    fn host_key_prompt_during_reconnect_pauses_too() {
        let mut m = machine();
        connected(&mut m, 0.0);
        let epoch = epoch_of(m.handle(Event::MasterDied, 1.0));
        m.handle(Event::HostKeyUnknown { epoch, prompt: prompt(), ssh_said: String::new() }, 1.1);
        assert!(matches!(m.phase(), Phase::HostKeyUnknown { .. }));
        assert_eq!(m.banner(2.0), None);
    }

    #[test]
    fn durations() {
        assert_eq!(duration(0.0), "0 s");
        assert_eq!(duration(59.9), "59 s");
        assert_eq!(duration(60.0), "1 min");
        assert_eq!(duration(3600.0), "1 h");
    }
}
