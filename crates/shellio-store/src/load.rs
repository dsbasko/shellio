//! Построчная загрузка `state.db` в состояние ядра с диагностикой (ADR-0018).
//!
//! Загрузка ничего не пишет. Повреждённая строка пропускается вместе с зависимыми, висячая
//! мягкая ссылка сбрасывается только в памяти, совпадения позиций и имён лишь сообщаются.
//! Раскладка с находкой в память не попадает: недостающие раскладки дописывает запуск ядра.
//! Каждая находка попадает в `State::diagnostics` как `{table, rowid, reason}`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row};
use shellio_core::{
    Diagnostic, Panel, PanelId, PluginId, Project, ProjectId, ProjectRoot, ProjectRootId, Revision,
    RootKey, RootKind, Route, State, Title, UnixMillis, WindowState, Workbench, WorkbenchId,
    Workspace, WorkspaceId,
};

use crate::layout_json;
use crate::open::StoreError;

/// Стабильная причина находки: уходит в `query.status`.
type Reason = &'static str;

// Строка пропущена.
const INVALID_ID: Reason = "invalid_id";
const INVALID_VALUE: Reason = "invalid_value";
const POSITION_OUT_OF_RANGE: Reason = "position_out_of_range";
const UNKNOWN_KIND: Reason = "unknown_kind";
const OWNER_MISSING: Reason = "owner_missing";
// Строка загружена, значение в памяти исправлено.
const TITLE_TRIMMED: Reason = "title_trimmed";
const LAST_WORKBENCH_MISSING: Reason = "last_workbench_missing";
const LAST_WORKBENCH_FOREIGN: Reason = "last_workbench_foreign";
const ACTIVE_WORKSPACE_MISSING: Reason = "active_workspace_missing";
const ROUTE_WORKBENCH_MISSING: Reason = "route_workbench_missing";
const ROUTE_WORKBENCH_FOREIGN: Reason = "route_workbench_foreign";
const ROW_MISSING: Reason = "row_missing";
// Только сообщение.
const POSITION_TAKEN: Reason = "position_taken";
const NAME_TAKEN: Reason = "name_taken";
// Раскладка не загружена, запуск ядра строит её заново.
const LAYOUT_UNREADABLE: Reason = "layout_unreadable";
const LAYOUT_INVALID: Reason = "layout_invalid";
const LAYOUT_MISSING: Reason = "layout_missing";

const WORKSPACES: &str = "workspaces";
const PROJECTS: &str = "projects";
const PROJECT_ROOTS: &str = "project_roots";
const WORKBENCHES: &str = "workbenches";
const WINDOW_STATE: &str = "window_state";
const PANELS: &str = "panels";
const WORKBENCH_LAYOUTS: &str = "workbench_layouts";
/// `window_state.id` — псевдоним `rowid`, строка единственная.
const WINDOW_ROWID: i64 = 1;

/// Загрузить состояние и ревизию одной читающей транзакцией.
pub(crate) fn load(conn: &Connection, path: &Path) -> Result<(State, Revision), StoreError> {
    let sqlite = |source| StoreError::Sqlite {
        path: path.to_path_buf(),
        source,
    };
    let tx = conn.unchecked_transaction().map_err(sqlite)?;
    let revision = revision(&tx, path)?;
    let mut loader = Loader::default();
    loader.workspaces(&tx).map_err(sqlite)?;
    loader.projects(&tx).map_err(sqlite)?;
    loader.roots(&tx).map_err(sqlite)?;
    loader.workbenches(&tx).map_err(sqlite)?;
    loader.last_workbenches();
    loader.window(&tx).map_err(sqlite)?;
    loader.position_collisions();
    loader.name_collisions();
    loader.panels(&tx).map_err(sqlite)?;
    loader.layouts(&tx).map_err(sqlite)?;
    loader.missing_layouts();
    tx.commit().map_err(sqlite)?;
    Ok((loader.finish(), revision))
}

fn revision(conn: &Connection, path: &Path) -> Result<Revision, StoreError> {
    let corrupt = |detail: String| StoreError::Corrupt {
        path: path.to_path_buf(),
        detail,
    };
    let value: Option<i64> = conn
        .query_row("SELECT value FROM meta WHERE key = 'revision'", [], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|source| StoreError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    match value {
        None => Err(corrupt("в таблице meta нет строки revision".to_owned())),
        Some(value) => u64::try_from(value)
            .map(Revision)
            .map_err(|_| corrupt(format!("отрицательная ревизия {value}"))),
    }
}

/// Мягкая ссылка до проверки: она может висеть, тогда сбрасывается.
#[derive(Clone, Copy)]
enum SoftRef {
    Unset,
    /// Не 16 байт: на запись такая ссылка указывать не может.
    Invalid,
    Id([u8; 16]),
}

#[derive(Default)]
struct Loader {
    state: State,
    diagnostics: Vec<Diagnostic>,
    workspace_rows: BTreeMap<WorkspaceId, i64>,
    project_rows: BTreeMap<ProjectId, i64>,
    workbench_rows: BTreeMap<WorkbenchId, i64>,
    /// `last_workbench_id` загруженных воркспейсов: проверяется после рабочих областей.
    last_workbenches: Vec<(i64, WorkspaceId, SoftRef)>,
    /// Рабочие области, у которых есть строка раскладки, даже испорченная.
    layout_rows: BTreeSet<WorkbenchId>,
}

impl Loader {
    fn note(&mut self, table: &str, rowid: i64, reason: Reason) {
        self.diagnostics.push(Diagnostic {
            table: table.to_owned(),
            rowid,
            reason: reason.to_owned(),
        });
    }

    fn workspaces(&mut self, conn: &Connection) -> rusqlite::Result<()> {
        let mut stmt = conn.prepare(
            "SELECT rowid, id, title, position, last_workbench_id, deleted_at \
             FROM workspaces ORDER BY rowid",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let rowid: i64 = row.get(0)?;
            let parsed = (|| -> Result<_, Reason> {
                let id = WorkspaceId::from_bytes(id(row, 1)?);
                let (title, trimmed) = title(row, 2)?;
                let workspace = Workspace {
                    id,
                    title,
                    position: position(row, 3)?,
                    last_workbench: None,
                    deleted_at: millis(row, 5)?,
                };
                Ok((workspace, trimmed, soft_ref(row, 4)))
            })();
            match parsed {
                Ok((workspace, trimmed, last)) => {
                    if trimmed {
                        self.note(WORKSPACES, rowid, TITLE_TRIMMED);
                    }
                    self.workspace_rows.insert(workspace.id, rowid);
                    self.last_workbenches.push((rowid, workspace.id, last));
                    self.state.workspaces.insert(workspace.id, workspace);
                }
                Err(reason) => self.note(WORKSPACES, rowid, reason),
            }
        }
        Ok(())
    }

    fn projects(&mut self, conn: &Connection) -> rusqlite::Result<()> {
        let mut stmt = conn.prepare(
            "SELECT rowid, id, workspace_id, title, list_position, removed_at, last_opened_at \
             FROM projects ORDER BY rowid",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let rowid: i64 = row.get(0)?;
            let parsed = (|| -> Result<_, Reason> {
                let id = ProjectId::from_bytes(id(row, 1)?);
                let workspace = owner(row, 2)
                    .map(WorkspaceId::from_bytes)
                    .filter(|workspace| self.state.workspaces.contains_key(workspace))
                    .ok_or(OWNER_MISSING)?;
                let (title, trimmed) = title(row, 3)?;
                let project = Project {
                    id,
                    workspace,
                    title,
                    list_position: optional_position(row, 4)?,
                    removed_at: millis(row, 5)?,
                    last_opened_at: millis(row, 6)?,
                };
                Ok((project, trimmed))
            })();
            match parsed {
                Ok((project, trimmed)) => {
                    if trimmed {
                        self.note(PROJECTS, rowid, TITLE_TRIMMED);
                    }
                    self.project_rows.insert(project.id, rowid);
                    self.state.projects.insert(project.id, project);
                }
                Err(reason) => self.note(PROJECTS, rowid, reason),
            }
        }
        Ok(())
    }

    fn roots(&mut self, conn: &Connection) -> rusqlite::Result<()> {
        let mut stmt = conn.prepare(
            "SELECT rowid, id, project_id, path, kind, key FROM project_roots ORDER BY rowid",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let rowid: i64 = row.get(0)?;
            let parsed = (|| -> Result<_, Reason> {
                let id = ProjectRootId::from_bytes(id(row, 1)?);
                let project = owner(row, 2)
                    .map(ProjectId::from_bytes)
                    .filter(|project| self.state.projects.contains_key(project))
                    .ok_or(OWNER_MISSING)?;
                let path: String = row.get(3).map_err(|_| INVALID_VALUE)?;
                let kind: String = row.get(4).map_err(|_| INVALID_VALUE)?;
                let key: String = row.get(5).map_err(|_| INVALID_VALUE)?;
                Ok(ProjectRoot {
                    id,
                    project,
                    path: PathBuf::from(path),
                    kind: RootKind::parse(&kind).ok_or(UNKNOWN_KIND)?,
                    key: RootKey::new(key),
                })
            })();
            match parsed {
                Ok(root) => {
                    self.state.roots.insert(root.id, root);
                }
                Err(reason) => self.note(PROJECT_ROOTS, rowid, reason),
            }
        }
        Ok(())
    }

    fn workbenches(&mut self, conn: &Connection) -> rusqlite::Result<()> {
        let mut stmt = conn.prepare(
            "SELECT rowid, id, project_id, title, position FROM workbenches ORDER BY rowid",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let rowid: i64 = row.get(0)?;
            let parsed = (|| -> Result<_, Reason> {
                let id = WorkbenchId::from_bytes(id(row, 1)?);
                let project = owner(row, 2)
                    .map(ProjectId::from_bytes)
                    .filter(|project| self.state.projects.contains_key(project))
                    .ok_or(OWNER_MISSING)?;
                let (title, trimmed) = title(row, 3)?;
                let workbench = Workbench {
                    id,
                    project,
                    title,
                    position: position(row, 4)?,
                };
                Ok((workbench, trimmed))
            })();
            match parsed {
                Ok((workbench, trimmed)) => {
                    if trimmed {
                        self.note(WORKBENCHES, rowid, TITLE_TRIMMED);
                    }
                    self.workbench_rows.insert(workbench.id, rowid);
                    self.state.workbenches.insert(workbench.id, workbench);
                }
                Err(reason) => self.note(WORKBENCHES, rowid, reason),
            }
        }
        Ok(())
    }

    /// Последняя рабочая область воркспейса должна существовать и принадлежать ему.
    fn last_workbenches(&mut self) {
        for (rowid, workspace, last) in std::mem::take(&mut self.last_workbenches) {
            let resolved = match last {
                SoftRef::Unset => None,
                SoftRef::Invalid => {
                    self.note(WORKSPACES, rowid, LAST_WORKBENCH_MISSING);
                    None
                }
                SoftRef::Id(bytes) => {
                    let workbench = WorkbenchId::from_bytes(bytes);
                    match self.state.workbench_workspace(workbench) {
                        Some(owner) if owner == workspace => Some(workbench),
                        Some(_) => {
                            self.note(WORKSPACES, rowid, LAST_WORKBENCH_FOREIGN);
                            None
                        }
                        None => {
                            self.note(WORKSPACES, rowid, LAST_WORKBENCH_MISSING);
                            None
                        }
                    }
                }
            };
            if let Some(record) = self.state.workspaces.get_mut(&workspace) {
                record.last_workbench = resolved;
            }
        }
    }

    /// Активный воркспейс должен существовать, маршрут — вести в его рабочую область.
    /// Сброшенный активный воркспейс запуск ядра заменяет фокусом.
    fn window(&mut self, conn: &Connection) -> rusqlite::Result<()> {
        let row = conn
            .query_row(
                "SELECT active_workspace_id, route_workbench_id FROM window_state WHERE id = 1",
                [],
                |row| Ok((soft_ref(row, 0), soft_ref(row, 1))),
            )
            .optional()?;
        let Some((active, route)) = row else {
            self.note(WINDOW_STATE, WINDOW_ROWID, ROW_MISSING);
            return Ok(());
        };

        let active = match active {
            SoftRef::Unset => None,
            SoftRef::Id(bytes)
                if self
                    .state
                    .workspaces
                    .contains_key(&WorkspaceId::from_bytes(bytes)) =>
            {
                Some(WorkspaceId::from_bytes(bytes))
            }
            SoftRef::Invalid | SoftRef::Id(_) => {
                self.note(WINDOW_STATE, WINDOW_ROWID, ACTIVE_WORKSPACE_MISSING);
                None
            }
        };
        let route = match route {
            SoftRef::Unset => Route::WorkspaceHome,
            SoftRef::Invalid => {
                self.note(WINDOW_STATE, WINDOW_ROWID, ROUTE_WORKBENCH_MISSING);
                Route::WorkspaceHome
            }
            SoftRef::Id(bytes) => {
                let workbench = WorkbenchId::from_bytes(bytes);
                match self.state.workbench_workspace(workbench) {
                    Some(owner) if Some(owner) == active => Route::Workbench(workbench),
                    Some(_) => {
                        self.note(WINDOW_STATE, WINDOW_ROWID, ROUTE_WORKBENCH_FOREIGN);
                        Route::WorkspaceHome
                    }
                    None => {
                        self.note(WINDOW_STATE, WINDOW_ROWID, ROUTE_WORKBENCH_MISSING);
                        Route::WorkspaceHome
                    }
                }
            }
        };
        self.state.window = WindowState {
            active_workspace: active,
            route,
        };
        Ok(())
    }

    /// Совпадения позиций внутри родителя по тем же областям, что `State::check_invariants`,
    /// включая удалённые и убранные записи.
    fn position_collisions(&mut self) {
        let state = &self.state;
        let workspaces = collided(
            self.workspace_rows
                .iter()
                .filter_map(|(id, &rowid)| Some((state.workspaces.get(id)?.position, rowid))),
        );
        let projects = collided(self.project_rows.iter().filter_map(|(id, &rowid)| {
            let project = state.projects.get(id)?;
            Some(((project.workspace, project.list_position?), rowid))
        }));
        let workbenches = collided(self.workbench_rows.iter().filter_map(|(id, &rowid)| {
            let workbench = state.workbenches.get(id)?;
            Some(((workbench.project, workbench.position), rowid))
        }));
        self.note_all(WORKSPACES, &workspaces, POSITION_TAKEN);
        self.note_all(PROJECTS, &projects, POSITION_TAKEN);
        self.note_all(WORKBENCHES, &workbenches, POSITION_TAKEN);
    }

    /// Совпадения имён без учёта регистра по областям уникальности модели: неудалённые
    /// воркспейсы, неубранные проекты воркспейса, рабочие области проекта.
    fn name_collisions(&mut self) {
        let state = &self.state;
        let workspaces = collided(self.workspace_rows.iter().filter_map(|(id, &rowid)| {
            let workspace = state.workspaces.get(id).filter(|w| !w.is_deleted())?;
            Some((workspace.title.folded(), rowid))
        }));
        let projects = collided(self.project_rows.iter().filter_map(|(id, &rowid)| {
            let project = state.projects.get(id).filter(|p| !p.is_removed())?;
            Some(((project.workspace, project.title.folded()), rowid))
        }));
        let workbenches = collided(self.workbench_rows.iter().filter_map(|(id, &rowid)| {
            let workbench = state.workbenches.get(id)?;
            Some(((workbench.project, workbench.title.folded()), rowid))
        }));
        self.note_all(WORKSPACES, &workspaces, NAME_TAKEN);
        self.note_all(PROJECTS, &projects, NAME_TAKEN);
        self.note_all(WORKBENCHES, &workbenches, NAME_TAKEN);
    }

    fn panels(&mut self, conn: &Connection) -> rusqlite::Result<()> {
        let mut stmt = conn.prepare(
            "SELECT rowid, id, workbench, plugin, title, created_at FROM panels ORDER BY rowid",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let rowid: i64 = row.get(0)?;
            let parsed = (|| -> Result<_, Reason> {
                let id = PanelId::from_bytes(id(row, 1)?);
                let workbench = owner(row, 2)
                    .map(WorkbenchId::from_bytes)
                    .filter(|workbench| self.state.workbenches.contains_key(workbench))
                    .ok_or(OWNER_MISSING)?;
                let plugin: String = row.get(3).map_err(|_| INVALID_VALUE)?;
                let plugin = PluginId::new(&plugin).map_err(|_| INVALID_VALUE)?;
                let (title, trimmed) = title(row, 4)?;
                let created_at: i64 = row.get(5).map_err(|_| INVALID_VALUE)?;
                let panel = Panel {
                    id,
                    workbench,
                    plugin,
                    title,
                    created_at: UnixMillis(created_at),
                };
                Ok((panel, trimmed))
            })();
            match parsed {
                Ok((panel, trimmed)) => {
                    if trimmed {
                        self.note(PANELS, rowid, TITLE_TRIMMED);
                    }
                    self.state.panels.insert(panel.id, panel);
                }
                Err(reason) => self.note(PANELS, rowid, reason),
            }
        }
        Ok(())
    }

    /// Раскладка разбирается и проверяется против загруженных панелей своей рабочей области.
    fn layouts(&mut self, conn: &Connection) -> rusqlite::Result<()> {
        let mut stmt =
            conn.prepare("SELECT rowid, workbench, layout FROM workbench_layouts ORDER BY rowid")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let rowid: i64 = row.get(0)?;
            let parsed = (|| -> Result<_, Reason> {
                let workbench = owner(row, 1)
                    .map(WorkbenchId::from_bytes)
                    .filter(|workbench| self.state.workbenches.contains_key(workbench))
                    .ok_or(OWNER_MISSING)?;
                self.layout_rows.insert(workbench);
                let text: String = row.get(2).map_err(|_| INVALID_VALUE)?;
                let layout = layout_json::decode(&text).ok_or(LAYOUT_UNREADABLE)?;
                layout
                    .validate(&self.state.workbench_panel_ids(workbench))
                    .map_err(|_| LAYOUT_INVALID)?;
                Ok((workbench, layout))
            })();
            match parsed {
                Ok((workbench, layout)) => {
                    self.state.layouts.insert(workbench, layout);
                }
                Err(reason) => self.note(WORKBENCH_LAYOUTS, rowid, reason),
            }
        }
        Ok(())
    }

    /// Рабочая область с живыми панелями и без строки раскладки.
    fn missing_layouts(&mut self) {
        let missing: Vec<i64> = self
            .workbench_rows
            .iter()
            .filter(|&(workbench, _)| !self.layout_rows.contains(workbench))
            .filter(|&(&workbench, _)| self.state.workbench_panels(workbench).next().is_some())
            .map(|(_, &rowid)| rowid)
            .collect();
        self.note_all(WORKBENCHES, &missing, LAYOUT_MISSING);
    }

    fn note_all(&mut self, table: &str, rowids: &[i64], reason: Reason) {
        for &rowid in rowids {
            self.note(table, rowid, reason);
        }
    }

    fn finish(self) -> State {
        let mut state = self.state;
        state.diagnostics = self.diagnostics;
        state
    }
}

/// Строки, чей ключ уже занят строкой с меньшим `rowid`, по возрастанию `rowid`.
fn collided<K: Ord>(items: impl IntoIterator<Item = (K, i64)>) -> Vec<i64> {
    let mut groups: BTreeMap<K, Vec<i64>> = BTreeMap::new();
    for (key, rowid) in items {
        groups.entry(key).or_default().push(rowid);
    }
    let mut rowids: Vec<i64> = groups
        .into_values()
        .flat_map(|mut group| {
            group.sort_unstable();
            group.into_iter().skip(1)
        })
        .collect();
    rowids.sort_unstable();
    rowids
}

fn id(row: &Row<'_>, index: usize) -> Result<[u8; 16], Reason> {
    row.get(index).map_err(|_| INVALID_ID)
}

/// Ссылка владения: не 16 байт — владельца нет.
fn owner(row: &Row<'_>, index: usize) -> Option<[u8; 16]> {
    row.get(index).ok()
}

fn soft_ref(row: &Row<'_>, index: usize) -> SoftRef {
    match row.get::<_, Option<[u8; 16]>>(index) {
        Ok(None) => SoftRef::Unset,
        Ok(Some(bytes)) => SoftRef::Id(bytes),
        Err(_) => SoftRef::Invalid,
    }
}

/// Имя по правилам `Title` и признак, что обрезка пробелов его изменила.
fn title(row: &Row<'_>, index: usize) -> Result<(Title, bool), Reason> {
    let raw: String = row.get(index).map_err(|_| INVALID_VALUE)?;
    let title = Title::new(&raw).map_err(|error| error.reason())?;
    let trimmed = title.as_str() != raw;
    Ok((title, trimmed))
}

fn position(row: &Row<'_>, index: usize) -> Result<u32, Reason> {
    let value: i64 = row.get(index).map_err(|_| INVALID_VALUE)?;
    u32::try_from(value).map_err(|_| POSITION_OUT_OF_RANGE)
}

fn optional_position(row: &Row<'_>, index: usize) -> Result<Option<u32>, Reason> {
    let value: Option<i64> = row.get(index).map_err(|_| INVALID_VALUE)?;
    value
        .map(|value| u32::try_from(value).map_err(|_| POSITION_OUT_OF_RANGE))
        .transpose()
}

fn millis(row: &Row<'_>, index: usize) -> Result<Option<UnixMillis>, Reason> {
    let value: Option<i64> = row.get(index).map_err(|_| INVALID_VALUE)?;
    Ok(value.map(UnixMillis))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collided_skips_first_row_of_each_group() {
        let rows = collided([("a", 5), ("b", 2), ("a", 3), ("a", 9), ("c", 1), ("b", 4)]);
        assert_eq!(rows, [4, 5, 9]);
    }
}
