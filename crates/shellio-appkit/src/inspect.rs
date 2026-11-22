//! Чтения настоящих видов для `ui.window` и `ui.inspect`: кадр в точках окна, видимость, first
//! responder, AX-роль, имя и подсказка, ключи анимаций у слоёв, найденные классы материала и
//! экраны. Чтения ничего не меняют.

use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::{NSAccessibility, NSScreen, NSView, NSWindow};
use objc2_quartz_core::CALayer;

use crate::chrome_bar::{ChromeState, Frame};
use crate::components::menu::MenuState;
use crate::components::text_field::TextFieldState;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeState {
    Chrome(ChromeState),
    /// Поле ввода: состояние, которое показывает вид, и идёт ли ввод.
    TextField {
        state: TextFieldState,
        editing: bool,
    },
    /// Меню кнопки: пункты, открыто ли, подсвеченный пункт.
    Menu(MenuState),
}

/// Компонент или его часть в дереве `ui.inspect`.
#[derive(Debug, Clone, PartialEq)]
pub struct InspectNode {
    /// Устойчивое имя в окне.
    pub id: &'static str,
    pub class: &'static str,
    /// Адрес объекта вида: идентичность в процессе.
    pub object: u64,
    pub parent: Option<&'static str>,
    pub facts: ViewFacts,
    pub state: Option<NodeState>,
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
        role: text(view.accessibilityRole()),
        label,
        help: text(view.accessibilityHelp()),
        selected: None,
        expanded: None,
        animation_keys: view.layer().map_or(0, |layer| animation_keys(&layer)),
    }
}

/// Узел дерева по виду.
pub(crate) fn node(
    id: &'static str,
    class: &'static str,
    parent: Option<&'static str>,
    view: &NSView,
    enabled: Option<bool>,
    state: Option<NodeState>,
) -> InspectNode {
    InspectNode {
        id,
        class,
        object: object_id(view),
        parent,
        facts: facts(view, enabled),
        state,
    }
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
