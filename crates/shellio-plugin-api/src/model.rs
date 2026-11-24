//! Контракт модели панели и её стёртая обёртка (ADR-0008, «Уточнено в P1.5 (планирование)»).
//!
//! Модель — чистая функция над сериализуемым состоянием: команд она не шлёт и побочных эффектов не
//! имеет. Хост хранит данные панели исходным текстом JSON и работает с моделью через [`DynModel`].

use std::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use shellio_layout::{Geometry, Rect};

use crate::manifest::Manifest;

/// Наибольший размер данных панели в байтах UTF-8 JSON.
pub const STATE_LIMIT: usize = 65_536;

/// Отказ модели или её обёртки. Хост отдаёт его как `invalid_argument` с причиной `reason`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Поле запроса: `name`, `args` или поле внутри `args`.
    pub field: String,
    /// Стабильная причина.
    pub reason: String,
    /// Сообщение для человека.
    pub message: String,
    pub details: Map<String, Value>,
}

impl Refusal {
    /// Имени нет среди действий манифеста.
    pub const COMMAND_UNKNOWN: &'static str = "command_unknown";
    /// `args` не разобраны типом модели.
    pub const ARGS_INVALID: &'static str = "args_invalid";
    /// Результат модели больше [`STATE_LIMIT`]. Детали — `size` и `limit` в байтах.
    pub const STATE_TOO_LARGE: &'static str = "panel_state_too_large";
    /// Сохранённые данные не разобраны типом состояния модели вместе с его инвариантами.
    pub const STATE_CORRUPT: &'static str = "state_corrupt";
    /// Результат модели не сериализуется в JSON.
    pub const STATE_UNSERIALIZABLE: &'static str = "state_unserializable";

    pub fn new(field: &str, reason: &str, message: impl Into<String>) -> Self {
        Self {
            field: field.to_owned(),
            reason: reason.to_owned(),
            message: message.into(),
            details: Map::new(),
        }
    }

    #[must_use]
    pub fn with_detail(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.details.insert(key.to_owned(), value.into());
        self
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refusal {}

/// Именованный кадр внутри тела панели: начало координат — левый верхний угол тела.
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub name: &'static str,
    pub frame: Rect,
}

/// Как действие достижимо с клавиатуры в этом представлении.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Via {
    /// Прямо в показанном представлении.
    Direct,
    /// Через раскрытие вглубь.
    Drill,
    /// Через `zoom` панели.
    Zoom,
}

/// Достижимость действия манифеста.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reach {
    /// Полное имя действия манифеста.
    pub action: &'static str,
    pub via: Via,
}

/// Представление панели для геометрии части зоны.
#[derive(Clone, Debug, PartialEq)]
pub struct Presentation {
    /// Вариант представления: `drill`, `split`, `bar`, …
    pub variant: &'static str,
    pub regions: Vec<Region>,
    /// Каждое действие манифеста ровно один раз.
    pub reach: Vec<Reach>,
}

/// Модель панели плагина.
///
/// `Args` и `Command` объявляют `deny_unknown_fields`. `Command` — перечисление с
/// `tag = "name", content = "args"`, имена вариантов — полные имена действий манифеста. Действие без
/// аргументов — вариант с пустыми фигурными скобками (`Back {}`): хост подставляет вместо
/// отсутствующих `args` пустой объект.
pub trait PanelModel: Send + Sync + 'static {
    /// Данные панели. Разбор проверяет инварианты: данные, которые их нарушают, — `state_corrupt`.
    type State: Serialize + DeserializeOwned + Clone + PartialEq;
    /// Контекст `open` и `reopen`.
    type Args: DeserializeOwned;
    type Command: DeserializeOwned;

    fn manifest(&self) -> &'static Manifest;
    fn init(&self, args: Option<Self::Args>) -> Result<Self::State, Refusal>;
    fn reopen(&self, state: &Self::State, args: Option<Self::Args>)
    -> Result<Self::State, Refusal>;
    fn apply(&self, state: &Self::State, command: Self::Command) -> Result<Self::State, Refusal>;
    /// Данные версии `from`, меньшей `state_version` манифеста, в данные текущей версии.
    fn migrate(&self, from: u32, data: Value) -> Result<Self::State, Refusal>;
    /// Зона разбирается исчерпывающим `match` без `_`.
    fn adapt(&self, geometry: &Geometry, state: &Self::State) -> Presentation;
}

/// Итог `reopen` и команды над сохранёнными данными.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Состояние прежнее: записи и ревизии нет.
    Unchanged,
    /// Новые данные текстом JSON не длиннее [`STATE_LIMIT`].
    Changed(String),
}

trait Erased: Send + Sync {
    fn manifest(&self) -> &'static Manifest;
    fn init(&self, args: Option<Value>) -> Result<String, Refusal>;
    fn reopen(&self, state: &str, args: Option<Value>) -> Result<Step, Refusal>;
    fn command(&self, state: &str, name: &str, args: Option<Value>) -> Result<Step, Refusal>;
    fn parse_command(&self, name: &str, args: Option<Value>) -> Result<(), Refusal>;
    fn migrate(&self, from: u32, state: &str) -> Result<String, Refusal>;
    fn check(&self, state: &str) -> Result<(), Refusal>;
    fn adapt(&self, geometry: &Geometry, state: &str) -> Result<Presentation, Refusal>;
}

fn typed_args<A: DeserializeOwned>(args: Option<Value>) -> Result<Option<A>, Refusal> {
    args.map(|value| {
        serde_json::from_value(value).map_err(|error| {
            Refusal::new(
                "args",
                Refusal::ARGS_INVALID,
                format!("args не разобраны: {error}"),
            )
        })
    })
    .transpose()
}

fn typed_state<S: DeserializeOwned>(state: &str) -> Result<S, Refusal> {
    serde_json::from_str(state).map_err(|error| {
        Refusal::new(
            "panel",
            Refusal::STATE_CORRUPT,
            format!("данные панели не разобраны: {error}"),
        )
    })
}

fn typed_command<M: PanelModel>(
    model: &M,
    name: &str,
    args: Option<Value>,
) -> Result<M::Command, Refusal> {
    let manifest = model.manifest();
    if !manifest.has_command(name) {
        return Err(Refusal::new(
            "name",
            Refusal::COMMAND_UNKNOWN,
            format!("у плагина «{}» нет действия «{name}»", manifest.id),
        ));
    }
    let args = match args {
        None => Value::Object(Map::new()),
        Some(args @ Value::Object(_)) => args,
        Some(_) => {
            return Err(Refusal::new(
                "args",
                Refusal::ARGS_INVALID,
                format!("args действия «{name}» — не объект JSON"),
            ));
        }
    };
    let mut frame = Map::new();
    frame.insert("name".to_owned(), Value::String(name.to_owned()));
    frame.insert("args".to_owned(), args);
    serde_json::from_value(Value::Object(frame)).map_err(|error| {
        Refusal::new(
            "args",
            Refusal::ARGS_INVALID,
            format!("args действия «{name}» не разобраны: {error}"),
        )
    })
}

fn encode<S: Serialize>(state: &S) -> Result<String, Refusal> {
    let text = serde_json::to_string(state).map_err(|error| {
        Refusal::new(
            "args",
            Refusal::STATE_UNSERIALIZABLE,
            format!("данные панели не записаны в JSON: {error}"),
        )
    })?;
    if text.len() > STATE_LIMIT {
        return Err(Refusal::new(
            "args",
            Refusal::STATE_TOO_LARGE,
            format!(
                "данные панели занимают {} байт при пределе {STATE_LIMIT}",
                text.len()
            ),
        )
        .with_detail("size", text.len())
        .with_detail("limit", STATE_LIMIT));
    }
    Ok(text)
}

fn step<S: Serialize + PartialEq>(old: &S, new: &S) -> Result<Step, Refusal> {
    if new == old {
        Ok(Step::Unchanged)
    } else {
        encode(new).map(Step::Changed)
    }
}

impl<M: PanelModel> Erased for M {
    fn manifest(&self) -> &'static Manifest {
        PanelModel::manifest(self)
    }

    fn init(&self, args: Option<Value>) -> Result<String, Refusal> {
        encode(&PanelModel::init(self, typed_args(args)?)?)
    }

    fn reopen(&self, state: &str, args: Option<Value>) -> Result<Step, Refusal> {
        let old: M::State = typed_state(state)?;
        let new = PanelModel::reopen(self, &old, typed_args(args)?)?;
        step(&old, &new)
    }

    fn command(&self, state: &str, name: &str, args: Option<Value>) -> Result<Step, Refusal> {
        let command = typed_command(self, name, args)?;
        let old: M::State = typed_state(state)?;
        let new = self.apply(&old, command)?;
        step(&old, &new)
    }

    fn parse_command(&self, name: &str, args: Option<Value>) -> Result<(), Refusal> {
        typed_command(self, name, args).map(|_| ())
    }

    fn migrate(&self, from: u32, state: &str) -> Result<String, Refusal> {
        let data: Value = typed_state(state)?;
        encode(&PanelModel::migrate(self, from, data)?)
    }

    fn check(&self, state: &str) -> Result<(), Refusal> {
        typed_state::<M::State>(state).map(|_| ())
    }

    fn adapt(&self, geometry: &Geometry, state: &str) -> Result<Presentation, Refusal> {
        let state: M::State = typed_state(state)?;
        Ok(PanelModel::adapt(self, geometry, &state))
    }
}

/// Модель плагина без типов состояния, контекста и команды: данные панели — текст JSON, как их
/// хранит хост. Каждый результат уже проверен на [`STATE_LIMIT`], повторной сериализации хосту не
/// нужно.
pub struct DynModel {
    inner: Box<dyn Erased>,
}

impl DynModel {
    pub fn new<M: PanelModel>(model: M) -> Self {
        Self {
            inner: Box::new(model),
        }
    }

    #[must_use]
    pub fn manifest(&self) -> &'static Manifest {
        self.inner.manifest()
    }

    /// Данные новой панели. `args` — контекст открытия, `None` — без контекста.
    pub fn init(&self, args: Option<Value>) -> Result<String, Refusal> {
        self.inner.init(args)
    }

    /// Повторное открытие существующей панели с контекстом.
    pub fn reopen(&self, state: &str, args: Option<Value>) -> Result<Step, Refusal> {
        self.inner.reopen(state, args)
    }

    /// Команда панели. Имя ищется среди действий манифеста, иначе `command_unknown`. `args`:
    /// `None` — пустой объект, объект разбирается строго, всё остальное, включая `null`, — отказ.
    /// Модель неразобранный JSON не видит.
    pub fn command(&self, state: &str, name: &str, args: Option<Value>) -> Result<Step, Refusal> {
        self.inner.command(state, name, args)
    }

    /// Данные версии `from` в данные текущей версии.
    pub fn migrate(&self, from: u32, state: &str) -> Result<String, Refusal> {
        self.inner.migrate(from, state)
    }

    /// Разбор сохранённых данных текущей версии типом состояния вместе с его инвариантами.
    pub fn check(&self, state: &str) -> Result<(), Refusal> {
        self.inner.check(state)
    }

    /// Представление по геометрии части зоны и сохранённым данным.
    pub fn adapt(&self, geometry: &Geometry, state: &str) -> Result<Presentation, Refusal> {
        self.inner.adapt(geometry, state)
    }

    /// Разбирает пример `args` каждого действия манифеста: имя манифеста и вариант команды модели
    /// не расходятся. Модель при этом не вызывается.
    pub fn check_examples(&self) -> Result<(), Refusal> {
        for spec in self.manifest().commands {
            let args: Value = serde_json::from_str(spec.args).map_err(|error| {
                Refusal::new(
                    "args",
                    Refusal::ARGS_INVALID,
                    format!("пример args действия «{}» — не JSON: {error}", spec.name),
                )
            })?;
            self.inner.parse_command(spec.name, Some(args))?;
        }
        Ok(())
    }
}

impl fmt::Debug for DynModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DynModel")
            .field("plugin", &self.manifest().id)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::fake::{Fake, FakeState, manifest_with};
    use crate::manifest::CommandSpec;

    fn model() -> DynModel {
        DynModel::new(Fake::new())
    }

    fn state(text: &str, bumps: u32) -> String {
        serde_json::to_string(&FakeState {
            text: text.to_owned(),
            bumps,
        })
        .unwrap()
    }

    fn refused<T: fmt::Debug>(result: Result<T, Refusal>, field: &str, reason: &str) -> Refusal {
        let refusal = result.unwrap_err();
        assert_eq!(
            (refusal.field.as_str(), refusal.reason.as_str()),
            (field, reason),
            "{refusal}"
        );
        refusal
    }

    /// Текст с многобайтовыми символами, с которым данные занимают ровно `size` байт.
    fn text_for(size: usize) -> String {
        let overhead = state("", 0).len();
        let payload = size - overhead;
        let mut text = "я".repeat(payload / 2);
        text.push_str(&"a".repeat(payload % 2));
        assert_eq!(state(&text, 0).len(), size);
        assert!(text.chars().count() < text.len(), "текст многобайтовый");
        text
    }

    #[test]
    fn init_and_reopen_pass_the_context_to_the_model() {
        let model = model();
        assert_eq!(model.manifest().id, "fake");
        assert_eq!(model.init(None).unwrap(), state("", 0));
        assert_eq!(
            model.init(Some(json!({"text": "привет"}))).unwrap(),
            state("привет", 0)
        );
        let old = state("привет", 2);
        assert_eq!(model.reopen(&old, None), Ok(Step::Unchanged));
        assert_eq!(
            model.reopen(&old, Some(json!({"text": "привет"}))),
            Ok(Step::Unchanged)
        );
        assert_eq!(
            model.reopen(&old, Some(json!({"text": "пока"}))),
            Ok(Step::Changed(state("пока", 2)))
        );
        for args in [
            json!({"text": 1}),
            json!({"text": "a", "x": 1}),
            json!(null),
        ] {
            refused(
                model.init(Some(args.clone())),
                "args",
                Refusal::ARGS_INVALID,
            );
            refused(
                model.reopen(&old, Some(args)),
                "args",
                Refusal::ARGS_INVALID,
            );
        }
    }

    #[test]
    fn command_is_parsed_strictly_before_the_model_sees_it() {
        let model = model();
        let old = state("a", 0);

        let unknown = refused(
            model.command(&old, "fake.nope", None),
            "name",
            Refusal::COMMAND_UNKNOWN,
        );
        assert!(unknown.message.contains("fake.nope"), "{unknown}");
        // Имя варианта без записи в манифесте до разбора не доходит.
        refused(
            model.command(&old, "fake.hidden", None),
            "name",
            Refusal::COMMAND_UNKNOWN,
        );

        // Лишнее поле.
        refused(
            model.command(&old, "fake.fill", Some(json!({"text": "b", "x": 1}))),
            "args",
            Refusal::ARGS_INVALID,
        );
        refused(
            model.command(&old, "fake.bump", Some(json!({"x": 1}))),
            "args",
            Refusal::ARGS_INVALID,
        );
        // `args: null` и прочие не объекты.
        for args in [json!(null), json!([]), json!("b"), json!(1)] {
            refused(
                model.command(&old, "fake.bump", Some(args)),
                "args",
                Refusal::ARGS_INVALID,
            );
        }
        // Неверный тип и отсутствующее обязательное поле.
        refused(
            model.command(&old, "fake.fill", Some(json!({"text": 1}))),
            "args",
            Refusal::ARGS_INVALID,
        );
        refused(
            model.command(&old, "fake.fill", None),
            "args",
            Refusal::ARGS_INVALID,
        );

        // Отсутствующие `args` — пустой объект.
        assert_eq!(
            model.command(&old, "fake.bump", None),
            Ok(Step::Changed(state("a", 1)))
        );
        assert_eq!(
            model.command(&old, "fake.bump", Some(json!({}))),
            Ok(Step::Changed(state("a", 1)))
        );
        assert_eq!(
            model.command(&old, "fake.fill", Some(json!({"text": "b"}))),
            Ok(Step::Changed(state("b", 0)))
        );
        // Повтор того же значения записи не даёт.
        assert_eq!(
            model.command(&old, "fake.fill", Some(json!({"text": "a"}))),
            Ok(Step::Unchanged)
        );
    }

    #[test]
    fn model_refusal_keeps_its_field_and_reason() {
        let refusal = refused(
            model().command(&state("a", 0), "fake.fill", Some(json!({"text": "!"}))),
            "text",
            "text_forbidden",
        );
        assert_eq!(refusal.to_string(), "текст «!» запрещён");
        assert!(refusal.details.is_empty());
    }

    #[test]
    fn stored_state_is_parsed_with_its_invariants() {
        let model = model();
        assert_eq!(model.check(&state("a", 3)), Ok(()));
        for corrupt in [
            "{",
            "null",
            r#"{"text":"a"}"#,
            r#"{"text":"a","bumps":0,"x":1}"#,
            r#"{"text":"a","bumps":1001}"#,
        ] {
            refused(model.check(corrupt), "panel", Refusal::STATE_CORRUPT);
            refused(
                model.command(corrupt, "fake.bump", None),
                "panel",
                Refusal::STATE_CORRUPT,
            );
            refused(model.reopen(corrupt, None), "panel", Refusal::STATE_CORRUPT);
        }
        // Команда не разобрана — данные не читаются.
        refused(
            model.command("{", "fake.nope", None),
            "name",
            Refusal::COMMAND_UNKNOWN,
        );
    }

    #[test]
    fn migrate_gets_the_old_data_as_json() {
        let model = model();
        assert_eq!(
            model.migrate(1, r#"{"note":"старое"}"#),
            Ok(state("старое", 0))
        );
        refused(model.migrate(1, r#"{"note":1}"#), "note", "note_invalid");
        refused(model.migrate(7, "{}"), "version", "version_unknown");
        refused(model.migrate(1, "{"), "panel", Refusal::STATE_CORRUPT);
    }

    #[test]
    fn result_is_limited_to_65536_bytes() {
        assert_eq!(STATE_LIMIT, 65_536);
        let model = model();
        let old = state("a", 0);
        for size in [STATE_LIMIT - 1, STATE_LIMIT] {
            let text = text_for(size);
            let expected = state(&text, 0);
            assert_eq!(expected.len(), size);
            let args = json!({"text": text});
            assert_eq!(model.init(Some(args.clone())), Ok(expected.clone()));
            assert_eq!(
                model.reopen(&old, Some(args.clone())),
                Ok(Step::Changed(expected.clone()))
            );
            assert_eq!(
                model.command(&old, "fake.fill", Some(args)),
                Ok(Step::Changed(expected.clone()))
            );
            let legacy = serde_json::to_string(&json!({"note": text})).unwrap();
            assert_eq!(model.migrate(1, &legacy), Ok(expected));
        }

        let size = STATE_LIMIT + 1;
        let text = text_for(size);
        let args = json!({"text": text});
        let legacy = serde_json::to_string(&json!({"note": text})).unwrap();
        for refusal in [
            model.init(Some(args.clone())).unwrap_err(),
            model.reopen(&old, Some(args.clone())).unwrap_err(),
            model.command(&old, "fake.fill", Some(args)).unwrap_err(),
            model.migrate(1, &legacy).unwrap_err(),
        ] {
            assert_eq!(refusal.reason, Refusal::STATE_TOO_LARGE);
            assert_eq!(refusal.details.get("size"), Some(&json!(size)));
            assert_eq!(refusal.details.get("limit"), Some(&json!(STATE_LIMIT)));
        }

        // Ранее записанное большее значение читается и без изменения проходит.
        let large = state(&text, 0);
        assert_eq!(model.check(&large), Ok(()));
        assert_eq!(model.reopen(&large, None), Ok(Step::Unchanged));
        refused(
            model.command(&large, "fake.bump", None),
            "args",
            Refusal::STATE_TOO_LARGE,
        );
    }

    #[test]
    fn examples_of_the_manifest_parse_as_commands() {
        assert_eq!(model().check_examples(), Ok(()));

        let wrong_args = Fake::with_manifest(manifest_with(|manifest| {
            manifest.commands = Box::leak(Box::new([CommandSpec {
                name: "fake.fill",
                args: r#"{"txt":"a"}"#,
            }]));
        }));
        refused(
            DynModel::new(wrong_args).check_examples(),
            "args",
            Refusal::ARGS_INVALID,
        );

        // Имя манифеста без варианта команды.
        let no_variant = Fake::with_manifest(manifest_with(|manifest| {
            manifest.commands = Box::leak(Box::new([CommandSpec {
                name: "fake.gone",
                args: "{}",
            }]));
        }));
        refused(
            DynModel::new(no_variant).check_examples(),
            "args",
            Refusal::ARGS_INVALID,
        );

        let not_json = Fake::with_manifest(manifest_with(|manifest| {
            manifest.commands = Box::leak(Box::new([CommandSpec {
                name: "fake.bump",
                args: "{",
            }]));
        }));
        refused(
            DynModel::new(not_json).check_examples(),
            "args",
            Refusal::ARGS_INVALID,
        );
    }

    #[test]
    fn erased_model_crosses_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DynModel>();
        assert_eq!(format!("{:?}", model()), r#"DynModel { plugin: "fake" }"#);
    }
}
