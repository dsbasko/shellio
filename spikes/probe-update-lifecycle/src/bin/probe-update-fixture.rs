//! Нагрузка пробы: см. [`probe_update_lifecycle::fixture`].

use std::process::ExitCode;

fn main() -> ExitCode {
    match probe_update_lifecycle::helper_args(probe_update_lifecycle::fixture::FIXTURE_NAME) {
        Ok(args) => probe_update_lifecycle::fixture::main(&args),
        Err(code) => code,
    }
}
