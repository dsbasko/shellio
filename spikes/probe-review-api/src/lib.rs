//! Проба P0.7 `probe-review-api` (ADR-0034): API ревью GitHub и GitLab по ADR-0015, журнал
//! черновиков, `forge-fake` и стенд матрицы возможностей. Библиотека — модули, которые идут в
//! `make check`; бинарь `probe-review` только вызывает [`main`].
//!
//! Проба не зависит от других проб: модель результатов взята из
//! `spikes/probe-update-lifecycle/src/results.rs` и переделана под матрицу «строка × столбец».

pub mod results;
pub mod scenario;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use results::{Column, Execution, Pair, ResultFile};
use scenario::{Context, Scenario};

/// Ошибка исполнения: стенд не дошёл до конца, предусловие обязательной цели не выполнено,
/// повреждённый результат, ошибка агрегатора. `fail`, `absent` и `not_run` ею не считаются.
pub const EXIT_EXECUTION: u8 = 3;
pub const EXIT_USAGE: u8 = 2;

pub const ENV_SERIES: &str = "PROBE_REVIEW_SERIES";
pub const ENV_BUILD: &str = "PROBE_REVIEW_BUILD";
pub const ENV_STATE_DIR: &str = "SHELLIO_STATE_DIR";

pub const USAGE: &str = "использование:
  probe-review --scenario <сценарий> --column <столбец> --out <каталог процесса>
  probe-review --aggregate --pairs \"<столбец>:<сценарий> …\" [--out <каталог>] <каталог прогона>…
  probe-review --list [<столбец>]";

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    Scenario {
        scenario: Scenario,
        column: Column,
        out: PathBuf,
    },
    Aggregate {
        pairs: Vec<Pair>,
        out: Option<PathBuf>,
        dirs: Vec<PathBuf>,
    },
    List {
        column: Option<Column>,
    },
}

pub fn parse_args(args: &[String]) -> Result<Mode, String> {
    let mut iter = args.iter();
    let first = iter.next().ok_or("не задан режим")?;
    let mut options: Vec<(&str, &str)> = Vec::new();
    let mut positional: Vec<&str> = Vec::new();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--column" | "--out" | "--pairs" => {
                let value = iter.next().ok_or_else(|| format!("у {arg} нет значения"))?;
                options.push((arg.as_str(), value.as_str()));
            }
            other if other.starts_with("--") => return Err(format!("неизвестный флаг {other}")),
            other => positional.push(other),
        }
    }
    let option = |name: &str| {
        options
            .iter()
            .rev()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| *value)
    };
    match first.as_str() {
        "--scenario" => {
            let [name] = positional[..] else {
                return Err("--scenario ждёт одно имя сценария".to_owned());
            };
            let scenario = Scenario::from_name(name)
                .ok_or_else(|| format!("неизвестный сценарий «{name}»"))?;
            let column = option("--column").ok_or("не задан --column")?;
            let column = Column::from_name(column)
                .ok_or_else(|| format!("неизвестный столбец «{column}»"))?;
            if !scenario.columns().contains(&column) {
                return Err(format!(
                    "сценарий {} не идёт в столбце {}",
                    scenario.name(),
                    column.name()
                ));
            }
            let out = PathBuf::from(option("--out").ok_or("не задан --out")?);
            Ok(Mode::Scenario {
                scenario,
                column,
                out,
            })
        }
        "--aggregate" => {
            let pairs = Pair::parse_list(option("--pairs").ok_or("не задан --pairs")?)?;
            if positional.is_empty() {
                return Err("не заданы каталоги прогона".to_owned());
            }
            let dirs: Vec<PathBuf> = positional.iter().map(PathBuf::from).collect();
            let out = option("--out").map(PathBuf::from);
            if out.is_none() && dirs.len() > 1 {
                return Err("для нескольких каталогов нужен --out".to_owned());
            }
            Ok(Mode::Aggregate { pairs, out, dirs })
        }
        "--list" => {
            let column = match positional[..] {
                [] => None,
                [name] => Some(
                    Column::from_name(name)
                        .ok_or_else(|| format!("неизвестный столбец «{name}»"))?,
                ),
                _ => return Err("--list ждёт не больше одного столбца".to_owned()),
            };
            Ok(Mode::List { column })
        }
        other => Err(format!("неизвестный режим {other}")),
    }
}

/// Пары `столбец:сценарий`, которые проба умеет прогонять, в порядке каталога.
pub fn known_pairs(column: Option<Column>) -> Vec<Pair> {
    Scenario::ALL
        .iter()
        .flat_map(|scenario| {
            scenario
                .columns()
                .iter()
                .filter(move |c| column.is_none_or(|wanted| wanted == **c))
                .map(move |c| Pair {
                    column: *c,
                    scenario: scenario.name().to_owned(),
                })
        })
        .collect()
}

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = match parse_args(&args) {
        Ok(mode) => mode,
        Err(err) => {
            eprintln!("probe-review: {err}\n{USAGE}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match mode {
        Mode::List { column } => {
            for pair in known_pairs(column) {
                println!("{pair}");
            }
            ExitCode::SUCCESS
        }
        Mode::Scenario {
            scenario,
            column,
            out,
        } => run_scenario(scenario, column, &out),
        Mode::Aggregate { pairs, out, dirs } => {
            let out = out.unwrap_or_else(|| dirs[0].clone());
            run_aggregate(&pairs, &dirs, &out)
        }
    }
}

fn env_value(name: &str) -> Result<String, String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(format!("предусловие: не задана {name}")),
    }
}

fn run_scenario(scenario: Scenario, column: Column, out: &Path) -> ExitCode {
    let series = env_value(ENV_SERIES);
    let build = env_value(ENV_BUILD);
    let state_dir = env_value(ENV_STATE_DIR);
    let mut result = ResultFile::new(
        series.as_deref().unwrap_or(""),
        build.as_deref().unwrap_or(""),
        scenario.name(),
        column,
    );
    match (&series, &build, &state_dir) {
        (Ok(_), Ok(_), Ok(state_dir)) => {
            let ctx = Context {
                column,
                out: out.to_path_buf(),
                state_dir: PathBuf::from(state_dir),
            };
            scenario.run(&ctx, &mut result);
        }
        _ => {
            let reasons: Vec<String> = [series, build, state_dir]
                .into_iter()
                .filter_map(Result::err)
                .collect();
            result.execution = Execution::error(reasons.join("; "));
        }
    }
    if let Err(err) = results::write_json(out, results::RESULT_FILE, &result) {
        eprintln!("probe-review: {err}");
        return ExitCode::from(EXIT_EXECUTION);
    }
    match &result.execution {
        Execution::Ok => ExitCode::SUCCESS,
        Execution::Error { reason } => {
            eprintln!(
                "probe-review: ошибка исполнения {}: {reason}",
                scenario.name()
            );
            ExitCode::from(EXIT_EXECUTION)
        }
    }
}

fn run_aggregate(pairs: &[Pair], dirs: &[PathBuf], out: &Path) -> ExitCode {
    let summary = match results::aggregate(dirs, pairs) {
        Ok(summary) => summary,
        Err(err) => {
            eprintln!("probe-review: агрегирование: {err}");
            return ExitCode::from(EXIT_EXECUTION);
        }
    };
    let markdown = results::render_markdown(&summary);
    let written = results::write_json(out, results::SUMMARY_FILE, &summary).and_then(|_| {
        std::fs::write(out.join(results::SUMMARY_MD), &markdown)
            .map_err(|err| format!("запись {}: {err}", results::SUMMARY_MD))
    });
    if let Err(err) = written {
        eprintln!("probe-review: {err}");
        return ExitCode::from(EXIT_EXECUTION);
    }
    print!("{markdown}");
    if summary.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_EXECUTION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_scenario_mode() {
        let mode = parse_args(&args(&[
            "--scenario",
            "smoke",
            "--column",
            "fake",
            "--out",
            "/tmp/x",
        ]));
        assert_eq!(
            mode,
            Ok(Mode::Scenario {
                scenario: Scenario::Smoke,
                column: Column::Fake,
                out: PathBuf::from("/tmp/x"),
            })
        );
    }

    #[test]
    fn rejects_scenario_outside_its_columns() {
        let mode = parse_args(&args(&[
            "--scenario",
            "smoke",
            "--column",
            "github_cloud",
            "--out",
            "/tmp/x",
        ]));
        assert!(mode.is_err());
    }

    #[test]
    fn aggregate_needs_pairs_and_out_for_several_dirs() {
        assert!(parse_args(&args(&["--aggregate", "a"])).is_err());
        assert!(parse_args(&args(&["--aggregate", "--pairs", "fake:smoke", "a", "b"])).is_err());
        assert!(
            parse_args(&args(&[
                "--aggregate",
                "--pairs",
                "fake:smoke",
                "--out",
                "o",
                "a",
                "b"
            ]))
            .is_ok()
        );
    }

    #[test]
    fn list_filters_by_column() {
        assert_eq!(
            known_pairs(Some(Column::Fake))
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["fake:smoke"]
        );
        assert!(known_pairs(Some(Column::Ghes)).is_empty());
    }
}
