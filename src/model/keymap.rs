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
        todo!()
    }
    /// Back to spec notation (canonical order `Mod+Shift+Alt+Key`).
    pub fn spec(&self) -> String {
        todo!()
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
        todo!()
    }
    /// Physical modifiers for a chord's modifiers; `None` if the preset cannot express it
    /// (e.g. `Mod+Shift+Alt` under Linux Ctrl+Shift).
    pub fn resolve(self, mods: Mods) -> Option<Physical> {
        todo!()
    }
    /// Inverse of [`Preset::resolve`], for matching key events.
    pub fn interpret(self, physical: Physical) -> Option<Mods> {
        todo!()
    }
    /// Tooltip/menu label: `⌘⇧K` on macOS, `Ctrl+Shift+K` / `Super+K` on Linux.
    pub fn label(self, chord: &Chord) -> String {
        todo!()
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

/// One variant per palette action. Extend from §7 and §5.26; keep ids stable once shipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    CommandPalette,
    ShortcutOverlay,
    OpenFolder,
    CloneRepository,
    ConnectToHost,
    NewWorkspace,
    CloseWorkspace,
    Push,
    // … every other row of §7.1–7.6 and §5.26
}

pub struct ActionDef {
    pub action: Action,
    pub id: &'static str,
    pub label: &'static str,
    pub bindings: &'static [&'static str],
    pub context: Context,
    pub menu: Option<Menu>,
}

/// Every action, in palette/menu order.
pub fn table() -> &'static [ActionDef] {
    todo!()
}

pub fn def(action: Action) -> &'static ActionDef {
    todo!()
}

/// Active bindings: defaults from [`table`] plus user overrides.
#[derive(Debug, Clone)]
pub struct Keymap {
    pub preset: Preset,
    overrides: BTreeMap<Action, Vec<Chord>>,
}

impl Keymap {
    pub fn new(preset: Preset) -> Self {
        todo!()
    }
    pub fn bindings(&self, action: Action) -> Vec<Chord> {
        todo!()
    }
    pub fn rebind(&mut self, action: Action, chords: Vec<Chord>) {
        todo!()
    }
    pub fn reset(&mut self) {
        todo!()
    }
    /// The action for `chord` in `ctx`: the most specific context wins (Terminal-context
    /// `Mod+Shift+R` "Run…" beats global rebase); Graph/Details/Refs fall back to GitPane then
    /// Global; Terminal falls back to Global only for chords containing `Mod`.
    pub fn lookup(&self, ctx: Context, chord: &Chord) -> Option<Action> {
        todo!()
    }
    /// Pairs of actions sharing a chord in the same context (shown in `danger` in Settings).
    pub fn conflicts(&self) -> Vec<(Action, Action, Chord)> {
        todo!()
    }
    /// Export/import overrides as `{"<action id>": ["Mod+K", …]}`.
    pub fn export_json(&self) -> String {
        todo!()
    }
    pub fn import_json(&mut self, json: &str) -> Result<(), String> {
        todo!()
    }
}
