//! Адаптер GitHub по ADR-0015: две ветви отправки, существующий `PENDING`, просмотренные файлы.
//!
//! Адаптер строит запросы и разбирает ответы; журнал он не трогает. Каждый метод записи — ровно
//! один запрос, итог его ошибки для журнала задаёт [`ApiError::write_fate`]: отказ сервера и отказ
//! по лимиту значат, что запись не принята, а отказ транспорта после отправки, 5xx и успешный
//! ответ без ожидаемых полей — что итог неизвестен и нужна сверка. Чтения идут отдельными
//! методами, их ошибки к итогу записи не относятся.
//!
//! - Только локальные черновики: один REST Create review с `commit_id` ревизии сессии, `event`,
//!   верхнеуровневым `body` и `comments[]`.
//! - Серверный `PENDING`: REST Create review без `event`, комментарии — GraphQL
//!   `addPullRequestReviewThread` по node ID review и `updatePullRequestReviewComment` по node ID
//!   комментария, Submit — REST `…/reviews/{id}/events` без `comments[]`.
//! - Запись обеих ветвей требует [`NoPending`] — итога отдельного чтения
//!   [`GitHub::check_no_pending`]. Существующий `PENDING` того же аккаунта это чтение возвращает
//!   целиком ошибкой [`ApiError::PendingExists`]; в работу он идёт только через
//!   [`ExistingPending::adopt`]. Submit принятого `PENDING` отправляет весь его состав.
//!
//! Страницы списков адаптер строит сам (`per_page=100&page=N`), заголовок `Link` только говорит,
//! есть ли следующая: транспорт подставляет токен в любой адрес, ходить по адресу из ответа нельзя.

pub mod capabilities;

use std::fmt;

use serde_json::{Map, Value, json};

use crate::model::diff::FileDiff;
use crate::model::revision::{Revision, Sha};
use crate::model::{DraftComment, ModelError, Provider, ProviderId, ReviewSession, ReviewTarget};
use crate::transport::{Failure, Method, Request, Response, Transport, percent_encode};

pub use crate::transport::WriteFate;
pub use capabilities::{ApiVersion, Capabilities};

/// Версия REST API в заголовке `X-GitHub-Api-Version`; без заголовка GitHub берёт её же.
pub const API_VERSION: &str = "2022-11-28";
const ACCEPT: &str = "application/vnd.github+json";
const PER_PAGE: usize = 100;
/// Предел страниц одного списка: дальше — ошибка, а не бесконечный обход.
const MAX_PAGES: usize = 100;

const ADD_THREAD: &str = "mutation($input: AddPullRequestReviewThreadInput!) { \
addPullRequestReviewThread(input: $input) { thread { id comments(first: 1) { nodes { \
id fullDatabaseId body pullRequestReview { id } } } } } }";
const UPDATE_COMMENT: &str = "mutation($input: UpdatePullRequestReviewCommentInput!) { \
updatePullRequestReviewComment(input: $input) { pullRequestReviewComment { \
id fullDatabaseId body pullRequestReview { id } } } }";
const MARK_VIEWED: &str = "mutation($input: MarkFileAsViewedInput!) { \
markFileAsViewed(input: $input) { pullRequest { id } } }";
const UNMARK_VIEWED: &str = "mutation($input: UnmarkFileAsViewedInput!) { \
unmarkFileAsViewed(input: $input) { pullRequest { id } } }";
const RESOLVE_THREAD: &str = "mutation($input: ResolveReviewThreadInput!) { \
resolveReviewThread(input: $input) { thread { id isResolved } } }";
const UNRESOLVE_THREAD: &str = "mutation($input: UnresolveReviewThreadInput!) { \
unresolveReviewThread(input: $input) { thread { id isResolved } } }";
const VIEWED_FILES: &str = "query($id: ID!, $cursor: String) { node(id: $id) { \
... on PullRequest { files(first: 100, after: $cursor) { nodes { path viewerViewedState } \
pageInfo { hasNextPage endCursor } } } } }";
/// Треды PR: node ID, `isResolved` и корень (первый комментарий) — его node ID для соединения с
/// REST `node_id` и `fullDatabaseId` (`databaseId` комментария устарел).
const REVIEW_THREADS: &str = "query($id: ID!, $cursor: String) { node(id: $id) { \
... on PullRequest { reviewThreads(first: 100, after: $cursor) { nodes { id isResolved \
comments(first: 1) { totalCount nodes { id fullDatabaseId } } } \
pageInfo { hasNextPage endCursor } } } } }";

/// Ошибки GraphQL, после которых мутация точно не исполнялась: разбор, проверка схемы и
/// переменных, отказ по объекту или правам. Остальные (в том числе без типа) — итог неизвестен.
const DEFINITIVE_GRAPHQL: [&str; 12] = [
    "NOT_FOUND",
    "FORBIDDEN",
    "UNPROCESSABLE",
    "INSUFFICIENT_SCOPES",
    "PARSE",
    "VARIABLE",
    "undefinedField",
    "undefinedType",
    "argumentNotAccepted",
    "missingRequiredArguments",
    "variableMismatch",
    "argumentLiteralsIncompatible",
];

/// Адреса API. REST и GraphQL задаются явно: у github.com это `https://api.github.com` и
/// `…/graphql`, у GHES `https://<хост>/api/v3` и `https://<хост>/api/graphql`, у `forge-fake`
/// оба на одном адресе. Адаптер не выводит их из имени хоста.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    pub rest: String,
    pub graphql: String,
}

impl Endpoints {
    pub fn cloud() -> Endpoints {
        Endpoints::new("https://api.github.com", "https://api.github.com/graphql")
    }

    pub fn new(rest: impl Into<String>, graphql: impl Into<String>) -> Endpoints {
        Endpoints {
            rest: rest.into().trim_end_matches('/').to_owned(),
            graphql: graphql.into(),
        }
    }

    /// REST и GraphQL (`/graphql`) на одном адресе, как у `forge-fake`.
    pub fn single(base: &str) -> Endpoints {
        let base = base.trim_end_matches('/');
        Endpoints::new(base, format!("{base}/graphql"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphqlError {
    /// `type` или `extensions.code` ошибки.
    pub kind: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// Вход неверен, запрос не построен и не отправлен.
    Invalid(String),
    /// Модель отказала (устаревший якорь, другая ревизия, ответ вместо якоря): запросов не было.
    Model(ModelError),
    /// Возможности нет на хосте по чтению возможностей: запросов записи не было.
    Absent(&'static str),
    /// У аккаунта уже есть `PENDING`: он не перезаписывается, запросов записи не было.
    PendingExists(Box<ExistingPending>),
    /// Запрос не ушёл.
    NotSent(Failure),
    /// Ответа нет или он неполон, запрос мог дойти.
    Transport(Failure),
    /// Явный отказ по лимиту: 403 или 429 REST, ошибка GraphQL при исчерпанном лимите.
    RateLimited { status: u16, detail: String },
    /// Ответ HTTP не 2xx.
    Status {
        status: u16,
        message: String,
        errors: Vec<String>,
    },
    /// HTTP 200 с `errors` GraphQL.
    Graphql(Vec<GraphqlError>),
    /// Успешный ответ не той формы или не с тем итогом.
    Unexpected(String),
    /// Ревизия PR сменилась между чтениями одного снимка.
    HeadMoved {
        before: Box<Revision>,
        after: Box<Revision>,
    },
}

impl ApiError {
    pub fn write_fate(&self) -> WriteFate {
        match self {
            ApiError::Invalid(_)
            | ApiError::Model(_)
            | ApiError::Absent(_)
            | ApiError::PendingExists(_)
            | ApiError::NotSent(_)
            | ApiError::RateLimited { .. }
            | ApiError::HeadMoved { .. } => WriteFate::NotAccepted,
            ApiError::Status { status, .. } if (400..500).contains(status) => {
                WriteFate::NotAccepted
            }
            ApiError::Graphql(errors)
                if !errors.is_empty()
                    && errors.iter().all(|error| {
                        error
                            .kind
                            .as_deref()
                            .is_some_and(|kind| DEFINITIVE_GRAPHQL.contains(&kind))
                    }) =>
            {
                WriteFate::NotAccepted
            }
            ApiError::Status { .. }
            | ApiError::Graphql(_)
            | ApiError::Transport(_)
            | ApiError::Unexpected(_) => WriteFate::Unknown,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Invalid(reason) => write!(f, "запрос не построен: {reason}"),
            ApiError::Model(err) => write!(f, "модель: {err}"),
            ApiError::Absent(what) => write!(f, "возможности нет на хосте: {what}"),
            ApiError::PendingExists(existing) => write!(
                f,
                "у аккаунта уже есть PENDING {} с {} комментариями",
                existing.pending.id.as_str(),
                existing.comments.len()
            ),
            ApiError::NotSent(failure) => write!(f, "запрос не ушёл: {failure}"),
            ApiError::Transport(failure) => write!(f, "итог неизвестен: {failure}"),
            ApiError::RateLimited { status, detail } => {
                write!(f, "лимит исчерпан (код {status}): {detail}")
            }
            ApiError::Status {
                status,
                message,
                errors,
            } => {
                write!(f, "код {status}: {message}")?;
                if !errors.is_empty() {
                    write!(f, " ({})", errors.join("; "))?;
                }
                Ok(())
            }
            ApiError::Graphql(errors) => {
                let parts: Vec<String> = errors
                    .iter()
                    .map(|error| match &error.kind {
                        Some(kind) => format!("{kind}: {}", error.message),
                        None => error.message.clone(),
                    })
                    .collect();
                write!(f, "ошибки GraphQL: {}", parts.join("; "))
            }
            ApiError::Unexpected(reason) => write!(f, "неожиданный ответ: {reason}"),
            ApiError::HeadMoved { before, after } => write!(
                f,
                "ревизия PR сменилась во время чтения: head {} → {}",
                before.head_sha(),
                after.head_sha()
            ),
        }
    }
}

impl std::error::Error for ApiError {}

impl From<ModelError> for ApiError {
    fn from(err: ModelError) -> ApiError {
        ApiError::Model(err)
    }
}

/// Итог отправленного review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    pub fn name(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        }
    }

    /// Состояние review после отправки с этим итогом.
    pub fn state(self) -> ReviewState {
        match self {
            ReviewEvent::Comment => ReviewState::Commented,
            ReviewEvent::Approve => ReviewState::Approved,
            ReviewEvent::RequestChanges => ReviewState::ChangesRequested,
        }
    }

    /// Верхнеуровневый `body` обязателен для `COMMENT` и `REQUEST_CHANGES`.
    fn check_body(self, body: &str) -> Result<(), ApiError> {
        match self {
            ReviewEvent::Comment | ReviewEvent::RequestChanges if body.trim().is_empty() => Err(
                ApiError::Invalid(format!("для {} нужен непустой body", self.name())),
            ),
            ReviewEvent::Comment | ReviewEvent::RequestChanges | ReviewEvent::Approve => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewState {
    Pending,
    Commented,
    Approved,
    ChangesRequested,
    Dismissed,
}

impl ReviewState {
    pub fn name(self) -> &'static str {
        match self {
            ReviewState::Pending => "PENDING",
            ReviewState::Commented => "COMMENTED",
            ReviewState::Approved => "APPROVED",
            ReviewState::ChangesRequested => "CHANGES_REQUESTED",
            ReviewState::Dismissed => "DISMISSED",
        }
    }

    pub fn parse(name: &str) -> Option<ReviewState> {
        [
            ReviewState::Pending,
            ReviewState::Commented,
            ReviewState::Approved,
            ReviewState::ChangesRequested,
            ReviewState::Dismissed,
        ]
        .into_iter()
        .find(|state| state.name() == name)
    }
}

/// Review на сервере: числовой ID для REST и node ID для GraphQL из одного ответа.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteReview {
    pub id: ProviderId,
    pub node_id: ProviderId,
    pub user: String,
    pub state: ReviewState,
    pub body: String,
    pub commit_id: String,
}

/// Комментарий review из REST.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteComment {
    pub id: ProviderId,
    pub node_id: ProviderId,
    pub review_id: Option<ProviderId>,
    pub user: String,
    pub body: String,
    pub path: String,
    pub line: Option<u32>,
    pub side: Option<String>,
    pub start_line: Option<u32>,
    pub start_side: Option<String>,
    pub commit_id: String,
    pub in_reply_to: Option<ProviderId>,
}

/// `PENDING`, в который адаптер добавляет комментарии и который отправляет. Получается только
/// созданием ([`GitHub::start_pending`]) или явным принятием существующего
/// ([`ExistingPending::adopt`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReview {
    id: ProviderId,
    node_id: ProviderId,
    commit_id: Sha,
    /// Комментарии другого клиента на момент принятия; у созданного адаптером — пусто.
    foreign: Vec<ProviderId>,
    adopted: bool,
}

impl PendingReview {
    /// Числовой ID для REST (Submit, чтение).
    pub fn id(&self) -> &ProviderId {
        &self.id
    }

    /// Node ID для GraphQL (`pullRequestReviewId`).
    pub fn node_id(&self) -> &ProviderId {
        &self.node_id
    }

    /// Commit, к которому привязаны треды review: GraphQL-тред его наследует.
    pub fn commit_id(&self) -> &Sha {
        &self.commit_id
    }

    pub fn is_adopted(&self) -> bool {
        self.adopted
    }

    /// Комментарии другого клиента, которые уйдут вместе с review при Submit.
    pub fn foreign(&self) -> &[ProviderId] {
        &self.foreign
    }
}

/// `PENDING` того же аккаунта, созданный другим клиентом, прочитанный целиком по всем страницам.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExistingPending {
    pending: PendingReview,
    pub body: String,
    pub comments: Vec<RemoteComment>,
}

impl ExistingPending {
    pub fn id(&self) -> &ProviderId {
        &self.pending.id
    }

    pub fn node_id(&self) -> &ProviderId {
        &self.pending.node_id
    }

    pub fn commit_id(&self) -> &Sha {
        &self.pending.commit_id
    }

    /// Явное принятие в работу. Submit принятого `PENDING` отправит весь состав, в том числе
    /// комментарии другого клиента: выборочной отправки части `PENDING` адаптер не даёт, а
    /// изменить чужой комментарий нельзя — [`GitHub::update_pending_comment`] принимает только
    /// комментарии, добавленные через адаптер.
    pub fn adopt(self) -> PendingReview {
        self.pending
    }
}

/// Комментарий, добавленный адаптером в `PENDING`: node ID (для изменения), числовой ID, тред.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingComment {
    pub id: ProviderId,
    /// Числовой ID: REST `id` или GraphQL `fullDatabaseId` (`databaseId` устарел). Поле GraphQL
    /// допускает `null`, а записи адаптера числовой ID не нужен, поэтому его отсутствие в ответе
    /// 2xx не делает итог записи неизвестным.
    pub database_id: Option<ProviderId>,
    /// Тред из ответа `addPullRequestReviewThread`; у восстановленного из журнала его нет: REST
    /// комментария тред не отдаёт.
    pub thread_id: Option<ProviderId>,
    pub body: String,
    review: ProviderId,
}

impl PendingComment {
    /// Node ID review, в котором лежит комментарий.
    pub fn review(&self) -> &ProviderId {
        &self.review
    }
}

/// Итог чтения [`GitHub::check_no_pending`]: у аккаунта не было `PENDING`. Без него методы
/// создания review не вызываются, поэтому существующий `PENDING` нельзя обойти, а метод записи
/// остаётся одним запросом.
#[derive(Debug, PartialEq, Eq)]
pub struct NoPending {
    _checked: (),
}

/// Снимок PR: ревизия и diff одного head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullSnapshot {
    pub node_id: ProviderId,
    pub author: String,
    /// GitHub не отдаёт тройку SHA: `base.sha` идёт в `base_sha` и `start_sha`, `head.sha` — в
    /// `head_sha`, все из одного ответа `GET …/pulls/{n}`.
    pub revision: Revision,
    /// Файлы с `patch`; двоичные и слишком большие файлы без `patch` якорей не принимают.
    pub files: Vec<FileDiff>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewedState {
    Viewed,
    Unviewed,
    Dismissed,
}

impl ViewedState {
    pub fn parse(name: &str) -> Option<ViewedState> {
        match name {
            "VIEWED" => Some(ViewedState::Viewed),
            "UNVIEWED" => Some(ViewedState::Unviewed),
            "DISMISSED" => Some(ViewedState::Dismissed),
            _ => None,
        }
    }
}

/// Тред review PR из GraphQL `reviewThreads`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteThread {
    pub node_id: ProviderId,
    pub is_resolved: bool,
    /// Node ID корня треда: равен REST `node_id` его комментария.
    pub root: Option<ProviderId>,
    /// `fullDatabaseId` корня, если хост его вернул: числовой ID REST.
    pub root_database_id: Option<String>,
    /// `comments.totalCount`: корень и ответы.
    pub comments: u64,
}

/// Адаптер одного PR.
pub struct GitHub<T> {
    transport: T,
    endpoints: Endpoints,
    owner: String,
    repo: String,
    number: u64,
}

impl<T: Transport> GitHub<T> {
    pub fn new(
        transport: T,
        endpoints: Endpoints,
        target: &ReviewTarget,
    ) -> Result<GitHub<T>, ApiError> {
        if target.provider != Provider::GitHub {
            return Err(ApiError::Invalid(format!(
                "цель {} для адаптера GitHub",
                target.provider.name()
            )));
        }
        let Some((owner, repo)) = target.repo.split_once('/') else {
            return Err(ApiError::Invalid(
                "путь репозитория GitHub — владелец/имя".to_owned(),
            ));
        };
        if owner.is_empty() || repo.is_empty() || repo.contains('/') {
            return Err(ApiError::Invalid(
                "путь репозитория GitHub — владелец/имя".to_owned(),
            ));
        }
        Ok(GitHub {
            transport,
            endpoints,
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            number: target.number,
        })
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    fn repo_path(&self) -> String {
        format!(
            "/repos/{}/{}",
            percent_encode(&self.owner),
            percent_encode(&self.repo)
        )
    }

    fn pull_path(&self) -> String {
        format!("{}/pulls/{}", self.repo_path(), self.number)
    }

    fn rest(&self, method: Method, path: &str) -> Request {
        Request::new(method, format!("{}{path}", self.endpoints.rest))
            .header("accept", ACCEPT)
            .header("x-github-api-version", API_VERSION)
    }

    fn call(&self, request: &Request) -> Result<Response, ApiError> {
        let response = self.transport.send(request).map_err(|failure| {
            if failure.may_have_been_sent() {
                ApiError::Transport(failure)
            } else {
                ApiError::NotSent(failure)
            }
        })?;
        if response.is_success() {
            Ok(response)
        } else {
            Err(rest_error(&response))
        }
    }

    fn get_json(&self, path: &str) -> Result<(Value, Response), ApiError> {
        let response = self.call(&self.rest(Method::Get, path))?;
        let value = json_of(&response)?;
        Ok((value, response))
    }

    fn write_json(&self, method: Method, path: &str, body: &Value) -> Result<Value, ApiError> {
        let response = self.call(&self.rest(method, path).json(body))?;
        json_of(&response)
    }

    /// Все страницы списка REST.
    fn pages(&self, path: &str) -> Result<Vec<Value>, ApiError> {
        let mut items = Vec::new();
        for page in 1..=MAX_PAGES {
            let request = self.rest(
                Method::Get,
                &format!("{path}?per_page={PER_PAGE}&page={page}"),
            );
            let response = self.call(&request)?;
            let Value::Array(chunk) = json_of(&response)? else {
                return Err(ApiError::Unexpected(format!(
                    "страница {page} списка не массив"
                )));
            };
            let empty = chunk.is_empty();
            items.extend(chunk);
            if !has_next(&response) {
                return Ok(items);
            }
            if empty {
                return Err(ApiError::Unexpected(format!(
                    "пустая страница {page} со ссылкой next"
                )));
            }
        }
        Err(ApiError::Unexpected(format!(
            "в списке больше {MAX_PAGES} страниц"
        )))
    }

    /// Запрос GraphQL: аргументы только в `variables`. Возвращает `data.<field>`.
    fn graphql(&self, query: &str, variables: Value, field: &str) -> Result<Value, ApiError> {
        let request = Request::new(Method::Post, self.endpoints.graphql.clone())
            .header("accept", ACCEPT)
            .json(&json!({"query": query, "variables": variables}));
        let response = self.call(&request)?;
        let body = json_of(&response)?;
        if let Some(errors) = body
            .get("errors")
            .and_then(Value::as_array)
            .filter(|errors| !errors.is_empty())
        {
            let errors: Vec<GraphqlError> = errors.iter().map(graphql_error).collect();
            // `x-ratelimit-remaining: 0` бывает и у запроса, который потратил последнее очко:
            // без явного признака лимита его ошибка остаётся обычной ошибкой GraphQL.
            let exhausted = response.header("x-ratelimit-remaining") == Some("0");
            let explicit = errors.iter().any(|error| {
                error.kind.as_deref() == Some("RATE_LIMITED")
                    || (exhausted && error.message.to_ascii_lowercase().contains("rate limit"))
            });
            if explicit {
                let detail = errors
                    .iter()
                    .map(|error| error.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(ApiError::RateLimited {
                    status: response.status,
                    detail,
                });
            }
            return Err(ApiError::Graphql(errors));
        }
        body.get("data")
            .and_then(|data| data.get(field))
            .filter(|value| !value.is_null())
            .cloned()
            .ok_or_else(|| ApiError::Unexpected(format!("в ответе GraphQL нет data.{field}")))
    }

    /// Ревизия PR и diff её head. Ревизия читается до и после списка файлов: сменилась —
    /// [`ApiError::HeadMoved`], файлы другого head с ревизией не смешиваются.
    pub fn pull(&self) -> Result<PullSnapshot, ApiError> {
        let (before, _) = self.get_json(&self.pull_path())?;
        let files = self.pages(&format!("{}/files", self.pull_path()))?;
        let (after, _) = self.get_json(&self.pull_path())?;
        let revision = revision_of(&before)?;
        let again = revision_of(&after)?;
        if revision != again {
            return Err(ApiError::HeadMoved {
                before: Box::new(revision),
                after: Box::new(again),
            });
        }
        let mut diffs = Vec::new();
        for file in &files {
            let new_path = str_field(file, "filename")?;
            let Some(patch) = file.get("patch").and_then(Value::as_str) else {
                continue;
            };
            let old_path = file
                .get("previous_filename")
                .and_then(Value::as_str)
                .unwrap_or(new_path.as_str());
            diffs.push(FileDiff::parse(old_path, &new_path, patch)?);
        }
        Ok(PullSnapshot {
            node_id: ProviderId::new(str_field(&before, "node_id")?),
            author: before
                .pointer("/user/login")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            revision,
            files: diffs,
        })
    }

    /// Все reviews PR, видимые токену (`PENDING` видит только его владелец).
    pub fn reviews(&self) -> Result<Vec<RemoteReview>, ApiError> {
        self.pages(&format!("{}/reviews", self.pull_path()))?
            .iter()
            .map(review_of)
            .collect()
    }

    /// Review по числовому ID: восстанавливает пару ID (числовой и node) из одного.
    pub fn review(&self, id: &ProviderId) -> Result<RemoteReview, ApiError> {
        let (value, _) = self.get_json(&format!(
            "{}/reviews/{}",
            self.pull_path(),
            percent_encode(id.as_str())
        ))?;
        review_of(&value)
    }

    /// Все комментарии review по всем страницам.
    pub fn review_comments(&self, id: &ProviderId) -> Result<Vec<RemoteComment>, ApiError> {
        self.pages(&format!(
            "{}/reviews/{}/comments",
            self.pull_path(),
            percent_encode(id.as_str())
        ))?
        .iter()
        .map(comment_of)
        .collect()
    }

    /// `PENDING` аккаунта `viewer` с полным составом или `None`.
    pub fn find_pending(&self, viewer: &str) -> Result<Option<ExistingPending>, ApiError> {
        let mut pending = self.reviews()?.into_iter().filter(|review| {
            review.state == ReviewState::Pending && review.user.eq_ignore_ascii_case(viewer)
        });
        let Some(review) = pending.next() else {
            return Ok(None);
        };
        if pending.next().is_some() {
            return Err(ApiError::Unexpected(
                "у аккаунта больше одного PENDING в PR".to_owned(),
            ));
        }
        let comments = self.review_comments(&review.id)?;
        let commit_id = parse_sha(&review.commit_id)?;
        Ok(Some(ExistingPending {
            pending: PendingReview {
                id: review.id,
                node_id: review.node_id,
                commit_id,
                foreign: comments.iter().map(|comment| comment.id.clone()).collect(),
                adopted: true,
            },
            body: review.body,
            comments,
        }))
    }

    /// Свой `PENDING` после переоткрытия журнала: review читается по записанному числовому ID и
    /// должен быть `PENDING` аккаунта сессии на head ревизии сессии. Комментарии с node ID из
    /// журнала (`own`) — свои и снова изменяемы, остальные — чужие ([`PendingReview::foreign`],
    /// числовые ID). `adopted` — `PENDING` был принят, а не создан отправкой. В отличие от
    /// [`GitHub::find_pending`] свои комментарии чужими не становятся.
    pub fn resume_pending(
        &self,
        session: &ReviewSession,
        review_id: &ProviderId,
        own: &[ProviderId],
        adopted: bool,
    ) -> Result<(PendingReview, Vec<PendingComment>), ApiError> {
        let review = self.review(review_id)?;
        expect_state(&review, ReviewState::Pending)?;
        if !review.user.eq_ignore_ascii_case(session.account()) {
            return Err(ApiError::Unexpected(format!(
                "PENDING {} принадлежит другому аккаунту",
                review_id.as_str()
            )));
        }
        let head = session.revision().head_sha();
        expect_commit(&review, head)?;
        let comments = self.review_comments(review_id)?;
        let mut restored = Vec::with_capacity(own.len());
        for id in own {
            let comment = comments
                .iter()
                .find(|comment| &comment.node_id == id)
                .ok_or_else(|| {
                    ApiError::Unexpected(format!(
                        "свой комментарий {} пропал из PENDING",
                        id.as_str()
                    ))
                })?;
            restored.push(PendingComment {
                id: comment.node_id.clone(),
                database_id: Some(comment.id.clone()),
                thread_id: None,
                body: comment.body.clone(),
                review: review.node_id.clone(),
            });
        }
        let foreign = comments
            .iter()
            .filter(|comment| !own.contains(&comment.node_id))
            .map(|comment| comment.id.clone())
            .collect();
        Ok((
            PendingReview {
                id: review.id,
                node_id: review.node_id,
                commit_id: head.clone(),
                foreign,
                adopted,
            },
            restored,
        ))
    }

    /// Чтение перед записью обеих ветвей: `PENDING` аккаунта нет — [`NoPending`], есть —
    /// [`ApiError::PendingExists`] с полным составом. Существующий `PENDING` не перезаписывается.
    pub fn check_no_pending(&self, caps: &Capabilities) -> Result<NoPending, ApiError> {
        match self.find_pending(&caps.login)? {
            Some(existing) => Err(ApiError::PendingExists(Box::new(existing))),
            None => Ok(NoPending { _checked: () }),
        }
    }

    /// Ветвь «только локальные черновики»: один запрос Create review с `comments[]` и `event`.
    /// Черновики без якоря (ответ в треде), устаревшие и другой ревизии отвергаются до запроса.
    pub fn create_review(
        &self,
        session: &ReviewSession,
        _clear: &NoPending,
        drafts: &[DraftComment],
        event: ReviewEvent,
        body: &str,
    ) -> Result<RemoteReview, ApiError> {
        event.check_body(body)?;
        let review = self.post_review(session, drafts, Some(event), body)?;
        expect_state(&review, event.state())?;
        Ok(review)
    }

    /// Запись второго клиента пробы, а не ветвь отправки: `PENDING` с `comments[]` одним запросом
    /// Create review без `event`, как черновик веб-интерфейса. Продуктовая отправка так не пишет:
    /// `PENDING` ветви отправки создаёт [`GitHub::start_pending`] без комментариев.
    pub fn create_pending_with_comments(
        &self,
        session: &ReviewSession,
        drafts: &[DraftComment],
        body: &str,
    ) -> Result<RemoteReview, ApiError> {
        let review = self.post_review(session, drafts, None, body)?;
        expect_state(&review, ReviewState::Pending)?;
        Ok(review)
    }

    fn post_review(
        &self,
        session: &ReviewSession,
        drafts: &[DraftComment],
        event: Option<ReviewEvent>,
        body: &str,
    ) -> Result<RemoteReview, ApiError> {
        let head = session.revision().head_sha();
        let mut comments = Vec::with_capacity(drafts.len());
        for draft in drafts {
            let anchor = session.github_anchor(draft)?;
            if anchor.commit_id() != head {
                return Err(ApiError::Invalid(
                    "commit якоря не равен head ревизии сессии".to_owned(),
                ));
            }
            let mut comment = Map::new();
            comment.insert("path".to_owned(), json!(anchor.path()));
            comment.insert("line".to_owned(), json!(anchor.line()));
            comment.insert("side".to_owned(), json!(anchor.side()));
            if let Some(start_line) = anchor.start_line() {
                comment.insert("start_line".to_owned(), json!(start_line));
            }
            if let Some(start_side) = anchor.start_side() {
                comment.insert("start_side".to_owned(), json!(start_side));
            }
            comment.insert("body".to_owned(), json!(draft.body));
            comments.push(Value::Object(comment));
        }
        let mut request = Map::new();
        request.insert("commit_id".to_owned(), json!(head.as_str()));
        if !body.is_empty() {
            request.insert("body".to_owned(), json!(body));
        }
        if let Some(event) = event {
            request.insert("event".to_owned(), json!(event.name()));
        }
        if !comments.is_empty() {
            request.insert("comments".to_owned(), Value::Array(comments));
        }
        let value = self.write_json(
            Method::Post,
            &format!("{}/reviews", self.pull_path()),
            &Value::Object(request),
        )?;
        let review = review_of(&value)?;
        expect_commit(&review, head)?;
        Ok(review)
    }

    /// Ветвь «серверный `PENDING`»: один запрос Create review без `event` с `commit_id` ревизии
    /// сессии. Существующий `PENDING` аккаунта отсекает [`GitHub::check_no_pending`].
    pub fn start_pending(
        &self,
        session: &ReviewSession,
        caps: &Capabilities,
        _clear: &NoPending,
    ) -> Result<PendingReview, ApiError> {
        if !caps.server_pending() {
            return Err(ApiError::Absent(
                "addPullRequestReviewThread и updatePullRequestReviewComment",
            ));
        }
        let head = session.revision().head_sha();
        let value = self.write_json(
            Method::Post,
            &format!("{}/reviews", self.pull_path()),
            &json!({"commit_id": head.as_str()}),
        )?;
        let review = review_of(&value)?;
        expect_state(&review, ReviewState::Pending)?;
        expect_commit(&review, head)?;
        Ok(PendingReview {
            id: review.id,
            node_id: review.node_id,
            commit_id: head.clone(),
            foreign: Vec::new(),
            adopted: false,
        })
    }

    /// Новый тред в `PENDING` через `addPullRequestReviewThread`. Тред наследует commit review,
    /// поэтому review на другом commit, чем head ревизии сессии, отвергается до запроса.
    pub fn add_pending_comment(
        &self,
        session: &ReviewSession,
        pending: &PendingReview,
        draft: &DraftComment,
    ) -> Result<PendingComment, ApiError> {
        let anchor = session.github_anchor(draft)?;
        if &pending.commit_id != session.revision().head_sha() {
            return Err(ApiError::Invalid(format!(
                "PENDING на commit {}, ревизия сессии на {}",
                pending.commit_id,
                session.revision().head_sha()
            )));
        }
        let mut input = Map::new();
        input.insert(
            "pullRequestReviewId".to_owned(),
            json!(pending.node_id.as_str()),
        );
        input.insert("path".to_owned(), json!(anchor.path()));
        input.insert("line".to_owned(), json!(anchor.line()));
        input.insert("side".to_owned(), json!(anchor.side()));
        if let Some(start_line) = anchor.start_line() {
            input.insert("startLine".to_owned(), json!(start_line));
        }
        if let Some(start_side) = anchor.start_side() {
            input.insert("startSide".to_owned(), json!(start_side));
        }
        input.insert("body".to_owned(), json!(draft.body));
        let value = self.graphql(
            ADD_THREAD,
            json!({"input": Value::Object(input)}),
            "addPullRequestReviewThread",
        )?;
        let thread = value
            .get("thread")
            .filter(|thread| !thread.is_null())
            .ok_or_else(|| ApiError::Unexpected("addPullRequestReviewThread без thread".into()))?;
        let thread_id = str_field(thread, "id")?;
        let comment = thread
            .pointer("/comments/nodes/0")
            .ok_or_else(|| ApiError::Unexpected("тред без комментария".to_owned()))?;
        let comment = pending_comment_of(comment, Some(thread_id))?;
        if comment.review != pending.node_id {
            return Err(ApiError::Unexpected(format!(
                "комментарий попал в review {}, ожидался {}",
                comment.review.as_str(),
                pending.node_id.as_str()
            )));
        }
        Ok(comment)
    }

    /// Новый текст комментария, добавленного адаптером, через `updatePullRequestReviewComment`.
    /// ID комментария сохраняется: ответ с другим ID — неожиданный.
    pub fn update_pending_comment(
        &self,
        pending: &PendingReview,
        comment: &PendingComment,
        body: &str,
    ) -> Result<PendingComment, ApiError> {
        if comment.review != pending.node_id {
            return Err(ApiError::Invalid(
                "комментарий не из этого PENDING".to_owned(),
            ));
        }
        if body.trim().is_empty() {
            return Err(ApiError::Invalid("пустой текст комментария".to_owned()));
        }
        let value = self.graphql(
            UPDATE_COMMENT,
            json!({"input": {
                "pullRequestReviewCommentId": comment.id.as_str(),
                "body": body,
            }}),
            "updatePullRequestReviewComment",
        )?;
        let updated = value
            .get("pullRequestReviewComment")
            .filter(|value| !value.is_null())
            .ok_or_else(|| {
                ApiError::Unexpected("updatePullRequestReviewComment без комментария".into())
            })?;
        let mut updated = pending_comment_of(updated, None)?;
        updated.thread_id = comment.thread_id.clone();
        let numeric_changed = matches!(
            (&updated.database_id, &comment.database_id),
            (Some(new), Some(old)) if new != old
        );
        if updated.id != comment.id || numeric_changed {
            return Err(ApiError::Unexpected(format!(
                "ID комментария сменился: {} → {}",
                comment.id.as_str(),
                updated.id.as_str()
            )));
        }
        if updated.review != comment.review {
            return Err(ApiError::Unexpected(
                "комментарий оказался в другом review".to_owned(),
            ));
        }
        Ok(updated)
    }

    /// Submit именно этого `PENDING`: `…/reviews/{id}/events` с `event` и `body`, без
    /// `comments[]`. Отправляется весь состав review.
    pub fn submit_pending(
        &self,
        pending: &PendingReview,
        event: ReviewEvent,
        body: &str,
    ) -> Result<RemoteReview, ApiError> {
        event.check_body(body)?;
        let mut request = Map::new();
        request.insert("event".to_owned(), json!(event.name()));
        if !body.is_empty() {
            request.insert("body".to_owned(), json!(body));
        }
        let value = self.write_json(
            Method::Post,
            &format!(
                "{}/reviews/{}/events",
                self.pull_path(),
                percent_encode(pending.id.as_str())
            ),
            &Value::Object(request),
        )?;
        let review = review_of(&value)?;
        if review.id != pending.id {
            return Err(ApiError::Unexpected(format!(
                "отправлен review {}, ожидался {}",
                review.id.as_str(),
                pending.id.as_str()
            )));
        }
        expect_state(&review, event.state())?;
        Ok(review)
    }

    /// Ответ в треде к опубликованному комментарию: REST `…/comments/{id}/replies` публикует его
    /// сразу отдельным review, в `PENDING` и в Create review он не входит.
    pub fn reply(&self, root: &ProviderId, body: &str) -> Result<RemoteComment, ApiError> {
        if body.trim().is_empty() {
            return Err(ApiError::Invalid("пустой текст ответа".to_owned()));
        }
        let value = self.write_json(
            Method::Post,
            &format!(
                "{}/comments/{}/replies",
                self.pull_path(),
                percent_encode(root.as_str())
            ),
            &json!({"body": body}),
        )?;
        let comment = comment_of(&value)?;
        if comment.in_reply_to.as_ref() != Some(root) {
            return Err(ApiError::Unexpected(format!(
                "ответ привязан к {:?}, ожидался {}",
                comment.in_reply_to.as_ref().map(ProviderId::as_str),
                root.as_str()
            )));
        }
        Ok(comment)
    }

    /// `resolveReviewThread` или `unresolveReviewThread` по node ID треда.
    pub fn set_thread_resolved(&self, thread: &ProviderId, resolved: bool) -> Result<(), ApiError> {
        let (query, field) = if resolved {
            (RESOLVE_THREAD, "resolveReviewThread")
        } else {
            (UNRESOLVE_THREAD, "unresolveReviewThread")
        };
        let value = self.graphql(
            query,
            json!({"input": {"threadId": thread.as_str()}}),
            field,
        )?;
        let observed = value.pointer("/thread/isResolved").and_then(Value::as_bool);
        if observed != Some(resolved) {
            return Err(ApiError::Unexpected(format!(
                "{field}: isResolved {observed:?}"
            )));
        }
        Ok(())
    }

    /// `markFileAsViewed` или `unmarkFileAsViewed` для файла PR по node ID PR.
    pub fn set_viewed(
        &self,
        caps: &Capabilities,
        pull: &ProviderId,
        path: &str,
        viewed: bool,
    ) -> Result<(), ApiError> {
        if !caps.viewed() {
            return Err(ApiError::Absent("markFileAsViewed и unmarkFileAsViewed"));
        }
        let (query, field) = if viewed {
            (MARK_VIEWED, "markFileAsViewed")
        } else {
            (UNMARK_VIEWED, "unmarkFileAsViewed")
        };
        let value = self.graphql(
            query,
            json!({"input": {"pullRequestId": pull.as_str(), "path": path}}),
            field,
        )?;
        let echoed = value.pointer("/pullRequest/id").and_then(Value::as_str);
        if echoed != Some(pull.as_str()) {
            return Err(ApiError::Unexpected(format!(
                "{field} вернул PR {echoed:?}"
            )));
        }
        Ok(())
    }

    /// Состояние просмотра файлов PR для токена (`viewerViewedState`), по курсорам.
    pub fn viewed_files(&self, pull: &ProviderId) -> Result<Vec<(String, ViewedState)>, ApiError> {
        let mut files = Vec::new();
        let mut cursor = Value::Null;
        for _ in 0..MAX_PAGES {
            let node = self.graphql(
                VIEWED_FILES,
                json!({"id": pull.as_str(), "cursor": cursor}),
                "node",
            )?;
            let connection = node
                .get("files")
                .ok_or_else(|| ApiError::Unexpected("у PR нет files".to_owned()))?;
            let nodes = connection
                .get("nodes")
                .and_then(Value::as_array)
                .ok_or_else(|| ApiError::Unexpected("files без nodes".to_owned()))?;
            for file in nodes {
                let path = str_field(file, "path")?;
                let state = str_field(file, "viewerViewedState")?;
                let state = ViewedState::parse(&state).ok_or_else(|| {
                    ApiError::Unexpected(format!("неизвестное viewerViewedState {state}"))
                })?;
                files.push((path, state));
            }
            let next = connection
                .pointer("/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !next {
                return Ok(files);
            }
            cursor = connection
                .pointer("/pageInfo/endCursor")
                .filter(|cursor| cursor.is_string())
                .cloned()
                .ok_or_else(|| ApiError::Unexpected("hasNextPage без endCursor".to_owned()))?;
        }
        Err(ApiError::Unexpected(format!(
            "в files больше {MAX_PAGES} страниц"
        )))
    }

    /// Треды review PR, видимые токену (GraphQL `reviewThreads`), по курсорам.
    pub fn review_threads(&self, pull: &ProviderId) -> Result<Vec<RemoteThread>, ApiError> {
        let mut threads = Vec::new();
        let mut cursor = Value::Null;
        for _ in 0..MAX_PAGES {
            let node = self.graphql(
                REVIEW_THREADS,
                json!({"id": pull.as_str(), "cursor": cursor}),
                "node",
            )?;
            let connection = node
                .get("reviewThreads")
                .ok_or_else(|| ApiError::Unexpected("у PR нет reviewThreads".to_owned()))?;
            let nodes = connection
                .get("nodes")
                .and_then(Value::as_array)
                .ok_or_else(|| ApiError::Unexpected("reviewThreads без nodes".to_owned()))?;
            for thread in nodes {
                threads.push(thread_of(thread)?);
            }
            let next = connection
                .pointer("/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !next {
                return Ok(threads);
            }
            cursor = connection
                .pointer("/pageInfo/endCursor")
                .filter(|cursor| cursor.is_string())
                .cloned()
                .ok_or_else(|| ApiError::Unexpected("hasNextPage без endCursor".to_owned()))?;
        }
        Err(ApiError::Unexpected(format!(
            "в reviewThreads больше {MAX_PAGES} страниц"
        )))
    }
}

/// Явный отказ по лимиту REST: 429, или 403 при `x-ratelimit-remaining: 0`, `retry-after` или
/// сообщении о лимите (вторичный лимит).
pub fn is_rate_limited(response: &Response, message: &str) -> bool {
    match response.status {
        429 => true,
        403 => {
            response.header("x-ratelimit-remaining") == Some("0")
                || response.header("retry-after").is_some()
                || message.to_ascii_lowercase().contains("rate limit")
        }
        _ => false,
    }
}

fn rest_error(response: &Response) -> ApiError {
    let body = response.body_json().unwrap_or(Value::Null);
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if is_rate_limited(response, &message) {
        return ApiError::RateLimited {
            status: response.status,
            detail: message,
        };
    }
    let errors = body
        .get("errors")
        .and_then(Value::as_array)
        .map(|errors| {
            errors
                .iter()
                .map(|error| match error {
                    Value::String(text) => text.clone(),
                    other => other
                        .get("message")
                        .or_else(|| other.get("code"))
                        .and_then(Value::as_str)
                        .map_or_else(|| other.to_string(), str::to_owned),
                })
                .collect()
        })
        .unwrap_or_default();
    ApiError::Status {
        status: response.status,
        message,
        errors,
    }
}

fn json_of(response: &Response) -> Result<Value, ApiError> {
    response.body_json().map_err(ApiError::Unexpected)
}

fn has_next(response: &Response) -> bool {
    response.header("link").is_some_and(|link| {
        link.split(',')
            .any(|part| part.split(';').skip(1).any(|p| p.trim() == "rel=\"next\""))
    })
}

fn graphql_error(value: &Value) -> GraphqlError {
    GraphqlError {
        kind: value
            .get("type")
            .or_else(|| value.pointer("/extensions/code"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| value.to_string(), str::to_owned),
    }
}

fn str_field(value: &Value, name: &str) -> Result<String, ApiError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ApiError::Unexpected(format!("нет строки {name}")))
}

/// ID провайдера из строки или числа JSON.
fn id_field(value: &Value, name: &str) -> Result<ProviderId, ApiError> {
    match value.get(name) {
        Some(Value::Number(number)) => Ok(ProviderId::new(number.to_string())),
        Some(Value::String(text)) if !text.is_empty() => Ok(ProviderId::new(text.clone())),
        _ => Err(ApiError::Unexpected(format!("нет ID {name}"))),
    }
}

fn optional_id(value: &Value, name: &str) -> Option<ProviderId> {
    id_field(value, name).ok()
}

fn optional_line(value: &Value, name: &str) -> Option<u32> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|line| u32::try_from(line).ok())
}

fn parse_sha(value: &str) -> Result<Sha, ApiError> {
    Sha::parse(value).map_err(|err| ApiError::Unexpected(err.to_string()))
}

fn revision_of(pull: &Value) -> Result<Revision, ApiError> {
    let sha = |pointer: &str| {
        pull.pointer(pointer)
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::Unexpected(format!("у PR нет {pointer}")))
            .and_then(parse_sha)
    };
    let base = sha("/base/sha")?;
    let head = sha("/head/sha")?;
    Ok(Revision::new(base.clone(), base, head))
}

fn review_of(value: &Value) -> Result<RemoteReview, ApiError> {
    let state = str_field(value, "state")?;
    Ok(RemoteReview {
        id: id_field(value, "id")?,
        node_id: id_field(value, "node_id")?,
        user: value
            .pointer("/user/login")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        state: ReviewState::parse(&state)
            .ok_or_else(|| ApiError::Unexpected(format!("неизвестное состояние review {state}")))?,
        body: value
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        commit_id: value
            .get("commit_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

fn comment_of(value: &Value) -> Result<RemoteComment, ApiError> {
    let text = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    Ok(RemoteComment {
        id: id_field(value, "id")?,
        node_id: id_field(value, "node_id")?,
        review_id: optional_id(value, "pull_request_review_id"),
        user: value
            .pointer("/user/login")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        body: text("body").unwrap_or_default(),
        path: str_field(value, "path")?,
        line: optional_line(value, "line"),
        side: text("side"),
        start_line: optional_line(value, "start_line"),
        start_side: text("start_side"),
        commit_id: text("commit_id").unwrap_or_default(),
        in_reply_to: optional_id(value, "in_reply_to_id"),
    })
}

fn thread_of(value: &Value) -> Result<RemoteThread, ApiError> {
    let comments = value
        .get("comments")
        .ok_or_else(|| ApiError::Unexpected("тред без comments".to_owned()))?;
    let root = comments
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::Unexpected("comments треда без nodes".to_owned()))?
        .first();
    Ok(RemoteThread {
        node_id: id_field(value, "id")?,
        is_resolved: value
            .get("isResolved")
            .and_then(Value::as_bool)
            .ok_or_else(|| ApiError::Unexpected("тред без isResolved".to_owned()))?,
        root: root.map(|root| id_field(root, "id")).transpose()?,
        root_database_id: root
            .and_then(|root| optional_id(root, "fullDatabaseId"))
            .map(|id| id.as_str().to_owned()),
        comments: comments
            .get("totalCount")
            .and_then(Value::as_u64)
            .ok_or_else(|| ApiError::Unexpected("comments треда без totalCount".to_owned()))?,
    })
}

fn pending_comment_of(
    value: &Value,
    thread_id: Option<String>,
) -> Result<PendingComment, ApiError> {
    Ok(PendingComment {
        id: id_field(value, "id")?,
        database_id: optional_id(value, "fullDatabaseId"),
        thread_id: thread_id.map(ProviderId::new),
        body: str_field(value, "body")?,
        review: value
            .get("pullRequestReview")
            .ok_or_else(|| ApiError::Unexpected("комментарий без pullRequestReview".to_owned()))
            .and_then(|review| id_field(review, "id"))?,
    })
}

fn expect_state(review: &RemoteReview, state: ReviewState) -> Result<(), ApiError> {
    if review.state == state {
        Ok(())
    } else {
        Err(ApiError::Unexpected(format!(
            "review {} в состоянии {}, ожидалось {}",
            review.id.as_str(),
            review.state.name(),
            state.name()
        )))
    }
}

fn expect_commit(review: &RemoteReview, head: &Sha) -> Result<(), ApiError> {
    if review.commit_id.eq_ignore_ascii_case(head.as_str()) {
        Ok(())
    } else {
        Err(ApiError::Unexpected(format!(
            "review {} на commit {}, ожидался {head}",
            review.id.as_str(),
            review.commit_id
        )))
    }
}

#[cfg(test)]
mod tests;
