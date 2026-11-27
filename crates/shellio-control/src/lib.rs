//! Управление shellio: протокол v2 управляющего сокета, выбор владельца, сроки, сервер
//! и клиент.
//! Без зависимостей от AppKit.

#![forbid(unsafe_code)]

mod client;
mod clock;
mod codec;
mod error;
mod instance;
mod keymap;
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
pub use keymap::{
    KeymapDiagnostic, KeymapHandler, KeymapRequest, KeymapRow, KeymapSource, KeymapView,
    WithoutKeymap,
};
pub use server::{LaunchHandler, SHUTDOWN_GRACE, Server, ServerConfig, UiHandler, WithoutUi};
pub use ui::{
    Accepted, AnimationPath, AnimationRecord, Axis, AxisSource, BodiesBuiltView, CellContrast,
    CellMotion, CellView, ChromeView, ClassAliveView, CmdlineOutcome, CmdlineRunView,
    CmdlineStateView, ColorSpaceView, ComponentState, ComponentView, Contrast, ExitChordState,
    FontView, GalleryCommand, GalleryFocus, GalleryStrip, GalleryTreeView, GalleryView,
    HintLabelView, HintsStateView, HistoryPoint, HistoryView, IconView, InputEventKind,
    InputEventView, InputJournalView, InsetsView, InspectView, InstanceKind, InstanceView,
    IntentKind, IntentRecord, KeyLatencyView, KeyPressView, LateCallbackView, Lightness, LineSide,
    ListRowView, MaterialView, MenuItemView, MenuView, ModeReceiver, ModeView, Motion,
    OverlaySourceView, OverlayView, OverlayViewKind, OwnedWindowRole, OwnedWindowView,
    PaletteCategory, PaletteChange, PaletteGroupView, PaletteLatencyView, PaletteRowView,
    PaletteSampleView, PaletteStateView, PanelMode, PanelNodeView, PartNodeView, ReceiptSource,
    ReceiptView, ReceiptsView, Rect, Rgba, RowViewsView, RunLoopView, ScreenView, ShadowView, Size,
    StartupView, StatsView, StripButtonView, StyleView, TabAxis, TabItemView, TabRegime,
    TabStatusView, TabStripView, TabZone, TextFieldView, TreeListView, TreeMutation, TreeRowView,
    UI_TIMEOUT, UiGate, UiRefusal, UiReply, UiRequest, VirtualListView, WhichItemView,
    WhichKeyView, WindowMode, WindowView,
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
