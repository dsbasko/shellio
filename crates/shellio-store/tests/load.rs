//! Построчная загрузка `SqlitePersist` из настоящего `state.db`: записи, вставленные прямым
//! SQL в обход ядра, пропускаются, сбрасываются или только сообщаются диагностикой, а запуск
//! ядра на загруженном состоянии держит инварианты.

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::Connection;
use shellio_core::test_support::{FakeRootResolver, ManualClock, SequentialIds};
use shellio_core::{
    Change, Command, CommandValue, Core, CoreHandle, Diagnostic, ErrorCode, EventsSince, Loaded,
    NAVIGATOR_PLUGIN, PanelId, Persist, ProjectId, ProjectRootId, Query, ReplyValue, Revision,
    RootResolver, Route, StartError, State, TERMINAL_PLUGIN, TreeScope, UnixMillis, Violation,
    WindowState, WorkbenchId, WorkbenchLayout, WorkspaceId,
};
use shellio_store::{Database, Profile, SqlitePersist, StoreError};
use tempfile::TempDir;
use tokio::runtime::Handle;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);
const FIXTURE_V1: &str = include_str!("fixtures/v1.sql");

const WS: u8 = 0xa0;
const PR: u8 = 0xb0;
const WB: u8 = 0xc0;
const RT: u8 = 0xd0;
const PN: u8 = 0xe0;

/// ID записи, вставленной в обход ядра: первый байт — таблица, последний — номер.
/// Последовательный генератор ядра выдаёт ID с нулевым первым байтом, поэтому они не совпадут.
fn id(table: u8, n: u8) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[0] = table;
    bytes[15] = n;
    bytes
}

fn workspace_id(n: u8) -> WorkspaceId {
    WorkspaceId::from_bytes(id(WS, n))
}

fn project_id(n: u8) -> ProjectId {
    ProjectId::from_bytes(id(PR, n))
}

fn workbench_id(n: u8) -> WorkbenchId {
    WorkbenchId::from_bytes(id(WB, n))
}

fn root_id(n: u8) -> ProjectRootId {
    ProjectRootId::from_bytes(id(RT, n))
}

fn panel_id(n: u8) -> PanelId {
    PanelId::from_bytes(id(PN, n))
}

fn diag(table: &str, rowid: i64, reason: &str) -> Diagnostic {
    Diagnostic {
        table: table.to_owned(),
        rowid,
        reason: reason.to_owned(),
    }
}

/// Пустая база v1 в изолированном профиле.
struct Db {
    _dir: TempDir,
    profile: Profile,
}

impl Db {
    fn new() -> TestResult<Self> {
        let dir = TempDir::new()?;
        let profile = Profile::isolated(dir.path().join("state"));
        drop(Database::open(&profile, &ManualClock::new(NOW))?);
        Ok(Self { _dir: dir, profile })
    }

    /// База v1 из образца, ещё не мигрированная.
    fn fixture_v1() -> TestResult<Self> {
        let dir = TempDir::new()?;
        let profile = Profile::isolated(dir.path().join("state"));
        profile.ensure_state_dir()?;
        Connection::open(profile.state_db())?.execute_batch(FIXTURE_V1)?;
        Ok(Self { _dir: dir, profile })
    }

    /// Соединение в обход хранилища, без проверки внешних ключей: так вставляется то,
    /// чего ядро не пишет.
    fn raw(&self) -> TestResult<Connection> {
        let conn = Connection::open(self.profile.state_db())?;
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        Ok(conn)
    }

    fn persist(&self) -> TestResult<SqlitePersist> {
        Ok(SqlitePersist::new(Database::open(
            &self.profile,
            &ManualClock::new(NOW),
        )?))
    }

    fn load(&self) -> TestResult<Loaded> {
        self.persist()?.load()
    }

    fn bytes(&self) -> TestResult<Vec<u8>> {
        Ok(fs::read(self.profile.state_db())?)
    }

    fn count(&self, table: &str) -> TestResult<usize> {
        let count: i64 =
            self.raw()?
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
        Ok(usize::try_from(count)?)
    }

    fn revision_in_db(&self) -> TestResult<i64> {
        Ok(self
            .raw()?
            .query_row("SELECT value FROM meta WHERE key = 'revision'", [], |row| {
                row.get(0)
            })?)
    }

    fn start(&self, resolver: impl RootResolver + 'static) -> TestResult<CoreHandle> {
        let core = Core::start(
            &Handle::current(),
            Box::new(self.persist()?),
            Box::new(ManualClock::new(NOW)),
            Box::new(SequentialIds::new()),
            Arc::new(resolver),
        )
        .map_err(|error: StartError| error.to_string())?;
        Ok(core)
    }
}

fn workspace_row(
    conn: &Connection,
    rowid: i64,
    id: &[u8],
    title: &str,
    position: i64,
) -> TestResult {
    conn.execute(
        "INSERT INTO workspaces (rowid, id, title, position) VALUES (?1, ?2, ?3, ?4)",
        (rowid, id, title, position),
    )?;
    Ok(())
}

fn project_row(
    conn: &Connection,
    rowid: i64,
    id: &[u8],
    workspace: &[u8],
    title: &str,
    list_position: Option<i64>,
) -> TestResult {
    conn.execute(
        "INSERT INTO projects (rowid, id, workspace_id, title, list_position) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (rowid, id, workspace, title, list_position),
    )?;
    Ok(())
}

fn root_row(
    conn: &Connection,
    rowid: i64,
    id: &[u8],
    project: &[u8],
    path: &str,
    kind: &str,
) -> TestResult {
    let key = if kind == "git" {
        format!("{path}/.git")
    } else {
        path.to_owned()
    };
    conn.execute(
        "INSERT INTO project_roots (rowid, id, project_id, path, kind, key) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (rowid, id, project, path, kind, key),
    )?;
    Ok(())
}

fn workbench_row(
    conn: &Connection,
    rowid: i64,
    id: &[u8],
    project: &[u8],
    title: &str,
    position: i64,
) -> TestResult {
    conn.execute(
        "INSERT INTO workbenches (rowid, id, project_id, title, position) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (rowid, id, project, title, position),
    )?;
    Ok(())
}

fn panel_row(
    conn: &Connection,
    rowid: i64,
    id: &[u8],
    workbench: &[u8],
    plugin: &str,
    title: &str,
    created_at: i64,
) -> TestResult {
    conn.execute(
        "INSERT INTO panels (rowid, id, workbench, plugin, title, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (rowid, id, workbench, plugin, title, created_at),
    )?;
    Ok(())
}

fn layout_row(conn: &Connection, rowid: i64, workbench: &[u8], text: &str) -> TestResult {
    conn.execute(
        "INSERT INTO workbench_layouts (rowid, workbench, layout) VALUES (?1, ?2, ?3)",
        (rowid, workbench, text),
    )?;
    Ok(())
}

/// Текст строки раскладки в конверте версии `version`.
fn envelope(version: u32, layout: &WorkbenchLayout) -> TestResult<String> {
    Ok(serde_json::json!({"version": version, "layout": layout}).to_string())
}

fn set_window(conn: &Connection, active: Option<&[u8]>, route: Option<&[u8]>) -> TestResult {
    conn.execute(
        "UPDATE window_state SET active_workspace_id = ?1, route_workbench_id = ?2 WHERE id = 1",
        (active, route),
    )?;
    Ok(())
}

fn set_revision(conn: &Connection, revision: i64) -> TestResult {
    conn.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'revision'",
        [revision],
    )?;
    Ok(())
}

fn keys<K: Ord + Copy, V>(map: &std::collections::BTreeMap<K, V>) -> BTreeSet<K> {
    map.keys().copied().collect()
}

#[test]
fn fixture_v1_loads_without_findings() {
    let dir = TempDir::new().unwrap();
    let profile = Profile::isolated(dir.path().join("state"));
    profile.ensure_state_dir().unwrap();
    Connection::open(profile.state_db())
        .unwrap()
        .execute_batch(FIXTURE_V1)
        .unwrap();

    let loaded = SqlitePersist::new(Database::open(&profile, &ManualClock::new(NOW)).unwrap())
        .load()
        .unwrap();
    assert_eq!(loaded.revision, Revision(12));
    assert_eq!(loaded.storage.schema_version, 2);
    assert_eq!(loaded.storage.state_db, profile.state_db());
    let state = &loaded.state;
    assert_eq!(state.diagnostics, Vec::new());
    assert_eq!(state.check_invariants(), Vec::new());
    assert_eq!(
        (
            state.workspaces.len(),
            state.projects.len(),
            state.roots.len(),
            state.workbenches.len()
        ),
        (3, 4, 4, 3)
    );

    let main: WorkspaceId = "019a8f2e-0000-7000-8000-00000000a001".parse().unwrap();
    let archive: WorkspaceId = "019a8f2e-0000-7000-8000-00000000a003".parse().unwrap();
    let main_workbench: WorkbenchId = "019a8f2e-0000-7000-8000-00000000c001".parse().unwrap();
    let worktree: ProjectRootId = "019a8f2e-0000-7000-8000-00000000d003".parse().unwrap();
    let old: ProjectId = "019a8f2e-0000-7000-8000-00000000b004".parse().unwrap();
    assert_eq!(state.window.active_workspace, Some(main));
    assert_eq!(state.window.route, Route::Workbench(main_workbench));
    assert_eq!(state.workspaces[&main].last_workbench, Some(main_workbench));
    assert_eq!(
        state.workspaces[&archive].deleted_at,
        Some(UnixMillis(1_795_000_000_000))
    );
    assert_eq!(
        state.roots[&worktree].path,
        PathBuf::from("/Users/example/src/site-feature")
    );
    assert_eq!(
        state.roots[&worktree].key.as_str(),
        "/Users/example/src/site/.git"
    );
    assert_eq!(
        state.projects[&old].removed_at,
        Some(UnixMillis(1_795_000_200_000))
    );
    assert_eq!(state.projects[&old].list_position, None);
}

#[test]
fn corrupt_rows_are_skipped_with_dependents_and_database_is_untouched() {
    let db = Db::new().unwrap();
    let raw = db.raw().unwrap();
    let short_id = &id(WS, 2)[..15];
    let w = |n| id(WS, n);
    let p = |n| id(PR, n);
    let b = |n| id(WB, n);
    let r = |n| id(RT, n);

    workspace_row(&raw, 1, &w(1), "Работа", 0).unwrap();
    workspace_row(&raw, 2, short_id, "Битый ID", 1).unwrap();
    workspace_row(&raw, 3, &w(3), "   ", 2).unwrap();
    workspace_row(&raw, 4, &w(4), "Дом", 3).unwrap();
    workspace_row(&raw, 5, &w(5), "Архив", 0).unwrap();
    raw.execute(
        "UPDATE workspaces SET last_workbench_id = ?1 WHERE rowid = 4",
        [id(WB, 9).as_slice()],
    )
    .unwrap();

    project_row(&raw, 1, &p(1), &w(1), "shellio", Some(0)).unwrap();
    project_row(&raw, 2, &p(2), &w(3), "у пустого имени", Some(0)).unwrap();
    project_row(&raw, 3, &p(3), &id(WS, 0xff), "висячий", Some(0)).unwrap();
    project_row(&raw, 4, &p(4), &w(4), "home-a", Some(0)).unwrap();
    project_row(&raw, 5, &p(5), &w(4), "home-b", Some(1)).unwrap();
    project_row(&raw, 6, &p(6), &w(1), "notes", Some(1)).unwrap();
    project_row(&raw, 7, &p(7), short_id, "у битого ID", Some(0)).unwrap();

    root_row(&raw, 1, &r(1), &p(1), "/src/shellio", "git").unwrap();
    root_row(&raw, 2, &r(2), &p(2), "/src/empty", "folder").unwrap();
    root_row(&raw, 3, &r(3), &p(6), "/src/notes", "svn").unwrap();
    root_row(&raw, 4, &r(4), &p(4), "/home/a", "folder").unwrap();
    root_row(&raw, 5, &r(5), &p(5), "/home/b", "folder").unwrap();
    root_row(&raw, 6, &r(6), &p(7), "/src/short", "folder").unwrap();

    workbench_row(&raw, 1, &b(1), &p(1), "main", 0).unwrap();
    workbench_row(&raw, 2, &b(2), &p(3), "висячая", 0).unwrap();
    workbench_row(&raw, 3, &b(3), &p(4), "a", 0).unwrap();
    workbench_row(&raw, 4, &b(4), &p(2), "у пустого имени", 0).unwrap();
    set_window(&raw, Some(&w(1)), Some(&b(1))).unwrap();
    drop(raw);

    let before = db.bytes().unwrap();
    let loaded = db.load().unwrap();
    assert_eq!(db.bytes().unwrap(), before, "загрузка изменила байты базы");

    let state = &loaded.state;
    assert_eq!(
        state.diagnostics,
        [
            diag("workspaces", 2, "invalid_id"),
            diag("workspaces", 3, "title_empty"),
            diag("projects", 2, "owner_missing"),
            diag("projects", 3, "owner_missing"),
            diag("projects", 7, "owner_missing"),
            diag("project_roots", 2, "owner_missing"),
            diag("project_roots", 3, "unknown_kind"),
            diag("project_roots", 6, "owner_missing"),
            diag("workbenches", 2, "owner_missing"),
            diag("workbenches", 4, "owner_missing"),
            diag("workspaces", 4, "last_workbench_missing"),
            diag("workspaces", 5, "position_taken"),
        ]
    );
    assert_eq!(
        keys(&state.workspaces),
        BTreeSet::from([workspace_id(1), workspace_id(4), workspace_id(5)])
    );
    assert_eq!(
        keys(&state.projects),
        BTreeSet::from([project_id(1), project_id(4), project_id(5), project_id(6)])
    );
    assert_eq!(
        keys(&state.roots),
        BTreeSet::from([root_id(1), root_id(4), root_id(5)])
    );
    assert_eq!(
        keys(&state.workbenches),
        BTreeSet::from([workbench_id(1), workbench_id(3)])
    );
    assert_eq!(state.workspaces[&workspace_id(4)].last_workbench, None);
    assert_eq!(state.workspaces[&workspace_id(5)].position, 0);
    assert_eq!(state.window.active_workspace, Some(workspace_id(1)));
    assert_eq!(state.window.route, Route::Workbench(workbench_id(1)));
    // Совпадение позиций — единственное, что загрузка оставляет в состоянии.
    let violations = state.check_invariants();
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(matches!(violations[0], Violation::PositionTaken { .. }));

    assert_eq!(db.bytes().unwrap(), before);
    assert_eq!(db.count("workspaces").unwrap(), 5);
    assert_eq!(db.count("workbenches").unwrap(), 4);
}

#[test]
fn values_outside_model_are_reset_or_reported() {
    let db = Db::new().unwrap();
    let raw = db.raw().unwrap();
    let w = |n| id(WS, n);
    let p = |n| id(PR, n);
    let b = |n| id(WB, n);

    workspace_row(&raw, 1, &w(1), "Работа", 0).unwrap();
    workspace_row(&raw, 2, &w(2), "  Дом  ", 1).unwrap();
    workspace_row(&raw, 3, &w(3), &"я".repeat(201), 2).unwrap();
    workspace_row(&raw, 4, &w(4), "звонок\u{7}", 3).unwrap();
    workspace_row(&raw, 5, &w(5), "Минус", -1).unwrap();
    workspace_row(&raw, 6, &w(6), "работа", 4).unwrap();
    workspace_row(&raw, 7, &w(7), "РАБОТА", 5).unwrap();
    workspace_row(&raw, 8, &w(8), &"я".repeat(200), 6).unwrap();
    raw.execute(
        "UPDATE workspaces SET deleted_at = ?1 WHERE rowid = 7",
        [NOW.0],
    )
    .unwrap();
    raw.execute(
        "UPDATE workspaces SET last_workbench_id = ?1 WHERE rowid = 1",
        [b(2).as_slice()],
    )
    .unwrap();

    project_row(&raw, 1, &p(1), &w(1), "app", Some(0)).unwrap();
    project_row(&raw, 2, &p(2), &w(2), "site", Some(1 << 32)).unwrap();
    project_row(&raw, 3, &p(3), &w(2), "web", None).unwrap();
    project_row(&raw, 4, &p(4), &w(1), "APP", Some(1)).unwrap();
    project_row(&raw, 5, &p(5), &w(1), "\ttools", Some(2)).unwrap();

    workbench_row(&raw, 1, &b(1), &p(1), "main", 0).unwrap();
    workbench_row(&raw, 2, &b(2), &p(3), "main", 0).unwrap();
    workbench_row(&raw, 3, &b(3), &p(1), "Main", 1).unwrap();
    workbench_row(&raw, 4, &b(4), &p(1), "extra", 1).unwrap();
    set_window(&raw, Some(&w(1)[..15]), Some(&b(1))).unwrap();
    drop(raw);

    let state = db.load().unwrap().state;
    assert_eq!(
        state.diagnostics,
        [
            diag("workspaces", 2, "title_trimmed"),
            diag("workspaces", 3, "title_too_long"),
            diag("workspaces", 4, "title_control_char"),
            diag("workspaces", 5, "position_out_of_range"),
            diag("projects", 2, "position_out_of_range"),
            diag("projects", 5, "title_trimmed"),
            diag("workspaces", 1, "last_workbench_foreign"),
            diag("window_state", 1, "active_workspace_missing"),
            diag("window_state", 1, "route_workbench_foreign"),
            diag("workbenches", 4, "position_taken"),
            diag("workspaces", 6, "name_taken"),
            diag("projects", 4, "name_taken"),
            diag("workbenches", 3, "name_taken"),
        ]
    );
    assert_eq!(
        keys(&state.workspaces),
        BTreeSet::from([
            workspace_id(1),
            workspace_id(2),
            workspace_id(6),
            workspace_id(7),
            workspace_id(8)
        ])
    );
    assert_eq!(state.workspaces[&workspace_id(2)].title.as_str(), "Дом");
    assert_eq!(state.projects[&project_id(5)].title.as_str(), "tools");
    assert_eq!(state.workspaces[&workspace_id(1)].last_workbench, None);
    assert!(!state.projects.contains_key(&project_id(2)));
    assert_eq!(state.window.active_workspace, None);
    assert_eq!(state.window.route, Route::WorkspaceHome);
    for violation in state.check_invariants() {
        assert!(
            matches!(
                violation,
                Violation::PositionTaken { .. }
                    | Violation::WorkspaceNameTaken { .. }
                    | Violation::ProjectNameTaken { .. }
                    | Violation::WorkbenchNameTaken { .. }
                    | Violation::NoActiveWorkspace
            ),
            "{violation:?}"
        );
    }
}

#[test]
fn text_not_in_utf8_and_remaining_position_findings_are_reported() {
    let db = Db::new().unwrap();
    let raw = db.raw().unwrap();
    let w = |n| id(WS, n);
    let p = |n| id(PR, n);
    let b = |n| id(WB, n);

    workspace_row(&raw, 1, &w(1), "Работа", 0).unwrap();
    workspace_row(&raw, 2, &w(2), "Дом", 1).unwrap();
    raw.execute(
        "UPDATE workspaces SET title = CAST(x'ff' AS TEXT) WHERE rowid = 2",
        [],
    )
    .unwrap();
    project_row(&raw, 1, &p(1), &w(1), "app", Some(0)).unwrap();
    project_row(&raw, 2, &p(2), &w(1), "web", Some(0)).unwrap();
    root_row(&raw, 1, &id(RT, 1), &p(1), "/src/app", "folder").unwrap();
    raw.execute(
        "UPDATE project_roots SET path = CAST(x'2fff' AS TEXT) WHERE rowid = 1",
        [],
    )
    .unwrap();
    workbench_row(&raw, 1, &b(1), &p(1), "main", 0).unwrap();
    workbench_row(&raw, 2, &b(2), &p(1), "extra", -1).unwrap();
    set_window(&raw, Some(&w(1)), None).unwrap();
    drop(raw);

    let state = db.load().unwrap().state;
    assert_eq!(
        state.diagnostics,
        [
            diag("workspaces", 2, "invalid_value"),
            diag("project_roots", 1, "invalid_value"),
            diag("workbenches", 2, "position_out_of_range"),
            diag("projects", 2, "position_taken"),
        ]
    );
    assert_eq!(keys(&state.workspaces), BTreeSet::from([workspace_id(1)]));
    assert_eq!(
        keys(&state.projects),
        BTreeSet::from([project_id(1), project_id(2)])
    );
    assert!(state.roots.is_empty());
    assert_eq!(keys(&state.workbenches), BTreeSet::from([workbench_id(1)]));
}

#[test]
fn window_findings_fall_back_to_workspace_home() {
    let w = |n| id(WS, n);
    let p = |n| id(PR, n);
    let b = |n| id(WB, n);
    // Две записи окна на одном наборе данных: маршрут в другой воркспейс и на отсутствующую
    // рабочую область, в том числе ссылкой не из 16 байт.
    let cases: [(Option<Vec<u8>>, &str); 3] = [
        (Some(b(2).to_vec()), "route_workbench_foreign"),
        (Some(b(9).to_vec()), "route_workbench_missing"),
        (Some(b(1)[..8].to_vec()), "route_workbench_missing"),
    ];
    for (route, reason) in cases {
        let db = Db::new().unwrap();
        let raw = db.raw().unwrap();
        workspace_row(&raw, 1, &w(1), "Работа", 0).unwrap();
        workspace_row(&raw, 2, &w(2), "Дом", 1).unwrap();
        project_row(&raw, 1, &p(1), &w(1), "app", Some(0)).unwrap();
        project_row(&raw, 2, &p(2), &w(2), "site", Some(0)).unwrap();
        workbench_row(&raw, 1, &b(1), &p(1), "main", 0).unwrap();
        workbench_row(&raw, 2, &b(2), &p(2), "main", 0).unwrap();
        set_window(&raw, Some(&w(1)), route.as_deref()).unwrap();
        drop(raw);

        let state = db.load().unwrap().state;
        assert_eq!(state.diagnostics, [diag("window_state", 1, reason)]);
        assert_eq!(state.window.active_workspace, Some(workspace_id(1)));
        assert_eq!(state.window.route, Route::WorkspaceHome);
        assert_eq!(state.check_invariants(), Vec::new());
    }

    let db = Db::new().unwrap();
    db.raw()
        .unwrap()
        .execute("DELETE FROM window_state", [])
        .unwrap();
    let state = db.load().unwrap().state;
    assert_eq!(state.diagnostics, [diag("window_state", 1, "row_missing")]);
    assert_eq!(state.window, State::default().window);
}

#[test]
fn missing_or_negative_revision_is_corrupt() {
    for sql in [
        "DELETE FROM meta",
        "UPDATE meta SET value = -3 WHERE key = 'revision'",
    ] {
        let db = Db::new().unwrap();
        db.raw().unwrap().execute(sql, []).unwrap();
        let before = db.bytes().unwrap();
        let error = db.persist().unwrap().load().unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<StoreError>(),
                Some(StoreError::Corrupt { .. })
            ),
            "{sql}: {error:?}"
        );
        assert_eq!(db.bytes().unwrap(), before);
    }
}

#[tokio::test]
async fn after_position_collision_new_records_go_after_max_and_reload_keeps_rows() {
    let db = Db::new().unwrap();
    let raw = db.raw().unwrap();
    let w = |n| id(WS, n);
    let p = |n| id(PR, n);
    let b = |n| id(WB, n);
    workspace_row(&raw, 1, &w(1), "Работа", 0).unwrap();
    workspace_row(&raw, 2, &w(2), "Дом", 0).unwrap();
    workspace_row(&raw, 3, &w(3), "Архив", 7).unwrap();
    raw.execute(
        "UPDATE workspaces SET deleted_at = ?1 WHERE rowid = 3",
        [NOW.0],
    )
    .unwrap();
    project_row(&raw, 1, &p(1), &w(1), "app", Some(0)).unwrap();
    workbench_row(&raw, 1, &b(1), &p(1), "main", 2).unwrap();
    workbench_row(&raw, 2, &b(2), &p(1), "review", 2).unwrap();
    set_window(&raw, Some(&w(1)), None).unwrap();
    set_revision(&raw, 4).unwrap();
    drop(raw);

    // Находки загрузки: запуск сдвигает ревизию записью без дельт.
    let core = db.start(FakeRootResolver::new()).unwrap();
    let created = core
        .request(Command::WorkspaceCreate {
            title: "Новый".into(),
        })
        .await
        .unwrap();
    let ReplyValue::Command(CommandValue::Workspace { workspace }) = created.value else {
        panic!("неожиданный ответ {created:?}");
    };
    assert_eq!(created.revision, Revision(6));
    let created = core
        .request(Command::WorkbenchCreate {
            project: project_id(1),
            title: "третья".into(),
        })
        .await
        .unwrap();
    let ReplyValue::Command(CommandValue::Workbench { workbench }) = created.value else {
        panic!("неожиданный ответ {created:?}");
    };
    let (snapshot, _) = core.subscribe().await.unwrap();
    core.shutdown().await;

    let state = &snapshot.state;
    assert_eq!(state.workspaces[&workspace].position, 8);
    assert_eq!(state.workbenches[&workbench].position, 3);

    let reloaded = db.load().unwrap();
    assert_eq!(reloaded.revision, Revision(7));
    assert_eq!(reloaded.state, **state);
    assert_eq!(
        reloaded.state.diagnostics,
        [
            diag("workspaces", 2, "position_taken"),
            diag("workbenches", 2, "position_taken"),
        ]
    );
    assert_eq!(
        db.count("workspaces").unwrap(),
        reloaded.state.workspaces.len()
    );
    assert_eq!(db.count("projects").unwrap(), reloaded.state.projects.len());
    assert_eq!(
        db.count("workbenches").unwrap(),
        reloaded.state.workbenches.len()
    );
    assert_eq!(reloaded.state.workspaces.len(), 4);
    assert_eq!(reloaded.state.workbenches.len(), 3);
}

#[tokio::test]
async fn dangling_active_workspace_is_reset_and_start_focuses_in_one_record() {
    let db = Db::new().unwrap();
    let raw = db.raw().unwrap();
    workspace_row(&raw, 1, &id(WS, 1), "Работа", 1).unwrap();
    workspace_row(&raw, 2, &id(WS, 2), "Дом", 0).unwrap();
    set_window(&raw, Some(&id(WS, 9)), None).unwrap();
    set_revision(&raw, 5).unwrap();
    drop(raw);

    let core = db.start(FakeRootResolver::new()).unwrap();
    let (snapshot, _) = core.subscribe().await.unwrap();
    assert_eq!(snapshot.revision, Revision(6));
    assert_eq!(
        snapshot.state.window.active_workspace,
        Some(workspace_id(2))
    );
    assert_eq!(snapshot.state.window.route, Route::WorkspaceHome);

    // Загрузка изменила состояние ревизии 5: курсор прошлого запуска на ней получает снимок,
    // а не событие запуска поверх зеркала со сброшенным воркспейсом.
    assert_eq!(
        core.events_since(Revision(5)).await.unwrap(),
        EventsSince::Reset(snapshot.clone())
    );
    assert_eq!(
        core.events_since(Revision(6)).await.unwrap(),
        EventsSince::Events(Vec::new())
    );

    let status = core.query(Query::Status {}).await.unwrap();
    let ReplyValue::Status(status) = status.value else {
        panic!("неожиданный ответ {status:?}");
    };
    assert_eq!(
        status.diagnostics,
        [diag("window_state", 1, "active_workspace_missing")]
    );
    assert_eq!(status.schema_version, 2);
    assert_eq!(status.state_db, db.profile.state_db());
    core.shutdown().await;

    assert_eq!(db.revision_in_db().unwrap(), 6);
    let reloaded = db.load().unwrap();
    assert_eq!(reloaded.state.diagnostics, Vec::new());
    assert_eq!(
        reloaded.state.window.active_workspace,
        Some(workspace_id(2))
    );
    assert_eq!(reloaded.state.check_invariants(), Vec::new());
}

#[tokio::test]
async fn findings_without_startup_change_still_move_revision_past_old_cursors() {
    let db = Db::new().unwrap();
    let raw = db.raw().unwrap();
    workspace_row(&raw, 1, &id(WS, 1), "Работа", 0).unwrap();
    workspace_row(&raw, 2, &id(WS, 2), "работа", 1).unwrap();
    set_window(&raw, Some(&id(WS, 1)), None).unwrap();
    set_revision(&raw, 4).unwrap();
    drop(raw);
    assert_eq!(
        db.load().unwrap().state.diagnostics,
        [diag("workspaces", 2, "name_taken")]
    );

    // Окно цело, внутренних команд нет: запуск пишет запись без дельт и без события.
    let core = db.start(FakeRootResolver::new()).unwrap();
    let (snapshot, _) = core.subscribe().await.unwrap();
    assert_eq!(snapshot.revision, Revision(5));
    assert_eq!(db.revision_in_db().unwrap(), 5);
    assert_eq!(
        core.events_since(Revision(4)).await.unwrap(),
        EventsSince::Reset(snapshot.clone())
    );
    for _ in 0..2 {
        assert_eq!(
            core.events_since(snapshot.revision).await.unwrap(),
            EventsSince::Events(Vec::new())
        );
    }
    core.shutdown().await;
    assert_eq!(db.revision_in_db().unwrap(), 5);
}

#[tokio::test]
async fn route_into_removed_project_goes_home_on_start_and_focus_skips_it() {
    let db = Db::new().unwrap();
    let raw = db.raw().unwrap();
    workspace_row(&raw, 1, &id(WS, 1), "Работа", 0).unwrap();
    workspace_row(&raw, 2, &id(WS, 2), "Дом", 1).unwrap();
    project_row(&raw, 1, &id(PR, 1), &id(WS, 1), "old", None).unwrap();
    raw.execute(
        "UPDATE projects SET removed_at = ?1 WHERE rowid = 1",
        [NOW.0 - 1],
    )
    .unwrap();
    workbench_row(&raw, 1, &id(WB, 1), &id(PR, 1), "Ревью", 0).unwrap();
    raw.execute(
        "UPDATE workspaces SET last_workbench_id = ?1 WHERE rowid = 1",
        [&id(WB, 1)[..]],
    )
    .unwrap();
    set_window(&raw, Some(&id(WS, 1)), Some(&id(WB, 1))).unwrap();
    set_revision(&raw, 3).unwrap();
    drop(raw);

    // Область существует и принадлежит воркспейсу: загрузка оставляет обе ссылки.
    let loaded = db.load().unwrap();
    assert_eq!(loaded.state.diagnostics, Vec::new());
    assert_eq!(loaded.state.window.route, Route::Workbench(workbench_id(1)));
    assert_eq!(
        loaded.state.workspaces[&workspace_id(1)].last_workbench,
        Some(workbench_id(1))
    );

    let core = db.start(FakeRootResolver::new()).unwrap();
    let (snapshot, _) = core.subscribe().await.unwrap();
    assert_eq!(snapshot.revision, Revision(4));
    assert_eq!(
        snapshot.state.window,
        WindowState {
            active_workspace: Some(workspace_id(1)),
            route: Route::WorkspaceHome,
        }
    );
    assert_eq!(snapshot.state.check_invariants(), Vec::new());

    for workspace in [workspace_id(2), workspace_id(1)] {
        core.request(Command::WindowFocusWorkspace { workspace })
            .await
            .unwrap();
    }
    let (snapshot, _) = core.subscribe().await.unwrap();
    assert_eq!(snapshot.revision, Revision(6));
    assert_eq!(snapshot.state.window.route, Route::WorkspaceHome);
    core.shutdown().await;
    assert_eq!(db.revision_in_db().unwrap(), 6);
}

#[tokio::test]
async fn removed_project_keeps_its_root_and_stays_out_of_tree() {
    let db = Db::new().unwrap();
    let raw = db.raw().unwrap();
    workspace_row(&raw, 1, &id(WS, 1), "Работа", 0).unwrap();
    project_row(&raw, 1, &id(PR, 1), &id(WS, 1), "old", None).unwrap();
    raw.execute(
        "UPDATE projects SET removed_at = ?1 WHERE rowid = 1",
        [NOW.0 - 1],
    )
    .unwrap();
    root_row(&raw, 1, &id(RT, 1), &id(PR, 1), "/src/app", "git").unwrap();
    set_window(&raw, Some(&id(WS, 1)), None).unwrap();
    set_revision(&raw, 3).unwrap();
    drop(raw);

    let resolver = FakeRootResolver::new().with_git("/src/app-copy", "/src/app/.git");
    let core = db.start(resolver).unwrap();
    let error = core
        .request(Command::ProjectAdd {
            workspace: workspace_id(1),
            path: PathBuf::from("/src/app-copy"),
            title: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::RootTaken, "{error}");
    assert_eq!(
        error.detail_str("project"),
        Some(project_id(1).to_string().as_str())
    );
    assert_eq!(
        error.detail_str("workspace"),
        Some(workspace_id(1).to_string().as_str())
    );

    let tree = core
        .query(Query::Tree {
            scope: TreeScope::All,
        })
        .await
        .unwrap();
    assert_eq!(tree.revision, Revision(3));
    let ReplyValue::Tree(tree) = tree.value else {
        panic!("неожиданный ответ {tree:?}");
    };
    assert_eq!(tree.workspaces.len(), 1);
    assert_eq!(tree.workspaces[0].id, workspace_id(1));
    assert!(tree.workspaces[0].projects.is_empty());
    let (snapshot, _) = core.subscribe().await.unwrap();
    assert!(snapshot.state.projects[&project_id(1)].is_removed());
    core.shutdown().await;
    assert_eq!(db.revision_in_db().unwrap(), 3);
}

#[tokio::test]
async fn fixture_v1_migrates_and_start_gives_default_layouts_once() {
    let db = Db::fixture_v1().unwrap();
    let core = db.start(FakeRootResolver::new()).unwrap();
    let (snapshot, _) = core.subscribe().await.unwrap();
    assert_eq!(snapshot.revision, Revision(13));
    let state = &snapshot.state;
    assert_eq!(state.diagnostics, Vec::new());
    assert_eq!(state.check_invariants(), Vec::new());
    assert_eq!(keys(&state.layouts), keys(&state.workbenches));
    assert_eq!(state.panels.len(), 6);

    // Открытая рабочая область образца получила раскладку без единой команды.
    let Route::Workbench(open) = state.window.route else {
        panic!("образец открыт на рабочей области");
    };
    let layout = &state.layouts[&open];
    let navigator = layout.navigator.unwrap();
    let terminal = layout.focused().unwrap();
    assert_eq!(*layout, WorkbenchLayout::default_for(navigator, terminal));
    assert_eq!(state.panels[&navigator].plugin.as_str(), NAVIGATOR_PLUGIN);
    assert_eq!(state.panels[&terminal].plugin.as_str(), TERMINAL_PLUGIN);
    assert_eq!(state.panels[&terminal].workbench, open);
    assert_eq!(state.panels[&terminal].created_at, NOW);

    let EventsSince::Events(events) = core.events_since(Revision(12)).await.unwrap() else {
        panic!("ожидалось событие запуска");
    };
    let [event] = events.as_slice() else {
        panic!("одна запись запуска: {events:?}");
    };
    let adds = event
        .changes
        .iter()
        .filter(|change| matches!(change, Change::PanelAdd { .. }))
        .count();
    let sets = event
        .changes
        .iter()
        .filter(|change| matches!(change, Change::LayoutSet { .. }))
        .count();
    assert_eq!((adds, sets, event.changes.len()), (6, 3, 9));
    core.shutdown().await;
    assert_eq!(db.revision_in_db().unwrap(), 13);
    assert_eq!(db.count("panels").unwrap(), 6);
    assert_eq!(db.count("workbench_layouts").unwrap(), 3);

    // Второй запуск находит раскладки и ничего не пишет: `PanelId` прежние.
    let core = db.start(FakeRootResolver::new()).unwrap();
    let (again, _) = core.subscribe().await.unwrap();
    assert_eq!(again, snapshot);
    core.shutdown().await;
    assert_eq!(db.revision_in_db().unwrap(), 13);
}

#[test]
fn panel_rows_are_checked_like_other_rows() {
    let db = Db::new().unwrap();
    let raw = db.raw().unwrap();
    workspace_row(&raw, 1, &id(WS, 1), "Работа", 0).unwrap();
    project_row(&raw, 1, &id(PR, 1), &id(WS, 1), "shellio", Some(0)).unwrap();
    workbench_row(&raw, 1, &id(WB, 1), &id(PR, 1), "main", 0).unwrap();
    let wb = id(WB, 1);
    panel_row(&raw, 1, &id(PN, 1), &wb, "navigator", "Навигатор", 1).unwrap();
    panel_row(&raw, 2, &id(PN, 2)[..15], &wb, "terminal", "Терминал", 1).unwrap();
    panel_row(&raw, 3, &id(PN, 3), &id(WB, 9), "terminal", "Терминал", 1).unwrap();
    panel_row(&raw, 4, &id(PN, 4), &wb, "Terminal", "Терминал", 1).unwrap();
    panel_row(&raw, 5, &id(PN, 5), &wb, "terminal", " ", 1).unwrap();
    panel_row(&raw, 6, &id(PN, 6), &wb, "terminal", "  Терминал ", 1).unwrap();
    panel_row(&raw, 7, &id(PN, 7), &wb[..15], "terminal", "Терминал", 1).unwrap();
    panel_row(&raw, 8, &id(PN, 8), &wb, "", "Пусто", 1).unwrap();
    drop(raw);
    let before = db.bytes().unwrap();

    let loaded = db.load().unwrap();
    assert_eq!(
        loaded.state.diagnostics,
        [
            diag("panels", 2, "invalid_id"),
            diag("panels", 3, "owner_missing"),
            diag("panels", 4, "invalid_value"),
            diag("panels", 5, "title_empty"),
            diag("panels", 6, "title_trimmed"),
            diag("panels", 7, "owner_missing"),
            diag("panels", 8, "invalid_value"),
            diag("workbenches", 1, "layout_missing"),
        ]
    );
    assert_eq!(
        keys(&loaded.state.panels),
        BTreeSet::from([panel_id(1), panel_id(6)])
    );
    assert_eq!(loaded.state.panels[&panel_id(6)].title.as_str(), "Терминал");
    assert_eq!(loaded.state.layouts.len(), 0);
    assert_eq!(db.bytes().unwrap(), before, "загрузка не пишет в базу");
}

#[test]
fn layout_row_without_loaded_workbench_is_owner_missing() {
    let db = Db::new().unwrap();
    let raw = db.raw().unwrap();
    let layout = WorkbenchLayout::default_for(panel_id(1), panel_id(2));
    layout_row(&raw, 1, &id(WB, 9), &envelope(1, &layout).unwrap()).unwrap();
    layout_row(&raw, 2, &id(WB, 8)[..15], &envelope(1, &layout).unwrap()).unwrap();
    drop(raw);
    let loaded = db.load().unwrap();
    assert_eq!(
        loaded.state.diagnostics,
        [
            diag("workbench_layouts", 1, "owner_missing"),
            diag("workbench_layouts", 2, "owner_missing"),
        ]
    );
    assert_eq!(loaded.state.layouts.len(), 0);
}

#[tokio::test]
async fn broken_layouts_are_found_and_rebuilt_from_panels_on_start() {
    let db = Db::new().unwrap();
    let raw = db.raw().unwrap();
    workspace_row(&raw, 1, &id(WS, 1), "Работа", 0).unwrap();
    project_row(&raw, 1, &id(PR, 1), &id(WS, 1), "shellio", Some(0)).unwrap();
    for n in 1..=9u8 {
        workbench_row(
            &raw,
            i64::from(n),
            &id(WB, n),
            &id(PR, 1),
            &format!("wb{n}"),
            n.into(),
        )
        .unwrap();
    }
    set_window(&raw, Some(&id(WS, 1)), Some(&id(WB, 1))).unwrap();
    set_revision(&raw, 7).unwrap();

    // У каждой рабочей области `n`, кроме 7 и 8, — навигатор 10n+1 и терминал 10n+2.
    let mut rowid = 0;
    let mut panel = |n: u8, k: u8, plugin: &str, created_at: i64| {
        rowid += 1;
        panel_row(
            &raw,
            rowid,
            &id(PN, 10 * n + k),
            &id(WB, n),
            plugin,
            plugin,
            created_at,
        )
        .unwrap();
        panel_id(10 * n + k)
    };
    let mut pairs = Vec::new();
    for n in [1, 2, 3, 4, 5, 6, 9] {
        pairs.push((panel(n, 1, "navigator", 10), panel(n, 2, "terminal", 20)));
    }
    // Ни одного навигатора, одинаковое время: порядок по `PanelId`.
    let lone_b = panel(7, 2, "terminal", 30);
    let lone_a = panel(7, 1, "notes", 30);
    // Два навигатора с одинаковым временем и терминал.
    let terminal_8 = panel(8, 3, "terminal", 5);
    let second_nav = panel(8, 2, "navigator", 10);
    let first_nav = panel(8, 1, "navigator", 10);

    let default = |n: usize| WorkbenchLayout::default_for(pairs[n].0, pairs[n].1);
    let mut active = default(2);
    active.zones.center.parts[0].active = 5;
    let mut last_active = default(3);
    last_active.zones.center.last_active = 3;
    let mut missing = default(4);
    missing.zones.center.parts.clear();
    missing.focus = vec![pairs[4].0];
    let mut extra = default(5);
    extra.zones.center.parts[0].tabs.push(panel_id(99));
    let mut kept = default(6);
    kept.sizes.bottom = 300;
    kept.hidden.navigator = true;

    layout_row(&raw, 1, &id(WB, 1), "не JSON").unwrap();
    layout_row(&raw, 2, &id(WB, 2), &envelope(2, &default(1)).unwrap()).unwrap();
    layout_row(&raw, 3, &id(WB, 3), &envelope(1, &active).unwrap()).unwrap();
    layout_row(&raw, 4, &id(WB, 4), &envelope(1, &last_active).unwrap()).unwrap();
    layout_row(&raw, 5, &id(WB, 5), &envelope(1, &missing).unwrap()).unwrap();
    layout_row(&raw, 6, &id(WB, 6), &envelope(1, &extra).unwrap()).unwrap();
    layout_row(&raw, 8, &id(WB, 8), "{}").unwrap();
    layout_row(&raw, 9, &id(WB, 9), &envelope(1, &kept).unwrap()).unwrap();
    drop(raw);
    let before = db.bytes().unwrap();

    let findings = [
        diag("workbench_layouts", 1, "layout_unreadable"),
        diag("workbench_layouts", 2, "layout_unreadable"),
        diag("workbench_layouts", 3, "layout_invalid"),
        diag("workbench_layouts", 4, "layout_invalid"),
        diag("workbench_layouts", 5, "layout_invalid"),
        diag("workbench_layouts", 6, "layout_invalid"),
        diag("workbench_layouts", 8, "layout_unreadable"),
        diag("workbenches", 7, "layout_missing"),
    ];
    let loaded = db.load().unwrap();
    assert_eq!(loaded.state.diagnostics, findings);
    assert_eq!(
        keys(&loaded.state.layouts),
        BTreeSet::from([workbench_id(9)])
    );
    assert_eq!(loaded.state.panels.len(), 19);
    assert_eq!(db.bytes().unwrap(), before, "загрузка не пишет в базу");

    let core = db.start(FakeRootResolver::new()).unwrap();
    let (snapshot, _) = core.subscribe().await.unwrap();
    assert_eq!(snapshot.revision, Revision(8));
    assert_eq!(
        core.events_since(Revision(7)).await.unwrap(),
        EventsSince::Reset(snapshot.clone()),
        "исправление пишет запись запуска без события"
    );
    let status = core.query(Query::Status {}).await.unwrap();
    let ReplyValue::Status(status) = status.value else {
        panic!("неожиданный ответ {status:?}");
    };
    assert_eq!(status.diagnostics, findings);

    let state = &snapshot.state;
    assert_eq!(state.check_invariants(), Vec::new());
    assert_eq!(state.panels.len(), 19, "восстановление не создаёт панелей");
    for (n, &(navigator, terminal)) in pairs.iter().enumerate().take(6) {
        let workbench = workbench_id(u8::try_from(n + 1).unwrap());
        assert_eq!(
            state.layouts[&workbench],
            WorkbenchLayout::default_for(navigator, terminal),
            "{workbench}"
        );
    }
    let recovered = |navigator: Option<PanelId>, center: Vec<PanelId>| {
        let mut layout = WorkbenchLayout::default_for(center[0], center[0]);
        layout.navigator = navigator;
        layout.zones.center.parts[0].tabs = center;
        layout
    };
    assert_eq!(
        state.layouts[&workbench_id(7)],
        recovered(None, vec![lone_a, lone_b])
    );
    assert_eq!(
        state.layouts[&workbench_id(8)],
        recovered(Some(first_nav), vec![terminal_8, second_nav])
    );
    assert_eq!(state.layouts[&workbench_id(9)], kept);

    // Первое изменение после восстановления проходит, повторная загрузка чиста.
    let renamed = core
        .request(Command::WorkbenchRename {
            workbench: workbench_id(1),
            title: "Переименована".into(),
        })
        .await
        .unwrap();
    assert_eq!(renamed.revision, Revision(9));
    let (after, _) = core.subscribe().await.unwrap();
    core.shutdown().await;

    let reloaded = db.load().unwrap();
    assert_eq!(reloaded.revision, Revision(9));
    assert_eq!(reloaded.state.diagnostics, Vec::new());
    assert_eq!(reloaded.state.layouts, after.state.layouts);
    assert_eq!(
        State {
            diagnostics: Vec::new(),
            ..reloaded.state.clone()
        },
        State {
            diagnostics: Vec::new(),
            ..(*after.state).clone()
        }
    );

    let core = db.start(FakeRootResolver::new()).unwrap();
    let (again, _) = core.subscribe().await.unwrap();
    assert_eq!(again.revision, Revision(9), "чистая база ничего не пишет");
    assert_eq!(again.state.layouts, after.state.layouts);
    core.shutdown().await;
}
