//! Рубеж D — запросы терминала и эффекты на zmx (задача 9) и на Rust-хосте (задача 12), клетки
//! D1–D6.
//!
//! Ответы считает фикстура `query` по своему входу (`Event::Response`), стенд читает их из журнала
//! квитанций с отметки части. Отсутствие ответа доказывает более позднее упорядоченное событие:
//! барьер DSR через каждого живого клиента или, без клиента, контрольный DA, на который отвечает
//! сам хост. Нет такого события — часть `not_run`, а не `pass` и не `fail`. Каждая часть идёт на
//! своей сессии: поздние ответы одной части не попадают в другую.
//!
//! У Rust-хоста эффекты UI идут типизированным путём: стенд играет GUI лидера соединением роли
//! GUI с его поколением. Чтение буфера обмена (D4), фокус и видимость окна (D5) идут через это
//! соединение, а фокус приёмника клиента — отрицательный контроль: режимы отчётов хост вырезает
//! из потока клиенту. Граница разреза D2 у zmx — ревизия `zmx screen`, у Rust-хоста — событие
//! разбора наблюдателя.

use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::args::Target;
use crate::backend::{Backend, InputPath, Lead, Role, SessionId, parse_role};
use crate::fixture::Mode;
use crate::fixture::channel::{Command, Event, Link, hex};
use crate::fixture::query::{Query, ResponseKind};
use crate::matrix::Candidate;
use crate::matrix::capi::{D1_QUERIES, GUI_CLIPBOARD, LATE_CLIPBOARD, base64, osc52_payload};
use crate::matrix::evidence::{Evidence, Spec, cell};
use crate::matrix::stand::{Client, DETACH_KEY, ParsedCut, Session, Stand, WAIT, generation};
use crate::matrix::util::{Tally, cuts, revision_boundary};
use crate::oracle::vt::{BARRIER, Journaled};
use crate::results::{Cell, DefectOrigin, Environment, Report};

/// Ответ приёмника на барьер `CSI 5 n`.
const BARRIER_REPLY: &str = "1b5b306e";
/// Срок контрольного события без клиента и роста ревизии в протоколе разреза.
const SETTLE: Duration = Duration::from_secs(5);
/// Предел headless-операции D6: дольше — операция считается заблокированной.
const OP_LIMIT: Duration = Duration::from_secs(5);
/// Запросы D2: короткие и длинные, CSI и OSC.
const D2_QUERIES: [(&str, Query); 4] = [
    ("da1", Query::Da1),
    ("cpr", Query::CursorPosition),
    ("xtversion", Query::XtVersion),
    ("osc11", Query::BackgroundColor),
];
const OSC52_WRITE: &[u8] = b"\x1b]52;c;aGVsbG8=\x07";
const FOCUS_SIZE_ON: &[u8] = b"\x1b[?1004h\x1b[?2048h";
/// Включение отчётов видимости (2033) и отчёт хоста о скрытом окне `CSI ? 999 ; 2 n`.
const VISIBILITY_ON: &[u8] = b"\x1b[?2033h";
const HIDDEN_REPORT: &str = "1b5b3f3939393b326e";
/// Ответ DECRQM `CSI ? 1004 ; 1 $ y`: режим фокуса установлен.
const FOCUS_MODE_SET: &str = "1b5b3f313030343b312479";

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    let candidate = target
        .backend
        .ok_or("queries идёт на кандидате, а не на стенде")?;
    let mut stand = Stand::for_candidate(&target.out, environment, candidate)?;
    let outcome = cells(&mut stand);
    let cleanup = stand.teardown();
    let mut report = outcome?;
    report.diagnostics.notes.extend(cleanup);
    Ok(report)
}

fn cells(stand: &mut Stand) -> Result<Report, String> {
    let mut report = Report::default();
    report.cells.extend(d1(stand)?);
    report.cells.extend(d2(stand)?);
    report.cells.push(d3(stand)?);
    report.cells.extend(d4(stand)?);
    report.cells.push(d5(stand)?);
    report.cells.extend(d6(stand)?);
    Ok(report)
}

// --- общие шаги ---

/// Ответы во вводе фикстуры с отметки журнала квитанций, без ответов на барьер.
fn responses(link: &Link, mark: usize) -> Vec<(ResponseKind, String)> {
    link.log()
        .iter()
        .skip(mark)
        .filter_map(|receipt| match &receipt.event {
            Event::Response { response, hex, .. } if hex != BARRIER_REPLY => {
                Some((*response, hex.clone()))
            }
            _ => None,
        })
        .collect()
}

fn barrier_replies(link: &Link) -> usize {
    link.log()
        .iter()
        .filter(
            |receipt| matches!(&receipt.event, Event::Response { hex, .. } if hex == BARRIER_REPLY),
        )
        .count()
}

/// Квитанции, пока условие над журналом не выполнено; `false` — срок вышел.
fn pull_until(
    link: &mut Link,
    timeout: Duration,
    mut done: impl FnMut(&Link) -> bool,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    while !done(link) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        match link.recv(left) {
            Ok(_) => {}
            Err(crate::fixture::channel::LinkError::Timeout) => return Ok(false),
            Err(err) => return Err(format!("квитанции фикстуры: {err}")),
        }
    }
    Ok(true)
}

fn write(link: &mut Link, bytes: &[u8]) -> Result<(), String> {
    let written = link
        .request(Command::Write { hex: hex(bytes) }, WAIT)
        .map_err(|err| format!("запись фикстуры: {err}"))?;
    if written != bytes.len() as u64 {
        return Err(format!(
            "фикстура записала {written} байт из {}",
            bytes.len()
        ));
    }
    Ok(())
}

/// Запрос целиком или префиксом `split`; номер команды нужен для остатка.
fn send_query(link: &mut Link, query: Query, split: Option<u32>) -> Result<u64, String> {
    let expected = split.map_or(query.bytes().len(), |at| at as usize);
    let id = link
        .send(Command::Query { query, split })
        .map_err(|err| format!("запрос: {err}"))?;
    let (bytes, error) = link
        .wait_for(WAIT, |receipt| match &receipt.event {
            Event::Ack {
                id: acked,
                bytes,
                error,
            } if *acked == id => Some((*bytes, error.clone())),
            _ => None,
        })
        .map_err(|err| format!("подтверждение запроса: {err}"))?;
    if bytes != expected as u64 || error.is_some() {
        return Err(format!(
            "запрос {query:?} разрез {split:?}: записано {bytes} из {expected}, {error:?}"
        ));
    }
    Ok(id)
}

/// Ответы на барьер в журнале приёмника клиента.
fn journal_barriers(client: &Client) -> Result<usize, String> {
    Ok(client.route()?.with(|rx| {
        rx.journal()
            .iter()
            .filter(
                |entry| matches!(&entry.event, Journaled::Reply { hex } if hex == BARRIER_REPLY),
            )
            .count()
    }))
}

/// Барьер при лидере и ведомых. Ввод ведомого управляемого клиента демон zmx отбрасывает целиком
/// (`handleInput` в `src/loop.zig` и патч `0001`), поэтому до фикстуры доходит только ответ лидера,
/// а то, что ведомый обработал все прежние байты и ответил, видно по журналу его приёмника. Затем
/// второй барьер через лидера: к его квитанции ответы ведомых уже записаны в их PTY раньше, чем
/// фикстура написала второй запрос. `Some` — порядок не доказан.
fn barrier_led(
    link: &mut Link,
    leader: &Client,
    followers: &[&Client],
) -> Result<Option<String>, String> {
    let before: Vec<usize> = followers
        .iter()
        .map(|client| journal_barriers(client))
        .collect::<Result<_, _>>()?;
    if let Some(reason) = barrier_n(link, 1)? {
        return Ok(Some(format!(
            "барьер через лидера {}: {reason}",
            leader.label
        )));
    }
    for (client, seen) in followers.iter().zip(before) {
        let answered = client.route()?.wait_until(WAIT, |rx, _| {
            rx.journal()
                .iter()
                .filter(|entry| {
                    matches!(&entry.event, Journaled::Reply { hex } if hex == BARRIER_REPLY)
                })
                .count()
                > seen
        });
        if let Err(err) = answered {
            return Ok(Some(format!(
                "приёмник ведомого {} не ответил на барьер: {err}",
                client.label
            )));
        }
    }
    Ok(barrier_n(link, 1)?
        .map(|reason| format!("второй барьер через лидера {}: {reason}", leader.label)))
}

/// Барьер через `clients` клиентов, ответы которых доходят до нагрузки: каждый приёмник ответил
/// на `CSI 5 n` после всех прежних байтов. `Some` — ответов меньше, порядок не доказан.
fn barrier_n(link: &mut Link, clients: usize) -> Result<Option<String>, String> {
    let before = barrier_replies(link);
    write(link, BARRIER)?;
    let done = pull_until(link, WAIT, |link| barrier_replies(link) >= before + clients)?;
    let got = barrier_replies(link) - before;
    Ok((!done).then(|| format!("барьер: ответов {got} из {clients}")))
}

/// Без клиента: контрольный DA, на который отвечает сам zmx; вид — тот, которого среди
/// проверяемых нет или меньше. `Some` — контрольного ответа нет, отсутствие не доказано.
fn settle_headless(
    link: &mut Link,
    mark: usize,
    tested: &[ResponseKind],
) -> Result<Option<String>, String> {
    let (query, kind) =
        if tested.contains(&ResponseKind::Da1) && !tested.contains(&ResponseKind::Da2) {
            (Query::Da2, ResponseKind::Da2)
        } else {
            (Query::Da1, ResponseKind::Da1)
        };
    let need = tested.iter().filter(|seen| **seen == kind).count() + 1;
    send_query(link, query, None)?;
    let count = |link: &Link| {
        responses(link, mark)
            .iter()
            .filter(|(seen, _)| *seen == kind)
            .count()
    };
    if pull_until(link, SETTLE, |link| count(link) >= need)? {
        Ok(None)
    } else {
        Ok(Some(format!(
            "без клиента контрольный {kind:?} не дал {need}-го ответа за {} с: отсутствие ответов \
             не доказано",
            SETTLE.as_secs()
        )))
    }
}

/// Ответы с отметки без последнего ответа вида `kind`: контрольного.
fn without_last(
    mut got: Vec<(ResponseKind, String)>,
    kind: ResponseKind,
) -> Vec<(ResponseKind, String)> {
    if let Some(at) = got.iter().rposition(|(seen, _)| *seen == kind) {
        got.remove(at);
    }
    got
}

fn settle_kind(tested: &[ResponseKind]) -> ResponseKind {
    if tested.contains(&ResponseKind::Da1) && !tested.contains(&ResponseKind::Da2) {
        ResponseKind::Da2
    } else {
        ResponseKind::Da1
    }
}

fn tally(
    evidence: &mut Evidence,
    name: &str,
    expected: &[ResponseKind],
    got: &[(ResponseKind, String)],
) -> bool {
    let kinds: Vec<ResponseKind> = got.iter().map(|(kind, _)| *kind).collect();
    let tally = Tally::new(expected, &kinds);
    evidence.note(&format!("{name}.responses"), got);
    evidence.note(&format!("{name}.tally"), &tally);
    evidence.check(name, tally.exact(), tally.describe())
}

fn wait_role(client: &Client, role: Role) -> Result<(), String> {
    client
        .route()?
        .wait_until(WAIT, |rx, _| {
            rx.last_role()
                .and_then(parse_role)
                .is_some_and(|report| report.role == role)
        })
        .map_err(|err| format!("клиент {} не получил роль {role:?}: {err}", client.label))
}

fn journal(client: &Client) -> Result<Vec<Journaled>, String> {
    Ok(client.route()?.with(|rx| {
        rx.journal()
            .iter()
            .map(|entry| entry.event.clone())
            .collect()
    }))
}

fn clipboard_writes(events: &[Journaled]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, Journaled::ClipboardWrite { .. }))
        .count()
}

/// Лидер c1 и барьер через него; `None` — кандидат отказал, отказ записан проверкой.
fn leader(
    stand: &Stand,
    session: &mut Session,
    evidence: &mut Evidence,
    label: &str,
) -> Result<Option<Client>, String> {
    let client = stand.client(&session.id, label)?;
    if evidence
        .require(&format!("{label} лидер"), client.wait_leader())
        .is_none()
    {
        return Ok(None);
    }
    let settled = barrier_n(&mut session.link, 1)?;
    if let Some(reason) = settled {
        evidence.check(&format!("барьер через {label}"), false, reason);
        return Ok(None);
    }
    Ok(Some(client))
}

fn follower(
    stand: &Stand,
    session: &mut Session,
    evidence: &mut Evidence,
    label: &str,
) -> Result<Option<Client>, String> {
    let client = stand.client_on(stand.main(), &session.id, label, Lead::IfFree, stand.size)?;
    if evidence
        .require(
            &format!("{label} ведомый"),
            wait_role(&client, Role::Follower),
        )
        .is_none()
    {
        return Ok(None);
    }
    Ok(Some(client))
}

/// Квитанция «не доказано» или проверка числа ответов.
fn settled_tally(
    evidence: &mut Evidence,
    name: &str,
    settled: Option<String>,
    expected: &[ResponseKind],
    got: &[(ResponseKind, String)],
) {
    match settled {
        None => {
            tally(evidence, name, expected, got);
        }
        Some(reason) => {
            evidence.note(&format!("{name}.responses"), got);
            evidence.not_proven = Some(format!("{name}: {reason}"));
        }
    }
}

// --- D1 ---

#[derive(Clone, Copy)]
enum D1Mode {
    Headless,
    One,
    Two,
    LeaderDrop,
    LeaderDropFollower,
}

impl D1Mode {
    const ALL: [D1Mode; 5] = [
        D1Mode::Headless,
        D1Mode::One,
        D1Mode::Two,
        D1Mode::LeaderDrop,
        D1Mode::LeaderDropFollower,
    ];

    fn part(self) -> &'static str {
        match self {
            D1Mode::Headless => "без-клиента",
            D1Mode::One => "один-клиент",
            D1Mode::Two => "два-клиента",
            D1Mode::LeaderDrop => "обрыв-лидера",
            D1Mode::LeaderDropFollower => "обрыв-лидера-при-ведомом",
        }
    }

    fn label(self) -> &'static str {
        match self {
            D1Mode::Headless => "d1h",
            D1Mode::One => "d1o",
            D1Mode::Two => "d1t",
            D1Mode::LeaderDrop => "d1l",
            D1Mode::LeaderDropFollower => "d1f",
        }
    }

    fn stimulus(self) -> &'static str {
        match self {
            D1Mode::Headless => "сессия ни разу без клиента",
            D1Mode::One => "один клиент-лидер c1",
            D1Mode::Two => "лидер c1 и ведомый c2 того же размера",
            D1Mode::LeaderDrop => {
                "лидер c1 остановлен SIGSTOP до запросов и убит SIGKILL после них, затем \
                 подключается c2"
            }
            D1Mode::LeaderDropFollower => {
                "лидер c1 и ведомый c2; c1 остановлен SIGSTOP до запросов и убит SIGKILL после них"
            }
        }
    }
}

fn d1(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let names: Vec<&str> = D1_QUERIES.iter().map(|(name, _)| *name).collect();
    let mut cells = Vec::new();
    for mode in D1Mode::ALL {
        let id = stand.session(mode.label())?;
        let mut session = stand.ensure(&id, Mode::Query)?;
        let mut evidence = Evidence::default();
        evidence.note("record", &session.record);
        d1_body(stand, &mut session, mode, &mut evidence)?;
        let artifact = stand.artifact(&format!("{}.json", mode.label()), &evidence)?;
        stand.discard(&id);
        cells.push(cell(
            Spec {
                id: "D1",
                part: Some(mode.part()),
                stimulus: format!(
                    "{}; фикстура query отправляет {}",
                    mode.stimulus(),
                    names.join(", ")
                ),
                oracle: "счётчик ответов фикстуры по видам; приёмники отвечают через PTY \
                         attach-клиента",
                completion: "барьер DSR через каждого живого клиента или, без клиента, \
                             контрольный DA после запросов",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact, stand.log_name(&id)],
        ));
    }
    Ok(cells)
}

fn send_d1(link: &mut Link) -> Result<(), String> {
    for (_, query) in D1_QUERIES {
        send_query(link, query, None)?;
    }
    Ok(())
}

fn d1_body(
    stand: &Stand,
    session: &mut Session,
    mode: D1Mode,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let expected: Vec<ResponseKind> = D1_QUERIES.iter().map(|(_, q)| q.response()).collect();
    match mode {
        D1Mode::Headless => {
            let mark = session.link.log().len();
            send_d1(&mut session.link)?;
            let settled = settle_headless(&mut session.link, mark, &expected)?;
            let got = without_last(responses(&session.link, mark), settle_kind(&expected));
            settled_tally(evidence, "ровно один ответ", settled, &expected, &got);
        }
        D1Mode::One => {
            let Some(_c1) = leader(stand, session, evidence, "c1")? else {
                return Ok(());
            };
            let mark = session.link.log().len();
            send_d1(&mut session.link)?;
            let settled = barrier_n(&mut session.link, 1)?;
            let got = responses(&session.link, mark);
            settled_tally(evidence, "ровно один ответ", settled, &expected, &got);
        }
        D1Mode::Two => {
            let Some(c1) = leader(stand, session, evidence, "c1")? else {
                return Ok(());
            };
            let Some(c2) = follower(stand, session, evidence, "c2")? else {
                return Ok(());
            };
            if let Some(reason) = barrier_led(&mut session.link, &c1, &[&c2])? {
                evidence.not_proven = Some(format!("барьер через двух клиентов: {reason}"));
                return Ok(());
            }
            let mark = session.link.log().len();
            send_d1(&mut session.link)?;
            let settled = barrier_led(&mut session.link, &c1, &[&c2])?;
            let got = responses(&session.link, mark);
            settled_tally(evidence, "ровно один ответ", settled, &expected, &got);
        }
        D1Mode::LeaderDrop => {
            let Some(mut c1) = leader(stand, session, evidence, "c1")? else {
                return Ok(());
            };
            stand
                .main()
                .stall_client(&c1.process, true)
                .map_err(|err| err.to_string())?;
            let mark = session.link.log().len();
            send_d1(&mut session.link)?;
            let exit = c1.kill(stand.main())?;
            evidence.note("c1.exit", format!("{exit:?}"));
            drop(c1);
            let Some(_c2) = leader(stand, session, evidence, "c2")? else {
                return Ok(());
            };
            let settled = barrier_n(&mut session.link, 1)?;
            let got = responses(&session.link, mark);
            settled_tally(evidence, "ровно один ответ", settled, &expected, &got);
        }
        D1Mode::LeaderDropFollower => {
            let Some(mut c1) = leader(stand, session, evidence, "c1")? else {
                return Ok(());
            };
            let Some(c2) = follower(stand, session, evidence, "c2")? else {
                return Ok(());
            };
            if let Some(reason) = barrier_led(&mut session.link, &c1, &[&c2])? {
                evidence.not_proven = Some(format!("барьер через двух клиентов: {reason}"));
                return Ok(());
            }
            stand
                .main()
                .stall_client(&c1.process, true)
                .map_err(|err| err.to_string())?;
            let mark = session.link.log().len();
            send_d1(&mut session.link)?;
            let exit = c1.kill(stand.main())?;
            evidence.note("c1.exit", format!("{exit:?}"));
            drop(c1);
            // Ведомый управляемый клиент лидером сам не становится, а его ввод демон отбрасывает:
            // упорядочивающее событие — барьер через нового клиента, который ведёт ничейную сессию.
            let Some(_c3) = leader(stand, session, evidence, "c3")? else {
                return Ok(());
            };
            let settled = barrier_n(&mut session.link, 1)?;
            let got = responses(&session.link, mark);
            settled_tally(evidence, "ровно один ответ", settled, &expected, &got);
        }
    }
    Ok(())
}

// --- D2 ---

fn d2(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    for (mode, with_client) in [("без-клиента", false), ("один-клиент", true)] {
        let label = if with_client { "d2c" } else { "d2h" };
        let id = stand.session(label)?;
        let mut session = stand.ensure(&id, Mode::Query)?;
        let mut setup = Evidence::default();
        let client = if with_client {
            leader(stand, &mut session, &mut setup, "c1")?
        } else {
            None
        };
        let mut artifact = Map::new();
        artifact.insert("record".to_owned(), json!(session.record));
        let mut parts = Vec::new();
        if with_client && client.is_none() {
            artifact.insert("setup".to_owned(), json!(setup));
            parts.push((format!("{mode}-подготовка"), String::new(), setup));
        } else {
            for (name, query) in D2_QUERIES {
                for cut in cuts(&query.bytes()) {
                    let mut evidence = Evidence::default();
                    d2_cut(
                        stand,
                        &mut session,
                        query,
                        cut.at,
                        with_client,
                        &mut evidence,
                    )?;
                    let part = format!("{mode}-{name}-{}", cut.at);
                    artifact.insert(part.clone(), json!(evidence));
                    parts.push((
                        part,
                        format!(
                            "запрос {name} {} разрезан после {} байт",
                            hex(&query.bytes()),
                            cut.at
                        ),
                        evidence,
                    ));
                }
            }
        }
        drop(client);
        let file = stand.artifact(&format!("{label}.json"), &Value::Object(artifact))?;
        stand.discard(&id);
        for (part, what, evidence) in parts {
            cells.push(cell(
                Spec {
                    id: "D2",
                    part: Some(&part),
                    stimulus: format!(
                        "{}; префикс, {}, остаток",
                        if what.is_empty() {
                            mode.to_owned()
                        } else {
                            what
                        },
                        match stand.candidate {
                            Candidate::Zmx => "рост ревизии обработки ровно на единицу",
                            Candidate::Rust => "разбор ровно префикса одним куском",
                        }
                    ),
                    oracle: match stand.candidate {
                        Candidate::Zmx => {
                            "счётчик ответов фикстуры; граница — по протоколу разреза через \
                             ревизию zmx screen"
                        }
                        Candidate::Rust => {
                            "счётчик ответов фикстуры; граница — по протоколу разреза через \
                             событие разбора наблюдателя Rust-хоста"
                        }
                    },
                    completion: "барьер DSR через клиента или контрольный DA без клиента",
                    origin: DefectOrigin::Host,
                },
                &evidence,
                vec![file.clone(), stand.log_name(&id)],
            ));
        }
    }
    Ok(cells)
}

/// Ревизия обработки после префикса: первая отличная от `before` или причина, почему её нет.
fn next_revision(stand: &Stand, id: &SessionId, before: u64) -> Result<u64, String> {
    let deadline = Instant::now() + SETTLE;
    loop {
        let revision = stand
            .zmx
            .read_boundary(id)
            .map_err(|err| format!("ревизия: {err}"))?;
        if revision != before {
            return Ok(revision);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "ревизия {before} не изменилась за {} с",
                SETTLE.as_secs()
            ));
        }
    }
}

/// Начало разреза: ревизия обработки zmx или точка покоя разбора Rust-хоста.
enum CutStart {
    Revision(u64),
    Parsed(ParsedCut),
}

fn d2_cut(
    stand: &Stand,
    session: &mut Session,
    query: Query,
    at: usize,
    with_client: bool,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let expected = [query.response()];
    let mark = session.link.log().len();
    let start = match stand.candidate {
        Candidate::Zmx => {
            let Some(before) = evidence.require(
                "ревизия до префикса",
                stand
                    .zmx
                    .read_boundary(&session.id)
                    .map_err(|err| err.to_string()),
            ) else {
                return Ok(());
            };
            CutStart::Revision(before)
        }
        Candidate::Rust => CutStart::Parsed(stand.parsed_cut(&session.id)?),
    };
    let request = send_query(&mut session.link, query, Some(at as u32))?;
    let reached = match &start {
        CutStart::Revision(before) => next_revision(stand, &session.id, *before).inspect(|after| {
            evidence.note("revision", json!({ "before": before, "after": after }));
        }),
        CutStart::Parsed(cut) => {
            let (note, verdict) = cut.end(at, &[]);
            evidence.note("parsed", note);
            verdict.map(|()| 0)
        }
    };
    let rest = session
        .link
        .request(Command::Rest { query: request }, WAIT)
        .map_err(|err| format!("остаток запроса: {err}"))?;
    if rest != (query.bytes().len() - at) as u64 {
        return Err(format!("остаток запроса: записано {rest} байт"));
    }
    let violations: Vec<String> = session
        .link
        .log()
        .iter()
        .skip(mark)
        .filter_map(|receipt| match &receipt.event {
            Event::Winch { size } => Some(format!("SIGWINCH {size:?} во время разреза")),
            _ => None,
        })
        .collect();
    let boundary = reached.and_then(|after| match &start {
        CutStart::Revision(before) => revision_boundary(*before, after, &violations),
        CutStart::Parsed(_) if violations.is_empty() => Ok(()),
        CutStart::Parsed(_) => Err(format!(
            "предусловие разреза нарушено: {}",
            violations.join("; ")
        )),
    });
    let settled = if with_client {
        barrier_n(&mut session.link, 1)?
    } else {
        settle_headless(&mut session.link, mark, &expected)?
    };
    let mut got = responses(&session.link, mark);
    if !with_client {
        got = without_last(got, settle_kind(&expected));
    }
    match boundary {
        Ok(()) => settled_tally(evidence, "ровно один ответ", settled, &expected, &got),
        Err(reason) => {
            evidence.note("responses", &got);
            evidence.not_proven = Some(format!("граница разреза не доказана: {reason}"));
        }
    }
    Ok(())
}

// --- D3 ---

fn d3(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("d3")?;
    let mut session = stand.ensure(&id, Mode::Query)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    d3_body(stand, &mut session, &mut evidence)?;
    let artifact = stand.artifact("d3.json", &evidence)?;
    stand.discard(&id);
    Ok(cell(
        Spec {
            id: "D3",
            part: None,
            stimulus: "при живом лидере c1 фикстура пишет OSC 52 записи; c1 отключается \
                       клавишей, подключается c2 (повторный attach со снимком); затем без \
                       клиента ещё одна запись OSC 52 и подключение c3"
                .to_owned(),
            oracle: "журнал эффектов приёмника каждого клиента",
            completion: "барьер DSR через каждого клиента после записи и после подключения",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ))
}

fn d3_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let Some(mut c1) = leader(stand, session, evidence, "c1")? else {
        return Ok(());
    };
    write(&mut session.link, OSC52_WRITE)?;
    if let Some(reason) = barrier_n(&mut session.link, 1)? {
        evidence.check("барьер через c1", false, reason);
        return Ok(());
    }
    let first = journal(&c1)?;
    evidence.note("c1.journal", &first);
    evidence.check(
        "эффект один раз при живом клиенте",
        clipboard_writes(&first) == 1,
        format!("записей буфера обмена у c1: {}", clipboard_writes(&first)),
    );
    c1.input(DETACH_KEY)?;
    if evidence
        .require("c1 отключился клавишей", c1.wait_exit())
        .is_none()
    {
        return Ok(());
    }
    let Some(mut c2) = leader(stand, session, evidence, "c2")? else {
        return Ok(());
    };
    let second = journal(&c2)?;
    evidence.note("c2.journal", &second);
    evidence.check(
        "нет повтора при новом подключении",
        clipboard_writes(&second) == 0,
        format!("записей буфера обмена у c2: {}", clipboard_writes(&second)),
    );
    c2.input(DETACH_KEY)?;
    if evidence
        .require("c2 отключился клавишей", c2.wait_exit())
        .is_none()
    {
        return Ok(());
    }
    write(&mut session.link, OSC52_WRITE)?;
    let Some(c3) = leader(stand, session, evidence, "c3")? else {
        return Ok(());
    };
    let third = journal(&c3)?;
    evidence.note("c3.journal", &third);
    evidence.note(
        "запись без клиента доставлена при подключении",
        clipboard_writes(&third),
    );
    evidence.check(
        "запись без клиента не повторяется больше одного раза",
        clipboard_writes(&third) <= 1,
        format!("записей буфера обмена у c3: {}", clipboard_writes(&third)),
    );
    Ok(())
}

// --- D4 ---

/// Части D4: метка сессии, имя части и стимул у zmx и у Rust-хоста.
const D4_PARTS: [(&str, &str, &str, &str); 3] = [
    (
        "d4c",
        "клиент",
        "лидер c1 отвечает на чтение OSC 52 из профиля приёмника; затем c1 отключается и \
         подключается c2",
        "лидер c1 и GUI его поколения, GUI сразу отвечает на чтение OSC 52; затем c1 \
         отключается и подключается c2",
    ),
    (
        "d4h",
        "без-gui",
        "сессия без клиента: чтение OSC 52 без GUI",
        "сессия без клиента и без GUI: чтение OSC 52",
    ),
    (
        "d4l",
        "поздний-ответ",
        "лидер c1 остановлен SIGSTOP, фикстура читает OSC 52, c2 забирает лидерство, c1 \
         продолжает и отвечает поздно",
        "лидер c1 и GUI его поколения, который молчит; фикстура читает OSC 52, c2 забирает \
         лидерство, GUI c1 отвечает поздно от имени прежнего поколения",
    ),
];

fn d4(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    let mut late_failed = None;
    for (label, part, zmx_stimulus, rust_stimulus) in D4_PARTS {
        let id = stand.session(label)?;
        let mut session = stand.ensure(&id, Mode::Query)?;
        let mut evidence = Evidence::default();
        let mut generation = Evidence::default();
        evidence.note("record", &session.record);
        match label {
            "d4c" => d4_client(stand, &mut session, &mut evidence)?,
            "d4h" => d4_headless(stand, &mut session, &mut evidence)?,
            _ => match stand.candidate {
                Candidate::Zmx => d4_late(stand, &mut session, &mut evidence)?,
                Candidate::Rust => {
                    d4_late_rust(stand, &mut session, &mut evidence, &mut generation)?
                }
            },
        }
        if label == "d4l" {
            late_failed = Some(evidence.status().0 == crate::results::Status::Fail);
        }
        let artifact = stand.artifact(&format!("{label}.json"), &evidence)?;
        let generation_artifact = if label == "d4l" && stand.candidate == Candidate::Rust {
            Some(stand.artifact("d4g.json", &generation)?)
        } else {
            None
        };
        stand.discard(&id);
        cells.push(cell(
            Spec {
                id: "D4",
                part: Some(part),
                stimulus: match stand.candidate {
                    Candidate::Zmx => zmx_stimulus,
                    Candidate::Rust => rust_stimulus,
                }
                .to_owned(),
                oracle: "счётчик ответов фикстуры вида clipboard",
                completion: "барьер DSR через живых клиентов или контрольный DA без клиента",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact.clone(), stand.log_name(&id)],
        ));
        if label != "d4l" {
            continue;
        }
        let (stimulus, oracle, artifacts) = match generation_artifact {
            None => {
                if late_failed == Some(true) {
                    generation.unsupported = Some(
                        "ответ GUI на чтение буфера обмена идёт в zmx обычным вводом клиента: у \
                         него нет поколения и тайм-аута, поздний ответ прежнего лидера доходит \
                         до нагрузки (часть поздний-ответ)"
                            .to_owned(),
                    );
                } else {
                    generation.not_proven = Some(
                        "поздний ответ не дошёл до нагрузки: отсутствие поколения не доказано"
                            .to_owned(),
                    );
                }
                (
                    "по части поздний-ответ",
                    "счётчик ответов фикстуры вида clipboard",
                    vec![artifact],
                )
            }
            Some(file) => (
                "по части поздний-ответ; затем GUI нового лидера c2 молчит на чтение OSC 52 до \
                 срока хоста и отвечает после него",
                "ответы фикстуры вида clipboard и их данные, счётчики чтения буфера обмена в \
                 состоянии хоста",
                vec![file, artifact, stand.log_name(&id)],
            ),
        };
        cells.push(cell(
            Spec {
                id: "D4",
                part: Some("поколение-и-тайм-аут"),
                stimulus: stimulus.to_owned(),
                oracle,
                completion: "часть поздний-ответ закончена, барьер DSR через c2 после ответа по \
                             сроку",
                origin: DefectOrigin::Host,
            },
            &generation,
            artifacts,
        ));
    }
    Ok(cells)
}

/// Данные ответов OSC 52 фикстуре.
fn clipboard_payloads(got: &[(ResponseKind, String)]) -> Vec<Option<String>> {
    got.iter()
        .filter(|(kind, _)| *kind == ResponseKind::Clipboard)
        .map(|(_, raw)| osc52_payload(raw))
        .collect()
}

/// GUI лидера `client` на Rust-хосте; `auto` — данные мгновенного ответа. `None` у zmx или при
/// отказе, записанном проверкой.
fn bind_gui(
    stand: &Stand,
    session: &Session,
    client: &Client,
    auto: Option<&[u8]>,
    evidence: &mut Evidence,
) -> Result<Option<crate::backend::rust::Gui>, String> {
    let Some(rust) = &stand.rust else {
        return Ok(None);
    };
    let generation = generation(client)?;
    Ok(evidence.require(
        &format!("GUI поколения {generation} лидера {}", client.label),
        rust.gui(&session.id, generation, auto.map(<[u8]>::to_vec))
            .map_err(|err| err.to_string()),
    ))
}

fn d4_client(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let Some(mut c1) = leader(stand, session, evidence, "c1")? else {
        return Ok(());
    };
    let gui = bind_gui(stand, session, &c1, Some(GUI_CLIPBOARD), evidence)?;
    if stand.rust.is_some() && gui.is_none() {
        return Ok(());
    }
    let mark = session.link.log().len();
    send_query(&mut session.link, Query::ClipboardRead, None)?;
    let settled = barrier_n(&mut session.link, 1)?;
    let got = responses(&session.link, mark);
    settled_tally(
        evidence,
        "один ответ при живом клиенте",
        settled,
        &[ResponseKind::Clipboard],
        &got,
    );
    if let Some(gui) = &gui {
        let payloads = clipboard_payloads(&got);
        evidence.note("gui.asks", gui.asks());
        evidence.check(
            "данные GUI лидера дошли до нагрузки",
            payloads == [Some(base64(GUI_CLIPBOARD))],
            format!("данные ответов: {payloads:?}"),
        );
    }
    c1.input(DETACH_KEY)?;
    if evidence
        .require("c1 отключился клавишей", c1.wait_exit())
        .is_none()
    {
        return Ok(());
    }
    let Some(_c2) = leader(stand, session, evidence, "c2")? else {
        return Ok(());
    };
    let got = responses(&session.link, mark);
    tally(
        evidence,
        "нет повторного ответа при новом подключении",
        &[ResponseKind::Clipboard],
        &got,
    );
    Ok(())
}

fn d4_headless(
    stand: &Stand,
    session: &mut Session,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let mark = session.link.log().len();
    send_query(&mut session.link, Query::ClipboardRead, None)?;
    let expected = [ResponseKind::Clipboard];
    let settled = settle_headless(&mut session.link, mark, &expected)?;
    let got = without_last(responses(&session.link, mark), settle_kind(&expected));
    settled_tally(evidence, "явный ответ без GUI", settled, &expected, &got);
    if let Some(rust) = &stand.rust {
        let status = rust
            .status(&session.id)
            .map_err(|err| format!("состояние хоста: {err}"))?;
        evidence.note("clipboard", status.clipboard);
        evidence.note("payloads", clipboard_payloads(&got));
    }
    Ok(())
}

fn d4_late(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let Some(c1) = leader(stand, session, evidence, "c1")? else {
        return Ok(());
    };
    stand
        .zmx
        .stall_client(&c1.process, true)
        .map_err(|err| err.to_string())?;
    let mark = session.link.log().len();
    send_query(&mut session.link, Query::ClipboardRead, None)?;
    let c2 = stand.client_on(stand.main(), &session.id, "c2", Lead::Claim, stand.size)?;
    if evidence
        .require("c2 забрал лидерство", c2.wait_leader())
        .is_none()
    {
        let _ = stand.zmx.stall_client(&c1.process, false);
        return Ok(());
    }
    stand
        .zmx
        .stall_client(&c1.process, false)
        .map_err(|err| err.to_string())?;
    let settled = barrier_led(&mut session.link, &c2, &[&c1])?;
    let got = responses(&session.link, mark);
    evidence.note("c1.journal", journal(&c1)?);
    evidence.note("c2.journal", journal(&c2)?);
    match settled {
        None => {
            let late = got
                .iter()
                .filter(|(kind, _)| *kind == ResponseKind::Clipboard)
                .count();
            evidence.note("responses", &got);
            evidence.check(
                "поздний ответ прежнего лидера отброшен",
                late == 0,
                format!("ответов clipboard после смены лидера: {late}"),
            );
        }
        Some(reason) => {
            evidence.note("responses", &got);
            evidence.not_proven = Some(reason);
        }
    }
    Ok(())
}

/// Поздний ответ на Rust-хосте: ожидание отменяет смена лидера, хост отвечает нагрузке пусто, а
/// ответ GUI прежнего поколения отбрасывается. Затем часть «поколение-и-тайм-аут»: GUI нового
/// лидера молчит до срока хоста.
fn d4_late_rust(
    stand: &Stand,
    session: &mut Session,
    evidence: &mut Evidence,
    generation_evidence: &mut Evidence,
) -> Result<(), String> {
    let rust = stand.rust()?;
    let Some(c1) = leader(stand, session, evidence, "c1")? else {
        return Ok(());
    };
    let Some(gui1) = bind_gui(stand, session, &c1, None, evidence)? else {
        return Ok(());
    };
    let late = Some(base64(LATE_CLIPBOARD));
    let mark = session.link.log().len();
    send_query(&mut session.link, Query::ClipboardRead, None)?;
    let Some(ask) = evidence.require(
        "чтение дошло до GUI c1",
        gui1.wait_ask(1, WAIT).map_err(|err| err.to_string()),
    ) else {
        return Ok(());
    };
    evidence.note("c1.ask", ask);
    let c2 = stand.client_on(stand.main(), &session.id, "c2", Lead::Claim, stand.size)?;
    if evidence
        .require("c2 забрал лидерство", c2.wait_leader())
        .is_none()
    {
        return Ok(());
    }
    gui1.answer(ask, ask.generation, Some(LATE_CLIPBOARD.to_vec()))
        .map_err(|err| err.to_string())?;
    let settled = barrier_led(&mut session.link, &c2, &[&c1])?;
    let got = responses(&session.link, mark);
    let status = rust
        .status(&session.id)
        .map_err(|err| format!("состояние хоста: {err}"))?;
    let payloads = clipboard_payloads(&got);
    evidence.note("responses", &got);
    evidence.note("clipboard", status.clipboard);
    evidence.note("c1.journal", journal(&c1)?);
    evidence.note("c2.journal", journal(&c2)?);
    if let Some(reason) = settled {
        evidence.not_proven = Some(reason);
        return Ok(());
    }
    let dropped = !payloads.contains(&late) && status.clipboard.dropped >= 1;
    evidence.check(
        "поздний ответ прежнего лидера отброшен",
        dropped,
        format!(
            "данные ответов {payloads:?}, отброшено хостом {}",
            status.clipboard.dropped
        ),
    );
    evidence.check(
        "ожидание закончилось одним пустым ответом",
        payloads == [Some(String::new())],
        format!(
            "данные ответов {payloads:?}; отменено {}, по сроку {}",
            status.clipboard.cancelled, status.clipboard.timeouts
        ),
    );
    generation_evidence.check(
        "ответ прежнего поколения отброшен (часть поздний-ответ)",
        dropped,
        format!("отброшено хостом {}", status.clipboard.dropped),
    );
    d4_timeout(stand, session, &c2, generation_evidence)
}

fn d4_timeout(
    stand: &Stand,
    session: &mut Session,
    c2: &Client,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let rust = stand.rust()?;
    let Some(gui2) = bind_gui(stand, session, c2, None, evidence)? else {
        return Ok(());
    };
    let id = session.id.clone();
    let status = || {
        rust.status(&id)
            .map(|status| status.clipboard)
            .map_err(|err| format!("состояние хоста: {err}"))
    };
    let before = status()?;
    let mark = session.link.log().len();
    let started = Instant::now();
    send_query(&mut session.link, Query::ClipboardRead, None)?;
    let Some(ask) = evidence.require(
        "чтение дошло до GUI c2",
        gui2.wait_ask(1, WAIT).map_err(|err| err.to_string()),
    ) else {
        return Ok(());
    };
    let answered = pull_until(&mut session.link, WAIT, |link| {
        count(&responses(link, mark), ResponseKind::Clipboard) >= 1
    })?;
    let waited = started.elapsed();
    gui2.answer(ask, ask.generation, Some(LATE_CLIPBOARD.to_vec()))
        .map_err(|err| err.to_string())?;
    let settled = barrier_n(&mut session.link, 1)?;
    let got = responses(&session.link, mark);
    let after = status()?;
    let payloads = clipboard_payloads(&got);
    evidence.note("timeout.ask", ask);
    evidence.note("timeout.waited_ms", waited.as_millis() as u64);
    evidence.note("timeout.responses", &got);
    evidence.note("timeout.clipboard_before", before);
    evidence.note("timeout.clipboard_after", after);
    if let Some(reason) = settled {
        evidence.not_proven = Some(reason);
        return Ok(());
    }
    evidence.check(
        "без ответа GUI хост отвечает пусто по сроку",
        answered && payloads == [Some(String::new())] && after.timeouts == before.timeouts + 1,
        format!(
            "данные ответов {payloads:?} через {} мс, по сроку {} → {}",
            waited.as_millis(),
            before.timeouts,
            after.timeouts
        ),
    );
    evidence.check(
        "ответ GUI после срока отброшен",
        after.dropped == before.dropped + 1 && !payloads.contains(&Some(base64(LATE_CLIPBOARD))),
        format!("отброшено {} → {}", before.dropped, after.dropped),
    );
    Ok(())
}

// --- D5 ---

fn count(got: &[(ResponseKind, String)], kind: ResponseKind) -> usize {
    got.iter().filter(|(seen, _)| *seen == kind).count()
}

fn d5(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("d5")?;
    let mut session = stand.ensure(&id, Mode::Query)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    d5_body(stand, &mut session, &mut evidence)?;
    let artifact = stand.artifact("d5.json", &evidence)?;
    stand.discard(&id);
    Ok(cell(
        Spec {
            id: "D5",
            part: None,
            stimulus: match stand.candidate {
                Candidate::Zmx => {
                    "при лидере c1 фикстура включает 1004 и 2048, окно c1 получает фокус; c1 \
                     отключается клавишей; подключается c2 того же размера и получает фокус"
                }
                Candidate::Rust => {
                    "при лидере c1 фикстура включает 1004 и 2048, окно c1 получает фокус \
                     (приёмник c1 и сообщение GUI поколения c1); фикстура спрашивает DECRQM 1004; \
                     фикстура включает 2033, GUI c1 \
                     сообщает скрытие окна; c1 отключается клавишей; подключается c2 того же \
                     размера и получает фокус тем же путём; GUI прежнего поколения сообщает \
                     потерю фокуса"
                }
            }
            .to_owned(),
            oracle: "счётчик отчётов фокуса, размера и видимости фикстуры по событиям; квитанции \
                     SIGWINCH; отказ хоста GUI прежнего поколения",
            completion: "барьер DSR через живого клиента после каждого события",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ))
}

fn d5_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let size = format!("48;{};{};", stand.size.rows, stand.size.cols);
    let size_hex = hex(size.as_bytes());
    let Some(mut c1) = leader(stand, session, evidence, "c1")? else {
        return Ok(());
    };
    let gui1 = bind_gui(stand, session, &c1, None, evidence)?;
    if stand.rust.is_some() && gui1.is_none() {
        return Ok(());
    }
    let mark = session.link.log().len();
    write(&mut session.link, FOCUS_SIZE_ON)?;
    let settled = barrier_n(&mut session.link, 1)?;
    let got = responses(&session.link, mark);
    evidence.note("enable.responses", &got);
    if let Some(reason) = settled {
        evidence.not_proven = Some(reason);
        return Ok(());
    }
    evidence.check(
        "включение 2048: один отчёт размера",
        count(&got, ResponseKind::SizeReport) == 1,
        format!("{:?}", got),
    );
    let mark = session.link.log().len();
    c1.route()?.focus(true)?;
    if let Some(gui) = &gui1
        && !gui_accepted(evidence, "фокус через GUI c1", gui.focus(true))
    {
        return Ok(());
    }
    let settled = barrier_n(&mut session.link, 1)?;
    let got = responses(&session.link, mark);
    evidence.note("focus.responses", &got);
    if let Some(reason) = settled {
        evidence.not_proven = Some(reason);
        return Ok(());
    }
    evidence.check(
        "фокус окна c1: один FocusIn",
        count(&got, ResponseKind::FocusIn) == 1 && got.len() == 1,
        format!("{:?}", got),
    );
    if let Some(gui) = &gui1
        && !(d5_mode_query(session, evidence)? && d5_visibility(session, gui, evidence)?)
    {
        return Ok(());
    }
    let mark = session.link.log().len();
    c1.input(DETACH_KEY)?;
    if evidence
        .require("c1 отключился клавишей", c1.wait_exit())
        .is_none()
    {
        return Ok(());
    }
    let c2 = stand.client(&session.id, "c2")?;
    if evidence.require("c2 лидер", c2.wait_leader()).is_none() {
        return Ok(());
    }
    let gui2 = bind_gui(stand, session, &c2, None, evidence)?;
    c2.route()?.focus(true)?;
    if let (Some(gui1), Some(gui2)) = (&gui1, &gui2) {
        if !gui_accepted(evidence, "фокус через GUI c2", gui2.focus(true)) {
            return Ok(());
        }
        let stale = gui1.focus(false).map_err(|err| err.to_string())?;
        evidence.check(
            "GUI прежнего поколения отвергнут",
            stale.is_some(),
            format!("ответ хоста на потерю фокуса от GUI c1: {stale:?}"),
        );
    } else if stand.rust.is_some() {
        return Ok(());
    }
    let settled = barrier_n(&mut session.link, 1)?;
    let got = responses(&session.link, mark);
    let winches = session
        .link
        .log()
        .iter()
        .skip(mark)
        .filter(|receipt| matches!(receipt.event, Event::Winch { .. }))
        .count();
    evidence.note("reattach.responses", &got);
    evidence.note("reattach.sigwinch", winches);
    if let Some(reason) = settled {
        evidence.not_proven = Some(reason);
        return Ok(());
    }
    // Отключение клиента — не потеря фокуса окна, поэтому FocusOut не обязателен; обязательно
    // отсутствие повторов.
    let focus_out = count(&got, ResponseKind::FocusOut);
    evidence.note("detach.focus_out", focus_out);
    evidence.check(
        "отключение c1: FocusOut не повторяется",
        focus_out <= 1,
        format!("FocusOut: {focus_out}"),
    );
    let sizes: Vec<&String> = got
        .iter()
        .filter(|(kind, _)| *kind == ResponseKind::SizeReport)
        .map(|(_, raw)| raw)
        .collect();
    evidence.check(
        "подключение c2: не больше одного отчёта размера и с размером окна",
        sizes.len() <= 1 && sizes.iter().all(|raw| raw.contains(&size_hex)),
        format!("отчёты размера: {sizes:?}"),
    );
    evidence.check(
        "фокус окна c2: один FocusIn",
        count(&got, ResponseKind::FocusIn) == 1,
        format!("FocusIn: {}", count(&got, ResponseKind::FocusIn)),
    );
    Ok(())
}

/// Сообщение GUI принято хостом; отказ или ошибка записаны проверкой.
fn gui_accepted(
    evidence: &mut Evidence,
    name: &str,
    reply: crate::backend::Result<Option<String>>,
) -> bool {
    match reply {
        Ok(None) => evidence.check(name, true, "принято"),
        Ok(Some(reason)) => evidence.check(name, false, format!("хост отказал: {reason}")),
        Err(err) => evidence.check(name, false, err.to_string()),
    }
}

/// DECRQM режима отчётов на Rust-хосте: режим ведёт хост, отвечает он один и по своему
/// состоянию, хотя клиенту режим не передавался.
fn d5_mode_query(session: &mut Session, evidence: &mut Evidence) -> Result<bool, String> {
    let mark = session.link.log().len();
    send_query(&mut session.link, Query::ModeDec { mode: 1004 }, None)?;
    let settled = barrier_n(&mut session.link, 1)?;
    let got = responses(&session.link, mark);
    evidence.note("decrqm.responses", &got);
    if let Some(reason) = settled {
        evidence.not_proven = Some(reason);
        return Ok(false);
    }
    evidence.check(
        "DECRQM 1004: один ответ хоста «установлен»",
        got.len() == 1 && got[0].1 == FOCUS_MODE_SET,
        format!("{got:?}"),
    );
    Ok(true)
}

/// Видимость окна на Rust-хосте: фикстура включает 2033, GUI лидера сообщает скрытие, нагрузка
/// получает ровно один отчёт хоста. `false` — дальше идти нельзя, причина записана.
fn d5_visibility(
    session: &mut Session,
    gui: &crate::backend::rust::Gui,
    evidence: &mut Evidence,
) -> Result<bool, String> {
    let mark = session.link.log().len();
    write(&mut session.link, VISIBILITY_ON)?;
    let settled = barrier_n(&mut session.link, 1)?;
    evidence.note(
        "visibility.enable.responses",
        responses(&session.link, mark),
    );
    if let Some(reason) = settled {
        evidence.not_proven = Some(reason);
        return Ok(false);
    }
    let mark = session.link.log().len();
    if !gui_accepted(evidence, "скрытие окна через GUI c1", gui.visible(false)) {
        return Ok(false);
    }
    let settled = barrier_n(&mut session.link, 1)?;
    let got = responses(&session.link, mark);
    evidence.note("visibility.responses", &got);
    if let Some(reason) = settled {
        evidence.not_proven = Some(reason);
        return Ok(false);
    }
    evidence.check(
        "скрытие окна c1: один отчёт видимости",
        got.len() == 1 && got[0].1 == HIDDEN_REPORT,
        format!("{got:?}"),
    );
    Ok(true)
}

// --- D6 ---

fn d6(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    for (label, part, stimulus, stall) in [
        ("d6h", "без-клиента", "сессия ни разу без клиента", false),
        (
            "d6s",
            "зависший-клиент",
            "лидер c1 остановлен SIGSTOP на время операций",
            true,
        ),
    ] {
        let id = stand.session(label)?;
        let mut session = stand.ensure(&id, Mode::Query)?;
        let mut evidence = Evidence::default();
        evidence.note("record", &session.record);
        d6_body(stand, &mut session, stall, &mut evidence)?;
        let artifact = stand.artifact(&format!("{label}.json"), &evidence)?;
        stand.discard(&id);
        cells.push(cell(
            Spec {
                id: "D6",
                part: Some(part),
                stimulus: match stand.candidate {
                    Candidate::Zmx => format!(
                        "{stimulus}; snapshot (zmx screen), list, input raw (zmx send) и input \
                         typed (zmx type)"
                    ),
                    Candidate::Rust => format!(
                        "{stimulus}; snapshot, list, input raw и input typed управляющими \
                         соединениями хоста"
                    ),
                },
                oracle: "время каждой операции против предела, квитанции прочитанного ввода",
                completion: "каждая операция вернулась или вышел срок команды кандидата",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact, stand.log_name(&id)],
        ));
    }
    Ok(cells)
}

fn timed<T, E: std::fmt::Display>(
    evidence: &mut Evidence,
    name: &str,
    op: impl FnOnce() -> Result<T, E>,
) -> Option<T> {
    let started = Instant::now();
    let result = op();
    let elapsed = started.elapsed();
    evidence.note(&format!("{name}.elapsed_ms"), elapsed.as_millis() as u64);
    let value = evidence.require(name, result)?;
    evidence.check(
        &format!("{name} не блокируется"),
        elapsed < OP_LIMIT,
        format!(
            "{} мс, предел {} мс",
            elapsed.as_millis(),
            OP_LIMIT.as_millis()
        ),
    );
    Some(value)
}

fn read_bytes(link: &Link, mark: usize) -> Vec<u8> {
    link.log()
        .iter()
        .skip(mark)
        .filter_map(|receipt| match &receipt.event {
            Event::Read { hex, .. } => crate::fixture::channel::unhex(hex),
            _ => None,
        })
        .flatten()
        .collect()
}

fn d6_body(
    stand: &Stand,
    session: &mut Session,
    stall: bool,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let client = if stall {
        let Some(client) = leader(stand, session, evidence, "c1")? else {
            return Ok(());
        };
        stand
            .main()
            .stall_client(&client.process, true)
            .map_err(|err| err.to_string())?;
        Some(client)
    } else {
        None
    };
    let id = session.id.clone();
    let mark = session.link.log().len();
    let backend = stand.main();
    if let Some(snapshot) = timed(evidence, "snapshot", || backend.snapshot(&id)) {
        evidence.note("snapshot.revision", snapshot.revision);
    }
    if let Some(listed) = timed(evidence, "list", || backend.list()) {
        evidence.note("list", &listed);
    }
    if let Some(outcome) = timed(evidence, "input raw", || {
        backend.input(&id, b"x", InputPath::Raw)
    }) {
        evidence.note("input_raw", &outcome);
    }
    if let Some(outcome) = timed(evidence, "input typed", || {
        backend.input(&id, b"y", InputPath::Typed)
    }) {
        evidence.note("input_typed", &outcome);
    }
    let arrived = pull_until(&mut session.link, WAIT, |link| {
        let bytes = read_bytes(link, mark);
        bytes.contains(&b'x') && bytes.contains(&b'y')
    })?;
    let bytes = read_bytes(&session.link, mark);
    evidence.check(
        "ввод дошёл до нагрузки",
        arrived,
        format!("прочитано: {}", hex(&bytes)),
    );
    if let Some(client) = client {
        let _ = stand.main().stall_client(&client.process, false);
    }
    Ok(())
}
