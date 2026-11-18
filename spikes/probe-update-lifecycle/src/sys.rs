//! Системные сведения через `sysctlbyname` и монотонные отметки `CLOCK_UPTIME_RAW`. Только
//! системные C API (ADR-0034). Источник — `spikes/probe-session-survival/src/sys.rs`, без
//! показаний CPU и памяти. Обмен путей, `fsync` каталога и случайный номер попытки добавлены в
//! задаче 5 для журнала обновления; CDHash процесса из ядра, путь исполняемого файла по PID,
//! дескрипторы процесса и переименование без перезаписи — в задаче 6 для владения.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    let (first, second) = (c_path(first)?, c_path(second)?);
    // SAFETY: обе строки живут на время вызова и оканчиваются нулём.
    let status = unsafe { libc::renamex_np(first.as_ptr(), second.as_ptr(), libc::RENAME_SWAP) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Итог `setsockopt` срока сокета Unix (задача 17). Сокету, который собеседник уже закрыл, macOS
/// отвечает `EINVAL`, хотя непрочитанные кадры ещё лежат в буфере. Чтение такого сокета не
/// блокируется — отдаёт буфер, затем конец потока, — а запись получает `EPIPE`, поэтому `EINVAL`
/// здесь не ошибка: итог решает следующее чтение или запись.
pub fn socket_timeout(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(err) if err.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        other => other,
    }
}

/// `fsync` каталога: запись о переименовании внутри него доходит до диска.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Место тома пути по `statfs` (задача 13): доступно непривилегированному процессу и всего, байт.
pub fn volume_space(path: &Path) -> io::Result<(u64, u64)> {
    let path = c_path(path)?;
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: строка оканчивается нулём, буфер — `struct statfs` подходящего размера.
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `statfs` вернул 0 и заполнил структуру.
    let stat = unsafe { stat.assume_init() };
    let block = u64::from(stat.f_bsize);
    Ok((stat.f_bavail * block, stat.f_blocks * block))
}

/// Случайный номер попытки: 16 байт из `arc4random_buf` в шестнадцатеричном виде.
pub fn random_id() -> String {
    let mut bytes = [0u8; 16];
    // SAFETY: буфер живёт на время вызова, длина совпадает с его размером.
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast::<libc::c_void>(), bytes.len()) };
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "нулевой байт в пути"))
}

/// Переименование, которое не заменяет существующий путь (`renamex_np` с `RENAME_EXCL`): у
/// `rename(2)` каталог заменил бы пустой каталог назначения.
pub fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: обе строки живут на время вызова и оканчиваются нулём.
    let status = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

unsafe extern "C" {
    /// `sys/codesign.h`: в заголовках SDK пользователя её нет, символ — в `libsystem_kernel`.
    fn csops(
        pid: libc::pid_t,
        ops: libc::c_uint,
        useraddr: *mut libc::c_void,
        usersize: libc::size_t,
    ) -> libc::c_int;
}

const CS_OPS_CDHASH: libc::c_uint = 5;

/// CDHash кода процесса из ядра (`csops` `CS_OPS_CDHASH`), строчными: у процесса, загруженного до
/// обмена бандлов, он остаётся прежним при том же пути.
pub fn process_cdhash(pid: i32) -> io::Result<String> {
    let mut hash = [0u8; 20];
    // SAFETY: буфер из 20 байт живёт на время вызова, ядро пишет не больше `usersize`.
    let status = unsafe { csops(pid, CS_OPS_CDHASH, hash.as_mut_ptr().cast(), hash.len()) };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(hash.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// `PROC_PIDPATHINFO_MAXSIZE` из `libproc.h`.
const PIDPATH_MAX: usize = 4096;

/// Исполняемый файл процесса по `proc_pidpath`: путь, который ядро находит по vnode сейчас.
pub fn process_path(pid: i32) -> io::Result<PathBuf> {
    let mut buffer = vec![0u8; PIDPATH_MAX];
    // SAFETY: буфер длины `PIDPATH_MAX` живёт на время вызова.
    let len = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), PIDPATH_MAX as u32) };
    if len <= 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(len as usize);
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer)))
}

/// Открытый дескриптор процесса по `proc_pidfdinfo`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Descriptor {
    Vnode {
        fd: i32,
        dev: u64,
        ino: u64,
        path: String,
    },
    Socket {
        fd: i32,
        family: i32,
        /// Непрозрачный идентификатор сокета в ядре (`soi_so`): один и тот же у дескрипторов
        /// одного сокета в разных процессах.
        handle: u64,
        /// Свой адрес Unix-сокета (`unsi_addr`), у слушающего — путь `bind`.
        local: Option<String>,
        /// Адрес другой стороны (`unsi_caddr`).
        peer: Option<String>,
    },
    Other {
        fd: i32,
        fdtype: u32,
    },
    /// Закрылся между списком и чтением сведений.
    Gone {
        fd: i32,
    },
}

impl Descriptor {
    pub fn fd(&self) -> i32 {
        match self {
            Descriptor::Vnode { fd, .. }
            | Descriptor::Socket { fd, .. }
            | Descriptor::Other { fd, .. }
            | Descriptor::Gone { fd } => *fd,
        }
    }
}

/// `struct proc_fileinfo` из `sys/proc_info.h`.
#[repr(C)]
struct ProcFileInfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: i64,
    fi_type: i32,
    fi_guardflags: u32,
}

/// `struct vnode_fdinfowithpath` из `sys/proc_info.h`.
#[repr(C)]
struct VnodeFdInfoWithPath {
    pfi: ProcFileInfo,
    pvip: libc::vnode_info_path,
}

const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;
const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
const VNODE_INFO_SIZE: usize = 1200;
const _: () = assert!(size_of::<VnodeFdInfoWithPath>() == VNODE_INFO_SIZE);
/// `struct socket_fdinfo` (`sys/proc_info.h` SDK macOS 27): размер и смещения полей измерены
/// `offsetof` при сборке C-программы с этим заголовком. Ядро возвращает число записанных байтов,
/// оно сверяется с размером, а тест `descriptors_find_own_lock_and_listener` сверяет поля на
/// собственном сокете.
const SOCKET_INFO_SIZE: usize = 792;
const SOI_SO: usize = 160;
const SOI_FAMILY: usize = 184;
const SOI_KIND: usize = 256;
/// `soi_proto.pri_un.unsi_addr.ua_sun.sun_path` и `unsi_caddr…sun_path`.
const UNSI_ADDR_PATH: usize = 282;
const UNSI_CADDR_PATH: usize = 537;
const SUN_PATH_LEN: usize = 104;
const SOCKINFO_UN: i32 = 3;

fn field<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes[at..at + N]);
    out
}

fn sun_path(bytes: &[u8], at: usize) -> Option<String> {
    let raw = &bytes[at..at + SUN_PATH_LEN];
    let len = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    (len > 0).then(|| String::from_utf8_lossy(&raw[..len]).into_owned())
}

fn socket_descriptor(fd: i32, bytes: &[u8]) -> Descriptor {
    let kind = i32::from_ne_bytes(field(bytes, SOI_KIND));
    let unix = kind == SOCKINFO_UN;
    Descriptor::Socket {
        fd,
        family: i32::from_ne_bytes(field(bytes, SOI_FAMILY)),
        handle: u64::from_ne_bytes(field(bytes, SOI_SO)),
        local: unix.then(|| sun_path(bytes, UNSI_ADDR_PATH)).flatten(),
        peer: unix.then(|| sun_path(bytes, UNSI_CADDR_PATH)).flatten(),
    }
}

/// Открытые дескрипторы процесса: `PROC_PIDLISTFDS`, затем сведения о каждом.
pub fn descriptors(pid: i32) -> io::Result<Vec<Descriptor>> {
    let entry = size_of::<libc::proc_fdinfo>();
    // SAFETY: без буфера ядро возвращает нужный размер.
    let needed =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Err(io::Error::last_os_error());
    }
    let mut list: Vec<libc::proc_fdinfo> = Vec::with_capacity(needed as usize / entry + 16);
    let capacity = (list.capacity() * entry) as libc::c_int;
    // SAFETY: буфер на `capacity` байт живёт на время вызова.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            list.as_mut_ptr().cast(),
            capacity,
        )
    };
    if written <= 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ядро записало `written` байт целыми записями `proc_fdinfo`.
    unsafe { list.set_len(written as usize / entry) };
    let mut found = Vec::with_capacity(list.len());
    for info in &list {
        let fd = info.proc_fd;
        let descriptor = match info.proc_fdtype as libc::c_int {
            libc::PROX_FDTYPE_VNODE => {
                let mut vnode = MaybeUninit::<VnodeFdInfoWithPath>::zeroed();
                // SAFETY: буфер размера `VNODE_INFO_SIZE` живёт на время вызова.
                let got = unsafe {
                    libc::proc_pidfdinfo(
                        pid,
                        fd,
                        PROC_PIDFDVNODEPATHINFO,
                        vnode.as_mut_ptr().cast(),
                        VNODE_INFO_SIZE as libc::c_int,
                    )
                };
                match got {
                    0 => Descriptor::Gone { fd },
                    n if n as usize == VNODE_INFO_SIZE => {
                        // SAFETY: ядро записало структуру целиком, остальное обнулено заранее.
                        let vnode = unsafe { vnode.assume_init() };
                        let stat = &vnode.pvip.vip_vi.vi_stat;
                        let path: Vec<u8> = vnode
                            .pvip
                            .vip_path
                            .iter()
                            .flatten()
                            .take_while(|byte| **byte != 0)
                            .map(|byte| *byte as u8)
                            .collect();
                        Descriptor::Vnode {
                            fd,
                            dev: u64::from(stat.vst_dev),
                            ino: stat.vst_ino,
                            path: String::from_utf8_lossy(&path).into_owned(),
                        }
                    }
                    n => {
                        return Err(io::Error::other(format!(
                            "proc_pidfdinfo vnode {fd}: {n} байт, ожидалось {VNODE_INFO_SIZE}"
                        )));
                    }
                }
            }
            libc::PROX_FDTYPE_SOCKET => {
                let mut buffer = [0u64; SOCKET_INFO_SIZE / 8];
                // SAFETY: буфер размера `SOCKET_INFO_SIZE` живёт на время вызова.
                let got = unsafe {
                    libc::proc_pidfdinfo(
                        pid,
                        fd,
                        PROC_PIDFDSOCKETINFO,
                        buffer.as_mut_ptr().cast(),
                        SOCKET_INFO_SIZE as libc::c_int,
                    )
                };
                match got {
                    0 => Descriptor::Gone { fd },
                    n if n as usize == SOCKET_INFO_SIZE => {
                        let bytes: Vec<u8> =
                            buffer.iter().flat_map(|word| word.to_ne_bytes()).collect();
                        socket_descriptor(fd, &bytes)
                    }
                    n => {
                        return Err(io::Error::other(format!(
                            "proc_pidfdinfo socket {fd}: {n} байт, ожидалось {SOCKET_INFO_SIZE}"
                        )));
                    }
                }
            }
            _ => Descriptor::Other {
                fd,
                fdtype: info.proc_fdtype,
            },
        };
        found.push(descriptor);
    }
    Ok(found)
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
    fn timeout_of_socket_closed_by_peer_keeps_buffered_frames() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let (mut ours, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(b"frame").unwrap();
        drop(peer);
        let raw = ours.set_read_timeout(Some(Duration::from_secs(1)));
        assert_eq!(raw.unwrap_err().raw_os_error(), Some(libc::EINVAL));
        socket_timeout(ours.set_read_timeout(Some(Duration::from_secs(1)))).unwrap();
        let mut got = Vec::new();
        ours.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"frame");
        let zero = socket_timeout(ours.set_read_timeout(Some(Duration::ZERO)));
        assert!(zero.is_err(), "нулевой срок отвергает std, а не EINVAL");
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

    #[test]
    fn process_identity_from_the_kernel() {
        let pid = std::process::id() as i32;
        let hash = process_cdhash(pid).unwrap();
        assert_eq!(hash.len(), 40);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        let path = process_path(pid).unwrap();
        assert_eq!(
            std::fs::canonicalize(path).unwrap(),
            std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap()
        );
        assert!(process_cdhash(-7).is_err());
    }

    #[test]
    fn descriptors_find_own_lock_and_listener() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let dir =
            std::env::temp_dir().join(format!("psu-fd-{}-{}", std::process::id(), uptime_ns()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let file = File::create(dir.join("lock")).unwrap();
        let meta = file.metadata().unwrap();
        let socket = dir.join("l.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let client = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        let found = descriptors(std::process::id() as i32).unwrap();
        let vnode = found.iter().find(|d| d.fd() == file.as_raw_fd()).unwrap();
        assert!(
            matches!(vnode, Descriptor::Vnode { dev, ino, path, .. }
                if *dev == meta.dev() && *ino == meta.ino() && path.ends_with("/lock")),
            "{vnode:?}"
        );
        let listening = found
            .iter()
            .find(|d| d.fd() == listener.as_raw_fd())
            .unwrap();
        let Descriptor::Socket {
            family,
            handle,
            local,
            ..
        } = listening
        else {
            panic!("{listening:?}");
        };
        assert_eq!(*family, libc::AF_UNIX);
        assert_ne!(*handle, 0);
        assert_eq!(local.as_deref(), Some(socket.to_str().unwrap()));
        let connected = found.iter().find(|d| d.fd() == client.as_raw_fd()).unwrap();
        assert!(
            matches!(connected, Descriptor::Socket { peer: Some(peer), local: None, handle: other, .. }
                if peer == socket.to_str().unwrap() && other != handle),
            "{connected:?}"
        );
        drop((client, listener, file));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn exclusive_rename_does_not_replace() {
        let dir =
            std::env::temp_dir().join(format!("psu-rx-{}-{}", std::process::id(), uptime_ns()));
        let (from, to) = (dir.join("from"), dir.join("to"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        assert_eq!(
            rename_exclusive(&from, &to).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        std::fs::remove_dir(&to).unwrap();
        rename_exclusive(&from, &to).unwrap();
        assert!(to.is_dir() && !from.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
