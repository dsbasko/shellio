//! Минимальный хост пробы: см. [`probe_update_lifecycle::host`].

use std::process::ExitCode;

fn main() -> ExitCode {
    match probe_update_lifecycle::helper_args(probe_update_lifecycle::host::HOST_NAME) {
        Ok(args) => {
            ExitCode::from(u8::try_from(probe_update_lifecycle::host::main(&args)).unwrap_or(1))
        }
        Err(code) => code,
    }
}
