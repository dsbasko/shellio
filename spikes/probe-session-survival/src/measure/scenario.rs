//! Сценарий `measure` на кандидате: сравнительные замеры по разделу плана «Замеры». Наблюдения
//! без вердикта, клеток у сценария нет; сырые ряды — в `measure.json`.
//!
//! Ход на кандидате:
//! 1. сверка перевода тиков CPU с `getrusage` — предусловие, иначе ошибка исполнения;
//! 2. attach: [`REPEATS`] сессий, в каждой история [`HISTORY_LINES`] строк без клиента, затем
//!    холодный attach (первое восстановление этой истории хостом) и прогретый (сразу следом);
//! 3. CPU хоста, клиента и приёмника в простое и под потоком с фиксированным темпом, под потоком
//!    также на МиБ вывода по квитанциям хода; процесс стенда — контроль счётчика пробуждений;
//! 4. скрытые сессии 1, 8 и 32 с той же историей: их память и CPU в простое и задержка эха в
//!    видимой сессии.
//!
//! zmx при первом attach после работы без единого клиента снимка не отдаёт (A7), поэтому каждая
//! сессия attach до истории один раз подключает и отключает клиента c0, на обоих кандидатах.
//! Пока идёт окно сэмплера или отмеряется attach и эхо, стенд кандидата не опрашивает.

use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::args::Target;
use crate::backend::{Liveness, SessionId};
use crate::fixture::channel::{Command, Event, Link, LinkError, Receipt, hex};
use crate::fixture::{Mode, flood_line};
use crate::matrix::stand::{Client, DETACH_KEY, Session, Stand, WAIT};
use crate::measure::{
    self, COLS, Echo, EchoLegs, FLOOD_INTERVAL_US, FLOOD_LINES, FLOOD_REPORT_EVERY, GroupLines,
    HIDDEN, HIDDEN_WINDOW, HISTORY_LINES, KEYSTROKES, LINE_WIDTH, Part, REPEATS, ROWS, Reading,
    SURFACE_THREAD, Series, Timebase, UNITS_BURN_NS, UNITS_RANGE, WINDOW, Watch, mib,
};
use crate::observer::{self, ProcessKey};
use crate::oracle::vt::{BARRIER, BARRIER_REPLY, Capture, Config};
use crate::results::{Environment, Report};
use crate::sys;

/// Срок истории без клиента: запись и разбор хостом.
const FILL_WAIT: Duration = Duration::from_secs(120);
/// Начало строки потока, по которому видно, что хост её обработал.
const PREFIX: usize = 32;
const ARTIFACT: &str = "measure.json";

pub fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    let candidate = target.backend.ok_or("measure идёт на кандидате")?;
    let mut stand = Stand::for_candidate(&target.out, environment, candidate)?;
    stand.config = Config::new(COLS, ROWS);
    stand.size = stand.config.winsize(COLS, ROWS);
    let outcome = body(&mut stand);
    let cleanup = stand.teardown();
    let mut report = outcome?;
    report.diagnostics.notes.extend(cleanup);
    Ok(report)
}

#[derive(Default, Serialize)]
struct Raw {
    conditions: serde_json::Value,
    units: serde_json::Value,
    attach: Vec<AttachRecord>,
    sessions: Vec<SessionMemory>,
    cpu: Vec<CpuSeries>,
    /// Пробуждения процесса стенда в окнах простоя: контроль счётчика.
    wakeup_control: Vec<f64>,
    hidden: Vec<HiddenRecord>,
    loc: Vec<GroupLines>,
}

fn body(stand: &mut Stand) -> Result<Report, String> {
    let timebase = Timebase::system()?;
    let mut report = Report::default();
    let mut raw = Raw {
        conditions: json!({
            "cols": COLS,
            "rows": ROWS,
            "history_lines": HISTORY_LINES,
            "line_width": LINE_WIDTH,
            "repeats": REPEATS,
            "window_ms": WINDOW.as_millis() as u64,
            "hidden_window_ms": HIDDEN_WINDOW.as_millis() as u64,
            "flood_interval_us": FLOOD_INTERVAL_US,
            "flood_report_every": FLOOD_REPORT_EVERY,
            "hidden": HIDDEN,
            "keystrokes": KEYSTROKES,
            "surface_thread": SURFACE_THREAD,
            "timebase": timebase,
        }),
        ..Raw::default()
    };
    let ratio = units_check(timebase)?;
    raw.units = json!({ "ratio": ratio, "range": [UNITS_RANGE.start(), UNITS_RANGE.end()] });
    report.diagnostics.notes.push(format!(
        "сверка перевода тиков CPU: proc_pid_rusage к getrusage {ratio:.3}"
    ));

    attach(stand, &mut raw, &mut report)?;
    let wakeups = cpu(stand, timebase, &mut raw, &mut report)?;
    hidden(stand, timebase, wakeups, &mut raw, &mut report)?;

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    raw.loc = measure::count_lines(root, stand.candidate)?;
    for group in &raw.loc {
        report.diagnostics.notes.push(format!(
            "объём кода, {}: файлов {}, Rust {} строк кода и {} тестов, сгенерировано {}, \
             данные тестов {}, патчи +{} −{}, скрипты {}",
            group.name,
            group.files,
            group.rust.code,
            group.rust.tests,
            group.generated,
            group.test_data,
            group.patch.added,
            group.patch.removed,
            group.script
        ));
    }
    stand.artifact(ARTIFACT, &raw)?;
    report
        .diagnostics
        .notes
        .push(format!("сырые ряды и записи — {ARTIFACT}"));
    Ok(report)
}

// --- общие шаги ---

/// Сверка перевода тиков `proc_pid_rusage` с `getrusage` на работе своего процесса.
fn units_check(timebase: Timebase) -> Result<f64, String> {
    let pid = std::process::id() as i32;
    let own = || {
        Ok::<_, String>((
            sys::usage(pid).ok_or("предусловие: proc_pid_rusage своего процесса")?,
            sys::rusage_self_ns().ok_or("предусловие: getrusage")?,
        ))
    };
    let (before, rusage_before) = own()?;
    let deadline = Instant::now() + WAIT;
    let mut value = 1u64;
    loop {
        for _ in 0..100_000 {
            value = std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
        let spent = sys::rusage_self_ns()
            .ok_or("предусловие: getrusage")?
            .saturating_sub(rusage_before);
        if spent >= UNITS_BURN_NS {
            break;
        }
        if Instant::now() >= deadline {
            return Err("предусловие: сверка тиков не набрала работы за срок".to_owned());
        }
    }
    let (after, rusage_after) = own()?;
    let ratio = measure::units_ratio(
        timebase.ns(after.cpu_ticks.saturating_sub(before.cpu_ticks)),
        rusage_after.saturating_sub(rusage_before),
        UNITS_BURN_NS,
    )
    .ok_or("предусловие: сверка тиков без работы")?;
    if !UNITS_RANGE.contains(&ratio) {
        return Err(format!(
            "предусловие: CPU по proc_pid_rusage к getrusage {ratio:.3}, вне допуска \
             {:.2}–{:.2}: перевод тиков неверен",
            UNITS_RANGE.start(),
            UNITS_RANGE.end()
        ));
    }
    Ok(ratio)
}

fn key_of(pid: i32) -> Result<ProcessKey, String> {
    observer::inspect(pid)?
        .map(|info| info.key)
        .ok_or_else(|| format!("процесса {pid} нет"))
}

/// Процессы окна живы и те же, что в начале: иначе ряд снят с другого процесса.
fn same_alive(keys: &[ProcessKey]) -> Result<(), String> {
    for key in keys {
        if !observer::identify(*key)?.is_alive() {
            return Err(format!("процесс {} сменился или вышел за окно", key.pid));
        }
    }
    Ok(())
}

/// Ждёт, пока у сессии станет `clients` клиентов. Опрос хоста — только вне окон замера.
fn wait_clients(stand: &Stand, id: &SessionId, clients: u32) -> Result<(), String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let liveness = stand
            .main()
            .liveness(id)
            .map_err(|err| format!("состояние {id}: {err}"))?;
        if matches!(liveness, Liveness::Live { clients: seen, .. } if seen == clients) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "у {id} нет {clients} клиентов за {} с: {liveness:?}",
                WAIT.as_secs()
            ));
        }
    }
}

/// Барьер через клиента: отметка `CLOCK_UPTIME_RAW` чтения ответа фикстурой. До ответа приёмник
/// обработал всё, что кандидат отдал раньше барьера, в том числе восстановление.
fn barrier_at(session: &mut Session) -> Result<u64, String> {
    let written = session
        .link
        .request(Command::Write { hex: hex(BARRIER) }, WAIT)
        .map_err(|err| format!("барьер: {err}"))?;
    if written != BARRIER.len() as u64 {
        return Err(format!("барьер: записано {written} байт"));
    }
    let reply = hex(BARRIER_REPLY);
    session
        .link
        .wait_for(WAIT, |receipt| match &receipt.event {
            Event::Response { hex, .. } if *hex == reply => Some(receipt.t_ns),
            _ => None,
        })
        .map_err(|err| format!("барьер: ответа приёмника нет: {err}"))
}

/// Клиент c0 подключается к пустой сессии и отключается клавишей: дальше у сессии нет клиентов,
/// но клиент у неё уже был (A7 у zmx).
fn prime(stand: &Stand, session: &mut Session) -> Result<(), String> {
    let mut client = stand.client(&session.id, "c0")?;
    client.wait_leader()?;
    barrier_at(session)?;
    client.input(DETACH_KEY)?;
    client.wait_exit()?;
    wait_clients(stand, &session.id, 0)
}

/// История [`HISTORY_LINES`] строк во всю ширину без клиента; итог — начало последней строки,
/// когда хост её обработал.
fn fill(stand: &Stand, session: &mut Session) -> Result<String, String> {
    let read_before = match stand.rust() {
        Ok(rust) => {
            rust.status(&session.id)
                .map_err(|err| format!("состояние {}: {err}", session.id))?
                .read_bytes
        }
        Err(_) => 0,
    };
    session
        .link
        .request(
            Command::Flood {
                lines: HISTORY_LINES,
                width: LINE_WIDTH,
                interval_us: 0,
                report_every: HISTORY_LINES,
            },
            WAIT,
        )
        .map_err(|err| format!("история {}: {err}", session.id))?;
    let (lines, bytes, cancelled) = session
        .link
        .wait_for(FILL_WAIT, |receipt| match &receipt.event {
            Event::FloodDone {
                lines,
                bytes,
                cancelled,
                ..
            } => Some((*lines, *bytes, *cancelled)),
            _ => None,
        })
        .map_err(|err| format!("история {}: {err}", session.id))?;
    if lines != HISTORY_LINES || cancelled {
        return Err(format!(
            "история {}: записано {lines} строк из {HISTORY_LINES}",
            session.id
        ));
    }
    let last = flood_line(&session.fixture_nonce, HISTORY_LINES, LINE_WIDTH);
    let prefix = String::from_utf8_lossy(&last[..PREFIX.min(last.len())]).into_owned();
    let deadline = Instant::now() + FILL_WAIT;
    // Rust-хост снимок делает поток разбора, частый опрос снимком тормозил бы сам разбор: покой
    // ждётся по счётчикам состояния, снимок проверяет только итог.
    if let Ok(rust) = stand.rust() {
        loop {
            let status = rust
                .status(&session.id)
                .map_err(|err| format!("состояние {}: {err}", session.id))?;
            if status.read_bytes >= read_before + bytes && status.parsed_bytes == status.read_bytes
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "хост {} не разобрал историю за {} с: прочитано {}, разобрано {}",
                    session.id,
                    FILL_WAIT.as_secs(),
                    status.read_bytes,
                    status.parsed_bytes
                ));
            }
        }
    }
    loop {
        let snapshot = stand
            .main()
            .snapshot(&session.id)
            .map_err(|err| format!("снимок {}: {err}", session.id))?;
        if snapshot.text.contains(&prefix) {
            return Ok(prefix);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "хост {} не обработал историю за {} с",
                session.id,
                FILL_WAIT.as_secs()
            ));
        }
    }
}

/// Забирает из связи всё, что уже пришло: очередь пропущенных ожиданиями квитанций и новые.
fn drain(link: &mut Link) -> Result<Vec<Receipt>, String> {
    let mut drained = Vec::new();
    loop {
        match link.recv(Duration::ZERO) {
            Ok(receipt) => drained.push(receipt),
            Err(LinkError::Timeout) => return Ok(drained),
            Err(err) => return Err(format!("связь с фикстурой: {err}")),
        }
    }
}

fn footprint(pid: i32) -> Result<u64, String> {
    sys::phys_footprint(pid).ok_or_else(|| format!("память процесса {pid} не читается"))
}

fn ms(from_ns: u64, to_ns: u64) -> f64 {
    to_ns.saturating_sub(from_ns) as f64 / 1e6
}

// --- attach ---

#[derive(Clone, Debug, Serialize)]
struct AttachRecord {
    repeat: usize,
    kind: &'static str,
    /// От запуска attach-клиента до сообщения «лидер».
    leader_ms: Option<f64>,
    /// От запуска до ответа на барьер: восстановление дошло до приёмника.
    total_ms: Option<f64>,
    /// Байтов от клиента к приёмнику до барьера.
    restore_bytes: u64,
    /// Строк истории у приёмника после восстановления.
    history_rows: Option<usize>,
    /// Последняя строка истории видна у приёмника.
    restored: bool,
    client_footprint: Option<u64>,
    host_footprint: Option<u64>,
    failure: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct SessionMemory {
    repeat: usize,
    host: ProcessKey,
    empty: u64,
    history: u64,
    /// После отключения обоих клиентов: держит ли хост память восстановления.
    detached: u64,
}

fn attach(stand: &mut Stand, raw: &mut Raw, report: &mut Report) -> Result<(), String> {
    for repeat in 0..REPEATS {
        let id = stand.session(&format!("ma{repeat}"))?;
        let mut session = stand.ensure(&id, Mode::Flood)?;
        let host = session.record.host;
        let empty = footprint(host.pid)?;
        prime(stand, &mut session)?;
        let last = fill(stand, &mut session)?;
        let history = footprint(host.pid)?;
        for kind in ["cold", "warm"] {
            let record = attach_once(stand, &mut session, repeat, kind, &last)?;
            raw.attach.push(record);
            wait_clients(stand, &id, 0)?;
        }
        raw.sessions.push(SessionMemory {
            repeat,
            host,
            empty,
            history,
            detached: footprint(host.pid)?,
        });
        stand.discard(&id);
    }
    let pick = |kind: &str, value: &dyn Fn(&AttachRecord) -> Option<f64>| -> Vec<f64> {
        raw.attach
            .iter()
            .filter(|record| record.kind == kind && record.restored)
            .filter_map(value)
            .collect()
    };
    for kind in ["cold", "warm"] {
        report.metric(
            &format!("attach.{kind}.total_ms"),
            "ms",
            pick(kind, &|record| record.total_ms),
        );
        report.metric(
            &format!("attach.{kind}.leader_ms"),
            "ms",
            pick(kind, &|record| record.leader_ms),
        );
        report.metric(
            &format!("attach.{kind}.restore_kib"),
            "KiB",
            pick(kind, &|record| Some(record.restore_bytes as f64 / 1024.0)),
        );
        let unrestored: Vec<String> = raw
            .attach
            .iter()
            .filter(|record| record.kind == kind && !record.restored)
            .map(|record| {
                format!(
                    "повтор {}: {}",
                    record.repeat,
                    record
                        .failure
                        .as_deref()
                        .unwrap_or("последней строки истории у приёмника нет")
                )
            })
            .collect();
        if !unrestored.is_empty() {
            report.diagnostics.notes.push(format!(
                "attach {kind}: без восстановления, в замер не вошли — {}",
                unrestored.join("; ")
            ));
        }
    }
    let memory = |value: &dyn Fn(&SessionMemory) -> u64| -> Vec<f64> {
        raw.sessions
            .iter()
            .map(|session| mib(value(session)))
            .collect()
    };
    report.metric("memory.host.empty_mib", "MiB", memory(&|s| s.empty));
    report.metric("memory.host.history_mib", "MiB", memory(&|s| s.history));
    report.metric("memory.host.detached_mib", "MiB", memory(&|s| s.detached));
    let cold = |value: &dyn Fn(&AttachRecord) -> Option<u64>| -> Vec<f64> {
        raw.attach
            .iter()
            .filter(|record| record.kind == "cold" && record.restored)
            .filter_map(value)
            .map(mib)
            .collect()
    };
    report.metric(
        "memory.host.attached_mib",
        "MiB",
        cold(&|record| record.host_footprint),
    );
    report.metric(
        "memory.client_mib",
        "MiB",
        cold(&|record| record.client_footprint),
    );
    Ok(())
}

/// Один attach с отметками; отказ кандидата — запись с причиной, ошибка стенда — `Err`.
fn attach_once(
    stand: &Stand,
    session: &mut Session,
    repeat: usize,
    kind: &'static str,
    last: &str,
) -> Result<AttachRecord, String> {
    let mut record = AttachRecord {
        repeat,
        kind,
        leader_ms: None,
        total_ms: None,
        restore_bytes: 0,
        history_rows: None,
        restored: false,
        client_footprint: None,
        host_footprint: None,
        failure: None,
    };
    let label = if kind == "cold" { "c1" } else { "c2" };
    let spawned = sys::uptime_ns();
    let mut client = stand.client(&session.id, label)?;
    if let Err(err) = client.wait_leader() {
        record.failure = Some(err);
        client.hangup()?;
        return Ok(record);
    }
    record.leader_ms = Some(ms(spawned, sys::uptime_ns()));
    let restored_at = match barrier_at(session) {
        Ok(at) => at,
        Err(err) => {
            record.failure = Some(err);
            client.hangup()?;
            return Ok(record);
        }
    };
    record.total_ms = Some(ms(spawned, restored_at));
    record.restore_bytes = client.route()?.stats().read;
    let (visible, rows) = client.route()?.with(|rx| {
        let visible = rx
            .capture(Capture {
                history: 0,
                hidden: false,
            })
            .map(|state| state.text().iter().any(|row| row.starts_with(last)));
        (visible, rx.terminal().scrollback_rows().ok())
    });
    record.restored = visible.map_err(|err| format!("состояние приёмника {label}: {err}"))?;
    record.history_rows = rows;
    record.client_footprint = sys::phys_footprint(client.pid());
    record.host_footprint = sys::phys_footprint(session.record.host.pid);
    client.hangup()?;
    Ok(record)
}

// --- CPU ---

#[derive(Clone, Debug, Serialize)]
struct CpuSeries {
    load: &'static str,
    watches: Vec<Watch>,
    readings: Vec<Reading>,
    /// Квитанции хода потока `(t_ns, байты)`; у простоя нет.
    progress: Vec<(u64, u64)>,
}

fn reader(timebase: Timebase) -> impl Fn(&Watch) -> Option<measure::Sample> + Send + 'static {
    move |watch| measure::read_system(watch, timebase)
}

/// CPU в простое и под потоком. Итог — растёт ли счётчик пробуждений у процесса стенда: без этого
/// нулевые пробуждения кандидатов ничего не доказывают и не записываются.
fn cpu(
    stand: &mut Stand,
    timebase: Timebase,
    raw: &mut Raw,
    report: &mut Report,
) -> Result<bool, String> {
    let id = stand.session("mcpu")?;
    let mut session = stand.ensure(&id, Mode::Flood)?;
    prime(stand, &mut session)?;
    fill(stand, &mut session)?;
    let client = stand.client(&id, "c1")?;
    if let Err(err) = client.wait_leader().and_then(|_| barrier_at(&mut session)) {
        report.diagnostics.notes.push(format!(
            "CPU: клиент сессии с историей не дошёл до барьера, замеров CPU нет — {err}"
        ));
        drop(client);
        stand.discard(&id);
        return Ok(false);
    }
    let keys = [session.record.host, key_of(client.pid())?];
    let watches = vec![
        Watch::Process {
            part: Part::Host,
            label: "host".to_owned(),
            key: keys[0],
        },
        Watch::Process {
            part: Part::Client,
            label: "client".to_owned(),
            key: keys[1],
        },
        Watch::Threads {
            part: Part::Surface,
            name: SURFACE_THREAD.to_owned(),
        },
        Watch::Process {
            part: Part::Stand,
            label: "stand".to_owned(),
            key: key_of(std::process::id() as i32)?,
        },
    ];
    let control_index = watches.len() - 1;

    let idle = Series::start(watches.clone(), REPEATS + 1, WINDOW, reader(timebase))?.join()?;
    same_alive(&keys)?;
    raw.wakeup_control = measure::window_wakeups(&idle, &[control_index])?;
    let wakeups = raw.wakeup_control.iter().any(|value| *value > 0.0);
    if !wakeups {
        report.diagnostics.notes.push(
            "контроль: у процесса стенда счётчик пробуждений не растёт, пробуждения не \
             записываются"
                .to_owned(),
        );
    }
    cpu_metrics(report, "idle", &watches, &idle, wakeups)?;
    raw.cpu.push(CpuSeries {
        load: "idle",
        watches: watches.clone(),
        readings: idle,
        progress: Vec::new(),
    });

    let mark = session.link.log().len();
    session
        .link
        .request(
            Command::Flood {
                lines: FLOOD_LINES,
                width: LINE_WIDTH,
                interval_us: FLOOD_INTERVAL_US,
                report_every: FLOOD_REPORT_EVERY,
            },
            WAIT,
        )
        .map_err(|err| format!("поток: {err}"))?;
    session
        .link
        .wait_for(WAIT, |receipt| {
            matches!(receipt.event, Event::FloodProgress { .. }).then_some(())
        })
        .map_err(|err| format!("поток не пошёл: {err}"))?;
    let flood = Series::start(watches.clone(), REPEATS + 1, WINDOW, reader(timebase))?.join()?;
    // Квитанция конца потока приходит до подтверждения остановки: после него все квитанции хода
    // уже в журнале связи.
    session
        .link
        .request(Command::StopFlood, WAIT)
        .map_err(|err| format!("поток кончился раньше окон замера: {err}"))?;
    same_alive(&keys)?;
    barrier_at(&mut session)?;
    let progress: Vec<(u64, u64)> = session.link.log()[mark..]
        .iter()
        .filter_map(|receipt| match receipt.event {
            Event::FloodProgress { bytes, .. } | Event::FloodDone { bytes, .. } => {
                Some((receipt.t_ns, bytes))
            }
            _ => None,
        })
        .collect();
    cpu_metrics(report, "flood", &watches, &flood, wakeups)?;
    let written = measure::window_bytes(&flood, &progress)?;
    report.metric(
        "flood.rate_kib_s",
        "KiB/s",
        written
            .iter()
            .zip(flood.windows(2))
            .map(|(bytes, pair)| bytes / 1024.0 / (ms(pair[0].t_ns, pair[1].t_ns) / 1e3))
            .collect(),
    );
    for (index, watch) in watches.iter().enumerate() {
        report.metric(
            &format!("cpu.flood.{}_ms_per_mib", watch.part().name()),
            "ms/MiB",
            measure::window_cpu_per_mib(&flood, &[index], &written)?,
        );
    }
    raw.cpu.push(CpuSeries {
        load: "flood",
        watches,
        readings: flood,
        progress,
    });
    drop(client);
    stand.discard(&id);
    Ok(wakeups)
}

fn cpu_metrics(
    report: &mut Report,
    load: &str,
    watches: &[Watch],
    readings: &[Reading],
    wakeups: bool,
) -> Result<(), String> {
    for (index, watch) in watches.iter().enumerate() {
        let part = watch.part().name();
        report.metric(
            &format!("cpu.{load}.{part}_pct"),
            "%",
            measure::window_cpu(readings, &[index])?,
        );
        if wakeups && matches!(watch, Watch::Process { .. }) {
            report.metric(
                &format!("wakeups.{load}.{part}_per_s"),
                "1/s",
                measure::window_wakeups(readings, &[index])?,
            );
        }
    }
    Ok(())
}

// --- скрытые сессии и эхо ---

#[derive(Clone, Debug, Serialize)]
struct HiddenRecord {
    hidden: usize,
    hosts: Vec<ProcessKey>,
    footprints: Vec<u64>,
    idle: Vec<Reading>,
    echo: Vec<EchoRecord>,
    echo_failure: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct EchoRecord {
    repeat: usize,
    marker: u8,
    echo: Echo,
    legs: Option<EchoLegs>,
}

fn hidden(
    stand: &mut Stand,
    timebase: Timebase,
    wakeups: bool,
    raw: &mut Raw,
    report: &mut Report,
) -> Result<(), String> {
    let id = stand.session("mfg")?;
    let mut foreground = stand.ensure(&id, Mode::SlowReader)?;
    let client = stand.client(&id, "c1")?;
    client.wait_leader()?;
    barrier_at(&mut foreground)?;
    let mut hosts: Vec<ProcessKey> = Vec::new();
    for target in HIDDEN {
        while hosts.len() < target {
            let hidden_id = stand.session(&format!("mh{}", hosts.len()))?;
            let mut session = stand.ensure(&hidden_id, Mode::Flood)?;
            fill(stand, &mut session)?;
            hosts.push(session.record.host);
        }
        let footprints = hosts
            .iter()
            .map(|key| footprint(key.pid))
            .collect::<Result<Vec<_>, _>>()?;
        let watches: Vec<Watch> = hosts
            .iter()
            .enumerate()
            .map(|(index, key)| Watch::Process {
                part: Part::Host,
                label: format!("hidden-{index}"),
                key: *key,
            })
            .collect();
        let idle = Series::start(watches, REPEATS + 1, HIDDEN_WINDOW, reader(timebase))?.join()?;
        same_alive(&hosts)?;
        let all: Vec<usize> = (0..hosts.len()).collect();
        let per = |values: Vec<f64>| -> Vec<f64> {
            values
                .into_iter()
                .map(|value| value / target as f64)
                .collect()
        };
        report.metric(
            &format!("hidden-{target}.host_mib"),
            "MiB",
            footprints.iter().copied().map(mib).collect(),
        );
        report.metric(
            &format!("hidden-{target}.idle_cpu_pct_per_session"),
            "%",
            per(measure::window_cpu(&idle, &all)?),
        );
        if wakeups {
            report.metric(
                &format!("hidden-{target}.wakeups_per_s_per_session"),
                "1/s",
                per(measure::window_wakeups(&idle, &all)?),
            );
        }
        let (echo, echo_failure) = match echo_series(&mut foreground, &client) {
            Ok(echo) => (echo, None),
            Err(EchoError::Candidate(reason)) => (Vec::new(), Some(reason)),
            Err(EchoError::Stand(reason)) => return Err(reason),
        };
        echo_metrics(report, target, &echo, echo_failure.as_deref());
        raw.hidden.push(HiddenRecord {
            hidden: target,
            hosts: hosts.clone(),
            footprints,
            idle,
            echo,
            echo_failure,
        });
    }
    let totals: Vec<(usize, u64)> = raw
        .hidden
        .iter()
        .map(|record| (record.hidden, record.footprints.iter().sum()))
        .collect();
    for pair in totals.windows(2) {
        if let Some(marginal) = measure::marginal(pair[0], pair[1]) {
            report.diagnostics.notes.push(format!(
                "память скрытой сессии от {} к {}: {:.2} МиБ на сессию",
                pair[0].0,
                pair[1].0,
                marginal / measure::MIB
            ));
        }
    }
    drop(client);
    Ok(())
}

enum EchoError {
    /// Кандидат не дал доказуемого эха: запись без значений.
    Candidate(String),
    Stand(String),
}

/// [`REPEATS`] повторов по [`KEYSTROKES`] нажатий в окно клиента-лидера. Нагрузка отражает
/// прочитанное; приход эха — рост байтов от клиента к приёмнику. В конце ряда байтов от клиента
/// ровно по одному на нажатие: иначе первый пришедший байт мог быть не эхом.
fn echo_series(session: &mut Session, client: &Client) -> Result<Vec<EchoRecord>, EchoError> {
    let stand_err = |err: String| EchoError::Stand(err);
    session
        .link
        .request(Command::Echo { on: true }, WAIT)
        .map_err(|err| stand_err(format!("эхо фикстуры: {err}")))?;
    // Квитанции чтения ответов на прежние барьеры остаются в очереди связи: ряд начинается с пустой.
    drain(&mut session.link).map_err(stand_err)?;
    let route = client.route().map_err(stand_err)?;
    let start = route.stats().read;
    let mut records = Vec::with_capacity(REPEATS * KEYSTROKES);
    let mut failure = None;
    'series: for repeat in 0..REPEATS {
        for stroke in 0..KEYSTROKES {
            let marker = b'a' + ((repeat * KEYSTROKES + stroke) % 26) as u8;
            let before = route.stats().read;
            let sent_ns = sys::uptime_ns();
            route.input(&[marker]);
            if let Err(err) = route.wait_until(WAIT, |_, stats| stats.read > before) {
                failure = Some(format!("эхо «{}» не пришло: {err:?}", marker as char));
                break 'series;
            }
            let echoed_ns = sys::uptime_ns();
            let expected = hex(&[marker]);
            let read_ns = match session.link.wait_for(WAIT, |receipt| match &receipt.event {
                Event::Read { hex, .. } if *hex == expected => Some(receipt.t_ns),
                _ => None,
            }) {
                Ok(read_ns) => read_ns,
                Err(err) => {
                    failure = Some(format!("нагрузка не прочитала «{}»: {err}", marker as char));
                    break 'series;
                }
            };
            let echo = Echo {
                sent_ns,
                read_ns,
                echoed_ns,
            };
            records.push(EchoRecord {
                repeat,
                marker,
                echo,
                legs: echo.legs(),
            });
        }
    }
    // Чтения нагрузки, кроме нажатий: посторонний ввод во время ряда.
    let foreign: Vec<String> = drain(&mut session.link)
        .map_err(stand_err)?
        .into_iter()
        .filter_map(|receipt| match receipt.event {
            Event::Read { hex, .. } => Some(hex),
            _ => None,
        })
        .collect();
    session
        .link
        .request(Command::Echo { on: false }, WAIT)
        .map_err(|err| stand_err(format!("эхо фикстуры: {err}")))?;
    barrier_at(session).map_err(stand_err)?;
    if let Some(failure) = failure {
        return Err(EchoError::Candidate(failure));
    }
    if !foreign.is_empty() {
        return Err(EchoError::Candidate(format!(
            "нагрузка прочитала посторонний ввод во время ряда: {}",
            foreign.join(", ")
        )));
    }
    let got = route.stats().read.saturating_sub(start);
    let expected = records.len() as u64 + BARRIER.len() as u64;
    if got != expected {
        return Err(EchoError::Candidate(format!(
            "от клиента пришло {got} байт, а эха и барьера — {expected}: приход эха не доказан"
        )));
    }
    if let Some(record) = records.iter().find(|record| record.legs.is_none()) {
        return Err(EchoError::Candidate(format!(
            "отметки нажатия «{}» не по порядку: {:?}",
            record.marker as char, record.echo
        )));
    }
    Ok(records)
}

fn echo_metrics(report: &mut Report, hidden: usize, echo: &[EchoRecord], failure: Option<&str>) {
    if let Some(failure) = failure {
        report.diagnostics.notes.push(format!(
            "эхо при {hidden} скрытых: без значений — {failure}"
        ));
    }
    let legs: Vec<EchoLegs> = echo.iter().filter_map(|record| record.legs).collect();
    let name = |what: &str| format!("echo.hidden-{hidden}.{what}");
    report.metric(
        &name("total_ms"),
        "ms",
        legs.iter().map(|legs| legs.total_ms).collect(),
    );
    report.metric(
        &name("input_ms"),
        "ms",
        legs.iter().map(|legs| legs.input_ms).collect(),
    );
    report.metric(
        &name("output_ms"),
        "ms",
        legs.iter().map(|legs| legs.output_ms).collect(),
    );
    let medians: Vec<f64> = (0..REPEATS)
        .filter_map(|repeat| {
            let totals: Vec<f64> = echo
                .iter()
                .filter(|record| record.repeat == repeat)
                .filter_map(|record| record.legs.map(|legs| legs.total_ms))
                .collect();
            measure::median(&totals)
        })
        .collect();
    report.metric(&name("repeat_median_ms"), "ms", medians);
}
