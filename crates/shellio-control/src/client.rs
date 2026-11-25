//! Клиент управляющего сокета (ADR-0007): подключение, `hello`, запрос и подписка под одним
//! абсолютным сроком, классификация недоступности владельца пробой `flock` (ADR-0006).
//!
//! Клиент не создаёт файл блокировки, не удаляет сокет, не становится владельцем и не
//! запускает приложение. Мутация и `launch` после полной отправки не повторяются: срок, обрыв
//! или негодный ответ дают `outcome_unknown`.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shellio_core::ErrorCode;
use shellio_store::{Profile, ProfileError};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::clock::{Clock, Expired};
use crate::codec::{LineError, LineReader, REQUEST_LINE_LIMIT, RESPONSE_LINE_LIMIT, encode_line};
use crate::error::{ControlError, ControlErrorCode};
use crate::instance::{Probe, probe_owner, socket_path};
use crate::keymap::KeymapRequest;
use crate::ui::UiRequest;
use crate::wire::{
    Hello, OkValue, OwnerState, PROTOCOL, Request, RequestBody, RequestId, Response,
};

/// Переменная окружения с явным адресом сокета.
pub const SOCKET_VAR: &str = "SHELLIO_SOCKET";

/// Срок вызова CLI по умолчанию.
pub const CLI_TIMEOUT: Duration = Duration::from_secs(5);

/// Код выхода CLI и второго запуска.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Exit {
    /// 0 — успех.
    Success,
    /// 1 — отказ ядра или неповторяемый отказ транспорта.
    Failure,
    /// 2 — ошибка использования.
    Usage,
    /// 69 — `not_running`.
    NotRunning,
    /// 74 — `outcome_unknown`.
    OutcomeUnknown,
    /// 75 — повторяемый отказ.
    Retry,
}

impl Exit {
    /// Числовой код выхода.
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Usage => 2,
            Self::NotRunning => 69,
            Self::OutcomeUnknown => 74,
            Self::Retry => 75,
        }
    }

    /// Код выхода отказа транспорта.
    pub const fn of_control(code: ControlErrorCode) -> Self {
        match code {
            ControlErrorCode::NotRunning => Self::NotRunning,
            ControlErrorCode::OutcomeUnknown => Self::OutcomeUnknown,
            ControlErrorCode::OwnerLeaving
            | ControlErrorCode::OwnerStarting
            | ControlErrorCode::Unavailable
            | ControlErrorCode::UiTimeout => Self::Retry,
            ControlErrorCode::HelloRequired
            | ControlErrorCode::ProtocolMismatch
            | ControlErrorCode::MalformedRequest
            | ControlErrorCode::RequestTooLarge
            | ControlErrorCode::ScopeRequired
            | ControlErrorCode::ScopeUnsupported
            | ControlErrorCode::NoUi
            | ControlErrorCode::GalleryOnly
            | ControlErrorCode::IsolatedProfileOnly => Self::Failure,
        }
    }

    /// Код выхода отказа ядра.
    pub const fn of_core(code: ErrorCode) -> Self {
        match code {
            ErrorCode::Busy | ErrorCode::StorageUnavailable => Self::Retry,
            ErrorCode::InvalidArgument
            | ErrorCode::NotFound
            | ErrorCode::NameTaken
            | ErrorCode::RootTaken
            | ErrorCode::StorageConflict
            | ErrorCode::ShuttingDown
            | ErrorCode::NoUi => Self::Failure,
        }
    }

    /// Код выхода кадра сервера.
    pub fn of_response(response: &Response) -> Self {
        match response {
            Response::Ok { .. }
            | Response::Snapshot { .. }
            | Response::Event { .. }
            | Response::Reset { .. }
            | Response::Keymap { .. } => Self::Success,
            Response::CoreError { error, .. } => Self::of_core(error.code()),
            Response::Error { error, .. } => Self::of_control(error.code()),
        }
    }

    /// Код выхода отказа клиента.
    pub fn of_error(error: &ClientError) -> Self {
        match error {
            ClientError::Control(error) => Self::of_control(error.code()),
            ClientError::Failed(_) => Self::Failure,
        }
    }
}

/// Вызов не дошёл до кадра ответа сервера.
#[derive(Debug)]
pub enum ClientError {
    /// Отказ со стабильным кодом: `not_running`, `owner_starting`, `owner_leaving`,
    /// `unavailable`, `outcome_unknown` или `request_too_large` до отправки.
    Control(ControlError),
    /// Неповторяемый отказ без кода протокола: ответ чтения не разобран, не относится к
    /// запросу или длиннее лимита клиента, проба блокировки не удалась.
    Failed(String),
}

impl From<ControlError> for ClientError {
    fn from(error: ControlError) -> Self {
        Self::Control(error)
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Control(error) => error.fmt(f),
            Self::Failed(reason) => f.write_str(reason),
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Control(error) => Some(error),
            Self::Failed(_) => None,
        }
    }
}

/// Адрес владельца.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Address {
    /// Сокет профиля `<state>/shellio.sock`: недоступность классифицируется пробой `flock`.
    Profile(Profile),
    /// Явный путь сокета: блокировка чужого профиля неизвестна, проба не делается.
    Socket(PathBuf),
}

impl Address {
    /// Адрес по флагу `--socket`, иначе по `SHELLIO_SOCKET`, иначе по профилю. Профиль
    /// вычисляется, только если явного адреса нет.
    pub fn resolve(
        flag: Option<PathBuf>,
        env: Option<OsString>,
        profile: impl FnOnce() -> Result<Profile, ProfileError>,
    ) -> Result<Self, AddressError> {
        if let Some(path) = flag {
            if path.as_os_str().is_empty() {
                return Err(AddressError::EmptySocket { source: "--socket" });
            }
            return Ok(Self::Socket(path));
        }
        if let Some(value) = env {
            if value.is_empty() {
                return Err(AddressError::EmptySocket { source: SOCKET_VAR });
            }
            return Ok(Self::Socket(PathBuf::from(value)));
        }
        profile().map(Self::Profile).map_err(AddressError::Profile)
    }

    /// Путь сокета.
    pub fn socket(&self) -> PathBuf {
        match self {
            Self::Profile(profile) => socket_path(profile),
            Self::Socket(path) => path.clone(),
        }
    }
}

/// Адрес владельца не вычислен.
#[derive(Debug)]
pub enum AddressError {
    /// Явный адрес задан пустым.
    EmptySocket {
        /// Флаг или переменная.
        source: &'static str,
    },
    /// Профиль не вычислен.
    Profile(ProfileError),
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySocket { source } => write!(f, "адрес сокета {source} задан пустым"),
            Self::Profile(error) => write!(f, "профиль не вычислен: {error}"),
        }
    }
}

impl Error for AddressError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Profile(error) => Some(error),
            Self::EmptySocket { .. } => None,
        }
    }
}

/// Подключение не удалось.
#[derive(Debug)]
pub enum ConnectError {
    /// Срок истёк во время подключения.
    Expired,
    /// Подключение отказано.
    Io(io::Error),
}

impl ConnectError {
    /// Сокета нет или его никто не слушает: владелец ещё не опубликовал сокет или уже ушёл.
    pub fn is_absent(&self) -> bool {
        match self {
            Self::Io(error) => matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ),
            Self::Expired => false,
        }
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expired => f.write_str("подключение не уложилось в срок"),
            Self::Io(error) => write!(f, "подключение не удалось: {error}"),
        }
    }
}

impl Error for ConnectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Expired => None,
        }
    }
}

type Reader = LineReader<BufReader<OwnedReadHalf>>;

/// Строка ответа не прочитана.
enum ReadFailure {
    /// Срок истёк.
    Expired,
    /// Соединение оборвалось.
    Closed(String),
    /// Ответ негоден: не разобран, длиннее лимита или не относится к запросу.
    Bad(String),
}

impl ReadFailure {
    fn reason(&self) -> String {
        match self {
            Self::Expired => "ответ не пришёл до срока".to_owned(),
            Self::Closed(reason) => format!("соединение оборвалось до ответа: {reason}"),
            Self::Bad(reason) => format!("ответ негоден: {reason}"),
        }
    }

    /// Отказ после полной отправки запроса.
    fn after_send(self, mutation: bool) -> ClientError {
        let reason = self.reason();
        match (mutation, self) {
            (true, _) => ControlError::outcome_unknown(reason).into(),
            (false, Self::Expired | Self::Closed(_)) => ControlError::unavailable(reason).into(),
            (false, Self::Bad(_)) => ClientError::Failed(format!("владелец: {reason}")),
        }
    }
}

async fn read_frame(reader: &mut Reader) -> Result<Response, ReadFailure> {
    match reader.next_line().await {
        Ok(Some(line)) => Response::parse(line)
            .map_err(|error| ReadFailure::Bad(format!("кадр не разобран: {error}"))),
        Ok(None) => Err(ReadFailure::Closed("соединение закрыто".to_owned())),
        Err(error @ LineError::TooLong { .. }) => Err(ReadFailure::Bad(error.to_string())),
        Err(error @ (LineError::Truncated { .. } | LineError::Io(_))) => {
            Err(ReadFailure::Closed(error.to_string()))
        }
    }
}

/// Какой успешный ответ ждёт запрос.
#[derive(Clone, Copy)]
enum Expect {
    Hello,
    Reply,
    EventsSince,
    Launch,
    /// Ответ `ui` того же запроса.
    Ui(UiRequest),
    Keymap,
}

impl Expect {
    fn matches(self, value: &OkValue) -> bool {
        match value {
            OkValue::Hello(_) => matches!(self, Self::Hello),
            OkValue::Reply(_) => matches!(self, Self::Reply),
            OkValue::EventsSince(_) => matches!(self, Self::EventsSince),
            OkValue::Launch(_) => matches!(self, Self::Launch),
            OkValue::Ui(reply) => match self {
                Self::Ui(request) => request.answered_by(reply),
                Self::Hello | Self::Reply | Self::EventsSince | Self::Launch | Self::Keymap => {
                    false
                }
            },
            OkValue::Keymap(_) => matches!(self, Self::Keymap),
        }
    }
}

/// Кадр, если он отвечает на запрос `id`: успех ожидаемого вида, отказ ядра или отказ
/// транспорта, в том числе без номера.
fn answer(id: RequestId, expect: Expect, frame: Response) -> Result<Response, ReadFailure> {
    let fits = match &frame {
        Response::Ok { id: got, value } => *got == id && expect.matches(value),
        Response::CoreError { id: got, .. } => *got == id,
        Response::Error { id: got, .. } => got.is_none_or(|got| got == id),
        Response::Snapshot { .. }
        | Response::Event { .. }
        | Response::Reset { .. }
        | Response::Keymap { .. } => false,
    };
    if fits {
        Ok(frame)
    } else {
        Err(ReadFailure::Bad(format!("кадр не отвечает на запрос {id}")))
    }
}

/// Какой поток несёт подписка: события ядра или таблицы keymap. Чужие кадры поток отклоняет.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Events,
    Keymap,
}

/// Кадр потока подписки `id`: первый — снимок, дальше события и `reset`; отказ ядра или
/// транспорта кончает поток.
fn stream_frame(
    stream: Stream,
    id: RequestId,
    frame: Response,
    first: bool,
) -> Result<Response, ClientError> {
    let events = stream == Stream::Events;
    let fits = match &frame {
        Response::Snapshot { id: got, .. } => events && first && *got == id,
        Response::Event { id: got, .. } | Response::Reset { id: got, .. } => {
            events && !first && *got == id
        }
        // Каждый кадр подписки keymap — полная таблица, первый и следующие одного вида.
        Response::Keymap { id: got, .. } => !events && *got == id,
        Response::CoreError { id: got, .. } => *got == id,
        Response::Error { id: got, .. } => got.is_none_or(|got| got == id),
        Response::Ok { .. } => false,
    };
    if fits {
        Ok(frame)
    } else {
        Err(ClientError::Failed(format!(
            "владелец: кадр не относится к подписке {id}"
        )))
    }
}

/// Соединение с владельцем под одним абсолютным сроком: подключение, `hello`, запись запроса
/// и чтение ответа прерываются по нему.
pub struct Session<C: Clock> {
    clock: C,
    deadline: C::Deadline,
    reader: Reader,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl<C: Clock> fmt::Debug for Session<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("deadline", &self.deadline)
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl<C: Clock> Session<C> {
    /// Подключиться к сокету `socket` до срока `deadline`. Отказ подключения не
    /// классифицируется: это дело вызывающего.
    pub async fn connect(
        clock: C,
        deadline: C::Deadline,
        socket: &Path,
    ) -> Result<Self, ConnectError> {
        let stream = match clock.within(deadline, UnixStream::connect(socket)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => return Err(ConnectError::Io(error)),
            Err(Expired) => return Err(ConnectError::Expired),
        };
        let (reader, writer) = stream.into_split();
        Ok(Self {
            clock,
            deadline,
            reader: LineReader::new(BufReader::new(reader), RESPONSE_LINE_LIMIT),
            writer,
            next_id: 1,
        })
    }

    fn next_id(&mut self) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        id
    }

    /// `hello` с именем клиента `client`. Кадр ответа — успех `hello` или отказ транспорта.
    pub async fn hello(&mut self, client: &str) -> Result<Response, ClientError> {
        let id = self.next_id();
        let body = RequestBody::Hello(Hello {
            protocol: PROTOCOL,
            client: client.to_owned(),
        });
        self.exchange(id, body, Expect::Hello, false).await
    }

    /// Запрос после `hello`. Кадр ответа — успех, отказ ядра или отказ транспорта.
    ///
    /// До полной отправки исход срока повторяемый. После полной отправки команды, `launch` или
    /// изменяющего запроса `ui` срок, обрыв или негодный ответ — `outcome_unknown`, у остальных
    /// запросов —
    /// повторяемый `unavailable`, а негодный ответ — неповторяемый отказ. Подписка идёт через
    /// [`Self::subscribe`].
    pub async fn request(&mut self, body: RequestBody) -> Result<Response, ClientError> {
        let (expect, mutation) = match &body {
            RequestBody::Hello(_) => (Expect::Hello, false),
            RequestBody::Command { .. } => (Expect::Reply, true),
            RequestBody::Query(_) => (Expect::Reply, false),
            RequestBody::EventsSince { .. } => (Expect::EventsSince, false),
            RequestBody::Launch(_) => (Expect::Launch, true),
            RequestBody::Ui(request) => (Expect::Ui(*request), request.is_mutation()),
            // Перечитывание файла повторяемо: тот же файл даёт ту же таблицу.
            RequestBody::Keymap(KeymapRequest::Query | KeymapRequest::Reload) => {
                (Expect::Keymap, false)
            }
            RequestBody::Subscribe | RequestBody::Keymap(KeymapRequest::Subscribe) => {
                return Err(ClientError::Failed(
                    "подписка идёт через Session::subscribe".to_owned(),
                ));
            }
        };
        let id = self.next_id();
        self.exchange(id, body, expect, mutation).await
    }

    async fn exchange(
        &mut self,
        id: RequestId,
        body: RequestBody,
        expect: Expect,
        mutation: bool,
    ) -> Result<Response, ClientError> {
        self.send(&Request { id, body }).await?;
        self.read()
            .await
            .and_then(|frame| answer(id, expect, frame))
            .map_err(|failure| failure.after_send(mutation))
    }

    /// Подписка: первый кадр ждётся до срока, дальше поток идёт без срока.
    pub async fn subscribe(self) -> Result<Subscription, ClientError> {
        self.open_stream(Stream::Events).await
    }

    /// Подписка keymap: первый кадр с полной таблицей ждётся до срока, дальше поток идёт без
    /// срока.
    pub async fn subscribe_keymap(self) -> Result<Subscription, ClientError> {
        self.open_stream(Stream::Keymap).await
    }

    async fn open_stream(mut self, stream: Stream) -> Result<Subscription, ClientError> {
        let id = self.next_id();
        let body = match stream {
            Stream::Events => RequestBody::Subscribe,
            Stream::Keymap => RequestBody::Keymap(KeymapRequest::Subscribe),
        };
        self.send(&Request { id, body }).await?;
        let first = self
            .read()
            .await
            .map_err(|failure| failure.after_send(false))?;
        let first = stream_frame(stream, id, first, true)?;
        Ok(self.into_subscription(stream, id, first))
    }

    fn into_subscription(self, stream: Stream, id: RequestId, first: Response) -> Subscription {
        Subscription {
            reader: self.reader,
            _writer: self.writer,
            stream,
            id,
            pending: Some(first),
        }
    }

    /// Записать строку запроса. Неполная отправка не исполняется сервером, поэтому её исход
    /// повторяемый. Строку длиннее лимита сервера клиент не отправляет: сервер закрыл бы
    /// соединение посреди записи, и отказ выглядел бы повторяемым.
    async fn send(&mut self, request: &Request) -> Result<(), ClientError> {
        let line = encode_line(request)
            .map_err(|error| ClientError::Failed(format!("запрос не закодирован: {error}")))?;
        if line.len() - 1 > REQUEST_LINE_LIMIT {
            return Err(ControlError::request_too_large(REQUEST_LINE_LIMIT).into());
        }
        match self
            .clock
            .within(self.deadline, self.writer.write_all(&line))
            .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                Err(ControlError::unavailable(format_args!("запрос не отправлен: {error}")).into())
            }
            Err(Expired) => Err(ControlError::unavailable("запрос не отправлен до срока").into()),
        }
    }

    async fn read(&mut self) -> Result<Response, ReadFailure> {
        match self
            .clock
            .within(self.deadline, read_frame(&mut self.reader))
            .await
        {
            Ok(result) => result,
            Err(Expired) => Err(ReadFailure::Expired),
        }
    }
}

/// Поток подписки без срока.
#[derive(Debug)]
pub struct Subscription {
    reader: Reader,
    /// Закрытие записи сервер читает как уход клиента: она живёт, пока жив поток.
    _writer: OwnedWriteHalf,
    stream: Stream,
    id: RequestId,
    pending: Option<Response>,
}

impl Subscription {
    /// Следующий кадр: сначала снимок, затем события и `reset`; у подписки keymap — полные
    /// таблицы. Отказ ядра или транспорта,
    /// в том числе `owner_leaving`, — последний кадр потока. Обрыв без него — повторяемый
    /// `unavailable`.
    pub async fn next(&mut self) -> Result<Response, ClientError> {
        if let Some(frame) = self.pending.take() {
            return Ok(frame);
        }
        match read_frame(&mut self.reader).await {
            Ok(frame) => stream_frame(self.stream, self.id, frame, false),
            Err(ReadFailure::Bad(reason)) => Err(ClientError::Failed(format!(
                "владелец: кадр подписки негоден: {reason}"
            ))),
            Err(ReadFailure::Closed(reason)) => Err(ControlError::unavailable(format_args!(
                "поток подписки оборвался без кадра owner_leaving: {reason}"
            ))
            .into()),
            Err(ReadFailure::Expired) => {
                Err(ControlError::unavailable("поток подписки истёк").into())
            }
        }
    }
}

/// `hello` дал отказ транспорта: он и есть исход вызова.
enum Greeted<C: Clock> {
    Ready(Session<C>),
    Refused {
        session: Session<C>,
        frame: Response,
    },
}

/// Клиент владельца: один абсолютный срок на все операции вызова.
#[derive(Clone, Debug)]
pub struct Client<C> {
    address: Address,
    clock: C,
    timeout: Duration,
    name: String,
}

impl<C: Clock + Clone> Client<C> {
    /// Клиент адреса `address` с именем `name` в `hello` и сроком вызова `timeout`.
    pub fn new(address: Address, clock: C, name: impl Into<String>, timeout: Duration) -> Self {
        Self {
            address,
            clock,
            timeout,
            name: name.into(),
        }
    }

    /// `hello`: кадр ответа как есть, в том числе с `owner: leaving`.
    pub async fn version(&self) -> Result<Response, ClientError> {
        let deadline = self.clock.deadline(self.timeout);
        self.open(deadline).await?.hello(&self.name).await
    }

    /// Один запрос: подключение, `hello` и запрос под одним сроком. Владелец, ответивший
    /// `hello` с `leaving`, даёт `owner_leaving` без отправки запроса.
    pub async fn call(&self, body: RequestBody) -> Result<Response, ClientError> {
        let deadline = self.clock.deadline(self.timeout);
        match self.greeted(deadline).await? {
            Greeted::Ready(mut session) => session.request(body).await,
            Greeted::Refused { frame, .. } => Ok(frame),
        }
    }

    /// Подписка: срок действует до первого кадра.
    pub async fn subscribe(&self) -> Result<Subscription, ClientError> {
        self.open_stream(Stream::Events).await
    }

    /// Подписка keymap: срок действует до первого кадра.
    pub async fn subscribe_keymap(&self) -> Result<Subscription, ClientError> {
        self.open_stream(Stream::Keymap).await
    }

    async fn open_stream(&self, stream: Stream) -> Result<Subscription, ClientError> {
        let deadline = self.clock.deadline(self.timeout);
        match self.greeted(deadline).await? {
            Greeted::Ready(session) => session.open_stream(stream).await,
            Greeted::Refused { session, frame } => {
                let id = frame.id().unwrap_or(RequestId(1));
                Ok(session.into_subscription(stream, id, frame))
            }
        }
    }

    async fn greeted(&self, deadline: C::Deadline) -> Result<Greeted<C>, ClientError> {
        let mut session = self.open(deadline).await?;
        match session.hello(&self.name).await? {
            Response::Ok {
                value: OkValue::Hello(hello),
                ..
            } => match hello.owner {
                OwnerState::Ready => Ok(Greeted::Ready(session)),
                OwnerState::Leaving => Err(ControlError::owner_leaving().into()),
            },
            frame => Ok(Greeted::Refused { session, frame }),
        }
    }

    /// Подключиться до срока. У сокета профиля каждая попытка начинается с пробы `flock`:
    /// свободная блокировка — сразу `not_running`.
    async fn open(&self, deadline: C::Deadline) -> Result<Session<C>, ClientError> {
        let socket = self.address.socket();
        loop {
            if let Address::Profile(profile) = &self.address {
                match probe_owner(profile) {
                    Ok(Probe::NotRunning) => return Err(ControlError::not_running().into()),
                    Ok(Probe::Held) => {}
                    Err(error) => {
                        return Err(ClientError::Failed(format!(
                            "проба владельца не удалась: {error}"
                        )));
                    }
                }
            }
            let failure = match Session::connect(self.clock.clone(), deadline, &socket).await {
                Ok(session) => return Ok(session),
                Err(failure) => failure,
            };
            if matches!(failure, ConnectError::Expired)
                || self.clock.wait_retry(deadline).await.is_err()
            {
                return Err(self.expired(&socket, &failure).into());
            }
        }
    }

    /// Срок подключения истёк. Блокировка профиля занята, а сокета нет или его никто не
    /// слушает — владелец запускается или перезапускается.
    fn expired(&self, socket: &Path, failure: &ConnectError) -> ControlError {
        match &self.address {
            Address::Profile(_) if failure.is_absent() => ControlError::owner_starting(),
            Address::Profile(_) | Address::Socket(_) => {
                ControlError::unavailable(format_args!("{} до срока: {failure}", socket.display()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use shellio_core::{CoreError, Entity};

    use super::*;

    #[test]
    fn exit_codes_follow_table() {
        let expected = [
            (ControlErrorCode::HelloRequired, 1),
            (ControlErrorCode::ProtocolMismatch, 1),
            (ControlErrorCode::MalformedRequest, 1),
            (ControlErrorCode::RequestTooLarge, 1),
            (ControlErrorCode::ScopeRequired, 1),
            (ControlErrorCode::ScopeUnsupported, 1),
            (ControlErrorCode::OwnerLeaving, 75),
            (ControlErrorCode::NotRunning, 69),
            (ControlErrorCode::OwnerStarting, 75),
            (ControlErrorCode::Unavailable, 75),
            (ControlErrorCode::OutcomeUnknown, 74),
            (ControlErrorCode::NoUi, 1),
            (ControlErrorCode::UiTimeout, 75),
            (ControlErrorCode::GalleryOnly, 1),
            (ControlErrorCode::IsolatedProfileOnly, 1),
        ];
        assert_eq!(expected.len(), ControlErrorCode::ALL.len());
        for (code, exit) in expected {
            assert_eq!(Exit::of_control(code).code(), exit, "{code}");
        }
        let expected = [
            (ErrorCode::InvalidArgument, 1),
            (ErrorCode::NotFound, 1),
            (ErrorCode::NameTaken, 1),
            (ErrorCode::RootTaken, 1),
            (ErrorCode::StorageUnavailable, 75),
            (ErrorCode::StorageConflict, 1),
            (ErrorCode::Busy, 75),
            (ErrorCode::ShuttingDown, 1),
            (ErrorCode::NoUi, 1),
        ];
        assert_eq!(expected.len(), ErrorCode::ALL.len());
        for (code, exit) in expected {
            assert_eq!(Exit::of_core(code).code(), exit, "{code}");
        }
        assert_eq!(Exit::Success.code(), 0);
        assert_eq!(Exit::Usage.code(), 2);
    }

    #[test]
    fn exit_of_frames_and_client_errors() {
        let core = Response::CoreError {
            id: RequestId(2),
            error: CoreError::not_found(Entity::Workspace, "x"),
        };
        assert_eq!(Exit::of_response(&core), Exit::Failure);
        let busy = Response::CoreError {
            id: RequestId(2),
            error: CoreError::busy(),
        };
        assert_eq!(Exit::of_response(&busy), Exit::Retry);
        let leaving = Response::Error {
            id: Some(RequestId(2)),
            error: ControlError::owner_leaving(),
        };
        assert_eq!(Exit::of_response(&leaving), Exit::Retry);
        assert_eq!(
            Exit::of_error(&ControlError::outcome_unknown("обрыв").into()),
            Exit::OutcomeUnknown
        );
        assert_eq!(
            Exit::of_error(&ClientError::Failed("негодный ответ".to_owned())),
            Exit::Failure
        );
    }

    #[test]
    fn address_prefers_flag_then_env_then_profile() {
        let calls = Cell::new(0);
        let profile = || {
            calls.set(calls.get() + 1);
            Ok(Profile::isolated("/state"))
        };

        let flag = Address::resolve(
            Some(PathBuf::from("/flag.sock")),
            Some(OsString::from("/env.sock")),
            profile,
        )
        .unwrap();
        assert_eq!(flag, Address::Socket(PathBuf::from("/flag.sock")));
        let env = Address::resolve(None, Some(OsString::from("/env.sock")), profile).unwrap();
        assert_eq!(env, Address::Socket(PathBuf::from("/env.sock")));
        assert_eq!(calls.get(), 0, "профиль при явном адресе не вычисляется");

        let own = Address::resolve(None, None, profile).unwrap();
        assert_eq!(own, Address::Profile(Profile::isolated("/state")));
        assert_eq!(own.socket(), PathBuf::from("/state/shellio.sock"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn empty_explicit_address_is_refused() {
        let profile = || Ok(Profile::isolated("/state"));
        assert!(matches!(
            Address::resolve(Some(PathBuf::new()), None, profile),
            Err(AddressError::EmptySocket { source: "--socket" })
        ));
        assert!(matches!(
            Address::resolve(None, Some(OsString::new()), profile),
            Err(AddressError::EmptySocket { source: SOCKET_VAR })
        ));
        assert!(matches!(
            Address::resolve(None, None, || Err(ProfileError::HomeMissing)),
            Err(AddressError::Profile(ProfileError::HomeMissing))
        ));
    }

    #[test]
    fn failure_after_send_depends_on_mutation() {
        let cases = [
            (ReadFailure::Expired, true, Exit::OutcomeUnknown),
            (ReadFailure::Closed("x".into()), true, Exit::OutcomeUnknown),
            (ReadFailure::Bad("x".into()), true, Exit::OutcomeUnknown),
            (ReadFailure::Expired, false, Exit::Retry),
            (ReadFailure::Closed("x".into()), false, Exit::Retry),
            (ReadFailure::Bad("x".into()), false, Exit::Failure),
        ];
        for (failure, mutation, exit) in cases {
            assert_eq!(Exit::of_error(&failure.after_send(mutation)), exit);
        }
    }

    #[test]
    fn answer_requires_same_id_and_expected_kind() {
        use shellio_core::{Reply, ReplyValue, Revision, StatusView};

        let reply = || {
            OkValue::Reply(Reply {
                revision: Revision(1),
                value: ReplyValue::Status(StatusView {
                    schema_version: 1,
                    state_db: PathBuf::from("/state.db"),
                    diagnostics: Vec::new(),
                }),
            })
        };
        let id = RequestId(2);
        let ok = Response::Ok { id, value: reply() };
        assert!(answer(id, Expect::Reply, ok.clone()).is_ok());
        assert!(answer(id, Expect::EventsSince, ok).is_err());
        let other = Response::Ok {
            id: RequestId(3),
            value: reply(),
        };
        assert!(answer(id, Expect::Reply, other).is_err());
        let unnumbered = Response::Error {
            id: None,
            error: ControlError::request_too_large(1),
        };
        assert!(answer(id, Expect::Reply, unnumbered).is_ok());
    }
}
