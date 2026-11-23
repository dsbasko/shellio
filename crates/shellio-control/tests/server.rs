//! Сервер управления над `CoreHandle` на хранилище в памяти: прямые JSON-строки по
//! настоящему Unix-сокету во временном каталоге.
//!
//! Синхронизация — только кадры ответа, крючки сервера и барьер записи ядра. Сроки идут по
//! ручным часам: срок завершения истекает только по `expire` теста.

use std::error::Error;
use std::future::{Future, ready};
use std::os::unix::net::UnixListener as StdListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use shellio_control::test_support::ManualClock;
use shellio_control::{
    Activation, ControlError, ControlErrorCode, Launch, LaunchHandler, LaunchReceipt, LineError,
    LineReader, OkValue, OwnerState, REQUEST_LINE_LIMIT, RESPONSE_LINE_LIMIT, RefuseReason,
    RefusedArg, RequestId, Response, Server, ServerConfig, WithoutUi,
};
use shellio_core::test_support::{
    FakeRootResolver, Faults, FaultyPersist, ManualClock as CoreClock, MemoryPersist, SequentialIds,
};
use shellio_core::{
    CommandValue, Core, CoreError, Entity, ErrorCode, Event, EventsSince, ProjectId, Reply,
    ReplyValue, Revision, Route, SUBSCRIBER_CAPACITY, Snapshot, StatusView, TreeView, UnixMillis,
    WorkbenchId, WorkspaceId,
};
use tempfile::TempDir;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::runtime::Handle;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);
const PID: u32 = 4242;
const SERVER: &str = "test 0.1.0";
const MISSING_PATH: &str = "/missing";

/// Обработчик запуска: записывает аргументы и отказывает каждому как папке.
#[derive(Clone, Default)]
struct FakeLauncher {
    launches: Arc<Mutex<Vec<Launch>>>,
}

impl FakeLauncher {
    fn launches(&self) -> Vec<Launch> {
        self.launches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl LaunchHandler for FakeLauncher {
    fn launch(&self, launch: Launch) -> impl Future<Output = LaunchReceipt> + Send {
        let refused = launch
            .args
            .iter()
            .map(|arg| RefusedArg {
                arg: arg.clone(),
                reason: RefuseReason::FolderOpenUnsupported,
            })
            .collect();
        self.launches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(launch);
        ready(LaunchReceipt {
            activation: Activation::NoUi,
            refused,
        })
    }
}

/// Ядро на `MemoryPersist` и сервер на сокете во временном каталоге `/tmp`.
struct Fixture {
    server: Option<Server<ManualClock, FakeLauncher>>,
    memory: MemoryPersist,
    faults: Faults,
    clock: ManualClock,
    launcher: FakeLauncher,
    socket: PathBuf,
    _dir: TempDir,
}

impl Fixture {
    async fn start() -> TestResult<Self> {
        Self::start_on(MemoryPersist::new()).await
    }

    async fn start_on(memory: MemoryPersist) -> TestResult<Self> {
        let dir = tempfile::Builder::new()
            .prefix("shellio-")
            .tempdir_in("/tmp")?;
        let socket = dir.path().join("s.sock");
        let listener = StdListener::bind(&socket)?;
        let (persist, faults) = FaultyPersist::new(memory.clone());
        let resolver = FakeRootResolver::new().with(
            MISSING_PATH,
            Err(shellio_core::RootError::NotFound {
                path: MISSING_PATH.into(),
            }),
        );
        let core = Core::start(
            &Handle::current(),
            Box::new(persist),
            Box::new(CoreClock::new(NOW)),
            Box::new(SequentialIds::new()),
            Arc::new(resolver),
        )?;
        let clock = ManualClock::new();
        let launcher = FakeLauncher::default();
        let config = ServerConfig {
            server: SERVER.to_owned(),
            pid: PID,
            grace: shellio_control::SHUTDOWN_GRACE,
        };
        let server = Server::start(
            &Handle::current(),
            listener,
            core,
            launcher.clone(),
            WithoutUi,
            clock.clone(),
            config,
        )?;
        Ok(Self {
            server: Some(server),
            memory,
            faults,
            clock,
            launcher,
            socket,
            _dir: dir,
        })
    }

    fn server(&self) -> TestResult<&Server<ManualClock, FakeLauncher>> {
        Ok(self.server.as_ref().ok_or("сервер уже завершён")?)
    }

    /// Начать завершение: переход в `leaving` происходит при вызове.
    fn shutdown(&mut self) -> TestResult<tokio::task::JoinHandle<()>> {
        let server = self.server.take().ok_or("сервер уже завершён")?;
        Ok(tokio::spawn(server.shutdown()))
    }

    async fn connect(&self) -> TestResult<Peer> {
        Peer::connect(&self.socket).await
    }

    /// Соединение после успешного `hello`.
    async fn greeted(&self) -> TestResult<Peer> {
        let mut peer = self.connect().await?;
        let response = peer.call(&hello(1)).await?;
        assert!(
            matches!(
                response,
                Response::Ok {
                    value: OkValue::Hello(_),
                    ..
                }
            ),
            "{response:?}"
        );
        Ok(peer)
    }
}

/// Клиент сокета: пишет строки JSON как есть и читает кадры сервера.
struct Peer {
    reader: LineReader<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
}

impl Peer {
    async fn connect(socket: &Path) -> TestResult<Self> {
        let (reader, writer) = UnixStream::connect(socket).await?.into_split();
        Ok(Self {
            reader: LineReader::new(BufReader::new(reader), RESPONSE_LINE_LIMIT),
            writer,
        })
    }

    async fn send(&mut self, line: &str) -> TestResult {
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        Ok(())
    }

    /// Следующая строка как значение JSON.
    async fn recv_value(&mut self) -> TestResult<Value> {
        let line = self.reader.next_line().await?.ok_or("соединение закрыто")?;
        Ok(serde_json::from_slice(line)?)
    }

    async fn recv(&mut self) -> TestResult<Response> {
        Ok(serde_json::from_value(self.recv_value().await?)?)
    }

    async fn call(&mut self, line: &str) -> TestResult<Response> {
        self.send(line).await?;
        self.recv().await
    }

    async fn call_value(&mut self, line: &str) -> TestResult<Value> {
        self.send(line).await?;
        self.recv_value().await
    }

    /// Сервер закрыл соединение, не прислав больше ни строки.
    async fn assert_closed(&mut self) -> TestResult {
        match self.reader.next_line().await {
            Ok(None) => Ok(()),
            Ok(Some(line)) => Err(format!(
                "после закрытия пришла строка {}",
                String::from_utf8_lossy(line)
            )
            .into()),
            Err(error) => Err(error.into()),
        }
    }

    /// Команда в области `scope`, ответ — квитанция ядра или его отказ.
    async fn command(
        &mut self,
        id: u64,
        command: Value,
        scope: Value,
    ) -> TestResult<Result<Reply, CoreError>> {
        let line = json!({"id": id, "command": command, "scope": scope}).to_string();
        reply_or_core_error(id, self.call(&line).await?)
    }

    async fn tree(&mut self, id: u64, scope: Value) -> TestResult<TreeView> {
        let line = json!({"id": id, "query": {"query": "tree", "scope": scope}}).to_string();
        match reply_or_core_error(id, self.call(&line).await?)??.value {
            ReplyValue::Tree(tree) => Ok(tree),
            other => Err(format!("ожидалось дерево, получено {other:?}").into()),
        }
    }

    async fn status(&mut self, id: u64) -> TestResult<(Revision, StatusView)> {
        let line = json!({"id": id, "query": {"query": "status"}}).to_string();
        let reply = reply_or_core_error(id, self.call(&line).await?)??;
        match reply.value {
            ReplyValue::Status(status) => Ok((reply.revision, status)),
            other => Err(format!("ожидался статус, получено {other:?}").into()),
        }
    }

    async fn events_since(&mut self, id: u64, cursor: u64) -> TestResult<Response> {
        let line = json!({"id": id, "events_since": {"cursor": cursor}, "scope": "all"});
        self.call(&line.to_string()).await
    }
}

fn hello(id: u64) -> String {
    json!({"id": id, "hello": {"protocol": 2, "client": "test"}}).to_string()
}

fn subscribe(id: u64) -> String {
    json!({"id": id, "subscribe": {}, "scope": "all"}).to_string()
}

fn all() -> Value {
    json!("all")
}

fn in_workspace(workspace: WorkspaceId) -> Value {
    json!({"workspace": workspace})
}

fn reply_or_core_error(id: u64, response: Response) -> TestResult<Result<Reply, CoreError>> {
    match response {
        Response::Ok {
            id: got,
            value: OkValue::Reply(reply),
        } if got == RequestId(id) => Ok(Ok(reply)),
        Response::CoreError { id: got, error } if got == RequestId(id) => Ok(Err(error)),
        other => Err(format!("ожидался ответ ядра на {id}, получено {other:?}").into()),
    }
}

/// Отказ транспорта с номером `id`.
fn control_error(id: Option<u64>, response: Response) -> TestResult<ControlError> {
    match response {
        Response::Error { id: got, error } if got == id.map(RequestId) => Ok(error),
        other => Err(format!("ожидался отказ транспорта на {id:?}, получено {other:?}").into()),
    }
}

fn command_value(reply: Reply) -> TestResult<CommandValue> {
    match reply.value {
        ReplyValue::Command(value) => Ok(value),
        other => Err(format!("ожидался ответ команды, получено {other:?}").into()),
    }
}

fn rename_workspace(workspace: WorkspaceId, title: &str) -> Value {
    json!({"cmd": "workspace.rename", "workspace": workspace, "title": title})
}

/// Имя, отличное от предыдущего: каждое переименование меняет состояние.
fn alternating(index: usize) -> &'static str {
    if index.is_multiple_of(2) {
        "Альфа"
    } else {
        "Бета"
    }
}

/// Два воркспейса с проектом и рабочей областью в каждом. Активен `home`, окно на главной.
struct World {
    home: WorkspaceId,
    work: WorkspaceId,
    home_project: ProjectId,
    work_project: ProjectId,
    home_workbench: WorkbenchId,
    work_workbench: WorkbenchId,
}

async fn world(peer: &mut Peer) -> TestResult<World> {
    let home = peer
        .tree(100, all())
        .await?
        .active_workspace
        .ok_or("нет активного воркспейса")?;
    let created = json!({"cmd": "workspace.create", "title": "Работа"});
    let CommandValue::Workspace { workspace: work } =
        command_value(peer.command(101, created, all()).await??)?
    else {
        return Err("ожидался воркспейс".into());
    };
    let mut add = async |id, workspace: WorkspaceId, path: &str| -> TestResult<ProjectId> {
        let add = json!({"cmd": "project.add", "workspace": workspace, "path": path});
        match command_value(peer.command(id, add, all()).await??)? {
            CommandValue::Project { project, .. } => Ok(project),
            other => Err(format!("ожидался проект, получено {other:?}").into()),
        }
    };
    let home_project = add(102, home, "/src/home").await?;
    let work_project = add(103, work, "/src/work").await?;
    let mut bench = async |id, project: ProjectId| -> TestResult<WorkbenchId> {
        let create = json!({"cmd": "workbench.create", "project": project, "title": "Основная"});
        match command_value(peer.command(id, create, all()).await??)? {
            CommandValue::Workbench { workbench } => Ok(workbench),
            other => Err(format!("ожидалась рабочая область, получено {other:?}").into()),
        }
    };
    let home_workbench = bench(104, home_project).await?;
    let work_workbench = bench(105, work_project).await?;
    Ok(World {
        home,
        work,
        home_project,
        work_project,
        home_workbench,
        work_workbench,
    })
}

#[tokio::test]
async fn hello_must_come_first() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.connect().await?;

    let status = json!({"id": 1, "query": {"query": "status"}}).to_string();
    let error = control_error(Some(1), peer.call(&status).await?)?;
    assert_eq!(error.code(), ControlErrorCode::HelloRequired);

    // Отказ области говорит о виде запроса: до `hello` это тоже `hello_required`.
    let unscoped = json!({"id": 2, "command": {"cmd": "workspace.create", "title": "X"}});
    let error = control_error(Some(2), peer.call(&unscoped.to_string()).await?)?;
    assert_eq!(error.code(), ControlErrorCode::HelloRequired);
    let launch = json!({"id": 3, "launch": {"args": [], "cwd": "/"}}).to_string();
    let error = control_error(Some(3), peer.call(&launch).await?)?;
    assert_eq!(error.code(), ControlErrorCode::HelloRequired);

    // Негодная строка — не запрос: её отказ остаётся `malformed_request`.
    let error = control_error(None, peer.call("не JSON").await?)?;
    assert_eq!(error.code(), ControlErrorCode::MalformedRequest);
    assert!(fixture.launcher.launches().is_empty());

    let reply = peer.call_value(&hello(4)).await?;
    assert_eq!(
        reply,
        json!({"id": 4, "ok": {"hello": {
            "protocol": 2, "server": SERVER, "pid": PID, "owner": "ready"
        }}})
    );
    let again = peer.call_value(&hello(5)).await?;
    assert_eq!(
        again["ok"], reply["ok"],
        "повторный hello отвечается так же"
    );

    let (revision, _) = peer.status(6).await?;
    assert_eq!(revision, Revision(1));
    Ok(())
}

/// Клиент протокола v1 у владельца v2 получает отказ `hello` и ни одной команды не выполняет.
#[tokio::test]
async fn protocol_mismatch_keeps_connection_without_greeting() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.connect().await?;

    let wrong = json!({"id": 1, "hello": {"protocol": 1, "client": "test"}}).to_string();
    let error = control_error(Some(1), peer.call(&wrong).await?)?;
    assert_eq!(error.code(), ControlErrorCode::ProtocolMismatch);
    assert_eq!(error.detail("supported"), Some(&json!([2])));

    let created = json!({"id": 5, "command": {"cmd": "workspace.create", "title": "Работа"},
        "scope": "all"});
    let error = control_error(Some(5), peer.call(&created.to_string()).await?)?;
    assert_eq!(error.code(), ControlErrorCode::HelloRequired);
    assert_eq!(
        fixture.memory.revision(),
        Revision(1),
        "команда не выполнена"
    );

    let status = json!({"id": 2, "query": {"query": "status"}}).to_string();
    let error = control_error(Some(2), peer.call(&status).await?)?;
    assert_eq!(error.code(), ControlErrorCode::HelloRequired);

    let response = peer.call(&hello(3)).await?;
    assert!(matches!(
        response,
        Response::Ok {
            value: OkValue::Hello(_),
            ..
        }
    ));
    peer.status(4).await?;
    Ok(())
}

#[tokio::test]
async fn request_of_exactly_limit_is_served() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.connect().await?;
    let head = r#"{"id":1,"hello":{"protocol":2,"client":""#;
    let tail = r#""}}"#;
    let padding = "a".repeat(REQUEST_LINE_LIMIT - head.len() - tail.len());
    let line = format!("{head}{padding}{tail}");
    assert_eq!(line.len(), REQUEST_LINE_LIMIT);

    let response = peer.call(&line).await?;
    assert!(
        matches!(
            response,
            Response::Ok {
                id: RequestId(1),
                value: OkValue::Hello(_)
            }
        ),
        "{response:?}"
    );
    Ok(())
}

#[tokio::test]
async fn request_too_large_closes_connection() -> TestResult {
    let fixture = Fixture::start().await?;
    let peer = fixture.greeted().await?;
    let Peer { reader, mut writer } = peer;
    // Сервер перестаёт читать после лимита: запись идёт отдельной задачей и может оборваться.
    let writing = tokio::spawn(async move {
        let mut line = vec![b'a'; REQUEST_LINE_LIMIT + 1];
        line.push(b'\n');
        let _ = writer.write_all(&line).await;
        writer
    });
    let mut peer_reader = reader;
    let line = peer_reader.next_line().await?.ok_or("соединение закрыто")?;
    let error = control_error(None, serde_json::from_slice(line)?)?;
    assert_eq!(error.code(), ControlErrorCode::RequestTooLarge);
    assert_eq!(error.detail("limit"), Some(&json!(REQUEST_LINE_LIMIT)));
    assert!(matches!(peer_reader.next_line().await, Ok(None)));
    drop(writing.await?);
    Ok(())
}

#[tokio::test]
async fn malformed_request_keeps_connection() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.greeted().await?;

    let error = control_error(None, peer.call("{\"id\":").await?)?;
    assert_eq!(error.code(), ControlErrorCode::MalformedRequest);
    let error = control_error(Some(5), peer.call(r#"{"id":5,"unknown":{}}"#).await?)?;
    assert_eq!(error.code(), ControlErrorCode::MalformedRequest);
    let scoped = r#"{"id":6,"query":{"query":"status"},"scope":"all"}"#;
    let error = control_error(Some(6), peer.call(scoped).await?)?;
    assert_eq!(error.code(), ControlErrorCode::MalformedRequest);

    peer.status(7).await?;
    Ok(())
}

#[tokio::test]
async fn scope_required_and_unsupported() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.greeted().await?;
    let home = peer
        .tree(1, all())
        .await?
        .active_workspace
        .ok_or("нет активного воркспейса")?;

    let cases = [
        (
            json!({"id": 2, "command": {"cmd": "workspace.create", "title": "X"}}),
            ControlErrorCode::ScopeRequired,
        ),
        (
            json!({"id": 3, "events_since": {"cursor": 0}}),
            ControlErrorCode::ScopeRequired,
        ),
        (
            json!({"id": 4, "events_since": {"cursor": 0}, "scope": in_workspace(home)}),
            ControlErrorCode::ScopeUnsupported,
        ),
        (
            json!({"id": 5, "subscribe": {}}),
            ControlErrorCode::ScopeRequired,
        ),
        (
            json!({"id": 6, "subscribe": {}, "scope": in_workspace(home)}),
            ControlErrorCode::ScopeUnsupported,
        ),
    ];
    for (request, code) in cases {
        let id = request["id"].as_u64();
        let error = control_error(id, peer.call(&request.to_string()).await?)?;
        assert_eq!(error.code(), code, "{request}");
    }
    let (revision, _) = peer.status(7).await?;
    assert_eq!(revision, Revision(1), "отказы области ничего не изменили");
    Ok(())
}

#[tokio::test]
async fn every_command_is_visible_in_tree() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.greeted().await?;
    let home = peer
        .tree(1, all())
        .await?
        .active_workspace
        .ok_or("нет активного воркспейса")?;

    let created = json!({"cmd": "workspace.create", "title": "Работа"});
    let CommandValue::Workspace { workspace: work } =
        command_value(peer.command(2, created, all()).await??)?
    else {
        return Err("ожидался воркспейс".into());
    };
    let tree = peer.tree(3, all()).await?;
    assert_eq!(tree.workspaces.len(), 2);
    assert_eq!(tree.workspaces[1].id, work);
    assert_eq!(tree.workspaces[1].title.as_str(), "Работа");

    peer.command(4, rename_workspace(work, "Проекты"), all())
        .await??;
    let tree = peer.tree(5, all()).await?;
    assert_eq!(tree.workspaces[1].title.as_str(), "Проекты");

    let add = json!({"cmd": "project.add", "workspace": work, "path": "/src/work"});
    let CommandValue::Project { project, root } =
        command_value(peer.command(6, add, all()).await??)?
    else {
        return Err("ожидался проект".into());
    };
    let tree = peer.tree(7, all()).await?;
    let listed = &tree.workspaces[1].projects;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, project);
    assert_eq!(listed[0].roots[0].id, root);
    assert_eq!(listed[0].roots[0].path, Path::new("/src/work"));

    let create = json!({"cmd": "workbench.create", "project": project, "title": "Основная"});
    let CommandValue::Workbench { workbench } =
        command_value(peer.command(8, create, all()).await??)?
    else {
        return Err("ожидалась рабочая область".into());
    };
    let tree = peer.tree(9, all()).await?;
    let benches = &tree.workspaces[1].projects[0].workbenches;
    assert_eq!(benches.len(), 1);
    assert_eq!(benches[0].id, workbench);

    let rename = json!({"cmd": "workbench.rename", "workbench": workbench, "title": "Ревью"});
    peer.command(10, rename, all()).await??;
    let tree = peer.tree(11, all()).await?;
    assert_eq!(
        tree.workspaces[1].projects[0].workbenches[0].title.as_str(),
        "Ревью"
    );

    let open = json!({"cmd": "workbench.open", "workbench": workbench});
    peer.command(12, open, all()).await??;
    let tree = peer.tree(13, all()).await?;
    assert_eq!(tree.active_workspace, Some(work));
    assert_eq!(tree.route, Some(Route::Workbench(workbench)));
    assert_eq!(tree.workspaces[1].last_workbench, Some(workbench));

    let focus = json!({"cmd": "window.focus_workspace", "workspace": home});
    peer.command(14, focus, all()).await??;
    let tree = peer.tree(15, all()).await?;
    assert_eq!(tree.active_workspace, Some(home));
    assert_eq!(fixture.memory.revision(), Revision(8));
    Ok(())
}

/// Команда в чужой области отвергнута ровно `expected`, и ядро не изменилось.
async fn assert_refused(
    peer: &mut Peer,
    id: u64,
    command: Value,
    scope: Value,
    expected: &CoreError,
) -> TestResult {
    let (before, _) = peer.status(id * 10).await?;
    let tree_before = peer.tree(id * 10 + 1, all()).await?;
    let error = peer
        .command(id, command.clone(), scope.clone())
        .await?
        .err()
        .ok_or_else(|| format!("{command} в {scope} прошла"))?;
    assert_eq!(&error, expected, "{command} в {scope}");
    let (after, _) = peer.status(id * 10 + 2).await?;
    assert_eq!(after, before, "{command} в {scope} дала новую ревизию");
    assert_eq!(
        peer.tree(id * 10 + 3, all()).await?,
        tree_before,
        "{command} в {scope} изменила дерево"
    );
    Ok(())
}

#[tokio::test]
async fn scope_matrix_for_two_workspaces() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.greeted().await?;
    let w = world(&mut peer).await?;
    let home = in_workspace(w.home);

    let refused = [
        (
            rename_workspace(w.work, "Чужое"),
            CoreError::not_found(Entity::Workspace, w.work),
        ),
        (
            json!({"cmd": "window.focus_workspace", "workspace": w.work}),
            CoreError::not_found(Entity::Workspace, w.work),
        ),
        (
            json!({"cmd": "project.add", "workspace": w.work, "path": MISSING_PATH}),
            CoreError::not_found(Entity::Workspace, w.work),
        ),
        (
            json!({"cmd": "workbench.create", "project": w.work_project, "title": "Чужая"}),
            CoreError::not_found(Entity::Project, w.work_project),
        ),
        (
            json!({"cmd": "workbench.rename", "workbench": w.work_workbench, "title": "Чужая"}),
            CoreError::not_found(Entity::Workbench, w.work_workbench),
        ),
        (
            json!({"cmd": "workbench.open", "workbench": w.work_workbench}),
            CoreError::not_found(Entity::Workbench, w.work_workbench),
        ),
        (
            json!({"cmd": "workspace.create", "title": "Новый"}),
            CoreError::scope_requires_all("workspace.create"),
        ),
    ];
    for (index, (command, expected)) in refused.into_iter().enumerate() {
        let id = 200 + u64::try_from(index)?;
        assert_refused(&mut peer, id, command, home.clone(), &expected).await?;
    }

    let own = [
        rename_workspace(w.home, "Дом"),
        json!({"cmd": "project.add", "workspace": w.home, "path": "/src/home2"}),
        json!({"cmd": "workbench.create", "project": w.home_project, "title": "Вторая"}),
        json!({"cmd": "workbench.rename", "workbench": w.home_workbench, "title": "Первая"}),
        json!({"cmd": "workbench.open", "workbench": w.home_workbench}),
    ];
    for (index, command) in own.into_iter().enumerate() {
        let id = 300 + u64::try_from(index)?;
        let (before, _) = peer.status(id * 10).await?;
        let reply = peer.command(id, command.clone(), home.clone()).await??;
        assert!(reply.revision > before, "{command} в своей области");
    }
    let tree = peer.tree(400, all()).await?;
    assert_eq!(tree.route, Some(Route::Workbench(w.home_workbench)));

    let (before, _) = peer.status(401).await?;
    let reply = peer
        .command(402, rename_workspace(w.work, "Работа 2"), all())
        .await??;
    assert!(reply.revision > before, "область all проходит");
    let focus_work = json!({"cmd": "window.focus_workspace", "workspace": w.work});
    peer.command(406, focus_work, all()).await??;
    let focus_home = json!({"cmd": "window.focus_workspace", "workspace": w.home});
    peer.command(407, focus_home, home.clone()).await??;

    let own_tree = peer.tree(403, home.clone()).await?;
    assert_eq!(own_tree.active_workspace, Some(w.home));
    assert!(own_tree.route.is_some());
    assert_eq!(
        own_tree
            .workspaces
            .iter()
            .map(|ws| ws.id)
            .collect::<Vec<_>>(),
        [w.home]
    );

    let line = json!({"id": 404, "query": {"query": "tree", "scope": in_workspace(w.work)}});
    let raw = peer.call_value(&line.to_string()).await?;
    let tree = &raw["ok"]["value"];
    assert_eq!(tree["active_workspace"], Value::Null);
    assert_eq!(tree["route"], Value::Null);
    let ids: Vec<&Value> = tree["workspaces"]
        .as_array()
        .ok_or("нет воркспейсов")?
        .iter()
        .map(|ws| &ws["id"])
        .collect();
    assert_eq!(ids, [&json!(w.work)], "дерево области не раскрывает home");

    let everything = peer.tree(405, all()).await?;
    assert_eq!(everything.workspaces.len(), 2);
    assert_eq!(everything.active_workspace, Some(w.home));
    Ok(())
}

#[tokio::test]
async fn events_since_inside_buffer_and_cursor_ahead() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.greeted().await?;
    let home = peer
        .tree(1, all())
        .await?
        .active_workspace
        .ok_or("нет активного воркспейса")?;
    for (index, id) in (2..4).enumerate() {
        peer.command(id, rename_workspace(home, alternating(index)), all())
            .await??;
    }

    let response = peer.events_since(4, 1).await?;
    let Response::Ok {
        id: RequestId(4),
        value: OkValue::EventsSince(EventsSince::Events(events)),
    } = response
    else {
        return Err(format!("ожидались события, получено {response:?}").into());
    };
    let revisions: Vec<u64> = events.iter().map(|event| event.revision.0).collect();
    assert_eq!(revisions, [2, 3]);

    let response = peer.events_since(5, 3).await?;
    assert!(matches!(
        response,
        Response::Ok { value: OkValue::EventsSince(EventsSince::Events(ref events)), .. }
            if events.is_empty()
    ));

    let error = reply_or_core_error(6, peer.events_since(6, 4).await?)?
        .err()
        .ok_or("курсор впереди прошёл")?;
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
    assert_eq!(error.detail_str("reason"), Some("cursor_ahead"));
    Ok(())
}

#[tokio::test]
async fn events_since_outside_buffer_is_reset() -> TestResult {
    let mut fixture = Fixture::start().await?;
    {
        let mut peer = fixture.greeted().await?;
        let created = json!({"cmd": "workspace.create", "title": "Работа"});
        peer.command(2, created, all()).await??;
    }
    fixture.shutdown()?.await?;
    let restarted = Fixture::start_on(fixture.memory.clone()).await?;
    let mut peer = restarted.greeted().await?;

    let response = peer.events_since(3, 1).await?;
    let Response::Ok {
        value: OkValue::EventsSince(EventsSince::Reset(snapshot)),
        ..
    } = response
    else {
        return Err(format!("ожидался снимок, получено {response:?}").into());
    };
    assert_eq!(snapshot.revision, Revision(2));
    assert_eq!(*snapshot.state, restarted.memory.state());
    Ok(())
}

/// Рабочие области, созданные командой, получают раскладку при перезапуске. Курсор прошлого
/// запуска получает снимок протокола v2 с панелями и раскладками целиком.
#[tokio::test]
async fn reset_after_restart_carries_full_layout() -> TestResult {
    let mut fixture = Fixture::start().await?;
    let world = {
        let mut peer = fixture.greeted().await?;
        world(&mut peer).await?
    };
    let before = fixture.memory.revision();
    fixture.shutdown()?.await?;
    let mut laid_out = Fixture::start_on(fixture.memory.clone()).await?;
    assert_eq!(laid_out.memory.revision(), before.next());
    laid_out.shutdown()?.await?;
    let restarted = Fixture::start_on(fixture.memory.clone()).await?;
    let mut peer = restarted.greeted().await?;

    let line = json!({"id": 3, "events_since": {"cursor": before.0}, "scope": "all"});
    let frame = peer.call_value(&line.to_string()).await?;
    let state = &frame["ok"]["reset"]["state"];
    for workbench in [world.home_workbench, world.work_workbench] {
        let layout = &state["layouts"][workbench.to_string()];
        let navigator = layout["navigator"]
            .as_str()
            .ok_or("нет колонки навигатора")?;
        assert_eq!(state["panels"][navigator]["plugin"], "navigator", "{frame}");
        let terminal = &layout["zones"]["center"]["parts"][0]["tabs"][0];
        let terminal = terminal.as_str().ok_or("нет терминала в центре")?;
        assert_eq!(state["panels"][terminal]["plugin"], "terminal", "{frame}");
        assert_eq!(
            state["panels"][terminal]["workbench"],
            workbench.to_string()
        );
    }

    let Response::Ok {
        value: OkValue::EventsSince(EventsSince::Reset(snapshot)),
        ..
    } = serde_json::from_value(frame)?
    else {
        return Err("ожидался снимок".into());
    };
    assert_eq!(snapshot.revision, before.next());
    assert_eq!(*snapshot.state, restarted.memory.state());
    assert_eq!(snapshot.state.layouts.len(), 2);
    assert_eq!(snapshot.state.panels.len(), 4);
    assert_eq!(snapshot.state.check_invariants(), Vec::new());
    Ok(())
}

fn snapshot_frame(id: u64, response: Response) -> TestResult<Snapshot> {
    match response {
        Response::Snapshot { id: got, snapshot } if got == RequestId(id) => Ok(snapshot),
        other => Err(format!("ожидался снимок подписки {id}, получено {other:?}").into()),
    }
}

fn event_frame(id: u64, response: Response) -> TestResult<Event> {
    match response {
        Response::Event { id: got, event } if got == RequestId(id) => Ok(event),
        other => Err(format!("ожидалось событие подписки {id}, получено {other:?}").into()),
    }
}

#[tokio::test]
async fn subscription_sends_snapshot_then_events() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut subscriber = fixture.greeted().await?;
    subscriber.send(&subscribe(9)).await?;
    let snapshot = snapshot_frame(9, subscriber.recv().await?)?;
    assert_eq!(snapshot.revision, Revision(1));
    let home = snapshot
        .state
        .window
        .active_workspace
        .ok_or("нет активного воркспейса")?;

    let mut other = fixture.greeted().await?;
    for (index, id) in (2..4).enumerate() {
        other
            .command(id, rename_workspace(home, alternating(index)), all())
            .await??;
    }
    for revision in [2, 3] {
        let event = event_frame(9, subscriber.recv().await?)?;
        assert_eq!(event.revision, Revision(revision));
    }
    Ok(())
}

#[tokio::test]
async fn lagged_subscriber_gets_reset_with_fresh_snapshot() -> TestResult {
    let fixture = Fixture::start().await?;
    fixture.server()?.hooks().pause_subscriptions();
    let mut subscriber = fixture.greeted().await?;
    subscriber.send(&subscribe(9)).await?;
    let snapshot = snapshot_frame(9, subscriber.recv().await?)?;
    let home = snapshot
        .state
        .window
        .active_workspace
        .ok_or("нет активного воркспейса")?;

    let mut other = fixture.greeted().await?;
    let changes = SUBSCRIBER_CAPACITY + 1;
    for index in 0..changes {
        let id = 10 + u64::try_from(index)?;
        other
            .command(id, rename_workspace(home, alternating(index)), all())
            .await??;
    }
    let lagged = Revision(snapshot.revision.0 + u64::try_from(changes)?);
    fixture.server()?.hooks().resume_subscriptions();

    match subscriber.recv().await? {
        Response::Reset {
            id: RequestId(9),
            snapshot,
        } => {
            assert_eq!(snapshot.revision, lagged);
            assert_eq!(*snapshot.state, fixture.memory.state());
        }
        other => return Err(format!("ожидался reset, получено {other:?}").into()),
    }
    for (index, revision) in (1..3).enumerate() {
        other
            .command(
                1000 + revision,
                rename_workspace(home, alternating(index + 1)),
                all(),
            )
            .await??;
        let event = event_frame(9, subscriber.recv().await?)?;
        assert_eq!(event.revision, Revision(lagged.0 + revision));
    }
    Ok(())
}

#[tokio::test]
async fn slow_subscriber_does_not_delay_other_connections() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut subscriber = fixture.greeted().await?;
    subscriber.send(&subscribe(9)).await?;
    let snapshot = snapshot_frame(9, subscriber.recv().await?)?;
    let home = snapshot
        .state
        .window
        .active_workspace
        .ok_or("нет активного воркспейса")?;

    // Подписчик больше не читает: его буфер сокета заполняется длинными событиями, задача
    // подписки встаёт на записи и отстаёт больше ёмкости канала событий.
    let mut other = fixture.greeted().await?;
    let long = ["Ж".repeat(200), "Щ".repeat(200)];
    let changes = 3 * SUBSCRIBER_CAPACITY;
    for index in 0..changes {
        let id = 10 + u64::try_from(index)?;
        other
            .command(id, rename_workspace(home, &long[index % 2]), all())
            .await??;
    }
    let last = Revision(snapshot.revision.0 + u64::try_from(changes)?);
    assert_eq!(fixture.memory.revision(), last);

    // Теперь подписчик читает: где-то в потоке есть `reset`, значит он и правда отстал.
    let mut previous = snapshot.revision;
    loop {
        match subscriber.recv().await? {
            Response::Event {
                id: RequestId(9),
                event,
            } => {
                assert!(event.revision < last, "подписчик не отстал");
                assert_eq!(event.revision, previous.next(), "события идут подряд");
                previous = event.revision;
            }
            Response::Reset {
                id: RequestId(9),
                snapshot,
            } => {
                assert_eq!(snapshot.revision, last);
                break;
            }
            other => return Err(format!("неожиданный кадр подписки {other:?}").into()),
        }
    }
    Ok(())
}

#[tokio::test]
async fn dropped_clients_do_not_disturb_others() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut subscriber = fixture.greeted().await?;
    subscriber.send(&subscribe(9)).await?;
    snapshot_frame(9, subscriber.recv().await?)?;
    drop(subscriber);
    let mut partial = fixture.greeted().await?;
    partial.writer.write_all(br#"{"id":2,"command":"#).await?;
    drop(partial);

    let mut peer = fixture.greeted().await?;
    let home = peer
        .tree(1, all())
        .await?
        .active_workspace
        .ok_or("нет активного воркспейса")?;
    for (index, id) in (2..5).enumerate() {
        peer.command(id, rename_workspace(home, alternating(index)), all())
            .await??;
    }
    let (revision, _) = peer.status(5).await?;
    assert_eq!(revision, Revision(4));
    Ok(())
}

#[tokio::test]
async fn launch_goes_to_handler_and_returns_receipt() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.greeted().await?;
    let line = json!({"id": 6, "launch": {"args": ["/abs/path"], "cwd": "/abs"}}).to_string();
    let reply = peer.call_value(&line).await?;
    assert_eq!(
        reply,
        json!({"id": 6, "ok": {"launch": {
            "activation": "no_ui",
            "refused": [{"arg": "/abs/path", "reason": "folder_open_unsupported"}]
        }}})
    );
    assert_eq!(
        fixture.launcher.launches(),
        [Launch {
            args: vec!["/abs/path".to_owned()],
            cwd: PathBuf::from("/abs"),
        }]
    );
    Ok(())
}

#[tokio::test]
async fn held_reply_is_sent_after_core_receipt() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.greeted().await?;
    let home = peer
        .tree(1, all())
        .await?
        .active_workspace
        .ok_or("нет активного воркспейса")?;

    let mut hold = fixture.server()?.hooks().hold_next_reply();
    let line = json!({"id": 2, "command": rename_workspace(home, "Альфа"), "scope": "all"});
    peer.send(&line.to_string()).await?;
    hold.entered().await;
    assert_eq!(fixture.memory.revision(), Revision(2), "ядро уже ответило");
    hold.release();
    let reply = reply_or_core_error(2, peer.recv().await?)??;
    assert_eq!(reply.revision, Revision(2));
    Ok(())
}

#[tokio::test]
async fn request_at_admission_barrier_gets_owner_leaving() -> TestResult {
    let mut fixture = Fixture::start().await?;
    let mut peer = fixture.greeted().await?;
    let home = peer
        .tree(1, all())
        .await?
        .active_workspace
        .ok_or("нет активного воркспейса")?;
    let before = fixture.memory.state();
    let commits = fixture.faults.commits();

    let mut barrier = fixture.server()?.hooks().barrier_next_admission();
    let line = json!({"id": 2, "command": rename_workspace(home, "Альфа"), "scope": "all"});
    peer.send(&line.to_string()).await?;
    barrier.entered().await;
    let done = fixture.shutdown()?;
    barrier.release();

    let error = control_error(Some(2), peer.recv().await?)?;
    assert_eq!(error.code(), ControlErrorCode::OwnerLeaving);
    assert!(error.retryable());
    peer.assert_closed().await?;
    done.await?;
    assert_eq!(fixture.faults.commits(), commits, "ядро запрос не получило");
    assert_eq!(fixture.memory.state(), before);
    assert!(fixture.faults.is_dropped(), "ядро остановлено");
    Ok(())
}

#[tokio::test]
async fn hello_while_leaving_reports_leaving_owner() -> TestResult {
    let mut fixture = Fixture::start().await?;
    let mut peer = fixture.connect().await?;
    let mut barrier = fixture.server()?.hooks().barrier_next_admission();
    peer.send(&hello(1)).await?;
    barrier.entered().await;
    let done = fixture.shutdown()?;
    barrier.release();

    match peer.recv().await? {
        Response::Ok {
            id: RequestId(1),
            value: OkValue::Hello(reply),
        } => assert_eq!(reply.owner, OwnerState::Leaving),
        other => return Err(format!("ожидался hello, получено {other:?}").into()),
    }
    peer.assert_closed().await?;
    done.await?;
    Ok(())
}

#[tokio::test]
async fn shutdown_finishes_admitted_command_and_closes_the_rest() -> TestResult {
    let mut fixture = Fixture::start().await?;
    let mut idle = fixture.greeted().await?;
    let mut subscriber = fixture.greeted().await?;
    subscriber.send(&subscribe(9)).await?;
    let snapshot = snapshot_frame(9, subscriber.recv().await?)?;
    let home = snapshot
        .state
        .window
        .active_workspace
        .ok_or("нет активного воркспейса")?;

    // Подписчик больше не читает: события забивают его буфер сокета.
    let mut other = fixture.greeted().await?;
    let long = ["Ж".repeat(200), "Щ".repeat(200)];
    for index in 0..SUBSCRIBER_CAPACITY {
        let id = 10 + u64::try_from(index)?;
        other
            .command(id, rename_workspace(home, &long[index % 2]), all())
            .await??;
    }

    let mut barrier = fixture.faults.barrier_next_commit();
    let mut committing = fixture.greeted().await?;
    let line = json!({"id": 3, "command": rename_workspace(home, "Финал"), "scope": "all"});
    committing.send(&line.to_string()).await?;
    barrier.entered().await;

    let done = fixture.shutdown()?;
    idle.assert_closed().await?;
    other.assert_closed().await?;
    barrier.release();
    let reply = reply_or_core_error(3, committing.recv().await?)??;
    assert_eq!(reply.revision, fixture.memory.revision());
    assert_eq!(
        fixture.memory.state().workspaces[&home].title.as_str(),
        "Финал",
        "принятая команда записана"
    );
    committing.assert_closed().await?;

    // Запись подписчику, не читающему сокет, обрывается по сроку завершения.
    fixture.clock.expire();
    done.await?;
    assert!(fixture.faults.is_dropped(), "ядро остановлено");

    let mut leaving = false;
    loop {
        let line = match subscriber.reader.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) | Err(LineError::Truncated { .. }) => break,
            Err(error) => return Err(error.into()),
        };
        assert!(!leaving, "после owner_leaving кадров нет");
        match serde_json::from_slice(line)? {
            Response::Event {
                id: RequestId(9), ..
            }
            | Response::Reset {
                id: RequestId(9), ..
            } => {}
            Response::Error {
                id: Some(RequestId(9)),
                error,
            } if error.code() == ControlErrorCode::OwnerLeaving => leaving = true,
            other => return Err(format!("неожиданный кадр подписки {other:?}").into()),
        }
    }
    Ok(())
}
