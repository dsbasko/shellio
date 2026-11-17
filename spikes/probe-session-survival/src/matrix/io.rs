//! Рубеж E — ввод-вывод, очереди и публикация сокета (план P0.4, «Рубежи и клетки»), клетки
//! E1–E5 на обоих кандидатах.
//!
//! E1 — остановленный (`SIGSTOP`) клиент под потоком двух объёмов, прирост `phys_footprint` хоста
//! сравнивается с объявленным пределом [`E1_LIMIT`]; у Rust-хоста ещё причина разрыва у клиента и
//! в журнале хоста и повторный снимок нового клиента. E2–E4 — ввод в `slow-reader`: сколько байтов
//! дошло, считается по квитанциям чтения до маркера конца, который идёт сырым вводом после
//! основного (порядок PTY сохраняет FIFO). Часть `attach` клетки E2 вводит через терминал
//! attach-клиента, маркер идёт тем же путём, а после разрыва клиента — сырым вводом. E3 у
//! Rust-хоста — номер ввода и потерянное
//! подтверждение. E4 у Rust-хоста ещё и хвост вывода с кадром выхода после выхода нагрузки. E5 —
//! публикация сокета: у zmx запасной путь патча `0003`, у Rust-хоста точки остановки окна
//! (`publish`), у обоих конкурентный стресс `ensure` и `list` (исследование).

use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::args::Target;
use crate::backend::zmx::{self, EnvKind, MAX_SOCKET_PATH, STAGING_DIR, ZmxSet};
use crate::backend::{Backend, InputOutcome, InputPath, Liveness, SessionId, signal, wait_child};
use crate::fixture::channel::{Command, Event, Link, LinkError, hex, unhex};
use crate::fixture::{Mode, flood_line};
use crate::host::attach::{EXIT_EVICTED, input_source};
use crate::host::io::InputId;
use crate::launch;
use crate::matrix::Candidate;
use crate::matrix::evidence::{Evidence, Spec, cell};
use crate::matrix::publish;
use crate::matrix::stand::{
    COMMAND_TIMEOUT, Client, STOP_GRACE, Session, Stand, WAIT, barrier, compare_states,
};
use crate::matrix::util::Growth;
use crate::oracle::vt::{BARRIER_REPLY, Reference, WaitError};
use crate::pty;
use crate::results::{Cell, DefectOrigin, Environment, Report, Requirement};
use crate::sys;

/// Объявленный предел E1: удвоение объёма потока при остановленном клиенте добавляет хосту не
/// больше стольких байтов памяти.
pub const E1_LIMIT: u64 = 8 << 20;
/// Меньший объём потока E1; больший — вдвое.
pub const E1_VOLUME: u64 = 32 << 20;
/// Ширина строки потока E1 с `\r\n` — 128 байт.
const E1_WIDTH: u16 = 126;
const E1_LINE: u64 = E1_WIDTH as u64 + 2;
/// Срок потока E1: десятки мегабайт через PTY и VT хоста.
const E1_WAIT: Duration = Duration::from_secs(180);
/// Предел очереди ввода zmx в PTY (`PTY_WRITE_BUF_MAX`).
pub const ZMX_PTY_WRITE_MAX: usize = 256 << 10;
/// Ввод E2: вдвое больше предела zmx и предела Rust-хоста (`host::io::INPUT_QUEUE_BYTES`).
const E2_BYTES: usize = 2 * ZMX_PTY_WRITE_MAX;
/// Срок записи ввода E2 в терминал attach-клиента: дольше записи стенд не держит чтение
/// остановленным.
const E2_ATTACH_WAIT: Duration = Duration::from_secs(30);
/// Ввод E4 в остановленное чтение: меньше предела, пишется в PTY частями.
const E4_PTY_BYTES: usize = 128 << 10;
/// Перерисовок экрана в E4: больше 256 КиБ вывода остановленному клиенту.
const E4_PAINTS: usize = 140;
const E3_INPUT: &[u8] = b"e3retry";
/// Повторов маркера конца ввода: переполненный буфер кандидата может отбросить и его.
const MARKER_ATTEMPTS: u32 = 4;
/// Тишина чтения, после которой маркер повторяется.
const MARKER_WAIT: Duration = Duration::from_secs(5);
/// Тишина чтения после маркера, за которой опоздавших байтов ввода уже не ждут.
const LATE_WAIT: Duration = Duration::from_secs(1);
/// Код выхода нагрузки в части `exit-tail`.
const TAIL_EXIT: u8 = 7;
/// Одновременных `ensure` и `list` в стрессе E5.
const E5_RACERS: usize = 8;
const E5_LISTS: usize = 16;
/// Длина каталога E5 с запасным путём: имя сессии из трёх байтов помещается в `sun_path`
/// (98 + 1 + 3 = 102), а `.s/<pid>` с PID от двух цифр — нет (98 + 4 + 2 = 104).
const E5_DIR_LEN: usize = 98;
const E5_NAME: &str = "e5a";
const E5_SHORT_NAME: &str = "e5u";

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    let Some(candidate) = target.backend else {
        return Err("io идёт на кандидате".to_owned());
    };
    let mut stand = Stand::for_candidate(&target.out, environment, candidate)?;
    let outcome = cells(&mut stand);
    let cleanup = stand.teardown();
    let mut report = outcome?;
    report.diagnostics.notes.extend(cleanup);
    Ok(report)
}

fn cells(stand: &mut Stand) -> Result<Report, String> {
    let mut report = Report::default();
    report.cells.push(e1(stand)?);
    report.cells.extend(e2(stand)?);
    report.cells.push(e3(stand)?);
    report.cells.extend(e4(stand)?);
    match stand.candidate {
        Candidate::Zmx => report.cells.extend(e5(stand)?),
        Candidate::Rust => {
            report.cells.extend(publish::cells(stand)?);
            report.cells.push(e5_stress_cell(stand)?);
        }
    }
    Ok(report)
}

// --- общие шаги ---

/// Следующее чтение фикстуры по квитанции; `None` — за `timeout` чтений не было.
fn next_read(link: &mut Link, timeout: Duration) -> Result<Option<Vec<u8>>, String> {
    match link.wait_for(timeout, |receipt| match &receipt.event {
        Event::Read { hex, .. } => Some(hex.clone()),
        _ => None,
    }) {
        Ok(text) => unhex(&text)
            .map(Some)
            .ok_or_else(|| "квитанция чтения: hex не разобран".to_owned()),
        Err(LinkError::Timeout) => Ok(None),
        Err(err) => Err(format!("квитанции фикстуры: {err}")),
    }
}

/// Чтения фикстуры, квитанции которых уже пришли, но ожиданиями не взяты.
fn pending_reads(link: &mut Link) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    while let Some(read) = next_read(link, Duration::ZERO)? {
        bytes.extend_from_slice(&read);
    }
    Ok(bytes)
}

/// Начало первого полного маркера `#END<цифра>#`: байты данных `#` не содержат.
fn marker_at(received: &[u8]) -> Option<usize> {
    let start = received.iter().position(|byte| *byte == b'#')?;
    let tail = &received[start..];
    (tail.len() >= 6 && tail.starts_with(b"#END") && tail[4].is_ascii_digit() && tail[5] == b'#')
        .then_some(start)
}

/// Байты, которые фикстура прочитала до маркера конца ввода, и после него до тишины
/// [`LATE_WAIT`]. Маркер идёт сырым вводом после основного и повторяется, если его отбросил
/// переполненный буфер кандидата; `None` — маркер не дошёл ни разу, это записано проверкой.
fn drain_until_marker(
    stand: &Stand,
    session: &mut Session,
    evidence: &mut Evidence,
) -> Result<Option<Vec<u8>>, String> {
    let id = session.id.clone();
    drain_until_marker_via(session, evidence, |marker| {
        stand
            .main()
            .input(&id, marker, InputPath::Raw)
            .map(|_| ())
            .map_err(|err| err.to_string())
    })
}

/// [`drain_until_marker`] с маркером, который отправляет `send`: тем же путём, что и основной
/// ввод, чтобы порядок между ними держал сам путь.
fn drain_until_marker_via(
    session: &mut Session,
    evidence: &mut Evidence,
    send: impl Fn(&[u8]) -> Result<(), String>,
) -> Result<Option<Vec<u8>>, String> {
    let mut received = Vec::new();
    for attempt in 1..=MARKER_ATTEMPTS {
        let marker = format!("#END{attempt}#").into_bytes();
        if let Err(err) = send(&marker) {
            evidence.check("маркер конца ввода отправлен", false, err);
            return Ok(None);
        }
        while let Some(bytes) = next_read(&mut session.link, MARKER_WAIT)? {
            received.extend_from_slice(&bytes);
            if let Some(at) = marker_at(&received) {
                evidence.note("marker_attempts", attempt);
                // Ввод, дошедший после маркера, — перестановка, а не потеря: это видно здесь.
                let mut late = received.split_off(at);
                while let Some(bytes) = next_read(&mut session.link, LATE_WAIT)? {
                    late.extend_from_slice(&bytes);
                }
                evidence.note("after_marker_bytes", strip_markers(&late).len());
                return Ok(Some(received));
            }
        }
    }
    evidence.check(
        "маркер конца ввода дошёл",
        false,
        format!(
            "{MARKER_ATTEMPTS} маркеров потеряны, прочитано {} байт",
            received.len()
        ),
    );
    Ok(None)
}

/// Байты без маркеров конца ввода.
fn strip_markers(bytes: &[u8]) -> Vec<u8> {
    let mut rest = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if marker_at(&bytes[at..]) == Some(0) {
            at += 6;
        } else {
            rest.push(bytes[at]);
            at += 1;
        }
    }
    rest
}

/// Данные ввода без `#`: цифры и строчные буквы по кругу.
fn pattern(len: usize) -> Vec<u8> {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    (0..len)
        .map(|index| ALPHABET[index % ALPHABET.len()])
        .collect()
}

fn read_rate(link: &mut Link, bytes_per_sec: Option<u64>) -> Result<(), String> {
    link.request(Command::ReadRate { bytes_per_sec }, WAIT)
        .map(|_| ())
        .map_err(|err| format!("темп чтения фикстуры: {err}"))
}

/// Остановка чтения фикстуры на время шага `step` и её проверка: подтверждение остановки
/// приходит до шага, а квитанций чтения между подтверждениями остановки и возобновления нет.
/// Квитанции идут в порядке исполнения фикстуры, поэтому окно между подтверждениями точное.
fn with_reading_paused<T>(
    session: &mut Session,
    evidence: &mut Evidence,
    step: impl FnOnce(&mut Session) -> T,
) -> Result<T, String> {
    read_rate(&mut session.link, Some(0))?;
    let paused = session.link.log().len();
    let value = step(session);
    read_rate(&mut session.link, None)?;
    let resumed = session.link.log().len();
    let reads = session.link.log()[paused..resumed]
        .iter()
        .filter(|receipt| matches!(receipt.event, Event::Read { .. }))
        .count();
    evidence.note("reads_while_paused", reads);
    if reads != 0 {
        evidence.not_proven = Some(format!(
            "фикстура читала {reads} раз во время остановки чтения: порядок стенда не доказан"
        ));
    }
    Ok(value)
}

/// Запись фикстуры с подтверждением и та же запись в эталон. Отказ записи — вывод: кандидат не
/// читает PTY, и фикстура стоит на записи.
fn emit_checked(
    session: &mut Session,
    reference: &mut Reference,
    evidence: &mut Evidence,
    name: &str,
    bytes: &[u8],
) -> bool {
    let written = session
        .link
        .request(Command::Write { hex: hex(bytes) }, WAIT * 2);
    match written {
        Ok(count) if count == bytes.len() as u64 => {
            reference.feed(bytes);
            true
        }
        Ok(count) => evidence.check(name, false, format!("записано {count} из {}", bytes.len())),
        Err(err) => evidence.check(name, false, err),
    }
}

fn leader(evidence: &mut Evidence, session: &mut Session, client: &Client, label: &str) -> bool {
    evidence
        .require(&format!("{label} лидер"), client.wait_leader())
        .is_some()
        && evidence
            .require(
                &format!("барьер через {label}"),
                barrier(&mut session.link, client),
            )
            .is_some()
}

// --- E1 ---

/// Замер одного объёма: память хоста до и после потока при остановленном клиенте.
#[derive(Debug, serde::Serialize)]
struct Volume {
    lines: u64,
    bytes: u64,
    base: u64,
    after: u64,
}

fn e1(stand: &mut Stand) -> Result<Cell, String> {
    let mut evidence = Evidence::default();
    let mut artifacts = Vec::new();
    let mut volumes = Vec::new();
    for (label, volume) in [("e1-v", E1_VOLUME), ("e1-2v", 2 * E1_VOLUME)] {
        let id = stand.session(label)?;
        artifacts.push(stand.log_name(&id));
        let mut session = stand.ensure(&id, Mode::Flood)?;
        let measured = e1_volume(stand, &mut session, &mut evidence, label, volume / E1_LINE);
        stand.discard(&id);
        match measured? {
            Some(measured) => volumes.push(measured),
            None => break,
        }
    }
    if let [small, large] = volumes.as_slice() {
        let growth = Growth::new(E1_VOLUME, small.base, small.after, large.base, large.after);
        evidence.note("growth", growth);
        evidence.note("growth.doubling", growth.doubling());
        evidence.check(
            "прирост памяти при удвоении объёма не больше предела",
            growth.within(E1_LIMIT),
            format!(
                "приросты {} и {} байт, удвоение добавило {} байт, предел {E1_LIMIT}",
                growth.small,
                growth.large,
                growth.doubling()
            ),
        );
    }
    evidence.note("volumes", &volumes);
    artifacts.insert(0, stand.artifact("e1.json", &evidence)?);
    let (stimulus_tail, oracle) = match stand.candidate {
        Candidate::Zmx => (
            "",
            "квитанции фикстуры (поток закончен, задержки записи); снимок хоста с последней \
             строкой; клиенты по списку во время остановки; ri_phys_footprint хоста до и после \
             каждого объёма; предел удвоения E1_LIMIT 8 МиБ",
        ),
        Candidate::Rust => (
            "; после потока c1 продолжен, затем подключается c2",
            "квитанции фикстуры (поток закончен, задержки записи); снимок хоста с последней \
             строкой; клиенты по списку во время остановки; разрыв с причиной в журнале и \
             состоянии хоста и код выхода c1 после продолжения; последняя строка у приёмника \
             c2; ri_phys_footprint хоста до и после каждого объёма; предел удвоения E1_LIMIT \
             8 МиБ",
        ),
    };
    Ok(cell(
        Spec {
            id: "E1",
            part: None,
            stimulus: format!(
                "клиент c1 подключён и остановлен SIGSTOP; фикстура пишет {} и {} МиБ строками по \
                 {E1_LINE} байт без паузы в двух сессиях{stimulus_tail}",
                E1_VOLUME >> 20,
                (2 * E1_VOLUME) >> 20
            ),
            oracle,
            completion: "квитанция конца потока или срок 180 с; снимок хоста после неё",
            origin: DefectOrigin::Host,
        },
        &evidence,
        artifacts,
    ))
}

fn e1_volume(
    stand: &Stand,
    session: &mut Session,
    evidence: &mut Evidence,
    label: &str,
    lines: u64,
) -> Result<Option<Volume>, String> {
    let mut client = stand.client(&session.id, "c1")?;
    if !leader(evidence, session, &client, &format!("{label}: c1")) {
        return Ok(None);
    }
    let host = session.record.host.pid;
    if let Err(err) = stand.main().stall_client(&client.process, true) {
        return Err(format!("SIGSTOP клиенту: {err}"));
    }
    let Some(base) = sys::phys_footprint(host) else {
        evidence.not_proven = Some(format!("{label}: память хоста PID {host} не читается"));
        return Ok(None);
    };
    session
        .link
        .request(
            Command::Flood {
                lines,
                width: E1_WIDTH,
                interval_us: 0,
                report_every: (lines / 8).max(1),
            },
            WAIT,
        )
        .map_err(|err| format!("{label}: команда потока: {err}"))?;
    let started = Instant::now();
    let done = session
        .link
        .wait_for(E1_WAIT, |receipt| match &receipt.event {
            Event::FloodDone {
                lines,
                bytes,
                cancelled,
                writes,
                ..
            } => Some((*lines, *bytes, *cancelled, *writes)),
            _ => None,
        });
    evidence.note(
        &format!("{label}.elapsed_ms"),
        started.elapsed().as_millis() as u64,
    );
    let liveness = stand.main().liveness(&session.id);
    evidence.note(&format!("{label}.liveness"), format!("{liveness:?}"));
    let Some((written, bytes, cancelled, writes)) = evidence.require(
        &format!("{label}: хост читает PTY при остановленном клиенте"),
        done,
    ) else {
        let _ = stand.main().stall_client(&client.process, false);
        return Ok(None);
    };
    evidence.note(&format!("{label}.writes"), writes);
    evidence.check(
        &format!("{label}: поток записан целиком"),
        written == lines && !cancelled,
        format!(
            "строк {written} из {lines}, байт {bytes}, задержек {} на {} мс",
            writes.stalls,
            writes.stalled_ns / 1_000_000
        ),
    );
    let last = flood_line(&session.fixture_nonce, lines, E1_WIDTH);
    let last = String::from_utf8_lossy(&last[..last.len().min(32)]).into_owned();
    match stand.main().snapshot(&session.id) {
        Ok(snapshot) => {
            evidence.check(
                &format!("{label}: хост обработал последнюю строку"),
                snapshot.text.contains(&last),
                format!("ревизия {}", snapshot.revision),
            );
        }
        Err(err) => {
            evidence.check(&format!("{label}: снимок хоста"), false, err);
        }
    }
    let disconnected = matches!(liveness, Ok(Liveness::Live { clients: 0, .. }));
    evidence.check(
        &format!("{label}: явный разрыв медленного клиента или обратное давление"),
        disconnected,
        format!(
            "клиент не отключён и поток не остановлен: {}",
            match &liveness {
                Ok(state) => format!("{state:?}"),
                Err(err) => err.to_string(),
            }
        ),
    );
    let after = sys::phys_footprint(host);
    stand
        .main()
        .stall_client(&client.process, false)
        .map_err(|err| format!("SIGCONT клиенту: {err}"))?;
    if stand.candidate == Candidate::Rust && disconnected {
        e1_rust_resync(stand, session, evidence, label, &mut client, &last)?;
    }
    let Some(after) = after else {
        evidence.not_proven = Some(format!("{label}: память хоста PID {host} не читается"));
        return Ok(None);
    };
    Ok(Some(Volume {
        lines,
        bytes: lines * E1_LINE,
        base,
        after,
    }))
}

/// Rust-хост после разрыва: причина в состоянии и журнале хоста и у самого клиента (код выхода
/// после продолжения), новый клиент получает повторный снимок с последней строкой потока.
fn e1_rust_resync(
    stand: &Stand,
    session: &mut Session,
    evidence: &mut Evidence,
    label: &str,
    client: &mut Client,
    last: &str,
) -> Result<(), String> {
    let status = stand
        .rust()?
        .status(&session.id)
        .map_err(|err| format!("{label}: состояние хоста: {err}"))?;
    let log = stand.daemon_log(&session.id);
    let reasons: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("разорван: очередь вывода клиента переполнена"))
        .collect();
    evidence.note(&format!("{label}.evictions"), status.evictions);
    evidence.note(&format!("{label}.eviction_log"), &reasons);
    evidence.check(
        &format!("{label}: разрыв с причиной в журнале и состоянии хоста"),
        status.evictions == 1 && reasons.len() == 1,
        format!(
            "разрывов в состоянии {}, строк причины в журнале {}",
            status.evictions,
            reasons.len()
        ),
    );
    let exit = client.wait_exit();
    evidence.note(&format!("{label}.c1_exit"), format!("{exit:?}"));
    evidence.check(
        &format!("{label}: c1 после продолжения получил причину разрыва"),
        exit.as_ref()
            .is_ok_and(|status| status.code() == Some(EXIT_EVICTED)),
        format!("выход c1 {exit:?}, ожидался код {EXIT_EVICTED}"),
    );
    let fresh = stand.client(&session.id, "c2")?;
    if !leader(evidence, session, &fresh, &format!("{label}: c2")) {
        return Ok(());
    }
    let rows = fresh.capture()?.text();
    evidence.check(
        &format!("{label}: повторный снимок у c2 с последней строкой потока"),
        rows.iter().any(|row| row.contains(last)),
        format!("последняя строка «{last}» у приёмника c2 не найдена"),
    );
    Ok(())
}

// --- E2 ---

fn e2(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    for (part, path) in [("send", InputPath::Raw), ("type", InputPath::Typed)] {
        let id = stand.session(&format!("e2-{part}"))?;
        let mut session = stand.ensure(&id, Mode::SlowReader)?;
        let mut evidence = Evidence::default();
        evidence.note("record", &session.record);
        let outcome = e2_body(stand, &mut session, &mut evidence, path);
        stand.discard(&id);
        outcome?;
        let artifact = stand.artifact(&format!("e2-{part}.json"), &evidence)?;
        let way = match stand.candidate {
            Candidate::Zmx => format!("путём zmx {part}"),
            Candidate::Rust => format!("управляющим вводом с номером (путь {part})"),
        };
        cells.push(cell(
            Spec {
                id: "E2",
                part: Some(part),
                stimulus: format!(
                    "чтение фикстуры остановлено; {} КиБ ввода {way} без клиента; затем чтение \
                     возобновлено и отправлен маркер конца",
                    E2_BYTES >> 10
                ),
                oracle: "квитанции прочитанных байтов до маркера против отправленного; ответ \
                         операции ввода",
                completion: "маркер конца ввода прочитан фикстурой",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact, stand.log_name(&id)],
        ));
    }
    let id = stand.session("e2-attach")?;
    let mut session = stand.ensure(&id, Mode::SlowReader)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    let outcome = e2_attach(stand, &mut session, &mut evidence);
    stand.discard(&id);
    outcome?;
    let artifact = stand.artifact("e2-attach.json", &evidence)?;
    cells.push(cell(
        Spec {
            id: "E2",
            part: Some("attach"),
            stimulus: format!(
                "клиент c1 ведёт сессию; чтение фикстуры остановлено; {} КиБ ввода в терминал \
                 c1, как вставка в окне GUI; затем чтение возобновлено и маркер конца отправлен \
                 через c1, а если c1 закончился, — сырым вводом без клиента",
                E2_BYTES >> 10
            ),
            oracle: "квитанции прочитанных байтов до маркера против отправленного: клиент жив — \
                     ввод целиком; клиент разорван с причиной (код выхода и журнал хоста) — \
                     дошло начало ввода без дыр; иначе потеря молчаливая",
            completion: "маркер конца ввода прочитан фикстурой",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ));
    Ok(cells)
}

/// Ввод сверх предела обычным путём GUI: через attach-клиента, который подтверждений не читает.
fn e2_attach(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let mut client = stand.client(&session.id, "c1")?;
    if !leader(evidence, session, &client, "c1") {
        return Ok(());
    }
    // Ответ c1 на барьер фикстура читает как ввод, а барьер ждёт только квитанцию ответа: квитанция
    // чтения остаётся в очереди и без этого открыла бы принятые байты.
    let before = pending_reads(&mut session.link)?;
    evidence.note("reads_before_input", hex(&before));
    if before != BARRIER_REPLY {
        evidence.not_proven = Some(format!(
            "до ввода фикстура прочитала {} вместо ответа барьера {}",
            hex(&before),
            hex(BARRIER_REPLY)
        ));
    }
    let data = pattern(E2_BYTES);
    let written_before = client.route()?.stats().written;
    // Чтение возобновляется, когда весь ввод записан в терминал клиента или клиент закончился:
    // до этого переполнение кандидата ещё не застигнуто.
    let passed = with_reading_paused(session, evidence, |_| -> Result<_, String> {
        client.input(&data)?;
        Ok(client.route()?.wait_until(E2_ATTACH_WAIT, |_, stats| {
            stats.written - written_before >= data.len() as u64
        }))
    })??;
    evidence.note("client_input", format!("{passed:?}"));
    let id = session.id.clone();
    // Весь ввод в терминале c1 ещё не значит, что c1 жив: хост разбирает накопленные события
    // ввода позже и может разорвать его после записи. Путь маркера выбирается на каждой попытке.
    let paths = std::cell::RefCell::new(Vec::new());
    let received = drain_until_marker_via(session, evidence, |marker| {
        let live = client.route()?.end().is_none();
        paths.borrow_mut().push(if live { "c1" } else { "raw" });
        if live {
            client.input(marker)
        } else {
            stand
                .main()
                .input(&id, marker, InputPath::Raw)
                .map(|_| ())
                .map_err(|err| err.to_string())
        }
    })?;
    evidence.note("marker_paths", paths.into_inner());
    let exit = if client.route()?.end().is_some() {
        Some(client.wait_exit())
    } else {
        None
    };
    evidence.note("c1_exit", format!("{exit:?}"));
    let Some(received) = received else {
        return Ok(());
    };
    evidence.note("sent", data.len());
    evidence.note("received", received.len());
    evidence.note(
        "first_difference",
        received
            .iter()
            .zip(&data)
            .position(|(got, want)| got != want),
    );
    let log = stand.daemon_log(&id);
    let reasons: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("разорван: ввод клиента не принят"))
        .collect();
    evidence.note("eviction_log", &reasons);
    match exit {
        None => {
            evidence.check(
                "ввод через клиента доставлен целиком или отвергнут явно",
                received == data,
                format!(
                    "клиент жив, а дошло {} из {} байт: потеря молчаливая",
                    received.len(),
                    data.len()
                ),
            );
        }
        Some(Ok(status)) if status.code() == Some(EXIT_EVICTED) => {
            evidence.check(
                "явный отказ: дошло начало ввода без дыр",
                received.len() < data.len() && data.starts_with(&received),
                format!(
                    "дошло {} из {} байт, совпадает с началом ввода: {}",
                    received.len(),
                    data.len(),
                    data.starts_with(&received)
                ),
            );
            evidence.check(
                "причина разрыва в журнале хоста",
                reasons.len() == 1,
                format!("строк причины в журнале {}", reasons.len()),
            );
        }
        Some(other) => {
            evidence.check(
                "клиент закончился разрывом с причиной",
                false,
                format!(
                    "выход c1 {other:?}, ожидался код {EXIT_EVICTED}; дошло {} из {} байт",
                    received.len(),
                    data.len()
                ),
            );
        }
    }
    Ok(())
}

fn e2_body(
    stand: &Stand,
    session: &mut Session,
    evidence: &mut Evidence,
    path: InputPath,
) -> Result<(), String> {
    let data = pattern(E2_BYTES);
    let outcome = with_reading_paused(session, evidence, |session| {
        stand.main().input(&session.id, &data, path)
    })?;
    evidence.note("outcome", format!("{outcome:?}"));
    let Some(received) = drain_until_marker(stand, session, evidence)? else {
        return Ok(());
    };
    evidence.note("sent", data.len());
    evidence.note("received", received.len());
    let complete = received == data;
    match outcome {
        Ok(InputOutcome::Sent | InputOutcome::Accepted) => {
            evidence.check(
                "ввод сверх предела доставлен или отвергнут явно",
                complete,
                format!(
                    "операция ответила успехом, а дошло {} из {} байт: потеря молчаливая",
                    received.len(),
                    data.len()
                ),
            );
        }
        Ok(InputOutcome::Rejected(reason)) => {
            evidence.check(
                "явный отказ без частичной доставки",
                received.is_empty(),
                format!("отказ «{reason}», дошло {} байт", received.len()),
            );
        }
        Ok(other @ (InputOutcome::Duplicate | InputOutcome::Unknown)) => {
            evidence.check(
                "операция ввода дала определённый ответ",
                false,
                format!("{other:?}; дошло {} из {} байт", received.len(), data.len()),
            );
        }
        Err(err) => {
            evidence.check(
                "операция ввода дала ответ",
                false,
                format!("{err}; дошло {} из {} байт", received.len(), data.len()),
            );
        }
    }
    let log = stand.daemon_log(&session.id);
    evidence.note(
        "daemon_log_drops",
        log.lines()
            .filter(|line| {
                line.contains("drop") || line.contains("discard") || line.contains("отвергнут")
            })
            .take(8)
            .collect::<Vec<_>>(),
    );
    Ok(())
}

// --- E3 ---

fn e3(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("e3")?;
    let mut session = stand.ensure(&id, Mode::SlowReader)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    let outcome = match stand.candidate {
        Candidate::Zmx => e3_body(stand, &mut session, &mut evidence),
        Candidate::Rust => e3_rust(stand, &mut session, &mut evidence),
    };
    stand.discard(&id);
    outcome?;
    let artifact = stand.artifact("e3.json", &evidence)?;
    let (stimulus, oracle) = match stand.candidate {
        Candidate::Zmx => (
            "zmx type «e3retry» с подтверждением, затем тот же запрос повторён, как после \
             потерянного подтверждения; затем маркер конца",
            "квитанции прочитанных байтов: сколько раз ввод дошёл; ответы операций",
        ),
        Candidate::Rust => (
            "ввод «e3retry» с номером, соединение закрыто без чтения подтверждения; тот же номер \
             повторён с подтверждением и ещё раз; затем маркер конца",
            "квитанции прочитанных байтов: сколько раз ввод дошёл; ответы операций: «неизвестно» \
             у потерянного подтверждения, «повтор» у повтора принятого номера",
        ),
    };
    Ok(cell(
        Spec {
            id: "E3",
            part: None,
            stimulus: stimulus.to_owned(),
            oracle,
            completion: "маркер конца ввода прочитан фикстурой",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ))
}

fn e3_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let first = stand.zmx.input(&session.id, E3_INPUT, InputPath::Typed);
    let retry = stand.zmx.input(&session.id, E3_INPUT, InputPath::Typed);
    evidence.note("first", format!("{first:?}"));
    evidence.note("retry", format!("{retry:?}"));
    if evidence.require("первый ввод принят", first).is_none() {
        return Ok(());
    }
    let Some(received) = drain_until_marker(stand, session, evidence)? else {
        return Ok(());
    };
    let copies = copies(&received);
    evidence.note("received", String::from_utf8_lossy(&received));
    evidence.check(
        "повтор не дублирует ввод",
        copies == 1,
        format!(
            "ввод дошёл {copies} раз: у zmx type нет номера запроса, повтор неотличим от нового \
             ввода"
        ),
    );
    evidence.note(
        "unknown_outcome",
        "zmx type различает «принято», «отказ» и прочую ошибку; ответа «неизвестно» с номером \
         для повтора нет",
    );
    Ok(())
}

fn copies(received: &[u8]) -> usize {
    received
        .windows(E3_INPUT.len())
        .filter(|window| *window == E3_INPUT)
        .count()
}

/// Потерянное подтверждение у Rust-хоста: первая попытка не читает ответ («неизвестно»), повтор
/// того же номера принимается не больше одного раза. Какая из двух попыток дошла до хоста
/// первой, зависит от потоков соединений; вывод от этого не зависит.
fn e3_rust(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let rust = stand.rust()?;
    let input = InputId {
        source: input_source(),
        seq: 1,
    };
    evidence.note("input", format!("{:x}:{}", input.source, input.seq));
    let first = rust.input_numbered(&session.id, input, E3_INPUT, false);
    let retry = rust.input_numbered(&session.id, input, E3_INPUT, true);
    let again = rust.input_numbered(&session.id, input, E3_INPUT, true);
    evidence.note("first", format!("{first:?}"));
    evidence.note("retry", format!("{retry:?}"));
    evidence.note("again", format!("{again:?}"));
    evidence.check(
        "потерянное подтверждение даёт итог «неизвестно»",
        matches!(first, Ok(InputOutcome::Unknown)),
        format!("{first:?}"),
    );
    evidence.check(
        "повтор после потерянного подтверждения принят или узнан как повтор",
        matches!(retry, Ok(InputOutcome::Accepted | InputOutcome::Duplicate)),
        format!("{retry:?}"),
    );
    evidence.check(
        "повтор принятого номера подтверждён как повтор",
        matches!(again, Ok(InputOutcome::Duplicate)),
        format!("{again:?}"),
    );
    let Some(received) = drain_until_marker(stand, session, evidence)? else {
        return Ok(());
    };
    let copies = copies(&received);
    evidence.note("received", String::from_utf8_lossy(&received));
    evidence.check(
        "повтор не дублирует ввод",
        copies == 1,
        format!("ввод дошёл {copies} раз"),
    );
    Ok(())
}

// --- E4 ---

fn e4(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();

    let id = stand.session("e4-sock")?;
    let mut session = stand.ensure(&id, Mode::Emitter)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    let outcome = e4_socket(stand, &mut session, &mut evidence);
    stand.discard(&id);
    outcome?;
    let artifact = stand.artifact("e4-socket.json", &evidence)?;
    cells.push(cell(
        Spec {
            id: "E4",
            part: Some("socket"),
            stimulus: format!(
                "c1 подключён и остановлен; фикстура пишет {E4_PAINTS} перерисовок экрана \
                 (больше 256 КиБ) и начало CSI 38;2; c1 убит SIGKILL посреди последовательности; \
                 фикстура пишет остаток; подключается c2"
            ),
            oracle: "эталон и приёмник c2",
            completion: "c2 стал лидером, барьер DSR через c2 пройден, состояния сняты",
            origin: DefectOrigin::Unknown,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ));

    let id = stand.session("e4-pty")?;
    let mut session = stand.ensure(&id, Mode::SlowReader)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    let outcome = e4_pty(stand, &mut session, &mut evidence);
    stand.discard(&id);
    outcome?;
    let artifact = stand.artifact("e4-pty.json", &evidence)?;
    cells.push(cell(
        Spec {
            id: "E4",
            part: Some("pty"),
            stimulus: format!(
                "чтение фикстуры остановлено; {} КиБ сырого ввода без клиента (меньше предела \
                 256 КиБ) — запись в PTY частями; затем чтение возобновлено и отправлен маркер \
                 конца",
                E4_PTY_BYTES >> 10
            ),
            oracle: "квитанции прочитанных байтов: ввод целиком и по порядку; квитанций чтения \
                     между подтверждениями остановки и возобновления нет",
            completion: "маркер конца ввода прочитан фикстурой, после него — тишина 1 с",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ));

    if stand.candidate == Candidate::Rust {
        let id = stand.session("e4-tail")?;
        let mut session = stand.ensure(&id, Mode::Emitter)?;
        let mut evidence = Evidence::default();
        evidence.note("record", &session.record);
        let outcome = e4_exit_tail(stand, &mut session, &mut evidence);
        stand.discard(&id);
        outcome?;
        let artifact = stand.artifact("e4-exit-tail.json", &evidence)?;
        cells.push(cell(
            Spec {
                id: "E4",
                part: Some("exit-tail"),
                stimulus: format!(
                    "лидер c1 подключён; фикстура пишет {E4_PAINTS} перерисовок экрана и \
                     последнюю строку и сразу выходит с кодом {TAIL_EXIT}"
                ),
                oracle: "эталон и приёмник c1 после выхода клиента; код выхода c1 (0 — кадр \
                         выхода нагрузки); выход хоста",
                completion: "маршрут c1 закрыт выходом клиента, хост вышел",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact, stand.log_name(&id)],
        ));
    }
    Ok(cells)
}

fn paint(round: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for row in 1..=crate::matrix::stand::ROWS {
        let text = format!("paint {round:03} row {row:02} ");
        let mut line = format!("\x1b[{row};1H{text}").into_bytes();
        line.resize(line.len() + 78 - text.len(), b'.');
        bytes.extend_from_slice(&line);
    }
    bytes
}

fn e4_socket(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let mut reference = Reference::new(&stand.config).map_err(|err| format!("эталон: {err}"))?;
    let mut first = stand.client(&session.id, "c1")?;
    if !leader(evidence, session, &first, "c1") {
        return Ok(());
    }
    if !emit_checked(
        session,
        &mut reference,
        evidence,
        "сцена до остановки",
        b"\x1b[2J\x1b[Hbefore stall\r\n",
    ) {
        return Ok(());
    }
    stand
        .main()
        .stall_client(&first.process, true)
        .map_err(|err| format!("SIGSTOP клиенту: {err}"))?;
    let block: Vec<u8> = (0..E4_PAINTS).flat_map(paint).collect();
    evidence.note("block_bytes", block.len());
    let wrote = emit_checked(session, &mut reference, evidence, "перерисовки", &block)
        && emit_checked(
            session,
            &mut reference,
            evidence,
            "начало последовательности",
            b"\x1b[5;5H\x1b[38;2;10;20",
        );
    // Ревизия после записи доказывает, что хост прочитал начало последовательности до обрыва.
    let revision = stand.main().read_boundary(&session.id);
    evidence.note("revision_before_kill", format!("{revision:?}"));
    let killed = first.kill(stand.main());
    evidence.note("c1.exit", format!("{killed:?}"));
    if !wrote
        || evidence
            .require("c1 убит посреди последовательности", killed)
            .is_none()
    {
        return Ok(());
    }
    if !emit_checked(
        session,
        &mut reference,
        evidence,
        "остаток последовательности",
        b";30mcut\x1b[0m tail",
    ) {
        return Ok(());
    }
    let second = stand.client(&session.id, "c2")?;
    if !leader(evidence, session, &second, "c2") {
        return Ok(());
    }
    compare_states(evidence, "после обрыва у c2", &reference, &second)?;
    Ok(())
}

fn e4_pty(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let data = pattern(E4_PTY_BYTES);
    let outcome = with_reading_paused(session, evidence, |session| {
        stand.main().input(&session.id, &data, InputPath::Raw)
    })?;
    evidence.note("outcome", format!("{outcome:?}"));
    if evidence.require("ввод отправлен", outcome).is_none() {
        return Ok(());
    }
    let Some(received) = drain_until_marker(stand, session, evidence)? else {
        return Ok(());
    };
    let first_difference = received
        .iter()
        .zip(&data)
        .position(|(got, want)| got != want);
    evidence.check(
        "ввод дошёл целиком и по порядку",
        received == data,
        format!(
            "дошло {} из {} байт до маркера, первое расхождение {first_difference:?}",
            received.len(),
            data.len()
        ),
    );
    Ok(())
}

/// Хвост вывода и кадр выхода у Rust-хоста: нагрузка пишет хвост и сразу выходит, хост дочитывает
/// PTY, шлёт кадр выхода и дописывает очередь клиента до своего выхода.
fn e4_exit_tail(
    stand: &Stand,
    session: &mut Session,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let mut reference = Reference::new(&stand.config).map_err(|err| format!("эталон: {err}"))?;
    let mut client = stand.client(&session.id, "c1")?;
    if !leader(evidence, session, &client, "c1") {
        return Ok(());
    }
    let mut tail: Vec<u8> = (0..E4_PAINTS).flat_map(paint).collect();
    tail.extend_from_slice(b"\x1b[24;1H\x1b[2Ktail end");
    evidence.note("tail_bytes", tail.len());
    if !emit_checked(session, &mut reference, evidence, "хвост вывода", &tail) {
        return Ok(());
    }
    session
        .link
        .request(Command::Exit { code: TAIL_EXIT }, WAIT)
        .map_err(|err| format!("команда выхода фикстуры: {err}"))?;
    let exit = client.wait_exit();
    evidence.note("c1_exit", format!("{exit:?}"));
    if evidence
        .require("c1 вышел после выхода нагрузки", exit.as_ref())
        .is_none()
    {
        return Ok(());
    }
    evidence.check(
        "c1 получил кадр выхода нагрузки",
        exit.as_ref().is_ok_and(|status| status.code() == Some(0)),
        format!("выход c1 {exit:?}: код 0 — кадр выхода, другие — разрыв без него"),
    );
    let route = client.route()?;
    match route.wait_until(WAIT, |_, _| false) {
        Err(WaitError::Ended(end)) => evidence.note("route_end", format!("{end:?}")),
        other => {
            return Err(format!(
                "маршрут c1 не закрылся после выхода клиента: {other:?}"
            ));
        }
    }
    let route_stats = route.stats();
    evidence.note("route", route_stats);
    compare_states(evidence, "хвост у c1 после выхода", &reference, &client)?;
    let host = session.record.host;
    let gone = launch::wait_exit(&[host.pid], WAIT).is_empty();
    evidence.check(
        "хост вышел после выхода нагрузки",
        gone,
        format!("хост PID {} жив через {} с", host.pid, WAIT.as_secs()),
    );
    let log = stand.daemon_log(&session.id);
    evidence.note(
        "host_log_exit",
        log.lines()
            .filter(|line| line.contains("PTY") || line.contains("дочитал"))
            .collect::<Vec<_>>(),
    );
    Ok(())
}

// --- E5 ---

fn e5(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();

    let mut evidence = Evidence::default();
    e5_fallback(stand, &mut evidence)?;
    let artifact = stand.artifact("e5-fallback.json", &evidence)?;
    cells.push(cell(
        Spec {
            id: "E5",
            part: Some("fallback-path"),
            stimulus: format!(
                "ZMX_DIR длиной {E5_DIR_LEN} байт, сессия «{E5_NAME}»: имя сессии помещается в \
                 sun_path, а .s/<pid> демона — нет; zmx attach запущен напрямую (адаптер такой \
                 каталог отвергает)"
            ),
            oracle: "сокет сессии после команды, PID демона и длина его пути подготовки; \
                     опубликованный при невозможном пути подготовки сокет — прямой bind без \
                     атомарной публикации",
            completion: "команда создания вышла; сокет и процессы проверены",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact],
    ));

    let mut evidence = Evidence::default();
    e5_unwritable(stand, &mut evidence)?;
    let artifact = stand.artifact("e5-staging.json", &evidence)?;
    cells.push(cell(
        Spec {
            id: "E5",
            part: Some("staging-unwritable"),
            stimulus: format!(
                "ZMX_DIR с каталогом {STAGING_DIR} без права записи (0500), ensure сессии \
                 «{E5_SHORT_NAME}»"
            ),
            oracle: "итог ensure, сокет сессии и каталог подготовки",
            completion: "ensure вернулся",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact],
    ));

    cells.push(e5_stress_cell(stand)?);
    Ok(cells)
}

fn e5_stress_cell(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("e5")?;
    stand.sessions.push(id.clone());
    let mut evidence = Evidence::default();
    let outcome = e5_stress(stand, &id, &mut evidence);
    stand.discard(&id);
    outcome?;
    let artifact = stand.artifact("e5-stress.json", &evidence)?;
    let mut stress = cell(
        Spec {
            id: "E5",
            part: Some("stress"),
            stimulus: match stand.candidate {
                Candidate::Zmx => format!(
                    "{E5_RACERS} одновременных ensure одной сессии и {E5_RACERS} потоков по \
                     {E5_LISTS} list; attach zmx без нагрузки создал бы сессию сам, поэтому \
                     attach в окне — это ensure"
                ),
                Candidate::Rust => format!(
                    "{E5_RACERS} одновременных ensure одной сессии и {E5_RACERS} потоков по \
                     {E5_LISTS} list"
                ),
            },
            oracle: "одна нагрузка и один хост, сокет и пустой каталог подготовки, list без \
                     ошибок по сессии; попадание в окно публикации не доказывается",
            completion: "все ensure и list вернулись",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    );
    stress.requirement = Requirement::Research;
    Ok(stress)
}

/// Каталог длиной ровно `len` байт внутри корня стенда.
fn padded_dir(root: &Path, len: usize) -> Result<Option<PathBuf>, String> {
    let base = root.as_os_str().len() + 1;
    if len <= base {
        return Ok(None);
    }
    let dir = root.join("p".repeat(len - base));
    fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    Ok(Some(dir))
}

fn digits(value: i32) -> usize {
    value.unsigned_abs().to_string().len()
}

/// Добивает процессы с меткой в аргументах: демон и нагрузку сессии вне адаптера стенда.
fn kill_tagged(token: &str) -> Vec<i32> {
    let found = launch::find_by_token(token);
    let pids: Vec<i32> = found.iter().map(|process| process.pid).collect();
    for pid in &pids {
        let _ = signal(*pid, libc::SIGKILL);
    }
    launch::wait_exit(&pids, STOP_GRACE);
    pids
}

fn socket_at(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_socket())
}

fn e5_fallback(stand: &Stand, evidence: &mut Evidence) -> Result<(), String> {
    let Some(dir) = padded_dir(&stand.root, E5_DIR_LEN)? else {
        evidence.not_proven = Some(format!(
            "корень стенда {} не короче {E5_DIR_LEN} байт",
            stand.root.display()
        ));
        return Ok(());
    };
    let id = SessionId::new(E5_NAME)?;
    let final_path = dir.join(E5_NAME);
    evidence.note("dir", dir.display().to_string());
    evidence.note("final_len", final_path.as_os_str().len());
    let refused = zmx::check_socket_paths(&dir, E5_NAME.len());
    evidence.note("adapter_refusal", format!("{refused:?}"));
    let config = zmx::Config {
        set: ZmxSet::Current,
        dir: dir.clone(),
        ..stand.zmx.config().clone()
    };
    let fixture_nonce = format!("s{}-e5a-w", stand.nonce);
    let workload = stand.workload(Mode::Emitter, &fixture_nonce)?;
    let env = zmx::command_env(&config, EnvKind::Create).map_err(|err| err.to_string())?;
    let terminal = pty::open(stand.size).map_err(|err| format!("PTY: {err}"))?;
    let stdin = terminal
        .slave
        .try_clone()
        .map_err(|err| format!("PTY: {err}"))?;
    let (stdout, closed) = UnixStream::pair().map_err(|err| format!("канал stdout: {err}"))?;
    drop(closed);
    let mut command = Process::new(&config.binary);
    command
        .args(zmx::ensure_args(&id, &workload))
        .env_clear()
        .envs(env)
        .current_dir(&config.cwd)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(OwnedFd::from(stdout)))
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|err| format!("zmx attach: {err}"))?;
    // Команда, которая не вышла в срок, — тоже наблюдение: сокет проверяется в любом случае.
    let exit = match wait_child(&mut child, COMMAND_TIMEOUT) {
        Ok(status) => format!("{status:?}"),
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            format!("не вышла: {err}")
        }
    };
    evidence.note("exit", exit);
    let published = socket_at(&final_path);
    let staged = fs::read_dir(dir.join(STAGING_DIR))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    evidence.note("published", published);
    evidence.note("staged", &staged);
    let mut host = None;
    if published {
        match stand.accept(Some(&fixture_nonce)) {
            Ok((_link, ready)) => {
                host = crate::observer::inspect(ready.pid)
                    .ok()
                    .flatten()
                    .map(|info| info.ppid);
                evidence.note("ready", &ready);
            }
            Err(err) => evidence.note("accept", err),
        }
    }
    drop(terminal);
    let killed = kill_tagged(&fixture_nonce);
    evidence.note("killed", killed);
    if !published {
        evidence.check(
            "без пути подготовки публикация не идёт молча",
            true,
            "сессия не опубликована: zmx отказал явно",
        );
        return Ok(());
    }
    let Some(host) = host else {
        evidence.not_proven = Some("PID демона не найден: длина пути подготовки неизвестна".into());
        return Ok(());
    };
    let staged_len = dir.as_os_str().len() + 1 + STAGING_DIR.len() + 1 + digits(host);
    evidence.note("host_pid", host);
    evidence.note("staged_len", staged_len);
    if staged_len <= MAX_SOCKET_PATH {
        evidence.not_proven = Some(format!(
            "путь подготовки демона PID {host} — {staged_len} байт, помещается в sun_path"
        ));
        return Ok(());
    }
    evidence.check(
        "без пути подготовки публикация не идёт молча",
        false,
        format!(
            "сокет опубликован, хотя .s/{host} — {staged_len} байт больше {MAX_SOCKET_PATH}: \
             запасной путь патча 0003, прямой bind под именем сессии"
        ),
    );
    Ok(())
}

fn e5_unwritable(stand: &Stand, evidence: &mut Evidence) -> Result<(), String> {
    let dir = stand.root.join("u");
    let staging = dir.join(STAGING_DIR);
    fs::create_dir_all(&staging).map_err(|err| format!("{}: {err}", staging.display()))?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o500))
        .map_err(|err| format!("{}: {err}", staging.display()))?;
    let backend = zmx::Zmx::new(zmx::Config {
        set: ZmxSet::Current,
        dir: dir.clone(),
        ..stand.zmx.config().clone()
    })?;
    let id = SessionId::new(E5_SHORT_NAME)?;
    let fixture_nonce = format!("s{}-e5u-w", stand.nonce);
    let workload = stand.workload(Mode::Emitter, &fixture_nonce)?;
    let ensured = backend.ensure(&id, &workload, stand.size);
    evidence.note("ensure", format!("{ensured:?}"));
    let published = socket_at(&backend.socket_path(&id));
    evidence.note("published", published);
    if published {
        let accepted = stand.accept(Some(&fixture_nonce));
        evidence.note("accept", format!("{:?}", accepted.map(|(_, ready)| ready)));
    }
    let killed = kill_tagged(&fixture_nonce);
    evidence.note("killed", killed);
    let _ = fs::set_permissions(&staging, fs::Permissions::from_mode(0o700));
    evidence.check(
        "без записи в каталог подготовки публикация не идёт молча",
        !published,
        if published {
            "сокет опубликован в обход каталога подготовки".to_owned()
        } else {
            "сессия не создана: отказ явный".to_owned()
        },
    );
    Ok(())
}

fn e5_stress(stand: &Stand, id: &SessionId, evidence: &mut Evidence) -> Result<(), String> {
    let prefix = format!("{id}-w");
    let workloads = (0..E5_RACERS)
        .map(|index| stand.workload(Mode::Emitter, &format!("{prefix}{index}")))
        .collect::<Result<Vec<_>, _>>()?;
    let (backend, size) = (stand.main(), stand.size);
    let started = Instant::now();
    let (ensured, listed) = thread::scope(|scope| {
        let racers: Vec<_> = workloads
            .iter()
            .map(|workload| scope.spawn(move || backend.ensure(id, workload, size)))
            .collect();
        let listers: Vec<_> = (0..E5_RACERS)
            .map(|_| scope.spawn(move || (0..E5_LISTS).map(|_| backend.list()).collect::<Vec<_>>()))
            .collect();
        let ensured: Vec<_> = racers.into_iter().map(|handle| handle.join()).collect();
        let listed: Vec<_> = listers.into_iter().map(|handle| handle.join()).collect();
        (ensured, listed)
    });
    evidence.note("elapsed_ms", started.elapsed().as_millis() as u64);
    let mut keys = Vec::new();
    let mut winners = Vec::new();
    for (index, result) in ensured.into_iter().enumerate() {
        match result.map_err(|_| "поток ensure упал".to_owned())? {
            Ok(value) => {
                if value.created {
                    winners.push(index);
                }
                keys.push(value.record.workload);
            }
            Err(err) => {
                evidence.check(&format!("ensure {index}"), false, err);
            }
        }
    }
    let mut seen = 0usize;
    let mut broken = Vec::new();
    for lister in listed {
        for result in lister.map_err(|_| "поток list упал".to_owned())? {
            match result {
                Ok(entries) => {
                    for entry in entries.iter().filter(|entry| entry.name == id.as_str()) {
                        seen += 1;
                        if let Some(error) = &entry.error {
                            broken.push(error.clone());
                        }
                    }
                }
                Err(err) => broken.push(err.to_string()),
            }
        }
    }
    evidence.note("list_saw_session", seen);
    evidence.check(
        "list в гонке без ошибок по сессии",
        broken.is_empty(),
        format!("{broken:?}"),
    );
    keys.dedup();
    evidence.check(
        "одна нагрузка у всех ensure и один победитель",
        keys.len() == 1 && winners.len() == 1,
        format!("нагрузки {keys:?}, победители {winners:?}"),
    );
    let sockets = stand
        .main()
        .socket_entries()
        .map_err(|err| format!("сокеты кандидата: {err}"))?;
    evidence.note("sockets", &sockets);
    evidence.check(
        "один сокет и пустой каталог подготовки",
        sockets
            .sessions
            .iter()
            .filter(|name| *name == id.as_str())
            .count()
            == 1
            && sockets.staged.is_empty(),
        format!("{sockets:?}"),
    );
    if let [winner] = winners.as_slice() {
        match stand.accept(Some(&format!("{prefix}{winner}"))) {
            Ok((_link, ready)) => evidence.note("ready", &ready),
            Err(err) => {
                evidence.check("нагрузка победителя готова", false, err);
            }
        }
    }
    evidence.not_proven = Some(match stand.candidate {
        Candidate::Zmx => "попадание list и ensure в окно между bind, listen и публикацией не \
                           доказано: точку остановки в zmx без патча не поставить"
            .to_owned(),
        Candidate::Rust => "стресс не доказывает попадание в окно; окно Rust-хоста доказано \
                            точками остановки, часть window"
            .to_owned(),
    });
    Ok(())
}
