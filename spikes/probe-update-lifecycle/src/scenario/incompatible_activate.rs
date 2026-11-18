//! Сценарий `incompatible-activate` (задача 8): клетка D2, активация отложенного кандидата.
//! Установленный N — поколение 1 с живой сессией `h1` (хост 1..1). `update` кандидатом
//! `incompatible` (поколение 3, attach 2..2) откладывается, после остановки `h1` повтор ставит его
//! в `staged`, `activate` проводит замену. Часть `activation`:
//!
//! - N+1 поколения 3 готов, CDHash процесса — кандидата, установлен кандидат;
//! - история попытки: `staged-deferred → staged → intent → swapped → launched → committed →
//!   cleaning → idle` («подтверждена»);
//! - health N+1 без нарушений при пустом снимке сессий;
//! - рантайм `runtime/3/` подготовлен, новая сессия `h2` идёт от хоста из `runtime/3/` с
//!   протоколом 2..2.
//!
//! Шаги и факты — в `incompatible-activate/steps.json`.

use std::fs;
use std::path::Path;
use std::time::Duration;

use serde_json::json;

use crate::compat::Staging;
use crate::generation::{Layout, Part};
use crate::journal::{Outcome, State};
use crate::owner::{Reply, Request};
use crate::proto::Versions;
use crate::recover::Seen;
use crate::results::{Cell, Diagnostics, Environment, Report, Requirement};
use crate::runtime;

use super::owner_basic::{Guard, ask};
use super::update_flow::{self, Stand, canonical};
use super::{PartSpec, Verdict};

pub const DEADLINE: Duration = Duration::from_secs(180);
pub const DIR: &str = "incompatible-activate";
pub const STEPS_ARTIFACT: &str = "incompatible-activate/steps.json";
const CELL: &str = "D2";
const COMPLETION: &str =
    "ответ владельца или готовность N+1 в срок шага (30 с); иначе ошибка исполнения";

pub const ACTIVATION: PartSpec = PartSpec {
    name: "activation",
    stimulus: "отсрочка incompatible при живой h1, остановка h1, retry, activate",
    oracle: "N+1 поколения 3 готов, CDHash процесса — кандидата, установлен кандидат; история попытки staged-deferred → staged → intent → swapped → launched → committed → cleaning → idle «подтверждена»; health без нарушений, снимок сессий пуст; runtime/3 подготовлен, новая сессия h2 — хост из runtime/3 с протоколом 2..2",
};

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

const EXPECTED: [State; 8] = [
    State::StagedDeferred,
    State::Staged,
    State::Intent,
    State::Swapped,
    State::Launched,
    State::Committed,
    State::Cleaning,
    State::Idle,
];

fn walk(
    state: &Path,
    nonce: &str,
    layout: &Layout,
    out: &Path,
    steps: &mut Vec<serde_json::Value>,
    guard: &mut Guard,
) -> Result<Vec<Cell>, String> {
    let stand = Stand::new(state, nonce, layout, "gen-1")?;
    let candidate = layout.get("incompatible")?.clone();
    let (gui_index, info) = stand.start_owner(guard, out, "owner-n")?;
    let record = stand.create_session(guard, "h1")?;
    let deferred = match ask(
        state,
        &Request::Update {
            candidate: candidate.app.display().to_string(),
        },
    )? {
        Reply::Staging(staging) => match *staging {
            Staging::Deferred {
                attempt,
                written: true,
                ..
            } => attempt,
            other => return Err(format!("стенд: update incompatible: {other:?}")),
        },
        other => return Err(format!("стенд: update incompatible: {other:?}")),
    };
    update_flow::stop(&record)?;
    let restaged = match ask(state, &Request::Retry)? {
        Reply::Staging(staging) => match *staging {
            Staging::Staged { attempt, .. } => attempt,
            other => return Err(format!("стенд: retry: {other:?}")),
        },
        other => return Err(format!("стенд: retry: {other:?}")),
    };
    if restaged != deferred {
        return Err(format!(
            "стенд: retry поставил {restaged}, отложена {deferred}"
        ));
    }
    let activating = update_flow::activate(state)?;
    steps.push(json!({
        "name": "prepare",
        "owner_n": info,
        "record": record,
        "deferred": deferred,
        "activating": activating,
    }));
    let followed =
        update_flow::follow(&stand, guard, gui_index, activating, false, out, "owner-n")?;
    let next = followed.next.clone();
    let h2 = ask(
        state,
        &Request::CreateSession {
            name: "h2".to_owned(),
        },
    );
    let h2_record = match &h2 {
        Ok(Reply::Session(record)) => {
            guard.sessions.push((**record).clone());
            Some((**record).clone())
        }
        _ => None,
    };
    let installed = stand.check(&stand.installed)?;
    let runtime3 = runtime::ready_dir(state, 3);
    steps.push(json!({
        "name": "follow",
        "facts": followed.facts(),
        "installed": installed,
        "create_h2": format!("{h2:?}"),
    }));

    let mut v = Vec::new();
    if !(next.ready && next.generation == 3 && next.cdhash == candidate.cdhash) {
        v.push(format!(
            "N+1: готов {}, поколение {}, CDHash {}",
            next.ready, next.generation, next.cdhash
        ));
    }
    if installed
        != (Seen::Valid {
            cdhash: candidate.cdhash.clone(),
        })
    {
        v.push(format!("установленный бандл: {installed:?}"));
    }
    let states = followed.states();
    if states != EXPECTED {
        v.push(format!("история попытки: {states:?}"));
    }
    match &followed.journal {
        Some(journal)
            if journal.attempt == deferred
                && journal.state == State::Idle
                && journal.outcome == Some(Outcome::Committed) => {}
        other => v.push(format!("итог журнала: {other:?}")),
    }
    match &next.update {
        Some(confirmed)
            if crate::health::judge(&confirmed.health).is_empty()
                && confirmed.health.sessions.is_empty() => {}
        other => v.push(format!("подтверждение N+1: {other:?}")),
    }
    if next.runtime.dir != runtime3 {
        v.push(format!("рантайм N+1: {:?}", next.runtime));
    }
    match &h2_record {
        Some(record)
            if record.versions == Versions::new(2, 2)
                && canonical(Path::new(&record.executable))
                    == canonical(&runtime::binary(state, 3, Part::Host)) => {}
        other => v.push(format!("сессия h2: {other:?}, ответ {h2:?}")),
    }
    let verdict = Verdict::of(
        v,
        format!(
            "попытка {deferred}: {}; N+1 {} поколения 3 (CDHash {}), h2 из {}",
            states
                .iter()
                .map(|state| state.name())
                .collect::<Vec<_>>()
                .join(" → "),
            next.key.pid,
            &candidate.cdhash[..8.min(candidate.cdhash.len())],
            runtime3.display()
        ),
    );
    let cells = vec![Cell {
        id: CELL.to_owned(),
        part: Some(ACTIVATION.name.to_owned()),
        requirement: Requirement::Required,
        status: verdict.status,
        stimulus: ACTIVATION.stimulus.to_owned(),
        oracle: ACTIVATION.oracle.to_owned(),
        completion: COMPLETION.to_owned(),
        reason: verdict.reason,
        artifacts: vec![
            STEPS_ARTIFACT.to_owned(),
            format!("{DIR}/helper-{deferred}"),
        ],
    }];
    let finished = update_flow::finish(&stand, guard, next.key)?;
    steps.push(json!({ "name": "finish", "facts": finished }));
    Ok(cells)
}
