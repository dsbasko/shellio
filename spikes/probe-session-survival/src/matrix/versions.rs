//! Рубеж G без бандла: клиент текущей комбинации `zmx` и демон предыдущей `zmx-prev` той же
//! ревизии (G1, G2). Ожидаемое поведение каждой команды записано в отчёте до прогона, раздел
//! «Отличия `zmx-prev`, записанные до прогона». G3 — задача 16.
//!
//! У `zmx-prev` нет сообщений о роли, поэтому квитанция подключения — клиент в списке демона
//! (`zmx list`), а не роль «лидер»; после неё — барьер DSR через клиента. Первый attach после работы без клиентов снимка не получает (A7), поэтому до
//! сцены подключается и отключается клиент старой комбинации.

use std::time::Instant;

use serde_json::json;

use crate::args::Target;
use crate::backend::zmx::{self, ZmxSet};
use crate::backend::{Backend, BackendError, InputPath, Lead, Liveness, SessionId};
use crate::fixture::Mode;
use crate::fixture::channel::{Event, Link, unhex};
use crate::matrix::Candidate;
use crate::matrix::evidence::{Evidence, Spec, cell};
use crate::matrix::stand::{
    DETACH_KEY, Session, Stand, WAIT, barrier, compare_states, emit, same_alive,
};
use crate::oracle::vt::Reference;
use crate::results::{DefectOrigin, Environment, Report};

/// Срок ответа команды zmx без ответа демона (`Timeout` клиента zmx).
const ZMX_REPLY_TIMEOUT_MS: u64 = 2000;
const G1_INPUT: &[u8] = b"g1-input\r";

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    match target.backend {
        Some(Candidate::Zmx) => {}
        other => return Err(format!("versions идёт на zmx, а не на {other:?}")),
    }
    let mut stand = Stand::new(&target.out, environment)?;
    let outcome = cells(&mut stand);
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
