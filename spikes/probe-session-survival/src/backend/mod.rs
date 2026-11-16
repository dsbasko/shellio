//! Кандидаты хоста сессии в форме контракта `SessionBackend` (ADR-0005), которая нужна матрице:
//! операции сессии, наблюдения для оракулов и управление сбоями для стенда. Это форма контракта,
//! а не продуктовый `shellio-session`.
//!
//! Модуль чистый: команды кандидатов запускаются как процессы, FFI нет, поэтому разбор их вывода и
//! построение команд идут в `make check`.

pub mod zmx;

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::launch;
use crate::matrix::Candidate;
use crate::observer::ProcessKey;
use crate::pty::Winsize;

/// Имя сессии: строчные латинские буквы, цифры и дефисы, не с дефиса. Короткое: путь сокета
/// сессии должен помещаться в `sun_path`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub const MAX_LEN: usize = 48;

    pub fn new(name: &str) -> std::result::Result<SessionId, String> {
        let valid = !name.is_empty()
            && name.len() <= SessionId::MAX_LEN
            && !name.starts_with('-')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if valid {
            Ok(SessionId(name.to_owned()))
        } else {
            Err(format!(
                "имя сессии «{name}»: нужны 1–{} строчных букв, цифр и дефисов, не с дефиса",
                SessionId::MAX_LEN
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Нагрузка сессии: программа и аргументы. Окружение нагрузки задаёт кандидат.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workload {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Workload {
    /// `argv` нагрузки так, как его видит ядро: программа и аргументы.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.display().to_string())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

/// Запись о сессии, сохранённая при `ensure`: по ней находятся процессы после потери хоста.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRecord {
    pub session: SessionId,
    pub host: ProcessKey,
    pub workload: ProcessKey,
    pub workload_pgid: i32,
    pub workload_sid: Option<i32>,
}

/// Итог `ensure`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ensured {
    pub record: HostRecord,
    /// Нагрузка запущена этим вызовом, а не найдена готовой.
    pub created: bool,
    pub command: CommandRun,
}

/// Запущенная команда кандидата и её итог: сырой след для артефактов.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRun {
    pub args: Vec<String>,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_ms: u64,
}

/// Строка списка сессий кандидата.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listed {
    pub name: String,
    /// PID нагрузки по ответу хоста.
    pub workload_pid: Option<i32>,
    pub clients: Option<u32>,
    /// Хост не ответил: ошибка подключения.
    pub error: Option<String>,
}

/// Что известно о сессии по записи, процессам и списку.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Liveness {
    Live {
        workload_pid: i32,
        clients: u32,
    },
    /// Хост жив, но на запрос не ответил.
    Unreachable {
        error: String,
    },
    /// Хоста нет, а его сокет остался: хост потерян, а не завершён.
    Lost,
    /// Хоста нет и сокета нет: сессия закончилась.
    Ended,
    /// Ни записи, ни сессии.
    Absent,
}

/// Снимок состояния без клиента: ревизия обработки, геометрия, курсор и текст.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Растёт с каждым чтением PTY, поданным в VT хоста, ресайзом и внедрённым выводом.
    pub revision: u64,
    pub cols: u16,
    pub rows: u16,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub alternate: bool,
    pub text: String,
}

/// Путь ввода без клиента.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputPath {
    /// Байты как есть, без подтверждения.
    Raw,
    /// Текст клавишами по режимам нагрузки, с ответом «принято» или «отказ».
    Typed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "detail", rename_all = "kebab-case")]
pub enum InputOutcome {
    /// Отправлено; у пути без подтверждения это не значит, что принято.
    Sent,
    Accepted,
    Rejected(String),
}

/// Как attach-клиент берёт лидерство.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Lead {
    /// Ведёт, если сессию никто не ведёт.
    IfFree,
    /// Забирает лидерство у живого лидера.
    Claim,
}

/// Команда attach-клиента: стенд запускает её в PTY окна клиента.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Итог `stop`: подтверждение — гибель процессов сессии, а не исчезновение сокета.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopOutcome {
    pub command: CommandRun,
    /// Процессы сессии на момент `stop`: хост, нагрузка и её потомки.
    pub processes: Vec<ProcessKey>,
    /// Живы сразу после возврата команды остановки.
    pub alive_after_command: Vec<ProcessKey>,
    /// Живы после ожидания.
    pub survivors: Vec<ProcessKey>,
    pub waited_ms: u64,
}

impl StopOutcome {
    pub fn confirmed(&self) -> bool {
        self.survivors.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// У кандидата нет этой операции.
    Unsupported(String),
    Failed(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Unsupported(what) => write!(f, "нет у кандидата: {what}"),
            BackendError::Failed(reason) => f.write_str(reason),
        }
    }
}

pub type Result<T> = std::result::Result<T, BackendError>;

/// Кандидат хоста сессии для матрицы. Операции сессии повторяют `SessionBackend` из ADR-0005;
/// наблюдения и управление сбоями нужны только стенду.
pub trait Backend {
    fn candidate(&self) -> Candidate;
    /// Имя комбинации или сборки кандидата для отчёта.
    fn flavor(&self) -> &str;

    // --- операции сессии ---

    /// Create-or-attach без скрытого attach: сессия существует, клиентов у неё не прибавилось.
    fn ensure(&self, id: &SessionId, workload: &Workload, size: Winsize) -> Result<Ensured>;
    fn attach_command(&self, id: &SessionId, lead: Lead) -> Result<ClientCommand>;
    fn snapshot(&self, id: &SessionId) -> Result<Snapshot>;
    fn input(&self, id: &SessionId, bytes: &[u8], path: InputPath) -> Result<InputOutcome>;
    fn resize(&self, id: &SessionId, size: Winsize) -> Result<()>;
    /// Останавливает сессию и ждёт гибели её процессов не дольше `grace`.
    fn stop(&self, id: &SessionId, grace: Duration) -> Result<StopOutcome>;
    fn list(&self) -> Result<Vec<Listed>>;

    // --- наблюдения ---

    /// Запись последнего `ensure` этой сессии.
    fn record(&self, id: &SessionId) -> Option<HostRecord>;
    fn liveness(&self, id: &SessionId) -> Result<Liveness>;
    /// Ревизия обработки для протокола разреза.
    fn read_boundary(&self, id: &SessionId) -> Result<u64>;
    /// Префикс заголовка, которым клиент сообщает терминалу роль; `None` — клиент её не сообщает.
    fn role_prefix(&self) -> Option<String>;

    // --- управление сбоями ---

    /// `SIGKILL` хосту сессии; возвращается после его выхода.
    fn kill_host(&self, id: &SessionId, timeout: Duration) -> Result<ProcessKey>;

    /// `SIGKILL` attach-клиенту.
    fn kill_client(&self, client: &mut Child, timeout: Duration) -> Result<ExitStatus> {
        signal(client.id() as i32, libc::SIGKILL)
            .map_err(|err| BackendError::Failed(format!("SIGKILL клиенту: {err}")))?;
        wait_child(client, timeout)
    }

    /// Остановка (`SIGSTOP`) или продолжение (`SIGCONT`) attach-клиента.
    fn stall_client(&self, client: &Child, stalled: bool) -> Result<()> {
        let sig = if stalled {
            libc::SIGSTOP
        } else {
            libc::SIGCONT
        };
        signal(client.id() as i32, sig)
            .map_err(|err| BackendError::Failed(format!("сигнал {sig} клиенту: {err}")))
    }
}

pub fn signal(pid: i32, sig: libc::c_int) -> io::Result<()> {
    // SAFETY: сигнал процессу по PID, памяти вызов не касается.
    if unsafe { libc::kill(pid, sig) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Ждёт выхода своего потомка не дольше `timeout`: выход ловит `kqueue`, затем потомок прибирается.
pub fn wait_child(child: &mut Child, timeout: Duration) -> Result<ExitStatus> {
    let pid = child.id() as i32;
    if let Some(status) = child
        .try_wait()
        .map_err(|err| BackendError::Failed(format!("PID {pid}: {err}")))?
    {
        return Ok(status);
    }
    if !launch::wait_exit(&[pid], timeout).is_empty() {
        return Err(BackendError::Failed(format!(
            "PID {pid} не вышел за {} мс",
            timeout.as_millis()
        )));
    }
    child
        .wait()
        .map_err(|err| BackendError::Failed(format!("PID {pid}: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_short_lowercase_tokens() {
        assert_eq!(SessionId::new("s0a1-b2").unwrap().as_str(), "s0a1-b2");
        for bad in ["", "-a", "A1", "a_b", "a b", "a/b", &"a".repeat(49)] {
            assert!(SessionId::new(bad).is_err(), "{bad}");
        }
        assert!(SessionId::new(&"a".repeat(48)).is_ok());
    }

    #[test]
    fn workload_argv_starts_with_the_program() {
        let workload = Workload {
            program: PathBuf::from("/bin/fixture"),
            args: vec!["--mode".to_owned(), "shell".to_owned()],
        };
        assert_eq!(workload.argv(), ["/bin/fixture", "--mode", "shell"]);
    }

    #[test]
    fn stop_is_confirmed_only_without_survivors() {
        let key = ProcessKey {
            pid: 10,
            start_us: 1,
        };
        let mut outcome = StopOutcome {
            command: CommandRun::default(),
            processes: vec![key],
            alive_after_command: vec![key],
            survivors: Vec::new(),
            waited_ms: 5,
        };
        assert!(outcome.confirmed());
        outcome.survivors.push(key);
        assert!(!outcome.confirmed());
    }

    #[test]
    fn waiting_for_a_child_reaps_it() {
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let status = wait_child(&mut child, Duration::from_secs(10)).unwrap();
        assert!(status.success());
    }
}
