//! Локальный GitLab CE (план P0.7, «Локальный GitLab CE»): закрепление образа `gitlab/pin`,
//! настройка поднятого контейнера через API GitLab и описание хоста для процессов сценариев.
//!
//! Контейнер поднимает `gitlab/up.sh` и удаляет `run.sh`; настройка — режим бинаря
//! `--gitlab-ce-provision`, по одному разу на подъём:
//! - токен root — OAuth password grant по паролю прогона `GITLAB_ROOT_PASSWORD`; отказ хоста в
//!   гранте — [`ProvisionError::GrantRefused`] (код [`crate::EXIT_ROOT_TOKEN`]): `run.sh` создаёт
//!   личный токен root выражением `gitlab-rails runner` и повторяет настройку с
//!   `PROBE_REVIEW_GITLAB_ROOT_TOKEN`;
//! - root (администратор): версия хоста против закрепления, пользователи автора и ревьюера и их
//!   личные токены (User tokens API self-managed);
//! - автор: приватный проект с README, ревьюер — участник проекта с ролью Developer;
//! - ревьюер: проект виден ему с ролью Developer.
//!
//! Каждый адрес проверяется на `127.0.0.1` до запроса. Записи настройки идут мимо
//! [`super::guard::Guarded`]: проекта до настройки нет, и проверка сторожа отказала бы навсегда.
//! Токены уходят в stdout режима (`run.sh` передаёт их процессам сценариев окружением), описание
//! хоста без секретов — в `gitlab-ce.json` каталога подъёма.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::guard::is_loopback;
use crate::gitlab::{DEVELOPER, Endpoints};
use crate::transport::http::{Credential, Secret};
use crate::transport::{Method, Request, Transport, percent_encode};

/// Закрепление образа, встроенное в бинарь: сборка с другим закреплением — другой хеш сборки.
pub const PIN_TEXT: &str = include_str!("../../gitlab/pin");
pub const IMAGE: &str = "gitlab/gitlab-ce";
pub const PLATFORM: &str = "linux/arm64";

/// Файл описания хоста в каталоге подъёма.
pub const HOST_FILE: &str = "gitlab-ce.json";
/// Путь к [`HOST_FILE`] для процесса сценария.
pub const ENV_HOST: &str = "PROBE_REVIEW_GITLAB_CE";
/// Токены ролей для процесса сценария: JSON [`CeTokens`].
pub const ENV_TOKENS: &str = "PROBE_REVIEW_GITLAB_CE_TOKENS";
/// Пароль root прогона.
pub const ENV_ROOT_PASSWORD: &str = "GITLAB_ROOT_PASSWORD";
/// Личный токен root от `gitlab-rails runner`, если password grant отклонён.
pub const ENV_ROOT_TOKEN: &str = "PROBE_REVIEW_GITLAB_ROOT_TOKEN";

pub const ROOT: &str = "root";
pub const PROJECT: &str = "shellio-review-sandbox";
pub const TOKEN_NAME: &str = "probe-review";
const SCHEMA: u32 = 1;

/// Пользователь, которого заводит настройка.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Person {
    pub username: &'static str,
    pub name: &'static str,
    pub email: &'static str,
}

pub const AUTHOR: Person = Person {
    username: "probe-author",
    name: "Probe Author",
    email: "probe-author@probe-review.test",
};

pub const REVIEWER: Person = Person {
    username: "probe-reviewer",
    name: "Probe Reviewer",
    email: "probe-reviewer@probe-review.test",
};

/// Закрепление образа `gitlab/pin`: строки `ключ = значение`, комментарии `#`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub image: String,
    pub tag: String,
    /// Digest списка манифестов.
    pub index: String,
    /// Digest манифеста `linux/arm64`.
    pub digest: String,
    pub platform: String,
    pub date: String,
}

const PIN_KEYS: [&str; 6] = ["image", "tag", "index", "digest", "platform", "date"];

impl Pin {
    pub fn embedded() -> Result<Pin, String> {
        Pin::parse(PIN_TEXT)
    }

    pub fn parse(text: &str) -> Result<Pin, String> {
        let mut values: [Option<String>; 6] = Default::default();
        for (index, raw) in text.lines().enumerate() {
            let line_no = index + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let err = |reason: String| format!("pin, строка {line_no}: {reason}");
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| err("ждётся «ключ = значение»".to_owned()))?;
            let (key, value) = (key.trim(), value.trim());
            let slot = PIN_KEYS
                .iter()
                .position(|known| *known == key)
                .ok_or_else(|| err(format!("неизвестный ключ «{key}»")))?;
            if values[slot].is_some() {
                return Err(err(format!("повтор ключа «{key}»")));
            }
            if value.is_empty() || value.contains(char::is_whitespace) {
                return Err(err(format!("значение «{key}» пустое или с пробелом")));
            }
            values[slot] = Some(value.to_owned());
        }
        let [image, tag, index, digest, platform, date] = values;
        let take = |value: Option<String>, key: &str| {
            value.ok_or_else(|| format!("pin: нет ключа «{key}»"))
        };
        let pin = Pin {
            image: take(image, "image")?,
            tag: take(tag, "tag")?,
            index: take(index, "index")?,
            digest: take(digest, "digest")?,
            platform: take(platform, "platform")?,
            date: take(date, "date")?,
        };
        pin.validate()?;
        Ok(pin)
    }

    fn validate(&self) -> Result<(), String> {
        if self.image != IMAGE {
            return Err(format!("pin: образ «{}», ждётся {IMAGE}", self.image));
        }
        if self.platform != PLATFORM {
            return Err(format!(
                "pin: платформа «{}», ждётся {PLATFORM}",
                self.platform
            ));
        }
        if !stable_tag(&self.tag) {
            return Err(format!("pin: тег «{}» не стабильный X.Y.Z-ce.0", self.tag));
        }
        for (key, value) in [("index", &self.index), ("digest", &self.digest)] {
            if !sha256_digest(value) {
                return Err(format!("pin: {key} «{value}» не sha256:<64 hex>"));
            }
        }
        if self.index == self.digest {
            return Err("pin: index и digest совпадают".to_owned());
        }
        if !iso_date(&self.date) {
            return Err(format!("pin: дата «{}» не ГГГГ-ММ-ДД", self.date));
        }
        Ok(())
    }

    /// Ссылка на образ для `docker pull` и `docker run`: по digest списка манифестов.
    pub fn reference(&self) -> String {
        format!("{}@{}", self.image, self.index)
    }

    /// Версия GitLab тега: `19.6.1` из `19.6.1-ce.0`.
    pub fn version(&self) -> &str {
        self.tag.strip_suffix("-ce.0").unwrap_or(&self.tag)
    }

    /// Версия, которую сообщил хост, — версия тега: равна ей или продолжается после `-`.
    pub fn matches_version(&self, reported: &str) -> bool {
        let version = self.version();
        reported == version
            || reported
                .strip_prefix(version)
                .is_some_and(|rest| rest.starts_with('-'))
    }
}

fn stable_tag(tag: &str) -> bool {
    let Some(version) = tag.strip_suffix("-ce.0") else {
        return false;
    };
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

fn sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        })
}

/// Учётные данные root для настройки.
#[derive(Debug)]
pub enum RootAuth {
    /// Пароль прогона: OAuth password grant.
    Password(Secret),
    /// Личный токен root от `gitlab-rails runner`.
    Token(Secret),
}

impl RootAuth {
    /// `PROBE_REVIEW_GITLAB_ROOT_TOKEN`, иначе `GITLAB_ROOT_PASSWORD`.
    pub fn from_env() -> Result<RootAuth, String> {
        let value = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        if let Some(token) = value(ENV_ROOT_TOKEN) {
            return Ok(RootAuth::Token(Secret::new(token.trim())));
        }
        value(ENV_ROOT_PASSWORD)
            .map(|password| RootAuth::Password(Secret::new(password)))
            .ok_or_else(|| format!("не задан ни {ENV_ROOT_TOKEN}, ни {ENV_ROOT_PASSWORD}"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionError {
    /// Адрес не на `127.0.0.1`: запроса не было.
    NotLoopback(String),
    /// Хост отказал в password grant: нужен токен root от `gitlab-rails runner`.
    GrantRefused(String),
    /// Шаг настройки не удался: отказ транспорта, код или ответ не той формы.
    Step { step: &'static str, detail: String },
}

impl fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProvisionError::NotLoopback(url) => {
                write!(f, "адрес настройки GitLab CE не 127.0.0.1: {url}")
            }
            ProvisionError::GrantRefused(detail) => {
                write!(f, "password grant отклонён: {detail}")
            }
            ProvisionError::Step { step, detail } => write!(f, "{step}: {detail}"),
        }
    }
}

impl std::error::Error for ProvisionError {}

/// Пользователь, заведённый настройкой.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CeAccount {
    pub id: u64,
    pub username: String,
    pub name: String,
}

/// Описание настроенного хоста для процессов сценариев; секретов в нём нет.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CeHost {
    pub schema: u32,
    /// Корень инсталляции: `http://127.0.0.1:<порт>`.
    pub url: String,
    pub image: String,
    pub tag: String,
    /// Версия по `GET /metadata`.
    pub version: String,
    pub revision: Option<String>,
    pub enterprise: Option<bool>,
    /// `path_with_namespace` проекта песочницы.
    pub project: String,
    pub default_branch: String,
    pub author: CeAccount,
    pub reviewer: CeAccount,
    /// Как получен токен root: `password_grant` или `rails_runner`.
    pub root_token: String,
    /// Адрес, который хост сам сообщает в `web_url` (без `external_url` — имя контейнера), если
    /// он не совпадает с `url`: тоже адрес локального GitLab для очистителя.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_host: Option<String>,
}

impl CeHost {
    pub fn load(path: &Path) -> Result<CeHost, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("{} не читается: {}", path.display(), err.kind()))?;
        let host: CeHost = serde_json::from_str(&text)
            .map_err(|err| format!("{} не разобран: {err}", path.display()))?;
        if host.schema != SCHEMA {
            return Err(format!("{}: схема {}", path.display(), host.schema));
        }
        if !is_loopback(&host.url) {
            return Err(format!("{}: адрес не 127.0.0.1", path.display()));
        }
        Ok(host)
    }

    /// `127.0.0.1:<порт>` для очистителя.
    pub fn address(&self) -> &str {
        self.url
            .strip_prefix("http://")
            .or_else(|| self.url.strip_prefix("https://"))
            .unwrap_or(&self.url)
            .trim_end_matches('/')
    }

    pub fn endpoints(&self) -> Endpoints {
        Endpoints::instance(&self.url)
    }
}

/// Токены ролей; `Debug` без значений.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CeTokens {
    pub author: Secret,
    pub reviewer: Secret,
}

impl CeTokens {
    /// JSON для stdout режима настройки и окружения процесса сценария.
    pub fn to_json(&self) -> String {
        json!({"author": self.author.expose(), "reviewer": self.reviewer.expose()}).to_string()
    }

    pub fn parse(text: &str) -> Result<CeTokens, String> {
        let value: Value =
            serde_json::from_str(text.trim()).map_err(|_| "токены GitLab CE не JSON".to_owned())?;
        let token = |role: &str| {
            value
                .get(role)
                .and_then(Value::as_str)
                .filter(|token| valid_token(token))
                .map(Secret::new)
                .ok_or_else(|| format!("нет токена {role} GitLab CE"))
        };
        Ok(CeTokens {
            author: token("author")?,
            reviewer: token("reviewer")?,
        })
    }
}

fn valid_token(token: &str) -> bool {
    token.len() >= 8 && !token.contains(|c: char| c.is_whitespace() || c.is_control())
}

/// Настройка поднятого GitLab CE. `connect` строит транспорт с учётными данными роли: `None` —
/// без них (password grant).
pub fn provision<T: Transport>(
    url: &str,
    pin: &Pin,
    auth: RootAuth,
    connect: impl Fn(Option<Credential>) -> T,
) -> Result<(CeHost, CeTokens), ProvisionError> {
    let root_url = url.trim_end_matches('/');
    if !is_loopback(root_url) {
        return Err(ProvisionError::NotLoopback(root_url.to_owned()));
    }
    let endpoints = Endpoints::instance(root_url);
    let rest = endpoints.rest.as_str();

    let (root_credential, root_token) = match auth {
        RootAuth::Token(token) => (Credential::private_token(token), "rails_runner"),
        RootAuth::Password(password) => (
            Credential::bearer(password_grant(&connect(None), root_url, &password)?),
            "password_grant",
        ),
    };
    let root = connect(Some(root_credential));
    let me = send(
        &root,
        Request::get(format!("{rest}/user")),
        "GET /user root",
    )?;
    if me.get("username").and_then(Value::as_str) != Some(ROOT)
        || me.get("is_admin").and_then(Value::as_bool) != Some(true)
    {
        return Err(step("GET /user root", "токен не администратора root"));
    }
    let metadata = send(
        &root,
        Request::get(format!("{rest}/metadata")),
        "GET /metadata",
    )?;
    let version = text(&metadata, "version", "GET /metadata")?;
    if !pin.matches_version(&version) {
        return Err(step(
            "GET /metadata",
            &format!(
                "версия хоста {version} не совпадает с закреплённой {}",
                pin.version()
            ),
        ));
    }

    let (author, author_token) = create_user(&root, rest, &AUTHOR)?;
    let (reviewer, reviewer_token) = create_user(&root, rest, &REVIEWER)?;

    let as_author = connect(Some(Credential::private_token(author_token.clone())));
    let project = send(
        &as_author,
        Request::new(Method::Post, format!("{rest}/projects")).json(&json!({
            "name": PROJECT,
            "path": PROJECT,
            "visibility": "private",
            "initialize_with_readme": true,
        })),
        "POST /projects",
    )?;
    let project_id = number(&project, "id", "POST /projects")?;
    let path = text(&project, "path_with_namespace", "POST /projects")?;
    let expected_path = format!("{}/{PROJECT}", AUTHOR.username);
    if !path.eq_ignore_ascii_case(&expected_path) {
        return Err(step(
            "POST /projects",
            &format!("проект создан не в пространстве имён автора: {path}"),
        ));
    }
    if project.get("visibility").and_then(Value::as_str) != Some("private") {
        return Err(step("POST /projects", "проект не приватный"));
    }
    let default_branch = text(&project, "default_branch", "POST /projects")?;

    let member = send(
        &as_author,
        Request::new(
            Method::Post,
            format!("{rest}/projects/{project_id}/members"),
        )
        .json(&json!({"user_id": reviewer.id, "access_level": DEVELOPER})),
        "POST /projects/:id/members",
    )?;
    if member.get("access_level").and_then(Value::as_u64) != Some(u64::from(DEVELOPER)) {
        return Err(step(
            "POST /projects/:id/members",
            "ревьюер добавлен не с ролью Developer",
        ));
    }

    let as_reviewer = connect(Some(Credential::private_token(reviewer_token.clone())));
    let seen = send(
        &as_reviewer,
        Request::get(format!("{rest}/projects/{}", percent_encode(&path))),
        "GET /projects/:id ревьюером",
    )?;
    let level = seen
        .pointer("/permissions/project_access/access_level")
        .and_then(Value::as_u64);
    if level != Some(u64::from(DEVELOPER)) {
        return Err(step(
            "GET /projects/:id ревьюером",
            &format!("роль ревьюера {level:?}, ждётся Developer"),
        ));
    }

    Ok((
        CeHost {
            schema: SCHEMA,
            url: root_url.to_owned(),
            image: pin.reference(),
            tag: pin.tag.clone(),
            version,
            revision: metadata
                .get("revision")
                .and_then(Value::as_str)
                .map(str::to_owned),
            enterprise: metadata.get("enterprise").and_then(Value::as_bool),
            project: path,
            default_branch,
            author,
            reviewer,
            root_token: root_token.to_owned(),
            web_host: project
                .get("web_url")
                .and_then(Value::as_str)
                .and_then(web_authority)
                .filter(|host| {
                    !host.eq_ignore_ascii_case(root_url.split("://").nth(1).unwrap_or_default())
                }),
        },
        CeTokens {
            author: author_token,
            reviewer: reviewer_token,
        },
    ))
}

/// Хост и порт адреса `web_url`: `http://95b8b759b74e/…` → `95b8b759b74e`; также
/// `git@host:…` и `ssh://git@host:порт/…`.
pub fn web_authority(url: &str) -> Option<String> {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url
            .split_once('@')
            .map(|(_, rest)| rest.split(':').next().unwrap_or(rest))?,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or_default();
    (!authority.is_empty()).then(|| authority.to_owned())
}

/// OAuth password grant root: `POST /oauth/token`. 400 и 401 — отказ в гранте.
fn password_grant<T: Transport>(
    anonymous: &T,
    root_url: &str,
    password: &Secret,
) -> Result<Secret, ProvisionError> {
    const STEP: &str = "POST /oauth/token";
    let request = Request::new(Method::Post, format!("{root_url}/oauth/token"))
        .header("accept", "application/json")
        .form(&[
            ("grant_type", "password"),
            ("username", ROOT),
            ("password", password.expose()),
            ("scope", "api"),
        ]);
    let response = deliver(anonymous, &request, STEP)?;
    let body = response.body_json().unwrap_or(Value::Null);
    if matches!(response.status, 400 | 401) {
        let field = |name: &str| body.get(name).and_then(Value::as_str).unwrap_or_default();
        return Err(ProvisionError::GrantRefused(
            format!(
                "код {}: {} {}",
                response.status,
                field("error"),
                field("error_description")
            )
            .trim()
            .to_owned(),
        ));
    }
    if !response.is_success() {
        return Err(step(STEP, &format!("код {}", response.status)));
    }
    body.get("access_token")
        .and_then(Value::as_str)
        .filter(|token| valid_token(token))
        .map(Secret::new)
        .ok_or_else(|| step(STEP, "в ответе нет access_token"))
}

/// Пользователь и его личный токен `api` (User tokens API, только self-managed).
fn create_user<T: Transport>(
    root: &T,
    rest: &str,
    person: &Person,
) -> Result<(CeAccount, Secret), ProvisionError> {
    let user = send(
        root,
        Request::new(Method::Post, format!("{rest}/users")).json(&json!({
            "username": person.username,
            "name": person.name,
            "email": person.email,
            "force_random_password": true,
            "skip_confirmation": true,
        })),
        "POST /users",
    )?;
    if user.get("username").and_then(Value::as_str) != Some(person.username) {
        return Err(step("POST /users", "в ответе другой username"));
    }
    let id = number(&user, "id", "POST /users")?;
    let token = send(
        root,
        Request::new(
            Method::Post,
            format!("{rest}/users/{id}/personal_access_tokens"),
        )
        .json(&json!({"name": TOKEN_NAME, "scopes": ["api"]})),
        "POST /users/:id/personal_access_tokens",
    )?;
    let secret = token
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| valid_token(token))
        .map(Secret::new)
        .ok_or_else(|| {
            step(
                "POST /users/:id/personal_access_tokens",
                "в ответе нет token",
            )
        })?;
    Ok((
        CeAccount {
            id,
            username: person.username.to_owned(),
            name: person.name.to_owned(),
        },
        secret,
    ))
}

fn step(step: &'static str, detail: &str) -> ProvisionError {
    ProvisionError::Step {
        step,
        detail: detail.to_owned(),
    }
}

/// Запрос с проверкой адреса; ответ любого кода.
fn deliver<T: Transport>(
    transport: &T,
    request: &Request,
    name: &'static str,
) -> Result<crate::transport::Response, ProvisionError> {
    if !is_loopback(&request.url) {
        return Err(ProvisionError::NotLoopback(request.path().to_owned()));
    }
    transport
        .send(request)
        .map_err(|failure| step(name, &failure.to_string()))
}

/// Запрос JSON: адрес на `127.0.0.1`, ответ 2xx с телом JSON.
fn send<T: Transport>(
    transport: &T,
    request: Request,
    name: &'static str,
) -> Result<Value, ProvisionError> {
    let request = request.header("accept", "application/json");
    let response = deliver(transport, &request, name)?;
    if !response.is_success() {
        let body = response.body_json().unwrap_or(Value::Null);
        let message = match body.get("message").or_else(|| body.get("error")) {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        return Err(step(name, &format!("код {}: {message}", response.status)));
    }
    response.body_json().map_err(|err| step(name, &err))
}

fn text(value: &Value, field: &str, name: &'static str) -> Result<String, ProvisionError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| step(name, &format!("в ответе нет {field}")))
}

fn number(value: &Value, field: &str, name: &'static str) -> Result<u64, ProvisionError> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| step(name, &format!("в ответе нет числа {field}")))
}

#[cfg(test)]
mod tests;
