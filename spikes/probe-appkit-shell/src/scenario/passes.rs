//! Конец прохода отображения. Наблюдатель `kCFRunLoopBeforeWaiting` главного цикла стоит в
//! общих режимах, включая отслеживание, с порядком после наблюдателей AppKit и Core Animation.
//! К его вызову AppKit выполнил `layout` и `display`, а транзакция Core Animation
//! зафиксирована. Это не момент появления кадра на экране.
//!
//! Таймер-предикат стенда будит цикл каждые 5 мс, поэтому проходы идут постоянно. Отметкой
//! изменения считается первый проход не раньше самого изменения: возврата `sendEvent` или
//! наблюдённого состояния.

use std::collections::VecDeque;
use std::time::Duration;

use block2::RcBlock;
use objc2_core_foundation::{
    CFIndex, CFRunLoop, CFRunLoopActivity, CFRunLoopObserver, kCFRunLoopCommonModes,
};
use serde::Serialize;

/// Порядок наблюдателя: после фиксации транзакции Core Animation (2 000 000) и до снятия пула
/// autorelease (2 147 483 647). Список наблюдателей главного цикла записывает `readback-check`.
pub const ORDER: CFIndex = 2_100_000;
/// Сколько последних проходов хранится: около 80 с при опросе раз в 5 мс.
const CAPACITY: usize = 16_384;

/// Отметки проходов от установки стенда, по возрастанию.
pub struct PassLog {
    times: VecDeque<Duration>,
    capacity: usize,
    /// Последняя вытесненная отметка: проход раньше неё уже не найти.
    evicted: Option<Duration>,
    total: u64,
}

impl Default for PassLog {
    fn default() -> Self {
        PassLog::with_capacity(CAPACITY)
    }
}

impl PassLog {
    pub fn with_capacity(capacity: usize) -> PassLog {
        PassLog {
            times: VecDeque::with_capacity(capacity.min(CAPACITY)),
            capacity: capacity.max(1),
            evicted: None,
            total: 0,
        }
    }

    pub fn record(&mut self, at: Duration) {
        if self.times.len() == self.capacity {
            self.evicted = self.times.pop_front();
        }
        self.times.push_back(at);
        self.total += 1;
    }

    /// Первый проход не раньше `after`. `None`, если его ещё не было или он вытеснен.
    pub fn first_after(&self, after: Duration) -> Option<Duration> {
        if self.evicted.is_some_and(|evicted| evicted >= after) {
            return None;
        }
        let index = self.times.partition_point(|time| *time < after);
        self.times.get(index).copied()
    }

    pub fn total(&self) -> u64 {
        self.total
    }
}

/// Ставит наблюдатель на главный цикл в общих режимах. `on_pass` вызывается на главном потоке.
pub fn install(on_pass: impl Fn() + 'static) -> Result<(), String> {
    let block = RcBlock::new(
        move |_observer: *mut CFRunLoopObserver, _activity: CFRunLoopActivity| on_pass(),
    );
    // SAFETY: распределитель по умолчанию, блок жив на время вызова и копируется наблюдателем.
    let observer = unsafe {
        CFRunLoopObserver::with_handler(
            None,
            CFRunLoopActivity::BeforeWaiting.0,
            true,
            ORDER,
            Some(&block),
        )
    }
    .ok_or("CFRunLoopObserverCreateWithHandler вернул NULL")?;
    let run_loop = CFRunLoop::main().ok_or("нет главного цикла CFRunLoop")?;
    // SAFETY: `kCFRunLoopCommonModes` — неизменяемая константа Core Foundation.
    run_loop.add_observer(Some(&observer), unsafe { kCFRunLoopCommonModes });
    Ok(())
}

/// Наблюдатель из описания цикла Core Foundation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ObserverInfo {
    pub activities: usize,
    pub order: isize,
    pub callout: String,
}

impl ObserverInfo {
    pub fn before_waiting(&self) -> bool {
        self.activities & CFRunLoopActivity::BeforeWaiting.0 != 0
    }
}

/// Наблюдатели главного цикла: из `CFCopyDescription`, без повторов между режимами, по порядку.
pub fn main_observers() -> Vec<ObserverInfo> {
    CFRunLoop::main()
        .map(|run_loop| parse_observers(&format!("{:?}", *run_loop)))
        .unwrap_or_default()
}

/// Разбирает строки вида `<CFRunLoopObserver …>{valid = Yes, activities = 0xa0, repeats = Yes,
/// order = 2000000, callout = имя (0x…), …}`.
pub fn parse_observers(description: &str) -> Vec<ObserverInfo> {
    let mut found: Vec<ObserverInfo> = Vec::new();
    for line in description.lines() {
        if !line.contains("CFRunLoopObserver") {
            continue;
        }
        let field = |name: &str| {
            let start = line.find(&format!("{name} = "))? + name.len() + 3;
            let rest = &line[start..];
            let end = rest.find([',', '}']).unwrap_or(rest.len());
            Some(rest[..end].trim())
        };
        let activities = field("activities")
            .and_then(|value| usize::from_str_radix(value.trim_start_matches("0x"), 16).ok());
        let order = field("order").and_then(|value| value.parse::<isize>().ok());
        let callout = field("callout")
            .map(|value| value.split(" (").next().unwrap_or(value).trim().to_owned());
        if let (Some(activities), Some(order), Some(callout)) = (activities, order, callout) {
            let info = ObserverInfo {
                activities,
                order,
                callout,
            };
            if !found.contains(&info) {
                found.push(info);
            }
        }
    }
    found.sort_by_key(|info| info.order);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn first_pass_not_before_change() {
        let mut log = PassLog::default();
        for at in [5, 10, 15] {
            log.record(ms(at));
        }
        assert_eq!(log.first_after(ms(0)), Some(ms(5)));
        assert_eq!(log.first_after(ms(10)), Some(ms(10)));
        assert_eq!(log.first_after(ms(11)), Some(ms(15)));
        assert_eq!(log.first_after(ms(16)), None);
        assert_eq!(log.total(), 3);
    }

    #[test]
    fn evicted_passes_are_unknown() {
        let mut log = PassLog::with_capacity(2);
        for at in [5, 10, 15] {
            log.record(ms(at));
        }
        assert_eq!(log.first_after(ms(1)), None, "проход 5 мс вытеснен");
        assert_eq!(log.first_after(ms(5)), None);
        assert_eq!(log.first_after(ms(6)), Some(ms(10)));
        assert_eq!(log.total(), 3);
    }

    #[test]
    fn parses_observer_lines() {
        let description = "\
<CFRunLoop 0x1>{wakeup port = 0x1, stopped = false,
\tmodes = <CFBasicHash>{
\t\tobservers = (
    \"<CFRunLoopObserver 0x6000>{valid = Yes, activities = 0xa0, repeats = Yes, order = 2000000, callout = _ZN2CA11Transaction17observer_callbackEP19__CFRunLoopObservermPv (0x18b), context = <CFRunLoopObserver context 0x0>}\",
    \"<CFRunLoopObserver 0x6001>{valid = Yes, activities = 0x1, repeats = Yes, order = -2147483647, callout = _runLoopObserverCallout (0x1), context = <CFArray>}\",
    \"<CFRunLoopObserver 0x6002>{valid = Yes, activities = 0x20, repeats = Yes, order = 2100000, callout = __block (0x2), context = <CFRunLoopObserver context 0x0>}\"
)}
\t\tobservers = (
    \"<CFRunLoopObserver 0x6000>{valid = Yes, activities = 0xa0, repeats = Yes, order = 2000000, callout = _ZN2CA11Transaction17observer_callbackEP19__CFRunLoopObservermPv (0x18b), context = <CFRunLoopObserver context 0x0>}\"
)}";
        let observers = parse_observers(description);
        assert_eq!(
            observers
                .iter()
                .map(|o| (o.order, o.activities))
                .collect::<Vec<_>>(),
            [(-2_147_483_647, 0x1), (2_000_000, 0xa0), (2_100_000, 0x20)]
        );
        assert_eq!(
            observers[1].callout,
            "_ZN2CA11Transaction17observer_callbackEP19__CFRunLoopObservermPv"
        );
        assert!(observers[1].before_waiting());
        assert!(!observers[0].before_waiting());
    }
}
