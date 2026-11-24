//! Чистые обработчики команд: по снимку состояния — ответ и список дельт.

use std::path::Path;

use shellio_layout::Viewport;
use shellio_plugin_api::Registry;

use crate::change::Change;
use crate::clock::Clock;
use crate::command::{CommandValue, PreparedCommand};
use crate::error::{CoreError, Entity};
use crate::ids::{IdGen, PanelId, ProjectId, ProjectRootId, WorkbenchId, WorkspaceId};
use crate::layout_cmd::layout_command;
use crate::model::{Project, ProjectRoot, Route, State, WindowState, Workbench, Workspace};
use crate::panel::{
    NAVIGATOR_PLUGIN, Panel, PluginId, TERMINAL_PLUGIN, WorkbenchLayout, recovered_layout,
};
use crate::plugin_host::created;
use crate::root::ResolvedRoot;
use crate::title::Title;

/// Имя воркспейса, который создаёт первый запуск.
pub const BOOTSTRAP_WORKSPACE: &str = "Основной";

/// Внедряемые источники времени и идентификаторов обработчика, реестр плагинов и размер окна.
pub(crate) struct Ctx<'a> {
    pub(crate) clock: &'a dyn Clock,
    pub(crate) ids: &'a mut dyn IdGen,
    pub(crate) registry: &'a Registry,
    /// Размер окна на момент команды. `None` — окна нет (ADR-0007, «Внутренний вход размера
    /// окна»).
    pub(crate) viewport: Option<Viewport>,
}

impl<'a> Ctx<'a> {
    /// Контекст из часов, генератора идентификаторов и реестра плагинов, без размера окна.
    pub(crate) fn new(
        clock: &'a dyn Clock,
        ids: &'a mut dyn IdGen,
        registry: &'a Registry,
    ) -> Self {
        Self {
            clock,
            ids,
            registry,
            viewport: None,
        }
    }

    /// Контекст с размером окна.
    pub(crate) const fn with_viewport(mut self, viewport: Option<Viewport>) -> Self {
        self.viewport = viewport;
        self
    }
}

/// Результат команды: значение ответа и дельты. Пустой список — состояние не меняется.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub(crate) value: CommandValue,
    pub(crate) changes: Vec<Change>,
}

impl Outcome {
    pub(crate) const fn new(value: CommandValue, changes: Vec<Change>) -> Self {
        Self { value, changes }
    }

    pub(crate) const fn unchanged() -> Self {
        Self::new(CommandValue::None, Vec::new())
    }
}

/// Обработать подготовленную команду над снимком состояния.
pub(crate) fn handle(
    state: &State,
    command: PreparedCommand,
    ctx: &mut Ctx<'_>,
) -> Result<Outcome, CoreError> {
    match command {
        PreparedCommand::WorkspaceCreate { title } => {
            workspace_create(state, parse_title(&title)?, ctx)
        }
        PreparedCommand::WorkspaceRename { workspace, title } => {
            workspace_rename(state, workspace, &title)
        }
        PreparedCommand::ProjectAdd {
            workspace,
            root,
            title,
        } => project_add(state, workspace, root, title.as_deref(), ctx),
        PreparedCommand::WorkbenchCreate { project, title } => {
            workbench_create(state, project, &title, ctx)
        }
        PreparedCommand::WorkbenchRename { workbench, title } => {
            workbench_rename(state, workbench, &title)
        }
        PreparedCommand::WorkbenchOpen { workbench } => workbench_open(state, workbench, ctx),
        PreparedCommand::WindowFocusWorkspace { workspace } => {
            let record = state
                .live_workspace(workspace)
                .ok_or_else(|| CoreError::not_found(Entity::Workspace, workspace))?;
            Ok(focus(state, record))
        }
        PreparedCommand::Layout(command) => layout_command(state, command, ctx),
    }
}

/// Внутренняя команда запуска ядра до приёма внешних. `None` — запись не нужна.
///
/// Нет ни одного неудалённого воркспейса — первый воркспейс [`BOOTSTRAP_WORKSPACE`] и окно
/// на его главной. Активный воркспейс пуст или удалён — фокус на первый неудалённый по
/// `(position, id)`. Маршрут ведёт в скрытую рабочую область — маршрут активного воркспейса,
/// как при фокусе на него. Каждая загруженная рабочая область без раскладки получает её
/// (ADR-0018, «Рабочие области без раскладки»). Всё это — одна запись.
pub(crate) fn startup(state: &State, ctx: &mut Ctx<'_>) -> Result<Option<Outcome>, CoreError> {
    let mut outcome = startup_window(state, ctx)?.unwrap_or_else(Outcome::unchanged);
    outcome.changes.extend(missing_layouts(state, ctx)?);
    Ok((!outcome.changes.is_empty()).then_some(outcome))
}

fn startup_window(state: &State, ctx: &mut Ctx<'_>) -> Result<Option<Outcome>, CoreError> {
    let Some(first) = state.workspaces_in_order().first().copied() else {
        let title = parse_title(BOOTSTRAP_WORKSPACE)?;
        return workspace_create(state, title, ctx).map(Some);
    };
    let Some(active) = live_active(state) else {
        return Ok(Some(focus(state, first)));
    };
    Ok(match state.window.route {
        Route::Workbench(workbench) if state.live_workbench(workbench).is_none() => {
            Some(window_set(active.id, workspace_route(state, active)))
        }
        Route::Workbench(_) | Route::WorkspaceHome => None,
    })
}

/// Раскладки рабочих областей без неё в порядке `WorkbenchId`: без панелей — раскладка по
/// умолчанию с новыми панелями, с панелями — восстановленная из них.
fn missing_layouts(state: &State, ctx: &mut Ctx<'_>) -> Result<Vec<Change>, CoreError> {
    let mut changes = Vec::new();
    for &workbench in state.workbenches.keys() {
        if state.layouts.contains_key(&workbench) {
            continue;
        }
        match recovered_layout(state.workbench_panels(workbench)) {
            Some(layout) => changes.push(Change::LayoutSet { workbench, layout }),
            None => changes.extend(default_layout(workbench, ctx)?),
        }
    }
    Ok(changes)
}

/// Раскладка по умолчанию (ADR-0009): новые панели `navigator` и `terminal` с начальными данными,
/// затем раскладка. Экземпляр не ищется: в рабочей области ещё ничего нет.
fn default_layout(workbench: WorkbenchId, ctx: &mut Ctx<'_>) -> Result<Vec<Change>, CoreError> {
    let plugin = |raw: &str| {
        PluginId::new(raw).map_err(|error| {
            CoreError::invalid_argument("plugin", "plugin_invalid", error.to_string())
        })
    };
    let navigator = new_panel(workbench, plugin(NAVIGATOR_PLUGIN)?, ctx)?;
    let terminal = new_panel(workbench, plugin(TERMINAL_PLUGIN)?, ctx)?;
    let layout = WorkbenchLayout::default_for(navigator.id, terminal.id);
    let mut changes = created(ctx.registry, navigator);
    changes.extend(created(ctx.registry, terminal));
    changes.push(Change::LayoutSet { workbench, layout });
    Ok(changes)
}

/// Новая панель плагина `plugin` с названием из его манифеста, у незарегистрированного плагина —
/// сам plugin id.
pub(crate) fn new_panel(
    workbench: WorkbenchId,
    plugin: PluginId,
    ctx: &mut Ctx<'_>,
) -> Result<Panel, CoreError> {
    let title = ctx
        .registry
        .get(&plugin)
        .map_or(plugin.as_str(), |registered| registered.manifest().title);
    Ok(Panel {
        id: PanelId::generate(ctx.ids),
        workbench,
        title: parse_title(title)?,
        plugin,
        created_at: ctx.clock.now(),
    })
}

/// Активный воркспейс, если он есть и не удалён.
fn live_active(state: &State) -> Option<&Workspace> {
    state
        .window
        .active_workspace
        .and_then(|active| state.live_workspace(active))
}

fn parse_title(raw: &str) -> Result<Title, CoreError> {
    Title::new(raw).map_err(|error| CoreError::invalid_title("title", &error))
}

/// Позиция после наибольшей. Пустой родитель — 0.
fn next_position(positions: impl Iterator<Item = u32>, scope: &str) -> Result<u32, CoreError> {
    match positions.max() {
        None => Ok(0),
        Some(max) => max
            .checked_add(1)
            .ok_or_else(|| CoreError::position_exhausted(scope)),
    }
}

fn workspace_create(state: &State, title: Title, ctx: &mut Ctx<'_>) -> Result<Outcome, CoreError> {
    if let Some(taken) = state
        .workspaces
        .values()
        .find(|workspace| !workspace.is_deleted() && workspace.title.same_name(&title))
    {
        return Err(CoreError::name_taken(Entity::Workspace, &title, taken.id));
    }
    let position = next_position(
        state
            .workspaces
            .values()
            .map(|workspace| workspace.position),
        "workspaces",
    )?;
    let id = WorkspaceId::generate(ctx.ids);
    let mut changes = vec![Change::WorkspaceInserted {
        workspace: Workspace {
            id,
            title,
            position,
            last_workbench: None,
            deleted_at: None,
        },
    }];
    if live_active(state).is_none() {
        changes.push(Change::WindowSet {
            active: id,
            route: Route::WorkspaceHome,
        });
    }
    Ok(Outcome::new(
        CommandValue::Workspace { workspace: id },
        changes,
    ))
}

fn workspace_rename(
    state: &State,
    workspace: WorkspaceId,
    raw: &str,
) -> Result<Outcome, CoreError> {
    let record = state
        .live_workspace(workspace)
        .ok_or_else(|| CoreError::not_found(Entity::Workspace, workspace))?;
    let title = parse_title(raw)?;
    if record.title == title {
        return Ok(Outcome::unchanged());
    }
    if let Some(taken) = state
        .workspaces
        .values()
        .find(|other| other.id != workspace && !other.is_deleted() && other.title.same_name(&title))
    {
        return Err(CoreError::name_taken(Entity::Workspace, &title, taken.id));
    }
    Ok(Outcome::new(
        CommandValue::None,
        vec![Change::WorkspaceRenamed { workspace, title }],
    ))
}

/// Имя проекта по умолчанию — последний компонент канонического пути корня.
fn default_project_title(path: &Path) -> Result<Title, CoreError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            CoreError::invalid_argument(
                "title",
                "title_required",
                format!(
                    "у пути {} нет имени в UTF-8: имя проекта нужно указать явно",
                    path.display()
                ),
            )
        })?;
    parse_title(name)
}

fn project_add(
    state: &State,
    workspace: WorkspaceId,
    root: ResolvedRoot,
    title: Option<&str>,
    ctx: &mut Ctx<'_>,
) -> Result<Outcome, CoreError> {
    if state.live_workspace(workspace).is_none() {
        return Err(CoreError::not_found(Entity::Workspace, workspace));
    }
    let title = match title {
        Some(raw) => parse_title(raw)?,
        None => default_project_title(&root.path)?,
    };
    // Занятый корень проверяется до имени: повторное добавление того же пути даёт и то же
    // имя по умолчанию, а причина отказа — корень.
    let owner = state
        .roots
        .values()
        .filter(|existing| existing.key == root.key)
        .find_map(|existing| state.projects.get(&existing.project));
    if let Some(owner) = owner {
        return Err(CoreError::root_taken(&root.key, owner.id, owner.workspace));
    }
    if let Some(taken) = state.projects.values().find(|project| {
        project.workspace == workspace && !project.is_removed() && project.title.same_name(&title)
    }) {
        return Err(CoreError::name_taken(Entity::Project, &title, taken.id));
    }
    let list_position = next_position(
        state
            .projects
            .values()
            .filter(|project| project.workspace == workspace)
            .filter_map(|project| project.list_position),
        "project_list",
    )?;
    let project = ProjectId::generate(ctx.ids);
    let root_id = ProjectRootId::generate(ctx.ids);
    let change = Change::ProjectInserted {
        project: Project {
            id: project,
            workspace,
            title,
            list_position: Some(list_position),
            removed_at: None,
            last_opened_at: None,
        },
        roots: vec![ProjectRoot {
            id: root_id,
            project,
            path: root.path,
            kind: root.kind,
            key: root.key,
        }],
    };
    Ok(Outcome::new(
        CommandValue::Project {
            project,
            root: root_id,
        },
        vec![change],
    ))
}

fn workbench_create(
    state: &State,
    project: ProjectId,
    raw: &str,
    ctx: &mut Ctx<'_>,
) -> Result<Outcome, CoreError> {
    if state.live_project(project).is_none() {
        return Err(CoreError::not_found(Entity::Project, project));
    }
    let title = parse_title(raw)?;
    let siblings = || {
        state
            .workbenches
            .values()
            .filter(move |workbench| workbench.project == project)
    };
    if let Some(taken) = siblings().find(|workbench| workbench.title.same_name(&title)) {
        return Err(CoreError::name_taken(Entity::Workbench, &title, taken.id));
    }
    let position = next_position(
        siblings().map(|workbench| workbench.position),
        "workbenches",
    )?;
    let id = WorkbenchId::generate(ctx.ids);
    let mut changes = vec![Change::WorkbenchInserted {
        workbench: Workbench {
            id,
            project,
            title,
            position,
        },
    }];
    changes.extend(default_layout(id, ctx)?);
    Ok(Outcome::new(
        CommandValue::Workbench { workbench: id },
        changes,
    ))
}

fn workbench_rename(
    state: &State,
    workbench: WorkbenchId,
    raw: &str,
) -> Result<Outcome, CoreError> {
    let record = state
        .live_workbench(workbench)
        .ok_or_else(|| CoreError::not_found(Entity::Workbench, workbench))?;
    let title = parse_title(raw)?;
    if record.title == title {
        return Ok(Outcome::unchanged());
    }
    if let Some(taken) = state.workbenches.values().find(|other| {
        other.id != workbench && other.project == record.project && other.title.same_name(&title)
    }) {
        return Err(CoreError::name_taken(Entity::Workbench, &title, taken.id));
    }
    Ok(Outcome::new(
        CommandValue::None,
        vec![Change::WorkbenchRenamed { workbench, title }],
    ))
}

fn workbench_open(
    state: &State,
    workbench: WorkbenchId,
    ctx: &mut Ctx<'_>,
) -> Result<Outcome, CoreError> {
    let not_found = || CoreError::not_found(Entity::Workbench, workbench);
    let record = state.live_workbench(workbench).ok_or_else(not_found)?;
    let project = state.projects.get(&record.project).ok_or_else(not_found)?;
    let workspace = state
        .workspaces
        .get(&project.workspace)
        .ok_or_else(not_found)?;

    let window = WindowState {
        active_workspace: Some(workspace.id),
        route: Route::Workbench(workbench),
    };
    let mut changes = Vec::new();
    if state.window != window {
        changes.push(Change::WindowSet {
            active: workspace.id,
            route: window.route,
        });
    }
    if workspace.last_workbench != Some(workbench) {
        changes.push(Change::WorkspaceLastWorkbenchSet {
            workspace: workspace.id,
            workbench: Some(workbench),
        });
    }
    // Уже открытая область — пустое изменение: время открытия без смены окна ревизию не даёт.
    if changes.is_empty() {
        return Ok(Outcome::unchanged());
    }
    let now = ctx.clock.now();
    if project.last_opened_at != Some(now) {
        changes.push(Change::ProjectOpened {
            project: project.id,
            at: now,
        });
    }
    Ok(Outcome::new(CommandValue::None, changes))
}

/// Фокус на воркспейс: маршрут — его последняя живая рабочая область или главная.
fn focus(state: &State, workspace: &Workspace) -> Outcome {
    if state.window.active_workspace == Some(workspace.id) {
        return Outcome::unchanged();
    }
    window_set(workspace.id, workspace_route(state, workspace))
}

/// Последняя живая рабочая область воркспейса или его главная.
fn workspace_route(state: &State, workspace: &Workspace) -> Route {
    workspace
        .last_workbench
        .filter(|&last| {
            state.live_workbench(last).is_some()
                && state.workbench_workspace(last) == Some(workspace.id)
        })
        .map_or(Route::WorkspaceHome, Route::Workbench)
}

fn window_set(active: WorkspaceId, route: Route) -> Outcome {
    Outcome::new(
        CommandValue::None,
        vec![Change::WindowSet { active, route }],
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::clock::UnixMillis;
    use crate::command::Command;
    use crate::error::ErrorCode;
    use crate::model::Violation;
    use crate::panel::{NAVIGATOR_TITLE, TERMINAL_TITLE};
    use crate::root::{RootError, RootKey, RootKind};
    use crate::test_support::{FakeRootResolver, ManualClock, SequentialIds, registry};

    const START: UnixMillis = UnixMillis(1_000);

    fn title(name: &str) -> Title {
        Title::new(name).unwrap()
    }

    struct Harness {
        state: State,
        ids: SequentialIds,
        clock: ManualClock,
        resolver: FakeRootResolver,
        registry: Arc<Registry>,
    }

    impl Harness {
        fn empty() -> Self {
            Self {
                state: State::default(),
                ids: SequentialIds::new(),
                clock: ManualClock::new(START),
                registry: registry(),
                resolver: FakeRootResolver::new()
                    .with_git("/src/shellio", "/src/shellio/.git")
                    .with_git("/src/shellio-wt", "/src/shellio/.git")
                    .with(
                        "/src/missing",
                        Err(RootError::NotFound {
                            path: "/src/missing".into(),
                        }),
                    )
                    .with(
                        "/src/file.txt",
                        Err(RootError::NotADirectory {
                            path: "/src/file.txt".into(),
                        }),
                    ),
            }
        }

        /// Состояние после первого запуска: воркспейс «Основной», активный.
        fn new() -> Self {
            let mut harness = Self::empty();
            let outcome = harness.startup().unwrap();
            harness.commit(&outcome);
            harness
        }

        fn startup(&mut self) -> Option<Outcome> {
            let mut ctx = Ctx::new(&self.clock, &mut self.ids, &self.registry);
            startup(&self.state, &mut ctx).unwrap()
        }

        /// Состояние базы v1: рабочие области без панелей и раскладок.
        fn strip_layouts(&mut self) {
            self.state.panels.clear();
            self.state.panel_data.clear();
            self.state.layouts.clear();
        }

        /// Применить дельты и проверить инварианты. Допустимы только совпадения позиций,
        /// заведённые тестом до команды: так выглядят загруженные записи.
        fn commit(&mut self, outcome: &Outcome) {
            let before = self.state.check_invariants();
            self.state.apply_all(&outcome.changes);
            let introduced: Vec<_> = self
                .state
                .check_invariants()
                .into_iter()
                .filter(|violation| {
                    !(matches!(violation, Violation::PositionTaken { .. })
                        && before.contains(violation))
                })
                .collect();
            assert_eq!(introduced, Vec::new());
        }

        fn run(&mut self, command: Command) -> Result<Outcome, CoreError> {
            let prepared = command.prepare(&self.resolver)?;
            let mut ctx = Ctx::new(&self.clock, &mut self.ids, &self.registry);
            let result = handle(&self.state, prepared, &mut ctx);
            if let Ok(outcome) = &result {
                self.commit(outcome);
            }
            result
        }

        fn ok(&mut self, command: Command) -> Outcome {
            self.run(command).unwrap()
        }

        fn err(&mut self, command: Command) -> CoreError {
            self.run(command).unwrap_err()
        }

        fn bootstrap_workspace(&self) -> WorkspaceId {
            self.state.workspaces_in_order()[0].id
        }

        fn create_workspace(&mut self, name: &str) -> WorkspaceId {
            match self
                .ok(Command::WorkspaceCreate { title: name.into() })
                .value
            {
                CommandValue::Workspace { workspace } => workspace,
                other => panic!("неожиданное значение {other:?}"),
            }
        }

        fn add_project(
            &mut self,
            workspace: WorkspaceId,
            path: &str,
        ) -> (ProjectId, ProjectRootId) {
            let outcome = self.ok(Command::ProjectAdd {
                workspace,
                path: path.into(),
                title: None,
            });
            match outcome.value {
                CommandValue::Project { project, root } => (project, root),
                other => panic!("неожиданное значение {other:?}"),
            }
        }

        fn create_workbench(&mut self, project: ProjectId, name: &str) -> WorkbenchId {
            match self
                .ok(Command::WorkbenchCreate {
                    project,
                    title: name.into(),
                })
                .value
            {
                CommandValue::Workbench { workbench } => workbench,
                other => panic!("неожиданное значение {other:?}"),
            }
        }
    }

    fn assert_error(error: &CoreError, code: ErrorCode, details: &[(&str, &str)]) {
        assert_eq!(error.code(), code, "{error}");
        for (key, value) in details {
            assert_eq!(error.detail_str(key), Some(*value), "{key} в {error}");
        }
    }

    // --- запуск ---

    #[test]
    fn startup_bootstraps_empty_state_in_one_record() {
        let mut h = Harness::empty();
        let outcome = h.startup().unwrap();
        let workspace = match outcome.value {
            CommandValue::Workspace { workspace } => workspace,
            other => panic!("неожиданное значение {other:?}"),
        };
        assert_eq!(
            outcome.changes,
            vec![
                Change::WorkspaceInserted {
                    workspace: Workspace {
                        id: workspace,
                        title: title(BOOTSTRAP_WORKSPACE),
                        position: 0,
                        last_workbench: None,
                        deleted_at: None,
                    },
                },
                Change::WindowSet {
                    active: workspace,
                    route: Route::WorkspaceHome,
                },
            ]
        );
        h.commit(&outcome);
        assert_eq!(h.state.window.active_workspace, Some(workspace));
        assert_eq!(h.startup(), None);
    }

    #[test]
    fn startup_focuses_first_live_workspace_by_position_then_id() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let work = h.create_workspace("Работа");
        let (app, _) = h.add_project(work, "/src/shellio");
        let review = h.create_workbench(app, "Ревью");
        h.ok(Command::WorkbenchOpen { workbench: review });
        assert_eq!(h.startup(), None);
        assert!(main < work);

        // Совпадение позиций: первым идёт меньший id.
        h.state.workspaces.get_mut(&work).unwrap().position = 0;
        h.state.window.active_workspace = None;
        h.state.window.route = Route::WorkspaceHome;
        let outcome = h.startup().unwrap();
        assert_eq!(
            outcome.changes,
            vec![Change::WindowSet {
                active: main,
                route: Route::WorkspaceHome,
            }]
        );
        h.commit(&outcome);
        assert_eq!(h.startup(), None);

        // Удалённый воркспейс пропускается, маршрут — последняя область фокусного.
        h.state.workspaces.get_mut(&main).unwrap().deleted_at = Some(UnixMillis(5));
        let outcome = h.startup().unwrap();
        assert_eq!(
            outcome.changes,
            vec![Change::WindowSet {
                active: work,
                route: Route::Workbench(review),
            }]
        );
        h.commit(&outcome);
        assert_eq!(h.startup(), None);
    }

    #[test]
    fn startup_moves_route_out_of_removed_project() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let (app, _) = h.add_project(main, "/src/shellio");
        let review = h.create_workbench(app, "Ревью");
        let (docs, _) = h.add_project(main, "/src/docs");
        let notes = h.create_workbench(docs, "Заметки");
        h.ok(Command::WorkbenchOpen { workbench: notes });
        h.ok(Command::WorkbenchOpen { workbench: review });
        assert_eq!(h.startup(), None);

        // Маршрут и последняя область — в убранном проекте: главная воркспейса.
        h.state.projects.get_mut(&app).unwrap().removed_at = Some(UnixMillis(5));
        let outcome = h.startup().unwrap();
        assert_eq!(
            outcome.changes,
            vec![Change::WindowSet {
                active: main,
                route: Route::WorkspaceHome,
            }]
        );

        // Последняя область жива: маршрут на неё, как при фокусе.
        h.state.workspaces.get_mut(&main).unwrap().last_workbench = Some(notes);
        let outcome = h.startup().unwrap();
        assert_eq!(
            outcome.changes,
            vec![Change::WindowSet {
                active: main,
                route: Route::Workbench(notes),
            }]
        );
        h.commit(&outcome);
        assert_eq!(h.startup(), None);
    }

    #[test]
    fn startup_bootstraps_when_every_workspace_is_deleted() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        h.state.workspaces.get_mut(&main).unwrap().deleted_at = Some(UnixMillis(5));
        let outcome = h.startup().unwrap();
        let [
            Change::WorkspaceInserted { workspace },
            Change::WindowSet { active, route },
        ] = outcome.changes.as_slice()
        else {
            panic!("неожиданные дельты {:?}", outcome.changes);
        };
        assert_eq!(workspace.title, title(BOOTSTRAP_WORKSPACE));
        assert_eq!(workspace.position, 1);
        assert_eq!(*active, workspace.id);
        assert_eq!(*route, Route::WorkspaceHome);
        h.commit(&outcome);
        assert_eq!(h.startup(), None);
    }

    fn default_layout_changes(
        workbench: WorkbenchId,
        navigator: PanelId,
        terminal: PanelId,
    ) -> Vec<Change> {
        let panel = |id, plugin: &str, name: &str| Panel {
            id,
            workbench,
            plugin: PluginId::new(plugin).unwrap(),
            title: title(name),
            created_at: START,
        };
        // Начальные данные заполнителя — `null` версии 1, той же записью, что панель.
        let init = |panel| Change::PanelData {
            panel,
            version: 1,
            data: "null".to_owned(),
        };
        vec![
            Change::PanelAdd {
                panel: panel(navigator, NAVIGATOR_PLUGIN, NAVIGATOR_TITLE),
            },
            init(navigator),
            Change::PanelAdd {
                panel: panel(terminal, TERMINAL_PLUGIN, TERMINAL_TITLE),
            },
            init(terminal),
            Change::LayoutSet {
                workbench,
                layout: WorkbenchLayout::default_for(navigator, terminal),
            },
        ]
    }

    /// Идентификаторы панелей из дельт `PanelAdd` по порядку.
    fn added_panels(changes: &[Change]) -> Vec<PanelId> {
        changes
            .iter()
            .filter_map(|change| match change {
                Change::PanelAdd { panel } => Some(panel.id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn startup_gives_default_layout_to_each_workbench_in_one_record() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let (app, _) = h.add_project(main, "/src/shellio");
        let review = h.create_workbench(app, "Ревью");
        let notes = h.create_workbench(app, "Заметки");
        assert!(review < notes);
        h.strip_layouts();

        let outcome = h.startup().unwrap();
        let ids = added_panels(&outcome.changes);
        assert!(
            ids.is_sorted(),
            "панели выдаются по порядку рабочих областей"
        );
        let mut expected = default_layout_changes(review, ids[0], ids[1]);
        expected.extend(default_layout_changes(notes, ids[2], ids[3]));
        assert_eq!(outcome.changes, expected);
        assert_eq!(outcome.value, CommandValue::None);
        h.commit(&outcome);
        assert_eq!(h.state.layouts.len(), 2);
        assert_eq!(h.state.panels.len(), 4);

        // Второй запуск ничего не пишет, панели прежние.
        let panels = h.state.panels.clone();
        assert_eq!(h.startup(), None);
        assert_eq!(h.state.panels, panels);
    }

    #[test]
    fn startup_writes_window_fix_and_layouts_in_one_record() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let (app, _) = h.add_project(main, "/src/shellio");
        let review = h.create_workbench(app, "Ревью");
        h.strip_layouts();
        h.state.window.active_workspace = None;

        let outcome = h.startup().unwrap();
        let [navigator, terminal] = added_panels(&outcome.changes)[..] else {
            panic!("неожиданные дельты {:?}", outcome.changes);
        };
        let mut expected = vec![Change::WindowSet {
            active: main,
            route: Route::WorkspaceHome,
        }];
        expected.extend(default_layout_changes(review, navigator, terminal));
        assert_eq!(outcome.changes, expected);
        h.commit(&outcome);
        assert_eq!(h.startup(), None);
    }

    #[test]
    fn startup_gives_layout_to_workbench_of_hidden_project() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let (app, _) = h.add_project(main, "/src/shellio");
        let review = h.create_workbench(app, "Ревью");
        h.strip_layouts();
        h.state.projects.get_mut(&app).unwrap().removed_at = Some(UnixMillis(5));
        let outcome = h.startup().unwrap();
        assert!(matches!(
            outcome.changes.last(),
            Some(Change::LayoutSet { workbench, .. }) if *workbench == review
        ));
        h.commit(&outcome);
        assert_eq!(h.startup(), None);
    }

    #[test]
    fn startup_recovers_layout_from_live_panels_without_new_ones() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let (app, _) = h.add_project(main, "/src/shellio");
        let review = h.create_workbench(app, "Ревью");
        let layout = h.state.layouts.remove(&review).unwrap();
        let terminal = layout.focused().unwrap();
        let navigator = layout.navigator.unwrap();
        let other = PanelId::generate(&mut h.ids);
        h.state.panels.insert(
            other,
            Panel {
                id: other,
                workbench: review,
                plugin: PluginId::new("notes").unwrap(),
                title: title("Заметки"),
                created_at: UnixMillis(START.0 - 1),
            },
        );

        let outcome = h.startup().unwrap();
        let [Change::LayoutSet { workbench, layout }] = outcome.changes.as_slice() else {
            panic!("неожиданные дельты {:?}", outcome.changes);
        };
        assert_eq!(*workbench, review);
        assert_eq!(layout.navigator, Some(navigator));
        assert_eq!(layout.zones.center.parts[0].tabs, [other, terminal]);
        assert_eq!(layout.focus, [other]);
        h.commit(&outcome);
        assert_eq!(h.startup(), None);
    }

    // --- успех команд ---

    #[test]
    fn workspace_create_appends_workspace() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let outcome = h.ok(Command::WorkspaceCreate {
            title: "  Работа ".into(),
        });
        let CommandValue::Workspace { workspace } = outcome.value else {
            panic!("неожиданное значение {:?}", outcome.value);
        };
        assert_eq!(
            outcome.changes,
            vec![Change::WorkspaceInserted {
                workspace: Workspace {
                    id: workspace,
                    title: title("Работа"),
                    position: 1,
                    last_workbench: None,
                    deleted_at: None,
                },
            }]
        );
        assert_eq!(h.state.workspaces[&workspace].title.as_str(), "Работа");
        assert_eq!(h.state.window.active_workspace, Some(main));
        let order: Vec<_> = h.state.workspaces_in_order().iter().map(|w| w.id).collect();
        assert_eq!(order, vec![main, workspace]);
    }

    #[test]
    fn workspace_create_activates_first_workspace_of_empty_state() {
        let mut h = Harness::empty();
        let outcome = h.ok(Command::WorkspaceCreate {
            title: "Работа".into(),
        });
        let CommandValue::Workspace { workspace } = outcome.value else {
            panic!("неожиданное значение {:?}", outcome.value);
        };
        assert_eq!(outcome.changes.len(), 2);
        assert_eq!(
            outcome.changes[1],
            Change::WindowSet {
                active: workspace,
                route: Route::WorkspaceHome,
            }
        );
        assert_eq!(h.state.window.active_workspace, Some(workspace));
    }

    #[test]
    fn workspace_rename_changes_title() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let outcome = h.ok(Command::WorkspaceRename {
            workspace: main,
            title: "Работа".into(),
        });
        assert_eq!(outcome.value, CommandValue::None);
        assert_eq!(
            outcome.changes,
            vec![Change::WorkspaceRenamed {
                workspace: main,
                title: title("Работа"),
            }]
        );
        assert_eq!(h.state.workspaces[&main].title, title("Работа"));

        // Смена только регистра — настоящее изменение.
        let outcome = h.ok(Command::WorkspaceRename {
            workspace: main,
            title: "РАБОТА".into(),
        });
        assert_eq!(outcome.changes.len(), 1);
        assert_eq!(h.state.workspaces[&main].title, title("РАБОТА"));
    }

    #[test]
    fn project_add_appends_project_with_one_root() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let outcome = h.ok(Command::ProjectAdd {
            workspace: main,
            path: "/src/shellio".into(),
            title: None,
        });
        let CommandValue::Project { project, root } = outcome.value else {
            panic!("неожиданное значение {:?}", outcome.value);
        };
        assert_eq!(
            outcome.changes,
            vec![Change::ProjectInserted {
                project: Project {
                    id: project,
                    workspace: main,
                    title: title("shellio"),
                    list_position: Some(0),
                    removed_at: None,
                    last_opened_at: None,
                },
                roots: vec![ProjectRoot {
                    id: root,
                    project,
                    path: PathBuf::from("/src/shellio"),
                    kind: RootKind::Git,
                    key: RootKey::new("/src/shellio/.git"),
                }],
            }]
        );
        assert_eq!(h.state.roots[&root].project, project);

        let outcome = h.ok(Command::ProjectAdd {
            workspace: main,
            path: "/notes".into(),
            title: Some(" Заметки ".into()),
        });
        let CommandValue::Project { project, root } = outcome.value else {
            panic!("неожиданное значение {:?}", outcome.value);
        };
        let record = &h.state.projects[&project];
        assert_eq!(record.title, title("Заметки"));
        assert_eq!(record.list_position, Some(1));
        assert_eq!(h.state.roots[&root].kind, RootKind::Folder);
        assert_eq!(h.state.roots[&root].key, RootKey::new("/notes"));
    }

    #[test]
    fn workbench_create_appends_workbench() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let (app, _) = h.add_project(main, "/src/shellio");
        let first = h.create_workbench(app, "Основная");
        let outcome = h.ok(Command::WorkbenchCreate {
            project: app,
            title: "Ревью".into(),
        });
        let CommandValue::Workbench { workbench } = outcome.value else {
            panic!("неожиданное значение {:?}", outcome.value);
        };
        let [navigator, terminal] = added_panels(&outcome.changes)[..] else {
            panic!("неожиданные дельты {:?}", outcome.changes);
        };
        assert!(navigator < terminal);
        let mut expected = vec![Change::WorkbenchInserted {
            workbench: Workbench {
                id: workbench,
                project: app,
                title: title("Ревью"),
                position: 1,
            },
        }];
        expected.extend(default_layout_changes(workbench, navigator, terminal));
        assert_eq!(outcome.changes, expected);
        assert_eq!(h.state.layouts[&workbench].focused(), Some(terminal));
        assert_eq!(h.startup(), None, "запуск раскладку не дописывает");
        let order: Vec<_> = h
            .state
            .workbenches_in_order(app)
            .iter()
            .map(|w| w.id)
            .collect();
        assert_eq!(order, vec![first, workbench]);
    }

    #[test]
    fn workbench_rename_changes_title() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let (app, _) = h.add_project(main, "/src/shellio");
        let workbench = h.create_workbench(app, "Основная");
        let outcome = h.ok(Command::WorkbenchRename {
            workbench,
            title: "Ревью".into(),
        });
        assert_eq!(outcome.value, CommandValue::None);
        assert_eq!(
            outcome.changes,
            vec![Change::WorkbenchRenamed {
                workbench,
                title: title("Ревью"),
            }]
        );
        assert_eq!(h.state.workbenches[&workbench].title, title("Ревью"));

        // Смена только регистра — настоящее изменение.
        let outcome = h.ok(Command::WorkbenchRename {
            workbench,
            title: "РЕВЬЮ".into(),
        });
        assert_eq!(outcome.changes.len(), 1);
        assert_eq!(h.state.workbenches[&workbench].title, title("РЕВЬЮ"));
    }

    #[test]
    fn workbench_open_sets_route_last_workbench_and_open_time() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let (app, _) = h.add_project(main, "/src/shellio");
        let first = h.create_workbench(app, "Основная");
        let second = h.create_workbench(app, "Ревью");

        let outcome = h.ok(Command::WorkbenchOpen { workbench: first });
        assert_eq!(outcome.value, CommandValue::None);
        assert_eq!(
            outcome.changes,
            vec![
                Change::WindowSet {
                    active: main,
                    route: Route::Workbench(first),
                },
                Change::WorkspaceLastWorkbenchSet {
                    workspace: main,
                    workbench: Some(first),
                },
                Change::ProjectOpened {
                    project: app,
                    at: START,
                },
            ]
        );
        assert_eq!(h.state.window.route, Route::Workbench(first));
        assert_eq!(h.state.workspaces[&main].last_workbench, Some(first));
        assert_eq!(h.state.projects[&app].last_opened_at, Some(START));

        h.clock.advance(10);
        h.ok(Command::WorkbenchOpen { workbench: second });
        assert_eq!(h.state.window.route, Route::Workbench(second));
        assert_eq!(h.state.workspaces[&main].last_workbench, Some(second));
        assert_eq!(
            h.state.projects[&app].last_opened_at,
            Some(UnixMillis(START.0 + 10))
        );
    }

    #[test]
    fn workbench_open_of_other_workspace_switches_workspace_and_route() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let work = h.create_workspace("Работа");
        let (app, _) = h.add_project(work, "/src/shellio");
        let workbench = h.create_workbench(app, "Основная");
        assert_eq!(h.state.window.active_workspace, Some(main));

        let outcome = h.ok(Command::WorkbenchOpen { workbench });
        assert_eq!(
            outcome.changes[0],
            Change::WindowSet {
                active: work,
                route: Route::Workbench(workbench),
            }
        );
        assert_eq!(
            h.state.window,
            WindowState {
                active_workspace: Some(work),
                route: Route::Workbench(workbench),
            }
        );
        assert_eq!(h.state.workspaces[&work].last_workbench, Some(workbench));
        assert_eq!(h.state.workspaces[&main].last_workbench, None);
    }

    #[test]
    fn focus_workspace_routes_to_last_workbench_or_home() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let work = h.create_workspace("Работа");
        let (app, _) = h.add_project(work, "/src/shellio");
        let workbench = h.create_workbench(app, "Основная");

        let outcome = h.ok(Command::WindowFocusWorkspace { workspace: work });
        assert_eq!(outcome.value, CommandValue::None);
        assert_eq!(
            outcome.changes,
            vec![Change::WindowSet {
                active: work,
                route: Route::WorkspaceHome,
            }]
        );

        h.ok(Command::WorkbenchOpen { workbench });
        let outcome = h.ok(Command::WindowFocusWorkspace { workspace: main });
        assert_eq!(
            outcome.changes,
            vec![Change::WindowSet {
                active: main,
                route: Route::WorkspaceHome,
            }]
        );
        let outcome = h.ok(Command::WindowFocusWorkspace { workspace: work });
        assert_eq!(
            outcome.changes,
            vec![Change::WindowSet {
                active: work,
                route: Route::Workbench(workbench),
            }]
        );
    }

    #[test]
    fn focus_workspace_skips_last_workbench_of_removed_project() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let work = h.create_workspace("Работа");
        let (app, _) = h.add_project(main, "/src/shellio");
        let review = h.create_workbench(app, "Ревью");
        h.ok(Command::WorkbenchOpen { workbench: review });
        h.ok(Command::WindowFocusWorkspace { workspace: work });

        h.state.projects.get_mut(&app).unwrap().removed_at = Some(UnixMillis(5));
        let outcome = h.ok(Command::WindowFocusWorkspace { workspace: main });
        assert_eq!(
            outcome.changes,
            vec![Change::WindowSet {
                active: main,
                route: Route::WorkspaceHome,
            }]
        );
        assert_eq!(h.state.workspaces[&main].last_workbench, Some(review));
    }

    // --- пустые изменения ---

    #[test]
    fn commands_that_change_nothing_give_no_changes() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let (app, _) = h.add_project(main, "/src/shellio");
        let workbench = h.create_workbench(app, "Основная");
        let before = h.state.clone();

        let unchanged = [
            Command::WorkspaceRename {
                workspace: main,
                title: BOOTSTRAP_WORKSPACE.into(),
            },
            Command::WorkspaceRename {
                workspace: main,
                title: format!("  {BOOTSTRAP_WORKSPACE}\t"),
            },
            Command::WorkbenchRename {
                workbench,
                title: "Основная".into(),
            },
            Command::WindowFocusWorkspace { workspace: main },
        ];
        for command in unchanged {
            let name = command.name();
            assert_eq!(h.ok(command), Outcome::unchanged(), "{name}");
        }
        assert_eq!(h.state, before);

        h.ok(Command::WorkbenchOpen { workbench });
        let opened = h.state.clone();
        h.clock.advance(1_000);
        assert_eq!(
            h.ok(Command::WorkbenchOpen { workbench }),
            Outcome::unchanged()
        );
        assert_eq!(h.state, opened);
    }

    // --- отказы ---

    #[test]
    fn workspace_create_rejects_bad_and_taken_names() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let work = h.create_workspace("Work");
        let main_id = main.to_string();
        let work_id = work.to_string();

        type Case<'a> = (String, ErrorCode, Vec<(&'a str, &'a str)>);
        let cases: [Case<'_>; 6] = [
            (
                "   ".into(),
                ErrorCode::InvalidArgument,
                vec![("field", "title"), ("reason", "title_empty")],
            ),
            (
                "я".repeat(201),
                ErrorCode::InvalidArgument,
                vec![("reason", "title_too_long")],
            ),
            (
                "a\u{1b}b".into(),
                ErrorCode::InvalidArgument,
                vec![("reason", "title_control_char")],
            ),
            (
                "основной".into(),
                ErrorCode::NameTaken,
                vec![
                    ("entity", "workspace"),
                    ("name", "основной"),
                    ("existing", main_id.as_str()),
                ],
            ),
            (
                " ОСНОВНОЙ ".into(),
                ErrorCode::NameTaken,
                vec![("existing", main_id.as_str())],
            ),
            (
                "WORK".into(),
                ErrorCode::NameTaken,
                vec![("existing", work_id.as_str())],
            ),
        ];
        for (raw, code, details) in cases {
            let error = h.err(Command::WorkspaceCreate { title: raw });
            assert_error(&error, code, &details);
        }

        // 200 символов проходят, имя удалённого воркспейса свободно.
        h.create_workspace(&"я".repeat(200));
        h.state.workspaces.get_mut(&work).unwrap().deleted_at = Some(UnixMillis(3));
        h.create_workspace("work");
    }

    #[test]
    fn workspace_rename_rejects_missing_bad_and_taken() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let work = h.create_workspace("Работа");
        let gone = h.create_workspace("Старое");
        h.state.workspaces.get_mut(&gone).unwrap().deleted_at = Some(UnixMillis(3));
        let ghost = WorkspaceId::generate(&mut h.ids);

        for workspace in [ghost, gone] {
            let error = h.err(Command::WorkspaceRename {
                workspace,
                title: "Новое".into(),
            });
            let id = workspace.to_string();
            assert_error(
                &error,
                ErrorCode::NotFound,
                &[("entity", "workspace"), ("id", id.as_str())],
            );
        }
        let error = h.err(Command::WorkspaceRename {
            workspace: work,
            title: String::new(),
        });
        assert_error(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "title_empty")],
        );
        let error = h.err(Command::WorkspaceRename {
            workspace: work,
            title: "оСНОВНОЙ".into(),
        });
        let main_id = main.to_string();
        assert_error(
            &error,
            ErrorCode::NameTaken,
            &[("existing", main_id.as_str())],
        );
        // Имя удалённого воркспейса свободно.
        h.ok(Command::WorkspaceRename {
            workspace: work,
            title: "старое".into(),
        });
    }

    #[test]
    fn project_add_rejects_missing_workspace_bad_names_and_paths() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let gone = h.create_workspace("Старое");
        h.state.workspaces.get_mut(&gone).unwrap().deleted_at = Some(UnixMillis(3));
        let ghost = WorkspaceId::generate(&mut h.ids);

        for workspace in [ghost, gone] {
            let error = h.err(Command::ProjectAdd {
                workspace,
                path: "/notes".into(),
                title: None,
            });
            let id = workspace.to_string();
            assert_error(
                &error,
                ErrorCode::NotFound,
                &[("entity", "workspace"), ("id", id.as_str())],
            );
        }

        let error = h.err(Command::ProjectAdd {
            workspace: main,
            path: "/notes".into(),
            title: Some("\t".into()),
        });
        assert_error(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "title_empty")],
        );
        let error = h.err(Command::ProjectAdd {
            workspace: main,
            path: "/".into(),
            title: None,
        });
        assert_error(
            &error,
            ErrorCode::InvalidArgument,
            &[("field", "title"), ("reason", "title_required")],
        );
        let long = format!("/{}", "x".repeat(201));
        let error = h.err(Command::ProjectAdd {
            workspace: main,
            path: long.clone().into(),
            title: None,
        });
        assert_error(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "title_too_long")],
        );
        // Явное имя снимает ограничение имени по умолчанию.
        h.ok(Command::ProjectAdd {
            workspace: main,
            path: long.into(),
            title: Some("Длинный".into()),
        });

        for (path, reason) in [
            ("src/shellio", "path_not_absolute"),
            ("/src/missing", "path_not_found"),
            ("/src/file.txt", "not_a_directory"),
        ] {
            let error = h.err(Command::ProjectAdd {
                workspace: main,
                path: path.into(),
                title: None,
            });
            assert_error(
                &error,
                ErrorCode::InvalidArgument,
                &[("field", "path"), ("reason", reason), ("path", path)],
            );
        }
    }

    #[test]
    fn project_add_rejects_taken_name_ignoring_case() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let work = h.create_workspace("Работа");
        let (app, _) = h.add_project(main, "/src/shellio");
        let app_id = app.to_string();

        let error = h.err(Command::ProjectAdd {
            workspace: main,
            path: "/other/Shellio".into(),
            title: None,
        });
        assert_error(
            &error,
            ErrorCode::NameTaken,
            &[
                ("entity", "project"),
                ("name", "Shellio"),
                ("existing", app_id.as_str()),
            ],
        );
        // Имена уникальны только внутри воркспейса.
        h.add_project(work, "/other/Shellio");
        // Имя убранного проекта свободно.
        h.state.projects.get_mut(&app).unwrap().removed_at = Some(UnixMillis(3));
        h.ok(Command::ProjectAdd {
            workspace: main,
            path: "/third".into(),
            title: Some("SHELLIO".into()),
        });
    }

    #[test]
    fn project_add_rejects_taken_root_naming_its_owner() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let work = h.create_workspace("Работа");
        let (app, _) = h.add_project(work, "/src/shellio");
        let expected = [
            ("project", app.to_string()),
            ("workspace", work.to_string()),
        ];
        let expected: Vec<_> = expected.iter().map(|(k, v)| (*k, v.as_str())).collect();

        // Тот же корень в другом воркспейсе, его worktree и тот же путь с другим именем.
        for (workspace, path, title) in [
            (main, "/src/shellio", None),
            (main, "/src/shellio-wt", None),
            (work, "/src/shellio", None),
            (work, "/src/shellio-wt", Some("Ветка")),
        ] {
            let error = h.err(Command::ProjectAdd {
                workspace,
                path: path.into(),
                title: title.map(str::to_owned),
            });
            assert_error(&error, ErrorCode::RootTaken, &expected);
        }

        // Убранный проект держит свой корень.
        h.state.projects.get_mut(&app).unwrap().removed_at = Some(UnixMillis(3));
        let error = h.err(Command::ProjectAdd {
            workspace: main,
            path: "/src/shellio".into(),
            title: None,
        });
        assert_error(&error, ErrorCode::RootTaken, &expected);
    }

    #[test]
    fn workbench_create_rejects_missing_project_bad_and_taken_names() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let work = h.create_workspace("Работа");
        let (app, _) = h.add_project(main, "/src/shellio");
        let (removed, _) = h.add_project(main, "/old");
        let (orphaned, _) = h.add_project(work, "/work");
        h.state.projects.get_mut(&removed).unwrap().removed_at = Some(UnixMillis(3));
        h.state.workspaces.get_mut(&work).unwrap().deleted_at = Some(UnixMillis(3));
        let ghost = ProjectId::generate(&mut h.ids);

        for project in [ghost, removed, orphaned] {
            let error = h.err(Command::WorkbenchCreate {
                project,
                title: "Основная".into(),
            });
            let id = project.to_string();
            assert_error(
                &error,
                ErrorCode::NotFound,
                &[("entity", "project"), ("id", id.as_str())],
            );
        }

        let error = h.err(Command::WorkbenchCreate {
            project: app,
            title: "\u{0}".into(),
        });
        assert_error(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "title_control_char")],
        );

        let first = h.create_workbench(app, "Основная");
        let error = h.err(Command::WorkbenchCreate {
            project: app,
            title: "ОСНОВНАЯ".into(),
        });
        let first_id = first.to_string();
        assert_error(
            &error,
            ErrorCode::NameTaken,
            &[("entity", "workbench"), ("existing", first_id.as_str())],
        );
    }

    #[test]
    fn workbench_rename_rejects_missing_bad_and_taken() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let (app, _) = h.add_project(main, "/src/shellio");
        let (other, _) = h.add_project(main, "/notes");
        let first = h.create_workbench(app, "Основная");
        let second = h.create_workbench(app, "Ревью");
        let foreign = h.create_workbench(other, "Черновик");
        h.state.projects.get_mut(&other).unwrap().removed_at = Some(UnixMillis(3));
        let ghost = WorkbenchId::generate(&mut h.ids);

        for workbench in [ghost, foreign] {
            let error = h.err(Command::WorkbenchRename {
                workbench,
                title: "Новая".into(),
            });
            let id = workbench.to_string();
            assert_error(
                &error,
                ErrorCode::NotFound,
                &[("entity", "workbench"), ("id", id.as_str())],
            );
        }
        let error = h.err(Command::WorkbenchRename {
            workbench: second,
            title: " ".into(),
        });
        assert_error(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "title_empty")],
        );
        let error = h.err(Command::WorkbenchRename {
            workbench: second,
            title: "основная".into(),
        });
        let first_id = first.to_string();
        assert_error(
            &error,
            ErrorCode::NameTaken,
            &[("existing", first_id.as_str())],
        );
        // Имена уникальны только внутри проекта.
        h.ok(Command::WorkbenchRename {
            workbench: second,
            title: "Черновик".into(),
        });
    }

    #[test]
    fn workbench_open_rejects_missing_and_hidden_workbench() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let (app, _) = h.add_project(main, "/src/shellio");
        let workbench = h.create_workbench(app, "Основная");
        h.state.projects.get_mut(&app).unwrap().removed_at = Some(UnixMillis(3));
        let ghost = WorkbenchId::generate(&mut h.ids);

        for workbench in [ghost, workbench] {
            let error = h.err(Command::WorkbenchOpen { workbench });
            let id = workbench.to_string();
            assert_error(
                &error,
                ErrorCode::NotFound,
                &[("entity", "workbench"), ("id", id.as_str())],
            );
        }
    }

    #[test]
    fn focus_workspace_rejects_missing_and_deleted() {
        let mut h = Harness::new();
        let gone = h.create_workspace("Старое");
        h.state.workspaces.get_mut(&gone).unwrap().deleted_at = Some(UnixMillis(3));
        let ghost = WorkspaceId::generate(&mut h.ids);

        for workspace in [ghost, gone] {
            let error = h.err(Command::WindowFocusWorkspace { workspace });
            let id = workspace.to_string();
            assert_error(
                &error,
                ErrorCode::NotFound,
                &[("entity", "workspace"), ("id", id.as_str())],
            );
        }
    }

    // --- позиции ---

    #[test]
    fn new_records_follow_highest_position_after_gap() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let old = h.create_workspace("Старое");
        let (app, _) = h.add_project(main, "/src/shellio");
        let (gone, _) = h.add_project(main, "/old");
        let workbench = h.create_workbench(app, "Основная");

        // Пропуски позиций, наибольшая — у удалённой и убранной записей.
        let record = h.state.workspaces.get_mut(&old).unwrap();
        record.position = 5;
        record.deleted_at = Some(UnixMillis(3));
        let record = h.state.projects.get_mut(&gone).unwrap();
        record.list_position = Some(4);
        record.removed_at = Some(UnixMillis(3));
        h.state.workbenches.get_mut(&workbench).unwrap().position = 7;

        let workspace = h.create_workspace("Новое");
        assert_eq!(h.state.workspaces[&workspace].position, 6);
        let (project, _) = h.add_project(main, "/notes");
        assert_eq!(h.state.projects[&project].list_position, Some(5));
        let next = h.create_workbench(app, "Ревью");
        assert_eq!(h.state.workbenches[&next].position, 8);

        // Проект вне списка позиции списка не занимает.
        h.state.projects.get_mut(&project).unwrap().list_position = None;
        h.state.projects.get_mut(&gone).unwrap().list_position = None;
        let (last, _) = h.add_project(main, "/last");
        assert_eq!(h.state.projects[&last].list_position, Some(1));
    }

    #[test]
    fn equal_positions_are_ordered_by_id() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        let second = h.create_workspace("Второе");
        let third = h.create_workspace("Третье");
        let (app, _) = h.add_project(main, "/src/shellio");
        let first_wb = h.create_workbench(app, "Основная");
        let second_wb = h.create_workbench(app, "Ревью");

        h.state.workspaces.get_mut(&main).unwrap().position = 4;
        h.state.workspaces.get_mut(&second).unwrap().position = 4;
        h.state.workspaces.get_mut(&third).unwrap().position = 2;
        let order: Vec<_> = h.state.workspaces_in_order().iter().map(|w| w.id).collect();
        assert_eq!(order, vec![third, main, second]);

        h.state.workbenches.get_mut(&first_wb).unwrap().position = 3;
        h.state.workbenches.get_mut(&second_wb).unwrap().position = 3;
        let order: Vec<_> = h
            .state
            .workbenches_in_order(app)
            .iter()
            .map(|w| w.id)
            .collect();
        assert_eq!(order, vec![first_wb, second_wb]);

        // Совпадение не мешает новой записи встать после наибольшей.
        let next = h.create_workbench(app, "Третья");
        assert_eq!(h.state.workbenches[&next].position, 4);
    }

    #[test]
    fn exhausted_position_is_rejected() {
        let mut h = Harness::new();
        let main = h.bootstrap_workspace();
        h.state.workspaces.get_mut(&main).unwrap().position = u32::MAX;
        let error = h.err(Command::WorkspaceCreate {
            title: "Работа".into(),
        });
        assert_error(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "position_exhausted"), ("scope", "workspaces")],
        );

        let (app, _) = h.add_project(main, "/src/shellio");
        h.state.projects.get_mut(&app).unwrap().list_position = Some(u32::MAX);
        let error = h.err(Command::ProjectAdd {
            workspace: main,
            path: "/src/docs".into(),
            title: None,
        });
        assert_error(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "position_exhausted"), ("scope", "project_list")],
        );

        let workbench = h.create_workbench(app, "Основная");
        h.state.workbenches.get_mut(&workbench).unwrap().position = u32::MAX;
        let error = h.err(Command::WorkbenchCreate {
            project: app,
            title: "Ревью".into(),
        });
        assert_error(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "position_exhausted"), ("scope", "workbenches")],
        );
    }
}
