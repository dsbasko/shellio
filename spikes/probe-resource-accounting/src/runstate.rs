//! Состояние длинного прогона (план P0.8, «Длинный прогон» и «Замерная блокировка»):
//! `status` по процессам, сверка сборки под одним run-id и счёт «не начат» подряд.
//!
//! `run.sh` вызывает эти правила через режимы бинаря, чтобы их проверяли тесты `make check`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::results::{
    self, BackgroundRecord, Cell, Environment, Execution, NotRunCause, NotStartedRecord, Report,
    ResultFile,
};

pub const STATUS_FILE: &str = "status";
pub const BUILD_FILE: &str = "build.json";
/// Третий подряд «не начат» одного процесса даёт `not_run`.
pub const NOT_STARTED_LIMIT: u32 = 3;
/// Код выхода процесса и цели, который не дождался блокировки или тишины.
pub const EXIT_NOT_STARTED: i32 = 3;
/// Код выхода сверки сборки при расхождении: нужен новый run-id.
pub const EXIT_BUILD_MISMATCH: i32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnitState {
    Done,
    NotStarted,
    Error,
}

impl UnitState {
    pub fn name(self) -> &'static str {
        match self {
            UnitState::Done => "done",
            UnitState::NotStarted => "not-started",
            UnitState::Error => "error",
        }
    }

    pub fn from_name(name: &str) -> Option<UnitState> {
        match name {
            "done" => Some(UnitState::Done),
            "not-started" => Some(UnitState::NotStarted),
            "error" => Some(UnitState::Error),
            _ => None,
        }
    }
}

/// Строки `<процесс> <состояние>`; последняя запись процесса побеждает.
pub fn read_status(run_dir: &Path) -> Result<BTreeMap<String, UnitState>, String> {
    let path = run_dir.join(STATUS_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => return Err(format!("чтение {}: {err}", path.display())),
    };
    let mut states = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(unit), Some(state), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(format!("{}:{}: строка «{line}»", path.display(), index + 1));
        };
        let state = UnitState::from_name(state)
            .ok_or_else(|| format!("{}:{}: состояние «{state}»", path.display(), index + 1))?;
        states.insert(unit.to_owned(), state);
    }
    Ok(states)
}

pub fn set_status(run_dir: &Path, unit: &str, state: UnitState) -> Result<(), String> {
    if unit.is_empty() || unit.contains(char::is_whitespace) {
        return Err(format!("имя процесса «{unit}» недопустимо"));
    }
    let mut text = match fs::read_to_string(run_dir.join(STATUS_FILE)) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("чтение status: {err}")),
    };
    text.push_str(&format!("{unit} {}\n", state.name()));
    results::write_atomic(run_dir, STATUS_FILE, text.as_bytes()).map(|_| ())
}

/// Процессы из заказанных, которые ещё не завершены: продолжение прогона идёт с них.
pub fn pending_units<'a>(
    states: &BTreeMap<String, UnitState>,
    ordered: &[&'a str],
) -> Vec<&'a str> {
    ordered
        .iter()
        .copied()
        .filter(|unit| states.get(*unit) != Some(&UnitState::Done))
        .collect()
}

/// Сборка и параметры, под которыми идут процессы одного run-id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildFingerprint {
    pub binary_sha256: String,
    pub profile: String,
    /// Параметры протокола из `--params`.
    pub params: serde_json::Value,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FingerprintCheck {
    Recorded,
    Same,
    /// Описание расхождений: результаты разных сборок под одним run-id не смешиваются.
    Mismatch(Vec<String>),
}

pub fn check_fingerprint(
    run_dir: &Path,
    current: &BuildFingerprint,
) -> Result<FingerprintCheck, String> {
    match results::read_json::<BuildFingerprint>(&run_dir.join(BUILD_FILE)) {
        Err(results::ReadError::Missing) => {
            results::write_json(run_dir, BUILD_FILE, current)?;
            Ok(FingerprintCheck::Recorded)
        }
        Err(results::ReadError::Corrupt(err) | results::ReadError::Io(err)) => Err(err),
        Ok(saved) if &saved == current => Ok(FingerprintCheck::Same),
        Ok(saved) => {
            let mut diff = Vec::new();
            if saved.binary_sha256 != current.binary_sha256 {
                diff.push(format!(
                    "хеш бинаря {} → {}",
                    saved.binary_sha256, current.binary_sha256
                ));
            }
            if saved.profile != current.profile {
                diff.push(format!("профиль {} → {}", saved.profile, current.profile));
            }
            if saved.params != current.params {
                diff.push("параметры протокола".to_owned());
            }
            Ok(FingerprintCheck::Mismatch(diff))
        }
    }
}

/// Исход попытки процесса для счёта «не начат» подряд.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attempt {
    Started,
    LockBusy,
    Noisy,
}

impl Attempt {
    pub fn from_name(name: &str) -> Option<Attempt> {
        match name {
            "started" => Some(Attempt::Started),
            "lock-busy" => Some(Attempt::LockBusy),
            "noisy" => Some(Attempt::Noisy),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Counter {
    count: u32,
    causes: Vec<NotRunCause>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Настоящий старт: счёт сброшен.
    Reset,
    /// «Не начат» `n` раз подряд из трёх: повтор следующим вызовом.
    Retry(u32),
    /// Третий подряд: записан `not_run`.
    NotRun(NotRunCause),
}

fn counter_path(counter_dir: &Path, unit: &str) -> PathBuf {
    counter_dir.join(format!("{unit}.json"))
}

fn attempt_file(unit: &str, attempt: u32) -> String {
    format!("{unit}-attempt-{attempt}.json")
}

/// Номер попытки из имени копии записи `<процесс>-attempt-<n>.json`.
fn attempt_of(unit: &str, file: &str) -> Option<u32> {
    file.strip_prefix(unit)?
        .strip_prefix("-attempt-")?
        .strip_suffix(".json")?
        .parse()
        .ok()
}

/// Стирает счёт процесса и копии записей его попыток.
fn clear_counter(counter_dir: &Path, unit: &str) -> Result<(), String> {
    let entries = match fs::read_dir(counter_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("чтение {}: {err}", counter_dir.display())),
    };
    let counter = format!("{unit}.json");
    for entry in entries {
        let entry = entry.map_err(|err| format!("чтение {}: {err}", counter_dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == counter || attempt_of(unit, &name).is_some() {
            let path = entry.path();
            fs::remove_file(&path).map_err(|err| format!("удаление {}: {err}", path.display()))?;
        }
    }
    Ok(())
}

/// Процесс, чья попытка учитывается.
pub struct AttemptContext<'a> {
    /// Каталог счёта вне каталога прогона: каждый вызов без `RUN` заводит новый каталог прогона.
    pub counter_dir: &'a Path,
    pub unit: &'a str,
    /// Каталог процесса в прогоне.
    pub out: &'a Path,
    pub scenario: &'a str,
    pub point: Option<&'a str>,
    /// Окружение вызова учёта: подставляется, когда процесс сценария не записал своё.
    pub environment: Environment,
}

/// Запись попытки «не начат». Процесс, не дождавшийся тишины, пишет её сам вместе с окнами
/// фона; при занятой блокировке процесс сценария не запускался, и запись заводится здесь.
fn attempt_record(
    ctx: &AttemptContext,
    attempt: Attempt,
    number: u32,
) -> Result<NotStartedRecord, String> {
    let path = ctx.out.join(results::NOT_STARTED_FILE);
    let mut record = match attempt {
        Attempt::Noisy => match results::read_json::<NotStartedRecord>(&path) {
            Ok(record) => record,
            Err(results::ReadError::Missing) => {
                return Err(format!(
                    "процесс вернул «не начат», но не записал {}: окна фона попытки потеряны",
                    path.display()
                ));
            }
            Err(results::ReadError::Corrupt(err) | results::ReadError::Io(err)) => {
                return Err(err);
            }
        },
        Attempt::Started => {
            return Err("начатая попытка не заводит запись «не начат»".to_owned());
        }
        Attempt::LockBusy => NotStartedRecord {
            schema: results::SCHEMA,
            scenario: ctx.scenario.to_owned(),
            point: ctx.point.map(str::to_owned),
            cause: NotRunCause::LockBusy,
            reason: format!(
                "{}: процесс сценария не запускался",
                NotRunCause::LockBusy.label()
            ),
            background: Vec::new(),
            attempt: None,
            environment: None,
        },
    };
    record.attempt = Some(number);
    if record.environment.is_none() {
        record.environment = Some(Box::new(ctx.environment.clone()));
    }
    Ok(record)
}

/// Ведёт счёт «не начат» вне каталога прогона: счёт относится к процессу сценария или точки, а
/// не к прогону, и переживает смену run-id. Копия записи каждой попытки хранится рядом со
/// счётом, поэтому `not_run` третьей попытки несёт окна фона и окружение всех трёх, даже если
/// прежние попытки шли под другим run-id.
pub fn record_attempt(ctx: &AttemptContext, attempt: Attempt) -> Result<Decision, String> {
    let cause = match attempt {
        Attempt::Started => {
            clear_counter(ctx.counter_dir, ctx.unit)?;
            return Ok(Decision::Reset);
        }
        Attempt::LockBusy => NotRunCause::LockBusy,
        Attempt::Noisy => NotRunCause::Noisy,
    };
    fs::create_dir_all(ctx.counter_dir)
        .map_err(|err| format!("создание {}: {err}", ctx.counter_dir.display()))?;
    let path = counter_path(ctx.counter_dir, ctx.unit);
    let mut counter = match results::read_json::<Counter>(&path) {
        Ok(counter) => counter,
        Err(results::ReadError::Missing) => Counter::default(),
        Err(results::ReadError::Corrupt(err) | results::ReadError::Io(err)) => return Err(err),
    };
    counter.count += 1;
    counter.causes.push(cause);
    let record = attempt_record(ctx, attempt, counter.count)?;
    results::write_json(ctx.out, results::NOT_STARTED_FILE, &record)?;
    results::write_json(
        ctx.counter_dir,
        &attempt_file(ctx.unit, counter.count),
        &record,
    )?;
    if counter.count < NOT_STARTED_LIMIT {
        results::write_json(ctx.counter_dir, &format!("{}.json", ctx.unit), &counter)?;
        return Ok(Decision::Retry(counter.count));
    }
    let result = not_run_result(ctx, &counter, cause, record)?;
    results::write_json(ctx.out, results::RESULT_FILE, &result)?;
    clear_counter(ctx.counter_dir, ctx.unit)?;
    Ok(Decision::NotRun(cause))
}

/// `not_run` после третьей попытки подряд: окна фона всех попыток с меткой `attempt-<n>/`,
/// окружение последней, сами попытки — в `raw.attempts`.
fn not_run_result(
    ctx: &AttemptContext,
    counter: &Counter,
    cause: NotRunCause,
    last: NotStartedRecord,
) -> Result<ResultFile, String> {
    let mut background = Vec::new();
    let mut attempts = Vec::new();
    let mut notes = Vec::new();
    for (index, attempt_cause) in counter.causes.iter().enumerate() {
        let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let path = ctx.counter_dir.join(attempt_file(ctx.unit, number));
        let record = match results::read_json::<NotStartedRecord>(&path) {
            Ok(record) => record,
            Err(results::ReadError::Missing) => {
                notes.push(format!(
                    "попытка {number} ({}): копии записи в счёте попыток нет, запись осталась только в каталоге процесса своего прогона",
                    attempt_cause.label()
                ));
                attempts.push(serde_json::json!({
                    "attempt": number,
                    "cause": attempt_cause,
                    "record": serde_json::Value::Null,
                }));
                continue;
            }
            Err(results::ReadError::Corrupt(err) | results::ReadError::Io(err)) => {
                return Err(err);
            }
        };
        attempts.push(serde_json::json!({
            "attempt": number,
            "cause": attempt_cause,
            "reason": record.reason,
            "windows": record.background.len(),
            "environment": record.environment,
        }));
        background.extend(
            record
                .background
                .into_iter()
                .map(|window| BackgroundRecord {
                    label: format!("attempt-{number}/{}", window.label),
                    check: window.check,
                }),
        );
    }
    let causes: Vec<&str> = counter.causes.iter().map(|c| c.label()).collect();
    let reason = format!(
        "{} попытки подряд не начаты: {}; окон проверки фона {}",
        counter.count,
        causes.join(", "),
        background.len()
    );
    let cell_id = ctx.point.map_or_else(
        || ctx.scenario.to_owned(),
        |p| format!("{}/{p}", ctx.scenario),
    );
    let mut raw = BTreeMap::new();
    raw.insert("attempts".to_owned(), serde_json::Value::Array(attempts));
    Ok(ResultFile::new(
        ctx.scenario,
        ctx.point,
        last.environment
            .map_or_else(|| ctx.environment.clone(), |environment| *environment),
        Execution::Ok,
        Report {
            cells: vec![Cell::not_run(
                &cell_id,
                cause,
                "замерная блокировка и проверка фона",
                reason,
            )],
            background,
            raw,
            notes,
            ..Report::default()
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "probe-resources-runstate-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn status_resume_skips_done_units_only() {
        let run = temp_dir("status");
        set_status(&run, "cost-pids8", UnitState::Done).unwrap();
        set_status(&run, "cost-pids32", UnitState::NotStarted).unwrap();
        set_status(&run, "cost-pids128", UnitState::Error).unwrap();
        set_status(&run, "cost-pids32", UnitState::Done).unwrap();
        let states = read_status(&run).unwrap();
        let ordered = ["cost-pids8", "cost-pids32", "cost-pids128", "cost-idle"];
        assert_eq!(
            pending_units(&states, &ordered),
            vec!["cost-pids128", "cost-idle"]
        );
        assert!(set_status(&run, "a b", UnitState::Done).is_err());
        fs::write(run.join(STATUS_FILE), "x bogus\n").unwrap();
        assert!(read_status(&run).is_err());
        fs::remove_dir_all(&run).unwrap();
    }

    fn fingerprint(sha: &str, profile: &str, window: u64) -> BuildFingerprint {
        BuildFingerprint {
            binary_sha256: sha.to_owned(),
            profile: profile.to_owned(),
            params: serde_json::json!({ "window_s": window }),
        }
    }

    #[test]
    fn builds_are_not_mixed_under_one_run_id() {
        let run = temp_dir("fingerprint");
        let first = fingerprint("aa", "release", 30);
        assert_eq!(
            check_fingerprint(&run, &first).unwrap(),
            FingerprintCheck::Recorded
        );
        assert_eq!(
            check_fingerprint(&run, &first).unwrap(),
            FingerprintCheck::Same
        );
        let FingerprintCheck::Mismatch(diff) =
            check_fingerprint(&run, &fingerprint("bb", "debug", 31)).unwrap()
        else {
            panic!("ожидалось расхождение");
        };
        assert_eq!(diff.len(), 3, "{diff:?}");
        let FingerprintCheck::Mismatch(diff) =
            check_fingerprint(&run, &fingerprint("aa", "release", 31)).unwrap()
        else {
            panic!("ожидалось расхождение параметров");
        };
        assert_eq!(diff, vec!["параметры протокола".to_owned()]);
        fs::remove_dir_all(&run).unwrap();
    }

    fn env(model: &str) -> Environment {
        let mut environment = Environment::default();
        environment.hardware.model = Some(model.to_owned());
        environment.build.profile = "release".to_owned();
        environment
    }

    fn window(label: &str, background_pct: f64) -> BackgroundRecord {
        use crate::background::{BackgroundCheck, Coverage, THRESHOLDS, Units};
        BackgroundRecord {
            label: label.to_owned(),
            check: BackgroundCheck {
                window_s: 10.0,
                system_busy_pct: background_pct + 1.0,
                stand_pct: 1.0,
                background_pct,
                busiest: None,
                stand_pids: vec![1],
                coverage: Coverage {
                    listed_end: 790,
                    ..Coverage::default()
                },
                thresholds: THRESHOLDS,
                units: Units {
                    mach_numer: 125,
                    mach_denom: 3,
                    clk_tck: 100,
                },
                quiet: false,
            },
        }
    }

    /// Подставная запись процесса сценария, который не дождался тишины.
    fn process_record(out: &Path, model: &str, pcts: &[f64]) {
        let record = NotStartedRecord {
            schema: results::SCHEMA,
            scenario: "smoke".to_owned(),
            point: None,
            cause: NotRunCause::Noisy,
            reason: format!("тишины фона не было: {} окон", pcts.len()),
            background: pcts
                .iter()
                .enumerate()
                .map(|(i, pct)| window(&format!("before:{}", i + 1), *pct))
                .collect(),
            attempt: None,
            environment: Some(Box::new(env(model))),
        };
        results::write_json(out, results::NOT_STARTED_FILE, &record).unwrap();
    }

    fn ctx<'a>(counters: &'a Path, out: &'a Path, point: Option<&'a str>) -> AttemptContext<'a> {
        AttemptContext {
            counter_dir: counters,
            unit: point.map_or("smoke", |_| "cost-idle"),
            out,
            scenario: point.map_or("smoke", |_| "cost"),
            point,
            environment: env("record-call"),
        }
    }

    #[test]
    fn third_not_started_in_a_row_becomes_not_run_with_evidence() {
        let base = temp_dir("counter");
        let counters = base.join("not-started");
        let out = base.join("smoke");
        fs::create_dir_all(&out).unwrap();
        let context = ctx(&counters, &out, None);

        process_record(&out, "attempt-1", &[90.0, 117.0]);
        assert_eq!(
            record_attempt(&context, Attempt::Noisy).unwrap(),
            Decision::Retry(1)
        );
        let first: NotStartedRecord =
            results::read_json(&out.join(results::NOT_STARTED_FILE)).unwrap();
        assert_eq!(first.attempt, Some(1));
        assert_eq!(first.background.len(), 2);

        // Занятая блокировка: устаревшая запись прежней попытки не наследуется.
        assert_eq!(
            record_attempt(&context, Attempt::LockBusy).unwrap(),
            Decision::Retry(2)
        );
        let second: NotStartedRecord =
            results::read_json(&out.join(results::NOT_STARTED_FILE)).unwrap();
        assert_eq!(second.attempt, Some(2));
        assert_eq!(second.cause, NotRunCause::LockBusy);
        assert!(second.background.is_empty());
        assert_eq!(second.environment, Some(Box::new(env("record-call"))));

        process_record(&out, "attempt-3", &[80.0, 548.0, 99.0]);
        assert_eq!(
            record_attempt(&context, Attempt::Noisy).unwrap(),
            Decision::NotRun(NotRunCause::Noisy)
        );
        let result: ResultFile = results::read_json(&out.join(results::RESULT_FILE)).unwrap();
        assert!(result.problems().is_empty(), "{:?}", result.problems());
        assert_eq!(result.environment, env("attempt-3"));
        assert_eq!(result.cells[0].not_run_cause, Some(NotRunCause::Noisy));
        assert!(
            result.cells[0]
                .reason
                .contains("шумно, замерная блокировка занята, шумно; окон проверки фона 5"),
            "{}",
            result.cells[0].reason
        );
        let labels: Vec<&str> = result.background.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "attempt-1/before:1",
                "attempt-1/before:2",
                "attempt-3/before:1",
                "attempt-3/before:2",
                "attempt-3/before:3",
            ]
        );
        assert_eq!(result.background[3].check.background_pct, 548.0);
        let attempts = result.raw["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 3);
        assert_eq!(attempts[1]["cause"], "lock_busy");
        assert_eq!(attempts[0]["environment"]["hardware"]["model"], "attempt-1");
        assert!(result.notes.is_empty(), "{:?}", result.notes);
        assert_eq!(fs::read_dir(&counters).unwrap().count(), 0);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn counter_from_before_evidence_still_loads_and_says_what_is_missing() {
        let base = temp_dir("legacy");
        let counters = base.join("not-started");
        let out = base.join("smoke");
        fs::create_dir_all(&counters).unwrap();
        fs::create_dir_all(&out).unwrap();
        fs::write(
            counters.join("smoke.json"),
            r#"{"count":1,"causes":["noisy"]}"#,
        )
        .unwrap();
        let context = ctx(&counters, &out, None);
        process_record(&out, "attempt-2", &[100.0]);
        assert_eq!(
            record_attempt(&context, Attempt::Noisy).unwrap(),
            Decision::Retry(2)
        );
        process_record(&out, "attempt-3", &[110.0]);
        assert_eq!(
            record_attempt(&context, Attempt::Noisy).unwrap(),
            Decision::NotRun(NotRunCause::Noisy)
        );
        let result: ResultFile = results::read_json(&out.join(results::RESULT_FILE)).unwrap();
        assert_eq!(result.background.len(), 2);
        assert_eq!(result.background[0].label, "attempt-2/before:1");
        assert_eq!(result.notes.len(), 1);
        assert!(result.notes[0].starts_with("попытка 1 (шумно)"));
        assert!(result.raw["attempts"][0]["record"].is_null());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn noisy_attempt_without_process_record_is_an_error() {
        let base = temp_dir("lost");
        let counters = base.join("not-started");
        let out = base.join("smoke");
        fs::create_dir_all(&out).unwrap();
        let err = record_attempt(&ctx(&counters, &out, None), Attempt::Noisy).unwrap_err();
        assert!(err.contains("окна фона попытки потеряны"), "{err}");
        assert!(!counters.join("smoke.json").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn real_start_resets_the_count_and_evidence() {
        let base = temp_dir("reset");
        let counters = base.join("not-started");
        let out = base.join("cost-idle");
        fs::create_dir_all(&out).unwrap();
        let context = ctx(&counters, &out, Some("idle"));
        assert_eq!(
            record_attempt(&context, Attempt::LockBusy).unwrap(),
            Decision::Retry(1)
        );
        assert_eq!(
            record_attempt(&context, Attempt::LockBusy).unwrap(),
            Decision::Retry(2)
        );
        let other = base.join("cost-idle-attempt-x.json");
        fs::write(&other, "{}").unwrap();
        assert!(counters.join("cost-idle-attempt-2.json").exists());
        assert_eq!(
            record_attempt(&context, Attempt::Started).unwrap(),
            Decision::Reset
        );
        assert_eq!(fs::read_dir(&counters).unwrap().count(), 0);
        assert!(other.exists());
        assert_eq!(
            record_attempt(&context, Attempt::LockBusy).unwrap(),
            Decision::Retry(1)
        );
        assert!(!out.join(results::RESULT_FILE).exists());
        assert_eq!(
            attempt_of("cost-idle", "cost-idle-attempt-12.json"),
            Some(12)
        );
        assert_eq!(attempt_of("cost-pids3", "cost-pids32-attempt-1.json"), None);
        assert_eq!(attempt_of("cost-idle", "cost-idle.json"), None);
        fs::remove_dir_all(&base).unwrap();
    }
}
