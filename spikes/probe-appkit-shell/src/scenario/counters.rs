//! Счётчики живых экземпляров классов `define_class!`. Создание считается при построении данных
//! экземпляра перед `init`, освобождение — в `Drop` этих данных. Блоки, наблюдатели и системные
//! объекты счётчики не охватывают.
//!
//! Подключение к классу — поле `live: Live` в данных экземпляра, заполненное `COUNTER.live()`,
//! где `COUNTER` объявлен рядом с классом через `counter!`.
//!
//! Поздний обратный вызов — вызов метода или замыкания, который не нашёл своего владельца
//! живым: владелец освобождён, а объект, через который пришёл вызов, ещё нет. Места таких
//! вызовов отмечает `owner`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Объявляет статический счётчик класса: `counter!(STAND_TARGET, "ProbeStandTarget");`.
macro_rules! counter {
    ($ident:ident, $name:literal) => {
        static $ident: $crate::scenario::counters::Counter =
            $crate::scenario::counters::Counter::new($name);
    };
}
pub(crate) use counter;

pub struct Counter {
    name: &'static str,
    created: AtomicUsize,
    released: AtomicUsize,
    registered: AtomicBool,
}

static REGISTRY: Mutex<Vec<&'static Counter>> = Mutex::new(Vec::new());

impl Counter {
    pub const fn new(name: &'static str) -> Counter {
        Counter {
            name,
            created: AtomicUsize::new(0),
            released: AtomicUsize::new(0),
            registered: AtomicBool::new(false),
        }
    }

    /// Отметка живого экземпляра для поля данных экземпляра.
    pub fn live(&'static self) -> Live {
        if !self.registered.swap(true, Ordering::AcqRel) {
            REGISTRY
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(self);
        }
        self.created.fetch_add(1, Ordering::Relaxed);
        Live(self)
    }

    pub fn count(&self) -> Count {
        let created = self.created.load(Ordering::Relaxed);
        let released = self.released.load(Ordering::Relaxed);
        Count {
            name: self.name,
            created,
            released,
        }
    }
}

/// Живой экземпляр. Освобождение отмечается, когда AppKit освобождает объект с этими данными.
pub struct Live(&'static Counter);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.released.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Count {
    pub name: &'static str,
    pub created: usize,
    pub released: usize,
}

impl Count {
    pub fn live(&self) -> usize {
        self.created.saturating_sub(self.released)
    }
}

static LATE: Mutex<Vec<(&'static str, usize)>> = Mutex::new(Vec::new());

/// Владелец обратного вызова в месте `site`; `None` отмечается как поздний вызов.
pub fn owner<T>(site: &'static str, owner: Option<T>) -> Option<T> {
    if owner.is_none() {
        let mut late = LATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match late.iter_mut().find(|(known, _)| *known == site) {
            Some((_, count)) => *count += 1,
            None => late.push((site, 1)),
        }
    }
    owner
}

/// Поздние обратные вызовы по местам.
pub fn late_calls() -> Vec<(&'static str, usize)> {
    LATE.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Классы, у которых число живых экземпляров в `actual` отличается от `expected`: имя, живых
/// в `expected` и в `actual`. Класса, которого нет в снимке, в нём ноль живых.
pub fn live_changes(expected: &[Count], actual: &[Count]) -> Vec<(&'static str, usize, usize)> {
    let live = |counts: &[Count], name: &str| {
        counts
            .iter()
            .find(|count| count.name == name)
            .map_or(0, Count::live)
    };
    let mut names: Vec<&'static str> = expected
        .iter()
        .chain(actual)
        .map(|count| count.name)
        .collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .map(|name| (name, live(expected, name), live(actual, name)))
        .filter(|(_, before, after)| before != after)
        .collect()
}

/// Счётчики всех классов, у которых был хотя бы один экземпляр, по имени.
pub fn snapshot() -> Vec<Count> {
    let mut counts: Vec<Count> = REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .map(|counter| counter.count())
        .collect();
    counts.sort_by_key(|count| count.name);
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    counter!(FIRST, "TestFirst");
    counter!(SECOND, "TestSecond");
    counter!(UNUSED, "TestUnused");

    #[test]
    fn live_counts_creation_and_release() {
        let a = FIRST.live();
        let b = FIRST.live();
        assert_eq!(
            FIRST.count(),
            Count {
                name: "TestFirst",
                created: 2,
                released: 0
            }
        );
        drop(a);
        assert_eq!(FIRST.count().live(), 1);
        drop(b);
        let count = FIRST.count();
        assert_eq!((count.created, count.released, count.live()), (2, 2, 0));
    }

    #[test]
    fn snapshot_lists_only_used_classes_by_name() {
        let _second = SECOND.live();
        let names: Vec<&str> = snapshot().iter().map(|count| count.name).collect();
        assert!(names.contains(&"TestSecond"), "{names:?}");
        assert!(!names.contains(&UNUSED.count().name), "{names:?}");
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    fn count(name: &'static str, created: usize, released: usize) -> Count {
        Count {
            name,
            created,
            released,
        }
    }

    #[test]
    fn live_changes_compare_live_not_created() {
        let tenth = [count("A", 5, 2), count("B", 1, 0)];
        let hundredth = [count("A", 50, 47), count("B", 1, 0)];
        assert_eq!(live_changes(&tenth, &hundredth), vec![]);
        let leaked = [count("A", 50, 40), count("B", 1, 0)];
        assert_eq!(live_changes(&tenth, &leaked), vec![("A", 3, 10)]);
    }

    #[test]
    fn live_changes_treat_missing_class_as_zero() {
        let baseline = [count("Stand", 1, 0)];
        let closed = [count("Scene", 2, 2), count("Stand", 1, 0)];
        assert_eq!(live_changes(&baseline, &closed), vec![]);
        let leaked = [count("Scene", 2, 1), count("Stand", 1, 0)];
        assert_eq!(live_changes(&baseline, &leaked), vec![("Scene", 0, 1)]);
        assert_eq!(live_changes(&leaked, &baseline), vec![("Scene", 1, 0)]);
    }

    #[test]
    fn owner_counts_only_missing_owners() {
        assert_eq!(owner("test.alive", Some(1)), Some(1));
        assert_eq!(owner::<u8>("test.gone", None), None);
        assert_eq!(owner::<u8>("test.gone", None), None);
        let late = late_calls();
        assert!(late.contains(&("test.gone", 2)), "{late:?}");
        assert!(
            !late.iter().any(|(site, _)| *site == "test.alive"),
            "{late:?}"
        );
    }

    #[test]
    fn live_never_underflows() {
        let count = Count {
            name: "x",
            created: 1,
            released: 2,
        };
        assert_eq!(count.live(), 0);
    }
}
