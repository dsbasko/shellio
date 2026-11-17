//! Системные сведения через `sysctlbyname`, монотонные отметки `CLOCK_UPTIME_RAW`, путь
//! исполняемого файла процесса, атомарный обмен двух путей и показания CPU и памяти процессов и
//! потоков для замеров. Только системные C API (ADR-0034).

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Монотонная отметка в наносекундах; шкала общая у процессов стенда.
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

fn sysctl_raw(name: &str) -> Option<Vec<u8>> {
    let name = CString::new(name).ok()?;
    let mut len = 0usize;
    // SAFETY: первый вызов узнаёт размер значения, буфер не передаётся.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || len == 0 {
        return None;
    }
    let mut buffer = vec![0u8; len];
    // SAFETY: буфер длины `len` жив на время вызова.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    buffer.truncate(len);
    Some(buffer)
}

/// Строковое значение без завершающего нуля и пробелов; пустое — `None`.
pub fn sysctl_string(name: &str) -> Option<String> {
    let bytes = sysctl_raw(name)?;
    let text = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
    Some(String::from_utf8_lossy(text).trim().to_owned()).filter(|text| !text.is_empty())
}

pub fn sysctl_u64(name: &str) -> Option<u64> {
    let bytes: [u8; 8] = sysctl_raw(name)?.try_into().ok()?;
    Some(u64::from_ne_bytes(bytes))
}

/// `ri_phys_footprint` процесса: физическая память по счёту ядра (`proc_pid_rusage`). Процесс
/// своего пользователя; `None` — процесса нет или доступа нет.
pub fn phys_footprint(pid: i32) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    // SAFETY: буфер — структура версии, указанной во втором аргументе, и живёт на время вызова.
    let status = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V2,
            info.as_mut_ptr().cast::<libc::rusage_info_t>(),
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: ядро заполнило структуру при нулевом коде возврата; до этого она обнулена.
    Some(unsafe { info.assume_init() }.ri_phys_footprint)
}

/// Показание процесса по `proc_pid_rusage`. CPU — в тиках `mach_absolute_time`: на Apple Silicon
/// тик не равен наносекунде, перевод — [`timebase`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcUsage {
    /// `ri_user_time + ri_system_time`, тики.
    pub cpu_ticks: u64,
    /// `ri_phys_footprint`, байты.
    pub footprint: u64,
    /// `ri_pkg_idle_wkups + ri_interrupt_wkups`.
    pub wakeups: u64,
}

/// Показание процесса своего пользователя; `None` — процесса нет или доступа нет.
pub fn usage(pid: i32) -> Option<ProcUsage> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    // SAFETY: буфер — структура версии, указанной во втором аргументе, и живёт на время вызова.
    let status = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V2,
            info.as_mut_ptr().cast::<libc::rusage_info_t>(),
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: ядро заполнило структуру при нулевом коде возврата; до этого она обнулена.
    let info = unsafe { info.assume_init() };
    Some(ProcUsage {
        cpu_ticks: info.ri_user_time.saturating_add(info.ri_system_time),
        footprint: info.ri_phys_footprint,
        wakeups: info
            .ri_pkg_idle_wkups
            .saturating_add(info.ri_interrupt_wkups),
    })
}

/// Числитель и знаменатель `mach_timebase_info`: наносекунды = тики × числитель / знаменатель.
/// libc помечает привязки Mach устаревшими в пользу отдельного crate, сама функция не устарела.
#[allow(deprecated)]
pub fn timebase() -> Option<(u32, u32)> {
    let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
    // SAFETY: `mach_timebase_info` пишет числитель и знаменатель по указателю.
    let status = unsafe { libc::mach_timebase_info(&mut info) };
    (status == 0 && info.numer != 0 && info.denom != 0).then_some((info.numer, info.denom))
}

/// CPU своего процесса по `getrusage(RUSAGE_SELF)`, нс: сверка перевода тиков `proc_pid_rusage`.
pub fn rusage_self_ns() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `getrusage` пишет `rusage` по указателю, буфер живёт на время вызова.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: при нулевом коде структура заполнена.
    let usage = unsafe { usage.assume_init() };
    let ns = |time: libc::timeval| {
        u64::try_from(time.tv_sec)
            .unwrap_or_default()
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::try_from(time.tv_usec).unwrap_or_default() * 1000)
    };
    Some(ns(usage.ru_utime).saturating_add(ns(usage.ru_stime)))
}

/// `PROC_PIDLISTTHREADS` из `sys/proc_info.h`: в libc его нет.
const PROC_PIDLISTTHREADS: libc::c_int = 6;
const MAX_THREADS: usize = 4096;

/// CPU потоков своего процесса с именем `name` (`pthread_setname_np`), нс, и их число. Время потока
/// ядро отдаёт в наносекундах с точностью до микросекунды (`proc_threadinfo`). `None` — список
/// потоков не читается.
pub fn threads_cpu_ns(name: &str) -> Option<(u64, usize)> {
    let pid = std::process::id() as libc::c_int;
    let mut handles = vec![0u64; 256];
    let filled = loop {
        let size = (handles.len() * std::mem::size_of::<u64>()) as libc::c_int;
        // SAFETY: буфер из `handles.len()` значений u64 живёт на время вызова, его размер передан.
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                PROC_PIDLISTTHREADS,
                0,
                handles.as_mut_ptr().cast::<libc::c_void>(),
                size,
            )
        };
        let filled = usize::try_from(filled).ok().filter(|filled| *filled > 0)?;
        if filled < size as usize || handles.len() >= MAX_THREADS {
            break filled / std::mem::size_of::<u64>();
        }
        handles.resize(handles.len() * 2, 0);
    };
    let mut total = 0u64;
    let mut count = 0usize;
    for handle in &handles[..filled] {
        let mut info = std::mem::MaybeUninit::<libc::proc_threadinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_threadinfo>() as libc::c_int;
        // SAFETY: буфер — `proc_threadinfo` своего размера и живёт на время вызова.
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTHREADINFO,
                *handle,
                info.as_mut_ptr().cast::<libc::c_void>(),
                size,
            )
        };
        if got != size {
            // Поток вышел между списком и показанием.
            continue;
        }
        // SAFETY: ядро заполнило структуру целиком; до этого она обнулена.
        let info = unsafe { info.assume_init() };
        // SAFETY: `pth_name` — массив, обнулённый до вызова; ядро пишет имя с завершающим нулём.
        let thread = unsafe { std::ffi::CStr::from_ptr(info.pth_name.as_ptr()) };
        if thread.to_bytes() == name.as_bytes() {
            total = total
                .saturating_add(info.pth_user_time)
                .saturating_add(info.pth_system_time);
            count += 1;
        }
    }
    Some((total, count))
}

/// Путь исполняемого файла процесса по ядру (`proc_pidpath`); `None` — процесса нет.
pub fn executable_path(pid: i32) -> Option<PathBuf> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: буфер живёт на время вызова, его размер передан ядру.
    let len = unsafe {
        libc::proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            buffer.len() as u32,
        )
    };
    let len = usize::try_from(len).ok().filter(|len| *len > 0)?;
    buffer.truncate(len);
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer)))
}

/// Атомарный обмен двух существующих путей одного тома (`renamex_np` с `RENAME_SWAP`): ни в
/// какой момент ни одного из путей не нет.
pub fn swap_paths(first: &Path, second: &Path) -> io::Result<()> {
    let c_path = |path: &Path| {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "нулевой байт в пути"))
    };
    let (first, second) = (c_path(first)?, c_path(second)?);
    // SAFETY: обе строки живут на время вызова и оканчиваются нулём.
    let status = unsafe { libc::renamex_np(first.as_ptr(), second.as_ptr(), libc::RENAME_SWAP) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_path_of_self_is_the_test_binary() {
        let own = executable_path(std::process::id() as i32).map(|path| path.canonicalize());
        let expected = std::env::current_exe().and_then(|path| path.canonicalize());
        assert_eq!(own.and_then(Result::ok), expected.ok());
        assert_eq!(executable_path(-1), None);
    }

    #[test]
    fn swap_exchanges_two_directories() {
        let dir = std::env::temp_dir().join(format!("pss-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).expect("каталог a");
        std::fs::create_dir_all(&b).expect("каталог b");
        std::fs::write(a.join("f"), "first").expect("файл a");
        std::fs::write(b.join("f"), "second").expect("файл b");
        swap_paths(&a, &b).expect("обмен");
        assert_eq!(
            std::fs::read_to_string(a.join("f")).ok().as_deref(),
            Some("second")
        );
        assert_eq!(
            std::fs::read_to_string(b.join("f")).ok().as_deref(),
            Some("first")
        );
        assert!(swap_paths(&a, &dir.join("missing")).is_err());
        std::fs::remove_dir_all(&dir).expect("уборка");
    }

    #[test]
    fn system_values_are_read() {
        assert!(sysctl_string("kern.osproductversion").is_some());
        assert!(sysctl_u64("hw.memsize").is_some_and(|bytes| bytes > 0));
        assert_eq!(sysctl_string("probe.session.no-such-name"), None);
        assert_eq!(sysctl_string("bad\0name"), None);
    }

    #[test]
    fn uptime_is_monotonic() {
        let first = uptime_ns();
        let second = uptime_ns();
        assert!(first > 0);
        assert!(second >= first);
    }

    /// Работа потока до тех пор, пока не скажут остановиться: CPU для показаний без `sleep`.
    fn burn(stop: &std::sync::atomic::AtomicBool) -> u64 {
        let mut value = 1u64;
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            value = std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
        value
    }

    #[test]
    fn named_threads_cpu_is_read() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("pss-burn-test".to_owned())
                .spawn(move || burn(&stop))
                .expect("поток")
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = (0, 0);
        while std::time::Instant::now() < deadline {
            seen = threads_cpu_ns("pss-burn-test").expect("список потоков");
            if seen.0 >= 20_000_000 {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        worker.join().expect("поток вышел");
        assert_eq!(seen.1, 1, "{seen:?}");
        assert!(seen.0 >= 20_000_000, "{seen:?}");
        assert_eq!(threads_cpu_ns("pss-no-such-thread"), Some((0, 0)));
    }

    #[test]
    fn usage_ticks_convert_to_rusage_time() {
        use std::sync::atomic::AtomicBool;
        let (numer, denom) = timebase().expect("mach_timebase_info");
        let pid = std::process::id() as i32;
        let before = (
            usage(pid).expect("показание"),
            rusage_self_ns().expect("getrusage"),
        );
        let stop = AtomicBool::new(false);
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| burn(&stop));
            while start.elapsed() < std::time::Duration::from_millis(200) {}
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let after = (
            usage(pid).expect("показание"),
            rusage_self_ns().expect("getrusage"),
        );
        let ticks = after.0.cpu_ticks - before.0.cpu_ticks;
        let proc_ns = u128::from(ticks) * u128::from(numer) / u128::from(denom);
        let rusage_ns = after.1 - before.1;
        let ratio = proc_ns as f64 / rusage_ns as f64;
        assert!((0.9..=1.1).contains(&ratio), "{proc_ns} против {rusage_ns}");
        assert!(after.0.footprint > 0);
        assert_eq!(usage(-1), None);
    }

    #[test]
    fn phys_footprint_of_self_is_positive_and_of_missing_pid_is_none() {
        let own = phys_footprint(std::process::id() as i32);
        assert!(own.is_some_and(|bytes| bytes > 0), "{own:?}");
        assert_eq!(phys_footprint(-1), None);
    }
}
