//! Ядро shellio без зависимостей от AppKit и SQLite.

#![forbid(unsafe_code)]

mod change;
mod clock;
mod command;
mod error;
mod handler;
mod ids;
mod model;
mod query;
mod root;
mod title;
mod tree;
mod wire;

pub use change::Change;
pub use clock::{Clock, SystemClock, UnixMillis};
pub use command::{Command, CommandValue, PreparedCommand};
pub use error::{CoreError, Entity, ErrorCode};
pub use handler::{BOOTSTRAP_WORKSPACE, Ctx, Outcome, handle, startup};
pub use ids::{
    AgentId, IdGen, IdParseError, PanelId, ProjectId, ProjectRootId, SessionId, SystemIds,
    WorkbenchId, WorkspaceId,
};
pub use model::{
    Diagnostic, EntityRef, PositionScope, Project, ProjectRoot, Revision, Route, State, Violation,
    WindowState, Workbench, Workspace,
};
pub use query::{Query, StatusView, StorageInfo, read};
pub use root::{ResolvedRoot, RootError, RootKey, RootKind, RootResolver};
pub use title::{Title, TitleError};
pub use tree::{TreeProject, TreeRoot, TreeScope, TreeView, TreeWorkbench, TreeWorkspace};
pub use wire::{Event, Reply, ReplyValue};

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
