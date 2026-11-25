//! Кадры протокола v2 управляющего сокета: одна строка JSON на кадр, конверт с номером
//! запроса.
//!
//! Команды, чтения, ответы, события и `CoreError` — типы P1.1 как есть. Лишнее поле,
//! неизвестный вид кадра и `null` на месте вида отвергаются во всех кадрах.

use std::fmt;
use std::path::PathBuf;

use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use shellio_core::{
    Command, CoreError, Event, EventsSince, Query, Reply, Revision, Snapshot, TreeScope,
};

use crate::error::ControlError;
use crate::ui::{UiReply, UiRequest};

/// Версия протокола, которую говорят сервер и клиент этой сборки. Версия 2 добавила в снимок
/// панели и раскладки (ADR-0007, «Уточнено в P1.4 (планирование)»), версия 3 — данные и статус
/// панелей, вызывающую панель и их дельты (ADR-0007, «Уточнено в P1.5 (планирование)»): смешанные
/// версии расходятся на `hello`.
pub const PROTOCOL: u32 = 4;

/// Номер запроса. Ответ несёт номер своего запроса, кадры подписки — номер `subscribe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Запрос клиента: номер и вид.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "WireRequest")]
pub struct Request {
    pub id: RequestId,
    pub body: RequestBody,
}

/// Вид запроса. `scope` есть только у команды, `events_since` и `subscribe`; у двух
/// последних допустима только область всех воркспейсов, поэтому она не хранится.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestBody {
    Hello(Hello),
    Command {
        command: Command,
        scope: TreeScope,
    },
    Query(Query),
    EventsSince {
        cursor: Revision,
    },
    Subscribe,
    Launch(Launch),
    /// Запрос к UI владельца (P1.3): выполняется на главном потоке.
    Ui(UiRequest),
}

impl RequestBody {
    /// Ключ вида в кадре.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Hello(_) => "hello",
            Self::Command { .. } => "command",
            Self::Query(_) => "query",
            Self::EventsSince { .. } => "events_since",
            Self::Subscribe => "subscribe",
            Self::Launch(_) => "launch",
            Self::Ui(_) => "ui",
        }
    }
}

/// Первый запрос соединения: версия протокола и имя клиента.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub protocol: u32,
    pub client: String,
}

/// Аргументы повторного запуска и рабочий каталог отправителя.
///
/// Аргумент не в UTF-8 второй запуск не отправляет: его отказ `arg_not_utf8` печатается
/// локально.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorBody {
    cursor: Revision,
}

/// Тело `subscribe`: пустой объект. Единичная структура сериализовалась бы как `null`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyBody {}

/// Поле вида: ключ есть, значение не `null`. Отсутствие ключа — `None` через `default`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Кадр запроса до проверки вида и области.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    id: RequestId,
    #[serde(default, deserialize_with = "present")]
    hello: Option<Hello>,
    #[serde(default, deserialize_with = "present")]
    command: Option<Command>,
    #[serde(default, deserialize_with = "present")]
    query: Option<Query>,
    #[serde(default, deserialize_with = "present")]
    events_since: Option<CursorBody>,
    #[serde(default, deserialize_with = "present")]
    subscribe: Option<EmptyBody>,
    #[serde(default, deserialize_with = "present")]
    launch: Option<Launch>,
    #[serde(default, deserialize_with = "present")]
    ui: Option<UiRequest>,
    #[serde(default, deserialize_with = "present")]
    scope: Option<TreeScope>,
}

const REQUEST_KINDS: &str = "hello, command, query, events_since, subscribe, launch, ui";

/// У вида `kind` поля `scope` нет.
fn without_scope(kind: &str, scope: Option<TreeScope>) -> Result<(), ControlError> {
    match scope {
        None => Ok(()),
        Some(_) => Err(ControlError::malformed_request(format_args!(
            "поле scope у запроса {kind} не допускается"
        ))),
    }
}

/// У вида `kind` область обязательна и допустима только `all`.
fn all_scope(kind: &str, scope: Option<TreeScope>) -> Result<(), ControlError> {
    match scope {
        Some(TreeScope::All) => Ok(()),
        Some(TreeScope::Workspace(_)) => Err(ControlError::scope_unsupported(kind)),
        None => Err(ControlError::scope_required(kind)),
    }
}

impl WireRequest {
    fn into_body(self) -> Result<RequestBody, ControlError> {
        let Self {
            id: _,
            hello,
            command,
            query,
            events_since,
            subscribe,
            launch,
            ui,
            scope,
        } = self;
        let kinds = [
            hello.is_some(),
            command.is_some(),
            query.is_some(),
            events_since.is_some(),
            subscribe.is_some(),
            launch.is_some(),
            ui.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if kinds > 1 {
            return Err(ControlError::malformed_request(format_args!(
                "в кадре {kinds} вида запроса вместо одного"
            )));
        }
        if let Some(hello) = hello {
            without_scope("hello", scope)?;
            Ok(RequestBody::Hello(hello))
        } else if let Some(command) = command {
            let scope = scope.ok_or_else(|| ControlError::scope_required("command"))?;
            Ok(RequestBody::Command { command, scope })
        } else if let Some(query) = query {
            without_scope("query", scope)?;
            Ok(RequestBody::Query(query))
        } else if let Some(CursorBody { cursor }) = events_since {
            all_scope("events_since", scope)?;
            Ok(RequestBody::EventsSince { cursor })
        } else if let Some(EmptyBody {}) = subscribe {
            all_scope("subscribe", scope)?;
            Ok(RequestBody::Subscribe)
        } else if let Some(launch) = launch {
            without_scope("launch", scope)?;
            Ok(RequestBody::Launch(launch))
        } else if let Some(ui) = ui {
            without_scope("ui", scope)?;
            Ok(RequestBody::Ui(ui))
        } else {
            Err(ControlError::malformed_request(format_args!(
                "нет вида запроса, ожидается одно из: {REQUEST_KINDS}"
            )))
        }
    }
}

impl TryFrom<WireRequest> for Request {
    type Error = String;

    fn try_from(wire: WireRequest) -> Result<Self, Self::Error> {
        let id = wire.id;
        let body = wire.into_body().map_err(|error| error.to_string())?;
        Ok(Self { id, body })
    }
}

/// Отказ разбора строки запроса: номер, если он разобран, и ошибка для ответа.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected {
    pub id: Option<RequestId>,
    pub error: ControlError,
}

impl Rejected {
    /// Кадр ответа с этим отказом.
    pub fn into_response(self) -> Response {
        Response::Error {
            id: self.id,
            error: self.error,
        }
    }
}

impl Request {
    /// Разобрать строку запроса без перевода строки.
    ///
    /// Негодный кадр — `malformed_request` с номером, если номер читается и без остального
    /// кадра; у команды, `events_since` и `subscribe` без `scope` — `scope_required`, у двух
    /// последних с областью воркспейса — `scope_unsupported`.
    pub fn parse(line: &[u8]) -> Result<Self, Rejected> {
        match serde_json::from_slice::<WireRequest>(line) {
            Ok(wire) => {
                let id = wire.id;
                match wire.into_body() {
                    Ok(body) => Ok(Self { id, body }),
                    Err(error) => Err(Rejected {
                        id: Some(id),
                        error,
                    }),
                }
            }
            Err(error) => Err(Rejected {
                id: recover_id(line),
                error: ControlError::malformed_request(error),
            }),
        }
    }
}

/// Номер негодного кадра, если строка — JSON с числовым `id`.
fn recover_id(line: &[u8]) -> Option<RequestId> {
    #[derive(Deserialize)]
    struct Numbered {
        id: RequestId,
    }
    serde_json::from_slice::<Numbered>(line)
        .ok()
        .map(|numbered| numbered.id)
}

impl Serialize for Request {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("id", &self.id)?;
        let kind = self.body.kind();
        match &self.body {
            RequestBody::Hello(hello) => map.serialize_entry(kind, hello)?,
            RequestBody::Command { command, scope } => {
                map.serialize_entry(kind, command)?;
                map.serialize_entry("scope", scope)?;
            }
            RequestBody::Query(query) => map.serialize_entry(kind, query)?,
            RequestBody::EventsSince { cursor } => {
                map.serialize_entry(kind, &CursorBody { cursor: *cursor })?;
                map.serialize_entry("scope", &TreeScope::All)?;
            }
            RequestBody::Subscribe => {
                map.serialize_entry(kind, &EmptyBody {})?;
                map.serialize_entry("scope", &TreeScope::All)?;
            }
            RequestBody::Launch(launch) => map.serialize_entry(kind, launch)?,
            RequestBody::Ui(ui) => map.serialize_entry(kind, ui)?,
        }
        map.end()
    }
}

/// Кадр сервера: ответ на запрос или кадр потока подписки.
///
/// Номер равен `null` только у отказа транспорта, когда номер запроса не разобран.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "WireResponse")]
pub enum Response {
    /// Успех запроса.
    Ok { id: RequestId, value: OkValue },
    /// Отказ ядра.
    CoreError { id: RequestId, error: CoreError },
    /// Отказ транспорта.
    Error {
        id: Option<RequestId>,
        error: ControlError,
    },
    /// Первый кадр подписки.
    Snapshot { id: RequestId, snapshot: Snapshot },
    /// Событие подписки.
    Event { id: RequestId, event: Event },
    /// Подписчик отстал: новый снимок, дальше события после его ревизии.
    Reset { id: RequestId, snapshot: Snapshot },
}

impl Response {
    /// Номер запроса, к которому относится кадр.
    pub const fn id(&self) -> Option<RequestId> {
        match self {
            Self::Error { id, .. } => *id,
            Self::Ok { id, .. }
            | Self::CoreError { id, .. }
            | Self::Snapshot { id, .. }
            | Self::Event { id, .. }
            | Self::Reset { id, .. } => Some(*id),
        }
    }

    /// Разобрать строку кадра сервера без перевода строки.
    pub fn parse(line: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(line)
    }
}

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("id", &self.id())?;
        match self {
            Self::Ok { value, .. } => map.serialize_entry("ok", value)?,
            Self::CoreError { error, .. } => map.serialize_entry("core_error", error)?,
            Self::Error { error, .. } => map.serialize_entry("error", error)?,
            Self::Snapshot { snapshot, .. } => map.serialize_entry("snapshot", snapshot)?,
            Self::Event { event, .. } => map.serialize_entry("event", event)?,
            Self::Reset { snapshot, .. } => map.serialize_entry("reset", snapshot)?,
        }
        map.end()
    }
}

/// Кадр сервера до проверки вида и номера. Ключ `id` обязателен и у отказа может быть
/// `null`: `deserialize_with` снимает умолчание `None` для отсутствующего ключа.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireResponse {
    #[serde(deserialize_with = "Option::deserialize")]
    id: Option<RequestId>,
    #[serde(default, deserialize_with = "present")]
    ok: Option<OkValue>,
    #[serde(default, deserialize_with = "present")]
    core_error: Option<CoreError>,
    #[serde(default, deserialize_with = "present")]
    error: Option<ControlError>,
    #[serde(default, deserialize_with = "present")]
    snapshot: Option<Snapshot>,
    #[serde(default, deserialize_with = "present")]
    event: Option<Event>,
    #[serde(default, deserialize_with = "present")]
    reset: Option<Snapshot>,
}

impl TryFrom<WireResponse> for Response {
    type Error = String;

    fn try_from(wire: WireResponse) -> Result<Self, String> {
        let WireResponse {
            id,
            ok,
            core_error,
            error,
            snapshot,
            event,
            reset,
        } = wire;
        let kinds = [
            ok.is_some(),
            core_error.is_some(),
            error.is_some(),
            snapshot.is_some(),
            event.is_some(),
            reset.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if kinds > 1 {
            return Err(format!("в кадре {kinds} вида ответа вместо одного"));
        }
        if let Some(error) = error {
            return Ok(Self::Error { id, error });
        }
        let id = id.ok_or_else(|| "id равен null только у кадра error".to_owned())?;
        if let Some(value) = ok {
            Ok(Self::Ok { id, value })
        } else if let Some(error) = core_error {
            Ok(Self::CoreError { id, error })
        } else if let Some(snapshot) = snapshot {
            Ok(Self::Snapshot { id, snapshot })
        } else if let Some(event) = event {
            Ok(Self::Event { id, event })
        } else if let Some(snapshot) = reset {
            Ok(Self::Reset { id, snapshot })
        } else {
            Err(
                "нет вида ответа, ожидается одно из: ok, core_error, error, snapshot, event, reset"
                    .to_owned(),
            )
        }
    }
}

/// Значение успешного ответа. `Reply` и `EventsSince` идут как есть, `hello`, `launch` и
/// `ui` — под своим ключом.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(from = "WireOk")]
pub enum OkValue {
    /// Ответ `hello`.
    Hello(HelloReply),
    /// Ответ команды или чтения P1.1.
    Reply(Reply),
    /// Ответ `events_since` P1.1.
    EventsSince(EventsSince),
    /// Квитанция `launch`.
    Launch(LaunchReceipt),
    /// Ответ запроса `ui`.
    Ui(UiReply),
}

impl Serialize for OkValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Hello(hello) => keyed(serializer, "hello", hello),
            Self::Reply(reply) => reply.serialize(serializer),
            Self::EventsSince(since) => since.serialize(serializer),
            Self::Launch(receipt) => keyed(serializer, "launch", receipt),
            Self::Ui(reply) => keyed(serializer, "ui", reply),
        }
    }
}

fn keyed<S: Serializer, T: Serialize>(
    serializer: S,
    key: &str,
    value: &T,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(1))?;
    map.serialize_entry(key, value)?;
    map.end()
}

/// Без тега: варианты различаются ключами, лишние ключи отвергаются, поэтому разбор
/// однозначен.
#[derive(Deserialize)]
#[serde(untagged)]
enum WireOk {
    Keyed(KeyedOk),
    Reply(Reply),
    EventsSince(EventsSince),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum KeyedOk {
    Hello(HelloReply),
    Launch(LaunchReceipt),
    Ui(UiReply),
}

impl From<WireOk> for OkValue {
    fn from(wire: WireOk) -> Self {
        match wire {
            WireOk::Keyed(KeyedOk::Hello(hello)) => Self::Hello(hello),
            WireOk::Keyed(KeyedOk::Launch(receipt)) => Self::Launch(receipt),
            WireOk::Keyed(KeyedOk::Ui(reply)) => Self::Ui(reply),
            WireOk::Reply(reply) => Self::Reply(reply),
            WireOk::EventsSince(since) => Self::EventsSince(since),
        }
    }
}

/// Ответ `hello`: версия протокола, версия сервера, PID владельца и его состояние.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloReply {
    pub protocol: u32,
    pub server: String,
    pub pid: u32,
    pub owner: OwnerState,
}

/// Состояние владельца в ответе `hello`. `starting` по сокету не наблюдается: сокет
/// публикуется после готовности.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerState {
    Ready,
    Leaving,
}

/// Квитанция `launch`: исход активации и отказанные аргументы. Квитанция с отказами —
/// успех передачи.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchReceipt {
    pub activation: Activation,
    pub refused: Vec<RefusedArg>,
}

/// Исход активации владельца.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
    /// Вызов активации на главном потоке выполнен. Выход на передний план он не обещает.
    Requested,
    /// UI нет: только у тестового владельца.
    NoUi,
}

/// Отказанный аргумент запуска.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefusedArg {
    pub arg: String,
    pub reason: RefuseReason,
}

/// Причина отказа аргумента запуска. Значения стабильны.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefuseReason {
    /// Существующий каталог: открытие папок появится в P1.7.
    FolderOpenUnsupported,
    /// Существующий путь, но не каталог.
    NotADirectory,
    /// Пути нет.
    PathNotFound,
    /// Флаг, которого владелец не знает.
    UnknownArgument,
    /// Аргумент не в UTF-8. Ставится вторым запуском локально, владельцу не отправляется.
    ArgNotUtf8,
    /// `--gallery`, пересланный вторым запуском живому владельцу: режим галереи выбирается
    /// только при запуске владельца.
    GalleryNotAtStart,
}

impl RefuseReason {
    /// Все причины.
    pub const ALL: [Self; 6] = [
        Self::FolderOpenUnsupported,
        Self::NotADirectory,
        Self::PathNotFound,
        Self::UnknownArgument,
        Self::ArgNotUtf8,
        Self::GalleryNotAtStart,
    ];

    /// Имя причины в JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FolderOpenUnsupported => "folder_open_unsupported",
            Self::NotADirectory => "not_a_directory",
            Self::PathNotFound => "path_not_found",
            Self::UnknownArgument => "unknown_argument",
            Self::ArgNotUtf8 => "arg_not_utf8",
            Self::GalleryNotAtStart => "gallery_not_at_start",
        }
    }

    /// Текст квитанции для человека.
    pub const fn message(self) -> &'static str {
        match self {
            Self::FolderOpenUnsupported => {
                "папка не открыта: открытие папок пока не поддерживается"
            }
            Self::NotADirectory => "не открыт: это не папка",
            Self::PathNotFound => "не открыт: путь не найден",
            Self::UnknownArgument => "неизвестный аргумент",
            Self::ArgNotUtf8 => "не передан: аргумент не в UTF-8",
            Self::GalleryNotAtStart => "галерея открывается только при запуске владельца",
        }
    }
}

impl fmt::Display for RefuseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn refuse_reasons_have_stable_names() {
        let expected = [
            "folder_open_unsupported",
            "not_a_directory",
            "path_not_found",
            "unknown_argument",
            "arg_not_utf8",
            "gallery_not_at_start",
        ];
        assert_eq!(RefuseReason::ALL.len(), expected.len());
        for (reason, name) in RefuseReason::ALL.into_iter().zip(expected) {
            assert_eq!(reason.as_str(), name);
            assert_eq!(reason.to_string(), name);
            assert_eq!(serde_json::to_value(reason).unwrap(), Value::from(name));
            assert!(!reason.message().is_empty());
        }
    }

    #[test]
    fn folder_refusal_says_folder_is_not_opened() {
        assert!(
            RefuseReason::FolderOpenUnsupported
                .message()
                .starts_with("папка не открыта")
        );
    }

    #[test]
    fn activation_and_owner_state_names() {
        assert_eq!(
            serde_json::to_value(Activation::Requested).unwrap(),
            Value::from("requested")
        );
        assert_eq!(
            serde_json::to_value(Activation::NoUi).unwrap(),
            Value::from("no_ui")
        );
        assert_eq!(
            serde_json::to_value(OwnerState::Ready).unwrap(),
            Value::from("ready")
        );
        assert_eq!(
            serde_json::to_value(OwnerState::Leaving).unwrap(),
            Value::from("leaving")
        );
    }

    #[test]
    fn rejected_frame_carries_explicit_null_id() {
        let rejected = Rejected {
            id: None,
            error: ControlError::hello_required(),
        };
        let json = serde_json::to_value(rejected.into_response()).unwrap();
        assert_eq!(json.get("id"), Some(&Value::Null));
    }
}
