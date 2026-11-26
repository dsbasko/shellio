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

use std::collections::{BTreeSet, VecDeque};

use shellio_control::{
    GalleryCommand, GalleryFocus, GalleryStrip, IntentKind, IntentRecord, TreeMutation, UiRefusal,
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
    pub const LIST: &str = "gallery.list";
    pub const TREE: &str = "gallery.tree";
    pub const CALIBRATION: &str = "gallery.calibration";
    /// Подложка приёмника ввода и сам приёмник.
    pub const INPUT_PLATE: &str = "gallery.input.plate";
    pub const INPUT: &str = "gallery.input";

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
    tab("term", "zsh", "terminal", "term", StripStatus::Running),
    tab("build", "Сборка", "task", "hammer", StripStatus::Done),
    tab("agent", "Агент", "agent", "agent", StripStatus::Waiting),
    tab(
        "changes",
        "Изменения",
        "changes",
        "changes",
        StripStatus::None,
    ),
    tab("log", "Журнал", "log", "doc.plaintext", StripStatus::Error),
    tab("nvim", "nvim", "editor", "file", StripStatus::Idle),
    tab(
        "tests",
        "Тесты",
        "task",
        "checkmark.circle",
        StripStatus::Exited,
    ),
    tab("review", "Ревью", "review", "review", StripStatus::None),
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

/// Число строк списка галереи.
pub const LIST_ROWS: usize = 10_000;
/// Зерно набора строк фикстуры.
pub const LIST_SEED: u64 = 0;
/// Выбранная строка фикстуры.
pub const LIST_SELECTED: usize = 0;
/// AX-имя списка.
pub const LIST_LABEL: &str = "Список галереи";

/// Id строки `index` набора `seed`. Набор другого зерна не делит с ним ни одного id.
pub fn list_row_id(seed: u64, index: usize) -> String {
    format!("s{seed}.r{index:05}")
}

/// Текст строки `index` набора `seed`: короткий, правая часть строки остаётся пустой для участка
/// фона в ожиданиях оракула.
pub fn list_row_text(seed: u64, index: usize) -> String {
    format!("Строка {index:05} · {seed}")
}

/// Номер строки по id набора `seed`; `None` — id не из этого набора.
pub fn list_row_index(seed: u64, id: &str) -> Option<usize> {
    let digits = id.strip_prefix(&format!("s{seed}.r"))?;
    let valid = digits.len() == 5 && digits.bytes().all(|byte| byte.is_ascii_digit());
    let index: usize = digits.parse().ok().filter(|_| valid)?;
    (index < LIST_ROWS).then_some(index)
}

/// Действие списка галереи.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListAction {
    Select(String),
    Activate(String),
}

/// Корней, веток у корня и листьев у ветки в дереве галереи: 100 × 9 × 10, глубина 3.
pub const TREE_ROOTS: usize = 100;
pub const TREE_BRANCHES: usize = 9;
pub const TREE_LEAVES: usize = 10;
/// AX-имя дерева.
pub const TREE_LABEL: &str = "Дерево галереи";
/// Раскрытые узлы фикстуры: первый корень и его первая ветка.
pub const TREE_EXPANDED: [&str; 2] = ["n000", "n000.0"];
/// Выбранный узел фикстуры: первый лист первой ветки, строка 2.
pub const TREE_SELECTED: &str = "n000.0.0";

/// Узел фикстуры дерева.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFixture {
    pub id: String,
    pub text: String,
    pub children: Vec<TreeFixture>,
}

/// Корни дерева галереи после `inserts` вставок и с обменом первых двух корней фикстуры при
/// `swapped`. Вставка `k` даёт корень `x<k>` сверху и ветку `n000.x<k>` первой в `n000`: новые
/// выше прежних. Id иерархичны: родитель узла — id до последней точки.
pub fn tree_roots(inserts: u32, swapped: bool) -> Vec<TreeFixture> {
    let leaf = |id: String, text: String| TreeFixture {
        id,
        text,
        children: Vec::new(),
    };
    let mut roots: Vec<TreeFixture> = (0..TREE_ROOTS)
        .map(|root| {
            let mut children: Vec<TreeFixture> = (0..TREE_BRANCHES)
                .map(|branch| TreeFixture {
                    id: format!("n{root:03}.{branch}"),
                    text: format!("Ветка {root:03}.{branch}"),
                    children: (0..TREE_LEAVES)
                        .map(|item| {
                            leaf(
                                format!("n{root:03}.{branch}.{item}"),
                                format!("Лист {root:03}.{branch}.{item}"),
                            )
                        })
                        .collect(),
                })
                .collect();
            if root == 0 {
                let inserted = (1..=inserts)
                    .rev()
                    .map(|k| leaf(format!("n000.x{k}"), format!("Новая ветка {k}")));
                children.splice(0..0, inserted);
            }
            TreeFixture {
                id: format!("n{root:03}"),
                text: format!("Корень {root:03}"),
                children,
            }
        })
        .collect();
    if swapped {
        roots.swap(0, 1);
    }
    let inserted = (1..=inserts)
        .rev()
        .map(|k| leaf(format!("x{k}"), format!("Новый корень {k}")));
    roots.splice(0..0, inserted);
    roots
}

/// Родитель узла по иерархичному id.
pub fn tree_parent(id: &str) -> Option<&str> {
    id.rsplit_once('.').map(|(parent, _)| parent)
}

/// Номер из десятичных цифр `digits` не больше `bound`, без знака и лишних символов.
fn number(digits: &str, width: Option<usize>, bound: usize) -> Option<usize> {
    let valid = !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && width.is_none_or(|width| digits.len() == width);
    digits.parse().ok().filter(|value| valid && *value < bound)
}

/// Есть ли узел `id` в дереве после `inserts` вставок, и есть ли у него дети.
pub fn tree_node(inserts: u32, id: &str) -> Option<bool> {
    let inserted = usize::try_from(inserts)
        .unwrap_or(usize::MAX)
        .saturating_add(1);
    let parts: Vec<&str> = id.split('.').collect();
    let root = |part: &str| {
        part.strip_prefix('n')
            .and_then(|d| number(d, Some(3), TREE_ROOTS))
    };
    let new = |part: &str| {
        part.strip_prefix('x')
            .filter(|d| !d.starts_with('0'))
            .and_then(|d| number(d, None, inserted))
            .filter(|k| *k >= 1)
    };
    match parts.as_slice() {
        [single] if new(single).is_some() => Some(false),
        [single] => root(single).map(|_| true),
        [first, second] if new(second).is_some() => (root(first) == Some(0)).then_some(false),
        [first, second] => root(first)
            .and(number(second, Some(1), TREE_BRANCHES))
            .map(|_| true),
        [first, second, third] => root(first)
            .and(number(second, Some(1), TREE_BRANCHES))
            .and(number(third, Some(1), TREE_LEAVES))
            .map(|_| false),
        _ => None,
    }
}

/// Действие дерева галереи.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeAction {
    Select(String),
    /// Раскрыть (`true`) или свернуть узел.
    Toggle(String, bool),
    Activate(String),
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
    List(ListAction),
    Tree(TreeAction),
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
            Intent::List(ListAction::Select(id)) => {
                (ids::LIST, IntentKind::Select, Some(id.clone()))
            }
            Intent::List(ListAction::Activate(id)) => {
                (ids::LIST, IntentKind::Activate, Some(id.clone()))
            }
            Intent::Tree(TreeAction::Select(id)) => {
                (ids::TREE, IntentKind::Select, Some(id.clone()))
            }
            Intent::Tree(TreeAction::Toggle(id, _)) => {
                (ids::TREE, IntentKind::Toggle, Some(id.clone()))
            }
            Intent::Tree(TreeAction::Activate(id)) => {
                (ids::TREE, IntentKind::Activate, Some(id.clone()))
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

/// Страница окна галереи.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// Сцена компонентов P1.3.
    Components,
    /// Материалы калибровки допусков сверки (`gallery.calibration`, ADR-0037).
    Calibration,
}

/// Состояние галереи, которое `gallery.reset` возвращает к фикстуре.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GalleryState {
    /// Страница окна: дерево видов строится по ней.
    pub page: Page,
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
    /// Зерно набора строк списка.
    pub list_seed: u64,
    /// Номер выбранной строки списка: замена набора того же размера оставляет его на месте.
    pub list_selected: Option<usize>,
    /// Такты `CADisplayLink` в серии задержки списка (`gallery.list.ticks`).
    pub list_ticks: bool,
    /// Вставок в дерево и обмен первых двух корней фикстуры ([`tree_roots`]).
    pub tree_inserts: u32,
    pub tree_swapped: bool,
    /// Раскрытые узлы дерева: предки каждого тоже раскрыты.
    pub tree_expanded: BTreeSet<String>,
    /// Выбранный узел дерева: все его предки раскрыты.
    pub tree_selected: Option<String>,
    pub intents: VecDeque<IntentRecord>,
}

impl GalleryState {
    /// Номер выбранной вкладки полосы `strip`.
    pub fn selected(&self, strip: GalleryStrip) -> usize {
        self.tabs[strip_index(strip)]
    }

    pub fn fixture() -> GalleryState {
        GalleryState {
            page: Page::Components,
            appearance: None,
            contrast: None,
            motion: None,
            focus: GalleryFocus::Window,
            screen: None,
            field_value: field_fixture().value,
            tabs: GalleryStrip::ALL.map(|strip| strip_fixture(strip).selected),
            list_seed: LIST_SEED,
            list_selected: Some(LIST_SELECTED),
            list_ticks: false,
            tree_inserts: 0,
            tree_swapped: false,
            tree_expanded: TREE_EXPANDED.iter().map(|id| (*id).to_owned()).collect(),
            tree_selected: Some(TREE_SELECTED.to_owned()),
            intents: VecDeque::new(),
        }
    }

    /// Все предки узла `id` раскрыты.
    fn tree_visible(&self, id: &str) -> bool {
        let mut cursor = tree_parent(id);
        while let Some(parent) = cursor {
            if !self.tree_expanded.contains(parent) {
                return false;
            }
            cursor = tree_parent(parent);
        }
        true
    }

    /// Раскрыть узел с детьми вместе с предками.
    fn tree_expand(&mut self, id: &str) {
        if tree_node(self.tree_inserts, id) != Some(true) {
            return;
        }
        let mut cursor = Some(id);
        while let Some(open) = cursor {
            self.tree_expanded.insert(open.to_owned());
            cursor = tree_parent(open);
        }
    }

    /// Свернуть узел: он и потомки выходят из раскрытых, выбор под ним переходит на него.
    fn tree_collapse(&mut self, id: &str) {
        if tree_node(self.tree_inserts, id).is_none() {
            return;
        }
        let below = format!("{id}.");
        self.tree_expanded
            .retain(|open| open != id && !open.starts_with(&below));
        if self
            .tree_selected
            .as_deref()
            .is_some_and(|selected| selected.starts_with(&below))
        {
            self.tree_selected = Some(id.to_owned());
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
    /// Дерево другой страницы вид перед этим заменяет деревом сцены компонентов.
    Reset,
    /// Выбор вкладки полосы изменился.
    Tabs(GalleryStrip),
    /// Набор строк или выбор списка изменился.
    List,
    /// Прокрутить список к строке.
    ListScroll(u32),
    /// Такты серии задержки списка включены или выключены.
    ListTicks,
    /// Узлы, раскрытие или выбор дерева изменились.
    Tree,
    /// Запрошенная страница уже показана: вид не меняется.
    Page,
    /// Очистить журнал приёмника ввода.
    InputReset,
}

/// Часть вида, которую намерение изменило.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Changed {
    Tabs(GalleryStrip),
    List,
    Tree,
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
                // Смена страницы строит новое дерево: его номер в `ui.stats` не повторяется.
                if self.state.page != Page::Components {
                    self.tree += 1;
                }
                self.state = GalleryState::fixture();
                Effect::Reset
            }
            GalleryCommand::Calibration => {
                if self.state.page == Page::Calibration {
                    Effect::Page
                } else {
                    self.state.page = Page::Calibration;
                    self.tree += 1;
                    Effect::Rebuild
                }
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
            GalleryCommand::ListReplace { seed } => {
                self.state.list_seed = seed;
                Effect::List
            }
            GalleryCommand::ListScroll { row } => Effect::ListScroll(row),
            GalleryCommand::InputReset => Effect::InputReset,
            GalleryCommand::ListTicks { enabled } => {
                self.state.list_ticks = enabled;
                Effect::ListTicks
            }
            GalleryCommand::TreeMutate { op } => {
                match op {
                    TreeMutation::Insert => {
                        self.state.tree_inserts = self.state.tree_inserts.saturating_add(1);
                    }
                    TreeMutation::Reorder => self.state.tree_swapped = !self.state.tree_swapped,
                    TreeMutation::CollapseParentOfSelected => {
                        let parent = self
                            .state
                            .tree_selected
                            .as_deref()
                            .and_then(tree_parent)
                            .map(str::to_owned);
                        if let Some(parent) = parent {
                            self.state.tree_collapse(&parent);
                        }
                    }
                }
                Effect::Tree
            }
        };
        generation.bump();
        effect
    }

    /// Записать намерение компонента. Поле владелец не перерисовывает: вид уже показывает
    /// значение, а эхо отставшего значения стёрло бы ввод, пришедший после намерения. Выбор
    /// вкладки или строки меняет состояние: возвращается часть, которую вид приводит к нему.
    pub fn intent(&mut self, intent: &Intent, generation: &mut Generation) -> Option<Changed> {
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
                    changed = Some(Changed::Tabs(*strip));
                }
            }
            Intent::List(ListAction::Select(id)) => {
                if let Some(index) = list_row_index(self.state.list_seed, id) {
                    self.state.list_selected = Some(index);
                    changed = Some(Changed::List);
                }
            }
            Intent::Tree(TreeAction::Select(id) | TreeAction::Toggle(id, _))
                if tree_node(self.state.tree_inserts, id).is_none()
                    || !self.state.tree_visible(id) => {}
            Intent::Tree(TreeAction::Select(id)) => {
                self.state.tree_selected = Some(id.clone());
                changed = Some(Changed::Tree);
            }
            Intent::Tree(TreeAction::Toggle(id, true)) => {
                self.state.tree_expand(id);
                changed = Some(Changed::Tree);
            }
            Intent::Tree(TreeAction::Toggle(id, false)) => {
                self.state.tree_collapse(id);
                changed = Some(Changed::Tree);
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
/// Высота приёмника ввода: его ширина — ширина поля.
pub const INPUT_HEIGHT: f64 = 48.0;
/// Ширина списка и число его видимых строк: список занимает левую часть подложки без фокуса над
/// её участком фона, правая часть остаётся дереву той же высоты.
pub const LIST_WIDTH: f64 = 260.0;
pub const LIST_VISIBLE: f64 = 16.0;

/// Раскладка галереи в содержимом окна. Маркер, подложки и полосы `half` и `wide` — в
/// координатах содержимого, поле, кнопка «…», полосы `side` и `center` и приёмник ввода — в
/// координатах подложки с фокусом.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub marker: Rect,
    pub focus: Rect,
    pub plain: Rect,
    pub field: Rect,
    pub menu: Rect,
    /// Приёмник ввода под полосой `center`.
    pub input: Rect,
    /// Полосы вкладок в порядке [`GalleryStrip::ALL`].
    pub strips: [Rect; 4],
    /// Список и дерево в координатах подложки без фокуса.
    pub list: Rect,
    pub tree: Rect,
}

impl Layout {
    pub const fn strip(&self, strip: GalleryStrip) -> Rect {
        self.strips[strip_index(strip)]
    }
}

/// Лист страницы «Калибровка» в содержимом `width`×`height` под заголовком высотой `titlebar`:
/// справа от маркера поколения до отступа у краёв. Края — на целых точках.
pub fn calibration_sheet(width: f64, height: f64, titlebar: f64) -> Rect {
    let top = titlebar + PAD;
    let left = PAD + MARKER_SIZE + PAD;
    Rect::new(
        left,
        top,
        (width - left - PAD).floor().max(0.0),
        (height - top - PAD).floor().max(0.0),
    )
}

/// Раскладка для содержимого `width`×`height` под заголовком высотой `titlebar`, поля высотой
/// `field_height`, полосы вкладок высотой `tabbar` и строки списка высотой `row`. Маркер стоит в левом верхнем углу под
/// заголовком: вдали от скруглённых углов окна и от кнопок заголовка. Полосы `side` и `center`
/// лежат в подложке с фокусом под полем. `wide` и `half` шире подложки, поэтому лежат на уровне
/// окна под подложками, не накрывая их: участок внутри полосы поверх подложки оракул отверг бы
/// как участок у чужого узла. Края — на целых точках.
pub fn layout(
    width: f64,
    height: f64,
    titlebar: f64,
    field_height: f64,
    tabbar: f64,
    row: f64,
) -> Layout {
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
    let input = Rect::new(PAD, center.bottom() + PAD, FIELD_WIDTH, INPUT_HEIGHT);
    let wide = Rect::new(focus.x, focus.bottom() + PAD, STRIP_WIDE, tabbar);
    let half = Rect::new(focus.x, wide.bottom() + PAD, STRIP_HALF, tabbar);
    let list = Rect::new(PAD, PAD, LIST_WIDTH, LIST_VISIBLE * row);
    let tree_x = list.right() + PAD;
    let tree = Rect::new(
        tree_x,
        PAD,
        (surface_width - tree_x - PAD).max(0.0),
        list.height,
    );
    Layout {
        marker,
        focus,
        plain,
        field,
        menu,
        input,
        strips: [side, center, half, wide],
        list,
        tree,
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
            GalleryCommand::Focus(GalleryFocus::List),
            GalleryCommand::ListReplace { seed: 4 },
            GalleryCommand::ListScroll { row: 9_999 },
            GalleryCommand::ListTicks { enabled: true },
            GalleryCommand::Focus(GalleryFocus::Tree),
            GalleryCommand::TreeMutate {
                op: TreeMutation::Insert,
            },
            GalleryCommand::InputReset,
            GalleryCommand::Calibration,
        ]
    }

    #[test]
    fn calibration_sheet_stays_inside_the_content_right_of_the_marker() {
        let sheet = calibration_sheet(1180.0, 720.0, 40.0);
        let marker = layout(1180.0, 720.0, 40.0, 28.0, 34.0, 24.0).marker;
        assert!(Rect::new(0.0, 40.0, 1180.0, 680.0).contains(&sheet));
        assert!(!sheet.intersects(&marker));
        assert_eq!(sheet.y, marker.y);
        assert_eq!(calibration_sheet(10.0, 10.0, 40.0).width, 0.0);
    }

    #[test]
    fn calibration_page_replaces_the_scene_until_reset() {
        let mut gallery = Gallery::new();
        let mut generation = Generation::default();
        assert_eq!(gallery.state().page, Page::Components);
        assert_eq!(
            gallery.apply(GalleryCommand::Calibration, &mut generation),
            Effect::Rebuild
        );
        assert_eq!(gallery.state().page, Page::Calibration);
        assert_eq!((gallery.tree(), generation.get()), (2, 1));
        // Повтор страницу не строит заново, но поколение растёт, как у любой команды.
        assert_eq!(
            gallery.apply(GalleryCommand::Calibration, &mut generation),
            Effect::Page
        );
        assert_eq!((gallery.tree(), generation.get()), (2, 2));
        // Пересборка и новое окно страницу не меняют.
        gallery.apply(GalleryCommand::Rebuild, &mut generation);
        gallery.apply(GalleryCommand::WindowRecreate, &mut generation);
        assert_eq!(gallery.state().page, Page::Calibration);
        assert_eq!(gallery.tree(), 4);
        assert_eq!(
            gallery.apply(GalleryCommand::Reset, &mut generation),
            Effect::Reset
        );
        assert_eq!(gallery.state().page, Page::Components);
        assert_eq!(gallery.tree(), 5);
        // Сброс сцены компонентов дерево не трогает.
        gallery.apply(GalleryCommand::Reset, &mut generation);
        assert_eq!(gallery.tree(), 5);
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
        // Сброс возвращает состояние к фикстуре, но не номера дерева и окна: со страницы
        // «Калибровка» он строит сцену компонентов новым деревом, со сцены дерево не трогает.
        assert_eq!(gallery.state().page, Page::Calibration);
        assert_eq!(
            gallery.tree(),
            12,
            "три пересборки, три пересоздания окна, три страницы «Калибровка» и два возврата"
        );
        gallery.apply(GalleryCommand::Reset, &mut generation);
        assert_eq!(gallery.state(), &GalleryState::fixture());
        assert_eq!((gallery.tree(), gallery.window()), (13, 4));
        gallery.apply(GalleryCommand::Reset, &mut generation);
        assert_eq!((gallery.tree(), gallery.window()), (13, 4));
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
                Effect::Focus(GalleryFocus::List),
                Effect::List,
                Effect::ListScroll(9_999),
                Effect::ListTicks,
                Effect::Focus(GalleryFocus::Tree),
                Effect::Tree,
                Effect::InputReset,
                Effect::Rebuild,
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
        let layout = layout(width, height, titlebar, field, 34.0, 26.0);
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
        let layout = layout(1180.0, 720.0, 40.0, 32.0, 34.0, 26.0);
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

    /// Приёмник ввода лежит в подложке с фокусом под полосой `center` и держится дальше 4 точек
    /// от поля, кнопки «…», полос подложки и участков оракула (`scripts/e2e/appkit-expect`).
    #[test]
    fn input_sink_stays_clear_of_its_neighbours_and_the_oracle_regions() {
        let layout = layout(1180.0, 720.0, 40.0, 32.0, 34.0, 26.0);
        let inner = Rect::new(0.0, 0.0, layout.focus.width, layout.focus.height);
        assert!(inner.contains(&layout.input), "приёмник в подложке");
        let around = Rect::new(
            layout.input.x - 4.0,
            layout.input.y - 4.0,
            layout.input.width + 8.0,
            layout.input.height + 8.0,
        );
        let neighbours = [
            ("поле", layout.field),
            ("кнопка «…»", layout.menu),
            ("side", layout.strip(GalleryStrip::Side)),
            ("center", layout.strip(GalleryStrip::Center)),
            ("фон подложки", Rect::new(24.0, 460.0, 120.0, 48.0)),
            (
                "фон поля",
                Rect::new(layout.field.x + 180.0, layout.field.y + 8.0, 88.0, 16.0),
            ),
        ];
        for (name, rect) in neighbours {
            assert!(!around.intersects(&rect), "приёмник и {name}");
        }
        assert!(layout.input.width > 0.0 && layout.input.height > 0.0);
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
        assert_eq!(changed, Some(Changed::Tabs(GalleryStrip::Center)));
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

    #[test]
    fn list_rows_are_named_by_seed_and_index() {
        assert_eq!(list_row_id(3, 42), "s3.r00042");
        assert_eq!(list_row_text(3, 42), "Строка 00042 · 3");
        assert_eq!(list_row_index(3, "s3.r00042"), Some(42));
        assert_eq!(list_row_index(3, "s3.r09999"), Some(9_999));
        for foreign in [
            "s4.r00042",
            "s3.r10000",
            "s3.r42",
            "s3.r0004x",
            "s3.r+0042",
            "",
        ] {
            assert_eq!(list_row_index(3, foreign), None, "{foreign}");
        }
        let ids: std::collections::HashSet<String> =
            (0..LIST_ROWS).map(|index| list_row_id(0, index)).collect();
        assert_eq!(ids.len(), LIST_ROWS);
    }

    #[test]
    fn list_commands_and_intents_follow_the_fixture() {
        let mut gallery = Gallery::new();
        let mut generation = Generation::default();
        assert_eq!(gallery.state().list_seed, LIST_SEED);
        assert_eq!(gallery.state().list_selected, Some(LIST_SELECTED));
        assert_eq!(
            gallery.apply(GalleryCommand::ListReplace { seed: 5 }, &mut generation),
            Effect::List
        );
        assert_eq!(gallery.state().list_seed, 5);
        assert_eq!(
            gallery.state().list_selected,
            Some(LIST_SELECTED),
            "номер выбора переживает замену"
        );
        assert_eq!(
            gallery.apply(GalleryCommand::ListScroll { row: 9_999 }, &mut generation),
            Effect::ListScroll(9_999)
        );
        assert!(!gallery.state().list_ticks, "серия по умолчанию без тактов");
        assert_eq!(
            gallery.apply(GalleryCommand::ListTicks { enabled: true }, &mut generation),
            Effect::ListTicks
        );
        assert!(gallery.state().list_ticks);
        let changed = gallery.intent(
            &Intent::List(ListAction::Select(list_row_id(5, 20))),
            &mut generation,
        );
        assert_eq!(changed, Some(Changed::List));
        assert_eq!(gallery.state().list_selected, Some(20));
        let stale = gallery.intent(
            &Intent::List(ListAction::Select(list_row_id(0, 21))),
            &mut generation,
        );
        assert_eq!(stale, None, "id прежнего набора выбора не меняет");
        let opened = gallery.intent(
            &Intent::List(ListAction::Activate(list_row_id(5, 20))),
            &mut generation,
        );
        assert_eq!(opened, None, "открытие только записывается");
        let records: Vec<(IntentKind, Option<&str>)> = gallery
            .state()
            .intents
            .iter()
            .map(|r| (r.intent, r.value.as_deref()))
            .collect();
        assert_eq!(
            records,
            [
                (IntentKind::Select, Some("s5.r00020")),
                (IntentKind::Select, Some("s0.r00021")),
                (IntentKind::Activate, Some("s5.r00020")),
            ]
        );
        gallery.apply(GalleryCommand::Reset, &mut generation);
        assert_eq!(gallery.state().list_seed, LIST_SEED);
        assert_eq!(gallery.state().list_selected, Some(LIST_SELECTED));
    }

    /// Список в подложке без фокуса держится дальше 4 точек от её участка фона (24, 460) 120×48 и
    /// целиком внутри подложки.
    #[test]
    fn list_stays_inside_the_plain_surface_and_clear_of_its_region() {
        let layout = layout(1180.0, 720.0, 40.0, 32.0, 34.0, 26.0);
        let inner = Rect::new(0.0, 0.0, layout.plain.width, layout.plain.height);
        assert!(inner.contains(&layout.list), "{:?}", layout.list);
        let region = Rect::new(20.0, 456.0, 128.0, 56.0);
        assert!(!layout.list.intersects(&region), "{:?}", layout.list);
        assert_eq!(layout.list.height, 16.0 * 26.0);
        assert!(
            layout.plain.width - layout.list.right() >= 240.0,
            "правая часть подложки свободна"
        );
    }

    #[test]
    fn tree_fixture_is_hundred_by_nine_by_ten() {
        let roots = tree_roots(0, false);
        assert_eq!(roots.len(), TREE_ROOTS);
        assert!(
            roots
                .iter()
                .all(|root| root.children.len() == TREE_BRANCHES)
        );
        assert!(roots.iter().all(|root| {
            root.children
                .iter()
                .all(|branch| branch.children.len() == TREE_LEAVES)
        }));
        assert_eq!(roots[0].id, "n000");
        assert_eq!(roots[0].children[0].id, "n000.0");
        assert_eq!(roots[0].children[0].children[0].id, TREE_SELECTED);
        assert_eq!(roots[99].children[8].children[9].text, "Лист 099.8.9");
        let mut ids = std::collections::HashSet::new();
        let mut stack: Vec<&TreeFixture> = roots.iter().collect();
        while let Some(node) = stack.pop() {
            assert!(ids.insert(node.id.clone()), "{} повторяется", node.id);
            assert_eq!(
                tree_node(0, &node.id),
                Some(!node.children.is_empty()),
                "{}",
                node.id
            );
            for child in &node.children {
                assert_eq!(tree_parent(&child.id), Some(node.id.as_str()));
            }
            stack.extend(node.children.iter());
        }
        assert_eq!(ids.len(), 10_000);
        for foreign in [
            "n100",
            "n00",
            "n000.9",
            "n000.0.10",
            "x1",
            "n000.x1",
            "n0000",
            "n000.00",
            "",
        ] {
            assert_eq!(tree_node(0, foreign), None, "{foreign}");
        }
    }

    #[test]
    fn tree_mutations_shift_rows_and_keep_expansion_and_selection() {
        let roots = tree_roots(2, true);
        let order: Vec<&str> = roots.iter().take(4).map(|root| root.id.as_str()).collect();
        assert_eq!(order, ["x2", "x1", "n001", "n000"]);
        let first: Vec<&str> = roots[3]
            .children
            .iter()
            .take(3)
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(first, ["n000.x2", "n000.x1", "n000.0"]);
        assert_eq!(tree_node(2, "x2"), Some(false));
        assert_eq!(tree_node(2, "n000.x2"), Some(false));
        assert_eq!(tree_node(2, "x3"), None);
        assert_eq!(tree_node(2, "n001.x1"), None, "вставка только в n000");

        let mut gallery = Gallery::new();
        let mut generation = Generation::default();
        let insert = GalleryCommand::TreeMutate {
            op: TreeMutation::Insert,
        };
        assert_eq!(gallery.apply(insert, &mut generation), Effect::Tree);
        gallery.apply(
            GalleryCommand::TreeMutate {
                op: TreeMutation::Reorder,
            },
            &mut generation,
        );
        let state = gallery.state();
        assert_eq!((state.tree_inserts, state.tree_swapped), (1, true));
        assert_eq!(state.tree_selected.as_deref(), Some(TREE_SELECTED));
        assert_eq!(
            state.tree_expanded,
            TREE_EXPANDED.iter().map(|id| (*id).to_owned()).collect()
        );

        gallery.apply(
            GalleryCommand::TreeMutate {
                op: TreeMutation::CollapseParentOfSelected,
            },
            &mut generation,
        );
        assert_eq!(gallery.state().tree_selected.as_deref(), Some("n000.0"));
        assert_eq!(
            gallery.state().tree_expanded,
            BTreeSet::from(["n000".to_owned()])
        );
        gallery.apply(
            GalleryCommand::TreeMutate {
                op: TreeMutation::CollapseParentOfSelected,
            },
            &mut generation,
        );
        assert_eq!(gallery.state().tree_selected.as_deref(), Some("n000"));
        assert!(gallery.state().tree_expanded.is_empty());
        gallery.apply(
            GalleryCommand::TreeMutate {
                op: TreeMutation::CollapseParentOfSelected,
            },
            &mut generation,
        );
        assert_eq!(
            gallery.state().tree_selected.as_deref(),
            Some("n000"),
            "у корня родителя нет"
        );
        gallery.apply(GalleryCommand::Reset, &mut generation);
        assert_eq!(gallery.state(), &GalleryState::fixture());
    }

    #[test]
    fn tree_intents_follow_the_visible_tree() {
        let mut gallery = Gallery::new();
        let mut generation = Generation::default();
        let tree = |action| Intent::Tree(action);
        assert_eq!(
            gallery.intent(
                &tree(TreeAction::Select("n000.0.5".into())),
                &mut generation
            ),
            Some(Changed::Tree)
        );
        assert_eq!(
            gallery.intent(&tree(TreeAction::Select("n001.0".into())), &mut generation),
            None,
            "скрытый узел не выбирается"
        );
        assert_eq!(
            gallery.intent(&tree(TreeAction::Select("нет".into())), &mut generation),
            None
        );
        assert_eq!(
            gallery.intent(
                &tree(TreeAction::Toggle("n001".into(), true)),
                &mut generation
            ),
            Some(Changed::Tree)
        );
        assert!(gallery.state().tree_expanded.contains("n001"));
        gallery.intent(
            &tree(TreeAction::Toggle("n000.0".into(), false)),
            &mut generation,
        );
        assert_eq!(gallery.state().tree_selected.as_deref(), Some("n000.0"));
        gallery.intent(
            &tree(TreeAction::Toggle("n000.0.3".into(), true)),
            &mut generation,
        );
        assert!(
            !gallery.state().tree_expanded.contains("n000.0.3"),
            "лист не раскрывается"
        );
        gallery.intent(&tree(TreeAction::Activate("n000".into())), &mut generation);
        let records: Vec<(&str, IntentKind, Option<&str>)> = gallery
            .state()
            .intents
            .iter()
            .map(|r| (r.component.as_str(), r.intent, r.value.as_deref()))
            .collect();
        assert_eq!(
            records[0],
            (ids::TREE, IntentKind::Select, Some("n000.0.5"))
        );
        assert_eq!(records[3], (ids::TREE, IntentKind::Toggle, Some("n001")));
        assert_eq!(
            records.last(),
            Some(&(ids::TREE, IntentKind::Activate, Some("n000")))
        );
    }

    /// Дерево справа от списка в подложке без фокуса: целиком внутри, дальше 4 точек от участка
    /// фона (24, 460) 120×48 и не накрывает список.
    #[test]
    fn tree_stays_inside_the_plain_surface_right_of_the_list() {
        let layout = layout(1180.0, 720.0, 40.0, 32.0, 34.0, 26.0);
        let inner = Rect::new(0.0, 0.0, layout.plain.width, layout.plain.height);
        assert!(inner.contains(&layout.tree), "{:?}", layout.tree);
        assert!(!layout.tree.intersects(&Rect::new(20.0, 456.0, 128.0, 56.0)));
        assert!(!layout.tree.intersects(&layout.list));
        assert!(layout.tree.x >= 284.0);
        assert!(layout.tree.width >= 240.0, "{:?}", layout.tree);
        assert_eq!(layout.tree.height, layout.list.height);
    }
}
