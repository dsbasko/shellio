//! Канал фикстуры: сокет Unix вне проверяемого потока PTY. Фикстура подключается к сокету
//! стенда (`--channel`) и шлёт квитанции, стенд по тому же соединению шлёт команды. Кадр в обе
//! стороны — одна строка JSON и `\n`.
//!
//! У квитанций сквозной номер `seq` с единицы: стенд проверяет, что номера идут подряд, и
//! отличает повреждённую или потерянную квитанцию от конца соединения.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::Mode;
use super::query::{Query, ResponseKind};
use crate::pty::Winsize;

/// Предел кадра в байтах без `\n`.
pub const MAX_FRAME: usize = 4 << 20;
/// `sockaddr_un.sun_path` на macOS — 104 байта вместе с нулём.
pub const SUN_PATH_MAX: usize = 103;
const READ_BUF: usize = 64 << 10;

// ---- команды стенда ----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Command {
    /// Байты в PTY как есть.
    Write {
        hex: String,
    },
    /// Нумерованный поток `lines` строк шириной `width`; строка `n` уходит через
    /// `n × interval_us` от начала, каждые `report_every` строк — квитанция хода.
    Flood {
        lines: u64,
        width: u16,
        interval_us: u64,
        report_every: u64,
    },
    StopFlood,
    /// Запрос терминала; при `split` пишется только префикс длиной `split` байт, остаток — по
    /// команде `rest` с `id` этого запроса.
    Query {
        query: Query,
        split: Option<u32>,
    },
    Rest {
        query: u64,
    },
    /// Темп чтения PTY в байтах в секунду: `None` — без предела, `Some(0)` — чтение остановлено.
    ReadRate {
        bytes_per_sec: Option<u64>,
    },
    /// Новый срок фикстуры от момента команды.
    Lifetime {
        ms: u64,
    },
    /// `replay`: префикс `prefix` и байты сцены одной записью.
    Replay {
        prefix: String,
    },
    Exit {
        code: u8,
    },
}

// ---- квитанции фикстуры ----

/// Квитанция. `t_ns` — `CLOCK_UPTIME_RAW` события: для `read` сразу после `read`, для сигнала
/// сразу после `sigwait`, для `ack` после записи байтов команды.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub seq: u64,
    pub t_ns: u64,
    pub event: Event,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Event {
    Ready(Ready),
    /// Чтение из PTY: номер с единицы и байты.
    Read {
        index: u64,
        hex: String,
    },
    Winch {
        size: Option<Winsize>,
    },
    /// Сигнал, принятый `sigwait`.
    Signal {
        signal: String,
    },
    Ack {
        id: u64,
        bytes: u64,
        error: Option<String>,
    },
    /// `writes` — записи фикстуры в PTY с её запуска на момент квитанции: задержки видны сразу,
    /// а не только в квитанции выхода.
    FloodProgress {
        id: u64,
        lines: u64,
        bytes: u64,
        writes: WriteStats,
    },
    FloodDone {
        id: u64,
        lines: u64,
        bytes: u64,
        cancelled: bool,
        writes: WriteStats,
    },
    /// Запись `replay` в PTY по команде `id`: источник (`scene`, `live`, `query`), длина до фильтра,
    /// записанные байты и сколько параметров режимов отчётов фильтр вырезал из этой записи.
    Emitted {
        id: u64,
        source: String,
        input: u64,
        hex: String,
        removed: u64,
    },
    /// Ответ терминала, разобранный во вводе: номер с единицы, вид и байты.
    Response {
        index: u64,
        response: ResponseKind,
        hex: String,
    },
    /// Фоновый потомок `agent`.
    Spawned {
        pid: i32,
        mode: Mode,
    },
    ChildExit {
        pid: i32,
        status: WaitStatus,
    },
    /// Перемена задания `shell`; `foreground_pgid` — `tcgetpgrp` после неё.
    Job {
        job: u32,
        pid: i32,
        change: JobChange,
        foreground_pgid: Option<i32>,
    },
    /// Строка `shell` выполнена.
    Shell {
        line: String,
        error: Option<String>,
        jobs: Vec<JobView>,
        foreground_pgid: Option<i32>,
    },
    /// Кадр команды не разобран.
    Protocol {
        error: String,
    },
    Exit {
        reason: String,
        detail: Option<String>,
        code: u8,
        writes: WriteStats,
    },
}

/// Готовность: процесс, терминал и режим фикстуры.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    pub nonce: String,
    pub mode: Mode,
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub sid: i32,
    /// `tcgetpgrp` ведомой стороны PTY.
    pub foreground_pgid: Option<i32>,
    pub tty: Option<String>,
    pub size: Option<Winsize>,
    /// Терминал переведён в raw-режим до готовности.
    pub raw: bool,
}

/// Записи фикстуры в PTY. Запись не ждёт: при `EAGAIN` фикстура считает задержку и ждёт
/// готовности дескриптора, поэтому `stalls` — сколько раз PTY не принял байты сразу.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteStats {
    pub bytes: u64,
    pub stalls: u64,
    pub stalled_ns: u64,
}

/// Итог `waitpid`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WaitStatus {
    Exited { code: i32 },
    Signaled { signal: i32 },
    Stopped { signal: i32 },
    Continued,
}

impl WaitStatus {
    /// Разбор статуса `waitpid`. «Продолжен» проверяется раньше «остановлен»: на macOS оба
    /// кодируются младшими битами `0x7f`.
    pub fn decode(status: libc::c_int) -> Option<WaitStatus> {
        if libc::WIFCONTINUED(status) {
            Some(WaitStatus::Continued)
        } else if libc::WIFSTOPPED(status) {
            Some(WaitStatus::Stopped {
                signal: libc::WSTOPSIG(status),
            })
        } else if libc::WIFEXITED(status) {
            Some(WaitStatus::Exited {
                code: libc::WEXITSTATUS(status),
            })
        } else if libc::WIFSIGNALED(status) {
            Some(WaitStatus::Signaled {
                signal: libc::WTERMSIG(status),
            })
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum JobChange {
    Started { foreground: bool },
    Stopped { signal: String },
    Continued { foreground: bool },
    Exited { code: i32 },
    Signaled { signal: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobView {
    pub job: u32,
    pub pid: i32,
    pub stopped: bool,
    pub foreground: bool,
    pub command: String,
}

// ---- кадры ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Кадр не разбирается как JSON ожидаемой схемы.
    Corrupt(String),
    /// Поток закончился посреди кадра.
    Truncated(usize),
    /// Кадр длиннее `MAX_FRAME`; байты до следующего `\n` отброшены.
    Oversized(usize),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Corrupt(reason) => write!(f, "повреждённый кадр: {reason}"),
            FrameError::Truncated(bytes) => {
                write!(f, "обрезанный кадр: {bytes} байт без конца строки")
            }
            FrameError::Oversized(bytes) => write!(f, "кадр длиннее {MAX_FRAME} байт: {bytes}"),
        }
    }
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut frame = serde_json::to_vec(value).map_err(|err| err.to_string())?;
    frame.push(b'\n');
    Ok(frame)
}

pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, FrameError> {
    serde_json::from_slice(frame).map_err(|err| FrameError::Corrupt(err.to_string()))
}

/// Нарезка потока байтов на кадры по `\n`.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
    /// После слишком длинного кадра байты до следующего `\n` отбрасываются.
    discarding: bool,
}

impl FrameReader {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Следующий полный кадр без `\n`.
    pub fn next_frame(&mut self) -> Option<Result<Vec<u8>, FrameError>> {
        loop {
            let Some(end) = self.buf.iter().position(|&byte| byte == b'\n') else {
                if self.discarding {
                    self.buf.clear();
                } else if self.buf.len() > MAX_FRAME {
                    let bytes = self.buf.len();
                    self.buf.clear();
                    self.discarding = true;
                    return Some(Err(FrameError::Oversized(bytes)));
                }
                return None;
            };
            let mut frame: Vec<u8> = self.buf.drain(..=end).collect();
            frame.pop();
            if std::mem::take(&mut self.discarding) {
                continue;
            }
            if frame.len() > MAX_FRAME {
                return Some(Err(FrameError::Oversized(frame.len())));
            }
            if frame.is_empty() {
                return Some(Err(FrameError::Corrupt("пустой кадр".to_owned())));
            }
            return Some(Ok(frame));
        }
    }

    /// Конец потока: недочитанный кадр — ошибка.
    pub fn finish(&self) -> Result<(), FrameError> {
        if self.buf.is_empty() || self.discarding {
            Ok(())
        } else {
            Err(FrameError::Truncated(self.buf.len()))
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Байты строки [`hex`]; `None` для нечётной длины и не шестнадцатеричных символов.
pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

/// Путь сокета помещается в `sun_path`. Длинный путь — явный отказ: смена рабочего каталога
/// ради короткого имени действует на весь процесс.
pub fn check_path(path: &Path) -> Result<(), String> {
    let len = path.as_os_str().len();
    if len > SUN_PATH_MAX {
        return Err(format!(
            "путь сокета {} длиннее {SUN_PATH_MAX} байт: {len}",
            path.display()
        ));
    }
    Ok(())
}

/// Подключение фикстуры к сокету стенда.
pub fn connect(path: &Path) -> Result<UnixStream, String> {
    check_path(path)?;
    UnixStream::connect(path).map_err(|err| format!("сокет {}: {err}", path.display()))
}

// ---- сторона стенда ----

/// Сокет стенда, к которому подключаются фикстуры. Файл сокета удаляется при освобождении:
/// `--launch` считает оставшийся в `SHELLIO_STATE_DIR` файл ошибкой исполнения.
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
}

impl Listener {
    pub fn bind(path: &Path) -> Result<Listener, String> {
        check_path(path)?;
        let listener =
            UnixListener::bind(path).map_err(|err| format!("сокет {}: {err}", path.display()))?;
        Ok(Listener {
            listener,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Следующее подключение фикстуры не позже `timeout`.
    pub fn accept(&self, timeout: Duration) -> Result<Link, LinkError> {
        if !poll_readable(self.listener.as_raw_fd(), timeout)? {
            return Err(LinkError::Timeout);
        }
        let (stream, _) = self
            .listener
            .accept()
            .map_err(|err| LinkError::Io(format!("accept: {err}")))?;
        Link::new(stream)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn poll_readable(fd: libc::c_int, timeout: Duration) -> Result<bool, LinkError> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = libc::c_int::try_from(left.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: одна структура на стеке живёт на время вызова.
        let ready = unsafe { libc::poll(&mut poll, 1, ms) };
        if ready > 0 {
            return Ok(true);
        }
        if ready == 0 {
            return Ok(false);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(LinkError::Io(format!("poll: {err}")));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    Timeout,
    /// Фикстура закрыла соединение.
    Closed,
    /// Кадр квитанции не разобран.
    Corrupt(String),
    /// Номер квитанции не следующий: квитанция потеряна или повторена.
    Sequence {
        expected: u64,
        got: u64,
    },
    /// Команда отвергнута фикстурой.
    Rejected {
        id: u64,
        error: String,
    },
    Io(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::Timeout => f.write_str("квитанция не пришла в срок"),
            LinkError::Closed => f.write_str("фикстура закрыла канал"),
            LinkError::Corrupt(reason) => write!(f, "повреждённая квитанция: {reason}"),
            LinkError::Sequence { expected, got } => {
                write!(f, "квитанция {got} вместо {expected}")
            }
            LinkError::Rejected { id, error } => write!(f, "команда {id} отвергнута: {error}"),
            LinkError::Io(reason) => f.write_str(reason),
        }
    }
}

enum Incoming {
    Frame(Result<Vec<u8>, FrameError>),
    Closed,
    Error(String),
}

/// Соединение стенда с одной фикстурой. Все квитанции сохраняются в `log` в порядке прихода;
/// квитанции, которые ожидание пропустило, остаются в очереди для следующих ожиданий.
pub struct Link {
    writer: UnixStream,
    rx: Receiver<Incoming>,
    next_id: u64,
    last_seq: u64,
    backlog: VecDeque<Receipt>,
    log: Vec<Receipt>,
    closed: bool,
}

impl Link {
    pub fn new(stream: UnixStream) -> Result<Link, LinkError> {
        let reader = stream
            .try_clone()
            .map_err(|err| LinkError::Io(format!("сокет не дублируется: {err}")))?;
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("stand-receipts".to_owned())
            .spawn(move || read_frames(reader, &tx))
            .map_err(|err| LinkError::Io(format!("поток квитанций не запущен: {err}")))?;
        Ok(Link {
            writer: stream,
            rx,
            next_id: 0,
            last_seq: 0,
            backlog: VecDeque::new(),
            log: Vec::new(),
            closed: false,
        })
    }

    /// Все квитанции в порядке прихода.
    pub fn log(&self) -> &[Receipt] {
        &self.log
    }

    /// Следующая квитанция: сначала пропущенные ожиданиями, затем новые.
    pub fn recv(&mut self, timeout: Duration) -> Result<Receipt, LinkError> {
        if let Some(receipt) = self.backlog.pop_front() {
            return Ok(receipt);
        }
        self.next_new(Instant::now() + timeout)
    }

    /// Первая квитанция, для которой `pick` даёт значение; остальные остаются в очереди.
    pub fn wait_for<T>(
        &mut self,
        timeout: Duration,
        mut pick: impl FnMut(&Receipt) -> Option<T>,
    ) -> Result<T, LinkError> {
        if let Some(at) = self
            .backlog
            .iter()
            .position(|receipt| pick(receipt).is_some())
            && let Some(receipt) = self.backlog.remove(at)
            && let Some(value) = pick(&receipt)
        {
            return Ok(value);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let receipt = self.next_new(deadline)?;
            if let Some(value) = pick(&receipt) {
                return Ok(value);
            }
            self.backlog.push_back(receipt);
        }
    }

    pub fn send(&mut self, command: Command) -> Result<u64, LinkError> {
        self.next_id += 1;
        let id = self.next_id;
        let frame = encode(&Request { id, command }).map_err(LinkError::Io)?;
        self.writer
            .write_all(&frame)
            .map_err(|err| LinkError::Io(format!("запись команды: {err}")))?;
        Ok(id)
    }

    /// Команда и её подтверждение: число записанных в PTY байтов или отказ.
    pub fn request(&mut self, command: Command, timeout: Duration) -> Result<u64, LinkError> {
        let id = self.send(command)?;
        let (bytes, error) = self.wait_for(timeout, |receipt| match &receipt.event {
            Event::Ack {
                id: acked,
                bytes,
                error,
            } if *acked == id => Some((*bytes, error.clone())),
            _ => None,
        })?;
        match error {
            None => Ok(bytes),
            Some(error) => Err(LinkError::Rejected { id, error }),
        }
    }

    fn next_new(&mut self, deadline: Instant) -> Result<Receipt, LinkError> {
        if self.closed {
            return Err(LinkError::Closed);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        let incoming = match self.rx.recv_timeout(left) {
            Ok(incoming) => incoming,
            Err(RecvTimeoutError::Timeout) => return Err(LinkError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Incoming::Closed,
        };
        let frame = match incoming {
            Incoming::Frame(frame) => frame,
            Incoming::Closed => {
                self.closed = true;
                return Err(LinkError::Closed);
            }
            Incoming::Error(reason) => {
                self.closed = true;
                return Err(LinkError::Io(reason));
            }
        };
        let receipt: Receipt = frame
            .and_then(|frame| decode(&frame))
            .map_err(|err| LinkError::Corrupt(err.to_string()))?;
        let expected = self.last_seq + 1;
        if receipt.seq != expected {
            return Err(LinkError::Sequence {
                expected,
                got: receipt.seq,
            });
        }
        self.last_seq = receipt.seq;
        self.log.push(receipt.clone());
        Ok(receipt)
    }
}

fn read_frames(mut stream: UnixStream, tx: &mpsc::Sender<Incoming>) {
    let mut frames = FrameReader::default();
    let mut buf = vec![0u8; READ_BUF];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => {
                if let Err(err) = frames.finish() {
                    let _ = tx.send(Incoming::Frame(Err(err)));
                }
                let _ = tx.send(Incoming::Closed);
                return;
            }
            Ok(read) => {
                frames.push(&buf[..read]);
                while let Some(frame) = frames.next_frame() {
                    if tx.send(Incoming::Frame(frame)).is_err() {
                        return;
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => {
                let _ = tx.send(Incoming::Error(format!("чтение квитанций: {err}")));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    fn receipt(seq: u64, event: Event) -> Vec<u8> {
        encode(&Receipt {
            seq,
            t_ns: seq * 10,
            event,
        })
        .unwrap()
    }

    fn signal(name: &str) -> Event {
        Event::Signal {
            signal: name.to_owned(),
        }
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0x00, 0x1b, 0x5b, 0xd0, 0xb6, 0xff];
        assert_eq!(unhex(&hex(&bytes)).as_deref(), Some(&bytes[..]));
        assert_eq!(unhex(""), Some(Vec::new()));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex("жж"), None);
    }

    #[test]
    fn frames_are_split_on_newlines_across_reads() {
        let first = receipt(1, signal("SIGWINCH"));
        let second = receipt(2, signal("SIGCONT"));
        let mut all = first.clone();
        all.extend_from_slice(&second);
        let mut reader = FrameReader::default();
        for byte in &all {
            reader.push(std::slice::from_ref(byte));
        }
        let decoded: Vec<Receipt> = std::iter::from_fn(|| reader.next_frame())
            .map(|frame| decode(&frame.unwrap()).unwrap())
            .collect();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[1].event, signal("SIGCONT"));
        assert_eq!(reader.finish(), Ok(()));
        reader.push(b"{\"seq\":");
        assert_eq!(reader.finish(), Err(FrameError::Truncated(7)));
    }

    #[test]
    fn corrupt_empty_and_oversized_frames_are_errors() {
        let mut reader = FrameReader::default();
        reader.push(b"\n{oops}\n");
        assert!(matches!(
            reader.next_frame(),
            Some(Err(FrameError::Corrupt(_)))
        ));
        let frame = reader.next_frame().unwrap().unwrap();
        assert!(matches!(
            decode::<Receipt>(&frame),
            Err(FrameError::Corrupt(_))
        ));
        reader.push(&vec![b'x'; MAX_FRAME + 1]);
        assert!(matches!(
            reader.next_frame(),
            Some(Err(FrameError::Oversized(_)))
        ));
        reader.push(b"tail\n");
        reader.push(&receipt(1, signal("SIGHUP")));
        let frame = reader.next_frame().unwrap().unwrap();
        assert_eq!(decode::<Receipt>(&frame).unwrap().seq, 1);
    }

    #[test]
    fn wait_statuses_are_decoded() {
        // Кодировки статуса waitpid в BSD: код выхода в старшем байте, сигнал в младших битах,
        // остановка и продолжение — 0x7f с номером сигнала, продолжение — сигнал 0x13.
        assert_eq!(
            WaitStatus::decode(3 << 8),
            Some(WaitStatus::Exited { code: 3 })
        );
        assert_eq!(
            WaitStatus::decode(libc::SIGINT),
            Some(WaitStatus::Signaled {
                signal: libc::SIGINT
            })
        );
        assert_eq!(
            WaitStatus::decode((libc::SIGTSTP << 8) | 0x7f),
            Some(WaitStatus::Stopped {
                signal: libc::SIGTSTP
            })
        );
        assert_eq!(WaitStatus::decode(0x137f), Some(WaitStatus::Continued));
    }

    #[test]
    fn path_longer_than_sun_path_is_rejected() {
        assert!(check_path(Path::new("/tmp/s.sock")).is_ok());
        let long = PathBuf::from("/tmp").join("s".repeat(SUN_PATH_MAX));
        assert!(check_path(&long).unwrap_err().contains("длиннее"));
        assert!(connect(&long).is_err());
    }

    fn pair() -> (Link, UnixStream) {
        let (stand, fixture) = UnixStream::pair().unwrap();
        (Link::new(stand).unwrap(), fixture)
    }

    #[test]
    fn corrupt_receipt_is_reported_and_sequence_is_checked() {
        let (mut link, mut fixture) = pair();
        fixture.write_all(&receipt(1, signal("SIGWINCH"))).unwrap();
        fixture.write_all(b"{\"seq\":2,\"t_ns\":1}\n").unwrap();
        fixture.write_all(&receipt(4, signal("SIGCONT"))).unwrap();
        assert_eq!(link.recv(WAIT).unwrap().event, signal("SIGWINCH"));
        assert!(matches!(link.recv(WAIT), Err(LinkError::Corrupt(_))));
        assert_eq!(
            link.recv(WAIT),
            Err(LinkError::Sequence {
                expected: 2,
                got: 4
            })
        );
        assert_eq!(link.log().len(), 1);
        fixture.write_all(b"{\"seq\":").unwrap();
        drop(fixture);
        assert!(matches!(link.recv(WAIT), Err(LinkError::Corrupt(_))));
        assert_eq!(link.recv(WAIT), Err(LinkError::Closed));
        assert_eq!(link.recv(WAIT), Err(LinkError::Closed));
    }

    #[test]
    fn waiting_keeps_skipped_receipts_and_requests_get_their_ack() {
        let (mut link, mut fixture) = pair();
        let id = link.send(Command::StopFlood).unwrap();
        let mut frames = FrameReader::default();
        let mut buf = [0u8; 256];
        let read = fixture.read(&mut buf).unwrap();
        frames.push(&buf[..read]);
        let request: Request = decode(&frames.next_frame().unwrap().unwrap()).unwrap();
        assert_eq!(
            request,
            Request {
                id,
                command: Command::StopFlood
            }
        );
        fixture.write_all(&receipt(1, signal("SIGWINCH"))).unwrap();
        fixture
            .write_all(&receipt(
                2,
                Event::Ack {
                    id,
                    bytes: 0,
                    error: Some("потока нет".to_owned()),
                },
            ))
            .unwrap();
        let acked = link.wait_for(WAIT, |receipt| match &receipt.event {
            Event::Ack { id, error, .. } => Some((*id, error.clone())),
            _ => None,
        });
        assert_eq!(acked.unwrap(), (id, Some("потока нет".to_owned())));
        assert_eq!(link.recv(WAIT).unwrap().event, signal("SIGWINCH"));
        assert_eq!(link.recv(Duration::ZERO), Err(LinkError::Timeout));
        assert_eq!(link.log().len(), 2);

        let next = id + 1;
        fixture
            .write_all(&receipt(
                3,
                Event::Ack {
                    id: next,
                    bytes: 5,
                    error: None,
                },
            ))
            .unwrap();
        assert_eq!(link.request(Command::Lifetime { ms: 1 }, WAIT), Ok(5));
    }

    #[test]
    fn listener_accepts_and_removes_its_socket() {
        let dir = std::env::temp_dir().join(format!(
            "pss-ch-{}-{}",
            std::process::id(),
            crate::sys::uptime_ns()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("ch.sock");
        let listener = Listener::bind(&path).unwrap();
        assert_eq!(listener.path(), path);
        assert!(matches!(
            listener.accept(Duration::ZERO),
            Err(LinkError::Timeout)
        ));
        let mut fixture = connect(&path).unwrap();
        let mut link = listener.accept(WAIT).unwrap();
        fixture.write_all(&receipt(1, signal("SIGHUP"))).unwrap();
        assert_eq!(link.recv(WAIT).unwrap().event, signal("SIGHUP"));
        drop(listener);
        assert!(!path.exists());
        std::fs::remove_dir(&dir).unwrap();
    }
}
