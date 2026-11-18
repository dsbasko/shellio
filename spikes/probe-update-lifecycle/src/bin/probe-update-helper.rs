//! Помощник замены (задача 8): ждёт выхода GUI N, меняет бандлы местами, запускает N+1 и
//! сторожит попытку; `--report` — заглушка задачи 4. См.
//! [`probe_update_lifecycle::update::helper_main`].

use std::process::ExitCode;

use probe_update_lifecycle::generation::HELPER_BIN;

fn main() -> ExitCode {
    match probe_update_lifecycle::helper_args(HELPER_BIN) {
        Ok(args) => probe_update_lifecycle::update::helper_main(&args),
        Err(code) => code,
    }
}
