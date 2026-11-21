//! Модель состояния ядра и её инварианты.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::clock::UnixMillis;
use crate::ids::{ProjectId, ProjectRootId, WorkbenchId, WorkspaceId};
use crate::root::{RootKey, RootKind};
use crate::title::Title;

/// Ревизия состояния. Растёт на 1 с каждой записанной командой и хранится вместе с данными.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Revision(pub u64);

impl Revision {
    /// Ревизия пустой базы до первой записи.
    pub const ZERO: Self = Self(0);

    /// Следующая ревизия.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Состояние ядра.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub workspaces: BTreeMap<WorkspaceId, Workspace>,
    pub projects: BTreeMap<ProjectId, Project>,
    pub roots: BTreeMap<ProjectRootId, ProjectRoot>,
    pub workbenches: BTreeMap<WorkbenchId, Workbench>,
    pub window: WindowState,
    /// Находки загрузки. Не хранится в базе.
    pub diagnostics: Vec<Diagnostic>,
}

/// Воркспейс.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub title: Title,
    pub position: u32,
    pub last_workbench: Option<WorkbenchId>,
    pub deleted_at: Option<UnixMillis>,
}

impl Workspace {
    /// Воркспейс удалён и не показывается.
    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }
}

/// Проект воркспейса.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub workspace: WorkspaceId,
    pub title: Title,
    /// Позиция в списке воркспейса. `None` — проект вне списка.
    pub list_position: Option<u32>,
    pub removed_at: Option<UnixMillis>,
    pub last_opened_at: Option<UnixMillis>,
}

impl Project {
    /// Проект убран и не показывается, но держит свои корни.
    pub fn is_removed(&self) -> bool {
        self.removed_at.is_some()
    }
}

/// Корень проекта.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRoot {
    pub id: ProjectRootId,
    pub project: ProjectId,
    pub path: PathBuf,
    pub kind: RootKind,
    pub key: RootKey,
}

/// Рабочая область проекта.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workbench {
    pub id: WorkbenchId,
    pub project: ProjectId,
    pub title: Title,
    pub position: u32,
}

/// Состояние окна.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowState {
    /// Активный воркспейс. `None` только до первого запуска.
    pub active_workspace: Option<WorkspaceId>,
    pub route: Route,
}

/// Что показывает окно в активном воркспейсе.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// Главная воркспейса.
    #[default]
    WorkspaceHome,
    /// Рабочая область.
    Workbench(WorkbenchId),
}

/// Находка загрузки: строка таблицы и причина.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub table: String,
    pub rowid: i64,
    pub reason: String,
}

/// Ссылка на запись любой таблицы модели.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityRef {
    Workspace(WorkspaceId),
    Project(ProjectId),
    Root(ProjectRootId),
    Workbench(WorkbenchId),
}

/// Родитель, внутри которого позиции записей не совпадают.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionScope {
    /// Порядок воркспейсов.
    Workspaces,
    /// Список проектов воркспейса.
    ProjectList(WorkspaceId),
    /// Порядок рабочих областей проекта.
    Workbenches(ProjectId),
}

/// Нарушение инварианта модели.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    /// Ключ записи в карте не совпадает с её `id`.
    KeyMismatch { key: EntityRef, id: EntityRef },
    /// Проект ссылается на несуществующий воркспейс.
    ProjectOwnerMissing {
        project: ProjectId,
        workspace: WorkspaceId,
    },
    /// Корень ссылается на несуществующий проект.
    RootOwnerMissing {
        root: ProjectRootId,
        project: ProjectId,
    },
    /// Рабочая область ссылается на несуществующий проект.
    WorkbenchOwnerMissing {
        workbench: WorkbenchId,
        project: ProjectId,
    },
    /// Последняя рабочая область воркспейса не существует.
    LastWorkbenchMissing {
        workspace: WorkspaceId,
        workbench: WorkbenchId,
    },
    /// Последняя рабочая область воркспейса принадлежит не ему.
    LastWorkbenchForeign {
        workspace: WorkspaceId,
        workbench: WorkbenchId,
    },
    /// Активный воркспейс не существует.
    ActiveWorkspaceMissing { workspace: WorkspaceId },
    /// Воркспейсы есть, а активного нет.
    NoActiveWorkspace,
    /// Маршрут ведёт на несуществующую рабочую область.
    RouteWorkbenchMissing { workbench: WorkbenchId },
    /// Маршрут ведёт на рабочую область не активного воркспейса.
    RouteWorkbenchForeign {
        workbench: WorkbenchId,
        active: Option<WorkspaceId>,
    },
    /// Два корня с одним ключом.
    RootKeyTaken {
        key: RootKey,
        first: ProjectRootId,
        second: ProjectRootId,
    },
    /// Два неудалённых воркспейса с одним именем без учёта регистра.
    WorkspaceNameTaken {
        name: String,
        first: WorkspaceId,
        second: WorkspaceId,
    },
    /// Два неубранных проекта воркспейса с одним именем без учёта регистра.
    ProjectNameTaken {
        workspace: WorkspaceId,
        name: String,
        first: ProjectId,
        second: ProjectId,
    },
    /// Две рабочие области проекта с одним именем без учёта регистра.
    WorkbenchNameTaken {
        project: ProjectId,
        name: String,
        first: WorkbenchId,
        second: WorkbenchId,
    },
    /// Две записи одного родителя с одной позицией. Ядро таких позиций не выдаёт;
    /// у загруженных записей совпадение допустимо и сопровождается диагностикой.
    PositionTaken {
        scope: PositionScope,
        position: u32,
        first: EntityRef,
        second: EntityRef,
    },
}

/// Пары значений с одинаковым ключом: для каждой группы — первое значение с каждым следующим.
fn collisions<K: Ord + Clone, V: Copy>(items: impl IntoIterator<Item = (K, V)>) -> Vec<(K, V, V)> {
    let mut groups: BTreeMap<K, Vec<V>> = BTreeMap::new();
    for (key, value) in items {
        groups.entry(key).or_default().push(value);
    }
    let mut pairs = Vec::new();
    for (key, values) in groups {
        if let Some((&first, rest)) = values.split_first() {
            pairs.extend(rest.iter().map(|&second| (key.clone(), first, second)));
        }
    }
    pairs
}

impl State {
    /// Воркспейс, которому принадлежит рабочая область. `None`, если области или её проекта нет.
    pub fn workbench_workspace(&self, workbench: WorkbenchId) -> Option<WorkspaceId> {
        let workbench = self.workbenches.get(&workbench)?;
        Some(self.projects.get(&workbench.project)?.workspace)
    }

    /// Все нарушения инвариантов раздела «Модель» плана P1.1. Пустой список — состояние цело.
    pub fn check_invariants(&self) -> Vec<Violation> {
        let mut violations = Vec::new();
        self.check_keys(&mut violations);
        self.check_owners(&mut violations);
        self.check_soft_refs(&mut violations);
        self.check_unique(&mut violations);
        self.check_positions(&mut violations);
        violations
    }

    fn check_keys(&self, out: &mut Vec<Violation>) {
        let mut mismatch = |key: EntityRef, id: EntityRef| {
            if key != id {
                out.push(Violation::KeyMismatch { key, id });
            }
        };
        for (key, record) in &self.workspaces {
            mismatch(EntityRef::Workspace(*key), EntityRef::Workspace(record.id));
        }
        for (key, record) in &self.projects {
            mismatch(EntityRef::Project(*key), EntityRef::Project(record.id));
        }
        for (key, record) in &self.roots {
            mismatch(EntityRef::Root(*key), EntityRef::Root(record.id));
        }
        for (key, record) in &self.workbenches {
            mismatch(EntityRef::Workbench(*key), EntityRef::Workbench(record.id));
        }
    }

    fn check_owners(&self, out: &mut Vec<Violation>) {
        for project in self.projects.values() {
            if !self.workspaces.contains_key(&project.workspace) {
                out.push(Violation::ProjectOwnerMissing {
                    project: project.id,
                    workspace: project.workspace,
                });
            }
        }
        for root in self.roots.values() {
            if !self.projects.contains_key(&root.project) {
                out.push(Violation::RootOwnerMissing {
                    root: root.id,
                    project: root.project,
                });
            }
        }
        for workbench in self.workbenches.values() {
            if !self.projects.contains_key(&workbench.project) {
                out.push(Violation::WorkbenchOwnerMissing {
                    workbench: workbench.id,
                    project: workbench.project,
                });
            }
        }
    }

    fn check_soft_refs(&self, out: &mut Vec<Violation>) {
        // Ссылки идут на ключ карты, как и ссылки владения проектов.
        for (&workspace, record) in &self.workspaces {
            let Some(workbench) = record.last_workbench else {
                continue;
            };
            if !self.workbenches.contains_key(&workbench) {
                out.push(Violation::LastWorkbenchMissing {
                    workspace,
                    workbench,
                });
            } else if self.workbench_workspace(workbench) != Some(workspace) {
                out.push(Violation::LastWorkbenchForeign {
                    workspace,
                    workbench,
                });
            }
        }

        let active = self.window.active_workspace;
        match active {
            Some(workspace) if !self.workspaces.contains_key(&workspace) => {
                out.push(Violation::ActiveWorkspaceMissing { workspace });
            }
            None if !self.workspaces.is_empty() => out.push(Violation::NoActiveWorkspace),
            Some(_) | None => {}
        }

        match self.window.route {
            Route::WorkspaceHome => {}
            Route::Workbench(workbench) => {
                if !self.workbenches.contains_key(&workbench) {
                    out.push(Violation::RouteWorkbenchMissing { workbench });
                } else if active.is_none() || self.workbench_workspace(workbench) != active {
                    out.push(Violation::RouteWorkbenchForeign { workbench, active });
                }
            }
        }
    }

    fn check_unique(&self, out: &mut Vec<Violation>) {
        let keys = self.roots.values().map(|root| (root.key.clone(), root.id));
        for (key, first, second) in collisions(keys) {
            out.push(Violation::RootKeyTaken { key, first, second });
        }

        let workspaces = self
            .workspaces
            .values()
            .filter(|workspace| !workspace.is_deleted())
            .map(|workspace| (workspace.title.folded(), workspace.id));
        for (name, first, second) in collisions(workspaces) {
            out.push(Violation::WorkspaceNameTaken {
                name,
                first,
                second,
            });
        }

        let projects = self
            .projects
            .values()
            .filter(|project| !project.is_removed())
            .map(|project| ((project.workspace, project.title.folded()), project.id));
        for ((workspace, name), first, second) in collisions(projects) {
            out.push(Violation::ProjectNameTaken {
                workspace,
                name,
                first,
                second,
            });
        }

        let workbenches = self
            .workbenches
            .values()
            .map(|workbench| ((workbench.project, workbench.title.folded()), workbench.id));
        for ((project, name), first, second) in collisions(workbenches) {
            out.push(Violation::WorkbenchNameTaken {
                project,
                name,
                first,
                second,
            });
        }
    }

    fn check_positions(&self, out: &mut Vec<Violation>) {
        let mut taken = |scope: PositionScope, position: u32, first, second| {
            out.push(Violation::PositionTaken {
                scope,
                position,
                first,
                second,
            });
        };

        let workspaces = self
            .workspaces
            .values()
            .map(|workspace| (workspace.position, EntityRef::Workspace(workspace.id)));
        for (position, first, second) in collisions(workspaces) {
            taken(PositionScope::Workspaces, position, first, second);
        }

        let projects = self.projects.values().filter_map(|project| {
            let position = project.list_position?;
            Some((
                (project.workspace, position),
                EntityRef::Project(project.id),
            ))
        });
        for ((workspace, position), first, second) in collisions(projects) {
            taken(
                PositionScope::ProjectList(workspace),
                position,
                first,
                second,
            );
        }

        let workbenches = self.workbenches.values().map(|workbench| {
            (
                (workbench.project, workbench.position),
                EntityRef::Workbench(workbench.id),
            )
        });
        for ((project, position), first, second) in collisions(workbenches) {
            taken(PositionScope::Workbenches(project), position, first, second);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{IdGen, SequentialIds};

    fn title(name: &str) -> Title {
        Title::new(name).unwrap()
    }

    /// Цельное состояние: два воркспейса, в каждом проект с корнем и рабочей областью,
    /// плюс удалённый воркспейс и убранный проект, чьи имена совпадают с живыми.
    struct Fixture {
        state: State,
        work: WorkspaceId,
        home: WorkspaceId,
        deleted: WorkspaceId,
        app: ProjectId,
        removed: ProjectId,
        notes: ProjectId,
        app_root: ProjectRootId,
        removed_root: ProjectRootId,
        notes_root: ProjectRootId,
        main: WorkbenchId,
        review: WorkbenchId,
        draft: WorkbenchId,
    }

    fn fixture() -> Fixture {
        let mut ids = SequentialIds::new();
        let ids: &mut dyn IdGen = &mut ids;
        let work = WorkspaceId::generate(ids);
        let home = WorkspaceId::generate(ids);
        let deleted = WorkspaceId::generate(ids);
        let app = ProjectId::generate(ids);
        let removed = ProjectId::generate(ids);
        let notes = ProjectId::generate(ids);
        let app_root = ProjectRootId::generate(ids);
        let removed_root = ProjectRootId::generate(ids);
        let notes_root = ProjectRootId::generate(ids);
        let main = WorkbenchId::generate(ids);
        let review = WorkbenchId::generate(ids);
        let draft = WorkbenchId::generate(ids);

        let workspace = |id, name: &str, position, last_workbench, deleted_at| Workspace {
            id,
            title: title(name),
            position,
            last_workbench,
            deleted_at,
        };
        let project = |id, workspace, name: &str, list_position, removed_at| Project {
            id,
            workspace,
            title: title(name),
            list_position,
            removed_at,
            last_opened_at: None,
        };
        let root = |id, project, path: &str, kind| ProjectRoot {
            id,
            project,
            path: PathBuf::from(path),
            kind,
            key: RootKey::new(path),
        };
        let workbench = |id, project, name: &str, position| Workbench {
            id,
            project,
            title: title(name),
            position,
        };

        let state = State {
            workspaces: BTreeMap::from([
                (work, workspace(work, "Работа", 0, Some(main), None)),
                (home, workspace(home, "Дом", 1, Some(draft), None)),
                (
                    deleted,
                    workspace(deleted, "РАБОТА", 2, None, Some(UnixMillis(7))),
                ),
            ]),
            projects: BTreeMap::from([
                (app, project(app, work, "shellio", Some(0), None)),
                (
                    removed,
                    project(removed, work, "Shellio", None, Some(UnixMillis(9))),
                ),
                (notes, project(notes, home, "shellio", Some(0), None)),
            ]),
            roots: BTreeMap::from([
                (app_root, root(app_root, app, "/src/shellio", RootKind::Git)),
                (
                    removed_root,
                    root(removed_root, removed, "/src/old", RootKind::Folder),
                ),
                (
                    notes_root,
                    root(notes_root, notes, "/notes", RootKind::Folder),
                ),
            ]),
            workbenches: BTreeMap::from([
                (main, workbench(main, app, "Основная", 0)),
                (review, workbench(review, app, "Ревью", 1)),
                (draft, workbench(draft, notes, "Основная", 0)),
            ]),
            window: WindowState {
                active_workspace: Some(work),
                route: Route::Workbench(main),
            },
            diagnostics: Vec::new(),
        };
        Fixture {
            state,
            work,
            home,
            deleted,
            app,
            removed,
            notes,
            app_root,
            removed_root,
            notes_root,
            main,
            review,
            draft,
        }
    }

    fn unknown<T>(make: impl FnOnce(&mut dyn IdGen) -> T) -> T {
        let mut ids = SequentialIds::new();
        for _ in 0..1000 {
            ids.next_uuid();
        }
        make(&mut ids)
    }

    #[test]
    fn empty_and_fixture_states_are_whole() {
        assert_eq!(State::default().check_invariants(), Vec::new());
        assert_eq!(fixture().state.check_invariants(), Vec::new());
    }

    #[test]
    fn workbench_belongs_to_workspace_of_its_project() {
        let f = fixture();
        assert_eq!(f.state.workbench_workspace(f.review), Some(f.work));
        assert_eq!(f.state.workbench_workspace(f.draft), Some(f.home));
        assert_eq!(
            f.state
                .workbench_workspace(unknown(|ids| WorkbenchId::generate(ids))),
            None
        );
    }

    #[test]
    fn key_mismatch_is_caught() {
        let mut f = fixture();
        let mut moved = f.state.workspaces.remove(&f.home).unwrap();
        let wrong = unknown(|ids| WorkspaceId::generate(ids));
        moved.id = wrong;
        f.state.workspaces.insert(f.home, moved);
        // Записи воркспейса ссылаются на прежний ключ, поэтому видна только подмена.
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::KeyMismatch {
                key: EntityRef::Workspace(f.home),
                id: EntityRef::Workspace(wrong),
            }]
        );
    }

    #[test]
    fn missing_owners_are_caught() {
        let ghost_workspace = unknown(|ids| WorkspaceId::generate(ids));
        let ghost_project = unknown(|ids| ProjectId::generate(ids));

        let mut f = fixture();
        f.state.projects.get_mut(&f.notes).unwrap().workspace = ghost_workspace;
        f.state.workspaces.get_mut(&f.home).unwrap().last_workbench = None;
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::ProjectOwnerMissing {
                project: f.notes,
                workspace: ghost_workspace,
            }]
        );

        let mut f = fixture();
        f.state.roots.get_mut(&f.notes_root).unwrap().project = ghost_project;
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::RootOwnerMissing {
                root: f.notes_root,
                project: ghost_project,
            }]
        );

        let mut f = fixture();
        f.state.workbenches.get_mut(&f.review).unwrap().project = ghost_project;
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::WorkbenchOwnerMissing {
                workbench: f.review,
                project: ghost_project,
            }]
        );
    }

    #[test]
    fn last_workbench_must_exist_in_own_workspace() {
        let ghost = unknown(|ids| WorkbenchId::generate(ids));

        let mut f = fixture();
        f.state.workspaces.get_mut(&f.home).unwrap().last_workbench = Some(ghost);
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::LastWorkbenchMissing {
                workspace: f.home,
                workbench: ghost,
            }]
        );

        let mut f = fixture();
        f.state.workspaces.get_mut(&f.home).unwrap().last_workbench = Some(f.review);
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::LastWorkbenchForeign {
                workspace: f.home,
                workbench: f.review,
            }]
        );
    }

    #[test]
    fn active_workspace_must_exist_when_any_workspace_does() {
        let mut f = fixture();
        f.state.window = WindowState {
            active_workspace: None,
            route: Route::WorkspaceHome,
        };
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::NoActiveWorkspace]
        );

        let ghost = unknown(|ids| WorkspaceId::generate(ids));
        let mut f = fixture();
        f.state.window = WindowState {
            active_workspace: Some(ghost),
            route: Route::WorkspaceHome,
        };
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::ActiveWorkspaceMissing { workspace: ghost }]
        );

        let empty = State {
            window: WindowState {
                active_workspace: Some(ghost),
                route: Route::WorkspaceHome,
            },
            ..State::default()
        };
        assert_eq!(
            empty.check_invariants(),
            vec![Violation::ActiveWorkspaceMissing { workspace: ghost }]
        );
    }

    #[test]
    fn route_must_point_to_workbench_of_active_workspace() {
        let ghost = unknown(|ids| WorkbenchId::generate(ids));
        let mut f = fixture();
        f.state.window.route = Route::Workbench(ghost);
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::RouteWorkbenchMissing { workbench: ghost }]
        );

        let mut f = fixture();
        f.state.window.route = Route::Workbench(f.draft);
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::RouteWorkbenchForeign {
                workbench: f.draft,
                active: Some(f.work),
            }]
        );

        let mut f = fixture();
        f.state.window.active_workspace = Some(f.home);
        f.state.window.route = Route::Workbench(f.draft);
        assert_eq!(f.state.check_invariants(), Vec::new());
    }

    #[test]
    fn route_without_active_workspace_is_foreign() {
        let mut f = fixture();
        f.state.window.active_workspace = None;
        assert_eq!(
            f.state.check_invariants(),
            vec![
                Violation::NoActiveWorkspace,
                Violation::RouteWorkbenchForeign {
                    workbench: f.main,
                    active: None,
                },
            ]
        );
    }

    #[test]
    fn root_keys_are_unique_including_removed_projects() {
        let mut f = fixture();
        let key = f.state.roots[&f.removed_root].key.clone();
        f.state.roots.get_mut(&f.notes_root).unwrap().key = key.clone();
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::RootKeyTaken {
                key,
                first: f.removed_root,
                second: f.notes_root,
            }]
        );
        assert!(f.state.projects[&f.removed].is_removed());
        assert_eq!(f.state.roots[&f.app_root].project, f.app);
    }

    #[test]
    fn workspace_names_are_unique_ignoring_case_and_deleted() {
        let mut f = fixture();
        assert!(f.state.workspaces[&f.deleted].is_deleted());
        f.state.workspaces.get_mut(&f.home).unwrap().title = title("рАбОтА");
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::WorkspaceNameTaken {
                name: "работа".to_owned(),
                first: f.work,
                second: f.home,
            }]
        );
    }

    #[test]
    fn project_names_are_unique_within_workspace_ignoring_removed() {
        let mut f = fixture();
        f.state.projects.get_mut(&f.removed).unwrap().removed_at = None;
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::ProjectNameTaken {
                workspace: f.work,
                name: "shellio".to_owned(),
                first: f.app,
                second: f.removed,
            }]
        );
    }

    #[test]
    fn workbench_names_are_unique_within_project() {
        let mut f = fixture();
        f.state.workbenches.get_mut(&f.review).unwrap().title = title("ОСНОВНАЯ");
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::WorkbenchNameTaken {
                project: f.app,
                name: "основная".to_owned(),
                first: f.main,
                second: f.review,
            }]
        );
    }

    #[test]
    fn positions_are_unique_within_parent() {
        let mut f = fixture();
        f.state.workspaces.get_mut(&f.deleted).unwrap().position = 0;
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::PositionTaken {
                scope: PositionScope::Workspaces,
                position: 0,
                first: EntityRef::Workspace(f.work),
                second: EntityRef::Workspace(f.deleted),
            }]
        );

        let mut f = fixture();
        f.state.projects.get_mut(&f.removed).unwrap().list_position = Some(0);
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::PositionTaken {
                scope: PositionScope::ProjectList(f.work),
                position: 0,
                first: EntityRef::Project(f.app),
                second: EntityRef::Project(f.removed),
            }]
        );

        let mut f = fixture();
        f.state.workbenches.get_mut(&f.review).unwrap().position = 0;
        assert_eq!(
            f.state.check_invariants(),
            vec![Violation::PositionTaken {
                scope: PositionScope::Workbenches(f.app),
                position: 0,
                first: EntityRef::Workbench(f.main),
                second: EntityRef::Workbench(f.review),
            }]
        );
    }

    #[test]
    fn every_extra_collision_is_reported_against_the_first() {
        let mut f = fixture();
        let extra = unknown(|ids| WorkbenchId::generate(ids));
        f.state.workbenches.insert(
            extra,
            Workbench {
                id: extra,
                project: f.app,
                title: title("Третья"),
                position: 0,
            },
        );
        f.state.workbenches.get_mut(&f.review).unwrap().position = 0;
        assert_eq!(
            f.state.check_invariants(),
            vec![
                Violation::PositionTaken {
                    scope: PositionScope::Workbenches(f.app),
                    position: 0,
                    first: EntityRef::Workbench(f.main),
                    second: EntityRef::Workbench(f.review),
                },
                Violation::PositionTaken {
                    scope: PositionScope::Workbenches(f.app),
                    position: 0,
                    first: EntityRef::Workbench(f.main),
                    second: EntityRef::Workbench(extra),
                },
            ]
        );
    }

    #[test]
    fn revision_counts_from_zero() {
        assert_eq!(Revision::default(), Revision::ZERO);
        assert_eq!(Revision::ZERO.next().next(), Revision(2));
        assert_eq!(serde_json::to_string(&Revision(12)).unwrap(), "12");
    }

    #[test]
    fn state_round_trips_through_json() {
        let state = fixture().state;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<State>(&json).unwrap(), state);
    }
}
