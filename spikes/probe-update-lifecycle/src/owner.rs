//! Роль GUI пробы (задача 6; план P0.5, «Владение блокировкой и сокетом»; ADR-0006): владение
//! `<state>/shellio.lock` и управляющим сокетом `<state>/shellio.sock`, право владения по CDHash
//! процесса из ядра и ключу владельца, реестр сессий, запуск хостов из `runtime/<N>/`, второй
//! запуск и клиент пробы.
//!
//! Порядок взявшего блокировку: восстановление попытки обновления по таблице ([`recover`]),
//! затем решение о праве ([`decide_role`]). Право есть, только если CDHash процесса равен CDHash
//! установленного бандла, прошедшего `codesign --verify --strict --deep -R`, а вне `idle` и
//! `staged-deferred` — ещё и ключ владельца попытки. Владелец готовит рантайм, удаляет оставшийся
//! путь сокета прежнего владельца и публикует свой: `bind` временного имени, `listen` и `rename`,
//! поэтому путь сокета появляется уже слушающим. Процесс без права отпускает блокировку до любого
//! ожидания сокета, запускает установленный бандл через `open`, если владельца нет, и передаёт
//! аргументы владельцу. Второй запуск, не получивший блокировку, ждёт публикации сокета не
//! дольше [`SOCKET_WAIT`] (`kqueue` на каталог состояния) и повторяет взятие блокировки, если
//! владелец исчез (блокирующий `flock` в отдельном потоке будит ожидание событием
//! `EVFILT_USER`). Без `sleep` и опроса.
//!
//! Создание сессий идёт под мьютексом заморозки ([`compat::Gate`]): проверка совместимости
//! обновления (задача 7) замораживает создание, и запрос под заморозкой получает временный отказ.
//!
//! Обновление (задача 8). `activate` после `staged` этого процесса пишет `intent` (срок health,
//! аргументы режима, снимок живых сессий, ключ GUI N), запускает копию помощника и выходит. N+1,
//! запущенный помощником с `--update-attempt`, до первой попытки взять блокировку сообщает о себе
//! ([`update::report_started`]). Взяв блокировку при `launched` своей попытки с ключом владельца,
//! он готовит `runtime/<N+1>/`, публикует сокет неготовым, выполняет health ([`health`]),
//! проверяет срок под блокировкой непосредственно перед записью `committed`, убирает прежний N
//! (`cleaning`, `idle`) и только тогда становится готовым. Процесс, которому восстановление
//! вернуло `swapped`, выполняет `swapped → launched` как помощник ([`update::launch_next`]) и
//! передаёт аргументы владельцу.
//!
//! Протокол управляющего сокета — одна строка JSON запроса и одна строка ответа на соединение.
//! Клиент пробы при ошибке `connect` сокет не удаляет: `ENOENT` и `ECONNREFUSED` — явный
//! временный отказ «владелец запускается или перезапускается» с признаком повтора. «Недоступен»
//! (без повтора) — только если файла блокировки нет вовсе: блокировку клиент не трогает, иначе
//! он мешал бы её взятию.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::attach::{self, Connected};
use crate::barrier::{self, Point};
use crate::compat::{self, Gate, Install, Staging};
use crate::generation::{self, APP_BIN, BUNDLE_ID, Manifest, StubMode};
use crate::health;
use crate::host::{self, SessionRecord};
use crate::journal::{self, Journal, SessionKeys, State};
use crate::keychain;
use crate::observer::{self, ProcessKey};
use crate::proto::{ATTACH_VERSIONS, Hello, Purpose};
use crate::pty::Winsize;
use crate::recover::{self, Actor, Checker, Next, Seen};
use crate::runtime::{self, Prepared};
use crate::tcc;
use crate::update::{self, Launched};
use crate::{EXIT_USAGE, kq, launch, say, spawn, sys, window};

pub const LOCK_FILE: &str = "shellio.lock";
pub const SOCKET_FILE: &str = "shellio.sock";
/// Сколько второй запуск ждёт публикации сокета и квитанции владельца за один круг.
pub const SOCKET_WAIT: Duration = Duration::from_secs(10);
/// Срок ответа владельца сверх ожидания на его стороне.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Код выхода второго запуска при временном отказе (`EX_TEMPFAIL`).
pub const EXIT_TEMPFAIL: u8 = 75;
pub const EXIT_ERROR: u8 = 1;
/// Сколько раз один запуск берёт блокировку: в начале и после исчезновения владельца.
const MAX_LOCK_ROUNDS: usize = 4;
const MAX_LINE: u64 = 1 << 20;
const OPEN: &str = "/usr/bin/open";
pub const SESSION_SIZE: Winsize = Winsize {
    rows: 24,
    cols: 80,
    xpixel: 0,
    ypixel: 0,
};
/// Срок рукопожатия `probe` при сверке реестра.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub fn lock_path(state: &Path) -> PathBuf {
    state.join(LOCK_FILE)
}

pub fn socket_path(state: &Path) -> PathBuf {
    state.join(SOCKET_FILE)
}

/// Начало временного имени, под которым владелец делает `bind` до `rename` на путь сокета; оно же
/// остаётся адресом слушающего сокета в ядре.
pub const BIND_PREFIX: &str = ".sock.";

pub fn bind_path(state: &Path, pid: u32) -> PathBuf {
    state.join(format!("{BIND_PREFIX}{pid}"))
}

// --- аргументы ---

/// Аргументы роли GUI. Установленный путь и закреплённый лист задаются явно: путь исполняемого
/// файла старого N после обмена ведёт в каталог попытки, а designated requirement бандла пишет
/// сам подписант.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppOptions {
    pub state: PathBuf,
    pub installed: PathBuf,
    /// Закреплённый SHA-1 листового сертификата, строчными.
    pub leaf: String,
    /// Идентификатор бандла и основа идентификаторов частей.
    pub identifier: String,
    /// Метка запуска (nonce): по ней внешний сторож находит процессы стенда.
    pub tag: String,
    pub update_attempt: Option<String>,
    pub window: bool,
    /// Вариант клетки E: хосты запускаются с `responsibility_spawnattrs_setdisclaim`.
    pub disclaim: bool,
    /// Клетка E: файл-фикстура, который роль GUI читает при старте (`<state>/tcc/gui-<pid>.json`).
    pub tcc_fixture: Option<PathBuf>,
    /// Аргументы, которые получает владелец: свои у владельца, переданные у второго запуска.
    pub forward: Vec<String>,
}

impl AppOptions {
    /// Аргументы режима для запуска установленного бандла: без номера попытки и переданных.
    pub fn relaunch_args(&self) -> Vec<String> {
        let mut args = vec![
            "--state".to_owned(),
            self.state.display().to_string(),
            "--installed".to_owned(),
            self.installed.display().to_string(),
            "--leaf".to_owned(),
            self.leaf.clone(),
            "--identifier".to_owned(),
            self.identifier.clone(),
            "--tag".to_owned(),
            self.tag.clone(),
        ];
        if self.window {
            args.push("--window".to_owned());
        }
        if self.disclaim {
            args.push("--disclaim".to_owned());
        }
        if let Some(fixture) = &self.tcc_fixture {
            args.extend(["--tcc-fixture".to_owned(), fixture.display().to_string()]);
        }
        args
    }

    pub fn to_args(&self) -> Vec<String> {
        let mut args = self.relaunch_args();
        if let Some(attempt) = &self.update_attempt {
            args.extend(["--update-attempt".to_owned(), attempt.clone()]);
        }
        if !self.forward.is_empty() {
            args.push("--".to_owned());
            args.extend(self.forward.iter().cloned());
        }
        args
    }

    fn requirement(&self) -> String {
        generation::requirement(&self.identifier, &self.leaf)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppMode {
    /// `--manifest` и `--report` задачи 4.
    Stub(StubMode),
    /// Разовая операция keychain клетки F (задача 14).
    Keychain(keychain::OpArgs),
    /// Разовое чтение фикстуры клетки E (задача 16).
    Tcc(tcc::OpArgs),
    Run(AppOptions),
}

pub const USAGE: &str = "\
использование: probe-update-app --installed <бандл> --leaf <SHA-1> --tag <метка>
       [--state <каталог>] [--identifier <id>] [--update-attempt <номер>] [--window]
       [--disclaim] [--tcc-fixture <файл>] [-- <аргументы владельцу>…]
       probe-update-app --manifest | --report
       probe-update-app --keychain … (операция клетки F, см. ниже)
каталог состояния — --state или SHELLIO_STATE_DIR, по умолчанию его нет";

fn absolute(flag: &str, value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!("{flag} {value}: нужен абсолютный путь"))
    }
}

/// Разбор аргументов; `env_state` — `SHELLIO_STATE_DIR`, если `--state` не задан.
pub fn parse_app_args(args: &[String], env_state: Option<&str>) -> Result<AppMode, String> {
    if args.first().map(String::as_str) == Some("--keychain") {
        return keychain::OpArgs::parse(args).map(AppMode::Keychain);
    }
    if args.first().map(String::as_str) == Some("--tcc") {
        return tcc::OpArgs::parse(args).map(AppMode::Tcc);
    }
    if args
        .iter()
        .any(|arg| arg == "--manifest" || arg == "--report")
    {
        return generation::parse_stub_args(APP_BIN, args).map(AppMode::Stub);
    }
    let (mut state, mut installed, mut leaf, mut identifier, mut tag, mut attempt) =
        (None, None, None, None, None, None);
    let mut fixture = None;
    let (mut window, mut disclaim) = (false, false);
    let mut forward = Vec::new();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        if flag == "--" {
            forward = rest.cloned().collect();
            break;
        }
        if flag == "--window" {
            window = true;
            continue;
        }
        if flag == "--disclaim" {
            disclaim = true;
            continue;
        }
        let value = rest.next().ok_or_else(|| format!("{flag}: нет значения"))?;
        let slot = match flag.as_str() {
            "--tcc-fixture" => &mut fixture,
            "--state" => &mut state,
            "--installed" => &mut installed,
            "--leaf" => &mut leaf,
            "--identifier" => &mut identifier,
            "--tag" => &mut tag,
            "--update-attempt" => &mut attempt,
            other => return Err(format!("неизвестный аргумент «{other}»")),
        };
        if slot.replace(value.clone()).is_some() {
            return Err(format!("{flag} задан дважды"));
        }
    }
    let state = match state.as_deref().or(env_state.filter(|dir| !dir.is_empty())) {
        Some(dir) => absolute("--state", dir)?,
        None => {
            return Err(format!(
                "нет каталога состояния: --state или {}",
                launch::STATE_DIR
            ));
        }
    };
    let installed = absolute("--installed", &installed.ok_or("нет --installed")?)?;
    if installed.extension().and_then(|ext| ext.to_str()) != Some("app") {
        return Err(format!(
            "--installed {}: не бандл .app",
            installed.display()
        ));
    }
    let leaf = leaf.ok_or("нет --leaf")?.to_ascii_lowercase();
    if leaf.len() != 40 || !leaf.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("--leaf {leaf}: не 40 шестнадцатеричных знаков"));
    }
    let tag = tag.ok_or("нет --tag")?;
    if tag.is_empty() {
        return Err("--tag пуст".to_owned());
    }
    if let Some(attempt) = &attempt
        && !journal::is_attempt(attempt)
    {
        return Err(format!("--update-attempt {attempt}: не номер попытки"));
    }
    Ok(AppMode::Run(AppOptions {
        state,
        installed,
        leaf,
        identifier: identifier.unwrap_or_else(|| BUNDLE_ID.to_owned()),
        tag,
        update_attempt: attempt,
        window,
        disclaim,
        tcc_fixture: fixture
            .map(|fixture| absolute("--tcc-fixture", &fixture))
            .transpose()?,
        forward,
    }))
}

// --- блокировка ---

/// Взятый `flock` на `shellio.lock`. Файл открыт с `O_CLOEXEC` и никогда не удаляется;
/// закрытие дескриптора отпускает блокировку.
pub struct Lock {
    _file: File,
    pub dev: u64,
    pub ino: u64,
}

impl Lock {
    fn open(path: &Path) -> io::Result<File> {
        File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)
    }

    fn take(path: &Path, operation: libc::c_int) -> io::Result<Option<Lock>> {
        let file = Lock::open(path)?;
        loop {
            // SAFETY: дескриптор открыт этой функцией.
            if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
                break;
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => {}
                Some(libc::EWOULDBLOCK) => return Ok(None),
                _ => return Err(err),
            }
        }
        let meta = file.metadata()?;
        Ok(Some(Lock {
            dev: meta.dev(),
            ino: meta.ino(),
            _file: file,
        }))
    }

    /// Неблокирующее взятие; `None` — блокировку держит другой.
    pub fn try_acquire(path: &Path) -> io::Result<Option<Lock>> {
        Lock::take(path, libc::LOCK_EX | libc::LOCK_NB)
    }

    /// Взятие с ожиданием освобождения.
    pub fn acquire(path: &Path) -> io::Result<Lock> {
        Lock::take(path, libc::LOCK_EX)?
            .ok_or_else(|| io::Error::other("блокирующий flock вернул EWOULDBLOCK"))
    }
}

// --- протокол ---

/// Пара устройства и inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inode {
    pub dev: u64,
    pub ino: u64,
}

impl Inode {
    pub fn of(path: &Path) -> io::Result<Inode> {
        let meta = fs::symlink_metadata(path)?;
        Ok(Inode {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "kebab-case")]
pub enum Request {
    /// Состояние владельца; `wait_ms` — сколько владелец ждёт своей готовности перед ответом.
    Ping {
        wait_ms: u64,
    },
    /// Аргументы второго запуска.
    Forward {
        args: Vec<String>,
        from: ProcessKey,
        wait_ms: u64,
    },
    CreateSession {
        name: String,
    },
    Sessions,
    /// Начать попытку обновления кандидатом — бандлом по абсолютному пути вне установки.
    Update {
        candidate: String,
    },
    /// Повторить отложенную попытку из журнала: `staged-deferred → staged`.
    Retry,
    /// Активировать попытку, поставленную этим процессом: `staged → intent`, запуск помощника и
    /// выход.
    Activate,
    /// Квитанция окна `--window` (задача 9): текст, прочитанный обратно из текстового вида,
    /// когда выполнено `wait`, но не позже `wait_ms`.
    WindowReceipt {
        wait: window::Wait,
        wait_ms: u64,
    },
    /// Владелец убирает свой сокет и выходит; сессии живут.
    Quit,
}

/// Принятые владельцем аргументы.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forwarded {
    pub seq: u64,
    pub from: ProcessKey,
    pub args: Vec<String>,
}

/// Что владелец знает о себе.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerInfo {
    pub generation: u32,
    pub key: ProcessKey,
    /// `proc_pidpath` владельца.
    pub executable: String,
    /// CDHash процесса из ядра.
    pub cdhash: String,
    pub installed: String,
    /// CDHash установленного бандла, прошедшего проверку подписи.
    pub installed_cdhash: String,
    pub socket: String,
    pub socket_inode: Inode,
    pub lock_inode: Inode,
    /// Оставшийся путь сокета прежнего владельца, удалённый перед публикацией.
    pub stale_socket: Option<Inode>,
    pub runtime: Prepared,
    /// Шаги восстановления при взятии блокировки.
    pub recovery: Vec<String>,
    pub window: bool,
    pub ready: bool,
    pub received: Vec<Forwarded>,
    /// Подтверждение обновления этим процессом (N+1).
    #[serde(default)]
    pub update: Option<Confirmed>,
}

/// Подтверждение обновления N+1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirmed {
    pub attempt: String,
    pub deadline_ms: u64,
    pub health: health::Facts,
    /// Время записи `committed`, мс от эпохи Unix.
    pub committed_ms: u64,
    /// Шаги уборки после `committed`.
    pub cleanup: Vec<String>,
}

/// Как сессия ответила на `probe`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "probe", rename_all = "kebab-case")]
pub enum Probe {
    Welcome { version: u16 },
    Refused { reason: String },
    Unreachable { reason: String },
}

/// Сессия реестра: запись хоста и её проверка сейчас.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub record: SessionRecord,
    pub host_alive: bool,
    pub workload_alive: bool,
    pub probe: Probe,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "kebab-case")]
pub enum Reply {
    Pong(Box<OwnerInfo>),
    Receipt {
        seq: u64,
        owner: ProcessKey,
    },
    Session(Box<SessionRecord>),
    Sessions {
        entries: Vec<Entry>,
        /// Записи, которые не разобрались.
        problems: Vec<String>,
        /// Кандидаты на уборку `runtime/` по живым хостам.
        cleanup: Vec<u32>,
    },
    /// Итог постановки или повторной проверки обновления.
    Staging(Box<Staging>),
    /// `intent` записан, помощник запущен, GUI N выходит.
    Activating {
        attempt: String,
        helper: ProcessKey,
    },
    Window(Box<window::Receipt>),
    Done,
    Refused {
        reason: String,
        retry: bool,
    },
}

fn write_json_line<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

/// Строка JSON; `None` — соединение закрыто до строки.
fn read_json_line<T: DeserializeOwned>(reader: impl Read) -> io::Result<Option<T>> {
    let mut line = String::new();
    let read = BufReader::new(reader.take(MAX_LINE)).read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    serde_json::from_str(line.trim())
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

// --- клиент пробы ---

/// Явный отказ, когда ответа владельца нет.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unavailable {
    /// Сокета нет или его никто не слушает при существующем файле блокировки.
    Restarting,
    /// Файла блокировки нет: приложение с этим каталогом состояния не запускалось.
    Down,
    /// Соединение есть, ответа в срок нет.
    NotResponding,
}

impl Unavailable {
    pub fn reason(self) -> &'static str {
        match self {
            Unavailable::Restarting => "владелец запускается или перезапускается",
            Unavailable::Down => "недоступен: приложение с этим каталогом состояния не запускалось",
            Unavailable::NotResponding => "владелец не отвечает",
        }
    }

    pub fn retry(self) -> bool {
        match self {
            Unavailable::Restarting | Unavailable::NotResponding => true,
            Unavailable::Down => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Reply(Reply),
    Unavailable(Unavailable),
}

/// Ошибка `connect` к сокету владельца: временный отказ или ошибка клиента. Сокет при этом не
/// удаляется.
pub fn classify_connect(err: &io::Error, lock_exists: bool) -> Result<Unavailable, String> {
    match err.raw_os_error() {
        Some(libc::ENOENT) if lock_exists => Ok(Unavailable::Restarting),
        Some(libc::ENOENT) => Ok(Unavailable::Down),
        Some(libc::ECONNREFUSED) => Ok(Unavailable::Restarting),
        _ => Err(format!("connect: {err}")),
    }
}

/// Запрос к владельцу каталога состояния `state` с ответом не позже `timeout`.
pub fn request(state: &Path, request: &Request, timeout: Duration) -> Result<Answer, String> {
    let socket = socket_path(state);
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(err) => {
            return classify_connect(&err, lock_path(state).exists())
                .map(Answer::Unavailable)
                .map_err(|reason| format!("{}: {reason}", socket.display()));
        }
    };
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|err| format!("сроки сокета: {err}"))?;
    if let Err(err) = write_json_line(&mut stream, request) {
        return match err.kind() {
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset => {
                Ok(Answer::Unavailable(Unavailable::Restarting))
            }
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                Ok(Answer::Unavailable(Unavailable::NotResponding))
            }
            _ => Err(format!("запрос владельцу: {err}")),
        };
    }
    match read_json_line::<Reply>(&mut stream) {
        Ok(Some(reply)) => Ok(Answer::Reply(reply)),
        Ok(None) => Ok(Answer::Unavailable(Unavailable::Restarting)),
        Err(err) => match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                Ok(Answer::Unavailable(Unavailable::NotResponding))
            }
            io::ErrorKind::ConnectionReset => Ok(Answer::Unavailable(Unavailable::Restarting)),
            _ => Err(format!("ответ владельца: {err}")),
        },
    }
}

/// Ждёт, пока владелец опубликует сокет и будет готов, не позже `deadline`. Для сценариев.
pub fn wait_for_owner(state: &Path, deadline: Instant) -> Result<OwnerInfo, String> {
    let watch = kq::Queue::on_dir(state)?;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ping = Request::Ping {
            wait_ms: left.as_millis() as u64,
        };
        match request(state, &ping, left + REQUEST_TIMEOUT)? {
            Answer::Reply(Reply::Pong(info)) if info.ready => return Ok(*info),
            Answer::Reply(other) => return Err(format!("владелец не готов к сроку: {other:?}")),
            Answer::Unavailable(Unavailable::NotResponding) => {
                return Err(Unavailable::NotResponding.reason().to_owned());
            }
            Answer::Unavailable(_) => {}
        }
        if watch.wait(deadline)? == kq::Event::TimedOut {
            return Err(format!(
                "владелец не опубликовал сокет {} к сроку",
                socket_path(state).display()
            ));
        }
    }
}

// --- право владения ---

/// Факты взявшего блокировку после восстановления.
#[derive(Clone, Debug)]
pub struct Facts<'a> {
    /// CDHash процесса из ядра.
    pub own_cdhash: Option<&'a str>,
    /// CDHash установленного бандла, прошедшего проверку; `None` — не прошёл или его нет.
    pub installed_cdhash: Option<&'a str>,
    pub journal: Option<&'a Journal>,
    pub next: Next,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "kebab-case")]
pub enum Role {
    Owner,
    /// Без права: отпустить блокировку и передать аргументы; `launch` — владельца нет, запустить
    /// установленный бандл.
    Yield {
        launch: bool,
        reason: String,
    },
    /// `swapped` без помощника: `swapped → launched` как помощник (задача 8).
    Helper {
        attempt: String,
    },
}

fn mismatch(facts: &Facts) -> String {
    match (facts.own_cdhash, facts.installed_cdhash) {
        (None, _) => "CDHash процесса не прочитан".to_owned(),
        (_, None) => "установленный бандл не прошёл проверку подписи".to_owned(),
        (Some(own), Some(installed)) => {
            format!("CDHash процесса {own} не равен установленному {installed}")
        }
    }
}

/// Решение о праве владения (раздел модуля).
pub fn decide_role(facts: &Facts) -> Role {
    let same_code = matches!(
        (facts.own_cdhash, facts.installed_cdhash),
        (Some(own), Some(installed)) if own == installed
    );
    match facts.next {
        Next::LaunchAsHelper => match facts.journal {
            Some(journal) => Role::Helper {
                attempt: journal.attempt.clone(),
            },
            None => Role::Yield {
                launch: false,
                reason: "восстановление вернуло swapped без журнала".to_owned(),
            },
        },
        Next::Yield => Role::Yield {
            launch: false,
            reason: "попытка в launched: владелец с ключом жив, срок не истёк".to_owned(),
        },
        Next::Owner if same_code => Role::Owner,
        Next::Owner => Role::Yield {
            launch: false,
            reason: format!("ключ владельца совпал, но {}", mismatch(facts)),
        },
        Next::Nothing | Next::LaunchInstalled => {
            let state = facts.journal.map_or(State::Idle, |journal| journal.state);
            if !matches!(state, State::Idle | State::StagedDeferred) {
                Role::Yield {
                    launch: false,
                    reason: format!("после восстановления журнал в {}", state.name()),
                }
            } else if same_code {
                Role::Owner
            } else {
                Role::Yield {
                    launch: true,
                    reason: mismatch(facts),
                }
            }
        }
    }
}

// --- вход роли GUI ---

/// Строка итога процесса роли GUI в stdout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "kebab-case")]
pub enum AppReport {
    /// Владелец опубликовал сокет.
    Owner(Box<OwnerInfo>),
    /// Владелец вышел по `Quit`.
    Stopped { key: ProcessKey },
    /// GUI N записал `intent`, запустил помощника и вышел.
    Updated {
        key: ProcessKey,
        attempt: String,
        helper: ProcessKey,
    },
    /// N+1 не подтвердился и вышел без `committed`.
    Unconfirmed {
        key: ProcessKey,
        attempt: String,
        reason: String,
        health: Option<Box<health::Facts>>,
    },
    Forwarded {
        key: ProcessKey,
        receipt: u64,
        owner: ProcessKey,
        /// Почему процесс, взявший блокировку, отказался от владения.
        yielded: Option<String>,
        launched_installed: bool,
    },
    Refused {
        key: ProcessKey,
        reason: String,
        retry: bool,
        yielded: Option<String>,
    },
    Error {
        key: Option<ProcessKey>,
        reason: String,
    },
}

impl AppReport {
    fn code(&self) -> u8 {
        match self {
            AppReport::Owner(_)
            | AppReport::Stopped { .. }
            | AppReport::Updated { .. }
            | AppReport::Forwarded { .. } => 0,
            AppReport::Unconfirmed { .. } => EXIT_ERROR,
            AppReport::Refused { .. } => EXIT_TEMPFAIL,
            AppReport::Error { .. } => EXIT_ERROR,
        }
    }

    fn exit_code(&self) -> ExitCode {
        ExitCode::from(self.code())
    }
}

fn print_report(report: &AppReport) {
    let line =
        serde_json::to_string(report).unwrap_or_else(|err| format!("{{\"error\":\"{err}\"}}"));
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// Вход `probe-update-app`.
pub fn app_main(args: &[String]) -> ExitCode {
    let env_state = std::env::var(launch::STATE_DIR).ok();
    match parse_app_args(args, env_state.as_deref()) {
        Err(reason) => {
            say(&format!(
                "{APP_BIN}: {reason}\n\n{USAGE}\n{}",
                keychain::USAGE
            ));
            ExitCode::from(EXIT_USAGE)
        }
        Ok(AppMode::Stub(_)) => generation::stub_main(APP_BIN, args),
        Ok(AppMode::Keychain(op)) => keychain_main(op),
        Ok(AppMode::Tcc(op)) => tcc_main(op),
        Ok(AppMode::Run(options)) => {
            barrier::set_role(if options.update_attempt.is_some() {
                barrier::Role::Next
            } else {
                barrier::Role::Gui
            });
            if options.window {
                // Главный поток держит AppKit; роль GUI завершает процесс сама.
                let started = window::run_main(crate::GENERATION, move || {
                    let report = run(&options);
                    print_report(&report);
                    std::process::exit(i32::from(report.code()));
                });
                let report = AppReport::Error {
                    key: None,
                    reason: match started {
                        Ok(()) => "цикл окна вернулся".to_owned(),
                        Err(reason) => reason,
                    },
                };
                print_report(&report);
                return report.exit_code();
            }
            let report = run(&options);
            print_report(&report);
            report.exit_code()
        }
    }
}

/// Разовая операция keychain клетки F: итог в `--keychain-out`. С `--window` операция идёт в
/// процессе с окном AppKit, как у GUI N, и завершает процесс сама.
fn keychain_main(op: keychain::OpArgs) -> ExitCode {
    fn finish(op: &keychain::OpArgs) -> u8 {
        let report = keychain::perform(op);
        match keychain::write_report(&op.out, &report) {
            Ok(()) => 0,
            Err(reason) => {
                say(&format!("{APP_BIN}: --keychain: {reason}"));
                EXIT_ERROR
            }
        }
    }
    if op.window {
        let started = window::run_main(crate::GENERATION, move || {
            std::process::exit(i32::from(finish(&op)));
        });
        say(&format!(
            "{APP_BIN}: --keychain --window: {}",
            started
                .err()
                .unwrap_or_else(|| "цикл окна вернулся".to_owned())
        ));
        return ExitCode::from(EXIT_ERROR);
    }
    ExitCode::from(finish(&op))
}

fn tcc_main(op: tcc::OpArgs) -> ExitCode {
    fn finish(op: &tcc::OpArgs) -> u8 {
        match tcc::write_report(&op.out, &tcc::perform(op)) {
            Ok(()) => 0,
            Err(reason) => {
                say(&format!("{APP_BIN}: --tcc: {reason}"));
                EXIT_ERROR
            }
        }
    }
    if op.window {
        let started = window::run_main(crate::GENERATION, move || {
            std::process::exit(i32::from(finish(&op)));
        });
        say(&format!(
            "{APP_BIN}: --tcc --window: {}",
            started
                .err()
                .unwrap_or_else(|| "цикл окна вернулся".to_owned())
        ));
        return ExitCode::from(EXIT_ERROR);
    }
    ExitCode::from(finish(&op))
}

/// Каталог отчётов чтения роли GUI в каталоге состояния.
pub const TCC_DIR: &str = "tcc";

/// Чтение фикстуры ролью GUI при старте: `<state>/tcc/gui-<pid>.json`. Ошибка записи отчёта не
/// роняет роль: её увидит сценарий по отсутствию отчёта.
fn tcc_gui_read(options: &AppOptions) {
    let Some(fixture) = &options.tcc_fixture else {
        return;
    };
    let dir = options.state.join(TCC_DIR);
    let out = dir.join(format!("gui-{}.json", std::process::id()));
    let op = tcc::OpArgs {
        op: tcc::Op::Read,
        fixture: fixture.clone(),
        out: out.clone(),
        window: options.window,
    };
    let written = fs::create_dir_all(&dir)
        .map_err(|err| format!("{}: {err}", dir.display()))
        .and_then(|()| tcc::write_report(&out, &tcc::perform(&op)));
    if let Err(reason) = written {
        say(&format!("{APP_BIN}: --tcc-fixture: {reason}"));
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Чем закончилось владение блокировкой.
enum Held {
    Served(Served),
    Yielded { reason: String, launch: bool },
}

/// Чем закончилась работа владельцем.
enum Served {
    Stopped,
    Handed {
        attempt: String,
        helper: ProcessKey,
    },
    Unconfirmed {
        attempt: String,
        reason: String,
        health: Option<Box<health::Facts>>,
    },
}

enum Waited {
    Forwarded { seq: u64, owner: ProcessKey },
    Lock(Lock),
    Refused { reason: String, retry: bool },
}

fn run(options: &AppOptions) -> AppReport {
    tcc_gui_read(options);
    let me = match observer::current() {
        Ok(info) => info.key,
        Err(reason) => return AppReport::Error { key: None, reason },
    };
    let error = |reason: String| AppReport::Error {
        key: Some(me),
        reason,
    };
    let own_cdhash = match sys::process_cdhash(me.pid) {
        Ok(hash) => Some(hash),
        Err(err) => {
            say(&format!("{APP_BIN}: csops CS_OPS_CDHASH: {err}"));
            None
        }
    };
    barrier::note(
        &options.state,
        "start",
        json!({
            "forward": options.forward,
            "attempt": options.update_attempt,
            "generation": crate::GENERATION,
            "cdhash": own_cdhash,
        }),
    );
    if let Some(attempt) = &options.update_attempt {
        barrier::reach(&options.state, Point::Launching);
        if let Err(reason) = update::report_started(&options.state, attempt, me) {
            return error(reason);
        }
        barrier::reach(&options.state, Point::Started);
    }
    let lock_file = lock_path(&options.state);
    let mut may_lock = true;
    let mut yielded = None;
    let mut launched_installed = false;
    let mut held = None;
    for _ in 0..MAX_LOCK_ROUNDS {
        if held.is_none() && may_lock {
            match Lock::try_acquire(&lock_file) {
                Ok(lock) => held = lock,
                Err(err) => return error(format!("{}: {err}", lock_file.display())),
            }
            barrier::note(&options.state, "lock", json!({ "taken": held.is_some() }));
        }
        if let Some(lock) = held.take() {
            match with_lock(options, me, own_cdhash.as_deref(), lock) {
                Ok(Held::Served(Served::Stopped)) => return AppReport::Stopped { key: me },
                Ok(Held::Served(Served::Handed { attempt, helper })) => {
                    return AppReport::Updated {
                        key: me,
                        attempt,
                        helper,
                    };
                }
                Ok(Held::Served(Served::Unconfirmed {
                    attempt,
                    reason,
                    health,
                })) => {
                    return AppReport::Unconfirmed {
                        key: me,
                        attempt,
                        reason,
                        health,
                    };
                }
                Ok(Held::Yielded { reason, launch }) => {
                    say(&format!("{APP_BIN}: без права владения: {reason}"));
                    may_lock = false;
                    yielded = Some(reason);
                    if launch && !launched_installed {
                        if let Err(reason) = launch_installed(options) {
                            return error(reason);
                        }
                        launched_installed = true;
                    }
                }
                Err(reason) => return error(reason),
            }
        }
        match wait_and_forward(options, me, may_lock, Instant::now() + SOCKET_WAIT) {
            Ok(Waited::Forwarded { seq, owner }) => {
                return AppReport::Forwarded {
                    key: me,
                    receipt: seq,
                    owner,
                    yielded,
                    launched_installed,
                };
            }
            Ok(Waited::Lock(lock)) => held = Some(lock),
            Ok(Waited::Refused { reason, retry }) => {
                return AppReport::Refused {
                    key: me,
                    reason,
                    retry,
                    yielded,
                };
            }
            Err(reason) => return error(reason),
        }
    }
    error(format!(
        "блокировка освобождалась {MAX_LOCK_ROUNDS} раз, владелец так и не определился"
    ))
}

/// Под блокировкой: восстановление, решение о праве и, при праве, работа владельцем.
fn with_lock(
    options: &AppOptions,
    me: ProcessKey,
    own_cdhash: Option<&str>,
    lock: Lock,
) -> Result<Held, String> {
    let store = journal::Store::new(&options.state);
    let checker = recover::Codesign {
        requirement: options.requirement(),
    };
    let before = store.read().map_err(|err| err.to_string())?;
    let launched_for = match (&options.update_attempt, &before, own_cdhash) {
        (Some(attempt), Some(journal), Some(own))
            if &journal.attempt == attempt && journal.cdhash_to == own =>
        {
            Some(attempt.clone())
        }
        _ => None,
    };
    let alive = |key: &ProcessKey| observer::identify(*key).is_ok_and(|found| found.is_alive());
    let ctx = recover::Context {
        now_ms: now_ms(),
        actor: Actor {
            key: me,
            launched_for,
        },
        alive: &alive,
    };
    let report = recover::recover(&store, &checker, &ctx).map_err(|err| err.to_string())?;
    let after = store.read().map_err(|err| err.to_string())?;
    let installed = match checker.check(&options.installed)? {
        Seen::Valid { cdhash } => Some(cdhash),
        Seen::Absent | Seen::Invalid { .. } => None,
    };
    let role = decide_role(&Facts {
        own_cdhash,
        installed_cdhash: installed.as_deref(),
        journal: after.as_ref(),
        next: report.next,
    });
    let recovery: Vec<String> = report
        .steps
        .iter()
        .map(|step| format!("{step:?}"))
        .collect();
    barrier::note(
        &options.state,
        "role",
        json!({ "role": role, "recovery": recovery }),
    );
    match role {
        Role::Owner => {
            let (Some(own), Some(installed)) = (own_cdhash, installed) else {
                return Err("право владения без CDHash".to_owned());
            };
            let pending = after.filter(|journal| {
                journal.state == State::Launched
                    && journal.owner == Some(me)
                    && options.update_attempt.as_deref() == Some(journal.attempt.as_str())
            });
            serve(options, me, own, &installed, lock, recovery, pending).map(Held::Served)
        }
        Role::Yield { launch, reason } => {
            drop(lock);
            Ok(Held::Yielded { reason, launch })
        }
        Role::Helper { attempt } => {
            let Some(journal) = after
                .filter(|journal| journal.attempt == attempt && journal.state == State::Swapped)
            else {
                drop(lock);
                return Err(format!(
                    "попытка {attempt}: восстановление вернуло swapped, журнал другой"
                ));
            };
            let launched = update::launch_next(&store, &journal, me);
            drop(lock);
            match launched? {
                Launched::Owner(key) => Ok(Held::Yielded {
                    reason: format!(
                        "swapped → launched выполнен как помощник, владелец {}",
                        key.pid
                    ),
                    launch: false,
                }),
                Launched::RolledBack => Ok(Held::Yielded {
                    reason: "N+1 не появился за срок запуска: попытка откачена".to_owned(),
                    launch: true,
                }),
            }
        }
    }
}

/// Запуск установленного бандла через LaunchServices: каталог состояния — через `--env` и
/// `--args`, окружение `open` не передаёт.
fn launch_installed(options: &AppOptions) -> Result<(), String> {
    let mut args = vec![
        "--env".to_owned(),
        format!("{}={}", launch::STATE_DIR, options.state.display()),
        options.installed.display().to_string(),
        "--args".to_owned(),
    ];
    args.extend(options.relaunch_args());
    let output = spawn::output(Path::new(OPEN), &args).map_err(|err| format!("open: {err}"))?;
    if output.exit.success() {
        Ok(())
    } else {
        Err(format!(
            "open {}: {}; {}",
            options.installed.display(),
            output.exit,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Ожидание сокета и передача аргументов; `watch_lock` — ещё и повтор взятия блокировки, если
/// владелец исчезнет.
fn wait_and_forward(
    options: &AppOptions,
    me: ProcessKey,
    watch_lock: bool,
    deadline: Instant,
) -> Result<Waited, String> {
    let watch = kq::Queue::on_dir(&options.state)?;
    let (sender, receiver) = mpsc::channel();
    if watch_lock {
        let trigger = watch.trigger();
        let path = lock_path(&options.state);
        thread::Builder::new()
            .name("lock-wait".to_owned())
            .spawn(move || {
                let taken = Lock::acquire(&path);
                let _ = sender.send(taken);
                trigger.fire();
            })
            .map_err(|err| format!("поток ожидания блокировки: {err}"))?;
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let forward = Request::Forward {
            args: options.forward.clone(),
            from: me,
            wait_ms: left.as_millis() as u64,
        };
        let answer = request(&options.state, &forward, left + REQUEST_TIMEOUT)?;
        barrier::note(&options.state, "answer", answer_note(&answer));
        match answer {
            Answer::Reply(Reply::Receipt { seq, owner }) => {
                return Ok(Waited::Forwarded { seq, owner });
            }
            Answer::Reply(Reply::Refused { reason, retry }) => {
                return Ok(Waited::Refused { reason, retry });
            }
            Answer::Reply(other) => return Err(format!("на Forward пришло {other:?}")),
            Answer::Unavailable(Unavailable::NotResponding) => {
                return Ok(Waited::Refused {
                    reason: Unavailable::NotResponding.reason().to_owned(),
                    retry: true,
                });
            }
            Answer::Unavailable(_) => {}
        }
        match watch.wait(deadline)? {
            kq::Event::Changed | kq::Event::Exited(_) => {}
            kq::Event::Fired => match receiver.try_recv() {
                Ok(Ok(lock)) => return Ok(Waited::Lock(lock)),
                Ok(Err(err)) => return Err(format!("ожидание блокировки: {err}")),
                Err(_) => {}
            },
            kq::Event::TimedOut => {
                return Ok(Waited::Refused {
                    reason: format!(
                        "{}: сокет не опубликован за {} с",
                        Unavailable::NotResponding.reason(),
                        SOCKET_WAIT.as_secs()
                    ),
                    retry: true,
                });
            }
        }
    }
}

/// Ответ на `forward` для отметки стенда.
fn answer_note(answer: &Answer) -> serde_json::Value {
    match answer {
        Answer::Reply(Reply::Receipt { seq, owner }) => {
            json!({ "answer": "receipt", "seq": seq, "owner": owner })
        }
        Answer::Reply(Reply::Refused { reason, retry }) => {
            json!({ "answer": "refused", "reason": reason, "retry": retry })
        }
        Answer::Reply(other) => json!({ "answer": "other", "reply": format!("{other:?}") }),
        Answer::Unavailable(why) => json!({ "answer": "unavailable", "unavailable": why }),
    }
}

// --- владелец ---

/// Опубликованный сокет владельца: убирается только свой узел.
struct Published {
    path: PathBuf,
    inode: Inode,
}

impl Published {
    fn unpublish(&self) {
        if Inode::of(&self.path).is_ok_and(|inode| inode == self.inode) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Окно владельца показывает первую по имени живую сессию реестра: N+1 открывает ту же сессию,
/// что была у N.
fn show_first_session(state: &Path) {
    if !window::active() || window::showing() {
        return;
    }
    let (entries, _) = registry(state);
    let first = entries
        .iter()
        .filter(|entry| entry.host_alive && entry.workload_alive)
        .min_by(|a, b| a.record.session.cmp(&b.record.session));
    if let Some(entry) = first
        && let Err(reason) = window::show(&entry.record)
    {
        say(&format!("{APP_BIN}: окно: {reason}"));
    }
}

/// Удаление оставшегося пути сокета под блокировкой: прежний владелец мёртв, раз блокировка
/// свободна. Не сокет на этом пути — явная ошибка.
fn remove_stale_socket(socket: &Path) -> Result<Option<Inode>, String> {
    match fs::symlink_metadata(socket) {
        Ok(meta) if meta.file_type().is_socket() => {
            fs::remove_file(socket).map_err(|err| format!("{}: {err}", socket.display()))?;
            Ok(Some(Inode {
                dev: meta.dev(),
                ino: meta.ino(),
            }))
        }
        Ok(_) => Err(format!(
            "{}: на пути сокета не сокет, не удаляется",
            socket.display()
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{}: {err}", socket.display())),
    }
}

/// `bind` временного имени, `listen`, `rename` на путь сокета.
fn publish(state: &Path, socket: &Path) -> Result<(UnixListener, Published), String> {
    let temp = bind_path(state, std::process::id());
    host::check_socket_path(&temp)?;
    let _ = fs::remove_file(&temp);
    let listener =
        UnixListener::bind(&temp).map_err(|err| format!("bind {}: {err}", temp.display()))?;
    if let Err(err) = fs::rename(&temp, socket) {
        let _ = fs::remove_file(&temp);
        return Err(format!("{} → {}: {err}", temp.display(), socket.display()));
    }
    let inode = Inode::of(socket).map_err(|err| format!("{}: {err}", socket.display()))?;
    Ok((
        listener,
        Published {
            path: socket.to_path_buf(),
            inode,
        },
    ))
}

/// Почему N+1 не подтвердился, с фактами health, если они собраны.
type Unhealthy = (String, Option<Box<health::Facts>>);

struct ServerState {
    ready: bool,
    received: Vec<Forwarded>,
    /// Владелец уходит: `activate` начал передачу или ответ на `quit` дан. Аргументы второго
    /// запуска он больше не принимает: его квитанция была бы ложным успехом.
    closing: bool,
    quitting: bool,
    /// `activate` записал `intent` и запустил помощника.
    handing: Option<(String, ProcessKey)>,
    update: Option<Confirmed>,
}

struct Server {
    options: AppOptions,
    info: OwnerInfo,
    state: Mutex<ServerState>,
    changed: Condvar,
    /// Сессии запускаются по одной под этим мьютексом; заморозка — флаг под ним же.
    gate: Mutex<Gate>,
    /// Проверки обновления идут по одной.
    update: Mutex<()>,
}

fn serve(
    options: &AppOptions,
    me: ProcessKey,
    own_cdhash: &str,
    installed_cdhash: &str,
    lock: Lock,
    recovery: Vec<String>,
    pending: Option<Journal>,
) -> Result<Served, String> {
    let manifest = Manifest::read(&options.installed)?;
    if manifest.generation != crate::GENERATION {
        return Err(format!(
            "манифест установленного бандла — поколение {}, сборка — {}",
            manifest.generation,
            crate::GENERATION
        ));
    }
    let prepared = runtime::prepare(
        &options.state,
        crate::GENERATION,
        &options.installed,
        &runtime::Codesign {
            base: options.identifier.clone(),
            leaf: options.leaf.clone(),
        },
    )?;
    barrier::reach(&options.state, Point::BeforePublish);
    let socket = socket_path(&options.state);
    host::check_socket_path(&socket)?;
    let stale = remove_stale_socket(&socket)?;
    let (listener, published) = publish(&options.state, &socket)?;
    let executable = sys::process_path(me.pid)
        .map(|path| path.display().to_string())
        .map_err(|err| format!("proc_pidpath: {err}"))?;
    let server = Arc::new(Server {
        options: options.clone(),
        info: OwnerInfo {
            generation: crate::GENERATION,
            key: me,
            executable,
            cdhash: own_cdhash.to_owned(),
            installed: options.installed.display().to_string(),
            installed_cdhash: installed_cdhash.to_owned(),
            socket: socket.display().to_string(),
            socket_inode: published.inode,
            lock_inode: Inode {
                dev: lock.dev,
                ino: lock.ino,
            },
            stale_socket: stale,
            runtime: prepared,
            recovery,
            window: options.window,
            ready: pending.is_none(),
            received: Vec::new(),
            update: None,
        },
        state: Mutex::new(ServerState {
            ready: pending.is_none(),
            received: Vec::new(),
            closing: false,
            quitting: false,
            handing: None,
            update: None,
        }),
        changed: Condvar::new(),
        gate: Mutex::new(Gate::default()),
        update: Mutex::new(()),
    });
    let _ = server.record(me, options.forward.clone());
    if pending.is_none() {
        print_report(&AppReport::Owner(Box::new(server.snapshot())));
        show_first_session(&options.state);
    }
    let accepting = Arc::clone(&server);
    thread::Builder::new()
        .name("owner-accept".to_owned())
        .spawn(move || accepting.accept(listener))
        .map_err(|err| format!("поток приёма: {err}"))?;
    barrier::reach(&options.state, Point::Published);
    if let Some(journal) = pending {
        match server.confirm(&journal) {
            Ok(Some(confirmed)) => {
                {
                    let mut state = server.lock();
                    state.update = Some(confirmed);
                    state.ready = true;
                }
                server.changed.notify_all();
                print_report(&AppReport::Owner(Box::new(server.snapshot())));
                show_first_session(&options.state);
            }
            Ok(None) => {
                published.unpublish();
                drop(lock);
                return Ok(Served::Unconfirmed {
                    attempt: journal.attempt,
                    reason: "владелец вышел по quit до подтверждения".to_owned(),
                    health: None,
                });
            }
            Err((reason, health)) => {
                published.unpublish();
                drop(lock);
                return Ok(Served::Unconfirmed {
                    attempt: journal.attempt,
                    reason,
                    health,
                });
            }
        }
    }
    let handing = {
        let mut state = server.lock();
        while !state.quitting {
            state = server
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.handing.clone()
    };
    if handing.is_some() {
        barrier::reach(&options.state, Point::HelperSpawned);
    }
    published.unpublish();
    drop(lock);
    Ok(match handing {
        Some((attempt, helper)) => Served::Handed { attempt, helper },
        None => Served::Stopped,
    })
}

impl Server {
    fn lock(&self) -> MutexGuard<'_, ServerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Принять аргументы; `None` — владелец уходит и больше их не принимает.
    fn record(&self, from: ProcessKey, args: Vec<String>) -> Option<u64> {
        let mut state = self.lock();
        if state.closing {
            return None;
        }
        let seq = state.received.len() as u64 + 1;
        state.received.push(Forwarded { seq, from, args });
        Some(seq)
    }

    fn set_closing(&self, closing: bool) {
        self.lock().closing = closing;
    }

    fn snapshot(&self) -> OwnerInfo {
        let state = self.lock();
        let mut info = self.info.clone();
        info.ready = state.ready;
        info.received = state.received.clone();
        info.update = state.update.clone();
        info
    }

    /// Ждёт готовности не дольше `wait_ms`; готов ли владелец.
    fn wait_ready(&self, wait_ms: u64) -> bool {
        let state = self.lock();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_millis(wait_ms), |state| !state.ready)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.ready
    }

    fn accept(self: Arc<Self>, listener: UnixListener) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let server = Arc::clone(&self);
            let spawned = thread::Builder::new()
                .name("owner-conn".to_owned())
                .spawn(move || server.connection(stream));
            if let Err(err) = spawned {
                say(&format!("{APP_BIN}: поток соединения: {err}"));
            }
        }
    }

    fn connection(&self, mut stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
        let request = match read_json_line::<Request>(&mut stream) {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(err) => {
                let _ = write_json_line(
                    &mut stream,
                    &Reply::Refused {
                        reason: format!("запрос: {err}"),
                        retry: false,
                    },
                );
                return;
            }
        };
        let quit = request == Request::Quit;
        let reply = self.handle(request);
        let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
        let _ = write_json_line(&mut stream, &reply);
        if quit || matches!(reply, Reply::Activating { .. }) {
            self.lock().quitting = true;
            self.changed.notify_all();
        }
    }

    fn not_ready() -> Reply {
        Reply::Refused {
            reason: Unavailable::Restarting.reason().to_owned(),
            retry: true,
        }
    }

    fn handle(&self, request: Request) -> Reply {
        match request {
            Request::Ping { wait_ms } => {
                self.wait_ready(wait_ms);
                Reply::Pong(Box::new(self.snapshot()))
            }
            Request::Forward {
                args,
                from,
                wait_ms,
            } => {
                let state = &self.options.state;
                if !self.lock().ready {
                    barrier::note(state, "forward-held", json!({ "from": from }));
                }
                if !self.wait_ready(wait_ms) {
                    return Server::not_ready();
                }
                match self.record(from, args) {
                    Some(seq) => Reply::Receipt {
                        seq,
                        owner: self.info.key,
                    },
                    None => {
                        barrier::note(state, "forward-refused", json!({ "from": from }));
                        Server::not_ready()
                    }
                }
            }
            Request::CreateSession { name } => {
                if !self.wait_ready(0) {
                    return Server::not_ready();
                }
                let reply = self.create_session(&name);
                if let Reply::Session(record) = &reply
                    && window::active()
                    && !window::showing()
                    && let Err(reason) = window::show(record)
                {
                    say(&format!("{APP_BIN}: окно: {reason}"));
                }
                reply
            }
            Request::WindowReceipt { wait, wait_ms } => {
                if !window::active() {
                    return Reply::Refused {
                        reason: "окна нет: владелец запущен без --window".to_owned(),
                        retry: false,
                    };
                }
                match window::receipt(&wait, Instant::now() + Duration::from_millis(wait_ms)) {
                    Ok(receipt) => Reply::Window(Box::new(receipt)),
                    Err(reason) => Reply::Refused {
                        reason,
                        retry: false,
                    },
                }
            }
            Request::Update { candidate } => {
                if !self.wait_ready(0) {
                    return Server::not_ready();
                }
                let candidate = PathBuf::from(candidate);
                if !candidate.is_absolute() {
                    return Reply::Refused {
                        reason: format!("кандидат {}: нужен абсолютный путь", candidate.display()),
                        retry: false,
                    };
                }
                self.staging(|install, gate| compat::stage(install, gate, &candidate))
            }
            Request::Retry => {
                if !self.wait_ready(0) {
                    return Server::not_ready();
                }
                self.staging(compat::restage)
            }
            Request::Activate => {
                if !self.wait_ready(0) {
                    return Server::not_ready();
                }
                self.activate()
            }
            Request::Sessions => {
                let (entries, problems) = registry(&self.options.state);
                let hosts: Vec<PathBuf> = entries
                    .iter()
                    .filter(|entry| entry.host_alive)
                    .filter_map(|entry| sys::process_path(entry.record.host.pid).ok())
                    .collect();
                // `proc_pidpath` хостов канонический: корень рантаймов сравнивается тоже им.
                let root = runtime::root(&self.options.state);
                let root = fs::canonicalize(&root).unwrap_or(root);
                let cleanup = runtime::ready_generations(&self.options.state)
                    .map(|ready| {
                        runtime::cleanup_candidates(&root, &ready, crate::GENERATION, &hosts)
                    })
                    .unwrap_or_default();
                Reply::Sessions {
                    entries,
                    problems,
                    cleanup,
                }
            }
            Request::Quit => {
                self.set_closing(true);
                Reply::Done
            }
        }
    }

    /// Постановка или повторная проверка обновления, по одной за раз.
    fn staging(
        &self,
        work: impl FnOnce(&Install, &Mutex<Gate>) -> Result<Staging, compat::Refusal>,
    ) -> Reply {
        let _one = self
            .update
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let install = Install {
            state: &self.options.state,
            installed: &self.options.installed,
            identifier: &self.options.identifier,
            leaf: &self.options.leaf,
        };
        match work(&install, &self.gate) {
            Ok(staging) => Reply::Staging(Box::new(staging)),
            Err(refusal) => Reply::Refused {
                reason: refusal.reason,
                retry: refusal.retry,
            },
        }
    }

    /// `staged → intent` попытки, поставленной этим процессом (заморозка не снималась), и запуск
    /// копии помощника. Помощник не запустился — попытка прерывается здесь же по таблице.
    fn activate(&self) -> Reply {
        let _one = self
            .update
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let refused = |reason: String| Reply::Refused {
            reason,
            retry: false,
        };
        let frozen = self
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .frozen()
            .map(str::to_owned);
        if frozen.as_deref() != Some(compat::STAGED) {
            return refused(format!(
                "попытка не поставлена этим процессом: заморозка {frozen:?}"
            ));
        }
        let state = &self.options.state;
        let store = journal::Store::new(state);
        let journal = match store.read() {
            Ok(Some(journal)) if journal.state == State::Staged => journal,
            other => return refused(format!("activate: журнал {other:?}")),
        };
        let (entries, problems) = registry(state);
        if !problems.is_empty() {
            return refused(format!(
                "activate: записи сессий не разобраны: {problems:?}"
            ));
        }
        let sessions: Vec<SessionKeys> = entries
            .iter()
            .filter(|entry| entry.host_alive)
            .map(|entry| SessionKeys {
                session: entry.record.session.clone(),
                host: entry.record.host,
                workload: entry.record.workload,
            })
            .collect();
        let me = self.info.key;
        let deadline = now_ms() + update::HEALTH_WINDOW.as_millis() as u64;
        let launch = self.options.relaunch_args();
        self.set_closing(true);
        if let Err(err) = store.advance(&journal.attempt, &[State::Staged], |next| {
            next.state = State::Intent;
            next.deadline_ms = Some(deadline);
            next.launch = launch;
            next.sessions = sessions;
            next.gui_from = Some(me);
        }) {
            self.set_closing(false);
            return refused(err.to_string());
        }
        match self.spawn_helper(&journal.attempt) {
            Ok(helper) => {
                self.lock().handing = Some((journal.attempt.clone(), helper));
                Reply::Activating {
                    attempt: journal.attempt,
                    helper,
                }
            }
            Err(reason) => {
                let aborted = update::recover_now(&store, &self.options, me);
                self.set_closing(false);
                self.gate
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .thaw();
                refused(format!(
                    "помощник не запущен: {reason}; восстановление: {aborted:?}"
                ))
            }
        }
    }

    /// Копия помощника попытки в своей сессии; stdout и stderr — в каталог помощника.
    fn spawn_helper(&self, attempt: &str) -> Result<ProcessKey, String> {
        let state = &self.options.state;
        let dir = update::helper_dir(state, attempt);
        let output = |name: &str| {
            File::create(dir.join(name))
                .map(OwnedFd::from)
                .map_err(|err| format!("{}: {err}", dir.join(name).display()))
        };
        let program = update::helper_copy(state, attempt);
        let args = update::HelperArgs {
            state: state.clone(),
            attempt: attempt.to_owned(),
            tag: Some(self.options.tag.clone()),
        }
        .to_args();
        let pid = spawn::spawn(spawn::Spec {
            program: &program,
            args: &args,
            env: None,
            stdio: [
                None,
                Some(output(update::HELPER_STDOUT)?),
                Some(output(update::HELPER_STDERR)?),
            ],
            setsid: true,
            disclaim: false,
        })
        .map_err(|err| format!("запуск {}: {err}", program.display()))?;
        Ok(observer::inspect(pid)?
            .ok_or_else(|| format!("помощник {pid} не найден сразу после запуска"))?
            .key)
    }

    /// Health, срок под блокировкой непосредственно перед записью, `committed` и уборка прежнего
    /// N. `Ok(None)` — до подтверждения пришёл `quit`.
    fn confirm(&self, journal: &Journal) -> Result<Option<Confirmed>, Unhealthy> {
        let state = &self.options.state;
        let me = self.info.key;
        let deadline = journal
            .deadline_ms
            .ok_or_else(|| ("в launched нет срока".to_owned(), None))?;
        let facts = health::gather(
            state,
            &self.options.installed,
            journal,
            me,
            crate::GENERATION,
        );
        if self.lock().quitting {
            return Ok(None);
        }
        let violations = health::judge(&facts);
        if !violations.is_empty() {
            return Err((
                format!("health не выполнен: {}", violations.join("; ")),
                Some(Box::new(facts)),
            ));
        }
        let fail = |reason: String| (reason, Some(Box::new(facts.clone())));
        let store = journal::Store::new(state);
        match store.read() {
            Ok(Some(current))
                if current.attempt == journal.attempt
                    && current.state == State::Launched
                    && current.owner == Some(me) => {}
            other => return Err(fail(format!("перед committed журнал {other:?}"))),
        }
        let now = now_ms();
        if now >= deadline {
            return Err(fail(format!(
                "срок health истёк до подтверждения: {now} ≥ {deadline}"
            )));
        }
        store
            .advance(&journal.attempt, &[State::Launched], |next| {
                next.state = State::Committed;
            })
            .map_err(|err| fail(err.to_string()))?;
        let report = update::recover_now(&store, &self.options, me).map_err(fail)?;
        match store.read() {
            Ok(Some(done)) if update::committed(&done, &journal.attempt) => {}
            other => return Err(fail(format!("после уборки журнал {other:?}"))),
        }
        Ok(Some(Confirmed {
            attempt: journal.attempt.clone(),
            deadline_ms: deadline,
            health: facts,
            committed_ms: now,
            cleanup: report
                .steps
                .iter()
                .map(|step| format!("{step:?}"))
                .collect(),
        }))
    }

    /// Сессия под мьютексом заморозки: под заморозкой — временный отказ без запуска.
    fn create_session(&self, name: &str) -> Reply {
        let gate = self
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(reason) = gate.admit() {
            return Reply::Refused {
                reason,
                retry: true,
            };
        }
        match self.launch_session(name) {
            Ok(record) => Reply::Session(Box::new(record)),
            Err(reason) => Reply::Refused {
                reason,
                retry: false,
            },
        }
    }

    /// Хост из `runtime/<N>/` поколения владельца с нагрузкой оттуда же
    /// ([`runtime::for_new_session`]); запись сессии — в `<state>/s`.
    fn launch_session(&self, name: &str) -> Result<SessionRecord, String> {
        host::check_session_name(name)?;
        let state = &self.options.state;
        let dir = state.join(host::SESSIONS_DIR);
        fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        if host::record_path(&dir, name).exists() {
            return Err(format!("сессия {name} уже есть"));
        }
        let binaries = runtime::for_new_session(state, crate::GENERATION)?;
        let tag = self.options.tag.clone();
        let options = host::Options {
            dir,
            session: name.to_owned(),
            size: SESSION_SIZE,
            tag: tag.clone(),
            program: binaries.fixture,
            args: vec!["--tag".to_owned(), tag],
        };
        let stderr = io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .map(File::from)
            .map_err(|err| format!("stderr для хоста: {err}"))?;
        host::launch(&binaries.host, &options, stderr, self.options.disclaim)
    }
}

/// Реестр сессий по записям хостов в `<state>/s`: после гибели владельца он строится заново с
/// диска, а не из памяти. Каждая запись проверяется сейчас: живы ли хост и нагрузка по ключам,
/// отвечает ли хост на `probe` с диапазоном attach этой сборки.
pub fn registry(state: &Path) -> (Vec<Entry>, Vec<String>) {
    registry_with(
        state,
        &attach::hello(Purpose::Probe, ATTACH_VERSIONS, 0, Winsize::cells(0, 0)),
    )
}

/// Реестр, где каждый хост проверяется рукопожатием `hello` (назначение `probe`): так проверка
/// совместимости спрашивает диапазоном attach кандидата.
pub fn registry_with(state: &Path, hello: &Hello) -> (Vec<Entry>, Vec<String>) {
    let dir = state.join(host::SESSIONS_DIR);
    let mut entries = Vec::new();
    let mut problems = Vec::new();
    let names = match fs::read_dir(&dir) {
        Ok(names) => names,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return (entries, problems),
        Err(err) => {
            problems.push(format!("{}: {err}", dir.display()));
            return (entries, problems);
        }
    };
    let mut sessions: Vec<String> = names
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .filter_map(|name| name.strip_suffix(".json").map(str::to_owned))
        .collect();
    sessions.sort();
    for session in sessions {
        let record = match host::read_record(&dir, &session) {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(err) => {
                problems.push(format!("запись {session}: {err}"));
                continue;
            }
        };
        let alive = |key: ProcessKey| observer::identify(key).is_ok_and(|found| found.is_alive());
        let probe = match attach::connect(Path::new(&record.socket), hello, PROBE_TIMEOUT) {
            Ok(Connected::Welcome(client)) => Probe::Welcome {
                version: client.welcome.version,
            },
            Ok(Connected::Refused(reason)) => Probe::Refused { reason },
            Err(reason) => Probe::Unreachable { reason },
        };
        entries.push(Entry {
            host_alive: alive(record.host),
            workload_alive: alive(record.workload),
            probe,
            record,
        });
    }
    (entries, problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::tests::{journal_in, temp_dir};

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// Короткий каталог для сокетов: `$TMPDIR` с длинным именем не помещается в `sun_path`.
    fn socket_dir(name: &str) -> PathBuf {
        let dir = PathBuf::from(format!(
            "/tmp/psu-{name}-{}-{}",
            std::process::id(),
            sys::uptime_ns() % 1_000_000
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const LEAF: &str = "406F897FD84D95069D9A50ABD81AB09B7AFB7E55";

    fn options() -> AppOptions {
        AppOptions {
            state: PathBuf::from("/tmp/psu.x"),
            installed: PathBuf::from("/tmp/psu.x/install/probe-update.app"),
            leaf: LEAF.to_ascii_lowercase(),
            identifier: BUNDLE_ID.to_owned(),
            tag: "0123456789abcdef".to_owned(),
            update_attempt: None,
            window: false,
            disclaim: false,
            tcc_fixture: None,
            forward: Vec::new(),
        }
    }

    #[test]
    fn app_args_round_trip_and_reject_ambiguity() {
        let mut full = options();
        full.update_attempt = Some("ab12".to_owned());
        full.window = true;
        full.forward = strings(&["activate", "--x"]);
        assert_eq!(
            parse_app_args(&full.to_args(), None),
            Ok(AppMode::Run(full.clone()))
        );
        // Каталог состояния из окружения, если --state не задан.
        let base = options();
        let without_state: Vec<String> = base.to_args().into_iter().skip(2).collect();
        assert_eq!(
            parse_app_args(&without_state, Some("/tmp/psu.x")),
            Ok(AppMode::Run(base.clone()))
        );
        assert!(
            parse_app_args(&without_state, None)
                .unwrap_err()
                .contains("каталога состояния")
        );
        assert!(parse_app_args(&without_state, Some("")).is_err());
        let mut relative = base.to_args();
        relative[1] = "psu.x".to_owned();
        assert!(
            parse_app_args(&relative, None)
                .unwrap_err()
                .contains("абсолютный")
        );
        let mut bad_leaf = base.to_args();
        bad_leaf[5] = "xyz".to_owned();
        assert!(parse_app_args(&bad_leaf, None).is_err());
        let mut twice = base.to_args();
        twice.extend(strings(&["--tag", "again"]));
        assert!(parse_app_args(&twice, None).unwrap_err().contains("дважды"));
        let mut not_app = base.to_args();
        not_app[3] = "/tmp/psu.x/install/probe-update".to_owned();
        assert!(parse_app_args(&not_app, None).is_err());
        let mut bad_attempt = base.to_args();
        bad_attempt.extend(strings(&["--update-attempt", "XYZ"]));
        assert!(parse_app_args(&bad_attempt, None).is_err());
        assert!(
            !base
                .relaunch_args()
                .contains(&"--update-attempt".to_owned())
        );
    }

    #[test]
    fn stub_modes_still_parse() {
        assert_eq!(
            parse_app_args(&strings(&["--manifest"]), None),
            Ok(AppMode::Stub(StubMode::Manifest))
        );
        assert_eq!(
            parse_app_args(&strings(&["--report", "--tag", "n"]), None),
            Ok(AppMode::Stub(StubMode::Report))
        );
        assert!(parse_app_args(&strings(&["--report", "--state", "/x"]), None).is_err());
    }

    #[test]
    fn keychain_operation_is_its_own_mode() {
        let op = keychain::OpArgs {
            op: keychain::Op::Read,
            keychain: PathBuf::from("/k/psu-f-1.keychain-db"),
            account: keychain::ACCOUNT_N.to_owned(),
            no_ui: keychain::NoUi::ProcessFlag,
            secret_file: None,
            out: PathBuf::from("/o/next-read.json"),
            tag: "nonce.kc-next-read".to_owned(),
            window: true,
        };
        assert_eq!(
            parse_app_args(&op.to_args(), Some("/state")),
            Ok(AppMode::Keychain(op.clone()))
        );
        // Не первым аргументом `--keychain` — обычная роль GUI, которой он неизвестен.
        let mut late = strings(&["--tag", "n"]);
        late.extend(op.to_args());
        assert!(parse_app_args(&late, Some("/state")).is_err());
    }

    #[test]
    fn lock_is_exclusive_close_on_exec_and_keeps_its_inode() {
        let dir = temp_dir("owner-lock");
        let path = dir.join(LOCK_FILE);
        let first = Lock::try_acquire(&path).unwrap().expect("первое взятие");
        assert!(Lock::try_acquire(&path).unwrap().is_none(), "второе взятие");
        // SAFETY: дескриптор открыт, флаги читаются без указателей.
        let flags = unsafe { libc::fcntl(first._file.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        let inode = (first.dev, first.ino);
        drop(first);
        let again = Lock::try_acquire(&path)
            .unwrap()
            .expect("после освобождения");
        assert_eq!(
            (again.dev, again.ino),
            inode,
            "файл блокировки не пересоздаётся"
        );
        let waiter = {
            let path = path.clone();
            thread::spawn(move || Lock::acquire(&path).map(|lock| (lock.dev, lock.ino)))
        };
        drop(again);
        assert_eq!(waiter.join().unwrap().unwrap(), inode);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn facts<'a>(
        own: Option<&'a str>,
        installed: Option<&'a str>,
        journal: Option<&'a Journal>,
        next: Next,
    ) -> Facts<'a> {
        Facts {
            own_cdhash: own,
            installed_cdhash: installed,
            journal,
            next,
        }
    }

    #[test]
    fn ownership_needs_the_installed_code_and_the_owner_key() {
        let (a, b) = (Some("aa"), Some("bb"));
        let idle = journal_in(State::Idle, Path::new("/i/probe-update.app"));
        let deferred = journal_in(State::StagedDeferred, Path::new("/i/probe-update.app"));
        let launched = journal_in(State::Launched, Path::new("/i/probe-update.app"));
        let swapped = journal_in(State::Swapped, Path::new("/i/probe-update.app"));
        assert_eq!(decide_role(&facts(a, a, None, Next::Nothing)), Role::Owner);
        assert_eq!(
            decide_role(&facts(a, a, Some(&idle), Next::Nothing)),
            Role::Owner
        );
        assert_eq!(
            decide_role(&facts(a, a, Some(&deferred), Next::Nothing)),
            Role::Owner,
            "отложенное обновление обычному запуску не мешает"
        );
        assert_eq!(
            decide_role(&facts(a, a, Some(&idle), Next::LaunchInstalled)),
            Role::Owner,
            "после отката запущенный N сам становится владельцем"
        );
        for (own, installed, why) in [
            (a, b, "не равен"),
            (None, a, "не прочитан"),
            (a, None, "не прошёл"),
        ] {
            match decide_role(&facts(own, installed, Some(&idle), Next::Nothing)) {
                Role::Yield {
                    launch: true,
                    reason,
                } => assert!(reason.contains(why), "{reason}"),
                other => panic!("{own:?} {installed:?}: {other:?}"),
            }
        }
        assert!(matches!(
            decide_role(&facts(a, b, Some(&idle), Next::LaunchInstalled)),
            Role::Yield { launch: true, .. }
        ));
        assert!(matches!(
            decide_role(&facts(a, a, Some(&launched), Next::Yield)),
            Role::Yield { launch: false, .. }
        ));
        assert_eq!(
            decide_role(&facts(a, a, Some(&launched), Next::Owner)),
            Role::Owner
        );
        assert!(matches!(
            decide_role(&facts(a, b, Some(&launched), Next::Owner)),
            Role::Yield { launch: false, .. }
        ));
        assert_eq!(
            decide_role(&facts(a, a, Some(&swapped), Next::LaunchAsHelper)),
            Role::Helper {
                attempt: swapped.attempt.clone()
            }
        );
        // Восстановление, остановившееся вне idle с «ничего», права не даёт.
        assert!(matches!(
            decide_role(&facts(a, a, Some(&launched), Next::Nothing)),
            Role::Yield { launch: false, .. }
        ));
    }

    #[test]
    fn connect_errors_are_classified_without_touching_the_socket() {
        let err = |code| io::Error::from_raw_os_error(code);
        assert_eq!(
            classify_connect(&err(libc::ENOENT), true),
            Ok(Unavailable::Restarting)
        );
        assert_eq!(
            classify_connect(&err(libc::ENOENT), false),
            Ok(Unavailable::Down)
        );
        assert_eq!(
            classify_connect(&err(libc::ECONNREFUSED), false),
            Ok(Unavailable::Restarting)
        );
        assert!(classify_connect(&err(libc::EACCES), true).is_err());
        assert!(Unavailable::Restarting.retry() && Unavailable::NotResponding.retry());
        assert!(!Unavailable::Down.retry());
    }

    #[test]
    fn client_answers_from_a_live_stale_silent_or_missing_owner() {
        let dir = socket_dir("oc");
        let ping = Request::Ping { wait_ms: 0 };
        let short = Duration::from_millis(300);
        assert_eq!(
            request(&dir, &ping, short),
            Ok(Answer::Unavailable(Unavailable::Down))
        );
        File::create(lock_path(&dir)).unwrap();
        assert_eq!(
            request(&dir, &ping, short),
            Ok(Answer::Unavailable(Unavailable::Restarting))
        );
        // Путь сокета без слушающего: ECONNREFUSED, путь остаётся.
        drop(UnixListener::bind(socket_path(&dir)).unwrap());
        assert_eq!(
            request(&dir, &ping, short),
            Ok(Answer::Unavailable(Unavailable::Restarting))
        );
        assert!(socket_path(&dir).exists(), "клиент не удаляет чужой сокет");
        fs::remove_file(socket_path(&dir)).unwrap();
        // Слушает, но не отвечает.
        let silent = UnixListener::bind(socket_path(&dir)).unwrap();
        assert_eq!(
            request(&dir, &ping, short),
            Ok(Answer::Unavailable(Unavailable::NotResponding))
        );
        drop(silent);
        fs::remove_file(socket_path(&dir)).unwrap();
        // Отвечает строкой JSON.
        let listener = UnixListener::bind(socket_path(&dir)).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let got: Request = read_json_line(&mut stream).unwrap().unwrap();
            write_json_line(
                &mut stream,
                &Reply::Receipt {
                    seq: 7,
                    owner: ProcessKey {
                        pid: 1,
                        start_us: 2,
                    },
                },
            )
            .unwrap();
            got
        });
        let forward = Request::Forward {
            args: strings(&["a b", "ü"]),
            from: ProcessKey {
                pid: 3,
                start_us: 4,
            },
            wait_ms: 5,
        };
        assert_eq!(
            request(&dir, &forward, Duration::from_secs(5)),
            Ok(Answer::Reply(Reply::Receipt {
                seq: 7,
                owner: ProcessKey {
                    pid: 1,
                    start_us: 2
                }
            }))
        );
        assert_eq!(server.join().unwrap(), forward);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn messages_round_trip_as_json_lines() {
        let requests = [
            Request::Ping { wait_ms: 1 },
            Request::CreateSession {
                name: "h1".to_owned(),
            },
            Request::Sessions,
            Request::Update {
                candidate: "/b/probe-update.app".to_owned(),
            },
            Request::Retry,
            Request::Quit,
        ];
        for request in requests {
            let mut line = Vec::new();
            write_json_line(&mut line, &request).unwrap();
            assert_eq!(line.iter().filter(|byte| **byte == b'\n').count(), 1);
            assert_eq!(read_json_line::<Request>(&line[..]).unwrap(), Some(request));
        }
        let reply = Reply::Refused {
            reason: "нет".to_owned(),
            retry: true,
        };
        let mut line = Vec::new();
        write_json_line(&mut line, &reply).unwrap();
        assert_eq!(read_json_line::<Reply>(&line[..]).unwrap(), Some(reply));
        assert_eq!(read_json_line::<Reply>(&b""[..]).unwrap(), None);
        assert!(read_json_line::<Reply>(&b"{\n"[..]).is_err());
    }

    #[test]
    fn stale_socket_is_removed_but_nothing_else() {
        let dir = socket_dir("os");
        let socket = socket_path(&dir);
        assert_eq!(remove_stale_socket(&socket), Ok(None));
        drop(UnixListener::bind(&socket).unwrap());
        let inode = Inode::of(&socket).unwrap();
        assert_eq!(remove_stale_socket(&socket), Ok(Some(inode)));
        assert!(!socket.exists());
        fs::write(&socket, b"x").unwrap();
        assert!(remove_stale_socket(&socket).is_err());
        assert!(socket.exists(), "не сокет не удаляется");
        fs::remove_file(&socket).unwrap();
        let (listener, published) = publish(&dir, &socket).unwrap();
        assert_eq!(published.inode, Inode::of(&socket).unwrap());
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [SOCKET_FILE], "временное имя не осталось");
        UnixStream::connect(&socket).unwrap();
        published.unpublish();
        assert!(!socket.exists());
        drop(listener);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn test_server(state: &Path) -> Server {
        let key = ProcessKey {
            pid: 1,
            start_us: 1,
        };
        let inode = Inode { dev: 0, ino: 0 };
        Server {
            options: AppOptions {
                state: state.to_path_buf(),
                installed: state.join("install").join(generation::APP_NAME),
                ..options()
            },
            info: OwnerInfo {
                generation: 1,
                key,
                executable: String::new(),
                cdhash: String::new(),
                installed: String::new(),
                installed_cdhash: String::new(),
                socket: String::new(),
                socket_inode: inode,
                lock_inode: inode,
                stale_socket: None,
                runtime: Prepared {
                    dir: runtime::ready_dir(state, 1),
                    outcome: runtime::Outcome::Created,
                    recovered: None,
                },
                recovery: Vec::new(),
                window: false,
                ready: true,
                received: Vec::new(),
                update: None,
            },
            state: Mutex::new(ServerState {
                ready: true,
                received: Vec::new(),
                closing: false,
                quitting: false,
                handing: None,
                update: None,
            }),
            changed: Condvar::new(),
            gate: Mutex::new(Gate::default()),
            update: Mutex::new(()),
        }
    }

    /// Уходящий владелец (после `quit` или начала `activate`) аргументы второго запуска не
    /// принимает: временный отказ с повтором, квитанции нет, принятые раньше остаются.
    #[test]
    fn a_closing_owner_refuses_forwarded_args_with_a_retry() {
        let dir = temp_dir("owner-closing");
        let server = test_server(&dir);
        let from = ProcessKey {
            pid: 7,
            start_us: 7,
        };
        let forward = |arg: &str| {
            server.handle(Request::Forward {
                args: vec![arg.to_owned()],
                from,
                wait_ms: 0,
            })
        };
        assert_eq!(
            forward("before"),
            Reply::Receipt {
                seq: 1,
                owner: server.info.key
            }
        );
        assert_eq!(server.handle(Request::Quit), Reply::Done);
        assert_eq!(
            forward("after-quit"),
            Reply::Refused {
                reason: Unavailable::Restarting.reason().to_owned(),
                retry: true
            }
        );
        let snapshot = server.snapshot();
        assert_eq!(snapshot.received.len(), 1);
        assert_eq!(snapshot.received[0].args, ["before"]);
        assert!(matches!(
            server.handle(Request::Ping { wait_ms: 0 }),
            Reply::Pong(_)
        ));
        fs::remove_dir_all(&dir).unwrap();
        // Отказ activate по предусловию владельца не закрывает: квитанции идут дальше.
        let dir = temp_dir("owner-closing-activate");
        let server = test_server(&dir);
        assert!(matches!(
            server.handle(Request::Activate),
            Reply::Refused { retry: false, .. }
        ));
        assert!(matches!(
            server.handle(Request::Forward {
                args: Vec::new(),
                from,
                wait_ms: 0,
            }),
            Reply::Receipt { seq: 1, .. }
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn session_creation_under_freeze_is_a_temporary_refusal_without_launch() {
        let dir = temp_dir("owner-freeze");
        let server = test_server(&dir);
        let create = || {
            server.handle(Request::CreateSession {
                name: "h1".to_owned(),
            })
        };
        let freeze = compat::Freeze::take(&server.gate).unwrap();
        assert_eq!(
            create(),
            Reply::Refused {
                reason: compat::CHECKING.to_owned(),
                retry: true
            }
        );
        assert!(
            !dir.join(host::SESSIONS_DIR).exists(),
            "под заморозкой хост не запускался"
        );
        drop(freeze);
        // Без заморозки запрос доходит до запуска хоста; рантайма нет — окончательный отказ.
        match create() {
            Reply::Refused {
                retry: false,
                reason,
            } => assert!(reason.contains("runtime"), "{reason}"),
            other => panic!("{other:?}"),
        }
        compat::Freeze::take(&server.gate).unwrap().keep();
        assert!(matches!(create(), Reply::Refused { retry: true, .. }));
        // Под оставленной заморозкой новая проверка обновления не начинается.
        assert!(matches!(
            server.handle(Request::Update {
                candidate: "/nonexistent/probe-update.app".to_owned()
            }),
            Reply::Refused { retry: true, .. }
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn update_requests_refuse_without_touching_the_journal() {
        let dir = temp_dir("owner-update");
        let server = test_server(&dir);
        assert!(matches!(
            server.handle(Request::Update {
                candidate: "probe-update.app".to_owned()
            }),
            Reply::Refused { retry: false, .. }
        ));
        match server.handle(Request::Retry) {
            Reply::Refused {
                retry: false,
                reason,
            } => assert!(reason.contains("отложенной попытки нет"), "{reason}"),
            other => panic!("{other:?}"),
        }
        let store = journal::Store::new(&dir);
        assert_eq!(store.read(), Ok(None));
        let deferred = journal_in(State::StagedDeferred, &server.options.installed);
        journal::tests::put(&store, &deferred);
        let before = fs::read(store.path()).unwrap();
        match server.handle(Request::Update {
            candidate: "/nonexistent/probe-update.app".to_owned(),
        }) {
            Reply::Refused {
                retry: false,
                reason,
            } => assert!(reason.contains("staged-deferred"), "{reason}"),
            other => panic!("{other:?}"),
        }
        // Повтор без кандидата на диске: проверка подписи не проходит, журнал прежний.
        assert!(matches!(
            server.handle(Request::Retry),
            Reply::Refused { retry: false, .. }
        ));
        assert_eq!(fs::read(store.path()).unwrap(), before);
        assert_eq!(
            server.gate.lock().unwrap().admit(),
            Ok(()),
            "заморозка снята"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn registry_reports_dead_sessions_from_disk() {
        let dir = temp_dir("owner-registry");
        let sessions = dir.join(host::SESSIONS_DIR);
        fs::create_dir_all(&sessions).unwrap();
        let record = SessionRecord {
            session: "h1".to_owned(),
            host: ProcessKey {
                pid: i32::MAX - 7,
                start_us: 1,
            },
            executable: "/x/runtime/1/probe-update-host".to_owned(),
            workload: ProcessKey {
                pid: i32::MAX - 8,
                start_us: 1,
            },
            workload_pgid: 1,
            workload_sid: None,
            tty: "/dev/ttys999".to_owned(),
            versions: ATTACH_VERSIONS,
            generation: 1,
            socket: sessions.join("h1.sock").display().to_string(),
        };
        host::write_record(&sessions, &record).unwrap();
        fs::write(sessions.join("bad.json"), b"{").unwrap();
        fs::write(sessions.join(".h2.json.1"), b"{").unwrap();
        let (entries, problems) = registry(&dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].record, record);
        assert!(!entries[0].host_alive && !entries[0].workload_alive);
        assert!(matches!(entries[0].probe, Probe::Unreachable { .. }));
        assert_eq!(problems.len(), 1, "{problems:?}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
