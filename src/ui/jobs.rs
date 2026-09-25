//! Background work. The UI thread never waits on git, ssh, sockets, or disk (§2 "Never block"):
//! it hands a closure to a worker thread and receives the result on a channel next frame.

use std::sync::mpsc::Sender;

/// Run `work` on a new thread, send its result to `tx`, and wake the UI.
pub fn spawn<T: Send + 'static>(ctx: &egui::Context, tx: &Sender<T>, work: impl FnOnce() -> T + Send + 'static) {
    let (ctx, tx) = (ctx.clone(), tx.clone());
    std::thread::spawn(move || {
        // A closed channel means the receiver (a pane or the app) is gone; drop the result.
        if tx.send(work()).is_ok() {
            ctx.request_repaint();
        }
    });
}
