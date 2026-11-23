//! Дельты состояния: их пишет хранилище, применяет память и несёт событие.

use serde::{Deserialize, Serialize};

use crate::clock::UnixMillis;
use crate::ids::{PanelId, ProjectId, WorkbenchId, WorkspaceId};
use crate::model::{Project, ProjectRoot, Route, State, WindowState, Workbench, Workspace};
use crate::panel::{Panel, WorkbenchLayout};
use crate::title::Title;

/// Изменение состояния. Закрытый перечень: каждое место, которое его разбирает,
/// делает это исчерпывающим `match` без `_`.
///
/// Дельта самодостаточна: несёт записи целиком с идентификаторами, позициями и временем,
/// поэтому применение ничего не вычисляет заново.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    /// Новый воркспейс.
    WorkspaceInserted { workspace: Workspace },
    /// Новое имя воркспейса.
    WorkspaceRenamed {
        workspace: WorkspaceId,
        title: Title,
    },
    /// Последняя открытая рабочая область воркспейса.
    WorkspaceLastWorkbenchSet {
        workspace: WorkspaceId,
        workbench: Option<WorkbenchId>,
    },
    /// Новый проект вместе с корнями.
    ProjectInserted {
        project: Project,
        roots: Vec<ProjectRoot>,
    },
    /// Проект открыт в момент `at`.
    ProjectOpened { project: ProjectId, at: UnixMillis },
    /// Новая рабочая область.
    WorkbenchInserted { workbench: Workbench },
    /// Новое имя рабочей области.
    WorkbenchRenamed {
        workbench: WorkbenchId,
        title: Title,
    },
    /// Активный воркспейс и маршрут окна.
    WindowSet { active: WorkspaceId, route: Route },
    /// Новая панель рабочей области.
    PanelAdd { panel: Panel },
    /// Панель удалена.
    PanelRemove { panel: PanelId },
    /// Раскладка рабочей области целиком.
    LayoutSet {
        workbench: WorkbenchId,
        layout: WorkbenchLayout,
    },
}

impl State {
    /// Применить дельту к состоянию, по которому её построил обработчик.
    ///
    /// Вставка заменяет запись с тем же `id`. Изменение записи, которой нет, пропускается:
    /// расхождение с ядром зеркало обнаруживает по ревизии.
    pub fn apply(&mut self, change: &Change) {
        match change {
            Change::WorkspaceInserted { workspace } => {
                self.workspaces.insert(workspace.id, workspace.clone());
            }
            Change::WorkspaceRenamed { workspace, title } => {
                if let Some(record) = self.workspaces.get_mut(workspace) {
                    record.title = title.clone();
                }
            }
            Change::WorkspaceLastWorkbenchSet {
                workspace,
                workbench,
            } => {
                if let Some(record) = self.workspaces.get_mut(workspace) {
                    record.last_workbench = *workbench;
                }
            }
            Change::ProjectInserted { project, roots } => {
                self.projects.insert(project.id, project.clone());
                for root in roots {
                    self.roots.insert(root.id, root.clone());
                }
            }
            Change::ProjectOpened { project, at } => {
                if let Some(record) = self.projects.get_mut(project) {
                    record.last_opened_at = Some(*at);
                }
            }
            Change::WorkbenchInserted { workbench } => {
                self.workbenches.insert(workbench.id, workbench.clone());
            }
            Change::WorkbenchRenamed { workbench, title } => {
                if let Some(record) = self.workbenches.get_mut(workbench) {
                    record.title = title.clone();
                }
            }
            Change::WindowSet { active, route } => {
                self.window = WindowState {
                    active_workspace: Some(*active),
                    route: *route,
                };
            }
            Change::PanelAdd { panel } => {
                self.panels.insert(panel.id, panel.clone());
            }
            Change::PanelRemove { panel } => {
                self.panels.remove(panel);
            }
            Change::LayoutSet { workbench, layout } => {
                self.layouts.insert(*workbench, layout.clone());
            }
        }
    }

    /// Применить дельты по порядку.
    pub fn apply_all<'a>(&mut self, changes: impl IntoIterator<Item = &'a Change>) {
        for change in changes {
            self.apply(change);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ids::{IdGen, ProjectRootId, SequentialIds};
    use crate::panel::{NAVIGATOR_PLUGIN, PluginId, TERMINAL_PLUGIN};
    use crate::root::{RootKey, RootKind};

    fn title(name: &str) -> Title {
        Title::new(name).unwrap()
    }

    #[test]
    fn every_change_applies_to_its_record() {
        let mut ids = SequentialIds::new();
        let ids: &mut dyn IdGen = &mut ids;
        let workspace = WorkspaceId::generate(ids);
        let project = ProjectId::generate(ids);
        let root = ProjectRootId::generate(ids);
        let workbench = WorkbenchId::generate(ids);

        let workspace_record = Workspace {
            id: workspace,
            title: title("Работа"),
            position: 0,
            last_workbench: None,
            deleted_at: None,
        };
        let project_record = Project {
            id: project,
            workspace,
            title: title("shellio"),
            list_position: Some(0),
            removed_at: None,
            last_opened_at: None,
        };
        let root_record = ProjectRoot {
            id: root,
            project,
            path: PathBuf::from("/src/shellio"),
            kind: RootKind::Git,
            key: RootKey::new("/src/shellio/.git"),
        };
        let workbench_record = Workbench {
            id: workbench,
            project,
            title: title("Основная"),
            position: 0,
        };

        let mut state = State::default();
        state.apply_all(&[
            Change::WorkspaceInserted {
                workspace: workspace_record.clone(),
            },
            Change::WindowSet {
                active: workspace,
                route: Route::WorkspaceHome,
            },
            Change::ProjectInserted {
                project: project_record.clone(),
                roots: vec![root_record.clone()],
            },
            Change::WorkbenchInserted {
                workbench: workbench_record.clone(),
            },
        ]);
        assert_eq!(state.workspaces[&workspace], workspace_record);
        assert_eq!(state.projects[&project], project_record);
        assert_eq!(state.roots[&root], root_record);
        assert_eq!(state.workbenches[&workbench], workbench_record);
        assert_eq!(state.window.active_workspace, Some(workspace));
        assert_eq!(state.check_invariants(), Vec::new());

        state.apply_all(&[
            Change::WorkspaceRenamed {
                workspace,
                title: title("Дом"),
            },
            Change::WorkbenchRenamed {
                workbench,
                title: title("Ревью"),
            },
            Change::ProjectOpened {
                project,
                at: UnixMillis(42),
            },
            Change::WorkspaceLastWorkbenchSet {
                workspace,
                workbench: Some(workbench),
            },
            Change::WindowSet {
                active: workspace,
                route: Route::Workbench(workbench),
            },
        ]);
        assert_eq!(state.workspaces[&workspace].title, title("Дом"));
        assert_eq!(state.workbenches[&workbench].title, title("Ревью"));
        assert_eq!(
            state.projects[&project].last_opened_at,
            Some(UnixMillis(42))
        );
        assert_eq!(state.workspaces[&workspace].last_workbench, Some(workbench));
        assert_eq!(state.window.route, Route::Workbench(workbench));
        assert_eq!(state.check_invariants(), Vec::new());

        state.apply(&Change::WorkspaceLastWorkbenchSet {
            workspace,
            workbench: None,
        });
        assert_eq!(state.workspaces[&workspace].last_workbench, None);

        let navigator = PanelId::generate(ids);
        let terminal = PanelId::generate(ids);
        let panel = |id, plugin: &str| Panel {
            id,
            workbench,
            plugin: PluginId::new(plugin).unwrap(),
            title: title(plugin),
            created_at: UnixMillis(43),
        };
        let layout = WorkbenchLayout::default_for(navigator, terminal);
        state.apply_all(&[
            Change::PanelAdd {
                panel: panel(navigator, NAVIGATOR_PLUGIN),
            },
            Change::PanelAdd {
                panel: panel(terminal, TERMINAL_PLUGIN),
            },
            Change::LayoutSet {
                workbench,
                layout: layout.clone(),
            },
        ]);
        assert_eq!(state.panels[&navigator], panel(navigator, NAVIGATOR_PLUGIN));
        assert_eq!(state.layouts[&workbench], layout);
        assert_eq!(state.check_invariants(), Vec::new());

        let mut without_terminal = layout;
        without_terminal.zones.center.parts.clear();
        without_terminal.focus = vec![navigator];
        state.apply_all(&[
            Change::LayoutSet {
                workbench,
                layout: without_terminal.clone(),
            },
            Change::PanelRemove { panel: terminal },
        ]);
        assert!(!state.panels.contains_key(&terminal));
        assert_eq!(state.layouts[&workbench], without_terminal);
        assert_eq!(state.check_invariants(), Vec::new());
    }

    #[test]
    fn change_of_missing_record_is_skipped() {
        let mut ids = SequentialIds::new();
        let ids: &mut dyn IdGen = &mut ids;
        let mut state = State::default();
        state.apply_all(&[
            Change::WorkspaceRenamed {
                workspace: WorkspaceId::generate(ids),
                title: title("Нет"),
            },
            Change::WorkspaceLastWorkbenchSet {
                workspace: WorkspaceId::generate(ids),
                workbench: None,
            },
            Change::ProjectOpened {
                project: ProjectId::generate(ids),
                at: UnixMillis(1),
            },
            Change::WorkbenchRenamed {
                workbench: WorkbenchId::generate(ids),
                title: title("Нет"),
            },
            Change::PanelRemove {
                panel: PanelId::generate(ids),
            },
        ]);
        assert_eq!(state, State::default());
    }

    #[test]
    fn change_tag_is_snake_case() {
        let mut ids = SequentialIds::new();
        let change = Change::ProjectOpened {
            project: ProjectId::generate(&mut ids),
            at: UnixMillis(7),
        };
        let json = serde_json::to_value(&change).unwrap();
        assert_eq!(json["change"], "project_opened");
        assert_eq!(json["at"], 7);
        assert_eq!(serde_json::from_value::<Change>(json).unwrap(), change);

        let panel = PanelId::generate(&mut ids);
        let json = serde_json::to_value(Change::PanelRemove { panel }).unwrap();
        assert_eq!(json["change"], "panel_remove");
    }
}
