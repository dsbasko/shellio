//! Разбор аргументов командной строки. Модуль не зависит от AppKit.

use std::fmt;
use std::path::PathBuf;

use crate::scenario::Scenario;

pub const USAGE: &str = "\
использование:
  probe-appkit-shell [--split sidebar|plain] [--contrast normal|more] [--motion normal|reduced]
      демонстрационная сцена для ручной работы
  probe-appkit-shell --scenario <имя> --out <каталог> [--split …] [--contrast …] [--motion …]
      сценарий стенда, итог в <каталог>
  probe-appkit-shell --aggregate <каталог прогона>
      сводка результатов прогона без окна

без --contrast и --motion действуют системные настройки, --split по умолчанию plain";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Split {
    Sidebar,
    #[default]
    Plain,
}

impl Split {
    pub fn name(self) -> &'static str {
        match self {
            Split::Sidebar => "sidebar",
            Split::Plain => "plain",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contrast {
    Normal,
    More,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Normal,
    Reduced,
}

/// Настройки сцены. `None` у контраста и движения означает системное значение.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SceneOptions {
    pub split: Split,
    pub contrast: Option<Contrast>,
    pub motion: Option<Motion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Help,
    Demo(SceneOptions),
    Scenario {
        scenario: Scenario,
        scene: SceneOptions,
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
        }
    }
}

#[derive(Clone, Copy)]
enum Flag {
    Scenario,
    Split,
    Contrast,
    Motion,
    Out,
    Aggregate,
}

impl Flag {
    const ALL: [Flag; 6] = [
        Flag::Scenario,
        Flag::Split,
        Flag::Contrast,
        Flag::Motion,
        Flag::Out,
        Flag::Aggregate,
    ];

    fn name(self) -> &'static str {
        match self {
            Flag::Scenario => "--scenario",
            Flag::Split => "--split",
            Flag::Contrast => "--contrast",
            Flag::Motion => "--motion",
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
    split: Option<Split>,
    contrast: Option<Contrast>,
    motion: Option<Motion>,
    out: Option<PathBuf>,
    aggregate: Option<PathBuf>,
}

/// Разбирает аргументы без имени программы.
pub fn parse(args: &[String]) -> Result<Mode, ArgsError> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Mode::Help);
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
            Flag::Split => {
                let split = match value {
                    "sidebar" => Split::Sidebar,
                    "plain" => Split::Plain,
                    _ => return Err(invalid(flag, value, "sidebar или plain")),
                };
                set(&mut raw.split, flag, split)?;
            }
            Flag::Contrast => {
                let contrast = match value {
                    "normal" => Contrast::Normal,
                    "more" => Contrast::More,
                    _ => return Err(invalid(flag, value, "normal или more")),
                };
                set(&mut raw.contrast, flag, contrast)?;
            }
            Flag::Motion => {
                let motion = match value {
                    "normal" => Motion::Normal,
                    "reduced" => Motion::Reduced,
                    _ => return Err(invalid(flag, value, "normal или reduced")),
                };
                set(&mut raw.motion, flag, motion)?;
            }
            Flag::Out => set(&mut raw.out, flag, path(flag, value)?)?,
            Flag::Aggregate => set(&mut raw.aggregate, flag, path(flag, value)?)?,
        }
    }

    if let Some(run_dir) = raw.aggregate {
        let others = raw.scenario.is_some()
            || raw.split.is_some()
            || raw.contrast.is_some()
            || raw.motion.is_some()
            || raw.out.is_some();
        return if others {
            Err(ArgsError::AggregateWithOthers)
        } else {
            Ok(Mode::Aggregate { run_dir })
        };
    }

    let scene = SceneOptions {
        split: raw.split.unwrap_or_default(),
        contrast: raw.contrast,
        motion: raw.motion,
    };
    match (raw.scenario, raw.out) {
        (Some(scenario), Some(out)) => Ok(Mode::Scenario {
            scenario,
            scene,
            out,
        }),
        (Some(_), None) => Err(ArgsError::ScenarioWithoutOut),
        (None, Some(_)) => Err(ArgsError::OutWithoutScenario),
        (None, None) => Ok(Mode::Demo(scene)),
    }
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

    fn parse_str(args: &[&str]) -> Result<Mode, ArgsError> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        parse(&args)
    }

    #[test]
    fn no_arguments_is_demo_with_defaults() {
        assert_eq!(parse_str(&[]), Ok(Mode::Demo(SceneOptions::default())));
        let defaults = SceneOptions::default();
        assert_eq!(defaults.split, Split::Plain);
        assert_eq!(defaults.contrast, None);
        assert_eq!(defaults.motion, None);
    }

    #[test]
    fn demo_accepts_scene_flags() {
        assert_eq!(
            parse_str(&[
                "--split",
                "sidebar",
                "--contrast",
                "more",
                "--motion",
                "reduced"
            ]),
            Ok(Mode::Demo(SceneOptions {
                split: Split::Sidebar,
                contrast: Some(Contrast::More),
                motion: Some(Motion::Reduced),
            }))
        );
        assert_eq!(
            parse_str(&[
                "--split",
                "plain",
                "--contrast",
                "normal",
                "--motion",
                "normal"
            ]),
            Ok(Mode::Demo(SceneOptions {
                split: Split::Plain,
                contrast: Some(Contrast::Normal),
                motion: Some(Motion::Normal),
            }))
        );
    }

    #[test]
    fn scenario_with_out() {
        assert_eq!(
            parse_str(&["--out", "/tmp/run/launch", "--scenario", "launch"]),
            Ok(Mode::Scenario {
                scenario: Scenario::Launch,
                scene: SceneOptions::default(),
                out: PathBuf::from("/tmp/run/launch"),
            })
        );
    }

    #[test]
    fn scenario_with_scene_flags() {
        assert_eq!(
            parse_str(&[
                "--scenario",
                "launch",
                "--split",
                "sidebar",
                "--motion",
                "reduced",
                "--out",
                "out"
            ]),
            Ok(Mode::Scenario {
                scenario: Scenario::Launch,
                scene: SceneOptions {
                    split: Split::Sidebar,
                    contrast: None,
                    motion: Some(Motion::Reduced),
                },
                out: PathBuf::from("out"),
            })
        );
    }

    #[test]
    fn aggregate_alone() {
        assert_eq!(
            parse_str(&["--aggregate", "target/probes/appkit-shell/run"]),
            Ok(Mode::Aggregate {
                run_dir: PathBuf::from("target/probes/appkit-shell/run"),
            })
        );
    }

    #[test]
    fn aggregate_rejects_other_flags() {
        for extra in [
            ["--scenario", "launch"],
            ["--split", "plain"],
            ["--contrast", "more"],
            ["--motion", "normal"],
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
        assert_eq!(parse_str(&["--help"]), Ok(Mode::Help));
        assert_eq!(parse_str(&["--split", "plain", "-h"]), Ok(Mode::Help));
    }

    #[test]
    fn unknown_scenario() {
        assert_eq!(
            parse_str(&["--scenario", "nope", "--out", "out"]),
            Err(ArgsError::UnknownScenario("nope".to_owned()))
        );
    }

    #[test]
    fn every_known_scenario_round_trips() {
        for scenario in Scenario::ALL {
            assert_eq!(Scenario::from_name(scenario.name()), Some(scenario));
            assert_eq!(
                parse_str(&["--scenario", scenario.name(), "--out", "out"]),
                Ok(Mode::Scenario {
                    scenario,
                    scene: SceneOptions::default(),
                    out: PathBuf::from("out"),
                })
            );
        }
    }

    #[test]
    fn scenario_and_out_require_each_other() {
        assert_eq!(
            parse_str(&["--scenario", "launch"]),
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
            ("--split", "inspector"),
            ("--contrast", "high"),
            ("--motion", "off"),
            ("--split", "Sidebar"),
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
            parse_str(&["--scenario", "launch", "--out", ""]),
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
            parse_str(&["--split"]),
            Err(ArgsError::MissingValue("--split"))
        );
        assert_eq!(
            parse_str(&["--out", "--scenario", "launch"]),
            Err(ArgsError::MissingValue("--out"))
        );
    }

    #[test]
    fn repeated_flag() {
        assert_eq!(
            parse_str(&["--split", "plain", "--split", "sidebar"]),
            Err(ArgsError::Repeated("--split"))
        );
        assert_eq!(
            parse_str(&["--scenario", "launch", "--scenario", "launch", "--out", "o"]),
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
            parse_str(&["launch"]),
            Err(ArgsError::UnknownArgument("launch".to_owned()))
        );
        assert_eq!(
            parse_str(&["--split=plain"]),
            Err(ArgsError::UnknownArgument("--split=plain".to_owned()))
        );
    }

    #[test]
    fn errors_have_messages() {
        let message = ArgsError::UnknownScenario("nope".to_owned()).to_string();
        assert!(
            message.contains("nope") && message.contains("launch"),
            "{message}"
        );
        assert_eq!(
            ArgsError::ScenarioWithoutOut.to_string(),
            "--scenario требует --out"
        );
    }
}
