//! Сценарии атрибуции на настоящих процессах: `sessions`, `git-shared`, `reparent`, `churn`,
//! `attach-move`, `login`, `eperm-inject`. Ожидания — отчёт, раздел «Ожидания».
//!
//! Каждый сценарий — оркестрация фикстур и снимков, а клетки — чистые функции наблюдений.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::stand::{
    self, EXIT, READY, Stand, calls, composition_problems, json, key_of, occurrences,
    owner_summary, session_doubts, snap_claims,
};
use crate::attrib::{Attribution, Claims, GitWork, Note, Origin, Owner, OwnerRecord, Panel, Role};
use crate::fixture::receipts::{Event, Mode, Receipt};
use crate::fixture::{self, Program, Spec};
use crate::proc::{BsdInfo, Listed, ReadError, Source, System, Unread, Usage};
use crate::results::{Cell, NotRunCause, Report, Status};
use crate::snapshot::{Gap, Key, Snapshot, Summary, Value};

fn host(name: &str, children: u32, grandchildren: u32, double_fork: u32) -> Spec {
    let mut spec = Spec::new(Mode::Host, name);
    spec.program = Program::Tree;
    spec.children = children;
    spec.grandchildren = grandchildren;
    spec.double_fork = double_fork;
    spec
}

fn idle(name: &str) -> Spec {
    Spec::new(Mode::Idle, name)
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

fn panel(id: &str, surface: Vec<Key>, record: Option<OwnerRecord>) -> Panel {
    Panel {
        id: id.to_owned(),
        surface,
        record,
    }
}

fn owner(id: &str) -> Owner {
    Owner::Panel(id.to_owned())
}

/// Ожидаемое множество по именам фикстур.
fn expected(
    receipts: &BTreeMap<String, Receipt>,
    list: &[&str],
) -> Result<BTreeMap<String, Key>, String> {
    list.iter()
        .map(|name| Ok(((*name).to_owned(), key_of(receipts, name)?)))
        .collect()
}

fn entry(summary: &Summary, key: Key) -> Option<&crate::snapshot::Entry> {
    summary.entries.iter().find(|entry| entry.key == key)
}

fn status_of(problems: &[String], doubts: &[String]) -> Status {
    if !problems.is_empty() {
        Status::Fail
    } else if !doubts.is_empty() {
        Status::Unconfirmed
    } else {
        Status::Pass
    }
}

fn reason_of(problems: &[String], doubts: &[String], ok: String) -> String {
    let mut parts = Vec::new();
    if !problems.is_empty() {
        parts.push(problems.join("; "));
    }
    if !doubts.is_empty() {
        parts.push(format!(
            "принадлежность не проверена: {}",
            doubts.join("; ")
        ));
    }
    if parts.is_empty() {
        ok
    } else {
        parts.join("; ")
    }
}

/// Все члены сводки прочитаны: память и CPU измерены.
fn unread_members(summary: &Summary) -> Vec<String> {
    summary
        .entries
        .iter()
        .filter_map(|entry| {
            let memory = matches!(entry.memory, Some(Value::Measured(_)));
            let cpu = matches!(entry.cpu, Value::Measured(_));
            (!memory || !cpu).then(|| {
                format!(
                    "PID {}: память {:?}, CPU {:?}",
                    entry.key.pid, entry.memory, entry.cpu
                )
            })
        })
        .collect()
}

const ORACLE_SESSION: &str = "итог панели = {attach, хост, программа, ребёнок, внук} по ключам квитанций на обеих границах; каждый ключ в одном итоге; все члены прочитаны";
const ORACLE_FOREIGN: &str =
    "посторонний процесс не входит ни в один итог и в уникальное множество";

/// Клетка состава сессии.
pub fn session_cell(
    id: &str,
    attributions: &[&Attribution],
    owner: &Owner,
    expected: &BTreeMap<String, Key>,
    summary: &Summary,
) -> Cell {
    let mut problems = Vec::new();
    let mut doubts = Vec::new();
    for (index, attribution) in attributions.iter().enumerate() {
        for problem in composition_problems(attribution, owner, expected) {
            problems.push(format!("граница {}: {problem}", index + 1));
        }
        doubts.extend(session_doubts(attribution));
    }
    problems.extend(unread_members(summary));
    let ok = format!(
        "членов {}, память {} байт, покрытие памяти {}/{}, CPU {}/{}",
        expected.len(),
        summary.memory.bytes,
        summary.memory.coverage.counted,
        summary.memory.coverage.expected,
        summary.cpu.coverage.counted,
        summary.cpu.coverage.expected
    );
    Cell::new(
        id,
        status_of(&problems, &doubts),
        ORACLE_SESSION,
        reason_of(&problems, &doubts, ok),
    )
}

pub fn foreign_cell(attributions: &[&Attribution], foreign: Key) -> Cell {
    let found: Vec<String> = attributions
        .iter()
        .enumerate()
        .filter(|(_, attribution)| attribution.all().contains(&foreign))
        .map(|(index, _)| format!("граница {}", index + 1))
        .collect();
    if found.is_empty() {
        Cell::new(
            "sessions-foreign",
            Status::Pass,
            ORACLE_FOREIGN,
            format!("PID {} ни в одном итоге", foreign.pid),
        )
    } else {
        Cell::new(
            "sessions-foreign",
            Status::Fail,
            ORACLE_FOREIGN,
            format!("PID {} учтён: {}", foreign.pid, found.join(", ")),
        )
    }
}

/// Запись владения совпадает с квитанциями хоста и программы.
fn record_problems(
    record: &OwnerRecord,
    receipts: &BTreeMap<String, Receipt>,
    host: &str,
) -> Vec<String> {
    let mut problems = Vec::new();
    if key_of(receipts, host).ok() != Some(record.host) {
        problems.push(format!(
            "ключ хоста «{host}» в записи не совпал с квитанцией"
        ));
    }
    let program = format!("{host}/p");
    if receipts.contains_key(&program)
        && key_of(receipts, &program).ok() != record.programs.first().copied()
    {
        problems.push(format!(
            "ключ программы «{program}» в записи не совпал с квитанцией"
        ));
    }
    problems
}

fn add_record_problems(cell: Cell, problems: Vec<String>) -> Cell {
    if problems.is_empty() {
        return cell;
    }
    Cell::new(
        &cell.id,
        Status::Fail,
        &cell.oracle,
        format!("{}; {}", problems.join("; "), cell.reason),
    )
}

fn finish(stand: Stand, report: &mut Report) -> Result<(), String> {
    let faults = stand.finish()?;
    if !faults.is_empty() {
        return Err(format!("канал фикстур: {}", faults.join("; ")));
    }
    let _ = report;
    Ok(())
}

pub fn run_sessions(out: &Path) -> Result<Report, String> {
    let mut stand = Stand::start(out)?;
    let specs = [
        host("s1", 1, 1, 0),
        host("s2", 1, 1, 0),
        idle("s1-attach"),
        idle("s2-attach"),
        idle("foreign"),
    ];
    let ready = stand.spawn_ready(
        &specs,
        &names(&[
            "s1/p",
            "s1/p/c1",
            "s1/p/c1/c1",
            "s2/p",
            "s2/p/c1",
            "s2/p/c1/c1",
        ]),
    )?;
    let r1 = stand.owner_record("s1")?;
    let r2 = stand.owner_record("s2")?;
    let claims = Claims {
        panels: vec![
            panel("s1", vec![key_of(&ready, "s1-attach")?], Some(r1.clone())),
            panel("s2", vec![key_of(&ready, "s2-attach")?], Some(r2.clone())),
        ],
        git: Vec::new(),
    };
    let (s0, at0) = snap_claims(&System, &claims)?;
    let (s1, at1) = snap_claims(&System, &claims)?;
    let timebase = stand.timebase;
    let mut report = Report::default();
    finish(stand, &mut report)?;

    for (id, record) in [("s1", &r1), ("s2", &r2)] {
        let set = expected(
            &ready,
            &[
                &format!("{id}-attach"),
                id,
                &format!("{id}/p"),
                &format!("{id}/p/c1"),
                &format!("{id}/p/c1/c1"),
            ],
        )?;
        let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &owner(id), timebase)?;
        let cell = session_cell(
            &format!("sessions-{id}"),
            &[&at0, &at1],
            &owner(id),
            &set,
            &summary,
        );
        report.cells.push(add_record_problems(
            cell,
            record_problems(record, &ready, id),
        ));
        report.raw.insert(format!("summary.{id}"), json(&summary));
    }
    report
        .cells
        .push(foreign_cell(&[&at0, &at1], key_of(&ready, "foreign")?));
    report.raw.insert("claims".to_owned(), json(&claims));
    report
        .raw
        .insert("attribution".to_owned(), json(&[&at0, &at1]));
    report.raw.insert("ready".to_owned(), json(&ready));
    Ok(report)
}

const ORACLE_GIT_GROUP: &str = "итог группы «Git» = {git-work, git-work/c1}, запросившие {A, B}, оба прочитаны, ключ регистрации = ключ квитанции";
const ORACLE_GIT_ONCE: &str =
    "каждый процесс группы встречается в итогах один раз и не начислен ни A, ни B";

pub fn git_group_cell(
    attribution: &Attribution,
    expected: &BTreeMap<String, Key>,
    registered: Key,
    summary: &Summary,
) -> Cell {
    let mut problems = Vec::new();
    let got = attribution.members(&Owner::Git);
    let want: BTreeSet<Key> = expected.values().copied().collect();
    if got != want {
        problems.push(format!(
            "итог группы {:?}, ожидалось {:?}",
            got.iter().map(|k| k.pid).collect::<Vec<_>>(),
            want.iter().map(|k| k.pid).collect::<Vec<_>>()
        ));
    }
    let requested: BTreeSet<String> = attribution
        .share(&Owner::Git)
        .map(|share| share.requested_by.clone())
        .unwrap_or_default();
    if requested != BTreeSet::from(["A".to_owned(), "B".to_owned()]) {
        problems.push(format!("запросившие {requested:?}"));
    }
    if expected.get("git-work") != Some(&registered) {
        problems.push("ключ регистрации не совпал с квитанцией".to_owned());
    }
    problems.extend(unread_members(summary));
    let doubts = session_doubts(attribution);
    Cell::new(
        "git-shared-group",
        status_of(&problems, &doubts),
        ORACLE_GIT_GROUP,
        reason_of(
            &problems,
            &doubts,
            format!(
                "группа «Git»: {} процесса, память {} байт, запросившие A и B",
                got.len(),
                summary.memory.bytes
            ),
        ),
    )
}

pub fn git_once_cell(attributions: &[&Attribution], expected: &BTreeMap<String, Key>) -> Cell {
    let mut problems = Vec::new();
    for (index, attribution) in attributions.iter().enumerate() {
        for (name, key) in expected {
            let count = occurrences(attribution, *key);
            if count != 1 {
                problems.push(format!("граница {}: «{name}» {count} раз", index + 1));
            }
            for panel in ["A", "B"] {
                if attribution.members(&owner(panel)).contains(key) {
                    problems.push(format!("граница {}: «{name}» у панели {panel}", index + 1));
                }
            }
        }
    }
    Cell::new(
        "git-shared-once",
        if problems.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        ORACLE_GIT_ONCE,
        if problems.is_empty() {
            "каждый процесс группы учтён один раз, панелям A и B не начислен".to_owned()
        } else {
            problems.join("; ")
        },
    )
}

pub fn run_git_shared(out: &Path) -> Result<Report, String> {
    let mut stand = Stand::start(out)?;
    let mut ready = stand.spawn_ready(
        &[host("A", 0, 0, 0), host("B", 0, 0, 0)],
        &names(&["A/p", "B/p"]),
    )?;
    let ra = stand.owner_record("A")?;
    let rb = stand.owner_record("B")?;
    let mut work = Spec::new(Mode::Tree, "git-work");
    work.children = 1;
    let pid = stand.bench.spawn(&work)?;
    // Исполнитель общей работы: ключ ребёнка до waitpid, пока PID держит он.
    let registered = fixture::child_key(pid)?;
    ready.extend(
        stand
            .bench
            .wait_ready(&names(&["git-work", "git-work/c1"]), READY)?,
    );
    let claims = Claims {
        panels: vec![
            panel("A", Vec::new(), Some(ra)),
            panel("B", Vec::new(), Some(rb)),
        ],
        git: vec![GitWork {
            child: registered.key,
            requested_by: names(&["A", "B"]),
        }],
    };
    let (s0, at0) = snap_claims(&System, &claims)?;
    let (s1, at1) = snap_claims(&System, &claims)?;
    let timebase = stand.timebase;
    let mut report = Report::default();
    finish(stand, &mut report)?;

    let set = expected(&ready, &["git-work", "git-work/c1"])?;
    let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &Owner::Git, timebase)?;
    report
        .cells
        .push(git_group_cell(&at1, &set, registered.key, &summary));
    report.cells.push(git_once_cell(&[&at0, &at1], &set));
    report
        .raw
        .insert("registered".to_owned(), json(&registered));
    report.raw.insert("claims".to_owned(), json(&claims));
    report
        .raw
        .insert("attribution".to_owned(), json(&[&at0, &at1]));
    report.raw.insert("summary.git".to_owned(), json(&summary));
    Ok(report)
}

const ORACLE_REPARENT: &str = "родитель сироты в списке — 1, промежуточного процесса нет; сирота в итоге панели один раз, источники включают session и group, без tree; сирота прочитан";

/// Наблюдение `reparent`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ReparentSeen {
    pub orphan: Key,
    pub orphan_ppid: Option<i32>,
    pub middle_listed: bool,
}

pub fn reparent_cell(
    seen: &ReparentSeen,
    attribution: &Attribution,
    panel: &Owner,
    summary: &Summary,
) -> Cell {
    if seen.orphan_ppid != Some(1) {
        return Cell::not_run(
            "reparent",
            NotRunCause::NotObserved,
            ORACLE_REPARENT,
            format!(
                "ухода из дерева нет: родитель сироты {:?}",
                seen.orphan_ppid
            ),
        );
    }
    let mut problems = Vec::new();
    if seen.middle_listed {
        problems.push("промежуточный процесс ещё в списке".to_owned());
    }
    let origins: BTreeSet<Origin> = attribution
        .member(panel, seen.orphan)
        .map(|member| member.origins.clone())
        .unwrap_or_default();
    if origins.is_empty() {
        problems.push("сироты нет в итоге панели".to_owned());
    }
    if !origins.contains(&Origin::Session) || !origins.contains(&Origin::Group) {
        problems.push(format!("источники {origins:?} без session или group"));
    }
    if origins.contains(&Origin::Tree) {
        problems.push("сирота найдена деревом".to_owned());
    }
    let count = occurrences(attribution, seen.orphan);
    if count != 1 {
        problems.push(format!("сирота в итогах {count} раз"));
    }
    match entry(summary, seen.orphan).map(|e| e.memory) {
        Some(Some(Value::Measured(_))) => {}
        other => problems.push(format!("сирота не прочитана: {other:?}")),
    }
    let doubts = session_doubts(attribution);
    Cell::new(
        "reparent",
        status_of(&problems, &doubts),
        ORACLE_REPARENT,
        reason_of(
            &problems,
            &doubts,
            format!(
                "сирота PID {}, родитель 1, источники {:?}",
                seen.orphan.pid, origins
            ),
        ),
    )
}

pub fn run_reparent(out: &Path) -> Result<Report, String> {
    let mut stand = Stand::start(out)?;
    let ready = stand.spawn_ready(
        &[host("r", 0, 0, 1)],
        &names(&["r/p", "r/p/d1", "r/p/d1/o"]),
    )?;
    let record = stand.owner_record("r")?;
    let middle = ready
        .get("r/p/d1")
        .map(|r| r.process.pid)
        .ok_or("нет квитанции r/p/d1")?;
    stand.child_exit("r/p", middle)?;
    let claims = Claims {
        panels: vec![panel("r", Vec::new(), Some(record))],
        git: Vec::new(),
    };
    let (s0, at0) = snap_claims(&System, &claims)?;
    let (s1, at1) = snap_claims(&System, &claims)?;
    let timebase = stand.timebase;
    let mut report = Report::default();
    finish(stand, &mut report)?;

    let orphan = key_of(&ready, "r/p/d1/o")?;
    let middle_key = key_of(&ready, "r/p/d1")?;
    let seen = ReparentSeen {
        orphan,
        orphan_ppid: s1.listed.get(&orphan).map(|l| l.ppid),
        middle_listed: s1.listed.contains_key(&middle_key),
    };
    let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &owner("r"), timebase)?;
    report
        .cells
        .push(reparent_cell(&seen, &at1, &owner("r"), &summary));
    report.raw.insert("seen".to_owned(), json(&seen));
    report
        .raw
        .insert("attribution".to_owned(), json(&[&at0, &at1]));
    report.raw.insert("summary".to_owned(), json(&summary));
    Ok(report)
}

const ORACLE_FRESH: &str = "у a2 CPU — «свежий» без повторного PID, память измерена";
const ORACLE_EXITED: &str =
    "у c/p/c1 и a1 CPU — «вышел», в памяти не ожидаются; у q2 заметка «вышел», в множестве его нет";
const ORACLE_PARTIAL: &str =
    "partial истинен; CPU учтён ровно у хоста, программы и c/p/c2; у неучтённых своя причина";
const ORACLE_ZOMBIE: &str =
    "наблюдение: зомби q1 в списке S1, итоги вызовов и запись сводки записаны";

/// Ключи `churn`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ChurnKeys {
    pub a1: Key,
    pub a2: Key,
    pub c1: Key,
    pub q1: Key,
    pub q2: Key,
    pub with_base: BTreeSet<Key>,
}

pub fn churn_cells(
    keys: &ChurnKeys,
    summary: &Summary,
    current: &Snapshot,
    at1: &Attribution,
) -> Vec<Cell> {
    let mut cells = Vec::new();
    let a2 = entry(summary, keys.a2);
    let fresh_ok = a2.is_some_and(|e| {
        e.cpu == Value::Missing(Gap::Fresh { reused_pid: false })
            && matches!(e.memory, Some(Value::Measured(_)))
    });
    cells.push(Cell::new(
        "churn-fresh",
        if fresh_ok { Status::Pass } else { Status::Fail },
        ORACLE_FRESH,
        format!("a2: {:?}", a2.map(|e| (&e.memory, &e.cpu))),
    ));

    let mut problems = Vec::new();
    for (name, key) in [("c/p/c1", keys.c1), ("a1", keys.a1)] {
        match entry(summary, key) {
            Some(e) if e.cpu == Value::Missing(Gap::Exited) && e.memory.is_none() => {}
            other => problems.push(format!("{name}: {:?}", other.map(|e| (&e.memory, &e.cpu)))),
        }
    }
    let q2_noted = at1.notes.iter().any(
        |note| matches!(note, Note::Exited { role: Role::Surface, key, .. } if *key == keys.q2),
    );
    if !q2_noted {
        problems.push("у q2 нет заметки «вышел»".to_owned());
    }
    if at1.all().contains(&keys.q2) {
        problems.push("q2 в множестве".to_owned());
    }
    cells.push(Cell::new(
        "churn-exited",
        if problems.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        ORACLE_EXITED,
        if problems.is_empty() {
            "c/p/c1 и a1 — «вышел» без памяти, q2 — заметка «вышел»".to_owned()
        } else {
            problems.join("; ")
        },
    ));

    let counted: BTreeSet<Key> = summary
        .entries
        .iter()
        .filter(|e| matches!(e.cpu, Value::Measured(_)))
        .map(|e| e.key)
        .collect();
    let gaps: Vec<String> = summary
        .entries
        .iter()
        .filter_map(|e| match e.cpu {
            Value::Missing(gap) => Some(format!("PID {}: {gap:?}", e.key.pid)),
            Value::Measured(_) => None,
        })
        .collect();
    let partial_ok = summary.partial && counted == keys.with_base;
    cells.push(Cell::new(
        "churn-partial",
        if partial_ok { Status::Pass } else { Status::Fail },
        ORACLE_PARTIAL,
        format!(
            "partial {}, CPU {}/{} (доля {:.2}), память {}/{}, учтены {:?}, ожидались {:?}; причины: {}",
            summary.partial,
            summary.cpu.coverage.counted,
            summary.cpu.coverage.expected,
            summary.cpu.coverage.share(),
            summary.memory.coverage.counted,
            summary.memory.coverage.expected,
            counted.iter().map(|k| k.pid).collect::<Vec<_>>(),
            keys.with_base.iter().map(|k| k.pid).collect::<Vec<_>>(),
            gaps.join("; ")
        ),
    ));

    let zombie = current.listed.get(&keys.q1);
    cells.push(match zombie {
        Some(listed) if listed.zombie => Cell::new(
            "churn-zombie",
            Status::Pass,
            ORACLE_ZOMBIE,
            format!(
                "q1 PID {} в списке зомби; чтение снимка {:?}; запись сводки {:?}",
                keys.q1.pid,
                current
                    .reads
                    .get(&keys.q1)
                    .map(|r| (&r.reading, &r.session, &r.info)),
                entry(summary, keys.q1).map(|e| (&e.memory, &e.cpu))
            ),
        ),
        other => Cell::not_run(
            "churn-zombie",
            NotRunCause::NotObserved,
            ORACLE_ZOMBIE,
            format!("зомби q1 в списке нет: {other:?}"),
        ),
    });
    cells
}

pub fn run_churn(out: &Path) -> Result<Report, String> {
    let mut stand = Stand::start(out)?;
    let mut ready = stand.spawn_ready(
        &[host("c", 2, 0, 0), idle("a1")],
        &names(&["c/p", "c/p/c1", "c/p/c2"]),
    )?;
    let record = stand.owner_record("c")?;
    let a1 = key_of(&ready, "a1")?;
    let claims0 = Claims {
        panels: vec![panel("c", vec![a1], Some(record.clone()))],
        git: Vec::new(),
    };
    let (s0, at0) = snap_claims(&System, &claims0)?;

    let c1 = key_of(&ready, "c/p/c1")?;
    stand.bench.exit("c/p/c1")?;
    stand.child_exit("c/p", c1.pid)?;
    stand.bench.exit("a1")?;
    stand.bench.wait_exit(&names(&["a1"]), EXIT)?;
    stand.bench.reap(a1.pid)?;
    let (_, a2_ready) = stand.spawn_one(&idle("a2"))?;
    ready.insert("a2".to_owned(), a2_ready);
    let (q1_pid, q1_ready) = stand.spawn_one(&Spec::new(Mode::QuickExit, "q1"))?;
    stand.bench.wait_exited(q1_pid, EXIT)?;
    ready.insert("q1".to_owned(), q1_ready);
    let (q2_pid, q2_ready) = stand.spawn_one(&Spec::new(Mode::QuickExit, "q2"))?;
    stand.bench.wait_exited(q2_pid, EXIT)?;
    stand.bench.reap(q2_pid)?;
    ready.insert("q2".to_owned(), q2_ready);

    let keys = ChurnKeys {
        a1,
        a2: key_of(&ready, "a2")?,
        c1,
        q1: key_of(&ready, "q1")?,
        q2: key_of(&ready, "q2")?,
        with_base: [
            key_of(&ready, "c")?,
            key_of(&ready, "c/p")?,
            key_of(&ready, "c/p/c2")?,
        ]
        .into_iter()
        .collect(),
    };
    let claims1 = Claims {
        panels: vec![panel("c", vec![keys.a2, keys.q1, keys.q2], Some(record))],
        git: Vec::new(),
    };
    let (s1, at1) = snap_claims(&System, &claims1)?;
    let zombie_calls = calls(q1_pid);
    let timebase = stand.timebase;
    let mut report = Report::default();
    finish(stand, &mut report)?;

    let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &owner("c"), timebase)?;
    report.cells.extend(churn_cells(&keys, &summary, &s1, &at1));
    report.raw.insert("keys".to_owned(), json(&keys));
    report.raw.insert("zombie_calls".to_owned(), zombie_calls);
    report
        .raw
        .insert("zombie_listed".to_owned(), json(&s1.listed.get(&keys.q1)));
    report
        .raw
        .insert("claims".to_owned(), json(&[&claims0, &claims1]));
    report
        .raw
        .insert("attribution".to_owned(), json(&[&at0, &at1]));
    report.raw.insert("summary".to_owned(), json(&summary));
    Ok(report)
}

const ORACLE_CLOSED: &str =
    "на S1 итог панели {хост, программа, ребёнок}, у всех трёх дельта CPU от S0, у x1 — «вышел»";
const ORACLE_MOVED: &str = "на S2 итог {x2, хост, программа, ребёнок}, у хоста и программ дельта CPU от S1, у x2 — «свежий»";

fn continuity_problems(summary: &Summary, session: &BTreeMap<String, Key>) -> Vec<String> {
    session
        .iter()
        .filter_map(|(name, key)| match entry(summary, *key).map(|e| e.cpu) {
            Some(Value::Measured(_)) => None,
            other => Some(format!("{name}: CPU {other:?}")),
        })
        .collect()
}

pub fn attach_cells(
    session: &BTreeMap<String, Key>,
    x1: Key,
    x2: Key,
    closed: (&Attribution, &Summary),
    moved: (&Attribution, &Summary),
    panel: &Owner,
) -> Vec<Cell> {
    let mut problems = composition_problems(closed.0, panel, session);
    problems.extend(continuity_problems(closed.1, session));
    match entry(closed.1, x1).map(|e| e.cpu) {
        Some(Value::Missing(Gap::Exited)) => {}
        other => problems.push(format!("x1: CPU {other:?}")),
    }
    let doubts = session_doubts(closed.0);
    let first = Cell::new(
        "attach-closed",
        status_of(&problems, &doubts),
        ORACLE_CLOSED,
        reason_of(
            &problems,
            &doubts,
            format!(
                "сессия из {} процессов осталась у панели, x1 — «вышел»",
                session.len()
            ),
        ),
    );
    let mut with_x2 = session.clone();
    with_x2.insert("x2".to_owned(), x2);
    let mut problems = composition_problems(moved.0, panel, &with_x2);
    problems.extend(continuity_problems(moved.1, session));
    match entry(moved.1, x2).map(|e| e.cpu) {
        Some(Value::Missing(Gap::Fresh { .. })) => {}
        other => problems.push(format!("x2: CPU {other:?}")),
    }
    let doubts = session_doubts(moved.0);
    let second = Cell::new(
        "attach-move",
        status_of(&problems, &doubts),
        ORACLE_MOVED,
        reason_of(
            &problems,
            &doubts,
            "после переноса сессия та же, x2 — «свежий»".to_owned(),
        ),
    );
    vec![first, second]
}

pub fn run_attach_move(out: &Path) -> Result<Report, String> {
    let mut stand = Stand::start(out)?;
    let mut ready = stand.spawn_ready(
        &[host("m", 1, 0, 0), idle("x1")],
        &names(&["m/p", "m/p/c1"]),
    )?;
    let record = stand.owner_record("m")?;
    let x1 = key_of(&ready, "x1")?;
    let claims = |surface: Vec<Key>| Claims {
        panels: vec![panel("m", surface, Some(record.clone()))],
        git: Vec::new(),
    };
    let (s0, at0) = snap_claims(&System, &claims(vec![x1]))?;
    stand.bench.exit("x1")?;
    stand.bench.wait_exit(&names(&["x1"]), EXIT)?;
    stand.bench.reap(x1.pid)?;
    let (s1, at1) = snap_claims(&System, &claims(Vec::new()))?;
    let (_, x2_ready) = stand.spawn_one(&idle("x2"))?;
    ready.insert("x2".to_owned(), x2_ready);
    let x2 = key_of(&ready, "x2")?;
    let (s2, at2) = snap_claims(&System, &claims(vec![x2]))?;
    let timebase = stand.timebase;
    let mut report = Report::default();
    finish(stand, &mut report)?;

    let session = expected(&ready, &["m", "m/p", "m/p/c1"])?;
    let panel_m = owner("m");
    let sum1 = owner_summary(Some((&s0, &at0)), (&s1, &at1), &panel_m, timebase)?;
    let sum2 = owner_summary(Some((&s1, &at1)), (&s2, &at2), &panel_m, timebase)?;
    report.cells.extend(attach_cells(
        &session,
        x1,
        x2,
        (&at1, &sum1),
        (&at2, &sum2),
        &panel_m,
    ));
    report
        .raw
        .insert("attribution".to_owned(), json(&[&at0, &at1, &at2]));
    report
        .raw
        .insert("summaries".to_owned(), json(&[&sum1, &sum2]));
    Ok(report)
}

const ORACLE_LOGIN: &str = "login в итоге панели (record), на S1 proc_pid_rusage login отказал с errno 1 (EPERM), ребёнок прочитан, partial истинен, покрытие памяти на один меньше; оба живы после чтения";

/// Наблюдение `login`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct LoginSeen {
    pub login: Key,
    pub child: Key,
    /// Ключ `login` в новом списке после S1.
    pub login_alive_after: bool,
    /// Квитанция ребёнка после S1.
    pub child_alive_after: bool,
}

pub fn login_cell(
    seen: &LoginSeen,
    current: &Snapshot,
    attribution: &Attribution,
    summary: &Summary,
) -> Cell {
    let read = current.reads.get(&seen.login).map(|r| r.reading);
    if !current.listed.contains_key(&seen.login) || !seen.login_alive_after {
        return Cell::not_run(
            "login",
            NotRunCause::NotObserved,
            ORACLE_LOGIN,
            format!("отказ доступа не наблюдался: login вышел до чтения или во время ({read:?})"),
        );
    }
    if let Some(Ok(reading)) = read {
        return Cell::not_run(
            "login",
            NotRunCause::NotObserved,
            ORACLE_LOGIN,
            format!(
                "отказ доступа не наблюдался: живой login прочитан, footprint {} байт — отличие окружения от P0.3",
                reading.usage.phys_footprint
            ),
        );
    }
    let mut problems = Vec::new();
    let errno = match read {
        Some(Err(Unread::Failed { error })) => error.errno(),
        _ => None,
    };
    if errno != Some(libc::EPERM) {
        problems.push(format!("чтение login: {read:?}"));
    }
    let origins = attribution
        .member(&owner("l"), seen.login)
        .map(|m| m.origins.clone())
        .unwrap_or_default();
    if !origins.contains(&Origin::Record) {
        problems.push(format!("login в итоге с источниками {origins:?}"));
    }
    match entry(summary, seen.child).map(|e| e.memory) {
        Some(Some(Value::Measured(_))) => {}
        other => problems.push(format!("ребёнок не прочитан: {other:?}")),
    }
    if !seen.child_alive_after {
        problems.push("квитанции ребёнка после чтения нет".to_owned());
    }
    let coverage = summary.memory.coverage;
    if !summary.partial || coverage.counted + 1 != coverage.expected {
        problems.push(format!(
            "partial {}, покрытие памяти {}/{}",
            summary.partial, coverage.counted, coverage.expected
        ));
    }
    let doubts = session_doubts(attribution);
    Cell::new(
        "login",
        status_of(&problems, &doubts),
        ORACLE_LOGIN,
        reason_of(
            &problems,
            &doubts,
            format!(
                "login PID {} не прочитан (errno {}), ребёнок PID {} прочитан, покрытие памяти {}/{}, partial",
                seen.login.pid,
                libc::EPERM,
                seen.child.pid,
                coverage.counted,
                coverage.expected
            ),
        ),
    )
}

pub fn run_login(out: &Path) -> Result<Report, String> {
    let mut stand = Stand::start(out)?;
    let mut spec = Spec::new(Mode::Host, "l");
    spec.program = Program::Login;
    let started = stand
        .bench
        .spawn(&spec)
        .and_then(|_| stand.bench.wait_ready(&names(&["l", "l/p/c"]), READY));
    let spawned: Vec<serde_json::Value> = stand
        .bench
        .seen("l")
        .map(|seen| {
            seen.events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        Event::Spawned { .. } | Event::Error { .. } | Event::ChildExit { .. }
                    )
                })
                .map(json)
                .collect()
        })
        .unwrap_or_default();
    let mut report = Report::default();
    report.raw.insert("host_events".to_owned(), json(&spawned));
    let ready = match started {
        Ok(ready) => ready,
        Err(reason) => {
            // Отказ остановки не скрывает причину: она в заметке.
            if let Err(err) = stand.finish() {
                report.notes.push(format!("остановка стенда: {err}"));
            }
            report.cells.push(Cell::not_run(
                "login",
                NotRunCause::Precondition,
                ORACLE_LOGIN,
                format!("login не запустился: {reason}"),
            ));
            return Ok(report);
        }
    };
    let record = stand.owner_record("l")?;
    let login = *record
        .programs
        .first()
        .ok_or("в записи владения нет программы")?;
    let claims = Claims {
        panels: vec![panel("l", Vec::new(), Some(record))],
        git: Vec::new(),
    };
    let (s0, at0) = snap_claims(&System, &claims)?;
    let (s1, at1) = snap_claims(&System, &claims)?;
    let login_calls = calls(login.pid);
    let child = key_of(&ready, "l/p/c")?;
    let child_calls = calls(child.pid);
    let child_alive_after = stand
        .bench
        .receipt("l/p/c", "alive", stand::RECEIPT)
        .is_ok();
    let login_alive_after = stand::listing_now()?.contains_key(&login);
    let timebase = stand.timebase;
    finish(stand, &mut report)?;

    let seen = LoginSeen {
        login,
        child,
        login_alive_after,
        child_alive_after,
    };
    let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &owner("l"), timebase)?;
    report.cells.push(login_cell(&seen, &s1, &at1, &summary));
    report.raw.insert("seen".to_owned(), json(&seen));
    report.raw.insert("login_calls".to_owned(), login_calls);
    report.raw.insert("child_calls".to_owned(), child_calls);
    report
        .raw
        .insert("login_listed".to_owned(), json(&s1.listed.get(&login)));
    report
        .raw
        .insert("child_ready".to_owned(), json(&ready.get("l/p/c")));
    report
        .raw
        .insert("attribution".to_owned(), json(&[&at0, &at1]));
    report.raw.insert("summary".to_owned(), json(&summary));
    Ok(report)
}

/// Подставной отказ: `proc_pid_rusage` и `proc_pidinfo` одного PID отвечают `EPERM`.
pub struct Inject<'a, S: Source> {
    pub inner: &'a S,
    pub denied: i32,
}

impl<S: Source> Source for Inject<'_, S> {
    fn list(&self) -> Result<Vec<Listed>, String> {
        self.inner.list()
    }

    fn usage(&self, pid: i32) -> Result<Usage, ReadError> {
        if pid == self.denied {
            return Err(ReadError::Denied);
        }
        self.inner.usage(pid)
    }

    fn session(&self, pid: i32) -> Result<i32, ReadError> {
        self.inner.session(pid)
    }

    fn bsd_info(&self, pid: i32) -> Result<BsdInfo, ReadError> {
        if pid == self.denied {
            return Err(ReadError::Denied);
        }
        self.inner.bsd_info(pid)
    }
}

const ORACLE_EPERM: &str = "у e/p/c1 память и CPU — отказ EPERM, partial истинен, покрытие памяти на один меньше, память = Σ остальных; в контрольной сводке e/p/c1 прочитан";

pub fn eperm_cell(denied: Key, summary: &Summary, control: &Summary) -> Cell {
    let mut problems = Vec::new();
    let failed = Gap::Unread {
        unread: Unread::Failed {
            error: ReadError::Denied,
        },
    };
    match entry(summary, denied) {
        Some(e) if e.memory == Some(Value::Missing(failed)) && e.cpu == Value::Missing(failed) => {}
        other => problems.push(format!("e/p/c1: {:?}", other.map(|e| (&e.memory, &e.cpu)))),
    }
    let measured: u64 = summary
        .entries
        .iter()
        .filter_map(|e| match e.memory {
            Some(Value::Measured(bytes)) => Some(bytes),
            _ => None,
        })
        .sum();
    let coverage = summary.memory.coverage;
    if !summary.partial
        || coverage.counted + 1 != coverage.expected
        || summary.memory.bytes != measured
    {
        problems.push(format!(
            "partial {}, покрытие памяти {}/{}, память {} при Σ {measured}",
            summary.partial, coverage.counted, coverage.expected, summary.memory.bytes
        ));
    }
    match entry(control, denied).map(|e| e.memory) {
        Some(Some(Value::Measured(_))) => {}
        other => problems.push(format!("контроль: {other:?}")),
    }
    Cell::new(
        "eperm-inject",
        if problems.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        ORACLE_EPERM,
        if problems.is_empty() {
            format!(
                "e/p/c1 PID {} — отказ EPERM в памяти и CPU, покрытие памяти {}/{}, partial, контроль прочитан",
                denied.pid, coverage.counted, coverage.expected
            )
        } else {
            problems.join("; ")
        },
    )
}

pub fn run_eperm_inject(out: &Path) -> Result<Report, String> {
    let mut stand = Stand::start(out)?;
    let ready = stand.spawn_ready(&[host("e", 1, 0, 0)], &names(&["e/p", "e/p/c1"]))?;
    let record = stand.owner_record("e")?;
    let denied = key_of(&ready, "e/p/c1")?;
    let claims = Claims {
        panels: vec![panel("e", Vec::new(), Some(record))],
        git: Vec::new(),
    };
    let inject = Inject {
        inner: &System,
        denied: denied.pid,
    };
    let (s0, at0) = snap_claims(&inject, &claims)?;
    let (s1, at1) = snap_claims(&inject, &claims)?;
    let (c1, atc) = snap_claims(&System, &claims)?;
    let timebase = stand.timebase;
    let mut report = Report::default();
    finish(stand, &mut report)?;

    let panel_e = owner("e");
    let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &panel_e, timebase)?;
    let control = owner_summary(Some((&s1, &at1)), (&c1, &atc), &panel_e, timebase)?;
    report.cells.push(eperm_cell(denied, &summary, &control));
    report.raw.insert("summary".to_owned(), json(&summary));
    report.raw.insert("control".to_owned(), json(&control));
    report
        .raw
        .insert("attribution".to_owned(), json(&[&at0, &at1, &atc]));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::{FakeSource, start};
    use crate::proc::units::Timebase;

    const TB: Timebase = Timebase {
        numer: 125,
        denom: 3,
    };

    fn proc_entry(pid: i32, ppid: i32, pgid: i32) -> Listed {
        Listed {
            pid,
            ppid,
            pgid,
            start: start(pid as u64),
            ruid: 501,
            uid: 501,
            comm: format!("p{pid}"),
            zombie: false,
        }
    }

    /// Стенд 50; хост 100, программа 101 (лидер сессии и группы), ребёнок 102, attach 103,
    /// посторонний 104.
    fn world() -> FakeSource {
        let mut source = FakeSource::default();
        for (pid, ppid, pgid, sid) in [
            (50, 1, 50, 50),
            (100, 50, 50, 50),
            (101, 100, 101, 101),
            (102, 101, 101, 101),
            (103, 50, 50, 50),
            (104, 50, 50, 50),
        ] {
            source.add(proc_entry(pid, ppid, pgid), 1_000 + pid as u64, 10);
            source.session.insert(pid, Ok(sid));
        }
        source
    }

    fn key(source: &FakeSource, pid: i32) -> Key {
        Key::of(source.listed.iter().find(|l| l.pid == pid).unwrap())
    }

    fn claims(source: &FakeSource) -> Claims {
        let program = key(source, 101);
        Claims {
            panels: vec![panel(
                "p",
                vec![key(source, 103)],
                Some(OwnerRecord {
                    host: key(source, 100),
                    programs: vec![program],
                    group: program,
                    session: program,
                }),
            )],
            git: Vec::new(),
        }
    }

    fn session_set(source: &FakeSource) -> BTreeMap<String, Key> {
        [
            ("host", 100),
            ("program", 101),
            ("child", 102),
            ("attach", 103),
        ]
        .into_iter()
        .map(|(name, pid)| (name.to_owned(), key(source, pid)))
        .collect()
    }

    fn two(
        source: &impl Source,
        claims: &Claims,
    ) -> (Snapshot, Attribution, Snapshot, Attribution) {
        let (s0, at0) = snap_claims(source, claims).unwrap();
        let (s1, at1) = snap_claims(source, claims).unwrap();
        (s0, at0, s1, at1)
    }

    #[test]
    fn session_and_foreign_cells_follow_the_receipts() {
        let source = world();
        let claims = claims(&source);
        let (s0, at0, s1, at1) = two(&source, &claims);
        let p = owner("p");
        let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &p, TB).unwrap();
        let set = session_set(&source);
        let cell = session_cell("sessions-p", &[&at0, &at1], &p, &set, &summary);
        assert_eq!(cell.status, Status::Pass, "{}", cell.reason);
        assert_eq!(
            foreign_cell(&[&at0, &at1], key(&source, 104)).status,
            Status::Pass
        );
        assert_eq!(
            foreign_cell(&[&at0, &at1], key(&source, 102)).status,
            Status::Fail
        );

        let mut short = set.clone();
        short.remove("child");
        let extra = session_cell("sessions-p", &[&at1], &p, &short, &summary);
        assert_eq!(extra.status, Status::Fail);
        assert!(extra.reason.contains("лишний PID 102"), "{}", extra.reason);
        let mut more = set;
        more.insert("foreign".to_owned(), key(&source, 104));
        let missing = session_cell("sessions-p", &[&at1], &p, &more, &summary);
        assert!(
            missing.reason.contains("нет «foreign»"),
            "{}",
            missing.reason
        );
    }

    #[test]
    fn session_unknown_other_than_exit_makes_the_cell_unconfirmed() {
        let mut source = world();
        source.session.insert(104, Err(ReadError::Denied));
        let claims = claims(&source);
        let (s0, at0, s1, at1) = two(&source, &claims);
        let p = owner("p");
        let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &p, TB).unwrap();
        let cell = session_cell("sessions-p", &[&at1], &p, &session_set(&source), &summary);
        assert_eq!(cell.status, Status::Unconfirmed, "{}", cell.reason);
        source.session.insert(104, Err(ReadError::Exited));
        let (_, _, _, at_exit) = two(&source, &claims);
        assert!(session_doubts(&at_exit).is_empty());
    }

    #[test]
    fn injected_denial_is_marked_and_not_hidden() {
        let source = world();
        let claims = claims(&source);
        let inject = Inject {
            inner: &source,
            denied: 102,
        };
        let (s0, at0, s1, at1) = two(&inject, &claims);
        let (c1, atc) = snap_claims(&source, &claims).unwrap();
        let p = owner("p");
        let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &p, TB).unwrap();
        let control = owner_summary(Some((&s1, &at1)), (&c1, &atc), &p, TB).unwrap();
        let cell = eperm_cell(key(&source, 102), &summary, &control);
        assert_eq!(cell.status, Status::Pass, "{}", cell.reason);
        let clean = owner_summary(Some((&c1, &atc)), (&c1, &atc), &p, TB);
        assert!(clean.is_err(), "та же граница — ΔT ≤ 0");
        assert_eq!(
            eperm_cell(key(&source, 102), &control, &control).status,
            Status::Fail,
            "без подставного отказа клетка не проходит"
        );
    }

    #[test]
    fn login_cell_tells_denial_from_a_readable_or_gone_login() {
        let mut source = world();
        source.usage.insert(101, Err(ReadError::Denied));
        source.info.insert(101, Err(ReadError::Denied));
        let claims = claims(&source);
        let (s0, at0, s1, at1) = two(&source, &claims);
        let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &owner("p"), TB).unwrap();
        let mut seen = LoginSeen {
            login: key(&source, 101),
            child: key(&source, 102),
            login_alive_after: true,
            child_alive_after: true,
        };
        // Клетка ищет панель «l»: сценарий называет так панель login.
        let renamed = Attribution {
            shares: at1
                .shares
                .iter()
                .cloned()
                .map(|mut share| {
                    if share.owner == owner("p") {
                        share.owner = owner("l");
                    }
                    share
                })
                .collect(),
            notes: at1.notes.clone(),
        };
        let cell = login_cell(&seen, &s1, &renamed, &summary);
        assert_eq!(cell.status, Status::Pass, "{}", cell.reason);

        seen.login_alive_after = false;
        let gone = login_cell(&seen, &s1, &renamed, &summary);
        assert_eq!(gone.status, Status::NotRun);
        assert!(gone.reason.contains("вышел"));

        let readable = world();
        let (r0, ra0, r1, ra1) = two(&readable, &claims);
        let rsummary = owner_summary(Some((&r0, &ra0)), (&r1, &ra1), &owner("p"), TB).unwrap();
        seen.login_alive_after = true;
        let read = login_cell(&seen, &r1, &ra1, &rsummary);
        assert_eq!(read.status, Status::NotRun);
        assert!(read.reason.contains("отличие окружения от P0.3"));
    }

    #[test]
    fn reparent_requires_leaving_the_tree_and_session_and_group_origins() {
        let mut source = world();
        let orphan = proc_entry(105, 1, 101);
        source.add(orphan.clone(), 2_000, 10);
        source.session.insert(105, Ok(101));
        let claims = claims(&source);
        let (s0, at0, s1, at1) = two(&source, &claims);
        let p = owner("p");
        let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &p, TB).unwrap();
        let mut seen = ReparentSeen {
            orphan: Key::of(&orphan),
            orphan_ppid: Some(1),
            middle_listed: false,
        };
        let cell = reparent_cell(&seen, &at1, &p, &summary);
        assert_eq!(cell.status, Status::Pass, "{}", cell.reason);
        seen.middle_listed = true;
        assert_eq!(
            reparent_cell(&seen, &at1, &p, &summary).status,
            Status::Fail
        );
        seen.orphan_ppid = Some(101);
        assert_eq!(
            reparent_cell(&seen, &at1, &p, &summary).status,
            Status::NotRun
        );
    }

    #[test]
    fn git_cells_reject_a_panel_share_and_a_wrong_group() {
        let source = world();
        let mut claims = claims(&source);
        claims.panels[0].id = "A".to_owned();
        claims.git = vec![GitWork {
            child: key(&source, 104),
            requested_by: names(&["A", "B"]),
        }];
        let (s0, at0, s1, at1) = two(&source, &claims);
        let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &Owner::Git, TB).unwrap();
        let set = BTreeMap::from([("git-work".to_owned(), key(&source, 104))]);
        assert_eq!(
            git_group_cell(&at1, &set, key(&source, 104), &summary).status,
            Status::Pass
        );
        assert_eq!(git_once_cell(&[&at0, &at1], &set).status, Status::Pass);
        assert_eq!(
            git_group_cell(&at1, &set, key(&source, 103), &summary).status,
            Status::Fail,
            "ключ регистрации не совпал"
        );
        let panel_key = BTreeMap::from([("git-work".to_owned(), key(&source, 102))]);
        let cell = git_once_cell(&[&at1], &panel_key);
        assert_eq!(cell.status, Status::Fail);
        assert!(cell.reason.contains("у панели A"), "{}", cell.reason);
    }

    #[test]
    fn churn_zombie_cell_needs_a_listed_zombie() {
        let mut source = world();
        let zombie = Listed {
            zombie: true,
            ..proc_entry(106, 50, 50)
        };
        source.listed.push(zombie.clone());
        source
            .usage
            .insert(106, Ok(crate::proc::fake::usage_of(3_000, 1)));
        let claims = claims(&source);
        let (s0, at0, s1, at1) = two(&source, &claims);
        let summary = owner_summary(Some((&s0, &at0)), (&s1, &at1), &owner("p"), TB).unwrap();
        let mut keys = ChurnKeys {
            a1: key(&source, 103),
            a2: key(&source, 103),
            c1: key(&source, 102),
            q1: Key::of(&zombie),
            q2: key(&source, 104),
            with_base: BTreeSet::new(),
        };
        let cells = churn_cells(&keys, &summary, &s1, &at1);
        let by_id = |id: &str| cells.iter().find(|c| c.id == id).unwrap().status;
        assert_eq!(by_id("churn-zombie"), Status::Pass);
        assert_eq!(by_id("churn-fresh"), Status::Fail, "a2 не свежий");
        assert_eq!(by_id("churn-exited"), Status::Fail, "c/p/c1 не вышел");
        assert_eq!(by_id("churn-partial"), Status::Fail, "сводка полная");
        keys.q1 = key(&source, 104);
        let cells = churn_cells(&keys, &summary, &s1, &at1);
        assert_eq!(
            cells
                .iter()
                .find(|c| c.id == "churn-zombie")
                .unwrap()
                .status,
            Status::NotRun
        );
    }
}
