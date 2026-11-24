//! Ядро shellio без зависимостей от AppKit и SQLite.

#![forbid(unsafe_code)]

mod actor;
mod change;
mod clock;
mod command;
mod error;
mod events;
mod handle;
mod handler;
mod ids;
mod layout_cmd;
#[cfg(feature = "test-support")]
mod loopback;
mod mirror;
mod model;
mod panel;
mod panel_cmd;
mod persist;
mod plugin_host;
mod query;
mod root;
mod title;
mod tree;
mod ui_command;
mod wire;

pub use actor::{QUEUE_CAPACITY, SUBSCRIBER_CAPACITY};
pub use change::Change;
pub use clock::{Clock, SystemClock, UnixMillis};
pub use command::{Command, CommandValue, OpenOutcome, open_workbench};
pub use error::{CoreError, Entity, ErrorCode};
pub use events::{EVENT_BUFFER, EventsSince, Snapshot};
pub use handle::{Core, CoreHandle, ReplyReceipt, StartError};
pub use handler::BOOTSTRAP_WORKSPACE;
pub use ids::{
    AgentId, IdGen, IdParseError, PanelId, ProjectId, ProjectRootId, SessionId, SystemIds,
    WorkbenchId, WorkspaceId,
};
pub use mirror::{Delivery, Mirror, MirrorStep, window_title, workspace_name};
pub use model::{
    Diagnostic, EntityRef, PositionScope, Project, ProjectRoot, Revision, Route, State, Violation,
    WindowState, Workbench, Workspace,
};
pub use panel::{
    NAVIGATOR_PLUGIN, NAVIGATOR_TITLE, Panel, PanelData, PanelStatus, PluginId, PluginIdError,
    TERMINAL_PLUGIN, TERMINAL_TITLE, WorkbenchLayout, is_navigator, layout_metrics,
};
pub use persist::{CommitError, LoadError, Loaded, Persist};
pub use plugin_host::{STATUS_RULES, fingerprint, min_size};
pub use query::{
    LayoutPanel, LayoutView, PanelView, PluginsView, Query, StatusView, StorageInfo, read,
};
pub use root::{FsRootResolver, ResolvedRoot, RootError, RootKey, RootKind, RootResolver};
pub use title::{Title, TitleError};
pub use tree::{TreeProject, TreeRoot, TreeScope, TreeView, TreeWorkbench, TreeWorkspace};
pub use ui_command::{AppearanceChoice, UiCommand};
pub use wire::{Event, Reply, ReplyValue};

/// Тестовая поддержка ядра, доступная только с функцией `test-support`.
#[cfg(feature = "test-support")]
pub mod test_support {
    pub use crate::clock::ManualClock;
    pub use crate::ids::SequentialIds;
    pub use crate::loopback::{Loopback, LoopbackEvents, WireError, Wired};
    pub use crate::persist::{CommitBarrier, Faults, FaultyPersist, MemoryPersist};
    pub use crate::plugin_host::{
        COUNTER_MAX, Counter, TestInit, TestMigrate, TestPlugin, registry, registry_of,
    };
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
