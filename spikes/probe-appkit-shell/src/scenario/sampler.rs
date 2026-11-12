//! Сэмплер процесса — единственный источник замеров `startup`, `idle` и `frames`: монотонное
//! время `CLOCK_UPTIME_RAW` (те же часы, что у `CACurrentMediaTime` и меток `displayLink`), время
//! CPU процесса `CLOCK_PROCESS_CPUTIME_ID`, `ri_phys_footprint` из `proc_pid_rusage`, стенные
//! часы `gettimeofday` и время запуска процесса из `sysctl` `kern.proc.pid`.
//!
//! `ri_user_time` и `ri_system_time` на Apple Silicon идут в тиках `mach_absolute_time`, поэтому
//! время CPU берётся из `clock_gettime`. `ri_proc_start_abstime` тоже в тиках и переводится в
//! наносекунды через `mach_timebase_info`.

use std::mem::MaybeUninit;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// Отметки входа в `main`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub wall_us: i64,
    pub mono_ns: u64,
}

static MAIN_ENTRY: OnceLock<Entry> = OnceLock::new();

/// Отмечает вход в `main`; вызывается первой строкой `main`.
pub fn mark_main_entry() {
    if let (Ok(wall_us), Ok(mono_ns)) = (wall_us(), uptime_ns()) {
        let _ = MAIN_ENTRY.set(Entry { wall_us, mono_ns });
    }
}

pub fn main_entry() -> Option<Entry> {
    MAIN_ENTRY.get().copied()
}

fn clock_ns(clock: libc::clockid_t, name: &str) -> Result<u64, String> {
    let mut time = MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: `clock_gettime` пишет `timespec` по переданному указателю.
    if unsafe { libc::clock_gettime(clock, time.as_mut_ptr()) } != 0 {
        return Err(format!(
            "clock_gettime({name}): {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: при нулевом коде структура заполнена.
    let time = unsafe { time.assume_init() };
    Ok(time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64)
}

/// Монотонное время от старта системы без сна, нс.
pub fn uptime_ns() -> Result<u64, String> {
    clock_ns(libc::CLOCK_UPTIME_RAW, "CLOCK_UPTIME_RAW")
}

/// Время CPU всех потоков процесса, нс.
pub fn cpu_ns() -> Result<u64, String> {
    clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID, "CLOCK_PROCESS_CPUTIME_ID")
}

/// Стенные часы, мкс от эпохи Unix.
pub fn wall_us() -> Result<i64, String> {
    let mut time = MaybeUninit::<libc::timeval>::uninit();
    // SAFETY: `gettimeofday` пишет `timeval`, часовой пояс не запрашивается.
    if unsafe { libc::gettimeofday(time.as_mut_ptr(), std::ptr::null_mut()) } != 0 {
        return Err(format!("gettimeofday: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: при нулевом коде структура заполнена.
    let time = unsafe { time.assume_init() };
    Ok(time.tv_sec * 1_000_000 + i64::from(time.tv_usec))
}

fn rusage() -> Result<libc::rusage_info_v0, String> {
    let mut info = MaybeUninit::<libc::rusage_info_v0>::uninit();
    // SAFETY: для `RUSAGE_INFO_V0` ядро пишет `rusage_info_v0` по переданному указателю.
    let code = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V0,
            info.as_mut_ptr().cast(),
        )
    };
    if code != 0 {
        return Err(format!(
            "proc_pid_rusage: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: при нулевом коде структура заполнена.
    Ok(unsafe { info.assume_init() })
}

/// Тики `mach_absolute_time` в наносекундах. libc помечает привязки Mach устаревшими в пользу
/// отдельного crate, сама системная функция не устарела.
#[allow(deprecated)]
fn mach_to_ns(ticks: u64) -> u64 {
    static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
    let (numer, denom) = *TIMEBASE.get_or_init(|| {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: `mach_timebase_info` пишет числитель и знаменатель по указателю.
        let code = unsafe { libc::mach_timebase_info(&mut info) };
        if code == 0 && info.denom != 0 {
            (info.numer, info.denom)
        } else {
            (1, 1)
        }
    });
    (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64
}

/// Замер процесса в один момент.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    pub mono_ns: u64,
    pub cpu_ns: u64,
    pub footprint_bytes: u64,
    pub idle_wakeups: u64,
    pub interrupt_wakeups: u64,
}

pub fn sample() -> Result<Sample, String> {
    let mono_ns = uptime_ns()?;
    let cpu_ns = cpu_ns()?;
    let info = rusage()?;
    Ok(Sample {
        mono_ns,
        cpu_ns,
        footprint_bytes: info.ri_phys_footprint,
        idle_wakeups: info.ri_pkg_idle_wkups,
        interrupt_wakeups: info.ri_interrupt_wkups,
    })
}

/// Физический след памяти процесса, байты.
pub fn footprint() -> Result<u64, String> {
    Ok(rusage()?.ri_phys_footprint)
}

/// Время запуска процесса: стенное из `sysctl` (`p_starttime`), стенное из `proc_pidinfo`
/// для сверки и монотонное из `ri_proc_start_abstime`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessStart {
    pub wall_us: i64,
    pub bsd_wall_us: Option<i64>,
    pub mono_ns: u64,
}

pub fn process_start() -> Result<ProcessStart, String> {
    let pid = std::process::id() as libc::c_int;
    Ok(ProcessStart {
        wall_us: sysctl_start_us(pid)?,
        bsd_wall_us: bsd_start_us(pid),
        mono_ns: mach_to_ns(rusage()?.ri_proc_start_abstime),
    })
}

/// `kinfo_proc.kp_proc.p_un.__p_starttime` — первое поле структуры, `timeval` (16 байт). Своего
/// описания `kinfo_proc` для macOS в libc нет, поэтому читается только это поле; значение
/// сверяется с `proc_pidinfo` в тесте и в результате сценария.
fn sysctl_start_us(pid: libc::c_int) -> Result<i64, String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: запрос размера: буфер не передаётся, ядро пишет только `size`.
    let code = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if code != 0 {
        return Err(format!(
            "sysctl kern.proc.pid: {}",
            std::io::Error::last_os_error()
        ));
    }
    let timeval = std::mem::size_of::<libc::timeval>();
    if size < timeval {
        return Err(format!("sysctl kern.proc.pid: размер ответа {size} байт"));
    }
    let mut buffer = vec![0u64; size.div_ceil(8)];
    let mut written = size;
    // SAFETY: буфер выровнен по 8 байтам и не меньше `size`, ядро пишет не больше `written`.
    let code = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buffer.as_mut_ptr().cast(),
            &mut written,
            std::ptr::null_mut(),
            0,
        )
    };
    if code != 0 {
        return Err(format!(
            "sysctl kern.proc.pid: {}",
            std::io::Error::last_os_error()
        ));
    }
    // На пустой запрос ядро сообщает размер с запасом; запись одного процесса — `kinfo_proc`.
    if written < timeval || written > size {
        return Err(format!(
            "sysctl kern.proc.pid: записано {written} байт при размере {size}"
        ));
    }
    // SAFETY: первые 16 байт буфера — `timeval` начала процесса, буфер выровнен.
    let start = unsafe { std::ptr::read(buffer.as_ptr().cast::<libc::timeval>()) };
    Ok(start.tv_sec * 1_000_000 + i64::from(start.tv_usec))
}

fn bsd_start_us(pid: libc::c_int) -> Option<i64> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: для `PROC_PIDTBSDINFO` ядро пишет `proc_bsdinfo` размера `size`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: структура записана целиком.
    let info = unsafe { info.assume_init() };
    Some(info.pbi_start_tvsec as i64 * 1_000_000 + info.pbi_start_tvusec as i64)
}

/// Загрузка CPU в процентах одного ядра: `100 × Δcpu / Δмонотонного времени`, без деления на
/// число ядер. `None`, если окно пустое.
pub fn cpu_percent(cpu_ns: u64, mono_ns: u64) -> Option<f64> {
    (mono_ns > 0).then(|| 100.0 * cpu_ns as f64 / mono_ns as f64)
}

/// Мебибайты.
pub fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_formula() {
        assert_eq!(cpu_percent(50_000_000, 10_000_000_000), Some(0.5));
        assert_eq!(cpu_percent(10_000_000_000, 10_000_000_000), Some(100.0));
        assert_eq!(cpu_percent(25_000_000_000, 10_000_000_000), Some(250.0));
        assert_eq!(cpu_percent(0, 10_000_000_000), Some(0.0));
        assert_eq!(cpu_percent(1, 0), None);
    }

    #[test]
    fn mebibytes() {
        assert_eq!(mib(150 << 20), 150.0);
        assert!((mib(150_000_000) - 143.05).abs() < 0.01);
    }

    #[test]
    fn clocks_advance() {
        let a = sample().unwrap();
        let b = sample().unwrap();
        assert!(b.mono_ns >= a.mono_ns);
        assert!(b.cpu_ns >= a.cpu_ns);
        assert!(a.footprint_bytes > 0);
    }

    /// Начало процесса из `sysctl` совпадает с `proc_pidinfo`, предшествует текущему времени
    /// и монотонно лежит раньше текущего момента.
    #[test]
    fn process_start_is_consistent() {
        let start = process_start().unwrap();
        let now = wall_us().unwrap();
        assert!(start.wall_us <= now, "{start:?} {now}");
        assert!(now - start.wall_us < 3_600_000_000, "{start:?} {now}");
        assert_eq!(start.bsd_wall_us, Some(start.wall_us));
        assert!(start.mono_ns <= uptime_ns().unwrap());
        assert!(start.mono_ns > 0);
    }
}
