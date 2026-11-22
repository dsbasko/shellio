//! Ошибки транспорта управляющего сокета со стабильными кодами.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::wire::PROTOCOL;

/// Стабильный код ошибки транспорта. Коды не переименовываются: новый смысл получает
/// новый код.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlErrorCode {
    /// Первый запрос соединения не `hello`.
    HelloRequired,
    /// Версия протокола клиента не поддерживается. Поддерживаемые — в `details.supported`.
    ProtocolMismatch,
    /// Строка не разбирается как кадр запроса. Соединение остаётся.
    MalformedRequest,
    /// Строка длиннее лимита до перевода строки. Соединение закрывается.
    RequestTooLarge,
    /// У команды, `events_since` или `subscribe` нет `scope`.
    ScopeRequired,
    /// `events_since` или `subscribe` с областью воркспейса.
    ScopeUnsupported,
    /// Владелец завершает работу.
    OwnerLeaving,
    /// Только клиент: блокировка свободна или файла блокировки нет.
    NotRunning,
    /// Только клиент: блокировка занята, сокета нет до срока.
    OwnerStarting,
    /// Только клиент: подключение или ответ не уложились в срок при занятой блокировке.
    Unavailable,
    /// После полной отправки мутации или `launch` истёк срок, оборвалось соединение или пришёл
    /// негодный ответ (клиент); изменяющий запрос `ui` начат, но не закончен к сроку (сервер).
    OutcomeUnknown,
    /// UI нет: тестовый владелец без окна или главный поток больше не выполняет работу.
    NoUi,
    /// Запрос `ui` не начался на главном потоке до срока и уже не выполнится.
    UiTimeout,
    /// Запрос `gallery.*` вне режима галереи.
    GalleryOnly,
}

impl ControlErrorCode {
    /// Все коды.
    pub const ALL: [Self; 14] = [
        Self::HelloRequired,
        Self::ProtocolMismatch,
        Self::MalformedRequest,
        Self::RequestTooLarge,
        Self::ScopeRequired,
        Self::ScopeUnsupported,
        Self::OwnerLeaving,
        Self::NotRunning,
        Self::OwnerStarting,
        Self::Unavailable,
        Self::OutcomeUnknown,
        Self::NoUi,
        Self::UiTimeout,
        Self::GalleryOnly,
    ];

    /// Имя кода в JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HelloRequired => "hello_required",
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::MalformedRequest => "malformed_request",
            Self::RequestTooLarge => "request_too_large",
            Self::ScopeRequired => "scope_required",
            Self::ScopeUnsupported => "scope_unsupported",
            Self::OwnerLeaving => "owner_leaving",
            Self::NotRunning => "not_running",
            Self::OwnerStarting => "owner_starting",
            Self::Unavailable => "unavailable",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::NoUi => "no_ui",
            Self::UiTimeout => "ui_timeout",
            Self::GalleryOnly => "gallery_only",
        }
    }

    /// Имеет ли смысл повторить тот же запрос позже.
    pub const fn retryable(self) -> bool {
        match self {
            Self::OwnerLeaving
            | Self::NotRunning
            | Self::OwnerStarting
            | Self::Unavailable
            | Self::UiTimeout => true,
            Self::HelloRequired
            | Self::ProtocolMismatch
            | Self::MalformedRequest
            | Self::RequestTooLarge
            | Self::ScopeRequired
            | Self::ScopeUnsupported
            | Self::OutcomeUnknown
            | Self::NoUi
            | Self::GalleryOnly => false,
        }
    }
}

impl fmt::Display for ControlErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Ошибка транспорта: стабильный код, сообщение для человека, признак повтора и детали.
/// Форма та же, что у `CoreError`.
///
/// Признак повтора следует из кода: разбор JSON с другим признаком отказывает.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireControlError")]
pub struct ControlError {
    code: ControlErrorCode,
    message: String,
    retryable: bool,
    details: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireControlError {
    code: ControlErrorCode,
    message: String,
    retryable: bool,
    details: Map<String, Value>,
}

impl TryFrom<WireControlError> for ControlError {
    type Error = String;

    fn try_from(wire: WireControlError) -> Result<Self, Self::Error> {
        if wire.retryable != wire.code.retryable() {
            return Err(format!(
                "retryable {} не соответствует коду {}",
                wire.retryable, wire.code
            ));
        }
        Ok(Self::new(wire.code, wire.message, wire.details))
    }
}

impl ControlError {
    fn new(code: ControlErrorCode, message: String, details: Map<String, Value>) -> Self {
        Self {
            code,
            message,
            retryable: code.retryable(),
            details,
        }
    }

    fn plain(code: ControlErrorCode, message: impl Into<String>) -> Self {
        Self::new(code, message.into(), Map::new())
    }

    fn with_detail(code: ControlErrorCode, message: String, key: &str, value: Value) -> Self {
        Self::new(code, message, Map::from_iter([(key.to_owned(), value)]))
    }

    /// Первый запрос соединения не `hello`.
    pub fn hello_required() -> Self {
        Self::plain(
            ControlErrorCode::HelloRequired,
            "первым запросом соединения должен быть hello",
        )
    }

    /// Клиент просит версию протокола `requested`, которую сервер не поддерживает.
    pub fn protocol_mismatch(requested: u32) -> Self {
        Self::with_detail(
            ControlErrorCode::ProtocolMismatch,
            format!(
                "версия протокола {requested} не поддерживается, сервер поддерживает {PROTOCOL}"
            ),
            "supported",
            Value::from(vec![PROTOCOL]),
        )
    }

    /// Строка не разобрана как кадр запроса по причине `reason`.
    pub fn malformed_request(reason: impl fmt::Display) -> Self {
        Self::plain(
            ControlErrorCode::MalformedRequest,
            format!("запрос не разобран: {reason}"),
        )
    }

    /// Строка запроса длиннее `limit` байт до перевода строки.
    pub fn request_too_large(limit: usize) -> Self {
        Self::with_detail(
            ControlErrorCode::RequestTooLarge,
            format!("строка запроса длиннее {limit} байт"),
            "limit",
            Value::from(limit),
        )
    }

    /// У запроса вида `kind` нет `scope`.
    pub fn scope_required(kind: &str) -> Self {
        Self::plain(
            ControlErrorCode::ScopeRequired,
            format!("у запроса {kind} нет области scope"),
        )
    }

    /// Запрос вида `kind` поддерживает только область всех воркспейсов.
    pub fn scope_unsupported(kind: &str) -> Self {
        Self::plain(
            ControlErrorCode::ScopeUnsupported,
            format!("запрос {kind} поддерживает только область all"),
        )
    }

    /// Владелец завершает работу.
    pub fn owner_leaving() -> Self {
        Self::plain(
            ControlErrorCode::OwnerLeaving,
            "владелец завершает работу, повторите позже",
        )
    }

    /// Блокировка профиля свободна или файла блокировки нет.
    pub fn not_running() -> Self {
        Self::plain(ControlErrorCode::NotRunning, "shellio не запущен")
    }

    /// Блокировка занята, сокета нет до срока.
    pub fn owner_starting() -> Self {
        Self::plain(
            ControlErrorCode::OwnerStarting,
            "владелец запускается или перезапускается",
        )
    }

    /// Подключение или ответ не уложились в срок; `reason` — что именно.
    pub fn unavailable(reason: impl fmt::Display) -> Self {
        Self::plain(
            ControlErrorCode::Unavailable,
            format!("владелец недоступен: {reason}"),
        )
    }

    /// Запрос отправлен полностью, но ответа нет; `reason` — что произошло.
    pub fn outcome_unknown(reason: impl fmt::Display) -> Self {
        Self::plain(
            ControlErrorCode::OutcomeUnknown,
            format!("исход неизвестен: {reason}"),
        )
    }

    /// UI нет: тестовый владелец без окна или главный поток больше не выполняет работу.
    pub fn no_ui() -> Self {
        Self::plain(ControlErrorCode::NoUi, "у владельца нет интерфейса")
    }

    /// Запрос `ui` не начался на главном потоке до срока `timeout` и уже не выполнится.
    pub fn ui_timeout(timeout: std::time::Duration) -> Self {
        Self::with_detail(
            ControlErrorCode::UiTimeout,
            format!(
                "главный поток не начал запрос за {} мс, повторите позже",
                timeout.as_millis()
            ),
            "timeout_ms",
            Value::from(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
        )
    }

    /// Запрос `gallery.*` вне режима галереи.
    pub fn gallery_only() -> Self {
        Self::plain(
            ControlErrorCode::GalleryOnly,
            "запрос доступен только в режиме галереи",
        )
    }

    /// Стабильный код.
    pub const fn code(&self) -> ControlErrorCode {
        self.code
    }

    /// Сообщение для человека.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Имеет ли смысл повторить запрос.
    pub const fn retryable(&self) -> bool {
        self.retryable
    }

    /// Деталь `key`, если она есть.
    pub fn detail(&self, key: &str) -> Option<&Value> {
        self.details.get(key)
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ControlError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_have_stable_names_and_retry_flags() {
        let expected = [
            ("hello_required", false),
            ("protocol_mismatch", false),
            ("malformed_request", false),
            ("request_too_large", false),
            ("scope_required", false),
            ("scope_unsupported", false),
            ("owner_leaving", true),
            ("not_running", true),
            ("owner_starting", true),
            ("unavailable", true),
            ("outcome_unknown", false),
            ("no_ui", false),
            ("ui_timeout", true),
            ("gallery_only", false),
        ];
        assert_eq!(ControlErrorCode::ALL.len(), expected.len());
        for (code, (name, retryable)) in ControlErrorCode::ALL.into_iter().zip(expected) {
            assert_eq!(code.as_str(), name);
            assert_eq!(code.to_string(), name);
            assert_eq!(code.retryable(), retryable, "{name}");
            assert_eq!(serde_json::to_value(code).unwrap(), Value::from(name));
        }
    }

    #[test]
    fn parse_rejects_retry_flag_other_than_code() {
        let leaving = ControlError::owner_leaving();
        let mut json = serde_json::to_value(&leaving).unwrap();
        assert_eq!(
            serde_json::from_value::<ControlError>(json.clone()).unwrap(),
            leaving
        );
        json["retryable"] = Value::Bool(false);
        let error = serde_json::from_value::<ControlError>(json).unwrap_err();
        assert!(error.to_string().contains("retryable"), "{error}");
    }

    #[test]
    fn constructors_set_code_and_retry_flag() {
        let cases = [
            (
                ControlError::hello_required(),
                ControlErrorCode::HelloRequired,
            ),
            (
                ControlError::protocol_mismatch(2),
                ControlErrorCode::ProtocolMismatch,
            ),
            (
                ControlError::malformed_request("x"),
                ControlErrorCode::MalformedRequest,
            ),
            (
                ControlError::request_too_large(1),
                ControlErrorCode::RequestTooLarge,
            ),
            (
                ControlError::scope_required("command"),
                ControlErrorCode::ScopeRequired,
            ),
            (
                ControlError::scope_unsupported("subscribe"),
                ControlErrorCode::ScopeUnsupported,
            ),
            (
                ControlError::owner_leaving(),
                ControlErrorCode::OwnerLeaving,
            ),
            (ControlError::not_running(), ControlErrorCode::NotRunning),
            (
                ControlError::owner_starting(),
                ControlErrorCode::OwnerStarting,
            ),
            (
                ControlError::unavailable("срок"),
                ControlErrorCode::Unavailable,
            ),
            (
                ControlError::outcome_unknown("обрыв"),
                ControlErrorCode::OutcomeUnknown,
            ),
            (ControlError::no_ui(), ControlErrorCode::NoUi),
            (
                ControlError::ui_timeout(std::time::Duration::from_secs(2)),
                ControlErrorCode::UiTimeout,
            ),
            (ControlError::gallery_only(), ControlErrorCode::GalleryOnly),
        ];
        assert_eq!(cases.len(), ControlErrorCode::ALL.len());
        for (error, code) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(error.retryable(), code.retryable());
            assert!(!error.message().is_empty());
            assert!(error.to_string().starts_with(code.as_str()));
        }
    }

    #[test]
    fn protocol_mismatch_names_supported_versions() {
        let error = ControlError::protocol_mismatch(7);
        assert_eq!(error.detail("supported"), Some(&Value::from(vec![1])));
        assert!(error.message().contains('7'), "{error}");
    }
}
