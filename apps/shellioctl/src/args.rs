//! Разбор аргументов `shellioctl` вручную по таблице ADR-0007.
//!
//! Флаги и операнды идут в любом порядке, `--` кончает флаги. Область задаётся явно: внешний
//! вызов без контекста сессии её не наследует.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde_json::Value;
use shellio_control::{
    CLI_TIMEOUT, Contrast, GalleryCommand, GalleryFocus, GalleryStrip, Lightness, Motion,
    TreeMutation, UiRequest,
};
use shellio_core::{
    AppearanceChoice, Command, PanelId, ProjectId, Revision, TreeScope, UiCommand, WorkbenchId,
    WorkspaceId,
};
use shellio_layout::{Direction, Edge, Hideable, Zone};

/// Справка.
pub const USAGE: &str = "\
Использование: shellioctl [--json] [--timeout <мс>] [--socket <путь>] <вызов>

Вызовы:
  version
  status
  tree (--workspace <id> | --all)
  workspace new <имя> --all
  workspace rename [--target <id>] <имя> (--workspace <id> | --all)
  workspace focus [--target <id>] (--workspace <id> | --all)
  project add <путь> [--title <имя>] [--target <id>] (--workspace <id> | --all)
  workbench new --target <проект> <имя> (--workspace <id> | --all)
  workbench rename --target <id> <имя> (--workspace <id> | --all)
  workbench open --target <id> (--workspace <id> | --all)
  layout get [--workbench <id>] (--workspace <id> | --all)
  layout split <панель> [--open <плагин>] (--workspace <id> | --all)
  layout resize --zone <зона> [--part-of <панель>] [--workbench <id>] <размер> (--workspace <id> | --all)
  layout equal --zone <зона> [--workbench <id>] (--workspace <id> | --all)
  layout zoom <панель> (--workspace <id> | --all)
  layout zoom --off [--workbench <id>] (--workspace <id> | --all)
  layout only [--workbench <id>] (--workspace <id> | --all)
  layout <show|hide> <navigator|plate|right|bottom> [--workbench <id>] (--workspace <id> | --all)
  panel focus <панель> (--workspace <id> | --all)
  panel focus --dir <h|j|k|l> [--workbench <id>] (--workspace <id> | --all)
  panel select <панель> (--workspace <id> | --all)
  panel move <панель> --zone <зона> (--workspace <id> | --all)
  panel move-part <панель> --to <панель> [--edge <before|after>] (--workspace <id> | --all)
  panel close <панель> (--workspace <id> | --all)
  panel open <плагин> [--workbench <id>] [--zone <зона>] [--args <json>] (--workspace <id> | --all)
  panel command <панель> <действие> [--args <json>] (--workspace <id> | --all)
  panel get <панель> (--workspace <id> | --all)
  plugin list
  events --since <курсор> --all
  events --follow --all
  ui window
  ui inspect
  ui stats
  ui appearance <light|dark|system>
  window close
  app quit
  gallery cell <light|dark|auto> <normal|increased|auto>
  gallery motion <normal|reduced>
  gallery focus <field|window|tabs|list|tree>
  gallery menu open
  gallery rebuild
  gallery window recreate
  gallery place <экран>
  gallery reset
  gallery tabs select <side|center|half|wide> <номер вкладки>
  gallery list replace <зерно>
  gallery list scroll <номер строки>
  gallery list ticks <on|off>
  gallery tree mutate <insert|reorder|collapse_parent_of_selected>

У команд с целью-воркспейсом --target по умолчанию равен --workspace, с --all он обязателен.
Зона — left, right, center или bottom. Размер у layout resize — точки зоны, с --part-of — доля
части в тысячных. Команда с панелью берёт её рабочую область, без панели — --workbench, иначе
открытую в окне.
panel open открывает панель плагина по правилу его кардинальности: новую или уже существующую.
Действие у panel command — полное имя из манифеста плагина, <плагин>.<имя>. --args — объект JSON.
--json печатает кадры ответа построчно, --timeout задаёт срок вызова (по умолчанию 5000 мс).
Вызовы gallery — служебные входы тестов, только у владельца в режиме --gallery; auto у gallery cell
оставляет ось без переопределения галереи.
Адрес: --socket, иначе SHELLIO_SOCKET, иначе сокет профиля (SHELLIO_STATE_DIR или HOME).
Коды выхода: 0 — успех, 1 — отказ, 2 — ошибка использования, 69 — shellio не запущен,
74 — исход неизвестен, 75 — повторить позже.
";

/// Разобранный вызов.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// `--help`.
    Help,
    /// Вызов владельца.
    Run(Invocation),
}

/// Вызов владельца с общими флагами.
#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    pub json: bool,
    pub timeout: Duration,
    pub socket: Option<PathBuf>,
    pub action: Action,
}

/// Запрос вызова.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Version,
    Status,
    Tree(TreeScope),
    Command {
        command: Command,
        scope: TreeScope,
    },
    /// Чтение `query.layout`.
    Layout {
        scope: TreeScope,
        workbench: Option<WorkbenchId>,
    },
    /// Чтение `query.panel`.
    Panel {
        scope: TreeScope,
        panel: PanelId,
    },
    /// Чтение `query.plugins`.
    Plugins,
    EventsSince(Revision),
    Follow,
    /// Запрос к UI владельца.
    Ui(UiRequest),
}

/// Ошибка использования.
#[derive(Debug, PartialEq, Eq)]
pub struct Usage(pub String);

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn usage(message: impl Into<String>) -> Usage {
    Usage(message.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flag {
    Json,
    Timeout,
    Socket,
    Workspace,
    All,
    Target,
    Title,
    Since,
    Follow,
    Workbench,
    Open,
    Zone,
    PartOf,
    Off,
    Dir,
    To,
    Edge,
    Args,
    Help,
}

impl Flag {
    const ALL: [Self; 19] = [
        Self::Json,
        Self::Timeout,
        Self::Socket,
        Self::Workspace,
        Self::All,
        Self::Target,
        Self::Title,
        Self::Since,
        Self::Follow,
        Self::Workbench,
        Self::Open,
        Self::Zone,
        Self::PartOf,
        Self::Off,
        Self::Dir,
        Self::To,
        Self::Edge,
        Self::Args,
        Self::Help,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Json => "--json",
            Self::Timeout => "--timeout",
            Self::Socket => "--socket",
            Self::Workspace => "--workspace",
            Self::All => "--all",
            Self::Target => "--target",
            Self::Title => "--title",
            Self::Since => "--since",
            Self::Follow => "--follow",
            Self::Workbench => "--workbench",
            Self::Open => "--open",
            Self::Zone => "--zone",
            Self::PartOf => "--part-of",
            Self::Off => "--off",
            Self::Dir => "--dir",
            Self::To => "--to",
            Self::Edge => "--edge",
            Self::Args => "--args",
            Self::Help => "--help",
        }
    }

    const fn takes_value(self) -> bool {
        match self {
            Self::Timeout
            | Self::Socket
            | Self::Workspace
            | Self::Target
            | Self::Title
            | Self::Since
            | Self::Workbench
            | Self::Open
            | Self::Zone
            | Self::PartOf
            | Self::Dir
            | Self::To
            | Self::Edge
            | Self::Args => true,
            Self::Json | Self::All | Self::Follow | Self::Off | Self::Help => false,
        }
    }

    /// Флаг допустим у любого вызова.
    const fn is_common(self) -> bool {
        match self {
            Self::Json | Self::Timeout | Self::Socket | Self::Help => true,
            Self::Workspace
            | Self::All
            | Self::Target
            | Self::Title
            | Self::Since
            | Self::Follow
            | Self::Workbench
            | Self::Open
            | Self::Zone
            | Self::PartOf
            | Self::Off
            | Self::Dir
            | Self::To
            | Self::Edge
            | Self::Args => false,
        }
    }

    fn by_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|flag| &flag.name()[2..] == name)
    }
}

/// Флаги и операнды командной строки.
#[derive(Default)]
struct Given {
    flags: Vec<(Flag, Option<String>)>,
    operands: Vec<String>,
}

impl Given {
    fn scan(args: Vec<String>) -> Result<Self, Usage> {
        let mut given = Self::default();
        let mut args = args.into_iter();
        let mut flags_done = false;
        while let Some(arg) = args.next() {
            if flags_done || arg == "-" || !arg.starts_with('-') {
                given.operands.push(arg);
                continue;
            }
            if arg == "--" {
                flags_done = true;
                continue;
            }
            if arg == "-h" {
                given.add(Flag::Help, None)?;
                continue;
            }
            let Some(body) = arg.strip_prefix("--") else {
                return Err(usage(format!("неизвестный флаг {arg}")));
            };
            let (name, inline) = match body.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (body, None),
            };
            let flag =
                Flag::by_name(name).ok_or_else(|| usage(format!("неизвестный флаг --{name}")))?;
            let value = match (flag.takes_value(), inline) {
                (true, Some(value)) => Some(value),
                (true, None) => Some(
                    args.next()
                        .ok_or_else(|| usage(format!("флагу {} нужно значение", flag.name())))?,
                ),
                (false, Some(_)) => {
                    return Err(usage(format!("флаг {} не принимает значения", flag.name())));
                }
                (false, None) => None,
            };
            given.add(flag, value)?;
        }
        Ok(given)
    }

    fn add(&mut self, flag: Flag, value: Option<String>) -> Result<(), Usage> {
        if self.has(flag) {
            return Err(usage(format!("флаг {} указан дважды", flag.name())));
        }
        self.flags.push((flag, value));
        Ok(())
    }

    fn has(&self, flag: Flag) -> bool {
        self.flags.iter().any(|(given, _)| *given == flag)
    }

    fn value(&self, flag: Flag) -> Option<&str> {
        self.flags
            .iter()
            .find(|(given, _)| *given == flag)
            .and_then(|(_, value)| value.as_deref())
    }

    /// Флаги вызова `call` — общие и `allowed`.
    fn only(&self, call: &str, allowed: &[Flag]) -> Result<(), Usage> {
        match self
            .flags
            .iter()
            .find(|(flag, _)| !flag.is_common() && !allowed.contains(flag))
        {
            Some((flag, _)) => Err(usage(format!("флаг {} не применим к {call}", flag.name()))),
            None => Ok(()),
        }
    }

    /// Область `--workspace <id>` или `--all`.
    fn scope(&self, call: &str) -> Result<TreeScope, Usage> {
        match (self.value(Flag::Workspace), self.has(Flag::All)) {
            (Some(_), true) => Err(usage(format!(
                "{call}: --workspace и --all взаимоисключающие"
            ))),
            (Some(id), false) => Ok(TreeScope::Workspace(parse_id(Flag::Workspace, id)?)),
            (None, true) => Ok(TreeScope::All),
            (None, false) => Err(usage(format!(
                "{call}: нужна область --workspace <id> или --all"
            ))),
        }
    }

    /// Только область всех воркспейсов.
    fn all_only(&self, call: &str) -> Result<TreeScope, Usage> {
        if self.has(Flag::All) {
            Ok(TreeScope::All)
        } else {
            Err(usage(format!("{call}: нужна область --all")))
        }
    }

    /// Цель-воркспейс: `--target`, иначе воркспейс области.
    fn workspace_target(&self, call: &str, scope: TreeScope) -> Result<WorkspaceId, Usage> {
        match (self.value(Flag::Target), scope) {
            (Some(id), _) => parse_id(Flag::Target, id),
            (None, TreeScope::Workspace(id)) => Ok(id),
            (None, TreeScope::All) => Err(usage(format!("{call}: с --all нужен --target <id>"))),
        }
    }

    /// Обязательная цель `--target`.
    fn target<T: FromStr>(&self, call: &str) -> Result<T, Usage> {
        let id = self
            .value(Flag::Target)
            .ok_or_else(|| usage(format!("{call}: нужен --target <id>")))?;
        parse_id(Flag::Target, id)
    }

    /// Необязательная рабочая область `--workbench`.
    fn workbench(&self) -> Result<Option<WorkbenchId>, Usage> {
        self.value(Flag::Workbench)
            .map(|id| parse_id(Flag::Workbench, id))
            .transpose()
    }

    /// Обязательная зона `--zone`.
    fn zone(&self, call: &str) -> Result<Zone, Usage> {
        self.optional_zone()?
            .ok_or_else(|| usage(format!("{call}: нужна --zone <зона>")))
    }

    /// Необязательная зона `--zone`.
    fn optional_zone(&self) -> Result<Option<Zone>, Usage> {
        self.value(Flag::Zone)
            .map(|value| {
                name_of(
                    Flag::Zone.name(),
                    value,
                    &[
                        ("left", Zone::Left),
                        ("right", Zone::Right),
                        ("center", Zone::Center),
                        ("bottom", Zone::Bottom),
                    ],
                )
            })
            .transpose()
    }

    /// Необязательные `--args`: объект JSON. Содержимое объекта судит модель плагина.
    fn args(&self) -> Result<Option<Value>, Usage> {
        let Some(text) = self.value(Flag::Args) else {
            return Ok(None);
        };
        match serde_json::from_str::<Value>(text) {
            Ok(args @ Value::Object(_)) => Ok(Some(args)),
            Ok(_) => Err(usage(format!(
                "--args: ожидается объект JSON, получено «{text}»"
            ))),
            Err(error) => Err(usage(format!(
                "--args: ожидается объект JSON, «{text}» не разобран: {error}"
            ))),
        }
    }
}

/// Значение из таблицы имён.
fn name_of<T: Copy>(what: &str, value: &str, names: &[(&str, T)]) -> Result<T, Usage> {
    names
        .iter()
        .find(|(name, _)| *name == value)
        .map(|(_, item)| *item)
        .ok_or_else(|| {
            let expected: Vec<&str> = names.iter().map(|(name, _)| *name).collect();
            usage(format!(
                "{what}: ожидается {}, получено «{value}»",
                expected.join(", ")
            ))
        })
}

/// Панель-операнд вызова `call`.
fn panel_id(call: &str, value: &str) -> Result<PanelId, Usage> {
    value
        .parse()
        .map_err(|_| usage(format!("{call}: неверный идентификатор панели «{value}»")))
}

/// Не больше одного операнда вызова `call`.
fn optional_operand<'a>(call: &str, operands: &'a [String]) -> Result<Option<&'a str>, Usage> {
    match operands {
        [] => Ok(None),
        [operand] => Ok(Some(operand)),
        _ => Err(usage(format!(
            "{call}: ожидается не больше одного операнда, получено: {}",
            operands.len()
        ))),
    }
}

fn parse_id<T: FromStr>(flag: Flag, value: &str) -> Result<T, Usage> {
    value
        .parse()
        .map_err(|_| usage(format!("{}: неверный идентификатор «{value}»", flag.name())))
}

/// Ровно `N` операндов вызова `call`.
fn operands<const N: usize>(call: &str, operands: &[String]) -> Result<[String; N], Usage> {
    <[String; N]>::try_from(operands.to_vec()).map_err(|_| {
        usage(format!(
            "{call}: ожидается операндов: {N}, получено: {}",
            operands.len()
        ))
    })
}

/// Разобрать аргументы без имени программы. Относительный путь `project add` делается
/// абсолютным через `absolute` без канонизации: канонизирует ядро.
pub fn parse(
    args: Vec<OsString>,
    absolute: &dyn Fn(&Path) -> io::Result<PathBuf>,
) -> Result<Parsed, Usage> {
    let args = args
        .into_iter()
        .enumerate()
        .map(|(index, arg)| {
            arg.into_string()
                .map_err(|_| usage(format!("аргумент {} не в UTF-8", index + 1)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let given = Given::scan(args)?;
    if given.has(Flag::Help) {
        return Ok(Parsed::Help);
    }
    let timeout = match given.value(Flag::Timeout) {
        None => CLI_TIMEOUT,
        Some(value) => match value.parse::<u64>() {
            Ok(millis) if millis > 0 => Duration::from_millis(millis),
            Ok(_) | Err(_) => {
                return Err(usage(format!(
                    "--timeout: ожидается целое число миллисекунд больше нуля, получено «{value}»"
                )));
            }
        },
    };
    let socket = match given.value(Flag::Socket) {
        None => None,
        Some("") => return Err(usage("--socket: пустой путь")),
        Some(path) => Some(PathBuf::from(path)),
    };
    Ok(Parsed::Run(Invocation {
        json: given.has(Flag::Json),
        timeout,
        socket,
        action: action(&given, absolute)?,
    }))
}

fn action(given: &Given, absolute: &dyn Fn(&Path) -> io::Result<PathBuf>) -> Result<Action, Usage> {
    let (call, rest) = match given.operands.split_first() {
        None => return Err(usage("не указан вызов")),
        Some((group, rest))
            if matches!(
                group.as_str(),
                "workspace"
                    | "project"
                    | "workbench"
                    | "layout"
                    | "panel"
                    | "plugin"
                    | "ui"
                    | "window"
                    | "app"
                    | "gallery"
            ) =>
        {
            match rest.split_first() {
                Some((verb, rest)) => (format!("{group} {verb}"), rest),
                None => return Err(usage(format!("{group}: не указано действие"))),
            }
        }
        Some((call, rest)) => (call.clone(), rest),
    };
    let call = call.as_str();
    let command = |command: Command, scope: TreeScope| Ok(Action::Command { command, scope });
    match call {
        "version" => {
            given.only(call, &[])?;
            operands::<0>(call, rest)?;
            Ok(Action::Version)
        }
        "status" => {
            given.only(call, &[])?;
            operands::<0>(call, rest)?;
            Ok(Action::Status)
        }
        "tree" => {
            given.only(call, &[Flag::Workspace, Flag::All])?;
            operands::<0>(call, rest)?;
            Ok(Action::Tree(given.scope(call)?))
        }
        "workspace new" => {
            given.only(call, &[Flag::All])?;
            let [title] = operands(call, rest)?;
            let scope = given.all_only(call)?;
            command(Command::WorkspaceCreate { title }, scope)
        }
        "workspace rename" => {
            given.only(call, &[Flag::Target, Flag::Workspace, Flag::All])?;
            let [title] = operands(call, rest)?;
            let scope = given.scope(call)?;
            let workspace = given.workspace_target(call, scope)?;
            command(Command::WorkspaceRename { workspace, title }, scope)
        }
        "workspace focus" => {
            given.only(call, &[Flag::Target, Flag::Workspace, Flag::All])?;
            operands::<0>(call, rest)?;
            let scope = given.scope(call)?;
            let workspace = given.workspace_target(call, scope)?;
            command(Command::WindowFocusWorkspace { workspace }, scope)
        }
        "project add" => {
            given.only(
                call,
                &[Flag::Title, Flag::Target, Flag::Workspace, Flag::All],
            )?;
            let [path] = operands(call, rest)?;
            let scope = given.scope(call)?;
            let workspace = given.workspace_target(call, scope)?;
            let path = absolute(Path::new(&path)).map_err(|error| {
                usage(format!(
                    "{call}: путь «{path}» не сделан абсолютным: {error}"
                ))
            })?;
            let title = given.value(Flag::Title).map(str::to_owned);
            command(
                Command::ProjectAdd {
                    workspace,
                    path,
                    title,
                },
                scope,
            )
        }
        "workbench new" => {
            given.only(call, &[Flag::Target, Flag::Workspace, Flag::All])?;
            let [title] = operands(call, rest)?;
            let scope = given.scope(call)?;
            let project: ProjectId = given.target(call)?;
            command(Command::WorkbenchCreate { project, title }, scope)
        }
        "workbench rename" => {
            given.only(call, &[Flag::Target, Flag::Workspace, Flag::All])?;
            let [title] = operands(call, rest)?;
            let scope = given.scope(call)?;
            let workbench: WorkbenchId = given.target(call)?;
            command(Command::WorkbenchRename { workbench, title }, scope)
        }
        "workbench open" => {
            given.only(call, &[Flag::Target, Flag::Workspace, Flag::All])?;
            operands::<0>(call, rest)?;
            let scope = given.scope(call)?;
            let workbench: WorkbenchId = given.target(call)?;
            command(Command::WorkbenchOpen { workbench }, scope)
        }
        "layout get" => {
            given.only(call, &[Flag::Workbench, Flag::Workspace, Flag::All])?;
            operands::<0>(call, rest)?;
            Ok(Action::Layout {
                scope: given.scope(call)?,
                workbench: given.workbench()?,
            })
        }
        "layout split" => {
            given.only(call, &[Flag::Open, Flag::Workspace, Flag::All])?;
            let [panel] = operands(call, rest)?;
            let scope = given.scope(call)?;
            let panel = panel_id(call, &panel)?;
            let open = given.value(Flag::Open).map(str::to_owned);
            command(Command::LayoutSplit { panel, open }, scope)
        }
        "layout resize" => {
            given.only(
                call,
                &[
                    Flag::Zone,
                    Flag::PartOf,
                    Flag::Workbench,
                    Flag::Workspace,
                    Flag::All,
                ],
            )?;
            let [size] = operands(call, rest)?;
            let scope = given.scope(call)?;
            let zone = given.zone(call)?;
            let part_of = given
                .value(Flag::PartOf)
                .map(|panel| panel_id(call, panel))
                .transpose()?;
            let size = size.parse().map_err(|_| {
                usage(format!(
                    "{call}: ожидается размер — целое без знака, получено «{size}»"
                ))
            })?;
            command(
                Command::LayoutResize {
                    workbench: given.workbench()?,
                    zone,
                    part_of,
                    size,
                },
                scope,
            )
        }
        "layout equal" => {
            given.only(
                call,
                &[Flag::Zone, Flag::Workbench, Flag::Workspace, Flag::All],
            )?;
            operands::<0>(call, rest)?;
            let scope = given.scope(call)?;
            let zone = given.zone(call)?;
            command(
                Command::LayoutEqual {
                    workbench: given.workbench()?,
                    zone,
                },
                scope,
            )
        }
        "layout zoom" => {
            given.only(
                call,
                &[Flag::Off, Flag::Workbench, Flag::Workspace, Flag::All],
            )?;
            let panel = optional_operand(call, rest)?;
            let scope = given.scope(call)?;
            let workbench = given.workbench()?;
            let zoom = match (panel, given.has(Flag::Off)) {
                (Some(_), true) => {
                    return Err(usage(format!("{call}: панель и --off взаимоисключающие")));
                }
                (Some(_), false) if workbench.is_some() => {
                    return Err(usage(format!(
                        "{call}: --workbench не применим к развороту панели"
                    )));
                }
                (Some(panel), false) => Command::LayoutZoom {
                    panel: Some(panel_id(call, panel)?),
                    workbench: None,
                    off: None,
                },
                (None, true) => Command::LayoutZoom {
                    panel: None,
                    workbench,
                    off: Some(true),
                },
                (None, false) => return Err(usage(format!("{call}: нужна панель или --off"))),
            };
            command(zoom, scope)
        }
        "layout only" => {
            given.only(call, &[Flag::Workbench, Flag::Workspace, Flag::All])?;
            operands::<0>(call, rest)?;
            let scope = given.scope(call)?;
            command(
                Command::LayoutOnly {
                    workbench: given.workbench()?,
                },
                scope,
            )
        }
        "layout show" | "layout hide" => {
            given.only(call, &[Flag::Workbench, Flag::Workspace, Flag::All])?;
            let [target] = operands(call, rest)?;
            let scope = given.scope(call)?;
            let target = name_of(
                call,
                &target,
                &[
                    ("navigator", Hideable::Navigator),
                    ("plate", Hideable::Plate),
                    ("right", Hideable::Right),
                    ("bottom", Hideable::Bottom),
                ],
            )?;
            command(
                Command::LayoutVisibility {
                    workbench: given.workbench()?,
                    target,
                    visible: call == "layout show",
                },
                scope,
            )
        }
        "panel focus" => {
            given.only(
                call,
                &[Flag::Dir, Flag::Workbench, Flag::Workspace, Flag::All],
            )?;
            let panel = optional_operand(call, rest)?;
            let scope = given.scope(call)?;
            let workbench = given.workbench()?;
            let focus = match (panel, given.value(Flag::Dir)) {
                (Some(_), Some(_)) => {
                    return Err(usage(format!("{call}: панель и --dir взаимоисключающие")));
                }
                (Some(_), None) if workbench.is_some() => {
                    return Err(usage(format!(
                        "{call}: --workbench не применим к фокусу панели"
                    )));
                }
                (Some(panel), None) => Command::PanelFocus {
                    panel: Some(panel_id(call, panel)?),
                    workbench: None,
                    direction: None,
                },
                (None, Some(direction)) => Command::PanelFocus {
                    panel: None,
                    workbench,
                    direction: Some(name_of(
                        Flag::Dir.name(),
                        direction,
                        &[
                            ("h", Direction::Left),
                            ("j", Direction::Down),
                            ("k", Direction::Up),
                            ("l", Direction::Right),
                        ],
                    )?),
                },
                (None, None) => return Err(usage(format!("{call}: нужна панель или --dir"))),
            };
            command(focus, scope)
        }
        "panel select" | "panel close" => {
            given.only(call, &[Flag::Workspace, Flag::All])?;
            let [panel] = operands(call, rest)?;
            let scope = given.scope(call)?;
            let panel = panel_id(call, &panel)?;
            command(
                if call == "panel select" {
                    Command::PanelSelect { panel }
                } else {
                    Command::PanelClose { panel }
                },
                scope,
            )
        }
        "panel move" => {
            given.only(call, &[Flag::Zone, Flag::Workspace, Flag::All])?;
            let [panel] = operands(call, rest)?;
            let scope = given.scope(call)?;
            let panel = panel_id(call, &panel)?;
            let zone = given.zone(call)?;
            command(Command::PanelMove { panel, zone }, scope)
        }
        "panel move-part" => {
            given.only(call, &[Flag::To, Flag::Edge, Flag::Workspace, Flag::All])?;
            let [panel] = operands(call, rest)?;
            let scope = given.scope(call)?;
            let panel = panel_id(call, &panel)?;
            let anchor = given
                .value(Flag::To)
                .ok_or_else(|| usage(format!("{call}: нужен --to <панель>")))?;
            let anchor = panel_id(call, anchor)?;
            let edge = given
                .value(Flag::Edge)
                .map(|edge| {
                    name_of(
                        Flag::Edge.name(),
                        edge,
                        &[("before", Edge::Before), ("after", Edge::After)],
                    )
                })
                .transpose()?;
            command(
                Command::PanelMovePart {
                    panel,
                    anchor,
                    edge,
                },
                scope,
            )
        }
        "panel open" => {
            given.only(
                call,
                &[
                    Flag::Workbench,
                    Flag::Zone,
                    Flag::Args,
                    Flag::Workspace,
                    Flag::All,
                ],
            )?;
            let [plugin] = operands(call, rest)?;
            let scope = given.scope(call)?;
            command(
                Command::PanelOpen {
                    plugin,
                    workbench: given.workbench()?,
                    zone: given.optional_zone()?,
                    args: given.args()?,
                },
                scope,
            )
        }
        "panel command" => {
            given.only(call, &[Flag::Args, Flag::Workspace, Flag::All])?;
            let [panel, name] = operands(call, rest)?;
            let scope = given.scope(call)?;
            let panel = panel_id(call, &panel)?;
            command(
                Command::PanelCommand {
                    panel,
                    name,
                    args: given.args()?,
                },
                scope,
            )
        }
        "panel get" => {
            given.only(call, &[Flag::Workspace, Flag::All])?;
            let [panel] = operands(call, rest)?;
            let scope = given.scope(call)?;
            Ok(Action::Panel {
                scope,
                panel: panel_id(call, &panel)?,
            })
        }
        "plugin list" => {
            given.only(call, &[])?;
            operands::<0>(call, rest)?;
            Ok(Action::Plugins)
        }
        "events" => {
            given.only(call, &[Flag::Since, Flag::Follow, Flag::All])?;
            operands::<0>(call, rest)?;
            given.all_only(call)?;
            match (given.value(Flag::Since), given.has(Flag::Follow)) {
                (Some(_), true) => Err(usage("events: --since и --follow взаимоисключающие")),
                (Some(cursor), false) => cursor
                    .parse()
                    .map(|cursor| Action::EventsSince(Revision(cursor)))
                    .map_err(|_| usage(format!("--since: ожидается ревизия, получено «{cursor}»"))),
                (None, true) => Ok(Action::Follow),
                (None, false) => Err(usage("events: нужен --since <курсор> или --follow")),
            }
        }
        "ui window" | "ui inspect" | "ui stats" | "window close" | "app quit" => {
            given.only(call, &[])?;
            operands::<0>(call, rest)?;
            let request = match call {
                "ui window" => UiRequest::Window,
                "ui inspect" => UiRequest::Inspect,
                "ui stats" => UiRequest::Stats,
                "window close" => UiRequest::Command(UiCommand::WindowClose),
                _ => UiRequest::Command(UiCommand::AppQuit),
            };
            Ok(Action::Ui(request))
        }
        "ui appearance" => {
            given.only(call, &[])?;
            let [value] = operands(call, rest)?;
            let choice = AppearanceChoice::parse(&value).ok_or_else(|| {
                usage(format!(
                    "{call}: ожидается light, dark или system, получено «{value}»"
                ))
            })?;
            Ok(Action::Ui(UiRequest::Command(UiCommand::AppearanceSet(
                choice,
            ))))
        }
        "gallery cell" => {
            given.only(call, &[])?;
            let [appearance, contrast] = operands(call, rest)?;
            let appearance = axis(
                call,
                &appearance,
                &[("light", Lightness::Light), ("dark", Lightness::Dark)],
            )?;
            let contrast = axis(
                call,
                &contrast,
                &[
                    ("normal", Contrast::Normal),
                    ("increased", Contrast::Increased),
                ],
            )?;
            Ok(gallery(GalleryCommand::Cell {
                appearance,
                contrast,
            }))
        }
        "gallery motion" => {
            given.only(call, &[])?;
            let [value] = operands(call, rest)?;
            let motion = match value.as_str() {
                "normal" => Motion::Normal,
                "reduced" => Motion::Reduced,
                _ => {
                    return Err(usage(format!(
                        "{call}: ожидается normal или reduced, получено «{value}»"
                    )));
                }
            };
            Ok(gallery(GalleryCommand::Motion(motion)))
        }
        "gallery focus" => {
            given.only(call, &[])?;
            let [value] = operands(call, rest)?;
            let focus = GalleryFocus::ALL
                .into_iter()
                .find(|focus| focus.as_str() == value)
                .ok_or_else(|| {
                    usage(format!(
                        "{call}: ожидается field, window, tabs, list или tree, получено «{value}»"
                    ))
                })?;
            Ok(gallery(GalleryCommand::Focus(focus)))
        }
        "gallery menu" | "gallery window" => {
            given.only(call, &[])?;
            let [verb] = operands(call, rest)?;
            match (call, verb.as_str()) {
                ("gallery menu", "open") => Ok(gallery(GalleryCommand::MenuOpen)),
                ("gallery window", "recreate") => Ok(gallery(GalleryCommand::WindowRecreate)),
                _ => Err(usage(format!("неизвестный вызов «{call} {verb}»"))),
            }
        }
        "gallery rebuild" | "gallery reset" => {
            given.only(call, &[])?;
            operands::<0>(call, rest)?;
            Ok(gallery(if call == "gallery rebuild" {
                GalleryCommand::Rebuild
            } else {
                GalleryCommand::Reset
            }))
        }
        "gallery tabs" => {
            given.only(call, &[])?;
            let [verb, strip, tab] = operands(call, rest)?;
            if verb != "select" {
                return Err(usage(format!("неизвестный вызов «{call} {verb}»")));
            }
            let strip = GalleryStrip::ALL
                .into_iter()
                .find(|candidate| candidate.as_str() == strip)
                .ok_or_else(|| {
                    usage(format!(
                        "{call} select: ожидается side, center, half или wide, получено «{strip}»"
                    ))
                })?;
            let tab = tab.parse().map_err(|_| {
                usage(format!(
                    "{call} select: ожидается номер вкладки с нуля, получено «{tab}»"
                ))
            })?;
            Ok(gallery(GalleryCommand::TabsSelect { strip, tab }))
        }
        "gallery list" => {
            given.only(call, &[])?;
            let [verb, value] = operands(call, rest)?;
            match verb.as_str() {
                "replace" => {
                    let seed = value.parse().map_err(|_| {
                        usage(format!(
                            "{call} replace: ожидается зерно — целое без знака, получено «{value}»"
                        ))
                    })?;
                    Ok(gallery(GalleryCommand::ListReplace { seed }))
                }
                "scroll" => {
                    let row = value.parse().map_err(|_| {
                        usage(format!(
                            "{call} scroll: ожидается номер строки с нуля, получено «{value}»"
                        ))
                    })?;
                    Ok(gallery(GalleryCommand::ListScroll { row }))
                }
                "ticks" => {
                    let enabled = match value.as_str() {
                        "on" => true,
                        "off" => false,
                        _ => {
                            return Err(usage(format!(
                                "{call} ticks: ожидается on или off, получено «{value}»"
                            )));
                        }
                    };
                    Ok(gallery(GalleryCommand::ListTicks { enabled }))
                }
                _ => Err(usage(format!("неизвестный вызов «{call} {verb}»"))),
            }
        }
        "gallery tree" => {
            given.only(call, &[])?;
            let [verb, value] = operands(call, rest)?;
            if verb != "mutate" {
                return Err(usage(format!("неизвестный вызов «{call} {verb}»")));
            }
            let op = TreeMutation::ALL
                .into_iter()
                .find(|op| op.as_str() == value)
                .ok_or_else(|| {
                    usage(format!(
                        "{call} mutate: ожидается insert, reorder или collapse_parent_of_selected, получено «{value}»"
                    ))
                })?;
            Ok(gallery(GalleryCommand::TreeMutate { op }))
        }
        "gallery place" => {
            given.only(call, &[])?;
            let [value] = operands(call, rest)?;
            let screen = value.parse().map_err(|_| {
                usage(format!(
                    "{call}: ожидается номер экрана из ui window, получено «{value}»"
                ))
            })?;
            Ok(gallery(GalleryCommand::Place { screen }))
        }
        other => Err(usage(format!("неизвестный вызов «{other}»"))),
    }
}

fn gallery(command: GalleryCommand) -> Action {
    Action::Ui(UiRequest::Gallery(command))
}

/// Значение оси `gallery cell`: `auto` — без переопределения галереи.
fn axis<T: Copy>(call: &str, value: &str, names: &[(&str, T)]) -> Result<Option<T>, Usage> {
    if value == "auto" {
        return Ok(None);
    }
    names
        .iter()
        .find(|(name, _)| *name == value)
        .map(|(_, axis)| Some(*axis))
        .ok_or_else(|| {
            let expected: Vec<&str> = names.iter().map(|(name, _)| *name).collect();
            usage(format!(
                "{call}: ожидается {} или auto, получено «{value}»",
                expected.join(", ")
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: &str = "00000000-0000-0000-0000-000000000001";
    const HOME: &str = "00000000-0000-0000-0000-000000000002";
    const PROJECT: &str = "00000000-0000-0000-0000-000000000003";
    const BENCH: &str = "00000000-0000-0000-0000-000000000004";

    fn fake_absolute(path: &Path) -> io::Result<PathBuf> {
        if path.as_os_str().is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "пустой путь"));
        }
        Ok(Path::new("/cwd").join(path))
    }

    fn parse_args(args: &[&str]) -> Result<Parsed, Usage> {
        parse(args.iter().map(OsString::from).collect(), &fake_absolute)
    }

    fn run(args: &[&str]) -> Invocation {
        match parse_args(args) {
            Ok(Parsed::Run(invocation)) => invocation,
            other => panic!("{args:?}: ожидался вызов, получено {other:?}"),
        }
    }

    fn action_of(args: &[&str]) -> Action {
        run(args).action
    }

    fn refused(args: &[&str]) -> String {
        match parse_args(args) {
            Err(Usage(message)) => message,
            other => panic!("{args:?}: ожидалась ошибка использования, получено {other:?}"),
        }
    }

    fn id<T: FromStr>(value: &str) -> T
    where
        T::Err: fmt::Debug,
    {
        value.parse().unwrap()
    }

    fn workspace(value: &str) -> TreeScope {
        TreeScope::Workspace(id(value))
    }

    fn command(command: Command, scope: TreeScope) -> Action {
        Action::Command { command, scope }
    }

    #[test]
    fn every_call_form() {
        assert_eq!(action_of(&["version"]), Action::Version);
        assert_eq!(action_of(&["status"]), Action::Status);
        assert_eq!(action_of(&["tree", "--all"]), Action::Tree(TreeScope::All));
        assert_eq!(
            action_of(&["tree", "--workspace", WORK]),
            Action::Tree(workspace(WORK))
        );
        assert_eq!(
            action_of(&["workspace", "new", "Альфа", "--all"]),
            command(
                Command::WorkspaceCreate {
                    title: "Альфа".into()
                },
                TreeScope::All
            )
        );
        assert_eq!(
            action_of(&["workspace", "rename", "--target", HOME, "Бета", "--all"]),
            command(
                Command::WorkspaceRename {
                    workspace: id(HOME),
                    title: "Бета".into()
                },
                TreeScope::All
            )
        );
        assert_eq!(
            action_of(&["workspace", "focus", "--target", HOME, "--all"]),
            command(
                Command::WindowFocusWorkspace {
                    workspace: id(HOME)
                },
                TreeScope::All
            )
        );
        assert_eq!(
            action_of(&[
                "project",
                "add",
                "/src/app",
                "--title",
                "Приложение",
                "--target",
                WORK,
                "--all"
            ]),
            command(
                Command::ProjectAdd {
                    workspace: id(WORK),
                    path: PathBuf::from("/src/app"),
                    title: Some("Приложение".into()),
                },
                TreeScope::All
            )
        );
        assert_eq!(
            action_of(&[
                "workbench",
                "new",
                "--target",
                PROJECT,
                "Основная",
                "--workspace",
                WORK
            ]),
            command(
                Command::WorkbenchCreate {
                    project: id(PROJECT),
                    title: "Основная".into()
                },
                workspace(WORK)
            )
        );
        assert_eq!(
            action_of(&["workbench", "rename", "--target", BENCH, "Вторая", "--all"]),
            command(
                Command::WorkbenchRename {
                    workbench: id(BENCH),
                    title: "Вторая".into()
                },
                TreeScope::All
            )
        );
        assert_eq!(
            action_of(&["workbench", "open", "--target", BENCH, "--workspace", WORK]),
            command(
                Command::WorkbenchOpen {
                    workbench: id(BENCH)
                },
                workspace(WORK)
            )
        );
        assert_eq!(
            action_of(&["events", "--since", "12", "--all"]),
            Action::EventsSince(Revision(12))
        );
        assert_eq!(action_of(&["events", "--follow", "--all"]), Action::Follow);
        assert_eq!(action_of(&["ui", "window"]), Action::Ui(UiRequest::Window));
        assert_eq!(
            action_of(&["ui", "inspect"]),
            Action::Ui(UiRequest::Inspect)
        );
        assert_eq!(action_of(&["ui", "stats"]), Action::Ui(UiRequest::Stats));
        for choice in AppearanceChoice::ALL {
            assert_eq!(
                action_of(&["ui", "appearance", choice.as_str()]),
                Action::Ui(UiRequest::Command(UiCommand::AppearanceSet(choice)))
            );
        }
        assert_eq!(
            action_of(&["window", "close"]),
            Action::Ui(UiRequest::Command(UiCommand::WindowClose))
        );
        assert_eq!(
            action_of(&["app", "quit"]),
            Action::Ui(UiRequest::Command(UiCommand::AppQuit))
        );
    }

    #[test]
    fn ui_calls_take_no_scope_and_check_operands() {
        for args in [
            &["ui", "window", "--all"][..],
            &["ui", "stats", "--workspace", WORK],
            &["window", "close", "--all"],
            &["app", "quit", "--target", WORK],
            &["ui", "appearance", "dark", "--all"],
        ] {
            let message = refused(args);
            assert!(message.contains("не применим"), "{args:?}: {message}");
        }
        let cases: [(&[&str], &str); 6] = [
            (&["ui"], "не указано действие"),
            (&["ui", "window", "extra"], "операндов"),
            (&["ui", "appearance"], "операндов"),
            (&["ui", "appearance", "sepia"], "light, dark или system"),
            (&["ui", "appearance", "Dark"], "light, dark или system"),
            (&["app", "restart"], "неизвестный вызов"),
        ];
        for (args, expected) in cases {
            let message = refused(args);
            assert!(message.contains(expected), "{args:?}: {message}");
        }
        let flagged = run(&["--json", "ui", "window", "--timeout", "50"]);
        assert!(flagged.json);
        assert_eq!(flagged.action, Action::Ui(UiRequest::Window));
    }

    #[test]
    fn gallery_calls_map_to_gallery_commands() {
        let gallery = |command| Action::Ui(UiRequest::Gallery(command));
        assert_eq!(
            action_of(&["gallery", "cell", "light", "auto"]),
            gallery(GalleryCommand::Cell {
                appearance: Some(Lightness::Light),
                contrast: None,
            })
        );
        assert_eq!(
            action_of(&["gallery", "cell", "auto", "increased"]),
            gallery(GalleryCommand::Cell {
                appearance: None,
                contrast: Some(Contrast::Increased),
            })
        );
        assert_eq!(
            action_of(&["gallery", "cell", "dark", "normal"]),
            gallery(GalleryCommand::Cell {
                appearance: Some(Lightness::Dark),
                contrast: Some(Contrast::Normal),
            })
        );
        assert_eq!(
            action_of(&["gallery", "motion", "reduced"]),
            gallery(GalleryCommand::Motion(Motion::Reduced))
        );
        for focus in GalleryFocus::ALL {
            assert_eq!(
                action_of(&["gallery", "focus", focus.as_str()]),
                gallery(GalleryCommand::Focus(focus))
            );
        }
        assert_eq!(
            action_of(&["gallery", "menu", "open"]),
            gallery(GalleryCommand::MenuOpen)
        );
        assert_eq!(
            action_of(&["gallery", "rebuild"]),
            gallery(GalleryCommand::Rebuild)
        );
        assert_eq!(
            action_of(&["gallery", "window", "recreate"]),
            gallery(GalleryCommand::WindowRecreate)
        );
        assert_eq!(
            action_of(&["gallery", "place", "1"]),
            gallery(GalleryCommand::Place { screen: 1 })
        );
        assert_eq!(
            action_of(&["gallery", "reset"]),
            gallery(GalleryCommand::Reset)
        );
        for strip in GalleryStrip::ALL {
            assert_eq!(
                action_of(&["gallery", "tabs", "select", strip.as_str(), "7"]),
                gallery(GalleryCommand::TabsSelect { strip, tab: 7 })
            );
        }
        assert_eq!(
            action_of(&["gallery", "list", "replace", "3"]),
            gallery(GalleryCommand::ListReplace { seed: 3 })
        );
        assert_eq!(
            action_of(&["gallery", "list", "scroll", "9999"]),
            gallery(GalleryCommand::ListScroll { row: 9_999 })
        );
        assert_eq!(
            action_of(&["gallery", "list", "ticks", "on"]),
            gallery(GalleryCommand::ListTicks { enabled: true })
        );
        assert_eq!(
            action_of(&["gallery", "list", "ticks", "off"]),
            gallery(GalleryCommand::ListTicks { enabled: false })
        );
        for op in TreeMutation::ALL {
            assert_eq!(
                action_of(&["gallery", "tree", "mutate", op.as_str()]),
                gallery(GalleryCommand::TreeMutate { op })
            );
        }
    }

    #[test]
    fn gallery_calls_check_operands() {
        let cases: [(&[&str], &str); 19] = [
            (&["gallery"], "не указано действие"),
            (&["gallery", "cell", "light"], "операндов"),
            (
                &["gallery", "cell", "sepia", "auto"],
                "light, dark или auto",
            ),
            (
                &["gallery", "cell", "auto", "high"],
                "normal, increased или auto",
            ),
            (&["gallery", "motion", "slow"], "normal или reduced"),
            (
                &["gallery", "focus", "grid"],
                "field, window, tabs, list или tree",
            ),
            (&["gallery", "tree", "grow", "insert"], "неизвестный вызов"),
            (
                &["gallery", "tree", "mutate", "shuffle"],
                "insert, reorder или collapse_parent_of_selected",
            ),
            (&["gallery", "list", "sort", "1"], "неизвестный вызов"),
            (&["gallery", "list", "replace", "1e3"], "зерно"),
            (&["gallery", "list", "scroll", "конец"], "номер строки"),
            (&["gallery", "list", "ticks", "yes"], "on или off"),
            (
                &["gallery", "tabs", "close", "side", "1"],
                "неизвестный вызов",
            ),
            (
                &["gallery", "tabs", "select", "left", "1"],
                "side, center, half или wide",
            ),
            (
                &["gallery", "tabs", "select", "side", "первая"],
                "номер вкладки",
            ),
            (&["gallery", "menu", "close"], "неизвестный вызов"),
            (&["gallery", "window", "close"], "неизвестный вызов"),
            (&["gallery", "place", "первый"], "номер экрана"),
            (&["gallery", "reset", "now"], "операндов"),
        ];
        for (args, expected) in cases {
            let message = refused(args);
            assert!(message.contains(expected), "{args:?}: {message}");
        }
        let message = refused(&["gallery", "reset", "--all"]);
        assert!(message.contains("не применим"), "{message}");
    }

    #[test]
    fn common_flags_anywhere_with_defaults() {
        let plain = run(&["status"]);
        assert!(!plain.json);
        assert_eq!(plain.timeout, CLI_TIMEOUT);
        assert_eq!(plain.socket, None);

        let flagged = run(&[
            "--json",
            "tree",
            "--timeout",
            "50",
            "--all",
            "--socket=/s.sock",
        ]);
        assert!(flagged.json);
        assert_eq!(flagged.timeout, Duration::from_millis(50));
        assert_eq!(flagged.socket, Some(PathBuf::from("/s.sock")));
        assert_eq!(flagged.action, Action::Tree(TreeScope::All));
    }

    #[test]
    fn help_wins_over_everything() {
        assert_eq!(parse_args(&["--help"]), Ok(Parsed::Help));
        assert_eq!(parse_args(&["tree", "-h"]), Ok(Parsed::Help));
    }

    #[test]
    fn scope_is_required_where_table_has_it() {
        for args in [
            &["tree"][..],
            &["workspace", "new", "Альфа"],
            &["workspace", "rename", "--target", HOME, "Бета"],
            &["workspace", "focus", "--target", HOME],
            &["project", "add", "/src", "--target", WORK],
            &["workbench", "new", "--target", PROJECT, "Основная"],
            &["workbench", "rename", "--target", BENCH, "Вторая"],
            &["workbench", "open", "--target", BENCH],
            &["events", "--since", "1"],
            &["events", "--follow"],
        ] {
            let message = refused(args);
            assert!(message.contains("область"), "{args:?}: {message}");
        }
        let message = refused(&["tree", "--workspace", WORK, "--all"]);
        assert!(message.contains("взаимоисключающие"), "{message}");
    }

    #[test]
    fn status_and_version_take_no_scope() {
        for args in [
            &["status", "--all"][..],
            &["status", "--workspace", WORK],
            &["version", "--all"],
            &["version", "--workspace", WORK],
        ] {
            let message = refused(args);
            assert!(message.contains("не применим"), "{args:?}: {message}");
        }
    }

    #[test]
    fn all_only_calls_refuse_workspace_scope() {
        for args in [
            &["workspace", "new", "Альфа", "--workspace", WORK][..],
            &["events", "--since", "1", "--workspace", WORK],
            &["events", "--follow", "--workspace", WORK],
        ] {
            let message = refused(args);
            assert!(message.contains("--workspace"), "{args:?}: {message}");
        }
    }

    #[test]
    fn since_and_follow_are_exclusive() {
        let message = refused(&["events", "--since", "3", "--follow", "--all"]);
        assert!(message.contains("взаимоисключающие"), "{message}");
        let message = refused(&["events", "--all"]);
        assert!(message.contains("--since"), "{message}");
        let message = refused(&["events", "--since", "x", "--all"]);
        assert!(message.contains("--since"), "{message}");
    }

    #[test]
    fn workspace_target_defaults_to_scope_workspace() {
        assert_eq!(
            action_of(&["workspace", "rename", "Бета", "--workspace", WORK]),
            command(
                Command::WorkspaceRename {
                    workspace: id(WORK),
                    title: "Бета".into()
                },
                workspace(WORK)
            )
        );
        assert_eq!(
            action_of(&["workspace", "focus", "--workspace", WORK]),
            command(
                Command::WindowFocusWorkspace {
                    workspace: id(WORK)
                },
                workspace(WORK)
            )
        );
        assert_eq!(
            action_of(&["project", "add", "/src", "--workspace", WORK]),
            command(
                Command::ProjectAdd {
                    workspace: id(WORK),
                    path: PathBuf::from("/src"),
                    title: None,
                },
                workspace(WORK)
            )
        );
        assert_eq!(
            action_of(&[
                "workspace",
                "rename",
                "--target",
                HOME,
                "Бета",
                "--workspace",
                WORK
            ]),
            command(
                Command::WorkspaceRename {
                    workspace: id(HOME),
                    title: "Бета".into()
                },
                workspace(WORK)
            ),
            "явная цель отправляется как есть: область проверяет сервер"
        );
        for args in [
            &["workspace", "rename", "Бета", "--all"][..],
            &["workspace", "focus", "--all"],
            &["project", "add", "/src", "--all"],
        ] {
            let message = refused(args);
            assert!(message.contains("--target"), "{args:?}: {message}");
        }
    }

    #[test]
    fn workbench_target_is_required_even_with_workspace() {
        for args in [
            &["workbench", "new", "Основная", "--workspace", WORK][..],
            &["workbench", "rename", "Вторая", "--workspace", WORK],
            &["workbench", "open", "--workspace", WORK],
        ] {
            let message = refused(args);
            assert!(message.contains("--target"), "{args:?}: {message}");
        }
    }

    #[test]
    fn relative_project_path_becomes_absolute() {
        let add = |path: &str| match action_of(&["project", "add", path, "--target", WORK, "--all"])
        {
            Action::Command {
                command: Command::ProjectAdd { path, .. },
                ..
            } => path,
            other => panic!("ожидался project.add, получено {other:?}"),
        };
        assert_eq!(add("src/app"), PathBuf::from("/cwd/src/app"));
        assert_eq!(
            add("../app"),
            PathBuf::from("/cwd/../app"),
            "без канонизации"
        );
        assert_eq!(add("/abs"), PathBuf::from("/abs"));
        let message = refused(&["project", "add", "", "--target", WORK, "--all"]);
        assert!(message.contains("абсолютным"), "{message}");
    }

    #[test]
    fn operands_after_double_dash_are_not_flags() {
        assert_eq!(
            action_of(&["workspace", "new", "--all", "--", "--всё"]),
            command(
                Command::WorkspaceCreate {
                    title: "--всё".into()
                },
                TreeScope::All
            )
        );
        assert_eq!(
            action_of(&["workspace", "new", "-", "--all"]),
            command(
                Command::WorkspaceCreate { title: "-".into() },
                TreeScope::All
            )
        );
    }

    #[test]
    fn malformed_invocations_are_usage_errors() {
        let cases: [(&[&str], &str); 16] = [
            (&[], "не указан вызов"),
            (&["launch"], "неизвестный вызов"),
            (&["workspace"], "не указано действие"),
            (&["workspace", "delete", "--all"], "неизвестный вызов"),
            (&["status", "--verbose"], "неизвестный флаг"),
            (&["status", "-v"], "неизвестный флаг"),
            (&["status", "--json", "--json"], "дважды"),
            (&["status", "--json=yes"], "не принимает значения"),
            (&["tree", "--workspace"], "нужно значение"),
            (&["tree", "--workspace", "nope"], "неверный идентификатор"),
            (&["status", "extra"], "операндов"),
            (&["workspace", "new", "--all"], "операндов"),
            (&["workspace", "new", "А", "Б", "--all"], "операндов"),
            (&["status", "--timeout", "0"], "--timeout"),
            (&["status", "--timeout", "soon"], "--timeout"),
            (&["status", "--socket", ""], "--socket"),
        ];
        for (args, expected) in cases {
            let message = refused(args);
            assert!(message.contains(expected), "{args:?}: {message}");
        }
        let message = refused(&["tree", "--title", "x", "--all"]);
        assert!(message.contains("не применим"), "{message}");
    }

    #[test]
    fn non_utf8_argument_is_usage_error() {
        use std::os::unix::ffi::OsStringExt;

        let args = vec![
            OsString::from("workspace"),
            OsString::from("new"),
            OsString::from_vec(vec![0xff, 0xfe]),
            OsString::from("--all"),
        ];
        let Err(Usage(message)) = parse(args, &fake_absolute) else {
            panic!("ожидалась ошибка использования");
        };
        assert!(message.contains("UTF-8"), "{message}");
    }

    const PANEL: &str = "00000000-0000-0000-0000-000000000005";
    const ANCHOR: &str = "00000000-0000-0000-0000-000000000006";

    #[test]
    fn layout_and_panel_call_forms() {
        let all = |args: &[&str]| {
            let mut args = args.to_vec();
            args.push("--all");
            action_of(&args)
        };
        let cmd = |command: Command| Action::Command {
            command,
            scope: TreeScope::All,
        };
        let bench = Some(id::<WorkbenchId>(BENCH));
        assert_eq!(
            action_of(&["layout", "get", "--workspace", WORK]),
            Action::Layout {
                scope: workspace(WORK),
                workbench: None
            }
        );
        assert_eq!(
            all(&["layout", "get", "--workbench", BENCH]),
            Action::Layout {
                scope: TreeScope::All,
                workbench: bench
            }
        );
        assert_eq!(
            all(&["layout", "split", PANEL]),
            cmd(Command::LayoutSplit {
                panel: id(PANEL),
                open: None
            })
        );
        assert_eq!(
            all(&["layout", "split", PANEL, "--open", "Bad Id"]),
            cmd(Command::LayoutSplit {
                panel: id(PANEL),
                open: Some("Bad Id".into())
            }),
            "plugin id проверяет ядро"
        );
        assert_eq!(
            all(&["layout", "resize", "--zone", "right", "400"]),
            cmd(Command::LayoutResize {
                workbench: None,
                zone: Zone::Right,
                part_of: None,
                size: 400
            })
        );
        assert_eq!(
            all(&[
                "layout",
                "resize",
                "--zone",
                "center",
                "--part-of",
                PANEL,
                "--workbench",
                BENCH,
                "250"
            ]),
            cmd(Command::LayoutResize {
                workbench: bench,
                zone: Zone::Center,
                part_of: Some(id(PANEL)),
                size: 250
            })
        );
        assert_eq!(
            all(&["layout", "equal", "--zone", "bottom"]),
            cmd(Command::LayoutEqual {
                workbench: None,
                zone: Zone::Bottom
            })
        );
        assert_eq!(
            all(&["layout", "zoom", PANEL]),
            cmd(Command::LayoutZoom {
                panel: Some(id(PANEL)),
                workbench: None,
                off: None
            })
        );
        assert_eq!(
            all(&["layout", "zoom", "--off", "--workbench", BENCH]),
            cmd(Command::LayoutZoom {
                panel: None,
                workbench: bench,
                off: Some(true)
            })
        );
        assert_eq!(
            all(&["layout", "only"]),
            cmd(Command::LayoutOnly { workbench: None })
        );
        for (verb, visible) in [("show", true), ("hide", false)] {
            for (name, target) in [
                ("navigator", Hideable::Navigator),
                ("plate", Hideable::Plate),
                ("right", Hideable::Right),
                ("bottom", Hideable::Bottom),
            ] {
                assert_eq!(
                    all(&["layout", verb, name]),
                    cmd(Command::LayoutVisibility {
                        workbench: None,
                        target,
                        visible
                    })
                );
            }
        }
        assert_eq!(
            all(&["panel", "focus", PANEL]),
            cmd(Command::PanelFocus {
                panel: Some(id(PANEL)),
                workbench: None,
                direction: None
            })
        );
        for (key, direction) in [
            ("h", Direction::Left),
            ("j", Direction::Down),
            ("k", Direction::Up),
            ("l", Direction::Right),
        ] {
            assert_eq!(
                all(&["panel", "focus", "--dir", key, "--workbench", BENCH]),
                cmd(Command::PanelFocus {
                    panel: None,
                    workbench: bench,
                    direction: Some(direction)
                })
            );
        }
        assert_eq!(
            all(&["panel", "select", PANEL]),
            cmd(Command::PanelSelect { panel: id(PANEL) })
        );
        assert_eq!(
            all(&["panel", "close", PANEL]),
            cmd(Command::PanelClose { panel: id(PANEL) })
        );
        for (name, zone) in [
            ("left", Zone::Left),
            ("right", Zone::Right),
            ("center", Zone::Center),
            ("bottom", Zone::Bottom),
        ] {
            assert_eq!(
                all(&["panel", "move", PANEL, "--zone", name]),
                cmd(Command::PanelMove {
                    panel: id(PANEL),
                    zone
                })
            );
        }
        assert_eq!(
            all(&["panel", "move-part", PANEL, "--to", ANCHOR]),
            cmd(Command::PanelMovePart {
                panel: id(PANEL),
                anchor: id(ANCHOR),
                edge: None
            })
        );
        assert_eq!(
            all(&[
                "panel",
                "move-part",
                PANEL,
                "--to",
                ANCHOR,
                "--edge",
                "before"
            ]),
            cmd(Command::PanelMovePart {
                panel: id(PANEL),
                anchor: id(ANCHOR),
                edge: Some(Edge::Before)
            })
        );
        assert_eq!(
            action_of(&[
                "panel",
                "move-part",
                PANEL,
                "--to",
                ANCHOR,
                "--edge=after",
                "--workspace",
                WORK
            ]),
            command(
                Command::PanelMovePart {
                    panel: id(PANEL),
                    anchor: id(ANCHOR),
                    edge: Some(Edge::After)
                },
                workspace(WORK)
            )
        );
    }

    #[test]
    fn plugin_panel_call_forms() {
        let all = |args: &[&str]| {
            let mut args = args.to_vec();
            args.push("--all");
            action_of(&args)
        };
        let cmd = |command: Command| Action::Command {
            command,
            scope: TreeScope::All,
        };
        assert_eq!(
            all(&["panel", "open", "demo-list"]),
            cmd(Command::PanelOpen {
                plugin: "demo-list".into(),
                workbench: None,
                zone: None,
                args: None
            })
        );
        assert_eq!(
            all(&["panel", "open", "Bad Id"]),
            cmd(Command::PanelOpen {
                plugin: "Bad Id".into(),
                workbench: None,
                zone: None,
                args: None
            }),
            "plugin id проверяет ядро"
        );
        for (name, zone) in [
            ("left", Zone::Left),
            ("right", Zone::Right),
            ("center", Zone::Center),
            ("bottom", Zone::Bottom),
        ] {
            assert_eq!(
                all(&[
                    "panel",
                    "open",
                    "demo-list",
                    "--workbench",
                    BENCH,
                    "--zone",
                    name,
                    "--args",
                    r#"{"key":"c"}"#
                ]),
                cmd(Command::PanelOpen {
                    plugin: "demo-list".into(),
                    workbench: Some(id(BENCH)),
                    zone: Some(zone),
                    args: Some(serde_json::json!({"key": "c"}))
                })
            );
        }
        assert_eq!(
            action_of(&[
                "panel",
                "open",
                "demo-native",
                "--args={}",
                "--workspace",
                WORK
            ]),
            command(
                Command::PanelOpen {
                    plugin: "demo-native".into(),
                    workbench: None,
                    zone: None,
                    args: Some(serde_json::json!({}))
                },
                workspace(WORK)
            ),
            "область PerApp проверяет ядро, пустой объект отличается от отсутствия --args"
        );
        assert_eq!(
            all(&["panel", "command", PANEL, "demo-list.back"]),
            cmd(Command::PanelCommand {
                panel: id(PANEL),
                name: "demo-list.back".into(),
                args: None
            })
        );
        assert_eq!(
            action_of(&[
                "panel",
                "command",
                PANEL,
                "demo-native.set",
                "--args",
                r#"{"value":"проверка","nested":{"list":[1,null]}}"#,
                "--workspace",
                WORK
            ]),
            command(
                Command::PanelCommand {
                    panel: id(PANEL),
                    name: "demo-native.set".into(),
                    args: Some(serde_json::json!({
                        "value": "проверка",
                        "nested": {"list": [1, null]}
                    }))
                },
                workspace(WORK)
            )
        );
        assert_eq!(
            all(&["panel", "command", PANEL, "no such action"]),
            cmd(Command::PanelCommand {
                panel: id(PANEL),
                name: "no such action".into(),
                args: None
            }),
            "имя действия проверяет ядро"
        );
        assert_eq!(
            all(&["panel", "get", PANEL]),
            Action::Panel {
                scope: TreeScope::All,
                panel: id(PANEL)
            }
        );
        assert_eq!(
            action_of(&["panel", "get", PANEL, "--workspace", WORK]),
            Action::Panel {
                scope: workspace(WORK),
                panel: id(PANEL)
            }
        );
        assert_eq!(action_of(&["plugin", "list"]), Action::Plugins);
        let flagged = run(&["--json", "plugin", "list", "--timeout", "50"]);
        assert!(flagged.json);
        assert_eq!(flagged.action, Action::Plugins);
    }

    #[test]
    fn args_flag_takes_only_json_object() {
        for value in [
            "[1]",
            "null",
            "7",
            "true",
            r#""key""#,
            "{",
            "",
            "key=c",
            r#"{"key":"c"} {}"#,
        ] {
            for call in [
                &["panel", "open", "demo-list"][..],
                &["panel", "command", PANEL, "demo-list.select"],
            ] {
                let mut args = call.to_vec();
                args.extend(["--args", value, "--all"]);
                let message = refused(&args);
                assert!(
                    message.contains("--args: ожидается объект JSON"),
                    "{args:?}: {message}"
                );
            }
        }
        for args in [
            &["panel", "get", PANEL, "--args", "{}", "--all"][..],
            &["panel", "close", PANEL, "--args", "{}", "--all"],
            &["layout", "split", PANEL, "--args", "{}", "--all"],
            &["plugin", "list", "--args", "{}"],
        ] {
            let message = refused(args);
            assert!(message.contains("не применим"), "{args:?}: {message}");
        }
        let message = refused(&["panel", "open", "demo-list", "--all", "--args"]);
        assert!(message.contains("флагу --args нужно значение"), "{message}");
    }

    #[test]
    fn plugin_panel_misuse_is_usage_error() {
        let cases: &[(&[&str], &str)] = &[
            (&["plugin"], "не указано действие"),
            (&["plugin", "get", "--all"], "неизвестный вызов"),
            (&["plugin", "list", "extra"], "операндов"),
            (&["plugin", "list", "--all"], "не применим"),
            (&["plugin", "list", "--workspace", WORK], "не применим"),
            (&["panel", "open", "demo-list"], "нужна область"),
            (&["panel", "open", "--all"], "операндов"),
            (
                &["panel", "open", "demo-list", "extra", "--all"],
                "операндов",
            ),
            (
                &["panel", "open", "demo-list", "--zone", "top", "--all"],
                "center",
            ),
            (
                &["panel", "open", "demo-list", "--workbench", "nope", "--all"],
                "неверный идентификатор",
            ),
            (
                &["panel", "open", "demo-list", "--to", ANCHOR, "--all"],
                "не применим",
            ),
            (
                &["panel", "open", "demo-list", "--workspace", WORK, "--all"],
                "взаимоисключающие",
            ),
            (
                &["panel", "command", PANEL, "demo-list.back"],
                "нужна область",
            ),
            (&["panel", "command", PANEL, "--all"], "операндов"),
            (
                &[
                    "panel",
                    "command",
                    PANEL,
                    "demo-list.back",
                    "extra",
                    "--all",
                ],
                "операндов",
            ),
            (
                &["panel", "command", "nope", "demo-list.back", "--all"],
                "идентификатор панели",
            ),
            (
                &[
                    "panel",
                    "command",
                    PANEL,
                    "demo-list.back",
                    "--workbench",
                    BENCH,
                    "--all",
                ],
                "не применим",
            ),
            (
                &[
                    "panel",
                    "command",
                    PANEL,
                    "demo-list.back",
                    "--zone",
                    "left",
                    "--all",
                ],
                "не применим",
            ),
            (&["panel", "get", PANEL], "нужна область"),
            (&["panel", "get", "--all"], "операндов"),
            (&["panel", "get", "nope", "--all"], "идентификатор панели"),
            (
                &["panel", "get", PANEL, "--workbench", BENCH, "--all"],
                "не применим",
            ),
        ];
        for (args, expected) in cases {
            let message = refused(args);
            assert!(message.contains(expected), "{args:?}: {message}");
        }
    }

    #[test]
    fn layout_and_panel_misuse_is_usage_error() {
        let cases: &[(&[&str], &str)] = &[
            (&["layout"], "не указано действие"),
            (&["layout", "split", PANEL], "нужна область"),
            (&["layout", "split", "--all"], "операндов"),
            (
                &["layout", "split", "nope", "--all"],
                "идентификатор панели",
            ),
            (
                &["layout", "split", PANEL, "--workbench", BENCH, "--all"],
                "не применим",
            ),
            (&["layout", "resize", "400", "--all"], "--zone"),
            (
                &["layout", "resize", "--zone", "top", "400", "--all"],
                "center",
            ),
            (
                &["layout", "resize", "--zone", "left", "--all", "--", "-4"],
                "целое без знака",
            ),
            (&["layout", "equal", "--all"], "--zone"),
            (&["layout", "zoom", "--all"], "панель или --off"),
            (
                &["layout", "zoom", PANEL, "--off", "--all"],
                "взаимоисключающие",
            ),
            (
                &["layout", "zoom", PANEL, "--workbench", BENCH, "--all"],
                "--workbench",
            ),
            (
                &["layout", "zoom", PANEL, ANCHOR, "--all"],
                "не больше одного",
            ),
            (&["layout", "off", "--all"], "неизвестный вызов"),
            (&["layout", "show", "center", "--all"], "navigator"),
            (&["layout", "hide", "--all"], "операндов"),
            (&["panel", "focus", "--all"], "панель или --dir"),
            (
                &["panel", "focus", PANEL, "--dir", "h", "--all"],
                "взаимоисключающие",
            ),
            (
                &["panel", "focus", PANEL, "--workbench", BENCH, "--all"],
                "--workbench",
            ),
            (&["panel", "focus", "--dir", "left", "--all"], "h, j, k, l"),
            (
                &["panel", "select", PANEL, "--workbench", BENCH, "--all"],
                "не применим",
            ),
            (&["panel", "move", PANEL, "--all"], "--zone"),
            (&["panel", "move-part", PANEL, "--all"], "--to"),
            (
                &[
                    "panel",
                    "move-part",
                    PANEL,
                    "--to",
                    ANCHOR,
                    "--edge",
                    "side",
                    "--all",
                ],
                "before",
            ),
            (&["panel", "close", PANEL, "--off", "--all"], "не применим"),
            (
                &["panel", "close", PANEL, "--off=1", "--all"],
                "не принимает значения",
            ),
        ];
        for (args, expected) in cases {
            let message = refused(args);
            assert!(message.contains(expected), "{args:?}: {message}");
        }
    }
}
