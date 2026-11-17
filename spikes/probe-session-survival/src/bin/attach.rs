//! Attach-клиент Rust-хоста пробы P0.4: вся логика в `probe_session_survival::host::attach`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(probe_session_survival::host::attach::main(&args));
}
