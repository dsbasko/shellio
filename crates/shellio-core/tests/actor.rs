//! Актор ядра и `CoreHandle` на хранилище в памяти с отказами по плану теста.
//!
//! Синхронизация — только квитанции, каналы и барьер записи: поток хранения сообщает,
//! что дошёл до барьера, и ждёт, пока тест его отпустит.

use std::error::Error;
use std::sync::Arc;

use shellio_core::test_support::{
    FakeRootResolver, Faults, FaultyPersist, ManualClock, MemoryPersist, SequentialIds, TestPlugin,
    registry, registry_of,
};
use shellio_core::{
    BOOTSTRAP_WORKSPACE, Change, Command, CommandValue, Core, CoreError, CoreHandle, EVENT_BUFFER,
    ErrorCode, Event, EventsSince, IdGen, NAVIGATOR_PLUGIN, Project, ProjectId, QUEUE_CAPACITY,
    Query, ReplyValue, Revision, RootKey, RootKind, Route, SUBSCRIBER_CAPACITY, Snapshot,
    StartError, State, StatusView, TERMINAL_PLUGIN, Title, TreeRoot, TreeScope, TreeView,
    UnixMillis, WindowState, Workbench, WorkbenchId, Workspace, WorkspaceId, fingerprint,
};
use shellio_core::{OpenOutcome, PanelData, PanelId, Reply};
use shellio_layout::Viewport;
use shellio_plugin_api::Cardinality;
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::oneshot;

type TestResult<T> = Result<T, Box<dyn Error>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);

fn start(persist: FaultyPersist) -> Result<CoreHandle, StartError> {
    Core::start(
        &Handle::current(),
        Box::new(persist),
        Box::new(ManualClock::new(NOW)),
        Box::new(SequentialIds::new()),
        Arc::new(FakeRootResolver::new()),
        registry(),
    )
}

/// Ядро на `memory` и пульт его отказов.
fn start_on(memory: &MemoryPersist) -> Result<(CoreHandle, Faults), StartError> {
    let (persist, faults) = FaultyPersist::new(memory.clone());
    Ok((start(persist)?, faults))
}

async fn snapshot(core: &CoreHandle) -> Result<Snapshot, CoreError> {
    Ok(core.subscribe().await?.0)
}

/// Активный воркспейс по снимку.
async fn active(core: &CoreHandle) -> TestResult<WorkspaceId> {
    snapshot(core)
        .await?
        .state
        .window
        .active_workspace
        .ok_or_else(|| "нет активного воркспейса".into())
}

fn rename(workspace: WorkspaceId, title: &str) -> Command {
    Command::WorkspaceRename {
        workspace,
        title: title.into(),
    }
}

/// Имя, отличное от предыдущего в последовательности: каждая команда меняет состояние.
fn alternating(index: usize) -> &'static str {
    if index.is_multiple_of(2) {
        "Альфа"
    } else {
        "Бета"
    }
}

fn create(title: &str) -> Command {
    Command::WorkspaceCreate {
        title: title.into(),
    }
}

fn events(since: EventsSince) -> TestResult<Vec<Event>> {
    match since {
        EventsSince::Events(events) => Ok(events),
        EventsSince::Reset(snapshot) => Err(format!(
            "ожидались события, получен снимок ревизии {:?}",
            snapshot.revision
        )
        .into()),
    }
}

fn revisions(events: &[Event]) -> Vec<u64> {
    events.iter().map(|event| event.revision.0).collect()
}

fn titles(state: &State) -> Vec<String> {
    state
        .workspaces
        .values()
        .map(|workspace| workspace.title.as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn start_on_empty_state_bootstraps_one_workspace() {
    let memory = MemoryPersist::new();
    let (core, faults) = start_on(&memory).unwrap();

    let snapshot = snapshot(&core).await.unwrap();
    assert_eq!(snapshot.revision, Revision(1));
    assert_eq!(faults.commits(), 1);
    assert_eq!(memory.revision(), Revision(1));
    assert_eq!(*snapshot.state, memory.state());

    let workspace = active(&core).await.unwrap();
    assert_eq!(titles(&snapshot.state), [BOOTSTRAP_WORKSPACE]);
    assert_eq!(snapshot.state.window.route, Route::WorkspaceHome);
    assert_eq!(snapshot.state.check_invariants(), Vec::new());

    assert_eq!(snapshot.state.window.active_workspace, Some(workspace));

    // Отпечатка реестра в новой базе нет: запись запуска несёт его и в буфер не попадает,
    // поэтому курсор 0 получает снимок, а не событие.
    assert_eq!(memory.fingerprint(), Some(fingerprint(&registry())));
    assert_eq!(
        core.events_since(Revision::ZERO).await.unwrap(),
        EventsSince::Reset(snapshot)
    );
}

#[tokio::test]
async fn start_on_saved_state_writes_nothing_and_resets_old_cursors() {
    let memory = MemoryPersist::new();
    let (core, _) = start_on(&memory).unwrap();
    core.request(create("Работа")).await.unwrap();
    core.shutdown().await;
    let before = memory.state();

    let (core, faults) = start_on(&memory).unwrap();
    let snapshot = snapshot(&core).await.unwrap();
    assert_eq!(faults.commits(), 0);
    assert_eq!(snapshot.revision, Revision(2));
    assert_eq!(*snapshot.state, before);

    assert_eq!(
        core.events_since(Revision(2)).await.unwrap(),
        EventsSince::Events(Vec::new())
    );
    assert_eq!(
        core.events_since(Revision(1)).await.unwrap(),
        EventsSince::Reset(snapshot)
    );
}

#[tokio::test]
async fn start_focuses_first_workspace_when_active_is_missing() {
    let mut ids = SequentialIds::new();
    let ids: &mut dyn IdGen = &mut ids;
    let mut state = State::default();
    for (position, title) in [(1, "Дом"), (0, "Работа")] {
        let id = WorkspaceId::generate(ids);
        state.workspaces.insert(
            id,
            Workspace {
                id,
                title: Title::new(title).unwrap(),
                position,
                last_workbench: None,
                deleted_at: None,
            },
        );
    }
    let first = state.workspaces_in_order()[0].id;
    let memory = MemoryPersist::with_state(state, Revision(3));

    let (core, faults) = start_on(&memory).unwrap();
    let snapshot = snapshot(&core).await.unwrap();
    assert_eq!(faults.commits(), 1);
    assert_eq!(snapshot.revision, Revision(4));
    assert_eq!(snapshot.state.window.active_workspace, Some(first));
    assert_eq!(snapshot.state.workspaces.len(), 2);
    assert_eq!(*snapshot.state, memory.state());
}

/// Состояние базы v1: открытая рабочая область без панелей и раскладки.
fn open_workbench_without_layout() -> TestResult<(State, WorkbenchId)> {
    let mut ids = SequentialIds::new();
    let ids: &mut dyn IdGen = &mut ids;
    let workspace = WorkspaceId::generate(ids);
    let project = ProjectId::generate(ids);
    let workbench = WorkbenchId::generate(ids);
    let mut state = State {
        window: WindowState {
            active_workspace: Some(workspace),
            route: Route::Workbench(workbench),
        },
        ..State::default()
    };
    state.workspaces.insert(
        workspace,
        Workspace {
            id: workspace,
            title: Title::new("Работа")?,
            position: 0,
            last_workbench: Some(workbench),
            deleted_at: None,
        },
    );
    state.projects.insert(
        project,
        Project {
            id: project,
            workspace,
            title: Title::new("shellio")?,
            list_position: Some(0),
            removed_at: None,
            last_opened_at: None,
        },
    );
    state.workbenches.insert(
        workbench,
        Workbench {
            id: workbench,
            project,
            title: Title::new("Основная")?,
            position: 0,
        },
    );
    assert_eq!(state.check_invariants(), Vec::new());
    Ok((state, workbench))
}

#[tokio::test]
async fn start_writes_default_layout_and_snapshot_plus_events_equal_state() {
    let (seeded, workbench) = open_workbench_without_layout().unwrap();
    // Базу писал владелец с тем же реестром: запись запуска — обычное событие в буфере.
    let memory = MemoryPersist::with_state(seeded.clone(), Revision(3)).written_by(&registry());

    let (core, faults) = start_on(&memory).unwrap();
    assert_eq!(
        faults.commits(),
        1,
        "раскладка пишется одной записью запуска"
    );
    let snapshot = snapshot(&core).await.unwrap();
    assert_eq!(snapshot.revision, Revision(4));
    assert_eq!(*snapshot.state, memory.state());
    assert_eq!(snapshot.state.check_invariants(), Vec::new());

    let layout = &snapshot.state.layouts[&workbench];
    let navigator = &snapshot.state.panels[&layout.navigator.unwrap()];
    let terminal = &snapshot.state.panels[&layout.focused().unwrap()];
    assert_eq!(navigator.plugin.as_str(), NAVIGATOR_PLUGIN);
    assert_eq!(terminal.plugin.as_str(), TERMINAL_PLUGIN);
    assert_eq!(layout.zones.center.parts[0].tabs, [terminal.id]);
    assert_eq!((navigator.created_at, terminal.created_at), (NOW, NOW));

    // Снимок до запуска и событие запуска дают то же состояние с раскладкой.
    let EventsSince::Events(events) = core.events_since(Revision(3)).await.unwrap() else {
        panic!("ожидалось событие запуска");
    };
    let mut replayed = seeded;
    for event in &events {
        replayed.apply_all(&event.changes);
    }
    assert_eq!(events.len(), 1);
    assert_eq!(replayed, *snapshot.state);
    core.shutdown().await;

    // Второй запуск находит раскладку и ничего не пишет: `PanelId` прежние.
    let (core, faults) = start_on(&memory).unwrap();
    let again = self::snapshot(&core).await.unwrap();
    assert_eq!(faults.commits(), 0);
    assert_eq!(again, snapshot);
    core.shutdown().await;
}

#[tokio::test]
async fn load_failure_is_returned_as_is() {
    let (persist, faults) = FaultyPersist::new(MemoryPersist::new());
    faults.fail_load();
    match start(persist) {
        Err(StartError::Load(error)) => {
            assert_eq!(error.to_string(), "отказ загрузки по плану теста");
        }
        other => panic!("ожидался отказ загрузки: {other:?}"),
    }
    assert_eq!(faults.commits(), 0);
    assert!(faults.is_dropped(), "поток хранения не завершён");
}

#[tokio::test]
async fn startup_commit_failure_stops_start() {
    let memory = MemoryPersist::new();
    let (persist, faults) = FaultyPersist::new(memory.clone());
    faults.fail_commit(1);
    match start(persist) {
        Err(StartError::Startup(error)) => {
            assert_eq!(error.code(), ErrorCode::StorageUnavailable);
        }
        other => panic!("ожидался отказ записи запуска: {other:?}"),
    }
    assert_eq!(memory.revision(), Revision::ZERO);
    assert_eq!(memory.state(), State::default());
    assert!(faults.is_dropped(), "поток хранения не завершён");
}

#[tokio::test]
async fn reply_comes_after_commit_receipt() {
    let memory = MemoryPersist::new();
    let (core, faults) = start_on(&memory).unwrap();
    let workspace = active(&core).await.unwrap();

    let mut barrier = faults.barrier_next_commit();
    let mut receipt = core.try_request(rename(workspace, "Работа")).unwrap();
    barrier.entered().await;
    assert!(matches!(
        receipt.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(memory.revision(), Revision(1));

    barrier.release();
    let reply = receipt.await.unwrap().unwrap();
    assert_eq!(reply.revision, Revision(2));
    assert_eq!(reply.value, ReplyValue::Command(CommandValue::None));
    assert_eq!(memory.revision(), Revision(2));
    assert_eq!(titles(&memory.state()), ["Работа"]);
}

#[tokio::test]
async fn failed_commit_changes_nothing_and_next_command_passes() {
    let memory = MemoryPersist::new();
    let (core, faults) = start_on(&memory).unwrap();
    let workspace = active(&core).await.unwrap();
    let (before, mut events) = core.subscribe().await.unwrap();

    faults.fail_next_commit();
    let error = core.request(rename(workspace, "Работа")).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::StorageUnavailable);
    assert!(error.retryable());

    assert_eq!(snapshot(&core).await.unwrap(), before);
    assert_eq!(memory.revision(), before.revision);
    assert_eq!(memory.state(), *before.state);
    assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!(
        core.events_since(before.revision).await.unwrap(),
        EventsSince::Events(Vec::new())
    );

    let reply = core.request(rename(workspace, "Работа")).await.unwrap();
    assert_eq!(reply.revision, before.revision.next());
    let event = events.recv().await.unwrap();
    assert_eq!(event.revision, before.revision.next());
    assert!(matches!(
        event.changes.as_slice(),
        [Change::WorkspaceRenamed { workspace: renamed, title }]
            if *renamed == workspace && title.as_str() == "Работа"
    ));
    assert_eq!(memory.revision(), before.revision.next());
}

#[tokio::test]
async fn empty_change_gives_no_revision_and_no_event() {
    let memory = MemoryPersist::new();
    let (core, faults) = start_on(&memory).unwrap();
    let workspace = active(&core).await.unwrap();
    let (before, mut events) = core.subscribe().await.unwrap();
    let commits = faults.commits();

    for command in [
        rename(workspace, BOOTSTRAP_WORKSPACE),
        Command::WindowFocusWorkspace { workspace },
    ] {
        let reply = core.request(command).await.unwrap();
        assert_eq!(reply.revision, before.revision);
        assert_eq!(reply.value, ReplyValue::Command(CommandValue::None));
    }

    assert_eq!(faults.commits(), commits);
    assert_eq!(snapshot(&core).await.unwrap(), before);
    assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!(
        core.events_since(before.revision).await.unwrap(),
        EventsSince::Events(Vec::new())
    );
}

#[tokio::test]
async fn handler_refusal_does_not_reach_storage() {
    let memory = MemoryPersist::new();
    let (core, faults) = start_on(&memory).unwrap();
    let before = snapshot(&core).await.unwrap();

    let error = core.request(create(BOOTSTRAP_WORKSPACE)).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::NameTaken);
    assert_eq!(faults.commits(), 1);
    assert_eq!(snapshot(&core).await.unwrap(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_concurrent_with_commands_has_no_gap() {
    const WRITERS: usize = 4;
    const PER_WRITER: usize = 50;
    const SUBSCRIPTIONS: usize = 20;
    // Подписчики не читают до конца записи, поэтому событий меньше ёмкости их канала.
    const _: () = assert!(WRITERS * PER_WRITER < SUBSCRIBER_CAPACITY);

    let memory = MemoryPersist::new();
    let (core, _) = start_on(&memory).unwrap();

    let writers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let core = core.clone();
            tokio::spawn(async move {
                for index in 0..PER_WRITER {
                    core.request(create(&format!("w{writer}-{index}"))).await?;
                }
                Ok::<_, CoreError>(())
            })
        })
        .collect();
    let subscriber = {
        let core = core.clone();
        tokio::spawn(async move {
            let mut subscriptions = Vec::new();
            for _ in 0..SUBSCRIPTIONS {
                subscriptions.push(core.subscribe().await?);
                tokio::task::yield_now().await;
            }
            Ok::<_, CoreError>(subscriptions)
        })
    };
    for writer in writers {
        writer.await.unwrap().unwrap();
    }
    let subscriptions = subscriber.await.unwrap().unwrap();
    let last = snapshot(&core).await.unwrap();
    assert_eq!(last.revision, Revision(1 + (WRITERS * PER_WRITER) as u64));

    for (snapshot, mut events) in subscriptions {
        let mut mirror = (*snapshot.state).clone();
        let mut expected = snapshot.revision.next();
        while expected <= last.revision {
            let event = events.recv().await.unwrap();
            assert_eq!(
                event.revision, expected,
                "разрыв после снимка {:?}",
                snapshot.revision
            );
            mirror.apply_all(&event.changes);
            expected = expected.next();
        }
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(mirror, *last.state);
    }
    assert_eq!(last.state.check_invariants(), Vec::new());
}

#[tokio::test]
async fn events_since_covers_buffer_and_resets_beyond_it() {
    let memory = MemoryPersist::new();
    let (core, _) = start_on(&memory).unwrap();
    let workspace = active(&core).await.unwrap();
    for index in 0..EVENT_BUFFER + 10 {
        core.request(rename(workspace, alternating(index)))
            .await
            .unwrap();
    }
    let current = snapshot(&core).await.unwrap();
    let head = current.revision.0;
    let buffer = EVENT_BUFFER as u64;
    assert_eq!(head, 1 + buffer + 10);

    let since = |cursor: u64| core.events_since(Revision(cursor));
    assert_eq!(events(since(head).await.unwrap()).unwrap(), Vec::new());
    assert_eq!(
        revisions(&events(since(head - 3).await.unwrap()).unwrap()),
        [head - 2, head - 1, head]
    );

    let all = events(since(head - buffer).await.unwrap()).unwrap();
    assert_eq!(all.len(), EVENT_BUFFER);
    assert_eq!(all[0].revision, Revision(head - buffer + 1));
    assert_eq!(all[EVENT_BUFFER - 1].revision, current.revision);

    for cursor in [head - buffer - 1, 0] {
        assert_eq!(
            since(cursor).await.unwrap(),
            EventsSince::Reset(current.clone()),
            "курсор {cursor}"
        );
    }

    let error = since(head + 1).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
    assert_eq!(error.detail_str("reason"), Some("cursor_ahead"));
}

#[tokio::test]
async fn lagging_subscriber_gets_lagged() {
    let memory = MemoryPersist::new();
    let (core, _) = start_on(&memory).unwrap();
    let workspace = active(&core).await.unwrap();
    let (_, mut events) = core.subscribe().await.unwrap();

    for index in 0..=SUBSCRIBER_CAPACITY {
        core.request(rename(workspace, alternating(index)))
            .await
            .unwrap();
    }
    assert!(matches!(events.recv().await, Err(RecvError::Lagged(1))));
}

#[tokio::test]
async fn try_request_on_full_queue_is_busy() {
    let memory = MemoryPersist::new();
    let (core, faults) = start_on(&memory).unwrap();
    let workspace = active(&core).await.unwrap();

    let mut barrier = faults.barrier_next_commit();
    let mut receipts = vec![core.try_request(rename(workspace, "Работа")).unwrap()];
    barrier.entered().await;
    for index in 0..QUEUE_CAPACITY {
        receipts.push(
            core.try_request(rename(workspace, alternating(index)))
                .unwrap(),
        );
    }
    let error = core.try_request(rename(workspace, "Лишний")).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Busy);
    assert!(error.retryable());

    barrier.release();
    for (index, receipt) in receipts.into_iter().enumerate() {
        let reply = receipt.await.unwrap().unwrap();
        assert_eq!(reply.revision, Revision(2 + index as u64));
    }
    assert_eq!(memory.revision(), Revision(2 + QUEUE_CAPACITY as u64));
}

#[tokio::test]
async fn dropped_receipt_does_not_cancel_command() {
    let memory = MemoryPersist::new();
    let (core, faults) = start_on(&memory).unwrap();

    let mut barrier = faults.barrier_next_commit();
    let in_flight = core.try_request(create("В записи")).unwrap();
    barrier.entered().await;
    drop(core.try_request(create("В очереди")).unwrap());
    drop(in_flight);
    barrier.release();

    core.request(create("После")).await.unwrap();
    let state = memory.state();
    let mut names = titles(&state);
    names.sort();
    assert_eq!(names, ["В записи", "В очереди", "Основной", "После"]);
    assert_eq!(*snapshot(&core).await.unwrap().state, state);
}

#[tokio::test]
async fn shutdown_refuses_new_requests_and_finishes_accepted() {
    let memory = MemoryPersist::new();
    let (core, faults) = start_on(&memory).unwrap();

    let mut barrier = faults.barrier_next_commit();
    let first = core.try_request(create("Первый")).unwrap();
    barrier.entered().await;
    let second = core.try_request(create("Второй")).unwrap();
    let third = core.request(create("Третий"));

    let stop = core.shutdown();
    let refused = [
        core.try_request(create("Поздний")).unwrap_err(),
        core.request(create("Поздний")).await.unwrap_err(),
        core.query(Query::Status {}).await.unwrap_err(),
        core.subscribe().await.unwrap_err(),
        core.events_since(Revision::ZERO).await.unwrap_err(),
    ];
    for error in refused {
        assert_eq!(error.code(), ErrorCode::ShuttingDown);
        assert!(!error.retryable());
    }
    let late_add = core
        .try_request(Command::ProjectAdd {
            workspace: WorkspaceId::generate(&mut SequentialIds::new()),
            path: "/src/late".into(),
            title: None,
        })
        .unwrap();
    assert_eq!(
        late_add.await.unwrap().unwrap_err().code(),
        ErrorCode::ShuttingDown
    );
    // Future `request`, созданный до `shutdown` и не опрошенный, в очередь не попал.
    assert_eq!(third.await.unwrap_err().code(), ErrorCode::ShuttingDown);

    barrier.release();
    stop.await;
    assert_eq!(first.await.unwrap().unwrap().revision, Revision(2));
    assert_eq!(second.await.unwrap().unwrap().revision, Revision(3));
    assert_eq!(memory.revision(), Revision(3));
    let mut names = titles(&memory.state());
    names.sort();
    assert_eq!(names, ["Второй", "Основной", "Первый"]);
    assert!(faults.is_dropped(), "поток хранения не присоединён");

    core.shutdown().await;
    assert_eq!(
        core.try_request(create("После")).unwrap_err().code(),
        ErrorCode::ShuttingDown
    );
}

#[tokio::test]
async fn project_add_is_prepared_before_actor() {
    let memory = MemoryPersist::new();
    let (core, _) = start_on(&memory).unwrap();
    let workspace = active(&core).await.unwrap();

    let reply = core
        .request(Command::ProjectAdd {
            workspace,
            path: "/src/shellio".into(),
            title: None,
        })
        .await
        .unwrap();
    let ReplyValue::Command(CommandValue::Project { project, .. }) = reply.value else {
        panic!("ожидался ответ project.add: {reply:?}");
    };
    let receipt = core
        .try_request(Command::ProjectAdd {
            workspace,
            path: "/src/notes".into(),
            title: Some("Заметки".into()),
        })
        .unwrap();
    assert_eq!(receipt.await.unwrap().unwrap().revision, Revision(3));

    let state = snapshot(&core).await.unwrap().state;
    assert_eq!(state.projects[&project].title.as_str(), "shellio");
    assert_eq!(state.projects.len(), 2);
    assert_eq!(state.roots.len(), 2);

    let relative = || Command::ProjectAdd {
        workspace,
        path: "src/relative".into(),
        title: None,
    };
    let direct = core.request(relative()).await.unwrap_err();
    let receipt = core.try_request(relative()).unwrap();
    let through_receipt = receipt.await.unwrap().unwrap_err();
    for error in [direct, through_receipt] {
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(error.detail_str("reason"), Some("path_not_absolute"));
    }
}

#[tokio::test]
async fn reads_do_not_reach_storage() {
    let memory = MemoryPersist::new();
    let (core, faults) = start_on(&memory).unwrap();

    let status = core.query(Query::Status {}).await.unwrap();
    assert_eq!(status.revision, Revision(1));
    assert_eq!(
        status.value,
        ReplyValue::Status(StatusView {
            schema_version: 0,
            state_db: MemoryPersist::STATE_DB.into(),
            diagnostics: Vec::new(),
        })
    );
    let tree = core
        .query(Query::Tree {
            scope: TreeScope::All,
        })
        .await
        .unwrap();
    assert_eq!(tree.revision, Revision(1));
    let ReplyValue::Tree(tree) = tree.value else {
        panic!("ожидалось дерево");
    };
    assert_eq!(tree.workspaces.len(), 1);
    assert_eq!(faults.commits(), 1);
}

/// Дерево всех воркспейсов вместе с ревизией ответа.
async fn tree_all(core: &CoreHandle) -> TestResult<(Revision, TreeView)> {
    let reply = core
        .query(Query::Tree {
            scope: TreeScope::All,
        })
        .await?;
    match reply.value {
        ReplyValue::Tree(tree) => Ok((reply.revision, tree)),
        other => Err(format!("ожидалось дерево, получено {other:?}").into()),
    }
}

/// Значение ответа команды и её ревизия.
async fn command(core: &CoreHandle, command: Command) -> TestResult<(Revision, CommandValue)> {
    let reply = core.request(command).await?;
    match reply.value {
        ReplyValue::Command(value) => Ok((reply.revision, value)),
        other => Err(format!("ожидался ответ команды, получено {other:?}").into()),
    }
}

#[tokio::test]
async fn every_command_result_is_read_through_query_tree() {
    let clock = ManualClock::new(NOW);
    let (persist, _) = FaultyPersist::new(MemoryPersist::new());
    let core = Core::start(
        &Handle::current(),
        Box::new(persist),
        Box::new(clock.clone()),
        Box::new(SequentialIds::new()),
        Arc::new(FakeRootResolver::new().with_git("/src/shellio", "/src/shellio/.git")),
        registry(),
    )
    .unwrap();
    let (_, start) = tree_all(&core).await.unwrap();
    let home = start.active_workspace.unwrap();

    let (revision, value) = command(&core, create("Работа")).await.unwrap();
    let CommandValue::Workspace { workspace: work } = value else {
        panic!("ожидался воркспейс: {value:?}");
    };
    let (read, view) = tree_all(&core).await.unwrap();
    assert_eq!((revision, read), (Revision(2), Revision(2)));
    let ids: Vec<_> = view
        .workspaces
        .iter()
        .map(|workspace| workspace.id)
        .collect();
    assert_eq!(ids, [home, work], "новый воркспейс — в конце");
    assert_eq!(view.workspaces[1].title.as_str(), "Работа");
    assert_eq!(view.workspaces[1].projects, []);
    assert_eq!(view.active_workspace, Some(home));

    let (revision, value) = command(&core, rename(work, "Проекты")).await.unwrap();
    assert_eq!(value, CommandValue::None);
    let (read, view) = tree_all(&core).await.unwrap();
    assert_eq!((revision, read), (Revision(3), Revision(3)));
    assert_eq!(view.workspaces[1].title.as_str(), "Проекты");

    let add = Command::ProjectAdd {
        workspace: work,
        path: "/src/shellio".into(),
        title: None,
    };
    let (revision, value) = command(&core, add).await.unwrap();
    let CommandValue::Project { project, root } = value else {
        panic!("ожидался проект: {value:?}");
    };
    let (read, view) = tree_all(&core).await.unwrap();
    assert_eq!((revision, read), (Revision(4), Revision(4)));
    assert_eq!(
        view.workspaces[0].projects,
        [],
        "проект — в своём воркспейсе"
    );
    let [added] = view.workspaces[1].projects.as_slice() else {
        panic!("ожидался один проект: {view:?}");
    };
    assert_eq!(added.id, project);
    assert_eq!(added.title.as_str(), "shellio");
    assert_eq!(added.list_position, Some(0));
    assert_eq!(added.last_opened_at, None);
    assert_eq!(
        added.roots,
        [TreeRoot {
            id: root,
            path: "/src/shellio".into(),
            kind: RootKind::Git,
            key: RootKey::new("/src/shellio/.git"),
        }]
    );
    assert_eq!(added.workbenches, []);

    let create_workbench = Command::WorkbenchCreate {
        project,
        title: "main".into(),
    };
    let (revision, value) = command(&core, create_workbench).await.unwrap();
    let CommandValue::Workbench { workbench } = value else {
        panic!("ожидалась рабочая область: {value:?}");
    };
    let (read, view) = tree_all(&core).await.unwrap();
    assert_eq!((revision, read), (Revision(5), Revision(5)));
    let workbenches: Vec<_> = view.workspaces[1].projects[0]
        .workbenches
        .iter()
        .map(|workbench| (workbench.id, workbench.title.as_str().to_owned()))
        .collect();
    assert_eq!(workbenches, [(workbench, "main".to_owned())]);

    let rename_workbench = Command::WorkbenchRename {
        workbench,
        title: "Основная".into(),
    };
    let (revision, value) = command(&core, rename_workbench).await.unwrap();
    assert_eq!(value, CommandValue::None);
    let (read, view) = tree_all(&core).await.unwrap();
    assert_eq!((revision, read), (Revision(6), Revision(6)));
    assert_eq!(
        view.workspaces[1].projects[0].workbenches[0].title.as_str(),
        "Основная"
    );
    assert_eq!(view.route, Some(Route::WorkspaceHome));

    clock.advance(1_000);
    let (revision, value) = command(&core, Command::WorkbenchOpen { workbench })
        .await
        .unwrap();
    assert_eq!(value, CommandValue::None);
    let (read, view) = tree_all(&core).await.unwrap();
    assert_eq!((revision, read), (Revision(7), Revision(7)));
    assert_eq!(view.active_workspace, Some(work));
    assert_eq!(view.route, Some(Route::Workbench(workbench)));
    assert_eq!(view.workspaces[1].last_workbench, Some(workbench));
    assert_eq!(
        view.workspaces[1].projects[0].last_opened_at,
        Some(UnixMillis(NOW.0 + 1_000))
    );

    let focus = |workspace| Command::WindowFocusWorkspace { workspace };
    let (revision, value) = command(&core, focus(home)).await.unwrap();
    assert_eq!(value, CommandValue::None);
    let (read, view) = tree_all(&core).await.unwrap();
    assert_eq!((revision, read), (Revision(8), Revision(8)));
    assert_eq!(view.active_workspace, Some(home));
    assert_eq!(view.route, Some(Route::WorkspaceHome));
    assert_eq!(view.workspaces[1].last_workbench, Some(workbench));

    let (revision, _) = command(&core, focus(work)).await.unwrap();
    let (read, view) = tree_all(&core).await.unwrap();
    assert_eq!((revision, read), (Revision(9), Revision(9)));
    assert_eq!(view.active_workspace, Some(work));
    assert_eq!(
        view.route,
        Some(Route::Workbench(workbench)),
        "маршрут — последняя область"
    );

    let missing_workbench = WorkbenchId::from_bytes([0xee; 16]);
    let refusals = [
        (create("проекты"), ErrorCode::NameTaken),
        (rename(home, "ПРОЕКТЫ"), ErrorCode::NameTaken),
        (
            Command::ProjectAdd {
                workspace: home,
                path: "/src/shellio".into(),
                title: Some("Другое имя".into()),
            },
            ErrorCode::RootTaken,
        ),
        (
            Command::WorkbenchCreate {
                project,
                title: "основная".into(),
            },
            ErrorCode::NameTaken,
        ),
        (
            Command::WorkbenchRename {
                workbench: missing_workbench,
                title: "Новая".into(),
            },
            ErrorCode::NotFound,
        ),
        (
            Command::WorkbenchOpen {
                workbench: missing_workbench,
            },
            ErrorCode::NotFound,
        ),
        (
            focus(WorkspaceId::from_bytes([0xee; 16])),
            ErrorCode::NotFound,
        ),
    ];
    for (refused, code) in refusals {
        let name = refused.name();
        let error = core.request(refused).await.unwrap_err();
        assert_eq!(error.code(), code, "{name}");
        assert_eq!(
            tree_all(&core).await.unwrap(),
            (Revision(9), view.clone()),
            "отказ {name} изменил дерево"
        );
    }
}

/// Гонка открытия панели (ADR-0021, «Уточнено в P1.5 (планирование)»): первая команда держит
/// запись на барьере, вторая подтверждённо стоит в очереди актора до его отпускания.
struct Race {
    core: CoreHandle,
    faults: Faults,
    memory: MemoryPersist,
    workspace: WorkspaceId,
    workbench: WorkbenchId,
    terminal: PanelId,
    base: Revision,
}

/// Каким входом подана команда: квитанцией главного потока или ожиданием в области воркспейса.
#[derive(Clone, Copy, Debug)]
enum Entry {
    Try,
    Scoped,
}

type Pending = tokio::task::JoinHandle<Result<Reply, CoreError>>;

impl Race {
    async fn new() -> TestResult<Self> {
        let memory = MemoryPersist::new();
        let (persist, faults) = FaultyPersist::new(memory.clone());
        let core = Core::start(
            &Handle::current(),
            Box::new(persist),
            Box::new(ManualClock::new(NOW)),
            Box::new(SequentialIds::new()),
            Arc::new(FakeRootResolver::new()),
            registry_of(vec![
                TestPlugin::new("rows")
                    .cardinality(Cardinality::PerWorkbench)
                    .actions(),
            ]),
        )?;
        core.set_viewport(Some(Viewport {
            width: 1180,
            height: 720,
        }));
        let workspace = active(&core).await?;
        let reply = core
            .request(Command::ProjectAdd {
                workspace,
                path: "/src/app".into(),
                title: None,
            })
            .await?;
        let ReplyValue::Command(CommandValue::Project { project, .. }) = reply.value else {
            return Err(format!("ожидался проект: {reply:?}").into());
        };
        let reply = core
            .request(Command::WorkbenchCreate {
                project,
                title: "Основная".into(),
            })
            .await?;
        let ReplyValue::Command(CommandValue::Workbench { workbench }) = reply.value else {
            return Err(format!("ожидалась рабочая область: {reply:?}").into());
        };
        let reply = core.request(Command::WorkbenchOpen { workbench }).await?;
        let state = snapshot(&core).await?.state;
        let terminal = state.layouts[&workbench]
            .focused()
            .ok_or("в раскладке нет фокуса")?;
        Ok(Self {
            core,
            faults,
            memory,
            workspace,
            workbench,
            terminal,
            base: reply.revision,
        })
    }

    fn open(&self, plugin: &str, count: Option<u32>) -> Command {
        Command::PanelOpen {
            plugin: plugin.into(),
            workbench: Some(self.workbench),
            zone: None,
            args: count.map(|count| serde_json::json!({"count": count})),
        }
    }

    fn split(&self) -> Command {
        Command::LayoutSplit {
            panel: self.terminal,
            open: Some("rows".into()),
            axis: None,
        }
    }

    /// Подать команду входом `entry`. Ответ приходит задачей.
    fn submit(&self, entry: Entry, command: Command) -> TestResult<Pending> {
        Ok(match entry {
            Entry::Try => {
                let receipt = self.core.try_request(command)?;
                tokio::spawn(async move {
                    receipt
                        .await
                        .unwrap_or_else(|_| Err(CoreError::shutting_down()))
                })
            }
            Entry::Scoped => {
                let core = self.core.clone();
                let scope = TreeScope::Workspace(self.workspace);
                tokio::spawn(async move { core.request_in(scope, command).await })
            }
        })
    }

    /// Первая команда держит запись на барьере, вторая стоит в очереди; затем барьер отпущен.
    async fn overlap(
        &self,
        first: (Entry, Command),
        second: (Entry, Command),
    ) -> TestResult<(Result<Reply, CoreError>, Result<Reply, CoreError>)> {
        let mut barrier = self.faults.barrier_next_commit();
        let first = self.submit(first.0, first.1)?;
        barrier.entered().await;
        let second = self.submit(second.0, second.1)?;
        while self.core.queued() == 0 {
            // Команда, которой отказали до очереди, оставила бы цикл без конца.
            assert!(
                !second.is_finished(),
                "вторая команда завершилась, не встав в очередь"
            );
            tokio::task::yield_now().await;
        }
        // Вторая ждёт: до отпускания барьера ни записи, ни ответов нет.
        assert_eq!(self.core.queued(), 1);
        assert_eq!(self.memory.revision(), self.base);
        assert!(!first.is_finished() && !second.is_finished());
        barrier.release();
        Ok((first.await?, second.await?))
    }

    /// События после подготовки и согласие зеркала с хранилищем.
    async fn after(&self) -> TestResult<Vec<Event>> {
        let snapshot = snapshot(&self.core).await?;
        assert_eq!(snapshot.revision, self.memory.revision());
        assert_eq!(*snapshot.state, self.memory.state());
        assert_eq!(snapshot.state.check_invariants(), Vec::new());
        events(self.core.events_since(self.base).await?)
    }
}

fn opened(reply: &Result<Reply, CoreError>) -> TestResult<(PanelId, OpenOutcome, Revision)> {
    match reply {
        Ok(Reply {
            revision,
            value: ReplyValue::Command(CommandValue::Opened { panel, outcome, .. }),
        }) => Ok((*panel, *outcome, *revision)),
        other => Err(format!("ожидался исход panel.open: {other:?}").into()),
    }
}

fn added(events: &[Event]) -> Vec<PanelId> {
    events
        .iter()
        .flat_map(|event| &event.changes)
        .filter_map(|change| match change {
            Change::PanelAdd { panel } => Some(panel.id),
            _ => None,
        })
        .collect()
}

const ENTRIES: [(Entry, Entry); 2] = [(Entry::Try, Entry::Scoped), (Entry::Scoped, Entry::Try)];

#[tokio::test]
async fn racing_opens_of_singleton_with_same_context_give_one_panel_and_one_revision() {
    for (first, second) in ENTRIES {
        let race = Race::new().await.unwrap();
        let (one, two) = race
            .overlap(
                (first, race.open("rows", None)),
                (second, race.open("rows", None)),
            )
            .await
            .unwrap();
        let (panel, outcome, revision) = opened(&one).unwrap();
        let next = race.base.next();
        assert_eq!((outcome, revision), (OpenOutcome::Created, next));
        // Вторая нашла ту же панель и ничего не записала: её квитанция несёт ту же ревизию.
        assert_eq!(opened(&two).unwrap(), (panel, OpenOutcome::Focused, next));
        let events = race.after().await.unwrap();
        assert_eq!(revisions(&events), [next.0], "{first:?} {second:?}");
        assert_eq!(added(&events), [panel]);
        assert_eq!(race.memory.revision(), next);
    }
}

#[tokio::test]
async fn racing_opens_with_different_context_keep_one_panel_and_data_of_the_later() {
    for (first, second) in ENTRIES {
        let race = Race::new().await.unwrap();
        let (one, two) = race
            .overlap(
                (first, race.open("rows", Some(1))),
                (second, race.open("rows", Some(2))),
            )
            .await
            .unwrap();
        let (panel, outcome, revision) = opened(&one).unwrap();
        let next = race.base.next();
        assert_eq!((outcome, revision), (OpenOutcome::Created, next));
        assert_eq!(
            opened(&two).unwrap(),
            (panel, OpenOutcome::Focused, next.next())
        );
        let events = race.after().await.unwrap();
        assert_eq!(revisions(&events), [next.0, next.0 + 1]);
        assert_eq!(added(&events), [panel]);
        assert_eq!(
            race.memory.state().panel_data.get(&panel),
            Some(&PanelData::Text {
                version: 1,
                raw: r#"{"count":2}"#.into(),
            })
        );
    }
}

#[tokio::test]
async fn racing_opens_of_multi_plugin_give_two_panels() {
    for (first, second) in ENTRIES {
        let race = Race::new().await.unwrap();
        let (one, two) = race
            .overlap(
                (first, race.open(TERMINAL_PLUGIN, None)),
                (second, race.open(TERMINAL_PLUGIN, None)),
            )
            .await
            .unwrap();
        let (one, outcome_one, revision_one) = opened(&one).unwrap();
        let (two, outcome_two, revision_two) = opened(&two).unwrap();
        assert_ne!(one, two);
        assert_eq!(
            (outcome_one, outcome_two),
            (OpenOutcome::Created, OpenOutcome::Created)
        );
        let next = race.base.next();
        assert_eq!((revision_one, revision_two), (next, next.next()));
        assert_eq!(added(&race.after().await.unwrap()), [one, two]);
    }
}

#[tokio::test]
async fn failed_commit_of_first_open_lets_the_waiting_second_create_the_panel() {
    for (first, second) in ENTRIES {
        let race = Race::new().await.unwrap();
        let commits = race.faults.commits();
        race.faults.fail_commit(commits + 1);
        let (one, two) = race
            .overlap(
                (first, race.open("rows", None)),
                (second, race.open("rows", None)),
            )
            .await
            .unwrap();
        // Один отказ, один успех, одна запись.
        assert_eq!(one.unwrap_err().code(), ErrorCode::StorageUnavailable);
        let next = race.base.next();
        let (panel, outcome, revision) = opened(&two).unwrap();
        assert_eq!((outcome, revision), (OpenOutcome::Created, next));
        assert_eq!(race.faults.commits(), commits + 2);
        assert_eq!(race.memory.revision(), next);
        // У отказавшей первой события нет: единственное событие — запись второй.
        let events = race.after().await.unwrap();
        assert_eq!(revisions(&events), [next.0]);
        assert_eq!(added(&events), [panel]);
        let panels = race.memory.state().workbench_panel_ids(race.workbench);
        assert_eq!(panels.len(), 3, "{panels:?}");
    }
}

#[tokio::test]
async fn racing_split_open_and_panel_open_give_one_panel_in_both_orders() {
    for (first, second) in ENTRIES {
        // Деление первым: `panel.open` находит его панель.
        let race = Race::new().await.unwrap();
        let (one, two) = race
            .overlap((first, race.split()), (second, race.open("rows", None)))
            .await
            .unwrap();
        let next = race.base.next();
        let one = one.unwrap();
        let ReplyValue::Command(CommandValue::Panel { panel }) = one.value else {
            panic!("ожидалась панель деления: {one:?}");
        };
        assert_eq!(one.revision, next);
        assert_eq!(opened(&two).unwrap(), (panel, OpenOutcome::Focused, next));
        assert_eq!(added(&race.after().await.unwrap()), [panel]);

        // `panel.open` первым: деление отказывает `singleton_exists` с его панелью.
        let race = Race::new().await.unwrap();
        let (one, two) = race
            .overlap((first, race.open("rows", None)), (second, race.split()))
            .await
            .unwrap();
        let (panel, outcome, revision) = opened(&one).unwrap();
        assert_eq!((outcome, revision), (OpenOutcome::Created, next));
        let error = two.unwrap_err();
        assert_eq!(error.detail_str("reason"), Some("singleton_exists"));
        assert_eq!(error.detail_str("panel"), Some(panel.to_string().as_str()));
        let events = race.after().await.unwrap();
        assert_eq!(revisions(&events), [next.0]);
        assert_eq!(added(&events), [panel]);
    }
}
