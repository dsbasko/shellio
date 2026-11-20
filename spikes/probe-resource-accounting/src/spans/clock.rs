//! Часы CPU потока для учёта участков: кандидаты правила «Часы потока» и выбранные часы.
//!
//! Выбор делает сценарий `clock-bench` по точности против `getrusage(RUSAGE_SELF)` в
//! однопоточном процессе и по стоимости чтения; итог записан в отчёт, раздел «Учёт участков».

use serde::{Deserialize, Serialize};

/// CPU вызывающего потока, нс. `None` — чтение отказало: участок не начисляется и помечается.
pub trait Clock: Clone + Send + Sync + Unpin + 'static {
    fn now_ns(&self) -> Option<u64>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockKind {
    ClockGettime,
    ThreadInfo,
}

impl ClockKind {
    pub const ALL: [ClockKind; 2] = [ClockKind::ClockGettime, ClockKind::ThreadInfo];

    pub fn name(self) -> &'static str {
        match self {
            ClockKind::ClockGettime => "clock_gettime",
            ClockKind::ThreadInfo => "thread_info",
        }
    }

    pub fn now_ns(self) -> Option<u64> {
        match self {
            ClockKind::ClockGettime => ClockGettime.now_ns(),
            ClockKind::ThreadInfo => ThreadInfo.now_ns(),
        }
    }
}

/// `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClockGettime;

impl Clock for ClockGettime {
    fn now_ns(&self) -> Option<u64> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: ts — действительный выходной буфер.
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        (rc == 0).then(|| (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64)
    }
}

/// `thread_info(THREAD_BASIC_INFO)` для порта из `pthread_mach_thread_np(pthread_self())`:
/// `user_time + system_time`, разрешение — микросекунда. Порт ищется на каждом чтении, как
/// было бы на каждом `poll`; `mach_thread_self()` не берётся: его право на порт пришлось бы
/// освобождать.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadInfo;

impl Clock for ThreadInfo {
    #[allow(deprecated)]
    fn now_ns(&self) -> Option<u64> {
        // SAFETY: thread_basic_info — POD, нули допустимы.
        let mut info: libc::thread_basic_info = unsafe { std::mem::zeroed() };
        let mut count = libc::THREAD_BASIC_INFO_COUNT;
        // SAFETY: pthread_self всегда действителен для вызывающего потока; порт не освобождается
        // (право не выдаётся), info и count — действительные выходные буферы нужного размера.
        let rc = unsafe {
            let port = libc::pthread_mach_thread_np(libc::pthread_self());
            libc::thread_info(
                port,
                libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                (&mut info as *mut libc::thread_basic_info).cast(),
                &mut count,
            )
        };
        if rc != libc::KERN_SUCCESS {
            return None;
        }
        let ns = |t: libc::time_value_t| {
            (t.seconds as u64) * 1_000_000_000 + (t.microseconds as u64) * 1_000
        };
        Some(ns(info.user_time) + ns(info.system_time))
    }
}

/// Часы учёта участков по правилу «Часы потока» (сценарий `clock-bench`).
pub type Chosen = ClockGettime;

/// Имя типа выбранных часов для записи рядом с выбором `clock-bench`.
pub fn chosen_name() -> &'static str {
    std::any::type_name::<Chosen>()
}

/// Подставные часы: у каждого потока свой счётчик, его двигает только сам поток.
#[cfg(test)]
pub mod fake {
    use std::cell::Cell;

    thread_local! {
        static NOW: Cell<u64> = const { Cell::new(0) };
        static BROKEN: Cell<bool> = const { Cell::new(false) };
    }

    #[derive(Clone, Copy, Debug, Default)]
    pub struct FakeClock;

    impl super::Clock for FakeClock {
        fn now_ns(&self) -> Option<u64> {
            (!BROKEN.get()).then(|| NOW.get())
        }
    }

    /// Поток тратит `ns` своего CPU.
    pub fn spend(ns: u64) {
        NOW.set(NOW.get() + ns);
    }

    /// Начало отсчёта потока: у разных потоков разные базы.
    pub fn set(ns: u64) {
        NOW.set(ns);
    }

    /// Чтения часов на этом потоке отказывают.
    pub fn break_reads(broken: bool) {
        BROKEN.set(broken);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_clocks_read_and_grow_on_own_work() {
        for kind in ClockKind::ALL {
            let start = kind.now_ns().expect("часы потока читаются");
            let mut x: u64 = 1;
            for _ in 0..2_000_000 {
                x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
            }
            let end = kind.now_ns().expect("часы потока читаются");
            assert!(end > start, "{}: {start} → {end}", kind.name());
        }
        assert!(chosen_name().ends_with("ClockGettime"));
    }

    #[test]
    fn fake_clock_is_per_thread() {
        fake::set(100);
        fake::spend(5);
        let other = std::thread::spawn(|| {
            fake::spend(7);
            fake::FakeClock.now_ns()
        })
        .join()
        .unwrap();
        assert_eq!(other, Some(7));
        assert_eq!(fake::FakeClock.now_ns(), Some(105));
        fake::break_reads(true);
        assert_eq!(fake::FakeClock.now_ns(), None);
        fake::break_reads(false);
    }
}
