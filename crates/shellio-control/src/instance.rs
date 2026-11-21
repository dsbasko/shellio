//! Выбор владельца профиля: блокировка единственного экземпляра и публикация сокета (ADR-0006).
//!
//! Владелец держит неблокирующий эксклюзивный `flock` на `<state>/shellio.lock` и только под
//! ним публикует `<state>/shellio.sock`. Клиент пробует разделяемый `flock` без создания файла
//! и сразу его отпускает. Все дескрипторы открываются с `CLOEXEC`: std делает это сама.

use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions, TryLockError};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use shellio_store::Profile;
use tokio::net::UnixStream;

/// Имя файла блокировки в каталоге состояния. Файл постоянный и не удаляется.
pub const LOCK_FILE: &str = "shellio.lock";

/// Имя опубликованного сокета в каталоге состояния.
pub const SOCKET_FILE: &str = "shellio.sock";

/// Начало временного имени сокета: за ним следует PID владельца.
const TEMP_SOCKET_PREFIX: &str = ".shellio.sock.";

/// Наибольшая длина пути сокета в байтах: `sun_path` Darwin — 104 байта с завершающим нулём.
pub const MAX_SOCKET_PATH: usize = 103;

/// Права файла блокировки и сокета.
const PRIVATE_MODE: u32 = 0o600;

/// Путь файла блокировки профиля.
pub fn lock_path(profile: &Profile) -> PathBuf {
    profile.state_dir().join(LOCK_FILE)
}

/// Путь опубликованного сокета профиля.
pub fn socket_path(profile: &Profile) -> PathBuf {
    profile.state_dir().join(SOCKET_FILE)
}

/// Проверить длину пути сокета: не длиннее [`MAX_SOCKET_PATH`] байт.
pub fn check_socket_path(path: &Path) -> Result<(), InstanceError> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(InstanceError::SocketPathTooLong {
            path: path.to_path_buf(),
            len,
        });
    }
    Ok(())
}

/// Конечное и временное имя сокета владельца с данным PID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocketPaths {
    published: PathBuf,
    temporary: PathBuf,
}

impl SocketPaths {
    /// Имена сокета в каталоге состояния профиля для владельца с PID `pid`.
    pub fn new(profile: &Profile, pid: u32) -> Self {
        Self::in_dir(profile.state_dir(), pid)
    }

    fn in_dir(state_dir: &Path, pid: u32) -> Self {
        Self {
            published: state_dir.join(SOCKET_FILE),
            temporary: state_dir.join(format!("{TEMP_SOCKET_PREFIX}{pid}")),
        }
    }

    /// Конечное имя `<state>/shellio.sock`.
    pub fn published(&self) -> &Path {
        &self.published
    }

    /// Временное имя `<state>/.shellio.sock.<pid>`.
    pub fn temporary(&self) -> &Path {
        &self.temporary
    }

    /// Проверить длины обоих имён: сначала конечного, затем временного.
    pub fn check(&self) -> Result<(), InstanceError> {
        check_socket_path(&self.published)?;
        check_socket_path(&self.temporary)
    }
}

/// Узел на месте имени сокета.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// Обычный файл.
    File,
    /// Символическая ссылка.
    Symlink,
    /// Каталог.
    Directory,
    /// Другой узел: FIFO, устройство.
    Other,
}

impl NodeKind {
    fn of(file_type: fs::FileType) -> Self {
        if file_type.is_symlink() {
            Self::Symlink
        } else if file_type.is_file() {
            Self::File
        } else if file_type.is_dir() {
            Self::Directory
        } else {
            Self::Other
        }
    }
}

impl fmt::Display for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::File => "обычный файл",
            Self::Symlink => "символическая ссылка",
            Self::Directory => "каталог",
            Self::Other => "узел, не являющийся сокетом",
        })
    }
}

/// Ошибка выбора владельца или публикации сокета.
#[derive(Debug)]
pub enum InstanceError {
    /// Системный вызов над путём не удался.
    Io {
        /// Что не удалось сделать.
        op: &'static str,
        /// Путь.
        path: PathBuf,
        /// Ошибка ввода-вывода.
        source: io::Error,
    },
    /// Путь сокета длиннее [`MAX_SOCKET_PATH`] байт. Другой путь не подбирается.
    SocketPathTooLong {
        /// Путь сокета.
        path: PathBuf,
        /// Длина в байтах.
        len: usize,
    },
    /// На месте имени сокета лежит не сокет. Удаляются только сокеты.
    Occupied {
        /// Занятое имя.
        path: PathBuf,
        /// Что лежит на его месте.
        kind: NodeKind,
    },
}

impl InstanceError {
    fn io(op: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io {
            op,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for InstanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { op, path, source } => {
                write!(f, "не удалось {op} {}: {source}", path.display())
            }
            Self::SocketPathTooLong { path, len } => write!(
                f,
                "путь сокета {} занимает {len} байт, допустимо не больше {MAX_SOCKET_PATH}",
                path.display()
            ),
            Self::Occupied { path, kind } => write!(
                f,
                "на месте сокета {} лежит {kind}: удаляются только сокеты",
                path.display()
            ),
        }
    }
}

impl Error for InstanceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::SocketPathTooLong { .. } | Self::Occupied { .. } => None,
        }
    }
}

/// Сокет, опубликованный этим владельцем: удаляется только тот же узел.
#[derive(Debug)]
struct Published {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

/// Роль владельца профиля: взятая блокировка и, после публикации, свой сокет.
///
/// [`Ownership::release`] и `Drop` удаляют свой сокет, затем отпускают блокировку. Файл
/// блокировки остаётся на месте.
#[derive(Debug)]
pub struct Ownership {
    lock: File,
    lock_path: PathBuf,
    state_dir: PathBuf,
    locked: bool,
    published: Option<Published>,
}

impl Ownership {
    /// Взять блокировку профиля без ожидания. `None` — блокировку держит другой.
    ///
    /// Каталог состояния должен существовать. Файл блокировки создаётся с правами `0600`.
    pub fn try_acquire(profile: &Profile) -> Result<Option<Self>, InstanceError> {
        let lock_path = lock_path(profile);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(PRIVATE_MODE)
            .open(&lock_path)
            .map_err(InstanceError::io("открыть", &lock_path))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(source)) => {
                return Err(InstanceError::io("взять блокировку", &lock_path)(
                    source,
                ));
            }
        }
        let ownership = Self {
            lock,
            lock_path,
            state_dir: profile.state_dir().to_path_buf(),
            locked: true,
            published: None,
        };
        ownership.restrict_lock()?;
        Ok(Some(ownership))
    }

    /// Права `0600` у файла блокировки, если umask их сузил или файл остался от прежней
    /// версии с другими.
    fn restrict_lock(&self) -> Result<(), InstanceError> {
        let mode = self
            .lock
            .metadata()
            .map_err(InstanceError::io("прочитать атрибуты", &self.lock_path))?
            .permissions()
            .mode();
        if mode & 0o777 != PRIVATE_MODE {
            self.lock
                .set_permissions(Permissions::from_mode(PRIVATE_MODE))
                .map_err(InstanceError::io("задать права", &self.lock_path))?;
        }
        Ok(())
    }

    /// Опубликовать сокет: проверить длины имён, удалить оставшиеся сокеты прежних
    /// владельцев (`shellio.sock` и `.shellio.sock.*`), привязать сокет к временному имени,
    /// дать ему права `0600` и переименовать в `shellio.sock`.
    ///
    /// Не сокет под любым из этих имён — ошибка до удаления чего-либо. Повторная публикация
    /// заменяет прежнюю. Слушатель неблокирующий и готов к `tokio::net::UnixListener::from_std`.
    pub fn publish(&mut self) -> Result<UnixListener, InstanceError> {
        let paths = SocketPaths::in_dir(&self.state_dir, std::process::id());
        paths.check()?;
        for path in self.leftover_sockets()? {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(InstanceError::io("удалить", &path)(error)),
            }
        }
        self.published = None;

        let temporary = paths.temporary();
        let listener = UnixListener::bind(temporary)
            .map_err(InstanceError::io("привязать сокет к", temporary))?;
        match Self::finish_publish(&listener, &paths) {
            Ok(published) => {
                self.published = Some(published);
                Ok(listener)
            }
            Err(error) => {
                let _ = fs::remove_file(temporary);
                Err(error)
            }
        }
    }

    fn finish_publish(
        listener: &UnixListener,
        paths: &SocketPaths,
    ) -> Result<Published, InstanceError> {
        let temporary = paths.temporary();
        fs::set_permissions(temporary, Permissions::from_mode(PRIVATE_MODE))
            .map_err(InstanceError::io("задать права", temporary))?;
        let metadata = fs::symlink_metadata(temporary)
            .map_err(InstanceError::io("прочитать атрибуты", temporary))?;
        listener.set_nonblocking(true).map_err(InstanceError::io(
            "перевести в неблокирующий режим",
            temporary,
        ))?;
        fs::rename(temporary, paths.published())
            .map_err(InstanceError::io("переименовать", temporary))?;
        Ok(Published {
            path: paths.published().to_path_buf(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }

    /// Оставшиеся сокеты под именами `shellio.sock` и `.shellio.sock.*`. Не сокет под таким
    /// именем — ошибка.
    fn leftover_sockets(&self) -> Result<Vec<PathBuf>, InstanceError> {
        let entries = fs::read_dir(&self.state_dir)
            .map_err(InstanceError::io("прочитать каталог", &self.state_dir))?;
        let mut sockets = Vec::new();
        for entry in entries {
            let entry = entry.map_err(InstanceError::io("прочитать каталог", &self.state_dir))?;
            let name = entry.file_name();
            let name = name.as_bytes();
            if name != SOCKET_FILE.as_bytes() && !name.starts_with(TEMP_SOCKET_PREFIX.as_bytes()) {
                continue;
            }
            let path = entry.path();
            let file_type = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata.file_type(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(InstanceError::io("прочитать атрибуты", &path)(
                        error,
                    ));
                }
            };
            if !file_type.is_socket() {
                return Err(InstanceError::Occupied {
                    path,
                    kind: NodeKind::of(file_type),
                });
            }
            sockets.push(path);
        }
        Ok(sockets)
    }

    /// Удалить свой сокет и отпустить блокировку. Блокировка отпускается и при ошибке
    /// удаления; возвращается первая ошибка.
    pub fn release(mut self) -> Result<(), InstanceError> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<(), InstanceError> {
        let removed = self.remove_published();
        let unlocked = if self.locked {
            self.locked = false;
            self.lock
                .unlock()
                .map_err(InstanceError::io("снять блокировку", &self.lock_path))
        } else {
            Ok(())
        };
        removed.and(unlocked)
    }

    /// Удалить опубликованный сокет, если на его месте тот же узел.
    fn remove_published(&mut self) -> Result<(), InstanceError> {
        let Some(published) = self.published.take() else {
            return Ok(());
        };
        let path = &published.path;
        match fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_type().is_socket()
                    && metadata.dev() == published.dev
                    && metadata.ino() == published.ino =>
            {
                match fs::remove_file(path) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(InstanceError::io("удалить", path)(error)),
                }
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(InstanceError::io("прочитать атрибуты", path)(
                error,
            )),
        }
    }
}

/// Дескриптор файла блокировки: для проверки, что он не наследуется.
impl AsFd for Ownership {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.lock.as_fd()
    }
}

impl Drop for Ownership {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Исход пробы владельца клиентом.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Файла блокировки нет или блокировка свободна.
    NotRunning,
    /// Блокировку держит владелец или претендент.
    Held,
}

/// Проба владельца: открыть файл блокировки без создания, попробовать разделяемый `flock`
/// и сразу его отпустить. Ничего не создаёт и не удаляет.
pub fn probe_owner(profile: &Profile) -> Result<Probe, InstanceError> {
    let path = lock_path(profile);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Probe::NotRunning),
        Err(error) => return Err(InstanceError::io("открыть", &path)(error)),
    };
    match file.try_lock_shared() {
        Ok(()) => {
            file.unlock()
                .map_err(InstanceError::io("снять блокировку", &path))?;
            Ok(Probe::NotRunning)
        }
        Err(TryLockError::WouldBlock) => Ok(Probe::Held),
        Err(TryLockError::Error(error)) => Err(InstanceError::io("взять блокировку", &path)(
            error,
        )),
    }
}

/// UID совпадает с UID процесса.
pub fn is_own_uid(uid: u32) -> bool {
    uid == rustix::process::getuid().as_raw()
}

/// Собеседник отвергнут: соединение закрыто без ответа.
#[derive(Debug)]
pub enum PeerRefused {
    /// Собеседник работает под другим пользователем.
    Foreign {
        /// UID собеседника.
        uid: u32,
    },
    /// UID собеседника не прочитан.
    Unknown(io::Error),
}

impl fmt::Display for PeerRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Foreign { uid } => write!(f, "собеседник под чужим UID {uid}"),
            Self::Unknown(error) => write!(f, "UID собеседника не прочитан: {error}"),
        }
    }
}

impl Error for PeerRefused {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unknown(error) => Some(error),
            Self::Foreign { .. } => None,
        }
    }
}

/// Допустить принятое соединение, только если собеседник — тот же пользователь. Чужое
/// соединение закрывается без ответа.
pub fn admit_peer(stream: UnixStream) -> Result<UnixStream, PeerRefused> {
    let uid = stream.peer_cred().map_err(PeerRefused::Unknown)?.uid();
    if is_own_uid(uid) {
        Ok(stream)
    } else {
        Err(PeerRefused::Foreign { uid })
    }
}
