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
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.to_string()),
        };
        let settings: Settings = toml::from_str(&text).map_err(|e| e.to_string())?;
        Ok(settings.normalized())
    }

    /// Writes `settings.toml`, creating parent directories and writing atomically (temp file in
    /// the same directory, then rename) so a crash or concurrent read never sees a partial file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => {
                std::fs::create_dir_all(dir)?;
                dir
            }
            _ => Path::new("."),
        };
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let tmp_path = dir.join(format!(".settings-{}-{nanos}.tmp", std::process::id()));
        std::fs::write(&tmp_path, text)?;
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }

    /// Clamps or snaps out-of-range values to the nearest valid setting; applied by `load` so a
    /// hand-edited file with e.g. `ui_scale = 500` never reaches the rest of the app unclamped.
    pub fn normalized(mut self) -> Self {
        self.appearance.ui_scale = round_to_step(self.appearance.ui_scale.clamp(80, 200), 10);
        self.appearance.row_height = self.appearance.row_height.clamp(20, 32);
        self.general.fetch_interval_min =
            nearest_allowed(self.general.fetch_interval_min, &[0, 1, 2, 5, 10, 30]);
        self.terminal.scrollback = self.terminal.scrollback.min(100_000);
        self
    }

    /// Does a branch match `protected_branches` (exact or trailing `/*` glob)?
    ///
    /// A pattern ending in `/*` matches any branch strictly under that prefix (`release/*`
    /// matches `release/1.2` and `release/a/b`, but not `release` itself). Any other pattern
    /// must match the branch name exactly.
    pub fn is_protected(&self, branch: &str) -> bool {
        self.git.protected_branches.iter().any(|pattern| match pattern.strip_suffix("/*") {
            Some(prefix) => branch
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(|rest| !rest.is_empty()),
            None => branch == pattern,
        })
    }
}

/// Rounds `value` to the nearest multiple of `step` (ties round up).
fn round_to_step(value: u32, step: u32) -> u32 {
    ((value + step / 2) / step) * step
}

/// The `allowed` value closest to `value` (ties keep the first, i.e. smallest, candidate).
fn nearest_allowed(value: u32, allowed: &[u32]) -> u32 {
    *allowed.iter().min_by_key(|&&a| (a as i64 - value as i64).abs()).unwrap_or(&value)
}

/// Per-repo overrides (§5.23 last bullet): only set fields apply.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoOverrides {
    pub fast_forward: Option<FastForward>,
    pub sign_off: Option<bool>,
}

impl RepoOverrides {
    /// Missing file → empty overrides (`Self::default()`), same convention as `Settings::load`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.to_string()),
        };
        toml::from_str(&text).map_err(|e| e.to_string())
    }

    /// Applies only the fields this override sets, leaving everything else in `s` untouched.
    pub fn apply(&self, s: &Settings) -> Settings {
        let mut out = s.clone();
        if let Some(fast_forward) = self.fast_forward {
            out.git.fast_forward = fast_forward;
        }
        if let Some(sign_off) = self.sign_off {
            out.git.sign_off = sign_off;
        }
        out
    }
}

impl Default for General {
    fn default() -> Self {
        General {
            default_clone_dir: None,
            restore_workspaces: true,
            fetch_interval_min: 5,
            skip_fetch_on_low_battery: true,
            avatars: true,
            absolute_timestamps: false,
        }
    }
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance {
            theme: ThemeMode::System,
            // §4: "on by default on macOS, off on Linux where no standard exists". This module
            // may not branch on the OS (only `src/platform/` may), so the persisted default is
            // `true` everywhere; the UI layer is expected to treat the OS accent as unavailable
            // (i.e. behave as if this were off) on platforms with no standard accent color.
            use_system_accent: true,
            ui_scale: 100,
            row_height: 24,
            // No explicit default in the spec text; left off until a human picks one — it is an
            // accessibility aid layered on top of lane colors that already pass WCAG on their own.
            shape_coded_lanes: false,
            reduce_motion: None,
            git_pane: PanePosition::Right,
            show_remote_branches: true,
            show_tags: true,
            show_stashes: true,
            auto_collapse_merged: false,
        }
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Terminal {
            // §4 Typography: "Terminal, code, and hashes: JetBrains Mono 12 px."
            font_size: 12.0,
            scrollback: 10_000,
            shell: None,
            // No explicit default in the spec text; off, matching most terminal emulators.
            copy_on_select: false,
            theme_light: None,
            theme_dark: None,
        }
    }
}

impl Default for Git {
    fn default() -> Self {
        Git {
            git_path: None,
            fast_forward: FastForward::Ff,
            cherry_pick_x: true,
            allow_amend_pushed: false,
            protected_branches: vec![
                "main".to_string(),
                "master".to_string(),
                "develop".to_string(),
                "release/*".to_string(),
            ],
            // No explicit default in the spec text; off, matching "Allow amending pushed" above.
            sign_off: false,
            push_new_branches_without_asking: false,
            remote_refresh_secs: 10,
        }
    }
}

impl Default for Ssh {
    fn default() -> Self {
        Ssh {
            ssh_path: None,
            keepalive_interval: 20,
            keepalive_count: 2,
            control_persist: "10m".to_string(),
            tmux_default: true,
            reconnect_cap_secs: 60,
        }
    }
}

impl Default for Agents {
    fn default() -> Self {
        Agents {
            // No explicit default in the spec text; on, so a needs-input agent is noticed the
            // same way an OS notification would be (§5.29's "unless muted" framing assumes on).
            notification_sound: true,
            os_notifications: true,
            hibernation: false,
            idle_threshold_min: 30,
            live_limit: None,
            auto_resume_on_focus: true,
            install_hooks_on_remote: true,
        }
    }
}

impl Default for Advanced {
    fn default() -> Self {
        Advanced { journal_size: 200, notification_history: 500, log_level: "info".to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn settings_path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join("settings.toml")
    }

    // -- spec defaults (§5.23 and the sections it points at) ----------------------------------

    #[test]
    fn defaults_match_spec() {
        let s = Settings::default();
        assert!(s.general.restore_workspaces);
        assert_eq!(s.general.fetch_interval_min, 5);
        assert!(s.general.skip_fetch_on_low_battery);
        assert!(s.general.avatars);

        assert_eq!(s.appearance.theme, ThemeMode::System);
        assert_eq!(s.appearance.ui_scale, 100);
        assert_eq!(s.appearance.row_height, 24);
        assert_eq!(s.appearance.git_pane, PanePosition::Right);
        assert!(s.appearance.show_remote_branches);
        assert!(s.appearance.show_tags);
        assert!(s.appearance.show_stashes);
        assert!(!s.appearance.auto_collapse_merged);

        assert_eq!(s.terminal.font_size, 12.0);
        assert_eq!(s.terminal.scrollback, 10_000);

        assert_eq!(s.git.protected_branches, vec!["main", "master", "develop", "release/*"]);
        assert!(s.git.cherry_pick_x);
        assert_eq!(s.git.fast_forward, FastForward::Ff);
        assert!(!s.git.allow_amend_pushed);
        assert_eq!(s.git.remote_refresh_secs, 10);

        assert_eq!(s.ssh.keepalive_interval, 20);
        assert_eq!(s.ssh.keepalive_count, 2);
        assert_eq!(s.ssh.control_persist, "10m");
        assert!(s.ssh.tmux_default);
        assert_eq!(s.ssh.reconnect_cap_secs, 60);

        assert_eq!(s.agents.idle_threshold_min, 30);
        assert!(!s.agents.hibernation);
        assert!(s.agents.auto_resume_on_focus);
        assert!(s.agents.install_hooks_on_remote);

        assert_eq!(s.advanced.journal_size, 200);
        assert_eq!(s.advanced.notification_history, 500);
        assert_eq!(s.advanced.log_level, "info");
    }

    // -- load ---------------------------------------------------------------------------------

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loaded = Settings::load(&settings_path(dir.path())).expect("load");
        assert_eq!(loaded, Settings::default());
    }

    #[test]
    fn load_parse_error_is_returned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = settings_path(dir.path());
        fs::write(&path, "not = valid = toml").expect("write");
        assert!(Settings::load(&path).is_err());
    }

    #[test]
    fn load_ignores_unknown_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = settings_path(dir.path());
        fs::write(
            &path,
            "[general]\nfetch_interval_min = 10\nsome_future_field = 42\n\n[totally_unknown]\nx = 1\n",
        )
        .expect("write");
        let loaded = Settings::load(&path).expect("load");
        assert_eq!(loaded.general.fetch_interval_min, 10);
        assert_eq!(loaded.general.avatars, Settings::default().general.avatars);
    }

    #[test]
    fn load_partial_file_keeps_other_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = settings_path(dir.path());
        fs::write(&path, "[git]\nsign_off = true\n").expect("write");
        let loaded = Settings::load(&path).expect("load");
        assert!(loaded.git.sign_off);
        assert_eq!(loaded.git.cherry_pick_x, Settings::default().git.cherry_pick_x);
        assert_eq!(loaded.terminal, Settings::default().terminal);
        assert_eq!(loaded.appearance, Settings::default().appearance);
        assert_eq!(loaded.general, Settings::default().general);
    }

    #[test]
    fn load_normalizes_out_of_range_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = settings_path(dir.path());
        fs::write(&path, "[appearance]\nui_scale = 500\n\n[general]\nfetch_interval_min = 7\n")
            .expect("write");
        let loaded = Settings::load(&path).expect("load");
        assert_eq!(loaded.appearance.ui_scale, 200);
        assert_eq!(loaded.general.fetch_interval_min, 5);
    }

    // -- save / round-trip ----------------------------------------------------------------------

    #[test]
    fn save_creates_parent_dirs_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("settings.toml");
        let mut s = Settings::default();
        s.git.sign_off = true;
        s.appearance.ui_scale = 150;
        s.general.fetch_interval_min = 10;
        s.git.protected_branches = vec!["trunk".to_string()];

        s.save(&path).expect("save");
        assert!(path.exists());
        let loaded = Settings::load(&path).expect("load");
        assert_eq!(loaded, s);
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = settings_path(dir.path());
        Settings::default().save(&path).expect("save");
        let names: Vec<_> = fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["settings.toml".to_string()]);
    }

    // -- normalized -----------------------------------------------------------------------------

    #[test]
    fn normalized_clamps_ui_scale_to_multiple_of_ten() {
        let mut s = Settings::default();
        s.appearance.ui_scale = 500;
        assert_eq!(s.clone().normalized().appearance.ui_scale, 200);
        s.appearance.ui_scale = 5;
        assert_eq!(s.clone().normalized().appearance.ui_scale, 80);
        s.appearance.ui_scale = 95;
        assert_eq!(s.normalized().appearance.ui_scale, 100);
    }

    #[test]
    fn normalized_clamps_row_height() {
        let mut s = Settings::default();
        s.appearance.row_height = 5;
        assert_eq!(s.clone().normalized().appearance.row_height, 20);
        s.appearance.row_height = 999;
        assert_eq!(s.normalized().appearance.row_height, 32);
    }

    #[test]
    fn normalized_snaps_fetch_interval_to_nearest_allowed() {
        let mut s = Settings::default();
        s.general.fetch_interval_min = 7;
        assert_eq!(s.clone().normalized().general.fetch_interval_min, 5);
        s.general.fetch_interval_min = 3;
        assert_eq!(s.clone().normalized().general.fetch_interval_min, 2);
        s.general.fetch_interval_min = 4;
        assert_eq!(s.normalized().general.fetch_interval_min, 5);
    }

    #[test]
    fn normalized_caps_scrollback() {
        let mut s = Settings::default();
        s.terminal.scrollback = 500_000;
        assert_eq!(s.clone().normalized().terminal.scrollback, 100_000);
        s.terminal.scrollback = 1_000;
        assert_eq!(s.normalized().terminal.scrollback, 1_000);
    }

    // -- is_protected -----------------------------------------------------------------------------

    #[test]
    fn is_protected_exact_match() {
        let s = Settings::default();
        assert!(s.is_protected("main"));
        assert!(s.is_protected("master"));
        assert!(s.is_protected("develop"));
        assert!(!s.is_protected("feature/x"));
        assert!(!s.is_protected("mainline")); // no accidental prefix match
    }

    #[test]
    fn is_protected_trailing_glob() {
        let s = Settings::default();
        assert!(s.is_protected("release/1.2"));
        assert!(s.is_protected("release/a/b"));
        assert!(!s.is_protected("release"));
        assert!(!s.is_protected("release-1.2")); // not under the `/` prefix
        assert!(!s.is_protected("prerelease/1.2")); // prefix must match from the start
    }

    // -- RepoOverrides ----------------------------------------------------------------------------

    #[test]
    fn repo_overrides_load_missing_file_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let overrides = RepoOverrides::load(&dir.path().join("repo.toml")).expect("load");
        assert_eq!(overrides, RepoOverrides::default());
    }

    #[test]
    fn repo_overrides_load_partial_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("repo.toml");
        fs::write(&path, "sign_off = true\n").expect("write");
        let overrides = RepoOverrides::load(&path).expect("load");
        assert_eq!(overrides.sign_off, Some(true));
        assert_eq!(overrides.fast_forward, None);
    }

    #[test]
    fn repo_overrides_apply_only_set_fields() {
        let base = Settings::default();
        let overrides = RepoOverrides { fast_forward: Some(FastForward::NoFf), sign_off: None };
        let applied = overrides.apply(&base);

        let mut expected = base.clone();
        expected.git.fast_forward = FastForward::NoFf;
        assert_eq!(applied, expected, "only fast_forward should change");
        assert_eq!(applied.git.sign_off, base.git.sign_off);
    }

    #[test]
    fn repo_overrides_empty_changes_nothing() {
        let base = Settings::default();
        let applied = RepoOverrides::default().apply(&base);
        assert_eq!(applied, base);
    }
}
