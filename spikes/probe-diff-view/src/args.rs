//! Разбор аргументов командной строки и перечень сценариев. Модуль не зависит от AppKit.
//!
//! Источник — `spikes/probe-appkit-shell/src/args.rs` (P0.2). Упрощён: вместо сплита и настроек
//! сцены — вариант отрисовки (`a`, `b`) и режим (`unified`, `side`), которые у сценария стенда
//! отсутствуют, а у сценария вида обязательны.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use probe_diff_view::open::OpenFixture;

pub const USAGE: &str = "\
использование:
  probe-diff-view [--variant a|b] [--mode unified|side]
      демонстрация без стенда и result.json
  probe-diff-view --scenario <имя> --out <каталог> [--variant a|b] [--mode unified|side]
                  [--fixture added|modified|sparse|wrap|real|threads] [--fixtures <каталог фикстур>]
                  [--frames-path none|xctrace|screencapturekit]
      сценарий стенда, итог в <каталог>/result.json
  probe-diff-view --aggregate <каталог прогона>
      сводка результатов прогона без окна
  probe-diff-view --reference <каталог фикстур>
      эталоны подсветки фикстур в <каталог фикстур>/<имя>/reference.json
  probe-diff-view --neighbor
      служебный сосед калибровки: движущееся окно до закрытия stdin
  probe-diff-view --ax-client <pid>
      внешний клиент доступности сценария ax: снимки дерева AX процесса <pid> по командам stdin

у демонстрации по умолчанию --variant a --mode unified; сценарий стенда идёт без --variant и
--mode, сценарий вида требует обоих, сценарии открытия и кадров также --fixture; --fixtures нужен
ровно сценариям с эталоном подсветки; --frames-path — только у frames, по умолчанию none";

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

/// Путь замера кадров сценария `frames`. Значение ставит исполнитель по объединённому итогу
/// калибровки отчёта, сценарий сам итог `calibrate` не читает.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FramesPath {
    /// Без записи трассы: вердикт по кадрам `not_run`, потери в процессе — наблюдение.
    #[default]
    None,
    Xctrace,
    Screencapturekit,
}

impl FramesPath {
    pub const ALL: [FramesPath; 3] = [
        FramesPath::None,
        FramesPath::Xctrace,
        FramesPath::Screencapturekit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            FramesPath::None => "none",
            FramesPath::Xctrace => "xctrace",
            FramesPath::Screencapturekit => "screencapturekit",
        }
    }

    pub fn from_name(name: &str) -> Option<FramesPath> {
        FramesPath::ALL.into_iter().find(|path| path.name() == name)
    }
}

/// По каким осям идёт сценарий. Сценарий стенда — один процесс без варианта и режима, сценарий
/// вида — процесс на каждую пару варианта и режима, сценарий открытия — также на каждую
/// фикстуру. Таблица `run.sh` (`axes_of`) повторяет её.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Axes {
    pub variants: bool,
    pub modes: bool,
    pub fixtures: bool,
}

impl Axes {
    pub const STAND: Axes = Axes {
        variants: false,
        modes: false,
        fixtures: false,
    };
    pub const VIEW: Axes = Axes {
        variants: true,
        modes: true,
        fixtures: false,
    };
    pub const OPEN: Axes = Axes {
        variants: true,
        modes: true,
        fixtures: true,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// Пустое окно: приложение активно, окно видимо и key, `result.json` записан.
    Smoke,
    /// Подсветка на worker без окна: замеры разбора и запросов, сверка с эталоном.
    Highlight,
    /// Калибровка замера кадров: окно с движением по S1, отметка `drawRect:`, сверка пика.
    Calibrate,
    /// Открытие `added` в виде варианта и режима: первый экран нарисован, `G`, `gg`, щелчок.
    OpenSmoke,
    /// Доступность вида: внешний клиент читает строки, выбранность и фокус, стенд сверяет с моделью.
    Ax,
    /// Открытие фикстуры в свежем процессе: отметки до первого экрана с правильной подсветкой.
    Open,
    /// 20 открытий фикстуры в одном процессе без вердикта.
    OpenWarm,
    /// Кадры прокрутки: отрезки S1, бросок S2 у `modified`, ресайз S3 у `wrap` в «Рядом».
    Frames,
}

impl Scenario {
    pub const ALL: [Scenario; 8] = [
        Scenario::Smoke,
        Scenario::Highlight,
        Scenario::Calibrate,
        Scenario::OpenSmoke,
        Scenario::Ax,
        Scenario::Open,
        Scenario::OpenWarm,
        Scenario::Frames,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Smoke => "smoke",
            Scenario::Highlight => "highlight",
            Scenario::Calibrate => "calibrate",
            Scenario::OpenSmoke => "open-smoke",
            Scenario::Ax => "ax",
            Scenario::Open => "open",
            Scenario::OpenWarm => "open-warm",
            Scenario::Frames => "frames",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL
            .into_iter()
            .find(|scenario| scenario.name() == name)
    }

    pub fn axes(self) -> Axes {
        match self {
            Scenario::Smoke | Scenario::Highlight | Scenario::Calibrate => Axes::STAND,
            Scenario::OpenSmoke | Scenario::Ax => Axes::VIEW,
            Scenario::Open | Scenario::OpenWarm | Scenario::Frames => Axes::OPEN,
        }
    }

    /// Фикстуры сценария с осью фикстур, пусто — оси нет. Таблица `run.sh` (`fixtures_of`)
    /// повторяет её.
    pub fn fixtures(self) -> &'static [OpenFixture] {
        match self {
            Scenario::Smoke
            | Scenario::Highlight
            | Scenario::Calibrate
            | Scenario::OpenSmoke
            | Scenario::Ax => &[],
            Scenario::Open | Scenario::OpenWarm => &[
                OpenFixture::Added,
                OpenFixture::Modified,
                OpenFixture::Sparse,
                OpenFixture::Wrap,
                OpenFixture::Real,
            ],
            Scenario::Frames => &[
                OpenFixture::Added,
                OpenFixture::Modified,
                OpenFixture::Threads,
                OpenFixture::Wrap,
            ],
        }
    }

    /// Пара фикстуры и режима идёт процессом сценария: у `frames` фикстура `wrap` — только в
    /// «Рядом» (ресайз S3). `run.sh` (`skip_cell`) повторяет правило.
    pub fn accepts(self, fixture: OpenFixture, mode: ViewMode) -> bool {
        self.fixtures().contains(&fixture)
            && !(self == Scenario::Frames
                && fixture == OpenFixture::Wrap
                && mode == ViewMode::Unified)
    }

    /// Сценарию нужен каталог фикстур с эталонами `--reference`. Таблица `run.sh`
    /// (`needs_fixtures`) повторяет её.
    pub fn needs_fixtures(self) -> bool {
        match self {
            Scenario::Smoke
            | Scenario::Calibrate
            | Scenario::OpenSmoke
            | Scenario::Ax
            | Scenario::Frames => false,
            Scenario::Highlight | Scenario::Open | Scenario::OpenWarm => true,
        }
    }
}

/// Вариант, режим и фикстура открытия из командной строки; `None` — флаг не указан.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewOptions {
    pub variant: Option<Variant>,
    pub mode: Option<ViewMode>,
    pub fixture: Option<OpenFixture>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Demo(ViewOptions),
    Scenario {
        scenario: Scenario,
        view: ViewOptions,
        out: PathBuf,
        fixtures: Option<PathBuf>,
        /// Путь замера кадров, только у `frames`.
        frames_path: Option<FramesPath>,
    },
    Aggregate {
        run_dir: PathBuf,
    },
    Reference {
        dir: PathBuf,
    },
    /// Служебный сосед обратного контроля `calibrate`.
    Neighbor,
    /// Внешний клиент доступности сценария `ax` к процессу `pid`.
    AxClient {
        pid: i32,
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
    FixturesWithoutScenario,
    /// Команда `--aggregate` или `--reference` идёт одна.
    Exclusive(&'static str),
    FixturesRequired(&'static str),
    FixturesNotAccepted(&'static str),
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
    /// Фикстура или пара фикстуры и режима не идёт процессом сценария.
    FixtureNotAccepted {
        scenario: &'static str,
        fixture: &'static str,
        mode: &'static str,
    },
    /// `--frames-path` есть только у `frames`.
    FramesPathNotAccepted(&'static str),
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
            ArgsError::FixturesWithoutScenario => {
                write!(f, "--fixtures имеет смысл только с --scenario")
            }
            ArgsError::Exclusive(flag) => write!(f, "{flag} не сочетается с другими флагами"),
            ArgsError::FixturesRequired(scenario) => {
                write!(f, "сценарий {scenario} требует --fixtures")
            }
            ArgsError::FixturesNotAccepted(scenario) => {
                write!(f, "сценарий {scenario} идёт без --fixtures")
            }
            ArgsError::AxisRequired { scenario, flag } => {
                write!(f, "сценарий {scenario} требует {flag}")
            }
            ArgsError::AxisNotAccepted { scenario, flag } => {
                write!(f, "сценарий {scenario} идёт без {flag}")
            }
            ArgsError::FixtureNotAccepted {
                scenario,
                fixture,
                mode,
            } => write!(
                f,
                "сценарий {scenario} не идёт с фикстурой {fixture} в режиме {mode}"
            ),
            ArgsError::FramesPathNotAccepted(scenario) => {
                write!(f, "сценарий {scenario} идёт без --frames-path")
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flag {
    Scenario,
    Variant,
    Mode,
    Fixture,
    Out,
    Fixtures,
    Aggregate,
    Reference,
    FramesPath,
}

impl Flag {
    const ALL: [Flag; 9] = [
        Flag::Scenario,
        Flag::Variant,
        Flag::Mode,
        Flag::Fixture,
        Flag::Out,
        Flag::Fixtures,
        Flag::Aggregate,
        Flag::Reference,
        Flag::FramesPath,
    ];

    fn name(self) -> &'static str {
        match self {
            Flag::Scenario => "--scenario",
            Flag::Variant => "--variant",
            Flag::Mode => "--mode",
            Flag::Fixture => "--fixture",
            Flag::Out => "--out",
            Flag::Fixtures => "--fixtures",
            Flag::Aggregate => "--aggregate",
            Flag::Reference => "--reference",
            Flag::FramesPath => "--frames-path",
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
    fixture: Option<OpenFixture>,
    out: Option<PathBuf>,
    fixtures: Option<PathBuf>,
    aggregate: Option<PathBuf>,
    reference: Option<PathBuf>,
    frames_path: Option<FramesPath>,
}

impl Raw {
    fn others_than(&self, flag: Flag) -> bool {
        let given = [
            (Flag::Scenario, self.scenario.is_some()),
            (Flag::Variant, self.variant.is_some()),
            (Flag::Mode, self.mode.is_some()),
            (Flag::Fixture, self.fixture.is_some()),
            (Flag::Out, self.out.is_some()),
            (Flag::Fixtures, self.fixtures.is_some()),
            (Flag::Aggregate, self.aggregate.is_some()),
            (Flag::Reference, self.reference.is_some()),
            (Flag::FramesPath, self.frames_path.is_some()),
        ];
        given
            .into_iter()
            .any(|(other, present)| present && other != flag)
    }
}

/// Флаг служебного соседа, без значения.
const NEIGHBOR: &str = "--neighbor";
/// Флаг клиента доступности, значение — pid процесса пробы.
const AX_CLIENT: &str = "--ax-client";

/// Разбирает аргументы без имени программы.
pub fn parse(args: &[String]) -> Result<Command, ArgsError> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Command::Help);
    }

    if args.iter().any(|arg| arg == NEIGHBOR) {
        return if args.len() == 1 {
            Ok(Command::Neighbor)
        } else {
            Err(ArgsError::Exclusive(NEIGHBOR))
        };
    }

    if let Some(position) = args.iter().position(|arg| arg == AX_CLIENT) {
        if position != 0 || args.len() > 2 {
            return Err(ArgsError::Exclusive(AX_CLIENT));
        }
        let value = args.get(1).ok_or(ArgsError::MissingValue(AX_CLIENT))?;
        return match value.parse::<i32>() {
            Ok(pid) if pid > 0 => Ok(Command::AxClient { pid }),
            _ => Err(ArgsError::InvalidValue {
                flag: AX_CLIENT,
                value: value.clone(),
                expected: "pid процесса пробы",
            }),
        };
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
            Flag::Fixture => {
                let fixture = OpenFixture::parse(value).ok_or_else(|| {
                    invalid(
                        flag,
                        value,
                        "added, modified, sparse, wrap, real или threads",
                    )
                })?;
                set(&mut raw.fixture, flag, fixture)?;
            }
            Flag::Out => set(&mut raw.out, flag, path(flag, value)?)?,
            Flag::Fixtures => set(&mut raw.fixtures, flag, path(flag, value)?)?,
            Flag::Aggregate => set(&mut raw.aggregate, flag, path(flag, value)?)?,
            Flag::Reference => set(&mut raw.reference, flag, path(flag, value)?)?,
            Flag::FramesPath => {
                let frames = FramesPath::from_name(value)
                    .ok_or_else(|| invalid(flag, value, "none, xctrace или screencapturekit"))?;
                set(&mut raw.frames_path, flag, frames)?;
            }
        }
    }

    for (flag, given) in [
        (Flag::Aggregate, raw.aggregate.is_some()),
        (Flag::Reference, raw.reference.is_some()),
    ] {
        if given && raw.others_than(flag) {
            return Err(ArgsError::Exclusive(flag.name()));
        }
    }
    if let Some(run_dir) = raw.aggregate {
        return Ok(Command::Aggregate { run_dir });
    }
    if let Some(dir) = raw.reference {
        return Ok(Command::Reference { dir });
    }

    let view = ViewOptions {
        variant: raw.variant,
        mode: raw.mode,
        fixture: raw.fixture,
    };
    match (raw.scenario, raw.out) {
        (Some(scenario), Some(out)) => {
            check_axes(scenario.name(), scenario.axes(), view)?;
            if let (Some(fixture), Some(mode)) = (view.fixture, view.mode)
                && !scenario.accepts(fixture, mode)
            {
                return Err(ArgsError::FixtureNotAccepted {
                    scenario: scenario.name(),
                    fixture: fixture.name(),
                    mode: mode.name(),
                });
            }
            if raw.frames_path.is_some() && scenario != Scenario::Frames {
                return Err(ArgsError::FramesPathNotAccepted(scenario.name()));
            }
            match (scenario.needs_fixtures(), raw.fixtures.is_some()) {
                (true, false) => return Err(ArgsError::FixturesRequired(scenario.name())),
                (false, true) => return Err(ArgsError::FixturesNotAccepted(scenario.name())),
                (true, true) | (false, false) => {}
            }
            Ok(Command::Scenario {
                scenario,
                view,
                out,
                fixtures: raw.fixtures,
                frames_path: raw.frames_path,
            })
        }
        (Some(_), None) => Err(ArgsError::ScenarioWithoutOut),
        (None, Some(_)) => Err(ArgsError::OutWithoutScenario),
        (None, None) if raw.fixtures.is_some() => Err(ArgsError::FixturesWithoutScenario),
        (None, None) if raw.frames_path.is_some() => {
            Err(ArgsError::FramesPathNotAccepted("демонстрация"))
        }
        (None, None) if raw.fixture.is_some() => Err(ArgsError::AxisNotAccepted {
            scenario: "демонстрация",
            flag: Flag::Fixture.name(),
        }),
        (None, None) => Ok(Command::Demo(view)),
    }
}

/// Вариант, режим и фикстура указаны ровно у сценариев, которые идут по этим осям.
fn check_axes(scenario: &'static str, axes: Axes, view: ViewOptions) -> Result<(), ArgsError> {
    for (wanted, given, flag) in [
        (axes.variants, view.variant.is_some(), Flag::Variant),
        (axes.modes, view.mode.is_some(), Flag::Mode),
        (axes.fixtures, view.fixture.is_some(), Flag::Fixture),
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
                fixture: None,
            }))
        );
        assert_eq!(
            parse_str(&["--mode", "unified"]),
            Ok(Command::Demo(ViewOptions {
                variant: None,
                mode: Some(ViewMode::Unified),
                fixture: None,
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
                fixtures: None,
                frames_path: None,
            })
        );
    }

    #[test]
    fn highlight_needs_fixtures_and_smoke_rejects_them() {
        assert_eq!(
            parse_str(&["--scenario", "highlight", "--out", "o", "--fixtures", "f"]),
            Ok(Command::Scenario {
                scenario: Scenario::Highlight,
                view: ViewOptions::default(),
                out: PathBuf::from("o"),
                fixtures: Some(PathBuf::from("f")),
                frames_path: None,
            })
        );
        assert_eq!(
            parse_str(&["--scenario", "highlight", "--out", "o"]),
            Err(ArgsError::FixturesRequired("highlight"))
        );
        assert_eq!(
            parse_str(&["--scenario", "smoke", "--out", "o", "--fixtures", "f"]),
            Err(ArgsError::FixturesNotAccepted("smoke"))
        );
        assert_eq!(
            parse_str(&["--fixtures", "f"]),
            Err(ArgsError::FixturesWithoutScenario)
        );
        assert_eq!(
            parse_str(&[
                "--scenario",
                "highlight",
                "--out",
                "o",
                "--fixtures",
                "f",
                "--mode",
                "side"
            ]),
            Err(ArgsError::AxisNotAccepted {
                scenario: "highlight",
                flag: "--mode"
            })
        );
    }

    #[test]
    fn ax_client_takes_a_pid_and_nothing_else() {
        assert_eq!(
            parse_str(&["--ax-client", "4242"]),
            Ok(Command::AxClient { pid: 4242 })
        );
        assert_eq!(
            parse_str(&["--ax-client"]),
            Err(ArgsError::MissingValue("--ax-client"))
        );
        for bad in ["0", "-1", "x", ""] {
            assert!(
                matches!(
                    parse_str(&["--ax-client", bad]),
                    Err(ArgsError::InvalidValue { .. })
                ),
                "{bad}"
            );
        }
        assert_eq!(
            parse_str(&["--ax-client", "1", "--out", "o"]),
            Err(ArgsError::Exclusive("--ax-client"))
        );
        assert_eq!(
            parse_str(&["--scenario", "ax", "--ax-client", "1"]),
            Err(ArgsError::Exclusive("--ax-client"))
        );
    }

    #[test]
    fn ax_is_a_view_scenario() {
        assert_eq!(Scenario::from_name("ax"), Some(Scenario::Ax));
        assert_eq!(Scenario::Ax.axes(), Axes::VIEW);
        assert!(!Scenario::Ax.needs_fixtures());
        assert_eq!(
            parse_str(&[
                "--scenario",
                "ax",
                "--out",
                "o",
                "--variant",
                "b",
                "--mode",
                "side"
            ]),
            Ok(Command::Scenario {
                scenario: Scenario::Ax,
                view: ViewOptions {
                    variant: Some(Variant::B),
                    mode: Some(ViewMode::Side),
                    fixture: None,
                },
                out: PathBuf::from("o"),
                fixtures: None,
                frames_path: None,
            })
        );
    }

    #[test]
    fn neighbor_alone() {
        assert_eq!(parse_str(&["--neighbor"]), Ok(Command::Neighbor));
        assert_eq!(
            parse_str(&["--neighbor", "--scenario", "calibrate"]),
            Err(ArgsError::Exclusive("--neighbor"))
        );
        assert_eq!(
            parse_str(&["--out", "o", "--neighbor"]),
            Err(ArgsError::Exclusive("--neighbor"))
        );
    }

    #[test]
    fn reference_alone() {
        assert_eq!(
            parse_str(&["--reference", "target/probes/diff-view/fixtures"]),
            Ok(Command::Reference {
                dir: PathBuf::from("target/probes/diff-view/fixtures"),
            })
        );
        for extra in [
            ["--scenario", "highlight"],
            ["--fixtures", "f"],
            ["--out", "o"],
            ["--variant", "a"],
        ] {
            let mut args = vec!["--reference", "dir"];
            args.extend(extra);
            assert_eq!(
                parse_str(&args),
                Err(ArgsError::Exclusive("--reference")),
                "{extra:?}"
            );
        }
        assert!(matches!(
            parse_str(&["--reference", ""]),
            Err(ArgsError::InvalidValue {
                flag: "--reference",
                ..
            })
        ));
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
    fn open_smoke_is_a_view_scenario() {
        assert_eq!(
            parse_str(&[
                "--scenario",
                "open-smoke",
                "--out",
                "o",
                "--variant",
                "a",
                "--mode",
                "unified",
            ]),
            Ok(Command::Scenario {
                scenario: Scenario::OpenSmoke,
                view: ViewOptions {
                    variant: Some(Variant::A),
                    mode: Some(ViewMode::Unified),
                    fixture: None,
                },
                out: PathBuf::from("o"),
                fixtures: None,
                frames_path: None,
            })
        );
        assert_eq!(
            parse_str(&["--scenario", "open-smoke", "--out", "o", "--variant", "b"]),
            Err(ArgsError::AxisRequired {
                scenario: "open-smoke",
                flag: "--mode"
            })
        );
        assert_eq!(
            parse_str(&[
                "--scenario",
                "open-smoke",
                "--out",
                "o",
                "--variant",
                "a",
                "--mode",
                "side",
                "--fixtures",
                "f",
            ]),
            Err(ArgsError::FixturesNotAccepted("open-smoke"))
        );
    }

    #[test]
    fn open_needs_a_fixture_and_the_fixtures_dir() {
        let open = |scenario: &'static str| {
            parse_str(&[
                "--scenario",
                scenario,
                "--out",
                "o",
                "--variant",
                "b",
                "--mode",
                "side",
                "--fixture",
                "sparse",
                "--fixtures",
                "f",
            ])
        };
        for (name, scenario) in [("open", Scenario::Open), ("open-warm", Scenario::OpenWarm)] {
            assert_eq!(
                open(name),
                Ok(Command::Scenario {
                    scenario,
                    view: ViewOptions {
                        variant: Some(Variant::B),
                        mode: Some(ViewMode::Side),
                        fixture: Some(OpenFixture::Sparse),
                    },
                    out: PathBuf::from("o"),
                    fixtures: Some(PathBuf::from("f")),
                    frames_path: None,
                })
            );
        }
        assert_eq!(
            parse_str(&[
                "--scenario",
                "open",
                "--out",
                "o",
                "--variant",
                "a",
                "--mode",
                "unified",
                "--fixtures",
                "f",
            ]),
            Err(ArgsError::AxisRequired {
                scenario: "open",
                flag: "--fixture"
            })
        );
        assert_eq!(
            parse_str(&[
                "--scenario",
                "open",
                "--out",
                "o",
                "--variant",
                "a",
                "--mode",
                "unified",
                "--fixture",
                "real",
            ]),
            Err(ArgsError::FixturesRequired("open"))
        );
        assert_eq!(
            parse_str(&[
                "--scenario",
                "ax",
                "--out",
                "o",
                "--variant",
                "a",
                "--mode",
                "unified",
                "--fixture",
                "added",
            ]),
            Err(ArgsError::AxisNotAccepted {
                scenario: "ax",
                flag: "--fixture"
            })
        );
        assert_eq!(
            parse_str(&["--fixture", "added"]),
            Err(ArgsError::AxisNotAccepted {
                scenario: "демонстрация",
                flag: "--fixture"
            })
        );
        assert!(matches!(
            parse_str(&["--fixture", "large"]),
            Err(ArgsError::InvalidValue {
                flag: "--fixture",
                ..
            })
        ));
    }

    #[test]
    fn view_axes_are_required_and_checked_one_by_one() {
        let both = Axes::VIEW;
        let full = ViewOptions {
            variant: Some(Variant::A),
            mode: Some(ViewMode::Side),
            fixture: None,
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
            fixtures: false,
        };
        assert_eq!(
            check_axes(
                "bounded",
                variants_only,
                ViewOptions {
                    variant: Some(Variant::B),
                    mode: None,
                    fixture: None,
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
            ["--fixtures", "f"],
            ["--reference", "dir"],
        ] {
            let mut args = vec!["--aggregate", "run"];
            args.extend(extra);
            assert_eq!(
                parse_str(&args),
                Err(ArgsError::Exclusive("--aggregate")),
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
    fn frames_takes_its_own_fixtures_and_path() {
        let base = [
            "--scenario",
            "frames",
            "--out",
            "o",
            "--variant",
            "b",
            "--mode",
            "side",
        ];
        let with = |extra: &[&str]| {
            let mut args: Vec<&str> = base.to_vec();
            args.extend_from_slice(extra);
            parse_str(&args)
        };
        assert_eq!(
            with(&["--fixture", "threads", "--frames-path", "screencapturekit"]),
            Ok(Command::Scenario {
                scenario: Scenario::Frames,
                view: ViewOptions {
                    variant: Some(Variant::B),
                    mode: Some(ViewMode::Side),
                    fixture: Some(OpenFixture::Threads),
                },
                out: PathBuf::from("o"),
                fixtures: None,
                frames_path: Some(FramesPath::Screencapturekit),
            })
        );
        assert!(matches!(
            with(&["--fixture", "wrap"]),
            Ok(Command::Scenario {
                frames_path: None,
                ..
            })
        ));
        assert_eq!(
            with(&["--fixture", "sparse"]),
            Err(ArgsError::FixtureNotAccepted {
                scenario: "frames",
                fixture: "sparse",
                mode: "side",
            })
        );
        assert_eq!(
            parse_str(&[
                "--scenario",
                "frames",
                "--out",
                "o",
                "--variant",
                "a",
                "--mode",
                "unified",
                "--fixture",
                "wrap",
            ]),
            Err(ArgsError::FixtureNotAccepted {
                scenario: "frames",
                fixture: "wrap",
                mode: "unified",
            })
        );
        assert_eq!(
            with(&["--fixture", "added", "--frames-path", "trace"]),
            Err(ArgsError::InvalidValue {
                flag: "--frames-path",
                value: "trace".to_owned(),
                expected: "none, xctrace или screencapturekit",
            })
        );
        assert_eq!(
            parse_str(&[
                "--scenario",
                "open",
                "--out",
                "o",
                "--fixtures",
                "f",
                "--variant",
                "a",
                "--mode",
                "side",
                "--fixture",
                "threads",
            ]),
            Err(ArgsError::FixtureNotAccepted {
                scenario: "open",
                fixture: "threads",
                mode: "side",
            })
        );
        assert_eq!(
            parse_str(&["--scenario", "smoke", "--out", "o", "--frames-path", "none"]),
            Err(ArgsError::FramesPathNotAccepted("smoke"))
        );
        assert_eq!(
            parse_str(&["--frames-path", "none"]),
            Err(ArgsError::FramesPathNotAccepted("демонстрация"))
        );
        for path in FramesPath::ALL {
            assert_eq!(FramesPath::from_name(path.name()), Some(path));
        }
        assert_eq!(FramesPath::default(), FramesPath::None);
    }

    #[test]
    fn scenario_fixtures_match_their_axes() {
        let cells = |scenario: Scenario| -> Vec<String> {
            OpenFixture::ALL
                .into_iter()
                .flat_map(|fixture| ViewMode::ALL.map(|mode| (fixture, mode)))
                .filter(|&(fixture, mode)| scenario.accepts(fixture, mode))
                .map(|(fixture, mode)| format!("{}:{}", fixture.name(), mode.name()))
                .collect()
        };
        for scenario in Scenario::ALL {
            assert_eq!(scenario.axes().fixtures, !scenario.fixtures().is_empty());
        }
        assert_eq!(
            cells(Scenario::Frames),
            [
                "added:unified",
                "added:side",
                "modified:unified",
                "modified:side",
                "wrap:side",
                "threads:unified",
                "threads:side",
            ]
        );
        assert_eq!(cells(Scenario::Open).len(), 10);
        assert!(!cells(Scenario::Open).contains(&"threads:side".to_owned()));
        assert!(cells(Scenario::Ax).is_empty());
        assert!(!Scenario::Frames.needs_fixtures());
        assert_eq!(Scenario::from_name("frames"), Some(Scenario::Frames));
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
