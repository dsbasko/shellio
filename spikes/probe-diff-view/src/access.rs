//! Доступность строк diff без AppKit: что элемент строки показа сообщает клиенту доступности,
//! разбор этого сообщения и сверка снимка внешнего клиента с моделью. Варианты A и B берут
//! описание строки отсюда, а сценарий `ax` сравнивает с ним то, что клиент прочитал через
//! `AXUIElement`.
//!
//! Номера строк и признак изменения стандартного атрибута не имеют. Они идут в подпись
//! (`AXDescription`), которую озвучивает VoiceOver, в виде «добавлена, новая 12». Текст строки —
//! значение (`AXValue`). Выбранность (`AXSelected`) — строка внутри выделения, а без выделения —
//! строка курсора, как её рисует вид.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::model::state::Selection;
use crate::model::{Diff, LineKind, Mode, Side, TextBody};

/// Роль элемента строки и контейнера строк.
pub const ROW_ROLE: &str = "AXRow";
pub const TABLE_ROLE: &str = "AXTable";
/// Подпись строки таблицы с блоками тредов (вариант B).
pub const BLOCK_LABEL: &str = "обсуждение";
/// Допуск рамки элемента против прямоугольника строки, точки.
pub const FRAME_TOLERANCE: f64 = 1.0;

/// Что случилось со строкой показа.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    Context,
    Added,
    Removed,
    /// Пара «Рядом»: удалённая слева, добавленная справа.
    Changed,
}

impl Change {
    pub const ALL: [Change; 4] = [
        Change::Context,
        Change::Added,
        Change::Removed,
        Change::Changed,
    ];

    pub fn word(self) -> &'static str {
        match self {
            Change::Context => "без изменений",
            Change::Added => "добавлена",
            Change::Removed => "удалена",
            Change::Changed => "изменена",
        }
    }
}

/// Сообщение элемента строки показа.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowAx {
    pub change: Change,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
    pub selected: bool,
}

const OLD: &str = "старая ";
const NEW: &str = "новая ";

impl RowAx {
    /// Подпись: признак изменения и номера строк сторон.
    pub fn label(&self) -> String {
        label(self.change, self.old, self.new)
    }

    /// Ключ элемента: номера строк сторон. Элемент строки переиспользуется только для того же
    /// ключа.
    pub fn key(&self) -> (Option<u32>, Option<u32>) {
        (self.old, self.new)
    }
}

pub fn label(change: Change, old: Option<u32>, new: Option<u32>) -> String {
    let mut text = change.word().to_owned();
    if let Some(old) = old {
        text.push_str(&format!(", {OLD}{old}"));
    }
    if let Some(new) = new {
        text.push_str(&format!(", {NEW}{new}"));
    }
    text
}

/// Признак изменения и номера сторон из подписи; `None` — подпись не того вида.
pub fn parse_label(label: &str) -> Option<(Change, Option<u32>, Option<u32>)> {
    let mut parts = label.split(", ");
    let word = parts.next()?;
    let change = Change::ALL
        .into_iter()
        .find(|change| change.word() == word)?;
    let (mut old, mut new) = (None, None);
    for part in parts {
        let (slot, number) = match part.strip_prefix(OLD) {
            Some(number) => (&mut old, number),
            None => (&mut new, part.strip_prefix(NEW)?),
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(number.parse().ok()?);
    }
    (label == self::label(change, old, new)).then_some((change, old, new))
}

/// Сообщение строки показа `row` в режиме `mode`. `cursor_row` — строка курсора, `selection` —
/// выделение состояния. Выбранность — как у отрисовки: половина строки на стороне выделения в
/// его диапазоне, без выделения — строка курсора.
pub fn describe_row(
    diff: &Diff,
    body: &TextBody,
    mode: Mode,
    row: u32,
    cursor_row: Option<u32>,
    selection: Option<Selection>,
) -> Option<RowAx> {
    let halves = body.row(mode, row)?;
    let mut lines: Vec<u32> = Vec::new();
    for side in Side::BOTH {
        if let Some(index) = halves.half(side)
            && !lines.contains(&index)
        {
            lines.push(index);
        }
    }
    let first = body.line(*lines.first()?)?;
    let (old, new) = match mode {
        Mode::Unified => (first.line(Side::Old), first.line(Side::New)),
        Mode::SideBySide => (
            halves
                .old
                .and_then(|index| body.line(index)?.line(Side::Old)),
            halves
                .new
                .and_then(|index| body.line(index)?.line(Side::New)),
        ),
    };
    let change = match (mode, first.kind()) {
        (Mode::Unified, LineKind::Context) => Change::Context,
        (Mode::Unified, LineKind::Added) => Change::Added,
        (Mode::Unified, LineKind::Removed) => Change::Removed,
        (Mode::SideBySide, _) => match (halves.old, halves.new) {
            (Some(old), Some(new)) if old == new => Change::Context,
            (Some(_), Some(_)) => Change::Changed,
            (Some(_), None) => Change::Removed,
            (None, _) => Change::Added,
        },
    };
    let text = lines
        .iter()
        .filter_map(|&index| body.line(index))
        .map(|line| String::from_utf8_lossy(diff.line_text(line)).into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    let selected = match selection {
        Some(selection) => Side::BOTH.into_iter().any(|side| {
            let drawn = match mode {
                Mode::Unified => true,
                Mode::SideBySide => side == selection.side,
            };
            drawn
                && halves
                    .half(side)
                    .and_then(|index| body.line(index)?.addr(selection.side))
                    .is_some_and(|addr| selection.contains(addr))
        }),
        None => cursor_row == Some(row),
    };
    Some(RowAx {
        change,
        old,
        new,
        text,
        selected,
    })
}

/// Рамка элемента в глобальных координатах доступности (начало вверху слева главного экрана).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AxFrame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl AxFrame {
    pub fn near(&self, other: &AxFrame, tolerance: f64) -> bool {
        (self.x - other.x).abs() <= tolerance
            && (self.y - other.y).abs() <= tolerance
            && (self.width - other.width).abs() <= tolerance
            && (self.height - other.height).abs() <= tolerance
    }
}

/// Элемент доступности глазами внешнего клиента.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AxElement {
    pub role: Option<String>,
    pub role_description: Option<String>,
    pub description: Option<String>,
    pub value: Option<String>,
    pub selected: Option<bool>,
    pub index: Option<i64>,
    pub frame: Option<AxFrame>,
    /// Имена атрибутов элемента: наблюдение.
    pub attributes: Vec<String>,
    /// Код ошибки AX, если элемент не прочитан (например, освобождён).
    pub error: Option<i32>,
}

/// Строка из видимых строк контейнера. `same_as_before` — тот же элемент (`CFEqual`), что у
/// строки с той же подписью в предыдущем снимке; `None` — такой строки там не было.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AxRowRead {
    pub element: AxElement,
    pub same_as_before: Option<bool>,
}

/// Элемент строки предыдущего снимка, перечитанный сейчас по удержанной ссылке.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AxHeld {
    pub before: AxElement,
    pub now: AxElement,
}

/// Уведомление доступности, которое получил наблюдатель клиента.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AxNote {
    pub name: String,
    pub role: Option<String>,
    pub index: Option<i64>,
    /// Миллисекунды от запуска клиента.
    pub at_ms: f64,
}

/// Снимок клиента: контейнер строк окна, его видимые строки, выбранные строки, элемент с
/// фокусом, удержанные элементы предыдущего снимка и уведомления после него.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AxSnapshot {
    pub label: String,
    /// Роли от окна до контейнера строк.
    pub path: Vec<String>,
    pub table: Option<AxElement>,
    /// Атрибут, из которого прочитаны строки.
    pub rows_source: String,
    pub rows: Vec<AxRowRead>,
    pub selected_rows: Option<Vec<i64>>,
    pub focused: Option<AxElement>,
    pub focused_is_table: bool,
    pub held: Vec<AxHeld>,
    pub notes: Vec<AxNote>,
    pub read_ms: f64,
    pub error: Option<String>,
}

/// Ожидаемая строка контейнера: индекс элемента и сообщение строки показа, `None` — строка
/// блоков тредов.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpectedRow {
    pub index: i64,
    pub row: Option<RowAx>,
    /// Рамка строки в глобальных координатах доступности.
    pub frame: Option<AxFrame>,
}

impl ExpectedRow {
    pub fn label(&self) -> String {
        self.row
            .as_ref()
            .map_or_else(|| BLOCK_LABEL.to_owned(), RowAx::label)
    }

    fn selected(&self) -> bool {
        self.row.as_ref().is_some_and(|row| row.selected)
    }
}

/// Где ожидается фокус доступности.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpectedFocus {
    /// Элемент строки с этим индексом: строка курсора видна.
    Row(i64),
    /// Сам контейнер: строка курсора не видна.
    Table,
}

/// Что модель ждёт от снимка.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Expectation {
    pub rows: Vec<ExpectedRow>,
    pub focus: ExpectedFocus,
}

/// Расхождения снимка с моделью по частям контракта.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Comparison {
    /// Видимые строки, их атрибуты и выбранные строки контейнера.
    pub rows: Vec<String>,
    /// Элемент с фокусом.
    pub focus: Vec<String>,
    /// Удержанные элементы предыдущего снимка.
    pub held: Vec<String>,
}

impl Comparison {
    pub fn is_clean(&self) -> bool {
        self.rows.is_empty() && self.focus.is_empty() && self.held.is_empty()
    }

    pub fn all(&self) -> Vec<String> {
        self.rows
            .iter()
            .chain(&self.focus)
            .chain(&self.held)
            .cloned()
            .collect()
    }
}

/// Сверка снимка с моделью.
pub fn compare(snapshot: &AxSnapshot, expected: &Expectation) -> Comparison {
    let mut out = Comparison::default();
    let errors = &mut out.rows;
    if let Some(error) = &snapshot.error {
        errors.push(format!("клиент: {error}"));
        return out;
    }
    match &snapshot.table {
        Some(table) if table.role.as_deref() == Some(TABLE_ROLE) => {}
        Some(table) => errors.push(format!("контейнер строк с ролью {:?}", table.role)),
        None => {
            errors.push(format!(
                "контейнер строк не найден, путь {:?}",
                snapshot.path
            ));
            return out;
        }
    }
    let read: BTreeSet<i64> = snapshot
        .rows
        .iter()
        .filter_map(|row| row.element.index)
        .collect();
    let wanted: BTreeSet<i64> = expected.rows.iter().map(|row| row.index).collect();
    if snapshot.rows.iter().any(|row| row.element.index.is_none()) {
        errors.push("строка без индекса".to_owned());
    }
    if read.len() != snapshot.rows.len() {
        errors.push("индексы строк повторяются".to_owned());
    }
    if read != wanted {
        let missing: Vec<i64> = wanted.difference(&read).copied().collect();
        let extra: Vec<i64> = read.difference(&wanted).copied().collect();
        errors.push(format!(
            "видимые строки: нет {}, лишние {}",
            short(&missing),
            short(&extra)
        ));
    }
    for want in &expected.rows {
        let Some(got) = snapshot
            .rows
            .iter()
            .find(|row| row.element.index == Some(want.index))
        else {
            continue;
        };
        let got = &got.element;
        let at = want.index;
        if got.role.as_deref() != Some(ROW_ROLE) {
            errors.push(format!("строка {at}: роль {:?}", got.role));
        }
        let label = want.label();
        if got.description.as_deref() != Some(label.as_str()) {
            errors.push(format!(
                "строка {at}: подпись {:?} вместо «{label}»",
                got.description
            ));
        }
        if let Some(row) = &want.row
            && got.value.as_deref() != Some(row.text.as_str())
        {
            errors.push(format!(
                "строка {at}: текст {:?} вместо {:?}",
                got.value, row.text
            ));
        }
        if got.selected != Some(want.selected()) {
            errors.push(format!(
                "строка {at}: выбрана {:?} вместо {}",
                got.selected,
                want.selected()
            ));
        }
        if let Some(frame) = &want.frame {
            match &got.frame {
                Some(actual) if actual.near(frame, FRAME_TOLERANCE) => {}
                actual => errors.push(format!("строка {at}: рамка {actual:?} вместо {frame:?}")),
            }
        }
    }
    let selected: BTreeSet<i64> = expected
        .rows
        .iter()
        .filter(|row| row.selected())
        .map(|row| row.index)
        .collect();
    match &snapshot.selected_rows {
        Some(indexes) => {
            let visible: BTreeSet<i64> = indexes
                .iter()
                .copied()
                .filter(|index| wanted.contains(index))
                .collect();
            if visible != selected {
                errors.push(format!(
                    "выбранные строки {} вместо {}",
                    short(&visible.into_iter().collect::<Vec<_>>()),
                    short(&selected.iter().copied().collect::<Vec<_>>())
                ));
            }
        }
        None => errors.push("нет выбранных строк контейнера".to_owned()),
    }
    let errors = &mut out.focus;
    match expected.focus {
        ExpectedFocus::Table => {
            if !snapshot.focused_is_table {
                errors.push(format!(
                    "фокус у {:?} вместо контейнера",
                    snapshot.focused.as_ref().map(brief)
                ));
            }
        }
        ExpectedFocus::Row(index) => {
            let label = expected
                .rows
                .iter()
                .find(|row| row.index == index)
                .map(ExpectedRow::label);
            let ok = snapshot.focused.as_ref().is_some_and(|focused| {
                focused.role.as_deref() == Some(ROW_ROLE)
                    && focused.index == Some(index)
                    && label.is_none_or(|label| focused.description.as_deref() == Some(&label))
            });
            if !ok {
                errors.push(format!(
                    "фокус у {:?} вместо строки {index}",
                    snapshot.focused.as_ref().map(brief)
                ));
            }
        }
    }
    for held in &snapshot.held {
        if held.now.error.is_some() {
            continue;
        }
        if held.now.description != held.before.description || held.now.index != held.before.index {
            out.held.push(format!(
                "удержанный элемент строки {} указывает на другую строку: {}",
                brief(&held.before),
                brief(&held.now)
            ));
        }
    }
    out
}

/// Строки, у которых в предыдущем снимке была строка с той же подписью: тот же элемент и другой.
pub fn reuse(snapshot: &AxSnapshot) -> (usize, usize) {
    snapshot
        .rows
        .iter()
        .fold((0, 0), |(same, other), row| match row.same_as_before {
            Some(true) => (same + 1, other),
            Some(false) => (same, other + 1),
            None => (same, other),
        })
}

/// Число уведомлений с именем `name` в снимке.
pub fn notes_named(snapshot: &AxSnapshot, name: &str) -> usize {
    snapshot
        .notes
        .iter()
        .filter(|note| note.name == name)
        .count()
}

fn brief(element: &AxElement) -> String {
    format!(
        "{} #{} «{}»{}",
        element.role.as_deref().unwrap_or("?"),
        element
            .index
            .map_or_else(|| "?".to_owned(), |index| index.to_string()),
        element.description.as_deref().unwrap_or(""),
        element
            .error
            .map_or_else(String::new, |error| format!(" ошибка {error}"))
    )
}

fn short(indexes: &[i64]) -> String {
    const SHOWN: usize = 8;
    let mut text = indexes
        .iter()
        .take(SHOWN)
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if indexes.len() > SHOWN {
        text.push_str(&format!(" … всего {}", indexes.len()));
    }
    format!("[{text}]")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::model::parse;
    use crate::worker::Cancel;

    const DIFF: &str = "diff --git a/f.rs b/f.rs\n\
        index 1111111..2222222 100644\n\
        --- a/f.rs\n\
        +++ b/f.rs\n\
        @@ -1,4 +1,5 @@\n \
        a\n\
        -b\n\
        +c\n \
        d\n\
        -e\n\
        +f\n\
        +g\n";

    fn diff() -> Diff {
        parse::parse(Arc::from(DIFF.as_bytes()), &Cancel::never()).unwrap()
    }

    fn rows(
        diff: &Diff,
        mode: Mode,
        cursor: Option<u32>,
        selection: Option<Selection>,
    ) -> Vec<RowAx> {
        let body = diff.file(0).unwrap().text().unwrap();
        (0..body.row_count(mode) as u32)
            .map(|row| describe_row(diff, body, mode, row, cursor, selection).unwrap())
            .collect()
    }

    #[test]
    fn labels_round_trip() {
        for change in Change::ALL {
            for (old, new) in [(Some(3), None), (None, Some(12)), (Some(1), Some(100_000))] {
                let text = label(change, old, new);
                assert_eq!(parse_label(&text), Some((change, old, new)), "{text}");
            }
        }
        assert_eq!(label(Change::Added, None, Some(12)), "добавлена, новая 12");
        assert_eq!(
            label(Change::Context, Some(4), Some(5)),
            "без изменений, старая 4, новая 5"
        );
        for broken in [
            "",
            "добавлено, новая 1",
            "добавлена, новая",
            "добавлена, новая x",
            "добавлена, новая 1, новая 2",
            "добавлена, новая 2, старая 1",
            "добавлена,новая 1",
            BLOCK_LABEL,
        ] {
            assert_eq!(parse_label(broken), None, "{broken}");
        }
    }

    #[test]
    fn unified_rows_carry_kind_numbers_and_text() {
        let diff = diff();
        let got = rows(&diff, Mode::Unified, Some(0), None);
        let summary: Vec<(String, &str, bool)> = got
            .iter()
            .map(|row| (row.label(), row.text.as_str(), row.selected))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("без изменений, старая 1, новая 1".to_owned(), "a", true),
                ("удалена, старая 2".to_owned(), "b", false),
                ("добавлена, новая 2".to_owned(), "c", false),
                ("без изменений, старая 3, новая 3".to_owned(), "d", false),
                ("удалена, старая 4".to_owned(), "e", false),
                ("добавлена, новая 4".to_owned(), "f", false),
                ("добавлена, новая 5".to_owned(), "g", false),
            ]
        );
    }

    #[test]
    fn side_rows_pair_halves() {
        let diff = diff();
        let got = rows(&diff, Mode::SideBySide, None, None);
        let summary: Vec<(String, &str)> = got
            .iter()
            .map(|row| (row.label(), row.text.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("без изменений, старая 1, новая 1".to_owned(), "a"),
                ("изменена, старая 2, новая 2".to_owned(), "b\nc"),
                ("без изменений, старая 3, новая 3".to_owned(), "d"),
                ("изменена, старая 4, новая 4".to_owned(), "e\nf"),
                ("добавлена, новая 5".to_owned(), "g"),
            ]
        );
        assert!(got.iter().all(|row| !row.selected));
    }

    #[test]
    fn selection_marks_rows_like_the_painter() {
        let diff = diff();
        let new = Selection {
            side: Side::New,
            tail: 2,
            head: 4,
        };
        let unified: Vec<bool> = rows(&diff, Mode::Unified, Some(5), Some(new))
            .iter()
            .map(|row| row.selected)
            .collect();
        // Удалённые строки внутри диапазона новой стороны адреса новой стороны не имеют.
        assert_eq!(unified, vec![false, false, true, true, false, true, false]);
        let side: Vec<bool> = rows(&diff, Mode::SideBySide, Some(3), Some(new))
            .iter()
            .map(|row| row.selected)
            .collect();
        assert_eq!(side, vec![false, true, true, true, false]);
        let old = Selection {
            side: Side::Old,
            tail: 4,
            head: 4,
        };
        let side: Vec<bool> = rows(&diff, Mode::SideBySide, None, Some(old))
            .iter()
            .map(|row| row.selected)
            .collect();
        assert_eq!(side, vec![false, false, false, true, false]);
    }

    fn element(index: i64, row: &RowAx) -> AxRowRead {
        AxRowRead {
            element: AxElement {
                role: Some(ROW_ROLE.to_owned()),
                description: Some(row.label()),
                value: Some(row.text.clone()),
                selected: Some(row.selected),
                index: Some(index),
                frame: Some(AxFrame {
                    x: 10.0,
                    y: 100.0 + 15.0 * index as f64,
                    width: 500.0,
                    height: 15.0,
                }),
                ..AxElement::default()
            },
            same_as_before: None,
        }
    }

    fn sample(rows: &[RowAx], cursor: i64) -> (AxSnapshot, Expectation) {
        let reads: Vec<AxRowRead> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| element(index as i64, row))
            .collect();
        let selected = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.selected)
            .map(|(index, _)| index as i64)
            .collect();
        let snapshot = AxSnapshot {
            label: "first".to_owned(),
            path: vec!["AXWindow".to_owned(), "AXScrollArea".to_owned()],
            table: Some(AxElement {
                role: Some(TABLE_ROLE.to_owned()),
                ..AxElement::default()
            }),
            rows_source: "AXVisibleRows".to_owned(),
            focused: Some(reads[cursor as usize].element.clone()),
            rows: reads.clone(),
            selected_rows: Some(selected),
            ..AxSnapshot::default()
        };
        let expected = Expectation {
            rows: rows
                .iter()
                .enumerate()
                .map(|(index, row)| ExpectedRow {
                    index: index as i64,
                    row: Some(row.clone()),
                    frame: reads[index].element.frame,
                })
                .collect(),
            focus: ExpectedFocus::Row(cursor),
        };
        (snapshot, expected)
    }

    #[test]
    fn matching_snapshot_has_no_errors() {
        let diff = diff();
        let (snapshot, expected) = sample(&rows(&diff, Mode::Unified, Some(3), None), 3);
        assert!(compare(&snapshot, &expected).is_clean());
    }

    #[test]
    fn shifted_stale_and_wrongly_selected_rows_are_caught() {
        let diff = diff();
        let model = rows(&diff, Mode::Unified, Some(3), None);
        let (good, expected) = sample(&model, 3);

        // Элемент строки 2 после переиспользования рассказывает о строке 1.
        let mut shifted = good.clone();
        shifted.rows[2].element.description = Some(model[1].label());
        let errors = compare(&shifted, &expected).rows;
        assert!(
            errors.iter().any(|e| e.contains("строка 2: подпись")),
            "{errors:?}"
        );

        let mut stale = good.clone();
        stale.rows[4].element.value = Some("старый текст".to_owned());
        let errors = compare(&stale, &expected).rows;
        assert!(
            errors.iter().any(|e| e.contains("строка 4: текст")),
            "{errors:?}"
        );

        let mut selected = good.clone();
        selected.rows[0].element.selected = Some(true);
        let errors = compare(&selected, &expected).rows;
        assert!(
            errors.iter().any(|e| e.contains("строка 0: выбрана")),
            "{errors:?}"
        );

        let mut missing = good.clone();
        missing.rows.pop();
        let errors = compare(&missing, &expected).rows;
        assert!(errors.iter().any(|e| e.contains("нет [6]")), "{errors:?}");

        let mut focus = good.clone();
        focus.focused = Some(good.rows[2].element.clone());
        let errors = compare(&focus, &expected).focus;
        assert!(
            errors.iter().any(|e| e.contains("вместо строки 3")),
            "{errors:?}"
        );

        let mut frame = good.clone();
        if let Some(frame) = frame.rows[1].element.frame.as_mut() {
            frame.y += 15.0;
        }
        let errors = compare(&frame, &expected).rows;
        assert!(
            errors.iter().any(|e| e.contains("строка 1: рамка")),
            "{errors:?}"
        );

        let mut drifted = good.clone();
        drifted.held.push(AxHeld {
            before: good.rows[1].element.clone(),
            now: good.rows[2].element.clone(),
        });
        let errors = compare(&drifted, &expected).held;
        assert!(
            errors.iter().any(|e| e.contains("другую строку")),
            "{errors:?}"
        );

        let mut released = good.clone();
        released.held.push(AxHeld {
            before: good.rows[1].element.clone(),
            now: AxElement {
                error: Some(-25202),
                ..AxElement::default()
            },
        });
        assert!(compare(&released, &expected).is_clean());
    }

    #[test]
    fn focus_on_the_table_when_the_cursor_is_off_screen() {
        let diff = diff();
        let (mut snapshot, mut expected) = sample(&rows(&diff, Mode::Unified, None, None), 0);
        expected.focus = ExpectedFocus::Table;
        let errors = compare(&snapshot, &expected).focus;
        assert!(
            errors.iter().any(|e| e.contains("вместо контейнера")),
            "{errors:?}"
        );
        snapshot.focused_is_table = true;
        assert!(compare(&snapshot, &expected).is_clean());
    }

    /// Записанный образец прогона: снимок клиента и ожидание модели в момент запроса.
    #[derive(Deserialize)]
    struct Recorded {
        snapshot: AxSnapshot,
        expected: Expectation,
    }

    fn recorded(json: &str) -> Recorded {
        serde_json::from_str(json).unwrap()
    }

    const A_UNIFIED_SELECT: &str = include_str!("../testdata/ax/a-unified-select.json");
    const A_SIDE_BOTTOM: &str = include_str!("../testdata/ax/a-side-bottom.json");
    /// Образец до исправления B (прогон `20261119-130028-16537`, шаг `scroll`): элемент жил в виде
    /// строки и после переиспользования вида описывал другую строку.
    const B_UNIFIED_SCROLL: &str = include_str!("../testdata/ax/b-unified-scroll.json");
    /// Образец после исправления B (прогон `20261119-131049-29704`, файл шага `ax-reuse.json`):
    /// элементы по адресу строки показа.
    const B_SIDE_REUSE: &str = include_str!("../testdata/ax/b-side-reuse.json");

    #[test]
    fn recorded_a_snapshots_match_the_model() {
        let select = recorded(A_UNIFIED_SELECT);
        assert_eq!(
            compare(&select.snapshot, &select.expected),
            Comparison::default()
        );
        assert_eq!(select.expected.focus, ExpectedFocus::Row(517));
        let selected = select
            .expected
            .rows
            .iter()
            .filter(|row| row.selected())
            .count();
        assert!(selected >= 2, "{selected}");
        assert_eq!(reuse(&select.snapshot).1, 0);

        let bottom = recorded(A_SIDE_BOTTOM);
        assert!(compare(&bottom.snapshot, &bottom.expected).is_clean());
        let pairs: Vec<&ExpectedRow> = bottom
            .expected
            .rows
            .iter()
            .filter(|row| {
                row.row
                    .as_ref()
                    .is_some_and(|row| row.change == Change::Changed)
            })
            .collect();
        assert!(!pairs.is_empty());
        assert!(
            pairs
                .iter()
                .all(|row| row.row.as_ref().is_some_and(|row| row.text.contains('\n')))
        );
        for row in &bottom.snapshot.rows {
            let label = row.element.description.as_deref().unwrap_or_default();
            assert!(parse_label(label).is_some(), "{label}");
        }
    }

    #[test]
    fn recorded_b_snapshot_shows_held_elements_drifting() {
        let scroll = recorded(B_UNIFIED_SCROLL);
        let comparison = compare(&scroll.snapshot, &scroll.expected);
        assert!(comparison.rows.is_empty(), "{:?}", comparison.rows);
        assert!(comparison.focus.is_empty(), "{:?}", comparison.focus);
        assert_eq!(scroll.expected.focus, ExpectedFocus::Table);
        assert!(!comparison.held.is_empty());
        assert!(
            comparison
                .held
                .iter()
                .all(|error| error.contains("другую строку"))
        );
    }

    #[test]
    fn recorded_b_snapshot_after_the_fix_keeps_held_elements() {
        let sample = recorded(B_SIDE_REUSE);
        assert_eq!(
            compare(&sample.snapshot, &sample.expected),
            Comparison::default()
        );
        let released = sample
            .snapshot
            .held
            .iter()
            .filter(|held| held.now.error.is_some())
            .count();
        let still_read = sample.snapshot.held.len() - released;
        assert!(released > 0 && still_read > 0, "{released} {still_read}");
        let (same, other) = reuse(&sample.snapshot);
        assert!(same > 0, "{same}");
        assert_eq!(other, 0);
        assert!(
            sample
                .expected
                .rows
                .iter()
                .any(|row| row.row.as_ref().is_some_and(|row| row.text.contains('\n')))
        );
    }

    #[test]
    fn recorded_sample_mutations_are_caught() {
        let good = recorded(A_UNIFIED_SELECT);
        let expected = &good.expected;

        let mut shifted = good.snapshot.clone();
        let label = shifted.rows[1].element.description.clone();
        shifted.rows[2].element.description = label;
        let errors = compare(&shifted, expected).rows;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("подпись"), "{errors:?}");

        let mut stale = good.snapshot.clone();
        stale.rows[3].element.value = Some("старый текст".to_owned());
        assert!(
            compare(&stale, expected)
                .rows
                .iter()
                .any(|e| e.contains("текст"))
        );

        let mut selected = good.snapshot.clone();
        let flip = selected.rows[0].element.selected.map(|value| !value);
        selected.rows[0].element.selected = flip;
        assert!(
            compare(&selected, expected)
                .rows
                .iter()
                .any(|e| e.contains("выбрана"))
        );

        let mut lost = good.snapshot.clone();
        lost.selected_rows = Some(Vec::new());
        assert!(
            compare(&lost, expected)
                .rows
                .iter()
                .any(|e| e.contains("выбранные строки"))
        );

        let mut focus = good.snapshot.clone();
        focus.focused = Some(good.snapshot.rows[0].element.clone());
        assert!(!compare(&focus, expected).focus.is_empty());

        let mut missing = good.snapshot.clone();
        missing.rows.remove(10);
        assert!(
            compare(&missing, expected)
                .rows
                .iter()
                .any(|e| e.contains("видимые строки"))
        );

        let mut moved = good.snapshot.clone();
        if let Some(frame) = moved.rows[5].element.frame.as_mut() {
            frame.y += 2.0;
        }
        assert!(
            compare(&moved, expected)
                .rows
                .iter()
                .any(|e| e.contains("рамка"))
        );
    }

    #[test]
    fn reuse_counts_same_and_other_elements() {
        let diff = diff();
        let (mut snapshot, _) = sample(&rows(&diff, Mode::Unified, Some(0), None), 0);
        snapshot.rows[0].same_as_before = Some(true);
        snapshot.rows[1].same_as_before = Some(true);
        snapshot.rows[2].same_as_before = Some(false);
        assert_eq!(reuse(&snapshot), (2, 1));
    }
}
