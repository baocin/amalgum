use amalgum::ctl::cli::{self, Cli};
use clap::Parser;

fn main() {
    let cli = Cli::parse();
    if let Some(code) = cli::run_rebase_helper(&cli) {
        std::process::exit(code);
    }
    let code = match cli.command {
        Some(cmd) => cli::run(cmd, &cli::Env::from_process(), &mut std::io::stdin(), &mut std::io::stdout()),
        None => launch(cli.path),
    };
    std::process::exit(code);
}

/// No subcommand: open `path` (default `.`) in the running app, or start the app.
#[cfg(feature = "gui")]
fn launch(path: Option<String>) -> i32 {
    amalgum::ui::launch(path)
}

#[cfg(not(feature = "gui"))]
fn launch(_path: Option<String>) -> i32 {
    eprintln!("this is the headless amalgum CLI; run `amalgum --help` for commands");
    2
}
