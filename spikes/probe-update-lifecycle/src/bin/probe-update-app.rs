//! Роль GUI пробы (задача 6): владение `shellio.lock` и управляющим сокетом, реестр сессий,
//! запуск хостов из `runtime/<N>/`, второй запуск; `--manifest` и `--report` задачи 4. См.
//! [`probe_update_lifecycle::owner`].

use std::process::ExitCode;

use probe_update_lifecycle::generation::APP_BIN;

fn main() -> ExitCode {
    match probe_update_lifecycle::helper_args(APP_BIN) {
        Ok(args) => probe_update_lifecycle::owner::app_main(&args),
        Err(code) => code,
    }
}
