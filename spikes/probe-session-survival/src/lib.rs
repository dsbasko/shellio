//! Проба P0.4 `probe-session-survival` (ADR-0034, ADR-0035): выживание сессий на двух кандидатах
//! хоста, zmx и Rust-хосте. Библиотека — чистые модули, которые идут в `make check`; модули с FFI
//! над наборами пробы стоят за функцией `vt`. Бинарь `probe-session` только вызывает [`main`].

pub mod args;
pub mod backend;
pub mod bridge;
pub mod fixture;
pub mod host;
pub mod launch;
pub mod matrix;
pub mod observer;
pub mod oracle;
pub mod proto;
pub mod pty;
pub mod results;
pub mod stats;
#[cfg(feature = "surface")]
pub mod surface;
pub mod sys;
#[cfg(feature = "vt")]
pub mod vt;

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use args::{Mode, USAGE};

pub const PROBE_NAME: &str = "probe-session";
/// Код выхода при ошибке исполнения: стенд не дошёл до конца, предусловие не выполнено,
/// тайм-аут, сторож, повреждённый результат. `fail`, `not_run` и `unsupported` ошибкой
/// исполнения не считаются.
pub const EXIT_EXECUTION: u8 = 3;
pub const EXIT_USAGE: u8 = 2;

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
        Mode::Scenes { out } => scenes(&out),
        Mode::Launch(launch) => match std::env::current_exe() {
            Ok(program) => launch::run(launch, &program),
            Err(err) => {
                say(&format!("{PROBE_NAME}: --launch: путь раннера: {err}"));
                ExitCode::from(EXIT_EXECUTION)
            }
        },
        Mode::Scenario { target, nonce } => matrix::process_main(&target, nonce, &entry_env),
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

/// Байты сцен `report-policy`: кодировщику нужно состояние, снятое с `libghostty-vt`.
#[cfg(feature = "vt")]
fn scenes(out: &Path) -> ExitCode {
    match matrix::report_policy::build_scenes(out) {
        Ok(manifest) => {
            let _ = writeln!(io::stdout().lock(), "сцены: {}", manifest.display());
            ExitCode::SUCCESS
        }
        Err(reason) => {
            say(&format!("{PROBE_NAME}: --scenes: {reason}"));
            ExitCode::from(EXIT_EXECUTION)
        }
    }
}

#[cfg(not(feature = "vt"))]
fn scenes(_out: &Path) -> ExitCode {
    say(&format!("{PROBE_NAME}: --scenes требует функцию vt"));
    ExitCode::from(EXIT_EXECUTION)
}

fn usage_error(message: &str) -> ExitCode {
    say(&format!("{PROBE_NAME}: {message}\n\n{USAGE}"));
    ExitCode::from(EXIT_USAGE)
}

pub(crate) fn say(text: &str) {
    let _ = writeln!(io::stderr().lock(), "{text}");
}
