//! Подписка `NSNotificationCenter` как значение: отписка в `Drop` (инвариант 4 P0.2).
//!
//! Центр хранит наблюдателя слабо и сам его не снимает, поэтому подписку снимает её владелец.
//! Уведомление, отправленное не с главного потока, наблюдатель переотправляет себе на главный
//! (`performSelectorOnMainThread`): обработчик выполняется только там.

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSNotificationName, NSObjectNSThreadPerformAdditions,
};

use crate::counters::{Live, counter};
use crate::own::Callback;

counter!(NOTIFICATION_OBSERVER, Class, "ShellioNotificationObserver");

/// Действующая подписка. [`Subscription::cancel`] и `Drop` снимают её ровно один раз.
pub(crate) struct Subscription {
    cancel: Option<Box<dyn FnOnce()>>,
}

impl Subscription {
    /// Подписка, которую снимает `cancel`.
    fn from_cancel(cancel: impl FnOnce() + 'static) -> Subscription {
        Subscription {
            cancel: Some(Box::new(cancel)),
        }
    }

    /// Подписывает владельца `owner` на уведомление `name` центра `center`. `on` вызывается на
    /// главном потоке, пока владелец жив; уведомление после его освобождения — поздний вызов.
    pub(crate) fn notification<T: 'static>(
        mtm: MainThreadMarker,
        center: &NSNotificationCenter,
        name: &NSNotificationName,
        owner: Callback<T>,
        on: impl Fn(&T) + 'static,
    ) -> Subscription {
        let observer = NotificationObserver::new(
            mtm,
            Box::new(move || {
                owner.call(&on);
            }),
        );
        // SAFETY: `shellioNotified:` реализован наблюдателем и принимает уведомление; центр держит
        // наблюдателя слабо, а подписка снимается в отписке ниже до освобождения наблюдателя.
        unsafe {
            center.addObserver_selector_name_object(
                &observer,
                sel!(shellioNotified:),
                Some(name),
                None,
            );
        }
        let center = center.retain();
        Subscription::from_cancel(move || {
            // SAFETY: снимаются все подписки этого наблюдателя, он жив до конца замыкания.
            unsafe { center.removeObserver(&observer) };
        })
    }

    /// Снимает подписку. `true` — снята этим вызовом, `false` — уже была снята.
    pub(crate) fn cancel(&mut self) -> bool {
        match self.cancel.take() {
            Some(cancel) => {
                cancel();
                true
            }
            None => false,
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct ObserverIvars {
    _live: Live,
    on: Box<dyn Fn()>,
}

define_class!(
    // SAFETY: NSObject не требует от подкласса ничего сверх `init`, `Drop` у класса нет.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioNotificationObserver"]
    #[ivars = ObserverIvars]
    struct NotificationObserver;

    // SAFETY: протокол NSObject реализует суперкласс NSObject.
    unsafe impl NSObjectProtocol for NotificationObserver {}

    impl NotificationObserver {
        // SAFETY: селектор подписки: один аргумент-уведомление, результата нет.
        #[unsafe(method(shellioNotified:))]
        fn notified(&self, notification: &NSNotification) {
            if MainThreadMarker::new().is_none() {
                // Обработчик и его данные трогаются только на главном потоке. Отправка держит
                // наблюдателя и уведомление до выполнения.
                // SAFETY: селектор реализован этим классом и принимает уведомление.
                unsafe {
                    self.performSelectorOnMainThread_withObject_waitUntilDone(
                        sel!(shellioNotified:),
                        Some(notification.as_ref()),
                        false,
                    );
                }
                return;
            }
            (self.ivars().on)();
        }
    }
);

impl NotificationObserver {
    fn new(mtm: MainThreadMarker, on: Box<dyn Fn()>) -> Retained<NotificationObserver> {
        let this = NotificationObserver::alloc(mtm).set_ivars(ObserverIvars {
            _live: NOTIFICATION_OBSERVER.live(),
            on,
        });
        // SAFETY: `init` у NSObject без аргументов, ivars уже заданы.
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    fn counting() -> (Rc<Cell<u32>>, Subscription) {
        let calls = Rc::new(Cell::new(0));
        let seen = Rc::clone(&calls);
        (
            calls,
            Subscription::from_cancel(move || seen.set(seen.get() + 1)),
        )
    }

    #[test]
    fn cancel_unsubscribes_once() {
        let (calls, mut subscription) = counting();
        assert!(subscription.cancel());
        assert!(!subscription.cancel());
        drop(subscription);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn drop_unsubscribes_active_subscription() {
        let (calls, subscription) = counting();
        assert_eq!(calls.get(), 0);
        drop(subscription);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn unsubscribe_releases_what_it_holds() {
        let held = Rc::new(());
        let inside = Rc::clone(&held);
        let mut subscription = Subscription::from_cancel(move || drop(inside));
        assert_eq!(Rc::strong_count(&held), 2);
        subscription.cancel();
        assert_eq!(Rc::strong_count(&held), 1);
    }
}
