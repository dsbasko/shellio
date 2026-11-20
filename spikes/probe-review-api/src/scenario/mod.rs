//! Каталог сценариев пробы. Сценарий — строка матрицы возможностей или самопроверка стенда;
//! процесс сценария идёт в одном столбце и пишет один `result.json`.

use std::fs;
use std::path::PathBuf;

use crate::results::{Column, Execution, ResultFile, Status};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scenario {
    /// Пустой прогон стенда: временный каталог состояния, `result.json`, агрегирование.
    Smoke,
}

impl Scenario {
    pub const ALL: [Scenario; 1] = [Scenario::Smoke];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Smoke => "smoke",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Столбцы, в которых сценарий прогоняется.
    pub fn columns(self) -> &'static [Column] {
        match self {
            Scenario::Smoke => &[Column::Fake],
        }
    }

    /// Самопроверка стенда в матрицу возможностей не входит.
    pub fn is_stand(self) -> bool {
        match self {
            Scenario::Smoke => true,
        }
    }

    pub fn run(self, ctx: &Context, result: &mut ResultFile) {
        match self {
            Scenario::Smoke => smoke(ctx, result),
        }
    }
}

pub struct Context {
    pub column: Column,
    /// Каталог процесса, он же `--out`.
    pub out: PathBuf,
    /// Временный `SHELLIO_STATE_DIR` процесса.
    pub state_dir: PathBuf,
}

fn smoke(ctx: &Context, result: &mut ResultFile) {
    if !ctx.state_dir.is_dir() {
        result.execution = Execution::error(format!(
            "предусловие: SHELLIO_STATE_DIR {} не каталог",
            ctx.state_dir.display()
        ));
        return;
    }
    let probe = ctx.state_dir.join("smoke.probe");
    let written = fs::write(&probe, b"smoke").and_then(|()| fs::remove_file(&probe));
    if let Err(err) = written {
        result.execution =
            Execution::error(format!("запись во временный каталог состояния: {err}"));
        return;
    }
    if !ctx.out.is_dir() {
        result.execution = Execution::error(format!(
            "каталог процесса {} не существует",
            ctx.out.display()
        ));
        return;
    }
    result.status = Status::Pass;
    result.reason = format!(
        "стенд в столбце {}: временный каталог состояния доступен для записи, result.json пишется в каталог процесса",
        ctx.column.name()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for scenario in Scenario::ALL {
            assert_eq!(Scenario::from_name(scenario.name()), Some(scenario));
            assert!(!scenario.columns().is_empty());
        }
        assert_eq!(Scenario::from_name("nope"), None);
    }

    #[test]
    fn smoke_passes_in_writable_dirs() -> Result<(), String> {
        let state = tempfile::tempdir().map_err(|e| e.to_string())?;
        let out = tempfile::tempdir().map_err(|e| e.to_string())?;
        let ctx = Context {
            column: Column::Fake,
            out: out.path().to_path_buf(),
            state_dir: state.path().to_path_buf(),
        };
        let mut result = ResultFile::new("s", "b", "smoke", Column::Fake);
        Scenario::Smoke.run(&ctx, &mut result);
        assert_eq!(result.execution, Execution::Ok);
        assert_eq!(result.status, Status::Pass);
        assert!(result.problems().is_empty(), "{:?}", result.problems());
        Ok(())
    }

    #[test]
    fn smoke_without_state_dir_is_execution_error() {
        let ctx = Context {
            column: Column::Fake,
            out: PathBuf::from("/nonexistent-out"),
            state_dir: PathBuf::from("/nonexistent-state"),
        };
        let mut result = ResultFile::new("s", "b", "smoke", Column::Fake);
        Scenario::Smoke.run(&ctx, &mut result);
        assert!(!result.execution.is_ok());
    }
}
