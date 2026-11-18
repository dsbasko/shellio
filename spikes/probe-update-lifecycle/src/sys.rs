//! Системные сведения через `sysctlbyname` и монотонные отметки `CLOCK_UPTIME_RAW`. Только
//! системные C API (ADR-0034). Источник — `spikes/probe-session-survival/src/sys.rs`, без
//! показаний CPU и памяти. Обмен путей, `fsync` каталога и случайный номер попытки добавлены в
//! задаче 5 для журнала обновления.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

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

/// Атомарный обмен двух существующих путей одного тома (`renamex_np` с `RENAME_SWAP`): ни в
/// какой момент ни одного из путей не нет. Источник — `spikes/probe-session-survival/src/sys.rs`.
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

/// `fsync` каталога: запись о переименовании внутри него доходит до диска.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Случайный номер попытки: 16 байт из `arc4random_buf` в шестнадцатеричном виде.
pub fn random_id() -> String {
    let mut bytes = [0u8; 16];
    // SAFETY: буфер живёт на время вызова, длина совпадает с его размером.
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast::<libc::c_void>(), bytes.len()) };
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_facts_are_read() {
        assert!(sysctl_string("kern.osproductversion").is_some());
        assert!(sysctl_u64("hw.memsize").is_some_and(|bytes| bytes > 0));
        assert_eq!(sysctl_string("kern.no-such-name"), None);
        let first = uptime_ns();
        assert!(first > 0 && uptime_ns() >= first);
    }

    #[test]
    fn swap_sync_and_random_id() {
        let dir =
            std::env::temp_dir().join(format!("psu-sys-{}-{}", std::process::id(), uptime_ns()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("mark"), "a").unwrap();
        swap_paths(&a, &b).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("mark")).unwrap(), "a");
        assert!(!a.join("mark").exists());
        assert!(swap_paths(&a, &dir.join("missing")).is_err());
        sync_dir(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let id = random_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(id, random_id());
    }
}
