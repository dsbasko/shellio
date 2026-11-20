//! Возможности GitLab, определённые на хосте чтением, а не по имени хоста.
//!
//! Чтения: `GET /user` (логин), `GET /metadata` (версия, ревизия, `enterprise`; 404 у старых
//! версий — `GET /version` без редакции), `GET /projects/:id` (видимость, роль токена — большая из
//! `project_access` и `group_access`), `GET /namespaces/:id` (тариф `plan`, если виден токену:
//! 403 и 404 — тариф не виден) и introspection `__type(name: "Mutation")` GraphQL (наличие
//! `mergeRequestRequestChanges`).

use serde::Serialize;
use serde_json::{Value, json};

use super::{ApiError, DEVELOPER, GitLab, str_field};
use crate::transport::Transport;

pub const REQUEST_CHANGES: &str = "mergeRequestRequestChanges";

/// Мутации, которые нужны адаптеру; остальные поля `Mutation` не записываются.
pub const MUTATIONS: [&str; 1] = [REQUEST_CHANGES];

pub(super) const INTROSPECT: &str =
    "query($name: String!) { __type(name: $name) { fields { name } } }";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostVersion {
    pub version: String,
    pub revision: Option<String>,
    /// `enterprise` из `GET /metadata`; `None` — хост сообщил версию только через `GET /version`.
    pub enterprise: Option<bool>,
    /// Откуда взята версия: `metadata` или `version`.
    pub source: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    pub username: String,
    pub host: HostVersion,
    /// `plan` пространства имён проекта, если тариф виден токену.
    pub plan: Option<String>,
    pub visibility: String,
    /// Большая из ролей `project_access` и `group_access`; `None` — роли нет.
    pub access_level: Option<u8>,
    /// Мутации из [`MUTATIONS`], которые есть в схеме, в порядке [`MUTATIONS`].
    pub mutations: Vec<String>,
}

impl Capabilities {
    pub fn has(&self, mutation: &str) -> bool {
        self.mutations.iter().any(|name| name == mutation)
    }

    pub fn request_changes(&self) -> bool {
        self.has(REQUEST_CHANGES)
    }

    /// Роль не ниже Developer: ревью и запрос изменений (порог — предположение пробы).
    pub fn can_review(&self) -> bool {
        self.access_level.is_some_and(|level| level >= DEVELOPER)
    }

    pub fn private(&self) -> bool {
        self.visibility == "private"
    }

    /// Состояние для `result.json`; логин — до записи через очиститель.
    pub fn to_json(&self) -> Value {
        json!({
            "username": self.username,
            "host": self.host,
            "plan": self.plan,
            "visibility": self.visibility,
            "access_level": self.access_level,
            "mutations": self.mutations,
            "draft_publish": true,
            "approve_sha": true,
            "request_changes": self.request_changes(),
            "can_review": self.can_review(),
        })
    }
}

impl<T: Transport> GitLab<T> {
    /// Возможности хоста и токена чтением.
    pub fn capabilities(&self) -> Result<Capabilities, ApiError> {
        let user = self.get_json("/user")?;
        let username = user
            .get("username")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| ApiError::Unexpected("GET /user без username".to_owned()))?
            .to_owned();
        let host = self.host_version()?;
        let project = self.get_json(&self.project_path())?;
        let visibility = str_field(&project, "visibility")?;
        let level = |pointer: &str| {
            project
                .pointer(pointer)
                .and_then(Value::as_u64)
                .and_then(|level| u8::try_from(level).ok())
        };
        let access_level = level("/permissions/project_access/access_level")
            .max(level("/permissions/group_access/access_level"));
        let plan = match project.pointer("/namespace/id").and_then(Value::as_u64) {
            Some(namespace) => match self.get_json(&format!("/namespaces/{namespace}")) {
                Ok(value) => value.get("plan").and_then(Value::as_str).map(str::to_owned),
                Err(ApiError::Status {
                    status: 403 | 404, ..
                }) => None,
                Err(err) => return Err(err),
            },
            None => None,
        };
        let data = self.graphql(INTROSPECT, json!({"name": "Mutation"}))?;
        let fields = data
            .get("__type")
            .and_then(|schema| schema.get("fields"))
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
            username,
            host,
            plan,
            visibility,
            access_level,
            mutations,
        })
    }

    fn host_version(&self) -> Result<HostVersion, ApiError> {
        let (value, source) = match self.get_json("/metadata") {
            Ok(value) => (value, "metadata"),
            Err(ApiError::Status { status: 404, .. }) => (self.get_json("/version")?, "version"),
            Err(err) => return Err(err),
        };
        Ok(HostVersion {
            version: str_field(&value, "version")?,
            revision: value
                .get("revision")
                .and_then(Value::as_str)
                .map(str::to_owned),
            enterprise: value.get("enterprise").and_then(Value::as_bool),
            source,
        })
    }
}
