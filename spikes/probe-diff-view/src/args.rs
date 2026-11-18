//! Разбор аргументов командной строки и перечень сценариев. Модуль не зависит от AppKit.
//!
//! Источник — `spikes/probe-appkit-shell/src/args.rs` (P0.2). Упрощён: вместо сплита и настроек
//! сцены — вариант отрисовки (`a`, `b`) и режим (`unified`, `side`), которые у сценария стенда
//! отсутствуют, а у сценария вида обязательны.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const USAGE: &str = "\
использование:
  probe-diff-view [--variant a|b] [--mode unified|side]
      демонстрация без стенда и result.json
  probe-diff-view --scenario <имя> --out <каталог> [--variant a|b] [--mode unified|side]
      сценарий стенда, итог в <каталог>/result.json
  probe-diff-view --aggregate <каталог прогона>
      сводка результатов прогона без окна

у демонстрации по умолчанию --variant a --mode unified; сценарий стенда идёт без --variant и
--mode, сценарий вида требует обоих";

/// Вариант отрисовки: A — собственный `NSView` с Core Text, B — view-based `NSTableView`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Variant {
    A,
    B,
}

impl Variant {
    pub const ALL: [Variant; 2] = [Variant::A, Variant::B];

    pub fn name(self) -> &'static str {
        match self {
            Variant::A => "a",
            Variant::B => "b",
        }
    }

    pub fn from_name(name: &str) -> Option<Variant> {
        Variant::ALL
            .into_iter()
            .find(|variant| variant.name() == name)
    }
}

/// Режим показа: unified или «Рядом».
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    Unified,
    Side,
}

impl ViewMode {
    pub const ALL: [ViewMode; 2] = [ViewMode::Unified, ViewMode::Side];

    pub fn name(self) -> &'static str {
        match self {
            ViewMode::Unified => "unified",
            ViewMode::Side => "side",
        }
    }

    pub fn from_name(name: &str) -> Option<ViewMode> {
        ViewMode::ALL.into_iter().find(|mode| mode.name() == name)
    }
}

/// По каким осям идёт сценарий. Сценарий стенда — один процесс без варианта и режима, сценарий
/// вида — процесс на каждую пару варианта и режима. Таблица `run.sh` (`axes_of`) повторяет её.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Axes {
    pub variants: bool,
    pub modes: bool,
}

impl Axes {
    pub const STAND: Axes = Axes {
        variants: false,
        modes: false,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// Пустое окно: приложение активно, окно видимо и key, `result.json` записан.
    Smoke,
}

impl Scenario {
    pub const ALL: [Scenario; 1] = [Scenario::Smoke];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Smoke => "smoke",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL
            .into_iter()
            .find(|scenario| scenario.name() == name)
    }

    pub fn axes(self) -> Axes {
        match self {
            Scenario::Smoke => Axes::STAND,
        }
    }
}

/// Вариант и режим из командной строки; `None` — флаг не указан.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewOptions {
    pub variant: Option<Variant>,
    pub mode: Option<ViewMode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Demo(ViewOptions),
    Scenario {
        scenario: Scenario,
        view: ViewOptions,
        out: PathBuf,
    },
    Aggregate {
        run_dir: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgsError {
    UnknownArgument(String),
    MissingValue(&'static str),
    Repeated(&'static str),
    InvalidValue {
        flag: &'static str,
        value: String,
        expected: &'static str,
    },
    UnknownScenario(String),
    ScenarioWithoutOut,
    OutWithoutScenario,
    AggregateWithOthers,
    /// Сценарий вида идёт только с этим флагом.
    AxisRequired {
        scenario: &'static str,
        flag: &'static str,
    },
    /// Сценарий стенда от варианта или режима не зависит.
    AxisNotAccepted {
        scenario: &'static str,
        flag: &'static str,
    },
}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
            ArgsError::ScenarioWithoutOut => write!(f, "--scenario требует --out"),
            ArgsError::OutWithoutScenario => write!(f, "--out имеет смысл только с --scenario"),
            ArgsError::AggregateWithOthers => {
                write!(f, "--aggregate не сочетается с другими флагами")
            }
            ArgsError::AxisRequired { scenario, flag } => {
                write!(f, "сценарий {scenario} требует {flag}")
            }
            ArgsError::AxisNotAccepted { scenario, flag } => {
                write!(f, "сценарий {scenario} идёт без {flag}")
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Flag {
    Scenario,
    Variant,
    Mode,
    Out,
    Aggregate,
}

impl Flag {
    const ALL: [Flag; 5] = [
        Flag::Scenario,
        Flag::Variant,
        Flag::Mode,
        Flag::Out,
        Flag::Aggregate,
    ];

    fn name(self) -> &'static str {
        match self {
            Flag::Scenario => "--scenario",
            Flag::Variant => "--variant",
            Flag::Mode => "--mode",
            Flag::Out => "--out",
            Flag::Aggregate => "--aggregate",
        }
    }

    fn from_arg(arg: &str) -> Option<Flag> {
        Flag::ALL.into_iter().find(|flag| flag.name() == arg)
    }
}

#[derive(Default)]
struct Raw {
    scenario: Option<Scenario>,
    variant: Option<Variant>,
    mode: Option<ViewMode>,
    out: Option<PathBuf>,
    aggregate: Option<PathBuf>,
}

/// Разбирает аргументы без имени программы.
pub fn parse(args: &[String]) -> Result<Command, ArgsError> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Command::Help);
    }

    let mut raw = Raw::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let flag = Flag::from_arg(arg).ok_or_else(|| ArgsError::UnknownArgument(arg.clone()))?;
        let value = match rest.next() {
            Some(value) if !value.starts_with("--") => value.as_str(),
            _ => return Err(ArgsError::MissingValue(flag.name())),
        };
        match flag {
            Flag::Scenario => {
                let scenario = Scenario::from_name(value)
                    .ok_or_else(|| ArgsError::UnknownScenario(value.to_owned()))?;
                set(&mut raw.scenario, flag, scenario)?;
            }
            Flag::Variant => {
                let variant =
                    Variant::from_name(value).ok_or_else(|| invalid(flag, value, "a или b"))?;
                set(&mut raw.variant, flag, variant)?;
            }
            Flag::Mode => {
                let mode = ViewMode::from_name(value)
                    .ok_or_else(|| invalid(flag, value, "unified или side"))?;
                set(&mut raw.mode, flag, mode)?;
            }
            Flag::Out => set(&mut raw.out, flag, path(flag, value)?)?,
            Flag::Aggregate => set(&mut raw.aggregate, flag, path(flag, value)?)?,
        }
    }

    if let Some(run_dir) = raw.aggregate {
        let others = raw.scenario.is_some()
            || raw.variant.is_some()
            || raw.mode.is_some()
            || raw.out.is_some();
        return if others {
            Err(ArgsError::AggregateWithOthers)
        } else {
            Ok(Command::Aggregate { run_dir })
        };
    }

    let view = ViewOptions {
        variant: raw.variant,
        mode: raw.mode,
    };
    match (raw.scenario, raw.out) {
        (Some(scenario), Some(out)) => {
            check_axes(scenario.name(), scenario.axes(), view)?;
            Ok(Command::Scenario {
                scenario,
                view,
                out,
            })
        }
        (Some(_), None) => Err(ArgsError::ScenarioWithoutOut),
        (None, Some(_)) => Err(ArgsError::OutWithoutScenario),
        (None, None) => Ok(Command::Demo(view)),
    }
}

/// Вариант и режим указаны ровно у сценариев, которые идут по этим осям.
fn check_axes(scenario: &'static str, axes: Axes, view: ViewOptions) -> Result<(), ArgsError> {
    for (wanted, given, flag) in [
        (axes.variants, view.variant.is_some(), Flag::Variant),
        (axes.modes, view.mode.is_some(), Flag::Mode),
    ] {
        match (wanted, given) {
            (true, false) => {
                return Err(ArgsError::AxisRequired {
                    scenario,
                    flag: flag.name(),
                });
            }
            (false, true) => {
                return Err(ArgsError::AxisNotAccepted {
                    scenario,
                    flag: flag.name(),
                });
            }
            (true, true) | (false, false) => {}
        }
    }
    Ok(())
}

fn set<T>(slot: &mut Option<T>, flag: Flag, value: T) -> Result<(), ArgsError> {
    if slot.is_some() {
        return Err(ArgsError::Repeated(flag.name()));
    }
    *slot = Some(value);
    Ok(())
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

    fn parse_str(args: &[&str]) -> Result<Command, ArgsError> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        parse(&args)
    }

    #[test]
    fn no_arguments_is_demo_without_flags() {
        assert_eq!(parse_str(&[]), Ok(Command::Demo(ViewOptions::default())));
    }

    #[test]
    fn demo_accepts_variant_and_mode() {
        assert_eq!(
            parse_str(&["--variant", "b", "--mode", "side"]),
            Ok(Command::Demo(ViewOptions {
                variant: Some(Variant::B),
                mode: Some(ViewMode::Side),
            }))
        );
        assert_eq!(
            parse_str(&["--mode", "unified"]),
            Ok(Command::Demo(ViewOptions {
                variant: None,
                mode: Some(ViewMode::Unified),
            }))
        );
    }

    #[test]
    fn stand_scenario_with_out() {
        assert_eq!(
            parse_str(&["--out", "/tmp/run/smoke", "--scenario", "smoke"]),
            Ok(Command::Scenario {
                scenario: Scenario::Smoke,
                view: ViewOptions::default(),
                out: PathBuf::from("/tmp/run/smoke"),
            })
        );
    }

    #[test]
    fn stand_scenario_rejects_variant_and_mode() {
        assert_eq!(
            parse_str(&["--scenario", "smoke", "--out", "o", "--variant", "a"]),
            Err(ArgsError::AxisNotAccepted {
                scenario: "smoke",
                flag: "--variant"
            })
        );
        assert_eq!(
            parse_str(&["--scenario", "smoke", "--out", "o", "--mode", "side"]),
            Err(ArgsError::AxisNotAccepted {
                scenario: "smoke",
                flag: "--mode"
            })
        );
    }

    #[test]
    fn view_axes_are_required_and_checked_one_by_one() {
        let both = Axes {
            variants: true,
            modes: true,
        };
        let full = ViewOptions {
            variant: Some(Variant::A),
            mode: Some(ViewMode::Side),
        };
        assert_eq!(check_axes("open", both, full), Ok(()));
        assert_eq!(
            check_axes("open", both, ViewOptions { mode: None, ..full }),
            Err(ArgsError::AxisRequired {
                scenario: "open",
                flag: "--mode"
            })
        );
        assert_eq!(
            check_axes("open", both, ViewOptions::default()),
            Err(ArgsError::AxisRequired {
                scenario: "open",
                flag: "--variant"
            })
        );
        let variants_only = Axes {
            variants: true,
            modes: false,
        };
        assert_eq!(
            check_axes(
                "bounded",
                variants_only,
                ViewOptions {
                    variant: Some(Variant::B),
                    mode: None
                }
            ),
            Ok(())
        );
        assert_eq!(
            check_axes("bounded", variants_only, full),
            Err(ArgsError::AxisNotAccepted {
                scenario: "bounded",
                flag: "--mode"
            })
        );
        assert_eq!(
            check_axes("smoke", Axes::STAND, ViewOptions::default()),
            Ok(())
        );
    }

    #[test]
    fn aggregate_alone() {
        assert_eq!(
            parse_str(&["--aggregate", "target/probes/diff-view/run"]),
            Ok(Command::Aggregate {
                run_dir: PathBuf::from("target/probes/diff-view/run"),
            })
        );
    }

    #[test]
    fn aggregate_rejects_other_flags() {
        for extra in [
            ["--scenario", "smoke"],
            ["--variant", "a"],
            ["--mode", "side"],
            ["--out", "out"],
        ] {
            let mut args = vec!["--aggregate", "run"];
            args.extend(extra);
            assert_eq!(
                parse_str(&args),
                Err(ArgsError::AggregateWithOthers),
                "{extra:?}"
            );
        }
    }

    #[test]
    fn help() {
        assert_eq!(parse_str(&["--help"]), Ok(Command::Help));
        assert_eq!(parse_str(&["--variant", "a", "-h"]), Ok(Command::Help));
    }

    #[test]
    fn unknown_scenario() {
        assert_eq!(
            parse_str(&["--scenario", "nope", "--out", "out"]),
            Err(ArgsError::UnknownScenario("nope".to_owned()))
        );
    }

    #[test]
    fn names_round_trip() {
        for scenario in Scenario::ALL {
            assert_eq!(Scenario::from_name(scenario.name()), Some(scenario));
        }
        for variant in Variant::ALL {
            assert_eq!(Variant::from_name(variant.name()), Some(variant));
        }
        for mode in ViewMode::ALL {
            assert_eq!(ViewMode::from_name(mode.name()), Some(mode));
        }
        assert_eq!(
            serde_json::to_string(&[ViewMode::Unified, ViewMode::Side]).unwrap(),
            r#"["unified","side"]"#
        );
        assert_eq!(serde_json::to_string(&Variant::B).unwrap(), r#""b""#);
    }

    #[test]
    fn scenario_and_out_require_each_other() {
        assert_eq!(
            parse_str(&["--scenario", "smoke"]),
            Err(ArgsError::ScenarioWithoutOut)
        );
        assert_eq!(
            parse_str(&["--out", "out"]),
            Err(ArgsError::OutWithoutScenario)
        );
    }

    #[test]
    fn invalid_values() {
        for (flag, value) in [
            ("--variant", "c"),
            ("--variant", "A"),
            ("--mode", "split"),
            ("--mode", "Side"),
        ] {
            assert!(
                matches!(
                    parse_str(&[flag, value]),
                    Err(ArgsError::InvalidValue { flag: f, value: v, .. }) if f == flag && v == value
                ),
                "{flag} {value}"
            );
        }
        assert!(matches!(
            parse_str(&["--scenario", "smoke", "--out", ""]),
            Err(ArgsError::InvalidValue { flag: "--out", .. })
        ));
        assert!(matches!(
            parse_str(&["--aggregate", ""]),
            Err(ArgsError::InvalidValue {
                flag: "--aggregate",
                ..
            })
        ));
    }

    #[test]
    fn missing_value() {
        assert_eq!(
            parse_str(&["--variant"]),
            Err(ArgsError::MissingValue("--variant"))
        );
        assert_eq!(
            parse_str(&["--out", "--scenario", "smoke"]),
            Err(ArgsError::MissingValue("--out"))
        );
    }

    #[test]
    fn repeated_flag() {
        assert_eq!(
            parse_str(&["--mode", "side", "--mode", "unified"]),
            Err(ArgsError::Repeated("--mode"))
        );
        assert_eq!(
            parse_str(&["--scenario", "smoke", "--scenario", "smoke", "--out", "o"]),
            Err(ArgsError::Repeated("--scenario"))
        );
    }

    #[test]
    fn unknown_argument() {
        assert_eq!(
            parse_str(&["--fast"]),
            Err(ArgsError::UnknownArgument("--fast".to_owned()))
        );
        assert_eq!(
            parse_str(&["smoke"]),
            Err(ArgsError::UnknownArgument("smoke".to_owned()))
        );
        assert_eq!(
            parse_str(&["--variant=a"]),
            Err(ArgsError::UnknownArgument("--variant=a".to_owned()))
        );
    }

    #[test]
    fn errors_have_messages() {
        let message = ArgsError::UnknownScenario("nope".to_owned()).to_string();
        assert!(
            message.contains("nope") && message.contains("smoke"),
            "{message}"
        );
        assert_eq!(
            ArgsError::AxisRequired {
                scenario: "open",
                flag: "--variant"
            }
            .to_string(),
            "сценарий open требует --variant"
        );
        assert_eq!(
            ArgsError::AxisNotAccepted {
                scenario: "smoke",
                flag: "--mode"
            }
            .to_string(),
            "сценарий smoke идёт без --mode"
        );
    }
}
