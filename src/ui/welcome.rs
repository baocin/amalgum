//! Welcome screen (§5.1, W1): Open Folder / Clone Repository / Connect to Host cards with their
//! shortcuts, Recent locations (Enter/double-click opens, Backspace removes, missing local paths
//! greyed with "Not found" + Remove), the drop hint, and the first-launch setup banner.

use super::theme::Colors;
use crate::git::Location;
use crate::model::keymap::Keymap;
use crate::model::state::Recent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WelcomeAction {
    OpenFolder,
    CloneRepository,
    ConnectToHost,
    OpenRecent(Location),
    RemoveRecent(Location),
    ClearRecents,
    SetUpHooks,
    SkipSetUp,
}

pub fn show(
    ui: &mut egui::Ui,
    recents: &[Recent],
    show_setup_banner: bool,
    keymap: &Keymap,
    colors: &Colors,
    now: u64,
) -> Option<WelcomeAction> {
    todo!()
}
