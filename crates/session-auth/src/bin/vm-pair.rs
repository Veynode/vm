fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Err(error) = vm_session_auth::cli::run_cli(&args) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
