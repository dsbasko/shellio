//! Wire-формат Rust-хоста пробы (план P0.4, «Rust-хост», «Версии и обновление»). Кадр: тег `u8`,
//! длина тела `u32` в порядке big-endian, тело. Числа в теле — big-endian, строки и байты — с длиной
//! `u32` впереди.
//!
//! Соединение начинается рукопожатием: `Hello` клиента с наибольшей версией протокола, которую он
//! понимает, и `Welcome` хоста с выбранной версией или `Refused` с причиной. Кадры рукопожатия
//! (`Hello`, `Welcome`, `Refused`) одинаковы во всех версиях, остальные кадры обе стороны пишут и
//! разбирают кодеком выбранной версии ([`Codec`]). Версия 1 заморожена в [`v1`] вместе с векторами
//! `vectors/v1.txt`; текущая версия 2 добавляет в `Input` признак `ack`.
//!
//! Attach-клиент после рукопожатия получает состояние VT-байтами (`Restore`, кадрами), затем живой
//! поток `Output` с номером первого байта PTY, шлёт ввод, ресайз и `Detach`. Снимок `GHOSTSNP`
//! в протокол не входит: клиент VT не держит, поэтому формат снимка в совместимость поколений не
//! входит. Ввод несёт источник и номер, хост отвечает `InputAck`: принят, повтор уже принятого
//! номера или отказ с причиной. Клиент, чья очередь вывода переполнилась, получает `Evicted` с
//! причиной и разрыв. Управляющее соединение (`list`, `stop`, ввод, ресайз и снимок без клиента)
//! клиентом сессии не считается. Соединение роли GUI (`Gui`) привязывается к поколению лидера и
//! несёт типизированные эффекты: чтение буфера обмена, фокус и видимость. Соединение наблюдения
//! (`Observe`, только проба) получает событие разбора `Parsed` после каждого куска PTY.
//!
//! Модуль чистый, он идёт в `make check`.

pub mod v1;

use std::fmt;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::backend::{Lead, Role, Snapshot};
use crate::observer::ProcessKey;
use crate::pty::Winsize;

/// Версия текущего кодека.
pub const VERSION: u16 = 2;
/// Самая ранняя версия, кодек которой есть в этой сборке.
pub const MIN_VERSION: u16 = 1;
/// Версии, которые понимает эта сборка. Хост с функцией `proto-v1` — предыдущее поколение для G1
/// и G2: он знает только замороженный v1.
pub const SUPPORTED: Versions = if cfg!(feature = "proto-v1") {
    Versions {
        min: MIN_VERSION,
        max: v1::VERSION,
    }
} else {
    Versions {
        min: MIN_VERSION,
        max: VERSION,
    }
};
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
    /// Ввод источника `source` с номером `seq`. `ack = false` (с версии 2) — клиент подтверждения
    /// не читает, хост его не шлёт; в версии 1 признака нет, подтверждается каждый ввод.
    Input {
        source: u64,
        seq: u64,
        bytes: Vec<u8>,
        ack: bool,
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

/// Диапазон версий протокола стороны.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versions {
    pub min: u16,
    pub max: u16,
}

impl fmt::Display for Versions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.min == self.max {
            write!(f, "{}", self.min)
        } else {
            write!(f, "{}–{}", self.min, self.max)
        }
    }
}

impl Versions {
    pub fn contains(self, version: u16) -> bool {
        (self.min..=self.max).contains(&version)
    }

    /// Хост: версия соединения по наибольшей версии клиента из `Hello` — общая наибольшая, иначе
    /// отказ с причиной.
    pub fn choose(self, client_max: u16) -> Result<Codec, String> {
        let chosen = client_max.min(self.max);
        if chosen < self.min {
            return Err(format!(
                "несовместимое поколение: клиент понимает протокол до {client_max}, хост — {self}"
            ));
        }
        Codec::of(chosen).ok_or_else(|| format!("у сборки нет кодека протокола {chosen}"))
    }

    /// Клиент: версия, которую выбрал хост, должна быть в его диапазоне.
    pub fn confirm(self, chosen: u16) -> Result<Codec, String> {
        if !self.contains(chosen) {
            return Err(format!(
                "несовместимое поколение: хост выбрал протокол {chosen}, клиент понимает {self}"
            ));
        }
        Codec::of(chosen).ok_or_else(|| format!("у сборки нет кодека протокола {chosen}"))
    }
}

/// Кодек кадров после рукопожатия.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    /// Замороженный [`v1`].
    V1,
    /// Текущий: `Input` с признаком `ack`.
    V2,
}

impl Codec {
    /// Кадры рукопожатия одинаковы во всех версиях; до `Welcome` стороны пишут и читают ими.
    pub const HANDSHAKE: Codec = Codec::V1;

    pub fn of(version: u16) -> Option<Codec> {
        match version {
            v1::VERSION => Some(Codec::V1),
            VERSION => Some(Codec::V2),
            _ => None,
        }
    }

    pub fn version(self) -> u16 {
        match self {
            Codec::V1 => v1::VERSION,
            Codec::V2 => VERSION,
        }
    }
}

// --- кодирование ---

struct Body(Vec<u8>);

impl Body {
    fn frame(self, tag: u8) -> Vec<u8> {
        let mut frame = Vec::with_capacity(HEADER + self.0.len());
        frame.push(tag);
        frame.extend_from_slice(&(self.0.len() as u32).to_be_bytes());
        frame.extend_from_slice(&self.0);
        frame
    }
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

/// Кадр сообщения кодеком `codec`.
pub fn encode(codec: Codec, message: &Message) -> Vec<u8> {
    match codec {
        Codec::V1 => v1::encode(message),
        Codec::V2 => encode_v2(message),
    }
}

/// Кадр в формате версии 2: v1 и признак `ack` у `Input`.
fn encode_v2(message: &Message) -> Vec<u8> {
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
        Message::Input {
            source,
            seq,
            bytes,
            ack,
        } => {
            body.u64(*source);
            body.u64(*seq);
            body.bytes(bytes);
            body.bool(*ack);
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
    body.frame(tag)
}

// --- разбор ---

struct Fields<'a> {
    rest: &'a [u8],
}

impl<'a> Fields<'a> {
    fn end(&self) -> Result<(), ProtoError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(ProtoError::Malformed("лишние байты в теле"))
        }
    }
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

/// Сообщение по тегу и телу кадра кодеком `codec`.
pub fn decode_body(codec: Codec, tag: u8, body: &[u8]) -> Result<Message, ProtoError> {
    match codec {
        Codec::V1 => v1::decode_body(tag, body),
        Codec::V2 => decode_v2(tag, body),
    }
}

fn decode_v2(tag: u8, body: &[u8]) -> Result<Message, ProtoError> {
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
            ack: fields.bool("input.ack")?,
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
    fields.end()?;
    Ok(message)
}

/// Разбор потока кадров, пришедшего произвольными кусками.
#[derive(Debug)]
pub struct Decoder {
    codec: Codec,
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new(codec: Codec) -> Decoder {
        Decoder {
            codec,
            buf: Vec::new(),
        }
    }

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
        let message = decode_body(self.codec, self.buf[0], &self.buf[HEADER..HEADER + len])?;
        self.buf.drain(..HEADER + len);
        Ok(Some(message))
    }

    /// Байты, ждущие конца кадра.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

pub fn write_message(writer: &mut impl Write, codec: Codec, message: &Message) -> io::Result<()> {
    writer.write_all(&encode(codec, message))?;
    writer.flush()
}

/// Читает одно сообщение; `None` — соединение закрыто на границе кадра.
pub fn read_message(reader: &mut impl Read, codec: Codec) -> io::Result<Option<Message>> {
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
    decode_body(codec, header[0], &body)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Отказ рукопожатия на стороне клиента.
#[derive(Debug)]
pub enum HandshakeError {
    Io(io::Error),
    /// Хост ответил `Refused`.
    Refused(String),
    /// Хост выбрал версию вне диапазона клиента: несовместимое поколение.
    Incompatible(String),
    /// Вместо `Welcome` пришло другое сообщение или соединение закрылось.
    Unexpected(String),
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HandshakeError::Io(err) => write!(f, "рукопожатие: {err}"),
            HandshakeError::Refused(reason) => write!(f, "хост отказал: {reason}"),
            HandshakeError::Incompatible(reason) | HandshakeError::Unexpected(reason) => {
                f.write_str(reason)
            }
        }
    }
}

/// Рукопожатие клиента: `Hello` с наибольшей версией из `versions`, затем `Welcome` с версией,
/// которую выбрал хост, и её кодек для всех следующих кадров.
pub fn handshake<S: Read + Write>(
    stream: &mut S,
    versions: Versions,
    purpose: Purpose,
    lead: Lead,
    client: String,
    size: Winsize,
) -> Result<(Welcome, Codec), HandshakeError> {
    let hello = Message::Hello(Hello {
        version: versions.max,
        purpose,
        lead,
        client,
        size,
    });
    write_message(stream, Codec::HANDSHAKE, &hello).map_err(HandshakeError::Io)?;
    match read_message(stream, Codec::HANDSHAKE).map_err(HandshakeError::Io)? {
        Some(Message::Welcome(welcome)) => {
            let codec = versions
                .confirm(welcome.version)
                .map_err(HandshakeError::Incompatible)?;
            Ok((welcome, codec))
        }
        Some(Message::Refused(reason)) => Err(HandshakeError::Refused(reason)),
        Some(other) => Err(HandshakeError::Unexpected(format!(
            "вместо welcome: {other:?}"
        ))),
        None => Err(HandshakeError::Unexpected(
            "хост закрыл соединение до welcome".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_VECTORS: &str = include_str!("vectors/v1.txt");

    fn size() -> Winsize {
        Winsize {
            rows: 24,
            cols: 80,
            xpixel: 640,
            ypixel: 384,
        }
    }

    /// Сообщения в порядке векторов `vectors/v1.txt` с их именами; у `Input` — значение `ack`,
    /// которое даёт разбор v1.
    fn named() -> Vec<(&'static str, Message)> {
        let size = size();
        let key = |pid| ProcessKey {
            pid,
            start_us: 1_794_854_604_000_123,
        };
        vec![
            (
                "hello",
                Message::Hello(Hello {
                    version: 1,
                    purpose: Purpose::Attach,
                    lead: Lead::Claim,
                    client: "attach 0.1.0".to_owned(),
                    size,
                }),
            ),
            (
                "input",
                Message::Input {
                    source: 0x5eed,
                    seq: 2,
                    bytes: b"\x03ls\r".to_vec(),
                    ack: true,
                },
            ),
            ("resize", Message::Resize(size)),
            ("status-request", Message::StatusRequest),
            ("stop", Message::Stop),
            ("detach", Message::Detach),
            (
                "welcome",
                Message::Welcome(Welcome {
                    version: 1,
                    host: "host 0.1.0".to_owned(),
                    vt: "нет".to_owned(),
                    session: "s1-a1".to_owned(),
                    role: Role::Leader,
                    generation: 3,
                    offset: 1 << 40,
                }),
            ),
            (
                "output",
                Message::Output {
                    offset: 7,
                    bytes: vec![0, 0x1b, b'[', 0xff],
                },
            ),
            (
                "status",
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
            ),
            ("snapshot-request", Message::SnapshotRequest),
            ("gui-bind", Message::GuiBind { generation: 2 }),
            ("focus", Message::Focus(true)),
            ("visible", Message::Visible(false)),
            (
                "clipboard-reply-data",
                Message::ClipboardReply {
                    serial: 9,
                    generation: 2,
                    data: Some(b"clip".to_vec()),
                },
            ),
            (
                "clipboard-reply-empty",
                Message::ClipboardReply {
                    serial: 10,
                    generation: 2,
                    data: None,
                },
            ),
            ("restore", Message::Restore(b"\x1b[?25l".to_vec())),
            (
                "snapshot",
                Message::Snapshot(Snapshot {
                    revision: 4,
                    cols: 80,
                    rows: 24,
                    cursor_x: 3,
                    cursor_y: 1,
                    alternate: true,
                    text: "a\nb".to_owned(),
                }),
            ),
            (
                "clipboard-request",
                Message::ClipboardRequest {
                    serial: 9,
                    generation: 2,
                },
            ),
            (
                "parsed",
                Message::Parsed {
                    read: 3,
                    offset: 1 << 33,
                    held: 2,
                    ground: false,
                },
            ),
            (
                "role",
                Message::Role {
                    role: Role::Follower,
                    generation: 4,
                },
            ),
            (
                "exited-code",
                Message::Exited {
                    code: Some(0),
                    signal: None,
                },
            ),
            (
                "exited-signal",
                Message::Exited {
                    code: None,
                    signal: Some(9),
                },
            ),
            ("refused", Message::Refused("нет сессии".to_owned())),
            ("done", Message::Done),
            (
                "input-ack-accepted",
                Message::InputAck {
                    source: 0x5eed,
                    seq: 2,
                    ack: Ack::Accepted,
                },
            ),
            (
                "input-ack-duplicate",
                Message::InputAck {
                    source: 0x5eed,
                    seq: 2,
                    ack: Ack::Duplicate,
                },
            ),
            (
                "input-ack-rejected",
                Message::InputAck {
                    source: 1,
                    seq: 9,
                    ack: Ack::Rejected("очередь заполнена".to_owned()),
                },
            ),
            (
                "evicted",
                Message::Evicted("очередь клиента переполнена".to_owned()),
            ),
        ]
    }

    /// Все сообщения текущей версии, включая ввод без подтверждения.
    fn samples() -> Vec<Message> {
        let mut samples: Vec<Message> = named().into_iter().map(|(_, message)| message).collect();
        samples.push(Message::Input {
            source: 0x5eed,
            seq: 3,
            bytes: b"q".to_vec(),
            ack: false,
        });
        samples
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    fn v1_vectors() -> Vec<(&'static str, Vec<u8>)> {
        V1_VECTORS
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let (name, frame) = line.split_once(' ').unwrap();
                (name, unhex(frame))
            })
            .collect()
    }

    #[test]
    fn v1_vectors_cover_every_tag_and_stay_frozen() {
        let vectors = v1_vectors();
        let named = named();
        assert_eq!(
            vectors.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            named.iter().map(|(name, _)| *name).collect::<Vec<_>>()
        );
        let mut tags: Vec<u8> = vectors.iter().map(|(_, frame)| frame[0]).collect();
        tags.dedup();
        let expected: Vec<u8> = (TAG_HELLO..=TAG_CLIPBOARD_REPLY)
            .chain(TAG_WELCOME..=TAG_EVICTED)
            .collect();
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, expected, "у каждого тега v1 есть вектор");
        for ((name, frame), (_, message)) in vectors.iter().zip(&named) {
            assert_eq!(&v1::encode(message), frame, "кодек v1 изменился: {name}");
        }
    }

    #[test]
    fn v1_vectors_are_decoded_by_the_current_code() {
        for ((name, frame), (_, message)) in v1_vectors().iter().zip(named()) {
            let mut reader = &frame[..];
            assert_eq!(
                read_message(&mut reader, Codec::V1).unwrap(),
                Some(message),
                "{name}"
            );
            assert!(reader.is_empty(), "{name}");
        }
    }

    #[test]
    fn handshake_frames_are_the_same_in_every_version() {
        for (name, message) in named() {
            if matches!(
                message,
                Message::Hello(_) | Message::Welcome(_) | Message::Refused(_)
            ) {
                assert_eq!(
                    encode(Codec::V1, &message),
                    encode(Codec::V2, &message),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn version_2_adds_ack_to_input_and_v1_rejects_it() {
        let input = Message::Input {
            source: 0x0102,
            seq: 3,
            bytes: b"q".to_vec(),
            ack: false,
        };
        let v2 = encode(Codec::V2, &input);
        assert_eq!(
            v2,
            [
                2, 0, 0, 0, 22, 0, 0, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 1, b'q', 0
            ]
        );
        // Кодек v1 признак не пишет: кадр — v2 без последнего байта, разбор даёт подтверждение.
        let v1 = encode(Codec::V1, &input);
        assert_eq!(&v1[HEADER..], &v2[HEADER..v2.len() - 1]);
        assert!(matches!(
            decode_body(Codec::V1, TAG_INPUT, &v2[HEADER..]),
            Err(ProtoError::Malformed(_))
        ));
        assert!(matches!(
            decode_body(Codec::V2, TAG_INPUT, &v1[HEADER..]),
            Err(ProtoError::Malformed(_))
        ));
        assert_eq!(
            decode_body(Codec::V1, TAG_INPUT, &v1[HEADER..]),
            Ok(Message::Input {
                source: 0x0102,
                seq: 3,
                bytes: b"q".to_vec(),
                ack: true,
            })
        );
    }

    #[test]
    fn negotiation_picks_the_highest_common_version() {
        let current = Versions { min: 1, max: 2 };
        let previous = Versions { min: 1, max: 1 };
        // Текущий клиент и хост предыдущего поколения.
        assert_eq!(previous.choose(current.max), Ok(Codec::V1));
        assert_eq!(current.confirm(1), Ok(Codec::V1));
        assert_eq!(current.choose(current.max), Ok(Codec::V2));
        // Клиент предыдущего поколения и текущий хост.
        assert_eq!(current.choose(previous.max), Ok(Codec::V1));
        // Клиент будущего поколения: хост выбирает свою наибольшую.
        assert_eq!(current.choose(7), Ok(Codec::V2));
        assert_eq!(Codec::of(3), None);
        assert_eq!(Codec::V2.version(), VERSION);
        assert!(SUPPORTED.contains(MIN_VERSION) && SUPPORTED.min <= SUPPORTED.max);
    }

    #[test]
    fn incompatible_generation_is_refused_with_a_reason() {
        // Хост, отказавшийся от v1, и клиент, который знает только v1.
        let host = Versions { min: 2, max: 2 };
        let refused = host.choose(1).unwrap_err();
        assert!(refused.contains("несовместимое поколение"), "{refused}");
        assert!(
            refused.contains("до 1") && refused.contains("хост — 2"),
            "{refused}"
        );
        // Клиент, отказавшийся от v1, и хост, выбравший v1.
        let client = Versions { min: 2, max: 2 };
        let refused = client.confirm(1).unwrap_err();
        assert!(refused.contains("выбрал протокол 1"), "{refused}");
        assert!(refused.contains("понимает 2"), "{refused}");
        // Хост выбрал версию выше наибольшей клиента.
        let refused = Versions { min: 1, max: 2 }.confirm(3).unwrap_err();
        assert!(refused.contains("понимает 1–2"), "{refused}");
    }

    /// Хост на другом конце сокета: принимает `Hello` и отвечает тем, что решит `answer`.
    fn host_answers(
        answer: impl FnOnce(Hello) -> Message + Send + 'static,
    ) -> std::os::unix::net::UnixStream {
        let (client, mut host) = std::os::unix::net::UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let Ok(Some(Message::Hello(hello))) = read_message(&mut host, Codec::HANDSHAKE) else {
                return;
            };
            let _ = write_message(&mut host, Codec::HANDSHAKE, &answer(hello));
        });
        client
    }

    fn welcome_at(version: u16) -> Message {
        Message::Welcome(Welcome {
            version,
            host: "host".to_owned(),
            vt: "vt".to_owned(),
            session: "s".to_owned(),
            role: Role::Unowned,
            generation: 0,
            offset: 0,
        })
    }

    #[test]
    fn client_handshake_offers_its_highest_version_and_follows_the_host() {
        let previous = Versions { min: 1, max: 1 };
        let mut stream = host_answers(move |hello| match previous.choose(hello.version) {
            Ok(codec) => welcome_at(codec.version()),
            Err(reason) => Message::Refused(reason),
        });
        let (welcome, codec) = handshake(
            &mut stream,
            Versions { min: 1, max: 2 },
            Purpose::Control,
            Lead::IfFree,
            "c".to_owned(),
            size(),
        )
        .unwrap();
        assert_eq!((welcome.version, codec), (1, Codec::V1));

        let mut stream = host_answers(|_| welcome_at(1));
        let err = handshake(
            &mut stream,
            Versions { min: 2, max: 2 },
            Purpose::Attach,
            Lead::IfFree,
            "c".to_owned(),
            size(),
        )
        .unwrap_err();
        assert!(matches!(err, HandshakeError::Incompatible(_)), "{err}");

        let mut stream = host_answers(|_| Message::Refused("занято".to_owned()));
        let err = handshake(
            &mut stream,
            SUPPORTED,
            Purpose::Control,
            Lead::IfFree,
            "c".to_owned(),
            size(),
        )
        .unwrap_err();
        assert!(matches!(err, HandshakeError::Refused(ref reason) if reason == "занято"));
    }

    #[test]
    fn every_message_round_trips_in_the_current_version() {
        for message in samples() {
            let frame = encode(Codec::V2, &message);
            let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
            assert_eq!(frame.len(), HEADER + len, "{message:?}");
            assert_eq!(
                decode_body(Codec::V2, frame[0], &frame[HEADER..]),
                Ok(message.clone())
            );
            let mut reader = &frame[..];
            assert_eq!(read_message(&mut reader, Codec::V2).unwrap(), Some(message));
            assert_eq!(read_message(&mut reader, Codec::V2).unwrap(), None);
        }
    }

    #[test]
    fn wire_layout_is_tag_length_and_big_endian_fields() {
        for codec in [Codec::V1, Codec::V2] {
            assert_eq!(
                encode(
                    codec,
                    &Message::Output {
                        offset: 0x0102,
                        bytes: b"ab".to_vec(),
                    }
                ),
                [
                    17, 0, 0, 0, 14, 0, 0, 0, 0, 0, 0, 1, 2, 0, 0, 0, 2, b'a', b'b'
                ]
            );
            assert_eq!(encode(codec, &Message::Done), [22, 0, 0, 0, 0]);
        }
    }

    #[test]
    fn decoder_assembles_frames_cut_at_every_byte() {
        let stream: Vec<u8> = samples()
            .iter()
            .flat_map(|message| encode(Codec::V2, message))
            .collect();
        for cut in 0..=stream.len() {
            let mut decoder = Decoder::new(Codec::V2);
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
        for codec in [Codec::V1, Codec::V2] {
            let mut decoder = Decoder::new(codec);
            decoder.push(&[99, 0, 0, 0, 0]);
            assert_eq!(decoder.next_message(), Err(ProtoError::UnknownTag(99)));

            let mut decoder = Decoder::new(codec);
            decoder.push(&[TAG_INPUT, 0xff, 0xff, 0xff, 0xff]);
            assert!(matches!(
                decoder.next_message(),
                Err(ProtoError::TooLarge(_))
            ));

            // Длина строки внутри тела больше самого тела.
            assert!(matches!(
                decode_body(codec, TAG_REFUSED, &[0, 0, 0, 9, b'x']),
                Err(ProtoError::Malformed(_))
            ));
            assert!(matches!(
                decode_body(codec, TAG_DONE, &[1]),
                Err(ProtoError::Malformed(_))
            ));
            assert!(matches!(
                decode_body(codec, TAG_ROLE, &[7, 0, 0, 0, 1]),
                Err(ProtoError::Malformed(_))
            ));
            assert!(matches!(
                decode_body(
                    codec,
                    TAG_INPUT_ACK,
                    &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 3]
                ),
                Err(ProtoError::Malformed(_))
            ));
            let input = Message::Input {
                source: 1,
                seq: 1,
                bytes: b"abc".to_vec(),
                ack: true,
            };
            let mut cut = &encode(codec, &input)[..7];
            assert_eq!(
                read_message(&mut cut, codec).unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
    }
}
