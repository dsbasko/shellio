//! Запросы терминала режима `query` и разбор ответов во вводе фикстуры. Фикстура знает, какие
//! запросы отправила, и считает ответы по своему входу: разбор держит состояние между чтениями,
//! поэтому ответ, разрезанный на любой границе байта, считается один раз. Похожие байты на
//! обратном потоке никто не фильтрует (ADR-0005).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::channel::{Event, Receipt};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
/// Предел одной последовательности: длиннее — `overflow`, разбор возвращается в начало.
pub const MAX_SEQUENCE: usize = 64 << 10;

/// Запрос терминала из профиля D1, D4 и D5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Query {
    /// `CSI c`.
    Da1,
    /// `CSI > c`.
    Da2,
    /// DSR позиции курсора, `CSI 6 n`.
    CursorPosition,
    /// DECRQM частного режима, `CSI ? Pd $ p`.
    ModeDec { mode: u16 },
    /// DECRQM режима ANSI, `CSI Pd $ p`.
    ModeAnsi { mode: u16 },
    /// OSC 10.
    ForegroundColor,
    /// OSC 11.
    BackgroundColor,
    /// OSC 4 одного индекса.
    Palette { index: u8 },
    /// XTVERSION, `CSI > 0 q`.
    XtVersion,
    /// OSC 52 чтения буфера обмена.
    ClipboardRead,
}

impl Query {
    pub fn bytes(self) -> Vec<u8> {
        match self {
            Query::Da1 => b"\x1b[c".to_vec(),
            Query::Da2 => b"\x1b[>c".to_vec(),
            Query::CursorPosition => b"\x1b[6n".to_vec(),
            Query::ModeDec { mode } => format!("\x1b[?{mode}$p").into_bytes(),
            Query::ModeAnsi { mode } => format!("\x1b[{mode}$p").into_bytes(),
            Query::ForegroundColor => b"\x1b]10;?\x1b\\".to_vec(),
            Query::BackgroundColor => b"\x1b]11;?\x1b\\".to_vec(),
            Query::Palette { index } => format!("\x1b]4;{index};?\x1b\\").into_bytes(),
            Query::XtVersion => b"\x1b[>0q".to_vec(),
            Query::ClipboardRead => b"\x1b]52;c;?\x1b\\".to_vec(),
        }
    }

    /// Вид ответа, которого ждёт запрос.
    pub fn response(self) -> ResponseKind {
        match self {
            Query::Da1 => ResponseKind::Da1,
            Query::Da2 => ResponseKind::Da2,
            Query::CursorPosition => ResponseKind::CursorPosition,
            Query::ModeDec { .. } | Query::ModeAnsi { .. } => ResponseKind::ModeReport,
            Query::ForegroundColor => ResponseKind::ForegroundColor,
            Query::BackgroundColor => ResponseKind::BackgroundColor,
            Query::Palette { .. } => ResponseKind::Palette,
            Query::XtVersion => ResponseKind::XtVersion,
            Query::ClipboardRead => ResponseKind::Clipboard,
        }
    }
}

/// Вид последовательности во вводе фикстуры.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResponseKind {
    Da1,
    Da2,
    CursorPosition,
    /// DECRPM: `CSI ? Pd ; Ps $ y` или `CSI Pd ; Ps $ y`.
    ModeReport,
    ForegroundColor,
    BackgroundColor,
    Palette,
    XtVersion,
    Clipboard,
    /// Отчёты фокуса режима 1004.
    FocusIn,
    FocusOut,
    /// Отчёт размера режима 2048, `CSI 48 ; … t`.
    SizeReport,
    OtherCsi,
    OtherOsc,
    OtherDcs,
    /// Последовательность длиннее [`MAX_SEQUENCE`].
    Overflow,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    Dcs,
    DcsEscape,
    /// `ESC O` и один байт: клавиши SS3, не ответы.
    Ss3,
}

/// Разбор ответов терминала с состоянием между чтениями. Байты вне последовательностей
/// пропускаются. `ESC` внутри строки OSC или DCS без `\` следом обрывает строку без ответа и
/// начинает новую последовательность; `CAN` и `SUB` обрывают любую.
#[derive(Debug, Default)]
pub struct ResponseParser {
    state: State,
    seq: Vec<u8>,
}

impl ResponseParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<(ResponseKind, Vec<u8>)> {
        let mut out = Vec::new();
        for &byte in bytes {
            self.step(byte, &mut out);
        }
        out
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.seq.clear();
    }

    fn restart(&mut self) {
        self.seq.clear();
        self.seq.push(ESC);
        self.state = State::Escape;
    }

    fn emit(&mut self, kind: ResponseKind, out: &mut Vec<(ResponseKind, Vec<u8>)>) {
        out.push((kind, std::mem::take(&mut self.seq)));
        self.state = State::Ground;
    }

    fn push(&mut self, byte: u8, out: &mut Vec<(ResponseKind, Vec<u8>)>) {
        self.seq.push(byte);
        if self.seq.len() > MAX_SEQUENCE {
            self.emit(ResponseKind::Overflow, out);
        }
    }

    fn step(&mut self, byte: u8, out: &mut Vec<(ResponseKind, Vec<u8>)>) {
        match self.state {
            State::Ground => {
                if byte == ESC {
                    self.restart();
                }
            }
            State::Escape => match byte {
                b'[' => {
                    self.seq.push(byte);
                    self.state = State::Csi;
                }
                b']' => {
                    self.seq.push(byte);
                    self.state = State::Osc;
                }
                b'P' => {
                    self.seq.push(byte);
                    self.state = State::Dcs;
                }
                b'O' => self.state = State::Ss3,
                ESC => self.restart(),
                _ => self.reset(),
            },
            State::Ss3 => {
                if byte == ESC {
                    self.restart();
                } else {
                    self.reset();
                }
            }
            State::Csi => match byte {
                0x20..=0x3f => self.push(byte, out),
                0x40..=0x7e => {
                    self.seq.push(byte);
                    let kind = classify_csi(&self.seq);
                    self.emit(kind, out);
                }
                ESC => self.restart(),
                CAN | SUB => self.reset(),
                // Прочие управляющие байты внутри CSI исполняются и последовательность не рвут.
                _ => {}
            },
            State::Osc => match byte {
                BEL => {
                    self.seq.push(byte);
                    let kind = classify_osc(&self.seq[2..self.seq.len() - 1]);
                    self.emit(kind, out);
                }
                ESC => {
                    self.seq.push(byte);
                    self.state = State::OscEscape;
                }
                CAN | SUB => self.reset(),
                _ => self.push(byte, out),
            },
            State::Dcs => match byte {
                ESC => {
                    self.seq.push(byte);
                    self.state = State::DcsEscape;
                }
                CAN | SUB => self.reset(),
                _ => self.push(byte, out),
            },
            State::OscEscape | State::DcsEscape => {
                if byte == b'\\' {
                    self.seq.push(byte);
                    let body = &self.seq[2..self.seq.len() - 2];
                    let kind = if self.state == State::OscEscape {
                        classify_osc(body)
                    } else {
                        classify_dcs(body)
                    };
                    self.emit(kind, out);
                } else {
                    self.restart();
                    self.step(byte, out);
                }
            }
        }
    }
}

/// `seq` — `ESC [`, параметры, промежуточные байты и последний байт.
fn classify_csi(seq: &[u8]) -> ResponseKind {
    let (Some(&last), Some(body)) = (seq.last(), seq.get(2..seq.len() - 1)) else {
        return ResponseKind::OtherCsi;
    };
    let prefix = body.first().copied().filter(|byte| b"<=>?".contains(byte));
    let rest = if prefix.is_some() { &body[1..] } else { body };
    let intermediates: Vec<u8> = rest
        .iter()
        .copied()
        .filter(|byte| (0x20..=0x2f).contains(byte))
        .collect();
    let params: Vec<u8> = rest
        .iter()
        .copied()
        .filter(|byte| (0x30..=0x3f).contains(byte))
        .collect();
    match (last, prefix, intermediates.as_slice()) {
        // Сам запрос DA параметров не несёт, ответ — всегда с параметрами.
        (b'c', Some(b'?'), []) if !params.is_empty() => ResponseKind::Da1,
        (b'c', Some(b'>'), []) if !params.is_empty() => ResponseKind::Da2,
        (b'R', None, []) if !params.is_empty() => ResponseKind::CursorPosition,
        (b'y', None | Some(b'?'), [b'$']) => ResponseKind::ModeReport,
        (b'I', None, []) if params.is_empty() => ResponseKind::FocusIn,
        (b'O', None, []) if params.is_empty() => ResponseKind::FocusOut,
        (b't', None, []) if params.starts_with(b"48;") => ResponseKind::SizeReport,
        _ => ResponseKind::OtherCsi,
    }
}

/// `body` — содержимое OSC без `ESC ]` и без терминатора. `?` последним параметром — запрос, а
/// не ответ.
fn classify_osc(body: &[u8]) -> ResponseKind {
    if body.ends_with(b";?") {
        return ResponseKind::OtherOsc;
    }
    let number = body.split(|byte| *byte == b';').next().unwrap_or_default();
    match number {
        b"10" => ResponseKind::ForegroundColor,
        b"11" => ResponseKind::BackgroundColor,
        b"4" => ResponseKind::Palette,
        b"52" => ResponseKind::Clipboard,
        _ => ResponseKind::OtherOsc,
    }
}

fn classify_dcs(body: &[u8]) -> ResponseKind {
    if body.starts_with(b">|") {
        ResponseKind::XtVersion
    } else {
        ResponseKind::OtherDcs
    }
}

/// Счётчик ответов по квитанциям `response` одной фикстуры.
pub fn tally(receipts: &[Receipt]) -> BTreeMap<ResponseKind, u64> {
    let mut counts = BTreeMap::new();
    for receipt in receipts {
        if let Event::Response { response, .. } = receipt.event {
            *counts.entry(response).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ответы Ghostty и xterm на запросы профиля и отчёты режимов.
    const SAMPLES: [(&[u8], ResponseKind); 13] = [
        (b"\x1b[?62;22;52c", ResponseKind::Da1),
        (b"\x1b[>1;10;0c", ResponseKind::Da2),
        (b"\x1b[12;40R", ResponseKind::CursorPosition),
        (b"\x1b[?2004;2$y", ResponseKind::ModeReport),
        (b"\x1b[4;2$y", ResponseKind::ModeReport),
        (
            b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\",
            ResponseKind::ForegroundColor,
        ),
        (
            b"\x1b]11;rgb:0000/0000/0000\x07",
            ResponseKind::BackgroundColor,
        ),
        (b"\x1b]4;1;rgb:cccc/0000/0000\x1b\\", ResponseKind::Palette),
        (b"\x1bP>|ghostty 1.4.0\x1b\\", ResponseKind::XtVersion),
        (b"\x1b]52;c;aGVsbG8=\x1b\\", ResponseKind::Clipboard),
        (b"\x1b[I", ResponseKind::FocusIn),
        (b"\x1b[O", ResponseKind::FocusOut),
        (b"\x1b[48;24;80;480;800t", ResponseKind::SizeReport),
    ];

    #[test]
    fn every_response_is_counted_once_at_every_split() {
        for (sample, kind) in SAMPLES {
            for cut in 0..=sample.len() {
                let mut parser = ResponseParser::default();
                let mut got = parser.feed(&sample[..cut]);
                got.extend(parser.feed(&sample[cut..]));
                assert_eq!(
                    got,
                    [(kind, sample.to_vec())],
                    "{:?} разрезан на {cut}",
                    String::from_utf8_lossy(sample)
                );
            }
        }
    }

    #[test]
    fn byte_by_byte_stream_with_text_and_keys_between() {
        let mut stream = b"text ".to_vec();
        for (sample, _) in SAMPLES {
            stream.extend_from_slice(sample);
            stream.extend_from_slice(b"\x1bOA\r\x03\x1b[1;5A");
        }
        let mut parser = ResponseParser::default();
        let mut got = Vec::new();
        for byte in &stream {
            got.extend(parser.feed(std::slice::from_ref(byte)));
        }
        let kinds: Vec<ResponseKind> = got.iter().map(|(kind, _)| *kind).collect();
        let mut expected = Vec::new();
        for (_, kind) in SAMPLES {
            expected.push(kind);
            expected.push(ResponseKind::OtherCsi);
        }
        assert_eq!(kinds, expected);
    }

    #[test]
    fn aborted_and_overlong_sequences() {
        let mut parser = ResponseParser::default();
        assert!(
            parser.feed(b"\x1b[?62\x18;22c").is_empty(),
            "CAN обрывает CSI"
        );
        assert_eq!(
            parser.feed(b"\x1b]10;rgb\x1b[c"),
            [(ResponseKind::OtherCsi, b"\x1b[c".to_vec())],
            "ESC внутри OSC без \\ начинает новую последовательность"
        );
        assert_eq!(
            parser.feed(b"\x1b\x1b[>1;10;0c"),
            [(ResponseKind::Da2, b"\x1b[>1;10;0c".to_vec())],
            "ESC перед ESC начинает последовательность заново"
        );
        let mut long = b"\x1b]52;c;".to_vec();
        long.extend(std::iter::repeat_n(b'A', MAX_SEQUENCE));
        let got = parser.feed(&long);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, ResponseKind::Overflow);
        assert_eq!(
            parser.feed(b"\x1b[12;1R"),
            [(ResponseKind::CursorPosition, b"\x1b[12;1R".to_vec())]
        );
    }

    #[test]
    fn queries_expect_their_response_kind() {
        for query in [
            Query::Da1,
            Query::Da2,
            Query::CursorPosition,
            Query::ModeDec { mode: 2004 },
            Query::ModeAnsi { mode: 4 },
            Query::ForegroundColor,
            Query::BackgroundColor,
            Query::Palette { index: 1 },
            Query::XtVersion,
            Query::ClipboardRead,
        ] {
            let bytes = query.bytes();
            assert_eq!(bytes[0], ESC, "{query:?}");
            // Запрос сам не похож на ответ своего вида.
            let parsed = ResponseParser::default().feed(&bytes);
            assert!(
                parsed.iter().all(|(kind, _)| *kind != query.response()),
                "{query:?}: {parsed:?}"
            );
        }
        assert_eq!(Query::ModeDec { mode: 2026 }.bytes(), b"\x1b[?2026$p");
    }

    #[test]
    fn tally_counts_response_receipts() {
        let receipts: Vec<Receipt> = [
            ResponseKind::Da1,
            ResponseKind::Clipboard,
            ResponseKind::Da1,
        ]
        .into_iter()
        .enumerate()
        .map(|(at, response)| Receipt {
            seq: at as u64 + 1,
            t_ns: 0,
            event: Event::Response {
                index: at as u64 + 1,
                response,
                hex: String::new(),
            },
        })
        .chain(std::iter::once(Receipt {
            seq: 4,
            t_ns: 0,
            event: Event::Signal {
                signal: "SIGWINCH".to_owned(),
            },
        }))
        .collect();
        let counts = tally(&receipts);
        assert_eq!(counts.get(&ResponseKind::Da1), Some(&2));
        assert_eq!(counts.get(&ResponseKind::Clipboard), Some(&1));
        assert_eq!(counts.len(), 2);
    }
}
