//! Схема `result.json` процесса сценария и `summary.json` прогона, атомарная запись и
//! агрегирование.
//!
//! Источник: `spikes/probe-session-survival/src/results.rs` (P0.4), упрощено под P0.8: нет
//! кандидатов, матрицы и `unsupported`, есть итог «не подтверждено», точка осей и фон.
//!
//! Исполнение (`execution`) и выводы (`cells`) разделены. Ошибка исполнения роняет цель,
//! `pass`, `fail`, «не подтверждено» и `not_run` с причиной — нет (ADR-0034).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::background::BackgroundCheck;

pub const SCHEMA: u32 = 1;
pub const RESULT_FILE: &str = "result.json";
pub const NOT_STARTED_FILE: &str = "not-started.json";
pub const SUMMARY_FILE: &str = "summary.json";
pub const SUMMARY_MD_FILE: &str = "summary.md";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    NotRun,
    /// «Не подтверждено»: наблюдения по обе стороны порога или чистых блоков меньше нужного.
    Unconfirmed,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::NotRun => "not_run",
            Status::Unconfirmed => "не подтверждено",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotRunCause {
    /// «Шумно»: тишины фона не было на трёх попытках подряд.
    Noisy,
    /// «Замерная блокировка занята» на трёх попытках подряд.
    LockBusy,
    /// Наблюдению нужна настоящая поверхность libghostty, передача P2.4.
    NoSurface,
    /// Условие клетки не наступило, например отказ доступа не наблюдался.
    NotObserved,
    /// Не выполнено предусловие наблюдения.
    Precondition,
}

impl NotRunCause {
    pub fn label(self) -> &'static str {
        match self {
            NotRunCause::Noisy => "шумно",
            NotRunCause::LockBusy => "замерная блокировка занята",
            NotRunCause::NoSurface => "нет поверхности",
            NotRunCause::NotObserved => "не наблюдалось",
            NotRunCause::Precondition => "предусловие наблюдения",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub id: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_run_cause: Option<NotRunCause>,
    pub oracle: String,
    /// Доказательство вывода или причина `not_run`.
    pub reason: String,
}

impl Cell {
    pub fn new(id: &str, status: Status, oracle: &str, reason: impl Into<String>) -> Cell {
        Cell {
            id: id.to_owned(),
            status,
            not_run_cause: None,
            oracle: oracle.to_owned(),
            reason: reason.into(),
        }
    }

    pub fn not_run(id: &str, cause: NotRunCause, oracle: &str, reason: impl Into<String>) -> Cell {
        Cell {
            id: id.to_owned(),
            status: Status::NotRun,
            not_run_cause: Some(cause),
            oracle: oracle.to_owned(),
            reason: reason.into(),
        }
    }

    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.id.trim().is_empty() {
            problems.push("клетка без ID".to_owned());
        }
        if self.oracle.trim().is_empty() {
            problems.push(format!("клетка {}: пустой оракул", self.id));
        }
        if self.reason.trim().is_empty() {
            problems.push(format!("клетка {}: пустая причина", self.id));
        }
        match (self.status, self.not_run_cause) {
            (Status::NotRun, None) => {
                problems.push(format!("клетка {}: not_run без причины", self.id))
            }
            (status, Some(_)) if status != Status::NotRun => problems.push(format!(
                "клетка {}: причина not_run у итога {}",
                self.id,
                status.name()
            )),
            _ => {}
        }
        problems
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Execution {
    Ok,
    Error { reason: String },
}

impl Execution {
    pub fn error(reason: impl Into<String>) -> Execution {
        Execution::Error {
            reason: reason.into(),
        }
    }
}

/// Сырые значения замера с минимумом, медианой и максимумом.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub unit: String,
    pub samples: Vec<f64>,
    pub min: Option<f64>,
    pub median: Option<f64>,
    pub max: Option<f64>,
}

impl Metric {
    pub fn new(name: &str, unit: &str, samples: Vec<f64>) -> Metric {
        let mut sorted: Vec<f64> = samples.iter().copied().filter(|v| v.is_finite()).collect();
        sorted.sort_by(f64::total_cmp);
        let median = match sorted.len() {
            0 => None,
            n if n % 2 == 1 => Some(sorted[n / 2]),
            n => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2.0),
        };
        Metric {
            name: name.to_owned(),
            unit: unit.to_owned(),
            min: sorted.first().copied(),
            median,
            max: sorted.last().copied(),
            samples,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Os {
    pub product_version: Option<String>,
    pub build: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hardware {
    pub model: Option<String>,
    pub cpu: Option<String>,
    pub memory_bytes: Option<u64>,
    pub logical_cpus: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub commit: Option<String>,
    pub rustc: Option<String>,
    pub profile: String,
    pub binary_sha256: Option<String>,
    pub probe_version: String,
}

/// Режим энергопотребления на входе в сценарий.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Power {
    /// `AC Power` или `Battery Power` из `pmset -g batt`.
    pub source: Option<String>,
    /// `lowpowermode` из `pmset -g`.
    pub low_power_mode: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub state_dir: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    pub os: Os,
    pub hardware: Hardware,
    pub build: Build,
    pub power: Power,
    pub process: ProcessInfo,
}

/// Проверка фона с меткой момента: `before`, `during:<окно>`, `after`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackgroundRecord {
    pub label: String,
    pub check: BackgroundCheck,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub cells: Vec<Cell>,
    pub metrics: Vec<Metric>,
    pub background: Vec<BackgroundRecord>,
    pub raw: BTreeMap<String, serde_json::Value>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultFile {
    pub schema: u32,
    pub scenario: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<String>,
    pub environment: Environment,
    pub execution: Execution,
    pub background: Vec<BackgroundRecord>,
    pub cells: Vec<Cell>,
    pub metrics: Vec<Metric>,
    /// Сырые счётчики с идентичностями и отметками времени, квитанции фикстур.
    #[serde(default)]
    pub raw: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl ResultFile {
    pub fn new(
        scenario: &str,
        point: Option<&str>,
        environment: Environment,
        execution: Execution,
        report: Report,
    ) -> ResultFile {
        ResultFile {
            schema: SCHEMA,
            scenario: scenario.to_owned(),
            point: point.map(str::to_owned),
            environment,
            execution,
            background: report.background,
            cells: report.cells,
            metrics: report.metrics,
            raw: report.raw,
            notes: report.notes,
        }
    }

    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.schema != SCHEMA {
            problems.push(format!("схема {}, ожидается {SCHEMA}", self.schema));
        }
        if self.execution == Execution::Ok && self.cells.is_empty() {
            problems.push("исполнение без ошибки, но без клеток".to_owned());
        }
        let mut seen = std::collections::BTreeSet::new();
        for cell in &self.cells {
            problems.extend(cell.problems());
            if !seen.insert(cell.id.as_str()) {
                problems.push(format!("клетка {} записана дважды", cell.id));
            }
        }
        problems
    }
}

/// Запись процесса, который не дождался блокировки или тишины: код «не начат».
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotStartedRecord {
    pub schema: u32,
    pub scenario: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<String>,
    pub cause: NotRunCause,
    pub reason: String,
    #[serde(default)]
    pub background: Vec<BackgroundRecord>,
}

/// Пишет JSON через временный файл и переименование: оборванная запись не оставляет полупустой
/// файл под окончательным именем.
pub fn write_json(dir: &Path, name: &str, value: &impl Serialize) -> Result<PathBuf, String> {
    let mut bytes =
        serde_json::to_vec_pretty(value).map_err(|err| format!("сериализация {name}: {err}"))?;
    bytes.push(b'\n');
    write_atomic(dir, name, &bytes)
}

pub fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = dir.join(name);
    let partial = dir.join(format!(".{name}.partial"));
    fs::write(&partial, bytes).map_err(|err| format!("запись {}: {err}", partial.display()))?;
    fs::rename(&partial, &path)
        .map_err(|err| format!("переименование в {}: {err}", path.display()))?;
    Ok(path)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    Missing,
    Corrupt(String),
    Io(String),
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, ReadError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(ReadError::Missing),
        Err(err) => return Err(ReadError::Io(format!("{}: {err}", path.display()))),
    };
    serde_json::from_slice(&bytes)
        .map_err(|err| ReadError::Corrupt(format!("{}: {err}", path.display())))
}

/// Итог одного процесса сценария в сводке.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnitSummary {
    pub unit: String,
    pub scenario: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<String>,
    pub execution: Execution,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub schema: u32,
    pub units: Vec<UnitSummary>,
    /// Процессы с кодом «не начат», которые повторяются следующим вызовом.
    pub pending: Vec<String>,
    /// Ошибки исполнения: они роняют цель.
    pub errors: Vec<String>,
    pub counts: BTreeMap<String, usize>,
}

impl Summary {
    pub fn failed(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// Собирает сводку по каталогам процессов прогона. Каталог без `result.json` и без записи
/// «не начат» — потерянный результат, то есть ошибка исполнения.
pub fn aggregate(run_dir: &Path) -> Result<Summary, String> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(run_dir)
        .map_err(|err| format!("чтение {}: {err}", run_dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    let mut summary = Summary {
        schema: SCHEMA,
        units: Vec::new(),
        pending: Vec::new(),
        errors: Vec::new(),
        counts: BTreeMap::new(),
    };
    for dir in dirs {
        let unit = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        match read_json::<ResultFile>(&dir.join(RESULT_FILE)) {
            Ok(result) => {
                for problem in result.problems() {
                    summary.errors.push(format!("{unit}: {problem}"));
                }
                if let Execution::Error { reason } = &result.execution {
                    summary
                        .errors
                        .push(format!("{unit}: ошибка исполнения: {reason}"));
                }
                for cell in &result.cells {
                    *summary
                        .counts
                        .entry(cell.status.name().to_owned())
                        .or_default() += 1;
                }
                summary.units.push(UnitSummary {
                    unit,
                    scenario: result.scenario,
                    point: result.point,
                    execution: result.execution,
                    cells: result.cells,
                });
            }
            Err(ReadError::Missing) => {
                if dir.join(NOT_STARTED_FILE).exists() {
                    summary.pending.push(unit);
                } else {
                    summary.errors.push(format!(
                        "{unit}: нет {RESULT_FILE}, процесс не записал итог"
                    ));
                }
            }
            Err(ReadError::Corrupt(err) | ReadError::Io(err)) => {
                summary.errors.push(format!("{unit}: {err}"));
            }
        }
    }
    Ok(summary)
}

pub fn summary_markdown(summary: &Summary) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "| Сценарий | Точка | Клетка | Итог | Основание |");
    let _ = writeln!(out, "| --- | --- | --- | --- | --- |");
    for unit in &summary.units {
        for cell in &unit.cells {
            let status = match cell.not_run_cause {
                Some(cause) => format!("{} ({})", cell.status.name(), cause.label()),
                None => cell.status.name().to_owned(),
            };
            let _ = writeln!(
                out,
                "| `{}` | {} | `{}` | {} | {} |",
                unit.scenario,
                unit.point.as_deref().unwrap_or("—"),
                cell.id,
                status,
                cell.reason.replace('|', "\\|").replace('\n', " ")
            );
        }
    }
    for unit in &summary.pending {
        let _ = writeln!(
            out,
            "| `{unit}` | — | — | не начат | повтор следующим вызовом |"
        );
    }
    out
}

pub fn write_summary(run_dir: &Path, summary: &Summary) -> Result<(), String> {
    write_json(run_dir, SUMMARY_FILE, summary)?;
    write_atomic(
        run_dir,
        SUMMARY_MD_FILE,
        summary_markdown(summary).as_bytes(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "probe-resources-results-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn result(scenario: &str, cells: Vec<Cell>, execution: Execution) -> ResultFile {
        ResultFile::new(
            scenario,
            None,
            Environment::default(),
            execution,
            Report {
                cells,
                ..Report::default()
            },
        )
    }

    #[test]
    fn not_run_requires_cause_and_cause_requires_not_run() {
        let mut cell = Cell::new("x", Status::NotRun, "оракул", "причина");
        assert!(cell.problems().iter().any(|p| p.contains("без причины")));
        cell.not_run_cause = Some(NotRunCause::Noisy);
        assert!(cell.problems().is_empty());
        cell.status = Status::Pass;
        assert!(
            cell.problems()
                .iter()
                .any(|p| p.contains("причина not_run"))
        );
    }

    #[test]
    fn duplicate_and_empty_cells_are_problems() {
        let cell = Cell::new("x", Status::Pass, "оракул", "есть");
        let file = result("smoke", vec![cell.clone(), cell], Execution::Ok);
        assert!(file.problems().iter().any(|p| p.contains("дважды")));
        let empty = result("smoke", Vec::new(), Execution::Ok);
        assert!(!empty.problems().is_empty());
    }

    #[test]
    fn result_round_trips_through_atomic_write() {
        let dir = temp_dir("roundtrip");
        let file = result(
            "smoke",
            vec![Cell::new("smoke", Status::Unconfirmed, "оракул", "разброс")],
            Execution::Ok,
        );
        write_json(&dir, RESULT_FILE, &file).unwrap();
        assert!(!dir.join(format!(".{RESULT_FILE}.partial")).exists());
        let back: ResultFile = read_json(&dir.join(RESULT_FILE)).unwrap();
        assert_eq!(back, file);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn metric_median_ignores_order() {
        let metric = Metric::new("m", "%", vec![3.0, 1.0, 2.0, 10.0]);
        assert_eq!(metric.min, Some(1.0));
        assert_eq!(metric.median, Some(2.5));
        assert_eq!(metric.max, Some(10.0));
        assert_eq!(Metric::new("m", "%", Vec::new()).median, None);
    }

    #[test]
    fn aggregate_separates_verdicts_pending_and_errors() {
        let run = temp_dir("aggregate");
        for (unit, file) in [
            (
                "a",
                result(
                    "smoke",
                    vec![Cell::new("smoke", Status::Fail, "о", "превышение")],
                    Execution::Ok,
                ),
            ),
            (
                "b",
                result(
                    "smoke",
                    vec![Cell::not_run(
                        "smoke",
                        NotRunCause::Noisy,
                        "о",
                        "три попытки",
                    )],
                    Execution::Ok,
                ),
            ),
            ("c", result("smoke", Vec::new(), Execution::error("упал"))),
        ] {
            let dir = run.join(unit);
            fs::create_dir_all(&dir).unwrap();
            write_json(&dir, RESULT_FILE, &file).unwrap();
        }
        let pending = run.join("d");
        fs::create_dir_all(&pending).unwrap();
        fs::write(pending.join(NOT_STARTED_FILE), "{}").unwrap();
        fs::create_dir_all(run.join("e")).unwrap();

        let summary = aggregate(&run).unwrap();
        assert_eq!(summary.units.len(), 3);
        assert_eq!(summary.pending, vec!["d".to_owned()]);
        assert!(
            summary
                .errors
                .iter()
                .any(|e| e.starts_with("c: ошибка исполнения"))
        );
        assert!(
            summary
                .errors
                .iter()
                .any(|e| e.starts_with("e: нет result.json"))
        );
        assert_eq!(summary.counts.get("fail"), Some(&1));
        assert_eq!(summary.counts.get("not_run"), Some(&1));
        assert!(summary.failed());

        let md = summary_markdown(&summary);
        assert!(md.contains("not_run (шумно)"));
        assert!(md.contains("| `d` | — | — | не начат |"));
        write_summary(&run, &summary).unwrap();
        assert!(run.join(SUMMARY_FILE).exists() && run.join(SUMMARY_MD_FILE).exists());
        fs::remove_dir_all(&run).unwrap();
    }

    #[test]
    fn verdicts_alone_do_not_fail_the_run() {
        let run = temp_dir("verdicts");
        let dir = run.join("smoke");
        fs::create_dir_all(&dir).unwrap();
        let file = result(
            "smoke",
            vec![Cell::new("smoke", Status::Fail, "о", "отрицательный вывод")],
            Execution::Ok,
        );
        write_json(&dir, RESULT_FILE, &file).unwrap();
        assert!(!aggregate(&run).unwrap().failed());
        fs::remove_dir_all(&run).unwrap();
    }
}
