//! Отметка отрисовки по поколению. Вид в конце `drawRect:` сообщает поколение содержимого,
//! диапазон отрисованных строк показа и время начала и конца прохода. Стенд ждёт, пока текущим
//! поколением покрыты все видимые строки, а не конца прохода цикла (передача P0.2,
//! ограничение 4: отметка конца прохода один раз опередила `drawRect:`).
//!
//! Самопроверка: у каждого изменения содержимого отметка, которой стенд засчитал покрытие,
//! пришла не раньше начала `drawRect:` того же поколения, а начало — не раньше самого изменения.

use std::cell::RefCell;
use std::ops::Range;

use serde::{Deserialize, Serialize};

/// Одна отметка конца `drawRect:`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub generation: u64,
    pub rows: Range<usize>,
    pub draw_start_ns: u64,
    pub draw_end_ns: u64,
}

/// Покрытие диапазона строк отметками одного поколения.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    ranges: Vec<Range<usize>>,
}

impl Coverage {
    pub fn add(&mut self, rows: Range<usize>) {
        if rows.is_empty() {
            return;
        }
        self.ranges.push(rows);
        self.ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(self.ranges.len());
        for range in self.ranges.drain(..) {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        self.ranges = merged;
    }

    pub fn covers(&self, rows: &Range<usize>) -> bool {
        rows.is_empty()
            || self
                .ranges
                .iter()
                .any(|range| range.start <= rows.start && rows.end <= range.end)
    }
}

/// Изменение содержимого, которого ждёт стенд: поколение, видимые строки и время изменения.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub generation: u64,
    pub visible: Range<usize>,
    pub changed_ns: u64,
}

/// Итог ожидания одного изменения.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Covered {
    pub change: Change,
    /// Отметка, которая закрыла покрытие.
    pub mark: Mark,
    /// Число отметок этого поколения до покрытия включительно.
    pub marks: usize,
}

impl Covered {
    /// Нарушение порядка «изменение ≤ начало `drawRect:` ≤ отметка», если оно есть.
    pub fn order_violation(&self) -> Option<String> {
        let Covered { change, mark, .. } = self;
        if mark.draw_start_ns < change.changed_ns {
            return Some(format!(
                "поколение {}: drawRect: начат за {} нс до изменения",
                change.generation,
                change.changed_ns - mark.draw_start_ns
            ));
        }
        if mark.draw_end_ns < mark.draw_start_ns {
            return Some(format!(
                "поколение {}: отметка раньше начала drawRect:",
                change.generation
            ));
        }
        None
    }

    /// Время от изменения до конца `drawRect:`, наносекунды.
    pub fn latency_ns(&self) -> u64 {
        self.mark.draw_end_ns.saturating_sub(self.change.changed_ns)
    }
}

/// Ожидание покрытия: отметки младших поколений не засчитываются. Отметка старшего поколения
/// (например, снимок подсветки пришёл между изменением и отрисовкой) начинает покрытие заново
/// под этим поколением.
#[derive(Debug, Default)]
pub struct Waiter {
    pending: Option<(Change, Coverage, usize)>,
}

impl Waiter {
    pub fn expect(&mut self, change: Change) {
        self.pending = Some((change, Coverage::default(), 0));
    }

    /// Учитывает отметку. Возвращает итог, если она закрыла покрытие.
    pub fn observe(&mut self, mark: &Mark) -> Option<Covered> {
        let (change, coverage, marks) = self.pending.as_mut()?;
        if mark.generation < change.generation {
            return None;
        }
        if mark.generation > change.generation {
            change.generation = mark.generation;
            *coverage = Coverage::default();
            *marks = 0;
        }
        coverage.add(mark.rows.clone());
        *marks += 1;
        if !coverage.covers(&change.visible) {
            return None;
        }
        let (change, _, marks) = self.pending.take()?;
        Some(Covered {
            change,
            mark: mark.clone(),
            marks,
        })
    }
}

/// Изменение содержимого, объявленное видом: поколение и строки показа, которые оно затронуло.
/// `rows == None` — все строки: открытие, замена diff, смена ширины или режима. Прокрутка
/// содержимого не меняет и не объявляется.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declared {
    pub generation: u64,
    pub rows: Option<Vec<Range<usize>>>,
    pub at_ns: u64,
}

/// Поколение по строкам: отсортированные непересекающиеся полуинтервалы, соседние с одним
/// поколением слиты.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RowGenerations {
    ranges: Vec<(Range<usize>, u64)>,
}

impl RowGenerations {
    fn get(&self, row: usize) -> Option<u64> {
        let index = self.ranges.partition_point(|(range, _)| range.end <= row);
        self.ranges
            .get(index)
            .filter(|(range, _)| range.start <= row)
            .map(|(_, generation)| *generation)
    }

    /// Ставит поколение строкам `rows` поверх прежнего.
    fn set(&mut self, rows: Range<usize>, generation: u64) {
        if rows.is_empty() {
            return;
        }
        let mut next: Vec<(Range<usize>, u64)> = Vec::with_capacity(self.ranges.len() + 2);
        let mut placed = false;
        for (range, value) in self.ranges.drain(..) {
            if range.end <= rows.start {
                next.push((range, value));
                continue;
            }
            if range.start < rows.start {
                next.push((range.start..rows.start, value));
            }
            if !placed && range.start >= rows.start {
                next.push((rows.clone(), generation));
                placed = true;
            }
            if range.end > rows.end {
                if !placed {
                    next.push((rows.clone(), generation));
                    placed = true;
                }
                next.push((range.start.max(rows.end)..range.end, value));
            }
        }
        if !placed {
            next.push((rows, generation));
        }
        let mut merged: Vec<(Range<usize>, u64)> = Vec::with_capacity(next.len());
        for (range, value) in next {
            match merged.last_mut() {
                Some((last, last_value)) if last.end == range.start && *last_value == value => {
                    last.end = range.end;
                }
                _ => merged.push((range, value)),
            }
        }
        self.ranges = merged;
    }

    fn clear(&mut self) {
        self.ranges.clear();
    }

    fn len(&self) -> usize {
        self.ranges.len()
    }
}

/// Непокрытые видимые строки: первая из них, отставшие (нарисованы старым поколением) и
/// ненарисованные после последнего полного объявления.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Uncovered {
    pub first: usize,
    pub stale: usize,
    pub undrawn: usize,
}

/// Учёт покрытия по строкам для вида, который перерисовывает только затронутые строки. Строка
/// требует поколения последнего объявленного изменения, которое её затронуло, и покрыта, если
/// последняя отметка над ней не младше. Отметки идут с неубывающим поколением: вид рисует
/// текущее поколение, а объявляет его до отрисовки.
///
/// Покрытие — верхняя оценка: строка, отрисованная давно и выгруженная AppKit из кэша плиток,
/// считается покрытой до новой отрисовки. Содержимое такой строки не менялось.
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    /// Поколение последнего изменения всех строк.
    base: u64,
    /// Частичные изменения после него.
    required: RowGenerations,
    drawn: RowGenerations,
    latest: u64,
}

impl Ledger {
    pub fn declare(&mut self, declared: &Declared) {
        self.latest = self.latest.max(declared.generation);
        match &declared.rows {
            None => {
                self.base = declared.generation;
                self.required.clear();
                self.drawn.clear();
            }
            Some(rows) => {
                for range in rows {
                    self.required.set(range.clone(), declared.generation);
                }
            }
        }
    }

    pub fn mark(&mut self, mark: &Mark) {
        self.drawn.set(mark.rows.clone(), mark.generation);
    }

    /// Последнее объявленное поколение.
    pub fn latest(&self) -> u64 {
        self.latest
    }

    pub fn required(&self, row: usize) -> u64 {
        self.base.max(self.required.get(row).unwrap_or(0))
    }

    pub fn is_covered(&self, row: usize) -> bool {
        self.base > 0
            && self
                .drawn
                .get(row)
                .is_some_and(|drawn| drawn >= self.required(row))
    }

    /// Первая непокрытая строка из `visible`.
    pub fn uncovered(&self, visible: Range<usize>) -> Option<usize> {
        visible.into_iter().find(|&row| !self.is_covered(row))
    }

    /// Состав непокрытых строк `visible`: `None`, если все покрыты. Строка без отметки после
    /// последнего полного объявления не нарисована, строка с отметкой младше требуемой отстаёт.
    pub fn census(&self, visible: Range<usize>) -> Option<Uncovered> {
        let mut census: Option<Uncovered> = None;
        for row in visible {
            if self.is_covered(row) {
                continue;
            }
            let entry = census.get_or_insert(Uncovered {
                first: row,
                stale: 0,
                undrawn: 0,
            });
            if self.drawn.get(row).is_some() {
                entry.stale += 1;
            } else {
                entry.undrawn += 1;
            }
        }
        census
    }

    /// Число полуинтервалов учёта: для наблюдения за его ростом.
    pub fn size(&self) -> (usize, usize) {
        (self.required.len(), self.drawn.len())
    }
}

type Hook = Box<dyn FnMut(&Mark)>;

type DeclareHook = Box<dyn FnMut(&Declared)>;

thread_local! {
    static MARKS: RefCell<Vec<Mark>> = const { RefCell::new(Vec::new()) };
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    static DECLARED: RefCell<Vec<Declared>> = const { RefCell::new(Vec::new()) };
    static DECLARE_HOOK: RefCell<Option<DeclareHook>> = const { RefCell::new(None) };
}

/// Записывает объявленное изменение и передаёт его наблюдателю стенда. Вызывается видом в момент
/// смены поколения, до отрисовки.
pub fn declare(declared: Declared) {
    let taken = DECLARE_HOOK.with(|hook| hook.borrow_mut().take());
    if let Some(mut hook) = taken {
        hook(&declared);
        DECLARE_HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(hook);
            }
        });
    }
    DECLARED.with(|journal| journal.borrow_mut().push(declared));
}

/// Наблюдатель объявленных изменений; заменяет прежний.
pub fn on_declare(hook: impl FnMut(&Declared) + 'static) {
    DECLARE_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

/// Забирает журнал объявленных изменений.
pub fn take_declared() -> Vec<Declared> {
    DECLARED.with(|journal| std::mem::take(&mut *journal.borrow_mut()))
}

/// Записывает отметку и передаёт её наблюдателю стенда. Вызывается видом в конце `drawRect:`.
pub fn mark(mark: Mark) {
    let taken = HOOK.with(|hook| hook.borrow_mut().take());
    if let Some(mut hook) = taken {
        hook(&mark);
        HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(hook);
            }
        });
    }
    MARKS.with(|marks| marks.borrow_mut().push(mark));
}

/// Наблюдатель отметок; заменяет прежний.
pub fn on_mark(hook: impl FnMut(&Mark) + 'static) {
    HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

/// Забирает журнал отметок.
pub fn take() -> Vec<Mark> {
    MARKS.with(|marks| std::mem::take(&mut *marks.borrow_mut()))
}

/// Половина строки показа в журнале гуттера: unified, старая или новая.
pub type HalfKey = Option<probe_diff_view::model::Side>;

/// Журнал отрисованных строк сценариев контракта (строка отчёта «Журнал отрисованных строк
/// контракта»): последний прямоугольник текста и блоков каждой строки показа и последний
/// нарисованный текст гуттера каждой половины.
#[derive(Clone, Debug, Default)]
pub struct Journal {
    pub text: std::collections::BTreeMap<u32, super::anchor::Drawn>,
    pub blocks: std::collections::BTreeMap<u32, super::anchor::Drawn>,
    pub gutter: std::collections::BTreeMap<(u32, u8), (u64, String)>,
}

fn half_key(half: HalfKey) -> u8 {
    match half {
        None => 0,
        Some(probe_diff_view::model::Side::Old) => 1,
        Some(probe_diff_view::model::Side::New) => 2,
    }
}

thread_local! {
    static JOURNAL: RefCell<Option<Journal>> = const { RefCell::new(None) };
}

/// Включает журнал отрисованных строк с пустого или выключает его.
pub fn record_rows(on: bool) {
    JOURNAL.with(|journal| *journal.borrow_mut() = on.then(Journal::default));
}

/// Журнал включён: вид пишет в него при отрисовке.
pub fn recording() -> bool {
    JOURNAL.with(|journal| journal.borrow().is_some())
}

fn with_journal(write: impl FnOnce(&mut Journal)) {
    JOURNAL.with(|journal| {
        if let Some(journal) = journal.borrow_mut().as_mut() {
            write(journal);
        }
    });
}

/// Прямоугольник нарисованного текста строки показа.
pub fn drawn_text(drawn: super::anchor::Drawn) {
    with_journal(|journal| {
        journal.text.insert(drawn.row, drawn);
    });
}

/// Прямоугольник нарисованных блоков строки показа.
pub fn drawn_block(drawn: super::anchor::Drawn) {
    with_journal(|journal| {
        journal.blocks.insert(drawn.row, drawn);
    });
}

/// Нарисованный текст гуттера половины строки показа.
pub fn drawn_gutter(generation: u64, row: u32, half: HalfKey, text: &str) {
    with_journal(|journal| {
        journal
            .gutter
            .insert((row, half_key(half)), (generation, text.to_owned()));
    });
}

/// Последний нарисованный текст и блоки строки показа.
pub fn drawn_row(row: u32) -> (Option<super::anchor::Drawn>, Option<super::anchor::Drawn>) {
    JOURNAL.with(|journal| {
        journal.borrow().as_ref().map_or((None, None), |journal| {
            (
                journal.text.get(&row).copied(),
                journal.blocks.get(&row).copied(),
            )
        })
    })
}

/// Записей журнала: строк с текстом, блоками и гуттером. Ноль после сброса — ничего не нарисовано.
pub fn journal_entries() -> usize {
    JOURNAL.with(|journal| {
        journal.borrow().as_ref().map_or(0, |journal| {
            journal.text.len() + journal.blocks.len() + journal.gutter.len()
        })
    })
}

/// Последний нарисованный текст гуттера половины строки показа и его поколение.
pub fn gutter_of(row: u32, half: HalfKey) -> Option<(u64, String)> {
    JOURNAL.with(|journal| {
        journal
            .borrow()
            .as_ref()
            .and_then(|journal| journal.gutter.get(&(row, half_key(half))).cloned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_journal_records_only_while_on() {
        use super::super::anchor::Drawn;
        let drawn = Drawn {
            generation: 3,
            row: 7,
            top: 10.0,
            height: 15.0,
        };
        record_rows(false);
        drawn_text(drawn);
        assert!(!recording());
        assert_eq!(drawn_row(7), (None, None));
        record_rows(true);
        drawn_text(drawn);
        drawn_block(Drawn { top: 25.0, ..drawn });
        drawn_gutter(3, 7, Some(probe_diff_view::model::Side::Old), " 7 - ");
        assert_eq!(drawn_row(7).0, Some(drawn));
        assert_eq!(drawn_row(7).1.map(|block| block.top), Some(25.0));
        assert_eq!(
            gutter_of(7, Some(probe_diff_view::model::Side::Old)),
            Some((3, " 7 - ".to_owned()))
        );
        assert_eq!(gutter_of(7, None), None);
        record_rows(false);
        assert_eq!(drawn_row(7), (None, None));
    }

    fn mark(generation: u64, rows: Range<usize>, start: u64, end: u64) -> Mark {
        Mark {
            generation,
            rows,
            draw_start_ns: start,
            draw_end_ns: end,
        }
    }

    #[test]
    fn coverage_merges_adjacent_ranges() {
        let mut coverage = Coverage::default();
        coverage.add(10..20);
        assert!(!coverage.covers(&(10..30)));
        coverage.add(25..30);
        assert!(!coverage.covers(&(10..30)));
        coverage.add(20..25);
        assert!(coverage.covers(&(10..30)));
        assert!(coverage.covers(&(0..0)));
        coverage.add(5..5);
        assert!(!coverage.covers(&(5..10)));
    }

    #[test]
    fn waiter_ignores_other_generations() {
        let mut waiter = Waiter::default();
        waiter.expect(Change {
            generation: 2,
            visible: 0..40,
            changed_ns: 100,
        });
        assert_eq!(waiter.observe(&mark(1, 0..40, 110, 120)), None);
        assert_eq!(waiter.observe(&mark(2, 0..20, 130, 140)), None);
        assert_eq!(waiter.observe(&mark(3, 0..20, 150, 160)), None);
        assert_eq!(waiter.observe(&mark(2, 20..40, 150, 160)), None);
        let covered = waiter.observe(&mark(3, 20..40, 150, 160)).unwrap();
        assert_eq!(covered.change.generation, 3);
        assert_eq!(covered.marks, 2);
        assert_eq!(covered.latency_ns(), 60);
        assert_eq!(covered.order_violation(), None);
        assert_eq!(waiter.observe(&mark(2, 0..40, 170, 180)), None);
    }

    #[test]
    fn order_violation_is_reported() {
        let change = Change {
            generation: 1,
            visible: 0..1,
            changed_ns: 100,
        };
        let early = Covered {
            change: change.clone(),
            mark: mark(1, 0..1, 90, 120),
            marks: 1,
        };
        assert!(early.order_violation().is_some());
        let reversed = Covered {
            change,
            mark: mark(1, 0..1, 130, 120),
            marks: 1,
        };
        assert!(reversed.order_violation().is_some());
    }

    #[test]
    fn hook_sees_marks_in_order() {
        let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
        let sink = seen.clone();
        on_mark(move |mark| sink.borrow_mut().push(mark.generation));
        super::mark(mark(1, 0..1, 1, 2));
        super::mark(mark(2, 0..1, 3, 4));
        assert_eq!(*seen.borrow(), vec![1, 2]);
        assert_eq!(take().len(), 2);
        assert!(take().is_empty());
    }

    fn declared(generation: u64, rows: Option<Vec<Range<usize>>>) -> Declared {
        Declared {
            generation,
            rows,
            at_ns: 0,
        }
    }

    #[test]
    fn row_generations_overwrite_and_merge() {
        let mut map = RowGenerations::default();
        map.set(0..10, 1);
        map.set(3..5, 2);
        assert_eq!(map.ranges, vec![(0..3, 1), (3..5, 2), (5..10, 1)]);
        map.set(3..5, 1);
        assert_eq!(map.ranges, vec![(0..10, 1)]);
        map.set(8..12, 3);
        map.set(20..22, 3);
        assert_eq!(map.ranges, vec![(0..8, 1), (8..12, 3), (20..22, 3)]);
        map.set(0..30, 4);
        assert_eq!(map.ranges, vec![(0..30, 4)]);
        assert_eq!(map.get(29), Some(4));
        assert_eq!(map.get(30), None);
        map.set(5..5, 9);
        assert_eq!(map.ranges, vec![(0..30, 4)]);
    }

    #[test]
    fn row_generations_match_a_per_row_reference() {
        let mut map = RowGenerations::default();
        let mut reference = [None::<u64>; 64];
        let mut seed = 0x6d61_726bu64;
        for generation in 1..400u64 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let a = (seed >> 33) as usize % 64;
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let b = (seed >> 33) as usize % 64;
            let (start, end) = (a.min(b), a.max(b));
            let value = generation % 5;
            map.set(start..end, value);
            for slot in &mut reference[start..end] {
                *slot = Some(value);
            }
            for (row, expected) in reference.iter().enumerate() {
                assert_eq!(map.get(row), *expected, "строка {row}, шаг {generation}");
            }
            for pair in map.ranges.windows(2) {
                assert!(pair[0].0.end <= pair[1].0.start);
                assert!(pair[0].0.end < pair[1].0.start || pair[0].1 != pair[1].1);
            }
        }
    }

    #[test]
    fn ledger_needs_all_visible_rows_after_open() {
        let mut ledger = Ledger::default();
        assert_eq!(ledger.uncovered(0..3), Some(0));
        ledger.declare(&declared(1, None));
        assert_eq!(ledger.uncovered(0..40), Some(0));
        ledger.mark(&mark(1, 0..20, 10, 20));
        assert_eq!(ledger.uncovered(0..40), Some(20));
        ledger.mark(&mark(1, 20..40, 30, 40));
        assert_eq!(ledger.uncovered(0..40), None);
        assert_eq!(ledger.latest(), 1);
    }

    #[test]
    fn ledger_partial_change_needs_only_its_rows() {
        let mut ledger = Ledger::default();
        ledger.declare(&declared(1, None));
        ledger.mark(&mark(1, 0..40, 10, 20));
        // Снимок подсветки затронул строки 5–7 и 30: остальные покрыты прежним поколением.
        ledger.declare(&declared(2, Some(vec![5..8, 30..31])));
        assert_eq!(ledger.uncovered(0..40), Some(5));
        ledger.mark(&mark(2, 5..8, 30, 40));
        assert_eq!(ledger.uncovered(0..40), Some(30));
        ledger.mark(&mark(2, 30..31, 50, 60));
        assert_eq!(ledger.uncovered(0..40), None);
        assert_eq!(ledger.required(6), 2);
        assert_eq!(ledger.required(10), 1);
    }

    #[test]
    fn census_splits_stale_and_undrawn_rows() {
        let mut ledger = Ledger::default();
        ledger.declare(&declared(1, None));
        assert_eq!(
            ledger.census(0..4),
            Some(Uncovered {
                first: 0,
                stale: 0,
                undrawn: 4
            })
        );
        ledger.mark(&mark(1, 0..20, 10, 20));
        assert_eq!(ledger.census(0..20), None);
        // Подсветка отстала на строках 12–14, ниже прокрутка открыла ненарисованные 20–29:
        // первая непокрытая строка — отставшая, но ненарисованные тоже считаются.
        ledger.declare(&declared(2, Some(std::iter::once(12..15).collect())));
        assert_eq!(
            ledger.census(5..30),
            Some(Uncovered {
                first: 12,
                stale: 3,
                undrawn: 10
            })
        );
        ledger.mark(&mark(2, 12..15, 30, 40));
        ledger.mark(&mark(2, 20..30, 50, 60));
        assert_eq!(ledger.census(5..30), None);
        ledger.declare(&declared(3, None));
        assert_eq!(
            ledger.census(0..2),
            Some(Uncovered {
                first: 0,
                stale: 0,
                undrawn: 2
            })
        );
    }

    #[test]
    fn ledger_keeps_rows_drawn_before_a_scroll() {
        let mut ledger = Ledger::default();
        ledger.declare(&declared(1, None));
        ledger.mark(&mark(1, 0..40, 10, 20));
        // Прокрутка на 10 строк: AppKit рисует только открывшуюся полосу.
        assert_eq!(ledger.uncovered(10..50), Some(40));
        ledger.mark(&mark(1, 40..50, 30, 40));
        assert_eq!(ledger.uncovered(10..50), None);
    }

    #[test]
    fn ledger_resets_on_full_change_and_ignores_older_marks() {
        let mut ledger = Ledger::default();
        ledger.declare(&declared(1, None));
        ledger.mark(&mark(1, 0..40, 10, 20));
        ledger.declare(&declared(2, Some(std::iter::once(0..40).collect())));
        ledger.declare(&declared(3, None));
        assert_eq!(ledger.uncovered(0..10), Some(0));
        ledger.mark(&mark(2, 0..10, 30, 40));
        assert_eq!(ledger.uncovered(0..10), Some(0));
        ledger.mark(&mark(3, 0..10, 50, 60));
        assert_eq!(ledger.uncovered(0..10), None);
        assert_eq!(ledger.size(), (0, 1));
    }

    #[test]
    fn declare_reaches_hook_and_journal() {
        let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
        {
            let seen = std::rc::Rc::clone(&seen);
            on_declare(move |declared| seen.borrow_mut().push(declared.generation));
        }
        take_declared();
        declare(declared(7, None));
        declare(declared(8, Some(std::iter::once(1..2).collect())));
        assert_eq!(*seen.borrow(), vec![7, 8]);
        assert_eq!(take_declared().len(), 2);
        on_declare(|_| {});
    }
}
