//! Вид содержимого окна стенда: принимает фокус и записывает дошедшие до него клавиши.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSEvent, NSResponder, NSView};
use objc2_foundation::NSRect;

use super::counters::{Live, counter};
use crate::clock;

counter!(STAND_VIEW, "ProbeStandView");

/// Клавиша, дошедшая до вида.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arrival {
    pub kind: &'static str,
    pub key_code: u16,
    pub window: isize,
    /// Отметка `CLOCK_UPTIME_RAW` в момент вызова метода вида.
    pub at_ns: u64,
}

pub struct StandIvars {
    _live: Live,
    arrivals: RefCell<Vec<Arrival>>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeStandView"]
    #[ivars = StandIvars]
    pub struct StandView;

    impl StandView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.arrive("keyDown", event);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.arrive("keyUp", event);
        }
    }
);

impl StandView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<StandView> {
        let this = StandView::alloc(mtm).set_ivars(StandIvars {
            _live: STAND_VIEW.live(),
            arrivals: RefCell::new(Vec::new()),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn arrive(&self, kind: &'static str, event: &NSEvent) {
        let arrival = Arrival {
            kind,
            key_code: event.keyCode(),
            window: event.windowNumber(),
            at_ns: clock::uptime_ns(),
        };
        super::events::note(format!(
            "ProbeStandView {kind} keyCode={} win={}",
            arrival.key_code, arrival.window
        ));
        self.ivars().arrivals.borrow_mut().push(arrival);
    }

    pub fn arrivals(&self) -> Vec<Arrival> {
        self.ivars().arrivals.borrow().clone()
    }
}
