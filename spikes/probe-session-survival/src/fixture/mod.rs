//! Фикстуры — программы нагрузки в PTY кандидата. Квитанции идут по отдельному сокету
//! ([`channel`]), а не через PTY. Экран по `SIGWINCH` не перерисовывается: иначе полный redraw
//! скрыл бы потери при восстановлении. Сигналы заблокированы во всех потоках и принимаются
//! `sigwait`, как в P0.3.
//!
//! Режимы:
//! - `shell` — построчный интерпретатор с управлением заданиями в каноническом режиме PTY;
//! - `agent` — долгий процесс с фоновым потомком `nohup-child`;
//! - `nohup-child` — процесс без PTY, игнорирующий `SIGHUP`, живёт до своего срока;
//! - `emitter`, `query`, `flood`, `slow-reader` — raw-режим, вывод и чтение по командам стенда;
//!   по команде `echo` прочитанное отражается обратно в PTY (задержка эха в замерах);
//! - `replay` — фиктивный attach для поверхности: по команде отдаёт байты сцены из файла, затем
//!   живой поток команд стенда, оба через [`ReportFilter`] при `--filter`, и квитирует каждую
//!   запись тем, что ушло в PTY.
//!
//! Фикстура завершается по закрытию PTY, `SIGHUP`, `SIGTERM`, команде и своему сроку. Задания
//! `shell` получают `SIGHUP` при выходе; потомок `agent` живёт до собственного срока.

pub mod channel;
pub mod query;

use std::collections::BTreeMap;
use std::ffi::c_int;
use std::fs::File;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::bridge::ReportFilter;
use crate::matrix::Scenario;
use crate::pty as ptys;
use crate::{EXIT_EXECUTION, EXIT_USAGE, sys};
use channel::{
    Command, Event, JobChange, JobView, Ready, Receipt, Request, WaitStatus, WriteStats, hex, unhex,
};
use query::ResponseParser;

pub const FIXTURE_NAME: &str = "probe-session-fixture";
const LIFETIME_MARGIN: Duration = Duration::from_secs(30);
const MAX_LIFETIME: Duration = Duration::from_secs(3600);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const READ_BUF: usize = 4096;
const MAX_FLOOD_WIDTH: u16 = 4096;
const MAX_FLOOD_LINES: u64 = 100_000_000;
const MAX_INTERVAL_US: u64 = 10_000_000;
const MAX_PENDING_SPLITS: usize = 1024;
/// Сигналы, которые фикстура принимает `sigwait`. `SIGTTOU` и `SIGTTIN` в наборе, чтобы
/// `tcsetpgrp` и чтение из фоновой группы не останавливали процесс.
const SIGNALS: [c_int; 10] = [
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTSTP,
    libc::SIGCONT,
    libc::SIGWINCH,
    libc::SIGHUP,
    libc::SIGTERM,
    libc::SIGCHLD,
    libc::SIGTTIN,
    libc::SIGTTOU,
];

/// Режим фикстуры.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Shell,
    Agent,
    NohupChild,
    Emitter,
    Query,
    Flood,
    SlowReader,
    Replay,
}

impl Mode {
    pub const ALL: [Mode; 8] = [
        Mode::Shell,
        Mode::Agent,
        Mode::NohupChild,
        Mode::Emitter,
        Mode::Query,
        Mode::Flood,
        Mode::SlowReader,
        Mode::Replay,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Shell => "shell",
            Mode::Agent => "agent",
            Mode::NohupChild => "nohup-child",
            Mode::Emitter => "emitter",
            Mode::Query => "query",
            Mode::Flood => "flood",
            Mode::SlowReader => "slow-reader",
            Mode::Replay => "replay",
        }
    }

    pub fn from_name(name: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|mode| mode.name() == name)
    }

    /// Режим работает в PTY. `nohup-child` запускается без терминала.
    pub fn has_pty(self) -> bool {
        self != Mode::NohupChild
    }

    /// Терминал в raw-режиме до готовности: байты проходят без эха и сигналов от клавиш.
    pub fn raw(self) -> bool {
        matches!(
            self,
            Mode::Emitter | Mode::Query | Mode::Flood | Mode::SlowReader | Mode::Replay
        )
    }
}

// ---- аргументы ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    pub mode: Mode,
    pub channel: PathBuf,
    pub nonce: String,
    pub lifetime: Duration,
    /// Начальный темп чтения `slow-reader`, байт в секунду.
    pub rate: Option<u64>,
    /// Файл байтов сцены `replay`.
    pub scene: Option<PathBuf>,
    /// `replay` пропускает свой вывод через фильтр режимов отчётов.
    pub filter: bool,
}

pub const USAGE: &str = "\
probe-session-fixture --mode <режим> --channel <сокет> --nonce <строка> [--lifetime-ms <мс>] [--rate <байт/с>]
                      [--scene <файл> [--filter]]
режимы: shell, agent, nohup-child, emitter, query, flood, slow-reader, replay; --scene обязателен у
replay и только у него";

pub fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut mode = None;
    let mut channel = None;
    let mut nonce = None;
    let mut lifetime = None;
    let mut rate = None;
    let mut scene = None;
    let mut filter = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg}: нет значения"))
        };
        match arg.as_str() {
            "--mode" => {
                let name = value()?;
                mode = Some(Mode::from_name(&name).ok_or_else(|| format!("режим {name}"))?);
            }
            "--channel" => channel = Some(PathBuf::from(value()?)),
            "--nonce" => nonce = Some(value()?),
            "--lifetime-ms" => {
                let ms: u64 = value()?
                    .parse()
                    .map_err(|err| format!("--lifetime-ms: {err}"))?;
                let lifetime_value = Duration::from_millis(ms);
                if lifetime_value.is_zero() || lifetime_value > MAX_LIFETIME {
                    return Err(format!(
                        "--lifetime-ms вне (0, {}]",
                        MAX_LIFETIME.as_millis()
                    ));
                }
                lifetime = Some(lifetime_value);
            }
            "--rate" => rate = Some(value()?.parse().map_err(|err| format!("--rate: {err}"))?),
            "--scene" => scene = Some(PathBuf::from(value()?)),
            "--filter" => filter = true,
            other => return Err(format!("неизвестный аргумент {other}")),
        }
    }
    let nonce = nonce.ok_or("нет --nonce")?;
    if nonce.is_empty() || nonce.chars().any(char::is_whitespace) {
        return Err("--nonce пустой или с пробелами".to_owned());
    }
    let mode = mode.ok_or("нет --mode")?;
    if (mode == Mode::Replay) != scene.is_some() {
        return Err("--scene обязателен у replay и только у него".to_owned());
    }
    if filter && mode != Mode::Replay {
        return Err("--filter только у replay".to_owned());
    }
    Ok(Args {
        mode,
        channel: channel.ok_or("нет --channel")?,
        nonce,
        lifetime: lifetime.unwrap_or_else(default_lifetime),
        rate,
        scene,
        filter,
    })
}

/// Срок по умолчанию переживает самый долгий сценарий: процесс сценария отсчитывает свой раньше.
pub fn default_lifetime() -> Duration {
    Scenario::ALL
        .into_iter()
        .map(Scenario::deadline)
        .max()
        .unwrap_or_default()
        + LIFETIME_MARGIN
}

/// Аргументы запуска фикстуры для стенда.
pub fn command_args(
    mode: Mode,
    channel: &Path,
    nonce: &str,
    lifetime: Option<Duration>,
) -> Vec<String> {
    let mut args = vec![
        "--mode".to_owned(),
        mode.name().to_owned(),
        "--channel".to_owned(),
        channel.display().to_string(),
        "--nonce".to_owned(),
        nonce.to_owned(),
    ];
    if let Some(lifetime) = lifetime {
        args.push("--lifetime-ms".to_owned());
        args.push(lifetime.as_millis().max(1).to_string());
    }
    args
}

// ---- чистые части ----

/// Строка потока `flood`: nonce, номер, заполнение до ширины и `\r\n`. Номер по строке
/// восстанавливается [`flood_index`].
pub fn flood_line(nonce: &str, index: u64, width: u16) -> Vec<u8> {
    let mut line = format!("{nonce} {index:010} ").into_bytes();
    let width = usize::from(width);
    if line.len() < width {
        let fill = b'a' + (index % 26) as u8;
        line.resize(width, fill);
    }
    line.extend_from_slice(b"\r\n");
    line
}

/// Номер строки `flood` или `None`, если строка не от этого nonce.
pub fn flood_index(nonce: &str, line: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(line).ok()?;
    let rest = text.strip_prefix(nonce)?.strip_prefix(' ')?;
    rest.get(..10)?.parse().ok()
}

/// Сколько ждать после чтения `bytes` байт при темпе `rate`, чтобы средний темп не превысил его.
pub fn pace(bytes: usize, rate: u64) -> Duration {
    if rate == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos((bytes as u64).saturating_mul(1_000_000_000) / rate)
}

/// Строка `shell`: `run <программа> [аргументы] [&]`, `fg <n>`, `bg <n>`, `jobs`, `exit [код]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellLine {
    Empty,
    Run {
        program: String,
        args: Vec<String>,
        background: bool,
    },
    Fg(u32),
    Bg(u32),
    Jobs,
    Exit(u8),
}

pub fn parse_shell_line(line: &str) -> Result<ShellLine, String> {
    let mut words: Vec<&str> = line.split_whitespace().collect();
    let Some(first) = words.first().copied() else {
        return Ok(ShellLine::Empty);
    };
    let job = |words: &[&str]| -> Result<u32, String> {
        match words {
            [_, n] => n
                .trim_start_matches('%')
                .parse()
                .map_err(|_| format!("задание {n}")),
            _ => Err(format!("{first}: нужен номер задания")),
        }
    };
    match first {
        "run" => {
            let background = words.last() == Some(&"&");
            if background {
                words.pop();
            }
            let Some(program) = words.get(1) else {
                return Err("run: нет программы".to_owned());
            };
            Ok(ShellLine::Run {
                program: (*program).to_owned(),
                args: words[2..].iter().map(|word| (*word).to_owned()).collect(),
                background,
            })
        }
        "fg" => job(&words).map(ShellLine::Fg),
        "bg" => job(&words).map(ShellLine::Bg),
        "jobs" if words.len() == 1 => Ok(ShellLine::Jobs),
        "exit" => match words.as_slice() {
            [_] => Ok(ShellLine::Exit(0)),
            [_, code] => code
                .parse()
                .map(ShellLine::Exit)
                .map_err(|_| format!("код {code}")),
            _ => Err("exit: лишние слова".to_owned()),
        },
        other => Err(format!("неизвестная команда {other}")),
    }
}

pub fn signal_name(signal: c_int) -> String {
    match signal {
        libc::SIGINT => "SIGINT".to_owned(),
        libc::SIGQUIT => "SIGQUIT".to_owned(),
        libc::SIGTSTP => "SIGTSTP".to_owned(),
        libc::SIGCONT => "SIGCONT".to_owned(),
        libc::SIGWINCH => "SIGWINCH".to_owned(),
        libc::SIGHUP => "SIGHUP".to_owned(),
        libc::SIGTERM => "SIGTERM".to_owned(),
        libc::SIGCHLD => "SIGCHLD".to_owned(),
        libc::SIGTTIN => "SIGTTIN".to_owned(),
        libc::SIGTTOU => "SIGTTOU".to_owned(),
        libc::SIGKILL => "SIGKILL".to_owned(),
        libc::SIGSTOP => "SIGSTOP".to_owned(),
        other => format!("сигнал {other}"),
    }
}

// ---- процесс ----

/// Почему фикстура завершилась.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    PtyClosed(String),
    Hangup,
    Terminated,
    ChannelClosed,
    ChannelError(String),
    Timeout,
    Command(u8),
}

impl StopReason {
    pub fn name(&self) -> &'static str {
        match self {
            StopReason::PtyClosed(_) => "pty-closed",
            StopReason::Hangup => "hangup",
            StopReason::Terminated => "terminated",
            StopReason::ChannelClosed => "channel-closed",
            StopReason::ChannelError(_) => "channel-error",
            StopReason::Timeout => "timeout",
            StopReason::Command(_) => "command",
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            StopReason::PtyClosed(detail) | StopReason::ChannelError(detail) => {
                Some(detail.clone())
            }
            _ => None,
        }
    }

    pub fn code(&self) -> u8 {
        match self {
            StopReason::PtyClosed(_)
            | StopReason::Hangup
            | StopReason::Terminated
            | StopReason::ChannelClosed => 0,
            StopReason::ChannelError(_) | StopReason::Timeout => EXIT_EXECUTION,
            StopReason::Command(code) => *code,
        }
    }
}

enum Input {
    Pty { t_ns: u64, bytes: Vec<u8> },
    PtyClosed(String),
    Request(Request),
    Corrupt(String),
    ChannelClosed,
    ChannelError(String),
    Signal { t_ns: u64, signal: c_int },
}

/// Общее состояние потока чтения PTY и цикла: темп и разрешение читать после возврата
/// терминала `shell`.
#[derive(Default)]
struct Gate {
    rate: Option<u64>,
    /// Растёт, когда `shell` забирает терминал обратно у задания.
    foreground_epoch: u64,
    stop: bool,
}

type SharedGate = Arc<(Mutex<Gate>, Condvar)>;

struct Job {
    number: u32,
    pid: i32,
    command: String,
    stopped: bool,
    foreground: bool,
}

struct Flood {
    id: u64,
    total: u64,
    done: u64,
    width: u16,
    interval: Duration,
    report_every: u64,
    started: Instant,
    bytes: u64,
}

impl Flood {
    fn due(&self) -> Instant {
        let offset = u32::try_from(self.done).map_or(Duration::MAX, |done| self.interval * done);
        self.started.checked_add(offset).unwrap_or(self.started)
    }
}

struct Fixture {
    mode: Mode,
    nonce: String,
    channel: UnixStream,
    pty: Option<OwnedFd>,
    output: Option<File>,
    gate: SharedGate,
    seq: u64,
    reads: u64,
    responses: u64,
    parser: ResponseParser,
    deadline: Instant,
    writes: WriteStats,
    flood: Option<Flood>,
    splits: BTreeMap<u64, Vec<u8>>,
    line: Vec<u8>,
    jobs: Vec<Job>,
    next_job: u32,
    child: Option<i32>,
    raw: bool,
    saved_termios: Option<libc::termios>,
    /// Байты сцены `replay`, прочитанные до готовности.
    scene: Option<Vec<u8>>,
    filter: Option<ReportFilter>,
    echo: bool,
}

/// Вход бинаря фикстуры.
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&args) {
        Ok(args) => args,
        Err(reason) => {
            let _ = writeln!(io::stderr(), "{FIXTURE_NAME}: {reason}\n{USAGE}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if args.mode == Mode::NohupChild {
        // До блокировки: заблокированный и игнорируемый сигнал отбрасывается.
        // SAFETY: смена действия сигнала без указателей на данные Rust.
        unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
    }
    let signals = match block_signals(args.mode) {
        Ok(signals) => signals,
        Err(reason) => return fail(&reason),
    };
    let channel = match channel::connect(&args.channel) {
        Ok(stream) => stream,
        Err(reason) => return fail(&reason),
    };
    match serve(args, channel, signals) {
        Ok(reason) => ExitCode::from(reason.code()),
        Err(reason) => fail(&reason),
    }
}

/// Ошибка до готовности. stderr фикстуры — PTY кандидата, поэтому текст короткий.
fn fail(reason: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "{FIXTURE_NAME}: {reason}");
    ExitCode::from(EXIT_EXECUTION)
}

fn serve(args: Args, channel: UnixStream, signals: libc::sigset_t) -> Result<StopReason, String> {
    let (tx, rx) = mpsc::channel();
    spawn_signal_waiter(signals, tx.clone()).map_err(|err| format!("поток сигналов: {err}"))?;
    channel
        .set_write_timeout(Some(SEND_TIMEOUT))
        .map_err(|err| format!("срок записи в сокет: {err}"))?;
    let reader = channel
        .try_clone()
        .map_err(|err| format!("сокет не дублируется: {err}"))?;
    let (pty, output) = if args.mode.has_pty() {
        let input = dup(libc::STDIN_FILENO)?;
        let output = open_writer(input.as_raw_fd())?;
        (Some(input), Some(output))
    } else {
        (None, None)
    };
    let scene = match &args.scene {
        Some(path) => {
            Some(std::fs::read(path).map_err(|err| format!("сцена {}: {err}", path.display()))?)
        }
        None => None,
    };
    let gate: SharedGate = Arc::new((
        Mutex::new(Gate {
            rate: args.rate,
            ..Gate::default()
        }),
        Condvar::new(),
    ));
    let mut fixture = Fixture {
        mode: args.mode,
        nonce: args.nonce,
        channel,
        pty,
        output,
        gate: Arc::clone(&gate),
        seq: 0,
        reads: 0,
        responses: 0,
        parser: ResponseParser::default(),
        deadline: Instant::now() + args.lifetime,
        writes: WriteStats::default(),
        flood: None,
        splits: BTreeMap::new(),
        line: Vec::new(),
        jobs: Vec::new(),
        next_job: 1,
        child: None,
        raw: false,
        saved_termios: None,
        scene,
        filter: args.filter.then(ReportFilter::new),
        echo: false,
    };
    if fixture.mode.raw() {
        fixture.set_raw()?;
    }
    if let Err(reason) = fixture.send(sys::uptime_ns(), Event::Ready(fixture.ready())) {
        return Ok(reason);
    }
    if fixture.mode == Mode::Agent
        && let Err(reason) = fixture.spawn_child(&args.channel)
    {
        return Ok(reason);
    }
    if let Some(pty) = &fixture.pty {
        let fd = pty
            .try_clone()
            .map_err(|err| format!("PTY не дублируется: {err}"))?;
        spawn_pty_reader(fd, gate, tx.clone()).map_err(|err| format!("поток PTY: {err}"))?;
    }
    spawn_channel_reader(reader, tx).map_err(|err| format!("поток сокета: {err}"))?;
    Ok(fixture.run(&rx))
}

impl Fixture {
    fn run(&mut self, rx: &Receiver<Input>) -> StopReason {
        let reason = loop {
            if let Err(reason) = self.step_flood() {
                break reason;
            }
            let now = Instant::now();
            if now >= self.deadline {
                break StopReason::Timeout;
            }
            let wake = self
                .flood
                .as_ref()
                .map_or(self.deadline, |flood| flood.due().min(self.deadline));
            match rx.recv_timeout(wake.saturating_duration_since(now)) {
                Ok(input) => {
                    if let Err(reason) = self.handle(input) {
                        break reason;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break StopReason::ChannelClosed,
            }
        };
        self.finish(&reason);
        reason
    }

    fn handle(&mut self, input: Input) -> Result<(), StopReason> {
        match input {
            Input::Pty { t_ns, bytes } => self.on_read(t_ns, &bytes),
            Input::PtyClosed(detail) => Err(StopReason::PtyClosed(detail)),
            Input::Request(request) => self.on_request(request),
            Input::Corrupt(error) => self.send(sys::uptime_ns(), Event::Protocol { error }),
            Input::ChannelClosed => Err(StopReason::ChannelClosed),
            Input::ChannelError(detail) => Err(StopReason::ChannelError(detail)),
            Input::Signal { t_ns, signal } => self.on_signal(t_ns, signal),
        }
    }

    fn on_signal(&mut self, t_ns: u64, signal: c_int) -> Result<(), StopReason> {
        match signal {
            libc::SIGWINCH => {
                let size = self
                    .pty
                    .as_ref()
                    .and_then(|fd| ptys::winsize(fd.as_fd()).ok());
                self.send(t_ns, Event::Winch { size })
            }
            libc::SIGHUP => Err(StopReason::Hangup),
            libc::SIGTERM => Err(StopReason::Terminated),
            libc::SIGCHLD => self.reap(),
            other => self.send(
                t_ns,
                Event::Signal {
                    signal: signal_name(other),
                },
            ),
        }
    }

    fn on_read(&mut self, t_ns: u64, bytes: &[u8]) -> Result<(), StopReason> {
        self.reads += 1;
        if self.echo {
            self.write_all(bytes)?;
        }
        self.send(
            t_ns,
            Event::Read {
                index: self.reads,
                hex: hex(bytes),
            },
        )?;
        for (response, raw) in self.parser.feed(bytes) {
            self.responses += 1;
            self.send(
                sys::uptime_ns(),
                Event::Response {
                    index: self.responses,
                    response,
                    hex: hex(&raw),
                },
            )?;
        }
        if self.mode == Mode::Shell {
            self.line.extend_from_slice(bytes);
            while let Some(end) = self.line.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.line.drain(..=end).collect();
                let text = String::from_utf8_lossy(&line).trim().to_owned();
                self.run_shell_line(&text)?;
            }
        }
        Ok(())
    }

    fn on_request(&mut self, request: Request) -> Result<(), StopReason> {
        let Request { id, command } = request;
        if let Command::Exit { code } = command {
            self.ack(id, Ok(0))?;
            return Err(StopReason::Command(code));
        }
        let outcome = self.apply(id, command)?;
        self.ack(id, outcome)
    }

    fn ack(&mut self, id: u64, outcome: Result<u64, String>) -> Result<(), StopReason> {
        let (bytes, error) = match outcome {
            Ok(bytes) => (bytes, None),
            Err(error) => (0, Some(error)),
        };
        self.send(sys::uptime_ns(), Event::Ack { id, bytes, error })
    }

    /// Внешний `Err` останавливает фикстуру, внутренний — отказ команды в квитанции.
    fn apply(&mut self, id: u64, command: Command) -> Result<Result<u64, String>, StopReason> {
        match command {
            Command::Write { hex } => match unhex(&hex) {
                Some(bytes) if self.mode == Mode::Replay => self.emit(id, "live", &bytes),
                Some(bytes) => self.write_counted(&bytes),
                None => Ok(Err("hex не разобран".to_owned())),
            },
            Command::Replay { prefix } => {
                let (Some(prefix), Some(scene)) = (unhex(&prefix), self.scene.clone()) else {
                    return Ok(Err("нет сцены или hex префикса не разобран".to_owned()));
                };
                let mut bytes = prefix;
                bytes.extend_from_slice(&scene);
                self.emit(id, "scene", &bytes)
            }
            Command::Flood {
                lines,
                width,
                interval_us,
                report_every,
            } => Ok(self.start_flood(id, lines, width, interval_us, report_every)),
            Command::StopFlood => match self.flood.take() {
                Some(flood) => {
                    self.flood_done(&flood, true)?;
                    Ok(Ok(0))
                }
                None => Ok(Err("потока нет".to_owned())),
            },
            Command::Query { query, split } => {
                let bytes = query.bytes();
                let Some(split) = split else {
                    if self.mode == Mode::Replay {
                        return self.emit(id, "query", &bytes);
                    }
                    return self.write_counted(&bytes);
                };
                if self.mode == Mode::Replay {
                    return Ok(Err("replay не разрезает запросы".to_owned()));
                }
                let at = usize::try_from(split).unwrap_or(usize::MAX);
                if at == 0 || at >= bytes.len() {
                    return Ok(Err(format!("разрез {split} вне (0, {})", bytes.len())));
                }
                if self.splits.len() >= MAX_PENDING_SPLITS {
                    return Ok(Err("слишком много незаконченных запросов".to_owned()));
                }
                self.splits.insert(id, bytes[at..].to_vec());
                self.write_counted(&bytes[..at])
            }
            Command::Rest { query } => match self.splits.remove(&query) {
                Some(rest) => self.write_counted(&rest),
                None => Ok(Err(format!("нет незаконченного запроса {query}"))),
            },
            Command::ReadRate { bytes_per_sec } => {
                if self.pty.is_none() {
                    return Ok(Err("у режима нет PTY".to_owned()));
                }
                let (lock, wake) = &*self.gate;
                lock.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .rate = bytes_per_sec;
                wake.notify_all();
                Ok(Ok(0))
            }
            Command::Lifetime { ms } => {
                let lifetime = Duration::from_millis(ms);
                if lifetime > MAX_LIFETIME {
                    return Ok(Err(format!("срок больше {} мс", MAX_LIFETIME.as_millis())));
                }
                self.deadline = Instant::now() + lifetime;
                Ok(Ok(0))
            }
            Command::Echo { on } => {
                if !self.raw {
                    return Ok(Err("эхо только в raw-режиме".to_owned()));
                }
                self.echo = on;
                Ok(Ok(0))
            }
            Command::Exit { .. } => Ok(Ok(0)),
        }
    }

    fn start_flood(
        &mut self,
        id: u64,
        lines: u64,
        width: u16,
        interval_us: u64,
        report_every: u64,
    ) -> Result<u64, String> {
        if self.output.is_none() {
            return Err("у режима нет PTY".to_owned());
        }
        if self.flood.is_some() {
            return Err("поток уже идёт".to_owned());
        }
        if lines == 0 || lines > MAX_FLOOD_LINES {
            return Err(format!("строк вне (0, {MAX_FLOOD_LINES}]"));
        }
        if width > MAX_FLOOD_WIDTH || interval_us > MAX_INTERVAL_US || report_every == 0 {
            return Err("ширина, интервал или шаг квитанций вне пределов".to_owned());
        }
        self.flood = Some(Flood {
            id,
            total: lines,
            done: 0,
            width,
            interval: Duration::from_micros(interval_us),
            report_every,
            started: Instant::now(),
            bytes: 0,
        });
        Ok(0)
    }

    /// Строки, срок которых наступил. Без интервала поток идёт пачками, чтобы цикл успевал
    /// принимать команды и сигналы.
    fn step_flood(&mut self) -> Result<(), StopReason> {
        const BATCH: u64 = 256;
        let Some(mut flood) = self.flood.take() else {
            return Ok(());
        };
        let mut sent = 0;
        while flood.done < flood.total && flood.due() <= Instant::now() && sent < BATCH {
            flood.done += 1;
            let line = flood_line(&self.nonce, flood.done, flood.width);
            flood.bytes += line.len() as u64;
            self.write_all(&line)?;
            sent += 1;
            if flood.done.is_multiple_of(flood.report_every) && flood.done < flood.total {
                self.send(
                    sys::uptime_ns(),
                    Event::FloodProgress {
                        id: flood.id,
                        lines: flood.done,
                        bytes: flood.bytes,
                        writes: self.writes,
                    },
                )?;
            }
        }
        if flood.done == flood.total {
            return self.flood_done(&flood, false);
        }
        self.flood = Some(flood);
        Ok(())
    }

    fn flood_done(&mut self, flood: &Flood, cancelled: bool) -> Result<(), StopReason> {
        self.send(
            sys::uptime_ns(),
            Event::FloodDone {
                id: flood.id,
                lines: flood.done,
                bytes: flood.bytes,
                cancelled,
                writes: self.writes,
            },
        )
    }

    /// Вывод `replay`: через фильтр, если он включён, затем квитанция того, что ушло в PTY, и
    /// сама запись.
    fn emit(
        &mut self,
        id: u64,
        source: &str,
        bytes: &[u8],
    ) -> Result<Result<u64, String>, StopReason> {
        let (out, removed) = match self.filter.as_mut() {
            Some(filter) => {
                let before = filter.removed();
                let out = filter.push(bytes);
                (out, (filter.removed() - before) as u64)
            }
            None => (bytes.to_vec(), 0),
        };
        self.send(
            sys::uptime_ns(),
            Event::Emitted {
                id,
                source: source.to_owned(),
                input: bytes.len() as u64,
                hex: hex(&out),
                removed,
            },
        )?;
        self.write_counted(&out)
    }

    fn write_counted(&mut self, bytes: &[u8]) -> Result<Result<u64, String>, StopReason> {
        if self.output.is_none() {
            return Ok(Err("у режима нет PTY".to_owned()));
        }
        self.write_all(bytes)?;
        Ok(Ok(bytes.len() as u64))
    }

    /// Запись в PTY без блокировки цикла навсегда: при `EAGAIN` ждёт готовности дескриптора до
    /// срока фикстуры и считает задержку.
    fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), StopReason> {
        let Some(output) = self.output.as_mut() else {
            return Ok(());
        };
        let mut stalled_at: Option<Instant> = None;
        while !bytes.is_empty() {
            match output.write(bytes) {
                Ok(0) => return Err(StopReason::PtyClosed("запись: 0 байт".to_owned())),
                Ok(written) => {
                    self.writes.bytes += written as u64;
                    bytes = &bytes[written..];
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    if stalled_at.is_none() {
                        self.writes.stalls += 1;
                        stalled_at = Some(Instant::now());
                    }
                    let left = self.deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(StopReason::Timeout);
                    }
                    ptys::wait_ready(output.as_fd(), true, Some(left))
                        .map_err(|err| StopReason::PtyClosed(format!("ожидание записи: {err}")))?;
                }
                Err(err) => return Err(StopReason::PtyClosed(format!("запись: {err}"))),
            }
            if let Some(at) = stalled_at.take() {
                self.writes.stalled_ns += at.elapsed().as_nanos() as u64;
            }
        }
        Ok(())
    }

    fn send(&mut self, t_ns: u64, event: Event) -> Result<(), StopReason> {
        self.seq += 1;
        let frame = channel::encode(&Receipt {
            seq: self.seq,
            t_ns,
            event,
        })
        .map_err(StopReason::ChannelError)?;
        self.channel
            .write_all(&frame)
            .map_err(|err| match err.kind() {
                io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::NotConnected => StopReason::ChannelClosed,
                _ => StopReason::ChannelError(format!("запись квитанции: {err}")),
            })
    }

    fn ready(&self) -> Ready {
        // SAFETY: вызовы без указателей.
        let (ppid, pgid, sid) = unsafe { (libc::getppid(), libc::getpgrp(), libc::getsid(0)) };
        let fd = self.pty.as_ref();
        Ready {
            nonce: self.nonce.clone(),
            mode: self.mode,
            pid: std::process::id() as i32,
            ppid,
            pgid,
            sid,
            foreground_pgid: fd.and_then(|fd| ptys::foreground_pgid(fd.as_fd()).ok()),
            tty: fd.and_then(|fd| ptys::tty_name(fd.as_raw_fd()).ok()),
            size: fd.and_then(|fd| ptys::winsize(fd.as_fd()).ok()),
            raw: self.raw,
        }
    }

    fn set_raw(&mut self) -> Result<(), String> {
        let Some(fd) = self.pty.as_ref() else {
            return Ok(());
        };
        let saved = ptys::get_termios(fd.as_fd()).map_err(|err| format!("tcgetattr: {err}"))?;
        ptys::set_termios(fd.as_fd(), &ptys::raw_mode(&saved))
            .map_err(|err| format!("tcsetattr: {err}"))?;
        self.saved_termios = Some(saved);
        self.raw = true;
        Ok(())
    }

    fn foreground_pgid(&self) -> Option<i32> {
        self.pty
            .as_ref()
            .and_then(|fd| ptys::foreground_pgid(fd.as_fd()).ok())
    }

    // ---- agent ----

    /// Фоновый потомок в той же группе и сессии: стандартные потоки — `/dev/null`, `SIGHUP`
    /// заблокирован до `exec`, а `nohup-child` ставит ему `SIG_IGN` первым действием.
    fn spawn_child(&mut self, channel_path: &Path) -> Result<(), StopReason> {
        let exe = std::env::current_exe()
            .map_err(|err| StopReason::ChannelError(format!("путь фикстуры: {err}")))?;
        let lifetime = self.deadline.saturating_duration_since(Instant::now());
        let nonce = format!("{}-child", self.nonce);
        let child = Process::new(exe)
            .args(command_args(
                Mode::NohupChild,
                channel_path,
                &nonce,
                Some(lifetime),
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| StopReason::ChannelError(format!("потомок не запущен: {err}")))?;
        let pid = child.id() as i32;
        self.child = Some(pid);
        self.send(
            sys::uptime_ns(),
            Event::Spawned {
                pid,
                mode: Mode::NohupChild,
            },
        )
    }

    // ---- shell ----

    fn run_shell_line(&mut self, text: &str) -> Result<(), StopReason> {
        let error = match parse_shell_line(text) {
            Ok(ShellLine::Empty) | Ok(ShellLine::Jobs) => None,
            Ok(ShellLine::Exit(code)) => return Err(StopReason::Command(code)),
            Ok(ShellLine::Run {
                program,
                args,
                background,
            }) => self.start_job(&program, &args, background, text).err(),
            Ok(ShellLine::Fg(number)) => self.continue_job(number, true).err(),
            Ok(ShellLine::Bg(number)) => self.continue_job(number, false).err(),
            Err(error) => Some(error),
        };
        // XNU не посылает `SIGCHLD` при продолжении, а `waitpid(WCONTINUED)` его отдаёт сразу
        // после `SIGCONT`: продолжение прибирается здесь же.
        self.reap()?;
        let jobs = self
            .jobs
            .iter()
            .map(|job| JobView {
                job: job.number,
                pid: job.pid,
                stopped: job.stopped,
                foreground: job.foreground,
                command: job.command.clone(),
            })
            .collect();
        let foreground_pgid = self.foreground_pgid();
        self.send(
            sys::uptime_ns(),
            Event::Shell {
                line: text.to_owned(),
                error,
                jobs,
                foreground_pgid,
            },
        )
    }

    /// Задание — лидер своей группы. Терминал передаётся ему и родителем, и самим потомком до
    /// `exec`, поэтому нет окна, где задание читает терминал из фоновой группы.
    fn start_job(
        &mut self,
        program: &str,
        args: &[String],
        background: bool,
        command: &str,
    ) -> Result<(), String> {
        let fd = self.pty.as_ref().ok_or("у режима нет PTY")?.as_raw_fd();
        let mut process = Process::new(program);
        process.args(args);
        // SAFETY: в потомке между fork и exec вызываются только async-signal-safe функции.
        unsafe {
            process.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if !background && libc::tcsetpgrp(fd, libc::getpid()) != 0 {
                    return Err(io::Error::last_os_error());
                }
                ptys::clear_signal_mask()
            });
        }
        let child = process.spawn().map_err(|err| format!("{program}: {err}"))?;
        let pid = child.id() as i32;
        // SAFETY: вызовы без указателей; `EACCES` после `exec` потомка ожидаем.
        unsafe {
            libc::setpgid(pid, pid);
            if !background {
                libc::tcsetpgrp(fd, pid);
            }
        }
        let number = self.next_job;
        self.next_job += 1;
        self.jobs.push(Job {
            number,
            pid,
            command: command.to_owned(),
            stopped: false,
            foreground: !background,
        });
        // Процесс прибирается через `waitpid`, а не через `Child`.
        drop(child);
        self.job_event(
            number,
            pid,
            JobChange::Started {
                foreground: !background,
            },
        )
        .map_err(|reason| reason.name().to_owned())
    }

    fn continue_job(&mut self, number: u32, foreground: bool) -> Result<(), String> {
        let fd = self.pty.as_ref().ok_or("у режима нет PTY")?.as_raw_fd();
        let job = self
            .jobs
            .iter_mut()
            .find(|job| job.number == number)
            .ok_or_else(|| format!("нет задания {number}"))?;
        if foreground {
            // SAFETY: вызов без указателей; `SIGTTOU` заблокирован.
            if unsafe { libc::tcsetpgrp(fd, job.pid) } != 0 {
                return Err(format!("tcsetpgrp: {}", io::Error::last_os_error()));
            }
        }
        job.foreground = foreground;
        job.stopped = false;
        // SAFETY: сигнал группе задания.
        if unsafe { libc::kill(-job.pid, libc::SIGCONT) } != 0 {
            return Err(format!("SIGCONT: {}", io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Все изменения потомков после `SIGCHLD`.
    fn reap(&mut self) -> Result<(), StopReason> {
        loop {
            let mut status = 0;
            // SAFETY: `status` живёт на время вызова.
            let pid = unsafe {
                libc::waitpid(
                    -1,
                    &mut status,
                    libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED,
                )
            };
            if pid <= 0 {
                return Ok(());
            }
            let Some(decoded) = WaitStatus::decode(status) else {
                continue;
            };
            if self.child == Some(pid) {
                if !matches!(decoded, WaitStatus::Stopped { .. } | WaitStatus::Continued) {
                    self.child = None;
                }
                self.send(
                    sys::uptime_ns(),
                    Event::ChildExit {
                        pid,
                        status: decoded,
                    },
                )?;
                continue;
            }
            let Some(at) = self.jobs.iter().position(|job| job.pid == pid) else {
                continue;
            };
            let change = match decoded {
                WaitStatus::Exited { code } => JobChange::Exited { code },
                WaitStatus::Signaled { signal } => JobChange::Signaled {
                    signal: signal_name(signal),
                },
                WaitStatus::Stopped { signal } => JobChange::Stopped {
                    signal: signal_name(signal),
                },
                WaitStatus::Continued => JobChange::Continued {
                    foreground: self.jobs[at].foreground,
                },
            };
            let number = self.jobs[at].number;
            let was_foreground = self.jobs[at].foreground;
            match decoded {
                WaitStatus::Stopped { .. } => {
                    self.jobs[at].stopped = true;
                    self.jobs[at].foreground = false;
                }
                WaitStatus::Continued => self.jobs[at].stopped = false,
                WaitStatus::Exited { .. } | WaitStatus::Signaled { .. } => {
                    self.jobs.remove(at);
                }
            }
            if was_foreground && !matches!(decoded, WaitStatus::Continued) {
                self.reclaim_terminal();
            }
            self.job_event(number, pid, change)?;
        }
    }

    /// Терминал снова у `shell`; поток чтения, ждавший после `EIO`, продолжает.
    fn reclaim_terminal(&mut self) {
        if let Some(fd) = &self.pty {
            // SAFETY: вызов без указателей; `SIGTTOU` заблокирован.
            unsafe { libc::tcsetpgrp(fd.as_raw_fd(), libc::getpgrp()) };
        }
        let (lock, wake) = &*self.gate;
        lock.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .foreground_epoch += 1;
        wake.notify_all();
    }

    fn job_event(&mut self, job: u32, pid: i32, change: JobChange) -> Result<(), StopReason> {
        let foreground_pgid = self.foreground_pgid();
        self.send(
            sys::uptime_ns(),
            Event::Job {
                job,
                pid,
                change,
                foreground_pgid,
            },
        )
    }

    fn finish(&mut self, reason: &StopReason) {
        for job in &self.jobs {
            // SAFETY: сигналы группе задания.
            unsafe {
                libc::kill(-job.pid, libc::SIGHUP);
                libc::kill(-job.pid, libc::SIGCONT);
            }
        }
        if let (Some(fd), Some(saved)) = (&self.pty, self.saved_termios) {
            let _ = ptys::set_termios(fd.as_fd(), &saved);
        }
        let (lock, wake) = &*self.gate;
        lock.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stop = true;
        wake.notify_all();
        let event = Event::Exit {
            reason: reason.name().to_owned(),
            detail: reason.detail(),
            code: reason.code(),
            writes: self.writes,
        };
        let _ = self.send(sys::uptime_ns(), event);
    }
}

// ---- источники событий ----

/// Чтение PTY с темпом из [`Gate`]. Поток ждёт данных вне блокировки, а читает под ней: после
/// подтверждённой команды «темп 0» не проходит ни одно чтение. Дескриптор к этому моменту готов,
/// и `read` не ждёт: читает его только этот поток, а из фоновой группы `read` сразу даёт `EIO`.
/// `EIO` из фоновой группы (`shell` отдал терминал заданию) — не конец: поток ждёт, пока терминал
/// вернётся.
fn spawn_pty_reader(fd: OwnedFd, gate: SharedGate, tx: Sender<Input>) -> io::Result<()> {
    thread::Builder::new()
        .name("fixture-pty".to_owned())
        .spawn(move || {
            let mut buf = [0u8; READ_BUF];
            let mut epoch = 0;
            loop {
                if let Err(err) = ptys::wait_ready(fd.as_fd(), false, None) {
                    let _ = tx.send(Input::PtyClosed(format!("ожидание чтения: {err}")));
                    return;
                }
                let (read, rate) = {
                    let (lock, wake) = &*gate;
                    let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    while state.rate == Some(0) && !state.stop {
                        state = wake
                            .wait(state)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    if state.stop {
                        return;
                    }
                    epoch = epoch.max(state.foreground_epoch);
                    let limit = state.rate.map_or(buf.len(), |rate| {
                        usize::try_from(rate)
                            .unwrap_or(usize::MAX)
                            .clamp(1, buf.len())
                    });
                    // SAFETY: буфер действителен на `limit` байт, `fd` принадлежит потоку.
                    let read =
                        unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), limit) };
                    let read = usize::try_from(read).map_err(|_| io::Error::last_os_error());
                    (read, state.rate)
                };
                let t_ns = sys::uptime_ns();
                let input = match read {
                    Ok(0) => Input::PtyClosed("конец чтения".to_owned()),
                    Ok(read) => Input::Pty {
                        t_ns,
                        bytes: buf[..read].to_vec(),
                    },
                    Err(err) => match err.raw_os_error() {
                        Some(libc::EINTR) => continue,
                        Some(libc::EIO) if wait_foreground(&gate, fd.as_fd(), epoch) => {
                            continue;
                        }
                        Some(libc::EIO) => Input::PtyClosed("EIO".to_owned()),
                        _ => Input::PtyClosed(format!("чтение: {err}")),
                    },
                };
                let last = matches!(input, Input::PtyClosed(_));
                let paced = match &input {
                    Input::Pty { bytes, .. } => rate.map(|rate| pace(bytes.len(), rate)),
                    _ => None,
                };
                if tx.send(input).is_err() || last {
                    return;
                }
                if let Some(wait) = paced {
                    thread::sleep(wait);
                }
            }
        })
        .map(drop)
}

/// Терминал есть, но группа переднего плана чужая.
fn in_background(fd: BorrowedFd<'_>) -> bool {
    // SAFETY: вызов без указателей.
    let own = unsafe { libc::getpgrp() };
    ptys::foreground_pgid(fd).is_ok_and(|pgid| pgid != own)
}

/// `EIO` пришёл, пока терминал был у задания `shell`: ждёт его возврата и даёт `true`. Терминал
/// мог вернуться раньше, чем поток увидел `EIO`, это видно по выросшей эпохе. `false` — PTY
/// действительно закрыт.
fn wait_foreground(gate: &SharedGate, fd: BorrowedFd<'_>, epoch: u64) -> bool {
    let (lock, wake) = &**gate;
    let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.foreground_epoch > epoch {
        return true;
    }
    if !in_background(fd) {
        return false;
    }
    while state.foreground_epoch <= epoch && !state.stop {
        state = wake
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    true
}

fn spawn_channel_reader(mut stream: UnixStream, tx: Sender<Input>) -> io::Result<()> {
    use std::io::Read;

    thread::Builder::new()
        .name("fixture-channel".to_owned())
        .spawn(move || {
            let mut frames = channel::FrameReader::default();
            let mut buf = [0u8; READ_BUF];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => {
                        if let Err(err) = frames.finish() {
                            let _ = tx.send(Input::Corrupt(err.to_string()));
                        }
                        let _ = tx.send(Input::ChannelClosed);
                        return;
                    }
                    Ok(read) => {
                        frames.push(&buf[..read]);
                        while let Some(frame) = frames.next_frame() {
                            let input =
                                match frame.and_then(|frame| channel::decode::<Request>(&frame)) {
                                    Ok(request) => Input::Request(request),
                                    Err(err) => Input::Corrupt(err.to_string()),
                                };
                            if tx.send(input).is_err() {
                                return;
                            }
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        let _ = tx.send(Input::ChannelError(format!("чтение команды: {err}")));
                        return;
                    }
                }
            }
        })
        .map(drop)
}

fn spawn_signal_waiter(set: libc::sigset_t, tx: Sender<Input>) -> io::Result<()> {
    thread::Builder::new()
        .name("fixture-signals".to_owned())
        .spawn(move || {
            loop {
                let mut signal: c_int = 0;
                // SAFETY: `set` инициализирован, `signal` — место для результата.
                let status = unsafe { libc::sigwait(&set, &mut signal) };
                let t_ns = sys::uptime_ns();
                if status != 0 || tx.send(Input::Signal { t_ns, signal }).is_err() {
                    return;
                }
            }
        })
        .map(drop)
}

/// Обработчик, который никогда не выполняется: сигналы заблокированы во всех потоках.
extern "C" fn never_runs(_: c_int) {}

/// Блокирует [`SIGNALS`] до первого потока и ставит им обработчик: XNU отбрасывает при генерации
/// сигнал с действием «игнорировать» по умолчанию (`SIGWINCH`, `SIGCHLD`) даже заблокированным.
/// У `nohup-child` `SIGHUP` остаётся игнорируемым.
fn block_signals(mode: Mode) -> Result<libc::sigset_t, String> {
    let wanted: Vec<c_int> = SIGNALS
        .into_iter()
        .filter(|signal| !(mode == Mode::NohupChild && *signal == libc::SIGHUP))
        .collect();
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigemptyset` инициализирует набор, затем он заполняется.
    let set = unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        for signal in &wanted {
            libc::sigaddset(set.as_mut_ptr(), *signal);
        }
        set.assume_init()
    };
    // SAFETY: набор инициализирован, старая маска не запрашивается.
    let status = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    if status != 0 {
        return Err(format!(
            "pthread_sigmask: {}",
            io::Error::from_raw_os_error(status)
        ));
    }
    for signal in wanted {
        // SAFETY: структура обнуляется и заполняется до вызова; обработчик — функция с ABI C.
        let status = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = never_runs as extern "C" fn(c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut())
        };
        if status != 0 {
            return Err(format!(
                "sigaction {}: {}",
                signal_name(signal),
                io::Error::last_os_error()
            ));
        }
    }
    Ok(set)
}

fn dup(fd: c_int) -> Result<OwnedFd, String> {
    // SAFETY: стандартные потоки открыты на всё время процесса.
    unsafe { BorrowedFd::borrow_raw(fd) }
        .try_clone_to_owned()
        .map_err(|err| format!("дескриптор {fd} не дублируется: {err}"))
}

/// Отдельное открытие терминала для записи с `O_NONBLOCK`. Флаг принадлежит открытому файлу,
/// поэтому на копии stdin он сделал бы неблокирующим и чтение другого потока.
fn open_writer(input: c_int) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;

    let tty = ptys::tty_name(input).map_err(|err| format!("ttyname: {err}"))?;
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(&tty)
        .map_err(|err| format!("{tty}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn modes_round_trip_by_name() {
        for mode in Mode::ALL {
            assert_eq!(Mode::from_name(mode.name()), Some(mode));
        }
        assert_eq!(Mode::from_name("bash"), None);
        assert!(!Mode::NohupChild.has_pty());
        assert!(Mode::Query.raw() && !Mode::Shell.raw() && !Mode::Agent.raw());
    }

    #[test]
    fn arguments_are_parsed_and_checked() {
        let args = parse_args(&strings(&[
            "--mode",
            "slow-reader",
            "--channel",
            "/tmp/c.sock",
            "--nonce",
            "n1",
            "--lifetime-ms",
            "1500",
            "--rate",
            "64",
        ]))
        .unwrap();
        assert_eq!(args.mode, Mode::SlowReader);
        assert_eq!(args.channel, PathBuf::from("/tmp/c.sock"));
        assert_eq!(args.lifetime, Duration::from_millis(1500));
        assert_eq!(args.rate, Some(64));
        let built = command_args(Mode::Agent, Path::new("/tmp/c.sock"), "n2", None);
        let parsed = parse_args(&built).unwrap();
        assert_eq!(
            (parsed.mode, parsed.lifetime),
            (Mode::Agent, default_lifetime())
        );
        for bad in [
            &["--mode", "shell", "--channel", "/c"][..],
            &["--mode", "shell", "--nonce", "a b", "--channel", "/c"],
            &["--mode", "zsh", "--nonce", "n", "--channel", "/c"],
            &[
                "--mode",
                "shell",
                "--nonce",
                "n",
                "--channel",
                "/c",
                "--lifetime-ms",
                "0",
            ],
            &["--mode", "shell", "--nonce", "n", "--channel"],
            &["--extra"],
        ] {
            assert!(parse_args(&strings(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn flood_lines_carry_their_index_and_width() {
        let line = flood_line("abc", 42, 30);
        assert_eq!(line.len(), 32);
        assert!(line.ends_with(b"\r\n"));
        assert_eq!(flood_index("abc", &line), Some(42));
        assert_eq!(flood_index("abd", &line), None);
        let short = flood_line("abc", 7, 0);
        assert_eq!(short, b"abc 0000000007 \r\n");
        assert_eq!(flood_index("abc", &short), Some(7));
    }

    #[test]
    fn pace_keeps_average_rate() {
        assert_eq!(pace(100, 100), Duration::from_secs(1));
        assert_eq!(pace(1, 1000), Duration::from_millis(1));
        assert_eq!(pace(10, 0), Duration::ZERO);
    }

    #[test]
    fn shell_lines_are_parsed() {
        assert_eq!(parse_shell_line("  "), Ok(ShellLine::Empty));
        assert_eq!(
            parse_shell_line("run /bin/sleep 600 &"),
            Ok(ShellLine::Run {
                program: "/bin/sleep".to_owned(),
                args: vec!["600".to_owned()],
                background: true
            })
        );
        assert_eq!(
            parse_shell_line("run /bin/cat"),
            Ok(ShellLine::Run {
                program: "/bin/cat".to_owned(),
                args: Vec::new(),
                background: false
            })
        );
        assert_eq!(parse_shell_line("fg %2"), Ok(ShellLine::Fg(2)));
        assert_eq!(parse_shell_line("bg 1"), Ok(ShellLine::Bg(1)));
        assert_eq!(parse_shell_line("jobs"), Ok(ShellLine::Jobs));
        assert_eq!(parse_shell_line("exit 3"), Ok(ShellLine::Exit(3)));
        for bad in ["run", "run &", "fg", "fg x", "exit 1 2", "ls"] {
            assert!(parse_shell_line(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn stop_reasons_map_to_exit_codes() {
        assert_eq!(StopReason::Hangup.code(), 0);
        assert_eq!(StopReason::PtyClosed("EIO".to_owned()).code(), 0);
        assert_eq!(StopReason::Timeout.code(), EXIT_EXECUTION);
        assert_eq!(StopReason::Command(7).code(), 7);
        assert_eq!(signal_name(libc::SIGTSTP), "SIGTSTP");
    }
}
