//! Схема `result.json` процесса сценария, `launch.json` его запуска и `summary.json` прогона,
//! каталог клеток, проверка результата, агрегирование и итог пробы по контракту ADR-0019.
//!
//! Исполнение (`execution`) и выводы (`cells`) разделены. Ошибка исполнения значит, что стенд не
//! дошёл до конца: падение, тайм-аут, сторож, пропавшее предусловие, повреждённый результат. Она
//! роняет цель. `pass` и `fail` — проверенная гипотеза, `not_run` — заранее перечисленная
//! недоступная часть наблюдения с причиной; цель они не роняют (ADR-0034). У обязательной части
//! `not_run` не бывает: такой результат повреждён, и процесс получает ошибку исполнения.
//!
//! Форма записей и агрегирования взята из `spikes/probe-session-survival/src/results.rs` без
//! кандидатов хоста, `unsupported` и замеров.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::scenario::Scenario;

pub const SCHEMA: u32 = 1;
pub const RESULT_FILE: &str = "result.json";
pub const LAUNCH_FILE: &str = "launch.json";
pub const SUMMARY_FILE: &str = "summary.json";

/// Итог части клетки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    /// Часть наблюдения, которую автоматика не наблюдает; причина записана.
    NotRun,
}

impl Status {
    pub const ALL: [Status; 3] = [Status::Pass, Status::Fail, Status::NotRun];

    pub fn name(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::NotRun => "not_run",
        }
    }

    /// Вес при сведении частей: `pass` только при `pass` всех частей, провал важнее
    /// ненаблюдённой части.
    fn weight(self) -> u8 {
        match self {
            Status::Pass => 0,
            Status::NotRun => 1,
            Status::Fail => 2,
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

/// Вид клетки по разделу плана «Рубежи и клетки».
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Обязательная клетка: её обязательные части `not_run` не получают.
    Required,
    /// Клетка-наблюдение (E, F, G): `not_run` не блокирует план.
    Observation,
    /// Клетка стенда: самопроверка, в итог пробы не входит.
    Stand,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Required => "required",
            Kind::Observation => "observation",
            Kind::Stand => "stand",
        }
    }
}

/// Обязательность части клетки. У обязательной клетки бывают части-наблюдения: устойчивость
/// отсрочки после `SIGKILL` в D2, дополнительные наблюдения приёмочного прохода.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    Required,
    Observation,
}

impl Requirement {
    pub fn name(self) -> &'static str {
        match self {
            Requirement::Required => "required",
            Requirement::Observation => "observation",
        }
    }
}

/// Самопроверка стенда: разрешения терминала исполнителя и окружение процесса.
pub const SMOKE_CELL: &str = "smoke";
/// Самопроверка стенда: минимальный хост, attach-клиент и нагрузка.
pub const HOST_CELL: &str = "host";

/// Каталог клеток в порядке сводки.
pub const CELLS: [(&str, Kind); 14] = [
    ("A1", Kind::Required),
    ("A2", Kind::Required),
    ("B1", Kind::Required),
    ("B2", Kind::Required),
    ("B3", Kind::Required),
    ("C1", Kind::Required),
    ("D1", Kind::Required),
    ("D2", Kind::Required),
    ("H", Kind::Required),
    ("E", Kind::Observation),
    ("F", Kind::Observation),
    ("G", Kind::Observation),
    (SMOKE_CELL, Kind::Stand),
    (HOST_CELL, Kind::Stand),
];

pub fn kind_of(id: &str) -> Option<Kind> {
    CELLS
        .iter()
        .find(|(known, _)| *known == id)
        .map(|(_, kind)| *kind)
}

/// Порядок строк сводки: по каталогу, неизвестные в конце.
pub fn cell_order(id: &str) -> usize {
    CELLS
        .iter()
        .position(|(known, _)| *known == id)
        .unwrap_or(CELLS.len())
}

/// Часть клетки в итоге процесса. Клетка без частей записывается одной записью с `part: None`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<String>,
    pub requirement: Requirement,
    pub status: Status,
    pub stimulus: String,
    pub oracle: String,
    /// Условие завершения части.
    pub completion: String,
    /// Доказательство вывода или причина `not_run`.
    pub reason: String,
    /// Сырые артефакты: пути внутри каталога процесса.
    #[serde(default)]
    pub artifacts: Vec<String>,
}

impl Cell {
    pub fn label(&self) -> String {
        match &self.part {
            Some(part) => format!("{}/{part}", self.id),
            None => self.id.clone(),
        }
    }

    /// Нарушения схемы части без обращения к файлам.
    pub fn problems(&self) -> Vec<String> {
        let label = self.label();
        let mut problems = Vec::new();
        match kind_of(&self.id) {
            None => problems.push(format!("клетка {label}: неизвестный ID")),
            Some(Kind::Observation) if self.requirement == Requirement::Required => problems.push(
                format!("клетка {label}: обязательная часть у клетки-наблюдения"),
            ),
            Some(_) => {}
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
        if self.status == Status::NotRun && self.requirement == Requirement::Required {
            problems.push(format!(
                "клетка {label}: not_run у обязательной части; невыполненное предусловие — ошибка исполнения"
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
    /// Версии внешних инструментов, например Homebrew.
    #[serde(default)]
    pub versions: BTreeMap<String, String>,
    /// CDHash подписанных бандлов стенда по именам из `bundles.json` (задача 4). Пусто, если
    /// процесс шёл без бандлов; ошибку чтения `bundles.json` несёт ключ `error`.
    #[serde(default)]
    pub generations: BTreeMap<String, String>,
    pub process: ProcessInfo,
}

/// Итог сценария, который собирает стенд.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub cells: Vec<Cell>,
    pub diagnostics: Diagnostics,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultFile {
    pub schema: u32,
    pub scenario: String,
    pub variant: String,
    pub environment: Environment,
    pub execution: Execution,
    pub cells: Vec<Cell>,
    pub diagnostics: Diagnostics,
}

impl ResultFile {
    pub fn new(
        scenario: &str,
        variant: &str,
        environment: Environment,
        execution: Execution,
        report: Report,
    ) -> ResultFile {
        ResultFile {
            schema: SCHEMA,
            scenario: scenario.to_owned(),
            variant: variant.to_owned(),
            environment,
            execution,
            cells: report.cells,
            diagnostics: report.diagnostics,
        }
    }

    /// Нарушения схемы итога; артефакты ищутся в каталоге процесса `dir`.
    pub fn problems(&self, dir: &Path) -> Vec<String> {
        let mut problems = Vec::new();
        if self.schema != SCHEMA {
            problems.push(format!("схема {}, ожидается {SCHEMA}", self.schema));
        }
        if Scenario::from_name(&self.scenario).is_none() {
            problems.push(format!("неизвестный сценарий «{}»", self.scenario));
        }
        let mut seen = BTreeSet::new();
        for cell in &self.cells {
            problems.extend(cell.problems());
            if !seen.insert((cell.id.as_str(), cell.part.as_deref())) {
                problems.push(format!("клетка {} записана дважды", cell.label()));
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

/// Итог `--launch` одного процесса сценария. Сценарий и вариант записаны здесь, чтобы сводка
/// знала их и без `result.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub schema: u32,
    pub scenario: String,
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
}

impl StatusCounts {
    fn add(&mut self, status: Status) {
        match status {
            Status::Pass => self.pass += 1,
            Status::Fail => self.fail += 1,
            Status::NotRun => self.not_run += 1,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub processes: usize,
    pub execution_ok: usize,
    pub execution_errors: usize,
    /// Сведённые клетки процессов без ошибки исполнения.
    pub cells: StatusCounts,
    /// Из них обязательные.
    pub required: StatusCounts,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSummary {
    /// Каталог процесса внутри прогона, он же `--out` процесса.
    pub dir: String,
    pub scenario: String,
    pub variant: String,
    pub execution: Execution,
    /// Записей частей в `result.json`.
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
    pub reason: String,
}

/// Клетка по всем процессам прогона.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellRow {
    pub id: String,
    pub kind: Kind,
    /// Свод частей. У обязательной клетки — только обязательных частей: части-наблюдения её
    /// итог не меняют и входят в итог пробы отдельно. У наблюдения и стенда — всех частей.
    pub status: Status,
    pub parts: Vec<CellPart>,
}

/// Итог пробы по контракту ADR-0019 (раздел плана «Результаты»).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Outcome {
    /// Все клетки каталога, кроме стенда, — `pass`.
    Confirmed,
    /// Обязательные `pass`, в наблюдениях есть `not_run`.
    ConfirmedExcept { not_run: Vec<String> },
    /// Наблюдённый `fail` обязательной клетки или наблюдения.
    NotConfirmed { failed: Vec<String> },
    /// Итог не выносится: прогон неполный или в нём есть ошибки исполнения.
    Undetermined { reasons: Vec<String> },
}

impl Outcome {
    pub fn describe(&self) -> String {
        match self {
            Outcome::Confirmed => "подтверждено".to_owned(),
            Outcome::ConfirmedExcept { not_run } => {
                format!("подтверждено, кроме: {}", not_run.join(", "))
            }
            Outcome::NotConfirmed { failed } => {
                format!("не подтверждено: {}", failed.join(", "))
            }
            Outcome::Undetermined { reasons } => {
                format!("не вынесен: {}", reasons.join("; "))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SummaryFile {
    pub schema: u32,
    pub run_dir: String,
    pub totals: Totals,
    pub processes: Vec<ProcessSummary>,
    pub cells: Vec<CellRow>,
    pub outcome: Outcome,
}

/// Итог одного процесса. Отсутствующий, повреждённый или несогласованный `result.json` или
/// `launch.json` — ошибка исполнения процесса; клетки такого процесса в сводку не идут.
fn process_summary(dir: &Path, name: String) -> (ProcessSummary, Vec<Cell>) {
    let mut reasons = Vec::new();
    let mut summary = ProcessSummary {
        dir: name,
        scenario: String::new(),
        variant: String::new(),
        execution: Execution::Ok,
        cells: 0,
    };
    let launch = match read_launch(dir) {
        Ok(launch) => {
            summary.scenario.clone_from(&launch.scenario);
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
    match read_result(dir) {
        Ok(result) => {
            let mut problems = result.problems(dir);
            if let Some(launch) = &launch
                && (launch.scenario != result.scenario || launch.variant != result.variant)
            {
                problems.push(format!(
                    "сценарий {} {} не совпадает с {LAUNCH_FILE}",
                    result.scenario, result.variant
                ));
            }
            if launch.is_none() {
                summary.scenario.clone_from(&result.scenario);
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
    }
    (summary, cells)
}

fn read_problem(name: &str, err: ReadError, missing_hint: &str) -> String {
    match err {
        ReadError::Missing if missing_hint.is_empty() => format!("{name} отсутствует"),
        ReadError::Missing => format!("{name} отсутствует: {missing_hint}"),
        ReadError::Corrupt(err) => format!("{name} повреждён: {err}"),
        ReadError::Io(err) => format!("{name} не прочитан: {err}"),
    }
}

/// Собирает итоги всех процессов прогона: каждый подкаталог — один процесс. Части клеток
/// сводятся из процессов без ошибки исполнения.
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
    let mut rows: BTreeMap<(usize, String), CellRow> = BTreeMap::new();
    for (name, path) in dirs {
        let (process, cells) = process_summary(&path, name);
        totals.processes += 1;
        if process.execution.is_ok() {
            totals.execution_ok += 1;
        } else {
            totals.execution_errors += 1;
        }
        for cell in cells {
            // Неизвестный ID отвергла проверка схемы, процесс с ним сюда не дошёл.
            let kind = kind_of(&cell.id).unwrap_or(Kind::Stand);
            let row = rows
                .entry((cell_order(&cell.id), cell.id.clone()))
                .or_insert_with(|| CellRow {
                    id: cell.id.clone(),
                    kind,
                    status: Status::Pass,
                    parts: Vec::new(),
                });
            row.parts.push(CellPart {
                dir: process.dir.clone(),
                part: cell.part,
                requirement: cell.requirement,
                status: cell.status,
                reason: cell.reason,
            });
        }
        processes.push(process);
    }
    let mut cells: Vec<CellRow> = rows.into_values().collect();
    for row in &mut cells {
        row.status = row
            .parts
            .iter()
            .filter(|part| row.kind != Kind::Required || part.requirement == Requirement::Required)
            .fold(Status::Pass, |status, part| status.combine(part.status));
    }
    for row in &cells {
        totals.cells.add(row.status);
        if row.kind == Kind::Required {
            totals.required.add(row.status);
        }
    }
    let outcome = outcome(&cells, &processes);
    Ok(SummaryFile {
        schema: SCHEMA,
        run_dir: run_dir.display().to_string(),
        totals,
        processes,
        cells,
        outcome,
    })
}

fn part_label(row: &CellRow, part: &CellPart) -> String {
    match &part.part {
        Some(name) => format!("{}/{name}", row.id),
        None => row.id.clone(),
    }
}

/// Итог пробы по сведённым клеткам и процессам прогона. Неполный прогон итога не получает:
/// процесс с ошибкой исполнения мог нести недостающие части, сценарий без процесса — тоже (клетки
/// A1, A2 и D2 собираются из двух сценариев), а клетка каталога, которой нет, и обязательная
/// клетка без обязательных частей — не `pass`.
pub fn outcome(cells: &[CellRow], processes: &[ProcessSummary]) -> Outcome {
    let mut reasons = Vec::new();
    let execution_errors = processes
        .iter()
        .filter(|process| !process.execution.is_ok())
        .count();
    if execution_errors > 0 {
        reasons.push(format!("ошибок исполнения: {execution_errors}"));
    }
    let absent: Vec<&str> = Scenario::ALL
        .iter()
        .map(|scenario| scenario.name())
        .filter(|name| {
            !processes
                .iter()
                .any(|process| process.execution.is_ok() && process.scenario == *name)
        })
        .collect();
    if !absent.is_empty() {
        reasons.push(format!(
            "нет сценариев без ошибки исполнения: {}",
            absent.join(", ")
        ));
    }
    let missing: Vec<&str> = CELLS
        .iter()
        .filter(|(id, kind)| *kind != Kind::Stand && !cells.iter().any(|row| row.id == *id))
        .map(|(id, _)| *id)
        .collect();
    if !missing.is_empty() {
        reasons.push(format!("нет клеток в прогоне: {}", missing.join(", ")));
    }
    let hollow: Vec<&str> = cells
        .iter()
        .filter(|row| {
            row.kind == Kind::Required
                && !row
                    .parts
                    .iter()
                    .any(|part| part.requirement == Requirement::Required)
        })
        .map(|row| row.id.as_str())
        .collect();
    if !hollow.is_empty() {
        reasons.push(format!(
            "у обязательных клеток нет обязательных частей: {}",
            hollow.join(", ")
        ));
    }
    if !reasons.is_empty() {
        return Outcome::Undetermined { reasons };
    }
    let mut failed = Vec::new();
    let mut not_run = Vec::new();
    for row in cells.iter().filter(|row| row.kind != Kind::Stand) {
        for part in &row.parts {
            match part.status {
                Status::Pass => {}
                Status::Fail => failed.push(part_label(row, part)),
                Status::NotRun => not_run.push(part_label(row, part)),
            }
        }
    }
    failed.dedup();
    not_run.dedup();
    if !failed.is_empty() {
        Outcome::NotConfirmed { failed }
    } else if !not_run.is_empty() {
        Outcome::ConfirmedExcept { not_run }
    } else {
        Outcome::Confirmed
    }
}

pub fn write_summary(run_dir: &Path, summary: &SummaryFile) -> Result<PathBuf, String> {
    write_json(run_dir, SUMMARY_FILE, summary)
}

fn counts_text(counts: &StatusCounts) -> String {
    format!(
        "pass {}, fail {}, not_run {}",
        counts.pass, counts.fail, counts.not_run
    )
}

/// Текстовая сводка: исполнение, клетки с видом и итогом, части кроме `pass` с причинами, итог
/// пробы.
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
    for row in &summary.cells {
        let _ = writeln!(
            text,
            "  {:<6} {:<12} {:<8} частей {}",
            row.id,
            row.kind.name(),
            row.status.name(),
            row.parts.len()
        );
    }
    for row in &summary.cells {
        for part in row.parts.iter().filter(|part| part.status != Status::Pass) {
            let _ = writeln!(
                text,
                "  {:<8} {:<11} {} [{}]: {}",
                part.status.name(),
                part.requirement.name(),
                part_label(row, part),
                part.dir,
                part.reason
            );
        }
    }
    let _ = writeln!(text, "итог пробы: {}", summary.outcome.describe());
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "probe-update-results-{name}-{}-{}",
            std::process::id(),
            crate::sys::uptime_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cell(id: &str, part: Option<&str>, requirement: Requirement, status: Status) -> Cell {
        Cell {
            id: id.to_owned(),
            part: part.map(str::to_owned),
            requirement,
            status,
            stimulus: "стимул".to_owned(),
            oracle: "оракул".to_owned(),
            completion: "условие".to_owned(),
            reason: "доказательство".to_owned(),
            artifacts: if status == Status::NotRun {
                Vec::new()
            } else {
                vec!["raw.json".to_owned()]
            },
        }
    }

    fn required(id: &str, status: Status) -> Cell {
        cell(id, None, Requirement::Required, status)
    }

    fn environment() -> Environment {
        Environment {
            os: Os {
                product_version: Some("27.0.1".to_owned()),
                build: Some("26A434".to_owned()),
            },
            hardware: Hardware::default(),
            build: Build {
                commit: Some("5c3d886".to_owned()),
                rustc: None,
                profile: "release".to_owned(),
                probe_version: "0.1.0".to_owned(),
            },
            versions: BTreeMap::new(),
            generations: BTreeMap::new(),
            process: ProcessInfo::default(),
        }
    }

    fn result(cells: Vec<Cell>) -> ResultFile {
        ResultFile::new(
            "smoke",
            "base",
            environment(),
            Execution::Ok,
            Report {
                cells,
                diagnostics: Diagnostics::default(),
            },
        )
    }

    fn launch(scenario: &str, execution: Execution, watchdog_fired: bool) -> LaunchRecord {
        LaunchRecord {
            schema: SCHEMA,
            scenario: scenario.to_owned(),
            variant: "base".to_owned(),
            program: "/t/probe-update".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            nonce: "n1".to_owned(),
            state_dir: "/tmp/psu.n1".to_owned(),
            spawn_uptime_ns: 1,
            exit_uptime_ns: Some(2),
            exit: Some(ExitRecord {
                code: Some(0),
                signal: None,
            }),
            deadline_s: 70.0,
            watchdog_fired,
            killed: Vec::new(),
            leftovers: Vec::new(),
            state_dir_leftovers: Vec::new(),
            result_present: !watchdog_fired,
            execution,
        }
    }

    /// Каталог процесса с `launch.json`, `result.json` и артефактом `raw.json`.
    fn process(run: &Path, name: &str, cells: Vec<Cell>) -> PathBuf {
        let dir = run.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("raw.json"), "{}").unwrap();
        write_launch(&dir, &launch("smoke", Execution::Ok, false)).unwrap();
        write_result(&dir, &result(cells)).unwrap();
        dir
    }

    fn row(id: &str, parts: &[(Requirement, Status)]) -> CellRow {
        CellRow {
            id: id.to_owned(),
            kind: kind_of(id).unwrap(),
            status: Status::Pass,
            parts: parts
                .iter()
                .enumerate()
                .map(|(index, (requirement, status))| CellPart {
                    dir: "p".to_owned(),
                    part: (parts.len() > 1).then(|| format!("p{index}")),
                    requirement: *requirement,
                    status: *status,
                    reason: "r".to_owned(),
                })
                .collect(),
        }
    }

    fn summary(scenario: &str, execution: Execution) -> ProcessSummary {
        ProcessSummary {
            dir: format!("{scenario}-base"),
            scenario: scenario.to_owned(),
            variant: "base".to_owned(),
            execution,
            cells: 1,
        }
    }

    /// Процесс без ошибки на каждый сценарий.
    fn all_scenarios() -> Vec<ProcessSummary> {
        Scenario::ALL
            .iter()
            .map(|scenario| summary(scenario.name(), Execution::Ok))
            .collect()
    }

    fn full_catalogue(observation: Status) -> Vec<CellRow> {
        CELLS
            .iter()
            .filter(|(_, kind)| *kind != Kind::Stand)
            .map(|(id, kind)| match kind {
                Kind::Observation => row(id, &[(Requirement::Observation, observation)]),
                _ => row(id, &[(Requirement::Required, Status::Pass)]),
            })
            .collect()
    }

    #[test]
    fn statuses_combine_by_weight() {
        assert_eq!(Status::Pass.combine(Status::NotRun), Status::NotRun);
        assert_eq!(Status::NotRun.combine(Status::Fail), Status::Fail);
        assert_eq!(Status::Fail.combine(Status::Pass), Status::Fail);
        assert_eq!(Status::Pass.combine(Status::Pass), Status::Pass);
        let names: Vec<&str> = Status::ALL.iter().map(|status| status.name()).collect();
        assert_eq!(names, ["pass", "fail", "not_run"]);
    }

    #[test]
    fn catalogue_follows_the_plan() {
        let required: Vec<&str> = CELLS
            .iter()
            .filter(|(_, kind)| *kind == Kind::Required)
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            required,
            ["A1", "A2", "B1", "B2", "B3", "C1", "D1", "D2", "H"]
        );
        let observations: Vec<&str> = CELLS
            .iter()
            .filter(|(_, kind)| *kind == Kind::Observation)
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(observations, ["E", "F", "G"]);
        assert_eq!(kind_of(SMOKE_CELL), Some(Kind::Stand));
        assert_eq!(kind_of(HOST_CELL), Some(Kind::Stand));
        assert_eq!(kind_of("B4"), None);
        assert!(cell_order("A1") < cell_order("H"));
        assert!(cell_order("H") < cell_order("E"));
        assert_eq!(cell_order("B4"), CELLS.len());
    }

    #[test]
    fn valid_cells_have_no_problems() {
        assert!(required("B1", Status::Pass).problems().is_empty());
        assert!(required("B1", Status::Fail).problems().is_empty());
        assert!(
            cell("E", Some("gui-n"), Requirement::Observation, Status::NotRun)
                .problems()
                .is_empty()
        );
        assert!(
            cell(
                "D2",
                Some("deferred-after-kill"),
                Requirement::Observation,
                Status::NotRun
            )
            .problems()
            .is_empty()
        );
    }

    #[test]
    fn required_part_never_gets_not_run() {
        let problems = required("B2", Status::NotRun).problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("not_run у обязательной части"));
    }

    #[test]
    fn observation_cell_has_no_required_parts() {
        let problems = cell("F", None, Requirement::Required, Status::Pass).problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("обязательная часть у клетки-наблюдения"));
    }

    #[test]
    fn malformed_cells_are_reported() {
        let mut broken = required("Z9", Status::Pass);
        broken.part = Some(" ".to_owned());
        broken.reason = String::new();
        broken.artifacts = vec!["../outside.json".to_owned(), "/abs.json".to_owned()];
        let problems = broken.problems();
        assert_eq!(problems.len(), 5, "{problems:?}");
        assert!(problems[0].contains("неизвестный ID"));
        assert!(problems[1].contains("пустое имя части"));
        assert!(problems[2].contains("пустое поле reason"));
        assert!(problems[3].contains("вне каталога процесса"));
        let mut bare = required("A1", Status::Fail);
        bare.artifacts.clear();
        assert!(bare.problems()[0].contains("без сырого артефакта"));
    }

    #[test]
    fn result_problems_check_artifacts_duplicates_and_scenario() {
        let dir = temp_dir("problems");
        let mut file = result(vec![
            required("A1", Status::Pass),
            required("A1", Status::Fail),
        ]);
        let problems = file.problems(&dir);
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("артефакта «raw.json» нет"));
        assert!(problems[1].contains("записана дважды"));
        fs::write(dir.join("raw.json"), "{}").unwrap();
        file.cells.pop();
        assert!(file.problems(&dir).is_empty());
        file.scenario = "nope".to_owned();
        file.schema = 0;
        assert_eq!(file.problems(&dir).len(), 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn result_round_trips() {
        let dir = temp_dir("round-trip");
        let file = result(vec![cell(
            "E",
            Some("gui-n"),
            Requirement::Observation,
            Status::NotRun,
        )]);
        write_result(&dir, &file).unwrap();
        assert_eq!(read_result(&dir).unwrap(), file);
        assert!(!dir.join(".result.json.partial").exists());
        fs::write(dir.join(RESULT_FILE), "{").unwrap();
        assert!(matches!(read_result(&dir), Err(ReadError::Corrupt(_))));
        assert_eq!(read_launch(&dir), Err(ReadError::Missing));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn outcome_of_a_complete_run() {
        assert_eq!(
            outcome(&full_catalogue(Status::Pass), &all_scenarios()),
            Outcome::Confirmed
        );
        assert_eq!(
            outcome(&full_catalogue(Status::NotRun), &all_scenarios()),
            Outcome::ConfirmedExcept {
                not_run: vec!["E".to_owned(), "F".to_owned(), "G".to_owned()]
            }
        );
        let mut observed_fail = full_catalogue(Status::NotRun);
        observed_fail[10] = row("F", &[(Requirement::Observation, Status::Fail)]);
        assert_eq!(
            outcome(&observed_fail, &all_scenarios()),
            Outcome::NotConfirmed {
                failed: vec!["F".to_owned()]
            }
        );
        let mut required_fail = full_catalogue(Status::Pass);
        required_fail[3] = row(
            "B2",
            &[
                (Requirement::Required, Status::Pass),
                (Requirement::Required, Status::Fail),
            ],
        );
        assert_eq!(
            outcome(&required_fail, &all_scenarios()),
            Outcome::NotConfirmed {
                failed: vec!["B2/p1".to_owned()]
            }
        );
    }

    #[test]
    fn observation_part_of_a_required_cell_counts_as_observation() {
        let mut cells = full_catalogue(Status::Pass);
        cells[7] = row(
            "D2",
            &[
                (Requirement::Required, Status::Pass),
                (Requirement::Observation, Status::NotRun),
            ],
        );
        assert_eq!(
            outcome(&cells, &all_scenarios()),
            Outcome::ConfirmedExcept {
                not_run: vec!["D2/p1".to_owned()]
            }
        );
    }

    #[test]
    fn partial_run_or_execution_error_has_no_verdict() {
        let mut partial = full_catalogue(Status::Pass);
        partial.retain(|row| row.id != "C1" && row.id != "G");
        match outcome(&partial, &all_scenarios()) {
            Outcome::Undetermined { reasons } => {
                assert_eq!(reasons, ["нет клеток в прогоне: C1, G"]);
            }
            other => panic!("{other:?}"),
        }
        let mut errored = all_scenarios();
        errored.push(summary("smoke", Execution::error("сторож")));
        errored.push(summary("smoke", Execution::error("падение")));
        match outcome(&full_catalogue(Status::Pass), &errored) {
            Outcome::Undetermined { reasons } => {
                assert_eq!(reasons, ["ошибок исполнения: 2"]);
            }
            other => panic!("{other:?}"),
        }
        let smoke_only = vec![row(SMOKE_CELL, &[(Requirement::Required, Status::Pass)])];
        assert!(matches!(
            outcome(&smoke_only, &all_scenarios()),
            Outcome::Undetermined { .. }
        ));
    }

    #[test]
    fn scenario_without_a_clean_process_leaves_no_verdict() {
        let mut processes = all_scenarios();
        let first = processes.remove(0);
        match outcome(&full_catalogue(Status::Pass), &processes) {
            Outcome::Undetermined { reasons } => {
                assert_eq!(
                    reasons,
                    [format!(
                        "нет сценариев без ошибки исполнения: {}",
                        first.scenario
                    )]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn required_cell_of_observation_parts_only_leaves_no_verdict() {
        let mut cells = full_catalogue(Status::Pass);
        cells[7] = row("D2", &[(Requirement::Observation, Status::Pass)]);
        match outcome(&cells, &all_scenarios()) {
            Outcome::Undetermined { reasons } => {
                assert_eq!(
                    reasons,
                    ["у обязательных клеток нет обязательных частей: D2"]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn aggregate_rolls_up_parts_and_skips_errored_processes() {
        let run = temp_dir("aggregate");
        process(
            &run,
            "a-smoke",
            vec![cell(SMOKE_CELL, None, Requirement::Required, Status::Pass)],
        );
        process(
            &run,
            "b-incompatible",
            vec![
                cell("D2", Some("deferral"), Requirement::Required, Status::Pass),
                cell(
                    "D2",
                    Some("after-kill"),
                    Requirement::Observation,
                    Status::Fail,
                ),
            ],
        );
        // Ошибка исполнения: клетки этого процесса в сводку не идут.
        let errored = run.join("c-kill");
        fs::create_dir_all(&errored).unwrap();
        fs::write(errored.join("raw.json"), "{}").unwrap();
        write_launch(&errored, &launch("smoke", Execution::Ok, false)).unwrap();
        let mut failed = result(vec![required("B2", Status::Fail)]);
        failed.execution = Execution::error("стенд не дошёл до конца");
        write_result(&errored, &failed).unwrap();
        // Сторож снял процесс до result.json.
        let hung = run.join("d-owner");
        fs::create_dir_all(&hung).unwrap();
        write_launch(&hung, &launch("smoke", Execution::Ok, true)).unwrap();
        // Повреждённый результат: not_run у обязательной части.
        process(&run, "e-env", vec![required("B3", Status::NotRun)]);
        // Процесс мимо --launch.
        let bare = run.join("f-bare");
        fs::create_dir_all(&bare).unwrap();
        write_result(&bare, &result(Vec::new())).unwrap();

        let summary = aggregate(&run).unwrap();
        assert_eq!(summary.totals.processes, 6);
        assert_eq!(summary.totals.execution_ok, 2);
        assert_eq!(summary.totals.execution_errors, 4);
        let ids: Vec<&str> = summary.cells.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["D2", SMOKE_CELL]);
        assert_eq!(summary.cells[0].status, Status::Pass);
        assert_eq!(summary.cells[0].parts.len(), 2);
        assert_eq!(summary.totals.required.pass, 1);
        let reasons: Vec<String> = summary
            .processes
            .iter()
            .filter_map(|process| match &process.execution {
                Execution::Error { reason, .. } => Some(reason.clone()),
                Execution::Ok => None,
            })
            .collect();
        assert_eq!(reasons.len(), 4);
        assert!(reasons[0].contains("стенд не дошёл до конца"));
        assert!(reasons[1].contains("процесс снял внешний сторож"));
        assert!(reasons[2].contains("not_run у обязательной части"));
        assert!(reasons[3].contains("процесс запущен не через --launch"));
        assert!(matches!(summary.outcome, Outcome::Undetermined { .. }));
        let text = render_summary(&summary);
        assert!(text.contains("с ошибкой 4"));
        assert!(text.contains("D2/after-kill"));
        assert!(text.contains("итог пробы: не вынесен"));
        let path = write_summary(&run, &summary).unwrap();
        let written: SummaryFile = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(written, summary);
        fs::remove_dir_all(run).unwrap();
    }

    #[test]
    fn launch_and_result_must_agree() {
        let run = temp_dir("mismatch");
        let dir = process(&run, "a", Vec::new());
        let mut record = launch("smoke", Execution::Ok, false);
        record.variant = "other".to_owned();
        write_launch(&dir, &record).unwrap();
        let summary = aggregate(&run).unwrap();
        assert_eq!(summary.totals.execution_errors, 1);
        fs::remove_dir_all(&run).unwrap();
        assert!(aggregate(&run).is_err());
    }
}
