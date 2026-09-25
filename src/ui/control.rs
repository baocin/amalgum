//! The app side of the control socket (§5.31). The socket thread answers immediately: `list`
//! from the latest snapshot the UI published, everything else is acknowledged and queued for
//! the UI thread, which applies it on the next frame.

use crate::ctl::protocol::{Command, Request, Response};
use crate::ctl::socket::{self, ServerHandle};
use crate::paths::Dirs;
use std::io;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, PoisonError};

pub struct Control {
    _server: ServerHandle,
    rx: Receiver<Request>,
    snapshot: Arc<Mutex<serde_json::Value>>,
}

impl Control {
    pub fn start(dirs: &Dirs, ctx: &egui::Context) -> io::Result<Self> {
        let (tx, rx) = channel();
        let snapshot = Arc::new(Mutex::new(serde_json::json!({ "workspaces": [] })));
        let (shared, ctx) = (Arc::clone(&snapshot), ctx.clone());
        let tx = Mutex::new(tx);
        let server = socket::serve(&dirs.socket(), move |req: Request| {
            if matches!(req.cmd, Command::List) {
                let data = shared.lock().unwrap_or_else(PoisonError::into_inner).clone();
                return Response::with_data(data);
            }
            let sent = tx.lock().unwrap_or_else(PoisonError::into_inner).send(req).is_ok();
            ctx.request_repaint();
            if sent { Response::ok() } else { Response::err("Amalgum is shutting down") }
        })?;
        Ok(Self { _server: server, rx, snapshot })
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
