//! Запрос `ui` протокола v1 (ADR-0007, «Уточнено в P1.3»): чтения окна владельца, действия
//! интерфейса [`UiCommand`] и отметка начала выполнения на главном потоке.
//!
//! Кадр запроса — объект с одним ключом, именем запроса: `{"ui.window": {}}`,
//! `{"ui.appearance.set": {"appearance": "light"}}`. Ответ лежит под ключом `ui` значения `ok`
//! и тоже несёт один ключ: `window`, `inspect`, `stats` или `accepted`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};
use shellio_core::{AppearanceChoice, UiCommand};

/// Срок запроса `ui` на главном потоке владельца.
pub const UI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Запрос `ui`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "WireUiRequest", into = "WireUiRequest")]
pub enum UiRequest {
    /// `ui.window`: окно владельца.
    Window,
    /// `ui.inspect`: видимое дерево компонентов окна.
    Inspect,
    /// `ui.stats`: счётчики, монитор цикла и старт процесса.
    Stats,
    /// Действие интерфейса: тот же обработчик, что у главного меню.
    Command(UiCommand),
}

impl UiRequest {
    /// Имя запроса в кадре и в ADR-0007.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Window => "ui.window",
            Self::Inspect => "ui.inspect",
            Self::Stats => "ui.stats",
            Self::Command(command) => command.name(),
        }
    }

    /// Изменяет ли запрос состояние: начатый изменяющий запрос не повторяется.
    pub const fn is_mutation(self) -> bool {
        match self {
            Self::Window | Self::Inspect | Self::Stats => false,
            Self::Command(_) => true,
        }
    }

    /// Отвечает ли `reply` на этот запрос.
    pub const fn answered_by(self, reply: &UiReply) -> bool {
        matches!(
            (self, reply),
            (Self::Window, UiReply::Window(_))
                | (Self::Inspect, UiReply::Inspect(_))
                | (Self::Stats, UiReply::Stats(_))
                | (Self::Command(_), UiReply::Accepted(_))
        )
    }
}

/// Пустое тело запроса без аргументов: `{}`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppearanceArgs {
    appearance: AppearanceChoice,
}

#[derive(Serialize, Deserialize)]
enum WireUiRequest {
    #[serde(rename = "ui.window")]
    Window(NoArgs),
    #[serde(rename = "ui.inspect")]
    Inspect(NoArgs),
    #[serde(rename = "ui.stats")]
    Stats(NoArgs),
    #[serde(rename = "window.close")]
    WindowClose(NoArgs),
    #[serde(rename = "app.quit")]
    AppQuit(NoArgs),
    #[serde(rename = "ui.appearance.set")]
    AppearanceSet(AppearanceArgs),
}

impl From<WireUiRequest> for UiRequest {
    fn from(wire: WireUiRequest) -> Self {
        match wire {
            WireUiRequest::Window(NoArgs {}) => Self::Window,
            WireUiRequest::Inspect(NoArgs {}) => Self::Inspect,
            WireUiRequest::Stats(NoArgs {}) => Self::Stats,
            WireUiRequest::WindowClose(NoArgs {}) => Self::Command(UiCommand::WindowClose),
            WireUiRequest::AppQuit(NoArgs {}) => Self::Command(UiCommand::AppQuit),
            WireUiRequest::AppearanceSet(AppearanceArgs { appearance }) => {
                Self::Command(UiCommand::AppearanceSet(appearance))
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
            UiRequest::Command(UiCommand::WindowClose) => Self::WindowClose(NoArgs {}),
            UiRequest::Command(UiCommand::AppQuit) => Self::AppQuit(NoArgs {}),
            UiRequest::Command(UiCommand::AppearanceSet(appearance)) => {
                Self::AppearanceSet(AppearanceArgs { appearance })
            }
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
    /// Действие принято обработчиком на главном потоке.
    Accepted(Accepted),
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
    /// Состояние компонента; у частей компонента его нет.
    pub state: Option<ComponentState>,
}

/// Состояние компонента по классу.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    Chrome(ChromeView),
    TextField(TextFieldView),
    Menu(MenuView),
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GalleryTreeView {
    /// Номер построения дерева галереи.
    pub tree: u64,
    /// Видов под слабыми ссылками.
    pub views: u64,
    /// Из них ещё живы.
    pub alive: u64,
}

/// Отказ обработчика `ui`. Срок и неизвестный исход ставит сервер.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UiRefusal {
    /// UI нет: тестовый владелец или главный поток больше не выполняет работу.
    NoUi,
    /// Запрос `gallery.*` вне режима галереи.
    GalleryOnly,
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
        ];
        for (request, name, mutation) in cases {
            assert_eq!(request.name(), name);
            assert_eq!(request.is_mutation(), mutation, "{name}");
            let json = serde_json::to_value(request).unwrap();
            let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
            assert_eq!(keys, [name]);
            assert_eq!(serde_json::from_value::<UiRequest>(json).unwrap(), request);
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
        ] {
            assert!(
                serde_json::from_value::<UiRequest>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn axis_sources_have_stable_names() {
        for (source, name) in [
            (AxisSource::System, "system"),
            (AxisSource::UiAppearance, "ui.appearance"),
            (AxisSource::Gallery, "gallery"),
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
    fn replies_answer_only_their_request() {
        let accepted = UiReply::Accepted(Accepted {
            command: "window.close".into(),
            pid: 1,
            generation: 2,
        });
        assert!(UiRequest::Command(UiCommand::WindowClose).answered_by(&accepted));
        assert!(!UiRequest::Window.answered_by(&accepted));
    }
}
