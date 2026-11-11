//! Временная заглушка P0.1: проверяет раскладку бандла и слинкованный libghostty и завершается.
//! Окно появится в фазе 1.

mod self_check;

use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
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
