//! Состояние палитры и его отображение в строки `VirtualList`: заголовок группы — строка без
//! выбора, строка палитры — выбираемая строка с ревизией содержимого (значок, пояснение, клавиши).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use super::layout::ListContent;
use crate::components::virtual_list::model::{ListState, Row, RowKind};

/// Плейсхолдер поля запроса (прототип, `overlays.js`).
pub const PLACEHOLDER: &str = "Сессия, команда или действие…";
/// Сообщение пустой выдачи.
pub const EMPTY_TEXT: &str = "Ничего не найдено";
/// AX-имя списка палитры.
pub const LIST_LABEL: &str = "Результаты палитры";
/// Подсказки подвала: подписи плашек и подпись подсказки.
pub const FOOTER_HINTS: [(&[&str], &str); 3] = [
    (&["↑", "↓"], "выбрать"),
    (&["⏎"], "выполнить"),
    (&["esc"], "закрыть"),
];

/// Строка палитры: устойчивый id, значок набора, название, пояснение и подписи плашек клавиш.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaletteRowSpec {
    pub id: String,
    pub icon: String,
    pub title: String,
    pub detail: String,
    pub keys: Vec<String>,
}

/// Группа палитры по порядку показа.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaletteGroupSpec {
    pub title: String,
    pub rows: Vec<PaletteRowSpec>,
}

/// Состояние вида палитры.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandPaletteState {
    pub query: String,
    /// Подпись плашки категории; `None` — категория «все», плашки нет.
    pub category: Option<String>,
    pub groups: Vec<PaletteGroupSpec>,
    /// Id выбранной строки.
    pub selected: Option<String>,
}

impl CommandPaletteState {
    /// Выдача пуста: ни одной строки ни в одной группе.
    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(|group| group.rows.is_empty())
    }

    /// Строка по id.
    pub fn row(&self, id: &str) -> Option<&PaletteRowSpec> {
        self.groups
            .iter()
            .flat_map(|group| &group.rows)
            .find(|row| row.id == id)
    }
}

/// Id строки заголовка группы `group` (с единицы). Id действий и панелей знака `#` не содержат.
pub fn header_id(group: usize) -> String {
    format!("#group.{group}")
}

/// Ревизия содержимого строки сверх названия: её смена перезагружает вид строки.
pub fn revision(row: &PaletteRowSpec) -> u64 {
    let mut hasher = DefaultHasher::new();
    (&row.icon, &row.detail, &row.keys).hash(&mut hasher);
    hasher.finish()
}

/// Состояние списка: группы без строк не показываются, как в прототипе.
pub fn list_state(state: &CommandPaletteState) -> ListState {
    let mut rows = Vec::new();
    for (index, group) in shown_groups(state) {
        rows.push(Row {
            id: header_id(index),
            text: group.title.clone(),
            kind: RowKind::Header,
            content: 0,
        });
        rows.extend(group.rows.iter().map(|row| Row {
            id: row.id.clone(),
            text: row.title.clone(),
            kind: RowKind::Item,
            content: revision(row),
        }));
    }
    let selected = state
        .selected
        .clone()
        .filter(|id| rows.iter().any(|row| row.id == *id));
    ListState {
        label: LIST_LABEL.to_owned(),
        rows: Rc::from(rows),
        selected,
    }
}

/// Содержимое списка для раскладки.
pub fn list_content(state: &CommandPaletteState) -> ListContent {
    if state.is_empty() {
        return ListContent::Empty;
    }
    ListContent::Groups(
        shown_groups(state)
            .map(|(_, group)| group.rows.len())
            .collect(),
    )
}

/// Ключ сверки строки списка с номером `index`: заголовок — `palette.group.{n}.title`, строка —
/// `palette.group.{n}.row.{m}`. Номера групп — по показанным группам, с единицы.
pub fn row_ck(state: &CommandPaletteState, index: usize) -> Option<String> {
    let mut at = 0;
    for (number, (_, group)) in shown_groups(state).enumerate() {
        let group_ck = format!("palette.group.{}", number + 1);
        if index == at {
            return Some(format!("{group_ck}.title"));
        }
        at += 1;
        if index < at + group.rows.len() {
            return Some(format!("{group_ck}.row.{}", index - at + 1));
        }
        at += group.rows.len();
    }
    None
}

/// Показанные группы с номером группы в состоянии (с единицы).
fn shown_groups(state: &CommandPaletteState) -> impl Iterator<Item = (usize, &PaletteGroupSpec)> {
    state
        .groups
        .iter()
        .enumerate()
        .filter(|(_, group)| !group.rows.is_empty())
        .map(|(index, group)| (index + 1, group))
}

/// Ключ сверки плашки `key` из `count` плашек под ключом `parent`: одна плашка — без номера, как
/// `K.kbd` прототипа.
pub fn key_ck(parent: &str, key: usize, count: usize) -> String {
    if count == 1 {
        format!("{parent}.key")
    } else {
        format!("{parent}.key.{}", key + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, keys: &[&str]) -> PaletteRowSpec {
        PaletteRowSpec {
            id: id.to_owned(),
            icon: "command".to_owned(),
            title: id.to_uppercase(),
            detail: String::new(),
            keys: keys.iter().map(|key| (*key).to_owned()).collect(),
        }
    }

    fn state() -> CommandPaletteState {
        CommandPaletteState {
            query: String::new(),
            category: None,
            groups: vec![
                PaletteGroupSpec {
                    title: "Команды".to_owned(),
                    rows: vec![
                        row("mode.toggle", &["Ctrl", "\\"]),
                        row("app.quit", &["⌘Q"]),
                    ],
                },
                PaletteGroupSpec {
                    title: "Пусто".to_owned(),
                    rows: Vec::new(),
                },
                PaletteGroupSpec {
                    title: "Панели".to_owned(),
                    rows: vec![row("panel.open.terminal", &[])],
                },
            ],
            selected: Some("mode.toggle".to_owned()),
        }
    }

    #[test]
    fn groups_become_headers_and_rows_in_order() {
        let list = list_state(&state());
        let shown: Vec<(&str, RowKind)> = list
            .rows
            .iter()
            .map(|row| (row.id.as_str(), row.kind))
            .collect();
        assert_eq!(
            shown,
            [
                ("#group.1", RowKind::Header),
                ("mode.toggle", RowKind::Item),
                ("app.quit", RowKind::Item),
                ("#group.3", RowKind::Header),
                ("panel.open.terminal", RowKind::Item),
            ]
        );
        assert_eq!(list.rows[0].text, "Команды");
        assert_eq!(list.selected.as_deref(), Some("mode.toggle"));
        assert_eq!(list.label, LIST_LABEL);
        assert!(crate::components::virtual_list::model::check(&list).is_ok());
    }

    #[test]
    fn unknown_selection_is_dropped() {
        let mut palette = state();
        palette.selected = Some("gone".to_owned());
        assert_eq!(list_state(&palette).selected, None);
    }

    #[test]
    fn content_revision_follows_keys_detail_and_icon() {
        let base = row("zoom", &["Ctrl", "w", "z"]);
        let mut keys = base.clone();
        keys.keys.pop();
        let mut detail = base.clone();
        detail.detail = "панель".to_owned();
        let mut icon = base.clone();
        icon.icon = "maximize".to_owned();
        let mut title = base.clone();
        title.title = "Другое".to_owned();
        assert_ne!(revision(&base), revision(&keys));
        assert_ne!(revision(&base), revision(&detail));
        assert_ne!(revision(&base), revision(&icon));
        // Название несёт текст строки, ревизия его не повторяет.
        assert_eq!(revision(&base), revision(&title));
    }

    #[test]
    fn list_content_counts_shown_groups_or_is_empty() {
        assert_eq!(list_content(&state()), ListContent::Groups(vec![2, 1]));
        let empty = CommandPaletteState {
            query: "нет такого".to_owned(),
            ..CommandPaletteState::default()
        };
        assert!(empty.is_empty());
        assert_eq!(list_content(&empty), ListContent::Empty);
        assert!(list_state(&empty).rows.is_empty());
    }

    #[test]
    fn row_keys_number_shown_groups_from_one() {
        let palette = state();
        let keys: Vec<Option<String>> = (0..6).map(|index| row_ck(&palette, index)).collect();
        assert_eq!(
            keys,
            [
                Some("palette.group.1.title".to_owned()),
                Some("palette.group.1.row.1".to_owned()),
                Some("palette.group.1.row.2".to_owned()),
                Some("palette.group.2.title".to_owned()),
                Some("palette.group.2.row.1".to_owned()),
                None,
            ]
        );
        assert_eq!(
            key_ck("palette.footer.hint.2", 0, 1),
            "palette.footer.hint.2.key"
        );
        assert_eq!(
            key_ck("palette.footer.hint.1", 1, 2),
            "palette.footer.hint.1.key.2"
        );
        assert_eq!(palette.row("app.quit").map(|row| row.keys.len()), Some(1));
    }
}
