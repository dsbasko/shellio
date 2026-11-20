//! Проба P0.8 `probe-resource-accounting`: учёт CPU и памяти (ADR-0011, ADR-0034).
//!
//! Режимы:
//! - `--list` — сценарии: имя, `measured` или `free`, точки оси;
//! - `--scenario <имя> [--point <точка>] --out <каталог>` — процесс сценария;
//! - `--fixture <режим> --channel <сокет> --name <имя> --out <каталог> …` — фикстура процессов;
//! - `--observer` — наблюдатель стоимости (задача 11);
//! - `--aggregate <каталог прогона>` — `summary.json` и `summary.md`;
//! - служебные режимы `run.sh`: `--params`, `--fingerprint`, `--status-set`, `--status-pending`,
//!   `--record`.

mod arena;
mod attrib;
mod background;
mod fixture;
mod proc;
mod results;
mod runstate;
// Сэмплер до сценария `cost` задачи 11 работает только в тестах.
#[cfg_attr(not(test), expect(dead_code))]
mod sampler;
mod scenario;
mod snapshot;
mod spans;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use scenario::Scenario;

/// Параметры протокола, которые сверяются при продолжении прогона под тем же run-id.
fn params() -> serde_json::Value {
    serde_json::json!({
        "schema": results::SCHEMA,
        "background_window_s": background::WINDOW.as_secs(),
        "quiet_deadline_s": background::QUIET_DEADLINE.as_secs(),
        "background_system_pct": background::THRESHOLDS.system_pct,
        "background_foreign_pct": background::THRESHOLDS.foreign_pct,
        "not_started_limit": runstate::NOT_STARTED_LIMIT,
        "scenarios": Scenario::ALL
            .iter()
            .map(|s| serde_json::json!({ "name": s.name(), "measured": s.measured(), "points": s.points() }))
            .collect::<Vec<_>>(),
    })
}

struct Args {
    rest: Vec<String>,
}

impl Args {
    fn value(&mut self, flag: &str) -> Result<Option<String>, String> {
        let Some(index) = self.rest.iter().position(|a| a == flag) else {
            return Ok(None);
        };
        if index + 1 >= self.rest.len() {
            return Err(format!("{flag}: нужно значение"));
        }
        let value = self.rest.remove(index + 1);
        self.rest.remove(index);
        Ok(Some(value))
    }

    fn required(&mut self, flag: &str) -> Result<String, String> {
        self.value(flag)?.ok_or_else(|| format!("нужен {flag}"))
    }

    fn finish(self) -> Result<(), String> {
        match self.rest.first() {
            None => Ok(()),
            Some(extra) => Err(format!("лишний аргумент «{extra}»")),
        }
    }
}

fn run_scenario(mut args: Args, name: &str) -> Result<i32, String> {
    let point = args.value("--point")?;
    let out = PathBuf::from(args.required("--out")?);
    args.finish()?;
    let scenario =
        Scenario::from_name(name).ok_or_else(|| format!("неизвестный сценарий «{name}»"))?;
    std::fs::create_dir_all(&out).map_err(|err| format!("создание {}: {err}", out.display()))?;
    scenario::write_outcome(&out, scenario::run(scenario, point.as_deref(), &out))
}

fn aggregate(run_dir: &Path) -> Result<i32, String> {
    let summary = results::aggregate(run_dir)?;
    results::write_summary(run_dir, &summary)?;
    print!("{}", results::summary_markdown(&summary));
    for error in &summary.errors {
        eprintln!("ошибка исполнения: {error}");
    }
    Ok(if summary.failed() { 1 } else { 0 })
}

fn fingerprint(mut args: Args, run_dir: &str) -> Result<i32, String> {
    let current = runstate::BuildFingerprint {
        binary_sha256: args.required("--sha256")?,
        profile: args.required("--profile")?,
        params: params(),
    };
    args.finish()?;
    match runstate::check_fingerprint(Path::new(run_dir), &current)? {
        runstate::FingerprintCheck::Recorded | runstate::FingerprintCheck::Same => Ok(0),
        runstate::FingerprintCheck::Mismatch(diff) => {
            eprintln!(
                "сборка расходится с прогоном {run_dir}: {}",
                diff.join(", ")
            );
            Ok(runstate::EXIT_BUILD_MISMATCH)
        }
    }
}

fn record(mut args: Args, counter_dir: &str) -> Result<i32, String> {
    let unit = args.required("--unit")?;
    let attempt = args.required("--attempt")?;
    let attempt = runstate::Attempt::from_name(&attempt)
        .ok_or_else(|| format!("неизвестный исход попытки «{attempt}»"))?;
    let out = PathBuf::from(args.required("--out")?);
    let scenario = args.required("--scenario")?;
    let point = args.value("--point")?;
    args.finish()?;
    let context = runstate::AttemptContext {
        counter_dir: Path::new(counter_dir),
        unit: &unit,
        out: &out,
        scenario: &scenario,
        point: point.as_deref(),
        environment: scenario::environment(),
    };
    let decision = runstate::record_attempt(&context, attempt)?;
    Ok(match decision {
        runstate::Decision::Reset => 0,
        runstate::Decision::Retry(count) => {
            println!("retry {count}/{}", runstate::NOT_STARTED_LIMIT);
            runstate::EXIT_NOT_STARTED
        }
        runstate::Decision::NotRun(cause) => {
            println!("not_run {}", cause.label());
            0
        }
    })
}

fn dispatch(mut rest: Vec<String>) -> Result<i32, String> {
    if rest.is_empty() {
        return Err(
            "нужен режим: --list, --scenario, --fixture, --observer, --aggregate".to_owned(),
        );
    }
    let mode = rest.remove(0);
    let mut args = Args { rest };
    match mode.as_str() {
        "--list" => {
            args.finish()?;
            for scenario in Scenario::ALL {
                let kind = if scenario.measured() {
                    "measured"
                } else {
                    "free"
                };
                let mut line = format!("{} {kind}", scenario.name());
                for point in scenario.points() {
                    line.push(' ');
                    line.push_str(point);
                }
                println!("{line}");
            }
            Ok(0)
        }
        "--params" => {
            args.finish()?;
            println!("{}", params());
            Ok(0)
        }
        "--scenario" => {
            let name = take_positional(&mut args, "--scenario")?;
            run_scenario(args, &name)
        }
        "--fixture" => Ok(fixture::main(args.rest)),
        "--observer" => Err("наблюдатель появляется в задаче 11".to_owned()),
        "--aggregate" => {
            let run_dir = take_positional(&mut args, "--aggregate")?;
            args.finish()?;
            aggregate(Path::new(&run_dir))
        }
        "--fingerprint" => {
            let run_dir = take_positional(&mut args, "--fingerprint")?;
            fingerprint(args, &run_dir)
        }
        "--status-set" => {
            let run_dir = take_positional(&mut args, "--status-set")?;
            let unit = take_positional(&mut args, "--status-set")?;
            let state = take_positional(&mut args, "--status-set")?;
            args.finish()?;
            let state = runstate::UnitState::from_name(&state)
                .ok_or_else(|| format!("неизвестное состояние «{state}»"))?;
            runstate::set_status(Path::new(&run_dir), &unit, state)?;
            Ok(0)
        }
        "--status-pending" => {
            let run_dir = take_positional(&mut args, "--status-pending")?;
            let states = runstate::read_status(Path::new(&run_dir))?;
            let ordered: Vec<&str> = args.rest.iter().map(String::as_str).collect();
            for unit in runstate::pending_units(&states, &ordered) {
                println!("{unit}");
            }
            Ok(0)
        }
        "--record" => {
            let counter_dir = take_positional(&mut args, "--record")?;
            record(args, &counter_dir)
        }
        other => Err(format!("неизвестный режим «{other}»")),
    }
}

fn take_positional(args: &mut Args, flag: &str) -> Result<String, String> {
    if args.rest.is_empty() || args.rest[0].starts_with("--") {
        return Err(format!("{flag}: нужно значение"));
    }
    Ok(args.rest.remove(0))
}

fn main() -> ExitCode {
    match dispatch(std::env::args().skip(1).collect()) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(err) => {
            eprintln!("probe-resources: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests;
