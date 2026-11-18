//! Сценарий `reject` (задача 8): интеграционная часть A2. Живой GUI N поколения 1 с сессией `h1`
//! и подключённым лидером получает `update` каждым кандидатом-отказником. Части:
//!
//! 1. `live-tampered`, `live-foreign`, `live-wrong-id` — отказ без повтора до `intent` с видом
//!    отказа в причине (`integrity`; `requirement` с несовпадением листа; `requirement` с
//!    несовпадением идентификатора); журнала нет, каталогов попытки и помощника нет, истории
//!    журнала нет;
//! 2. `live-installed-untouched` — установленный бандл проходит `codesign` с CDHash поколения 1,
//!    GUI N отвечает тем же ключом и поколением;
//! 3. `live-session` — хост и нагрузка `h1` прежние, лидер получает квитанцию ввода после каждого
//!    отказа без пропуска строк;
//! 4. `live-accept-control` — принятый контроль на том же живом GUI N: кандидат `incompatible`
//!    проходит проверку и доходит до записи журнала (`staged-deferred`). Без него отказы ничего не
//!    доказывают.
//!
//! Шаги и факты — в `reject/steps.json`.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::attach::{self, Attached, Connected};
use crate::compat::Staging;
use crate::generation::Layout;
use crate::journal::State;
use crate::owner::{self, Answer, Reply, Request};
use crate::proto::{ATTACH_VERSIONS, Ack, Purpose};
use crate::pty::Winsize;
use crate::recover::Seen;
use crate::results::{Cell, Diagnostics, Environment, Report, Requirement};

use super::owner_basic::{Guard, ping};
use super::update_flow::{self, Stand};
use super::{PartSpec, Verdict};

pub const DEADLINE: Duration = Duration::from_secs(180);
pub const DIR: &str = "reject";
pub const STEPS_ARTIFACT: &str = "reject/steps.json";
const CELL: &str = "A2";
const COMPLETION: &str = "ответ владельца или хоста в срок шага (30 с); иначе ошибка исполнения";

pub const TAMPERED: PartSpec = PartSpec {
    name: "live-tampered",
    stimulus: "update кандидатом tampered живому GUI N поколения 1 с сессией h1",
    oracle: "refused без повтора, причина называет вид integrity; журнала, истории, каталогов .shellio-* и каталога помощника нет",
};
pub const FOREIGN: PartSpec = PartSpec {
    name: "live-foreign",
    stimulus: "update кандидатом foreign тому же GUI N",
    oracle: "refused без повтора, причина называет вид requirement и несовпадение листа (Signer); журнала, истории, каталогов .shellio-* и каталога помощника нет",
};
pub const WRONG: PartSpec = PartSpec {
    name: "live-wrong-id",
    stimulus: "update кандидатом wrong-id тому же GUI N",
    oracle: "refused без повтора, причина называет вид requirement и несовпадение идентификатора (Identifier); журнала, истории, каталогов .shellio-* и каталога помощника нет",
};
pub const INSTALLED: PartSpec = PartSpec {
    name: "live-installed-untouched",
    stimulus: "три отказа подряд",
    oracle: "установленный бандл проходит codesign с CDHash поколения 1 до и после; GUI N отвечает тем же ключом и поколением 1",
};
pub const SESSION: PartSpec = PartSpec {
    name: "live-session",
    stimulus: "три отказа подряд при подключённом лидере h1",
    oracle: "хост и нагрузка h1 с прежними PID и временем старта; лидер получает квитанции ack 2..4 ping после каждого отказа без пропуска строк",
};
pub const CONTROL: PartSpec = PartSpec {
    name: "live-accept-control",
    stimulus: "update кандидатом incompatible тому же GUI N после отказов",
    oracle: "кандидат проходит проверку и доходит до записи журнала: ответ staged-deferred с записью, журнал staged-deferred этой попытки",
};

fn part(spec: &PartSpec, verdict: Verdict) -> Cell {
    Cell {
        id: CELL.to_owned(),
        part: Some(spec.name.to_owned()),
        requirement: Requirement::Required,
        status: verdict.status,
        stimulus: spec.stimulus.to_owned(),
        oracle: spec.oracle.to_owned(),
        completion: COMPLETION.to_owned(),
        reason: verdict.reason,
        artifacts: vec![STEPS_ARTIFACT.to_owned()],
    }
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let (state, nonce, layout) = super::stand_inputs(environment)?;
    let dir = out.join(DIR);
    fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut guard = Guard {
        state: state.clone(),
        apps: Vec::new(),
        sessions: Vec::new(),
        nonce: Some(nonce.clone()),
        armed: true,
    };
    let mut steps = Vec::new();
    let walked = walk(&state, &nonce, &layout, &dir, &mut steps, &mut guard);
    drop(guard);
    if let Err(reason) = &walked {
        steps.push(json!({ "name": "error", "reason": reason }));
    }
    let written = crate::results::write_json(&dir, "steps.json", &steps);
    let cells = walked?;
    written?;
    Ok(Report {
        cells,
        diagnostics: Diagnostics::default(),
    })
}

/// Отказ кандидату и чистота состояния после него.
fn rejection(
    stand: &Stand,
    name: &str,
    answer: &Result<Answer, String>,
    kind: &str,
    mismatch: Option<&str>,
) -> Verdict {
    let mut v = Vec::new();
    match answer {
        Ok(Answer::Reply(Reply::Refused {
            reason,
            retry: false,
        })) if reason.contains(&format!("({kind})"))
            && mismatch.is_none_or(|word| reason.contains(word)) => {}
        other => v.push(format!("ответ на update {name}: {other:?}")),
    }
    let journal = stand.store.path();
    if journal.exists() {
        v.push(format!("журнал {} записан", journal.display()));
    }
    match stand.store.history() {
        Ok(history) if history.is_empty() => {}
        other => v.push(format!("история журнала: {other:?}")),
    }
    let dirs = stand.attempt_dirs();
    if !dirs.is_empty() {
        v.push(format!("каталоги попытки остались: {dirs:?}"));
    }
    let helpers = stand.helper_dirs();
    if !helpers.is_empty() {
        v.push(format!("каталоги помощника: {helpers:?}"));
    }
    let reason = match answer {
        Ok(Answer::Reply(Reply::Refused { reason, .. })) => reason.clone(),
        _ => String::new(),
    };
    Verdict::of(
        v,
        format!("{name} отвергнут до intent: «{reason}»; журнала нет"),
    )
}

fn walk(
    state: &Path,
    nonce: &str,
    layout: &Layout,
    out: &Path,
    steps: &mut Vec<serde_json::Value>,
    guard: &mut Guard,
) -> Result<Vec<Cell>, String> {
    let stand = Stand::new(state, nonce, layout, "gen-1")?;
    let gen1 = layout.get("gen-1")?.cdhash.clone();
    let (_, info) = stand.start_owner(guard, out, "owner-n")?;
    let record = stand.create_session(guard, "h1")?;
    let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, Winsize::cells(0, 0));
    let mut leader = match attach::connect(Path::new(&record.socket), &hello, update_flow::STEP)? {
        Connected::Welcome(client) => Attached::new(client, Some(1)),
        Connected::Refused(reason) => return Err(format!("стенд: attach h1: {reason}")),
    };
    let deadline = Instant::now() + update_flow::STEP;
    leader.pump_until(deadline, |transcript| !transcript.lines.is_empty())?;
    leader.exchange(1, "ping", deadline)?;
    let installed_before = stand.check(&stand.installed)?;
    if installed_before
        != (Seen::Valid {
            cdhash: gen1.clone(),
        })
    {
        return Err(format!(
            "стенд: установленный бандл до отказов: {installed_before:?}"
        ));
    }

    let mut cells = Vec::new();
    let mut exchanges = Vec::new();
    for (seq, (name, spec, kind, mismatch)) in [
        ("tampered", &TAMPERED, "integrity", None),
        ("foreign", &FOREIGN, "requirement", Some("Signer")),
        ("wrong-id", &WRONG, "requirement", Some("Identifier")),
    ]
    .into_iter()
    .enumerate()
    {
        let candidate = layout.get(name)?;
        let answer = owner::request(
            state,
            &Request::Update {
                candidate: candidate.app.display().to_string(),
            },
            update_flow::STEP,
        );
        let verdict = rejection(&stand, name, &answer, kind, mismatch);
        let seq = seq as u64 + 2;
        let exchanged = leader.exchange(seq, "ping", Instant::now() + update_flow::STEP);
        steps.push(json!({
            "name": name,
            "answer": format!("{answer:?}"),
            "verdict": format!("{verdict:?}"),
            "exchange": format!("{exchanged:?}"),
        }));
        exchanges.push((seq, exchanged));
        cells.push(part(spec, verdict));
    }

    let installed_after = stand.check(&stand.installed)?;
    let ping_after = ping(state);
    let mut v = Vec::new();
    if installed_after != installed_before {
        v.push(format!("установленный бандл после: {installed_after:?}"));
    }
    match &ping_after {
        Ok(after) if after.key == info.key && after.generation == 1 => {}
        other => v.push(format!("GUI N после отказов: {other:?}")),
    }
    cells.push(part(
        &INSTALLED,
        Verdict::of(
            v,
            format!(
                "установленный бандл проходит codesign с CDHash {} до и после, GUI N {} отвечает",
                &gen1[..8.min(gen1.len())],
                info.key.pid
            ),
        ),
    ));

    let same = update_flow::same_processes(&record)?;
    let mut v = Vec::new();
    if !same {
        v.push("хост или нагрузка h1 сменились".to_owned());
    }
    for (seq, exchanged) in &exchanges {
        if exchanged.is_err()
            || leader.transcript.receipt(*seq, "ping").is_none()
            || !leader.acks.contains(&(*seq, Ack::Accepted))
        {
            v.push(format!("квитанция {seq}: {exchanged:?}"));
        }
    }
    if !leader.transcript.problems.is_empty() {
        v.push(format!("пропуски: {:?}", leader.transcript.problems));
    }
    cells.push(part(
        &SESSION,
        Verdict::of(
            v,
            format!(
                "h1: хост {} и нагрузка {} прежние, квитанции ack 2..4 ping без пропуска",
                record.host.pid, record.workload.pid
            ),
        ),
    ));
    steps.push(json!({
        "name": "after",
        "installed": installed_after,
        "ping": format!("{ping_after:?}"),
        "same_processes": same,
        "transcript": leader.transcript,
        "acks": leader.acks,
    }));
    drop(leader);

    let control = owner::request(
        state,
        &Request::Update {
            candidate: layout.get("incompatible")?.app.display().to_string(),
        },
        update_flow::STEP,
    );
    let journal = stand.store.read();
    let mut v = Vec::new();
    let attempt = match &control {
        Ok(Answer::Reply(Reply::Staging(staging))) => match staging.as_ref() {
            Staging::Deferred {
                attempt,
                written: true,
                ..
            } => Some(attempt.clone()),
            _ => None,
        },
        _ => None,
    };
    match (&attempt, &journal) {
        (Some(attempt), Ok(Some(journal)))
            if journal.attempt == *attempt && journal.state == State::StagedDeferred => {}
        other => v.push(format!("контроль: ответ {control:?}, журнал {other:?}")),
    }
    steps.push(json!({
        "name": "control",
        "answer": format!("{control:?}"),
        "journal": format!("{journal:?}"),
    }));
    cells.push(part(
        &CONTROL,
        Verdict::of(
            v,
            format!(
                "incompatible прошёл проверку и записан в staged-deferred попытки {}",
                attempt.unwrap_or_default()
            ),
        ),
    ));
    let finished = update_flow::finish(&stand, guard, info.key)?;
    steps.push(json!({ "name": "finish", "facts": finished }));
    Ok(cells)
}
