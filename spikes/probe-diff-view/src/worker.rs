//! Отменяемый фоновый исполнитель с номерами поколений. Модуль не зависит от AppKit.
//!
//! Задания идут по очереди на одном потоке. Новое задание заменяет прежнее: номер текущего
//! поколения растёт, и задание старого поколения видит отмену на ближайшей проверке
//! [`Cancel::is_cancelled`]. Результат отменённого задания не доставляется. Между последней
//! проверкой и доставкой поколение может смениться ещё раз, поэтому получатель сам сверяет
//! [`Delivery::generation`] с текущим.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

/// Признак отмены задания одного поколения.
#[derive(Clone, Debug)]
pub struct Cancel {
    current: Arc<AtomicU64>,
    generation: u64,
}

impl Cancel {
    /// Поколение, которое никогда не отменяется: для разбора вне исполнителя.
    pub fn never() -> Self {
        Cancel {
            current: Arc::new(AtomicU64::new(0)),
            generation: 0,
        }
    }

    /// Уже отменённое поколение: проверка отмены без потока исполнителя.
    #[cfg(test)]
    pub fn cancelled() -> Self {
        Cancel {
            current: Arc::new(AtomicU64::new(1)),
            generation: 0,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_cancelled(&self) -> bool {
        self.current.load(Ordering::Acquire) != self.generation
    }
}

/// Результат задания вместе с его поколением.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery<T> {
    pub generation: u64,
    pub value: T,
}

/// Поток исполнителя завершился, задание не принято.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stopped;

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "поток исполнителя завершился")
    }
}

impl std::error::Error for Stopped {}

/// Исполнитель заданий типа `I`. `Drop` отменяет текущее задание и ждёт поток: задание
/// проверяет отмену часто, поэтому ожидание короткое. Колбэк доставки не должен ждать того
/// потока, который освобождает исполнитель, иначе `Drop` не дождётся конца задания.
pub struct Worker<I: Send + 'static> {
    current: Arc<AtomicU64>,
    jobs: Option<Sender<(u64, I)>>,
    thread: Option<JoinHandle<()>>,
}

impl<I: Send + 'static> Worker<I> {
    /// `job` возвращает `None`, если увидел отмену. `deliver` вызывается на потоке исполнителя.
    pub fn spawn<O, J, D>(name: &str, job: J, deliver: D) -> io::Result<Self>
    where
        O: Send + 'static,
        J: Fn(I, &Cancel) -> Option<O> + Send + 'static,
        D: Fn(Delivery<O>) + Send + 'static,
    {
        let current = Arc::new(AtomicU64::new(0));
        let (jobs, queue) = mpsc::channel::<(u64, I)>();
        let shared = Arc::clone(&current);
        let thread = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                for (generation, input) in queue {
                    let cancel = Cancel {
                        current: Arc::clone(&shared),
                        generation,
                    };
                    if cancel.is_cancelled() {
                        continue;
                    }
                    if let Some(value) = job(input, &cancel)
                        && !cancel.is_cancelled()
                    {
                        deliver(Delivery { generation, value });
                    }
                }
            })?;
        Ok(Worker {
            current,
            jobs: Some(jobs),
            thread: Some(thread),
        })
    }

    /// Ставит задание нового поколения и отменяет прежнее.
    pub fn submit(&self, input: I) -> Result<u64, Stopped> {
        let generation = self.current.fetch_add(1, Ordering::AcqRel) + 1;
        let jobs = self.jobs.as_ref().ok_or(Stopped)?;
        jobs.send((generation, input)).map_err(|_| Stopped)?;
        Ok(generation)
    }

    /// Отменяет текущее задание, не ставя нового.
    pub fn cancel(&self) {
        self.current.fetch_add(1, Ordering::AcqRel);
    }

    /// Текущее поколение: доставка с другим номером устарела.
    pub fn generation(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }
}

impl<I: Send + 'static> Drop for Worker<I> {
    fn drop(&mut self) {
        self.cancel();
        drop(self.jobs.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::Duration;

    use super::*;

    /// Предел ожидания доставки: защита от зависания теста, а не синхронизация.
    const WAIT: Duration = Duration::from_secs(10);

    fn recv<T>(rx: &Receiver<T>) -> T {
        match rx.recv_timeout(WAIT) {
            Ok(value) => value,
            Err(RecvTimeoutError::Timeout) => panic!("доставки нет за {WAIT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("канал доставки закрыт"),
        }
    }

    #[test]
    fn delivers_result_with_generation() {
        let (tx, rx) = mpsc::channel();
        let worker = Worker::spawn(
            "test-worker",
            |input: u32, _: &Cancel| Some(input * 2),
            move |delivery| {
                let _ = tx.send(delivery);
            },
        )
        .unwrap();
        assert_eq!(worker.submit(21), Ok(1));
        assert_eq!(
            recv(&rx),
            Delivery {
                generation: 1,
                value: 42
            }
        );
        assert_eq!(worker.generation(), 1);
    }

    #[test]
    fn replaced_job_sees_cancel_and_is_not_delivered() {
        let (started_tx, started_rx) = mpsc::channel::<u64>();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let (seen_tx, seen_rx) = mpsc::channel::<(u64, bool)>();
        let (tx, rx) = mpsc::channel();
        let worker = Worker::spawn(
            "test-worker",
            move |input: &'static str, cancel: &Cancel| {
                if input == "a" {
                    let _ = started_tx.send(cancel.generation());
                    let _ = gate_rx.recv();
                }
                let cancelled = cancel.is_cancelled();
                let _ = seen_tx.send((cancel.generation(), cancelled));
                if cancelled { None } else { Some(input) }
            },
            move |delivery| {
                let _ = tx.send(delivery);
            },
        )
        .unwrap();
        assert_eq!(worker.submit("a"), Ok(1));
        assert_eq!(recv(&started_rx), 1);
        assert_eq!(worker.submit("b"), Ok(2));
        gate_tx.send(()).unwrap();
        assert_eq!(recv(&seen_rx), (1, true));
        assert_eq!(recv(&seen_rx), (2, false));
        // Очередь одна и идёт по порядку: доставка задания 1 пришла бы раньше этой.
        assert_eq!(
            recv(&rx),
            Delivery {
                generation: 2,
                value: "b"
            }
        );
    }

    #[test]
    fn queued_stale_job_is_skipped() {
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let (ran_tx, ran_rx) = mpsc::channel::<u32>();
        let worker = Worker::spawn(
            "test-worker",
            move |input: u32, _: &Cancel| {
                let _ = ran_tx.send(input);
                if input == 0 {
                    let _ = started_tx.send(());
                    let _ = gate_rx.recv();
                }
                Some(input)
            },
            |_: Delivery<u32>| {},
        )
        .unwrap();
        worker.submit(0).unwrap();
        recv(&started_rx);
        worker.submit(1).unwrap();
        worker.submit(2).unwrap();
        gate_tx.send(()).unwrap();
        assert_eq!(recv(&ran_rx), 0);
        assert_eq!(recv(&ran_rx), 2);
    }

    #[test]
    fn drop_cancels_running_job_and_joins() {
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (seen_tx, seen_rx) = mpsc::channel::<bool>();
        let worker = Worker::spawn(
            "test-worker",
            move |_: (), cancel: &Cancel| {
                let _ = started_tx.send(());
                while !cancel.is_cancelled() {
                    thread::yield_now();
                }
                let _ = seen_tx.send(true);
                None::<()>
            },
            |_| {},
        )
        .unwrap();
        worker.submit(()).unwrap();
        recv(&started_rx);
        drop(worker);
        assert_eq!(seen_rx.try_recv(), Ok(true));
    }

    #[test]
    fn cancel_without_new_job_stops_current() {
        let worker = Worker::spawn("test-worker", |_: (), _: &Cancel| Some(()), |_| {}).unwrap();
        let generation = worker.submit(()).unwrap();
        worker.cancel();
        assert_ne!(worker.generation(), generation);
        assert!(!Cancel::never().is_cancelled());
    }
}
