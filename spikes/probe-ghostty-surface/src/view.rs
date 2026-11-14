//! Вид поверхности: подкласс `NSView`, который libghostty делает layer-hosting в
//! `ghostty_surface_new` (`platform.macos.nsview`). Вид сам `layer` и `wantsLayer` не ставит.
//!
//! Вид подаёт поверхности размер и масштаб в пикселях с `refresh` после каждой подачи размера,
//! фокус и `set_display_id`. Вне окна размер и масштаб не подаются: у вида без окна нет своей
//! подложки, и подача дала бы ребёнку лишний `SIGWINCH`. Ручку поверхности вид получает в
//! [`SurfaceView::attach`] и снимает в [`SurfaceView::detach`] до освобождения; состояние runtime
//! он не трогает.
//!
//! Ввод:
//! - `keyDown` собирает событие и текст, накопленный `insertText` во время `interpretKeyEvents`,
//!   в `ghostty_input_key_s` ([`keys::plan_key_down`]); коммит вне `keyDown` — текстовое событие без
//!   физической клавиши через тот же `ghostty_surface_key`. `ghostty_surface_text` не используется:
//!   это вставка;
//! - `performKeyEquivalent` отдаёт поверхности сочетания, которые сейчас — её привязки (`Cmd-V`);
//!   остальные идут по обычной цепочке AppKit и, если меню их не взяло, приходят в `keyDown`;
//! - получатель, съевший `keyDown`, забирает его `keyUp` и автоповтор: `sendEvent` пробы доставляет
//!   их владельцу ([`before_dispatch`]), отпускание уходит в ghostty, только если нажатие ушло туда
//!   физической клавишей;
//! - клавиша действует на видимое: вид вне окна, скрытый или без поверхности ввод не отправляет;
//!   новое нажатие и коммит вне `keyDown` — только у вида с фокусом клавиатуры, отпускание и
//!   автоповтор — у владельца нажатия;
//! - фокус поверхности — first responder своего окна, когда окно key.
//!
//! `NSTextInputClient`: документ клиента — только marked text, отправленный текст уже в PTY и не
//! редактируется. `markedRange` и `selectedRange` считают единицы UTF-16 marked text; без него оба
//! `{NSNotFound, 0}`. Смена фокуса при marked text снимает его без коммита: `discardMarkedText`
//! просит метод ввода забыть composition, поэтому повторного коммита нет.

use std::cell::{Cell, RefCell};

use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSEvent, NSEventModifierFlags, NSEventType, NSResponder, NSTextInputClient, NSView,
    NSWindowDidBecomeKeyNotification, NSWindowDidChangeScreenNotification,
    NSWindowDidResignKeyNotification,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSNotification, NSNotificationCenter,
    NSObject, NSPoint, NSRange, NSRangePointer, NSRect, NSSize, NSString, NSUInteger,
};
use objc2_quartz_core::CATransaction;
use serde::Serialize;
use shellio_sys_ghostty as sys;

use crate::clock;
use crate::ghostty::surface::Handle;
use crate::keys::{self, Action, EventKey, KeyDown, KeyInput, NO_PHYSICAL_KEY, Owners};
use crate::scenario::counters::{Live, counter};
use crate::utf16::{self, Range16, Surrogates};

counter!(SURFACE_VIEW, "ProbeSurfaceView");

/// Предел каждой записи журнала ввода.
const LOG_LIMIT: usize = 4000;

/// Что вид подал поверхности.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Feeds {
    pub size_feeds: u64,
    pub refreshes: u64,
    /// Подачи, пропущенные из-за нулевого размера в пикселях.
    pub zero_size_skipped: u64,
    /// Подачи размера и масштаба, пропущенные, пока вид вне окна.
    pub detached_skipped: u64,
    pub last_size_px: Option<(u32, u32)>,
    pub scale_feeds: u64,
    pub last_scale: Option<(f64, f64)>,
    pub display_ids: Vec<u32>,
    pub focus: Vec<bool>,
    /// Подачи `ghostty_surface_preedit`; пустая строка снимает preedit.
    pub preedits: Vec<String>,
}

/// Клавиша, дошедшая до вида.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Arrival {
    pub kind: &'static str,
    pub key_code: u16,
    pub repeat: bool,
    /// Доставлена владельцу нажатия мимо обычной цепочки.
    pub owned: bool,
    pub at_ns: u64,
}

/// Вызов `ghostty_surface_key`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Sent {
    pub from: &'static str,
    pub input: KeyInput,
    pub handled: Result<bool, String>,
    pub at_ns: u64,
}

/// Клавиша или текст, которые вид не отправил.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Dropped {
    pub kind: &'static str,
    pub key_code: u16,
    pub reason: &'static str,
}

/// Журнал ввода вида: фактические вызовы, а не модель.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct InputLog {
    pub arrivals: Vec<Arrival>,
    pub sent: Vec<Sent>,
    pub dropped: Vec<Dropped>,
    /// `performKeyEquivalent`, отданные поверхности как привязки.
    pub bindings: u64,
    /// Селекторы `doCommandBySelector:`.
    pub commands: Vec<String>,
    pub marked_discarded: u64,
    pub mouse_buttons: u64,
    pub mouse_positions: u64,
    pub scrolls: u64,
    pub pressures: u64,
}

fn push_limited<T>(list: &mut Vec<T>, item: T) {
    if list.len() < LOG_LIMIT {
        list.push(item);
    }
}

/// Состояние `NSTextInputClient`, прочитанное через методы протокола.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TextInputState {
    pub marked_text: String,
    pub has_marked_text: bool,
    pub marked_range: Range16,
    pub selected_range: Range16,
}

#[derive(Debug, Default)]
struct Marked {
    text: String,
    /// Выделение внутри marked text в единицах UTF-16.
    selected: Range16,
}

pub struct ViewIvars {
    _live: Live,
    handle: Cell<Option<Handle>>,
    /// Вид — first responder своего окна (по `become`/`resign`).
    responder: Cell<bool>,
    /// Последняя поданная фокусность.
    fed_focus: Cell<Option<bool>>,
    feeds: RefCell<Feeds>,
    marked: RefCell<Marked>,
    /// Текст `insertText` во время `interpretKeyEvents`; `None` — вне `keyDown`.
    accumulator: RefCell<Option<Vec<String>>>,
    surrogates: RefCell<Surrogates>,
    /// Коды клавиш, нажатие которых ушло в ghostty физической клавишей.
    sent_down: RefCell<Vec<u16>>,
    log: RefCell<InputLog>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeSurfaceView"]
    #[ivars = ViewIvars]
    pub struct SurfaceView;

    impl SurfaceView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            !self.isHiddenOrHasHiddenAncestor()
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            true
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: вызов реализации суперкласса без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if accepted {
                self.ivars().responder.set(true);
                self.refresh_focus();
            }
            accepted
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            // SAFETY: вызов реализации суперкласса без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), resignFirstResponder] };
            if accepted {
                self.discard_marked();
                self.ivars().responder.set(false);
                self.refresh_focus();
            }
            accepted
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            // SAFETY: вызов реализации суперкласса с тем же размером.
            let () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.feed_size();
        }

        #[unsafe(method(viewDidChangeBackingProperties))]
        fn view_did_change_backing_properties(&self) {
            // SAFETY: вызов реализации суперкласса без аргументов.
            let () = unsafe { msg_send![super(self), viewDidChangeBackingProperties] };
            self.backing_changed();
        }

        #[unsafe(method(viewDidMoveToWindow))]
        fn view_did_move_to_window(&self) {
            // SAFETY: вызов реализации суперкласса без аргументов.
            let () = unsafe { msg_send![super(self), viewDidMoveToWindow] };
            match self.window() {
                None => {
                    self.discard_marked();
                    self.ivars().responder.set(false);
                }
                Some(window) => {
                    let first = window
                        .firstResponder()
                        .is_some_and(|responder| std::ptr::addr_eq(&*responder, self));
                    self.ivars().responder.set(first);
                }
            }
            self.observe_window();
            self.feed_display();
            self.backing_changed();
            self.refresh_focus();
        }

        #[unsafe(method(windowDidChangeScreen:))]
        fn window_did_change_screen(&self, _notification: &NSNotification) {
            self.feed_display();
            self.backing_changed();
        }

        #[unsafe(method(windowKeyChanged:))]
        fn window_key_changed(&self, _notification: &NSNotification) {
            self.refresh_focus();
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.on_key_down(event, false);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.on_key_up(event, false);
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            self.on_flags_changed(event);
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            self.on_key_equivalent(event)
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.on_mouse_button(event, sys::ghostty_input_mouse_state_e_GHOSTTY_MOUSE_PRESS, 0);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.on_mouse_button(event, sys::ghostty_input_mouse_state_e_GHOSTTY_MOUSE_RELEASE, 0);
            if let Some(handle) = self.input_handle() {
                // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
                unsafe { handle.mouse_pressure(0, 0.0) };
            }
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            let state = sys::ghostty_input_mouse_state_e_GHOSTTY_MOUSE_PRESS;
            if !self.on_mouse_button(event, state, 1) {
                // SAFETY: вызов реализации суперкласса с тем же событием.
                let () = unsafe { msg_send![super(self), rightMouseDown: event] };
            }
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            let state = sys::ghostty_input_mouse_state_e_GHOSTTY_MOUSE_RELEASE;
            if !self.on_mouse_button(event, state, 1) {
                // SAFETY: вызов реализации суперкласса с тем же событием.
                let () = unsafe { msg_send![super(self), rightMouseUp: event] };
            }
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            let state = sys::ghostty_input_mouse_state_e_GHOSTTY_MOUSE_PRESS;
            self.on_mouse_button(event, state, event.buttonNumber());
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            let state = sys::ghostty_input_mouse_state_e_GHOSTTY_MOUSE_RELEASE;
            self.on_mouse_button(event, state, event.buttonNumber());
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.on_mouse_moved(event);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.on_mouse_moved(event);
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            self.on_mouse_moved(event);
        }

        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            self.on_mouse_moved(event);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            self.on_scroll(event);
        }

        #[unsafe(method(pressureChangeWithEvent:))]
        fn pressure_change(&self, event: &NSEvent) {
            let Some(handle) = self.input_handle() else {
                return;
            };
            let stage = u32::try_from(event.stage()).unwrap_or(0);
            // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
            unsafe { handle.mouse_pressure(stage, f64::from(event.pressure())) };
            self.ivars().log.borrow_mut().pressures += 1;
        }
    }

    unsafe impl NSTextInputClient for SurfaceView {
        #[unsafe(method(insertText:replacementRange:))]
        fn insert_text(&self, string: &AnyObject, _replacement: NSRange) {
            self.on_insert_text(string);
        }

        #[unsafe(method(doCommandBySelector:))]
        fn do_command(&self, selector: Sel) {
            self.on_command(selector);
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        fn set_marked_text(&self, string: &AnyObject, selected: NSRange, _replacement: NSRange) {
            self.on_set_marked(string, selected);
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            self.unmark();
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            to_ns_range(self.text_input_state().selected_range)
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            to_ns_range(self.text_input_state().marked_range)
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            self.text_input_state().has_marked_text
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        fn attributed_substring(
            &self,
            range: NSRange,
            actual: NSRangePointer,
        ) -> Option<Retained<NSAttributedString>> {
            self.substring(range, actual)
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes(&self) -> Retained<NSArray<NSAttributedStringKey>> {
            NSArray::new()
        }

        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        fn first_rect(&self, range: NSRange, actual: NSRangePointer) -> NSRect {
            self.ime_rect(range, actual)
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index(&self, _point: NSPoint) -> NSUInteger {
            utf16::NOT_FOUND
        }
    }
);

fn to_ns_range(range: Range16) -> NSRange {
    NSRange::new(range.location, range.length)
}

fn from_ns_range(range: NSRange) -> Range16 {
    Range16::new(range.location, range.length)
}

/// Единицы UTF-16 строки `insertText:` или `setMarkedText:` (`NSString` или `NSAttributedString`).
fn utf16_units(string: &AnyObject) -> Vec<u16> {
    let text = match string.downcast_ref::<NSAttributedString>() {
        Some(attributed) => attributed.string(),
        None => match string.downcast_ref::<NSString>() {
            Some(text) => text.retain(),
            None => return Vec::new(),
        },
    };
    (0..text.length())
        .map(|index| text.characterAtIndex(index))
        .collect()
}

fn flags_of(event: &NSEvent) -> u64 {
    event.modifierFlags().0 as u64
}

fn string_of(text: Option<Retained<NSString>>) -> Option<String> {
    text.map(|text| text.to_string())
}

/// Владелец нажатой клавиши для всего процесса.
#[derive(Debug)]
enum Owner {
    Surface(Weak<SurfaceView>),
    /// `keyDown` съел не вид поверхности: меню, вид стенда, окно.
    Elsewhere,
}

thread_local! {
    static OWNERS: RefCell<Owners<Owner>> = RefCell::new(Owners::default());
}

fn owner_is(code: u16, view: &SurfaceView) -> Option<bool> {
    OWNERS.with(|owners| {
        owners.borrow().owner(code).map(|owner| match owner {
            Owner::Surface(weak) => weak
                .load()
                .is_some_and(|owner| std::ptr::addr_eq(Retained::as_ptr(&owner), view)),
            Owner::Elsewhere => false,
        })
    })
}

/// Вызывается `sendEvent` до обычной доставки. Нажатие без повтора начинает с владельца
/// «не поверхность»: вид поверхности, съевший его, перепишет владельца. `keyUp` и автоповтор
/// нажатия, съеденного видом поверхности, доставляются ему напрямую. `true` — событие доставлено.
pub fn before_dispatch(event: &NSEvent) -> bool {
    let kind = event.r#type();
    if kind == NSEventType::KeyDown && !event.isARepeat() {
        OWNERS.with(|owners| owners.borrow_mut().claim(event.keyCode(), Owner::Elsewhere));
        return false;
    }
    let owned = kind == NSEventType::KeyUp || (kind == NSEventType::KeyDown && event.isARepeat());
    if !owned {
        return false;
    }
    let code = event.keyCode();
    let view = OWNERS.with(|owners| match owners.borrow().owner(code) {
        Some(Owner::Surface(weak)) => weak.load(),
        _ => None,
    });
    let Some(view) = view else {
        return false;
    };
    match kind {
        NSEventType::KeyUp => view.on_key_up(event, true),
        _ => view.on_key_down(event, true),
    }
    if kind == NSEventType::KeyUp {
        OWNERS.with(|owners| owners.borrow_mut().release(code));
    }
    true
}

/// Вызывается после обычной доставки: отпускание снимает владельца.
pub fn after_dispatch(event: &NSEvent) {
    if event.r#type() == NSEventType::KeyUp {
        OWNERS.with(|owners| owners.borrow_mut().release(event.keyCode()));
    }
}

impl SurfaceView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<SurfaceView> {
        let this = SurfaceView::alloc(mtm).set_ivars(ViewIvars {
            _live: SURFACE_VIEW.live(),
            handle: Cell::new(None),
            responder: Cell::new(false),
            fed_focus: Cell::new(None),
            feeds: RefCell::new(Feeds::default()),
            marked: RefCell::new(Marked::default()),
            accumulator: RefCell::new(None),
            surrogates: RefCell::new(Surrogates::default()),
            sent_down: RefCell::new(Vec::new()),
            log: RefCell::new(InputLog::default()),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Размер содержимого в пикселях подложки, с отбрасыванием дробной части.
    pub fn pixel_size(&self) -> (u32, u32) {
        crate::scene::to_pixels(self.bounds().size, self.backing_scale())
    }

    /// Масштаб подложки окна или главного экрана, если окна ещё нет.
    pub fn backing_scale(&self) -> f64 {
        match self.window() {
            Some(window) => window.backingScaleFactor(),
            None => {
                let size = self.convertSizeToBacking(NSSize::new(1.0, 1.0));
                if size.width > 0.0 { size.width } else { 1.0 }
            }
        }
    }

    pub fn feeds(&self) -> Feeds {
        self.ivars().feeds.borrow().clone()
    }

    pub fn input_log(&self) -> InputLog {
        self.ivars().log.borrow().clone()
    }

    /// Клавиши, дошедшие до вида: вид события и код.
    pub fn key_arrivals(&self) -> Vec<(&'static str, u16)> {
        self.ivars()
            .log
            .borrow()
            .arrivals
            .iter()
            .map(|arrival| (arrival.kind, arrival.key_code))
            .collect()
    }

    pub fn is_attached(&self) -> bool {
        self.ivars().handle.get().is_some()
    }

    /// Ручка живой поверхности для чтений сценария.
    pub fn handle(&self) -> Option<Handle> {
        self.ivars().handle.get()
    }

    /// Поверхность создана: рождённая с размером 800×600, она сразу получает масштаб, размер в
    /// пикселях с `refresh`, дисплей и фокус.
    pub fn attach(&self, handle: Handle) {
        self.ivars().handle.set(Some(handle));
        self.feed_scale();
        self.feed_size();
        self.feed_display();
        self.feed_focus(self.effective_focus());
    }

    /// Ручка снимается до `ghostty_surface_free`.
    pub fn detach(&self) {
        self.ivars().handle.set(None);
        OWNERS.with(|owners| {
            owners.borrow_mut().forget(|owner| match owner {
                Owner::Surface(weak) => weak
                    .load()
                    .is_none_or(|owner| std::ptr::addr_eq(Retained::as_ptr(&owner), self)),
                Owner::Elsewhere => false,
            })
        });
    }

    /// Фокус поверхности: first responder окна, которое сейчас key.
    fn effective_focus(&self) -> bool {
        self.ivars().responder.get() && self.window().is_some_and(|window| window.isKeyWindow())
    }

    fn refresh_focus(&self) {
        let focused = self.effective_focus();
        if self.ivars().fed_focus.get() != Some(focused) {
            self.feed_focus(focused);
        }
    }

    fn feed_focus(&self, focused: bool) {
        let Some(handle) = self.ivars().handle.get() else {
            return;
        };
        self.ivars().fed_focus.set(Some(focused));
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        unsafe { handle.set_focus(focused) };
        self.ivars().feeds.borrow_mut().focus.push(focused);
    }

    /// Ручка для подачи размера и масштаба: только пока вид в окне.
    fn sized_handle(&self) -> Option<Handle> {
        let handle = self.ivars().handle.get()?;
        if self.window().is_none() {
            self.ivars().feeds.borrow_mut().detached_skipped += 1;
            return None;
        }
        Some(handle)
    }

    fn feed_size(&self) {
        let Some(handle) = self.sized_handle() else {
            return;
        };
        let (width, height) = self.pixel_size();
        if width == 0 || height == 0 {
            self.ivars().feeds.borrow_mut().zero_size_skipped += 1;
            return;
        }
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        unsafe {
            handle.set_size(width, height);
            handle.refresh();
        }
        let mut feeds = self.ivars().feeds.borrow_mut();
        feeds.size_feeds += 1;
        feeds.refreshes += 1;
        feeds.last_size_px = Some((width, height));
    }

    fn feed_scale(&self) {
        let Some(handle) = self.sized_handle() else {
            return;
        };
        let frame = self.frame();
        if frame.size.width <= 0.0 || frame.size.height <= 0.0 {
            return;
        }
        let backing = self.convertRectToBacking(frame);
        let x = backing.size.width / frame.size.width;
        let y = backing.size.height / frame.size.height;
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        unsafe { handle.set_content_scale(x, y) };
        let mut feeds = self.ivars().feeds.borrow_mut();
        feeds.scale_feeds += 1;
        feeds.last_scale = Some((x, y));
    }

    fn feed_display(&self) {
        let Some(handle) = self.ivars().handle.get() else {
            return;
        };
        let Some(screen) = self.window().and_then(|window| window.screen()) else {
            return;
        };
        let display = screen.CGDirectDisplayID();
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        unsafe { handle.set_display_id(display) };
        self.ivars().feeds.borrow_mut().display_ids.push(display);
    }

    /// Смена масштаба подложки: `contentsScale` слоя без анимации, затем масштаб и размер.
    fn backing_changed(&self) {
        if let (Some(layer), Some(window)) = (self.layer(), self.window()) {
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            layer.setContentsScale(window.backingScaleFactor());
            CATransaction::commit();
        }
        self.feed_scale();
        self.feed_size();
    }

    /// Подписки на смену экрана и key-статуса окна переустанавливаются при каждом переходе в
    /// другое окно.
    fn observe_window(&self) {
        let center = NSNotificationCenter::defaultCenter();
        // SAFETY: имена уведомлений — неизменяемые константы AppKit; селекторы реализованы
        // видом. Подписки по селектору снимаются сами при освобождении вида.
        unsafe {
            for name in [
                NSWindowDidChangeScreenNotification,
                NSWindowDidBecomeKeyNotification,
                NSWindowDidResignKeyNotification,
            ] {
                center.removeObserver_name_object(self, Some(name), None);
            }
            if let Some(window) = self.window() {
                center.addObserver_selector_name_object(
                    self,
                    sel!(windowDidChangeScreen:),
                    Some(NSWindowDidChangeScreenNotification),
                    Some(&window),
                );
                for name in [
                    NSWindowDidBecomeKeyNotification,
                    NSWindowDidResignKeyNotification,
                ] {
                    center.addObserver_selector_name_object(
                        self,
                        sel!(windowKeyChanged:),
                        Some(name),
                        Some(&window),
                    );
                }
            }
        }
    }

    // ---- клавиатура ----

    /// Ручка для ввода: вид в окне, видим и с поверхностью. Клавиша действует на видимое.
    fn input_handle(&self) -> Option<Handle> {
        let handle = self.ivars().handle.get()?;
        if self.window().is_none() || self.isHiddenOrHasHiddenAncestor() {
            return None;
        }
        Some(handle)
    }

    fn arrive(&self, kind: &'static str, event: &NSEvent, owned: bool) {
        let repeat = event.r#type() == NSEventType::KeyDown && event.isARepeat();
        let arrival = Arrival {
            kind,
            key_code: event.keyCode(),
            repeat,
            owned,
            at_ns: clock::uptime_ns(),
        };
        push_limited(&mut self.ivars().log.borrow_mut().arrivals, arrival);
    }

    fn drop_key(&self, kind: &'static str, key_code: u16, reason: &'static str) {
        let dropped = Dropped {
            kind,
            key_code,
            reason,
        };
        push_limited(&mut self.ivars().log.borrow_mut().dropped, dropped);
    }

    /// `ghostty_surface_key` вне заимствований вида: библиотека синхронно зовёт колбэки.
    fn send(&self, from: &'static str, handle: Handle, input: &KeyInput) {
        let at_ns = clock::uptime_ns();
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        let handled = unsafe { handle.key(input) };
        let sent = Sent {
            from,
            input: input.clone(),
            handled,
            at_ns,
        };
        push_limited(&mut self.ivars().log.borrow_mut().sent, sent);
    }

    /// Событие клавиши с символами события перевода.
    fn event_key(
        &self,
        event: &NSEvent,
        translation: &NSEvent,
        translation_flags: u64,
    ) -> EventKey {
        let without_control = NSEventModifierFlags(
            (translation_flags & !keys::flags::CONTROL & !keys::flags::RIGHT_CONTROL) as usize,
        );
        EventKey {
            key_code: event.keyCode(),
            flags: flags_of(event),
            translation_flags,
            characters: string_of(translation.characters()),
            without_control: string_of(translation.charactersByApplyingModifiers(without_control)),
            unmodified: string_of(
                event.charactersByApplyingModifiers(NSEventModifierFlags::empty()),
            ),
            repeat: event.r#type() == NSEventType::KeyDown && event.isARepeat(),
        }
    }

    /// Событие перевода: то же событие, если модификаторы перевода совпали, иначе копия с
    /// символами по модификаторам перевода. Совпавшее событие нужно переиспользовать: от этого
    /// зависят методы ввода (корейский).
    fn translation_event(&self, event: &NSEvent, translation_flags: u64) -> Retained<NSEvent> {
        if translation_flags == flags_of(event) {
            return event.retain();
        }
        let modifiers = NSEventModifierFlags(translation_flags as usize);
        let characters = event
            .charactersByApplyingModifiers(modifiers)
            .unwrap_or_default();
        let ignoring = event.charactersIgnoringModifiers().unwrap_or_default();
        NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            event.r#type(),
            event.locationInWindow(),
            modifiers,
            event.timestamp(),
            event.windowNumber(),
            None,
            &characters,
            &ignoring,
            event.isARepeat(),
            event.keyCode(),
        )
        .unwrap_or_else(|| event.retain())
    }

    fn on_key_down(&self, event: &NSEvent, owned: bool) {
        self.arrive("keyDown", event, owned);
        let code = event.keyCode();
        let repeat = event.isARepeat();
        if repeat && !owned && owner_is(code, self) == Some(false) {
            return self.drop_key("keyDown", code, "автоповтор нажатия, съеденного другим");
        }
        let Some(handle) = self.input_handle() else {
            return self.drop_key("keyDown", code, "вид вне окна, скрыт или без поверхности");
        };
        if !owned && !self.has_key_focus() {
            return self.drop_key("keyDown", code, "нет фокуса клавиатуры");
        }
        if !repeat {
            OWNERS.with(|owners| {
                owners
                    .borrow_mut()
                    .claim(code, Owner::Surface(Weak::new(self)))
            });
        }
        let flags = flags_of(event);
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        let translation_mods = unsafe { handle.key_translation_mods(keys::mods_from_flags(flags)) };
        let translation_flags = keys::translation_flags(flags, translation_mods);
        let translation = self.translation_event(event, translation_flags);
        let event_key = self.event_key(event, &translation, translation_flags);
        let marked_before = self.has_marked();
        *self.ivars().accumulator.borrow_mut() = Some(Vec::new());
        self.interpretKeyEvents(&NSArray::from_retained_slice(&[translation]));
        let accumulated = self
            .ivars()
            .accumulator
            .borrow_mut()
            .take()
            .unwrap_or_default();
        self.sync_preedit(marked_before);
        let plan = keys::plan_key_down(&KeyDown {
            event: event_key,
            accumulated,
            marked_before,
            marked_after: self.has_marked(),
        });
        if plan.is_empty() {
            self.drop_key("keyDown", code, "съедено composition");
        }
        let mut physical = false;
        for input in &plan {
            physical |= input.keycode != NO_PHYSICAL_KEY;
            self.send("keyDown", handle, input);
        }
        let mut sent_down = self.ivars().sent_down.borrow_mut();
        sent_down.retain(|held| *held != code);
        if physical {
            sent_down.push(code);
        }
    }

    fn on_key_up(&self, event: &NSEvent, owned: bool) {
        self.arrive("keyUp", event, owned);
        let code = event.keyCode();
        if !owned && owner_is(code, self) != Some(true) {
            return self.drop_key("keyUp", code, "отпускание чужого нажатия");
        }
        let sent = {
            let mut sent_down = self.ivars().sent_down.borrow_mut();
            let before = sent_down.len();
            sent_down.retain(|held| *held != code);
            sent_down.len() != before
        };
        if !sent {
            return self.drop_key("keyUp", code, "нажатие не ушло в терминал");
        }
        // Владелец получает отпускание, даже если вид уже скрыт: нажатие ушло в его терминал.
        let Some(handle) = self.ivars().handle.get() else {
            return self.drop_key("keyUp", code, "поверхность освобождена");
        };
        let event_key = self.event_key(event, event, flags_of(event));
        self.send("keyUp", handle, &keys::key_up(&event_key));
    }

    fn on_flags_changed(&self, event: &NSEvent) {
        self.arrive("flagsChanged", event, false);
        let code = event.keyCode();
        if self.has_marked() {
            return self.drop_key("flagsChanged", code, "идёт composition");
        }
        let Some(handle) = self.input_handle() else {
            return self.drop_key(
                "flagsChanged",
                code,
                "вид вне окна, скрыт или без поверхности",
            );
        };
        if !self.has_key_focus() {
            return self.drop_key("flagsChanged", code, "нет фокуса клавиатуры");
        }
        let flags = flags_of(event);
        let event_key = EventKey {
            key_code: code,
            flags,
            translation_flags: flags,
            ..EventKey::default()
        };
        if let Some(input) = keys::flags_changed(&event_key) {
            self.send("flagsChanged", handle, &input);
        }
    }

    /// Вид — first responder key-окна и видим: только тогда он берёт сочетания клавиш.
    fn has_key_focus(&self) -> bool {
        let Some(window) = self.window() else {
            return false;
        };
        window.isKeyWindow()
            && !self.isHiddenOrHasHiddenAncestor()
            && window
                .firstResponder()
                .is_some_and(|responder| std::ptr::addr_eq(&*responder, self))
    }

    fn on_key_equivalent(&self, event: &NSEvent) -> bool {
        if event.r#type() != NSEventType::KeyDown || !self.has_key_focus() {
            return false;
        }
        let Some(handle) = self.input_handle() else {
            return false;
        };
        let flags = flags_of(event);
        let probe = self.event_key(event, event, flags).binding_probe();
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        if let Ok(Some(_)) = unsafe { handle.key_binding(&probe) } {
            self.ivars().log.borrow_mut().bindings += 1;
            self.on_key_down(event, false);
            return true;
        }
        let control = flags & keys::flags::CONTROL != 0;
        let others = flags & (keys::flags::SHIFT | keys::flags::COMMAND | keys::flags::OPTION);
        let equivalent = match string_of(event.charactersIgnoringModifiers()).as_deref() {
            // Ctrl-Return уходит в терминал, а не в контекстное меню.
            Some("\r") if control => "\r",
            // Ctrl-/ даёт системный звук; ghostty получает его как Ctrl-_.
            Some("/") if control && others == 0 => "_",
            _ => return false,
        };
        let text = NSString::from_str(equivalent);
        let synthetic = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            NSEventType::KeyDown,
            event.locationInWindow(),
            event.modifierFlags(),
            event.timestamp(),
            event.windowNumber(),
            None,
            &text,
            &text,
            event.isARepeat(),
            event.keyCode(),
        );
        match synthetic {
            Some(synthetic) => {
                self.on_key_down(&synthetic, false);
                true
            }
            None => false,
        }
    }

    // ---- NSTextInputClient ----

    fn has_marked(&self) -> bool {
        !self.ivars().marked.borrow().text.is_empty()
    }

    fn in_key_down(&self) -> bool {
        self.ivars().accumulator.borrow().is_some()
    }

    /// Состояние клиента ввода так, как его отдают методы протокола.
    pub fn text_input_state(&self) -> TextInputState {
        let marked = self.ivars().marked.borrow();
        let len16 = utf16::len(&marked.text);
        let has = len16 > 0;
        TextInputState {
            marked_text: marked.text.clone(),
            has_marked_text: has,
            marked_range: if has {
                Range16::new(0, len16)
            } else {
                Range16::NONE
            },
            selected_range: match (has, marked.selected.is_none()) {
                (false, _) => Range16::NONE,
                (true, true) => Range16::new(len16, 0),
                (true, false) => utf16::clamp(marked.selected, len16),
            },
        }
    }

    fn on_insert_text(&self, string: &AnyObject) {
        let units = utf16_units(string);
        let text = self.ivars().surrogates.borrow_mut().push(&units);
        self.unmark();
        let accumulating = {
            let mut accumulator = self.ivars().accumulator.borrow_mut();
            match accumulator.as_mut() {
                Some(list) => {
                    if !text.is_empty() {
                        list.push(text.clone());
                    }
                    true
                }
                None => false,
            }
        };
        if accumulating || text.is_empty() {
            return;
        }
        let Some(handle) = self.input_handle() else {
            return self.drop_key("insertText", 0, "вид вне окна, скрыт или без поверхности");
        };
        if !self.has_key_focus() {
            return self.drop_key("insertText", 0, "нет фокуса клавиатуры");
        }
        self.send(
            "insertText",
            handle,
            &KeyInput::committed(Action::Press, &text),
        );
    }

    fn on_set_marked(&self, string: &AnyObject, selected: NSRange) {
        let text = String::from_utf16_lossy(&utf16_units(string));
        let len16 = utf16::len(&text);
        *self.ivars().marked.borrow_mut() = Marked {
            selected: utf16::clamp(from_ns_range(selected), len16),
            text,
        };
        if !self.in_key_down() {
            self.sync_preedit(true);
        }
    }

    /// Снимает marked text без коммита. Внутри `keyDown` preedit сверяет сам `keyDown`.
    fn unmark(&self) {
        let had = {
            let mut marked = self.ivars().marked.borrow_mut();
            let had = !marked.text.is_empty();
            *marked = Marked::default();
            had
        };
        if had && !self.in_key_down() {
            self.sync_preedit(true);
        }
    }

    /// Смена фокуса или отмена при marked text: метод ввода забывает composition, вид снимает
    /// marked text без коммита.
    fn discard_marked(&self) {
        if !self.has_marked() {
            return;
        }
        if let Some(context) = self.inputContext() {
            context.discardMarkedText();
        }
        self.ivars().log.borrow_mut().marked_discarded += 1;
        self.unmark();
    }

    fn on_command(&self, selector: Sel) {
        let name = selector.name().to_string_lossy().into_owned();
        push_limited(&mut self.ivars().log.borrow_mut().commands, name);
        if selector == sel!(cancelOperation:) {
            self.discard_marked();
        }
    }

    /// Preedit поверхности по marked text; без него — снятие, если `clear_if_needed`.
    fn sync_preedit(&self, clear_if_needed: bool) {
        let Some(handle) = self.ivars().handle.get() else {
            return;
        };
        let text = self.ivars().marked.borrow().text.clone();
        if text.is_empty() && !clear_if_needed {
            return;
        }
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        unsafe { handle.preedit(&text) };
        self.ivars().feeds.borrow_mut().preedits.push(text);
    }

    fn substring(
        &self,
        range: NSRange,
        actual: NSRangePointer,
    ) -> Option<Retained<NSAttributedString>> {
        let text = self.ivars().marked.borrow().text.clone();
        if text.is_empty() {
            return None;
        }
        let range = utf16::clamp(from_ns_range(range), utf16::len(&text));
        let bytes = utf16::range_to_utf8(&text, range)?;
        if !actual.is_null() {
            // SAFETY: AppKit передаёт указатель на свой `NSRange` или NULL.
            unsafe { *actual = to_ns_range(range) };
        }
        Some(NSAttributedString::from_nsstring(&NSString::from_str(
            &text[bytes],
        )))
    }

    /// Прямоугольник окна кандидатов в координатах экрана по точке IME поверхности.
    fn ime_rect(&self, range: NSRange, actual: NSRangePointer) -> NSRect {
        if !actual.is_null() {
            // SAFETY: AppKit передаёт указатель на свой `NSRange` или NULL.
            unsafe { *actual = range };
        }
        let Some(handle) = self.ivars().handle.get() else {
            return NSRect::new(self.frame().origin, NSSize::ZERO);
        };
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        let ime = unsafe { handle.ime_point() };
        let height = self.bounds().size.height;
        let rect = NSRect::new(
            NSPoint::new(ime.x, height - ime.y),
            NSSize::new(ime.width, ime.height),
        );
        let in_window = self.convertRect_toView(rect, None);
        match self.window() {
            Some(window) => window.convertRectToScreen(in_window),
            None => in_window,
        }
    }

    // ---- мышь ----

    /// Точка события в точках вида от левого верхнего угла.
    fn mouse_point(&self, event: &NSEvent) -> (f64, f64) {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        (point.x, self.bounds().size.height - point.y)
    }

    /// Положение подаётся перед каждой кнопкой: без движения курсора над окном ghostty иначе
    /// не знает, где нажатие.
    fn on_mouse_button(
        &self,
        event: &NSEvent,
        state: sys::ghostty_input_mouse_state_e,
        number: isize,
    ) -> bool {
        let Some(handle) = self.input_handle() else {
            return false;
        };
        let mods = keys::mods_from_flags(flags_of(event));
        let (x, y) = self.mouse_point(event);
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        let consumed = unsafe {
            handle.mouse_pos(x, y, mods);
            handle.mouse_button(state, keys::mouse_button(number), mods)
        };
        let mut log = self.ivars().log.borrow_mut();
        log.mouse_positions += 1;
        log.mouse_buttons += 1;
        consumed
    }

    fn on_mouse_moved(&self, event: &NSEvent) {
        let Some(handle) = self.input_handle() else {
            return;
        };
        let mods = keys::mods_from_flags(flags_of(event));
        let (x, y) = self.mouse_point(event);
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        unsafe { handle.mouse_pos(x, y, mods) };
        self.ivars().log.borrow_mut().mouse_positions += 1;
    }

    fn on_scroll(&self, event: &NSEvent) {
        let Some(handle) = self.input_handle() else {
            return;
        };
        let precise = event.hasPreciseScrollingDeltas();
        let factor = if precise { 2.0 } else { 1.0 };
        let mods = keys::scroll_mods(precise, event.momentumPhase().0 as u64);
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        unsafe {
            handle.mouse_scroll(
                event.scrollingDeltaX() * factor,
                event.scrollingDeltaY() * factor,
                mods,
            )
        };
        self.ivars().log.borrow_mut().scrolls += 1;
    }
}
