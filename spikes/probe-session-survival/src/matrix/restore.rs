//! Рубеж B — восстановление экрана на zmx, клетки B1–B9.
//!
//! Сцены и суффиксы — те же, что у ранней проверки C API (`capi`), поэтому итог zmx сопоставим с
//! таблицей возможностей C API. Первый attach после работы без единого клиента у zmx снимка не
//! получает (A7), поэтому каждая клетка сначала подключает и отключает клиента c1: сцена идёт без
//! клиента, восстанавливает её второй клиент c2. Эталон получает исходные байты фикстуры, приёмник
//! c2 — только то, что отдал attach-клиент.

use std::time::Instant;

use serde_json::json;

use crate::args::Target;
use crate::backend::Backend;
use crate::fixture::channel::{Command, Event, Receipt, hex};
use crate::fixture::{Mode, flood_line};
use crate::matrix::Candidate;
use crate::matrix::capi::{self, B7_VARIANTS, Story};
use crate::matrix::evidence::{Evidence, Spec, cell};
use crate::matrix::stand::{
    COLS, Client, DETACH_KEY, Session, Stand, WAIT, barrier, compare_states, emit,
};
use crate::matrix::util;
use crate::oracle::model::Outcome;
use crate::oracle::suffix::{self, Sink, Step};
use crate::oracle::vt::{Capture, Reference, ThroughCandidate};
use crate::results::{Cell, DefectOrigin, Environment, Report};
use crate::sys;

/// Строк потока B8: при темпе [`B8_INTERVAL_US`] поток идёт около двух секунд, attach c2 попадает
/// внутрь него.
const B8_LINES: u64 = 4000;
const B8_INTERVAL_US: u64 = 500;
/// Уже окна: строки потока не переносятся, номер строки читается по строке экрана.
const B8_WIDTH: u16 = COLS - 2;

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    match target.backend {
        Some(Candidate::Zmx) => {}
        other => return Err(format!("restore идёт на zmx, а не на {other:?}")),
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
    for story in capi::stories() {
        report.cells.push(story_cell(stand, &story)?);
    }
    report.cells.extend(b7(stand)?);
    report.cells.push(b8(stand)?);
    report.cells.sort_by_key(|cell| super::cell_order(&cell.id));
    Ok(report)
}

// --- общие шаги ---

/// Клиент c1 подключается, становится лидером и отключается клавишей: дальше сессия без клиентов,
/// но уже имела клиента. `false` — отказ кандидата записан в доказательства.
fn prime(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<bool, String> {
    let mut first = stand.client(&session.id, "c1")?;
    if evidence.require("c1 лидер", first.wait_leader()).is_none()
        || evidence
            .require("барьер через c1", barrier(&mut session.link, &first))
            .is_none()
    {
        return Ok(false);
    }
    first.input(DETACH_KEY)?;
    Ok(evidence
        .require("c1 отключился клавишей", first.wait_exit())
        .is_some())
}

/// Второй клиент после сцены без клиентов. С `with_barrier` — и барьер DSR через него.
fn second(
    stand: &Stand,
    session: &mut Session,
    evidence: &mut Evidence,
    with_barrier: bool,
) -> Result<Option<Client>, String> {
    let client = stand.client(&session.id, "c2")?;
    if evidence.require("c2 лидер", client.wait_leader()).is_none() {
        return Ok(None);
    }
    if with_barrier
        && evidence
            .require("барьер через c2", barrier(&mut session.link, &client))
            .is_none()
    {
        return Ok(None);
    }
    Ok(Some(client))
}

/// Суффикс приёмнику через кандидата и эталону напрямую, затем сравнение. `false` — кандидат не
/// пропустил шаг суффикса или состояния разошлись.
fn suffix_step(
    session: &mut Session,
    client: &Client,
    reference: &mut Reference,
    evidence: &mut Evidence,
    name: &str,
    steps: &[Step],
) -> Result<bool, String> {
    let mut through = ThroughCandidate {
        link: &mut session.link,
        route: client.route()?,
        timeout: WAIT,
    };
    if evidence
        .require(
            &format!("суффикс {name} через кандидата"),
            suffix::apply(steps, &mut through),
        )
        .is_none()
    {
        return Ok(false);
    }
    suffix::apply(steps, reference as &mut dyn Sink)
        .map_err(|err| format!("суффикс {name} эталону: {err}"))?;
    let outcome = compare_states(
        evidence,
        &format!("после суффикса {name}"),
        reference,
        client,
    )?;
    Ok(outcome != Outcome::Differs)
}

fn finish(
    stand: &Stand,
    session: &Session,
    spec: Spec<'_>,
    evidence: &Evidence,
    artifact: &str,
) -> Result<Cell, String> {
    let artifact = stand.artifact(artifact, evidence)?;
    stand.discard(&session.id);
    Ok(cell(
        spec,
        evidence,
        vec![artifact, stand.log_name(&session.id)],
    ))
}

// --- B1–B6, B9 ---

fn story_cell(stand: &mut Stand, story: &Story) -> Result<Cell, String> {
    let label = story.cell.to_lowercase();
    let id = stand.session(&label)?;
    let mut session = stand.ensure(&id, Mode::Emitter)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    story_body(stand, &mut session, story, &mut evidence)?;
    let suffixes: Vec<&str> = story
        .suffixes
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    finish(
        stand,
        &session,
        Spec {
            id: story.cell,
            part: None,
            stimulus: format!(
                "c1 подключился и отключился; без клиентов: {}; затем attach c2; суффиксы через \
                 кандидата: {}",
                story.stimulus,
                suffixes.join(", ")
            ),
            oracle: "эталон и приёмник c2 сразу после attach и после каждого суффикса",
            completion: "c2 лидер, барьер DSR через c2 после attach и после каждого шага суффикса, \
                         состояния сняты",
            // Снимок zmx — formatter его ghostty 1.3.2-dev; те же потери у formatter 1.4 в
            // сценарии capi.
            origin: DefectOrigin::Vt,
        },
        &evidence,
        &format!("{label}.json"),
    )
}

fn story_body(
    stand: &Stand,
    session: &mut Session,
    story: &Story,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let mut reference = Reference::new(&stand.config).map_err(|err| format!("эталон: {err}"))?;
    if !prime(stand, session, evidence)? {
        return Ok(());
    }
    for step in &story.scene {
        emit(&mut session.link, &mut reference, step)?;
    }
    let Some(client) = second(stand, session, evidence, true)? else {
        return Ok(());
    };
    compare_states(evidence, "после attach", &reference, &client)?;
    for (name, steps) in &story.suffixes {
        if !suffix_step(session, &client, &mut reference, evidence, name, steps)? {
            break;
        }
    }
    Ok(())
}

// --- B7: незавершённая последовательность в точке снимка ---

/// Ждёт роста ревизии обработки после `before` не дольше [`WAIT`]: опрос `zmx screen`.
fn revision_after(stand: &Stand, session: &Session, before: u64) -> Result<u64, String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let now = stand
            .zmx
            .read_boundary(&session.id)
            .map_err(|err| format!("ревизия обработки: {err}"))?;
        if now != before || Instant::now() >= deadline {
            return Ok(now);
        }
    }
}

/// Ревизия обработки, когда снимок хоста уже содержит `marker`: всё записанное до разреза
/// прочитано. `None` — за [`WAIT`] не появился.
fn processed(stand: &Stand, session: &Session, marker: &str) -> Result<Option<u64>, String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = stand
            .zmx
            .snapshot(&session.id)
            .map_err(|err| format!("zmx screen: {err}"))?;
        if snapshot.text.contains(marker) {
            return Ok(Some(snapshot.revision));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
    }
}

/// Квитанции, которые нарушают предусловие разреза: чтение ввода, `SIGWINCH`, сигналы.
fn cut_violations(receipts: &[Receipt]) -> Vec<String> {
    receipts
        .iter()
        .filter_map(|receipt| match &receipt.event {
            Event::Read { index, .. } => Some(format!("фикстура прочитала ввод #{index}")),
            Event::Winch { size } => Some(format!("SIGWINCH {size:?}")),
            Event::Signal { signal } => Some(format!("сигнал {signal}")),
            _ => None,
        })
        .collect()
}

fn b7(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    for (variant, prefix, _, rest) in B7_VARIANTS {
        let id = stand.session(&format!("b7{variant}"))?;
        let mut session = stand.ensure(&id, Mode::Emitter)?;
        let mut evidence = Evidence::default();
        evidence.note("record", &session.record);
        evidence.note("prefix", hex(prefix));
        evidence.note("rest", hex(rest));
        b7_body(stand, &mut session, prefix, rest, &mut evidence)?;
        cells.push(finish(
            stand,
            &session,
            Spec {
                id: "B7",
                part: Some(variant),
                stimulus: format!(
                    "c1 подключился и отключился; без клиентов фикстура пишет строку и префикс \
                     {variant} ({}), хост его прочитал (ревизия +1); attach c2 посреди \
                     последовательности; фикстура пишет остаток ({})",
                    hex(prefix),
                    hex(rest)
                ),
                oracle: "эталон и приёмник c2 после остатка: суффикс — сам остаток \
                         последовательности",
                completion: "ревизия обработки выросла ровно на единицу без других событий \
                             фикстуры, c2 лидер, барьер DSR через c2 после остатка",
                origin: DefectOrigin::Host,
            },
            &evidence,
            &format!("b7-{variant}.json"),
        )?);
    }
    Ok(cells)
}

fn b7_body(
    stand: &Stand,
    session: &mut Session,
    prefix: &[u8],
    rest: &[u8],
    evidence: &mut Evidence,
) -> Result<(), String> {
    let mut reference = Reference::new(&stand.config).map_err(|err| format!("эталон: {err}"))?;
    if !prime(stand, session, evidence)? {
        return Ok(());
    }
    emit(&mut session.link, &mut reference, b"before the cut\r\n")?;
    let Some(before) = processed(stand, session, "before the cut")? else {
        evidence.not_proven = Some("хост не показал строку перед разрезом".to_owned());
        return Ok(());
    };
    let mark = session.link.log().len();
    emit(&mut session.link, &mut reference, prefix)?;
    let after = revision_after(stand, session, before)?;
    let violations = cut_violations(&session.link.log()[mark..]);
    evidence.note(
        "revision",
        json!({ "before": before, "after": after, "violations": violations }),
    );
    if let Err(reason) = util::revision_boundary(before, after, &violations) {
        evidence.not_proven = Some(format!("граница разреза не доказана: {reason}"));
        return Ok(());
    }
    let Some(client) = second(stand, session, evidence, false)? else {
        return Ok(());
    };
    emit(&mut session.link, &mut reference, rest)?;
    if evidence
        .require("барьер через c2", barrier(&mut session.link, &client))
        .is_none()
    {
        return Ok(());
    }
    compare_states(evidence, "после остатка", &reference, &client)?;
    Ok(())
}

// --- B8: снимок и живой поток под непрерывным выводом ---

fn b8(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("b8")?;
    let mut session = stand.ensure(&id, Mode::Flood)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    b8_body(stand, &mut session, &mut evidence)?;
    finish(
        stand,
        &session,
        Spec {
            id: "B8",
            part: None,
            stimulus: format!(
                "c1 подключился и отключился; без клиентов фикстура начинает поток {B8_LINES} \
                 нумерованных строк по {B8_WIDTH} колонок с темпом строка в {B8_INTERVAL_US} мкс; \
                 после первой квитанции хода подключается c2"
            ),
            oracle: "номера строк потока у приёмника c2 идут подряд без пропуска и повтора до \
                     последней; активная область совпадает с эталоном",
            completion: "квитанция конца потока позже, чем c2 стал лидером (attach внутри потока), \
                         барьер DSR через c2",
            origin: DefectOrigin::Host,
        },
        &evidence,
        "b8.json",
    )
}

fn b8_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let mut reference = Reference::new(&stand.config).map_err(|err| format!("эталон: {err}"))?;
    if !prime(stand, session, evidence)? {
        return Ok(());
    }
    session
        .link
        .request(
            Command::Flood {
                lines: B8_LINES,
                width: B8_WIDTH,
                interval_us: B8_INTERVAL_US,
                report_every: B8_LINES / 20,
            },
            WAIT,
        )
        .map_err(|err| format!("команда потока: {err}"))?;
    let first_progress = session
        .link
        .wait_for(WAIT, |receipt| match &receipt.event {
            Event::FloodProgress { lines, .. } => Some((receipt.t_ns, *lines)),
            _ => None,
        })
        .map_err(|err| format!("квитанция хода потока: {err}"))?;
    let spawned_ns = sys::uptime_ns();
    let client = stand.client(&session.id, "c2")?;
    let leader = evidence.require("c2 лидер", client.wait_leader());
    let leader_ns = sys::uptime_ns();
    let done = session
        .link
        .wait_for(WAIT * 4, |receipt| match &receipt.event {
            Event::FloodDone {
                lines, cancelled, ..
            } => Some((receipt.t_ns, *lines, *cancelled)),
            _ => None,
        })
        .map_err(|err| format!("конец потока: {err}"))?;
    let (done_ns, lines, cancelled) = done;
    evidence.note(
        "timeline_ns",
        json!({
            "first_progress": first_progress.0,
            "first_progress_lines": first_progress.1,
            "c2_spawned": spawned_ns,
            "c2_leader": leader_ns,
            "flood_done": done_ns,
        }),
    );
    if leader.is_none() {
        return Ok(());
    }
    if lines != B8_LINES || cancelled {
        return Err(format!(
            "предусловие: поток не дописан: строк {lines}, отменён {cancelled}"
        ));
    }
    for index in 1..=B8_LINES {
        reference.feed(&flood_line(&session.fixture_nonce, index, B8_WIDTH));
    }
    if evidence
        .require("барьер через c2", barrier(&mut session.link, &client))
        .is_none()
    {
        return Ok(());
    }
    let capture = Capture {
        history: usize::MAX,
        hidden: false,
    };
    let expected = reference
        .capture(capture)
        .map_err(|err| format!("состояние эталона: {err}"))?;
    let actual = client
        .route()?
        .with(|rx| rx.capture(capture))
        .map_err(|err| format!("состояние приёмника c2: {err}"))?;
    let sequence = capi::flood_sequence(&session.fixture_nonce, &actual);
    let problems = capi::sequence_problems(&sequence, B8_LINES);
    evidence.note(
        "sequence",
        json!({
            "rows": sequence.len(),
            "first": sequence.first(),
            "last": sequence.last(),
            "history_total": actual.history_total,
        }),
    );
    evidence.check(
        "номера строк у c2 подряд до последней",
        problems.is_empty(),
        if problems.is_empty() {
            format!("{} строк подряд", sequence.len())
        } else {
            problems.join("; ")
        },
    );
    let active: Vec<String> = expected
        .active
        .iter()
        .zip(&actual.active)
        .enumerate()
        .filter(|(_, (a, b))| a.text() != b.text())
        .map(|(y, (a, b))| format!("строка {y}: «{}» против «{}»", a.text(), b.text()))
        .collect();
    evidence.check(
        "активная область c2 совпадает с эталоном",
        active.is_empty(),
        if active.is_empty() {
            "совпала".to_owned()
        } else {
            active.join("; ")
        },
    );
    if first_progress.0 >= spawned_ns || done_ns <= leader_ns {
        evidence.not_proven = Some(format!(
            "attach не доказан внутри потока: первый ход {} нс, запуск c2 {spawned_ns} нс, лидер \
             {leader_ns} нс, конец потока {done_ns} нс",
            first_progress.0
        ));
    }
    Ok(())
}
