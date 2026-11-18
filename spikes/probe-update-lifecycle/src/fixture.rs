//! Нагрузка пробы `probe-update-fixture` (план P0.5, раскладка cargo): нумерованный вывод и
//! квитанции ввода в том же нумерованном потоке. Чтение файла TCC добавляет задача 16.
//!
//! Нагрузка работает в PTY хоста лидером своей сессии. Первым делом она переводит терминал в
//! raw-режим без эха и только потом печатает строку `ready`: ввод, пришедший после неё, не
//! отражается обратно и не ломает нумерацию. Каждая строка вывода — `<номер> <текст>\n` с
//! номером по порядку от 1, поэтому пропуск и повтор видны по номерам ([`parse_line`]).
//!
//! Ввод — строки `<seq> <команда>`. Ответ на каждую — квитанция `ack <seq> <команда>` в
//! нумерованном выводе: она доказывает, что ввод дошёл до нагрузки, и переживает смену клиента,
//! потому что её получает любой следующий `attach` в хвосте. Команды:
//! - `emit <k>` — `k` строк `emit <seq> <i>/<k>`, затем квитанция;
//! - `ping` — квитанция;
//! - `exit` — квитанция и выход с кодом 0.
//!
//! Нагрузка выходит по `exit`, по концу PTY и по `SIGHUP` или `SIGTERM` (действие по умолчанию).
//! Метка `--tag` в `argv` нужна внешнему сторожу.

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::process::ExitCode;

use crate::pty;

pub const FIXTURE_NAME: &str = "probe-update-fixture";
/// Наибольшее `k` у `emit`.
pub const MAX_EMIT: u64 = 100_000;
const READ_BUF: usize = 4096;
/// Наибольшая строка ввода без перевода строки.
const MAX_INPUT_LINE: usize = 64 << 10;

/// Строка нумерованного вывода.
pub fn format_line(number: u64, text: &str) -> String {
    format!("{number:06} {text}\n")
}

/// Номер и текст строки вывода без перевода строки; `None` — строка без номера.
pub fn parse_line(line: &str) -> Option<(u64, &str)> {
    let (number, text) = line.split_once(' ')?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((number.parse().ok()?, text))
}

/// Байты ввода с номером: строка `<seq> <команда>\n`.
pub fn input_line(seq: u64, command: &str) -> Vec<u8> {
    format!("{seq} {command}\n").into_bytes()
}

/// Текст квитанции ввода `seq` на `command`.
pub fn receipt(seq: u64, command: &str) -> String {
    format!("ack {seq} {command}")
}

/// Строка `ready` нагрузки.
pub fn ready_text(pid: u32, generation: u32) -> String {
    format!("ready pid={pid} generation={generation}")
}

/// Ответ нагрузки на строку ввода: тексты строк вывода по порядку и признак выхода.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub lines: Vec<String>,
    pub exit: bool,
}

/// Ответ на строку ввода без перевода строки.
pub fn respond(line: &str) -> Response {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let Some((seq, command)) = line
        .split_once(' ')
        .and_then(|(seq, command)| Some((seq.parse::<u64>().ok()?, command.trim())))
    else {
        return Response {
            lines: vec![format!("error - строка ввода без номера: {line:?}")],
            exit: false,
        };
    };
    let words: Vec<&str> = command.split_whitespace().collect();
    match words[..] {
        ["emit", count] => match count.parse::<u64>() {
            Ok(count) if count <= MAX_EMIT => {
                let mut lines: Vec<String> = (1..=count)
                    .map(|index| format!("emit {seq} {index}/{count}"))
                    .collect();
                lines.push(receipt(seq, command));
                Response { lines, exit: false }
            }
            _ => Response {
                lines: vec![format!("error {seq} emit: число от 0 до {MAX_EMIT}")],
                exit: false,
            },
        },
        ["ping"] => Response {
            lines: vec![receipt(seq, command)],
            exit: false,
        },
        ["exit"] => Response {
            lines: vec![receipt(seq, command)],
            exit: true,
        },
        _ => Response {
            lines: vec![format!("error {seq} неизвестная команда: {command:?}")],
            exit: false,
        },
    }
}

/// Нумерованный вывод в stdout.
struct Output {
    next: u64,
}

impl Output {
    /// Все строки одной записью.
    fn emit(&mut self, texts: &[String]) -> io::Result<()> {
        let mut bytes = String::new();
        for text in texts {
            self.next += 1;
            bytes.push_str(&format_line(self.next, text));
        }
        let mut stdout = io::stdout().lock();
        stdout.write_all(bytes.as_bytes())?;
        stdout.flush()
    }
}

fn parse_args(args: &[String]) -> Result<(), String> {
    match args {
        [flag, tag] if flag == "--tag" && !tag.is_empty() => Ok(()),
        _ => Err("использование: probe-update-fixture --tag <метка>".to_owned()),
    }
}

/// Вход `probe-update-fixture`.
pub fn main(args: &[String]) -> ExitCode {
    if let Err(err) = parse_args(args) {
        eprintln!("{FIXTURE_NAME}: {err}");
        return ExitCode::from(2);
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{FIXTURE_NAME}: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let stdin = io::stdin();
    let terminal =
        pty::get_termios(stdin.as_fd()).map_err(|err| format!("stdin не терминал: {err}"))?;
    pty::set_termios(stdin.as_fd(), &pty::raw_mode(&terminal))
        .map_err(|err| format!("raw-режим: {err}"))?;
    let mut output = Output { next: 0 };
    output
        .emit(&[ready_text(std::process::id(), crate::GENERATION)])
        .map_err(|err| format!("вывод: {err}"))?;
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; READ_BUF];
    let mut input = stdin.lock();
    loop {
        let read = match input.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            // Конец PTY на macOS приходит как EIO.
            Err(_) => return Ok(()),
        };
        pending.extend_from_slice(&buf[..read]);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            let response = respond(&text);
            output
                .emit(&response.lines)
                .map_err(|err| format!("вывод: {err}"))?;
            if response.exit {
                return Ok(());
            }
        }
        if pending.len() > MAX_INPUT_LINE {
            pending.clear();
            output
                .emit(&[format!(
                    "error - строка ввода длиннее {MAX_INPUT_LINE} байт"
                )])
                .map_err(|err| format!("вывод: {err}"))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_numbered_and_parsed_back() {
        assert_eq!(format_line(7, "ack 1 ping"), "000007 ack 1 ping\n");
        assert_eq!(parse_line("000007 ack 1 ping"), Some((7, "ack 1 ping")));
        assert_eq!(parse_line("1234567 x"), Some((1_234_567, "x")));
        assert_eq!(parse_line("ack 1 ping"), None);
        assert_eq!(parse_line("12a x"), None);
        assert_eq!(parse_line(" x"), None);
        assert_eq!(parse_line("000007"), None);
    }

    #[test]
    fn emit_prints_lines_then_the_receipt() {
        assert_eq!(input_line(2, "emit 2"), b"2 emit 2\n");
        assert_eq!(
            respond("2 emit 2"),
            Response {
                lines: vec![
                    "emit 2 1/2".to_owned(),
                    "emit 2 2/2".to_owned(),
                    "ack 2 emit 2".to_owned()
                ],
                exit: false
            }
        );
        assert_eq!(respond("3 emit 0").lines, ["ack 3 emit 0"]);
        assert_eq!(respond("4 ping\r").lines, ["ack 4 ping"]);
        assert_eq!(
            respond("5 exit"),
            Response {
                lines: vec!["ack 5 exit".to_owned()],
                exit: true
            }
        );
    }

    #[test]
    fn bad_input_gets_an_error_line_not_a_receipt() {
        for line in [
            "ping",
            "x ping",
            "",
            "6 emit many",
            "7 emit 100001",
            "8 dance",
        ] {
            let response = respond(line);
            assert_eq!(response.lines.len(), 1, "{line:?}");
            assert!(response.lines[0].starts_with("error "), "{line:?}");
            assert!(!response.exit);
        }
        assert!(parse_args(&["--tag".to_owned(), "n".to_owned()]).is_ok());
        assert!(parse_args(&[]).is_err());
    }
}
