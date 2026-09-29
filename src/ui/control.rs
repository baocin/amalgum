//! The app side of the control socket (§5.31). The socket thread answers immediately: `list`
//! from the latest snapshot the UI published, everything else is acknowledged and queued for
//! the UI thread, which applies it on the next frame.
//!
//! Remote hosts reach the app through a second socket ([`Dirs::remote_socket`], the target of
//! every ssh reverse forward, §5.28). A remote host is not trusted to drive this machine, so that
//! socket accepts notification-type commands only: an agent there can report status and notify,
//! never `run`, `open`, or `focus` here (invariant 10).

use crate::ctl::protocol::{Command, Request, Response};
use crate::ctl::socket::{self, ServerHandle};
use crate::paths::Dirs;
use std::io;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, PoisonError};

pub struct Control {
    _server: ServerHandle,
    _remote: ServerHandle,
    rx: Receiver<Request>,
    snapshot: Arc<Mutex<serde_json::Value>>,
}

impl Control {
    pub fn start(dirs: &Dirs, ctx: &egui::Context) -> io::Result<Self> {
        let (tx, rx) = channel();
        let snapshot = Arc::new(Mutex::new(serde_json::json!({ "workspaces": [] })));
        let forward = {
            let (tx, ctx) = (Mutex::new(tx), ctx.clone());
            Arc::new(move |req: Request| {
                let sent = tx.lock().unwrap_or_else(PoisonError::into_inner).send(req).is_ok();
                ctx.request_repaint();
                if sent { Response::ok() } else { Response::err("Amalgum is shutting down") }
            })
        };
        let (shared, local) = (Arc::clone(&snapshot), Arc::clone(&forward));
        let server = socket::serve(&dirs.socket(), move |req: Request| {
            if matches!(req.cmd, Command::List) {
                let data = shared.lock().unwrap_or_else(PoisonError::into_inner).clone();
                return Response::with_data(data);
            }
            local(req)
        })?;
        let remote =
            socket::serve(&dirs.remote_socket(), move |req: Request| match remote_refusal(&req.cmd) {
                Some(why) => Response::err(why),
                None => forward(req),
            })?;
        Ok(Self { _server: server, _remote: remote, rx, snapshot })
    }

    /// Requests received since the last call.
    pub fn drain(&self) -> Vec<Request> {
        self.rx.try_iter().collect()
    }

    /// Publish the data `amalgum list` returns (shape documented in `ctl::cli`).
    pub fn publish(&self, data: serde_json::Value) {
        *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner) = data;
    }
}

/// Why a request from a remote host is refused, if it is: only notification-type commands
/// cross from a host to this machine.
fn remote_refusal(cmd: &Command) -> Option<&'static str> {
    (!cmd.is_notification()).then_some("Only notifications are accepted from remote hosts")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_hosts_may_notify_but_never_drive_this_machine() {
        let notify = Command::Notify { title: "done".into(), body: None, workspace: None, tab: None };
        assert_eq!(remote_refusal(&notify), None);
        for cmd in [
            Command::Run { command: "rm -rf ~".into(), split: None, workspace: None, tab: None },
            Command::Open { location: "/etc".into(), remote: None, name: None, run: None },
            Command::Focus { target: "w1".into() },
            Command::List,
        ] {
            assert!(remote_refusal(&cmd).is_some(), "{cmd:?} must be refused");
        }
    }
}
