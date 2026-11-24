//! Счётчики созданных и освобождённых экземпляров для `ui.stats`: классов `define_class!` и
//! владельцев Rust ([`crate::own::View`]), а также поздних обратных вызовов.
//!
//! Создание считается при построении данных экземпляра, освобождение — в `Drop` этих данных, то
//! есть когда Objective-C освобождает объект или Rust — владельца. Счётчики включены всегда: это
//! атомарные прибавления. Блоки, наблюдатели Core Foundation и системные объекты они не охватывают.
//!
//! Поздний обратный вызов — вызов из Objective-C, который не нашёл владельца живым: владелец
//! освобождён, а объект, через который пришёл вызов, ещё нет. Он ничего не делает и прибавляет
//! счётчик своего места.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Объявляет статический счётчик: `counter!(SURFACE_VIEW, Class, "ShellioSurface");`.
macro_rules! counter {
    ($ident:ident, $kind:ident, $name:literal) => {
        static $ident: $crate::counters::Counter =
            $crate::counters::Counter::new($crate::counters::CountKind::$kind, $name);
    };
}
pub(crate) use counter;

/// Что считает счётчик.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CountKind {
    /// Класс `define_class!`.
    Class,
    /// Владелец вида и его Rust-части.
    Owner,
}

impl CountKind {
    pub const fn name(self) -> &'static str {
        match self {
            CountKind::Class => "class",
            CountKind::Owner => "owner",
        }
    }
}

/// Счётчик одного класса или владельца. Регистрируется при первом экземпляре.
pub(crate) struct Counter {
    kind: CountKind,
    name: &'static str,
    created: AtomicU64,
    released: AtomicU64,
    registered: AtomicBool,
}

static REGISTRY: Mutex<Vec<&'static Counter>> = Mutex::new(Vec::new());

impl Counter {
    pub(crate) const fn new(kind: CountKind, name: &'static str) -> Counter {
        Counter {
            kind,
            name,
            created: AtomicU64::new(0),
            released: AtomicU64::new(0),
            registered: AtomicBool::new(false),
        }
    }

    /// Отметка живого экземпляра для поля его данных.
    pub(crate) fn live(&'static self) -> Live {
        if !self.registered.swap(true, Ordering::AcqRel) {
            REGISTRY
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(self);
        }
        self.created.fetch_add(1, Ordering::Relaxed);
        Live(self)
    }

    fn count(&self) -> InstanceCount {
        InstanceCount {
            kind: self.kind,
            name: self.name,
            created: self.created.load(Ordering::Relaxed),
            released: self.released.load(Ordering::Relaxed),
        }
    }
}

/// Живой экземпляр. Освобождение отмечается вместе с данными экземпляра.
pub(crate) struct Live(&'static Counter);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.released.fetch_add(1, Ordering::Relaxed);
    }
}

/// Созданные и освобождённые экземпляры одного класса или владельца.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceCount {
    pub kind: CountKind,
    pub name: &'static str,
    pub created: u64,
    pub released: u64,
}

impl InstanceCount {
    pub fn live(&self) -> u64 {
        self.created.saturating_sub(self.released)
    }
}

/// Счётчики всех классов и владельцев, у которых был хотя бы один экземпляр, по имени.
pub fn instance_counts() -> Vec<InstanceCount> {
    let mut counts: Vec<InstanceCount> = REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .map(|counter| counter.count())
        .collect();
    counts.sort_by_key(|count| count.name);
    counts
}

/// Поздние обратные вызовы одного места.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LateCallbacks {
    pub site: &'static str,
    pub count: u64,
}

static LATE: Mutex<Vec<LateCallbacks>> = Mutex::new(Vec::new());

/// Отмечает поздний обратный вызов в месте `site`.
pub(crate) fn late(site: &'static str) {
    let mut late = LATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match late.iter_mut().find(|known| known.site == site) {
        Some(known) => known.count += 1,
        None => late.push(LateCallbacks { site, count: 1 }),
    }
}

/// Отмечает поздний обратный вызов вне этого crate: хост видов считает им запрос и исход запроса
/// тела панели, которого уже нет.
pub fn late_callback(site: &'static str) {
    late(site);
}

/// Поздние обратные вызовы по местам, в порядке первого появления.
pub fn late_callbacks() -> Vec<LateCallbacks> {
    LATE.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Все поздние обратные вызовы процесса.
pub fn late_callbacks_total() -> u64 {
    late_callbacks().iter().map(|late| late.count).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    counter!(TEST_FIRST, Class, "TestCountersFirst");
    counter!(TEST_OWNER, Owner, "TestCountersOwner");
    counter!(TEST_UNUSED, Class, "TestCountersUnused");

    fn find(name: &str) -> Option<InstanceCount> {
        instance_counts()
            .into_iter()
            .find(|count| count.name == name)
    }

    #[test]
    fn live_counts_creation_and_release() {
        let a = TEST_FIRST.live();
        let b = TEST_FIRST.live();
        let count = TEST_FIRST.count();
        assert_eq!((count.created, count.released, count.live()), (2, 0, 2));
        drop(a);
        assert_eq!(TEST_FIRST.count().live(), 1);
        drop(b);
        let count = TEST_FIRST.count();
        assert_eq!((count.created, count.released, count.live()), (2, 2, 0));
        assert_eq!(count.kind, CountKind::Class);
    }

    #[test]
    fn registry_lists_only_used_counters_sorted_with_kind() {
        let owner = TEST_OWNER.live();
        let found = find("TestCountersOwner").unwrap();
        assert_eq!(found.kind, CountKind::Owner);
        assert_eq!(found.live(), 1);
        assert!(find("TestCountersUnused").is_none());
        assert_eq!(TEST_UNUSED.count().created, 0);
        let names: Vec<&str> = instance_counts().iter().map(|count| count.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        drop(owner);
        assert_eq!(find("TestCountersOwner").unwrap().live(), 0);
    }

    #[test]
    fn late_callbacks_are_counted_per_site() {
        late("test.counters.gone");
        late("test.counters.gone");
        late("test.counters.other");
        let all = late_callbacks();
        let site = |name: &str| all.iter().find(|late| late.site == name).map(|l| l.count);
        assert_eq!(site("test.counters.gone"), Some(2));
        assert_eq!(site("test.counters.other"), Some(1));
        assert_eq!(site("test.counters.never"), None);
        assert!(late_callbacks_total() >= 3);
    }

    #[test]
    fn host_counts_late_callbacks_through_the_public_entry() {
        late_callback("test.counters.host");
        let all = late_callbacks();
        let host = all.iter().find(|late| late.site == "test.counters.host");
        assert_eq!(host.map(|late| late.count), Some(1));
    }

    #[test]
    fn live_never_underflows() {
        let count = InstanceCount {
            kind: CountKind::Owner,
            name: "x",
            created: 1,
            released: 2,
        };
        assert_eq!(count.live(), 0);
        assert_eq!(CountKind::Class.name(), "class");
        assert_eq!(CountKind::Owner.name(), "owner");
    }
}
