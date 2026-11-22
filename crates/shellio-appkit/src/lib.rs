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
//! [`MenuButton`]) и [`TabStrip`] разделены на чистую часть и вид; главное меню построено на
//! [`Menu`]. Смена состояния компонента отдаёт [`Transition`] для журнала анимаций.
//! Компоненты размещаются в окне и в подложке через [`Piece`], галерея получает маркер поколения
//! [`GenerationMarker`] и слабое наблюдение за видами освобождённого дерева [`ViewWatch`].

#![deny(clippy::undocumented_unsafe_blocks)]

mod app;
mod chrome_bar;
mod components;
mod counters;
mod inspect;
mod layer;
mod main_menu;
mod marker;
mod notify;
mod own;
mod piece;
mod runloop;
mod surface;
mod theme_cell;
mod window;

pub use app::{App, Delegate, MainThread, Reopened, RunError, Transfer};
pub use chrome_bar::{
    ChromeBar, ChromeIntent, ChromeLayout, ChromeState, Frame, NO_WORKSPACE, shorten,
};
pub use components::menu::{
    ItemChange, Menu, MenuButton, MenuEntry, MenuError, MenuIntent, MenuItem, MenuKey, MenuOp,
    MenuState, check_ids as check_menu_ids, diff as menu_diff,
};
pub use components::tab_strip::layout::{
    Regime, StripLayout, TabParts, layout as tab_strip_layout, regime as tab_strip_regime,
    tab_parts,
};
pub use components::tab_strip::model::{
    ButtonKeys, NO_KEY, PAYLOAD_ITEM, Presentation, SplitAxis, StripButton, StripKey, StripOp, Tab,
    TabChange, TabError, TabIntent, TabOp, TabStatus, TabStripState, ZoneSide,
    diff as tab_strip_diff, diff_tabs, key_intent, menu_entries as tab_menu_entries,
};
pub use components::tab_strip::{ButtonFacts, TabFacts, TabStrip, TabStripFacts};
pub use components::text_field::{
    TextField, TextFieldIntent, TextFieldOp, TextFieldState, preferred_height as text_field_height,
};
pub use counters::{
    CountKind, InstanceCount, LateCallbacks, instance_counts, late_callbacks, late_callbacks_total,
};
pub use inspect::{
    InspectNode, Inspection, MaterialFact, NodeState, ScreenFacts, Transition, TransitionPath,
    ViewFacts, WindowFacts,
};
pub use main_menu::{MainMenu, MenuAction};
pub use marker::{GenerationMarker, MARKER_CELL, MARKER_CELLS, marker_bit};
pub use own::ComponentId;
pub use piece::{Piece, ViewWatch};
pub use runloop::{
    DrawMark, ITERATION_BUDGET_MS, IterationStats, LoopMonitor, LoopStats, MonitorError,
};
pub use surface::{
    FocusLayer, LINE_WIDTH, Surface, SurfaceFill, SurfacePaint, SurfaceRadius, SurfaceSpec,
};
pub use theme_cell::{CellObserver, SystemInputs, ThemeSelection};
pub use window::{DEFAULT_TITLE, MainWindow, Startup, WINDOW_HEIGHT, WINDOW_WIDTH};
