//! Транспорт адаптеров: запрос, ответ, отказ и след вызовов.
//!
//! Отказ транспорта отличает запрос, который точно не ушёл ([`FailureKind::NotSent`]), от
//! запроса, который мог дойти до сервера: таймаут, обрыв соединения, испорченный ответ. Только
//! первый даёт право считать запись не принятой; остальные ведут элемент отправки в `unknown` и
//! к сверке (ADR-0015). Ответ HTTP с любым кодом — это ответ, а не отказ.
//!
//! След пишет каждый вызов, в том числе отказавший. Сырой вызов [`Call`] на диск не попадает:
//! сериализуется только [`TraceEntry`], собранный очистителем ([`crate::scrub`]). Заголовки
//! запроса в след не входят вовсе, из заголовков ответа — только заголовки лимитов. Тело ответа
//! с полной интроспекцией `data.__schema` (9,4 МБ у GitLab CE 19.6.1) идёт в след сводкой
//! [`introspection_summary`].

pub mod http;
pub mod scripted;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::scrub::Scrubber;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    /// Метод, который может изменить состояние сервера.
    pub fn is_write(self) -> bool {
        match self {
            Method::Get => false,
            Method::Post | Method::Put | Method::Patch | Method::Delete => true,
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Заголовки, значение которых — учётные данные. `Debug` запроса их не показывает.
const SECRET_HEADERS: [&str; 6] = [
    "authorization",
    "proxy-authorization",
    "private-token",
    "cookie",
    "set-cookie",
    "x-gitlab-token",
];

pub fn is_secret_header(name: &str) -> bool {
    SECRET_HEADERS
        .iter()
        .any(|secret| secret.eq_ignore_ascii_case(name))
}

/// Запрос транспорта. Учётные данные держит сам транспорт ([`http::Credential`]), а не запрос:
/// запрос видят адаптер, встроенный транспорт и след.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl Request {
    pub fn new(method: Method, url: impl Into<String>) -> Request {
        Request {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn get(url: impl Into<String>) -> Request {
        Request::new(Method::Get, url)
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Request {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn json(mut self, value: &Value) -> Request {
        self.headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
        self.headers
            .push(("content-type".to_owned(), "application/json".to_owned()));
        self.body = Some(value.to_string().into_bytes());
        self
    }

    pub fn form(mut self, pairs: &[(&str, &str)]) -> Request {
        let body = pairs
            .iter()
            .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        self.headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
        self.headers.push((
            "content-type".to_owned(),
            "application/x-www-form-urlencoded".to_owned(),
        ));
        self.body = Some(body.into_bytes());
        self
    }

    /// Путь с запросом: часть адреса после хоста.
    pub fn path(&self) -> &str {
        path_of(&self.url)
    }

    pub fn body_json(&self) -> Option<Value> {
        self.body
            .as_deref()
            .and_then(|body| serde_json::from_slice(body).ok())
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(name, value)| {
                let shown = if is_secret_header(name) {
                    "<redacted>"
                } else {
                    value.as_str()
                };
                (name.as_str(), shown)
            })
            .collect();
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field(
                "body",
                &self.body.as_deref().map(String::from_utf8_lossy).as_deref(),
            )
            .finish()
    }
}

/// Часть адреса после схемы и хоста; адрес без схемы возвращается целиком.
pub fn path_of(url: &str) -> &str {
    match url.find("://") {
        Some(scheme_end) => {
            let rest = &url[scheme_end + 3..];
            match rest.find(['/', '?']) {
                Some(start) => &rest[start..],
                None => "/",
            }
        }
        None => url,
    }
}

pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Ответ HTTP с любым кодом. Имена заголовков в нижнем регистре.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn json(status: u16, value: &Value) -> Response {
        Response::new(status)
            .with_header("content-type", "application/json")
            .with_body(value.to_string())
    }

    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Response {
        self.headers.push((name.to_ascii_lowercase(), value.into()));
        self
    }

    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Response {
        self.body = body.into();
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn body_json(&self) -> Result<Value, String> {
        serde_json::from_slice(&self.body)
            .map_err(|err| format!("ответ {} не JSON: {err}", self.status))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FailureKind {
    /// Запрос не ушёл: неверный адрес, хост не найден, соединение не установлено.
    NotSent,
    /// Срок истёк после установки соединения: запрос мог дойти и быть принят.
    Timeout,
    /// Соединение оборвалось после отправки или во время ответа.
    Disconnected,
    /// Сервер ответил не по протоколу HTTP.
    Malformed,
}

impl FailureKind {
    pub fn name(self) -> &'static str {
        match self {
            FailureKind::NotSent => "not_sent",
            FailureKind::Timeout => "timeout",
            FailureKind::Disconnected => "disconnected",
            FailureKind::Malformed => "malformed",
        }
    }
}

/// Отказ транспорта: ответа нет или он неполон.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    /// Код, если заголовки ответа пришли, а тело — нет.
    pub status: Option<u16>,
    /// Описание без адреса и тела запроса.
    pub detail: String,
}

impl Failure {
    pub fn new(kind: FailureKind, detail: impl Into<String>) -> Failure {
        Failure {
            kind,
            status: None,
            detail: detail.into(),
        }
    }

    pub fn not_sent(detail: impl Into<String>) -> Failure {
        Failure::new(FailureKind::NotSent, detail)
    }

    pub fn timeout(detail: impl Into<String>) -> Failure {
        Failure::new(FailureKind::Timeout, detail)
    }

    pub fn disconnected(detail: impl Into<String>) -> Failure {
        Failure::new(FailureKind::Disconnected, detail)
    }

    pub fn with_status(mut self, status: u16) -> Failure {
        self.status = Some(status);
        self
    }

    /// Запрос мог дойти до сервера, и запись могла быть принята: итог неизвестен.
    pub fn may_have_been_sent(&self) -> bool {
        self.kind != FailureKind::NotSent
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.name(), self.detail)?;
        if let Some(status) = self.status {
            write!(f, " (код {status})")?;
        }
        Ok(())
    }
}

/// Итог записи для журнала, общий для адаптеров обоих провайдеров.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteFate {
    /// Запись не принята: запрос не ушёл или сервер явно отказал. Элемент — `failed`.
    NotAccepted,
    /// Запись могла быть принята. Элемент — `unknown`, нужна сверка.
    Unknown,
}

pub trait Transport {
    fn send(&self, request: &Request) -> Result<Response, Failure>;
}

impl<T: Transport + ?Sized> Transport for &T {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        (**self).send(request)
    }
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        (**self).send(request)
    }
}

/// Сырой вызов в памяти. Не сериализуется: на диск идёт только [`TraceEntry`].
#[derive(Clone, Debug)]
pub struct Call {
    pub seq: u64,
    pub actor: &'static str,
    pub request: Request,
    pub outcome: Result<Response, Failure>,
    pub duration: Duration,
}

impl Call {
    pub fn status(&self) -> Option<u16> {
        match &self.outcome {
            Ok(response) => Some(response.status),
            Err(failure) => failure.status,
        }
    }
}

/// Заголовки лимитов GitHub (`x-ratelimit-*`), GitLab (`ratelimit-*`) и `retry-after`.
pub fn is_rate_limit_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("x-ratelimit-") || name.starts_with("ratelimit-") || name == "retry-after"
}

/// Очищенная запись следа для `result.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceEntry {
    pub seq: u64,
    pub actor: String,
    pub method: String,
    pub path: String,
    pub request_body: Value,
    pub status: Option<u16>,
    pub failure: Option<String>,
    pub failure_detail: Option<String>,
    pub response_body: Value,
    pub rate_limit: BTreeMap<String, String>,
    pub duration_us: u64,
}

impl TraceEntry {
    pub fn from_call(call: &Call, scrubber: &Scrubber) -> TraceEntry {
        let (response_body, rate_limit, failure, failure_detail) = match &call.outcome {
            Ok(response) => {
                let rate_limit = response
                    .headers
                    .iter()
                    .filter(|(name, _)| is_rate_limit_header(name))
                    .map(|(name, value)| {
                        (
                            name.to_ascii_lowercase(),
                            scrubber.scrub_header(name, value),
                        )
                    })
                    .collect();
                let body =
                    if is_profile_read(&call.request) && (200..300).contains(&response.status) {
                        profile_summary(&response.body, scrubber)
                    } else {
                        response_body(&response.body, scrubber)
                    };
                (body, rate_limit, None, None)
            }
            Err(failure) => (
                Value::Null,
                BTreeMap::new(),
                Some(failure.kind.name().to_owned()),
                Some(scrubber.scrub_text(&failure.detail)),
            ),
        };
        TraceEntry {
            seq: call.seq,
            actor: call.actor.to_owned(),
            method: call.request.method.as_str().to_owned(),
            path: scrubber.scrub_url(call.request.path()),
            request_body: call
                .request
                .body
                .as_deref()
                .map_or(Value::Null, |body| scrubber.scrub_body(body)),
            status: call.status(),
            failure,
            failure_detail,
            response_body,
            rate_limit,
            duration_us: u64::try_from(call.duration.as_micros()).unwrap_or(u64::MAX),
        }
    }
}

/// Чтение профиля своего аккаунта: `GET /user` REST обоих хостов.
fn is_profile_read(request: &Request) -> bool {
    let path = request.path();
    request.method == Method::Get && path.split('?').next().unwrap_or(path).ends_with("/user")
}

/// Тело `GET /user` для следа — сводка ключей вместо значений. Кроме логина, имени и почты профиль
/// несёт ID аккаунта, биографию, место, организацию, сайты, внешние учётные записи и даты входа, а
/// набор этих полей у хостов разный, поэтому очиститель по значениям и ключам его не покрывает.
fn profile_summary(body: &[u8], scrubber: &Scrubber) -> Value {
    let keys: Option<Vec<String>> = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .as_object()
                .map(|map| map.keys().map(|key| scrubber.scrub_text(key)).collect())
        });
    json!({
        "summary": "account profile",
        "bytes": body.len(),
        "keys": keys,
    })
}

/// Тело ответа для следа: полная интроспекция — сводкой, остальное — через очиститель.
fn response_body(body: &[u8], scrubber: &Scrubber) -> Value {
    const MARK: &[u8] = b"\"__schema\"";
    if body.windows(MARK.len()).any(|window| window == MARK)
        && let Ok(value) = serde_json::from_slice::<Value>(body)
        && let Some(summary) = introspection_summary(&value, body.len())
    {
        return summary;
    }
    scrubber.scrub_body(body)
}

/// Сводка ответа с полной интроспекцией `data.__schema` вместо тела: ключи верхнего уровня, размер
/// тела, число типов, имя и число полей типа мутаций и наличие мутаций, которые читают адаптеры.
/// Ответ без объекта `data.__schema` — `None`. В сводке только имена схемы и числа, очищать нечего.
pub fn introspection_summary(value: &Value, bytes: usize) -> Option<Value> {
    let schema = value.get("data")?.get("__schema")?.as_object()?;
    let keys: Vec<&str> = value.as_object()?.keys().map(String::as_str).collect();
    let types = schema.get("types").and_then(Value::as_array);
    let mutation_type = schema
        .get("mutationType")
        .and_then(|ty| ty.get("name"))
        .and_then(Value::as_str);
    let fields: Option<Vec<&str>> = mutation_type.and_then(|name| {
        let found = types?
            .iter()
            .find(|ty| ty.get("name").and_then(Value::as_str) == Some(name))?;
        Some(
            found
                .get("fields")?
                .as_array()?
                .iter()
                .filter_map(|field| field.get("name").and_then(Value::as_str))
                .collect(),
        )
    });
    let mut watched = Map::new();
    for name in crate::gitlab::capabilities::MUTATIONS
        .iter()
        .chain(crate::github::capabilities::MUTATIONS.iter())
    {
        watched.insert(
            (*name).to_owned(),
            fields
                .as_ref()
                .map_or(Value::Null, |fields| Value::Bool(fields.contains(name))),
        );
    }
    Some(json!({
        "summary": "introspection __schema",
        "bytes": bytes,
        "keys": keys,
        "types": types.map(Vec::len),
        "mutation_type": mutation_type,
        "mutation_fields": fields.as_ref().map(Vec::len),
        "mutations": watched,
    }))
}

/// Общий след нескольких транспортов одного сценария; клон пишет в тот же след.
#[derive(Clone, Default)]
pub struct Trace {
    calls: Arc<Mutex<Vec<Call>>>,
}

impl Trace {
    pub fn new() -> Trace {
        Trace::default()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Call>> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(
        &self,
        actor: &'static str,
        request: &Request,
        outcome: &Result<Response, Failure>,
        duration: Duration,
    ) {
        let mut calls = self.lock();
        let seq = calls.len() as u64 + 1;
        calls.push(Call {
            seq,
            actor,
            request: request.clone(),
            outcome: outcome.clone(),
            duration,
        });
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Сырые вызовы для оракулов в памяти. На диск — только [`Trace::entries`].
    pub fn calls(&self) -> Vec<Call> {
        self.lock().clone()
    }

    pub fn entries(&self, scrubber: &Scrubber) -> Vec<TraceEntry> {
        self.lock()
            .iter()
            .map(|call| TraceEntry::from_call(call, scrubber))
            .collect()
    }

    /// Число вызовов по ключу `МЕТОД путь` без строки запроса; путь очищен, как в
    /// [`Trace::entries`].
    pub fn counts(&self, scrubber: &Scrubber) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for call in self.lock().iter() {
            let path = call.request.path();
            let path = path.split_once('?').map_or(path, |(path, _)| path);
            *counts
                .entry(format!(
                    "{} {}",
                    call.request.method,
                    scrubber.scrub_url(path)
                ))
                .or_insert(0) += 1;
        }
        counts
    }
}

/// Транспорт, который пишет каждый вызов в след от имени роли (`author`, `reviewer`).
pub struct Traced<T> {
    inner: T,
    trace: Trace,
    actor: &'static str,
}

impl<T: Transport> Traced<T> {
    pub fn new(inner: T, trace: Trace, actor: &'static str) -> Traced<T> {
        Traced {
            inner,
            trace,
            actor,
        }
    }
}

impl<T: Transport> Transport for Traced<T> {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        let started = Instant::now();
        let outcome = self.inner.send(request);
        self.trace
            .record(self.actor, request, &outcome, started.elapsed());
        outcome
    }
}

#[cfg(test)]
mod tests;
