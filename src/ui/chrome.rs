//! Window chrome (§3, §5.24, §5.26, §5.29): the in-window menu bar generated from the keymap's
//! action table, the status bar, toasts (bottom-left over the sidebar, never over a terminal
//! cursor or the commit button), and the notification panel (W14).

use super::theme::Colors;
use crate::agent::notifications::Store;
use crate::agent::status::Status;
use crate::git::status::RepoOp;
use crate::model::keymap::{Action, Keymap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    /// `danger` toasts never auto-dismiss (§5.24).
    Error,
}

/// A toast: one human line, and the exact command + stderr one click away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub level: ToastLevel,
    pub title: String,
    pub details: Option<String>,
}

impl Toast {
    pub fn error(title: impl Into<String>, details: impl Into<String>) -> Self {
        Self { level: ToastLevel::Error, title: title.into(), details: Some(details.into()) }
    }
    pub fn info(title: impl Into<String>) -> Self {
        Self { level: ToastLevel::Info, title: title.into(), details: None }
    }
}

/// Toast stack. Non-error toasts auto-dismiss after 8 s unless hovered or expanded.
#[derive(Debug, Default)]
pub struct Toasts {
    // private
}

impl Toasts {
    pub fn push(&mut self, toast: Toast, now: f64) {
        todo!()
    }
    /// Draw the stack anchored bottom-left; `now` is `ctx.input(|i| i.time)`.
    pub fn show(&mut self, ctx: &egui::Context, colors: &Colors, now: f64) {
        todo!()
    }
}

/// Menu bar from `keymap::table()` grouped by `Menu`, each item labelled with its shortcut.
pub fn menu_bar(ui: &mut egui::Ui, keymap: &Keymap) -> Option<Action> {
    todo!()
}

/// Everything the status bar shows (W2 bottom row).
#[derive(Debug, Clone, Default)]
pub struct StatusInfo {
    pub branch: Option<String>,
    pub detached: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changed: usize,
    pub op: Option<(RepoOp, usize)>,
    /// Focused tab's agent status and label ("claude running 4m").
    pub agent: Option<(Status, String)>,
    pub ports: Vec<u16>,
    pub unread: usize,
    pub remote_state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusAction {
    OpenChanges,
    ToggleNotifications,
    OpenPort(u16),
    ContinueOp,
    AbortOp,
    CreateBranch,
}

pub fn status_bar(ui: &mut egui::Ui, info: &StatusInfo, colors: &Colors) -> Option<StatusAction> {
    todo!()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelAction {
    Focus { workspace: Option<String>, tab: Option<String> },
    Close,
}

/// The notification panel sliding in from the right (`Mod+Shift+I`, W14). Clicking a row
/// focuses its workspace/tab; **Clear all** empties the store; unread rows are bold.
pub fn notification_panel(
    ctx: &egui::Context,
    store: &mut Store,
    colors: &Colors,
    now: u64,
) -> Option<PanelAction> {
    todo!()
}
