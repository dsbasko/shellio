//! Публикация сокета Rust-хоста (план P0.4, «Rust-хост», «Окно публикации»): `bind` и `listen` во
//! временном имени `.<имя>.<pid>`, затем жёсткая ссылка на имя сессии. Проигравший получает
//! «занято» и чужой сокет не трогает. Неатомарного запасного пути нет: если временное или
//! конечное имя не помещается в `sun_path`, публикация отказывает явно.

use std::fmt;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use crate::backend::SocketEntries;

/// `sockaddr_un.sun_path` на macOS — 104 байта вместе с нулём.
pub const MAX_SOCKET_PATH: usize = 103;
const BACKLOG: libc::c_int = 64;

/// Точка окна публикации, где проба может остановить хост.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    /// После `bind` временного имени, до `listen`.
    Bound,
    /// После `listen`, до ссылки на имя сессии.
    Listening,
}

impl Point {
    pub const ALL: [Point; 2] = [Point::Bound, Point::Listening];

    pub fn name(self) -> &'static str {
        match self {
            Point::Bound => "bound",
            Point::Listening => "listening",
        }
    }

    pub fn from_name(name: &str) -> Option<Point> {
        Point::ALL.into_iter().find(|point| point.name() == name)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PublishError {
    /// Путь не помещается в `sun_path`: явный отказ вместо неатомарного пути.
    TooLong {
        path: PathBuf,
        len: usize,
    },
    /// Имя сессии уже занято сокетом другого хоста.
    Busy {
        path: PathBuf,
    },
    Io(String),
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishError::TooLong { path, len } => write!(
                f,
                "путь сокета {} — {len} байт, больше {MAX_SOCKET_PATH}: публикация невозможна",
                path.display()
            ),
            PublishError::Busy { path } => write!(f, "имя сессии занято: {}", path.display()),
            PublishError::Io(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for PublishError {}

pub fn temp_name(session: &str, pid: i32) -> String {
    format!(".{session}.{pid}")
}

/// Имя сессии из имени файла каталога: временные имена начинаются с точки.
pub fn is_temp_name(name: &str) -> bool {
    name.starts_with('.')
}

pub fn check_path(path: &Path) -> Result<(), PublishError> {
    let len = path.as_os_str().as_bytes().len();
    if len > MAX_SOCKET_PATH {
        return Err(PublishError::TooLong {
            path: path.to_path_buf(),
            len,
        });
    }
    Ok(())
}

/// Опубликованный сокет: слушатель и узел файла, по которому хост убирает только свою ссылку.
#[derive(Debug)]
pub struct Published {
    pub listener: UnixListener,
    pub path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Published {
    /// Удаляет имя сессии, если под ним всё ещё этот сокет.
    pub fn unpublish(&self) -> io::Result<bool> {
        match fs::symlink_metadata(&self.path) {
            Ok(meta) if meta.dev() == self.dev && meta.ino() == self.ino => {
                fs::remove_file(&self.path)?;
                Ok(true)
            }
            Ok(_) => Ok(false),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// Публикует сокет сессии `session` в `dir`. `pause` вызывается в каждой точке окна.
pub fn publish(
    dir: &Path,
    session: &str,
    pid: i32,
    pause: &mut dyn FnMut(Point),
) -> Result<Published, PublishError> {
    let final_path = dir.join(session);
    let temp = dir.join(temp_name(session, pid));
    check_path(&final_path)?;
    check_path(&temp)?;
    let io_err = |what: &str, err: io::Error| PublishError::Io(format!("{what}: {err}"));
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(io_err(&format!("{}", temp.display()), err)),
    }
    let socket = unix_socket().map_err(|err| io_err("socket", err))?;
    bind(&socket, &temp).map_err(|err| io_err(&format!("bind {}", temp.display()), err))?;
    pause(Point::Bound);
    // SAFETY: дескриптор открыт и принадлежит `socket`.
    if unsafe { libc::listen(socket.as_raw_fd(), BACKLOG) } != 0 {
        let err = io::Error::last_os_error();
        let _ = fs::remove_file(&temp);
        return Err(io_err("listen", err));
    }
    pause(Point::Listening);
    let linked = fs::hard_link(&temp, &final_path);
    let meta = fs::symlink_metadata(&temp);
    let _ = fs::remove_file(&temp);
    match linked {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return Err(PublishError::Busy { path: final_path });
        }
        Err(err) => return Err(io_err(&format!("link {}", final_path.display()), err)),
    }
    let meta = meta.map_err(|err| io_err("stat временного имени", err))?;
    Ok(Published {
        listener: UnixListener::from(socket),
        path: final_path,
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

fn unix_socket() -> io::Result<OwnedFd> {
    // SAFETY: вызов без указателей; результат проверяется.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `socket` вернул открытый дескриптор, которым больше никто не владеет.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: дескриптор открыт, флаги читаются и пишутся без указателей.
    let status = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(socket)
}

fn bind(socket: &OwnedFd, path: &Path) -> io::Result<()> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: нулевая структура `sockaddr_un` допустима.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "путь длиннее sun_path",
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len() + 1) as libc::socklen_t;
    addr.sun_len = len as u8;
    // SAFETY: адрес на стеке жив на время вызова, длина не больше структуры.
    let status = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&raw const addr).cast::<libc::sockaddr>(),
            len,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Сокеты каталога хостов без обращения к ним: опубликованные имена и временные.
pub fn entries(dir: &Path) -> io::Result<SocketEntries> {
    let mut found = SocketEntries::default();
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(found),
        Err(err) => return Err(err),
    };
    for entry in listing {
        let entry = entry?;
        if !entry.file_type()?.is_socket() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_temp_name(&name) {
            found.staged.push(name);
        } else {
            found.sessions.push(name);
        }
    }
    found.sessions.sort();
    found.staged.sort();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// Короткий каталог теста: `TMPDIR` macOS длинный, но имя сессии в нём помещается.
    fn dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pss-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn published_socket_accepts_through_the_session_name() {
        let dir = dir("pub");
        let mut points = Vec::new();
        let published = publish(&dir, "s1", 4242, &mut |point| {
            points.push(point);
        })
        .unwrap();
        assert_eq!(points, Point::ALL);
        assert_eq!(
            entries(&dir).unwrap(),
            SocketEntries {
                sessions: vec!["s1".to_owned()],
                staged: Vec::new(),
            }
        );
        let _client = UnixStream::connect(dir.join("s1")).unwrap();
        let (_accepted, _) = published.listener.accept().unwrap();
        assert!(published.unpublish().unwrap());
        assert!(!dir.join("s1").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn window_shows_only_the_temporary_name() {
        let dir = dir("win");
        let mut seen = Vec::new();
        let published = publish(&dir, "s1", 77, &mut |point| {
            let found = entries(&dir).unwrap();
            let connect = UnixStream::connect(dir.join("s1")).is_ok();
            seen.push((point, found, connect));
        })
        .unwrap();
        for (point, found, connect) in seen {
            assert_eq!(found.sessions, Vec::<String>::new(), "{point:?}");
            assert_eq!(found.staged, vec![temp_name("s1", 77)], "{point:?}");
            assert!(!connect, "{point:?}");
        }
        drop(published);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn busy_name_is_refused_and_the_winner_keeps_its_socket() {
        let dir = dir("busy");
        let winner = publish(&dir, "s1", 1, &mut |_| {}).unwrap();
        let loser = publish(&dir, "s1", 2, &mut |_| {});
        assert_eq!(
            loser.unwrap_err(),
            PublishError::Busy {
                path: dir.join("s1")
            }
        );
        assert_eq!(entries(&dir).unwrap().staged, Vec::<String>::new());
        let _client = UnixStream::connect(dir.join("s1")).unwrap();
        winner.listener.accept().unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn foreign_socket_under_the_name_is_not_unpublished() {
        let dir = dir("own");
        let first = publish(&dir, "s1", 1, &mut |_| {}).unwrap();
        fs::remove_file(dir.join("s1")).unwrap();
        let second = publish(&dir, "s1", 2, &mut |_| {}).unwrap();
        assert!(!first.unpublish().unwrap());
        assert!(dir.join("s1").exists());
        assert!(second.unpublish().unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn too_long_path_is_refused_without_a_fallback() {
        let dir = dir("long");
        // Конечное имя помещается, временное `.<имя>.<pid>` — нет.
        let base = dir.as_os_str().as_bytes().len() + 1;
        let name = "s".repeat(MAX_SOCKET_PATH - base);
        assert!(check_path(&dir.join(&name)).is_ok());
        let refused = publish(&dir, &name, 12345, &mut |_| panic!("окна быть не должно"));
        assert!(
            matches!(refused, Err(PublishError::TooLong { ref path, .. }) if path == &dir.join(temp_name(&name, 12345))),
            "{refused:?}"
        );
        assert_eq!(entries(&dir).unwrap(), SocketEntries::default());
        let longer = "s".repeat(MAX_SOCKET_PATH + 1 - base);
        assert!(matches!(
            publish(&dir, &longer, 1, &mut |_| {}),
            Err(PublishError::TooLong { .. })
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn point_names_round_trip() {
        for point in Point::ALL {
            assert_eq!(Point::from_name(point.name()), Some(point));
        }
        assert!(is_temp_name(&temp_name("s1", 3)));
        assert!(!is_temp_name("s1"));
    }
}
