//! Дерево воркспейсов для чтения `query.tree`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::clock::UnixMillis;
use crate::error::{CoreError, Entity};
use crate::ids::{ProjectId, ProjectRootId, WorkbenchId, WorkspaceId};
use crate::model::{Project, Route, State, Workspace};
use crate::root::{RootKey, RootKind};
use crate::title::Title;

/// Область чтения дерева. Внешний вызов без контекста сессии задаёт её явно (ADR-0025).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeScope {
    /// Один неудалённый воркспейс.
    Workspace(WorkspaceId),
    /// Все неудалённые воркспейсы.
    All,
}

/// Дерево: состояние окна и видимые воркспейсы. Удалённые воркспейсы, убранные проекты
/// и рабочие области таких проектов в него не входят.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeView {
    pub active_workspace: Option<WorkspaceId>,
    pub route: Route,
    /// Воркспейсы области по `(position, id)`.
    pub workspaces: Vec<TreeWorkspace>,
}

/// Воркспейс дерева.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeWorkspace {
    pub id: WorkspaceId,
    pub title: Title,
    pub last_workbench: Option<WorkbenchId>,
    /// Участники списка по `(list_position, id)`, затем проекты вне списка по имени.
    pub projects: Vec<TreeProject>,
}

/// Проект дерева.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeProject {
    pub id: ProjectId,
    pub title: Title,
    /// Позиция в списке воркспейса. `None` — проект вне списка.
    pub list_position: Option<u32>,
    pub last_opened_at: Option<UnixMillis>,
    /// Корни по `(path, id)`.
    pub roots: Vec<TreeRoot>,
    /// Рабочие области по `(position, id)`.
    pub workbenches: Vec<TreeWorkbench>,
}

/// Корень проекта в дереве.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeRoot {
    pub id: ProjectRootId,
    pub path: PathBuf,
    pub kind: RootKind,
    pub key: RootKey,
}

/// Рабочая область в дереве.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeWorkbench {
    pub id: WorkbenchId,
    pub title: Title,
}

impl TreeView {
    /// Дерево области `scope`. Удалённый или несуществующий воркспейс области — `not_found`.
    pub fn build(state: &State, scope: TreeScope) -> Result<Self, CoreError> {
        let workspaces = match scope {
            TreeScope::Workspace(id) => vec![
                state
                    .live_workspace(id)
                    .ok_or_else(|| CoreError::not_found(Entity::Workspace, id))?,
            ],
            TreeScope::All => state.workspaces_in_order(),
        };
        Ok(Self {
            active_workspace: state.window.active_workspace,
            route: state.window.route,
            workspaces: workspaces
                .into_iter()
                .map(|workspace| tree_workspace(state, workspace))
                .collect(),
        })
    }
}

fn tree_workspace(state: &State, workspace: &Workspace) -> TreeWorkspace {
    let (mut listed, mut unlisted): (Vec<&Project>, Vec<&Project>) = state
        .projects
        .values()
        .filter(|project| project.workspace == workspace.id && !project.is_removed())
        .partition(|project| project.list_position.is_some());
    listed.sort_by_key(|project| (project.list_position, project.id));
    unlisted.sort_by(|a, b| {
        (a.title.folded(), a.title.as_str(), a.id).cmp(&(b.title.folded(), b.title.as_str(), b.id))
    });
    TreeWorkspace {
        id: workspace.id,
        title: workspace.title.clone(),
        last_workbench: workspace.last_workbench,
        projects: listed
            .into_iter()
            .chain(unlisted)
            .map(|project| tree_project(state, project))
            .collect(),
    }
}

fn tree_project(state: &State, project: &Project) -> TreeProject {
    let mut roots: Vec<_> = state
        .roots
        .values()
        .filter(|root| root.project == project.id)
        .collect();
    roots.sort_by(|a, b| (&a.path, a.id).cmp(&(&b.path, b.id)));
    TreeProject {
        id: project.id,
        title: project.title.clone(),
        list_position: project.list_position,
        last_opened_at: project.last_opened_at,
        roots: roots
            .into_iter()
            .map(|root| TreeRoot {
                id: root.id,
                path: root.path.clone(),
                kind: root.kind,
                key: root.key.clone(),
            })
            .collect(),
        workbenches: state
            .workbenches_in_order(project.id)
            .into_iter()
            .map(|workbench| TreeWorkbench {
                id: workbench.id,
                title: workbench.title.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::error::ErrorCode;
    use crate::ids::{IdGen, SequentialIds};
    use crate::model::{ProjectRoot, Workbench};

    fn title(name: &str) -> Title {
        Title::new(name).unwrap()
    }

    struct Builder {
        state: State,
        ids: SequentialIds,
    }

    impl Builder {
        fn new() -> Self {
            Self {
                state: State::default(),
                ids: SequentialIds::new(),
            }
        }

        fn workspace(&mut self, name: &str, position: u32) -> WorkspaceId {
            let id = WorkspaceId::generate(&mut self.ids);
            self.state.workspaces.insert(
                id,
                Workspace {
                    id,
                    title: title(name),
                    position,
                    last_workbench: None,
                    deleted_at: None,
                },
            );
            id
        }

        fn project(
            &mut self,
            workspace: WorkspaceId,
            name: &str,
            list_position: Option<u32>,
        ) -> ProjectId {
            let id = ProjectId::generate(&mut self.ids);
            self.state.projects.insert(
                id,
                Project {
                    id,
                    workspace,
                    title: title(name),
                    list_position,
                    removed_at: None,
                    last_opened_at: None,
                },
            );
            id
        }

        fn root(&mut self, project: ProjectId, path: &str, kind: RootKind) -> ProjectRootId {
            let id = ProjectRootId::generate(&mut self.ids);
            self.state.roots.insert(
                id,
                ProjectRoot {
                    id,
                    project,
                    path: PathBuf::from(path),
                    kind,
                    key: RootKey::new(path),
                },
            );
            id
        }

        fn workbench(&mut self, project: ProjectId, name: &str, position: u32) -> WorkbenchId {
            let id = WorkbenchId::generate(&mut self.ids);
            self.state.workbenches.insert(
                id,
                Workbench {
                    id,
                    project,
                    title: title(name),
                    position,
                },
            );
            id
        }
    }

    fn project_titles(workspace: &TreeWorkspace) -> Vec<&str> {
        workspace
            .projects
            .iter()
            .map(|project| project.title.as_str())
            .collect()
    }

    #[test]
    fn all_scope_lists_live_workspaces_in_order() {
        let mut b = Builder::new();
        let second = b.workspace("Дом", 5);
        let first = b.workspace("Работа", 1);
        let deleted = b.workspace("Архив", 0);
        b.state.workspaces.get_mut(&deleted).unwrap().deleted_at = Some(UnixMillis(9));
        // Совпадение позиций у загруженных записей упорядочивается по `id`.
        let tie = b.workspace("Личное", 5);
        b.state.window.active_workspace = Some(first);

        let tree = TreeView::build(&b.state, TreeScope::All).unwrap();
        let order: Vec<_> = tree.workspaces.iter().map(|w| w.id).collect();
        assert_eq!(order, vec![first, second, tie]);
        assert_eq!(tree.active_workspace, Some(first));
        assert_eq!(tree.route, Route::WorkspaceHome);
    }

    #[test]
    fn workspace_scope_shows_only_that_workspace() {
        let mut b = Builder::new();
        let work = b.workspace("Работа", 0);
        let home = b.workspace("Дом", 1);
        b.project(work, "shellio", Some(0));
        let notes = b.project(home, "notes", Some(0));
        let draft = b.workbench(notes, "Черновик", 0);
        b.state.workspaces.get_mut(&home).unwrap().last_workbench = Some(draft);
        b.state.window.active_workspace = Some(work);

        let tree = TreeView::build(&b.state, TreeScope::Workspace(home)).unwrap();
        assert_eq!(tree.workspaces.len(), 1);
        let workspace = &tree.workspaces[0];
        assert_eq!(workspace.id, home);
        assert_eq!(workspace.title, title("Дом"));
        assert_eq!(workspace.last_workbench, Some(draft));
        assert_eq!(project_titles(workspace), vec!["notes"]);
        assert_eq!(workspace.projects[0].workbenches[0].id, draft);
        // Состояние окна одно на все области.
        assert_eq!(tree.active_workspace, Some(work));
    }

    #[test]
    fn workspace_scope_of_missing_or_deleted_workspace_is_not_found() {
        let mut b = Builder::new();
        let deleted = b.workspace("Архив", 0);
        b.state.workspaces.get_mut(&deleted).unwrap().deleted_at = Some(UnixMillis(1));
        let missing = WorkspaceId::generate(&mut b.ids);
        for id in [deleted, missing] {
            let error = TreeView::build(&b.state, TreeScope::Workspace(id)).unwrap_err();
            assert_eq!(error.code(), ErrorCode::NotFound);
            assert_eq!(error.detail_str("entity"), Some("workspace"));
            assert_eq!(error.detail_str("id"), Some(id.to_string().as_str()));
        }
    }

    #[test]
    fn listed_projects_come_first_then_unlisted_by_name() {
        let mut b = Builder::new();
        let work = b.workspace("Работа", 0);
        b.project(work, "zeta", None);
        b.project(work, "третий", Some(7));
        b.project(work, "Alpha", None);
        b.project(work, "первый", Some(0));
        b.project(work, "beta", None);
        // Совпадение позиций в списке упорядочивается по `id`.
        b.project(work, "второй", Some(3));
        b.project(work, "второй-б", Some(3));
        b.project(work, "Яблоко", None);
        b.project(work, "арбуз", None);

        let tree = TreeView::build(&b.state, TreeScope::Workspace(work)).unwrap();
        assert_eq!(
            project_titles(&tree.workspaces[0]),
            vec![
                "первый",
                "второй",
                "второй-б",
                "третий",
                "Alpha",
                "beta",
                "zeta",
                "арбуз",
                "Яблоко"
            ]
        );
    }

    #[test]
    fn removed_projects_and_their_workbenches_are_hidden() {
        let mut b = Builder::new();
        let work = b.workspace("Работа", 0);
        let live = b.project(work, "shellio", Some(0));
        let removed = b.project(work, "old", Some(1));
        b.root(removed, "/src/old", RootKind::Folder);
        b.workbench(removed, "Основная", 0);
        b.state.projects.get_mut(&removed).unwrap().removed_at = Some(UnixMillis(3));

        let tree = TreeView::build(&b.state, TreeScope::All).unwrap();
        let projects = &tree.workspaces[0].projects;
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, live);
    }

    #[test]
    fn project_carries_roots_and_workbenches_in_order() {
        let mut b = Builder::new();
        let work = b.workspace("Работа", 0);
        let app = b.project(work, "shellio", Some(0));
        let git = b.root(app, "/src/shellio", RootKind::Git);
        let docs = b.root(app, "/src/docs", RootKind::Folder);
        let review = b.workbench(app, "Ревью", 2);
        let main = b.workbench(app, "Основная", 0);
        let tie = b.workbench(app, "Ветка", 2);
        b.state.projects.get_mut(&app).unwrap().last_opened_at = Some(UnixMillis(42));

        let tree = TreeView::build(&b.state, TreeScope::All).unwrap();
        let project = &tree.workspaces[0].projects[0];
        assert_eq!(project.list_position, Some(0));
        assert_eq!(project.last_opened_at, Some(UnixMillis(42)));
        assert_eq!(
            project.roots,
            vec![
                TreeRoot {
                    id: docs,
                    path: PathBuf::from("/src/docs"),
                    kind: RootKind::Folder,
                    key: RootKey::new("/src/docs"),
                },
                TreeRoot {
                    id: git,
                    path: PathBuf::from("/src/shellio"),
                    kind: RootKind::Git,
                    key: RootKey::new("/src/shellio"),
                },
            ]
        );
        let workbenches: Vec<_> = project.workbenches.iter().map(|w| w.id).collect();
        assert_eq!(workbenches, vec![main, review, tie]);
    }

    #[test]
    fn empty_workspace_has_no_projects() {
        let mut b = Builder::new();
        let empty = b.workspace("Пусто", 0);
        b.state.window.active_workspace = Some(empty);

        for scope in [TreeScope::All, TreeScope::Workspace(empty)] {
            let tree = TreeView::build(&b.state, scope).unwrap();
            assert_eq!(
                tree,
                TreeView {
                    active_workspace: Some(empty),
                    route: Route::WorkspaceHome,
                    workspaces: vec![TreeWorkspace {
                        id: empty,
                        title: title("Пусто"),
                        last_workbench: None,
                        projects: Vec::new(),
                    }],
                }
            );
        }
    }

    #[test]
    fn empty_state_gives_empty_tree() {
        let tree = TreeView::build(&State::default(), TreeScope::All).unwrap();
        assert_eq!(tree.active_workspace, None);
        assert!(tree.workspaces.is_empty());
    }

    #[test]
    fn route_is_reported_as_is() {
        let mut b = Builder::new();
        let work = b.workspace("Работа", 0);
        let app = b.project(work, "shellio", Some(0));
        let main = b.workbench(app, "Основная", 0);
        b.state.window.active_workspace = Some(work);
        b.state.window.route = Route::Workbench(main);

        let tree = TreeView::build(&b.state, TreeScope::All).unwrap();
        assert_eq!(tree.route, Route::Workbench(main));
    }

    #[test]
    fn ids_are_unrelated_to_the_order() {
        // Позиция, а не порядок выдачи ID, задаёт порядок воркспейсов.
        let mut ids = SequentialIds::new();
        let ids: &mut dyn IdGen = &mut ids;
        let early = WorkspaceId::generate(ids);
        let late = WorkspaceId::generate(ids);
        let workspace = |id, name: &str, position| Workspace {
            id,
            title: title(name),
            position,
            last_workbench: None,
            deleted_at: None,
        };
        let state = State {
            workspaces: BTreeMap::from([
                (early, workspace(early, "Второй", 1)),
                (late, workspace(late, "Первый", 0)),
            ]),
            ..State::default()
        };
        let tree = TreeView::build(&state, TreeScope::All).unwrap();
        let order: Vec<_> = tree.workspaces.iter().map(|w| w.id).collect();
        assert_eq!(order, vec![late, early]);
    }
}
