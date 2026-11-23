//! Клиент управления в процессе теста: настоящие блокировка, сокет и сервер над ядром
//! в памяти либо поддельный владелец на сыром сокете.
//!
//! У клиента свои ручные часы: тест решает, когда попытка продолжается и когда истекает срок.
//! Синхронизация — ожидания часов, крючки сервера и строки поддельного владельца.

use std::error::Error;
use std::fs;
use std::future::{Future, ready};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use shellio_control::test_support::ManualClock;
use shellio_control::{
    Accepted, Activation, Address, Client, ClientError, ControlErrorCode, Exit, GalleryCommand,
    HelloReply, LOCK_FILE, Launch, LaunchHandler, LaunchReceipt, LineReader, OkValue, OwnerState,
    Ownership, PROTOCOL, REQUEST_LINE_LIMIT, RequestBody, RequestId, Response, SHUTDOWN_GRACE,
    SOCKET_FILE, Server, ServerConfig, Session, UiReply, UiRequest, WithoutUi, encode_line,
};
use shellio_core::test_support::{
    FakeRootResolver, ManualClock as CoreClock, MemoryPersist, SequentialIds,
};
use shellio_core::{
    Command, Core, Query, Reply, ReplyValue, Revision, Snapshot, State, TreeScope, UiCommand,
    UnixMillis,
};
use shellio_store::Profile;
use tempfile::TempDir;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
type Outcome = Result<Response, ClientError>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);
const PID: u32 = 4242;
const TIMEOUT: Duration = Duration::from_secs(5);

/// Обработчик запуска без отказов.
struct Launcher;

impl LaunchHandler for Launcher {
    fn launch(&self, _launch: Launch) -> impl Future<Output = LaunchReceipt> + Send {
        ready(LaunchReceipt {
            activation: Activation::NoUi,
            refused: Vec::new(),
        })
    }
}

/// Профиль во временном каталоге `/tmp`: путь сокета укладывается в `sun_path`. Каталог
/// состояния не создаётся.
struct Dir {
    profile: Profile,
    dir: TempDir,
}

impl Dir {
    fn new() -> TestResult<Self> {
        let dir = tempfile::Builder::new()
            .prefix("shellio-")
            .tempdir_in("/tmp")?;
        let profile = Profile::isolated(dir.path().join("state"));
        Ok(Self { profile, dir })
    }

    fn address(&self) -> Address {
        Address::Profile(self.profile.clone())
    }

    /// Взять блокировку, как претендент, не публикуя сокет.
    fn hold(&self) -> TestResult<Ownership> {
        self.profile.ensure_state_dir()?;
        Ok(Ownership::try_acquire(&self.profile)?.ok_or("блокировка занята")?)
    }

    fn entries(&self) -> TestResult<Vec<String>> {
        let mut names = fs::read_dir(self.profile.state_dir())?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>, std::io::Error>>()?;
        names.sort();
        Ok(names)
    }
}

/// Владелец: блокировка, опубликованный сокет и сервер над ядром в памяти.
struct Owner {
    server: Option<Server<ManualClock, Launcher>>,
    memory: MemoryPersist,
    ownership: Option<Ownership>,
}

impl Owner {
    async fn start(dir: &Dir) -> TestResult<Self> {
        Self::start_with(dir.hold()?).await
    }

    async fn start_with(mut ownership: Ownership) -> TestResult<Self> {
        let listener = ownership.publish()?;
        let memory = MemoryPersist::new();
        let core = Core::start(
            &Handle::current(),
            Box::new(memory.clone()),
            Box::new(CoreClock::new(NOW)),
            Box::new(SequentialIds::new()),
            Arc::new(FakeRootResolver::new()),
        )?;
        let config = ServerConfig {
            server: "test 0.1.0".to_owned(),
            pid: PID,
            grace: SHUTDOWN_GRACE,
        };
        let server = Server::start(
            &Handle::current(),
            listener,
            core,
            Launcher,
            WithoutUi,
            ManualClock::new(),
            config,
        )?;
        Ok(Self {
            server: Some(server),
            memory,
            ownership: Some(ownership),
        })
    }

    fn server(&self) -> TestResult<&Server<ManualClock, Launcher>> {
        Ok(self.server.as_ref().ok_or("сервер уже завершён")?)
    }

    /// Начать завершение: переход в `leaving` происходит при вызове.
    fn shutdown(&mut self) -> TestResult<JoinHandle<()>> {
        let server = self.server.take().ok_or("сервер уже завершён")?;
        Ok(tokio::spawn(server.shutdown()))
    }

    /// Завершить сервер, удалить сокет и отпустить блокировку.
    async fn stop(mut self) -> TestResult {
        if self.server.is_some() {
            self.shutdown()?.await?;
        }
        if let Some(ownership) = self.ownership.take() {
            ownership.release()?;
        }
        Ok(())
    }
}

fn client(address: Address) -> (Client<ManualClock>, ManualClock) {
    let clock = ManualClock::new();
    let client = Client::new(address, clock.clone(), "test-client 0.1.0", TIMEOUT);
    (client, clock)
}

fn tree_all() -> RequestBody {
    RequestBody::Query(Query::Tree {
        scope: TreeScope::All,
    })
}

fn create(title: &str) -> RequestBody {
    RequestBody::Command {
        command: Command::WorkspaceCreate {
            title: title.to_owned(),
        },
        scope: TreeScope::All,
    }
}

/// Изменяющие запросы `ui`: действие окна и служебная команда галереи.
fn ui_mutations() -> [RequestBody; 2] {
    [
        RequestBody::Ui(UiRequest::Command(UiCommand::AppQuit)),
        RequestBody::Ui(UiRequest::Gallery(GalleryCommand::Reset)),
    ]
}

/// Чтения `ui`.
fn ui_reads() -> [RequestBody; 3] {
    [
        RequestBody::Ui(UiRequest::Window),
        RequestBody::Ui(UiRequest::Inspect),
        RequestBody::Ui(UiRequest::Stats),
    ]
}

fn launch() -> RequestBody {
    RequestBody::Launch(Launch {
        args: vec!["/tmp".to_owned()],
        cwd: PathBuf::from("/"),
    })
}

/// Код отказа клиента или `None`, если вызов дошёл до кадра.
fn refusal(outcome: &Outcome) -> Option<ControlErrorCode> {
    match outcome {
        Err(ClientError::Control(error)) => Some(error.code()),
        Ok(_) | Err(ClientError::Failed(_)) => None,
    }
}

fn exit(outcome: &Outcome) -> Exit {
    match outcome {
        Ok(response) => Exit::of_response(response),
        Err(error) => Exit::of_error(error),
    }
}

fn reply(response: Response) -> TestResult<Reply> {
    match response {
        Response::Ok {
            id: RequestId(2),
            value: OkValue::Reply(reply),
        } => Ok(reply),
        other => Err(format!("ожидался ответ ядра на 2, получено {other:?}").into()),
    }
}

/// Поддельный владелец на сыром сокете.
struct Fake {
    listener: UnixListener,
    socket: PathBuf,
    _dir: TempDir,
}

impl Fake {
    fn bind() -> TestResult<Self> {
        let dir = tempfile::Builder::new()
            .prefix("shellio-")
            .tempdir_in("/tmp")?;
        let socket = dir.path().join("fake.sock");
        let listener = UnixListener::bind(&socket)?;
        Ok(Self {
            listener,
            socket,
            _dir: dir,
        })
    }

    fn client(&self) -> (Client<ManualClock>, ManualClock) {
        client(Address::Socket(self.socket.clone()))
    }

    async fn accept(&self) -> TestResult<Peer> {
        let (stream, _) = self.listener.accept().await?;
        let (reader, writer) = stream.into_split();
        Ok(Peer {
            reader: LineReader::new(BufReader::new(reader), REQUEST_LINE_LIMIT),
            writer,
        })
    }
}

/// Соединение поддельного владельца с клиентом.
struct Peer {
    reader: LineReader<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
}

impl Peer {
    /// Следующая строка клиента, `None` — клиент закрыл соединение.
    async fn read_line(&mut self) -> TestResult<Option<String>> {
        Ok(self
            .reader
            .next_line()
            .await?
            .map(|line| String::from_utf8_lossy(line).into_owned()))
    }

    async fn write(&mut self, bytes: &[u8]) -> TestResult {
        self.writer.write_all(bytes).await?;
        Ok(())
    }

    async fn write_frame(&mut self, response: &Response) -> TestResult {
        self.write(&encode_line(response)?).await
    }

    /// Прочитать `hello` и ответить готовым владельцем.
    async fn greet(&mut self) -> TestResult {
        let hello = self.read_line().await?.ok_or("hello не пришёл")?;
        assert!(hello.contains("\"hello\""), "{hello}");
        self.write_frame(&Response::Ok {
            id: RequestId(1),
            value: OkValue::Hello(HelloReply {
                protocol: PROTOCOL,
                server: "fake".to_owned(),
                pid: 1,
                owner: OwnerState::Ready,
            }),
        })
        .await
    }
}

/// Поддельный владелец отвечает на `hello`, читает запрос и выполняет `then`.
async fn fake_after_request<F>(body: RequestBody, then: F) -> TestResult<Outcome>
where
    F: AsyncFnOnce(Peer) -> TestResult,
{
    let fake = Fake::bind()?;
    let (client, _clock) = fake.client();
    let call = tokio::spawn(async move { client.call(body).await });
    let mut peer = fake.accept().await?;
    peer.greet().await?;
    peer.read_line().await?.ok_or("запрос не пришёл")?;
    then(peer).await?;
    Ok(call.await?)
}

#[tokio::test]
async fn free_or_missing_lock_is_not_running_and_creates_nothing() -> TestResult {
    let dir = Dir::new()?;
    let (client, _clock) = client(dir.address());

    let missing_dir = client.call(tree_all()).await;
    assert_eq!(refusal(&missing_dir), Some(ControlErrorCode::NotRunning));
    assert_eq!(exit(&missing_dir), Exit::NotRunning);
    assert!(!dir.profile.state_dir().exists(), "каталог не создан");

    dir.profile.ensure_state_dir()?;
    let empty = client.call(create("Альфа")).await;
    assert_eq!(refusal(&empty), Some(ControlErrorCode::NotRunning));
    assert!(dir.entries()?.is_empty(), "файлы не созданы");

    drop(dir.hold()?);
    let free = client.version().await;
    assert_eq!(refusal(&free), Some(ControlErrorCode::NotRunning));
    assert_eq!(
        dir.entries()?,
        [LOCK_FILE],
        "файл блокировки на месте, сокета нет"
    );
    Ok(())
}

#[tokio::test]
async fn held_lock_without_socket_is_owner_starting() -> TestResult {
    let dir = Dir::new()?;
    let _contender = dir.hold()?;
    let (client, clock) = client(dir.address());
    let call = tokio::spawn(async move { client.call(create("Альфа")).await });

    clock.until_waiting(1).await;
    clock.tick();
    clock.until_waiting(2).await;
    clock.expire();
    let outcome = call.await?;
    assert_eq!(refusal(&outcome), Some(ControlErrorCode::OwnerStarting));
    assert_eq!(
        exit(&outcome),
        Exit::Retry,
        "срок до отправки — повторяемый исход"
    );
    assert_eq!(dir.entries()?, [LOCK_FILE], "клиент сокет не создаёт");
    Ok(())
}

#[tokio::test]
async fn lock_released_during_wait_is_not_running() -> TestResult {
    let dir = Dir::new()?;
    let contender = dir.hold()?;
    let (client, clock) = client(dir.address());
    let call = tokio::spawn(async move { client.call(tree_all()).await });

    clock.until_waiting(1).await;
    contender.release()?;
    clock.tick();
    let outcome = call.await?;
    assert_eq!(refusal(&outcome), Some(ControlErrorCode::NotRunning));
    Ok(())
}

#[tokio::test]
async fn client_waits_for_socket_of_starting_owner() -> TestResult {
    let dir = Dir::new()?;
    let contender = dir.hold()?;
    let (client, clock) = client(dir.address());
    let call = tokio::spawn(async move { client.call(tree_all()).await });

    clock.until_waiting(1).await;
    let owner = Owner::start_with(contender).await?;
    clock.tick();
    let tree = reply(call.await??)?;
    assert!(matches!(tree.value, ReplyValue::Tree(_)), "{tree:?}");
    owner.stop().await
}

#[tokio::test]
async fn explicit_missing_socket_is_unavailable_without_probe() -> TestResult {
    let dir = Dir::new()?;
    dir.profile.ensure_state_dir()?;
    let socket = dir.dir.path().join("missing.sock");
    let (client, clock) = client(Address::Socket(socket));
    let call = tokio::spawn(async move { client.version().await });

    clock.until_waiting(1).await;
    clock.expire();
    let outcome = call.await?;
    assert_eq!(refusal(&outcome), Some(ControlErrorCode::Unavailable));
    assert_eq!(exit(&outcome), Exit::Retry);
    assert!(dir.entries()?.is_empty(), "проба блокировки не делалась");
    Ok(())
}

#[tokio::test]
async fn probe_failure_is_not_retryable() -> TestResult {
    let dir = Dir::new()?;
    // Каталог состояния — обычный файл: файл блокировки в нём не открыть (`ENOTDIR`).
    fs::write(dir.profile.state_dir(), b"")?;
    let (client, clock) = client(dir.address());

    let outcome = client.call(tree_all()).await;
    assert!(
        matches!(&outcome, Err(ClientError::Failed(reason)) if reason.starts_with("проба владельца не удалась")),
        "{outcome:?}"
    );
    assert_eq!(exit(&outcome), Exit::Failure);
    assert_eq!(clock.waits(), 0, "повтора нет");
    Ok(())
}

/// Поддельный владелец протокола v1: клиент v2 получает отказ `hello` до первой команды.
#[tokio::test]
async fn hello_refusal_is_the_outcome_without_request() -> TestResult {
    let refuse = async |fake: &Fake| -> TestResult<Peer> {
        let mut peer = fake.accept().await?;
        let hello = peer.read_line().await?.ok_or("hello не пришёл")?;
        let hello: serde_json::Value = serde_json::from_str(&hello)?;
        assert_eq!(hello["hello"]["protocol"], PROTOCOL, "{hello}");
        assert_eq!(PROTOCOL, 2);
        let refusal = serde_json::json!({"id": 1, "error": {
            "code": "protocol_mismatch",
            "message": "версия протокола 2 не поддерживается, сервер поддерживает 1",
            "retryable": false,
            "details": {"supported": [1]},
        }});
        peer.write(format!("{refusal}\n").as_bytes()).await?;
        Ok(peer)
    };
    let is_mismatch = |frame: &Response| {
        matches!(frame, Response::Error { id: Some(RequestId(1)), error }
            if error.code() == ControlErrorCode::ProtocolMismatch
                && error.detail("supported") == Some(&serde_json::json!([1])))
    };

    let fake = Fake::bind()?;
    let (client, _clock) = fake.client();
    let call = tokio::spawn(async move { client.call(create("Альфа")).await });
    let mut peer = refuse(&fake).await?;
    let outcome = call.await?;
    assert!(
        matches!(&outcome, Ok(frame) if is_mismatch(frame)),
        "{outcome:?}"
    );
    assert_eq!(exit(&outcome), Exit::Failure);
    assert_eq!(peer.read_line().await?, None, "команда не отправлена");

    let (client, _clock) = fake.client();
    let subscribe = tokio::spawn(async move {
        let mut subscription = client.subscribe().await?;
        subscription.next().await
    });
    let mut peer = refuse(&fake).await?;
    let first = subscribe.await?;
    assert!(
        matches!(&first, Ok(frame) if is_mismatch(frame)),
        "{first:?}"
    );
    assert_eq!(exit(&first), Exit::Failure);
    assert_eq!(peer.read_line().await?, None, "подписка не отправлена");
    Ok(())
}

#[tokio::test]
async fn request_over_server_limit_is_refused_before_sending() -> TestResult {
    let fake = Fake::bind()?;
    let (client, clock) = fake.client();
    let title = "а".repeat(REQUEST_LINE_LIMIT / 2 + 1);
    let call = tokio::spawn(async move { client.call(create(&title)).await });

    let mut peer = fake.accept().await?;
    peer.greet().await?;
    let outcome = call.await?;
    assert_eq!(refusal(&outcome), Some(ControlErrorCode::RequestTooLarge));
    assert_eq!(exit(&outcome), Exit::Failure, "отказ не повторяется");
    assert_eq!(clock.waits(), 0);
    assert_eq!(peer.read_line().await?, None, "строка не отправлена");
    Ok(())
}

#[tokio::test]
async fn deadline_before_hello_reply_is_retryable_without_request() -> TestResult {
    let fake = Fake::bind()?;
    let (client, clock) = fake.client();
    let call = tokio::spawn(async move { client.call(create("Альфа")).await });

    let mut peer = fake.accept().await?;
    peer.read_line().await?.ok_or("hello не пришёл")?;
    clock.expire();
    let outcome = call.await?;
    assert_eq!(refusal(&outcome), Some(ControlErrorCode::Unavailable));
    assert_eq!(exit(&outcome), Exit::Retry);
    assert_eq!(peer.read_line().await?, None, "команда не отправлена");
    Ok(())
}

#[tokio::test]
async fn deadline_after_core_receipt_is_outcome_unknown_for_mutation() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start(&dir).await?;
    let before = owner.memory.revision();
    let (client, clock) = client(dir.address());

    let mut hold = owner.server()?.hooks().hold_next_reply();
    let call = tokio::spawn(async move { client.call(create("Альфа")).await });
    hold.entered().await;
    clock.expire();
    let outcome = call.await?;
    assert_eq!(refusal(&outcome), Some(ControlErrorCode::OutcomeUnknown));
    assert_eq!(exit(&outcome), Exit::OutcomeUnknown);
    hold.release();
    assert_eq!(
        owner.memory.revision(),
        Revision(before.0 + 1),
        "команда записана, хотя клиент исхода не знает"
    );
    owner.stop().await
}

#[tokio::test]
async fn deadline_after_core_receipt_is_retryable_for_read() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start(&dir).await?;
    let (client, clock) = client(dir.address());

    let mut hold = owner.server()?.hooks().hold_next_reply();
    let call = tokio::spawn(async move { client.call(tree_all()).await });
    hold.entered().await;
    clock.expire();
    let outcome = call.await?;
    assert_eq!(refusal(&outcome), Some(ControlErrorCode::Unavailable));
    assert_eq!(exit(&outcome), Exit::Retry);
    hold.release();
    owner.stop().await
}

#[tokio::test]
async fn hangup_after_sent_request_depends_on_mutation() -> TestResult {
    let hangup = async |_peer: Peer| Ok(());
    for body in [create("Альфа"), launch()]
        .into_iter()
        .chain(ui_mutations())
    {
        let outcome = fake_after_request(body, hangup).await?;
        assert_eq!(refusal(&outcome), Some(ControlErrorCode::OutcomeUnknown));
        assert_eq!(exit(&outcome), Exit::OutcomeUnknown);
    }
    for body in [
        tree_all(),
        RequestBody::EventsSince {
            cursor: Revision(0),
        },
    ]
    .into_iter()
    .chain(ui_reads())
    {
        let outcome = fake_after_request(body, hangup).await?;
        assert_eq!(refusal(&outcome), Some(ControlErrorCode::Unavailable));
        assert_eq!(exit(&outcome), Exit::Retry);
    }
    Ok(())
}

#[tokio::test]
async fn bad_response_after_sent_request_depends_on_mutation() -> TestResult {
    let garbage = async |mut peer: Peer| peer.write(b"not json\n").await;
    let foreign = async |mut peer: Peer| {
        peer.write_frame(&Response::Ok {
            id: RequestId(7),
            value: OkValue::Launch(LaunchReceipt {
                activation: Activation::NoUi,
                refused: Vec::new(),
            }),
        })
        .await
    };

    let outcome = fake_after_request(create("Альфа"), garbage).await?;
    assert_eq!(refusal(&outcome), Some(ControlErrorCode::OutcomeUnknown));
    let outcome = fake_after_request(launch(), foreign).await?;
    assert_eq!(refusal(&outcome), Some(ControlErrorCode::OutcomeUnknown));

    let outcome = fake_after_request(tree_all(), garbage).await?;
    assert!(
        matches!(outcome, Err(ClientError::Failed(_))),
        "{outcome:?}"
    );
    assert_eq!(exit(&outcome), Exit::Failure);
    let outcome = fake_after_request(tree_all(), foreign).await?;
    assert!(
        matches!(outcome, Err(ClientError::Failed(_))),
        "{outcome:?}"
    );

    for body in ui_mutations() {
        let outcome = fake_after_request(body.clone(), garbage).await?;
        assert_eq!(refusal(&outcome), Some(ControlErrorCode::OutcomeUnknown));
        let outcome = fake_after_request(body, foreign).await?;
        assert_eq!(refusal(&outcome), Some(ControlErrorCode::OutcomeUnknown));
    }
    for body in ui_reads() {
        let outcome = fake_after_request(body.clone(), garbage).await?;
        assert!(
            matches!(outcome, Err(ClientError::Failed(_))),
            "{outcome:?}"
        );
        let outcome = fake_after_request(body, foreign).await?;
        assert!(
            matches!(outcome, Err(ClientError::Failed(_))),
            "{outcome:?}"
        );
    }
    Ok(())
}

/// Квитанция `ui` чужого запроса: изменение без исхода, чтение — сбой без кода.
#[tokio::test]
async fn ui_reply_of_another_kind_is_not_an_answer() -> TestResult {
    let accepted = async |mut peer: Peer| {
        peer.write_frame(&Response::Ok {
            id: RequestId(2),
            value: OkValue::Ui(UiReply::Accepted(Accepted {
                command: "app.quit".to_owned(),
                pid: PID,
                generation: 1,
            })),
        })
        .await
    };
    for body in ui_reads() {
        let outcome = fake_after_request(body, accepted).await?;
        assert!(
            matches!(outcome, Err(ClientError::Failed(_))),
            "{outcome:?}"
        );
        assert_eq!(exit(&outcome), Exit::Failure);
    }
    let outcome = fake_after_request(
        RequestBody::Ui(UiRequest::Command(UiCommand::AppQuit)),
        accepted,
    )
    .await?;
    assert!(outcome.is_ok(), "{outcome:?}");
    Ok(())
}

#[tokio::test]
async fn request_at_admission_barrier_then_leaving_is_owner_leaving() -> TestResult {
    let dir = Dir::new()?;
    let mut owner = Owner::start(&dir).await?;
    let before = owner.memory.revision();
    let (client, _clock) = client(dir.address());

    let mut greeting = owner.server()?.hooks().barrier_next_admission();
    let call = tokio::spawn(async move { client.call(create("Альфа")).await });
    greeting.entered().await;
    let mut admission = owner.server()?.hooks().barrier_next_admission();
    greeting.release();
    admission.entered().await;
    let done = owner.shutdown()?;
    admission.release();

    let outcome = call.await?;
    match &outcome {
        Ok(Response::Error {
            id: Some(RequestId(2)),
            error,
        }) => assert_eq!(error.code(), ControlErrorCode::OwnerLeaving),
        other => return Err(format!("ожидался owner_leaving, получено {other:?}").into()),
    }
    assert_eq!(exit(&outcome), Exit::Retry);
    assert_eq!(exit(&outcome).code(), 75);
    done.await?;
    assert_eq!(owner.memory.revision(), before, "ядро запрос не получило");
    owner.stop().await
}

#[tokio::test]
async fn hello_from_leaving_owner_is_owner_leaving_without_request() -> TestResult {
    let dir = Dir::new()?;
    let mut owner = Owner::start(&dir).await?;
    let before = owner.memory.revision();
    let (client, _clock) = client(dir.address());

    let mut greeting = owner.server()?.hooks().barrier_next_admission();
    let call = tokio::spawn(async move { client.call(create("Альфа")).await });
    greeting.entered().await;
    let done = owner.shutdown()?;
    greeting.release();

    let outcome = call.await?;
    assert_eq!(refusal(&outcome), Some(ControlErrorCode::OwnerLeaving));
    assert_eq!(exit(&outcome), Exit::Retry);
    done.await?;
    assert_eq!(owner.memory.revision(), before);
    owner.stop().await
}

#[tokio::test]
async fn version_returns_hello_frame() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start(&dir).await?;
    let (client, _clock) = client(dir.address());

    match client.version().await? {
        Response::Ok {
            id: RequestId(1),
            value: OkValue::Hello(hello),
        } => {
            assert_eq!(hello.pid, PID);
            assert_eq!(hello.owner, OwnerState::Ready);
        }
        other => return Err(format!("ожидался hello, получено {other:?}").into()),
    }
    owner.stop().await
}

#[tokio::test]
async fn session_keeps_one_deadline_across_requests() -> TestResult {
    let dir = Dir::new()?;
    let owner = Owner::start(&dir).await?;
    let clock = ManualClock::new();
    let deadline = shellio_control::Clock::deadline(&clock, TIMEOUT);
    let socket = dir.address().socket();
    let mut session = Session::connect(clock.clone(), deadline, &socket).await?;

    session.hello("test-client 0.1.0").await?;
    let created = session.request(create("Альфа")).await?;
    assert!(matches!(created, Response::Ok { .. }), "{created:?}");
    let revision = owner.memory.revision();
    clock.expire();
    let outcome = session.request(create("Бета")).await;
    assert_eq!(
        refusal(&outcome),
        Some(ControlErrorCode::Unavailable),
        "команда, не отправленная до срока, повторяема"
    );
    assert_eq!(exit(&outcome), Exit::Retry);
    let outcome = session.request(tree_all()).await;
    assert_eq!(refusal(&outcome), Some(ControlErrorCode::Unavailable));
    assert_eq!(
        owner.memory.revision(),
        revision,
        "команда не дошла до ядра"
    );
    owner.stop().await
}

#[tokio::test]
async fn subscription_deadline_ends_at_first_frame() -> TestResult {
    let dir = Dir::new()?;
    let mut owner = Owner::start(&dir).await?;
    let (client, clock) = client(dir.address());

    let mut subscription = client.subscribe().await?;
    let snapshot = match subscription.next().await? {
        Response::Snapshot {
            id: RequestId(2),
            snapshot,
        } => snapshot,
        other => return Err(format!("ожидался снимок, получено {other:?}").into()),
    };
    clock.expire();

    let (other, _other_clock) = self::client(dir.address());
    let created = reply(other.call(create("Альфа")).await?)?;
    match subscription.next().await? {
        Response::Event {
            id: RequestId(2),
            event,
        } => {
            assert_eq!(event.revision, created.revision);
            assert_eq!(event.revision, Revision(snapshot.revision.0 + 1));
        }
        other => return Err(format!("ожидалось событие, получено {other:?}").into()),
    }

    let done = owner.shutdown()?;
    let last = subscription.next().await?;
    match &last {
        Response::Error {
            id: Some(RequestId(2)),
            error,
        } => assert_eq!(error.code(), ControlErrorCode::OwnerLeaving),
        other => return Err(format!("ожидался owner_leaving, получено {other:?}").into()),
    }
    assert_eq!(Exit::of_response(&last), Exit::Retry);
    done.await?;
    owner.stop().await
}

#[tokio::test]
async fn subscription_hangup_without_owner_leaving_is_unavailable() -> TestResult {
    let fake = Fake::bind()?;
    let (client, _clock) = fake.client();
    let subscribe = tokio::spawn(async move {
        let mut subscription = client.subscribe().await?;
        let first = subscription.next().await?;
        let end = subscription.next().await;
        Ok::<_, ClientError>((first, end))
    });

    let mut peer = fake.accept().await?;
    peer.greet().await?;
    let line = peer.read_line().await?.ok_or("subscribe не пришёл")?;
    assert!(line.contains("\"subscribe\""), "{line}");
    peer.write_frame(&Response::Snapshot {
        id: RequestId(2),
        snapshot: Snapshot {
            revision: Revision(1),
            state: Arc::new(State::default()),
        },
    })
    .await?;
    drop(peer);

    let (first, end) = subscribe.await??;
    assert!(matches!(first, Response::Snapshot { .. }), "{first:?}");
    assert_eq!(refusal(&end), Some(ControlErrorCode::Unavailable));
    assert_eq!(exit(&end), Exit::Retry);
    Ok(())
}

#[test]
fn socket_and_lock_names_are_profile_files() {
    let address = Address::Profile(Profile::isolated("/state"));
    assert_eq!(address.socket(), PathBuf::from("/state").join(SOCKET_FILE));
}
