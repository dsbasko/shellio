//! Палитра: дочерняя непрозрачная `NSPanel` основного окна, один экземпляр на процесс. Окно под
//! ней не затемняется (ADR-0024). Панель привязывается к родителю `addChildWindow` при открытии и
//! отвязывается `removeChildWindow` при закрытии, сама она живёт дольше этой связи.
//!
//! В цепочке ввода палитра стоит после composition и перед маршрутизатором режимов
//! ([`intercept`] первым в `sendEvent` пробы). При marked text у first responder нажатие идёт в
//! composition. Открытие — `Cmd-K` окна сцены (у ghostty на нём привязка очистки экрана, поэтому
//! дошедшее до поверхности нажатие было бы видно), закрытие — `Esc` или `Cmd-K` в палитре, щелчок
//! в родителя или потеря key. Пока палитра открыта, нажатия, адресованные родителю, съедаются:
//! ввод получает только её поле. Съеденное нажатие забирает свои `keyUp` и автоповтор.
//!
//! Режим маршрутизатора запоминается до того, как панель станет key, и возвращается при закрытии
//! (переход «открытие наложения» таблицы ожидания). Щелчок в родителя закрывает палитру раньше,
//! чем его увидит маршрутизатор, и затем доставляется обычным путём.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use objc2::rc::{Retained, Weak};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearanceCustomization, NSBackingStoreType, NSEvent, NSEventModifierFlags, NSEventType,
    NSFont, NSPanel, NSResponder, NSTextField, NSTextInputClient, NSTextView, NSView, NSWindow,
    NSWindowDidResignKeyNotification, NSWindowOrderingMode, NSWindowStyleMask,
};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObject, NSPoint, NSRect, NSSize, NSString,
};
use serde::Serialize;

use crate::clock;
use crate::scenario::counters::{Live, counter};
use crate::scene::{self, Palette as Colors, Scene};
use crate::view::{self, SurfaceView};

counter!(PALETTE_PANEL, "ProbePalettePanel");
counter!(PALETTE_OBSERVER, "ProbePaletteObserver");

pub const WIDTH: f64 = 480.0;
pub const HEIGHT: f64 = 200.0;
/// Отступ панели от верхнего края содержимого родителя.
pub const TOP: f64 = 64.0;
const FIELD_HEIGHT: f64 = 28.0;
const INSET: f64 = 14.0;
const FONT: f64 = 15.0;
/// `k` и `Esc` по физическому `keyCode`.
pub const OPEN_CODE: u16 = 40;
pub const ESC_CODE: u16 = 53;

/// Модификаторы, значимые для клавиш палитры.
fn significant(flags: usize) -> usize {
    flags
        & (NSEventModifierFlags::Command.0
            | NSEventModifierFlags::Control.0
            | NSEventModifierFlags::Option.0
            | NSEventModifierFlags::Shift.0)
}

/// Клавиша открытия: `Cmd-K` без других модификаторов.
pub fn is_open_key(code: u16, flags: usize) -> bool {
    code == OPEN_CODE && significant(flags) == NSEventModifierFlags::Command.0
}

/// Клавиша закрытия: `Esc` без модификаторов.
pub fn is_close_key(code: u16, flags: usize) -> bool {
    code == ESC_CODE && significant(flags) == 0
}

/// Рамка панели в координатах экрана: по центру содержимого родителя, [`TOP`] от его верха.
pub fn frame_in(content: NSRect) -> NSRect {
    let width = WIDTH.min(content.size.width.max(0.0));
    let height = HEIGHT.min((content.size.height - TOP).max(0.0));
    NSRect::new(
        NSPoint::new(
            (content.origin.x + (content.size.width - width) / 2.0).round(),
            (content.origin.y + content.size.height - TOP - height).round(),
        ),
        NSSize::new(width, height),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Close {
    Escape,
    Toggle,
    OutsideClick,
    ResignKey,
    Command,
}

#[derive(Clone, Debug, Serialize)]
pub struct OpenRecord {
    pub source: &'static str,
    pub t_start_ns: u64,
    /// От начала открытия до возврата из `makeKeyAndOrderFront` и смены first responder.
    pub sync_ms: f64,
    pub dark: bool,
    pub responder_before: Option<String>,
    /// Composition first responder родителя снята без коммита.
    pub composition_discarded: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CloseRecord {
    pub reason: Close,
    pub t_start_ns: u64,
    pub sync_ms: f64,
    pub text: String,
    /// First responder родителя возвращён к сохранённому при открытии.
    pub restored: bool,
    pub responder_after: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Record {
    pub opens: Vec<OpenRecord>,
    pub closes: Vec<CloseRecord>,
    /// Нажатия, адресованные родителю при открытой палитре.
    pub parent_keys_swallowed: u64,
    pub key_ups_swallowed: u64,
    pub repeats_swallowed: u64,
    /// Нажатия, пропущенные к composition.
    pub composing_passed: u64,
    pub outside_clicks: u64,
}

/// Состояние палитры для сценария.
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub open: bool,
    pub panel_visible: bool,
    pub panel_key: bool,
    pub panel_opaque: bool,
    pub panel_class: String,
    pub child_of_parent: bool,
    pub frame: [f64; 4],
    pub field_text: String,
    pub field_editing: bool,
    /// Панелей палитры создано за процесс.
    pub panels_created: u64,
    pub record: Record,
}

pub struct PanelIvars {
    _live: Live,
}

define_class!(
    /// Панель палитры: без рамки, но может стать key.
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeGhosttyPalettePanel"]
    #[ivars = PanelIvars]
    pub struct PalettePanel;

    impl PalettePanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }
    }
);

pub struct ObserverIvars {
    _live: Live,
}

define_class!(
    /// Наблюдатель потери key панелью: переключение приложения или окна закрывает палитру.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeGhosttyPaletteObserver"]
    #[ivars = ObserverIvars]
    struct PaletteObserver;

    impl PaletteObserver {
        #[unsafe(method(panelDidResignKey:))]
        fn panel_did_resign_key(&self, _notification: &NSNotification) {
            if is_open() {
                close(Close::ResignKey);
            }
        }
    }
);

struct Glue {
    panel: Retained<PalettePanel>,
    content: Retained<NSView>,
    field: Retained<NSTextField>,
    hint: Retained<NSTextField>,
    observer: Retained<PaletteObserver>,
    parent: Weak<NSWindow>,
    scene: Option<Rc<Scene>>,
    open: bool,
    saved: Option<Weak<NSResponder>>,
    record: Record,
}

thread_local! {
    static GLUE: RefCell<Option<Glue>> = const { RefCell::new(None) };
    static CREATED: Cell<u64> = const { Cell::new(0) };
}

/// Короткое заимствование состояния; AppKit внутри `f` не вызывается.
fn with<R>(f: impl FnOnce(&mut Glue) -> R) -> Option<R> {
    GLUE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut glue) => glue.as_mut().map(f),
        Err(_) => None,
    })
}

fn label(mtm: MainThreadMarker, text: &str, size: f64) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setFont(Some(&NSFont::systemFontOfSize(size)));
    label
}

fn build(mtm: MainThreadMarker) -> Glue {
    let this = PalettePanel::alloc(mtm).set_ivars(PanelIvars {
        _live: PALETTE_PANEL.live(),
    });
    // SAFETY: назначенный инициализатор NSPanel с рамкой, стилем, хранилищем и отсрочкой.
    let panel: Retained<PalettePanel> = unsafe {
        msg_send![
            super(this),
            initWithContentRect: NSRect::new(NSPoint::ZERO, NSSize::new(WIDTH, HEIGHT)),
            styleMask: NSWindowStyleMask::Borderless,
            backing: NSBackingStoreType::Buffered,
            defer: false
        ]
    };
    CREATED.with(|count| count.set(count.get() + 1));
    // SAFETY: панель держит Rust, AppKit не должен освобождать её при закрытии.
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setOpaque(true);
    panel.setHasShadow(true);
    panel.setHidesOnDeactivate(false);
    let content = NSView::new(mtm);
    content.setWantsLayer(true);
    panel.setContentView(Some(&content));
    let field = NSTextField::new(mtm);
    field.setBezeled(false);
    field.setBordered(false);
    field.setDrawsBackground(true);
    field.setFocusRingType(objc2_app_kit::NSFocusRingType::None);
    field.setFont(Some(&NSFont::systemFontOfSize(FONT)));
    field.setPlaceholderString(Some(&NSString::from_str("Команда")));
    field.setFrame(NSRect::new(
        NSPoint::new(INSET, HEIGHT - INSET - FIELD_HEIGHT),
        NSSize::new(WIDTH - 2.0 * INSET, FIELD_HEIGHT),
    ));
    content.addSubview(&field);
    let hint = label(mtm, "Палитра пробы: Esc — закрыть", 12.0);
    hint.setFrame(NSRect::new(
        NSPoint::new(INSET, INSET),
        NSSize::new(WIDTH - 2.0 * INSET, 18.0),
    ));
    content.addSubview(&hint);
    let observer = PaletteObserver::alloc(mtm).set_ivars(ObserverIvars {
        _live: PALETTE_OBSERVER.live(),
    });
    // SAFETY: `init` — назначенный инициализатор NSObject.
    let observer: Retained<PaletteObserver> = unsafe { msg_send![super(observer), init] };
    // SAFETY: имя уведомления — константа AppKit, селектор реализован наблюдателем, объект —
    // панель; подписка снимается в `uninstall`.
    unsafe {
        NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
            &observer,
            sel!(panelDidResignKey:),
            Some(NSWindowDidResignKeyNotification),
            Some(&panel),
        );
    }
    Glue {
        panel,
        content,
        field,
        hint,
        observer,
        parent: Weak::default(),
        scene: None,
        open: false,
        saved: None,
        record: Record::default(),
    }
}

/// Ставит палитру на окно сцены. Панель создаётся один раз за процесс и переиспользуется.
pub fn install(parent: &NSWindow, scene: Option<Rc<Scene>>) {
    let exists = GLUE.with(|cell| cell.borrow().is_some());
    if !exists {
        let glue = build(parent.mtm());
        GLUE.with(|cell| *cell.borrow_mut() = Some(glue));
    }
    with(|glue| {
        glue.parent = Weak::new(parent);
        glue.scene = scene;
        glue.record = Record::default();
    });
}

/// Закрывает палитру, отвязывает её от окна и снимает подписку. Панель освобождается.
pub fn uninstall() -> Option<Snapshot> {
    if is_open() {
        close(Close::Command);
    }
    let last = snapshot();
    let glue = GLUE.with(|cell| cell.borrow_mut().take());
    if let Some(glue) = glue {
        // SAFETY: снимается подписка, поставленная в `build`.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&glue.observer) };
        glue.panel.orderOut(None);
    }
    last
}

pub fn is_open() -> bool {
    with(|glue| glue.open).unwrap_or(false)
}

fn responder_name(responder: Option<&NSResponder>) -> Option<String> {
    responder.map(|responder| {
        let object: &objc2::runtime::AnyObject = responder;
        object.class().name().to_string_lossy().into_owned()
    })
}

/// Поле палитры редактируется и в нём есть marked text.
fn field_composing(field: &NSTextField) -> bool {
    field
        .currentEditor()
        .and_then(|editor| editor.downcast::<NSTextView>().ok())
        .is_some_and(|editor| editor.hasMarkedText())
}

fn parent_composing(parent: &NSWindow) -> bool {
    parent
        .firstResponder()
        .as_deref()
        .and_then(|responder| responder.downcast_ref::<SurfaceView>())
        .is_some_and(SurfaceView::is_composing)
}

pub fn snapshot() -> Option<Snapshot> {
    let (panel, field, parent, open, record) = with(|glue| {
        (
            glue.panel.clone(),
            glue.field.clone(),
            glue.parent.load(),
            glue.open,
            glue.record.clone(),
        )
    })?;
    let child_of_parent = parent.as_ref().is_some_and(|parent| {
        parent.childWindows().is_some_and(|children| {
            children
                .iter()
                .any(|child| std::ptr::addr_eq(Retained::as_ptr(&child), Retained::as_ptr(&panel)))
        })
    });
    let frame = panel.frame();
    let object: &objc2::runtime::AnyObject = &panel;
    Some(Snapshot {
        open,
        panel_visible: panel.isVisible(),
        panel_key: panel.isKeyWindow(),
        panel_opaque: panel.isOpaque(),
        panel_class: object.class().name().to_string_lossy().into_owned(),
        child_of_parent,
        frame: [
            frame.origin.x,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
        ],
        field_text: field.stringValue().to_string(),
        field_editing: field.currentEditor().is_some(),
        panels_created: CREATED.with(Cell::get),
        record,
    })
}

/// Цвета палитры по оформлению родителя; непрозрачные, из токенов сцены.
fn recolor(
    glue_panel: &PalettePanel,
    content: &NSView,
    field: &NSTextField,
    hint: &NSTextField,
    parent: &NSWindow,
) {
    let dark = scene::is_dark(&parent.effectiveAppearance());
    let colors = Colors::of(dark);
    glue_panel.setAppearance(Some(&parent.effectiveAppearance()));
    glue_panel.setBackgroundColor(Some(&colors.panel.color()));
    if let Some(layer) = content.layer() {
        layer.setBackgroundColor(Some(&colors.panel.color().CGColor()));
        layer.setBorderColor(Some(&colors.line.color().CGColor()));
        layer.setBorderWidth(1.0);
    }
    field.setBackgroundColor(Some(&colors.panel_head.color()));
    field.setTextColor(Some(&colors.text.color()));
    hint.setTextColor(Some(&colors.text_2.color()));
}

/// Открывает палитру. `source` — откуда пришло открытие, для журнала.
pub fn open(source: &'static str) -> Result<(), String> {
    let started = clock::uptime_ns();
    let (panel, content, field, hint, parent, already) = with(|glue| {
        (
            glue.panel.clone(),
            glue.content.clone(),
            glue.field.clone(),
            glue.hint.clone(),
            glue.parent.load(),
            glue.open,
        )
    })
    .ok_or("палитра не поставлена")?;
    let parent = parent.ok_or("окно палитры освобождено")?;
    if already {
        return Ok(());
    }
    crate::router::overlay_opened();
    let responder = parent.firstResponder();
    let composing = responder
        .as_deref()
        .and_then(|responder| responder.downcast_ref::<SurfaceView>())
        .filter(|surface| surface.is_composing())
        .map(|surface| {
            surface.discard_composition();
            true
        })
        .unwrap_or(false);
    with(|glue| {
        glue.open = true;
        glue.saved = responder.as_deref().map(Weak::new);
    });
    recolor(&panel, &content, &field, &hint, &parent);
    field.setStringValue(&NSString::from_str(""));
    let screen = parent.convertRectToScreen(parent.contentLayoutRect());
    panel.setFrame_display(frame_in(screen), false);
    // SAFETY: панель и родитель живы; связь снимается в `close`.
    unsafe { parent.addChildWindow_ordered(&panel, NSWindowOrderingMode::Above) };
    panel.makeKeyAndOrderFront(None);
    panel.makeFirstResponder(Some(&field));
    let record = OpenRecord {
        source,
        t_start_ns: started,
        sync_ms: clock::interval_ms(started, clock::uptime_ns()),
        dark: scene::is_dark(&parent.effectiveAppearance()),
        responder_before: responder_name(responder.as_deref()),
        composition_discarded: composing,
    };
    with(|glue| glue.record.opens.push(record));
    Ok(())
}

/// Закрывает палитру и возвращает first responder родителя и режим маршрутизатора.
pub fn close(reason: Close) {
    let started = clock::uptime_ns();
    let Some((panel, field, parent, saved, scene)) = with(|glue| {
        if !glue.open {
            return None;
        }
        glue.open = false;
        Some((
            glue.panel.clone(),
            glue.field.clone(),
            glue.parent.load(),
            glue.saved.take(),
            glue.scene.clone(),
        ))
    })
    .flatten() else {
        return;
    };
    let text = field.stringValue().to_string();
    if let Some(parent) = parent.as_ref() {
        parent.removeChildWindow(&panel);
    }
    panel.orderOut(None);
    let mut restored = false;
    let mut responder_after = None;
    if let Some(parent) = parent.as_ref() {
        parent.makeKeyWindow();
        if let Some(saved) = saved.as_ref().and_then(Weak::load) {
            let in_parent = saved
                .downcast_ref::<NSView>()
                .map(|view| {
                    view.window()
                        .is_some_and(|owner| std::ptr::eq(&*owner, &**parent))
                })
                .unwrap_or(true);
            if in_parent {
                restored = parent.makeFirstResponder(Some(&saved));
            }
        }
        let after = parent.firstResponder();
        restored = restored
            && match (after.as_deref(), saved.as_ref().and_then(Weak::load)) {
                (Some(after), Some(saved)) => std::ptr::eq(after, &*saved),
                _ => false,
            };
        responder_after = responder_name(after.as_deref());
        if let Some(scene) = scene {
            scene.sync_focus(parent);
        }
    }
    crate::router::overlay_closed();
    let record = CloseRecord {
        reason,
        t_start_ns: started,
        sync_ms: clock::interval_ms(started, clock::uptime_ns()),
        text,
        restored,
        responder_after,
    };
    with(|glue| glue.record.closes.push(record));
}

/// Вызывается `sendEvent` пробы первым. `true` — событие съедено.
pub fn intercept(event: &NSEvent) -> bool {
    let Some((panel, field, parent, open)) = with(|glue| {
        (
            glue.panel.clone(),
            glue.field.clone(),
            glue.parent.load(),
            glue.open,
        )
    }) else {
        return false;
    };
    let Some(parent) = parent else {
        return false;
    };
    let kind = event.r#type();
    // `keyCode` и `isARepeat` у событий не клавиатуры бросают исключение Objective-C.
    match kind {
        NSEventType::KeyUp if view::palette_owns(event.keyCode()) => {
            view::release_palette(event.keyCode());
            with(|glue| glue.record.key_ups_swallowed += 1);
            return true;
        }
        NSEventType::KeyDown if event.isARepeat() && view::palette_owns(event.keyCode()) => {
            with(|glue| glue.record.repeats_swallowed += 1);
            return true;
        }
        _ => {}
    }
    let number = event.windowNumber();
    let to_panel = number == panel.windowNumber() || (number == 0 && panel.isKeyWindow());
    let to_parent = number == parent.windowNumber() || (number == 0 && parent.isKeyWindow());
    match kind {
        NSEventType::KeyDown if !event.isARepeat() => {
            let code = event.keyCode();
            let flags = event.modifierFlags().0;
            if open && to_panel {
                if field_composing(&field) {
                    with(|glue| glue.record.composing_passed += 1);
                    return false;
                }
                let reason = if is_close_key(code, flags) {
                    Close::Escape
                } else if is_open_key(code, flags) {
                    Close::Toggle
                } else {
                    return false;
                };
                view::claim_for_palette(code);
                close(reason);
                return true;
            }
            if open && to_parent {
                view::claim_for_palette(code);
                with(|glue| glue.record.parent_keys_swallowed += 1);
                return true;
            }
            if !open && to_parent && is_open_key(code, flags) {
                if parent_composing(&parent) {
                    with(|glue| glue.record.composing_passed += 1);
                    return false;
                }
                view::claim_for_palette(code);
                if open_or_log("key") {
                    return true;
                }
            }
            false
        }
        NSEventType::LeftMouseDown | NSEventType::RightMouseDown | NSEventType::OtherMouseDown
            if open && number == parent.windowNumber() =>
        {
            with(|glue| glue.record.outside_clicks += 1);
            close(Close::OutsideClick);
            false
        }
        _ => false,
    }
}

fn open_or_log(source: &'static str) -> bool {
    match open(source) {
        Ok(()) => true,
        Err(reason) => {
            crate::scenario::events::note(format!("палитра не открыта: {reason}"));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: usize = NSEventModifierFlags::Command.0;
    const SHIFT: usize = NSEventModifierFlags::Shift.0;
    const CAPS: usize = NSEventModifierFlags::CapsLock.0;

    #[test]
    fn open_key_is_command_k_only() {
        assert!(is_open_key(OPEN_CODE, CMD));
        assert!(is_open_key(OPEN_CODE, CMD | CAPS));
        assert!(!is_open_key(OPEN_CODE, CMD | SHIFT));
        assert!(!is_open_key(OPEN_CODE, 0));
        assert!(!is_open_key(ESC_CODE, CMD));
    }

    #[test]
    fn close_key_is_plain_escape() {
        assert!(is_close_key(ESC_CODE, 0));
        assert!(is_close_key(ESC_CODE, CAPS));
        assert!(!is_close_key(ESC_CODE, SHIFT));
        assert!(!is_close_key(OPEN_CODE, 0));
    }

    #[test]
    fn frame_is_centered_below_the_top() {
        let content = NSRect::new(NSPoint::new(100.0, 50.0), NSSize::new(1180.0, 720.0));
        let frame = frame_in(content);
        assert_eq!(frame.size.width, WIDTH);
        assert_eq!(frame.size.height, HEIGHT);
        assert_eq!(frame.origin.x, 100.0 + (1180.0 - WIDTH) / 2.0);
        assert_eq!(frame.origin.y + frame.size.height, 50.0 + 720.0 - TOP);
    }

    #[test]
    fn frame_shrinks_in_a_small_parent() {
        let content = NSRect::new(NSPoint::ZERO, NSSize::new(300.0, 120.0));
        let frame = frame_in(content);
        assert_eq!(frame.size.width, 300.0);
        assert_eq!(frame.size.height, 120.0 - TOP);
        assert!(frame.origin.y >= 0.0);
    }
}
