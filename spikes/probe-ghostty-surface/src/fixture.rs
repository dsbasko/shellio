//! Фикстура в PTY: режим `--fixture`, программа внутри поверхности для детерминированных
//! проверок. Работает без AppKit и без `ghostty_init` и не запускает дочерних процессов.
//!
//! Фикстура подключается к сокету Unix пробы (`--channel`) и шлёт по нему квитанции: готовность с
//! окружением, каждый `SIGWINCH` с `TIOCGWINSZ`, каждое чтение из PTY с порядковым номером, hex и
//! временем `CLOCK_UPTIME_RAW` сразу после `read`. По второму направлению того же сокета она
//! принимает команды и подтверждает каждую квитанцией `ack`, когда байты команды уже записаны в
//! PTY. Кадр в обе стороны — одна строка JSON с `\n` в конце.
//!
//! Фикстура завершается сама: по закрытию PTY (конец чтения, `EIO`, `SIGHUP`), по закрытию сокета,
//! по `SIGTERM`, по команде `exit` и по своему сроку.
//!
//! На macOS libghostty запускает команду поверхности как
//! `/usr/bin/login -flp <user> /bin/bash --noprofile --norc -c "exec -l <команда>"`
//! (`src/termio/Exec.zig`, `execCommand`), поэтому команда — один простой вызов без своего `exec`
//! ([`shell_command`]), а `argv[0]` фикстуры начинается с `-`.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use libc::c_int;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::clock;
use crate::scenario::{EXIT_EXECUTION, Scenario};

/// Переменная окружения поверхности с nonce окружения; фикстура возвращает её в готовности.
pub const ENV_NONCE: &str = "SHELLIO_PROBE_ENV_NONCE";
/// Предел кадра в байтах без `\n`.
pub const MAX_FRAME: usize = 1 << 20;
/// `sockaddr_un.sun_path` на macOS — 104 байта вместе с нулём.
const SUN_PATH_MAX: usize = 103;
/// Запас срока фикстуры сверх самого долгого сценария.
const LIFETIME_MARGIN: Duration = Duration::from_secs(30);
const MAX_LIFETIME_SECS: u64 = 3600;
/// Запись квитанции не ждёт пробу дольше этого: иначе фикстура зависла бы вместе с ней.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const READ_BUF: usize = 4096;
/// Строк потока за один проход цикла: между пачками цикл принимает ввод и команды.
const STREAM_BATCH: u32 = 64;
const MAX_STREAM_LINES: u32 = 10_000_000;
const MAX_STREAM_INTERVAL_US: u64 = 10_000_000;
const MAX_COLUMNS: u16 = 1000;
const MAX_BLOCK: u16 = 1000;
/// Предел одной записи `scrollback`: команда собирает байты целиком до записи.
const MAX_SCROLLBACK_BYTES: u64 = 64 << 20;
const MAX_TRIGGER: usize = 256;
const MAX_ANSWER: usize = 4096;
const MAX_PENDING_REPLIES: usize = 1024;
const MAX_KITTY_FLAGS: u8 = 31;
/// Цвета клеток контрастной сетки.
pub const GRID_DARK: [u8; 3] = [0, 0, 0];
pub const GRID_LIGHT: [u8; 3] = [255, 255, 255];
/// Каталоги записи `xterm-ghostty` в `TERMINFO`: шестнадцатеричный, как в наборе ghostty, и буквенный.
const TERMINFO_ENTRIES: [&str; 2] = ["78/xterm-ghostty", "x/xterm-ghostty"];
/// Сигналы, которые фикстура принимает сама, а не действием по умолчанию: `Ctrl-\` в
/// каноническом режиме не должен убить её `SIGQUIT`.
const SIGNALS: [c_int; 6] = [
    libc::SIGWINCH,
    libc::SIGHUP,
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTSTP,
];

// ---- протокол ----

/// Размер окна PTY по `TIOCGWINSZ`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

/// Отчёт мыши; включённый всегда в кодировке SGR (`?1006`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MouseMode {
    #[default]
    Off,
    /// Нажатия и отпускания (`?1000`).
    Click,
    /// И движение с нажатой кнопкой (`?1002`).
    Drag,
    /// Любое движение (`?1003`).
    Motion,
}

/// Команда пробы фикстуре.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Command {
    /// `cfmakeraw` на PTY или возврат сохранённого режима.
    Raw {
        on: bool,
    },
    BracketedPaste {
        on: bool,
    },
    Mouse {
        mode: MouseMode,
    },
    /// Согласование kitty keyboard protocol: `CSI > flags u`.
    KittyPush {
        flags: u8,
    },
    /// `CSI < u`, снимает последний `kitty-push`.
    KittyPop,
    /// Ограниченный поток строк `stream <id> <n>`, строка `n` (с нуля) уходит через
    /// `n × interval_us` от начала. Каждая `osc_every`-я строка предваряется OSC 2 (заголовок) и
    /// OSC 7 (рабочий каталог); каждые `report_every` строк — квитанция хода. Ноль выключает.
    Stream {
        lines: u32,
        interval_us: u64,
        columns: u16,
        osc_every: u32,
        report_every: u32,
    },
    StopStream,
    /// Контрастная сетка блоками `block_cols × block_rows` клеток на весь экран.
    Grid {
        block_cols: u16,
        block_rows: u16,
    },
    /// Прямоугольник цвета `rgb` с верхней левой клеткой `row`, `col` (с единицы).
    Marker {
        row: u16,
        col: u16,
        rows: u16,
        cols: u16,
        rgb: [u8; 3],
    },
    /// `lines` строк шириной `columns` для заполнения scrollback.
    Scrollback {
        lines: u32,
        columns: u16,
    },
    /// Однократный ответ: прочитав `trigger` после этой команды, фикстура пишет `answer`.
    Reply {
        trigger: String,
        answer: String,
    },
    /// Текст в PTY как есть.
    Print {
        text: String,
    },
    /// Новый срок фикстуры от момента команды.
    Lifetime {
        secs: u64,
    },
    Exit {
        code: u8,
    },
}

/// Квитанция фикстуры пробе. `t_ns` — `CLOCK_UPTIME_RAW` события: для `read` сразу после
/// `read`, для `winch` сразу после приёма сигнала, для `ack` после записи байтов команды.
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
    Winch {
        size: Option<Winsize>,
    },
    /// `index` — номер чтения с единицы, `replied` — ответы, записанные на это чтение.
    Read {
        index: u64,
        hex: String,
        replied: Vec<Replied>,
    },
    /// `SIGINT`, `SIGQUIT` или `SIGTSTP`, принятый вместо действия по умолчанию.
    Signal {
        signal: String,
    },
    Ack {
        id: u64,
        bytes: u64,
        error: Option<String>,
    },
    StreamProgress {
        id: u64,
        lines: u64,
        bytes: u64,
    },
    StreamDone {
        id: u64,
        lines: u64,
        bytes: u64,
        t_first_ns: Option<u64>,
        cancelled: bool,
    },
    /// Кадр команды не разобран.
    Protocol {
        error: String,
    },
    Exit {
        reason: String,
        detail: Option<String>,
        code: u8,
    },
}

/// Готовность: процесс, терминал и окружение фикстуры.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    pub nonce: String,
    pub env_nonce: Option<String>,
    pub argv0: String,
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub sid: i32,
    /// `tcgetpgrp` PTY: группа переднего плана терминала.
    pub foreground_pgid: Option<i32>,
    pub tty: Option<String>,
    pub term: Option<String>,
    pub terminfo: Option<String>,
    /// Путь записи `xterm-ghostty` внутри `TERMINFO`, если она там есть.
    pub terminfo_entry: Option<String>,
    pub lang: Option<String>,
    /// Имена всех переменных окружения, без значений.
    pub env_names: Vec<String>,
    pub size: Option<Winsize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replied {
    pub id: u64,
    /// Ответ записан в PTY.
    pub t_ns: u64,
}

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
            FrameError::Oversized(bytes) => {
                write!(f, "кадр длиннее {MAX_FRAME} байт: {bytes}")
            }
        }
    }
}

/// Кадр: JSON в одну строку и `\n`.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut frame = serde_json::to_vec(value).map_err(|err| err.to_string())?;
    frame.push(b'\n');
    Ok(frame)
}

/// Содержимое кадра без `\n`.
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

/// Команда поверхности для запуска `program` с `args`: один простой вызов, каждое слово в
/// одинарных кавычках. libghostty сама ставит перед ней `exec -l` и исполняет через shell.
pub fn shell_command(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .map(|word| format!("'{}'", word.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- байты в PTY ----

fn push_str(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
}

fn background(out: &mut Vec<u8>, [r, g, b]: [u8; 3]) {
    push_str(out, &format!("\x1b[48;2;{r};{g};{b}m"));
}

fn cursor_to(out: &mut Vec<u8>, row: u16, col: u16) {
    push_str(out, &format!("\x1b[{row};{col}H"));
}

fn bracketed_paste_bytes(on: bool) -> &'static [u8] {
    if on { b"\x1b[?2004h" } else { b"\x1b[?2004l" }
}

/// Сначала выключает все режимы отчёта мыши, затем включает нужный с SGR.
fn mouse_bytes(mode: MouseMode) -> Vec<u8> {
    let mut out = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l".to_vec();
    let on: &[u8] = match mode {
        MouseMode::Off => b"",
        MouseMode::Click => b"\x1b[?1000h\x1b[?1006h",
        MouseMode::Drag => b"\x1b[?1002h\x1b[?1006h",
        MouseMode::Motion => b"\x1b[?1003h\x1b[?1006h",
    };
    out.extend_from_slice(on);
    out
}

/// Сетка без переводов строк: каждая строка ставит курсор сама, поэтому экран не прокручивается.
/// Курсор остаётся в нижней левой клетке.
fn grid_bytes(size: Winsize, block_cols: u16, block_rows: u16) -> Result<Vec<u8>, String> {
    if size.rows == 0 || size.cols == 0 {
        return Err(format!("размер PTY {}×{}", size.cols, size.rows));
    }
    if !(1..=MAX_BLOCK).contains(&block_cols) || !(1..=MAX_BLOCK).contains(&block_rows) {
        return Err(format!("блок сетки 1…{MAX_BLOCK} клеток"));
    }
    let mut out = Vec::with_capacity(usize::from(size.rows) * (usize::from(size.cols) + 24));
    for row in 0..size.rows {
        cursor_to(&mut out, row + 1, 1);
        let mut current = None;
        for col in 0..size.cols {
            let dark = (row / block_rows + col / block_cols).is_multiple_of(2);
            if current != Some(dark) {
                background(&mut out, if dark { GRID_DARK } else { GRID_LIGHT });
                current = Some(dark);
            }
            out.push(b' ');
        }
    }
    push_str(&mut out, "\x1b[0m");
    cursor_to(&mut out, size.rows, 1);
    Ok(out)
}

fn marker_bytes(
    size: Winsize,
    row: u16,
    col: u16,
    rows: u16,
    cols: u16,
    rgb: [u8; 3],
) -> Result<Vec<u8>, String> {
    let last_row = u32::from(row) + u32::from(rows);
    let last_col = u32::from(col) + u32::from(cols);
    if row == 0
        || col == 0
        || rows == 0
        || cols == 0
        || last_row > u32::from(size.rows) + 1
        || last_col > u32::from(size.cols) + 1
    {
        return Err(format!(
            "маркер {cols}×{rows} в {row}:{col} вне PTY {}×{}",
            size.cols, size.rows
        ));
    }
    let mut out = Vec::new();
    for current in row..row + rows {
        cursor_to(&mut out, current, col);
        background(&mut out, rgb);
        out.extend(std::iter::repeat_n(b' ', usize::from(cols)));
    }
    push_str(&mut out, "\x1b[0m");
    cursor_to(&mut out, size.rows, 1);
    Ok(out)
}

/// Дополняет строку до `columns` байт повтором `fill`; длинную строку не обрезает.
fn pad(text: &mut Vec<u8>, columns: u16, fill: u8) {
    let columns = usize::from(columns);
    if text.len() < columns {
        text.resize(columns, fill);
    }
}

fn scrollback_bytes(nonce: &str, lines: u32, columns: u16) -> Result<Vec<u8>, String> {
    if lines == 0 || !(1..=MAX_COLUMNS).contains(&columns) {
        return Err(format!("scrollback: строк ≥ 1, ширина 1…{MAX_COLUMNS}"));
    }
    let total = u64::from(lines) * (u64::from(columns) + 2);
    if total > MAX_SCROLLBACK_BYTES {
        return Err(format!(
            "scrollback: {total} байт больше предела {MAX_SCROLLBACK_BYTES}"
        ));
    }
    let mut out = Vec::with_capacity(usize::try_from(total).unwrap_or(0));
    for index in 0..lines {
        let mut line = format!("scrollback {nonce} {index:07} ").into_bytes();
        pad(&mut line, columns, b'a' + (index % 26) as u8);
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
    Ok(out)
}

/// Строка `index` (с нуля) потока `id`.
fn stream_line(nonce: &str, id: u64, index: u32, columns: u16, osc_every: u32) -> Vec<u8> {
    let mut out = Vec::new();
    if osc_every > 0 && (index + 1).is_multiple_of(osc_every) {
        push_str(
            &mut out,
            &format!(
                "\x1b]2;probe {nonce} {index}\x07\x1b]7;file://localhost/tmp/shellio-probe/{index}\x07"
            ),
        );
    }
    let mut line = format!("stream {id} {index:07} ").into_bytes();
    pad(&mut line, columns, b'.');
    out.extend_from_slice(&line);
    out.extend_from_slice(b"\r\n");
    out
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

// ---- однократные ответы ----

struct PendingReply {
    id: u64,
    trigger: Vec<u8>,
    answer: Vec<u8>,
}

/// Ответы на уникальный ввод. Триггер ищется только во вводе, прочитанном после его взведения,
/// в том числе на стыке чтений.
#[derive(Default)]
struct ReplyBook {
    pending: Vec<PendingReply>,
    window: Vec<u8>,
}

impl ReplyBook {
    fn arm(&mut self, id: u64, trigger: String, answer: String) -> Result<(), String> {
        if trigger.is_empty() || trigger.len() > MAX_TRIGGER {
            return Err(format!("триггер 1…{MAX_TRIGGER} байт"));
        }
        if answer.is_empty() || answer.len() > MAX_ANSWER {
            return Err(format!("ответ 1…{MAX_ANSWER} байт"));
        }
        if self.pending.len() >= MAX_PENDING_REPLIES {
            return Err(format!("ждут ответа уже {MAX_PENDING_REPLIES}"));
        }
        if self
            .pending
            .iter()
            .any(|pending| pending.trigger == trigger.as_bytes())
        {
            return Err("такой триггер уже ждёт ввода".to_owned());
        }
        // Ввод до взведения не должен сработать на стыке с новым.
        self.window.clear();
        self.pending.push(PendingReply {
            id,
            trigger: trigger.into_bytes(),
            answer: answer.into_bytes(),
        });
        Ok(())
    }

    /// Сработавшие ответы в порядке взведения.
    fn feed(&mut self, bytes: &[u8]) -> Vec<(u64, Vec<u8>)> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        self.window.extend_from_slice(bytes);
        let mut fired = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            let trigger = &self.pending[index].trigger;
            if self
                .window
                .windows(trigger.len())
                .any(|window| window == trigger.as_slice())
            {
                let reply = self.pending.remove(index);
                fired.push((reply.id, reply.answer));
            } else {
                index += 1;
            }
        }
        let keep = if fired.is_empty() {
            self.pending
                .iter()
                .map(|pending| pending.trigger.len() - 1)
                .max()
                .unwrap_or(0)
        } else {
            0
        };
        let drop = self.window.len().saturating_sub(keep);
        self.window.drain(..drop);
        fired
    }
}

// ---- цикл ----

/// Событие для цикла фикстуры от потоков PTY, сокета и сигналов.
#[derive(Debug)]
pub enum Input {
    Pty { t_ns: u64, bytes: Vec<u8> },
    PtyClosed(String),
    Request(Request),
    Corrupt(String),
    ChannelClosed,
    ChannelError(String),
    Signal { t_ns: u64, signal: c_int },
}

/// PTY фикстуры: чтение, режимы и размер идут через `input`, запись — через `output`.
pub struct PtyIo {
    pub input: OwnedFd,
    pub output: OwnedFd,
}

pub struct Config {
    pub nonce: String,
    pub argv0: String,
    pub env: Vec<(String, String)>,
    pub lifetime: Duration,
}

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
    fn name(&self) -> &'static str {
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

#[derive(Default)]
struct Modes {
    /// Режим PTY до первого `raw`.
    saved: Option<libc::termios>,
    raw: bool,
    bracketed: bool,
    mouse: MouseMode,
    kitty: u32,
}

struct Stream {
    id: u64,
    total: u32,
    done: u32,
    interval: Duration,
    columns: u16,
    osc_every: u32,
    report_every: u32,
    started: Instant,
    bytes: u64,
    t_first_ns: Option<u64>,
}

impl Stream {
    /// Когда уходит следующая строка.
    fn due(&self) -> Instant {
        self.started + self.interval * self.done
    }
}

struct Fixture {
    config: Config,
    input: OwnedFd,
    output: File,
    channel: UnixStream,
    seq: u64,
    reads: u64,
    deadline: Instant,
    modes: Modes,
    stream: Option<Stream>,
    replies: ReplyBook,
}

/// Цикл фикстуры до её завершения. Сначала уходит готовность, затем запускаются потоки чтения
/// PTY и сокета; `tx` и `rx` — общий канал событий, в который вызывающий может добавить свои
/// источники (сигналы).
pub fn serve(
    config: Config,
    pty: PtyIo,
    channel: UnixStream,
    tx: Sender<Input>,
    rx: Receiver<Input>,
) -> Result<StopReason, String> {
    let channel_reader = channel
        .try_clone()
        .map_err(|err| format!("сокет не дублируется: {err}"))?;
    channel
        .set_write_timeout(Some(SEND_TIMEOUT))
        .map_err(|err| format!("срок записи в сокет: {err}"))?;
    let pty_reader = pty
        .input
        .try_clone()
        .map_err(|err| format!("PTY не дублируется: {err}"))?;
    let deadline = Instant::now() + config.lifetime;
    let mut fixture = Fixture {
        config,
        input: pty.input,
        output: File::from(pty.output),
        channel,
        seq: 0,
        reads: 0,
        deadline,
        modes: Modes::default(),
        stream: None,
        replies: ReplyBook::default(),
    };
    let ready = fixture.ready();
    if let Err(reason) = fixture.send(clock::uptime_ns(), Event::Ready(ready)) {
        return Ok(reason);
    }
    spawn_pty_reader(pty_reader, tx.clone())
        .map_err(|err| format!("поток чтения PTY не запущен: {err}"))?;
    spawn_channel_reader(channel_reader, tx)
        .map_err(|err| format!("поток чтения сокета не запущен: {err}"))?;
    Ok(fixture.run(&rx))
}

impl Fixture {
    fn run(&mut self, rx: &Receiver<Input>) -> StopReason {
        let reason = loop {
            if let Err(reason) = self.step_stream() {
                break reason;
            }
            let now = Instant::now();
            if now >= self.deadline {
                break StopReason::Timeout;
            }
            let wake = self
                .stream
                .as_ref()
                .map_or(self.deadline, |stream| stream.due().min(self.deadline));
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
            Input::Corrupt(error) => self.send(clock::uptime_ns(), Event::Protocol { error }),
            Input::ChannelClosed => Err(StopReason::ChannelClosed),
            Input::ChannelError(detail) => Err(StopReason::ChannelError(detail)),
            Input::Signal { t_ns, signal } => match signal {
                libc::SIGWINCH => {
                    let size = winsize(self.input_fd());
                    self.send(t_ns, Event::Winch { size })
                }
                libc::SIGHUP => Err(StopReason::Hangup),
                libc::SIGTERM => Err(StopReason::Terminated),
                other => self.send(
                    t_ns,
                    Event::Signal {
                        signal: signal_name(other),
                    },
                ),
            },
        }
    }

    fn on_read(&mut self, t_ns: u64, bytes: &[u8]) -> Result<(), StopReason> {
        self.reads += 1;
        let mut replied = Vec::new();
        for (id, answer) in self.replies.feed(bytes) {
            self.write(&answer)?;
            replied.push(Replied {
                id,
                t_ns: clock::uptime_ns(),
            });
        }
        self.send(
            t_ns,
            Event::Read {
                index: self.reads,
                hex: hex(bytes),
                replied,
            },
        )
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
        self.send(clock::uptime_ns(), Event::Ack { id, bytes, error })
    }

    /// Внешний `Err` останавливает фикстуру, внутренний — отказ команды в квитанции.
    fn apply(&mut self, id: u64, command: Command) -> Result<Result<u64, String>, StopReason> {
        let bytes = match command {
            Command::Raw { on } => return Ok(self.set_raw(on).map(|()| 0)),
            Command::BracketedPaste { on } => {
                self.modes.bracketed = on;
                bracketed_paste_bytes(on).to_vec()
            }
            Command::Mouse { mode } => {
                self.modes.mouse = mode;
                mouse_bytes(mode)
            }
            Command::KittyPush { flags } => {
                if !(1..=MAX_KITTY_FLAGS).contains(&flags) {
                    return Ok(Err(format!("флаги kitty 1…{MAX_KITTY_FLAGS}")));
                }
                self.modes.kitty += 1;
                format!("\x1b[>{flags}u").into_bytes()
            }
            Command::KittyPop => {
                if self.modes.kitty == 0 {
                    return Ok(Err("kitty keyboard protocol не согласован".to_owned()));
                }
                self.modes.kitty -= 1;
                b"\x1b[<u".to_vec()
            }
            Command::Stream {
                lines,
                interval_us,
                columns,
                osc_every,
                report_every,
            } => {
                return Ok(self.start_stream(
                    id,
                    lines,
                    interval_us,
                    columns,
                    osc_every,
                    report_every,
                ));
            }
            Command::StopStream => {
                return match self.stream.take() {
                    None => Ok(Err("потока нет".to_owned())),
                    Some(stream) => self.stream_done(stream, true).map(|()| Ok(0)),
                };
            }
            Command::Grid {
                block_cols,
                block_rows,
            } => match self
                .size()
                .and_then(|size| grid_bytes(size, block_cols, block_rows))
            {
                Ok(bytes) => bytes,
                Err(error) => return Ok(Err(error)),
            },
            Command::Marker {
                row,
                col,
                rows,
                cols,
                rgb,
            } => match self
                .size()
                .and_then(|size| marker_bytes(size, row, col, rows, cols, rgb))
            {
                Ok(bytes) => bytes,
                Err(error) => return Ok(Err(error)),
            },
            Command::Scrollback { lines, columns } => {
                match scrollback_bytes(&self.config.nonce, lines, columns) {
                    Ok(bytes) => bytes,
                    Err(error) => return Ok(Err(error)),
                }
            }
            Command::Reply { trigger, answer } => {
                return Ok(self.replies.arm(id, trigger, answer).map(|()| 0));
            }
            Command::Print { text } => text.into_bytes(),
            Command::Lifetime { secs } => {
                if !(1..=MAX_LIFETIME_SECS).contains(&secs) {
                    return Ok(Err(format!("срок 1…{MAX_LIFETIME_SECS} с")));
                }
                self.deadline = Instant::now() + Duration::from_secs(secs);
                return Ok(Ok(0));
            }
            Command::Exit { .. } => return Ok(Ok(0)),
        };
        self.write(&bytes)?;
        Ok(Ok(bytes.len() as u64))
    }

    fn start_stream(
        &mut self,
        id: u64,
        lines: u32,
        interval_us: u64,
        columns: u16,
        osc_every: u32,
        report_every: u32,
    ) -> Result<u64, String> {
        if self.stream.is_some() {
            return Err("поток уже идёт".to_owned());
        }
        if !(1..=MAX_STREAM_LINES).contains(&lines) {
            return Err(format!("строк потока 1…{MAX_STREAM_LINES}"));
        }
        if interval_us > MAX_STREAM_INTERVAL_US {
            return Err(format!("интервал потока до {MAX_STREAM_INTERVAL_US} мкс"));
        }
        if columns > MAX_COLUMNS {
            return Err(format!("ширина строки потока до {MAX_COLUMNS}"));
        }
        self.stream = Some(Stream {
            id,
            total: lines,
            done: 0,
            interval: Duration::from_micros(interval_us),
            columns,
            osc_every,
            report_every,
            started: Instant::now(),
            bytes: 0,
            t_first_ns: None,
        });
        Ok(0)
    }

    /// Пишет строки потока, срок которых наступил, не больше `STREAM_BATCH` за проход.
    fn step_stream(&mut self) -> Result<(), StopReason> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        let now = Instant::now();
        let mut batch = Vec::new();
        let mut written = 0;
        let mut report = false;
        while stream.done < stream.total && written < STREAM_BATCH && stream.due() <= now {
            batch.extend(stream_line(
                &self.config.nonce,
                stream.id,
                stream.done,
                stream.columns,
                stream.osc_every,
            ));
            stream.done += 1;
            written += 1;
            report |= stream.report_every > 0 && stream.done.is_multiple_of(stream.report_every);
        }
        if batch.is_empty() {
            return Ok(());
        }
        self.write(&batch)?;
        let t_ns = clock::uptime_ns();
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        stream.t_first_ns.get_or_insert(t_ns);
        stream.bytes += batch.len() as u64;
        let (id, lines, bytes) = (stream.id, u64::from(stream.done), stream.bytes);
        let finished = stream.done == stream.total;
        if report {
            self.send(t_ns, Event::StreamProgress { id, lines, bytes })?;
        }
        if finished && let Some(stream) = self.stream.take() {
            self.stream_done(stream, false)?;
        }
        Ok(())
    }

    fn stream_done(&mut self, stream: Stream, cancelled: bool) -> Result<(), StopReason> {
        self.send(
            clock::uptime_ns(),
            Event::StreamDone {
                id: stream.id,
                lines: u64::from(stream.done),
                bytes: stream.bytes,
                t_first_ns: stream.t_first_ns,
                cancelled,
            },
        )
    }

    /// Возврат режимов и последняя квитанция. В PTY пишется только по команде `exit`: при сроке
    /// или потере сокета терминал может не читать, и запись повесила бы фикстуру.
    fn finish(&mut self, reason: &StopReason) {
        if let Some(stream) = self.stream.take() {
            let _ = self.stream_done(stream, true);
        }
        match reason {
            StopReason::PtyClosed(_) | StopReason::Hangup => {}
            StopReason::Command(_) => {
                let mut bytes = Vec::new();
                if self.modes.kitty > 0 {
                    push_str(&mut bytes, &format!("\x1b[<{}u", self.modes.kitty));
                }
                if self.modes.mouse != MouseMode::Off {
                    bytes.extend(mouse_bytes(MouseMode::Off));
                }
                if self.modes.bracketed {
                    bytes.extend_from_slice(bracketed_paste_bytes(false));
                }
                self.modes.kitty = 0;
                self.modes.mouse = MouseMode::Off;
                self.modes.bracketed = false;
                if !bytes.is_empty() {
                    let _ = self.write(&bytes);
                }
                let _ = self.set_raw(false);
            }
            StopReason::Terminated
            | StopReason::ChannelClosed
            | StopReason::ChannelError(_)
            | StopReason::Timeout => {
                let _ = self.set_raw(false);
            }
        }
        let _ = self.send(
            clock::uptime_ns(),
            Event::Exit {
                reason: reason.name().to_owned(),
                detail: reason.detail(),
                code: reason.code(),
            },
        );
    }

    fn send(&mut self, t_ns: u64, event: Event) -> Result<(), StopReason> {
        self.seq += 1;
        let frame = encode(&Receipt {
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

    fn write(&mut self, bytes: &[u8]) -> Result<(), StopReason> {
        self.output
            .write_all(bytes)
            .map_err(|err| StopReason::PtyClosed(format!("запись: {err}")))
    }

    fn input_fd(&self) -> BorrowedFd<'_> {
        self.input.as_fd()
    }

    fn size(&self) -> Result<Winsize, String> {
        winsize(self.input_fd()).ok_or_else(|| "TIOCGWINSZ не прочитан".to_owned())
    }

    fn set_raw(&mut self, on: bool) -> Result<(), String> {
        let fd = self.input.as_raw_fd();
        if on == self.modes.raw {
            return Ok(());
        }
        let saved = match self.modes.saved {
            Some(saved) => saved,
            None => {
                let saved = tcgetattr(fd)?;
                self.modes.saved = Some(saved);
                saved
            }
        };
        let mut next = saved;
        if on {
            // SAFETY: `next` — инициализированная структура termios на стеке.
            unsafe { libc::cfmakeraw(&mut next) };
        }
        // Без сброса очереди: ввод, пришедший до смены режима, не теряется.
        // SAFETY: `fd` открыт, `next` живёт на время вызова.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &next) } != 0 {
            return Err(format!("tcsetattr: {}", io::Error::last_os_error()));
        }
        self.modes.raw = on;
        Ok(())
    }

    fn ready(&self) -> Ready {
        let get = |name: &str| {
            self.config
                .env
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, value)| value.clone())
        };
        let terminfo = get("TERMINFO");
        let terminfo_entry = terminfo.as_deref().and_then(terminfo_entry);
        let mut env_names: Vec<String> = self
            .config
            .env
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        env_names.sort();
        let fd = self.input.as_raw_fd();
        // SAFETY: вызовы без аргументов-указателей; `fd` открыт.
        let (ppid, pgid, sid, foreground) = unsafe {
            (
                libc::getppid(),
                libc::getpgrp(),
                libc::getsid(0),
                libc::tcgetpgrp(fd),
            )
        };
        Ready {
            nonce: self.config.nonce.clone(),
            env_nonce: get(ENV_NONCE),
            argv0: self.config.argv0.clone(),
            pid: std::process::id() as i32,
            ppid,
            pgid,
            sid,
            foreground_pgid: (foreground >= 0).then_some(foreground),
            tty: ttyname(fd),
            term: get("TERM"),
            terminfo,
            terminfo_entry,
            lang: get("LANG"),
            env_names,
            size: winsize(self.input_fd()),
        }
    }
}

// ---- источники событий ----

fn spawn_pty_reader(fd: OwnedFd, tx: Sender<Input>) -> io::Result<()> {
    thread::Builder::new()
        .name("fixture-pty".to_owned())
        .spawn(move || {
            let mut buf = [0u8; READ_BUF];
            loop {
                // SAFETY: буфер действителен на `buf.len()` байт, `fd` принадлежит потоку.
                let read =
                    unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                let t_ns = clock::uptime_ns();
                let input = match read {
                    0 => Input::PtyClosed("конец чтения".to_owned()),
                    read if read > 0 => Input::Pty {
                        t_ns,
                        bytes: buf[..read as usize].to_vec(),
                    },
                    _ => {
                        let err = io::Error::last_os_error();
                        match err.raw_os_error() {
                            Some(libc::EINTR) => continue,
                            Some(libc::EIO) => Input::PtyClosed("EIO".to_owned()),
                            _ => Input::PtyClosed(format!("чтение: {err}")),
                        }
                    }
                };
                let last = matches!(input, Input::PtyClosed(_));
                if tx.send(input).is_err() || last {
                    return;
                }
            }
        })
        .map(drop)
}

fn spawn_channel_reader(mut stream: UnixStream, tx: Sender<Input>) -> io::Result<()> {
    thread::Builder::new()
        .name("fixture-channel".to_owned())
        .spawn(move || {
            let mut frames = FrameReader::default();
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
                            let input = match frame.and_then(|frame| decode::<Request>(&frame)) {
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

/// Сигналы заблокированы во всех потоках и принимаются здесь `sigwait`, без обработчиков.
fn spawn_signal_waiter(set: libc::sigset_t, tx: Sender<Input>) -> io::Result<()> {
    thread::Builder::new()
        .name("fixture-signals".to_owned())
        .spawn(move || {
            loop {
                let mut signal: c_int = 0;
                // SAFETY: `set` — инициализированный набор, `signal` — место для результата.
                let status = unsafe { libc::sigwait(&set, &mut signal) };
                let t_ns = clock::uptime_ns();
                if status != 0 || tx.send(Input::Signal { t_ns, signal }).is_err() {
                    return;
                }
            }
        })
        .map(drop)
}

// ---- процесс ----

/// Режим `--fixture`. `env` — окружение, снятое первым действием процесса.
pub fn main(channel: &Path, nonce: &str, env: Vec<(String, String)>) -> ExitCode {
    // До первого потока: поток без маски получил бы сигнал с действием по умолчанию.
    let signals = match block_signals() {
        Ok(signals) => signals,
        Err(reason) => return fail(&reason),
    };
    let stream = match connect(channel) {
        Ok(stream) => stream,
        Err(reason) => return fail(&format!("сокет {}: {reason}", channel.display())),
    };
    let pty = match stdio_pty() {
        Ok(pty) => pty,
        Err(reason) => return fail(&reason),
    };
    let (tx, rx) = mpsc::channel();
    if let Err(err) = spawn_signal_waiter(signals, tx.clone()) {
        return fail(&format!("поток сигналов не запущен: {err}"));
    }
    let config = Config {
        nonce: nonce.to_owned(),
        argv0: std::env::args_os()
            .next()
            .map(|arg| arg.to_string_lossy().into_owned())
            .unwrap_or_default(),
        env,
        lifetime: default_lifetime(),
    };
    match serve(config, pty, stream, tx, rx) {
        Ok(reason) => ExitCode::from(reason.code()),
        Err(reason) => fail(&reason),
    }
}

/// Срок по умолчанию переживает самый долгий сценарий: процесс пробы отсчитывает свой срок раньше.
pub fn default_lifetime() -> Duration {
    Scenario::ALL
        .into_iter()
        .map(Scenario::deadline)
        .max()
        .unwrap_or_default()
        + LIFETIME_MARGIN
}

/// Ошибка до готовности; stderr фикстуры — это PTY поверхности.
fn fail(reason: &str) -> ExitCode {
    let _ = writeln!(
        io::stderr().lock(),
        "{}: --fixture: {reason}",
        crate::PROBE_NAME
    );
    ExitCode::from(EXIT_EXECUTION)
}

/// Обработчик, который никогда не выполняется: сигналы заблокированы во всех потоках.
extern "C" fn never_runs(_: c_int) {}

/// Блокирует `SIGNALS` и ставит им обработчик. XNU отбрасывает при генерации сигнал с действием
/// по умолчанию «игнорировать» (`SIGWINCH`) даже заблокированным, и `sigwait` его бы не увидел.
fn block_signals() -> Result<libc::sigset_t, String> {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigemptyset` инициализирует набор, затем он заполняется и передаётся по ссылке.
    let set = unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        for signal in SIGNALS {
            libc::sigaddset(set.as_mut_ptr(), signal);
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
    for signal in SIGNALS {
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

/// PTY поверхности — стандартные потоки процесса.
fn stdio_pty() -> Result<PtyIo, String> {
    let duplicate = |fd: c_int| {
        // SAFETY: стандартные потоки открыты на всё время процесса.
        unsafe { BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .map_err(|err| format!("дескриптор {fd} не дублируется: {err}"))
    };
    Ok(PtyIo {
        input: duplicate(libc::STDIN_FILENO)?,
        output: duplicate(libc::STDOUT_FILENO)?,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChannelRoute {
    Direct,
    /// Путь длиннее `sun_path`: подключение по имени из родительского каталога.
    ViaParent {
        dir: PathBuf,
        name: PathBuf,
    },
}

pub fn channel_route(path: &Path) -> Result<ChannelRoute, String> {
    if path.as_os_str().len() <= SUN_PATH_MAX {
        return Ok(ChannelRoute::Direct);
    }
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) if !dir.as_os_str().is_empty() && name.len() <= SUN_PATH_MAX => {
            Ok(ChannelRoute::ViaParent {
                dir: dir.to_path_buf(),
                name: PathBuf::from(name),
            })
        }
        _ => Err(format!("имя сокета длиннее {SUN_PATH_MAX} байт")),
    }
}

/// Длинный путь подключается сменой рабочего каталога; потоков в этот момент ещё нет.
fn connect(path: &Path) -> Result<UnixStream, String> {
    match channel_route(path)? {
        ChannelRoute::Direct => UnixStream::connect(path).map_err(|err| err.to_string()),
        ChannelRoute::ViaParent { dir, name } => {
            let back = std::env::current_dir().map_err(|err| format!("рабочий каталог: {err}"))?;
            std::env::set_current_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
            let stream = UnixStream::connect(&name).map_err(|err| err.to_string());
            // Рабочий каталог фикстуре не нужен: сбой возврата подключение не отменяет.
            let _ = std::env::set_current_dir(back);
            stream
        }
    }
}

pub fn winsize(fd: BorrowedFd<'_>) -> Option<Winsize> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `fd` открыт, `size` живёт на время вызова.
    let status = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    (status == 0).then_some(Winsize {
        rows: size.ws_row,
        cols: size.ws_col,
        xpixel: size.ws_xpixel,
        ypixel: size.ws_ypixel,
    })
}

fn tcgetattr(fd: c_int) -> Result<libc::termios, String> {
    let mut termios = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `tcgetattr` заполняет структуру при успехе.
    if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } != 0 {
        return Err(format!("tcgetattr: {}", io::Error::last_os_error()));
    }
    // SAFETY: успешный `tcgetattr` инициализировал структуру.
    Ok(unsafe { termios.assume_init() })
}

fn ttyname(fd: c_int) -> Option<String> {
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: буфер действителен на `buf.len()` байт, при успехе строка завершена нулём.
    if unsafe { libc::ttyname_r(fd, buf.as_mut_ptr(), buf.len()) } != 0 {
        return None;
    }
    // SAFETY: успешный `ttyname_r` записал строку с нулём внутри буфера.
    let name = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

fn terminfo_entry(dir: &str) -> Option<String> {
    TERMINFO_ENTRIES
        .iter()
        .map(|entry| Path::new(dir).join(entry))
        .find(|path| path.is_file())
        .map(|path| path.display().to_string())
}

fn signal_name(signal: c_int) -> String {
    match signal {
        libc::SIGINT => "SIGINT".to_owned(),
        libc::SIGQUIT => "SIGQUIT".to_owned(),
        libc::SIGTSTP => "SIGTSTP".to_owned(),
        libc::SIGWINCH => "SIGWINCH".to_owned(),
        libc::SIGHUP => "SIGHUP".to_owned(),
        libc::SIGTERM => "SIGTERM".to_owned(),
        other => format!("сигнал {other}"),
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{FromRawFd, RawFd};
    use std::process::Command as Process;
    use std::thread::JoinHandle;

    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn hex_round_trips() {
        let bytes = [0x00, 0x1b, 0x5b, 0xd0, 0xb6, 0xff];
        assert_eq!(unhex(&hex(&bytes)).as_deref(), Some(&bytes[..]));
        assert_eq!(unhex(""), Some(Vec::new()));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex("жж"), None);
    }

    const WAIT: Duration = Duration::from_secs(10);

    fn size(rows: u16, cols: u16) -> Winsize {
        Winsize {
            rows,
            cols,
            xpixel: cols * 10,
            ypixel: rows * 20,
        }
    }

    // ---- кадрирование и схема ----

    #[test]
    fn frames_split_across_pushes_and_several_in_one_push() {
        let mut frames = FrameReader::default();
        frames.push(b"{\"a\":1}\n{\"b\"");
        assert_eq!(frames.next_frame(), Some(Ok(b"{\"a\":1}".to_vec())));
        assert_eq!(frames.next_frame(), None);
        frames.push(b":2}\n{\"c\":3}\n");
        assert_eq!(frames.next_frame(), Some(Ok(b"{\"b\":2}".to_vec())));
        assert_eq!(frames.next_frame(), Some(Ok(b"{\"c\":3}".to_vec())));
        assert_eq!(frames.next_frame(), None);
        assert_eq!(frames.finish(), Ok(()));
    }

    #[test]
    fn truncated_frame_is_reported_at_end_of_stream() {
        let mut frames = FrameReader::default();
        frames.push(b"{\"seq\":1}\n{\"seq\":");
        assert!(matches!(frames.next_frame(), Some(Ok(_))));
        assert_eq!(frames.next_frame(), None);
        assert_eq!(frames.finish(), Err(FrameError::Truncated(7)));
        assert!(FrameError::Truncated(7).to_string().contains("обрезанный"));
    }

    #[test]
    fn oversized_frame_is_dropped_up_to_next_newline() {
        let mut frames = FrameReader::default();
        frames.push(&vec![b'x'; MAX_FRAME + 1]);
        assert_eq!(
            frames.next_frame(),
            Some(Err(FrameError::Oversized(MAX_FRAME + 1)))
        );
        frames.push(b"tail of the long frame\n{\"ok\":true}\n");
        assert_eq!(frames.next_frame(), Some(Ok(b"{\"ok\":true}".to_vec())));
        assert_eq!(frames.finish(), Ok(()));
    }

    #[test]
    fn empty_frame_is_corrupt() {
        let mut frames = FrameReader::default();
        frames.push(b"\n");
        assert!(matches!(
            frames.next_frame(),
            Some(Err(FrameError::Corrupt(_)))
        ));
    }

    #[test]
    fn corrupt_frames_do_not_decode() {
        for frame in [
            &b"{oops}"[..],
            b"{\"seq\":1,\"t_ns\":2}",
            b"{\"seq\":1,\"t_ns\":2,\"event\":{\"kind\":\"nope\"}}",
            b"{\"seq\":1,\"t_ns\":2,\"event\":{\"kind\":\"read\",\"index\":1}}",
            b"{\"seq\":\"1\",\"t_ns\":2,\"event\":{\"kind\":\"winch\",\"size\":null}}",
            b"\xff\xfe",
        ] {
            assert!(
                matches!(decode::<Receipt>(frame), Err(FrameError::Corrupt(_))),
                "{}",
                String::from_utf8_lossy(frame)
            );
        }
        for frame in [
            &b"{\"id\":1,\"command\":{\"kind\":\"raw\"}}"[..],
            b"{\"id\":1,\"command\":{\"kind\":\"launch\"}}",
            b"{\"command\":{\"kind\":\"kitty-pop\"}}",
        ] {
            assert!(decode::<Request>(frame).is_err());
        }
    }

    #[test]
    fn receipts_round_trip_and_keep_wire_shape() {
        let receipts = [
            Receipt {
                seq: 1,
                t_ns: 10,
                event: Event::Ready(Ready {
                    nonce: "n-1".to_owned(),
                    env_nonce: Some("e-1".to_owned()),
                    argv0: "-/p".to_owned(),
                    pid: 7,
                    ppid: 6,
                    pgid: 7,
                    sid: 5,
                    foreground_pgid: Some(7),
                    tty: Some("/dev/ttys009".to_owned()),
                    term: Some("xterm-ghostty".to_owned()),
                    terminfo: Some("/b/terminfo".to_owned()),
                    terminfo_entry: Some("/b/terminfo/78/xterm-ghostty".to_owned()),
                    lang: None,
                    env_names: vec!["HOME".to_owned()],
                    size: Some(size(24, 80)),
                }),
            },
            Receipt {
                seq: 2,
                t_ns: 11,
                event: Event::Read {
                    index: 1,
                    hex: "1c".to_owned(),
                    replied: vec![Replied { id: 4, t_ns: 12 }],
                },
            },
            Receipt {
                seq: 3,
                t_ns: 13,
                event: Event::Exit {
                    reason: "timeout".to_owned(),
                    detail: None,
                    code: 3,
                },
            },
        ];
        for receipt in receipts {
            let frame = encode(&receipt).unwrap();
            assert_eq!(frame.last(), Some(&b'\n'));
            assert_eq!(frame.iter().filter(|&&byte| byte == b'\n').count(), 1);
            assert_eq!(
                decode::<Receipt>(&frame[..frame.len() - 1]).unwrap(),
                receipt
            );
        }
        let read: Value = serde_json::to_value(Receipt {
            seq: 2,
            t_ns: 11,
            event: Event::Winch { size: None },
        })
        .unwrap();
        assert_eq!(
            read,
            json!({"seq": 2, "t_ns": 11, "event": {"kind": "winch", "size": null}})
        );
    }

    #[test]
    fn requests_decode_from_wire_shape() {
        let cases = [
            (
                json!({"id": 1, "command": {"kind": "raw", "on": true}}),
                Command::Raw { on: true },
            ),
            (
                json!({"id": 1, "command": {"kind": "bracketed-paste", "on": false}}),
                Command::BracketedPaste { on: false },
            ),
            (
                json!({"id": 1, "command": {"kind": "mouse", "mode": "drag"}}),
                Command::Mouse {
                    mode: MouseMode::Drag,
                },
            ),
            (
                json!({"id": 1, "command": {"kind": "kitty-push", "flags": 3}}),
                Command::KittyPush { flags: 3 },
            ),
            (
                json!({"id": 1, "command": {"kind": "marker", "row": 2, "col": 3, "rows": 1,
                    "cols": 2, "rgb": [255, 0, 255]}}),
                Command::Marker {
                    row: 2,
                    col: 3,
                    rows: 1,
                    cols: 2,
                    rgb: [255, 0, 255],
                },
            ),
            (
                json!({"id": 1, "command": {"kind": "reply", "trigger": "\u{1c}", "answer": "A"}}),
                Command::Reply {
                    trigger: "\u{1c}".to_owned(),
                    answer: "A".to_owned(),
                },
            ),
        ];
        for (wire, command) in cases {
            let frame = serde_json::to_vec(&wire).unwrap();
            assert_eq!(
                decode::<Request>(&frame).unwrap(),
                Request { id: 1, command }
            );
        }
    }

    // ---- экранирование для shell ----

    /// Слова после запуска через `wrap` и `printf '%s\0'`.
    fn shell_words(shell: &str, wrap: impl Fn(&str) -> String, words: &[&str]) -> Vec<String> {
        let mut args = vec!["%s\\0"];
        args.extend_from_slice(words);
        let command = wrap(&shell_command("/usr/bin/printf", &args));
        let mut shell_args: Vec<&str> = match shell {
            "/bin/bash" => vec!["--noprofile", "--norc"],
            _ => Vec::new(),
        };
        shell_args.extend(["-c", command.as_str()]);
        let output = Process::new(shell)
            .args(&shell_args)
            .env_clear()
            .output()
            .unwrap();
        assert!(output.status.success(), "{command}: {output:?}");
        let mut words: Vec<String> = output
            .stdout
            .split(|&byte| byte == 0)
            .map(|word| String::from_utf8(word.to_vec()).unwrap())
            .collect();
        assert_eq!(words.pop().as_deref(), Some(""));
        words
    }

    #[test]
    fn shell_command_survives_sh_and_the_ghostty_login_wrapper() {
        let words = [
            "--fixture",
            "",
            "two words",
            "it's",
            "'",
            "\"$HOME\" `id` $(id) ; && | > < * ? ~ # !",
            "line\nbreak\ttab",
            "кириллица ж",
            "\\backslash\\",
            "-l",
        ];
        assert_eq!(shell_words("/bin/sh", str::to_owned, &words), words);
        assert_eq!(
            shell_words("/bin/bash", |command| format!("exec -l {command}"), &words),
            words
        );
        assert_eq!(shell_command("/a b/p", &["it's"]), r#"'/a b/p' 'it'\''s'"#);
    }

    // ---- байты в PTY ----

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

    #[test]
    fn grid_covers_the_screen_without_scrolling() {
        let bytes = grid_bytes(size(4, 6), 2, 1).unwrap();
        assert!(!bytes.contains(&b'\n') && !bytes.contains(&b'\r'));
        for row in 1..=4 {
            assert_eq!(
                count(&bytes, format!("\x1b[{row};1H").as_bytes()),
                1 + usize::from(row == 4)
            );
        }
        assert_eq!(count(&bytes, b" "), 24);
        let dark = b"\x1b[48;2;0;0;0m";
        let light = b"\x1b[48;2;255;255;255m";
        // Блоки по 2 колонки: в строке три смены цвета.
        assert_eq!(count(&bytes, dark) + count(&bytes, light), 12);
        assert!(bytes.starts_with(b"\x1b[1;1H\x1b[48;2;0;0;0m  \x1b[48;2;255;255;255m"));
        assert!(bytes.ends_with(b"\x1b[0m\x1b[4;1H"));
        assert!(grid_bytes(size(0, 80), 2, 1).is_err());
        assert!(grid_bytes(size(24, 80), 0, 1).is_err());
    }

    #[test]
    fn marker_paints_its_rectangle_inside_the_screen() {
        let bytes = marker_bytes(size(24, 80), 2, 79, 2, 2, [255, 0, 128]).unwrap();
        assert_eq!(
            bytes,
            b"\x1b[2;79H\x1b[48;2;255;0;128m  \x1b[3;79H\x1b[48;2;255;0;128m  \x1b[0m\x1b[24;1H"
        );
        for (row, col, rows, cols) in [
            (0, 1, 1, 1),
            (1, 0, 1, 1),
            (24, 80, 2, 1),
            (1, 80, 1, 2),
            (1, 1, 0, 1),
        ] {
            assert!(
                marker_bytes(size(24, 80), row, col, rows, cols, [0, 0, 0]).is_err(),
                "{row}:{col} {rows}×{cols}"
            );
        }
        assert!(marker_bytes(size(24, 80), 24, 80, 1, 1, [0, 0, 0]).is_ok());
    }

    #[test]
    fn scrollback_has_exact_volume() {
        let bytes = scrollback_bytes("n1", 30, 40).unwrap();
        assert_eq!(bytes.len(), 30 * 42);
        assert_eq!(count(&bytes, b"\r\n"), 30);
        assert!(bytes.starts_with(b"scrollback n1 0000000 aaaa"));
        assert!(scrollback_bytes("n1", 0, 40).is_err());
        assert!(scrollback_bytes("n1", 1, 0).is_err());
        assert!(scrollback_bytes("n1", 1_000_000, 1000).is_err());
        // Ширина меньше заголовка строки: строка не обрезается, объём растёт.
        assert!(
            scrollback_bytes("n1", 2, 5)
                .unwrap()
                .starts_with(b"scrollback n1 0000000 \r\n")
        );
    }

    #[test]
    fn stream_lines_carry_counter_padding_and_osc() {
        assert_eq!(
            stream_line("n", 4, 0, 24, 2),
            b"stream 4 0000000 .......\r\n"
        );
        let with_osc = stream_line("n", 4, 1, 0, 2);
        assert_eq!(
            with_osc,
            b"\x1b]2;probe n 1\x07\x1b]7;file://localhost/tmp/shellio-probe/1\x07stream 4 0000001 \r\n"
        );
        assert!(!stream_line("n", 4, 1, 0, 0).starts_with(b"\x1b]"));
    }

    #[test]
    fn mouse_modes_reset_before_enabling() {
        assert_eq!(
            mouse_bytes(MouseMode::Off),
            b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l"
        );
        assert!(mouse_bytes(MouseMode::Motion).ends_with(b"\x1b[?1003h\x1b[?1006h"));
    }

    #[test]
    fn hex_is_lowercase_pairs() {
        assert_eq!(hex(b"a\x1c\xd0\xb6"), "611cd0b6");
        assert_eq!(hex(b""), "");
    }

    // ---- ответы ----

    #[test]
    fn reply_fires_once_and_across_reads() {
        let mut book = ReplyBook::default();
        book.arm(1, "xyz".to_owned(), "A1".to_owned()).unwrap();
        assert!(book.feed(b"..x").is_empty());
        assert_eq!(book.feed(b"yz"), vec![(1, b"A1".to_vec())]);
        assert!(book.feed(b"xyz").is_empty());
    }

    #[test]
    fn reply_ignores_input_before_arming() {
        let mut book = ReplyBook::default();
        book.arm(1, "q".to_owned(), "first".to_owned()).unwrap();
        assert!(book.feed(b"x").is_empty());
        // «x» прочитан до взведения «xy» и не должен сработать вместе с «y».
        book.arm(2, "xy".to_owned(), "second".to_owned()).unwrap();
        assert!(book.feed(b"y").is_empty());
        assert_eq!(
            book.feed(b"xyq"),
            vec![(1, b"first".to_vec()), (2, b"second".to_vec())]
        );
    }

    #[test]
    fn reply_rejects_bad_and_duplicate_triggers() {
        let mut book = ReplyBook::default();
        assert!(book.arm(1, String::new(), "a".to_owned()).is_err());
        assert!(book.arm(1, "t".to_owned(), String::new()).is_err());
        assert!(
            book.arm(1, "t".repeat(MAX_TRIGGER + 1), "a".to_owned())
                .is_err()
        );
        book.arm(1, "t".to_owned(), "a".to_owned()).unwrap();
        assert!(book.arm(2, "t".to_owned(), "b".to_owned()).is_err());
    }

    // ---- процесс ----

    #[test]
    fn long_channel_path_connects_through_its_parent() {
        assert_eq!(
            channel_route(Path::new("/tmp/x.sock")),
            Ok(ChannelRoute::Direct)
        );
        let dir = format!("/Users/someone/{}", "d".repeat(100));
        assert_eq!(
            channel_route(&Path::new(&dir).join("fixture-1.sock")),
            Ok(ChannelRoute::ViaParent {
                dir: PathBuf::from(&dir),
                name: PathBuf::from("fixture-1.sock"),
            })
        );
        assert!(channel_route(&Path::new("/tmp").join("s".repeat(120))).is_err());
    }

    #[test]
    fn default_lifetime_outlives_every_scenario() {
        for scenario in Scenario::ALL {
            assert!(default_lifetime() > scenario.deadline() + Duration::from_secs(10));
        }
    }

    #[test]
    fn stop_reasons_have_names_and_codes() {
        assert_eq!(StopReason::PtyClosed("EIO".to_owned()).code(), 0);
        assert_eq!(StopReason::Hangup.code(), 0);
        assert_eq!(StopReason::Timeout.code(), EXIT_EXECUTION);
        assert_eq!(
            StopReason::ChannelError("x".to_owned()).code(),
            EXIT_EXECUTION
        );
        assert_eq!(StopReason::Command(7).code(), 7);
        assert_eq!(StopReason::Command(7).name(), "command");
        assert_eq!(signal_name(libc::SIGQUIT), "SIGQUIT");
    }

    // ---- цикл на настоящем PTY ----

    struct Harness {
        probe: UnixStream,
        frames: FrameReader,
        master: Option<OwnedFd>,
        screen: Vec<u8>,
        inject: Sender<Input>,
        fixture: Option<JoinHandle<Result<StopReason, String>>>,
        next_id: u64,
        terminfo: PathBuf,
    }

    fn openpty(size: Winsize) -> (OwnedFd, OwnedFd) {
        let (mut master, mut slave): (RawFd, RawFd) = (-1, -1);
        let mut ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.xpixel,
            ws_ypixel: size.ypixel,
        };
        // SAFETY: указатели на локальные переменные, имя и termios не запрашиваются.
        let status = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        };
        assert_eq!(status, 0, "{}", io::Error::last_os_error());
        // SAFETY: `openpty` вернул два открытых дескриптора, которыми больше никто не владеет.
        unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
    }

    impl Harness {
        fn start(lifetime: Duration) -> Harness {
            let (master, slave) = openpty(size(24, 80));
            let (probe, channel) = UnixStream::pair().unwrap();
            probe.set_read_timeout(Some(WAIT)).unwrap();
            let terminfo = std::env::temp_dir().join(format!(
                "shellio-fixture-test.{}.{}",
                std::process::id(),
                clock::uptime_ns()
            ));
            terminfo_dir(&terminfo);
            let config = Config {
                nonce: "fx-test-1".to_owned(),
                argv0: "-/bundle/probe-ghostty-surface".to_owned(),
                env: vec![
                    ("TERM".to_owned(), "xterm-ghostty".to_owned()),
                    ("TERMINFO".to_owned(), terminfo.display().to_string()),
                    (ENV_NONCE.to_owned(), "env-1".to_owned()),
                    ("HOME".to_owned(), "/home/test".to_owned()),
                ],
                lifetime,
            };
            let pty = PtyIo {
                input: slave.try_clone().unwrap(),
                output: slave,
            };
            let (tx, rx) = mpsc::channel();
            let inject = tx.clone();
            let fixture = thread::spawn(move || serve(config, pty, channel, tx, rx));
            Harness {
                probe,
                frames: FrameReader::default(),
                master: Some(master),
                screen: Vec::new(),
                inject,
                fixture: Some(fixture),
                next_id: 0,
                terminfo,
            }
        }

        fn receipt(&mut self) -> Receipt {
            let mut buf = [0u8; READ_BUF];
            loop {
                if let Some(frame) = self.frames.next_frame() {
                    return decode(&frame.unwrap()).unwrap();
                }
                let read = self
                    .probe
                    .read(&mut buf)
                    .expect("квитанция не пришла в срок");
                assert!(read > 0, "сокет закрыт до квитанции");
                self.frames.push(&buf[..read]);
            }
        }

        fn send(&mut self, command: Command) -> u64 {
            self.next_id += 1;
            let id = self.next_id;
            self.probe
                .write_all(&encode(&Request { id, command }).unwrap())
                .unwrap();
            id
        }

        /// Команда и её подтверждение; между ними квитанций нет.
        fn command(&mut self, command: Command) -> (u64, Option<String>) {
            let id = self.send(command);
            match self.receipt().event {
                Event::Ack {
                    id: acked,
                    bytes,
                    error,
                } => {
                    assert_eq!(acked, id);
                    (bytes, error)
                }
                other => panic!("ожидался ack {id}, пришло {other:?}"),
            }
        }

        fn type_bytes(&self, bytes: &[u8]) {
            let master = self.master.as_ref().unwrap();
            // SAFETY: буфер действителен на свою длину, дескриптор открыт.
            let written =
                unsafe { libc::write(master.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
            assert_eq!(written, bytes.len() as isize);
        }

        /// Читает вывод фикстуры, пока в нём не появится `needle`.
        fn screen_until(&mut self, needle: &[u8]) {
            let fd = self.master.as_ref().unwrap().as_raw_fd();
            let deadline = Instant::now() + WAIT;
            while count(&self.screen, needle) == 0 {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !left.is_zero(),
                    "на экране нет {:?}",
                    String::from_utf8_lossy(needle)
                );
                let mut set = MaybeUninit::<libc::fd_set>::uninit();
                let mut timeout = libc::timeval {
                    tv_sec: left.as_secs() as libc::time_t,
                    tv_usec: left.subsec_micros() as libc::suseconds_t,
                };
                // SAFETY: набор инициализируется `FD_ZERO` до использования, `fd` открыт.
                let ready = unsafe {
                    libc::FD_ZERO(set.as_mut_ptr());
                    libc::FD_SET(fd, set.as_mut_ptr());
                    libc::select(
                        fd + 1,
                        set.as_mut_ptr(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &mut timeout,
                    )
                };
                if ready <= 0 {
                    continue;
                }
                let mut buf = [0u8; READ_BUF];
                // SAFETY: буфер действителен на свою длину.
                let read = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                assert!(read > 0, "{}", io::Error::last_os_error());
                self.screen.extend_from_slice(&buf[..read as usize]);
            }
        }

        fn exit_receipt(&mut self) -> (String, u8) {
            loop {
                if let Event::Exit { reason, code, .. } = self.receipt().event {
                    return (reason, code);
                }
            }
        }

        fn join(&mut self) -> StopReason {
            self.fixture.take().unwrap().join().unwrap().unwrap()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.terminfo);
        }
    }

    fn terminfo_dir(terminfo: &Path) {
        std::fs::create_dir_all(terminfo.join("78")).unwrap();
        std::fs::write(terminfo.join("78/xterm-ghostty"), b"entry").unwrap();
    }

    #[test]
    fn fixture_reports_ready_reads_replies_and_winch_on_a_real_pty() {
        let mut harness = Harness::start(WAIT * 3);
        let ready = match harness.receipt() {
            Receipt {
                seq: 1,
                event: Event::Ready(ready),
                ..
            } => ready,
            other => panic!("первой должна быть готовность: {other:?}"),
        };
        assert_eq!(ready.nonce, "fx-test-1");
        assert_eq!(ready.env_nonce.as_deref(), Some("env-1"));
        assert_eq!(ready.term.as_deref(), Some("xterm-ghostty"));
        assert_eq!(
            ready.terminfo_entry,
            Some(
                harness
                    .terminfo
                    .join("78/xterm-ghostty")
                    .display()
                    .to_string()
            )
        );
        assert_eq!(ready.pid, std::process::id() as i32);
        assert_eq!(ready.size, Some(size(24, 80)));
        assert!(ready.tty.is_some_and(|tty| tty.starts_with("/dev/tty")));
        let mut names = vec![ENV_NONCE, "HOME", "TERM", "TERMINFO"];
        names.sort_unstable();
        assert_eq!(ready.env_names, names);

        assert_eq!(harness.command(Command::Raw { on: true }), (0, None));
        let before = clock::uptime_ns();
        harness.type_bytes("a\x1cж".as_bytes());
        let mut hex_read = String::new();
        let mut index = 0;
        while hex_read.len() < 8 {
            match harness.receipt() {
                Receipt {
                    t_ns,
                    event:
                        Event::Read {
                            index: got,
                            hex,
                            replied,
                        },
                    ..
                } => {
                    assert!(t_ns >= before);
                    assert_eq!(got, index + 1);
                    assert!(replied.is_empty());
                    index = got;
                    hex_read.push_str(&hex);
                }
                other => panic!("ожидалось чтение, пришло {other:?}"),
            }
        }
        assert_eq!(hex_read, "611cd0b6");

        let (_, error) = harness.command(Command::Reply {
            trigger: "?".to_owned(),
            answer: "ANSWER-42".to_owned(),
        });
        assert_eq!(error, None);
        let reply_id = harness.next_id;
        harness.type_bytes(b"?");
        match harness.receipt().event {
            Event::Read { hex, replied, .. } => {
                assert_eq!(hex, "3f");
                assert_eq!(replied.len(), 1);
                assert_eq!(replied[0].id, reply_id);
            }
            other => panic!("ожидалось чтение с ответом, пришло {other:?}"),
        }
        harness.screen_until(b"ANSWER-42");

        let master = harness.master.as_ref().unwrap().as_raw_fd();
        let resized = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 1000,
            ws_ypixel: 600,
        };
        // SAFETY: дескриптор открыт, структура живёт на время вызова.
        assert_eq!(
            unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &resized) },
            0
        );
        harness
            .inject
            .send(Input::Signal {
                t_ns: clock::uptime_ns(),
                signal: libc::SIGWINCH,
            })
            .unwrap();
        assert_eq!(
            harness.receipt().event,
            Event::Winch {
                size: Some(size(30, 100))
            }
        );
        harness
            .inject
            .send(Input::Signal {
                t_ns: clock::uptime_ns(),
                signal: libc::SIGQUIT,
            })
            .unwrap();
        assert_eq!(
            harness.receipt().event,
            Event::Signal {
                signal: "SIGQUIT".to_owned()
            }
        );

        harness.probe.write_all(b"{oops}\n").unwrap();
        assert!(matches!(harness.receipt().event, Event::Protocol { .. }));
        let (_, error) = harness.command(Command::KittyPop);
        assert!(error.is_some());
        let (_, error) = harness.command(Command::Marker {
            row: 31,
            col: 1,
            rows: 1,
            cols: 1,
            rgb: [0, 0, 0],
        });
        assert!(error.is_some());

        assert_eq!(
            harness.command(Command::BracketedPaste { on: true }),
            (8, None)
        );
        harness.screen_until(b"\x1b[?2004h");
        assert_eq!(harness.command(Command::KittyPush { flags: 3 }), (5, None));
        harness.screen_until(b"\x1b[>3u");
    }

    #[test]
    fn stream_runs_to_completion_and_can_be_stopped() {
        let mut harness = Harness::start(WAIT * 3);
        assert!(matches!(harness.receipt().event, Event::Ready(_)));
        // Без raw-режима ONLCR превратил бы `\r\n` в `\r\r\n`.
        harness.command(Command::Raw { on: true });
        let (_, error) = harness.command(Command::Stream {
            lines: 3,
            interval_us: 0,
            columns: 20,
            osc_every: 2,
            report_every: 2,
        });
        assert_eq!(error, None);
        let id = harness.next_id;
        let expected: u64 = (0..3)
            .map(|index| stream_line("fx-test-1", id, index, 20, 2).len() as u64)
            .sum();
        assert_eq!(
            harness.receipt().event,
            Event::StreamProgress {
                id,
                lines: 3,
                bytes: expected
            }
        );
        match harness.receipt().event {
            Event::StreamDone {
                id: done,
                lines,
                bytes,
                t_first_ns,
                cancelled,
            } => {
                assert_eq!((done, lines, bytes, cancelled), (id, 3, expected, false));
                assert!(t_first_ns.is_some());
            }
            other => panic!("ожидалось завершение потока, пришло {other:?}"),
        }
        harness.screen_until(b"stream 2 0000002 ...\r\n");
        harness.screen_until(b"\x1b]2;probe fx-test-1 1\x07");

        let (_, error) = harness.command(Command::Stream {
            lines: 1_000,
            interval_us: MAX_STREAM_INTERVAL_US,
            columns: 0,
            osc_every: 0,
            report_every: 0,
        });
        assert_eq!(error, None);
        let slow = harness.next_id;
        let (_, error) = harness.command(Command::Stream {
            lines: 1,
            interval_us: 0,
            columns: 0,
            osc_every: 0,
            report_every: 0,
        });
        assert!(error.is_some(), "второй поток при идущем");
        let stop = harness.send(Command::StopStream);
        match harness.receipt().event {
            Event::StreamDone {
                id,
                lines,
                cancelled,
                ..
            } => assert_eq!((id, lines, cancelled), (slow, 1, true)),
            other => panic!("ожидалась отмена потока, пришло {other:?}"),
        }
        assert!(
            matches!(harness.receipt().event, Event::Ack { id, error: None, .. } if id == stop)
        );
    }

    #[test]
    fn exit_command_restores_modes_and_ends_with_its_code() {
        let mut harness = Harness::start(WAIT * 3);
        assert!(matches!(harness.receipt().event, Event::Ready(_)));
        harness.command(Command::Raw { on: true });
        harness.command(Command::BracketedPaste { on: true });
        harness.command(Command::Mouse {
            mode: MouseMode::Click,
        });
        harness.command(Command::KittyPush { flags: 1 });
        assert_eq!(harness.command(Command::Exit { code: 7 }), (0, None));
        assert_eq!(harness.exit_receipt(), ("command".to_owned(), 7));
        assert_eq!(harness.join(), StopReason::Command(7));
        harness.screen_until(b"\x1b[<1u\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l");
    }

    #[test]
    fn fixture_stops_when_the_pty_closes() {
        let mut harness = Harness::start(WAIT * 3);
        assert!(matches!(harness.receipt().event, Event::Ready(_)));
        drop(harness.master.take());
        assert_eq!(harness.exit_receipt(), ("pty-closed".to_owned(), 0));
        assert!(matches!(harness.join(), StopReason::PtyClosed(_)));
    }

    #[test]
    fn fixture_stops_on_its_own_deadline_and_on_lifetime_change() {
        let mut harness = Harness::start(Duration::from_millis(100));
        assert!(matches!(harness.receipt().event, Event::Ready(_)));
        assert_eq!(
            harness.exit_receipt(),
            ("timeout".to_owned(), EXIT_EXECUTION)
        );
        assert_eq!(harness.join(), StopReason::Timeout);

        let mut harness = Harness::start(WAIT * 3);
        assert!(matches!(harness.receipt().event, Event::Ready(_)));
        let (_, error) = harness.command(Command::Lifetime { secs: 0 });
        assert!(error.is_some());
        assert_eq!(harness.command(Command::Lifetime { secs: 1 }), (0, None));
        assert_eq!(
            harness.exit_receipt(),
            ("timeout".to_owned(), EXIT_EXECUTION)
        );
    }

    #[test]
    fn fixture_stops_when_the_channel_closes_or_on_hangup() {
        let mut harness = Harness::start(WAIT * 3);
        assert!(matches!(harness.receipt().event, Event::Ready(_)));
        harness.probe.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(harness.exit_receipt(), ("channel-closed".to_owned(), 0));
        assert_eq!(harness.join(), StopReason::ChannelClosed);

        let mut harness = Harness::start(WAIT * 3);
        assert!(matches!(harness.receipt().event, Event::Ready(_)));
        harness.probe.write_all(b"{\"id\":9,").unwrap();
        harness
            .inject
            .send(Input::Signal {
                t_ns: clock::uptime_ns(),
                signal: libc::SIGHUP,
            })
            .unwrap();
        assert_eq!(harness.exit_receipt(), ("hangup".to_owned(), 0));
        assert_eq!(harness.join(), StopReason::Hangup);
    }
}
