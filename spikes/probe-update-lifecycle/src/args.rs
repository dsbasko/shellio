//! Разбор аргументов раннера `probe-update`. Источник — `spikes/probe-session-survival/src/args.rs`
//! без кандидатов хоста и режимов поверхности.

use std::fmt;
use std::path::PathBuf;

use crate::scenario::Scenario;

pub const USAGE: &str = "\
использование:
  probe-update --scenario <имя> [--variant <имя>] --out <каталог> [--nonce …]
      процесс сценария, итог в <каталог>/result.json
  probe-update --aggregate <каталог прогона>
      summary.json прогона, сводка клеток и итог пробы
  probe-update --launch -- <аргументы сценария>
      процесс сценария с окружением, собранным с нуля, временным SHELLIO_STATE_DIR и внешним
      сторожем

--nonce процессу сценария выставляет --launch; --variant по умолчанию base, имя варианта —
строчные латинские буквы и цифры";

pub const DEFAULT_VARIANT: &str = "base";

/// Что запускает процесс сценария.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub scenario: Scenario,
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
    Variant,
    Out,
    Nonce,
    Aggregate,
    Launch,
}

impl Flag {
    const ALL: [Flag; 6] = [
        Flag::Scenario,
        Flag::Variant,
        Flag::Out,
        Flag::Nonce,
        Flag::Aggregate,
        Flag::Launch,
    ];

    fn name(self) -> &'static str {
        match self {
            Flag::Scenario => "--scenario",
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
        (Some(scenario), Some(out)) => Ok(Mode::Scenario {
            target: Target {
                scenario,
                variant: raw.variant.unwrap_or_else(|| DEFAULT_VARIANT.to_owned()),
                out,
            },
            nonce: raw.nonce,
        }),
        (Some(_), None) => Err(requires(Flag::Scenario, Flag::Out)),
        (None, Some(_)) => Err(requires(Flag::Out, Flag::Scenario)),
        (None, None) => match raw.seen.first() {
            Some(flag) => Err(requires(*flag, Flag::Scenario)),
            None => Err(ArgsError::NoMode),
        },
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

fn path(flag: Flag, value: &str) -> Result<PathBuf, ArgsError> {
    if value.is_empty() {
        return Err(ArgsError::MissingValue(flag.name()));
    }
    Ok(PathBuf::from(value))
}

fn invalid(flag: Flag, value: &str, expected: &'static str) -> ArgsError {
    ArgsError::InvalidValue {
        flag: flag.name(),
        value: value.to_owned(),
        expected,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn parsed(args: &[&str]) -> Result<Mode, ArgsError> {
        parse(&strings(args))
    }

    #[test]
    fn scenario_with_default_variant() {
        assert_eq!(
            parsed(&["--scenario", "smoke", "--out", "o", "--nonce", "ab-12"]),
            Ok(Mode::Scenario {
                target: Target {
                    scenario: Scenario::Smoke,
                    variant: DEFAULT_VARIANT.to_owned(),
                    out: PathBuf::from("o"),
                },
                nonce: Some("ab-12".to_owned()),
            })
        );
        assert!(matches!(
            parsed(&["--scenario", "smoke", "--out", "o", "--variant", "v2"]),
            Ok(Mode::Scenario { target, nonce: None }) if target.variant == "v2"
        ));
    }

    #[test]
    fn aggregate_and_help() {
        assert_eq!(
            parsed(&["--aggregate", "run"]),
            Ok(Mode::Aggregate {
                run_dir: PathBuf::from("run")
            })
        );
        assert_eq!(parsed(&["--out", "o", "-h"]), Ok(Mode::Help));
        assert_eq!(
            parsed(&["--aggregate", "run", "--out", "o"]),
            Err(not_with(Flag::Aggregate, Flag::Out))
        );
    }

    #[test]
    fn launch_takes_the_scenario_after_separator() {
        let tail = ["--scenario", "smoke", "--out", "o"];
        let mut args = vec!["--launch", "--"];
        args.extend(tail);
        match parsed(&args) {
            Ok(Mode::Launch(launch)) => {
                assert_eq!(launch.target.scenario, Scenario::Smoke);
                assert_eq!(launch.args, strings(&tail));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            parsed(&[
                "--launch",
                "--",
                "--scenario",
                "smoke",
                "--out",
                "o",
                "--nonce",
                "n"
            ]),
            Err(ArgsError::LaunchOwns("--nonce"))
        );
        assert_eq!(
            parsed(&["--launch", "--", "--aggregate", "run"]),
            Err(ArgsError::LaunchTarget)
        );
        assert_eq!(
            parsed(&["--launch", "--out", "o", "--"]),
            Err(not_with(Flag::Launch, Flag::Out))
        );
    }

    #[test]
    fn malformed_arguments_are_rejected() {
        assert_eq!(parsed(&[]), Err(ArgsError::NoMode));
        assert_eq!(
            parsed(&["--bogus"]),
            Err(ArgsError::UnknownArgument("--bogus".to_owned()))
        );
        assert_eq!(
            parsed(&["--scenario"]),
            Err(ArgsError::MissingValue("--scenario"))
        );
        assert_eq!(
            parsed(&["--scenario", "smoke", "--scenario", "smoke"]),
            Err(ArgsError::Repeated("--scenario"))
        );
        assert_eq!(
            parsed(&["--scenario", "nope", "--out", "o"]),
            Err(ArgsError::UnknownScenario("nope".to_owned()))
        );
        assert_eq!(
            parsed(&["--scenario", "smoke"]),
            Err(requires(Flag::Scenario, Flag::Out))
        );
        assert_eq!(
            parsed(&["--out", "o"]),
            Err(requires(Flag::Out, Flag::Scenario))
        );
        assert_eq!(
            parsed(&["--variant", "Base", "--scenario", "smoke", "--out", "o"]),
            Err(invalid(
                Flag::Variant,
                "Base",
                "строчные латинские буквы и цифры"
            ))
        );
        assert_eq!(
            parsed(&["--scenario", "smoke", "--out", "o", "--nonce", "a b"]),
            Err(invalid(Flag::Nonce, "a b", "латинские буквы, цифры и «-»"))
        );
        assert_eq!(
            parsed(&["--scenario", "smoke", "--out", "o", "--", "x"]),
            Err(ArgsError::TailWithoutLaunch)
        );
    }
}
