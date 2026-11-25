//! Намерения панели (ADR-0008, «Уточнено в P1.6 (планирование)»): хост переводит действия keymap
//! `list.*` и `panel.filter` в [`ViewIntent`] и отдаёт виду панели с фокусом.

/// Движение списка. Порядок — порядок действий `list.*` в ADR-0010.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ListMove {
    Down,
    Up,
    Top,
    Bottom,
    PageDown,
    PageUp,
    Collapse,
    Expand,
    Activate,
}

impl ListMove {
    pub const ALL: [Self; 9] = [
        Self::Down,
        Self::Up,
        Self::Top,
        Self::Bottom,
        Self::PageDown,
        Self::PageUp,
        Self::Collapse,
        Self::Expand,
        Self::Activate,
    ];
}

/// Намерение панели от хоста.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ViewIntent {
    /// Действие `list.*`: его получает вид плагина с признаком `list_keys`.
    List(ListMove),
    /// Текст строки фильтра хоста, пустой — фильтр снят: его получает вид плагина с признаком
    /// `filter`. Строки сужает плагин.
    Filter { text: String },
}

/// Ответ вида на намерение. Намерение, которое состояние не изменило (`j` на последней строке),
/// тоже принято: клавиша остаётся в панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Handled {
    Yes,
    /// Вид намерение не обрабатывает. У плагина, объявившего признак намерения, это ошибка
    /// контракта.
    No,
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn list_moves_are_nine_distinct_steps() {
        let steps: HashSet<ListMove> = ListMove::ALL.into_iter().collect();
        assert_eq!(steps.len(), 9);
        assert_eq!(ListMove::ALL[0], ListMove::Down);
        assert_eq!(ListMove::ALL[8], ListMove::Activate);
    }
}
