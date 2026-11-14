//! Схема `result.json` процесса сценария, `launch.json` его запуска и `summary.json` прогона,
//! итог сценария и агрегирование. Модуль не зависит от AppKit.
//!
//! Исполнение (`execution`) и выводы (`findings`) разделены: ошибка исполнения значит, что стенд
//! не дошёл до конца, а `fail` и `pending-manual` — законченные наблюдения. Вывод бывает
//! обязательным или исследовательским: обязательный `pending-manual` оставляет план незавершённым.

use std::collections::BTreeMap;
use std::fmt::{Display, Write as _};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::stats::{self, Summary};

pub const SCHEMA: u32 = 1;
pub const RESULT_FILE: &str = "result.json";
pub const SUMMARY_FILE: &str = "summary.json";
/// Итог `--launch`: маршрут, окружение, внешний сторож и оставшиеся процессы.
pub const LAUNCH_FILE: &str = "launch.json";
/// След сторожа: процесс, завершённый сторожем, `result.json` не пишет.
pub const WATCHDOG_FILE: &str = "watchdog.txt";

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Pass,
    Fail,
    PendingManual,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::PendingManual => "pending-manual",
        }
    }
}

/// Обязательная проверка или исследование.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub name: String,
    pub requirement: Requirement,
    pub verdict: Verdict,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
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

/// Прочитанное состояние: строка или структура.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub key: String,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub unit: String,
    pub samples: Vec<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_error: Option<String>,
}

impl Metric {
    pub fn new(name: impl Into<String>, unit: impl Into<String>, samples: Vec<f64>) -> Metric {
        let (summary, summary_error) = match stats::summarize(&samples) {
            Ok(summary) => (Some(summary), None),
            Err(err) => (None, Some(err.to_string())),
        };
        Metric {
            name: name.into(),
            unit: unit.into(),
            samples,
            summary,
            summary_error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepStatus {
    Ready,
    Timeout,
    ActionError,
}

/// Шаг сценария: когда выполнилось его ожидание и что стенд прочитал в этот момент.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepRecord {
    pub index: usize,
    pub name: String,
    pub status: StepStatus,
    pub waited_ms: f64,
    /// Время от установки стенда.
    pub at_ms: f64,
    pub state: String,
}

/// Отметки синтетического события в наносекундах `CLOCK_UPTIME_RAW`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub number: u64,
    pub label: String,
    pub window: isize,
    /// Состояние AppKit перед отправкой.
    pub before: String,
    pub t_post_ns: u64,
    pub dispatch_start_ns: Option<u64>,
    pub send_return_ns: Option<u64>,
    /// Конец первого прохода отображения после возврата `sendEvent`.
    pub display_pass_ns: Option<u64>,
    pub undispatched: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotKind {
    /// `screencapture -l`: окно с тенью.
    Window,
    /// `screencapture -R`: прямоугольник экрана с окном, палитрой и тем, что под ними.
    Composition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotStatus {
    Ok,
    PendingManual,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub kind: SnapshotKind,
    pub status: SnapshotStatus,
    /// Имя файла внутри `--out`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect_points: Option<Rect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixels: Option<Size>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveObjects {
    pub class: String,
    pub created: usize,
    pub released: usize,
    pub live: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Diagnostics {
    #[serde(default)]
    pub trace: Vec<String>,
    #[serde(default)]
    pub appkit_internal_events: Vec<String>,
    /// Счётчики после очистки таблицы событий; блоки, наблюдатели и системные объекты не
    /// охвачены.
    #[serde(default)]
    pub live_objects: Vec<LiveObjects>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hardware {
    pub model: Option<String>,
    pub cpu: Option<String>,
    pub memory_bytes: Option<u64>,
}

/// Ключи `Info.plist` бандла пробы, которые `run.sh` заполняет перед подписью.
pub const INFO_COMMIT: &str = "ShellioProbeCommit";
pub const INFO_RUSTC: &str = "ShellioProbeRustc";
pub const INFO_OBJC2: &str = "ShellioProbeObjc2";
pub const INFO_GHOSTTY_REV: &str = "ShellioGhosttyRev";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub commit: Option<String>,
    pub rustc: Option<String>,
    pub objc2: Option<String>,
    /// Версия ghostty из штампа набора, с которым слинкована проба.
    pub ghostty_version: String,
    pub ghostty_rev: Option<String>,
    pub profile: String,
    pub probe_version: String,
}

impl Build {
    /// Сведения о сборке лежат в подписанном `Info.plist` бандла: окружение процесса пробы
    /// собирается по белому списку и их не несёт. Сама проба git и rustc не вызывает.
    pub fn from_info(lookup: impl Fn(&str) -> Option<String>) -> Build {
        let value = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        Build {
            commit: value(INFO_COMMIT),
            rustc: value(INFO_RUSTC),
            objc2: value(INFO_OBJC2),
            ghostty_version: shellio_sys_ghostty::GHOSTTY_VERSION.to_owned(),
            ghostty_rev: value(INFO_GHOSTTY_REV),
            profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_owned(),
            probe_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub builtin: bool,
    /// Какой дисплей выбрал стенд: `builtin` или `main`.
    pub chosen: String,
    pub frame: Rect,
    pub scale: f64,
    pub max_fps: isize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub frame: Rect,
    pub content_points: Size,
    pub bitmap_pixels: Size,
    pub backing_scale: f64,
}

/// Бандл, из которого запущен процесс, и его подпись.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleInfo {
    pub path: Option<String>,
    pub identifier: Option<String>,
    pub executable: Option<String>,
    /// Итог проверки подписи запущенного кода требованием пробы.
    pub signature: String,
}

/// Процесс пробы: маршрут запуска и фактическое окружение на входе в `main`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub route: String,
    pub nonce: Option<String>,
    pub env: BTreeMap<String, String>,
    /// Переменные вне белого списка, которые не запрещены.
    pub env_extra: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub os: String,
    pub hardware: Hardware,
    pub build: Build,
    pub process: ProcessInfo,
    pub bundle: Option<BundleInfo>,
    pub display: Option<DisplayInfo>,
    pub window: Option<WindowInfo>,
    pub input_source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultFile {
    pub schema: u32,
    pub scenario: String,
    pub variant: String,
    pub environment: Environment,
    pub execution: Execution,
    pub percentile_method: String,
    pub steps: Vec<StepRecord>,
    pub events: Vec<EventRecord>,
    pub observations: Vec<Observation>,
    pub metrics: Vec<Metric>,
    pub findings: Vec<Finding>,
    pub snapshots: Vec<Snapshot>,
    pub diagnostics: Diagnostics,
}

/// Итог сценария, который собирают стенд и сценарий.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub observations: Vec<Observation>,
    pub findings: Vec<Finding>,
    pub metrics: Vec<Metric>,
    pub snapshots: Vec<Snapshot>,
    pub steps: Vec<StepRecord>,
    pub events: Vec<EventRecord>,
    pub diagnostics: Diagnostics,
    pub bundle: Option<BundleInfo>,
    pub display: Option<DisplayInfo>,
    pub window: Option<WindowInfo>,
    pub input_source: Option<String>,
}

impl Report {
    pub fn push(&mut self, key: &str, value: impl Display) {
        self.observations.push(Observation {
            key: key.to_owned(),
            value: Value::String(value.to_string()),
        });
    }

    pub fn metric(&mut self, name: &str, unit: &str, samples: Vec<f64>) {
        self.metrics.push(Metric::new(name, unit, samples));
    }

    /// Прочитанное состояние как структура JSON.
    pub fn push_json(&mut self, key: &str, value: &impl Serialize) {
        let value = serde_json::to_value(value)
            .unwrap_or_else(|err| Value::String(format!("не сериализуется: {err}")));
        self.observations.push(Observation {
            key: key.to_owned(),
            value,
        });
    }

    pub fn finding(
        &mut self,
        name: &str,
        requirement: Requirement,
        verdict: Verdict,
        reason: impl Into<String>,
    ) {
        self.findings.push(Finding {
            name: name.to_owned(),
            requirement,
            verdict,
            reason: reason.into(),
        });
    }
}

impl ResultFile {
    /// Собирает `result.json` из постоянной части окружения и итога сценария.
    pub fn new(
        scenario: &str,
        variant: &str,
        mut environment: Environment,
        execution: Execution,
        report: Report,
    ) -> ResultFile {
        environment.bundle = report.bundle.or(environment.bundle);
        environment.display = report.display.or(environment.display);
        environment.window = report.window.or(environment.window);
        environment.input_source = report.input_source.or(environment.input_source);
        ResultFile {
            schema: SCHEMA,
            scenario: scenario.to_owned(),
            variant: variant.to_owned(),
            environment,
            execution,
            percentile_method: stats::METHOD.to_owned(),
            steps: report.steps,
            events: report.events,
            observations: report.observations,
            metrics: report.metrics,
            findings: report.findings,
            snapshots: report.snapshots,
            diagnostics: report.diagnostics,
        }
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

/// Итог `--launch` одного процесса сценария.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub schema: u32,
    pub route: String,
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub nonce: String,
    pub state_dir: String,
    /// Отметка `CLOCK_UPTIME_RAW` непосредственно перед запуском процесса или `open`.
    pub spawn_uptime_ns: u64,
    pub exit_uptime_ns: Option<u64>,
    pub exit: Option<ExitRecord>,
    pub deadline_s: f64,
    pub watchdog_fired: bool,
    /// Процессы с nonce, которые снял внешний сторож.
    pub killed: Vec<ProcessRecord>,
    /// Процессы с nonce, оставшиеся после выхода процесса пробы.
    pub leftovers: Vec<ProcessRecord>,
    /// Содержимое временного `SHELLIO_STATE_DIR` после выхода.
    pub state_dir_leftovers: Vec<String>,
    pub result_present: bool,
    pub execution: Execution,
}

/// Пишет файл через временный и переименование, чтобы оборванная запись не оставила
/// полупустой JSON под окончательным именем.
fn write_json(dir: &Path, name: &str, value: &impl Serialize) -> Result<PathBuf, String> {
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub processes: usize,
    pub execution_ok: usize,
    pub execution_errors: usize,
    pub pass: usize,
    pub fail: usize,
    pub pending_manual: usize,
    /// Обязательные проверки без результата: план с ними незавершён.
    pub required_pending_manual: usize,
    pub snapshots_pending_manual: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricSummary {
    pub name: String,
    pub unit: String,
    pub summary: Option<Summary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotBrief {
    pub name: String,
    pub kind: SnapshotKind,
    pub status: SnapshotStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessSummary {
    /// Каталог процесса внутри прогона, он же `--out` процесса.
    pub dir: String,
    pub scenario: String,
    pub variant: String,
    pub route: Option<String>,
    pub execution: Execution,
    pub findings: Vec<Finding>,
    pub metrics: Vec<MetricSummary>,
    pub snapshots: Vec<SnapshotBrief>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFinding {
    pub scenario: String,
    pub variant: String,
    pub name: String,
    pub requirement: Requirement,
    pub verdict: Verdict,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SummaryFile {
    pub schema: u32,
    pub run_dir: String,
    pub percentile_method: String,
    pub totals: Totals,
    pub processes: Vec<ProcessSummary>,
    /// Сводка выводов всех процессов отдельно от исполнения.
    pub findings: Vec<RunFinding>,
}

/// Сценарий и вариант по имени каталога `<сценарий>-<вариант>` или `<сценарий>-<вариант>-<NN>`
/// у повторных запусков, который даёт `run.sh`. Имя варианта дефиса не содержит.
pub fn split_dir_name(name: &str) -> (String, String) {
    let name = match name.rsplit_once('-') {
        Some((head, run))
            if head.contains('-') && !run.is_empty() && run.bytes().all(|b| b.is_ascii_digit()) =>
        {
            head
        }
        _ => name,
    };
    match name.rsplit_once('-') {
        Some((scenario, variant)) if !scenario.is_empty() && !variant.is_empty() => {
            (scenario.to_owned(), variant.to_owned())
        }
        _ => (name.to_owned(), String::new()),
    }
}

/// Причина из следа сторожа, если процесс завершил он.
fn watchdog_reason(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join(WATCHDOG_FILE)).ok()?;
    let reason = text
        .lines()
        .find_map(|line| line.strip_prefix("reason="))
        .unwrap_or("сторож завершил процесс");
    Some(format!("{reason}, след в {WATCHDOG_FILE}"))
}

fn process_summary(dir: &Path, name: String) -> ProcessSummary {
    let (scenario, variant) = split_dir_name(&name);
    let mut summary = ProcessSummary {
        dir: name,
        scenario,
        variant,
        route: None,
        execution: Execution::Ok,
        findings: Vec::new(),
        metrics: Vec::new(),
        snapshots: Vec::new(),
    };
    let mut reasons = Vec::new();
    match read_result(dir) {
        Ok(result) => {
            summary.scenario = result.scenario;
            summary.variant = result.variant;
            summary.route = Some(result.environment.process.route);
            if let Execution::Error { reason, .. } = result.execution {
                reasons.push(reason);
            }
            summary.findings = result.findings;
            summary.metrics = result
                .metrics
                .into_iter()
                .map(|metric| MetricSummary {
                    name: metric.name,
                    unit: metric.unit,
                    summary: metric.summary,
                })
                .collect();
            summary.snapshots = result
                .snapshots
                .into_iter()
                .map(|snapshot| SnapshotBrief {
                    name: snapshot.name,
                    kind: snapshot.kind,
                    status: snapshot.status,
                    reason: snapshot.reason,
                })
                .collect();
        }
        Err(ReadError::Missing) => reasons.push(match watchdog_reason(dir) {
            Some(reason) => format!("{RESULT_FILE} отсутствует: {reason}"),
            None => format!("{RESULT_FILE} отсутствует"),
        }),
        Err(ReadError::Corrupt(err)) => reasons.push(format!("{RESULT_FILE} повреждён: {err}")),
        Err(ReadError::Io(err)) => reasons.push(format!("{RESULT_FILE} не прочитан: {err}")),
    }
    match read_launch(dir) {
        Ok(launch) => {
            summary.route = Some(launch.route);
            if let Execution::Error { reason, .. } = launch.execution {
                reasons.push(format!("запуск: {reason}"));
            }
        }
        Err(ReadError::Missing) => reasons.push(format!(
            "{LAUNCH_FILE} отсутствует: процесс запущен не через --launch"
        )),
        Err(ReadError::Corrupt(err)) => reasons.push(format!("{LAUNCH_FILE} повреждён: {err}")),
        Err(ReadError::Io(err)) => reasons.push(format!("{LAUNCH_FILE} не прочитан: {err}")),
    }
    if !reasons.is_empty() {
        summary.execution = Execution::error(reasons.join("; "));
    }
    summary
}

/// Собирает итоги всех процессов прогона: каждый подкаталог — один процесс. Отсутствующий или
/// повреждённый `result.json` или `launch.json` — ошибка исполнения этого процесса, а не
/// агрегирования.
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
    dirs.sort();

    let mut totals = Totals::default();
    let mut processes = Vec::new();
    let mut findings = Vec::new();
    for (name, path) in dirs {
        let process = process_summary(&path, name);
        totals.processes += 1;
        if process.execution.is_ok() {
            totals.execution_ok += 1;
        } else {
            totals.execution_errors += 1;
        }
        for finding in &process.findings {
            match finding.verdict {
                Verdict::Pass => totals.pass += 1,
                Verdict::Fail => totals.fail += 1,
                Verdict::PendingManual => {
                    totals.pending_manual += 1;
                    if finding.requirement == Requirement::Required {
                        totals.required_pending_manual += 1;
                    }
                }
            }
            findings.push(RunFinding {
                scenario: process.scenario.clone(),
                variant: process.variant.clone(),
                name: finding.name.clone(),
                requirement: finding.requirement,
                verdict: finding.verdict,
                reason: finding.reason.clone(),
            });
        }
        totals.snapshots_pending_manual += process
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.status == SnapshotStatus::PendingManual)
            .count();
        processes.push(process);
    }
    Ok(SummaryFile {
        schema: SCHEMA,
        run_dir: run_dir.display().to_string(),
        percentile_method: stats::METHOD.to_owned(),
        totals,
        processes,
        findings,
    })
}

pub fn write_summary(run_dir: &Path, summary: &SummaryFile) -> Result<PathBuf, String> {
    write_json(run_dir, SUMMARY_FILE, summary)
}

/// Текстовая сводка: сначала исполнение, затем выводы отдельным списком.
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
        "выводы: pass {}, fail {}, pending-manual {} (обязательных {}); снимков pending-manual {}",
        totals.pass,
        totals.fail,
        totals.pending_manual,
        totals.required_pending_manual,
        totals.snapshots_pending_manual
    );
    for finding in &summary.findings {
        let _ = writeln!(
            text,
            "  {:<14} {:<8} {} {} {}: {}",
            finding.verdict.name(),
            finding.requirement.name(),
            finding.scenario,
            finding.variant,
            finding.name,
            finding.reason
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "probe-ghostty-surface-results-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn environment() -> Environment {
        Environment {
            os: "Version 27.0.1 (Build 26A434)".to_owned(),
            hardware: Hardware {
                model: Some("Mac15,6".to_owned()),
                cpu: Some("Apple M3 Pro".to_owned()),
                memory_bytes: Some(18 << 30),
            },
            build: Build::from_info(|name| match name {
                INFO_COMMIT => Some("59dd8bd".to_owned()),
                INFO_RUSTC => Some("rustc 1.99.1".to_owned()),
                _ => None,
            }),
            process: ProcessInfo {
                pid: 4242,
                route: "direct".to_owned(),
                nonce: Some("0123abcd".to_owned()),
                env: BTreeMap::from([("LANG".to_owned(), "en_US.UTF-8".to_owned())]),
                env_extra: Vec::new(),
            },
            bundle: None,
            display: None,
            window: None,
            input_source: None,
        }
    }

    fn finding(name: &str, requirement: Requirement, verdict: Verdict) -> Finding {
        Finding {
            name: name.to_owned(),
            requirement,
            verdict,
            reason: format!("{name}: причина"),
        }
    }

    fn sample(scenario: &str, execution: Execution, report: Report) -> ResultFile {
        ResultFile::new(scenario, "base", environment(), execution, report)
    }

    fn error_sample() -> ResultFile {
        let mut report = Report::default();
        report.push("wait_ms", "5000.0");
        report
            .diagnostics
            .trace
            .push("0.100ms post #1 keyDown Return".to_owned());
        sample(
            "window",
            Execution::Error {
                reason: "нет активности и key window за 5000 мс".to_owned(),
                last_state: Some("isActive=false keyWindow=none".to_owned()),
            },
            report,
        )
    }

    fn negative_sample() -> ResultFile {
        let mut report = Report::default();
        report.findings.push(finding(
            "chord_legacy",
            Requirement::Required,
            Verdict::Fail,
        ));
        report
            .findings
            .push(finding("latency_d", Requirement::Research, Verdict::Pass));
        report.metric("key_dispatch", "ms", vec![0.7]);
        report.steps.push(StepRecord {
            index: 0,
            name: "key window".to_owned(),
            status: StepStatus::Ready,
            waited_ms: 12.5,
            at_ms: 40.0,
            state: "isActive=true".to_owned(),
        });
        report.events.push(EventRecord {
            number: 1,
            label: "keyDown Return".to_owned(),
            window: 7,
            before: "isActive=true".to_owned(),
            t_post_ns: 1_000_000,
            dispatch_start_ns: Some(1_500_000),
            send_return_ns: Some(1_700_000),
            display_pass_ns: Some(2_000_000),
            undispatched: false,
        });
        sample("chord", Execution::Ok, report)
    }

    fn pending_sample() -> ResultFile {
        let mut report = Report::default();
        report.findings.push(finding(
            "palette_composition",
            Requirement::Required,
            Verdict::PendingManual,
        ));
        report.findings.push(finding(
            "latency_photon",
            Requirement::Research,
            Verdict::PendingManual,
        ));
        report.snapshots.push(Snapshot {
            name: "window".to_owned(),
            kind: SnapshotKind::Window,
            status: SnapshotStatus::PendingManual,
            file: None,
            reason: Some("нет разрешения Screen Recording".to_owned()),
            rect_points: None,
            pixels: None,
        });
        sample("palette", Execution::Ok, report)
    }

    fn launch_record(execution: Execution) -> LaunchRecord {
        LaunchRecord {
            schema: SCHEMA,
            route: "direct".to_owned(),
            program: "/b/probe-ghostty-surface.app/Contents/MacOS/probe-ghostty-surface".to_owned(),
            args: vec!["--scenario".to_owned(), "window".to_owned()],
            env: BTreeMap::from([("HOME".to_owned(), "/Users/test".to_owned())]),
            nonce: "0123abcd".to_owned(),
            state_dir: "/tmp/shellio-probe-state.0123abcd".to_owned(),
            spawn_uptime_ns: 10,
            exit_uptime_ns: Some(20),
            exit: Some(ExitRecord {
                code: Some(0),
                signal: None,
            }),
            deadline_s: 30.0,
            watchdog_fired: false,
            killed: Vec::new(),
            leftovers: Vec::new(),
            state_dir_leftovers: Vec::new(),
            result_present: true,
            execution,
        }
    }

    #[test]
    fn result_and_launch_round_trip() {
        let dir = temp_dir("round-trip");
        for result in [error_sample(), negative_sample(), pending_sample()] {
            write_result(&dir, &result).unwrap();
            assert_eq!(read_result(&dir), Ok(result));
        }
        let launch = launch_record(Execution::error("внешний сторож"));
        write_launch(&dir, &launch).unwrap();
        assert_eq!(read_launch(&dir), Ok(launch));
        assert!(!dir.join(format!(".{RESULT_FILE}.partial")).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn findings_carry_requirement_and_stay_apart_from_execution() {
        let json = serde_json::to_value(error_sample()).unwrap();
        assert_eq!(json["execution"]["status"], "error");
        assert_eq!(json["findings"], Value::Array(Vec::new()));
        assert_eq!(json["environment"]["process"]["route"], "direct");
        assert_eq!(json["environment"]["process"]["env"]["LANG"], "en_US.UTF-8");

        let json = serde_json::to_value(negative_sample()).unwrap();
        assert_eq!(json["execution"], serde_json::json!({ "status": "ok" }));
        assert_eq!(json["findings"][0]["verdict"], "fail");
        assert_eq!(json["findings"][0]["requirement"], "required");
        assert_eq!(json["findings"][1]["requirement"], "research");
        assert_eq!(json["events"][0]["t_post_ns"], 1_000_000);
        assert_eq!(json["metrics"][0]["summary"]["p95"], 0.7);
        assert_eq!(json["percentile_method"], "nearest-rank");
    }

    #[test]
    fn empty_metric_keeps_error_instead_of_summary() {
        let metric = Metric::new("x", "ms", Vec::new());
        assert_eq!(metric.summary, None);
        assert_eq!(
            metric.summary_error.as_deref(),
            Some("пустой набор замеров")
        );
    }

    #[test]
    fn report_environment_overrides_static_part() {
        let report = Report {
            input_source: Some("com.apple.keylayout.ABC".to_owned()),
            bundle: Some(BundleInfo {
                path: Some("/b/p.app".to_owned()),
                identifier: Some("io.github.dsbasko.shellio.probe.ghostty-surface".to_owned()),
                executable: None,
                signature: "valid".to_owned(),
            }),
            ..Report::default()
        };
        let result = sample("window", Execution::Ok, report);
        assert_eq!(
            result.environment.input_source.as_deref(),
            Some("com.apple.keylayout.ABC")
        );
        assert_eq!(
            result.environment.bundle.map(|bundle| bundle.signature),
            Some("valid".to_owned())
        );
        assert_eq!(result.environment.build.commit.as_deref(), Some("59dd8bd"));
        assert_eq!(result.environment.build.objc2, None);
        assert_eq!(
            result.environment.build.ghostty_version,
            shellio_sys_ghostty::GHOSTTY_VERSION
        );
    }

    #[test]
    fn build_ignores_blank_values() {
        let build = Build::from_info(|_| Some("  ".to_owned()));
        assert_eq!(
            (build.commit, build.rustc, build.objc2, build.ghostty_rev),
            (None, None, None, None)
        );
    }

    #[test]
    fn missing_and_corrupt_files_are_reported() {
        let dir = temp_dir("missing");
        assert_eq!(read_result(&dir), Err(ReadError::Missing));
        assert_eq!(read_launch(&dir), Err(ReadError::Missing));
        fs::write(dir.join(RESULT_FILE), b"{\"schema\": 1,").unwrap();
        assert!(matches!(read_result(&dir), Err(ReadError::Corrupt(_))));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dir_names_split_at_last_dash() {
        for (name, scenario, variant) in [
            ("window-base", "window", "base"),
            ("surface-env-base", "surface-env", "base"),
            ("startup-ls-base-01", "startup-ls", "base"),
            ("startup-base-21", "startup", "base"),
            ("gpu-calibration-base", "gpu-calibration", "base"),
            ("teardown-scribble", "teardown", "scribble"),
            ("window-07", "window", "07"),
            ("odd", "odd", ""),
            ("odd-", "odd-", ""),
        ] {
            assert_eq!(
                split_dir_name(name),
                (scenario.to_owned(), variant.to_owned()),
                "{name}"
            );
        }
    }

    #[test]
    fn aggregate_run() {
        let run = temp_dir("aggregate");
        let process = |name: &str| {
            let dir = run.join(name);
            fs::create_dir_all(&dir).unwrap();
            dir
        };
        let ok_launch = launch_record(Execution::Ok);

        let failed = process("window-base");
        write_result(&failed, &error_sample()).unwrap();
        write_launch(&failed, &ok_launch).unwrap();

        let negative = process("chord-base");
        write_result(&negative, &negative_sample()).unwrap();
        write_launch(&negative, &ok_launch).unwrap();

        let pending = process("palette-base");
        write_result(&pending, &pending_sample()).unwrap();
        write_launch(&pending, &ok_launch).unwrap();

        let watchdog = process("input-base");
        fs::write(
            watchdog.join(WATCHDOG_FILE),
            "scenario=input\nexecution=error\nreason=сторож: срок 40 с истёк\ntrace:\n",
        )
        .unwrap();
        write_launch(&watchdog, &ok_launch).unwrap();

        let leftover = process("zones-base");
        write_result(
            &leftover,
            &sample("zones", Execution::Ok, Report::default()),
        )
        .unwrap();
        write_launch(
            &leftover,
            &launch_record(Execution::error("остались процессы с nonce: 812")),
        )
        .unwrap();

        let unlaunched = process("ime-base");
        write_result(
            &unlaunched,
            &sample("ime", Execution::Ok, Report::default()),
        )
        .unwrap();

        let corrupt = process("teardown-base");
        fs::write(corrupt.join(RESULT_FILE), b"not json").unwrap();
        write_launch(&corrupt, &ok_launch).unwrap();
        fs::write(run.join("stray.txt"), b"not a process").unwrap();

        let summary = aggregate(&run).unwrap();
        assert_eq!(
            summary.totals,
            Totals {
                processes: 7,
                execution_ok: 2,
                execution_errors: 5,
                pass: 1,
                fail: 1,
                pending_manual: 2,
                required_pending_manual: 1,
                snapshots_pending_manual: 1,
            }
        );
        let execution = |dir: &str| {
            let process = summary
                .processes
                .iter()
                .find(|process| process.dir == dir)
                .unwrap();
            match &process.execution {
                Execution::Error { reason, .. } => reason.clone(),
                Execution::Ok => String::new(),
            }
        };
        assert!(execution("input-base").contains("сторож: срок 40 с истёк"));
        assert!(execution("input-base").contains(WATCHDOG_FILE));
        assert!(execution("window-base").contains("нет активности"));
        assert!(execution("zones-base").starts_with("запуск: остались процессы"));
        assert!(execution("ime-base").contains(LAUNCH_FILE));
        assert!(execution("teardown-base").contains("повреждён"));
        assert_eq!(execution("chord-base"), "");
        let route = summary
            .processes
            .iter()
            .find(|process| process.dir == "teardown-base")
            .and_then(|process| process.route.clone());
        assert_eq!(route.as_deref(), Some("direct"), "маршрут из launch.json");

        let names: Vec<(&str, Requirement, Verdict)> = summary
            .findings
            .iter()
            .map(|finding| (finding.name.as_str(), finding.requirement, finding.verdict))
            .collect();
        assert_eq!(
            names,
            [
                ("chord_legacy", Requirement::Required, Verdict::Fail),
                ("latency_d", Requirement::Research, Verdict::Pass),
                (
                    "palette_composition",
                    Requirement::Required,
                    Verdict::PendingManual
                ),
                (
                    "latency_photon",
                    Requirement::Research,
                    Verdict::PendingManual
                ),
            ]
        );

        write_summary(&run, &summary).unwrap();
        let written: SummaryFile =
            serde_json::from_slice(&fs::read(run.join(SUMMARY_FILE)).unwrap()).unwrap();
        assert_eq!(written, summary);
        let again = aggregate(&run).unwrap();
        assert_eq!(
            again.totals, summary.totals,
            "summary.json не считается процессом"
        );

        let text = render_summary(&summary);
        assert!(text.starts_with("исполнение: процессов 7, без ошибок 2, с ошибкой 5\n"));
        assert!(text.contains("pending-manual 2 (обязательных 1)"));
        assert!(text.contains("pending-manual required palette base palette_composition"));
        assert!(text.contains("ошибка  zones-base"));
        fs::remove_dir_all(run).unwrap();
    }

    #[test]
    fn aggregate_of_missing_run_dir_fails() {
        let run = temp_dir("absent");
        fs::remove_dir_all(&run).unwrap();
        assert!(aggregate(&run).is_err());
    }
}
