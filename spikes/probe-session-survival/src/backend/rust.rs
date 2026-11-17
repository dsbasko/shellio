//! Адаптер Rust-хоста пробы (план P0.4, «Rust-хост»). `ensure` — create-or-attach без скрытого
//! attach: готовую сессию адаптер спрашивает управляющим соединением, иначе запускает
//! `probe-session-host`, который сам уходит из дерева запустившего и отчитывается строкой
//! [`Launch`]. Проигравший гонку `ensure` получает «занято» и спрашивает победителя. Управляющее
//! соединение (`list`, `stop`, ввод и ресайз без клиента) клиентом сессии не считается. `list`
//! сокеты не удаляет.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use super::{
    Backend, BackendError, ClientCommand, CommandRun, Ensured, HostRecord, InputOutcome, InputPath,
    Lead, Listed, Liveness, Result, SessionId, Snapshot, SocketEntries, StopOutcome, Workload,
    alive, judge_liveness, kill_recorded_host, session_processes,
};
use crate::host::{self, Launch, attach, registry, socket};
use crate::launch;
use crate::matrix::Candidate;
use crate::observer::ProcessKey;
use crate::proto::{self, Hello, Message, Purpose, Status, Welcome};
use crate::pty::Winsize;

const ROLE_TITLE: &str = "pss-role;";
const TERM_VALUE: &str = "xterm-256color";
pub const FLAVOR: &str = "rust-host";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// `probe-session-host`.
    pub host: PathBuf,
    /// `probe-session-attach`.
    pub attach: PathBuf,
    /// Короткий каталог сокетов и записей реестра.
    pub dir: PathBuf,
    /// Журналы хостов: stderr каждого хоста.
    pub logs: PathBuf,
    pub cwd: PathBuf,
    /// Окружение хоста и нагрузки без `TERM`.
    pub env: Vec<(String, String)>,
    /// Токен заголовка роли и метка запуска в `argv` хоста.
    pub token: String,
    pub timeout: Duration,
    /// Сокет точек остановки окна публикации (E5); `None` — хост не останавливается.
    pub break_socket: Option<PathBuf>,
}

pub struct RustHost {
    config: Config,
    records: Mutex<BTreeMap<SessionId, HostRecord>>,
}

impl RustHost {
    pub fn new(config: Config) -> std::result::Result<RustHost, String> {
        for binary in [&config.host, &config.attach] {
            if !binary.is_file() {
                return Err(format!("предусловие: нет {}", binary.display()));
            }
        }
        fs::create_dir_all(&config.logs)
            .map_err(|err| format!("{}: {err}", config.logs.display()))?;
        Ok(RustHost {
            config,
            records: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Тот же хост с другим каталогом сокетов или точками остановки.
    pub fn with_config(&self, config: Config) -> std::result::Result<RustHost, String> {
        RustHost::new(config)
    }

    pub fn session_log(&self, id: &SessionId) -> PathBuf {
        self.config.logs.join(format!("{id}.log"))
    }

    fn records(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, HostRecord>> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn env(&self) -> Vec<(String, String)> {
        let mut env = self.config.env.clone();
        env.push(("TERM".to_owned(), TERM_VALUE.to_owned()));
        env
    }

    /// Соединение назначения `purpose` после `Welcome`.
    fn open(&self, id: &SessionId, purpose: Purpose) -> Result<(UnixStream, Welcome)> {
        let path = Backend::socket_path(self, id);
        let failed = |what: &str, err: io::Error| BackendError::Failed(format!("{what}: {err}"));
        let mut stream = UnixStream::connect(&path)
            .map_err(|err| failed(&format!("подключение к {}", path.display()), err))?;
        stream
            .set_read_timeout(Some(self.config.timeout))
            .map_err(|err| failed("срок чтения", err))?;
        proto::write_message(
            &mut stream,
            &Message::Hello(Hello {
                version: proto::VERSION,
                purpose,
                lead: Lead::IfFree,
                client: format!("probe-session {purpose:?}"),
                size: Winsize::cells(0, 0),
            }),
        )
        .map_err(|err| failed("hello", err))?;
        match proto::read_message(&mut stream).map_err(|err| failed("welcome", err))? {
            Some(Message::Welcome(welcome)) => Ok((stream, welcome)),
            Some(Message::Refused(reason)) => {
                Err(BackendError::Failed(format!("хост отказал: {reason}")))
            }
            other => Err(BackendError::Failed(format!("вместо welcome: {other:?}"))),
        }
    }

    /// Соединение роли GUI от имени лидера поколения `generation`. `auto` — данные, которыми GUI
    /// сразу отвечает на чтение буфера обмена; `None` — запросы ждут [`Gui::answer`].
    pub fn gui(&self, id: &SessionId, generation: u32, auto: Option<Vec<u8>>) -> Result<Gui> {
        let (stream, _) = self.open(id, Purpose::Gui)?;
        Gui::start(stream, generation, auto, self.config.timeout)
    }

    /// Наблюдатель разбора хоста: события `Parsed` после каждого куска PTY.
    pub fn observe(&self, id: &SessionId) -> Result<Observer> {
        let (stream, welcome) = self.open(id, Purpose::Observe)?;
        Observer::start(stream, welcome.offset)
    }

    /// Управляющее соединение: hello, одно сообщение и ответ.
    pub fn control(&self, id: &SessionId, message: &Message) -> Result<Message> {
        let failed = |what: &str, err: io::Error| BackendError::Failed(format!("{what}: {err}"));
        let (mut stream, _) = self.open(id, Purpose::Control)?;
        proto::write_message(&mut stream, message).map_err(|err| failed("запрос", err))?;
        proto::read_message(&mut stream)
            .map_err(|err| failed("ответ", err))?
            .ok_or_else(|| BackendError::Failed("хост закрыл соединение без ответа".to_owned()))
    }

    pub fn status(&self, id: &SessionId) -> Result<Status> {
        match self.control(id, &Message::StatusRequest)? {
            Message::Status(status) => Ok(status),
            other => Err(BackendError::Failed(format!("вместо состояния: {other:?}"))),
        }
    }

    fn record_from(status: &Status, id: &SessionId) -> HostRecord {
        HostRecord {
            session: id.clone(),
            host: status.host,
            workload: status.workload,
            workload_pgid: status.workload_pgid,
            workload_sid: status.workload_sid,
        }
    }

    /// Запуск хоста; строка отчёта и след команды.
    fn launch(
        &self,
        id: &SessionId,
        workload: &Workload,
        size: Winsize,
    ) -> Result<(Launch, CommandRun)> {
        let options = host::Options {
            dir: self.config.dir.clone(),
            session: id.clone(),
            size,
            tag: self.config.token.clone(),
            program: workload.program.clone(),
            args: workload.args.clone(),
        };
        let args = options.to_args();
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.session_log(id))
            .map_err(|err| BackendError::Failed(format!("журнал хоста {id}: {err}")))?;
        let mut command = Command::new(&self.config.host);
        command
            .args(&args)
            .env_clear()
            .envs(self.env())
            .current_dir(&self.config.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log);
        if let Some(path) = &self.config.break_socket {
            command.env(host::BREAK_ENV, path);
        }
        let started = Instant::now();
        let mut child = {
            let _guard = crate::pty::spawn_lock();
            command
                .spawn()
                .map_err(|err| BackendError::Failed(format!("запуск хоста {id}: {err}")))?
        };
        let stdout = child.stdout.take();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut text = String::new();
            let read = stdout.map(|mut pipe| pipe.read_to_string(&mut text));
            let _ = tx.send((read.map(|r| r.map(|_| ())), text));
        });
        let status = super::wait_child(&mut child, self.config.timeout)?;
        let (read, text) = rx.recv_timeout(self.config.timeout).map_err(|_| {
            BackendError::Failed(format!(
                "хост {id} не отчитался за {} мс",
                self.config.timeout.as_millis()
            ))
        })?;
        if let Some(Err(err)) = read {
            return Err(BackendError::Failed(format!("stdout хоста {id}: {err}")));
        }
        use std::os::unix::process::ExitStatusExt;
        let run = CommandRun {
            args: std::iter::once(self.config.host.display().to_string())
                .chain(args)
                .collect(),
            code: status.code(),
            signal: status.signal(),
            stdout: text.clone(),
            stderr: String::new(),
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
        let line = text.lines().next().unwrap_or_default();
        let launched = serde_json::from_str::<Launch>(line).map_err(|err| {
            BackendError::Failed(format!("строка отчёта хоста {id} «{line}»: {err}"))
        })?;
        Ok((launched, run))
    }
}

impl Backend for RustHost {
    fn candidate(&self) -> Candidate {
        Candidate::Rust
    }

    fn flavor(&self) -> &str {
        FLAVOR
    }

    fn ensure(&self, id: &SessionId, workload: &Workload, size: Winsize) -> Result<Ensured> {
        if let Ok(status) = self.status(id) {
            let record = RustHost::record_from(&status, id);
            self.records().insert(id.clone(), record.clone());
            return Ok(Ensured {
                record,
                created: false,
                command: CommandRun {
                    args: vec!["status".to_owned(), id.to_string()],
                    ..CommandRun::default()
                },
            });
        }
        let (launched, run) = self.launch(id, workload, size)?;
        let (record, created) = match launched {
            Launch::Ready(record) => (record, true),
            Launch::Busy(_) => {
                let status = self.status(id)?;
                (RustHost::record_from(&status, id), false)
            }
            Launch::Failed(reason) => {
                return Err(BackendError::Failed(format!("хост {id}: {reason}")));
            }
        };
        if record.session != *id {
            return Err(BackendError::Failed(format!(
                "хост отчитался о сессии {} вместо {id}",
                record.session
            )));
        }
        self.records().insert(id.clone(), record.clone());
        Ok(Ensured {
            record,
            created,
            command: run,
        })
    }

    fn attach_command(&self, id: &SessionId, lead: Lead) -> Result<ClientCommand> {
        Ok(ClientCommand {
            program: self.config.attach.clone(),
            args: attach::Options {
                socket: Backend::socket_path(self, id),
                lead,
                role_prefix: self.role_prefix(),
            }
            .to_args(),
            env: self.env(),
        })
    }

    fn snapshot(&self, id: &SessionId) -> Result<Snapshot> {
        match self.control(id, &Message::SnapshotRequest)? {
            Message::Snapshot(snapshot) => Ok(snapshot),
            other => Err(BackendError::Failed(format!("вместо снимка: {other:?}"))),
        }
    }

    fn input(&self, id: &SessionId, bytes: &[u8], path: InputPath) -> Result<InputOutcome> {
        if bytes.is_empty() {
            return Err(BackendError::Failed("пустой ввод".to_owned()));
        }
        match (self.control(id, &Message::Input(bytes.to_vec()))?, path) {
            (Message::Done, InputPath::Raw) => Ok(InputOutcome::Sent),
            (Message::Done, InputPath::Typed) => Ok(InputOutcome::Accepted),
            (Message::Refused(reason), _) => Ok(InputOutcome::Rejected(reason)),
            (other, _) => Err(BackendError::Failed(format!("ответ на ввод: {other:?}"))),
        }
    }

    fn resize(&self, id: &SessionId, size: Winsize) -> Result<()> {
        match self.control(id, &Message::Resize(size))? {
            Message::Done => Ok(()),
            other => Err(BackendError::Failed(format!("ответ на ресайз: {other:?}"))),
        }
    }

    fn stop(&self, id: &SessionId, grace: Duration) -> Result<StopOutcome> {
        let record = self
            .record(id)
            .ok_or_else(|| BackendError::Failed(format!("нет записи о сессии {id}")))?;
        let processes = session_processes(&record)?;
        let started = Instant::now();
        let reply = self.control(id, &Message::Stop);
        let command = CommandRun {
            args: vec!["stop".to_owned(), id.to_string()],
            code: Some(i32::from(!matches!(reply, Ok(Message::Done)))),
            signal: None,
            stdout: format!("{reply:?}"),
            stderr: String::new(),
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
        let alive_after_command = alive(&processes)?;
        let waited = Instant::now();
        let pids: Vec<i32> = alive_after_command.iter().map(|key| key.pid).collect();
        launch::wait_exit(&pids, grace);
        let survivors = alive(&alive_after_command)?;
        Ok(StopOutcome {
            command,
            processes,
            alive_after_command,
            survivors,
            waited_ms: waited.elapsed().as_millis() as u64,
        })
    }

    fn list(&self) -> Result<Vec<Listed>> {
        let entries = socket::entries(&self.config.dir)
            .map_err(|err| BackendError::Failed(format!("каталог сокетов: {err}")))?;
        let mut listed = Vec::new();
        for name in entries.sessions {
            let Ok(id) = SessionId::new(&name) else {
                continue;
            };
            listed.push(match self.status(&id) {
                Ok(status) => Listed {
                    name,
                    workload_pid: Some(status.workload.pid),
                    clients: Some(status.clients),
                    error: None,
                },
                Err(err) => Listed {
                    name,
                    workload_pid: None,
                    clients: None,
                    error: Some(err.to_string()),
                },
            });
        }
        Ok(listed)
    }

    fn record(&self, id: &SessionId) -> Option<HostRecord> {
        self.records().get(id).cloned()
    }

    fn liveness(&self, id: &SessionId) -> Result<Liveness> {
        let socket_present = fs::symlink_metadata(Backend::socket_path(self, id))
            .is_ok_and(|meta| meta.file_type().is_socket());
        let record = self.record(id);
        judge_liveness(record.as_ref(), socket_present, || match self.status(id) {
            Ok(status) => Liveness::Live {
                workload_pid: status.workload.pid,
                clients: status.clients,
            },
            Err(err) => Liveness::Unreachable {
                error: err.to_string(),
            },
        })
    }

    fn read_boundary(&self, id: &SessionId) -> Result<u64> {
        Ok(self.status(id)?.reads)
    }

    fn role_prefix(&self) -> Option<String> {
        Some(format!("{ROLE_TITLE}{}:", self.config.token))
    }

    fn socket_path(&self, id: &SessionId) -> PathBuf {
        self.config.dir.join(id.as_str())
    }

    fn socket_entries(&self) -> io::Result<SocketEntries> {
        socket::entries(&self.config.dir)
    }

    fn remaining(&self, id: &SessionId) -> Result<Vec<ProcessKey>> {
        let record = registry::read(&self.config.dir, id.as_str())
            .map_err(|err| BackendError::Failed(format!("запись реестра {id}: {err}")))?
            .ok_or_else(|| BackendError::Failed(format!("нет записи реестра {id}")))?;
        let processes = registry::all_processes().map_err(BackendError::Failed)?;
        Ok(registry::remaining(&record, &processes))
    }

    fn kill_host(&self, id: &SessionId, timeout: Duration) -> Result<ProcessKey> {
        let record = self
            .record(id)
            .ok_or_else(|| BackendError::Failed(format!("нет записи о сессии {id}")))?;
        kill_recorded_host(&record, timeout)
    }
}

/// Строк восстановления для attach в журнале хоста: `attach <id>: граница …`.
pub fn attach_events(log: &str) -> usize {
    log.lines()
        .filter(|line| line.contains(": attach ") && line.contains(": граница "))
        .count()
}

// --- роль GUI и наблюдение ---

/// Запрос хоста на чтение буфера обмена, полученный GUI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ClipboardAsk {
    pub serial: u64,
    pub generation: u32,
}

#[derive(Default)]
struct GuiInbox {
    asks: Vec<ClipboardAsk>,
    replies: Vec<Message>,
    closed: bool,
}

/// Соединение роли GUI: типизированный путь эффектов UI.
pub struct Gui {
    writer: Mutex<UnixStream>,
    inbox: Arc<(Mutex<GuiInbox>, Condvar)>,
    pub generation: u32,
    timeout: Duration,
}

impl Gui {
    fn start(
        stream: UnixStream,
        generation: u32,
        auto: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Gui> {
        let failed = |what: &str, err: io::Error| BackendError::Failed(format!("{what}: {err}"));
        stream
            .set_read_timeout(None)
            .map_err(|err| failed("срок чтения GUI", err))?;
        let mut reader = stream.try_clone().map_err(|err| failed("сокет GUI", err))?;
        let mut answerer = stream.try_clone().map_err(|err| failed("сокет GUI", err))?;
        let inbox = Arc::new((Mutex::new(GuiInbox::default()), Condvar::new()));
        let shared = Arc::clone(&inbox);
        thread::Builder::new()
            .name("gui".to_owned())
            .spawn(move || {
                loop {
                    let message = proto::read_message(&mut reader);
                    let (lock, wake) = &*shared;
                    let mut inbox = lock.lock().unwrap_or_else(|p| p.into_inner());
                    match message {
                        Ok(Some(Message::ClipboardRequest { serial, generation })) => {
                            inbox.asks.push(ClipboardAsk { serial, generation });
                            if let Some(data) = &auto {
                                let _ = proto::write_message(
                                    &mut answerer,
                                    &Message::ClipboardReply {
                                        serial,
                                        generation,
                                        data: Some(data.clone()),
                                    },
                                );
                            }
                        }
                        Ok(Some(other)) => inbox.replies.push(other),
                        Ok(None) | Err(_) => {
                            inbox.closed = true;
                            wake.notify_all();
                            return;
                        }
                    }
                    wake.notify_all();
                }
            })
            .map_err(|err| failed("поток GUI", err))?;
        let gui = Gui {
            writer: Mutex::new(stream),
            inbox,
            generation,
            timeout,
        };
        match gui.request(&Message::GuiBind { generation })? {
            Message::Done => Ok(gui),
            other => Err(BackendError::Failed(format!("привязка GUI: {other:?}"))),
        }
    }

    fn write(&self, message: &Message) -> Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        proto::write_message(&mut *writer, message)
            .map_err(|err| BackendError::Failed(format!("запись GUI: {err}")))
    }

    /// Сообщение и ответ хоста на него.
    fn request(&self, message: &Message) -> Result<Message> {
        let seen = self
            .inbox
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .replies
            .len();
        self.write(message)?;
        let (lock, wake) = &*self.inbox;
        let inbox = lock.lock().unwrap_or_else(|p| p.into_inner());
        let (inbox, _) = wake
            .wait_timeout_while(inbox, self.timeout, |inbox| {
                inbox.replies.len() <= seen && !inbox.closed
            })
            .unwrap_or_else(|p| p.into_inner());
        inbox
            .replies
            .get(seen)
            .cloned()
            .ok_or_else(|| BackendError::Failed(format!("хост не ответил GUI на {message:?}")))
    }

    /// Фокус окна; `Refused` хоста — `Ok(Some(причина))`.
    pub fn focus(&self, gained: bool) -> Result<Option<String>> {
        match self.request(&Message::Focus(gained))? {
            Message::Done => Ok(None),
            Message::Refused(reason) => Ok(Some(reason)),
            other => Err(BackendError::Failed(format!("ответ на фокус: {other:?}"))),
        }
    }

    pub fn visible(&self, visible: bool) -> Result<Option<String>> {
        match self.request(&Message::Visible(visible))? {
            Message::Done => Ok(None),
            Message::Refused(reason) => Ok(Some(reason)),
            other => Err(BackendError::Failed(format!(
                "ответ на видимость: {other:?}"
            ))),
        }
    }

    /// Запросы чтения буфера обмена, пришедшие к GUI.
    pub fn asks(&self) -> Vec<ClipboardAsk> {
        self.inbox
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .asks
            .clone()
    }

    /// Ждёт `count`-го запроса чтения буфера обмена.
    pub fn wait_ask(&self, count: usize, timeout: Duration) -> Result<ClipboardAsk> {
        let (lock, wake) = &*self.inbox;
        let inbox = lock.lock().unwrap_or_else(|p| p.into_inner());
        let (inbox, _) = wake
            .wait_timeout_while(inbox, timeout, |inbox| {
                inbox.asks.len() < count && !inbox.closed
            })
            .unwrap_or_else(|p| p.into_inner());
        inbox
            .asks
            .get(count - 1)
            .copied()
            .ok_or_else(|| BackendError::Failed(format!("GUI не получил запрос №{count}")))
    }

    /// Ответ на запрос чтения буфера обмена от имени поколения `generation`.
    pub fn answer(&self, ask: ClipboardAsk, generation: u32, data: Option<Vec<u8>>) -> Result<()> {
        self.write(&Message::ClipboardReply {
            serial: ask.serial,
            generation,
            data,
        })
    }
}

/// Событие разбора хоста.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Parsed {
    pub read: u64,
    pub offset: u64,
    pub held: u32,
    pub ground: bool,
}

#[derive(Default)]
struct ObserverLog {
    events: Vec<Parsed>,
    closed: bool,
}

/// Наблюдатель разбора: соединение `Observe` и журнал событий `Parsed`.
pub struct Observer {
    log: Arc<(Mutex<ObserverLog>, Condvar)>,
    /// Разобрано хостом к подключению наблюдателя.
    pub start: u64,
    _stream: UnixStream,
}

impl Observer {
    fn start(stream: UnixStream, start: u64) -> Result<Observer> {
        let failed = |what: &str, err: io::Error| BackendError::Failed(format!("{what}: {err}"));
        stream
            .set_read_timeout(None)
            .map_err(|err| failed("срок чтения наблюдателя", err))?;
        let mut reader = stream
            .try_clone()
            .map_err(|err| failed("сокет наблюдателя", err))?;
        let log = Arc::new((Mutex::new(ObserverLog::default()), Condvar::new()));
        let shared = Arc::clone(&log);
        thread::Builder::new()
            .name("observe".to_owned())
            .spawn(move || {
                loop {
                    let message = proto::read_message(&mut reader);
                    let (lock, wake) = &*shared;
                    let mut log = lock.lock().unwrap_or_else(|p| p.into_inner());
                    match message {
                        Ok(Some(Message::Parsed {
                            read,
                            offset,
                            held,
                            ground,
                        })) => log.events.push(Parsed {
                            read,
                            offset,
                            held,
                            ground,
                        }),
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => {
                            log.closed = true;
                            wake.notify_all();
                            return;
                        }
                    }
                    wake.notify_all();
                }
            })
            .map_err(|err| failed("поток наблюдателя", err))?;
        Ok(Observer {
            log,
            start,
            _stream: stream,
        })
    }

    pub fn events(&self) -> Vec<Parsed> {
        self.log
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .events
            .clone()
    }

    /// Первое событие, на котором разобрано не меньше `offset`; `None` — срок вышел.
    pub fn wait_offset(&self, offset: u64, timeout: Duration) -> Option<Parsed> {
        let (lock, wake) = &*self.log;
        let log = lock.lock().unwrap_or_else(|p| p.into_inner());
        let reached = |log: &ObserverLog| log.events.iter().find(|e| e.offset >= offset).copied();
        let (log, _) = wake
            .wait_timeout_while(log, timeout, |log| reached(log).is_none() && !log.closed)
            .unwrap_or_else(|p| p.into_inner());
        reached(&log)
    }
}
