//! Ядро shellio без зависимостей от AppKit и SQLite.

#![forbid(unsafe_code)]

mod clock;
mod ids;
mod model;
mod root;
mod title;

pub use clock::{Clock, SystemClock, UnixMillis};
pub use ids::{
    AgentId, IdGen, IdParseError, PanelId, ProjectId, ProjectRootId, SessionId, SystemIds,
    WorkbenchId, WorkspaceId,
};
pub use model::{
    Diagnostic, EntityRef, PositionScope, Project, ProjectRoot, Revision, Route, State, Violation,
    WindowState, Workbench, Workspace,
};
pub use root::{ResolvedRoot, RootError, RootKey, RootKind, RootResolver};
pub use title::{Title, TitleError};

/// Тестовая поддержка ядра, доступная только с функцией `test-support`.
#[cfg(feature = "test-support")]
pub mod test_support {
    pub use crate::clock::ManualClock;
    pub use crate::ids::SequentialIds;
    pub use crate::root::FakeRootResolver;
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_support_enabled_without_flags() {
        const {
            assert!(
                cfg!(feature = "test-support"),
                "функция test-support не подключена dev-зависимостью crate на себя"
            )
        };
    }
}
