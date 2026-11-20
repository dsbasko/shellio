//! Источник процессов: `sysctl(KERN_PROC_ALL)`, `proc_pid_rusage`, `proc_pidinfo(PROC_PIDTBSDINFO)`
//! и `getsid` с различением отказов (план P0.8, «Solution Overview»).
//!
//! Чтение PID связывается с записью списка временем старта из `proc_pidinfo`, взятым после
//! `proc_pid_rusage` и `getsid`: оба времени — настенный `p_start` ядра, одни часы. Совпадение
//! значит, что PID от перечисления до конца чтения держал один процесс. Идентичность —
//! `(pid, ri_proc_start_abstime)`; повторное чтение известной идентичности сверяет
//! `ri_proc_start_abstime`. Часы Mach с настенными часами не сравниваются.

pub mod units;

use std::collections::BTreeMap;
use std::fmt;
use std::mem::{offset_of, size_of};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Identity {
    pub pid: i32,
    /// `ri_proc_start_abstime`.
    pub start_abstime: u64,
}

/// Настенное время старта процесса (`p_start` ядра): `timeval` из `kinfo_proc` или
/// `pbi_start_tvsec`/`pbi_start_tvusec` из `proc_bsdinfo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StartTime {
    pub sec: u64,
    pub usec: u32,
}

/// Отказ одного вызова для одного PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadError {
    /// `ESRCH`: процесс вышел.
    Exited,
    /// `EPERM`: нет прав.
    Denied,
    Other {
        errno: i32,
    },
    /// `proc_pidinfo` вернул не весь буфер.
    Short {
        bytes: i32,
    },
}

impl ReadError {
    pub fn from_errno(errno: i32) -> ReadError {
        match errno {
            libc::ESRCH => ReadError::Exited,
            libc::EPERM => ReadError::Denied,
            errno => ReadError::Other { errno },
        }
    }

    /// `errno` отказа; у короткого ответа его нет.
    pub fn errno(self) -> Option<i32> {
        match self {
            ReadError::Exited => Some(libc::ESRCH),
            ReadError::Denied => Some(libc::EPERM),
            ReadError::Other { errno } => Some(errno),
            ReadError::Short { .. } => None,
        }
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Exited => write!(f, "процесс вышел (ESRCH)"),
            ReadError::Denied => write!(f, "нет прав (EPERM)"),
            ReadError::Other { errno } => write!(f, "errno {errno}"),
            ReadError::Short { bytes } => write!(f, "короткий ответ: {bytes} байт"),
        }
    }
}

/// Запись списка `sysctl(KERN_PROC_ALL)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listed {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub start: StartTime,
    pub ruid: u32,
    pub uid: u32,
    pub comm: String,
    pub zombie: bool,
}

/// `proc_pidinfo(PROC_PIDTBSDINFO)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BsdInfo {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub start: StartTime,
    pub uid: u32,
    pub ruid: u32,
    /// `pbi_name`, при пустом — `pbi_comm`.
    pub name: String,
}

/// `proc_pid_rusage(RUSAGE_INFO_V4)`: нужные поля.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub start_abstime: u64,
    /// `ri_user_time` в тиках Mach.
    pub user_mach: u64,
    /// `ri_system_time` в тиках Mach.
    pub system_mach: u64,
    /// `ri_phys_footprint`, байты.
    pub phys_footprint: u64,
}

impl Usage {
    pub fn cpu_mach(self) -> u64 {
        self.user_mach.saturating_add(self.system_mach)
    }
}

/// Источник процессов: настоящий ([`System`]) или подставной в тестах.
pub trait Source {
    fn list(&self) -> Result<Vec<Listed>, String>;
    fn usage(&self, pid: i32) -> Result<Usage, ReadError>;
    fn session(&self, pid: i32) -> Result<i32, ReadError>;
    fn bsd_info(&self, pid: i32) -> Result<BsdInfo, ReadError>;
}

/// Почему чтение `rusage` не вошло в учёт.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Unread {
    /// `proc_pid_rusage` отказал.
    Failed { error: ReadError },
    /// Время старта после чтения не совпало со временем из списка: PID сменился между
    /// перечислением и чтением.
    Replaced { listed: StartTime, read: StartTime },
    /// Связь с записью списка не подтверждена: `proc_pidinfo` после чтения отказал.
    Unconfirmed { error: ReadError },
    /// Известная идентичность, а `rusage` отвечает другой жизнью PID.
    IdentityChanged { expected: u64, observed: u64 },
}

impl fmt::Display for Unread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unread::Failed { error } => write!(f, "proc_pid_rusage: {error}"),
            Unread::Replaced { listed, read } => write!(
                f,
                "PID сменился между перечислением и чтением: старт {}.{:06} в списке, {}.{:06} после чтения",
                listed.sec, listed.usec, read.sec, read.usec
            ),
            Unread::Unconfirmed { error } => {
                write!(f, "связь со списком не подтверждена: proc_pidinfo: {error}")
            }
            Unread::IdentityChanged { expected, observed } => write!(
                f,
                "другая жизнь PID: ri_proc_start_abstime {observed}, ожидалось {expected}"
            ),
        }
    }
}

/// Прочитанный процесс: идентичность и счётчики одной жизни PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    pub identity: Identity,
    pub usage: Usage,
}

/// Итог чтения одной записи списка: каждый вызов со своим итогом.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcRead {
    pub listed: Listed,
    pub reading: Result<Reading, Unread>,
    /// `getsid`. Подтверждён той же проверкой времени старта, что и чтение: при
    /// [`Unread::Replaced`] сессия и `info` принадлежат новому процессу и не связаны с записью.
    pub session: Result<i32, ReadError>,
    pub info: Result<BsdInfo, ReadError>,
}

/// Читает запись списка: `rusage`, `getsid`, затем `proc_pidinfo` как проверка, что PID всё это
/// время держал процесс из списка.
pub fn read_listed(source: &impl Source, listed: &Listed) -> ProcRead {
    let pid = listed.pid;
    let usage = source.usage(pid);
    let session = source.session(pid);
    let info = source.bsd_info(pid);
    let reading = match (usage, &info) {
        (_, Ok(info)) if info.start != listed.start => Err(Unread::Replaced {
            listed: listed.start,
            read: info.start,
        }),
        (Err(error), _) => Err(Unread::Failed { error }),
        (Ok(_), Err(error)) => Err(Unread::Unconfirmed { error: *error }),
        (Ok(usage), Ok(_)) => Ok(Reading {
            identity: Identity {
                pid,
                start_abstime: usage.start_abstime,
            },
            usage,
        }),
    };
    ProcRead {
        listed: listed.clone(),
        reading,
        session,
        info,
    }
}

/// Читает известную идентичность: другая жизнь PID — пропуск, а не её CPU и память.
pub fn read_identity(source: &impl Source, expected: Identity) -> Result<Usage, Unread> {
    let usage = source
        .usage(expected.pid)
        .map_err(|error| Unread::Failed { error })?;
    if usage.start_abstime != expected.start_abstime {
        return Err(Unread::IdentityChanged {
            expected: expected.start_abstime,
            observed: usage.start_abstime,
        });
    }
    Ok(usage)
}

/// Один проход: список и чтение каждой записи.
pub fn read_all(source: &impl Source) -> Result<Vec<ProcRead>, String> {
    Ok(source
        .list()?
        .iter()
        .map(|listed| read_listed(source, listed))
        .collect())
}

/// Отказы прохода по вызову и `errno`; короткий ответ — под ключом 0.
pub fn failure_counts(reads: &[ProcRead]) -> BTreeMap<String, BTreeMap<i32, usize>> {
    let mut counts: BTreeMap<String, BTreeMap<i32, usize>> = BTreeMap::new();
    let mut add = |call: &str, error: ReadError| {
        *counts
            .entry(call.to_owned())
            .or_default()
            .entry(error.errno().unwrap_or(0))
            .or_default() += 1;
    };
    for read in reads {
        if let Err(Unread::Failed { error }) = read.reading {
            add("proc_pid_rusage", error);
        }
        if let Err(error) = read.session {
            add("getsid", error);
        }
        if let Err(error) = read.info {
            add("proc_pidinfo", error);
        }
    }
    counts
}

/// Настоящий источник на системных вызовах macOS.
pub struct System;

impl Source for System {
    fn list(&self) -> Result<Vec<Listed>, String> {
        list_all()
    }

    fn usage(&self, pid: i32) -> Result<Usage, ReadError> {
        usage(pid)
    }

    fn session(&self, pid: i32) -> Result<i32, ReadError> {
        session(pid)
    }

    fn bsd_info(&self, pid: i32) -> Result<BsdInfo, ReadError> {
        bsd_info(pid)
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn c_name(raw: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub fn usage(pid: i32) -> Result<Usage, ReadError> {
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
    if rc != 0 {
        return Err(ReadError::from_errno(errno()));
    }
    Ok(Usage {
        start_abstime: info.ri_proc_start_abstime,
        user_mach: info.ri_user_time,
        system_mach: info.ri_system_time,
        phys_footprint: info.ri_phys_footprint,
    })
}

pub fn session(pid: i32) -> Result<i32, ReadError> {
    // SAFETY: getsid без указателей.
    let sid = unsafe { libc::getsid(pid) };
    if sid < 0 {
        return Err(ReadError::from_errno(errno()));
    }
    Ok(sid)
}

pub fn bsd_info(pid: i32) -> Result<BsdInfo, ReadError> {
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
    if got <= 0 {
        return Err(ReadError::from_errno(errno()));
    }
    if got != size {
        return Err(ReadError::Short { bytes: got });
    }
    let name = c_name(&info.pbi_name);
    Ok(BsdInfo {
        pid: info.pbi_pid as i32,
        ppid: info.pbi_ppid as i32,
        pgid: info.pbi_pgid as i32,
        start: StartTime {
            sec: info.pbi_start_tvsec,
            usec: info.pbi_start_tvusec as u32,
        },
        uid: info.pbi_uid,
        ruid: info.pbi_ruid,
        name: if name.is_empty() {
            c_name(&info.pbi_comm)
        } else {
            name
        },
    })
}

/// `struct kinfo_proc` из `<sys/sysctl.h>`: в libc 0.2.191 её нет. Нужные поля стоят на
/// смещениях, измеренных `clang` на macOS 27 arm64 (`offsetof`), остальное — заполнитель.
#[repr(C)]
#[derive(Clone, Copy)]
struct KinfoProc {
    /// `kp_proc.p_un.__p_starttime`.
    p_starttime: libc::timeval,
    _pad0: [u8; 36 - 16],
    p_stat: i8,
    _pad1: [u8; 3],
    p_pid: i32,
    _pad2: [u8; 243 - 44],
    p_comm: [libc::c_char; 17],
    _pad3: [u8; 392 - 260],
    /// `kp_eproc.e_pcred.p_ruid`.
    p_ruid: u32,
    _pad4: [u8; 420 - 396],
    /// `kp_eproc.e_ucred.cr_uid`.
    cr_uid: u32,
    _pad5: [u8; 560 - 424],
    e_ppid: i32,
    e_pgid: i32,
    _pad6: [u8; 648 - 568],
}

const _: () = {
    assert!(size_of::<KinfoProc>() == 648);
    assert!(offset_of!(KinfoProc, p_starttime) == 0);
    assert!(offset_of!(KinfoProc, p_stat) == 36);
    assert!(offset_of!(KinfoProc, p_pid) == 40);
    assert!(offset_of!(KinfoProc, p_comm) == 243);
    assert!(offset_of!(KinfoProc, p_ruid) == 392);
    assert!(offset_of!(KinfoProc, cr_uid) == 420);
    assert!(offset_of!(KinfoProc, e_ppid) == 560);
    assert!(offset_of!(KinfoProc, e_pgid) == 564);
};

/// `SZOMB` из `<sys/proc.h>`.
const SZOMB: i8 = 5;
/// Запас записей на процессы, появившиеся между запросом размера и чтением.
const LIST_SLACK: usize = 64;
const LIST_ATTEMPTS: usize = 4;

fn to_listed(raw: &KinfoProc) -> Listed {
    Listed {
        pid: raw.p_pid,
        ppid: raw.e_ppid,
        pgid: raw.e_pgid,
        start: StartTime {
            sec: raw.p_starttime.tv_sec as u64,
            usec: raw.p_starttime.tv_usec as u32,
        },
        ruid: raw.p_ruid,
        uid: raw.cr_uid,
        comm: c_name(&raw.p_comm),
        zombie: raw.p_stat == SZOMB,
    }
}

/// Список всех процессов одним вызовом `sysctl(KERN_PROC_ALL)`; при росте списка между
/// запросом размера и чтением (`ENOMEM`) — повтор с большим буфером.
pub fn list_all() -> Result<Vec<Listed>, String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL];
    let entry = size_of::<KinfoProc>();
    for _ in 0..LIST_ATTEMPTS {
        let mut bytes: libc::size_t = 0;
        // SAFETY: нулевой буфер запрашивает только размер.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                std::ptr::null_mut(),
                &mut bytes,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(format!("sysctl(KERN_PROC_ALL) размер: errno {}", errno()));
        }
        let capacity = bytes / entry + LIST_SLACK;
        // SAFETY: KinfoProc — POD, нули допустимы.
        let mut buffer: Vec<KinfoProc> = vec![unsafe { std::mem::zeroed() }; capacity];
        let mut got = capacity * entry;
        // SAFETY: буфер на capacity записей kinfo_proc, got — его размер в байтах.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                buffer.as_mut_ptr().cast(),
                &mut got,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            let code = errno();
            if code == libc::ENOMEM {
                continue;
            }
            return Err(format!("sysctl(KERN_PROC_ALL): errno {code}"));
        }
        if !got.is_multiple_of(entry) {
            return Err(format!(
                "sysctl(KERN_PROC_ALL): {got} байт не кратно записи {entry}"
            ));
        }
        buffer.truncate(got / entry);
        return Ok(buffer
            .iter()
            .filter(|raw| raw.p_pid > 0)
            .map(to_listed)
            .collect());
    }
    Err(format!(
        "sysctl(KERN_PROC_ALL): список рос быстрее буфера {LIST_ATTEMPTS} раза подряд"
    ))
}

/// Подставной источник: ответы каждого вызова заданы заранее.
#[cfg(test)]
pub mod fake {
    use super::*;

    #[derive(Default)]
    pub struct FakeSource {
        pub listed: Vec<Listed>,
        pub usage: BTreeMap<i32, Result<Usage, ReadError>>,
        pub session: BTreeMap<i32, Result<i32, ReadError>>,
        pub info: BTreeMap<i32, Result<BsdInfo, ReadError>>,
    }

    pub fn start(sec: u64) -> StartTime {
        StartTime { sec, usec: 0 }
    }

    pub fn listed(pid: i32, ppid: i32, sec: u64) -> Listed {
        Listed {
            pid,
            ppid,
            pgid: ppid,
            start: start(sec),
            ruid: 501,
            uid: 501,
            comm: format!("p{pid}"),
            zombie: false,
        }
    }

    pub fn info_of(listed: &Listed) -> BsdInfo {
        BsdInfo {
            pid: listed.pid,
            ppid: listed.ppid,
            pgid: listed.pgid,
            start: listed.start,
            uid: listed.uid,
            ruid: listed.ruid,
            name: listed.comm.clone(),
        }
    }

    pub fn usage_of(start_abstime: u64, cpu_mach: u64) -> Usage {
        Usage {
            start_abstime,
            user_mach: cpu_mach,
            system_mach: 0,
            phys_footprint: 1 << 20,
        }
    }

    impl FakeSource {
        /// Процесс, который все вызовы читают согласованно.
        pub fn add(&mut self, listed: Listed, start_abstime: u64, cpu_mach: u64) {
            let pid = listed.pid;
            self.info.insert(pid, Ok(info_of(&listed)));
            self.usage
                .insert(pid, Ok(usage_of(start_abstime, cpu_mach)));
            self.session.insert(pid, Ok(1));
            self.listed.push(listed);
        }
    }

    impl Source for FakeSource {
        fn list(&self) -> Result<Vec<Listed>, String> {
            Ok(self.listed.clone())
        }

        fn usage(&self, pid: i32) -> Result<Usage, ReadError> {
            self.usage
                .get(&pid)
                .copied()
                .unwrap_or(Err(ReadError::Exited))
        }

        fn session(&self, pid: i32) -> Result<i32, ReadError> {
            self.session
                .get(&pid)
                .copied()
                .unwrap_or(Err(ReadError::Exited))
        }

        fn bsd_info(&self, pid: i32) -> Result<BsdInfo, ReadError> {
            self.info
                .get(&pid)
                .cloned()
                .unwrap_or(Err(ReadError::Exited))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    #[test]
    fn errno_maps_to_exited_denied_and_other() {
        assert_eq!(ReadError::from_errno(libc::ESRCH), ReadError::Exited);
        assert_eq!(ReadError::from_errno(libc::EPERM), ReadError::Denied);
        assert_eq!(
            ReadError::from_errno(libc::EINVAL),
            ReadError::Other {
                errno: libc::EINVAL
            }
        );
        assert_eq!(ReadError::Denied.errno(), Some(libc::EPERM));
        assert_eq!(ReadError::Short { bytes: 12 }.errno(), None);
        let json = serde_json::to_string(&ReadError::Other { errno: 22 }).unwrap();
        assert_eq!(json, r#"{"kind":"other","errno":22}"#);
    }

    #[test]
    fn consistent_process_is_read_with_its_identity() {
        let mut source = FakeSource::default();
        source.add(listed(10, 1, 100), 5_000, 240);
        let read = read_listed(&source, &source.listed[0]);
        let reading = read.reading.unwrap();
        assert_eq!(
            reading.identity,
            Identity {
                pid: 10,
                start_abstime: 5_000
            }
        );
        assert_eq!(reading.usage.cpu_mach(), 240);
        assert_eq!(read.session, Ok(1));
    }

    #[test]
    fn pid_replaced_between_listing_and_read_is_skipped() {
        // В списке — процесс со стартом 100, а `rusage` и `proc_pidinfo` уже отвечают новым
        // процессом с тем же PID и стартом 105.
        let mut source = FakeSource::default();
        let old = listed(10, 1, 100);
        let new = listed(10, 1, 105);
        source.usage.insert(10, Ok(usage_of(9_000, 7)));
        source.session.insert(10, Ok(42));
        source.info.insert(10, Ok(info_of(&new)));
        let read = read_listed(&source, &old);
        assert_eq!(
            read.reading,
            Err(Unread::Replaced {
                listed: start(100),
                read: start(105)
            })
        );
        assert_eq!(
            read.session,
            Ok(42),
            "сессия нового процесса не выдаётся за связанную"
        );
    }

    #[test]
    fn exit_after_rusage_leaves_link_unconfirmed() {
        let mut source = FakeSource::default();
        let entry = listed(10, 1, 100);
        source.usage.insert(10, Ok(usage_of(9_000, 7)));
        let read = read_listed(&source, &entry);
        assert_eq!(
            read.reading,
            Err(Unread::Unconfirmed {
                error: ReadError::Exited
            })
        );
    }

    #[test]
    fn denied_rusage_keeps_listing_and_session() {
        // Случай `login` с setuid: `rusage` и `proc_pidinfo` отказывают, `getsid` отвечает.
        let mut source = FakeSource::default();
        let entry = listed(20, 10, 100);
        source.usage.insert(20, Err(ReadError::Denied));
        source.info.insert(20, Err(ReadError::Denied));
        source.session.insert(20, Ok(10));
        source.listed.push(entry.clone());
        let reads = read_all(&source).unwrap();
        assert_eq!(reads.len(), 1);
        assert_eq!(
            reads[0].reading,
            Err(Unread::Failed {
                error: ReadError::Denied
            })
        );
        assert_eq!(reads[0].session, Ok(10));
        assert_eq!(reads[0].listed, entry);
        let counts = failure_counts(&reads);
        assert_eq!(counts["proc_pid_rusage"][&libc::EPERM], 1);
        assert_eq!(counts["proc_pidinfo"][&libc::EPERM], 1);
        assert!(!counts.contains_key("getsid"));
    }

    #[test]
    fn known_identity_rejects_another_life_of_the_pid() {
        let mut source = FakeSource::default();
        source.add(listed(10, 1, 100), 5_000, 240);
        let same = Identity {
            pid: 10,
            start_abstime: 5_000,
        };
        assert_eq!(read_identity(&source, same).unwrap().cpu_mach(), 240);
        let earlier = Identity {
            pid: 10,
            start_abstime: 4_000,
        };
        assert_eq!(
            read_identity(&source, earlier),
            Err(Unread::IdentityChanged {
                expected: 4_000,
                observed: 5_000
            })
        );
        let gone = Identity {
            pid: 11,
            start_abstime: 1,
        };
        assert_eq!(
            read_identity(&source, gone),
            Err(Unread::Failed {
                error: ReadError::Exited
            })
        );
    }

    /// Настоящие вызовы на собственном процессе: раскладка `kinfo_proc` и связь со списком.
    #[test]
    fn system_source_reads_own_process_consistently() {
        let pid = std::process::id() as i32;
        let listing = list_all().unwrap();
        let own = listing
            .iter()
            .find(|l| l.pid == pid)
            .expect("свой PID в списке");
        // SAFETY: вызовы без указателей.
        let (ppid, pgid, uid) = unsafe { (libc::getppid(), libc::getpgrp(), libc::getuid()) };
        assert_eq!(
            (own.ppid, own.pgid, own.uid, own.ruid),
            (ppid, pgid, uid, uid)
        );
        assert!(!own.zombie);
        assert!(own.start.sec > 0);
        let read = read_listed(&System, own);
        let reading = read.reading.unwrap();
        assert!(reading.usage.phys_footprint > 0);
        assert_eq!(read.info.unwrap().start, own.start);
        // SAFETY: getsid(0) — сессия своего процесса.
        assert_eq!(read.session, Ok(unsafe { libc::getsid(0) }));
        let again = read_identity(&System, reading.identity).unwrap();
        assert!(again.cpu_mach() >= reading.usage.cpu_mach());
        assert!(listing.iter().any(|l| l.pid == 1));
    }

    #[test]
    fn system_source_tells_exit_from_denial() {
        // PID выше `PID_MAX` (99 999) не существует.
        let absent = 999_999;
        assert_eq!(super::usage(absent), Err(ReadError::Exited));
        assert_eq!(session(absent), Err(ReadError::Exited));
        assert_eq!(bsd_info(absent), Err(ReadError::Exited));
    }
}
