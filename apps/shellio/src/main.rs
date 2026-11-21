//! `shellio`: владелец профиля или второй запуск на `NSApplication` без окна (ADR-0006).
//!
//! Процесс стартует с политикой `.accessory`: без Dock, но с правом стать активным. Владелец
//! переходит в `.regular`, второй запуск остаётся `.accessory` и получает передний план только
//! чтобы до отправки `launch` передать его владельцу.
//!
//! `--self-check` — самопроверка бандла P0.1: выполняется до профиля, блокировки и AppKit и
//! выходит. Окно появится в P1.3.

mod app_ui;
mod self_check;

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::Arc;

use shellio::launch::{self, Mode};
use shellio::owner;
use shellio_appkit::App;
use shellio_store::Profile;

use crate::app_ui::{AppDelegate, AppUi};

fn main() -> ExitCode {
    let args = match launch::mode(std::env::args_os().skip(1).collect()) {
        Mode::SelfCheck => return self_check_main(),
        Mode::Usage(message) => {
            owner::say_err(format_args!("shellio: {message}"));
            return ExitCode::from(2);
        }
        Mode::Run(args) => args,
    };
    let ui = Arc::new(AppUi::new());
    // Чтение окружения, а не файловый ввод-вывод: каталог создаёт задача runtime.
    let profile = Profile::from_process_env();
    let _runtime = match owner::start(Arc::clone(&ui), args, profile) {
        Ok(runtime) => runtime,
        Err(error) => {
            owner::say_err(format_args!("shellio: runtime не запущен: {error}"));
            return ExitCode::FAILURE;
        }
    };
    // Штатный выход — внутри AppKit или через `Ui::exit`: возврат из `run` значит отказ.
    match App::run(AppDelegate::new(ui)) {
        Ok(()) => owner::say_err("shellio: цикл приложения остановлен до выхода процесса"),
        Err(error) => owner::say_err(format_args!("shellio: {error}")),
    }
    owner::exit_process(1)
}

fn self_check_main() -> ExitCode {
    let result = self_check::run().and_then(|line| {
        writeln!(io::stdout().lock(), "{line}").map_err(self_check::Error::Stdout)
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Если и stderr закрыт, сообщить уже некуда; код выхода остаётся 1.
            let _ = writeln!(
                io::stderr().lock(),
                "shellio: самопроверка не прошла: {err}"
            );
            ExitCode::FAILURE
        }
    }
}
