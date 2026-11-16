//! Разбор аргументов раннера `probe-session`.

use std::fmt;
use std::path::PathBuf;

use crate::matrix::{Candidate, Scenario};

pub const USAGE: &str = "\
использование:
  probe-session --scenario <имя> [--backend zmx|rust] [--variant <имя>] --out <каталог> [--nonce …]
      процесс сценария, итог в <каталог>/result.json; --backend обязателен у сценария с
      кандидатами и не принимается у сценария стенда
  probe-session --aggregate <каталог прогона>
      summary.json прогона и сводка клеток по кандидатам
  probe-session --launch -- <аргументы сценария>
      процесс сценария с окружением, собранным с нуля, временным SHELLIO_STATE_DIR и внешним
      сторожем

--nonce процессу сценария выставляет --launch; --variant по умолчанию base, имя варианта —
строчные латинские буквы и цифры";

pub const DEFAULT_VARIANT: &str = "base";

/// Что запускает процесс сценария.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub scenario: Scenario,
    pub backend: Option<Candidate>,
    pub variant: String,
    pub out: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub target: Target,
    /// Аргументы процесса сценария после `--`, без `--nonce`.
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Help,
    Scenario {
        target: Target,
        nonce: Option<String>,
    },
    Aggregate {
        run_dir: PathBuf,
    },
    Launch(Launch),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgsError {
    NoMode,
    UnknownArgument(String),
    MissingValue(&'static str),
    Repeated(&'static str),
    InvalidValue {
        flag: &'static str,
        value: String,
        expected: &'static str,
    },
    UnknownScenario(String),
    /// Режиму `mode` нужен флаг `flag`.
    Requires {
        mode: &'static str,
        flag: &'static str,
    },
    /// Флаг `flag` не сочетается с режимом `mode`.
    NotWith {
        mode: &'static str,
        flag: &'static str,
    },
    /// Сценарий с кандидатами запущен без `--backend`.
    BackendRequired(Scenario),
    /// Кандидата нет среди кандидатов сценария.
    BackendNotAccepted {
        scenario: Scenario,
        backend: Candidate,
    },
    /// Аргументы после `--` бывают только у `--launch`.
    TailWithoutLaunch,
    /// Этот флаг процессу сценария выставляет сам `--launch`.
    LaunchOwns(&'static str),
    /// После `--` у `--launch` ожидается сценарий.
    LaunchTarget,
}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgsError::NoMode => write!(f, "нужен --scenario, --aggregate или --launch"),
            ArgsError::UnknownArgument(arg) => write!(f, "неизвестный аргумент «{arg}»"),
            ArgsError::MissingValue(flag) => write!(f, "{flag} требует значения"),
            ArgsError::Repeated(flag) => write!(f, "{flag} указан дважды"),
            ArgsError::InvalidValue {
                flag,
                value,
                expected,
            } => write!(
                f,
                "{flag}: неверное значение «{value}», ожидается {expected}"
            ),
            ArgsError::UnknownScenario(name) => {
                let known: Vec<&str> = Scenario::ALL.iter().map(|s| s.name()).collect();
                write!(
                    f,
                    "неизвестный сценарий «{name}», известны: {}",
                    known.join(", ")
                )
            }
            ArgsError::Requires { mode, flag } => write!(f, "{mode} требует {flag}"),
            ArgsError::NotWith { mode, flag } => write!(f, "{flag} не сочетается с {mode}"),
            ArgsError::BackendRequired(scenario) => {
                let known: Vec<&str> = scenario.candidates().iter().map(|c| c.name()).collect();
                write!(
                    f,
                    "сценарий {} требует --backend: {}",
                    scenario.name(),
                    known.join(" или ")
                )
            }
            ArgsError::BackendNotAccepted { scenario, backend } => {
                if scenario.candidates().is_empty() {
                    write!(
                        f,
                        "сценарий стенда {} не принимает --backend {}",
                        scenario.name(),
                        backend.name()
                    )
                } else {
                    write!(
                        f,
                        "сценарий {} не идёт на кандидате {}",
                        scenario.name(),
                        backend.name()
                    )
                }
            }
            ArgsError::TailWithoutLaunch => {
                write!(f, "аргументы после «--» бывают только у --launch")
            }
            ArgsError::LaunchOwns(flag) => {
                write!(f, "{flag} процессу сценария выставляет сам --launch")
            }
            ArgsError::LaunchTarget => write!(f, "после «--» у --launch ожидается сценарий"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flag {
    Scenario,
    Backend,
    Variant,
    Out,
    Nonce,
    Aggregate,
    Launch,
}

impl Flag {
    const ALL: [Flag; 7] = [
        Flag::Scenario,
        Flag::Backend,
        Flag::Variant,
        Flag::Out,
        Flag::Nonce,
        Flag::Aggregate,
        Flag::Launch,
    ];

    fn name(self) -> &'static str {
        match self {
            Flag::Scenario => "--scenario",
            Flag::Backend => "--backend",
            Flag::Variant => "--variant",
            Flag::Out => "--out",
            Flag::Nonce => "--nonce",
            Flag::Aggregate => "--aggregate",
            Flag::Launch => "--launch",
        }
    }

    fn takes_value(self) -> bool {
        self != Flag::Launch
    }

    fn from_arg(arg: &str) -> Option<Flag> {
        Flag::ALL.into_iter().find(|flag| flag.name() == arg)
    }
}

#[derive(Default)]
struct Raw {
    seen: Vec<Flag>,
    scenario: Option<Scenario>,
    backend: Option<Candidate>,
    variant: Option<String>,
    out: Option<PathBuf>,
    nonce: Option<String>,
    aggregate: Option<PathBuf>,
    launch: bool,
}

impl Raw {
    /// Первый из заданных флагов, которого нет в `allowed`.
    fn extra(&self, allowed: &[Flag]) -> Option<Flag> {
        self.seen
            .iter()
            .copied()
            .find(|flag| !allowed.contains(flag))
    }
}

/// Разбирает аргументы без имени программы.
pub fn parse(args: &[String]) -> Result<Mode, ArgsError> {
    let (head, tail) = match args.iter().position(|arg| arg == "--") {
        Some(index) => (&args[..index], Some(&args[index + 1..])),
        None => (args, None),
    };
    if head.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Mode::Help);
    }
    let raw = parse_flags(head)?;
    if raw.launch {
        return launch(raw, tail.unwrap_or_default());
    }
    if tail.is_some() {
        return Err(ArgsError::TailWithoutLaunch);
    }
    if let Some(run_dir) = raw.aggregate.clone() {
        return match raw.extra(&[Flag::Aggregate]) {
            Some(flag) => Err(not_with(Flag::Aggregate, flag)),
            None => Ok(Mode::Aggregate { run_dir }),
        };
    }
    match (raw.scenario, raw.out) {
        (Some(scenario), Some(out)) => {
            check_backend(scenario, raw.backend)?;
            Ok(Mode::Scenario {
                target: Target {
                    scenario,
                    backend: raw.backend,
                    variant: raw.variant.unwrap_or_else(|| DEFAULT_VARIANT.to_owned()),
                    out,
                },
                nonce: raw.nonce,
            })
        }
        (Some(_), None) => Err(requires(Flag::Scenario, Flag::Out)),
        (None, Some(_)) => Err(requires(Flag::Out, Flag::Scenario)),
        (None, None) => match raw.seen.first() {
            Some(flag) => Err(requires(*flag, Flag::Scenario)),
            None => Err(ArgsError::NoMode),
        },
    }
}

/// Сценарий с кандидатами идёт ровно на одном из них, сценарий стенда — без кандидата.
fn check_backend(scenario: Scenario, backend: Option<Candidate>) -> Result<(), ArgsError> {
    let accepted = scenario.candidates();
    match backend {
        None if !accepted.is_empty() => Err(ArgsError::BackendRequired(scenario)),
        Some(backend) if !accepted.contains(&backend) => {
            Err(ArgsError::BackendNotAccepted { scenario, backend })
        }
        _ => Ok(()),
    }
}

fn parse_flags(args: &[String]) -> Result<Raw, ArgsError> {
    let mut raw = Raw::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let flag = Flag::from_arg(arg).ok_or_else(|| ArgsError::UnknownArgument(arg.clone()))?;
        if raw.seen.contains(&flag) {
            return Err(ArgsError::Repeated(flag.name()));
        }
        raw.seen.push(flag);
        if !flag.takes_value() {
            raw.launch = true;
            continue;
        }
        let value = match rest.next() {
            Some(value) if !value.starts_with("--") => value.as_str(),
            _ => return Err(ArgsError::MissingValue(flag.name())),
        };
        match flag {
            Flag::Scenario => {
                let scenario = Scenario::from_name(value)
                    .ok_or_else(|| ArgsError::UnknownScenario(value.to_owned()))?;
                raw.scenario = Some(scenario);
            }
            Flag::Backend => {
                let backend = Candidate::from_name(value)
                    .ok_or_else(|| invalid(flag, value, "zmx или rust"))?;
                raw.backend = Some(backend);
            }
            Flag::Variant => {
                let valid = !value.is_empty()
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
                if !valid {
                    return Err(invalid(flag, value, "строчные латинские буквы и цифры"));
                }
                raw.variant = Some(value.to_owned());
            }
            Flag::Nonce => {
                let valid = !value.is_empty()
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
                if !valid {
                    return Err(invalid(flag, value, "латинские буквы, цифры и «-»"));
                }
                raw.nonce = Some(value.to_owned());
            }
            Flag::Out => raw.out = Some(path(flag, value)?),
            Flag::Aggregate => raw.aggregate = Some(path(flag, value)?),
            Flag::Launch => {}
        }
    }
    Ok(raw)
}

fn launch(raw: Raw, tail: &[String]) -> Result<Mode, ArgsError> {
    if let Some(flag) = raw.extra(&[Flag::Launch]) {
        return Err(not_with(Flag::Launch, flag));
    }
    if tail.iter().any(|arg| arg == Flag::Nonce.name()) {
        return Err(ArgsError::LaunchOwns(Flag::Nonce.name()));
    }
    match parse(tail)? {
        Mode::Scenario { target, .. } => Ok(Mode::Launch(Launch {
            target,
            args: tail.to_vec(),
        })),
        Mode::Help | Mode::Aggregate { .. } | Mode::Launch(_) => Err(ArgsError::LaunchTarget),
    }
}

fn requires(mode: Flag, flag: Flag) -> ArgsError {
    ArgsError::Requires {
        mode: mode.name(),
        flag: flag.name(),
    }
}

fn not_with(mode: Flag, flag: Flag) -> ArgsError {
    ArgsError::NotWith {
        mode: mode.name(),
        flag: flag.name(),
    }
}

fn invalid(flag: Flag, value: &str, expected: &'static str) -> ArgsError {
    ArgsError::InvalidValue {
        flag: flag.name(),
        value: value.to_owned(),
        expected,
    }
}

fn path(flag: Flag, value: &str) -> Result<PathBuf, ArgsError> {
    if value.is_empty() {
        return Err(invalid(flag, value, "непустой путь"));
    }
    Ok(PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn parse_str(args: &[&str]) -> Result<Mode, ArgsError> {
        parse(&strings(args))
    }

    fn smoke(out: &str) -> Target {
        Target {
            scenario: Scenario::Smoke,
            backend: None,
            variant: DEFAULT_VARIANT.to_owned(),
            out: PathBuf::from(out),
        }
    }

    #[test]
    fn help_and_empty_arguments() {
        assert_eq!(parse_str(&["--help"]), Ok(Mode::Help));
        assert_eq!(parse_str(&["--scenario", "smoke", "-h"]), Ok(Mode::Help));
        assert_eq!(parse_str(&[]), Err(ArgsError::NoMode));
    }

    #[test]
    fn scenario_with_defaults_and_nonce() {
        assert_eq!(
            parse_str(&["--scenario", "smoke", "--out", "o"]),
            Ok(Mode::Scenario {
                target: smoke("o"),
                nonce: None,
            })
        );
        assert_eq!(
            parse_str(&[
                "--out",
                "o",
                "--variant",
                "v2",
                "--nonce",
                "ab-12",
                "--scenario",
                "smoke"
            ]),
            Ok(Mode::Scenario {
                target: Target {
                    variant: "v2".to_owned(),
                    ..smoke("o")
                },
                nonce: Some("ab-12".to_owned()),
            })
        );
    }

    #[test]
    fn stand_scenario_rejects_backend() {
        assert_eq!(
            parse_str(&["--scenario", "smoke", "--backend", "zmx", "--out", "o"]),
            Err(ArgsError::BackendNotAccepted {
                scenario: Scenario::Smoke,
                backend: Candidate::Zmx,
            })
        );
        assert_eq!(
            ArgsError::BackendNotAccepted {
                scenario: Scenario::Smoke,
                backend: Candidate::Rust,
            }
            .to_string(),
            "сценарий стенда smoke не принимает --backend rust"
        );
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert_eq!(
            parse_str(&["--scenario", "nope", "--out", "o"]),
            Err(ArgsError::UnknownScenario("nope".to_owned()))
        );
        assert!(matches!(
            parse_str(&["--scenario", "smoke", "--backend", "tmux", "--out", "o"]),
            Err(ArgsError::InvalidValue {
                flag: "--backend",
                ..
            })
        ));
        for variant in ["Base", "a-b", "a_b", ""] {
            assert!(
                matches!(
                    parse_str(&["--scenario", "smoke", "--variant", variant, "--out", "o"]),
                    Err(ArgsError::InvalidValue {
                        flag: "--variant",
                        ..
                    })
                ),
                "{variant}"
            );
        }
        assert!(matches!(
            parse_str(&["--scenario", "smoke", "--nonce", "a b", "--out", "o"]),
            Err(ArgsError::InvalidValue {
                flag: "--nonce",
                ..
            })
        ));
        assert!(matches!(
            parse_str(&["--scenario", "smoke", "--out", ""]),
            Err(ArgsError::InvalidValue { flag: "--out", .. })
        ));
    }

    #[test]
    fn malformed_flags() {
        assert_eq!(
            parse_str(&["--scenario"]),
            Err(ArgsError::MissingValue("--scenario"))
        );
        assert_eq!(
            parse_str(&["--scenario", "--out", "o"]),
            Err(ArgsError::MissingValue("--scenario"))
        );
        assert_eq!(
            parse_str(&["--scenario", "smoke", "--scenario", "smoke"]),
            Err(ArgsError::Repeated("--scenario"))
        );
        assert_eq!(
            parse_str(&["--fixture"]),
            Err(ArgsError::UnknownArgument("--fixture".to_owned()))
        );
        assert_eq!(
            parse_str(&["--scenario", "smoke"]),
            Err(ArgsError::Requires {
                mode: "--scenario",
                flag: "--out"
            })
        );
        assert_eq!(
            parse_str(&["--out", "o"]),
            Err(ArgsError::Requires {
                mode: "--out",
                flag: "--scenario"
            })
        );
        assert_eq!(
            parse_str(&["--variant", "base"]),
            Err(ArgsError::Requires {
                mode: "--variant",
                flag: "--scenario"
            })
        );
        assert_eq!(
            parse_str(&["--scenario", "smoke", "--out", "o", "--", "x"]),
            Err(ArgsError::TailWithoutLaunch)
        );
    }

    #[test]
    fn aggregate_mode() {
        assert_eq!(
            parse_str(&["--aggregate", "run"]),
            Ok(Mode::Aggregate {
                run_dir: PathBuf::from("run")
            })
        );
        assert_eq!(
            parse_str(&["--aggregate", "run", "--out", "o"]),
            Err(ArgsError::NotWith {
                mode: "--aggregate",
                flag: "--out"
            })
        );
    }

    #[test]
    fn launch_wraps_scenario() {
        let tail = ["--scenario", "smoke", "--out", "o"];
        let mut args = vec!["--launch", "--"];
        args.extend(tail);
        assert_eq!(
            parse_str(&args),
            Ok(Mode::Launch(Launch {
                target: smoke("o"),
                args: strings(&tail),
            }))
        );
    }

    #[test]
    fn launch_rejects_foreign_flags_and_targets() {
        assert_eq!(
            parse_str(&[
                "--launch",
                "--",
                "--scenario",
                "smoke",
                "--out",
                "o",
                "--nonce",
                "x"
            ]),
            Err(ArgsError::LaunchOwns("--nonce"))
        );
        assert_eq!(
            parse_str(&["--launch", "--out", "o", "--", "--scenario", "smoke"]),
            Err(ArgsError::NotWith {
                mode: "--launch",
                flag: "--out"
            })
        );
        assert_eq!(
            parse_str(&["--launch", "--", "--aggregate", "run"]),
            Err(ArgsError::LaunchTarget)
        );
        assert_eq!(
            parse_str(&[
                "--launch",
                "--",
                "--launch",
                "--",
                "--scenario",
                "smoke",
                "--out",
                "o"
            ]),
            Err(ArgsError::LaunchTarget)
        );
        assert_eq!(
            parse_str(&["--launch", "--", "--help"]),
            Err(ArgsError::LaunchTarget)
        );
        assert_eq!(parse_str(&["--launch"]), Err(ArgsError::NoMode));
        assert_eq!(
            parse_str(&[
                "--launch",
                "--",
                "--scenario",
                "smoke",
                "--backend",
                "rust",
                "--out",
                "o"
            ]),
            Err(ArgsError::BackendNotAccepted {
                scenario: Scenario::Smoke,
                backend: Candidate::Rust,
            })
        );
    }
}
