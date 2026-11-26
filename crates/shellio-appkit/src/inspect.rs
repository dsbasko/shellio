//! Чтения настоящих видов для `ui.window` и `ui.inspect`: кадр в точках окна, видимость, first
//! responder, AX-роль, имя и подсказка, ключи анимаций у слоёв, найденные классы материала и
//! экраны. Чтения ничего не меняют.

use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::{
    NSAccessibility, NSButton, NSControl, NSScreen, NSTextField, NSView, NSWindow,
};
use objc2_quartz_core::CALayer;

use crate::chrome_bar::{ChromeState, Frame};
use crate::components::menu::MenuState;
use crate::components::tab_strip::TabStripFacts;
use crate::components::text_field::TextFieldState;
use crate::components::tree_list::TreeListFacts;
use crate::components::virtual_list::VirtualListFacts;
use crate::style::{self, Style};

/// Ключи сверки ADR-0037 узлов заголовка и строки статуса. Таблица шаблонов — в
/// `shellio-conformance`; тест продукта сверяет с ней каждый ключ этого списка.
pub mod ck {
    pub const CHROME: &str = "chrome";
    pub const WORKSPACE: &str = "chrome.workspace";
    pub const SEARCH: &str = "chrome.search";
    pub const SEARCH_ICON: &str = "chrome.search.icon";
    pub const SEARCH_LABEL: &str = "chrome.search.label";
    pub const SEARCH_KBD: &str = "chrome.search.kbd";
    pub const ZONES: &str = "chrome.zones";
    /// Кнопки зон слева направо: третья — `right`, что бы она ни переключала.
    pub const ZONE_BUTTONS: [&str; 3] = [
        "chrome.zones.left",
        "chrome.zones.bottom",
        "chrome.zones.right",
    ];
    /// Системные кнопки окна слева направо.
    pub const WINDOW_BUTTONS: [&str; 3] = [
        "chrome.window.close",
        "chrome.window.minimize",
        "chrome.window.zoom",
    ];
    pub const APPEARANCE: &str = "chrome.appearance";
    pub const WORKSPACE_LABEL: &str = "chrome.workspace.label";
    pub const WORKSPACE_CHEVRON: &str = "chrome.workspace.chevron";
    /// Значки кнопок зон в порядке [`ZONE_BUTTONS`].
    pub const ZONE_ICONS: [&str; 3] = [
        "chrome.zones.left.icon",
        "chrome.zones.bottom.icon",
        "chrome.zones.right.icon",
    ];
    pub const APPEARANCE_ICON: &str = "chrome.appearance.icon";
    /// Строка статуса окна. Ключ назначает окно: тот же компонент рисует подписи в телах панелей.
    pub const STATUS: &str = "status";
    pub const STATUS_MESSAGE: &str = "status.message";
    /// Плашка режима строки статуса. Ключи подсказок — `status.hint.{n}` с `.key.{m}` и `.label`.
    pub const STATUS_MODE: &str = "status.mode";

    /// Все ключи, которые назначают заголовок и строка статуса.
    pub const ALL: [&str; 23] = [
        CHROME,
        WORKSPACE,
        SEARCH,
        SEARCH_ICON,
        SEARCH_LABEL,
        SEARCH_KBD,
        ZONES,
        ZONE_BUTTONS[0],
        ZONE_BUTTONS[1],
        ZONE_BUTTONS[2],
        APPEARANCE,
        STATUS,
        STATUS_MESSAGE,
        STATUS_MODE,
        WINDOW_BUTTONS[0],
        WINDOW_BUTTONS[1],
        WINDOW_BUTTONS[2],
        WORKSPACE_LABEL,
        WORKSPACE_CHEVRON,
        ZONE_ICONS[0],
        ZONE_ICONS[1],
        ZONE_ICONS[2],
        APPEARANCE_ICON,
    ];
}

/// Факты одного вида.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewFacts {
    /// Кадр в точках окна, начало внизу слева.
    pub frame: Frame,
    /// Вид и его предки не скрыты, окно видимо.
    pub visible: bool,
    pub first_responder: bool,
    /// Доступность контрола; `None` — не контрол.
    pub enabled: Option<bool>,
    pub role: Option<String>,
    pub label: Option<String>,
    pub help: Option<String>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    /// Ключи анимаций у слоя вида и всех его подслоёв.
    pub animation_keys: u32,
}

/// Состояние компонента по классу.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeState {
    Chrome(ChromeState),
    /// Поле ввода: состояние, которое показывает вид, и идёт ли ввод.
    TextField {
        state: TextFieldState,
        editing: bool,
    },
    /// Меню кнопки: пункты, открыто ли, подсвеченный пункт.
    Menu(MenuState),
    /// Полоса вкладок: состояние, режим по ширине, прокрутка и факты частей.
    TabStrip(Box<TabStripFacts>),
    /// Список: состояние, прокрутка, видимые строки и созданные виды строк.
    VirtualList(Box<VirtualListFacts>),
    /// Дерево: выбор и раскрытие у `NSOutlineView`, прокрутка, видимые строки и виды строк.
    TreeList(Box<TreeListFacts>),
}

/// Компонент или его часть в дереве `ui.inspect`.
#[derive(Debug, Clone, PartialEq)]
pub struct InspectNode {
    /// Устойчивое имя в окне.
    pub id: String,
    pub class: &'static str,
    /// Адрес объекта вида: идентичность в процессе.
    pub object: u64,
    pub parent: Option<String>,
    /// Ключ сверки ADR-0037: его назначает компонент по месту узла в сцене.
    pub ck: Option<String>,
    pub facts: ViewFacts,
    /// Свойства вида, прочитанные у него и его слоя; `None` — у вида их нет.
    pub style: Option<Style>,
    /// Обрезан ли текст узла; `None` — текста у узла нет.
    pub truncated: Option<bool>,
    /// Заливка, которую видно под видом: его слой, слои предков и фон окна. `None` — непрозрачной
    /// заливки под видом нет.
    pub under: Option<style::Rgba>,
    /// Заливка, которую видно за видом: слои предков и фон окна без его собственного слоя.
    pub behind: Option<style::Rgba>,
    pub state: Option<NodeState>,
}

impl InspectNode {
    /// Узел с ключом сверки.
    #[must_use]
    pub fn with_ck(mut self, ck: impl Into<String>) -> InspectNode {
        self.ck = Some(ck.into());
        self
    }

    /// Ключ корня поверхности (узел без родителя с ключом): заливка такого узла — та, что видна
    /// под видом, а не прозрачная (ADR-0037, правило фона паспорта).
    #[must_use]
    pub fn with_root_ck(mut self, ck: impl Into<String>) -> InspectNode {
        self.ck = Some(ck.into());
        if let Some(fill) = self.under {
            self.style.get_or_insert_with(Style::default).fill = Some(fill);
        }
        self
    }

    /// Линия, нарисованная отдельным слоем у сторон `sides`: цвет — заливка слоя, толщина — его
    /// меньшая сторона.
    #[must_use]
    pub(crate) fn with_line_layer(mut self, line: &CALayer, sides: &[style::Side]) -> InspectNode {
        let drawn = style::layer_style(line).fill;
        let size = line.bounds().size;
        if let Some(color) = drawn {
            let style = self.style.get_or_insert_with(Style::default);
            style.line_color = Some(color);
            style.line_width = Some(size.width.min(size.height));
            style.line_sides = sides.to_vec();
        }
        self
    }

    /// Линия у стороны `side`, нарисованная видом-полосой `line` со слоем.
    #[must_use]
    pub(crate) fn with_line(self, line: &NSView, side: style::Side) -> InspectNode {
        match line.layer() {
            Some(layer) => self.with_line_layer(&layer, &[side]),
            None => self,
        }
    }

    /// Линия по краю вида, нарисованная отдельным слоем `ring` с линией слоя: цвет и толщина —
    /// его, стороны — все четыре.
    #[must_use]
    pub(crate) fn with_ring_layer(mut self, ring: &CALayer) -> InspectNode {
        let line = style::layer_style(ring);
        if line.line_color.is_some() {
            let style = self.style.get_or_insert_with(Style::default);
            style.line_color = line.line_color;
            style.line_width = line.line_width;
            style.line_sides = line.line_sides;
        }
        self
    }

    /// Тени вида, нарисованные отдельными слоями под ним, по порядку записи `box-shadow`: тени
    /// слоёв `shadows`, затем кольцо снаружи кадра — линия слоя `ring`. Слой, который ничего не
    /// рисует, записи не даёт.
    #[must_use]
    pub(crate) fn with_shadow_layers(
        mut self,
        shadows: &[Retained<CALayer>],
        ring: &CALayer,
    ) -> InspectNode {
        let drawn: Vec<_> = shadows
            .iter()
            .filter_map(|layer| style::layer_shadow(layer))
            .chain(style::ring_shadow(ring))
            .collect();
        if !drawn.is_empty() {
            self.style.get_or_insert_with(Style::default).shadow = drawn;
        }
        self
    }

    /// Текст узла рисует вложенное поле `field`: подпись, шрифт, цвет и обрезка — его.
    #[must_use]
    pub(crate) fn with_text_of(mut self, field: &NSTextField) -> InspectNode {
        let text = style::view_style(field);
        let style = self.style.get_or_insert_with(Style::default);
        style.font = text.font;
        style.line_height = text.line_height;
        style.letter_spacing = text.letter_spacing;
        style.color = text.color;
        self.facts.label = Some(field.stringValue().to_string());
        self.truncated = Some(style::truncated(field));
        self
    }

    /// Значок узла: имя в наборе и сторона в точках; цвет — оттенок вида значка.
    #[must_use]
    pub(crate) fn with_icon(
        mut self,
        name: &str,
        size: f64,
        color: Option<style::Rgba>,
    ) -> InspectNode {
        let style = self.style.get_or_insert_with(Style::default);
        style.icon = Some(style::IconFacts {
            name: name.to_owned(),
            size,
        });
        if color.is_some() {
            style.color = color;
        }
        self
    }
}

/// Заливка, которую видно под видом: его слой, слои предков и фон окна.
pub(crate) fn seen_fill(view: &NSView) -> Option<style::Rgba> {
    let mut chain = Vec::new();
    let mut current = Some(view.retain());
    while let Some(view) = current {
        if let Some(fill) = view
            .layer()
            .and_then(|layer| style::layer_style(&layer).fill)
        {
            chain.push(fill);
        }
        // SAFETY: `superview` у живого вида на главном потоке.
        current = unsafe { view.superview() };
    }
    if let Some(fill) = view
        .window()
        .and_then(|window| style::rgba(&window.backgroundColor()))
    {
        chain.push(fill);
    }
    style::seen_fill(&chain)
}

/// Заливка, которую видно за видом: то, что видно под его родителем.
pub(crate) fn seen_behind(view: &NSView) -> Option<style::Rgba> {
    // SAFETY: `superview` у живого вида на главном потоке.
    let parent = unsafe { view.superview() }?;
    seen_fill(&parent)
}

/// Вид или слой материала с прозрачностью.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterialFact {
    pub class: String,
    pub visible: bool,
    /// Внутри собственного содержимого окна.
    pub own: bool,
}

/// Видимое дерево компонентов окна.
#[derive(Debug, Clone, PartialEq)]
pub struct Inspection {
    /// Размер содержимого окна в точках.
    pub content: (f64, f64),
    pub nodes: Vec<InspectNode>,
    pub materials: Vec<MaterialFact>,
}

/// Экран.
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenFacts {
    pub name: String,
    pub frame: Frame,
    pub backing_scale_factor: f64,
    pub maximum_frames_per_second: i64,
}

/// Окно владельца.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowFacts {
    pub number: i64,
    /// Кадр в точках экрана.
    pub frame: Frame,
    pub backing_scale_factor: f64,
    pub visible: bool,
    pub key: bool,
    pub color_space: Option<String>,
    pub color_space_srgb: bool,
    pub title: String,
    /// Номер экрана окна в [`Self::screens`].
    pub screen: Option<usize>,
    pub screens: Vec<ScreenFacts>,
}

/// Окно владельца в списке окон `ui.window`.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedWindowFacts {
    pub number: i64,
    /// Кадр `NSWindow.frame` в точках AppKit, начало внизу слева основного экрана.
    pub frame: Frame,
    pub backing_scale_factor: f64,
    pub visible: bool,
    pub key: bool,
    /// Номер экрана окна в `NSScreen.screens`.
    pub screen: Option<usize>,
}

pub(crate) fn owned_window_facts(window: &NSWindow, mtm: MainThreadMarker) -> OwnedWindowFacts {
    OwnedWindowFacts {
        number: i64::try_from(window.windowNumber()).unwrap_or(-1),
        frame: frame_of(window.frame()),
        backing_scale_factor: window.backingScaleFactor(),
        visible: window.isVisible(),
        key: window.isKeyWindow(),
        screen: screen_index(window, mtm),
    }
}

/// Кадр `frame` в системе координат окна `child` (начало внизу слева его кадра) — в системе
/// координат окна `parent`. Кадры окон — в точках экрана AppKit; у окна владельца содержимое
/// занимает весь кадр (`FullSizeContentView`), поэтому его координаты окна — координаты
/// содержимого, и высота содержимого переворачивает результат в строки снимка.
#[must_use]
pub fn into_parent(frame: Frame, child: Frame, parent: Frame) -> Frame {
    Frame::new(
        frame.x + child.x - parent.x,
        frame.y + child.y - parent.y,
        frame.width,
        frame.height,
    )
}

/// Имена классов материала с прозрачностью и размытием (ADR-0023).
const MATERIAL_CLASSES: [&str; 2] = ["NSVisualEffectView", "NSGlassEffectView"];

pub(crate) fn frame_of(rect: objc2_foundation::NSRect) -> Frame {
    Frame::new(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    )
}

/// Адрес объекта как его идентичность.
pub(crate) fn object_id<T: Message>(object: &T) -> u64 {
    std::ptr::from_ref(object) as usize as u64
}

pub(crate) fn text(value: Option<Retained<objc2_foundation::NSString>>) -> Option<String> {
    value
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
}

/// Факты вида `view`. `enabled`, `selected` и `expanded` даёт компонент: у вида без них этих
/// свойств нет.
pub(crate) fn facts(view: &NSView, enabled: Option<bool>) -> ViewFacts {
    let window = view.window();
    let frame = frame_of(view.convertRect_toView(view.bounds(), None));
    let visible =
        !view.isHiddenOrHasHiddenAncestor() && window.as_ref().is_some_and(|w| w.isVisible());
    let first_responder = window
        .as_ref()
        .and_then(|window| window.firstResponder())
        .is_some_and(|responder| {
            std::ptr::eq(
                std::ptr::from_ref::<objc2_app_kit::NSResponder>(&responder).cast::<u8>(),
                std::ptr::from_ref(view).cast::<u8>(),
            )
        });
    let label = text(view.accessibilityLabel()).or_else(|| text(view.accessibilityTitle()));
    ViewFacts {
        frame,
        visible,
        first_responder,
        enabled,
        role: role(view),
        label,
        help: text(view.accessibilityHelp()),
        selected: None,
        expanded: None,
        animation_keys: view.layer().map_or(0, |layer| animation_keys(&layer)),
    }
}

/// AX-роль вида, как её видит клиент доступности. Контрол отдаёт клиентам роль своей ячейки, а
/// собственная роль вида у него `AXUnknown` (находка 2 P0.2 для поля, у кнопки так же).
fn role(view: &NSView) -> Option<String> {
    let own = text(view.accessibilityRole());
    if own.as_deref().is_some_and(|role| role != UNKNOWN_ROLE) {
        return own;
    }
    view.downcast_ref::<NSControl>()
        .and_then(NSControl::cell)
        .and_then(|cell| text(cell.accessibilityRole()))
        .or(own)
}

const UNKNOWN_ROLE: &str = "AXUnknown";

/// Полный текст поля-подписи без имени доступности: его несёт строка поля. Поле ввода имя не
/// получает: его строка — значение, а не подпись.
fn with_field_text(mut facts: ViewFacts, view: &NSView) -> ViewFacts {
    if facts.label.is_none()
        && let Some(field) = view.downcast_ref::<NSTextField>()
        && !field.isEditable()
    {
        facts.label = text(Some(field.stringValue()));
    }
    facts
}

/// Узел дерева по виду.
pub(crate) fn node(
    id: &str,
    class: &'static str,
    parent: Option<&str>,
    view: &NSView,
    enabled: Option<bool>,
    state: Option<NodeState>,
) -> InspectNode {
    InspectNode {
        id: id.to_owned(),
        class,
        object: object_id(view),
        parent: parent.map(str::to_owned),
        ck: None,
        facts: with_field_text(facts(view, enabled), view),
        style: Some(style::view_style(view)).filter(|style| !style.is_empty()),
        truncated: view.downcast_ref::<NSTextField>().map(style::truncated),
        under: seen_fill(view),
        behind: seen_behind(view),
        state,
    }
}

/// Узел прямоугольника без собственного вида: кадр `frame` — в точках окна, свойств и состояния
/// нет.
pub(crate) fn rect_node(
    id: &str,
    class: &'static str,
    parent: &str,
    object: u64,
    frame: Frame,
) -> InspectNode {
    InspectNode {
        id: id.to_owned(),
        class,
        object,
        parent: Some(parent.to_owned()),
        ck: None,
        facts: ViewFacts {
            frame,
            visible: frame.width > 0.0 && frame.height > 0.0,
            first_responder: false,
            enabled: None,
            role: None,
            label: None,
            help: None,
            selected: None,
            expanded: None,
            animation_keys: 0,
        },
        style: None,
        truncated: None,
        under: None,
        behind: None,
        state: None,
    }
}

/// Часть кнопки, которую рисует её ячейка: у ячейки вида нет, кадр — её прямоугольник в окне.
fn button_part(
    id: &str,
    class: &'static str,
    parent: &str,
    button: &NSButton,
    rect: objc2_foundation::NSRect,
    object: u64,
) -> InspectNode {
    let whole = facts(button, None);
    InspectNode {
        id: id.to_owned(),
        class,
        object,
        parent: Some(parent.to_owned()),
        ck: None,
        facts: ViewFacts {
            frame: frame_of(button.convertRect_toView(rect, None)),
            visible: whole.visible && rect.size.width > 0.0 && rect.size.height > 0.0,
            first_responder: false,
            enabled: None,
            role: None,
            label: None,
            help: None,
            selected: None,
            expanded: None,
            animation_keys: 0,
        },
        style: None,
        truncated: None,
        under: None,
        behind: None,
        state: None,
    }
}

/// Подпись, поле которой шире набора на поля ячейки с обеих сторон ([`style::LABEL_INSET`]): кадр
/// узла — место набора, а не кадр поля. Так поле ставит компонент, которому важно начало набора.
pub(crate) fn typeset_node(
    id: &str,
    class: &'static str,
    parent: &str,
    field: &NSTextField,
) -> InspectNode {
    let mut node = node(id, class, Some(parent), field, None, None);
    let bounds = field.bounds();
    let typeset = objc2_foundation::NSRect::new(
        objc2_foundation::NSPoint::new(bounds.origin.x + style::LABEL_INSET, bounds.origin.y),
        objc2_foundation::NSSize::new(
            (bounds.size.width - 2.0 * style::LABEL_INSET).max(0.0),
            bounds.size.height,
        ),
    );
    node.facts.frame = frame_of(field.convertRect_toView(typeset, None));
    if !style::wraps(field) {
        node.truncated = Some(field.intrinsicContentSize().width > typeset.size.width + 0.5);
    }
    node
}

/// Значок кнопки: прямоугольник картинки её ячейки (`imageRectForBounds:`) и сторона картинки.
/// `name` — имя значка, которое компонент поставил кнопке. Шаблонную картинку красит
/// `contentTintColor`; цветная несёт цвет в себе, и свойством вида он не читается.
pub(crate) fn button_image(
    id: &str,
    parent: &str,
    button: &NSButton,
    name: &str,
) -> Option<InspectNode> {
    let image = button.image()?;
    let cell = button.cell()?;
    let rect = cell.imageRectForBounds(button.bounds());
    let color = button
        .contentTintColor()
        .filter(|_| image.isTemplate())
        .and_then(|color| style::rgba(&color));
    Some(
        button_part(id, "ButtonIcon", parent, button, rect, object_id(&*image)).with_icon(
            name,
            image.size().width,
            color,
        ),
    )
}

/// Ключи анимаций у слоя и всех его подслоёв.
pub(crate) fn animation_keys(layer: &CALayer) -> u32 {
    let own = layer
        .animationKeys()
        .map_or(0, |keys| u32::try_from(keys.count()).unwrap_or(u32::MAX));
    // SAFETY: `sublayers` у живого слоя на главном потоке; массив читается сразу и не меняется.
    let children = unsafe { layer.sublayers() };
    children.map_or(own, |children| {
        children
            .iter()
            .fold(own, |sum, child| sum.saturating_add(animation_keys(&child)))
    })
}

/// Цепочка имён классов объекта от него к корню.
fn class_chain(class: &AnyClass) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = Some(class);
    while let Some(class) = current {
        names.push(class.name().to_string_lossy().into_owned());
        current = class.superclass();
    }
    names
}

fn material_name(class: &AnyClass) -> Option<String> {
    let chain = class_chain(class);
    let own = chain.first().cloned()?;
    let material = chain
        .iter()
        .any(|name| MATERIAL_CLASSES.contains(&name.as_str()) || name.contains("Backdrop"));
    material.then_some(own)
}

/// Виды и слои материала во всём окне, включая системный контейнер заголовка. `root` — корень
/// собственного содержимого окна.
pub(crate) fn materials(window: &NSWindow, root: &NSView) -> Vec<MaterialFact> {
    let mut found = Vec::new();
    let Some(content) = window.contentView() else {
        return found;
    };
    // SAFETY: `superview` у вида содержимого, которое держит окно.
    let top = unsafe { content.superview() }.unwrap_or(content);
    let window_visible = window.isVisible();
    walk_views(&top, &mut |view| {
        let visible =
            window_visible && !view.isHiddenOrHasHiddenAncestor() && view.alphaValue() > 0.0;
        let own = view.isDescendantOf(root);
        if let Some(class) = material_name(view.class()) {
            found.push(MaterialFact {
                class,
                visible,
                own,
            });
        }
        if let Some(layer) = view.layer() {
            walk_layers(&layer, &mut |layer| {
                if let Some(class) = material_name(layer.class()) {
                    found.push(MaterialFact {
                        class,
                        visible: visible && !layer.isHidden() && layer.opacity() > 0.0,
                        own,
                    });
                }
            });
        }
    });
    found
}

fn walk_views(view: &NSView, visit: &mut impl FnMut(&NSView)) {
    visit(view);
    for child in view.subviews().iter() {
        walk_views(&child, visit);
    }
}

/// Подслои слоя без слоёв подвидов: их обходит [`walk_views`] со своим видом.
fn walk_layers(layer: &CALayer, visit: &mut impl FnMut(&CALayer)) {
    // SAFETY: `sublayers` у живого слоя на главном потоке; массив читается сразу.
    let Some(children) = (unsafe { layer.sublayers() }) else {
        return;
    };
    for child in children.iter() {
        if child.delegate().is_some() {
            continue;
        }
        visit(&child);
        walk_layers(&child, visit);
    }
}

/// Экраны в порядке `NSScreen.screens`.
pub(crate) fn screens(mtm: MainThreadMarker) -> Vec<ScreenFacts> {
    NSScreen::screens(mtm)
        .iter()
        .map(|screen| ScreenFacts {
            name: screen.localizedName().to_string(),
            frame: frame_of(screen.frame()),
            backing_scale_factor: screen.backingScaleFactor(),
            maximum_frames_per_second: i64::try_from(screen.maximumFramesPerSecond()).unwrap_or(-1),
        })
        .collect()
}

/// Номер экрана окна в `NSScreen.screens`.
pub(crate) fn screen_index(window: &NSWindow, mtm: MainThreadMarker) -> Option<usize> {
    let screen = window.screen()?;
    NSScreen::screens(mtm)
        .iter()
        .position(|candidate| std::ptr::eq(&*candidate, &*screen))
}

/// Путь смены состояния компонента: с анимацией или без.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionPath {
    Animated,
    Instant,
}

/// Смена состояния компонента для журнала анимаций `ui.stats`: путь и число анимаций, которых не
/// было у слоёв компонента до смены.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Transition {
    pub path: TransitionPath,
    pub added: u32,
}

impl Transition {
    /// Смена без анимаций.
    pub const INSTANT: Transition = Transition {
        path: TransitionPath::Instant,
        added: 0,
    };
}

/// Анимации у слоя и всех его подслоёв: адрес слоя, ключ и адрес объекта анимации.
pub(crate) fn animation_set(layer: &CALayer) -> Vec<(usize, String, usize)> {
    let mut found = Vec::new();
    collect_animations(layer, &mut found);
    found
}

fn collect_animations(layer: &CALayer, found: &mut Vec<(usize, String, usize)>) {
    if let Some(keys) = layer.animationKeys() {
        for key in keys.iter() {
            // SAFETY: ключ взят из `animationKeys` того же слоя на главном потоке, результат
            // читается сразу.
            let animation = unsafe { layer.animationForKey(&key) };
            let address = animation.map_or(0, |animation| object_id(&*animation) as usize);
            found.push((object_id(layer) as usize, key.to_string(), address));
        }
    }
    // SAFETY: `sublayers` у живого слоя на главном потоке; массив читается сразу.
    if let Some(children) = unsafe { layer.sublayers() } {
        for child in children.iter() {
            collect_animations(&child, found);
        }
    }
}

/// Сколько анимаций из `after` не было в `before`: новая анимация под прежним ключом тоже новая.
pub(crate) fn added_animations(
    before: &[(usize, String, usize)],
    after: &[(usize, String, usize)],
) -> u32 {
    let added = after.iter().filter(|entry| !before.contains(entry)).count();
    u32::try_from(added).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Палитра над окном 1180×720 в (370, 240) с полем тени 66: плашка `palette` паспорта
    /// `palette-groups` (270, 46, 640×560) лежит в окне палитры в (66, 66).
    #[test]
    fn into_parent_moves_child_frame_into_owner_window() {
        let owner = Frame::new(370.0, 240.0, 1180.0, 720.0);
        let plate = Frame::new(270.0, 46.0, 640.0, 560.0);
        let margin = 66.0;
        let child = Frame::new(
            owner.x + plate.x - margin,
            owner.y + owner.height - plate.y - plate.height - margin,
            plate.width + 2.0 * margin,
            plate.height + 2.0 * margin,
        );
        let local = Frame::new(margin, margin, plate.width, plate.height);
        let shifted = into_parent(local, child, owner);
        assert_eq!(
            shifted,
            Frame::new(
                plate.x,
                owner.height - plate.y - plate.height,
                plate.width,
                plate.height
            )
        );
        // Переворот по высоте содержимого окна владельца даёт строки паспорта.
        assert_eq!(owner.height - shifted.y - shifted.height, plate.y);
        // Окно на другом месте экрана кадр в окне владельца не меняет.
        let moved = Frame::new(child.x - 500.0, child.y + 30.0, child.width, child.height);
        let owner_moved = Frame::new(owner.x - 500.0, owner.y + 30.0, owner.width, owner.height);
        assert_eq!(into_parent(local, moved, owner_moved), shifted);
    }

    fn bare(style: Option<Style>) -> InspectNode {
        InspectNode {
            id: "tab".to_owned(),
            class: "Tab",
            object: 0,
            parent: None,
            ck: None,
            facts: ViewFacts {
                frame: Frame::new(0.0, 0.0, 120.0, 26.0),
                visible: true,
                first_responder: false,
                enabled: None,
                role: None,
                label: None,
                help: None,
                selected: None,
                expanded: None,
                animation_keys: 0,
            },
            style,
            truncated: None,
            under: None,
            behind: None,
            state: None,
        }
    }

    #[test]
    fn line_layer_gives_the_node_a_line_at_the_named_sides() {
        let line = CALayer::new();
        line.setFrame(objc2_foundation::NSRect::new(
            objc2_foundation::NSPoint::ZERO,
            objc2_foundation::NSSize::new(1180.0, 1.0),
        ));
        assert_eq!(
            bare(None).with_line_layer(&line, &[style::Side::Top]).style,
            None,
            "слой без заливки линии не рисует"
        );

        crate::layer::set_fill(&line, shellio_theme::Srgb8::new(48, 46, 43));
        let style = bare(None)
            .with_line_layer(&line, &[style::Side::Top])
            .style
            .unwrap();
        assert_eq!(
            style.line_color,
            Some(style::Rgba {
                r: 48,
                g: 46,
                b: 43,
                a: 1.0
            })
        );
        assert_eq!(
            style.line_width,
            Some(1.0),
            "толщина — меньшая сторона слоя"
        );
        assert_eq!(style.line_sides, [style::Side::Top]);
        assert_eq!(style.fill, None, "заливка слоя линии — не заливка узла");
    }

    #[test]
    fn ring_layer_gives_the_node_its_line_on_four_sides() {
        let ring = CALayer::new();
        assert_eq!(
            bare(None).with_ring_layer(&ring).style,
            None,
            "слой без линии узлу ничего не даёт"
        );

        crate::layer::set_line(&ring, Some((shellio_theme::Srgb8::new(64, 61, 57), 1.0)));
        let style = bare(None).with_ring_layer(&ring).style.unwrap();
        assert_eq!(
            style.line_color,
            Some(style::Rgba {
                r: 64,
                g: 61,
                b: 57,
                a: 1.0
            })
        );
        assert_eq!(style.line_width, Some(1.0));
        assert_eq!(style.line_sides, style::Side::ALL);
        assert_eq!(style.fill, None);

        let filled = Style {
            fill: Some(style::Rgba {
                r: 10,
                g: 9,
                b: 8,
                a: 1.0,
            }),
            ..Style::default()
        };
        let kept = bare(Some(filled.clone()))
            .with_ring_layer(&ring)
            .style
            .unwrap();
        assert_eq!(kept.fill, filled.fill, "кольцо не трогает заливку узла");
        assert_eq!(kept.line_width, Some(1.0));

        crate::layer::set_line(&ring, None);
        assert_eq!(
            bare(Some(filled.clone())).with_ring_layer(&ring).style,
            Some(filled),
            "снятая линия кольца узел не меняет"
        );
    }
}
