//! Адаптер GitLab по ADR-0015: черновики `draft_notes`, поштучная публикация известных
//! черновиков, сводка отдельной заметкой, `approve` с `sha` ревизии сессии и запрос изменений
//! через GraphQL `mergeRequestRequestChanges`.
//!
//! Адаптер строит запросы и разбирает ответы; журнал он не трогает. Итог ошибки записи для журнала
//! задаёт [`ApiError::write_fate`]. Метод записи — один запрос, кроме
//! [`GitLab::request_changes`]: его итог проверяется отдельным чтением состояния ревьюера после
//! мутации, и сбой этого чтения оставляет итог неизвестным.
//!
//! - Публикуется только [`KnownDraft`]: его выдаёт создание черновика этим клиентом или явный
//!   конструктор для ID из своего журнала. Чтение списка черновиков его не выдаёт: в списке бывают
//!   черновики того же пользователя из другого клиента. Массовой публикации в адаптере нет.
//! - Позицию черновика строит только сессия ([`ReviewSession::gitlab_position`]) из своей ревизии
//!   целиком; тройку SHA, которую записал сервер, адаптер сверяет с ревизией сессии.
//! - `approve` всегда несёт `sha` ревизии сессии; 409 — [`ApiError::ShaRejected`].
//! - Запрос изменений — только при мутации в схеме и роли не ниже Developer по чтению
//!   возможностей; обычной заметкой он не подменяется.
//!
//! Страницы списков адаптер строит сам (`per_page=100&page=N`), заголовки `x-next-page` и `link`
//! только говорят, есть ли следующая: транспорт подставляет токен в любой адрес.

pub mod capabilities;

use std::fmt;

use serde_json::{Map, Value, json};

use crate::model::diff::FileDiff;
use crate::model::revision::{Revision, Sha};
use crate::model::{
    DraftComment, ModelError, Placement, Provider, ProviderId, ReviewSession, ReviewTarget,
};
use crate::transport::{Failure, Method, Request, Response, Transport, percent_encode};

pub use crate::transport::WriteFate;
pub use capabilities::{Capabilities, HostVersion};

const ACCEPT: &str = "application/json";
const PER_PAGE: usize = 100;
/// Предел страниц одного списка: дальше — ошибка, а не бесконечный обход.
const MAX_PAGES: usize = 100;
/// Роль Developer (`access_level` 30): ниже неё ревью и запрос изменений не выполняются.
pub const DEVELOPER: u8 = 30;

const REQUEST_CHANGES: &str = "mutation($input: MergeRequestRequestChangesInput!) { \
mergeRequestRequestChanges(input: $input) { errors } }";
const REVIEWERS: &str = "query($fullPath: ID!, $iid: String!) { project(fullPath: $fullPath) { \
mergeRequest(iid: $iid) { reviewers(first: 100) { nodes { username \
mergeRequestInteraction { reviewState } } } } } }";

/// Сообщение `ResourceNotAvailable` GitLab: объекта нет или нет прав, точной причины ответ не даёт.
const NOT_AVAILABLE: &str = "does not exist or you don't have permission";

/// Коды статической проверки GraphQL (graphql-ruby): запрос отвергнут до исполнения.
const DEFINITIVE_GRAPHQL: [&str; 8] = [
    "undefinedField",
    "undefinedType",
    "argumentNotAccepted",
    "missingRequiredArguments",
    "variableMismatch",
    "argumentLiteralsIncompatible",
    "missingRequiredInputObjectAttribute",
    "inputObjectAttributeNotAccepted",
];

/// Адреса API. У GitLab REST и GraphQL лежат под корнем инсталляции: `<корень>/api/v4` и
/// `<корень>/api/graphql`. Корень — настройка, адаптер не выводит его из имени хоста.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    pub rest: String,
    pub graphql: String,
}

impl Endpoints {
    pub fn cloud() -> Endpoints {
        Endpoints::instance("https://gitlab.com")
    }

    pub fn instance(root: &str) -> Endpoints {
        let root = root.trim_end_matches('/');
        Endpoints {
            rest: format!("{root}/api/v4"),
            graphql: format!("{root}/api/graphql"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphqlError {
    /// `extensions.code` ошибки; у ошибок исполнения GitLab его обычно нет.
    pub code: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// Вход неверен, запрос не построен и не отправлен.
    Invalid(String),
    /// Модель отказала (устаревший якорь, другая ревизия): запросов не было.
    Model(ModelError),
    /// Возможности нет на хосте: по чтению возможностей (запросов записи не было) или по ответу
    /// схемы `undefinedField` на вызов.
    Absent(&'static str),
    /// Роль токена по чтению возможностей ниже нужной: запросов записи не было.
    NotPermitted {
        access_level: Option<u8>,
        required: u8,
    },
    /// Запрос не ушёл.
    NotSent(Failure),
    /// Ответа нет или он неполон, запрос мог дойти.
    Transport(Failure),
    /// Явный отказ по лимиту: 429.
    RateLimited { status: u16, detail: String },
    /// Ответ HTTP не 2xx.
    Status { status: u16, message: String },
    /// `approve` с `sha` отвергнут кодом 409: `sha` не равен head исходной ветки, или целевая ветка
    /// изменила diff без нового head (документация Approvals).
    ShaRejected(String),
    /// Ответ «объекта нет или нет прав» (GraphQL-ошибка `ResourceNotAvailable` или `project: null`):
    /// точной причины ответ не даёт.
    Unavailable(String),
    /// Прочие ошибки GraphQL верхнего уровня при HTTP 200.
    Graphql(Vec<GraphqlError>),
    /// Непустой `payload.errors` мутации при HTTP 200.
    Payload(Vec<String>),
    /// Успешный ответ не той формы или не с тем итогом.
    Unexpected(String),
    /// Запись выполнена, а проверочное чтение итога не удалось.
    Unverified(Box<ApiError>),
}

impl ApiError {
    pub fn write_fate(&self) -> WriteFate {
        match self {
            ApiError::Invalid(_)
            | ApiError::Model(_)
            | ApiError::Absent(_)
            | ApiError::NotPermitted { .. }
            | ApiError::NotSent(_)
            | ApiError::RateLimited { .. }
            | ApiError::ShaRejected(_)
            | ApiError::Unavailable(_)
            | ApiError::Payload(_) => WriteFate::NotAccepted,
            ApiError::Status { status, .. } if (400..500).contains(status) => {
                WriteFate::NotAccepted
            }
            ApiError::Graphql(errors)
                if !errors.is_empty()
                    && errors.iter().all(|error| {
                        error
                            .code
                            .as_deref()
                            .is_some_and(|code| DEFINITIVE_GRAPHQL.contains(&code))
                    }) =>
            {
                WriteFate::NotAccepted
            }
            ApiError::Status { .. }
            | ApiError::Graphql(_)
            | ApiError::Transport(_)
            | ApiError::Unexpected(_)
            | ApiError::Unverified(_) => WriteFate::Unknown,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Invalid(reason) => write!(f, "запрос не построен: {reason}"),
            ApiError::Model(err) => write!(f, "модель: {err}"),
            ApiError::Absent(what) => write!(f, "возможности нет на хосте: {what}"),
            ApiError::NotPermitted {
                access_level,
                required,
            } => write!(
                f,
                "роль токена {access_level:?} ниже нужной {required}, запрос не отправлен"
            ),
            ApiError::NotSent(failure) => write!(f, "запрос не ушёл: {failure}"),
            ApiError::Transport(failure) => write!(f, "итог неизвестен: {failure}"),
            ApiError::RateLimited { status, detail } => {
                write!(f, "лимит исчерпан (код {status}): {detail}")
            }
            ApiError::Status { status, message } => write!(f, "код {status}: {message}"),
            ApiError::ShaRejected(message) => write!(f, "approve с sha отвергнут (409): {message}"),
            ApiError::Unavailable(message) => {
                write!(
                    f,
                    "объекта нет или нет прав (без точной причины): {message}"
                )
            }
            ApiError::Graphql(errors) => {
                let parts: Vec<String> = errors
                    .iter()
                    .map(|error| match &error.code {
                        Some(code) => format!("{code}: {}", error.message),
                        None => error.message.clone(),
                    })
                    .collect();
                write!(f, "ошибки GraphQL: {}", parts.join("; "))
            }
            ApiError::Payload(errors) => write!(f, "payload.errors: {}", errors.join("; ")),
            ApiError::Unexpected(reason) => write!(f, "неожиданный ответ: {reason}"),
            ApiError::Unverified(read) => {
                write!(f, "запись выполнена, проверочное чтение не удалось: {read}")
            }
        }
    }
}

impl std::error::Error for ApiError {}

impl From<ModelError> for ApiError {
    fn from(err: ModelError) -> ApiError {
        ApiError::Model(err)
    }
}

/// `MergeRequestReviewState` GraphQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewerState {
    Unreviewed,
    ReviewStarted,
    Reviewed,
    RequestedChanges,
    Approved,
    Unapproved,
}

impl ReviewerState {
    const ALL: [ReviewerState; 6] = [
        ReviewerState::Unreviewed,
        ReviewerState::ReviewStarted,
        ReviewerState::Reviewed,
        ReviewerState::RequestedChanges,
        ReviewerState::Approved,
        ReviewerState::Unapproved,
    ];

    pub fn graphql(self) -> &'static str {
        match self {
            ReviewerState::Unreviewed => "UNREVIEWED",
            ReviewerState::ReviewStarted => "REVIEW_STARTED",
            ReviewerState::Reviewed => "REVIEWED",
            ReviewerState::RequestedChanges => "REQUESTED_CHANGES",
            ReviewerState::Approved => "APPROVED",
            ReviewerState::Unapproved => "UNAPPROVED",
        }
    }

    pub fn parse(name: &str) -> Option<ReviewerState> {
        ReviewerState::ALL
            .into_iter()
            .find(|state| state.graphql() == name)
    }
}

/// Ревьюер MR и его состояние (`null` у GitLab — `None`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reviewer {
    pub username: String,
    pub state: Option<ReviewerState>,
}

/// Снимок MR: ревизия из `diff_refs` и diff версии MR именно с этой тройкой SHA.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeSnapshot {
    pub id: ProviderId,
    pub author: String,
    pub reviewers: Vec<String>,
    pub version_id: ProviderId,
    pub revision: Revision,
    /// Файлы с `diff`; слишком большие и свёрнутые файлы без `diff` якорей не принимают.
    pub files: Vec<FileDiff>,
}

/// Позиция заметки или черновика в ответе сервера.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotePosition {
    pub base_sha: String,
    pub start_sha: String,
    pub head_sha: String,
    pub old_path: String,
    pub new_path: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub line_range: Option<Value>,
}

impl RemotePosition {
    /// Тройка SHA позиции.
    pub fn revision(&self) -> Result<Revision, ApiError> {
        Ok(Revision::new(
            parse_sha(&self.base_sha)?,
            parse_sha(&self.start_sha)?,
            parse_sha(&self.head_sha)?,
        ))
    }
}

/// Черновик в ответе сервера.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDraft {
    pub id: ProviderId,
    pub author_id: Option<ProviderId>,
    pub note: String,
    pub discussion_id: Option<ProviderId>,
    pub resolve_discussion: bool,
    pub line_code: Option<String>,
    pub position: Option<RemotePosition>,
}

/// Черновик, который адаптер вправе изменять, удалять и публиковать по ID: создан этим клиентом
/// ([`GitLab::create_draft`]) или записан в его журнале ([`KnownDraft::recorded`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownDraft {
    id: ProviderId,
}

impl KnownDraft {
    /// ID черновика из своего журнала. ID из списка черновиков сервера сюда не передаются: среди
    /// них бывают черновики того же пользователя из другого клиента.
    pub fn recorded(id: ProviderId) -> KnownDraft {
        KnownDraft { id }
    }

    pub fn id(&self) -> &ProviderId {
        &self.id
    }
}

/// Заметка MR в ответе сервера.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteNote {
    pub id: ProviderId,
    pub author: String,
    pub body: String,
    pub system: bool,
    pub resolvable: bool,
    pub resolved: Option<bool>,
    pub created_at: Option<String>,
    pub position: Option<RemotePosition>,
}

/// Тред MR с заметками.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDiscussion {
    pub id: ProviderId,
    pub individual_note: bool,
    pub notes: Vec<RemoteNote>,
}

/// Состояние одобрения MR для токена.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    pub approved: bool,
    pub approved_by: Vec<String>,
    pub user_has_approved: Option<bool>,
    pub user_can_approve: Option<bool>,
}

/// Адаптер одного MR.
pub struct GitLab<T> {
    transport: T,
    endpoints: Endpoints,
    project: String,
    iid: u64,
}

impl<T: Transport> GitLab<T> {
    pub fn new(
        transport: T,
        endpoints: Endpoints,
        target: &ReviewTarget,
    ) -> Result<GitLab<T>, ApiError> {
        if target.provider != Provider::GitLab {
            return Err(ApiError::Invalid(format!(
                "цель {} для адаптера GitLab",
                target.provider.name()
            )));
        }
        let segments: Vec<&str> = target.repo.split('/').collect();
        if segments.len() < 2 || segments.iter().any(|segment| segment.is_empty()) {
            return Err(ApiError::Invalid(
                "путь проекта GitLab — пространство имён/проект".to_owned(),
            ));
        }
        Ok(GitLab {
            transport,
            endpoints,
            project: target.repo.clone(),
            iid: target.number,
        })
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Проект целиком одним сегментом: подгруппы входят в путь, `/` кодируется.
    fn project_path(&self) -> String {
        format!("/projects/{}", percent_encode(&self.project))
    }

    fn mr_path(&self) -> String {
        format!("{}/merge_requests/{}", self.project_path(), self.iid)
    }

    fn drafts_path(&self) -> String {
        format!("{}/draft_notes", self.mr_path())
    }

    fn draft_path(&self, id: &ProviderId) -> String {
        format!("{}/{}", self.drafts_path(), percent_encode(id.as_str()))
    }

    fn rest(&self, method: Method, path: &str) -> Request {
        Request::new(method, format!("{}{path}", self.endpoints.rest)).header("accept", ACCEPT)
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

    fn get_json(&self, path: &str) -> Result<Value, ApiError> {
        let response = self.call(&self.rest(Method::Get, path))?;
        json_of(&response)
    }

    fn write_json(&self, method: Method, path: &str, body: &Value) -> Result<Value, ApiError> {
        let response = self.call(&self.rest(method, path).json(body))?;
        json_of(&response)
    }

    /// Запись без тела в ответе (204): тело не читается.
    fn write_empty(&self, method: Method, path: &str) -> Result<(), ApiError> {
        self.call(&self.rest(method, path)).map(|_| ())
    }

    /// Все страницы списка REST.
    fn pages(&self, path: &str) -> Result<Vec<Value>, ApiError> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut items = Vec::new();
        for page in 1..=MAX_PAGES {
            let request = self.rest(
                Method::Get,
                &format!("{path}{separator}per_page={PER_PAGE}&page={page}"),
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
                    "пустая страница {page} со ссылкой на следующую"
                )));
            }
        }
        Err(ApiError::Unexpected(format!(
            "в списке больше {MAX_PAGES} страниц"
        )))
    }

    /// Запрос GraphQL: аргументы только в `variables`. Возвращает `data` целиком: `null` у поля —
    /// значимый ответ (`project: null`), его разбирает вызывающий.
    fn graphql(&self, query: &str, variables: Value) -> Result<Map<String, Value>, ApiError> {
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
            if errors
                .iter()
                .all(|error| error.message.contains(NOT_AVAILABLE))
            {
                return Err(ApiError::Unavailable(errors[0].message.clone()));
            }
            return Err(ApiError::Graphql(errors));
        }
        match body.get("data") {
            Some(Value::Object(data)) => Ok(data.clone()),
            _ => Err(ApiError::Unexpected("ответ GraphQL без data".to_owned())),
        }
    }

    /// Ревизия из `diff_refs` MR и diff версии MR с той же тройкой SHA. Версия неизменна, поэтому
    /// файлы другого head с ревизией не смешиваются; версии с этой тройкой нет — ошибка, а не
    /// ближайшая версия.
    pub fn merge_request(&self) -> Result<MergeSnapshot, ApiError> {
        let mr = self.get_json(&self.mr_path())?;
        let refs = mr
            .get("diff_refs")
            .filter(|refs| refs.is_object())
            .ok_or_else(|| ApiError::Unexpected("у MR нет diff_refs".to_owned()))?;
        let revision = triple(refs, "base_sha", "start_sha", "head_sha")?;
        let versions = self.pages(&format!("{}/versions", self.mr_path()))?;
        let version = versions
            .iter()
            .find(|version| version_revision(version).is_ok_and(|candidate| candidate == revision))
            .ok_or_else(|| {
                ApiError::Unexpected(format!(
                    "у MR нет версии с тройкой diff_refs (head {})",
                    revision.head_sha()
                ))
            })?;
        let version_id = id_field(version, "id")?;
        let detail = self.get_json(&format!(
            "{}/versions/{}",
            self.mr_path(),
            percent_encode(version_id.as_str())
        ))?;
        if version_revision(&detail)? != revision {
            return Err(ApiError::Unexpected(format!(
                "версия {} вернула другую тройку SHA",
                version_id.as_str()
            )));
        }
        let diffs = detail
            .get("diffs")
            .and_then(Value::as_array)
            .ok_or_else(|| ApiError::Unexpected("у версии MR нет diffs".to_owned()))?;
        let mut files = Vec::new();
        for diff in diffs {
            let flag = |name: &str| diff.get(name).and_then(Value::as_bool).unwrap_or(false);
            let text = diff.get("diff").and_then(Value::as_str).unwrap_or("");
            if flag("too_large") || flag("collapsed") || text.is_empty() {
                continue;
            }
            files.push(FileDiff::parse(
                &str_field(diff, "old_path")?,
                &str_field(diff, "new_path")?,
                text,
            )?);
        }
        Ok(MergeSnapshot {
            id: id_field(&mr, "id")?,
            author: username_of(mr.get("author")),
            reviewers: mr
                .get("reviewers")
                .and_then(Value::as_array)
                .map(|reviewers| {
                    reviewers
                        .iter()
                        .map(|reviewer| username_of(Some(reviewer)))
                        .collect()
                })
                .unwrap_or_default(),
            version_id,
            revision,
            files,
        })
    }

    /// Ревьюеры MR с состоянием (GraphQL). `project: null` и `mergeRequest: null` —
    /// [`ApiError::Unavailable`].
    pub fn reviewers(&self) -> Result<Vec<Reviewer>, ApiError> {
        let data = self.graphql(
            REVIEWERS,
            json!({"fullPath": self.project, "iid": self.iid.to_string()}),
        )?;
        let project = data
            .get("project")
            .filter(|project| !project.is_null())
            .ok_or_else(|| ApiError::Unavailable("project: null".to_owned()))?;
        let mr = project
            .get("mergeRequest")
            .filter(|mr| !mr.is_null())
            .ok_or_else(|| ApiError::Unavailable("mergeRequest: null".to_owned()))?;
        let nodes = mr
            .pointer("/reviewers/nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| ApiError::Unexpected("reviewers без nodes".to_owned()))?;
        nodes
            .iter()
            .map(|node| {
                let state = match node.pointer("/mergeRequestInteraction/reviewState") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(name)) => {
                        Some(ReviewerState::parse(name).ok_or_else(|| {
                            ApiError::Unexpected(format!("неизвестное reviewState {name}"))
                        })?)
                    }
                    Some(other) => {
                        return Err(ApiError::Unexpected(format!("reviewState {other}")));
                    }
                };
                Ok(Reviewer {
                    username: str_field(node, "username")?,
                    state,
                })
            })
            .collect()
    }

    /// Состояние ревьюера `username`; `None` — пользователь не ревьюер MR.
    pub fn reviewer_state(&self, username: &str) -> Result<Option<ReviewerState>, ApiError> {
        Ok(self
            .reviewers()?
            .into_iter()
            .find(|reviewer| reviewer.username.eq_ignore_ascii_case(username))
            .and_then(|reviewer| reviewer.state))
    }

    /// Свои черновики MR по всем страницам: сервер показывает только черновики токена, в том числе
    /// созданные другим клиентом того же пользователя.
    pub fn drafts(&self) -> Result<Vec<RemoteDraft>, ApiError> {
        self.pages(&self.drafts_path())?
            .iter()
            .map(draft_of)
            .collect()
    }

    /// Черновик по ID.
    pub fn draft(&self, id: &ProviderId) -> Result<RemoteDraft, ApiError> {
        draft_of(&self.get_json(&self.draft_path(id))?)
    }

    /// Тело черновика: позиция из ревизии сессии или ID треда ответа.
    fn draft_body(
        &self,
        session: &ReviewSession,
        draft: &DraftComment,
        resolve: bool,
    ) -> Result<(Map<String, Value>, Option<Value>), ApiError> {
        if draft.body.trim().is_empty() {
            return Err(ApiError::Invalid("пустой текст черновика".to_owned()));
        }
        let mut body = Map::new();
        body.insert("note".to_owned(), json!(draft.body));
        let sent = match draft.placement() {
            Placement::Line(_) => {
                if resolve {
                    return Err(ApiError::Invalid(
                        "resolve_discussion — только у ответа в треде".to_owned(),
                    ));
                }
                let position = session.gitlab_position(draft)?;
                session.verify_outgoing(&position.revision())?;
                let position = serde_json::to_value(&position)
                    .map_err(|err| ApiError::Invalid(format!("позиция: {err}")))?;
                body.insert("position".to_owned(), position.clone());
                Some(position)
            }
            Placement::Reply { thread } => {
                let provider = session.target().provider;
                if provider != Provider::GitLab {
                    return Err(ApiError::Model(ModelError::Provider {
                        expected: Provider::GitLab,
                        found: provider,
                    }));
                }
                body.insert(
                    "in_reply_to_discussion_id".to_owned(),
                    json!(thread.as_str()),
                );
                if resolve {
                    body.insert("resolve_discussion".to_owned(), json!(true));
                }
                None
            }
        };
        Ok((body, sent))
    }

    /// Ответ сервера на запись черновика совпадает с отправленным: тройка SHA ревизии сессии,
    /// пути и строки позиции или тред ответа.
    fn check_echo(
        &self,
        session: &ReviewSession,
        draft: &DraftComment,
        sent: Option<&Value>,
        remote: &RemoteDraft,
    ) -> Result<(), ApiError> {
        match (draft.placement(), sent) {
            (Placement::Line(_), Some(sent)) => {
                let position = remote.position.as_ref().ok_or_else(|| {
                    ApiError::Unexpected(format!(
                        "черновик {} записан без позиции",
                        remote.id.as_str()
                    ))
                })?;
                let triple = position.revision()?;
                if session.verify_outgoing(&triple).is_err() {
                    return Err(ApiError::Unexpected(format!(
                        "черновик {} записан с другой тройкой SHA (head {})",
                        remote.id.as_str(),
                        position.head_sha
                    )));
                }
                let line = |name: &str| sent.get(name).and_then(Value::as_u64);
                let same = position.old_path == sent["old_path"].as_str().unwrap_or_default()
                    && position.new_path == sent["new_path"].as_str().unwrap_or_default()
                    && position.old_line.map(u64::from) == line("old_line")
                    && position.new_line.map(u64::from) == line("new_line");
                if !same {
                    return Err(ApiError::Unexpected(format!(
                        "черновик {} записан на другой строке",
                        remote.id.as_str()
                    )));
                }
                Ok(())
            }
            (Placement::Reply { thread }, _) => {
                if remote.discussion_id.as_ref() == Some(thread) {
                    Ok(())
                } else {
                    Err(ApiError::Unexpected(format!(
                        "черновик {} привязан к треду {:?}, ожидался {}",
                        remote.id.as_str(),
                        remote.discussion_id.as_ref().map(ProviderId::as_str),
                        thread.as_str()
                    )))
                }
            }
            (Placement::Line(_), None) => Err(ApiError::Invalid("позиция не построена".to_owned())),
        }
    }

    /// Новый черновик: позиция из ревизии сессии (`Placement::Line`) или ответ в треде
    /// (`Placement::Reply`, `resolve` — разрешить тред при публикации). Один запрос.
    pub fn create_draft(
        &self,
        session: &ReviewSession,
        draft: &DraftComment,
        resolve: bool,
    ) -> Result<(KnownDraft, RemoteDraft), ApiError> {
        let (body, sent) = self.draft_body(session, draft, resolve)?;
        let value = self.write_json(Method::Post, &self.drafts_path(), &Value::Object(body))?;
        let remote = draft_of(&value)?;
        self.check_echo(session, draft, sent.as_ref(), &remote)?;
        Ok((
            KnownDraft {
                id: remote.id.clone(),
            },
            remote,
        ))
    }

    /// Новый текст и позиция известного черновика (после «перейти» или «перепривязать» позиция
    /// берётся из новой ревизии сессии). ID сохраняется: ответ с другим ID — неожиданный.
    pub fn update_draft(
        &self,
        session: &ReviewSession,
        known: &KnownDraft,
        draft: &DraftComment,
    ) -> Result<RemoteDraft, ApiError> {
        let (mut body, sent) = self.draft_body(session, draft, false)?;
        body.remove("in_reply_to_discussion_id");
        let value = self.write_json(
            Method::Put,
            &self.draft_path(&known.id),
            &Value::Object(body),
        )?;
        let remote = draft_of(&value)?;
        if remote.id != known.id {
            return Err(ApiError::Unexpected(format!(
                "ID черновика сменился: {} → {}",
                known.id.as_str(),
                remote.id.as_str()
            )));
        }
        self.check_echo(session, draft, sent.as_ref(), &remote)?;
        Ok(remote)
    }

    /// Удаление известного черновика (уборка клетки).
    pub fn delete_draft(&self, known: &KnownDraft) -> Result<(), ApiError> {
        self.write_empty(Method::Delete, &self.draft_path(&known.id))
    }

    /// Публикация одного известного черновика по ID. Ответ — 204 без тела: заметку и её ID он не
    /// возвращает, поэтому успешный ответ публикацию не подтверждает связью с заметкой — это
    /// дело сверки. 404 — обычный отказ, вывода «уже опубликован» адаптер не делает.
    pub fn publish_draft(&self, known: &KnownDraft) -> Result<(), ApiError> {
        self.write_empty(
            Method::Put,
            &format!("{}/publish", self.draft_path(&known.id)),
        )
    }

    /// Сводка ревью отдельной заметкой MR (отдельный шаг после публикации черновиков).
    pub fn summary_note(&self, body: &str) -> Result<RemoteNote, ApiError> {
        if body.trim().is_empty() {
            return Err(ApiError::Invalid("пустой текст сводки".to_owned()));
        }
        let value = self.write_json(
            Method::Post,
            &format!("{}/notes", self.mr_path()),
            &json!({"body": body}),
        )?;
        note_of(&value)
    }

    /// Заметки MR по всем страницам, старые первыми.
    pub fn notes(&self) -> Result<Vec<RemoteNote>, ApiError> {
        self.pages(&format!("{}/notes?sort=asc", self.mr_path()))?
            .iter()
            .map(note_of)
            .collect()
    }

    /// Треды MR по всем страницам.
    pub fn discussions(&self) -> Result<Vec<RemoteDiscussion>, ApiError> {
        self.pages(&format!("{}/discussions", self.mr_path()))?
            .iter()
            .map(discussion_of)
            .collect()
    }

    pub fn discussion(&self, id: &ProviderId) -> Result<RemoteDiscussion, ApiError> {
        discussion_of(&self.get_json(&format!(
            "{}/discussions/{}",
            self.mr_path(),
            percent_encode(id.as_str())
        ))?)
    }

    /// Состояние одобрения MR для токена.
    pub fn approvals(&self) -> Result<Approval, ApiError> {
        approval_of(&self.get_json(&format!("{}/approvals", self.mr_path()))?)
    }

    /// `approve` с `sha` — head ревизии сессии; без `sha` запрос не отправляется. 409 —
    /// [`ApiError::ShaRejected`]. Ответ без собственного одобрения токена — неожиданный.
    pub fn approve(&self, session: &ReviewSession) -> Result<Approval, ApiError> {
        let head = session.revision().head_sha();
        let value = match self.write_json(
            Method::Post,
            &format!("{}/approve", self.mr_path()),
            &json!({"sha": head.as_str()}),
        ) {
            Err(ApiError::Status {
                status: 409,
                message,
            }) => return Err(ApiError::ShaRejected(message)),
            other => other?,
        };
        let approval = approval_of(&value)?;
        let own = match approval.user_has_approved {
            Some(flag) => flag,
            None => approval
                .approved_by
                .iter()
                .any(|name| name.eq_ignore_ascii_case(session.account())),
        };
        if !own {
            return Err(ApiError::Unexpected(
                "ответ approve без одобрения токена".to_owned(),
            ));
        }
        Ok(approval)
    }

    /// Снятие одобрения токена. Тело ответа не читается: снятие подтверждает только чтение
    /// одобрений.
    pub fn unapprove(&self) -> Result<(), ApiError> {
        self.write_empty(Method::Post, &format!("{}/unapprove", self.mr_path()))
    }

    /// Запрос изменений через `mergeRequestRequestChanges(projectPath, iid)` без SHA-предусловия.
    /// Без мутации в схеме — [`ApiError::Absent`], при роли ниже Developer —
    /// [`ApiError::NotPermitted`]; в обоих случаях запросов нет. Итог проверяется отдельным
    /// чтением состояния ревьюера после мутации: другое состояние или сбой чтения оставляют итог
    /// неизвестным. Обычной заметкой запрос изменений не подменяется.
    pub fn request_changes(&self, caps: &Capabilities) -> Result<ReviewerState, ApiError> {
        if !caps.request_changes() {
            return Err(ApiError::Absent(capabilities::REQUEST_CHANGES));
        }
        if !caps.can_review() {
            return Err(ApiError::NotPermitted {
                access_level: caps.access_level,
                required: DEVELOPER,
            });
        }
        let data = match self.graphql(
            REQUEST_CHANGES,
            json!({"input": {"projectPath": self.project, "iid": self.iid.to_string()}}),
        ) {
            Err(ApiError::Graphql(errors))
                if errors
                    .iter()
                    .any(|error| error.code.as_deref() == Some("undefinedField")) =>
            {
                return Err(ApiError::Absent(capabilities::REQUEST_CHANGES));
            }
            other => other?,
        };
        let payload = data
            .get(capabilities::REQUEST_CHANGES)
            .filter(|payload| !payload.is_null())
            .ok_or_else(|| {
                ApiError::Unexpected("mergeRequestRequestChanges: null без ошибок".to_owned())
            })?;
        let errors = payload
            .get("errors")
            .and_then(Value::as_array)
            .ok_or_else(|| ApiError::Unexpected("payload без errors".to_owned()))?;
        if !errors.is_empty() {
            return Err(ApiError::Payload(
                errors
                    .iter()
                    .map(|error| {
                        error
                            .as_str()
                            .map_or_else(|| error.to_string(), str::to_owned)
                    })
                    .collect(),
            ));
        }
        match self.reviewer_state(&caps.username) {
            Ok(Some(ReviewerState::RequestedChanges)) => Ok(ReviewerState::RequestedChanges),
            Ok(other) => Err(ApiError::Unexpected(format!(
                "после mergeRequestRequestChanges состояние ревьюера {:?}",
                other.map(ReviewerState::graphql)
            ))),
            Err(read) => Err(ApiError::Unverified(Box::new(read))),
        }
    }
}

/// Отказ по лимиту GitLab — 429 (`Retry later`, тело text/plain).
fn rest_error(response: &Response) -> ApiError {
    let body = response.body_json().unwrap_or(Value::Null);
    let message = match body.get("message").or_else(|| body.get("error")) {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::from_utf8_lossy(&response.body).trim().to_owned(),
    };
    if response.status == 429 {
        return ApiError::RateLimited {
            status: response.status,
            detail: message,
        };
    }
    ApiError::Status {
        status: response.status,
        message,
    }
}

fn json_of(response: &Response) -> Result<Value, ApiError> {
    response.body_json().map_err(ApiError::Unexpected)
}

/// Следующая страница: непустой `x-next-page`; без заголовка — ссылка `rel="next"`.
fn has_next(response: &Response) -> bool {
    match response.header("x-next-page") {
        Some(next) => !next.trim().is_empty(),
        None => response.header("link").is_some_and(|link| {
            link.split(',')
                .any(|part| part.split(';').skip(1).any(|p| p.trim() == "rel=\"next\""))
        }),
    }
}

fn graphql_error(value: &Value) -> GraphqlError {
    GraphqlError {
        code: value
            .pointer("/extensions/code")
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

fn optional_line(value: &Value, name: &str) -> Option<u32> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|line| u32::try_from(line).ok())
}

fn username_of(user: Option<&Value>) -> String {
    user.and_then(|user| user.get("username"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn parse_sha(value: &str) -> Result<Sha, ApiError> {
    Sha::parse(value).map_err(|err| ApiError::Unexpected(err.to_string()))
}

fn triple(value: &Value, base: &str, start: &str, head: &str) -> Result<Revision, ApiError> {
    let sha = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::Unexpected(format!("нет {name}")))
            .and_then(parse_sha)
    };
    Ok(Revision::new(sha(base)?, sha(start)?, sha(head)?))
}

fn version_revision(version: &Value) -> Result<Revision, ApiError> {
    triple(
        version,
        "base_commit_sha",
        "start_commit_sha",
        "head_commit_sha",
    )
}

/// Позиция ответа; черновик без позиции GitLab отдаёт объектом из `null`.
fn position_of(value: Option<&Value>) -> Result<Option<RemotePosition>, ApiError> {
    let Some(position) = value.filter(|position| position.is_object()) else {
        return Ok(None);
    };
    if position.get("head_sha").is_none_or(Value::is_null) {
        return Ok(None);
    }
    let text = |name: &str| {
        position
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok(Some(RemotePosition {
        base_sha: str_field(position, "base_sha")?,
        start_sha: str_field(position, "start_sha")?,
        head_sha: str_field(position, "head_sha")?,
        old_path: text("old_path"),
        new_path: text("new_path"),
        old_line: optional_line(position, "old_line"),
        new_line: optional_line(position, "new_line"),
        line_range: position
            .get("line_range")
            .filter(|range| !range.is_null())
            .cloned(),
    }))
}

fn draft_of(value: &Value) -> Result<RemoteDraft, ApiError> {
    Ok(RemoteDraft {
        id: id_field(value, "id")?,
        author_id: id_field(value, "author_id").ok(),
        note: value
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        discussion_id: id_field(value, "discussion_id").ok(),
        resolve_discussion: value
            .get("resolve_discussion")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        line_code: value
            .get("line_code")
            .and_then(Value::as_str)
            .map(str::to_owned),
        position: position_of(value.get("position"))?,
    })
}

fn note_of(value: &Value) -> Result<RemoteNote, ApiError> {
    Ok(RemoteNote {
        id: id_field(value, "id")?,
        author: username_of(value.get("author")),
        body: value
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        system: value
            .get("system")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        resolvable: value
            .get("resolvable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        resolved: value.get("resolved").and_then(Value::as_bool),
        created_at: value
            .get("created_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
        position: position_of(value.get("position"))?,
    })
}

fn discussion_of(value: &Value) -> Result<RemoteDiscussion, ApiError> {
    let notes = value
        .get("notes")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::Unexpected("тред без notes".to_owned()))?;
    Ok(RemoteDiscussion {
        id: id_field(value, "id")?,
        individual_note: value
            .get("individual_note")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        notes: notes.iter().map(note_of).collect::<Result<_, _>>()?,
    })
}

fn approval_of(value: &Value) -> Result<Approval, ApiError> {
    let approved_by = value
        .get("approved_by")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::Unexpected("ответ одобрения без approved_by".to_owned()))?
        .iter()
        .map(|entry| username_of(entry.get("user")))
        .collect();
    Ok(Approval {
        approved: value
            .get("approved")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        approved_by,
        user_has_approved: value.get("user_has_approved").and_then(Value::as_bool),
        user_can_approve: value.get("user_can_approve").and_then(Value::as_bool),
    })
}

#[cfg(test)]
mod tests;
