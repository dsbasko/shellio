//! Сторож записи (план P0.7, «Живой контур»): транспорт-обёртка, через которую идут все
//! запросы живой клетки — жизненного цикла и адаптеров ревью.
//!
//! Чтения проходят сразу. Перед первой записью сторож читает репозиторий и проверяет:
//! - облачный хост — путь перечислен в `sandbox.toml`; локальный GitLab CE — адрес API на
//!   `127.0.0.1` (проверяется до запроса, иначе запросов нет вовсе);
//! - репозиторий приватный: только явные `private: true` у GitHub и `visibility: "private"` у
//!   GitLab, отсутствие поля — отказ;
//! - имя в ответе совпадает с проверяемым путём, есть ветка по умолчанию.
//!
//! Отказ запоминается: ни одна запись после него не уходит, транспорт отвечает отказом
//! `not_sent`, а [`Guarded::refusal`] даёт причину для ошибки исполнения. После проверки запись
//! REST допускается только под путём проверенного репозитория, GraphQL — на адрес GraphQL хоста:
//! мутацию по node ID другого репозитория сторож не различает.

use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::Value;

use crate::model::Provider;
use crate::transport::{Failure, Request, Response, Transport, percent_encode};
use crate::{github, gitlab};

use super::sandbox::Sandbox;
use crate::results::Column;

/// Что делает репозиторий пригодным для записи помимо приватности.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteScope {
    /// Облачный хост: репозитории, перечисленные у хоста столбца в `sandbox.toml`.
    Listed(Vec<String>),
    /// Локальный GitLab CE: адреса API на `127.0.0.1`.
    Loopback,
}

impl WriteScope {
    pub fn from_sandbox(sandbox: &Sandbox, column: Column) -> WriteScope {
        WriteScope::Listed(
            sandbox
                .host(column)
                .map(|host| vec![host.repo.clone()])
                .unwrap_or_default(),
        )
    }
}

/// Хост, адреса API и путь репозитория, которые проверяет сторож.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardTarget {
    pub provider: Provider,
    /// Корень REST без `/` в конце: `https://api.github.com`, `<корень>/api/v4`.
    pub rest: String,
    pub graphql: String,
    pub repo: String,
}

impl GuardTarget {
    pub fn github(endpoints: &github::Endpoints, repo: &str) -> GuardTarget {
        GuardTarget {
            provider: Provider::GitHub,
            rest: endpoints.rest.trim_end_matches('/').to_owned(),
            graphql: endpoints.graphql.clone(),
            repo: repo.to_owned(),
        }
    }

    pub fn gitlab(endpoints: &gitlab::Endpoints, repo: &str) -> GuardTarget {
        GuardTarget {
            provider: Provider::GitLab,
            rest: endpoints.rest.trim_end_matches('/').to_owned(),
            graphql: endpoints.graphql.clone(),
            repo: repo.to_owned(),
        }
    }

    /// Путь репозитория в адресе REST, как его строят адаптеры.
    pub fn repo_path(&self) -> String {
        match self.provider {
            Provider::GitHub => self
                .repo
                .split('/')
                .map(percent_encode)
                .collect::<Vec<_>>()
                .join("/"),
            Provider::GitLab => percent_encode(&self.repo),
        }
    }

    /// Адрес репозитория REST без `/` в конце.
    pub fn repo_url(&self) -> String {
        match self.provider {
            Provider::GitHub => format!("{}/repos/{}", self.rest, self.repo_path()),
            Provider::GitLab => format!("{}/projects/{}", self.rest, self.repo_path()),
        }
    }
}

/// Репозиторий по чтению сторожа.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInfo {
    pub default_branch: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardError {
    /// Путь не перечислен в `sandbox.toml` у хоста столбца. Запросов не было.
    NotListed,
    /// Адрес API локального GitLab CE не на `127.0.0.1`. Запросов не было.
    NotLoopback,
    /// Чтение репозитория не удалось: нет репозитория, нет доступа, отказ транспорта.
    Read(String),
    /// Репозиторий не приватный или приватность не подтверждена ответом.
    NotPrivate(String),
    /// Имя репозитория в ответе другое.
    Mismatch,
    /// У репозитория нет ветки по умолчанию.
    NoDefaultBranch,
}

impl fmt::Display for GuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("сторож записи: ")?;
        match self {
            GuardError::NotListed => f.write_str("репозиторий не перечислен в sandbox.toml"),
            GuardError::NotLoopback => f.write_str("адрес API локального GitLab не 127.0.0.1"),
            GuardError::Read(detail) => write!(f, "репозиторий не прочитан: {detail}"),
            GuardError::NotPrivate(seen) => write!(f, "репозиторий не приватный ({seen})"),
            GuardError::Mismatch => f.write_str("имя репозитория в ответе не совпадает"),
            GuardError::NoDefaultBranch => f.write_str("у репозитория нет ветки по умолчанию"),
        }
    }
}

impl std::error::Error for GuardError {}

#[derive(Clone, Debug)]
enum State {
    Unchecked,
    Open(RepoInfo),
    Refused(GuardError),
}

/// Транспорт со сторожем записи.
pub struct Guarded<T> {
    inner: T,
    target: GuardTarget,
    scope: WriteScope,
    state: Mutex<State>,
    refused: Mutex<Vec<String>>,
}

impl<T: Transport> Guarded<T> {
    pub fn new(inner: T, target: GuardTarget, scope: WriteScope) -> Guarded<T> {
        Guarded {
            inner,
            target,
            scope,
            state: Mutex::new(State::Unchecked),
            refused: Mutex::new(Vec::new()),
        }
    }

    pub fn target(&self) -> &GuardTarget {
        &self.target
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Проверка репозитория; повторный вызов возвращает запомненный итог без запросов.
    pub fn check(&self) -> Result<RepoInfo, GuardError> {
        let mut state = self.state();
        match &*state {
            State::Open(info) => return Ok(info.clone()),
            State::Refused(err) => return Err(err.clone()),
            State::Unchecked => {}
        }
        let verdict = self.inspect();
        *state = match &verdict {
            Ok(info) => State::Open(info.clone()),
            Err(err) => State::Refused(err.clone()),
        };
        verdict
    }

    /// Причина отказа, если проверка не прошла.
    pub fn refusal(&self) -> Option<GuardError> {
        match &*self.state() {
            State::Refused(err) => Some(err.clone()),
            State::Unchecked | State::Open(_) => None,
        }
    }

    /// Записи, которые сторож не пропустил: `МЕТОД путь`. Любая — ошибка исполнения сценария.
    pub fn refused_writes(&self) -> Vec<String> {
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn inspect(&self) -> Result<RepoInfo, GuardError> {
        match &self.scope {
            WriteScope::Listed(repos) => {
                if !repos
                    .iter()
                    .any(|repo| repo.eq_ignore_ascii_case(&self.target.repo))
                {
                    return Err(GuardError::NotListed);
                }
            }
            WriteScope::Loopback => {
                if !is_loopback(&self.target.rest) || !is_loopback(&self.target.graphql) {
                    return Err(GuardError::NotLoopback);
                }
            }
        }
        let mut request = Request::get(self.target.repo_url());
        request = match self.target.provider {
            Provider::GitHub => request
                .header("accept", "application/vnd.github+json")
                .header("x-github-api-version", github::API_VERSION),
            Provider::GitLab => request.header("accept", "application/json"),
        };
        let response = self
            .inner
            .send(&request)
            .map_err(|failure| GuardError::Read(failure.to_string()))?;
        if !response.is_success() {
            return Err(GuardError::Read(format!("код {}", response.status)));
        }
        self.verdict(&response)
    }

    fn verdict(&self, response: &Response) -> Result<RepoInfo, GuardError> {
        let body = response
            .body_json()
            .map_err(|_| GuardError::Read("ответ не JSON".to_owned()))?;
        let (name_field, private) = match self.target.provider {
            Provider::GitHub => (
                "full_name",
                match body.get("private") {
                    Some(Value::Bool(true)) => Ok(()),
                    Some(Value::Bool(false)) => Err("private: false".to_owned()),
                    _ => Err("private не подтверждён".to_owned()),
                },
            ),
            Provider::GitLab => (
                "path_with_namespace",
                match body.get("visibility").and_then(Value::as_str) {
                    Some("private") => Ok(()),
                    Some(other) => Err(format!("visibility: {other}")),
                    None => Err("visibility не подтверждена".to_owned()),
                },
            ),
        };
        private.map_err(GuardError::NotPrivate)?;
        let name = body.get(name_field).and_then(Value::as_str);
        if !name.is_some_and(|name| name.eq_ignore_ascii_case(&self.target.repo)) {
            return Err(GuardError::Mismatch);
        }
        let default_branch = body
            .get("default_branch")
            .and_then(Value::as_str)
            .filter(|branch| !branch.is_empty())
            .ok_or(GuardError::NoDefaultBranch)?;
        Ok(RepoInfo {
            default_branch: default_branch.to_owned(),
        })
    }

    /// Запись после проверки идёт только в проверенный репозиторий или на адрес GraphQL.
    fn allowed(&self, request: &Request) -> bool {
        let url = request.url.to_ascii_lowercase();
        let prefix = format!("{}/", self.target.repo_url()).to_ascii_lowercase();
        url.starts_with(&prefix) || request.url == self.target.graphql
    }
}

impl<T: Transport> Transport for Guarded<T> {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        if request.method.is_write() {
            let refusal = match self.check() {
                Ok(_) if self.allowed(request) => None,
                Ok(_) => Some("запись вне проверенного репозитория".to_owned()),
                Err(err) => Some(err.to_string()),
            };
            if let Some(reason) = refusal {
                self.refused
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(format!("{} {}", request.method, request.path()));
                return Err(Failure::not_sent(reason));
            }
        }
        self.inner.send(request)
    }
}

/// Адрес `http(s)://127.0.0.1[:порт][/…]`.
pub fn is_loopback(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    match authority.strip_prefix("127.0.0.1") {
        Some("") => true,
        Some(port) => port
            .strip_prefix(':')
            .is_some_and(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())),
        None => false,
    }
}
