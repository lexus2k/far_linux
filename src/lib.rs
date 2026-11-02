//! far_linux: a Far Manager 3 style two-panel file manager for Linux terminals.

pub mod app;
pub mod dialog;
pub mod editor;
pub mod encoding;
pub mod fsops;
pub mod lineedit;
pub mod panel;
pub mod screen;
pub mod shell;
pub mod util;
pub mod viewer;

/// Starts the application in the directories given on the command line.
pub fn run() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Usage: far [left_dir [right_dir]]");
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("far_linux {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    app::run(args.first().cloned(), args.get(1).cloned())
}
