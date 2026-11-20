//! Состояние длинного прогона (план P0.8, «Длинный прогон» и «Замерная блокировка»):
//! `status` по процессам, сверка сборки под одним run-id и счёт «не начат» подряд.
//!
//! `run.sh` вызывает эти правила через режимы бинаря, чтобы их проверяли тесты `make check`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::results::{
    self, Cell, Environment, Execution, NotRunCause, NotStartedRecord, Report, ResultFile,
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

/// Ведёт счёт «не начат» вне каталога прогона: каждый вызов без `RUN` заводит новый каталог, а
/// счёт относится к процессу сценария или точки. На третьем подряд пишет `not_run` в `out`.
pub fn record_attempt(
    counter_dir: &Path,
    unit: &str,
    attempt: Attempt,
    out: &Path,
    scenario: &str,
    point: Option<&str>,
) -> Result<Decision, String> {
    let path = counter_path(counter_dir, unit);
    let cause = match attempt {
        Attempt::Started => {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(format!("удаление {}: {err}", path.display())),
            }
            return Ok(Decision::Reset);
        }
        Attempt::LockBusy => NotRunCause::LockBusy,
        Attempt::Noisy => NotRunCause::Noisy,
    };
    fs::create_dir_all(counter_dir)
        .map_err(|err| format!("создание {}: {err}", counter_dir.display()))?;
    let mut counter = match results::read_json::<Counter>(&path) {
        Ok(counter) => counter,
        Err(results::ReadError::Missing) => Counter::default(),
        Err(results::ReadError::Corrupt(err) | results::ReadError::Io(err)) => return Err(err),
    };
    counter.count += 1;
    counter.causes.push(cause);
    if counter.count < NOT_STARTED_LIMIT {
        results::write_json(counter_dir, &format!("{unit}.json"), &counter)?;
        if !out.join(results::NOT_STARTED_FILE).exists() {
            let record = NotStartedRecord {
                schema: results::SCHEMA,
                scenario: scenario.to_owned(),
                point: point.map(str::to_owned),
                cause,
                reason: format!(
                    "{}, попытка {} из {NOT_STARTED_LIMIT}",
                    cause.label(),
                    counter.count
                ),
                background: Vec::new(),
            };
            results::write_json(out, results::NOT_STARTED_FILE, &record)?;
        }
        return Ok(Decision::Retry(counter.count));
    }
    let causes: Vec<&str> = counter.causes.iter().map(|c| c.label()).collect();
    let reason = format!(
        "{NOT_STARTED_LIMIT} попытки подряд не начаты: {}",
        causes.join(", ")
    );
    let cell_id = point.map_or_else(|| scenario.to_owned(), |p| format!("{scenario}/{p}"));
    let result = ResultFile::new(
        scenario,
        point,
        Environment::default(),
        Execution::Ok,
        Report {
            cells: vec![Cell::not_run(
                &cell_id,
                cause,
                "замерная блокировка и проверка фона",
                reason,
            )],
            ..Report::default()
        },
    );
    results::write_json(out, results::RESULT_FILE, &result)?;
    fs::remove_file(&path).map_err(|err| format!("удаление {}: {err}", path.display()))?;
    Ok(Decision::NotRun(cause))
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

    #[test]
    fn third_not_started_in_a_row_becomes_not_run() {
        let base = temp_dir("counter");
        let counters = base.join("not-started");
        let out = base.join("smoke");
        fs::create_dir_all(&out).unwrap();
        assert_eq!(
            record_attempt(&counters, "smoke", Attempt::Noisy, &out, "smoke", None).unwrap(),
            Decision::Retry(1)
        );
        assert!(out.join(results::NOT_STARTED_FILE).exists());
        assert_eq!(
            record_attempt(&counters, "smoke", Attempt::LockBusy, &out, "smoke", None).unwrap(),
            Decision::Retry(2)
        );
        assert_eq!(
            record_attempt(&counters, "smoke", Attempt::Noisy, &out, "smoke", None).unwrap(),
            Decision::NotRun(NotRunCause::Noisy)
        );
        let result: ResultFile = results::read_json(&out.join(results::RESULT_FILE)).unwrap();
        assert!(result.problems().is_empty(), "{:?}", result.problems());
        assert_eq!(result.cells[0].not_run_cause, Some(NotRunCause::Noisy));
        assert!(
            result.cells[0]
                .reason
                .contains("шумно, замерная блокировка занята, шумно")
        );
        assert!(!counters.join("smoke.json").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn real_start_resets_the_count() {
        let base = temp_dir("reset");
        let counters = base.join("not-started");
        let out = base.join("cost-idle");
        fs::create_dir_all(&out).unwrap();
        let record = |attempt| {
            record_attempt(&counters, "cost-idle", attempt, &out, "cost", Some("idle")).unwrap()
        };
        assert_eq!(record(Attempt::LockBusy), Decision::Retry(1));
        assert_eq!(record(Attempt::LockBusy), Decision::Retry(2));
        assert_eq!(record(Attempt::Started), Decision::Reset);
        assert_eq!(record(Attempt::LockBusy), Decision::Retry(1));
        assert!(!out.join(results::RESULT_FILE).exists());
        fs::remove_dir_all(&base).unwrap();
    }
}
