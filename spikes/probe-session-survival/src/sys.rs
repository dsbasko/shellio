//! Системные сведения через `sysctlbyname` и монотонные отметки `CLOCK_UPTIME_RAW`. Только
//! системные C API (ADR-0034).

use std::ffi::CString;

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

#[cfg(test)]
mod tests {
    use super::*;

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
