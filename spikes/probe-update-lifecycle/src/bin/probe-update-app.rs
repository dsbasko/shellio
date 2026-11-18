//! Заглушка роли GUI (задача 4): сообщает поколение сборки и печатает манифест поколения, см.
//! [`probe_update_lifecycle::generation::stub_main`]. Роль GUI добавляет задача 6.

use std::process::ExitCode;

use probe_update_lifecycle::generation::{APP_BIN, stub_main};

fn main() -> ExitCode {
    match probe_update_lifecycle::helper_args(APP_BIN) {
        Ok(args) => stub_main(APP_BIN, &args),
        Err(code) => code,
    }
}
