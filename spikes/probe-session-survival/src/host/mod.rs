//! Rust-хост пробы (план P0.4, «Rust-хост»): жизненный цикл, сокет (задача 11), VT с
//! восстановлением для attach, ответы на запросы и эффекты UI (задача 12). Процесс на сессию вне дерева запустившего: запускающий процесс выходит сразу, его
//! потомок делает `setsid` и запускает хост ещё одним `fork`, поэтому хост не лидер сессии и не
//! получает управляющий терминал. Хост публикует сокет до запуска нагрузки (проигравший
//! конкурентный `ensure` нагрузку не запускает), запускает нагрузку во втором PTY, пишет запись
//! реестра и отчитывается одной строкой [`Launch`] в stdout.
//!
//! Потоки: читатель PTY кладёт куски в очередь разбора с пределом в байтах и не ждёт клиентов;
//! поток разбора подаёт их в VT через автомат ответов (`responder`) и фильтр режимов отчётов,
//! пишет ответы в PTY, а поток клиентам отдаёт главному потоку. Главный поток ведёт соединения,
//! лидерство и очереди клиентов и никогда не ждёт GUI: чтение буфера обмена ждёт поток разбора.
//! Восстановление для attach и управляющий снимок делает поток разбора по команде, поэтому
//! граница «восстановление → живой поток» — номер байта PTY. Клиенту кадры идут через его
//! очередь с пределом в байтах, переполнение разрывает клиента с причиной; пределы очередей, номера
//! ввода и правило размера — в [`io`]. Версию протокола соединения выбирает поток соединения по
//! `Hello` ([`proto::Versions::choose`]); дальше кадры соединения в обе стороны идут её кодеком.

pub mod attach;
pub mod effects;
pub mod io;
pub mod pty;
pub mod registry;
pub mod responder;
pub mod socket;
#[cfg(feature = "vt")]
pub mod vt;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::backend::{HostRecord, SessionId};
use crate::pty::Winsize;

#[cfg(feature = "vt")]
use std::collections::BTreeMap;
#[cfg(feature = "vt")]
use std::io::Write;
#[cfg(feature = "vt")]
use std::os::unix::net::UnixStream;
#[cfg(feature = "vt")]
use std::process::ExitStatus;
#[cfg(feature = "vt")]
use std::sync::Arc;
#[cfg(feature = "vt")]
use std::sync::mpsc::{self, Receiver, SyncSender};
#[cfg(feature = "vt")]
use std::thread;

#[cfg(feature = "vt")]
use crate::backend::Role;
#[cfg(feature = "vt")]
use crate::bridge::ReportFilter;
#[cfg(feature = "vt")]
use crate::observer;
#[cfg(feature = "vt")]
use crate::proto::{self, Ack, Codec, Hello, Message, Purpose, Status, Welcome};
#[cfg(feature = "vt")]
use effects::Clipboard;
#[cfg(feature = "vt")]
use io::{ByteQueue, InputId, InputLedger, Leadership, Origin, Room};
#[cfg(feature = "vt")]
use responder::Responder;
#[cfg(feature = "vt")]
use socket::Point;

pub const HOST_NAME: &str = "probe-session-host";
/// Сокет точек остановки окна публикации: только проба.
pub const BREAK_ENV: &str = "PSS_HOST_BREAK";
/// Событий в очереди главного потока; кусок вывода в событии — не больше `pty::READ_CHUNK`.
pub const EVENT_QUEUE: usize = 256;
/// Срок записи одного кадра клиенту. Остановленный клиент разрывается раньше переполнением
/// очереди; срок освобождает писателя клиента, который так и не продолжил чтение.
pub const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Срок от `SIGHUP` нагрузке до `SIGKILL` оставшимся процессам сессии при `Stop`.
pub const STOP_GRACE: Duration = Duration::from_secs(1);

/// Аргументы `probe-session-host serve`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub dir: PathBuf,
    pub session: SessionId,
    pub size: Winsize,
    /// Метка запуска в `argv`: по ней стенд находит оставшиеся процессы.
    pub tag: String,
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Options {
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "serve".to_owned(),
            "--dir".to_owned(),
            self.dir.display().to_string(),
            "--session".to_owned(),
            self.session.to_string(),
            "--size".to_owned(),
            format!(
                "{}x{}x{}x{}",
                self.size.rows, self.size.cols, self.size.xpixel, self.size.ypixel
            ),
            "--tag".to_owned(),
            self.tag.clone(),
            "--".to_owned(),
            self.program.display().to_string(),
        ];
        args.extend(self.args.iter().cloned());
        args
    }

    pub fn parse(args: &[String]) -> Result<Options, String> {
        let mut rest = args.iter();
        if rest.next().map(String::as_str) != Some("serve") {
            return Err("ожидается подкоманда serve".to_owned());
        }
        let (mut dir, mut session, mut size, mut tag) = (None, None, None, None);
        loop {
            let flag = rest.next().ok_or("нет -- и нагрузки")?;
            if flag == "--" {
                break;
            }
            let value = rest.next().ok_or_else(|| format!("{flag}: нет значения"))?;
            match flag.as_str() {
                "--dir" => dir = Some(PathBuf::from(value)),
                "--session" => session = Some(SessionId::new(value)?),
                "--size" => size = Some(parse_size(value)?),
                "--tag" => tag = Some(value.clone()),
                other => return Err(format!("неизвестный флаг {other}")),
            }
        }
        let program = PathBuf::from(rest.next().ok_or("нет программы нагрузки")?);
        Ok(Options {
            dir: dir.ok_or("нет --dir")?,
            session: session.ok_or("нет --session")?,
            size: size.ok_or("нет --size")?,
            tag: tag.ok_or("нет --tag")?,
            program,
            args: rest.cloned().collect(),
        })
    }
}

fn parse_size(text: &str) -> Result<Winsize, String> {
    let parts: Vec<u16> = text
        .split('x')
        .map(|part| part.parse::<u16>())
        .collect::<Result<_, _>>()
        .map_err(|err| format!("--size {text}: {err}"))?;
    match parts[..] {
        [rows, cols, xpixel, ypixel] if rows > 0 && cols > 0 => Ok(Winsize {
            rows,
            cols,
            xpixel,
            ypixel,
        }),
        _ => Err(format!(
            "--size {text}: ожидается строки x колонки x пиксели x пиксели"
        )),
    }
}

/// Строка отчёта хоста в stdout запускающего.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "launch", content = "detail", rename_all = "kebab-case")]
pub enum Launch {
    Ready(HostRecord),
    /// Имя сессии занято другим хостом; нагрузка не запускалась.
    Busy(String),
    Failed(String),
}

#[cfg(feature = "vt")]
/// Вход `probe-session-host`.
pub fn main(args: &[String]) -> i32 {
    let options = match Options::parse(args) {
        Ok(options) => options,
        Err(err) => {
            eprintln!("{HOST_NAME}: {err}");
            return 2;
        }
    };
    // SAFETY: потоков ещё нет; после `fork` в потомке вызываются только `setsid`, `fork` и
    // `_exit` до возврата в обычный код единственного потока.
    unsafe {
        match libc::fork() {
            -1 => {
                eprintln!("{HOST_NAME}: fork: {}", std::io::Error::last_os_error());
                return 1;
            }
            0 => {}
            _ => libc::_exit(0),
        }
        if libc::setsid() < 0 {
            report(&Launch::Failed(format!(
                "setsid: {}",
                std::io::Error::last_os_error()
            )));
            libc::_exit(1);
        }
        match libc::fork() {
            -1 => {
                report(&Launch::Failed(format!(
                    "второй fork: {}",
                    std::io::Error::last_os_error()
                )));
                libc::_exit(1);
            }
            0 => {}
            _ => libc::_exit(0),
        }
    }
    serve(&options)
}

#[cfg(feature = "vt")]
fn report(launch: &Launch) {
    let line = serde_json::to_string(launch).unwrap_or_else(|_| "{}".to_owned());
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

#[cfg(feature = "vt")]
/// stdin и stdout на `/dev/null`: запускающий дочитывает stdout до конца.
fn detach_stdio() {
    if let Ok(null) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        use std::os::fd::AsRawFd;
        // SAFETY: дескрипторы открыты; `dup2` заменяет стандартные потоки.
        unsafe {
            libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO);
            libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO);
        }
    }
}

#[cfg(feature = "vt")]
fn pause_at(point: Point) {
    let Ok(path) = std::env::var(BREAK_ENV) else {
        return;
    };
    let Ok(mut stream) = UnixStream::connect(&path) else {
        eprintln!("{HOST_NAME}: точка {}: нет сокета {path}", point.name());
        return;
    };
    let _ = writeln!(stream, "{}", point.name());
    let mut go = [0u8; 1];
    let _ = std::io::Read::read(&mut stream, &mut go);
}

#[cfg(feature = "vt")]
fn serve(options: &Options) -> i32 {
    let pid = std::process::id() as i32;
    let published =
        match socket::publish(&options.dir, options.session.as_str(), pid, &mut pause_at) {
            Ok(published) => published,
            Err(socket::PublishError::Busy { path }) => {
                report(&Launch::Busy(path.display().to_string()));
                return 0;
            }
            Err(err) => {
                report(&Launch::Failed(err.to_string()));
                return 1;
            }
        };
    let env: Vec<(String, String)> = std::env::vars()
        .filter(|(name, _)| name != BREAK_ENV)
        .collect();
    let workload = match pty::spawn(&options.program, &options.args, &env, options.size) {
        Ok(workload) => workload,
        Err(err) => {
            let _ = published.unpublish();
            report(&Launch::Failed(format!("запуск нагрузки: {err}")));
            return 1;
        }
    };
    let record = match describe(options, &workload) {
        Ok(record) => record,
        Err(err) => {
            let _ = published.unpublish();
            report(&Launch::Failed(err));
            return 1;
        }
    };
    if let Err(err) = registry::write(&options.dir, &record) {
        let _ = published.unpublish();
        report(&Launch::Failed(format!("запись реестра: {err}")));
        return 1;
    }
    report(&Launch::Ready(record.clone()));
    detach_stdio();
    let code = match Hub::start(options, record, workload, &published) {
        Ok(hub) => hub.run(),
        Err(err) => {
            eprintln!("{HOST_NAME}: {err}");
            1
        }
    };
    let _ = published.unpublish();
    let _ = std::fs::remove_file(registry::record_path(
        &options.dir,
        options.session.as_str(),
    ));
    code
}

#[cfg(feature = "vt")]
fn describe(options: &Options, workload: &pty::Workload) -> Result<HostRecord, String> {
    let host = observer::current()?;
    let info = observer::inspect(workload.pid())?
        .ok_or_else(|| format!("нагрузка {} вышла до записи", workload.pid()))?;
    Ok(HostRecord {
        session: options.session.clone(),
        host: host.key,
        workload: info.key,
        workload_pgid: info.pgid,
        workload_sid: info.sid,
    })
}

// --- очередь разбора ---

pub type ConnId = u64;

/// Предел байтов PTY в очереди разбора (модель задачи 8): пока поток разбора ждёт GUI, чтение
/// продолжается до предела, затем ждёт.
pub const PARSE_QUEUE_BYTES: usize = 4 << 20;
/// Кадр восстановления: меньше `proto::MAX_BODY`.
pub const RESTORE_FRAME: usize = 512 << 10;
/// Срок дочитывания PTY после выхода нагрузки: фоновый потомок может держать ведомую сторону.
pub const EXIT_DRAIN: Duration = Duration::from_secs(2);

/// Вход потока разбора: куски PTY и команды главного потока в одном порядке.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseIn {
    Bytes(Vec<u8>),
    End(String),
    Resize(Winsize),
    Focus(bool),
    Visible(bool),
    Restore(ConnId),
    Snapshot(ConnId),
}

/// Очередь разбора: байты PTY ждут места сверх предела, команды не ждут никогда.
pub struct ParseQueue {
    state: Mutex<(VecDeque<ParseIn>, usize)>,
    ready: Condvar,
    room: Condvar,
    limit: usize,
}

impl ParseQueue {
    pub fn new(limit: usize) -> ParseQueue {
        ParseQueue {
            state: Mutex::new((VecDeque::new(), 0)),
            ready: Condvar::new(),
            room: Condvar::new(),
            limit,
        }
    }

    fn lock(&self) -> MutexGuard<'_, (VecDeque<ParseIn>, usize)> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn push_bytes(&self, bytes: Vec<u8>) {
        let mut state = self.lock();
        while state.1 > 0 && state.1 + bytes.len() > self.limit {
            state = self
                .room
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.1 += bytes.len();
        state.0.push_back(ParseIn::Bytes(bytes));
        self.ready.notify_one();
    }

    pub fn push(&self, item: ParseIn) {
        let mut state = self.lock();
        if let ParseIn::Bytes(bytes) = &item {
            state.1 += bytes.len();
        }
        state.0.push_back(item);
        self.ready.notify_one();
    }

    pub fn pop(&self) -> ParseIn {
        let mut state = self.lock();
        loop {
            if let Some(item) = state.0.pop_front() {
                if let ParseIn::Bytes(bytes) = &item {
                    state.1 -= bytes.len();
                    self.room.notify_all();
                }
                return item;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub fn queued_bytes(&self) -> usize {
        self.lock().1
    }
}

/// Счётчики хоста для `Status`.
#[derive(Debug, Default)]
pub struct Counters {
    pub reads: AtomicU64,
    pub read_bytes: AtomicU64,
    pub parsed: AtomicU64,
    pub replies: AtomicU64,
    pub dropped: AtomicU64,
}

impl Counters {
    pub fn get(value: &AtomicU64) -> u64 {
        value.load(Ordering::Acquire)
    }

    pub fn add(value: &AtomicU64, by: usize) {
        value.fetch_add(by as u64, Ordering::AcqRel);
    }
}

// --- главный поток ---

#[cfg(feature = "vt")]
enum Event {
    Output {
        offset: u64,
        bytes: Vec<u8>,
    },
    Parsed {
        read: u64,
        offset: u64,
        held: u32,
        ground: bool,
    },
    Restored {
        id: ConnId,
        offset: u64,
        result: Result<vt::Restored, String>,
    },
    Snapshot {
        id: ConnId,
        result: Result<crate::backend::Snapshot, String>,
    },
    /// Поток разбора дочитал PTY до конца.
    Drained,
    Exited(ExitStatus),
    /// Новое соединение: `Hello` и версия, выбранная по нему, или причина отказа.
    Conn(ConnId, UnixStream, Hello, Result<Codec, String>),
    Msg(ConnId, Message),
    Closed(ConnId),
    /// Запись клиенту не удалась: срок или разрыв соединения.
    WriteFailed(ConnId, String),
    StopDeadline,
    DrainDeadline,
}

#[cfg(feature = "vt")]
struct Conn {
    purpose: Purpose,
    /// Кодек версии соединения; у отвергнутого — кодек рукопожатия.
    codec: Codec,
    /// Наибольшая версия клиента из `Hello`.
    client_max: u16,
    role: Role,
    out: Arc<ByteQueue>,
    stream: UnixStream,
    /// Номер байта PTY, с которого attach-клиент получает живой поток; `None` — ещё не
    /// восстановлен.
    live_from: Option<u64>,
}

#[cfg(feature = "vt")]
struct Hub<'a> {
    options: &'a Options,
    record: HostRecord,
    terminal: pty::Terminal,
    input: pty::InputQueue,
    queue: Arc<ParseQueue>,
    counters: Arc<Counters>,
    clipboard: Arc<Clipboard>,
    events: Receiver<Event>,
    sender: SyncSender<Event>,
    conns: BTreeMap<ConnId, Conn>,
    lead: Leadership<ConnId>,
    ledger: InputLedger,
    evictions: u32,
    size: Winsize,
    stopping: bool,
    exit: Option<ExitStatus>,
    drained: bool,
    vt_version: String,
}

/// Поток разбора: VT, автомат ответов и фильтр режимов отчётов.
#[cfg(feature = "vt")]
struct Parser {
    queue: Arc<ParseQueue>,
    events: SyncSender<Event>,
    input: pty::InputQueue,
    counters: Arc<Counters>,
}

#[cfg(feature = "vt")]
impl Parser {
    fn reply(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        Counters::add(&self.counters.replies, bytes.len());
        if let Err(reason) = self.input.reply(bytes) {
            eprintln!("{HOST_NAME}: ответ хоста не записан: {reason}");
        }
    }

    fn run(self, mut vt: vt::HostVt) {
        let mut responder = Responder::new();
        let mut filter = ReportFilter::new();
        let mut offset = 0u64;
        loop {
            match self.queue.pop() {
                ParseIn::Bytes(bytes) => {
                    let start = offset;
                    offset += bytes.len() as u64;
                    let fed = responder.feed(&mut vt, &bytes);
                    Counters::add(&self.counters.parsed, bytes.len());
                    Counters::add(&self.counters.dropped, fed.dropped.len());
                    self.reply(fed.replies);
                    let client = filter.push(&fed.client);
                    let held = responder.held().len() + filter.held().len();
                    if !client.is_empty() {
                        let _ = self.events.send(Event::Output {
                            offset: start,
                            bytes: client,
                        });
                    }
                    let _ = self.events.send(Event::Parsed {
                        read: bytes.len() as u64,
                        offset,
                        held: held as u32,
                        ground: vt.ground(),
                    });
                }
                ParseIn::End(reason) => {
                    eprintln!("{HOST_NAME}: конец PTY: {reason}");
                    let _ = self.events.send(Event::Drained);
                    return;
                }
                ParseIn::Resize(size) => match vt.resize(size) {
                    Ok(report) => self.reply(report),
                    Err(err) => eprintln!("{HOST_NAME}: ресайз VT: {err}"),
                },
                ParseIn::Focus(gained) => self.reply(vt.focus(gained)),
                ParseIn::Visible(visible) => self.reply(vt.visibility(visible)),
                ParseIn::Restore(id) => {
                    let mut held = filter.held().to_vec();
                    held.extend_from_slice(responder.held());
                    let _ = self.events.send(Event::Restored {
                        id,
                        offset,
                        result: vt.restore(&held),
                    });
                }
                ParseIn::Snapshot(id) => {
                    let _ = self.events.send(Event::Snapshot {
                        id,
                        result: vt.snapshot(),
                    });
                }
            }
        }
    }
}

#[cfg(feature = "vt")]
impl<'a> Hub<'a> {
    fn start(
        options: &'a Options,
        record: HostRecord,
        workload: pty::Workload,
        published: &socket::Published,
    ) -> Result<Hub<'a>, String> {
        let (sender, events) = mpsc::sync_channel(EVENT_QUEUE);
        let queue = Arc::new(ParseQueue::new(PARSE_QUEUE_BYTES));
        let counters = Arc::new(Counters::default());
        let clipboard = Arc::new(Clipboard::new(effects::CLIPBOARD_TIMEOUT));
        let size = workload.terminal.size().unwrap_or(options.size);
        let host_vt = vt::HostVt::new(size, Arc::clone(&clipboard))
            .map_err(|err| format!("VT хоста: {err}"))?;
        let input = workload
            .terminal
            .start_writer()
            .map_err(|err| format!("поток записи PTY: {err}"))?;
        let (reads, ends) = (Arc::clone(&queue), Arc::clone(&queue));
        let read_counters = Arc::clone(&counters);
        workload
            .terminal
            .start_reader(
                move |bytes| {
                    Counters::add(&read_counters.reads, 1);
                    Counters::add(&read_counters.read_bytes, bytes.len());
                    reads.push_bytes(bytes);
                    true
                },
                move |reason| ends.push(ParseIn::End(reason)),
            )
            .map_err(|err| format!("поток чтения PTY: {err}"))?;
        let parser = Parser {
            queue: Arc::clone(&queue),
            events: sender.clone(),
            input: input.clone(),
            counters: Arc::clone(&counters),
        };
        // VT создаётся здесь, но живёт только в потоке разбора: терминал передаётся туда целиком.
        let parse_vt = SendVt(host_vt);
        thread::Builder::new()
            .name("parse".to_owned())
            .spawn(move || {
                let SendVt(host_vt) = parse_vt;
                parser.run(host_vt);
            })
            .map_err(|err| format!("поток разбора: {err}"))?;
        let pty::Workload {
            mut child,
            terminal,
        } = workload;
        let exits = sender.clone();
        thread::Builder::new()
            .name("workload-wait".to_owned())
            .spawn(move || {
                if let Ok(status) = child.wait() {
                    let _ = exits.send(Event::Exited(status));
                }
            })
            .map_err(|err| format!("поток ожидания нагрузки: {err}"))?;
        let listener = published
            .listener
            .try_clone()
            .map_err(|err| format!("слушатель: {err}"))?;
        let accepts = sender.clone();
        let deliveries = Arc::clone(&clipboard);
        thread::Builder::new()
            .name("accept".to_owned())
            .spawn(move || accept_loop(listener, accepts, deliveries))
            .map_err(|err| format!("поток приёма: {err}"))?;
        Ok(Hub {
            options,
            record,
            terminal,
            input,
            queue,
            counters,
            clipboard,
            events,
            sender,
            conns: BTreeMap::new(),
            lead: Leadership::default(),
            ledger: InputLedger::default(),
            evictions: 0,
            size,
            stopping: false,
            exit: None,
            drained: false,
            vt_version: crate::vt::library_version()
                .unwrap_or_else(|err| format!("неизвестна: {err}")),
        })
    }

    fn run(mut self) -> i32 {
        while let Ok(event) = self.events.recv() {
            match event {
                Event::Output { offset, bytes } => self.output(offset, bytes),
                Event::Parsed {
                    read,
                    offset,
                    held,
                    ground,
                } => self.parsed(read, offset, held, ground),
                Event::Restored { id, offset, result } => self.restored(id, offset, result),
                Event::Snapshot { id, result } => {
                    let reply = match result {
                        Ok(snapshot) => Message::Snapshot(snapshot),
                        Err(reason) => Message::Refused(reason),
                    };
                    self.send(id, &reply);
                }
                Event::Drained => {
                    self.drained = true;
                    if let Some(status) = self.exit {
                        self.exited(status);
                        return 0;
                    }
                }
                Event::Exited(status) => {
                    self.exit = Some(status);
                    if self.drained {
                        self.exited(status);
                        return 0;
                    }
                    self.after(EXIT_DRAIN, Event::DrainDeadline);
                }
                Event::DrainDeadline => {
                    if let Some(status) = self.exit {
                        eprintln!(
                            "{HOST_NAME}: PTY не дочитан за {} мс после выхода нагрузки",
                            EXIT_DRAIN.as_millis()
                        );
                        self.exited(status);
                        return 0;
                    }
                }
                Event::Conn(id, stream, hello, codec) => self.connect(id, stream, hello, codec),
                Event::Msg(id, message) => self.message(id, message),
                Event::Closed(id) => self.drop_conn(id, None),
                Event::WriteFailed(id, reason) => {
                    self.drop_conn(id, Some(format!("запись клиенту: {reason}")));
                }
                Event::StopDeadline => self.kill_session(),
            }
        }
        1
    }

    fn after(&self, delay: Duration, event: Event) {
        let sender = self.sender.clone();
        let _ = thread::Builder::new()
            .name("deadline".to_owned())
            .spawn(move || {
                let (_keep, wait) = mpsc::channel::<()>();
                let _ = wait.recv_timeout(delay);
                let _ = sender.send(event);
            });
    }

    /// Живой поток и события разбора идут в пределе очереди клиента, остальное — вне его: их
    /// объём ограничен состоянием VT и числом запросов.
    fn send(&mut self, id: ConnId, message: &Message) {
        let Some(conn) = self.conns.get(&id) else {
            return;
        };
        let room = match message {
            Message::Output { .. } | Message::Parsed { .. } => Room::Limit,
            _ => Room::Always,
        };
        match conn.out.push(proto::encode(conn.codec, message), room) {
            Ok(()) => {}
            Err(io::Refusal::Full {
                len,
                counted,
                limit,
            }) => self.evict(
                id,
                format!(
                    "очередь вывода клиента переполнена: кадр {len} байт, занято {counted} из \
                     {limit}"
                ),
            ),
            Err(io::Refusal::Closed) => self.drop_conn(id, None),
        }
    }

    /// Разрыв медленного клиента: его очередь очищается, последним кадром идёт причина. Сокет
    /// закрывает писатель клиента, дописав её; переподключение получает новое восстановление.
    fn evict(&mut self, id: ConnId, reason: String) {
        let Some(conn) = self.conns.remove(&id) else {
            return;
        };
        let dropped = conn
            .out
            .evict(proto::encode(conn.codec, &Message::Evicted(reason.clone())));
        self.evictions += 1;
        eprintln!("{HOST_NAME}: клиент {id} разорван: {reason}; отброшено {dropped} байт очереди");
        self.forget(id, conn.purpose);
    }

    fn drop_conn(&mut self, id: ConnId, reason: Option<String>) {
        let Some(conn) = self.conns.remove(&id) else {
            return;
        };
        conn.out.close();
        let _ = conn.stream.shutdown(std::net::Shutdown::Read);
        if let Some(reason) = reason {
            eprintln!("{HOST_NAME}: клиент {id} разорван: {reason}");
        }
        self.forget(id, conn.purpose);
    }

    fn forget(&mut self, id: ConnId, purpose: Purpose) {
        if purpose == Purpose::Gui {
            self.clipboard.unbind(id);
        }
        if self.lead.detach(id) {
            self.clipboard.set_leader(None);
        }
    }

    fn ids(&self, purpose: Purpose) -> Vec<ConnId> {
        self.conns
            .iter()
            .filter(|(_, conn)| conn.purpose == purpose)
            .map(|(id, _)| *id)
            .collect()
    }

    fn output(&mut self, offset: u64, bytes: Vec<u8>) {
        let message = Message::Output { offset, bytes };
        let live: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, conn)| {
                conn.purpose == Purpose::Attach && conn.live_from.is_some_and(|from| from <= offset)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in live {
            self.send(id, &message);
        }
    }

    fn parsed(&mut self, read: u64, offset: u64, held: u32, ground: bool) {
        let message = Message::Parsed {
            read,
            offset,
            held,
            ground,
        };
        for id in self.ids(Purpose::Observe) {
            self.send(id, &message);
        }
    }

    fn welcome(&self, codec: Codec, role: Role, offset: u64) -> Message {
        Message::Welcome(Welcome {
            version: codec.version(),
            host: format!(
                "{HOST_NAME} {}, протокол {}",
                env!("CARGO_PKG_VERSION"),
                proto::SUPPORTED
            ),
            vt: self.vt_version.clone(),
            session: self.options.session.to_string(),
            role,
            generation: self.lead.generation(),
            offset,
        })
    }

    fn restored(&mut self, id: ConnId, offset: u64, result: Result<vt::Restored, String>) {
        let Some(conn) = self.conns.get(&id) else {
            return;
        };
        let (role, codec, client_max) = (conn.role, conn.codec, conn.client_max);
        let restored = match result {
            Ok(restored) => restored,
            Err(reason) => {
                eprintln!("{HOST_NAME}: attach {id}: отказ восстановления: {reason}");
                self.send(id, &Message::Refused(reason));
                self.drop_conn(id, None);
                return;
            }
        };
        eprintln!(
            "{HOST_NAME}: attach {id}: граница {offset}, восстановление {} байт, continuation {}, \
             удержано {}, протокол {} (клиент до {client_max}){}",
            restored.bytes.len(),
            restored.continuation,
            restored.held,
            codec.version(),
            if restored.inexpressible.is_empty() {
                String::new()
            } else {
                format!(", не выражается: {}", restored.inexpressible.join("; "))
            }
        );
        let welcome = self.welcome(codec, role, offset);
        self.send(id, &welcome);
        for chunk in restored.bytes.chunks(RESTORE_FRAME) {
            self.send(id, &Message::Restore(chunk.to_vec()));
        }
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.live_from = Some(offset);
        }
    }

    fn resize(&mut self, size: Winsize) {
        if size == self.size {
            return;
        }
        match self.terminal.resize(size) {
            Ok(()) => {
                self.size = size;
                self.queue.push(ParseIn::Resize(size));
            }
            Err(err) => eprintln!("{HOST_NAME}: ресайз: {err}"),
        }
    }

    fn connect(
        &mut self,
        id: ConnId,
        stream: UnixStream,
        hello: Hello,
        negotiated: Result<Codec, String>,
    ) {
        let writer = match stream.try_clone() {
            Ok(writer) => writer,
            Err(err) => {
                eprintln!("{HOST_NAME}: соединение {id}: {err}");
                return;
            }
        };
        let _ = writer.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
        let out = Arc::new(ByteQueue::new(io::CLIENT_QUEUE_BYTES));
        let queue = Arc::clone(&out);
        let failed = self.sender.clone();
        let spawned = thread::Builder::new()
            .name(format!("client-{id}"))
            .spawn(move || {
                let mut writer = writer;
                match io::drain(&queue, &mut writer) {
                    Ok(()) => {
                        let _ = writer.shutdown(std::net::Shutdown::Write);
                    }
                    Err(err) => {
                        let _ = failed.send(Event::WriteFailed(id, err.to_string()));
                    }
                }
            });
        if spawned.is_err() {
            return;
        }
        self.conns.insert(
            id,
            Conn {
                purpose: hello.purpose,
                codec: negotiated.clone().unwrap_or(Codec::HANDSHAKE),
                client_max: hello.version,
                role: Role::Unowned,
                out,
                stream,
                live_from: None,
            },
        );
        let codec = match negotiated {
            Ok(codec) => codec,
            Err(reason) => {
                eprintln!(
                    "{HOST_NAME}: соединение {id} {:?} «{}»: {reason}",
                    hello.purpose, hello.client
                );
                self.send(id, &Message::Refused(reason));
                self.drop_conn(id, None);
                return;
            }
        };
        if hello.purpose != Purpose::Attach {
            let welcome = self.welcome(codec, Role::Unowned, Counters::get(&self.counters.parsed));
            self.send(id, &welcome);
            return;
        }
        let attached = self.lead.attach(id, hello.lead, hello.size);
        if attached.role == Role::Leader {
            self.clipboard.set_leader(Some(attached.generation));
        }
        if let Some(previous) = attached.demoted {
            if let Some(conn) = self.conns.get_mut(&previous) {
                conn.role = Role::Follower;
            }
            if self
                .conns
                .get(&previous)
                .is_some_and(|conn| conn.live_from.is_some())
            {
                self.send(
                    previous,
                    &Message::Role {
                        role: Role::Follower,
                        generation: attached.generation,
                    },
                );
            }
        }
        if let Some(size) = attached.resize {
            self.resize(size);
        }
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.role = attached.role;
        }
        self.queue.push(ParseIn::Restore(id));
    }

    /// Ввод с номером: от управляющего соединения и от лидера; повтор принятого номера в PTY не
    /// пишется, переполнение очереди — явный отказ целиком. Без `ack_wanted` подтверждение не
    /// шлётся, отказ остаётся в журнале.
    fn input(
        &mut self,
        id: ConnId,
        origin: Origin<ConnId>,
        input: InputId,
        bytes: Vec<u8>,
        ack_wanted: bool,
    ) {
        let len = bytes.len();
        let ack = if !self.lead.accepts_input(origin) {
            Ack::Rejected("ввод без лидерства".to_owned())
        } else if self.ledger.is_duplicate(input) {
            Ack::Duplicate
        } else {
            match self.input.push(bytes) {
                Ok(()) => {
                    self.ledger.record(input);
                    Ack::Accepted
                }
                Err(reason) => Ack::Rejected(reason),
            }
        };
        if ack != Ack::Accepted {
            eprintln!(
                "{HOST_NAME}: ввод {len} байт соединения {id} (источник {:x}, номер {}): {ack:?}",
                input.source, input.seq
            );
        }
        if ack_wanted {
            self.send(
                id,
                &Message::InputAck {
                    source: input.source,
                    seq: input.seq,
                    ack,
                },
            );
        }
    }

    fn message(&mut self, id: ConnId, message: Message) {
        let purpose = match self.conns.get(&id) {
            Some(conn) => conn.purpose,
            None => return,
        };
        let control = purpose == Purpose::Control;
        let origin = if control {
            Some(Origin::Control)
        } else {
            (purpose == Purpose::Attach).then_some(Origin::Client(id))
        };
        match message {
            Message::Input {
                source,
                seq,
                bytes,
                ack,
            } if origin.is_some() => {
                if let Some(origin) = origin {
                    self.input(id, origin, InputId { source, seq }, bytes, ack);
                }
            }
            Message::Resize(size) => {
                if origin.is_some_and(|origin| self.lead.accepts_resize(origin)) {
                    self.resize(size);
                }
                if control {
                    self.send(id, &Message::Done);
                }
            }
            Message::GuiBind { generation } if purpose == Purpose::Gui => {
                if let Some(conn) = self.conns.get(&id) {
                    self.clipboard
                        .bind(id, generation, conn.codec, Arc::clone(&conn.out));
                }
                self.send(id, &Message::Done);
            }
            Message::Focus(_) | Message::Visible(_) if purpose == Purpose::Gui => {
                if !self.clipboard.current(id) {
                    self.send(
                        id,
                        &Message::Refused("GUI не текущего поколения лидера".to_owned()),
                    );
                    return;
                }
                self.queue.push(match message {
                    Message::Focus(gained) => ParseIn::Focus(gained),
                    Message::Visible(visible) => ParseIn::Visible(visible),
                    _ => return,
                });
                self.send(id, &Message::Done);
            }
            Message::SnapshotRequest => self.queue.push(ParseIn::Snapshot(id)),
            Message::StatusRequest => {
                let status = Message::Status(Status {
                    host: self.record.host,
                    workload: self.record.workload,
                    workload_pgid: self.record.workload_pgid,
                    workload_sid: self.record.workload_sid,
                    clients: self.ids(Purpose::Attach).len() as u32,
                    leader: self.lead.leader().is_some(),
                    reads: Counters::get(&self.counters.reads),
                    read_bytes: Counters::get(&self.counters.read_bytes),
                    parsed_bytes: Counters::get(&self.counters.parsed),
                    replies: Counters::get(&self.counters.replies),
                    dropped_replies: Counters::get(&self.counters.dropped),
                    clipboard: self.clipboard.stats(),
                    size: self.size,
                    generation: self.lead.generation(),
                    evictions: self.evictions,
                    input_queued: self.input.queued() as u64,
                });
                self.send(id, &status);
            }
            Message::Stop => {
                self.send(id, &Message::Done);
                self.stop();
            }
            Message::Detach => {
                self.send(id, &Message::Done);
                self.drop_conn(id, None);
            }
            other => {
                self.send(
                    id,
                    &Message::Refused(format!("сообщение не для хоста: {other:?}")),
                );
            }
        }
    }

    fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        let pgid = self.record.workload_pgid;
        let _ = crate::backend::signal(-pgid, libc::SIGHUP);
        let _ = crate::backend::signal(-pgid, libc::SIGCONT);
        self.after(STOP_GRACE, Event::StopDeadline);
    }

    /// `SIGKILL` всем живым процессам сессии нагрузки, кроме самого хоста.
    fn kill_session(&self) {
        let processes = registry::all_processes().unwrap_or_default();
        for key in registry::remaining(&self.record, &processes) {
            if key != self.record.host {
                let _ = crate::backend::signal(key.pid, libc::SIGKILL);
            }
        }
    }

    /// Нагрузка вышла и PTY дочитан (или вышел срок): клиенты получают выход, их очереди
    /// дописываются до конца не дольше [`io::EXIT_FLUSH`], затем хост выходит.
    fn exited(&mut self, status: ExitStatus) {
        use std::os::unix::process::ExitStatusExt;
        if self.stopping {
            self.kill_session();
        }
        let message = Message::Exited {
            code: status.code(),
            signal: status.signal(),
        };
        for id in self.ids(Purpose::Attach) {
            self.send(id, &message);
        }
        let queued = self.queue.queued_bytes();
        if queued > 0 {
            eprintln!("{HOST_NAME}: при выходе в очереди разбора {queued} байт");
        }
        let conns = std::mem::take(&mut self.conns);
        for conn in conns.values() {
            conn.out.close();
        }
        let deadline = std::time::Instant::now() + io::EXIT_FLUSH;
        for (id, conn) in &conns {
            if !conn.out.wait_finished(deadline) {
                eprintln!(
                    "{HOST_NAME}: клиент {id} не дочитал вывод за {} мс до выхода хоста",
                    io::EXIT_FLUSH.as_millis()
                );
            }
            let _ = conn.stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Терминал VT переходит в поток разбора целиком и больше нигде не используется.
#[cfg(feature = "vt")]
struct SendVt(vt::HostVt);

// SAFETY: `HostVt` создан в главном потоке до запуска потока разбора, передаётся туда один раз и
// используется только там; колбэки терминала держат `Arc` на потокобезопасные данные.
#[cfg(feature = "vt")]
unsafe impl Send for SendVt {}

#[cfg(feature = "vt")]
fn accept_loop(
    listener: std::os::unix::net::UnixListener,
    events: SyncSender<Event>,
    clipboard: Arc<Clipboard>,
) {
    let mut next: ConnId = 1;
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let id = next;
        next += 1;
        let events = events.clone();
        let clipboard = Arc::clone(&clipboard);
        let _ = thread::Builder::new()
            .name(format!("conn-{id}"))
            .spawn(move || conn_loop(id, stream, events, &clipboard));
    }
}

/// Чтение кадров соединения. Ответ GUI на чтение буфера обмена доставляется прямо в
/// [`Clipboard`]: главный поток в это время может быть занят, а поток разбора ждёт ответ.
#[cfg(feature = "vt")]
fn conn_loop(id: ConnId, stream: UnixStream, events: SyncSender<Event>, clipboard: &Clipboard) {
    let mut reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(_) => return,
    };
    let hello = match proto::read_message(&mut reader, Codec::HANDSHAKE) {
        Ok(Some(Message::Hello(hello))) => hello,
        _ => return,
    };
    let negotiated = proto::SUPPORTED.choose(hello.version);
    let codec = negotiated.clone();
    if events
        .send(Event::Conn(id, stream, hello, negotiated))
        .is_err()
    {
        return;
    }
    let Ok(codec) = codec else {
        return;
    };
    loop {
        match proto::read_message(&mut reader, codec) {
            Ok(Some(Message::ClipboardReply {
                serial,
                generation,
                data,
            })) => {
                if clipboard.deliver(id, serial, generation, data) == effects::Delivery::Dropped {
                    eprintln!(
                        "{HOST_NAME}: ответ GUI {id} на чтение буфера обмена {serial} поколения \
                         {generation} отброшен"
                    );
                }
            }
            Ok(Some(message)) => {
                if events.send(Event::Msg(id, message)).is_err() {
                    return;
                }
            }
            Ok(None) | Err(_) => {
                let _ = events.send(Event::Closed(id));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_round_trip_through_argv() {
        let options = Options {
            dir: PathBuf::from("/tmp/pss-n/r"),
            session: SessionId::new("sn-a1k").unwrap(),
            size: Winsize {
                rows: 24,
                cols: 80,
                xpixel: 640,
                ypixel: 384,
            },
            tag: "pss-n".to_owned(),
            program: PathBuf::from("/bin/fixture"),
            args: vec!["--mode".to_owned(), "emitter".to_owned(), "--".to_owned()],
        };
        assert_eq!(Options::parse(&options.to_args()), Ok(options));
        assert!(Options::parse(&["serve".to_owned(), "--dir".to_owned()]).is_err());
        assert!(parse_size("0x80x0x0").is_err());
    }

    #[test]
    fn parse_queue_keeps_order_and_counts_only_bytes() {
        let queue = ParseQueue::new(8);
        queue.push_bytes(b"abcd".to_vec());
        queue.push(ParseIn::Restore(3));
        queue.push_bytes(b"efgh".to_vec());
        assert_eq!(queue.queued_bytes(), 8);
        // Команда не ждёт места.
        queue.push(ParseIn::Focus(true));
        assert_eq!(queue.pop(), ParseIn::Bytes(b"abcd".to_vec()));
        assert_eq!(queue.pop(), ParseIn::Restore(3));
        assert_eq!(queue.queued_bytes(), 4);
        assert_eq!(queue.pop(), ParseIn::Bytes(b"efgh".to_vec()));
        assert_eq!(queue.pop(), ParseIn::Focus(true));
        assert_eq!(queue.queued_bytes(), 0);
    }

    #[test]
    fn parse_queue_holds_the_reader_until_there_is_room() {
        let queue = std::sync::Arc::new(ParseQueue::new(4));
        queue.push_bytes(b"abcd".to_vec());
        let writer = std::sync::Arc::clone(&queue);
        let reader = std::thread::spawn(move || writer.push_bytes(b"ef".to_vec()));
        assert_eq!(queue.pop(), ParseIn::Bytes(b"abcd".to_vec()));
        reader.join().unwrap();
        assert_eq!(queue.pop(), ParseIn::Bytes(b"ef".to_vec()));
    }

    #[test]
    fn launch_lines_are_tagged() {
        let line = serde_json::to_string(&Launch::Busy("/tmp/x".to_owned())).unwrap();
        assert_eq!(line, r#"{"launch":"busy","detail":"/tmp/x"}"#);
        assert_eq!(
            serde_json::from_str::<Launch>(&line).unwrap(),
            Launch::Busy("/tmp/x".to_owned())
        );
    }
}
