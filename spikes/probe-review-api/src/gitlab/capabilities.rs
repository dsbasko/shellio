//! Возможности GitLab, определённые на хосте чтением, а не по имени хоста.
//!
//! Чтения: `GET /user` (логин), `GET /metadata` (версия, ревизия, `enterprise`; 404 у старых
//! версий — `GET /version` без редакции), `GET /projects/:id` (видимость, роль токена — большая из
//! `project_access` и `group_access`), `GET /namespaces/:id` (тариф `plan`, если виден токену:
//! 403 и 404 — тариф не виден; тогда `plan` из ответа `GET /metadata`, если хост его дал) и
//! introspection `__type(name: "Mutation")` GraphQL (наличие `mergeRequestRequestChanges`).
//!
//! На `__type` хост может ответить полной интроспекцией `data.__schema` (GitLab CE 19.6.1 в первом
//! живом прогоне P0.7): тогда мутации берутся из типа `mutationType.name` в `types`. Ответ без
//! `__type` и без `__schema` — [`ApiError::Unexpected`].

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{ACCEPT, ApiError, DEVELOPER, GitLab, json_of, str_field};
use crate::transport::{Method, Request, Transport};

pub const REQUEST_CHANGES: &str = "mergeRequestRequestChanges";

/// Мутации, которые нужны адаптеру; остальные поля `Mutation` не записываются.
pub const MUTATIONS: [&str; 1] = [REQUEST_CHANGES];

pub(super) const INTROSPECT: &str =
    "query($name: String!) { __type(name: $name) { fields { name } } }";

/// Та же introspection с литералом имени и без `variables`: наблюдение формы ответа.
pub const INTROSPECT_LITERAL: &str = "{ __type(name: \"Mutation\") { fields { name } } }";

/// Форма ответа introspection: `__type` по спецификации или полная схема `__schema`.
pub const FORM_TYPE: &str = "__type";
pub const FORM_SCHEMA: &str = "__schema";

/// Ответ introspection без разбора в возможности: форма, размер тела и наличие мутации.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntrospectionReply {
    /// [`FORM_TYPE`], [`FORM_SCHEMA`], `errors` или `другая`.
    pub form: &'static str,
    pub bytes: usize,
    /// `None` — по ответу не определяется.
    pub request_changes: Option<bool>,
}

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
    /// Тариф, если виден токену: `plan` пространства имён проекта, иначе `plan` из `GET /metadata`.
    pub plan: Option<String>,
    /// Откуда взят тариф: `namespace` или `metadata`.
    pub plan_source: Option<&'static str>,
    pub visibility: String,
    /// Большая из ролей `project_access` и `group_access`; `None` — роли нет.
    pub access_level: Option<u8>,
    /// Мутации из [`MUTATIONS`], которые есть в схеме, в порядке [`MUTATIONS`].
    pub mutations: Vec<String>,
    /// Форма ответа introspection: [`FORM_TYPE`] или [`FORM_SCHEMA`].
    pub introspection: &'static str,
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
            "plan_source": self.plan_source,
            "visibility": self.visibility,
            "access_level": self.access_level,
            "mutations": self.mutations,
            "introspection": self.introspection,
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
        let (host, metadata_plan) = self.host_version()?;
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
        let namespace_plan = match project.pointer("/namespace/id").and_then(Value::as_u64) {
            Some(namespace) => match self.get_json(&format!("/namespaces/{namespace}")) {
                Ok(value) => value.get("plan").and_then(Value::as_str).map(str::to_owned),
                Err(ApiError::Status {
                    status: 403 | 404, ..
                }) => None,
                Err(err) => return Err(err),
            },
            None => None,
        };
        let (plan, plan_source) = match (namespace_plan, metadata_plan) {
            (Some(plan), _) => (Some(plan), Some("namespace")),
            (None, Some(plan)) => (Some(plan), Some("metadata")),
            (None, None) => (None, None),
        };
        let data = self.graphql(INTROSPECT, json!({"name": "Mutation"}))?;
        let (introspection, present) = mutation_fields(&data)?;
        let mutations = MUTATIONS
            .iter()
            .filter(|name| present.contains(name))
            .map(|name| (*name).to_owned())
            .collect();
        Ok(Capabilities {
            username,
            host,
            plan,
            plan_source,
            visibility,
            access_level,
            mutations,
            introspection,
        })
    }

    /// Introspection [`INTROSPECT_LITERAL`] без `variables`: форма ответа и размер тела для
    /// наблюдения. Ошибки GraphQL — форма `errors`, а не отказ: разбор в возможности идёт только
    /// через [`GitLab::capabilities`].
    pub fn introspect_literal(&self) -> Result<IntrospectionReply, ApiError> {
        let request = Request::new(Method::Post, self.endpoints.graphql.clone())
            .header("accept", ACCEPT)
            .json(&json!({"query": INTROSPECT_LITERAL}));
        let response = self.call(&request)?;
        let body = json_of(&response)?;
        Ok(reply_of(&body, response.body.len()))
    }

    /// Версия хоста и `plan` из `GET /metadata`, если хост его дал (CE 19.6.1 даёт `free`).
    fn host_version(&self) -> Result<(HostVersion, Option<String>), ApiError> {
        let (value, source) = match self.get_json("/metadata") {
            Ok(value) => (value, "metadata"),
            Err(ApiError::Status { status: 404, .. }) => (self.get_json("/version")?, "version"),
            Err(err) => return Err(err),
        };
        let plan = match source {
            "metadata" => value.get("plan").and_then(Value::as_str).map(str::to_owned),
            _ => None,
        };
        let host = HostVersion {
            version: str_field(&value, "version")?,
            revision: value
                .get("revision")
                .and_then(Value::as_str)
                .map(str::to_owned),
            enterprise: value.get("enterprise").and_then(Value::as_bool),
            source,
        };
        Ok((host, plan))
    }
}

fn field_names(fields: &Value) -> Option<Vec<&str>> {
    Some(
        fields
            .as_array()?
            .iter()
            .filter_map(|field| field.get("name").and_then(Value::as_str))
            .collect(),
    )
}

/// Имена полей типа `Mutation` из `data` ответа introspection и форма ответа: `__type.fields` или
/// тип `mutationType.name` в `__schema.types`. Схема без `mutationType`, без такого типа или без
/// его `fields` — [`ApiError::Unexpected`], а не «мутации нет».
pub fn mutation_fields(data: &Map<String, Value>) -> Result<(&'static str, Vec<&str>), ApiError> {
    if let Some(found) = data.get("__type").filter(|value| !value.is_null()) {
        let fields = found
            .get("fields")
            .and_then(field_names)
            .ok_or_else(|| ApiError::Unexpected("__type(Mutation) без fields".to_owned()))?;
        return Ok((FORM_TYPE, fields));
    }
    let Some(schema) = data.get("__schema").filter(|value| !value.is_null()) else {
        return Err(ApiError::Unexpected(
            "ответ introspection без __type и без __schema".to_owned(),
        ));
    };
    let mutation_type = schema
        .pointer("/mutationType/name")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::Unexpected("__schema без mutationType.name".to_owned()))?;
    let types = schema
        .get("types")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::Unexpected("__schema без types".to_owned()))?;
    let fields = types
        .iter()
        .find(|ty| ty.get("name").and_then(Value::as_str) == Some(mutation_type))
        .and_then(|ty| ty.get("fields"))
        .and_then(field_names)
        .ok_or_else(|| {
            ApiError::Unexpected(format!("в __schema.types нет fields типа {mutation_type}"))
        })?;
    Ok((FORM_SCHEMA, fields))
}

/// Форма и наличие [`REQUEST_CHANGES`] по телу ответа introspection любой формы.
pub fn reply_of(body: &Value, bytes: usize) -> IntrospectionReply {
    let errors = body
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty());
    let data = body.get("data").and_then(Value::as_object);
    let parsed = data.map(mutation_fields);
    let form = match (&parsed, errors) {
        (Some(Ok((form, _))), _) => *form,
        (_, true) => "errors",
        _ => "другая",
    };
    IntrospectionReply {
        form,
        bytes,
        request_changes: match parsed {
            Some(Ok((_, fields))) => Some(fields.contains(&REQUEST_CHANGES)),
            _ => None,
        },
    }
}
