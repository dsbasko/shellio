//! Запрос `keymap` протокола (ADR-0007, «Уточнено в P1.6 (планирование)»): чтение действующей
//! таблицы клавиш, её перечитывание и подписка на неё.
//!
//! Кадр запроса — объект с одним ключом, именем запроса: `{"query.keymap": {}}`,
//! `{"keymap.reload": {}}`, `{"keymap.subscribe": {}}`. Таблица живёт вне ядра: ответ не несёт
//! доменной ревизии и лежит под ключом `keymap` значения `ok`, кадр подписки — под ключом `keymap`
//! кадра.

use std::future::{Future, ready};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

/// Запрос `keymap`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "WireKeymapRequest", into = "WireKeymapRequest")]
pub enum KeymapRequest {
    /// `query.keymap`: действующая таблица.
    Query,
    /// `keymap.reload`: применить `keymap.toml` сейчас. Ответ — таблица после применения.
    Reload,
    /// `keymap.subscribe`: поток полных таблиц, первая — сразу.
    Subscribe,
}

impl KeymapRequest {
    /// Стабильное имя запроса.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Query => "query.keymap",
            Self::Reload => "keymap.reload",
            Self::Subscribe => "keymap.subscribe",
        }
    }
}

/// Пустое тело запроса: единичный вариант serde пропускал бы лишние поля.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Serialize, Deserialize)]
enum WireKeymapRequest {
    #[serde(rename = "query.keymap")]
    Query(Empty),
    #[serde(rename = "keymap.reload")]
    Reload(Empty),
    #[serde(rename = "keymap.subscribe")]
    Subscribe(Empty),
}

impl From<WireKeymapRequest> for KeymapRequest {
    fn from(wire: WireKeymapRequest) -> Self {
        match wire {
            WireKeymapRequest::Query(Empty {}) => Self::Query,
            WireKeymapRequest::Reload(Empty {}) => Self::Reload,
            WireKeymapRequest::Subscribe(Empty {}) => Self::Subscribe,
        }
    }
}

impl From<KeymapRequest> for WireKeymapRequest {
    fn from(request: KeymapRequest) -> Self {
        match request {
            KeymapRequest::Query => Self::Query(Empty {}),
            KeymapRequest::Reload => Self::Reload(Empty {}),
            KeymapRequest::Subscribe => Self::Subscribe(Empty {}),
        }
    }
}

/// Действующая таблица клавиш: ответ `query.keymap` и `keymap.reload`, кадр подписки.
///
/// Версия действует в пределах процесса владельца и растёт, когда меняются строки или лидер.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeymapView {
    pub version: u64,
    pub leader: String,
    pub bindings: Vec<KeymapRow>,
    /// Диагностика последнего разбора `keymap.toml`.
    pub diagnostics: Vec<KeymapDiagnostic>,
}

/// Строка таблицы: контекст, нормализованная последовательность, действие, `args` и источник.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeymapRow {
    pub context: String,
    pub sequence: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    pub source: KeymapSource,
}

/// Откуда строка таблицы.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeymapSource {
    Default,
    File,
}

impl KeymapSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::File => "file",
        }
    }
}

/// Запись диагностики: номер строки файла (у нечитаемого файла его нет), вид, причина и готовое
/// сообщение строки статуса.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeymapDiagnostic {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub kind: String,
    pub reason: String,
    pub message: String,
}

/// Порт службы keymap владельца. Сервер отвечает из снимка без ядра и без главного потока.
pub trait KeymapHandler: Send + Sync + 'static {
    /// Снимок службы: сервер берёт из него только представление таблицы.
    type Snapshot: AsRef<KeymapView> + Send + Sync + 'static;

    /// Получатель снимков: текущий снимок лежит в нём сразу.
    fn watch(&self) -> watch::Receiver<Arc<Self::Snapshot>>;

    /// Применить `keymap.toml` и вернуть снимок после применения.
    fn reload(&self) -> impl Future<Output = Arc<Self::Snapshot>> + Send;
}

impl AsRef<Self> for KeymapView {
    fn as_ref(&self) -> &Self {
        self
    }
}

/// Владелец без службы keymap: пустая таблица версии 0, которая не меняется.
#[derive(Clone, Debug)]
pub struct WithoutKeymap(watch::Sender<Arc<KeymapView>>);

impl Default for WithoutKeymap {
    fn default() -> Self {
        Self(watch::Sender::new(Arc::default()))
    }
}

impl KeymapHandler for WithoutKeymap {
    type Snapshot = KeymapView;

    fn watch(&self) -> watch::Receiver<Arc<KeymapView>> {
        self.0.subscribe()
    }

    fn reload(&self) -> impl Future<Output = Arc<KeymapView>> + Send {
        ready(Arc::clone(&self.0.borrow()))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn requests_are_single_key_objects_with_empty_bodies() {
        let cases = [
            (KeymapRequest::Query, "query.keymap"),
            (KeymapRequest::Reload, "keymap.reload"),
            (KeymapRequest::Subscribe, "keymap.subscribe"),
        ];
        for (request, name) in cases {
            assert_eq!(request.name(), name);
            let wire = json!({ name: {} });
            assert_eq!(serde_json::to_value(request).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<KeymapRequest>(wire).unwrap(),
                request
            );
            let extra = json!({ name: { "x": 1 } });
            assert!(serde_json::from_value::<KeymapRequest>(extra).is_err());
        }
        assert!(serde_json::from_value::<KeymapRequest>(json!({ "keymap.drop": {} })).is_err());
    }

    #[test]
    fn view_round_trips_and_omits_absent_fields() {
        let view = KeymapView {
            version: 3,
            leader: "Space".to_owned(),
            bindings: vec![
                KeymapRow {
                    context: "navigation".to_owned(),
                    sequence: "Ctrl-w v".to_owned(),
                    action: "layout.split".to_owned(),
                    args: Some(json!({ "axis": "row" })),
                    source: KeymapSource::Default,
                },
                KeymapRow {
                    context: "global".to_owned(),
                    sequence: "Cmd-Alt-b".to_owned(),
                    action: "layout.toggle.bottom".to_owned(),
                    args: None,
                    source: KeymapSource::File,
                },
            ],
            diagnostics: vec![KeymapDiagnostic {
                line: None,
                kind: "unreadable".to_owned(),
                reason: "строка 2".to_owned(),
                message: "keymap.toml не прочитан".to_owned(),
            }],
        };
        let wire = serde_json::to_value(&view).unwrap();
        assert_eq!(wire["bindings"][1].get("args"), None);
        assert_eq!(wire["bindings"][1]["source"], "file");
        assert_eq!(wire["diagnostics"][0].get("line"), None);
        assert_eq!(serde_json::from_value::<KeymapView>(wire).unwrap(), view);
    }

    #[tokio::test]
    async fn owner_without_keymap_serves_an_empty_table() {
        let port = WithoutKeymap::default();
        assert_eq!(**port.watch().borrow(), KeymapView::default());
        assert_eq!(*port.reload().await, KeymapView::default());
    }
}
