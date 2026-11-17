//! Wire-формат Rust-хоста пробы (план P0.4, «Rust-хост»). Кадр: тег `u8`, длина тела `u32` в
//! порядке big-endian, тело. Числа в теле — big-endian, строки и байты — с длиной `u32` впереди.
//!
//! Соединение начинается с `Hello` клиента и `Welcome` или `Refused` хоста. Attach-клиент после
//! этого получает состояние VT-байтами (`Restore`, кадрами), затем живой поток `Output` с номером
//! первого байта PTY, шлёт ввод, ресайз и `Detach`. Ввод несёт источник и номер, хост отвечает
//! `InputAck`: принят, повтор уже принятого номера или отказ с причиной. Клиент, чья очередь
//! вывода переполнилась, получает `Evicted` с причиной и разрыв. Управляющее соединение (`list`, `stop`, ввод,
//! ресайз и снимок без клиента) клиентом сессии не считается. Соединение роли GUI (`Gui`)
//! привязывается к поколению лидера и несёт типизированные эффекты: чтение буфера обмена, фокус и
//! видимость. Соединение наблюдения (`Observe`, только проба) получает событие разбора `Parsed`
//! после каждого куска PTY.
//!
//! Модуль чистый, он идёт в `make check`.

use std::fmt;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::backend::{Lead, Role, Snapshot};
use crate::observer::ProcessKey;
use crate::pty::Winsize;

/// Версия протокола хоста. Задача 14 замораживает этот кодек как v1.
pub const VERSION: u16 = 1;
/// Предел тела кадра.
pub const MAX_BODY: usize = 1 << 20;
/// Тег и длина.
pub const HEADER: usize = 5;

/// Назначение соединения.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Purpose {
    /// Attach-клиент: живой поток, ввод, ресайз, лидерство.
    Attach,
    /// Операция без клиента: состояние, ввод, ресайз, остановка.
    Control,
    /// Роль GUI лидера: буфер обмена, фокус, видимость; привязка — `GuiBind`.
    Gui,
    /// Наблюдение пробы: события разбора для протокола разреза.
    Observe,
}

/// Счётчики чтения буфера обмена через роль GUI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipStats {
    pub requests: u32,
    pub answered: u32,
    pub timeouts: u32,
    /// Ожидание отменено сменой лидера.
    pub cancelled: u32,
    /// Запрос без GUI текущего лидера: пустой ответ сразу.
    pub no_gui: u32,
    /// Отброшенные ответы: чужое поколение, чужой запрос или ответ после срока.
    pub dropped: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u16,
    pub purpose: Purpose,
    pub lead: Lead,
    /// Сборка и поколение клиента для отчёта.
    pub client: String,
    /// Размер окна клиента; у управляющего соединения не применяется.
    pub size: Winsize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub version: u16,
    /// Сборка хоста.
    pub host: String,
    /// Ревизия VT хоста; до задачи 12 хост VT не держит.
    pub vt: String,
    pub session: String,
    pub role: Role,
    /// Номер назначения лидера: растёт с каждой сменой лидера.
    pub generation: u32,
    /// Номер байта PTY, с которого клиент получает живой поток.
    pub offset: u64,
}

/// Состояние сессии по запросу управляющего соединения.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub host: ProcessKey,
    pub workload: ProcessKey,
    pub workload_pgid: i32,
    pub workload_sid: Option<i32>,
    /// Подключённые attach-клиенты.
    pub clients: u32,
    pub leader: bool,
    /// Чтения PTY хостом.
    pub reads: u64,
    pub read_bytes: u64,
    /// Байты, поданные в VT хоста потоком разбора.
    pub parsed_bytes: u64,
    /// Байты ответов хоста, записанные в PTY нагрузки.
    pub replies: u64,
    /// Байты ответов VT хоста на переданные клиенту запросы: отброшены, отвечает клиент.
    pub dropped_replies: u64,
    pub clipboard: ClipStats,
    pub size: Winsize,
    /// Номер назначения текущего лидера; растёт с каждой сменой.
    pub generation: u32,
    /// Клиенты, разорванные переполнением очереди вывода.
    pub evictions: u32,
    /// Байты ввода в очереди записи в PTY.
    pub input_queued: u64,
}

/// Ответ хоста на ввод.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ack", content = "detail", rename_all = "kebab-case")]
pub enum Ack {
    Accepted,
    /// Номер уже принят от этого источника: ввод в PTY повторно не пишется.
    Duplicate,
    Rejected(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    // --- клиент → хост ---
    Hello(Hello),
    /// Ввод источника `source` с номером `seq`.
    Input {
        source: u64,
        seq: u64,
        bytes: Vec<u8>,
    },
    Resize(Winsize),
    StatusRequest,
    Stop,
    Detach,
    SnapshotRequest,
    /// Первое сообщение соединения `Gui`: поколение лидера, от имени которого оно говорит.
    GuiBind {
        generation: u32,
    },
    Focus(bool),
    Visible(bool),
    ClipboardReply {
        serial: u64,
        generation: u32,
        /// `None` — у GUI нет данных.
        data: Option<Vec<u8>>,
    },
    // --- хост → клиент ---
    Welcome(Welcome),
    Output {
        offset: u64,
        bytes: Vec<u8>,
    },
    Status(Status),
    /// Часть состояния VT-байтами до живого потока.
    Restore(Vec<u8>),
    Snapshot(Snapshot),
    ClipboardRequest {
        serial: u64,
        generation: u32,
    },
    /// Кусок PTY разобран: длина куска, разобрано всего, удержано от клиентов, VT в ground.
    Parsed {
        read: u64,
        offset: u64,
        held: u32,
        ground: bool,
    },
    Role {
        role: Role,
        generation: u32,
    },
    /// Нагрузка вышла: код или сигнал.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Refused(String),
    /// Подтверждение `Resize`, `Stop` и `Detach`.
    Done,
    InputAck {
        source: u64,
        seq: u64,
        ack: Ack,
    },
    /// Очередь вывода клиента переполнилась: причина, затем разрыв.
    Evicted(String),
}

const TAG_HELLO: u8 = 1;
const TAG_INPUT: u8 = 2;
const TAG_RESIZE: u8 = 3;
const TAG_STATUS_REQUEST: u8 = 4;
const TAG_STOP: u8 = 5;
const TAG_DETACH: u8 = 6;
const TAG_SNAPSHOT_REQUEST: u8 = 7;
const TAG_GUI_BIND: u8 = 8;
const TAG_FOCUS: u8 = 9;
const TAG_VISIBLE: u8 = 10;
const TAG_CLIPBOARD_REPLY: u8 = 11;
const TAG_WELCOME: u8 = 16;
const TAG_OUTPUT: u8 = 17;
const TAG_STATUS: u8 = 18;
const TAG_ROLE: u8 = 19;
const TAG_EXITED: u8 = 20;
const TAG_REFUSED: u8 = 21;
const TAG_DONE: u8 = 22;
const TAG_RESTORE: u8 = 23;
const TAG_SNAPSHOT: u8 = 24;
const TAG_CLIPBOARD_REQUEST: u8 = 25;
const TAG_PARSED: u8 = 26;
const TAG_INPUT_ACK: u8 = 27;
const TAG_EVICTED: u8 = 28;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtoError {
    UnknownTag(u8),
    TooLarge(usize),
    /// Тело короче, чем требуют поля, или длиннее разобранного.
    Malformed(&'static str),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::UnknownTag(tag) => write!(f, "неизвестный тег кадра {tag}"),
            ProtoError::TooLarge(len) => write!(f, "тело кадра {len} байт больше {MAX_BODY}"),
            ProtoError::Malformed(what) => write!(f, "повреждённый кадр: {what}"),
        }
    }
}

impl std::error::Error for ProtoError {}

// --- кодирование ---

struct Body(Vec<u8>);

impl Body {
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }
    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn i32(&mut self, value: i32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn i64(&mut self, value: i64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }
    fn opt_bytes(&mut self, value: Option<&[u8]>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.bytes(value);
            }
            None => self.u8(0),
        }
    }
    fn clip(&mut self, stats: ClipStats) {
        for value in [
            stats.requests,
            stats.answered,
            stats.timeouts,
            stats.cancelled,
            stats.no_gui,
            stats.dropped,
        ] {
            self.u32(value);
        }
    }
    fn opt_i32(&mut self, value: Option<i32>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.i32(value);
            }
            None => self.u8(0),
        }
    }
    fn bytes(&mut self, value: &[u8]) {
        self.u32(value.len() as u32);
        self.0.extend_from_slice(value);
    }
    fn str(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }
    fn size(&mut self, size: Winsize) {
        self.u16(size.rows);
        self.u16(size.cols);
        self.u16(size.xpixel);
        self.u16(size.ypixel);
    }
    fn key(&mut self, key: ProcessKey) {
        self.i32(key.pid);
        self.i64(key.start_us);
    }
}

fn lead_code(lead: Lead) -> u8 {
    match lead {
        Lead::IfFree => 0,
        Lead::Claim => 1,
    }
}

fn role_code(role: Role) -> u8 {
    match role {
        Role::Unowned => 0,
        Role::Leader => 1,
        Role::Follower => 2,
    }
}

/// Кадр сообщения целиком.
pub fn encode(message: &Message) -> Vec<u8> {
    let mut body = Body(Vec::new());
    let tag = match message {
        Message::Hello(hello) => {
            body.u16(hello.version);
            body.u8(match hello.purpose {
                Purpose::Attach => 0,
                Purpose::Control => 1,
                Purpose::Gui => 2,
                Purpose::Observe => 3,
            });
            body.u8(lead_code(hello.lead));
            body.str(&hello.client);
            body.size(hello.size);
            TAG_HELLO
        }
        Message::Input { source, seq, bytes } => {
            body.u64(*source);
            body.u64(*seq);
            body.bytes(bytes);
            TAG_INPUT
        }
        Message::Resize(size) => {
            body.size(*size);
            TAG_RESIZE
        }
        Message::StatusRequest => TAG_STATUS_REQUEST,
        Message::Stop => TAG_STOP,
        Message::Detach => TAG_DETACH,
        Message::SnapshotRequest => TAG_SNAPSHOT_REQUEST,
        Message::GuiBind { generation } => {
            body.u32(*generation);
            TAG_GUI_BIND
        }
        Message::Focus(on) => {
            body.bool(*on);
            TAG_FOCUS
        }
        Message::Visible(on) => {
            body.bool(*on);
            TAG_VISIBLE
        }
        Message::ClipboardReply {
            serial,
            generation,
            data,
        } => {
            body.u64(*serial);
            body.u32(*generation);
            body.opt_bytes(data.as_deref());
            TAG_CLIPBOARD_REPLY
        }
        Message::Restore(bytes) => {
            body.bytes(bytes);
            TAG_RESTORE
        }
        Message::Snapshot(snapshot) => {
            body.u64(snapshot.revision);
            body.u16(snapshot.cols);
            body.u16(snapshot.rows);
            body.u16(snapshot.cursor_x);
            body.u16(snapshot.cursor_y);
            body.bool(snapshot.alternate);
            body.str(&snapshot.text);
            TAG_SNAPSHOT
        }
        Message::ClipboardRequest { serial, generation } => {
            body.u64(*serial);
            body.u32(*generation);
            TAG_CLIPBOARD_REQUEST
        }
        Message::Parsed {
            read,
            offset,
            held,
            ground,
        } => {
            body.u64(*read);
            body.u64(*offset);
            body.u32(*held);
            body.bool(*ground);
            TAG_PARSED
        }
        Message::Welcome(welcome) => {
            body.u16(welcome.version);
            body.str(&welcome.host);
            body.str(&welcome.vt);
            body.str(&welcome.session);
            body.u8(role_code(welcome.role));
            body.u32(welcome.generation);
            body.u64(welcome.offset);
            TAG_WELCOME
        }
        Message::Output { offset, bytes } => {
            body.u64(*offset);
            body.bytes(bytes);
            TAG_OUTPUT
        }
        Message::Status(status) => {
            body.key(status.host);
            body.key(status.workload);
            body.i32(status.workload_pgid);
            body.opt_i32(status.workload_sid);
            body.u32(status.clients);
            body.u8(u8::from(status.leader));
            body.u64(status.reads);
            body.u64(status.read_bytes);
            body.u64(status.parsed_bytes);
            body.u64(status.replies);
            body.u64(status.dropped_replies);
            body.clip(status.clipboard);
            body.size(status.size);
            body.u32(status.generation);
            body.u32(status.evictions);
            body.u64(status.input_queued);
            TAG_STATUS
        }
        Message::Role { role, generation } => {
            body.u8(role_code(*role));
            body.u32(*generation);
            TAG_ROLE
        }
        Message::Exited { code, signal } => {
            body.opt_i32(*code);
            body.opt_i32(*signal);
            TAG_EXITED
        }
        Message::Refused(reason) => {
            body.str(reason);
            TAG_REFUSED
        }
        Message::Done => TAG_DONE,
        Message::InputAck { source, seq, ack } => {
            body.u64(*source);
            body.u64(*seq);
            match ack {
                Ack::Accepted => body.u8(0),
                Ack::Duplicate => body.u8(1),
                Ack::Rejected(reason) => {
                    body.u8(2);
                    body.str(reason);
                }
            }
            TAG_INPUT_ACK
        }
        Message::Evicted(reason) => {
            body.str(reason);
            TAG_EVICTED
        }
    };
    let mut frame = Vec::with_capacity(HEADER + body.0.len());
    frame.push(tag);
    frame.extend_from_slice(&(body.0.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body.0);
    frame
}

// --- разбор ---

struct Fields<'a> {
    rest: &'a [u8],
}

impl<'a> Fields<'a> {
    fn take(&mut self, len: usize, what: &'static str) -> Result<&'a [u8], ProtoError> {
        if self.rest.len() < len {
            return Err(ProtoError::Malformed(what));
        }
        let (head, rest) = self.rest.split_at(len);
        self.rest = rest;
        Ok(head)
    }
    fn array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N], ProtoError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N, what)?);
        Ok(out)
    }
    fn u8(&mut self, what: &'static str) -> Result<u8, ProtoError> {
        Ok(self.take(1, what)?[0])
    }
    fn u16(&mut self, what: &'static str) -> Result<u16, ProtoError> {
        Ok(u16::from_be_bytes(self.array(what)?))
    }
    fn u32(&mut self, what: &'static str) -> Result<u32, ProtoError> {
        Ok(u32::from_be_bytes(self.array(what)?))
    }
    fn u64(&mut self, what: &'static str) -> Result<u64, ProtoError> {
        Ok(u64::from_be_bytes(self.array(what)?))
    }
    fn i32(&mut self, what: &'static str) -> Result<i32, ProtoError> {
        Ok(i32::from_be_bytes(self.array(what)?))
    }
    fn i64(&mut self, what: &'static str) -> Result<i64, ProtoError> {
        Ok(i64::from_be_bytes(self.array(what)?))
    }
    fn bool(&mut self, what: &'static str) -> Result<bool, ProtoError> {
        match self.u8(what)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ProtoError::Malformed(what)),
        }
    }
    fn opt_bytes(&mut self, what: &'static str) -> Result<Option<Vec<u8>>, ProtoError> {
        match self.u8(what)? {
            0 => Ok(None),
            1 => Ok(Some(self.bytes(what)?)),
            _ => Err(ProtoError::Malformed(what)),
        }
    }
    fn clip(&mut self) -> Result<ClipStats, ProtoError> {
        Ok(ClipStats {
            requests: self.u32("clip.requests")?,
            answered: self.u32("clip.answered")?,
            timeouts: self.u32("clip.timeouts")?,
            cancelled: self.u32("clip.cancelled")?,
            no_gui: self.u32("clip.no_gui")?,
            dropped: self.u32("clip.dropped")?,
        })
    }
    fn opt_i32(&mut self, what: &'static str) -> Result<Option<i32>, ProtoError> {
        match self.u8(what)? {
            0 => Ok(None),
            1 => Ok(Some(self.i32(what)?)),
            _ => Err(ProtoError::Malformed(what)),
        }
    }
    fn bytes(&mut self, what: &'static str) -> Result<Vec<u8>, ProtoError> {
        let len = self.u32(what)? as usize;
        Ok(self.take(len, what)?.to_vec())
    }
    fn string(&mut self, what: &'static str) -> Result<String, ProtoError> {
        String::from_utf8(self.bytes(what)?).map_err(|_| ProtoError::Malformed(what))
    }
    fn size(&mut self, what: &'static str) -> Result<Winsize, ProtoError> {
        Ok(Winsize {
            rows: self.u16(what)?,
            cols: self.u16(what)?,
            xpixel: self.u16(what)?,
            ypixel: self.u16(what)?,
        })
    }
    fn key(&mut self, what: &'static str) -> Result<ProcessKey, ProtoError> {
        Ok(ProcessKey {
            pid: self.i32(what)?,
            start_us: self.i64(what)?,
        })
    }
    fn lead(&mut self) -> Result<Lead, ProtoError> {
        match self.u8("lead")? {
            0 => Ok(Lead::IfFree),
            1 => Ok(Lead::Claim),
            _ => Err(ProtoError::Malformed("lead")),
        }
    }
    fn role(&mut self) -> Result<Role, ProtoError> {
        match self.u8("role")? {
            0 => Ok(Role::Unowned),
            1 => Ok(Role::Leader),
            2 => Ok(Role::Follower),
            _ => Err(ProtoError::Malformed("role")),
        }
    }
}

/// Сообщение по тегу и телу кадра.
pub fn decode_body(tag: u8, body: &[u8]) -> Result<Message, ProtoError> {
    let mut fields = Fields { rest: body };
    let message = match tag {
        TAG_HELLO => Message::Hello(Hello {
            version: fields.u16("hello.version")?,
            purpose: match fields.u8("hello.purpose")? {
                0 => Purpose::Attach,
                1 => Purpose::Control,
                2 => Purpose::Gui,
                3 => Purpose::Observe,
                _ => return Err(ProtoError::Malformed("hello.purpose")),
            },
            lead: fields.lead()?,
            client: fields.string("hello.client")?,
            size: fields.size("hello.size")?,
        }),
        TAG_INPUT => Message::Input {
            source: fields.u64("input.source")?,
            seq: fields.u64("input.seq")?,
            bytes: fields.bytes("input.bytes")?,
        },
        TAG_RESIZE => Message::Resize(fields.size("resize")?),
        TAG_STATUS_REQUEST => Message::StatusRequest,
        TAG_STOP => Message::Stop,
        TAG_DETACH => Message::Detach,
        TAG_SNAPSHOT_REQUEST => Message::SnapshotRequest,
        TAG_GUI_BIND => Message::GuiBind {
            generation: fields.u32("gui_bind.generation")?,
        },
        TAG_FOCUS => Message::Focus(fields.bool("focus")?),
        TAG_VISIBLE => Message::Visible(fields.bool("visible")?),
        TAG_CLIPBOARD_REPLY => Message::ClipboardReply {
            serial: fields.u64("clipboard_reply.serial")?,
            generation: fields.u32("clipboard_reply.generation")?,
            data: fields.opt_bytes("clipboard_reply.data")?,
        },
        TAG_RESTORE => Message::Restore(fields.bytes("restore")?),
        TAG_SNAPSHOT => Message::Snapshot(Snapshot {
            revision: fields.u64("snapshot.revision")?,
            cols: fields.u16("snapshot.cols")?,
            rows: fields.u16("snapshot.rows")?,
            cursor_x: fields.u16("snapshot.cursor_x")?,
            cursor_y: fields.u16("snapshot.cursor_y")?,
            alternate: fields.bool("snapshot.alternate")?,
            text: fields.string("snapshot.text")?,
        }),
        TAG_CLIPBOARD_REQUEST => Message::ClipboardRequest {
            serial: fields.u64("clipboard_request.serial")?,
            generation: fields.u32("clipboard_request.generation")?,
        },
        TAG_PARSED => Message::Parsed {
            read: fields.u64("parsed.read")?,
            offset: fields.u64("parsed.offset")?,
            held: fields.u32("parsed.held")?,
            ground: fields.bool("parsed.ground")?,
        },
        TAG_WELCOME => Message::Welcome(Welcome {
            version: fields.u16("welcome.version")?,
            host: fields.string("welcome.host")?,
            vt: fields.string("welcome.vt")?,
            session: fields.string("welcome.session")?,
            role: fields.role()?,
            generation: fields.u32("welcome.generation")?,
            offset: fields.u64("welcome.offset")?,
        }),
        TAG_OUTPUT => Message::Output {
            offset: fields.u64("output.offset")?,
            bytes: fields.bytes("output.bytes")?,
        },
        TAG_STATUS => Message::Status(Status {
            host: fields.key("status.host")?,
            workload: fields.key("status.workload")?,
            workload_pgid: fields.i32("status.pgid")?,
            workload_sid: fields.opt_i32("status.sid")?,
            clients: fields.u32("status.clients")?,
            leader: match fields.u8("status.leader")? {
                0 => false,
                1 => true,
                _ => return Err(ProtoError::Malformed("status.leader")),
            },
            reads: fields.u64("status.reads")?,
            read_bytes: fields.u64("status.read_bytes")?,
            parsed_bytes: fields.u64("status.parsed_bytes")?,
            replies: fields.u64("status.replies")?,
            dropped_replies: fields.u64("status.dropped_replies")?,
            clipboard: fields.clip()?,
            size: fields.size("status.size")?,
            generation: fields.u32("status.generation")?,
            evictions: fields.u32("status.evictions")?,
            input_queued: fields.u64("status.input_queued")?,
        }),
        TAG_ROLE => Message::Role {
            role: fields.role()?,
            generation: fields.u32("role.generation")?,
        },
        TAG_EXITED => Message::Exited {
            code: fields.opt_i32("exited.code")?,
            signal: fields.opt_i32("exited.signal")?,
        },
        TAG_REFUSED => Message::Refused(fields.string("refused")?),
        TAG_DONE => Message::Done,
        TAG_INPUT_ACK => Message::InputAck {
            source: fields.u64("input_ack.source")?,
            seq: fields.u64("input_ack.seq")?,
            ack: match fields.u8("input_ack.ack")? {
                0 => Ack::Accepted,
                1 => Ack::Duplicate,
                2 => Ack::Rejected(fields.string("input_ack.reason")?),
                _ => return Err(ProtoError::Malformed("input_ack.ack")),
            },
        },
        TAG_EVICTED => Message::Evicted(fields.string("evicted")?),
        other => return Err(ProtoError::UnknownTag(other)),
    };
    if !fields.rest.is_empty() {
        return Err(ProtoError::Malformed("лишние байты в теле"));
    }
    Ok(message)
}

/// Разбор потока кадров, пришедшего произвольными кусками.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Следующее целое сообщение; `None` — кадр ещё не пришёл целиком.
    pub fn next_message(&mut self) -> Result<Option<Message>, ProtoError> {
        if self.buf.len() < HEADER {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > MAX_BODY {
            return Err(ProtoError::TooLarge(len));
        }
        if self.buf.len() < HEADER + len {
            return Ok(None);
        }
        let message = decode_body(self.buf[0], &self.buf[HEADER..HEADER + len])?;
        self.buf.drain(..HEADER + len);
        Ok(Some(message))
    }

    /// Байты, ждущие конца кадра.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

pub fn write_message(writer: &mut impl Write, message: &Message) -> io::Result<()> {
    writer.write_all(&encode(message))?;
    writer.flush()
}

/// Читает одно сообщение; `None` — соединение закрыто на границе кадра.
pub fn read_message(reader: &mut impl Read) -> io::Result<Option<Message>> {
    let mut header = [0u8; HEADER];
    let mut filled = 0;
    while filled < HEADER {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "соединение закрыто посреди заголовка кадра",
                ));
            }
            Ok(read) => filled += read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_BODY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            ProtoError::TooLarge(len),
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    decode_body(header[0], &body)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Message> {
        let size = Winsize {
            rows: 24,
            cols: 80,
            xpixel: 640,
            ypixel: 384,
        };
        let key = |pid| ProcessKey {
            pid,
            start_us: 1_794_854_604_000_123,
        };
        vec![
            Message::Hello(Hello {
                version: VERSION,
                purpose: Purpose::Attach,
                lead: Lead::Claim,
                client: "attach 0.1.0".to_owned(),
                size,
            }),
            Message::Input {
                source: 0x5eed,
                seq: 2,
                bytes: b"\x03ls\r".to_vec(),
            },
            Message::Resize(size),
            Message::StatusRequest,
            Message::Stop,
            Message::Detach,
            Message::Welcome(Welcome {
                version: VERSION,
                host: "host 0.1.0".to_owned(),
                vt: "нет".to_owned(),
                session: "s1-a1".to_owned(),
                role: Role::Leader,
                generation: 3,
                offset: 1 << 40,
            }),
            Message::Output {
                offset: 7,
                bytes: vec![0, 0x1b, b'[', 0xff],
            },
            Message::Status(Status {
                host: key(10),
                workload: key(11),
                workload_pgid: 11,
                workload_sid: None,
                clients: 2,
                leader: true,
                reads: 5,
                read_bytes: 4096,
                parsed_bytes: 4000,
                replies: 12,
                dropped_replies: 4,
                clipboard: ClipStats {
                    requests: 3,
                    answered: 1,
                    timeouts: 1,
                    cancelled: 0,
                    no_gui: 1,
                    dropped: 2,
                },
                size,
                generation: 3,
                evictions: 1,
                input_queued: 1 << 18,
            }),
            Message::SnapshotRequest,
            Message::GuiBind { generation: 2 },
            Message::Focus(true),
            Message::Visible(false),
            Message::ClipboardReply {
                serial: 9,
                generation: 2,
                data: Some(b"clip".to_vec()),
            },
            Message::ClipboardReply {
                serial: 10,
                generation: 2,
                data: None,
            },
            Message::Restore(b"\x1b[?25l".to_vec()),
            Message::Snapshot(Snapshot {
                revision: 4,
                cols: 80,
                rows: 24,
                cursor_x: 3,
                cursor_y: 1,
                alternate: true,
                text: "a\nb".to_owned(),
            }),
            Message::ClipboardRequest {
                serial: 9,
                generation: 2,
            },
            Message::Parsed {
                read: 3,
                offset: 1 << 33,
                held: 2,
                ground: false,
            },
            Message::Role {
                role: Role::Follower,
                generation: 4,
            },
            Message::Exited {
                code: Some(0),
                signal: None,
            },
            Message::Exited {
                code: None,
                signal: Some(9),
            },
            Message::Refused("нет сессии".to_owned()),
            Message::Done,
            Message::InputAck {
                source: 0x5eed,
                seq: 2,
                ack: Ack::Accepted,
            },
            Message::InputAck {
                source: 0x5eed,
                seq: 2,
                ack: Ack::Duplicate,
            },
            Message::InputAck {
                source: 1,
                seq: 9,
                ack: Ack::Rejected("очередь заполнена".to_owned()),
            },
            Message::Evicted("очередь клиента переполнена".to_owned()),
        ]
    }

    #[test]
    fn every_message_round_trips() {
        for message in samples() {
            let frame = encode(&message);
            let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
            assert_eq!(frame.len(), HEADER + len, "{message:?}");
            assert_eq!(decode_body(frame[0], &frame[HEADER..]), Ok(message.clone()));
            let mut reader = &frame[..];
            assert_eq!(read_message(&mut reader).unwrap(), Some(message));
            assert_eq!(read_message(&mut reader).unwrap(), None);
        }
    }

    #[test]
    fn wire_layout_is_tag_length_and_big_endian_fields() {
        assert_eq!(
            encode(&Message::Output {
                offset: 0x0102,
                bytes: b"ab".to_vec(),
            }),
            [
                17, 0, 0, 0, 14, 0, 0, 0, 0, 0, 0, 1, 2, 0, 0, 0, 2, b'a', b'b'
            ]
        );
        assert_eq!(encode(&Message::Done), [22, 0, 0, 0, 0]);
    }

    #[test]
    fn decoder_assembles_frames_cut_at_every_byte() {
        let stream: Vec<u8> = samples().iter().flat_map(encode).collect();
        for cut in 0..=stream.len() {
            let mut decoder = Decoder::default();
            let mut got = Vec::new();
            for part in [&stream[..cut], &stream[cut..]] {
                decoder.push(part);
                while let Some(message) = decoder.next_message().unwrap() {
                    got.push(message);
                }
            }
            assert_eq!(got, samples(), "разрез на {cut}");
            assert_eq!(decoder.pending(), 0);
        }
    }

    #[test]
    fn damaged_frames_are_rejected() {
        let mut decoder = Decoder::default();
        decoder.push(&[99, 0, 0, 0, 0]);
        assert_eq!(decoder.next_message(), Err(ProtoError::UnknownTag(99)));

        let mut decoder = Decoder::default();
        decoder.push(&[TAG_INPUT, 0xff, 0xff, 0xff, 0xff]);
        assert!(matches!(
            decoder.next_message(),
            Err(ProtoError::TooLarge(_))
        ));

        // Длина строки внутри тела больше самого тела.
        assert!(matches!(
            decode_body(TAG_REFUSED, &[0, 0, 0, 9, b'x']),
            Err(ProtoError::Malformed(_))
        ));
        assert!(matches!(
            decode_body(TAG_DONE, &[1]),
            Err(ProtoError::Malformed(_))
        ));
        assert!(matches!(
            decode_body(TAG_ROLE, &[7, 0, 0, 0, 1]),
            Err(ProtoError::Malformed(_))
        ));
        assert!(matches!(
            decode_body(
                TAG_INPUT_ACK,
                &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 3]
            ),
            Err(ProtoError::Malformed(_))
        ));
        let input = Message::Input {
            source: 1,
            seq: 1,
            bytes: b"abc".to_vec(),
        };
        let mut cut = &encode(&input)[..7];
        assert_eq!(
            read_message(&mut cut).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
