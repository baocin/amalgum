//! Destructive confirmations (§5.20, W13), headless: which actions confirm, which may offer
//! **Don't ask again for this action**, the dialog's content, and its key handling. The one
//! modal that renders a [`Confirm`] is `ui::dialogs`; don't-ask choices persist in
//! `settings.toml` (`[general] dont_ask_again`, see [`crate::model::settings::Settings::should_confirm`]).

/// Every §5.20 use of the destructive confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfirmKind {
    DeleteUnmergedBranch,
    DeleteRemoteTag,
    DropStash,
    /// Whole files, or **Discard all changes** (§5.7).
    DiscardChanges,
    DiscardHunk,
    DiscardLines,
    ResetSoft,
    ResetMixed,
    ResetHard,
    /// Plain `--force`, never `--force-with-lease` (§5.10).
    ForcePush,
    AbortMerge,
    AbortRebase,
    RemoveDirtyWorktree,
    /// `git clean -fd`, **Delete untracked files** (§5.7).
    Clean,
    RemoveRemote,
    CloseWorkspaceWithAgent,
    CloseTabWithAgent,
    /// A tab whose foreground process is not the shell and not an agent (§5.27 `Mod+W`).
    CloseTabWithProcess,
    /// `tmux -L amalgum kill-server` on a host (§5.28 **Kill sessions**).
    KillRemoteSessions,
    /// W16: an unknown ssh host key.
    TrustHostKey,
}

impl ConfirmKind {
    pub const ALL: &'static [ConfirmKind] = &[
        Self::DeleteUnmergedBranch,
        Self::DeleteRemoteTag,
        Self::DropStash,
        Self::DiscardChanges,
        Self::DiscardHunk,
        Self::DiscardLines,
        Self::ResetSoft,
        Self::ResetMixed,
        Self::ResetHard,
        Self::ForcePush,
        Self::AbortMerge,
        Self::AbortRebase,
        Self::RemoveDirtyWorktree,
        Self::Clean,
        Self::RemoveRemote,
        Self::CloseWorkspaceWithAgent,
        Self::CloseTabWithAgent,
        Self::CloseTabWithProcess,
        Self::KillRemoteSessions,
        Self::TrustHostKey,
    ];

    /// §5.20: the checkbox appears for discard hunk, drop stash, soft reset, and close tab with
    /// a running non-agent process — and for nothing else.
    pub fn allows_dont_ask(self) -> bool {
        matches!(self, Self::DiscardHunk | Self::DropStash | Self::ResetSoft | Self::CloseTabWithProcess)
    }

    /// The stable name persisted in `settings.toml`. Never rename one.
    pub fn key(self) -> &'static str {
        match self {
            Self::DeleteUnmergedBranch => "delete-unmerged-branch",
            Self::DeleteRemoteTag => "delete-remote-tag",
            Self::DropStash => "drop-stash",
            Self::DiscardChanges => "discard-changes",
            Self::DiscardHunk => "discard-hunk",
            Self::DiscardLines => "discard-lines",
            Self::ResetSoft => "reset-soft",
            Self::ResetMixed => "reset-mixed",
            Self::ResetHard => "reset-hard",
            Self::ForcePush => "force-push",
            Self::AbortMerge => "abort-merge",
            Self::AbortRebase => "abort-rebase",
            Self::RemoveDirtyWorktree => "remove-dirty-worktree",
            Self::Clean => "clean",
            Self::RemoveRemote => "remove-remote",
            Self::CloseWorkspaceWithAgent => "close-workspace-with-agent",
            Self::CloseTabWithAgent => "close-tab-with-agent",
            Self::CloseTabWithProcess => "close-tab-with-process",
            Self::KillRemoteSessions => "kill-remote-sessions",
            Self::TrustHostKey => "trust-host-key",
        }
    }

    /// The primary button's default verb (§5.20 "states the verb").
    pub fn verb(self) -> &'static str {
        match self {
            Self::DeleteUnmergedBranch | Self::DeleteRemoteTag => "Delete",
            Self::DropStash => "Drop",
            Self::DiscardChanges | Self::DiscardHunk | Self::DiscardLines => "Discard",
            Self::ResetSoft | Self::ResetMixed | Self::ResetHard => "Reset",
            Self::ForcePush => "Force push",
            Self::AbortMerge | Self::AbortRebase => "Abort",
            Self::RemoveDirtyWorktree | Self::RemoveRemote => "Remove",
            Self::Clean => "Delete",
            Self::CloseWorkspaceWithAgent | Self::CloseTabWithAgent | Self::CloseTabWithProcess => "Close",
            Self::KillRemoteSessions => "Kill sessions",
            Self::TrustHostKey => "Trust",
        }
    }
}

/// §5.20: the body lists at most this many lost items by name, then "+N more".
pub const LOST_LIMIT: usize = 10;

/// An extra checkbox in the dialog, e.g. W13's "Also delete `origin/feat/oauth`".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmOption {
    pub label: String,
    pub checked: bool,
}

/// One destructive confirmation (W13). The checkbox fields are the dialog's live state; read
/// them after [`Decision::Confirm`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub kind: ConfirmKind,
    /// Names the exact target: "Delete branch `feat/x`?".
    pub title: String,
    /// The sentence above the list: "4 commits are not on `main` and will be lost…".
    pub detail: String,
    /// What is lost, by name: commit subjects, file paths, agent session ids.
    pub lost: Vec<String>,
    pub verb: String,
    pub options: Vec<ConfirmOption>,
    /// The **Don't ask again** checkbox; only honoured where [`ConfirmKind::allows_dont_ask`].
    pub dont_ask: bool,
}

impl Confirm {
    pub fn new(kind: ConfirmKind, title: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            detail: String::new(),
            lost: Vec::new(),
            verb: kind.verb().to_string(),
            options: Vec::new(),
            dont_ask: false,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn lost<S: Into<String>>(mut self, items: impl IntoIterator<Item = S>) -> Self {
        self.lost = items.into_iter().map(Into::into).collect();
        self
    }

    pub fn verb(mut self, verb: impl Into<String>) -> Self {
        self.verb = verb.into();
        self
    }

    pub fn option(mut self, label: impl Into<String>, checked: bool) -> Self {
        self.options.push(ConfirmOption { label: label.into(), checked });
        self
    }

    /// The lost list as shown: up to [`LOST_LIMIT`] names, then "+N more" (§5.20).
    pub fn lost_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.lost.iter().take(LOST_LIMIT).cloned().collect();
        if self.lost.len() > LOST_LIMIT {
            lines.push(format!("+{} more", self.lost.len() - LOST_LIMIT));
        }
        lines
    }

    /// Whether confirming should persist "don't ask again" for this kind.
    pub fn remember(&self) -> bool {
        self.dont_ask && self.kind.allows_dont_ask()
    }
}

/// A key press the dialog cares about; the UI maps physical keys through the keymap preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    Enter,
    ModEnter,
    Escape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Confirm,
    Cancel,
}

/// §5.20: `Esc` cancels, `Mod+Enter` confirms, and plain `Enter` does **not** confirm.
pub fn decide(press: Press) -> Option<Decision> {
    match press {
        Press::Enter => None,
        Press::ModEnter => Some(Decision::Confirm),
        Press::Escape => Some(Decision::Cancel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::Settings;

    #[test]
    fn dont_ask_is_offered_exactly_for_the_four_spec_actions() {
        let allowed: Vec<ConfirmKind> =
            ConfirmKind::ALL.iter().copied().filter(|k| k.allows_dont_ask()).collect();
        assert_eq!(
            allowed,
            [
                ConfirmKind::DropStash,
                ConfirmKind::DiscardHunk,
                ConfirmKind::ResetSoft,
                ConfirmKind::CloseTabWithProcess
            ]
        );
    }

    #[test]
    fn dont_ask_is_never_offered_for_the_spec_never_list() {
        for kind in [
            ConfirmKind::ResetHard,
            ConfirmKind::ForcePush,
            ConfirmKind::DeleteUnmergedBranch,
            ConfirmKind::TrustHostKey,
            ConfirmKind::CloseTabWithAgent,
            ConfirmKind::CloseWorkspaceWithAgent,
        ] {
            assert!(!kind.allows_dont_ask(), "{kind:?}");
        }
    }

    #[test]
    fn keys_are_unique_so_persisted_choices_never_collide() {
        let keys: std::collections::HashSet<&str> = ConfirmKind::ALL.iter().map(|k| k.key()).collect();
        assert_eq!(keys.len(), ConfirmKind::ALL.len());
    }

    #[test]
    fn verb_defaults_from_the_kind_and_can_be_overridden() {
        let c = Confirm::new(ConfirmKind::DeleteUnmergedBranch, "Delete branch `feat/x`?");
        assert_eq!(c.verb, "Delete");
        assert_eq!(c.verb("Delete both").verb, "Delete both");
        assert_eq!(Confirm::new(ConfirmKind::ForcePush, "t").verb, "Force push");
    }

    #[test]
    fn lost_list_shows_everything_up_to_ten() {
        let c = Confirm::new(ConfirmKind::Clean, "t").lost((1..=10).map(|i| format!("f{i}")));
        assert_eq!(c.lost_lines().len(), 10);
        assert_eq!(c.lost_lines().last().unwrap(), "f10");
    }

    #[test]
    fn lost_list_truncates_to_ten_then_plus_n_more() {
        let c = Confirm::new(ConfirmKind::Clean, "t").lost((1..=13).map(|i| format!("f{i}")));
        let lines = c.lost_lines();
        assert_eq!(lines.len(), 11);
        assert_eq!(lines[9], "f10");
        assert_eq!(lines[10], "+3 more");
    }

    #[test]
    fn enter_never_confirms_mod_enter_does_escape_cancels() {
        assert_eq!(decide(Press::Enter), None);
        assert_eq!(decide(Press::ModEnter), Some(Decision::Confirm));
        assert_eq!(decide(Press::Escape), Some(Decision::Cancel));
    }

    #[test]
    fn remember_requires_both_the_checkbox_and_an_allowed_kind() {
        let mut c = Confirm::new(ConfirmKind::DiscardHunk, "t");
        assert!(!c.remember());
        c.dont_ask = true;
        assert!(c.remember());
        let mut hard = Confirm::new(ConfirmKind::ResetHard, "t");
        hard.dont_ask = true;
        assert!(!hard.remember());
    }

    // ---- persistence in settings.toml ----

    #[test]
    fn every_kind_confirms_by_default() {
        let s = Settings::default();
        assert!(ConfirmKind::ALL.iter().all(|k| s.should_confirm(*k)));
    }

    #[test]
    fn dont_ask_round_trips_through_settings_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let mut s = Settings::default();
        assert!(s.set_dont_ask(ConfirmKind::DropStash));
        assert!(!s.set_dont_ask(ConfirmKind::DropStash), "already set: no change, no save");
        s.save(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert!(!loaded.should_confirm(ConfirmKind::DropStash));
        assert!(loaded.should_confirm(ConfirmKind::DiscardHunk));
    }

    #[test]
    fn never_list_kinds_cannot_be_silenced() {
        let mut s = Settings::default();
        assert!(!s.set_dont_ask(ConfirmKind::ResetHard));
        assert!(s.should_confirm(ConfirmKind::ResetHard));
    }

    #[test]
    fn a_hand_edited_never_list_entry_is_ignored() {
        let s: Settings =
            toml::from_str("[general]\ndont_ask_again = [\"force-push\", \"discard-hunk\"]\n").unwrap();
        assert!(s.should_confirm(ConfirmKind::ForcePush));
        assert!(!s.should_confirm(ConfirmKind::DiscardHunk));
    }

    #[test]
    fn older_files_and_unknown_keys_still_load() {
        let old: Settings = toml::from_str("[general]\navatars = false\n").unwrap();
        assert!(old.general.dont_ask_again.is_empty());
        let newer: Settings =
            toml::from_str("[general]\ndont_ask_again = [\"from-a-future-version\"]\n").unwrap();
        assert!(ConfirmKind::ALL.iter().all(|k| newer.should_confirm(*k)));
    }
}
