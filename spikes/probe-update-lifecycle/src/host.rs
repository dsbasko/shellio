//! Минимальный хост пробы (план P0.5, «Протокол хоста», раскладка cargo `probe-update-host`):
//! процесс на сессию вне дерева запустившего, нагрузка во втором PTY лидером своей сессии,
//! сокет сессии, чтение PTY без клиентов в хвост с номерами байтов, ввод с ответом хоста и запись
//! сессии в каталоге состояния.
//!
//! Запуск — как в P0.4: запускающий процесс выходит сразу после `fork`, его потомок делает
//! `setsid` и запускает хост ещё одним `fork`, поэтому хост не лидер сессии и не получает
//! управляющий терминал. Хост сообщает итог запуска одной строкой [`Launch`] в stdout и
//! переводит stdin и stdout на `/dev/null`.
//!
//! Рукопожатие решает [`SessionState::admit`] до любого изменения сессии: `Refused` и `probe`
//! состояние не меняют, `attach` становится лидером и задаёт размер. Хост живёт, пока жива
//! нагрузка; `Stop` посылает группе нагрузки `SIGHUP`, по истечении [`STOP_GRACE`] — `SIGKILL`.
//! Перед выходом хост убирает свой сокет (только если под путём всё ещё его узел) и запись.
//!
//! Источник — `spikes/probe-session-survival/src/host/mod.rs`, `host/socket.rs`,
//! `host/registry.rs`, упрощено: без VT и восстановления экрана, ответов на запросы, ролей GUI,
//! очередей с пределом и атомарной публикации через временное имя. Сессии в P0.5 запускает
//! единственный владелец под блокировкой, поэтому занятый путь сокета — отказ запуска.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::observer::{self, ProcessKey};
use crate::proto::{
    self, Ack, HOST_VERSIONS, Hello, Message, OUTPUT_CHUNK, Purpose, Status, Versions, Welcome,
};
use crate::pty::{self, Spawn, Winsize};
use crate::spawn;

pub const HOST_NAME: &str = "probe-update-host";
/// Каталог сокетов и записей сессий внутри каталога состояния.
pub const SESSIONS_DIR: &str = "s";
/// Предел хранимого хвоста вывода в байтах; вытесненное начало сдвигает `start`.
pub const LOG_LIMIT: usize = 4 << 20;
/// Срок `Hello` от подключения.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Срок записи одного кадра клиенту.
pub const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// От `SIGHUP` группе нагрузки до `SIGKILL` при `Stop`.
pub const STOP_GRACE: Duration = Duration::from_secs(2);
/// Срок дочитывания PTY после выхода нагрузки.
pub const EXIT_DRAIN: Duration = Duration::from_secs(2);
/// Наибольший срок `AwaitLines`.
pub const MAX_AWAIT: Duration = Duration::from_secs(60);
/// Срок строки [`Launch`] от запуска хоста.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);
/// `sockaddr_un.sun_path` на macOS — 104 байта вместе с нулём.
pub const MAX_SOCKET_PATH: usize = 103;
const MAX_SESSION_NAME: usize = 32;
const READ_CHUNK: usize = 64 << 10;

/// Путь сокета сессии.
pub fn socket_path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.sock"))
}

/// Путь записи сессии.
pub fn record_path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.json"))
}

/// Путь сокета помещается в `sun_path`; иначе явный отказ, а не обрезанный путь.
pub fn check_socket_path(path: &Path) -> Result<(), String> {
    let len = path.as_os_str().as_bytes().len();
    if len > MAX_SOCKET_PATH {
        return Err(format!(
            "путь сокета {} — {len} байт, больше {MAX_SOCKET_PATH}",
            path.display()
        ));
    }
    Ok(())
}

/// Имя сессии: латиница, цифры и дефис, не длиннее [`MAX_SESSION_NAME`].
pub fn check_session_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= MAX_SESSION_NAME
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "имя сессии «{name}»: только латиница, цифры и дефис, до {MAX_SESSION_NAME}"
        ))
    }
}

/// Аргументы `probe-update-host serve`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Каталог сокетов и записей сессий.
    pub dir: PathBuf,
    pub session: String,
    pub size: Winsize,
    /// Метка запуска в `argv`: по ней внешний сторож находит оставшиеся процессы.
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
            self.session.clone(),
            "--size".to_owned(),
            format!("{}x{}", self.size.rows, self.size.cols),
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
                "--session" => {
                    check_session_name(value)?;
                    session = Some(value.clone());
                }
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

/// `строкиxколонки`.
pub fn parse_size(text: &str) -> Result<Winsize, String> {
    let parsed = text
        .split_once('x')
        .and_then(|(rows, cols)| Some((rows.parse::<u16>().ok()?, cols.parse::<u16>().ok()?)));
    match parsed {
        Some((rows, cols)) if rows > 0 && cols > 0 => Ok(Winsize::cells(rows, cols)),
        _ => Err(format!("размер «{text}»: ожидается строкиxколонки")),
    }
}

/// Запись сессии в каталоге состояния. Хост пишет её атомарно после запуска нагрузки и убирает
/// при выходе; после гибели хоста по ней находятся оставшиеся процессы.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session: String,
    pub host: ProcessKey,
    /// Исполняемый файл хоста.
    pub executable: String,
    pub workload: ProcessKey,
    pub workload_pgid: i32,
    pub workload_sid: Option<i32>,
    /// Ведомая сторона PTY нагрузки.
    pub tty: String,
    /// Диапазон протокола хоста.
    pub versions: Versions,
    /// Поколение сборки хоста.
    pub generation: u32,
    pub socket: String,
}

pub fn write_record(dir: &Path, record: &SessionRecord) -> io::Result<PathBuf> {
    let path = record_path(dir, &record.session);
    let temp = dir.join(format!(".{}.json.{}", record.session, std::process::id()));
    let mut bytes = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let written = (|| {
        let mut file = File::create(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, &path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written.map(|()| path)
}

pub fn read_record(dir: &Path, session: &str) -> io::Result<Option<SessionRecord>> {
    match fs::read(record_path(dir, session)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Строка итога запуска хоста в stdout запускающего.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "launch", content = "detail", rename_all = "kebab-case")]
pub enum Launch {
    Ready(SessionRecord),
    Failed(String),
}

/// Хранимый хвост вывода с абсолютными номерами байтов.
#[derive(Debug)]
pub struct OutputLog {
    start: u64,
    bytes: VecDeque<u8>,
    limit: usize,
    lines: u64,
}

impl OutputLog {
    pub fn new(limit: usize) -> OutputLog {
        OutputLog {
            start: 0,
            bytes: VecDeque::new(),
            limit: limit.max(1),
            lines: 0,
        }
    }

    pub fn append(&mut self, chunk: &[u8]) {
        self.lines += chunk.iter().filter(|byte| **byte == b'\n').count() as u64;
        self.bytes.extend(chunk);
        let excess = self.bytes.len().saturating_sub(self.limit);
        if excess > 0 {
            self.bytes.drain(..excess);
            self.start += excess as u64;
        }
    }

    /// Номер первого хранимого байта.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Номер байта после последнего прочитанного: всего прочитано из PTY.
    pub fn end(&self) -> u64 {
        self.start + self.bytes.len() as u64
    }

    /// Переводов строки во всём прочитанном выводе, включая вытесненный.
    pub fn lines(&self) -> u64 {
        self.lines
    }

    /// С какого байта получит вывод клиент, запросивший `from`: вытесненное начало сдвигает
    /// вперёд, будущее — назад к концу.
    pub fn first(&self, from: u64) -> u64 {
        from.clamp(self.start, self.end())
    }

    /// Не больше `max` байтов с [`OutputLog::first`]`(from)`.
    pub fn read(&self, from: u64, max: usize) -> (u64, Vec<u8>) {
        let first = self.first(from);
        let begin = (first - self.start) as usize;
        let len = max.min(self.bytes.len() - begin);
        (
            first,
            self.bytes.range(begin..begin + len).copied().collect(),
        )
    }
}

pub type ConnId = u64;

/// Лидерство, клиенты и размер сессии. Меняется только принятым `attach` и его уходом.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionState {
    pub clients: u32,
    pub leader: Option<ConnId>,
    /// Номер назначения лидера.
    pub lead: u32,
    pub size: Winsize,
}

/// Решение по `Hello`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Отказ с причиной; сессия не изменилась.
    Refused(String),
    /// Принято; `resize` — новый размер PTY, если `attach` его меняет.
    Welcome {
        version: u16,
        resize: Option<Winsize>,
    },
}

impl SessionState {
    pub fn new(size: Winsize) -> SessionState {
        SessionState {
            clients: 0,
            leader: None,
            lead: 0,
            size,
        }
    }

    /// Рукопожатие соединения `conn`. Версия согласуется до любого изменения: при пустом
    /// пересечении состояние не меняется. `probe` и `control` его не меняют и при согласии;
    /// принятый `attach` становится лидером и задаёт ненулевой размер.
    pub fn admit(&mut self, conn: ConnId, hello: &Hello, host: Versions) -> Admission {
        let version = match proto::negotiate(host, hello.versions) {
            Ok(version) => version,
            Err(reason) => return Admission::Refused(reason),
        };
        let resize = match hello.purpose {
            Purpose::Probe | Purpose::Control => None,
            Purpose::Attach => {
                self.clients += 1;
                self.leader = Some(conn);
                self.lead += 1;
                let wanted = hello.size;
                (wanted.rows > 0 && wanted.cols > 0 && wanted != self.size).then(|| {
                    self.size = wanted;
                    wanted
                })
            }
        };
        Admission::Welcome { version, resize }
    }

    /// Уход принятого `attach`.
    pub fn leave(&mut self, conn: ConnId) {
        self.clients = self.clients.saturating_sub(1);
        if self.leader == Some(conn) {
            self.leader = None;
        }
    }
}

/// Вход `probe-update-host`.
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
                eprintln!("{HOST_NAME}: fork: {}", io::Error::last_os_error());
                return 1;
            }
            0 => {}
            _ => libc::_exit(0),
        }
        if libc::setsid() < 0 {
            report(&Launch::Failed(format!(
                "setsid: {}",
                io::Error::last_os_error()
            )));
            libc::_exit(1);
        }
        match libc::fork() {
            -1 => {
                report(&Launch::Failed(format!(
                    "второй fork: {}",
                    io::Error::last_os_error()
                )));
                libc::_exit(1);
            }
            0 => {}
            _ => libc::_exit(0),
        }
    }
    serve(&options)
}

fn report(launch: &Launch) {
    let line = serde_json::to_string(launch).unwrap_or_else(|_| "{}".to_owned());
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// stdin и stdout на `/dev/null`: запускающий не ждёт хоста после строки запуска.
fn detach_stdio() {
    if let Ok(null) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        // SAFETY: дескрипторы открыты; `dup2` заменяет стандартные потоки.
        unsafe {
            libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO);
            libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO);
        }
    }
}

/// Опубликованный сокет: хост убирает только свой узел.
struct Published {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Published {
    fn bind(path: &Path) -> Result<(UnixListener, Published), String> {
        check_socket_path(path)?;
        let listener =
            UnixListener::bind(path).map_err(|err| format!("bind {}: {err}", path.display()))?;
        let meta =
            fs::symlink_metadata(path).map_err(|err| format!("stat {}: {err}", path.display()))?;
        if !meta.file_type().is_socket() {
            return Err(format!("{}: после bind не сокет", path.display()));
        }
        Ok((
            listener,
            Published {
                path: path.to_path_buf(),
                dev: meta.dev(),
                ino: meta.ino(),
            },
        ))
    }

    fn unpublish(&self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.dev() == self.dev
            && meta.ino() == self.ino
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn serve(options: &Options) -> i32 {
    let socket = socket_path(&options.dir, &options.session);
    let (listener, published) = match Published::bind(&socket) {
        Ok(bound) => bound,
        Err(err) => {
            report(&Launch::Failed(err));
            return 1;
        }
    };
    let env: Vec<(String, String)> = std::env::vars().collect();
    let spawned = match pty::spawn(&Spawn {
        program: &options.program,
        args: &options.args,
        env: &env,
        size: options.size,
    }) {
        Ok(spawned) => spawned,
        Err(err) => {
            published.unpublish();
            report(&Launch::Failed(format!(
                "запуск нагрузки {}: {err}",
                options.program.display()
            )));
            return 1;
        }
    };
    let record = match describe(options, &spawned, &socket) {
        Ok(record) => record,
        Err(err) => {
            published.unpublish();
            report(&Launch::Failed(err));
            return 1;
        }
    };
    if let Err(err) = write_record(&options.dir, &record) {
        published.unpublish();
        report(&Launch::Failed(format!("запись сессии: {err}")));
        return 1;
    }
    report(&Launch::Ready(record.clone()));
    detach_stdio();
    let code = match Hub::start(options, record, spawned.master, spawned.child, listener) {
        Ok(hub) => hub.run(),
        Err(err) => {
            eprintln!("{HOST_NAME}: {err}");
            1
        }
    };
    published.unpublish();
    let _ = fs::remove_file(record_path(&options.dir, &options.session));
    code
}

fn describe(
    options: &Options,
    spawned: &pty::PtyChild,
    socket: &Path,
) -> Result<SessionRecord, String> {
    let host = observer::current()?;
    let info = observer::inspect(spawned.pid())?
        .ok_or_else(|| format!("нагрузка {} вышла до записи сессии", spawned.pid()))?;
    let executable = std::env::current_exe()
        .map(|path| path.display().to_string())
        .map_err(|err| format!("путь хоста: {err}"))?;
    Ok(SessionRecord {
        session: options.session.clone(),
        host: host.key,
        executable,
        workload: info.key,
        workload_pgid: info.pgid,
        workload_sid: info.sid,
        tty: spawned.tty.clone(),
        versions: HOST_VERSIONS,
        generation: crate::GENERATION,
        socket: socket.display().to_string(),
    })
}

/// Общее состояние хоста под одной блокировкой.
struct Shared {
    log: OutputLog,
    session: SessionState,
    /// PTY дочитан до конца: причина.
    pty_end: Option<String>,
    /// Нагрузка вышла: код или сигнал.
    exited: Option<String>,
    stopping: bool,
}

struct Hub {
    record: SessionRecord,
    state: Mutex<Shared>,
    changed: Condvar,
    /// Запись ввода в PTY.
    input: Mutex<File>,
    master: OwnedFd,
    next_conn: AtomicU64,
}

impl Hub {
    fn start(
        options: &Options,
        record: SessionRecord,
        master: OwnedFd,
        child: Child,
        listener: UnixListener,
    ) -> Result<Arc<Hub>, String> {
        let input = File::from(
            master
                .try_clone()
                .map_err(|err| format!("дескриптор PTY: {err}"))?,
        );
        let reader = File::from(
            master
                .try_clone()
                .map_err(|err| format!("дескриптор PTY: {err}"))?,
        );
        let hub = Arc::new(Hub {
            record,
            state: Mutex::new(Shared {
                log: OutputLog::new(LOG_LIMIT),
                session: SessionState::new(options.size),
                pty_end: None,
                exited: None,
                stopping: false,
            }),
            changed: Condvar::new(),
            input: Mutex::new(input),
            master,
            next_conn: AtomicU64::new(1),
        });
        let spawn = |name: &str, body: Box<dyn FnOnce() + Send>| {
            thread::Builder::new()
                .name(name.to_owned())
                .spawn(body)
                .map(drop)
                .map_err(|err| format!("поток {name}: {err}"))
        };
        let pty_hub = Arc::clone(&hub);
        spawn("pty-read", Box::new(move || pty_hub.read_pty(reader)))?;
        let wait_hub = Arc::clone(&hub);
        spawn(
            "workload-wait",
            Box::new(move || wait_hub.wait_workload(child)),
        )?;
        let accept_hub = Arc::clone(&hub);
        spawn("accept", Box::new(move || accept_hub.accept(listener)))?;
        Ok(hub)
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn update(&self, change: impl FnOnce(&mut Shared)) {
        change(&mut self.lock());
        self.changed.notify_all();
    }

    /// Ждёт выхода нагрузки и дочитывания PTY; код выхода хоста.
    fn run(&self) -> i32 {
        let mut state = self.lock();
        while state.exited.is_none() {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, EXIT_DRAIN, |state| state.pty_end.is_none())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.stopping = true;
        drop(state);
        self.changed.notify_all();
        0
    }

    fn read_pty(&self, mut file: File) {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match file.read(&mut buf) {
                Ok(0) => return self.update(|state| state.pty_end = Some("EOF".to_owned())),
                Ok(read) => self.update(|state| state.log.append(&buf[..read])),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return self.update(|state| state.pty_end = Some(err.to_string())),
            }
        }
    }

    fn wait_workload(&self, mut child: Child) {
        let exited = match child.wait() {
            Ok(status) => status.to_string(),
            Err(err) => format!("wait: {err}"),
        };
        self.update(|state| state.exited = Some(exited));
    }

    fn accept(self: Arc<Self>, listener: UnixListener) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let hub = Arc::clone(&self);
            let conn = self.next_conn.fetch_add(1, Ordering::Relaxed);
            let spawned = thread::Builder::new()
                .name(format!("conn-{conn}"))
                .spawn(move || hub.connection(conn, stream));
            if let Err(err) = spawned {
                eprintln!("{HOST_NAME}: поток соединения: {err}");
            }
        }
    }

    fn status(&self) -> Status {
        let state = self.lock();
        Status {
            session: self.record.session.clone(),
            host: self.record.host,
            workload: self.record.workload,
            workload_pgid: self.record.workload_pgid,
            workload_sid: self.record.workload_sid,
            versions: self.record.versions,
            generation: self.record.generation,
            clients: state.session.clients,
            lead: state.session.lead,
            leader: state.session.leader.is_some(),
            size: state.session.size,
            start: state.log.start(),
            end: state.log.end(),
            lines: state.log.lines(),
            exited: state.exited.is_some(),
        }
    }

    /// Рукопожатие под блокировкой: решение, ресайз PTY и номера хвоста одним шагом.
    fn admit(&self, conn: ConnId, hello: &Hello) -> Result<Welcome, String> {
        let mut state = self.lock();
        let (version, resize) = match state.session.admit(conn, hello, self.record.versions) {
            Admission::Refused(reason) => return Err(reason),
            Admission::Welcome { version, resize } => (version, resize),
        };
        if let Some(size) = resize
            && let Err(err) = pty::set_winsize(self.master.as_fd(), size)
        {
            eprintln!("{HOST_NAME}: ресайз PTY {}x{}: {err}", size.rows, size.cols);
        }
        let start = match hello.purpose {
            Purpose::Attach => state.log.first(hello.from),
            Purpose::Control | Purpose::Probe => state.log.end(),
        };
        Ok(Welcome {
            version,
            host: format!("{HOST_NAME} {}", self.record.generation),
            generation: self.record.generation,
            session: self.record.session.clone(),
            lead: state.session.lead,
            start,
            end: state.log.end(),
        })
    }

    fn leave(&self, conn: ConnId) {
        self.update(|state| state.session.leave(conn));
    }

    fn input(&self, bytes: &[u8]) -> Ack {
        {
            let state = self.lock();
            if state.exited.is_some() {
                return Ack::Rejected("нагрузка вышла".to_owned());
            }
            if state.stopping {
                return Ack::Rejected("сессия останавливается".to_owned());
            }
        }
        let mut input = self
            .input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match input.write_all(bytes) {
            Ok(()) => Ack::Accepted,
            Err(err) => Ack::Rejected(format!("запись в PTY: {err}")),
        }
    }

    fn await_lines(&self, lines: u64, timeout: Duration) {
        let state = self.lock();
        let _ = self
            .changed
            .wait_timeout_while(state, timeout.min(MAX_AWAIT), |state| {
                state.log.lines() < lines && state.exited.is_none()
            });
    }

    /// `SIGHUP` группе нагрузки, через [`STOP_GRACE`] — `SIGKILL`, если она жива.
    fn stop(self: &Arc<Self>) {
        let pgid = self.record.workload_pgid;
        {
            let mut state = self.lock();
            state.stopping = true;
            if state.exited.is_none() {
                // SAFETY: сигнал группе нагрузки; она не прибрана, пока `exited` пуст.
                unsafe { libc::kill(-pgid, libc::SIGHUP) };
            }
        }
        self.changed.notify_all();
        let hub = Arc::clone(self);
        let _ = thread::Builder::new()
            .name("stop-grace".to_owned())
            .spawn(move || {
                let state = hub.lock();
                let (state, _) = hub
                    .changed
                    .wait_timeout_while(state, STOP_GRACE, |state| state.exited.is_none())
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if state.exited.is_none() {
                    // SAFETY: сигнал группе нагрузки, которая не прибрана.
                    unsafe { libc::kill(-pgid, libc::SIGKILL) };
                }
            });
    }

    fn connection(self: Arc<Self>, conn: ConnId, mut stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(HELLO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
        let hello = match proto::read_message(&mut stream) {
            Ok(Some(Message::Hello(hello))) => hello,
            Ok(Some(_)) => {
                let _ = proto::write_message(
                    &mut stream,
                    &Message::Refused("первым сообщением ожидается Hello".to_owned()),
                );
                return;
            }
            _ => return,
        };
        let welcome = match self.admit(conn, &hello) {
            Ok(welcome) => welcome,
            Err(reason) => {
                let _ = proto::write_message(&mut stream, &Message::Refused(reason));
                return;
            }
        };
        let start = welcome.start;
        if proto::write_message(&mut stream, &Message::Welcome(welcome)).is_err() {
            if hello.purpose == Purpose::Attach {
                self.leave(conn);
            }
            return;
        }
        let _ = stream.set_read_timeout(None);
        match hello.purpose {
            Purpose::Probe => {}
            Purpose::Control => self.control(stream),
            Purpose::Attach => self.attach(conn, stream, start),
        }
    }

    fn control(self: Arc<Self>, mut stream: UnixStream) {
        loop {
            let reply = match proto::read_message(&mut stream) {
                Ok(Some(Message::StatusRequest)) => Message::Status(self.status()),
                Ok(Some(Message::Input { seq, bytes })) => Message::InputAck {
                    seq,
                    ack: self.input(&bytes),
                },
                Ok(Some(Message::AwaitLines { lines, timeout_ms })) => {
                    self.await_lines(lines, Duration::from_millis(u64::from(timeout_ms)));
                    Message::Status(self.status())
                }
                Ok(Some(Message::Stop)) => {
                    self.stop();
                    Message::Done
                }
                Ok(Some(Message::Detach)) => {
                    let _ = proto::write_message(&mut stream, &Message::Done);
                    return;
                }
                Ok(Some(other)) => {
                    let _ = proto::write_message(
                        &mut stream,
                        &Message::Refused(format!("{other:?} не для управляющего соединения")),
                    );
                    return;
                }
                Ok(None) | Err(_) => return,
            };
            if proto::write_message(&mut stream, &reply).is_err() {
                return;
            }
        }
    }

    /// Хвост с `start`, затем живой вывод; ввод и `Detach` читает отдельный поток.
    fn attach(self: Arc<Self>, conn: ConnId, stream: UnixStream, start: u64) {
        let closed = Arc::new(AtomicBool::new(false));
        let close = {
            let hub = Arc::clone(&self);
            let closed = Arc::clone(&closed);
            move || {
                if !closed.swap(true, Ordering::SeqCst) {
                    hub.leave(conn);
                }
            }
        };
        let (Ok(mut reader), Ok(writer)) = (stream.try_clone(), stream.try_clone()) else {
            close();
            return;
        };
        let writer = Arc::new(Mutex::new(writer));
        let send = {
            let writer = Arc::clone(&writer);
            move |message: &Message| {
                let mut writer = writer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                proto::write_message(&mut *writer, message)
            }
        };
        let reader_hub = Arc::clone(&self);
        let reader_close = close.clone();
        let reader_send = send.clone();
        let spawned = thread::Builder::new()
            .name(format!("conn-{conn}-input"))
            .spawn(move || {
                loop {
                    match proto::read_message(&mut reader) {
                        Ok(Some(Message::Input { seq, bytes })) => {
                            let ack = reader_hub.input(&bytes);
                            if reader_send(&Message::InputAck { seq, ack }).is_err() {
                                break;
                            }
                        }
                        Ok(Some(Message::Detach)) => {
                            reader_close();
                            let _ = reader_send(&Message::Done);
                            return;
                        }
                        _ => break,
                    }
                }
                reader_close();
            });
        if spawned.is_err() {
            close();
            return;
        }
        let mut next = start;
        loop {
            let (offset, bytes) = {
                let mut state = self.lock();
                loop {
                    if closed.load(Ordering::SeqCst) || state.stopping {
                        break;
                    }
                    if state.log.end() > next {
                        break;
                    }
                    state = self
                        .changed
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                if closed.load(Ordering::SeqCst) || state.stopping {
                    break;
                }
                state.log.read(next, OUTPUT_CHUNK)
            };
            next = offset + bytes.len() as u64;
            if send(&Message::Output { offset, bytes }).is_err() {
                break;
            }
        }
        close();
    }
}

/// Запускает хост через `posix_spawn` с `POSIX_SPAWN_CLOEXEC_DEFAULT` ([`spawn`]) и ждёт строку
/// [`Launch`] не дольше [`LAUNCH_TIMEOUT`]. stderr хоста идёт в `stderr`, stdin — `/dev/null`,
/// окружение — вызывающего. Владелец запускает хосты, держа `shellio.lock` и свой сокет: хост их
/// не наследует. `disclaim` — вариант клетки E: хост запускается с
/// `responsibility_spawnattrs_setdisclaim` и отвечает перед TCC сам за себя.
pub fn launch(
    binary: &Path,
    options: &Options,
    stderr: File,
    disclaim: bool,
) -> Result<SessionRecord, String> {
    let (read, write) = spawn::pipe().map_err(|err| format!("канал строки запуска: {err}"))?;
    let pid = spawn::spawn(spawn::Spec {
        program: binary,
        args: &options.to_args(),
        env: None,
        stdio: [None, Some(write), Some(OwnedFd::from(stderr))],
        setsid: false,
        disclaim,
    })
    .map_err(|err| format!("запуск {}: {err}", binary.display()))?;
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("host-launch".to_owned())
        .spawn(move || {
            let mut line = String::new();
            let read = BufReader::new(File::from(read))
                .read_line(&mut line)
                .map(|_| line);
            let _ = sender.send(read);
        })
        .map_err(|err| format!("поток строки запуска: {err}"))?;
    let line = receiver.recv_timeout(LAUNCH_TIMEOUT);
    let _ = spawn::wait(pid);
    let line = match line {
        Ok(Ok(line)) => line,
        Ok(Err(err)) => return Err(format!("строка запуска хоста: {err}")),
        Err(_) => {
            return Err(format!(
                "хост не сообщил итог запуска за {} с",
                LAUNCH_TIMEOUT.as_secs()
            ));
        }
    };
    match serde_json::from_str::<Launch>(line.trim()) {
        Ok(Launch::Ready(record)) => Ok(record),
        Ok(Launch::Failed(reason)) => Err(format!("хост не запустился: {reason}")),
        Err(err) => Err(format!("строка запуска хоста «{}»: {err}", line.trim())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(purpose: Purpose, versions: Versions, size: Winsize) -> Hello {
        Hello {
            purpose,
            versions,
            client: "test".to_owned(),
            from: 0,
            size,
        }
    }

    #[test]
    fn refused_handshake_changes_nothing() {
        let host = Versions::new(1, 1);
        let mut state = SessionState::new(Winsize::cells(24, 80));
        assert!(matches!(
            state.admit(
                1,
                &hello(Purpose::Attach, host, Winsize::cells(30, 100)),
                host
            ),
            Admission::Welcome {
                version: 1,
                resize: Some(_)
            }
        ));
        let before = state.clone();
        for purpose in Purpose::ALL {
            for versions in [
                Versions::new(2, 2),
                Versions::new(2, 1),
                Versions::new(0, 0),
            ] {
                let admission =
                    state.admit(7, &hello(purpose, versions, Winsize::cells(50, 200)), host);
                assert!(
                    matches!(admission, Admission::Refused(_)),
                    "{purpose:?} {versions}"
                );
                assert_eq!(state, before, "отказ {purpose:?} {versions} изменил сессию");
            }
        }
    }

    #[test]
    fn probe_and_control_take_no_lead_and_no_resize() {
        let host = Versions::new(1, 2);
        let mut state = SessionState::new(Winsize::cells(24, 80));
        state.admit(
            1,
            &hello(Purpose::Attach, host, Winsize::cells(24, 80)),
            host,
        );
        let before = state.clone();
        for purpose in [Purpose::Probe, Purpose::Control] {
            let admission = state.admit(
                9,
                &hello(purpose, Versions::new(1, 2), Winsize::cells(50, 200)),
                host,
            );
            assert_eq!(
                admission,
                Admission::Welcome {
                    version: 2,
                    resize: None
                }
            );
            assert_eq!(state, before, "{purpose:?} изменил сессию");
        }
    }

    #[test]
    fn attach_takes_the_lead_and_leaving_releases_it() {
        let host = Versions::new(1, 1);
        let mut state = SessionState::new(Winsize::cells(24, 80));
        let same = state.admit(
            1,
            &hello(Purpose::Attach, host, Winsize::cells(24, 80)),
            host,
        );
        assert_eq!(
            same,
            Admission::Welcome {
                version: 1,
                resize: None
            }
        );
        let zero = state.admit(2, &hello(Purpose::Attach, host, Winsize::cells(0, 0)), host);
        assert_eq!(
            zero,
            Admission::Welcome {
                version: 1,
                resize: None
            }
        );
        assert_eq!((state.clients, state.leader, state.lead), (2, Some(2), 2));
        state.leave(1);
        assert_eq!((state.clients, state.leader), (1, Some(2)));
        state.leave(2);
        assert_eq!((state.clients, state.leader, state.lead), (0, None, 2));
        assert_eq!(state.size, Winsize::cells(24, 80));
    }

    #[test]
    fn output_log_keeps_offsets_across_eviction() {
        let mut log = OutputLog::new(8);
        log.append(b"ab\ncd\n");
        assert_eq!((log.start(), log.end(), log.lines()), (0, 6, 2));
        assert_eq!(log.read(0, 100), (0, b"ab\ncd\n".to_vec()));
        assert_eq!(log.read(3, 2), (3, b"cd".to_vec()));
        log.append(b"ef\ngh\n");
        assert_eq!((log.start(), log.end(), log.lines()), (4, 12, 4));
        // Вытесненное начало сдвигает первый байт вперёд, будущее — к концу.
        assert_eq!(log.first(0), 4);
        assert_eq!(log.read(0, 100), (4, b"d\nef\ngh\n".to_vec()));
        assert_eq!(log.first(99), 12);
        assert_eq!(log.read(99, 10), (12, Vec::new()));
        assert_eq!(log.read(10, 10), (10, b"h\n".to_vec()));
    }

    #[test]
    fn options_round_trip() {
        let options = Options {
            dir: PathBuf::from("/tmp/psu.x/s"),
            session: "h1".to_owned(),
            size: Winsize::cells(24, 80),
            tag: "0123456789abcdef".to_owned(),
            program: PathBuf::from("/bin/fixture"),
            args: vec!["--tag".to_owned(), "0123456789abcdef".to_owned()],
        };
        assert_eq!(Options::parse(&options.to_args()), Ok(options));
        assert!(Options::parse(&["serve".to_owned()]).is_err());
        assert!(check_session_name("h1").is_ok());
        assert!(check_session_name("../x").is_err());
        assert!(check_session_name("").is_err());
        assert!(parse_size("0x80").is_err());
        assert_eq!(parse_size("30x100"), Ok(Winsize::cells(30, 100)));
    }

    #[test]
    fn socket_path_length_is_checked() {
        let fits = PathBuf::from(format!("/{}", "a".repeat(MAX_SOCKET_PATH - 1)));
        assert!(check_socket_path(&fits).is_ok());
        let long = PathBuf::from(format!("/{}", "a".repeat(MAX_SOCKET_PATH)));
        assert!(check_socket_path(&long).unwrap_err().contains("103"));
    }

    #[test]
    fn record_is_written_atomically_and_read_back() {
        let dir = std::env::temp_dir().join(format!(
            "psu-host-record-{}-{}",
            std::process::id(),
            crate::sys::uptime_ns()
        ));
        fs::create_dir(&dir).unwrap();
        assert_eq!(read_record(&dir, "h1").unwrap(), None);
        let record = SessionRecord {
            session: "h1".to_owned(),
            host: ProcessKey {
                pid: 1,
                start_us: 2,
            },
            executable: "/x/probe-update-host".to_owned(),
            workload: ProcessKey {
                pid: 3,
                start_us: 4,
            },
            workload_pgid: 3,
            workload_sid: Some(3),
            tty: "/dev/ttys001".to_owned(),
            versions: Versions::new(1, 2),
            generation: 2,
            socket: "/x/h1.sock".to_owned(),
        };
        write_record(&dir, &record).unwrap();
        assert_eq!(read_record(&dir, "h1").unwrap(), Some(record));
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["h1.json"], "временный файл не остался");
        fs::write(record_path(&dir, "bad"), b"{").unwrap();
        assert_eq!(
            read_record(&dir, "bad").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
