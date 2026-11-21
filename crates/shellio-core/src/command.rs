//! Команды ядра и их подготовка до актора.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::ids::{ProjectId, ProjectRootId, WorkbenchId, WorkspaceId};
use crate::root::{ResolvedRoot, RootResolver};

/// Внешняя команда ядра. Имена в теге `cmd` стабильны.
///
/// Имена приходят строкой как есть: их проверяет обработчик и отвечает `invalid_argument`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", deny_unknown_fields)]
pub enum Command {
    /// Новый воркспейс в конец списка.
    #[serde(rename = "workspace.create")]
    WorkspaceCreate { title: String },
    /// Переименование воркспейса.
    #[serde(rename = "workspace.rename")]
    WorkspaceRename {
        workspace: WorkspaceId,
        title: String,
    },
    /// Проект с одним корнем в конец списка воркспейса.
    #[serde(rename = "project.add")]
    ProjectAdd {
        workspace: WorkspaceId,
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Новая рабочая область в конец проекта.
    #[serde(rename = "workbench.create")]
    WorkbenchCreate { project: ProjectId, title: String },
    /// Переименование рабочей области.
    #[serde(rename = "workbench.rename")]
    WorkbenchRename {
        workbench: WorkbenchId,
        title: String,
    },
    /// Открыть рабочую область в окне.
    #[serde(rename = "workbench.open")]
    WorkbenchOpen { workbench: WorkbenchId },
    /// Сделать воркспейс активным.
    #[serde(rename = "window.focus_workspace")]
    WindowFocusWorkspace { workspace: WorkspaceId },
}

impl Command {
    /// Стабильное имя команды, оно же значение тега `cmd`.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::WorkspaceCreate { .. } => "workspace.create",
            Self::WorkspaceRename { .. } => "workspace.rename",
            Self::ProjectAdd { .. } => "project.add",
            Self::WorkbenchCreate { .. } => "workbench.create",
            Self::WorkbenchRename { .. } => "workbench.rename",
            Self::WorkbenchOpen { .. } => "workbench.open",
            Self::WindowFocusWorkspace { .. } => "window.focus_workspace",
        }
    }

    /// Нужен ли команде блокирующий ввод-вывод до актора.
    pub const fn needs_io(&self) -> bool {
        match self {
            Self::ProjectAdd { .. } => true,
            Self::WorkspaceCreate { .. }
            | Self::WorkspaceRename { .. }
            | Self::WorkbenchCreate { .. }
            | Self::WorkbenchRename { .. }
            | Self::WorkbenchOpen { .. }
            | Self::WindowFocusWorkspace { .. } => false,
        }
    }

    /// Подготовить команду для обработчика. Блокирует только для команд с [`Self::needs_io`]:
    /// корень `project.add` разрешается здесь, ошибка разрешения — `invalid_argument`.
    pub(crate) fn prepare(self, resolver: &dyn RootResolver) -> Result<PreparedCommand, CoreError> {
        Ok(match self {
            Self::WorkspaceCreate { title } => PreparedCommand::WorkspaceCreate { title },
            Self::WorkspaceRename { workspace, title } => {
                PreparedCommand::WorkspaceRename { workspace, title }
            }
            Self::ProjectAdd {
                workspace,
                path,
                title,
            } => PreparedCommand::ProjectAdd {
                workspace,
                root: resolver.resolve(&path)?,
                title,
            },
            Self::WorkbenchCreate { project, title } => {
                PreparedCommand::WorkbenchCreate { project, title }
            }
            Self::WorkbenchRename { workbench, title } => {
                PreparedCommand::WorkbenchRename { workbench, title }
            }
            Self::WorkbenchOpen { workbench } => PreparedCommand::WorkbenchOpen { workbench },
            Self::WindowFocusWorkspace { workspace } => {
                PreparedCommand::WindowFocusWorkspace { workspace }
            }
        })
    }
}

/// Команда, готовая к чистому обработчику: весь ввод-вывод выполнен до актора.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreparedCommand {
    WorkspaceCreate {
        title: String,
    },
    WorkspaceRename {
        workspace: WorkspaceId,
        title: String,
    },
    /// `project.add` с уже разрешённым корнем.
    ProjectAdd {
        workspace: WorkspaceId,
        root: ResolvedRoot,
        title: Option<String>,
    },
    WorkbenchCreate {
        project: ProjectId,
        title: String,
    },
    WorkbenchRename {
        workbench: WorkbenchId,
        title: String,
    },
    WorkbenchOpen {
        workbench: WorkbenchId,
    },
    WindowFocusWorkspace {
        workspace: WorkspaceId,
    },
}

/// Значение ответа команды: идентификаторы созданных записей.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum CommandValue {
    /// `project.add`.
    Project {
        project: ProjectId,
        root: ProjectRootId,
    },
    /// `workspace.create`.
    Workspace { workspace: WorkspaceId },
    /// `workbench.create`.
    Workbench { workbench: WorkbenchId },
    /// Команды без значения.
    None,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::error::ErrorCode;
    use crate::ids::{IdGen, SequentialIds};
    use crate::root::{RootError, RootKey, RootKind};
    use crate::test_support::FakeRootResolver;

    fn all_commands() -> Vec<Command> {
        let mut ids = SequentialIds::new();
        let ids: &mut dyn IdGen = &mut ids;
        let workspace = WorkspaceId::generate(ids);
        let project = ProjectId::generate(ids);
        let workbench = WorkbenchId::generate(ids);
        vec![
            Command::WorkspaceCreate {
                title: "Работа".into(),
            },
            Command::WorkspaceRename {
                workspace,
                title: "Дом".into(),
            },
            Command::ProjectAdd {
                workspace,
                path: "/src/shellio".into(),
                title: None,
            },
            Command::WorkbenchCreate {
                project,
                title: "Ревью".into(),
            },
            Command::WorkbenchRename {
                workbench,
                title: "Основная".into(),
            },
            Command::WorkbenchOpen { workbench },
            Command::WindowFocusWorkspace { workspace },
        ]
    }

    #[test]
    fn name_matches_json_tag() {
        let commands = all_commands();
        assert_eq!(commands.len(), 7);
        for command in commands {
            let json = serde_json::to_value(&command).unwrap();
            assert_eq!(json["cmd"], command.name());
            assert_eq!(serde_json::from_value::<Command>(json).unwrap(), command);
        }
    }

    #[test]
    fn only_project_add_needs_io() {
        for command in all_commands() {
            assert_eq!(command.needs_io(), command.name() == "project.add");
        }
    }

    #[test]
    fn prepare_resolves_root_of_project_add() {
        let resolver = FakeRootResolver::new().with_git("/src/wt", "/src/main/.git");
        let mut ids = SequentialIds::new();
        let workspace = WorkspaceId::generate(&mut ids);
        let prepared = Command::ProjectAdd {
            workspace,
            path: "/src/wt".into(),
            title: Some("Ветка".into()),
        }
        .prepare(&resolver)
        .unwrap();
        assert_eq!(
            prepared,
            PreparedCommand::ProjectAdd {
                workspace,
                root: ResolvedRoot {
                    path: "/src/wt".into(),
                    kind: RootKind::Git,
                    key: RootKey::new("/src/main/.git"),
                },
                title: Some("Ветка".into()),
            }
        );
    }

    #[test]
    fn prepare_maps_root_errors_to_invalid_argument() {
        let resolver = FakeRootResolver::new()
            .with(
                "/missing",
                Err(RootError::NotFound {
                    path: "/missing".into(),
                }),
            )
            .with(
                "/file",
                Err(RootError::NotADirectory {
                    path: "/file".into(),
                }),
            );
        let mut ids = SequentialIds::new();
        let workspace = WorkspaceId::generate(&mut ids);
        for (path, reason) in [
            ("rel/dir", "path_not_absolute"),
            ("/missing", "path_not_found"),
            ("/file", "not_a_directory"),
        ] {
            let error = Command::ProjectAdd {
                workspace,
                path: path.into(),
                title: None,
            }
            .prepare(&resolver)
            .unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert_eq!(error.detail_str("reason"), Some(reason));
            assert_eq!(error.detail_str("path"), Some(path));
        }
    }

    #[test]
    fn prepare_keeps_other_commands_without_resolving() {
        struct Panicking;
        impl RootResolver for Panicking {
            fn resolve(&self, path: &Path) -> Result<ResolvedRoot, RootError> {
                panic!("разрешение корня {} не ожидалось", path.display())
            }
        }
        for command in all_commands().into_iter().filter(|c| !c.needs_io()) {
            command.prepare(&Panicking).unwrap();
        }
    }

    #[test]
    fn values_serialize_as_objects_of_ids() {
        let mut ids = SequentialIds::new();
        let ids: &mut dyn IdGen = &mut ids;
        let project = ProjectId::generate(ids);
        let root = ProjectRootId::generate(ids);
        let value = CommandValue::Project { project, root };
        let json = serde_json::to_value(value).unwrap();
        assert_eq!(json["project"], project.to_string());
        assert_eq!(json["root"], root.to_string());
        assert_eq!(serde_json::from_value::<CommandValue>(json).unwrap(), value);
        assert_eq!(
            serde_json::to_value(CommandValue::None).unwrap(),
            serde_json::Value::Null
        );
    }
}
