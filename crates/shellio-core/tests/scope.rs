//! Область команд и дерева: проверка в акторе на одном снимке с обработчиком.
//!
//! Цель вне области отвергается `not_found`, как несуществующая запись: без записи, без новой
//! ревизии и без события.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use shellio_core::test_support::{
    FakeRootResolver, Faults, FaultyPersist, ManualClock, MemoryPersist, SequentialIds,
};
use shellio_core::{
    Command, CommandValue, Core, CoreError, CoreHandle, Entity, ErrorCode, EventsSince, IdGen,
    Project, ProjectId, ProjectRoot, ProjectRootId, Query, ReplyValue, ResolvedRoot, Revision,
    RootError, RootKey, RootKind, RootResolver, Route, Snapshot, State, Title, TreeScope, TreeView,
    UnixMillis, WindowState, Workbench, WorkbenchId, Workspace, WorkspaceId,
};
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
    } = start_with(&MemoryPersist::new(), resolver)?;
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
