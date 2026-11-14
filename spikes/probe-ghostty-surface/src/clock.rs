//! Монотонные отметки `CLOCK_UPTIME_RAW` в наносекундах. Шкала общая у процессов пробы, запуска и
//! фикстуры, поэтому отметки разных процессов вычитаются друг из друга.
//!
//! Здесь же стенные часы, вход в `main` и время запуска процесса для сценариев старта:
//! `p_starttime` из `sysctl` `kern.proc.pid`, `proc_pidinfo` для сверки и `ri_proc_start_abstime`
//! в тиках `mach_absolute_time`, переведённых в наносекунды через `mach_timebase_info`.

use std::mem::MaybeUninit;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

pub fn uptime_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: структура живёт на стеке на время вызова, часы `CLOCK_UPTIME_RAW` есть всегда.
    let status = unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut now) };
    if status != 0 {
        return 0;
    }
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// Интервал `from` → `to` в миллисекундах; отрицательный, если `to` раньше.
pub fn interval_ms(from: u64, to: u64) -> f64 {
    (i128::from(to) - i128::from(from)) as f64 / 1e6
}

/// Отметки входа в `main`: стенные мкс от эпохи Unix и монотонные нс.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub wall_us: i64,
    pub mono_ns: u64,
}

static MAIN_ENTRY: OnceLock<Entry> = OnceLock::new();

/// Отмечает вход в `main`; вызывается первой строкой `main`.
pub fn mark_main_entry() {
    if let Ok(wall_us) = wall_us() {
        let _ = MAIN_ENTRY.set(Entry {
            wall_us,
            mono_ns: uptime_ns(),
        });
    }
}

pub fn main_entry() -> Option<Entry> {
    MAIN_ENTRY.get().copied()
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
        mono_ns: mach_to_ns(start_abstime(pid)?),
    })
}

fn start_abstime(pid: libc::c_int) -> Result<u64, String> {
    let mut info = MaybeUninit::<libc::rusage_info_v0>::uninit();
    // SAFETY: для `RUSAGE_INFO_V0` ядро пишет `rusage_info_v0` по переданному указателю.
    let code =
        unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, info.as_mut_ptr().cast()) };
    if code != 0 {
        return Err(format!(
            "proc_pid_rusage: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: при нулевом коде структура заполнена.
    Ok(unsafe { info.assume_init() }.ri_proc_start_abstime)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_start_is_consistent() {
        let start = process_start().unwrap();
        let now = wall_us().unwrap();
        assert!(start.wall_us <= now, "{start:?} {now}");
        assert!(now - start.wall_us < 3_600_000_000, "{start:?} {now}");
        assert_eq!(start.bsd_wall_us, Some(start.wall_us));
        assert!(start.mono_ns > 0);
        assert!(start.mono_ns <= uptime_ns());
    }

    #[test]
    fn main_entry_is_set_once() {
        mark_main_entry();
        let first = main_entry().unwrap();
        mark_main_entry();
        assert_eq!(main_entry(), Some(first));
        assert!(first.mono_ns <= uptime_ns());
    }

    #[test]
    fn uptime_is_monotonic() {
        let first = uptime_ns();
        let second = uptime_ns();
        assert!(first > 0);
        assert!(second >= first);
    }

    #[test]
    fn interval_in_ms_keeps_sign() {
        assert_eq!(interval_ms(1_000_000, 3_500_000), 2.5);
        assert_eq!(interval_ms(3_500_000, 1_000_000), -2.5);
    }
}
