//! The clone sheet (§5.3): `Mod+Shift+O`, File → Clone Repository, the welcome **Clone** card,
//! and the palette all open it. The form and its validation are `model::clone`; the clone
//! itself is `git::net::clone` on a worker thread, reporting to a progress pill with **Cancel**
//! (cancel kills git and `net` deletes the partial folder). Failures become toasts in the spec's
//! words with the command and stderr under **Details** (§5.24); the sheet comes back with what
//! was typed so the user can fix it.
//!
//! Not in this build: a native folder picker (`platform` has none, so the destination is a text
//! field), and the W16 host-key dialog (an unknown host key is a toast naming W16 and how to
//! trust the key by hand; it is never accepted automatically).

use super::chrome::Toast;
use super::dialogs::{self, FormOutcome, Popover};
use super::theme::Colors;
use crate::git::net::{self, NetError, NetErrorKind};
use crate::git::{Git, Location};
use crate::model::clone::{CloneForm, CloneRequest};
use crate::model::settings::Settings;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

/// What a finished clone asks of the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneEvent {
    Toast(Toast),
    /// The repository exists at `location`: add it to recents, and open it as a workspace (§5.2)
    /// when `open` is set.
    Cloned {
        location: Location,
        name: String,
        open: bool,
    },
}

enum Reply {
    /// Read on a worker when the sheet opens: clipboard text and `~/.ssh/config`'s hosts.
    Prefill {
        clipboard: Option<String>,
        hosts: Vec<String>,
    },
    Progress(u64, dialogs::Progress),
    Done(u64, Result<Location, NetError>),
}

struct Job {
    id: u64,
    request: CloneRequest,
    /// What was typed, to bring the sheet back after a failure.
    form: CloneForm,
    progress: dialogs::Progress,
    cancel: Arc<AtomicBool>,
}

pub struct CloneSheet {
    form: Option<CloneForm>,
    home: Option<PathBuf>,
    job: Option<Job>,
    next_id: u64,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
}

impl Default for CloneSheet {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self { form: None, home: None, job: None, next_id: 0, tx, rx }
    }
}

impl CloneSheet {
    /// The sheet is up (terminals yield the keyboard to it).
    pub fn is_open(&self) -> bool {
        self.form.is_some()
    }

    /// Open the sheet (a no-op while it is open). The clipboard and ssh config are read on a
    /// worker; they fill the form when they arrive.
    pub fn open(&mut self, ctx: &egui::Context, settings: &Settings) {
        if self.form.is_some() {
            return;
        }
        self.home = crate::paths::home();
        self.form = Some(CloneForm::new(&settings.clone_dir()));
        let home = self.home.clone();
        super::jobs::spawn(ctx, &self.tx, move || {
            let clipboard = arboard::Clipboard::new().and_then(|mut c| c.get_text()).ok();
            let config = home.and_then(|h| std::fs::read_to_string(h.join(".ssh").join("config")).ok());
            Reply::Prefill {
                clipboard,
                hosts: config.map(|c| crate::ssh::config_hosts(&c)).unwrap_or_default(),
            }
        });
    }

    /// Draw the sheet and the progress pill; returns what the app must do.
    pub fn show(&mut self, ctx: &egui::Context, colors: &Colors, settings: &Settings) -> Vec<CloneEvent> {
        let mut events = self.drain();
        if let Some(form) = self.form.take() {
            self.form = self.sheet(ctx, colors, settings, form, &mut events);
        }
        if let Some(job) = &self.job {
            let screen = ctx.content_rect();
            let at = egui::pos2(screen.right() - 16.0, screen.bottom() - 36.0);
            let cancel = egui::Area::new(egui::Id::new("amalgum_clone_progress"))
                .order(egui::Order::Foreground)
                .pivot(egui::Align2::RIGHT_BOTTOM)
                .fixed_pos(at)
                .show(ctx, |ui| dialogs::progress_pill(ui, colors, &job.progress, true))
                .inner;
            if cancel {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        events
    }

    fn drain(&mut self) -> Vec<CloneEvent> {
        let mut events = Vec::new();
        while let Ok(reply) = self.rx.try_recv() {
            match reply {
                Reply::Prefill { clipboard, hosts } => {
                    if let Some(form) = self.form.as_mut() {
                        form.set_hosts(hosts);
                        if let Some(text) = clipboard {
                            form.offer_clipboard(&text);
                        }
                    }
                }
                Reply::Progress(id, p) => {
                    if let Some(job) = self.job.as_mut().filter(|j| j.id == id) {
                        job.progress = p;
                    }
                }
                Reply::Done(id, result) => {
                    let Some(job) = self.job.take_if(|j| j.id == id) else { continue };
                    match result {
                        Ok(location) => {
                            events.push(CloneEvent::Toast(Toast::success(format!(
                                "Cloned {}",
                                job.request.name
                            ))));
                            events.push(CloneEvent::Cloned {
                                location,
                                name: job.request.name.clone(),
                                open: job.request.open_as_workspace,
                            });
                        }
                        Err(err) => {
                            events.push(CloneEvent::Toast(failure_toast(&err)));
                            if err.kind != NetErrorKind::Cancelled && self.form.is_none() {
                                self.form = Some(job.form);
                            }
                        }
                    }
                }
            }
        }
        events
    }

    /// One frame of the sheet; `None` once it closes.
    fn sheet(
        &mut self,
        ctx: &egui::Context,
        colors: &Colors,
        settings: &Settings,
        mut form: CloneForm,
        events: &mut Vec<CloneEvent>,
    ) -> Option<CloneForm> {
        let home = self.home.clone();
        let (url_err, dest_err) = (form.url_error(), form.dest_error(home.as_deref()));
        let (name_err, depth_err) = (form.name_error(), form.depth_error());
        let busy = self.job.is_some();
        let screen = ctx.content_rect();
        let at = egui::pos2(screen.center().x - 162.0, screen.top() + screen.height() * 0.12);
        let hosts = form.hosts.clone();
        let mut host_options: Vec<(Option<usize>, &str)> = vec![(None, "Off")];
        host_options.extend(hosts.iter().enumerate().map(|(i, h)| (Some(i), h.as_str())));
        let before = form.clone();

        let outcome = Popover::new("clone", "Clone Repository").primary("Clone").show(ctx, colors, at, |f| {
            f.note("Repository URL").text(&mut form.url, "git@github.com:owner/repo.git", url_err);
            f.dropdown("Clone on remote host", &mut form.host, &host_options);
            let dest_label = match before.host_name() {
                Some(host) => format!("Folder on {host}"),
                None => "Destination folder".to_string(),
            };
            f.note(&dest_label).text(&mut form.dest, "~/src", dest_err);
            f.note("Folder name").text(&mut form.name, "derived from the URL", name_err);
            f.checkbox(&mut form.shallow, "Shallow clone");
            if form.shallow {
                f.note("Depth (commits)").text(&mut form.depth, "1", depth_err);
            }
            f.checkbox(&mut form.open_as_workspace, "Open as workspace");
            if !form.name.trim().is_empty() && dest_err.is_none() {
                f.note(&format!("Clones into {}", before.target_label()));
            }
            if busy {
                f.note("Another clone is running; wait for it or cancel it.");
            }
        });

        if form.url != before.url {
            form.url_changed();
        }
        if form.name != before.name {
            form.name_changed();
        }
        if form.dest != before.dest {
            form.dest_changed();
        }
        if form.host != before.host {
            form.host_changed();
        }
        match outcome {
            FormOutcome::Open => Some(form),
            FormOutcome::Cancelled => None,
            FormOutcome::Submitted if busy => Some(form),
            FormOutcome::Submitted => match form.request(home.as_deref()) {
                Ok(request) => {
                    self.start(ctx, settings, request, form);
                    None
                }
                Err(message) => {
                    events.push(CloneEvent::Toast(Toast::warning(message, form.target_label())));
                    Some(form)
                }
            },
        }
    }

    /// Run the clone on a worker; progress and the result come back through `rx`.
    fn start(&mut self, ctx: &egui::Context, settings: &Settings, request: CloneRequest, form: CloneForm) {
        self.next_id += 1;
        let id = self.next_id;
        let cancel = Arc::new(AtomicBool::new(false));
        let mut git = Git::new(request.parent.clone());
        if let Some(bin) = settings.git.git_path.as_deref().filter(|b| !b.trim().is_empty()) {
            git.git_bin = bin.to_string();
        }
        if let Some(bin) = settings.ssh.ssh_path.as_deref().filter(|b| !b.trim().is_empty()) {
            git.ssh_bin = bin.to_string();
        }
        let progress = dialogs::Progress { phase: request.label(), percent: None };
        let (tx, ctx2, flag, req) = (self.tx.clone(), ctx.clone(), cancel.clone(), request.clone());
        std::thread::spawn(move || {
            let mut last: Option<(String, Option<u8>)> = None;
            let result = net::clone(&git, &req.url, &req.name, req.depth, &flag, &mut |p| {
                let key = (p.phase.clone(), p.percent);
                if last.as_ref() != Some(&key) {
                    last = Some(key);
                    let pill = dialogs::Progress { phase: p.phase.clone(), percent: p.percent };
                    if tx.send(Reply::Progress(id, pill)).is_ok() {
                        ctx2.request_repaint();
                    }
                }
            });
            if tx.send(Reply::Done(id, result)).is_ok() {
                ctx2.request_repaint();
            }
        });
        self.job = Some(Job { id, request, form, progress, cancel });
    }
}

/// The toast for a failed clone (§5.3 **Errors**, §5.24): the spec's sentence as the title, the
/// exact command and stderr as details.
fn failure_toast(err: &NetError) -> Toast {
    let details = err.git.as_ref().map(|g| format!("$ {}\n{}", g.command, g.stderr)).unwrap_or_default();
    match &err.kind {
        NetErrorKind::Cancelled => Toast::info("Clone cancelled"),
        NetErrorKind::HostKey(prompt) => {
            let key = prompt.as_ref().map_or_else(
                || "The host's key is unknown or has changed.".to_string(),
                |p| format!("{} key {} for {} is not trusted yet.", p.key_type, p.fingerprint, p.host),
            );
            Toast::error(
                err.message(),
                format!(
                    "{key}\nThe host-key dialog (W16) is not in this build, and keys are never accepted \
                     automatically: verify the fingerprint and connect once with `ssh` in a terminal, \
                     then clone again.\n\n{details}"
                ),
            )
        }
        NetErrorKind::AuthFailed | NetErrorKind::DestinationExists => Toast::error(err.message(), details),
        _ => Toast::error(format!("Clone failed: {}", err.message()), details),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitError;
    use crate::ssh::HostKeyPrompt;

    fn git_err(stderr: &str) -> NetError {
        NetError {
            kind: net::classify(stderr),
            git: Some(Box::new(GitError {
                command: "git clone --progress -- u conduit".into(),
                code: Some(128),
                stderr: stderr.into(),
            })),
        }
    }

    #[test]
    fn errors_use_the_spec_wording_and_keep_stderr() {
        let t = failure_toast(&git_err("git@github.com: Permission denied (publickey).\n"));
        assert_eq!(t.title, "Authentication failed. Check your SSH agent or credential helper.");
        assert!(
            t.details.as_deref().is_some_and(|d| d.contains("Permission denied") && d.contains("git clone"))
        );

        let t = failure_toast(&NetErrorKind::DestinationExists.into());
        assert_eq!(t.title, "Folder already exists");

        let t = failure_toast(&git_err("fatal: unable to access: Could not resolve host: github.com\n"));
        assert_eq!(t.title, "Clone failed: Offline");
    }

    #[test]
    fn unknown_host_key_names_w16_and_never_trusts() {
        let prompt = HostKeyPrompt {
            host: "github.com".into(),
            key_type: "ED25519".into(),
            fingerprint: "SHA256:abc".into(),
        };
        let err = NetError { kind: NetErrorKind::HostKey(Some(Box::new(prompt))), git: None };
        let t = failure_toast(&err);
        let details = t.details.expect("details");
        assert!(details.contains("W16") && details.contains("SHA256:abc"), "{details}");
    }

    #[test]
    fn cancel_is_quiet() {
        let t = failure_toast(&NetErrorKind::Cancelled.into());
        assert_eq!(t.level, super::super::chrome::ToastLevel::Info);
    }
}
