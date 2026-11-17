//! Rust-хост пробы P0.4: вся логика в `probe_session_survival::host`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(probe_session_survival::host::main(&args));
}
