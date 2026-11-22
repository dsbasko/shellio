//! Управление shellio: протокол v1 управляющего сокета, выбор владельца, сроки, сервер
//! и клиент.
//! Без зависимостей от AppKit.

#![forbid(unsafe_code)]

mod client;
mod clock;
mod codec;
mod error;
mod instance;
mod server;
mod ui;
mod wire;

pub use client::{
    Address, AddressError, CLI_TIMEOUT, Client, ClientError, ConnectError, Exit, SOCKET_VAR,
    Session, Subscription,
};
pub use clock::{Clock, Expired, RETRY_INTERVAL, TokioClock};
pub use codec::{LineError, LineReader, REQUEST_LINE_LIMIT, RESPONSE_LINE_LIMIT, encode_line};
pub use error::{ControlError, ControlErrorCode};
pub use instance::{
    InstanceError, LOCK_FILE, MAX_SOCKET_PATH, NodeKind, Ownership, Probe, SOCKET_FILE,
    SocketPaths, admit_peer, check_socket_path, is_own_uid, lock_path, probe_owner, socket_path,
};
pub use server::{LaunchHandler, SHUTDOWN_GRACE, Server, ServerConfig, UiHandler, WithoutUi};
pub use ui::{
    Accepted, AnimationPath, AnimationRecord, Axis, AxisSource, ChromeView, ClassAliveView,
    ColorSpaceView, ComponentState, ComponentView, Contrast, GalleryCommand, GalleryFocus,
    GalleryTreeView, GalleryView, InspectView, InstanceKind, InstanceView, IntentKind,
    IntentRecord, LateCallbackView, Lightness, MaterialView, MenuItemView, MenuView, Motion, Rect,
    RowViewsView, RunLoopView, ScreenView, Size, StartupView, StatsView, TextFieldView, UI_TIMEOUT,
    UiGate, UiRefusal, UiReply, UiRequest, WindowMode, WindowView,
};
pub use wire::{
    Activation, Hello, HelloReply, Launch, LaunchReceipt, OkValue, OwnerState, PROTOCOL,
    RefuseReason, RefusedArg, Rejected, Request, RequestBody, RequestId, Response,
};

/// Тестовая поддержка управления, доступная только с функцией `test-support`.
#[cfg(feature = "test-support")]
pub mod test_support {
    pub use crate::clock::manual::{ManualClock, ManualDeadline};
    pub use crate::server::hooks::{Hold, ServerHooks};
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
