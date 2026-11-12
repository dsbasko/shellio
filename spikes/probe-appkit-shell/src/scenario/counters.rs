//! Счётчики живых экземпляров классов `define_class!`. Создание считается при построении данных
//! экземпляра перед `init`, освобождение — в `Drop` этих данных. Блоки, наблюдатели и системные
//! объекты счётчики не охватывают.
//!
//! Подключение к классу — поле `live: Live` в данных экземпляра, заполненное `COUNTER.live()`,
//! где `COUNTER` объявлен рядом с классом через `counter!`.

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
