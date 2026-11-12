//! Объекты стенда для сценариев доставки: цель действий с делегатом меню и вид, который
//! записывает дошедший до него ввод. Сцену пробы они не заменяют.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSEvent, NSEventModifierFlags, NSMenu, NSMenuDelegate, NSMenuItem,
    NSResponder, NSView,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSString};

use super::counters::{Live, counter};

counter!(STAND_TARGET, "ProbeStandTarget");
counter!(INPUT_VIEW, "ProbeInputView");

pub struct TargetIvars {
    _live: Live,
    chosen: RefCell<Vec<isize>>,
    key_equivalents: Cell<usize>,
    opened: Cell<usize>,
    closed: Cell<usize>,
    highlights: RefCell<Vec<Option<String>>>,
}

define_class!(
    /// Цель пунктов меню стенда и делегат его меню.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeStandTarget"]
    #[ivars = TargetIvars]
    pub struct StandTarget;

    unsafe impl NSObjectProtocol for StandTarget {}

    impl StandTarget {
        #[unsafe(method(choose:))]
        fn choose(&self, sender: &NSMenuItem) {
            self.ivars().chosen.borrow_mut().push(sender.tag());
        }

        #[unsafe(method(keyEquivalent:))]
        fn key_equivalent(&self, _sender: &NSMenuItem) {
            let count = &self.ivars().key_equivalents;
            count.set(count.get() + 1);
        }
    }

    unsafe impl NSMenuDelegate for StandTarget {
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _menu: &NSMenu) {
            let opened = &self.ivars().opened;
            opened.set(opened.get() + 1);
            super::events::note("menuWillOpen");
        }

        #[unsafe(method(menuDidClose:))]
        fn menu_did_close(&self, _menu: &NSMenu) {
            let closed = &self.ivars().closed;
            closed.set(closed.get() + 1);
            super::events::note("menuDidClose");
        }

        #[unsafe(method(menu:willHighlightItem:))]
        fn menu_will_highlight(&self, _menu: &NSMenu, item: Option<&NSMenuItem>) {
            let title = item.map(|item| item.title().to_string());
            super::events::note(format!("menu:willHighlightItem: {title:?}"));
            self.ivars().highlights.borrow_mut().push(title);
        }
    }
);

impl StandTarget {
    pub fn new(mtm: MainThreadMarker) -> Retained<StandTarget> {
        let this = StandTarget::alloc(mtm).set_ivars(TargetIvars {
            _live: STAND_TARGET.live(),
            chosen: RefCell::new(Vec::new()),
            key_equivalents: Cell::new(0),
            opened: Cell::new(0),
            closed: Cell::new(0),
            highlights: RefCell::new(Vec::new()),
        });
        // SAFETY: `init` у NSObject без аргументов.
        unsafe { msg_send![super(this), init] }
    }

    pub fn chosen(&self) -> Vec<isize> {
        self.ivars().chosen.borrow().clone()
    }

    pub fn key_equivalents(&self) -> usize {
        self.ivars().key_equivalents.get()
    }

    pub fn opened(&self) -> usize {
        self.ivars().opened.get()
    }

    pub fn closed(&self) -> usize {
        self.ivars().closed.get()
    }

    pub fn highlights(&self) -> Vec<Option<String>> {
        self.ivars().highlights.borrow().clone()
    }

    pub fn as_delegate(&self) -> &ProtocolObject<dyn NSMenuDelegate> {
        ProtocolObject::from_ref(self)
    }

    /// Пункт меню с действием у этой цели.
    pub fn item(
        &self,
        mtm: MainThreadMarker,
        title: &str,
        action: Sel,
        key: &str,
        tag: isize,
    ) -> Retained<NSMenuItem> {
        // SAFETY: действия `choose:` и `keyEquivalent:` реализованы этим классом, а цель живёт
        // дольше меню: её держит сценарий до конца процесса.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        let target: &AnyObject = self;
        // SAFETY: см. выше.
        unsafe { item.setTarget(Some(target)) };
        item.setTag(tag);
        if !key.is_empty() {
            item.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
        }
        item
    }

    pub fn choose_action() -> Sel {
        sel!(choose:)
    }

    pub fn key_equivalent_action() -> Sel {
        sel!(keyEquivalent:)
    }
}

/// Событие, дошедшее до вида.
#[derive(Clone, Debug, PartialEq)]
pub struct Arrival {
    pub kind: &'static str,
    pub key_code: Option<u16>,
    /// Точка в координатах вида.
    pub location: Option<NSPoint>,
    pub window: isize,
}

impl Arrival {
    pub fn describe(&self) -> String {
        let mut text = format!("{} win={}", self.kind, self.window);
        if let Some(code) = self.key_code {
            text.push_str(&format!(" keyCode={code}"));
        }
        if let Some(point) = self.location {
            text.push_str(&format!(" at={},{}", point.x, point.y));
        }
        text
    }
}

pub struct InputIvars {
    _live: Live,
    arrivals: RefCell<Vec<Arrival>>,
}

define_class!(
    /// Вид, принимающий фокус и первый щелчок и записывающий клавиши и кнопки мыши.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeInputView"]
    #[ivars = InputIvars]
    pub struct InputView;

    impl InputView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.arrive("keyDown", event, false);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.arrive("keyUp", event, false);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.arrive("mouseDown", event, true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.arrive("mouseUp", event, true);
        }
    }
);

impl InputView {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        mask: NSAutoresizingMaskOptions,
    ) -> Retained<InputView> {
        let this = InputView::alloc(mtm).set_ivars(InputIvars {
            _live: INPUT_VIEW.live(),
            arrivals: RefCell::new(Vec::new()),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let view: Retained<InputView> = unsafe { msg_send![super(this), initWithFrame: frame] };
        view.setAutoresizingMask(mask);
        view
    }

    fn arrive(&self, kind: &'static str, event: &NSEvent, mouse: bool) {
        let arrival = Arrival {
            kind,
            key_code: (!mouse).then(|| event.keyCode()),
            location: mouse.then(|| self.convertPoint_fromView(event.locationInWindow(), None)),
            window: event.windowNumber(),
        };
        super::events::note(format!("ProbeInputView {}", arrival.describe()));
        self.ivars().arrivals.borrow_mut().push(arrival);
    }

    pub fn arrivals(&self) -> Vec<Arrival> {
        self.ivars().arrivals.borrow().clone()
    }

    pub fn as_view(&self) -> &NSView {
        self
    }
}
