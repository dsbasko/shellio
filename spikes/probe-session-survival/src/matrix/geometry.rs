//! Рубеж F без поверхности на zmx текущей комбинации: источник размера при двух клиентах разного
//! размера и устаревший ресайз прежнего лидера (F1), ввод из CLI без захвата лидерства и размера
//! (F3). F2 и F4 требуют поверхности и ручной приёмки — задачи 15 и 19.
//!
//! Оракул — квитанции `SIGWINCH` фикстуры с `TIOCGWINSZ`. Отсутствие ресайза доказывается
//! следующим упорядоченным событием: после проверяемого шага ресайз делает тот, чей размер должен
//! пройти, и между отметкой и его квитанцией других квитанций размера быть не должно.

use serde_json::json;

use crate::args::Target;
use crate::backend::zmx::{self, Role, ZmxSet};
use crate::backend::{Backend, InputPath, Lead, Liveness};
use crate::fixture::Mode;
use crate::fixture::channel::{Event, Link};
use crate::matrix::Candidate;
use crate::matrix::evidence::{Evidence, Spec, cell};
use crate::matrix::stand::{Client, Session, Stand, WAIT};
use crate::pty::Winsize;
use crate::results::{Cell, DefectOrigin, Environment, Report};

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    match target.backend {
        Some(Candidate::Zmx) => {}
        other => return Err(format!("geometry идёт на zmx, а не на {other:?}")),
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
    report.cells.push(f1(stand)?);
    report.cells.push(f3(stand)?);
    Ok(report)
}

/// Ждёт сообщения о роли `role` от управляемого клиента.
fn wait_role(client: &Client, role: Role) -> Result<String, String> {
    let route = client.route()?;
    route
        .wait_until(WAIT, |rx, _| {
            rx.last_role()
                .and_then(zmx::parse_role)
                .is_some_and(|report| report.role == role)
        })
        .map_err(|err| format!("клиент {} не получил роль {role:?}: {err}", client.label))?;
    Ok(route
        .with(|rx| rx.last_role().map(str::to_owned))
        .unwrap_or_default())
}

/// Размеры из квитанций `SIGWINCH`, пришедших после отметки `from` журнала квитанций.
fn winches_since(link: &Link, from: usize) -> Vec<Option<Winsize>> {
    link.log()[from.min(link.log().len())..]
        .iter()
        .filter_map(|receipt| match &receipt.event {
            Event::Winch { size } => Some(*size),
            _ => None,
        })
        .collect()
}

/// Ждёт квитанции `SIGWINCH` с размером `cols × rows`.
fn wait_winch(link: &mut Link, cols: u16, rows: u16) -> Result<(), String> {
    link.wait_for(WAIT, |receipt| match &receipt.event {
        Event::Winch { size: Some(size) } if size.cols == cols && size.rows == rows => Some(()),
        _ => None,
    })
    .map_err(|err| format!("нагрузка не получила размер {cols}×{rows}: {err}"))
}

fn sizes(winches: &[Option<Winsize>]) -> Vec<String> {
    winches
        .iter()
        .map(|size| match size {
            Some(size) => format!("{}×{}", size.cols, size.rows),
            None => "нет TIOCGWINSZ".to_owned(),
        })
        .collect()
}

/// Ресайз окна клиента `actor` и следующий упорядоченный шаг `witness`: размер свидетеля должен
/// дойти до нагрузки, а размер `actor` — нет, если `actor` не источник размера.
struct Step<'a> {
    name: &'a str,
    actor: &'a Client,
    actor_size: (u16, u16),
    witness: &'a Client,
    witness_size: (u16, u16),
}

/// Проверка «размер `actor` до нагрузки не дошёл». Отметка — до ресайза `actor`; ресайз свидетеля
/// идёт после того, как ресайз `actor` прочитан его клиентом (размер PTY клиента сменился, и клиент
/// получил `SIGWINCH` раньше свидетеля), поэтому квитанция свидетеля упорядочена после шага.
fn ignored_resize(evidence: &mut Evidence, link: &mut Link, step: Step<'_>) -> Result<(), String> {
    let mark = link.log().len();
    step.actor
        .route()?
        .resize(step.actor_size.0, step.actor_size.1)?;
    step.witness
        .route()?
        .resize(step.witness_size.0, step.witness_size.1)?;
    let arrived = wait_winch(link, step.witness_size.0, step.witness_size.1);
    let winches = winches_since(link, mark);
    evidence.note(&format!("{}.winches", step.name), sizes(&winches));
    if evidence
        .require(
            &format!(
                "{}: размер свидетеля {} дошёл",
                step.name, step.witness.label
            ),
            arrived,
        )
        .is_none()
    {
        return Ok(());
    }
    let leaked = winches
        .iter()
        .any(|size| size.is_some_and(|size| (size.cols, size.rows) == step.actor_size));
    evidence.check(
        &format!("{}: размер {} не дошёл", step.name, step.actor.label),
        !leaked,
        format!("квитанции размера после отметки: {:?}", sizes(&winches)),
    );
    Ok(())
}

// --- F1 ---

const F1_C1: (u16, u16) = (80, 24);
const F1_C2: (u16, u16) = (100, 30);
const F1_C3: (u16, u16) = (110, 32);

fn f1(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("f1")?;
    let mut session = stand.ensure(&id, Mode::Emitter)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    f1_body(stand, &mut session, &mut evidence)?;
    let artifact = stand.artifact("f1.json", &evidence)?;
    stand.discard(&id);
    Ok(cell(
        Spec {
            id: "F1",
            part: None,
            stimulus: format!(
                "c1 {}×{} ведёт; c2 {}×{} подключается без захвата и меняет размер; c3 {}×{} \
                 забирает лидерство; прежний лидер c1 меняет размер (устаревший ресайз)",
                F1_C1.0, F1_C1.1, F1_C2.0, F1_C2.1, F1_C3.0, F1_C3.1
            ),
            oracle: "квитанции SIGWINCH фикстуры с TIOCGWINSZ; сообщения о роли клиентов; \
                     отсутствие размера — до квитанции следующего ресайза лидера",
            completion: "квитанция размера каждого шага лидера получена, роли сняты",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ))
}

fn f1_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let size = |(cols, rows): (u16, u16)| stand.config.winsize(cols, rows);
    let c1 = stand.client_with(
        ZmxSet::Current,
        &session.id,
        "c1",
        Lead::IfFree,
        size(F1_C1),
    )?;
    if evidence
        .require("c1 лидер", wait_role(&c1, Role::Leader))
        .is_none()
    {
        return Ok(());
    }
    let mark = session.link.log().len();
    let c2 = stand.client_with(
        ZmxSet::Current,
        &session.id,
        "c2",
        Lead::IfFree,
        size(F1_C2),
    )?;
    if evidence
        .require("c2 ведомый", wait_role(&c2, Role::Follower))
        .is_none()
    {
        return Ok(());
    }
    // Подключение c2 не должно менять размер: свидетель — ресайз лидера c1.
    let witness = (F1_C1.0 + 1, F1_C1.1);
    c1.route()?.resize(witness.0, witness.1)?;
    let arrived = wait_winch(&mut session.link, witness.0, witness.1);
    let winches = winches_since(&session.link, mark);
    evidence.note("attach-c2.winches", sizes(&winches));
    if evidence
        .require("подключение c2: размер лидера c1 дошёл", arrived)
        .is_none()
    {
        return Ok(());
    }
    evidence.check(
        "подключение c2: размер c2 не дошёл",
        !winches
            .iter()
            .any(|size| size.is_some_and(|size| (size.cols, size.rows) == F1_C2)),
        format!("квитанции размера: {:?}", sizes(&winches)),
    );
    ignored_resize(
        evidence,
        &mut session.link,
        Step {
            name: "ресайз ведомого c2",
            actor: &c2,
            actor_size: (F1_C2.0 + 2, F1_C2.1 + 1),
            witness: &c1,
            witness_size: (witness.0 + 1, witness.1),
        },
    )?;
    let c3 = stand.client_with(ZmxSet::Current, &session.id, "c3", Lead::Claim, size(F1_C3))?;
    if evidence
        .require("c3 забрал лидерство", wait_role(&c3, Role::Leader))
        .is_none()
    {
        return Ok(());
    }
    let claimed = wait_winch(&mut session.link, F1_C3.0, F1_C3.1);
    if evidence
        .require("захват c3: нагрузка получила размер c3", claimed)
        .is_none()
    {
        return Ok(());
    }
    let demoted = wait_role(&c1, Role::Follower);
    evidence.require("c1 стал ведомым", demoted);
    ignored_resize(
        evidence,
        &mut session.link,
        Step {
            name: "устаревший ресайз прежнего лидера c1",
            actor: &c1,
            actor_size: (F1_C1.0 - 10, F1_C1.1 - 4),
            witness: &c3,
            witness_size: (F1_C3.0 + 1, F1_C3.1),
        },
    )?;
    let liveness = stand
        .zmx
        .liveness(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.note("liveness", &liveness);
    evidence.check(
        "три клиента подключены",
        matches!(liveness, Liveness::Live { clients: 3, .. }),
        format!("{liveness:?}"),
    );
    Ok(())
}

// --- F3 ---

const F3_TEXT: &[u8] = b"cli-input\r";

fn f3(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("f3")?;
    let mut session = stand.ensure(&id, Mode::Emitter)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    f3_body(stand, &mut session, &mut evidence)?;
    let artifact = stand.artifact("f3.json", &evidence)?;
    stand.discard(&id);
    Ok(cell(
        Spec {
            id: "F3",
            part: None,
            stimulus: "лидер c1 80×24; ввод из CLI обоими путями: zmx send и zmx type; затем \
                       ресайз лидера"
                .to_owned(),
            oracle: "квитанции прочитанных байтов фикстуры; квитанции SIGWINCH; роль c1 и \
                     назначения лидера в журнале демона; число клиентов",
            completion: "фикстура прочитала ввод каждого пути, квитанция ресайза лидера получена",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ))
}

/// Ждёт, пока фикстура прочитает `bytes` подряд в своих чтениях после отметки `from`.
fn wait_read(link: &mut Link, from: usize, bytes: &[u8]) -> Result<(), String> {
    let mut seen = Vec::new();
    for receipt in &link.log()[from.min(link.log().len())..] {
        if let Event::Read { hex, .. } = &receipt.event {
            seen.extend(crate::fixture::channel::unhex(hex).unwrap_or_default());
        }
    }
    let contains = |seen: &[u8]| seen.windows(bytes.len()).any(|window| window == bytes);
    if contains(&seen) {
        return Ok(());
    }
    link.wait_for(WAIT, |receipt| {
        if let Event::Read { hex, .. } = &receipt.event {
            seen.extend(crate::fixture::channel::unhex(hex).unwrap_or_default());
        }
        contains(&seen).then_some(())
    })
    .map_err(|err| {
        format!(
            "фикстура не прочитала {:?}: {err}",
            String::from_utf8_lossy(bytes)
        )
    })
}

fn f3_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let c1 = stand.client(&session.id, "c1")?;
    let Some(role) = evidence.require("c1 лидер", c1.wait_leader()) else {
        return Ok(());
    };
    let leaders_before = zmx::leadership_events(&stand.daemon_log(&session.id));
    let mark = session.link.log().len();
    for (name, path, text) in [
        ("send", InputPath::Raw, F3_TEXT.to_vec()),
        ("type", InputPath::Typed, b"typed-input\r".to_vec()),
    ] {
        let from = session.link.log().len();
        let outcome = stand.zmx.input(&session.id, &text, path);
        evidence.note(&format!("{name}.outcome"), format!("{outcome:?}"));
        if evidence
            .require(&format!("zmx {name} принят"), outcome)
            .is_none()
        {
            continue;
        }
        let read = wait_read(&mut session.link, from, &text);
        evidence.require(&format!("фикстура прочитала ввод zmx {name}"), read);
    }
    let witness = (81, 24);
    c1.route()?.resize(witness.0, witness.1)?;
    let arrived = wait_winch(&mut session.link, witness.0, witness.1);
    let winches = winches_since(&session.link, mark);
    evidence.note("winches", sizes(&winches));
    if evidence
        .require("ресайз лидера после ввода дошёл", arrived)
        .is_none()
    {
        return Ok(());
    }
    evidence.check(
        "ввод из CLI не менял размер",
        winches.len() == 1,
        format!("квитанции размера после отметки: {:?}", sizes(&winches)),
    );
    let role_after = c1
        .route()?
        .with(|rx| rx.last_role().map(str::to_owned))
        .unwrap_or_default();
    let leaders_after = zmx::leadership_events(&stand.daemon_log(&session.id));
    evidence.note(
        "leadership",
        json!({
            "role_before": role,
            "role_after": role_after,
            "leaders_before": leaders_before,
            "leaders_after": leaders_after,
        }),
    );
    evidence.check(
        "ввод из CLI не перехватил лидерство",
        role_after == role && leaders_after == leaders_before,
        format!(
            "роль c1 {role} → {role_after}, назначений лидера {leaders_before} → {leaders_after}"
        ),
    );
    let liveness = stand
        .zmx
        .liveness(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.note("liveness", &liveness);
    evidence.check(
        "ввод из CLI не стал клиентом",
        matches!(liveness, Liveness::Live { clients: 1, .. }),
        format!("{liveness:?}"),
    );
    Ok(())
}
