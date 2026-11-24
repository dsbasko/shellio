//! Чтения ядра: они не меняют ревизию и не проходят через поток хранения.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use shellio_layout::{Frames, MinSize, Viewport, geometry};
use shellio_plugin_api::Registry;

use crate::clock::UnixMillis;
use crate::command::scoped_workbench;
use crate::error::{CoreError, Entity};
use crate::ids::{PanelId, WorkbenchId};
use crate::layout_cmd::panel_min_size;
use crate::model::{Diagnostic, Revision, State};
use crate::panel::{Panel, PluginId, WorkbenchLayout, layout_metrics};
use crate::plugin_host::min_size;
use crate::title::Title;
use crate::tree::{TreeScope, TreeView};
use crate::wire::{Reply, ReplyValue};

/// Чтение ядра. Значения тега `query` стабильны.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "query", deny_unknown_fields)]
pub enum Query {
    /// Дерево воркспейсов области `scope`.
    #[serde(rename = "tree")]
    Tree { scope: TreeScope },
    /// Ревизия, версия схемы, путь базы и диагностика загрузки.
    ///
    /// Вариант с пустыми полями, а не единичный: у единичного варианта внутренне
    /// помеченного перечня serde молча пропускает лишние поля.
    #[serde(rename = "status")]
    Status {},
    /// Раскладка рабочей области: явной или открытой в окне, в области `scope`.
    #[serde(rename = "layout")]
    Layout {
        scope: TreeScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workbench: Option<WorkbenchId>,
    },
}

impl Query {
    /// Стабильное имя чтения.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Tree { .. } => "query.tree",
            Self::Status {} => "query.status",
            Self::Layout { .. } => "query.layout",
        }
    }
}

/// Сведения хранилища для `query.status`: их сообщает хранилище, а не модель.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageInfo {
    /// Версия схемы базы (`user_version`).
    pub schema_version: u32,
    /// Путь `state.db`.
    pub state_db: PathBuf,
}

/// Ответ `query.status`. Ревизия — в [`Reply`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusView {
    pub schema_version: u32,
    pub state_db: PathBuf,
    /// Находки последней загрузки.
    pub diagnostics: Vec<Diagnostic>,
}

/// Панель в ответе `query.layout`: запись панели и `min_size` из манифеста её плагина.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutPanel {
    pub id: PanelId,
    pub workbench: WorkbenchId,
    pub plugin: PluginId,
    pub title: Title,
    pub created_at: UnixMillis,
    /// Наименьший размер тела: меньше него часть показывает заглушку.
    pub min_size: MinSize,
}

impl LayoutPanel {
    fn new(panel: &Panel, registry: &Registry) -> Self {
        Self {
            id: panel.id,
            workbench: panel.workbench,
            plugin: panel.plugin.clone(),
            title: panel.title.clone(),
            created_at: panel.created_at,
            min_size: min_size(registry, &panel.plugin),
        }
    }
}

/// Ответ `query.layout`: раскладка, панели рабочей области по `PanelId` и кадры по текущему размеру
/// окна. Без размера окна `frames` — `null`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutView {
    pub workbench: WorkbenchId,
    pub layout: WorkbenchLayout,
    pub panels: Vec<LayoutPanel>,
    pub frames: Option<Frames<PanelId>>,
}

impl LayoutView {
    fn build(
        state: &State,
        registry: &Registry,
        scope: TreeScope,
        workbench: Option<WorkbenchId>,
        viewport: Option<Viewport>,
    ) -> Result<Self, CoreError> {
        let workbench = scoped_workbench(state, scope, workbench)?;
        let layout = state
            .layouts
            .get(&workbench)
            .ok_or_else(|| CoreError::not_found(Entity::Workbench, workbench))?;
        let frames = viewport.map(|viewport| {
            geometry(layout, viewport, &layout_metrics(), |panel| {
                panel_min_size(state, registry, panel)
            })
        });
        Ok(Self {
            workbench,
            layout: layout.clone(),
            panels: state
                .workbench_panels(workbench)
                .map(|panel| LayoutPanel::new(panel, registry))
                .collect(),
            frames,
        })
    }
}

/// Выполнить чтение над снимком состояния с ревизией `revision`. `viewport` — размер окна, по
/// которому `query.layout` считает кадры, `registry` даёт `min_size` панелей.
pub fn read(
    state: &State,
    registry: &Registry,
    revision: Revision,
    storage: &StorageInfo,
    viewport: Option<Viewport>,
    query: &Query,
) -> Result<Reply, CoreError> {
    let value = match query {
        Query::Tree { scope } => ReplyValue::Tree(TreeView::build(state, *scope)?),
        Query::Status {} => ReplyValue::Status(StatusView {
            schema_version: storage.schema_version,
            state_db: storage.state_db.clone(),
            diagnostics: state.diagnostics.clone(),
        }),
        Query::Layout { scope, workbench } => ReplyValue::Layout(Box::new(LayoutView::build(
            state, registry, *scope, *workbench, viewport,
        )?)),
    };
    Ok(Reply { revision, value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::ids::{SequentialIds, WorkspaceId};
    use crate::model::Workspace;
    use crate::test_support::registry;

    fn storage() -> StorageInfo {
        StorageInfo {
            schema_version: 1,
            state_db: PathBuf::from("/state/state.db"),
        }
    }

    #[test]
    fn status_reports_storage_and_diagnostics() {
        let diagnostic = Diagnostic {
            table: "projects".into(),
            rowid: 4,
            reason: "висячая ссылка workspace_id".into(),
        };
        let state = State {
            diagnostics: vec![diagnostic.clone()],
            ..State::default()
        };
        let reply = read(
            &state,
            &registry(),
            Revision(7),
            &storage(),
            None,
            &Query::Status {},
        )
        .unwrap();
        assert_eq!(
            reply,
            Reply {
                revision: Revision(7),
                value: ReplyValue::Status(StatusView {
                    schema_version: 1,
                    state_db: PathBuf::from("/state/state.db"),
                    diagnostics: vec![diagnostic],
                }),
            }
        );
    }

    #[test]
    fn tree_reply_carries_revision_and_tree() {
        let mut ids = SequentialIds::new();
        let work = WorkspaceId::generate(&mut ids);
        let mut state = State::default();
        state.workspaces.insert(
            work,
            Workspace {
                id: work,
                title: Title::new("Работа").unwrap(),
                position: 0,
                last_workbench: None,
                deleted_at: None,
            },
        );
        state.window.active_workspace = Some(work);

        let query = Query::Tree {
            scope: TreeScope::All,
        };
        let reply = read(&state, &registry(), Revision(3), &storage(), None, &query).unwrap();
        assert_eq!(reply.revision, Revision(3));
        assert_eq!(
            reply.value,
            ReplyValue::Tree(TreeView::build(&state, TreeScope::All).unwrap())
        );
    }

    #[test]
    fn tree_of_missing_workspace_is_not_found() {
        let mut ids = SequentialIds::new();
        let query = Query::Tree {
            scope: TreeScope::Workspace(WorkspaceId::generate(&mut ids)),
        };
        let error = read(
            &State::default(),
            &registry(),
            Revision(0),
            &storage(),
            None,
            &query,
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
    }

    #[test]
    fn names_are_stable() {
        let tree = Query::Tree {
            scope: TreeScope::All,
        };
        assert_eq!(tree.name(), "query.tree");
        assert_eq!(Query::Status {}.name(), "query.status");
    }
}
