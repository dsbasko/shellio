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

use shellio_control::{GalleryCommand, GalleryFocus, IntentKind, IntentRecord, UiRefusal};
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

/// Сколько последних намерений держит журнал.
pub const INTENT_LOG: usize = 256;

/// Намерение компонента галереи.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    FieldChanged(String),
    FieldSubmitted(String),
    FieldCancelled,
    MenuChosen(String),
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
    pub intents: VecDeque<IntentRecord>,
}

impl GalleryState {
    pub fn fixture() -> GalleryState {
        GalleryState {
            appearance: None,
            contrast: None,
            motion: None,
            focus: GalleryFocus::Window,
            screen: None,
            field_value: field_fixture().value,
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
    /// Состояние вернулось к фикстуре: тема, поле, фокус и меню приводятся к нему.
    Reset,
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
        };
        generation.bump();
        effect
    }

    /// Записать намерение компонента. Поле владелец не перерисовывает: вид уже показывает
    /// значение, а эхо отставшего значения стёрло бы ввод, пришедший после намерения.
    pub fn intent(&mut self, intent: &Intent, generation: &mut Generation) {
        let generation = generation.bump();
        if let Intent::FieldChanged(value) = intent {
            value.clone_into(&mut self.state.field_value);
        }
        if self.state.intents.len() == INTENT_LOG {
            self.state.intents.pop_front();
        }
        self.state.intents.push_back(intent.record(generation));
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

/// Раскладка галереи в содержимом окна. Маркер и подложки — в координатах содержимого, поле и
/// кнопка «…» — в координатах подложки с фокусом.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub marker: Rect,
    pub focus: Rect,
    pub plain: Rect,
    pub field: Rect,
    pub menu: Rect,
}

/// Раскладка для содержимого `width`×`height` под заголовком высотой `titlebar` и поля высотой
/// `field_height`. Маркер стоит в левом верхнем углу под заголовком: вдали от скруглённых углов
/// окна и от кнопок заголовка. Края — на целых точках.
pub fn layout(width: f64, height: f64, titlebar: f64, field_height: f64) -> Layout {
    let top = titlebar + PAD;
    let marker = Rect::new(PAD, top, MARKER_SIZE, MARKER_SIZE);
    let left = marker.right() + PAD;
    let surface_width = ((width - left - 2.0 * PAD) / 2.0).floor().max(0.0);
    let surface_height = (height - top - PAD).max(0.0);
    let focus = Rect::new(left, top, surface_width, surface_height);
    let plain = Rect::new(focus.right() + PAD, top, surface_width, surface_height);
    let field = Rect::new(PAD, PAD, FIELD_WIDTH, field_height);
    let menu = Rect::new(
        field.right() + PAD,
        PAD + ((field_height - MENU_BUTTON) / 2.0).round(),
        MENU_BUTTON,
        MENU_BUTTON,
    );
    Layout {
        marker,
        focus,
        plain,
        field,
        menu,
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
        let layout = layout(width, height, titlebar, field);
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
}
