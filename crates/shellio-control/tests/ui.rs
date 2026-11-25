//! Запрос `ui` через сервер управления: порт владельца подделан, сроки идут по ручным часам
//! сервера, клиент — `Session` на часах tokio с запасным сроком.
//!
//! Подделка порта передаёт каждый вызов тесту вместе с отметкой начала [`UiGate`], и тест сам
//! решает, ответить, начать работу или оставить её в очереди до срока.

use std::error::Error;
use std::future::Future;
use std::os::unix::net::UnixListener as StdListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use shellio_control::test_support::ManualClock;
use shellio_control::{
    Accepted, Activation, ControlErrorCode, Launch, LaunchHandler, LaunchReceipt, OkValue,
    Response, Server, ServerConfig, Session, TokioClock, UiGate, UiHandler, UiRefusal, UiReply,
    UiRequest, WithoutUi,
};
use shellio_core::test_support::{
    FakeRootResolver, ManualClock as CoreClock, MemoryPersist, SequentialIds, registry,
};
use shellio_core::{AppearanceChoice, Core, UiCommand, UnixMillis};
use tempfile::TempDir;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const PID: u32 = 4242;
/// Запасной срок клиента: ограничивает зависание теста, а не синхронизирует.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

struct Launcher;

impl LaunchHandler for Launcher {
    fn launch(&self, _launch: Launch) -> impl Future<Output = LaunchReceipt> + Send {
        std::future::ready(LaunchReceipt {
            activation: Activation::NoUi,
            refused: Vec::new(),
        })
    }
}

/// Вызов порта, переданный тесту.
struct Call {
    request: UiRequest,
    gate: UiGate,
    answer: oneshot::Sender<Result<UiReply, UiRefusal>>,
}

/// Порт UI, который отдаёт каждый вызов тесту и ждёт ответа теста.
#[derive(Clone)]
struct FakeUi {
    calls: mpsc::UnboundedSender<Call>,
}

impl UiHandler for FakeUi {
    fn ui(
        &self,
        request: UiRequest,
        gate: UiGate,
    ) -> impl Future<Output = Result<UiReply, UiRefusal>> + Send {
        let (answer, reply) = oneshot::channel();
        let _ = self.calls.send(Call {
            request,
            gate,
            answer,
        });
        async move { reply.await.unwrap_or(Err(UiRefusal::NoUi)) }
    }
}

struct Fixture<U: UiHandler> {
    server: Option<Server<ManualClock, Launcher, U>>,
    clock: ManualClock,
    socket: PathBuf,
    _dir: TempDir,
}

impl<U: UiHandler> Fixture<U> {
    async fn start(ui: U) -> TestResult<Self> {
        let dir = tempfile::Builder::new()
            .prefix("shellio-")
            .tempdir_in("/tmp")?;
        let socket = dir.path().join("s.sock");
        let listener = StdListener::bind(&socket)?;
        let core = Core::start(
            &Handle::current(),
            Box::new(MemoryPersist::new()),
            Box::new(CoreClock::new(UnixMillis(1_795_000_000_000))),
            Box::new(SequentialIds::new()),
            Arc::new(FakeRootResolver::new()),
            registry(),
        )?;
        let clock = ManualClock::new();
        let config = ServerConfig {
            server: "test 0.1.0".to_owned(),
            pid: PID,
            grace: shellio_control::SHUTDOWN_GRACE,
        };
        let server = Server::start(
            &Handle::current(),
            listener,
            core,
            Launcher,
            ui,
            clock.clone(),
            config,
        )?;
        Ok(Self {
            server: Some(server),
            clock,
            socket,
            _dir: dir,
        })
    }

    async fn session(&self) -> TestResult<Session<TokioClock>> {
        let deadline = shellio_control::Clock::deadline(&TokioClock, CLIENT_TIMEOUT);
        let mut session = Session::connect(TokioClock, deadline, &self.socket).await?;
        let hello = session.hello("ui-test").await?;
        assert!(
            matches!(
                hello,
                Response::Ok {
                    value: OkValue::Hello(_),
                    ..
                }
            ),
            "{hello:?}"
        );
        Ok(session)
    }

    async fn stop(mut self) -> TestResult {
        if let Some(server) = self.server.take() {
            server.shutdown().await;
        }
        Ok(())
    }
}

fn fake() -> (FakeUi, mpsc::UnboundedReceiver<Call>) {
    let (calls, receiver) = mpsc::unbounded_channel();
    (FakeUi { calls }, receiver)
}

fn accepted(command: &str) -> UiReply {
    UiReply::Accepted(Accepted {
        command: command.to_owned(),
        pid: PID,
        generation: 3,
    })
}

fn control_code(response: &Response) -> Option<ControlErrorCode> {
    match response {
        Response::Error { error, .. } => Some(error.code()),
        _ => None,
    }
}

const CLOSE: UiRequest = UiRequest::Command(UiCommand::WindowClose);

/// Запрос доходит до порта как есть, ответ порта возвращается под ключом `ui`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_reaches_handler_and_reply_returns() -> TestResult {
    let (ui, mut calls) = fake();
    let fixture = Fixture::start(ui).await?;
    let mut session = fixture.session().await?;
    let set = UiRequest::Command(UiCommand::AppearanceSet(AppearanceChoice::Dark));
    let request = tokio::spawn({
        let set = set.clone();
        async move { session.request(shellio_control::RequestBody::Ui(set)).await }
    });
    let call = calls.recv().await.ok_or("порт не вызван")?;
    assert_eq!(call.request, set);
    assert!(call.gate.begin());
    let _ = call.answer.send(Ok(accepted("ui.appearance.set")));
    match request.await?? {
        Response::Ok {
            value: OkValue::Ui(reply),
            ..
        } => assert_eq!(reply, accepted("ui.appearance.set")),
        other => return Err(format!("ожидался ответ ui, получено {other:?}").into()),
    }
    fixture.stop().await
}

/// Владелец без UI отвечает `no_ui` без повтора, отказ галереи — `gallery_only`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_become_stable_codes() -> TestResult {
    let fixture = Fixture::start(WithoutUi).await?;
    let mut session = fixture.session().await?;
    for request in [UiRequest::Window, UiRequest::Stats, CLOSE] {
        let response = session
            .request(shellio_control::RequestBody::Ui(request))
            .await?;
        assert_eq!(control_code(&response), Some(ControlErrorCode::NoUi));
        let Response::Error { error, .. } = response else {
            unreachable!()
        };
        assert!(!error.retryable());
    }
    fixture.stop().await?;

    let (ui, mut calls) = fake();
    let fixture = Fixture::start(ui).await?;
    let mut session = fixture.session().await?;
    let request = tokio::spawn(async move {
        session
            .request(shellio_control::RequestBody::Ui(UiRequest::Inspect))
            .await
    });
    let call = calls.recv().await.ok_or("порт не вызван")?;
    let _ = call.answer.send(Err(UiRefusal::GalleryOnly));
    let response = request.await??;
    assert_eq!(control_code(&response), Some(ControlErrorCode::GalleryOnly));
    fixture.stop().await
}

/// `ui.cell` у владельца без явного каталога состояния — `isolated_profile_only` без повтора.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cell_refusal_is_isolated_profile_only() -> TestResult {
    let (ui, mut calls) = fake();
    let fixture = Fixture::start(ui).await?;
    let mut session = fixture.session().await?;
    let cell = UiRequest::Cell {
        contrast: Some(shellio_control::CellContrast::Increased),
        motion: None,
    };
    let request = tokio::spawn(async move {
        session
            .request(shellio_control::RequestBody::Ui(cell))
            .await
    });
    let call = calls.recv().await.ok_or("порт не вызван")?;
    let _ = call.answer.send(Err(UiRefusal::IsolatedProfileOnly));
    let response = request.await??;
    assert_eq!(
        control_code(&response),
        Some(ControlErrorCode::IsolatedProfileOnly)
    );
    let Response::Error { error, .. } = response else {
        unreachable!()
    };
    assert!(!error.retryable());
    fixture.stop().await
}

/// Работа, не начатая к сроку, отменяется: ответ — повторяемый `ui_timeout`, а исполнитель,
/// дошедший до неё позже, её не выполняет. Так и для чтения, и для изменяющего запроса.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_not_started_by_deadline_is_cancelled() -> TestResult {
    for request in [UiRequest::Window, CLOSE] {
        let (ui, mut calls) = fake();
        let fixture = Fixture::start(ui).await?;
        let mut session = fixture.session().await?;
        let sent = request.clone();
        let pending = tokio::spawn(async move {
            session
                .request(shellio_control::RequestBody::Ui(sent))
                .await
        });
        let call = calls.recv().await.ok_or("порт не вызван")?;
        fixture.clock.expire();
        let response = pending.await??;
        let Response::Error { error, .. } = &response else {
            return Err(format!("ожидался отказ, получено {response:?}").into());
        };
        assert_eq!(error.code(), ControlErrorCode::UiTimeout, "{request:?}");
        assert!(error.retryable());
        assert!(!call.gate.begin(), "отменённая работа не начинается");
        fixture.stop().await?;
    }
    Ok(())
}

/// Начатый изменяющий запрос срок не прерывает: ответ — `outcome_unknown` без повтора. Начатое
/// чтение по сроку даёт `ui_timeout`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn started_work_past_deadline_depends_on_mutation() -> TestResult {
    for (request, code) in [
        (CLOSE, ControlErrorCode::OutcomeUnknown),
        (
            UiRequest::Command(UiCommand::AppQuit),
            ControlErrorCode::OutcomeUnknown,
        ),
        (UiRequest::Stats, ControlErrorCode::UiTimeout),
    ] {
        let (ui, mut calls) = fake();
        let fixture = Fixture::start(ui).await?;
        let mut session = fixture.session().await?;
        let sent = request.clone();
        let pending = tokio::spawn(async move {
            session
                .request(shellio_control::RequestBody::Ui(sent))
                .await
        });
        let call = calls.recv().await.ok_or("порт не вызван")?;
        assert!(call.gate.begin());
        fixture.clock.expire();
        let response = pending.await??;
        assert_eq!(control_code(&response), Some(code), "{request:?}");
        assert!(!call.gate.cancel(), "начатая работа не отменяется");
        fixture.stop().await?;
    }
    Ok(())
}

/// Квитанция, отданная до срока, приходит, даже если срок истёк позже.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answer_before_deadline_wins() -> TestResult {
    let (ui, mut calls) = fake();
    let fixture = Fixture::start(ui).await?;
    let mut session = fixture.session().await?;
    let hold = fixture
        .server
        .as_ref()
        .ok_or("сервер завершён")?
        .hooks()
        .hold_next_reply();
    let mut hold = hold;
    let pending = tokio::spawn(async move {
        session
            .request(shellio_control::RequestBody::Ui(CLOSE))
            .await
    });
    let call = calls.recv().await.ok_or("порт не вызван")?;
    assert!(call.gate.begin());
    let _ = call.answer.send(Ok(accepted("window.close")));
    hold.entered().await;
    fixture.clock.expire();
    hold.release();
    match pending.await?? {
        Response::Ok {
            value: OkValue::Ui(reply),
            ..
        } => assert_eq!(reply, accepted("window.close")),
        other => return Err(format!("ожидалась квитанция, получено {other:?}").into()),
    }
    fixture.stop().await
}
