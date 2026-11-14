//! Разбор аргументов командной строки. Модуль не зависит от AppKit.

use std::fmt;
use std::path::PathBuf;

use crate::env;
use crate::scenario::Scenario;

pub const USAGE: &str = "\
использование:
  probe-ghostty-surface [--route direct|launch-services] [--nonce <строка>]
      демонстрационная сцена для ручной работы
  probe-ghostty-surface --scenario <имя> --out <каталог> [--variant <имя>] [--route …] [--nonce …]
      сценарий стенда, итог в <каталог>
  probe-ghostty-surface --aggregate <каталог прогона>
      сводка результатов прогона без окна
  probe-ghostty-surface --fixture --channel <сокет> --nonce <строка>
      фикстура в PTY, без AppKit и без ghostty_init
  probe-ghostty-surface --launch --app <бандл> [--route …] [--set ИМЯ=ЗНАЧЕНИЕ]… -- [аргументы пробы]
      демонстрация или сценарий из бандла с построенным окружением и внешним сторожем

--route и --nonce процессу пробы передаёт --launch; --variant по умолчанию base, имя варианта —
строчные латинские буквы и цифры";

pub const DEFAULT_VARIANT: &str = "base";

/// Маршрут запуска процесса пробы.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Route {
    /// Исполняемый файл бандла запускается напрямую.
    #[default]
    Direct,
    /// Бандл открывается через `open`, то есть LaunchServices.
    LaunchServices,
}

impl Route {
    pub fn name(self) -> &'static str {
        match self {
            Route::Direct => "direct",
            Route::LaunchServices => "launch-services",
        }
    }
}

/// Сведения, которые процессу пробы передаёт `--launch`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub route: Route,
    pub nonce: Option<String>,
}

/// Что запускает `--launch`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Demo,
    Scenario {
        scenario: Scenario,
        variant: String,
        out: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub app: PathBuf,
    pub route: Route,
    /// Переменные варианта: имя и значение.
    pub set: Vec<(String, String)>,
    pub target: Target,
    /// Аргументы процесса пробы после `--`, без `--route` и `--nonce`.
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Help,
    Demo(Run),
    Scenario {
        scenario: Scenario,
        variant: String,
        out: PathBuf,
        run: Run,
    },
    Aggregate {
        run_dir: PathBuf,
    },
    Fixture {
        channel: PathBuf,
        nonce: String,
    },
    Launch(Launch),
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
    /// Этот флаг процессу пробы выставляет сам `--launch`.
    LaunchOwns(&'static str),
    /// После `--` у `--launch` ожидается демонстрация или сценарий.
    LaunchTarget,
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
            ArgsError::Requires { mode, flag } => write!(f, "{mode} требует {flag}"),
            ArgsError::NotWith { mode, flag } => write!(f, "{flag} не сочетается с {mode}"),
            ArgsError::TailWithoutLaunch => {
                write!(f, "аргументы после «--» бывают только у --launch")
            }
            ArgsError::LaunchOwns(flag) => {
                write!(f, "{flag} процессу пробы выставляет сам --launch")
            }
            ArgsError::LaunchTarget => write!(
                f,
                "после «--» у --launch ожидается демонстрация или сценарий"
            ),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flag {
    Scenario,
    Variant,
    Out,
    Route,
    Nonce,
    Aggregate,
    Fixture,
    Channel,
    Launch,
    App,
    Set,
}

impl Flag {
    const ALL: [Flag; 11] = [
        Flag::Scenario,
        Flag::Variant,
        Flag::Out,
        Flag::Route,
        Flag::Nonce,
        Flag::Aggregate,
        Flag::Fixture,
        Flag::Channel,
        Flag::Launch,
        Flag::App,
        Flag::Set,
    ];

    fn name(self) -> &'static str {
        match self {
            Flag::Scenario => "--scenario",
            Flag::Variant => "--variant",
            Flag::Out => "--out",
            Flag::Route => "--route",
            Flag::Nonce => "--nonce",
            Flag::Aggregate => "--aggregate",
            Flag::Fixture => "--fixture",
            Flag::Channel => "--channel",
            Flag::Launch => "--launch",
            Flag::App => "--app",
            Flag::Set => "--set",
        }
    }

    fn takes_value(self) -> bool {
        !matches!(self, Flag::Fixture | Flag::Launch)
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
    route: Option<Route>,
    nonce: Option<String>,
    aggregate: Option<PathBuf>,
    fixture: bool,
    channel: Option<PathBuf>,
    launch: bool,
    app: Option<PathBuf>,
    set: Vec<(String, String)>,
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
    if raw.fixture {
        if let Some(flag) = raw.extra(&[Flag::Fixture, Flag::Channel, Flag::Nonce]) {
            return Err(not_with(Flag::Fixture, flag));
        }
        let channel = raw.channel.ok_or(requires(Flag::Fixture, Flag::Channel))?;
        let nonce = raw.nonce.ok_or(requires(Flag::Fixture, Flag::Nonce))?;
        return Ok(Mode::Fixture { channel, nonce });
    }
    for flag in [Flag::Channel, Flag::App, Flag::Set] {
        if raw.seen.contains(&flag) {
            let mode = match flag {
                Flag::Channel => Flag::Fixture,
                _ => Flag::Launch,
            };
            return Err(requires(flag, mode));
        }
    }

    let run = Run {
        route: raw.route.unwrap_or_default(),
        nonce: raw.nonce,
    };
    match (raw.scenario, raw.out) {
        (Some(scenario), Some(out)) => Ok(Mode::Scenario {
            scenario,
            variant: raw.variant.unwrap_or_else(|| DEFAULT_VARIANT.to_owned()),
            out,
            run,
        }),
        (Some(_), None) => Err(requires(Flag::Scenario, Flag::Out)),
        (None, Some(_)) => Err(requires(Flag::Out, Flag::Scenario)),
        (None, None) if raw.variant.is_some() => Err(requires(Flag::Variant, Flag::Scenario)),
        (None, None) => Ok(Mode::Demo(run)),
    }
}

fn parse_flags(args: &[String]) -> Result<Raw, ArgsError> {
    let mut raw = Raw::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let flag = Flag::from_arg(arg).ok_or_else(|| ArgsError::UnknownArgument(arg.clone()))?;
        if raw.seen.contains(&flag) && flag != Flag::Set {
            return Err(ArgsError::Repeated(flag.name()));
        }
        raw.seen.push(flag);
        if !flag.takes_value() {
            match flag {
                Flag::Fixture => raw.fixture = true,
                _ => raw.launch = true,
            }
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
            Flag::Route => {
                let route = match value {
                    "direct" => Route::Direct,
                    "launch-services" => Route::LaunchServices,
                    _ => return Err(invalid(flag, value, "direct или launch-services")),
                };
                raw.route = Some(route);
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
            Flag::Set => {
                let (name, assigned) = value
                    .split_once('=')
                    .ok_or_else(|| invalid(flag, value, "ИМЯ=ЗНАЧЕНИЕ"))?;
                if !env::VARIANT_VARS.contains(&name) {
                    return Err(invalid(
                        flag,
                        value,
                        "переменную варианта из env::VARIANT_VARS",
                    ));
                }
                if raw.set.iter().any(|(known, _)| known == name) {
                    return Err(ArgsError::Repeated(flag.name()));
                }
                raw.set.push((name.to_owned(), assigned.to_owned()));
            }
            Flag::Out => raw.out = Some(path(flag, value)?),
            Flag::Aggregate => raw.aggregate = Some(path(flag, value)?),
            Flag::Channel => raw.channel = Some(path(flag, value)?),
            Flag::App => raw.app = Some(path(flag, value)?),
            Flag::Fixture | Flag::Launch => {}
        }
    }
    Ok(raw)
}

fn launch(raw: Raw, tail: &[String]) -> Result<Mode, ArgsError> {
    if let Some(flag) = raw.extra(&[Flag::Launch, Flag::App, Flag::Route, Flag::Set]) {
        return Err(not_with(Flag::Launch, flag));
    }
    let app = raw.app.ok_or(requires(Flag::Launch, Flag::App))?;
    for owned in [Flag::Route, Flag::Nonce] {
        if tail.iter().any(|arg| arg == owned.name()) {
            return Err(ArgsError::LaunchOwns(owned.name()));
        }
    }
    let target = match parse(tail)? {
        Mode::Demo(_) => Target::Demo,
        Mode::Scenario {
            scenario,
            variant,
            out,
            ..
        } => Target::Scenario {
            scenario,
            variant,
            out,
        },
        Mode::Help | Mode::Aggregate { .. } | Mode::Fixture { .. } | Mode::Launch(_) => {
            return Err(ArgsError::LaunchTarget);
        }
    };
    Ok(Mode::Launch(Launch {
        app,
        route: raw.route.unwrap_or_default(),
        set: raw.set,
        target,
        args: tail.to_vec(),
    }))
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

    #[test]
    fn no_arguments_is_demo_with_direct_route() {
        assert_eq!(parse_str(&[]), Ok(Mode::Demo(Run::default())));
        assert_eq!(Run::default().route, Route::Direct);
    }

    #[test]
    fn demo_accepts_route_and_nonce() {
        assert_eq!(
            parse_str(&["--route", "launch-services", "--nonce", "a1-b2"]),
            Ok(Mode::Demo(Run {
                route: Route::LaunchServices,
                nonce: Some("a1-b2".to_owned()),
            }))
        );
    }

    #[test]
    fn scenario_with_out_and_default_variant() {
        assert_eq!(
            parse_str(&["--out", "/tmp/run/window-base", "--scenario", "window"]),
            Ok(Mode::Scenario {
                scenario: Scenario::Window,
                variant: DEFAULT_VARIANT.to_owned(),
                out: PathBuf::from("/tmp/run/window-base"),
                run: Run::default(),
            })
        );
    }

    #[test]
    fn scenario_with_variant_route_and_nonce() {
        assert_eq!(
            parse_str(&[
                "--scenario",
                "window",
                "--variant",
                "scribble",
                "--out",
                "out",
                "--route",
                "direct",
                "--nonce",
                "ff00"
            ]),
            Ok(Mode::Scenario {
                scenario: Scenario::Window,
                variant: "scribble".to_owned(),
                out: PathBuf::from("out"),
                run: Run {
                    route: Route::Direct,
                    nonce: Some("ff00".to_owned()),
                },
            })
        );
    }

    #[test]
    fn variant_must_not_contain_dash_or_capitals() {
        for value in ["with-dash", "Base", "", "a b"] {
            assert!(
                matches!(
                    parse_str(&["--scenario", "window", "--out", "o", "--variant", value]),
                    Err(ArgsError::InvalidValue {
                        flag: "--variant",
                        ..
                    }) | Err(ArgsError::MissingValue("--variant"))
                ),
                "{value:?}"
            );
        }
        assert_eq!(
            parse_str(&["--variant", "base"]),
            Err(ArgsError::Requires {
                mode: "--variant",
                flag: "--scenario"
            })
        );
    }

    #[test]
    fn aggregate_alone() {
        assert_eq!(
            parse_str(&["--aggregate", "target/probes/ghostty-surface/run"]),
            Ok(Mode::Aggregate {
                run_dir: PathBuf::from("target/probes/ghostty-surface/run"),
            })
        );
        for extra in [
            vec!["--scenario", "window"],
            vec!["--out", "out"],
            vec!["--fixture"],
            vec!["--route", "direct"],
        ] {
            let mut args = vec!["--aggregate", "run"];
            args.extend(extra.iter().copied());
            assert!(
                matches!(
                    parse_str(&args),
                    Err(ArgsError::NotWith {
                        mode: "--aggregate",
                        ..
                    })
                ),
                "{extra:?}"
            );
        }
    }

    #[test]
    fn fixture_requires_channel_and_nonce() {
        assert_eq!(
            parse_str(&["--fixture", "--channel", "/tmp/s.sock", "--nonce", "abc"]),
            Ok(Mode::Fixture {
                channel: PathBuf::from("/tmp/s.sock"),
                nonce: "abc".to_owned(),
            })
        );
        assert_eq!(
            parse_str(&["--fixture", "--nonce", "abc"]),
            Err(ArgsError::Requires {
                mode: "--fixture",
                flag: "--channel"
            })
        );
        assert_eq!(
            parse_str(&["--fixture", "--channel", "s"]),
            Err(ArgsError::Requires {
                mode: "--fixture",
                flag: "--nonce"
            })
        );
        assert_eq!(
            parse_str(&["--fixture", "--channel", "s", "--nonce", "n", "--out", "o"]),
            Err(ArgsError::NotWith {
                mode: "--fixture",
                flag: "--out"
            })
        );
        assert_eq!(
            parse_str(&["--channel", "s"]),
            Err(ArgsError::Requires {
                mode: "--channel",
                flag: "--fixture"
            })
        );
    }

    #[test]
    fn launch_of_scenario() {
        assert_eq!(
            parse_str(&[
                "--launch",
                "--app",
                "target/probes/ghostty-surface/bundle/probe-ghostty-surface.app",
                "--set",
                "MallocScribble=1",
                "--",
                "--scenario",
                "window",
                "--out",
                "run/window-base"
            ]),
            Ok(Mode::Launch(Launch {
                app: PathBuf::from(
                    "target/probes/ghostty-surface/bundle/probe-ghostty-surface.app"
                ),
                route: Route::Direct,
                set: vec![("MallocScribble".to_owned(), "1".to_owned())],
                target: Target::Scenario {
                    scenario: Scenario::Window,
                    variant: DEFAULT_VARIANT.to_owned(),
                    out: PathBuf::from("run/window-base"),
                },
                args: strings(&["--scenario", "window", "--out", "run/window-base"]),
            }))
        );
    }

    #[test]
    fn launch_of_demo_with_and_without_tail() {
        let expected = |args: Vec<String>| {
            Ok(Mode::Launch(Launch {
                app: PathBuf::from("p.app"),
                route: Route::LaunchServices,
                set: Vec::new(),
                target: Target::Demo,
                args,
            }))
        };
        assert_eq!(
            parse_str(&["--launch", "--app", "p.app", "--route", "launch-services"]),
            expected(Vec::new())
        );
        assert_eq!(
            parse_str(&[
                "--launch",
                "--app",
                "p.app",
                "--route",
                "launch-services",
                "--"
            ]),
            expected(Vec::new())
        );
    }

    #[test]
    fn launch_rejects_owned_flags_and_other_targets() {
        assert_eq!(
            parse_str(&["--launch", "--app", "p.app", "--", "--nonce", "x"]),
            Err(ArgsError::LaunchOwns("--nonce"))
        );
        assert_eq!(
            parse_str(&["--launch", "--app", "p.app", "--", "--route", "direct"]),
            Err(ArgsError::LaunchOwns("--route"))
        );
        assert_eq!(
            parse_str(&["--launch", "--app", "p.app", "--", "--aggregate", "run"]),
            Err(ArgsError::LaunchTarget)
        );
        assert_eq!(
            parse_str(&["--launch", "--app", "p.app", "--", "--launch", "--app", "q"]),
            Err(ArgsError::LaunchTarget)
        );
        assert_eq!(
            parse_str(&["--launch", "--", "--scenario", "window", "--out", "o"]),
            Err(ArgsError::Requires {
                mode: "--launch",
                flag: "--app"
            })
        );
        assert_eq!(
            parse_str(&["--launch", "--app", "p.app", "--scenario", "window"]),
            Err(ArgsError::NotWith {
                mode: "--launch",
                flag: "--scenario"
            })
        );
        assert_eq!(
            parse_str(&["--scenario", "window", "--out", "o", "--", "x"]),
            Err(ArgsError::TailWithoutLaunch)
        );
        assert_eq!(
            parse_str(&["--set", "MallocScribble=1"]),
            Err(ArgsError::Requires {
                mode: "--set",
                flag: "--launch"
            })
        );
    }

    #[test]
    fn set_accepts_only_variant_variables_once() {
        assert!(matches!(
            parse_str(&["--launch", "--app", "p.app", "--set", "LANG=C"]),
            Err(ArgsError::InvalidValue { flag: "--set", .. })
        ));
        assert!(matches!(
            parse_str(&["--launch", "--app", "p.app", "--set", "MallocScribble"]),
            Err(ArgsError::InvalidValue { flag: "--set", .. })
        ));
        assert_eq!(
            parse_str(&[
                "--launch",
                "--app",
                "p.app",
                "--set",
                "MallocScribble=1",
                "--set",
                "MallocScribble=0"
            ]),
            Err(ArgsError::Repeated("--set"))
        );
    }

    #[test]
    fn help() {
        assert_eq!(parse_str(&["--help"]), Ok(Mode::Help));
        assert_eq!(parse_str(&["--route", "direct", "-h"]), Ok(Mode::Help));
    }

    #[test]
    fn every_known_scenario_round_trips() {
        for scenario in Scenario::ALL {
            assert_eq!(Scenario::from_name(scenario.name()), Some(scenario));
            assert!(matches!(
                parse_str(&["--scenario", scenario.name(), "--out", "out"]),
                Ok(Mode::Scenario { scenario: parsed, .. }) if parsed == scenario
            ));
        }
        assert_eq!(
            parse_str(&["--scenario", "nope", "--out", "out"]),
            Err(ArgsError::UnknownScenario("nope".to_owned()))
        );
    }

    #[test]
    fn scenario_and_out_require_each_other() {
        assert_eq!(
            parse_str(&["--scenario", "window"]),
            Err(ArgsError::Requires {
                mode: "--scenario",
                flag: "--out"
            })
        );
        assert_eq!(
            parse_str(&["--out", "out"]),
            Err(ArgsError::Requires {
                mode: "--out",
                flag: "--scenario"
            })
        );
    }

    #[test]
    fn invalid_values_missing_values_and_repeats() {
        assert!(matches!(
            parse_str(&["--route", "open"]),
            Err(ArgsError::InvalidValue {
                flag: "--route",
                ..
            })
        ));
        assert!(matches!(
            parse_str(&["--nonce", "a/b"]),
            Err(ArgsError::InvalidValue {
                flag: "--nonce",
                ..
            })
        ));
        assert!(matches!(
            parse_str(&["--scenario", "window", "--out", ""]),
            Err(ArgsError::InvalidValue { flag: "--out", .. })
        ));
        assert_eq!(
            parse_str(&["--route"]),
            Err(ArgsError::MissingValue("--route"))
        );
        assert_eq!(
            parse_str(&["--out", "--scenario", "window"]),
            Err(ArgsError::MissingValue("--out"))
        );
        assert_eq!(
            parse_str(&["--route", "direct", "--route", "direct"]),
            Err(ArgsError::Repeated("--route"))
        );
        assert_eq!(
            parse_str(&["--launch", "--launch", "--app", "p"]),
            Err(ArgsError::Repeated("--launch"))
        );
    }

    #[test]
    fn unknown_argument() {
        for arg in ["--fast", "launch", "--route=direct"] {
            assert_eq!(
                parse_str(&[arg]),
                Err(ArgsError::UnknownArgument(arg.to_owned()))
            );
        }
    }

    #[test]
    fn errors_have_messages() {
        let message = ArgsError::UnknownScenario("nope".to_owned()).to_string();
        assert!(
            message.contains("nope") && message.contains("window"),
            "{message}"
        );
        assert_eq!(
            requires(Flag::Scenario, Flag::Out).to_string(),
            "--scenario требует --out"
        );
        assert_eq!(
            ArgsError::LaunchOwns("--nonce").to_string(),
            "--nonce процессу пробы выставляет сам --launch"
        );
    }
}
