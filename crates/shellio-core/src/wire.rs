//! Сообщения транспортной границы, которые не являются командами и чтениями: ответ и событие.

use serde::{Deserialize, Serialize};

use crate::change::Change;
use crate::command::CommandValue;
use crate::model::Revision;
use crate::query::{LayoutView, StatusView};
use crate::tree::TreeView;

/// Успешный ответ команды или чтения.
///
/// У команды `revision` — ревизия после её записи, у пустого изменения и чтения — текущая.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub revision: Revision,
    pub value: ReplyValue,
}

/// Значение ответа. Без тега: варианты различаются набором полей, лишние поля
/// отвергаются, поэтому разбор однозначен.
///
/// Кадры `query.layout` — числа с плавающей точкой, поэтому ответ сравнивается только `PartialEq`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReplyValue {
    /// `query.tree`.
    Tree(TreeView),
    /// `query.status`.
    Status(StatusView),
    /// `query.layout`.
    Layout(Box<LayoutView>),
    /// Команда: идентификаторы созданных записей или `null`.
    Command(CommandValue),
}

impl From<CommandValue> for ReplyValue {
    fn from(value: CommandValue) -> Self {
        Self::Command(value)
    }
}

/// Событие: дельты одной записанной команды с её ревизией.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub revision: Revision,
    pub changes: Vec<Change>,
}
