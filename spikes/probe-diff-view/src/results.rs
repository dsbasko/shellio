//! Схема `result.json` процесса сценария и `summary.json` прогона, имя каталога процесса и
//! агрегирование. Модуль не зависит от AppKit.
//!
//! Источник — `spikes/probe-appkit-shell/src/results.rs` (P0.2). Упрощён: без отметок старта,
//! шагов, событий и снимков (их добавляют задачи стенда), вместо `pending-manual` — `not_run`
//! с обязательной причиной, у процесса есть вариант отрисовки и режим.
//!
//! Исполнение (`execution`) и выводы (`findings`) разделены: ошибка исполнения значит, что стенд
//! не дошёл до конца, а `fail` и `not_run` — законченные наблюдения. Выводы процесса с ошибкой
//! исполнения в итоги прогона не входят.

use std::fmt::{Display, Write as _};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::args::{Variant, ViewMode};
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
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    /// Заранее перечисленное наблюдение, которое автоматика не провела; причина обязательна.
    NotRun,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::NotRun => "not_run",
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
        /// Состояние, прочитанное в момент ошибки.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_state: Option<String>,
    },
}

impl Execution {
    pub fn is_ok(&self) -> bool {
        matches!(self, Execution::Ok)
    }

    fn error(reason: String) -> Execution {
        Execution::Error {
            reason,
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
    /// `tree-sitter`, `tree-sitter-language` и грамматика `tree-sitter-rust` из `Cargo.lock`.
    pub tree_sitter: Option<String>,
    pub xcode: Option<String>,
    pub xctrace: Option<String>,
    pub profile: String,
    pub probe_version: String,
}

impl Build {
    /// Сведения о сборке и инструментах передаёт `run.sh` переменными окружения; сама проба
    /// git, rustc и xcrun не вызывает.
    pub fn from_env(lookup: impl Fn(&str) -> Option<String>) -> Build {
        let value = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        Build {
            commit: value("PROBE_COMMIT"),
            rustc: value("PROBE_RUSTC"),
            objc2: value("PROBE_OBJC2"),
            tree_sitter: value("PROBE_TREE_SITTER"),
            xcode: value("PROBE_XCODE"),
            xctrace: value("PROBE_XCTRACE"),
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
    pub frame: Rect,
    pub visible_frame: Rect,
    pub scale: f64,
    pub max_fps: isize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// Дисплей, на котором стоит окно.
    pub display_id: Option<u32>,
    pub frame: Rect,
    pub content_points: Size,
    pub backing_scale: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub os: String,
    pub hardware: Hardware,
    pub build: Build,
    #[serde(default)]
    pub displays: Vec<DisplayInfo>,
    #[serde(default)]
    pub window: Option<WindowInfo>,
}

impl Environment {
    /// Поля окружения, которых нет: процесс без них описан не полностью.
    pub fn missing(&self) -> Vec<&'static str> {
        let build = &self.build;
        [
            ("os", !self.os.trim().is_empty()),
            ("hardware.model", self.hardware.model.is_some()),
            ("hardware.cpu", self.hardware.cpu.is_some()),
            (
                "hardware.memory_bytes",
                self.hardware.memory_bytes.is_some(),
            ),
            ("build.commit", build.commit.is_some()),
            ("build.rustc", build.rustc.is_some()),
            ("build.objc2", build.objc2.is_some()),
            ("build.tree_sitter", build.tree_sitter.is_some()),
            ("build.xcode", build.xcode.is_some()),
            ("build.xctrace", build.xctrace.is_some()),
            ("displays", !self.displays.is_empty()),
        ]
        .into_iter()
        .filter_map(|(name, present)| (!present).then_some(name))
        .collect()
    }
}

/// Кто процесс: сценарий, вариант, режим и номер повторного запуска. Каталог процесса внутри
/// прогона называется `<сценарий>[-<вариант>][-<режим>][-<NN>]`, так его строит `run.sh`.
/// По имени каталога процесс опознаётся и без `result.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessKey {
    pub scenario: String,
    pub variant: Option<Variant>,
    pub mode: Option<ViewMode>,
    pub run: Option<u32>,
}

impl ProcessKey {
    pub fn dir_name(&self) -> String {
        let mut name = self.scenario.clone();
        if let Some(variant) = self.variant {
            let _ = write!(name, "-{}", variant.name());
        }
        if let Some(mode) = self.mode {
            let _ = write!(name, "-{}", mode.name());
        }
        if let Some(run) = self.run {
            let _ = write!(name, "-{run:02}");
        }
        name
    }

    /// Разбирает имя каталога с конца: номер, режим, вариант, остаток — сценарий. Имена
    /// сценариев не оканчиваются на `-<вариант>`, `-<режим>` или `-<число>`, это проверяет тест.
    pub fn parse(name: &str) -> Option<ProcessKey> {
        let mut parts: Vec<&str> = name.split('-').collect();
        if parts.iter().any(|part| part.is_empty()) {
            return None;
        }
        let mut take = |accept: &dyn Fn(&str) -> bool| match parts.as_slice() {
            [_, .., last] if accept(last) => parts.pop(),
            _ => None,
        };
        let run = take(&|part| part.bytes().all(|byte| byte.is_ascii_digit()))
            .map(str::parse::<u32>)
            .transpose()
            .ok()?;
        let mode = take(&|part| ViewMode::from_name(part).is_some()).and_then(ViewMode::from_name);
        let variant = take(&|part| Variant::from_name(part).is_some()).and_then(Variant::from_name);
        Some(ProcessKey {
            scenario: parts.join("-"),
            variant,
            mode,
            run,
        })
    }

    fn same_process(&self, result: &ResultFile) -> bool {
        self.scenario == result.scenario
            && self.variant == result.variant
            && self.mode == result.mode
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultFile {
    pub schema: u32,
    pub scenario: String,
    #[serde(default)]
    pub variant: Option<Variant>,
    #[serde(default)]
    pub mode: Option<ViewMode>,
    pub environment: Environment,
    pub execution: Execution,
    pub percentile_method: String,
    pub observations: Vec<Observation>,
    pub metrics: Vec<Metric>,
    pub findings: Vec<Finding>,
}

/// Итог сценария, который собирают стенд и сценарий.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub observations: Vec<Observation>,
    pub findings: Vec<Finding>,
    pub metrics: Vec<Metric>,
    pub displays: Vec<DisplayInfo>,
    pub window: Option<WindowInfo>,
}

impl Report {
    pub fn push(&mut self, key: &str, value: impl Display) {
        self.observations.push(Observation {
            key: key.to_owned(),
            value: Value::String(value.to_string()),
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
}

impl ResultFile {
    /// Собирает `result.json` из постоянной части окружения и итога сценария.
    pub fn new(
        scenario: &str,
        variant: Option<Variant>,
        mode: Option<ViewMode>,
        mut environment: Environment,
        execution: Execution,
        report: Report,
    ) -> ResultFile {
        if !report.displays.is_empty() {
            environment.displays = report.displays;
        }
        environment.window = report.window.or(environment.window);
        ResultFile {
            schema: SCHEMA,
            scenario: scenario.to_owned(),
            variant,
            mode,
            environment,
            execution,
            percentile_method: stats::METHOD.to_owned(),
            observations: report.observations,
            metrics: report.metrics,
            findings: report.findings,
        }
    }

    /// Нарушения схемы, которые serde не ловит: другая версия схемы, вывод без имени,
    /// `not_run` без причины.
    pub fn violations(&self) -> Vec<String> {
        let mut violations = Vec::new();
        if self.schema != SCHEMA {
            violations.push(format!("схема {} вместо {SCHEMA}", self.schema));
        }
        for (index, finding) in self.findings.iter().enumerate() {
            if finding.name.trim().is_empty() {
                violations.push(format!("вывод №{index} без имени"));
            }
            if finding.verdict == Verdict::NotRun && finding.reason.trim().is_empty() {
                violations.push(format!("not_run «{}» без причины", finding.name));
            }
        }
        violations
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
    pub not_run: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricSummary {
    pub name: String,
    pub unit: String,
    pub summary: Option<Summary>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessSummary {
    /// Каталог процесса внутри прогона, он же `--out` процесса.
    pub dir: String,
    pub scenario: String,
    pub variant: Option<Variant>,
    pub mode: Option<ViewMode>,
    pub execution: Execution,
    /// Выводы процесса; при ошибке исполнения они сохраняются для разбора, но в итоги не входят.
    pub findings: Vec<Finding>,
    pub metrics: Vec<MetricSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFinding {
    pub dir: String,
    pub scenario: String,
    pub variant: Option<Variant>,
    pub mode: Option<ViewMode>,
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
    /// Выводы процессов без ошибки исполнения, отдельно от исполнения.
    pub findings: Vec<RunFinding>,
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
    let key = ProcessKey::parse(&name);
    let error = |reason: String| ProcessSummary {
        dir: name.clone(),
        scenario: key
            .as_ref()
            .map_or_else(|| name.clone(), |key| key.scenario.clone()),
        variant: key.as_ref().and_then(|key| key.variant),
        mode: key.as_ref().and_then(|key| key.mode),
        execution: Execution::error(reason),
        findings: Vec::new(),
        metrics: Vec::new(),
    };
    let Some(key) = key.clone() else {
        return error(format!("имя каталога «{name}» не по схеме процесса"));
    };
    let result = match read_result(dir) {
        Ok(result) => result,
        Err(ReadError::Missing) => {
            return error(match watchdog_reason(dir) {
                Some(reason) => format!("{RESULT_FILE} отсутствует: {reason}"),
                None => format!("{RESULT_FILE} отсутствует"),
            });
        }
        Err(ReadError::Corrupt(err)) => return error(format!("{RESULT_FILE} повреждён: {err}")),
        Err(ReadError::Io(err)) => return error(format!("{RESULT_FILE} не прочитан: {err}")),
    };
    let violations = result.violations();
    let execution = if !key.same_process(&result) {
        let written = ProcessKey {
            scenario: result.scenario.clone(),
            variant: result.variant,
            mode: result.mode,
            run: key.run,
        };
        Execution::error(format!(
            "{RESULT_FILE} другого процесса: записан {}, каталог {name}",
            written.dir_name()
        ))
    } else if !violations.is_empty() {
        Execution::error(format!(
            "{RESULT_FILE} нарушает схему: {}",
            violations.join("; ")
        ))
    } else {
        result.execution
    };
    ProcessSummary {
        dir: name.clone(),
        scenario: key.scenario,
        variant: key.variant,
        mode: key.mode,
        execution,
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
    }
}

/// Собирает `result.json` всех процессов прогона: каждый подкаталог — один процесс.
/// Отсутствующий, повреждённый или чужой `result.json` — ошибка исполнения этого процесса, а не
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
            for finding in &process.findings {
                match finding.verdict {
                    Verdict::Pass => totals.pass += 1,
                    Verdict::Fail => totals.fail += 1,
                    Verdict::NotRun => totals.not_run += 1,
                }
                findings.push(RunFinding {
                    dir: process.dir.clone(),
                    scenario: process.scenario.clone(),
                    variant: process.variant,
                    mode: process.mode,
                    name: finding.name.clone(),
                    verdict: finding.verdict,
                    reason: finding.reason.clone(),
                });
            }
        } else {
            totals.execution_errors += 1;
        }
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
        "выводы: pass {}, fail {}, not_run {}",
        totals.pass, totals.fail, totals.not_run
    );
    for finding in &summary.findings {
        let _ = writeln!(
            text,
            "  {:<8} {} {}: {}",
            finding.verdict.name(),
            finding.dir,
            finding.name,
            finding.reason
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Scenario;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "probe-diff-view-results-{name}-{}",
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
                "PROBE_COMMIT" => Some("e89bdb4304b4".to_owned()),
                "PROBE_RUSTC" => Some("rustc 1.99.1".to_owned()),
                "PROBE_TREE_SITTER" => Some("tree-sitter 0.27.2".to_owned()),
                _ => None,
            }),
            displays: Vec::new(),
            window: None,
        }
    }

    fn display() -> DisplayInfo {
        let frame = Rect {
            x: 0.0,
            y: 0.0,
            width: 1512.0,
            height: 982.0,
        };
        DisplayInfo {
            id: 1,
            name: "Built-in Retina Display".to_owned(),
            builtin: true,
            frame,
            visible_frame: frame,
            scale: 2.0,
            max_fps: 120,
        }
    }

    fn sample(
        scenario: &str,
        variant: Option<Variant>,
        mode: Option<ViewMode>,
        execution: Execution,
        report: Report,
    ) -> ResultFile {
        ResultFile::new(scenario, variant, mode, environment(), execution, report)
    }

    fn ok_sample() -> ResultFile {
        let mut report = Report::default();
        report.finding("window_ready", Verdict::Pass, "окно видимо и key");
        report.metric("ready_ms", "ms", vec![42.5]);
        report.push("wait", "окно");
        report.displays.push(display());
        sample("smoke", None, None, Execution::Ok, report)
    }

    fn not_run_sample() -> ResultFile {
        let mut report = Report::default();
        report.finding("frames_120hz", Verdict::NotRun, "120 Гц не подтверждены");
        report.finding(
            "open_budget",
            Verdict::Fail,
            "максимум 230 мс при бюджете 200 мс",
        );
        report.observations.push(Observation {
            key: "frame".to_owned(),
            value: serde_json::json!({ "width": 1180.0, "height": 720.0 }),
        });
        sample(
            "open",
            Some(Variant::B),
            Some(ViewMode::Side),
            Execution::Ok,
            report,
        )
    }

    fn error_sample() -> ResultFile {
        let mut report = Report::default();
        report.finding("window_ready", Verdict::Pass, "не дойдёт до итогов");
        sample(
            "smoke",
            None,
            None,
            Execution::Error {
                reason: "окно не стало key за 10000 мс".to_owned(),
                last_state: Some("isActive=false keyWindow=none".to_owned()),
            },
            report,
        )
    }

    #[test]
    fn result_round_trips() {
        let dir = temp_dir("round-trip");
        for result in [ok_sample(), not_run_sample(), error_sample()] {
            write_result(&dir, &result).unwrap();
            assert_eq!(read_result(&dir), Ok(result));
        }
        assert!(!dir.join(format!(".{RESULT_FILE}.partial")).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn json_shape() {
        let json = serde_json::to_value(not_run_sample()).unwrap();
        assert_eq!(json["execution"], serde_json::json!({ "status": "ok" }));
        assert_eq!(json["variant"], "b");
        assert_eq!(json["mode"], "side");
        assert_eq!(json["findings"][0]["verdict"], "not_run");
        assert_eq!(json["findings"][0]["reason"], "120 Гц не подтверждены");
        assert_eq!(json["findings"][1]["verdict"], "fail");
        assert_eq!(json["percentile_method"], "nearest-rank");
        assert_eq!(json["observations"][0]["value"]["width"], 1180.0);

        let json = serde_json::to_value(ok_sample()).unwrap();
        assert_eq!(json["variant"], Value::Null);
        assert_eq!(json["metrics"][0]["summary"]["p95"], 42.5);
        assert_eq!(json["environment"]["displays"][0]["max_fps"], 120);
        assert_eq!(
            json["environment"]["build"]["tree_sitter"],
            "tree-sitter 0.27.2"
        );

        let json = serde_json::to_value(error_sample()).unwrap();
        assert_eq!(json["execution"]["status"], "error");
        assert_eq!(
            json["execution"]["last_state"],
            "isActive=false keyWindow=none"
        );
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
    fn build_ignores_blank_values() {
        let build = Build::from_env(|_| Some("  ".to_owned()));
        assert_eq!(
            (
                build.commit,
                build.rustc,
                build.objc2,
                build.tree_sitter,
                build.xcode,
                build.xctrace
            ),
            (None, None, None, None, None, None)
        );
    }

    #[test]
    fn missing_environment_fields() {
        let result = ok_sample();
        assert_eq!(
            result.environment.missing(),
            ["build.objc2", "build.xcode", "build.xctrace"]
        );
        let mut empty = environment();
        empty.os = " ".to_owned();
        empty.hardware = Hardware::default();
        assert!(empty.missing().contains(&"os"));
        assert!(empty.missing().contains(&"hardware.memory_bytes"));
        assert!(empty.missing().contains(&"displays"));
    }

    #[test]
    fn violations_of_schema() {
        assert!(ok_sample().violations().is_empty());
        assert!(not_run_sample().violations().is_empty());
        let mut bad = not_run_sample();
        bad.schema = 7;
        bad.findings.push(Finding {
            name: "ax_voiceover".to_owned(),
            verdict: Verdict::NotRun,
            reason: " ".to_owned(),
        });
        bad.findings.push(Finding {
            name: String::new(),
            verdict: Verdict::Pass,
            reason: "есть".to_owned(),
        });
        assert_eq!(
            bad.violations(),
            [
                "схема 7 вместо 1",
                "not_run «ax_voiceover» без причины",
                "вывод №3 без имени"
            ]
        );
    }

    #[test]
    fn missing_or_corrupt_result_is_reported() {
        let dir = temp_dir("missing");
        assert_eq!(read_result(&dir), Err(ReadError::Missing));
        fs::write(dir.join(RESULT_FILE), b"{\"schema\": 1,").unwrap();
        assert!(matches!(read_result(&dir), Err(ReadError::Corrupt(_))));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn process_dir_names() {
        let key = |scenario: &str, variant, mode, run| ProcessKey {
            scenario: scenario.to_owned(),
            variant,
            mode,
            run,
        };
        for (name, expected) in [
            ("smoke", key("smoke", None, None, None)),
            (
                "open-a-unified-01",
                key("open", Some(Variant::A), Some(ViewMode::Unified), Some(1)),
            ),
            (
                "open-warm-b-side",
                key("open-warm", Some(Variant::B), Some(ViewMode::Side), None),
            ),
            (
                "open-smoke-a-side-11",
                key(
                    "open-smoke",
                    Some(Variant::A),
                    Some(ViewMode::Side),
                    Some(11),
                ),
            ),
            ("bounded-b", key("bounded", Some(Variant::B), None, None)),
            (
                "zones-unified",
                key("zones", None, Some(ViewMode::Unified), None),
            ),
            ("highlight-03", key("highlight", None, None, Some(3))),
        ] {
            assert_eq!(ProcessKey::parse(name).as_ref(), Some(&expected), "{name}");
            assert_eq!(expected.dir_name(), name);
        }
        for odd in ["", "-a", "smoke-", "open--a", "a-"] {
            assert_eq!(ProcessKey::parse(odd), None, "{odd:?}");
        }
        assert_eq!(
            ProcessKey::parse("a"),
            Some(key("a", None, None, None)),
            "одна часть — всегда сценарий"
        );
        assert_eq!(ProcessKey::parse("open-99999999999"), None, "номер вне u32");
    }

    /// Имя сценария с любыми осями разбирается обратно в то же имя.
    #[test]
    fn scenario_names_survive_dir_parsing() {
        for scenario in Scenario::ALL {
            for variant in [None, Some(Variant::A), Some(Variant::B)] {
                for mode in [None, Some(ViewMode::Unified), Some(ViewMode::Side)] {
                    for run in [None, Some(1), Some(11)] {
                        let key = ProcessKey {
                            scenario: scenario.name().to_owned(),
                            variant,
                            mode,
                            run,
                        };
                        assert_eq!(ProcessKey::parse(&key.dir_name()), Some(key));
                    }
                }
            }
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
        write_result(&process("smoke"), &ok_sample()).unwrap();
        write_result(&process("open-b-side-02"), &not_run_sample()).unwrap();
        write_result(&process("smoke-03"), &error_sample()).unwrap();
        let watchdog = process("open-a-unified-01");
        fs::write(
            watchdog.join(WATCHDOG_FILE),
            "scenario=open\nexecution=error\nreason=сторож: срок 60 с истёк\ntrace:\n",
        )
        .unwrap();
        let corrupt = process("open-a-side");
        fs::write(corrupt.join(RESULT_FILE), b"not json").unwrap();
        write_result(&process("open-a-unified-03"), &not_run_sample()).unwrap();
        let mut unreasoned = ok_sample();
        unreasoned.findings.push(Finding {
            name: "ax_voiceover".to_owned(),
            verdict: Verdict::NotRun,
            reason: String::new(),
        });
        write_result(&process("smoke-04"), &unreasoned).unwrap();
        process("odd--name");
        process("zones-b");
        fs::write(run.join("stray.txt"), b"not a process").unwrap();

        let summary = aggregate(&run).unwrap();
        assert_eq!(
            summary.totals,
            Totals {
                processes: 9,
                execution_ok: 2,
                execution_errors: 7,
                pass: 1,
                fail: 1,
                not_run: 1,
            }
        );
        let dirs: Vec<&str> = summary.processes.iter().map(|p| p.dir.as_str()).collect();
        assert_eq!(
            dirs,
            [
                "odd--name",
                "open-a-side",
                "open-a-unified-01",
                "open-a-unified-03",
                "open-b-side-02",
                "smoke",
                "smoke-03",
                "smoke-04",
                "zones-b"
            ]
        );
        let reason = |dir: &str| {
            let process = summary.processes.iter().find(|p| p.dir == dir).unwrap();
            match &process.execution {
                Execution::Error { reason, .. } => reason.clone(),
                Execution::Ok => String::new(),
            }
        };
        assert!(reason("odd--name").contains("не по схеме"));
        assert!(reason("open-a-side").contains("повреждён"));
        let watchdog = reason("open-a-unified-01");
        assert!(watchdog.contains("сторож: срок 60 с истёк"), "{watchdog}");
        assert!(watchdog.contains(WATCHDOG_FILE), "{watchdog}");
        assert_eq!(
            reason("open-a-unified-03"),
            format!(
                "{RESULT_FILE} другого процесса: записан open-b-side-03, каталог open-a-unified-03"
            )
        );
        assert_eq!(reason("open-b-side-02"), "");
        assert!(reason("smoke-03").contains("не стало key"));
        assert!(reason("smoke-04").contains("not_run «ax_voiceover» без причины"));
        assert_eq!(reason("zones-b"), format!("{RESULT_FILE} отсутствует"));
        let zones = summary
            .processes
            .iter()
            .find(|p| p.dir == "zones-b")
            .unwrap();
        assert_eq!(
            (zones.scenario.as_str(), zones.variant, zones.mode),
            ("zones", Some(Variant::B), None)
        );

        let errored = summary
            .processes
            .iter()
            .find(|p| p.dir == "smoke-03")
            .unwrap();
        assert_eq!(errored.findings.len(), 1, "выводы сохранены для разбора");
        let names: Vec<(&str, &str, Verdict)> = summary
            .findings
            .iter()
            .map(|finding| (finding.dir.as_str(), finding.name.as_str(), finding.verdict))
            .collect();
        assert_eq!(
            names,
            [
                ("open-b-side-02", "frames_120hz", Verdict::NotRun),
                ("open-b-side-02", "open_budget", Verdict::Fail),
                ("smoke", "window_ready", Verdict::Pass),
            ],
            "выводы процессов с ошибкой исполнения в итоги не входят"
        );
        assert_eq!(
            summary
                .processes
                .iter()
                .find(|p| p.dir == "smoke")
                .unwrap()
                .metrics[0]
                .summary
                .map(|s| s.max),
            Some(42.5)
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
        assert!(text.starts_with("исполнение: процессов 9, без ошибок 2, с ошибкой 7\n"));
        assert!(text.contains("выводы: pass 1, fail 1, not_run 1\n"));
        assert!(text.contains("  not_run  open-b-side-02 frames_120hz: 120 Гц не подтверждены"));
        assert!(text.contains("  ошибка  open-a-side: "));
        fs::remove_dir_all(run).unwrap();
    }

    #[test]
    fn aggregate_of_missing_run_dir_fails() {
        let run = temp_dir("absent");
        fs::remove_dir_all(&run).unwrap();
        assert!(aggregate(&run).is_err());
    }
}
