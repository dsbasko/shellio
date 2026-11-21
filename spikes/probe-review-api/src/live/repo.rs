//! Запросы жизненного цикла клетки к репозиторию песочницы: ветки, коммиты через API, PR/MR,
//! черновики своего аккаунта для уборки. Реализации — [`super::github::GitHubRepo`] и
//! [`super::gitlab::GitLabRepo`]; обе ходят только через [`super::guard::Guarded`].
//!
//! Формы запросов взяты из документации REST GitHub (git refs, contents, pulls, reviews) и GitLab
//! (branches, commits, merge requests, draft notes); `forge-fake` их не реализует, поэтому до живой
//! клетки они проверены только встроенным транспортом.

use std::fmt;

use serde_json::Value;

use crate::transport::{Failure, Request, Response, Transport};

use super::guard::{GuardError, RepoInfo};

/// Аккаунт токена: логин, числовой ID (ID нужен назначению ревьюера MR) и отображаемое имя
/// профиля, если оно задано.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub login: String,
    pub id: u64,
    pub name: Option<String>,
}

impl Account {
    /// Логин, отображаемое имя и числовой ID — в очиститель под меткой роли до первой записи
    /// следа: имя профиля — те же персональные данные, что логин, а ID публично переводится в
    /// логин.
    pub fn register(&self, scrubber: &mut crate::scrub::Scrubber, label: &'static str) {
        scrubber
            .add_login(&self.login, label)
            .add_account_id(self.id, label);
        if let Some(name) = self.name.as_deref().filter(|name| !name.trim().is_empty()) {
            scrubber.add_login(name.trim(), label);
        }
    }
}

/// Отображаемое имя из ответа `GET /user`: `null` и пустая строка — имени нет.
pub(super) fn display_name(user: &Value) -> Option<String> {
    user.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// Изменение одного файла одним коммитом.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileChange<'a> {
    pub path: &'a str,
    pub content: &'a str,
    pub message: &'a str,
    /// Файла в ветке ещё нет.
    pub create: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveError {
    /// Сторож записи отказал: записей не было.
    Guard(GuardError),
    /// Запрос не ушёл.
    NotSent(Failure),
    /// Ответа нет или он неполон, запрос мог дойти.
    Transport(Failure),
    /// Ответ HTTP не 2xx.
    Status { status: u16, message: String },
    /// Успешный ответ не той формы.
    Unexpected(String),
}

impl LiveError {
    pub fn status(&self) -> Option<u16> {
        match self {
            LiveError::Status { status, .. } => Some(*status),
            LiveError::Guard(_)
            | LiveError::NotSent(_)
            | LiveError::Transport(_)
            | LiveError::Unexpected(_) => None,
        }
    }
}

impl fmt::Display for LiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiveError::Guard(err) => write!(f, "{err}"),
            LiveError::NotSent(failure) => write!(f, "запрос не ушёл: {failure}"),
            LiveError::Transport(failure) => write!(f, "итог неизвестен: {failure}"),
            LiveError::Status { status, message } => write!(f, "код {status}: {message}"),
            LiveError::Unexpected(reason) => write!(f, "неожиданный ответ: {reason}"),
        }
    }
}

impl std::error::Error for LiveError {}

impl From<GuardError> for LiveError {
    fn from(err: GuardError) -> LiveError {
        LiveError::Guard(err)
    }
}

/// Репозиторий песочницы от имени одной роли.
pub trait LiveRepo {
    /// «PR» или «MR» для описаний.
    fn pull_word(&self) -> &'static str;
    /// Сторож записи этой роли: чтение и проверка репозитория.
    fn check(&self) -> Result<RepoInfo, GuardError>;
    fn whoami(&self) -> Result<Account, LiveError>;
    /// SHA головы ветки; ветки нет — `None`.
    fn branch_head(&self, branch: &str) -> Result<Option<String>, LiveError>;
    fn create_branch(&self, name: &str, from_sha: &str) -> Result<(), LiveError>;
    /// Коммит одного файла в ветку; возвращает SHA коммита.
    fn commit_file(&self, branch: &str, change: &FileChange<'_>) -> Result<String, LiveError>;
    /// PR/MR из `head` в `base`; ревьюер назначается там, где хост это делает при создании.
    fn open_pull(
        &self,
        head: &str,
        base: &str,
        title: &str,
        reviewer: Option<&Account>,
    ) -> Result<u64, LiveError>;
    /// Открытые PR/MR с исходной веткой `head`.
    fn open_pulls_from(&self, head: &str) -> Result<Vec<u64>, LiveError>;
    fn pull_is_open(&self, number: u64) -> Result<bool, LiveError>;
    fn close_pull(&self, number: u64) -> Result<(), LiveError>;
    fn delete_branch(&self, name: &str) -> Result<(), LiveError>;
    /// Черновики этого токена в PR/MR: `PENDING` review у GitHub, `draft_notes` у GitLab.
    fn own_drafts(&self, number: u64) -> Result<Vec<String>, LiveError>;
    fn delete_draft(&self, number: u64, id: &str) -> Result<(), LiveError>;
}

/// Запрос с разбором отказа транспорта и кода.
pub(super) fn call<T: Transport>(transport: &T, request: &Request) -> Result<Response, LiveError> {
    let response = transport.send(request).map_err(|failure| {
        if failure.may_have_been_sent() {
            LiveError::Transport(failure)
        } else {
            LiveError::NotSent(failure)
        }
    })?;
    if response.is_success() {
        Ok(response)
    } else {
        let body = response.body_json().unwrap_or(Value::Null);
        let message = match body.get("message").or_else(|| body.get("error")) {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        Err(LiveError::Status {
            status: response.status,
            message,
        })
    }
}

pub(super) fn json<T: Transport>(transport: &T, request: &Request) -> Result<Value, LiveError> {
    let response = call(transport, request)?;
    response.body_json().map_err(LiveError::Unexpected)
}

/// Есть ли следующая страница: `x-next-page` GitLab или `Link: …; rel="next"`.
pub(super) fn has_next(response: &Response) -> bool {
    match response.header("x-next-page") {
        Some(next) => !next.trim().is_empty(),
        None => response.header("link").is_some_and(|link| {
            link.split(',')
                .any(|part| part.split(';').skip(1).any(|p| p.trim() == "rel=\"next\""))
        }),
    }
}

/// Предел страниц одного списка.
const MAX_PAGES: usize = 100;

/// Все страницы списка: адрес страницы строит вызывающий по номеру.
pub(super) fn pages<T: Transport>(
    transport: &T,
    request: impl Fn(usize) -> Request,
) -> Result<Vec<Value>, LiveError> {
    let mut items = Vec::new();
    for page in 1..=MAX_PAGES {
        let response = call(transport, &request(page))?;
        let Value::Array(chunk) = response.body_json().map_err(LiveError::Unexpected)? else {
            return Err(LiveError::Unexpected(format!(
                "страница {page} списка не массив"
            )));
        };
        let empty = chunk.is_empty();
        items.extend(chunk);
        if !has_next(&response) {
            return Ok(items);
        }
        if empty {
            return Err(LiveError::Unexpected(format!(
                "пустая страница {page} со ссылкой на следующую"
            )));
        }
    }
    Err(LiveError::Unexpected(format!(
        "в списке больше {MAX_PAGES} страниц"
    )))
}

pub(super) fn str_of(value: &Value, pointer: &str) -> Result<String, LiveError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| LiveError::Unexpected(format!("в ответе нет {pointer}")))
}

pub(super) fn u64_of(value: &Value, pointer: &str) -> Result<u64, LiveError> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .ok_or_else(|| LiveError::Unexpected(format!("в ответе нет числа {pointer}")))
}

/// ID в ответе: число или строка.
pub(super) fn id_of(value: &Value, pointer: &str) -> Result<String, LiveError> {
    match value.pointer(pointer) {
        Some(Value::Number(number)) => Ok(number.to_string()),
        Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
        _ => Err(LiveError::Unexpected(format!("в ответе нет ID {pointer}"))),
    }
}

/// Base64 (RFC 4648, с `=`) для Contents API GitHub.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (index, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}
