//! Зеркало состояния у потребителя событий: снимок и ревизия плюс события по порядку.

use std::sync::Arc;

use crate::events::Snapshot;
use crate::model::{Revision, Route, State};
use crate::title::Title;
use crate::wire::Event;

/// Что подписка доставляет зеркалу.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Событие ядра.
    Event(Event),
    /// Подписка отстала и потеряла события: нужен новый снимок.
    Reset,
    /// Новый снимок, взятый после `NeedSnapshot`.
    Snapshot(Snapshot),
}

/// Исход доставки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum MirrorStep {
    /// Событие применено или снимок заменил состояние.
    Applied,
    /// Ревизия не новее зеркала: состояние не тронуто.
    Ignored,
    /// Разрыв ревизий или `Reset`: состояние не тронуто, нужен новый снимок.
    NeedSnapshot,
}

/// Копия `State` с ревизией, которую ведут события ядра.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mirror {
    revision: Revision,
    state: Arc<State>,
}

impl Mirror {
    /// Зеркало на ревизии снимка. Состояние разделяется со снимком до первого события.
    pub fn from_snapshot(snapshot: Snapshot) -> Self {
        Self {
            revision: snapshot.revision,
            state: snapshot.state,
        }
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Применить доставку.
    ///
    /// Событие следующей ревизии применяется, событие не новее зеркала пропускается, событие
    /// после разрыва и `Reset` требуют снимка. Снимок старее зеркала пропускается.
    pub fn apply(&mut self, delivery: Delivery) -> MirrorStep {
        match delivery {
            Delivery::Event(event) => self.apply_event(&event),
            Delivery::Reset => MirrorStep::NeedSnapshot,
            Delivery::Snapshot(snapshot) => {
                if snapshot.revision < self.revision {
                    return MirrorStep::Ignored;
                }
                *self = Self::from_snapshot(snapshot);
                MirrorStep::Applied
            }
        }
    }

    fn apply_event(&mut self, event: &Event) -> MirrorStep {
        if event.revision <= self.revision {
            return MirrorStep::Ignored;
        }
        if event.revision != self.revision.next() {
            return MirrorStep::NeedSnapshot;
        }
        Arc::make_mut(&mut self.state).apply_all(&event.changes);
        self.revision = event.revision;
        MirrorStep::Applied
    }

    /// Имя активного воркспейса для кнопки воркспейса.
    pub fn workspace_name(&self) -> Option<&Title> {
        workspace_name(&self.state)
    }

    /// Системный заголовок окна.
    pub fn window_title(&self) -> Option<String> {
        window_title(&self.state)
    }
}

/// Имя активного неудалённого воркспейса. `None` — активного воркспейса нет.
pub fn workspace_name(state: &State) -> Option<&Title> {
    let active = state.window.active_workspace?;
    state
        .live_workspace(active)
        .map(|workspace| &workspace.title)
}

/// Системный заголовок окна по ADR-0026: «<воркспейс> — <проект> / <область>», без открытого
/// места — имя воркспейса, без активного воркспейса — `None`.
///
/// Убранный проект открытой области остаётся в заголовке, пока область открыта (ADR-0026).
/// Маршрут в область чужого воркспейса или в запись, которой нет, даёт имя воркспейса.
pub fn window_title(state: &State) -> Option<String> {
    let active = state.window.active_workspace?;
    let workspace = state.live_workspace(active)?;
    let place = match state.window.route {
        Route::WorkspaceHome => None,
        Route::Workbench(workbench) => state.workbenches.get(&workbench).and_then(|workbench| {
            state
                .projects
                .get(&workbench.project)
                .filter(|project| project.workspace == active)
                .map(|project| (project, workbench))
        }),
    };
    Some(match place {
        Some((project, workbench)) => format!(
            "{} — {} / {}",
            workspace.title.as_str(),
            project.title.as_str(),
            workbench.title.as_str()
        ),
        None => workspace.title.as_str().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::Change;
    use crate::ids::{IdGen, SequentialIds, WorkspaceId};
    use crate::model::Workspace;

    fn title(name: &str) -> Title {
        Title::new(name).unwrap()
    }

    fn renamed(revision: u64, workspace: WorkspaceId, name: &str) -> Delivery {
        Delivery::Event(Event {
            revision: Revision(revision),
            changes: vec![Change::WorkspaceRenamed {
                workspace,
                title: title(name),
            }],
        })
    }

    /// Зеркало на ревизии 3 с одним активным воркспейсом «Работа».
    fn mirror() -> (Mirror, WorkspaceId) {
        let mut ids = SequentialIds::new();
        let ids: &mut dyn IdGen = &mut ids;
        let workspace = WorkspaceId::generate(ids);
        let mut state = State::default();
        state.apply_all(&[
            Change::WorkspaceInserted {
                workspace: Workspace {
                    id: workspace,
                    title: title("Работа"),
                    position: 0,
                    last_workbench: None,
                    deleted_at: None,
                },
            },
            Change::WindowSet {
                active: workspace,
                route: Route::WorkspaceHome,
            },
        ]);
        let snapshot = Snapshot {
            revision: Revision(3),
            state: Arc::new(state),
        };
        (Mirror::from_snapshot(snapshot), workspace)
    }

    #[test]
    fn next_revision_applies_and_shares_nothing_with_snapshot_after() {
        let (mut mirror, workspace) = mirror();
        let snapshot = Arc::clone(&mirror.state);
        assert_eq!(
            mirror.apply(renamed(4, workspace, "Дом")),
            MirrorStep::Applied
        );
        assert_eq!(mirror.revision(), Revision(4));
        assert_eq!(mirror.workspace_name(), Some(&title("Дом")));
        assert_eq!(workspace_name(&snapshot), Some(&title("Работа")));
    }

    #[test]
    fn old_and_repeated_revisions_are_ignored() {
        let (mut mirror, workspace) = mirror();
        let before = mirror.clone();
        for revision in [0, 2, 3] {
            assert_eq!(
                mirror.apply(renamed(revision, workspace, "Дом")),
                MirrorStep::Ignored
            );
        }
        assert_eq!(mirror, before);

        assert_eq!(
            mirror.apply(renamed(4, workspace, "Дом")),
            MirrorStep::Applied
        );
        let applied = mirror.clone();
        assert_eq!(
            mirror.apply(renamed(4, workspace, "Сад")),
            MirrorStep::Ignored,
            "повтор ревизии"
        );
        assert_eq!(mirror, applied);
    }

    #[test]
    fn gap_and_reset_need_snapshot_and_keep_state() {
        let (mut mirror, workspace) = mirror();
        let before = mirror.clone();
        assert_eq!(
            mirror.apply(renamed(5, workspace, "Дом")),
            MirrorStep::NeedSnapshot
        );
        assert_eq!(mirror.apply(Delivery::Reset), MirrorStep::NeedSnapshot);
        assert_eq!(mirror, before);
    }

    #[test]
    fn snapshot_replaces_unless_older() {
        let (mut mirror, workspace) = mirror();
        let mut newer = (*mirror.state).clone();
        newer.workspaces.get_mut(&workspace).unwrap().title = title("Дом");
        let older = Snapshot {
            revision: Revision(2),
            state: Arc::new(State::default()),
        };
        assert_eq!(mirror.apply(Delivery::Snapshot(older)), MirrorStep::Ignored);
        assert_eq!(mirror.revision(), Revision(3));

        let newer = Snapshot {
            revision: Revision(7),
            state: Arc::new(newer),
        };
        assert_eq!(
            mirror.apply(Delivery::Snapshot(newer.clone())),
            MirrorStep::Applied
        );
        assert_eq!(mirror, Mirror::from_snapshot(newer));
        assert_eq!(
            mirror.apply(renamed(8, workspace, "Сад")),
            MirrorStep::Applied
        );
        assert_eq!(mirror.window_title().as_deref(), Some("Сад"));
    }
}
