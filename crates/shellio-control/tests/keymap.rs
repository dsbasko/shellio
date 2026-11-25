//! Запрос `keymap` сервера управления (ADR-0007, «Уточнено в P1.6 (планирование)»): чтение,
//! перечитывание и подписка через настоящий Unix-сокет над поддельной службой keymap.
//!
//! Ядро — на хранилище в памяти, окна нет. Синхронизация — кадры ответа и канал службы.

use std::error::Error;
use std::future::{Future, ready};
use std::os::unix::net::UnixListener as StdListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use shellio_control::test_support::ManualClock;
use shellio_control::{
    Activation, Address, Client, ClientError, ControlErrorCode, KeymapHandler, KeymapRequest,
    KeymapRow, KeymapSource, KeymapView, Launch, LaunchHandler, LaunchReceipt, LineReader, OkValue,
    PROTOCOL, RESPONSE_LINE_LIMIT, RequestBody, RequestId, Response, SHUTDOWN_GRACE, Server,
    ServerConfig, TokioClock, WithoutUi,
};
use shellio_core::test_support::{
    FakeRootResolver, ManualClock as CoreClock, MemoryPersist, SequentialIds, registry,
};
use shellio_core::{Core, UnixMillis};
use tempfile::TempDir;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::runtime::Handle;
use tokio::sync::watch;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

struct Launcher;

impl LaunchHandler for Launcher {
    fn launch(&self, _launch: Launch) -> impl Future<Output = LaunchReceipt> + Send {
        ready(LaunchReceipt {
            activation: Activation::NoUi,
            refused: Vec::new(),
        })
    }
}

fn table(version: u64) -> KeymapView {
    KeymapView {
        version,
        leader: "Space".to_owned(),
        bindings: vec![KeymapRow {
            context: "navigation".to_owned(),
            sequence: format!("Ctrl-w {version}"),
            action: "layout.zoom".to_owned(),
            args: None,
            source: KeymapSource::File,
        }],
        diagnostics: Vec::new(),
    }
}

/// Поддельная служба: таблицу ставит тест, `keymap.reload` публикует следующую версию.
#[derive(Clone)]
struct FakeKeymap {
    tables: watch::Sender<Arc<KeymapView>>,
    reloads: Arc<AtomicU64>,
}

impl FakeKeymap {
    fn new() -> Self {
        Self {
            tables: watch::Sender::new(Arc::new(table(1))),
            reloads: Arc::default(),
        }
    }

    fn publish(&self, version: u64) {
        self.tables.send_replace(Arc::new(table(version)));
    }
}

impl KeymapHandler for FakeKeymap {
    type Snapshot = KeymapView;

    fn watch(&self) -> watch::Receiver<Arc<KeymapView>> {
        self.tables.subscribe()
    }

    fn reload(&self) -> impl Future<Output = Arc<KeymapView>> + Send {
        self.reloads.fetch_add(1, Ordering::SeqCst);
        let next = self.tables.borrow().version + 1;
        self.publish(next);
        ready(Arc::clone(&self.tables.borrow()))
    }
}

struct Fixture {
    server: Option<Server<ManualClock, Launcher, WithoutUi, FakeKeymap>>,
    memory: MemoryPersist,
    keymap: FakeKeymap,
    socket: PathBuf,
    _dir: TempDir,
}

impl Fixture {
    async fn start() -> TestResult<Self> {
        let dir = tempfile::Builder::new()
            .prefix("shellio-")
            .tempdir_in("/tmp")?;
        let socket = dir.path().join("s.sock");
        let listener = StdListener::bind(&socket)?;
        let memory = MemoryPersist::new();
        let core = Core::start(
            &Handle::current(),
            Box::new(memory.clone()),
            Box::new(CoreClock::new(UnixMillis(1_795_000_000_000))),
            Box::new(SequentialIds::new()),
            Arc::new(FakeRootResolver::new()),
            registry(),
        )?;
        let keymap = FakeKeymap::new();
        let server = Server::start_with_keymap(
            &Handle::current(),
            listener,
            core,
            Launcher,
            WithoutUi,
            keymap.clone(),
            ManualClock::new(),
            ServerConfig {
                server: "test 0.1.0".to_owned(),
                pid: 4242,
                grace: SHUTDOWN_GRACE,
            },
        )?;
        Ok(Self {
            server: Some(server),
            memory,
            keymap,
            socket,
            _dir: dir,
        })
    }

    async fn connect(&self) -> TestResult<Peer> {
        let (reader, writer) = UnixStream::connect(&self.socket).await?.into_split();
        Ok(Peer {
            reader: LineReader::new(BufReader::new(reader), RESPONSE_LINE_LIMIT),
            writer,
        })
    }

    async fn greeted(&self) -> TestResult<Peer> {
        let mut peer = self.connect().await?;
        let hello = json!({"id": 1, "hello": {"protocol": PROTOCOL, "client": "test"}});
        let response = peer.call(&hello).await?;
        assert!(matches!(response, Response::Ok { .. }), "{response:?}");
        Ok(peer)
    }

    fn client(&self) -> Client<TokioClock> {
        Client::new(
            Address::Socket(self.socket.clone()),
            TokioClock,
            "keymap-test",
            std::time::Duration::from_secs(10),
        )
    }
}

struct Peer {
    reader: LineReader<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
}

impl Peer {
    async fn send(&mut self, frame: &Value) -> TestResult {
        self.writer.write_all(frame.to_string().as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        Ok(())
    }

    async fn recv(&mut self) -> TestResult<Response> {
        let line = self.reader.next_line().await?.ok_or("соединение закрыто")?;
        Ok(Response::parse(line)?)
    }

    async fn call(&mut self, frame: &Value) -> TestResult<Response> {
        self.send(frame).await?;
        self.recv().await
    }

    /// Кадр подписки `id`: полная таблица.
    async fn table(&mut self, id: u64) -> TestResult<KeymapView> {
        match self.recv().await? {
            Response::Keymap { id: got, keymap } if got == RequestId(id) => Ok(keymap),
            other => Err(format!("ожидался кадр keymap {id}, получено {other:?}").into()),
        }
    }
}

fn keymap(id: u64, name: &str) -> Value {
    json!({"id": id, "keymap": {name: {}}})
}

fn ok_table(id: u64, response: Response) -> TestResult<KeymapView> {
    match response {
        Response::Ok {
            id: got,
            value: OkValue::Keymap(view),
        } if got == RequestId(id) => Ok(view),
        other => Err(format!("ожидалась таблица на {id}, получено {other:?}").into()),
    }
}

fn error_code(response: Response) -> TestResult<ControlErrorCode> {
    match response {
        Response::Error { error, .. } => Ok(error.code()),
        other => Err(format!("ожидался отказ транспорта, получено {other:?}").into()),
    }
}

/// `query.keymap` отвечает из снимка службы: без окна, без ядра и без доменной ревизии в ответе.
#[tokio::test]
async fn query_answers_from_the_service_snapshot() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.greeted().await?;
    let revision = fixture.memory.revision();

    let view = ok_table(2, peer.call(&keymap(2, "query.keymap")).await?)?;
    assert_eq!(view, table(1));
    fixture.keymap.publish(7);
    let view = ok_table(3, peer.call(&keymap(3, "query.keymap")).await?)?;
    assert_eq!(view, table(7));

    assert_eq!(
        fixture.memory.revision(),
        revision,
        "ревизия ядра не тронута"
    );
    assert_eq!(fixture.keymap.reloads.load(Ordering::SeqCst), 0);
    Ok(())
}

/// `keymap.reload` отвечает таблицей после применения, доменная ревизия при этом прежняя.
#[tokio::test]
async fn reload_answers_with_the_applied_table() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.greeted().await?;
    let revision = fixture.memory.revision();

    let applied = ok_table(2, peer.call(&keymap(2, "keymap.reload")).await?)?;
    assert_eq!(applied, table(2));
    assert_eq!(fixture.keymap.reloads.load(Ordering::SeqCst), 1);
    let read = ok_table(3, peer.call(&keymap(3, "query.keymap")).await?)?;
    assert_eq!(read, applied);
    assert_eq!(fixture.memory.revision(), revision);
    Ok(())
}

/// Запрос `keymap` без `hello`, с областью и с неизвестным именем отвергается до службы.
#[tokio::test]
async fn keymap_request_needs_hello_and_takes_no_scope() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut bare = fixture.connect().await?;
    let code = error_code(bare.call(&keymap(1, "query.keymap")).await?)?;
    assert_eq!(code, ControlErrorCode::HelloRequired);

    let mut peer = fixture.greeted().await?;
    let scoped = json!({"id": 2, "keymap": {"query.keymap": {}}, "scope": "all"});
    assert_eq!(
        error_code(peer.call(&scoped).await?)?,
        ControlErrorCode::MalformedRequest
    );
    for body in [
        json!({"keymap.drop": {}}),
        json!({"query.keymap": {"x": 1}}),
    ] {
        let frame = json!({"id": 3, "keymap": body});
        assert_eq!(
            error_code(peer.call(&frame).await?)?,
            ControlErrorCode::MalformedRequest,
            "{frame}"
        );
    }
    assert_eq!(fixture.keymap.reloads.load(Ordering::SeqCst), 0);
    Ok(())
}

/// Подписка начинается с полной таблицы и отдаёт таблицу на каждое изменение. Отставший
/// подписчик получает последнюю таблицу, а не каждое поколение.
#[tokio::test]
async fn subscription_starts_full_and_follows_to_the_last_table() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut peer = fixture.greeted().await?;

    peer.send(&keymap(2, "keymap.subscribe")).await?;
    assert_eq!(peer.table(2).await?, table(1));
    fixture.keymap.publish(2);
    assert_eq!(peer.table(2).await?, table(2));

    // Три поколения подряд без чтения: версии только растут, последняя приходит обязательно.
    for version in 3..=5 {
        fixture.keymap.publish(version);
    }
    let mut seen = 2;
    while seen < 5 {
        let next = peer.table(2).await?.version;
        assert!(next > seen, "{next} после {seen}");
        seen = next;
    }

    // Ввод клиента в потоке не отвечается: следующий кадр — снова таблица.
    peer.send(&keymap(3, "query.keymap")).await?;
    fixture.keymap.publish(6);
    assert_eq!(peer.table(2).await?, table(6));
    Ok(())
}

/// Новое подключение начинается с полного кадра действующей таблицы, а не с дельты.
#[tokio::test]
async fn reconnect_starts_with_a_full_frame() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut first = fixture.greeted().await?;
    first.send(&keymap(2, "keymap.subscribe")).await?;
    assert_eq!(first.table(2).await?, table(1));
    fixture.keymap.publish(4);
    assert_eq!(first.table(2).await?, table(4));
    drop(first);

    let mut second = fixture.greeted().await?;
    second.send(&keymap(9, "keymap.subscribe")).await?;
    assert_eq!(second.table(9).await?, table(4));
    Ok(())
}

/// Одновременные `keymap.reload` и доменная команда: подписчик догоняет версию `query.keymap`, а
/// ревизия ядра растёт ровно на одну доменную запись.
#[tokio::test]
async fn reload_next_to_a_domain_command_leaves_the_revision_to_the_core() -> TestResult {
    let fixture = Fixture::start().await?;
    let mut follower = fixture.greeted().await?;
    follower.send(&keymap(2, "keymap.subscribe")).await?;
    assert_eq!(follower.table(2).await?.version, 1);
    let before = fixture.memory.revision();

    let mut reloader = fixture.greeted().await?;
    let mut writer = fixture.greeted().await?;
    let create = json!({"id": 5, "command": {"cmd": "workspace.create", "title": "Работа"},
        "scope": "all"});
    reloader.send(&keymap(4, "keymap.reload")).await?;
    writer.send(&create).await?;
    let (applied, created) = tokio::join!(reloader.recv(), writer.recv());
    let applied = ok_table(4, applied?)?;
    assert!(matches!(created?, Response::Ok { .. }));

    let read = ok_table(6, reloader.call(&keymap(6, "query.keymap")).await?)?;
    assert_eq!(read.version, applied.version);
    let mut seen = 1;
    while seen < read.version {
        seen = follower.table(2).await?.version;
    }
    assert_eq!(seen, read.version, "подписчик догнал версию чтения");
    assert_eq!(fixture.memory.revision(), before.next());
    Ok(())
}

/// Уходящий владелец кончает поток подписки кадром `owner_leaving`, новый запрос `keymap` получает
/// тот же отказ.
#[tokio::test]
async fn leaving_owner_ends_the_keymap_stream() -> TestResult {
    let mut fixture = Fixture::start().await?;
    let mut follower = fixture.greeted().await?;
    follower.send(&keymap(2, "keymap.subscribe")).await?;
    assert_eq!(follower.table(2).await?.version, 1);

    let server = fixture.server.take().ok_or("сервер уже завершён")?;
    let shutdown = tokio::spawn(server.shutdown());
    assert_eq!(
        error_code(follower.recv().await?)?,
        ControlErrorCode::OwnerLeaving
    );
    shutdown.await?;
    Ok(())
}

/// Клиент: чтение и перечитывание дают таблицу, подписка keymap — поток таблиц. Чужой кадр поток
/// отклоняет, подписка событий таблиц не принимает.
#[tokio::test]
async fn client_reads_reloads_and_follows() -> TestResult {
    let fixture = Fixture::start().await?;
    let client = fixture.client();

    let read = client
        .call(RequestBody::Keymap(KeymapRequest::Query))
        .await?;
    assert_eq!(ok_table(2, read)?, table(1));
    let applied = client
        .call(RequestBody::Keymap(KeymapRequest::Reload))
        .await?;
    assert_eq!(ok_table(2, applied)?, table(2));
    let refused = client
        .call(RequestBody::Keymap(KeymapRequest::Subscribe))
        .await;
    assert!(
        matches!(refused, Err(ClientError::Failed(_))),
        "{refused:?}"
    );

    let mut tables = client.subscribe_keymap().await?;
    let first = tables.next().await?;
    assert!(matches!(&first, Response::Keymap { keymap, .. } if *keymap == table(2)));
    fixture.keymap.publish(3);
    let next = tables.next().await?;
    assert!(matches!(&next, Response::Keymap { keymap, .. } if *keymap == table(3)));

    let mut events = client.subscribe().await?;
    assert!(matches!(events.next().await?, Response::Snapshot { .. }));
    Ok(())
}
