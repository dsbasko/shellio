//! Команды ядра и их подготовка до актора.

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use shellio_layout::{Direction, Edge, Hideable, Zone};

use crate::error::{CoreError, Entity};
use crate::ids::{PanelId, ProjectId, ProjectRootId, WorkbenchId, WorkspaceId};
use crate::model::{Route, State};
use crate::root::{ResolvedRoot, RootResolver};
use crate::tree::TreeScope;

/// Внешняя команда ядра. Имена в теге `cmd` стабильны.
///
/// Имена приходят строкой как есть: их проверяет обработчик и отвечает `invalid_argument`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", deny_unknown_fields)]
pub enum Command {
    /// Новый воркспейс в конец списка.
    #[serde(rename = "workspace.create")]
    WorkspaceCreate { title: String },
    /// Переименование воркспейса.
    #[serde(rename = "workspace.rename")]
    WorkspaceRename {
        workspace: WorkspaceId,
        title: String,
    },
    /// Проект с одним корнем в конец списка воркспейса.
    #[serde(rename = "project.add")]
    ProjectAdd {
        workspace: WorkspaceId,
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Новая рабочая область в конец проекта.
    #[serde(rename = "workbench.create")]
    WorkbenchCreate { project: ProjectId, title: String },
    /// Переименование рабочей области.
    #[serde(rename = "workbench.rename")]
    WorkbenchRename {
        workbench: WorkbenchId,
        title: String,
    },
    /// Открыть рабочую область в окне.
    #[serde(rename = "workbench.open")]
    WorkbenchOpen { workbench: WorkbenchId },
    /// Сделать воркспейс активным.
    #[serde(rename = "window.focus_workspace")]
    WindowFocusWorkspace { workspace: WorkspaceId },
    /// Деление части панели вдоль оси зоны. `open` — plugin id новой панели новой части.
    #[serde(rename = "layout.split")]
    LayoutSplit {
        panel: PanelId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        open: Option<String>,
    },
    /// Фокус на панель (`panel`) или на соседнюю плитку по направлению (`direction`).
    #[serde(rename = "panel.focus")]
    PanelFocus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        panel: Option<PanelId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workbench: Option<WorkbenchId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        direction: Option<Direction>,
    },
    /// Сделать вкладку активной в своей части.
    #[serde(rename = "panel.select")]
    PanelSelect { panel: PanelId },
    /// Перенос панели в последнюю активную часть зоны.
    #[serde(rename = "panel.move")]
    PanelMove { panel: PanelId, zone: Zone },
    /// Перенос вкладки в часть якоря, с `edge` — в новую часть рядом с ней.
    #[serde(rename = "panel.move_part")]
    PanelMovePart {
        panel: PanelId,
        anchor: PanelId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edge: Option<Edge>,
    },
    /// Закрытие вида панели. Сессии у панели до P2.1 нет, поэтому панель удаляется.
    #[serde(rename = "panel.close")]
    PanelClose { panel: PanelId },
    /// Размер зоны в точках, с `part_of` — доля части этой панели в тысячных.
    #[serde(rename = "layout.resize")]
    LayoutResize {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workbench: Option<WorkbenchId>,
        zone: Zone,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        part_of: Option<PanelId>,
        size: u32,
    },
    /// Равные доли частей зоны.
    #[serde(rename = "layout.equal")]
    LayoutEqual {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workbench: Option<WorkbenchId>,
        zone: Zone,
    },
    /// Разворот панели (`panel`) или его снятие (`off: true`).
    #[serde(rename = "layout.zoom")]
    LayoutZoom {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        panel: Option<PanelId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workbench: Option<WorkbenchId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        off: Option<bool>,
    },
    /// Видна только зона с фокусом; повтор возвращает прежнее скрытие.
    #[serde(rename = "layout.only")]
    LayoutOnly {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workbench: Option<WorkbenchId>,
    },
    /// Показ или скрытие колонки навигатора, плитки, правой или нижней зоны.
    #[serde(rename = "layout.visibility")]
    LayoutVisibility {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workbench: Option<WorkbenchId>,
        target: Hideable,
        visible: bool,
    },
    /// Открыть панель плагина по правилу его кардинальности (`open-or-focus`). `args` — контекст
    /// `init` новой панели и `reopen` существующей.
    #[serde(rename = "panel.open")]
    PanelOpen {
        plugin: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workbench: Option<WorkbenchId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        zone: Option<Zone>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        args: Option<Value>,
    },
    /// Действие плагина над данными панели. `name` — полное имя действия манифеста.
    #[serde(rename = "panel.command")]
    PanelCommand {
        panel: PanelId,
        name: String,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        args: Option<Value>,
    },
}

/// Поле `args`, если оно есть в запросе. `null` остаётся значением, а не отсутствием: его
/// отвергает подготовка команды.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// `args` команды — объект JSON.
fn object_args(command: &str, args: Option<Value>) -> Result<Option<Value>, CoreError> {
    match args {
        None | Some(Value::Object(_)) => Ok(args),
        Some(_) => Err(CoreError::invalid_argument(
            "args",
            "args_invalid",
            format!("{command}: args — не объект JSON"),
        )),
    }
}

impl Command {
    /// Стабильное имя команды, оно же значение тега `cmd`.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::WorkspaceCreate { .. } => "workspace.create",
            Self::WorkspaceRename { .. } => "workspace.rename",
            Self::ProjectAdd { .. } => "project.add",
            Self::WorkbenchCreate { .. } => "workbench.create",
            Self::WorkbenchRename { .. } => "workbench.rename",
            Self::WorkbenchOpen { .. } => "workbench.open",
            Self::WindowFocusWorkspace { .. } => "window.focus_workspace",
            Self::LayoutSplit { .. } => "layout.split",
            Self::PanelFocus { .. } => "panel.focus",
            Self::PanelSelect { .. } => "panel.select",
            Self::PanelMove { .. } => "panel.move",
            Self::PanelMovePart { .. } => "panel.move_part",
            Self::PanelClose { .. } => "panel.close",
            Self::LayoutResize { .. } => "layout.resize",
            Self::LayoutEqual { .. } => "layout.equal",
            Self::LayoutZoom { .. } => "layout.zoom",
            Self::LayoutOnly { .. } => "layout.only",
            Self::LayoutVisibility { .. } => "layout.visibility",
            Self::PanelOpen { .. } => "panel.open",
            Self::PanelCommand { .. } => "panel.command",
        }
    }

    /// Нужен ли команде блокирующий ввод-вывод до актора.
    pub const fn needs_io(&self) -> bool {
        match self {
            Self::ProjectAdd { .. } => true,
            Self::WorkspaceCreate { .. }
            | Self::WorkspaceRename { .. }
            | Self::WorkbenchCreate { .. }
            | Self::WorkbenchRename { .. }
            | Self::WorkbenchOpen { .. }
            | Self::WindowFocusWorkspace { .. }
            | Self::LayoutSplit { .. }
            | Self::PanelFocus { .. }
            | Self::PanelSelect { .. }
            | Self::PanelMove { .. }
            | Self::PanelMovePart { .. }
            | Self::PanelClose { .. }
            | Self::LayoutResize { .. }
            | Self::LayoutEqual { .. }
            | Self::LayoutZoom { .. }
            | Self::LayoutOnly { .. }
            | Self::LayoutVisibility { .. }
            | Self::PanelOpen { .. }
            | Self::PanelCommand { .. } => false,
        }
    }

    /// Область `scope` с целью команды. Цель берётся до подготовки: `prepare` меняет только
    /// корень `project.add`, от которого цель не зависит.
    pub(crate) const fn in_scope(&self, scope: TreeScope) -> CommandScope {
        CommandScope {
            scope,
            target: self.target(),
        }
    }

    const fn target(&self) -> Target {
        match self {
            Self::WorkspaceCreate { .. } => Target::Global(self.name()),
            Self::WorkspaceRename { workspace, .. }
            | Self::ProjectAdd { workspace, .. }
            | Self::WindowFocusWorkspace { workspace } => Target::Workspace(*workspace),
            Self::WorkbenchCreate { project, .. } => Target::Project(*project),
            Self::WorkbenchRename { workbench, .. } | Self::WorkbenchOpen { workbench } => {
                Target::Workbench(*workbench)
            }
            Self::LayoutSplit { panel, .. }
            | Self::PanelFocus {
                panel: Some(panel), ..
            }
            | Self::PanelSelect { panel }
            | Self::PanelMove { panel, .. }
            | Self::PanelMovePart { panel, .. }
            | Self::PanelClose { panel }
            | Self::PanelCommand { panel, .. }
            | Self::LayoutResize {
                part_of: Some(panel),
                ..
            }
            | Self::LayoutZoom {
                panel: Some(panel), ..
            } => Target::Panel(*panel),
            Self::PanelFocus {
                panel: None,
                workbench,
                ..
            }
            | Self::LayoutResize {
                part_of: None,
                workbench,
                ..
            }
            | Self::LayoutEqual { workbench, .. }
            | Self::LayoutZoom {
                panel: None,
                workbench,
                ..
            }
            | Self::LayoutOnly { workbench }
            | Self::LayoutVisibility { workbench, .. } => match workbench {
                Some(workbench) => Target::Workbench(*workbench),
                None => Target::OpenWorkbench,
            },
            Self::PanelOpen { .. } => Target::Handler,
        }
    }

    /// Подготовить команду для обработчика. Блокирует только для команд с [`Self::needs_io`]:
    /// корень `project.add` разрешается здесь, ошибка разрешения — `invalid_argument`.
    pub(crate) fn prepare(self, resolver: &dyn RootResolver) -> Result<PreparedCommand, CoreError> {
        Ok(match self {
            Self::WorkspaceCreate { title } => PreparedCommand::WorkspaceCreate { title },
            Self::WorkspaceRename { workspace, title } => {
                PreparedCommand::WorkspaceRename { workspace, title }
            }
            Self::ProjectAdd {
                workspace,
                path,
                title,
            } => PreparedCommand::ProjectAdd {
                workspace,
                root: resolver.resolve(&path)?,
                title,
            },
            Self::WorkbenchCreate { project, title } => {
                PreparedCommand::WorkbenchCreate { project, title }
            }
            Self::WorkbenchRename { workbench, title } => {
                PreparedCommand::WorkbenchRename { workbench, title }
            }
            Self::WorkbenchOpen { workbench } => PreparedCommand::WorkbenchOpen { workbench },
            Self::WindowFocusWorkspace { workspace } => {
                PreparedCommand::WindowFocusWorkspace { workspace }
            }
            Self::LayoutSplit { panel, open } => {
                PreparedCommand::Layout(LayoutCommand::Split { panel, open })
            }
            Self::PanelFocus {
                panel,
                workbench,
                direction,
            } => PreparedCommand::Layout(match (panel, direction) {
                (Some(panel), None) if workbench.is_none() => LayoutCommand::Focus { panel },
                (None, Some(direction)) => LayoutCommand::FocusDirection {
                    workbench,
                    direction,
                },
                (Some(_), Some(_)) => {
                    return Err(CoreError::arguments_conflict(
                        "direction",
                        "panel.focus: panel и direction не задаются вместе",
                    ));
                }
                (Some(_), None) => {
                    return Err(CoreError::arguments_conflict(
                        "workbench",
                        "panel.focus: рабочая область берётся у панели, workbench не задаётся",
                    ));
                }
                (None, None) => {
                    return Err(CoreError::arguments_conflict(
                        "panel",
                        "panel.focus: нужен panel или direction",
                    ));
                }
            }),
            Self::PanelSelect { panel } => PreparedCommand::Layout(LayoutCommand::Select { panel }),
            Self::PanelMove { panel, zone } => {
                PreparedCommand::Layout(LayoutCommand::Move { panel, zone })
            }
            Self::PanelMovePart {
                panel,
                anchor,
                edge,
            } => PreparedCommand::Layout(LayoutCommand::MovePart {
                panel,
                anchor,
                edge,
            }),
            Self::PanelClose { panel } => PreparedCommand::Layout(LayoutCommand::Close { panel }),
            Self::LayoutResize {
                workbench,
                zone,
                part_of,
                size,
            } => PreparedCommand::Layout(LayoutCommand::Resize {
                workbench,
                zone,
                part_of,
                size,
            }),
            Self::LayoutEqual { workbench, zone } => {
                PreparedCommand::Layout(LayoutCommand::Equal { workbench, zone })
            }
            Self::LayoutZoom {
                panel,
                workbench,
                off,
            } => PreparedCommand::Layout(match (panel, off) {
                (Some(panel), None) if workbench.is_none() => LayoutCommand::Zoom { panel },
                (None, Some(true)) => LayoutCommand::Unzoom { workbench },
                (None, Some(false)) => {
                    return Err(CoreError::arguments_conflict(
                        "off",
                        "layout.zoom: off принимает только true",
                    ));
                }
                (Some(_), Some(_)) => {
                    return Err(CoreError::arguments_conflict(
                        "off",
                        "layout.zoom: panel и off не задаются вместе",
                    ));
                }
                (Some(_), None) => {
                    return Err(CoreError::arguments_conflict(
                        "workbench",
                        "layout.zoom: рабочая область берётся у панели, workbench не задаётся",
                    ));
                }
                (None, None) => {
                    return Err(CoreError::arguments_conflict(
                        "panel",
                        "layout.zoom: нужен panel или off",
                    ));
                }
            }),
            Self::LayoutOnly { workbench } => {
                PreparedCommand::Layout(LayoutCommand::Only { workbench })
            }
            Self::LayoutVisibility {
                workbench,
                target,
                visible,
            } => PreparedCommand::Layout(LayoutCommand::Visibility {
                workbench,
                target,
                visible,
            }),
            Self::PanelOpen {
                plugin,
                workbench,
                zone,
                args,
            } => PreparedCommand::PanelOpen {
                plugin,
                workbench,
                zone,
                args: object_args("panel.open", args)?,
            },
            Self::PanelCommand { panel, name, args } => PreparedCommand::PanelCommand {
                panel,
                name,
                args: object_args("panel.command", args)?,
            },
        })
    }
}

/// Команда раскладки после проверки сочетания полей. Рабочую область выбирает обработчик.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LayoutCommand {
    Split {
        panel: PanelId,
        open: Option<String>,
    },
    Focus {
        panel: PanelId,
    },
    FocusDirection {
        workbench: Option<WorkbenchId>,
        direction: Direction,
    },
    Select {
        panel: PanelId,
    },
    Move {
        panel: PanelId,
        zone: Zone,
    },
    MovePart {
        panel: PanelId,
        anchor: PanelId,
        edge: Option<Edge>,
    },
    Close {
        panel: PanelId,
    },
    Resize {
        workbench: Option<WorkbenchId>,
        zone: Zone,
        part_of: Option<PanelId>,
        size: u32,
    },
    Equal {
        workbench: Option<WorkbenchId>,
        zone: Zone,
    },
    Zoom {
        panel: PanelId,
    },
    Unzoom {
        workbench: Option<WorkbenchId>,
    },
    Only {
        workbench: Option<WorkbenchId>,
    },
    Visibility {
        workbench: Option<WorkbenchId>,
        target: Hideable,
        visible: bool,
    },
}

/// Команда, готовая к чистому обработчику: весь ввод-вывод выполнен до актора.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreparedCommand {
    WorkspaceCreate {
        title: String,
    },
    WorkspaceRename {
        workspace: WorkspaceId,
        title: String,
    },
    /// `project.add` с уже разрешённым корнем.
    ProjectAdd {
        workspace: WorkspaceId,
        root: ResolvedRoot,
        title: Option<String>,
    },
    WorkbenchCreate {
        project: ProjectId,
        title: String,
    },
    WorkbenchRename {
        workbench: WorkbenchId,
        title: String,
    },
    WorkbenchOpen {
        workbench: WorkbenchId,
    },
    WindowFocusWorkspace {
        workspace: WorkspaceId,
    },
    Layout(LayoutCommand),
    PanelOpen {
        plugin: String,
        workbench: Option<WorkbenchId>,
        zone: Option<Zone>,
        args: Option<Value>,
    },
    PanelCommand {
        panel: PanelId,
        name: String,
        args: Option<Value>,
    },
}

/// Цель команды для проверки области.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// Команда без цели: допустима только в области всех воркспейсов.
    Global(&'static str),
    Workspace(WorkspaceId),
    Project(ProjectId),
    Workbench(WorkbenchId),
    /// Рабочая область панели.
    Panel(PanelId),
    /// Рабочая область, открытая в окне.
    OpenWorkbench,
    /// Область проверяет обработчик: у `panel.open` она идёт после проверки plugin id.
    Handler,
}

/// Область команды и её цель: проверяются до подготовки и в акторе.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommandScope {
    scope: TreeScope,
    target: Target,
}

impl CommandScope {
    /// Область команды.
    pub(crate) const fn scope(self) -> TreeScope {
        self.scope
    }

    /// Часть проверки, которой не нужно состояние. Идёт до подготовки, поэтому прямой
    /// воркспейс `project.add` вне области отвергается без разрешения корня. Прямой воркспейс
    /// вне области отвергается раньше проверки самой области: так отказ одинаков до и после
    /// подготовки.
    pub(crate) fn precheck(self) -> Result<(), CoreError> {
        let scope = match self.scope {
            TreeScope::All => return Ok(()),
            TreeScope::Workspace(scope) => scope,
        };
        match self.target {
            Target::Global(command) => Err(CoreError::scope_requires_all(command)),
            Target::Workspace(workspace) if workspace != scope => {
                Err(CoreError::not_found(Entity::Workspace, workspace))
            }
            Target::Workspace(_)
            | Target::Project(_)
            | Target::Workbench(_)
            | Target::Panel(_)
            | Target::OpenWorkbench
            | Target::Handler => Ok(()),
        }
    }

    /// Полная проверка на снимке, на котором затем выполняется обработчик. Цель вне области —
    /// `not_found`, как у несуществующей записи.
    pub(crate) fn check(self, state: &State) -> Result<(), CoreError> {
        self.precheck()?;
        let scope = match (self.scope, self.target) {
            (TreeScope::All, _) | (_, Target::Handler) => return Ok(()),
            (TreeScope::Workspace(scope), _) => scope,
        };
        if state.live_workspace(scope).is_none() {
            return Err(CoreError::not_found(Entity::Workspace, scope));
        }
        match self.target {
            Target::Global(_) | Target::Workspace(_) | Target::Handler => Ok(()),
            Target::Project(project) => {
                if state
                    .projects
                    .get(&project)
                    .is_some_and(|record| record.workspace == scope)
                {
                    Ok(())
                } else {
                    Err(CoreError::not_found(Entity::Project, project))
                }
            }
            Target::Workbench(workbench) => {
                if state.workbench_workspace(workbench) == Some(scope) {
                    Ok(())
                } else {
                    Err(CoreError::not_found(Entity::Workbench, workbench))
                }
            }
            Target::Panel(panel) => {
                let owner = state
                    .panels
                    .get(&panel)
                    .and_then(|record| state.workbench_workspace(record.workbench));
                if owner == Some(scope) {
                    Ok(())
                } else {
                    Err(CoreError::not_found(Entity::Panel, panel))
                }
            }
            Target::OpenWorkbench => match open_workbench(state) {
                Some(workbench) if state.workbench_workspace(workbench) != Some(scope) => {
                    Err(CoreError::open_workbench_hidden())
                }
                Some(_) | None => Ok(()),
            },
        }
    }
}

/// Рабочая область, открытая в окне, если она жива.
pub fn open_workbench(state: &State) -> Option<WorkbenchId> {
    match state.window.route {
        Route::Workbench(workbench) if state.live_workbench(workbench).is_some() => Some(workbench),
        Route::Workbench(_) | Route::WorkspaceHome => None,
    }
}

/// Рабочая область команды или чтения раскладки без проверки области доступа: явная живая, иначе
/// открытая в окне, иначе `no_open_workbench`.
pub(crate) fn chosen_workbench(
    state: &State,
    workbench: Option<WorkbenchId>,
) -> Result<WorkbenchId, CoreError> {
    match workbench {
        Some(workbench) => state
            .live_workbench(workbench)
            .map(|record| record.id)
            .ok_or_else(|| CoreError::not_found(Entity::Workbench, workbench)),
        None => open_workbench(state).ok_or_else(CoreError::no_open_workbench),
    }
}

/// Рабочая область чтения `query.layout` в области `scope`: та же проверка области доступа, что у
/// команды без панели, затем тот же выбор рабочей области.
pub(crate) fn scoped_workbench(
    state: &State,
    scope: TreeScope,
    workbench: Option<WorkbenchId>,
) -> Result<WorkbenchId, CoreError> {
    let target = workbench.map_or(Target::OpenWorkbench, Target::Workbench);
    CommandScope { scope, target }.check(state)?;
    chosen_workbench(state, workbench)
}

/// Проверка области чтения по явной панели: как у команды с панелью.
pub(crate) fn scoped_panel(
    state: &State,
    scope: TreeScope,
    panel: PanelId,
) -> Result<WorkbenchId, CoreError> {
    CommandScope {
        scope,
        target: Target::Panel(panel),
    }
    .check(state)?;
    panel_workbench(state, panel)
}

/// Рабочая область живой панели. Панель удалённой рабочей области не найдена.
pub(crate) fn panel_workbench(state: &State, panel: PanelId) -> Result<WorkbenchId, CoreError> {
    state
        .panels
        .get(&panel)
        .map(|record| record.workbench)
        .filter(|&workbench| state.live_workbench(workbench).is_some())
        .ok_or_else(|| CoreError::not_found(Entity::Panel, panel))
}

/// Исход `panel.open`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenOutcome {
    /// Новая панель.
    Created,
    /// Канонический экземпляр получил фокус на месте.
    Focused,
    /// Канонический экземпляр переехал в целевую рабочую область.
    Moved,
    /// Окно перешло к рабочей области владельца экземпляра.
    Routed,
}

/// Значение ответа команды: идентификаторы созданных записей.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum CommandValue {
    /// `panel.open`: панель, рабочая область, где она стоит, и исход.
    Opened {
        panel: PanelId,
        workbench: WorkbenchId,
        outcome: OpenOutcome,
    },
    /// `project.add`.
    Project {
        project: ProjectId,
        root: ProjectRootId,
    },
    /// `workspace.create`.
    Workspace { workspace: WorkspaceId },
    /// `workbench.create`.
    Workbench { workbench: WorkbenchId },
    /// `layout.split` с `open`: новая панель. `panel.command`: панель действия.
    Panel { panel: PanelId },
    /// Команды без значения.
    None,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::error::ErrorCode;
    use crate::ids::{IdGen, SequentialIds};
    use crate::root::{RootError, RootKey, RootKind};
    use crate::test_support::FakeRootResolver;

    fn all_commands() -> Vec<Command> {
        let mut ids = SequentialIds::new();
        let ids: &mut dyn IdGen = &mut ids;
        let workspace = WorkspaceId::generate(ids);
        let project = ProjectId::generate(ids);
        let workbench = WorkbenchId::generate(ids);
        vec![
            Command::WorkspaceCreate {
                title: "Работа".into(),
            },
            Command::WorkspaceRename {
                workspace,
                title: "Дом".into(),
            },
            Command::ProjectAdd {
                workspace,
                path: "/src/shellio".into(),
                title: None,
            },
            Command::WorkbenchCreate {
                project,
                title: "Ревью".into(),
            },
            Command::WorkbenchRename {
                workbench,
                title: "Основная".into(),
            },
            Command::WorkbenchOpen { workbench },
            Command::WindowFocusWorkspace { workspace },
        ]
    }

    #[test]
    fn name_matches_json_tag() {
        let commands = all_commands();
        assert_eq!(commands.len(), 7);
        for command in commands {
            let json = serde_json::to_value(&command).unwrap();
            assert_eq!(json["cmd"], command.name());
            assert_eq!(serde_json::from_value::<Command>(json).unwrap(), command);
        }
    }

    #[test]
    fn only_project_add_needs_io() {
        for command in all_commands() {
            assert_eq!(command.needs_io(), command.name() == "project.add");
        }
    }

    #[test]
    fn prepare_resolves_root_of_project_add() {
        let resolver = FakeRootResolver::new().with_git("/src/wt", "/src/main/.git");
        let mut ids = SequentialIds::new();
        let workspace = WorkspaceId::generate(&mut ids);
        let prepared = Command::ProjectAdd {
            workspace,
            path: "/src/wt".into(),
            title: Some("Ветка".into()),
        }
        .prepare(&resolver)
        .unwrap();
        assert_eq!(
            prepared,
            PreparedCommand::ProjectAdd {
                workspace,
                root: ResolvedRoot {
                    path: "/src/wt".into(),
                    kind: RootKind::Git,
                    key: RootKey::new("/src/main/.git"),
                },
                title: Some("Ветка".into()),
            }
        );
    }

    #[test]
    fn prepare_maps_root_errors_to_invalid_argument() {
        let resolver = FakeRootResolver::new()
            .with(
                "/missing",
                Err(RootError::NotFound {
                    path: "/missing".into(),
                }),
            )
            .with(
                "/file",
                Err(RootError::NotADirectory {
                    path: "/file".into(),
                }),
            );
        let mut ids = SequentialIds::new();
        let workspace = WorkspaceId::generate(&mut ids);
        for (path, reason) in [
            ("rel/dir", "path_not_absolute"),
            ("/missing", "path_not_found"),
            ("/file", "not_a_directory"),
        ] {
            let error = Command::ProjectAdd {
                workspace,
                path: path.into(),
                title: None,
            }
            .prepare(&resolver)
            .unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert_eq!(error.detail_str("reason"), Some(reason));
            assert_eq!(error.detail_str("path"), Some(path));
        }
    }

    #[test]
    fn prepare_keeps_other_commands_without_resolving() {
        struct Panicking;
        impl RootResolver for Panicking {
            fn resolve(&self, path: &Path) -> Result<ResolvedRoot, RootError> {
                panic!("разрешение корня {} не ожидалось", path.display())
            }
        }
        for command in all_commands().into_iter().filter(|c| !c.needs_io()) {
            command.prepare(&Panicking).unwrap();
        }
    }

    /// Имена команд, чтений и действий хоста в исходнике до его тестов: строки вида
    /// `<пространство>.<имя>`.
    fn host_names(source: &str) -> Vec<&str> {
        let code = source.split("#[cfg(test)]").next().unwrap_or(source);
        code.split('"')
            .filter(|piece| {
                piece.contains('.')
                    && piece.split('.').all(|segment| {
                        !segment.is_empty()
                            && segment
                                .chars()
                                .all(|ch| ch.is_ascii_lowercase() || ch == '_')
                    })
            })
            .collect()
    }

    #[test]
    fn host_names_stay_in_namespaces_closed_to_plugins() {
        let sources = [
            (include_str!("command.rs"), "panel.close"),
            (include_str!("query.rs"), "query.layout"),
            (include_str!("ui_command.rs"), "app.quit"),
            // Запросы `ui` и команды галереи объявляет crate управления.
            (
                include_str!("../../shellio-control/src/ui.rs"),
                "gallery.cell",
            ),
        ];
        for (source, known) in sources {
            let names = host_names(source);
            assert!(names.contains(&known), "{known}: {names:?}");
            for name in names {
                let namespace = name.split('.').next().unwrap();
                assert!(
                    shellio_plugin_api::HOST_NAMESPACES.contains(&namespace),
                    "пространства имён «{namespace}» команды «{name}» нет в HOST_NAMESPACES: \
                     плагин с таким id мог бы объявить действие с именем команды хоста"
                );
            }
        }
    }

    #[test]
    fn values_serialize_as_objects_of_ids() {
        let mut ids = SequentialIds::new();
        let ids: &mut dyn IdGen = &mut ids;
        let project = ProjectId::generate(ids);
        let root = ProjectRootId::generate(ids);
        let value = CommandValue::Project { project, root };
        let json = serde_json::to_value(value).unwrap();
        assert_eq!(json["project"], project.to_string());
        assert_eq!(json["root"], root.to_string());
        assert_eq!(serde_json::from_value::<CommandValue>(json).unwrap(), value);
        assert_eq!(
            serde_json::to_value(CommandValue::None).unwrap(),
            serde_json::Value::Null
        );
    }
}
