//! Эффекты UI Rust-хоста (план P0.4, «Rust-хост»; схема — по итогам задачи 8): чтение буфера
//! обмена — типизированный вызов к GUI лидера с поколением и сроком. Колбэк чтения в C API
//! синхронный, поэтому поток разбора ждёт ответ GUI не дольше срока, пока PTY читается другим
//! потоком в ограниченную очередь. Ответ приходит в соединение GUI и доставляется сюда в обход
//! главного потока хоста, иначе ожидание кончалось бы сроком всегда.
//!
//! Правила: без GUI текущего лидера — пустой ответ сразу; смена лидера отменяет ожидание; ответ
//! чужого поколения, чужого запроса или после срока отбрасывается и считается. Модуль чистый.

use std::collections::BTreeMap;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::proto::{self, ClipStats, Codec, Message};

/// Срок ответа GUI на чтение буфера обмена (модель задачи 8).
pub const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(1);

/// Итог чтения буфера обмена.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    Answered(Vec<u8>),
    /// GUI ответил, что данных нет.
    Empty,
    NoGui,
    Timeout,
    /// Лидер сменился во время ожидания.
    Cancelled,
}

impl Read {
    pub fn data(self) -> Option<Vec<u8>> {
        match self {
            Read::Answered(data) => Some(data),
            _ => None,
        }
    }
}

/// Итог доставки ответа GUI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Accepted,
    Dropped,
}

struct Pending {
    serial: u64,
    generation: u32,
    answer: Option<Option<Vec<u8>>>,
    cancelled: bool,
}

/// Очередь кадров соединения GUI: запрос либо поставлен, либо отвергнут сразу.
pub trait FrameSink: Send + Sync {
    fn offer(&self, frame: Vec<u8>) -> bool;
}

impl FrameSink for SyncSender<Vec<u8>> {
    fn offer(&self, frame: Vec<u8>) -> bool {
        self.try_send(frame).is_ok()
    }
}

impl FrameSink for crate::host::io::ByteQueue {
    fn offer(&self, frame: Vec<u8>) -> bool {
        self.push(frame, crate::host::io::Room::Always).is_ok()
    }
}

impl<T: FrameSink + ?Sized> FrameSink for Arc<T> {
    fn offer(&self, frame: Vec<u8>) -> bool {
        (**self).offer(frame)
    }
}

struct Gui {
    generation: u32,
    codec: Codec,
    frames: Arc<dyn FrameSink>,
}

#[derive(Default)]
struct State {
    /// Поколение текущего лидера; `None` — лидера нет.
    leader: Option<u32>,
    /// Соединения GUI: поколение, от имени которого они говорят, кодек и очередь кадров.
    guis: BTreeMap<u64, Gui>,
    serial: u64,
    pending: Option<Pending>,
    stats: ClipStats,
}

pub struct Clipboard {
    state: Mutex<State>,
    wake: Condvar,
    timeout: Duration,
}

impl Clipboard {
    pub fn new(timeout: Duration) -> Clipboard {
        Clipboard {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            timeout,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn stats(&self) -> ClipStats {
        self.lock().stats
    }

    /// Соединение GUI `conn` говорит от имени поколения `generation`; его кадры пишутся кодеком
    /// `codec`.
    pub fn bind(&self, conn: u64, generation: u32, codec: Codec, frames: impl FrameSink + 'static) {
        self.lock().guis.insert(
            conn,
            Gui {
                generation,
                codec,
                frames: Arc::new(frames),
            },
        );
    }

    pub fn unbind(&self, conn: u64) {
        self.lock().guis.remove(&conn);
    }

    /// Поколение соединения GUI, если оно говорит от имени текущего лидера.
    pub fn current(&self, conn: u64) -> bool {
        let state = self.lock();
        matches!((state.guis.get(&conn), state.leader), (Some(gui), Some(leader)) if gui.generation == leader)
    }

    /// Новый лидер или его отсутствие: ожидание чужого поколения отменяется.
    pub fn set_leader(&self, leader: Option<u32>) {
        let mut state = self.lock();
        state.leader = leader;
        if let Some(pending) = &mut state.pending
            && Some(pending.generation) != leader
            && pending.answer.is_none()
        {
            pending.cancelled = true;
            self.wake.notify_all();
        }
    }

    /// Запрос к GUI текущего лидера и ожидание ответа не дольше срока. Зовёт поток разбора.
    pub fn read(&self) -> Read {
        let mut state = self.lock();
        state.stats.requests += 1;
        let Some(leader) = state.leader else {
            state.stats.no_gui += 1;
            return Read::NoGui;
        };
        let Some((codec, frames)) = state
            .guis
            .values()
            .find(|gui| gui.generation == leader)
            .map(|gui| (gui.codec, Arc::clone(&gui.frames)))
        else {
            state.stats.no_gui += 1;
            return Read::NoGui;
        };
        state.serial += 1;
        let serial = state.serial;
        let frame = proto::encode(
            codec,
            &Message::ClipboardRequest {
                serial,
                generation: leader,
            },
        );
        if !frames.offer(frame) {
            state.stats.no_gui += 1;
            return Read::NoGui;
        }
        state.pending = Some(Pending {
            serial,
            generation: leader,
            answer: None,
            cancelled: false,
        });
        let deadline = Instant::now() + self.timeout;
        loop {
            let done = match &state.pending {
                Some(pending) => pending.answer.is_some() || pending.cancelled,
                None => true,
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if done || left.is_zero() {
                break;
            }
            state = self
                .wake
                .wait_timeout(state, left)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
        let pending = state.pending.take();
        let read = match pending {
            Some(Pending {
                answer: Some(Some(data)),
                ..
            }) => Read::Answered(data),
            Some(Pending {
                answer: Some(None), ..
            }) => Read::Empty,
            Some(Pending {
                cancelled: true, ..
            }) => Read::Cancelled,
            _ => Read::Timeout,
        };
        match read {
            Read::Answered(_) | Read::Empty => state.stats.answered += 1,
            Read::Cancelled => state.stats.cancelled += 1,
            Read::Timeout => state.stats.timeouts += 1,
            Read::NoGui => {}
        }
        read
    }

    /// Ответ GUI из соединения `conn`; принимается только ответ на ждущий запрос от GUI его
    /// поколения.
    pub fn deliver(
        &self,
        conn: u64,
        serial: u64,
        generation: u32,
        data: Option<Vec<u8>>,
    ) -> Delivery {
        let mut state = self.lock();
        let bound = state.guis.get(&conn).map(|gui| gui.generation);
        let accepted = match &mut state.pending {
            Some(pending)
                if pending.serial == serial
                    && pending.generation == generation
                    && bound == Some(generation)
                    && pending.answer.is_none()
                    && !pending.cancelled =>
            {
                pending.answer = Some(data);
                true
            }
            _ => false,
        };
        if accepted {
            self.wake.notify_all();
            Delivery::Accepted
        } else {
            state.stats.dropped += 1;
            Delivery::Dropped
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    fn request(frame: &[u8]) -> (u64, u32) {
        // Кадр запроса одинаков в v1 и v2.
        match proto::read_message(&mut &frame[..], Codec::V2) {
            Ok(Some(Message::ClipboardRequest { serial, generation })) => (serial, generation),
            other => panic!("вместо запроса: {other:?}"),
        }
    }

    #[test]
    fn without_gui_the_read_is_answered_empty_at_once() {
        let clipboard = Clipboard::new(Duration::from_secs(30));
        assert_eq!(clipboard.read(), Read::NoGui);
        clipboard.set_leader(Some(1));
        assert_eq!(clipboard.read(), Read::NoGui);
        assert_eq!(clipboard.stats().no_gui, 2);
    }

    #[test]
    fn current_gui_answers_and_late_answer_of_old_generation_is_dropped() {
        let clipboard = Arc::new(Clipboard::new(Duration::from_secs(30)));
        let (tx1, rx1) = mpsc::sync_channel(4);
        let (tx2, rx2) = mpsc::sync_channel(4);
        clipboard.bind(10, 1, Codec::V2, tx1);
        clipboard.set_leader(Some(1));
        assert!(clipboard.current(10));

        let reader = Arc::clone(&clipboard);
        let first = thread::spawn(move || reader.read());
        let (serial, generation) = request(&rx1.recv().unwrap());
        assert_eq!(generation, 1);
        // Ответ на чужой номер запроса не принимается.
        assert_eq!(
            clipboard.deliver(10, serial + 1, 1, Some(b"x".to_vec())),
            Delivery::Dropped
        );
        assert_eq!(
            clipboard.deliver(10, serial, 1, Some(b"gui".to_vec())),
            Delivery::Accepted
        );
        assert_eq!(first.join().unwrap(), Read::Answered(b"gui".to_vec()));

        // Лидер c1 молчит, c2 забирает лидерство: ожидание отменено, ответ c1 опоздал.
        clipboard.bind(20, 2, Codec::V1, tx2);
        let reader = Arc::clone(&clipboard);
        let second = thread::spawn(move || reader.read());
        let (late_serial, _) = request(&rx1.recv().unwrap());
        clipboard.set_leader(Some(2));
        assert_eq!(second.join().unwrap(), Read::Cancelled);
        assert!(!clipboard.current(10));
        assert!(clipboard.current(20));
        assert_eq!(
            clipboard.deliver(10, late_serial, 1, Some(b"late".to_vec())),
            Delivery::Dropped
        );
        assert!(rx2.try_recv().is_err());

        let stats = clipboard.stats();
        assert_eq!(stats.requests, 2);
        assert_eq!(stats.answered, 1);
        assert_eq!(stats.cancelled, 1);
        assert_eq!(stats.dropped, 2);
    }

    #[test]
    fn answer_from_a_connection_of_another_generation_is_dropped() {
        let clipboard = Arc::new(Clipboard::new(Duration::from_secs(30)));
        let (tx1, rx1) = mpsc::sync_channel(4);
        let (tx2, _rx2) = mpsc::sync_channel(4);
        clipboard.bind(10, 1, Codec::V2, tx1);
        clipboard.bind(20, 2, Codec::V2, tx2);
        clipboard.set_leader(Some(1));
        let reader = Arc::clone(&clipboard);
        let read = thread::spawn(move || reader.read());
        let (serial, generation) = request(&rx1.recv().unwrap());
        // Номер и поколение верны, но соединение говорит от имени поколения 2.
        assert_eq!(
            clipboard.deliver(20, serial, generation, Some(b"x".to_vec())),
            Delivery::Dropped
        );
        // Пустой ответ GUI своего поколения — ответ без данных.
        assert_eq!(
            clipboard.deliver(10, serial, generation, None),
            Delivery::Accepted
        );
        assert_eq!(read.join().unwrap(), Read::Empty);
        let stats = clipboard.stats();
        assert_eq!((stats.answered, stats.dropped), (1, 1));
    }

    #[test]
    fn refused_request_frame_means_no_gui() {
        let clipboard = Clipboard::new(Duration::from_secs(30));
        let (tx, _rx) = mpsc::sync_channel(0);
        clipboard.bind(1, 3, Codec::V2, tx);
        clipboard.set_leader(Some(3));
        assert_eq!(clipboard.read(), Read::NoGui);
        assert_eq!(clipboard.stats().no_gui, 1);
    }

    #[test]
    fn silent_gui_gives_a_timeout_and_a_later_answer_is_dropped() {
        let clipboard = Clipboard::new(Duration::from_millis(20));
        let (tx, rx) = mpsc::sync_channel(4);
        clipboard.bind(1, 7, Codec::V2, tx);
        clipboard.set_leader(Some(7));
        assert_eq!(clipboard.read(), Read::Timeout);
        let (serial, _) = request(&rx.recv().unwrap());
        assert_eq!(clipboard.deliver(1, serial, 7, None), Delivery::Dropped);
        let stats = clipboard.stats();
        assert_eq!((stats.timeouts, stats.dropped), (1, 1));
    }
}
