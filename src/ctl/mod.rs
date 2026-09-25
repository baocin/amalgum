//! Control plane (§5.31): the JSON-lines [`protocol`], the unix-socket [`socket`] server and
//! client, the [`cli`], and the remote offline [`queue`]. Everything here builds without the
//! `gui` feature because it *is* the headless remote CLI — never import `ui` or egui here.

pub mod cli;
pub mod protocol;
pub mod queue;
pub mod socket;
