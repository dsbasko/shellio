//! Запрос `ui` протокола (ADR-0007, «Уточнено в P1.3» и «Уточнено в `prototype-conformance`»):
//! чтения окна владельца, действия интерфейса [`UiCommand`] и отметка начала выполнения на главном
//! потоке.
//!
//! Кадр запроса — объект с одним ключом, именем запроса: `{"ui.window": {}}`,
//! `{"ui.appearance.set": {"appearance": "light"}}`. Ответ лежит под ключом `ui` значения `ok`
//! и тоже несёт один ключ: `window`, `inspect`, `stats`, `receipts`, `cell`, `mode`, `history`,
//! `overlay`, `cmdline`, `input_journal` или `accepted`.
//!
//! Служебные команды галереи [`GalleryCommand`] (`gallery.*`) выполняются только в режиме галереи,
//! иначе отказ `gallery_only`. Ответ — та же квитанция `accepted` с поколением состояния, которое
//! сценарий ждёт по `ui.window` и маркеру поколения в окне.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};
use shellio_core::{
    AppearanceChoice, HistoryStep, ListAction, ModeAction, OpenOutcome, OverlayKind, PanelId,
    PanelStatus, Revision, ToggleTarget, UiCommand, WorkbenchId, WorkspaceId,
};
use shellio_layout::{SizeClass, Zone};

/// Срок запроса `ui` на главном потоке владельца.
pub const UI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Запрос `ui`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "WireUiRequest", into = "WireUiRequest")]
pub enum UiRequest {
    /// `ui.window`: окно владельца.
    Window,
    /// `ui.inspect`: видимое дерево компонентов окна.
    Inspect,
    /// `ui.stats`: счётчики, монитор цикла и старт процесса. После чтения владелец сливает пул
    /// автоосвобождения цикла событий: следующее чтение видит освобождения, отложенные до ввода.
    Stats,
    /// `ui.receipts`: квитанции действий главного потока с номером больше `after`.
    /// Чтение ничего не меняет.
    Receipts { after: Option<u64> },
    /// `ui.mode`: режим панели с фокусом и автомат аккорда выхода. Чтение ничего не меняет.
    Mode,
    /// `ui.history`: точки истории переходов активного воркспейса и курсор. Чтение ничего не
    /// меняет.
    History,
    /// `gallery.input.journal`: журнал событий клавиш, принятых приёмником ввода галереи. Чтение
    /// ничего не меняет; вне галереи — отказ `gallery_only`.
    InputJournal,
    /// Действие интерфейса: тот же обработчик, что у главного меню.
    Command(UiCommand),
    /// `panel.filter` с полем `text`: текст фильтра панели с фокусом, пустой снимает фильтр. Тот
    /// же запрос без `text` — действие [`UiCommand::PanelFilter`].
    Filter { text: String },
    /// Служебная команда галереи.
    Gallery(GalleryCommand),
    /// `ui.cell`: переопределение контраста и движения в окне владельца с явным
    /// `SHELLIO_STATE_DIR`. `None` — ось не меняется, `System` снимает её переопределение.
    Cell {
        contrast: Option<CellContrast>,
        motion: Option<CellMotion>,
    },
    /// `ui.window.size`: размер окна владельца с явным `SHELLIO_STATE_DIR` в точках. Сторона меньше
    /// наименьшего размера окна поднимается до него, верхний левый угол остаётся на месте. Итог
    /// читает `ui.window`.
    WindowSize { width: u32, height: u32 },
    /// `ui.latency`: новая серия измерителя задержки палитры обычного окна у владельца с явным
    /// `SHELLIO_STATE_DIR`. Ряды очищены, `ticks` — такты `CADisplayLink` окна владельца на время
    /// серии (блоки A протокола), без них display link снят. Ряды читает `ui.stats`.
    Latency { ticks: bool },
    /// `ui.overlay`: открытое наложение, его источник и состав. Чтение ничего не меняет.
    Overlay,
    /// `palette.open` с запросом или категорией. Тот же запрос без них — действие
    /// [`UiCommand::OverlayOpen`].
    PaletteOpen {
        query: Option<String>,
        category: Option<PaletteCategory>,
    },
    /// Служебное закрытие `palette.close`, `hints.close` или `cmdline.close` без имени keymap:
    /// источник восстанавливается, как при `Esc`.
    OverlayClose(OverlayKind),
    /// `cmdline.run`: выполнить текст командной строки, как `Enter` в её поле.
    CmdlineRun { text: String },
}

/// Категория выдачи палитры на проводе и в CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PaletteCategory {
    #[serde(rename = "all")]
    All,
    #[serde(rename = "cmd")]
    Commands,
    #[serde(rename = "panel")]
    Panels,
}

impl PaletteCategory {
    pub const ALL: [Self; 3] = [Self::All, Self::Commands, Self::Panels];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Commands => "cmd",
            Self::Panels => "panel",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|category| category.as_str() == value)
    }
}

/// Контраст в запросе `ui.cell`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellContrast {
    /// Снять переопределение: контраст даёт система.
    System,
    Normal,
    Increased,
}

impl CellContrast {
    pub const ALL: [Self; 3] = [Self::System, Self::Normal, Self::Increased];

    /// Переопределение оси; `None` — ось следует системе.
    pub const fn value(self) -> Option<Contrast> {
        match self {
            Self::System => None,
            Self::Normal => Some(Contrast::Normal),
            Self::Increased => Some(Contrast::Increased),
        }
    }
}

/// Движение в запросе `ui.cell`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellMotion {
    /// Снять переопределение: движение даёт система.
    System,
    Normal,
    Reduced,
}

impl CellMotion {
    pub const ALL: [Self; 3] = [Self::System, Self::Normal, Self::Reduced];

    /// Переопределение оси; `None` — ось следует системе.
    pub const fn value(self) -> Option<Motion> {
        match self {
            Self::System => None,
            Self::Normal => Some(Motion::Normal),
            Self::Reduced => Some(Motion::Reduced),
        }
    }
}

/// Служебная команда галереи (`gallery.*`): вход тестов без keymap, только в режиме галереи.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GalleryCommand {
    /// `gallery.cell`: переопределение светлоты и контраста в окне галереи. `None` — ось без
    /// переопределения галереи: её значение даёт выбор `ui.appearance.set` или система.
    Cell {
        appearance: Option<Lightness>,
        contrast: Option<Contrast>,
    },
    /// `gallery.motion`: переопределение движения в окне галереи.
    Motion(Motion),
    /// `gallery.focus`: first responder окна галереи.
    Focus(GalleryFocus),
    /// `gallery.menu.open`: открыть меню кнопки «…» тем же путём, что нажатие.
    MenuOpen,
    /// `gallery.rebuild`: освободить дерево компонентов галереи и построить его заново.
    Rebuild,
    /// `gallery.window.recreate`: закрыть и отпустить окно галереи, создать новое.
    WindowRecreate,
    /// `gallery.place`: перенести окно галереи на экран с номером из `ui.window.screens`.
    Place { screen: u32 },
    /// `gallery.reset`: вернуть состояние галереи к фикстуре; поколение не сбрасывается.
    Reset,
    /// `gallery.tabs.select`: выбрать вкладку с номером `tab` в полосе `strip` тем же путём, что
    /// намерение выбора полосы.
    TabsSelect { strip: GalleryStrip, tab: u32 },
    /// `gallery.list.replace`: заменить строки списка фикстурой зерна `seed` того же размера.
    ListReplace { seed: u64 },
    /// `gallery.list.scroll`: прокрутить список так, чтобы строка `row` стояла у верхнего края, в
    /// пределах прокрутки.
    ListScroll { row: u32 },
    /// `gallery.list.ticks`: такты `CADisplayLink` во время серии задержки клавиш списка, со
    /// следующего получения фокуса. По умолчанию серия идёт без них.
    ListTicks { enabled: bool },
    /// `gallery.tree.mutate`: изменить дерево галереи; раскрытие и выбор следуют за узлами.
    TreeMutate { op: TreeMutation },
    /// `gallery.calibration`: показать страницу «Калибровка» вместо сцены компонентов;
    /// `gallery.reset` возвращает сцену.
    Calibration,
    /// `gallery.input.reset`: очистить журнал приёмника ввода; приёмник и его поколение прежние.
    InputReset,
}

impl GalleryCommand {
    /// Имя команды в кадре и в ADR-0007.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cell { .. } => "gallery.cell",
            Self::Motion(_) => "gallery.motion",
            Self::Focus(_) => "gallery.focus",
            Self::MenuOpen => "gallery.menu.open",
            Self::Rebuild => "gallery.rebuild",
            Self::WindowRecreate => "gallery.window.recreate",
            Self::Place { .. } => "gallery.place",
            Self::Reset => "gallery.reset",
            Self::TabsSelect { .. } => "gallery.tabs.select",
            Self::ListReplace { .. } => "gallery.list.replace",
            Self::ListScroll { .. } => "gallery.list.scroll",
            Self::ListTicks { .. } => "gallery.list.ticks",
            Self::TreeMutate { .. } => "gallery.tree.mutate",
            Self::Calibration => "gallery.calibration",
            Self::InputReset => "gallery.input.reset",
        }
    }
}

/// Изменение дерева галереи.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeMutation {
    /// Новый корень сверху и новая ветка первой в первом корне фикстуры.
    Insert,
    /// Обмен местами первых двух корней фикстуры.
    Reorder,
    /// Свернуть родителя выбранного узла: выбор переходит на него.
    CollapseParentOfSelected,
}

impl TreeMutation {
    pub const ALL: [Self; 3] = [Self::Insert, Self::Reorder, Self::CollapseParentOfSelected];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Reorder => "reorder",
            Self::CollapseParentOfSelected => "collapse_parent_of_selected",
        }
    }
}

/// Полоса вкладок галереи.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GalleryStrip {
    /// Боковая зона, 372 точки, в подложке с фокусом.
    Side,
    /// Центр той же ширины, в подложке с фокусом.
    Center,
    /// Половина центра, 560 точек, в подложке без фокуса.
    Half,
    /// Широкий центр, 1100 точек, в своей плитке.
    Wide,
}

impl GalleryStrip {
    pub const ALL: [Self; 4] = [Self::Side, Self::Center, Self::Half, Self::Wide];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Side => "side",
            Self::Center => "center",
            Self::Half => "half",
            Self::Wide => "wide",
        }
    }
}

/// Компонент, которому `gallery.focus` отдаёт first responder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GalleryFocus {
    /// Само окно: ни один компонент не в фокусе.
    Window,
    /// Поле ввода: first responder — его field editor.
    Field,
    /// Боковая полоса вкладок: стрелки меняют выбор.
    Tabs,
    /// Список: Up и Down меняют выбор.
    List,
    /// Дерево: Up, Down, Left и Right меняют выбор и раскрытие.
    Tree,
    /// Приёмник ввода: клавиши, дошедшие до него в режиме `Input`, пишутся в журнал.
    Input,
}

impl GalleryFocus {
    pub const ALL: [Self; 6] = [
        Self::Window,
        Self::Field,
        Self::Tabs,
        Self::List,
        Self::Tree,
        Self::Input,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Field => "field",
            Self::Tabs => "tabs",
            Self::List => "list",
            Self::Tree => "tree",
            Self::Input => "input",
        }
    }
}

impl UiRequest {
    /// Имя запроса в кадре и в ADR-0007.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Window => "ui.window",
            Self::Inspect => "ui.inspect",
            Self::Stats => "ui.stats",
            Self::Receipts { .. } => "ui.receipts",
            Self::Mode => "ui.mode",
            Self::History => "ui.history",
            Self::InputJournal => "gallery.input.journal",
            Self::Command(command) => command.name(),
            Self::Filter { .. } => UiCommand::PanelFilter.name(),
            Self::Gallery(command) => command.name(),
            Self::Cell { .. } => "ui.cell",
            Self::WindowSize { .. } => "ui.window.size",
            Self::Latency { .. } => "ui.latency",
            Self::Overlay => "ui.overlay",
            Self::PaletteOpen { .. } => OverlayKind::Palette.keymap_name(),
            Self::OverlayClose(OverlayKind::Palette) => "palette.close",
            Self::OverlayClose(OverlayKind::Hints) => "hints.close",
            Self::OverlayClose(OverlayKind::Cmdline) => "cmdline.close",
            Self::CmdlineRun { .. } => "cmdline.run",
        }
    }

    /// Изменяет ли запрос состояние: начатый изменяющий запрос не повторяется.
    pub const fn is_mutation(&self) -> bool {
        match self {
            Self::Window
            | Self::Inspect
            | Self::Stats
            | Self::Receipts { .. }
            | Self::Mode
            | Self::History
            | Self::Overlay
            | Self::InputJournal => false,
            Self::Command(_)
            | Self::Filter { .. }
            | Self::Gallery(_)
            | Self::Cell { .. }
            | Self::WindowSize { .. }
            | Self::Latency { .. }
            | Self::PaletteOpen { .. }
            | Self::OverlayClose(_)
            | Self::CmdlineRun { .. } => true,
        }
    }

    /// Отвечает ли `reply` на этот запрос.
    pub const fn answered_by(&self, reply: &UiReply) -> bool {
        matches!(
            (self, reply),
            (Self::Window, UiReply::Window(_))
                | (Self::Inspect, UiReply::Inspect(_))
                | (Self::Stats, UiReply::Stats(_))
                | (Self::Receipts { .. }, UiReply::Receipts(_))
                | (Self::Mode, UiReply::Mode(_))
                | (Self::History, UiReply::History(_))
                | (Self::InputJournal, UiReply::InputJournal(_))
                | (Self::Overlay, UiReply::Overlay(_))
                | (Self::CmdlineRun { .. }, UiReply::Cmdline(_))
                | (
                    Self::Command(_)
                        | Self::Filter { .. }
                        | Self::Gallery(_)
                        | Self::WindowSize { .. }
                        | Self::Latency { .. }
                        | Self::PaletteOpen { .. }
                        | Self::OverlayClose(_),
                    UiReply::Accepted(_)
                )
                | (Self::Cell { .. }, UiReply::Cell(_))
        )
    }
}

/// Пустое тело запроса без аргументов: `{}`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptsArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppearanceArgs {
    appearance: AppearanceChoice,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilterArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CellArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    appearance: Option<Lightness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    contrast: Option<Contrast>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UiCellArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    contrast: Option<CellContrast>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    motion: Option<CellMotion>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowSizeArgs {
    width: u32,
    height: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LatencyArgs {
    ticks: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaletteOpenArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    category: Option<PaletteCategory>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CmdlineRunArgs {
    text: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MotionArgs {
    motion: Motion,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FocusArgs {
    component: GalleryFocus,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaceArgs {
    screen: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TabsSelectArgs {
    strip: GalleryStrip,
    tab: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListReplaceArgs {
    seed: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListScrollArgs {
    row: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListTicksArgs {
    enabled: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TreeMutateArgs {
    op: TreeMutation,
}

#[derive(Serialize, Deserialize)]
enum WireUiRequest {
    #[serde(rename = "ui.window")]
    Window(NoArgs),
    #[serde(rename = "ui.inspect")]
    Inspect(NoArgs),
    #[serde(rename = "ui.stats")]
    Stats(NoArgs),
    #[serde(rename = "ui.receipts")]
    Receipts(ReceiptsArgs),
    #[serde(rename = "window.close")]
    WindowClose(NoArgs),
    #[serde(rename = "app.quit")]
    AppQuit(NoArgs),
    #[serde(rename = "ui.appearance.set")]
    AppearanceSet(AppearanceArgs),
    #[serde(rename = "ui.appearance.cycle")]
    AppearanceCycle(NoArgs),
    #[serde(rename = "layout.toggle.navigator")]
    ToggleNavigator(NoArgs),
    #[serde(rename = "layout.toggle.bottom")]
    ToggleBottom(NoArgs),
    #[serde(rename = "layout.toggle.side")]
    ToggleSide(NoArgs),
    #[serde(rename = "layout.toggle.plate")]
    TogglePlate(NoArgs),
    #[serde(rename = "layout.toggle.right")]
    ToggleRight(NoArgs),
    #[serde(rename = "list.down")]
    ListDown(NoArgs),
    #[serde(rename = "list.up")]
    ListUp(NoArgs),
    #[serde(rename = "list.top")]
    ListTop(NoArgs),
    #[serde(rename = "list.bottom")]
    ListBottom(NoArgs),
    #[serde(rename = "list.page_down")]
    ListPageDown(NoArgs),
    #[serde(rename = "list.page_up")]
    ListPageUp(NoArgs),
    #[serde(rename = "list.collapse")]
    ListCollapse(NoArgs),
    #[serde(rename = "list.expand")]
    ListExpand(NoArgs),
    #[serde(rename = "list.activate")]
    ListActivate(NoArgs),
    #[serde(rename = "panel.filter")]
    PanelFilter(FilterArgs),
    #[serde(rename = "ui.mode")]
    Mode(NoArgs),
    #[serde(rename = "mode.navigation")]
    ModeNavigation(NoArgs),
    #[serde(rename = "mode.input")]
    ModeInput(NoArgs),
    #[serde(rename = "mode.toggle")]
    ModeToggle(NoArgs),
    #[serde(rename = "ui.history")]
    History(NoArgs),
    #[serde(rename = "history.back")]
    HistoryBack(NoArgs),
    #[serde(rename = "history.forward")]
    HistoryForward(NoArgs),
    #[serde(rename = "ui.cell")]
    Cell(UiCellArgs),
    #[serde(rename = "ui.window.size")]
    WindowSize(WindowSizeArgs),
    #[serde(rename = "ui.latency")]
    Latency(LatencyArgs),
    #[serde(rename = "ui.overlay")]
    Overlay(NoArgs),
    #[serde(rename = "palette.open")]
    PaletteOpen(PaletteOpenArgs),
    #[serde(rename = "palette.close")]
    PaletteClose(NoArgs),
    #[serde(rename = "hints.open")]
    HintsOpen(NoArgs),
    #[serde(rename = "hints.close")]
    HintsClose(NoArgs),
    #[serde(rename = "cmdline.open")]
    CmdlineOpen(NoArgs),
    #[serde(rename = "cmdline.run")]
    CmdlineRun(CmdlineRunArgs),
    #[serde(rename = "cmdline.close")]
    CmdlineClose(NoArgs),
    #[serde(rename = "gallery.cell")]
    GalleryCell(CellArgs),
    #[serde(rename = "gallery.motion")]
    GalleryMotion(MotionArgs),
    #[serde(rename = "gallery.focus")]
    GalleryFocus(FocusArgs),
    #[serde(rename = "gallery.menu.open")]
    GalleryMenuOpen(NoArgs),
    #[serde(rename = "gallery.rebuild")]
    GalleryRebuild(NoArgs),
    #[serde(rename = "gallery.window.recreate")]
    GalleryWindowRecreate(NoArgs),
    #[serde(rename = "gallery.place")]
    GalleryPlace(PlaceArgs),
    #[serde(rename = "gallery.reset")]
    GalleryReset(NoArgs),
    #[serde(rename = "gallery.tabs.select")]
    GalleryTabsSelect(TabsSelectArgs),
    #[serde(rename = "gallery.list.replace")]
    GalleryListReplace(ListReplaceArgs),
    #[serde(rename = "gallery.list.scroll")]
    GalleryListScroll(ListScrollArgs),
    #[serde(rename = "gallery.list.ticks")]
    GalleryListTicks(ListTicksArgs),
    #[serde(rename = "gallery.tree.mutate")]
    GalleryTreeMutate(TreeMutateArgs),
    #[serde(rename = "gallery.calibration")]
    GalleryCalibration(NoArgs),
    #[serde(rename = "gallery.input.journal")]
    GalleryInputJournal(NoArgs),
    #[serde(rename = "gallery.input.reset")]
    GalleryInputReset(NoArgs),
}

impl From<WireUiRequest> for UiRequest {
    fn from(wire: WireUiRequest) -> Self {
        match wire {
            WireUiRequest::Window(NoArgs {}) => Self::Window,
            WireUiRequest::Inspect(NoArgs {}) => Self::Inspect,
            WireUiRequest::Stats(NoArgs {}) => Self::Stats,
            WireUiRequest::Receipts(ReceiptsArgs { after }) => Self::Receipts { after },
            WireUiRequest::WindowClose(NoArgs {}) => Self::Command(UiCommand::WindowClose),
            WireUiRequest::AppQuit(NoArgs {}) => Self::Command(UiCommand::AppQuit),
            WireUiRequest::AppearanceSet(AppearanceArgs { appearance }) => {
                Self::Command(UiCommand::AppearanceSet(appearance))
            }
            WireUiRequest::AppearanceCycle(NoArgs {}) => Self::Command(UiCommand::AppearanceCycle),
            WireUiRequest::ToggleNavigator(NoArgs {}) => {
                Self::Command(UiCommand::LayoutToggle(ToggleTarget::Navigator))
            }
            WireUiRequest::ToggleBottom(NoArgs {}) => {
                Self::Command(UiCommand::LayoutToggle(ToggleTarget::Bottom))
            }
            WireUiRequest::ToggleSide(NoArgs {}) => {
                Self::Command(UiCommand::LayoutToggle(ToggleTarget::Side))
            }
            WireUiRequest::TogglePlate(NoArgs {}) => {
                Self::Command(UiCommand::LayoutToggle(ToggleTarget::Plate))
            }
            WireUiRequest::ToggleRight(NoArgs {}) => {
                Self::Command(UiCommand::LayoutToggle(ToggleTarget::Right))
            }
            WireUiRequest::ListDown(NoArgs {}) => Self::Command(UiCommand::List(ListAction::Down)),
            WireUiRequest::ListUp(NoArgs {}) => Self::Command(UiCommand::List(ListAction::Up)),
            WireUiRequest::ListTop(NoArgs {}) => Self::Command(UiCommand::List(ListAction::Top)),
            WireUiRequest::ListBottom(NoArgs {}) => {
                Self::Command(UiCommand::List(ListAction::Bottom))
            }
            WireUiRequest::ListPageDown(NoArgs {}) => {
                Self::Command(UiCommand::List(ListAction::PageDown))
            }
            WireUiRequest::ListPageUp(NoArgs {}) => {
                Self::Command(UiCommand::List(ListAction::PageUp))
            }
            WireUiRequest::ListCollapse(NoArgs {}) => {
                Self::Command(UiCommand::List(ListAction::Collapse))
            }
            WireUiRequest::ListExpand(NoArgs {}) => {
                Self::Command(UiCommand::List(ListAction::Expand))
            }
            WireUiRequest::ListActivate(NoArgs {}) => {
                Self::Command(UiCommand::List(ListAction::Activate))
            }
            WireUiRequest::PanelFilter(FilterArgs { text: None }) => {
                Self::Command(UiCommand::PanelFilter)
            }
            WireUiRequest::PanelFilter(FilterArgs { text: Some(text) }) => Self::Filter { text },
            WireUiRequest::Mode(NoArgs {}) => Self::Mode,
            WireUiRequest::ModeNavigation(NoArgs {}) => {
                Self::Command(UiCommand::Mode(ModeAction::Navigation))
            }
            WireUiRequest::ModeInput(NoArgs {}) => {
                Self::Command(UiCommand::Mode(ModeAction::Input))
            }
            WireUiRequest::ModeToggle(NoArgs {}) => {
                Self::Command(UiCommand::Mode(ModeAction::Toggle))
            }
            WireUiRequest::History(NoArgs {}) => Self::History,
            WireUiRequest::HistoryBack(NoArgs {}) => {
                Self::Command(UiCommand::History(HistoryStep::Back))
            }
            WireUiRequest::HistoryForward(NoArgs {}) => {
                Self::Command(UiCommand::History(HistoryStep::Forward))
            }
            WireUiRequest::Cell(UiCellArgs { contrast, motion }) => Self::Cell { contrast, motion },
            WireUiRequest::WindowSize(WindowSizeArgs { width, height }) => {
                Self::WindowSize { width, height }
            }
            WireUiRequest::Latency(LatencyArgs { ticks }) => Self::Latency { ticks },
            WireUiRequest::Overlay(NoArgs {}) => Self::Overlay,
            WireUiRequest::PaletteOpen(PaletteOpenArgs {
                query: None,
                category: None,
            }) => Self::Command(UiCommand::OverlayOpen(OverlayKind::Palette)),
            WireUiRequest::PaletteOpen(PaletteOpenArgs { query, category }) => {
                Self::PaletteOpen { query, category }
            }
            WireUiRequest::PaletteClose(NoArgs {}) => Self::OverlayClose(OverlayKind::Palette),
            WireUiRequest::HintsOpen(NoArgs {}) => {
                Self::Command(UiCommand::OverlayOpen(OverlayKind::Hints))
            }
            WireUiRequest::HintsClose(NoArgs {}) => Self::OverlayClose(OverlayKind::Hints),
            WireUiRequest::CmdlineOpen(NoArgs {}) => {
                Self::Command(UiCommand::OverlayOpen(OverlayKind::Cmdline))
            }
            WireUiRequest::CmdlineRun(CmdlineRunArgs { text }) => Self::CmdlineRun { text },
            WireUiRequest::CmdlineClose(NoArgs {}) => Self::OverlayClose(OverlayKind::Cmdline),
            WireUiRequest::GalleryCell(CellArgs {
                appearance,
                contrast,
            }) => Self::Gallery(GalleryCommand::Cell {
                appearance,
                contrast,
            }),
            WireUiRequest::GalleryMotion(MotionArgs { motion }) => {
                Self::Gallery(GalleryCommand::Motion(motion))
            }
            WireUiRequest::GalleryFocus(FocusArgs { component }) => {
                Self::Gallery(GalleryCommand::Focus(component))
            }
            WireUiRequest::GalleryMenuOpen(NoArgs {}) => Self::Gallery(GalleryCommand::MenuOpen),
            WireUiRequest::GalleryRebuild(NoArgs {}) => Self::Gallery(GalleryCommand::Rebuild),
            WireUiRequest::GalleryWindowRecreate(NoArgs {}) => {
                Self::Gallery(GalleryCommand::WindowRecreate)
            }
            WireUiRequest::GalleryPlace(PlaceArgs { screen }) => {
                Self::Gallery(GalleryCommand::Place { screen })
            }
            WireUiRequest::GalleryReset(NoArgs {}) => Self::Gallery(GalleryCommand::Reset),
            WireUiRequest::GalleryTabsSelect(TabsSelectArgs { strip, tab }) => {
                Self::Gallery(GalleryCommand::TabsSelect { strip, tab })
            }
            WireUiRequest::GalleryListReplace(ListReplaceArgs { seed }) => {
                Self::Gallery(GalleryCommand::ListReplace { seed })
            }
            WireUiRequest::GalleryListScroll(ListScrollArgs { row }) => {
                Self::Gallery(GalleryCommand::ListScroll { row })
            }
            WireUiRequest::GalleryListTicks(ListTicksArgs { enabled }) => {
                Self::Gallery(GalleryCommand::ListTicks { enabled })
            }
            WireUiRequest::GalleryTreeMutate(TreeMutateArgs { op }) => {
                Self::Gallery(GalleryCommand::TreeMutate { op })
            }
            WireUiRequest::GalleryCalibration(NoArgs {}) => {
                Self::Gallery(GalleryCommand::Calibration)
            }
            WireUiRequest::GalleryInputJournal(NoArgs {}) => Self::InputJournal,
            WireUiRequest::GalleryInputReset(NoArgs {}) => {
                Self::Gallery(GalleryCommand::InputReset)
            }
        }
    }
}

impl From<UiRequest> for WireUiRequest {
    fn from(request: UiRequest) -> Self {
        match request {
            UiRequest::Window => Self::Window(NoArgs {}),
            UiRequest::Inspect => Self::Inspect(NoArgs {}),
            UiRequest::Stats => Self::Stats(NoArgs {}),
            UiRequest::Receipts { after } => Self::Receipts(ReceiptsArgs { after }),
            UiRequest::Command(UiCommand::WindowClose) => Self::WindowClose(NoArgs {}),
            UiRequest::Command(UiCommand::AppQuit) => Self::AppQuit(NoArgs {}),
            UiRequest::Command(UiCommand::AppearanceSet(appearance)) => {
                Self::AppearanceSet(AppearanceArgs { appearance })
            }
            UiRequest::Command(UiCommand::AppearanceCycle) => Self::AppearanceCycle(NoArgs {}),
            UiRequest::Command(UiCommand::LayoutToggle(target)) => match target {
                ToggleTarget::Navigator => Self::ToggleNavigator(NoArgs {}),
                ToggleTarget::Bottom => Self::ToggleBottom(NoArgs {}),
                ToggleTarget::Side => Self::ToggleSide(NoArgs {}),
                ToggleTarget::Plate => Self::TogglePlate(NoArgs {}),
                ToggleTarget::Right => Self::ToggleRight(NoArgs {}),
            },
            UiRequest::Command(UiCommand::List(action)) => match action {
                ListAction::Down => Self::ListDown(NoArgs {}),
                ListAction::Up => Self::ListUp(NoArgs {}),
                ListAction::Top => Self::ListTop(NoArgs {}),
                ListAction::Bottom => Self::ListBottom(NoArgs {}),
                ListAction::PageDown => Self::ListPageDown(NoArgs {}),
                ListAction::PageUp => Self::ListPageUp(NoArgs {}),
                ListAction::Collapse => Self::ListCollapse(NoArgs {}),
                ListAction::Expand => Self::ListExpand(NoArgs {}),
                ListAction::Activate => Self::ListActivate(NoArgs {}),
            },
            UiRequest::Command(UiCommand::PanelFilter) => {
                Self::PanelFilter(FilterArgs { text: None })
            }
            UiRequest::Filter { text } => Self::PanelFilter(FilterArgs { text: Some(text) }),
            UiRequest::Mode => Self::Mode(NoArgs {}),
            UiRequest::Command(UiCommand::Mode(action)) => match action {
                ModeAction::Navigation => Self::ModeNavigation(NoArgs {}),
                ModeAction::Input => Self::ModeInput(NoArgs {}),
                ModeAction::Toggle => Self::ModeToggle(NoArgs {}),
            },
            UiRequest::History => Self::History(NoArgs {}),
            UiRequest::InputJournal => Self::GalleryInputJournal(NoArgs {}),
            UiRequest::Command(UiCommand::History(step)) => match step {
                HistoryStep::Back => Self::HistoryBack(NoArgs {}),
                HistoryStep::Forward => Self::HistoryForward(NoArgs {}),
            },
            UiRequest::Cell { contrast, motion } => Self::Cell(UiCellArgs { contrast, motion }),
            UiRequest::WindowSize { width, height } => {
                Self::WindowSize(WindowSizeArgs { width, height })
            }
            UiRequest::Latency { ticks } => Self::Latency(LatencyArgs { ticks }),
            UiRequest::Overlay => Self::Overlay(NoArgs {}),
            UiRequest::Command(UiCommand::OverlayOpen(kind)) => match kind {
                OverlayKind::Palette => Self::PaletteOpen(PaletteOpenArgs {
                    query: None,
                    category: None,
                }),
                OverlayKind::Hints => Self::HintsOpen(NoArgs {}),
                OverlayKind::Cmdline => Self::CmdlineOpen(NoArgs {}),
            },
            UiRequest::PaletteOpen { query, category } => {
                Self::PaletteOpen(PaletteOpenArgs { query, category })
            }
            UiRequest::OverlayClose(OverlayKind::Palette) => Self::PaletteClose(NoArgs {}),
            UiRequest::OverlayClose(OverlayKind::Hints) => Self::HintsClose(NoArgs {}),
            UiRequest::OverlayClose(OverlayKind::Cmdline) => Self::CmdlineClose(NoArgs {}),
            UiRequest::CmdlineRun { text } => Self::CmdlineRun(CmdlineRunArgs { text }),
            UiRequest::Gallery(command) => match command {
                GalleryCommand::Cell {
                    appearance,
                    contrast,
                } => Self::GalleryCell(CellArgs {
                    appearance,
                    contrast,
                }),
                GalleryCommand::Motion(motion) => Self::GalleryMotion(MotionArgs { motion }),
                GalleryCommand::Focus(component) => Self::GalleryFocus(FocusArgs { component }),
                GalleryCommand::MenuOpen => Self::GalleryMenuOpen(NoArgs {}),
                GalleryCommand::Rebuild => Self::GalleryRebuild(NoArgs {}),
                GalleryCommand::WindowRecreate => Self::GalleryWindowRecreate(NoArgs {}),
                GalleryCommand::Place { screen } => Self::GalleryPlace(PlaceArgs { screen }),
                GalleryCommand::Reset => Self::GalleryReset(NoArgs {}),
                GalleryCommand::TabsSelect { strip, tab } => {
                    Self::GalleryTabsSelect(TabsSelectArgs { strip, tab })
                }
                GalleryCommand::ListReplace { seed } => {
                    Self::GalleryListReplace(ListReplaceArgs { seed })
                }
                GalleryCommand::ListScroll { row } => {
                    Self::GalleryListScroll(ListScrollArgs { row })
                }
                GalleryCommand::ListTicks { enabled } => {
                    Self::GalleryListTicks(ListTicksArgs { enabled })
                }
                GalleryCommand::TreeMutate { op } => Self::GalleryTreeMutate(TreeMutateArgs { op }),
                GalleryCommand::Calibration => Self::GalleryCalibration(NoArgs {}),
                GalleryCommand::InputReset => Self::GalleryInputReset(NoArgs {}),
            },
        }
    }
}

/// Ответ на запрос `ui`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiReply {
    Window(Box<WindowView>),
    Inspect(Box<InspectView>),
    Stats(Box<StatsView>),
    Receipts(ReceiptsView),
    /// Оси окна после `ui.cell`.
    Cell(CellView),
    /// Режим панели с фокусом: ответ `ui.mode`.
    Mode(ModeView),
    /// История переходов активного воркспейса: ответ `ui.history`.
    History(HistoryView),
    /// Журнал приёмника ввода галереи: ответ `gallery.input.journal`.
    InputJournal(InputJournalView),
    /// Открытое наложение: ответ `ui.overlay`.
    Overlay(Box<OverlayView>),
    /// Исход строки `:`: ответ `cmdline.run`.
    Cmdline(Box<CmdlineRunView>),
    /// Действие принято обработчиком на главном потоке.
    Accepted(Accepted),
}

/// Вид открытого наложения в `ui.overlay`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayViewKind {
    Palette,
    Hints,
    Cmdline,
    /// Подсказка набранного префикса: не модальна и ввода не держит.
    WhichKey,
}

impl OverlayViewKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Palette => "palette",
            Self::Hints => "hints",
            Self::Cmdline => "cmdline",
            Self::WhichKey => "which_key",
        }
    }
}

/// Источник модального наложения: ему возвращаются фокус и режим при отмене.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlaySourceView {
    /// Панель с фокусом в момент открытия; `null` — панели не было.
    pub panel: Option<PanelId>,
    /// Получатель ввода источника; `null` — панели не было.
    pub receiver: Option<ModeReceiver>,
    /// Режим панели-источника в момент открытия.
    pub mode: PanelMode,
    /// Ввод держало поле панели.
    pub field: bool,
}

/// Строка палитры.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaletteRowView {
    pub id: String,
    pub title: String,
    pub detail: String,
    /// Запись клавиши действия; `null` — привязки нет.
    pub key: Option<String>,
}

/// Группа строк палитры по порядку выдачи.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaletteGroupView {
    pub title: String,
    pub rows: Vec<PaletteRowView>,
}

/// Состав открытой палитры.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaletteStateView {
    pub query: String,
    pub category: PaletteCategory,
    pub groups: Vec<PaletteGroupView>,
    /// `id` выбранной строки; `null` — строк нет.
    pub selected: Option<String>,
    /// Число совпадений до предела строк.
    pub matched: usize,
}

/// Метка `f`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HintLabelView {
    pub text: String,
    /// Ключ сверки цели в `ui.inspect`.
    pub target: String,
    /// Кадр цели в окне владельца.
    pub frame: Rect,
}

/// Состав открытых меток.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HintsStateView {
    /// Набранная часть метки.
    pub typed: String,
    /// Метки, совпавшие с набранной частью.
    pub labels: Vec<HintLabelView>,
}

/// Состав открытой командной строки.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CmdlineStateView {
    pub text: String,
}

/// Продолжение набранного префикса.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhichItemView {
    pub key: String,
    pub label: String,
    /// У продолжения есть более длинные последовательности.
    pub group: bool,
}

/// Подсказка which-key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhichKeyView {
    pub prefix: Vec<String>,
    pub title: Option<String>,
    pub items: Vec<WhichItemView>,
}

/// Ответ `ui.overlay` (ADR-0007, «Уточнено в P1.6 (планирование): наложения»). Ключ состава есть
/// только у открытого вида.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayView {
    /// PID владельца.
    pub pid: u32,
    /// Открытое наложение; `null` — его нет.
    pub kind: Option<OverlayViewKind>,
    /// Источник модального наложения.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<OverlaySourceView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub palette: Option<PaletteStateView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<HintsStateView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<CmdlineStateView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub which_key: Option<WhichKeyView>,
}

/// Исход строки `:`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmdlineOutcome {
    /// Пустая строка: ничего не выполнено.
    Empty,
    /// Действие передано своему обработчику.
    Run,
    /// Ни грамматика, ни откат: сообщение в строке статуса.
    Unknown,
}

impl CmdlineOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Run => "run",
            Self::Unknown => "unknown",
        }
    }
}

/// Ответ `cmdline.run`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CmdlineRunView {
    /// PID владельца.
    pub pid: u32,
    pub text: String,
    pub outcome: CmdlineOutcome,
    /// Имя действия keymap при исходе `run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    /// Сообщение строки статуса при исходе `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Ответ `ui.cell`: оси окна с источниками, как в `ui.window`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellView {
    /// PID владельца.
    pub pid: u32,
    pub lightness: Axis<Lightness>,
    pub contrast: Axis<Contrast>,
    pub motion: Axis<Motion>,
    /// Поколение отрисовки после запроса.
    pub generation: u64,
}

/// Режим клавиатуры панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelMode {
    Navigation,
    Input,
}

impl PanelMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Navigation => "navigation",
            Self::Input => "input",
        }
    }
}

/// Получатель ввода: по нему хранится режим, ему второй аккорд выхода возвращает исходную пару.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeReceiver {
    /// Панель рабочей области: `{"panel": <id>}`.
    Panel(PanelId),
    /// Приёмник ввода галереи: `{"gallery_input": <поколение>}`. Поколение растёт с каждым новым
    /// видом приёмника.
    GalleryInput(u64),
}

/// Автомат аккорда выхода.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitChordState {
    Idle,
    /// Первый аккорд принят: следующий такой же вернёт получателю исходную пару.
    Armed,
}

impl ExitChordState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Armed => "armed",
        }
    }
}

/// Ответ `ui.mode` (ADR-0007, «Уточнено в P1.6 (планирование)»).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeView {
    /// PID владельца.
    pub pid: u32,
    /// Панель с фокусом видимой части открытой рабочей области.
    pub panel: Option<PanelId>,
    /// Режим панели с фокусом; без неё — `navigation`.
    pub mode: PanelMode,
    /// Получатель ввода панели с фокусом; `null` — панель без ввода или панели нет.
    pub receiver: Option<ModeReceiver>,
    pub exit: ExitChordState,
    /// Получатель, которому второй аккорд вернёт пару; ключа нет, пока автомат в `idle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_receiver: Option<ModeReceiver>,
}

/// Точка истории переходов.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryPoint {
    pub workbench: WorkbenchId,
    pub panel: PanelId,
    /// Режим, в котором панель была оставлена.
    pub mode: PanelMode,
}

/// Ответ `ui.history` (ADR-0007, «Уточнено в P1.6 (планирование)»): история активного воркспейса.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryView {
    /// PID владельца.
    pub pid: u32,
    /// Активный воркспейс; `null` — его нет.
    pub workspace: Option<WorkspaceId>,
    /// Точки по порядку переходов, не больше 100.
    pub points: Vec<HistoryPoint>,
    /// Номер текущей точки в `points` с нуля; `null` — история пуста.
    pub cursor: Option<usize>,
}

/// Половина нажатия клавиши в журнале приёмника ввода.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputEventKind {
    Down,
    Up,
}

impl InputEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Up => "up",
        }
    }
}

/// Событие клавиши, принятое приёмником ввода галереи.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputEventView {
    pub kind: InputEventKind,
    /// Виртуальный код физической клавиши.
    pub key_code: u16,
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Автоповтор удержанной клавиши.
    pub repeat: bool,
}

/// Ответ `gallery.input.journal` (ADR-0007, «Уточнено в P1.6 (планирование)»).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputJournalView {
    /// PID владельца.
    pub pid: u32,
    /// Нынешний приёмник: журнал прежнего поколения не отдаётся.
    pub receiver: ModeReceiver,
    /// Режим приёмника.
    pub mode: PanelMode,
    /// Принятые события по порядку, не больше 256 последних.
    pub events: Vec<InputEventView>,
}

/// Квитанции действий главного потока: ответ `ui.receipts` (ADR-0007, «Квитанции UI» и «Уточнено
/// в P1.6 (планирование)»).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptsView {
    /// Номер последней записанной квитанции процесса; 0 — их не было.
    pub last: u64,
    /// Записи кольца с номером больше `after` по возрастанию номера.
    pub receipts: Vec<ReceiptView>,
}

/// Откуда пришло действие главного потока.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptSource {
    /// Клавиша через маршрутизатор.
    Key,
    /// Пункт меню приложения, меню «…» или выбора «Открыть в новой части».
    Menu,
    /// Кнопка заголовка, строки вкладок, заглушки или тела панели.
    Button,
}

impl ReceiptSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Menu => "menu",
            Self::Button => "button",
        }
    }
}

/// Завершение одного действия главного потока: имя, источник и исход. Данных команды в записи
/// нет. Исход — отсутствие `error`; у `panel.open` к нему добавлены `outcome`, `panel` и `revision`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptView {
    /// Монотонный номер в пределах процесса, с 1.
    pub seq: u64,
    /// Имя действия: имя keymap UI-команды или имя команды ядра.
    pub action: String,
    pub source: ReceiptSource,
    /// Рабочая область `panel.open`: при успехе — где стоит панель, при отказе — куда шла команда.
    pub workbench: Option<WorkbenchId>,
    /// Плагин `panel.open`; у остальных действий ключа нет.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// Исход успешной команды; при отказе ключа нет.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<OpenOutcome>,
    /// Панель успешной команды.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<PanelId>,
    /// Ревизия ответа успешной команды ядра; у действия без команды ядра ключа нет.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
    /// Код отказа ядра; ранний отказ главного потока — `busy`, ядро без ответа — `no_reply`,
    /// действию не на что подействовать — `not_applicable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Квитанция действия интерфейса. `app.quit` отвечает до порядка завершения.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accepted {
    /// Имя действия, например `window.close`.
    pub command: String,
    /// PID владельца, выполнившего действие.
    pub pid: u32,
    /// Поколение отрисовки после действия.
    pub generation: u64,
}

/// Прямоугольник в точках, начало внизу слева.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Размер в точках.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

/// Режим окна владельца.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    Normal,
    Gallery,
}

/// Светлота ячейки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lightness {
    Light,
    Dark,
}

/// Контраст ячейки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Contrast {
    Normal,
    Increased,
}

/// Движение.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Motion {
    Normal,
    Reduced,
}

/// Источник значения оси ячейки или движения.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AxisSource {
    #[serde(rename = "system")]
    System,
    /// Выбор `ui.appearance.set`.
    #[serde(rename = "ui.appearance")]
    UiAppearance,
    /// Переопределение галереи в её окне.
    #[serde(rename = "gallery")]
    Gallery,
    /// Переопределение `ui.cell`.
    #[serde(rename = "override")]
    Override,
}

/// Значение оси и его источник.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Axis<T> {
    pub value: T,
    pub source: AxisSource,
}

/// Пространство цвета окна.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColorSpaceView {
    /// `localizedName`; `None` — пространства нет.
    pub name: Option<String>,
    /// Равно `NSColorSpace.sRGBColorSpace`.
    pub srgb: bool,
}

/// Экран.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenView {
    /// Порядковый номер в `NSScreen.screens`.
    pub index: u32,
    pub name: String,
    pub frame: Rect,
    pub backing_scale_factor: f64,
    pub maximum_frames_per_second: i64,
}

/// Окно владельца: ответ `ui.window`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowView {
    pub pid: u32,
    pub mode: WindowMode,
    pub window_number: i64,
    /// Кадр окна в точках экрана.
    pub frame: Rect,
    pub backing_scale_factor: f64,
    pub visible: bool,
    pub key: bool,
    pub color_space: ColorSpaceView,
    /// Системный заголовок окна.
    pub title: String,
    /// Выбор `ui.appearance.set`.
    pub appearance_choice: AppearanceChoice,
    pub lightness: Axis<Lightness>,
    pub contrast: Axis<Contrast>,
    pub motion: Axis<Motion>,
    /// Номер экрана окна в [`Self::screens`]; `None` — окно не на экране.
    pub screen: Option<u32>,
    /// `maximumFramesPerSecond` экрана окна.
    pub maximum_frames_per_second: Option<i64>,
    pub screens: Vec<ScreenView>,
    /// Ревизия зеркала состояния; `None` — снимка ещё не было.
    pub mirror_revision: Option<u64>,
    /// Поколение отрисовки окна.
    pub generation: u64,
    /// Окна владельца: главное окно и окно палитры обычного режима, видимое или нет.
    /// Пустой список — владелец до плана `command-overlays`, ключа в кадре нет.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<OwnedWindowView>,
}

/// Назначение окна владельца в [`WindowView::windows`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnedWindowRole {
    /// Главное окно: то же, что [`WindowView::window_number`].
    Main,
    /// Плавающая панель `CommandPalette`.
    Palette,
}

/// Окно владельца.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedWindowView {
    pub role: OwnedWindowRole,
    pub window_number: i64,
    /// Кадр `NSWindow.frame` в точках AppKit, начало внизу слева основного экрана.
    pub frame: Rect,
    pub backing_scale_factor: f64,
    pub visible: bool,
    pub key: bool,
    /// Номер экрана окна в [`WindowView::screens`]; `None` — окно не на экране.
    pub screen: Option<u32>,
}

/// Видимое дерево компонентов окна: ответ `ui.inspect`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectView {
    pub window_number: i64,
    /// Размер содержимого окна: высота переводит кадры в строки снимка.
    pub content: Size,
    pub generation: u64,
    pub components: Vec<ComponentView>,
    /// Найденные в окне виды и слои материала с прозрачностью.
    pub materials: Vec<MaterialView>,
    /// Состояние галереи; `None` — окно не в режиме галереи, и ключа в кадре нет.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gallery: Option<GalleryView>,
}

/// Состояние галереи в `ui.inspect`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GalleryView {
    /// Номер построения дерева компонентов: растёт с `gallery.rebuild` и
    /// `gallery.window.recreate`.
    pub tree: u64,
    /// Номер окна галереи: растёт с `gallery.window.recreate`.
    pub window: u64,
    /// Переопределения галереи по осям; `None` — ось без переопределения.
    pub appearance: Option<Lightness>,
    pub contrast: Option<Contrast>,
    pub motion: Option<Motion>,
    /// Последний запрос `gallery.focus`.
    pub focus: GalleryFocus,
    /// Последний запрос `gallery.place`; `None` — окно не переносилось.
    pub screen: Option<u32>,
    /// Журнал намерений компонентов галереи, старые записи первыми.
    pub intents: Vec<IntentRecord>,
}

/// Намерение компонента, записанное галереей.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentRecord {
    /// Поколение после записи.
    pub generation: u64,
    /// Идентификатор компонента в `ui.inspect`.
    pub component: String,
    pub intent: IntentKind,
    /// Значение поля у `changed` и `submitted`, id пункта у `chosen`, id вкладки у `select` и
    /// `close`, ось у `split`.
    pub value: Option<String>,
}

/// Вид намерения.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
    Changed,
    Submitted,
    Cancelled,
    Chosen,
    /// Выбор вкладки; значение — id вкладки.
    Select,
    /// Закрытие вкладки; значение — id вкладки.
    Close,
    /// «Разделить»; значение — ось.
    Split,
    /// «Развернуть панель».
    Zoom,
    /// «Вернуть раскладку».
    Restore,
    /// Открыть строку списка или узел дерева; значение — id.
    Activate,
    /// Раскрыть или свернуть узел дерева; значение — id узла.
    Toggle,
}

/// Компонент или его часть.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentView {
    /// Устойчивое имя в окне, например `chrome.workspace`.
    pub id: String,
    /// Класс компонента, например `ChromeBar`.
    pub class: String,
    /// Идентичность объекта вида в процессе.
    pub object: u64,
    /// Имя компонента-родителя.
    pub parent: Option<String>,
    /// Ключ сверки ADR-0037 из таблицы шаблонов `shellio-conformance`; `None` — узел не сверяется.
    pub ck: Option<String>,
    /// Кадр в точках окна, начало внизу слева.
    pub frame: Rect,
    pub visible: bool,
    pub first_responder: bool,
    /// Доступность контрола; `None` — не контрол.
    pub enabled: Option<bool>,
    pub role: Option<String>,
    pub label: Option<String>,
    pub help: Option<String>,
    /// `None` — выбора у компонента нет.
    pub selected: Option<bool>,
    /// `None` — раскрытия у компонента нет.
    pub expanded: Option<bool>,
    /// Ключи анимаций у слоёв вида и его подвидов.
    pub animation_keys: u32,
    /// Свойства вида для полей паспорта, прочитанные у вида и его слоёв; `None` — не читаются.
    pub style: Option<StyleView>,
    /// Обрезан ли текст узла; `None` — текста у узла нет. Полный текст — в `label`.
    pub truncated: Option<bool>,
    /// Состояние компонента; у частей компонента его нет.
    pub state: Option<ComponentState>,
}

/// Цвет sRGB с непрозрачностью 0–1.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f64,
}

/// Внутренние отступы в точках.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsetsView {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

/// Шрифт текста узла.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FontView {
    /// Семейство шрифта (`familyName`).
    pub family: String,
    /// Имя PostScript начертания, например `JetBrainsMono-Regular`.
    pub face: String,
    /// Начертание по шкале CSS: 100–900.
    pub weight: f64,
    /// Кегль в точках.
    pub size: f64,
    /// URL файла фактически выбранного шрифта (`kCTFontURLAttribute`); `None` — файла у шрифта нет.
    pub file: Option<String>,
}

/// Тень слоя.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowView {
    pub color: Rgba,
    pub x: f64,
    pub y: f64,
    pub blur: f64,
    pub spread: f64,
    pub inset: bool,
}

/// Сторона, у которой нарисована линия.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineSide {
    Top,
    Right,
    Bottom,
    Left,
}

impl LineSide {
    /// Стороны по часовой стрелке сверху.
    pub const ALL: [Self; 4] = [Self::Top, Self::Right, Self::Bottom, Self::Left];
}

/// Значок набора.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IconView {
    /// Имя значка в наборе `P`; у символа SF — имя символа.
    pub name: String,
    /// Сторона значка в точках.
    pub size: f64,
}

/// Свойства вида узла `ui.inspect` (ADR-0037, «Контракт полей»). Имена полей — как у элемента
/// паспорта. Отсутствующий ключ — свойства у вида нет.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyleView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<FontView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_height: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub letter_spacing: Option<f64>,
    /// Цвет текста или значка.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgba>,
    /// Заливка; у корня поверхности — заливка, которую видно под ним.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Rgba>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_color: Option<Rgba>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_width: Option<f64>,
    /// Стороны с линией по часовой стрелке сверху.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub line_sides: Vec<LineSide>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding: Option<InsetsView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadow: Vec<ShadowView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<IconView>,
}

/// Состояние компонента по классу.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    Chrome(ChromeView),
    TextField(TextFieldView),
    Menu(MenuView),
    TabStrip(Box<TabStripView>),
    VirtualList(Box<VirtualListView>),
    TreeList(Box<TreeListView>),
    /// Плитка части зоны раскладки или развёрнутой панели (P1.4).
    Part(Box<PartNodeView>),
    /// Вид панели раскладки (P1.4).
    Panel(Box<PanelNodeView>),
}

/// Плитка части зоны в `ui.inspect`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartNodeView {
    /// Зона части; у развёрнутой панели — `center`.
    pub zone: Zone,
    /// Номер части в зоне, у развёрнутой панели — 0.
    pub index: u32,
    /// Вкладки части по порядку.
    pub tabs: Vec<PanelId>,
    pub active: PanelId,
    /// Часть показана: зона не скрыта и не пуста, панель не развёрнута поверх.
    pub shown: bool,
    /// Фокус в этой части.
    pub focused: bool,
    /// Плитка развёрнутой панели.
    pub zoomed: bool,
}

/// Вид панели в `ui.inspect`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelNodeView {
    pub panel: PanelId,
    /// `ComponentId` вида: тот же, пока вид не пересоздан.
    pub component: u64,
    pub plugin: String,
    pub title: String,
    /// Вид показан: активная вкладка показанной части, колонка навигатора или развёрнутая панель.
    pub shown: bool,
    /// Зона тела по геометрии; `None` — вид не показан.
    pub zone: Option<Zone>,
    /// Класс размера тела; `None` — вид не показан.
    pub size: Option<SizeClass>,
    /// Статус панели: тело плагина есть только у `ready`, иначе на его месте заглушка статуса.
    pub status: PanelStatus,
    /// Тело активно: показано в видимом окне и не меньше `min_size`.
    pub active: bool,
    /// Фокус раскладки на этой панели.
    pub focused: bool,
    /// Вариант представления, который получило тело; `None` — тела нет или оно ещё не показано.
    pub variant: Option<String>,
}

/// Сторона зоны полосы вкладок.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabZone {
    Side,
    Center,
    Bottom,
}

/// Ось деления зоны.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabAxis {
    /// Части рядом.
    Columns,
    /// Части одна над другой.
    Rows,
}

/// Режим полосы вкладок по ширине (ADR-0032).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabRegime {
    /// Вкладки, нагрузка и кнопки видны.
    Visible,
    /// Нагрузка в заголовке меню «…».
    InMenu,
    /// Вкладки прокручиваются.
    Scroll,
}

/// Статус вкладки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabStatusView {
    None,
    Running,
    Waiting,
    Error,
    Done,
    Idle,
    Exited,
}

/// Состояние `TabStrip`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabStripView {
    /// AX-имя полосы.
    pub label: String,
    pub zone: TabZone,
    pub axis: TabAxis,
    pub navigator: bool,
    pub zoomed: bool,
    /// Фокус у части: тон и линия под активной вкладкой.
    pub focused: bool,
    /// Полоса — first responder окна.
    pub keyboard_focus: bool,
    /// Режим по последней раскладке; `None` — раскладки ещё не было.
    pub regime: Option<TabRegime>,
    pub selected: Option<String>,
    pub payload: Option<String>,
    /// Нагрузка видна в строке.
    pub payload_visible: bool,
    /// Смещение прокрутки ряда вкладок в точках.
    pub offset: f64,
    pub max_offset: f64,
    pub tabs: Vec<TabItemView>,
    /// Кнопки справа по порядку.
    pub buttons: Vec<StripButtonView>,
}

/// Вкладка полосы.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabItemView {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: TabStatusView,
    /// Показанный значок типа; `None` — значка нет (боковая зона).
    pub icon: Option<String>,
    pub close_visible: bool,
    /// Вкладка видна в ряду целиком.
    pub fully_visible: bool,
}

/// Кнопка полосы.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StripButtonView {
    /// `split`, `zoom`, `restore` или `menu`.
    pub id: String,
    pub label: String,
    /// Имя значка набора прототипа; у «Разделить» он показывает ось.
    pub symbol: String,
    /// `toolTip`: действие и клавиша.
    pub hint: String,
    /// `accessibilityHelp`: клавиша или «Клавиша не назначена».
    pub help: String,
}

/// Состояние `VirtualList`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualListView {
    /// AX-имя таблицы.
    pub label: String,
    pub count: u64,
    pub selected: Option<String>,
    /// Смещение прокрутки в точках и его предел.
    pub offset: f64,
    pub max_offset: f64,
    /// Таблица — first responder окна.
    pub keyboard_focus: bool,
    /// Созданные виды строк и ячеек за жизнь списка.
    pub row_views_created: u64,
    pub cell_views_created: u64,
    /// Видимые строки по порядку.
    pub visible: Vec<ListRowView>,
}

/// Видимая строка списка.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRowView {
    pub index: u64,
    pub id: String,
    pub text: String,
}

/// Состояние `TreeList`. Выбор и раскрытие прочитаны у `NSOutlineView`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeListView {
    /// AX-имя дерева.
    pub label: String,
    /// Строк у `NSOutlineView`.
    pub count: u64,
    /// Узлов во всём дереве.
    pub nodes: u64,
    pub selected: Option<String>,
    /// Раскрытые узлы в прямом порядке, включая прокрученные за край.
    pub expanded: Vec<String>,
    /// Смещение прокрутки в точках и его предел.
    pub offset: f64,
    pub max_offset: f64,
    /// Дерево — first responder окна.
    pub keyboard_focus: bool,
    /// Созданные виды строк и ячеек за жизнь дерева.
    pub row_views_created: u64,
    pub cell_views_created: u64,
    /// Видимые строки по порядку.
    pub visible: Vec<TreeRowView>,
}

/// Видимая строка дерева.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeRowView {
    pub index: u64,
    pub id: String,
    pub text: String,
    /// AX-уровень: у корня 0.
    pub level: u64,
    pub expandable: bool,
    pub expanded: bool,
}

/// Состояние `ChromeBar`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChromeView {
    /// Имя активного воркспейса из зеркала; `None` — его нет.
    pub workspace: Option<String>,
    /// Текст кнопки воркспейса.
    pub workspace_text: String,
    pub appearance_choice: AppearanceChoice,
    pub workspace_menu: bool,
    pub zones: bool,
}

/// Состояние `TextField`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextFieldView {
    /// Значение, которое показывает поле, включая ввод пользователя.
    pub value: String,
    pub placeholder: String,
    pub editable: bool,
    /// Идёт ввод: у поля есть field editor.
    pub editing: bool,
}

/// Состояние меню кнопки.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MenuView {
    /// Корневое меню открыто: идёт отслеживание.
    pub open: bool,
    /// Подсвеченный пункт корневого меню.
    pub highlighted: Option<String>,
    pub items: Vec<MenuItemView>,
}

/// Пункт меню или разделитель.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MenuItemView {
    pub id: String,
    pub separator: bool,
    pub title: String,
    /// Сочетание, например `cmd+q`; `None` — без клавиши.
    pub key: Option<String>,
    pub enabled: bool,
    pub checked: bool,
    /// Пункты подменю; `None` — пункт без подменю.
    pub submenu: Option<Vec<MenuItemView>>,
}

/// Вид или слой материала.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialView {
    pub class: String,
    pub visible: bool,
    /// Внутри собственных видов окна, а не в системном контейнере заголовка.
    pub own: bool,
}

/// Счётчики и замеры: ответ `ui.stats`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatsView {
    pub pid: u32,
    pub generation: u64,
    /// Созданные и освобождённые экземпляры по классам `define_class!` и владельцам Rust.
    pub instances: Vec<InstanceView>,
    pub late_callbacks: Vec<LateCallbackView>,
    /// Монитор цикла главного потока; `None` — не поставлен.
    pub run_loop: Option<RunLoopView>,
    /// `launchDate` → первая отрисовка окна key с первым снимком зеркала; `None` — не наблюдалась.
    pub startup: Option<StartupView>,
    /// Журнал анимаций по поколениям.
    pub animations: Vec<AnimationRecord>,
    /// Созданные виды строк по спискам и деревьям.
    pub row_views: Vec<RowViewsView>,
    /// Слабые ссылки на виды прежних деревьев галереи.
    pub gallery_trees: Vec<GalleryTreeView>,
    /// Построенные тела плагинов по панелям с живым или припаркованным контейнером; без
    /// контейнеров ключа в кадре нет.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bodies_built: Vec<BodiesBuiltView>,
    /// Ряд задержек клавиш списка галереи; `None` — серии не было, и ключа в кадре нет.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_latency: Option<KeyLatencyView>,
    /// Ряды измерителя задержки палитры обычного окна (план `command-overlays`, «Бюджет
    /// палитры»); `None` — серия пуста, и ключа в кадре нет.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub palette_latency: Option<PaletteLatencyView>,
}

/// Серия измерителя задержки палитры с `ui.latency`: открытия и закрытия палитры с клавиши и
/// такты `CADisplayLink` окна владельца. Время — секунды с загрузки, часы `NSEvent.timestamp`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaletteLatencyView {
    /// Серия ведёт такты `CADisplayLink` (блок A протокола).
    pub ticking: bool,
    pub samples: Vec<PaletteSampleView>,
    pub ticks: Vec<f64>,
}

/// Что клавиша сделала с палитрой.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaletteChange {
    Open,
    Close,
}

/// Открытие или закрытие палитры одной клавишей до прохода отображения.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaletteSampleView {
    pub change: PaletteChange,
    /// `NSEvent.timestamp` клавиши.
    pub event: f64,
    /// Вход в отложенный обработчик решения клавиши.
    pub entry: f64,
    /// Новое состояние палитры применено к окнам: конец своего кода.
    pub applied: f64,
    /// Первый проход отображения цикла главного потока не раньше [`Self::applied`]; `None` — не
    /// наблюдался или вытеснен из журнала.
    pub drawn: Option<f64>,
    /// Поколение окна после применения.
    pub generation: u64,
    /// Окно палитры на экране после применения.
    pub palette_visible: bool,
    /// Окно палитры key после применения.
    pub palette_key: bool,
    /// Окно владельца key после применения.
    pub owner_key: bool,
    /// Строка годна для вердикта: отметки монотонны, проход наблюдался, состояние окон
    /// подтверждает изменение.
    pub valid: bool,
}

/// Ряд задержек клавиш (план P1.3, «Замеры»): нажатия серии и такты `CADisplayLink` окна. Время
/// — секунды с загрузки, часы одного процесса. Серия начинается, когда список получает фокус.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyLatencyView {
    pub component: String,
    pub presses: Vec<KeyPressView>,
    pub ticks: Vec<f64>,
}

/// Нажатие Up или Down серии.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyPressView {
    /// `up` или `down`.
    pub key: String,
    /// `NSEvent.timestamp`.
    pub event: f64,
    /// Последнее пробуждение цикла до входа в обработчик.
    pub wake: Option<f64>,
    /// Вход в `keyDown:` списка.
    pub entry: f64,
    /// Возврат обработчика `keyDown:`: намерение поставлено в очередь главного потока.
    pub handled: f64,
    /// Состояние с выбором нажатия применено к списку: конец своего кода.
    pub applied: Option<f64>,
    /// Строка, которую выбирает нажатие.
    pub target: String,
    /// Поколение, в котором выбор уже сдвинут этим нажатием.
    pub generation: Option<u64>,
    /// Первая отрисовка списка с поколением не меньше [`Self::generation`] и её поколение.
    pub drawn: Option<f64>,
    pub drawn_generation: Option<u64>,
}

/// Вид счётчика экземпляров.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceKind {
    /// Класс `define_class!`.
    Class,
    /// Владелец вида и его Rust-части.
    Owner,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceView {
    pub kind: InstanceKind,
    pub name: String,
    pub created: u64,
    pub released: u64,
    pub live: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LateCallbackView {
    pub site: String,
    pub count: u64,
}

/// Монитор цикла главного потока: занятость итераций, пробуждения, проходы отображения и
/// отрисовки компонентов.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunLoopView {
    pub iterations: u64,
    pub wakeups: u64,
    pub unpaired: u64,
    pub display_passes: u64,
    pub over_budget: u64,
    pub max_ms: f64,
    /// p95 занятости по ближайшему рангу за окно; `None` — итераций не было.
    pub p95_ms: Option<f64>,
    pub window: u64,
    /// Вызовы `drawRect:` и `updateLayer` компонентов.
    pub draws: u64,
}

/// Старт процесса в мс от `launchDate`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupView {
    /// Первая отрисовка окна с первым снимком зеркала.
    pub first_draw_ms: f64,
    /// Окно впервые стало key.
    pub key_ms: f64,
    /// Позднее из двух: окно key показывает первый снимок.
    pub key_draw_ms: f64,
}

/// Путь смены состояния компонента.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimationPath {
    Animated,
    Instant,
}

/// Запись журнала анимаций.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnimationRecord {
    pub generation: u64,
    pub component: String,
    pub path: AnimationPath,
    /// Анимации, добавленные к слоям компонента при фиксации.
    pub added: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowViewsView {
    pub component: String,
    pub created: u64,
}

/// Сколько тел плагина построено в контейнере панели за жизнь контейнера: перенос, `zoom`,
/// заглушка и парковка тело не пересоздают, возврат в `ready` строит новое.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BodiesBuiltView {
    pub panel: PanelId,
    pub built: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GalleryTreeView {
    /// Номер построения дерева галереи.
    pub tree: u64,
    /// Видов под слабыми ссылками.
    pub views: u64,
    /// Из них ещё живы.
    pub alive: u64,
    /// Живые по классам; пусто — живых нет.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alive_classes: Vec<ClassAliveView>,
}

/// Живые объекты одного класса среди слабых ссылок прежнего дерева.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassAliveView {
    pub class: String,
    pub alive: u64,
}

/// Отказ обработчика `ui`. Срок и неизвестный исход ставит сервер.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UiRefusal {
    /// UI нет: тестовый владелец или главный поток больше не выполняет работу.
    NoUi,
    /// Запрос `gallery.*` вне режима галереи.
    GalleryOnly,
    /// Запрос `ui.cell` или `ui.window.size` владельцу без явного `SHELLIO_STATE_DIR`.
    IsolatedProfileOnly,
}

const QUEUED: u8 = 0;
const STARTED: u8 = 1;
const CANCELLED: u8 = 2;

/// Отметка начала запроса на главном потоке, общая для сервера и исполнителя.
///
/// Исполнитель вызывает [`Self::begin`] перед работой и не выполняет её, если получил `false`.
/// Сервер по сроку вызывает [`Self::cancel`]: `true` — работа не начиналась и уже не начнётся,
/// `false` — начата. Переход из ожидания делается один раз, поэтому ответ сервера и решение
/// исполнителя не расходятся.
#[derive(Clone, Debug, Default)]
pub struct UiGate(Arc<AtomicU8>);

impl UiGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Начать работу. `false` — срок уже отменил её.
    pub fn begin(&self) -> bool {
        match self
            .0
            .compare_exchange(QUEUED, STARTED, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(state) => state == STARTED,
        }
    }

    /// Отменить работу по сроку. `true` — она не начиналась и не начнётся.
    pub fn cancel(&self) -> bool {
        match self
            .0
            .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(state) => state == CANCELLED,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn request_names_follow_adr() {
        let cases = [
            (UiRequest::Window, "ui.window", false),
            (UiRequest::Inspect, "ui.inspect", false),
            (UiRequest::Stats, "ui.stats", false),
            (UiRequest::Receipts { after: None }, "ui.receipts", false),
            (UiRequest::Receipts { after: Some(7) }, "ui.receipts", false),
            (
                UiRequest::Command(UiCommand::WindowClose),
                "window.close",
                true,
            ),
            (UiRequest::Command(UiCommand::AppQuit), "app.quit", true),
            (
                UiRequest::Command(UiCommand::AppearanceSet(AppearanceChoice::Dark)),
                "ui.appearance.set",
                true,
            ),
            (
                UiRequest::Command(UiCommand::AppearanceCycle),
                "ui.appearance.cycle",
                true,
            ),
            (
                UiRequest::Command(UiCommand::LayoutToggle(ToggleTarget::Side)),
                "layout.toggle.side",
                true,
            ),
            (
                UiRequest::Command(UiCommand::List(ListAction::PageDown)),
                "list.page_down",
                true,
            ),
            (
                UiRequest::Command(UiCommand::PanelFilter),
                "panel.filter",
                true,
            ),
            (UiRequest::Mode, "ui.mode", false),
            (
                UiRequest::Command(UiCommand::Mode(ModeAction::Toggle)),
                "mode.toggle",
                true,
            ),
            (UiRequest::History, "ui.history", false),
            (UiRequest::InputJournal, "gallery.input.journal", false),
            (UiRequest::Overlay, "ui.overlay", false),
            (
                UiRequest::Command(UiCommand::OverlayOpen(OverlayKind::Palette)),
                "palette.open",
                true,
            ),
            (
                UiRequest::PaletteOpen {
                    query: Some("тема".into()),
                    category: Some(PaletteCategory::Commands),
                },
                "palette.open",
                true,
            ),
            (
                UiRequest::PaletteOpen {
                    query: None,
                    category: Some(PaletteCategory::Panels),
                },
                "palette.open",
                true,
            ),
            (
                UiRequest::OverlayClose(OverlayKind::Palette),
                "palette.close",
                true,
            ),
            (
                UiRequest::Command(UiCommand::OverlayOpen(OverlayKind::Hints)),
                "hints.open",
                true,
            ),
            (
                UiRequest::OverlayClose(OverlayKind::Hints),
                "hints.close",
                true,
            ),
            (
                UiRequest::Command(UiCommand::OverlayOpen(OverlayKind::Cmdline)),
                "cmdline.open",
                true,
            ),
            (
                UiRequest::OverlayClose(OverlayKind::Cmdline),
                "cmdline.close",
                true,
            ),
            (
                UiRequest::CmdlineRun {
                    text: "theme dark".into(),
                },
                "cmdline.run",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::InputReset),
                "gallery.input.reset",
                true,
            ),
            (
                UiRequest::Command(UiCommand::History(HistoryStep::Back)),
                "history.back",
                true,
            ),
            (
                UiRequest::Filter {
                    text: "вид".into()
                },
                "panel.filter",
                true,
            ),
            (
                UiRequest::Filter {
                    text: String::new(),
                },
                "panel.filter",
                true,
            ),
            (
                UiRequest::Cell {
                    contrast: Some(CellContrast::Increased),
                    motion: None,
                },
                "ui.cell",
                true,
            ),
            (
                UiRequest::WindowSize {
                    width: 1600,
                    height: 720,
                },
                "ui.window.size",
                true,
            ),
            (UiRequest::Latency { ticks: true }, "ui.latency", true),
            (UiRequest::Latency { ticks: false }, "ui.latency", true),
            (
                UiRequest::Gallery(GalleryCommand::Cell {
                    appearance: Some(Lightness::Light),
                    contrast: None,
                }),
                "gallery.cell",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::Motion(Motion::Reduced)),
                "gallery.motion",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::Focus(GalleryFocus::Field)),
                "gallery.focus",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::MenuOpen),
                "gallery.menu.open",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::Rebuild),
                "gallery.rebuild",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::WindowRecreate),
                "gallery.window.recreate",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::Place { screen: 1 }),
                "gallery.place",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::Reset),
                "gallery.reset",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::TabsSelect {
                    strip: GalleryStrip::Half,
                    tab: 2,
                }),
                "gallery.tabs.select",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::ListReplace { seed: 3 }),
                "gallery.list.replace",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::ListScroll { row: 9_999 }),
                "gallery.list.scroll",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::ListTicks { enabled: true }),
                "gallery.list.ticks",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::TreeMutate {
                    op: TreeMutation::CollapseParentOfSelected,
                }),
                "gallery.tree.mutate",
                true,
            ),
            (
                UiRequest::Gallery(GalleryCommand::Calibration),
                "gallery.calibration",
                true,
            ),
        ];
        // Номер варианта без `_`: новый вариант не соберётся, пока его нет здесь, а проверка ниже
        // требует его строки в таблице.
        let variant = |command: GalleryCommand| match command {
            GalleryCommand::Cell { .. } => 0,
            GalleryCommand::Motion(_) => 1,
            GalleryCommand::Focus(_) => 2,
            GalleryCommand::MenuOpen => 3,
            GalleryCommand::Rebuild => 4,
            GalleryCommand::WindowRecreate => 5,
            GalleryCommand::Place { .. } => 6,
            GalleryCommand::Reset => 7,
            GalleryCommand::TabsSelect { .. } => 8,
            GalleryCommand::ListReplace { .. } => 9,
            GalleryCommand::ListScroll { .. } => 10,
            GalleryCommand::TreeMutate { .. } => 11,
            GalleryCommand::ListTicks { .. } => 12,
            GalleryCommand::Calibration => 13,
            GalleryCommand::InputReset => 14,
        };
        let covered: std::collections::BTreeSet<usize> = cases
            .iter()
            .filter_map(|(request, _, _)| match request {
                UiRequest::Gallery(command) => Some(variant(*command)),
                _ => None,
            })
            .collect();
        assert_eq!(covered, (0..15).collect());
        for (request, name, mutation) in cases {
            assert_eq!(request.name(), name);
            assert_eq!(request.is_mutation(), mutation, "{name}");
            let json = serde_json::to_value(request.clone()).unwrap();
            let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
            assert_eq!(keys, [name]);
            assert_eq!(serde_json::from_value::<UiRequest>(json).unwrap(), request);
        }
    }

    /// Один кадр — одно значение: без `text` это действие «открыть строку фильтра», с `text`,
    /// даже пустым, — текст фильтра.
    #[test]
    fn panel_filter_text_separates_opening_from_setting() {
        let open: UiRequest = serde_json::from_value(json!({"panel.filter": {}})).unwrap();
        assert_eq!(open, UiRequest::Command(UiCommand::PanelFilter));
        assert_eq!(
            serde_json::to_value(open).unwrap(),
            json!({"panel.filter": {}})
        );
        for text in ["", "вид"] {
            let frame = json!({"panel.filter": {"text": text}});
            let set: UiRequest = serde_json::from_value(frame.clone()).unwrap();
            assert_eq!(set, UiRequest::Filter { text: text.into() });
            assert_eq!(serde_json::to_value(set).unwrap(), frame);
        }
    }

    #[test]
    fn request_body_is_strict() {
        for bad in [
            json!({"ui.window": {"extra": 1}}),
            json!({"ui.window": null}),
            json!({"ui.unknown": {}}),
            json!({"ui.window": {}, "ui.stats": {}}),
            json!({"ui.appearance.set": {}}),
            json!({"ui.appearance.set": {"appearance": "Light"}}),
            json!("ui.window"),
            json!({"ui.receipts": {"after": -1}}),
            json!({"ui.receipts": {"extra": 1}}),
            json!({"ui.receipts": null}),
            json!({"ui.receipts": {"after": "x"}}),
            json!({"panel.filter": null}),
            json!({"panel.filter": {"text": 1}}),
            json!({"panel.filter": {"text": "вид", "panel": 1}}),
            json!({"ui.cell": {"contrast": "high"}}),
            json!({"ui.cell": {"motion": "reduce"}}),
            json!({"ui.cell": {"appearance": "dark"}}),
            json!({"ui.cell": null}),
            json!({"ui.window.size": {}}),
            json!({"ui.window.size": {"width": 1600}}),
            json!({"ui.window.size": {"width": -1, "height": 720}}),
            json!({"ui.window.size": {"width": 1600.5, "height": 720}}),
            json!({"ui.window.size": {"width": 1600, "height": 720, "screen": 0}}),
            json!({"ui.latency": {}}),
            json!({"ui.latency": {"ticks": "on"}}),
            json!({"ui.latency": {"ticks": true, "extra": 1}}),
            json!({"gallery.cell": {"appearance": "sepia"}}),
            json!({"gallery.cell": {"extra": 1}}),
            json!({"gallery.motion": {}}),
            json!({"gallery.focus": {"component": "grid"}}),
            json!({"gallery.list.replace": {}}),
            json!({"gallery.list.replace": {"seed": -1}}),
            json!({"gallery.list.scroll": {"row": "end"}}),
            json!({"gallery.list.ticks": {}}),
            json!({"gallery.list.ticks": {"enabled": "on"}}),
            json!({"gallery.list.ticks": {"enabled": true, "extra": 1}}),
            json!({"gallery.tree.mutate": {}}),
            json!({"gallery.tree.mutate": {"op": "shuffle"}}),
            json!({"gallery.tree.mutate": {"op": "insert", "at": 0}}),
            json!({"gallery.place": {"screen": -1}}),
            json!({"gallery.place": {}}),
            json!({"gallery.reset": {"seed": 1}}),
            json!({"gallery.calibration": {"page": 1}}),
            json!({"gallery.calibration": null}),
            json!({"gallery.tabs.select": {"strip": "top", "tab": 0}}),
            json!({"gallery.tabs.select": {"strip": "side", "tab": -1}}),
            json!({"gallery.tabs.select": {"strip": "side"}}),
            json!({"gallery.tabs.select": {"tab": 0}}),
            json!({"gallery.tabs.select": {"strip": "side", "tab": 0, "extra": 1}}),
        ] {
            assert!(
                serde_json::from_value::<UiRequest>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn cell_axes_are_optional_and_omitted_when_absent() {
        let both = UiRequest::Gallery(GalleryCommand::Cell {
            appearance: Some(Lightness::Dark),
            contrast: Some(Contrast::Increased),
        });
        assert_eq!(
            serde_json::to_value(both).unwrap(),
            json!({"gallery.cell": {"appearance": "dark", "contrast": "increased"}})
        );
        let contrast_only: UiRequest =
            serde_json::from_value(json!({"gallery.cell": {"contrast": "normal"}})).unwrap();
        assert_eq!(
            contrast_only,
            UiRequest::Gallery(GalleryCommand::Cell {
                appearance: None,
                contrast: Some(Contrast::Normal),
            })
        );
        assert_eq!(
            serde_json::to_value(contrast_only).unwrap(),
            json!({"gallery.cell": {"contrast": "normal"}})
        );
        let neither: UiRequest = serde_json::from_value(json!({"gallery.cell": {}})).unwrap();
        assert_eq!(
            neither,
            UiRequest::Gallery(GalleryCommand::Cell {
                appearance: None,
                contrast: None,
            })
        );
    }

    #[test]
    fn ui_cell_axes_are_tristate_and_omitted_when_absent() {
        let both = UiRequest::Cell {
            contrast: Some(CellContrast::Increased),
            motion: Some(CellMotion::Reduced),
        };
        assert_eq!(
            serde_json::to_value(&both).unwrap(),
            json!({"ui.cell": {"contrast": "increased", "motion": "reduced"}})
        );
        let back: UiRequest =
            serde_json::from_value(json!({"ui.cell": {"contrast": "system"}})).unwrap();
        assert_eq!(
            back,
            UiRequest::Cell {
                contrast: Some(CellContrast::System),
                motion: None,
            }
        );
        assert_eq!(
            serde_json::to_value(back).unwrap(),
            json!({"ui.cell": {"contrast": "system"}})
        );
        assert_eq!(CellContrast::System.value(), None);
        assert_eq!(CellContrast::Normal.value(), Some(Contrast::Normal));
        assert_eq!(CellContrast::Increased.value(), Some(Contrast::Increased));
        assert_eq!(CellMotion::System.value(), None);
        assert_eq!(CellMotion::Normal.value(), Some(Motion::Normal));
        assert_eq!(CellMotion::Reduced.value(), Some(Motion::Reduced));
        let reply = UiReply::Cell(CellView {
            pid: 1,
            lightness: Axis {
                value: Lightness::Dark,
                source: AxisSource::UiAppearance,
            },
            contrast: Axis {
                value: Contrast::Increased,
                source: AxisSource::Override,
            },
            motion: Axis {
                value: Motion::Normal,
                source: AxisSource::System,
            },
            generation: 2,
        });
        assert!(both.answered_by(&reply));
        assert!(!UiRequest::Window.answered_by(&reply));
    }

    #[test]
    fn style_omits_absent_properties() {
        assert_eq!(
            serde_json::to_value(StyleView::default()).unwrap(),
            json!({})
        );
        let style = StyleView {
            fill: Some(Rgba {
                r: 30,
                g: 29,
                b: 27,
                a: 1.0,
            }),
            line_sides: vec![LineSide::Bottom],
            ..StyleView::default()
        };
        let json = serde_json::to_value(&style).unwrap();
        assert_eq!(
            json,
            json!({"fill": {"r": 30, "g": 29, "b": 27, "a": 1.0}, "line_sides": ["bottom"]})
        );
        assert_eq!(serde_json::from_value::<StyleView>(json).unwrap(), style);
        assert!(serde_json::from_value::<StyleView>(json!({"colour": null})).is_err());
    }

    #[test]
    fn mode_requests_and_reply_have_stable_frames() {
        for action in ModeAction::ALL {
            let frame = json!({ UiCommand::Mode(action).name(): {} });
            let request: UiRequest = serde_json::from_value(frame.clone()).unwrap();
            assert_eq!(request, UiRequest::Command(UiCommand::Mode(action)));
            assert_eq!(serde_json::to_value(&request).unwrap(), frame);
        }
        let read: UiRequest = serde_json::from_value(json!({"ui.mode": {}})).unwrap();
        assert_eq!(read, UiRequest::Mode);
        assert_eq!(serde_json::to_value(&read).unwrap(), json!({"ui.mode": {}}));
        assert!(serde_json::from_value::<UiRequest>(json!({"ui.mode": {"panel": 1}})).is_err());

        let idle = ModeView {
            pid: 7,
            panel: None,
            mode: PanelMode::Navigation,
            receiver: None,
            exit: ExitChordState::Idle,
            exit_receiver: None,
        };
        let json = serde_json::to_value(UiReply::Mode(idle.clone())).unwrap();
        assert_eq!(
            json,
            json!({"mode": {
                "pid": 7, "panel": null, "mode": "navigation", "receiver": null, "exit": "idle"
            }})
        );
        assert_eq!(
            serde_json::from_value::<UiReply>(json).unwrap(),
            UiReply::Mode(idle.clone())
        );
        let id = "00000000-0000-0000-0000-000000000003";
        let panel: PanelId = serde_json::from_value(json!(id)).unwrap();
        let armed = ModeView {
            panel: Some(panel),
            receiver: Some(ModeReceiver::Panel(panel)),
            exit: ExitChordState::Armed,
            exit_receiver: Some(ModeReceiver::Panel(panel)),
            ..idle
        };
        let json = serde_json::to_value(&armed).unwrap();
        assert_eq!(
            json,
            json!({
                "pid": 7, "panel": id, "mode": "navigation", "receiver": {"panel": id},
                "exit": "armed", "exit_receiver": {"panel": id}
            })
        );
        assert_eq!(serde_json::from_value::<ModeView>(json).unwrap(), armed);
        assert!(UiRequest::Mode.answered_by(&UiReply::Mode(armed.clone())));
        assert!(!UiRequest::Window.answered_by(&UiReply::Mode(armed)));
    }

    #[test]
    fn input_journal_request_and_reply_have_stable_frames() {
        let read: UiRequest = serde_json::from_value(json!({"gallery.input.journal": {}})).unwrap();
        assert_eq!(read, UiRequest::InputJournal);
        assert_eq!(
            serde_json::to_value(&read).unwrap(),
            json!({"gallery.input.journal": {}})
        );
        let reset: UiRequest = serde_json::from_value(json!({"gallery.input.reset": {}})).unwrap();
        assert_eq!(reset, UiRequest::Gallery(GalleryCommand::InputReset));
        assert_eq!(
            serde_json::to_value(&reset).unwrap(),
            json!({"gallery.input.reset": {}})
        );
        assert!(
            serde_json::from_value::<UiRequest>(json!({"gallery.input.journal": {"after": 1}}))
                .is_err()
        );

        let view = InputJournalView {
            pid: 7,
            receiver: ModeReceiver::GalleryInput(3),
            mode: PanelMode::Input,
            events: vec![InputEventView {
                kind: InputEventKind::Down,
                key_code: 42,
                cmd: false,
                ctrl: true,
                alt: false,
                shift: false,
                repeat: false,
            }],
        };
        let json = serde_json::to_value(UiReply::InputJournal(view.clone())).unwrap();
        assert_eq!(
            json,
            json!({"input_journal": {
                "pid": 7, "receiver": {"gallery_input": 3}, "mode": "input",
                "events": [{
                    "kind": "down", "key_code": 42, "cmd": false, "ctrl": true, "alt": false,
                    "shift": false, "repeat": false
                }]
            }})
        );
        assert_eq!(
            serde_json::from_value::<UiReply>(json).unwrap(),
            UiReply::InputJournal(view.clone())
        );
        assert!(UiRequest::InputJournal.answered_by(&UiReply::InputJournal(view.clone())));
        assert!(!UiRequest::History.answered_by(&UiReply::InputJournal(view)));
    }

    #[test]
    fn history_requests_and_reply_have_stable_frames() {
        for step in HistoryStep::ALL {
            let frame = json!({ UiCommand::History(step).name(): {} });
            let request: UiRequest = serde_json::from_value(frame.clone()).unwrap();
            assert_eq!(request, UiRequest::Command(UiCommand::History(step)));
            assert_eq!(serde_json::to_value(&request).unwrap(), frame);
        }
        let read: UiRequest = serde_json::from_value(json!({"ui.history": {}})).unwrap();
        assert_eq!(read, UiRequest::History);
        assert_eq!(
            serde_json::to_value(&read).unwrap(),
            json!({"ui.history": {}})
        );
        assert!(serde_json::from_value::<UiRequest>(json!({"ui.history": {"step": 1}})).is_err());

        let empty = HistoryView {
            pid: 7,
            workspace: None,
            points: Vec::new(),
            cursor: None,
        };
        let json = serde_json::to_value(UiReply::History(empty.clone())).unwrap();
        assert_eq!(
            json,
            json!({"history": {"pid": 7, "workspace": null, "points": [], "cursor": null}})
        );
        assert_eq!(
            serde_json::from_value::<UiReply>(json).unwrap(),
            UiReply::History(empty.clone())
        );
        let workspace = "00000000-0000-0000-0000-000000000001";
        let workbench = "00000000-0000-0000-0000-000000000002";
        let panel = "00000000-0000-0000-0000-000000000003";
        let one = HistoryView {
            workspace: Some(serde_json::from_value(json!(workspace)).unwrap()),
            points: vec![HistoryPoint {
                workbench: serde_json::from_value(json!(workbench)).unwrap(),
                panel: serde_json::from_value(json!(panel)).unwrap(),
                mode: PanelMode::Input,
            }],
            cursor: Some(0),
            ..empty
        };
        let json = serde_json::to_value(&one).unwrap();
        assert_eq!(
            json,
            json!({
                "pid": 7, "workspace": workspace, "cursor": 0,
                "points": [{"workbench": workbench, "panel": panel, "mode": "input"}]
            })
        );
        assert_eq!(serde_json::from_value::<HistoryView>(json).unwrap(), one);
        assert!(UiRequest::History.answered_by(&UiReply::History(one.clone())));
        assert!(!UiRequest::Mode.answered_by(&UiReply::History(one)));
    }

    #[test]
    fn receipts_cursor_is_optional_and_omitted_when_absent() {
        let all: UiRequest = serde_json::from_value(json!({"ui.receipts": {}})).unwrap();
        assert_eq!(all, UiRequest::Receipts { after: None });
        assert_eq!(
            serde_json::to_value(all).unwrap(),
            json!({"ui.receipts": {}})
        );
        let later = UiRequest::Receipts { after: Some(3) };
        assert_eq!(
            serde_json::to_value(&later).unwrap(),
            json!({"ui.receipts": {"after": 3}})
        );
        assert_eq!(
            serde_json::from_value::<UiRequest>(json!({"ui.receipts": {"after": 0}})).unwrap(),
            UiRequest::Receipts { after: Some(0) }
        );
        assert!(later.answered_by(&UiReply::Receipts(ReceiptsView::default())));
        assert!(!UiRequest::Stats.answered_by(&UiReply::Receipts(ReceiptsView::default())));
    }

    #[test]
    fn gallery_focus_names_are_stable() {
        for focus in GalleryFocus::ALL {
            assert_eq!(
                serde_json::to_value(focus).unwrap(),
                Value::from(focus.as_str())
            );
        }
    }

    #[test]
    fn tree_mutations_have_stable_names() {
        for op in TreeMutation::ALL {
            assert_eq!(serde_json::to_value(op).unwrap(), Value::from(op.as_str()));
            let request = UiRequest::Gallery(GalleryCommand::TreeMutate { op });
            assert_eq!(
                serde_json::to_value(request).unwrap(),
                json!({"gallery.tree.mutate": {"op": op.as_str()}})
            );
        }
    }

    #[test]
    fn axis_sources_have_stable_names() {
        for (source, name) in [
            (AxisSource::System, "system"),
            (AxisSource::UiAppearance, "ui.appearance"),
            (AxisSource::Gallery, "gallery"),
            (AxisSource::Override, "override"),
        ] {
            assert_eq!(serde_json::to_value(source).unwrap(), Value::from(name));
        }
    }

    #[test]
    fn gate_decides_once_between_start_and_cancel() {
        let gate = UiGate::new();
        assert!(gate.begin());
        assert!(gate.begin(), "повторное начало той же работы");
        assert!(!gate.cancel(), "начатую работу срок не отменяет");

        let gate = UiGate::new();
        assert!(gate.cancel());
        assert!(gate.cancel());
        assert!(!gate.begin(), "отменённая работа не начинается");
    }

    #[test]
    fn overlay_requests_and_replies_keep_their_wire_shape() {
        // `palette.open` без аргументов — действие keymap, с ними — запрос палитры.
        assert_eq!(
            serde_json::from_value::<UiRequest>(json!({"palette.open": {}})).unwrap(),
            UiRequest::Command(UiCommand::OverlayOpen(OverlayKind::Palette))
        );
        assert_eq!(
            serde_json::from_value::<UiRequest>(
                json!({"palette.open": {"query": "тема", "category": "panel"}})
            )
            .unwrap(),
            UiRequest::PaletteOpen {
                query: Some("тема".into()),
                category: Some(PaletteCategory::Panels),
            }
        );
        for bad in [
            json!({"palette.open": {"category": "files"}}),
            json!({"hints.open": {"x": 1}}),
            json!({"cmdline.run": {}}),
            json!({"palette.close": {"query": ""}}),
        ] {
            assert!(
                serde_json::from_value::<UiRequest>(bad.clone()).is_err(),
                "{bad}"
            );
        }
        for category in PaletteCategory::ALL {
            assert_eq!(PaletteCategory::parse(category.as_str()), Some(category));
        }

        let closed = OverlayView {
            pid: 7,
            kind: None,
            source: None,
            palette: None,
            hints: None,
            cmdline: None,
            which_key: None,
        };
        assert_eq!(
            serde_json::to_value(UiReply::Overlay(Box::new(closed.clone()))).unwrap(),
            json!({"overlay": {"pid": 7, "kind": null}})
        );
        let panel: PanelId =
            serde_json::from_value(json!("00000000-0000-0000-0000-000000000003")).unwrap();
        let open = OverlayView {
            kind: Some(OverlayViewKind::Palette),
            source: Some(OverlaySourceView {
                panel: Some(panel),
                receiver: Some(ModeReceiver::Panel(panel)),
                mode: PanelMode::Input,
                field: true,
            }),
            palette: Some(PaletteStateView {
                query: String::new(),
                category: PaletteCategory::All,
                groups: vec![PaletteGroupView {
                    title: "Команды".into(),
                    rows: vec![PaletteRowView {
                        id: "ui.appearance.dark".into(),
                        title: "Тёмная тема".into(),
                        detail: String::new(),
                        key: None,
                    }],
                }],
                selected: Some("ui.appearance.dark".into()),
                matched: 1,
            }),
            ..closed
        };
        let json = serde_json::to_value(UiReply::Overlay(Box::new(open.clone()))).unwrap();
        assert_eq!(json["overlay"]["kind"], "palette");
        assert_eq!(json["overlay"]["source"]["mode"], "input");
        assert_eq!(json["overlay"]["palette"]["category"], "all");
        assert_eq!(
            serde_json::from_value::<UiReply>(json).unwrap(),
            UiReply::Overlay(Box::new(open.clone()))
        );
        assert!(UiRequest::Overlay.answered_by(&UiReply::Overlay(Box::new(open.clone()))));
        assert!(!UiRequest::Mode.answered_by(&UiReply::Overlay(Box::new(open))));

        let run = CmdlineRunView {
            pid: 7,
            text: "нет такой".into(),
            outcome: CmdlineOutcome::Unknown,
            action: None,
            args: None,
            message: Some("Неизвестная команда: нет такой".into()),
        };
        let request = UiRequest::CmdlineRun {
            text: "нет такой".into(),
        };
        assert!(request.answered_by(&UiReply::Cmdline(Box::new(run.clone()))));
        assert!(!request.answered_by(&UiReply::Accepted(Accepted {
            command: "cmdline.run".into(),
            pid: 7,
            generation: 1,
        })));
        assert_eq!(
            serde_json::to_value(UiReply::Cmdline(Box::new(run))).unwrap()["cmdline"]["outcome"],
            "unknown"
        );
    }
}
