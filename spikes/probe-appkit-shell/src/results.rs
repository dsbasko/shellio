//! Схема `result.json` процесса сценария и `summary.json` прогона, итог сценария и
//! агрегирование. Модуль не зависит от AppKit.
//!
//! Исполнение (`execution`) и выводы (`findings`) разделены: ошибка исполнения значит, что стенд
//! не дошёл до конца, а `fail` и `pending-manual` — законченные наблюдения.

use std::collections::BTreeMap;
use std::fmt::{Display, Write as _};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::stats::{self, Summary};

pub const SCHEMA: u32 = 1;
pub const RESULT_FILE: &str = "result.json";
pub const SUMMARY_FILE: &str = "summary.json";
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub name: String,
    pub verdict: Verdict,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Execution {
    Ok,
    Error {
        reason: String,
        /// Состояние AppKit, прочитанное в момент ошибки.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_state: Option<String>,
    },
}

impl Execution {
    pub fn is_ok(&self) -> bool {
        matches!(self, Execution::Ok)
    }
}

/// Прочитанное состояние: строка или структура, например иерархия видов.
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

/// Отметки синтетического события. Время — от установки стенда.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub number: u64,
    pub label: String,
    pub window: isize,
    /// Состояние AppKit перед отправкой.
    pub before: String,
    pub posted_ms: f64,
    pub dispatch_start_ms: Option<f64>,
    pub send_return_ms: Option<f64>,
    /// Конец первого прохода отображения после возврата `sendEvent`.
    pub display_pass_ms: Option<f64>,
    pub undispatched: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotKind {
    /// Рендер отдельного вида через `cacheDisplay(in:to:)`, не снимок экрана.
    ViewRender,
    /// `screencapture -l`: окно с тенью.
    Window,
    /// `screencapture -R`: прямоугольник экрана с окном, палитрой, тенью и обоями.
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
    /// Снятая область в точках: у вида — его границы, у снимков экрана — прямоугольник экрана.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect_points: Option<Rect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixels: Option<Size>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_space: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unclosed_dispatch: Option<String>,
    #[serde(default)]
    pub notes: Vec<Observation>,
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
    pub objc2: Option<String>,
    pub profile: String,
    pub probe_version: String,
}

impl Build {
    /// Сведения о сборке передаёт `run.sh` переменными окружения; сама проба git и rustc не
    /// вызывает.
    pub fn from_env(lookup: impl Fn(&str) -> Option<String>) -> Build {
        let value = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        Build {
            commit: value("PROBE_COMMIT"),
            rustc: value("PROBE_RUSTC"),
            objc2: value("PROBE_OBJC2"),
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
    /// Какой дисплей выбрала сцена: `builtin` или `main`.
    pub chosen: String,
    pub frame: Rect,
    pub visible_frame: Rect,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneInfo {
    pub split: String,
    pub contrast: String,
    pub motion: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub os: String,
    pub hardware: Hardware,
    pub build: Build,
    pub scene: SceneInfo,
    pub display: Option<DisplayInfo>,
    pub window: Option<WindowInfo>,
    pub input_source: Option<String>,
    #[serde(default)]
    pub fixtures: BTreeMap<String, String>,
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
    pub display: Option<DisplayInfo>,
    pub window: Option<WindowInfo>,
    pub input_source: Option<String>,
    pub fixtures: BTreeMap<String, String>,
}

impl Report {
    pub fn push(&mut self, key: &str, value: impl Display) {
        self.observations.push(Observation {
            key: key.to_owned(),
            value: Value::String(value.to_string()),
        });
    }

    pub fn push_json(&mut self, key: &str, value: &impl Serialize) {
        let value = serde_json::to_value(value)
            .unwrap_or_else(|err| Value::String(format!("ошибка сериализации: {err}")));
        self.observations.push(Observation {
            key: key.to_owned(),
            value,
        });
    }

    pub fn finding(&mut self, name: &str, verdict: Verdict, reason: impl Into<String>) {
        self.findings.push(Finding {
            name: name.to_owned(),
            verdict,
            reason: reason.into(),
        });
    }

    pub fn metric(&mut self, name: &str, unit: &str, samples: Vec<f64>) {
        self.metrics.push(Metric::new(name, unit, samples));
    }

    pub fn snapshot(&mut self, snapshot: Snapshot) {
        self.snapshots.push(snapshot);
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
        environment.display = report.display.or(environment.display);
        environment.window = report.window.or(environment.window);
        environment.input_source = report.input_source.or(environment.input_source);
        environment.fixtures.extend(report.fixtures);
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

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    Missing,
    Corrupt(String),
    Io(String),
}

pub fn read_result(dir: &Path) -> Result<ResultFile, ReadError> {
    let path = dir.join(RESULT_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(ReadError::Missing),
        Err(err) => return Err(ReadError::Io(err.to_string())),
    };
    serde_json::from_slice(&bytes).map_err(|err| ReadError::Corrupt(err.to_string()))
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub processes: usize,
    pub execution_ok: usize,
    pub execution_errors: usize,
    pub pass: usize,
    pub fail: usize,
    pub pending_manual: usize,
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

/// Сценарий и вариант по имени каталога `<сценарий>-<вариант>`, который даёт `run.sh`.
pub fn split_dir_name(name: &str) -> (String, String) {
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
    let error = |reason: String| ProcessSummary {
        dir: name.clone(),
        scenario: scenario.clone(),
        variant: variant.clone(),
        execution: Execution::Error {
            reason,
            last_state: None,
        },
        findings: Vec::new(),
        metrics: Vec::new(),
        snapshots: Vec::new(),
    };
    match read_result(dir) {
        Ok(result) => ProcessSummary {
            dir: name.clone(),
            scenario: result.scenario,
            variant: result.variant,
            execution: result.execution,
            findings: result.findings,
            metrics: result
                .metrics
                .into_iter()
                .map(|metric| MetricSummary {
                    name: metric.name,
                    unit: metric.unit,
                    summary: metric.summary,
                })
                .collect(),
            snapshots: result
                .snapshots
                .into_iter()
                .map(|snapshot| SnapshotBrief {
                    name: snapshot.name,
                    kind: snapshot.kind,
                    status: snapshot.status,
                    reason: snapshot.reason,
                })
                .collect(),
        },
        Err(ReadError::Missing) => error(match watchdog_reason(dir) {
            Some(reason) => format!("{RESULT_FILE} отсутствует: {reason}"),
            None => format!("{RESULT_FILE} отсутствует"),
        }),
        Err(ReadError::Corrupt(err)) => error(format!("{RESULT_FILE} повреждён: {err}")),
        Err(ReadError::Io(err)) => error(format!("{RESULT_FILE} не прочитан: {err}")),
    }
}

/// Собирает `result.json` всех процессов прогона: каждый подкаталог — один процесс.
/// Отсутствующий или повреждённый `result.json` — ошибка исполнения этого процесса, а не
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
                Verdict::PendingManual => totals.pending_manual += 1,
            }
            findings.push(RunFinding {
                scenario: process.scenario.clone(),
                variant: process.variant.clone(),
                name: finding.name.clone(),
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
        "выводы: pass {}, fail {}, pending-manual {}; снимков pending-manual {}",
        totals.pass, totals.fail, totals.pending_manual, totals.snapshots_pending_manual
    );
    for finding in &summary.findings {
        let _ = writeln!(
            text,
            "  {:<14} {} {} {}: {}",
            finding.verdict.name(),
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
            "probe-appkit-shell-results-{name}-{}",
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
            build: Build::from_env(|name| match name {
                "PROBE_COMMIT" => Some("72ae1ce".to_owned()),
                "PROBE_RUSTC" => Some("rustc 1.99.1".to_owned()),
                _ => None,
            }),
            scene: SceneInfo {
                split: "plain".to_owned(),
                contrast: "system".to_owned(),
                motion: "system".to_owned(),
            },
            display: None,
            window: None,
            input_source: None,
            fixtures: BTreeMap::new(),
        }
    }

    fn sample(scenario: &str, execution: Execution, report: Report) -> ResultFile {
        ResultFile::new(scenario, "plain", environment(), execution, report)
    }

    fn error_sample() -> ResultFile {
        let mut report = Report::default();
        report.push("wait_ms", "5000.0");
        report
            .diagnostics
            .trace
            .push("0.100ms post #1 keyDown Down".to_owned());
        sample(
            "launch",
            Execution::Error {
                reason: "нет активности и key window за 5000 мс".to_owned(),
                last_state: Some("isActive=false keyWindow=none".to_owned()),
            },
            report,
        )
    }

    fn negative_sample() -> ResultFile {
        let mut report = Report::default();
        report.finding("native_menu", Verdict::Fail, "вызван не тот пункт");
        report.finding("parent_click", Verdict::Pass, "дошёл");
        report.metric("mouse_down_send_event", "ms", vec![434.7]);
        report.steps.push(StepRecord {
            index: 0,
            name: "меню открыто".to_owned(),
            status: StepStatus::Ready,
            waited_ms: 12.5,
            at_ms: 40.0,
            state: "isActive=true".to_owned(),
        });
        report.events.push(EventRecord {
            number: 1,
            label: "mouseDown NSPopUpButton".to_owned(),
            window: 7,
            before: "isActive=true".to_owned(),
            posted_ms: 1.0,
            dispatch_start_ms: Some(1.5),
            send_return_ms: Some(436.2),
            display_pass_ms: Some(437.0),
            undispatched: false,
        });
        sample("native-menu", Execution::Ok, report)
    }

    fn pending_sample() -> ResultFile {
        let mut report = Report::default();
        report.finding(
            "screen_capture",
            Verdict::PendingManual,
            "нет разрешения Screen Recording",
        );
        report.snapshot(Snapshot {
            name: "window".to_owned(),
            kind: SnapshotKind::Window,
            status: SnapshotStatus::PendingManual,
            file: None,
            reason: Some("нет разрешения Screen Recording".to_owned()),
            rect_points: None,
            pixels: None,
            color_space: None,
        });
        report.push_json(
            "frame",
            &Rect {
                x: 0.0,
                y: 0.0,
                width: 1180.0,
                height: 720.0,
            },
        );
        sample("readback-check", Execution::Ok, report)
    }

    #[test]
    fn result_round_trips() {
        let dir = temp_dir("round-trip");
        for result in [error_sample(), negative_sample(), pending_sample()] {
            write_result(&dir, &result).unwrap();
            assert_eq!(read_result(&dir), Ok(result));
        }
        assert!(!dir.join(format!(".{RESULT_FILE}.partial")).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn execution_and_findings_are_separate_in_json() {
        let json = serde_json::to_value(error_sample()).unwrap();
        assert_eq!(json["execution"]["status"], "error");
        assert_eq!(
            json["execution"]["last_state"],
            "isActive=false keyWindow=none"
        );
        assert_eq!(json["findings"], Value::Array(Vec::new()));

        let json = serde_json::to_value(negative_sample()).unwrap();
        assert_eq!(json["execution"], serde_json::json!({ "status": "ok" }));
        assert_eq!(json["findings"][0]["verdict"], "fail");
        assert_eq!(json["metrics"][0]["summary"]["p95"], 434.7);
        assert_eq!(json["percentile_method"], "nearest-rank");

        let json = serde_json::to_value(pending_sample()).unwrap();
        assert_eq!(json["findings"][0]["verdict"], "pending-manual");
        assert_eq!(json["snapshots"][0]["status"], "pending-manual");
        assert_eq!(json["snapshots"][0]["kind"], "window");
        assert_eq!(json["observations"][0]["value"]["width"], 1180.0);
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
            fixtures: BTreeMap::from([("list".to_owned(), "100000".to_owned())]),
            ..Report::default()
        };
        let result = sample("launch", Execution::Ok, report);
        assert_eq!(
            result.environment.input_source.as_deref(),
            Some("com.apple.keylayout.ABC")
        );
        assert_eq!(result.environment.fixtures["list"], "100000");
        assert_eq!(result.environment.build.commit.as_deref(), Some("72ae1ce"));
        assert_eq!(result.environment.build.objc2, None);
    }

    #[test]
    fn build_ignores_blank_values() {
        let build = Build::from_env(|_| Some("  ".to_owned()));
        assert_eq!((build.commit, build.rustc, build.objc2), (None, None, None));
    }

    #[test]
    fn missing_result_is_reported() {
        let dir = temp_dir("missing");
        assert_eq!(read_result(&dir), Err(ReadError::Missing));
        fs::write(dir.join(RESULT_FILE), b"{\"schema\": 1,").unwrap();
        assert!(matches!(read_result(&dir), Err(ReadError::Corrupt(_))));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dir_names_split_at_last_dash() {
        assert_eq!(
            split_dir_name("native-menu-plain"),
            ("native-menu".to_owned(), "plain".to_owned())
        );
        assert_eq!(
            split_dir_name("launch-sidebar"),
            ("launch".to_owned(), "sidebar".to_owned())
        );
        assert_eq!(split_dir_name("odd"), ("odd".to_owned(), String::new()));
        assert_eq!(split_dir_name("odd-"), ("odd-".to_owned(), String::new()));
    }

    #[test]
    fn aggregate_run() {
        let run = temp_dir("aggregate");
        let process = |name: &str| {
            let dir = run.join(name);
            fs::create_dir_all(&dir).unwrap();
            dir
        };
        write_result(&process("launch-plain"), &error_sample()).unwrap();
        write_result(&process("native-menu-plain"), &negative_sample()).unwrap();
        write_result(&process("readback-check-plain"), &pending_sample()).unwrap();
        let watchdog = process("key-delivery-plain");
        fs::write(
            watchdog.join(WATCHDOG_FILE),
            "scenario=key-delivery\nexecution=error\nreason=сторож: срок 40 с истёк\ntrace:\n",
        )
        .unwrap();
        let corrupt = process("parent-click-plain");
        fs::write(corrupt.join(RESULT_FILE), b"not json").unwrap();
        process("split-sidebar");
        fs::write(run.join("stray.txt"), b"not a process").unwrap();

        let summary = aggregate(&run).unwrap();
        assert_eq!(
            summary.totals,
            Totals {
                processes: 6,
                execution_ok: 2,
                execution_errors: 4,
                pass: 1,
                fail: 1,
                pending_manual: 1,
                snapshots_pending_manual: 1,
            }
        );
        let dirs: Vec<&str> = summary.processes.iter().map(|p| p.dir.as_str()).collect();
        assert_eq!(
            dirs,
            [
                "key-delivery-plain",
                "launch-plain",
                "native-menu-plain",
                "parent-click-plain",
                "readback-check-plain",
                "split-sidebar"
            ]
        );
        let reason = |index: usize| match &summary.processes[index].execution {
            Execution::Error { reason, .. } => reason.clone(),
            Execution::Ok => String::new(),
        };
        assert!(
            reason(0).contains("сторож: срок 40 с истёк"),
            "{}",
            reason(0)
        );
        assert!(reason(0).contains(WATCHDOG_FILE), "{}", reason(0));
        assert!(reason(1).contains("нет активности"), "{}", reason(1));
        assert!(reason(3).contains("повреждён"), "{}", reason(3));
        assert_eq!(reason(5), format!("{RESULT_FILE} отсутствует"));
        assert_eq!(
            (
                summary.processes[3].scenario.as_str(),
                summary.processes[3].variant.as_str()
            ),
            ("parent-click", "plain")
        );
        let names: Vec<(&str, Verdict)> = summary
            .findings
            .iter()
            .map(|finding| (finding.name.as_str(), finding.verdict))
            .collect();
        assert_eq!(
            names,
            [
                ("native_menu", Verdict::Fail),
                ("parent_click", Verdict::Pass),
                ("screen_capture", Verdict::PendingManual)
            ]
        );
        assert_eq!(
            summary.processes[2].metrics[0].summary.map(|s| s.max),
            Some(434.7)
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
        assert!(text.starts_with("исполнение: процессов 6, без ошибок 2, с ошибкой 4\n"));
        assert!(text.contains("pending-manual readback-check plain screen_capture"));
        assert!(text.contains("ошибка  parent-click-plain"));
        fs::remove_dir_all(run).unwrap();
    }

    #[test]
    fn aggregate_of_missing_run_dir_fails() {
        let run = temp_dir("absent");
        fs::remove_dir_all(&run).unwrap();
        assert!(aggregate(&run).is_err());
    }
}
