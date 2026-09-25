//! The eframe app (feature `gui`). Layout per §3 / W2: menu bar (top), status bar (bottom),
//! sidebar (left), git pane (right), terminal tabs and splits (center), welcome when empty.
//!
//! Threading (§2 "Never block"): the UI thread only draws and dispatches. Each terminal owns a
//! PTY thread (`terminal`), git runs on worker threads (`jobs`), the control socket has its own
//! thread (`control`); all hand results back over channels plus `Context::request_repaint`.

mod app;
mod chrome;
mod control;
mod fonts;
mod git_pane;
mod jobs;
mod palette;
mod screenshot;
mod shortcuts;
mod sidebar;
mod terminal;
mod theme;
mod welcome;

use crate::ctl::protocol::{Command, Request};
use crate::ctl::socket;
use crate::git::Location;
use crate::paths::Dirs;
use std::path::Path;
use std::time::Duration;

/// `amalgum [path]`: forward to a running app (one instance per user, §5.31) or start one.
pub fn launch(path: Option<String>) -> i32 {
    let Some(dirs) = Dirs::discover() else {
        eprintln!("amalgum: cannot determine the config/data directories (is $HOME set?)");
        return 1;
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let target = open_target(path.as_deref(), &cwd);

    if let Some(loc) = &target {
        let req =
            Request::new(Command::Open { location: location_arg(loc), remote: None, name: None, run: None });
        if let Ok(resp) = socket::send(&dirs.socket(), &req, Duration::from_millis(500)) {
            if resp.ok {
                return 0;
            }
            eprintln!("amalgum: {}", resp.error.unwrap_or_default());
            return 1;
        }
    }
    run(dirs, target)
}

/// What to open at launch: an explicit path (resolved against the cwd), or the cwd when it is
/// inside a git work tree. Launched from Finder or a desktop menu (cwd `/`), nothing.
fn open_target(path: Option<&str>, cwd: &Path) -> Option<Location> {
    match path {
        Some(p) => Some(match Location::parse(p) {
            Location::Local { path } => Location::Local { path: cwd.join(path) },
            remote => remote,
        }),
        None => cwd
            .ancestors()
            .any(|d| d.join(".git").exists())
            .then(|| Location::Local { path: cwd.to_path_buf() }),
    }
}

fn location_arg(loc: &Location) -> String {
    match loc {
        Location::Local { path } => path.display().to_string(),
        Location::Remote { host, path } => format!("{host}:{path}"),
    }
}

fn run(dirs: Dirs, initial: Option<Location>) -> i32 {
    let options = |renderer| eframe::NativeOptions {
        renderer,
        viewport: egui::ViewportBuilder::default()
            .with_title("Amalgum")
            .with_app_id("amalgum")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 600.0])
            .with_fullsize_content_view(crate::platform::window_decorations().fullsize_content),
        ..Default::default()
    };
    let start = |renderer| {
        let (dirs, initial) = (dirs.clone(), initial.clone());
        eframe::run_native(
            "Amalgum",
            options(renderer),
            Box::new(move |cc| Ok(Box::new(app::App::new(cc, dirs, initial)) as Box<dyn eframe::App>)),
        )
    };
    // wgpu first; OpenGL when no usable GPU adapter exists (VMs, old drivers, remote X) — §1.
    let result = start(eframe::Renderer::Wgpu).or_else(|e| {
        eprintln!("amalgum: wgpu renderer unavailable ({e}); falling back to OpenGL");
        start(eframe::Renderer::Glow)
    });
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("amalgum: could not open a window: {e}");
            1
        }
    }
}
