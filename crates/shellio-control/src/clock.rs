//! Сроки управления: абсолютный срок вызова и ожидание до следующей попытки.
//!
//! Каждая операция ввода-вывода клиента и каждый шаг завершения владельца идут под
//! [`Clock::within`] до одного абсолютного срока, повторы — через [`Clock::wait_retry`].

use std::fmt;
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

use tokio::time::Instant;

/// Интервал между попытками в продукте.
pub const RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Срок дальше любого разумного вызова: на него заменяется срок, не помещающийся в часы.
const FAR_FUTURE: Duration = Duration::from_secs(30 * 365 * 24 * 60 * 60);

/// Срок истёк.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expired;

impl fmt::Display for Expired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("срок истёк")
    }
}

impl std::error::Error for Expired {}

/// Порт часов управления.
pub trait Clock: Send + Sync + 'static {
    /// Абсолютный срок.
    type Deadline: Copy + Send + Sync + fmt::Debug + 'static;

    /// Срок через `timeout` от текущего момента.
    fn deadline(&self, timeout: Duration) -> Self::Deadline;

    /// Ждать до следующей попытки или до срока, что раньше. `Ok` — пора повторить,
    /// `Err` — срок истёк, попыток больше нет.
    fn wait_retry(
        &self,
        deadline: Self::Deadline,
    ) -> impl Future<Output = Result<(), Expired>> + Send;

    /// Выполнить `future` не дольше срока. По сроку `future` отбрасывается.
    fn within<F>(
        &self,
        deadline: Self::Deadline,
        future: F,
    ) -> impl Future<Output = Result<F::Output, Expired>> + Send
    where
        F: Future + Send,
        F::Output: Send;
}

/// Часы продукта на `tokio::time` с интервалом попыток [`RETRY_INTERVAL`]. Под
/// `tokio::time::pause` ими управляют `advance` и автоматический сдвиг простаивающего runtime.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioClock;

/// `start + span`, а если сумма не помещается в часы — далёкое будущее.
fn saturating_add(start: Instant, span: Duration) -> Instant {
    start
        .checked_add(span)
        .or_else(|| start.checked_add(FAR_FUTURE))
        .unwrap_or(start)
}

impl Clock for TokioClock {
    type Deadline = Instant;

    fn deadline(&self, timeout: Duration) -> Instant {
        saturating_add(Instant::now(), timeout)
    }

    async fn wait_retry(&self, deadline: Instant) -> Result<(), Expired> {
        let next = saturating_add(Instant::now(), RETRY_INTERVAL);
        if next >= deadline {
            tokio::time::sleep_until(deadline).await;
            Err(Expired)
        } else {
            tokio::time::sleep_until(next).await;
            Ok(())
        }
    }

    async fn within<F>(&self, deadline: Instant, future: F) -> Result<F::Output, Expired>
    where
        F: Future + Send,
        F::Output: Send,
    {
        // `timeout_at` опрашивает операцию раньше таймера, а таймер готов только после хода
        // драйвера: по истёкшему сроку операция успела бы дописать запрос. Поэтому срок
        // сверяется с часами перед каждым опросом, а таймер только будит задачу.
        let mut future = pin!(future);
        let guarded = poll_fn(move |cx| {
            if Instant::now() >= deadline {
                Poll::Ready(Err(Expired))
            } else {
                future.as_mut().poll(cx).map(Ok)
            }
        });
        tokio::time::timeout_at(deadline, guarded)
            .await
            .unwrap_or(Err(Expired))
    }
}

#[cfg(feature = "test-support")]
pub(crate) mod manual {
    use std::future::Future;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::watch;

    use super::{Clock, Expired};

    /// Срок ручных часов: номер в порядке выдачи.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub struct ManualDeadline(u64);

    #[derive(Debug, Default)]
    struct State {
        /// Номер следующего выданного срока.
        issued: u64,
        /// Сроки с номером меньше этого истекли.
        expired_below: u64,
        /// Начатые ожидания попытки.
        waits: u64,
        /// Разрешённые ожидания попытки.
        ticks: u64,
    }

    impl State {
        fn is_expired(&self, deadline: ManualDeadline) -> bool {
            deadline.0 < self.expired_below
        }
    }

    /// Ручные часы с барьером: тест решает, когда ожидание попытки продолжается и когда
    /// истекают сроки. Реального времени они не читают. Клоны делят одно состояние.
    #[derive(Clone, Debug)]
    pub struct ManualClock {
        state: Arc<watch::Sender<State>>,
    }

    impl Default for ManualClock {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ManualClock {
        /// Часы без выданных сроков и ожиданий.
        pub fn new() -> Self {
            Self {
                state: Arc::new(watch::Sender::new(State::default())),
            }
        }

        /// Сколько ожиданий попытки начато.
        pub fn waits(&self) -> u64 {
            self.state.borrow().waits
        }

        /// Дождаться, пока начнётся `count`-е ожидание попытки.
        pub async fn until_waiting(&self, count: u64) {
            let mut receiver = self.state.subscribe();
            // Отправитель живёт в `self`, поэтому канал не закрывается.
            let _ = receiver.wait_for(|state| state.waits >= count).await;
        }

        /// Разрешить следующее по порядку ожидание попытки: оно вернёт `Ok`. Разрешение,
        /// выданное заранее, достаётся следующему начатому ожиданию.
        pub fn tick(&self) {
            self.state.send_modify(|state| state.ticks += 1);
        }

        /// Истечь все выданные сроки: ждущие и будущие ожидания и операции под ними
        /// получают `Expired`. Сроки, выданные после вызова, не истекли.
        pub fn expire(&self) {
            self.state
                .send_modify(|state| state.expired_below = state.issued);
        }
    }

    impl Clock for ManualClock {
        type Deadline = ManualDeadline;

        fn deadline(&self, _timeout: Duration) -> ManualDeadline {
            let mut deadline = ManualDeadline(0);
            self.state.send_modify(|state| {
                deadline = ManualDeadline(state.issued);
                state.issued += 1;
            });
            deadline
        }

        fn wait_retry(
            &self,
            deadline: ManualDeadline,
        ) -> impl Future<Output = Result<(), Expired>> + Send {
            let state = Arc::clone(&self.state);
            async move {
                let mut ticket = None;
                state.send_if_modified(|state| {
                    if state.is_expired(deadline) {
                        return false;
                    }
                    state.waits += 1;
                    ticket = Some(state.waits);
                    true
                });
                let Some(ticket) = ticket else {
                    return Err(Expired);
                };
                let mut receiver = state.subscribe();
                let expired = match receiver
                    .wait_for(|state| state.is_expired(deadline) || state.ticks >= ticket)
                    .await
                {
                    Ok(state) => state.is_expired(deadline),
                    Err(_) => true,
                };
                if expired { Err(Expired) } else { Ok(()) }
            }
        }

        fn within<F>(
            &self,
            deadline: ManualDeadline,
            future: F,
        ) -> impl Future<Output = Result<F::Output, Expired>> + Send
        where
            F: Future + Send,
            F::Output: Send,
        {
            let mut receiver = self.state.subscribe();
            async move {
                // Истёкший срок проверяется раньше операции: тест, истёкший срок до вызова,
                // получает отказ без попытки ввода-вывода.
                tokio::select! {
                    biased;
                    _ = receiver.wait_for(|state| state.is_expired(deadline)) => Err(Expired),
                    output = future => Ok(output),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn tokio_clock_retries_until_deadline() {
        let clock = TokioClock;
        let start = Instant::now();
        let deadline = clock.deadline(Duration::from_millis(120));

        assert_eq!(clock.wait_retry(deadline).await, Ok(()));
        assert_eq!(start.elapsed(), Duration::from_millis(50));
        assert_eq!(clock.wait_retry(deadline).await, Ok(()));
        assert_eq!(start.elapsed(), Duration::from_millis(100));
        assert_eq!(clock.wait_retry(deadline).await, Err(Expired));
        assert_eq!(start.elapsed(), Duration::from_millis(120));
        assert_eq!(clock.wait_retry(deadline).await, Err(Expired));
        assert_eq!(start.elapsed(), Duration::from_millis(120));
    }

    #[tokio::test(start_paused = true)]
    async fn tokio_clock_bounds_operation_by_deadline() {
        let clock = TokioClock;
        let start = Instant::now();
        let deadline = clock.deadline(Duration::from_millis(300));

        assert_eq!(clock.within(deadline, async { 7 }).await, Ok(7));
        assert_eq!(clock.within(deadline, pending::<()>()).await, Err(Expired));
        assert_eq!(start.elapsed(), Duration::from_millis(300));
    }

    #[tokio::test(start_paused = true)]
    async fn tokio_clock_cuts_operation_after_deadline_before_it_runs() {
        let clock = TokioClock;
        let deadline = clock.deadline(Duration::from_millis(100));
        tokio::time::advance(Duration::from_millis(100)).await;

        let mut ran = false;
        let result = clock.within(deadline, async { ran = true }).await;
        assert_eq!(result, Err(Expired));
        assert!(!ran);
    }

    #[tokio::test(start_paused = true)]
    async fn tokio_clock_cuts_pending_operation_ready_after_deadline() {
        let clock = TokioClock;
        let deadline = clock.deadline(Duration::from_millis(100));

        let mut ran = false;
        let result = clock
            .within(deadline, async {
                tokio::time::advance(Duration::from_millis(100)).await;
                ran = true;
            })
            .await;
        assert_eq!(result, Err(Expired));
        assert!(!ran);
    }

    #[tokio::test(start_paused = true)]
    async fn tokio_clock_saturates_huge_timeout() {
        let clock = TokioClock;
        let deadline = clock.deadline(Duration::MAX);
        assert!(deadline > Instant::now() + Duration::from_secs(365 * 24 * 60 * 60));
        assert_eq!(clock.wait_retry(deadline).await, Ok(()));
    }

    mod manual {
        use std::future::pending;
        use std::time::Duration;

        use tokio::sync::oneshot;

        use crate::clock::manual::ManualClock;
        use crate::clock::{Clock, Expired};

        #[tokio::test]
        async fn tick_lets_waits_continue_in_order() {
            let clock = ManualClock::new();
            let deadline = clock.deadline(Duration::from_secs(1));
            let waiter = tokio::spawn({
                let clock = clock.clone();
                async move {
                    let first = clock.wait_retry(deadline).await;
                    let second = clock.wait_retry(deadline).await;
                    (first, second)
                }
            });

            clock.until_waiting(1).await;
            clock.tick();
            clock.until_waiting(2).await;
            clock.tick();
            assert_eq!(waiter.await.unwrap(), (Ok(()), Ok(())));
            assert_eq!(clock.waits(), 2);
        }

        #[tokio::test]
        async fn expire_ends_pending_wait_and_operation() {
            let clock = ManualClock::new();
            let deadline = clock.deadline(Duration::from_secs(1));
            let (started_tx, started_rx) = oneshot::channel();
            let operation = tokio::spawn({
                let clock = clock.clone();
                async move {
                    clock
                        .within(deadline, async move {
                            let _ = started_tx.send(());
                            pending::<()>().await
                        })
                        .await
                }
            });
            let waiter = tokio::spawn({
                let clock = clock.clone();
                async move { clock.wait_retry(deadline).await }
            });

            started_rx.await.unwrap();
            clock.until_waiting(1).await;
            clock.expire();
            assert_eq!(operation.await.unwrap(), Err(Expired));
            assert_eq!(waiter.await.unwrap(), Err(Expired));
            assert_eq!(clock.wait_retry(deadline).await, Err(Expired));
            assert_eq!(clock.waits(), 1);
        }

        #[tokio::test]
        async fn expired_deadline_cuts_operation_before_it_runs() {
            let clock = ManualClock::new();
            let deadline = clock.deadline(Duration::from_secs(1));
            clock.expire();
            let mut ran = false;
            let result = clock.within(deadline, async { ran = true }).await;
            assert_eq!(result, Err(Expired));
            assert!(!ran);
        }

        #[tokio::test]
        async fn deadline_issued_after_expire_is_fresh() {
            let clock = ManualClock::new();
            let old = clock.deadline(Duration::from_secs(1));
            clock.expire();
            let fresh = clock.deadline(Duration::from_secs(1));
            assert_eq!(clock.within(old, async { 1 }).await, Err(Expired));
            assert_eq!(clock.within(fresh, async { 2 }).await, Ok(2));
            clock.tick();
            assert_eq!(clock.wait_retry(fresh).await, Ok(()));
        }
    }
}
