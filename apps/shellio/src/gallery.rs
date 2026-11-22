//! Галерея компонентов (`--gallery`, план P1.3): чистая модель без AppKit.
//!
//! Модель держит фикстуры компонентов, состояние галереи, применение служебных команд
//! `gallery.*`, журнал намерений и раскладку окна галереи. Вид на главном потоке
//! (`gallery_ui` бинаря `shellio`) переводит фикстуры в компоненты `shellio-appkit` и исполняет
//! эффекты команд.
//!
//! Поколение отрисовки [`Generation`] живёт вне состояния галереи: `gallery.reset`,
//! `gallery.rebuild` и `gallery.window.recreate` его не сбрасывают, поэтому оно монотонно в
//! пределах процесса, и маркер поколения в снимке однозначно называет состояние.

use std::collections::VecDeque;

use shellio_control::{
    GalleryCommand, GalleryFocus, GalleryStrip, IntentKind, IntentRecord, UiRefusal,
};
use shellio_theme::{Appearance, Contrast, Motion};

use crate::launch::WindowMode;

/// Поколение отрисовки окна владельца: растёт с каждой видимой сменой состояния окна и не
/// убывает до выхода процесса.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(u64);

impl Generation {
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Следующее поколение.
    pub fn bump(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// Допуск запроса `gallery.*`: только у окна в режиме галереи.
pub const fn admit(mode: WindowMode) -> Result<(), UiRefusal> {
    match mode {
        WindowMode::Gallery => Ok(()),
        WindowMode::Normal => Err(UiRefusal::GalleryOnly),
    }
}

/// Идентификаторы компонентов галереи в `ui.inspect` и в журнале намерений.
pub mod ids {
    pub const MARKER: &str = "gallery.marker";
    pub const SURFACE_FOCUS: &str = "gallery.surface.focus";
    pub const SURFACE_PLAIN: &str = "gallery.surface.plain";
    pub const FIELD: &str = "gallery.field";
    pub const MENU: &str = "gallery.menu";
    pub const TABS_SIDE: &str = "gallery.tabs.side";
    pub const TABS_CENTER: &str = "gallery.tabs.center";
    pub const TABS_HALF: &str = "gallery.tabs.half";
    pub const TABS_WIDE: &str = "gallery.tabs.wide";

    /// Идентификатор полосы вкладок.
    pub const fn strip(strip: shellio_control::GalleryStrip) -> &'static str {
        match strip {
            shellio_control::GalleryStrip::Side => TABS_SIDE,
            shellio_control::GalleryStrip::Center => TABS_CENTER,
            shellio_control::GalleryStrip::Half => TABS_HALF,
            shellio_control::GalleryStrip::Wide => TABS_WIDE,
        }
    }
}

/// Фикстура поля ввода.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldFixture {
    pub value: String,
    pub placeholder: String,
    pub editable: bool,
    /// Имя поля для доступности.
    pub label: &'static str,
}

/// Пункт фикстуры меню «…»: пункт с id или разделитель.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuFixtureItem {
    Item {
        id: &'static str,
        title: &'static str,
        /// Клавиша с `Cmd`.
        key: Option<&'static str>,
        enabled: bool,
        checked: bool,
    },
    Separator {
        id: &'static str,
    },
}

/// Фикстура кнопки «…» и её меню.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuFixture {
    /// Имя кнопки для доступности и подсказки.
    pub label: &'static str,
    /// Заголовок меню.
    pub title: &'static str,
    pub items: Vec<MenuFixtureItem>,
}

/// Поле ввода галереи: пустое редактируемое поле с плейсхолдером.
pub fn field_fixture() -> FieldFixture {
    FieldFixture {
        value: String::new(),
        placeholder: "Фильтр".to_owned(),
        editable: true,
        label: "Фильтр галереи",
    }
}

/// Меню «…» галереи: доступные пункты, отмеченный, разделитель и недоступный пункт. Первый
/// доступный пункт — `copy`: Down и Return в открытом меню выбирают его.
pub fn menu_fixture() -> MenuFixture {
    let item = |id, title, key, enabled, checked| MenuFixtureItem::Item {
        id,
        title,
        key,
        enabled,
        checked,
    };
    MenuFixture {
        label: "Ещё",
        title: "Действия",
        items: vec![
            item("copy", "Копировать путь", Some("c"), true, false),
            item("rename", "Переименовать", None, true, false),
            item("pin", "Закрепить", None, true, true),
            MenuFixtureItem::Separator { id: "sep" },
            item("close", "Закрыть", None, false, false),
        ],
    }
}

/// Сторона зоны полосы вкладок галереи.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripZone {
    Side,
    Center,
}

/// Статус вкладки фикстуры.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripStatus {
    None,
    Running,
    Waiting,
    Error,
    Done,
    Idle,
    Exited,
}

/// Вкладка фикстуры.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabFixture {
    pub id: &'static str,
    pub title: &'static str,
    pub kind: &'static str,
    /// Системный значок типа.
    pub icon: &'static str,
    pub status: StripStatus,
}

/// Где стоит полоса галереи.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripHost {
    /// В подложке с фокусом, под её слоем тона.
    FocusSurface,
    /// На уровне окна поверх подложек: 560 и 1100 точек не помещаются в подложку.
    Window,
}

/// Фикстура полосы вкладок. Режим по ширине (ADR-0032) задают ширина, вкладки и нагрузка:
/// `side` и `center` — прокрутка, `half` — нагрузка в меню «…», `wide` — всё видно.
#[derive(Debug, Clone, PartialEq)]
pub struct StripFixture {
    pub strip: GalleryStrip,
    pub width: f64,
    pub zone: StripZone,
    pub host: StripHost,
    /// Фокус у части: линия под активной вкладкой.
    pub focused: bool,
    /// Имя полосы для доступности.
    pub label: &'static str,
    pub tabs: Vec<TabFixture>,
    /// Номер выбранной вкладки в фикстуре.
    pub selected: usize,
    pub payload: &'static str,
}

/// Восемь вкладок с типами и статусами.
pub const TABS: [TabFixture; 8] = [
    tab("term", "zsh", "terminal", "terminal", StripStatus::Running),
    tab("build", "Сборка", "task", "hammer", StripStatus::Done),
    tab("agent", "Агент", "agent", "sparkles", StripStatus::Waiting),
    tab(
        "changes",
        "Изменения",
        "changes",
        "plusminus",
        StripStatus::None,
    ),
    tab("log", "Журнал", "log", "doc.plaintext", StripStatus::Error),
    tab("nvim", "nvim", "editor", "pencil", StripStatus::Idle),
    tab(
        "tests",
        "Тесты",
        "task",
        "checkmark.circle",
        StripStatus::Exited,
    ),
    tab(
        "review",
        "Ревью",
        "review",
        "text.bubble",
        StripStatus::None,
    ),
];

const fn tab(
    id: &'static str,
    title: &'static str,
    kind: &'static str,
    icon: &'static str,
    status: StripStatus,
) -> TabFixture {
    TabFixture {
        id,
        title,
        kind,
        icon,
        status,
    }
}

/// Ширины полос галереи (план P1.3).
pub const STRIP_NARROW: f64 = 372.0;
pub const STRIP_HALF: f64 = 560.0;
pub const STRIP_WIDE: f64 = 1100.0;

/// Фикстура полосы `strip`.
pub fn strip_fixture(strip: GalleryStrip) -> StripFixture {
    let short = "1,24 ГБ · 3,5 % CPU";
    match strip {
        GalleryStrip::Side => StripFixture {
            strip,
            width: STRIP_NARROW,
            zone: StripZone::Side,
            host: StripHost::FocusSurface,
            focused: true,
            label: "Вкладки боковой зоны",
            tabs: TABS.to_vec(),
            selected: 0,
            payload: short,
        },
        GalleryStrip::Center => StripFixture {
            strip,
            width: STRIP_NARROW,
            zone: StripZone::Center,
            host: StripHost::FocusSurface,
            focused: true,
            label: "Вкладки центра",
            tabs: TABS.to_vec(),
            selected: 0,
            payload: short,
        },
        GalleryStrip::Half => StripFixture {
            strip,
            width: STRIP_HALF,
            zone: StripZone::Center,
            host: StripHost::Window,
            focused: false,
            label: "Вкладки половины центра",
            tabs: TABS[..3].to_vec(),
            selected: 0,
            payload: "Память 1,24 ГБ · CPU 3,5 % · 12 процессов · 4 потока ввода",
        },
        GalleryStrip::Wide => StripFixture {
            strip,
            width: STRIP_WIDE,
            zone: StripZone::Center,
            host: StripHost::Window,
            focused: false,
            label: "Вкладки широкого центра",
            tabs: TABS[..4].to_vec(),
            selected: 1,
            payload: short,
        },
    }
}

/// Действие полосы вкладок галереи.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabAction {
    Select(String),
    Close(String),
    /// «Разделить»; значение — имя оси.
    Split(&'static str),
    Zoom,
    Restore,
    /// Пункт меню «…».
    Menu(String),
}

/// Сколько последних намерений держит журнал.
pub const INTENT_LOG: usize = 256;

/// Намерение компонента галереи.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    FieldChanged(String),
    FieldSubmitted(String),
    FieldCancelled,
    MenuChosen(String),
    Tab(GalleryStrip, TabAction),
}

impl Intent {
    fn record(&self, generation: u64) -> IntentRecord {
        let (component, intent, value) = match self {
            Intent::FieldChanged(value) => (ids::FIELD, IntentKind::Changed, Some(value.clone())),
            Intent::FieldSubmitted(value) => {
                (ids::FIELD, IntentKind::Submitted, Some(value.clone()))
            }
            Intent::FieldCancelled => (ids::FIELD, IntentKind::Cancelled, None),
            Intent::MenuChosen(id) => (ids::MENU, IntentKind::Chosen, Some(id.clone())),
            Intent::Tab(strip, action) => {
                let (intent, value) = match action {
                    TabAction::Select(id) => (IntentKind::Select, Some(id.clone())),
                    TabAction::Close(id) => (IntentKind::Close, Some(id.clone())),
                    TabAction::Split(axis) => (IntentKind::Split, Some((*axis).to_owned())),
                    TabAction::Zoom => (IntentKind::Zoom, None),
                    TabAction::Restore => (IntentKind::Restore, None),
                    TabAction::Menu(id) => (IntentKind::Chosen, Some(id.clone())),
                };
                (ids::strip(*strip), intent, value)
            }
        };
        IntentRecord {
            generation,
            component: component.to_owned(),
            intent,
            value,
        }
    }
}

/// Состояние галереи, которое `gallery.reset` возвращает к фикстуре.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GalleryState {
    /// Переопределения галереи по осям; `None` — ось следует выбору оформления или системе.
    pub appearance: Option<Appearance>,
    pub contrast: Option<Contrast>,
    pub motion: Option<Motion>,
    /// Последний запрос `gallery.focus`.
    pub focus: GalleryFocus,
    /// Последний запрос `gallery.place`.
    pub screen: Option<u32>,
    /// Значение поля, которое показывает вид: владелец следует намерениям `changed`.
    pub field_value: String,
    /// Выбранная вкладка каждой полосы в порядке [`GalleryStrip::ALL`].
    pub tabs: [usize; 4],
    pub intents: VecDeque<IntentRecord>,
}

impl GalleryState {
    /// Номер выбранной вкладки полосы `strip`.
    pub fn selected(&self, strip: GalleryStrip) -> usize {
        self.tabs[strip_index(strip)]
    }

    pub fn fixture() -> GalleryState {
        GalleryState {
            appearance: None,
            contrast: None,
            motion: None,
            focus: GalleryFocus::Window,
            screen: None,
            field_value: field_fixture().value,
            tabs: GalleryStrip::ALL.map(|strip| strip_fixture(strip).selected),
            intents: VecDeque::new(),
        }
    }
}

/// Что вид делает после команды.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Переопределения ячейки или движения изменились: окно получает их, компоненты — новую тему.
    Theme,
    /// Отдать first responder.
    Focus(GalleryFocus),
    /// Открыть меню кнопки «…».
    OpenMenu,
    /// Освободить дерево компонентов и построить его заново.
    Rebuild,
    /// Закрыть и отпустить окно, создать новое с новым деревом.
    RecreateWindow,
    /// Перенести окно на экран.
    Place(u32),
    /// Состояние вернулось к фикстуре: тема, поле, фокус, меню и полосы приводятся к нему.
    Reset,
    /// Выбор вкладки полосы изменился.
    Tabs(GalleryStrip),
}

const fn strip_index(strip: GalleryStrip) -> usize {
    match strip {
        GalleryStrip::Side => 0,
        GalleryStrip::Center => 1,
        GalleryStrip::Half => 2,
        GalleryStrip::Wide => 3,
    }
}

/// Модель галереи. Номера дерева и окна, как и поколение, не сбрасываются.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gallery {
    state: GalleryState,
    tree: u64,
    window: u64,
}

impl Default for Gallery {
    fn default() -> Self {
        Gallery::new()
    }
}

impl Gallery {
    /// Модель первого построения: дерево 1 в окне 1.
    pub fn new() -> Gallery {
        Gallery {
            state: GalleryState::fixture(),
            tree: 1,
            window: 1,
        }
    }

    pub fn state(&self) -> &GalleryState {
        &self.state
    }

    /// Номер построения дерева компонентов.
    pub const fn tree(&self) -> u64 {
        self.tree
    }

    /// Номер окна галереи.
    pub const fn window(&self) -> u64 {
        self.window
    }

    /// Применить команду: состояние меняется, поколение растёт, вид получает эффект.
    pub fn apply(&mut self, command: GalleryCommand, generation: &mut Generation) -> Effect {
        let effect = match command {
            GalleryCommand::Cell {
                appearance,
                contrast,
            } => {
                self.state.appearance = appearance.map(appearance_of);
                self.state.contrast = contrast.map(contrast_of);
                Effect::Theme
            }
            GalleryCommand::Motion(motion) => {
                self.state.motion = Some(motion_of(motion));
                Effect::Theme
            }
            GalleryCommand::Focus(focus) => {
                self.state.focus = focus;
                Effect::Focus(focus)
            }
            GalleryCommand::MenuOpen => Effect::OpenMenu,
            GalleryCommand::Rebuild => {
                self.tree += 1;
                Effect::Rebuild
            }
            GalleryCommand::WindowRecreate => {
                self.tree += 1;
                self.window += 1;
                Effect::RecreateWindow
            }
            GalleryCommand::Place { screen } => {
                self.state.screen = Some(screen);
                Effect::Place(screen)
            }
            GalleryCommand::Reset => {
                self.state = GalleryState::fixture();
                Effect::Reset
            }
            GalleryCommand::TabsSelect { strip, tab } => {
                // Номер за пределами фикстуры выбора не меняет: полоса остаётся прежней.
                let count = strip_fixture(strip).tabs.len();
                if let Ok(tab) = usize::try_from(tab)
                    && tab < count
                {
                    self.state.tabs[strip_index(strip)] = tab;
                }
                Effect::Tabs(strip)
            }
        };
        generation.bump();
        effect
    }

    /// Записать намерение компонента. Поле владелец не перерисовывает: вид уже показывает
    /// значение, а эхо отставшего значения стёрло бы ввод, пришедший после намерения. Выбор
    /// вкладки меняет состояние полосы: возвращается полоса, которую вид приводит к нему.
    pub fn intent(&mut self, intent: &Intent, generation: &mut Generation) -> Option<GalleryStrip> {
        let generation = generation.bump();
        let mut changed = None;
        match intent {
            Intent::FieldChanged(value) => value.clone_into(&mut self.state.field_value),
            Intent::Tab(strip, TabAction::Select(id)) => {
                let found = strip_fixture(*strip)
                    .tabs
                    .iter()
                    .position(|tab| tab.id == id);
                if let Some(index) = found {
                    self.state.tabs[strip_index(*strip)] = index;
                    changed = Some(*strip);
                }
            }
            _ => {}
        }
        if self.state.intents.len() == INTENT_LOG {
            self.state.intents.pop_front();
        }
        self.state.intents.push_back(intent.record(generation));
        changed
    }
}

fn appearance_of(lightness: shellio_control::Lightness) -> Appearance {
    match lightness {
        shellio_control::Lightness::Light => Appearance::Light,
        shellio_control::Lightness::Dark => Appearance::Dark,
    }
}

fn contrast_of(contrast: shellio_control::Contrast) -> Contrast {
    match contrast {
        shellio_control::Contrast::Normal => Contrast::Normal,
        shellio_control::Contrast::Increased => Contrast::Increased,
    }
}

fn motion_of(motion: shellio_control::Motion) -> Motion {
    match motion {
        shellio_control::Motion::Normal => Motion::Normal,
        shellio_control::Motion::Reduced => Motion::Reduced,
    }
}

/// Прямоугольник в точках родителя с началом сверху слева.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    /// `other` целиком внутри `self`.
    pub fn contains(&self, other: &Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

/// Отступ галереи от краёв содержимого и между частями.
pub const PAD: f64 = 12.0;
/// Маркер поколения: сетка 8×8 ячеек по 3 точки.
pub const MARKER_CELLS: u32 = 8;
pub const MARKER_CELL: f64 = 3.0;
pub const MARKER_SIZE: f64 = MARKER_CELL * MARKER_CELLS as f64;
/// Ширина поля ввода и сторона кнопки «…».
pub const FIELD_WIDTH: f64 = 280.0;
pub const MENU_BUTTON: f64 = 24.0;

/// Раскладка галереи в содержимом окна. Маркер, подложки и полосы `half` и `wide` — в
/// координатах содержимого, поле, кнопка «…» и полосы `side` и `center` — в координатах подложки
/// с фокусом.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub marker: Rect,
    pub focus: Rect,
    pub plain: Rect,
    pub field: Rect,
    pub menu: Rect,
    /// Полосы вкладок в порядке [`GalleryStrip::ALL`].
    pub strips: [Rect; 4],
}

impl Layout {
    pub const fn strip(&self, strip: GalleryStrip) -> Rect {
        self.strips[strip_index(strip)]
    }
}

/// Раскладка для содержимого `width`×`height` под заголовком высотой `titlebar`, поля высотой
/// `field_height` и полосы вкладок высотой `tabbar`. Маркер стоит в левом верхнем углу под
/// заголовком: вдали от скруглённых углов окна и от кнопок заголовка. Полосы `side` и `center`
/// лежат в подложке с фокусом под полем. `wide` и `half` шире подложки, поэтому лежат на уровне
/// окна под подложками, не накрывая их: участок внутри полосы поверх подложки оракул отверг бы
/// как участок у чужого узла. Края — на целых точках.
pub fn layout(width: f64, height: f64, titlebar: f64, field_height: f64, tabbar: f64) -> Layout {
    let top = titlebar + PAD;
    let marker = Rect::new(PAD, top, MARKER_SIZE, MARKER_SIZE);
    let left = marker.right() + PAD;
    let surface_width = ((width - left - 2.0 * PAD) / 2.0).floor().max(0.0);
    let band = 2.0 * (tabbar + PAD);
    let surface_height = (height - top - PAD - band).max(0.0);
    let focus = Rect::new(left, top, surface_width, surface_height);
    let plain = Rect::new(focus.right() + PAD, top, surface_width, surface_height);
    let field = Rect::new(PAD, PAD, FIELD_WIDTH, field_height);
    let menu = Rect::new(
        field.right() + PAD,
        PAD + ((field_height - MENU_BUTTON) / 2.0).round(),
        MENU_BUTTON,
        MENU_BUTTON,
    );
    let side = Rect::new(PAD, field.bottom() + PAD, STRIP_NARROW, tabbar);
    let center = Rect::new(PAD, side.bottom() + PAD, STRIP_NARROW, tabbar);
    let wide = Rect::new(focus.x, focus.bottom() + PAD, STRIP_WIDE, tabbar);
    let half = Rect::new(focus.x, wide.bottom() + PAD, STRIP_HALF, tabbar);
    Layout {
        marker,
        focus,
        plain,
        field,
        menu,
        strips: [side, center, half, wide],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_command() -> Vec<GalleryCommand> {
        use shellio_control::{Contrast as C, Lightness as L, Motion as M};
        vec![
            GalleryCommand::Cell {
                appearance: Some(L::Light),
                contrast: None,
            },
            GalleryCommand::Cell {
                appearance: None,
                contrast: Some(C::Increased),
            },
            GalleryCommand::Motion(M::Reduced),
            GalleryCommand::Focus(GalleryFocus::Field),
            GalleryCommand::MenuOpen,
            GalleryCommand::Rebuild,
            GalleryCommand::WindowRecreate,
            GalleryCommand::Place { screen: 1 },
            GalleryCommand::Reset,
            GalleryCommand::TabsSelect {
                strip: GalleryStrip::Side,
                tab: 3,
            },
        ]
    }

    #[test]
    fn generation_grows_through_reset_rebuild_and_recreate() {
        let mut gallery = Gallery::new();
        let mut generation = Generation::default();
        let mut seen = vec![generation.get()];
        // Каждая команда, в том числе сбрасывающие, и намерение между ними дают новое поколение.
        for round in 0..3 {
            for command in every_command() {
                gallery.apply(command, &mut generation);
                seen.push(generation.get());
                gallery.intent(&Intent::FieldChanged(format!("{round}")), &mut generation);
                seen.push(generation.get());
            }
        }
        assert!(
            seen.windows(2).all(|pair| pair[0] < pair[1]),
            "поколения: {seen:?}"
        );
        // Сброс возвращает состояние к фикстуре, но не номера дерева и окна.
        let before = (gallery.tree(), gallery.window());
        gallery.apply(GalleryCommand::Reset, &mut generation);
        assert_eq!(gallery.state(), &GalleryState::fixture());
        assert_eq!((gallery.tree(), gallery.window()), before);
        assert_eq!(gallery.tree(), 7, "три пересборки и три пересоздания окна");
        assert_eq!(gallery.window(), 4);
    }

    #[test]
    fn commands_change_state_and_name_their_effect() {
        let mut gallery = Gallery::new();
        let mut generation = Generation::default();
        let effects: Vec<Effect> = every_command()
            .into_iter()
            .map(|command| gallery.apply(command, &mut generation))
            .collect();
        assert_eq!(
            effects,
            [
                Effect::Theme,
                Effect::Theme,
                Effect::Theme,
                Effect::Focus(GalleryFocus::Field),
                Effect::OpenMenu,
                Effect::Rebuild,
                Effect::RecreateWindow,
                Effect::Place(1),
                Effect::Reset,
                Effect::Tabs(GalleryStrip::Side),
            ]
        );

        let mut gallery = Gallery::new();
        gallery.apply(
            GalleryCommand::Cell {
                appearance: Some(shellio_control::Lightness::Dark),
                contrast: Some(shellio_control::Contrast::Increased),
            },
            &mut generation,
        );
        assert_eq!(gallery.state().appearance, Some(Appearance::Dark));
        assert_eq!(gallery.state().contrast, Some(Contrast::Increased));
        // Пропущенная ось снимает переопределение: светлоту снова даёт выбор или система.
        gallery.apply(
            GalleryCommand::Cell {
                appearance: None,
                contrast: Some(shellio_control::Contrast::Normal),
            },
            &mut generation,
        );
        assert_eq!(gallery.state().appearance, None);
        assert_eq!(gallery.state().contrast, Some(Contrast::Normal));
        gallery.apply(
            GalleryCommand::Motion(shellio_control::Motion::Reduced),
            &mut generation,
        );
        assert_eq!(gallery.state().motion, Some(Motion::Reduced));
        gallery.apply(GalleryCommand::Place { screen: 2 }, &mut generation);
        assert_eq!(gallery.state().screen, Some(2));
        gallery.apply(GalleryCommand::Focus(GalleryFocus::Field), &mut generation);
        assert_eq!(gallery.state().focus, GalleryFocus::Field);
    }

    #[test]
    fn intents_are_logged_with_their_generation_and_bounded() {
        let mut gallery = Gallery::new();
        let mut generation = Generation::default();
        gallery.intent(&Intent::FieldChanged("s".into()), &mut generation);
        gallery.intent(&Intent::FieldSubmitted("s".into()), &mut generation);
        gallery.intent(&Intent::FieldCancelled, &mut generation);
        gallery.intent(&Intent::MenuChosen("copy".into()), &mut generation);
        let records: Vec<(u64, &str, IntentKind, Option<&str>)> = gallery
            .state()
            .intents
            .iter()
            .map(|record| {
                (
                    record.generation,
                    record.component.as_str(),
                    record.intent,
                    record.value.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            records,
            [
                (1, ids::FIELD, IntentKind::Changed, Some("s")),
                (2, ids::FIELD, IntentKind::Submitted, Some("s")),
                (3, ids::FIELD, IntentKind::Cancelled, None),
                (4, ids::MENU, IntentKind::Chosen, Some("copy")),
            ]
        );
        assert_eq!(gallery.state().field_value, "s");

        for index in 0..INTENT_LOG + 10 {
            gallery.intent(&Intent::MenuChosen(index.to_string()), &mut generation);
        }
        let intents = &gallery.state().intents;
        assert_eq!(intents.len(), INTENT_LOG);
        let last = (INTENT_LOG + 9).to_string();
        assert_eq!(
            intents.back().and_then(|r| r.value.as_deref()),
            Some(last.as_str())
        );

        gallery.apply(GalleryCommand::Reset, &mut generation);
        assert!(gallery.state().intents.is_empty());
        assert_eq!(gallery.state().field_value, field_fixture().value);
    }

    #[test]
    fn gallery_commands_only_in_gallery_mode() {
        assert_eq!(admit(WindowMode::Gallery), Ok(()));
        assert_eq!(admit(WindowMode::Normal), Err(UiRefusal::GalleryOnly));
    }

    #[test]
    fn fixtures_are_deterministic_and_have_unique_menu_ids() {
        assert_eq!(field_fixture(), field_fixture());
        assert_eq!(menu_fixture(), menu_fixture());
        let field = field_fixture();
        assert!(field.editable && field.value.is_empty() && !field.placeholder.is_empty());
        let menu = menu_fixture();
        let ids: Vec<&str> = menu
            .items
            .iter()
            .map(|item| match item {
                MenuFixtureItem::Item { id, .. } | MenuFixtureItem::Separator { id } => *id,
            })
            .collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        let first_enabled = menu.items.iter().find_map(|item| match item {
            MenuFixtureItem::Item {
                id, enabled: true, ..
            } => Some(*id),
            _ => None,
        });
        assert_eq!(first_enabled, Some("copy"));
        assert!(
            menu.items
                .iter()
                .any(|item| matches!(item, MenuFixtureItem::Item { checked: true, .. }))
        );
        assert!(
            menu.items
                .iter()
                .any(|item| matches!(item, MenuFixtureItem::Item { enabled: false, .. }))
        );
    }

    #[test]
    fn layout_keeps_parts_apart_inside_the_content() {
        let (width, height, titlebar, field) = (1180.0, 720.0, 40.0, 32.0);
        let layout = layout(width, height, titlebar, field, 34.0);
        let content = Rect::new(0.0, titlebar, width, height - titlebar);
        for part in [layout.marker, layout.focus, layout.plain] {
            assert!(content.contains(&part), "{part:?}");
            for edge in [part.x, part.y, part.width, part.height] {
                assert_eq!(edge, edge.round(), "{part:?} не на целых точках");
            }
        }
        assert_eq!(layout.marker.width, 24.0);
        assert!(!layout.marker.intersects(&layout.focus));
        assert!(!layout.focus.intersects(&layout.plain));
        assert_eq!(
            layout.focus.width, layout.plain.width,
            "подложки рядом и равны"
        );
        let inner = Rect::new(0.0, 0.0, layout.focus.width, layout.focus.height);
        assert!(inner.contains(&layout.field) && inner.contains(&layout.menu));
        assert!(!layout.field.intersects(&layout.menu));
        // Под полем остаётся пустая часть подложки для участка фона.
        assert!(layout.focus.height - layout.field.bottom() > 200.0);
    }

    /// Участки фона подложек и поля из ожиданий оракула (`scripts/e2e/appkit-expect`): (24, 460)
    /// 120×48 в подложке и (180, 8) 88×16 в поле. Полосы держатся от них дальше 4 точек, полосы
    /// уровня окна не накрывают подложек.
    #[test]
    fn strips_stay_clear_of_the_fixed_oracle_regions() {
        let layout = layout(1180.0, 720.0, 40.0, 32.0, 34.0);
        let inflate = |rect: Rect, by: f64| {
            Rect::new(
                rect.x - by,
                rect.y - by,
                rect.width + 2.0 * by,
                rect.height + 2.0 * by,
            )
        };
        let in_focus = |rect: Rect| {
            Rect::new(
                rect.x + layout.focus.x,
                rect.y + layout.focus.y,
                rect.width,
                rect.height,
            )
        };
        let regions = [
            in_focus(Rect::new(24.0, 460.0, 120.0, 48.0)),
            Rect::new(layout.plain.x + 24.0, layout.plain.y + 460.0, 120.0, 48.0),
            in_focus(Rect::new(
                layout.field.x + 180.0,
                layout.field.y + 8.0,
                88.0,
                16.0,
            )),
        ];
        assert!(
            layout.focus.height >= 512.0,
            "участки подложек целиком внутри"
        );
        let content = Rect::new(0.0, 40.0, 1180.0, 680.0);
        for strip in GalleryStrip::ALL {
            let fixture = strip_fixture(strip);
            let rect = match fixture.host {
                StripHost::FocusSurface => {
                    let inner = Rect::new(0.0, 0.0, layout.focus.width, layout.focus.height);
                    assert!(inner.contains(&layout.strip(strip)), "{strip:?} в подложке");
                    in_focus(layout.strip(strip))
                }
                StripHost::Window => {
                    let strip_rect = layout.strip(strip);
                    for surface in [layout.focus, layout.plain] {
                        assert!(
                            !inflate(strip_rect, 4.0).intersects(&surface),
                            "{strip:?} не накрывает подложку"
                        );
                    }
                    strip_rect
                }
            };
            assert_eq!(rect.width, fixture.width, "{strip:?}");
            assert!(content.contains(&rect), "{strip:?} в содержимом");
            assert!(!rect.intersects(&layout.marker), "{strip:?} и маркер");
            assert!(
                !inflate(rect, 4.0).intersects(&in_focus(layout.field)),
                "{strip:?} и поле"
            );
            for region in regions {
                assert!(
                    !inflate(rect, 4.0).intersects(&region),
                    "{strip:?} и {region:?}"
                );
            }
            for other in GalleryStrip::ALL
                .into_iter()
                .filter(|other| *other != strip)
            {
                let other_rect = match strip_fixture(other).host {
                    StripHost::FocusSurface => in_focus(layout.strip(other)),
                    StripHost::Window => layout.strip(other),
                };
                assert!(!rect.intersects(&other_rect), "{strip:?} и {other:?}");
            }
        }
    }

    #[test]
    fn strip_fixtures_name_their_regimes_and_select_existing_tabs() {
        for strip in GalleryStrip::ALL {
            let fixture = strip_fixture(strip);
            assert!(fixture.selected < fixture.tabs.len(), "{strip:?}");
            let mut ids: Vec<&str> = fixture.tabs.iter().map(|tab| tab.id).collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), fixture.tabs.len(), "{strip:?}: id уникальны");
        }
        let side = strip_fixture(GalleryStrip::Side);
        let center = strip_fixture(GalleryStrip::Center);
        assert_eq!(side.width, center.width, "пара одной ширины");
        assert_eq!(side.tabs, center.tabs);
        assert_ne!(side.zone, center.zone);
        assert_eq!(side.tabs.len(), 8);
        assert!(
            TABS.iter().any(|tab| tab.status == StripStatus::Running)
                && TABS.iter().any(|tab| tab.status == StripStatus::None),
            "статусы разные"
        );
    }

    #[test]
    fn tab_selection_follows_commands_and_select_intents() {
        let mut gallery = Gallery::new();
        let mut generation = Generation::default();
        let effect = gallery.apply(
            GalleryCommand::TabsSelect {
                strip: GalleryStrip::Side,
                tab: 7,
            },
            &mut generation,
        );
        assert_eq!(effect, Effect::Tabs(GalleryStrip::Side));
        assert_eq!(gallery.state().selected(GalleryStrip::Side), 7);
        gallery.apply(
            GalleryCommand::TabsSelect {
                strip: GalleryStrip::Side,
                tab: 8,
            },
            &mut generation,
        );
        assert_eq!(
            gallery.state().selected(GalleryStrip::Side),
            7,
            "номер за фикстурой выбора не меняет"
        );
        let changed = gallery.intent(
            &Intent::Tab(GalleryStrip::Center, TabAction::Select("agent".into())),
            &mut generation,
        );
        assert_eq!(changed, Some(GalleryStrip::Center));
        assert_eq!(gallery.state().selected(GalleryStrip::Center), 2);
        let closed = gallery.intent(
            &Intent::Tab(GalleryStrip::Center, TabAction::Close("agent".into())),
            &mut generation,
        );
        assert_eq!(closed, None, "закрытие только записывается");
        gallery.intent(
            &Intent::Tab(GalleryStrip::Wide, TabAction::Split("columns")),
            &mut generation,
        );
        let records: Vec<(&str, IntentKind, Option<&str>)> = gallery
            .state()
            .intents
            .iter()
            .map(|r| (r.component.as_str(), r.intent, r.value.as_deref()))
            .collect();
        assert_eq!(
            records,
            [
                (ids::TABS_CENTER, IntentKind::Select, Some("agent")),
                (ids::TABS_CENTER, IntentKind::Close, Some("agent")),
                (ids::TABS_WIDE, IntentKind::Split, Some("columns")),
            ]
        );
        gallery.apply(GalleryCommand::Reset, &mut generation);
        assert_eq!(
            gallery.state().selected(GalleryStrip::Side),
            strip_fixture(GalleryStrip::Side).selected
        );
    }
}
