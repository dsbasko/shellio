//! Раннер пробы P0.5: процессы сценариев, `--launch` и `--aggregate`.

use std::process::ExitCode;

fn main() -> ExitCode {
    probe_update_lifecycle::main()
}
