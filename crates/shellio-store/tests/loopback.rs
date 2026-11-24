//! JSON-петля транспортной границы: один сценарий напрямую и через петлю на двух временных
//! базах с одинаковыми `IdGen` и `Clock` даёт одинаковые ответы, отказы, события и дерево.

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use shellio_core::test_support::{
    FakeRootResolver, Loopback, LoopbackEvents, ManualClock, SequentialIds, registry,
};
use shellio_core::{
    Command, CommandValue, Core, CoreError, CoreHandle, ErrorCode, Event, EventsSince, ProjectId,
    Query, Reply, ReplyValue, Revision, TreeScope, UnixMillis, WorkbenchId, WorkspaceId,
};
use shellio_store::{Database, Profile, SqlitePersist};
use tempfile::TempDir;
use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);

/// Ядро на своей временной базе.
struct Side {
    _dir: TempDir,
    state_db: PathBuf,
    core: CoreHandle,
}

impl Side {
    fn start(clock: &ManualClock) -> TestResult<Self> {
        let dir = TempDir::new()?;
        let profile = Profile::isolated(dir.path().join("state"));
        let persist = SqlitePersist::new(Database::open(&profile, clock)?);
        let resolver = FakeRootResolver::new().with_git("/src/repo", "/src/repo/.git");
        let core = Core::start(
            &Handle::current(),
            Box::new(persist),
            Box::new(clock.clone()),
            Box::new(SequentialIds::new()),
            Arc::new(resolver),
            registry(),
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            _dir: dir,
            state_db: profile.state_db(),
            core,
        })
    }
}

/// Пара ядер: `direct` вызывается напрямую, `wired` — через петлю. Часы общие, генераторы ID
/// одинаковые, поэтому оба ядра выдают одни и те же ID и время.
struct Pair {
    direct: CoreHandle,
    wired: Loopback,
    direct_events: broadcast::Receiver<Event>,
    wired_events: LoopbackEvents,
    direct_db: PathBuf,
    wired_db: PathBuf,
    events: Vec<Event>,
    _sides: [Side; 2],
}

impl Pair {
    async fn start(clock: &ManualClock) -> TestResult<Self> {
        let direct_side = Side::start(clock)?;
        let wired_side = Side::start(clock)?;
        let direct = direct_side.core.clone();
        let wired = Loopback::new(wired_side.core.clone());
        let (direct_snapshot, direct_events) = direct.subscribe().await?;
        let (wired_snapshot, wired_events) = wired.subscribe().await??;
        assert_eq!(wired_snapshot, direct_snapshot);
        Ok(Self {
            direct,
            wired,
            direct_events,
            wired_events,
            direct_db: direct_side.state_db.clone(),
            wired_db: wired_side.state_db.clone(),
            events: Vec::new(),
            _sides: [direct_side, wired_side],
        })
    }

    /// Команда на обоих ядрах: результаты и события равны. Возвращает прямой результат.
    async fn command(&mut self, command: Command) -> TestResult<Result<Reply, CoreError>> {
        let direct = self.direct.request(command.clone()).await;
        let wired = self.wired.request(command.clone()).await?;
        assert_eq!(wired, direct, "{command:?}");
        self.drain_events()?;
        Ok(direct)
    }

    async fn ok(&mut self, command: Command) -> TestResult<CommandValue> {
        match self.command(command).await?? {
            Reply {
                value: ReplyValue::Command(value),
                ..
            } => Ok(value),
            other => Err(format!("ожидался ответ команды, получено {other:?}").into()),
        }
    }

    async fn refused(&mut self, command: Command, code: ErrorCode) -> TestResult<CoreError> {
        match self.command(command).await? {
            Ok(reply) => Err(format!("ожидался отказ {code:?}, получено {reply:?}").into()),
            Err(error) => {
                assert_eq!(error.code(), code, "{error:?}");
                Ok(error)
            }
        }
    }

    async fn query(&mut self, query: Query) -> TestResult<Result<Reply, CoreError>> {
        let direct = self.direct.query(query.clone()).await;
        let wired = self.wired.query(query.clone()).await?;
        assert_eq!(wired, direct, "{query:?}");
        Ok(direct)
    }

    async fn events_since(
        &mut self,
        cursor: Revision,
    ) -> TestResult<Result<EventsSince, CoreError>> {
        let direct = self.direct.events_since(cursor).await;
        let wired = self.wired.events_since(cursor).await?;
        assert_eq!(wired, direct, "курсор {cursor:?}");
        Ok(direct)
    }

    /// События, пришедшие подписчикам, равны и идут по одному на запись.
    fn drain_events(&mut self) -> TestResult {
        loop {
            let direct = self.direct_events.try_recv();
            let wired = self.wired_events.try_recv()?;
            match (direct, wired) {
                (Ok(direct), Ok(wired)) => {
                    assert_eq!(wired, direct);
                    self.events.push(direct);
                }
                (Err(TryRecvError::Empty), Err(TryRecvError::Empty))
                | (Err(TryRecvError::Closed), Err(TryRecvError::Closed)) => return Ok(()),
                (direct, wired) => {
                    return Err(format!("подписчики разошлись: {direct:?} и {wired:?}").into());
                }
            }
        }
    }
}

fn workspace(value: CommandValue) -> TestResult<WorkspaceId> {
    match value {
        CommandValue::Workspace { workspace } => Ok(workspace),
        other => Err(format!("ожидался воркспейс, получено {other:?}").into()),
    }
}

fn project(value: CommandValue) -> TestResult<ProjectId> {
    match value {
        CommandValue::Project { project, .. } => Ok(project),
        other => Err(format!("ожидался проект, получено {other:?}").into()),
    }
}

fn workbench(value: CommandValue) -> TestResult<WorkbenchId> {
    match value {
        CommandValue::Workbench { workbench } => Ok(workbench),
        other => Err(format!("ожидалась рабочая область, получено {other:?}").into()),
    }
}

#[tokio::test]
async fn loopback_scenario_matches_direct_calls() {
    let clock = ManualClock::new(NOW);
    let mut pair = Pair::start(&clock).await.unwrap();
    let home = pair
        .direct
        .subscribe()
        .await
        .unwrap()
        .0
        .state
        .window
        .active_workspace
        .unwrap();

    let work = workspace(
        pair.ok(Command::WorkspaceCreate {
            title: "Работа".into(),
        })
        .await
        .unwrap(),
    )
    .unwrap();
    pair.refused(
        Command::WorkspaceCreate {
            title: "работа".into(),
        },
        ErrorCode::NameTaken,
    )
    .await
    .unwrap();
    pair.refused(
        Command::WorkspaceCreate { title: " ".into() },
        ErrorCode::InvalidArgument,
    )
    .await
    .unwrap();
    pair.ok(Command::WorkspaceRename {
        workspace: home,
        title: "Личное".into(),
    })
    .await
    .unwrap();

    let repo = project(
        pair.ok(Command::ProjectAdd {
            workspace: work,
            path: "/src/repo".into(),
            title: None,
        })
        .await
        .unwrap(),
    )
    .unwrap();
    let taken = pair
        .refused(
            Command::ProjectAdd {
                workspace: home,
                path: "/src/repo".into(),
                title: Some("Другое имя".into()),
            },
            ErrorCode::RootTaken,
        )
        .await
        .unwrap();
    assert_eq!(taken.detail_str("project"), Some(repo.to_string().as_str()));
    let relative = pair
        .refused(
            Command::ProjectAdd {
                workspace: work,
                path: "src/notes".into(),
                title: None,
            },
            ErrorCode::InvalidArgument,
        )
        .await
        .unwrap();
    assert_eq!(relative.detail_str("reason"), Some("path_not_absolute"));
    pair.ok(Command::ProjectAdd {
        workspace: work,
        path: "/src/notes".into(),
        title: Some("Заметки".into()),
    })
    .await
    .unwrap();

    let main = workbench(
        pair.ok(Command::WorkbenchCreate {
            project: repo,
            title: "main".into(),
        })
        .await
        .unwrap(),
    )
    .unwrap();
    pair.refused(
        Command::WorkbenchCreate {
            project: repo,
            title: "MAIN".into(),
        },
        ErrorCode::NameTaken,
    )
    .await
    .unwrap();
    pair.ok(Command::WorkbenchRename {
        workbench: main,
        title: "Основная".into(),
    })
    .await
    .unwrap();
    let before = pair.events.len();
    pair.ok(Command::WorkbenchRename {
        workbench: main,
        title: "Основная".into(),
    })
    .await
    .unwrap();
    assert_eq!(pair.events.len(), before, "пустое изменение дало событие");

    clock.advance(1_000);
    pair.ok(Command::WorkbenchOpen { workbench: main })
        .await
        .unwrap();
    pair.refused(
        Command::WorkbenchOpen {
            workbench: WorkbenchId::from_bytes([0xee; 16]),
        },
        ErrorCode::NotFound,
    )
    .await
    .unwrap();
    pair.ok(Command::WindowFocusWorkspace { workspace: home })
        .await
        .unwrap();
    pair.ok(Command::WindowFocusWorkspace { workspace: work })
        .await
        .unwrap();

    pair.query(Query::Tree {
        scope: TreeScope::Workspace(work),
    })
    .await
    .unwrap()
    .unwrap();
    pair.query(Query::Tree {
        scope: TreeScope::Workspace(WorkspaceId::from_bytes([0xee; 16])),
    })
    .await
    .unwrap()
    .unwrap_err();

    // Путь `state.db` у двух баз разный, остальное в `query.status` совпадает.
    let status = |db: &PathBuf, reply: Result<Reply, CoreError>| match reply {
        Ok(Reply {
            revision,
            value: ReplyValue::Status(mut status),
        }) => {
            assert_eq!(&status.state_db, db);
            status.state_db = PathBuf::new();
            (revision, status)
        }
        other => panic!("ожидался ответ query.status, получено {other:?}"),
    };
    let direct = status(&pair.direct_db, pair.direct.query(Query::Status {}).await);
    let wired = status(
        &pair.wired_db,
        pair.wired.query(Query::Status {}).await.unwrap(),
    );
    assert_eq!(wired, direct);

    let current = Revision(u64::try_from(pair.events.len()).unwrap() + 1);
    let Ok(EventsSince::Events(since_start)) = pair.events_since(Revision(1)).await.unwrap() else {
        panic!("ожидались события после запуска");
    };
    assert_eq!(since_start, pair.events);
    assert_eq!(
        pair.events_since(current).await.unwrap(),
        Ok(EventsSince::Events(Vec::new()))
    );
    let ahead = pair
        .events_since(current.next())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(ahead.detail_str("reason"), Some("cursor_ahead"));

    let tree = pair
        .query(Query::Tree {
            scope: TreeScope::All,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tree.revision, current);
    let wired_tree = pair
        .wired
        .query(Query::Tree {
            scope: TreeScope::All,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_string(&wired_tree).unwrap(),
        serde_json::to_string(&tree).unwrap()
    );
    assert_eq!(pair.events.len(), 9, "записей в сценарии");

    pair.direct.shutdown().await;
    pair.wired.shutdown().await;
    let after = Command::WorkspaceCreate {
        title: "После".into(),
    };
    assert_eq!(
        pair.command(after).await.unwrap(),
        Err(CoreError::shutting_down())
    );
}
