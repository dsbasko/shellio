//! Схема `result.json` процесса сценария, `launch.json` его запуска и `summary.json` прогона,
//! проверка результата и агрегирование.
//!
//! Исполнение (`execution`) и выводы (`cells`) разделены. Ошибка исполнения значит, что стенд не
//! дошёл до конца: падение, тайм-аут, сторож, нарушенное предусловие, повреждённый результат. Она
//! роняет цель. `pass`, `fail` и `unsupported` — законченные выводы, `not_run` — клетка без вывода
//! с записанной причиной; цель они не роняют (ADR-0034).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::matrix::{self, Candidate, Scenario};
use crate::stats::{self, Repeats, Summary};

pub const SCHEMA: u32 = 1;
pub const RESULT_FILE: &str = "result.json";
pub const LAUNCH_FILE: &str = "launch.json";
pub const SUMMARY_FILE: &str = "summary.json";

/// Итог клетки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    /// Вывода нет: кандидат исключён, стенд не доказал условие клетки или клетка ещё не
    /// реализована. Невозможность наблюдения — это `not_run`, а не `unsupported`.
    NotRun,
    /// У кандидата доказанно нет нужной возможности.
    Unsupported,
}

impl Status {
    pub const ALL: [Status; 4] = [
        Status::Pass,
        Status::Fail,
        Status::NotRun,
        Status::Unsupported,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::NotRun => "not_run",
            Status::Unsupported => "unsupported",
        }
    }

    /// Вес при сведении частей клетки: `pass` у клетки только при `pass` всех частей,
    /// недоказанная часть оставляет клетку `not_run`, доказанное отсутствие возможности важнее
    /// недоказанной части, а провал — всего остального.
    fn weight(self) -> u8 {
        match self {
            Status::Pass => 0,
            Status::NotRun => 1,
            Status::Unsupported => 2,
            Status::Fail => 3,
        }
    }

    pub fn combine(self, other: Status) -> Status {
        if other.weight() > self.weight() {
            other
        } else {
            self
        }
    }
}

/// Обязательная клетка или исследование.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    Required,
    Research,
}

impl Requirement {
    pub fn name(self) -> &'static str {
        match self {
            Requirement::Required => "required",
            Requirement::Research => "research",
        }
    }
}

/// Причина `not_run`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotRunCause {
    /// Кандидат исключён на рубеже решения.
    Excluded,
    /// Стенд не доказал условие клетки, например границу разреза.
    Stand,
    /// Клетка для кандидата ещё не реализована; к концу плана таких не остаётся.
    Pending,
    /// Контроль без хоста в той же поверхности провален: дефект поверхности P0.3, а не
    /// кандидата (F4, задача 19).
    Surface,
}

impl NotRunCause {
    pub fn name(self) -> &'static str {
        match self {
            NotRunCause::Excluded => "excluded",
            NotRunCause::Stand => "stand",
            NotRunCause::Pending => "pending",
            NotRunCause::Surface => "surface",
        }
    }
}

/// Где дефект, приведший к `fail` или `unsupported`. Записывается отдельно и статус не меняет.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefectOrigin {
    /// VT кандидата, например ghostty 1.3.2-dev внутри zmx.
    Vt,
    Host,
    Attach,
    Unknown,
}

/// Клетка матрицы или стенда в итоге процесса. Одна клетка может состоять из частей (`part`):
/// разрезы, режимы клиента, headless и поверхность; сводка сводит их по [`Status::combine`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<String>,
    pub status: Status,
    pub requirement: Requirement,
    pub stimulus: String,
    pub oracle: String,
    /// Условие завершения клетки.
    pub completion: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_run_cause: Option<NotRunCause>,
    /// Доказательство вывода или причина `not_run`.
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defect_origin: Option<DefectOrigin>,
    /// Сырые артефакты: пути внутри каталога процесса.
    #[serde(default)]
    pub artifacts: Vec<String>,
}

impl Cell {
    fn label(&self) -> String {
        match &self.part {
            Some(part) => format!("{}/{part}", self.id),
            None => self.id.clone(),
        }
    }

    /// Нарушения схемы клетки без обращения к файлам.
    pub fn problems(&self) -> Vec<String> {
        let label = self.label();
        let mut problems = Vec::new();
        if !matrix::is_known_cell(&self.id) {
            problems.push(format!("клетка {label}: неизвестный ID"));
        }
        if self
            .part
            .as_deref()
            .is_some_and(|part| part.trim().is_empty())
        {
            problems.push(format!("клетка {label}: пустое имя части"));
        }
        for (field, value) in [
            ("stimulus", &self.stimulus),
            ("oracle", &self.oracle),
            ("completion", &self.completion),
            ("reason", &self.reason),
        ] {
            if value.trim().is_empty() {
                problems.push(format!("клетка {label}: пустое поле {field}"));
            }
        }
        match (self.status, self.not_run_cause) {
            (Status::NotRun, None) => {
                problems.push(format!("клетка {label}: not_run без причины"));
            }
            (status, Some(_)) if status != Status::NotRun => problems.push(format!(
                "клетка {label}: причина not_run у статуса {}",
                status.name()
            )),
            _ => {}
        }
        if self.defect_origin.is_some()
            && !matches!(self.status, Status::Fail | Status::Unsupported)
        {
            problems.push(format!(
                "клетка {label}: происхождение дефекта у статуса {}",
                self.status.name()
            ));
        }
        if self.status != Status::NotRun && self.artifacts.is_empty() {
            problems.push(format!(
                "клетка {label}: вывод {} без сырого артефакта",
                self.status.name()
            ));
        }
        for artifact in &self.artifacts {
            if !is_inside(artifact) {
                problems.push(format!(
                    "клетка {label}: артефакт «{artifact}» вне каталога процесса"
                ));
            }
        }
        problems
    }
}

/// Относительный путь без `..`, то есть внутри каталога процесса.
fn is_inside(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Execution {
    Ok,
    Error {
        reason: String,
        /// Состояние, прочитанное в момент ошибки.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_state: Option<String>,
    },
}

impl Execution {
    pub fn is_ok(&self) -> bool {
        matches!(self, Execution::Ok)
    }

    pub fn error(reason: impl Into<String>) -> Execution {
        Execution::Error {
            reason: reason.into(),
            last_state: None,
        }
    }
}

/// Замер: сырые значения, повторы (медиана и разброс) и перцентили.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub unit: String,
    pub samples: Vec<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeats: Option<Repeats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percentiles: Option<Summary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Metric {
    pub fn new(name: impl Into<String>, unit: impl Into<String>, samples: Vec<f64>) -> Metric {
        let (repeats, percentiles, error) =
            match (stats::repeats(&samples), stats::summarize(&samples)) {
                (Ok(repeats), Ok(percentiles)) => (Some(repeats), Some(percentiles), None),
                (Err(err), _) | (_, Err(err)) => (None, None, Some(err.to_string())),
            };
        Metric {
            name: name.into(),
            unit: unit.into(),
            samples,
            repeats,
            percentiles,
            error,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostics {
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Os {
    /// `kern.osproductversion`, например 27.0.1.
    pub product_version: Option<String>,
    /// `kern.osversion`, сборка, например 26A434.
    pub build: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hardware {
    pub model: Option<String>,
    pub cpu: Option<String>,
    pub memory_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub commit: Option<String>,
    pub rustc: Option<String>,
    pub profile: String,
    pub probe_version: String,
    /// Включённые функции пробы.
    pub features: Vec<String>,
}

/// Процесс сценария и его окружение на входе в `main`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub nonce: Option<String>,
    pub state_dir: Option<String>,
    pub env: BTreeMap<String, String>,
    /// Переменные вне белого списка, которые не запрещены.
    pub env_extra: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    pub os: Os,
    pub hardware: Hardware,
    pub build: Build,
    /// Версии Zig, комбинаций zmx, ghostty поверхности и `libghostty-vt`.
    #[serde(default)]
    pub versions: BTreeMap<String, String>,
    /// Штампы наборов пробы `zmx`, `zmx-prev` и `vt`.
    #[serde(default)]
    pub stamps: BTreeMap<String, String>,
    /// Версия профиля нормализаций оракула.
    #[serde(default)]
    pub normalization_profile: Option<u32>,
    pub process: ProcessInfo,
}

/// Итог сценария, который собирает стенд.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub cells: Vec<Cell>,
    pub metrics: Vec<Metric>,
    pub diagnostics: Diagnostics,
}

impl Report {
    pub fn metric(&mut self, name: &str, unit: &str, samples: Vec<f64>) {
        self.metrics.push(Metric::new(name, unit, samples));
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultFile {
    pub schema: u32,
    pub scenario: String,
    pub backend: Option<Candidate>,
    pub variant: String,
    pub environment: Environment,
    pub execution: Execution,
    pub percentile_method: String,
    pub cells: Vec<Cell>,
    pub metrics: Vec<Metric>,
    pub diagnostics: Diagnostics,
}

impl ResultFile {
    pub fn new(
        scenario: &str,
        backend: Option<Candidate>,
        variant: &str,
        environment: Environment,
        execution: Execution,
        report: Report,
    ) -> ResultFile {
        ResultFile {
            schema: SCHEMA,
            scenario: scenario.to_owned(),
            backend,
            variant: variant.to_owned(),
            environment,
            execution,
            percentile_method: stats::METHOD.to_owned(),
            cells: report.cells,
            metrics: report.metrics,
            diagnostics: report.diagnostics,
        }
    }

    /// Нарушения схемы итога; артефакты ищутся в каталоге процесса `dir`.
    pub fn problems(&self, dir: &Path) -> Vec<String> {
        let mut problems = Vec::new();
        if self.schema != SCHEMA {
            problems.push(format!("схема {}, ожидается {SCHEMA}", self.schema));
        }
        match Scenario::from_name(&self.scenario) {
            None => problems.push(format!("неизвестный сценарий «{}»", self.scenario)),
            Some(scenario) => {
                let accepted = scenario.candidates();
                let fits = match self.backend {
                    None => accepted.is_empty(),
                    Some(backend) => accepted.contains(&backend),
                };
                if !fits {
                    problems.push(format!(
                        "сценарий {} не идёт на кандидате {}",
                        self.scenario,
                        self.backend.map_or("стенда", Candidate::name)
                    ));
                }
            }
        }
        let mut seen = BTreeSet::new();
        for cell in &self.cells {
            problems.extend(cell.problems());
            if !seen.insert((cell.id.as_str(), cell.part.as_deref())) {
                problems.push(format!("клетка {} записана дважды", cell.label()));
            }
            if matrix::is_matrix_cell(&cell.id) != self.backend.is_some() {
                problems.push(format!(
                    "клетка {}: клетка матрицы бывает только у кандидата, клетка стенда — только без него",
                    cell.label()
                ));
            }
            for artifact in &cell.artifacts {
                if is_inside(artifact) && !dir.join(artifact).exists() {
                    problems.push(format!(
                        "клетка {}: артефакта «{artifact}» нет",
                        cell.label()
                    ));
                }
            }
        }
        problems
    }
}

/// Процесс, найденный по nonce в аргументах.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRecord {
    pub pid: i32,
    pub command: String,
}

/// Как завершился запущенный процесс: код выхода или сигнал.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitRecord {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// Итог `--launch` одного процесса сценария. Сценарий, кандидат и вариант записаны здесь, чтобы
/// сводка знала их и без `result.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub schema: u32,
    pub scenario: String,
    pub backend: Option<Candidate>,
    pub variant: String,
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub nonce: String,
    pub state_dir: String,
    /// Отметка `CLOCK_UPTIME_RAW` непосредственно перед запуском процесса.
    pub spawn_uptime_ns: u64,
    pub exit_uptime_ns: Option<u64>,
    pub exit: Option<ExitRecord>,
    pub deadline_s: f64,
    pub watchdog_fired: bool,
    /// Процессы, которые снял внешний сторож.
    pub killed: Vec<ProcessRecord>,
    /// Процессы с nonce, оставшиеся после выхода процесса сценария.
    pub leftovers: Vec<ProcessRecord>,
    /// Содержимое временного `SHELLIO_STATE_DIR` после выхода.
    pub state_dir_leftovers: Vec<String>,
    pub result_present: bool,
    pub execution: Execution,
}

/// Пишет файл через временный и переименование, чтобы оборванная запись не оставила полупустой
/// JSON под окончательным именем.
pub fn write_json(dir: &Path, name: &str, value: &impl Serialize) -> Result<PathBuf, String> {
    let mut bytes =
        serde_json::to_vec_pretty(value).map_err(|err| format!("сериализация {name}: {err}"))?;
    bytes.push(b'\n');
    let path = dir.join(name);
    let partial = dir.join(format!(".{name}.partial"));
    fs::write(&partial, &bytes).map_err(|err| format!("запись {}: {err}", partial.display()))?;
    fs::rename(&partial, &path)
        .map_err(|err| format!("переименование в {}: {err}", path.display()))?;
    Ok(path)
}

pub fn write_result(dir: &Path, result: &ResultFile) -> Result<PathBuf, String> {
    write_json(dir, RESULT_FILE, result)
}

pub fn write_launch(dir: &Path, record: &LaunchRecord) -> Result<PathBuf, String> {
    write_json(dir, LAUNCH_FILE, record)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    Missing,
    Corrupt(String),
    Io(String),
}

fn read_json<T: DeserializeOwned>(dir: &Path, name: &str) -> Result<T, ReadError> {
    let bytes = match fs::read(dir.join(name)) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(ReadError::Missing),
        Err(err) => return Err(ReadError::Io(err.to_string())),
    };
    serde_json::from_slice(&bytes).map_err(|err| ReadError::Corrupt(err.to_string()))
}

pub fn read_result(dir: &Path) -> Result<ResultFile, ReadError> {
    read_json(dir, RESULT_FILE)
}

pub fn read_launch(dir: &Path) -> Result<LaunchRecord, ReadError> {
    read_json(dir, LAUNCH_FILE)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusCounts {
    pub pass: usize,
    pub fail: usize,
    pub not_run: usize,
    pub unsupported: usize,
}

impl StatusCounts {
    fn add(&mut self, status: Status) {
        match status {
            Status::Pass => self.pass += 1,
            Status::Fail => self.fail += 1,
            Status::NotRun => self.not_run += 1,
            Status::Unsupported => self.unsupported += 1,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub processes: usize,
    pub execution_ok: usize,
    pub execution_errors: usize,
    /// Сведённые клетки (клетка × кандидат) процессов без ошибки исполнения.
    pub cells: StatusCounts,
    /// Из них обязательные.
    pub required: StatusCounts,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSummary {
    /// Каталог процесса внутри прогона, он же `--out` процесса.
    pub dir: String,
    pub scenario: String,
    pub backend: Option<Candidate>,
    pub variant: String,
    pub execution: Execution,
    /// Записей клеток в `result.json`.
    pub cells: usize,
}

/// Часть сведённой клетки: запись одного процесса.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellPart {
    pub dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<String>,
    pub requirement: Requirement,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_run_cause: Option<NotRunCause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defect_origin: Option<DefectOrigin>,
    pub reason: String,
}

/// Клетка на кандидате (или на стенде) по всем процессам прогона.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellRow {
    pub id: String,
    pub backend: Option<Candidate>,
    /// Обязательная, если обязательна хотя бы одна часть.
    pub requirement: Requirement,
    /// Свод частей с той же обязательностью, что у клетки: части исследования итог обязательной
    /// клетки не меняют и видны в сводке отдельно.
    pub status: Status,
    pub parts: Vec<CellPart>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricRow {
    pub dir: String,
    pub scenario: String,
    pub backend: Option<Candidate>,
    pub variant: String,
    pub name: String,
    pub unit: String,
    pub repeats: Option<Repeats>,
    pub percentiles: Option<Summary>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SummaryFile {
    pub schema: u32,
    pub run_dir: String,
    pub percentile_method: String,
    pub totals: Totals,
    pub processes: Vec<ProcessSummary>,
    pub cells: Vec<CellRow>,
    pub metrics: Vec<MetricRow>,
}

/// Итог одного процесса. Отсутствующий, повреждённый или несогласованный `result.json` или
/// `launch.json` — ошибка исполнения процесса; клетки и замеры такого процесса в сводку не идут.
fn process_summary(dir: &Path, name: String) -> (ProcessSummary, Vec<Cell>, Vec<Metric>) {
    let mut reasons = Vec::new();
    let mut summary = ProcessSummary {
        dir: name,
        scenario: String::new(),
        backend: None,
        variant: String::new(),
        execution: Execution::Ok,
        cells: 0,
    };
    let launch = match read_launch(dir) {
        Ok(launch) => {
            summary.scenario.clone_from(&launch.scenario);
            summary.backend = launch.backend;
            summary.variant.clone_from(&launch.variant);
            if let Execution::Error { reason, .. } = &launch.execution {
                reasons.push(format!("запуск: {reason}"));
            }
            Some(launch)
        }
        Err(err) => {
            reasons.push(read_problem(
                LAUNCH_FILE,
                err,
                "процесс запущен не через --launch",
            ));
            None
        }
    };
    let mut cells = Vec::new();
    let mut metrics = Vec::new();
    match read_result(dir) {
        Ok(result) => {
            let mut problems = result.problems(dir);
            if let Some(launch) = &launch
                && (launch.scenario != result.scenario
                    || launch.backend != result.backend
                    || launch.variant != result.variant)
            {
                problems.push(format!(
                    "сценарий {} {} {} не совпадает с {LAUNCH_FILE}",
                    result.scenario,
                    result.backend.map_or("стенд", Candidate::name),
                    result.variant
                ));
            }
            if launch.is_none() {
                summary.scenario.clone_from(&result.scenario);
                summary.backend = result.backend;
                summary.variant.clone_from(&result.variant);
            }
            if !problems.is_empty() {
                reasons.push(format!("{RESULT_FILE} повреждён: {}", problems.join("; ")));
            }
            if let Execution::Error { reason, .. } = &result.execution {
                reasons.push(reason.clone());
            }
            summary.cells = result.cells.len();
            cells = result.cells;
            metrics = result.metrics;
        }
        Err(ReadError::Missing) if launch.as_ref().is_some_and(|l| l.watchdog_fired) => {
            reasons.push(format!(
                "{RESULT_FILE} отсутствует: процесс снял внешний сторож"
            ));
        }
        Err(err) => reasons.push(read_problem(RESULT_FILE, err, "")),
    }
    if !reasons.is_empty() {
        summary.execution = Execution::error(reasons.join("; "));
        cells.clear();
        metrics.clear();
    }
    (summary, cells, metrics)
}

fn read_problem(name: &str, err: ReadError, missing_hint: &str) -> String {
    match err {
        ReadError::Missing if missing_hint.is_empty() => format!("{name} отсутствует"),
        ReadError::Missing => format!("{name} отсутствует: {missing_hint}"),
        ReadError::Corrupt(err) => format!("{name} повреждён: {err}"),
        ReadError::Io(err) => format!("{name} не прочитан: {err}"),
    }
}

/// Собирает итоги всех процессов прогона: каждый подкаталог — один процесс. Клетки сводятся по
/// паре «клетка × кандидат» из процессов без ошибки исполнения.
pub fn aggregate(run_dir: &Path) -> Result<SummaryFile, String> {
    let entries = fs::read_dir(run_dir).map_err(|err| format!("{}: {err}", run_dir.display()))?;
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("{}: {err}", run_dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            dirs.push((entry.file_name().to_string_lossy().into_owned(), path));
        }
    }
    if dirs.is_empty() {
        return Err(format!("в {} нет каталогов процессов", run_dir.display()));
    }
    dirs.sort();

    let mut totals = Totals::default();
    let mut processes = Vec::new();
    let mut rows: BTreeMap<(usize, String, Option<Candidate>), CellRow> = BTreeMap::new();
    let mut metric_rows = Vec::new();
    for (name, path) in dirs {
        let (process, cells, metrics) = process_summary(&path, name);
        totals.processes += 1;
        if process.execution.is_ok() {
            totals.execution_ok += 1;
        } else {
            totals.execution_errors += 1;
        }
        for cell in cells {
            let key = (
                matrix::cell_order(&cell.id),
                cell.id.clone(),
                process.backend,
            );
            let row = rows.entry(key).or_insert_with(|| CellRow {
                id: cell.id.clone(),
                backend: process.backend,
                requirement: cell.requirement,
                status: Status::Pass,
                parts: Vec::new(),
            });
            if cell.requirement == Requirement::Required {
                row.requirement = Requirement::Required;
            }
            row.parts.push(CellPart {
                dir: process.dir.clone(),
                part: cell.part,
                requirement: cell.requirement,
                status: cell.status,
                not_run_cause: cell.not_run_cause,
                defect_origin: cell.defect_origin,
                reason: cell.reason,
            });
        }
        for metric in metrics {
            metric_rows.push(MetricRow {
                dir: process.dir.clone(),
                scenario: process.scenario.clone(),
                backend: process.backend,
                variant: process.variant.clone(),
                name: metric.name,
                unit: metric.unit,
                repeats: metric.repeats,
                percentiles: metric.percentiles,
            });
        }
        processes.push(process);
    }
    let mut cells: Vec<CellRow> = rows.into_values().collect();
    for row in &mut cells {
        row.status = row
            .parts
            .iter()
            .filter(|part| part.requirement == row.requirement)
            .fold(Status::Pass, |status, part| status.combine(part.status));
    }
    for row in &cells {
        totals.cells.add(row.status);
        if row.requirement == Requirement::Required {
            totals.required.add(row.status);
        }
    }
    Ok(SummaryFile {
        schema: SCHEMA,
        run_dir: run_dir.display().to_string(),
        percentile_method: stats::METHOD.to_owned(),
        totals,
        processes,
        cells,
        metrics: metric_rows,
    })
}

pub fn write_summary(run_dir: &Path, summary: &SummaryFile) -> Result<PathBuf, String> {
    write_json(run_dir, SUMMARY_FILE, summary)
}

fn counts_text(counts: &StatusCounts) -> String {
    format!(
        "pass {}, fail {}, not_run {}, unsupported {}",
        counts.pass, counts.fail, counts.not_run, counts.unsupported
    )
}

/// Текстовая сводка: исполнение, таблица клеток по кандидатам, выводы кроме `pass` с причинами,
/// замеры.
pub fn render_summary(summary: &SummaryFile) -> String {
    let totals = &summary.totals;
    let mut text = format!(
        "исполнение: процессов {}, без ошибок {}, с ошибкой {}\n",
        totals.processes, totals.execution_ok, totals.execution_errors
    );
    for process in &summary.processes {
        if let Execution::Error { reason, .. } = &process.execution {
            let _ = writeln!(text, "  ошибка  {}: {reason}", process.dir);
        }
    }
    let _ = writeln!(
        text,
        "клетки: {}; обязательные: {}",
        counts_text(&totals.cells),
        counts_text(&totals.required)
    );
    let columns: Vec<Option<Candidate>> = std::iter::once(None)
        .chain(Candidate::ALL.into_iter().map(Some))
        .filter(|column| summary.cells.iter().any(|row| row.backend == *column))
        .collect();
    if !columns.is_empty() {
        let mut header = format!("  {:<8}", "клетка");
        for column in &columns {
            let _ = write!(header, " {:<12}", column.map_or("стенд", Candidate::name));
        }
        let _ = writeln!(text, "{}", header.trim_end());
        let mut ids: Vec<(usize, &str)> = summary
            .cells
            .iter()
            .map(|row| (matrix::cell_order(&row.id), row.id.as_str()))
            .collect();
        ids.dedup();
        for (_, id) in ids {
            let mut line = format!("  {id:<8}");
            for column in &columns {
                let status = summary
                    .cells
                    .iter()
                    .find(|row| row.id == id && row.backend == *column)
                    .map_or("—", |row| row.status.name());
                let _ = write!(line, " {status:<12}");
            }
            let _ = writeln!(text, "{}", line.trim_end());
        }
    }
    for row in &summary.cells {
        for part in row.parts.iter().filter(|part| part.status != Status::Pass) {
            let cause = part
                .not_run_cause
                .map(|cause| format!(" ({})", cause.name()))
                .unwrap_or_default();
            let _ = writeln!(
                text,
                "  {:<11} {:<8} {} {}{}{cause} [{}]: {}",
                part.status.name(),
                part.requirement.name(),
                row.id,
                row.backend.map_or("стенд", Candidate::name),
                part.part
                    .as_deref()
                    .map(|part| format!("/{part}"))
                    .unwrap_or_default(),
                part.dir,
                part.reason
            );
        }
    }
    if !summary.metrics.is_empty() {
        let _ = writeln!(text, "замеры:");
        for metric in &summary.metrics {
            let values = match (metric.repeats, metric.percentiles) {
                (Some(repeats), Some(percentiles)) => format!(
                    "медиана {:.3}, разброс {:.3}, p95 {:.3} {} (n={})",
                    repeats.median, repeats.spread, percentiles.p95, metric.unit, repeats.count
                ),
                _ => "нет значений".to_owned(),
            };
            let _ = writeln!(
                text,
                "  {} {} {} {}: {values}",
                metric.scenario,
                metric.backend.map_or("стенд", Candidate::name),
                metric.variant,
                metric.name
            );
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::matrix::tests::environment;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "probe-session-results-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn cell(id: &str, status: Status) -> Cell {
        Cell {
            id: id.to_owned(),
            part: None,
            status,
            requirement: Requirement::Required,
            stimulus: "стимул".to_owned(),
            oracle: "оракул".to_owned(),
            completion: "условие".to_owned(),
            not_run_cause: (status == Status::NotRun).then_some(NotRunCause::Stand),
            reason: format!("{id}: доказательство"),
            defect_origin: (status == Status::Fail).then_some(DefectOrigin::Host),
            artifacts: if status == Status::NotRun {
                Vec::new()
            } else {
                vec!["trace.txt".to_owned()]
            },
        }
    }

    fn launch_record(scenario: &str, backend: Option<Candidate>) -> LaunchRecord {
        LaunchRecord {
            schema: SCHEMA,
            scenario: scenario.to_owned(),
            backend,
            variant: "base".to_owned(),
            program: "/t/probe-session".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            nonce: "0123abcd".to_owned(),
            state_dir: "/tmp/shellio-probe-state.0123abcd".to_owned(),
            spawn_uptime_ns: 1,
            exit_uptime_ns: Some(2),
            exit: Some(ExitRecord {
                code: Some(0),
                signal: None,
            }),
            deadline_s: 20.0,
            watchdog_fired: false,
            killed: Vec::new(),
            leftovers: Vec::new(),
            state_dir_leftovers: Vec::new(),
            result_present: true,
            execution: Execution::Ok,
        }
    }

    /// Каталог процесса сценария `smoke` с `result.json`, `launch.json` и артефактами клеток.
    fn write_process(run: &Path, name: &str, result: &ResultFile) -> PathBuf {
        let dir = run.join(name);
        fs::create_dir_all(&dir).unwrap();
        for artifact in result.cells.iter().flat_map(|cell| &cell.artifacts) {
            fs::write(dir.join(artifact), "сырые данные").unwrap();
        }
        write_result(&dir, result).unwrap();
        write_launch(&dir, &launch_record(&result.scenario, result.backend)).unwrap();
        dir
    }

    fn smoke_result(execution: Execution, cells: Vec<Cell>) -> ResultFile {
        ResultFile::new(
            "smoke",
            None,
            "base",
            environment(),
            execution,
            Report {
                cells,
                ..Report::default()
            },
        )
    }

    #[test]
    fn status_names_match_schema() {
        for status in Status::ALL {
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{}\"", status.name())
            );
        }
        assert_eq!(
            serde_json::from_str::<Status>("\"not_run\"").unwrap(),
            Status::NotRun
        );
        assert!(serde_json::from_str::<Status>("\"not-run\"").is_err());
    }

    #[test]
    fn combine_orders_statuses() {
        use Status::*;
        assert_eq!(Pass.combine(Pass), Pass);
        assert_eq!(Pass.combine(NotRun), NotRun);
        assert_eq!(NotRun.combine(Unsupported), Unsupported);
        assert_eq!(Unsupported.combine(Fail), Fail);
        assert_eq!(Fail.combine(Pass), Fail);
        assert_eq!(Unsupported.combine(NotRun), Unsupported);
    }

    #[test]
    fn valid_cells_have_no_problems() {
        for status in Status::ALL {
            assert!(
                cell("A1", status).problems().is_empty(),
                "{status:?}: {:?}",
                cell("A1", status).problems()
            );
        }
    }

    #[test]
    fn not_run_and_unsupported_are_kept_apart() {
        let mut not_run = cell("B7", Status::NotRun);
        not_run.not_run_cause = None;
        assert_eq!(not_run.problems(), ["клетка B7: not_run без причины"]);

        let mut unsupported = cell("B7", Status::Unsupported);
        unsupported.not_run_cause = Some(NotRunCause::Stand);
        assert_eq!(
            unsupported.problems(),
            ["клетка B7: причина not_run у статуса unsupported"]
        );
        unsupported.not_run_cause = None;
        unsupported.artifacts.clear();
        assert_eq!(
            unsupported.problems(),
            ["клетка B7: вывод unsupported без сырого артефакта"]
        );
        // not_run не требует артефакта: наблюдения не было.
        assert!(cell("B7", Status::NotRun).problems().is_empty());
    }

    #[test]
    fn malformed_cells_are_reported() {
        let mut bad = cell("H9", Status::Pass);
        bad.part = Some(" ".to_owned());
        bad.oracle.clear();
        bad.defect_origin = Some(DefectOrigin::Vt);
        bad.artifacts = vec!["../escape".to_owned(), "/abs".to_owned()];
        let problems = bad.problems();
        assert_eq!(problems.len(), 6, "{problems:?}");
        assert!(problems[0].contains("неизвестный ID"));
        assert!(problems.iter().any(|p| p.contains("пустое поле oracle")));
        assert!(problems.iter().any(|p| p.contains("происхождение дефекта")));
        assert_eq!(
            problems
                .iter()
                .filter(|p| p.contains("вне каталога процесса"))
                .count(),
            2
        );
    }

    #[test]
    fn result_round_trips_and_checks_files() {
        let tmp = TempDir::new("round-trip");
        let mut result = smoke_result(Execution::Ok, vec![cell("smoke", Status::Pass)]);
        result
            .metrics
            .push(Metric::new("attach_ms", "ms", vec![3.0, 1.0, 2.0]));
        let dir = write_process(&tmp.0, "smoke-base", &result);
        let read = read_result(&dir).unwrap();
        assert_eq!(read, result);
        assert!(read.problems(&dir).is_empty(), "{:?}", read.problems(&dir));
        let json = fs::read_to_string(dir.join(RESULT_FILE)).unwrap();
        assert!(json.contains("\"backend\": null"));
        assert!(json.contains("\"status\": \"pass\""));

        fs::remove_file(dir.join("trace.txt")).unwrap();
        assert_eq!(
            read.problems(&dir),
            ["клетка smoke: артефакта «trace.txt» нет"]
        );
    }

    #[test]
    fn result_rejects_foreign_shapes() {
        let tmp = TempDir::new("shapes");
        fs::write(tmp.0.join("trace.txt"), "").unwrap();
        let mut result = smoke_result(
            Execution::Ok,
            vec![cell("smoke", Status::Pass), cell("smoke", Status::Pass)],
        );
        result.schema = 9;
        let problems = result.problems(&tmp.0);
        assert!(problems[0].starts_with("схема 9"));
        assert!(problems.iter().any(|p| p.contains("записана дважды")));

        let matrix_cell_on_stand = smoke_result(Execution::Ok, vec![cell("A1", Status::Pass)]);
        assert_eq!(matrix_cell_on_stand.problems(&tmp.0).len(), 1);

        let mut on_candidate = smoke_result(Execution::Ok, Vec::new());
        on_candidate.backend = Some(Candidate::Zmx);
        assert_eq!(
            on_candidate.problems(&tmp.0),
            ["сценарий smoke не идёт на кандидате zmx"]
        );
        on_candidate.scenario = "no-such".to_owned();
        assert_eq!(
            on_candidate.problems(&tmp.0),
            ["неизвестный сценарий «no-such»"]
        );
        on_candidate.scenario = "lifecycle".to_owned();
        assert!(on_candidate.problems(&tmp.0).is_empty());
    }

    #[test]
    fn metric_keeps_median_spread_and_errors() {
        let metric = Metric::new("cpu", "%", vec![0.5, 0.75, 0.625, 1.5, 0.5625]);
        let repeats = metric.repeats.unwrap();
        assert_eq!((repeats.median, repeats.spread), (0.625, 1.0));
        assert_eq!(metric.percentiles.unwrap().p95, 1.5);
        let empty = Metric::new("cpu", "%", Vec::new());
        assert_eq!(empty.error.as_deref(), Some("пустой набор замеров"));
        assert!(empty.repeats.is_none() && empty.percentiles.is_none());
    }

    #[test]
    fn aggregate_separates_execution_errors_from_findings() {
        let tmp = TempDir::new("aggregate");
        let run = &tmp.0;
        write_process(
            run,
            "a-fail",
            &smoke_result(Execution::Ok, vec![cell("smoke", Status::Fail)]),
        );
        write_process(
            run,
            "b-error",
            &smoke_result(
                Execution::error("сторож сценария"),
                vec![cell("smoke", Status::Pass)],
            ),
        );
        let missing = run.join("c-missing");
        fs::create_dir(&missing).unwrap();
        let mut record = launch_record("smoke", None);
        record.watchdog_fired = true;
        write_launch(&missing, &record).unwrap();
        let corrupt = run.join("d-corrupt");
        fs::create_dir(&corrupt).unwrap();
        fs::write(corrupt.join(RESULT_FILE), "{").unwrap();
        write_launch(&corrupt, &launch_record("smoke", None)).unwrap();
        let unlaunched = write_process(
            run,
            "e-unlaunched",
            &smoke_result(Execution::Ok, vec![cell("smoke", Status::Pass)]),
        );
        fs::remove_file(unlaunched.join(LAUNCH_FILE)).unwrap();
        fs::write(run.join(SUMMARY_FILE), "{}").unwrap();

        let summary = aggregate(run).unwrap();
        assert_eq!(summary.totals.processes, 5);
        assert_eq!(summary.totals.execution_ok, 1);
        assert_eq!(summary.totals.execution_errors, 4);
        let reasons: Vec<String> = summary
            .processes
            .iter()
            .map(|process| match &process.execution {
                Execution::Ok => "ok".to_owned(),
                Execution::Error { reason, .. } => reason.clone(),
            })
            .collect();
        assert_eq!(reasons[0], "ok", "fail — вывод, а не ошибка исполнения");
        assert_eq!(reasons[1], "сторож сценария");
        assert_eq!(
            reasons[2],
            "result.json отсутствует: процесс снял внешний сторож"
        );
        assert!(reasons[3].starts_with("result.json повреждён"));
        assert_eq!(
            reasons[4],
            "launch.json отсутствует: процесс запущен не через --launch"
        );
        assert_eq!(summary.processes[4].scenario, "smoke");
        // Клетки процессов с ошибкой исполнения в сводку не идут.
        assert_eq!(summary.cells.len(), 1);
        assert_eq!(summary.cells[0].status, Status::Fail);
        assert_eq!(summary.totals.cells.fail, 1);
        assert_eq!(summary.totals.cells.pass, 0);
        let text = render_summary(&summary);
        assert!(text.starts_with("исполнение: процессов 5, без ошибок 1, с ошибкой 4\n"));
        assert!(text.contains("  ошибка  b-error: сторож сценария\n"));
        assert!(text.contains("клетки: pass 0, fail 1, not_run 0, unsupported 0"));
    }

    #[test]
    fn aggregate_rejects_mismatch_with_launch() {
        let tmp = TempDir::new("mismatch");
        let dir = write_process(
            &tmp.0,
            "smoke-base",
            &smoke_result(Execution::Ok, vec![cell("smoke", Status::Pass)]),
        );
        let mut record = launch_record("smoke", None);
        record.variant = "other".to_owned();
        write_launch(&dir, &record).unwrap();
        let summary = aggregate(&tmp.0).unwrap();
        assert_eq!(summary.totals.execution_errors, 1);
        assert!(summary.cells.is_empty());
    }

    #[test]
    fn aggregate_combines_parts_by_candidate() {
        let tmp = TempDir::new("parts");
        let run = &tmp.0;
        let row = |status: Status, part: &str| Cell {
            part: Some(part.to_owned()),
            ..cell("smoke", status)
        };
        write_process(
            run,
            "a",
            &smoke_result(
                Execution::Ok,
                vec![row(Status::Pass, "first"), row(Status::NotRun, "second")],
            ),
        );
        let mut research = cell("smoke", Status::Unsupported);
        research.requirement = Requirement::Research;
        research.part = Some("third".to_owned());
        write_process(run, "b", &smoke_result(Execution::Ok, vec![research]));

        let summary = aggregate(run).unwrap();
        assert_eq!(summary.cells.len(), 1);
        let row = &summary.cells[0];
        assert_eq!(
            row.status,
            Status::NotRun,
            "unsupported части исследования не перевешивает обязательную"
        );
        assert_eq!(row.requirement, Requirement::Required);
        assert_eq!(row.parts.len(), 3);
        assert_eq!(summary.totals.required.not_run, 1);
        let text = render_summary(&summary);
        assert!(
            text.contains("  клетка   стенд\n  smoke    not_run\n"),
            "{text}"
        );
        assert!(text.contains("not_run     required smoke стенд/second (stand) [a]"));
        assert!(text.contains("unsupported research smoke стенд/third [b]"));
        assert!(!text.contains("стенд/first"));
    }

    #[test]
    fn research_part_does_not_change_required_cell() {
        let tmp = TempDir::new("research-part");
        let run = &tmp.0;
        let mut research = Cell {
            part: Some("stress".to_owned()),
            ..cell("smoke", Status::NotRun)
        };
        research.requirement = Requirement::Research;
        let required = Cell {
            part: Some("window".to_owned()),
            ..cell("smoke", Status::Pass)
        };
        write_process(
            run,
            "a",
            &smoke_result(Execution::Ok, vec![required, research]),
        );

        let summary = aggregate(run).unwrap();
        let row = &summary.cells[0];
        assert_eq!(row.requirement, Requirement::Required);
        assert_eq!(row.status, Status::Pass);
        assert_eq!(summary.totals.required.pass, 1);
        assert_eq!(summary.totals.required.not_run, 0);
        let text = render_summary(&summary);
        assert!(text.contains("  smoke    pass\n"), "{text}");
        assert!(
            text.contains("not_run     research smoke стенд/stress (stand) [a]"),
            "часть исследования видна среди выводов кроме pass: {text}"
        );
    }

    #[test]
    fn research_only_cell_combines_all_parts() {
        let tmp = TempDir::new("research-only");
        let run = &tmp.0;
        let research = |status: Status, part: &str| {
            let mut cell = Cell {
                part: Some(part.to_owned()),
                ..cell("smoke", status)
            };
            cell.requirement = Requirement::Research;
            cell
        };
        write_process(
            run,
            "a",
            &smoke_result(
                Execution::Ok,
                vec![
                    research(Status::Pass, "first"),
                    research(Status::NotRun, "second"),
                ],
            ),
        );

        let summary = aggregate(run).unwrap();
        let row = &summary.cells[0];
        assert_eq!(row.requirement, Requirement::Research);
        assert_eq!(row.status, Status::NotRun);
        assert_eq!(summary.totals.cells.not_run, 1);
        assert_eq!(summary.totals.required.not_run, 0);
    }

    #[test]
    fn aggregate_of_empty_or_missing_run_is_an_error() {
        let tmp = TempDir::new("empty");
        assert!(aggregate(&tmp.0).unwrap_err().contains("нет каталогов"));
        assert!(aggregate(&tmp.0.join("nope")).is_err());
    }

    #[test]
    fn rendered_metrics() {
        let tmp = TempDir::new("metrics");
        let mut result = smoke_result(Execution::Ok, vec![cell("smoke", Status::Pass)]);
        result
            .metrics
            .push(Metric::new("idle_cpu", "%", vec![1.0, 3.0, 2.0]));
        result.metrics.push(Metric::new("broken", "ms", Vec::new()));
        write_process(&tmp.0, "smoke-base", &result);
        let summary = aggregate(&tmp.0).unwrap();
        assert_eq!(summary.metrics.len(), 2);
        let text = render_summary(&summary);
        assert!(text.contains(
            "  smoke стенд base idle_cpu: медиана 2.000, разброс 2.000, p95 3.000 % (n=3)"
        ));
        assert!(text.contains("  smoke стенд base broken: нет значений"));
    }
}
