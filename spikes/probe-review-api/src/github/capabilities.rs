//! Возможности GitHub, определённые на хосте чтением, а не по имени хоста.
//!
//! Чтения: `GET /user` (логин, тариф, если виден токену, scopes classic-токена), `GET /versions`
//! (поддерживаемые версии REST API, 404 — хост их не сообщает), `GET /repos/{o}/{r}` (приватность и
//! право записи) и introspection `__type(name: "Mutation")` GraphQL (наличие мутаций серверного
//! `PENDING`, просмотренных файлов и resolve). Заголовок выбранной версии в ответе документация
//! не описывает, поэтому версия записывается как запрошенная плюс список `GET /versions`.

use serde::Serialize;
use serde_json::{Value, json};

use super::{API_VERSION, ApiError, GitHub};
use crate::transport::Transport;

pub const ADD_THREAD: &str = "addPullRequestReviewThread";
pub const UPDATE_COMMENT: &str = "updatePullRequestReviewComment";
pub const MARK_VIEWED: &str = "markFileAsViewed";
pub const UNMARK_VIEWED: &str = "unmarkFileAsViewed";
pub const RESOLVE_THREAD: &str = "resolveReviewThread";
pub const UNRESOLVE_THREAD: &str = "unresolveReviewThread";

/// Мутации, которые нужны адаптеру; остальные поля `Mutation` не записываются.
pub const MUTATIONS: [&str; 6] = [
    ADD_THREAD,
    UPDATE_COMMENT,
    MARK_VIEWED,
    UNMARK_VIEWED,
    RESOLVE_THREAD,
    UNRESOLVE_THREAD,
];

pub(super) const INTROSPECT: &str =
    "query($name: String!) { __type(name: $name) { fields { name } } }";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ApiVersion {
    /// Версия в заголовке `X-GitHub-Api-Version` каждого запроса REST.
    pub requested: String,
    /// Версии из `GET /versions`; `None` — хост ответил 404.
    pub supported: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    pub login: String,
    /// `plan.name` из `GET /user`, если тариф виден токену.
    pub plan: Option<String>,
    /// `x-oauth-scopes` classic-токена; у fine-grained токена заголовка нет.
    pub token_scopes: Option<Vec<String>>,
    pub api: ApiVersion,
    pub repo_private: bool,
    pub can_push: bool,
    /// Мутации из [`MUTATIONS`], которые есть в схеме, в порядке [`MUTATIONS`].
    pub mutations: Vec<String>,
}

impl Capabilities {
    pub fn has(&self, mutation: &str) -> bool {
        self.mutations.iter().any(|name| name == mutation)
    }

    /// Серверный `PENDING`: добавление треда по node ID review и изменение комментария.
    pub fn server_pending(&self) -> bool {
        self.has(ADD_THREAD) && self.has(UPDATE_COMMENT)
    }

    pub fn viewed(&self) -> bool {
        self.has(MARK_VIEWED) && self.has(UNMARK_VIEWED)
    }

    pub fn resolve(&self) -> bool {
        self.has(RESOLVE_THREAD) && self.has(UNRESOLVE_THREAD)
    }

    /// Состояние для `result.json`; логин — до записи через очиститель.
    pub fn to_json(&self) -> Value {
        json!({
            "login": self.login,
            "plan": self.plan,
            "token_scopes": self.token_scopes,
            "api_version": self.api,
            "repo_private": self.repo_private,
            "can_push": self.can_push,
            "mutations": self.mutations,
            "branches": {
                "local_create": true,
                "server_pending": self.server_pending(),
            },
            "viewed": self.viewed(),
            "resolve": self.resolve(),
        })
    }
}

impl<T: Transport> GitHub<T> {
    /// Возможности хоста и токена чтением.
    pub fn capabilities(&self) -> Result<Capabilities, ApiError> {
        let (user, response) = self.get_json("/user")?;
        let login = user
            .get("login")
            .and_then(Value::as_str)
            .filter(|login| !login.is_empty())
            .ok_or_else(|| ApiError::Unexpected("GET /user без login".to_owned()))?
            .to_owned();
        let plan = user
            .pointer("/plan/name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let token_scopes = response.header("x-oauth-scopes").map(|scopes| {
            scopes
                .split(',')
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .map(str::to_owned)
                .collect()
        });
        let supported = match self.get_json("/versions") {
            Ok((Value::Array(versions), _)) => Some(
                versions
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            ),
            Ok(_) => {
                return Err(ApiError::Unexpected(
                    "GET /versions вернул не массив".to_owned(),
                ));
            }
            Err(ApiError::Status { status: 404, .. }) => None,
            Err(err) => return Err(err),
        };
        let (repo, _) = self.get_json(&self.repo_path())?;
        let repo_private = repo
            .get("private")
            .and_then(Value::as_bool)
            .ok_or_else(|| ApiError::Unexpected("у репозитория нет private".to_owned()))?;
        let can_push = repo
            .pointer("/permissions/push")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let schema = self.graphql(INTROSPECT, json!({"name": "Mutation"}), "__type")?;
        let fields = schema
            .get("fields")
            .and_then(Value::as_array)
            .ok_or_else(|| ApiError::Unexpected("__type(Mutation) без fields".to_owned()))?;
        let present: Vec<&str> = fields
            .iter()
            .filter_map(|field| field.get("name").and_then(Value::as_str))
            .collect();
        let mutations = MUTATIONS
            .iter()
            .filter(|name| present.contains(name))
            .map(|name| (*name).to_owned())
            .collect();
        Ok(Capabilities {
            login,
            plan,
            token_scopes,
            api: ApiVersion {
                requested: API_VERSION.to_owned(),
                supported,
            },
            repo_private,
            can_push,
            mutations,
        })
    }
}
