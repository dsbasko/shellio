//! Время ядра: миллисекунды Unix и внедряемые часы.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Момент времени в миллисекундах от начала эпохи Unix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixMillis(pub i64);

/// Источник текущего времени. Обработчики команд время сами не читают.
pub trait Clock: Send + Sync {
    /// Текущий момент.
    fn now(&self) -> UnixMillis;
}

/// Системные часы продукта.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> UnixMillis {
        let millis = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
            Err(before) => i64::try_from(before.duration().as_millis())
                .map(|millis| -millis)
                .unwrap_or(i64::MIN),
        };
        UnixMillis(millis)
    }
}

/// Ручные часы для тестов. Клоны делят одно показание.
#[cfg(feature = "test-support")]
#[derive(Clone, Debug, Default)]
pub struct ManualClock {
    millis: std::sync::Arc<std::sync::atomic::AtomicI64>,
}

#[cfg(feature = "test-support")]
impl ManualClock {
    /// Часы, показывающие `start`.
    pub fn new(start: UnixMillis) -> Self {
        Self {
            millis: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(start.0)),
        }
    }

    /// Переставить показание на `now`.
    pub fn set(&self, now: UnixMillis) {
        self.millis
            .store(now.0, std::sync::atomic::Ordering::SeqCst);
    }

    /// Сдвинуть показание на `millis` миллисекунд.
    pub fn advance(&self, millis: i64) {
        self.millis
            .fetch_add(millis, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(feature = "test-support")]
impl Clock for ManualClock {
    fn now(&self) -> UnixMillis {
        UnixMillis(self.millis.load(std::sync::atomic::Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_is_shared_between_clones() {
        let clock = ManualClock::new(UnixMillis(1_000));
        let other = clock.clone();
        assert_eq!(other.now(), UnixMillis(1_000));
        clock.advance(250);
        assert_eq!(other.now(), UnixMillis(1_250));
        other.set(UnixMillis(5));
        assert_eq!(clock.now(), UnixMillis(5));
    }

    #[test]
    fn system_clock_is_after_2026() {
        assert!(SystemClock.now() > UnixMillis(1_767_225_600_000));
    }

    #[test]
    fn unix_millis_is_a_json_number() {
        assert_eq!(serde_json::to_string(&UnixMillis(-3)).unwrap(), "-3");
        assert_eq!(
            serde_json::from_str::<UnixMillis>("42").unwrap(),
            UnixMillis(42)
        );
    }
}
