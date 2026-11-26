//! Приёмник клавиш галереи (ADR-0010, «Уточнено в P1.6 (планирование)»): вид, который берёт
//! клавиатурный фокус и отдаёт получателю каждое дошедшее до него нажатие и отпускание клавиши.
//!
//! Вид ничего не рисует: его подложку ставит вызывающий. В цепочку `Tab` он не входит, фокус
//! получает щелчком или вызовом [`KeySink::focus`].

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSEvent, NSResponder, NSView};
use objc2_foundation::{NSObject, NSRect};

use crate::app::App;
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode};
use crate::key_monitor::{self, KeyMods};
use crate::own::{Callback, ComponentId, View};

counter!(SINK_OWNER, Owner, "KeySink");
counter!(SINK_VIEW, Class, "ShellioKeySink");

/// Событие приёмника. Вызывается изнутри AppKit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySinkEvent {
    /// Вид стал первым ответчиком окна (`true`) или перестал им быть.
    Focus(bool),
    /// До вида дошло нажатие (`down`) или отпускание клавиши.
    Key {
        down: bool,
        /// Виртуальный код физической клавиши.
        key_code: u16,
        mods: KeyMods,
        /// Автоповтор удержанной клавиши.
        repeat: bool,
    },
}

struct SinkModel {
    id: ComponentId,
    on_event: Box<dyn Fn(KeySinkEvent)>,
}

/// Приёмник клавиш на главном потоке.
pub struct KeySink {
    view: View<SinkView, SinkModel>,
}

impl KeySink {
    /// `on_event` вызывается на главном потоке изнутри AppKit и не должен трогать окно.
    pub fn new(app: &App, on_event: impl Fn(KeySinkEvent) + 'static) -> KeySink {
        let mtm = app.mtm();
        let model = SinkModel {
            id: ComponentId::next(),
            on_event: Box::new(on_event),
        };
        let view = View::new(&SINK_OWNER, model, "KeySink.event", |owner| {
            SinkView::new(mtm, owner)
        });
        KeySink { view }
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    /// Отдаёт виду клавиатурный фокус. `false` — вид не в окне или окно отказало.
    pub fn focus(&self) -> bool {
        let view: &NSView = self.view.view();
        view.window()
            .is_some_and(|window| window.makeFirstResponder(Some(view)))
    }

    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        vec![inspect::node(
            id,
            "KeySink",
            parent,
            self.view.view(),
            None,
            None,
        )]
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }
}

struct SinkIvars {
    _live: Live,
    owner: Callback<SinkModel>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioKeySink"]
    #[ivars = SinkIvars]
    struct SinkView;

    impl SinkView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `acceptsFirstResponder` NSResponder.
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `canBecomeKeyView` NSView: вид вне цепочки `Tab`.
        #[unsafe(method(canBecomeKeyView))]
        fn can_become_key_view(&self) -> bool {
            false
        }

        // SAFETY: селектор и результат — как у `becomeFirstResponder` NSResponder.
        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: `becomeFirstResponder` у NSView без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if accepted {
                self.emit(KeySinkEvent::Focus(true));
            }
            accepted
        }

        // SAFETY: селектор и результат — как у `resignFirstResponder` NSResponder.
        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            // SAFETY: `resignFirstResponder` у NSView без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), resignFirstResponder] };
            if accepted {
                self.emit(KeySinkEvent::Focus(false));
            }
            accepted
        }

        // SAFETY: селектор и аргумент — как у `keyDown:` NSResponder. Клавиша дальше по цепочке
        // ответчиков не идёт: необработанная она дала бы системный сигнал.
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.emit(KeySinkEvent::Key {
                down: true,
                key_code: event.keyCode(),
                mods: key_monitor::mods(event.modifierFlags()),
                repeat: event.isARepeat(),
            });
        }

        // SAFETY: селектор и аргумент — как у `keyUp:` NSResponder.
        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.emit(KeySinkEvent::Key {
                down: false,
                key_code: event.keyCode(),
                mods: key_monitor::mods(event.modifierFlags()),
                repeat: false,
            });
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder. Щелчок отдаёт виду фокус;
        // у вида, который уже первый ответчик, окно ничего не меняет.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            let view: &NSView = self;
            if let Some(window) = view.window() {
                window.makeFirstResponder(Some(view));
            }
        }
    }
);

impl SinkView {
    fn new(mtm: MainThreadMarker, owner: Callback<SinkModel>) -> Retained<SinkView> {
        let this = SinkView::alloc(mtm).set_ivars(SinkIvars {
            _live: SINK_VIEW.live(),
            owner,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
    }

    fn emit(&self, event: KeySinkEvent) {
        self.ivars().owner.call(|model| (model.on_event)(event));
    }
}
