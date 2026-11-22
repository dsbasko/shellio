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

use shellio_control::{
    CLI_TIMEOUT, Contrast, GalleryCommand, GalleryFocus, GalleryStrip, Lightness, Motion, UiRequest,
};
use shellio_core::{
    AppearanceChoice, Command, ProjectId, Revision, TreeScope, UiCommand, WorkbenchId, WorkspaceId,
};

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
  gallery focus <field|window|tabs|list>
  gallery menu open
  gallery rebuild
  gallery window recreate
  gallery place <экран>
  gallery reset
  gallery tabs select <side|center|half|wide> <номер вкладки>
  gallery list replace <зерно>
  gallery list scroll <номер строки>

У команд с целью-воркспейсом --target по умолчанию равен --workspace, с --all он обязателен.
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
    Help,
}

impl Flag {
    const ALL: [Self; 10] = [
        Self::Json,
        Self::Timeout,
        Self::Socket,
        Self::Workspace,
        Self::All,
        Self::Target,
        Self::Title,
        Self::Since,
        Self::Follow,
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
            | Self::Since => true,
            Self::Json | Self::All | Self::Follow | Self::Help => false,
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
            | Self::Follow => false,
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
                "workspace" | "project" | "workbench" | "ui" | "window" | "app" | "gallery"
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
                        "{call}: ожидается field, window, tabs или list, получено «{value}»"
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
                _ => Err(usage(format!("неизвестный вызов «{call} {verb}»"))),
            }
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
    }

    #[test]
    fn gallery_calls_check_operands() {
        let cases: [(&[&str], &str); 16] = [
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
                "field, window, tabs или list",
            ),
            (&["gallery", "list", "sort", "1"], "неизвестный вызов"),
            (&["gallery", "list", "replace", "1e3"], "зерно"),
            (&["gallery", "list", "scroll", "конец"], "номер строки"),
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
}
