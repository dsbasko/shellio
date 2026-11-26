//! Чистая часть `VirtualList`: строки с устойчивыми id и выбор, разница наборов в диапазоны
//! номеров для `NSTableView`, клавиатурный выбор и геометрия прокрутки.
//!
//! Разница не пересоздаёт строки, которые можно переиспользовать (P0.2, находка 4): полная
//! перезагрузка таблицы создаёт новые виды строк, а перезагрузка отдельных строк и прокрутка — нет.
//! Поэтому полный `reloadData` идёт только при первом наполнении, а замена набора — удалением,
//! вставкой и перезагрузкой диапазонов строк.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::ops::Range;
use std::rc::Rc;

/// Вид строки: выбираемая строка или заголовок группы.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RowKind {
    #[default]
    Item,
    /// Заголовок группы: не выбирается ни щелчком, ни клавишами.
    Header,
}

/// Строка списка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Устойчивый id: разница и выбор идут по нему.
    pub id: String,
    pub text: String,
    pub kind: RowKind,
    /// Ревизия содержимого, которое показывает поставщик вида строки сверх `text` (значок,
    /// пояснение, клавиши): её смена перезагружает строку. У текстовой строки — 0.
    pub content: u64,
}

impl Row {
    pub fn new(id: &str, text: &str) -> Row {
        Row {
            id: id.to_owned(),
            text: text.to_owned(),
            kind: RowKind::Item,
            content: 0,
        }
    }

    /// Заголовок группы.
    pub fn header(id: &str, text: &str) -> Row {
        Row {
            kind: RowKind::Header,
            ..Row::new(id, text)
        }
    }

    pub fn with_content(self, content: u64) -> Row {
        Row { content, ..self }
    }

    pub fn selectable(&self) -> bool {
        self.kind == RowKind::Item
    }
}

/// Состояние списка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListState {
    /// AX-имя таблицы.
    pub label: String,
    /// Строки общие для состояний: смена одного выбора не копирует набор, а разница пропускает
    /// тот же набор без сравнения.
    pub rows: Rc<[Row]>,
    /// Id выбранной строки.
    pub selected: Option<String>,
}

impl Default for ListState {
    fn default() -> Self {
        ListState {
            label: String::new(),
            rows: Rc::from(Vec::new()),
            selected: None,
        }
    }
}

impl ListState {
    pub fn new(label: &str, rows: impl Into<Rc<[Row]>>, selected: Option<&str>) -> ListState {
        ListState {
            label: label.to_owned(),
            rows: rows.into(),
            selected: selected.map(str::to_owned),
        }
    }

    /// Номер выбранной строки.
    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_deref()?;
        self.rows.iter().position(|row| row.id == id)
    }
}

/// Ошибка состояния списка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListError {
    DuplicateId(String),
    UnknownSelection(String),
    HeaderSelected(String),
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListError::DuplicateId(id) => write!(f, "строка {id} повторяется"),
            ListError::UnknownSelection(id) => write!(f, "выбрана строка {id}, которой нет"),
            ListError::HeaderSelected(id) => write!(f, "выбран заголовок группы {id}"),
        }
    }
}

impl std::error::Error for ListError {}

/// Id строк уникальны, выбранная строка есть в наборе и не заголовок группы.
pub fn check(state: &ListState) -> Result<(), ListError> {
    let mut seen = HashSet::with_capacity(state.rows.len());
    for row in state.rows.iter() {
        if !seen.insert(row.id.as_str()) {
            return Err(ListError::DuplicateId(row.id.clone()));
        }
    }
    match &state.selected {
        Some(id) if !seen.contains(id.as_str()) => Err(ListError::UnknownSelection(id.clone())),
        Some(id)
            if state
                .rows
                .iter()
                .any(|row| row.id == *id && !row.selectable()) =>
        {
            Err(ListError::HeaderSelected(id.clone()))
        }
        _ => Ok(()),
    }
}

/// Замена набора строк диапазонами номеров. Порядок применения: сначала [`Self::remove`] по
/// номерам прежнего набора, затем [`Self::insert`] по номерам нового, затем [`Self::reload`] по
/// номерам нового. Каждое поле — одна операция таблицы с набором номеров.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowDiff {
    pub remove: Vec<Range<usize>>,
    pub insert: Vec<Range<usize>>,
    pub reload: Vec<Range<usize>>,
}

impl RowDiff {
    pub fn is_empty(&self) -> bool {
        self.remove.is_empty() && self.insert.is_empty() && self.reload.is_empty()
    }

    /// Число диапазонов во всех трёх операциях.
    pub fn ranges(&self) -> usize {
        self.remove.len() + self.insert.len() + self.reload.len()
    }
}

/// Накопитель номеров по возрастанию в слитые диапазоны.
#[derive(Default)]
struct Ranges(Vec<Range<usize>>);

impl Ranges {
    fn push(&mut self, index: usize) {
        match self.0.last_mut() {
            Some(last) if last.end == index => last.end += 1,
            _ => self.0.push(index..index + 1),
        }
    }

    fn push_range(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        match self.0.last_mut() {
            Some(last) if last.end == range.start => last.end = range.end,
            _ => self.0.push(range),
        }
    }
}

/// Разница наборов по id. Строки, оставшиеся в обоих наборах в прежнем взаимном порядке
/// (наибольшая возрастающая подпоследовательность прежних номеров), — якоря: они остаются на месте
/// и перезагружаются, только если изменились текст, вид или ревизия содержимого. Промежутки между соседними якорями
/// сопоставляются попарно по порядку: пара перезагружается на месте, вид строки при этом
/// переиспользуется. Остаток промежутка удаляется из прежнего набора или вставляется в новый.
/// Время — `O(n log n)`, диапазонов не больше чем по одному на промежуток и операцию. Id в каждом
/// наборе уникальны ([`check`]).
pub fn diff_rows(before: &[Row], after: &[Row]) -> RowDiff {
    let old_index: HashMap<&str, usize> = before
        .iter()
        .enumerate()
        .map(|(index, row)| (row.id.as_str(), index))
        .collect();
    let survivors: Vec<(usize, usize)> = after
        .iter()
        .enumerate()
        .filter_map(|(new, row)| old_index.get(row.id.as_str()).map(|old| (*old, new)))
        .collect();
    let anchors = longest_increasing(&survivors);

    let mut remove = Ranges::default();
    let mut insert = Ranges::default();
    let mut reload = Ranges::default();
    let (mut old_from, mut new_from) = (0, 0);
    let ends = anchors
        .iter()
        .copied()
        .map(Some)
        .chain(std::iter::once(None));
    for anchor in ends {
        let (old_to, new_to) = anchor.unwrap_or((before.len(), after.len()));
        let paired = (old_to - old_from).min(new_to - new_from);
        reload.push_range(new_from..new_from + paired);
        remove.push_range(old_from + paired..old_to);
        insert.push_range(new_from + paired..new_to);
        if let Some((old, new)) = anchor {
            if before[old] != after[new] {
                reload.push(new);
            }
            old_from = old + 1;
            new_from = new + 1;
        }
    }
    RowDiff {
        remove: remove.0,
        insert: insert.0,
        reload: reload.0,
    }
}

/// Наибольшая подпоследовательность пар `(старый, новый)`, возрастающая по старому номеру.
/// Пары идут по возрастанию нового номера. Терпеливая сортировка с восстановлением цепочки.
fn longest_increasing(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    // tails[k] — номер пары, которой кончается лучшая цепочка длины k + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut previous: Vec<Option<usize>> = vec![None; pairs.len()];
    for (index, (old, _)) in pairs.iter().enumerate() {
        let place = tails.partition_point(|&tail| pairs[tail].0 < *old);
        previous[index] = place.checked_sub(1).map(|before| tails[before]);
        if place == tails.len() {
            tails.push(index);
        } else {
            tails[place] = index;
        }
    }
    let mut chain = Vec::with_capacity(tails.len());
    let mut cursor = tails.last().copied();
    while let Some(index) = cursor {
        chain.push(pairs[index]);
        cursor = previous[index];
    }
    chain.reverse();
    chain
}

/// Операция над списком.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListOp {
    /// Первое наполнение: полный `reloadData`.
    Fill,
    /// Замена набора диапазонами строк.
    Rows(RowDiff),
    /// Выбор по номеру нового набора; `None` — выбора нет.
    Select(Option<usize>),
    Label(String),
}

/// Операции, которые переводят список из `before` в `after`; `before` равно `None` до первого
/// наполнения. Выбор называется всегда, когда меняется его id или номер.
pub fn diff(before: Option<&ListState>, after: &ListState) -> Vec<ListOp> {
    let mut ops = Vec::new();
    let Some(before) = before else {
        ops.push(ListOp::Label(after.label.clone()));
        ops.push(ListOp::Fill);
        ops.push(ListOp::Select(after.selected_index()));
        return ops;
    };
    if before.label != after.label {
        ops.push(ListOp::Label(after.label.clone()));
    }
    let rows = if Rc::ptr_eq(&before.rows, &after.rows) {
        RowDiff::default()
    } else {
        diff_rows(&before.rows, &after.rows)
    };
    let rows_changed = !rows.is_empty();
    if rows_changed {
        ops.push(ListOp::Rows(rows));
    }
    if rows_changed || before.selected != after.selected {
        ops.push(ListOp::Select(after.selected_index()));
    }
    ops
}

/// Намерение списка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListIntent {
    /// Выбрать строку: щелчок, Up и Down.
    Select { id: String },
    /// Открыть строку: `Enter`.
    Activate { id: String },
    /// На уровень назад: `Escape`. От строк и выбора не зависит.
    Cancel,
}

/// Клавиша, которую список обрабатывает сам при фокусе в нём.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKey {
    Up,
    Down,
    /// `Enter`: открыть выбранную строку.
    Enter,
    /// `Escape`: на уровень назад.
    Escape,
}

/// Намерение клавиши: Up и Down выбирают ближайшую выбираемую строку без перехода через край,
/// заголовки групп пропускаются; без выбора Down выбирает первую строку, а Up — последнюю. `Enter` открывает выбранную. `Escape` просит
/// уровень назад при любом наборе строк и выборе. `None` — клавиша ничего не делает.
pub fn key_intent(state: &ListState, key: ListKey) -> Option<ListIntent> {
    let index = state.selected_index();
    let rows = &state.rows;
    let selectable = |index: &usize| rows[*index].selectable();
    let target = match (key, index) {
        (ListKey::Escape, _) => return Some(ListIntent::Cancel),
        (ListKey::Down, Some(index)) => (index + 1..rows.len()).find(selectable),
        (ListKey::Up, Some(index)) => (0..index).rev().find(selectable),
        (ListKey::Down, None) => (0..rows.len()).find(selectable),
        (ListKey::Up, None) => (0..rows.len()).rev().find(selectable),
        (ListKey::Enter, Some(index)) => {
            return Some(ListIntent::Activate {
                id: state.rows[index].id.clone(),
            });
        }
        (ListKey::Enter, None) => None,
    }?;
    Some(ListIntent::Select {
        id: state.rows[target].id.clone(),
    })
}

/// Нажатие Up или Down для ряда задержек клавиш (план P1.3, «Замеры»). Время — секунды с
/// загрузки, часы одного процесса.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyPress {
    pub key: ListKey,
    /// `NSEvent.timestamp`: время создания события.
    pub event: f64,
    /// Последнее пробуждение цикла до входа в обработчик.
    pub wake: Option<f64>,
    /// Вход в `keyDown:` таблицы.
    pub entry: f64,
    /// Возврат обработчика `keyDown:`: намерение поставлено в очередь главного потока.
    pub handled: f64,
    /// Строка, которую выбирает нажатие.
    pub target: String,
    /// Состояние с этим выбором применено к таблице (выбор и прокрутка к нему): конец своего кода.
    pub applied: Option<f64>,
    /// Поколение состояния, в котором выбор уже сдвинут этим нажатием.
    pub generation: Option<u64>,
}

/// Журнал нажатий серии. Нажатие получает поколение в два шага: состояние с его выбором
/// применяется к таблице ([`Self::applied`]), затем окно публикует поколение ([`Self::stamp`]).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PressLog {
    presses: VecDeque<KeyPress>,
}

impl PressLog {
    /// Сколько последних нажатий держит журнал: серия — 10 прогревочных и 100 учитываемых.
    pub const CAPACITY: usize = 512;

    pub fn press(&mut self, press: KeyPress) {
        if self.presses.len() == Self::CAPACITY {
            self.presses.pop_front();
        }
        self.presses.push_back(press);
    }

    /// В момент `at` применено состояние с выбором `selected`: его получает самое раннее
    /// неприменённое нажатие с этой целью.
    pub fn applied(&mut self, selected: Option<&str>, at: f64) {
        let Some(selected) = selected else {
            return;
        };
        if let Some(press) = self
            .presses
            .iter_mut()
            .find(|press| press.applied.is_none() && press.target == selected)
        {
            press.applied = Some(at);
        }
    }

    /// Окно опубликовало поколение: его получают применённые нажатия без поколения.
    pub fn stamp(&mut self, generation: u64) {
        for press in &mut self.presses {
            if press.applied.is_some() && press.generation.is_none() {
                press.generation = Some(generation);
            }
        }
    }

    /// Новая серия.
    pub fn clear(&mut self) {
        self.presses.clear();
    }

    pub fn presses(&self) -> Vec<KeyPress> {
        self.presses.iter().cloned().collect()
    }
}

/// Смещение прокрутки, при котором строка `index` высотой `row` видна целиком в окне высотой
/// `visible` со смещением `offset`. `None` — строка уже видна. Сдвиг наименьший: строка выше окна
/// встаёт к верхнему краю, ниже — к нижнему. У окна без высоты (список ещё не размещён) прокрутки
/// нет: строка будет видна после размещения, а не сдвинута за верхний край.
pub fn reveal(index: usize, row: f64, offset: f64, visible: f64) -> Option<f64> {
    if visible <= 0.0 {
        return None;
    }
    let top = index as f64 * row;
    let bottom = top + row;
    if top < offset {
        Some(top)
    } else if bottom > offset + visible {
        Some((bottom - visible).max(0.0))
    } else {
        None
    }
}

/// Смещение, при котором строка `index` стоит у верхнего края окна, в пределах прокрутки.
pub fn scroll_to(index: usize, row: f64, visible: f64, count: usize) -> f64 {
    let max = (count as f64 * row - visible).max(0.0);
    (index as f64 * row).clamp(0.0, max)
}

/// Окно прокрутки списка с полями сверху и снизу (отступ области прокрутки): смещение и высота
/// считаются в части без полей, где в покое смещение 0, а начало clip view — `-top`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub top: f64,
    pub bottom: f64,
}

impl Viewport {
    /// Смещение и высота части без полей по началу и высоте clip view.
    pub fn window(self, origin: f64, height: f64) -> (f64, f64) {
        (
            origin + self.top,
            (height - self.top - self.bottom).max(0.0),
        )
    }

    /// Начало clip view для смещения части без полей.
    pub fn origin(self, offset: f64) -> f64 {
        offset - self.top
    }
}

/// Номера строк, которые хотя бы частично видны в окне.
pub fn visible_rows(row: f64, offset: f64, visible: f64, count: usize) -> Range<usize> {
    if row <= 0.0 || visible <= 0.0 || count == 0 {
        return 0..0;
    }
    let first = (offset.max(0.0) / row).floor() as usize;
    let end = ((offset.max(0.0) + visible) / row).ceil() as usize;
    first.min(count)..end.min(count)
}

/// Верхние края строк разной высоты: у заголовка группы своя высота. Нужны списку, где строки
/// и заголовки различаются по высоте, вместо расчётов по одной высоте строки.
#[derive(Debug, Clone, PartialEq)]
pub struct RowOffsets {
    /// `tops[i]` — верхний край строки `i`, последний элемент — полная высота.
    tops: Vec<f64>,
}

impl RowOffsets {
    pub fn new(rows: &[Row], item: f64, header: f64) -> RowOffsets {
        let mut tops = Vec::with_capacity(rows.len() + 1);
        let mut y = 0.0;
        tops.push(y);
        for row in rows {
            y += match row.kind {
                RowKind::Item => item,
                RowKind::Header => header,
            };
            tops.push(y);
        }
        RowOffsets { tops }
    }

    pub fn len(&self) -> usize {
        self.tops.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn top(&self, index: usize) -> f64 {
        self.tops[index.min(self.len())]
    }

    pub fn height(&self, index: usize) -> f64 {
        match self.tops.get(index + 1) {
            Some(bottom) => bottom - self.tops[index],
            None => 0.0,
        }
    }

    /// Высота всех строк.
    pub fn total(&self) -> f64 {
        self.tops[self.len()]
    }

    /// Номера строк, которые хотя бы частично видны в окне.
    pub fn visible(&self, offset: f64, visible: f64) -> Range<usize> {
        if visible <= 0.0 || self.is_empty() {
            return 0..0;
        }
        let from = offset.max(0.0);
        let to = from + visible;
        let first = self.tops[1..].partition_point(|bottom| *bottom <= from);
        let end = self.tops[..self.len()].partition_point(|top| *top < to);
        first.min(end)..end
    }

    /// Смещение, при котором строка `index` видна целиком, с наименьшим сдвигом, как [`reveal`].
    pub fn reveal(&self, index: usize, offset: f64, visible: f64) -> Option<f64> {
        if visible <= 0.0 || index >= self.len() {
            return None;
        }
        let top = self.top(index);
        let bottom = top + self.height(index);
        if top < offset {
            Some(top)
        } else if bottom > offset + visible {
            Some((bottom - visible).max(0.0))
        } else {
            None
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::single_range_in_vec_init,
    reason = "ожидания разницы — наборы диапазонов номеров"
)]
mod tests {
    use super::*;

    fn rows(ids: &[&str]) -> Vec<Row> {
        ids.iter()
            .map(|id| Row::new(id, &format!("{id}.")))
            .collect()
    }

    fn seeded(seed: u64, count: usize) -> Vec<Row> {
        (0..count)
            .map(|index| {
                Row::new(
                    &format!("s{seed}-{index:06}"),
                    &format!("Строка {index} набора {seed}"),
                )
            })
            .collect()
    }

    /// Таблица, которая применяет разницу так, как её применяет вид: удаление по прежним номерам,
    /// вставка по новым, перезагрузка по новым. Строка, которой не коснулись, остаётся прежней.
    fn apply(before: &[Row], diff: &RowDiff, after: &[Row]) -> Vec<Row> {
        let mut table: Vec<Option<Row>> = before.iter().cloned().map(Some).collect();
        let removed: HashSet<usize> = diff.remove.iter().flat_map(Clone::clone).collect();
        let mut index = 0;
        table.retain(|_| {
            let keep = !removed.contains(&index);
            index += 1;
            keep
        });
        for range in &diff.insert {
            for index in range.clone() {
                assert!(index <= table.len(), "вставка за краем: {index}");
                table.insert(index, None);
            }
        }
        assert_eq!(table.len(), after.len(), "длина после вставки");
        for range in &diff.reload {
            for index in range.clone() {
                table[index] = Some(after[index].clone());
            }
        }
        for range in &diff.insert {
            for index in range.clone() {
                table[index] = Some(after[index].clone());
            }
        }
        table
            .into_iter()
            .enumerate()
            .map(|(index, row)| row.unwrap_or_else(|| panic!("пустая строка {index}")))
            .collect()
    }

    fn assert_applies(before: &[Row], after: &[Row]) -> RowDiff {
        let diff = diff_rows(before, after);
        assert_eq!(apply(before, &diff, after), after);
        for ranges in [&diff.remove, &diff.insert, &diff.reload] {
            for pair in ranges.windows(2) {
                assert!(pair[0].end < pair[1].start, "диапазоны слиты и упорядочены");
            }
            assert!(ranges.iter().all(|range| !range.is_empty()));
        }
        diff
    }

    #[test]
    fn same_size_with_new_ids_reloads_in_place() {
        let diff = assert_applies(&seeded(1, 10_000), &seeded(2, 10_000));
        assert_eq!(
            diff,
            RowDiff {
                remove: vec![],
                insert: vec![],
                reload: vec![0..10_000],
            }
        );
    }

    #[test]
    fn insert_in_the_middle_is_one_insert() {
        let before = rows(&["a", "b", "c", "d"]);
        let after = rows(&["a", "b", "x", "y", "c", "d"]);
        let diff = assert_applies(&before, &after);
        assert_eq!(
            diff,
            RowDiff {
                remove: vec![],
                insert: vec![2..4],
                reload: vec![],
            }
        );
    }

    #[test]
    fn remove_in_the_middle_is_one_remove() {
        let before = rows(&["a", "b", "x", "y", "c", "d"]);
        let after = rows(&["a", "b", "c", "d"]);
        let diff = assert_applies(&before, &after);
        assert_eq!(diff.remove, vec![2..4]);
        assert!(diff.insert.is_empty() && diff.reload.is_empty());
    }

    #[test]
    fn moved_row_is_removed_and_inserted() {
        let before = rows(&["a", "b", "c", "d", "e"]);
        let after = rows(&["b", "c", "d", "e", "a"]);
        let diff = assert_applies(&before, &after);
        assert_eq!(
            diff,
            RowDiff {
                remove: vec![0..1],
                insert: vec![4..5],
                reload: vec![],
            }
        );
    }

    #[test]
    fn permutation_keeps_the_longest_ordered_run() {
        let before = rows(&["a", "b", "c", "d", "e", "f"]);
        let after = rows(&["f", "e", "d", "c", "b", "a"]);
        let diff = assert_applies(&before, &after);
        assert!(diff.ranges() <= 3, "{diff:?}");
        let swapped = rows(&["b", "a", "d", "c", "f", "e"]);
        assert_applies(&before, &swapped);
    }

    #[test]
    fn changed_text_reloads_only_that_row() {
        let before = rows(&["a", "b", "c"]);
        let mut after = before.clone();
        after[1].text = "другой".to_owned();
        let diff = assert_applies(&before, &after);
        assert_eq!(
            diff,
            RowDiff {
                remove: vec![],
                insert: vec![],
                reload: vec![1..2],
            }
        );
    }

    #[test]
    fn empty_sets() {
        let some = rows(&["a", "b", "c"]);
        assert_eq!(assert_applies(&[], &some).insert, vec![0..3]);
        assert_eq!(assert_applies(&some, &[]).remove, vec![0..3]);
        assert!(assert_applies(&[], &[]).is_empty());
        assert!(assert_applies(&some, &some).is_empty());
    }

    #[test]
    fn grow_and_shrink_with_new_ids_pair_the_common_part() {
        let diff = assert_applies(&seeded(1, 10), &seeded(2, 15));
        assert_eq!(diff.reload, vec![0..10]);
        assert_eq!(diff.insert, vec![10..15]);
        let diff = assert_applies(&seeded(1, 15), &seeded(2, 10));
        assert_eq!(diff.reload, vec![0..10]);
        assert_eq!(diff.remove, vec![10..15]);
    }

    #[test]
    fn hundred_thousand_rows_take_a_bounded_number_of_ranges() {
        let before = seeded(1, 100_000);
        let renewed = assert_applies(&before, &seeded(2, 100_000));
        assert_eq!(renewed.ranges(), 1);

        let mut inserted = before.clone();
        inserted.splice(50_000..50_000, seeded(3, 1_000));
        assert_eq!(assert_applies(&before, &inserted).ranges(), 1);

        let mut rotated = before.clone();
        rotated.rotate_left(1);
        assert_eq!(assert_applies(&before, &rotated).ranges(), 2);

        let mut mixed = before.clone();
        mixed.drain(10_000..20_000);
        mixed.splice(60_000..60_000, seeded(4, 500));
        mixed[80_000].text = "правка".to_owned();
        assert!(assert_applies(&before, &mixed).ranges() <= 3);
    }

    #[test]
    fn random_sets_apply_exactly() {
        let mut state = 0x5EED_u64;
        let mut next = move || {
            // SplitMix64: воспроизводимые наборы без внешних зависимостей.
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        for _ in 0..200 {
            let pool: Vec<String> = (0..24).map(|id| format!("r{id}")).collect();
            let mut pick = |len: u64| {
                let mut ids: Vec<&str> = pool.iter().map(String::as_str).collect();
                for index in (1..ids.len()).rev() {
                    let swap = (next() % (index as u64 + 1)) as usize;
                    ids.swap(index, swap);
                }
                let count = (next() % len) as usize;
                let mut picked = rows(&ids[..count]);
                for row in &mut picked {
                    if next() % 5 == 0 {
                        row.text.push('!');
                    }
                }
                picked
            };
            let before = pick(25);
            let after = pick(25);
            assert_applies(&before, &after);
        }
    }

    #[test]
    fn first_fill_reloads_everything_once() {
        let state = ListState::new("Список", rows(&["a", "b"]), Some("b"));
        assert_eq!(
            diff(None, &state),
            vec![
                ListOp::Label("Список".to_owned()),
                ListOp::Fill,
                ListOp::Select(Some(1)),
            ]
        );
        assert!(diff(Some(&state), &state).is_empty());
    }

    #[test]
    fn replacement_never_fills_and_renames_the_selection_index() {
        let before = ListState::new("Список", rows(&["a", "b", "c"]), Some("c"));
        let after = ListState::new("Список", rows(&["x", "a", "b", "c"]), Some("c"));
        let ops = diff(Some(&before), &after);
        assert!(!ops.contains(&ListOp::Fill));
        assert_eq!(ops.last(), Some(&ListOp::Select(Some(3))));
        let moved = ListState::new("Список", rows(&["a", "b", "c"]), Some("a"));
        assert_eq!(diff(Some(&before), &moved), vec![ListOp::Select(Some(0))]);
        let renamed = ListState {
            label: "Другой".to_owned(),
            ..before.clone()
        };
        assert_eq!(
            diff(Some(&before), &renamed),
            vec![ListOp::Label("Другой".to_owned())]
        );
    }

    #[test]
    fn shared_rows_skip_the_row_diff() {
        let before = ListState::new("Список", seeded(1, 100_000), Some("s1-000005"));
        let after = ListState {
            selected: Some("s1-000006".to_owned()),
            ..before.clone()
        };
        assert_eq!(diff(Some(&before), &after), vec![ListOp::Select(Some(6))]);
    }

    #[test]
    fn check_rejects_duplicates_and_unknown_selection() {
        assert_eq!(
            check(&ListState::new("", rows(&["a", "a"]), None)),
            Err(ListError::DuplicateId("a".to_owned()))
        );
        assert_eq!(
            check(&ListState::new("", rows(&["a"]), Some("z"))),
            Err(ListError::UnknownSelection("z".to_owned()))
        );
        assert_eq!(check(&ListState::new("", rows(&["a"]), Some("a"))), Ok(()));
    }

    #[test]
    fn keys_step_without_wrapping() {
        let select = |id: &str| Some(ListIntent::Select { id: id.to_owned() });
        let mut state = ListState::new("", rows(&["a", "b", "c"]), None);
        assert_eq!(key_intent(&state, ListKey::Down), select("a"));
        assert_eq!(key_intent(&state, ListKey::Up), select("c"));
        assert_eq!(key_intent(&state, ListKey::Enter), None);
        state.selected = Some("b".to_owned());
        assert_eq!(key_intent(&state, ListKey::Down), select("c"));
        assert_eq!(key_intent(&state, ListKey::Up), select("a"));
        assert_eq!(
            key_intent(&state, ListKey::Enter),
            Some(ListIntent::Activate { id: "b".to_owned() })
        );
        state.selected = Some("c".to_owned());
        assert_eq!(key_intent(&state, ListKey::Down), None);
        state.selected = Some("a".to_owned());
        assert_eq!(key_intent(&state, ListKey::Up), None);
        let empty = ListState::default();
        for key in [ListKey::Up, ListKey::Down, ListKey::Enter] {
            assert_eq!(key_intent(&empty, key), None);
        }
    }

    #[test]
    fn escape_cancels_whatever_the_rows_and_the_selection() {
        let states = [
            ListState::default(),
            ListState::new("", rows(&["a", "b"]), None),
            ListState::new("", rows(&["a", "b"]), Some("a")),
            ListState::new("", rows(&["a", "b"]), Some("b")),
        ];
        for state in states {
            assert_eq!(
                key_intent(&state, ListKey::Escape),
                Some(ListIntent::Cancel),
                "{state:?}"
            );
        }
    }

    fn press(target: &str, entry: f64) -> KeyPress {
        KeyPress {
            key: ListKey::Down,
            event: entry - 0.001,
            wake: Some(entry - 0.0005),
            entry,
            handled: entry + 0.0001,
            target: target.to_owned(),
            applied: None,
            generation: None,
        }
    }

    #[test]
    fn presses_get_the_generation_that_shows_their_selection() {
        let mut log = PressLog::default();
        log.press(press("b", 1.0));
        log.stamp(5);
        assert_eq!(log.presses()[0].generation, None, "до применения");
        log.applied(Some("b"), 2.0);
        log.press(press("c", 1.1));
        log.stamp(6);
        log.applied(Some("z"), 2.0);
        log.applied(None, 2.0);
        log.stamp(7);
        let presses = log.presses();
        assert_eq!(presses[0].generation, Some(6));
        assert_eq!(presses[0].applied, Some(2.0));
        assert_eq!(presses[1].generation, None);
        log.applied(Some("c"), 2.0);
        log.stamp(8);
        assert_eq!(log.presses()[1].generation, Some(8));
        log.clear();
        assert!(log.presses().is_empty());
    }

    #[test]
    fn repeated_target_goes_to_the_earliest_unapplied_press() {
        let mut log = PressLog::default();
        log.press(press("b", 1.0));
        log.press(press("b", 1.2));
        log.applied(Some("b"), 2.0);
        log.stamp(3);
        log.applied(Some("b"), 2.0);
        log.stamp(4);
        let generations: Vec<_> = log.presses().iter().map(|p| p.generation).collect();
        assert_eq!(generations, [Some(3), Some(4)]);
    }

    #[test]
    fn press_log_keeps_the_last_presses() {
        let mut log = PressLog::default();
        for index in 0..PressLog::CAPACITY + 3 {
            log.press(press(&index.to_string(), index as f64));
        }
        let presses = log.presses();
        assert_eq!(presses.len(), PressLog::CAPACITY);
        assert_eq!(presses[0].target, "3");
    }

    #[test]
    fn viewport_rests_at_zero_with_insets() {
        let view = Viewport {
            top: 4.0,
            bottom: 6.0,
        };
        assert_eq!(
            view.window(-4.0, 482.0),
            (0.0, 472.0),
            "покой — начало -top"
        );
        assert_eq!(view.origin(0.0), -4.0);
        assert_eq!(view.origin(30.0), 26.0);
        assert_eq!(
            view.window(0.0, 8.0),
            (4.0, 0.0),
            "высота без полей не меньше 0"
        );
        let rows = vec![
            Row::header("#1", "Команды"),
            Row::new("a", "a"),
            Row::new("b", "b"),
        ];
        let offsets = RowOffsets::new(&rows, 34.0, 32.0);
        let (offset, visible) = view.window(-4.0, 482.0);
        assert_eq!(
            offsets.reveal(1, offset, visible),
            None,
            "первая строка видна в покое"
        );
        let (offset, visible) = view.window(-4.0, 60.0);
        assert_eq!(offsets.reveal(1, offset, visible), Some(16.0));
        assert_eq!(view.origin(16.0), 12.0);
    }

    #[test]
    fn reveal_moves_the_least() {
        assert_eq!(reveal(5, 26.0, 0.0, 260.0), None);
        assert_eq!(reveal(9, 26.0, 0.0, 260.0), None);
        assert_eq!(reveal(10, 26.0, 0.0, 260.0), Some(26.0));
        assert_eq!(reveal(20, 26.0, 0.0, 260.0), Some(286.0));
        assert_eq!(reveal(2, 26.0, 100.0, 260.0), Some(52.0));
        assert_eq!(reveal(4, 26.0, 104.0, 260.0), None);
        assert_eq!(reveal(0, 26.0, 0.0, 0.0), None, "окно без высоты");
        assert_eq!(reveal(30, 26.0, 0.0, 0.0), None);
    }

    #[test]
    fn scroll_and_visible_rows_stay_in_bounds() {
        assert_eq!(scroll_to(0, 26.0, 260.0, 10_000), 0.0);
        assert_eq!(scroll_to(9_999, 26.0, 260.0, 10_000), 259_740.0);
        assert_eq!(scroll_to(100, 26.0, 260.0, 10_000), 2_600.0);
        assert_eq!(scroll_to(3, 26.0, 260.0, 5), 0.0);
        assert_eq!(visible_rows(26.0, 0.0, 260.0, 10_000), 0..10);
        assert_eq!(visible_rows(26.0, 13.0, 260.0, 10_000), 0..11);
        assert_eq!(visible_rows(26.0, 259_740.0, 260.0, 10_000), 9_990..10_000);
        assert_eq!(visible_rows(26.0, 0.0, 260.0, 3), 0..3);
        assert_eq!(visible_rows(26.0, 0.0, 260.0, 0), 0..0);
    }

    fn grouped() -> Vec<Row> {
        vec![
            Row::header("g1", "Команды"),
            Row::new("a", "А"),
            Row::new("b", "Б"),
            Row::header("g2", "Панели"),
            Row::new("c", "В"),
        ]
    }

    #[test]
    fn keys_skip_group_headers() {
        let rows = grouped();
        let state = |selected: Option<&str>| ListState::new("список", rows.clone(), selected);
        let select = |id: &str| Some(ListIntent::Select { id: id.to_owned() });
        assert_eq!(key_intent(&state(None), ListKey::Down), select("a"));
        assert_eq!(key_intent(&state(None), ListKey::Up), select("c"));
        assert_eq!(key_intent(&state(Some("b")), ListKey::Down), select("c"));
        assert_eq!(key_intent(&state(Some("c")), ListKey::Up), select("b"));
        assert_eq!(key_intent(&state(Some("a")), ListKey::Up), None);
        assert_eq!(key_intent(&state(Some("c")), ListKey::Down), None);
        let headers = ListState::new("список", vec![Row::header("g", "Г")], None);
        assert_eq!(key_intent(&headers, ListKey::Down), None);
        assert_eq!(key_intent(&headers, ListKey::Enter), None);
    }

    #[test]
    fn check_rejects_selected_header() {
        let state = ListState::new("список", grouped(), Some("g2"));
        assert_eq!(
            check(&state),
            Err(ListError::HeaderSelected("g2".to_owned()))
        );
        assert_eq!(
            check(&ListState::new("список", grouped(), Some("c"))),
            Ok(())
        );
    }

    #[test]
    fn changed_content_or_kind_reloads_the_anchor() {
        let before = grouped();
        let mut after = grouped();
        after[2] = after[2].clone().with_content(7);
        after[4] = Row::header("c", "В");
        let diff = assert_applies(&before, &after);
        assert_eq!(diff.reload, vec![2..3, 4..5]);
        assert!(diff.remove.is_empty() && diff.insert.is_empty());
    }

    #[test]
    fn offsets_follow_row_kinds() {
        let offsets = RowOffsets::new(&grouped(), 34.0, 32.0);
        assert_eq!(offsets.len(), 5);
        assert_eq!(offsets.top(1), 32.0);
        assert_eq!(offsets.top(3), 100.0);
        assert_eq!(offsets.height(3), 32.0);
        assert_eq!(offsets.height(5), 0.0);
        assert_eq!(offsets.total(), 166.0);
        assert_eq!(offsets.visible(0.0, 32.0), 0..1);
        assert_eq!(offsets.visible(31.0, 2.0), 0..2);
        assert_eq!(offsets.visible(100.0, 1000.0), 3..5);
        assert_eq!(offsets.visible(500.0, 10.0), 5..5);
        assert_eq!(offsets.visible(0.0, 0.0), 0..0);
        assert_eq!(offsets.reveal(4, 0.0, 120.0), Some(46.0));
        assert_eq!(offsets.reveal(1, 50.0, 120.0), Some(32.0));
        assert_eq!(offsets.reveal(2, 50.0, 120.0), None);
        assert_eq!(offsets.reveal(9, 0.0, 120.0), None);
        assert!(RowOffsets::new(&[], 34.0, 32.0).is_empty());
    }
}
