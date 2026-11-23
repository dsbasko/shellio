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
    /// Команде нужно место, а размера окна у ядра нет.
    NoUi,
}

impl ErrorCode {
    /// Все коды.
    pub const ALL: [Self; 9] = [
        Self::InvalidArgument,
        Self::NotFound,
        Self::NameTaken,
        Self::RootTaken,
        Self::StorageUnavailable,
        Self::StorageConflict,
        Self::Busy,
        Self::ShuttingDown,
        Self::NoUi,
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
            Self::NoUi => "no_ui",
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
            | Self::ShuttingDown
            | Self::NoUi => false,
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
    Panel,
}

impl Entity {
    /// Имя вида в деталях ошибки.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Project => "project",
            Self::Workbench => "workbench",
            Self::Panel => "panel",
        }
    }

    const fn noun(self) -> &'static str {
        match self {
            Self::Workspace => "воркспейс",
            Self::Project => "проект",
            Self::Workbench => "рабочая область",
            Self::Panel => "панель",
        }
    }

    const fn not_found_suffix(self) -> &'static str {
        match self {
            Self::Workspace | Self::Project => "не найден",
            Self::Workbench | Self::Panel => "не найдена",
        }
    }
}

/// Ошибка ядра: стабильный код, сообщение для человека, признак повтора и детали.
///
/// Признак повтора следует из кода: разбор JSON с другим признаком отказывает.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireCoreError")]
pub struct CoreError {
    code: ErrorCode,
    message: String,
    retryable: bool,
    details: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCoreError {
    code: ErrorCode,
    message: String,
    retryable: bool,
    details: Map<String, Value>,
}

impl TryFrom<WireCoreError> for CoreError {
    type Error = String;

    fn try_from(wire: WireCoreError) -> Result<Self, Self::Error> {
        if wire.retryable != wire.code.retryable() {
            return Err(format!(
                "retryable {} не соответствует коду {}",
                wire.retryable, wire.code
            ));
        }
        Ok(Self::new(wire.code, wire.message, wire.details))
    }
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
        Self::invalid_argument(field, error.reason(), error.to_string())
    }

    /// Команда `command` выполняется только в области всех воркспейсов.
    pub fn scope_requires_all(command: &str) -> Self {
        Self::invalid_argument(
            "scope",
            "scope_requires_all",
            format!("{command} выполняется только в области всех воркспейсов"),
        )
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

    /// Открытая рабочая область окна вне области доступа: отвечает как несуществующая, но её
    /// идентификатор не раскрывается (`id: null`).
    pub fn open_workbench_hidden() -> Self {
        Self::new(
            ErrorCode::NotFound,
            "открытая рабочая область не найдена".to_owned(),
            Self::details([
                ("entity", Entity::Workbench.as_str().into()),
                ("id", Value::Null),
            ]),
        )
    }

    /// Отказ по состоянию: причина `reason` поля `field` с собственными деталями.
    fn refused<const N: usize>(
        field: &str,
        reason: &str,
        message: String,
        extra: [(&str, Value); N],
    ) -> Self {
        let mut details = Self::details([("field", field.into()), ("reason", reason.into())]);
        details.extend(
            extra
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value)),
        );
        Self::new(ErrorCode::InvalidArgument, message, details)
    }

    /// Нет ни явной, ни открытой рабочей области.
    pub fn no_open_workbench() -> Self {
        Self::refused(
            "workbench",
            "no_open_workbench",
            "рабочая область не указана и в окне не открыта".to_owned(),
            [],
        )
    }

    /// Деление одиночной вкладки `panel` без новой панели.
    pub fn split_needs_panel(panel: impl fmt::Display) -> Self {
        let panel = panel.to_string();
        Self::refused(
            "open",
            "split_needs_panel",
            format!("у части панели {panel} одна вкладка: новой части нужна панель"),
            [("panel", panel.into())],
        )
    }

    /// Затронутым частям деления не хватит места. `parts` — `[{panel, need, have}]`.
    pub fn split_no_room(parts: Value, message: String) -> Self {
        Self::refused("panel", "split_no_room", message, [("parts", parts)])
    }

    /// Панель `panel` стоит в колонке навигатора, а операции нужна часть зоны. Деление —
    /// `navigator_not_splittable`, якорь и `part_of` — `navigator_column`.
    pub fn navigator_column(field: &str, panel: impl fmt::Display) -> Self {
        let panel = panel.to_string();
        let reason = if field == "panel" {
            "navigator_not_splittable"
        } else {
            "navigator_column"
        };
        Self::refused(
            field,
            reason,
            format!("панель {panel} стоит в колонке навигатора"),
            [("panel", panel.into())],
        )
    }

    /// Зона `zone` (поле `field` — `zone` или `target`) пуста.
    pub fn zone_empty(field: &str, name: &str) -> Self {
        Self::refused(
            field,
            "zone_empty",
            format!("{name}: нет панелей"),
            [(field, name.into())],
        )
    }

    /// Размер вне границ `min..=max` в единицах запроса.
    pub fn resize_out_of_range(min: u32, max: u32) -> Self {
        Self::refused(
            "size",
            "resize_out_of_range",
            format!("размер вне границ {min}..={max}"),
            [("min", min.into()), ("max", max.into())],
        )
    }

    /// У зоны `zone` нет своего размера: размер центра — остаток.
    pub fn zone_not_resizable(zone: &str) -> Self {
        Self::refused(
            "zone",
            "zone_not_resizable",
            format!("у зоны {zone} нет своего размера: размер центра — остаток"),
            [("zone", zone.into())],
        )
    }

    /// Панель `panel` поля `part_of` стоит не в зоне `zone`.
    pub fn panel_not_in_zone(panel: impl fmt::Display, zone: &str) -> Self {
        let panel = panel.to_string();
        Self::refused(
            "part_of",
            "panel_not_in_zone",
            format!("панель {panel} стоит не в зоне {zone}"),
            [("panel", panel.into()), ("zone", zone.into())],
        )
    }

    /// Неверное сочетание полей команды: `field` лишнее или нужное отсутствует.
    pub fn arguments_conflict(field: &str, message: impl Into<String>) -> Self {
        Self::invalid_argument(field, "arguments_conflict", message)
    }

    /// Команде нужно место, а размера окна у ядра нет.
    pub fn no_ui(command: &str) -> Self {
        Self::new(
            ErrorCode::NoUi,
            format!("{command}: размер окна неизвестен, окна нет"),
            Map::new(),
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
            ("no_ui", false),
        ];
        for (code, (name, retryable)) in ErrorCode::ALL.into_iter().zip(expected) {
            assert_eq!(code.as_str(), name);
            assert_eq!(code.to_string(), name);
            assert_eq!(code.retryable(), retryable, "{name}");
            assert_eq!(serde_json::to_value(code).unwrap(), Value::from(name));
        }
    }

    #[test]
    fn parse_rejects_retry_flag_other_than_code() {
        let busy = CoreError::busy();
        let mut json = serde_json::to_value(&busy).unwrap();
        assert_eq!(
            serde_json::from_value::<CoreError>(json.clone()).unwrap(),
            busy
        );
        json["retryable"] = Value::Bool(false);
        let error = serde_json::from_value::<CoreError>(json).unwrap_err();
        assert!(error.to_string().contains("retryable"), "{error}");
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
            (CoreError::no_ui("layout.split"), ErrorCode::NoUi),
            (CoreError::no_open_workbench(), ErrorCode::InvalidArgument),
            (CoreError::open_workbench_hidden(), ErrorCode::NotFound),
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
    fn layout_refusals_name_field_reason_and_details() {
        let cases = [
            (
                CoreError::no_open_workbench(),
                "workbench",
                "no_open_workbench",
            ),
            (
                CoreError::split_needs_panel("p"),
                "open",
                "split_needs_panel",
            ),
            (
                CoreError::split_no_room(Value::Array(Vec::new()), "x".into()),
                "panel",
                "split_no_room",
            ),
            (
                CoreError::navigator_column("panel", "p"),
                "panel",
                "navigator_not_splittable",
            ),
            (
                CoreError::navigator_column("anchor", "p"),
                "anchor",
                "navigator_column",
            ),
            (CoreError::zone_empty("zone", "right"), "zone", "zone_empty"),
            (
                CoreError::zone_empty("target", "plate"),
                "target",
                "zone_empty",
            ),
            (
                CoreError::resize_out_of_range(1, 2),
                "size",
                "resize_out_of_range",
            ),
            (
                CoreError::zone_not_resizable("center"),
                "zone",
                "zone_not_resizable",
            ),
            (
                CoreError::panel_not_in_zone("p", "right"),
                "part_of",
                "panel_not_in_zone",
            ),
            (
                CoreError::arguments_conflict("panel", "x"),
                "panel",
                "arguments_conflict",
            ),
        ];
        for (error, field, reason) in cases {
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{error}");
            assert_eq!(error.detail_str("field"), Some(field), "{error}");
            assert_eq!(error.detail_str("reason"), Some(reason), "{error}");
        }
        let range = CoreError::resize_out_of_range(200, 640);
        assert_eq!(range.detail("min"), Some(&Value::from(200)));
        assert_eq!(range.detail("max"), Some(&Value::from(640)));
        assert_eq!(
            CoreError::zone_empty("target", "plate").detail_str("target"),
            Some("plate")
        );

        let hidden = CoreError::open_workbench_hidden();
        assert_eq!(hidden.detail_str("entity"), Some("workbench"));
        assert_eq!(hidden.detail("id"), Some(&Value::Null));
        let panel = CoreError::not_found(Entity::Panel, "p");
        assert_eq!(panel.detail_str("entity"), Some("panel"));
        assert_eq!(panel.message(), "панель p не найдена");
        assert!(CoreError::no_ui("layout.split").details.is_empty());
    }

    #[test]
    fn scope_requires_all_names_scope_field() {
        let error = CoreError::scope_requires_all("workspace.create");
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(!error.retryable());
        assert_eq!(error.detail_str("field"), Some("scope"));
        assert_eq!(error.detail_str("reason"), Some("scope_requires_all"));
        assert!(error.message().contains("workspace.create"), "{error}");
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
