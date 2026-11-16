//! Эталон и приёмник на `libghostty-vt` ревизии поверхности (функция `vt`, ADR-0035), раздел плана
//! «Оракулы».
//!
//! - [`Reference`] получает исходные байты фикстуры и ресайзы без переподключений. Обратного
//!   маршрута у него нет: `WRITE_PTY` не ставится, поэтому он не отвечает даже на DA.
//! - [`Receiver`] получает только вывод attach-клиента кандидата и отвечает на запросы по
//!   объявленному профилю колбэков ([`ReceiverProfile`]). Его ответы [`Route`] пишет обратно в PTY
//!   attach-клиента, дальше они идут через кандидата в PTY нагрузки, как у настоящего терминала.
//!   У каждого клиента свой приёмник и свой журнал эффектов.
//!
//! Состояние снимается в [`model::State`]. Сохранённый курсор и стек Kitty C API не отдаёт: они
//! снимаются на копии терминала, которая получает тот же журнал ввода заново (повтор), а не через
//! снимок, полнота которого сама проверяется пробой.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::model::{self, Observed, State};
use super::suffix::Sink;
use crate::fixture::channel::{self, Command, Event, Link};
use crate::pty::{self, Winsize};
use crate::vt::{self, Area};

pub type Result<T> = std::result::Result<T, vt::Error>;

/// Предел журнала ввода по умолчанию: сверх него скрытое состояние не наблюдается.
pub const DEFAULT_LOG_CAP: usize = 64 << 20;
/// Снятие вершины стека Kitty (`CSI < u`), повтор выдаёт стек целиком.
const KITTY_POP: &[u8] = b"\x1b[<u";
/// `CAN`: обрывает начатую последовательность без исполнения перед зондом на копии.
const CANCEL: &[u8] = b"\x18";
const RESTORE_CURSOR: &[u8] = b"\x1b8";
const READ_BUF: usize = 64 << 10;

// --- конфигурация ---

/// Конфигурация терминала-оракула. Эталон и приёмники одной клетки получают одинаковую.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u32,
    pub cell_height: u32,
    pub scrollback_lines: usize,
    /// Предел continuation; ноль — не отслеживается.
    pub continuation_max: usize,
    pub foreground: Option<vt::Rgb>,
    pub background: Option<vt::Rgb>,
    pub cursor: Option<vt::Rgb>,
    /// Предел журнала ввода для повтора.
    pub log_cap: usize,
}

impl Config {
    /// 10 000 строк истории (раздел «Замеры»), клетка 10×20 пикселей, светлый текст на тёмном фоне.
    pub fn new(cols: u16, rows: u16) -> Config {
        Config {
            cols,
            rows,
            cell_width: 10,
            cell_height: 20,
            scrollback_lines: 10_000,
            continuation_max: 0,
            foreground: Some(vt::Rgb {
                r: 0xdd,
                g: 0xdd,
                b: 0xdd,
            }),
            background: Some(vt::Rgb {
                r: 0x1e,
                g: 0x1e,
                b: 0x1e,
            }),
            cursor: Some(vt::Rgb {
                r: 0xff,
                g: 0xcc,
                b: 0x00,
            }),
            log_cap: DEFAULT_LOG_CAP,
        }
    }

    /// Размер PTY attach-клиента для окна `cols × rows` с пикселями по размеру клетки.
    pub fn winsize(&self, cols: u16, rows: u16) -> Winsize {
        let pixels = |cells: u16, cell: u32| {
            u16::try_from(u32::from(cells).saturating_mul(cell)).unwrap_or(u16::MAX)
        };
        Winsize {
            rows,
            cols,
            xpixel: pixels(cols, self.cell_width),
            ypixel: pixels(rows, self.cell_height),
        }
    }
}

/// Что снимать.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capture {
    /// Сколько строк истории снимать, от ближайшей к активной области.
    pub history: usize,
    /// Снимать ли скрытое состояние повтором журнала.
    pub hidden: bool,
}

impl Default for Capture {
    fn default() -> Capture {
        Capture {
            history: usize::MAX,
            hidden: true,
        }
    }
}

// --- журнал ввода ---

#[derive(Clone, Debug)]
enum Logged {
    Bytes(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// Ввод терминала по порядку для повтора на копии. Сверх предела журнал сбрасывается и больше не
/// ведётся: неполный повтор дал бы чужое скрытое состояние.
#[derive(Debug)]
struct Log {
    entries: Vec<Logged>,
    bytes: usize,
    cap: usize,
    overflow: bool,
}

impl Log {
    fn new(cap: usize) -> Log {
        Log {
            entries: Vec::new(),
            bytes: 0,
            cap,
            overflow: false,
        }
    }

    fn bytes(&mut self, bytes: &[u8]) {
        if self.overflow || bytes.is_empty() {
            return;
        }
        if self.bytes.saturating_add(bytes.len()) > self.cap {
            self.overflow = true;
            self.entries = Vec::new();
            return;
        }
        self.bytes += bytes.len();
        match self.entries.last_mut() {
            Some(Logged::Bytes(last)) => last.extend_from_slice(bytes),
            _ => self.entries.push(Logged::Bytes(bytes.to_vec())),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        if !self.overflow {
            self.entries.push(Logged::Resize { cols, rows });
        }
    }
}

// --- терминал-оракул ---

/// Общая часть эталона и приёмника: терминал, конфигурация и журнал ввода.
struct Instance {
    term: vt::Terminal,
    config: Config,
    log: Log,
    fed: u64,
}

impl Instance {
    fn new(config: &Config, log_cap: usize) -> Result<Instance> {
        let mut term = vt::Terminal::new(config.cols, config.rows)?;
        term.set_scrollback_max_lines(config.scrollback_lines)?;
        if config.continuation_max > 0 {
            term.set_continuation_max_bytes(config.continuation_max)?;
        }
        term.set_default_colors(config.foreground, config.background, config.cursor)?;
        term.resize(
            config.cols,
            config.rows,
            config.cell_width,
            config.cell_height,
        )?;
        Ok(Instance {
            term,
            config: config.clone(),
            log: Log::new(log_cap),
            fed: 0,
        })
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.term.write(bytes);
        self.log.bytes(bytes);
        self.fed += bytes.len() as u64;
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.term
            .resize(cols, rows, self.config.cell_width, self.config.cell_height)?;
        self.log.resize(cols, rows);
        Ok(())
    }

    fn capture(&self, options: Capture) -> Result<State> {
        let term = &self.term;
        let cols = term.cols()?;
        let rows = term.rows()?;
        let active = (0..rows)
            .map(|y| row(term, Area::Active, cols, u32::from(y)))
            .collect::<Result<Vec<_>>>()?;
        let total = term.scrollback_rows()?;
        let history = (1..=total.min(options.history))
            .map(|distance| {
                let y = u32::try_from(total - distance).unwrap_or(u32::MAX);
                row(term, Area::History, cols, y)
            })
            .collect::<Result<Vec<_>>>()?;
        let cursor = term.cursor()?;
        let modes = model::TRACKED_MODES
            .iter()
            .map(|mode| {
                let value = if mode.ansi {
                    vt::Mode::ansi(mode.value)
                } else {
                    vt::Mode::dec(mode.value)
                };
                term.mode(value).map(|on| (mode.key(), on))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let colors = term.colors()?;
        let (saved_cursor, kitty_stack) = if !options.hidden {
            let reason = "скрытое состояние не запрошено".to_owned();
            (
                Observed::NotObserved(reason.clone()),
                Observed::NotObserved(reason),
            )
        } else if self.log.overflow {
            let reason = format!("журнал ввода превысил {} байт", self.log.cap);
            (
                Observed::NotObserved(reason.clone()),
                Observed::NotObserved(reason),
            )
        } else {
            let (saved, stack) = self.hidden()?;
            (Observed::Value(saved), Observed::Value(stack))
        };
        Ok(State {
            cols,
            rows,
            screen: match term.active_screen()? {
                vt::Screen::Primary => model::Screen::Primary,
                vt::Screen::Alternate => model::Screen::Alternate,
            },
            active,
            history,
            history_total: total,
            cursor: model::Cursor {
                x: cursor.x,
                y: cursor.y,
                pending_wrap: cursor.pending_wrap,
                visible: cursor.visible,
                style: style(&cursor.style),
            },
            modes,
            kitty_flags: term.kitty_keyboard_flags()?,
            colors: model::Colors {
                foreground: colors.foreground.map(rgb),
                background: colors.background.map(rgb),
                cursor: colors.cursor.map(rgb),
                palette: colors.palette.into_iter().map(rgb).collect(),
            },
            title: String::from_utf8_lossy(&term.title()?).into_owned(),
            pwd: String::from_utf8_lossy(&term.pwd()?).into_owned(),
            input: input_encodings(term, &self.config, cols, rows)?,
            ground: term.vt_ground()?,
            saved_cursor,
            kitty_stack,
        })
    }

    /// Повтор журнала на копии и зонд скрытого состояния: снятие стека Kitty, затем `DECRC`.
    fn hidden(&self) -> Result<(model::SavedCursor, Vec<u8>)> {
        let mut copy = Instance::new(&self.config, 0)?;
        for entry in &self.log.entries {
            match entry {
                Logged::Bytes(bytes) => copy.term.write(bytes),
                Logged::Resize { cols, rows } => copy.resize(*cols, *rows)?,
            }
        }
        copy.term.write(CANCEL);
        let mut stack = Vec::with_capacity(model::KITTY_STACK_DEPTH);
        for _ in 0..model::KITTY_STACK_DEPTH {
            stack.push(copy.term.kitty_keyboard_flags()?);
            copy.term.write(KITTY_POP);
        }
        while stack.last() == Some(&0) {
            stack.pop();
        }
        copy.term.write(RESTORE_CURSOR);
        let cursor = copy.term.cursor()?;
        Ok((
            model::SavedCursor {
                x: cursor.x,
                y: cursor.y,
                pending_wrap: cursor.pending_wrap,
                style: style(&cursor.style),
                origin: copy.term.mode(vt::Mode::ORIGIN)?,
            },
            stack,
        ))
    }
}

fn rgb(color: vt::Rgb) -> model::Rgb {
    model::Rgb {
        r: color.r,
        g: color.g,
        b: color.b,
    }
}

fn color(color: vt::Color) -> model::Color {
    match color {
        vt::Color::None => model::Color::Default,
        vt::Color::Palette(index) => model::Color::Palette(index),
        vt::Color::Rgb(r, g, b) => model::Color::Rgb(model::Rgb { r, g, b }),
    }
}

fn style(style: &vt::Style) -> model::Style {
    model::Style {
        fg: color(style.fg),
        bg: color(style.bg),
        underline_color: color(style.underline_color),
        bold: style.bold,
        italic: style.italic,
        faint: style.faint,
        blink: style.blink,
        inverse: style.inverse,
        invisible: style.invisible,
        strikethrough: style.strikethrough,
        overline: style.overline,
        underline: style.underline,
    }
}

fn row(term: &vt::Terminal, area: Area, cols: u16, y: u32) -> Result<model::Row> {
    let mut cells = Vec::with_capacity(usize::from(cols));
    let mut flags = (false, false);
    for x in 0..cols {
        let cell = term.cell(area, x, y)?;
        if x == 0 {
            flags = (cell.row_wrap, cell.row_wrap_continuation);
        }
        cells.push(model::Cell {
            text: cell
                .text
                .iter()
                .map(|&code| char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect(),
            width: match cell.wide {
                vt::Wide::Narrow => model::Width::Narrow,
                vt::Wide::Wide => model::Width::Wide,
                vt::Wide::SpacerTail => model::Width::SpacerTail,
                vt::Wide::SpacerHead => model::Width::SpacerHead,
            },
            style: style(&cell.style),
            hyperlink: cell.hyperlink,
            protected: cell.protected,
        });
    }
    Ok(model::Row {
        cells,
        wrap: flags.0,
        wrap_continuation: flags.1,
    })
}

enum Probe {
    Key(vt::KeyInput),
    Mouse(vt::MouseInput),
}

/// Событие ввода для имени из [`model::INPUT_PROBES`]. Мышь — у клетки (2, 1).
fn input_probe(name: &str, config: &Config) -> Option<Probe> {
    let special = |key: vt::Key, mods: u16| {
        Probe::Key(vt::KeyInput {
            key,
            action: vt::KeyAction::Press,
            mods,
            text: String::new(),
            unshifted: None,
        })
    };
    let at = |column: u32, line: u32| {
        (
            (column * config.cell_width + config.cell_width / 2) as f32,
            (line * config.cell_height + config.cell_height / 2) as f32,
        )
    };
    let mouse = |action, button, (x, y): (f32, f32)| {
        Probe::Mouse(vt::MouseInput {
            action,
            button,
            mods: 0,
            x,
            y,
        })
    };
    Some(match name {
        "key:a" => Probe::Key(vt::KeyInput::press(vt::Key::A, "a")),
        "key:ctrl-a" => Probe::Key(vt::KeyInput {
            mods: vt::MODS_CTRL,
            text: String::new(),
            ..vt::KeyInput::press(vt::Key::A, "a")
        }),
        "key:enter" => special(vt::Key::ENTER, 0),
        "key:tab" => special(vt::Key::TAB, 0),
        "key:backspace" => special(vt::Key::BACKSPACE, 0),
        "key:escape" => special(vt::Key::ESCAPE, 0),
        "key:up" => special(vt::Key::ARROW_UP, 0),
        "key:shift-up" => special(vt::Key::ARROW_UP, vt::MODS_SHIFT),
        "key:numpad-1" => Probe::Key(vt::KeyInput::press(vt::Key::NUMPAD_1, "1")),
        "mouse:left-press" => mouse(
            vt::MouseAction::Press,
            Some(vt::MouseButton::Left),
            at(2, 1),
        ),
        "mouse:left-release" => mouse(
            vt::MouseAction::Release,
            Some(vt::MouseButton::Left),
            at(2, 1),
        ),
        "mouse:motion" => mouse(vt::MouseAction::Motion, None, at(3, 1)),
        _ => return None,
    })
}

fn input_encodings(
    term: &vt::Terminal,
    config: &Config,
    cols: u16,
    rows: u16,
) -> Result<BTreeMap<String, String>> {
    let mut keys = vt::KeyEncoder::new()?;
    keys.sync_from(term);
    let mut mouse = vt::MouseEncoder::new(vt::MouseGeometry {
        screen_width: u32::from(cols) * config.cell_width,
        screen_height: u32::from(rows) * config.cell_height,
        cell_width: config.cell_width,
        cell_height: config.cell_height,
    })?;
    mouse.sync_from(term);
    let mut out = BTreeMap::new();
    for name in model::INPUT_PROBES {
        let bytes = match input_probe(name, config) {
            Some(Probe::Key(input)) => keys.encode(&input)?,
            Some(Probe::Mouse(input)) => mouse.encode(&input)?,
            None => continue,
        };
        out.insert(name.to_owned(), channel::hex(&bytes));
    }
    Ok(out)
}

// --- эталон ---

/// Эталон: исходные байты фикстуры и ресайзы без переподключений, без обратного маршрута.
pub struct Reference {
    inner: Instance,
}

impl Reference {
    pub fn new(config: &Config) -> Result<Reference> {
        Ok(Reference {
            inner: Instance::new(config, config.log_cap)?,
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.inner.feed(bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.inner.resize(cols, rows)
    }

    pub fn capture(&self, options: Capture) -> Result<State> {
        self.inner.capture(options)
    }

    /// Сколько байтов получено.
    pub fn fed(&self) -> u64 {
        self.inner.fed
    }

    pub fn terminal(&self) -> &vt::Terminal {
        &self.inner.term
    }
}

impl Sink for Reference {
    fn output(&mut self, bytes: &[u8]) -> std::result::Result<(), String> {
        self.feed(bytes);
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> std::result::Result<(), String> {
        Reference::resize(self, cols, rows).map_err(|err| err.to_string())
    }
}

// --- приёмник ---

/// Профиль колбэков приёмника: ответы на DA, XTVERSION, размер, цветовую схему и чтение буфера
/// обмена. DSR, DECRQM и цвета OSC 10/11/4 терминал отвечает сам по своему состоянию и цветам из
/// [`Config`]; фокус (1004) и размер (2048) — [`Receiver::focus`] и [`Receiver::resize`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiverProfile {
    pub device_attributes: vt::DeviceAttributes,
    pub xtversion: String,
    pub color_scheme: vt::ColorScheme,
    pub clipboard: Option<Vec<u8>>,
}

impl ReceiverProfile {
    /// DA1 отличается от значения `libghostty-vt` по умолчанию (`CSI ? 62 ; 22 c`) признаком 52,
    /// а XTVERSION называет клиента: по квитанции фикстуры видно, кто ответил.
    pub fn for_client(client: &str) -> ReceiverProfile {
        ReceiverProfile {
            device_attributes: vt::DeviceAttributes {
                conformance_level: 62,
                features: vec![22, 52],
                device_type: 1,
                firmware_version: 7,
                unit_id: 0,
            },
            xtversion: format!("shellio-probe-receiver({client})"),
            color_scheme: vt::ColorScheme::Dark,
            clipboard: Some(b"shellio-probe-clipboard".to_vec()),
        }
    }
}

/// Запись журнала приёмника: что он сделал и после скольких полученных байтов.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub at: u64,
    pub event: Journaled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Journaled {
    /// Байты, которые приёмник пишет обратно в PTY attach-клиента.
    Reply {
        hex: String,
    },
    ClipboardWrite {
        location: i32,
        /// Пары «MIME, данные в hex»; пусто — очистка.
        contents: Vec<(String, String)>,
    },
    ClipboardRead {
        location: i32,
        mimes: Vec<String>,
        answered: bool,
    },
    Bell,
    TitleChanged,
}

/// Приёмник одного клиента.
pub struct Receiver {
    client: String,
    profile: ReceiverProfile,
    inner: Instance,
    cols: u16,
    rows: u16,
    journal: Vec<JournalEntry>,
}

impl Receiver {
    pub fn new(client: &str, config: &Config, profile: ReceiverProfile) -> Result<Receiver> {
        let mut receiver = Receiver {
            client: client.to_owned(),
            profile,
            inner: Instance::new(config, config.log_cap)?,
            cols: config.cols,
            rows: config.rows,
            journal: Vec::new(),
        };
        let responder = receiver.responder();
        receiver.inner.term.enable_effects(responder)?;
        Ok(receiver)
    }

    fn responder(&self) -> vt::Responder {
        vt::Responder {
            device_attributes: Some(self.profile.device_attributes.clone()),
            xtversion: Some(self.profile.xtversion.clone()),
            size: Some(vt::SizeReport {
                rows: self.rows,
                columns: self.cols,
                cell_width: self.inner.config.cell_width,
                cell_height: self.inner.config.cell_height,
            }),
            color_scheme: Some(self.profile.color_scheme),
            clipboard: self.profile.clipboard.clone(),
        }
    }

    pub fn client(&self) -> &str {
        &self.client
    }

    /// Байты от attach-клиента. Возвращает ответы, которые надо записать обратно в его PTY.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let at = self.inner.fed;
        self.inner.feed(bytes);
        self.collect(at)
    }

    /// Ресайз окна клиента. Возвращает отчёт размера режима 2048, если режим включён.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<Vec<u8>> {
        let at = self.inner.fed;
        self.inner.resize(cols, rows)?;
        self.cols = cols;
        self.rows = rows;
        let responder = self.responder();
        self.inner.term.set_responder(responder);
        Ok(self.collect(at))
    }

    /// Окно клиента получило или потеряло фокус. Отчёт — только при включённом режиме 1004.
    pub fn focus(&mut self, gained: bool) -> Result<Vec<u8>> {
        if !self.inner.term.mode(vt::Mode::FOCUS_EVENT)? {
            return Ok(Vec::new());
        }
        let report = vt::focus_report(gained)?;
        self.journal.push(JournalEntry {
            at: self.inner.fed,
            event: Journaled::Reply {
                hex: channel::hex(&report),
            },
        });
        Ok(report)
    }

    fn collect(&mut self, at: u64) -> Vec<u8> {
        for effect in self.inner.term.take_effects() {
            let event = match effect {
                vt::Effect::Bell => Journaled::Bell,
                vt::Effect::TitleChanged => Journaled::TitleChanged,
                vt::Effect::ClipboardWrite { location, contents } => Journaled::ClipboardWrite {
                    location,
                    contents: contents
                        .into_iter()
                        .map(|(mime, data)| (mime, channel::hex(&data)))
                        .collect(),
                },
                vt::Effect::ClipboardRead {
                    location,
                    mimes,
                    answered,
                } => Journaled::ClipboardRead {
                    location,
                    mimes,
                    answered,
                },
            };
            self.journal.push(JournalEntry { at, event });
        }
        let replies = self.inner.term.take_pty_output();
        if !replies.is_empty() {
            self.journal.push(JournalEntry {
                at,
                event: Journaled::Reply {
                    hex: channel::hex(&replies),
                },
            });
        }
        replies
    }

    pub fn journal(&self) -> &[JournalEntry] {
        &self.journal
    }

    pub fn capture(&self, options: Capture) -> Result<State> {
        self.inner.capture(options)
    }

    /// Сколько байтов получено от attach-клиента.
    pub fn fed(&self) -> u64 {
        self.inner.fed
    }

    pub fn terminal(&self) -> &vt::Terminal {
        &self.inner.term
    }
}

// --- обратный маршрут ---

/// Счётчики маршрута: прочитано из PTY attach-клиента, записано в него (ответы и ввод) и сколько
/// ввода стенд поставил в очередь.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteStats {
    pub read: u64,
    pub written: u64,
    pub input: u64,
}

/// Почему маршрут закончился.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "kebab-case")]
pub enum RouteEnd {
    /// PTY закрыт с той стороны: attach-клиент вышел.
    Closed,
    Stopped,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitError {
    Timeout,
    Ended(RouteEnd),
}

impl std::fmt::Display for WaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaitError::Timeout => f.write_str("срок ожидания приёмника вышел"),
            WaitError::Ended(end) => write!(f, "маршрут закончился до условия: {end:?}"),
        }
    }
}

struct Pump {
    receiver: Receiver,
    /// Ответы приёмника и ввод стенда, ещё не записанные в PTY.
    pending: Vec<u8>,
    stats: RouteStats,
    end: Option<RouteEnd>,
    stop: bool,
}

struct Shared {
    pump: Mutex<Pump>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Pump> {
        self.pump
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Приёмник, подключённый к PTY attach-клиента. Поток маршрута читает вывод клиента в приёмник и
/// пишет обратно его ответы и ввод стенда. Писатель в PTY один — поток маршрута: мастер
/// неблокирующий, остановленный клиент не держит стенд.
pub struct Route {
    shared: Arc<Shared>,
    master: OwnedFd,
    wake: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl Route {
    /// `master` — мастер PTY attach-клиента; флаг `O_NONBLOCK` ставится на его открытый файл.
    pub fn start(master: OwnedFd, receiver: Receiver) -> io::Result<Route> {
        set_nonblocking(master.as_raw_fd())?;
        let (wake_read, wake_write) = wake_pipe()?;
        let shared = Arc::new(Shared {
            pump: Mutex::new(Pump {
                receiver,
                pending: Vec::new(),
                stats: RouteStats::default(),
                end: None,
                stop: false,
            }),
            changed: Condvar::new(),
        });
        let reader = master.try_clone()?;
        let thread = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("oracle-route".to_owned())
                .spawn(move || pump(&shared, &reader, &wake_read))?
        };
        Ok(Route {
            shared,
            master,
            wake: wake_write,
            thread: Some(thread),
        })
    }

    fn wake(&self) {
        // SAFETY: запись одного байта из стека в свой неблокирующий конец канала; переполненный
        // канал уже разбудит поток.
        unsafe { libc::write(self.wake.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }

    /// Доступ к приёмнику под блокировкой маршрута: снятие состояния, журнал.
    pub fn with<R>(&self, f: impl FnOnce(&mut Receiver) -> R) -> R {
        f(&mut self.shared.lock().receiver)
    }

    pub fn stats(&self) -> RouteStats {
        self.shared.lock().stats
    }

    pub fn end(&self) -> Option<RouteEnd> {
        self.shared.lock().end.clone()
    }

    /// Ввод пользователя в окно клиента: идёт в PTY attach-клиента после уже поставленных ответов.
    pub fn input(&self, bytes: &[u8]) {
        {
            let mut pump = self.shared.lock();
            pump.pending.extend_from_slice(bytes);
            pump.stats.input += bytes.len() as u64;
        }
        self.wake();
    }

    /// Ресайз окна клиента, как у терминала: сначала сетка приёмника, затем размер PTY. Между ними
    /// приёмник байтов не получает.
    pub fn resize(&self, cols: u16, rows: u16) -> std::result::Result<(), String> {
        {
            let mut pump = self.shared.lock();
            let config = pump.receiver.inner.config.clone();
            let replies = pump
                .receiver
                .resize(cols, rows)
                .map_err(|err| err.to_string())?;
            pump.pending.extend_from_slice(&replies);
            pty::set_winsize(self.master.as_fd(), config.winsize(cols, rows))
                .map_err(|err| format!("TIOCSWINSZ: {err}"))?;
        }
        self.wake();
        Ok(())
    }

    /// Окно клиента получило или потеряло фокус.
    pub fn focus(&self, gained: bool) -> std::result::Result<(), String> {
        {
            let mut pump = self.shared.lock();
            let report = pump.receiver.focus(gained).map_err(|err| err.to_string())?;
            pump.pending.extend_from_slice(&report);
        }
        self.wake();
        Ok(())
    }

    /// Ждёт условия над приёмником и счётчиками; проверяется после каждого изменения.
    pub fn wait_until(
        &self,
        timeout: Duration,
        mut done: impl FnMut(&Receiver, &RouteStats) -> bool,
    ) -> std::result::Result<(), WaitError> {
        let deadline = Instant::now() + timeout;
        let mut pump = self.shared.lock();
        loop {
            if done(&pump.receiver, &pump.stats) {
                return Ok(());
            }
            if let Some(end) = &pump.end {
                return Err(WaitError::Ended(end.clone()));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(WaitError::Timeout);
            }
            pump = match self.shared.changed.wait_timeout(pump, left) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Останавливает поток маршрута. Приёмник остаётся доступен через [`Route::with`].
    pub fn stop(&mut self) -> RouteEnd {
        if let Some(thread) = self.thread.take() {
            self.shared.lock().stop = true;
            self.wake();
            let _ = thread.join();
        }
        self.end().unwrap_or(RouteEnd::Stopped)
    }
}

impl Drop for Route {
    fn drop(&mut self) {
        self.stop();
    }
}

fn pump(shared: &Shared, master: &OwnedFd, wake: &OwnedFd) {
    let mut buf = vec![0u8; READ_BUF];
    let end = loop {
        let want_write = {
            let pump = shared.lock();
            if pump.stop {
                break RouteEnd::Stopped;
            }
            !pump.pending.is_empty()
        };
        let ready = match select(master.as_raw_fd(), wake.as_raw_fd(), want_write) {
            Ok(ready) => ready,
            Err(err) => break RouteEnd::Failed(format!("select: {err}")),
        };
        if ready.wake {
            drain(wake.as_raw_fd());
        }
        if ready.read {
            match read(master.as_raw_fd(), &mut buf) {
                Ok(0) => break RouteEnd::Closed,
                Ok(len) => {
                    let mut pump = shared.lock();
                    let replies = pump.receiver.feed(&buf[..len]);
                    pump.pending.extend_from_slice(&replies);
                    pump.stats.read += len as u64;
                    shared.changed.notify_all();
                }
                Err(err) if retry(&err) => {}
                // Ведомая сторона закрыта всеми: macOS отвечает мастеру EIO.
                Err(err) if err.raw_os_error() == Some(libc::EIO) => break RouteEnd::Closed,
                Err(err) => break RouteEnd::Failed(format!("чтение PTY: {err}")),
            }
        }
        if ready.write {
            let mut pump = shared.lock();
            if !pump.pending.is_empty() {
                match write(master.as_raw_fd(), &pump.pending) {
                    Ok(len) => {
                        pump.pending.drain(..len);
                        pump.stats.written += len as u64;
                        shared.changed.notify_all();
                    }
                    Err(err) if retry(&err) => {}
                    Err(err) if err.raw_os_error() == Some(libc::EIO) => break RouteEnd::Closed,
                    Err(err) => break RouteEnd::Failed(format!("запись PTY: {err}")),
                }
            }
        }
    };
    let mut pump = shared.lock();
    pump.end = Some(end);
    shared.changed.notify_all();
}

fn retry(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

struct Ready {
    read: bool,
    write: bool,
    wake: bool,
}

/// `select` без срока: чтение мастера и канала пробуждения, запись мастера по `want_write`.
/// Через `select`, а не `poll`: `poll` на macOS не поддерживает устройства (как `pty::wait_ready`).
fn select(master: RawFd, wake: RawFd, want_write: bool) -> io::Result<Ready> {
    for fd in [master, wake] {
        if usize::try_from(fd).map_or(true, |fd| fd >= libc::FD_SETSIZE) {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
    }
    loop {
        let mut reads = std::mem::MaybeUninit::<libc::fd_set>::uninit();
        let mut writes = std::mem::MaybeUninit::<libc::fd_set>::uninit();
        // SAFETY: FD_ZERO инициализирует наборы, дескрипторы меньше FD_SETSIZE.
        let (mut reads, mut writes) = unsafe {
            libc::FD_ZERO(reads.as_mut_ptr());
            libc::FD_ZERO(writes.as_mut_ptr());
            libc::FD_SET(master, reads.as_mut_ptr());
            libc::FD_SET(wake, reads.as_mut_ptr());
            if want_write {
                libc::FD_SET(master, writes.as_mut_ptr());
            }
            (reads.assume_init(), writes.assume_init())
        };
        // SAFETY: наборы живут на стеке на время вызова, срока нет.
        let ready = unsafe {
            libc::select(
                master.max(wake) + 1,
                &mut reads,
                &mut writes,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ready >= 0 {
            // SAFETY: наборы инициализированы и заполнены select.
            return Ok(unsafe {
                Ready {
                    read: libc::FD_ISSET(master, &reads),
                    write: libc::FD_ISSET(master, &writes),
                    wake: libc::FD_ISSET(wake, &reads),
                }
            });
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: буфер принадлежит вызывающему и пишется не дальше своей длины.
    let len = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    usize::try_from(len).map_err(|_| io::Error::last_os_error())
}

fn write(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: байты читаются не дальше своей длины.
    let len = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    usize::try_from(len).map_err(|_| io::Error::last_os_error())
}

fn drain(fd: RawFd) {
    let mut buf = [0u8; 64];
    while matches!(read(fd, &mut buf), Ok(len) if len > 0) {}
}

fn set_flags(fd: RawFd, get: libc::c_int, set: libc::c_int, flag: libc::c_int) -> io::Result<()> {
    // SAFETY: флаги открытого дескриптора читаются и пишутся без указателей.
    let status = unsafe {
        let flags = libc::fcntl(fd, get);
        if flags < 0 {
            flags
        } else {
            libc::fcntl(fd, set, flags | flag)
        }
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    set_flags(fd, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)
}

/// Канал пробуждения потока маршрута: оба конца неблокирующие и с `FD_CLOEXEC`. Создаётся под
/// блокировкой запусков `pty`, чтобы концы не унаследовал процесс, запущенный из другого потока.
fn wake_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let _guard = pty::spawn_lock();
    let mut fds = [-1 as RawFd; 2];
    // SAFETY: массив из двух дескрипторов на стеке.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe вернул два открытых дескриптора, которыми больше никто не владеет.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [read_end.as_raw_fd(), write_end.as_raw_fd()] {
        set_flags(fd, libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
        set_nonblocking(fd)?;
    }
    Ok((read_end, write_end))
}

// --- подача через кандидата ---

/// Барьер: запрос состояния DSR, на который приёмник отвечает `CSI 0 n` после всех предыдущих
/// байтов. Состояния терминала он не меняет, поэтому эталону не подаётся.
pub const BARRIER: &[u8] = b"\x1b[5n";
const BARRIER_REPLY: &[u8] = b"\x1b[0n";

/// Подача суффикса приёмнику через кандидата: вывод пишет фикстура нагрузки, ресайз идёт от окна
/// attach-клиента. Шаг заканчивается барьером: фикстура пишет [`BARRIER`], а квитанция её ответа
/// доказывает, что приёмник обработал всё до него. Барьер требует, чтобы кандидат передавал DSR
/// клиенту, и ставится только на границе последовательности: внутри начатой он стал бы её частью.
pub struct ThroughCandidate<'a> {
    pub link: &'a mut Link,
    pub route: &'a Route,
    pub timeout: Duration,
}

impl ThroughCandidate<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::result::Result<(), String> {
        let written = self
            .link
            .request(
                Command::Write {
                    hex: channel::hex(bytes),
                },
                self.timeout,
            )
            .map_err(|err| format!("запись фикстуры: {err}"))?;
        if written != bytes.len() as u64 {
            return Err(format!(
                "фикстура записала {written} байт из {}",
                bytes.len()
            ));
        }
        Ok(())
    }

    pub fn barrier(&mut self) -> std::result::Result<(), String> {
        self.write(BARRIER)?;
        let reply = channel::hex(BARRIER_REPLY);
        self.link
            .wait_for(self.timeout, |receipt| match &receipt.event {
                Event::Response { hex, .. } if *hex == reply => Some(()),
                _ => None,
            })
            .map_err(|err| format!("барьер: ответа приёмника нет: {err}"))
    }
}

impl Sink for ThroughCandidate<'_> {
    fn output(&mut self, bytes: &[u8]) -> std::result::Result<(), String> {
        self.write(bytes)?;
        self.barrier()
    }

    fn resize(&mut self, cols: u16, rows: u16) -> std::result::Result<(), String> {
        self.route.resize(cols, rows)?;
        self.link
            .wait_for(self.timeout, |receipt| match &receipt.event {
                Event::Winch { size: Some(size) } if size.cols == cols && size.rows == rows => {
                    Some(())
                }
                _ => None,
            })
            .map_err(|err| format!("нагрузка не получила размер {cols}×{rows}: {err}"))?;
        self.barrier()
    }
}

#[cfg(test)]
mod tests;
