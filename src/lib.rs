//! far_linux: a Far Manager 3 style two-panel file manager for Linux terminals.

pub mod app;
pub mod archive;
pub mod dialog;
pub mod editor;
pub mod encoding;
pub mod fsops;
pub mod history;
pub mod input;
pub mod lineedit;
pub mod panel;
pub mod screen;
pub mod search;
pub mod shell;
pub mod util;
pub mod viewer;

/// Starts the application in the directories given on the command line.
pub fn run() -> std::io::Result<()> {
    // Folder names need not be UTF-8, so these stay as they came.
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Usage: far [left_dir [right_dir]]\n       far --keys    show the keys the terminal sends");
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("far_linux {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--keys") {
        return app::show_keys();
    }
    app::run(args.first().cloned(), args.get(1).cloned())
}
