//! Сервер управляющего сокета над `CoreHandle`: протокол v1 (ADR-0007).
//!
//! Задача на соединение обслуживает запросы по одному, `hello` первым. Подписка делает
//! соединение потоковым до закрытия, медленный подписчик не держит актор и другие
//! соединения: отставший получает `reset` с новым снимком.
//!
//! [`Server::shutdown`] выполняет шаги 1–4 порядка завершения владельца (ADR-0006): закрыть
//! допуск и слушатель, отменить бездействующие соединения и подписки, довести допущенные
//! запросы до ответа, остановить ядро. Шаг 5 — удаление сокета и снятие блокировки — делает
//! держатель [`crate::Ownership`].

use std::future::{Future, pending};
use std::io;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::panic::resume_unwind;
use std::sync::Arc;
use std::time::Duration;

use shellio_core::{CoreError, CoreHandle};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;
use tokio::task::{JoinError, JoinHandle, JoinSet};

use crate::clock::{Clock, RETRY_INTERVAL};
use crate::codec::{LineError, LineReader, REQUEST_LINE_LIMIT, encode_line};
use crate::error::{ControlError, ControlErrorCode};
use crate::instance::admit_peer;
use crate::wire::{
    Hello, HelloReply, Launch, LaunchReceipt, OkValue, OwnerState, PROTOCOL, Rejected, Request,
    RequestBody, RequestId, Response,
};

/// Срок шагов отмены и отправки при завершении владельца.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// Порт приёма запуска: аргументы повторного запуска передаются обработчику владельца,
/// ответ — квитанция.
pub trait LaunchHandler: Send + Sync + 'static {
    /// Обработать аргументы запуска и вернуть квитанцию.
    fn launch(&self, launch: Launch) -> impl Future<Output = LaunchReceipt> + Send;
}

/// Сведения сервера для ответа `hello` и срок завершения.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    /// Версия сервера в ответе `hello`.
    pub server: String,
    /// PID владельца в ответе `hello`.
    pub pid: u32,
    /// Срок шагов отмены и отправки при завершении.
    pub grace: Duration,
}

impl ServerConfig {
    /// Сведения с версией `server`, PID текущего процесса и сроком [`SHUTDOWN_GRACE`].
    pub fn new(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            pid: std::process::id(),
            grace: SHUTDOWN_GRACE,
        }
    }
}

/// Сервер управляющего сокета.
///
/// Брошенный без [`Self::shutdown`] сервер закрывает допуск и слушатель и отменяет
/// соединения, но ядро не останавливает.
pub struct Server<C: Clock, L: LaunchHandler> {
    shared: Arc<Shared<C, L>>,
    accept: Option<JoinHandle<()>>,
}

impl<C: Clock, L: LaunchHandler> std::fmt::Debug for Server<C, L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("config", &self.shared.config)
            .field("leaving", &self.shared.is_leaving())
            .finish_non_exhaustive()
    }
}

/// Состояние допуска: `None` — владелец готов, `Some` — уходит, со сроком шагов завершения.
type Leaving<D> = watch::Sender<Option<D>>;

struct Shared<C: Clock, L> {
    core: CoreHandle,
    launcher: L,
    clock: C,
    config: ServerConfig,
    leaving: Leaving<C::Deadline>,
    hooks: hooks::ServerHooks,
}

impl<C: Clock, L> Shared<C, L> {
    fn is_leaving(&self) -> bool {
        self.leaving.borrow().is_some()
    }

    fn owner_state(&self) -> OwnerState {
        if self.is_leaving() {
            OwnerState::Leaving
        } else {
            OwnerState::Ready
        }
    }

    /// Завершается, когда истекает срок шагов завершения. До перехода в `leaving` не
    /// завершается.
    async fn cut(&self) {
        let mut leaving = self.leaving.subscribe();
        let deadline = wait_leaving(&mut leaving).await;
        let _ = self.clock.within(deadline, pending::<()>()).await;
    }
}

/// Дождаться перехода в `leaving` и вернуть срок шагов завершения.
async fn wait_leaving<D: Copy>(leaving: &mut watch::Receiver<Option<D>>) -> D {
    let deadline = leaving
        .wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|state| *state);
    match deadline {
        Some(deadline) => deadline,
        // Отправитель живёт в `Shared`, пока живы задачи сервера.
        None => pending().await,
    }
}

impl<C: Clock, L: LaunchHandler> Server<C, L> {
    /// Запустить сервер на `runtime` над слушателем `listener`, например из
    /// [`crate::Ownership::publish`]. Слушатель переводится в неблокирующий режим.
    pub fn start(
        runtime: &Handle,
        listener: StdUnixListener,
        core: CoreHandle,
        launcher: L,
        clock: C,
        config: ServerConfig,
    ) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        let listener = {
            let _entered = runtime.enter();
            UnixListener::from_std(listener)?
        };
        let shared = Arc::new(Shared {
            core,
            launcher,
            clock,
            config,
            leaving: watch::Sender::new(None),
            hooks: hooks::ServerHooks::new(),
        });
        let accept = runtime.spawn(accept(Arc::clone(&shared), listener));
        Ok(Self {
            shared,
            accept: Some(accept),
        })
    }

    /// Завершить сервер, шаги 1–4 порядка завершения владельца.
    ///
    /// Переход в `leaving` и срок шагов отмены и отправки ([`ServerConfig::grace`]) задаются
    /// при вызове, даже пока future не опрошен: допуск новых операций закрыт, `hello`
    /// отвечает `leaving`, остальные запросы — `owner_leaving`. Future закрывает слушатель,
    /// дожидается конца всех соединений (допущенные запросы доводятся до ответа ядра, ответ
    /// и кадр `owner_leaving` подписке пишутся в пределах срока) и затем останавливает ядро.
    pub fn shutdown(mut self) -> impl Future<Output = ()> + Send + 'static {
        self.leave();
        let accept = self.accept.take();
        let shared = Arc::clone(&self.shared);
        async move {
            if let Some(accept) = accept {
                rethrow(accept.await);
            }
            shared.core.shutdown().await;
        }
    }

    fn leave(&self) {
        let deadline = self.shared.clock.deadline(self.shared.config.grace);
        self.shared.leaving.send_if_modified(|state| {
            if state.is_some() {
                return false;
            }
            *state = Some(deadline);
            true
        });
    }

    /// Крючки тестов.
    #[cfg(feature = "test-support")]
    pub fn hooks(&self) -> &hooks::ServerHooks {
        &self.shared.hooks
    }
}

impl<C: Clock, L: LaunchHandler> Drop for Server<C, L> {
    fn drop(&mut self) {
        if self.accept.is_some() {
            self.leave();
        }
    }
}

/// Паника задачи сервера — нарушенный инвариант: она пробрасывается.
fn rethrow(joined: Result<(), JoinError>) {
    if let Err(error) = joined
        && error.is_panic()
    {
        resume_unwind(error.into_panic());
    }
}

/// Цикл приёма до `leaving`, затем ожидание конца всех соединений.
async fn accept<C: Clock, L: LaunchHandler>(shared: Arc<Shared<C, L>>, listener: UnixListener) {
    let mut leaving = shared.leaving.subscribe();
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = wait_leaving(&mut leaving) => break,
            Some(joined) = connections.join_next() => rethrow(joined),
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    // Чужой собеседник закрывается без ответа.
                    if let Ok(stream) = admit_peer(stream) {
                        connections.spawn(Connection::new(Arc::clone(&shared), stream).serve());
                    }
                }
                // Нехватка дескрипторов не проходит сама: пауза перед следующей попыткой.
                Err(_) => {
                    let pause = shared.clock.deadline(RETRY_INTERVAL);
                    tokio::select! {
                        _ = wait_leaving(&mut leaving) => {}
                        _ = shared.clock.wait_retry(pause) => {}
                    }
                }
            },
        }
    }
    drop(listener);
    while let Some(joined) = connections.join_next().await {
        rethrow(joined);
    }
}

/// Соединение закончено: клиент ушёл, запись не удалась или сервер его закрыл.
struct Hangup;

/// Прочитанная строка запроса.
enum Incoming {
    Frame(Result<Request, Rejected>),
    TooLong(usize),
    End,
}

impl Incoming {
    fn read(line: Result<Option<&[u8]>, LineError>) -> Self {
        match line {
            Ok(Some(line)) => Self::Frame(Request::parse(line)),
            Err(LineError::TooLong { limit }) => Self::TooLong(limit),
            Ok(None) | Err(LineError::Truncated { .. } | LineError::Io(_)) => Self::End,
        }
    }
}

/// Отказ разбора, который говорит о виде запроса: до `hello` он отвечается
/// `hello_required`.
const fn names_request_kind(code: ControlErrorCode) -> bool {
    match code {
        ControlErrorCode::ScopeRequired | ControlErrorCode::ScopeUnsupported => true,
        ControlErrorCode::HelloRequired
        | ControlErrorCode::ProtocolMismatch
        | ControlErrorCode::MalformedRequest
        | ControlErrorCode::RequestTooLarge
        | ControlErrorCode::OwnerLeaving
        | ControlErrorCode::NotRunning
        | ControlErrorCode::OwnerStarting
        | ControlErrorCode::Unavailable
        | ControlErrorCode::OutcomeUnknown => false,
    }
}

fn transport_error(id: Option<RequestId>, error: ControlError) -> Response {
    Response::Error { id, error }
}

struct Connection<C: Clock, L> {
    shared: Arc<Shared<C, L>>,
    reader: LineReader<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    leaving: watch::Receiver<Option<C::Deadline>>,
    greeted: bool,
}

impl<C: Clock, L: LaunchHandler> Connection<C, L> {
    fn new(shared: Arc<Shared<C, L>>, stream: UnixStream) -> Self {
        let (reader, writer) = stream.into_split();
        let leaving = shared.leaving.subscribe();
        Self {
            shared,
            reader: LineReader::new(BufReader::new(reader), REQUEST_LINE_LIMIT),
            writer,
            leaving,
            greeted: false,
        }
    }

    async fn serve(mut self) {
        let _ = self.run().await;
    }

    /// Запросы по одному. Отмена — только в ожидании строки: прочитанная строка всегда
    /// получает ответ, пусть и `owner_leaving`.
    async fn run(&mut self) -> Result<(), Hangup> {
        loop {
            let incoming = tokio::select! {
                biased;
                line = self.reader.next_line() => Incoming::read(line),
                _ = wait_leaving(&mut self.leaving) => return Err(Hangup),
            };
            match incoming {
                Incoming::Frame(parsed) => {
                    self.answer(parsed).await?;
                    // Клиент, который шлёт строки без перерыва, не держит завершение.
                    if self.shared.is_leaving() {
                        return Err(Hangup);
                    }
                }
                Incoming::TooLong(limit) => {
                    self.send(&transport_error(
                        None,
                        ControlError::request_too_large(limit),
                    ))
                    .await?;
                    return Err(Hangup);
                }
                Incoming::End => return Err(Hangup),
            }
        }
    }

    async fn answer(&mut self, parsed: Result<Request, Rejected>) -> Result<(), Hangup> {
        let Request { id, body } = match parsed {
            Ok(request) => request,
            Err(Rejected { id, error }) => {
                let error = if !self.greeted && names_request_kind(error.code()) {
                    ControlError::hello_required()
                } else {
                    error
                };
                return self.send(&transport_error(id, error)).await;
            }
        };
        if !self.greeted && !matches!(body, RequestBody::Hello(_)) {
            return self
                .send(&transport_error(Some(id), ControlError::hello_required()))
                .await;
        }
        self.shared.hooks.before_admission().await;
        match body {
            RequestBody::Hello(hello) => self.hello(id, hello).await,
            // Допуск закрыт. Проверка не атомарна с переходом, но и не должна быть: допущенный
            // запрос доводится до ответа, а ядро останавливается только после конца всех
            // соединений.
            _ if self.shared.is_leaving() => {
                self.send(&transport_error(Some(id), ControlError::owner_leaving()))
                    .await
            }
            RequestBody::Command { command, scope } => {
                let result = self.shared.core.request_in(scope, command).await;
                self.reply(id, result.map(OkValue::Reply)).await
            }
            RequestBody::Query(query) => {
                let result = self.shared.core.query(query).await;
                self.reply(id, result.map(OkValue::Reply)).await
            }
            RequestBody::EventsSince { cursor } => {
                let result = self.shared.core.events_since(cursor).await;
                self.reply(id, result.map(OkValue::EventsSince)).await
            }
            RequestBody::Launch(launch) => {
                let receipt = self.shared.launcher.launch(launch).await;
                self.reply(id, Ok(OkValue::Launch(receipt))).await
            }
            RequestBody::Subscribe => self.stream(id).await,
        }
    }

    /// Неверная версия оставляет соединение без приветствия.
    async fn hello(&mut self, id: RequestId, hello: Hello) -> Result<(), Hangup> {
        if hello.protocol != PROTOCOL {
            return self
                .send(&transport_error(
                    Some(id),
                    ControlError::protocol_mismatch(hello.protocol),
                ))
                .await;
        }
        self.greeted = true;
        let reply = HelloReply {
            protocol: PROTOCOL,
            server: self.shared.config.server.clone(),
            pid: self.shared.config.pid,
            owner: self.shared.owner_state(),
        };
        self.send(&Response::Ok {
            id,
            value: OkValue::Hello(reply),
        })
        .await
    }

    async fn reply(
        &mut self,
        id: RequestId,
        result: Result<OkValue, CoreError>,
    ) -> Result<(), Hangup> {
        self.shared.hooks.before_reply().await;
        let response = match result {
            Ok(value) => Response::Ok { id, value },
            Err(error) => Response::CoreError { id, error },
        };
        self.send(&response).await
    }

    /// Поток подписки до закрытия: снимок, события, `reset` при отставании. Ввод клиента
    /// в потоке не отвечается, конец ввода закрывает соединение.
    async fn stream(&mut self, id: RequestId) -> Result<(), Hangup> {
        let (snapshot, mut events) = match self.shared.core.subscribe().await {
            Ok(subscribed) => subscribed,
            Err(error) => return self.send(&Response::CoreError { id, error }).await,
        };
        self.send(&Response::Snapshot { id, snapshot }).await?;
        let mut pause = self.shared.hooks.pause();
        loop {
            let received = tokio::select! {
                biased;
                _ = wait_leaving(&mut self.leaving) => {
                    self.send(&transport_error(Some(id), ControlError::owner_leaving()))
                        .await?;
                    return Err(Hangup);
                }
                input = self.reader.next_line() => match input {
                    Ok(Some(_)) => continue,
                    Ok(None) | Err(_) => return Err(Hangup),
                },
                received = pause.recv(&mut events) => received,
            };
            match received {
                Ok(event) => self.send(&Response::Event { id, event }).await?,
                Err(RecvError::Lagged(_)) => {
                    let (snapshot, fresh) = match self.shared.core.subscribe().await {
                        Ok(subscribed) => subscribed,
                        Err(error) => {
                            self.send(&Response::CoreError { id, error }).await?;
                            return Err(Hangup);
                        }
                    };
                    events = fresh;
                    self.send(&Response::Reset { id, snapshot }).await?;
                }
                Err(RecvError::Closed) => return Err(Hangup),
            }
        }
    }

    /// Записать кадр. После перехода в `leaving` запись ограничена сроком завершения:
    /// по сроку соединение закрывается, оборванный кадр не дописывается.
    async fn send(&mut self, response: &Response) -> Result<(), Hangup> {
        let line = encode_line(response).map_err(|_| Hangup)?;
        tokio::select! {
            biased;
            written = self.writer.write_all(&line) => written.map_err(|_| Hangup),
            () = self.shared.cut() => Err(Hangup),
        }
    }
}

/// Крючки тестов сервера. Продуктовая сборка получает пустые заглушки без состояния.
#[cfg(feature = "test-support")]
pub(crate) mod hooks {
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use shellio_core::Event;
    use tokio::sync::broadcast::{self, error::RecvError};
    use tokio::sync::{oneshot, watch};

    /// Крючки сервера: барьер перед допуском, задержка ответа после квитанции ядра и пауза
    /// чтения событий подписками.
    #[derive(Debug)]
    pub struct ServerHooks {
        admission: Mutex<Option<Gate>>,
        reply: Mutex<Option<Gate>>,
        paused: watch::Sender<bool>,
    }

    impl Default for ServerHooks {
        fn default() -> Self {
            Self {
                admission: Mutex::default(),
                reply: Mutex::default(),
                paused: watch::Sender::new(false),
            }
        }
    }

    #[derive(Debug)]
    struct Gate {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    }

    impl Gate {
        fn arm(slot: &Mutex<Option<Self>>) -> Hold {
            let (entered_tx, entered_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            *lock(slot) = Some(Self {
                entered: entered_tx,
                release: release_rx,
            });
            Hold {
                entered: Some(entered_rx),
                release: release_tx,
            }
        }

        async fn pass(slot: &Mutex<Option<Self>>) {
            let gate = lock(slot).take();
            if let Some(gate) = gate {
                let _ = gate.entered.send(());
                // Отпускает и `release`, и брошенный `Hold`.
                let _ = gate.release.await;
            }
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Остановленный крючком запрос. Брошенный `Hold` отпускает его.
    #[derive(Debug)]
    pub struct Hold {
        entered: Option<oneshot::Receiver<()>>,
        release: oneshot::Sender<()>,
    }

    impl Hold {
        /// Дождаться, пока запрос дойдёт до крючка.
        pub async fn entered(&mut self) {
            if let Some(entered) = self.entered.take() {
                let _ = entered.await;
            }
        }

        /// Отпустить запрос.
        pub fn release(self) {
            let _ = self.release.send(());
        }
    }

    impl ServerHooks {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Следующий разобранный запрос после проверки `hello` остановится перед допуском
        /// к обработчику, пока его не отпустят.
        pub fn barrier_next_admission(&self) -> Hold {
            Gate::arm(&self.admission)
        }

        /// Следующий ответ команды, чтения, `events_since` или `launch` остановится после
        /// квитанции ядра и до записи, пока его не отпустят.
        pub fn hold_next_reply(&self) -> Hold {
            Gate::arm(&self.reply)
        }

        /// Подписки перестают читать события ядра. Событие, которое подписка ещё не взяла,
        /// после вызова она не возьмёт до [`Self::resume_subscriptions`].
        pub fn pause_subscriptions(&self) {
            self.paused.send_replace(true);
        }

        /// Подписки снова читают события ядра.
        pub fn resume_subscriptions(&self) {
            self.paused.send_replace(false);
        }

        pub(crate) async fn before_admission(&self) {
            Gate::pass(&self.admission).await;
        }

        pub(crate) async fn before_reply(&self) {
            Gate::pass(&self.reply).await;
        }

        pub(crate) fn pause(&self) -> Pause {
            Pause(self.paused.subscribe())
        }
    }

    /// Чтение событий подписки с паузой крючка.
    pub(crate) struct Pause(watch::Receiver<bool>);

    impl Pause {
        /// Следующее событие. Пауза проверяется раньше события: поставленная до него пауза
        /// его задерживает.
        pub(crate) async fn recv(
            &mut self,
            events: &mut broadcast::Receiver<Event>,
        ) -> Result<Event, RecvError> {
            loop {
                if self.0.wait_for(|paused| !*paused).await.is_err() {
                    return events.recv().await;
                }
                let closed = tokio::select! {
                    biased;
                    paused = self.0.wait_for(|paused| *paused) => paused.is_err(),
                    received = events.recv() => return received,
                };
                if closed {
                    return events.recv().await;
                }
            }
        }
    }
}

#[cfg(not(feature = "test-support"))]
mod hooks {
    use shellio_core::Event;
    use tokio::sync::broadcast::{self, error::RecvError};

    #[derive(Debug)]
    pub(crate) struct ServerHooks;

    impl ServerHooks {
        pub(crate) const fn new() -> Self {
            Self
        }

        pub(crate) async fn before_admission(&self) {}

        pub(crate) async fn before_reply(&self) {}

        pub(crate) fn pause(&self) -> Pause {
            Pause
        }
    }

    pub(crate) struct Pause;

    impl Pause {
        pub(crate) async fn recv(
            &mut self,
            events: &mut broadcast::Receiver<Event>,
        ) -> Result<Event, RecvError> {
            events.recv().await
        }
    }
}
