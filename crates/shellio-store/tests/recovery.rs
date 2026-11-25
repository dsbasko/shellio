//! Восстановление ядра на настоящем `state.db`: повторное открытие, первый запуск, отказ
//! посреди транзакции и случайные последовательности команд. Состояние актора, перезагрузка
//! из базы и зеркало из снимка и событий подписчика должны совпадать.

use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use shellio_core::test_support::{
    FakeRootResolver, ManualClock, SequentialIds, TestPlugin, registry, registry_of,
};
use shellio_core::{
    BOOTSTRAP_WORKSPACE, Change, Command, CommandValue, CommitError, Core, CoreHandle, ErrorCode,
    EventsSince, IdGen, Loaded, PanelId, Persist, Project, ProjectId, ProjectRoot, ProjectRootId,
    Query, Reply, ReplyValue, Revision, RootKey, RootKind, RootResolver, Route, Snapshot, State,
    Title, TreeScope, UnixMillis, Workbench, WorkbenchId, Workspace, WorkspaceId, fingerprint,
};
use shellio_core::{OpenOutcome, PanelData};
use shellio_layout::{Hideable, Viewport, Zone};
use shellio_plugin_api::{Cardinality, Registry};
use shellio_store::{Database, Profile, SqlitePersist};
use tempfile::TempDir;
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::TryRecvError;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);
const REPO: &str = "/src/repo";
const WORKTREE: &str = "/src/repo-wt";
const REPO_KEY: &str = "/src/repo/.git";
const SEEDS: u64 = 200;
const STEPS: usize = 50;

/// Изолированный профиль с базой, которую открывают и закрывают по ходу теста.
struct Store {
    _dir: TempDir,
    profile: Profile,
}

impl Store {
    fn new() -> TestResult<Self> {
        let dir = TempDir::new()?;
        let profile = Profile::isolated(dir.path().join("state"));
        Ok(Self { _dir: dir, profile })
    }

    fn persist(&self) -> TestResult<SqlitePersist> {
        Ok(SqlitePersist::new(Database::open(
            &self.profile,
            &ManualClock::new(NOW),
        )?))
    }

    /// Загрузка отдельным соединением, как при следующем запуске.
    fn load(&self) -> TestResult<Loaded> {
        self.persist()?.load()
    }

    fn raw(&self) -> TestResult<Connection> {
        Ok(Connection::open(self.profile.state_db())?)
    }

    fn count(&self, table: &str) -> TestResult<i64> {
        Ok(self
            .raw()?
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })?)
    }

    fn revision_in_db(&self) -> TestResult<i64> {
        Ok(self
            .raw()?
            .query_row("SELECT value FROM meta WHERE key = 'revision'", [], |row| {
                row.get(0)
            })?)
    }
}

fn start(
    persist: SqlitePersist,
    clock: &ManualClock,
    resolver: impl RootResolver + 'static,
) -> TestResult<CoreHandle> {
    start_with(persist, clock, resolver, registry())
}

fn start_with(
    persist: SqlitePersist,
    clock: &ManualClock,
    resolver: impl RootResolver + 'static,
    registry: Arc<Registry>,
) -> TestResult<CoreHandle> {
    Ok(Core::start(
        &Handle::current(),
        Box::new(persist),
        Box::new(clock.clone()),
        Box::new(SequentialIds::new()),
        Arc::new(resolver),
        registry,
    )
    .map_err(|error| error.to_string())?)
}

fn resolver() -> FakeRootResolver {
    FakeRootResolver::new()
        .with_git(REPO, REPO_KEY)
        .with_git(WORKTREE, REPO_KEY)
}

async fn snapshot(core: &CoreHandle) -> TestResult<Snapshot> {
    Ok(core.subscribe().await?.0)
}

async fn request(core: &CoreHandle, command: Command) -> TestResult<Reply> {
    Ok(core.request(command).await?)
}

fn created(reply: &Reply) -> TestResult<CommandValue> {
    match &reply.value {
        ReplyValue::Command(value) => Ok(*value),
        other => Err(format!("ожидался ответ команды, получено {other:?}").into()),
    }
}

async fn create_workspace(core: &CoreHandle, title: &str) -> TestResult<WorkspaceId> {
    let reply = request(
        core,
        Command::WorkspaceCreate {
            title: title.into(),
        },
    )
    .await?;
    match created(&reply)? {
        CommandValue::Workspace { workspace } => Ok(workspace),
        other => Err(format!("ожидался воркспейс, получено {other:?}").into()),
    }
}

async fn add_project(
    core: &CoreHandle,
    workspace: WorkspaceId,
    path: &str,
    title: Option<&str>,
) -> TestResult<ProjectId> {
    let reply = request(
        core,
        Command::ProjectAdd {
            workspace,
            path: path.into(),
            title: title.map(str::to_owned),
        },
    )
    .await?;
    match created(&reply)? {
        CommandValue::Project { project, .. } => Ok(project),
        other => Err(format!("ожидался проект, получено {other:?}").into()),
    }
}

async fn create_workbench(
    core: &CoreHandle,
    project: ProjectId,
    title: &str,
) -> TestResult<WorkbenchId> {
    let reply = request(
        core,
        Command::WorkbenchCreate {
            project,
            title: title.into(),
        },
    )
    .await?;
    match created(&reply)? {
        CommandValue::Workbench { workbench } => Ok(workbench),
        other => Err(format!("ожидалась рабочая область, получено {other:?}").into()),
    }
}

fn active(snapshot: &Snapshot) -> TestResult<WorkspaceId> {
    snapshot
        .state
        .window
        .active_workspace
        .ok_or_else(|| "нет активного воркспейса".into())
}

fn without_diagnostics(state: &State) -> State {
    State {
        diagnostics: Vec::new(),
        ..state.clone()
    }
}

/// Дерево без ревизии ответа: перезапуск может дописать раскладки, не меняя дерева.
async fn tree_json(core: &CoreHandle, scope: TreeScope) -> TestResult<String> {
    Ok(serde_json::to_string(
        &core.query(Query::Tree { scope }).await?.value,
    )?)
}

#[tokio::test]
async fn layout_survives_restart_on_same_database() -> TestResult {
    let store = Store::new()?;
    let clock = ManualClock::new(NOW);
    let core = start(store.persist()?, &clock, resolver())?;
    core.set_viewport(Some(Viewport {
        width: 1180,
        height: 720,
    }));
    let workspace = active(&snapshot(&core).await?)?;
    let project = add_project(&core, workspace, REPO, None).await?;
    let workbench = create_workbench(&core, project, "main").await?;
    request(&core, Command::WorkbenchOpen { workbench }).await?;
    let terminal = snapshot(&core).await?.state.layouts[&workbench]
        .focused()
        .ok_or("нет панели с фокусом")?;
    let reply = request(
        &core,
        Command::LayoutSplit {
            panel: terminal,
            open: Some("terminal".into()),
            axis: None,
        },
    )
    .await?;
    let CommandValue::Panel { panel: second } = created(&reply)? else {
        return Err("ожидалась панель".into());
    };
    for command in [
        Command::LayoutResize {
            workbench: None,
            zone: Zone::Center,
            part_of: Some(terminal),
            size: 600,
        },
        Command::PanelMove {
            panel: second,
            zone: Zone::Right,
        },
        Command::LayoutResize {
            workbench: None,
            zone: Zone::Right,
            part_of: None,
            size: 320,
        },
        Command::LayoutVisibility {
            workbench: None,
            target: Hideable::Navigator,
            visible: false,
        },
        Command::LayoutZoom {
            panel: Some(terminal),
            workbench: None,
            off: None,
        },
    ] {
        request(&core, command).await?;
    }
    let before = snapshot(&core).await?;
    let layout = before.state.layouts[&workbench].clone();
    assert_eq!(layout.zoomed, Some(terminal));
    assert!(layout.hidden.navigator);
    assert_eq!(layout.sizes.right, 320);
    core.shutdown().await;

    // Новый запуск на том же профиле: раскладка, панели и ревизия прежние, запуск не пишет.
    let core = start(store.persist()?, &clock, resolver())?;
    let after = snapshot(&core).await?;
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.state.diagnostics, Vec::new());
    assert_eq!(after.state.layouts, before.state.layouts);
    assert_eq!(after.state.panels, before.state.panels);
    assert_eq!(after.state.layouts[&workbench].focused(), Some(terminal));

    // Восстановленная раскладка принимает команды без окна, кроме тех, которым нужно место.
    let reply = request(
        &core,
        Command::LayoutZoom {
            panel: None,
            workbench: None,
            off: Some(true),
        },
    )
    .await?;
    assert_eq!(reply.revision, before.revision.next());
    let error = core
        .request(Command::LayoutSplit {
            panel: terminal,
            open: Some("terminal".into()),
            axis: None,
        })
        .await
        .err()
        .ok_or("деление без окна прошло")?;
    assert_eq!(error.code(), ErrorCode::NoUi);
    let actor = snapshot(&core).await?;
    core.shutdown().await;
    let reloaded = store.load()?;
    assert_eq!(reloaded.state, *actor.state);
    assert_eq!(reloaded.state.layouts[&workbench].zoomed, None);
    Ok(())
}

#[tokio::test]
async fn reopen_restores_state_revision_and_tree() {
    let store = Store::new().unwrap();
    let clock = ManualClock::new(NOW);
    let core = start(store.persist().unwrap(), &clock, resolver()).unwrap();

    let first = active(&snapshot(&core).await.unwrap()).unwrap();
    let work = create_workspace(&core, "Работа").await.unwrap();
    request(
        &core,
        Command::WorkspaceRename {
            workspace: first,
            title: "Личное".into(),
        },
    )
    .await
    .unwrap();
    let repo = add_project(&core, work, REPO, None).await.unwrap();
    add_project(&core, work, "/src/notes", Some("Заметки"))
        .await
        .unwrap();
    let site = add_project(&core, first, "/src/site", None).await.unwrap();
    let main = create_workbench(&core, repo, "main").await.unwrap();
    let review = create_workbench(&core, repo, "review").await.unwrap();
    let draft = create_workbench(&core, site, "Черновик").await.unwrap();
    request(
        &core,
        Command::WorkbenchRename {
            workbench: review,
            title: "Ревью".into(),
        },
    )
    .await
    .unwrap();
    clock.advance(1_000);
    request(&core, Command::WorkbenchOpen { workbench: main })
        .await
        .unwrap();
    clock.advance(1_000);
    request(&core, Command::WorkbenchOpen { workbench: draft })
        .await
        .unwrap();
    request(&core, Command::WindowFocusWorkspace { workspace: work })
        .await
        .unwrap();

    let before = snapshot(&core).await.unwrap();
    // Каждое необязательное поле, кроме удаления, заполнено хотя бы у одной записи.
    assert_eq!(before.state.window.route, Route::Workbench(main));
    assert_eq!(before.state.workspaces[&first].last_workbench, Some(draft));
    assert_eq!(before.state.workspaces[&work].last_workbench, Some(main));
    assert_eq!(
        before.state.projects[&repo].last_opened_at,
        Some(UnixMillis(NOW.0 + 1_000))
    );
    let kinds: BTreeSet<_> = before
        .state
        .roots
        .values()
        .map(|root| root.kind.as_str())
        .collect();
    assert_eq!(kinds, BTreeSet::from(["folder", "git"]));
    let tree_all = tree_json(&core, TreeScope::All).await.unwrap();
    let tree_first = tree_json(&core, TreeScope::Workspace(first)).await.unwrap();
    core.shutdown().await;
    assert_eq!(
        store.revision_in_db().unwrap(),
        i64::try_from(before.revision.0).unwrap()
    );

    let core = start(store.persist().unwrap(), &clock, resolver()).unwrap();
    let after = snapshot(&core).await.unwrap();
    // Перезапуск ничего не пишет: раскладки создала команда `workbench.create`.
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.state.diagnostics, Vec::new());
    assert_eq!(
        before
            .state
            .layouts
            .keys()
            .copied()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([main, review, draft])
    );
    assert_eq!(
        without_diagnostics(&after.state),
        without_diagnostics(&before.state)
    );
    assert_eq!(tree_json(&core, TreeScope::All).await.unwrap(), tree_all);
    assert_eq!(
        tree_json(&core, TreeScope::Workspace(first)).await.unwrap(),
        tree_first
    );

    // Ревизия продолжается через перезапуск, курсор прошлого запуска получает снимок.
    let renamed = request(
        &core,
        Command::WorkspaceRename {
            workspace: work,
            title: "Работа 2".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(renamed.revision, after.revision.next());
    match core
        .events_since(Revision(before.revision.0 - 1))
        .await
        .unwrap()
    {
        EventsSince::Reset(snapshot) => assert_eq!(snapshot.revision, renamed.revision),
        EventsSince::Events(events) => panic!("ожидался снимок, получены события {events:?}"),
    }
    core.shutdown().await;
}

#[tokio::test]
async fn first_launch_bootstraps_once() {
    let store = Store::new().unwrap();
    let clock = ManualClock::new(NOW);

    let db = Database::open(&store.profile, &clock).unwrap();
    let migrated: i64 = db
        .connection()
        .query_row("SELECT value FROM meta WHERE key = 'revision'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(migrated, 0);
    assert_eq!(store.count("workspaces").unwrap(), 0);

    let core = start(SqlitePersist::new(db), &clock, resolver()).unwrap();
    let first = snapshot(&core).await.unwrap();
    assert_eq!(first.revision, Revision(1));
    let workspace = active(&first).unwrap();
    let titles: Vec<_> = first
        .state
        .workspaces
        .values()
        .map(|workspace| workspace.title.as_str())
        .collect();
    assert_eq!(titles, [BOOTSTRAP_WORKSPACE]);
    assert_eq!(first.state.window.route, Route::WorkspaceHome);
    assert_eq!(first.state.window.active_workspace, Some(workspace));
    // В новой базе нет отпечатка реестра плагинов: запись запуска пишет его той же транзакцией
    // и в буфер событий не попадает, курсор 0 получает снимок.
    assert_eq!(
        core.events_since(Revision::ZERO).await.unwrap(),
        EventsSince::Reset(first.clone())
    );
    core.shutdown().await;
    assert_eq!(store.revision_in_db().unwrap(), 1);
    assert_eq!(
        store.load().unwrap().fingerprint,
        Some(fingerprint(&registry()))
    );

    let core = start(store.persist().unwrap(), &clock, resolver()).unwrap();
    let second = snapshot(&core).await.unwrap();
    assert_eq!(second, first);
    match core.events_since(Revision::ZERO).await.unwrap() {
        EventsSince::Reset(snapshot) => assert_eq!(snapshot, first),
        EventsSince::Events(events) => panic!("второй запуск записал события {events:?}"),
    }
    assert_eq!(
        core.events_since(Revision(1)).await.unwrap(),
        EventsSince::Events(Vec::new())
    );
    core.shutdown().await;
    assert_eq!(store.revision_in_db().unwrap(), 1);
    assert_eq!(store.count("workspaces").unwrap(), 1);
}

#[tokio::test]
async fn commit_fault_mid_transaction_changes_nothing_and_next_command_passes() {
    let store = Store::new().unwrap();
    let clock = ManualClock::new(NOW);
    let persist = store.persist().unwrap();
    let fault = persist.commit_fault();
    let core = start(persist, &clock, resolver()).unwrap();
    let (before, mut events) = core.subscribe().await.unwrap();
    let workspace = active(&before).unwrap();
    let add = Command::ProjectAdd {
        workspace,
        path: REPO.into(),
        title: None,
    };

    // `project.add` пишет проект и корень: сбой после первого оператора оставил бы проект
    // без корня, если бы транзакция не откатилась.
    fault.fail_next_commit_after(1);
    let error = core.request(add.clone()).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::StorageUnavailable);
    assert!(error.retryable());
    assert!(
        error.message().starts_with("тестовый сбой: "),
        "{}",
        error.message()
    );
    assert_eq!(snapshot(&core).await.unwrap(), before);
    assert_eq!(events.try_recv().unwrap_err(), TryRecvError::Empty);
    assert_eq!(store.count("projects").unwrap(), 0);
    assert_eq!(store.count("project_roots").unwrap(), 0);
    assert_eq!(store.revision_in_db().unwrap(), 1);

    let reply = core.request(add).await.unwrap();
    assert_eq!(reply.revision, Revision(2));
    assert_eq!(events.try_recv().unwrap().revision, Revision(2));
    let after = snapshot(&core).await.unwrap();
    assert_eq!(after.state.projects.len(), 1);
    assert_eq!(after.state.roots.len(), 1);
    core.shutdown().await;

    let reloaded = store.load().unwrap();
    assert_eq!(reloaded.revision, after.revision);
    assert_eq!(reloaded.state, *after.state);
}

fn open_panel(plugin: &str, workbench: WorkbenchId, count: u32) -> Command {
    Command::PanelOpen {
        plugin: plugin.into(),
        workbench: Some(workbench),
        zone: None,
        args: Some(serde_json::json!({"count": count})),
    }
}

fn opened(reply: &Reply) -> TestResult<(PanelId, OpenOutcome)> {
    match created(reply)? {
        CommandValue::Opened { panel, outcome, .. } => Ok((panel, outcome)),
        other => Err(format!("ожидался исход panel.open, получено {other:?}").into()),
    }
}

/// Переезд `PerApp` пишет одной записью `PanelRehome`, две раскладки, данные и `opened_from`.
/// Отказ после любого её оператора оставляет базу прежней.
#[tokio::test]
async fn commit_fault_mid_panel_move_keeps_database_and_retry_moves_once() -> TestResult {
    let store = Store::new()?;
    let clock = ManualClock::new(NOW);
    let persist = store.persist()?;
    let fault = persist.commit_fault();
    let registry = registry_of(vec![
        TestPlugin::new("field")
            .cardinality(Cardinality::PerApp)
            .actions(),
    ]);
    let core = start_with(persist, &clock, resolver(), registry)?;
    let workspace = active(&snapshot(&core).await?)?;
    let project = add_project(&core, workspace, REPO, None).await?;
    let from = create_workbench(&core, project, "a").await?;
    let to = create_workbench(&core, project, "b").await?;
    let (panel, outcome) = opened(&request(&core, open_panel("field", from, 1)).await?)?;
    assert_eq!(outcome, OpenOutcome::Created);
    let (before, mut events) = core.subscribe().await?;
    let caller = before.state.layouts[&to].focused().ok_or("нет фокуса")?;
    assert_ne!(before.state.opened_from.get(&panel), Some(&caller));

    // Операторы записи: `PanelRehome`, две раскладки, данные, `opened_from` и ревизия.
    const STATEMENTS: usize = 6;
    for statements in 0..STATEMENTS {
        fault.fail_next_commit_after(statements);
        let error = core.request(open_panel("field", to, 2)).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::StorageUnavailable, "{statements}");
        assert_eq!(snapshot(&core).await?, before, "сбой после {statements}");
        assert_eq!(events.try_recv().unwrap_err(), TryRecvError::Empty);
        let stored = store.load()?;
        assert_eq!(stored.revision, before.revision, "сбой после {statements}");
        assert_eq!(stored.state, *before.state, "сбой после {statements}");
    }

    fault.fail_next_commit_after(STATEMENTS);
    let reply = request(&core, open_panel("field", to, 2)).await?;
    assert_eq!(opened(&reply)?, (panel, OpenOutcome::Moved));
    assert_eq!(reply.revision, before.revision.next());
    let event = events.try_recv()?;
    assert_eq!(event.changes.len(), STATEMENTS - 1, "{:?}", event.changes);
    let after = snapshot(&core).await?;
    core.shutdown().await;

    let reloaded = store.load()?;
    assert_eq!(reloaded.revision, after.revision);
    assert_eq!(reloaded.state, *after.state);
    assert_eq!(reloaded.state.panels[&panel].workbench, to);
    assert_eq!(reloaded.state.layouts[&from].locate(panel), None);
    assert!(reloaded.state.layouts[&to].locate(panel).is_some());
    assert_eq!(reloaded.state.opened_from.get(&panel), Some(&caller));
    assert_eq!(
        reloaded.state.panel_data.get(&panel),
        Some(&PanelData::Text {
            version: 1,
            raw: r#"{"count":2}"#.into(),
        })
    );
    Ok(())
}

/// Отказ записи посреди `panel.open` singleton и успешный повтор: одна панель, её данные,
/// раскладка и одна прибавка ревизии, в том числе после повторного открытия базы.
#[tokio::test]
async fn commit_fault_mid_singleton_creation_then_retry_gives_one_panel() -> TestResult {
    let store = Store::new()?;
    let clock = ManualClock::new(NOW);
    let persist = store.persist()?;
    let fault = persist.commit_fault();
    let registry = registry_of(vec![
        TestPlugin::new("rows")
            .cardinality(Cardinality::PerWorkbench)
            .actions(),
    ]);
    let core = start_with(persist, &clock, resolver(), Arc::clone(&registry))?;
    let workspace = active(&snapshot(&core).await?)?;
    let project = add_project(&core, workspace, REPO, None).await?;
    let workbench = create_workbench(&core, project, "a").await?;
    let before = snapshot(&core).await?;
    let lists = |state: &State| -> Vec<PanelId> {
        state
            .workbench_panels(workbench)
            .filter(|panel| panel.plugin.as_str() == "rows")
            .map(|panel| panel.id)
            .collect()
    };

    // Запись создания: панель, данные, раскладка и ревизия. Сбой после панели и данных.
    fault.fail_next_commit_after(2);
    let error = core
        .request(open_panel("rows", workbench, 3))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::StorageUnavailable);
    assert_eq!(snapshot(&core).await?, before);
    assert_eq!(store.count("panel_states")?, store.count("panels")?);
    assert_eq!(lists(&store.load()?.state), Vec::new());

    let reply = request(&core, open_panel("rows", workbench, 3)).await?;
    let (panel, outcome) = opened(&reply)?;
    assert_eq!(outcome, OpenOutcome::Created);
    assert_eq!(reply.revision, before.revision.next());
    // Повтор находит созданную панель и ревизию не прибавляет.
    let again = request(&core, open_panel("rows", workbench, 3)).await?;
    assert_eq!(opened(&again)?, (panel, OpenOutcome::Focused));
    assert_eq!(again.revision, before.revision.next());
    let after = snapshot(&core).await?;
    core.shutdown().await;

    let reloaded = store.load()?;
    assert_eq!(reloaded.revision, before.revision.next());
    assert_eq!(reloaded.state, *after.state);
    assert_eq!(lists(&reloaded.state), [panel]);
    assert!(reloaded.state.layouts[&workbench].locate(panel).is_some());
    assert_eq!(
        reloaded.state.panel_data.get(&panel),
        Some(&PanelData::Text {
            version: 1,
            raw: r#"{"count":3}"#.into(),
        })
    );

    // Следующий запуск на той же базе находит ту же панель и ничего не пишет.
    let core = start_with(store.persist()?, &clock, resolver(), registry)?;
    let restarted = snapshot(&core).await?;
    assert_eq!(restarted.revision, before.revision.next());
    let again = request(&core, open_panel("rows", workbench, 3)).await?;
    assert_eq!(opened(&again)?, (panel, OpenOutcome::Focused));
    assert_eq!(again.revision, before.revision.next());
    core.shutdown().await;
    Ok(())
}

/// Унаследованные дубликаты на настоящей базе: две панели создал реестр, где плагин `Multi`,
/// следующий запуск считает его `PerApp`. Команда дубликату отказывает, закрытие канонического
/// передаёт право той же записью, холодный запуск после этого ничего не пишет.
#[tokio::test]
async fn legacy_duplicate_takes_the_right_after_close_and_survives_cold_start() -> TestResult {
    let store = Store::new()?;
    let clock = ManualClock::new(NOW);
    let multi = registry_of(vec![TestPlugin::new("field").actions()]);
    let per_app = registry_of(vec![
        TestPlugin::new("field")
            .cardinality(Cardinality::PerApp)
            .actions(),
    ]);
    let core = start_with(store.persist()?, &clock, resolver(), multi)?;
    let workspace = active(&snapshot(&core).await?)?;
    let project = add_project(&core, workspace, REPO, None).await?;
    let first = create_workbench(&core, project, "a").await?;
    let second = create_workbench(&core, project, "b").await?;
    let (old, _) = opened(&request(&core, open_panel("field", first, 1)).await?)?;
    clock.advance(10);
    let (young, _) = opened(&request(&core, open_panel("field", second, 2)).await?)?;
    core.shutdown().await;

    let core = start_with(store.persist()?, &clock, resolver(), Arc::clone(&per_app))?;
    let restored = snapshot(&core).await?;
    assert_eq!(
        restored.state.status_of(old),
        shellio_core::PanelStatus::Ready
    );
    assert_eq!(
        restored.state.status_of(young),
        shellio_core::PanelStatus::Duplicate
    );
    let command = |panel| Command::PanelCommand {
        panel,
        name: "field.add".into(),
        args: Some(serde_json::json!({"by": 1})),
    };
    let error = core.request(command(young)).await.unwrap_err();
    assert_eq!(error.detail_str("reason"), Some("panel_unavailable"));
    assert_eq!(error.detail_str("status"), Some("duplicate"));
    // Открытие в рабочей области дубликата переносит канонический экземпляр.
    let reply = request(&core, open_panel("field", second, 1)).await?;
    assert_eq!(opened(&reply)?, (old, OpenOutcome::Moved));
    // Закрытие канонического: дубликат получает право, его байты прежние. Статус приходит
    // дельтой того же события: снимок и событие дают состояние актора.
    let (base, mut events) = core.subscribe().await?;
    request(&core, Command::PanelClose { panel: old }).await?;
    let event = events.try_recv()?;
    assert!(
        event.changes.contains(&Change::PanelStatus {
            panel: young,
            status: shellio_core::PanelStatus::Ready,
        }),
        "{:?}",
        event.changes
    );
    let mut mirror = (*base.state).clone();
    mirror.apply_all(&event.changes);
    let after = snapshot(&core).await?;
    assert_eq!(*after.state, mirror, "зеркало разошлось с актором");
    assert_eq!(
        after.state.panel_data.get(&young),
        base.state.panel_data.get(&young)
    );
    assert_eq!(
        after.state.status_of(young),
        shellio_core::PanelStatus::Ready
    );
    request(&core, command(young)).await?;
    let reply = request(&core, open_panel("field", first, 3)).await?;
    assert_eq!(opened(&reply)?, (young, OpenOutcome::Moved));
    let last = snapshot(&core).await?;
    core.shutdown().await;

    // Холодный запуск: тот же экземпляр готов, записи запуска нет.
    let core = start_with(store.persist()?, &clock, resolver(), per_app)?;
    let cold = snapshot(&core).await?;
    assert_eq!(cold.revision, last.revision);
    assert_eq!(*cold.state, *last.state);
    assert_eq!(
        cold.state.status_of(young),
        shellio_core::PanelStatus::Ready
    );
    assert_eq!(
        cold.state.panel_data.get(&young),
        Some(&PanelData::Text {
            version: 1,
            raw: r#"{"count":3}"#.into(),
        })
    );
    core.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn foreign_write_while_running_is_storage_conflict_and_changes_nothing() {
    let store = Store::new().unwrap();
    let clock = ManualClock::new(NOW);
    let core = start(store.persist().unwrap(), &clock, resolver()).unwrap();
    let (before, mut events) = core.subscribe().await.unwrap();
    let workspace = active(&before).unwrap();

    // Ревизию сдвинули в обход ядра: базовая ревизия записи уже чужая.
    store
        .raw()
        .unwrap()
        .execute("UPDATE meta SET value = 7 WHERE key = 'revision'", [])
        .unwrap();
    let error = core
        .request(Command::WorkspaceRename {
            workspace,
            title: "Работа".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::StorageConflict, "{error}");
    assert!(!error.retryable());
    assert_eq!(snapshot(&core).await.unwrap(), before);
    assert_eq!(events.try_recv().unwrap_err(), TryRecvError::Empty);
    core.shutdown().await;

    assert_eq!(store.revision_in_db().unwrap(), 7);
    let title: String = store
        .raw()
        .unwrap()
        .query_row("SELECT title FROM workspaces", [], |row| row.get(0))
        .unwrap();
    assert_eq!(title, BOOTSTRAP_WORKSPACE);
}

fn title(name: &str) -> TestResult<Title> {
    Ok(Title::new(name)?)
}

/// Одна запись из шести операторов дельт и оператора ревизии.
fn seven_statement_record(ids: &mut dyn IdGen) -> TestResult<Vec<Change>> {
    let workspace = WorkspaceId::generate(ids);
    let project = ProjectId::generate(ids);
    let workbench = WorkbenchId::generate(ids);
    let root = |ids: &mut dyn IdGen, path: &str, kind, key: &str| ProjectRoot {
        id: ProjectRootId::generate(ids),
        project,
        path: PathBuf::from(path),
        kind,
        key: RootKey::new(key),
    };
    Ok(vec![
        Change::WorkspaceInserted {
            workspace: Workspace {
                id: workspace,
                title: title("Работа")?,
                position: 0,
                last_workbench: None,
                deleted_at: None,
            },
        },
        Change::ProjectInserted {
            project: Project {
                id: project,
                workspace,
                title: title("repo")?,
                list_position: Some(0),
                removed_at: None,
                last_opened_at: None,
            },
            roots: vec![
                root(ids, REPO, RootKind::Git, REPO_KEY),
                root(ids, "/src/docs", RootKind::Folder, "/src/docs"),
            ],
        },
        Change::WorkbenchInserted {
            workbench: Workbench {
                id: workbench,
                project,
                title: title("main")?,
                position: 0,
            },
        },
        Change::WindowSet {
            active: workspace,
            route: Route::Workbench(workbench),
        },
    ])
}

/// Запись запуска несёт отпечаток реестра той же транзакцией: отказ в любой точке, в том числе
/// между оператором отпечатка и оператором ревизии, оставляет базу и отпечаток прежними.
#[test]
fn commit_fault_between_fingerprint_and_revision_keeps_database_and_fingerprint() {
    // Шесть операторов дельт, оператор отпечатка и оператор ревизии.
    const BEFORE_REVISION: usize = 7;
    let store = Store::new().unwrap();
    let mut persist = store.persist().unwrap();
    let fault = persist.commit_fault();
    persist
        .commit(Revision::ZERO, &[], Some("прежний"))
        .unwrap();
    let before = store.load().unwrap();
    assert_eq!(before.revision, Revision(1));
    assert_eq!(before.fingerprint.as_deref(), Some("прежний"));
    let changes = seven_statement_record(&mut SequentialIds::new()).unwrap();

    for statements in 0..=BEFORE_REVISION {
        fault.fail_next_commit_after(statements);
        let error = persist
            .commit(Revision(1), &changes, Some("новый"))
            .unwrap_err();
        assert_eq!(
            error,
            CommitError::Unavailable(format!(
                "тестовый сбой: запись прервана после операторов: {statements}"
            ))
        );
        // Повторное открытие базы отдельным соединением.
        assert_eq!(store.load().unwrap(), before, "сбой после {statements}");
        assert_eq!(store.revision_in_db().unwrap(), 1);
        let stored: String = store
            .raw()
            .unwrap()
            .query_row(
                "SELECT fingerprint FROM plugin_registry WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "прежний", "сбой после {statements}");
        assert_eq!(store.count("workspaces").unwrap(), 0);
    }

    // Записи хватило операторов: дельты, отпечаток и ревизия записаны вместе.
    fault.fail_next_commit_after(BEFORE_REVISION + 1);
    persist
        .commit(Revision(1), &changes, Some("новый"))
        .unwrap();
    let written = store.load().unwrap();
    assert_eq!(written.revision, Revision(2));
    assert_eq!(written.fingerprint.as_deref(), Some("новый"));
    assert_eq!(store.count("workspaces").unwrap(), 1);
}

#[test]
fn commit_fault_after_each_statement_leaves_no_partial_record() {
    const STATEMENTS: usize = 7;
    let store = Store::new().unwrap();
    let mut persist = store.persist().unwrap();
    let fault = persist.commit_fault();
    let changes = seven_statement_record(&mut SequentialIds::new()).unwrap();
    let before = store.load().unwrap();
    assert_eq!(before.revision, Revision::ZERO);

    for statements in 0..STATEMENTS {
        fault.fail_next_commit_after(statements);
        let error = persist.commit(Revision::ZERO, &changes, None).unwrap_err();
        assert_eq!(
            error,
            CommitError::Unavailable(format!(
                "тестовый сбой: запись прервана после операторов: {statements}"
            ))
        );
        assert_eq!(store.load().unwrap(), before, "сбой после {statements}");
        for table in ["workspaces", "projects", "project_roots", "workbenches"] {
            assert_eq!(
                store.count(table).unwrap(),
                0,
                "{table}, сбой после {statements}"
            );
        }
        assert_eq!(store.revision_in_db().unwrap(), 0);
    }

    // Записи хватило операторов: крючок снят, запись прошла.
    fault.fail_next_commit_after(STATEMENTS);
    persist.commit(Revision::ZERO, &changes, None).unwrap();
    let mut expected = before.state.clone();
    expected.apply_all(&changes);
    let written = store.load().unwrap();
    assert_eq!(written.revision, Revision(1));
    assert_eq!(written.state, expected);

    // Крючок снят и следующей записи не мешает.
    let Change::WorkspaceInserted { workspace } = &changes[0] else {
        panic!("первая дельта — воркспейс");
    };
    persist
        .commit(
            Revision(1),
            &[Change::WorkspaceRenamed {
                workspace: workspace.id,
                title: title("Работа 2").unwrap(),
            }],
            None,
        )
        .unwrap();
    assert_eq!(store.revision_in_db().unwrap(), 2);
}

/// Генератор SplitMix64 с фиксированным зерном.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

/// Ожидаемый исход команды по зеркалу: запись с новой ревизией, пустое изменение или отказ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    Written,
    Unchanged,
    Refused(ErrorCode),
}

const TITLES: &[&str] = &[
    "Альфа",
    "альфа",
    "Бета",
    " Бета ",
    "Gamma",
    "GAMMA",
    "alpha",
    "repo",
    "Основной",
];
const PATHS: &[&str] = &[
    "/src/alpha",
    "/src/beta",
    "/src/gamma",
    "/work/alpha",
    REPO,
    WORKTREE,
];
const RELATIVE: &str = "src/alpha";

/// Команды над текущим состоянием: цели чаще живые, имена и пути — из малых наборов, поэтому
/// совпадения имён и корней возникают сами. Часть команд заведомо ошибочна.
struct Generator {
    rng: Rng,
    resolver: FakeRootResolver,
}

impl Generator {
    /// Живая цель или `None` — неизвестный ID.
    fn target<T: Copy>(&mut self, live: &[T]) -> Option<T> {
        if live.is_empty() || self.rng.chance(15) {
            None
        } else {
            Some(self.rng.pick(live))
        }
    }

    /// Имя: из набора или заведомо неверное (`None`).
    fn title(&mut self) -> TestResult<(String, Option<Title>)> {
        if self.rng.chance(15) {
            let bad = match self.rng.below(5) {
                0 => String::new(),
                1 => " \t ".to_owned(),
                2 => "a\u{1}b".to_owned(),
                3 => "Имя\nв две строки".to_owned(),
                _ => "я".repeat(201),
            };
            Ok((bad, None))
        } else {
            let raw = self.rng.pick(TITLES);
            Ok((raw.to_owned(), Some(title(raw)?)))
        }
    }

    fn command(&mut self, state: &State) -> TestResult<(Command, Expect)> {
        use Expect::{Refused, Unchanged, Written};

        let workspaces: Vec<_> = state
            .workspaces
            .values()
            .filter(|workspace| !workspace.is_deleted())
            .map(|workspace| workspace.id)
            .collect();
        let projects: Vec<_> = state
            .projects
            .keys()
            .copied()
            .filter(|&id| state.live_project(id).is_some())
            .collect();
        let workbenches: Vec<_> = state
            .workbenches
            .keys()
            .copied()
            .filter(|&id| state.live_workbench(id).is_some())
            .collect();
        let unknown_workspace = WorkspaceId::from_bytes([0xee; 16]);
        let unknown_project = ProjectId::from_bytes([0xee; 16]);
        let unknown_workbench = WorkbenchId::from_bytes([0xee; 16]);
        let workspace_taken = |title: &Title, except: Option<WorkspaceId>| {
            state.workspaces.values().any(|other| {
                Some(other.id) != except && !other.is_deleted() && other.title.same_name(title)
            })
        };
        let workbench_taken = |project: ProjectId, title: &Title, except: Option<WorkbenchId>| {
            state.workbenches.values().any(|other| {
                Some(other.id) != except && other.project == project && other.title.same_name(title)
            })
        };

        Ok(match self.rng.below(100) {
            0..10 => {
                let (raw, title) = self.title()?;
                let expect = match title {
                    None => Refused(ErrorCode::InvalidArgument),
                    Some(title) if workspace_taken(&title, None) => Refused(ErrorCode::NameTaken),
                    Some(_) => Written,
                };
                (Command::WorkspaceCreate { title: raw }, expect)
            }
            10..20 => {
                let target = self.target(&workspaces);
                let (raw, title) = self.title()?;
                let expect = match (target.and_then(|id| state.live_workspace(id)), title) {
                    (None, _) => Refused(ErrorCode::NotFound),
                    (Some(_), None) => Refused(ErrorCode::InvalidArgument),
                    (Some(record), Some(title)) if record.title == title => Unchanged,
                    (Some(record), Some(title)) if workspace_taken(&title, Some(record.id)) => {
                        Refused(ErrorCode::NameTaken)
                    }
                    (Some(_), Some(_)) => Written,
                };
                let workspace = target.unwrap_or(unknown_workspace);
                (
                    Command::WorkspaceRename {
                        workspace,
                        title: raw,
                    },
                    expect,
                )
            }
            20..42 => {
                let target = self.target(&workspaces);
                let relative = self.rng.chance(8);
                let path = PathBuf::from(if relative {
                    RELATIVE
                } else {
                    self.rng.pick(PATHS)
                });
                let (raw, title) = if self.rng.chance(40) {
                    let (raw, title) = self.title()?;
                    (Some(raw), title)
                } else {
                    (None, Some(default_title(&path)?))
                };
                let expect = if relative {
                    Refused(ErrorCode::InvalidArgument)
                } else if let Some(workspace) = target {
                    let key = self.resolver.resolve(&path)?.key;
                    match title {
                        None => Refused(ErrorCode::InvalidArgument),
                        Some(_) if state.roots.values().any(|root| root.key == key) => {
                            Refused(ErrorCode::RootTaken)
                        }
                        Some(title)
                            if state.projects.values().any(|project| {
                                project.workspace == workspace
                                    && !project.is_removed()
                                    && project.title.same_name(&title)
                            }) =>
                        {
                            Refused(ErrorCode::NameTaken)
                        }
                        Some(_) => Written,
                    }
                } else {
                    Refused(ErrorCode::NotFound)
                };
                let workspace = target.unwrap_or(unknown_workspace);
                (
                    Command::ProjectAdd {
                        workspace,
                        path,
                        title: raw,
                    },
                    expect,
                )
            }
            42..62 => {
                let target = self.target(&projects);
                let (raw, title) = self.title()?;
                let expect = match (target, title) {
                    (None, _) => Refused(ErrorCode::NotFound),
                    (Some(_), None) => Refused(ErrorCode::InvalidArgument),
                    (Some(project), Some(title)) if workbench_taken(project, &title, None) => {
                        Refused(ErrorCode::NameTaken)
                    }
                    (Some(_), Some(_)) => Written,
                };
                let project = target.unwrap_or(unknown_project);
                (
                    Command::WorkbenchCreate {
                        project,
                        title: raw,
                    },
                    expect,
                )
            }
            62..72 => {
                let target = self.target(&workbenches);
                let (raw, title) = self.title()?;
                let expect = match (target.and_then(|id| state.live_workbench(id)), title) {
                    (None, _) => Refused(ErrorCode::NotFound),
                    (Some(_), None) => Refused(ErrorCode::InvalidArgument),
                    (Some(record), Some(title)) if record.title == title => Unchanged,
                    (Some(record), Some(title))
                        if workbench_taken(record.project, &title, Some(record.id)) =>
                    {
                        Refused(ErrorCode::NameTaken)
                    }
                    (Some(_), Some(_)) => Written,
                };
                let workbench = target.unwrap_or(unknown_workbench);
                (
                    Command::WorkbenchRename {
                        workbench,
                        title: raw,
                    },
                    expect,
                )
            }
            72..84 => {
                let target = self.target(&workbenches);
                // Уже открытая область: окно на ней и она последняя в своём воркспейсе.
                let expect = target.map_or(Refused(ErrorCode::NotFound), |workbench| {
                    let workspace = state.workbench_workspace(workbench);
                    let open = state.window.active_workspace == workspace
                        && state.window.route == Route::Workbench(workbench)
                        && workspace.is_some_and(|id| {
                            state.workspaces[&id].last_workbench == Some(workbench)
                        });
                    if open { Unchanged } else { Written }
                });
                let workbench = target.unwrap_or(unknown_workbench);
                (Command::WorkbenchOpen { workbench }, expect)
            }
            92.. => self.layout_command(state)?,
            _ => {
                let target = self.target(&workspaces);
                let expect = target.map_or(Refused(ErrorCode::NotFound), |workspace| {
                    if state.window.active_workspace == Some(workspace) {
                        Unchanged
                    } else {
                        Written
                    }
                });
                let workspace = target.unwrap_or(unknown_workspace);
                (Command::WindowFocusWorkspace { workspace }, expect)
            }
        })
    }
}

impl Generator {
    /// Команда панели: закрытие, перенос в зону или выбор вкладки. Исход предсказывает та же
    /// чистая операция раскладки: тест сверяет запись, а не модель.
    fn layout_command(&mut self, state: &State) -> TestResult<(Command, Expect)> {
        use Expect::{Refused, Unchanged, Written};

        let live: Vec<PanelId> = state
            .panels
            .values()
            .filter(|panel| state.live_workbench(panel.workbench).is_some())
            .map(|panel| panel.id)
            .collect();
        let target = self.target(&live);
        let panel = target.unwrap_or(PanelId::from_bytes([0xee; 16]));
        let layout = target
            .and_then(|panel| state.layouts.get(&state.panels[&panel].workbench))
            .cloned();
        let changed = |next: Result<shellio_core::WorkbenchLayout, _>| match (&layout, next) {
            (Some(layout), Ok(next)) if next == *layout => Ok(Unchanged),
            (Some(_), Ok(_)) => Ok(Written),
            (_, Err(error)) => Err(format!("операция модели отказала: {error:?}")),
            (None, Ok(_)) => Err("нет раскладки".to_owned()),
        };
        let Some(current) = layout.clone() else {
            return Ok((Command::PanelSelect { panel }, Refused(ErrorCode::NotFound)));
        };
        Ok(match self.rng.below(3) {
            0 => (Command::PanelClose { panel }, Written),
            1 => {
                let zone = self.rng.pick(&Zone::ALL);
                let navigator = shellio_core::is_navigator(&state.panels[&panel].plugin);
                (
                    Command::PanelMove { panel, zone },
                    changed(current.move_to_zone(panel, zone, navigator))?,
                )
            }
            _ => (
                Command::PanelSelect { panel },
                changed(current.select(panel))?,
            ),
        })
    }
}

fn default_title(path: &Path) -> TestResult<Title> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("у пути {} нет имени", path.display()))?;
    title(name)
}

/// Исчерпывающий `match`: новая дельта не соберётся, пока тест её не учтёт.
const fn change_name(change: &Change) -> &'static str {
    match change {
        Change::WorkspaceInserted { .. } => "workspace_inserted",
        Change::WorkspaceRenamed { .. } => "workspace_renamed",
        Change::WorkspaceLastWorkbenchSet { .. } => "workspace_last_workbench_set",
        Change::ProjectInserted { .. } => "project_inserted",
        Change::ProjectOpened { .. } => "project_opened",
        Change::WorkbenchInserted { .. } => "workbench_inserted",
        Change::WorkbenchRenamed { .. } => "workbench_renamed",
        Change::WindowSet { .. } => "window_set",
        Change::PanelAdd { .. } => "panel_add",
        Change::PanelRemove { .. } => "panel_remove",
        Change::PanelRehome { .. } => "panel_rehome",
        Change::PanelData { .. } => "panel_data",
        Change::PanelOpenedFrom { .. } => "panel_opened_from",
        Change::PanelStatus { .. } => "panel_status",
        Change::LayoutSet { .. } => "layout_set",
    }
}

/// Что встретилось за все зёрна: без этого генератор мог бы выродиться в одни отказы.
#[derive(Default)]
struct Coverage {
    written: BTreeSet<&'static str>,
    changes: BTreeSet<&'static str>,
    refused: BTreeSet<&'static str>,
    unchanged: usize,
}

impl Coverage {
    fn assert_complete(&self) {
        assert_eq!(
            self.written,
            BTreeSet::from([
                "panel.close",
                "panel.move",
                "panel.select",
                "project.add",
                "window.focus_workspace",
                "workbench.create",
                "workbench.open",
                "workbench.rename",
                "workspace.create",
                "workspace.rename",
            ]),
            "команды с записью"
        );
        // Все дельты, кроме переезда панели, вызывающей панели и статуса: их пишет `panel.open`.
        assert_eq!(self.changes.len(), 12, "дельты: {:?}", self.changes);
        assert_eq!(
            self.refused,
            BTreeSet::from(["invalid_argument", "name_taken", "not_found", "root_taken"]),
            "отказы"
        );
        assert!(self.unchanged > 0, "ни одного пустого изменения");
    }
}

async fn run_seed(seed: u64, coverage: &mut Coverage) -> TestResult {
    let store = Store::new()?;
    let clock = ManualClock::new(NOW);
    let mut generator = Generator {
        rng: Rng(seed),
        resolver: resolver(),
    };
    let core = start(store.persist()?, &clock, generator.resolver.clone())?;
    let (initial, mut events) = core.subscribe().await?;
    let mut mirror = (*initial.state).clone();
    let mut revision = initial.revision;

    for step in 0..STEPS {
        clock.advance(1_000);
        let (command, expect) = generator.command(&mirror)?;
        let at = format!("зерно {seed}, шаг {step}, {command:?}");
        let name = command.name();
        let result = core.request(command).await;
        match (expect, result) {
            (Expect::Refused(code), Err(error)) => {
                assert_eq!(error.code(), code, "{at}: {error:?}");
                coverage.refused.insert(code.as_str());
            }
            (Expect::Unchanged, Ok(reply)) => {
                assert_eq!(reply.revision, revision, "{at}: ожидалось пустое изменение");
                coverage.unchanged += 1;
            }
            (Expect::Written, Ok(reply)) => {
                assert_eq!(reply.revision, revision.next(), "{at}: ожидалась запись");
                coverage.written.insert(name);
            }
            (expect, result) => panic!("{at}: ожидалось {expect:?}, получено {result:?}"),
        }
        // Событие уходит подписчикам до ответа, поэтому оно уже в канале.
        match events.try_recv() {
            Ok(event) => {
                assert_eq!(event.revision, revision.next(), "{at}");
                for change in &event.changes {
                    coverage.changes.insert(change_name(change));
                }
                mirror.apply_all(&event.changes);
                revision = event.revision;
            }
            Err(TryRecvError::Empty) => {}
            Err(error) => panic!("{at}: {error}"),
        }
        assert!(
            matches!(events.try_recv(), Err(TryRecvError::Empty)),
            "{at}: лишнее событие"
        );

        let actor = snapshot(&core).await?;
        assert_eq!(actor.revision, revision, "{at}");
        assert_eq!(*actor.state, mirror, "{at}: зеркало разошлось с актором");
        assert_eq!(actor.state.check_invariants(), Vec::new(), "{at}");
    }

    let actor = snapshot(&core).await?;
    core.shutdown().await;
    let reloaded = store.load()?;
    assert_eq!(reloaded.revision, actor.revision, "зерно {seed}");
    assert_eq!(reloaded.state, *actor.state, "зерно {seed}: перезагрузка");
    assert_eq!(reloaded.state, mirror, "зерно {seed}: зеркало");
    Ok(())
}

#[tokio::test]
async fn random_sequences_keep_actor_database_and_mirror_equal() {
    let mut coverage = Coverage::default();
    for seed in 0..SEEDS {
        run_seed(seed, &mut coverage)
            .await
            .unwrap_or_else(|error| panic!("зерно {seed}: {error}"));
    }
    coverage.assert_complete();
}
