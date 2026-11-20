//! `forge-fake`: HTTP-сервер пробы на loopback с состоянием провайдера.
//!
//! Сервер — оракул пробы, а не продуктовый `forge-fake`: он реализует подмножество REST и GraphQL
//! провайдера по документации и строже живого хоста там, где строгость ловит ошибку клиента.
//! Каркас общий для провайдеров: поток `tiny_http`, состояние под замком, правила отказов,
//! журнал вызовов. Провайдер реализует [`Forge`].
//!
//! Отказ задаётся двумя независимыми осями: применяется ли запись ([`Fault::applied`]) и что
//! видит клиент ([`Reply`]). Молчание не роняет запрос `tiny_http` (его `Drop` ответил бы 500):
//! запрос паркуется до [`FakeServer::release_parked`] или остановки сервера, клиент получает
//! отказ по своему сроку.
//!
//! Квитанция сервера — [`FakeServer::wait_calls`]: вызов записан в журнал, запись применена или
//! нет, молчащий запрос припаркован. Срок клиента не говорит, что сервер закончил обработку, поэтому
//! проверки журнала, парковки и состояния после отказа клиента ждут квитанцию.

pub mod github;
pub mod gitlab;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

use crate::transport::{Method, Response};

/// Входящий запрос в виде, удобном провайдеру. Имена заголовков в нижнем регистре.
#[derive(Clone, Debug)]
pub struct Incoming {
    pub method: Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Адрес сервера `http://127.0.0.1:<порт>` для ссылок пагинации.
    pub base: String,
}

impl Incoming {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Токен из `Authorization: Bearer <токен>` или `token <токен>`.
    pub fn bearer(&self) -> Option<&str> {
        let value = self.header("authorization")?;
        let (scheme, token) = value.split_once(' ')?;
        (scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("token"))
            .then_some(token.trim())
    }

    pub fn json(&self) -> Result<Value, String> {
        if self.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&self.body).map_err(|err| format!("тело не JSON: {err}"))
    }
}

/// Провайдер за сервером.
pub trait Forge: Send + 'static {
    /// Ключ вызова для счётчиков и правил отказа: `МЕТОД /шаблон/{пути}` или `graphql <поле>`.
    fn key(&self, request: &Incoming) -> String;
    /// Логин вызывающего по токену, если токен известен.
    fn user(&self, request: &Incoming) -> Option<String>;
    fn handle(&mut self, request: &Incoming) -> Response;
    /// Явный отказ «лимит исчерпан» в форме провайдера.
    fn rate_limited(&self, request: &Incoming) -> Response;
}

/// Что видит клиент при отказе.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// Ответ с этим кодом и телом-описанием.
    Status(u16),
    /// Ответа нет: запрос паркуется, клиент отказывает по своему сроку.
    Silence,
    /// Отказ по лимиту в форме провайдера ([`Forge::rate_limited`]).
    RateLimited,
}

impl fmt::Display for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reply::Status(code) => write!(f, "status {code}"),
            Reply::Silence => f.write_str("silence"),
            Reply::RateLimited => f.write_str("rate_limited"),
        }
    }
}

/// Одноразовое правило отказа на `skip + 1`-й вызов с ключом `key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    pub key: String,
    pub skip: usize,
    pub applied: bool,
    pub reply: Reply,
}

impl Fault {
    /// Запись не применяется, клиент видит `reply`.
    pub fn new(key: impl Into<String>, reply: Reply) -> Fault {
        Fault {
            key: key.into(),
            skip: 0,
            applied: false,
            reply,
        }
    }

    /// Запись применяется, ответ провайдера заменяется на `reply`: «принято, ответ потерян».
    pub fn after_apply(mut self) -> Fault {
        self.applied = true;
        self
    }

    /// Пропустить `count` вызовов с этим ключом.
    pub fn skip(mut self, count: usize) -> Fault {
        self.skip = count;
        self
    }
}

/// Вызов в журнале сервера. Журнал живёт в памяти: на диск состояние идёт через очиститель.
#[derive(Clone, Debug, PartialEq)]
pub struct Logged {
    pub seq: u64,
    pub key: String,
    pub user: Option<String>,
    pub method: Method,
    pub path: String,
    pub body: Value,
    /// Отправленный код; `None` — молчание.
    pub status: Option<u16>,
    pub fault: Option<Reply>,
    /// Обработчик провайдера исполнен (без правила отказа или с [`Fault::after_apply`]).
    pub applied: bool,
}

struct Shared<F> {
    forge: F,
    faults: Vec<Fault>,
    log: Vec<Logged>,
    parked: Vec<tiny_http::Request>,
}

/// Состояние под замком и квитанция о записи вызова в журнал.
struct Guarded<F> {
    state: Mutex<Shared<F>>,
    logged: Condvar,
}

impl<F> Shared<F> {
    fn take_fault(&mut self, key: &str) -> Option<Fault> {
        let index = self.faults.iter().position(|fault| fault.key == key)?;
        let fault = &mut self.faults[index];
        if fault.skip > 0 {
            fault.skip -= 1;
            return None;
        }
        Some(self.faults.remove(index))
    }
}

pub struct FakeServer<F: Forge> {
    base: String,
    shared: Arc<Guarded<F>>,
    server: Arc<tiny_http::Server>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

fn lock<F>(shared: &Guarded<F>) -> MutexGuard<'_, Shared<F>> {
    shared.state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<F: Forge> FakeServer<F> {
    /// Сервер на `127.0.0.1` со свободным портом.
    pub fn start(forge: F) -> Result<FakeServer<F>, String> {
        let server = tiny_http::Server::http("127.0.0.1:0")
            .map_err(|err| format!("forge-fake: bind: {err}"))?;
        let address = server
            .server_addr()
            .to_ip()
            .ok_or("forge-fake: адрес не IP")?;
        let base = format!("http://{address}");
        let server = Arc::new(server);
        let shared = Arc::new(Guarded {
            state: Mutex::new(Shared {
                forge,
                faults: Vec::new(),
                log: Vec::new(),
                parked: Vec::new(),
            }),
            logged: Condvar::new(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (server, shared, stop, base) =
                (server.clone(), shared.clone(), stop.clone(), base.clone());
            std::thread::Builder::new()
                .name("forge-fake".to_owned())
                .spawn(move || serve(&server, &shared, &stop, &base))
                .map_err(|err| format!("forge-fake: поток: {err}"))?
        };
        Ok(FakeServer {
            base,
            shared,
            server,
            stop,
            worker: Some(worker),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Доступ к состоянию провайдера в обход HTTP: управление сценарием и оракулы.
    pub fn with<R>(&self, f: impl FnOnce(&mut F) -> R) -> R {
        f(&mut lock(&self.shared).forge)
    }

    pub fn inject(&self, fault: Fault) {
        lock(&self.shared).faults.push(fault);
    }

    pub fn log(&self) -> Vec<Logged> {
        lock(&self.shared).log.clone()
    }

    /// Число вызовов по ключу, включая отказавшие.
    pub fn counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for call in &lock(&self.shared).log {
            *counts.entry(call.key.clone()).or_insert(0) += 1;
        }
        counts
    }

    pub fn count(&self, key: &str) -> usize {
        lock(&self.shared)
            .log
            .iter()
            .filter(|call| call.key == key)
            .count()
    }

    pub fn parked(&self) -> usize {
        lock(&self.shared).parked.len()
    }

    /// Квитанция: ждёт, пока в журнале будет не меньше `count` вызовов, но не дольше `timeout`.
    /// Вызов попадает в журнал после обработчика провайдера и парковки молчащего запроса.
    pub fn wait_calls(&self, count: usize, timeout: Duration) -> Result<(), String> {
        let guard = lock(&self.shared);
        let (guard, _) = self
            .shared
            .logged
            .wait_timeout_while(guard, timeout, |shared| shared.log.len() < count)
            .unwrap_or_else(PoisonError::into_inner);
        let logged = guard.log.len();
        if logged >= count {
            Ok(())
        } else {
            Err(format!(
                "forge-fake: за {timeout:?} записано вызовов {logged} из {count}"
            ))
        }
    }

    /// Отпустить запросы без ответа. `tiny_http` ответит на них 500 уже ушедшему клиенту.
    pub fn release_parked(&self) {
        let parked = std::mem::take(&mut lock(&self.shared).parked);
        drop(parked);
    }

    /// Все правила отказов исполнены.
    pub fn finish(&self) -> Result<(), String> {
        let shared = lock(&self.shared);
        if shared.faults.is_empty() {
            return Ok(());
        }
        Err(shared
            .faults
            .iter()
            .map(|fault| {
                format!(
                    "правило отказа не исполнено: {} ({})",
                    fault.key, fault.reply
                )
            })
            .collect::<Vec<_>>()
            .join("; "))
    }
}

impl<F: Forge> Drop for FakeServer<F> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.server.unblock();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.release_parked();
    }
}

enum Outcome {
    Respond(Response),
    Park,
}

fn serve<F: Forge>(server: &tiny_http::Server, shared: &Guarded<F>, stop: &AtomicBool, base: &str) {
    loop {
        let mut request = match server.recv() {
            Ok(request) => request,
            Err(_) if stop.load(Ordering::SeqCst) => return,
            Err(_) => continue,
        };
        let incoming = match read_incoming(&mut request, base) {
            Ok(incoming) => incoming,
            Err(status) => {
                let _ = request.respond(tiny_http::Response::empty(status));
                continue;
            }
        };
        let mut guard = lock(shared);
        let key = guard.forge.key(&incoming);
        let user = guard.forge.user(&incoming);
        let fault = guard.take_fault(&key);
        let applied = fault.as_ref().is_none_or(|fault| fault.applied);
        let handled = applied.then(|| guard.forge.handle(&incoming));
        let outcome = match (&fault, handled) {
            (None, Some(response)) => Outcome::Respond(response),
            (Some(fault), _) => match fault.reply {
                Reply::Status(code) => Outcome::Respond(Response::json(
                    code,
                    &json!({"message": format!("forge-fake: injected {code}")}),
                )),
                Reply::RateLimited => Outcome::Respond(guard.forge.rate_limited(&incoming)),
                Reply::Silence => Outcome::Park,
            },
            (None, None) => Outcome::Respond(Response::new(500)),
        };
        let seq = guard.log.len() as u64 + 1;
        guard.log.push(Logged {
            seq,
            key,
            user,
            method: incoming.method,
            path: incoming.path.clone(),
            body: incoming.json().unwrap_or(Value::Null),
            status: match &outcome {
                Outcome::Respond(response) => Some(response.status),
                Outcome::Park => None,
            },
            fault: fault.as_ref().map(|fault| fault.reply),
            applied,
        });
        match outcome {
            Outcome::Respond(response) => {
                drop(guard);
                shared.logged.notify_all();
                let _ = request.respond(to_tiny(response));
            }
            Outcome::Park => {
                guard.parked.push(request);
                drop(guard);
                shared.logged.notify_all();
            }
        }
    }
}

fn read_incoming(request: &mut tiny_http::Request, base: &str) -> Result<Incoming, u16> {
    let method = match request.method() {
        tiny_http::Method::Get => Method::Get,
        tiny_http::Method::Post => Method::Post,
        tiny_http::Method::Put => Method::Put,
        tiny_http::Method::Patch => Method::Patch,
        tiny_http::Method::Delete => Method::Delete,
        _ => return Err(405),
    };
    let (path, query) = match request.url().split_once('?') {
        Some((path, query)) => (path.to_owned(), parse_query(query)),
        None => (request.url().to_owned(), Vec::new()),
    };
    let headers = request
        .headers()
        .iter()
        .map(|header| {
            (
                header.field.as_str().as_str().to_ascii_lowercase(),
                header.value.as_str().to_owned(),
            )
        })
        .collect();
    let mut body = Vec::new();
    request
        .as_reader()
        .read_to_end(&mut body)
        .map_err(|_| 400u16)?;
    Ok(Incoming {
        method,
        path,
        query,
        headers,
        body,
        base: base.to_owned(),
    })
}

fn to_tiny(response: Response) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let mut out = tiny_http::Response::from_data(response.body).with_status_code(response.status);
    for (name, value) in &response.headers {
        if let Ok(header) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            out = out.with_header(header);
        }
    }
    out
}

pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Корневое поле операции GraphQL и вид операции. Сервер не разбирает GraphQL целиком:
/// аргументы берутся из `variables`, ответ имеет полную фиксированную форму.
pub fn graphql_root(query: &str) -> Option<(GraphqlKind, String)> {
    let text: String = query
        .lines()
        .map(|line| line.split_once('#').map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n");
    let open = text.find('{')?;
    let header = text[..open].trim_start();
    let kind = if header.starts_with("mutation") {
        GraphqlKind::Mutation
    } else if header.is_empty() || header.starts_with("query") {
        GraphqlKind::Query
    } else {
        return None;
    };
    let rest = &text[open + 1..];
    let first = take_name(rest)?;
    let after = rest.trim_start()[first.len()..].trim_start();
    let name = match after.strip_prefix(':') {
        Some(aliased) => take_name(aliased)?,
        None => first,
    };
    Some((kind, name.to_owned()))
}

fn take_name(text: &str) -> Option<&str> {
    let text = text.trim_start();
    let end = text
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map_or(text.len(), |(index, _)| index);
    let name = &text[..end];
    (!name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit())).then_some(name)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphqlKind {
    Query,
    Mutation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphql_root_reads_kind_name_and_alias() {
        assert_eq!(
            graphql_root(
                "# комментарий { не поле\nmutation Add($input: AddPullRequestReviewThreadInput!) {\n  \
                 addPullRequestReviewThread(input: $input) { thread { id } }\n}"
            ),
            Some((
                GraphqlKind::Mutation,
                "addPullRequestReviewThread".to_owned()
            ))
        );
        assert_eq!(
            graphql_root("query($id: ID!) { n: node(id: $id) { id } }"),
            Some((GraphqlKind::Query, "node".to_owned()))
        );
        assert_eq!(
            graphql_root("{ viewer { login } }"),
            Some((GraphqlKind::Query, "viewer".to_owned()))
        );
        assert_eq!(graphql_root("subscription { x }"), None);
        assert_eq!(graphql_root("no braces"), None);
    }

    #[test]
    fn query_is_percent_decoded() {
        assert_eq!(
            parse_query("per_page=100&page=2&q=a%2Fb+c&flag&bad=%zz"),
            vec![
                ("per_page".to_owned(), "100".to_owned()),
                ("page".to_owned(), "2".to_owned()),
                ("q".to_owned(), "a/b c".to_owned()),
                ("flag".to_owned(), String::new()),
                ("bad".to_owned(), "%zz".to_owned()),
            ]
        );
    }
}
