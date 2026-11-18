//! Заглушка помощника замены (задача 4): сообщает поколение сборки, см.
//! [`probe_update_lifecycle::generation::stub_main`]. Помощника добавляет задача 8.

use std::process::ExitCode;

use probe_update_lifecycle::generation::{HELPER_BIN, stub_main};

fn main() -> ExitCode {
    match probe_update_lifecycle::helper_args(HELPER_BIN) {
        Ok(args) => stub_main(HELPER_BIN, &args),
        Err(code) => code,
    }
}
