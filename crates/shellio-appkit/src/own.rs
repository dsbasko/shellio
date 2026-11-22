//! Владение видами (инварианты владения P0.2).
//!
//! - [`View`] держит вид и его Rust-часть. Источник данных, делегат и цели действий, которые
//!   AppKit хранит слабо, держит владелец сильно и отвязывает в `Drop` до освобождения
//!   ([`Detach`]), затем вид уходит из иерархии.
//! - Обратный вызов из Objective-C достаёт владельца через [`Callback`] со слабой ссылкой. Вызов без
//!   живого владельца ничего не делает и прибавляет `late_callbacks` своего места.
//! - От вида к Rust-части сильных ссылок нет: вид получает [`Callback`] при создании.

use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

use objc2::Message;
use objc2::rc::Retained;
use objc2_app_kit::NSView;

use crate::counters::{self, Counter, Live};

/// Идентификатор компонента, уникальный в пределах процесса: отметки отрисовки, `ui.inspect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentId(u64);

impl ComponentId {
    pub(crate) fn next() -> ComponentId {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        ComponentId(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Слабая ссылка обратного вызова на владельца с местом вызова для `late_callbacks`.
pub(crate) struct Callback<T> {
    owner: Weak<T>,
    site: &'static str,
}

impl<T> Callback<T> {
    pub(crate) fn new(owner: &Rc<T>, site: &'static str) -> Callback<T> {
        Callback {
            owner: Rc::downgrade(owner),
            site,
        }
    }

    /// Вызывает `f` с живым владельцем. Без владельца вызов поздний: `f` не вызывается.
    pub(crate) fn call<R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        match self.owner.upgrade() {
            Some(owner) => Some(f(&owner)),
            None => {
                counters::late(self.site);
                None
            }
        }
    }
}

impl<T> Clone for Callback<T> {
    fn clone(&self) -> Self {
        Callback {
            owner: Weak::clone(&self.owner),
            site: self.site,
        }
    }
}

/// Шаги отвязки: выполняются ровно один раз, в обратном порядке добавления, явным
/// [`Detach::run`] или в `Drop`. Шаг держит то, что отвязывает, и отпускает это после отвязки.
#[derive(Default)]
pub(crate) struct Detach {
    steps: Vec<Box<dyn FnOnce()>>,
}

impl Detach {
    pub(crate) fn push(&mut self, step: impl FnOnce() + 'static) {
        self.steps.push(Box::new(step));
    }

    pub(crate) fn run(&mut self) {
        while let Some(step) = self.steps.pop() {
            step();
        }
    }
}

impl Drop for Detach {
    fn drop(&mut self) {
        self.run();
    }
}

/// Владелец вида и его Rust-части. В `Drop` порядок такой: шаги [`Detach`] (источник, делегат,
/// цели), вид уходит из иерархии, освобождается Rust-часть, затем вид.
pub(crate) struct View<V: Message + AsRef<NSView>, T> {
    detach: Detach,
    rust: Rc<T>,
    view: Retained<V>,
    _live: Live,
}

impl<V: Message + AsRef<NSView>, T> View<V, T> {
    /// Строит Rust-часть, затем вид: `make` получает слабую ссылку на Rust-часть для обратных
    /// вызовов вида с местом `site`.
    pub(crate) fn new(
        counter: &'static Counter,
        rust: T,
        site: &'static str,
        make: impl FnOnce(Callback<T>) -> Retained<V>,
    ) -> View<V, T> {
        let rust = Rc::new(rust);
        let view = make(Callback::new(&rust, site));
        View {
            detach: Detach::default(),
            rust,
            view,
            _live: counter.live(),
        }
    }

    pub(crate) fn rust(&self) -> &T {
        &self.rust
    }

    pub(crate) fn view(&self) -> &V {
        &self.view
    }
}

impl<V: Message + AsRef<NSView>, T> Drop for View<V, T> {
    fn drop(&mut self) {
        self.detach.run();
        (*self.view).as_ref().removeFromSuperview();
    }
}

pub(crate) use action::Targets;

/// Цели действий кнопок и пунктов меню. AppKit хранит цель слабо, поэтому её держит владелец
/// (инвариант 2) и в `Drop` снимает с контрола до освобождения. Владелец с видом привязывает
/// цели через [`View::bind_action`], владелец без вида (главное меню, `Menu`) — через [`Targets`].
mod action {
    use objc2::rc::{self, Retained};
    use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
    use objc2::{
        DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel,
    };
    use objc2_app_kit::{NSControl, NSMenuItem, NSView};

    use super::{Callback, Detach, View};
    use crate::counters::{Live, counter};

    counter!(ACTION_TARGET, Class, "ShellioActionTarget");

    /// Контрол с целью и действием: `NSControl` или `NSMenuItem`.
    pub(crate) trait Actionable: Message {
        /// Ставит цель и её действие `shellioPerform:`; `None` снимает оба.
        fn set_action_target(&self, target: Option<&ActionTarget>);
    }

    impl Actionable for NSControl {
        fn set_action_target(&self, target: Option<&ActionTarget>) {
            // SAFETY: цель — объект ActionTarget, который реализует `shellioPerform:`; её держит
            // владелец до снятия, селектор и цель меняются вместе.
            unsafe {
                self.setTarget(target.map(|target| target.as_ref()));
                self.setAction(target.map(|_| sel!(shellioPerform:)));
            }
        }
    }

    impl Actionable for NSMenuItem {
        fn set_action_target(&self, target: Option<&ActionTarget>) {
            // SAFETY: как у NSControl: цель реализует `shellioPerform:` и живёт до снятия.
            unsafe {
                self.setTarget(target.map(|target| target.as_ref()));
                self.setAction(target.map(|_| sel!(shellioPerform:)));
            }
        }
    }

    pub(crate) struct ActionIvars {
        _live: Live,
        perform: Box<dyn Fn()>,
    }

    define_class!(
        // SAFETY: NSObject не требует от подкласса ничего сверх `init`, `Drop` у класса нет.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "ShellioActionTarget"]
        #[ivars = ActionIvars]
        pub(crate) struct ActionTarget;

        // SAFETY: протокол NSObject реализует суперкласс NSObject.
        unsafe impl NSObjectProtocol for ActionTarget {}

        impl ActionTarget {
            // SAFETY: действие AppKit: один аргумент-отправитель, результата нет.
            #[unsafe(method(shellioPerform:))]
            fn perform(&self, _sender: Option<&AnyObject>) {
                (self.ivars().perform)();
            }
        }
    );

    impl ActionTarget {
        fn new(mtm: MainThreadMarker, perform: Box<dyn Fn()>) -> Retained<ActionTarget> {
            let this = ActionTarget::alloc(mtm).set_ivars(ActionIvars {
                _live: ACTION_TARGET.live(),
                perform,
            });
            // SAFETY: `init` у NSObject без аргументов, ivars уже заданы.
            unsafe { msg_send![super(this), init] }
        }
    }

    /// Ставит на контрол цель с действием `perform`; отвязка в `detach` снимает её с контрола, если
    /// тот ещё жив, и отпускает цель.
    fn bind<C: Actionable + 'static>(
        detach: &mut Detach,
        mtm: MainThreadMarker,
        control: &C,
        perform: Box<dyn Fn()>,
    ) {
        let target = ActionTarget::new(mtm, perform);
        control.set_action_target(Some(&target));
        let control = rc::Weak::new(control);
        detach.push(move || {
            if let Some(control) = control.load() {
                control.set_action_target(None);
            }
            drop(target);
        });
    }

    impl<V: Message + AsRef<NSView> + MainThreadOnly, T: 'static> View<V, T> {
        /// Привязывает действие контрола к Rust-части: `perform` получает её, пока она жива.
        /// Цель держит владелец, отвязка снимает её с контрола, если тот ещё жив.
        pub(crate) fn bind_action<C: Actionable + 'static>(
            &mut self,
            control: &C,
            site: &'static str,
            perform: impl Fn(&T) + 'static,
        ) {
            let callback = Callback::new(&self.rust, site);
            let mtm = self.view.mtm();
            bind(
                &mut self.detach,
                mtm,
                control,
                Box::new(move || {
                    callback.call(&perform);
                }),
            );
        }
    }

    /// Цели владельца без вида. `Drop` снимает их с контролов до освобождения целей.
    pub(crate) struct Targets {
        mtm: MainThreadMarker,
        detach: Detach,
    }

    impl Targets {
        pub(crate) fn new(mtm: MainThreadMarker) -> Targets {
            Targets {
                mtm,
                detach: Detach::default(),
            }
        }

        /// Привязывает действие контрола к `perform`. Слабую ссылку на получателя `perform`
        /// держит сам: цель не продлевает жизнь владельца.
        pub(crate) fn bind<C: Actionable + 'static>(
            &mut self,
            control: &C,
            perform: impl Fn() + 'static,
        ) {
            bind(&mut self.detach, self.mtm, control, Box::new(perform));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn detach_runs_steps_once_in_reverse_order() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut detach = Detach::default();
        for step in 1..=3 {
            let log = Rc::clone(&log);
            detach.push(move || log.borrow_mut().push(step));
        }
        detach.run();
        assert_eq!(*log.borrow(), [3, 2, 1]);
        detach.run();
        drop(detach);
        assert_eq!(*log.borrow(), [3, 2, 1], "повторной отвязки нет");
    }

    #[test]
    fn detach_in_drop_unbinds_before_releasing_what_it_holds() {
        struct Held(Rc<RefCell<Vec<&'static str>>>);
        impl Drop for Held {
            fn drop(&mut self) {
                self.0.borrow_mut().push("released");
            }
        }
        let log = Rc::new(RefCell::new(Vec::new()));
        let held = Held(Rc::clone(&log));
        let mut detach = Detach::default();
        let step_log = Rc::clone(&log);
        detach.push(move || {
            step_log.borrow_mut().push("unbound");
            drop(held);
        });
        assert!(log.borrow().is_empty(), "до Drop ничего не отвязано");
        drop(detach);
        assert_eq!(*log.borrow(), ["unbound", "released"]);
    }

    #[test]
    fn callback_reaches_live_owner_and_counts_late_calls() {
        let site = "test.own.callback";
        let late = || {
            counters::late_callbacks()
                .iter()
                .find(|late| late.site == site)
                .map_or(0, |late| late.count)
        };
        let owner = Rc::new(41);
        let callback = Callback::new(&owner, site);
        let copy = callback.clone();
        assert_eq!(callback.call(|value| value + 1), Some(42));
        assert_eq!(late(), 0);
        assert_eq!(
            Rc::strong_count(&owner),
            1,
            "обратный вызов не держит владельца"
        );
        drop(owner);
        assert_eq!(callback.call(|value| value + 1), None);
        assert_eq!(copy.call(|_| unreachable!("владельца нет")), None::<()>);
        assert_eq!(late(), 2);
    }

    #[test]
    fn component_ids_are_unique_and_increasing() {
        let first = ComponentId::next();
        let second = ComponentId::next();
        assert!(second > first);
        assert_ne!(first.get(), 0);
    }
}
