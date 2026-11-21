//! Миграции схемы `state.db` и резервная копия перед ними (ADR-0018).

use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use shellio_core::UnixMillis;

use crate::open::StoreError;

/// Миграция: SQL, который переводит базу на версию `version`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Migration {
    pub version: u32,
    pub sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: include_str!("../migrations/0001_init.sql"),
}];

const BACKUPS_DIR_MODE: u32 = 0o700;
const BACKUP_FILE_MODE: u32 = 0o600;

/// Упорядоченный список миграций с версиями 1, 2, … подряд.
#[derive(Clone, Debug)]
pub struct Migrator {
    migrations: Vec<Migration>,
}

impl Default for Migrator {
    /// Встроенный список миграций бинаря.
    fn default() -> Self {
        Self {
            migrations: MIGRATIONS.to_vec(),
        }
    }
}

impl Migrator {
    /// Список миграций вместо встроенного.
    ///
    /// # Panics
    ///
    /// Версии не идут подряд с 1.
    #[cfg(feature = "test-support")]
    pub fn new(migrations: Vec<Migration>) -> Self {
        assert!(
            consecutive(&migrations),
            "версии миграций должны идти подряд с 1: {:?}",
            migrations.iter().map(|m| m.version).collect::<Vec<_>>()
        );
        Self { migrations }
    }

    /// Миграции по порядку версий.
    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    /// Последняя известная версия схемы.
    pub fn latest(&self) -> u32 {
        self.migrations
            .last()
            .map_or(0, |migration| migration.version)
    }

    /// Выполнить миграции новее `from`, каждую своей транзакцией вместе с `user_version`.
    /// Сбой откатывает только свою миграцию: база остаётся на последней исправной версии.
    pub(crate) fn migrate(&self, conn: &mut Connection, from: u32) -> Result<(), StoreError> {
        for migration in self.migrations.iter().filter(|m| m.version > from) {
            apply(conn, migration).map_err(|source| StoreError::Migration {
                to: migration.version,
                source,
            })?;
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "test-support"))]
fn consecutive(migrations: &[Migration]) -> bool {
    migrations
        .iter()
        .zip(1..)
        .all(|(migration, expected)| migration.version == expected)
}

fn apply(conn: &mut Connection, migration: &Migration) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(migration.sql)?;
    tx.pragma_update(None, "user_version", migration.version)?;
    tx.commit()
}

/// Резервная копия базы версии `version` в `<dir>/state.db.v<version>.<unix_ms>.bak`.
///
/// Копия — отдельный файл в режиме журнала `DELETE`: открывается без `-wal` и `-shm`
/// и проверяется по `user_version`. Занятое имя не перезаписывается. Неудачная копия
/// удаляется.
pub(crate) fn backup(
    conn: &Connection,
    version: u32,
    dir: &Path,
    now: UnixMillis,
) -> Result<PathBuf, StoreError> {
    let path = dir.join(format!("state.db.v{version}.{}.bak", now.0));
    let fail = |source| StoreError::Backup {
        path: path.clone(),
        source,
    };
    ensure_backups_dir(dir).map_err(|error| fail(BackupError::Dir(error)))?;
    reserve(&path).map_err(|error| {
        fail(if error.kind() == io::ErrorKind::AlreadyExists {
            BackupError::Exists
        } else {
            BackupError::File(error)
        })
    })?;
    if let Err(error) = copy_and_verify(conn, &path, version) {
        // Недоделанная копия не должна выглядеть исправной; уборка — по возможности.
        let _ = fs::remove_file(&path);
        return Err(fail(error));
    }
    Ok(path)
}

fn ensure_backups_dir(dir: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(BACKUPS_DIR_MODE).create(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if fs::metadata(dir)?.is_dir() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("путь {} занят не каталогом", dir.display()),
                ))
            }
        }
        Err(error) => Err(error),
    }
}

fn reserve(path: &Path) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(BACKUP_FILE_MODE)
        .open(path)
        .map(drop)
}

fn copy_and_verify(conn: &Connection, path: &Path, version: u32) -> Result<(), BackupError> {
    let mut copy = Connection::open(path)?;
    copy.pragma_update(None, "synchronous", "FULL")?;
    // В монопольном режиме SQLite держит индекс WAL в памяти и не заводит `-shm` рядом с копией.
    copy.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    {
        let backup = Backup::new(conn, &mut copy)?;
        if !matches!(backup.step(-1)?, StepResult::Done) {
            return Err(BackupError::Incomplete);
        }
    }
    // Копия наследует флаг WAL источника; режим DELETE делает её самодостаточным файлом.
    let mode: String =
        copy.pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("delete") {
        return Err(BackupError::JournalMode(mode));
    }
    copy.close().map_err(|(_, error)| error)?;

    let check = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let found: i64 = check.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found == i64::from(version) {
        Ok(())
    } else {
        Err(BackupError::Version {
            expected: version,
            found,
        })
    }
}

/// Причина неудачи резервной копии. Миграция после неё не начинается.
#[derive(Debug)]
pub enum BackupError {
    /// Каталог резервных копий не создан или путь занят не каталогом.
    Dir(io::Error),
    /// Копия с таким именем уже есть и не перезаписывается.
    Exists,
    /// Файл копии не создан.
    File(io::Error),
    /// Ошибка SQLite при копировании или проверке.
    Sqlite(rusqlite::Error),
    /// Backup API не скопировал базу за один шаг.
    Incomplete,
    /// Копия не перешла в режим журнала `DELETE`.
    JournalMode(String),
    /// `user_version` копии не совпала с версией источника.
    Version { expected: u32, found: i64 },
}

impl From<rusqlite::Error> for BackupError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl fmt::Display for BackupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dir(error) => write!(f, "каталог резервных копий не создан: {error}"),
            Self::Exists => f.write_str("копия с таким именем уже есть"),
            Self::File(error) => write!(f, "файл копии не создан: {error}"),
            Self::Sqlite(error) => write!(f, "ошибка SQLite: {error}"),
            Self::Incomplete => f.write_str("копирование не завершилось за один шаг"),
            Self::JournalMode(mode) => write!(f, "копия осталась в режиме журнала {mode}"),
            Self::Version { expected, found } => {
                write!(f, "user_version копии {found}, ожидалась {expected}")
            }
        }
    }
}

impl std::error::Error for BackupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Dir(error) | Self::File(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::Exists | Self::Incomplete | Self::JournalMode(_) | Self::Version { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_versions_are_consecutive_from_one() {
        let migrator = Migrator::default();
        assert!(consecutive(migrator.migrations()));
        assert_eq!(migrator.latest(), 1);
    }

    #[test]
    #[should_panic(expected = "подряд с 1")]
    fn gap_in_versions_is_refused() {
        let mut migrations = MIGRATIONS.to_vec();
        migrations.push(Migration {
            version: 3,
            sql: "",
        });
        Migrator::new(migrations);
    }

    #[test]
    fn empty_list_has_version_zero() {
        assert_eq!(Migrator::new(Vec::new()).latest(), 0);
    }
}
