//! Область команд и дерева: проверка в акторе на одном снимке с обработчиком.
//!
//! Цель вне области отвергается `not_found`, как несуществующая запись: без записи, без новой
//! ревизии и без события.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use shellio_core::test_support::{
    FakeRootResolver, Faults, FaultyPersist, ManualClock, MemoryPersist, SequentialIds, TestPlugin,
    registry, registry_of,
};
use shellio_core::{Change, OpenOutcome};
use shellio_core::{
    Command, CommandValue, Core, CoreError, CoreHandle, Entity, ErrorCode, EventsSince, IdGen,
    PanelId, Project, ProjectId, ProjectRoot, ProjectRootId, Query, ReplyValue, ResolvedRoot,
    Revision, RootError, RootKey, RootKind, RootResolver, Route, Snapshot, State, Title, TreeScope,
    TreeView, UnixMillis, WindowState, Workbench, WorkbenchId, Workspace, WorkspaceId,
};
use shellio_layout::{Viewport, Zone};
use shellio_plugin_api::{Cardinality, Registry};
use tokio::runtime::Handle;

type TestResult<T> = Result<T, Box<dyn Error>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);

/// Разрешение корней со счётчиком вызовов: показывает, дошла ли команда до подготовки.
struct CountingResolver {
    inner: FakeRootResolver,
    calls: Arc<AtomicUsize>,
}

impl RootResolver for CountingResolver {
    fn resolve(&self, path: &Path) -> Result<ResolvedRoot, RootError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.resolve(path)
    }
}

fn start_with(memory: &MemoryPersist, resolver: FakeRootResolver) -> TestResult<Started> {
    start_in(memory, resolver, registry())
}

fn start_in(
    memory: &MemoryPersist,
    resolver: FakeRootResolver,
    registry: Arc<Registry>,
) -> TestResult<Started> {
    let (persist, faults) = FaultyPersist::new(memory.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let core = Core::start(
        &Handle::current(),
        Box::new(persist),
        Box::new(ManualClock::new(NOW)),
        Box::new(SequentialIds::new()),
        Arc::new(CountingResolver {
            inner: resolver,
            calls: Arc::clone(&calls),
        }),
        registry,
    )?;
    Ok(Started {
        core,
        faults,
        resolutions: calls,
    })
}

struct Started {
    core: CoreHandle,
    faults: Faults,
    resolutions: Arc<AtomicUsize>,
}

/// Два воркспейса с проектом и рабочей областью в каждом. Активен `home`, окно на его главной.
struct World {
    core: CoreHandle,
    faults: Faults,
    resolutions: Arc<AtomicUsize>,
    home: WorkspaceId,
    work: WorkspaceId,
    home_project: ProjectId,
    work_project: ProjectId,
    home_workbench: WorkbenchId,
    work_workbench: WorkbenchId,
}

const MISSING_PATH: &str = "/missing";

async fn world() -> TestResult<World> {
    world_with(registry()).await
}

async fn world_with(registry: Arc<Registry>) -> TestResult<World> {
    let resolver = FakeRootResolver::new().with(
        MISSING_PATH,
        Err(RootError::NotFound {
            path: MISSING_PATH.into(),
        }),
    );
    let Started {
        core,
        faults,
        resolutions,
    } = start_in(&MemoryPersist::new(), resolver, registry)?;
    let home = snapshot(&core)
        .await?
        .state
        .window
        .active_workspace
        .ok_or("нет активного воркспейса")?;
    let CommandValue::Workspace { workspace: work } = value(&core, create("Работа")).await?
    else {
        return Err("ожидался воркспейс".into());
    };
    let home_project = add_project(&core, home, "/src/home").await?;
    let work_project = add_project(&core, work, "/src/work").await?;
    let home_workbench = add_workbench(&core, home_project).await?;
    let work_workbench = add_workbench(&core, work_project).await?;
    let world = World {
        core,
        faults,
        resolutions,
        home,
        work,
        home_project,
        work_project,
        home_workbench,
        work_workbench,
    };
    let window = snapshot(&world.core).await?.state.window.clone();
    assert_eq!(
        window,
        WindowState {
            active_workspace: Some(home),
            route: Route::WorkspaceHome,
        }
    );
    Ok(world)
}

async fn value(core: &CoreHandle, command: Command) -> TestResult<CommandValue> {
    match core.request(command).await?.value {
        ReplyValue::Command(value) => Ok(value),
        other => Err(format!("ожидался ответ команды, получено {other:?}").into()),
    }
}

async fn add_project(
    core: &CoreHandle,
    workspace: WorkspaceId,
    path: &str,
) -> TestResult<ProjectId> {
    let add = Command::ProjectAdd {
        workspace,
        path: path.into(),
        title: None,
    };
    match value(core, add).await? {
        CommandValue::Project { project, .. } => Ok(project),
        other => Err(format!("ожидался проект, получено {other:?}").into()),
    }
}

async fn add_workbench(core: &CoreHandle, project: ProjectId) -> TestResult<WorkbenchId> {
    let create = Command::WorkbenchCreate {
        project,
        title: "Основная".into(),
    };
    match value(core, create).await? {
        CommandValue::Workbench { workbench } => Ok(workbench),
        other => Err(format!("ожидалась рабочая область, получено {other:?}").into()),
    }
}

async fn snapshot(core: &CoreHandle) -> Result<Snapshot, CoreError> {
    Ok(core.subscribe().await?.0)
}

fn create(title: &str) -> Command {
    Command::WorkspaceCreate {
        title: title.into(),
    }
}

/// Запрос отвергнут ровно этой ошибкой, и ядро не изменилось: ни записи, ни ревизии, ни
/// события, ни состояния.
async fn assert_refused(
    core: &CoreHandle,
    faults: &Faults,
    scope: TreeScope,
    command: Command,
    expected: &CoreError,
) -> TestResult<()> {
    let name = command.name();
    let before = snapshot(core).await?;
    let commits = faults.commits();
    let error = core
        .request_in(scope, command)
        .await
        .err()
        .ok_or_else(|| format!("{name} в {scope:?} прошла"))?;
    assert_eq!(&error, expected, "{name} в {scope:?}");
    let after = snapshot(core).await?;
    assert_eq!(after, before, "{name} в {scope:?} изменила ядро");
    assert_eq!(
        faults.commits(),
        commits,
        "{name} в {scope:?} дошла до записи"
    );
    assert_eq!(
        core.events_since(before.revision).await?,
        EventsSince::Events(Vec::new()),
        "{name} в {scope:?} дала событие"
    );
    Ok(())
}

/// Команда прошла и дала ровно одну новую ревизию.
async fn assert_passes(core: &CoreHandle, scope: TreeScope, command: Command) -> TestResult<()> {
    let name = command.name();
    let before = snapshot(core).await?.revision;
    let reply = core
        .request_in(scope, command)
        .await
        .map_err(|error| format!("{name} в {scope:?}: {error}"))?;
    assert_eq!(reply.revision, before.next(), "{name} в {scope:?}");
    Ok(())
}

async fn tree(core: &CoreHandle, scope: TreeScope) -> Result<TreeView, Box<dyn Error>> {
    match core.query(Query::Tree { scope }).await?.value {
        ReplyValue::Tree(tree) => Ok(tree),
        other => Err(format!("ожидалось дерево, получено {other:?}").into()),
    }
}

/// Команды по записям `work`, каждая меняет состояние: имя новое, путь свободен, окно на `home`.
fn work_commands(w: &World) -> Vec<(Command, CoreError)> {
    vec![
        (
            Command::WorkspaceRename {
                workspace: w.work,
                title: "Проекты".into(),
            },
            CoreError::not_found(Entity::Workspace, w.work),
        ),
        (
            Command::ProjectAdd {
                workspace: w.work,
                path: "/src/other".into(),
                title: None,
            },
            CoreError::not_found(Entity::Workspace, w.work),
        ),
        (
            Command::WorkbenchCreate {
                project: w.work_project,
                title: "Ревью".into(),
            },
            CoreError::not_found(Entity::Project, w.work_project),
        ),
        (
            Command::WorkbenchRename {
                workbench: w.work_workbench,
                title: "Ветка".into(),
            },
            CoreError::not_found(Entity::Workbench, w.work_workbench),
        ),
        (
            Command::WindowFocusWorkspace { workspace: w.work },
            CoreError::not_found(Entity::Workspace, w.work),
        ),
        (
            Command::WorkbenchOpen {
                workbench: w.work_workbench,
            },
            CoreError::not_found(Entity::Workbench, w.work_workbench),
        ),
    ]
}

#[tokio::test]
async fn foreign_scope_is_not_found_and_changes_nothing() {
    let w = world().await.unwrap();
    for (command, expected) in work_commands(&w) {
        assert_refused(
            &w.core,
            &w.faults,
            TreeScope::Workspace(w.home),
            command,
            &expected,
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn foreign_target_looks_like_missing_record() {
    let w = world().await.unwrap();
    let missing_project = ProjectId::from_bytes([0xee; 16]);
    let missing_workbench = WorkbenchId::from_bytes([0xee; 16]);
    let home = TreeScope::Workspace(w.home);
    let create = |project| Command::WorkbenchCreate {
        project,
        title: "Ревью".into(),
    };
    let open = |workbench| Command::WorkbenchOpen { workbench };
    let cases = [
        (
            create(w.work_project),
            create(missing_project),
            Entity::Project,
            w.work_project.to_string(),
            missing_project.to_string(),
        ),
        (
            open(w.work_workbench),
            open(missing_workbench),
            Entity::Workbench,
            w.work_workbench.to_string(),
            missing_workbench.to_string(),
        ),
    ];
    // Отказ по чужой записи и по несуществующей различаются только идентификатором.
    for (foreign, missing, entity, foreign_id, missing_id) in cases {
        let foreign = w.core.request_in(home, foreign).await.unwrap_err();
        let missing = w.core.request_in(home, missing).await.unwrap_err();
        assert_eq!(foreign, CoreError::not_found(entity, &foreign_id));
        assert_eq!(missing, CoreError::not_found(entity, &missing_id));
    }
}

#[tokio::test]
async fn own_scope_and_all_scope_pass() {
    for own in [true, false] {
        let w = world().await.unwrap();
        let scope = if own {
            TreeScope::Workspace(w.work)
        } else {
            TreeScope::All
        };
        for (command, _) in work_commands(&w) {
            assert_passes(&w.core, scope, command).await.unwrap();
        }
        let view = tree(&w.core, TreeScope::Workspace(w.work)).await.unwrap();
        assert_eq!(view.workspaces[0].title.as_str(), "Проекты");
        assert_eq!(view.active_workspace, Some(w.work));
        assert_eq!(view.route, Some(Route::Workbench(w.work_workbench)));
    }
}

#[tokio::test]
async fn request_is_request_in_all() {
    let w = world().await.unwrap();
    let before = snapshot(&w.core).await.unwrap().revision;
    let reply = w
        .core
        .request(Command::WorkbenchOpen {
            workbench: w.work_workbench,
        })
        .await
        .unwrap();
    assert_eq!(reply.revision, before.next());
    let error = w
        .core
        .request(Command::WorkbenchOpen {
            workbench: WorkbenchId::from_bytes([0xee; 16]),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound);
}

#[tokio::test]
async fn workbench_open_under_foreign_scope_keeps_window() {
    let w = world().await.unwrap();
    let before = snapshot(&w.core).await.unwrap();
    let error = w
        .core
        .request_in(
            TreeScope::Workspace(w.home),
            Command::WorkbenchOpen {
                workbench: w.work_workbench,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        error,
        CoreError::not_found(Entity::Workbench, w.work_workbench)
    );
    let after = snapshot(&w.core).await.unwrap();
    assert_eq!(after.state.window, before.state.window);
    assert_eq!(after.state.window.active_workspace, Some(w.home));
    assert_eq!(
        after.state.workspaces[&w.work].last_workbench,
        before.state.workspaces[&w.work].last_workbench,
        "последняя рабочая область чужого воркспейса изменилась"
    );
}

#[tokio::test]
async fn workspace_create_requires_all_scope() {
    let w = world().await.unwrap();
    let expected = CoreError::scope_requires_all("workspace.create");
    assert_eq!(expected.detail_str("field"), Some("scope"));
    assert_eq!(expected.detail_str("reason"), Some("scope_requires_all"));
    for scope in [w.home, w.work, WorkspaceId::from_bytes([0xee; 16])] {
        assert_refused(
            &w.core,
            &w.faults,
            TreeScope::Workspace(scope),
            create("Новый"),
            &expected,
        )
        .await
        .unwrap();
    }
    assert_passes(&w.core, TreeScope::All, create("Новый"))
        .await
        .unwrap();
}

#[tokio::test]
async fn project_add_with_foreign_workspace_is_refused_before_root_resolution() {
    let w = world().await.unwrap();
    let add = |workspace| Command::ProjectAdd {
        workspace,
        path: MISSING_PATH.into(),
        title: None,
    };
    let resolutions = w.resolutions.load(Ordering::SeqCst);
    assert_refused(
        &w.core,
        &w.faults,
        TreeScope::Workspace(w.home),
        add(w.work),
        &CoreError::not_found(Entity::Workspace, w.work),
    )
    .await
    .unwrap();
    assert_eq!(
        w.resolutions.load(Ordering::SeqCst),
        resolutions,
        "корень разрешался до проверки области"
    );

    // Положительный контроль: в своей области та же команда доходит до разрешения пути.
    let error = w
        .core
        .request_in(TreeScope::Workspace(w.work), add(w.work))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
    assert_eq!(error.detail_str("reason"), Some("path_not_found"));
    assert_eq!(w.resolutions.load(Ordering::SeqCst), resolutions + 1);
}

#[tokio::test]
async fn root_of_other_workspace_project_is_root_taken() {
    let w = world().await.unwrap();
    let error = w
        .core
        .request_in(
            TreeScope::Workspace(w.home),
            Command::ProjectAdd {
                workspace: w.home,
                path: "/src/work".into(),
                title: Some("Чужой".into()),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::RootTaken);
    assert_eq!(
        error.detail_str("project"),
        Some(w.work_project.to_string().as_str())
    );
    assert_eq!(
        error.detail_str("workspace"),
        Some(w.work.to_string().as_str())
    );
}

/// Имена воркспейсов уникальны среди всех воркспейсов: занятое чужим воркспейсом имя под своей
/// областью даёт `name_taken` с id владельца имени, как `root_taken`, и ничего не меняет.
#[tokio::test]
async fn workspace_name_of_other_workspace_is_name_taken() {
    let w = world().await.unwrap();
    let before = snapshot(&w.core).await.unwrap().revision;
    let error = w
        .core
        .request_in(
            TreeScope::Workspace(w.home),
            Command::WorkspaceRename {
                workspace: w.home,
                title: "Работа".into(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::NameTaken);
    assert_eq!(
        error.detail_str("existing"),
        Some(w.work.to_string().as_str())
    );
    assert_eq!(snapshot(&w.core).await.unwrap().revision, before);
}

/// Состояние с живым активным воркспейсом и удалённым воркспейсом, в котором есть проект с
/// рабочей областью.
struct Deleted {
    state: State,
    live: WorkspaceId,
    live_workbench: WorkbenchId,
    deleted: WorkspaceId,
    deleted_project: ProjectId,
    deleted_workbench: WorkbenchId,
}

fn title(name: &str) -> TestResult<Title> {
    Title::new(name).map_err(|error| error.to_string().into())
}

fn deleted_state() -> TestResult<Deleted> {
    let mut ids = SequentialIds::new();
    let ids: &mut dyn IdGen = &mut ids;
    let live = WorkspaceId::generate(ids);
    let deleted = WorkspaceId::generate(ids);
    let live_project = ProjectId::generate(ids);
    let deleted_project = ProjectId::generate(ids);
    let live_workbench = WorkbenchId::generate(ids);
    let deleted_workbench = WorkbenchId::generate(ids);
    let mut state = State {
        window: WindowState {
            active_workspace: Some(live),
            route: Route::WorkspaceHome,
        },
        ..State::default()
    };
    for (id, name, position, deleted_at) in
        [(live, "Работа", 0, None), (deleted, "Архив", 1, Some(NOW))]
    {
        state.workspaces.insert(
            id,
            Workspace {
                id,
                title: title(name)?,
                position,
                last_workbench: None,
                deleted_at,
            },
        );
    }
    for (project, workspace, workbench, path) in [
        (live_project, live, live_workbench, "/src/live"),
        (deleted_project, deleted, deleted_workbench, "/src/deleted"),
    ] {
        state.projects.insert(
            project,
            Project {
                id: project,
                workspace,
                title: title("shellio")?,
                list_position: Some(0),
                removed_at: None,
                last_opened_at: None,
            },
        );
        let root = ProjectRootId::generate(ids);
        state.roots.insert(
            root,
            ProjectRoot {
                id: root,
                project,
                path: PathBuf::from(path),
                kind: RootKind::Folder,
                key: RootKey::new(path),
            },
        );
        state.workbenches.insert(
            workbench,
            Workbench {
                id: workbench,
                project,
                title: title("Основная")?,
                position: 0,
            },
        );
    }
    assert_eq!(state.check_invariants(), Vec::new());
    Ok(Deleted {
        state,
        live,
        live_workbench,
        deleted,
        deleted_project,
        deleted_workbench,
    })
}

#[tokio::test]
async fn deleted_or_missing_scope_workspace_is_not_found_by_workspace() {
    let d = deleted_state().unwrap();
    let memory = MemoryPersist::with_state(d.state.clone(), Revision(3));
    let Started { core, faults, .. } = start_with(&memory, FakeRootResolver::new()).unwrap();
    let deleted = TreeScope::Workspace(d.deleted);
    let gone = CoreError::not_found(Entity::Workspace, d.deleted);
    let commands = [
        Command::WorkspaceRename {
            workspace: d.deleted,
            title: "Новое".into(),
        },
        Command::WindowFocusWorkspace {
            workspace: d.deleted,
        },
        Command::ProjectAdd {
            workspace: d.deleted,
            path: "/src/new".into(),
            title: None,
        },
        Command::WorkbenchCreate {
            project: d.deleted_project,
            title: "Ревью".into(),
        },
        Command::WorkbenchRename {
            workbench: d.deleted_workbench,
            title: "Ветка".into(),
        },
        Command::WorkbenchOpen {
            workbench: d.deleted_workbench,
        },
    ];
    for command in commands {
        assert_refused(&core, &faults, deleted, command, &gone)
            .await
            .unwrap();
    }
    let error = core
        .query(Query::Tree { scope: deleted })
        .await
        .unwrap_err();
    assert_eq!(error, gone);

    // Прямой воркспейс вне области отвергается раньше проверки самой области.
    assert_refused(
        &core,
        &faults,
        deleted,
        Command::WorkspaceRename {
            workspace: d.live,
            title: "Новое".into(),
        },
        &CoreError::not_found(Entity::Workspace, d.live),
    )
    .await
    .unwrap();

    let missing = WorkspaceId::from_bytes([0xee; 16]);
    assert_refused(
        &core,
        &faults,
        TreeScope::Workspace(missing),
        Command::WorkbenchOpen {
            workbench: d.live_workbench,
        },
        &CoreError::not_found(Entity::Workspace, missing),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn tree_shows_window_fields_only_for_scope_of_active_workspace() {
    let w = world().await.unwrap();
    w.core
        .request(Command::WorkbenchOpen {
            workbench: w.home_workbench,
        })
        .await
        .unwrap();
    let window = (Some(w.home), Some(Route::Workbench(w.home_workbench)));
    for (scope, expected, workspaces) in [
        (TreeScope::Workspace(w.home), window, vec![w.home]),
        (TreeScope::All, window, vec![w.home, w.work]),
        (TreeScope::Workspace(w.work), (None, None), vec![w.work]),
    ] {
        let view = tree(&w.core, scope).await.unwrap();
        assert_eq!((view.active_workspace, view.route), expected, "{scope:?}");
        let ids: Vec<_> = view
            .workspaces
            .iter()
            .map(|workspace| workspace.id)
            .collect();
        assert_eq!(ids, workspaces, "{scope:?}");
    }
    let view = tree(&w.core, TreeScope::Workspace(w.work)).await.unwrap();
    assert_eq!(view.workspaces[0].projects[0].id, w.work_project);
    assert_ne!(view.workspaces[0].projects[0].id, w.home_project);
}

/// Панели рабочей области: колонка навигатора и терминал с фокусом.
async fn panels_of(core: &CoreHandle, workbench: WorkbenchId) -> TestResult<(PanelId, PanelId)> {
    let state = snapshot(core).await?.state;
    let layout = state
        .layouts
        .get(&workbench)
        .ok_or("у рабочей области нет раскладки")?;
    let navigator = layout.navigator.ok_or("нет колонки навигатора")?;
    let terminal = layout.focused().ok_or("нет панели с фокусом")?;
    Ok((navigator, terminal))
}

#[tokio::test]
async fn layout_targets_outside_scope_look_like_missing_records() -> TestResult<()> {
    let w = world().await?;
    let home = TreeScope::Workspace(w.home);
    let (_, home_terminal) = panels_of(&w.core, w.home_workbench).await?;
    let (_, work_terminal) = panels_of(&w.core, w.work_workbench).await?;
    let missing = PanelId::from_bytes([0xee; 16]);

    // Чужая панель в области `home` отвечает так же, как несуществующая.
    for panel in [work_terminal, missing] {
        let expected = CoreError::not_found(Entity::Panel, panel);
        for command in [
            Command::LayoutSplit { panel, open: None },
            Command::PanelFocus {
                panel: Some(panel),
                workbench: None,
                direction: None,
            },
            Command::PanelSelect { panel },
            Command::PanelMove {
                panel,
                zone: Zone::Right,
            },
            Command::PanelClose { panel },
            Command::LayoutResize {
                workbench: None,
                zone: Zone::Center,
                part_of: Some(panel),
                size: 500,
            },
            Command::LayoutZoom {
                panel: Some(panel),
                workbench: None,
                off: None,
            },
        ] {
            assert_refused(&w.core, &w.faults, home, command, &expected).await?;
        }
        // Чужой якорь при своей панели — тоже как несуществующий, в любой области.
        for scope in [home, TreeScope::All] {
            let command = Command::PanelMovePart {
                panel: home_terminal,
                anchor: panel,
                edge: None,
            };
            assert_refused(&w.core, &w.faults, scope, command, &expected).await?;
        }
    }

    // Явная чужая рабочая область — как несуществующая.
    let only = |workbench| Command::LayoutOnly {
        workbench: Some(workbench),
    };
    let expected = CoreError::not_found(Entity::Workbench, w.work_workbench);
    assert_refused(&w.core, &w.faults, home, only(w.work_workbench), &expected).await?;

    // Открытая рабочая область `work` в области `home` не раскрывается.
    w.core
        .request(Command::WorkbenchOpen {
            workbench: w.work_workbench,
        })
        .await?;
    let hidden = CoreError::open_workbench_hidden();
    assert_eq!(hidden.detail("id"), Some(&serde_json::Value::Null));
    assert_refused(
        &w.core,
        &w.faults,
        home,
        Command::LayoutOnly { workbench: None },
        &hidden,
    )
    .await?;
    let read = w
        .core
        .query(Query::Layout {
            scope: home,
            workbench: None,
        })
        .await
        .err()
        .ok_or("чтение чужой открытой раскладки прошло")?;
    assert_eq!(read, hidden);
    let read = w
        .core
        .query(Query::Layout {
            scope: home,
            workbench: Some(w.work_workbench),
        })
        .await
        .err()
        .ok_or("чтение чужой раскладки прошло")?;
    assert_eq!(read, expected);

    // Своя область и область всех проходят.
    assert_passes(
        &w.core,
        TreeScope::All,
        Command::LayoutOnly { workbench: None },
    )
    .await?;
    assert_passes(&w.core, home, only(w.home_workbench)).await?;
    assert_passes(
        &w.core,
        home,
        Command::PanelMove {
            panel: home_terminal,
            zone: Zone::Right,
        },
    )
    .await?;
    match w
        .core
        .query(Query::Layout {
            scope: home,
            workbench: Some(w.home_workbench),
        })
        .await?
        .value
    {
        ReplyValue::Layout(view) => {
            assert_eq!(view.workbench, w.home_workbench);
            assert_eq!(view.frames, None, "окна нет");
        }
        other => return Err(format!("ожидалась раскладка, получено {other:?}").into()),
    }
    Ok(())
}

#[tokio::test]
async fn viewport_reaches_commands_and_reads_without_revision() -> TestResult<()> {
    let w = world().await?;
    let (_, terminal) = panels_of(&w.core, w.home_workbench).await?;
    let split = || Command::LayoutSplit {
        panel: terminal,
        open: Some("terminal".into()),
    };
    let error = w
        .core
        .request(split())
        .await
        .err()
        .ok_or("деление без окна прошло")?;
    assert_eq!(error.code(), ErrorCode::NoUi);

    let before = snapshot(&w.core).await?.revision;
    let viewport = Viewport {
        width: 1180,
        height: 720,
    };
    w.core.set_viewport(Some(viewport));
    w.core.set_viewport(Some(viewport));
    assert_eq!(
        snapshot(&w.core).await?.revision,
        before,
        "размер окна — не ревизия"
    );
    let read = |workbench| Query::Layout {
        scope: TreeScope::All,
        workbench: Some(workbench),
    };
    match w.core.query(read(w.home_workbench)).await?.value {
        ReplyValue::Layout(view) => {
            let frames = view.frames.ok_or("нет кадров при известном размере")?;
            assert_eq!(frames.area.width, 1180.0);
        }
        other => return Err(format!("ожидалась раскладка, получено {other:?}").into()),
    }
    assert_passes(&w.core, TreeScope::All, split()).await?;

    w.core.set_viewport(None);
    match w.core.query(read(w.home_workbench)).await?.value {
        ReplyValue::Layout(view) => assert_eq!(view.frames, None),
        other => return Err(format!("ожидалась раскладка, получено {other:?}").into()),
    }
    Ok(())
}

fn open(plugin: &str, workbench: WorkbenchId) -> Command {
    Command::PanelOpen {
        plugin: plugin.into(),
        workbench: Some(workbench),
        zone: None,
        args: None,
    }
}

async fn opened(
    core: &CoreHandle,
    scope: TreeScope,
    command: Command,
) -> TestResult<(PanelId, WorkbenchId, OpenOutcome, Revision)> {
    let reply = core.request_in(scope, command).await?;
    match reply.value {
        ReplyValue::Command(CommandValue::Opened {
            panel,
            workbench,
            outcome,
        }) => Ok((panel, workbench, outcome, reply.revision)),
        other => Err(format!("ожидался исход panel.open, получено {other:?}").into()),
    }
}

/// Матрица области `panel.open` двух воркспейсов: экземпляры singleton живут в `work`, после
/// подготовки меняется только `home`. Чужой экземпляр не раскрывается ни ответом, ни ревизией.
#[tokio::test]
async fn panel_open_under_workspace_scope_never_reveals_a_foreign_instance() -> TestResult<()> {
    let w = world_with(registry_of(vec![
        TestPlugin::new("field")
            .cardinality(Cardinality::PerApp)
            .actions(),
        TestPlugin::new("ws")
            .cardinality(Cardinality::PerWorkspace)
            .actions(),
    ]))
    .await?;
    w.core.set_viewport(Some(Viewport {
        width: 1180,
        height: 720,
    }));
    let all = TreeScope::All;
    let (field, ..) = opened(&w.core, all, open("field", w.work_workbench)).await?;
    let (foreign, ..) = opened(&w.core, all, open("ws", w.work_workbench)).await?;
    let home = TreeScope::Workspace(w.home);
    let (_, home_terminal) = panels_of(&w.core, w.home_workbench).await?;
    let hidden = [field.to_string(), foreign.to_string()];
    let reveals = |error: &CoreError| {
        let text = serde_json::to_string(error).unwrap_or_default();
        hidden.iter().any(|id| text.contains(id))
    };

    // `PerApp` под областью воркспейса: отказ до поиска экземпляра, даже в своей рабочей области.
    let requires_all = [
        (
            open("field", w.home_workbench),
            CoreError::scope_requires_all("panel.open"),
        ),
        (
            Command::LayoutSplit {
                panel: home_terminal,
                open: Some("field".into()),
            },
            CoreError::scope_requires_all("layout.split"),
        ),
        // Чужая рабочая область и чужие панели — как несуществующие.
        (
            open("ws", w.work_workbench),
            CoreError::not_found(Entity::Workbench, w.work_workbench),
        ),
        (
            Command::PanelCommand {
                panel: field,
                name: "field.add".into(),
                args: Some(serde_json::json!({"by": 1})),
            },
            CoreError::not_found(Entity::Panel, field),
        ),
        (
            Command::PanelClose { panel: foreign },
            CoreError::not_found(Entity::Panel, foreign),
        ),
    ];
    for (command, expected) in requires_all {
        // Отказ называет только то, что прислал сам вызывающий: чужих экземпляров в нём нет.
        if expected.detail_str("entity") != Some("panel") {
            assert!(!reveals(&expected), "{expected}");
        }
        assert_refused(&w.core, &w.faults, home, command, &expected).await?;
    }
    for panel in [field, foreign] {
        let error = w
            .core
            .query(Query::Panel { scope: home, panel })
            .await
            .unwrap_err();
        assert_eq!(error, CoreError::not_found(Entity::Panel, panel));
    }

    // `PerWorkspace` в своём воркспейсе: новый экземпляр, чужой не тронут и не назван.
    let before = snapshot(&w.core).await?;
    let (own, workbench, outcome, revision) =
        opened(&w.core, home, open("ws", w.home_workbench)).await?;
    assert_eq!(
        (workbench, outcome, revision),
        (
            w.home_workbench,
            OpenOutcome::Created,
            before.revision.next()
        )
    );
    assert_ne!(own, foreign);
    let EventsSince::Events(events) = w.core.events_since(before.revision).await? else {
        return Err("ожидались события".into());
    };
    let [event] = events.as_slice() else {
        return Err(format!("ожидалось одно событие: {events:?}").into());
    };
    for change in &event.changes {
        let touched = match change {
            Change::PanelAdd { panel } => panel.workbench,
            Change::PanelData { panel, .. } => {
                assert_eq!(*panel, own);
                w.home_workbench
            }
            Change::LayoutSet { workbench, .. } => *workbench,
            other => return Err(format!("неожиданная дельта {other:?}").into()),
        };
        assert_eq!(touched, w.home_workbench);
    }
    let after = snapshot(&w.core).await?;
    assert_eq!(after.state.panels[&foreign], before.state.panels[&foreign]);
    assert_eq!(
        after.state.layouts[&w.work_workbench],
        before.state.layouts[&w.work_workbench]
    );
    // Повтор под областью находит свой экземпляр без ревизии.
    let (again, _, outcome, revision) = opened(&w.core, home, open("ws", w.home_workbench)).await?;
    assert_eq!(
        (again, outcome, revision),
        (own, OpenOutcome::Focused, after.revision)
    );
    let view = w
        .core
        .query(Query::Panel {
            scope: home,
            panel: own,
        })
        .await?;
    let ReplyValue::Panel(view) = view.value else {
        return Err("ожидалась панель".into());
    };
    assert_eq!((view.panel.id, view.canonical), (own, None));
    Ok(())
}
