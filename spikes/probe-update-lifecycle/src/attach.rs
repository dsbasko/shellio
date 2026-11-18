//! Клиент хоста пробы и attach-клиент `probe-update-attach` (план P0.5, раскладка cargo): hello с
//! диапазоном версий, хвост с номера байта, живой вывод, ввод с квитанцией нагрузки.
//!
//! [`Transcript`] собирает нумерованный вывод нагрузки и отмечает разрыв байтов, пропуск и повтор
//! номеров. Возобновление с [`Transcript::resume_offset`] начинается с начала последней неполной
//! строки, а ожидаемый номер первой строки — следующий за последним полученным.
//!
//! CLI печатает один JSON-объект [`CliReport`] в stdout; ненулевой код — только ошибка клиента,
//! отказ хоста — обычный ответ с `outcome: refused`.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::fixture;
use crate::proto::{
    self, ATTACH_VERSIONS, Ack, Hello, Message, Purpose, Status, Versions, Welcome,
};
use crate::pty::Winsize;

pub const ATTACH_NAME: &str = "probe-update-attach";
/// Срок одного шага клиента по умолчанию.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// Строка нумерованного вывода и номер её первого байта.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub number: u64,
    pub text: String,
    pub offset: u64,
}

/// Нумерованный вывод, собранный из кадров `Output`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcript {
    /// Номер первого байта, с которого начат сбор.
    pub start: u64,
    /// Номер следующего ожидаемого байта.
    pub next: u64,
    /// Ожидаемый номер первой строки; `None` — любой.
    pub expect_first: Option<u64>,
    pub lines: Vec<Line>,
    /// Байты последней неполной строки.
    #[serde(default)]
    pub partial: Vec<u8>,
    /// Разрывы байтов, пропуски, повторы и строки без номера.
    pub problems: Vec<String>,
}

impl Transcript {
    pub fn new(start: u64, expect_first: Option<u64>) -> Transcript {
        Transcript {
            start,
            next: start,
            expect_first,
            lines: Vec::new(),
            partial: Vec::new(),
            problems: Vec::new(),
        }
    }

    pub fn push(&mut self, offset: u64, bytes: &[u8]) {
        if offset != self.next {
            self.problems.push(format!(
                "разрыв байтов: ожидался {}, пришёл {offset}",
                self.next
            ));
            self.partial.clear();
            self.next = offset;
        }
        for byte in bytes {
            if *byte == b'\n' {
                let line_offset = self.next - self.partial.len() as u64;
                let text = String::from_utf8_lossy(&self.partial).into_owned();
                self.partial.clear();
                self.finish(line_offset, text);
            } else {
                self.partial.push(*byte);
            }
            self.next += 1;
        }
    }

    fn finish(&mut self, offset: u64, raw: String) {
        let Some((number, text)) = fixture::parse_line(&raw) else {
            self.problems
                .push(format!("строка без номера в байте {offset}: {raw:?}"));
            return;
        };
        let expected = match self.lines.last() {
            Some(last) => Some(last.number + 1),
            None => self.expect_first,
        };
        if let Some(expected) = expected
            && number != expected
        {
            let kind = if number > expected {
                "пропуск"
            } else {
                "повтор"
            };
            self.problems.push(format!(
                "{kind}: ожидалась строка {expected}, пришла {number}"
            ));
        }
        self.lines.push(Line {
            number,
            text: text.to_owned(),
            offset,
        });
    }

    /// Номер байта начала последней неполной строки: с него продолжает следующий `attach`.
    pub fn resume_offset(&self) -> u64 {
        self.next - self.partial.len() as u64
    }

    pub fn last_number(&self) -> Option<u64> {
        self.lines.last().map(|line| line.number)
    }

    /// Номер, ожидаемый у первой строки следующего `attach`.
    pub fn next_number(&self) -> Option<u64> {
        self.last_number()
            .map(|last| last + 1)
            .or(self.expect_first)
    }

    pub fn find(&self, text: &str) -> Option<&Line> {
        self.lines.iter().find(|line| line.text == text)
    }

    /// Квитанция нагрузки на ввод `seq` с командой `command`.
    pub fn receipt(&self, seq: u64, command: &str) -> Option<&Line> {
        self.find(&fixture::receipt(seq, command))
    }
}

/// Соединение после `Welcome`.
pub struct Client {
    stream: UnixStream,
    pub welcome: Welcome,
}

/// Ответ хоста на `Hello`.
pub enum Connected {
    Welcome(Client),
    Refused(String),
}

/// Подключение и рукопожатие. Версию, выбранную хостом, клиент сверяет со своим диапазоном.
pub fn connect(socket: &Path, hello: &Hello, timeout: Duration) -> Result<Connected, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|err| format!("connect {}: {err}", socket.display()))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|err| format!("сроки сокета: {err}"))?;
    proto::write_message(&mut stream, &Message::Hello(hello.clone()))
        .map_err(|err| format!("Hello: {err}"))?;
    match proto::read_message(&mut stream) {
        Ok(Some(Message::Welcome(welcome))) => {
            if !hello.versions.contains(welcome.version) {
                return Err(format!(
                    "хост выбрал протокол {}, клиент понимает {}",
                    welcome.version, hello.versions
                ));
            }
            Ok(Connected::Welcome(Client { stream, welcome }))
        }
        Ok(Some(Message::Refused(reason))) => Ok(Connected::Refused(reason)),
        Ok(Some(other)) => Err(format!("вместо Welcome пришло {other:?}")),
        Ok(None) => Err("хост закрыл соединение до ответа на Hello".to_owned()),
        Err(err) => Err(format!("ответ на Hello: {err}")),
    }
}

/// `Hello` этой сборки клиента.
pub fn hello(purpose: Purpose, versions: Versions, from: u64, size: Winsize) -> Hello {
    Hello {
        purpose,
        versions,
        client: format!("{ATTACH_NAME} {}", crate::GENERATION),
        from,
        size,
    }
}

impl Client {
    pub fn send(&mut self, message: &Message) -> Result<(), String> {
        proto::write_message(&mut self.stream, message).map_err(|err| format!("запись: {err}"))
    }

    /// Следующее сообщение не позже `deadline`.
    pub fn recv(&mut self, deadline: Instant) -> Result<Message, String> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("срок ответа хоста вышел".to_owned());
        }
        self.stream
            .set_read_timeout(Some(left))
            .map_err(|err| format!("срок чтения: {err}"))?;
        match proto::read_message(&mut self.stream) {
            Ok(Some(message)) => Ok(message),
            Ok(None) => Err("хост закрыл соединение".to_owned()),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                Err("срок ответа хоста вышел".to_owned())
            }
            Err(err) => Err(format!("чтение: {err}")),
        }
    }

    /// Запрос управляющего соединения с одним ответом.
    fn request(&mut self, message: &Message, timeout: Duration) -> Result<Message, String> {
        self.send(message)?;
        self.recv(Instant::now() + timeout)
    }

    pub fn status(&mut self) -> Result<Status, String> {
        match self.request(&Message::StatusRequest, STEP_TIMEOUT)? {
            Message::Status(status) => Ok(status),
            other => Err(format!("вместо Status пришло {other:?}")),
        }
    }

    /// Ввод через управляющее соединение: ответ хоста о записи в PTY.
    pub fn input(&mut self, seq: u64, command: &str) -> Result<Ack, String> {
        let message = Message::Input {
            seq,
            bytes: fixture::input_line(seq, command),
        };
        match self.request(&message, STEP_TIMEOUT)? {
            Message::InputAck { seq: got, ack } if got == seq => Ok(ack),
            other => Err(format!("вместо InputAck {seq} пришло {other:?}")),
        }
    }

    /// Ждёт на хосте `lines` переводов строки в выводе, не дольше `timeout`.
    pub fn await_lines(&mut self, lines: u64, timeout: Duration) -> Result<Status, String> {
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let message = Message::AwaitLines { lines, timeout_ms };
        match self.request(&message, timeout + STEP_TIMEOUT)? {
            Message::Status(status) => Ok(status),
            other => Err(format!("вместо Status пришло {other:?}")),
        }
    }

    pub fn stop(&mut self) -> Result<(), String> {
        match self.request(&Message::Stop, STEP_TIMEOUT)? {
            Message::Done => Ok(()),
            other => Err(format!("вместо Done пришло {other:?}")),
        }
    }
}

/// Клиент `attach` с собираемым выводом.
pub struct Attached {
    client: Client,
    pub transcript: Transcript,
    /// Ответы хоста на ввод по порядку.
    pub acks: Vec<(u64, Ack)>,
}

impl Attached {
    pub fn new(client: Client, expect_first: Option<u64>) -> Attached {
        let start = client.welcome.start;
        Attached {
            client,
            transcript: Transcript::new(start, expect_first),
            acks: Vec::new(),
        }
    }

    pub fn welcome(&self) -> &Welcome {
        &self.client.welcome
    }

    fn take(&mut self, message: Message) -> Result<bool, String> {
        match message {
            Message::Output { offset, bytes } => {
                self.transcript.push(offset, &bytes);
                Ok(false)
            }
            Message::InputAck { seq, ack } => {
                self.acks.push((seq, ack));
                Ok(false)
            }
            Message::Done => Ok(true),
            other => Err(format!("attach: неожиданное сообщение {other:?}")),
        }
    }

    /// Читает вывод, пока `done` не истинно, не позже `deadline`.
    pub fn pump_until(
        &mut self,
        deadline: Instant,
        done: impl Fn(&Transcript) -> bool,
    ) -> Result<(), String> {
        while !done(&self.transcript) {
            let message = self.client.recv(deadline)?;
            if self.take(message)? {
                return Err("хост завершил attach раньше ожидаемого вывода".to_owned());
            }
        }
        Ok(())
    }

    pub fn input(&mut self, seq: u64, command: &str) -> Result<(), String> {
        self.client.send(&Message::Input {
            seq,
            bytes: fixture::input_line(seq, command),
        })
    }

    /// Ввод и ожидание квитанции нагрузки и ответа хоста.
    pub fn exchange(&mut self, seq: u64, command: &str, deadline: Instant) -> Result<(), String> {
        self.input(seq, command)?;
        self.pump_until(deadline, |transcript| {
            transcript.receipt(seq, command).is_some()
        })?;
        while !self.acks.iter().any(|(got, _)| *got == seq) {
            let message = self.client.recv(deadline)?;
            if self.take(message)? {
                return Err(format!("хост завершил attach до ответа на ввод {seq}"));
            }
        }
        Ok(())
    }

    /// `Detach` и чтение до `Done`; вывод до него входит в стенограмму.
    pub fn detach(&mut self, deadline: Instant) -> Result<(), String> {
        self.client.send(&Message::Detach)?;
        loop {
            let message = self.client.recv(deadline)?;
            if self.take(message)? {
                return Ok(());
            }
        }
    }
}

/// Итог CLI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliReport {
    pub purpose: Purpose,
    pub versions: Versions,
    /// `welcome` или `refused`.
    pub outcome: String,
    #[serde(default)]
    pub welcome: Option<Welcome>,
    #[serde(default)]
    pub refused: Option<String>,
    #[serde(default)]
    pub transcript: Option<Transcript>,
    #[serde(default)]
    pub acks: Vec<(u64, Ack)>,
}

/// Аргументы CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliArgs {
    pub purpose: Purpose,
    pub socket: PathBuf,
    pub versions: Versions,
    pub from: u64,
    pub expect_first: Option<u64>,
    pub size: Winsize,
    /// Первый номер ввода; каждая следующая команда получает следующий.
    pub seq: u64,
    pub inputs: Vec<String>,
    pub tag: String,
}

pub const USAGE: &str = "\
использование: probe-update-attach <probe|attach> --socket <путь> --tag <метка>
    [--versions a..b] [--from <байт>] [--expect <номер строки>] [--size RxC]
    [--seq <номер>] [--input <команда>]…
probe — только рукопожатие; attach — хвост с --from, затем каждая --input с ожиданием квитанции
нагрузки, затем Detach. Итог — один JSON-объект в stdout.";

pub fn parse_cli(args: &[String]) -> Result<CliArgs, String> {
    let mut rest = args.iter();
    let purpose = match rest.next().map(String::as_str) {
        Some("probe") => Purpose::Probe,
        Some("attach") => Purpose::Attach,
        _ => return Err("ожидается probe или attach".to_owned()),
    };
    let mut parsed = CliArgs {
        purpose,
        socket: PathBuf::new(),
        versions: ATTACH_VERSIONS,
        from: 0,
        expect_first: None,
        size: Winsize::cells(0, 0),
        seq: 1,
        inputs: Vec::new(),
        tag: String::new(),
    };
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag}: нет значения"))?;
        let number = |value: &str| {
            value
                .parse::<u64>()
                .map_err(|err| format!("{flag} {value}: {err}"))
        };
        match flag.as_str() {
            "--socket" => parsed.socket = PathBuf::from(value),
            "--versions" => parsed.versions = Versions::parse(value)?,
            "--from" => parsed.from = number(value)?,
            "--expect" => parsed.expect_first = Some(number(value)?),
            "--size" => parsed.size = crate::host::parse_size(value)?,
            "--seq" => parsed.seq = number(value)?,
            "--input" => parsed.inputs.push(value.clone()),
            "--tag" => parsed.tag = value.clone(),
            other => return Err(format!("неизвестный флаг {other}")),
        }
    }
    if parsed.socket.as_os_str().is_empty() {
        return Err("нет --socket".to_owned());
    }
    if parsed.tag.is_empty() {
        return Err("нет --tag".to_owned());
    }
    if purpose == Purpose::Probe && !parsed.inputs.is_empty() {
        return Err("probe не принимает ввод".to_owned());
    }
    Ok(parsed)
}

/// Вход `probe-update-attach`.
pub fn main(args: &[String]) -> ExitCode {
    let parsed = match parse_cli(args) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("{ATTACH_NAME}: {err}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run_cli(&parsed) {
        Ok(report) => {
            let line = serde_json::to_string(&report).unwrap_or_else(|_| "{}".to_owned());
            let mut stdout = io::stdout().lock();
            let _ = writeln!(stdout, "{line}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{ATTACH_NAME}: {err}");
            ExitCode::from(1)
        }
    }
}

fn run_cli(args: &CliArgs) -> Result<CliReport, String> {
    let hello = hello(args.purpose, args.versions, args.from, args.size);
    let mut report = CliReport {
        purpose: args.purpose,
        versions: args.versions,
        outcome: String::new(),
        welcome: None,
        refused: None,
        transcript: None,
        acks: Vec::new(),
    };
    let client = match connect(&args.socket, &hello, STEP_TIMEOUT)? {
        Connected::Refused(reason) => {
            report.outcome = "refused".to_owned();
            report.refused = Some(reason);
            return Ok(report);
        }
        Connected::Welcome(client) => client,
    };
    report.outcome = "welcome".to_owned();
    report.welcome = Some(client.welcome.clone());
    if args.purpose == Purpose::Probe {
        return Ok(report);
    }
    let mut attached = Attached::new(client, args.expect_first);
    let deadline = Instant::now() + STEP_TIMEOUT * (args.inputs.len() as u32 + 1);
    for (index, command) in args.inputs.iter().enumerate() {
        attached.exchange(args.seq + index as u64, command, deadline)?;
    }
    attached.detach(deadline)?;
    report.transcript = Some(attached.transcript);
    report.acks = attached.acks;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(lines: &[(u64, &str)]) -> Vec<u8> {
        lines
            .iter()
            .flat_map(|(number, text)| fixture::format_line(*number, text).into_bytes())
            .collect()
    }

    #[test]
    fn continuous_output_split_anywhere_has_no_problems() {
        let bytes = numbered(&[(1, "ready"), (2, "emit 1 1/1"), (3, "ack 1 emit 1")]);
        for cut in 0..bytes.len() {
            let mut transcript = Transcript::new(0, Some(1));
            transcript.push(0, &bytes[..cut]);
            transcript.push(cut as u64, &bytes[cut..]);
            assert!(
                transcript.problems.is_empty(),
                "{cut}: {:?}",
                transcript.problems
            );
            assert_eq!(transcript.last_number(), Some(3));
            assert_eq!(transcript.next, bytes.len() as u64);
            assert_eq!(transcript.resume_offset(), bytes.len() as u64);
            assert_eq!(transcript.receipt(1, "emit 1").unwrap().number, 3);
        }
    }

    #[test]
    fn skipped_and_repeated_numbers_are_reported() {
        let mut skipped = Transcript::new(0, None);
        skipped.push(0, &numbered(&[(1, "a"), (3, "c")]));
        assert_eq!(skipped.problems.len(), 1);
        assert!(
            skipped.problems[0].starts_with("пропуск"),
            "{:?}",
            skipped.problems
        );

        let mut repeated = Transcript::new(0, None);
        repeated.push(0, &numbered(&[(1, "a"), (2, "b"), (2, "b")]));
        assert_eq!(repeated.problems.len(), 1);
        assert!(repeated.problems[0].starts_with("повтор"));

        let mut first = Transcript::new(100, Some(6));
        first.push(100, &numbered(&[(7, "x")]));
        assert!(first.problems[0].contains("ожидалась строка 6"));

        let mut unnumbered = Transcript::new(0, None);
        unnumbered.push(0, b"x\n");
        assert!(unnumbered.problems[0].starts_with("строка без номера"));
    }

    #[test]
    fn byte_gap_is_reported_and_resume_starts_at_the_partial_line() {
        let bytes = numbered(&[(1, "a"), (2, "b")]);
        let mut transcript = Transcript::new(0, Some(1));
        transcript.push(0, &bytes[..9]);
        assert_eq!(transcript.last_number(), Some(1));
        assert_eq!(transcript.resume_offset(), 9, "начало неполной строки 2");
        assert_eq!(transcript.next_number(), Some(2));
        transcript.push(12, &bytes[12..]);
        assert!(transcript.problems[0].starts_with("разрыв байтов"));

        let mut resumed = Transcript::new(9, Some(2));
        resumed.push(9, &bytes[9..]);
        assert!(resumed.problems.is_empty(), "{:?}", resumed.problems);
        assert_eq!(resumed.lines[0].offset, 9);
    }

    #[test]
    fn cli_arguments_parse() {
        let args: Vec<String> = [
            "attach",
            "--socket",
            "/s/h1.sock",
            "--tag",
            "n",
            "--from",
            "40",
            "--expect",
            "6",
            "--seq",
            "3",
            "--input",
            "ping",
            "--size",
            "30x100",
        ]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
        let parsed = parse_cli(&args).unwrap();
        assert_eq!(parsed.purpose, Purpose::Attach);
        assert_eq!(
            (parsed.from, parsed.expect_first, parsed.seq),
            (40, Some(6), 3)
        );
        assert_eq!(parsed.inputs, ["ping"]);
        assert_eq!(parsed.versions, ATTACH_VERSIONS);
        assert_eq!(parsed.size, Winsize::cells(30, 100));
        let probe: Vec<String> = [
            "probe",
            "--socket",
            "/s",
            "--tag",
            "n",
            "--versions",
            "2..2",
        ]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
        assert_eq!(parse_cli(&probe).unwrap().versions, Versions::new(2, 2));
        let mut bad = probe.clone();
        bad.extend(["--input".to_owned(), "ping".to_owned()]);
        assert!(parse_cli(&bad).is_err());
        assert!(parse_cli(&probe[..3]).is_err(), "нет --tag");
    }
}
