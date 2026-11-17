//! Адаптер zmx для обеих комбинаций патчей (план P0.4, «Сборка zmx»). Каждая команда — процесс
//! `zmx` с окружением, собранным с нуля, и изолированным `ZMX_DIR`.
//!
//! `ensure` без скрытого attach. `zmx attach <имя> <нагрузка>` создаёт демона до первой записи в
//! stdout, а в клиента превращается только после неё: сначала «session created», у готовой сессии
//! очистка экрана перед `Init`. Здесь stdout команды — канал с закрытым концом чтения: первая же
//! запись получает `EPIPE`, и процесс выходит с кодом 1, не послав `Init`. Демон своё уже сделал:
//! сокет опубликован, нагрузка запущена. Размер нагрузки демон берёт из stdin, это ведомая сторона
//! PTY нужного размера; PTY закрывается только после того, как список показал нагрузку, то есть
//! после того, как демон прочитал размер. Успех узнаётся по списку и процессам, а не по коду.
//!
//! Управляемое лидерство (`ZMX_MANAGED`) есть только у текущей комбинации: клиент ведёт только по
//! захвату и сообщает роль заголовком `zmx-role;<токен>:<роль>:<поколение>`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{
    Backend, BackendError, ClientCommand, CommandRun, Ensured, HostRecord, InputOutcome, InputPath,
    Lead, Listed, Liveness, Result, SessionId, Snapshot, StopOutcome, Workload, judge_liveness,
    kill_recorded_host, signal, wait_child,
};
pub use super::{Role, RoleReport, SocketEntries, alive, parse_role, session_processes};
use crate::launch;
use crate::matrix::Candidate;
use crate::observer::{self, ProcessKey};
use crate::pty::{self, Winsize};

/// `sockaddr_un.sun_path` на macOS — 104 байта вместе с нулём.
pub const MAX_SOCKET_PATH: usize = 103;
/// Каталог, где патч `0003` держит сокет между `bind` и публикацией: `<ZMX_DIR>/.s/<pid>`.
pub const STAGING_DIR: &str = ".s";
/// Цифр в PID на macOS: `kern.maxproc` не больше 99 998.
const MAX_PID_DIGITS: usize = 5;
const ROLE_TITLE: &str = "zmx-role;";
const TERM_VALUE: &str = "xterm-256color";
/// Предел вывода команды zmx: больше даёт только `screen --all` на огромной истории.
const MAX_OUTPUT: usize = 64 << 20;

/// Комбинация патчей zmx пробы (`scripts/zmx-pin`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ZmxSet {
    /// `0001`–`0004`: управляемое лидерство, `screen`, `type`, экран после выхода клиента.
    Current,
    /// `0002` и `0003`: предыдущее поколение для G1 и G2.
    Prev,
}

impl ZmxSet {
    pub const ALL: [ZmxSet; 2] = [ZmxSet::Current, ZmxSet::Prev];

    pub fn name(self) -> &'static str {
        match self {
            ZmxSet::Current => "zmx",
            ZmxSet::Prev => "zmx-prev",
        }
    }

    pub fn from_name(name: &str) -> Option<ZmxSet> {
        ZmxSet::ALL.into_iter().find(|set| set.name() == name)
    }

    /// `ZMX_MANAGED` и сообщения о роли приходят с патчем `0001`.
    pub fn managed(self) -> bool {
        self == ZmxSet::Current
    }

    /// Теги `screen` (24) и `type` (25) приходят с патчем `0001`.
    pub fn has_screen(self) -> bool {
        self == ZmxSet::Current
    }

    pub fn has_type(self) -> bool {
        self == ZmxSet::Current
    }

    /// Бинарь набора в каталоге наборов пробы.
    pub fn binary(self, sets_dir: &Path) -> PathBuf {
        sets_dir.join(self.name()).join("bin").join("zmx")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub set: ZmxSet,
    pub binary: PathBuf,
    /// `ZMX_DIR`: путь, под которым сокет сессии и путь подготовки помещаются в `sun_path`.
    pub dir: PathBuf,
    /// Рабочий каталог команд zmx; демон переходит в него, нагрузка наследует.
    pub cwd: PathBuf,
    /// Окружение команд без `ZMX_*` и `TERM`: их задаёт адаптер.
    pub env: Vec<(String, String)>,
    /// Токен управляемого лидерства: латинские буквы, цифры и дефисы.
    pub token: String,
    /// Срок одной команды zmx.
    pub timeout: Duration,
}

/// Окружение команды по её роли.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvKind {
    /// `list`, `screen`, `send`, `type`, `kill`, `version`.
    Control,
    /// `ensure`: окружение наследует нагрузка.
    Create,
    Attach(Lead),
}

pub struct Zmx {
    config: Config,
    records: Mutex<BTreeMap<SessionId, HostRecord>>,
}

impl Zmx {
    /// Проверяет, что сокет любой допустимой сессии и путь подготовки помещаются в `sun_path`:
    /// иначе патч `0003` молча публикует сокет неатомарно, а это предмет клетки E5.
    pub fn new(config: Config) -> std::result::Result<Zmx, String> {
        check_socket_paths(&config.dir, SessionId::MAX_LEN)?;
        if !valid_token(&config.token) {
            return Err(format!("токен управляемого лидерства «{}»", config.token));
        }
        Ok(Zmx {
            config,
            records: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn set(&self) -> ZmxSet {
        self.config.set
    }

    /// Журнал демона сессии: `ZMX_DIR/logs/<имя>.log`.
    pub fn session_log(&self, id: &SessionId) -> PathBuf {
        self.config.dir.join("logs").join(format!("{id}.log"))
    }

    pub fn socket_path(&self, id: &SessionId) -> PathBuf {
        self.config.dir.join(id.as_str())
    }

    /// Имена сокетов в `ZMX_DIR` и в каталоге подготовки, без вызова `zmx`: `list` удаляет
    /// сокет, на котором подключение отвергнуто.
    pub fn socket_entries(&self) -> io::Result<SocketEntries> {
        Ok(SocketEntries {
            sessions: sockets_in(&self.config.dir)?,
            staged: match sockets_in(&self.config.dir.join(STAGING_DIR)) {
                Ok(names) => names,
                Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(err) => return Err(err),
            },
        })
    }

    pub fn version(&self) -> Result<Version> {
        let output = self.run(&["version".into()], EnvKind::Control, None)?;
        expect_success(&output.run)?;
        parse_version(&output.run.stdout).map_err(BackendError::Failed)
    }

    fn run(&self, args: &[OsString], kind: EnvKind, input: Option<&[u8]>) -> Result<Output> {
        let env = command_env(&self.config, kind)?;
        run_command(
            &Spawn {
                program: &self.config.binary,
                args,
                env: &env,
                cwd: &self.config.cwd,
                timeout: self.config.timeout,
            },
            match input {
                Some(bytes) => Stdin::Bytes(bytes),
                None => Stdin::Null,
            },
            None,
        )
        .map_err(BackendError::Failed)
    }

    fn require(&self, ok: bool, what: &str) -> Result<()> {
        if ok {
            Ok(())
        } else {
            Err(BackendError::Unsupported(format!(
                "{what} в комбинации {}",
                self.config.set.name()
            )))
        }
    }

    fn records(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, HostRecord>> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn sockets_in(dir: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_socket() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

impl Backend for Zmx {
    fn candidate(&self) -> Candidate {
        Candidate::Zmx
    }

    fn flavor(&self) -> &str {
        self.config.set.name()
    }

    fn ensure(&self, id: &SessionId, workload: &Workload, size: Winsize) -> Result<Ensured> {
        check_socket_paths(&self.config.dir, id.as_str().len()).map_err(BackendError::Failed)?;
        let previous = self.records().get(id).map(|record| record.workload);
        let env = command_env(&self.config, EnvKind::Create)?;
        let terminal = pty::open(size)
            .map_err(|err| BackendError::Failed(format!("PTY размера нагрузки: {err}")))?;
        let stdin = terminal
            .slave
            .try_clone()
            .map_err(|err| BackendError::Failed(format!("PTY: {err}")))?;
        let stdout =
            broken_pipe().map_err(|err| BackendError::Failed(format!("канал stdout: {err}")))?;
        let args = ensure_args(id, workload);
        let output = run_command(
            &Spawn {
                program: &self.config.binary,
                args: &args,
                env: &env,
                cwd: &self.config.cwd,
                timeout: self.config.timeout,
            },
            Stdin::Fd(stdin),
            Some(stdout),
        )
        .map_err(BackendError::Failed)?;
        let listed = self.list()?;
        drop(terminal);
        let run = output.run;
        let entry = listed
            .iter()
            .find(|entry| entry.name == id.as_str())
            .ok_or_else(|| {
                BackendError::Failed(format!("после ensure сессии {id} нет в списке: {run:?}"))
            })?;
        if let Some(error) = &entry.error {
            return Err(BackendError::Failed(format!(
                "после ensure хост {id} не отвечает: {error}"
            )));
        }
        let pid = entry
            .workload_pid
            .ok_or_else(|| BackendError::Failed(format!("в списке у {id} нет PID нагрузки")))?;
        let info = observer::inspect(pid)
            .map_err(BackendError::Failed)?
            .ok_or_else(|| BackendError::Failed(format!("нагрузки {id} с PID {pid} уже нет")))?;
        let host = observer::inspect(info.ppid)
            .map_err(BackendError::Failed)?
            .ok_or_else(|| {
                BackendError::Failed(format!("хоста {id} с PID {} уже нет", info.ppid))
            })?;
        let host_args = launch::process_args(host.key.pid).unwrap_or_default();
        if !is_host_of(&host_args, id) {
            return Err(BackendError::Failed(format!(
                "родитель нагрузки {id} — не демон zmx этой сессии: {host_args:?}"
            )));
        }
        let argv_matches = launch::process_args(pid).is_some_and(|argv| argv == workload.argv());
        let record = HostRecord {
            session: id.clone(),
            host: host.key,
            workload: info.key,
            workload_pgid: info.pgid,
            workload_sid: info.sid,
        };
        self.records().insert(id.clone(), record.clone());
        Ok(Ensured {
            created: argv_matches && previous != Some(info.key),
            record,
            command: run,
        })
    }

    fn attach_command(&self, id: &SessionId, lead: Lead) -> Result<ClientCommand> {
        Ok(ClientCommand {
            program: self.config.binary.clone(),
            args: attach_args(id),
            env: command_env(&self.config, EnvKind::Attach(lead))?,
        })
    }

    fn snapshot(&self, id: &SessionId) -> Result<Snapshot> {
        self.require(self.config.set.has_screen(), "нет команды screen")?;
        let output = self.run(
            &["screen".into(), id.as_str().into()],
            EnvKind::Control,
            None,
        )?;
        expect_success(&output.run)?;
        parse_screen(&output.stdout).map_err(BackendError::Failed)
    }

    fn input(&self, id: &SessionId, bytes: &[u8], path: InputPath) -> Result<InputOutcome> {
        if bytes.is_empty() {
            return Err(BackendError::Failed("пустой ввод".to_owned()));
        }
        match path {
            InputPath::Raw => {
                // Байты одним аргументом: из stdin `send` отбрасывает последний перевод строки.
                if bytes.contains(&0) {
                    return Err(BackendError::Unsupported(
                        "NUL в аргументе zmx send".to_owned(),
                    ));
                }
                let args = [
                    OsString::from("send"),
                    id.as_str().into(),
                    std::ffi::OsStr::from_bytes(bytes).to_owned(),
                ];
                let output = self.run(&args, EnvKind::Control, None)?;
                expect_success(&output.run)?;
                Ok(InputOutcome::Sent)
            }
            InputPath::Typed => {
                self.require(self.config.set.has_type(), "нет команды type")?;
                let output = self.run(
                    &["type".into(), id.as_str().into()],
                    EnvKind::Control,
                    Some(bytes),
                )?;
                Ok(type_outcome(&output.run)?)
            }
        }
    }

    fn resize(&self, _id: &SessionId, _size: Winsize) -> Result<()> {
        Err(BackendError::Unsupported(
            "ресайз без клиента: размер задаёт лидер".to_owned(),
        ))
    }

    fn stop(&self, id: &SessionId, grace: Duration) -> Result<StopOutcome> {
        let record = self
            .record(id)
            .ok_or_else(|| BackendError::Failed(format!("нет записи о сессии {id}")))?;
        let processes = session_processes(&record)?;
        let output = self.run(&["kill".into(), id.as_str().into()], EnvKind::Control, None)?;
        let alive_after_command = alive(&processes)?;
        let started = Instant::now();
        let pids: Vec<i32> = alive_after_command.iter().map(|key| key.pid).collect();
        launch::wait_exit(&pids, grace);
        let survivors = alive(&alive_after_command)?;
        Ok(StopOutcome {
            command: output.run,
            processes,
            alive_after_command,
            survivors,
            waited_ms: started.elapsed().as_millis() as u64,
        })
    }

    fn list(&self) -> Result<Vec<Listed>> {
        let output = self.run(&["list".into()], EnvKind::Control, None)?;
        expect_success(&output.run)?;
        Ok(parse_list(&output.run.stdout))
    }

    fn record(&self, id: &SessionId) -> Option<HostRecord> {
        self.records().get(id).cloned()
    }

    fn liveness(&self, id: &SessionId) -> Result<Liveness> {
        let socket_present = fs::symlink_metadata(self.socket_path(id))
            .is_ok_and(|meta| meta.file_type().is_socket());
        let record = self.record(id);
        let mut listed = None;
        let judged = judge_liveness(record.as_ref(), socket_present, || {
            let entries = match self.list() {
                Ok(entries) => entries,
                Err(err) => {
                    listed = Some(err);
                    return Liveness::Absent;
                }
            };
            match entries.into_iter().find(|entry| entry.name == id.as_str()) {
                Some(Listed {
                    error: Some(error), ..
                }) => Liveness::Unreachable { error },
                Some(Listed {
                    workload_pid: Some(workload_pid),
                    clients,
                    ..
                }) => Liveness::Live {
                    workload_pid,
                    clients: clients.unwrap_or(0),
                },
                Some(_) => Liveness::Unreachable {
                    error: "в списке нет PID нагрузки".to_owned(),
                },
                None => Liveness::Unreachable {
                    error: "хост жив, но сессии нет в списке".to_owned(),
                },
            }
        })?;
        match listed {
            Some(err) => Err(err),
            None => Ok(judged),
        }
    }

    fn read_boundary(&self, id: &SessionId) -> Result<u64> {
        Ok(self.snapshot(id)?.revision)
    }

    fn role_prefix(&self) -> Option<String> {
        self.config
            .set
            .managed()
            .then(|| role_prefix(&self.config.token))
    }

    fn socket_path(&self, id: &SessionId) -> PathBuf {
        Zmx::socket_path(self, id)
    }

    fn socket_entries(&self) -> io::Result<SocketEntries> {
        Zmx::socket_entries(self)
    }

    fn kill_host(&self, id: &SessionId, timeout: Duration) -> Result<ProcessKey> {
        let record = self
            .record(id)
            .ok_or_else(|| BackendError::Failed(format!("нет записи о сессии {id}")))?;
        kill_recorded_host(&record, timeout)
    }
}

// --- чистые части ---

pub fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 64
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Сокет сессии с именем длиной `name_len` и путь подготовки `0003` помещаются в `sun_path`.
pub fn check_socket_paths(dir: &Path, name_len: usize) -> std::result::Result<(), String> {
    let dir_len = dir.as_os_str().len();
    let session = dir_len + 1 + name_len;
    let staged = dir_len + 1 + STAGING_DIR.len() + 1 + MAX_PID_DIGITS;
    if session > MAX_SOCKET_PATH {
        return Err(format!(
            "сокет сессии в {} с именем {name_len} байт — {session} байт, больше {MAX_SOCKET_PATH}",
            dir.display()
        ));
    }
    if staged > MAX_SOCKET_PATH {
        return Err(format!(
            "путь подготовки {}/{STAGING_DIR}/<pid> — {staged} байт, больше {MAX_SOCKET_PATH}: \
             патч 0003 опубликовал бы сокет неатомарно",
            dir.display()
        ));
    }
    Ok(())
}

pub fn ensure_args(id: &SessionId, workload: &Workload) -> Vec<OsString> {
    ["attach".to_owned(), id.as_str().to_owned()]
        .into_iter()
        .chain(workload.argv())
        .map(OsString::from)
        .collect()
}

pub fn attach_args(id: &SessionId) -> Vec<String> {
    vec!["attach".to_owned(), id.as_str().to_owned()]
}

/// Окружение команды: базовое без `ZMX_*` и `TERM`, затем `ZMX_DIR`, у создания и attach `TERM`,
/// у attach текущей комбинации — управляемое лидерство.
pub fn command_env(config: &Config, kind: EnvKind) -> Result<Vec<(String, String)>> {
    let mut env: Vec<(String, String)> = config
        .env
        .iter()
        .filter(|(name, _)| !name.starts_with("ZMX_") && name != "TERM")
        .cloned()
        .collect();
    env.push(("ZMX_DIR".to_owned(), config.dir.display().to_string()));
    if matches!(kind, EnvKind::Create | EnvKind::Attach(_)) {
        env.push(("TERM".to_owned(), TERM_VALUE.to_owned()));
    }
    if let EnvKind::Attach(lead) = kind {
        match (config.set.managed(), lead) {
            (true, _) => {
                env.push(("ZMX_MANAGED".to_owned(), config.token.clone()));
                if lead == Lead::Claim {
                    env.push(("ZMX_MANAGED_CLAIM".to_owned(), "1".to_owned()));
                }
            }
            (false, Lead::IfFree) => {}
            (false, Lead::Claim) => {
                return Err(BackendError::Unsupported(format!(
                    "захват лидерства в комбинации {}",
                    config.set.name()
                )));
            }
        }
    }
    Ok(env)
}

/// Префикс заголовка, которым управляемый клиент сообщает роль.
pub fn role_prefix(token: &str) -> String {
    format!("{ROLE_TITLE}{token}:")
}

/// Разбор `zmx list`: строки `name=…\tpid=…\tclients=…\t…` или `name=…\terr=…\tstatus=…`, перед
/// именем — два пробела или стрелка текущей сессии.
pub fn parse_list(text: &str) -> Vec<Listed> {
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim_start_matches(['→', ' ']);
        let fields: BTreeMap<&str, &str> = line
            .split('\t')
            .filter_map(|field| field.split_once('='))
            .collect();
        let Some(name) = fields.get("name") else {
            continue;
        };
        found.push(Listed {
            name: (*name).to_owned(),
            workload_pid: fields.get("pid").and_then(|pid| pid.parse().ok()),
            clients: fields.get("clients").and_then(|count| count.parse().ok()),
            error: fields.get("err").map(|err| (*err).to_owned()),
        });
    }
    found
}

/// `zmx version`: `zmx`, `ghostty_vt`, `socket_dir`, `log_dir`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub zmx: String,
    pub ghostty_vt: String,
    pub socket_dir: String,
    pub log_dir: String,
}

pub fn parse_version(text: &str) -> std::result::Result<Version, String> {
    let fields: BTreeMap<&str, &str> = text
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let split = line.find(char::is_whitespace)?;
            Some((&line[..split], line[split..].trim()))
        })
        .collect();
    let field = |name: &str| {
        fields
            .get(name)
            .map(|value| (*value).to_owned())
            .ok_or_else(|| format!("в выводе zmx version нет {name}: {text:?}"))
    };
    Ok(Version {
        zmx: field("zmx")?,
        ghostty_vt: field("ghostty_vt")?,
        socket_dir: field("socket_dir")?,
        log_dir: field("log_dir")?,
    })
}

/// `zmx screen`: строка `ревизия колонки строки x y alternate`, затем текст экрана.
pub fn parse_screen(bytes: &[u8]) -> std::result::Result<Snapshot, String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or("в выводе zmx screen нет строки заголовка")?;
    let header = std::str::from_utf8(&bytes[..end])
        .map_err(|_| "заголовок zmx screen не в UTF-8".to_owned())?;
    let numbers: Vec<u64> = header
        .split_whitespace()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| format!("заголовок zmx screen «{header}»"))?;
    let [revision, cols, rows, x, y, alternate] = numbers.as_slice() else {
        return Err(format!("в заголовке zmx screen не 6 чисел: «{header}»"));
    };
    let small = |value: u64, name: &str| {
        u16::try_from(value).map_err(|_| format!("{name} {value} в заголовке zmx screen"))
    };
    if *alternate > 1 {
        return Err(format!("alternate {alternate} в заголовке zmx screen"));
    }
    Ok(Snapshot {
        revision: *revision,
        cols: small(*cols, "колонки")?,
        rows: small(*rows, "строки")?,
        cursor_x: small(*x, "x")?,
        cursor_y: small(*y, "y")?,
        alternate: *alternate == 1,
        text: String::from_utf8_lossy(&bytes[end + 1..]).into_owned(),
    })
}

/// Ответ `zmx type`: ноль — принято в очередь, отказ очереди — `Rejected`, остальное — ошибка.
pub fn type_outcome(run: &CommandRun) -> Result<InputOutcome> {
    match run.code {
        Some(0) => Ok(InputOutcome::Accepted),
        Some(1) if run.stderr.contains("type rejected") => {
            Ok(InputOutcome::Rejected(run.stderr.trim().to_owned()))
        }
        _ => Err(BackendError::Failed(format!("zmx type: {run:?}"))),
    }
}

/// Аргументы процесса — демон zmx этой сессии: процесс команды `attach <имя>` после `fork`.
pub fn is_host_of(args: &[String], id: &SessionId) -> bool {
    matches!(args, [_, command, name, ..] if (command == "attach" || command == "a") && name == id.as_str())
}

/// Сколько раз демон назначал лидера: клиент с `Init` становится лидером свободной сессии.
pub fn leadership_events(log: &str) -> usize {
    log.lines()
        .filter(|line| line.contains("setting new leader"))
        .count()
}

/// Сколько раз демон отправлял снимок состояния клиенту.
pub fn snapshot_events(log: &str) -> usize {
    log.lines()
        .filter(|line| line.contains("serialize terminal state"))
        .count()
}

fn expect_success(run: &CommandRun) -> Result<()> {
    if run.code == Some(0) {
        Ok(())
    } else {
        Err(BackendError::Failed(format!("zmx: {run:?}")))
    }
}

// --- запуск команд ---

struct Spawn<'a> {
    program: &'a Path,
    args: &'a [OsString],
    env: &'a [(String, String)],
    cwd: &'a Path,
    timeout: Duration,
}

enum Stdin<'a> {
    Null,
    Bytes(&'a [u8]),
    Fd(OwnedFd),
}

#[derive(Debug)]
struct Output {
    run: CommandRun,
    stdout: Vec<u8>,
}

/// Канал, у которого конец чтения уже закрыт: первая запись получает `EPIPE`.
fn broken_pipe() -> io::Result<OwnedFd> {
    let _guard = pty::spawn_lock();
    let mut fds = [-1; 2];
    // SAFETY: массив из двух дескрипторов на стеке.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe вернул два открытых дескриптора, которыми больше никто не владеет.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [read_end.as_raw_fd(), write_end.as_raw_fd()] {
        // SAFETY: флаги открытого дескриптора читаются и пишутся без указателей.
        let status = unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 {
                flags
            } else {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC)
            }
        };
        if status < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    drop(read_end);
    Ok(write_end)
}

/// Команда со сроком: stdout и stderr читаются потоками, по сроку процесс снимается `SIGKILL`.
fn run_command(
    spawn: &Spawn<'_>,
    stdin: Stdin<'_>,
    stdout: Option<OwnedFd>,
) -> std::result::Result<Output, String> {
    let started = Instant::now();
    let mut command = Command::new(spawn.program);
    command
        .args(spawn.args)
        .env_clear()
        .envs(spawn.env.iter().map(|(name, value)| (name, value)))
        .current_dir(spawn.cwd)
        .stderr(Stdio::piped());
    let mut input = None;
    match stdin {
        Stdin::Null => {
            command.stdin(Stdio::null());
        }
        Stdin::Bytes(bytes) => {
            command.stdin(Stdio::piped());
            input = Some(bytes.to_vec());
        }
        Stdin::Fd(fd) => {
            command.stdin(Stdio::from(fd));
        }
    }
    let piped_stdout = stdout.is_none();
    match stdout {
        Some(fd) => command.stdout(Stdio::from(fd)),
        None => command.stdout(Stdio::piped()),
    };
    let args: Vec<String> = spawn
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let mut child = {
        let _guard = pty::spawn_lock();
        command
            .spawn()
            .map_err(|err| format!("{} не запущен: {err}", spawn.program.display()))?
    };
    // Команда с перенаправленным stdout отпускает дескрипторы в `command` только с ним.
    drop(command);
    let writer = match (input, child.stdin.take()) {
        (Some(bytes), Some(mut pipe)) => Some(thread::spawn(move || {
            let _ = pipe.write_all(&bytes);
        })),
        _ => None,
    };
    let stdout_rx = piped_stdout.then(|| reader(child.stdout.take()));
    let stderr_rx = reader(child.stderr.take());
    let status = match wait_child(&mut child, spawn.timeout) {
        Ok(status) => status,
        Err(err) => {
            kill_and_reap(&mut child);
            return Err(format!("zmx {}: {err}", args.join(" ")));
        }
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let left = spawn.timeout.saturating_sub(started.elapsed());
    let stdout = match stdout_rx {
        Some(rx) => collect(&rx, left, "stdout")?,
        None => Vec::new(),
    };
    let stderr = collect(&stderr_rx, left, "stderr")?;
    Ok(Output {
        run: CommandRun {
            args,
            code: status.code(),
            signal: status.signal(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            elapsed_ms: started.elapsed().as_millis() as u64,
        },
        stdout,
    })
}

fn kill_and_reap(child: &mut Child) {
    let _ = signal(child.id() as i32, libc::SIGKILL);
    let _ = child.wait();
}

fn reader(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    match pipe {
        Some(mut pipe) => {
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = pipe
                    .by_ref()
                    .take(MAX_OUTPUT as u64)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes);
                let _ = tx.send(result);
            });
        }
        None => {
            let _ = tx.send(Ok(Vec::new()));
        }
    }
    rx
}

fn collect(
    rx: &mpsc::Receiver<io::Result<Vec<u8>>>,
    timeout: Duration,
    name: &str,
) -> std::result::Result<Vec<u8>, String> {
    match rx.recv_timeout(timeout) {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(err)) => Err(format!("{name} команды zmx: {err}")),
        Err(_) => Err(format!(
            "{name} команды zmx не закрылся: его держит другой процесс"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(set: ZmxSet) -> Config {
        Config {
            set,
            binary: PathBuf::from("/sets/zmx/bin/zmx"),
            dir: PathBuf::from("/tmp/pss-0123456789abcdef/z"),
            cwd: PathBuf::from("/tmp/pss-0123456789abcdef"),
            env: vec![
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                ("HOME".to_owned(), "/Users/t".to_owned()),
                ("ZMX_SESSION".to_owned(), "outer".to_owned()),
                ("TERM".to_owned(), "dumb".to_owned()),
            ],
            token: "pss-t1".to_owned(),
            timeout: Duration::from_secs(5),
        }
    }

    fn id(name: &str) -> SessionId {
        SessionId::new(name).unwrap()
    }

    fn names(env: &[(String, String)]) -> Vec<&str> {
        env.iter().map(|(name, _)| name.as_str()).collect()
    }

    #[test]
    fn sets_name_binaries_and_capabilities() {
        assert_eq!(ZmxSet::from_name("zmx-prev"), Some(ZmxSet::Prev));
        assert_eq!(ZmxSet::from_name("zmx-next"), None);
        assert_eq!(
            ZmxSet::Prev.binary(Path::new("/r/sets")),
            PathBuf::from("/r/sets/zmx-prev/bin/zmx")
        );
        assert!(ZmxSet::Current.managed() && ZmxSet::Current.has_screen());
        assert!(!ZmxSet::Prev.managed() && !ZmxSet::Prev.has_type());
    }

    #[test]
    fn control_env_isolates_zmx_dir_and_drops_outer_session() {
        let env = command_env(&config(ZmxSet::Current), EnvKind::Control).unwrap();
        assert_eq!(names(&env), ["PATH", "HOME", "ZMX_DIR"]);
        assert_eq!(env[2].1, "/tmp/pss-0123456789abcdef/z");
        let create = command_env(&config(ZmxSet::Current), EnvKind::Create).unwrap();
        assert_eq!(names(&create), ["PATH", "HOME", "ZMX_DIR", "TERM"]);
        assert_eq!(create[3].1, "xterm-256color");
    }

    #[test]
    fn attach_env_opts_into_managed_leadership_only_in_current_set() {
        let current = config(ZmxSet::Current);
        let free = command_env(&current, EnvKind::Attach(Lead::IfFree)).unwrap();
        assert_eq!(
            names(&free),
            ["PATH", "HOME", "ZMX_DIR", "TERM", "ZMX_MANAGED"]
        );
        assert_eq!(free[4].1, "pss-t1");
        let claim = command_env(&current, EnvKind::Attach(Lead::Claim)).unwrap();
        assert_eq!(names(&claim).last(), Some(&"ZMX_MANAGED_CLAIM"));
        let prev = config(ZmxSet::Prev);
        let plain = command_env(&prev, EnvKind::Attach(Lead::IfFree)).unwrap();
        assert_eq!(names(&plain), ["PATH", "HOME", "ZMX_DIR", "TERM"]);
        assert!(matches!(
            command_env(&prev, EnvKind::Attach(Lead::Claim)),
            Err(BackendError::Unsupported(_))
        ));
    }

    #[test]
    fn commands_carry_session_and_workload() {
        let workload = Workload {
            program: PathBuf::from("/bin/fixture"),
            args: vec!["--mode".to_owned(), "emitter".to_owned()],
        };
        assert_eq!(
            ensure_args(&id("s1-a1"), &workload),
            ["attach", "s1-a1", "/bin/fixture", "--mode", "emitter"].map(OsString::from)
        );
        assert_eq!(attach_args(&id("s1-a1")), ["attach", "s1-a1"]);
        let zmx = Zmx::new(config(ZmxSet::Current)).unwrap();
        let client = zmx.attach_command(&id("s1-a1"), Lead::IfFree).unwrap();
        assert_eq!(client.program, PathBuf::from("/sets/zmx/bin/zmx"));
        assert_eq!(client.args, ["attach", "s1-a1"]);
        assert_eq!(zmx.role_prefix().as_deref(), Some("zmx-role;pss-t1:"));
        assert_eq!(
            zmx.session_log(&id("s1-a1")),
            PathBuf::from("/tmp/pss-0123456789abcdef/z/logs/s1-a1.log")
        );
        let prev = Zmx::new(config(ZmxSet::Prev)).unwrap();
        assert_eq!(prev.role_prefix(), None);
        assert!(matches!(
            prev.snapshot(&id("s1")),
            Err(BackendError::Unsupported(_))
        ));
        assert!(matches!(
            zmx.resize(&id("s1"), Winsize::cells(24, 80)),
            Err(BackendError::Unsupported(_))
        ));
    }

    #[test]
    fn socket_paths_must_fit_sun_path_including_staging() {
        assert!(check_socket_paths(Path::new("/tmp/pss-0123456789abcdef/z"), 48).is_ok());
        let long = PathBuf::from(format!("/{}", "d".repeat(60)));
        let err = check_socket_paths(&long, 48).unwrap_err();
        assert!(err.contains("сокет сессии"), "{err}");
        // Имя помещается, а путь подготовки — нет: это запасной путь 0003 из клетки E5.
        let edge = PathBuf::from(format!("/{}", "d".repeat(96)));
        let err = check_socket_paths(&edge, 2).unwrap_err();
        assert!(err.contains("путь подготовки"), "{err}");
        assert!(check_socket_paths(&PathBuf::from(format!("/{}", "d".repeat(93))), 2).is_ok());
        let mut long_config = config(ZmxSet::Current);
        long_config.dir = PathBuf::from(format!("/{}", "d".repeat(60)));
        assert!(Zmx::new(long_config).is_err());
        let mut bad_token = config(ZmxSet::Current);
        bad_token.token = "a;b".to_owned();
        assert!(Zmx::new(bad_token).is_err());
    }

    #[test]
    fn list_lines_are_parsed_with_errors_and_labels() {
        let text = "  name=s1-a1\tpid=53303\tclients=2\tcreated=1794854604\tcwd=file://h/tmp\tcmd=/bin/f --mode x\tproject=p\n\
                    → name=cur\tpid=7\tclients=0\tcreated=1\n\
                    \x20 name=s1-dead\terr=ConnectionRefused\tstatus=cleaning up\n\
                    garbage line\n";
        assert_eq!(
            parse_list(text),
            [
                Listed {
                    name: "s1-a1".to_owned(),
                    workload_pid: Some(53303),
                    clients: Some(2),
                    error: None,
                },
                Listed {
                    name: "cur".to_owned(),
                    workload_pid: Some(7),
                    clients: Some(0),
                    error: None,
                },
                Listed {
                    name: "s1-dead".to_owned(),
                    workload_pid: None,
                    clients: None,
                    error: Some("ConnectionRefused".to_owned()),
                },
            ]
        );
        assert!(parse_list("").is_empty());
    }

    #[test]
    fn version_fields_are_parsed() {
        let text = "zmx\t\t0.8.1\nghostty_vt\t1.3.2-dev+patched\nsocket_dir\t/tmp/z\nlog_dir\t\t/tmp/z/logs\n";
        assert_eq!(
            parse_version(text).unwrap(),
            Version {
                zmx: "0.8.1".to_owned(),
                ghostty_vt: "1.3.2-dev+patched".to_owned(),
                socket_dir: "/tmp/z".to_owned(),
                log_dir: "/tmp/z/logs".to_owned(),
            }
        );
        assert!(parse_version("zmx\t0.8.1\n").is_err());
    }

    #[test]
    fn screen_header_carries_revision_geometry_and_cursor() {
        let snapshot = parse_screen(b"17 100 30 4 1 0\nhello\n> ab").unwrap();
        assert_eq!(
            snapshot,
            Snapshot {
                revision: 17,
                cols: 100,
                rows: 30,
                cursor_x: 4,
                cursor_y: 1,
                alternate: false,
                text: "hello\n> ab".to_owned(),
            }
        );
        assert!(parse_screen(b"1 2 3 0 0 1\n").unwrap().alternate);
        for bad in [
            &b"no header"[..],
            b"1 2 3\ntext",
            b"1 2 3 4 5 2\n",
            b"1 70000 3 4 5 0\n",
            b"x 2 3 4 5 0\n",
        ] {
            assert!(
                parse_screen(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn role_prefix_carries_the_token() {
        assert_eq!(role_prefix("t1"), "zmx-role;t1:");
        assert_eq!(parse_role("leader:3").map(|r| r.role), Some(Role::Leader));
    }

    #[test]
    fn type_replies_split_rejection_from_failure() {
        let run = |code, stderr: &str| CommandRun {
            code: Some(code),
            stderr: stderr.to_owned(),
            ..CommandRun::default()
        };
        assert_eq!(type_outcome(&run(0, "")).unwrap(), InputOutcome::Accepted);
        assert!(matches!(
            type_outcome(&run(
                1,
                "error: type rejected by \"s\": input queue is full\n"
            )),
            Ok(InputOutcome::Rejected(_))
        ));
        assert!(type_outcome(&run(1, "error: type failed for \"s\": Timeout\n")).is_err());
    }

    #[test]
    fn host_is_the_attach_command_of_its_session() {
        let args = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        assert!(is_host_of(
            &args(&["/z/zmx", "attach", "s1", "/bin/f"]),
            &id("s1")
        ));
        assert!(!is_host_of(&args(&["/z/zmx", "attach", "s2"]), &id("s1")));
        assert!(!is_host_of(&args(&["/z/zmx", "list"]), &id("s1")));
    }

    #[test]
    fn daemon_log_counts_leadership_and_snapshots() {
        let log = "[1] [info] (default): client connected fd=9 total=1\n\
                   [1] [info] (default): setting new leader client_fd=10\n\
                   [1] [debug] (default): serialize terminal state\n\
                   [2] [info] (default): setting new leader client_fd=11\n";
        assert_eq!(leadership_events(log), 2);
        assert_eq!(snapshot_events(log), 1);
        assert_eq!(leadership_events(""), 0);
    }

    #[test]
    fn commands_run_with_deadline_and_capture_output() {
        let spawn = |args: &[&str], timeout| {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            run_command(
                &Spawn {
                    program: Path::new("/bin/sh"),
                    args: &args,
                    env: &[],
                    cwd: Path::new("/"),
                    timeout,
                },
                Stdin::Bytes(b"in"),
                None,
            )
        };
        let output = spawn(
            &["-c", "cat; echo err >&2; exit 3"],
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(output.stdout, b"in");
        assert_eq!(output.run.stderr, "err\n");
        assert_eq!(output.run.code, Some(3));
        let err = spawn(
            &["-c", "exec /bin/cat /dev/zero >/dev/null"],
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(err.contains("не вышел"), "{err}");
    }

    #[test]
    fn broken_pipe_fails_the_first_write() {
        let fd = broken_pipe().unwrap();
        let mut file = std::fs::File::from(fd);
        let err = file.write_all(b"x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
