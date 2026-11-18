//! Attach-клиент пробы: см. [`probe_update_lifecycle::attach`].

use std::process::ExitCode;

fn main() -> ExitCode {
    match probe_update_lifecycle::helper_args(probe_update_lifecycle::attach::ATTACH_NAME) {
        Ok(args) => probe_update_lifecycle::attach::main(&args),
        Err(code) => code,
    }
}
