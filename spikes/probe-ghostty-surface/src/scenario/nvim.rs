//! Сценарий `nvim` (H12, H4): `nvim --clean` в поверхности центральной зоны с временными `XDG_*`
//! и рабочим каталогом в `--out`. Проверки:
//!
//! - запуск: nvim из `command` поверхности слушает сокет, TUI nvim держит PTY поверхности
//!   управляющим терминалом и стоит на его переднем плане;
//! - ввод текста и `:w`: содержимое файла совпадает с напечатанным текстом;
//! - `:terminal`, затем `Ctrl-\ Ctrl-\ Ctrl-n`: `mode(1)` переходит из `t` в `nt`, маршрутизатор
//!   съедает первый аккорд и отдаёт второй повтором исходного события через `ghostty_surface_key`;
//! - ресайз зоны: сетка `ghostty_surface_size`, `TIOCGWINSZ` PTY и `&columns`/`&lines` nvim
//!   совпадают до и после, сетка меняется.
//!
//! Клавиши идут через `postEvent` и обычную доставку `NSApplication`, маршрутизатор режимов стоит в
//! `sendEvent`. Состояние nvim читает `nvim --server <сокет> --remote-expr` в рабочем потоке, по
//! одному запросу за раз. Сокет задан относительно рабочего каталога nvim и клиента: полный путь в
//! `--out` длиннее `sun_path` (104 байта). `:terminal` запускает `cat` через `/bin/sh`, а не shell
//! владельца с его настройками и историей. В конце `:qa!`, ожидание выхода процессов nvim и
//! задания `:terminal`, затем закрытие поверхности.
//!
//! nvim работает двумя процессами: TUI на PTY поверхности и сервер `--embed`. `getpid()` отдаёт
//! сервер, TUI — его родитель. Оба несут nonce запуска в имени файла, по нему их находит внешний
//! сторож `--launch`.
//!
//! Настоящий агент, удобство аккорда и `Ctrl-\ Ctrl-o` — ручная приёмка задачи 19.

use std::cell::{Cell, RefCell};
use std::fs;
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::NSEventModifierFlags;
use objc2_foundation::NSRect;
use serde::Serialize;

use super::events::{self, Key};
use super::input::{KEY_ESC, focused, key};
use super::surface_env::{self, Runtime};
use super::{Poll, Report, Scenario, Stand, Step, poll, run_steps, window};
use crate::fixture::{self, Winsize};
use crate::ghostty::config::ColorScheme;
use crate::ghostty::runtime::{self, Created};
use crate::ghostty::surface::{GridSize, Phase, Spec, SurfaceId};
use crate::keys::Action as KeyAction;
use crate::results::{Requirement, Verdict};
use crate::router::{self, Mode};
use crate::scene::{Divider, Scene, Zone};
use crate::view::SurfaceView;

const PROGRAM: &str = "nvim";
/// Сокет относительно рабочего каталога nvim и клиента.
const SOCKET: &str = "./nvim.sock";
const SOCKET_FILE: &str = "nvim.sock";
/// `shell` nvim: `:terminal` идёт через него без настроек и истории владельца.
const SHELL: &str = "/bin/sh";
const TERMINAL_COMMAND: &str = "cat";
/// Состояние nvim одним ответом: полный режим, размер, PID сервера, признак буфера терминала и
/// PID его задания.
const STATE_EXPR: &str = "join([mode(1), &columns, &lines, getpid(), &buftype ==# 'terminal', get(b:, 'terminal_job_pid', 0)], ' ')";
/// Переменные XDG и их каталоги внутри каталога сценария.
const XDG: [(&str, &str); 5] = [
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_STATE_HOME", "state"),
    ("XDG_CACHE_HOME", "cache"),
    ("XDG_RUNTIME_DIR", "run"),
];

const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const FOCUS_TIMEOUT: Duration = Duration::from_secs(5);
const SIZE_TIMEOUT: Duration = Duration::from_secs(3);
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const STATE_TIMEOUT: Duration = Duration::from_secs(6);
const ACT_TIMEOUT: Duration = Duration::from_secs(1);
/// Внутренний предел проверки: по нему итог записывается расхождением, а не тайм-аутом шага.
const CHECK_LIMIT: Duration = Duration::from_secs(4);
const CHECK_TIMEOUT: Duration = Duration::from_secs(8);
const QUIT_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Запрос `--remote-expr` дольше этого снимается.
const REQUEST_LIMIT: Duration = Duration::from_secs(5);
/// Сдвиг делителей при ресайзе, точки: центральная зона уже и ниже.
const RESIZE_SHIFT: f64 = 140.0;
const VIEWPORT_LIMIT: usize = 600;

const SHIFT: usize = NSEventModifierFlags::Shift.0;
const CONTROL: usize = NSEventModifierFlags::Control.0;

const CHORD: Key = key("Ctrl-\\", 42, "\u{1c}", "\\", CONTROL);
const CTRL_N: Key = key("Ctrl-n", 45, "\u{e}", "n", CONTROL);

/// Клавиши латинской раскладки ANSI для печати текста.
const TEXT_KEYS: [Key; 39] = [
    key("a", 0, "a", "a", 0),
    key("b", 11, "b", "b", 0),
    key("c", 8, "c", "c", 0),
    key("d", 2, "d", "d", 0),
    key("e", 14, "e", "e", 0),
    key("f", 3, "f", "f", 0),
    key("g", 5, "g", "g", 0),
    key("h", 4, "h", "h", 0),
    key("i", 34, "i", "i", 0),
    key("j", 38, "j", "j", 0),
    key("k", 40, "k", "k", 0),
    key("l", 37, "l", "l", 0),
    key("m", 46, "m", "m", 0),
    key("n", 45, "n", "n", 0),
    key("o", 31, "o", "o", 0),
    key("p", 35, "p", "p", 0),
    key("q", 12, "q", "q", 0),
    key("r", 15, "r", "r", 0),
    key("s", 1, "s", "s", 0),
    key("t", 17, "t", "t", 0),
    key("u", 32, "u", "u", 0),
    key("v", 9, "v", "v", 0),
    key("w", 13, "w", "w", 0),
    key("x", 7, "x", "x", 0),
    key("y", 16, "y", "y", 0),
    key("z", 6, "z", "z", 0),
    key("0", 29, "0", "0", 0),
    key("1", 18, "1", "1", 0),
    key("2", 19, "2", "2", 0),
    key("3", 20, "3", "3", 0),
    key("4", 21, "4", "4", 0),
    key("5", 23, "5", "5", 0),
    key("6", 22, "6", "6", 0),
    key("7", 26, "7", "7", 0),
    key("8", 28, "8", "8", 0),
    key("9", 25, "9", "9", 0),
    key("space", 49, " ", " ", 0),
    key("Shift-;", 41, ":", ";", SHIFT),
    key("Shift-1", 18, "!", "1", SHIFT),
];

/// Клавиши для печати `text`; символ вне таблицы — ошибка стенда.
pub fn keys_for(text: &str) -> Result<Vec<Key>, String> {
    text.chars()
        .map(|ch| {
            TEXT_KEYS
                .iter()
                .find(|key| key.characters.chars().eq(std::iter::once(ch)))
                .copied()
                .ok_or_else(|| format!("нет клавиши для {ch:?}"))
        })
        .collect()
}

// ---- окружение и команда ----

/// Каталоги nvim в каталоге сценария: рабочий и по одному на каждую переменную XDG.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Dirs {
    pub work: PathBuf,
    pub xdg: Vec<(&'static str, PathBuf)>,
}

impl Dirs {
    pub fn under(root: &Path) -> Dirs {
        Dirs {
            work: root.join("work"),
            xdg: XDG
                .iter()
                .map(|(name, dir)| (*name, root.join("xdg").join(dir)))
                .collect(),
        }
    }

    fn create(&self) -> Result<(), String> {
        for dir in std::iter::once(&self.work).chain(self.xdg.iter().map(|(_, dir)| dir)) {
            fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        }
        Ok(())
    }
}

/// Переменные XDG nvim: каждая — свой каталог сценария. Их получают и поверхность, и клиент.
pub fn nvim_env(dirs: &Dirs) -> Vec<(String, String)> {
    dirs.xdg
        .iter()
        .map(|(name, dir)| ((*name).to_owned(), dir.display().to_string()))
        .collect()
}

/// Окружение клиента `nvim --server`: с нуля, из `inherited` только белый список запуска и
/// `LANG`, затем XDG сценария.
pub fn client_env(
    inherited: impl IntoIterator<Item = (String, String)>,
    dirs: &Dirs,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = inherited
        .into_iter()
        .filter(|(name, _)| {
            crate::env::INHERITED.contains(&name.as_str()) || name == crate::env::LANG
        })
        .collect();
    env.sort();
    env.dedup_by(|later, earlier| later.0 == earlier.0);
    env.extend(nvim_env(dirs));
    env
}

/// Команда поверхности: nvim по абсолютному пути, сокет, `shell` для `:terminal` и файл.
pub fn command(program: &Path, file: &str) -> String {
    fixture::shell_command(
        &program.display().to_string(),
        &[
            "--clean",
            "--listen",
            SOCKET,
            "--cmd",
            &format!("set shell={SHELL}"),
            file,
        ],
    )
}

/// Исполняемый `name` в каталогах `path` по порядку.
fn find_program(name: &str, path: &str) -> Option<PathBuf> {
    path.split(':')
        .filter(|dir| dir.starts_with('/'))
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

// ---- ответы nvim ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteError {
    /// Сервер не принимает соединение (E247): ещё не слушает или уже вышел.
    Unreachable(String),
    Failed(String),
}

impl RemoteError {
    fn describe(&self) -> String {
        match self {
            RemoteError::Unreachable(reason) => format!("сервер недоступен: {reason}"),
            RemoteError::Failed(reason) => reason.clone(),
        }
    }
}

/// Ответ клиента nvim: stdout без перевода строки в конце. Ненулевой код — ошибка с первой
/// непустой строкой stderr.
pub fn parse_reply(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Result<String, RemoteError> {
    let stderr = String::from_utf8_lossy(stderr);
    let first = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_owned();
    match code {
        Some(0) => {}
        Some(code) if first.starts_with("E247:") => {
            return Err(RemoteError::Unreachable(format!("код {code}: {first}")));
        }
        Some(code) => return Err(RemoteError::Failed(format!("код {code}: {first}"))),
        None => {
            return Err(RemoteError::Failed(format!(
                "клиент завершён сигналом: {first}"
            )));
        }
    }
    let text = std::str::from_utf8(stdout)
        .map_err(|err| RemoteError::Failed(format!("ответ не UTF-8: {err}")))?;
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    Ok(text.to_owned())
}

/// Состояние nvim по [`STATE_EXPR`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NvimState {
    pub mode: String,
    pub columns: u16,
    pub lines: u16,
    pub pid: i32,
    pub terminal: bool,
    pub job_pid: i32,
}

pub fn parse_state(reply: &str) -> Result<NvimState, String> {
    let fields: Vec<&str> = reply.split(' ').collect();
    let &[mode, columns, lines, pid, terminal, job_pid] = fields.as_slice() else {
        return Err(format!("ответ {reply:?}: ожидалось шесть полей"));
    };
    if mode.is_empty() {
        return Err(format!("ответ {reply:?}: пустой режим"));
    }
    let field = |name: &str, text: &str| {
        text.parse::<i64>()
            .map_err(|err| format!("ответ {reply:?}: поле {name} {text:?}: {err}"))
    };
    let narrow = |name: &str, text: &str| {
        field(name, text).and_then(|value| {
            u16::try_from(value).map_err(|_| format!("ответ {reply:?}: поле {name} вне u16"))
        })
    };
    let pid_of = |name: &str, text: &str| {
        field(name, text).and_then(|value| {
            i32::try_from(value).map_err(|_| format!("ответ {reply:?}: поле {name} вне i32"))
        })
    };
    let terminal = match terminal {
        "0" => false,
        "1" => true,
        other => return Err(format!("ответ {reply:?}: признак терминала {other:?}")),
    };
    Ok(NvimState {
        mode: mode.to_owned(),
        columns: narrow("columns", columns)?,
        lines: narrow("lines", lines)?,
        pid: pid_of("pid", pid)?,
        terminal,
        job_pid: pid_of("job_pid", job_pid)?,
    })
}

// ---- клиент в рабочем потоке ----

struct Inflight {
    args: Vec<String>,
    started: Instant,
    pid: Arc<AtomicI32>,
    reply: Receiver<Result<Output, String>>,
}

impl Inflight {
    /// Снимает клиент, если он ещё не завершён.
    fn kill(&self) {
        let pid = self.pid.load(Ordering::SeqCst);
        if pid > 0 {
            // SAFETY: сигнал процессу клиента; PID обнуляется рабочим потоком после `wait`.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Ответ на запрос и время его запуска.
struct Answer {
    started: Instant,
    result: Result<String, RemoteError>,
}

#[derive(Clone, Debug, Serialize)]
struct Call {
    request: String,
    ms: f64,
    outcome: String,
}

/// Запросы к nvim отдельными процессами клиента: один в работе, ответ читается без ожидания.
struct Remote {
    program: PathBuf,
    dir: PathBuf,
    env: Vec<(String, String)>,
    inflight: Option<Inflight>,
    calls: Vec<Call>,
}

const CALLS_LIMIT: usize = 400;

impl Remote {
    /// Ответ на запрос `args`, если он пришёл. Ничего не идёт — запрос запускается; ответ на
    /// другой запрос отбрасывается.
    fn ask(&mut self, args: &[String]) -> Option<Answer> {
        let Some(inflight) = self.inflight.as_ref() else {
            self.start(args);
            return None;
        };
        let received = match inflight.reply.try_recv() {
            Ok(received) => received,
            Err(TryRecvError::Empty) if inflight.started.elapsed() < REQUEST_LIMIT => return None,
            Err(TryRecvError::Empty) => {
                inflight.kill();
                Err(format!(
                    "нет ответа за {} мс, клиент снят",
                    REQUEST_LIMIT.as_millis()
                ))
            }
            Err(TryRecvError::Disconnected) => Err("поток запроса вышел без ответа".to_owned()),
        };
        let inflight = self.inflight.take()?;
        let result = received
            .map_err(RemoteError::Failed)
            .and_then(|output| parse_reply(output.status.code(), &output.stdout, &output.stderr));
        if self.calls.len() < CALLS_LIMIT {
            self.calls.push(Call {
                request: inflight.args.last().cloned().unwrap_or_default(),
                ms: inflight.started.elapsed().as_secs_f64() * 1e3,
                outcome: match &result {
                    Ok(text) => truncate(text, 80),
                    Err(error) => error.describe(),
                },
            });
        }
        (inflight.args == args).then_some(Answer {
            started: inflight.started,
            result,
        })
    }

    fn start(&mut self, args: &[String]) {
        let (sender, reply) = mpsc::channel();
        let fallback = sender.clone();
        let pid = Arc::new(AtomicI32::new(0));
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .current_dir(&self.dir)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child_pid = Arc::clone(&pid);
        let spawned = thread::Builder::new()
            .name("probe-nvim-remote".to_owned())
            .spawn(move || {
                let result = match command.spawn() {
                    Ok(child) => {
                        child_pid.store(i32::try_from(child.id()).unwrap_or(0), Ordering::SeqCst);
                        let output = child
                            .wait_with_output()
                            .map_err(|err| format!("ожидание клиента: {err}"));
                        child_pid.store(0, Ordering::SeqCst);
                        output
                    }
                    Err(err) => Err(format!("запуск клиента: {err}")),
                };
                let _ = sender.send(result);
            });
        if let Err(err) = spawned {
            let _ = fallback.send(Err(format!("поток запроса не запущен: {err}")));
        }
        self.inflight = Some(Inflight {
            args: args.to_vec(),
            started: Instant::now(),
            pid,
            reply,
        });
    }

    /// Снимает клиент, который ещё работает: итог сценария его не ждёт.
    fn abandon(&mut self) {
        if let Some(inflight) = self.inflight.take() {
            inflight.kill();
        }
    }

    fn durations(&self) -> Vec<f64> {
        self.calls.iter().map(|call| call.ms).collect()
    }
}

fn state_args() -> Vec<String> {
    ["--clean", "--server", SOCKET, "--remote-expr", STATE_EXPR]
        .map(str::to_owned)
        .to_vec()
}

// ---- процессы и PTY ----

/// Сведения `proc_pidinfo(PROC_PIDTBSDINFO)`: родитель, группа, управляющий терминал и его
/// группа переднего плана.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct ProcInfo {
    pid: i32,
    ppid: i32,
    pgid: i32,
    tdev: u32,
    tpgid: i32,
}

fn proc_info(pid: i32) -> Option<ProcInfo> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: для `PROC_PIDTBSDINFO` ядро пишет `proc_bsdinfo` размера `size`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: ядро заполнило структуру целиком.
    let info = unsafe { info.assume_init() };
    Some(ProcInfo {
        pid,
        ppid: i32::try_from(info.pbi_ppid).ok()?,
        pgid: i32::try_from(info.pbi_pgid).ok()?,
        tdev: info.e_tdev,
        tpgid: i32::try_from(info.e_tpgid).ok()?,
    })
}

/// `TIOCGWINSZ` PTY по имени ведомой стороны; открытие не делает её управляющим терминалом.
fn tty_winsize(path: &str) -> Result<Winsize, String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(path)
        .map_err(|err| format!("открытие {path}: {err}"))?;
    fixture::winsize(file.as_fd())
        .ok_or_else(|| format!("TIOCGWINSZ {path}: {}", std::io::Error::last_os_error()))
}

fn tty_rdev(path: &str) -> Option<u64> {
    fs::metadata(path).ok().map(|meta| meta.rdev())
}

fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

// ---- записи проверок ----

/// Сетка: столбцы и строки.
type Grid = (u16, u16);

/// Сетки по трём источникам: `ghostty_surface_size`, `TIOCGWINSZ` и nvim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct Sizes {
    surface: Grid,
    tty: Grid,
    nvim: Grid,
}

impl Sizes {
    fn agree(&self) -> bool {
        self.surface == self.tty && self.surface == self.nvim
    }
}

#[derive(Clone, Copy, Default, Debug, Serialize)]
struct Marks {
    arrivals: usize,
    sent: usize,
    replays: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
struct ChordRecord {
    mode_before: Option<String>,
    router_after_first: Option<(Mode, bool)>,
    nvim_after_first: Option<String>,
    router_after_second: Option<(Mode, bool)>,
    replays: Vec<(bool, Option<String>, usize)>,
    replay_sent: Vec<(KeyAction, u32)>,
    chord_arrivals: usize,
    ctrl_n_sent: usize,
    mode_after: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct Pids {
    server_pid: Option<i32>,
    server: Option<ProcInfo>,
    tui: Option<ProcInfo>,
    job: Option<i32>,
    foreground_pid: Option<u64>,
    tty: Option<String>,
    tty_rdev: Option<u64>,
}

struct Nvim {
    /// Runtime со своим сокетом фикстур: держится до итога, как в других сценариях.
    _runtime: Runtime,
    scene: Rc<Scene>,
    view: Retained<SurfaceView>,
    id: SurfaceId,
    file: PathBuf,
    text: String,
    remote: Remote,
    state_args: Vec<String>,
    version: Option<String>,
    posted: Vec<u64>,
    /// Последнее прочитанное состояние nvim.
    last: Option<NvimState>,
    /// Ошибка, найденная предикатом; её возвращает действие шага.
    error: Option<String>,
    /// Начало внутреннего предела текущей проверки.
    since: Option<Instant>,
    pids: Pids,
    sizes_before: Option<Sizes>,
    sizes_after: Option<Sizes>,
    /// Сетка поверхности и PTY совпала после ресайза: ответ nvim принимается с этого момента.
    resized_at: Option<Instant>,
    marks: Marks,
    chord: ChordRecord,
    written: Option<String>,
    /// `show_child_exited` после `:qa!`.
    child_exits: u64,
    findings: Vec<(String, Requirement, Verdict, String)>,
}

impl Nvim {
    fn limit_passed(&self) -> bool {
        self.since
            .is_some_and(|since| since.elapsed() >= CHECK_LIMIT)
    }

    fn take_error(&mut self) -> Result<(), String> {
        match self.error.take() {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    fn finding(&mut self, name: &str, ok: bool, reason: String) {
        let verdict = if ok { Verdict::Pass } else { Verdict::Fail };
        self.findings
            .push((name.to_owned(), Requirement::Required, verdict, reason));
    }

    /// Состояние nvim по ответу, запущенному не раньше `after`. Ошибка запроса остаётся в
    /// [`Nvim::error`]; недоступный сервер — ожидание, если `tolerate_unreachable`.
    fn read_state(&mut self, after: Instant, tolerate_unreachable: bool) -> Option<NvimState> {
        let args = self.state_args.clone();
        let answer = self.remote.ask(&args)?;
        if answer.started < after {
            return None;
        }
        match answer.result {
            Ok(reply) => match parse_state(&reply) {
                Ok(state) => {
                    self.last = Some(state.clone());
                    Some(state)
                }
                Err(reason) => {
                    self.error = Some(reason);
                    None
                }
            },
            Err(RemoteError::Unreachable(_)) if tolerate_unreachable => None,
            Err(error) => {
                self.error = Some(format!("--remote-expr: {}", error.describe()));
                None
            }
        }
    }

    fn describe(&self) -> String {
        let text = runtime::viewport_text(self.id)
            .map(|text| truncate(text.trim_end(), VIEWPORT_LIMIT))
            .unwrap_or_else(|| "<не прочитан>".to_owned());
        format!(
            "последнее состояние nvim {:?}, поверхность {}; видимый текст: {text}",
            self.last,
            runtime::phase(self.id).map_or("нет", Phase::name)
        )
    }

    fn dispatched(&self) -> Poll<()> {
        let waiting: Vec<u64> = self
            .posted
            .iter()
            .copied()
            .filter(|number| {
                !events::record(*number)
                    .is_some_and(|record| record.send_return.is_some() || record.undispatched)
            })
            .collect();
        poll(waiting.is_empty(), || {
            format!("события {waiting:?} не вернулись из sendEvent")
        })
    }

    /// Сетки поверхности и PTY.
    fn surface_and_tty(&self) -> Result<(Grid, Grid), String> {
        let reading = runtime::read(self.id).ok_or("поверхность не живая")?;
        let tty = self.pids.tty.as_deref().ok_or("у поверхности нет tty")?;
        let winsize = tty_winsize(tty)?;
        Ok((grid(&reading.size), (winsize.cols, winsize.rows)))
    }

    fn router_marks(&self) -> Result<Marks, String> {
        let router = router::snapshot().ok_or("маршрутизатор не стоит")?;
        let log = self.view.input_log();
        Ok(Marks {
            arrivals: log.arrivals.len(),
            sent: log.sent.len(),
            replays: router.record.replays.len(),
        })
    }
}

fn grid(size: &GridSize) -> Grid {
    (size.columns, size.rows)
}

type Shared = Rc<RefCell<Nvim>>;

/// Сборщик шагов: замыкания получают стенд и состояние сценария.
struct Plan {
    stand: Rc<Stand>,
    nvim: Shared,
    steps: Vec<Step>,
}

impl Plan {
    fn step(
        &mut self,
        name: &'static str,
        timeout: Duration,
        mut ready: impl FnMut(&Stand, &mut Nvim) -> Poll<()> + 'static,
        act: impl FnOnce(&Stand, &mut Nvim) -> Result<(), String> + 'static,
    ) {
        let (ready_stand, ready_nvim) = (Rc::clone(&self.stand), Rc::clone(&self.nvim));
        let (act_stand, act_nvim) = (Rc::clone(&self.stand), Rc::clone(&self.nvim));
        self.steps.push(Step::new(
            name,
            timeout,
            move || ready(&ready_stand, &mut ready_nvim.borrow_mut()),
            move || act(&act_stand, &mut act_nvim.borrow_mut()),
        ));
    }

    fn act(
        &mut self,
        name: &'static str,
        act: impl FnOnce(&Stand, &mut Nvim) -> Result<(), String> + 'static,
    ) {
        self.step(name, ACT_TIMEOUT, |_, _| Poll::Ready(()), act);
    }

    fn wait(
        &mut self,
        name: &'static str,
        timeout: Duration,
        ready: impl FnMut(&Stand, &mut Nvim) -> Poll<()> + 'static,
    ) {
        self.step(name, timeout, ready, |_, nvim| nvim.take_error());
    }

    /// Клавиши через обычную доставку и ожидание их возврата из `sendEvent`.
    fn press(&mut self, name: &'static str, keys: Vec<Key>) {
        self.act(name, move |_, nvim| {
            let mut numbers = Vec::new();
            for key in keys {
                numbers.extend(events::press(key)?);
            }
            nvim.posted = numbers;
            Ok(())
        });
        self.wait(
            "события прошли sendEvent",
            STATE_TIMEOUT,
            |_, nvim| nvim.dispatched(),
        );
    }

    fn type_text(&mut self, name: &'static str, text: &str) -> Result<(), String> {
        let keys = keys_for(text)?;
        self.press(name, keys);
        Ok(())
    }

    /// Ожидание состояния nvim, прочитанного после начала шага.
    fn wait_nvim(
        &mut self,
        name: &'static str,
        timeout: Duration,
        accept: impl Fn(&NvimState) -> bool + 'static,
    ) {
        let since: Cell<Option<Instant>> = Cell::new(None);
        self.wait(name, timeout, move |_, nvim| {
            let after = since.get().unwrap_or_else(Instant::now);
            since.set(Some(after));
            match nvim.read_state(after, false) {
                Some(state) if accept(&state) => Poll::Ready(()),
                _ if nvim.error.is_some() => Poll::Ready(()),
                _ => Poll::Pending(nvim.describe()),
            }
        });
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Nvim);
    report.push("variant", &stand.prelude.variant);
    report.push(
        "route_note",
        "клавиши идут через postEvent и обычную доставку NSApplication, маршрутизатор режимов стоит в sendEvent; состояние nvim читает nvim --server --remote-expr в рабочем потоке",
    );
    report.push(
        "mode_note",
        "mode(1): t — Terminal mode, nt — Normal в буфере терминала; mode() без аргумента отдаёт только первую букву",
    );
    report.push(
        "manual_note",
        "настоящий агент, удобство аккорда и Ctrl-\\ Ctrl-o — ручная приёмка задачи 19",
    );
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let (bundle, problems) = window::verify_bundle();
    report.bundle = Some(bundle);
    if !problems.is_empty() {
        return stand.conclude(
            report,
            Err(format!("не бандл пробы: {}", problems.join("; "))),
        );
    }
    let path = std::env::var("PATH").unwrap_or_default();
    let Some(program) = find_program(PROGRAM, &path) else {
        return stand.conclude(
            report,
            Err(format!(
                "предусловие: {PROGRAM} не найден в PATH процесса пробы ({path})"
            )),
        );
    };
    report.push("nvim_program", program.display());
    let nvim = match prepare(&stand, &mut report, program) {
        Ok(nvim) => Rc::new(RefCell::new(nvim)),
        Err(reason) => {
            return surface_env::finish(&stand, report, Err(reason), None);
        }
    };
    let mut plan = Plan {
        stand: Rc::clone(&stand),
        nvim: Rc::clone(&nvim),
        steps: Vec::new(),
    };
    if let Err(reason) = build(&mut plan) {
        let view = nvim.borrow().view.clone();
        return surface_env::finish(&stand, report, Err(reason), Some(view));
    }
    let steps = plan.steps;
    run_steps(steps, move |result| {
        let verdict = result.map_err(|failure| failure.describe());
        if let Some(router) = router::uninstall() {
            report.push_json("router", &router);
        }
        let mut nvim = nvim.borrow_mut();
        nvim.remote.abandon();
        for (name, requirement, verdict, reason) in std::mem::take(&mut nvim.findings) {
            report.finding(&name, requirement, verdict, reason);
        }
        report.push_json("nvim_version", &nvim.version);
        report.push_json("nvim_pids", &nvim.pids);
        report.push_json("nvim_last_state", &nvim.last);
        report.push_json("nvim_written", &nvim.written);
        report.push("nvim_child_exits", nvim.child_exits);
        report.push_json("nvim_chord", &nvim.chord);
        report.push_json("nvim_sizes_before", &nvim.sizes_before);
        report.push_json("nvim_sizes_after", &nvim.sizes_after);
        report.push_json("nvim_remote_calls", &nvim.remote.calls);
        report.metric("nvim_remote_expr", "ms", nvim.remote.durations());
        report.push_json("view_input_log", &nvim.view.input_log());
        let view = nvim.view.clone();
        drop(nvim);
        surface_env::finish(&stand, report, verdict, Some(view));
    });
}

/// Runtime, каталоги, окружение и вид nvim в центральной зоне до создания поверхности.
fn prepare(stand: &Stand, report: &mut Report, program: PathBuf) -> Result<Nvim, String> {
    let runtime = surface_env::start_runtime(stand, report, ColorScheme::Light)?;
    let root = stand.prelude.out.join("nvim");
    let dirs = Dirs::under(&root);
    dirs.create()?;
    report.push_json("nvim_dirs", &dirs);
    let file = format!("note-{}.txt", stand.prelude.nonce);
    let spec = Spec {
        command: command(&program, &file),
        working_directory: dirs.work.display().to_string(),
        env: nvim_env(&dirs),
        wait_after_command: true,
    };
    report.push("surface_command", &spec.command);
    report.push_json("surface_env", &spec.env);
    let text = format!("nvim {}", stand.prelude.nonce);
    report.push("typed_text", &text);

    let mtm = stand.view.mtm();
    let scene = Rc::new(Scene::new(mtm, false));
    scene.install(&stand.view, &stand.window);
    let view = SurfaceView::new(mtm, NSRect::ZERO);
    scene.add_tab(Zone::Center, 0, "nvim", &view)?;
    scene.focus(&stand.window, &view);
    let id = runtime::add_surface(view.clone(), spec)?;
    let remote = Remote {
        program,
        dir: dirs.work.clone(),
        env: client_env(std::env::vars(), &dirs),
        inflight: None,
        calls: Vec::new(),
    };
    Ok(Nvim {
        _runtime: runtime,
        scene,
        view,
        id,
        file: dirs.work.join(&file),
        text,
        remote,
        state_args: state_args(),
        version: None,
        posted: Vec::new(),
        last: None,
        error: None,
        since: None,
        pids: Pids::default(),
        sizes_before: None,
        sizes_after: None,
        resized_at: None,
        marks: Marks::default(),
        chord: ChordRecord::default(),
        written: None,
        child_exits: 0,
        findings: Vec::new(),
    })
}

fn build(plan: &mut Plan) -> Result<(), String> {
    start(plan);
    write(plan)?;
    terminal(plan)?;
    chord(plan);
    resize(plan);
    quit(plan)?;
    close(plan);
    Ok(())
}

fn start(plan: &mut Plan) {
    let version_args: Vec<String> = vec!["--version".to_owned()];
    plan.wait("nvim --version", VERSION_TIMEOUT, move |_, nvim| {
        let Some(answer) = nvim.remote.ask(&version_args) else {
            return Poll::Pending("nvim --version не ответил".to_owned());
        };
        match answer.result {
            Ok(text) => nvim.version = text.lines().next().map(str::to_owned),
            Err(error) => {
                nvim.error = Some(format!("предусловие: nvim --version: {}", error.describe()))
            }
        }
        Poll::Ready(())
    });
    plan.wait(
        "активность, key window и фокус вида nvim",
        FOCUS_TIMEOUT,
        |stand, nvim| focused(stand, &nvim.view),
    );
    plan.step(
        "ненулевой размер вида в пикселях",
        SIZE_TIMEOUT,
        |_, nvim| {
            let pixels = nvim.view.pixel_size();
            poll(pixels.0 > 0 && pixels.1 > 0, || {
                format!("размер вида в пикселях {pixels:?}")
            })
        },
        |stand, nvim| {
            match runtime::create(nvim.id)? {
                Created::Live { key, new_ms } => {
                    events::note(format!("поверхность nvim {key} создана за {new_ms:.1} мс"));
                }
                other => return Err(format!("поверхность nvim не создана: {other:?}")),
            }
            router::install(Rc::clone(&nvim.scene), &stand.window);
            nvim.scene.sync_focus(&stand.window);
            Ok(())
        },
    );
    let started = Cell::new(None);
    plan.step(
        "nvim слушает сокет",
        READY_TIMEOUT,
        move |_, nvim| {
            let after = started.get().unwrap_or_else(Instant::now);
            started.set(Some(after));
            if !nvim.remote.dir.join(SOCKET_FILE).exists() {
                return Poll::Pending(format!("сокета {SOCKET} нет; {}", nvim.describe()));
            }
            match nvim.read_state(after, true) {
                Some(_) => Poll::Ready(()),
                None if nvim.error.is_some() => Poll::Ready(()),
                None => Poll::Pending(nvim.describe()),
            }
        },
        |_, nvim| {
            nvim.take_error()?;
            let state = nvim.last.clone().ok_or("нет состояния nvim")?;
            if state.mode != "n" {
                return Err(format!(
                    "предусловие: nvim после запуска не в Normal ({state:?}); {}",
                    nvim.describe()
                ));
            }
            let reading = runtime::read(nvim.id).ok_or("поверхность nvim не живая")?;
            let server = proc_info(state.pid);
            let tui = server.and_then(|server| proc_info(server.ppid));
            let rdev = reading.tty_name.as_deref().and_then(tty_rdev);
            nvim.pids = Pids {
                server_pid: Some(state.pid),
                server,
                tui,
                job: None,
                foreground_pid: Some(reading.foreground_pid),
                tty: reading.tty_name.clone(),
                tty_rdev: rdev,
            };
            let attached = tui.is_some_and(|tui| {
                rdev == Some(u64::from(tui.tdev))
                    && u64::try_from(tui.tpgid).ok() == Some(reading.foreground_pid)
            });
            let reason = format!(
                "{}; сервер {server:?}, TUI {tui:?}; ghostty_surface_tty_name {:?} (rdev {rdev:?}), ghostty_surface_foreground_pid {}",
                nvim.version.as_deref().unwrap_or("версия не прочитана"),
                reading.tty_name,
                reading.foreground_pid
            );
            nvim.finding("h12-nvim-start", attached, reason);
            let (surface, tty) = nvim.surface_and_tty()?;
            nvim.sizes_before = Some(Sizes {
                surface,
                tty,
                nvim: (state.columns, state.lines),
            });
            Ok(())
        },
    );
}

fn write(plan: &mut Plan) -> Result<(), String> {
    plan.type_text("i", "i")?;
    plan.wait_nvim("nvim в Insert", STATE_TIMEOUT, |state| state.mode == "i");
    let text = plan.nvim.borrow().text.clone();
    let mut keys = keys_for(&text)?;
    keys.push(KEY_ESC);
    plan.press("текст и Esc", keys);
    plan.wait_nvim("nvim в Normal", STATE_TIMEOUT, |state| state.mode == "n");
    let mut keys = keys_for(":w")?;
    keys.push(Key::RETURN);
    plan.press(":w", keys);
    plan.act("начало ожидания файла", |_, nvim| {
        nvim.since = Some(Instant::now());
        Ok(())
    });
    plan.wait(
        "файл записан или внутренний предел",
        CHECK_TIMEOUT,
        |_, nvim| {
            let expected = format!("{}\n", nvim.text);
            let content = fs::read(&nvim.file)
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
            let done = content.as_deref() == Some(expected.as_str());
            nvim.written = content;
            poll(done || nvim.limit_passed(), || {
                format!("файл {}: {:?}", nvim.file.display(), nvim.written)
            })
        },
    );
    plan.act("итог записи", |_, nvim| {
        let expected = format!("{}\n", nvim.text);
        let ok = nvim.written.as_deref() == Some(expected.as_str());
        let reason = format!(
            "напечатано {:?} с i, Esc и :w; файл {} содержит {:?}, ожидалось {expected:?}",
            nvim.text,
            nvim.file.display(),
            nvim.written
        );
        nvim.finding("h12-nvim-write", ok, reason);
        Ok(())
    });
    Ok(())
}

fn terminal(plan: &mut Plan) -> Result<(), String> {
    let mut keys = keys_for(&format!(":terminal {TERMINAL_COMMAND}"))?;
    keys.push(Key::RETURN);
    plan.press(":terminal cat", keys);
    plan.wait_nvim(
        "буфер терминала с заданием",
        STATE_TIMEOUT,
        |state| state.terminal && state.job_pid > 0 && state.mode == "nt",
    );
    plan.act("PID задания терминала", |_, nvim| {
        nvim.pids.job = nvim.last.as_ref().map(|state| state.job_pid);
        Ok(())
    });
    plan.type_text("i в буфере терминала", "i")?;
    plan.wait_nvim("nvim в Terminal mode", STATE_TIMEOUT, |state| {
        state.terminal && state.mode == "t"
    });
    Ok(())
}

fn chord(plan: &mut Plan) {
    plan.act("отметки до аккорда", |_, nvim| {
        nvim.marks = nvim.router_marks()?;
        nvim.chord.mode_before = nvim.last.as_ref().map(|state| state.mode.clone());
        Ok(())
    });
    plan.press("первый Ctrl-\\", vec![CHORD]);
    plan.act(
        "маршрутизатор после первого Ctrl-\\",
        |_, nvim| {
            let router = router::snapshot().ok_or("маршрутизатор не стоит")?;
            nvim.chord.router_after_first = Some((router.mode, router.waiting));
            let saved = router
                .saved_press
                .ok_or("нет сохранённого события первого Ctrl-\\")?;
            if saved.text.as_deref() != Some("\\") || saved.unshifted_codepoint != u32::from('\\') {
                return Err(format!(
                    "предусловие: латинская раскладка — клавиша 42 дала {:?}, unshifted {}",
                    saved.text, saved.unshifted_codepoint
                ));
            }
            Ok(())
        },
    );
    plan.wait_nvim("режим nvim после первого Ctrl-\\", STATE_TIMEOUT, |_| true);
    plan.act(
        "режим после первого Ctrl-\\ записан",
        |_, nvim| {
            nvim.chord.nvim_after_first = nvim.last.as_ref().map(|state| state.mode.clone());
            Ok(())
        },
    );
    plan.press("второй Ctrl-\\", vec![CHORD]);
    plan.act(
        "маршрутизатор после второго Ctrl-\\",
        |_, nvim| {
            let router = router::snapshot().ok_or("маршрутизатор не стоит")?;
            nvim.chord.router_after_second = Some((router.mode, router.waiting));
            Ok(())
        },
    );
    plan.press("Ctrl-n", vec![CTRL_N]);
    plan.act(
        "начало ожидания Normal в терминале",
        |_, nvim| {
            nvim.since = Some(Instant::now());
            Ok(())
        },
    );
    let since: Cell<Option<Instant>> = Cell::new(None);
    plan.wait(
        "nvim в Normal буфера терминала или внутренний предел",
        CHECK_TIMEOUT,
        move |_, nvim| {
            let after = since.get().unwrap_or_else(Instant::now);
            since.set(Some(after));
            let state = nvim.read_state(after, false);
            let normal = state.is_some_and(|state| state.mode == "nt");
            poll(
                normal || nvim.error.is_some() || nvim.limit_passed(),
                || nvim.describe(),
            )
        },
    );
    plan.act("итог аккорда в :terminal", |_, nvim| {
        let router = router::snapshot().ok_or("маршрутизатор не стоит")?;
        let marks = nvim.marks;
        let log = nvim.view.input_log();
        let chord = &mut nvim.chord;
        chord.replays = router.record.replays[marks.replays.min(router.record.replays.len())..]
            .iter()
            .map(|replay| (replay.delivered, replay.reason.clone(), replay.inputs.len()))
            .collect();
        let sent = &log.sent[marks.sent.min(log.sent.len())..];
        chord.replay_sent = sent
            .iter()
            .filter(|sent| sent.from == "chord-replay")
            .map(|sent| (sent.input.action, sent.input.keycode))
            .collect();
        chord.ctrl_n_sent = sent
            .iter()
            .filter(|sent| {
                sent.from != "chord-replay"
                    && sent.input.action == KeyAction::Press
                    && sent.input.keycode == u32::from(CTRL_N.code)
            })
            .count();
        chord.chord_arrivals = log.arrivals[marks.arrivals.min(log.arrivals.len())..]
            .iter()
            .filter(|arrival| arrival.key_code == CHORD.code)
            .count();
        chord.mode_after = nvim.last.as_ref().map(|state| state.mode.clone());
        let chord = nvim.chord.clone();
        let ok = chord.mode_before.as_deref() == Some("t")
            && chord.router_after_first == Some((Mode::Navigation, true))
            && chord.nvim_after_first.as_deref() == Some("t")
            && chord.router_after_second == Some((Mode::Input, false))
            && matches!(chord.replays.as_slice(), [(true, _, 2)])
            && chord.replay_sent == [(KeyAction::Press, 42), (KeyAction::Release, 42)]
            && chord.chord_arrivals == 0
            && chord.ctrl_n_sent == 1
            && chord.mode_after.as_deref() == Some("nt");
        let reason = format!(
            "mode(1) до аккорда {:?}, после первого Ctrl-\\ {:?}, после Ctrl-\\ Ctrl-\\ Ctrl-n {:?} (ожидались t, t, nt); маршрутизатор после первого {:?}, после второго {:?}; повторы (доставлен, причина, событий) {:?}; ghostty_surface_key из повтора {:?}; Ctrl-\\ у вида {}; Ctrl-n отправлено {}",
            chord.mode_before,
            chord.nvim_after_first,
            chord.mode_after,
            chord.router_after_first,
            chord.router_after_second,
            chord.replays,
            chord.replay_sent,
            chord.chord_arrivals,
            chord.ctrl_n_sent
        );
        nvim.finding("h4-nvim-terminal-chord", ok, reason);
        Ok(())
    });
}

fn resize(plan: &mut Plan) {
    plan.act(
        "сдвиг делителей центральной зоны",
        |_, nvim| {
            nvim.resized_at = None;
            let scene = &nvim.scene;
            for (divider, shift) in [
                (Divider::CenterRight, -RESIZE_SHIFT),
                (Divider::CenterBottom, -RESIZE_SHIFT),
            ] {
                let position = scene.divider(divider);
                scene.set_divider(divider, position + shift);
                events::note(format!(
                    "делитель {} {position:.1} → {:.1}",
                    divider.name(),
                    position + shift
                ));
            }
            nvim.since = Some(Instant::now());
            Ok(())
        },
    );
    plan.wait(
        "новая сетка у поверхности, PTY и nvim или внутренний предел",
        CHECK_TIMEOUT,
        |_, nvim| {
            let before = nvim.sizes_before.map(|sizes| sizes.surface);
            let (surface, tty) = match nvim.surface_and_tty() {
                Ok(sizes) => sizes,
                Err(reason) => {
                    nvim.error = Some(reason);
                    return Poll::Ready(());
                }
            };
            if Some(surface) == before || surface != tty {
                nvim.resized_at = None;
                return poll(nvim.limit_passed(), || {
                    format!("поверхность {surface:?}, PTY {tty:?}, до ресайза {before:?}")
                });
            }
            let after = *nvim.resized_at.get_or_insert_with(Instant::now);
            if let Some(state) = nvim.read_state(after, false) {
                let sizes = Sizes {
                    surface,
                    tty,
                    nvim: (state.columns, state.lines),
                };
                nvim.sizes_after = Some(sizes);
                if sizes.agree() {
                    return Poll::Ready(());
                }
            }
            poll(nvim.error.is_some() || nvim.limit_passed(), || {
                format!(
                    "поверхность {surface:?}, PTY {tty:?}, nvim {:?}",
                    nvim.last.as_ref().map(|state| (state.columns, state.lines))
                )
            })
        },
    );
    plan.act("итог ресайза", |_, nvim| {
        let before = nvim.sizes_before;
        let after = nvim.sizes_after;
        let ok = match (before, after) {
            (Some(before), Some(after)) => {
                before.agree()
                    && after.agree()
                    && after.surface.0 != before.surface.0
                    && after.surface.1 != before.surface.1
            }
            _ => false,
        };
        let reason = format!(
            "сетка (столбцы, строки) до ресайза {before:?}, после {after:?}: ghostty_surface_size, TIOCGWINSZ PTY и &columns/&lines nvim совпадают, обе оси меняются"
        );
        nvim.finding("h12-nvim-resize", ok, reason);
        Ok(())
    });
}

fn quit(plan: &mut Plan) -> Result<(), String> {
    let mut keys = keys_for(":qa!")?;
    keys.push(Key::RETURN);
    plan.press(":qa!", keys);
    plan.wait(
        "выход nvim, задания терминала и ребёнка поверхности",
        QUIT_TIMEOUT,
        |_, nvim| {
            let pids: Vec<i32> = [
                nvim.pids.server_pid,
                nvim.pids.tui.map(|info| info.pid),
                nvim.pids.job,
            ]
            .into_iter()
            .flatten()
            .collect();
            let alive: Vec<i32> = pids
                .iter()
                .copied()
                .filter(|pid| surface_env::process_alive(*pid))
                .collect();
            let exited = child_exits();
            poll(alive.is_empty() && exited >= 1, || {
                format!(
                    "живы {alive:?} из {pids:?}, child-exited {exited}; {}",
                    nvim.describe()
                )
            })
        },
    );
    plan.act(
        "выход ребёнка поверхности записан",
        |_, nvim| {
            nvim.child_exits = child_exits();
            Ok(())
        },
    );
    Ok(())
}

/// Сообщения `show_child_exited` от ghostty: ребёнок PTY поверхности вышел.
fn child_exits() -> u64 {
    runtime::stats()
        .and_then(|stats| stats.delivered.get("child-exited").copied())
        .unwrap_or(0)
}

fn close(plan: &mut Plan) {
    plan.act("два намерения закрыть", |_, nvim| {
        let first = runtime::request_close(nvim.id)?;
        let second = runtime::request_close(nvim.id)
            .map(|key| key.to_string())
            .unwrap_or_else(|reason| format!("отклонено: {reason}"));
        events::note(format!("намерения закрыть: {first}, {second}"));
        Ok(())
    });
    plan.wait(
        "освобождение поверхности",
        CLOSE_TIMEOUT,
        |_, nvim| {
            let phase = runtime::phase(nvim.id);
            poll(phase == Some(Phase::Freed), || {
                format!("поверхность {}", phase.map_or("нет", Phase::name))
            })
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_without_trailing_newline_is_taken_as_is() {
        assert_eq!(parse_reply(Some(0), b"nt", b""), Ok("nt".to_owned()));
        assert_eq!(
            parse_reply(Some(0), b"80x24\n", b""),
            Ok("80x24".to_owned())
        );
        assert_eq!(
            parse_reply(Some(0), b"a\nb\r\n", b""),
            Ok("a\nb".to_owned())
        );
        assert_eq!(parse_reply(Some(0), b"", b""), Ok(String::new()));
    }

    #[test]
    fn refused_connection_is_unreachable_and_other_errors_fail() {
        let refused = b"E247: Failed to connect to './nvim.sock': connection refused. Send expression failed.\n";
        assert!(matches!(
            parse_reply(Some(2), b"", refused),
            Err(RemoteError::Unreachable(reason)) if reason.contains("E247")
        ));
        let unknown = b"Lua: Vim:E117: Unknown function: nosuchfunc\nstack traceback:\n";
        assert!(matches!(
            parse_reply(Some(2), b"", unknown),
            Err(RemoteError::Failed(reason)) if reason.contains("E117") && !reason.contains("traceback")
        ));
        assert!(matches!(
            parse_reply(None, b"", b""),
            Err(RemoteError::Failed(reason)) if reason.contains("сигналом")
        ));
        assert!(matches!(
            parse_reply(Some(0), &[0xff, 0xfe], b""),
            Err(RemoteError::Failed(reason)) if reason.contains("UTF-8")
        ));
    }

    #[test]
    fn state_reply_is_parsed_by_fields() {
        assert_eq!(
            parse_state("nt 120 40 9123 1 9130"),
            Ok(NvimState {
                mode: "nt".to_owned(),
                columns: 120,
                lines: 40,
                pid: 9123,
                terminal: true,
                job_pid: 9130,
            })
        );
        assert_eq!(
            parse_state("n 80 24 77 0 0").map(|state| (state.mode, state.terminal)),
            Ok(("n".to_owned(), false))
        );
        assert!(parse_state("n 80 24 77 0").is_err());
        assert!(parse_state(" 80 24 77 0 0").is_err());
        assert!(parse_state("n 80 70000 77 0 0").is_err());
        assert!(parse_state("n 80 24 77 2 0").is_err());
        assert!(parse_state("table: 0x0100c9cc78").is_err());
    }

    #[test]
    fn xdg_directories_are_distinct_and_inside_the_scenario() {
        let root = Path::new("/tmp/probe/nvim");
        let dirs = Dirs::under(root);
        let env = nvim_env(&dirs);
        let names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
                "XDG_RUNTIME_DIR"
            ]
        );
        let mut paths: Vec<&str> = env.iter().map(|(_, path)| path.as_str()).collect();
        assert!(paths.iter().all(|path| Path::new(path).starts_with(root)));
        assert!(!paths.iter().any(|path| Path::new(path) == dirs.work));
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), XDG.len());
    }

    #[test]
    fn client_environment_is_built_from_scratch() {
        let dirs = Dirs::under(Path::new("/tmp/probe/nvim"));
        let inherited = [
            ("HOME", "/Users/owner"),
            ("PATH", "/opt/homebrew/bin:/usr/bin"),
            ("LANG", "en_US.UTF-8"),
            ("NVIM", "/tmp/outer.sock"),
            ("NVIM_LISTEN_ADDRESS", "/tmp/outer"),
            ("VIMINIT", "set nocp"),
            ("XDG_CONFIG_HOME", "/Users/owner/.config"),
            ("GHOSTTY_RESOURCES_DIR", "/Applications/Ghostty.app"),
            ("TERM", "xterm-ghostty"),
            ("SHELLIO_STATE_DIR", "/tmp/state"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));
        let env = client_env(inherited, &dirs);
        let get = |name: &str| {
            env.iter()
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(get("HOME"), ["/Users/owner"]);
        assert_eq!(get("PATH"), ["/opt/homebrew/bin:/usr/bin"]);
        assert_eq!(get("LANG"), ["en_US.UTF-8"]);
        assert_eq!(get("XDG_CONFIG_HOME"), ["/tmp/probe/nvim/xdg/config"]);
        for name in [
            "NVIM",
            "NVIM_LISTEN_ADDRESS",
            "VIMINIT",
            "GHOSTTY_RESOURCES_DIR",
            "TERM",
            "SHELLIO_STATE_DIR",
        ] {
            assert!(get(name).is_empty(), "{name} не должен попасть клиенту");
        }
        for (name, value) in nvim_env(&dirs) {
            assert_eq!(get(&name), [value.as_str()]);
        }
    }

    #[test]
    fn surface_command_quotes_every_word() {
        let command = command(Path::new("/opt/homebrew/bin/nvim"), "note-ab12.txt");
        assert_eq!(
            command,
            "'/opt/homebrew/bin/nvim' '--clean' '--listen' './nvim.sock' '--cmd' 'set shell=/bin/sh' 'note-ab12.txt'"
        );
    }

    #[test]
    fn text_keys_cover_typed_text_and_commands() {
        let keys = keys_for(":qa!").unwrap();
        let codes: Vec<(u16, &str, bool)> = keys
            .iter()
            .map(|key| {
                (
                    key.code,
                    key.characters,
                    key.flags.contains(NSEventModifierFlags::Shift),
                )
            })
            .collect();
        assert_eq!(
            codes,
            [
                (41, ":", true),
                (12, "q", false),
                (0, "a", false),
                (18, "!", true)
            ]
        );
        assert!(keys_for("nvim 0123456789abcdef").is_ok());
        assert!(keys_for(":terminal cat").is_ok());
        assert!(keys_for("Ж").is_err());
        let mut codes: Vec<(u16, bool)> = TEXT_KEYS
            .iter()
            .map(|key| (key.code, key.flags.contains(NSEventModifierFlags::Shift)))
            .collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), TEXT_KEYS.len(), "у двух символов одна клавиша");
    }

    #[test]
    fn program_is_found_by_absolute_path_entries_only() {
        assert_eq!(
            find_program("sh", "relative:/bin"),
            Some(PathBuf::from("/bin/sh"))
        );
        assert_eq!(find_program("sh", "bin"), None);
        assert_eq!(find_program("no-such-program-probe", "/bin:/usr/bin"), None);
    }
}
