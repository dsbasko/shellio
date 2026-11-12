//! Чтение фактического состояния AppKit: иерархия видов и слоёв всех окон, first responder с
//! разрешением field editor до логического поля, атрибуты доступности, геометрия в точках,
//! цвет и ключи анимаций слоя, окружение процесса. Снимок модели пробы проверкой не считается,
//! поэтому всё читается у AppKit.

use std::collections::{BTreeMap, HashSet};
use std::ffi::CString;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{msg_send, sel};
use objc2_app_kit::{NSApplication, NSColor, NSColorSpace, NSScreen, NSText, NSView, NSWindow};
use objc2_core_foundation::CGRect;
use objc2_core_graphics::{CGColor, CGDisplayIsBuiltin};
use objc2_foundation::{NSArray, NSProcessInfo, NSRect, NSSize, NSString};
use objc2_quartz_core::CALayer;
use serde::Serialize;

use crate::args::SceneOptions;
use crate::results::{
    Build, DisplayInfo, Environment, Hardware, Rect, SceneInfo, Size, WindowInfo,
};

pub fn rect(rect: NSRect) -> Rect {
    Rect {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

pub fn size(size: NSSize) -> Size {
    Size {
        width: size.width,
        height: size.height,
    }
}

fn cg_rect(rect: CGRect) -> Rect {
    Rect {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

pub use crate::tokens::Rgba;

/// Цвет AppKit в sRGB; `None` у цвета без RGB-представления, например шаблона.
pub fn srgb(color: &NSColor) -> Option<Rgba> {
    let color = color.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
    Some(Rgba::from_unit(
        color.redComponent(),
        color.greenComponent(),
        color.blueComponent(),
        color.alphaComponent(),
    ))
}

pub fn cg_srgb(color: &CGColor) -> Option<Rgba> {
    let color = NSColor::colorWithCGColor(color)?;
    srgb(&color)
}

/// Слой: собственные свойства и подслои, которые не принадлежат видам.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayerNode {
    pub class: String,
    /// Кадр в координатах надслоя, в точках.
    pub frame: Rect,
    pub hidden: bool,
    pub opacity: f32,
    pub background: Option<Rgba>,
    pub border_width: f64,
    pub border: Option<Rgba>,
    pub corner_radius: f64,
    pub animation_keys: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sublayers: Vec<LayerNode>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ViewNode {
    pub class: String,
    /// Кадр в координатах окна, в точках.
    pub frame_in_window: Rect,
    pub hidden: bool,
    pub hidden_or_ancestor: bool,
    pub alpha: f64,
    pub layer: Option<LayerNode>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subviews: Vec<ViewNode>,
}

impl ViewNode {
    /// Узлы дерева в глубину, начиная с этого.
    pub fn walk(&self) -> Vec<&ViewNode> {
        let mut nodes = vec![self];
        let mut index = 0;
        while index < nodes.len() {
            let node = nodes[index];
            nodes.extend(node.subviews.iter());
            index += 1;
        }
        nodes
    }

    pub fn find(&self, class: &str, frame: Rect) -> Option<&ViewNode> {
        self.walk()
            .into_iter()
            .find(|node| node.class == class && node.frame_in_window == frame)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WindowNode {
    pub number: isize,
    pub class: String,
    pub title: String,
    /// Кадр на экране, в точках.
    pub frame: Rect,
    pub visible: bool,
    pub key: bool,
    pub main: bool,
    pub alpha: f64,
    pub opaque: bool,
    pub background: Option<Rgba>,
    /// Корень иерархии: `contentView.superview`, а без него `contentView`.
    pub root: Option<ViewNode>,
}

/// Класс объекта по `-class`: KVO подменяет isa динамическим подклассом `NSKVONotifying_…`, а
/// `-class` возвращает исходный.
pub fn class_name(object: &AnyObject) -> String {
    // SAFETY: `-class` есть у каждого NSObject и возвращает класс.
    let class: &'static AnyClass = unsafe { msg_send![object, class] };
    class.name().to_string_lossy().into_owned()
}

/// Иерархия видов и слоёв всех окон приложения, включая панели.
pub fn hierarchy(app: &NSApplication) -> Vec<WindowNode> {
    app.windows()
        .iter()
        .map(|window| window_node(&window))
        .collect()
}

pub fn window_node(window: &NSWindow) -> WindowNode {
    let content = window.contentView();
    // SAFETY: `superview` у вида, который держит окно.
    let root = content
        .as_ref()
        .and_then(|content| unsafe { content.superview() })
        .or(content);
    let root = root.map(|root| {
        let mut layers = HashSet::new();
        collect_layers(&root, &mut layers);
        view_node(&root, &layers)
    });
    let object: &AnyObject = window;
    WindowNode {
        number: window.windowNumber(),
        class: class_name(object),
        title: window.title().to_string(),
        frame: rect(window.frame()),
        visible: window.isVisible(),
        key: window.isKeyWindow(),
        main: window.isMainWindow(),
        alpha: window.alphaValue(),
        opaque: window.isOpaque(),
        background: srgb(&window.backgroundColor()),
        root,
    }
}

/// Слои всех видов дерева: на них обход подслоёв останавливается, их описывает вид.
fn collect_layers(view: &NSView, layers: &mut HashSet<*const CALayer>) {
    if let Some(layer) = view.layer() {
        layers.insert(Retained::as_ptr(&layer));
    }
    for subview in view.subviews().iter() {
        collect_layers(&subview, layers);
    }
}

fn view_node(view: &NSView, view_layers: &HashSet<*const CALayer>) -> ViewNode {
    let object: &AnyObject = view;
    ViewNode {
        class: class_name(object),
        frame_in_window: frame_in_window(view),
        hidden: view.isHidden(),
        hidden_or_ancestor: view.isHiddenOrHasHiddenAncestor(),
        alpha: view.alphaValue(),
        layer: view
            .layer()
            .map(|layer| layer_node(&layer, view_layers, true)),
        subviews: view
            .subviews()
            .iter()
            .map(|subview| view_node(&subview, view_layers))
            .collect(),
    }
}

/// Свойства слоя без подслоёв.
pub fn layer_state(layer: &CALayer) -> LayerNode {
    layer_node(layer, &HashSet::new(), false)
}

fn layer_node(layer: &CALayer, view_layers: &HashSet<*const CALayer>, deep: bool) -> LayerNode {
    let object: &AnyObject = layer;
    let sublayers = if deep {
        // SAFETY: массив подслоёв читается на главном потоке, где живёт дерево слоёв.
        unsafe { layer.sublayers() }
            .map(|sublayers| {
                sublayers
                    .iter()
                    .filter(|sublayer| !view_layers.contains(&Retained::as_ptr(sublayer)))
                    .map(|sublayer| layer_node(&sublayer, view_layers, true))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    LayerNode {
        class: class_name(object),
        frame: cg_rect(layer.frame()),
        hidden: layer.isHidden(),
        opacity: layer.opacity(),
        background: layer.backgroundColor().as_deref().and_then(cg_srgb),
        border_width: layer.borderWidth(),
        border: layer.borderColor().as_deref().and_then(cg_srgb),
        corner_radius: layer.cornerRadius(),
        animation_keys: animation_keys(layer),
        sublayers,
    }
}

pub fn animation_keys(layer: &CALayer) -> Vec<String> {
    layer
        .animationKeys()
        .map(|keys| keys.iter().map(|key| key.to_string()).collect())
        .unwrap_or_default()
}

pub fn frame_in_window(view: &NSView) -> Rect {
    rect(view.convertRect_toView(view.bounds(), None))
}

pub fn frame_on_screen(view: &NSView) -> Option<Rect> {
    let window = view.window()?;
    Some(rect(window.convertRectToScreen(
        view.convertRect_toView(view.bounds(), None),
    )))
}

/// First responder окна. У field editor логическое поле — его делегат, то есть редактируемый
/// элемент управления.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Responder {
    pub class: String,
    pub field_editor: bool,
    pub logical_class: String,
}

pub fn first_responder(window: &NSWindow) -> Option<Responder> {
    let responder = window.firstResponder()?;
    let object: &AnyObject = &responder;
    let field_editor = object
        .downcast_ref::<NSText>()
        .is_some_and(|text| text.isFieldEditor());
    let logical = logical_first_responder(window);
    Some(Responder {
        class: class_name(object),
        field_editor,
        logical_class: logical
            .as_deref()
            .map_or_else(|| class_name(object), class_name),
    })
}

/// Логический first responder: поле, которое редактирует field editor, или сам ответчик.
pub fn logical_first_responder(window: &NSWindow) -> Option<Retained<AnyObject>> {
    let responder = window.firstResponder()?;
    let object: &AnyObject = &responder;
    if let Some(text) = object.downcast_ref::<NSText>()
        && text.isFieldEditor()
    {
        // SAFETY: `delegate` у NSText возвращает объект или nil.
        let delegate: Option<Retained<AnyObject>> = unsafe { msg_send![text, delegate] };
        if delegate.is_some() {
            return delegate;
        }
    }
    Some(Retained::into_super(Retained::into_super(responder)))
}

/// Фокус приложения: активность, key и main окна и first responder key window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Focus {
    pub active: bool,
    pub key_window: Option<isize>,
    pub main_window: Option<isize>,
    pub first_responder: Option<Responder>,
}

pub fn focus(app: &NSApplication) -> Focus {
    let key = app.keyWindow();
    Focus {
        active: app.isActive(),
        key_window: key.as_ref().map(|window| window.windowNumber()),
        main_window: app.mainWindow().map(|window| window.windowNumber()),
        first_responder: key.as_deref().and_then(first_responder),
    }
}

/// Атрибуты доступности элемента по протоколу `NSAccessibility`. Атрибут, на который объект не
/// отвечает, остаётся пустым.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AxElement {
    pub class: String,
    pub element: Option<bool>,
    pub role: Option<String>,
    pub subrole: Option<String>,
    pub role_description: Option<String>,
    pub label: Option<String>,
    pub title: Option<String>,
    pub value: Option<String>,
    pub help: Option<String>,
    pub identifier: Option<String>,
    pub enabled: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub focused: Option<bool>,
    pub disclosure_level: Option<isize>,
    /// Кадр на экране в точках, начало внизу слева главного экрана.
    pub frame: Option<Rect>,
    pub actions: Vec<String>,
    pub children: usize,
    pub navigation_children: Option<usize>,
}

fn responds(object: &AnyObject, selector: Sel) -> bool {
    object.class().responds_to(selector)
}

fn ax_string(object: &AnyObject, selector: Sel) -> Option<String> {
    if !responds(object, selector) {
        return None;
    }
    // SAFETY: строковые атрибуты `NSAccessibility` возвращают NSString или nil; селектор
    // проверен выше.
    let value: Option<Retained<AnyObject>> =
        unsafe { msg_send![object, performSelector: selector] };
    value.map(|value| describe(&value))
}

fn ax_bool(object: &AnyObject, selector: Sel) -> Option<bool> {
    if !responds(object, selector) {
        return None;
    }
    let value: bool = match selector {
        // SAFETY: булевы атрибуты `NSAccessibility` без аргументов; селектор проверен выше.
        s if s == sel!(isAccessibilityElement) => unsafe {
            msg_send![object, isAccessibilityElement]
        },
        s if s == sel!(isAccessibilityEnabled) => unsafe {
            msg_send![object, isAccessibilityEnabled]
        },
        s if s == sel!(isAccessibilitySelected) => unsafe {
            msg_send![object, isAccessibilitySelected]
        },
        s if s == sel!(isAccessibilityExpanded) => unsafe {
            msg_send![object, isAccessibilityExpanded]
        },
        s if s == sel!(isAccessibilityFocused) => unsafe {
            msg_send![object, isAccessibilityFocused]
        },
        _ => return None,
    };
    Some(value)
}

fn ax_array(object: &AnyObject, selector: Sel) -> Option<Retained<NSArray<AnyObject>>> {
    if !responds(object, selector) {
        return None;
    }
    // SAFETY: атрибуты-массивы `NSAccessibility` возвращают NSArray или nil.
    unsafe { msg_send![object, performSelector: selector] }
}

/// Строка объекта: у NSString — сама строка, у остальных `description`.
pub fn describe(object: &AnyObject) -> String {
    if let Some(string) = object.downcast_ref::<NSString>() {
        return string.to_string();
    }
    // SAFETY: `description` есть у каждого NSObject.
    let description: Option<Retained<NSString>> = unsafe { msg_send![object, description] };
    description.map_or_else(|| class_name(object), |text| text.to_string())
}

pub fn accessibility(object: &AnyObject) -> AxElement {
    let frame = responds(object, sel!(accessibilityFrame)).then(|| {
        // SAFETY: `accessibilityFrame` возвращает NSRect; селектор проверен.
        let frame: NSRect = unsafe { msg_send![object, accessibilityFrame] };
        rect(frame)
    });
    let disclosure_level = responds(object, sel!(accessibilityDisclosureLevel)).then(|| {
        // SAFETY: `accessibilityDisclosureLevel` возвращает NSInteger; селектор проверен.
        let level: isize = unsafe { msg_send![object, accessibilityDisclosureLevel] };
        level
    });
    let actions = ax_array(object, sel!(accessibilityActionNames))
        .map(|names| names.iter().map(|name| describe(&name)).collect())
        .unwrap_or_default();
    AxElement {
        class: class_name(object),
        element: ax_bool(object, sel!(isAccessibilityElement)),
        role: ax_string(object, sel!(accessibilityRole)),
        subrole: ax_string(object, sel!(accessibilitySubrole)),
        role_description: ax_string(object, sel!(accessibilityRoleDescription)),
        label: ax_string(object, sel!(accessibilityLabel)),
        title: ax_string(object, sel!(accessibilityTitle)),
        value: ax_string(object, sel!(accessibilityValue)),
        help: ax_string(object, sel!(accessibilityHelp)),
        identifier: ax_string(object, sel!(accessibilityIdentifier)),
        enabled: ax_bool(object, sel!(isAccessibilityEnabled)),
        selected: ax_bool(object, sel!(isAccessibilitySelected)),
        expanded: ax_bool(object, sel!(isAccessibilityExpanded)),
        focused: ax_bool(object, sel!(isAccessibilityFocused)),
        disclosure_level,
        frame,
        actions,
        children: accessibility_children(object).len(),
        navigation_children: ax_array(object, sel!(accessibilityChildrenInNavigationOrder))
            .map(|children| children.count()),
    }
}

/// Элемент доступности и его потомки до глубины `depth`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AxNode {
    #[serde(flatten)]
    pub element: AxElement,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<AxNode>,
}

impl AxNode {
    /// Элементы поддерева в глубину, начиная с этого.
    pub fn walk(&self) -> Vec<&AxElement> {
        let mut nodes = vec![self];
        let mut index = 0;
        while index < nodes.len() {
            let node = nodes[index];
            nodes.extend(node.children.iter());
            index += 1;
        }
        nodes.into_iter().map(|node| &node.element).collect()
    }

    /// Первый элемент доступности поддерева с ролью `role`.
    pub fn find_role(&self, role: &str) -> Option<&AxElement> {
        self.walk()
            .into_iter()
            .find(|element| element.element == Some(true) && element.role.as_deref() == Some(role))
    }
}

pub fn accessibility_tree(object: &AnyObject, depth: usize) -> AxNode {
    AxNode {
        element: accessibility(object),
        children: if depth == 0 {
            Vec::new()
        } else {
            accessibility_children(object)
                .iter()
                .map(|child| accessibility_tree(child, depth - 1))
                .collect()
        },
    }
}

pub fn accessibility_children(object: &AnyObject) -> Vec<Retained<AnyObject>> {
    ax_array(object, sel!(accessibilityChildren))
        .map(|children| children.to_vec())
        .unwrap_or_default()
}

/// Сцена в виде строк для `result.json`.
pub fn scene_info(scene: SceneOptions) -> SceneInfo {
    SceneInfo {
        split: scene.split.name().to_owned(),
        contrast: scene.contrast_name().to_owned(),
        motion: scene.motion_name().to_owned(),
    }
}

/// Часть окружения, которая не требует окна: ОС, железо, сборка, сцена.
pub fn static_environment(scene: SceneOptions) -> Environment {
    Environment {
        os: NSProcessInfo::processInfo()
            .operatingSystemVersionString()
            .to_string(),
        hardware: Hardware {
            model: sysctl_string("hw.model"),
            cpu: sysctl_string("machdep.cpu.brand_string"),
            memory_bytes: sysctl_u64("hw.memsize"),
        },
        build: Build::from_env(|name| std::env::var(name).ok()),
        scene: scene_info(scene),
        display: None,
        window: None,
        input_source: None,
        fixtures: BTreeMap::new(),
    }
}

fn sysctl_raw(name: &str) -> Option<Vec<u8>> {
    let name = CString::new(name).ok()?;
    let mut len = 0usize;
    // SAFETY: первый вызов узнаёт размер значения, буфер не передаётся.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || len == 0 {
        return None;
    }
    let mut buffer = vec![0u8; len];
    // SAFETY: буфер длины `len` жив на время вызова.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    buffer.truncate(len);
    Some(buffer)
}

fn sysctl_string(name: &str) -> Option<String> {
    let bytes = sysctl_raw(name)?;
    let text = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
    Some(String::from_utf8_lossy(text).trim().to_owned()).filter(|text| !text.is_empty())
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let bytes: [u8; 8] = sysctl_raw(name)?.try_into().ok()?;
    Some(u64::from_ne_bytes(bytes))
}

pub fn display_info(screen: &NSScreen, chosen_builtin: bool) -> DisplayInfo {
    let id = screen.CGDirectDisplayID();
    DisplayInfo {
        id,
        name: screen.localizedName().to_string(),
        builtin: CGDisplayIsBuiltin(id),
        chosen: if chosen_builtin { "builtin" } else { "main" }.to_owned(),
        frame: rect(screen.frame()),
        visible_frame: rect(screen.visibleFrame()),
        scale: screen.backingScaleFactor(),
        max_fps: screen.maximumFramesPerSecond(),
    }
}

pub fn window_info(window: &NSWindow) -> Option<WindowInfo> {
    let view = window.contentView()?;
    let bounds = view.bounds();
    Some(WindowInfo {
        frame: rect(window.frame()),
        content_points: size(bounds.size),
        bitmap_pixels: size(view.convertRectToBacking(bounds).size),
        backing_scale: window.backingScaleFactor(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(class: &str, x: f64, subviews: Vec<ViewNode>) -> ViewNode {
        ViewNode {
            class: class.to_owned(),
            frame_in_window: Rect {
                x,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            hidden: false,
            hidden_or_ancestor: false,
            alpha: 1.0,
            layer: None,
            subviews,
        }
    }

    fn ax(role: &str, element: bool, children: Vec<AxNode>) -> AxNode {
        AxNode {
            element: AxElement {
                role: Some(role.to_owned()),
                element: Some(element),
                ..AxElement::default()
            },
            children,
        }
    }

    #[test]
    fn accessibility_tree_finds_element_role() {
        let tree = ax(
            "AXUnknown",
            false,
            vec![ax("AXGroup", true, vec![ax("AXTextField", true, vec![])])],
        );
        assert_eq!(tree.walk().len(), 3);
        assert!(tree.find_role("AXTextField").is_some());
        assert!(tree.find_role("AXUnknown").is_none(), "не элемент");
        assert!(tree.find_role("AXButton").is_none());
    }

    #[test]
    fn tree_walk_and_find() {
        let tree = node(
            "NSThemeFrame",
            0.0,
            vec![node("NSView", 1.0, vec![node("NSTextField", 2.0, vec![])])],
        );
        let classes: Vec<&str> = tree.walk().iter().map(|n| n.class.as_str()).collect();
        assert_eq!(classes, ["NSThemeFrame", "NSView", "NSTextField"]);
        let field = node("NSTextField", 2.0, vec![]).frame_in_window;
        assert!(tree.find("NSTextField", field).is_some());
        assert!(tree.find("NSView", field).is_none());
    }
}
