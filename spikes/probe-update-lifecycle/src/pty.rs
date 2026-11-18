//! PTY хоста пробы: `openpty`, запуск нагрузки лидером новой сессии (`setsid`) с ведомой стороной
//! PTY в роли управляющего терминала, размер окна, группа переднего плана и режим терминала.
//! Только системные C API (ADR-0034).
//!
//! Оба конца PTY получают `FD_CLOEXEC`, а `openpty` и запуск идут под одной блокировкой процесса:
//! иначе дескриптор мастера унаследовал бы процесс, запущенный из другого потока, и PTY не
//! закрылся бы вместе со своим владельцем.
//!
//! Источник — `spikes/probe-session-survival/src/pty/mod.rs`, без ожидания готовности (`select`),
//! `sigwait` и смены группы переднего плана: минимальному хосту P0.5 они не нужны.

use std::ffi::CStr;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

/// `TIOCSCTTY` = `_IO('t', 97)` из `sys/ttycom.h`; в libc для macOS константы нет.
const TIOCSCTTY: libc::c_ulong = 0x2000_7461;

static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Блокировка `openpty` и запусков процесса. Удерживается до установки `FD_CLOEXEC` и до
/// возврата `spawn`.
pub fn spawn_lock() -> MutexGuard<'static, ()> {
    SPAWN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Размер окна PTY по `TIOCGWINSZ`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

impl Winsize {
    pub fn cells(rows: u16, cols: u16) -> Winsize {
        Winsize {
            rows,
            cols,
            xpixel: 0,
            ypixel: 0,
        }
    }

    fn raw(self) -> libc::winsize {
        libc::winsize {
            ws_row: self.rows,
            ws_col: self.cols,
            ws_xpixel: self.xpixel,
            ws_ypixel: self.ypixel,
        }
    }
}

/// Пара концов PTY.
pub struct Pty {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

/// Новый PTY заданного размера, оба конца с `FD_CLOEXEC`.
pub fn open(size: Winsize) -> io::Result<Pty> {
    let _guard = spawn_lock();
    open_locked(size)
}

fn open_locked(size: Winsize) -> io::Result<Pty> {
    let (mut master, mut slave): (RawFd, RawFd) = (-1, -1);
    let mut raw = size.raw();
    // SAFETY: указатели на локальные переменные; имя и termios не запрашиваются.
    let status = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openpty` вернул два открытых дескриптора, которыми больше никто не владеет.
    let pty = unsafe {
        Pty {
            master: OwnedFd::from_raw_fd(master),
            slave: OwnedFd::from_raw_fd(slave),
        }
    };
    set_cloexec(pty.master.as_raw_fd())?;
    set_cloexec(pty.slave.as_raw_fd())?;
    Ok(pty)
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` открыт, флаги дескриптора читаются и пишутся без указателей.
    let status = unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            flags
        } else {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC)
        }
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Что запустить в PTY.
#[derive(Clone, Debug)]
pub struct Spawn<'a> {
    pub program: &'a Path,
    pub args: &'a [String],
    /// Окружение процесса целиком: родительское не наследуется.
    pub env: &'a [(String, String)],
    pub size: Winsize,
}

/// Процесс в собственном PTY: лидер новой сессии, ведомая сторона — его управляющий терминал и
/// стандартные потоки.
pub struct PtyChild {
    pub master: OwnedFd,
    pub child: Child,
    /// Имя ведомой стороны, например `/dev/ttys012`.
    pub tty: String,
}

impl PtyChild {
    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }
}

/// Запуск процесса во втором PTY: новый PTY, `setsid`, `TIOCSCTTY`, пустая маска сигналов.
pub fn spawn(spec: &Spawn<'_>) -> io::Result<PtyChild> {
    let _guard = spawn_lock();
    let pty = open_locked(spec.size)?;
    let tty = tty_name(pty.slave.as_raw_fd())?;
    let stdio = |fd: &OwnedFd| fd.try_clone().map(Stdio::from);
    let mut command = Command::new(spec.program);
    command
        .args(spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(name, value)| (name, value)))
        .stdin(stdio(&pty.slave)?)
        .stdout(stdio(&pty.slave)?)
        .stderr(stdio(&pty.slave)?);
    // SAFETY: замыкание выполняется в потомке между fork и exec и вызывает только
    // async-signal-safe функции.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            clear_signal_mask()
        });
    }
    let child = command.spawn()?;
    drop(pty.slave);
    Ok(PtyChild {
        master: pty.master,
        child,
        tty,
    })
}

/// Пустая маска сигналов: потомок наследует маску родителя. Вызывается в `pre_exec`, поэтому
/// только async-signal-safe функции.
pub fn clear_signal_mask() -> io::Result<()> {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigemptyset` инициализирует набор, затем он передаётся по указателю.
    let status = unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        libc::sigprocmask(libc::SIG_SETMASK, set.as_ptr(), std::ptr::null_mut())
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn winsize(fd: BorrowedFd<'_>) -> io::Result<Winsize> {
    let mut size = Winsize::cells(0, 0).raw();
    // SAFETY: `fd` открыт, `size` живёт на время вызова.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Winsize {
        rows: size.ws_row,
        cols: size.ws_col,
        xpixel: size.ws_xpixel,
        ypixel: size.ws_ypixel,
    })
}

/// Новый размер окна; ядро посылает `SIGWINCH` группе переднего плана.
pub fn set_winsize(fd: BorrowedFd<'_>, size: Winsize) -> io::Result<()> {
    let raw = size.raw();
    // SAFETY: `fd` открыт, структура живёт на время вызова.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &raw) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `tcgetpgrp`: группа переднего плана терминала, с любой стороны PTY.
pub fn foreground_pgid(fd: BorrowedFd<'_>) -> io::Result<i32> {
    // SAFETY: вызов без указателей, `fd` открыт.
    let pgid = unsafe { libc::tcgetpgrp(fd.as_raw_fd()) };
    if pgid < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pgid)
}

/// Имя терминала дескриптора. `ttyname_r` macOS 27 при параллельном открытии и закрытии PTY
/// изредка отвечает `ERANGE` на буфер в 256 байт: имя устройства не нашлось в devfs. Тогда имя
/// берётся у самого дескриптора через `F_GETPATH`.
pub fn tty_name(fd: RawFd) -> io::Result<String> {
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: буфер действителен на `buf.len()` байт, при успехе строка завершена нулём.
    let status = unsafe { libc::ttyname_r(fd, buf.as_mut_ptr(), buf.len()) };
    if status == libc::ERANGE {
        let mut path = [0 as libc::c_char; libc::PATH_MAX as usize];
        // SAFETY: `F_GETPATH` пишет путь с нулём в буфер размера `PATH_MAX`.
        if unsafe { libc::fcntl(fd, libc::F_GETPATH, path.as_mut_ptr()) } == 0 {
            // SAFETY: успешный `F_GETPATH` записал строку с нулём внутри буфера.
            let name = unsafe { CStr::from_ptr(path.as_ptr()) }.to_string_lossy();
            if name.starts_with("/dev/") {
                return Ok(name.into_owned());
            }
        }
    }
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }
    // SAFETY: успешный `ttyname_r` записал строку с нулём внутри буфера.
    let name = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Ok(name.to_string_lossy().into_owned())
}

pub fn get_termios(fd: BorrowedFd<'_>) -> io::Result<libc::termios> {
    let mut termios = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `tcgetattr` заполняет структуру при успехе.
    if unsafe { libc::tcgetattr(fd.as_raw_fd(), termios.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: успешный `tcgetattr` инициализировал структуру.
    Ok(unsafe { termios.assume_init() })
}

/// Режим терминала без сброса очередей: ввод, пришедший до смены режима, не теряется.
pub fn set_termios(fd: BorrowedFd<'_>, termios: &libc::termios) -> io::Result<()> {
    // SAFETY: `fd` открыт, структура живёт на время вызова.
    if unsafe { libc::tcsetattr(fd.as_raw_fd(), libc::TCSANOW, termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `cfmakeraw` поверх `termios`: без эха, без канонического режима, без сигналов от клавиш и
/// без обработки вывода.
pub fn raw_mode(termios: &libc::termios) -> libc::termios {
    let mut raw = *termios;
    // SAFETY: `raw` — инициализированная структура на стеке.
    unsafe { libc::cfmakeraw(&mut raw) };
    raw
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;

    use super::*;

    #[test]
    fn opened_pty_has_size_cloexec_and_a_name() {
        let pty = open(Winsize {
            rows: 30,
            cols: 100,
            xpixel: 1000,
            ypixel: 600,
        })
        .unwrap();
        assert_eq!(
            winsize(pty.slave.as_fd()).unwrap(),
            Winsize {
                rows: 30,
                cols: 100,
                xpixel: 1000,
                ypixel: 600
            }
        );
        for fd in [&pty.master, &pty.slave] {
            // SAFETY: дескриптор открыт.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        }
        assert!(
            tty_name(pty.slave.as_raw_fd())
                .unwrap()
                .starts_with("/dev/tty")
        );
        set_winsize(pty.master.as_fd(), Winsize::cells(10, 20)).unwrap();
        assert_eq!(winsize(pty.slave.as_fd()).unwrap(), Winsize::cells(10, 20));
    }

    #[test]
    fn raw_mode_passes_bytes_without_echo_or_translation() {
        let pty = open(Winsize::cells(24, 80)).unwrap();
        let cooked = get_termios(pty.slave.as_fd()).unwrap();
        assert_ne!(cooked.c_lflag & libc::ECHO, 0);
        set_termios(pty.slave.as_fd(), &raw_mode(&cooked)).unwrap();
        let raw = get_termios(pty.slave.as_fd()).unwrap();
        assert_eq!(raw.c_lflag & (libc::ECHO | libc::ICANON | libc::ISIG), 0);
        let mut master = File::from(pty.master.try_clone().unwrap());
        let mut slave = File::from(pty.slave.try_clone().unwrap());
        master.write_all(b"\x03a\n").unwrap();
        let mut got = [0u8; 3];
        slave.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"\x03a\n");
        slave.write_all(b"x\n").unwrap();
        let mut out = [0u8; 2];
        master.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"x\n", "без ONLCR и без эха ввода");
    }

    #[test]
    fn spawned_process_leads_a_session_with_the_pty_as_controlling_terminal() {
        let env = vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())];
        // Оболочка сообщает в PTY свой PID и терминал и ждёт строки ввода.
        let args = vec![
            "-c".to_owned(),
            r#"echo "ids $$ $(tty)"; read line"#.to_owned(),
        ];
        let mut first = spawn(&Spawn {
            program: Path::new("/bin/sh"),
            args: &args,
            env: &env,
            size: Winsize::cells(24, 80),
        })
        .unwrap();
        let mut second = spawn(&Spawn {
            program: Path::new("/bin/sh"),
            args: &args,
            env: &env,
            size: Winsize::cells(24, 80),
        })
        .unwrap();
        for child in [&first, &second] {
            let mut master = File::from(child.master.try_clone().unwrap());
            let mut text = String::new();
            let mut buf = [0u8; 256];
            while !text.contains('\n') {
                let read = master.read(&mut buf).unwrap();
                assert!(read > 0);
                text.push_str(&String::from_utf8_lossy(&buf[..read]));
            }
            let words: Vec<&str> = text.split_whitespace().collect();
            assert_eq!(words[0], "ids", "{text:?}");
            let pid = child.pid().to_string();
            assert_eq!(words[1], pid);
            assert_eq!(words[2], child.tty, "управляющий терминал");
            assert_eq!(foreground_pgid(child.master.as_fd()).unwrap(), child.pid());
            let info = crate::observer::inspect(child.pid()).unwrap().unwrap();
            assert_eq!(info.pgid, child.pid(), "лидер группы");
            assert_eq!(info.sid, Some(child.pid()), "лидер сессии");
            assert_eq!(info.tpgid, Some(child.pid()));
        }
        assert_ne!(first.tty, second.tty);
        for child in [&mut first, &mut second] {
            let mut master = File::from(child.master.try_clone().unwrap());
            master.write_all(b"done\n").unwrap();
            // Лидер сессии при выходе ждёт, пока вывод PTY прочитают: эхо ввода дочитывается до
            // закрытия ведомой стороны (EIO).
            let mut rest = Vec::new();
            let _ = master.read_to_end(&mut rest);
            assert!(child.child.wait().unwrap().success());
        }
    }
}
