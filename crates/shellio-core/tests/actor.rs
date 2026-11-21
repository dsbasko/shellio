//! Актор ядра и `CoreHandle` на хранилище в памяти с отказами по плану теста.
//!
//! Синхронизация — только квитанции, каналы и барьер записи: поток хранения сообщает,
//! что дошёл до барьера, и ждёт, пока тест его отпустит.

use std::error::Error;
use std::sync::Arc;

use shellio_core::test_support::{
    FakeRootResolver, Faults, FaultyPersist, ManualClock, MemoryPersist, SequentialIds,
};
use shellio_core::{
    BOOTSTRAP_WORKSPACE, Change, Command, CommandValue, Core, CoreError, CoreHandle, EVENT_BUFFER,
    ErrorCode, Event, EventsSince, IdGen, QUEUE_CAPACITY, Query, ReplyValue, Revision, Route,
    SUBSCRIBER_CAPACITY, Snapshot, StartError, State, StatusView, Title, TreeScope, UnixMillis,
    Workspace, WorkspaceId,
};
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

    let events = events(core.events_since(Revision::ZERO).await.unwrap()).unwrap();
    assert_eq!(revisions(&events), [1]);
    assert!(matches!(
        events[0].changes.as_slice(),
        [
            Change::WorkspaceInserted { workspace: inserted },
            Change::WindowSet { active, route: Route::WorkspaceHome },
        ] if inserted.id == workspace && *active == workspace
    ));
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
