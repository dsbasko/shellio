//! Левая часть строки статуса после плашки режима (ADR-0010, «Строка статуса»): набранный
//! префикс, сообщение или не больше трёх подсказок из действующей таблицы.

use serde_json::{Value, json};

use crate::chord::Chord;
use crate::keymap::{Context, Keymap, Mode};
use crate::sequence::Sequence;

const MAX_HINTS: usize = 3;

/// Подсказка: клавиши по одной и подпись.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    pub keys: Vec<String>,
    pub label: String,
}

/// Своя подсказка панели: действие плагина и подпись.
#[derive(Clone, Copy, Debug)]
pub struct PanelHint<'a> {
    pub action: &'a str,
    pub args: Option<&'a Value>,
    pub label: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub struct HintQuery<'a> {
    pub mode: Mode,
    /// Первый ответчик — поле ввода.
    pub field: bool,
    /// Набранное начало последовательности.
    pub pending: &'a [Chord],
    /// Сообщение отказа или ошибки keymap: замещает подсказки до следующего действия.
    pub message: Option<&'a str>,
    /// Плагин панели с фокусом.
    pub plugin: Option<&'a str>,
    pub panel_hints: &'a [PanelHint<'a>],
    /// У панели есть получатель ввода.
    pub input_receiver: bool,
    /// Действие `palette.open` зарегистрировано.
    pub palette: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusText {
    Prefix(String),
    Message(String),
    Hints(Vec<Hint>),
}

fn chords(sequence: &Sequence) -> impl Iterator<Item = String> + '_ {
    sequence.chords().iter().map(Chord::to_string)
}

fn hint(keys: Vec<String>, label: &str) -> Option<Hint> {
    (!keys.is_empty()).then(|| Hint {
        keys,
        label: label.to_owned(),
    })
}

/// Что строка статуса показывает после плашки режима.
pub fn status_hints(keymap: &Keymap, query: &HintQuery<'_>) -> StatusText {
    if !query.pending.is_empty() {
        let typed: Vec<String> = query.pending.iter().map(Chord::to_string).collect();
        return StatusText::Prefix(typed.join(" "));
    }
    if let Some(message) = query.message {
        return StatusText::Message(message.to_owned());
    }

    let key = |contexts: &[Context], action: &str, args: Option<&Value>| -> Vec<String> {
        keymap
            .key_for(contexts, action, args)
            .map(|sequence| chords(sequence).collect())
            .unwrap_or_default()
    };
    let navigation = [Context::Navigation, Context::Global];

    if query.mode == Mode::Input {
        let keys = if query.field {
            vec!["Esc".to_owned()]
        } else {
            key(&[Context::Input], "mode.navigation", None)
        };
        return StatusText::Hints(hint(keys, "навигация").into_iter().collect());
    }

    if let Some(plugin) = query.plugin {
        let panel = [Context::Panel(plugin.to_owned())];
        let own: Vec<Hint> = query
            .panel_hints
            .iter()
            .filter_map(|own| hint(key(&panel, own.action, own.args), own.label))
            .take(MAX_HINTS)
            .collect();
        if !own.is_empty() {
            return StatusText::Hints(own);
        }
    }

    let panels = ["left", "down", "up", "right"]
        .iter()
        .flat_map(|direction| key(&navigation, &format!("panel.focus.{direction}"), None))
        .collect();
    let second = if query.input_receiver {
        hint(key(&navigation, "mode.input", None), "ввод")
    } else {
        let tabs = (1..=3)
            .flat_map(|index| key(&navigation, "tab.select", Some(&json!({ "index": index }))))
            .collect();
        hint(tabs, "вкладки")
    };
    let search = query
        .palette
        .then(|| hint(key(&navigation, "palette.open", None), "поиск"))
        .flatten();
    let hints = [hint(panels, "панели"), second, search];
    StatusText::Hints(hints.into_iter().flatten().take(MAX_HINTS).collect())
}
