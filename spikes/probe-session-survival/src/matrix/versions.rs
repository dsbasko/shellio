//! Рубеж G без бандла (G1, G2), G3 — задача 16. Ожидания обоих кандидатов записаны в отчёте до
//! прогона, раздел «Кандидаты».
//!
//! zmx: клиент текущей комбинации `zmx` и демон предыдущей `zmx-prev` той же ревизии. У `zmx-prev`
//! нет сообщений о роли, поэтому квитанция подключения — клиент в списке демона (`zmx list`), а не
//! роль «лидер»; после неё — барьер DSR через клиента. Первый attach после работы без клиентов
//! снимка не получает (A7), поэтому до сцены подключается и отключается клиент старой комбинации.
//!
//! Rust-хост: хост предыдущего поколения — `probe-session-host`, собранный с функцией `proto-v1`
//! (только замороженный протокол 1), клиент и адаптер — текущего (протоколы 1–2). G1 проверяет,
//! что текущий клиент договаривается о версии 1 и говорит ею: кадр `Input` версии 2 строгий
//! разбор v1 отверг бы. Восстановление идёт VT-байтами, поэтому формат `GHOSTSNP` в совместимость
//! не входит; G1 проверяет это по байтам кадров `Restore` хоста v1. G2 — поколение, которое
//! отказалось от версии 1 (нижняя версия протокола адаптера и клиента поднята до текущей).

use std::path::PathBuf;
use std::time::Instant;

use serde_json::json;

use crate::args::Target;
use crate::backend::rust::{self as rust_host, RustHost};
use crate::backend::zmx::{self, ZmxSet};
use crate::backend::{
    Backend, BackendError, InputOutcome, InputPath, Lead, Liveness, Role, SessionId,
};
use crate::fixture::Mode;
use crate::fixture::channel::{Event, Link, unhex};
use crate::host::attach::EXIT_INCOMPATIBLE;
use crate::launch;
use crate::matrix::Candidate;
use crate::matrix::evidence::{Evidence, Spec, cell};
use crate::matrix::stand::{
    DETACH_KEY, STOP_GRACE, Session, Stand, WAIT, barrier, compare_states, emit, same_alive,
};
use crate::oracle::model::{Outcome, compare};
use crate::oracle::vt::{Capture, Reference};
use crate::proto::{self, Versions};
use crate::results::{DefectOrigin, Environment, Report};

/// Срок ответа команды zmx без ответа демона (`Timeout` клиента zmx).
const ZMX_REPLY_TIMEOUT_MS: u64 = 2000;
const G1_INPUT: &[u8] = b"g1-input\r";
const G1_CONTROL_INPUT: &[u8] = b"g1-control\r";
/// Заголовок снимка `GHOSTSNP` (`include/ghostty/vt/snapshot.h`).
const SNAPSHOT_MAGIC: &[u8] = b"GHOSTSNP";

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    let Some(candidate) = target.backend else {
        return Err("versions идёт на кандидате".to_owned());
    };
    let mut stand = Stand::for_candidate(&target.out, environment, candidate)?;
    let outcome = match candidate {
        Candidate::Zmx => cells(&mut stand),
        Candidate::Rust => rust_cells(&mut stand, environment),
    };
    let cleanup = stand.teardown();
    let mut report = outcome?;
    report.diagnostics.notes.extend(cleanup);
    Ok(report)
}

fn cells(stand: &mut Stand) -> Result<Report, String> {
    let id = stand.session("g")?;
    let mut session = stand.ensure_on(ZmxSet::Prev, &id, Mode::Emitter)?;
    let mut g1_evidence = Evidence::default();
    g1_evidence.note("record", &session.record);
    g1_body(stand, &mut session, &mut g1_evidence)?;
    let mut g2_evidence = Evidence::default();
    g2_evidence.note("record", &session.record);
    g2_body(stand, &mut session, &mut g2_evidence)?;
    let g1_artifact = stand.artifact("g1.json", &g1_evidence)?;
    let g2_artifact = stand.artifact("g2.json", &g2_evidence)?;
    stand.discard(&id);
    let mut report = Report::default();
    report.cells.push(cell(
        Spec {
            id: "G1",
            part: None,
            stimulus: "демон zmx-prev с фикстурой; клиент zmx-prev подключается и отключается; \
                       сцена без клиента; клиент zmx подключается, вводит строку, фикстура \
                       пишет ещё"
                .to_owned(),
            oracle: "наблюдатель процессов (нагрузка и демон те же); эталон и приёмник клиента \
                     zmx после attach и после живого вывода; квитанция ввода фикстуры",
            completion: "барьеры DSR через клиентов пройдены, ввод прочитан, состояния сняты",
            origin: DefectOrigin::Attach,
        },
        &g1_evidence,
        vec![g1_artifact, stand.log_name(&id)],
    ));
    report.cells.push(cell(
        Spec {
            id: "G2",
            part: None,
            stimulus: "команды zmx screen и zmx type текущего поколения к демону zmx-prev, затем \
                       ensure текущего поколения той же сессии"
                .to_owned(),
            oracle: "ответ операции: явное состояние несовместимости или тайм-аут; наблюдатель \
                     процессов; ensure не создаёт нагрузку",
            completion: "команды вернулись, процессы проверены",
            origin: DefectOrigin::Host,
        },
        &g2_evidence,
        vec![g2_artifact, stand.log_name(&id)],
    ));
    Ok(report)
}

/// Ждёт, пока фикстура прочитает `bytes` подряд после отметки `from` журнала квитанций.
fn wait_read(link: &mut Link, from: usize, bytes: &[u8]) -> Result<(), String> {
    let mut seen: Vec<u8> = Vec::new();
    for receipt in &link.log()[from.min(link.log().len())..] {
        if let Event::Read { hex, .. } = &receipt.event {
            seen.extend(unhex(hex).unwrap_or_default());
        }
    }
    let contains = |seen: &[u8]| seen.windows(bytes.len()).any(|window| window == bytes);
    if contains(&seen) {
        return Ok(());
    }
    link.wait_for(WAIT, |receipt| {
        if let Event::Read { hex, .. } = &receipt.event {
            seen.extend(unhex(hex).unwrap_or_default());
        }
        contains(&seen).then_some(())
    })
    .map_err(|err| format!("фикстура не прочитала ввод: {err}"))
}

/// Ждёт, пока у сессии в списке демона станет `clients` клиентов: квитанция подключения клиента
/// без сообщения о роли. Опрос `zmx list` ограничен сроком `WAIT`.
fn wait_clients(stand: &Stand, id: &SessionId, clients: u32) -> Result<(), String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let liveness = stand.prev.liveness(id).map_err(|err| err.to_string())?;
        if matches!(liveness, Liveness::Live { clients: seen, .. } if seen == clients) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "у сессии нет {clients} клиентов за {} мс: {liveness:?}",
                WAIT.as_millis()
            ));
        }
    }
}

fn g1_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let mut reference = Reference::new(&stand.config).map_err(|err| format!("эталон: {err}"))?;
    let mut first = stand.client_with(ZmxSet::Prev, &session.id, "c0", Lead::IfFree, stand.size)?;
    if evidence
        .require(
            "клиент zmx-prev в списке демона",
            wait_clients(stand, &session.id, 1),
        )
        .is_none()
        || evidence
            .require(
                "барьер через клиент zmx-prev",
                barrier(&mut session.link, &first),
            )
            .is_none()
    {
        return Ok(());
    }
    first.input(DETACH_KEY)?;
    if evidence
        .require("клиент zmx-prev отключился клавишей", first.wait_exit())
        .is_none()
    {
        return Ok(());
    }
    for step in [
        b"\x1b]2;pss-g1\x07".to_vec(),
        b"\x1b[1;34mprev host\x1b[0m line\r\n".to_vec(),
        // Без пропусков в строке: пустые клетки против пробелов в снимке zmx — вывод B2 и B6, а не G1.
        b"second \x1b[7mreverse\x1b[0m".to_vec(),
    ] {
        emit(&mut session.link, &mut reference, &step)?;
    }
    let client = stand.client(&session.id, "c1")?;
    if evidence
        .require(
            "клиент zmx в списке демона zmx-prev",
            wait_clients(stand, &session.id, 1),
        )
        .is_none()
        || evidence
            .require(
                "барьер через клиент zmx",
                barrier(&mut session.link, &client),
            )
            .is_none()
    {
        return Ok(());
    }
    compare_states(evidence, "после attach", &reference, &client)?;
    let role = client.route()?.with(|rx| rx.last_role().map(str::to_owned));
    evidence.note("c1.role", &role);
    let from = session.link.log().len();
    client.input(G1_INPUT)?;
    let read = wait_read(&mut session.link, from, G1_INPUT);
    evidence.require("ввод клиента zmx дошёл до нагрузки", read);
    emit(&mut session.link, &mut reference, b"\r\nlive after attach")?;
    if evidence
        .require(
            "барьер после живого вывода",
            barrier(&mut session.link, &client),
        )
        .is_some()
    {
        compare_states(evidence, "живой поток", &reference, &client)?;
    }
    same_alive(evidence, "нагрузка та же", session.record.workload)?;
    same_alive(evidence, "демон zmx-prev тот же", session.record.host)?;
    Ok(())
}

/// Ответ команды кандидата: время и признак тайм-аута клиента zmx.
fn timed<T: std::fmt::Debug>(
    evidence: &mut Evidence,
    name: &str,
    run: impl FnOnce() -> Result<T, BackendError>,
) -> (Result<T, BackendError>, u64) {
    let started = Instant::now();
    let result = run();
    let elapsed = started.elapsed().as_millis() as u64;
    evidence.note(
        name,
        json!({ "elapsed_ms": elapsed, "result": format!("{result:?}") }),
    );
    (result, elapsed)
}

/// Явное состояние: команда вернула отказ быстрее тайм-аута клиента и не тайм-аутом.
fn explicit_refusal<T>(result: &Result<T, BackendError>, elapsed_ms: u64) -> bool {
    match result {
        Ok(_) => false,
        Err(err) => {
            let text = err.to_string();
            elapsed_ms < ZMX_REPLY_TIMEOUT_MS && !text.contains("Timeout")
        }
    }
}

fn g2_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let (screen, elapsed) = timed(evidence, "screen", || stand.zmx.snapshot(&session.id));
    evidence.check(
        "zmx screen к старому демону: явное состояние",
        explicit_refusal(&screen, elapsed),
        format!("{elapsed} мс: {screen:?}"),
    );
    let (typed, elapsed) = timed(evidence, "type", || {
        stand.zmx.input(&session.id, b"g2\r", InputPath::Typed)
    });
    evidence.check(
        "zmx type к старому демону: явное состояние",
        explicit_refusal(&typed, elapsed),
        format!("{elapsed} мс: {typed:?}"),
    );
    let workload = stand.workload(Mode::Emitter, &session.fixture_nonce)?;
    let (again, _) = timed(evidence, "ensure", || {
        stand.zmx.ensure(&session.id, &workload, stand.size)
    });
    match &again {
        Ok(ensured) => {
            // `created` считает адаптер по своим записям, а сессию создал адаптер `zmx-prev`:
            // перезапуск виден по ключу нагрузки.
            evidence.check(
                "ensure текущего поколения не перезапустил нагрузку",
                ensured.record.workload == session.record.workload,
                format!(
                    "нагрузка {:?}, до ensure {:?}",
                    ensured.record.workload, session.record.workload
                ),
            );
        }
        Err(err) => {
            evidence.check("ensure текущего поколения вернулся", false, err);
        }
    }
    let liveness = stand
        .prev
        .liveness(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.note("liveness", &liveness);
    evidence.check(
        "сессия жива",
        matches!(liveness, Liveness::Live { .. }),
        format!("{liveness:?}"),
    );
    same_alive(evidence, "нагрузка та же", session.record.workload)?;
    same_alive(evidence, "демон zmx-prev тот же", session.record.host)?;
    let log = stand.daemon_log(&session.id);
    evidence.note(
        "daemon_log",
        json!({ "leaders": zmx::leadership_events(&log), "lines": log.lines().count() }),
    );
    Ok(())
}

// --- Rust-хост ---

/// Поколения адаптера Rust-хоста в том же каталоге сокетов и с тем же токеном, что основной.
struct Generations {
    /// Запускает хост предыдущего поколения: сборка `proto-v1`.
    previous: RustHost,
    /// Поколение без версии 1: нижняя версия адаптера и его attach-клиента — текущая.
    dropped: RustHost,
}

fn generations(stand: &Stand, environment: &Environment) -> Result<Generations, String> {
    let host_v1 = environment
        .process
        .env
        .get(launch::HOST_V1)
        .map(PathBuf::from)
        .ok_or_else(|| format!("предусловие: нет {}", launch::HOST_V1))?;
    let current = stand.rust()?;
    let previous = current.with_config(rust_host::Config {
        host: host_v1,
        ..current.config().clone()
    })?;
    let dropped = current.with_config(rust_host::Config {
        versions: Versions {
            min: proto::VERSION,
            max: proto::SUPPORTED.max,
        },
        ..current.config().clone()
    })?;
    Ok(Generations { previous, dropped })
}

fn rust_cells(stand: &mut Stand, environment: &Environment) -> Result<Report, String> {
    let generations = generations(stand, environment)?;
    let id = stand.session("g")?;
    let mut session = stand.ensure_with(&generations.previous, &id, Mode::Emitter)?;
    // Предусловие: сессию держит хост предыдущего поколения. Если сборка `proto-v1` не дала
    // другого хоста, G1 сравнивал бы текущий хост с самим собой.
    let welcome = stand
        .rust()?
        .welcome(&id)
        .map_err(|err| format!("welcome хоста v1: {err}"))?;
    if welcome.version != proto::v1::VERSION {
        return Err(format!(
            "предусловие: хост из {} выбрал протокол {} для клиента {}, ожидался {}: {}",
            launch::HOST_V1,
            welcome.version,
            proto::SUPPORTED,
            proto::v1::VERSION,
            welcome.host
        ));
    }
    let mut g1_evidence = Evidence::default();
    g1_evidence.note("record", &session.record);
    g1_evidence.note("welcome", &welcome);
    rust_g1(stand, &mut session, &mut g1_evidence)?;
    let mut g2_evidence = Evidence::default();
    g2_evidence.note("record", &session.record);
    rust_g2(stand, &generations, &mut session, &mut g2_evidence)?;
    let stopped = generations
        .previous
        .stop(&id, STOP_GRACE)
        .map_err(|err| format!("остановка сессии хоста v1: {err}"))?;
    g2_evidence.note("stop", &stopped);
    let g1_artifact = stand.artifact("g1.json", &g1_evidence)?;
    let g2_artifact = stand.artifact("g2.json", &g2_evidence)?;
    let mut report = Report::default();
    report.cells.push(cell(
        Spec {
            id: "G1",
            part: None,
            stimulus:
                "хост предыдущего поколения (сборка proto-v1, протокол 1) с фикстурой; сцена \
                       без клиента; attach-клиент текущего поколения (протоколы 1–2) \
                       подключается и вводит строку; ввод управляющим соединением; кадры \
                       восстановления отдельным attach; фикстура пишет ещё; ensure текущего \
                       поколения"
                    .to_owned(),
            oracle: "журнал хоста (протокол attach-клиента), эталон и приёмник после attach и \
                     после живого вывода, VT-байты восстановления в чистом терминале против \
                     эталона и без заголовка GHOSTSNP, квитанции ввода фикстуры и InputAck, \
                     наблюдатель процессов",
            completion: "барьеры DSR через клиента пройдены, ввод прочитан, состояния сняты",
            origin: DefectOrigin::Attach,
        },
        &g1_evidence,
        vec![g1_artifact, stand.log_name(&id)],
    ));
    report.cells.push(cell(
        Spec {
            id: "G2",
            part: None,
            stimulus: "поколение без версии 1 (адаптер и attach-клиент с нижней версией \
                       протокола 2) против хоста v1: снимок, ввод, attach-клиент, ensure той же \
                       сессии, состояние"
                .to_owned(),
            oracle: "ответ операции: явное состояние несовместимости, код выхода attach-клиента; \
                     наблюдатель процессов; ensure не создаёт нагрузку",
            completion: "операции вернулись, attach-клиент вышел, процессы проверены",
            origin: DefectOrigin::Host,
        },
        &g2_evidence,
        vec![g2_artifact, stand.log_name(&id)],
    ));
    Ok(report)
}

/// Сравнение эталона с чистым терминалом, получившим только байты `bytes`.
fn compare_bytes(
    evidence: &mut Evidence,
    name: &str,
    stand: &Stand,
    reference: &Reference,
    bytes: &[u8],
) -> Result<(), String> {
    let expected = reference
        .capture(Capture::default())
        .map_err(|err| format!("состояние эталона: {err}"))?;
    let mut fresh = Reference::new(&stand.config).map_err(|err| format!("терминал: {err}"))?;
    fresh.feed(bytes);
    let actual = fresh
        .capture(Capture::default())
        .map_err(|err| format!("состояние терминала: {err}"))?;
    let comparison = compare(&expected, &actual);
    evidence.note(&format!("{name}.text"), actual.text());
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
    Ok(())
}

fn rust_g1(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let current = stand.rust()?;
    let mut reference = Reference::new(&stand.config).map_err(|err| format!("эталон: {err}"))?;
    for step in [
        b"\x1b]2;pss-g1\x07".to_vec(),
        b"\x1b[1;34mprev host\x1b[0m line\r\n".to_vec(),
        b"second \x1b[7mreverse\x1b[0m".to_vec(),
    ] {
        emit(&mut session.link, &mut reference, &step)?;
    }
    let client = stand.client_on(current, &session.id, "c1", Lead::IfFree, stand.size)?;
    let role = evidence.require(
        "клиент текущего поколения стал лидером",
        client.wait_leader(),
    );
    evidence.note("c1.role", &role);
    if role.is_none()
        || evidence
            .require("барьер через клиента", barrier(&mut session.link, &client))
            .is_none()
    {
        return Ok(());
    }
    compare_states(evidence, "после attach", &reference, &client)?;
    let protocols = rust_host::attach_protocols(&stand.daemon_log(&session.id));
    evidence.note("attach_protocols", &protocols);
    evidence.check(
        "attach-клиент текущего поколения договорился о протоколе 1",
        protocols == [(proto::v1::VERSION, proto::SUPPORTED.max)],
        format!("журнал хоста: {protocols:?}"),
    );

    // Байты восстановления отдельным attach при живом лидере: оно становится ведомым.
    let (raw, restored) = current
        .restore_frames(&session.id, stand.size)
        .map_err(|err| format!("кадры восстановления: {err}"))?;
    evidence.note("restore.welcome", &raw);
    evidence.note("restore.bytes", restored.len());
    if raw.role != Role::Follower {
        evidence.not_proven = Some(format!(
            "отдельный attach стал {:?}, а не ведомым: лидер c1 потерян",
            raw.role
        ));
        return Ok(());
    }
    evidence.check(
        "кадры восстановления пришли протоколом 1",
        raw.version == proto::v1::VERSION,
        format!("версия {}", raw.version),
    );
    let magic = restored
        .windows(SNAPSHOT_MAGIC.len())
        .any(|window| window == SNAPSHOT_MAGIC);
    evidence.check(
        "восстановление — VT-байты без заголовка GHOSTSNP",
        !restored.is_empty() && !magic,
        format!("{} байт, заголовок GHOSTSNP: {magic}", restored.len()),
    );
    compare_bytes(
        evidence,
        "VT-байты восстановления в чистом терминале",
        stand,
        &reference,
        &restored,
    )?;

    let from = session.link.log().len();
    client.input(G1_INPUT)?;
    evidence.require(
        "ввод attach-клиента текущего поколения дошёл до нагрузки",
        wait_read(&mut session.link, from, G1_INPUT),
    );
    let from = session.link.log().len();
    let outcome = current.input(&session.id, G1_CONTROL_INPUT, InputPath::Typed);
    evidence.note("control_input", format!("{outcome:?}"));
    evidence.check(
        "ввод управляющим соединением подтверждён хостом v1",
        matches!(outcome, Ok(InputOutcome::Accepted)),
        format!("{outcome:?}"),
    );
    evidence.require(
        "ввод управляющим соединением дошёл до нагрузки",
        wait_read(&mut session.link, from, G1_CONTROL_INPUT),
    );
    emit(&mut session.link, &mut reference, b"\r\nlive after attach")?;
    if evidence
        .require(
            "барьер после живого вывода",
            barrier(&mut session.link, &client),
        )
        .is_some()
    {
        compare_states(evidence, "живой поток", &reference, &client)?;
    }
    let workload = stand.workload(Mode::Emitter, &session.fixture_nonce)?;
    match current.ensure(&session.id, &workload, stand.size) {
        Ok(ensured) => {
            evidence.check(
                "ensure текущего поколения нашёл сессию хоста v1",
                !ensured.created && ensured.record.workload == session.record.workload,
                format!(
                    "created {}, нагрузка {:?}, до {:?}",
                    ensured.created, ensured.record.workload, session.record.workload
                ),
            );
        }
        Err(err) => {
            evidence.check("ensure текущего поколения вернулся", false, err);
        }
    }
    same_alive(evidence, "нагрузка та же", session.record.workload)?;
    same_alive(evidence, "хост v1 тот же", session.record.host)?;
    Ok(())
}

/// Явное состояние несовместимости вместо отказа другого рода или успеха.
fn incompatible<T: std::fmt::Debug>(result: &Result<T, BackendError>) -> bool {
    matches!(result, Err(BackendError::Incompatible(_)))
}

fn rust_g2(
    stand: &Stand,
    generations: &Generations,
    session: &mut Session,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let current = stand.rust()?;
    let dropped = &generations.dropped;
    let id = &session.id;
    let before = current
        .status(id)
        .map_err(|err| format!("состояние хоста v1: {err}"))?;
    evidence.note("status_before", &before);
    let processes_before = launch::find_by_token(id.as_str()).len();
    let mark = session.link.log().len();

    let (snapshot, _) = timed(evidence, "snapshot", || dropped.snapshot(id));
    evidence.check(
        "снимок поколения без v1: явное состояние несовместимости",
        incompatible(&snapshot),
        format!("{snapshot:?}"),
    );
    let (input, _) = timed(evidence, "input", || {
        dropped.input(id, b"g2\r", InputPath::Typed)
    });
    evidence.check(
        "ввод поколения без v1: явное состояние несовместимости",
        incompatible(&input),
        format!("{input:?}"),
    );
    let started = Instant::now();
    let mut client = stand.client_on(dropped, id, "c2", Lead::IfFree, stand.size)?;
    let exit = client.wait_exit();
    evidence.note(
        "attach",
        json!({
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "exit": format!("{exit:?}"),
        }),
    );
    evidence.check(
        "attach-клиент поколения без v1 вышел с кодом несовместимости",
        matches!(&exit, Ok(status) if status.code() == Some(EXIT_INCOMPATIBLE)),
        format!("{exit:?}"),
    );
    drop(client);
    let workload = stand.workload(Mode::Emitter, &session.fixture_nonce)?;
    let (again, _) = timed(evidence, "ensure", || {
        dropped.ensure(id, &workload, stand.size)
    });
    evidence.check(
        "ensure поколения без v1: явное состояние, нагрузка не запускается",
        incompatible(&again),
        format!("{again:?}"),
    );
    let liveness = dropped.liveness(id).map_err(|err| err.to_string())?;
    evidence.note("liveness", &liveness);
    evidence.check(
        "состояние сессии для поколения без v1 — несовместимость",
        matches!(liveness, Liveness::Incompatible { .. }),
        format!("{liveness:?}"),
    );
    let processes_after = launch::find_by_token(id.as_str()).len();
    evidence.check(
        "процессов сессии не прибавилось",
        processes_after == processes_before,
        format!("до {processes_before}, после {processes_after}"),
    );
    same_alive(evidence, "нагрузка та же", session.record.workload)?;
    same_alive(evidence, "хост v1 тот же", session.record.host)?;
    let after = current
        .status(id)
        .map_err(|err| format!("состояние хоста v1: {err}"))?;
    evidence.note("status_after", &after);
    evidence.check(
        "текущее поколение по-прежнему говорит с хостом v1",
        after.workload == before.workload,
        format!("{after:?}"),
    );
    // Наблюдение, а не условие клетки: хост выбирает версию раньше, чем клиент её проверит,
    // поэтому отвергнутый attach успевает стать лидером.
    let winches = session.link.log()[mark.min(session.link.log().len())..]
        .iter()
        .filter(|receipt| matches!(receipt.event, Event::Winch { .. }))
        .count();
    evidence.note(
        "rejected_attach_traces",
        json!({
            "generation_before": before.generation,
            "generation_after": after.generation,
            "clients_after": after.clients,
            "winch_receipts": winches,
        }),
    );
    Ok(())
}
