//! Очереди, подтверждение ввода и правило размера Rust-хоста (план P0.4, задача 13). Модуль
//! чистый и идёт в `make check`.
//!
//! Объявленные пределы:
//! - вывод клиенту — [`CLIENT_QUEUE_BYTES`] байт живого потока в очереди и в записи. Переполнение
//!   разрывает клиента: очередь очищается, клиент получает `Evicted` с причиной, хост пишет её в
//!   журнал и считает в `Status`. Переподключение получает новое восстановление. Восстановление,
//!   уведомление о разрыве и выход нагрузки идут вне предела: их объём ограничен состоянием VT;
//! - ввод в PTY — [`INPUT_QUEUE_BYTES`] байт в очереди и в записи. Ввод сверх предела отвергается
//!   целиком и явно, а не теряется. Ответы хоста на запросы нагрузки получают сверх него запас
//!   [`REPLY_RESERVE`];
//! - номера ввода — по [`INPUT_SOURCES`] источникам: повтор номера, уже принятого от источника,
//!   подтверждается как повтор и в PTY не пишется.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::backend::{Lead, Role};
use crate::pty::Winsize;

/// Предел живого вывода в очереди одного клиента.
pub const CLIENT_QUEUE_BYTES: usize = 8 << 20;
/// Предел ввода в очереди записи в PTY.
pub const INPUT_QUEUE_BYTES: usize = 256 << 10;
/// Запас для ответов хоста сверх предела ввода.
pub const REPLY_RESERVE: usize = 64 << 10;
/// Источников ввода, чьи номера хост помнит.
pub const INPUT_SOURCES: usize = 64;

/// Сколько места может занять кадр.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Room {
    /// В пределах очереди.
    Limit,
    /// В пределах очереди и запаса сверх него.
    Reserve(usize),
    /// Без предела: размер кадра ограничен источником.
    Always,
}

/// Отказ постановки в очередь.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Full {
        len: usize,
        counted: usize,
        limit: usize,
    },
    Closed,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Full {
                len,
                counted,
                limit,
            } => write!(
                f,
                "очередь заполнена: {len} байт не помещаются, занято {counted} из {limit}"
            ),
            Refusal::Closed => f.write_str("очередь закрыта"),
        }
    }
}

#[derive(Debug, Default)]
struct State {
    frames: VecDeque<Vec<u8>>,
    /// В очереди и в записи.
    counted: usize,
    /// Кадр в записи.
    writing: usize,
    closing: bool,
    /// Писатель вышел.
    finished: bool,
    peak: usize,
}

/// Очередь кадров с пределом в байтах и одним писателем. Байты кадра считаются до конца его
/// записи: остановленный приёмник держит в памяти не больше предела и одного кадра сверх запаса.
#[derive(Debug)]
pub struct ByteQueue {
    state: Mutex<State>,
    ready: Condvar,
    limit: usize,
}

impl ByteQueue {
    pub fn new(limit: usize) -> ByteQueue {
        ByteQueue {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
            limit,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Ставит кадр целиком или отказывает целиком.
    pub fn push(&self, frame: Vec<u8>, room: Room) -> Result<(), Refusal> {
        let mut state = self.lock();
        if state.closing {
            return Err(Refusal::Closed);
        }
        let allowed = match room {
            Room::Limit => self.limit,
            Room::Reserve(extra) => self.limit.saturating_add(extra),
            Room::Always => usize::MAX,
        };
        if state.counted.saturating_add(frame.len()) > allowed {
            return Err(Refusal::Full {
                len: frame.len(),
                counted: state.counted,
                limit: self.limit,
            });
        }
        state.counted += frame.len();
        state.peak = state.peak.max(state.counted);
        state.frames.push_back(frame);
        self.ready.notify_all();
        Ok(())
    }

    /// Разрыв: кадры, ещё не начатые записью, отбрасываются, последним идёт `notice`, затем
    /// очередь закрывается. Возвращает отброшенные байты.
    pub fn evict(&self, notice: Vec<u8>) -> usize {
        let mut state = self.lock();
        let dropped: usize = state.frames.drain(..).map(|frame| frame.len()).sum();
        state.counted -= dropped;
        if !state.closing {
            state.counted += notice.len();
            state.frames.push_back(notice);
        }
        state.closing = true;
        self.ready.notify_all();
        dropped
    }

    /// Закрытие: писатель дописывает поставленное и выходит.
    pub fn close(&self) {
        self.lock().closing = true;
        self.ready.notify_all();
    }

    /// Следующий кадр писателю; `None` — очередь закрыта и пуста.
    pub fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.lock();
        loop {
            if let Some(frame) = state.frames.pop_front() {
                state.writing = frame.len();
                return Some(frame);
            }
            if state.closing {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Кадр, выданный [`ByteQueue::pop`], записан или брошен.
    pub fn written(&self) {
        let mut state = self.lock();
        state.counted -= state.writing;
        state.writing = 0;
        self.ready.notify_all();
    }

    /// Писатель вышел: очередь закрыта, оставшиеся кадры уже не будут записаны.
    pub fn finish(&self) {
        let mut state = self.lock();
        let left: usize = state.frames.drain(..).map(|frame| frame.len()).sum();
        state.counted -= left + state.writing;
        state.writing = 0;
        state.closing = true;
        state.finished = true;
        self.ready.notify_all();
    }

    /// Ждёт выхода писателя не дольше `deadline`; `false` — срок вышел.
    pub fn wait_finished(&self, deadline: Instant) -> bool {
        let mut state = self.lock();
        while !state.finished {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = match self.ready.wait_timeout(state, left) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        true
    }

    /// Занято байт: в очереди и в записи.
    pub fn counted(&self) -> usize {
        self.lock().counted
    }

    pub fn peak(&self) -> usize {
        self.lock().peak
    }
}

/// Пишет `bytes` целиком: частичная запись продолжается с места остановки, `EINTR` повторяется.
pub fn write_fully(sink: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    let mut at = 0;
    while at < bytes.len() {
        match sink.write(&bytes[at..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("запись остановилась на {at} из {} байт", bytes.len()),
                ));
            }
            Ok(written) => at += written,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    sink.flush()
}

/// Цикл писателя: кадры очереди в `sink` до закрытия очереди или ошибки записи.
pub fn drain(queue: &ByteQueue, sink: &mut impl Write) -> io::Result<()> {
    let result = loop {
        let Some(frame) = queue.pop() else {
            break Ok(());
        };
        let written = write_fully(sink, &frame);
        queue.written();
        if let Err(err) = written {
            break Err(err);
        }
    };
    queue.finish();
    result
}

/// Источник ввода: управляющее соединение или attach-клиент. Номер `seq` растёт у источника с
/// каждым новым вводом; повтор того же номера — повтор после потерянного подтверждения.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputId {
    pub source: u64,
    pub seq: u64,
}

/// Номера ввода, уже принятые хостом, по [`INPUT_SOURCES`] последним источникам.
#[derive(Debug)]
pub struct InputLedger {
    seen: VecDeque<(u64, u64)>,
    cap: usize,
}

impl Default for InputLedger {
    fn default() -> InputLedger {
        InputLedger::new(INPUT_SOURCES)
    }
}

impl InputLedger {
    pub fn new(cap: usize) -> InputLedger {
        InputLedger {
            seen: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Последний принятый номер источника.
    pub fn last(&self, source: u64) -> Option<u64> {
        self.seen
            .iter()
            .find(|(known, _)| *known == source)
            .map(|(_, seq)| *seq)
    }

    /// Ввод с этим номером уже принят.
    pub fn is_duplicate(&self, id: InputId) -> bool {
        self.last(id.source).is_some_and(|last| id.seq <= last)
    }

    /// Ввод принят: номер запоминается, источник становится самым свежим.
    pub fn record(&mut self, id: InputId) {
        let last = self.last(id.source).unwrap_or(0).max(id.seq);
        self.seen.retain(|(known, _)| *known != id.source);
        self.seen.push_back((id.source, last));
        while self.seen.len() > self.cap {
            self.seen.pop_front();
        }
    }
}

/// Кто прислал ресайз или ввод.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin<Id> {
    /// Управляющее соединение: операция без клиента.
    Control,
    Client(Id),
}

/// Итог подключения attach-клиента.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attached<Id> {
    pub role: Role,
    pub generation: u32,
    /// Прежний лидер, ставший ведомым.
    pub demoted: Option<Id>,
    /// Размер окна нового лидера: единственный источник размера.
    pub resize: Option<Winsize>,
}

/// Лидерство и источник размера. Размер и ввод attach-клиента принимаются только от текущего
/// лидера; ресайз прежнего лидера, пришедший после смены, устарел и отбрасывается. Управляющее
/// соединение пишет ввод и меняет размер, лидерства не берёт.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leadership<Id> {
    leader: Option<Id>,
    generation: u32,
}

impl<Id> Default for Leadership<Id> {
    fn default() -> Leadership<Id> {
        Leadership {
            leader: None,
            generation: 0,
        }
    }
}

impl<Id: Copy + PartialEq> Leadership<Id> {
    pub fn leader(&self) -> Option<Id> {
        self.leader
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn attach(&mut self, id: Id, lead: Lead, size: Winsize) -> Attached<Id> {
        let leads = match lead {
            Lead::Claim => true,
            Lead::IfFree => self.leader.is_none(),
        };
        if !leads {
            return Attached {
                role: Role::Follower,
                generation: self.generation,
                demoted: None,
                resize: None,
            };
        }
        let demoted = self.leader.replace(id).filter(|previous| *previous != id);
        self.generation += 1;
        Attached {
            role: Role::Leader,
            generation: self.generation,
            demoted,
            resize: Some(size),
        }
    }

    /// Ресайз принимается от управляющего соединения и от текущего лидера.
    pub fn accepts_resize(&self, from: Origin<Id>) -> bool {
        match from {
            Origin::Control => true,
            Origin::Client(id) => self.leader == Some(id),
        }
    }

    /// Ввод принимается от управляющего соединения и от текущего лидера.
    pub fn accepts_input(&self, from: Origin<Id>) -> bool {
        self.accepts_resize(from)
    }

    /// Клиент ушёл; `true` — он был лидером, сессию больше никто не ведёт.
    pub fn detach(&mut self, id: Id) -> bool {
        if self.leader == Some(id) {
            self.leader = None;
            return true;
        }
        false
    }
}

/// Срок ожидания писателей при выходе хоста: остановленный клиент выхода не держит.
pub const EXIT_FLUSH: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests;
