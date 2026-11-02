fn main() {
    if let Err(e) = far_linux::run() {
        eprintln!("far: {e}");
        std::process::exit(1);
    }
}
