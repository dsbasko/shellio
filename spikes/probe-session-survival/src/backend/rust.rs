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
use std::sync::{Mutex, mpsc};
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
use crate::proto::{self, Hello, Message, Purpose, Status};
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

    /// Управляющее соединение: hello, одно сообщение и ответ.
    pub fn control(&self, id: &SessionId, message: &Message) -> Result<Message> {
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
                purpose: Purpose::Control,
                lead: Lead::IfFree,
                client: "probe-session control".to_owned(),
                size: Winsize::cells(0, 0),
            }),
        )
        .map_err(|err| failed("hello", err))?;
        match proto::read_message(&mut stream).map_err(|err| failed("welcome", err))? {
            Some(Message::Welcome(_)) => {}
            Some(Message::Refused(reason)) => {
                return Err(BackendError::Failed(format!("хост отказал: {reason}")));
            }
            other => {
                return Err(BackendError::Failed(format!("вместо welcome: {other:?}")));
            }
        }
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

    fn snapshot(&self, _id: &SessionId) -> Result<Snapshot> {
        Err(BackendError::Failed(
            "снимок Rust-хоста появляется в задаче 12".to_owned(),
        ))
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
