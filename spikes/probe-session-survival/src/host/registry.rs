//! Запись Rust-хоста для потери хоста (ADR-0004, план P0.4, «Rust-хост»): PID и время старта
//! хоста и нагрузки, группа и сессия нагрузки. Хост пишет её атомарно после запуска нагрузки;
//! после его гибели по ней находятся оставшиеся процессы, в том числе сменившие родителя.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::backend::HostRecord;
use crate::observer::{self, ProcessInfo, ProcessKey};

pub fn record_path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.record.json"))
}

pub fn write(dir: &Path, record: &HostRecord) -> io::Result<PathBuf> {
    let path = record_path(dir, record.session.as_str());
    let temp = dir.join(format!(".{}.record.{}", record.session, std::process::id()));
    let text = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
    fs::write(&temp, text)?;
    fs::rename(&temp, &path)?;
    Ok(path)
}

pub fn read(dir: &Path, session: &str) -> io::Result<Option<HostRecord>> {
    match fs::read(record_path(dir, session)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Живые процессы сессии по записи: хост и нагрузка по ключу, остальные — по сессии или группе
/// нагрузки и времени старта не раньше нагрузки (PID мог занять чужой процесс).
pub fn remaining(record: &HostRecord, processes: &[ProcessInfo]) -> Vec<ProcessKey> {
    let mut found: Vec<ProcessKey> = processes
        .iter()
        .filter(|info| info.is_alive())
        .filter(|info| {
            info.key == record.host
                || info.key == record.workload
                || (info.key.start_us >= record.workload.start_us
                    && (info.pgid == record.workload_pgid
                        || (record.workload_sid.is_some() && info.sid == record.workload_sid)))
        })
        .map(|info| info.key)
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Все процессы системы, которые удалось прочитать.
pub fn all_processes() -> Result<Vec<ProcessInfo>, String> {
    let mut found = Vec::new();
    for pid in all_pids()? {
        if pid > 0
            && let Some(info) = observer::inspect(pid)?
        {
            found.push(info);
        }
    }
    Ok(found)
}

fn all_pids() -> Result<Vec<i32>, String> {
    let mut pids = vec![0i32; 4096];
    loop {
        let bytes =
            libc::c_int::try_from(pids.len() * size_of::<i32>()).unwrap_or(libc::c_int::MAX);
        // SAFETY: буфер длиной `bytes` байт жив на время вызова.
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        let Ok(count) = usize::try_from(count) else {
            return Err(format!("proc_listallpids: {}", io::Error::last_os_error()));
        };
        if count < pids.len() {
            pids.truncate(count);
            return Ok(pids);
        }
        if pids.len() >= 1 << 20 {
            return Err("proc_listallpids: слишком много процессов".to_owned());
        }
        pids = vec![0; pids.len() * 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SessionId;
    use crate::observer::State;

    fn info(pid: i32, start_us: i64, pgid: i32, sid: Option<i32>, state: State) -> ProcessInfo {
        ProcessInfo {
            key: ProcessKey { pid, start_us },
            ppid: 1,
            pgid,
            sid,
            tpgid: None,
            tty: None,
            state,
            exiting: false,
            comm: "fixture".to_owned(),
        }
    }

    fn record() -> HostRecord {
        HostRecord {
            session: SessionId::new("s1-a6").unwrap(),
            host: ProcessKey {
                pid: 100,
                start_us: 1_000,
            },
            workload: ProcessKey {
                pid: 101,
                start_us: 2_000,
            },
            workload_pgid: 101,
            workload_sid: Some(101),
        }
    }

    #[test]
    fn record_round_trips_atomically() {
        let dir = std::env::temp_dir().join(format!("pss-reg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(read(&dir, "s1-a6").unwrap(), None);
        let path = write(&dir, &record()).unwrap();
        assert_eq!(path, record_path(&dir, "s1-a6"));
        assert_eq!(read(&dir, "s1-a6").unwrap(), Some(record()));
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["s1-a6.record.json"]);
        fs::write(record_path(&dir, "bad"), b"{").unwrap();
        assert_eq!(
            read(&dir, "bad").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remaining_finds_reparented_session_members_only() {
        let processes = vec![
            // нагрузка уже вышла и не прибрана
            info(101, 2_000, 101, None, State::Zombie),
            // потомок, игнорирующий HUP, сменил родителя, но остался в сессии
            info(205, 3_000, 205, Some(101), State::Sleeping),
            // фоновое задание в группе нагрузки
            info(206, 3_100, 101, Some(101), State::Running),
            // PID из той же сессии, но старше нагрузки: чужой процесс
            info(90, 500, 90, Some(101), State::Sleeping),
            // посторонний
            info(300, 4_000, 300, Some(300), State::Running),
        ];
        assert_eq!(
            remaining(&record(), &processes),
            [
                ProcessKey {
                    pid: 205,
                    start_us: 3_000
                },
                ProcessKey {
                    pid: 206,
                    start_us: 3_100
                },
            ]
        );
    }

    #[test]
    fn live_host_and_workload_are_found_by_key() {
        let processes = vec![
            info(100, 1_000, 100, Some(100), State::Running),
            info(101, 2_000, 101, Some(101), State::Sleeping),
            // тот же PID хоста, другое время старта
            info(100, 9_999, 100, Some(100), State::Running),
        ];
        assert_eq!(
            remaining(&record(), &processes),
            [record().host, record().workload]
        );
    }

    #[test]
    fn own_process_is_among_all_processes() {
        let own = std::process::id() as i32;
        assert!(
            all_processes()
                .unwrap()
                .iter()
                .any(|info| info.key.pid == own)
        );
    }
}
