//! The action table (§5.19: "Palette entries are the single source of truth for shortcuts; the
//! menu bar is generated from the same table") and platform key resolution (§1, §7).
//!
//! Bindings are written in spec notation with `Mod` (`"Mod+Shift+K"`). A [`Preset`] resolves
//! `Mod` to physical keys: macOS `Mod`=⌘, `Alt`=⌥; Linux `Mod`=Ctrl+Shift, `Mod+Shift`=Ctrl+Alt,
//! `Mod+Alt`=Ctrl+Alt+Shift (bare Ctrl belongs to the terminal); the **Linux Super** preset
//! maps `Mod`=Super and keeps Shift/Alt as written.
//!
//! [`Action`] has one variant per row of §7.1–7.6 and per menu item of §5.26. Each has an
//! [`ActionDef`] in [`table`] with a stable id (`"git.push"`, used by settings export/import),
//! a palette label, default bindings, a [`Context`], and an optional [`Menu`].
//!
//! ## Spec deviations (documented, not bugs)
//! - §7.1 lists both "`Mod+Alt+↑` / `↓`" (previous/next workspace) and
//!   "`Mod+Alt+←/→/↑/↓`" (move focus between panes) as *Global* bindings: a genuine
//!   same-context chord conflict in the spec text itself. [`FocusPaneUp`](Action::FocusPaneUp)
//!   and [`FocusPaneDown`](Action::FocusPaneDown) ship with no default binding (still
//!   rebindable) so [`Previous`](Action::PreviousWorkspace)/[`NextWorkspace`](Action::NextWorkspace)
//!   keep `Mod+Alt+↑/↓` and the default table has zero conflicts.
//! - §7.5's "`n` / `N`" (next/previous hunk) is stored as `"n"` / `"Shift+N"`: `Chord` tracks
//!   shift as a modifier bit, not letter case, so the bare-capital shorthand is normalized to
//!   an explicit `Shift+` binding rather than taught to `Chord::parse`.
//! - macOS labels follow the stated "Apple order `⌃⌥⇧⌘`" rule (Control, Option, Shift, Command),
//!   which renders `Mod+Shift+K` as `⇧⌘K` — matching real macOS menu conventions (e.g. Finder's
//!   `⇧⌘N`) even though the prose example elsewhere writes it as `⌘⇧K`.
//! - Mouse-only rows (`Mod+click`, `Shift+click`) have no keyboard [`Chord`] representation and
//!   are not modeled as actions; their keyboard equivalents (`Shift+↓/↑` etc.) are.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mods {
    /// The spec's `Mod`.
    pub primary: bool,
    pub shift: bool,
    pub alt: bool,
}

/// Keys we bind. Letters are stored lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Tab,
    Space,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Chord {
    pub mods: Mods,
    pub key: Key,
}

impl Chord {
    /// Parse spec notation: `Mod+Shift+K`, `Mod+/`, `Mod+Alt+←`, `Shift+PageUp`, `j`, `?`,
    /// `Esc`, `Enter`, `F2`, `Space`, `Backspace`. Arrows accept `←→↑↓` or names.
    pub fn parse(s: &str) -> Result<Self, String> {
        if s.is_empty() {
            return Err("empty chord".to_string());
        }
        let parts: Vec<&str> = s.split('+').collect();
        let (mod_tokens, key_token) = parts.split_at(parts.len() - 1);
        let key_token = key_token[0];
        if key_token.is_empty() {
            return Err(format!("empty key in {s:?}"));
        }
        let mut mods = Mods::default();
        for m in mod_tokens {
            match *m {
                "Mod" => mods.primary = true,
                "Shift" => mods.shift = true,
                "Alt" => mods.alt = true,
                other => return Err(format!("unknown modifier {other:?} in {s:?}")),
            }
        }
        let key = Self::parse_key(key_token)?;
        Ok(Chord { mods, key })
    }

    fn parse_key(token: &str) -> Result<Key, String> {
        let key = match token {
            "Enter" | "Return" => Key::Enter,
            "Esc" | "Escape" => Key::Escape,
            "Tab" => Key::Tab,
            "Space" => Key::Space,
            "Backspace" => Key::Backspace,
            "Up" | "↑" => Key::Up,
            "Down" | "↓" => Key::Down,
            "Left" | "←" => Key::Left,
            "Right" | "→" => Key::Right,
            "Home" => Key::Home,
            "End" => Key::End,
            "PageUp" => Key::PageUp,
            "PageDown" => Key::PageDown,
            "F2" => Key::F2,
            other => {
                let mut chars = other.chars();
                let Some(c) = chars.next() else { return Err(format!("empty key {other:?}")) };
                if chars.next().is_some() {
                    return Err(format!("unknown key {other:?}"));
                }
                Key::Char(c.to_lowercase().next().unwrap_or(c))
            }
        };
        Ok(key)
    }

    /// Back to spec notation (canonical order `Mod+Shift+Alt+Key`).
    pub fn spec(&self) -> String {
        let mut s = String::new();
        if self.mods.primary {
            s.push_str("Mod+");
        }
        if self.mods.shift {
            s.push_str("Shift+");
        }
        if self.mods.alt {
            s.push_str("Alt+");
        }
        s.push_str(&self.key_spec());
        s
    }

    fn key_spec(&self) -> String {
        match self.key {
            Key::Enter => "Enter".to_string(),
            Key::Escape => "Esc".to_string(),
            Key::Tab => "Tab".to_string(),
            Key::Space => "Space".to_string(),
            Key::Backspace => "Backspace".to_string(),
            Key::Up => "↑".to_string(),
            Key::Down => "↓".to_string(),
            Key::Left => "←".to_string(),
            Key::Right => "→".to_string(),
            Key::Home => "Home".to_string(),
            Key::End => "End".to_string(),
            Key::PageUp => "PageUp".to_string(),
            Key::PageDown => "PageDown".to_string(),
            Key::F2 => "F2".to_string(),
            Key::Char(c) => {
                if self.mods.primary {
                    c.to_uppercase().collect()
                } else {
                    c.to_string()
                }
            }
        }
    }
}

/// Physical modifier state as reported by the windowing layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Physical {
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub sup: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Preset {
    MacOs,
    LinuxCtrlShift,
    LinuxSuper,
}

impl Preset {
    /// Default preset for this OS, chosen via `platform::primary_modifier_name()`.
    pub fn native() -> Self {
        if crate::platform::primary_modifier_name() == "⌘" { Preset::MacOs } else { Preset::LinuxCtrlShift }
    }

    /// Physical modifiers for a chord's modifiers; `None` if the preset cannot express it
    /// (e.g. `Mod+Shift+Alt` under Linux Ctrl+Shift).
    pub fn resolve(self, mods: Mods) -> Option<Physical> {
        match self {
            Preset::MacOs => {
                Some(Physical { cmd: mods.primary, alt: mods.alt, shift: mods.shift, ..Physical::default() })
            }
            Preset::LinuxSuper => {
                Some(Physical { sup: mods.primary, alt: mods.alt, shift: mods.shift, ..Physical::default() })
            }
            Preset::LinuxCtrlShift => match (mods.primary, mods.shift, mods.alt) {
                (false, shift, alt) => Some(Physical { shift, alt, ..Physical::default() }),
                (true, false, false) => Some(Physical { ctrl: true, shift: true, ..Physical::default() }),
                (true, true, false) => Some(Physical { ctrl: true, alt: true, ..Physical::default() }),
                (true, false, true) => {
                    Some(Physical { ctrl: true, alt: true, shift: true, ..Physical::default() })
                }
                (true, true, true) => None,
            },
        }
    }

    /// Inverse of [`Preset::resolve`], for matching key events.
    pub fn interpret(self, physical: Physical) -> Option<Mods> {
        match self {
            Preset::MacOs => {
                if physical.ctrl || physical.sup {
                    return None;
                }
                Some(Mods { primary: physical.cmd, shift: physical.shift, alt: physical.alt })
            }
            Preset::LinuxSuper => {
                if physical.ctrl || physical.cmd {
                    return None;
                }
                Some(Mods { primary: physical.sup, shift: physical.shift, alt: physical.alt })
            }
            Preset::LinuxCtrlShift => {
                if physical.cmd || physical.sup {
                    return None;
                }
                match (physical.ctrl, physical.alt, physical.shift) {
                    (false, false, false) => Some(Mods::default()),
                    (false, true, false) => Some(Mods { alt: true, ..Mods::default() }),
                    (false, false, true) => Some(Mods { shift: true, ..Mods::default() }),
                    (false, true, true) => Some(Mods { shift: true, alt: true, ..Mods::default() }),
                    (true, false, true) => Some(Mods { primary: true, ..Mods::default() }),
                    (true, true, true) => Some(Mods { primary: true, alt: true, ..Mods::default() }),
                    (true, true, false) => Some(Mods { primary: true, shift: true, ..Mods::default() }),
                    // Bare Ctrl (no Shift) belongs to the terminal, not a `Mod` combo.
                    (true, false, false) => None,
                }
            }
        }
    }

    /// Tooltip/menu label: `⌘⇧K` on macOS, `Ctrl+Shift+K` / `Super+K` on Linux.
    pub fn label(self, chord: &Chord) -> String {
        let Some(physical) = self.resolve(chord.mods) else {
            // Unrepresentable in this preset (e.g. Mod+Shift+Alt on Linux Ctrl+Shift):
            // fall back to the canonical spec notation rather than lying about the keys.
            return chord.spec();
        };
        match self {
            Preset::MacOs => mac_label(physical, chord.key),
            Preset::LinuxCtrlShift | Preset::LinuxSuper => linux_label(physical, chord.key),
        }
    }
}

fn mac_label(p: Physical, key: Key) -> String {
    let mut s = String::new();
    // Apple order: Control, Option, Shift, Command.
    if p.ctrl {
        s.push('⌃');
    }
    if p.alt {
        s.push('⌥');
    }
    if p.shift {
        s.push('⇧');
    }
    if p.cmd {
        s.push('⌘');
    }
    let any_mod = p.ctrl || p.alt || p.shift || p.cmd || p.sup;
    s.push_str(&mac_key_name(key, any_mod));
    s
}

fn mac_key_name(key: Key, any_mod: bool) -> String {
    match key {
        Key::Enter => "↩".to_string(),
        Key::Escape => "⎋".to_string(),
        Key::Backspace => "⌫".to_string(),
        Key::Tab => "Tab".to_string(),
        Key::Space => "Space".to_string(),
        Key::Up => "↑".to_string(),
        Key::Down => "↓".to_string(),
        Key::Left => "←".to_string(),
        Key::Right => "→".to_string(),
        Key::Home => "Home".to_string(),
        Key::End => "End".to_string(),
        Key::PageUp => "PageUp".to_string(),
        Key::PageDown => "PageDown".to_string(),
        Key::F2 => "F2".to_string(),
        Key::Char(c) => {
            if any_mod {
                c.to_uppercase().collect()
            } else {
                c.to_string()
            }
        }
    }
}

fn linux_label(p: Physical, key: Key) -> String {
    let mut parts = Vec::new();
    if p.ctrl {
        parts.push("Ctrl".to_string());
    }
    if p.alt {
        parts.push("Alt".to_string());
    }
    if p.shift {
        parts.push("Shift".to_string());
    }
    if p.sup {
        parts.push("Super".to_string());
    }
    let any_mod = p.ctrl || p.alt || p.shift || p.sup || p.cmd;
    parts.push(linux_key_name(key, any_mod));
    parts.join("+")
}

fn linux_key_name(key: Key, any_mod: bool) -> String {
    match key {
        Key::Enter => "Enter".to_string(),
        Key::Escape => "Esc".to_string(),
        Key::Backspace => "Backspace".to_string(),
        Key::Tab => "Tab".to_string(),
        Key::Space => "Space".to_string(),
        Key::Up => "Up".to_string(),
        Key::Down => "Down".to_string(),
        Key::Left => "Left".to_string(),
        Key::Right => "Right".to_string(),
        Key::Home => "Home".to_string(),
        Key::End => "End".to_string(),
        Key::PageUp => "PageUp".to_string(),
        Key::PageDown => "PageDown".to_string(),
        Key::F2 => "F2".to_string(),
        Key::Char(c) => {
            if any_mod {
                c.to_uppercase().collect()
            } else {
                c.to_string()
            }
        }
    }
}

/// Where a binding applies. With a terminal focused only chords containing `Mod` are
/// intercepted; everything else reaches the PTY (§5.27 "Input mapping").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Context {
    /// Everywhere (subject to the terminal rule above).
    Global,
    /// Any git pane view but not a terminal (Undo/Redo, §7.1 note).
    GitPane,
    Terminal,
    Graph,
    /// Details, diff, and Changes (§7.5).
    Details,
    /// Refs view and sidebar (§7.6).
    Refs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Menu {
    App,
    File,
    Edit,
    View,
    Workspace,
    Repository,
    Window,
    Help,
}

/// One variant per palette action: every row of §7.1–7.6 (rows naming several actions, like
/// `Mod+O` / `Mod+Shift+O` / `Mod+Shift+K`, become separate variants; rows naming one action
/// bound to several keys, like `j`/`↓` for "move selection", stay one variant with several
/// bindings) plus every §5.26 menu item not already covered by a §7 row. See the module doc
/// for the two spec inconsistencies this resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    // ---- §7.1 Global: palette, window, workspaces, tabs, panes ----
    CommandPalette,
    ShortcutOverlay,
    OpenFolder,
    CloneRepository,
    ConnectToHost,
    NewWorkspace,
    CloseWorkspace,
    Workspace1,
    Workspace2,
    Workspace3,
    Workspace4,
    Workspace5,
    Workspace6,
    Workspace7,
    Workspace8,
    Workspace9,
    PreviousWorkspace,
    NextWorkspace,
    NewTerminalTab,
    /// `Mod+W`: closes the focused terminal tab, or the focused split pane within it.
    CloseTabOrPane,
    PreviousTerminalTab,
    NextTerminalTab,
    ReopenClosedTab,
    SplitRight,
    SplitDown,
    FocusPaneLeft,
    FocusPaneRight,
    /// No default binding: the spec's `Mod+Alt+↑` is claimed by [`PreviousWorkspace`] in the
    /// same (Global) context. See the module doc.
    FocusPaneUp,
    /// No default binding; see [`FocusPaneUp`].
    FocusPaneDown,
    /// `Mod+Shift+Enter`: fills the split's tab with the focused pane, or restores it.
    ZoomPane,
    ClosePane,

    // ---- §7.1 Global: git pane and view toggles ----
    ToggleGitPane,
    ToggleSidebar,
    /// `Mod+\`: fills the whole window with the focused pane (distinct from [`ZoomPane`],
    /// which only fills the pane's tab).
    MaximizePane,
    NotificationPanel,
    FocusGraph,
    GitPaneChangesView,
    GitPaneRefsView,
    FocusTerminalFromGitPane,
    Settings,

    // ---- §7.1 Global: undo, git pane commands ----
    /// Git pane undo (`Mod+Z`); [`Context::GitPane`], not Global (§7.1 note).
    Undo,
    /// Git pane redo (`Mod+Shift+Z`); [`Context::GitPane`].
    Redo,
    UndoHistoryPanel,
    FetchBoundRemote,
    Pull,
    Push,
    NewBranch,
    Stash,
    NewTag,
    InteractiveRebaseFromSelected,
    FileHistoryOfFocusedFile,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleTheme,

    // ---- §7.2 Terminal focused ----
    /// Copies the terminal selection ([`Context::Terminal`]); distinct from the generic
    /// [`Copy`] Edit-menu action.
    TerminalCopy,
    TerminalPaste,
    SearchInTerminal,
    ClearScrollback,
    ScrollToTop,
    ScrollToBottom,
    ScrollPageUp,
    ScrollPageDown,
    PreviousPrompt,
    NextPrompt,
    /// `Mod+Shift+R` while a terminal is focused: overrides [`InteractiveRebaseFromSelected`]
    /// (intentional, different [`Context`]s — see the module doc).
    RunCommand,

    // ---- §7.3 Git pane, Graph view ----
    MoveSelectionDown,
    MoveSelectionUp,
    ExtendSelectionDown,
    ExtendSelectionUp,
    FirstCommit,
    LastCommit,
    FocusDetails,
    SearchCommits,
    FocusModeOnSelectedBranch,
    CheckoutSelected,
    CreateTagAtSelectedCommit,
    CreateBranchAtSelectedCommit,
    MergeSelectedIntoCurrent,
    RebaseCurrentOntoSelected,
    CherryPickSelectedOntoCurrent,
    ToggleCollapseMergedBranch,
    CopyHash,
    CopySubject,
    CommitContextMenu,
    /// `Esc`: clear selection / exit focus mode / exit search / back to terminal (§7.3).
    GraphBack,

    // ---- §7.5 Git pane, details / diff / Changes ----
    NextFile,
    PreviousFile,
    NextHunk,
    PreviousHunk,
    ToggleTreeFlatFileList,
    ToggleUnifiedSplit,
    ToggleWhitespace,
    StageFocusedFile,
    UnstageFocusedFile,
    StageAll,
    UnstageAll,
    StageSelectedLinesOrHunk,
    DiscardSelectedLinesOrHunk,
    /// Commit from the message editor (`Mod+Enter`); also Repository menu "Commit" (§5.7,
    /// §5.26).
    Commit,
    ToggleAmend,
    BlameFocusedFile,
    OpenFileInDefaultApp,
    /// `Esc`: back to graph (§7.5).
    DetailsBack,
    /// The Changes view's "Commit and push → origin" split button (§5.7); not a §7 row.
    CommitAndPush,

    // ---- §7.6 Git pane, Refs view and sidebar ----
    MoveDownRefs,
    MoveUpRefs,
    CollapseSection,
    ExpandSection,
    CheckoutOrOpen,
    RenameRef,
    DeleteOrClose,
    FilterRefs,
    /// `Esc`: clear filter / back (§7.6).
    RefsBack,
    /// Refs row context menu "Set upstream…" (§5.9); not a §7 row.
    SetUpstream,

    // ---- §5.26 menu items with no §7 keyboard row ----
    About,
    Quit,
    Hibernate,
    Resume,
    Reconnect,
    MinimizeWindow,
    /// Window "Zoom" (native window zoom/maximize); distinct from the pane [`ZoomIn`] /
    /// [`ZoomOut`] / [`ZoomReset`] font-size zoom.
    ZoomWindow,
    Cut,
    /// Generic Edit-menu copy (text fields); distinct from [`TerminalCopy`] and [`CopyHash`].
    Copy,
    Paste,
    SelectAll,
    Find,
    ShowHideRemotes,
    ShowHideTags,
    ShowHideStashes,
    /// Repository "Force push ▸ Force push with lease" default (§5.10); plain `--force` is an
    /// Alt-held dropdown state, not a separate bound action.
    ForcePushWithLease,
    SetUpAgentHooks,
    OpenLogFolder,
}

impl Action {
    /// Every variant, independent of [`table`]'s own listing (used to test that the two agree).
    pub const ALL: &'static [Action] = &[
        Action::CommandPalette,
        Action::ShortcutOverlay,
        Action::OpenFolder,
        Action::CloneRepository,
        Action::ConnectToHost,
        Action::NewWorkspace,
        Action::CloseWorkspace,
        Action::Workspace1,
        Action::Workspace2,
        Action::Workspace3,
        Action::Workspace4,
        Action::Workspace5,
        Action::Workspace6,
        Action::Workspace7,
        Action::Workspace8,
        Action::Workspace9,
        Action::PreviousWorkspace,
        Action::NextWorkspace,
        Action::NewTerminalTab,
        Action::CloseTabOrPane,
        Action::PreviousTerminalTab,
        Action::NextTerminalTab,
        Action::ReopenClosedTab,
        Action::SplitRight,
        Action::SplitDown,
        Action::FocusPaneLeft,
        Action::FocusPaneRight,
        Action::FocusPaneUp,
        Action::FocusPaneDown,
        Action::ZoomPane,
        Action::ClosePane,
        Action::ToggleGitPane,
        Action::ToggleSidebar,
        Action::MaximizePane,
        Action::NotificationPanel,
        Action::FocusGraph,
        Action::GitPaneChangesView,
        Action::GitPaneRefsView,
        Action::FocusTerminalFromGitPane,
        Action::Settings,
        Action::Undo,
        Action::Redo,
        Action::UndoHistoryPanel,
        Action::FetchBoundRemote,
        Action::Pull,
        Action::Push,
        Action::NewBranch,
        Action::Stash,
        Action::NewTag,
        Action::InteractiveRebaseFromSelected,
        Action::FileHistoryOfFocusedFile,
        Action::ZoomIn,
        Action::ZoomOut,
        Action::ZoomReset,
        Action::ToggleTheme,
        Action::TerminalCopy,
        Action::TerminalPaste,
        Action::SearchInTerminal,
        Action::ClearScrollback,
        Action::ScrollToTop,
        Action::ScrollToBottom,
        Action::ScrollPageUp,
        Action::ScrollPageDown,
        Action::PreviousPrompt,
        Action::NextPrompt,
        Action::RunCommand,
        Action::MoveSelectionDown,
        Action::MoveSelectionUp,
        Action::ExtendSelectionDown,
        Action::ExtendSelectionUp,
        Action::FirstCommit,
        Action::LastCommit,
        Action::FocusDetails,
        Action::SearchCommits,
        Action::FocusModeOnSelectedBranch,
        Action::CheckoutSelected,
        Action::CreateTagAtSelectedCommit,
        Action::CreateBranchAtSelectedCommit,
        Action::MergeSelectedIntoCurrent,
        Action::RebaseCurrentOntoSelected,
        Action::CherryPickSelectedOntoCurrent,
        Action::ToggleCollapseMergedBranch,
        Action::CopyHash,
        Action::CopySubject,
        Action::CommitContextMenu,
        Action::GraphBack,
        Action::NextFile,
        Action::PreviousFile,
        Action::NextHunk,
        Action::PreviousHunk,
        Action::ToggleTreeFlatFileList,
        Action::ToggleUnifiedSplit,
        Action::ToggleWhitespace,
        Action::StageFocusedFile,
        Action::UnstageFocusedFile,
        Action::StageAll,
        Action::UnstageAll,
        Action::StageSelectedLinesOrHunk,
        Action::DiscardSelectedLinesOrHunk,
        Action::Commit,
        Action::ToggleAmend,
        Action::BlameFocusedFile,
        Action::OpenFileInDefaultApp,
        Action::DetailsBack,
        Action::CommitAndPush,
        Action::MoveDownRefs,
        Action::MoveUpRefs,
        Action::CollapseSection,
        Action::ExpandSection,
        Action::CheckoutOrOpen,
        Action::RenameRef,
        Action::DeleteOrClose,
        Action::FilterRefs,
        Action::RefsBack,
        Action::SetUpstream,
        Action::About,
        Action::Quit,
        Action::Hibernate,
        Action::Resume,
        Action::Reconnect,
        Action::MinimizeWindow,
        Action::ZoomWindow,
        Action::Cut,
        Action::Copy,
        Action::Paste,
        Action::SelectAll,
        Action::Find,
        Action::ShowHideRemotes,
        Action::ShowHideTags,
        Action::ShowHideStashes,
        Action::ForcePushWithLease,
        Action::SetUpAgentHooks,
        Action::OpenLogFolder,
    ];
}

pub struct ActionDef {
    pub action: Action,
    pub id: &'static str,
    pub label: &'static str,
    pub bindings: &'static [&'static str],
    pub context: Context,
    pub menu: Option<Menu>,
}

static TABLE: &[ActionDef] = &[
    ActionDef {
        action: Action::CommandPalette,
        id: "palette.open",
        label: "Command Palette",
        bindings: &["Mod+K", "Mod+Shift+P"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::ShortcutOverlay,
        id: "help.keyboard-shortcuts",
        label: "Keyboard Shortcuts",
        bindings: &["Mod+/", "?"],
        context: Context::Global,
        menu: Some(Menu::Help),
    },
    ActionDef {
        action: Action::OpenFolder,
        id: "file.open-folder",
        label: "Open Folder",
        bindings: &["Mod+O"],
        context: Context::Global,
        menu: Some(Menu::File),
    },
    ActionDef {
        action: Action::CloneRepository,
        id: "file.clone",
        label: "Clone Repository",
        bindings: &["Mod+Shift+O"],
        context: Context::Global,
        menu: Some(Menu::File),
    },
    ActionDef {
        action: Action::ConnectToHost,
        id: "file.connect-to-host",
        label: "Connect to Host",
        bindings: &["Mod+Shift+K"],
        context: Context::Global,
        menu: Some(Menu::File),
    },
    ActionDef {
        action: Action::NewWorkspace,
        id: "workspace.new",
        label: "New Workspace",
        bindings: &["Mod+N"],
        context: Context::Global,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::CloseWorkspace,
        id: "file.close-workspace",
        label: "Close Workspace",
        bindings: &["Mod+Shift+W"],
        context: Context::Global,
        menu: Some(Menu::File),
    },
    ActionDef {
        action: Action::Workspace1,
        id: "workspace.switch-1",
        label: "Switch to Workspace 1",
        bindings: &["Mod+1"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::Workspace2,
        id: "workspace.switch-2",
        label: "Switch to Workspace 2",
        bindings: &["Mod+2"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::Workspace3,
        id: "workspace.switch-3",
        label: "Switch to Workspace 3",
        bindings: &["Mod+3"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::Workspace4,
        id: "workspace.switch-4",
        label: "Switch to Workspace 4",
        bindings: &["Mod+4"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::Workspace5,
        id: "workspace.switch-5",
        label: "Switch to Workspace 5",
        bindings: &["Mod+5"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::Workspace6,
        id: "workspace.switch-6",
        label: "Switch to Workspace 6",
        bindings: &["Mod+6"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::Workspace7,
        id: "workspace.switch-7",
        label: "Switch to Workspace 7",
        bindings: &["Mod+7"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::Workspace8,
        id: "workspace.switch-8",
        label: "Switch to Workspace 8",
        bindings: &["Mod+8"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::Workspace9,
        id: "workspace.switch-9",
        label: "Switch to Workspace 9",
        bindings: &["Mod+9"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::PreviousWorkspace,
        id: "workspace.previous",
        label: "Previous Workspace",
        bindings: &["Mod+Alt+↑"],
        context: Context::Global,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::NextWorkspace,
        id: "workspace.next",
        label: "Next Workspace",
        bindings: &["Mod+Alt+↓"],
        context: Context::Global,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::NewTerminalTab,
        id: "workspace.new-terminal-tab",
        label: "New Terminal Tab",
        bindings: &["Mod+T"],
        context: Context::Global,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::CloseTabOrPane,
        id: "workspace.close-tab-or-pane",
        label: "Close Tab",
        bindings: &["Mod+W"],
        context: Context::Global,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::PreviousTerminalTab,
        id: "workspace.previous-tab",
        label: "Previous Tab",
        bindings: &["Mod+Shift+["],
        context: Context::Global,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::NextTerminalTab,
        id: "workspace.next-tab",
        label: "Next Tab",
        bindings: &["Mod+Shift+]"],
        context: Context::Global,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::ReopenClosedTab,
        id: "workspace.reopen-closed-tab",
        label: "Reopen Closed Tab",
        bindings: &["Mod+Alt+T"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::SplitRight,
        id: "workspace.split-right",
        label: "Split Right",
        bindings: &["Mod+D"],
        context: Context::Global,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::SplitDown,
        id: "workspace.split-down",
        label: "Split Down",
        bindings: &["Mod+Shift+D"],
        context: Context::Global,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::FocusPaneLeft,
        id: "workspace.focus-pane-left",
        label: "Focus Pane Left",
        bindings: &["Mod+Alt+←"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::FocusPaneRight,
        id: "workspace.focus-pane-right",
        label: "Focus Pane Right",
        bindings: &["Mod+Alt+→"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::FocusPaneUp,
        id: "workspace.focus-pane-up",
        label: "Focus Pane Up",
        bindings: &[],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::FocusPaneDown,
        id: "workspace.focus-pane-down",
        label: "Focus Pane Down",
        bindings: &[],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::ZoomPane,
        id: "workspace.zoom-pane",
        label: "Zoom Pane",
        bindings: &["Mod+Shift+Enter"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::ClosePane,
        id: "workspace.close-pane",
        label: "Close Pane",
        bindings: &["Mod+Alt+W"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::ToggleGitPane,
        id: "view.toggle-git-pane",
        label: "Toggle Git Pane",
        bindings: &["Mod+J"],
        context: Context::Global,
        menu: Some(Menu::View),
    },
    ActionDef {
        action: Action::ToggleSidebar,
        id: "view.toggle-sidebar",
        label: "Toggle Sidebar",
        bindings: &["Mod+Shift+1"],
        context: Context::Global,
        menu: Some(Menu::View),
    },
    ActionDef {
        action: Action::MaximizePane,
        id: "view.maximize-pane",
        label: "Maximize Pane",
        bindings: &["Mod+\\"],
        context: Context::Global,
        menu: Some(Menu::View),
    },
    ActionDef {
        action: Action::NotificationPanel,
        id: "view.notifications",
        label: "Notification Panel",
        bindings: &["Mod+Shift+I"],
        context: Context::Global,
        menu: Some(Menu::View),
    },
    ActionDef {
        action: Action::FocusGraph,
        id: "git.focus-graph",
        label: "Focus Git Pane Graph",
        bindings: &["Mod+Shift+G"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::GitPaneChangesView,
        id: "git.changes-view",
        label: "Changes View",
        bindings: &["Mod+Shift+C"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::GitPaneRefsView,
        id: "git.refs-view",
        label: "Refs View",
        bindings: &["Mod+Shift+E"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::FocusTerminalFromGitPane,
        id: "workspace.focus-terminal",
        label: "Focus Terminal",
        bindings: &["Mod+Alt+Enter"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::Settings,
        id: "app.settings",
        label: "Settings",
        bindings: &["Mod+,"],
        context: Context::Global,
        menu: Some(Menu::App),
    },
    ActionDef {
        action: Action::Undo,
        id: "git.undo",
        label: "Undo",
        bindings: &["Mod+Z"],
        context: Context::GitPane,
        menu: Some(Menu::Edit),
    },
    ActionDef {
        action: Action::Redo,
        id: "git.redo",
        label: "Redo",
        bindings: &["Mod+Shift+Z"],
        context: Context::GitPane,
        menu: Some(Menu::Edit),
    },
    ActionDef {
        action: Action::UndoHistoryPanel,
        id: "git.undo-history",
        label: "Undo History",
        bindings: &["Mod+Alt+Z"],
        context: Context::Global,
        menu: Some(Menu::Repository),
    },
    ActionDef {
        action: Action::FetchBoundRemote,
        id: "git.fetch",
        label: "Fetch",
        bindings: &["Mod+Shift+F"],
        context: Context::Global,
        menu: Some(Menu::Repository),
    },
    ActionDef {
        action: Action::Pull,
        id: "git.pull",
        label: "Pull",
        bindings: &["Mod+Shift+L"],
        context: Context::Global,
        menu: Some(Menu::Repository),
    },
    ActionDef {
        action: Action::Push,
        id: "git.push",
        label: "Push",
        bindings: &["Mod+P"],
        context: Context::Global,
        menu: Some(Menu::Repository),
    },
    ActionDef {
        action: Action::NewBranch,
        id: "git.new-branch",
        label: "New Branch",
        bindings: &["Mod+B"],
        context: Context::Global,
        menu: Some(Menu::Repository),
    },
    ActionDef {
        action: Action::Stash,
        id: "git.stash",
        label: "Stash",
        bindings: &["Mod+Shift+S"],
        context: Context::Global,
        menu: Some(Menu::Repository),
    },
    ActionDef {
        action: Action::NewTag,
        id: "git.new-tag",
        label: "New Tag",
        bindings: &["Mod+Shift+T"],
        context: Context::Global,
        menu: Some(Menu::Repository),
    },
    ActionDef {
        action: Action::InteractiveRebaseFromSelected,
        id: "git.interactive-rebase",
        label: "Interactive Rebase",
        bindings: &["Mod+Shift+R"],
        context: Context::Global,
        menu: Some(Menu::Repository),
    },
    ActionDef {
        action: Action::FileHistoryOfFocusedFile,
        id: "git.file-history",
        label: "File History",
        bindings: &["Mod+Shift+H"],
        context: Context::Global,
        menu: None,
    },
    ActionDef {
        action: Action::ZoomIn,
        id: "view.zoom-in",
        label: "Zoom In",
        bindings: &["Mod+="],
        context: Context::Global,
        menu: Some(Menu::View),
    },
    ActionDef {
        action: Action::ZoomOut,
        id: "view.zoom-out",
        label: "Zoom Out",
        bindings: &["Mod+-"],
        context: Context::Global,
        menu: Some(Menu::View),
    },
    ActionDef {
        action: Action::ZoomReset,
        id: "view.zoom-reset",
        label: "Zoom Reset",
        bindings: &["Mod+0"],
        context: Context::Global,
        menu: Some(Menu::View),
    },
    ActionDef {
        action: Action::ToggleTheme,
        id: "view.toggle-theme",
        label: "Toggle Theme",
        bindings: &["Mod+Alt+D"],
        context: Context::Global,
        menu: Some(Menu::View),
    },
    ActionDef {
        action: Action::TerminalCopy,
        id: "terminal.copy",
        label: "Copy",
        bindings: &["Mod+C"],
        context: Context::Terminal,
        menu: None,
    },
    ActionDef {
        action: Action::TerminalPaste,
        id: "terminal.paste",
        label: "Paste",
        bindings: &["Mod+V"],
        context: Context::Terminal,
        menu: None,
    },
    ActionDef {
        action: Action::SearchInTerminal,
        id: "terminal.search",
        label: "Search in Terminal",
        bindings: &["Mod+F"],
        context: Context::Terminal,
        menu: None,
    },
    ActionDef {
        action: Action::ClearScrollback,
        id: "terminal.clear-scrollback",
        label: "Clear Terminal",
        bindings: &["Mod+Alt+K"],
        context: Context::Terminal,
        menu: Some(Menu::Edit),
    },
    ActionDef {
        action: Action::ScrollToTop,
        id: "terminal.scroll-top",
        label: "Scroll to Top",
        bindings: &["Mod+Home"],
        context: Context::Terminal,
        menu: None,
    },
    ActionDef {
        action: Action::ScrollToBottom,
        id: "terminal.scroll-bottom",
        label: "Scroll to Bottom",
        bindings: &["Mod+End"],
        context: Context::Terminal,
        menu: None,
    },
    ActionDef {
        action: Action::ScrollPageUp,
        id: "terminal.scroll-page-up",
        label: "Scroll Page Up",
        bindings: &["Shift+PageUp"],
        context: Context::Terminal,
        menu: None,
    },
    ActionDef {
        action: Action::ScrollPageDown,
        id: "terminal.scroll-page-down",
        label: "Scroll Page Down",
        bindings: &["Shift+PageDown"],
        context: Context::Terminal,
        menu: None,
    },
    ActionDef {
        action: Action::PreviousPrompt,
        id: "terminal.previous-prompt",
        label: "Previous Prompt",
        bindings: &["Mod+↑"],
        context: Context::Terminal,
        menu: None,
    },
    ActionDef {
        action: Action::NextPrompt,
        id: "terminal.next-prompt",
        label: "Next Prompt",
        bindings: &["Mod+↓"],
        context: Context::Terminal,
        menu: None,
    },
    ActionDef {
        action: Action::RunCommand,
        id: "terminal.run",
        label: "Run…",
        bindings: &["Mod+Shift+R"],
        context: Context::Terminal,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::MoveSelectionDown,
        id: "graph.move-down",
        label: "Move Selection Down",
        bindings: &["j", "↓"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::MoveSelectionUp,
        id: "graph.move-up",
        label: "Move Selection Up",
        bindings: &["k", "↑"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::ExtendSelectionDown,
        id: "graph.extend-down",
        label: "Extend Selection Down",
        bindings: &["Shift+↓"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::ExtendSelectionUp,
        id: "graph.extend-up",
        label: "Extend Selection Up",
        bindings: &["Shift+↑"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::FirstCommit,
        id: "graph.first-commit",
        label: "First Commit",
        bindings: &["Mod+↑"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::LastCommit,
        id: "graph.last-commit",
        label: "Last Commit",
        bindings: &["Mod+↓"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::FocusDetails,
        id: "graph.focus-details",
        label: "Focus Details",
        bindings: &["Enter"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::SearchCommits,
        id: "graph.search",
        label: "Search Commits",
        bindings: &["/", "Mod+F"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::FocusModeOnSelectedBranch,
        id: "graph.focus-mode",
        label: "Focus Mode",
        bindings: &["f"],
        context: Context::Graph,
        menu: Some(Menu::View),
    },
    ActionDef {
        action: Action::CheckoutSelected,
        id: "graph.checkout",
        label: "Checkout",
        bindings: &["c"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::CreateTagAtSelectedCommit,
        id: "graph.create-tag",
        label: "Create Tag Here",
        bindings: &["t"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::CreateBranchAtSelectedCommit,
        id: "graph.create-branch",
        label: "Create Branch Here",
        bindings: &["b"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::MergeSelectedIntoCurrent,
        id: "graph.merge-into-current",
        label: "Merge into Current",
        bindings: &["m"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::RebaseCurrentOntoSelected,
        id: "graph.rebase-onto",
        label: "Rebase onto Selected",
        bindings: &["r"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::CherryPickSelectedOntoCurrent,
        id: "graph.cherry-pick",
        label: "Cherry-pick onto Current",
        bindings: &["p"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::ToggleCollapseMergedBranch,
        id: "graph.toggle-collapse",
        label: "Toggle Collapse Merged Branch",
        bindings: &["Space"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::CopyHash,
        id: "graph.copy-hash",
        label: "Copy Hash",
        bindings: &["Mod+C"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::CopySubject,
        id: "graph.copy-subject",
        label: "Copy Subject",
        bindings: &["Mod+Alt+C"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::CommitContextMenu,
        id: "graph.context-menu",
        label: "Commit Context Menu",
        bindings: &["Mod+Enter"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::GraphBack,
        id: "graph.back",
        label: "Back",
        bindings: &["Esc"],
        context: Context::Graph,
        menu: None,
    },
    ActionDef {
        action: Action::NextFile,
        id: "details.next-file",
        label: "Next File",
        bindings: &["↓"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::PreviousFile,
        id: "details.previous-file",
        label: "Previous File",
        bindings: &["↑"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::NextHunk,
        id: "details.next-hunk",
        label: "Next Hunk",
        bindings: &["n"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::PreviousHunk,
        id: "details.previous-hunk",
        label: "Previous Hunk",
        // Spec notation "N" for the bare-capital shorthand; see the module doc.
        bindings: &["Shift+N"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::ToggleTreeFlatFileList,
        id: "details.toggle-tree",
        label: "Toggle Tree/Flat List",
        bindings: &["t"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::ToggleUnifiedSplit,
        id: "details.toggle-split",
        label: "Toggle Unified/Split Diff",
        bindings: &["d"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::ToggleWhitespace,
        id: "details.toggle-whitespace",
        label: "Toggle Whitespace",
        bindings: &["w"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::StageFocusedFile,
        id: "details.stage-file",
        label: "Stage File",
        bindings: &["s"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::UnstageFocusedFile,
        id: "details.unstage-file",
        label: "Unstage File",
        bindings: &["u"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::StageAll,
        id: "details.stage-all",
        label: "Stage All",
        bindings: &["Mod+A"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::UnstageAll,
        id: "details.unstage-all",
        label: "Unstage All",
        bindings: &["Mod+Shift+A"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::StageSelectedLinesOrHunk,
        id: "details.stage-selection",
        label: "Stage Selection",
        bindings: &["Space"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::DiscardSelectedLinesOrHunk,
        id: "details.discard-selection",
        label: "Discard Selection",
        bindings: &["Backspace"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::Commit,
        id: "details.commit",
        label: "Commit",
        bindings: &["Mod+Enter"],
        context: Context::Details,
        menu: Some(Menu::Repository),
    },
    ActionDef {
        action: Action::ToggleAmend,
        id: "details.toggle-amend",
        label: "Toggle Amend",
        bindings: &["Mod+Shift+M"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::BlameFocusedFile,
        id: "details.blame",
        label: "Blame File",
        bindings: &["b"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::OpenFileInDefaultApp,
        id: "details.open-file",
        label: "Open File",
        bindings: &["o"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::DetailsBack,
        id: "details.back",
        label: "Back",
        bindings: &["Esc"],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::CommitAndPush,
        id: "details.commit-and-push",
        label: "Commit and Push",
        bindings: &[],
        context: Context::Details,
        menu: None,
    },
    ActionDef {
        action: Action::MoveDownRefs,
        id: "refs.move-down",
        label: "Move Down",
        bindings: &["↓", "j"],
        context: Context::Refs,
        menu: None,
    },
    ActionDef {
        action: Action::MoveUpRefs,
        id: "refs.move-up",
        label: "Move Up",
        bindings: &["↑", "k"],
        context: Context::Refs,
        menu: None,
    },
    ActionDef {
        action: Action::CollapseSection,
        id: "refs.collapse",
        label: "Collapse Section",
        bindings: &["←"],
        context: Context::Refs,
        menu: None,
    },
    ActionDef {
        action: Action::ExpandSection,
        id: "refs.expand",
        label: "Expand Section",
        bindings: &["→"],
        context: Context::Refs,
        menu: None,
    },
    ActionDef {
        action: Action::CheckoutOrOpen,
        id: "refs.checkout-or-open",
        label: "Checkout / Open",
        bindings: &["Enter"],
        context: Context::Refs,
        menu: None,
    },
    ActionDef {
        action: Action::RenameRef,
        id: "refs.rename",
        label: "Rename",
        bindings: &["F2"],
        context: Context::Refs,
        menu: None,
    },
    ActionDef {
        action: Action::DeleteOrClose,
        id: "refs.delete-or-close",
        label: "Delete",
        bindings: &["Backspace"],
        context: Context::Refs,
        menu: None,
    },
    ActionDef {
        action: Action::FilterRefs,
        id: "refs.filter",
        label: "Filter",
        bindings: &["/", "Mod+F"],
        context: Context::Refs,
        menu: None,
    },
    ActionDef {
        action: Action::RefsBack,
        id: "refs.back",
        label: "Back",
        bindings: &["Esc"],
        context: Context::Refs,
        menu: None,
    },
    ActionDef {
        action: Action::SetUpstream,
        id: "git.set-upstream",
        label: "Set Upstream…",
        bindings: &[],
        context: Context::Refs,
        menu: Some(Menu::Repository),
    },
    ActionDef {
        action: Action::About,
        id: "app.about",
        label: "About Amalgum",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::App),
    },
    ActionDef {
        action: Action::Quit,
        id: "app.quit",
        label: "Quit",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::App),
    },
    ActionDef {
        action: Action::Hibernate,
        id: "workspace.hibernate",
        label: "Hibernate",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::Resume,
        id: "workspace.resume",
        label: "Resume",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::Reconnect,
        id: "workspace.reconnect",
        label: "Reconnect",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::Workspace),
    },
    ActionDef {
        action: Action::MinimizeWindow,
        id: "window.minimize",
        label: "Minimize",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::Window),
    },
    ActionDef {
        action: Action::ZoomWindow,
        id: "window.zoom",
        label: "Zoom Window",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::Window),
    },
    ActionDef {
        action: Action::Cut,
        id: "edit.cut",
        label: "Cut",
        bindings: &["Mod+X"],
        context: Context::Global,
        menu: Some(Menu::Edit),
    },
    ActionDef {
        action: Action::Copy,
        id: "edit.copy",
        label: "Copy",
        bindings: &["Mod+C"],
        context: Context::Global,
        menu: Some(Menu::Edit),
    },
    ActionDef {
        action: Action::Paste,
        id: "edit.paste",
        label: "Paste",
        bindings: &["Mod+V"],
        context: Context::Global,
        menu: Some(Menu::Edit),
    },
    ActionDef {
        action: Action::SelectAll,
        id: "edit.select-all",
        label: "Select All",
        bindings: &["Mod+A"],
        context: Context::Global,
        menu: Some(Menu::Edit),
    },
    ActionDef {
        action: Action::Find,
        id: "edit.find",
        label: "Find",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::Edit),
    },
    ActionDef {
        action: Action::ShowHideRemotes,
        id: "view.show-hide-remotes",
        label: "Show/Hide Remote Branches",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::View),
    },
    ActionDef {
        action: Action::ShowHideTags,
        id: "view.show-hide-tags",
        label: "Show/Hide Tags",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::View),
    },
    ActionDef {
        action: Action::ShowHideStashes,
        id: "view.show-hide-stashes",
        label: "Show/Hide Stashes",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::View),
    },
    ActionDef {
        action: Action::ForcePushWithLease,
        id: "git.force-push-with-lease",
        label: "Force Push with Lease",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::Repository),
    },
    ActionDef {
        action: Action::SetUpAgentHooks,
        id: "help.set-up-agent-hooks",
        label: "Set Up Agent Hooks",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::Help),
    },
    ActionDef {
        action: Action::OpenLogFolder,
        id: "help.open-log-folder",
        label: "Open Log Folder",
        bindings: &[],
        context: Context::Global,
        menu: Some(Menu::Help),
    },
];

/// Every action, in palette/menu order.
pub fn table() -> &'static [ActionDef] {
    TABLE
}

pub fn def(action: Action) -> &'static ActionDef {
    match TABLE.iter().find(|d| d.action == action) {
        Some(d) => d,
        // Every `Action` has exactly one `TABLE` entry (tested); a miss is a bug in this file.
        None => unreachable!("Action {action:?} missing from TABLE"),
    }
}

/// Active bindings: defaults from [`table`] plus user overrides.
#[derive(Debug, Clone)]
pub struct Keymap {
    pub preset: Preset,
    overrides: BTreeMap<Action, Vec<Chord>>,
}

impl Keymap {
    pub fn new(preset: Preset) -> Self {
        Keymap { preset, overrides: BTreeMap::new() }
    }

    pub fn bindings(&self, action: Action) -> Vec<Chord> {
        if let Some(chords) = self.overrides.get(&action) {
            return chords.clone();
        }
        def(action).bindings.iter().filter_map(|s| Chord::parse(s).ok()).collect()
    }

    pub fn rebind(&mut self, action: Action, chords: Vec<Chord>) {
        self.overrides.insert(action, chords);
    }

    pub fn reset(&mut self) {
        self.overrides.clear();
    }

    /// The action for `chord` in `ctx`: the most specific context wins (Terminal-context
    /// `Mod+Shift+R` "Run…" beats global rebase); Graph/Details/Refs fall back to GitPane then
    /// Global; Terminal falls back to Global only for chords containing `Mod`.
    pub fn lookup(&self, ctx: Context, chord: &Chord) -> Option<Action> {
        let chain: &[Context] = match ctx {
            Context::Global => &[Context::Global],
            Context::GitPane => &[Context::GitPane, Context::Global],
            Context::Graph => &[Context::Graph, Context::GitPane, Context::Global],
            Context::Details => &[Context::Details, Context::GitPane, Context::Global],
            Context::Refs => &[Context::Refs, Context::GitPane, Context::Global],
            Context::Terminal => {
                if !chord.mods.primary {
                    return None;
                }
                &[Context::Terminal, Context::Global]
            }
        };
        for &level in chain {
            for def in TABLE.iter().filter(|d| d.context == level) {
                if self.bindings(def.action).contains(chord) {
                    return Some(def.action);
                }
            }
        }
        None
    }

    /// Pairs of actions sharing a chord in the same context (shown in `danger` in Settings).
    pub fn conflicts(&self) -> Vec<(Action, Action, Chord)> {
        let mut out = Vec::new();
        for (i, def_a) in TABLE.iter().enumerate() {
            for def_b in &TABLE[i + 1..] {
                if def_a.context != def_b.context {
                    continue;
                }
                for c1 in self.bindings(def_a.action) {
                    for c2 in self.bindings(def_b.action) {
                        if c1 == c2 {
                            out.push((def_a.action, def_b.action, c1));
                        }
                    }
                }
            }
        }
        out
    }

    /// Export/import overrides as `{"<action id>": ["Mod+K", …]}`.
    pub fn export_json(&self) -> String {
        let map: BTreeMap<&str, Vec<String>> = self
            .overrides
            .iter()
            .map(|(action, chords)| (def(*action).id, chords.iter().map(Chord::spec).collect()))
            .collect();
        serde_json::to_string(&map).unwrap_or_default()
    }

    pub fn import_json(&mut self, json: &str) -> Result<(), String> {
        let map: BTreeMap<String, Vec<String>> = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let mut new_overrides = BTreeMap::new();
        for (id, specs) in map {
            let action = TABLE
                .iter()
                .find(|d| d.id == id)
                .map(|d| d.action)
                .ok_or_else(|| format!("unknown action id: {id}"))?;
            let mut chords = Vec::with_capacity(specs.len());
            for s in specs {
                chords.push(Chord::parse(&s).map_err(|e| format!("invalid chord {s:?}: {e}"))?);
            }
            new_overrides.insert(action, chords);
        }
        self.overrides = new_overrides;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ---- Action / table bijection (§5.19, §7, §5.26) ----

    #[test]
    fn every_action_has_exactly_one_table_entry() {
        let all: HashSet<Action> = Action::ALL.iter().copied().collect();
        let table_actions: HashSet<Action> = TABLE.iter().map(|d| d.action).collect();
        assert_eq!(all.len(), Action::ALL.len(), "Action::ALL has a duplicate");
        assert_eq!(TABLE.len(), Action::ALL.len(), "TABLE and Action::ALL differ in length");
        assert_eq!(all, table_actions, "TABLE and Action::ALL cover different actions");
    }

    #[test]
    fn table_ids_are_unique_and_well_formed() {
        let re_ok = |id: &str| -> bool {
            let mut segs = id.split('.');
            let Some(first) = segs.next() else { return false };
            if first.is_empty() || !first.bytes().all(|b| b.is_ascii_lowercase()) {
                return false;
            }
            let mut has_rest = false;
            for seg in segs {
                has_rest = true;
                if seg.is_empty()
                    || !seg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                {
                    return false;
                }
            }
            has_rest
        };
        let mut seen = HashSet::new();
        for d in TABLE {
            assert!(re_ok(d.id), "id {:?} does not match ^[a-z]+(\\.[a-z0-9-]+)+$", d.id);
            assert!(seen.insert(d.id), "duplicate id {:?}", d.id);
        }
    }

    #[test]
    fn undo_redo_are_gitpane_not_global() {
        assert_eq!(def(Action::Undo).context, Context::GitPane);
        assert_eq!(def(Action::Redo).context, Context::GitPane);
    }

    #[test]
    fn pure_context_keys_have_no_menu() {
        for a in [Action::MoveSelectionDown, Action::CheckoutSelected, Action::MoveDownRefs] {
            assert_eq!(def(a).menu, None, "{a:?} should not appear in a menu");
        }
    }

    // ---- Chord parsing (§7 notations) ----

    #[test]
    fn parse_accepts_every_notation_used_in_section_7() {
        for s in [
            "Mod+Alt+←",
            "Shift+PageUp",
            "Mod+=",
            "Mod+\\",
            "Mod+,",
            "Mod+/",
            "?",
            "F2",
            "Esc",
            "Space",
            "Mod+Shift+P",
            "Mod+Alt+Enter",
            "Backspace",
            "Enter",
        ] {
            assert!(Chord::parse(s).is_ok(), "failed to parse {s:?}");
        }
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(Chord::parse("").is_err());
        assert!(Chord::parse("Mod+Bogus+K").is_err());
        assert!(Chord::parse("ab").is_err());
    }

    #[test]
    fn arrow_glyphs_and_names_parse_the_same() {
        assert_eq!(Chord::parse("↑").unwrap().key, Key::Up);
        assert_eq!(Chord::parse("Up").unwrap().key, Key::Up);
    }

    #[test]
    fn round_trip_every_default_binding() {
        for def in TABLE {
            for s in def.bindings {
                let chord = Chord::parse(s).unwrap_or_else(|e| panic!("{:?}: {s:?}: {e}", def.id));
                let round = Chord::parse(&chord.spec())
                    .unwrap_or_else(|e| panic!("{:?}: spec {:?}: {e}", def.id, chord.spec()));
                assert_eq!(chord, round, "{:?}: {s:?} -> {:?} -> mismatch", def.id, chord.spec());
            }
        }
    }

    // ---- Preset resolve / interpret / native / label ----

    fn all_mods() -> Vec<Mods> {
        let mut out = Vec::with_capacity(8);
        for primary in [false, true] {
            for shift in [false, true] {
                for alt in [false, true] {
                    out.push(Mods { primary, shift, alt });
                }
            }
        }
        out
    }

    #[test]
    fn resolve_interpret_round_trip_every_combo_every_preset() {
        for preset in [Preset::MacOs, Preset::LinuxCtrlShift, Preset::LinuxSuper] {
            for mods in all_mods() {
                if let Some(physical) = preset.resolve(mods) {
                    assert_eq!(
                        preset.interpret(physical),
                        Some(mods),
                        "{preset:?} resolve({mods:?}) -> {physical:?}, interpret didn't invert"
                    );
                }
            }
        }
    }

    #[test]
    fn linux_ctrl_shift_matches_documented_mapping() {
        let p = Preset::LinuxCtrlShift;
        assert_eq!(
            p.resolve(Mods { primary: true, shift: false, alt: false }),
            Some(Physical { ctrl: true, shift: true, ..Physical::default() })
        );
        assert_eq!(
            p.resolve(Mods { primary: true, shift: true, alt: false }),
            Some(Physical { ctrl: true, alt: true, ..Physical::default() })
        );
        assert_eq!(
            p.resolve(Mods { primary: true, shift: false, alt: true }),
            Some(Physical { ctrl: true, alt: true, shift: true, ..Physical::default() })
        );
        assert_eq!(p.resolve(Mods { primary: true, shift: true, alt: true }), None);
        assert_eq!(
            p.resolve(Mods { primary: false, shift: true, alt: true }),
            Some(Physical { shift: true, alt: true, ..Physical::default() })
        );
    }

    #[test]
    fn macos_and_linux_super_always_resolve() {
        for mods in all_mods() {
            assert!(Preset::MacOs.resolve(mods).is_some());
            assert!(Preset::LinuxSuper.resolve(mods).is_some());
        }
    }

    #[test]
    fn native_matches_platform_primary_modifier() {
        let expected = if crate::platform::primary_modifier_name() == "⌘" {
            Preset::MacOs
        } else {
            Preset::LinuxCtrlShift
        };
        assert_eq!(Preset::native(), expected);
    }

    #[test]
    fn mac_label_uses_apple_order_and_glyphs() {
        let chord = Chord::parse("Mod+K").unwrap();
        assert_eq!(Preset::MacOs.label(&chord), "⌘K");
        let chord = Chord::parse("Mod+Shift+K").unwrap();
        assert_eq!(Preset::MacOs.label(&chord), "⇧⌘K");
        assert_eq!(Preset::MacOs.label(&Chord::parse("Enter").unwrap()), "↩");
        assert_eq!(Preset::MacOs.label(&Chord::parse("Esc").unwrap()), "⎋");
        assert_eq!(Preset::MacOs.label(&Chord::parse("Backspace").unwrap()), "⌫");
        assert_eq!(Preset::MacOs.label(&Chord::parse("Mod+Alt+←").unwrap()), "⌥⌘←");
    }

    #[test]
    fn linux_labels_use_key_names() {
        let chord = Chord::parse("Mod+K").unwrap();
        assert_eq!(Preset::LinuxCtrlShift.label(&chord), "Ctrl+Shift+K");
        assert_eq!(Preset::LinuxSuper.label(&chord), "Super+K");
        assert_eq!(Preset::LinuxCtrlShift.label(&Chord::parse("Shift+PageUp").unwrap()), "Shift+PageUp");
        assert_eq!(Preset::LinuxCtrlShift.label(&Chord::parse("Mod+Alt+←").unwrap()), "Ctrl+Alt+Shift+Left");
    }

    #[test]
    fn label_falls_back_to_spec_when_unrepresentable() {
        let chord = Chord::parse("Mod+Shift+Alt+D").unwrap();
        assert_eq!(Preset::LinuxCtrlShift.resolve(chord.mods), None);
        assert_eq!(Preset::LinuxCtrlShift.label(&chord), chord.spec());
    }

    // ---- Keymap: bindings / rebind / reset ----

    #[test]
    fn bindings_default_then_override_then_reset() {
        let mut km = Keymap::new(Preset::MacOs);
        assert_eq!(km.bindings(Action::Push), vec![Chord::parse("Mod+P").unwrap()]);
        let custom = vec![Chord::parse("Mod+Shift+P").unwrap()];
        km.rebind(Action::Push, custom.clone());
        assert_eq!(km.bindings(Action::Push), custom);
        km.reset();
        assert_eq!(km.bindings(Action::Push), vec![Chord::parse("Mod+P").unwrap()]);
    }

    // ---- Keymap: default table has zero conflicts ----

    #[test]
    fn default_table_has_zero_conflicts() {
        let km = Keymap::new(Preset::MacOs);
        let conflicts = km.conflicts();
        assert!(conflicts.is_empty(), "default table has conflicts: {conflicts:?}");
    }

    #[test]
    fn rebind_can_introduce_a_detected_conflict() {
        let mut km = Keymap::new(Preset::MacOs);
        km.rebind(Action::Pull, vec![Chord::parse("Mod+P").unwrap()]);
        let conflicts = km.conflicts();
        assert!(conflicts.iter().any(|(a, b, c)| {
            let pair = (*a, *b) == (Action::Push, Action::Pull) || (*a, *b) == (Action::Pull, Action::Push);
            pair && *c == Chord::parse("Mod+P").unwrap()
        }));
    }

    #[test]
    fn cross_context_same_chord_is_not_a_conflict() {
        // Mod+Shift+R: global "Interactive Rebase" vs Terminal "Run…" (§7.1 note, §7.2).
        let km = Keymap::new(Preset::MacOs);
        let conflicts = km.conflicts();
        assert!(!conflicts.iter().any(|(a, b, _)| {
            matches!(
                (a, b),
                (&Action::InteractiveRebaseFromSelected, &Action::RunCommand)
                    | (&Action::RunCommand, &Action::InteractiveRebaseFromSelected)
            )
        }));
    }

    // ---- Keymap: lookup precedence ----

    #[test]
    fn lookup_finds_global_action() {
        let km = Keymap::new(Preset::MacOs);
        let chord = Chord::parse("Mod+P").unwrap();
        assert_eq!(km.lookup(Context::Global, &chord), Some(Action::Push));
    }

    #[test]
    fn lookup_graph_falls_back_to_global() {
        let km = Keymap::new(Preset::MacOs);
        // Mod+P has no Graph-context binding, so Graph should fall back to Global.
        let chord = Chord::parse("Mod+P").unwrap();
        assert_eq!(km.lookup(Context::Graph, &chord), Some(Action::Push));
        // "c" is Graph-specific and should win over nothing at GitPane/Global.
        let c = Chord::parse("c").unwrap();
        assert_eq!(km.lookup(Context::Graph, &c), Some(Action::CheckoutSelected));
    }

    #[test]
    fn lookup_gitpane_undo_beats_global_fallback_chain() {
        let km = Keymap::new(Preset::MacOs);
        let chord = Chord::parse("Mod+Z").unwrap();
        assert_eq!(km.lookup(Context::GitPane, &chord), Some(Action::Undo));
        assert_eq!(km.lookup(Context::Graph, &chord), Some(Action::Undo));
    }

    #[test]
    fn lookup_terminal_only_intercepts_mod_chords() {
        let km = Keymap::new(Preset::MacOs);
        // Bare "j" must never be intercepted in Terminal (reaches the PTY).
        let j = Chord::parse("j").unwrap();
        assert_eq!(km.lookup(Context::Terminal, &j), None);
        // Bare "?" likewise.
        let q = Chord::parse("?").unwrap();
        assert_eq!(km.lookup(Context::Terminal, &q), None);
        // Shift+PageUp has no Mod, so it is not looked up either (handled by the terminal
        // widget directly, per the §7 header rule).
        let page = Chord::parse("Shift+PageUp").unwrap();
        assert_eq!(km.lookup(Context::Terminal, &page), None);
        // Mod+C is Terminal-context and wins over the Global/Edit "Copy".
        let mod_c = Chord::parse("Mod+C").unwrap();
        assert_eq!(km.lookup(Context::Terminal, &mod_c), Some(Action::TerminalCopy));
    }

    #[test]
    fn lookup_terminal_run_overrides_global_rebase() {
        let km = Keymap::new(Preset::MacOs);
        let chord = Chord::parse("Mod+Shift+R").unwrap();
        assert_eq!(km.lookup(Context::Global, &chord), Some(Action::InteractiveRebaseFromSelected));
        assert_eq!(km.lookup(Context::Terminal, &chord), Some(Action::RunCommand));
    }

    #[test]
    fn lookup_mod_up_down_differ_by_context() {
        let km = Keymap::new(Preset::MacOs);
        let up = Chord::parse("Mod+↑").unwrap();
        assert_eq!(km.lookup(Context::Graph, &up), Some(Action::FirstCommit));
        assert_eq!(km.lookup(Context::Terminal, &up), Some(Action::PreviousPrompt));
    }

    // ---- Export / import ----

    #[test]
    fn export_import_round_trips_overrides_only() {
        let mut km = Keymap::new(Preset::MacOs);
        km.rebind(Action::Push, vec![Chord::parse("Mod+Shift+P").unwrap()]);
        let json = km.export_json();
        assert!(json.contains("git.push"));
        // Unmodified actions are not exported.
        assert!(!json.contains("\"git.pull\""));

        let mut km2 = Keymap::new(Preset::LinuxSuper);
        km2.import_json(&json).unwrap();
        assert_eq!(km2.bindings(Action::Push), vec![Chord::parse("Mod+Shift+P").unwrap()]);
        // Preset is untouched by import.
        assert_eq!(km2.preset, Preset::LinuxSuper);
    }

    #[test]
    fn import_rejects_unknown_id_and_bad_chord() {
        let mut km = Keymap::new(Preset::MacOs);
        assert!(km.import_json(r#"{"not.a.real.action":["Mod+K"]}"#).is_err());
        assert!(km.import_json(r#"{"git.push":["Mod+Bogus"]}"#).is_err());
    }
}
