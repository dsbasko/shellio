//! Сценарии пробы: процесс на сценарий и точку осей, окружение и итог процесса.

mod units;

use std::path::Path;
use std::process::Command;

use crate::background::{self, QuietOutcome};
use crate::results::{
    self, BackgroundRecord, Build, Cell, Environment, Execution, Hardware, NotRunCause,
    NotStartedRecord, Os, Power, ProcessInfo, Report, ResultFile, Status,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Smoke,
    Units,
}

impl Scenario {
    pub const ALL: [Scenario; 2] = [Scenario::Smoke, Scenario::Units];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Smoke => "smoke",
            Scenario::Units => "units",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL.into_iter().find(|s| s.name() == name)
    }

    /// Замерный сценарий берёт замерную блокировку и проверяет фон.
    pub fn measured(self) -> bool {
        match self {
            Scenario::Smoke | Scenario::Units => true,
        }
    }

    /// Точки оси процесса; пустой список — один процесс без точки.
    pub fn points(self) -> &'static [&'static str] {
        match self {
            Scenario::Smoke | Scenario::Units => &[],
        }
    }
}

/// Итог процесса сценария.
pub enum Outcome {
    Result(Box<ResultFile>),
    /// Тишины не было до срока: код «не начат».
    NotStarted(NotStartedRecord),
}

pub fn run(scenario: Scenario, point: Option<&str>) -> Outcome {
    let environment = environment();
    match point {
        Some(point) if !scenario.points().contains(&point) => {
            return error(
                scenario,
                Some(point),
                environment,
                format!("неизвестная точка «{point}»"),
            );
        }
        None if !scenario.points().is_empty() => {
            return error(
                scenario,
                None,
                environment,
                "сценарию нужна точка --point".to_owned(),
            );
        }
        _ => {}
    }
    match scenario {
        Scenario::Smoke => smoke(environment),
        Scenario::Units => measured(Scenario::Units, environment, units::run),
    }
}

fn error(
    scenario: Scenario,
    point: Option<&str>,
    environment: Environment,
    reason: String,
) -> Outcome {
    Outcome::Result(Box::new(ResultFile::new(
        scenario.name(),
        point,
        environment,
        Execution::error(reason),
        Report::default(),
    )))
}

/// Проверка фона до сценария: окна подряд до тишины или срока 5 минут.
pub fn quiet_or_not_started(
    scenario: Scenario,
    point: Option<&str>,
    environment: &Environment,
) -> Result<Result<Vec<BackgroundRecord>, NotStartedRecord>, String> {
    let outcome = background::wait_for_quiet(
        background::QUIET_DEADLINE,
        || background::measure(background::WINDOW, &[]),
        background::WINDOW,
    )?;
    let label = |checks: Vec<background::BackgroundCheck>| -> Vec<BackgroundRecord> {
        checks
            .into_iter()
            .enumerate()
            .map(|(index, check)| BackgroundRecord {
                label: format!("before:{}", index + 1),
                check,
            })
            .collect()
    };
    Ok(match outcome {
        QuietOutcome::Quiet(checks) => Ok(label(checks)),
        QuietOutcome::Noisy(checks) => Err(NotStartedRecord {
            schema: results::SCHEMA,
            scenario: scenario.name().to_owned(),
            point: point.map(str::to_owned),
            cause: NotRunCause::Noisy,
            reason: format!(
                "тишины фона не было {} с: {} окон проверки",
                background::QUIET_DEADLINE.as_secs(),
                checks.len()
            ),
            background: label(checks),
            attempt: None,
            environment: Some(Box::new(environment.clone())),
        }),
    })
}

/// Замерный сценарий: тихое окно фона, затем тело сценария; отказ тела — ошибка исполнения.
fn measured(
    scenario: Scenario,
    environment: Environment,
    body: impl FnOnce(Vec<BackgroundRecord>) -> Result<Report, String>,
) -> Outcome {
    let records = match quiet_or_not_started(scenario, None, &environment) {
        Err(reason) => return error(scenario, None, environment, reason),
        Ok(Err(not_started)) => return Outcome::NotStarted(not_started),
        Ok(Ok(records)) => records,
    };
    match body(records) {
        Err(reason) => error(scenario, None, environment, reason),
        Ok(report) => Outcome::Result(Box::new(ResultFile::new(
            scenario.name(),
            None,
            environment,
            Execution::Ok,
            report,
        ))),
    }
}

/// Пустой прогон стенда: блокировка, проверка фона, `result.json` и агрегирование.
fn smoke(environment: Environment) -> Outcome {
    let records = match quiet_or_not_started(Scenario::Smoke, None, &environment) {
        Err(reason) => return error(Scenario::Smoke, None, environment, reason),
        Ok(Err(not_started)) => return Outcome::NotStarted(not_started),
        Ok(Ok(records)) => records,
    };
    let mut report = Report::default();
    report.cells.push(smoke_cell(&records));
    report.metrics.push(results::Metric::new(
        "background_pct",
        "% одного ядра",
        records.iter().map(|r| r.check.background_pct).collect(),
    ));
    report.background = records;
    Outcome::Result(Box::new(ResultFile::new(
        Scenario::Smoke.name(),
        None,
        environment,
        Execution::Ok,
        report,
    )))
}

const SMOKE_ORACLE: &str =
    "последнее окно проверки фона тихое, покрытие и системная дельта записаны";

pub fn smoke_cell(records: &[BackgroundRecord]) -> Cell {
    let Some(last) = records.last() else {
        return Cell::new(
            "smoke",
            Status::Fail,
            SMOKE_ORACLE,
            "окон проверки фона нет",
        );
    };
    let check = &last.check;
    let covered = check.coverage.listed_end > 0 && check.system_busy_pct.is_finite();
    let failed: usize = check.coverage.failed_by_errno.values().sum();
    let reason = format!(
        "окон {}, фон {:.2}% одного ядра, система {:.2}%, стенд {:.2}%, самый занятый посторонний {}, перечислено {}, прочитано посторонних {}, отказов чтения {failed}",
        records.len(),
        check.background_pct,
        check.system_busy_pct,
        check.stand_pct,
        check.busiest.as_ref().map_or("нет".to_owned(), |b| format!(
            "{} ({}) {:.2}%",
            b.name, b.pid, b.pct
        )),
        check.coverage.listed_end,
        check.coverage.foreign_read_both,
    );
    let status = if check.quiet && covered {
        Status::Pass
    } else {
        Status::Fail
    };
    Cell::new("smoke", status, SMOKE_ORACLE, reason)
}

fn sysctl(name: &str) -> Option<String> {
    let output = Command::new("/usr/sbin/sysctl")
        .args(["-n", name])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Источник питания из первой строки `pmset -g batt`.
pub fn parse_power_source(batt: &str) -> Option<String> {
    let line = batt.lines().next()?;
    let start = line.find('\'')? + 1;
    let end = start + line[start..].find('\'')?;
    Some(line[start..end].to_owned())
}

/// `lowpowermode` из `pmset -g`.
pub fn parse_low_power(settings: &str) -> Option<bool> {
    settings.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next()? == "lowpowermode").then_some(())?;
        match parts.next()? {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        }
    })
}

pub fn environment() -> Environment {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    Environment {
        os: Os {
            product_version: sysctl("kern.osproductversion"),
            build: sysctl("kern.osversion"),
        },
        hardware: Hardware {
            model: sysctl("hw.model"),
            cpu: sysctl("machdep.cpu.brand_string"),
            memory_bytes: sysctl("hw.memsize").and_then(|v| v.parse().ok()),
            logical_cpus: sysctl("hw.logicalcpu").and_then(|v| v.parse().ok()),
        },
        build: Build {
            commit: env("SHELLIO_PROBE_COMMIT"),
            rustc: env("SHELLIO_PROBE_RUSTC"),
            profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_owned(),
            binary_sha256: env("SHELLIO_PROBE_BINARY_SHA256"),
            probe_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        power: Power {
            source: command_stdout("/usr/bin/pmset", &["-g", "batt"])
                .as_deref()
                .and_then(parse_power_source),
            low_power_mode: command_stdout("/usr/bin/pmset", &["-g"])
                .as_deref()
                .and_then(parse_low_power),
        },
        process: ProcessInfo {
            pid: std::process::id(),
            state_dir: env("SHELLIO_STATE_DIR"),
        },
    }
}

/// Пишет итог процесса и возвращает код выхода: 0 — итог записан, 1 — ошибка исполнения,
/// [`crate::runstate::EXIT_NOT_STARTED`] — «не начат».
pub fn write_outcome(out: &Path, outcome: Outcome) -> Result<i32, String> {
    match outcome {
        Outcome::Result(result) => {
            let problems = result.problems();
            results::write_json(out, results::RESULT_FILE, &result)?;
            if let Execution::Error { reason } = &result.execution {
                return Err(format!("ошибка исполнения: {reason}"));
            }
            if !problems.is_empty() {
                return Err(format!("итог нарушает схему: {}", problems.join("; ")));
            }
            Ok(0)
        }
        Outcome::NotStarted(record) => {
            results::write_json(out, results::NOT_STARTED_FILE, &record)?;
            Ok(crate::runstate::EXIT_NOT_STARTED)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::{BackgroundCheck, Coverage, Foreign, THRESHOLDS, Units};

    #[test]
    fn names_round_trip_and_smoke_is_measured() {
        for scenario in Scenario::ALL {
            assert_eq!(Scenario::from_name(scenario.name()), Some(scenario));
        }
        assert!(Scenario::Smoke.measured() && Scenario::Units.measured());
        assert!(Scenario::Smoke.points().is_empty() && Scenario::Units.points().is_empty());
        assert_eq!(Scenario::from_name("cost"), None);
    }

    #[test]
    fn power_parsers_read_pmset_output() {
        let batt = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1)\t100%; charged\n";
        assert_eq!(parse_power_source(batt).as_deref(), Some("AC Power"));
        assert_eq!(parse_power_source("garbage"), None);
        assert_eq!(
            parse_low_power(" sleep 1\n lowpowermode         1\n"),
            Some(true)
        );
        assert_eq!(parse_low_power(" lowpowermode 0"), Some(false));
        assert_eq!(parse_low_power(" sleep 1"), None);
    }

    fn check(quiet: bool, listed: usize) -> BackgroundRecord {
        BackgroundRecord {
            label: "before:1".to_owned(),
            check: BackgroundCheck {
                window_s: 10.0,
                system_busy_pct: 12.0,
                stand_pct: 1.0,
                background_pct: 11.0,
                busiest: Some(Foreign {
                    pid: 7,
                    name: "zig".to_owned(),
                    pct: 3.0,
                }),
                stand_pids: vec![1],
                coverage: Coverage {
                    listed_end: listed,
                    ..Coverage::default()
                },
                thresholds: THRESHOLDS,
                units: Units {
                    mach_numer: 125,
                    mach_denom: 3,
                    clk_tck: 100,
                },
                quiet,
            },
        }
    }

    #[test]
    fn smoke_cell_requires_quiet_covered_window() {
        assert_eq!(smoke_cell(&[check(true, 500)]).status, Status::Pass);
        assert_eq!(smoke_cell(&[check(true, 0)]).status, Status::Fail);
        assert_eq!(smoke_cell(&[check(false, 500)]).status, Status::Fail);
        assert_eq!(smoke_cell(&[]).status, Status::Fail);
        assert!(
            smoke_cell(&[check(true, 500)])
                .reason
                .contains("zig (7) 3.00%")
        );
    }

    #[test]
    fn outcome_codes_separate_error_and_not_started() {
        let dir =
            std::env::temp_dir().join(format!("probe-resources-outcome-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let not_started = Outcome::NotStarted(NotStartedRecord {
            schema: results::SCHEMA,
            scenario: "smoke".to_owned(),
            point: None,
            cause: NotRunCause::Noisy,
            reason: "шумно".to_owned(),
            background: Vec::new(),
            attempt: None,
            environment: None,
        });
        assert_eq!(
            write_outcome(&dir, not_started).unwrap(),
            crate::runstate::EXIT_NOT_STARTED
        );
        assert!(dir.join(results::NOT_STARTED_FILE).exists());
        let failed = error(
            Scenario::Smoke,
            None,
            Environment::default(),
            "упал".to_owned(),
        );
        assert!(write_outcome(&dir, failed).is_err());
        assert!(dir.join(results::RESULT_FILE).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
