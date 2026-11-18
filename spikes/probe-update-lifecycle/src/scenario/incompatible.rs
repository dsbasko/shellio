//! Сценарий `incompatible` (задача 7): клетка D2 до активации. Установленный N — копия поколения
//! 1 в `<state>/install/`, живая сессия `h1` идёт от хоста 1 (протокол 1..1) с подключённым
//! лидером размера 30x100, кандидат — `incompatible` (поколение 3, attach 2..2). Части по порядку:
//!
//! 1. `deferral` — `update` кандидатом: журнал `staged-deferred` с причиной, кандидат остаётся в
//!    `.shellio-update-<attempt>/` и проходит проверку, GUI N отвечает, заморозка снята;
//! 2. `session-untouched` — PID и время старта хоста и нагрузки, лидерство, число клиентов и
//!    размер прежние, лидер получает квитанцию ввода без пропуска строк;
//! 3. `message` — клиент, попросивший обновление, получает ответ с причиной;
//! 4. `retry-while-live` — повтор при живой `h1` снова откладывает и журнал не трогает;
//! 5. `after-kill` (наблюдение) — `SIGKILL` GUI N и обычный запуск N: N — владелец, сессия и
//!    отсрочка прежние;
//! 6. `restage` — после остановки `h1` повтор переводит журнал в `staged`, заморозка оставлена.
//!
//! Части клетки D2 обязательные, кроме наблюдения `after-kill`: невыполненный оракул — `fail`,
//! ошибка исполнения — только сломанный стенд. Если после `SIGKILL` владельца нет, `restage`
//! получает `fail` без стимула: отложенного кандидата некому активировать. Сама активация — задача
//! 8. В конце N выходит по `quit`, каталог состояния очищается: перезапуск при `staged` дал бы
//! восстановление `staged → aborting` задач 8 и 11. Шаги и факты — в `incompatible/steps.json`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::attach::{self, Attached, Client, Connected};
use crate::compat::{self, Decision, Staging};
use crate::generation::{self, BUNDLE_ID, Layout, Part};
use crate::host::{self, SessionRecord};
use crate::journal::{self, Journal, State};
use crate::launch;
use crate::observer::{self, ProcessKey};
use crate::owner::{self, Answer, AppOptions, Probe, Reply, Request};
use crate::proto::{self, ATTACH_VERSIONS, Ack, Purpose, Versions};
use crate::pty::Winsize;
use crate::recover::{Checker, Codesign, Seen};
use crate::results::{self, Cell, Diagnostics, Environment, Report, Requirement};

use super::owner_basic::{Guard, ask, clear_state, found_alive, ping, reap, sessions, start_app};
use super::reject::{INSTALL_DIR, install};
use super::{PartSpec, Verdict};

/// Срок процесса сценария.
pub const DEADLINE: Duration = Duration::from_secs(180);
pub const DIR: &str = "incompatible";
pub const STEPS_ARTIFACT: &str = "incompatible/steps.json";
pub const CELL: &str = "D2";
/// Срок одного шага: запуск владельца и постановка кандидата вызывают `codesign` и `ditto`.
const STEP: Duration = Duration::from_secs(20);
const INSTALLED_GENERATION: u32 = 1;
const CANDIDATE: &str = "incompatible";
/// Размер лидера не по умолчанию: изменение размера сессии было бы видно.
const LEADER_SIZE: Winsize = Winsize {
    rows: 30,
    cols: 100,
    xpixel: 0,
    ypixel: 0,
};
const COMPLETION: &str =
    "ответ владельца, хоста или выход процесса в срок шага (20 с); иначе ошибка исполнения";

pub const DEFERRAL: PartSpec = PartSpec {
    name: "deferral",
    stimulus: "update кандидатом incompatible (поколение 3, attach 2..2) при живой h1 от хоста 1 (1..1)",
    oracle: "ответ staged-deferred, журнал записан этим запросом: staged-deferred той же попытки, поколения 1 → 3, CDHash «из» — установленного, «в» — кандидата, причина равна ответу, срока и итога нет; проверка шла диапазоном 2..2 и h1 ответила Refused; кандидат в .shellio-update-<attempt>/ проходит codesign с CDHash кандидата; GUI N отвечает тем же ключом; заморозка снята — create-session h2 создаёт сессию",
};
pub const SESSION: PartSpec = PartSpec {
    name: "session-untouched",
    stimulus: "та же отсрочка; Status h1 по управляющему соединению до и после, ввод «2 ping» лидером",
    oracle: "хост и нагрузка h1 живы с прежними PID и временем старта; lead, leader, clients и размер 30x100 прежние; лидер получает квитанцию «ack 2 ping» без пропуска строк; реестр проверки называет h1 с теми же процессами",
};
pub const MESSAGE: PartSpec = PartSpec {
    name: "message",
    stimulus: "ответ владельца на update",
    oracle: "причина отсрочки называет h1, несовместимость и оба диапазона: кандидат 2..2, хост 1..1",
};
pub const RETRY_LIVE: PartSpec = PartSpec {
    name: "retry-while-live",
    stimulus: "retry при живой h1",
    oracle: "ответ staged-deferred той же попытки без записи журнала; байты журнала прежние; лидерство и размер h1 прежние",
};
pub const AFTER_KILL: PartSpec = PartSpec {
    name: "after-kill",
    stimulus: "SIGKILL GUI N в staged-deferred, затем обычный прямой запуск N",
    oracle: "новый процесс — владелец (ping с его ключом, поколение 1, шагов восстановления нет); байты журнала прежние; кандидат проходит codesign с прежним CDHash; реестр находит h1 с теми же процессами; лидерство и размер h1 прежние",
};
pub const RESTAGE: PartSpec = PartSpec {
    name: "restage",
    stimulus: "остановка h1 и retry новым владельцем",
    oracle: "ответ staged той же попытки, проверка совместимости — ни одной живой сессии; журнал: staged той же попытки, причины и срока нет, поколения и CDHash прежние; кандидат проходит codesign; заморозка оставлена — create-session h3 получает временный отказ «обновление поставлено», записи h3 нет; владелец прежний",
};

#[derive(Debug, Default, Serialize)]
struct Evidence {
    steps: Vec<serde_json::Value>,
}

impl Evidence {
    fn step(&mut self, name: &str, facts: serde_json::Value) {
        self.steps.push(json!({ "name": name, "facts": facts }));
    }
}

/// Лидерство и размер сессии по `Status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Leadership {
    pub lead: u32,
    pub leader: bool,
    pub clients: u32,
    pub rows: u16,
    pub cols: u16,
    pub host: ProcessKey,
    pub workload: ProcessKey,
}

impl Leadership {
    pub fn of(status: &proto::Status) -> Leadership {
        Leadership {
            lead: status.lead,
            leader: status.leader,
            clients: status.clients,
            rows: status.size.rows,
            cols: status.size.cols,
            host: status.host,
            workload: status.workload,
        }
    }
}

fn control(record: &SessionRecord) -> Result<Client, String> {
    let hello = attach::hello(Purpose::Control, ATTACH_VERSIONS, 0, owner::SESSION_SIZE);
    match attach::connect(Path::new(&record.socket), &hello, STEP)? {
        Connected::Welcome(client) => Ok(client),
        Connected::Refused(reason) => Err(format!("control {}: {reason}", record.session)),
    }
}

fn leadership(record: &SessionRecord) -> Result<Leadership, String> {
    Ok(Leadership::of(&control(record)?.status()?))
}

fn alive(key: ProcessKey) -> Result<bool, String> {
    Ok(observer::identify(key)?.is_alive())
}

/// Остановка сессии через управляющее соединение и ожидание выхода хоста и нагрузки.
fn stop_session(record: &SessionRecord) -> Result<(), String> {
    control(record)?.stop()?;
    let left = launch::wait_exit(&[record.host.pid, record.workload.pid], STEP);
    if left.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "сессия {} не остановилась: {left:?}",
            record.session
        ))
    }
}

fn violation(violations: &mut Vec<String>, ok: bool, what: impl FnOnce() -> String) {
    if !ok {
        violations.push(what());
    }
}

/// Часть клетки D2 с выводом.
pub fn part(spec: &PartSpec, requirement: Requirement, verdict: Verdict) -> Cell {
    Cell {
        id: CELL.to_owned(),
        part: Some(spec.name.to_owned()),
        requirement,
        status: verdict.status,
        stimulus: spec.stimulus.to_owned(),
        oracle: spec.oracle.to_owned(),
        completion: COMPLETION.to_owned(),
        reason: verdict.reason,
        artifacts: vec![STEPS_ARTIFACT.to_owned()],
    }
}

/// Ответ `update` или `retry`, если это итог постановки.
fn staging(answer: &Result<Answer, String>) -> Option<&Staging> {
    match answer {
        Ok(Answer::Reply(Reply::Staging(staging))) => Some(staging),
        _ => None,
    }
}

fn check_dir(checker: &Codesign, dir: Option<&PathBuf>) -> Result<Option<Seen>, String> {
    dir.map(|dir| checker.check(dir)).transpose()
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let nonce = environment
        .process
        .nonce
        .as_deref()
        .ok_or("предусловие: нет nonce")?;
    let state = environment
        .process
        .state_dir
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| format!("предусловие: нет {}", launch::STATE_DIR))?;
    let (_, layout) = super::bundles(environment)?;
    let dir = out.join(DIR);
    fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut guard = Guard {
        state: state.clone(),
        apps: Vec::new(),
        sessions: Vec::new(),
        nonce: Some(nonce.to_owned()),
        armed: true,
    };
    let mut evidence = Evidence::default();
    let walked = walk(&state, nonce, &layout, &dir, &mut evidence, &mut guard);
    drop(guard);
    if let Err(reason) = &walked {
        evidence.step("error", json!({ "reason": reason }));
    }
    let written = results::write_json(&dir, "steps.json", &evidence);
    let cells = walked?;
    written?;
    Ok(Report {
        cells,
        diagnostics: Diagnostics {
            notes: vec![format!(
                "срок шага {} с, срок рукопожатия probe {} с на сессию",
                STEP.as_secs(),
                5
            )],
        },
    })
}

fn walk(
    state: &Path,
    nonce: &str,
    layout: &Layout,
    out: &Path,
    evidence: &mut Evidence,
    guard: &mut Guard,
) -> Result<Vec<Cell>, String> {
    let installed = state.join(INSTALL_DIR).join(generation::APP_NAME);
    install(&layout.get("gen-1")?.app, &installed)?;
    let candidate = layout.get(CANDIDATE)?;
    if candidate.generation != 3 {
        return Err(format!(
            "стенд: кандидат {CANDIDATE} — поколение {}, ожидалось 3",
            candidate.generation
        ));
    }
    let app_bin = Part::App.path(&installed);
    let options = AppOptions {
        state: state.to_path_buf(),
        installed: installed.clone(),
        leaf: layout.leaf.clone(),
        identifier: BUNDLE_ID.to_owned(),
        tag: nonce.to_owned(),
        update_attempt: None,
        window: false,
        disclaim: false,
        tcc_fixture: None,
        forward: Vec::new(),
    };
    let checker = Codesign {
        requirement: generation::requirement(BUNDLE_ID, &layout.leaf),
    };
    let store = journal::Store::new(state);
    let journal_bytes = || fs::read(store.path()).ok();
    let mut cells = Vec::new();

    // Стенд: владелец N, сессия h1 и лидер размера 30x100.
    guard
        .apps
        .push(start_app(&app_bin, &options, out, "owner-a")?);
    let info_a = owner::wait_for_owner(state, Instant::now() + STEP)?;
    if info_a.generation != INSTALLED_GENERATION {
        return Err(format!("стенд: владелец — поколение {}", info_a.generation));
    }
    let record = match ask(
        state,
        &Request::CreateSession {
            name: "h1".to_owned(),
        },
    )? {
        Reply::Session(record) => *record,
        other => return Err(format!("стенд: create-session h1: {other:?}")),
    };
    guard.sessions.push(record.clone());
    let hello = attach::hello(Purpose::Attach, ATTACH_VERSIONS, 0, LEADER_SIZE);
    let mut leader = match attach::connect(Path::new(&record.socket), &hello, STEP)? {
        Connected::Welcome(client) => Attached::new(client, Some(1)),
        Connected::Refused(reason) => return Err(format!("стенд: attach h1: {reason}")),
    };
    let deadline = Instant::now() + STEP;
    leader.pump_until(deadline, |transcript| !transcript.lines.is_empty())?;
    leader.exchange(1, "ping", deadline)?;
    let before = leadership(&record)?;
    if !(before.leader
        && before.clients == 1
        && (before.rows, before.cols) == (LEADER_SIZE.rows, LEADER_SIZE.cols)
        && before.host == record.host
        && before.workload == record.workload)
    {
        return Err(format!(
            "стенд: лидер h1 не установлен, оракул неизменности был бы пустым: {before:?}"
        ));
    }
    if store
        .read()
        .map_err(|err| format!("стенд: {err}"))?
        .is_some()
    {
        return Err("стенд: журнал есть до обновления".to_owned());
    }
    evidence.step(
        "setup",
        json!({
            "owner": info_a,
            "record": record,
            "leader_welcome": leader.welcome(),
            "leadership": before,
            "candidate": candidate,
        }),
    );

    // 1–3. Постановка несовместимого кандидата.
    let update = owner::request(
        state,
        &Request::Update {
            candidate: candidate.app.display().to_string(),
        },
        STEP,
    );
    let journal_after = store.read();
    let bytes_after = journal_bytes();
    let staged_dir = journal_after
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .map(Journal::staged_dir);
    let staged_seen = check_dir(&checker, staged_dir.as_ref())?;
    let ping_after = ping(state);
    let leadership_after = leadership(&record);
    let processes_alive = (alive(record.host)?, alive(record.workload)?);
    let exchange = leader.exchange(2, "ping", Instant::now() + STEP);
    let h2 = ask(
        state,
        &Request::CreateSession {
            name: "h2".to_owned(),
        },
    );
    let h2_record = match &h2 {
        Ok(Reply::Session(record)) => Some((**record).clone()),
        _ => None,
    };
    if let Some(record2) = &h2_record {
        guard.sessions.push(record2.clone());
        stop_session(record2)?;
    }
    evidence.step(
        "update",
        json!({
            "answer": format!("{update:?}"),
            "staging": staging(&update),
            "journal": format!("{journal_after:?}"),
            "staged_dir": staged_dir,
            "staged_seen": format!("{staged_seen:?}"),
            "ping": format!("{ping_after:?}"),
            "leadership": format!("{leadership_after:?}"),
            "processes_alive": processes_alive,
            "exchange": format!("{exchange:?}"),
            "transcript": leader.transcript,
            "acks": leader.acks,
            "create_h2": format!("{h2:?}"),
        }),
    );
    let deferred = match staging(&update) {
        Some(Staging::Deferred {
            attempt,
            reason,
            written,
            checked,
        }) => Some((attempt.clone(), reason.clone(), *written, checked.clone())),
        _ => None,
    };
    let journal_ok = |journal: &Journal, attempt: &str| {
        journal.attempt == attempt
            && journal.from == INSTALLED_GENERATION
            && journal.to == candidate.generation
            && journal.cdhash_from == info_a.installed_cdhash
            && journal.cdhash_to == candidate.cdhash
            && journal.deadline_ms.is_none()
            && journal.outcome.is_none()
    };
    let candidate_ok = |seen: &Option<Seen>| matches!(seen, Some(Seen::Valid { cdhash }) if *cdhash == candidate.cdhash);

    let mut v = Vec::new();
    match &deferred {
        None => v.push(format!("ответ на update не staged-deferred: {update:?}")),
        Some((attempt, reason, written, checked)) => {
            violation(&mut v, *written, || {
                "журнал не записан этим запросом".to_owned()
            });
            match &journal_after {
                Ok(Some(journal))
                    if journal.state == State::StagedDeferred
                        && journal_ok(journal, attempt)
                        && journal.reason.as_deref() == Some(reason.as_str()) => {}
                other => v.push(format!("журнал после update: {other:?}")),
            }
            violation(
                &mut v,
                checked.versions == Versions::new(2, 2)
                    && matches!(checked.decision, Decision::Deferred { .. })
                    && checked.entries.iter().any(|entry| {
                        entry.record.session == record.session
                            && entry.host_alive
                            && matches!(entry.probe, Probe::Refused { .. })
                    }),
                || format!("проверка совместимости: {checked:?}"),
            );
        }
    }
    violation(&mut v, candidate_ok(&staged_seen), || {
        format!("кандидат в каталоге попытки {staged_dir:?}: {staged_seen:?}")
    });
    match &ping_after {
        Ok(info) if info.key == info_a.key && info.generation == INSTALLED_GENERATION => {}
        other => v.push(format!("GUI N после отсрочки: {other:?}")),
    }
    violation(&mut v, h2_record.is_some(), || {
        format!("после отсрочки create-session h2 не создал сессию: {h2:?}")
    });
    let attempt = deferred.as_ref().map(|(attempt, ..)| attempt.clone());
    cells.push(part(
        &DEFERRAL,
        Requirement::Required,
        Verdict::of(
            v,
            format!(
                "журнал staged-deferred попытки {}, кандидат проходит codesign (CDHash {}), GUI N {} отвечает, h2 создана после отсрочки",
                attempt.as_deref().unwrap_or("?"),
                &candidate.cdhash[..8.min(candidate.cdhash.len())],
                info_a.key.pid
            ),
        ),
    ));

    let mut v = Vec::new();
    violation(&mut v, processes_alive == (true, true), || {
        format!("хост и нагрузка h1 после отсрочки: {processes_alive:?}")
    });
    match &leadership_after {
        Ok(after) if *after == before => {}
        other => v.push(format!("лидерство h1: до {before:?}, после {other:?}")),
    }
    violation(
        &mut v,
        exchange.is_ok()
            && leader.transcript.problems.is_empty()
            && leader.transcript.receipt(2, "ping").is_some()
            && leader.acks.contains(&(2, Ack::Accepted)),
        || {
            format!(
                "лидер h1: {exchange:?}, проблемы {:?}, ответы {:?}",
                leader.transcript.problems, leader.acks
            )
        },
    );
    if let Some((_, _, _, checked)) = &deferred {
        violation(
            &mut v,
            checked.entries.iter().any(|entry| {
                entry.record.session == record.session
                    && entry.record.host == record.host
                    && entry.record.workload == record.workload
            }),
            || format!("реестр проверки без h1 с теми же процессами: {checked:?}"),
        );
    }
    cells.push(part(
        &SESSION,
        Requirement::Required,
        Verdict::of(
            v,
            format!(
                "хост {} и нагрузка {} прежние, lead {}, клиентов {}, размер {}x{}, квитанция «ack 2 ping» в строке {}",
                record.host.pid,
                record.workload.pid,
                before.lead,
                before.clients,
                before.rows,
                before.cols,
                leader
                    .transcript
                    .receipt(2, "ping")
                    .map_or(0, |line| line.number)
            ),
        ),
    ));

    let reason = deferred.as_ref().map(|(_, reason, ..)| reason.clone());
    let mut v = Vec::new();
    match &reason {
        Some(reason)
            if reason.contains(&record.session)
                && reason.contains("несовместим")
                && reason.contains("2..2")
                && reason.contains("1..1") => {}
        other => v.push(format!("причина отсрочки: {other:?}")),
    }
    cells.push(part(
        &MESSAGE,
        Requirement::Required,
        Verdict::of(
            v,
            format!("клиент получил: «{}»", reason.clone().unwrap_or_default()),
        ),
    ));

    let Some(attempt) = attempt else {
        for spec in [&RETRY_LIVE, &RESTAGE] {
            cells.push(part(
                spec,
                Requirement::Required,
                Verdict::of(
                    vec!["не выполнена: отсрочки не было".to_owned()],
                    String::new(),
                ),
            ));
        }
        cells.push(part(
            &AFTER_KILL,
            Requirement::Observation,
            Verdict::of(
                vec!["не выполнена: отсрочки не было".to_owned()],
                String::new(),
            ),
        ));
        return Ok(cells);
    };

    // 4. Повтор при живой h1.
    let retry = owner::request(state, &Request::Retry, STEP);
    let bytes_retry = journal_bytes();
    let leadership_retry = leadership(&record);
    evidence.step(
        "retry-while-live",
        json!({
            "answer": format!("{retry:?}"),
            "journal_unchanged": bytes_retry == bytes_after,
            "leadership": format!("{leadership_retry:?}"),
        }),
    );
    let mut v = Vec::new();
    violation(
        &mut v,
        matches!(staging(&retry), Some(Staging::Deferred { attempt: got, written: false, .. }) if *got == attempt),
        || format!("ответ на retry: {retry:?}"),
    );
    violation(
        &mut v,
        bytes_after.is_some() && bytes_retry == bytes_after,
        || "байты журнала изменились".to_owned(),
    );
    match &leadership_retry {
        Ok(after) if *after == before => {}
        other => v.push(format!("лидерство h1 после retry: {other:?}")),
    }
    cells.push(part(
        &RETRY_LIVE,
        Requirement::Required,
        Verdict::of(
            v,
            "retry снова отложил без записи, байты журнала и лидерство прежние".to_owned(),
        ),
    ));

    // 5. Наблюдение: SIGKILL GUI N и обычный запуск N.
    let bytes_before_kill = journal_bytes();
    guard.apps[0]
        .kill()
        .map_err(|err| format!("SIGKILL GUI N: {err}"))?;
    let killed = reap(&mut guard.apps[0], "убитый GUI N")?;
    if std::os::unix::process::ExitStatusExt::signal(&killed) != Some(libc::SIGKILL) {
        return Err(format!("стенд: GUI N вышел не от SIGKILL: {killed}"));
    }
    guard
        .apps
        .push(start_app(&app_bin, &options, out, "owner-b")?);
    let b_pid = guard.apps[1].id() as i32;
    let info_b = owner::wait_for_owner(state, Instant::now() + STEP);
    let bytes_after_kill = journal_bytes();
    let registry_b = sessions(state);
    let staged_after_kill = check_dir(&checker, staged_dir.as_ref())?;
    let leadership_kill = leadership(&record);
    evidence.step(
        "after-kill",
        json!({
            "killed": format!("{killed}"),
            "owner_b_pid": b_pid,
            "owner": format!("{info_b:?}"),
            "journal_unchanged": bytes_after_kill == bytes_before_kill,
            "registry": registry_b.as_ref().map(|registry| &registry.entries).ok(),
            "registry_error": registry_b.as_ref().err(),
            "staged_seen": format!("{staged_after_kill:?}"),
            "leadership": format!("{leadership_kill:?}"),
        }),
    );
    let mut v = Vec::new();
    match &info_b {
        Ok(info)
            if info.key.pid == b_pid
                && info.generation == INSTALLED_GENERATION
                && info.recovery.is_empty() => {}
        other => v.push(format!("новый запуск N не стал владельцем: {other:?}")),
    }
    violation(
        &mut v,
        bytes_before_kill.is_some() && bytes_after_kill == bytes_before_kill,
        || "байты журнала изменились".to_owned(),
    );
    violation(&mut v, candidate_ok(&staged_after_kill), || {
        format!("кандидат после SIGKILL: {staged_after_kill:?}")
    });
    match &registry_b {
        Ok(registry) if found_alive(&registry.entries, &record) => {}
        other => v.push(format!(
            "реестр нового владельца без h1: {:?}",
            other.as_ref().map(|registry| &registry.entries)
        )),
    }
    match &leadership_kill {
        Ok(after) if *after == before => {}
        other => v.push(format!("лидерство h1 после SIGKILL GUI N: {other:?}")),
    }
    let owner_b = info_b
        .as_ref()
        .ok()
        .filter(|info| info.key.pid == b_pid)
        .cloned();
    cells.push(part(
        &AFTER_KILL,
        Requirement::Observation,
        Verdict::of(
            v,
            format!(
                "GUI N {} убит, новый владелец {b_pid}: журнал и кандидат прежние, h1 жива с прежним лидерством",
                info_a.key.pid
            ),
        ),
    ));
    let Some(owner_b) = owner_b else {
        cells.push(part(
            &RESTAGE,
            Requirement::Required,
            Verdict::of(
                vec!["не выполнена: после SIGKILL GUI N владельца нет, отложенного кандидата некому активировать".to_owned()],
                String::new(),
            ),
        ));
        return Ok(cells);
    };

    // 6. Остановка h1 и повтор: staged-deferred → staged.
    drop(leader);
    stop_session(&record)?;
    let restage = owner::request(state, &Request::Retry, STEP);
    let journal_restaged = store.read();
    let staged_restaged = check_dir(&checker, staged_dir.as_ref())?;
    let h3 = ask(
        state,
        &Request::CreateSession {
            name: "h3".to_owned(),
        },
    );
    if let Ok(Reply::Session(record3)) = &h3 {
        guard.sessions.push((**record3).clone());
    }
    let h3_recorded = host::record_path(&state.join(host::SESSIONS_DIR), "h3").exists();
    let ping_restaged = ping(state);
    evidence.step(
        "restage",
        json!({
            "answer": format!("{restage:?}"),
            "staging": staging(&restage),
            "journal": format!("{journal_restaged:?}"),
            "staged_seen": format!("{staged_restaged:?}"),
            "create_h3": format!("{h3:?}"),
            "h3_recorded": h3_recorded,
            "ping": format!("{ping_restaged:?}"),
        }),
    );
    let mut v = Vec::new();
    violation(
        &mut v,
        matches!(
            staging(&restage),
            Some(Staging::Staged { attempt: got, checked })
                if *got == attempt && checked.decision == (Decision::Compatible { checked: 0 })
        ),
        || format!("ответ на retry после остановки h1: {restage:?}"),
    );
    match &journal_restaged {
        Ok(Some(journal))
            if journal.state == State::Staged
                && journal_ok(journal, &attempt)
                && journal.reason.is_none() => {}
        other => v.push(format!("журнал после retry: {other:?}")),
    }
    violation(&mut v, candidate_ok(&staged_restaged), || {
        format!("кандидат после retry: {staged_restaged:?}")
    });
    match &h3 {
        Ok(Reply::Refused {
            reason,
            retry: true,
        }) if reason == compat::STAGED => {}
        other => v.push(format!("create-session h3 под заморозкой: {other:?}")),
    }
    violation(&mut v, !h3_recorded, || "запись h3 появилась".to_owned());
    match &ping_restaged {
        Ok(info) if info.key == owner_b.key => {}
        other => v.push(format!("владелец после retry: {other:?}")),
    }
    cells.push(part(
        &RESTAGE,
        Requirement::Required,
        Verdict::of(
            v,
            format!(
                "после остановки h1 журнал попытки {attempt} в staged, create-session получил «{}»",
                compat::STAGED
            ),
        ),
    ));

    // Уборка: quit владельца, каталог состояния.
    let quit = ask(state, &Request::Quit)?;
    let quit_status = reap(&mut guard.apps[1], "GUI N после quit")?;
    evidence.step(
        "quit",
        json!({ "reply": format!("{quit:?}"), "exit": quit_status.code() }),
    );
    if quit != Reply::Done || !quit_status.success() {
        return Err(format!("стенд: quit: {quit:?}, {quit_status}"));
    }
    guard
        .sessions
        .retain(|record| observer::identify(record.host).is_ok_and(|identity| identity.is_alive()));
    if !guard.sessions.is_empty() {
        return Err(format!("стенд: остались сессии {:?}", guard.sessions));
    }
    guard.apps.clear();
    clear_state(state);
    guard.armed = false;
    Ok(cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_are_valid_cells_and_only_after_kill_is_an_observation() {
        let specs = [
            (&DEFERRAL, Requirement::Required),
            (&SESSION, Requirement::Required),
            (&MESSAGE, Requirement::Required),
            (&RETRY_LIVE, Requirement::Required),
            (&AFTER_KILL, Requirement::Observation),
            (&RESTAGE, Requirement::Required),
        ];
        let mut names: Vec<&str> = specs.iter().map(|(spec, _)| spec.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), specs.len(), "имена частей не повторяются");
        for (spec, requirement) in specs {
            for verdict in [
                Verdict::of(Vec::new(), "есть".to_owned()),
                Verdict::of(vec!["нет".to_owned()], String::new()),
            ] {
                let cell = part(spec, requirement, verdict);
                assert!(cell.problems().is_empty(), "{:?}", cell.problems());
                assert_eq!(cell.requirement, requirement);
            }
        }
    }

    #[test]
    fn leadership_compares_every_field_that_a_probe_must_not_change() {
        let status = proto::Status {
            session: "h1".to_owned(),
            host: ProcessKey {
                pid: 1,
                start_us: 2,
            },
            workload: ProcessKey {
                pid: 3,
                start_us: 4,
            },
            workload_pgid: 3,
            workload_sid: Some(3),
            versions: ATTACH_VERSIONS,
            generation: 1,
            clients: 1,
            lead: 1,
            leader: true,
            size: LEADER_SIZE,
            start: 0,
            end: 10,
            lines: 2,
            exited: false,
        };
        let base = Leadership::of(&status);
        let mut later = status.clone();
        later.end = 99;
        later.lines = 7;
        assert_eq!(Leadership::of(&later), base, "вывод идёт, лидерство то же");
        for change in [
            |s: &mut proto::Status| s.lead = 2,
            |s: &mut proto::Status| s.leader = false,
            |s: &mut proto::Status| s.clients = 2,
            |s: &mut proto::Status| s.size = owner::SESSION_SIZE,
            |s: &mut proto::Status| s.host.start_us = 9,
            |s: &mut proto::Status| s.workload.pid = 9,
        ] {
            let mut changed = status.clone();
            change(&mut changed);
            assert_ne!(Leadership::of(&changed), base);
        }
    }
}
