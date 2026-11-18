//! Протокол минимального хоста пробы (план P0.5, «Протокол хоста»). Кадр: тег `u8`, длина тела
//! `u32` в порядке big-endian, тело. Числа в теле — big-endian, строки и байты — с длиной `u32`
//! впереди.
//!
//! Соединение начинается рукопожатием: `Hello` клиента несёт назначение и **диапазон** версий
//! клиента, хост выбирает общую наибольшую версию ([`negotiate`]) и отвечает `Welcome` или
//! `Refused` с причиной. Раскладка `Hello`, `Welcome` и `Refused` заморожена: хост любого
//! поколения разбирает `Hello` клиента любого поколения и отвечает отказом, а не ошибкой разбора.
//! Версия — согласованный номер, отдельного кодека на версию нет: проба моделирует процессный
//! контракт согласования, а не эволюцию формата.
//!
//! Назначения: `attach` — клиент сессии (хвост вывода с номера байта, живой вывод, ввод,
//! лидерство и размер), `control` — операция без клиента (состояние, ввод, ожидание вывода,
//! остановка), `probe` — только рукопожатие: без лидерства, ресайза и ввода, им проверяется
//! совместимость перед обновлением.
//!
//! Источник кадра и кодирования полей — `spikes/probe-session-survival/src/proto/mod.rs`, упрощено:
//! диапазон в `Hello` вместо наибольшей версии, без VT, ролей GUI, наблюдения и буфера обмена.
//!
//! Модуль чистый, он идёт в `make check`.

use std::fmt;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::observer::ProcessKey;
use crate::pty::Winsize;

/// Предел тела кадра.
pub const MAX_BODY: usize = 1 << 20;
/// Тег и длина.
pub const HEADER: usize = 5;
/// Наибольший кусок вывода в одном кадре `Output`.
pub const OUTPUT_CHUNK: usize = 64 << 10;

/// Диапазон версий протокола стороны, границы включены.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versions {
    pub min: u16,
    pub max: u16,
}

impl fmt::Display for Versions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.min, self.max)
    }
}

impl Versions {
    pub const fn new(min: u16, max: u16) -> Versions {
        Versions { min, max }
    }

    /// Диапазон не пуст и не включает 0.
    pub fn is_valid(self) -> bool {
        self.min >= 1 && self.min <= self.max
    }

    pub fn contains(self, version: u16) -> bool {
        (self.min..=self.max).contains(&version)
    }

    /// `a..b` или одно число.
    pub fn parse(text: &str) -> Result<Versions, String> {
        let number = |part: &str| {
            part.trim()
                .parse::<u16>()
                .map_err(|err| format!("диапазон версий «{text}»: {err}"))
        };
        let versions = match text.split_once("..") {
            Some((min, max)) => Versions::new(number(min)?, number(max)?),
            None => {
                let only = number(text)?;
                Versions::new(only, only)
            }
        };
        if !versions.is_valid() {
            return Err(format!("диапазон версий «{text}» пуст"));
        }
        Ok(versions)
    }

    /// Диапазон, заданный при сборке переменной окружения (`a..b` или одно число); без неё —
    /// `default`. Разбор константный: неверное значение роняет сборку, а не подменяется молча.
    /// Поколения пробы собираются с разными значениями (`run.sh bundle`, задача 4), поэтому их
    /// исполняемые файлы различаются по существу.
    pub const fn from_build(value: Option<&str>, default: Versions) -> Versions {
        let Some(text) = value else {
            return default;
        };
        let bytes = text.as_bytes();
        let (min, mut at) = const_number(bytes, 0);
        let max = if at == bytes.len() {
            min
        } else {
            assert!(
                at + 2 < bytes.len() && bytes[at] == b'.' && bytes[at + 1] == b'.',
                "диапазон версий при сборке: ожидается a..b"
            );
            let (max, end) = const_number(bytes, at + 2);
            at = end;
            max
        };
        assert!(
            at == bytes.len(),
            "диапазон версий при сборке: лишние символы"
        );
        assert!(
            min >= 1 && min <= max,
            "диапазон версий при сборке пуст или включает 0"
        );
        Versions::new(min, max)
    }
}

/// Десятичное число `u16` с позиции `at`; позиция после него.
const fn const_number(bytes: &[u8], mut at: usize) -> (u16, usize) {
    let begin = at;
    let mut value: u32 = 0;
    while at < bytes.len() && bytes[at].is_ascii_digit() {
        value = value * 10 + (bytes[at] - b'0') as u32;
        assert!(value <= u16::MAX as u32, "версия при сборке больше u16");
        at += 1;
    }
    assert!(at > begin, "диапазон версий при сборке: нет числа");
    (value as u16, at)
}

/// Диапазон протокола хоста этой сборки: `PROBE_UPDATE_HOST_PROTO` при сборке, иначе 1..1.
pub const HOST_VERSIONS: Versions =
    Versions::from_build(option_env!("PROBE_UPDATE_HOST_PROTO"), Versions::new(1, 1));

/// Диапазон протокола attach-клиента этой сборки: `PROBE_UPDATE_ATTACH_PROTO`, иначе 1..1.
pub const ATTACH_VERSIONS: Versions = Versions::from_build(
    option_env!("PROBE_UPDATE_ATTACH_PROTO"),
    Versions::new(1, 1),
);

/// Общая наибольшая версия хоста и клиента; пустое пересечение или пустой диапазон клиента —
/// причина отказа.
pub fn negotiate(host: Versions, client: Versions) -> Result<u16, String> {
    if !client.is_valid() {
        return Err(format!("диапазон версий клиента {client} пуст"));
    }
    let low = host.min.max(client.min);
    let high = host.max.min(client.max);
    if low > high {
        return Err(format!(
            "несовместимые версии протокола: клиент {client}, хост {host}"
        ));
    }
    Ok(high)
}

/// Назначение соединения.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Purpose {
    /// Клиент сессии: хвост вывода, живой вывод, ввод, лидерство и размер.
    Attach,
    /// Операция без клиента: состояние, ввод, ожидание вывода, остановка.
    Control,
    /// Только рукопожатие: проверка совместимости без изменения сессии.
    Probe,
}

impl Purpose {
    pub const ALL: [Purpose; 3] = [Purpose::Attach, Purpose::Control, Purpose::Probe];

    pub fn name(self) -> &'static str {
        match self {
            Purpose::Attach => "attach",
            Purpose::Control => "control",
            Purpose::Probe => "probe",
        }
    }

    pub fn from_name(name: &str) -> Option<Purpose> {
        Purpose::ALL
            .into_iter()
            .find(|purpose| purpose.name() == name)
    }

    fn code(self) -> u8 {
        match self {
            Purpose::Attach => 0,
            Purpose::Control => 1,
            Purpose::Probe => 2,
        }
    }

    fn from_code(code: u8) -> Option<Purpose> {
        Purpose::ALL
            .into_iter()
            .find(|purpose| purpose.code() == code)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub purpose: Purpose,
    pub versions: Versions,
    /// Сборка и поколение клиента для отчёта.
    pub client: String,
    /// Номер байта вывода, с которого `attach` хочет хвост; у других назначений не применяется.
    pub from: u64,
    /// Размер окна `attach`; нулевой размер не меняет размер сессии.
    pub size: Winsize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// Выбранная версия протокола.
    pub version: u16,
    /// Сборка хоста.
    pub host: String,
    /// Поколение сборки хоста.
    pub generation: u32,
    pub session: String,
    /// Номер назначения лидера после рукопожатия: растёт с каждым принятым `attach`.
    pub lead: u32,
    /// Номер первого байта, который получит `attach`: запрошенный или начало хранимого хвоста,
    /// если запрошенное уже вытеснено.
    pub start: u64,
    /// Номер байта, следующего за последним прочитанным из PTY к моменту рукопожатия.
    pub end: u64,
}

/// Состояние сессии по запросу управляющего соединения.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub session: String,
    pub host: ProcessKey,
    pub workload: ProcessKey,
    pub workload_pgid: i32,
    pub workload_sid: Option<i32>,
    /// Диапазон протокола хоста.
    pub versions: Versions,
    /// Поколение сборки хоста.
    pub generation: u32,
    /// Подключённые `attach`-клиенты.
    pub clients: u32,
    /// Номер назначения лидера; растёт с каждым принятым `attach`.
    pub lead: u32,
    /// Лидер подключён.
    pub leader: bool,
    pub size: Winsize,
    /// Начало хранимого хвоста.
    pub start: u64,
    /// Байтов прочитано из PTY всего.
    pub end: u64,
    /// Переводов строки во всём прочитанном выводе.
    pub lines: u64,
    /// Нагрузка вышла.
    pub exited: bool,
}

/// Ответ хоста на ввод: принят и записан в PTY или отвергнут с причиной.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ack", content = "detail", rename_all = "kebab-case")]
pub enum Ack {
    Accepted,
    Rejected(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    // --- клиент → хост ---
    Hello(Hello),
    /// Ввод с номером: байты пишутся в PTY как есть, хост отвечает `InputAck`. Квитанцию о том,
    /// что ввод дошёл, печатает нагрузка в своём нумерованном выводе.
    Input {
        seq: u64,
        bytes: Vec<u8>,
    },
    Detach,
    StatusRequest,
    Stop,
    /// Ждать, пока в выводе станет не меньше `lines` переводов строки, но не дольше срока; ответ —
    /// `Status`. Даёт стенду барьер вывода без клиента без `sleep`.
    AwaitLines {
        lines: u64,
        timeout_ms: u32,
    },
    // --- хост → клиент ---
    Welcome(Welcome),
    Refused(String),
    Output {
        offset: u64,
        bytes: Vec<u8>,
    },
    InputAck {
        seq: u64,
        ack: Ack,
    },
    Status(Status),
    /// Подтверждение `Detach` и `Stop`.
    Done,
}

// Теги рукопожатия заморожены вместе с раскладкой тел.
const TAG_HELLO: u8 = 1;
const TAG_WELCOME: u8 = 2;
const TAG_REFUSED: u8 = 3;
const TAG_INPUT: u8 = 16;
const TAG_DETACH: u8 = 17;
const TAG_STATUS_REQUEST: u8 = 18;
const TAG_STOP: u8 = 19;
const TAG_AWAIT_LINES: u8 = 20;
const TAG_OUTPUT: u8 = 32;
const TAG_INPUT_ACK: u8 = 33;
const TAG_STATUS: u8 = 34;
const TAG_DONE: u8 = 35;

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
    fn versions(&mut self, versions: Versions) {
        self.u16(versions.min);
        self.u16(versions.max);
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

/// Кадр сообщения.
pub fn encode(message: &Message) -> Vec<u8> {
    let mut body = Body(Vec::new());
    let tag = match message {
        Message::Hello(hello) => {
            body.u8(hello.purpose.code());
            body.versions(hello.versions);
            body.str(&hello.client);
            body.u64(hello.from);
            body.size(hello.size);
            TAG_HELLO
        }
        Message::Welcome(welcome) => {
            body.u16(welcome.version);
            body.str(&welcome.host);
            body.u32(welcome.generation);
            body.str(&welcome.session);
            body.u32(welcome.lead);
            body.u64(welcome.start);
            body.u64(welcome.end);
            TAG_WELCOME
        }
        Message::Refused(reason) => {
            body.str(reason);
            TAG_REFUSED
        }
        Message::Input { seq, bytes } => {
            body.u64(*seq);
            body.bytes(bytes);
            TAG_INPUT
        }
        Message::Detach => TAG_DETACH,
        Message::StatusRequest => TAG_STATUS_REQUEST,
        Message::Stop => TAG_STOP,
        Message::AwaitLines { lines, timeout_ms } => {
            body.u64(*lines);
            body.u32(*timeout_ms);
            TAG_AWAIT_LINES
        }
        Message::Output { offset, bytes } => {
            body.u64(*offset);
            body.bytes(bytes);
            TAG_OUTPUT
        }
        Message::InputAck { seq, ack } => {
            body.u64(*seq);
            match ack {
                Ack::Accepted => body.u8(0),
                Ack::Rejected(reason) => {
                    body.u8(1);
                    body.str(reason);
                }
            }
            TAG_INPUT_ACK
        }
        Message::Status(status) => {
            body.str(&status.session);
            body.key(status.host);
            body.key(status.workload);
            body.i32(status.workload_pgid);
            body.opt_i32(status.workload_sid);
            body.versions(status.versions);
            body.u32(status.generation);
            body.u32(status.clients);
            body.u32(status.lead);
            body.bool(status.leader);
            body.size(status.size);
            body.u64(status.start);
            body.u64(status.end);
            body.u64(status.lines);
            body.bool(status.exited);
            TAG_STATUS
        }
        Message::Done => TAG_DONE,
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
    fn versions(&mut self, what: &'static str) -> Result<Versions, ProtoError> {
        Ok(Versions {
            min: self.u16(what)?,
            max: self.u16(what)?,
        })
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
}

/// Сообщение по тегу и телу кадра.
pub fn decode_body(tag: u8, body: &[u8]) -> Result<Message, ProtoError> {
    let mut fields = Fields { rest: body };
    let message = match tag {
        TAG_HELLO => Message::Hello(Hello {
            purpose: Purpose::from_code(fields.u8("hello.purpose")?)
                .ok_or(ProtoError::Malformed("hello.purpose"))?,
            versions: fields.versions("hello.versions")?,
            client: fields.string("hello.client")?,
            from: fields.u64("hello.from")?,
            size: fields.size("hello.size")?,
        }),
        TAG_WELCOME => Message::Welcome(Welcome {
            version: fields.u16("welcome.version")?,
            host: fields.string("welcome.host")?,
            generation: fields.u32("welcome.generation")?,
            session: fields.string("welcome.session")?,
            lead: fields.u32("welcome.lead")?,
            start: fields.u64("welcome.start")?,
            end: fields.u64("welcome.end")?,
        }),
        TAG_REFUSED => Message::Refused(fields.string("refused")?),
        TAG_INPUT => Message::Input {
            seq: fields.u64("input.seq")?,
            bytes: fields.bytes("input.bytes")?,
        },
        TAG_DETACH => Message::Detach,
        TAG_STATUS_REQUEST => Message::StatusRequest,
        TAG_STOP => Message::Stop,
        TAG_AWAIT_LINES => Message::AwaitLines {
            lines: fields.u64("await_lines.lines")?,
            timeout_ms: fields.u32("await_lines.timeout_ms")?,
        },
        TAG_OUTPUT => Message::Output {
            offset: fields.u64("output.offset")?,
            bytes: fields.bytes("output.bytes")?,
        },
        TAG_INPUT_ACK => Message::InputAck {
            seq: fields.u64("input_ack.seq")?,
            ack: match fields.u8("input_ack.ack")? {
                0 => Ack::Accepted,
                1 => Ack::Rejected(fields.string("input_ack.reason")?),
                _ => return Err(ProtoError::Malformed("input_ack.ack")),
            },
        },
        TAG_STATUS => Message::Status(Status {
            session: fields.string("status.session")?,
            host: fields.key("status.host")?,
            workload: fields.key("status.workload")?,
            workload_pgid: fields.i32("status.pgid")?,
            workload_sid: fields.opt_i32("status.sid")?,
            versions: fields.versions("status.versions")?,
            generation: fields.u32("status.generation")?,
            clients: fields.u32("status.clients")?,
            lead: fields.u32("status.lead")?,
            leader: fields.bool("status.leader")?,
            size: fields.size("status.size")?,
            start: fields.u64("status.start")?,
            end: fields.u64("status.end")?,
            lines: fields.u64("status.lines")?,
            exited: fields.bool("status.exited")?,
        }),
        TAG_DONE => Message::Done,
        other => return Err(ProtoError::UnknownTag(other)),
    };
    fields.end()?;
    Ok(message)
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

    fn hello(purpose: Purpose, versions: Versions) -> Hello {
        Hello {
            purpose,
            versions,
            client: "probe-update-attach 1".to_owned(),
            from: 42,
            size: Winsize::cells(30, 100),
        }
    }

    fn status() -> Status {
        Status {
            session: "h1".to_owned(),
            host: ProcessKey {
                pid: 10,
                start_us: 1_000,
            },
            workload: ProcessKey {
                pid: 11,
                start_us: 2_000,
            },
            workload_pgid: 11,
            workload_sid: Some(11),
            versions: Versions::new(1, 2),
            generation: 2,
            clients: 1,
            lead: 3,
            leader: true,
            size: Winsize::cells(24, 80),
            start: 0,
            end: 4096,
            lines: 77,
            exited: false,
        }
    }

    fn all_messages() -> Vec<Message> {
        let mut gone = status();
        gone.workload_sid = None;
        gone.exited = true;
        vec![
            Message::Hello(hello(Purpose::Attach, Versions::new(1, 2))),
            Message::Hello(hello(Purpose::Control, Versions::new(1, 1))),
            Message::Hello(hello(Purpose::Probe, Versions::new(2, 2))),
            Message::Welcome(Welcome {
                version: 2,
                host: "probe-update-host 2".to_owned(),
                generation: 2,
                session: "h1".to_owned(),
                lead: 4,
                start: 10,
                end: 20,
            }),
            Message::Refused("несовместимо".to_owned()),
            Message::Input {
                seq: 7,
                bytes: b"7 emit 3\n".to_vec(),
            },
            Message::Detach,
            Message::StatusRequest,
            Message::Stop,
            Message::AwaitLines {
                lines: 51,
                timeout_ms: 10_000,
            },
            Message::Output {
                offset: 1 << 40,
                bytes: vec![0, 1, 2, 255],
            },
            Message::InputAck {
                seq: 7,
                ack: Ack::Accepted,
            },
            Message::InputAck {
                seq: 8,
                ack: Ack::Rejected("нагрузка вышла".to_owned()),
            },
            Message::Status(status()),
            Message::Status(gone),
            Message::Done,
        ]
    }

    #[test]
    fn every_message_round_trips() {
        for message in all_messages() {
            let frame = encode(&message);
            let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
            assert_eq!(frame.len(), HEADER + len);
            assert_eq!(decode_body(frame[0], &frame[HEADER..]), Ok(message.clone()));
            let mut reader = io::Cursor::new(frame);
            assert_eq!(read_message(&mut reader).unwrap(), Some(message));
            assert_eq!(read_message(&mut reader).unwrap(), None);
        }
    }

    #[test]
    fn handshake_layout_is_frozen() {
        // Раскладка Hello не меняется между поколениями: хост 1 разбирает Hello клиента 2..2.
        let frame = encode(&Message::Hello(Hello {
            purpose: Purpose::Probe,
            versions: Versions::new(2, 2),
            client: "c".to_owned(),
            from: 1,
            size: Winsize::cells(2, 3),
        }));
        assert_eq!(
            frame,
            [
                1, 0, 0, 0, 26, // тег и длина
                2,  // probe
                0, 2, 0, 2, // диапазон 2..2
                0, 0, 0, 1, b'c', // клиент
                0, 0, 0, 0, 0, 0, 0, 1, // from
                0, 2, 0, 3, 0, 0, 0, 0, // размер
            ]
        );
        let refused = encode(&Message::Refused("r".to_owned()));
        assert_eq!(refused, [3, 0, 0, 0, 5, 0, 0, 0, 1, b'r']);
        let welcome = encode(&Message::Welcome(Welcome {
            version: 1,
            host: String::new(),
            generation: 1,
            session: "s".to_owned(),
            lead: 0,
            start: 0,
            end: 0,
        }));
        assert_eq!(&welcome[..7], &[2, 0, 0, 0, 35, 0, 1]);
    }

    #[test]
    fn malformed_frames_are_rejected() {
        let frame = encode(&Message::Input {
            seq: 1,
            bytes: b"abc".to_vec(),
        });
        assert_eq!(
            decode_body(frame[0], &frame[HEADER..frame.len() - 1]),
            Err(ProtoError::Malformed("input.bytes"))
        );
        let mut longer = frame[HEADER..].to_vec();
        longer.push(0);
        assert_eq!(
            decode_body(frame[0], &longer),
            Err(ProtoError::Malformed("лишние байты в теле"))
        );
        assert_eq!(decode_body(99, &[]), Err(ProtoError::UnknownTag(99)));
        assert_eq!(
            decode_body(TAG_HELLO, &[9, 0, 1, 0, 1]),
            Err(ProtoError::Malformed("hello.purpose"))
        );
        assert_eq!(
            decode_body(TAG_INPUT_ACK, &[0, 0, 0, 0, 0, 0, 0, 1, 7]),
            Err(ProtoError::Malformed("input_ack.ack"))
        );

        let mut huge = vec![TAG_OUTPUT];
        huge.extend_from_slice(&((MAX_BODY + 1) as u32).to_be_bytes());
        let err = read_message(&mut io::Cursor::new(huge)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let err = read_message(&mut io::Cursor::new(vec![TAG_DONE, 0, 0])).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        let err = read_message(&mut io::Cursor::new(vec![TAG_OUTPUT, 0, 0, 0, 9, 1])).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn negotiation_picks_the_highest_common_version() {
        let host1 = Versions::new(1, 1);
        let host2 = Versions::new(1, 2);
        assert_eq!(negotiate(host1, Versions::new(1, 1)), Ok(1));
        assert_eq!(negotiate(host1, Versions::new(1, 2)), Ok(1));
        assert_eq!(negotiate(host2, Versions::new(1, 1)), Ok(1));
        assert_eq!(negotiate(host2, Versions::new(1, 2)), Ok(2));
        assert_eq!(negotiate(host2, Versions::new(2, 2)), Ok(2));
        assert_eq!(negotiate(Versions::new(2, 2), Versions::new(1, 2)), Ok(2));
        assert_eq!(negotiate(Versions::new(1, 5), Versions::new(3, 9)), Ok(5));
    }

    #[test]
    fn empty_intersection_is_refused_with_both_ranges() {
        let err = negotiate(Versions::new(1, 1), Versions::new(2, 2)).unwrap_err();
        assert!(err.contains("2..2") && err.contains("1..1"), "{err}");
        assert!(negotiate(Versions::new(2, 2), Versions::new(1, 1)).is_err());
        assert!(negotiate(Versions::new(3, 4), Versions::new(1, 2)).is_err());
        // Пустой или нулевой диапазон клиента — отказ, а не версия 0.
        assert!(negotiate(Versions::new(1, 2), Versions::new(2, 1)).is_err());
        assert!(negotiate(Versions::new(0, 2), Versions::new(0, 0)).is_err());
    }

    #[test]
    #[should_panic(expected = "диапазон версий при сборке")]
    fn malformed_build_range_does_not_fall_back() {
        Versions::from_build(Some("2..1"), Versions::new(1, 1));
    }

    #[test]
    #[should_panic(expected = "диапазон версий при сборке")]
    fn garbage_build_range_does_not_fall_back() {
        Versions::from_build(Some("1-2"), Versions::new(1, 1));
    }

    #[test]
    fn versions_parse_and_build_defaults() {
        assert_eq!(Versions::parse("1..2"), Ok(Versions::new(1, 2)));
        assert_eq!(Versions::parse("2"), Ok(Versions::new(2, 2)));
        assert!(Versions::parse("2..1").is_err());
        assert!(Versions::parse("0..1").is_err());
        assert!(Versions::parse("a..1").is_err());
        assert_eq!(Versions::new(1, 2).to_string(), "1..2");
        let default = Versions::new(1, 1);
        assert_eq!(Versions::from_build(None, default), default);
        assert_eq!(
            Versions::from_build(Some("2..2"), default),
            Versions::new(2, 2)
        );
        assert_eq!(
            Versions::from_build(Some("1..12"), default),
            Versions::new(1, 12)
        );
        assert_eq!(
            Versions::from_build(Some("3"), default),
            Versions::new(3, 3)
        );
        assert!(HOST_VERSIONS.is_valid() && ATTACH_VERSIONS.is_valid());
        for purpose in Purpose::ALL {
            assert_eq!(Purpose::from_name(purpose.name()), Some(purpose));
            assert_eq!(Purpose::from_code(purpose.code()), Some(purpose));
        }
    }
}
