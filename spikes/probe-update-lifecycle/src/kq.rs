//! Ожидание событий без `sleep` и опроса: `kqueue` с `EVFILT_VNODE` (`NOTE_WRITE`) на каталоги,
//! `EVFILT_PROC` (`NOTE_EXIT`) на процессы и `EVFILT_USER` для пробуждения из другого потока.
//! Каталог меняется, когда в нём появляется, исчезает или заменяется `rename` запись: так видны
//! путь сокета, файл барьера и журнал, который пишется через временный файл. Только системные C
//! API (ADR-0034).

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

const USER_EVENT: usize = 1;

/// Очередь событий; каталоги держатся открытыми, пока она жива.
pub struct Queue {
    fd: Arc<OwnedFd>,
    dirs: Vec<File>,
}

/// Событие пробуждения ожидания из другого потока; держит очередь открытой, пока живо.
pub struct Trigger(Arc<OwnedFd>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Изменился один из каталогов.
    Changed,
    /// Вышел процесс с этим PID.
    Exited(i32),
    /// Сработал [`Trigger`].
    Fired,
    TimedOut,
}

fn entry(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    }
}

fn change(queue: &OwnedFd, change: &libc::kevent) -> io::Result<()> {
    // SAFETY: одно изменение на стеке, список событий не передаётся.
    let status = unsafe {
        libc::kevent(
            queue.as_raw_fd(),
            change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if status < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Queue {
    pub fn new() -> Result<Queue, String> {
        // SAFETY: вызов без аргументов возвращает дескриптор или -1.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(format!("kqueue: {}", io::Error::last_os_error()));
        }
        // SAFETY: `raw` только что открыт и никому больше не принадлежит.
        let fd = Arc::new(unsafe { OwnedFd::from_raw_fd(raw) });
        change(
            &fd,
            &entry(
                USER_EVENT,
                libc::EVFILT_USER,
                libc::EV_ADD | libc::EV_CLEAR,
                0,
            ),
        )
        .map_err(|err| format!("kevent EVFILT_USER: {err}"))?;
        Ok(Queue {
            fd,
            dirs: Vec::new(),
        })
    }

    /// Очередь с наблюдением за одним каталогом.
    pub fn on_dir(dir: &Path) -> Result<Queue, String> {
        let mut queue = Queue::new()?;
        queue.watch_dir(dir)?;
        Ok(queue)
    }

    /// Наблюдение за записями каталога. Регистрировать до проверки того, чего ждёшь: иначе
    /// изменение между проверкой и регистрацией потеряется.
    pub fn watch_dir(&mut self, dir: &Path) -> Result<(), String> {
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_EVTONLY | libc::O_CLOEXEC)
            .open(dir)
            .map_err(|err| format!("{}: {err}", dir.display()))?;
        change(
            &self.fd,
            &entry(
                file.as_raw_fd() as usize,
                libc::EVFILT_VNODE,
                libc::EV_ADD | libc::EV_CLEAR,
                libc::NOTE_WRITE,
            ),
        )
        .map_err(|err| format!("kevent каталога {}: {err}", dir.display()))?;
        self.dirs.push(file);
        Ok(())
    }

    /// Наблюдение за выходом процесса; `false` — процесса уже нет (`ESRCH`).
    pub fn watch_exit(&self, pid: i32) -> Result<bool, String> {
        match change(
            &self.fd,
            &entry(
                pid as usize,
                libc::EVFILT_PROC,
                libc::EV_ADD | libc::EV_ONESHOT,
                libc::NOTE_EXIT,
            ),
        ) {
            Ok(()) => Ok(true),
            Err(err) if err.raw_os_error() == Some(libc::ESRCH) => Ok(false),
            Err(err) => Err(format!("kevent NOTE_EXIT {pid}: {err}")),
        }
    }

    pub fn trigger(&self) -> Trigger {
        Trigger(Arc::clone(&self.fd))
    }

    /// Следующее событие не позже `deadline`.
    pub fn wait(&self, deadline: Instant) -> Result<Event, String> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Event::TimedOut);
            }
            let timeout = libc::timespec {
                tv_sec: left.as_secs() as libc::time_t,
                tv_nsec: libc::c_long::from(left.subsec_nanos()),
            };
            let mut event = entry(0, 0, 0, 0);
            // SAFETY: одно событие на стеке и срок живут на время вызова.
            let count = unsafe {
                libc::kevent(
                    self.fd.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &mut event,
                    1,
                    &timeout,
                )
            };
            match count {
                0 => return Ok(Event::TimedOut),
                n if n > 0 => {
                    return Ok(match event.filter {
                        libc::EVFILT_USER => Event::Fired,
                        libc::EVFILT_PROC => Event::Exited(event.ident as i32),
                        _ => Event::Changed,
                    });
                }
                _ => {
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::Interrupted {
                        return Err(format!("kevent: {err}"));
                    }
                }
            }
        }
    }
}

impl Trigger {
    pub fn fire(&self) {
        let _ = change(
            &self.0,
            &entry(USER_EVENT, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::tests::temp_dir;
    use std::fs;
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn wakes_on_entries_exits_and_triggers_without_polling() {
        let dir = temp_dir("kq");
        let queue = Queue::on_dir(&dir).unwrap();
        File::create(dir.join("published")).unwrap();
        assert_eq!(
            queue.wait(Instant::now() + Duration::from_secs(5)).unwrap(),
            Event::Changed
        );
        let trigger = queue.trigger();
        thread::spawn(move || trigger.fire()).join().unwrap();
        assert_eq!(
            queue.wait(Instant::now() + Duration::from_secs(5)).unwrap(),
            Event::Fired
        );
        let mut child = Command::new("/bin/cat")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        assert!(queue.watch_exit(pid).unwrap());
        drop(child.stdin.take());
        assert_eq!(
            queue.wait(Instant::now() + Duration::from_secs(5)).unwrap(),
            Event::Exited(pid)
        );
        child.wait().unwrap();
        assert!(
            !queue.watch_exit(pid).unwrap(),
            "прибранный процесс — ESRCH"
        );
        assert_eq!(queue.wait(Instant::now()).unwrap(), Event::TimedOut);
        fs::remove_dir_all(&dir).unwrap();
    }
}
