//! Запись дельт ядра в `state.db` одной транзакцией вместе с ревизией (ADR-0018).

use std::fmt;

use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Params, Transaction, TransactionBehavior,
};
use shellio_core::{
    Change, CommitError, Panel, Project, ProjectRoot, Revision, Route, Workbench, WorkbenchId,
    WorkbenchLayout, Workspace,
};

use crate::layout_json;

/// Записать `changes` поверх ревизии `base`: проверка `meta.revision == base`, SQL каждой
/// дельты, новый отпечаток реестра плагинов `fingerprint` записи запуска, `meta.revision =
/// base + 1`. Любой отказ откатывает транзакцию целиком.
///
/// `stop_after = Some(n)` прерывает запись отказом `Unavailable`, когда выполнено `n`
/// SQL-операторов дельт, отпечатка и ревизии: так тестовый крючок сбоя рвёт транзакцию
/// посередине. В продукте — `None`.
pub(crate) fn commit(
    conn: &mut Connection,
    base: Revision,
    changes: &[Change],
    fingerprint: Option<&str>,
    stop_after: Option<usize>,
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
    let mut tx = Writer {
        tx,
        executed: 0,
        stop_after,
    };
    for change in changes {
        write_change(&mut tx, change)?;
    }
    if let Some(fingerprint) = fingerprint {
        // Строка отпечатка единственная, как строка окна.
        tx.exec(
            "INSERT INTO plugin_registry (id, fingerprint) VALUES (1, ?1) \
             ON CONFLICT (id) DO UPDATE SET fingerprint = excluded.fingerprint",
            [fingerprint],
        )?;
    }
    update_one(
        &mut tx,
        "meta",
        "revision",
        "UPDATE meta SET value = ?1 WHERE key = 'revision'",
        [sql_revision(base.next())?],
    )?;
    tx.tx.commit().map_err(failed)
}

/// Транзакция записи со счётом выполненных операторов. Брошенная без `commit` откатывается.
struct Writer<'c> {
    tx: Transaction<'c>,
    executed: usize,
    stop_after: Option<usize>,
}

impl Writer<'_> {
    /// Один SQL-оператор записи.
    fn exec(&mut self, sql: &str, params: impl Params) -> Result<usize, CommitError> {
        if self.stop_after == Some(self.executed) {
            return Err(CommitError::Unavailable(format!(
                "тестовый сбой: запись прервана после операторов: {}",
                self.executed
            )));
        }
        let changed = self.tx.execute(sql, params).map_err(failed)?;
        self.executed += 1;
        Ok(changed)
    }
}

/// SQL дельты. Исчерпывающий `match` без `_`: новая дельта не соберётся без своего SQL.
fn write_change(tx: &mut Writer<'_>, change: &Change) -> Result<(), CommitError> {
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
            tx.exec(
                "INSERT INTO window_state (id, active_workspace_id, route_workbench_id) \
                 VALUES (1, ?1, ?2) \
                 ON CONFLICT (id) DO UPDATE SET \
                 active_workspace_id = excluded.active_workspace_id, \
                 route_workbench_id = excluded.route_workbench_id",
                (active.as_bytes().as_slice(), route),
            )
            .map(drop)
        }
        Change::PanelAdd { panel } => insert_panel(tx, panel),
        // Строку данных панели удаляет `ON DELETE CASCADE`.
        Change::PanelRemove { panel } => update_one(
            tx,
            "panels",
            panel,
            "DELETE FROM panels WHERE id = ?1",
            [panel.as_bytes().as_slice()],
        ),
        Change::PanelRehome { panel, workbench } => update_one(
            tx,
            "panels",
            panel,
            "UPDATE panels SET workbench = ?2 WHERE id = ?1",
            (panel.as_bytes().as_slice(), workbench.as_bytes().as_slice()),
        ),
        // Данные пишутся целиком вставкой с обновлением: строки могло не быть.
        Change::PanelData {
            panel,
            version,
            data,
        } => tx
            .exec(
                "INSERT INTO panel_states (panel, version, data) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (panel) DO UPDATE SET \
                 version = excluded.version, data = excluded.data",
                (
                    panel.as_bytes().as_slice(),
                    i64::from(*version),
                    data.as_str(),
                ),
            )
            .map(drop),
        Change::PanelOpenedFrom { panel, opened_from } => update_one(
            tx,
            "panels",
            panel,
            "UPDATE panels SET opened_from = ?2 WHERE id = ?1",
            (
                panel.as_bytes().as_slice(),
                opened_from.as_ref().map(|id| id.as_bytes().as_slice()),
            ),
        ),
        // Статус — производное значение: в базе его нет, оператора нет.
        Change::PanelStatus { .. } => Ok(()),
        Change::LayoutSet { workbench, layout } => set_layout(tx, *workbench, layout),
    }
}

fn insert_panel(tx: &mut Writer<'_>, panel: &Panel) -> Result<(), CommitError> {
    tx.exec(
        "INSERT INTO panels (id, workbench, plugin, title, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            panel.id.as_bytes().as_slice(),
            panel.workbench.as_bytes().as_slice(),
            panel.plugin.as_str(),
            panel.title.as_str(),
            panel.created_at.0,
        ),
    )
    .map(drop)
}

/// Раскладка пишется целиком вставкой с обновлением, как состояние окна.
fn set_layout(
    tx: &mut Writer<'_>,
    workbench: WorkbenchId,
    layout: &WorkbenchLayout,
) -> Result<(), CommitError> {
    let text = layout_json::encode(layout).map_err(|error| {
        CommitError::Conflict(format!(
            "раскладка рабочей области {workbench} не сериализуется: {error}"
        ))
    })?;
    tx.exec(
        "INSERT INTO workbench_layouts (workbench, layout) VALUES (?1, ?2) \
         ON CONFLICT (workbench) DO UPDATE SET layout = excluded.layout",
        (workbench.as_bytes().as_slice(), text),
    )
    .map(drop)
}

fn insert_workspace(tx: &mut Writer<'_>, workspace: &Workspace) -> Result<(), CommitError> {
    tx.exec(
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

fn insert_project(tx: &mut Writer<'_>, project: &Project) -> Result<(), CommitError> {
    tx.exec(
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

fn insert_root(tx: &mut Writer<'_>, root: &ProjectRoot) -> Result<(), CommitError> {
    let path = root.path.to_str().ok_or_else(|| {
        CommitError::Conflict(format!(
            "путь корня {} не в UTF-8, state.db хранит его строкой",
            root.path.display()
        ))
    })?;
    tx.exec(
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

fn insert_workbench(tx: &mut Writer<'_>, workbench: &Workbench) -> Result<(), CommitError> {
    tx.exec(
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
    tx: &mut Writer<'_>,
    table: &str,
    id: impl fmt::Display,
    sql: &str,
    params: impl Params,
) -> Result<(), CommitError> {
    match tx.exec(sql, params)? {
        1 => Ok(()),
        changed => Err(CommitError::Conflict(format!(
            "изменение {table} {id} затронуло строк: {changed}, ожидалась одна: \
             базу правили в обход ядра"
        ))),
    }
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
