//! `settings.toml` (§5.23): hand-editable, every field optional on disk (missing → default),
//! unknown keys ignored, sections as TOML tables. Per-repo overrides live in
//! `repos/<repo-id>.toml` and override only the fields they set.
//!
//! Defaults come from the spec text: fetch every 5 min, restore workspaces on, avatars on,
//! theme System, UI scale 100 %, row height 24, git pane right, scrollback 10 000 (max 100 000),
//! protected branches `main master develop release/*`, cherry-pick `-x` on, fast-forward `ff`,
//! idle threshold 30 min, hibernation off, auto-resume on focus on, notifications 500,
//! journal 200, ControlPersist 10m, keepalive 20 s × 2, reconnect backoff cap 60 s, remote
//! refresh 10 s, install hooks on remote hosts on, tmux on for new hosts.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FastForward {
    #[default]
    Ff,
    NoFf,
    FfOnly,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanePosition {
    #[default]
    Right,
    Bottom,
}

/// Sections mirror Settings window pages. Each struct is `#[serde(default)]` and implements
/// `Default` with the spec defaults. Add fields as features land; never rename a key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub general: General,
    pub appearance: Appearance,
    pub terminal: Terminal,
    pub git: Git,
    pub ssh: Ssh,
    pub agents: Agents,
    pub advanced: Advanced,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub default_clone_dir: Option<String>,
    pub restore_workspaces: bool,
    /// Minutes; `0` = off. Allowed: 0, 1, 2, 5, 10, 30.
    pub fetch_interval_min: u32,
    pub skip_fetch_on_low_battery: bool,
    pub avatars: bool,
    pub absolute_timestamps: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: ThemeMode,
    pub use_system_accent: bool,
    /// Percent, 80–200 in steps of 10.
    pub ui_scale: u32,
    pub row_height: u32,
    pub shape_coded_lanes: bool,
    pub reduce_motion: Option<bool>,
    pub git_pane: PanePosition,
    pub show_remote_branches: bool,
    pub show_tags: bool,
    pub show_stashes: bool,
    pub auto_collapse_merged: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Terminal {
    pub font_size: f32,
    pub scrollback: u32,
    /// `None` = `$SHELL`.
    pub shell: Option<String>,
    pub copy_on_select: bool,
    pub theme_light: Option<String>,
    pub theme_dark: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Git {
    pub git_path: Option<String>,
    pub fast_forward: FastForward,
    pub cherry_pick_x: bool,
    pub allow_amend_pushed: bool,
    pub protected_branches: Vec<String>,
    pub sign_off: bool,
    pub push_new_branches_without_asking: bool,
    pub remote_refresh_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ssh {
    pub ssh_path: Option<String>,
    pub keepalive_interval: u32,
    pub keepalive_count: u32,
    pub control_persist: String,
    pub tmux_default: bool,
    pub reconnect_cap_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Agents {
    pub notification_sound: bool,
    pub os_notifications: bool,
    pub hibernation: bool,
    pub idle_threshold_min: u32,
    /// `None` = no live-terminal limit.
    pub live_limit: Option<u32>,
    pub auto_resume_on_focus: bool,
    pub install_hooks_on_remote: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Advanced {
    pub journal_size: usize,
    pub notification_history: usize,
    pub log_level: String,
}

impl Settings {
    /// Missing file → defaults. A parse error is returned with the TOML error text (the app
    /// shows it and keeps running on defaults; the file is never overwritten on error).
    pub fn load(path: &Path) -> Result<Self, String> {
        todo!()
    }
    pub fn save(&self, path: &Path) -> io::Result<()> {
        todo!()
    }
    /// Does a branch match `protected_branches` (exact or trailing `/*` glob)?
    pub fn is_protected(&self, branch: &str) -> bool {
        todo!()
    }
}

/// Per-repo overrides (§5.23 last bullet): only set fields apply.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoOverrides {
    pub fast_forward: Option<FastForward>,
    pub sign_off: Option<bool>,
}

impl RepoOverrides {
    pub fn load(path: &Path) -> Result<Self, String> {
        todo!()
    }
    pub fn apply(&self, s: &Settings) -> Settings {
        todo!()
    }
}

// Spec defaults (§5.23). SKELETON: filled in by the settings implementation.
macro_rules! todo_default {
    ($($t:ty),*) => {$(impl Default for $t { fn default() -> Self { todo!() } })*};
}
todo_default!(General, Appearance, Terminal, Git, Ssh, Agents, Advanced);
