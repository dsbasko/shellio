//! Проверка фона под замерной блокировкой (план P0.8, «Проверка фона»).
//!
//! Фон — CPU системы по `host_statistics(HOST_CPU_LOAD_INFO)` за вычетом группы стенда
//! (процесс стенда и его потомки: фикстуры, наблюдатель) и самый занятый посторонний процесс по
//! `proc_pid_rusage`. Отказавшие чтения посторонних процессов не скрываются: записываются
//! покрытие и системная дельта. Правило вердикта чистое и отделено от чтения.
//!
//! `ri_user_time` и `ri_system_time` переводятся из тиков Mach через `mach_timebase_info`;
//! калибровку этого перевода проводит сценарий `units` (задача 2).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const WINDOW: Duration = Duration::from_secs(10);
pub const QUIET_DEADLINE: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    /// Фон системы, % одного логического ядра; граница «не выше» включает равенство.
    pub system_pct: f64,
    /// Самый занятый посторонний процесс, % одного логического ядра.
    pub foreign_pct: f64,
}

pub const THRESHOLDS: Thresholds = Thresholds {
    system_pct: 30.0,
    foreign_pct: 10.0,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Identity {
    pub pid: i32,
    /// `ri_proc_start_abstime`.
    pub start_abstime: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcReading {
    pub identity: Identity,
    pub name: String,
    /// `ri_user_time + ri_system_time` в тиках Mach.
    pub cpu_mach: u64,
}

/// Состояние системы на одной границе окна.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bound {
    /// `CLOCK_UPTIME_RAW`, нс.
    pub uptime_ns: u64,
    /// Тики `HOST_CPU_LOAD_INFO`: user, system, idle, nice.
    pub host_ticks: [u32; 4],
    /// Все перечисленные PID с родителем по `proc_pidinfo(PROC_PIDTBSDINFO)`; `None` — родитель не
    /// прочитан.
    pub parents: BTreeMap<i32, Option<i32>>,
    pub procs: Vec<ProcReading>,
    /// Отказы `proc_pid_rusage`: PID и `errno`.
    pub failures: Vec<(i32, i32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Units {
    pub mach_numer: u32,
    pub mach_denom: u32,
    /// Частота тиков `host_statistics`, `sysconf(_SC_CLK_TCK)`.
    pub clk_tck: u64,
}

impl Units {
    pub fn mach_to_ns(self, ticks: u64) -> f64 {
        if self.mach_denom == 0 {
            return f64::NAN;
        }
        (u128::from(ticks) * u128::from(self.mach_numer) / u128::from(self.mach_denom)) as f64
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Foreign {
    pub pid: i32,
    pub name: String,
    pub pct: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    pub listed_start: usize,
    pub listed_end: usize,
    /// Посторонние идентичности, прочитанные на обеих границах.
    pub foreign_read_both: usize,
    /// Отказы чтения на конечной границе по `errno`.
    pub failed_by_errno: BTreeMap<i32, usize>,
    /// Идентичности только на одной из границ: вышли или появились за окно.
    pub appeared: usize,
    pub exited: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackgroundCheck {
    pub window_s: f64,
    /// Занятость всех ядер системы, % одного ядра.
    pub system_busy_pct: f64,
    pub stand_pct: f64,
    pub background_pct: f64,
    pub busiest: Option<Foreign>,
    pub stand_pids: Vec<i32>,
    pub coverage: Coverage,
    pub thresholds: Thresholds,
    pub units: Units,
    pub quiet: bool,
}

/// PID группы стенда: корень, его потомки по родителю на любой из границ и явно указанные.
pub fn stand_group(start: &Bound, end: &Bound, root: i32, extra: &[i32]) -> BTreeSet<i32> {
    let mut group: BTreeSet<i32> = extra.iter().copied().collect();
    group.insert(root);
    loop {
        let before = group.len();
        for bound in [start, end] {
            for (pid, parent) in &bound.parents {
                if parent.is_some_and(|parent| group.contains(&parent)) {
                    group.insert(*pid);
                }
            }
        }
        if group.len() == before {
            return group;
        }
    }
}

pub fn evaluate(
    start: &Bound,
    end: &Bound,
    root: i32,
    extra: &[i32],
    units: Units,
    thresholds: Thresholds,
) -> BackgroundCheck {
    let wall_ns = end.uptime_ns.saturating_sub(start.uptime_ns) as f64;
    let pct = |ns: f64| {
        if wall_ns > 0.0 {
            100.0 * ns / wall_ns
        } else {
            f64::NAN
        }
    };

    let tick =
        |state: usize| u64::from(end.host_ticks[state].wrapping_sub(start.host_ticks[state]));
    let busy_ticks = tick(0) + tick(1) + tick(3);
    let system_busy_ns = if units.clk_tck == 0 {
        f64::NAN
    } else {
        busy_ticks as f64 * 1e9 / units.clk_tck as f64
    };

    let group = stand_group(start, end, root, extra);
    let start_cpu: BTreeMap<Identity, &ProcReading> =
        start.procs.iter().map(|p| (p.identity, p)).collect();
    let end_ids: BTreeSet<Identity> = end.procs.iter().map(|p| p.identity).collect();

    let mut stand_ns = 0.0;
    let mut busiest: Option<Foreign> = None;
    let mut coverage = Coverage {
        listed_start: start.parents.len(),
        listed_end: end.parents.len(),
        ..Coverage::default()
    };
    for reading in &end.procs {
        let Some(base) = start_cpu.get(&reading.identity) else {
            coverage.appeared += 1;
            continue;
        };
        let ns = units.mach_to_ns(reading.cpu_mach.saturating_sub(base.cpu_mach));
        if group.contains(&reading.identity.pid) {
            stand_ns += ns;
            continue;
        }
        coverage.foreign_read_both += 1;
        let foreign_pct = pct(ns);
        if busiest.as_ref().is_none_or(|b| foreign_pct > b.pct) {
            busiest = Some(Foreign {
                pid: reading.identity.pid,
                name: reading.name.clone(),
                pct: foreign_pct,
            });
        }
    }
    coverage.exited = start_cpu.keys().filter(|id| !end_ids.contains(id)).count();
    for (_, errno) in &end.failures {
        *coverage.failed_by_errno.entry(*errno).or_default() += 1;
    }

    let system_busy_pct = pct(system_busy_ns);
    let stand_pct = pct(stand_ns);
    let background_pct = (system_busy_pct - stand_pct).max(0.0);
    let quiet = background_pct <= thresholds.system_pct
        && busiest
            .as_ref()
            .is_none_or(|b| b.pct <= thresholds.foreign_pct)
        && system_busy_pct.is_finite();
    BackgroundCheck {
        window_s: wall_ns / 1e9,
        system_busy_pct,
        stand_pct,
        background_pct,
        busiest,
        stand_pids: group.into_iter().collect(),
        coverage,
        thresholds,
        units,
        quiet,
    }
}

/// Итог ожидания тишины: все проверенные окна по порядку.
#[derive(Clone, Debug, PartialEq)]
pub enum QuietOutcome {
    Quiet(Vec<BackgroundCheck>),
    /// Тишины не было до срока: процесс завершается кодом «не начат».
    Noisy(Vec<BackgroundCheck>),
}

/// Окна проверки идут подряд, пока одно не окажется тихим или следующее не уложится в срок.
pub fn wait_for_quiet(
    deadline: Duration,
    mut check: impl FnMut() -> Result<BackgroundCheck, String>,
    window: Duration,
) -> Result<QuietOutcome, String> {
    let mut checks = Vec::new();
    let mut spent = Duration::ZERO;
    while spent + window <= deadline {
        let result = check()?;
        spent += Duration::from_secs_f64(result.window_s.max(window.as_secs_f64()));
        let quiet = result.quiet;
        checks.push(result);
        if quiet {
            return Ok(QuietOutcome::Quiet(checks));
        }
    }
    Ok(QuietOutcome::Noisy(checks))
}

pub fn uptime_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts — действительный выходной буфер.
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
}

#[allow(deprecated)]
pub fn current_units() -> Units {
    let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
    // SAFETY: info — действительный выходной буфер.
    unsafe { libc::mach_timebase_info(&mut info) };
    // SAFETY: sysconf без побочных эффектов.
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    Units {
        mach_numer: info.numer,
        mach_denom: info.denom,
        clk_tck: u64::try_from(clk_tck).unwrap_or(0),
    }
}

#[allow(deprecated)]
fn host_port() -> libc::mach_port_t {
    static PORT: OnceLock<libc::mach_port_t> = OnceLock::new();
    // SAFETY: mach_host_self без аргументов; порт кэшируется, чтобы не копить ссылки.
    *PORT.get_or_init(|| unsafe { libc::mach_host_self() })
}

pub fn host_ticks() -> Result<[u32; 4], String> {
    let mut info = libc::host_cpu_load_info {
        cpu_ticks: [0; libc::CPU_STATE_MAX as usize],
    };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: буфер host_cpu_load_info на HOST_CPU_LOAD_INFO_COUNT слов.
    #[allow(deprecated)]
    let kr = unsafe {
        libc::host_statistics(
            host_port(),
            libc::HOST_CPU_LOAD_INFO,
            (&mut info as *mut libc::host_cpu_load_info).cast(),
            &mut count,
        )
    };
    if kr != libc::KERN_SUCCESS {
        return Err(format!("host_statistics(HOST_CPU_LOAD_INFO): код {kr}"));
    }
    Ok(info.cpu_ticks)
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

pub fn list_pids() -> Result<Vec<i32>, String> {
    // SAFETY: нулевой буфер запрашивает только число PID.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Err(format!("proc_listallpids: errno {}", errno()));
    }
    let mut pids = vec![0i32; count as usize + 64];
    let bytes = (pids.len() * size_of::<i32>()) as libc::c_int;
    // SAFETY: буфер на pids.len() значений pid_t.
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if got <= 0 {
        return Err(format!("proc_listallpids: errno {}", errno()));
    }
    pids.truncate(got as usize);
    pids.retain(|pid| *pid > 0);
    Ok(pids)
}

fn c_name(raw: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn bsd_info(pid: i32) -> Option<(i32, String)> {
    // SAFETY: proc_bsdinfo — POD, нули допустимы.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: буфер proc_bsdinfo размера size.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if got != size {
        return None;
    }
    let name = c_name(&info.pbi_name);
    let name = if name.is_empty() {
        c_name(&info.pbi_comm)
    } else {
        name
    };
    Some((info.pbi_ppid as i32, name))
}

fn rusage(pid: i32) -> Result<libc::rusage_info_v4, i32> {
    // SAFETY: rusage_info_v4 — POD, нули допустимы.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // SAFETY: буфер rusage_info_v4 для RUSAGE_INFO_V4.
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            (&mut info as *mut libc::rusage_info_v4).cast(),
        )
    };
    if rc == 0 { Ok(info) } else { Err(errno()) }
}

pub fn sample_bound() -> Result<Bound, String> {
    let host_ticks = host_ticks()?;
    let uptime_ns = uptime_ns();
    let mut bound = Bound {
        uptime_ns,
        host_ticks,
        ..Bound::default()
    };
    for pid in list_pids()? {
        let info = bsd_info(pid);
        bound
            .parents
            .insert(pid, info.as_ref().map(|(ppid, _)| *ppid));
        match rusage(pid) {
            Ok(usage) => bound.procs.push(ProcReading {
                identity: Identity {
                    pid,
                    start_abstime: usage.ri_proc_start_abstime,
                },
                name: info.map(|(_, name)| name).unwrap_or_default(),
                cpu_mach: usage.ri_user_time.saturating_add(usage.ri_system_time),
            }),
            Err(errno) => bound.failures.push((pid, errno)),
        }
    }
    Ok(bound)
}

/// Держит окно замера до отметки по монотонным часам; это интервал замера, а не синхронизация.
pub fn hold_until(deadline_ns: u64) {
    loop {
        let now = uptime_ns();
        if now >= deadline_ns {
            return;
        }
        std::thread::sleep(Duration::from_nanos(deadline_ns - now));
    }
}

/// Одно окно проверки фона длиной `window` с вычетом группы стенда.
pub fn measure(window: Duration, extra_stand: &[i32]) -> Result<BackgroundCheck, String> {
    let units = current_units();
    let start = sample_bound()?;
    hold_until(start.uptime_ns + window.as_nanos() as u64);
    let end = sample_bound()?;
    Ok(evaluate(
        &start,
        &end,
        std::process::id() as i32,
        extra_stand,
        units,
        THRESHOLDS,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNITS: Units = Units {
        mach_numer: 125,
        mach_denom: 3,
        clk_tck: 100,
    };

    fn ns_to_mach(ns: u64) -> u64 {
        ns * 3 / 125
    }

    fn proc(pid: i32, start: u64, cpu_ns: u64, name: &str) -> ProcReading {
        ProcReading {
            identity: Identity {
                pid,
                start_abstime: start,
            },
            name: name.to_owned(),
            cpu_mach: ns_to_mach(cpu_ns),
        }
    }

    /// Окно 10 с: система занята на `busy_ticks` тиков 100 Гц, стенд — PID 100 и его ребёнок 101.
    fn bounds(busy_ticks: u32, foreign_cpu_ns: u64) -> (Bound, Bound) {
        let parents: BTreeMap<i32, Option<i32>> = [
            (1, Some(0)),
            (100, Some(1)),
            (101, Some(100)),
            (200, Some(1)),
            (300, None),
        ]
        .into_iter()
        .collect();
        let start = Bound {
            uptime_ns: 0,
            host_ticks: [u32::MAX - 10, 0, 0, 0],
            parents: parents.clone(),
            procs: vec![
                proc(100, 1, 0, "stand"),
                proc(101, 2, 0, "fixture"),
                proc(200, 3, 0, "zig"),
            ],
            failures: vec![(300, libc::EPERM)],
        };
        let end = Bound {
            uptime_ns: 10_000_000_000,
            host_ticks: [busy_ticks.wrapping_sub(11), 0, 0, 0],
            parents,
            procs: vec![
                proc(100, 1, 200_000_000, "stand"),
                proc(101, 2, 800_000_000, "fixture"),
                proc(200, 3, foreign_cpu_ns, "zig"),
            ],
            failures: vec![(300, libc::EPERM)],
        };
        (start, end)
    }

    #[test]
    fn mach_ticks_convert_through_timebase() {
        assert_eq!(UNITS.mach_to_ns(24), 1000.0);
        let identity = Units {
            mach_numer: 1,
            mach_denom: 1,
            clk_tck: 100,
        };
        assert_eq!(identity.mach_to_ns(1000), 1000.0);
    }

    #[test]
    fn stand_group_follows_descendants() {
        let (start, end) = bounds(0, 0);
        let group = stand_group(&start, &end, 100, &[]);
        assert_eq!(group.into_iter().collect::<Vec<_>>(), vec![100, 101]);
        let with_extra = stand_group(&start, &end, 100, &[300]);
        assert!(with_extra.contains(&300));
    }

    #[test]
    fn stand_is_subtracted_and_wrapped_ticks_count() {
        // 1 с CPU стенда (10%) и 0,5 с постороннего (5%), система занята 3 с (30%).
        let (start, end) = bounds(300, 500_000_000);
        let check = evaluate(&start, &end, 100, &[], UNITS, THRESHOLDS);
        assert!((check.system_busy_pct - 30.0).abs() < 1e-9, "{check:?}");
        assert!((check.stand_pct - 10.0).abs() < 1e-6);
        assert!((check.background_pct - 20.0).abs() < 1e-6);
        let busiest = check.busiest.clone().unwrap();
        assert_eq!((busiest.pid, busiest.name.as_str()), (200, "zig"));
        assert!((busiest.pct - 5.0).abs() < 1e-6);
        assert!(check.quiet);
        assert_eq!(check.coverage.failed_by_errno.get(&libc::EPERM), Some(&1));
        assert_eq!(check.coverage.foreign_read_both, 1);
    }

    #[test]
    fn thresholds_are_inclusive() {
        // Фон ровно 30% (система 40%, стенд 10%), посторонний ровно 10%.
        let (start, end) = bounds(400, 1_000_000_000);
        let check = evaluate(&start, &end, 100, &[], UNITS, THRESHOLDS);
        assert!((check.background_pct - 30.0).abs() < 1e-6);
        assert!(check.quiet, "{check:?}");
    }

    #[test]
    fn busy_system_or_busy_foreign_is_noisy() {
        let (start, end) = bounds(500, 0);
        assert!(!evaluate(&start, &end, 100, &[], UNITS, THRESHOLDS).quiet);
        let (start, end) = bounds(250, 1_200_000_000);
        let check = evaluate(&start, &end, 100, &[], UNITS, THRESHOLDS);
        assert!(check.background_pct <= 30.0);
        assert!(!check.quiet);
    }

    #[test]
    fn foreign_without_base_is_not_busiest() {
        let (start, mut end) = bounds(100, 0);
        end.procs.push(proc(400, 9, 5_000_000_000, "fresh"));
        let check = evaluate(&start, &end, 100, &[], UNITS, THRESHOLDS);
        assert_eq!(check.coverage.appeared, 1);
        assert_ne!(check.busiest.map(|b| b.pid), Some(400));
    }

    #[test]
    fn reused_pid_is_a_different_identity() {
        let (start, mut end) = bounds(100, 0);
        end.procs.retain(|p| p.identity.pid != 200);
        end.procs.push(proc(200, 77, 9_000_000_000, "reused"));
        let check = evaluate(&start, &end, 100, &[], UNITS, THRESHOLDS);
        assert_eq!((check.coverage.appeared, check.coverage.exited), (1, 1));
        assert!(check.busiest.is_none());
    }

    fn fake(quiet: bool) -> BackgroundCheck {
        let (start, end) = bounds(if quiet { 100 } else { 900 }, 0);
        evaluate(&start, &end, 100, &[], UNITS, THRESHOLDS)
    }

    #[test]
    fn quiet_wait_stops_on_first_quiet_window() {
        let mut answers = vec![false, false, true, true].into_iter();
        let outcome =
            wait_for_quiet(QUIET_DEADLINE, || Ok(fake(answers.next().unwrap())), WINDOW).unwrap();
        match outcome {
            QuietOutcome::Quiet(checks) => assert_eq!(checks.len(), 3),
            QuietOutcome::Noisy(_) => panic!("ожидалась тишина"),
        }
    }

    #[test]
    fn quiet_wait_gives_up_at_deadline() {
        let mut calls = 0;
        let outcome = wait_for_quiet(
            QUIET_DEADLINE,
            || {
                calls += 1;
                Ok(fake(false))
            },
            WINDOW,
        )
        .unwrap();
        assert!(matches!(outcome, QuietOutcome::Noisy(ref checks) if checks.len() == 30));
        assert_eq!(calls, 30);
    }
}
