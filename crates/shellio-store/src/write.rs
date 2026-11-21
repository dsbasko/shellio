//! Запись дельт ядра в `state.db` одной транзакцией вместе с ревизией (ADR-0018).

use std::fmt;

use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Params, Transaction, TransactionBehavior,
};
use shellio_core::{
    Change, CommitError, Project, ProjectRoot, Revision, Route, Workbench, Workspace,
};

/// Записать `changes` поверх ревизии `base`: проверка `meta.revision == base`, SQL каждой
/// дельты, `meta.revision = base + 1`. Любой отказ откатывает транзакцию целиком.
pub(crate) fn commit(
    conn: &mut Connection,
    base: Revision,
    changes: &[Change],
) -> Result<(), CommitError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(failed)?;
    let stored: Option<i64> = tx
        .query_row("SELECT value FROM meta WHERE key = 'revision'", [], |row| {
            row.get(0)
        })
        .optional()
        .map_err(failed)?;
    if stored != Some(sql_revision(base)?) {
        let stored = stored.map_or_else(|| "нет".to_owned(), |value| value.to_string());
        return Err(CommitError::Conflict(format!(
            "ревизия state.db {stored} не совпадает с ревизией ядра {}: базу правили в обход ядра",
            base.0
        )));
    }
    for change in changes {
        write_change(&tx, change)?;
    }
    update_one(
        &tx,
        "meta",
        "revision",
        "UPDATE meta SET value = ?1 WHERE key = 'revision'",
        [sql_revision(base.next())?],
    )?;
    tx.commit().map_err(failed)
}

/// SQL дельты. Исчерпывающий `match` без `_`: новая дельта не соберётся без своего SQL.
fn write_change(tx: &Transaction<'_>, change: &Change) -> Result<(), CommitError> {
    match change {
        Change::WorkspaceInserted { workspace } => insert_workspace(tx, workspace),
        Change::WorkspaceRenamed { workspace, title } => update_one(
            tx,
            "workspaces",
            workspace,
            "UPDATE workspaces SET title = ?2 WHERE id = ?1",
            (workspace.as_bytes().as_slice(), title.as_str()),
        ),
        Change::WorkspaceLastWorkbenchSet {
            workspace,
            workbench,
        } => update_one(
            tx,
            "workspaces",
            workspace,
            "UPDATE workspaces SET last_workbench_id = ?2 WHERE id = ?1",
            (
                workspace.as_bytes().as_slice(),
                workbench.as_ref().map(|id| id.as_bytes().as_slice()),
            ),
        ),
        Change::ProjectInserted { project, roots } => {
            insert_project(tx, project)?;
            roots.iter().try_for_each(|root| insert_root(tx, root))
        }
        Change::ProjectOpened { project, at } => update_one(
            tx,
            "projects",
            project,
            "UPDATE projects SET last_opened_at = ?2 WHERE id = ?1",
            (project.as_bytes().as_slice(), at.0),
        ),
        Change::WorkbenchInserted { workbench } => insert_workbench(tx, workbench),
        Change::WorkbenchRenamed { workbench, title } => update_one(
            tx,
            "workbenches",
            workbench,
            "UPDATE workbenches SET title = ?2 WHERE id = ?1",
            (workbench.as_bytes().as_slice(), title.as_str()),
        ),
        Change::WindowSet { active, route } => {
            let route = match route {
                Route::WorkspaceHome => None,
                Route::Workbench(workbench) => Some(workbench.as_bytes().as_slice()),
            };
            // Строка окна единственная: запись восстанавливает её, если её удалили снаружи.
            exec(
                tx,
                "INSERT INTO window_state (id, active_workspace_id, route_workbench_id) \
                 VALUES (1, ?1, ?2) \
                 ON CONFLICT (id) DO UPDATE SET \
                 active_workspace_id = excluded.active_workspace_id, \
                 route_workbench_id = excluded.route_workbench_id",
                (active.as_bytes().as_slice(), route),
            )
            .map(drop)
        }
    }
}

fn insert_workspace(tx: &Transaction<'_>, workspace: &Workspace) -> Result<(), CommitError> {
    exec(
        tx,
        "INSERT INTO workspaces (id, title, position, last_workbench_id, deleted_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            workspace.id.as_bytes().as_slice(),
            workspace.title.as_str(),
            i64::from(workspace.position),
            workspace
                .last_workbench
                .as_ref()
                .map(|id| id.as_bytes().as_slice()),
            workspace.deleted_at.map(|at| at.0),
        ),
    )
    .map(drop)
}

fn insert_project(tx: &Transaction<'_>, project: &Project) -> Result<(), CommitError> {
    exec(
        tx,
        "INSERT INTO projects \
         (id, workspace_id, title, list_position, removed_at, last_opened_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            project.id.as_bytes().as_slice(),
            project.workspace.as_bytes().as_slice(),
            project.title.as_str(),
            project.list_position.map(i64::from),
            project.removed_at.map(|at| at.0),
            project.last_opened_at.map(|at| at.0),
        ),
    )
    .map(drop)
}

fn insert_root(tx: &Transaction<'_>, root: &ProjectRoot) -> Result<(), CommitError> {
    let path = root.path.to_str().ok_or_else(|| {
        CommitError::Conflict(format!(
            "путь корня {} не в UTF-8, state.db хранит его строкой",
            root.path.display()
        ))
    })?;
    exec(
        tx,
        "INSERT INTO project_roots (id, project_id, path, kind, key) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            root.id.as_bytes().as_slice(),
            root.project.as_bytes().as_slice(),
            path,
            root.kind.as_str(),
            root.key.as_str(),
        ),
    )
    .map(drop)
}

fn insert_workbench(tx: &Transaction<'_>, workbench: &Workbench) -> Result<(), CommitError> {
    exec(
        tx,
        "INSERT INTO workbenches (id, project_id, title, position) VALUES (?1, ?2, ?3, ?4)",
        (
            workbench.id.as_bytes().as_slice(),
            workbench.project.as_bytes().as_slice(),
            workbench.title.as_str(),
            i64::from(workbench.position),
        ),
    )
    .map(drop)
}

/// Изменение существующей записи: ровно одна строка. Ни одной — запись удалили в обход
/// ядра, состояние ядра разошлось с базой.
fn update_one(
    tx: &Transaction<'_>,
    table: &str,
    id: impl fmt::Display,
    sql: &str,
    params: impl Params,
) -> Result<(), CommitError> {
    match exec(tx, sql, params)? {
        1 => Ok(()),
        changed => Err(CommitError::Conflict(format!(
            "изменение {table} {id} затронуло строк: {changed}, ожидалась одна: \
             базу правили в обход ядра"
        ))),
    }
}

/// Один SQL-оператор дельты.
fn exec(tx: &Transaction<'_>, sql: &str, params: impl Params) -> Result<usize, CommitError> {
    tx.execute(sql, params).map_err(failed)
}

fn sql_revision(revision: Revision) -> Result<i64, CommitError> {
    i64::try_from(revision.0).map_err(|_| {
        CommitError::Conflict(format!("ревизия {} не помещается в state.db", revision.0))
    })
}

/// Нарушение ограничения (`UNIQUE`, внешний или первичный ключ) — база разошлась с ядром,
/// повтор не поможет. Остальное — отказ записи с возможностью повтора.
fn failed(error: rusqlite::Error) -> CommitError {
    if error.sqlite_error_code() == Some(ErrorCode::ConstraintViolation) {
        CommitError::Conflict(format!("запись нарушает ограничение state.db: {error}"))
    } else {
        CommitError::Unavailable(format!("запись в state.db не удалась: {error}"))
    }
}
