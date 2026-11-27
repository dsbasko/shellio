//! `ChromeBar` — заголовок окна высотой 40 точек (ADR-0024, ADR-0026). Слева кнопки окна и кнопка
//! воркспейса (имя и шеврон), по центру поле поиска — неактивный образец до P1.6, справа
//! кнопки зон (недоступны без открытой рабочей области) и кнопка оформления, которая по кругу вызывает
//! `ui.appearance.set`.
//!
//! Чистая часть — раскладка ([`ChromeLayout`]), сокращение имени ([`shorten`]) и состояние кнопок
//! ([`ChromeState`]); её тестирует `make check`. Вид применяет их к AppKit. Окно двигается за
//! пустую часть заголовка, а не за кнопки и поле.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityButtonRole, NSAccessibilityToggleSubrole,
    NSAutoresizingMaskOptions, NSButton, NSCellImagePosition, NSControl, NSEvent, NSFont,
    NSFontWeightMedium, NSFontWeightRegular, NSImageView, NSLineBreakMode, NSResponder,
    NSTextField, NSView, NSViewLayerContentsRedrawPolicy, NSWindow, NSWindowButton,
};
use objc2_foundation::{
    NSAlignmentOptions, NSNumber, NSObject, NSPoint, NSRect, NSSize, NSString, NSUserDefaults,
};
use objc2_quartz_core::CALayer;
use shellio_theme::{AppTheme, AppearanceChoice};

use crate::app::App;
use crate::counters::{Live, counter};
use crate::icon;
use crate::inspect::{self, InspectNode, NodeState};
use crate::key_badge::{KEY_SIZE, KeyBadge};
use crate::layer::{self, without_actions};
use crate::own::{Callback, ComponentId, View};
use crate::runloop;
use crate::status_line::key_badges;
use crate::style;

counter!(CHROME_OWNER, Owner, "ChromeBar");
counter!(CHROME_VIEW, Class, "ShellioChromeBar");
counter!(CHROME_SEARCH, Class, "ShellioChromeSearch");
counter!(CHROME_BUTTON, Class, "ShellioChromeButton");

/// Отступ содержимого от левого края (`.titlebar { padding: 0 10px 0 14px }`).
pub const PAD_LEFT: f64 = 14.0;
/// Отступ содержимого от правого края.
pub const PAD_RIGHT: f64 = 10.0;
/// Промежуток между колонками сетки заголовка.
pub const GRID_GAP: f64 = 16.0;
/// Промежуток между кнопками окна и кнопкой воркспейса (`.lights { margin-right: 12px }`).
pub const LIGHTS_GAP: f64 = 12.0;
/// Ширина поля поиска: не меньше и не больше.
pub const SEARCH_MIN: f64 = 300.0;
pub const SEARCH_MAX: f64 = 460.0;
pub const SEARCH_HEIGHT: f64 = 26.0;
/// Высота кнопки воркспейса.
pub const WORKSPACE_HEIGHT: f64 = 22.0;
/// Поля кнопки воркспейса и промежуток между названием и шевроном
/// (`.ws-btn { gap: 3px; padding: 0 3px 0 6px }`).
pub const WORKSPACE_PAD_LEFT: f64 = 6.0;
pub const WORKSPACE_PAD_RIGHT: f64 = 3.0;
pub const WORKSPACE_GAP: f64 = 3.0;
/// Кнопка группы зон и её рамка (`.seg .ib { width: 30px }`, рамка 1).
pub const ZONE_WIDTH: f64 = 30.0;
pub const ZONE_HEIGHT: f64 = 24.0;
pub const ZONE_COUNT: usize = 3;
pub const SEG_BORDER: f64 = 1.0;
/// Кнопка-значок (`.ib`).
pub const ICON_WIDTH: f64 = 28.0;
pub const ICON_HEIGHT: f64 = 24.0;
/// Промежуток между группой зон и кнопкой оформления.
pub const RIGHT_GAP: f64 = 8.0;
/// Радиус поля поиска и группы зон.
const FIELD_RADIUS: f64 = 6.0;
/// Сторона значка кнопок зон и оформления (`icon(name, 15)` прототипа).
const BUTTON_ICON: f64 = 15.0;
/// Сторона шеврона кнопки воркспейса.
pub const CHEVRON_ICON: f64 = 11.0;
/// Знак сокращения имени.
pub const ELLIPSIS: char = '…';
/// Имя без активного воркспейса.
pub const NO_WORKSPACE: &str = "Нет воркспейса";

/// Прямоугольник в точках, начало сверху слева.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Frame {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Frame {
        Frame {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn center_x(&self) -> f64 {
        self.x + self.width / 2.0
    }

    pub fn intersects(&self, other: &Frame) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }

    pub(crate) fn rect(self) -> NSRect {
        NSRect::new(
            NSPoint::new(self.x, self.y),
            NSSize::new(self.width, self.height),
        )
    }
}

/// Раскладка заголовка шириной `width` и высотой `height`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeLayout {
    /// Кнопка воркспейса шириной не больше [`ChromeLayout::workspace_max`].
    pub workspace: Frame,
    /// Сколько места есть у кнопки воркспейса до поля поиска.
    pub workspace_max: f64,
    /// Поле поиска по центру заголовка.
    pub search: Frame,
    /// Группа кнопок зон с рамкой.
    pub zones: Frame,
    /// Кнопки зон внутри группы, в координатах группы.
    pub zone_buttons: [Frame; ZONE_COUNT],
    pub appearance: Frame,
}

impl ChromeLayout {
    /// `lights_right` — правый край кнопок окна в координатах заголовка, `None` — кнопок в нём нет
    /// (полноэкранный режим). `workspace_width` — нужная кнопке воркспейса ширина с полным именем.
    ///
    /// Поле поиска стоит по центру заголовка, а не по центру сетки прототипа: отступы сетки слева
    /// и справа разные, а центр окна читают Mission Control и сценарий `window`.
    pub fn compute(
        width: f64,
        height: f64,
        lights_right: Option<f64>,
        workspace_width: f64,
    ) -> ChromeLayout {
        let search_width =
            (width - PAD_LEFT - PAD_RIGHT - 2.0 * GRID_GAP).clamp(SEARCH_MIN, SEARCH_MAX);
        let search = Frame::new(
            ((width - search_width) / 2.0).floor(),
            ((height - SEARCH_HEIGHT) / 2.0).floor(),
            search_width,
            SEARCH_HEIGHT,
        );
        let workspace_x = lights_right.map_or(PAD_LEFT, |right| (right + LIGHTS_GAP).ceil());
        let workspace_max = (search.x - GRID_GAP - workspace_x).max(0.0);
        let workspace = Frame::new(
            workspace_x,
            ((height - WORKSPACE_HEIGHT) / 2.0).floor(),
            workspace_width.clamp(0.0, workspace_max),
            WORKSPACE_HEIGHT,
        );
        let appearance = Frame::new(
            width - PAD_RIGHT - ICON_WIDTH,
            ((height - ICON_HEIGHT) / 2.0).floor(),
            ICON_WIDTH,
            ICON_HEIGHT,
        );
        // Разделитель — левая линия второй и третьей кнопки внутри их ширины (`.seg .ib + .ib`
        // прототипа), поэтому к ширине группы добавляется только её рамка.
        let zones_width = ZONE_COUNT as f64 * ZONE_WIDTH + 2.0 * SEG_BORDER;
        let zones_height = ZONE_HEIGHT + 2.0 * SEG_BORDER;
        let zones = Frame::new(
            appearance.x - RIGHT_GAP - zones_width,
            ((height - zones_height) / 2.0).floor(),
            zones_width,
            zones_height,
        );
        let zone_buttons = std::array::from_fn(|index| {
            Frame::new(
                SEG_BORDER + index as f64 * ZONE_WIDTH,
                SEG_BORDER,
                ZONE_WIDTH,
                ZONE_HEIGHT,
            )
        });
        ChromeLayout {
            workspace,
            workspace_max,
            search,
            zones,
            zone_buttons,
            appearance,
        }
    }
}

/// Ширина кнопки воркспейса с названием шириной набора `text_width`: поля, название, промежуток
/// и шеврон. Ширина не округляется: её несёт набор.
pub fn workspace_width(text_width: f64) -> f64 {
    WORKSPACE_PAD_LEFT + text_width.max(0.0) + WORKSPACE_GAP + CHEVRON_ICON + WORKSPACE_PAD_RIGHT
}

/// Части кнопки воркспейса в её точках, начало сверху слева.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorkspaceParts {
    /// Место набора названия.
    pub label: Frame,
    pub chevron: Frame,
}

/// Части кнопки воркспейса шириной `width` с названием высотой `text_height`: название от левого
/// поля, шеврон сразу за ним через промежуток, оба по середине высоты. В кнопке уже своей ширины
/// сжимается название, шеврон остаётся целым.
pub fn workspace_parts(width: f64, text_height: f64) -> WorkspaceParts {
    let fixed = workspace_width(0.0);
    let label_width = (width - fixed).max(0.0);
    let label = Frame::new(
        WORKSPACE_PAD_LEFT,
        (WORKSPACE_HEIGHT - text_height) / 2.0,
        label_width,
        text_height,
    );
    let chevron = Frame::new(
        label.right() + WORKSPACE_GAP,
        (WORKSPACE_HEIGHT - CHEVRON_ICON) / 2.0,
        CHEVRON_ICON,
        CHEVRON_ICON,
    );
    WorkspaceParts { label, chevron }
}

/// Имя, которое помещается в `max_width` по мерке `measure`. Длинное имя сокращается с конца до
/// [`ELLIPSIS`]; если не помещается и он, остаётся один знак сокращения. Мерка возвращает ширину
/// всей кнопки с этим текстом ([`workspace_width`]), поэтому шеврон остаётся виден при любом
/// имени.
pub fn shorten(name: &str, max_width: f64, measure: impl Fn(&str) -> f64) -> String {
    if measure(name) <= max_width {
        return name.to_owned();
    }
    let ends: Vec<usize> = name
        .char_indices()
        .map(|(index, _)| index)
        .skip(1)
        .chain(std::iter::once(name.len()))
        .collect();
    let candidate = |chars: usize| -> String {
        let prefix = match chars {
            0 => "",
            _ => &name[..ends[chars - 1]],
        };
        format!("{}{ELLIPSIS}", prefix.trim_end())
    };
    // Мерка растёт с числом знаков, поэтому ищется наибольший помещающийся префикс.
    let (mut fits, mut over) = (0_usize, ends.len());
    while over - fits > 1 {
        let middle = (fits + over) / 2;
        if measure(&candidate(middle)) <= max_width {
            fits = middle;
        } else {
            over = middle;
        }
    }
    candidate(fits)
}

/// Значок набора `P` и подпись кнопки оформления по текущему выбору из трёх.
pub fn appearance_button(choice: AppearanceChoice) -> (&'static str, &'static str) {
    match choice {
        AppearanceChoice::System => ("appearanceSystem", "Оформление: системное"),
        AppearanceChoice::Light => ("sun", "Оформление: светлое"),
        AppearanceChoice::Dark => ("moon", "Оформление: тёмное"),
    }
}

/// Подпись кнопки воркспейса для доступности: полное имя, даже когда на экране оно сокращено.
pub fn workspace_label(name: &str) -> String {
    format!("Воркспейс: {name}")
}

/// Состояние заголовка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromeState {
    /// Имя активного воркспейса; `None` — его нет.
    pub workspace: Option<String>,
    pub choice: AppearanceChoice,
    /// Меню воркспейса появится с источником в P1.7: до него кнопка не получает фокус.
    pub workspace_menu: bool,
    /// Кнопки зон доступны: в обычном окне с открытой рабочей областью.
    pub zones: bool,
    /// Третья кнопка смотрит на плитку левой зоны («Левая зона»), а не на правую зону.
    pub side_plate: bool,
    /// Видны ли цели кнопок: колонка навигатора, нижняя зона, третья цель.
    pub shown: [bool; ZONE_COUNT],
}

impl Default for ChromeState {
    fn default() -> ChromeState {
        ChromeState {
            workspace: None,
            choice: AppearanceChoice::System,
            workspace_menu: false,
            zones: false,
            side_plate: false,
            shown: [false; ZONE_COUNT],
        }
    }
}

impl ChromeState {
    pub fn workspace_text(&self) -> &str {
        self.workspace.as_deref().unwrap_or(NO_WORKSPACE)
    }
}

/// Намерение заголовка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeIntent {
    /// Щелчок по кнопке воркспейса: меню воркспейса появится в P1.7.
    WorkspaceMenu,
    /// Кнопка оформления: следующий выбор по кругу.
    AppearanceCycle,
    /// Кнопка группы зон.
    Zone(ZoneButton),
    /// Кнопка «⌘K поиск»: палитра команд.
    Search,
}

/// Кнопка группы зон заголовка и пункт меню «Вид» (ADR-0024).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZoneButton {
    /// Колонка навигатора, `⌘0`.
    Navigator,
    Bottom,
    /// Правая зона или плитка левой, `⌥⌘0`.
    Side,
}

impl ZoneButton {
    pub const ALL: [ZoneButton; ZONE_COUNT] =
        [ZoneButton::Navigator, ZoneButton::Bottom, ZoneButton::Side];
}

/// Значок набора прототипа и подпись кнопки зон при цели третьей кнопки `side_plate`.
pub fn zone_button(button: ZoneButton, side_plate: bool) -> (&'static str, &'static str) {
    match (button, side_plate) {
        (ZoneButton::Navigator, _) => ("sideL", "Навигация"),
        (ZoneButton::Bottom, _) => ("sideB", "Нижняя зона"),
        (ZoneButton::Side, false) => ("sideR", "Правая зона"),
        (ZoneButton::Side, true) => ("sideLP", "Левая зона"),
    }
}

/// Нажатые кнопки зон (прототип: класс `on` и `aria-pressed`): цель видна. Недоступные кнопки
/// без открытой рабочей области и в галерее не нажаты.
pub fn zone_pressed(state: &ChromeState) -> [bool; ZONE_COUNT] {
    if state.zones {
        state.shown
    } else {
        [false; ZONE_COUNT]
    }
}

/// Rust-часть заголовка.
struct ChromeModel {
    id: ComponentId,
    theme: Cell<AppTheme>,
    state: RefCell<ChromeState>,
    on_intent: Box<dyn Fn(ChromeIntent)>,
}

/// Заголовок окна на главном потоке.
pub struct ChromeBar {
    view: View<ChromeBarView, ChromeModel>,
}

impl ChromeBar {
    pub(crate) fn new(
        app: &App,
        theme: &AppTheme,
        on_intent: impl Fn(ChromeIntent) + 'static,
    ) -> ChromeBar {
        let mtm = app.mtm();
        let model = ChromeModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
            state: RefCell::new(ChromeState::default()),
            on_intent: Box::new(on_intent),
        };
        let mut view = View::new(&CHROME_OWNER, model, "ChromeBar.layout", |owner| {
            ChromeBarView::new(mtm, owner)
        });
        let parts = view.view().ivars();
        let (workspace, appearance) = (parts.workspace.clone(), parts.appearance.clone());
        let zone_buttons = parts.zone_buttons.clone();
        let workspace: &NSControl = &workspace;
        view.bind_action(workspace, "ChromeBar.workspace", |model| {
            (model.on_intent)(ChromeIntent::WorkspaceMenu)
        });
        let appearance: &NSControl = &appearance;
        view.bind_action(appearance, "ChromeBar.appearance", |model| {
            (model.on_intent)(ChromeIntent::AppearanceCycle)
        });
        for (button, kind) in zone_buttons.iter().zip(ZoneButton::ALL) {
            let control: &NSControl = button;
            view.bind_action(control, "ChromeBar.zone", move |model| {
                (model.on_intent)(ChromeIntent::Zone(kind))
            });
        }
        let bar = ChromeBar { view };
        bar.apply_state();
        bar.apply_theme();
        bar
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    pub fn state(&self) -> ChromeState {
        self.view.rust().state.borrow().clone()
    }

    /// Имя активного воркспейса; `None` — его нет.
    pub fn set_workspace(&self, name: Option<&str>) {
        self.update(|state| state.workspace = name.map(str::to_owned));
    }

    pub fn set_choice(&self, choice: AppearanceChoice) {
        self.update(|state| state.choice = choice);
    }

    /// Кнопки зон: `None` — недоступны (галерея или рабочая область не открыта), подписи прежние;
    /// иначе цель третьей кнопки и видимость целей.
    pub fn set_zones(&self, zones: Option<(bool, [bool; ZONE_COUNT])>) {
        self.update(|state| match zones {
            Some((side_plate, shown)) => {
                state.zones = true;
                state.side_plate = side_plate;
                state.shown = shown;
            }
            None => {
                state.zones = false;
                state.side_plate = false;
                state.shown = [false; ZONE_COUNT];
            }
        });
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        let model = self.view.rust();
        if model.theme.get() == *theme {
            return;
        }
        model.theme.set(*theme);
        self.apply_theme();
    }

    /// Пересчитать раскладку: кадр окна или кнопок окна изменился.
    pub(crate) fn relayout(&self) {
        self.view.view().setNeedsLayout(true);
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }

    /// Заголовок и его части для `ui.inspect`: сам заголовок с состоянием, кнопка воркспейса,
    /// поле поиска, группа и кнопки зон, кнопка оформления.
    pub(crate) fn inspect(&self) -> Vec<InspectNode> {
        const BAR: Option<&str> = Some("chrome");
        const ZONES: Option<&str> = Some("chrome.zones");
        let view = self.view.view();
        let parts = view.ivars();
        let state = self.state();
        const SEARCH: Option<&str> = Some("chrome.search");
        let search = parts.search.ivars();
        let glass: &NSView = &search.icon;
        let glass_size = glass.bounds().size.width;
        let glass_color = search
            .icon
            .contentTintColor()
            .and_then(|color| crate::style::rgba(&color));
        // Ключи сверки — таблица `shellio-conformance`; у кнопок зон они отличаются от имён узлов.
        let mut nodes = vec![
            inspect::node(
                "chrome",
                "ChromeBar",
                None,
                view,
                None,
                Some(NodeState::Chrome(state.clone())),
            )
            .with_root_ck(inspect::ck::CHROME)
            .with_line_layer(&parts.line, &[crate::style::Side::Bottom]),
            inspect::node(
                "chrome.workspace",
                "WorkspaceButton",
                BAR,
                &parts.workspace,
                Some(parts.workspace.isEnabled()),
                None,
            )
            .with_ck(inspect::ck::WORKSPACE),
        ];
        // Название и шеврон — виды внутри кнопки: кадр названия — место его набора.
        if parts.workspace_label.stringValue().length() > 0 {
            nodes.push(
                inspect::typeset_node(
                    "chrome.workspace.label",
                    "Label",
                    "chrome.workspace",
                    &parts.workspace_label,
                )
                .with_ck(inspect::ck::WORKSPACE_LABEL),
            );
        }
        let chevron: &NSView = &parts.workspace_chevron;
        nodes.push(
            inspect::node(
                "chrome.workspace.chevron",
                "Icon",
                Some("chrome.workspace"),
                chevron,
                None,
                None,
            )
            .with_ck(inspect::ck::WORKSPACE_CHEVRON)
            // Значок цветной: цвет он несёт в себе, свойством вида цвет не читается.
            .with_icon("chevD", chevron.bounds().size.width, None),
        );
        nodes.extend([
            inspect::node(
                "chrome.search",
                "SearchField",
                BAR,
                &parts.search,
                None,
                None,
            )
            .with_ck(inspect::ck::SEARCH),
            inspect::node("chrome.search.icon", "Icon", SEARCH, glass, None, None)
                .with_ck(inspect::ck::SEARCH_ICON)
                .with_icon("search", glass_size, glass_color),
            inspect::node(
                "chrome.search.label",
                "Label",
                SEARCH,
                &search.label,
                None,
                None,
            )
            .with_ck(inspect::ck::SEARCH_LABEL),
            search
                .key
                .node("chrome.search.kbd", "chrome.search")
                .with_ck(inspect::ck::SEARCH_KBD),
            inspect::node("chrome.zones", "ZoneGroup", BAR, &parts.zones, None, None)
                .with_ck(inspect::ck::ZONES),
        ]);
        let zone_ids = [
            "chrome.zone.left",
            "chrome.zone.bottom",
            "chrome.zone.right",
        ]
        .into_iter()
        .zip(inspect::ck::ZONE_BUTTONS);
        // Нажатая кнопка — значение AX-переключателя, а не выбор: факта `selected` у неё нет, как
        // и атрибута `AXSelected`, который с ним сверяет пункт `ax` E2E.
        let zone_parts = zone_ids
            .zip(&parts.zone_buttons)
            .zip(ZoneButton::ALL)
            .zip(inspect::ck::ZONE_ICONS)
            .enumerate();
        for (index, ((((id, ck), button), kind), icon_ck)) in zone_parts {
            let mut node = inspect::node(
                id,
                "ZoneButton",
                ZONES,
                button,
                Some(button.isEnabled()),
                None,
            )
            .with_ck(ck);
            // Линию слева от второй и третьей кнопки рисует слой-разделитель группы.
            if let Some(separator) = index.checked_sub(1).and_then(|i| parts.separators.get(i)) {
                node = node.with_line_layer(separator, &[crate::style::Side::Left]);
            }
            nodes.push(node);
            let (symbol, _) = zone_button(kind, state.side_plate);
            nodes.extend(
                inspect::button_image(&format!("{id}.icon"), id, button, symbol)
                    .map(|node| node.with_ck(icon_ck)),
            );
        }
        // Кнопки окна системные: сверяются место и состав, кадр и диаметр — исключение реестра.
        if let Some(window) = view.window() {
            let buttons = [
                (NSWindowButton::CloseButton, inspect::ck::WINDOW_BUTTONS[0]),
                (
                    NSWindowButton::MiniaturizeButton,
                    inspect::ck::WINDOW_BUTTONS[1],
                ),
                (NSWindowButton::ZoomButton, inspect::ck::WINDOW_BUTTONS[2]),
            ];
            for (kind, ck) in buttons {
                // В полноэкранном режиме кнопки живут в другом окне: их кадры к заголовку не
                // относятся.
                let own = |button: &NSButton| {
                    button
                        .window()
                        .is_some_and(|button_window| std::ptr::eq(&*button_window, &*window))
                };
                if let Some(button) = window.standardWindowButton(kind)
                    && own(&button)
                    && !button.isHidden()
                {
                    let enabled = button.isEnabled();
                    nodes.push(
                        inspect::node(ck, "WindowButton", BAR, &button, Some(enabled), None)
                            .with_ck(ck),
                    );
                }
            }
        }
        nodes.push(
            inspect::node(
                "chrome.appearance",
                "AppearanceButton",
                BAR,
                &parts.appearance,
                Some(parts.appearance.isEnabled()),
                None,
            )
            .with_ck(inspect::ck::APPEARANCE),
        );
        let (symbol, _) = appearance_button(state.choice);
        nodes.extend(
            inspect::button_image(
                "chrome.appearance.icon",
                "chrome.appearance",
                &parts.appearance,
                symbol,
            )
            .map(|node| node.with_ck(inspect::ck::APPEARANCE_ICON)),
        );
        nodes
    }

    fn update(&self, change: impl FnOnce(&mut ChromeState)) {
        let changed = {
            let mut state = self.view.rust().state.borrow_mut();
            let before = state.clone();
            change(&mut state);
            *state != before
        };
        if changed {
            self.apply_state();
        }
    }

    fn apply_state(&self) {
        let state = self.state();
        let parts = self.view.view().ivars();
        let label = NSString::from_str(&workspace_label(state.workspace_text()));
        parts.workspace.setAccessibilityLabel(Some(&label));
        parts.workspace.setToolTip(Some(&label));
        parts.workspace.ivars().focusable.set(state.workspace_menu);
        for (button, kind) in parts.zone_buttons.iter().zip(ZoneButton::ALL) {
            button.setEnabled(state.zones);
            let (symbol, label) = zone_button(kind, state.side_plate);
            let label = NSString::from_str(label);
            if button
                .accessibilityLabel()
                .is_none_or(|current| *current != *label)
            {
                button.setImage(icon::image(symbol, BUTTON_ICON, &label).as_deref());
                button.setAccessibilityLabel(Some(&label));
                button.setToolTip(Some(&label));
            }
        }
        self.style_zone_buttons();
        let (symbol, text) = appearance_button(state.choice);
        let text = NSString::from_str(text);
        parts
            .appearance
            .setImage(icon::image(symbol, BUTTON_ICON, &text).as_deref());
        parts.appearance.setAccessibilityLabel(Some(&text));
        parts.appearance.setToolTip(Some(&text));
        self.relayout();
        // Отметка отрисовки заголовка называет поколение, которое показывает новое состояние:
        // по ней `ui.stats.startup` находит первую отрисовку с первым снимком зеркала.
        self.view.view().setNeedsDisplay(true);
    }

    /// Нажатая кнопка зоны — заливка `chrome_hi` и текст `text`, остальные без заливки с `text_3`
    /// (`.ib.on` прототипа). Для доступности кнопка — переключатель со значением 1 или 0, роль
    /// остаётся `AXButton`.
    fn style_zone_buttons(&self) {
        let model = self.view.rust();
        let theme = model.theme.get();
        let pressed = zone_pressed(&model.state.borrow());
        let parts = self.view.view().ivars();
        for (button, pressed) in parts.zone_buttons.iter().zip(pressed) {
            let ink = if pressed {
                theme.colors.text
            } else {
                theme.colors.text_3
            };
            button.setContentTintColor(Some(&layer::ns_color(ink)));
            let value = NSNumber::new_bool(pressed);
            // SAFETY: значение переключателя доступности — NSNumber, AppKit его копирует.
            unsafe { button.setAccessibilityValue(Some(&value)) };
            if let Some(fill) = button.layer() {
                without_actions(|| {
                    if pressed {
                        layer::set_fill(&fill, theme.colors.chrome_hi);
                    } else {
                        fill.setBackgroundColor(None);
                    }
                });
            }
        }
    }

    fn apply_theme(&self) {
        let theme = self.view.rust().theme.get();
        let view = self.view.view();
        let parts = view.ivars();
        let colors = &theme.colors;
        let text = layer::ns_color(colors.text);
        let muted = layer::ns_color(colors.text_3);
        parts.workspace_label.setTextColor(Some(&text));
        // Шеврон приглушён (`.ws-btn svg`), название — цвета текста: значок несёт свой цвет.
        let chevron = icon::colored(
            "chevD",
            CHEVRON_ICON,
            colors.text_3,
            colors.line_2,
            &NSString::from_str("Выбрать воркспейс"),
        );
        parts.workspace_chevron.setImage(Some(&chevron));
        parts
            .workspace_label
            .setFont(Some(&font(theme.metrics.font.body, Weight::Medium)));
        parts.appearance.setContentTintColor(Some(&muted));
        self.style_zone_buttons();
        let search = parts.search.ivars();
        search.icon.setContentTintColor(Some(&muted));
        search.label.setTextColor(Some(&muted));
        search
            .label
            .setFont(Some(&font(theme.metrics.font.body, Weight::Regular)));
        search.key.apply_theme(colors);
        if let Some(seg) = parts.zones.layer() {
            without_actions(|| {
                layer::set_fill(&seg, colors.chrome);
                layer::set_line(&seg, Some((colors.line, SEG_BORDER)));
                layer::set_radius(&seg, FIELD_RADIUS);
                for separator in &parts.separators {
                    layer::set_fill(separator, colors.line);
                }
            });
        }
        view.setNeedsDisplay(true);
        parts.search.setNeedsDisplay(true);
        self.relayout();
    }
}

#[derive(Clone, Copy)]
enum Weight {
    Regular,
    Medium,
}

fn font(size: f64, weight: Weight) -> Retained<NSFont> {
    // SAFETY: веса шрифта — неизменяемые константы AppKit.
    let weight = unsafe {
        match weight {
            Weight::Regular => NSFontWeightRegular,
            Weight::Medium => NSFontWeightMedium,
        }
    };
    NSFont::systemFontOfSize_weight(size, weight)
}

struct ChromeIvars {
    _live: Live,
    owner: Callback<ChromeModel>,
    workspace: Retained<ChromeButton>,
    /// Название и шеврон внутри кнопки воркспейса: поля и промежуток прототипа ячейка кнопки не
    /// раскладывает.
    workspace_label: Retained<NSTextField>,
    workspace_chevron: Retained<NSImageView>,
    search: Retained<SearchView>,
    zones: Retained<NSView>,
    separators: Vec<Retained<CALayer>>,
    zone_buttons: Vec<Retained<ChromeButton>>,
    appearance: Retained<ChromeButton>,
    line: Retained<CALayer>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioChromeBar"]
    #[ivars = ChromeIvars]
    struct ChromeBarView;

    impl ChromeBarView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `wantsUpdateLayer` NSView.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `updateLayer` NSView.
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            let Some(theme) = self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                model.theme.get()
            }) else {
                return;
            };
            if let Some(layer) = self.layer() {
                without_actions(|| {
                    layer::set_fill(&layer, theme.colors.chrome);
                    layer::set_fill(&self.ivars().line, theme.colors.line);
                });
            }
        }

        // SAFETY: селектор и результат — как у `layout` NSView.
        #[unsafe(method(layout))]
        fn layout(&self) {
            // SAFETY: `layout` у NSView без аргументов.
            let _: () = unsafe { msg_send![super(self), layout] };
            self.place();
        }

        // SAFETY: селектор и результат — как у `mouseDownCanMoveWindow` NSView: пустая часть
        // заголовка двигает окно.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            true
        }

        // SAFETY: селектор, аргумент и результат — как у `acceptsFirstMouse:` NSView.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let Some(window) = self.window() else {
                return;
            };
            if event.clickCount() == 2 {
                double_click(&window);
            } else {
                window.performWindowDragWithEvent(event);
            }
        }
    }
);

impl ChromeBarView {
    fn new(mtm: MainThreadMarker, owner: Callback<ChromeModel>) -> Retained<ChromeBarView> {
        let workspace = ChromeButton::new(mtm, false);
        let workspace_label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        workspace_label.setLineBreakMode(NSLineBreakMode::ByClipping);
        workspace_label.setAccessibilityElement(false);
        let workspace_chevron = NSImageView::initWithFrame(NSImageView::alloc(mtm), NSRect::ZERO);
        workspace_chevron.setAccessibilityElement(false);
        workspace.addSubview(&workspace_label);
        workspace.addSubview(&workspace_chevron);

        let zones = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        layered(&zones);
        if let Some(seg) = zones.layer() {
            // Заливка нажатой кнопки не выходит за скруглённую рамку группы (`overflow: hidden`).
            seg.setMasksToBounds(true);
        }
        let zone_buttons: Vec<_> = ZoneButton::ALL
            .into_iter()
            .map(|button| zone_button(button, false))
            .map(|(symbol, label)| {
                let button = ChromeButton::new(mtm, true);
                let label = NSString::from_str(label);
                button.setWantsLayer(true);
                // SAFETY: подроль доступности — неизменяемая константа AppKit.
                button.setAccessibilitySubrole(Some(unsafe { NSAccessibilityToggleSubrole }));
                button.setImagePosition(NSCellImagePosition::ImageOnly);
                button.setImage(icon::image(symbol, BUTTON_ICON, &label).as_deref());
                button.setAccessibilityLabel(Some(&label));
                button.setToolTip(Some(&label));
                zones.addSubview(&button);
                button
            })
            .collect();
        let separators: Vec<_> = (1..ZONE_COUNT)
            .map(|_| {
                let separator = CALayer::new();
                // Слои кнопок AppKit кладёт после своих слоёв группы: без подъёма заливка
                // нажатой кнопки закрывает её левую линию.
                separator.setZPosition(1.0);
                if let Some(seg) = zones.layer() {
                    seg.addSublayer(&separator);
                }
                separator
            })
            .collect();
        let appearance = ChromeButton::new(mtm, true);
        appearance.setImagePosition(NSCellImagePosition::ImageOnly);
        let search = SearchView::new(mtm, owner.clone());
        let line = CALayer::new();

        let this = ChromeBarView::alloc(mtm).set_ivars(ChromeIvars {
            _live: CHROME_VIEW.live(),
            owner,
            workspace,
            workspace_label,
            workspace_chevron,
            search,
            zones,
            separators,
            zone_buttons,
            appearance,
            line,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<ChromeBarView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        layered(&view);
        if let Some(layer) = view.layer() {
            layer.addSublayer(&view.ivars().line);
        }
        let parts = view.ivars();
        view.addSubview(&parts.workspace);
        view.addSubview(&parts.search);
        view.addSubview(&parts.zones);
        view.addSubview(&parts.appearance);
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        view
    }

    /// Раскладка по чистой части, кнопки окна по середине высоты заголовка.
    fn place(&self) {
        let parts = self.ivars();
        let Some(text) = parts
            .owner
            .call(|model| model.state.borrow().workspace_text().to_owned())
        else {
            return;
        };
        let bounds = self.bounds();
        let lights = self.center_window_buttons();
        let font = parts.workspace_label.font();
        let measure = |title: &str| {
            workspace_width(
                font.as_deref()
                    .map_or(0.0, |font| style::typeset_width(title, font)),
            )
        };
        let full = measure(&text);
        let layout = ChromeLayout::compute(bounds.size.width, bounds.size.height, lights, full);
        let shown = shorten(&text, layout.workspace_max, measure);
        let width = measure(&shown).min(layout.workspace_max);
        let shown = NSString::from_str(&shown);
        if *parts.workspace_label.stringValue() != *shown {
            parts.workspace_label.setStringValue(&shown);
        }
        let workspace = Frame {
            width,
            ..layout.workspace
        };
        let inside = workspace_parts(width, parts.workspace_label.intrinsicContentSize().height);
        // Набор названия начинается от левого поля кнопки: поле шире его на поля ячейки.
        let label = style::label_field_rect(&parts.workspace, inside.label);
        // Значок встаёт на пиксели экрана: дробное начало размывает его штрихи.
        let chevron = parts.workspace.backingAlignedRect_options(
            inside.chevron.rect(),
            NSAlignmentOptions::AlignAllEdgesNearest,
        );
        without_actions(|| {
            parts.workspace.setFrame(workspace.rect());
            parts.workspace_label.setFrame(label);
            parts.workspace_chevron.setFrame(chevron);
            parts.search.setFrame(layout.search.rect());
            parts.zones.setFrame(layout.zones.rect());
            for (button, frame) in parts.zone_buttons.iter().zip(layout.zone_buttons) {
                button.setFrame(frame.rect());
            }
            for (index, separator) in parts.separators.iter().enumerate() {
                let x = layout.zone_buttons[index + 1].x;
                separator.setFrame(Frame::new(x, 0.0, SEG_BORDER, layout.zones.height).rect());
            }
            parts.appearance.setFrame(layout.appearance.rect());
            parts
                .line
                .setFrame(Frame::new(0.0, bounds.size.height - 1.0, bounds.size.width, 1.0).rect());
        });
        parts.search.place();
    }

    /// Ставит кнопки окна по середине высоты заголовка и возвращает их правый край в координатах
    /// заголовка. `None` — кнопок в этом окне нет: в полноэкранном режиме они в другом окне.
    fn center_window_buttons(&self) -> Option<f64> {
        let window = self.window()?;
        let bar_top = self.convertRect_toView(self.bounds(), None);
        let target = bar_top.origin.y + bar_top.size.height / 2.0;
        let mut right: Option<f64> = None;
        for kind in [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ] {
            let Some(button) = window.standardWindowButton(kind) else {
                continue;
            };
            let own = button
                .window()
                .is_some_and(|button_window| std::ptr::eq(&*button_window, &*window));
            if !own || button.isHidden() {
                continue;
            }
            // SAFETY: `superview` у кнопки, которую держит окно.
            let Some(parent) = (unsafe { button.superview() }) else {
                continue;
            };
            let in_window = button.convertRect_toView(button.bounds(), None);
            let delta = target - (in_window.origin.y + in_window.size.height / 2.0);
            if delta.abs() >= 0.25 {
                let mut origin = button.frame().origin;
                origin.y += if parent.isFlipped() { -delta } else { delta };
                button.setFrameOrigin(origin);
            }
            let in_bar = button.convertRect_toView(button.bounds(), Some(self));
            let edge = in_bar.origin.x + in_bar.size.width;
            right = Some(right.map_or(edge, |right| right.max(edge)));
        }
        right
    }
}

/// Двойной щелчок по пустой части заголовка — по системной настройке «Double-click a window's
/// title bar to».
fn double_click(window: &NSWindow) {
    let action = NSUserDefaults::standardUserDefaults()
        .stringForKey(&NSString::from_str("AppleActionOnDoubleClick"))
        .map(|value| value.to_string());
    match action.as_deref() {
        Some("Minimize") => window.performMiniaturize(None),
        Some("None") => {}
        _ => window.performZoom(None),
    }
}

struct SearchIvars {
    _live: Live,
    owner: Callback<ChromeModel>,
    icon: Retained<NSImageView>,
    label: Retained<NSTextField>,
    /// Плашка клавиши (`kbd` прототипа): образец клавиши палитры.
    key: KeyBadge,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioChromeSearch"]
    #[ivars = SearchIvars]
    struct SearchView;

    impl SearchView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `wantsUpdateLayer` NSView.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `updateLayer` NSView.
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            let Some(theme) = self.ivars().owner.call(|model| model.theme.get()) else {
                return;
            };
            if let Some(layer) = self.layer() {
                without_actions(|| {
                    layer::set_fill(&layer, theme.colors.surface);
                    layer::set_line(&layer, Some((theme.colors.line, 1.0)));
                    layer::set_radius(&layer, FIELD_RADIUS);
                });
            }
        }

        // SAFETY: селектор и результат — как у `mouseDownCanMoveWindow` NSView: за поле окно не
        // двигается.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder. Нажатие открывает палитру
        // и окно не двигает.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            self.press();
        }

        // SAFETY: селектор и результат — как у `accessibilityPerformPress` NSAccessibility:
        // `AXPress` поля открывает палитру, как нажатие указателем.
        #[unsafe(method(accessibilityPerformPress))]
        fn accessibility_perform_press(&self) -> bool {
            self.press()
        }

        // SAFETY: селектор, аргумент и результат — как у `hitTest:` NSView. Подписи внутри поля
        // нажатий не получают: адресат — всё поле.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            let frame = self.frame();
            let inside = point.x >= frame.origin.x
                && point.x < frame.origin.x + frame.size.width
                && point.y >= frame.origin.y
                && point.y < frame.origin.y + frame.size.height;
            if inside && !self.isHidden() {
                let view: &NSView = self;
                std::ptr::from_ref(view).cast_mut()
            } else {
                std::ptr::null_mut()
            }
        }
    }
);

/// Отступ значка поиска от левого края поля (поле 9 и линия поля поиска) и промежуток до подписи.
const SEARCH_INSET: f64 = 10.0;
const SEARCH_ICON: f64 = 14.0;
const SEARCH_ICON_GAP: f64 = 8.0;
/// Правый отступ плашки клавиши: поле 6 и линия поля поиска.
const KEY_INSET: f64 = 7.0;
/// Образец клавиши палитры в записи keymap.
const SEARCH_KEY: &str = "Cmd-k";

impl SearchView {
    fn new(mtm: MainThreadMarker, owner: Callback<ChromeModel>) -> Retained<SearchView> {
        let description = NSString::from_str("Поиск");
        let glass = NSImageView::initWithFrame(NSImageView::alloc(mtm), NSRect::ZERO);
        glass.setImage(icon::image("search", SEARCH_ICON, &description).as_deref());
        let label =
            NSTextField::labelWithString(&NSString::from_str("Сессии, файлы, команды"), mtm);
        label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        let key = KeyBadge::new(mtm, &key_badges(&[SEARCH_KEY.to_owned()]).concat());
        let this = SearchView::alloc(mtm).set_ivars(SearchIvars {
            _live: CHROME_SEARCH.live(),
            owner,
            icon: glass,
            label,
            key,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<SearchView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        layered(&view);
        let parts = view.ivars();
        view.addSubview(&parts.icon);
        view.addSubview(&parts.label);
        view.addSubview(parts.key.view());
        view.setAccessibilityElement(true);
        // SAFETY: роль доступности — неизменяемая константа AppKit.
        view.setAccessibilityRole(Some(unsafe { NSAccessibilityButtonRole }));
        view.setAccessibilityLabel(Some(&NSString::from_str("Поиск и команды")));
        view
    }

    fn press(&self) -> bool {
        self.ivars()
            .owner
            .call(|model| (model.on_intent)(ChromeIntent::Search))
            .is_some()
    }

    fn place(&self) {
        let parts = self.ivars();
        let bounds = self.bounds();
        let height = bounds.size.height;
        let label = parts.label.intrinsicContentSize();
        let key_x = bounds.size.width - KEY_INSET - parts.key.width();
        let label_x = SEARCH_INSET + SEARCH_ICON + SEARCH_ICON_GAP;
        without_actions(|| {
            parts.icon.setFrame(
                Frame::new(
                    SEARCH_INSET,
                    ((height - SEARCH_ICON) / 2.0).floor(),
                    SEARCH_ICON,
                    SEARCH_ICON,
                )
                .rect(),
            );
            parts.label.setFrame(
                Frame::new(
                    label_x,
                    ((height - label.height) / 2.0).floor(),
                    (key_x - SEARCH_ICON_GAP - label_x).max(0.0),
                    label.height,
                )
                .rect(),
            );
            parts.key.place(key_x, ((height - KEY_SIZE) / 2.0).floor());
        });
    }
}

struct ButtonIvars {
    _live: Live,
    /// Кнопка получает фокус клавиатуры независимо от системной настройки Full Keyboard Access:
    /// клавиатурный путь заголовка — `Tab` по кнопкам.
    focusable: Cell<bool>,
}

define_class!(
    // SAFETY: NSButton не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSButton, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioChromeButton"]
    #[ivars = ButtonIvars]
    struct ChromeButton;

    impl ChromeButton {
        // SAFETY: селектор и результат — как у `acceptsFirstResponder` NSResponder.
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            self.ivars().focusable.get() && self.isEnabled()
        }

        // SAFETY: селектор и результат — как у `canBecomeKeyView` NSView.
        #[unsafe(method(canBecomeKeyView))]
        fn can_become_key_view(&self) -> bool {
            self.ivars().focusable.get() && self.isEnabled() && !self.isHiddenOrHasHiddenAncestor()
        }

        // SAFETY: селектор и результат — как у `mouseDownCanMoveWindow` NSView.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        // SAFETY: селектор, аргумент и результат — как у `hitTest:` NSView. Название и шеврон
        // внутри кнопки нажатий не получают: адресат — вся кнопка.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            let frame = self.frame();
            let inside = point.x >= frame.origin.x
                && point.x < frame.origin.x + frame.size.width
                && point.y >= frame.origin.y
                && point.y < frame.origin.y + frame.size.height;
            if inside && !self.isHidden() {
                let view: &NSView = self;
                std::ptr::from_ref(view).cast_mut()
            } else {
                std::ptr::null_mut()
            }
        }
    }
);

impl ChromeButton {
    fn new(mtm: MainThreadMarker, focusable: bool) -> Retained<ChromeButton> {
        let this = ChromeButton::alloc(mtm).set_ivars(ButtonIvars {
            _live: CHROME_BUTTON.live(),
            focusable: Cell::new(focusable),
        });
        // SAFETY: назначенный инициализатор NSButton (через NSControl), ivars уже заданы.
        let button: Retained<ChromeButton> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        button.setBordered(false);
        button.setTitle(&NSString::from_str(""));
        button
    }
}

/// Вид со своим слоем, который перерисовывается только по `setNeedsDisplay`.
fn layered(view: &NSView) {
    view.setWantsLayer(true);
    view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Мерка кнопки: 7 точек на знак, 6 на знак сокращения, плюс шеврон и поля 25 точек.
    fn measure(text: &str) -> f64 {
        25.0 + text
            .chars()
            .map(|char| if char == ELLIPSIS { 6.0 } else { 7.0 })
            .sum::<f64>()
    }

    #[test]
    fn search_is_centered_in_the_bar_and_clamped() {
        for width in [1180.0, 1512.0, 2560.0] {
            let layout = ChromeLayout::compute(width, 40.0, Some(68.0), 120.0);
            assert!(
                (layout.search.center_x() - width / 2.0).abs() <= 1.0,
                "центр поиска {} при ширине {width}",
                layout.search.center_x()
            );
            assert_eq!(layout.search.width, SEARCH_MAX);
            assert_eq!(layout.search.height, SEARCH_HEIGHT);
            assert_eq!(layout.search.y, 7.0, "по середине высоты 40");
        }
        let narrow = ChromeLayout::compute(500.0, 40.0, Some(68.0), 120.0);
        assert_eq!(
            narrow.search.width,
            500.0 - PAD_LEFT - PAD_RIGHT - 2.0 * GRID_GAP
        );
        let tiny = ChromeLayout::compute(200.0, 40.0, Some(68.0), 120.0);
        assert_eq!(tiny.search.width, SEARCH_MIN, "не уже минимума");
    }

    #[test]
    fn workspace_button_follows_window_buttons_and_stops_before_search() {
        let layout = ChromeLayout::compute(1180.0, 40.0, Some(67.5), 2000.0);
        assert_eq!(
            layout.workspace.x, 80.0,
            "правый край кнопок окна + 12, вверх до точки"
        );
        assert_eq!(layout.workspace.y, 9.0);
        assert_eq!(layout.workspace.height, WORKSPACE_HEIGHT);
        assert_eq!(layout.workspace_max, layout.search.x - GRID_GAP - 80.0);
        assert_eq!(
            layout.workspace.width, layout.workspace_max,
            "длинное имя упирается"
        );
        assert!(!layout.workspace.intersects(&layout.search));
        let short = ChromeLayout::compute(1180.0, 40.0, Some(67.5), 90.0);
        assert_eq!(short.workspace.width, 90.0);
        let fullscreen = ChromeLayout::compute(1180.0, 40.0, None, 90.0);
        assert_eq!(
            fullscreen.workspace.x, PAD_LEFT,
            "без кнопок окна — от отступа"
        );
    }

    #[test]
    fn workspace_button_wraps_the_name_and_the_chevron_with_prototype_padding() {
        // Паспорт прототипа: название 45,03125 → кнопка 68,03125, шеврон с 54,03125.
        let text = 45.03125;
        let width = workspace_width(text);
        assert_eq!(width, 68.03125, "поля 6 и 3, промежуток 3, шеврон 11");
        let parts = workspace_parts(width, 16.0);
        assert_eq!(parts.label, Frame::new(6.0, 3.0, text, 16.0));
        assert_eq!(parts.chevron, Frame::new(54.03125, 5.5, 11.0, 11.0));
        assert_eq!(parts.chevron.right() + WORKSPACE_PAD_RIGHT, width);
        assert_eq!(
            parts.label.y + parts.label.height / 2.0,
            WORKSPACE_HEIGHT / 2.0,
            "название по середине высоты"
        );
    }

    #[test]
    fn narrow_workspace_button_shrinks_the_name_and_keeps_the_chevron() {
        let narrow = workspace_parts(40.0, 16.0);
        assert_eq!(narrow.label.width, 17.0);
        assert_eq!(narrow.chevron.x, 26.0, "шеврон сразу за названием");
        assert_eq!(narrow.chevron.right() + WORKSPACE_PAD_RIGHT, 40.0);
        let tiny = workspace_parts(10.0, 16.0);
        assert_eq!(tiny.label.width, 0.0);
        assert_eq!(tiny.chevron.width, CHEVRON_ICON);
        assert_eq!(workspace_width(-5.0), workspace_width(0.0));
    }

    #[test]
    fn right_cluster_ends_at_padding_without_overlaps() {
        for width in [1180.0, 1512.0] {
            let layout = ChromeLayout::compute(width, 40.0, Some(68.0), 100.0);
            assert_eq!(layout.appearance.right(), width - PAD_RIGHT);
            assert_eq!(layout.appearance.width, ICON_WIDTH);
            assert_eq!(layout.zones.right() + RIGHT_GAP, layout.appearance.x);
            assert_eq!(layout.zones.width, 92.0);
            assert_eq!(layout.zones.height, 26.0);
            assert!(!layout.zones.intersects(&layout.search));
            assert!(layout.zones.x >= layout.search.right() + GRID_GAP);
            let mut previous: Option<Frame> = None;
            for button in layout.zone_buttons {
                assert!(button.right() <= layout.zones.width - SEG_BORDER);
                assert_eq!((button.y, button.height), (SEG_BORDER, ZONE_HEIGHT));
                if let Some(previous) = previous {
                    assert_eq!(
                        button.x,
                        previous.right(),
                        "разделитель внутри ширины кнопки"
                    );
                }
                previous = Some(button);
            }
        }
    }

    #[test]
    fn name_that_fits_is_kept() {
        assert_eq!(shorten("Работа", 200.0, measure), "Работа");
        assert_eq!(shorten("", 200.0, measure), "");
    }

    #[test]
    fn long_name_is_shortened_to_the_widest_fitting_prefix() {
        let name = "Очень длинное имя воркспейса";
        let max = 120.0;
        let shown = shorten(name, max, measure);
        assert!(shown.ends_with(ELLIPSIS), "{shown}");
        assert!(measure(&shown) <= max, "{shown}: {}", measure(&shown));
        let prefix = shown.trim_end_matches(ELLIPSIS);
        assert!(name.starts_with(prefix), "{shown}");
        let longer: String = name.chars().take(prefix.chars().count() + 1).collect();
        assert!(
            measure(&format!("{}{ELLIPSIS}", longer.trim_end())) > max,
            "на знак длиннее уже не помещается: {shown}"
        );
        assert!(
            !prefix.ends_with(' '),
            "пробел перед сокращением убран: {shown:?}"
        );
    }

    #[test]
    fn shortening_keeps_whole_characters_and_the_chevron() {
        let shown = shorten("ёжик🦔ёжик🦔ёжик", 25.0 + 7.0 * 5.0 + 6.0, measure);
        assert_eq!(shown, "ёжик🦔…");
        assert_eq!(
            shorten("воркспейс", 10.0, measure),
            "…",
            "только знак сокращения"
        );
    }

    #[test]
    fn appearance_button_names_the_current_choice() {
        assert_eq!(
            appearance_button(AppearanceChoice::System),
            ("appearanceSystem", "Оформление: системное")
        );
        assert_eq!(
            appearance_button(AppearanceChoice::Light).1,
            "Оформление: светлое"
        );
        assert_eq!(
            appearance_button(AppearanceChoice::Dark).1,
            "Оформление: тёмное"
        );
    }

    #[test]
    fn state_defaults_without_workbench_and_until_p1_7() {
        let state = ChromeState::default();
        assert_eq!(state.workspace_text(), NO_WORKSPACE);
        assert!(
            !state.zones,
            "кнопки зон недоступны без открытой рабочей области"
        );
        assert!(!state.workspace_menu, "меню воркспейса — P1.7");
        assert_eq!(state.choice, AppearanceChoice::System);
        let named = ChromeState {
            workspace: Some("Работа".to_owned()),
            ..ChromeState::default()
        };
        assert_eq!(named.workspace_text(), "Работа");
        assert_eq!(workspace_label("Очень длинное"), "Воркспейс: Очень длинное");
    }

    #[test]
    fn zone_buttons_are_pressed_only_when_available_and_shown() {
        let mut state = ChromeState {
            shown: [true, false, true],
            ..ChromeState::default()
        };
        assert_eq!(
            zone_pressed(&state),
            [false; ZONE_COUNT],
            "недоступные не нажаты"
        );
        state.zones = true;
        assert_eq!(zone_pressed(&state), [true, false, true]);
        state.side_plate = true;
        state.shown = [false, true, false];
        assert_eq!(zone_pressed(&state), [false, true, false]);
    }

    #[test]
    fn zone_buttons_keep_their_labels_until_the_third_targets_the_plate() {
        let labels = |side_plate| ZoneButton::ALL.map(|button| zone_button(button, side_plate).1);
        assert_eq!(labels(false), ["Навигация", "Нижняя зона", "Правая зона"]);
        assert_eq!(labels(true), ["Навигация", "Нижняя зона", "Левая зона"]);
        assert_ne!(
            zone_button(ZoneButton::Side, true).0,
            zone_button(ZoneButton::Side, false).0,
            "значок третьей кнопки меняется вместе с подписью"
        );
        let state = ChromeState::default();
        assert!(!state.side_plate && state.shown == [false; ZONE_COUNT]);
    }
}
