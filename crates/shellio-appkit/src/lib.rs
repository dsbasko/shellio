//! Безопасный слой shellio над AppKit (ADR-0006, ADR-0020).
//!
//! Продуктовый `unsafe` вокруг Objective-C живёт здесь, типы objc2 наружу не выходят. P1.2 даёт
//! `NSApplication` без окна приложения: [`App::run`] крутит цикл главного потока с политикой
//! `.accessory` (без Dock, но с правом стать активным), [`App::become_regular`] переводит владельца
//! в `.regular`, [`App::activate`] просит ему передний план. Второй запуск показывает служебное
//! окно за пределами всех экранов ([`App::show_service_window`]), просит передний план себе
//! ([`App::request_activation`]), узнаёт об активности событием [`Delegate::did_become_active`] и
//! передаёт её владельцу по PID ([`App::hand_off_activation_to`]) с фактами [`Transfer`]. Без
//! собственной активности он просит LaunchServices открыть бандл владельца
//! ([`App::reopen_bundle_of`], ответ [`Reopened`]). [`App::finish_terminate`] отпускает приложение
//! после порядка завершения. Другие потоки попадают на главный только через [`MainThread`] и не
//! ждут его.
//!
//! P1.3 добавляет безопасный слой для видов: владельца вида со слабыми обратными вызовами и
//! отвязкой в `Drop`, подписку на уведомления значением, изменения слоёв без неявных анимаций,
//! счётчики экземпляров ([`instance_counts`], [`late_callbacks`]), подложку [`Surface`], наблюдатель
//! ячейки темы [`CellObserver`] и монитор цикла главного потока [`LoopMonitor`]. Окно владельца
//! [`MainWindow`] с заголовком [`ChromeBar`] создаётся после выбора роли, главное меню
//! [`MainMenu`] вызывает один обработчик действий, повторное открытие приложения приходит
//! делегату ([`Delegate::reopen`]). Компоненты ADR-0002 [`TextField`], [`Menu`] (с кнопкой
//! [`MenuButton`]), [`TabStrip`], [`VirtualList`] и [`TreeList`] разделены на чистую часть и вид; главное меню построено на
//! [`Menu`]. Смена состояния компонента отдаёт [`Transition`] для журнала анимаций.
//! Компоненты размещаются в окне и в подложке через [`Piece`], галерея получает маркер поколения
//! [`GenerationMarker`] и слабое наблюдение за видами освобождённого дерева [`ViewWatch`].
//!
//! P1.4 добавляет компоненты раскладки: сплит [`SplitView`] на обычных `NSSplitViewItem` с
//! контейнером [`Pane`], плитку части [`Plate`], контейнер панели [`PanelView`], заглушку
//! «Развернуть» [`ExpandStub`] и сообщение области зон [`EmptyNote`].
//!
//! P1.5 отделяет от контейнера панели его тело: нейтральная карточка [`NeutralCard`] — отдельный
//! компонент, контейнер [`PanelView`] держит любое тело ([`PanelView::set_body`]).

#![deny(clippy::undocumented_unsafe_blocks)]

mod app;
mod calibration;
mod chrome_bar;
mod command_palette;
mod components;
mod counters;
mod font;
mod icon;
mod inspect;
mod key_badge;
mod key_monitor;
mod key_sink;
mod layer;
mod main_menu;
mod marker;
mod notify;
mod own;
mod panel_stub;
mod piece;
mod plate;
mod runloop;
mod split_view;
mod status_line;
mod style;
mod surface;
mod theme_cell;
mod window;

pub use app::{App, Delegate, MainThread, Reopened, RunError, Transfer};
pub use calibration::{
    CalibrationFigure, CalibrationIcon, CalibrationOp, CalibrationShape, CalibrationSheet,
    CalibrationSpec, CalibrationText,
};
pub use chrome_bar::{
    ChromeBar, ChromeIntent, ChromeLayout, ChromeState, Frame, NO_WORKSPACE, ZoneButton, shorten,
    zone_button, zone_pressed,
};
pub use command_palette::layout::{
    GroupFrames as PaletteGroupFrames, HintFrames as PaletteHintFrames,
    HintSpec as PaletteHintSpec, ListContent as PaletteListContent, PaletteLayout, PaletteSpec,
    RowParts as PaletteRowParts, layout as palette_layout, row_parts as palette_row_parts,
};
pub use command_palette::rows::{
    CommandPaletteState, PLACEHOLDER as PALETTE_PLACEHOLDER, PaletteGroupSpec, PaletteRowSpec,
};
pub use command_palette::{CommandPalette, PaletteIntent, shadow_margin as palette_shadow_margin};
pub use components::menu::{
    ItemChange, Menu, MenuButton, MenuEntry, MenuError, MenuIntent, MenuItem, MenuKey, MenuOp,
    MenuState, check_ids as check_menu_ids, diff as menu_diff,
};
pub use components::tab_strip::layout::{
    Regime, StripLayout, TabParts, layout as tab_strip_layout, regime as tab_strip_regime,
    tab_parts,
};
pub use components::tab_strip::model::{
    ButtonKeys, DRAG_THRESHOLD, DragState, NO_KEY, PAYLOAD_ITEM, Presentation, SplitAxis,
    StripButton, StripKey, StripOp, Tab, TabChange, TabDrag, TabError, TabIntent, TabOp, TabPoint,
    TabStatus, TabStripState, ZoneSide, diff as tab_strip_diff, diff_tabs, key_intent,
    menu_entries as tab_menu_entries,
};
pub use components::tab_strip::{ButtonFacts, TabFacts, TabStrip, TabStripFacts};
pub use components::text_field::{
    FILTER_PLACEHOLDER, FieldFocus, FieldMetrics, TextField, TextFieldIntent, TextFieldKind,
    TextFieldOp, TextFieldState, filter_size, preferred_height as text_field_height,
};
pub use components::tree_list::model::{
    TreeListError, TreeListIntent, TreeListKey, TreeListState, TreeNode, TreeOp, TreeRow,
    check as check_tree_list, collapse as tree_list_collapse, diff as tree_list_diff,
    expand as tree_list_expand, key_intent as tree_list_key_intent,
    set_expanded as tree_list_set_expanded,
};
pub use components::tree_list::{TreeList, TreeListFacts, TreeRowFacts};
pub use components::virtual_list::model::{
    KeyPress, ListError, ListIntent, ListKey, ListOp, ListState, PressLog, Row, RowDiff, RowKind,
    RowOffsets, check as check_list, diff as list_diff, diff_rows, key_intent as list_key_intent,
    reveal as list_reveal, scroll_to as list_scroll_to, visible_rows,
};
pub use components::virtual_list::{ListRowFacts, VirtualList, VirtualListFacts};
pub use counters::{
    CountKind, InstanceCount, LateCallbacks, instance_counts, late_callback, late_callbacks,
    late_callbacks_total,
};
pub use inspect::{
    InspectNode, Inspection, MaterialFact, NodeState, OwnedWindowFacts, ScreenFacts, Transition,
    TransitionPath, ViewFacts, WindowFacts, ck, into_parent,
};
pub use key_monitor::{
    KeyFocus, KeyMods, KeyMonitor, KeyResponder, KeyStroke, MonitorEvent, Verdict,
};
pub use key_sink::{KeySink, KeySinkEvent};
pub use main_menu::{MENU_ACTIONS, MainMenu, MenuAction, MenuKeys, PanelRow, ZoneMenu};
pub use marker::{GenerationMarker, MARKER_CELL, MARKER_CELLS, marker_bit};
pub use own::ComponentId;
pub use panel_stub::{
    EXPAND_TITLE, EmptyNote, ExpandStub, NeutralCard, PanelCard, PanelView, StubCard, StubFrames,
    StubIntent, stub_layout,
};
pub use piece::{Piece, ViewWatch};
pub use plate::{Plate, plate_spec};
pub use runloop::{
    DrawMark, ITERATION_BUDGET_MS, IterationStats, LoopMonitor, LoopStats, MonitorError,
};
pub use split_view::{
    Insets, Pane, SplitDirection, SplitDrag, SplitSlot, SplitSpec, SplitView, drag_end,
};
pub use status_line::{StatusHint, StatusLine, chip_badges, key_badges};
pub use style::{
    FontFacts, IconFacts, Padding, Rgba, ShadowFacts, Side, Style, css_weight, seen_fill,
};
pub use surface::{
    DROP_LINE_WIDTH, DropPaint, FocusLayer, LINE_WIDTH, Surface, SurfaceFill, SurfacePaint,
    SurfaceRadius, SurfaceSpec,
};
pub use theme_cell::{CellObserver, SystemInputs, ThemeSelection};
pub use window::{DEFAULT_TITLE, MainWindow, Startup, WINDOW_HEIGHT, WINDOW_WIDTH};
