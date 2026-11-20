//! Единицы `ri_user_time` и `ri_system_time`: перевод тиков Mach через `mach_timebase_info` и его
//! калибровка против `getrusage(RUSAGE_SELF)` (сценарий `units`).
//!
//! [`Timebase::ticks_to_ns`] — единственный путь перевода: им пользуются проверка фона и снимок,
//! и его же проверяет сценарий `units`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Доля окна образца в допуске: 0,5 п. п. одного ядра строки «CPU% процесса» таблицы оракулов.
pub const WINDOW_FRACTION: f64 = 0.005;
/// Относительная часть допуска: 2% оракула.
pub const RELATIVE: f64 = 0.02;
/// Образец входит в клетку `units-system`, если оракул system не ниже этой доли окна.
pub const SYSTEM_SIGNAL_FRACTION: f64 = 0.10;
/// Длительности образцов по монотонным часам.
pub const DURATIONS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];
/// Повторы каждой пары нагрузки и длительности.
pub const REPEATS: usize = 2;
const READ_CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timebase {
    pub numer: u32,
    pub denom: u32,
}

impl Timebase {
    #[allow(deprecated)]
    pub fn current() -> Timebase {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: info — действительный выходной буфер.
        unsafe { libc::mach_timebase_info(&mut info) };
        Timebase {
            numer: info.numer,
            denom: info.denom,
        }
    }

    /// Тики Mach в наносекунды; `None` — `timebase` не прочитан.
    pub fn ticks_to_ns(self, ticks: u64) -> Option<u64> {
        if self.denom == 0 {
            return None;
        }
        let ns = u128::from(ticks) * u128::from(self.numer) / u128::from(self.denom);
        Some(u64::try_from(ns).unwrap_or(u64::MAX))
    }

    /// Тик Mach равен наносекунде: гипотезы «тики Mach» и «наносекунды» не различаются.
    pub fn is_identity(self) -> bool {
        self.denom != 0 && self.numer == self.denom
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Load {
    /// Арифметический занятый цикл.
    User,
    /// Чтение по 64 КиБ из `/dev/zero`.
    System,
}

/// Один образец калибровки: дельты оракула и сырых счётчиков на границах окна.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub load: Load,
    pub target_ms: u64,
    /// Окно образца по `CLOCK_UPTIME_RAW`, нс.
    pub wall_ns: u64,
    /// `getrusage(RUSAGE_SELF)`: дельты `ru_utime` и `ru_stime`, нс.
    pub oracle_user_ns: u64,
    pub oracle_system_ns: u64,
    /// `proc_pid_rusage`: дельты `ri_user_time` и `ri_system_time` в сырых единицах.
    pub raw_user: u64,
    pub raw_system: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    User,
    System,
    Total,
}

impl Sample {
    pub fn oracle_ns(&self, component: Component) -> u64 {
        match component {
            Component::User => self.oracle_user_ns,
            Component::System => self.oracle_system_ns,
            Component::Total => self.oracle_user_ns + self.oracle_system_ns,
        }
    }

    pub fn raw(&self, component: Component) -> u64 {
        match component {
            Component::User => self.raw_user,
            Component::System => self.raw_system,
            Component::Total => self.raw_user + self.raw_system,
        }
    }
}

/// Допуск образца, нс.
pub fn tolerance_ns(wall_ns: u64, oracle_ns: u64) -> f64 {
    WINDOW_FRACTION * wall_ns as f64 + RELATIVE * oracle_ns as f64
}

/// Сравнение одного компонента образца с оракулом по обеим гипотезам.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    pub load: Load,
    pub target_ms: u64,
    pub component: Component,
    pub oracle_ns: u64,
    pub tolerance_ns: f64,
    /// Гипотеза «тики Mach»: перевод через `timebase`.
    pub mach_ns: Option<u64>,
    pub mach_ratio: Option<f64>,
    pub mach_ok: bool,
    /// Гипотеза «наносекунды»: сырое значение как есть.
    pub raw_ns: u64,
    pub raw_ratio: Option<f64>,
    pub raw_ok: bool,
}

fn ratio(value: u64, oracle: u64) -> Option<f64> {
    (oracle > 0).then(|| value as f64 / oracle as f64)
}

pub fn compare(sample: &Sample, component: Component, timebase: Timebase) -> Comparison {
    let oracle_ns = sample.oracle_ns(component);
    let raw = sample.raw(component);
    let tolerance = tolerance_ns(sample.wall_ns, oracle_ns);
    let within = |value: u64| (value as f64 - oracle_ns as f64).abs() <= tolerance;
    let mach_ns = timebase.ticks_to_ns(raw);
    Comparison {
        load: sample.load,
        target_ms: sample.target_ms,
        component,
        oracle_ns,
        tolerance_ns: tolerance,
        mach_ns,
        mach_ratio: mach_ns.and_then(|ns| ratio(ns, oracle_ns)),
        mach_ok: mach_ns.is_some_and(within),
        raw_ns: raw,
        raw_ratio: ratio(raw, oracle_ns),
        raw_ok: within(raw),
    }
}

/// Итог клетки по сравнениям её образцов.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// `timebase` 1/1: гипотезы совпадают, итог даёт только допуск «тиков Mach».
    PassIndistinct,
    Fail,
    /// Обе гипотезы в допуске во всех образцах.
    Undecided,
    /// Ни одного образца с сигналом.
    NoSignal,
}

pub fn verdict(comparisons: &[Comparison], timebase: Timebase) -> Verdict {
    if comparisons.is_empty() {
        return Verdict::NoSignal;
    }
    if comparisons.iter().any(|c| !c.mach_ok) {
        return Verdict::Fail;
    }
    if timebase.is_identity() {
        return Verdict::PassIndistinct;
    }
    if comparisons.iter().all(|c| c.raw_ok) {
        Verdict::Undecided
    } else {
        Verdict::Pass
    }
}

/// Сравнения клетки: компонент system берётся только из образцов `system` с сигналом.
pub fn cell_comparisons(
    samples: &[Sample],
    component: Component,
    timebase: Timebase,
) -> Vec<Comparison> {
    samples
        .iter()
        .filter(|sample| match component {
            Component::User => sample.load == Load::User,
            Component::System => {
                sample.load == Load::System
                    && sample.oracle_system_ns as f64
                        >= SYSTEM_SIGNAL_FRACTION * sample.wall_ns as f64
            }
            Component::Total => true,
        })
        .map(|sample| compare(sample, component, timebase))
        .collect()
}

/// План образцов: каждая пара нагрузки и длительности [`REPEATS`] раз, нагрузки чередуются.
pub fn schedule() -> Vec<(Load, Duration)> {
    let mut plan = Vec::new();
    for _ in 0..REPEATS {
        for duration in DURATIONS {
            for load in [Load::User, Load::System] {
                plan.push((load, duration));
            }
        }
    }
    plan
}

/// `getrusage(RUSAGE_SELF)`: user и system, нс.
pub fn self_getrusage() -> Result<(u64, u64), String> {
    // SAFETY: rusage — POD, нули допустимы.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: usage — действительный выходной буфер.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return Err(format!(
            "getrusage(RUSAGE_SELF): {}",
            std::io::Error::last_os_error()
        ));
    }
    let ns = |tv: libc::timeval| (tv.tv_sec as u64) * 1_000_000_000 + (tv.tv_usec as u64) * 1_000;
    Ok((ns(usage.ru_utime), ns(usage.ru_stime)))
}

fn self_raw() -> Result<(u64, u64), String> {
    let pid = std::process::id() as i32;
    super::usage(pid)
        .map(|u| (u.user_mach, u.system_mach))
        .map_err(|err| format!("proc_pid_rusage(своего PID): {err}"))
}

pub(crate) fn busy_user(deadline_ns: u64) {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    loop {
        for _ in 0..4096 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
        }
        std::hint::black_box(x);
        if crate::background::uptime_ns() >= deadline_ns {
            return;
        }
    }
}

pub(crate) fn busy_system(deadline_ns: u64) -> Result<(), String> {
    let zero = std::fs::File::open("/dev/zero").map_err(|err| format!("/dev/zero: {err}"))?;
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        // SAFETY: буфер на READ_CHUNK байт, дескриптор открыт.
        let got = unsafe {
            libc::read(
                std::os::fd::AsRawFd::as_raw_fd(&zero),
                buffer.as_mut_ptr().cast(),
                READ_CHUNK,
            )
        };
        if got < 0 {
            return Err(format!(
                "read(/dev/zero): {}",
                std::io::Error::last_os_error()
            ));
        }
        std::hint::black_box(&buffer);
        if crate::background::uptime_ns() >= deadline_ns {
            return Ok(());
        }
    }
}

/// Один образец: границы зеркальные — оракул, сырое; нагрузка; сырое, оракул.
pub fn measure(load: Load, duration: Duration) -> Result<Sample, String> {
    let wall_start = crate::background::uptime_ns();
    let (oracle_user_0, oracle_system_0) = self_getrusage()?;
    let (raw_user_0, raw_system_0) = self_raw()?;
    let deadline = wall_start + duration.as_nanos() as u64;
    match load {
        Load::User => busy_user(deadline),
        Load::System => busy_system(deadline)?,
    }
    let (raw_user_1, raw_system_1) = self_raw()?;
    let (oracle_user_1, oracle_system_1) = self_getrusage()?;
    let wall_end = crate::background::uptime_ns();
    Ok(Sample {
        load,
        target_ms: duration.as_millis() as u64,
        wall_ns: wall_end.saturating_sub(wall_start),
        oracle_user_ns: oracle_user_1.saturating_sub(oracle_user_0),
        oracle_system_ns: oracle_system_1.saturating_sub(oracle_system_0),
        raw_user: raw_user_1.saturating_sub(raw_user_0),
        raw_system: raw_system_1.saturating_sub(raw_system_0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPLE: Timebase = Timebase {
        numer: 125,
        denom: 3,
    };
    const IDENTITY: Timebase = Timebase { numer: 1, denom: 1 };

    #[test]
    fn mach_ticks_convert_through_timebase() {
        assert_eq!(APPLE.ticks_to_ns(24), Some(1000));
        assert_eq!(APPLE.ticks_to_ns(0), Some(0));
        assert_eq!(IDENTITY.ticks_to_ns(1234), Some(1234));
        assert_eq!(Timebase { numer: 1, denom: 0 }.ticks_to_ns(5), None);
        // Без переполнения: u64::MAX тиков × 125 не помещается в u64 до деления.
        assert_eq!(
            APPLE.ticks_to_ns(u64::MAX / 125 * 3),
            Some(u64::MAX / 125 * 125)
        );
        assert!(IDENTITY.is_identity() && !APPLE.is_identity());
    }

    /// Образец 1 с, где оракул — `cpu_ns`, а сырое значение — точные тики Mach Apple Silicon.
    fn sample(load: Load, user_ns: u64, system_ns: u64) -> Sample {
        Sample {
            load,
            target_ms: 1000,
            wall_ns: 1_000_000_000,
            oracle_user_ns: user_ns,
            oracle_system_ns: system_ns,
            raw_user: user_ns * 3 / 125,
            raw_system: system_ns * 3 / 125,
        }
    }

    #[test]
    fn mach_hypothesis_passes_and_nanoseconds_fail_on_apple_timebase() {
        let samples = vec![
            sample(Load::User, 990_000_000, 4_000_000),
            sample(Load::System, 300_000_000, 690_000_000),
        ];
        let user = cell_comparisons(&samples, Component::User, APPLE);
        assert_eq!(user.len(), 1);
        assert!(user[0].mach_ok && !user[0].raw_ok);
        assert!((user[0].mach_ratio.unwrap() - 1.0).abs() < 1e-6);
        assert!((user[0].raw_ratio.unwrap() - 0.024).abs() < 1e-3);
        assert_eq!(verdict(&user, APPLE), Verdict::Pass);
        let system = cell_comparisons(&samples, Component::System, APPLE);
        assert_eq!(system.len(), 1);
        assert_eq!(verdict(&system, APPLE), Verdict::Pass);
        let total = cell_comparisons(&samples, Component::Total, APPLE);
        assert_eq!(total.len(), 2);
        assert_eq!(verdict(&total, APPLE), Verdict::Pass);
    }

    #[test]
    fn tolerance_is_half_point_of_window_plus_two_percent() {
        // Окно 1 с, оракул 1 с: 5 мс + 20 мс.
        assert!((tolerance_ns(1_000_000_000, 1_000_000_000) - 25_000_000.0).abs() < 1e-3);
        let mut edge = sample(Load::User, 1_000_000_000, 0);
        edge.raw_user = 1_025_000_000 * 3 / 125;
        assert!(compare(&edge, Component::User, APPLE).mach_ok);
        edge.raw_user = 1_026_000_000 * 3 / 125;
        let outside = compare(&edge, Component::User, APPLE);
        assert!(!outside.mach_ok);
        assert_eq!(verdict(&[outside], APPLE), Verdict::Fail);
    }

    #[test]
    fn one_sample_outside_tolerance_fails_the_cell() {
        let good = compare(&sample(Load::User, 900_000_000, 0), Component::User, APPLE);
        let mut bad_sample = sample(Load::User, 900_000_000, 0);
        bad_sample.raw_user *= 2;
        let bad = compare(&bad_sample, Component::User, APPLE);
        assert_eq!(verdict(&[good, bad], APPLE), Verdict::Fail);
    }

    #[test]
    fn both_hypotheses_within_tolerance_is_undecided() {
        // Почти нулевой сигнал: обе гипотезы в абсолютной части допуска.
        let comparison = compare(&sample(Load::User, 1_000_000, 0), Component::User, APPLE);
        assert!(comparison.mach_ok && comparison.raw_ok);
        assert_eq!(verdict(&[comparison], APPLE), Verdict::Undecided);
    }

    #[test]
    fn identity_timebase_is_indistinct_not_undecided() {
        let mut s = sample(Load::User, 900_000_000, 0);
        s.raw_user = 900_000_000;
        let comparison = compare(&s, Component::User, IDENTITY);
        assert!(comparison.mach_ok && comparison.raw_ok);
        assert_eq!(verdict(&[comparison], IDENTITY), Verdict::PassIndistinct);
    }

    #[test]
    fn system_cell_skips_samples_without_signal() {
        let samples = vec![
            sample(Load::System, 950_000_000, 50_000_000),
            sample(Load::User, 0, 900_000_000),
        ];
        let system = cell_comparisons(&samples, Component::System, APPLE);
        assert!(system.is_empty());
        assert_eq!(verdict(&system, APPLE), Verdict::NoSignal);
    }

    #[test]
    fn unread_timebase_fails_mach_hypothesis() {
        let broken = Timebase { numer: 0, denom: 0 };
        let comparison = compare(&sample(Load::User, 900_000_000, 0), Component::User, broken);
        assert_eq!(comparison.mach_ns, None);
        assert!(!comparison.mach_ok);
        assert_eq!(verdict(&[comparison], broken), Verdict::Fail);
    }

    #[test]
    fn schedule_covers_each_pair_twice() {
        let plan = schedule();
        assert_eq!(plan.len(), DURATIONS.len() * 2 * REPEATS);
        for duration in DURATIONS {
            for load in [Load::User, Load::System] {
                let count = plan.iter().filter(|p| **p == (load, duration)).count();
                assert_eq!(count, REPEATS);
            }
        }
    }
}
