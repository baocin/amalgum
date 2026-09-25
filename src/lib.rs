//! Amalgum: a workspace shell for running coding agents in parallel, with git built in.
//! The spec is `docs/SPEC.md`; section numbers in doc comments (`§5.28`) point into it.
//!
//! Module map. Everything except `ui` is portable, headless, and compiled into the CLI build.
//! - [`paths`]    where every file lives; never absolute paths (§1)
//! - [`util`]     time formatting, stable hashing
//! - [`ctl`]      control socket protocol, client/server, CLI, remote offline queue (§5.31)
//! - [`git`]      system-git runner (local + ssh), porcelain parsers, graph lanes, journal (§5.4–5.21)
//! - [`agent`]    agent status model, hook adapters/installers, OSC scanner, hibernation (§5.27–5.30)
//! - [`model`]    UI-agnostic app model: settings, state, split layout, keymap, theme, fuzzy (§3–4, §7)
//! - [`ssh`]      ssh/tmux argv builders, remote quoting, reconnect backoff (§5.28)
//! - [`ports`]    parse `lsof`/`ss` listening-port output (§5.27)
//! - [`platform`] the only module that may call OS-specific APIs (§1)
//! - `ui`         the eframe app (feature `gui`)

// SKELETON: contracts are `todo!()` stubs until implemented; remove this line once none remain.
#![allow(unused_variables, dead_code, clippy::todo)]

pub mod agent;
pub mod ctl;
pub mod git;
pub mod model;
pub mod paths;
pub mod platform;
pub mod ports;
pub mod ssh;
pub mod util;

#[cfg(feature = "gui")]
pub mod ui;

#[cfg(test)]
pub(crate) mod testutil;
