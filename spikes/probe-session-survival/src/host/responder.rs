//! Автомат ответов Rust-хоста (план P0.4, «Rust-хост»): разбор потока PTY с состоянием между
//! чтениями. Хост — единственный источник ответов на объявленный набор запросов ([`Served`]):
//! обслуженный запрос клиенту не уходит, а ответ на него даёт VT хоста. Модуль чистый, VT стоит за
//! трейтом [`Answer`].
//!
//! Поток режется на шаги по концу каждой последовательности, которая может дать ответ терминала.
//! Каждый шаг целиком подаётся в VT, и всё, что VT записал в PTY за шаг, принадлежит последней
//! последовательности шага. Ответ на обслуженный шаг уходит в PTY нагрузки; ответ VT на
//! переданный клиенту запрос отбрасывается: на него отвечает терминал клиента (ADR-0005, ровно
//! одна сторона). Начатая последовательность, которую ещё нельзя классифицировать, клиенту не
//! отдаётся до решения (удержание), но в VT уже подана: её continuation хоста знает.
//!
//! Объявленный набор: DA1 (`CSI c`, `CSI 0 c`), DA2 (`CSI > c`, `CSI > 0 c`), CPR (`CSI 6 n`),
//! DECRQM (`CSI ? Pd $ p`, `CSI Pd $ p`), XTVERSION (`CSI > q`, `CSI > 0 q`), запросы цвета
//! OSC 10, 11 и 4, в которых каждый цвет — `?`, и OSC 52 чтения. Набор режимов отчётов
//! (`bridge::REPORT_MODES`) тоже обслуживает хост: их set и reset вырезает `ReportFilter`, а
//! ответ VT на включение (отчёт размера 2048) уходит в PTY. `CSI 5 n` в набор не входит: это
//! барьер стенда, на него отвечает терминал клиента.

use serde::{Deserialize, Serialize};

use crate::bridge::is_report_mode;

/// Обслуживаемая хостом последовательность.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Served {
    Da1,
    Da2,
    CursorPosition,
    ModeQuery,
    XtVersion,
    Color,
    ClipboardRead,
    /// Set, reset, XTSAVE или XTRESTORE режима отчётов; клиенту идёт остаток CSI.
    ReportMode,
}

impl Served {
    /// Шаг вырезается из потока клиенту целиком.
    pub fn removed(self) -> bool {
        self != Served::ReportMode
    }
}

/// Шаг разбора: байты для VT, байты для клиента, решённые на этом шаге, и что обслужено.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub raw: Vec<u8>,
    pub client: Vec<u8>,
    pub served: Option<Served>,
}

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const BEL: u8 = 0x07;
const ENQ: u8 = 0x05;
/// Предел удерживаемой CSI: длиннее — передаётся как есть и не обслуживается.
pub const MAX_CSI: usize = 256;
/// Предел удерживаемой OSC запроса цвета и буфера обмена.
pub const MAX_OSC: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Parse {
    Ground,
    Escape,
    Csi,
    /// OSC, номер которой ещё читается или которая может оказаться запросом: удерживается.
    OscHeld,
    /// ESC внутри удерживаемой OSC, которая не обслуживается.
    OscHeldEscape,
    /// ESC закрыл обслуженную OSC: VT исполнил её на ESC. `\` терминатора ST клиенту не идёт;
    /// ESC удержан, другой байт начинает с ним новую последовательность.
    ServedEscape,
    /// Строка (OSC, DCS, SOS, PM, APC), которая идёт клиенту насквозь до ST, BEL или CAN/SUB.
    Text {
        dcs: bool,
    },
    TextEscape {
        dcs: bool,
    },
}

/// Разборщик потока PTY с состоянием между чтениями.
#[derive(Clone, Debug)]
pub struct Scanner {
    parse: Parse,
    /// Удерживаемая начатая последовательность.
    held: Vec<u8>,
    raw: Vec<u8>,
    client: Vec<u8>,
    steps: Vec<Step>,
}

impl Default for Scanner {
    fn default() -> Scanner {
        Scanner::new()
    }
}

impl Scanner {
    pub fn new() -> Scanner {
        Scanner {
            parse: Parse::Ground,
            held: Vec::new(),
            raw: Vec::new(),
            client: Vec::new(),
            steps: Vec::new(),
        }
    }

    /// Удерживаемые от клиента байты начатой последовательности.
    pub fn held(&self) -> &[u8] {
        &self.held
    }

    /// Шаги куска; последний шаг кончается на конце куска, если граница не совпала.
    pub fn push(&mut self, input: &[u8]) -> Vec<Step> {
        for &byte in input {
            self.raw.push(byte);
            self.byte(byte);
        }
        if !self.raw.is_empty() || !self.client.is_empty() {
            self.cut(None);
        }
        std::mem::take(&mut self.steps)
    }

    fn cut(&mut self, served: Option<Served>) {
        self.steps.push(Step {
            raw: std::mem::take(&mut self.raw),
            client: std::mem::take(&mut self.client),
            served,
        });
    }

    fn flush(&mut self) {
        self.client.append(&mut self.held);
    }

    fn byte(&mut self, byte: u8) {
        match self.parse {
            Parse::Ground => match byte {
                ESC => {
                    self.held.push(byte);
                    self.parse = Parse::Escape;
                }
                ENQ => {
                    self.client.push(byte);
                    self.cut(None);
                }
                _ => self.client.push(byte),
            },
            Parse::Escape => match byte {
                b'[' => {
                    self.held.push(byte);
                    self.parse = Parse::Csi;
                }
                b']' => {
                    self.held.push(byte);
                    self.parse = Parse::OscHeld;
                }
                b'P' | b'X' | b'^' | b'_' => {
                    self.flush();
                    self.client.push(byte);
                    self.parse = Parse::Text { dcs: byte == b'P' };
                }
                ESC => {
                    self.flush();
                    self.held.push(byte);
                }
                CAN | SUB => {
                    self.flush();
                    self.client.push(byte);
                    self.parse = Parse::Ground;
                }
                // DECID отвечает как DA1, но в набор не входит: передаётся клиенту.
                b'Z' => {
                    self.flush();
                    self.client.push(byte);
                    self.parse = Parse::Ground;
                    self.cut(None);
                }
                _ => {
                    self.flush();
                    self.client.push(byte);
                    self.parse = Parse::Ground;
                }
            },
            Parse::Csi => match byte {
                CAN | SUB => {
                    self.flush();
                    self.client.push(byte);
                    self.parse = Parse::Ground;
                }
                ESC => {
                    self.flush();
                    self.held.push(byte);
                    self.parse = Parse::Escape;
                }
                0x40..=0x7e => {
                    self.held.push(byte);
                    let held = std::mem::take(&mut self.held);
                    self.parse = Parse::Ground;
                    self.csi(&held);
                }
                _ => {
                    self.held.push(byte);
                    if self.held.len() > MAX_CSI {
                        self.flush();
                        self.parse = Parse::Ground;
                    }
                }
            },
            Parse::OscHeld => match byte {
                BEL => {
                    self.held.push(byte);
                    self.parse = Parse::Ground;
                    self.osc_end();
                }
                CAN | SUB => {
                    self.flush();
                    self.client.push(byte);
                    self.parse = Parse::Ground;
                }
                ESC => {
                    self.held.push(byte);
                    // VT исполняет OSC уже на ESC терминатора ST: ответ на обслуженный запрос
                    // принадлежит шагу, который кончается этим ESC.
                    let mut whole = self.held.clone();
                    whole.push(b'\\');
                    match osc_served(&whole) {
                        Some(served) => {
                            self.held = vec![ESC];
                            self.parse = Parse::ServedEscape;
                            self.cut(Some(served));
                        }
                        None => self.parse = Parse::OscHeldEscape,
                    }
                }
                _ => {
                    self.held.push(byte);
                    if !self.osc_may_be_query() || self.held.len() > MAX_OSC {
                        self.flush();
                        self.parse = Parse::Text { dcs: false };
                    }
                }
            },
            Parse::OscHeldEscape => {
                if byte == b'\\' {
                    self.held.push(byte);
                    self.parse = Parse::Ground;
                    self.osc_end();
                } else {
                    // ESC закрыл OSC без исполнения и начал новую последовательность.
                    self.held.pop();
                    self.flush();
                    self.held.push(ESC);
                    self.parse = Parse::Escape;
                    self.byte(byte);
                }
            }
            Parse::ServedEscape => {
                if byte == b'\\' {
                    self.held.clear();
                    self.parse = Parse::Ground;
                } else {
                    self.parse = Parse::Escape;
                    self.byte(byte);
                }
            }
            Parse::Text { dcs } => match byte {
                ESC => {
                    self.client.push(byte);
                    self.parse = Parse::TextEscape { dcs };
                }
                BEL if !dcs => {
                    self.client.push(byte);
                    self.parse = Parse::Ground;
                }
                CAN | SUB => {
                    self.client.push(byte);
                    self.parse = Parse::Ground;
                }
                _ => self.client.push(byte),
            },
            Parse::TextEscape { dcs } => {
                if byte == b'\\' {
                    self.client.push(byte);
                    self.parse = Parse::Ground;
                    // Конец DCS: DECRQSS и XTGETTCAP отвечают, но в набор не входят.
                    if dcs {
                        self.cut(None);
                    }
                } else {
                    // ESC уже передан клиенту: он закрыл строку и начинает новую последовательность.
                    self.parse = Parse::Escape;
                    self.client.pop();
                    self.held.push(ESC);
                    self.byte(byte);
                }
            }
        }
    }

    /// Начало удерживаемой OSC ещё может быть запросом цвета или чтением буфера обмена.
    fn osc_may_be_query(&self) -> bool {
        let body = &self.held[2..];
        let number_end = body.iter().position(|b| *b == b';');
        let Some(end) = number_end else {
            return body.iter().all(u8::is_ascii_digit) && body.len() <= 2;
        };
        match &body[..end] {
            b"4" | b"10" | b"11" => true,
            b"52" => {
                // `52;<выбор>;?`: после второго `;` первый байт решает.
                let rest = &body[end + 1..];
                match rest.iter().position(|b| *b == b';') {
                    None => rest.len() <= 16,
                    Some(at) => {
                        let data = &rest[at + 1..];
                        data.is_empty() || data == b"?"
                    }
                }
            }
            _ => false,
        }
    }

    fn osc_end(&mut self) {
        let held = std::mem::take(&mut self.held);
        let served = osc_served(&held);
        match served {
            Some(served) => {
                self.cut(Some(served));
            }
            None => {
                self.client.extend_from_slice(&held);
            }
        }
    }

    /// Полная CSI: обслуженная вырезается, остальные передаются; шаг кончается на CSI, которая
    /// может дать ответ.
    fn csi(&mut self, held: &[u8]) {
        let body = &held[2..held.len() - 1];
        let final_byte = held[held.len() - 1];
        let controls: Vec<u8> = body.iter().copied().filter(|b| *b < 0x20).collect();
        let text: Vec<u8> = body.iter().copied().filter(|b| *b >= 0x20).collect();
        match csi_kind(&text, final_byte) {
            CsiKind::Served(served) => {
                self.client.extend_from_slice(&controls);
                if !served.removed() {
                    self.client.extend_from_slice(held);
                }
                self.cut(Some(served));
            }
            CsiKind::MayReply => {
                self.client.extend_from_slice(held);
                self.cut(None);
            }
            CsiKind::Plain => self.client.extend_from_slice(held),
        }
    }
}

enum CsiKind {
    Served(Served),
    MayReply,
    Plain,
}

fn csi_kind(text: &[u8], final_byte: u8) -> CsiKind {
    match (final_byte, text) {
        (b'c', b"" | b"0") => CsiKind::Served(Served::Da1),
        (b'c', b">" | b">0") => CsiKind::Served(Served::Da2),
        (b'c', _) => CsiKind::MayReply,
        (b'n', b"6") => CsiKind::Served(Served::CursorPosition),
        (b'n', _) => CsiKind::MayReply,
        (b'p', _) if text.last() == Some(&b'$') => {
            let params = &text[..text.len() - 1];
            let digits = params.strip_prefix(b"?").unwrap_or(params);
            if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) {
                CsiKind::Served(Served::ModeQuery)
            } else {
                CsiKind::MayReply
            }
        }
        (b'q', b">" | b">0") => CsiKind::Served(Served::XtVersion),
        (b'h' | b'l' | b's' | b'r', _) if text.first() == Some(&b'?') => {
            let params = &text[1..];
            let report = params.iter().all(|b| b.is_ascii_digit() || *b == b';')
                && params
                    .split(|b| *b == b';')
                    .filter_map(|value| std::str::from_utf8(value).ok()?.parse::<u16>().ok())
                    .any(is_report_mode);
            if report {
                CsiKind::Served(Served::ReportMode)
            } else {
                CsiKind::Plain
            }
        }
        (b't' | b'u' | b'x', _) => CsiKind::MayReply,
        _ => CsiKind::Plain,
    }
}

/// Целая OSC с терминатором: обслуживается ли она хостом.
fn osc_served(held: &[u8]) -> Option<Served> {
    let end = if held.ends_with(b"\x1b\\") {
        held.len() - 2
    } else {
        held.len() - 1
    };
    let body = std::str::from_utf8(&held[2..end]).ok()?;
    let (number, rest) = body.split_once(';')?;
    match number {
        "10" | "11" => {
            let all = rest.split(';').all(|spec| spec == "?");
            all.then_some(Served::Color)
        }
        "4" => {
            let parts: Vec<&str> = rest.split(';').collect();
            let pairs = parts.len().is_multiple_of(2) && !parts.is_empty();
            let all = parts
                .chunks(2)
                .all(|pair| pair[0].parse::<u8>().is_ok() && pair.get(1) == Some(&"?"));
            (pairs && all).then_some(Served::Color)
        }
        "52" => {
            let (_, data) = rest.split_once(';')?;
            (data == "?").then_some(Served::ClipboardRead)
        }
        _ => None,
    }
}

/// VT хоста: байты шага и то, что он записал в PTY за этот шаг.
pub trait Answer {
    fn answer(&mut self, raw: &[u8]) -> Vec<u8>;
}

/// Итог куска.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fed {
    /// Ответы хоста в PTY нагрузки.
    pub replies: Vec<u8>,
    /// Байты клиенту до фильтра режимов отчётов.
    pub client: Vec<u8>,
    /// Ответы VT на переданные клиенту запросы: отброшены.
    pub dropped: Vec<u8>,
    pub served: Vec<Served>,
}

/// Разбор и ответы: кусок PTY через [`Scanner`] и VT.
#[derive(Clone, Debug, Default)]
pub struct Responder {
    scanner: Scanner,
}

impl Responder {
    pub fn new() -> Responder {
        Responder::default()
    }

    pub fn held(&self) -> &[u8] {
        self.scanner.held()
    }

    pub fn feed(&mut self, vt: &mut impl Answer, bytes: &[u8]) -> Fed {
        let mut fed = Fed::default();
        for step in self.scanner.push(bytes) {
            let out = vt.answer(&step.raw);
            match step.served {
                Some(served) => {
                    fed.replies.extend_from_slice(&out);
                    fed.served.push(served);
                }
                None => fed.dropped.extend_from_slice(&out),
            }
            fed.client.extend_from_slice(&step.client);
        }
        fed
    }
}

/// Continuation для клиента: continuation VT без удержанного хвоста. Удержанное (разборщик и
/// фильтр режимов отчётов) клиент получит живым потоком или не получит вовсе, если это
/// обслуженный запрос. `Err` — хвост не совпал с continuation VT.
pub fn client_continuation(vt: &[u8], held: &[u8]) -> Result<Vec<u8>, String> {
    if held.is_empty() {
        return Ok(vt.to_vec());
    }
    match vt.strip_suffix(held) {
        Some(rest) => Ok(rest.to_vec()),
        None => Err(format!(
            "удержанный хвост {} не совпал с continuation VT {}",
            hex(held),
            hex(vt)
        )),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests;
