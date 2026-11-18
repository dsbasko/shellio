//! Проба P0.5 `probe-update-lifecycle` (ADR-0034): протокол замены бандла по ADR-0019, владение
//! блокировкой и сокетом по ADR-0006 и рантаймы поколений по ADR-0004. Библиотека — модули,
//! которые идут в `make check`; бинарь `probe-update` только вызывает [`main`].
//!
//! Проба не зависит от других проб: нужное из `spikes/probe-session-survival` скопировано и
//! упрощено с пометкой источника в каждом модуле.

pub mod args;
pub mod attach;
pub mod fixture;
pub mod generation;
pub mod host;
pub mod launch;
pub mod observer;
pub mod proto;
pub mod pty;
pub mod results;
pub mod scenario;
pub mod sys;

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use args::{Mode, USAGE};

pub const PROBE_NAME: &str = "probe-update";
/// Код выхода при ошибке исполнения: стенд не дошёл до конца, предусловие не выполнено,
/// тайм-аут, сторож, повреждённый результат. `fail` и `not_run` ошибкой исполнения не считаются.
pub const EXIT_EXECUTION: u8 = 3;
pub const EXIT_USAGE: u8 = 2;
/// Поколение сборки: `PROBE_UPDATE_GENERATION` при сборке, иначе 1. `run.sh` собирает каждое
/// поколение в свой каталог cargo (задача 4).
pub const GENERATION: u32 = build_number(option_env!("PROBE_UPDATE_GENERATION"), 1);

/// Десятичное число из переменной сборки; разбор константный, неверное значение роняет сборку.
const fn build_number(value: Option<&str>, default: u32) -> u32 {
    let Some(text) = value else {
        return default;
    };
    let bytes = text.as_bytes();
    assert!(!bytes.is_empty(), "число при сборке пусто");
    let mut value: u64 = 0;
    let mut at = 0;
    while at < bytes.len() {
        assert!(bytes[at].is_ascii_digit(), "число при сборке: не цифра");
        value = value * 10 + (bytes[at] - b'0') as u64;
        assert!(value <= u32::MAX as u64, "число при сборке больше u32");
        at += 1;
    }
    value as u32
}

/// Вход раннера: разбор аргументов и выбор режима.
pub fn main() -> ExitCode {
    let entry_env: Vec<(String, String)> = std::env::vars_os()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let args = match std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(args) => args,
        Err(arg) => return usage_error(&format!("аргумент не в UTF-8: {arg:?}")),
    };
    let mode = match args::parse(&args) {
        Ok(mode) => mode,
        Err(err) => return usage_error(&err.to_string()),
    };
    match mode {
        Mode::Help => {
            let _ = writeln!(io::stdout().lock(), "{USAGE}");
            ExitCode::SUCCESS
        }
        Mode::Aggregate { run_dir } => aggregate(&run_dir),
        Mode::Launch(launch) => match std::env::current_exe() {
            Ok(program) => launch::run(launch, &program),
            Err(err) => {
                say(&format!("{PROBE_NAME}: --launch: путь раннера: {err}"));
                ExitCode::from(EXIT_EXECUTION)
            }
        },
        Mode::Scenario { target, nonce } => scenario::process_main(&target, nonce, &entry_env),
    }
}

/// `summary.json` прогона и сводка в stdout. Ненулевой код — только при ошибке исполнения хотя
/// бы одного процесса или самого агрегирования; выводы клеток на код не влияют.
fn aggregate(run_dir: &Path) -> ExitCode {
    let written = results::aggregate(run_dir)
        .and_then(|summary| results::write_summary(run_dir, &summary).map(|path| (summary, path)));
    let (summary, path) = match written {
        Ok(written) => written,
        Err(reason) => {
            say(&format!("{PROBE_NAME}: --aggregate: {reason}"));
            return ExitCode::from(EXIT_EXECUTION);
        }
    };
    let mut stdout = io::stdout().lock();
    let _ = write!(stdout, "{}", results::render_summary(&summary));
    let _ = writeln!(stdout, "сводка: {}", path.display());
    if summary.totals.execution_errors > 0 {
        ExitCode::from(EXIT_EXECUTION)
    } else {
        ExitCode::SUCCESS
    }
}

fn usage_error(message: &str) -> ExitCode {
    say(&format!("{PROBE_NAME}: {message}\n\n{USAGE}"));
    ExitCode::from(EXIT_USAGE)
}

pub(crate) fn say(text: &str) {
    let _ = writeln!(io::stderr().lock(), "{text}");
}

/// Аргументы вспомогательного бинаря без имени программы; не UTF-8 — ошибка использования.
pub fn helper_args(name: &str) -> Result<Vec<String>, ExitCode> {
    std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|arg| {
            say(&format!("{name}: аргумент не в UTF-8: {arg:?}"));
            ExitCode::from(EXIT_USAGE)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_numbers_parse_or_default() {
        assert_eq!(build_number(None, 1), 1);
        assert_eq!(build_number(Some("2"), 1), 2);
        assert_eq!(build_number(Some("4294967295"), 1), u32::MAX);
        const { assert!(GENERATION >= 1) };
    }

    #[test]
    #[should_panic(expected = "число при сборке")]
    fn malformed_build_number_does_not_fall_back() {
        build_number(Some("2a"), 1);
    }
}
