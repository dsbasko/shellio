//! Открытие `state.db` и миграции на временных каталогах: последняя схема v2, миграция образца
//! v1, резервная копия, откат, отказ на новой схеме и на повреждённом файле.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};
use shellio_core::UnixMillis;
use shellio_core::test_support::ManualClock;
use shellio_store::{BackupError, Database, Migration, Migrator, Profile, StoreError};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);
const SCHEMA_V1: &str = include_str!("schema_v1.sql");
const SCHEMA_V2: &str = include_str!("schema_v2.sql");
const FIXTURE_V1: &str = include_str!("fixtures/v1.sql");
/// Последняя версия схемы этой сборки.
const LATEST: u32 = 2;

/// Синтетическая v3 после последней: настоящее изменение схемы и данных.
const V3: Migration = Migration {
    version: 3,
    sql: "CREATE TABLE extra (x INTEGER NOT NULL) STRICT;\n\
          ALTER TABLE workspaces ADD COLUMN color TEXT;",
};

/// Синтетическая v3, которая падает после настоящего изменения схемы.
const V3_BROKEN: Migration = Migration {
    version: 3,
    sql: "CREATE TABLE extra (x INTEGER NOT NULL) STRICT;\n\
          INSERT INTO missing_table VALUES (1);",
};

const WORKSPACE_ID: [u8; 16] = [
    0x01, 0x9a, 0x8f, 0x2e, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0xa0, 0x01,
];

struct Fixture {
    _dir: TempDir,
    profile: Profile,
    clock: ManualClock,
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let dir = TempDir::new()?;
        let profile = Profile::isolated(dir.path().join("state"));
        Ok(Self {
            _dir: dir,
            profile,
            clock: ManualClock::new(NOW),
        })
    }

    fn db(&self) -> PathBuf {
        self.profile.state_db()
    }

    fn open(&self) -> Result<Database, StoreError> {
        Database::open(&self.profile, &self.clock)
    }

    fn open_with(&self, migrations: &[Migration]) -> Result<Database, StoreError> {
        let mut list = Migrator::default().migrations().to_vec();
        list.extend_from_slice(migrations);
        Database::open_with(&self.profile, &self.clock, &Migrator::new(list))
    }

    /// База последней версии с одним воркспейсом, закрытая.
    fn latest_with_workspace(&self) -> TestResult {
        let db = self.open()?;
        db.connection().execute(
            "INSERT INTO workspaces (id, title, position) VALUES (?1, 'Работа', 0)",
            [WORKSPACE_ID.as_slice()],
        )?;
        Ok(())
    }

    /// Отдельное соединение с `state.db` в обход хранилища.
    fn raw(&self) -> TestResult<Connection> {
        Ok(Connection::open(self.db())?)
    }

    fn backups(&self) -> TestResult<Vec<String>> {
        list_dir(&self.profile.backups_dir())
    }

    fn backup_name(version: u32) -> String {
        format!("state.db.v{version}.{}.bak", NOW.0)
    }
}

fn list_dir(dir: &Path) -> TestResult<Vec<String>> {
    let mut names = fs::read_dir(dir)?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<TestResult<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

/// Схема без служебных объектов SQLite в виде файла SQL.
fn schema(conn: &Connection) -> TestResult<String> {
    let mut stmt = conn.prepare(
        "SELECT sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
         ORDER BY type, name",
    )?;
    let statements = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|sql| Ok(format!("{};\n", sql?)))
        .collect::<TestResult<Vec<_>>>()?;
    Ok(statements.join("\n"))
}

fn user_version(conn: &Connection) -> TestResult<i64> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

fn table_exists(conn: &Connection, name: &str) -> TestResult<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(count == 1)
}

fn workspace_titles(conn: &Connection) -> TestResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT title FROM workspaces ORDER BY position, id")?;
    let titles = stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(titles)
}

fn sqlite_version(conn: &Connection) -> TestResult<(u32, u32, u32)> {
    let text: String = conn.query_row("SELECT sqlite_version()", [], |row| row.get(0))?;
    let mut parts = text.split('.').map(str::parse::<u32>);
    let mut next = || -> TestResult<u32> { Ok(parts.next().transpose()?.unwrap_or(0)) };
    Ok((next()?, next()?, next()?))
}

#[test]
fn fresh_database_matches_latest_schema() -> TestResult {
    for precreated in [false, true] {
        let fixture = Fixture::new()?;
        if precreated {
            fs::create_dir_all(fixture.profile.state_dir())?;
            fs::write(fixture.db(), b"")?;
        }
        let db = fixture.open()?;
        let conn = db.connection();

        let version = sqlite_version(conn)?;
        assert!(
            version >= (3, 37, 0),
            "таблицы STRICT требуют SQLite 3.37 или новее, системная libsqlite3 — {version:?}"
        );
        assert_eq!(
            schema(conn)?,
            SCHEMA_V2,
            "схема после открытия расходится с эталоном"
        );
        assert_eq!(user_version(conn)?, i64::from(LATEST));
        assert_eq!(db.schema_version(), LATEST);
        assert_eq!(db.path(), fixture.db());

        let journal: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 = conn.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        let foreign_keys: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
        let busy_timeout: i64 = conn.pragma_query_value(None, "busy_timeout", |row| row.get(0))?;
        assert_eq!(journal, "wal");
        assert_eq!(synchronous, 2, "synchronous=FULL");
        assert_eq!(foreign_keys, 1);
        assert_eq!(busy_timeout, 5000);

        let revision: i64 =
            conn.query_row("SELECT value FROM meta WHERE key = 'revision'", [], |row| {
                row.get(0)
            })?;
        assert_eq!(revision, 0);
        let window: (i64, Option<Vec<u8>>, Option<Vec<u8>>) = conn.query_row(
            "SELECT id, active_workspace_id, route_workbench_id FROM window_state",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(window, (1, None, None));

        assert!(
            !fixture.profile.backups_dir().exists(),
            "пустая база мигрирует без резервной копии"
        );
    }
    Ok(())
}

#[test]
fn migration_after_latest_backs_up_first() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.latest_with_workspace()?;

    let db = fixture.open_with(&[V3])?;
    assert_eq!(db.schema_version(), 3);
    let conn = db.connection();
    assert_eq!(user_version(conn)?, 3);
    assert!(table_exists(conn, "extra")?);
    assert_eq!(workspace_titles(conn)?, ["Работа"]);

    let name = Fixture::backup_name(LATEST);
    assert_eq!(
        fixture.backups()?,
        [name.as_str()],
        "одна копия без -wal и -shm"
    );
    let copy = Connection::open_with_flags(
        fixture.profile.backups_dir().join(&name),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    assert_eq!(user_version(&copy)?, i64::from(LATEST));
    assert_eq!(schema(&copy)?, SCHEMA_V2);
    assert_eq!(workspace_titles(&copy)?, ["Работа"]);
    let journal: String = copy.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    assert_eq!(journal, "delete");
    drop(copy);
    assert_eq!(fixture.backups()?, [name.as_str()]);
    Ok(())
}

#[test]
fn failed_second_migration_rolls_back_and_keeps_backup() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.latest_with_workspace()?;

    let error = fixture.open_with(&[V3_BROKEN]).err();
    assert!(
        matches!(error, Some(StoreError::Migration { to: 3, .. })),
        "{error:?}"
    );

    let raw = fixture.raw()?;
    assert_eq!(user_version(&raw)?, i64::from(LATEST));
    assert!(!table_exists(&raw, "extra")?, "первый оператор v3 откатан");
    assert_eq!(schema(&raw)?, SCHEMA_V2);
    assert_eq!(workspace_titles(&raw)?, ["Работа"]);
    drop(raw);

    let name = Fixture::backup_name(LATEST);
    assert_eq!(fixture.backups()?, [name.as_str()]);
    let copy = Connection::open_with_flags(
        fixture.profile.backups_dir().join(&name),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    assert_eq!(user_version(&copy)?, i64::from(LATEST));
    assert_eq!(workspace_titles(&copy)?, ["Работа"]);
    drop(copy);

    fixture.clock.advance(1);
    let db = fixture.open()?;
    assert_eq!(db.schema_version(), LATEST);
    assert_eq!(workspace_titles(db.connection())?, ["Работа"]);
    assert_eq!(
        fixture.backups()?,
        [name.as_str()],
        "последняя версия на последней копии не делает"
    );
    Ok(())
}

#[test]
fn failed_migration_on_fresh_file_stays_on_last_good_version() -> TestResult {
    let fixture = Fixture::new()?;
    let error = fixture.open_with(&[V3_BROKEN]).err();
    assert!(
        matches!(error, Some(StoreError::Migration { to: 3, .. })),
        "{error:?}"
    );

    let raw = fixture.raw()?;
    assert_eq!(user_version(&raw)?, i64::from(LATEST));
    assert_eq!(schema(&raw)?, SCHEMA_V2);
    assert!(!fixture.profile.backups_dir().exists());
    Ok(())
}

#[test]
fn backup_failure_prevents_migration() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.latest_with_workspace()?;
    let backups = fixture.profile.backups_dir();
    fs::write(&backups, b"not a directory")?;

    let error = fixture.open_with(&[V3]).err();
    assert!(
        matches!(
            &error,
            Some(StoreError::Backup {
                source: BackupError::Dir(_),
                ..
            })
        ),
        "{error:?}"
    );

    let raw = fixture.raw()?;
    assert_eq!(user_version(&raw)?, i64::from(LATEST));
    assert!(!table_exists(&raw, "extra")?);
    assert_eq!(schema(&raw)?, SCHEMA_V2);
    assert_eq!(workspace_titles(&raw)?, ["Работа"]);
    assert_eq!(fs::read(&backups)?, b"not a directory");
    Ok(())
}

#[test]
fn existing_backup_name_is_not_overwritten() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.latest_with_workspace()?;
    let backups = fixture.profile.backups_dir();
    fs::create_dir(&backups)?;
    let taken = backups.join(Fixture::backup_name(LATEST));
    fs::write(&taken, b"earlier copy")?;

    let error = fixture.open_with(&[V3]).err();
    assert!(
        matches!(
            &error,
            Some(StoreError::Backup { source: BackupError::Exists, path }) if *path == taken
        ),
        "{error:?}"
    );
    assert_eq!(fs::read(&taken)?, b"earlier copy");
    assert_eq!(user_version(&fixture.raw()?)?, i64::from(LATEST));

    fixture.clock.advance(1);
    let db = fixture.open_with(&[V3])?;
    assert_eq!(db.schema_version(), 3);
    assert_eq!(
        fixture.backups()?,
        [
            Fixture::backup_name(LATEST),
            format!("state.db.v{LATEST}.{}.bak", NOW.0 + 1)
        ]
    );
    Ok(())
}

#[test]
fn newer_schema_is_refused_without_writing() -> TestResult {
    let fixture = Fixture::new()?;
    fs::create_dir_all(fixture.profile.state_dir())?;
    {
        let raw = fixture.raw()?;
        raw.execute_batch("CREATE TABLE future (x INTEGER); PRAGMA user_version = 3;")?;
        let journal: String = raw.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        assert_eq!(
            journal, "delete",
            "без WAL преждевременная запись видна по байтам"
        );
    }
    let before = fs::read(fixture.db())?;

    let error = fixture.open().err();
    assert!(
        matches!(
            error,
            Some(StoreError::NewerSchema {
                found: 3,
                supported: LATEST
            })
        ),
        "{error:?}"
    );
    assert_eq!(fs::read(fixture.db())?, before);
    assert_eq!(list_dir(fixture.profile.state_dir())?, ["state.db"]);
    Ok(())
}

#[test]
fn refusal_leaves_outstanding_wal_unapplied() -> TestResult {
    let cases = [
        ("новая схема", 3, "NewerSchema"),
        ("отрицательная версия", -1, "Corrupt"),
    ];
    for (label, version, expected) in cases {
        let fixture = Fixture::new()?;
        fs::create_dir_all(fixture.profile.state_dir())?;
        let mut wal = fixture.db().into_os_string();
        wal.push("-wal");
        let wal = PathBuf::from(wal);
        {
            // Как после сбоя другой версии: кадры с её схемой остались в `-wal`.
            let raw = fixture.raw()?;
            raw.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
            let mode: String =
                raw.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
            assert_eq!(mode, "wal", "{label}");
            raw.pragma_update(None, "wal_autocheckpoint", 0)?;
            raw.execute_batch(&format!(
                "CREATE TABLE future (x INTEGER); PRAGMA user_version = {version};"
            ))?;
        }
        let before = fs::read(fixture.db())?;
        let wal_before = fs::read(&wal)?;
        assert!(!wal_before.is_empty(), "{label}: кадры остались в -wal");

        let error = fixture.open().err();
        let refused = match expected {
            "NewerSchema" => matches!(
                error,
                Some(StoreError::NewerSchema {
                    found: 3,
                    supported: LATEST
                })
            ),
            _ => matches!(error, Some(StoreError::Corrupt { .. })),
        };
        assert!(refused, "{label}: {error:?}");
        assert!(
            fs::read(fixture.db())? == before,
            "{label}: кадры -wal перенесены в файл базы"
        );
        assert!(
            fs::read(&wal).ok() == Some(wal_before),
            "{label}: -wal изменён или удалён"
        );
    }
    Ok(())
}

#[test]
fn garbage_file_is_corrupt_and_untouched() -> TestResult {
    let mut header_with_garbage = b"SQLite format 3\0".to_vec();
    header_with_garbage.extend(std::iter::repeat_n(0xA5, 4080));
    let samples: [(&str, Vec<u8>); 3] = [
        ("текст", b"definitely not a database\n".repeat(200)),
        ("два байта", b"xx".to_vec()),
        ("заголовок и мусор", header_with_garbage),
    ];
    for (label, bytes) in samples {
        let fixture = Fixture::new()?;
        fs::create_dir_all(fixture.profile.state_dir())?;
        fs::write(fixture.db(), &bytes)?;

        let error = fixture.open().err();
        assert!(
            matches!(&error, Some(StoreError::Corrupt { path, .. }) if *path == fixture.db()),
            "{label}: {error:?}"
        );
        assert_eq!(fs::read(fixture.db())?, bytes, "{label}");
        assert_eq!(
            list_dir(fixture.profile.state_dir())?,
            ["state.db"],
            "{label}"
        );
    }
    Ok(())
}

#[test]
fn foreign_schema_at_version_zero_is_backed_up_before_migration() -> TestResult {
    // Чужая таблица рядом со схемой: миграция идёт поверх, копия хранит файл до неё.
    let fixture = Fixture::new()?;
    fs::create_dir_all(fixture.profile.state_dir())?;
    fixture
        .raw()?
        .execute_batch("CREATE TABLE notes (x INTEGER); INSERT INTO notes VALUES (7);")?;
    let db = fixture.open()?;
    assert_eq!(user_version(db.connection())?, i64::from(LATEST));
    assert!(table_exists(db.connection(), "workspaces")?);
    let kept: i64 = db
        .connection()
        .query_row("SELECT x FROM notes", [], |row| row.get(0))?;
    assert_eq!(kept, 7);
    drop(db);
    let name = Fixture::backup_name(0);
    assert_eq!(fixture.backups()?, [name.as_str()]);
    let copy = Connection::open_with_flags(
        fixture.profile.backups_dir().join(&name),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    assert_eq!(user_version(&copy)?, 0);
    assert!(table_exists(&copy, "notes")?);
    assert!(!table_exists(&copy, "workspaces")?);
    drop(copy);

    // Чужая таблица с именем таблицы схемы: миграция откатывается, копия остаётся.
    let fixture = Fixture::new()?;
    fs::create_dir_all(fixture.profile.state_dir())?;
    fixture
        .raw()?
        .execute_batch("CREATE TABLE workspaces (x INTEGER);")?;
    let error = fixture.open().err();
    assert!(
        matches!(error, Some(StoreError::Migration { to: 1, .. })),
        "{error:?}"
    );
    let raw = fixture.raw()?;
    assert_eq!(user_version(&raw)?, 0);
    assert!(!table_exists(&raw, "meta")?);
    assert_eq!(fixture.backups()?, [Fixture::backup_name(0).as_str()]);
    Ok(())
}

#[test]
fn negative_user_version_is_corrupt() -> TestResult {
    let fixture = Fixture::new()?;
    fs::create_dir_all(fixture.profile.state_dir())?;
    fixture
        .raw()?
        .execute_batch("CREATE TABLE t (x INTEGER); PRAGMA user_version = -1;")?;
    let before = fs::read(fixture.db())?;

    let error = fixture.open().err();
    assert!(
        matches!(error, Some(StoreError::Corrupt { .. })),
        "{error:?}"
    );
    assert_eq!(fs::read(fixture.db())?, before);
    Ok(())
}

#[test]
fn fixture_v1_migrates_to_v2_with_backup() -> TestResult {
    let fixture = Fixture::new()?;
    fs::create_dir_all(fixture.profile.state_dir())?;
    fixture.raw()?.execute_batch(FIXTURE_V1)?;

    let counts = |conn: &Connection| -> TestResult<Vec<i64>> {
        [
            "meta",
            "workspaces",
            "projects",
            "project_roots",
            "workbenches",
            "window_state",
        ]
        .iter()
        .map(|table| {
            Ok(
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?,
            )
        })
        .collect()
    };
    let expected = counts(&fixture.raw()?)?;
    assert_eq!(expected, [1, 3, 4, 4, 3, 1]);
    assert_eq!(
        schema(&fixture.raw()?)?,
        SCHEMA_V1,
        "образец v1 расходится со схемой v1"
    );

    let db = fixture.open()?;
    let conn = db.connection();
    assert_eq!(db.schema_version(), 2);
    assert_eq!(user_version(conn)?, 2);
    assert_eq!(schema(conn)?, SCHEMA_V2);
    assert_eq!(counts(conn)?, expected);
    for table in ["panels", "workbench_layouts"] {
        let rows: i64 = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
        assert_eq!(rows, 0, "миграция не пишет данных в {table}");
    }
    let revision: i64 =
        conn.query_row("SELECT value FROM meta WHERE key = 'revision'", [], |row| {
            row.get(0)
        })?;
    assert_eq!(revision, 12);
    assert_eq!(workspace_titles(conn)?, ["Основной", "Работа", "Архив"]);
    drop(db);

    let name = Fixture::backup_name(1);
    assert_eq!(fixture.backups()?, [name.as_str()]);
    let copy = Connection::open_with_flags(
        fixture.profile.backups_dir().join(&name),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    assert_eq!(user_version(&copy)?, 1);
    assert_eq!(schema(&copy)?, SCHEMA_V1);
    assert_eq!(counts(&copy)?, expected);
    drop(copy);

    fixture.clock.advance(1);
    assert_eq!(fixture.open()?.schema_version(), 2);
    assert_eq!(
        fixture.backups()?,
        [name.as_str()],
        "база на последней версии открывается без миграции и копии"
    );
    Ok(())
}
