//! Рубеж A — жизненный цикл сессии (план P0.4, «Рубежи и клетки»), клетки A1–A7 на zmx текущей
//! комбинации (задача 7) и на Rust-хосте (задача 11: A7 и сравнение A2 после attach ждут снимка).
//! Стенд и пути сокетов — `matrix::stand`.

use std::thread;
use std::time::Instant;

use serde_json::json;

use crate::args::Target;
use crate::backend::zmx;
use crate::backend::{self, BackendError, InputPath, Liveness, SessionId, Workload};
use crate::fixture::channel::{Command, Event, JobChange, Link};
use crate::fixture::{self, Mode, flood_line};
use crate::launch;
use crate::matrix::Candidate;
use crate::matrix::evidence::{Evidence, Spec, cell, later};
use crate::matrix::stand::{
    COLS, DETACH_KEY, STOP_GRACE, Session, Stand, WAIT, barrier, compare_states, emit, identity,
    same_alive,
};
use crate::observer::{self, Identity, ProcessKey};
use crate::oracle::vt::Reference;
use crate::results::{Cell, DefectOrigin, Environment, Report};

const FLOOD_LINES: u64 = 4000;
/// Причина `not_run` клеток Rust-хоста, которым нужен снимок.
pub const SNAPSHOT_LATER: &str = "до задачи 12: attach-клиент Rust-хоста пока без снимка";
/// Одновременных `ensure` одной сессии в A4.
const RACERS: usize = 8;
/// Причины выхода фикстуры при потере PTY.
const PTY_LOSS: [&str; 2] = ["hangup", "pty-closed"];

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    let Some(candidate) = target.backend else {
        return Err("lifecycle идёт на кандидате".to_owned());
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
    report.cells.extend(a1(stand)?);
    report.cells.push(a2(stand)?);
    report.cells.push(a3(stand)?);
    report.cells.extend(a4(stand)?);
    report.cells.extend(a5(stand)?);
    report.cells.extend(a6(stand)?);
    report.cells.push(match stand.candidate {
        Candidate::Zmx => a7(stand)?,
        Candidate::Rust => later("A7", None, SNAPSHOT_LATER),
    });
    Ok(report)
}

fn a1(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    for (part, label, stimulus) in [
        (
            "client-sigkill",
            "a1k",
            "SIGKILL attach-клиента, который ведёт сессию",
        ),
        (
            "gui-hangup",
            "a1g",
            "гибель GUI: стенд закрывает все дескрипторы мастера PTY клиента, ядро шлёт клиенту \
             SIGHUP; настоящий SIGKILL процесса GUI — клетка C1",
        ),
    ] {
        let id = stand.session(label)?;
        let mut session = stand.ensure(&id, Mode::Emitter)?;
        let mut evidence = Evidence::default();
        evidence.note("record", &session.record);
        a1_part(stand, &mut session, part, &mut evidence)?;
        let notes = stand.discard(&id);
        evidence.note("cleanup", notes);
        let artifact = stand.artifact(&format!("a1-{part}.json"), &evidence)?;
        cells.push(cell(
            Spec {
                id: "A1",
                part: Some(part),
                stimulus: format!("{stimulus}; затем новый клиент подключается к той же сессии"),
                oracle: "наблюдатель процессов: PID и время старта нагрузки и хоста до и после; \
                         список сессий; барьер DSR через нового клиента",
                completion: "клиент вышел; нагрузка и хост — те же живые процессы; новый клиент \
                             стал лидером и передал ответ барьера",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact, stand.log_name(&id)],
        ));
    }
    Ok(cells)
}

fn a1_part(
    stand: &Stand,
    session: &mut Session,
    part: &str,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let mut first = stand.client(&session.id, "c1")?;
    let Some(role) = evidence.require("c1 лидер", first.wait_leader()) else {
        return Ok(());
    };
    evidence.note("c1.role", role);
    if evidence
        .require("барьер через c1", barrier(&mut session.link, &first))
        .is_none()
    {
        return Ok(());
    }
    let exit = if part == "client-sigkill" {
        first.kill(stand.main())?
    } else {
        first.hangup()?
    };
    evidence.note("c1.exit", format!("{exit:?}"));
    same_alive(evidence, "нагрузка та же и жива", session.record.workload)?;
    same_alive(evidence, "хост тот же и жив", session.record.host)?;
    let liveness = stand
        .main()
        .liveness(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.check(
        "сессия в списке",
        matches!(liveness, Liveness::Live { workload_pid, .. } if workload_pid == session.record.workload.pid),
        format!("{liveness:?}"),
    );
    let second = stand.client(&session.id, "c2")?;
    if evidence.require("c2 лидер", second.wait_leader()).is_some() {
        evidence.require("барьер через c2", barrier(&mut session.link, &second));
    }
    same_alive(
        evidence,
        "нагрузка та же после нового клиента",
        session.record.workload,
    )?;
    Ok(())
}

// --- A2 ---

fn a2(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("a2")?;
    let mut session = stand.ensure(&id, Mode::Flood)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    a2_body(stand, &mut session, &mut evidence)?;
    let artifact = stand.artifact("a2.json", &evidence)?;
    stand.discard(&id);
    Ok(cell(
        Spec {
            id: "A2",
            part: None,
            stimulus: format!(
                "клиент c1 подключился и отключился клавишей Ctrl-\\, затем без клиентов фикстура \
                 пишет {FLOOD_LINES} строк по {COLS} колонок и ED 3 (очистка истории: перенос \
                 истории — клетка B1), затем подключается c2; сессия без единого клиента — клетка A7"
            ),
            oracle: "квитанции фикстуры: поток закончен, записи и задержки записи; ревизия \
                     обработки хоста до и после; после attach — эталон и приёмник c2",
            completion: "квитанция конца потока; барьер DSR через c2; сравнение эталона с \
                         приёмником",
            origin: DefectOrigin::Unknown,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ))
}

fn a2_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let mut reference = Reference::new(&stand.config).map_err(|err| format!("эталон: {err}"))?;
    let mut first = stand.client(&session.id, "c1")?;
    if evidence.require("c1 лидер", first.wait_leader()).is_none()
        || evidence
            .require("барьер через c1", barrier(&mut session.link, &first))
            .is_none()
    {
        return Ok(());
    }
    first.input(DETACH_KEY)?;
    let Some(exit) = evidence.require("c1 отключился клавишей", first.wait_exit())
    else {
        return Ok(());
    };
    evidence.note("c1.exit", format!("{exit:?}"));
    let liveness = stand
        .main()
        .liveness(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.check(
        "без клиентов",
        matches!(liveness, Liveness::Live { clients: 0, .. }),
        format!("{liveness:?}"),
    );
    let before = stand
        .main()
        .read_boundary(&session.id)
        .map_err(|err| err.to_string())?;
    session
        .link
        .request(
            Command::Flood {
                lines: FLOOD_LINES,
                width: COLS,
                interval_us: 0,
                report_every: FLOOD_LINES / 4,
            },
            WAIT,
        )
        .map_err(|err| format!("команда потока: {err}"))?;
    let started = Instant::now();
    let done = session
        .link
        .wait_for(WAIT * 4, |receipt| match &receipt.event {
            Event::FloodDone {
                lines,
                bytes,
                cancelled,
                writes,
                ..
            } => Some((*lines, *bytes, *cancelled, *writes)),
            _ => None,
        });
    let Some((lines, bytes, cancelled, writes)) =
        evidence.require("поток закончен без клиентов", done)
    else {
        return Ok(());
    };
    evidence.note("flood.elapsed_ms", started.elapsed().as_millis() as u64);
    evidence.note("flood.writes", writes);
    evidence.check(
        "все строки записаны",
        lines == FLOOD_LINES && !cancelled,
        format!(
            "строк {lines}, байт {bytes}, отменён {cancelled}, задержек {} на {} мс",
            writes.stalls,
            writes.stalled_ns / 1_000_000
        ),
    );
    for index in 1..=FLOOD_LINES {
        reference.feed(&flood_line(&session.fixture_nonce, index, COLS));
    }
    emit(&mut session.link, &mut reference, b"\x1b[3J")?;
    let after = stand
        .main()
        .read_boundary(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.note("revision", json!({ "before": before, "after": after }));
    evidence.check(
        "хост обработал поток без клиентов",
        after > before,
        format!("ревизия {before} → {after}"),
    );
    let second = stand.client(&session.id, "c2")?;
    if evidence.require("c2 лидер", second.wait_leader()).is_none()
        || evidence
            .require("барьер через c2", barrier(&mut session.link, &second))
            .is_none()
    {
        return Ok(());
    }
    match stand.candidate {
        Candidate::Zmx => {
            compare_states(evidence, "конец потока у c2", &reference, &second)?;
        }
        Candidate::Rust => {
            evidence.not_proven = Some(format!("конец потока после attach: {SNAPSHOT_LATER}"))
        }
    }
    Ok(())
}

// --- A3 ---

fn a3(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("a3")?;
    let mut session = stand.ensure(&id, Mode::Shell)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    a3_body(stand, &mut session, &mut evidence)?;
    let artifact = stand.artifact("a3.json", &evidence)?;
    stand.discard(&id);
    Ok(cell(
        Spec {
            id: "A3",
            part: None,
            stimulus:
                "через attach-клиент, ведущий сессию: run /bin/sleep 600, Ctrl-Z, bg 1, fg 1, \
                       Ctrl-C"
                    .to_owned(),
            oracle: "квитанции заданий и сигналов фикстуры shell с tcgetpgrp внутреннего PTY; \
                     группа переднего плана PTY клиента (tcgetpgrp мастера)",
            completion: "квитанция каждого шага; клиент жив и остаётся группой переднего плана \
                         своего PTY",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ))
}

fn job(link: &mut Link) -> Result<(i32, JobChange, Option<i32>), String> {
    link.wait_for(WAIT, |receipt| match &receipt.event {
        Event::Job {
            pid,
            change,
            foreground_pgid,
            ..
        } => Some((*pid, change.clone(), *foreground_pgid)),
        _ => None,
    })
    .map_err(|err| err.to_string())
}

fn shell_done(link: &mut Link, line: &str) -> Result<Option<i32>, String> {
    let (error, foreground) = link
        .wait_for(WAIT, |receipt| match &receipt.event {
            Event::Shell {
                line: done,
                error,
                foreground_pgid,
                ..
            } if done == line => Some((error.clone(), *foreground_pgid)),
            _ => None,
        })
        .map_err(|err| err.to_string())?;
    match error {
        None => Ok(foreground),
        Some(error) => Err(format!("{line}: {error}")),
    }
}

fn a3_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let shell = session.record.workload.pid;
    let client = stand.client(&session.id, "c1")?;
    if evidence.require("c1 лидер", client.wait_leader()).is_none() {
        return Ok(());
    }
    let outer = client.pid();
    let outer_ok = |evidence: &mut Evidence, step: &str| -> Result<(), String> {
        let foreground = client.foreground()?;
        evidence.check(
            &format!("{step}: PTY клиента"),
            foreground == outer,
            format!("tcgetpgrp {foreground}, клиент {outer}"),
        );
        Ok(())
    };
    client.input(b"run /bin/sleep 600\n")?;
    let Some((job_pid, change, foreground)) = evidence.require("run", job(&mut session.link))
    else {
        return Ok(());
    };
    evidence.check(
        "run: задание на переднем плане",
        change == JobChange::Started { foreground: true } && foreground == Some(job_pid),
        format!("{change:?}, tcgetpgrp {foreground:?}, задание {job_pid}"),
    );
    let tpgid = observer::inspect(shell)?.and_then(|info| info.tpgid);
    evidence.check(
        "run: tpgid shell",
        tpgid == Some(job_pid),
        format!("{tpgid:?}"),
    );
    outer_ok(evidence, "run")?;

    client.input(b"\x1a")?;
    let Some((_, change, foreground)) = evidence.require("Ctrl-Z", job(&mut session.link)) else {
        return Ok(());
    };
    evidence.check(
        "Ctrl-Z: задание остановлено, терминал у shell",
        change
            == JobChange::Stopped {
                signal: "SIGTSTP".to_owned(),
            }
            && foreground == Some(shell),
        format!("{change:?}, tcgetpgrp {foreground:?}"),
    );
    outer_ok(evidence, "Ctrl-Z")?;

    client.input(b"bg 1\n")?;
    if evidence
        .require("bg 1", shell_done(&mut session.link, "bg 1"))
        .is_none()
    {
        return Ok(());
    }
    let Some((_, change, _)) = evidence.require("bg 1: квитанция", job(&mut session.link))
    else {
        return Ok(());
    };
    evidence.check(
        "bg 1: задание продолжено в фоне",
        change == JobChange::Continued { foreground: false },
        format!("{change:?}"),
    );

    client.input(b"fg 1\n")?;
    let Some(foreground) = evidence.require("fg 1", shell_done(&mut session.link, "fg 1")) else {
        return Ok(());
    };
    evidence.check(
        "fg 1: терминал у задания",
        foreground == Some(job_pid),
        format!("tcgetpgrp {foreground:?}"),
    );
    let Some((_, change, _)) = evidence.require("fg 1: квитанция", job(&mut session.link))
    else {
        return Ok(());
    };
    evidence.check(
        "fg 1: задание на переднем плане",
        change == JobChange::Continued { foreground: true },
        format!("{change:?}"),
    );

    client.input(b"\x03")?;
    let Some((_, change, foreground)) = evidence.require("Ctrl-C", job(&mut session.link)) else {
        return Ok(());
    };
    evidence.check(
        "Ctrl-C: задание прервано, терминал у shell",
        change
            == JobChange::Signaled {
                signal: "SIGINT".to_owned(),
            }
            && foreground == Some(shell),
        format!("{change:?}, tcgetpgrp {foreground:?}"),
    );
    outer_ok(evidence, "Ctrl-C")?;
    let alive = observer::inspect(outer)?.is_some_and(|info| info.is_alive());
    evidence.check(
        "клиент жив после Ctrl-Z и Ctrl-C",
        alive,
        format!("PID {outer}"),
    );
    Ok(())
}

// --- A4 ---

fn a4(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let id = stand.session("a4r")?;
    let mut session = stand.ensure(&id, Mode::Emitter)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    a4_reattach(stand, &mut session, &mut evidence)?;
    let artifact = stand.artifact("a4-reattach.json", &evidence)?;
    stand.discard(&id);
    let reattach = cell(
        Spec {
            id: "A4",
            part: Some("reattach"),
            stimulus: "attach, отключение Ctrl-\\, повторный attach и повторный ensure той же \
                       сессии"
                .to_owned(),
            oracle: "наблюдатель процессов: ключ нагрузки; процессы с nonce нагрузки в аргументах; \
                     список сессий",
            completion: "второй клиент стал лидером; повторный ensure вернулся",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    );

    let id = stand.session("a4c")?;
    stand.sessions.push(id.clone());
    let mut evidence = Evidence::default();
    a4_concurrent(stand, &id, &mut evidence)?;
    let artifact = stand.artifact("a4-concurrent.json", &evidence)?;
    stand.discard(&id);
    let concurrent = cell(
        Spec {
            id: "A4",
            part: Some("concurrent-ensure"),
            stimulus: format!(
                "{RACERS} одновременных ensure одной сессии, у каждого своя нагрузка"
            ),
            oracle: "наблюдатель процессов: процессы с общим префиксом nonce нагрузок; сокеты в \
                     ZMX_DIR и каталоге подготовки; список сессий; готовность фикстуры",
            completion: "все ensure вернулись",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    );
    Ok(vec![reattach, concurrent])
}

/// Процессы с `token` в аргументах: ключи по PID.
fn processes_with(token: &str) -> Result<Vec<ProcessKey>, String> {
    let mut keys = Vec::new();
    for process in launch::find_by_token(token) {
        if let Some(info) = observer::inspect(process.pid)?
            && info.is_alive()
        {
            keys.push(info.key);
        }
    }
    keys.sort();
    Ok(keys)
}

fn a4_reattach(
    stand: &Stand,
    session: &mut Session,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let workload = session.record.workload;
    let mut first = stand.client(&session.id, "c1")?;
    if evidence.require("c1 лидер", first.wait_leader()).is_none() {
        return Ok(());
    }
    first.input(DETACH_KEY)?;
    if evidence
        .require("c1 отключился", first.wait_exit())
        .is_none()
    {
        return Ok(());
    }
    let second = stand.client(&session.id, "c2")?;
    if evidence.require("c2 лидер", second.wait_leader()).is_none()
        || evidence
            .require("барьер через c2", barrier(&mut session.link, &second))
            .is_none()
    {
        return Ok(());
    }
    let workload_args = stand.workload(Mode::Emitter, &session.fixture_nonce)?;
    let again = stand
        .main()
        .ensure(&session.id, &workload_args, stand.size)
        .map_err(|err| format!("повторный ensure: {err}"))?;
    evidence.note("ensure_again", &again);
    evidence.check(
        "повторный ensure не создал нагрузку",
        !again.created && again.record.workload == workload,
        format!(
            "created {}, нагрузка {:?}",
            again.created, again.record.workload
        ),
    );
    same_alive(evidence, "нагрузка та же", workload)?;
    let found = processes_with(&session.fixture_nonce)?;
    let mut expected = vec![session.record.host, workload];
    expected.sort();
    evidence.note("processes", &found);
    evidence.check(
        "одна нагрузка и один хост",
        found == expected,
        format!("{found:?}, ожидалось {expected:?}"),
    );
    let listed = stand.main().list().map_err(|err| err.to_string())?;
    let entries: Vec<_> = listed
        .iter()
        .filter(|entry| entry.name == session.id.as_str())
        .collect();
    evidence.check(
        "одна сессия в списке",
        entries.len() == 1 && entries[0].workload_pid == Some(workload.pid),
        format!("{entries:?}"),
    );
    let log = stand.daemon_log(&session.id);
    evidence.note("leaders", zmx::leadership_events(&log));
    Ok(())
}

fn a4_concurrent(stand: &Stand, id: &SessionId, evidence: &mut Evidence) -> Result<(), String> {
    let prefix = format!("{id}-w");
    let workloads: Vec<(String, Workload)> = (0..RACERS)
        .map(|index| {
            let nonce = format!("{prefix}{index}");
            stand
                .workload(Mode::Emitter, &nonce)
                .map(|workload| (nonce, workload))
        })
        .collect::<Result<_, _>>()?;
    let (backend, size) = (stand.main(), stand.size);
    let results: Vec<_> = thread::scope(|scope| {
        let handles: Vec<_> = workloads
            .iter()
            .map(|(_, workload)| scope.spawn(move || backend.ensure(id, workload, size)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().map_err(|_| "поток ensure упал".to_owned()))
            .collect()
    });
    let mut ensured = Vec::new();
    for (index, result) in results.into_iter().enumerate() {
        match result? {
            Ok(value) => ensured.push((index, value)),
            Err(err) => {
                evidence.check(&format!("ensure {index}"), false, err);
            }
        }
    }
    evidence.note(
        "ensured",
        ensured
            .iter()
            .map(|(index, value)| json!({ "index": index, "created": value.created, "record": value.record, "command": value.command }))
            .collect::<Vec<_>>(),
    );
    if ensured.len() != RACERS {
        return Ok(());
    }
    let keys: Vec<ProcessKey> = ensured
        .iter()
        .map(|(_, value)| value.record.workload)
        .collect();
    evidence.check(
        "все ensure вернули одну нагрузку",
        keys.windows(2).all(|pair| pair[0] == pair[1]),
        format!("{keys:?}"),
    );
    let winners: Vec<usize> = ensured
        .iter()
        .filter(|(_, value)| value.created)
        .map(|(index, _)| *index)
        .collect();
    evidence.check(
        "нагрузку запустил ровно один ensure",
        winners.len() == 1,
        format!("{winners:?}"),
    );
    let host = ensured[0].1.record.host;
    let found = processes_with(&prefix)?;
    let mut expected = vec![host, keys[0]];
    expected.sort();
    evidence.note("processes", &found);
    evidence.check(
        "один хост и одна нагрузка",
        found == expected,
        format!("{found:?}, ожидалось {expected:?}"),
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
    let listed = stand.main().list().map_err(|err| err.to_string())?;
    let entries = listed
        .iter()
        .filter(|entry| entry.name == id.as_str())
        .count();
    evidence.check("одна сессия в списке", entries == 1, format!("{entries}"));
    if let [winner] = winners.as_slice() {
        let (link, ready) = stand.accept(None)?;
        evidence.note("ready", &ready);
        evidence.check(
            "готова нагрузка победителя",
            ready.nonce == workloads[*winner].0 && ready.pid == keys[0].pid,
            format!("{} PID {}", ready.nonce, ready.pid),
        );
        drop(link);
    }
    Ok(())
}

// --- A5 ---

fn a5(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    for (part, label, stimulus, completion) in [
        (
            "workload-exit",
            "a5x",
            "нагрузка выходит сама (команда Exit фикстуре) при подключённом клиенте",
            "выход нагрузки и хоста пойман kqueue; клиент вышел",
        ),
        (
            "stop",
            "a5s",
            "stop сессии (zmx kill) при подключённом клиенте",
            "stop вернулся после ожидания гибели процессов; клиент вышел",
        ),
    ] {
        let id = stand.session(label)?;
        let mut session = stand.ensure(&id, Mode::Emitter)?;
        let mut evidence = Evidence::default();
        evidence.note("record", &session.record);
        a5_part(stand, &mut session, part, &mut evidence)?;
        let artifact = stand.artifact(&format!("a5-{part}.json"), &evidence)?;
        stand.discard(&id);
        cells.push(cell(
            Spec {
                id: "A5",
                part: Some(part),
                stimulus: stimulus.to_owned(),
                oracle: "наблюдатель процессов по PID и времени старта: хост, нагрузка и её \
                         потомки; сокет — только сопутствующее наблюдение",
                completion,
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact, stand.log_name(&id)],
        ));
    }
    Ok(cells)
}

fn a5_part(
    stand: &Stand,
    session: &mut Session,
    part: &str,
    evidence: &mut Evidence,
) -> Result<(), String> {
    let mut client = stand.client(&session.id, "c1")?;
    if evidence.require("c1 лидер", client.wait_leader()).is_none() {
        return Ok(());
    }
    let record = session.record.clone();
    if part == "workload-exit" {
        session
            .link
            .request(Command::Exit { code: 0 }, WAIT)
            .map_err(|err| format!("команда выхода: {err}"))?;
        let survivors = launch::wait_exit(&[record.workload.pid, record.host.pid], WAIT);
        evidence.check(
            "нагрузка и хост вышли",
            survivors.is_empty(),
            format!("живы: {survivors:?}"),
        );
    } else {
        let outcome = stand
            .main()
            .stop(&session.id, STOP_GRACE)
            .map_err(|err| format!("stop: {err}"))?;
        evidence.note("stop", &outcome);
        evidence.check(
            "команда остановки",
            outcome.command.code == Some(0),
            format!("{:?}", outcome.command),
        );
        evidence.check(
            "stop подтверждён гибелью процессов",
            outcome.confirmed(),
            format!(
                "процессов {}, живы после команды {}, выжили {:?}, ждали {} мс",
                outcome.processes.len(),
                outcome.alive_after_command.len(),
                outcome.survivors,
                outcome.waited_ms
            ),
        );
    }
    for (name, key) in [("нагрузка", record.workload), ("хост", record.host)] {
        let found = identity(key)?;
        evidence.check(
            &format!("{name} не исполняется"),
            !found.is_alive(),
            format!("{found:?}"),
        );
    }
    let exit = client.wait_exit();
    evidence.require("клиент вышел", exit);
    let liveness = stand
        .main()
        .liveness(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.note("liveness", &liveness);
    evidence.note(
        "socket_present",
        stand.main().socket_path(&session.id).exists(),
    );
    Ok(())
}

// --- A6 ---

fn a6(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    for (part, label, mode, stimulus) in [
        (
            "shell",
            "a6s",
            Mode::Shell,
            "SIGKILL хосту сессии с фикстурой shell",
        ),
        (
            "agent",
            "a6a",
            Mode::Agent,
            "SIGKILL хосту сессии с фикстурой agent и её потомком nohup-child, игнорирующим HUP",
        ),
        (
            "background",
            "a6b",
            Mode::Shell,
            "SIGKILL хосту сессии с фикстурой shell и фоновым заданием nohup-child, \
             игнорирующим HUP (запущено через zmx send)",
        ),
    ] {
        let id = stand.session(label)?;
        let mut session = stand.ensure(&id, mode)?;
        let mut evidence = Evidence::default();
        evidence.note("record", &session.record);
        a6_part(stand, &mut session, part, &mut evidence)?;
        let artifact = stand.artifact(&format!("a6-{part}.json"), &evidence)?;
        stand.discard(&id);
        cells.push(cell(
            Spec {
                id: "A6",
                part: Some(part),
                stimulus: stimulus.to_owned(),
                oracle: "наблюдатель процессов по записи, сохранённой при ensure и перед сбоем \
                         (PID и время старта хоста, нагрузки и потомков); живые процессы с nonce \
                         нагрузки против записи; квитанции нагрузки и процесса, игнорирующего \
                         HUP; состояние сессии адаптера",
                completion: "выход хоста и нагрузки пойман kqueue; квитанция выхода нагрузки; \
                             каждый сохранённый процесс классифицирован, найденный отвечает на \
                             команду",
                origin: DefectOrigin::Host,
            },
            &evidence,
            vec![artifact, stand.log_name(&id)],
        ));
    }
    Ok(cells)
}

fn a6_part(
    stand: &Stand,
    session: &mut Session,
    part: &str,
    evidence: &mut Evidence,
) -> Result<(), String> {
    // Потомок, игнорирующий HUP: его канал и ключ.
    let mut hup_child: Option<(Link, ProcessKey)> = None;
    match part {
        "agent" => {
            let spawned = session
                .link
                .wait_for(WAIT, |receipt| match &receipt.event {
                    Event::Spawned { pid, mode } => Some((*pid, *mode)),
                    _ => None,
                })
                .map_err(|err| format!("потомок agent: {err}"))?;
            let (link, ready) = stand.accept(Some(&format!("{}-child", session.fixture_nonce)))?;
            if ready.pid != spawned.0 {
                return Err(format!("потомок agent: PID {} и {}", ready.pid, spawned.0));
            }
            let key = observer::inspect(ready.pid)?
                .ok_or("потомок agent вышел до сбоя")?
                .key;
            hup_child = Some((link, key));
        }
        "background" => {
            let nonce = format!("{}-bg", session.fixture_nonce);
            let args =
                fixture::command_args(Mode::NohupChild, stand.listener()?.path(), &nonce, None);
            let line = format!("run {} {} &\n", stand.fixture.display(), args.join(" "));
            stand
                .main()
                .input(&session.id, line.as_bytes(), InputPath::Raw)
                .map_err(|err| format!("ввод без клиента: {err}"))?;
            let (_, change, _) = job(&mut session.link)?;
            if change != (JobChange::Started { foreground: false }) {
                return Err(format!("фоновое задание: {change:?}"));
            }
            let (link, ready) = stand.accept(Some(&nonce))?;
            let key = observer::inspect(ready.pid)?
                .ok_or("фоновое задание вышло до сбоя")?
                .key;
            hup_child = Some((link, key));
        }
        _ => {}
    }
    let saved = backend::session_processes(&session.record).map_err(|err| err.to_string())?;
    evidence.note("saved", &saved);
    let host = stand
        .main()
        .kill_host(&session.id, WAIT)
        .map_err(|err| format!("kill_host: {err}"))?;
    evidence.note("killed_host", host);
    // Выход нагрузки доказывают её квитанция и kqueue; наблюдатель после kqueue видит процесс в
    // `exit()`, поэтому смена родителя потомков — наблюдение, а не проверка.
    let exit = session.link.wait_for(WAIT, |receipt| match &receipt.event {
        Event::Exit { reason, .. } => Some(reason.clone()),
        _ => None,
    });
    let left = launch::wait_exit(&[session.record.workload.pid], WAIT);
    let exit = exit.map_err(|err| err.to_string());
    evidence.note("workload_exit", &exit);
    evidence.check(
        "нагрузка вышла по потере PTY",
        left.is_empty()
            && exit
                .as_deref()
                .is_ok_and(|reason| PTY_LOSS.contains(&reason)),
        format!(
            "квитанция {exit:?}, жива после ожидания: {}",
            !left.is_empty()
        ),
    );
    let sockets = stand
        .main()
        .socket_entries()
        .map_err(|err| format!("сокеты кандидата: {err}"))?;
    evidence.note("sockets_before_liveness", &sockets);
    let liveness = stand
        .main()
        .liveness(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.check(
        "состояние «хост потерян»",
        liveness == Liveness::Lost,
        format!("{liveness:?}"),
    );
    let mut found = Vec::new();
    for key in &saved {
        let identity = identity(*key)?;
        let ppid = match &identity {
            Identity::Same(info) => Some(info.ppid),
            _ => None,
        };
        found.push(json!({
            "key": key,
            "alive": identity.is_alive(),
            "identity": identity,
            "ppid": ppid,
        }));
    }
    evidence.note("found", found);
    let survivors = backend::alive(&saved).map_err(|err| err.to_string())?;
    match stand.main().remaining(&session.id) {
        Ok(remaining) => {
            evidence.note("remaining_by_record", &remaining);
            evidence.check(
                "запись на диске находит выживших",
                survivors.iter().all(|key| remaining.contains(key)),
                format!("по записи {remaining:?}, выжили {survivors:?}"),
            );
        }
        Err(BackendError::Unsupported(what)) => evidence.note("remaining_by_record", what),
        Err(err) => {
            evidence.check("запись на диске находит выживших", false, err);
        }
    }
    let running = processes_with(&session.fixture_nonce)?;
    evidence.note("survivors", &survivors);
    evidence.note("running_with_nonce", &running);
    let unsaved: Vec<&ProcessKey> = running.iter().filter(|key| !saved.contains(key)).collect();
    evidence.check(
        "оставшиеся процессы сессии найдены по записи",
        unsaved.is_empty(),
        format!("живы по записи {survivors:?}, вне записи {unsaved:?}"),
    );
    if let Some((mut link, key)) = hup_child {
        evidence.check(
            "процесс, игнорирующий HUP, среди сохранённых",
            saved.contains(&key),
            format!("PID {}", key.pid),
        );
        if survivors.contains(&key) {
            // Квитанция команды — доказательство, что найденный процесс исполняется; затем уборка.
            evidence.require(
                "процесс, игнорирующий HUP, отвечает",
                link.request(Command::Exit { code: 0 }, WAIT),
            );
        } else {
            evidence.not_proven = Some(format!(
                "процесс, игнорирующий HUP (PID {}), не пережил сбой: стенд не создал условие \
                 клетки",
                key.pid
            ));
        }
        let cleaned = launch::wait_exit(&[key.pid], WAIT).is_empty();
        evidence.note("cleanup_hup_child_exited", cleaned);
    }
    Ok(())
}

// --- A7 ---

/// Долгая работа без клиента: заголовок, перо, позиционирование, режимы и много строк.
fn a7_steps() -> Vec<Vec<u8>> {
    let mut steps = vec![
        b"\x1b]2;pss-a7\x07".to_vec(),
        b"\x1b[1;31mred\x1b[0m plain\r\n".to_vec(),
    ];
    for line in 0..60 {
        steps.push(format!("headless line {line:03}\r\n").into_bytes());
    }
    steps.extend([
        b"\x1b[5;10H\x1b[4mpositioned\x1b[0m".to_vec(),
        b"\x1b[?2004h\x1b[?1h".to_vec(),
        b"\x1b[12;3Hcursor here".to_vec(),
    ]);
    steps
}

fn a7(stand: &mut Stand) -> Result<Cell, String> {
    let id = stand.session("a7")?;
    let mut session = stand.ensure(&id, Mode::Emitter)?;
    let mut evidence = Evidence::default();
    evidence.note("record", &session.record);
    a7_body(stand, &mut session, &mut evidence)?;
    let artifact = stand.artifact("a7.json", &evidence)?;
    stand.discard(&id);
    Ok(cell(
        Spec {
            id: "A7",
            part: None,
            stimulus:
                "сессия создана ensure и ни разу не имела клиента; фикстура пишет заголовок, \
                       перо, 62 строки, позиционирование и режимы; повторный ensure; затем первый \
                       attach"
                    .to_owned(),
            oracle: "эталон и приёмник первого клиента; журнал демона: назначения лидера и \
                     отправка снимка; ревизия обработки без клиента",
            completion: "c1 стал лидером, барьер DSR через c1 пройден, состояния сняты",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ))
}

fn a7_body(stand: &Stand, session: &mut Session, evidence: &mut Evidence) -> Result<(), String> {
    let mut reference = Reference::new(&stand.config).map_err(|err| format!("эталон: {err}"))?;
    for step in a7_steps() {
        emit(&mut session.link, &mut reference, &step)?;
    }
    let snapshot = stand
        .main()
        .snapshot(&session.id)
        .map_err(|err| err.to_string())?;
    evidence.note("headless_snapshot", &snapshot);
    evidence.check(
        "хост обработал вывод без клиента",
        snapshot.text.contains("cursor here"),
        format!("ревизия {}", snapshot.revision),
    );
    let again = stand
        .main()
        .ensure(
            &session.id,
            &stand.workload(Mode::Emitter, &session.fixture_nonce)?,
            stand.size,
        )
        .map_err(|err| format!("повторный ensure: {err}"))?;
    evidence.note("ensure_again", &again);
    let log = stand.daemon_log(&session.id);
    let leaders = zmx::leadership_events(&log);
    evidence.check(
        "ensure без скрытого attach",
        leaders == 0 && !again.created,
        format!("назначений лидера до attach: {leaders}"),
    );
    let client = stand.client(&session.id, "c1")?;
    if evidence.require("c1 лидер", client.wait_leader()).is_none()
        || evidence
            .require("барьер через c1", barrier(&mut session.link, &client))
            .is_none()
    {
        return Ok(());
    }
    compare_states(evidence, "первый attach", &reference, &client)?;
    let log = stand.daemon_log(&session.id);
    evidence.note(
        "daemon_log",
        json!({
            "leaders": zmx::leadership_events(&log),
            "snapshots": zmx::snapshot_events(&log),
        }),
    );
    Ok(())
}
