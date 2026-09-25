//! The sidebar (§3, §5.22, W2, W3): Repo › Remote › Workspace tree. Workspace rows show a
//! status dot (shape + color, §4), the name, a subtitle with location · branch (when it differs
//! from the name) · ↑↓ · dirty count, listening ports (clickable), and the last notification
//! line with its age. `⋯` on hover opens the same menu as right-click. Empty state: W20.

use super::theme::Colors;
use crate::agent::status::Status;
use crate::model::state::AppState;
use std::collections::HashMap;

/// Live, non-persisted facts for one workspace row.
#[derive(Debug, Clone, Default)]
pub struct RowInfo {
    pub status: Status,
    /// Heuristic-only status: draw the dot hollow.
    pub low_confidence: bool,
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub dirty: usize,
    pub ports: Vec<u16>,
    /// Last notification text and its Unix time.
    pub last_message: Option<(String, u64)>,
    /// "reconnecting 12s", "hibernated", …
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarAction {
    Select(String),
    NewWorkspace,
    Close(String),
    Rename(String, String),
    MoveUp(String),
    MoveDown(String),
    OpenPort(u16),
    RevealInFileManager(String),
    CopyPath(String),
    OpenInExternalTerminal(String),
    ToggleRepo(String),
    ForgetRepo(String),
}

pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    rows: &HashMap<String, RowInfo>,
    colors: &Colors,
    now: u64,
) -> Option<SidebarAction> {
    todo!()
}
