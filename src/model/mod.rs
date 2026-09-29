//! UI-agnostic app model. Nothing here imports egui, so it is unit-testable headless.
//!
//! - [`clone`]    the clone sheet's form: validation, derived folder name (§5.3)
//! - [`confirm`]  destructive confirmations: kinds, content, don't-ask-again (§5.20)
//! - [`settings`] `settings.toml` with every default from §5.23
//! - [`state`]    repos › remotes › workspaces › tabs, persisted to `state.json` (§5.22)
//! - [`layout`]   terminal split trees and pane focus movement (§5.27)
//! - [`keymap`]   the action table: single source of truth for palette, menus, shortcuts (§5.19, §7)
//! - [`theme`]    semantic color tokens, ANSI and lane palettes, WCAG contrast (§4)
//! - [`fuzzy`]    palette fuzzy matching and group prefixes (§5.19)
//! - [`sync`]     fetch / pull / push decisions and labels for the git pane toolbar (§5.10)
//! - [`git_menu`] commit context menu, ref-name validation, git-op confirmations (§5.4, §5.9–5.21)
//! - [`search`]   commit search field, history, and scheduling (§5.8)

pub mod clone;
pub mod confirm;
pub mod fuzzy;
pub mod git_menu;
pub mod keymap;
pub mod layout;
pub mod search;
pub mod settings;
pub mod state;
pub mod sync;
pub mod theme;
