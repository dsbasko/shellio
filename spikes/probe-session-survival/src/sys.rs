//! Системные сведения через `sysctlbyname`, монотонные отметки `CLOCK_UPTIME_RAW`, путь
//! исполняемого файла процесса и атомарный обмен двух путей. Только системные C API (ADR-0034).

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

    #[test]
    fn phys_footprint_of_self_is_positive_and_of_missing_pid_is_none() {
        let own = phys_footprint(std::process::id() as i32);
        assert!(own.is_some_and(|bytes| bytes > 0), "{own:?}");
        assert_eq!(phys_footprint(-1), None);
    }
}
