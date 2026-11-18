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

    pub fn is_waiting(&self) -> bool {
        self.pending.is_some()
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

type Hook = Box<dyn FnMut(&Mark)>;

thread_local! {
    static MARKS: RefCell<Vec<Mark>> = const { RefCell::new(Vec::new()) };
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!waiter.is_waiting());
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
}
