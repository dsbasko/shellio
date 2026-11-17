//! Сценарий `bridge` в поверхности (план P0.4, задача 15). Процесс сценария — водитель без
//! поверхности; обе GUI — дочерние процессы `probe-session-surface --gui`, первую стенд снимает
//! настоящим `SIGKILL`, вторая открывает поверхность к той же сессии. Клетки и части —
//! `matrix::bridge`.

use std::path::{Path, PathBuf};

use crate::args::Target;
use crate::backend::zmx::ZmxSet;
use crate::host::HOST_NAME;
use crate::host::attach::ATTACH_NAME;
use crate::launch;
use crate::matrix::bridge;
use crate::results::{Environment, Report};

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    let candidate = target
        .backend
        .ok_or("предусловие: сценарий bridge идёт на кандидате (--backend)")?;
    let entry = &environment.process.env;
    let host_dir = required_dir(entry.get(launch::HOST_DIR), launch::HOST_DIR)?;
    for name in [HOST_NAME, ATTACH_NAME] {
        required_file(&host_dir.join(name))?;
    }
    let sets_dir = required_dir(entry.get(launch::SETS_DIR), launch::SETS_DIR)?;
    required_file(&ZmxSet::Current.binary(&sets_dir))?;
    let mut report = Report {
        cells: bridge::pending("до задачи 15: GUI-процессы моста ещё не построены"),
        ..Report::default()
    };
    report.diagnostics.notes.push(format!(
        "кандидат {candidate}, хост и attach-клиент из {}",
        host_dir.display()
    ));
    Ok(report)
}

fn required_dir(value: Option<&String>, name: &str) -> Result<PathBuf, String> {
    let dir = value
        .map(PathBuf::from)
        .ok_or_else(|| format!("предусловие: нет {name}"))?;
    if dir.is_dir() {
        Ok(dir)
    } else {
        Err(format!(
            "предусловие: {name}={}: каталога нет",
            dir.display()
        ))
    }
}

fn required_file(path: &Path) -> Result<(), String> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!("предусловие: нет {}", path.display()))
    }
}
