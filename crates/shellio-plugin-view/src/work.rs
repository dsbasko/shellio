//! Работа, привязанная к виду: закрытие тела отменяет её, а работа без токена живёт по своим
//! правилам (ADR-0008, «Закрыть вид и остановить работу — разные действия»).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Источник токенов работы одного тела. Отменяет его хост при закрытии тела ([`crate::Body`]).
#[derive(Clone, Debug, Default)]
pub struct ViewWork {
    cancelled: Arc<AtomicBool>,
}

impl ViewWork {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Токен для работы вне главного потока. Токен, выданный после закрытия тела, уже отменён.
    #[must_use]
    pub fn token(&self) -> WorkToken {
        WorkToken {
            cancelled: Arc::clone(&self.cancelled),
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

/// Токен работы вида: работа проверяет его между шагами и перед отправкой результата.
#[derive(Clone, Debug)]
pub struct WorkToken {
    cancelled: Arc<AtomicBool>,
}

impl WorkToken {
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_crosses_threads() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<WorkToken>();
        assert_send_sync::<ViewWork>();
    }

    #[test]
    fn cancel_reaches_every_token_issued_before_and_after() {
        let work = ViewWork::new();
        let before = work.token();
        let copy = before.clone();
        assert!(!work.is_cancelled() && !before.is_cancelled() && !copy.is_cancelled());

        work.cancel();
        assert!(work.is_cancelled() && before.is_cancelled() && copy.is_cancelled());
        assert!(
            work.token().is_cancelled(),
            "токен после отмены уже отменён"
        );
        assert!(
            !ViewWork::new().token().is_cancelled(),
            "другое тело отмену не видит"
        );
    }
}
