//! Запуск потомков через `posix_spawn` с `POSIX_SPAWN_CLOEXEC_DEFAULT` (план P0.5, «Владение
//! блокировкой и сокетом»; ADR-0006). Потомок получает только stdin, stdout и stderr, заданные
//! явно, поэтому ни `shellio.lock`, ни сокет владельца к нему не попадут, даже если другой поток
//! в эту минуту открыл дескриптор без `O_CLOEXEC`. Сигналы потомка — по умолчанию с пустой маской:
//! раннер Rust игнорирует `SIGPIPE`, а игнорирование переживает `exec`. Только системные C API
//! (ADR-0034).

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::thread;

/// Что запустить.
pub struct Spec<'a> {
    pub program: &'a Path,
    /// Аргументы без имени программы: `argv[0]` — путь программы.
    pub args: &'a [String],
    /// Окружение целиком; `None` — окружение вызывающего.
    pub env: Option<&'a [(String, String)]>,
    /// stdin, stdout и stderr; `None` — `/dev/null`. Дескрипторы закрываются после запуска.
    pub stdio: [Option<OwnedFd>; 3],
    /// Новая сессия (`POSIX_SPAWN_SETSID`): потомок не делит группу и сессию с вызывающим.
    pub setsid: bool,
}

/// `POSIX_SPAWN_SETSID` из `spawn.h`: в `libc` для Apple его нет.
const POSIX_SPAWN_SETSID: libc::c_int = 0x0400;

/// Итог процесса.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl Exit {
    pub fn success(self) -> bool {
        self.code == Some(0)
    }
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.code, self.signal) {
            (Some(code), _) => write!(f, "код {code}"),
            (None, Some(signal)) => write!(f, "сигнал {signal}"),
            (None, None) => f.write_str("итог неизвестен"),
        }
    }
}

/// Итог процесса с выводом.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub exit: Exit,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

fn c_string(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "нулевой байт"))
}

fn check(code: libc::c_int) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

/// Копия дескриптора с номером не меньше 3 и `FD_CLOEXEC`: `dup2` в 0–2 тогда не совпадает с
/// исходным номером, а совпавший `dup2` флаг `FD_CLOEXEC` не снимает.
fn above_stdio(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: `fd` открыт вызывающим; `F_DUPFD_CLOEXEC` возвращает новый дескриптор или -1.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `copy` только что открыт и никому больше не принадлежит.
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

/// Канал, оба конца с `FD_CLOEXEC`: (чтение, запись).
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: массив из двух дескрипторов живёт на время вызова.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe` вернул два открытых дескриптора, которыми больше никто не владеет.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&read, &write] {
        // SAFETY: дескриптор открыт, флаги пишутся без указателей.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read, write))
}

/// Запуск; PID потомка. Ждать его выхода — [`wait`].
pub fn spawn(spec: Spec<'_>) -> io::Result<i32> {
    let null = File::options().read(true).write(true).open("/dev/null")?;
    let mut sources = Vec::with_capacity(3);
    for fd in &spec.stdio {
        let raw = fd.as_ref().map_or(null.as_raw_fd(), AsRawFd::as_raw_fd);
        sources.push(above_stdio(raw)?);
    }
    let program = c_string(spec.program.as_os_str().as_bytes())?;
    let mut argv = vec![program.clone()];
    for arg in spec.args {
        argv.push(c_string(arg.as_bytes())?);
    }
    let env: Vec<CString> = match spec.env {
        Some(env) => env
            .iter()
            .map(|(name, value)| c_string(format!("{name}={value}").as_bytes()))
            .collect::<io::Result<_>>()?,
        None => std::env::vars_os()
            .map(|(name, value)| {
                let mut pair = name.as_bytes().to_vec();
                pair.push(b'=');
                pair.extend_from_slice(value.as_bytes());
                c_string(&pair)
            })
            .collect::<io::Result<_>>()?,
    };
    let mut argv_ptrs: Vec<*mut libc::c_char> =
        argv.iter().map(|arg| arg.as_ptr().cast_mut()).collect();
    argv_ptrs.push(std::ptr::null_mut());
    let mut env_ptrs: Vec<*mut libc::c_char> =
        env.iter().map(|pair| pair.as_ptr().cast_mut()).collect();
    env_ptrs.push(std::ptr::null_mut());

    let mut actions = MaybeUninit::<libc::posix_spawn_file_actions_t>::uninit();
    let mut attr = MaybeUninit::<libc::posix_spawnattr_t>::uninit();
    // SAFETY: структуры инициализируются здесь и уничтожаются ниже при любом исходе; строки,
    // массивы указателей и дескрипторы живут до возврата `posix_spawn`.
    unsafe {
        check(libc::posix_spawn_file_actions_init(actions.as_mut_ptr()))?;
        if let Err(err) = check(libc::posix_spawnattr_init(attr.as_mut_ptr())) {
            libc::posix_spawn_file_actions_destroy(actions.as_mut_ptr());
            return Err(err);
        }
        let result = (|| {
            for (target, source) in sources.iter().enumerate() {
                check(libc::posix_spawn_file_actions_adddup2(
                    actions.as_mut_ptr(),
                    source.as_raw_fd(),
                    target as libc::c_int,
                ))?;
            }
            let mut empty = MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigemptyset(empty.as_mut_ptr());
            let mut defaults = MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigemptyset(defaults.as_mut_ptr());
            libc::sigaddset(defaults.as_mut_ptr(), libc::SIGPIPE);
            check(libc::posix_spawnattr_setsigmask(
                attr.as_mut_ptr(),
                empty.as_ptr(),
            ))?;
            check(libc::posix_spawnattr_setsigdefault(
                attr.as_mut_ptr(),
                defaults.as_ptr(),
            ))?;
            let mut flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT
                | libc::POSIX_SPAWN_SETSIGMASK
                | libc::POSIX_SPAWN_SETSIGDEF;
            if spec.setsid {
                flags |= POSIX_SPAWN_SETSID;
            }
            check(libc::posix_spawnattr_setflags(
                attr.as_mut_ptr(),
                flags as libc::c_short,
            ))?;
            let mut pid: libc::pid_t = 0;
            check(libc::posix_spawn(
                &mut pid,
                program.as_ptr(),
                actions.as_ptr(),
                attr.as_ptr(),
                argv_ptrs.as_ptr(),
                env_ptrs.as_ptr(),
            ))?;
            Ok(pid)
        })();
        libc::posix_spawnattr_destroy(attr.as_mut_ptr());
        libc::posix_spawn_file_actions_destroy(actions.as_mut_ptr());
        result
    }
}

/// Ждёт выхода потомка и прибирает его.
pub fn wait(pid: i32) -> io::Result<Exit> {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `status` живёт на время вызова.
        if unsafe { libc::waitpid(pid, &mut status, 0) } >= 0 {
            break;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    Ok(if libc::WIFEXITED(status) {
        Exit {
            code: Some(libc::WEXITSTATUS(status)),
            signal: None,
        }
    } else if libc::WIFSIGNALED(status) {
        Exit {
            code: None,
            signal: Some(libc::WTERMSIG(status)),
        }
    } else {
        Exit {
            code: None,
            signal: None,
        }
    })
}

fn read_all(fd: OwnedFd) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::from(fd).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Запуск до выхода с выводом; stdin — `/dev/null`, окружение вызывающего.
pub fn output(program: &Path, args: &[String]) -> io::Result<Output> {
    let (out_read, out_write) = pipe()?;
    let (err_read, err_write) = pipe()?;
    let pid = spawn(Spec {
        program,
        args,
        env: None,
        stdio: [None, Some(out_write), Some(err_write)],
        setsid: false,
    })?;
    let stderr = thread::Builder::new()
        .name("spawn-stderr".to_owned())
        .spawn(move || read_all(err_read))?;
    let stdout = read_all(out_read);
    let stderr = stderr
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("поток чтения stderr упал")));
    let exit = wait(pid)?;
    Ok(Output {
        exit,
        stdout: stdout?,
        stderr: stderr?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn output_captures_both_streams_and_exit() {
        let done = output(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "echo out; echo err >&2; exit 3".to_owned()],
        )
        .unwrap();
        assert_eq!(done.exit.code, Some(3));
        assert_eq!(done.stdout, b"out\n");
        assert_eq!(done.stderr, b"err\n");
        let killed = output(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "kill -9 $$".to_owned()],
        )
        .unwrap();
        assert_eq!(killed.exit.signal, Some(libc::SIGKILL));
        assert!(output(Path::new("/nonexistent/x"), &[]).is_err());
    }

    #[test]
    fn child_gets_only_the_three_standard_descriptors() {
        // Дескриптор без FD_CLOEXEC унаследовал бы обычный fork/exec.
        let leaky = File::open("/dev/null").unwrap();
        // SAFETY: дескриптор открыт; флаги снимаются без указателей.
        unsafe { libc::fcntl(leaky.as_fd().as_raw_fd(), libc::F_SETFD, 0) };
        let raw = leaky.as_raw_fd();
        let script = format!("if [ -e /dev/fd/{raw} ]; then echo leak; else echo clean; fi");
        let done = output(Path::new("/bin/sh"), &["-c".to_owned(), script]).unwrap();
        assert_eq!(String::from_utf8_lossy(&done.stdout).trim(), "clean");
        drop(leaky);
    }

    #[test]
    fn explicit_environment_replaces_the_callers() {
        let (read, write) = pipe().unwrap();
        let env = vec![("PSU_ONLY".to_owned(), "1".to_owned())];
        let pid = spawn(Spec {
            program: Path::new("/usr/bin/env"),
            args: &[],
            env: Some(&env),
            stdio: [None, Some(write), None],
            setsid: true,
        })
        .unwrap();
        let text = read_all(read).unwrap();
        assert!(wait(pid).unwrap().success());
        assert_eq!(String::from_utf8_lossy(&text), "PSU_ONLY=1\n");
    }
}
