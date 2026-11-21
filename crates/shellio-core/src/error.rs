//! Ошибки ядра со стабильными кодами.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ids::{ProjectId, WorkspaceId};
use crate::root::{RootError, RootKey};
use crate::title::{Title, TitleError};

/// Стабильный код ошибки ядра. Коды не переименовываются: новый смысл получает новый код.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Неверный аргумент команды. Причина — в `details.reason`.
    InvalidArgument,
    /// Записи нет или она удалена.
    NotFound,
    /// Имя уже занято в своей области.
    NameTaken,
    /// Корень уже принадлежит проекту.
    RootTaken,
    /// Запись в хранилище не удалась, повтор возможен.
    StorageUnavailable,
    /// Хранилище разошлось с ядром: в базу писал кто-то ещё.
    StorageConflict,
    /// Очередь команд полна, повтор возможен.
    Busy,
    /// Ядро завершается и новых запросов не принимает.
    ShuttingDown,
}

impl ErrorCode {
    /// Все коды.
    pub const ALL: [Self; 8] = [
        Self::InvalidArgument,
        Self::NotFound,
        Self::NameTaken,
        Self::RootTaken,
        Self::StorageUnavailable,
        Self::StorageConflict,
        Self::Busy,
        Self::ShuttingDown,
    ];

    /// Имя кода в JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::NotFound => "not_found",
            Self::NameTaken => "name_taken",
            Self::RootTaken => "root_taken",
            Self::StorageUnavailable => "storage_unavailable",
            Self::StorageConflict => "storage_conflict",
            Self::Busy => "busy",
            Self::ShuttingDown => "shutting_down",
        }
    }

    /// Имеет ли смысл повторить тот же запрос позже.
    pub const fn retryable(self) -> bool {
        match self {
            Self::StorageUnavailable | Self::Busy => true,
            Self::InvalidArgument
            | Self::NotFound
            | Self::NameTaken
            | Self::RootTaken
            | Self::StorageConflict
            | Self::ShuttingDown => false,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Вид записи в деталях `not_found` и `name_taken`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Entity {
    Workspace,
    Project,
    Workbench,
}

impl Entity {
    /// Имя вида в деталях ошибки.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Project => "project",
            Self::Workbench => "workbench",
        }
    }

    const fn noun(self) -> &'static str {
        match self {
            Self::Workspace => "воркспейс",
            Self::Project => "проект",
            Self::Workbench => "рабочая область",
        }
    }

    const fn not_found_suffix(self) -> &'static str {
        match self {
            Self::Workspace | Self::Project => "не найден",
            Self::Workbench => "не найдена",
        }
    }
}

/// Ошибка ядра: стабильный код, сообщение для человека, признак повтора и детали.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreError {
    code: ErrorCode,
    message: String,
    retryable: bool,
    details: Map<String, Value>,
}

impl CoreError {
    fn new(code: ErrorCode, message: String, details: Map<String, Value>) -> Self {
        Self {
            code,
            message,
            retryable: code.retryable(),
            details,
        }
    }

    fn details<const N: usize>(pairs: [(&str, Value); N]) -> Map<String, Value> {
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect()
    }

    /// Неверный аргумент `field` со стабильной причиной `reason`.
    pub fn invalid_argument(field: &str, reason: &str, message: impl Into<String>) -> Self {
        Self::new(
            ErrorCode::InvalidArgument,
            message.into(),
            Self::details([("field", field.into()), ("reason", reason.into())]),
        )
    }

    /// Неверное имя в поле `field`.
    pub fn invalid_title(field: &str, error: &TitleError) -> Self {
        let reason = match error {
            TitleError::Empty => "title_empty",
            TitleError::TooLong { .. } => "title_too_long",
            TitleError::ControlChar { .. } => "title_control_char",
        };
        Self::invalid_argument(field, reason, error.to_string())
    }

    /// У родителя `scope` не осталось позиции после наибольшей.
    pub fn position_exhausted(scope: &str) -> Self {
        Self::new(
            ErrorCode::InvalidArgument,
            format!("в «{scope}» нет позиции после наибольшей"),
            Self::details([
                ("reason", "position_exhausted".into()),
                ("scope", scope.into()),
            ]),
        )
    }

    /// Записи `id` вида `entity` нет.
    pub fn not_found(entity: Entity, id: impl fmt::Display) -> Self {
        let id = id.to_string();
        Self::new(
            ErrorCode::NotFound,
            format!("{} {id} {}", entity.noun(), entity.not_found_suffix()),
            Self::details([("entity", entity.as_str().into()), ("id", id.into())]),
        )
    }

    /// Имя `name` в области занято записью `existing`.
    pub fn name_taken(entity: Entity, name: &Title, existing: impl fmt::Display) -> Self {
        let existing = existing.to_string();
        Self::new(
            ErrorCode::NameTaken,
            format!("имя «{name}» уже занято: {} {existing}", entity.noun()),
            Self::details([
                ("entity", entity.as_str().into()),
                ("name", name.as_str().into()),
                ("existing", existing.into()),
            ]),
        )
    }

    /// Корень с ключом `key` уже принадлежит проекту `project` воркспейса `workspace`.
    pub fn root_taken(key: &RootKey, project: ProjectId, workspace: WorkspaceId) -> Self {
        Self::new(
            ErrorCode::RootTaken,
            format!("корень {key} уже принадлежит проекту {project} воркспейса {workspace}"),
            Self::details([
                ("project", project.to_string().into()),
                ("workspace", workspace.to_string().into()),
            ]),
        )
    }

    /// Запись в хранилище не удалась.
    pub fn storage_unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::StorageUnavailable, message.into(), Map::new())
    }

    /// Хранилище разошлось с ядром.
    pub fn storage_conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::StorageConflict, message.into(), Map::new())
    }

    /// Очередь команд полна.
    pub fn busy() -> Self {
        Self::new(
            ErrorCode::Busy,
            "очередь команд ядра полна".to_owned(),
            Map::new(),
        )
    }

    /// Ядро завершается.
    pub fn shutting_down() -> Self {
        Self::new(
            ErrorCode::ShuttingDown,
            "ядро завершается".to_owned(),
            Map::new(),
        )
    }

    /// Стабильный код.
    pub const fn code(&self) -> ErrorCode {
        self.code
    }

    /// Сообщение для человека.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Имеет ли смысл повторить запрос.
    pub const fn retryable(&self) -> bool {
        self.retryable
    }

    /// Деталь `key`, если она есть.
    pub fn detail(&self, key: &str) -> Option<&Value> {
        self.details.get(key)
    }

    /// Строковая деталь `key`, если она есть.
    pub fn detail_str(&self, key: &str) -> Option<&str> {
        self.detail(key).and_then(Value::as_str)
    }
}

impl From<RootError> for CoreError {
    fn from(error: RootError) -> Self {
        let mut details =
            Self::details([("field", "path".into()), ("reason", error.reason().into())]);
        details.insert(
            "path".to_owned(),
            error.path().to_string_lossy().into_owned().into(),
        );
        Self::new(ErrorCode::InvalidArgument, error.to_string(), details)
    }
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CoreError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{IdGen, SequentialIds};

    #[test]
    fn codes_have_stable_names_and_retry_flags() {
        let expected = [
            ("invalid_argument", false),
            ("not_found", false),
            ("name_taken", false),
            ("root_taken", false),
            ("storage_unavailable", true),
            ("storage_conflict", false),
            ("busy", true),
            ("shutting_down", false),
        ];
        for (code, (name, retryable)) in ErrorCode::ALL.into_iter().zip(expected) {
            assert_eq!(code.as_str(), name);
            assert_eq!(code.to_string(), name);
            assert_eq!(code.retryable(), retryable, "{name}");
            assert_eq!(serde_json::to_value(code).unwrap(), Value::from(name));
        }
    }

    #[test]
    fn constructors_set_code_and_retry_flag() {
        let mut ids = SequentialIds::new();
        let ids: &mut dyn IdGen = &mut ids;
        let workspace = WorkspaceId::generate(ids);
        let project = ProjectId::generate(ids);
        let title = Title::new("Работа").unwrap();
        let cases = [
            (
                CoreError::invalid_argument("title", "title_empty", "x"),
                ErrorCode::InvalidArgument,
            ),
            (
                CoreError::not_found(Entity::Workspace, workspace),
                ErrorCode::NotFound,
            ),
            (
                CoreError::name_taken(Entity::Workspace, &title, workspace),
                ErrorCode::NameTaken,
            ),
            (
                CoreError::root_taken(&RootKey::new("/r/.git"), project, workspace),
                ErrorCode::RootTaken,
            ),
            (
                CoreError::storage_unavailable("диск"),
                ErrorCode::StorageUnavailable,
            ),
            (
                CoreError::storage_conflict("ревизия"),
                ErrorCode::StorageConflict,
            ),
            (CoreError::busy(), ErrorCode::Busy),
            (CoreError::shutting_down(), ErrorCode::ShuttingDown),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(error.retryable(), code.retryable());
            assert!(!error.message().is_empty());
            assert!(error.to_string().starts_with(code.as_str()));
        }
    }

    #[test]
    fn details_name_the_subject() {
        let mut ids = SequentialIds::new();
        let ids: &mut dyn IdGen = &mut ids;
        let workspace = WorkspaceId::generate(ids);
        let project = ProjectId::generate(ids);

        let error = CoreError::root_taken(&RootKey::new("/r/.git"), project, workspace);
        assert_eq!(
            error.detail_str("project"),
            Some(project.to_string().as_str())
        );
        assert_eq!(
            error.detail_str("workspace"),
            Some(workspace.to_string().as_str())
        );

        let error = CoreError::not_found(Entity::Project, project);
        assert_eq!(error.detail_str("entity"), Some("project"));
        assert_eq!(error.detail_str("id"), Some(project.to_string().as_str()));

        let error = CoreError::name_taken(
            Entity::Workbench,
            &Title::new("Основная").unwrap(),
            workspace,
        );
        assert_eq!(error.detail_str("entity"), Some("workbench"));
        assert_eq!(error.detail_str("name"), Some("Основная"));
        assert_eq!(
            error.detail_str("existing"),
            Some(workspace.to_string().as_str())
        );
    }

    #[test]
    fn title_errors_map_to_stable_reasons() {
        let cases = [
            (TitleError::Empty, "title_empty"),
            (TitleError::TooLong { chars: 201 }, "title_too_long"),
            (
                TitleError::ControlChar { ch: '\u{7}' },
                "title_control_char",
            ),
        ];
        for (error, reason) in cases {
            let error = CoreError::invalid_title("title", &error);
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert_eq!(error.detail_str("field"), Some("title"));
            assert_eq!(error.detail_str("reason"), Some(reason));
        }
    }

    #[test]
    fn root_errors_map_to_invalid_argument_with_reason() {
        let cases = [
            RootError::NotAbsolute { path: "rel".into() },
            RootError::NotFound {
                path: "/missing".into(),
            },
            RootError::NotADirectory {
                path: "/file".into(),
            },
        ];
        for root_error in cases {
            let reason = root_error.reason();
            let path = root_error.path().to_string_lossy().into_owned();
            let error = CoreError::from(root_error);
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert!(!error.retryable());
            assert_eq!(error.detail_str("field"), Some("path"));
            assert_eq!(error.detail_str("reason"), Some(reason));
            assert_eq!(error.detail_str("path"), Some(path.as_str()));
        }
    }
}
