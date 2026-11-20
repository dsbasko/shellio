//! HTTP-транспорт на `ureq` с TLS платформы.
//!
//! `ureq` подключён с `native-tls-no-default`: корни задаются явно (`RootCerts::PlatformVerifier`),
//! иначе значение по умолчанию `WebPki` паникует при первом TLS-соединении. Коды 4xx и 5xx
//! возвращаются ответом с телом и заголовками, перенаправления не выполняются, прокси из
//! окружения не берётся, `ureq` запросы не повторяет.
//!
//! Записи идут отдельным агентом без пула соединений: устаревшее соединение из пула оборвалось
//! бы после отправки и дало ложный `unknown`. Чтения пул используют.

use std::fmt;
use std::io;
use std::time::Duration;

use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use super::{Failure, FailureKind, Method, Request, Response, Transport};

const USER_AGENT: &str = "shellio-probe-review";
/// Предел тела ответа: страница списка по 100 элементов укладывается с запасом.
const BODY_LIMIT: u64 = 64 * 1024 * 1024;

/// Секрет с `Debug` без значения.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Secret {
        Secret(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthScheme {
    /// `Authorization: Bearer <token>` (GitHub, OAuth GitLab).
    Bearer,
    /// `PRIVATE-TOKEN: <token>` (личный токен GitLab).
    PrivateToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    pub scheme: AuthScheme,
    pub secret: Secret,
}

impl Credential {
    pub fn bearer(secret: Secret) -> Credential {
        Credential {
            scheme: AuthScheme::Bearer,
            secret,
        }
    }

    pub fn private_token(secret: Secret) -> Credential {
        Credential {
            scheme: AuthScheme::PrivateToken,
            secret,
        }
    }

    fn header(&self) -> (&'static str, String) {
        match self.scheme {
            AuthScheme::Bearer => ("authorization", format!("Bearer {}", self.secret.expose())),
            AuthScheme::PrivateToken => ("private-token", self.secret.expose().to_owned()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpConfig {
    /// Срок разрешения имени, соединения и TLS: истёк — запрос не ушёл.
    pub connect_timeout: Duration,
    /// Общий срок вызова с телом ответа.
    pub call_timeout: Duration,
}

impl Default for HttpConfig {
    fn default() -> HttpConfig {
        HttpConfig {
            connect_timeout: Duration::from_secs(10),
            call_timeout: Duration::from_secs(30),
        }
    }
}

pub struct HttpTransport {
    reads: Agent,
    writes: Agent,
    credential: Option<Credential>,
}

impl HttpTransport {
    pub fn new(config: HttpConfig, credential: Option<Credential>) -> HttpTransport {
        HttpTransport {
            reads: agent(config, true),
            writes: agent(config, false),
            credential,
        }
    }
}

fn agent(config: HttpConfig, pooled: bool) -> Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    let builder = Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None)
        .user_agent(USER_AGENT)
        .timeout_connect(Some(config.connect_timeout))
        .timeout_global(Some(config.call_timeout));
    let builder = if pooled {
        builder
    } else {
        builder.max_idle_connections(0)
    };
    Agent::new_with_config(builder.build())
}

impl fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpTransport")
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

impl Transport for HttpTransport {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        let mut builder = ureq::http::Request::builder()
            .method(request.method.as_str())
            .uri(request.url.as_str());
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(credential) = &self.credential {
            let (name, value) = credential.header();
            builder = builder.header(name, value);
        }
        let agent = if request.method.is_write() {
            &self.writes
        } else {
            &self.reads
        };
        let sent = match (&request.body, request.method) {
            (None, Method::Get | Method::Delete) => builder
                .body(())
                .map_err(|_| Failure::not_sent("запрос не собран"))
                .and_then(|built| agent.run(built).map_err(classify)),
            // Пустое тело записи уходит с `Content-Length: 0`: GitHub отвечает 411 на PUT без
            // длины.
            (body, _) => builder
                .body(body.clone().unwrap_or_default())
                .map_err(|_| Failure::not_sent("запрос не собран"))
                .and_then(|built| agent.run(built).map_err(classify)),
        };
        let mut response = sent?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_vec()
            .map_err(|err| classify(err).with_status(status))?;
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

/// Отказ `ureq` → вид отказа. `NotSent` — только то, что точно случилось до отправки запроса;
/// остальное считается дошедшим. Описание не содержит адреса и тела.
fn classify(err: ureq::Error) -> Failure {
    use ureq::Error as E;
    use ureq::Timeout as T;
    let (kind, detail) = match err {
        E::BadUri(_) => (FailureKind::NotSent, "неверный адрес".to_owned()),
        E::Http(_) => (FailureKind::NotSent, "запрос не собран".to_owned()),
        E::HostNotFound => (FailureKind::NotSent, "хост не найден".to_owned()),
        E::InvalidProxyUrl => (FailureKind::NotSent, "неверный прокси".to_owned()),
        E::ConnectionFailed => (FailureKind::NotSent, "соединение не установлено".to_owned()),
        E::Timeout(reason @ (T::Resolve | T::Connect)) => {
            (FailureKind::NotSent, format!("таймаут {reason:?}"))
        }
        E::Timeout(reason) => (FailureKind::Timeout, format!("таймаут {reason:?}")),
        E::Io(io) if io.kind() == io::ErrorKind::ConnectionRefused => {
            (FailureKind::NotSent, "соединение отклонено".to_owned())
        }
        E::Io(io) => (
            FailureKind::Disconnected,
            format!("ввод-вывод {:?}", io.kind()),
        ),
        // Ошибка TLS приходит только из рукопожатия: после него ошибки потока — `Io`.
        E::NativeTls(tls) => (FailureKind::NotSent, format!("TLS: {tls}")),
        E::Der(_) | E::Tls(_) => (FailureKind::NotSent, "TLS: конфигурация".to_owned()),
        E::Protocol(protocol) => (FailureKind::Malformed, format!("протокол: {protocol}")),
        E::StatusCode(code) => (FailureKind::Malformed, format!("код {code} как ошибка")),
        other => (FailureKind::Disconnected, other.to_string()),
    };
    Failure::new(kind, detail)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    use serde_json::json;

    use super::*;

    /// Запрос, как его прочитал сервер.
    #[derive(Debug)]
    struct Received {
        line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Received {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    fn read_request(stream: &TcpStream) -> Received {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("строка запроса");
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("заголовок");
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let (name, value) = header.split_once(':').expect("заголовок с двоеточием");
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
        let length: usize = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map_or(0, |(_, value)| value.parse().expect("длина"));
        let mut body = vec![0; length];
        reader.read_exact(&mut body).expect("тело");
        Received {
            line: line.trim_end().to_owned(),
            headers,
            body,
        }
    }

    /// Сервер одного соединения: читает запрос, отдаёт его в канал и выполняет `then`.
    fn serve_once(
        then: impl FnOnce(TcpStream) + Send + 'static,
    ) -> (String, mpsc::Receiver<Received>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("адрес"));
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let received = read_request(&stream);
            tx.send(received).expect("канал");
            then(stream);
        });
        (base, rx, handle)
    }

    fn quick() -> HttpConfig {
        HttpConfig {
            connect_timeout: Duration::from_secs(5),
            call_timeout: Duration::from_millis(300),
        }
    }

    #[test]
    fn response_keeps_status_body_and_headers_and_sends_credential() {
        let (base, rx, handle) = serve_once(|mut stream| {
            let body = r#"{"id":7}"#;
            write!(
                stream,
                "HTTP/1.1 422 Unprocessable Entity\r\ncontent-type: application/json\r\n\
                 x-ratelimit-remaining: 4999\r\nlink: <next>; rel=\"next\"\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("ответ");
        });
        let transport = HttpTransport::new(
            quick(),
            Some(Credential::bearer(Secret::new("ghp_testtoken"))),
        );
        let request = Request::new(Method::Post, format!("{base}/repos/o/r/pulls/1/reviews"))
            .header("accept", "application/vnd.github+json")
            .json(&json!({"event": "COMMENT"}));
        let response = transport.send(&request).expect("ответ");
        let received = rx.recv().expect("запрос");
        handle.join().expect("сервер");

        assert_eq!(response.status, 422);
        assert_eq!(response.body_json(), Ok(json!({"id": 7})));
        assert_eq!(response.header("x-ratelimit-remaining"), Some("4999"));
        assert_eq!(response.header("link"), Some("<next>; rel=\"next\""));
        assert_eq!(received.line, "POST /repos/o/r/pulls/1/reviews HTTP/1.1");
        assert_eq!(
            received.header("authorization"),
            Some("Bearer ghp_testtoken")
        );
        assert_eq!(received.header("user-agent"), Some(USER_AGENT));
        assert_eq!(received.header("content-type"), Some("application/json"));
        assert_eq!(received.body, br#"{"event":"COMMENT"}"#);
    }

    #[test]
    fn redirect_is_returned_not_followed() {
        let (base, rx, handle) = serve_once(|mut stream| {
            write!(
                stream,
                "HTTP/1.1 301 Moved Permanently\r\nlocation: /elsewhere\r\n\
                 content-length: 0\r\nconnection: close\r\n\r\n"
            )
            .expect("ответ");
        });
        let transport = HttpTransport::new(quick(), None);
        let response = transport
            .send(&Request::get(format!("{base}/repos/o/r")))
            .expect("ответ");
        rx.recv().expect("запрос");
        handle.join().expect("сервер");
        assert_eq!(response.status, 301);
        assert_eq!(response.header("location"), Some("/elsewhere"));
    }

    #[test]
    fn private_token_and_empty_write_body_with_length() {
        let (base, rx, handle) = serve_once(|mut stream| {
            write!(
                stream,
                "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n"
            )
            .expect("ответ");
        });
        let transport = HttpTransport::new(
            quick(),
            Some(Credential::private_token(Secret::new("glpat-test"))),
        );
        let request = Request::new(
            Method::Put,
            format!("{base}/api/v4/projects/1/merge_requests/2/draft_notes/3/publish"),
        );
        let response = transport.send(&request).expect("ответ");
        let received = rx.recv().expect("запрос");
        handle.join().expect("сервер");
        assert_eq!(response.status, 204);
        assert_eq!(received.header("private-token"), Some("glpat-test"));
        assert_eq!(received.header("authorization"), None);
        assert_eq!(received.header("content-length"), Some("0"));
    }

    #[test]
    fn timeout_after_request_was_received_may_have_been_sent() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (base, rx, handle) = serve_once(move |stream| {
            // Соединение держится без ответа, пока клиент не отказался по сроку.
            let _ = release_rx.recv();
            drop(stream);
        });
        let transport = HttpTransport::new(quick(), None);
        let request = Request::new(Method::Post, format!("{base}/x")).json(&json!({}));
        let failure = transport.send(&request).expect_err("таймаут");
        let received = rx.recv().expect("сервер получил запрос");
        release_tx.send(()).expect("канал");
        handle.join().expect("сервер");

        assert_eq!(received.line, "POST /x HTTP/1.1");
        assert_eq!(failure.kind, FailureKind::Timeout, "{failure}");
        assert!(failure.may_have_been_sent());
        assert_eq!(failure.status, None);
    }

    #[test]
    fn drop_after_request_is_disconnected() {
        let (base, rx, handle) = serve_once(drop);
        let transport = HttpTransport::new(quick(), None);
        let request = Request::new(Method::Post, format!("{base}/x")).json(&json!({"a": 1}));
        let failure = transport.send(&request).expect_err("обрыв");
        rx.recv().expect("сервер получил запрос");
        handle.join().expect("сервер");
        assert_eq!(failure.kind, FailureKind::Disconnected, "{failure}");
        assert!(failure.may_have_been_sent());
    }

    #[test]
    fn drop_during_body_keeps_status() {
        let (base, rx, handle) = serve_once(|mut stream| {
            write!(
                stream,
                "HTTP/1.1 201 Created\r\ncontent-length: 100\r\n\r\n{{\"id\""
            )
            .expect("ответ");
        });
        let transport = HttpTransport::new(quick(), None);
        let request = Request::new(Method::Post, format!("{base}/x")).json(&json!({}));
        let failure = transport.send(&request).expect_err("обрыв тела");
        rx.recv().expect("запрос");
        handle.join().expect("сервер");
        assert!(failure.may_have_been_sent(), "{failure}");
        assert_eq!(failure.status, Some(201));
    }

    #[test]
    fn garbage_response_is_malformed() {
        let (base, rx, handle) = serve_once(|mut stream| {
            stream.write_all(b"NOT HTTP AT ALL\r\n\r\n").expect("ответ");
        });
        let transport = HttpTransport::new(quick(), None);
        let failure = transport
            .send(&Request::get(format!("{base}/x")))
            .expect_err("не HTTP");
        rx.recv().expect("запрос");
        handle.join().expect("сервер");
        assert_eq!(failure.kind, FailureKind::Malformed, "{failure}");
        assert!(failure.may_have_been_sent());
    }

    #[test]
    fn refused_connection_was_not_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("адрес");
        drop(listener);
        let transport = HttpTransport::new(quick(), None);
        let request = Request::new(Method::Post, format!("http://{address}/x")).json(&json!({}));
        let failure = transport.send(&request).expect_err("отказ");
        assert_eq!(failure.kind, FailureKind::NotSent, "{failure}");
        assert!(!failure.may_have_been_sent());
    }

    #[test]
    fn bad_url_was_not_sent_and_detail_has_no_url() {
        let transport = HttpTransport::new(quick(), None);
        let failure = transport
            .send(&Request::get("owner/secret-repo"))
            .expect_err("адрес");
        assert_eq!(failure.kind, FailureKind::NotSent);
        assert!(!failure.detail.contains("secret-repo"), "{failure}");
    }

    #[test]
    fn https_uses_native_tls_with_platform_roots_without_panic() {
        // Сервер без TLS закрывает соединение: рукопожатие проваливается ошибкой, а не
        // паникой `ureq` из-за корней `WebPki` по умолчанию.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("адрес");
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            drop(stream);
        });
        let transport = HttpTransport::new(quick(), None);
        let outcome = transport.send(&Request::get(format!("https://{address}/x")));
        handle.join().expect("сервер");
        assert!(outcome.is_err(), "{outcome:?}");
    }

    #[test]
    fn debug_hides_secret() {
        let transport =
            HttpTransport::new(quick(), Some(Credential::bearer(Secret::new("ghp_hidden"))));
        assert!(!format!("{transport:?}").contains("ghp_hidden"));
    }
}
