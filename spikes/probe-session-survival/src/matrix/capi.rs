//! Ранняя проверка C API `libghostty-vt` (план P0.4, задача 8): сценарий стенда `capi` без хоста.
//!
//! Модель хоста — исходный терминал `libghostty-vt` с continuation и колбэками ответов. Он читает
//! поток фикстуры из её PTY так же, как хост читал бы PTY нагрузки (`oracle::vt::Route`: чтение
//! мастера и ответы обратно), эталон получает те же байты напрямую. В точке восстановления из
//! исходного терминала строятся приёмники двумя способами: formatter VT со всеми дополнениями,
//! затем continuation (порядок «снимок → continuation → живой поток»), и полный снимок `GHOSTSNP`
//! с continuation внутри. Приёмник сравнивается с эталоном сразу и после суффиксов, живой поток
//! после точки восстановления получают эталон, исходный терминал и приёмники одинаково.
//!
//! Клетки — клетки стенда `capi-<клетка>`: это доказательство осуществимости, а не `pass` клеток
//! кандидата. Часть клетки — способ восстановления и вариант сюжета. Отказ C API — проверка с
//! `ok: false`; расхождение исходного терминала с эталоном — ошибка стенда.

use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::args::Target;
use crate::fixture::channel::{self, Command, Event, Link, Listener, Ready, WriteStats, hex};
use crate::fixture::query::{Query, ResponseKind, ResponseParser};
use crate::fixture::{self, Mode, flood_index, flood_line};
use crate::launch;
use crate::matrix::evidence::{Evidence, Spec, cell};
use crate::oracle::model::{self, Comparison, Outcome, compare};
use crate::oracle::suffix::{self, Sink, Step};
use crate::oracle::vt::{Capture, Config, Journaled, Receiver, ReceiverProfile, Reference, Route};
use crate::pty::{self, PtyChild, Spawn};
use crate::results::{self, Cell, DefectOrigin, Environment, Report};
use crate::vt::{self, ClipboardRequest};

const COLS: u16 = 80;
const ROWS: u16 = 24;
/// Срок одного ожидания квитанции, подтверждения или байтов у исходного терминала.
const WAIT: Duration = Duration::from_secs(15);
const STOP_GRACE: Duration = Duration::from_secs(5);
/// Объявленный бюджет continuation модели хоста.
pub const CONTINUATION_BUDGET: usize = 1 << 20;
/// Ожидание ответа GUI на чтение буфера обмена в потоке разбора (модель схемы ответа).
pub const CLIPBOARD_TIMEOUT: Duration = Duration::from_millis(1000);
/// Объём потока, который фикстура пишет, пока поток разбора ждёт GUI.
const BLOCK_FLOOD_LINES: u64 = 2000;
const BLOCK_FLOOD_WIDTH: u16 = 78;
/// Предел очереди чтения PTY модели хоста в байтах.
const READ_QUEUE_BYTES: usize = 4 << 20;
const READ_CHUNK: usize = 64 << 10;
/// Данные модели GUI для ответа на чтение буфера обмена.
const GUI_CLIPBOARD: &[u8] = b"gui-clipboard";
const LATE_CLIPBOARD: &[u8] = b"late-old-generation";

/// Запросы D1: ответы, которые хост даёт сам.
pub(crate) const D1_QUERIES: [(&str, Query); 9] = [
    ("da1", Query::Da1),
    ("da2", Query::Da2),
    ("cpr", Query::CursorPosition),
    ("decrqm-dec", Query::ModeDec { mode: 2004 }),
    ("decrqm-ansi", Query::ModeAnsi { mode: 4 }),
    ("osc10", Query::ForegroundColor),
    ("osc11", Query::BackgroundColor),
    ("osc4", Query::Palette { index: 1 }),
    ("xtversion", Query::XtVersion),
];

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    if let Some(candidate) = target.backend {
        return Err(format!(
            "capi — сценарий стенда, а не кандидата {candidate}"
        ));
    }
    let mut stand = Stand::new(&target.out, environment)?;
    let outcome = cells(&mut stand);
    let cleanup = stand.teardown();
    let mut report = outcome?;
    report.diagnostics.notes.extend(cleanup);
    Ok(report)
}

fn cells(stand: &mut Stand) -> Result<Report, String> {
    let mut report = Report::default();
    for story in stories() {
        report.cells.extend(restore_story(stand, &story)?);
    }
    report.cells.extend(b7(stand)?);
    report.cells.extend(b8(stand)?);
    report.cells.extend(d1(stand)?);
    report.cells.extend(d2(stand)?);
    report.cells.extend(d3(stand)?);
    report.cells.extend(d4(stand)?);
    report.cells.extend(budget(stand)?);
    let table: Vec<Value> = stand.verdicts.iter().map(|v| json!(v)).collect();
    let name = "capi-table.json";
    results::write_json(&stand.out, name, &table)?;
    report.diagnostics.notes.extend(
        stand
            .verdicts
            .iter()
            .map(|v| format!("{} {}: {:?} — {}", v.cell, v.part, v.verdict, v.detail)),
    );
    Ok(report)
}

// --- стенд ---

/// Итог части для таблицы возможностей C API.
#[derive(Clone, Debug, Serialize)]
struct Verdict {
    cell: String,
    part: String,
    verdict: Kind,
    detail: String,
}

/// Что показала часть сюжета восстановления.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Kind {
    /// Состояние и суффиксы совпали с эталоном.
    Restored,
    /// Состояние совпало, расхождение видно только суффиксом.
    SuffixOnly,
    /// Расхождение видно в состоянии сразу после восстановления.
    Lost,
    /// C API отказал в полезной нагрузке восстановления.
    Refused,
    /// Сравнение неполное.
    Incomplete,
    /// Сюжет запросов и эффектов: итог проверками.
    Checked,
}

struct Stand {
    nonce: String,
    out: PathBuf,
    root: PathBuf,
    listener: Option<Listener>,
    fixture: PathBuf,
    env: Vec<(String, String)>,
    /// Конфигурация эталона и приёмников клиента.
    config: Config,
    /// Конфигурация исходного терминала: та же и бюджет continuation.
    source: Config,
    feeds: usize,
    verdicts: Vec<Verdict>,
}

impl Stand {
    fn new(out: &Path, environment: &Environment) -> Result<Stand, String> {
        let nonce = environment
            .process
            .nonce
            .clone()
            .ok_or("предусловие: нет nonce запуска")?;
        let program = std::env::current_exe().map_err(|err| format!("путь раннера: {err}"))?;
        let fixture = program
            .parent()
            .map(|dir| dir.join(fixture::FIXTURE_NAME))
            .filter(|path| path.is_file())
            .ok_or_else(|| {
                format!(
                    "предусловие: нет {} рядом с раннером",
                    fixture::FIXTURE_NAME
                )
            })?;
        let root = PathBuf::from(format!("/tmp/pss-{nonce}"));
        fs::create_dir(&root).map_err(|err| format!("{}: {err}", root.display()))?;
        let listener = Listener::bind(&root.join("c.sock"))?;
        let entry = &environment.process.env;
        let env = launch::INHERITED
            .iter()
            .chain([launch::LANG].iter())
            .filter_map(|name| {
                entry
                    .get(*name)
                    .map(|value| ((*name).to_owned(), value.clone()))
            })
            .collect();
        let config = Config::new(COLS, ROWS);
        let source = Config {
            continuation_max: CONTINUATION_BUDGET,
            ..config.clone()
        };
        Ok(Stand {
            nonce,
            out: out.to_path_buf(),
            root,
            listener: Some(listener),
            fixture,
            env,
            config,
            source,
            feeds: 0,
            verdicts: Vec::new(),
        })
    }

    fn listener(&self) -> Result<&Listener, String> {
        self.listener
            .as_ref()
            .ok_or_else(|| "сокет квитанций закрыт".to_owned())
    }

    /// Фикстура `emitter` в PTY стенда и модель хоста на её мастере.
    fn feed(&mut self, label: &str, profile: ReceiverProfile) -> Result<Feed, String> {
        self.feeds += 1;
        let fixture_nonce = format!("{}-capi{}-{label}", self.nonce, self.feeds);
        let (child, link, _) = self.spawn(&fixture_nonce)?;
        let source = Receiver::new("host", &self.source, profile)
            .map_err(|err| format!("исходный терминал {label}: {err}"))?;
        let pump = child
            .master
            .try_clone()
            .map_err(|err| format!("PTY фикстуры {label}: {err}"))?;
        let route = Route::start(pump, source).map_err(|err| format!("маршрут {label}: {err}"))?;
        let reference =
            Reference::new(&self.config).map_err(|err| format!("эталон {label}: {err}"))?;
        Ok(Feed {
            label: label.to_owned(),
            nonce: fixture_nonce,
            child: Some(child),
            link,
            route: Some(route),
            reference,
            sent: 0,
            responses: Vec::new(),
            cprs_sent: 0,
        })
    }

    fn spawn(&self, fixture_nonce: &str) -> Result<(PtyChild, Link, Ready), String> {
        let args =
            fixture::command_args(Mode::Emitter, self.listener()?.path(), fixture_nonce, None);
        let child = pty::spawn(&Spawn {
            program: &self.fixture,
            args: &args,
            env: &self.env,
            size: self.config.winsize(COLS, ROWS),
        })
        .map_err(|err| format!("фикстура {fixture_nonce}: {err}"))?;
        let mut link = self
            .listener()?
            .accept(WAIT)
            .map_err(|err| format!("фикстура не подключилась: {err}"))?;
        let ready = link
            .wait_for(WAIT, |receipt| match &receipt.event {
                Event::Ready(ready) => Some(ready.clone()),
                _ => None,
            })
            .map_err(|err| format!("готовность фикстуры: {err}"))?;
        if ready.nonce != fixture_nonce || ready.pid != child.pid() || !ready.raw {
            return Err(format!(
                "предусловие: готова фикстура {} PID {} raw {}, ожидалась {fixture_nonce} PID {}",
                ready.nonce,
                ready.pid,
                ready.raw,
                child.pid()
            ));
        }
        Ok((child, link, ready))
    }

    fn artifact(&self, name: &str, value: &impl Serialize) -> Result<String, String> {
        results::write_json(&self.out, name, value)?;
        Ok(name.to_owned())
    }

    fn verdict(&mut self, cell: &str, part: &str, verdict: Kind, detail: impl Into<String>) {
        self.verdicts.push(Verdict {
            cell: cell.to_owned(),
            part: part.to_owned(),
            verdict,
            detail: detail.into(),
        });
    }

    /// Уборка: любые процессы с nonce запуска и короткий каталог.
    fn teardown(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        let leftovers = launch::find_by_token(&self.nonce);
        for process in &leftovers {
            // SAFETY: сигнал процессу по PID без указателей.
            unsafe { libc::kill(process.pid, libc::SIGKILL) };
            notes.push(format!(
                "уборка: оставшийся процесс {} {}",
                process.pid, process.command
            ));
        }
        let pids: Vec<i32> = leftovers.iter().map(|process| process.pid).collect();
        launch::wait_exit(&pids, STOP_GRACE);
        self.listener = None;
        if let Err(err) = fs::remove_dir_all(&self.root) {
            notes.push(format!("уборка: {}: {err}", self.root.display()));
        }
        notes
    }
}

/// Фикстура, модель хоста на её PTY и эталон.
struct Feed {
    label: String,
    nonce: String,
    child: Option<PtyChild>,
    link: Link,
    route: Option<Route>,
    reference: Reference,
    /// Сколько байтов фикстура записала в PTY по командам стенда.
    sent: u64,
    /// Ответы терминала, которые фикстура прочитала, по порядку.
    responses: Vec<(ResponseKind, String)>,
    /// Сколько полных запросов CPR фикстура отправила: по ним [`Feed::settle`] узнаёт свой.
    cprs_sent: usize,
}

impl Feed {
    fn route(&self) -> Result<&Route, String> {
        self.route
            .as_ref()
            .ok_or_else(|| format!("{}: маршрут остановлен", self.label))
    }

    /// Ждёт, пока исходный терминал получит всё записанное фикстурой.
    fn sync(&self) -> Result<(), String> {
        let sent = self.sent;
        self.route()?
            .wait_until(WAIT, |source, _| source.fed() >= sent)
            .map_err(|err| {
                format!(
                    "{}: исходный терминал не получил {sent} байт: {err}",
                    self.label
                )
            })?;
        let fed = self.route()?.with(|source| source.fed());
        if fed != sent {
            return Err(format!(
                "{}: исходный терминал получил {fed} байт, фикстура записала {sent}",
                self.label
            ));
        }
        Ok(())
    }

    fn written(&mut self, command: Command, expected: usize) -> Result<(), String> {
        let written = self
            .link
            .request(command, WAIT)
            .map_err(|err| format!("{}: запись фикстуры: {err}", self.label))?;
        if written != expected as u64 {
            return Err(format!(
                "{}: фикстура записала {written} байт из {expected}",
                self.label
            ));
        }
        self.sent += written;
        Ok(())
    }

    /// Вывод нагрузки: фикстура пишет байты, эталон получает их напрямую, исходный терминал — из
    /// PTY; возврат, когда исходный терминал получил всё.
    fn emit(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.written(Command::Write { hex: hex(bytes) }, bytes.len())?;
        self.reference.feed(bytes);
        self.sync()
    }

    /// Запрос целиком или префиксом `split`; возвращает номер команды для остатка.
    fn query(&mut self, query: Query, split: Option<u32>) -> Result<u64, String> {
        let bytes = query.bytes();
        let id = self
            .link
            .send(Command::Query { query, split })
            .map_err(|err| format!("{}: запрос: {err}", self.label))?;
        let expected = split.map_or(bytes.len(), |at| at as usize);
        let written = self
            .link
            .wait_for(WAIT, |receipt| match &receipt.event {
                Event::Ack {
                    id: acked,
                    bytes,
                    error,
                } if *acked == id => Some((*bytes, error.clone())),
                _ => None,
            })
            .map_err(|err| format!("{}: подтверждение запроса: {err}", self.label))?;
        match written {
            (bytes, None) if bytes == expected as u64 => {}
            (bytes, error) => {
                return Err(format!(
                    "{}: запрос {query:?} разрез {split:?}: записано {bytes} из {expected}, {error:?}",
                    self.label
                ));
            }
        }
        self.sent += expected as u64;
        if query == Query::CursorPosition && split.is_none() {
            self.cprs_sent += 1;
        }
        self.reference.feed(&bytes[..expected]);
        self.sync()?;
        Ok(id)
    }

    /// Остаток разрезанного запроса `id`.
    fn rest(&mut self, id: u64, query: Query, split: u32) -> Result<(), String> {
        let bytes = query.bytes();
        let rest = &bytes[split as usize..];
        self.written(Command::Rest { query: id }, rest.len())?;
        if query == Query::CursorPosition {
            self.cprs_sent += 1;
        }
        self.reference.feed(rest);
        self.sync()
    }

    /// Квитанции ответов до условия над ними; остальные квитанции не теряются.
    fn responses_until(
        &mut self,
        mut done: impl FnMut(&[(ResponseKind, String)]) -> bool,
    ) -> Result<(), String> {
        let deadline = Instant::now() + WAIT;
        while !done(&self.responses) {
            let left = deadline.saturating_duration_since(Instant::now());
            let receipt = self
                .link
                .recv(left)
                .map_err(|err| format!("{}: ответы фикстуры: {err}", self.label))?;
            if let Event::Response { response, hex, .. } = receipt.event {
                self.responses.push((response, hex));
            }
        }
        Ok(())
    }

    /// Контрольный CPR: когда пришёл ответ на него, ответы на всё, что фикстура отправила раньше,
    /// уже прочитаны ею. Ответы на CPR считаются по отправленным запросам, поэтому контрольный —
    /// последний CPR. Возвращает ответы с отметки `from` до контрольного, без него.
    fn settle(&mut self, from: usize) -> Result<Vec<(ResponseKind, String)>, String> {
        self.query(Query::CursorPosition, None)?;
        let expected = self.cprs_sent;
        self.responses_until(|responses| {
            responses
                .iter()
                .filter(|(kind, _)| *kind == ResponseKind::CursorPosition)
                .count()
                >= expected
        })?;
        let mut got: Vec<(ResponseKind, String)> = self.responses[from..].to_vec();
        if let Some(last) = got
            .iter()
            .rposition(|(kind, _)| *kind == ResponseKind::CursorPosition)
        {
            got.remove(last);
        }
        Ok(got)
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.reference
            .resize(cols, rows)
            .map_err(|err| format!("{}: ресайз эталона: {err}", self.label))?;
        self.route()?.resize(cols, rows)
    }

    /// Исходный терминал совпадает с эталоном: иначе стенд не доказал, что модель хоста получила
    /// поток без потерь.
    fn sane(&self) -> Result<(), String> {
        let expected = self
            .reference
            .capture(Capture::default())
            .map_err(|err| format!("{}: состояние эталона: {err}", self.label))?;
        let actual = self
            .route()?
            .with(|source| source.capture(Capture::default()))
            .map_err(|err| format!("{}: состояние исходного терминала: {err}", self.label))?;
        let comparison = compare(&expected, &actual);
        if comparison.outcome() == Outcome::Equal {
            Ok(())
        } else {
            Err(format!(
                "{}: исходный терминал разошёлся с эталоном: {comparison}",
                self.label
            ))
        }
    }

    fn journal(&self) -> Result<Vec<crate::oracle::vt::JournalEntry>, String> {
        Ok(self.route()?.with(|source| source.journal().to_vec()))
    }

    /// Выход фикстуры и остановка маршрута; оставшийся процесс добивается.
    fn close(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        let _ = self.link.send(Command::Exit { code: 0 });
        if let Some(mut route) = self.route.take() {
            route.stop();
        }
        if let Some(mut child) = self.child.take() {
            let pid = child.pid();
            if !launch::wait_exit(&[pid], STOP_GRACE).is_empty() {
                // SAFETY: сигнал своему потомку по PID.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                notes.push(format!("{}: фикстура {pid} добита", self.label));
            }
            let _ = child.child.wait();
        }
        notes
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.close();
    }
}

// --- восстановление ---

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Method {
    /// formatter VT со всеми дополнениями, затем continuation.
    Formatter,
    /// Полный снимок `GHOSTSNP` с continuation внутри.
    Snapshot,
}

impl Method {
    const ALL: [Method; 2] = [Method::Formatter, Method::Snapshot];

    fn name(self) -> &'static str {
        match self {
            Method::Formatter => "formatter",
            Method::Snapshot => "snapshot",
        }
    }
}

/// Полезная нагрузка восстановления: что отдал C API.
#[derive(Clone, Debug, Default, Serialize)]
struct Payload {
    /// formatter или снимок, байты.
    body: usize,
    /// continuation, байты (у снимка — внутри).
    continuation: Option<usize>,
    /// Байт потока, после которого сделано восстановление.
    boundary: u64,
    /// Ответы и эффекты, которые сама нагрузка восстановления вызвала у приёмника.
    restore_journal: Vec<Journaled>,
}

/// Восстановленный приёмник и нагрузка восстановления или отказ C API.
type Restored = Result<(Receiver, Payload), String>;

/// Приёмник из исходного терминала или отказ C API (`Ok(Err)`); `Err` — ошибка стенда.
fn restore(
    method: Method,
    source: &vt::Terminal,
    config: &Config,
    boundary: u64,
) -> Result<Restored, String> {
    let profile = ReceiverProfile::for_client(method.name());
    let mut payload = Payload {
        boundary,
        ..Payload::default()
    };
    let receiver = match method {
        Method::Formatter => {
            let body = match source.format(vt::FormatOptions::VT_FULL) {
                Ok(body) => body,
                Err(err) => return Ok(Err(format!("formatter: {err}"))),
            };
            let continuation = match source.continuation() {
                Ok(continuation) => continuation,
                Err(err) => return Ok(Err(format!("continuation: {err}"))),
            };
            let mut receiver = Receiver::new(method.name(), config, profile)
                .map_err(|err| format!("приёмник formatter: {err}"))?;
            receiver.feed(&body);
            receiver.feed(&continuation);
            payload.body = body.len();
            payload.continuation = Some(continuation.len());
            receiver
        }
        Method::Snapshot => {
            let body = match source.snapshot() {
                Ok(body) => body,
                Err(err) => return Ok(Err(format!("снимок: {err}"))),
            };
            payload.body = body.len();
            match Receiver::from_snapshot(
                method.name(),
                config,
                profile,
                &body,
                CONTINUATION_BUDGET,
            ) {
                Ok(receiver) => receiver,
                Err(err) => return Ok(Err(format!("декодирование снимка: {err}"))),
            }
        }
    };
    payload.restore_journal = receiver
        .journal()
        .iter()
        .map(|entry| entry.event.clone())
        .collect();
    Ok(Ok((receiver, payload)))
}

/// Приёмники обоими способами в текущей точке потока.
fn restore_all(stand: &Stand, feed: &Feed) -> Result<Vec<(Method, Restored)>, String> {
    let boundary = feed.sent;
    let config = stand.config.clone();
    Method::ALL
        .into_iter()
        .map(|method| {
            let restored = feed
                .route()?
                .with(|source| restore(method, source.terminal(), &config, boundary))?;
            Ok((method, restored))
        })
        .collect()
}

fn compare_receiver(
    evidence: &mut Evidence,
    name: &str,
    reference: &Reference,
    receiver: &Receiver,
) -> Result<Comparison, String> {
    let expected = reference
        .capture(Capture::default())
        .map_err(|err| format!("состояние эталона: {err}"))?;
    let actual = receiver
        .capture(Capture::default())
        .map_err(|err| format!("состояние приёмника: {err}"))?;
    let comparison = compare(&expected, &actual);
    evidence.note(&format!("{name}.reference_text"), expected.text());
    evidence.note(&format!("{name}.receiver_text"), actual.text());
    evidence.note(&format!("{name}.comparison"), &comparison);
    match comparison.outcome() {
        Outcome::Equal => {
            evidence.check(name, true, comparison.to_string());
        }
        Outcome::Differs => {
            evidence.check(name, false, comparison.to_string());
        }
        Outcome::Incomplete => {
            evidence.not_proven = Some(format!("{name}: сравнение неполное: {comparison}"));
        }
    }
    Ok(comparison)
}

/// Пути расхождений сравнения без индексов клеток: что именно не восстановилось.
fn mismatch_kinds(comparison: &Comparison) -> Vec<String> {
    let mut kinds: Vec<String> = comparison
        .mismatches
        .iter()
        .map(|diff| {
            let mut path = String::new();
            let mut depth = 0;
            for ch in diff.path.chars() {
                match ch {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ if depth == 0 => path.push(ch),
                    _ => {}
                }
            }
            path
        })
        .collect();
    kinds.sort();
    kinds.dedup();
    kinds
}

fn classify(direct: &Comparison, after: Option<&Comparison>) -> (Kind, String) {
    let summary = |comparison: &Comparison| {
        format!(
            "{} расхождений: {}",
            comparison.mismatch_count,
            mismatch_kinds(comparison).join(", ")
        )
    };
    match (direct.outcome(), after.map(Comparison::outcome)) {
        (Outcome::Differs, _) => (Kind::Lost, summary(direct)),
        (Outcome::Incomplete, _) | (_, Some(Outcome::Incomplete)) => {
            (Kind::Incomplete, "сравнение неполное".to_owned())
        }
        (Outcome::Equal, Some(Outcome::Differs)) => match after {
            Some(after) => (
                Kind::SuffixOnly,
                format!("после суффикса {}", summary(after)),
            ),
            None => (Kind::Restored, String::new()),
        },
        (Outcome::Equal, _) => (Kind::Restored, "совпало с эталоном".to_owned()),
    }
}

/// Суффикс приёмникам без кандидата; эталону и исходному терминалу — через фикстуру.
fn apply_suffix(
    feed: &mut Feed,
    receivers: &mut [&mut Receiver],
    steps: &[Step],
) -> Result<(), String> {
    for step in steps {
        match step {
            Step::Output(bytes) => {
                feed.emit(bytes)?;
                for receiver in receivers.iter_mut() {
                    Sink::output(&mut **receiver, bytes)?;
                }
            }
            Step::Resize { cols, rows } => {
                feed.resize(*cols, *rows)?;
                for receiver in receivers.iter_mut() {
                    Sink::resize(&mut **receiver, *cols, *rows)?;
                }
            }
        }
    }
    Ok(())
}

// --- сюжеты восстановления B1–B6, B9 ---

pub(crate) struct Story {
    pub(crate) cell: &'static str,
    pub(crate) stimulus: &'static str,
    pub(crate) scene: Vec<Vec<u8>>,
    pub(crate) suffixes: Vec<(String, Vec<Step>)>,
}

fn catalog_suffix(name: &str) -> (String, Vec<Step>) {
    let steps = suffix::by_name(name, COLS, ROWS)
        .map(|suffix| suffix.steps)
        .unwrap_or_default();
    (name.to_owned(), steps)
}

pub(crate) fn stories() -> Vec<Story> {
    vec![
        Story {
            cell: "B1",
            stimulus: "заголовок, 40 строк истории с SGR 8/256/RGB, подчёркивания с цветом, \
                       мягко перенесённая гиперссылка в истории и в активной области, перо и \
                       открытая гиперссылка на конце",
            scene: b1_scene(),
            suffixes: vec![(
                "link-tail".to_owned(),
                vec![Step::Output(b"tail\x1b]8;;\x1b\\ end".to_vec())],
            )],
        },
        Story {
            cell: "B2",
            stimulus: "основной экран с текстом, затем alternate screen 1049 со своим текстом",
            scene: vec![
                numbered("main", 10),
                b"\x1b[3;5Hmain-cursor".to_vec(),
                b"\x1b[?1049h\x1b[H\x1b[7malt screen\x1b[0m\x1b[10;20Halt-cursor".to_vec(),
            ],
            suffixes: vec![catalog_suffix("alt-exit")],
        },
        Story {
            cell: "B3",
            stimulus: "DECSC с позицией и пером, затем другое перо и позиция",
            scene: vec![
                numbered("b3", 6),
                b"\x1b[5;10H\x1b[1;32m\x1b7".to_vec(),
                b"\x1b[0m\x1b[20;1Hmoved".to_vec(),
            ],
            suffixes: vec![catalog_suffix("decrc")],
        },
        Story {
            cell: "B4",
            stimulus: "стек Kitty: push 1, push 3, текст",
            scene: vec![b"kitty\r\n\x1b[>1u\x1b[>3u".to_vec(), b"flags 3".to_vec()],
            suffixes: vec![catalog_suffix("kitty-pop")],
        },
        Story {
            cell: "B5",
            stimulus: "мышь 1000+1006, bracketed paste, DECCKM, DECKPAM, табуляторы 5 и 17, \
                       DECSTBM 5–15 с DECOM, режим 2048",
            scene: vec![
                numbered("b5", 3),
                b"\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1h\x1b=".to_vec(),
                b"\x1b[3g\x1b[1;5H\x1bH\x1b[1;17H\x1bH".to_vec(),
                b"\x1b[5;15r\x1b[?6h\x1b[2;1Hin region".to_vec(),
                b"\x1b[?2048h".to_vec(),
            ],
            suffixes: vec![catalog_suffix("tab"), catalog_suffix("margins")],
        },
        Story {
            cell: "B6",
            stimulus: "широкий символ на границе строки, комбинирующие и emoji, напечатанные \
                       пробелы против пустых клеток, строка до отложенного переноса",
            scene: b6_scene(),
            suffixes: vec![catalog_suffix("pending-wrap")],
        },
        Story {
            cell: "B9",
            stimulus: "мягко перенесённые строки 120 и 200 колонок в истории и активной области",
            scene: b9_scene(),
            suffixes: vec![catalog_suffix("resize")],
        },
    ]
}

fn numbered(prefix: &str, lines: usize) -> Vec<u8> {
    (0..lines)
        .map(|line| format!("{prefix} {line:02}\r\n"))
        .collect::<String>()
        .into_bytes()
}

fn b1_scene() -> Vec<Vec<u8>> {
    let mut steps = vec![b"\x1b]2;pss-capi-b1\x07".to_vec()];
    for line in 0..40u8 {
        let color = match line % 3 {
            0 => format!("\x1b[3{}m", line % 8),
            1 => format!("\x1b[38;5;{}m", 16 + line),
            _ => format!("\x1b[38;2;{};{};{}m", line * 5, 200 - line, 7),
        };
        steps.push(format!("{color}line {line:02}\x1b[0m plain\r\n").into_bytes());
        if line == 5 {
            steps.push(link_line("https://example.test/b1-history", 90));
        }
    }
    steps.push(b"\x1b[4:3;58;5;196mcurly\x1b[0m \x1b[9;53mstrike\x1b[0m\r\n".to_vec());
    steps.push(link_line("https://example.test/b1-active", 100));
    steps.push(b"\x1b[1;3;38;5;123mpen \x1b]8;;https://example.test/open\x1b\\open".to_vec());
    steps
}

fn link_line(uri: &str, len: usize) -> Vec<u8> {
    let mut line = format!("\x1b]8;id=lk;{uri}\x1b\\").into_bytes();
    line.extend(std::iter::repeat_n(b'L', len));
    line.extend_from_slice(b"\x1b]8;;\x1b\\\r\n");
    line
}

fn b6_scene() -> Vec<Vec<u8>> {
    let mut wide_edge = vec![b'w'; usize::from(COLS) - 1];
    wide_edge.extend_from_slice("世界\r\n".as_bytes());
    let mut full = vec![b'p'; usize::from(COLS)];
    full.splice(0..0, b"\x1b[2;1H".iter().copied());
    vec![
        wide_edge,
        "e\u{301} cafe\u{301} 👍🏽 🇺🇸 👨‍👩‍👧 ok\r\n".as_bytes().to_vec(),
        b"a   b\x1b[5Cc\r\n".to_vec(),
        b"\x1b[20;1H".to_vec(),
        vec![b'z'; usize::from(COLS)],
    ]
}

fn b9_scene() -> Vec<Vec<u8>> {
    let mut steps = Vec::new();
    for line in 0..30u32 {
        let width = if line % 2 == 0 { 120 } else { 200 };
        let mut text = format!("r{line:02}:").into_bytes();
        let fill = b'a' + (line % 26) as u8;
        text.resize(width, fill);
        text.extend_from_slice(b"\r\n");
        steps.push(text);
    }
    steps.push(b"tail".to_vec());
    steps
}

fn restore_story(stand: &mut Stand, story: &Story) -> Result<Vec<Cell>, String> {
    let label = story.cell.to_lowercase();
    let mut feed = stand.feed(&label, ReceiverProfile::for_client("host"))?;
    for step in &story.scene {
        feed.emit(step)?;
    }
    feed.sane()?;
    let restored = restore_all(stand, &feed)?;
    let id = format!("capi-{}", story.cell);
    let mut parts: Vec<(Method, Evidence, Option<Receiver>, Option<Comparison>)> = Vec::new();
    for (method, restored) in restored {
        let mut evidence = Evidence::default();
        match restored {
            Ok((receiver, payload)) => {
                evidence.note("payload", &payload);
                evidence.check("C API отдал нагрузку восстановления", true, "ok");
                let direct = compare_receiver(
                    &mut evidence,
                    "после восстановления",
                    &feed.reference,
                    &receiver,
                )?;
                parts.push((method, evidence, Some(receiver), Some(direct)));
            }
            Err(refusal) => {
                evidence.check("C API отдал нагрузку восстановления", false, &refusal);
                parts.push((method, evidence, None, None));
            }
        }
    }
    let mut after: Vec<Option<Comparison>> = vec![None; parts.len()];
    for (name, steps) in &story.suffixes {
        {
            let mut receivers: Vec<&mut Receiver> = parts
                .iter_mut()
                .filter_map(|(_, _, receiver, _)| receiver.as_mut())
                .collect();
            apply_suffix(&mut feed, &mut receivers, steps)?;
        }
        feed.sane()?;
        for (index, (_, evidence, receiver, _)) in parts.iter_mut().enumerate() {
            if let Some(receiver) = receiver {
                let comparison = compare_receiver(
                    evidence,
                    &format!("суффикс {name}"),
                    &feed.reference,
                    receiver,
                )?;
                if after[index]
                    .as_ref()
                    .is_none_or(|seen| seen.outcome() == Outcome::Equal)
                {
                    after[index] = Some(comparison);
                }
            }
        }
    }
    let mut cells = Vec::new();
    let artifact_name = format!("{id}.json").to_lowercase();
    let mut artifact = serde_json::Map::new();
    for (index, (method, evidence, _, direct)) in parts.iter().enumerate() {
        let (kind, detail) = match direct {
            Some(direct) => classify(direct, after[index].as_ref()),
            None => (Kind::Refused, evidence.status().1),
        };
        stand.verdict(&id, method.name(), kind, detail);
        artifact.insert(method.name().to_owned(), json!(evidence));
    }
    let artifact = stand.artifact(&artifact_name, &artifact)?;
    let suffix_names: Vec<&str> = story
        .suffixes
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    for (method, evidence, _, _) in &parts {
        cells.push(cell(
            Spec {
                id: &id,
                part: Some(method.name()),
                stimulus: format!(
                    "{}; восстановление {}; суффиксы: {}",
                    story.stimulus,
                    method.name(),
                    suffix_names.join(", ")
                ),
                oracle: "эталон против приёмника, восстановленного из исходного терминала, \
                         сразу и после каждого суффикса",
                completion: "исходный терминал получил весь поток и совпал с эталоном, \
                             состояния сняты после восстановления и после суффиксов",
                origin: DefectOrigin::Vt,
            },
            evidence,
            vec![artifact.clone()],
        ));
    }
    let notes = feed.close();
    if !notes.is_empty() {
        stand.verdict(&id, "уборка", Kind::Checked, notes.join("; "));
    }
    Ok(cells)
}

// --- B7: незавершённая последовательность в точке восстановления ---

/// Вариант B7: имя, префикс до точки восстановления, ожидаемая continuation и остаток после неё.
pub(crate) type B7Variant = (&'static str, &'static [u8], &'static [u8], &'static [u8]);

pub(crate) const B7_VARIANTS: [B7Variant; 5] = [
    ("csi", b"\x1b[3", b"\x1b[3", b"1mX"),
    ("osc", b"\x1b]2;tit", b"\x1b]2;tit", b"le-b7\x07X"),
    ("dcs", b"\x1bP$q", b"\x1bP$q", b"m\x1b\\X"),
    ("utf8", b"ab\xe4\xb8", b"\xe4\xb8", b"\x96X"),
    ("esc", b"\x1b", b"\x1b", b"7X"),
];

fn b7(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut artifact = serde_json::Map::new();
    let mut parts = Vec::new();
    for (variant, prefix, expected, rest) in B7_VARIANTS {
        let mut feed = stand.feed(
            &format!("b7-{variant}"),
            ReceiverProfile::for_client("host"),
        )?;
        feed.emit(b"b7 scene\r\n\x1b[1mbold ")?;
        feed.emit(prefix)?;
        let ground = feed
            .route()?
            .with(|source| source.terminal().vt_ground())
            .map_err(|err| format!("b7-{variant}: ground: {err}"))?;
        if ground {
            return Err(format!(
                "b7-{variant}: исходный терминал на ground после префикса"
            ));
        }
        feed.sane()?;
        let mut restored = Vec::new();
        for (method, outcome) in restore_all(stand, &feed)? {
            let mut evidence = Evidence::default();
            match outcome {
                Ok((receiver, payload)) => {
                    evidence.note("payload", &payload);
                    evidence.check(
                        "continuation равна незавершённой части префикса",
                        payload.continuation.is_none_or(|len| len == expected.len()),
                        format!(
                            "{:?} байт, ожидалось {}",
                            payload.continuation,
                            expected.len()
                        ),
                    );
                    let direct = compare_receiver(
                        &mut evidence,
                        "после восстановления",
                        &feed.reference,
                        &receiver,
                    )?;
                    restored.push((method, evidence, Some((receiver, direct))));
                }
                Err(refusal) => {
                    evidence.check("C API отдал нагрузку восстановления", false, &refusal);
                    restored.push((method, evidence, None));
                }
            }
        }
        feed.emit(rest)?;
        feed.sane()?;
        for (method, mut evidence, receiver) in restored {
            let part = format!("{}-{variant}", method.name());
            let (kind, detail) = match receiver {
                Some((mut receiver, direct)) => {
                    receiver.feed(rest);
                    let after = compare_receiver(
                        &mut evidence,
                        "после остатка",
                        &feed.reference,
                        &receiver,
                    )?;
                    classify(&direct, Some(&after))
                }
                None => (Kind::Refused, evidence.status().1),
            };
            stand.verdict("capi-B7", &part, kind, detail);
            artifact.insert(part.clone(), json!(evidence));
            parts.push((part, evidence));
        }
    }
    let artifact = stand.artifact("capi-b7.json", &artifact)?;
    Ok(parts
        .iter()
        .map(|(part, evidence)| {
            cell(
                Spec {
                    id: "capi-B7",
                    part: Some(part),
                    stimulus: format!(
                        "вывод с пером, затем префикс последовательности до точки \
                         восстановления, остаток и метка после неё ({part})"
                    ),
                    oracle: "эталон против приёмника сразу после восстановления и после остатка",
                    completion: "исходный терминал получил префикс отдельной записью до \
                                 восстановления и не на ground; остаток подан после",
                    origin: DefectOrigin::Vt,
                },
                evidence,
                vec![artifact.clone()],
            )
        })
        .collect())
}

// --- B8: граница байта между восстановлением и живым потоком в одном процессе ---

const B8_LINES: u64 = 300;
/// Шире окна: каждая строка потока мягко переносится.
const B8_WIDTH: u16 = 100;
/// Сколько строк истории сверяется построчно с эталоном.
const B8_HISTORY: usize = 80;
/// Срезов, равномерно разнесённых по всему потоку, вдобавок к окну каждого байта.
const B8_SAMPLES: usize = 16;

/// Номера строк потока по строкам экрана сверху вниз: история от дальней, затем активная область.
pub(crate) fn flood_sequence(nonce: &str, state: &model::State) -> Vec<u64> {
    state
        .history
        .iter()
        .rev()
        .chain(state.active.iter())
        .filter_map(|row| flood_index(nonce, row.text().as_bytes()))
        .collect()
}

/// Нарушения последовательности: пропуск, повтор, не тот конец.
pub(crate) fn sequence_problems(sequence: &[u64], last: u64) -> Vec<String> {
    let mut problems = Vec::new();
    for pair in sequence.windows(2) {
        if pair[1] != pair[0] + 1 {
            problems.push(format!("после {} идёт {}", pair[0], pair[1]));
        }
    }
    if sequence.last() != Some(&last) {
        problems.push(format!(
            "последняя строка {:?}, ожидалась {last}",
            sequence.last()
        ));
    }
    problems
}

/// Строки приёмника против строк эталона, выровненные от активной области вниз-вверх.
fn row_problems(reference: &model::State, receiver: &model::State) -> Vec<String> {
    let mut problems = Vec::new();
    for (y, (a, b)) in reference.active.iter().zip(&receiver.active).enumerate() {
        if a.text() != b.text() {
            problems.push(format!(
                "активная строка {y}: «{}» против «{}»",
                a.text(),
                b.text()
            ));
        }
    }
    for (index, (a, b)) in reference.history.iter().zip(&receiver.history).enumerate() {
        if a.text() != b.text() {
            problems.push(format!(
                "строка истории {}: «{}» против «{}»",
                index + 1,
                a.text(),
                b.text()
            ));
        }
    }
    problems
}

/// Срезы B8: каждый байт двух строк в середине потока и равномерная выборка.
fn b8_cuts(total: usize, line: usize) -> (Vec<usize>, Vec<usize>) {
    let mid = (total / line / 2) * line;
    let window: Vec<usize> = (mid.saturating_sub(3)..(mid + 2 * line + 3).min(total)).collect();
    let samples: Vec<usize> = (1..=B8_SAMPLES)
        .map(|step| step * total / (B8_SAMPLES + 1))
        .chain([1, total - 1])
        .collect();
    (window, samples)
}

fn b8(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut feed = stand.feed("b8", ReceiverProfile::for_client("host"))?;
    let stream: Vec<u8> = (1..=B8_LINES)
        .flat_map(|index| flood_line(&feed.nonce, index, B8_WIDTH))
        .collect();
    let line = flood_line(&feed.nonce, 1, B8_WIDTH).len();
    let id = feed
        .link
        .send(Command::Flood {
            lines: B8_LINES,
            width: B8_WIDTH,
            interval_us: 0,
            report_every: B8_LINES,
        })
        .map_err(|err| format!("b8: поток: {err}"))?;
    let (lines, bytes) = feed
        .link
        .wait_for(WAIT, |receipt| match &receipt.event {
            Event::FloodDone {
                id: done,
                lines,
                bytes,
                cancelled: false,
                ..
            } if *done == id => Some((*lines, *bytes)),
            _ => None,
        })
        .map_err(|err| format!("b8: конец потока: {err}"))?;
    if lines != B8_LINES || bytes != stream.len() as u64 {
        return Err(format!(
            "b8: фикстура записала {lines} строк и {bytes} байт, ожидалось {B8_LINES} и {}",
            stream.len()
        ));
    }
    feed.sent += bytes;
    feed.reference.feed(&stream);
    feed.sync()?;
    feed.sane()?;
    let reference_rows = feed
        .reference
        .capture(Capture {
            history: B8_HISTORY,
            hidden: false,
        })
        .map_err(|err| format!("b8: эталон: {err}"))?;
    let reference_full = feed
        .reference
        .capture(Capture::default())
        .map_err(|err| format!("b8: эталон: {err}"))?;
    let (window, samples) = b8_cuts(stream.len(), line);
    let mut cells = Vec::new();
    let mut artifact = serde_json::Map::new();
    for method in Method::ALL {
        let mut evidence = Evidence::default();
        let mut failures: Vec<Value> = Vec::new();
        let mut checked = 0usize;
        let mut full: Vec<Value> = Vec::new();
        let mut payload_sizes: Vec<usize> = Vec::new();
        for (cut, sampled) in window
            .iter()
            .map(|cut| (*cut, false))
            .chain(samples.iter().map(|cut| (*cut, true)))
        {
            let mut source =
                Reference::new(&stand.source).map_err(|err| format!("b8: источник: {err}"))?;
            source.feed(&stream[..cut]);
            let restored = restore(method, source.terminal(), &stand.config, cut as u64)?;
            let (mut receiver, payload) = match restored {
                Ok(restored) => restored,
                Err(refusal) => {
                    failures.push(json!({ "cut": cut, "refusal": refusal }));
                    continue;
                }
            };
            payload_sizes.push(payload.body + payload.continuation.unwrap_or(0));
            receiver.feed(&stream[cut..]);
            let state = receiver
                .capture(Capture {
                    history: B8_HISTORY,
                    hidden: false,
                })
                .map_err(|err| format!("b8: приёмник: {err}"))?;
            checked += 1;
            let mut problems = sequence_problems(&flood_sequence(&feed.nonce, &state), B8_LINES);
            problems.extend(row_problems(&reference_rows, &state));
            if !problems.is_empty() {
                problems.truncate(5);
                failures.push(json!({ "cut": cut, "problems": problems }));
            }
            if sampled {
                let state = receiver
                    .capture(Capture::default())
                    .map_err(|err| format!("b8: приёмник: {err}"))?;
                let comparison = compare(&reference_full, &state);
                full.push(json!({
                    "cut": cut,
                    "outcome": comparison.outcome(),
                    "mismatches": comparison.mismatch_count,
                    "kinds": mismatch_kinds(&comparison),
                }));
            }
        }
        evidence.note("stream_bytes", stream.len());
        evidence.note("line_bytes", line);
        evidence.note(
            "window",
            json!({ "from": window.first(), "to": window.last(), "cuts": window.len() }),
        );
        evidence.note("samples", &samples);
        evidence.note(
            "payload_bytes",
            json!({
                "min": payload_sizes.iter().min(),
                "max": payload_sizes.iter().max(),
            }),
        );
        evidence.note("full_compare_samples", &full);
        evidence.note("failures", &failures);
        let first: Vec<String> = failures.iter().take(3).map(Value::to_string).collect();
        evidence.check(
            "нет пропуска и повтора байтов на границе",
            failures.is_empty(),
            format!(
                "срезов {}, проверено {checked}, с нарушением {}{}",
                window.len() + samples.len(),
                failures.len(),
                if first.is_empty() {
                    String::new()
                } else {
                    format!(": {}", first.join("; "))
                }
            ),
        );
        let lost = full
            .iter()
            .filter(|entry| entry["outcome"] != json!(Outcome::Equal))
            .count();
        let kinds: Vec<String> = full
            .iter()
            .flat_map(|entry| {
                entry["kinds"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|kind| kind.as_str().map(str::to_owned))
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let (kind, detail) = if failures.is_empty() {
            (
                Kind::Checked,
                format!(
                    "граница точна на {checked} срезах; полное сравнение на выборке: {lost} из {} \
                     расходятся{}",
                    full.len(),
                    if kinds.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", kinds.join(", "))
                    }
                ),
            )
        } else {
            (
                Kind::Lost,
                format!("нарушений на {} срезах", failures.len()),
            )
        };
        stand.verdict("capi-B8", method.name(), kind, detail);
        artifact.insert(method.name().to_owned(), json!(evidence));
        cells.push((method, evidence));
    }
    let artifact = stand.artifact("capi-b8.json", &artifact)?;
    Ok(cells
        .iter()
        .map(|(method, evidence)| {
            cell(
                Spec {
                    id: "capi-B8",
                    part: Some(method.name()),
                    stimulus: format!(
                        "поток {B8_LINES} нумерованных строк по {B8_WIDTH} колонок из фикстуры; \
                         в одном процессе восстановление {} на каждом байте двух строк середины и \
                         на выборке, затем остаток потока",
                        method.name()
                    ),
                    oracle: "счётчик строк потока у приёмника без пропуска и повтора, строки \
                             приёмника против эталона",
                    completion: "исходный терминал получил весь поток и совпал с эталоном, все \
                                 срезы проверены",
                    origin: DefectOrigin::Vt,
                },
                evidence,
                vec![artifact.clone()],
            )
        })
        .collect())
}

// --- запросы и эффекты ---

fn kinds(responses: &[(ResponseKind, String)]) -> Vec<ResponseKind> {
    responses.iter().map(|(kind, _)| *kind).collect()
}

/// Ответы, которые приёмник записал бы в PTY, разобранные по видам.
fn receiver_replies(journal: &[crate::oracle::vt::JournalEntry]) -> Vec<ResponseKind> {
    let mut bytes = Vec::new();
    for entry in journal {
        if let Journaled::Reply { hex } = &entry.event {
            bytes.extend(channel::unhex(hex).unwrap_or_default());
        }
    }
    ResponseParser::default()
        .feed(&bytes)
        .into_iter()
        .map(|(kind, _)| kind)
        .collect()
}

fn d_cell(
    id: &str,
    part: &str,
    stimulus: String,
    oracle: &str,
    evidence: &Evidence,
    artifact: &str,
) -> Cell {
    cell(
        Spec {
            id,
            part: Some(part),
            stimulus,
            oracle,
            completion: "контрольный CPR после запросов получен: ответы на всё раньше прочитаны \
                         фикстурой",
            origin: DefectOrigin::Vt,
        },
        evidence,
        vec![artifact.to_owned()],
    )
}

fn d1(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut feed = stand.feed("d1", ReceiverProfile::for_client("host"))?;
    feed.emit(b"d1 queries\r\n")?;
    let mut parts: Vec<(String, String, &str, Evidence)> = Vec::new();

    // Хост отвечает сам.
    let mut host = Evidence::default();
    let from = feed.responses.len();
    for (_, query) in D1_QUERIES {
        feed.query(query, None)?;
    }
    let got = feed.settle(from)?;
    host.note("responses", &got);
    let expected: Vec<ResponseKind> = D1_QUERIES.iter().map(|(_, q)| q.response()).collect();
    host.check(
        "ровно один ответ хоста на каждый запрос по порядку",
        kinds(&got) == expected,
        format!("{:?}", kinds(&got)),
    );
    let da1 = got.iter().find(|(kind, _)| *kind == ResponseKind::Da1);
    host.check(
        "DA1 из колбэка профиля хоста",
        da1.is_some_and(|(_, value)| *value == hex(b"\x1b[?62;22;52c")),
        format!("{da1:?}"),
    );
    let xtversion = got
        .iter()
        .find(|(kind, _)| *kind == ResponseKind::XtVersion);
    host.check(
        "XTVERSION из колбэка профиля хоста",
        xtversion.is_some_and(|(_, value)| value.contains(&hex(b"shellio-probe-receiver(host)"))),
        format!("{xtversion:?}"),
    );
    parts.push((
        "host".to_owned(),
        "DA1, DA2, CPR, DECRQM частный и ANSI, OSC 10, 11, 4, XTVERSION целиком; хост — \
         исходный терминал с колбэками"
            .to_owned(),
        "квитанции ответов фикстуры по видам и байтам",
        host,
    ));

    // Те же запросы в живом потоке клиенту, восстановленному на ground.
    let mut forwarded = Evidence::default();
    let boundary = feed.sent;
    let config = stand.config.clone();
    let client = feed
        .route()?
        .with(|source| restore(Method::Formatter, source.terminal(), &config, boundary))?;
    match client {
        Ok((mut client, payload)) => {
            forwarded.note("payload", &payload);
            let before = client.journal().len();
            for (_, query) in D1_QUERIES {
                client.feed(&query.bytes());
            }
            let replies = receiver_replies(&client.journal()[before..]);
            forwarded.note("client_replies", &replies);
            forwarded.check(
                "клиент не отвечает на запросы, которые обслужил хост",
                replies.is_empty(),
                format!(
                    "клиент с WRITE_PTY ответил {} раз на {} запросов: C API не помечает \
                     обслуженные запросы в потоке, хосту нужен свой фильтр",
                    replies.len(),
                    D1_QUERIES.len()
                ),
            );
        }
        Err(refusal) => {
            forwarded.check("C API отдал нагрузку восстановления", false, refusal);
        }
    }
    parts.push((
        "client-stream".to_owned(),
        "те же запросы живым потоком клиенту, восстановленному formatter на ground".to_owned(),
        "ответы клиента из его журнала",
        forwarded,
    ));

    // Запрос, разрезанный точкой восстановления.
    let split = 2;
    let from = feed.responses.len();
    let id = feed.query(Query::Da1, Some(split))?;
    feed.sane()?;
    let restored = restore_all(stand, &feed)?;
    feed.rest(id, Query::Da1, split)?;
    let got = feed.settle(from)?;
    for (method, outcome) in restored {
        let mut evidence = Evidence::default();
        evidence.note("host_responses", &got);
        let host_answers = got
            .iter()
            .filter(|(kind, _)| *kind == ResponseKind::Da1)
            .count();
        match outcome {
            Ok((mut client, payload)) => {
                evidence.note("payload", &payload);
                let before = client.journal().len();
                client.feed(&Query::Da1.bytes()[split as usize..]);
                let replies = receiver_replies(&client.journal()[before..]);
                evidence.note("client_replies", &replies);
                let client_answers = replies
                    .iter()
                    .filter(|kind| **kind == ResponseKind::Da1)
                    .count();
                evidence.check(
                    "хост ответил ровно один раз",
                    host_answers == 1,
                    format!("{host_answers}"),
                );
                evidence.check(
                    "ровно один ответ на запрос среди хоста и клиента",
                    host_answers + client_answers == 1,
                    format!(
                        "хост {host_answers}, клиент {client_answers}: continuation отдаёт клиенту \
                         префикс запроса, остаток живым потоком завершает его у клиента"
                    ),
                );
            }
            Err(refusal) => {
                evidence.check("C API отдал нагрузку восстановления", false, refusal);
            }
        }
        parts.push((
            format!("restore-boundary-{}", method.name()),
            format!(
                "DA1 разрезан после 2 байт точкой восстановления {}; остаток хосту и клиенту",
                method.name()
            ),
            "ответы хоста по квитанциям фикстуры и ответы клиента из его журнала",
            evidence,
        ));
    }
    feed.close();
    finish_parts(stand, "capi-D1", "capi-d1.json", parts)
}

/// Итог сюжета запросов: артефакт, строки таблицы и клетки.
fn finish_parts(
    stand: &mut Stand,
    id: &str,
    name: &str,
    parts: Vec<(String, String, &str, Evidence)>,
) -> Result<Vec<Cell>, String> {
    let mut artifact = serde_json::Map::new();
    for (part, _, _, evidence) in &parts {
        artifact.insert(part.clone(), json!(evidence));
        let (status, reason) = evidence.status();
        stand.verdict(
            id,
            part,
            Kind::Checked,
            format!("{}: {reason}", status.name()),
        );
    }
    let artifact = stand.artifact(name, &artifact)?;
    Ok(parts
        .iter()
        .map(|(part, stimulus, oracle, evidence)| {
            d_cell(id, part, stimulus.clone(), oracle, evidence, &artifact)
        })
        .collect())
}

fn d2(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut feed = stand.feed("d2", ReceiverProfile::for_client("host"))?;
    feed.emit(b"d2 split queries\r\n")?;
    let mut parts = Vec::new();
    for (name, query) in D1_QUERIES {
        let mut evidence = Evidence::default();
        let len = query.bytes().len();
        let from = feed.responses.len();
        for split in 1..len {
            let split = u32::try_from(split).map_err(|err| format!("d2: разрез: {err}"))?;
            let id = feed.query(query, Some(split))?;
            feed.rest(id, query, split)?;
        }
        let got = feed.settle(from)?;
        evidence.note("responses", &got);
        let splits = len - 1;
        let matching = got
            .iter()
            .filter(|(kind, _)| *kind == query.response())
            .count();
        evidence.check(
            "ровно один ответ на каждый разрез",
            matching == splits && got.len() == splits,
            format!(
                "разрезов {splits}, ответов {}, нужного вида {matching}",
                got.len()
            ),
        );
        let first = got.first().map(|(_, value)| value.clone());
        evidence.check(
            "ответ не зависит от разреза",
            got.iter().all(|(_, value)| Some(value) == first.as_ref()),
            format!("{first:?}"),
        );
        parts.push((
            name.to_owned(),
            format!(
                "{query:?} разрезан на каждой границе байта: префикс отдельной записью исходному \
                 терминалу, остаток после"
            ),
            "квитанции ответов фикстуры; граница — по числу байтов, полученных исходным \
             терминалом до остатка",
            evidence,
        ));
    }
    feed.close();
    finish_parts(stand, "capi-D2", "capi-d2.json", parts)
}

const OSC52_WRITE: &[u8] = b"\x1b]52;c;aGVsbG8=\x07";
const OSC52_PREFIX: &[u8] = b"\x1b]52;c;d29y";
const OSC52_REST: &[u8] = b"bGQ=\x07";

fn clipboard_writes(journal: &[crate::oracle::vt::JournalEntry]) -> Vec<Vec<(String, String)>> {
    journal
        .iter()
        .filter_map(|entry| match &entry.event {
            Journaled::ClipboardWrite { contents, .. } => Some(contents.clone()),
            _ => None,
        })
        .collect()
}

fn d3(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut feed = stand.feed("d3", ReceiverProfile::for_client("host"))?;
    feed.emit(b"d3 clipboard\r\n")?;
    feed.emit(OSC52_WRITE)?;
    let host_first = clipboard_writes(&feed.journal()?);
    feed.sane()?;
    let replay = restore_all(stand, &feed)?;
    feed.emit(OSC52_PREFIX)?;
    feed.sane()?;
    let cut = restore_all(stand, &feed)?;
    feed.emit(OSC52_REST)?;
    let host_all = clipboard_writes(&feed.journal()?);
    let mut parts = Vec::new();
    for (method, outcome) in replay {
        let mut evidence = Evidence::default();
        evidence.note("host_writes", &host_first);
        evidence.check(
            "хост выполнил запись один раз",
            host_first.len() == 1,
            format!("{}", host_first.len()),
        );
        match outcome {
            Ok((receiver, payload)) => {
                evidence.note("payload", &payload);
                let writes = clipboard_writes(receiver.journal());
                evidence.check(
                    "восстановление не повторяет запись",
                    writes.is_empty(),
                    format!("записей у клиента {}", writes.len()),
                );
            }
            Err(refusal) => {
                evidence.check("C API отдал нагрузку восстановления", false, refusal);
            }
        }
        parts.push((
            format!("{}-replay", method.name()),
            format!(
                "OSC 52 записи целиком до точки восстановления {}",
                method.name()
            ),
            "журнал эффектов хоста и клиента",
            evidence,
        ));
    }
    for (method, outcome) in cut {
        let mut evidence = Evidence::default();
        evidence.note("host_writes", &host_all);
        match outcome {
            Ok((mut receiver, payload)) => {
                evidence.note("payload", &payload);
                let at_restore = clipboard_writes(receiver.journal()).len();
                receiver.feed(OSC52_REST);
                let writes = clipboard_writes(receiver.journal());
                evidence.note("client_writes", &writes);
                let world = vec![("text/plain".to_owned(), hex(b"world"))];
                evidence.check(
                    "клиент выполнил разрезанную запись один раз и после остатка",
                    at_restore == 0 && writes == [world.clone()],
                    format!("при восстановлении {at_restore}, всего {}", writes.len()),
                );
                evidence.note(
                    "both_sides",
                    format!(
                        "хост выполнил {} записей, клиент {}: одна сторона — решение хоста",
                        host_all.len(),
                        writes.len()
                    ),
                );
            }
            Err(refusal) => {
                evidence.check("C API отдал нагрузку восстановления", false, refusal);
            }
        }
        parts.push((
            format!("{}-cut", method.name()),
            format!(
                "OSC 52 записи разрезан точкой восстановления {}: префикс до неё, остаток после",
                method.name()
            ),
            "журнал эффектов хоста и клиента",
            evidence,
        ));
    }
    feed.close();
    finish_parts(stand, "capi-D3", "capi-d3.json", parts)
}

// --- D4: чтение буфера обмена ---

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(char::from(TABLE[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Полезная нагрузка ответа OSC 52: данные между `c;` и терминатором.
fn osc52_payload(response_hex: &str) -> Option<String> {
    let bytes = channel::unhex(response_hex)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let body = text.strip_prefix("\x1b]52;")?;
    let (_, data) = body.split_once(';')?;
    Some(
        data.trim_end_matches('\x07')
            .trim_end_matches("\x1b\\")
            .to_owned(),
    )
}

fn clipboard_answers(responses: &[(ResponseKind, String)]) -> Vec<Option<String>> {
    responses
        .iter()
        .filter(|(kind, _)| *kind == ResponseKind::Clipboard)
        .map(|(_, value)| osc52_payload(value))
        .collect()
}

fn d4(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut parts = Vec::new();
    let profile = ReceiverProfile::for_client("host");
    let data = profile.clipboard.clone().unwrap_or_default();

    // Синхронный ответ из колбэка.
    let mut feed = stand.feed("d4-sync", profile.clone())?;
    let from = feed.responses.len();
    feed.query(Query::ClipboardRead, None)?;
    let got = feed.settle(from)?;
    let mut evidence = Evidence::default();
    evidence.note("responses", &got);
    let answers = clipboard_answers(&got);
    evidence.check(
        "один ответ с данными колбэка",
        answers == [Some(base64(&data))],
        format!("{answers:?}"),
    );
    parts.push((
        "sync".to_owned(),
        "OSC 52 чтения, колбэк отвечает до возврата".to_owned(),
        "квитанции ответов фикстуры",
        evidence,
    ));

    // Разрез на каждой границе байта.
    let len = Query::ClipboardRead.bytes().len();
    let from = feed.responses.len();
    for split in 1..len {
        let split = u32::try_from(split).map_err(|err| format!("d4: разрез: {err}"))?;
        let id = feed.query(Query::ClipboardRead, Some(split))?;
        feed.rest(id, Query::ClipboardRead, split)?;
    }
    let got = feed.settle(from)?;
    let mut evidence = Evidence::default();
    evidence.note("responses", &got);
    let answers = clipboard_answers(&got);
    evidence.check(
        "ровно один ответ на каждый разрез",
        answers.len() == len - 1
            && got.len() == len - 1
            && answers.iter().all(|answer| *answer == Some(base64(&data))),
        format!("разрезов {}, ответов {}", len - 1, got.len()),
    );
    parts.push((
        "cut".to_owned(),
        "OSC 52 чтения разрезан на каждой границе байта".to_owned(),
        "квитанции ответов фикстуры",
        evidence,
    ));
    feed.close();

    // Колбэк вернулся без ответа.
    let mut silent = profile.clone();
    silent.clipboard = None;
    let mut feed = stand.feed("d4-no-reply", silent)?;
    let from = feed.responses.len();
    feed.query(Query::ClipboardRead, None)?;
    let got = feed.settle(from)?;
    let mut evidence = Evidence::default();
    evidence.note("responses", &got);
    let answers = clipboard_answers(&got);
    evidence.check(
        "возврат без ответа даёт ровно один пустой ответ",
        answers == [Some(String::new())],
        format!("{answers:?}"),
    );
    parts.push((
        "no-reply".to_owned(),
        "OSC 52 чтения, колбэк возвращается без reply".to_owned(),
        "квитанции ответов фикстуры",
        evidence,
    ));
    feed.close();

    // Колбэка нет.
    let mut feed = stand.feed("d4-unset", profile.clone())?;
    feed.route()?
        .with(Receiver::unset_clipboard_read)
        .map_err(|err| format!("d4: снятие колбэка: {err}"))?;
    let from = feed.responses.len();
    feed.query(Query::ClipboardRead, None)?;
    let got = feed.settle(from)?;
    let mut evidence = Evidence::default();
    evidence.note("responses", &got);
    evidence.check(
        "без колбэка терминал не отвечает",
        clipboard_answers(&got).is_empty(),
        format!("{got:?}"),
    );
    parts.push((
        "unset".to_owned(),
        "OSC 52 чтения без колбэка CLIPBOARD_READ".to_owned(),
        "квитанции ответов фикстуры",
        evidence,
    ));
    feed.close();

    // Схема ответа GUI: ожидание с поколением и тайм-аутом в потоке разбора.
    for threaded in [true, false] {
        let (part, evidence) = bounded_wait(stand, threaded)?;
        parts.push((
            part.to_owned(),
            format!(
                "модель хоста ждёт ответ модели GUI в колбэке до {} мс с поколением; чтение PTY \
                 {}; GUI отвечает сразу, молчит, отвечает поздно; пока поток разбора ждёт, \
                 фикстура пишет {BLOCK_FLOOD_LINES} строк по {BLOCK_FLOOD_WIDTH} колонок",
                CLIPBOARD_TIMEOUT.as_millis(),
                if threaded {
                    "в отдельном потоке с очередью до 4 МиБ"
                } else {
                    "в потоке разбора (контроль)"
                }
            ),
            "квитанции ответов фикстуры, квитанция конца потока с задержками записи, журнал \
             поколений модели",
            evidence,
        ));
    }
    finish_parts(stand, "capi-D4", "capi-d4.json", parts)
}

/// Что модель GUI делает с запросом своего поколения.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum GuiPlan {
    /// Отвечает сразу.
    Now,
    /// Не отвечает до следующего запроса, а перед ответом на него присылает поздний ответ.
    Late,
}

#[derive(Clone, Debug, Default, Serialize)]
struct GuiLog {
    requests: Vec<u64>,
    answered: Vec<u64>,
    timeouts: Vec<u64>,
    /// Поздние ответы старых поколений, отброшенные ожиданием.
    dropped: Vec<u64>,
    /// Сколько ждал поток разбора, мс.
    waited_ms: Vec<u128>,
}

/// Модель хоста с ожиданием GUI в колбэке: чтение PTY, разбор, ответы обратно в PTY.
struct BlockingHost {
    reader: Option<JoinHandle<()>>,
    parser: Option<JoinHandle<()>>,
    gui: Option<JoinHandle<()>>,
    log: Arc<Mutex<GuiLog>>,
    fed: Arc<Mutex<u64>>,
}

impl BlockingHost {
    fn start(
        master: &OwnedFd,
        plans: Vec<GuiPlan>,
        threaded: bool,
    ) -> Result<BlockingHost, String> {
        let log = Arc::new(Mutex::new(GuiLog::default()));
        let fed = Arc::new(Mutex::new(0u64));
        let (request_tx, request_rx) = mpsc::channel::<u64>();
        let (answer_tx, answer_rx) = mpsc::channel::<(u64, Vec<u8>)>();
        let gui = thread::Builder::new()
            .name("capi-gui".to_owned())
            .spawn(move || {
                let mut held: Vec<u64> = Vec::new();
                for generation in request_rx {
                    for late in held.drain(..) {
                        let _ = answer_tx.send((late, LATE_CLIPBOARD.to_vec()));
                    }
                    let plan = usize::try_from(generation - 1)
                        .ok()
                        .and_then(|index| plans.get(index).copied())
                        .unwrap_or(GuiPlan::Now);
                    match plan {
                        GuiPlan::Now => {
                            let _ = answer_tx.send((generation, GUI_CLIPBOARD.to_vec()));
                        }
                        GuiPlan::Late => held.push(generation),
                    }
                }
            })
            .map_err(|err| format!("поток модели GUI: {err}"))?;
        let mut term =
            vt::Terminal::new(COLS, ROWS).map_err(|err| format!("модель хоста: {err}"))?;
        term.enable_effects(vt::Responder::default())
            .map_err(|err| format!("модель хоста: {err}"))?;
        let source_log = Arc::clone(&log);
        let mut generation = 0u64;
        term.set_clipboard_source(Some(Box::new(move |_request: &ClipboardRequest| {
            generation += 1;
            let started = Instant::now();
            let record = |f: &dyn Fn(&mut GuiLog)| {
                let mut log = source_log
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                f(&mut log);
            };
            record(&|log| log.requests.push(generation));
            if request_tx.send(generation).is_err() {
                return None;
            }
            let deadline = started + CLIPBOARD_TIMEOUT;
            let answer = loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match answer_rx.recv_timeout(left) {
                    Ok((answered, data)) if answered == generation => break Some(data),
                    Ok((old, _)) => record(&|log| log.dropped.push(old)),
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break None,
                }
            };
            let waited = started.elapsed().as_millis();
            let current = generation;
            match &answer {
                Some(_) => record(&|log| {
                    log.answered.push(current);
                    log.waited_ms.push(waited);
                }),
                None => record(&|log| {
                    log.timeouts.push(current);
                    log.waited_ms.push(waited);
                }),
            }
            answer
        })));
        let reader_fd = master
            .try_clone()
            .map_err(|err| format!("мастер модели хоста: {err}"))?;
        let writer_fd = master
            .try_clone()
            .map_err(|err| format!("мастер модели хоста: {err}"))?;
        let mut parse = Parse {
            term,
            writer: writer_fd,
            fed: Arc::clone(&fed),
        };
        let (reader, parser) = if threaded {
            let queue = Arc::new(ByteQueue::new(READ_QUEUE_BYTES));
            let input = Arc::clone(&queue);
            let reader = thread::Builder::new()
                .name("capi-read".to_owned())
                .spawn(move || {
                    read_loop(&reader_fd, |chunk| input.push(&chunk));
                    input.close();
                })
                .map_err(|err| format!("поток чтения: {err}"))?;
            let parser = thread::Builder::new()
                .name("capi-parse".to_owned())
                .spawn(move || {
                    while let Some(bytes) = queue.pop() {
                        parse.feed(&bytes);
                    }
                })
                .map_err(|err| format!("поток разбора: {err}"))?;
            (reader, Some(parser))
        } else {
            let reader = thread::Builder::new()
                .name("capi-read-parse".to_owned())
                .spawn(move || {
                    read_loop(&reader_fd, |chunk| {
                        parse.feed(&chunk);
                        true
                    })
                })
                .map_err(|err| format!("поток чтения: {err}"))?;
            (reader, None)
        };
        Ok(BlockingHost {
            reader: Some(reader),
            parser,
            gui: Some(gui),
            log,
            fed,
        })
    }

    fn log(&self) -> GuiLog {
        self.log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn fed(&self) -> u64 {
        *self
            .fed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Ждёт конца потоков после выхода фикстуры: PTY закрыт, чтение кончилось.
    fn join(&mut self) {
        for handle in [self.reader.take(), self.parser.take(), self.gui.take()]
            .into_iter()
            .flatten()
        {
            let _ = handle.join();
        }
    }
}

/// Очередь байтов между чтением PTY и разбором с пределом в байтах: полная очередь
/// останавливает чтение, а не теряет вывод.
struct ByteQueue {
    state: Mutex<(Vec<u8>, bool)>,
    changed: std::sync::Condvar,
    cap: usize,
}

impl ByteQueue {
    fn new(cap: usize) -> ByteQueue {
        ByteQueue {
            state: Mutex::new((Vec::new(), false)),
            changed: std::sync::Condvar::new(),
            cap,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (Vec<u8>, bool)> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Ждёт места; `false` — очередь закрыта.
    fn push(&self, bytes: &[u8]) -> bool {
        let mut state = self.lock();
        while !state.1 && !state.0.is_empty() && state.0.len() + bytes.len() > self.cap {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        if state.1 {
            return false;
        }
        state.0.extend_from_slice(bytes);
        self.changed.notify_all();
        true
    }

    /// Всё накопленное; `None` — очередь закрыта и пуста.
    fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.lock();
        while state.0.is_empty() && !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        if state.0.is_empty() {
            return None;
        }
        let bytes = std::mem::take(&mut state.0);
        self.changed.notify_all();
        Some(bytes)
    }

    fn close(&self) {
        self.lock().1 = true;
        self.changed.notify_all();
    }
}

struct Parse {
    term: vt::Terminal,
    writer: OwnedFd,
    fed: Arc<Mutex<u64>>,
}

impl Parse {
    fn feed(&mut self, chunk: &[u8]) {
        self.term.write(chunk);
        *self
            .fed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) += chunk.len() as u64;
        let mut reply = self.term.take_pty_output();
        while !reply.is_empty() {
            // SAFETY: запись байтов ответа из своего буфера в открытый дескриптор мастера.
            let written =
                unsafe { libc::write(self.writer.as_raw_fd(), reply.as_ptr().cast(), reply.len()) };
            match usize::try_from(written) {
                Ok(len) if len > 0 => {
                    reply.drain(..len);
                }
                _ => {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::Interrupted {
                        break;
                    }
                }
            }
        }
    }
}

/// Блокирующее чтение мастера до конца PTY; `deliver` получает каждый кусок.
fn read_loop(master: &OwnedFd, mut deliver: impl FnMut(Vec<u8>) -> bool) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        // SAFETY: буфер свой, чтение не дальше его длины.
        let len = unsafe { libc::read(master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        match usize::try_from(len) {
            Ok(0) => break,
            Ok(len) => {
                if !deliver(buf[..len].to_vec()) {
                    break;
                }
            }
            Err(_) => {
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }
}

/// Ответ на чтение буфера обмена от фикстуры: время квитанции и данные.
fn next_clipboard(link: &mut Link) -> Result<(u64, Option<String>), String> {
    link.wait_for(WAIT, |receipt| match &receipt.event {
        Event::Response {
            response: ResponseKind::Clipboard,
            hex,
            ..
        } => Some((receipt.t_ns, osc52_payload(hex))),
        _ => None,
    })
    .map_err(|err| format!("ответ на чтение буфера обмена: {err}"))
}

fn bounded_wait(stand: &mut Stand, threaded: bool) -> Result<(&'static str, Evidence), String> {
    let part = if threaded {
        "bounded-wait-threaded"
    } else {
        "bounded-wait-inline"
    };
    stand.feeds += 1;
    let fixture_nonce = format!("{}-capi{}-{part}", stand.nonce, stand.feeds);
    let (mut child, mut link, _) = stand.spawn(&fixture_nonce)?;
    let mut host = BlockingHost::start(
        &child.master,
        vec![GuiPlan::Now, GuiPlan::Late, GuiPlan::Now],
        threaded,
    )?;
    let mut evidence = Evidence::default();
    let outcome = bounded_wait_body(&mut link, &host, &mut evidence, threaded);
    let _ = link.send(Command::Exit { code: 0 });
    let pid = child.pid();
    if !launch::wait_exit(&[pid], STOP_GRACE).is_empty() {
        // SAFETY: сигнал своему потомку по PID.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let _ = child.child.wait();
    drop(child.master);
    host.join();
    outcome?;
    evidence.note("gui_log", host.log());
    evidence.note("host_fed", host.fed());
    Ok((part, evidence))
}

fn bounded_wait_body(
    link: &mut Link,
    host: &BlockingHost,
    evidence: &mut Evidence,
    threaded: bool,
) -> Result<(), String> {
    let write = |link: &mut Link, command: Command| {
        link.request(command, WAIT)
            .map_err(|err| format!("команда фикстуры: {err}"))
    };
    // Поколение 1: GUI отвечает сразу.
    write(
        link,
        Command::Query {
            query: Query::ClipboardRead,
            split: None,
        },
    )?;
    let (_, first) = next_clipboard(link)?;
    evidence.check(
        "ответ GUI в срок доходит до программы",
        first.as_deref() == Some(base64(GUI_CLIPBOARD).as_str()),
        format!("{first:?}"),
    );
    // Поколение 2: GUI молчит; пока поток разбора ждёт, фикстура пишет поток.
    write(
        link,
        Command::Query {
            query: Query::ClipboardRead,
            split: None,
        },
    )?;
    let id = link
        .send(Command::Flood {
            lines: BLOCK_FLOOD_LINES,
            width: BLOCK_FLOOD_WIDTH,
            interval_us: 0,
            report_every: BLOCK_FLOOD_LINES,
        })
        .map_err(|err| format!("поток: {err}"))?;
    let (done_ns, writes): (u64, WriteStats) = link
        .wait_for(WAIT, |receipt| match &receipt.event {
            Event::FloodDone {
                id: done, writes, ..
            } if *done == id => Some((receipt.t_ns, *writes)),
            _ => None,
        })
        .map_err(|err| format!("конец потока: {err}"))?;
    let (answer_ns, second) = next_clipboard(link)?;
    evidence.note(
        "flood",
        json!({
            "done_before_answer_ms": (answer_ns as i128 - done_ns as i128) as f64 / 1e6,
            "writes": writes,
        }),
    );
    evidence.check(
        "GUI молчит: по тайм-ауту программа получает пустой ответ",
        second.as_deref() == Some(""),
        format!("{second:?}"),
    );
    let blocked = Duration::from_nanos(writes.stalled_ns) >= CLIPBOARD_TIMEOUT / 2;
    let flood_first = done_ns < answer_ns;
    if threaded {
        evidence.check(
            "чтение PTY не стоит, пока поток разбора ждёт GUI",
            flood_first && !blocked,
            format!(
                "поток фикстуры закончен {} ответа, задержки записи {} мс ({} раз)",
                if flood_first { "до" } else { "после" },
                writes.stalled_ns / 1_000_000,
                writes.stalls
            ),
        );
    } else {
        evidence.check(
            "контроль: без отдельного потока чтения запись фикстуры стоит весь тайм-аут",
            blocked,
            format!(
                "поток фикстуры закончен {} ответа, задержки записи {} мс ({} раз)",
                if flood_first { "до" } else { "после" },
                writes.stalled_ns / 1_000_000,
                writes.stalls
            ),
        );
    }
    // Поколение 3: перед ответом GUI присылает поздний ответ поколения 2.
    write(
        link,
        Command::Query {
            query: Query::ClipboardRead,
            split: None,
        },
    )?;
    let (_, third) = next_clipboard(link)?;
    let log = host.log();
    evidence.check(
        "поздний ответ старого поколения отброшен, программа получает ответ своего",
        third.as_deref() == Some(base64(GUI_CLIPBOARD).as_str()) && log.dropped == [2],
        format!("ответ {third:?}, отброшены {:?}", log.dropped),
    );
    evidence.check(
        "ровно один ответ на каждый запрос",
        log.requests == [1, 2, 3] && log.answered == [1, 3] && log.timeouts == [2],
        format!("{log:?}"),
    );
    Ok(())
}

// --- бюджет continuation ---

fn budget(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut parts = Vec::new();

    // Сверх бюджета.
    let mut feed = stand.feed("budget-over", ReceiverProfile::for_client("host"))?;
    feed.emit(b"budget\r\n")?;
    let mut prefix = b"\x1b]2;".to_vec();
    prefix.resize(CONTINUATION_BUDGET + 1024, b'a');
    feed.emit(&prefix)?;
    feed.sane()?;
    let mut evidence = Evidence::default();
    let (continuation, snapshot, formatted) = feed.route()?.with(|source| {
        let term = source.terminal();
        (
            term.continuation().map(|c| c.len()),
            term.snapshot().map(|s| s.len()),
            term.format(vt::FormatOptions::VT_FULL),
        )
    });
    evidence.note(
        "over",
        json!({
            "prefix": prefix.len(),
            "continuation": format!("{continuation:?}"),
            "snapshot": format!("{snapshot:?}"),
            "formatter": formatted.as_ref().map(Vec::len).map_err(ToString::to_string),
        }),
    );
    evidence.check(
        "сверх бюджета continuation недоступна",
        continuation.is_err(),
        format!("{continuation:?}"),
    );
    evidence.check(
        "сверх бюджета снимок недоступен",
        snapshot.is_err(),
        format!("{snapshot:?}"),
    );
    // Formatter без continuation: что получит клиент, если хост отдаст экран без неё.
    let lossy = match formatted {
        Ok(body) => {
            let mut receiver =
                Receiver::new("lossy", &stand.config, ReceiverProfile::for_client("lossy"))
                    .map_err(|err| format!("budget: приёмник: {err}"))?;
            receiver.feed(&body);
            Some(receiver)
        }
        Err(_) => None,
    };
    let rest = b"tail\x07X";
    feed.emit(rest)?;
    feed.sane()?;
    if let Some(mut receiver) = lossy {
        receiver.feed(rest);
        let comparison = {
            let mut scratch = Evidence::default();
            compare_receiver(&mut scratch, "без continuation", &feed.reference, &receiver)?
        };
        evidence.note(
            "lossy_formatter",
            json!({
                "outcome": comparison.outcome(),
                "mismatches": comparison.mismatch_count,
                "kinds": mismatch_kinds(&comparison),
            }),
        );
    }
    let (continuation, snapshot) = feed.route()?.with(|source| {
        let term = source.terminal();
        (
            term.continuation().map(|c| c.len()),
            term.snapshot().map(|s| s.len()),
        )
    });
    evidence.check(
        "после конца последовательности continuation и снимок снова доступны",
        continuation == Ok(0) && snapshot.is_ok(),
        format!("continuation {continuation:?}, снимок {snapshot:?}"),
    );
    parts.push((
        "over-budget".to_owned(),
        format!(
            "OSC 2 длиной {} байт при бюджете {CONTINUATION_BUDGET} не закончен в точке \
             восстановления, затем остаток",
            prefix.len()
        ),
        "результаты continuation, снимка и formatter исходного терминала; приёмник formatter \
         без continuation против эталона",
        evidence,
    ));
    feed.close();

    // В пределах бюджета.
    let mut feed = stand.feed("budget-within", ReceiverProfile::for_client("host"))?;
    feed.emit(b"budget\r\n")?;
    let mut prefix = b"\x1b]2;".to_vec();
    prefix.resize(CONTINUATION_BUDGET - 64, b'b');
    feed.emit(&prefix)?;
    feed.sane()?;
    let mut evidence = Evidence::default();
    let started = Instant::now();
    let (continuation, snapshot) = feed.route()?.with(|source| {
        let term = source.terminal();
        (term.continuation(), term.snapshot())
    });
    evidence.note("export_ms", started.elapsed().as_secs_f64() * 1e3);
    let continuation = continuation.map_err(|err| format!("budget: continuation: {err}"));
    let snapshot = snapshot.map_err(|err| format!("budget: снимок: {err}"));
    evidence.check(
        "в пределах бюджета continuation равна префиксу",
        continuation.as_ref().is_ok_and(|c| c.len() == prefix.len()),
        format!("{:?}", continuation.as_ref().map(Vec::len)),
    );
    if let Ok(snapshot) = &snapshot {
        evidence.note("snapshot_bytes", snapshot.len());
        let profile = ReceiverProfile::for_client("limit");
        let below = Receiver::from_snapshot(
            "limit",
            &stand.config,
            profile.clone(),
            snapshot,
            prefix.len() - 1,
        );
        let exact =
            Receiver::from_snapshot("limit", &stand.config, profile, snapshot, prefix.len());
        evidence.check(
            "декодер отвергает снимок, если его предел меньше continuation",
            below.is_err() && exact.is_ok(),
            format!(
                "предел {}: {:?}; предел {}: {:?}",
                prefix.len() - 1,
                below.as_ref().err(),
                prefix.len(),
                exact.as_ref().err()
            ),
        );
    } else {
        evidence.check(
            "снимок в пределах бюджета",
            false,
            format!("{:?}", snapshot.err()),
        );
    }
    let restored = restore_all(stand, &feed)?;
    let rest = b"tail\x07X";
    feed.emit(rest)?;
    feed.sane()?;
    for (method, outcome) in restored {
        match outcome {
            Ok((mut receiver, payload)) => {
                evidence.note(&format!("{}.payload", method.name()), &payload);
                receiver.feed(rest);
                compare_receiver(
                    &mut evidence,
                    &format!("{} после остатка", method.name()),
                    &feed.reference,
                    &receiver,
                )?;
            }
            Err(refusal) => {
                evidence.check(
                    &format!("{}: C API отдал нагрузку восстановления", method.name()),
                    false,
                    refusal,
                );
            }
        }
    }
    parts.push((
        "within-budget".to_owned(),
        format!(
            "OSC 2 длиной {} байт при бюджете {CONTINUATION_BUDGET} не закончен в точке \
             восстановления; восстановление обоими способами, затем остаток",
            prefix.len()
        ),
        "continuation и снимок исходного терминала, пределы декодера, приёмники против эталона \
         после остатка",
        evidence,
    ));
    feed.close();
    finish_parts(stand, "capi-budget", "capi-budget.json", parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn osc52_payload_strips_both_terminators() {
        assert_eq!(
            osc52_payload(&hex(b"\x1b]52;c;aGk=\x07")).as_deref(),
            Some("aGk=")
        );
        assert_eq!(
            osc52_payload(&hex(b"\x1b]52;c;\x1b\\")).as_deref(),
            Some("")
        );
        assert_eq!(osc52_payload(&hex(b"\x1b[0n")), None);
    }

    #[test]
    fn sequence_problems_find_gaps_repeats_and_wrong_end() {
        assert!(sequence_problems(&[3, 4, 5], 5).is_empty());
        assert_eq!(sequence_problems(&[3, 5], 5), ["после 3 идёт 5"]);
        assert_eq!(sequence_problems(&[3, 4, 4], 4), ["после 4 идёт 4"]);
        assert_eq!(
            sequence_problems(&[3, 4], 5),
            ["последняя строка Some(4), ожидалась 5"]
        );
    }

    #[test]
    fn b8_cuts_cover_window_and_samples_inside_stream() {
        let (window, samples) = b8_cuts(1000, 100);
        assert_eq!(window.first(), Some(&497));
        assert_eq!(window.len(), 206);
        assert!(samples.iter().all(|cut| (1..1000).contains(cut)));
        assert_eq!(samples.len(), B8_SAMPLES + 2);
    }

    #[test]
    fn byte_queue_bounds_bytes_and_drains_after_close() {
        let queue = Arc::new(ByteQueue::new(4));
        assert!(queue.push(b"ab"));
        assert!(queue.push(b"cd"));
        let pusher = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || queue.push(b"ef"))
        };
        assert_eq!(queue.pop().as_deref(), Some(&b"abcd"[..]));
        assert!(pusher.join().unwrap_or(false));
        queue.close();
        assert_eq!(queue.pop().as_deref(), Some(&b"ef"[..]));
        assert_eq!(queue.pop(), None);
        assert!(!queue.push(b"gh"));
    }

    #[test]
    fn mismatch_kinds_drop_indices() {
        let comparison = Comparison {
            mismatches: vec![
                model::Difference {
                    path: "active[0][3].text".to_owned(),
                    reference: String::new(),
                    receiver: String::new(),
                },
                model::Difference {
                    path: "active[2][1].text".to_owned(),
                    reference: String::new(),
                    receiver: String::new(),
                },
                model::Difference {
                    path: "saved_cursor.y".to_owned(),
                    reference: String::new(),
                    receiver: String::new(),
                },
            ],
            mismatch_count: 3,
            ..Comparison::default()
        };
        assert_eq!(
            mismatch_kinds(&comparison),
            ["active.text", "saved_cursor.y"]
        );
    }
}
