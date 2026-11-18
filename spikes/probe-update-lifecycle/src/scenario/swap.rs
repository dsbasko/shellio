//! Сценарий `swap` (задача 8): клетка B1 «Успешная замена». Установленный N — копия поколения 1,
//! живая сессия `h1` от хоста 1 с выводом и квитанцией ввода, кандидат — поколение 2. `update`
//! ставит его в `staged`, `activate` пишет `intent` и запускает помощника, GUI N выходит, помощник
//! меняет бандлы местами и запускает N+1 через `open -n`. N+1 стоит на барьере перед `rename`
//! записи `committed`: стенд проверяет, что N хранится в каталоге попытки и проходит `codesign`.
//! Части:
//!
//! 1. `sequence` — история журнала попытки: `staged → intent → swapped → launched → committed →
//!    cleaning → idle` («подтверждена»), писатели — GUI N, помощник, N+1;
//! 2. `gui-exit-first` — GUI N вышел раньше первой мутации: помощник увидел его выход до взятия
//!    блокировки и обмена, при обмене GUI N не жив;
//! 3. `n-kept-until-commit` — до `committed` N в `.shellio-update-<attempt>/` проходит `codesign`
//!    с CDHash N, установлен N+1; после — каталог попытки убран;
//! 4. `health` — N+1 подтвердился по правилу health: путь и CDHash процесса, единственный владелец,
//!    `ping` своего сокета с поколением 2, `h1` найдена с теми же процессами, attach согласовал
//!    версию 1, хвост без пропуска, квитанция ввода; стенд независимо видит те же процессы `h1` и
//!    квитанцию health в её хвосте.
//!
//! Невыполненный оракул — `fail` части, сломанный стенд — ошибка исполнения. Шаги и факты — в
//! `swap/steps.json`, файлы помощника и N+1 — в `swap/helper-<attempt>/`.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::attach::{self, Attached, Connected};
use crate::barrier;
use crate::compat::Staging;
use crate::generation::{Layout, Part};
use crate::host::SessionRecord;
use crate::journal::{Outcome, State};
use crate::observer::ProcessKey;
use crate::owner::{AppReport, OwnerInfo, Reply, Request};
use crate::proto::{ATTACH_VERSIONS, Purpose};
use crate::pty::Winsize;
use crate::recover::Seen;
use crate::results::{Cell, Diagnostics, Environment, Report, Requirement};
use crate::update;

use super::owner_basic::{Guard, ask};
use super::update_flow::{self, Followed, Stand};
use super::{PartSpec, Verdict};

pub const DEADLINE: Duration = Duration::from_secs(180);
pub const DIR: &str = "swap";
pub const STEPS_ARTIFACT: &str = "swap/steps.json";
pub const CELL: &str = "B1";
const COMPLETION: &str = "ответ владельца, барьер, выход процесса или готовность N+1 в срок шага (30 с); иначе ошибка исполнения";

pub const SEQUENCE: PartSpec = PartSpec {
    name: "sequence",
    stimulus: "update поколением 2 при живой h1, затем activate",
    oracle: "история журнала попытки — ровно staged, intent, swapped, launched, committed, cleaning, idle с итогом «подтверждена»; staged и intent пишет GUI N, swapped и launched — помощник, committed, cleaning и idle — N+1",
};
pub const GUI_FIRST: PartSpec = PartSpec {
    name: "gui-exit-first",
    stimulus: "тот же проход",
    oracle: "GUI N вышел с кодом 0 и строкой updated; помощник записал событие выхода GUI N раньше взятия блокировки и обмена; при обмене GUI N не жив; первая мутация (обмен, запись swapped) позже выхода GUI N",
};
pub const KEPT: PartSpec = PartSpec {
    name: "n-kept-until-commit",
    stimulus: "барьер N+1 перед rename записи committed",
    oracle: "на барьере: журнал launched этой попытки с ключом N+1, каталог попытки проходит codesign с CDHash поколения 1, установлен бандл с CDHash поколения 2; после: журнал idle «подтверждена», каталога попытки нет",
};
pub const HEALTH: PartSpec = PartSpec {
    name: "health",
    stimulus: "тот же проход",
    oracle: "N+1 готов, его отчёт подтверждения: правило health без нарушений — исполняемый файл установленного бандла, CDHash поколения 2, единственный владелец, ping поколения 2, h1 найдена с теми же процессами, версия 1, хвост без пропуска, квитанция; стенд видит h1 с прежними процессами и квитанцию health в её хвосте",
};

fn violation(violations: &mut Vec<String>, ok: bool, what: impl FnOnce() -> String) {
    if !ok {
        violations.push(what());
    }
}

pub fn part(spec: &PartSpec, verdict: Verdict, artifacts: &[String]) -> Cell {
    let mut all = vec![STEPS_ARTIFACT.to_owned()];
    all.extend(artifacts.iter().cloned());
    Cell {
        id: CELL.to_owned(),
        part: Some(spec.name.to_owned()),
        requirement: Requirement::Required,
        status: verdict.status,
        stimulus: spec.stimulus.to_owned(),
        oracle: spec.oracle.to_owned(),
        completion: COMPLETION.to_owned(),
        reason: verdict.reason,
        artifacts: all,
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
        diagnostics: Diagnostics {
            partial: None,
            notes: vec![format!(
                "срок health {} с от intent, срок запуска N+1 {} с, срок шага стенда {} с",
                update::HEALTH_WINDOW.as_secs(),
                update::LAUNCH_WAIT.as_secs(),
                update_flow::STEP.as_secs()
            )],
        },
    })
}

/// Стенд до выхода GUI N.
pub(super) struct Prepared {
    pub gui_index: usize,
    pub owner_n: OwnerInfo,
    pub record: SessionRecord,
    pub activating: (String, ProcessKey),
}

/// Стенд до `activate`: GUI N установленного поколения, сессия `h1` с выводом и квитанцией,
/// `update` и `activate` кандидатом `candidate`.
pub(super) fn prepare(
    stand: &Stand,
    layout: &Layout,
    candidate: &str,
    guard: &mut Guard,
    out: &Path,
    steps: &mut Vec<serde_json::Value>,
) -> Result<Prepared, String> {
    let (gui_index, info) = stand.start_owner(guard, out, "owner-n")?;
    let record = stand.create_session(guard, "h1")?;
    let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, Winsize::cells(0, 0));
    let mut first = match attach::connect(Path::new(&record.socket), &hello, update_flow::STEP)? {
        Connected::Welcome(client) => Attached::new(client, Some(1)),
        Connected::Refused(reason) => return Err(format!("стенд: attach h1: {reason}")),
    };
    let deadline = Instant::now() + update_flow::STEP;
    first.pump_until(deadline, |transcript| !transcript.lines.is_empty())?;
    first.exchange(1, "ping", deadline)?;
    first.detach(Instant::now() + update_flow::STEP)?;
    let candidate = layout.get(candidate)?;
    let staged = match ask(
        &stand.state,
        &Request::Update {
            candidate: candidate.app.display().to_string(),
        },
    )? {
        Reply::Staging(staging) => match *staging {
            Staging::Staged { attempt, checked } => (attempt, checked),
            other => return Err(format!("стенд: update {}: {other:?}", candidate.name)),
        },
        other => return Err(format!("стенд: update {}: {other:?}", candidate.name)),
    };
    let helper_copy = update::helper_copy(&stand.state, &staged.0);
    let activating = update_flow::activate(&stand.state)?;
    if activating.0 != staged.0 {
        return Err(format!(
            "стенд: activate попытки {}, поставлена {}",
            activating.0, staged.0
        ));
    }
    steps.push(json!({
        "name": "prepare",
        "owner_n": info,
        "record": record,
        "transcript": first.transcript,
        "staged": { "attempt": staged.0, "checked": staged.1 },
        "helper_copy": helper_copy,
        "helper_copy_exists": helper_copy.is_file(),
        "activating": { "attempt": activating.0, "helper": activating.1 },
    }));
    Ok(Prepared {
        gui_index,
        owner_n: info,
        record,
        activating,
    })
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
    let gen2 = layout.get("gen-2")?.cdhash.clone();
    barrier::arm(state, update_flow::PAUSE)?;
    let Prepared {
        gui_index,
        record,
        activating,
        ..
    } = prepare(&stand, layout, "gen-2", guard, out, steps)?;
    let followed = update_flow::follow(&stand, guard, gui_index, activating, true, out, "owner-n")?;
    steps.push(json!({ "name": "follow", "facts": followed.facts() }));
    let helper_files = format!("{DIR}/helper-{}", followed.attempt);
    let artifacts = vec![helper_files.clone(), format!("{DIR}/owner-n.stdout")];

    // Стенд независимо: h1 с прежними процессами и квитанция health в её хвосте.
    let same = update_flow::same_processes(&record)?;
    let receipt_seen = followed.next.update.as_ref().and_then(|confirmed| {
        let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, Winsize::cells(0, 0));
        let client = match attach::connect(Path::new(&record.socket), &hello, update_flow::STEP) {
            Ok(Connected::Welcome(client)) => client,
            _ => return None,
        };
        let mut tail = Attached::new(client, None);
        let seq = confirmed.health.input_seq;
        let found = tail
            .pump_until(Instant::now() + update_flow::STEP, |transcript| {
                transcript.receipt(seq, crate::health::COMMAND).is_some()
            })
            .is_ok();
        let gaps = tail.transcript.problems.clone();
        let _ = tail.detach(Instant::now() + update_flow::STEP);
        Some((found, gaps))
    });
    steps.push(json!({
        "name": "independent",
        "h1_same_processes": same,
        "health_receipt_in_tail": receipt_seen,
    }));

    let cells = vec![
        part(&SEQUENCE, sequence_verdict(&followed), &artifacts),
        part(&GUI_FIRST, gui_first_verdict(&followed), &artifacts),
        part(
            &KEPT,
            kept_verdict(&followed, &gen1, &gen2, &stand.attempt_dirs()),
            &artifacts,
        ),
        part(
            &HEALTH,
            health_verdict(&followed, &stand, &gen2, same, receipt_seen.as_ref()),
            &artifacts,
        ),
    ];

    let finished = update_flow::finish(&stand, guard, followed.next.key)?;
    steps.push(json!({ "name": "finish", "facts": finished }));
    Ok(cells)
}

const EXPECTED: [State; 7] = [
    State::Staged,
    State::Intent,
    State::Swapped,
    State::Launched,
    State::Committed,
    State::Cleaning,
    State::Idle,
];

/// Писатели записей истории: GUI N, помощник, N+1.
pub fn sequence_verdict(followed: &Followed) -> Verdict {
    let mut v = Vec::new();
    let states = followed.states();
    violation(&mut v, states == EXPECTED, || {
        format!("история попытки: {states:?}")
    });
    let writer = |state: State| followed.entry(state).map(|entry| entry.pid as i32);
    let next = followed.next.key.pid;
    for (state, pid, who) in [
        (State::Staged, followed.gui.pid, "GUI N"),
        (State::Intent, followed.gui.pid, "GUI N"),
        (State::Swapped, followed.helper.pid, "помощник"),
        (State::Launched, followed.helper.pid, "помощник"),
        (State::Committed, next, "N+1"),
        (State::Cleaning, next, "N+1"),
        (State::Idle, next, "N+1"),
    ] {
        violation(&mut v, writer(state) == Some(pid), || {
            format!(
                "{} записал {:?}, ждали {who} {pid}",
                state.name(),
                writer(state)
            )
        });
    }
    match &followed.journal {
        Some(journal)
            if journal.attempt == followed.attempt
                && journal.state == State::Idle
                && journal.outcome == Some(Outcome::Committed) => {}
        other => v.push(format!("итог журнала: {other:?}")),
    }
    Verdict::of(
        v,
        format!(
            "попытка {}: {} — GUI N {}, помощник {}, N+1 {}",
            followed.attempt,
            states
                .iter()
                .map(|state| state.name())
                .collect::<Vec<_>>()
                .join(" → "),
            followed.gui.pid,
            followed.helper.pid,
            next
        ),
    )
}

pub fn gui_first_verdict(followed: &Followed) -> Verdict {
    let mut v = Vec::new();
    violation(&mut v, followed.gui_exit.success(), || {
        format!("GUI N вышел: {}", followed.gui_exit)
    });
    violation(
        &mut v,
        followed
            .gui_reports
            .iter()
            .any(|report| matches!(report, AppReport::Updated { attempt, .. } if *attempt == followed.attempt)),
        || format!("строки GUI N: {:?}", followed.gui_reports),
    );
    let exited = followed.event_ns("gui-exited");
    let locked = followed.event_ns("locked");
    let exchanged = followed.event_ns("exchanged");
    let swapped = followed.entry(State::Swapped).map(|entry| entry.uptime_ns);
    violation(
        &mut v,
        matches!((exited, locked, exchanged, swapped), (Some(e), Some(l), Some(x), Some(s)) if e <= l && l <= x && x <= s),
        || {
            format!(
                "порядок помощника: выход GUI N {exited:?}, блокировка {locked:?}, обмен {exchanged:?}, swapped {swapped:?}"
            )
        },
    );
    let gui_alive = followed
        .event("exchanged")
        .and_then(|event| event.pointer("/detail/gui_alive"))
        .and_then(serde_json::Value::as_bool);
    violation(&mut v, gui_alive == Some(false), || {
        format!("GUI N при обмене жив: {gui_alive:?}")
    });
    violation(
        &mut v,
        followed
            .paused
            .as_ref()
            .is_some_and(|paused| !paused.gui_alive),
        || "GUI N жив на остановке N+1".to_owned(),
    );
    Verdict::of(
        v,
        format!(
            "GUI N {} вышел (код 0, updated); помощник: выход GUI N {} нс → блокировка {} нс → обмен {} нс, GUI N при обмене не жив",
            followed.gui.pid,
            exited.unwrap_or(0),
            locked.unwrap_or(0),
            exchanged.unwrap_or(0)
        ),
    )
}

pub fn kept_verdict(followed: &Followed, gen1: &str, gen2: &str, dirs_after: &[String]) -> Verdict {
    let mut v = Vec::new();
    match &followed.paused {
        None => v.push("остановки перед committed не было".to_owned()),
        Some(paused) => {
            match &paused.journal {
                Some(journal)
                    if journal.attempt == followed.attempt
                        && journal.state == State::Launched
                        && journal.owner == Some(followed.next.key) => {}
                other => v.push(format!("журнал на остановке: {other:?}")),
            }
            violation(
                &mut v,
                matches!(&paused.staged, Seen::Valid { cdhash } if cdhash == gen1),
                || format!("каталог попытки на остановке: {:?}", paused.staged),
            );
            violation(
                &mut v,
                matches!(&paused.installed, Seen::Valid { cdhash } if cdhash == gen2),
                || format!("установленный на остановке: {:?}", paused.installed),
            );
            violation(&mut v, paused.reached.process == followed.next.key, || {
                format!("на барьере стоял {:?}", paused.reached.process)
            });
        }
    }
    violation(&mut v, followed.staged_after == Seen::Absent, || {
        format!("каталог попытки после: {:?}", followed.staged_after)
    });
    violation(
        &mut v,
        !dirs_after
            .iter()
            .any(|name| name.ends_with(&followed.attempt)),
        || format!("каталоги попытки после: {dirs_after:?}"),
    );
    Verdict::of(
        v,
        format!(
            "на барьере N+1 {} перед committed: каталог попытки проходит codesign с CDHash {}, установлен {}; после — каталога нет",
            followed.next.key.pid,
            &gen1[..8.min(gen1.len())],
            &gen2[..8.min(gen2.len())]
        ),
    )
}

pub fn health_verdict(
    followed: &Followed,
    stand: &Stand,
    gen2: &str,
    same: bool,
    receipt: Option<&(bool, Vec<String>)>,
) -> Verdict {
    let mut v = Vec::new();
    let next = &followed.next;
    violation(&mut v, next.ready && next.generation == 2, || {
        format!("N+1: готов {}, поколение {}", next.ready, next.generation)
    });
    let expected_bin = update_flow::canonical(&Part::App.path(&stand.installed));
    match &next.update {
        None => v.push("у N+1 нет отчёта подтверждения".to_owned()),
        Some(confirmed) => {
            let violations = crate::health::judge(&confirmed.health);
            violation(&mut v, violations.is_empty(), || {
                format!("правило health: {violations:?}")
            });
            let facts = &confirmed.health;
            violation(
                &mut v,
                facts.executable.as_deref() == Some(expected_bin.as_path())
                    && facts.cdhash.as_deref() == Some(gen2)
                    && facts.lock_held
                    && facts.ping_generation == Some(2),
                || format!("факты процесса: {facts:?}"),
            );
            let h1 = facts
                .sessions
                .iter()
                .find(|session| session.expected.session == "h1");
            violation(
                &mut v,
                h1.is_some_and(|session| {
                    session.found
                        && session.same_processes
                        && session.version == Some(1)
                        && session.lines > 0
                        && session.gaps.is_empty()
                        && session.receipt
                }),
                || format!("h1 в health: {h1:?}"),
            );
            violation(
                &mut v,
                confirmed.committed_ms < confirmed.deadline_ms,
                || {
                    format!(
                        "committed {} не раньше срока {}",
                        confirmed.committed_ms, confirmed.deadline_ms
                    )
                },
            );
        }
    }
    violation(&mut v, same, || "h1: процессы сменились".to_owned());
    violation(
        &mut v,
        matches!(receipt, Some((true, gaps)) if gaps.is_empty()),
        || format!("квитанция health в хвосте h1: {receipt:?}"),
    );
    Verdict::of(
        v,
        format!(
            "N+1 {} готов: health без нарушений, исполняемый файл {}, CDHash {}, h1 версия 1 с квитанцией",
            next.key.pid,
            expected_bin.display(),
            &gen2[..8.min(gen2.len())]
        ),
    )
}
