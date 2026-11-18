//! Наблюдатель процессов: процесс узнаётся по паре PID и времени старта, а не по одному PID.
//!
//! Время старта читается из `sysctl` `kern.proc.pid` (`p_starttime`), остальное — из
//! `proc_pidinfo(PROC_PIDTBSDINFO)` с аргументом 1: так ядро отдаёт и процесс, который уже вышел,
//! но ещё не прибран родителем (зомби). Оба источника сверяются по времени старта: расхождение
//! значит, что PID успели переиспользовать между двумя чтениями. Сессия — `getsid`, у зомби её нет.
//! Только системные C API (ADR-0034).
//!
//! Выход процесса не мгновенный: `kqueue` сообщает о нём, когда процесс уже в `exit()`, а `p_stat`
//! ещё не `SZOMB`. Такой процесс отмечен признаком `PROC_FLAG_INEXIT` и не считается исполняющимся.
//!
//! Источник — `spikes/probe-session-survival/src/observer.rs`, без изменений по существу.

use std::io;
use std::mem::MaybeUninit;

use serde::{Deserialize, Serialize};

/// `NODEV`: у процесса нет управляющего терминала.
const NO_TTY: u32 = u32::MAX;
/// `PROC_FLAG_INEXIT` из `sys/proc_info.h`: процесс проходит `exit()`.
const PROC_FLAG_INEXIT: u32 = 4;
/// Повторы чтения, если PID переиспользован между `sysctl` и `proc_pidinfo`.
const READ_ATTEMPTS: usize = 3;
const MAX_TREE: usize = 4096;

/// Процесс: PID и время старта в микросекундах от эпохи Unix. Тот же PID с другим временем
/// старта — уже другой процесс.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProcessKey {
    pub pid: i32,
    pub start_us: i64,
}

/// Состояние процесса по `p_stat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Idle,
    Running,
    Sleeping,
    Stopped,
    /// Вышел, но не прибран родителем.
    Zombie,
    Other(u32),
}

impl State {
    pub fn from_raw(raw: u32) -> State {
        match raw {
            libc::SIDL => State::Idle,
            libc::SRUN => State::Running,
            libc::SSLEEP => State::Sleeping,
            libc::SSTOP => State::Stopped,
            libc::SZOMB => State::Zombie,
            other => State::Other(other),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub key: ProcessKey,
    pub ppid: i32,
    pub pgid: i32,
    /// `None` у зомби: `getsid` его уже не находит.
    pub sid: Option<i32>,
    /// Группа переднего плана управляющего терминала; `None` без терминала.
    pub tpgid: Option<i32>,
    /// Устройство управляющего терминала; `None` без терминала.
    pub tty: Option<u32>,
    pub state: State,
    /// Процесс уже в `exit()`, хотя `state` может быть ещё не `Zombie`.
    pub exiting: bool,
    pub comm: String,
}

impl ProcessInfo {
    /// Процесс исполняется: существует, не зомби и не выходит.
    pub fn is_alive(&self) -> bool {
        self.state != State::Zombie && !self.exiting
    }
}

/// Что стало с процессом, известным по [`ProcessKey`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Identity {
    /// Тот же процесс; он может быть зомби, это видно по `state`.
    Same(ProcessInfo),
    /// PID занят другим процессом.
    Reused { found: ProcessKey },
    /// Процесса с этим PID нет.
    Gone,
}

impl Identity {
    /// Тот же процесс и он исполняется.
    pub fn is_alive(&self) -> bool {
        matches!(self, Identity::Same(info) if info.is_alive())
    }
}

/// Сравнение известного процесса с тем, что сейчас стоит под его PID.
pub fn classify(expected: ProcessKey, found: Option<&ProcessInfo>) -> Identity {
    match found {
        None => Identity::Gone,
        Some(info) if info.key == expected => Identity::Same(info.clone()),
        Some(info) => Identity::Reused { found: info.key },
    }
}

pub fn identify(expected: ProcessKey) -> Result<Identity, String> {
    Ok(classify(expected, inspect(expected.pid)?.as_ref()))
}

/// Сведения о процессе; `None`, если процесса с этим PID нет.
pub fn inspect(pid: i32) -> Result<Option<ProcessInfo>, String> {
    if pid <= 0 {
        return Err(format!("PID {pid}"));
    }
    for _ in 0..READ_ATTEMPTS {
        let Some(start_us) = sysctl_start_us(pid)? else {
            return Ok(None);
        };
        let Some(bsd) = bsd_info(pid) else {
            return Ok(None);
        };
        let bsd_start = i64::try_from(bsd.pbi_start_tvsec).unwrap_or(i64::MAX / 2_000_000)
            * 1_000_000
            + i64::try_from(bsd.pbi_start_tvusec).unwrap_or(0);
        if bsd_start != start_us {
            continue;
        }
        let state = State::from_raw(bsd.pbi_status);
        // SAFETY: вызов без указателей.
        let sid = unsafe { libc::getsid(pid) };
        let tty = (bsd.e_tdev != NO_TTY).then_some(bsd.e_tdev);
        return Ok(Some(ProcessInfo {
            key: ProcessKey { pid, start_us },
            ppid: bsd.pbi_ppid as i32,
            pgid: bsd.pbi_pgid as i32,
            sid: (sid >= 0).then_some(sid),
            tpgid: tty.and(Some(bsd.e_tpgid as i32)).filter(|pgid| *pgid > 0),
            tty,
            state,
            exiting: bsd.pbi_flags & PROC_FLAG_INEXIT != 0,
            comm: c_text(&bsd.pbi_comm),
        }));
    }
    Err(format!(
        "PID {pid}: время старта в sysctl и proc_pidinfo расходится {READ_ATTEMPTS} раза подряд"
    ))
}

/// Сведения о текущем процессе.
pub fn current() -> Result<ProcessInfo, String> {
    let pid = std::process::id() as i32;
    inspect(pid)?.ok_or_else(|| format!("PID {pid}: собственный процесс не найден"))
}

/// Прямые потомки, в том числе зомби.
pub fn children(pid: i32) -> Vec<i32> {
    let mut pids = vec![0i32; 64];
    loop {
        let bytes =
            libc::c_int::try_from(pids.len() * size_of::<i32>()).unwrap_or(libc::c_int::MAX);
        // SAFETY: буфер длиной `bytes` байт жив на время вызова.
        let count = unsafe { libc::proc_listchildpids(pid, pids.as_mut_ptr().cast(), bytes) };
        let Ok(count) = usize::try_from(count) else {
            return Vec::new();
        };
        // Заполненный буфер может означать, что потомков больше: повтор с вдвое большим.
        if count < pids.len() || pids.len() >= MAX_TREE {
            pids.truncate(count.min(pids.len()));
            pids.retain(|child| *child > 0);
            pids.sort_unstable();
            return pids;
        }
        pids = vec![0; pids.len() * 2];
    }
}

/// Дерево потомков в ширину, без самого `root`. Процессы, вышедшие за время обхода, пропускаются.
pub fn descendants(root: i32) -> Result<Vec<ProcessInfo>, String> {
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from(children(root));
    while let Some(pid) = queue.pop_front() {
        if found.len() >= MAX_TREE {
            return Err(format!("у {root} больше {MAX_TREE} потомков"));
        }
        if let Some(info) = inspect(pid)? {
            queue.extend(children(pid));
            found.push(info);
        }
    }
    Ok(found)
}

/// `kinfo_proc.kp_proc.p_un.__p_starttime` — первое поле структуры, `timeval`. Своего описания
/// `kinfo_proc` для macOS в libc нет, поэтому читается только это поле. Пустой ответ — процесса
/// нет.
fn sysctl_start_us(pid: i32) -> Result<Option<i64>, String> {
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
            "sysctl kern.proc.pid {pid}: {}",
            io::Error::last_os_error()
        ));
    }
    let mut buffer = vec![0u64; size.div_ceil(8).max(2)];
    let mut written = buffer.len() * 8;
    // SAFETY: буфер выровнен по 8 байтам и длиной не меньше `written`.
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
            "sysctl kern.proc.pid {pid}: {}",
            io::Error::last_os_error()
        ));
    }
    if written == 0 {
        return Ok(None);
    }
    if written < size_of::<libc::timeval>() {
        return Err(format!("sysctl kern.proc.pid {pid}: ответ {written} байт"));
    }
    // SAFETY: первые 16 байт буфера — `timeval` начала процесса, буфер выровнен.
    let start = unsafe { std::ptr::read(buffer.as_ptr().cast::<libc::timeval>()) };
    Ok(Some(start.tv_sec * 1_000_000 + i64::from(start.tv_usec)))
}

/// Аргумент 1 у `PROC_PIDTBSDINFO` находит и зомби.
fn bsd_info(pid: i32) -> Option<libc::proc_bsdinfo> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: для `PROC_PIDTBSDINFO` ядро пишет `proc_bsdinfo` размера `size`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: структура записана целиком.
    Some(unsafe { info.assume_init() })
}

fn c_text(raw: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::*;

    fn info(pid: i32, start_us: i64) -> ProcessInfo {
        ProcessInfo {
            key: ProcessKey { pid, start_us },
            ppid: 1,
            pgid: pid,
            sid: Some(pid),
            tpgid: None,
            tty: None,
            state: State::Sleeping,
            exiting: false,
            comm: "fixture".to_owned(),
        }
    }

    #[test]
    fn classification_tells_reused_pid_from_the_same_process() {
        let key = ProcessKey {
            pid: 812,
            start_us: 1_000,
        };
        assert_eq!(
            classify(key, Some(&info(812, 1_000))),
            Identity::Same(info(812, 1_000))
        );
        assert_eq!(
            classify(key, Some(&info(812, 2_000))),
            Identity::Reused {
                found: ProcessKey {
                    pid: 812,
                    start_us: 2_000
                }
            }
        );
        assert_eq!(classify(key, None), Identity::Gone);
        let mut zombie = info(812, 1_000);
        zombie.state = State::Zombie;
        let same = classify(key, Some(&zombie));
        assert!(matches!(same, Identity::Same(_)) && !same.is_alive());
        assert!(classify(key, Some(&info(812, 1_000))).is_alive());
        // Выходящий процесс ещё не зомби, но уже не исполняется.
        let mut exiting = info(812, 1_000);
        exiting.state = State::Running;
        exiting.exiting = true;
        assert!(!classify(key, Some(&exiting)).is_alive());
    }

    #[test]
    fn own_process_is_inspected_consistently() {
        let me = current().unwrap();
        assert_eq!(me.key.pid, std::process::id() as i32);
        // SAFETY: вызовы без указателей.
        let (ppid, pgid, sid) = unsafe { (libc::getppid(), libc::getpgrp(), libc::getsid(0)) };
        assert_eq!((me.ppid, me.pgid, me.sid), (ppid, pgid, Some(sid)));
        assert!(me.is_alive() && !me.exiting);
        assert!(me.key.start_us > 0);
        assert_eq!(
            identify(me.key).unwrap(),
            Identity::Same(inspect(me.key.pid).unwrap().unwrap())
        );
        assert!(inspect(0).is_err());
    }

    #[test]
    fn live_pid_with_another_start_time_is_reused() {
        let me = current().unwrap();
        let earlier = ProcessKey {
            pid: me.key.pid,
            start_us: me.key.start_us - 1,
        };
        assert_eq!(
            identify(earlier).unwrap(),
            Identity::Reused { found: me.key }
        );
    }

    #[test]
    fn exited_child_is_a_zombie_then_gone() {
        let mut child = Command::new("/usr/bin/true")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let key = inspect(pid).unwrap().unwrap().key;
        // Ждёт выхода, не прибирая потомка.
        // SAFETY: структура на стеке живёт на время вызова.
        let status = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        assert_eq!(status, 0, "{}", io::Error::last_os_error());
        let zombie = identify(key).unwrap();
        match &zombie {
            Identity::Same(info) => {
                assert_eq!(info.state, State::Zombie);
                assert!(info.exiting);
                assert_eq!(info.ppid, std::process::id() as i32);
                assert_eq!(info.sid, None);
            }
            other => panic!("ожидался зомби, пришло {other:?}"),
        }
        assert!(!zombie.is_alive());
        assert!(children(std::process::id() as i32).contains(&pid));
        child.wait().unwrap();
        assert_eq!(identify(key).unwrap(), Identity::Gone);
    }

    #[test]
    fn stopped_child_is_seen_as_stopped() {
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let mut status = 0;
        // SAFETY: сигнал потомку по PID и ожидание его остановки.
        let waited = unsafe {
            libc::kill(pid, libc::SIGSTOP);
            libc::waitpid(pid, &mut status, libc::WUNTRACED)
        };
        assert_eq!(waited, pid);
        assert!(libc::WIFSTOPPED(status));
        assert_eq!(inspect(pid).unwrap().unwrap().state, State::Stopped);
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn descendants_cover_two_levels() {
        use std::io::BufRead;

        // Оболочка печатает PID внука после fork: строка в stdout — квитанция, что он уже есть.
        // Фоновая команда без явного перенаправления читала бы /dev/null и сразу вышла.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec 3<&0; /bin/cat <&3 >/dev/null & echo $!; wait"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let mut line = String::new();
        io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild: i32 = line.trim().parse().unwrap();
        let tree = descendants(std::process::id() as i32).unwrap();
        let shell = tree.iter().find(|info| info.key.pid == pid).unwrap();
        assert_eq!(shell.ppid, std::process::id() as i32);
        let cat = tree.iter().find(|info| info.key.pid == grandchild).unwrap();
        assert_eq!(cat.ppid, pid);
        // SAFETY: сигнал внуку по PID.
        unsafe { libc::kill(cat.key.pid, libc::SIGKILL) };
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
