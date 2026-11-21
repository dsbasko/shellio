//! Открытие `state.db`: проверка файла, резервная копия, прагмы и миграции (ADR-0018).

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode};
use shellio_core::Clock;

use crate::migrate::{self, BackupError, Migrator};
use crate::profile::{Profile, ProfileError};

const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// Открытая база состояния на последней известной версии схемы.
#[derive(Debug)]
pub struct Database {
    conn: Connection,
    path: PathBuf,
    schema_version: u32,
}

impl Database {
    /// Открыть `state.db` профиля со встроенными миграциями.
    ///
    /// Порядок: каталог состояния с `0700`; чтение `user_version` и схемы до любой записи
    /// (файл не база — [`StoreError::Corrupt`]); версия новее бинаря —
    /// [`StoreError::NewerSchema`]; резервная копия, если есть что мигрировать в непустой
    /// схеме; прагмы; миграции. Время из `clock` даёт имя копии.
    pub fn open(profile: &Profile, clock: &dyn Clock) -> Result<Self, StoreError> {
        Self::open_inner(profile, clock, &Migrator::default())
    }

    /// Открыть `state.db` профиля с заданным списком миграций.
    #[cfg(feature = "test-support")]
    pub fn open_with(
        profile: &Profile,
        clock: &dyn Clock,
        migrator: &Migrator,
    ) -> Result<Self, StoreError> {
        Self::open_inner(profile, clock, migrator)
    }

    fn open_inner(
        profile: &Profile,
        clock: &dyn Clock,
        migrator: &Migrator,
    ) -> Result<Self, StoreError> {
        profile.ensure_state_dir()?;
        let path = profile.state_db();
        let sqlite = |source| StoreError::Sqlite {
            path: path.clone(),
            source,
        };

        let mut conn = Connection::open(&path).map_err(sqlite)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(sqlite)?;
        let (found, empty) = inspect(&conn).map_err(|source| classify(&path, source))?;
        let found = u32::try_from(found).map_err(|_| StoreError::Corrupt {
            path: path.clone(),
            detail: format!("отрицательная user_version {found}"),
        })?;
        let supported = migrator.latest();
        if found > supported {
            return Err(StoreError::NewerSchema { found, supported });
        }
        if found < supported && !empty {
            migrate::backup(&conn, found, &profile.backups_dir(), clock.now())?;
        }

        let mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .map_err(sqlite)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::JournalMode { path, found: mode });
        }
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(sqlite)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(sqlite)?;
        migrator.migrate(&mut conn, found)?;

        Ok(Self {
            conn,
            path,
            schema_version: supported,
        })
    }

    /// Путь `state.db`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Версия схемы открытой базы.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Соединение с базой для проверок в тестах.
    #[cfg(feature = "test-support")]
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// `user_version` и признак пустой схемы. Первое чтение файла: здесь SQLite сообщает,
/// что файл не база или повреждён.
fn inspect(conn: &Connection) -> rusqlite::Result<(i64, bool)> {
    let version = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let objects: i64 =
        conn.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
    Ok((version, objects == 0))
}

fn classify(path: &Path, source: rusqlite::Error) -> StoreError {
    match source.sqlite_error_code() {
        Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) => StoreError::Corrupt {
            path: path.to_path_buf(),
            detail: source.to_string(),
        },
        _ => StoreError::Sqlite {
            path: path.to_path_buf(),
            source,
        },
    }
}

/// Ошибка открытия хранилища.
#[derive(Debug)]
pub enum StoreError {
    /// Каталог состояния не создан.
    Profile(ProfileError),
    /// Файл не база SQLite или повреждён. Файл не изменён.
    Corrupt { path: PathBuf, detail: String },
    /// Схема новее, чем знает бинарь. Ни одной записи не сделано.
    NewerSchema { found: u32, supported: u32 },
    /// Резервная копия перед миграцией не сделана, миграция не начата.
    Backup { path: PathBuf, source: BackupError },
    /// Миграция на версию `to` откатилась, база на предыдущей версии.
    Migration { to: u32, source: rusqlite::Error },
    /// База не перешла в режим WAL.
    JournalMode { path: PathBuf, found: String },
    /// Прочая ошибка SQLite.
    Sqlite {
        path: PathBuf,
        source: rusqlite::Error,
    },
}

impl From<ProfileError> for StoreError {
    fn from(error: ProfileError) -> Self {
        Self::Profile(error)
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Profile(error) => error.fmt(f),
            Self::Corrupt { path, detail } => {
                write!(
                    f,
                    "{} не база SQLite или повреждён: {detail}",
                    path.display()
                )
            }
            Self::NewerSchema { found, supported } => write!(
                f,
                "схема state.db версии {found} новее поддерживаемой {supported}"
            ),
            Self::Backup { path, source } => {
                write!(f, "резервная копия {} не сделана: {source}", path.display())
            }
            Self::Migration { to, source } => {
                write!(f, "миграция на версию {to} не прошла: {source}")
            }
            Self::JournalMode { path, found } => write!(
                f,
                "{} не перешла в режим WAL, режим журнала {found}",
                path.display()
            ),
            Self::Sqlite { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Profile(error) => Some(error),
            Self::Backup { source, .. } => Some(source),
            Self::Migration { source, .. } | Self::Sqlite { source, .. } => Some(source),
            Self::Corrupt { .. } | Self::NewerSchema { .. } | Self::JournalMode { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_error_passes_through_load_error() {
        const fn send_sync<T: Send + Sync + 'static>() {}
        send_sync::<StoreError>();
        let error: shellio_core::LoadError = Box::new(StoreError::NewerSchema {
            found: 2,
            supported: 1,
        });
        assert_eq!(
            error.to_string(),
            "схема state.db версии 2 новее поддерживаемой 1"
        );
    }
}
