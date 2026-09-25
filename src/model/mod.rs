//! UI-agnostic app model. Nothing here imports egui, so it is unit-testable headless.
//!
//! - [`settings`] `settings.toml` with every default from §5.23
//! - [`state`]    repos › remotes › workspaces › tabs, persisted to `state.json` (§5.22)
//! - [`layout`]   terminal split trees and pane focus movement (§5.27)
//! - [`keymap`]   the action table: single source of truth for palette, menus, shortcuts (§5.19, §7)
//! - [`theme`]    semantic color tokens, ANSI and lane palettes, WCAG contrast (§4)
//! - [`fuzzy`]    palette fuzzy matching and group prefixes (§5.19)

pub mod fuzzy;
pub mod keymap;
pub mod layout;
pub mod settings;
pub mod state;
pub mod theme;
