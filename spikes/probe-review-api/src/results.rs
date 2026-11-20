//! Схема `result.json` процесса, `run.json` и `done.json` каталога прогона, `summary.json` серии,
//! агрегирование матрицы возможностей и её таблица Markdown.
//!
//! Источник: `spikes/probe-update-lifecycle/src/results.rs` (разделение исполнения и выводов,
//! атомарная запись, чтение с видами ошибок). Клетки P0.5 заменены матрицей «строка × столбец»
//! плана P0.7, вход агрегатора — явный список каталогов одной серии и ожидаемые пары.
//!
//! Исполнение и вывод разделены. Ошибка исполнения — стенд не дошёл до конца или результат
//! повреждён; она роняет цель. `pass`, `fail`, `absent`, `not_run` — выводы. Исключение — столбец
//! `fake`: он проверяет наш клиент против нашего оракула, и его `fail` — ошибка в коде пробы,
//! то есть ошибка исполнения (ADR-0034).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::scenario::Scenario;

pub const SCHEMA: u32 = 1;
pub const RESULT_FILE: &str = "result.json";
pub const RUN_FILE: &str = "run.json";
pub const DONE_FILE: &str = "done.json";
pub const SUMMARY_FILE: &str = "summary.json";
pub const SUMMARY_MD: &str = "summary.md";

/// Строки матрицы возможностей (раздел плана «Матрица возможностей»): ID — имя сценария.
pub const MATRIX_ROWS: [&str; 18] = [
    "gh-local-create",
    "gh-pending-cycle",
    "gh-pending-foreign",
    "gh-anchors",
    "gh-viewed",
    "gl-draft-publish",
    "gl-foreign-draft",
    "gl-approve-sha",
    "gl-request-changes",
    "gl-anchors",
    "head-change",
    "journal-crash",
    "reconcile-ambiguous",
    "reconcile-foreign",
    "reconcile-deleted",
    "reconcile-paged",
    "partial",
    "capabilities",
];

/// Столбец матрицы: где наблюдалось утверждение. Имена безопасны для данных.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Fake,
    GithubCloud,
    GitlabCloud,
    GitlabCe,
    Ghes,
}

impl Column {
    pub const ALL: [Column; 5] = [
        Column::Fake,
        Column::GithubCloud,
        Column::GitlabCloud,
        Column::GitlabCe,
        Column::Ghes,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Column::Fake => "fake",
            Column::GithubCloud => "github_cloud",
            Column::GitlabCloud => "gitlab_cloud",
            Column::GitlabCe => "gitlab_ce",
            Column::Ghes => "ghes",
        }
    }

    pub fn from_name(name: &str) -> Option<Column> {
        Column::ALL.into_iter().find(|c| c.name() == name)
    }
}

/// Пара «столбец × сценарий», запись `столбец:сценарий`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pair {
    pub column: Column,
    pub scenario: String,
}

impl Pair {
    pub fn parse(text: &str) -> Result<Pair, String> {
        let (column, scenario) = text
            .split_once(':')
            .ok_or_else(|| format!("пара «{text}» не в виде столбец:сценарий"))?;
        let column =
            Column::from_name(column).ok_or_else(|| format!("неизвестный столбец «{column}»"))?;
        if scenario.is_empty() {
            return Err(format!("пара «{text}» без сценария"));
        }
        Ok(Pair {
            column,
            scenario: scenario.to_owned(),
        })
    }

    /// Список пар через пробелы или запятые; повтор пары — ошибка.
    pub fn parse_list(text: &str) -> Result<Vec<Pair>, String> {
        let mut pairs = Vec::new();
        let mut seen = BTreeSet::new();
        for item in text.split([' ', ',', '\n', '\t']).filter(|s| !s.is_empty()) {
            let pair = Pair::parse(item)?;
            if !seen.insert(pair.clone()) {
                return Err(format!("пара {pair} задана дважды"));
            }
            pairs.push(pair);
        }
        if pairs.is_empty() {
            return Err("пустой список пар".to_owned());
        }
        Ok(pairs)
    }

    /// Каталог процесса пары внутри каталога прогона.
    pub fn dir_name(&self) -> String {
        format!("{}-{}", self.scenario, self.column.name())
    }
}

impl fmt::Display for Pair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.column.name(), self.scenario)
    }
}

/// Итог клетки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    /// Отрицательный вывод.
    Fail,
    /// У проверенной роли или редакции хоста возможности нет — наблюдённое ограничение.
    Absent,
    /// Не наблюдалось: нет песочницы, токена или доступа; причина записана.
    NotRun,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::Absent => "absent",
            Status::NotRun => "not_run",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Execution {
    Ok,
    Error { reason: String },
}

impl Execution {
    pub fn is_ok(&self) -> bool {
        matches!(self, Execution::Ok)
    }

    pub fn error(reason: impl Into<String>) -> Execution {
        Execution::Error {
            reason: reason.into(),
        }
    }
}

/// Версия и тариф хоста; тариф — если виден токену.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

/// Итог процесса одной пары. Абсолютных путей и окружения процесса здесь нет: файл уходит
/// в `docs/probes/data/` после очистки.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultFile {
    pub schema: u32,
    pub series: String,
    /// SHA-256 бинаря пробы.
    pub build: String,
    pub scenario: String,
    pub column: Column,
    #[serde(default)]
    pub host: Host,
    pub execution: Execution,
    pub status: Status,
    /// Доказательство вывода или причина `not_run`.
    pub reason: String,
    /// Очищенный след вызовов.
    #[serde(default)]
    pub trace: Vec<Value>,
    /// Число вызовов по методам, например `POST /repos/{repo}/pulls/{n}/reviews`.
    #[serde(default)]
    pub calls: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_before: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_after: Option<Value>,
    /// Журнал после переоткрытия.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal: Option<Value>,
    /// Не убранные объекты живого хоста.
    #[serde(default)]
    pub leftovers: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl ResultFile {
    /// Заготовка до прогона: `not_run` с причиной «сценарий не вынес итог».
    pub fn new(series: &str, build: &str, scenario: &str, column: Column) -> ResultFile {
        ResultFile {
            schema: SCHEMA,
            series: series.to_owned(),
            build: build.to_owned(),
            scenario: scenario.to_owned(),
            column,
            host: Host::default(),
            execution: Execution::Ok,
            status: Status::NotRun,
            reason: "сценарий не вынес итог".to_owned(),
            trace: Vec::new(),
            calls: BTreeMap::new(),
            server_before: None,
            server_after: None,
            journal: None,
            leftovers: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Нарушения схемы без обращения к файлам.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.schema != SCHEMA {
            problems.push(format!("схема {}, ожидается {SCHEMA}", self.schema));
        }
        match Scenario::from_name(&self.scenario) {
            None => problems.push(format!("неизвестный сценарий «{}»", self.scenario)),
            Some(scenario) if !scenario.columns().contains(&self.column) => problems.push(format!(
                "сценарий {} не идёт в столбце {}",
                self.scenario,
                self.column.name()
            )),
            Some(_) => {}
        }
        if self.series.trim().is_empty() {
            problems.push("пустая серия".to_owned());
        }
        if self.build.trim().is_empty() {
            problems.push("пустой хеш сборки".to_owned());
        }
        if self.reason.trim().is_empty() {
            problems.push(format!("итог {} без причины", self.status.name()));
        }
        if self.status == Status::Absent
            && self
                .host
                .version
                .as_deref()
                .is_none_or(|v| v.trim().is_empty())
        {
            problems.push("absent без версии хоста".to_owned());
        }
        problems
    }
}

/// Уборка контейнеров прежних прогонов в начале вызова `run.sh`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerCleanup {
    /// `cleaned` — контейнеров с меткой не осталось; `not_observed` — docker недоступен.
    pub status: String,
    #[serde(default)]
    pub removed: u32,
    #[serde(default)]
    pub detail: String,
}

/// Пара, которую `run.sh` не запускал, с причиной: объяснённый пропуск, в матрице — `not_run`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub pair: String,
    pub reason: String,
}

/// `run.json` — пишет `run.sh` до первого процесса.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFile {
    pub schema: u32,
    pub series: String,
    pub build: String,
    /// `fake` или `live`.
    pub target: String,
    pub started_unix: u64,
    pub pairs: Vec<String>,
    #[serde(default)]
    pub skipped: Vec<Skipped>,
    pub docker: DockerCleanup,
}

/// `done.json` — пишет `run.sh` после последнего процесса; без него каталог прерван.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneFile {
    pub schema: u32,
    pub finished_unix: u64,
    pub errors: u32,
}

/// Пишет файл через временный и переименование: оборванная запись не оставляет полупустой JSON.
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

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    Missing,
    Corrupt(String),
    Io(String),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Missing => write!(f, "отсутствует"),
            ReadError::Corrupt(err) => write!(f, "повреждён: {err}"),
            ReadError::Io(err) => write!(f, "не прочитан: {err}"),
        }
    }
}

pub fn read_json<T: DeserializeOwned>(dir: &Path, name: &str) -> Result<T, ReadError> {
    let bytes = match fs::read(dir.join(name)) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(ReadError::Missing),
        Err(err) => return Err(ReadError::Io(err.to_string())),
    };
    serde_json::from_slice(&bytes).map_err(|err| ReadError::Corrupt(err.to_string()))
}

/// Клетка серии: итог пары и откуда он взят.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CellSummary {
    pub pair: String,
    pub column: Column,
    pub scenario: String,
    /// Каталог прогона (имя), из которого взят итог.
    pub run: String,
    pub execution: Execution,
    /// `None` — ошибка исполнения, вывода нет.
    pub status: Option<Status>,
    pub reason: String,
    #[serde(default)]
    pub host: Host,
}

/// Пара прерванного каталога, взятая из завершённого.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    pub pair: String,
    pub interrupted: String,
    pub completed: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub name: String,
    pub target: String,
    pub completed: bool,
    pub docker: DockerCleanup,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SummaryFile {
    pub schema: u32,
    pub series: String,
    pub build: String,
    pub runs: Vec<RunSummary>,
    pub expected: Vec<String>,
    pub cells: Vec<CellSummary>,
    pub replacements: Vec<Replacement>,
    /// Ошибки исполнения и агрегатора: непустой список роняет цель.
    pub errors: Vec<String>,
}

struct RunDir {
    name: String,
    path: PathBuf,
    run: RunFile,
    completed: bool,
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Собирает матрицу серии по явному списку каталогов прогона и ожидаемым парам.
///
/// Ошибки агрегатора: каталог чужой серии или сборки, необъяснённый пропуск пары, её дубль
/// в двух завершённых каталогах, неожиданная пара. Каталог без `done.json` прерван: его пары
/// берутся из последнего завершённого каталога, замена записывается; пара только из прерванного
/// каталога — пропуск. `Err` — только если каталог прогона не читается вовсе.
pub fn aggregate(dirs: &[PathBuf], expected: &[Pair]) -> Result<SummaryFile, String> {
    if dirs.is_empty() {
        return Err("нет каталогов прогона".to_owned());
    }
    let mut runs = Vec::new();
    for path in dirs {
        let name = dir_name(path);
        let run: RunFile = read_json(path, RUN_FILE)
            .map_err(|err| format!("{name}: {RUN_FILE} {err}: каталог не из run.sh"))?;
        let completed = match read_json::<DoneFile>(path, DONE_FILE) {
            Ok(_) => true,
            Err(ReadError::Missing) => false,
            Err(err) => return Err(format!("{name}: {DONE_FILE} {err}")),
        };
        runs.push(RunDir {
            name,
            path: path.clone(),
            run,
            completed,
        });
    }
    runs.sort_by(|a, b| (a.run.started_unix, &a.name).cmp(&(b.run.started_unix, &b.name)));

    let mut errors = Vec::new();
    let series = runs[0].run.series.clone();
    let build = runs[0].run.build.clone();
    let mut names = BTreeSet::new();
    for run in &runs {
        if !names.insert(run.name.clone()) {
            errors.push(format!("каталог {} задан дважды", run.name));
        }
        if run.run.series != series {
            errors.push(format!(
                "каталог {}: чужая серия {} (серия {series})",
                run.name, run.run.series
            ));
        }
        if run.run.build != build {
            errors.push(format!(
                "каталог {}: чужая сборка {} (сборка {build})",
                run.name, run.run.build
            ));
        }
    }

    let expected_set: BTreeSet<String> = expected.iter().map(ToString::to_string).collect();
    // Пара → каталоги, где она запускалась или объяснённо пропущена, в порядке старта.
    let mut sources: BTreeMap<String, Vec<(usize, Option<String>)>> = BTreeMap::new();
    for (index, run) in runs.iter().enumerate() {
        for pair in &run.run.pairs {
            if Pair::parse(pair).is_err() {
                errors.push(format!("каталог {}: неверная пара «{pair}»", run.name));
                continue;
            }
            sources.entry(pair.clone()).or_default().push((index, None));
        }
        for skipped in &run.run.skipped {
            sources
                .entry(skipped.pair.clone())
                .or_default()
                .push((index, Some(skipped.reason.clone())));
        }
    }
    for (pair, found) in &sources {
        if !expected_set.contains(pair) {
            let where_: Vec<&str> = found.iter().map(|(i, _)| runs[*i].name.as_str()).collect();
            errors.push(format!("неожиданная пара {pair} в {}", where_.join(", ")));
        }
    }

    let mut cells = Vec::new();
    let mut replacements = Vec::new();
    for pair in expected {
        let key = pair.to_string();
        let found = sources.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        let completed: Vec<&(usize, Option<String>)> =
            found.iter().filter(|(i, _)| runs[*i].completed).collect();
        let source = match completed.as_slice() {
            [] => {
                let interrupted: Vec<&str> =
                    found.iter().map(|(i, _)| runs[*i].name.as_str()).collect();
                if interrupted.is_empty() {
                    errors.push(format!("пара {key} пропущена"));
                } else {
                    errors.push(format!(
                        "пара {key} есть только в прерванных каталогах {}",
                        interrupted.join(", ")
                    ));
                }
                continue;
            }
            [one] => *one,
            many => {
                let where_: Vec<&str> = many.iter().map(|(i, _)| runs[*i].name.as_str()).collect();
                errors.push(format!(
                    "пара {key} — дубль в завершённых каталогах {}",
                    where_.join(", ")
                ));
                continue;
            }
        };
        let (index, skip_reason) = source;
        let run = &runs[*index];
        for (other, _) in found.iter().filter(|(i, _)| !runs[*i].completed) {
            replacements.push(Replacement {
                pair: key.clone(),
                interrupted: runs[*other].name.clone(),
                completed: run.name.clone(),
            });
        }
        let cell = match skip_reason {
            Some(reason) => CellSummary {
                pair: key.clone(),
                column: pair.column,
                scenario: pair.scenario.clone(),
                run: run.name.clone(),
                execution: Execution::Ok,
                status: Some(Status::NotRun),
                reason: reason.clone(),
                host: Host::default(),
            },
            None => read_cell(run, pair),
        };
        if let Execution::Error { reason } = &cell.execution {
            errors.push(format!("{key} ({}): ошибка исполнения: {reason}", run.name));
        }
        cells.push(cell);
    }

    Ok(SummaryFile {
        schema: SCHEMA,
        series,
        build,
        runs: runs
            .iter()
            .map(|run| RunSummary {
                name: run.name.clone(),
                target: run.run.target.clone(),
                completed: run.completed,
                docker: run.run.docker.clone(),
            })
            .collect(),
        expected: expected.iter().map(ToString::to_string).collect(),
        cells,
        replacements,
        errors,
    })
}

/// Итог пары из `result.json` её каталога; любое расхождение — ошибка исполнения пары.
fn read_cell(run: &RunDir, pair: &Pair) -> CellSummary {
    let mut cell = CellSummary {
        pair: pair.to_string(),
        column: pair.column,
        scenario: pair.scenario.clone(),
        run: run.name.clone(),
        execution: Execution::Ok,
        status: None,
        reason: String::new(),
        host: Host::default(),
    };
    let dir = run.path.join(pair.dir_name());
    let result: ResultFile = match read_json(&dir, RESULT_FILE) {
        Ok(result) => result,
        Err(err) => {
            cell.execution = Execution::error(format!("{RESULT_FILE} {err}"));
            return cell;
        }
    };
    let mut problems = result.problems();
    if result.series != run.run.series {
        problems.push(format!("чужая серия {}", result.series));
    }
    if result.build != run.run.build {
        problems.push(format!("чужая сборка {}", result.build));
    }
    if result.scenario != pair.scenario || result.column != pair.column {
        problems.push(format!(
            "итог пары {}:{} лежит в каталоге {}",
            result.column.name(),
            result.scenario,
            pair.dir_name()
        ));
    }
    cell.host = result.host.clone();
    cell.reason = result.reason.clone();
    if !problems.is_empty() {
        cell.execution =
            Execution::error(format!("{RESULT_FILE} повреждён: {}", problems.join("; ")));
        return cell;
    }
    if let Execution::Error { reason } = &result.execution {
        cell.execution = Execution::error(reason.clone());
        return cell;
    }
    if pair.column == Column::Fake && result.status != Status::Pass {
        cell.execution = Execution::error(format!(
            "{} в столбце fake — ошибка в коде пробы или стенда: {}",
            result.status.name(),
            result.reason
        ));
        return cell;
    }
    cell.status = Some(result.status);
    cell
}

fn cell_text(cell: &CellSummary) -> String {
    let text = match (&cell.execution, cell.status) {
        (Execution::Error { .. }, _) | (_, None) => "ошибка исполнения".to_owned(),
        (Execution::Ok, Some(Status::Pass)) => "pass".to_owned(),
        (Execution::Ok, Some(status)) => format!("{}: {}", status.name(), cell.reason),
    };
    let host = match (&cell.host.version, &cell.host.tier) {
        (Some(v), Some(t)) => format!(" ({v}, {t})"),
        (Some(v), None) => format!(" ({v})"),
        (None, Some(t)) => format!(" ({t})"),
        (None, None) => String::new(),
    };
    format!("{text}{host}").replace('|', "\\|")
}

/// Таблица матрицы по всем строкам и столбцам, затем самопроверки стенда, замены и ошибки.
pub fn render_markdown(summary: &SummaryFile) -> String {
    let lookup: BTreeMap<(&str, Column), &CellSummary> = summary
        .cells
        .iter()
        .map(|cell| ((cell.scenario.as_str(), cell.column), cell))
        .collect();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Серия `{}`, сборка `{}`.\n",
        summary.series, summary.build
    );
    let header: Vec<&str> = Column::ALL.iter().map(|c| c.name()).collect();
    let _ = writeln!(out, "| Строка | {} |", header.join(" | "));
    let _ = writeln!(out, "| --- |{}", " --- |".repeat(Column::ALL.len()));
    let stand: Vec<&str> = summary
        .cells
        .iter()
        .filter(|c| !MATRIX_ROWS.contains(&c.scenario.as_str()))
        .map(|c| c.scenario.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let row = |out: &mut String, scenario: &str| {
        let cells: Vec<String> = Column::ALL
            .iter()
            .map(|column| {
                lookup
                    .get(&(scenario, *column))
                    .map_or_else(|| "—".to_owned(), |cell| cell_text(cell))
            })
            .collect();
        let _ = writeln!(out, "| `{scenario}` | {} |", cells.join(" | "));
    };
    for scenario in MATRIX_ROWS {
        row(&mut out, scenario);
    }
    if !stand.is_empty() {
        let _ = writeln!(out, "\nСамопроверки стенда (в матрицу не входят):\n");
        let _ = writeln!(out, "| Сценарий | {} |", header.join(" | "));
        let _ = writeln!(out, "| --- |{}", " --- |".repeat(Column::ALL.len()));
        for scenario in stand {
            row(&mut out, scenario);
        }
    }
    let _ = writeln!(out, "\nКаталоги прогона:\n");
    for run in &summary.runs {
        let _ = writeln!(
            out,
            "- `{}` ({}, {}), уборка docker: {}{}",
            run.name,
            run.target,
            if run.completed {
                "завершён"
            } else {
                "прерван"
            },
            run.docker.status,
            if run.docker.detail.is_empty() {
                String::new()
            } else {
                format!(" — {}", run.docker.detail)
            }
        );
    }
    if !summary.replacements.is_empty() {
        let _ = writeln!(out, "\nЗамены:\n");
        for r in &summary.replacements {
            let _ = writeln!(
                out,
                "- `{}`: из прерванного `{}` взята из `{}`",
                r.pair, r.interrupted, r.completed
            );
        }
    }
    if !summary.errors.is_empty() {
        let _ = writeln!(out, "\nОшибки:\n");
        for err in &summary.errors {
            let _ = writeln!(out, "- {err}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), String>;

    fn pairs(text: &str) -> Vec<Pair> {
        Pair::parse_list(text).unwrap_or_default()
    }

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Result<Fixture, String> {
            Ok(Fixture {
                root: tempfile::tempdir().map_err(|e| e.to_string())?,
            })
        }

        /// Каталог прогона с `run.json`, итогами пар и `done.json`, если `completed`.
        fn run(
            &self,
            name: &str,
            started: u64,
            series: &str,
            build: &str,
            results: &[(&str, Status)],
            completed: bool,
        ) -> Result<PathBuf, String> {
            let dir = self.root.path().join(name);
            fs::create_dir(&dir).map_err(|e| e.to_string())?;
            let mut listed = Vec::new();
            for (pair, status) in results {
                let pair = Pair::parse(pair)?;
                listed.push(pair.to_string());
                let proc_dir = dir.join(pair.dir_name());
                fs::create_dir(&proc_dir).map_err(|e| e.to_string())?;
                let mut result = ResultFile::new(series, build, &pair.scenario, pair.column);
                result.status = *status;
                result.reason = "наблюдение".to_owned();
                write_json(&proc_dir, RESULT_FILE, &result)?;
            }
            let run = RunFile {
                schema: SCHEMA,
                series: series.to_owned(),
                build: build.to_owned(),
                target: "fake".to_owned(),
                started_unix: started,
                pairs: listed,
                skipped: Vec::new(),
                docker: DockerCleanup {
                    status: "not_observed".to_owned(),
                    removed: 0,
                    detail: String::new(),
                },
            };
            write_json(&dir, RUN_FILE, &run)?;
            if completed {
                let done = DoneFile {
                    schema: SCHEMA,
                    finished_unix: started + 1,
                    errors: 0,
                };
                write_json(&dir, DONE_FILE, &done)?;
            }
            Ok(dir)
        }
    }

    #[test]
    fn pair_parsing() {
        assert_eq!(
            Pair::parse("fake:smoke"),
            Ok(Pair {
                column: Column::Fake,
                scenario: "smoke".to_owned()
            })
        );
        assert!(Pair::parse("smoke").is_err());
        assert!(Pair::parse("nowhere:smoke").is_err());
        assert!(Pair::parse("fake:").is_err());
        assert!(Pair::parse_list("fake:smoke, fake:smoke").is_err());
        assert!(Pair::parse_list("  ").is_err());
        assert_eq!(pairs("fake:smoke").len(), 1);
    }

    #[test]
    fn result_problems() {
        let mut result = ResultFile::new("s", "b", "smoke", Column::Fake);
        assert!(result.problems().is_empty());
        result.status = Status::Absent;
        assert!(result.problems().iter().any(|p| p.contains("версии")));
        result.host.version = Some("17.0.0".to_owned());
        assert!(result.problems().is_empty());
        result.reason = " ".to_owned();
        assert!(!result.problems().is_empty());
        let wrong_column = ResultFile::new("s", "b", "smoke", Column::GitlabCe);
        assert!(!wrong_column.problems().is_empty());
        let unknown = ResultFile::new("s", "b", "nope", Column::Fake);
        assert!(!unknown.problems().is_empty());
        let no_series = ResultFile::new("", "b", "smoke", Column::Fake);
        assert!(!no_series.problems().is_empty());
    }

    #[test]
    fn single_completed_run() -> TestResult {
        let fx = Fixture::new()?;
        let dir = fx.run("r1", 10, "s1", "b1", &[("fake:smoke", Status::Pass)], true)?;
        let summary = aggregate(&[dir], &pairs("fake:smoke"))?;
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert_eq!(summary.cells.len(), 1);
        assert_eq!(summary.cells[0].status, Some(Status::Pass));
        assert_eq!(summary.series, "s1");
        let md = render_markdown(&summary);
        for row in MATRIX_ROWS {
            assert!(md.contains(&format!("`{row}`")), "нет строки {row}");
        }
        for column in Column::ALL {
            assert!(md.contains(column.name()));
        }
        assert!(md.contains("`smoke`"));
        Ok(())
    }

    #[test]
    fn foreign_series_and_build_are_errors() -> TestResult {
        let fx = Fixture::new()?;
        let a = fx.run("a", 1, "s1", "b1", &[("fake:smoke", Status::Pass)], true)?;
        let b = fx.run("b", 2, "s2", "b1", &[], true)?;
        let c = fx.run("c", 3, "s1", "b2", &[], true)?;
        let summary = aggregate(&[a, b, c], &pairs("fake:smoke"))?;
        assert!(summary.errors.iter().any(|e| e.contains("чужая серия")));
        assert!(summary.errors.iter().any(|e| e.contains("чужая сборка")));
        Ok(())
    }

    #[test]
    fn missing_and_unexpected_pairs_are_errors() -> TestResult {
        let fx = Fixture::new()?;
        let a = fx.run("a", 1, "s", "b", &[("fake:smoke", Status::Pass)], true)?;
        let summary = aggregate(
            std::slice::from_ref(&a),
            &pairs("fake:smoke gitlab_ce:capabilities"),
        )?;
        assert!(
            summary
                .errors
                .iter()
                .any(|e| e.contains("gitlab_ce:capabilities пропущена"))
        );
        let other = Fixture::new()?;
        let b = other.run("b", 1, "s", "b", &[("fake:smoke", Status::Pass)], true)?;
        let summary = aggregate(&[b], &pairs("gitlab_ce:capabilities"))?;
        assert!(
            summary
                .errors
                .iter()
                .any(|e| e.contains("неожиданная пара"))
        );
        Ok(())
    }

    #[test]
    fn duplicate_in_two_completed_runs_is_error() -> TestResult {
        let fx = Fixture::new()?;
        let a = fx.run("a", 1, "s", "b", &[("fake:smoke", Status::Pass)], true)?;
        let b = fx.run("b", 2, "s", "b", &[("fake:smoke", Status::Pass)], true)?;
        let summary = aggregate(&[a, b], &pairs("fake:smoke"))?;
        assert!(summary.errors.iter().any(|e| e.contains("дубль")));
        assert!(summary.cells.is_empty());
        Ok(())
    }

    #[test]
    fn interrupted_run_is_replaced_by_completed() -> TestResult {
        let fx = Fixture::new()?;
        let a = fx.run("a", 1, "s", "b", &[("fake:smoke", Status::Pass)], false)?;
        let b = fx.run("b", 2, "s", "b", &[("fake:smoke", Status::Pass)], true)?;
        let summary = aggregate(&[b, a], &pairs("fake:smoke"))?;
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert_eq!(summary.cells[0].run, "b");
        assert_eq!(
            summary.replacements,
            vec![Replacement {
                pair: "fake:smoke".to_owned(),
                interrupted: "a".to_owned(),
                completed: "b".to_owned(),
            }]
        );
        assert!(render_markdown(&summary).contains("Замены"));
        Ok(())
    }

    #[test]
    fn pair_only_in_interrupted_run_is_missing() -> TestResult {
        let fx = Fixture::new()?;
        let a = fx.run("a", 1, "s", "b", &[("fake:smoke", Status::Pass)], false)?;
        let summary = aggregate(&[a], &pairs("fake:smoke"))?;
        assert!(summary.errors.iter().any(|e| e.contains("прерванных")));
        Ok(())
    }

    #[test]
    fn skipped_pair_is_explained_not_run() -> TestResult {
        let fx = Fixture::new()?;
        let dir = fx.run("a", 1, "s", "b", &[("fake:smoke", Status::Pass)], true)?;
        let mut run: RunFile = read_json(&dir, RUN_FILE).map_err(|e| e.to_string())?;
        run.skipped.push(Skipped {
            pair: "gitlab_ce:capabilities".to_owned(),
            reason: "docker недоступен".to_owned(),
        });
        write_json(&dir, RUN_FILE, &run)?;
        let summary = aggregate(&[dir], &pairs("fake:smoke gitlab_ce:capabilities"))?;
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        let cell = summary
            .cells
            .iter()
            .find(|c| c.column == Column::GitlabCe)
            .ok_or("нет клетки gitlab_ce")?;
        assert_eq!(cell.status, Some(Status::NotRun));
        assert_eq!(cell.reason, "docker недоступен");
        Ok(())
    }

    #[test]
    fn fake_fail_is_execution_error() -> TestResult {
        for status in [Status::Fail, Status::NotRun] {
            let fx = Fixture::new()?;
            let dir = fx.run("a", 1, "s", "b", &[("fake:smoke", status)], true)?;
            let summary = aggregate(&[dir], &pairs("fake:smoke"))?;
            assert!(
                summary
                    .errors
                    .iter()
                    .any(|e| e.contains("ошибка в коде пробы")),
                "{status:?}: {:?}",
                summary.errors
            );
            assert_eq!(summary.cells[0].status, None);
        }
        Ok(())
    }

    #[test]
    fn missing_or_foreign_result_is_execution_error() -> TestResult {
        let fx = Fixture::new()?;
        let dir = fx.run("a", 1, "s", "b", &[("fake:smoke", Status::Pass)], true)?;
        let proc_dir = dir.join("smoke-fake");
        let mut result: ResultFile =
            read_json(&proc_dir, RESULT_FILE).map_err(|e| e.to_string())?;
        result.build = "other".to_owned();
        write_json(&proc_dir, RESULT_FILE, &result)?;
        let summary = aggregate(std::slice::from_ref(&dir), &pairs("fake:smoke"))?;
        assert!(summary.errors.iter().any(|e| e.contains("чужая сборка")));
        fs::remove_file(proc_dir.join(RESULT_FILE)).map_err(|e| e.to_string())?;
        let summary = aggregate(&[dir], &pairs("fake:smoke"))?;
        assert!(summary.errors.iter().any(|e| e.contains("отсутствует")));
        Ok(())
    }

    #[test]
    fn directory_without_run_file_is_refused() -> TestResult {
        let fx = Fixture::new()?;
        let dir = fx.root.path().join("empty");
        fs::create_dir(&dir).map_err(|e| e.to_string())?;
        assert!(aggregate(&[dir], &pairs("fake:smoke")).is_err());
        Ok(())
    }
}
