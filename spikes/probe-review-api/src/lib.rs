//! Проба P0.7 `probe-review-api` (ADR-0034): API ревью GitHub и GitLab по ADR-0015, журнал
//! черновиков, `forge-fake` и стенд матрицы возможностей. Библиотека — модули, которые идут в
//! `make check`; бинарь `probe-review` только вызывает [`main`].
//!
//! Проба не зависит от других проб: модель результатов взята из
//! `spikes/probe-update-lifecycle/src/results.rs` и переделана под матрицу «строка × столбец».

pub mod fake;
pub mod github;
pub mod gitlab;
pub mod journal;
pub mod live;
pub mod model;
pub mod results;
pub mod scenario;
pub mod scrub;
pub mod submit;
pub mod transport;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use results::{Column, Execution, Pair, ResultFile};
use scenario::live::SecondOp;
use scenario::{Context, Scenario};

/// Ошибка исполнения: стенд не дошёл до конца, предусловие обязательной цели не выполнено,
/// повреждённый результат, ошибка агрегатора. `fail`, `absent` и `not_run` ею не считаются.
pub const EXIT_EXECUTION: u8 = 3;
pub const EXIT_USAGE: u8 = 2;
/// Настройка GitLab CE: хост отказал в password grant, `run.sh` создаёт токен root через
/// `gitlab-rails runner` и повторяет настройку.
pub const EXIT_ROOT_TOKEN: u8 = 5;

pub const ENV_SERIES: &str = "PROBE_REVIEW_SERIES";
pub const ENV_BUILD: &str = "PROBE_REVIEW_BUILD";
pub const ENV_STATE_DIR: &str = "SHELLIO_STATE_DIR";

pub const USAGE: &str = "использование:
  probe-review --scenario <сценарий> --column <столбец> --out <каталог процесса>
  probe-review --aggregate --pairs \"<столбец>:<сценарий> …\" [--out <каталог>] <каталог прогона>…
  probe-review --list [<столбец>]
  probe-review --gitlab-ce-image
  probe-review --gitlab-ce-provision --url http://127.0.0.1:<порт> --out <каталог подъёма>
  probe-review --second-client gl-create-draft --column <столбец> --mr <iid> --line <строка> --out <каталог процесса>
  probe-review --second-client gl-delete-draft --column <столбец> --mr <iid> --draft <ID> --out <каталог процесса>
  probe-review --second-client gl-plant-notes --column <столбец> --mr <iid> --count <число> --out <каталог процесса>
  probe-review --second-client gh-create-pending --column <столбец> --mr <номер PR> --count <число> --out <каталог процесса>";

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
    /// Ссылка на закреплённый образ GitLab CE для `gitlab/up.sh`.
    GitlabCeImage,
    /// Настройка поднятого GitLab CE (`live::provision`).
    GitlabCeProvision {
        url: String,
        out: PathBuf,
    },
    /// Второй клиент ревьюера отдельным процессом: черновик на новой строке MR (`gl-foreign-draft`).
    SecondClient {
        column: Column,
        mr: u64,
        op: SecondOp,
        out: PathBuf,
    },
}

pub fn parse_args(args: &[String]) -> Result<Mode, String> {
    let mut iter = args.iter();
    let first = iter.next().ok_or("не задан режим")?;
    let mut options: Vec<(&str, &str)> = Vec::new();
    let mut positional: Vec<&str> = Vec::new();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--column" | "--out" | "--pairs" | "--url" | "--mr" | "--line" | "--draft"
            | "--count" => {
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
        "--gitlab-ce-image" => {
            if !positional.is_empty() || !options.is_empty() {
                return Err("--gitlab-ce-image без аргументов".to_owned());
            }
            Ok(Mode::GitlabCeImage)
        }
        "--gitlab-ce-provision" => {
            if !positional.is_empty() {
                return Err("--gitlab-ce-provision без позиционных аргументов".to_owned());
            }
            let url = option("--url").ok_or("не задан --url")?.to_owned();
            let out = PathBuf::from(option("--out").ok_or("не задан --out")?);
            Ok(Mode::GitlabCeProvision { url, out })
        }
        "--second-client" => {
            let [op] = positional[..] else {
                return Err("--second-client ждёт одну операцию".to_owned());
            };
            let column = option("--column").ok_or("не задан --column")?;
            let column = Column::from_name(column)
                .ok_or_else(|| format!("неизвестный столбец «{column}»"))?;
            let number = |name: &str| {
                option(name)
                    .ok_or_else(|| format!("не задан {name}"))?
                    .parse::<u64>()
                    .map_err(|_| format!("{name} — не число"))
            };
            let mr = number("--mr")?;
            let op = match op {
                "gl-create-draft" => SecondOp::CreateDraft {
                    line: u32::try_from(number("--line")?)
                        .map_err(|_| "--line вне u32".to_owned())?,
                },
                "gl-delete-draft" => {
                    let draft = option("--draft").ok_or("не задан --draft")?;
                    if draft.is_empty() || !draft.bytes().all(|b| b.is_ascii_digit()) {
                        return Err("--draft — не число".to_owned());
                    }
                    SecondOp::DeleteDraft {
                        draft: draft.to_owned(),
                    }
                }
                "gl-plant-notes" => {
                    let count = u32::try_from(number("--count")?)
                        .map_err(|_| "--count вне u32".to_owned())?;
                    if count == 0 {
                        return Err("--count — ноль".to_owned());
                    }
                    SecondOp::PlantNotes { count }
                }
                "gh-create-pending" => {
                    let count = u32::try_from(number("--count")?)
                        .map_err(|_| "--count вне u32".to_owned())?;
                    if count == 0 {
                        return Err("--count — ноль".to_owned());
                    }
                    SecondOp::GhCreatePending { count }
                }
                other => return Err(format!("неизвестная операция второго клиента {other}")),
            };
            let out = PathBuf::from(option("--out").ok_or("не задан --out")?);
            Ok(Mode::SecondClient {
                column,
                mr,
                op,
                out,
            })
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
        Mode::GitlabCeImage => match live::provision::Pin::embedded() {
            Ok(pin) => {
                println!("{}", pin.reference());
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("probe-review: {err}");
                ExitCode::from(EXIT_EXECUTION)
            }
        },
        Mode::GitlabCeProvision { url, out } => run_gitlab_ce_provision(&url, &out),
        Mode::SecondClient {
            column,
            mr,
            op,
            out,
        } => {
            let (reply, ok) = scenario::live::second_client(column, mr, &op, &out);
            println!("{reply}");
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(EXIT_EXECUTION)
            }
        }
    }
}

/// Настройка GitLab CE: описание хоста — в `<out>/gitlab-ce.json`, токены ролей — одной строкой
/// JSON в stdout. Токены не попадают ни в stderr, ни в файлы.
fn run_gitlab_ce_provision(url: &str, out: &Path) -> ExitCode {
    use live::provision::{self, Pin, ProvisionError, RootAuth};
    use transport::http::{HttpConfig, HttpTransport};

    let prepared = Pin::embedded().and_then(|pin| RootAuth::from_env().map(|auth| (pin, auth)));
    let (pin, auth) = match prepared {
        Ok(prepared) => prepared,
        Err(err) => {
            eprintln!("probe-review: настройка GitLab CE: {err}");
            return ExitCode::from(EXIT_EXECUTION);
        }
    };
    let connect = |credential| HttpTransport::new(HttpConfig::default(), credential);
    match provision::provision(url, &pin, auth, connect) {
        Ok((host, tokens)) => {
            if let Err(err) = results::write_json(out, provision::HOST_FILE, &host) {
                eprintln!("probe-review: настройка GitLab CE: {err}");
                return ExitCode::from(EXIT_EXECUTION);
            }
            println!("{}", tokens.to_json());
            eprintln!(
                "probe-review: GitLab CE {} настроен: проект {}, токен root — {}",
                host.version, host.project, host.root_token
            );
            ExitCode::SUCCESS
        }
        Err(ProvisionError::GrantRefused(detail)) => {
            eprintln!("probe-review: настройка GitLab CE: password grant отклонён: {detail}");
            ExitCode::from(EXIT_ROOT_TOKEN)
        }
        Err(err) => {
            eprintln!("probe-review: настройка GitLab CE: {err}");
            ExitCode::from(EXIT_EXECUTION)
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
        Execution::Ok if column == Column::Fake && result.status != results::Status::Pass => {
            eprintln!(
                "probe-review: ошибка исполнения {}: итог {} в столбце fake: {}",
                scenario.name(),
                result.status.name(),
                result.reason
            );
            ExitCode::from(EXIT_EXECUTION)
        }
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
    fn parses_gitlab_ce_modes() {
        assert_eq!(
            parse_args(&args(&["--gitlab-ce-image"])),
            Ok(Mode::GitlabCeImage)
        );
        assert!(parse_args(&args(&["--gitlab-ce-image", "x"])).is_err());
        assert_eq!(
            parse_args(&args(&[
                "--gitlab-ce-provision",
                "--url",
                "http://127.0.0.1:1",
                "--out",
                "/tmp/ce"
            ])),
            Ok(Mode::GitlabCeProvision {
                url: "http://127.0.0.1:1".to_owned(),
                out: PathBuf::from("/tmp/ce"),
            })
        );
        assert!(parse_args(&args(&["--gitlab-ce-provision", "--out", "/tmp/ce"])).is_err());
        assert!(parse_args(&args(&["--gitlab-ce-provision", "--url", "u"])).is_err());
    }

    #[test]
    fn parses_second_client_mode() {
        let full = [
            "--second-client",
            "gl-create-draft",
            "--column",
            "gitlab_ce",
            "--mr",
            "7",
            "--line",
            "24",
            "--out",
            "/tmp/p",
        ];
        assert_eq!(
            parse_args(&args(&full)),
            Ok(Mode::SecondClient {
                column: Column::GitlabCe,
                mr: 7,
                op: SecondOp::CreateDraft { line: 24 },
                out: PathBuf::from("/tmp/p"),
            })
        );
        let mut delete = full;
        delete[1] = "gl-delete-draft";
        delete[6] = "--draft";
        delete[7] = "31";
        assert_eq!(
            parse_args(&args(&delete)),
            Ok(Mode::SecondClient {
                column: Column::GitlabCe,
                mr: 7,
                op: SecondOp::DeleteDraft {
                    draft: "31".to_owned()
                },
                out: PathBuf::from("/tmp/p"),
            })
        );
        let mut no_draft = full;
        no_draft[1] = "gl-delete-draft";
        assert!(parse_args(&args(&no_draft)).is_err());
        let mut bad_draft = delete;
        bad_draft[7] = "31/../x";
        assert!(parse_args(&args(&bad_draft)).is_err());
        let mut plant = full;
        plant[1] = "gl-plant-notes";
        plant[6] = "--count";
        plant[7] = "100";
        assert_eq!(
            parse_args(&args(&plant)),
            Ok(Mode::SecondClient {
                column: Column::GitlabCe,
                mr: 7,
                op: SecondOp::PlantNotes { count: 100 },
                out: PathBuf::from("/tmp/p"),
            })
        );
        let mut zero = plant;
        zero[7] = "0";
        assert!(parse_args(&args(&zero)).is_err());
        let mut no_count = full;
        no_count[1] = "gl-plant-notes";
        assert!(parse_args(&args(&no_count)).is_err());
        let mut pending = plant;
        pending[1] = "gh-create-pending";
        pending[3] = "github_cloud";
        pending[7] = "101";
        assert_eq!(
            parse_args(&args(&pending)),
            Ok(Mode::SecondClient {
                column: Column::GithubCloud,
                mr: 7,
                op: SecondOp::GhCreatePending { count: 101 },
                out: PathBuf::from("/tmp/p"),
            })
        );
        let mut zero_pending = pending;
        zero_pending[7] = "0";
        assert!(parse_args(&args(&zero_pending)).is_err());
        let mut pending_no_count = full;
        pending_no_count[1] = "gh-create-pending";
        assert!(parse_args(&args(&pending_no_count)).is_err());
        let mut other = full;
        other[1] = "gl-publish-draft";
        assert!(parse_args(&args(&other)).is_err());
        let mut bad = full;
        bad[5] = "x";
        assert!(parse_args(&args(&bad)).is_err());
        assert!(parse_args(&args(&full[..8])).is_err());
    }

    #[test]
    fn list_filters_by_column() {
        let fake: Vec<String> = known_pairs(Some(Column::Fake))
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(fake.first().map(String::as_str), Some("fake:smoke"));
        assert_eq!(fake.len(), results::MATRIX_ROWS.len() + 1);
        assert!(fake.iter().all(|pair| pair.starts_with("fake:")));
        let ghes: Vec<String> = known_pairs(Some(Column::Ghes))
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(
            ghes.contains(&"ghes:gh-local-create".to_owned()),
            "{ghes:?}"
        );
        assert!(ghes.contains(&"ghes:capabilities".to_owned()), "{ghes:?}");
        assert!(
            !ghes.iter().any(|pair| pair.starts_with("ghes:gl-")),
            "{ghes:?}"
        );
    }
}
