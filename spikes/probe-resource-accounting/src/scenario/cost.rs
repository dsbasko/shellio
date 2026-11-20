//! Сценарий `cost`: стоимость сэмплера и учёта участков по точкам раздела «Стоимость» плана.
//! Ожидания, протокол и статистика вердикта — отчёт, раздел «Ожидания», `cost`.
//!
//! Процесс на точку. Фикстуры строят множество PID точки: на панель — хост, программа (лидер
//! своей сессии и группы) и её дети, запись владения хоста с сессией. Нагрузка — 200 операций в
//! секунду на `tokio` с двумя рабочими потоками, у операции 4 `poll` по 0,125 мс CPU потока.
//! Окно 30 с, блок — четыре окна по одному на состояние, порядок — сдвиг по слоту. CPU стенда на
//! границах окон читает наблюдатель отдельным процессом, он же снимает границы проверки фона.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::stand::{Stand, json};
use crate::attrib::{Claims, Owner, Panel};
use crate::background::{self, uptime_ns};
use crate::fixture::receipts::Mode;
use crate::fixture::{Program, Spec};
use crate::observer::{self, Mark, Reading, Reply};
use crate::proc::System;
use crate::results::{BackgroundRecord, Cell, Metric, NotRunCause, Report};
use crate::sampler::{self, Config, Digest, Sampler, SamplerLog, Visibility};
use crate::spans::clock::{Chosen, Clock};
use crate::spans::{Ledger, OwnerCharge};
use crate::stats::{
    self, BlockValues, CostRow, Effect, EffectRow, EffectVerdict, Next, Progress, Rules, State,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Point {
    pub name: &'static str,
    pub pids: usize,
    pub panels: usize,
    pub visibility: Visibility,
    /// Точка простоя работы не делает: мерится только эффект сэмплера.
    pub work: bool,
}

impl Point {
    /// Детей программы на панель: всего на панель хост, программа и дети.
    pub fn children(&self) -> Result<u32, String> {
        let per_panel = self.pids / self.panels.max(1);
        if self.panels == 0 || per_panel * self.panels != self.pids || per_panel < 2 {
            return Err(format!(
                "точка {}: {} PID на {} панелей не делится на хост и программу",
                self.name, self.pids, self.panels
            ));
        }
        u32::try_from(per_panel - 2).map_err(|err| format!("точка {}: {err}", self.name))
    }
}

pub const POINTS: [Point; 6] = [
    point("pids-8", 8, 4, Visibility::Visible, true),
    point("pids-32", 32, 4, Visibility::Visible, true),
    point("pids-128", 128, 4, Visibility::Visible, true),
    point("panels-16", 32, 16, Visibility::Visible, true),
    point("background", 32, 4, Visibility::Background, true),
    point("idle", 32, 4, Visibility::Visible, false),
];

pub const POINT_NAMES: &[&str] = &[
    "pids-8",
    "pids-32",
    "pids-128",
    "panels-16",
    "background",
    "idle",
];

const fn point(
    name: &'static str,
    pids: usize,
    panels: usize,
    visibility: Visibility,
    work: bool,
) -> Point {
    Point {
        name,
        pids,
        panels,
        visibility,
        work,
    }
}

pub fn find_point(name: &str) -> Result<Point, String> {
    POINTS
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("неизвестная точка «{name}»"))
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Protocol {
    pub warmup_ns: u64,
    pub window_ns: u64,
    pub rules: Rules,
    /// Общий срок попытки от отметки `run.sh` перед захватом блокировки.
    pub attempt_ns: u64,
    pub ops_per_s: u64,
    pub op_cpu_ns: u64,
    pub polls: u32,
    /// Окно с отклонением числа операций больше этой доли недействительно.
    pub ops_tolerance: f64,
    pub workers: usize,
    pub sampler_budget_pp: f64,
    pub spans_budget_pp: f64,
}

const SECOND: u64 = 1_000_000_000;

pub const PROTOCOL: Protocol = Protocol {
    warmup_ns: 10 * SECOND,
    window_ns: 30 * SECOND,
    rules: Rules {
        blocks: 5,
        max_repeats: 2,
        min_clean: 3,
        guard_ns: 180 * SECOND,
    },
    attempt_ns: 25 * 60 * SECOND,
    ops_per_s: 200,
    op_cpu_ns: 500_000,
    polls: 4,
    ops_tolerance: 0.01,
    workers: 2,
    sampler_budget_pp: stats::SAMPLER_BUDGET_PP,
    spans_budget_pp: stats::SPANS_BUDGET_PP,
};

/// Отметка `run.sh` перед захватом замерной блокировки, секунды эпохи.
pub const START_ENV: &str = "SHELLIO_PROBE_UNIT_START";

/// Срок попытки на монотонных часах: от отметки `run.sh`, а без неё — от текущего момента.
pub fn attempt_deadline(
    attempt_ns: u64,
    start_epoch_s: Option<u64>,
    now_epoch_s: u64,
    now_uptime_ns: u64,
) -> u64 {
    let elapsed_ns = start_epoch_s.map_or(0, |start| now_epoch_s.saturating_sub(start) * SECOND);
    now_uptime_ns + attempt_ns.saturating_sub(elapsed_ns)
}

/// Проходов сэмплера в окне: сэмплер запускается через полшага после начала окна.
pub fn passes_expected(window_ns: u64, step_ns: u64) -> u64 {
    window_ns / step_ns
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WindowRecord {
    pub run: usize,
    pub slot: usize,
    pub position: usize,
    pub state: State,
    pub start_ns: u64,
    pub end_ns: u64,
    pub stand_cpu_ns: f64,
    pub cpu_pct: f64,
    pub ops: u64,
    pub ops_expected: Option<f64>,
    pub cpu_per_op_us: Option<f64>,
    pub passes_expected: Option<u64>,
    pub sampler: Option<Digest>,
    pub footprint: u64,
    pub quiet: bool,
    /// Причины недействительного окна: операции, проходы, множество.
    pub problems: Vec<String>,
}

/// Причины недействительного окна.
pub fn window_problems(
    point: &Point,
    protocol: &Protocol,
    wall_ns: u64,
    ops: u64,
    log: Option<&SamplerLog>,
    step_ns: u64,
) -> Vec<String> {
    let mut problems = Vec::new();
    if point.work {
        let expected = protocol.ops_per_s as f64 * wall_ns as f64 / SECOND as f64;
        let deviation = (ops as f64 - expected).abs() / expected;
        if !deviation.is_finite() || deviation > protocol.ops_tolerance {
            problems.push(format!(
                "операции: {ops} против {expected:.0}, отклонение {:.2}%",
                100.0 * deviation
            ));
        }
    }
    if let Some(log) = log {
        let expected = passes_expected(protocol.window_ns, step_ns);
        if log.passes.len() as u64 != expected {
            problems.push(format!("проходы: {} против {expected}", log.passes.len()));
        }
        for pass in &log.passes {
            match &pass.outcome {
                Err(reason) => problems.push(format!("проход {}: {reason}", pass.index)),
                Ok(counts) if counts.set_size != point.pids => problems.push(format!(
                    "проход {}: множество {} против {}",
                    pass.index, counts.set_size, point.pids
                )),
                Ok(_) => {}
            }
        }
    }
    problems
}

/// Значения чистых блоков по состояниям.
pub fn block_values(
    blocks: &[BlockRecord],
    value: impl Fn(&WindowRecord) -> Option<f64>,
) -> Vec<BlockValues> {
    blocks
        .iter()
        .filter(|b| b.clean)
        .map(|b| {
            let mut values = [f64::NAN; 4];
            for window in &b.windows {
                values[window.state.index()] = value(window).unwrap_or(f64::NAN);
            }
            values
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BlockRecord {
    pub run: usize,
    pub slot: usize,
    pub repeat: bool,
    pub order: [State; 4],
    pub clean: bool,
    pub windows: Vec<WindowRecord>,
}

/// Наблюдатель — ребёнок стенда с каналами stdin и stdout.
struct ObserverProc {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl ObserverProc {
    fn spawn(out: &Path) -> Result<ObserverProc, String> {
        let exe = std::env::current_exe().map_err(|err| format!("current_exe: {err}"))?;
        let mut child = Command::new(exe)
            .arg("--observer")
            .arg("--stand")
            .arg(std::process::id().to_string())
            .arg("--out")
            .arg(out)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| format!("запуск наблюдателя: {err}"))?;
        let stdin = child.stdin.take().ok_or("наблюдатель без stdin")?;
        let stdout = child.stdout.take().ok_or("наблюдатель без stdout")?;
        Ok(ObserverProc {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            next: 0,
        })
    }

    fn mark(&mut self, ops: u64) -> Result<Reading, String> {
        let mark = Mark {
            index: self.next,
            ops,
        };
        self.next += 1;
        let mut line = serde_json::to_vec(&mark).map_err(|err| format!("отметка: {err}"))?;
        line.push(b'\n');
        let stdin = self.stdin.as_mut().ok_or("канал наблюдателя закрыт")?;
        stdin
            .write_all(&line)
            .and_then(|()| stdin.flush())
            .map_err(|err| format!("отметка наблюдателю: {err}"))?;
        let mut reply = String::new();
        let read = self
            .stdout
            .read_line(&mut reply)
            .map_err(|err| format!("ответ наблюдателя: {err}"))?;
        if read == 0 {
            return Err("наблюдатель закрыл канал".to_owned());
        }
        match serde_json::from_str::<Reply>(&reply)
            .map_err(|err| format!("ответ наблюдателя «{}»: {err}", reply.trim()))?
        {
            Reply::Reading(reading) if reading.index == mark.index => Ok(*reading),
            Reply::Reading(reading) => Err(format!(
                "наблюдатель ответил на отметку {} вместо {}",
                reading.index, mark.index
            )),
            Reply::Error { reason, .. } => Err(format!("наблюдатель: {reason}")),
        }
    }

    fn finish(mut self) -> Result<(), String> {
        drop(self.stdin.take());
        let status = self
            .child
            .wait()
            .map_err(|err| format!("ожидание наблюдателя: {err}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("наблюдатель завершился: {status}"))
        }
    }
}

impl Drop for ObserverProc {
    fn drop(&mut self) {
        if self.stdin.take().is_some() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Работа до `part_ns` CPU потока по выбранным часам; `false` — часы не прочитаны.
fn spin(part_ns: u64) -> bool {
    let clock = Chosen::default();
    let Some(start) = clock.now_ns() else {
        return false;
    };
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    loop {
        for _ in 0..256 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
        }
        std::hint::black_box(x);
        match clock.now_ns() {
            Some(now) if now.saturating_sub(start) >= part_ns => return true,
            Some(_) => {}
            None => return false,
        }
    }
}

async fn operation(part_ns: u64, polls: u32, done: Arc<AtomicU64>, failures: Arc<AtomicU64>) {
    for poll in 0..polls {
        if poll > 0 {
            tokio::task::yield_now().await;
        }
        if !spin(part_ns) {
            failures.fetch_add(1, Ordering::Relaxed);
        }
    }
    done.fetch_add(1, Ordering::Relaxed);
}

/// Нагрузка: генератор операций с фиксированным темпом, пропущенные такты не догоняются.
struct Work {
    runtime: tokio::runtime::Runtime,
    done: Arc<AtomicU64>,
    failures: Arc<AtomicU64>,
    spans_on: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    ledger: Arc<Ledger<Chosen>>,
}

#[derive(Clone, Debug, Serialize)]
struct WorkTotals {
    ops: u64,
    clock_failures: u64,
    charges: Vec<OwnerCharge>,
}

impl Work {
    fn start(protocol: &Protocol, owners: Vec<Owner>) -> Result<Work, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(protocol.workers)
            .enable_time()
            .build()
            .map_err(|err| format!("рантайм tokio: {err}"))?;
        let work = Work {
            runtime,
            done: Arc::new(AtomicU64::new(0)),
            failures: Arc::new(AtomicU64::new(0)),
            spans_on: Arc::new(AtomicBool::new(true)),
            stop: Arc::new(AtomicBool::new(false)),
            ledger: Arc::new(Ledger::new(Chosen::default())),
        };
        let period = Duration::from_nanos(SECOND / protocol.ops_per_s.max(1));
        let part_ns = protocol.op_cpu_ns / u64::from(protocol.polls.max(1));
        let polls = protocol.polls;
        let (done, failures, spans_on, stop, ledger) = (
            Arc::clone(&work.done),
            Arc::clone(&work.failures),
            Arc::clone(&work.spans_on),
            Arc::clone(&work.stop),
            Arc::clone(&work.ledger),
        );
        if owners.is_empty() {
            return Err("нагрузке нужен хотя бы один владелец".to_owned());
        }
        work.runtime.spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut k = 0usize;
            loop {
                interval.tick().await;
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let op = operation(part_ns, polls, Arc::clone(&done), Arc::clone(&failures));
                if spans_on.load(Ordering::Relaxed) {
                    tokio::spawn(ledger.wrap(&owners[k % owners.len()], op));
                } else {
                    tokio::spawn(op);
                }
                k += 1;
            }
        });
        Ok(work)
    }

    fn set_spans(&self, on: bool) {
        self.spans_on.store(on, Ordering::Relaxed);
    }

    fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    fn stop(self) -> WorkTotals {
        self.stop.store(true, Ordering::Relaxed);
        self.runtime.shutdown_timeout(Duration::from_secs(5));
        WorkTotals {
            ops: self.done.load(Ordering::Relaxed),
            clock_failures: self.failures.load(Ordering::Relaxed),
            charges: self.ledger.charges(),
        }
    }
}

const ORACLE_SAMPLER: &str = "наблюдатель: CPU стенда по окнам 30 с; оценки 10−00 и 11−01 в каждом чистом блоке ≤ 0,5 п. п. одного ядра — pass, все выше — fail, иначе не подтверждено; чистых блоков не меньше 3";
const ORACLE_SPANS: &str = "наблюдатель: CPU стенда по окнам 30 с; оценки 01−00 и 11−10 в каждом чистом блоке ≤ 1 п. п. одного ядра — pass, все выше — fail, иначе не подтверждено; чистых блоков не меньше 3";

fn effect_cell(id: &str, oracle: &str, verdict: &EffectVerdict, tail: &str) -> Cell {
    Cell::new(
        id,
        verdict.verdict.status(),
        oracle,
        format!("{}; {tail}", verdict.reason),
    )
}

fn now_epoch_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Тело `cost` после тихого окна фона.
pub fn run(point_name: &str, before: Vec<BackgroundRecord>, out: &Path) -> Result<Report, String> {
    let point = find_point(point_name)?;
    let start_epoch_s = std::env::var(START_ENV)
        .ok()
        .and_then(|value| value.parse().ok());
    let deadline_ns = attempt_deadline(
        PROTOCOL.attempt_ns,
        start_epoch_s,
        now_epoch_s(),
        uptime_ns(),
    );
    let mut report = run_point(&point, &PROTOCOL, deadline_ns, before, out)?;
    report
        .raw
        .insert("attempt_start_epoch_s".to_owned(), json(&start_epoch_s));
    Ok(report)
}

fn host_spec(name: &str, children: u32) -> Spec {
    let mut spec = Spec::new(Mode::Host, name);
    spec.program = Program::Tree;
    spec.children = children;
    spec
}

pub fn run_point(
    point: &Point,
    protocol: &Protocol,
    deadline_ns: u64,
    before: Vec<BackgroundRecord>,
    out: &Path,
) -> Result<Report, String> {
    let units = background::current_units();
    let children = point.children()?;
    let mut stand = Stand::start_with(out, Duration::from_nanos(protocol.attempt_ns))?;
    let hosts: Vec<String> = (0..point.panels).map(|i| format!("p{i}")).collect();
    let specs: Vec<Spec> = hosts.iter().map(|h| host_spec(h, children)).collect();
    let mut extra = Vec::new();
    for host in &hosts {
        extra.push(format!("{host}/p"));
        for c in 1..=children {
            extra.push(format!("{host}/p/c{c}"));
        }
    }
    let ready = stand.spawn_ready(&specs, &extra)?;
    let mut panels = Vec::new();
    for host in &hosts {
        panels.push(Panel {
            id: host.clone(),
            surface: Vec::new(),
            record: Some(stand.owner_record(host)?),
        });
    }
    let claims = Claims {
        panels,
        git: Vec::new(),
    };
    let (_, attribution, _) = sampler::attributed_snapshot(&System, uptime_ns, &claims)?;
    let found = attribution.all().len();
    if found != point.pids {
        return Err(format!(
            "предусловие: атрибуция точки {} нашла {found} PID вместо {}",
            point.name, point.pids
        ));
    }
    let owners: Vec<Owner> = hosts.iter().map(|h| Owner::Panel(h.clone())).collect();
    let config = Config {
        timebase: stand.timebase,
        steps: sampler::STEPS,
    };
    let step_ns = sampler::STEPS.of(point.visibility);

    let mut observer = ObserverProc::spawn(out)?;
    let work = if point.work {
        Some(Work::start(protocol, owners)?)
    } else {
        None
    };
    let ops = |work: &Option<Work>| work.as_ref().map_or(0, Work::done);

    let warmup = Sampler::spawn(System, config, claims.clone(), point.visibility)?;
    background::hold_until(uptime_ns() + protocol.warmup_ns);
    let warmup_log = warmup.stop()?;

    let mut readings = vec![observer.mark(ops(&work))?];
    let mut progress = Progress::default();
    let mut blocks: Vec<BlockRecord> = Vec::new();
    let mut during: Vec<BackgroundRecord> = Vec::new();
    let stop = loop {
        let (slot, order, repeat) = match progress.next(&protocol.rules, uptime_ns(), deadline_ns) {
            Next::Done(stop) => break stop,
            Next::Block {
                slot,
                order,
                repeat,
            } => (slot, order, repeat),
        };
        let mut windows = Vec::new();
        for (position, state) in order.into_iter().enumerate() {
            if let Some(work) = &work {
                work.set_spans(state.spans());
            }
            let last = readings.last().ok_or("нет прошлой отметки")?.clone();
            let sampler = if state.sampler() {
                background::hold_until(last.uptime_ns + step_ns / 2);
                Some(Sampler::spawn(
                    System,
                    config,
                    claims.clone(),
                    point.visibility,
                )?)
            } else {
                None
            };
            background::hold_until(last.uptime_ns + protocol.window_ns);
            let log = sampler.map(Sampler::stop).transpose()?;
            let reading = observer.mark(ops(&work))?;
            let wall_ns = reading.uptime_ns.saturating_sub(last.uptime_ns);
            let cpu_ns = reading.stand_cpu_ns - last.stand_cpu_ns;
            let done = reading.ops.saturating_sub(last.ops);
            let check = reading
                .background
                .clone()
                .ok_or("наблюдатель не дал проверку фона окна")?;
            let quiet = check.quiet;
            during.push(BackgroundRecord {
                label: format!("during:b{}-{}", progress.run, state.name()),
                check,
            });
            windows.push(WindowRecord {
                run: progress.run,
                slot,
                position,
                state,
                start_ns: last.uptime_ns,
                end_ns: reading.uptime_ns,
                stand_cpu_ns: cpu_ns,
                cpu_pct: 100.0 * cpu_ns / wall_ns.max(1) as f64,
                ops: done,
                ops_expected: point
                    .work
                    .then(|| protocol.ops_per_s as f64 * wall_ns as f64 / SECOND as f64),
                cpu_per_op_us: (point.work && done > 0).then(|| cpu_ns / done as f64 / 1e3),
                passes_expected: state
                    .sampler()
                    .then(|| passes_expected(protocol.window_ns, step_ns)),
                sampler: log.as_ref().map(SamplerLog::digest),
                footprint: reading.stand_footprint,
                quiet,
                problems: window_problems(point, protocol, wall_ns, done, log.as_ref(), step_ns),
            });
            readings.push(reading);
        }
        let clean = windows.iter().all(|w| w.quiet && w.problems.is_empty());
        blocks.push(BlockRecord {
            run: progress.run,
            slot,
            repeat,
            order,
            clean,
            windows,
        });
        progress.record(&protocol.rules, clean);
    };

    let totals = work.map(Work::stop);
    observer.finish()?;
    let faults = stand.finish()?;
    if !faults.is_empty() {
        return Err(format!("канал фикстур: {}", faults.join("; ")));
    }
    if let Some(totals) = &totals
        && totals.clock_failures > 0
    {
        return Err(format!(
            "часы потока не прочитаны в {} частях операций",
            totals.clock_failures
        ));
    }
    let after = background::measure(background::WINDOW, &[])?;

    let mut report = Report::default();
    let pct = block_values(&blocks, |w| Some(w.cpu_pct));
    let sampler_verdict = stats::effect_verdict(Effect::Sampler, &pct, protocol.rules.min_clean);
    let spans_verdict = point
        .work
        .then(|| stats::effect_verdict(Effect::Spans, &pct, protocol.rules.min_clean));
    let contaminated: Vec<String> = blocks
        .iter()
        .filter(|b| !b.clean)
        .map(|b| {
            let noisy = b.windows.iter().filter(|w| !w.quiet).count();
            let problems: Vec<&str> = b
                .windows
                .iter()
                .flat_map(|w| w.problems.iter().map(String::as_str))
                .collect();
            format!(
                "блок {} (слот {}): шумных окон {noisy}{}",
                b.run,
                b.slot,
                if problems.is_empty() {
                    String::new()
                } else {
                    format!(", недействительно: {}", problems.join(", "))
                }
            )
        })
        .collect();
    let tail = format!(
        "точка {}: {} PID, {} панелей, шаг {} с; блоков {}, остановка {:?}{}",
        point.name,
        point.pids,
        point.panels,
        step_ns / SECOND,
        blocks.len(),
        stop,
        if contaminated.is_empty() {
            String::new()
        } else {
            format!("; загрязнены: {}", contaminated.join("; "))
        }
    );
    report.cells.push(effect_cell(
        "cost-sampler",
        ORACLE_SAMPLER,
        &sampler_verdict,
        &tail,
    ));
    match &spans_verdict {
        Some(verdict) => report
            .cells
            .push(effect_cell("cost-spans", ORACLE_SPANS, verdict, &tail)),
        None => report.cells.push(Cell::not_run(
            "cost-spans",
            NotRunCause::NotObserved,
            ORACLE_SPANS,
            format!(
                "точка простоя: операций нет, CPU на операцию не определён, мерится только эффект сэмплера; {tail}"
            ),
        )),
    }

    for state in State::ALL {
        report.metrics.push(Metric::new(
            &format!("cpu_pct.{}", state.name()),
            "% одного ядра",
            pct.iter().map(|b| b[state.index()]).collect(),
        ));
    }
    for verdict in std::iter::once(&sampler_verdict).chain(spans_verdict.as_ref()) {
        let prefix = match verdict.effect {
            Effect::Sampler => "sampler",
            Effect::Spans => "spans",
        };
        for estimate in &verdict.estimates {
            report.metrics.push(Metric::new(
                &format!("{prefix}.{}", estimate.id),
                "п. п. одного ядра",
                estimate.samples.clone(),
            ));
        }
    }
    let per_op = block_values(&blocks, |w| w.cpu_per_op_us);
    if point.work {
        for state in State::ALL {
            report.metrics.push(Metric::new(
                &format!("cpu_per_op_us.{}", state.name()),
                "мкс CPU процесса",
                per_op.iter().map(|b| b[state.index()]).collect(),
            ));
        }
    }
    let clean_digests: Vec<&Digest> = blocks
        .iter()
        .filter(|b| b.clean)
        .flat_map(|b| b.windows.iter().filter_map(|w| w.sampler.as_ref()))
        .collect();
    let all_digests: Vec<&Digest> = blocks
        .iter()
        .flat_map(|b| b.windows.iter().filter_map(|w| w.sampler.as_ref()))
        .collect();
    report.metrics.push(Metric::new(
        "sampler.pass_duration_median_us",
        "мкс",
        clean_digests
            .iter()
            .filter_map(|d| d.duration_median_ns.map(|ns| ns as f64 / 1e3))
            .collect(),
    ));
    report.metrics.push(Metric::new(
        "stand.footprint_mib",
        "МиБ",
        readings
            .iter()
            .map(|r| r.stand_footprint as f64 / (1u64 << 20) as f64)
            .collect(),
    ));
    report.metrics.push(Metric::new(
        "background_pct.during",
        "% одного ядра",
        readings
            .iter()
            .filter_map(|r| r.background.as_ref().map(|b| b.background_pct))
            .collect(),
    ));

    let row = CostRow {
        point: point.name.to_owned(),
        pids: point.pids,
        panels: point.panels,
        step_s: step_ns as f64 / SECOND as f64,
        work: point.work,
        blocks_run: blocks.len(),
        clean_blocks: pct.len(),
        stop,
        sampler: EffectRow::of(&sampler_verdict),
        spans: spans_verdict.as_ref().map(EffectRow::of),
        cpu_per_op_us: point.work.then(|| {
            State::ALL
                .map(|s| stats::median(&per_op.iter().map(|b| b[s.index()]).collect::<Vec<_>>()))
        }),
        pass_duration_median_us: stats::median(
            &clean_digests
                .iter()
                .filter_map(|d| d.duration_median_ns.map(|ns| ns as f64 / 1e3))
                .collect::<Vec<_>>(),
        ),
        wakeups_per_minute: stats::median(
            &clean_digests
                .iter()
                .filter_map(|d| d.wakeups_per_minute)
                .collect::<Vec<_>>(),
        ),
        set_size: all_digests.iter().filter_map(|d| d.set_size_max).max(),
        partial_reads: all_digests.iter().map(|d| d.partial_reads_total).sum(),
        footprint_max_mib: readings
            .iter()
            .map(|r| r.stand_footprint as f64 / (1u64 << 20) as f64)
            .reduce(f64::max),
    };

    report.background = before;
    report.background.extend(during);
    report.background.push(BackgroundRecord {
        label: "after".to_owned(),
        check: after,
    });
    report.raw.insert("point".to_owned(), json(point));
    report.raw.insert("protocol".to_owned(), json(protocol));
    report.raw.insert("units".to_owned(), json(&units));
    report.raw.insert("cost_row".to_owned(), json(&row));
    report.raw.insert("blocks".to_owned(), json(&blocks));
    report.raw.insert("readings".to_owned(), json(&readings));
    report.raw.insert("progress".to_owned(), json(&progress));
    report.raw.insert("stop".to_owned(), json(&stop));
    report
        .raw
        .insert("sampler_verdict".to_owned(), json(&sampler_verdict));
    report
        .raw
        .insert("spans_verdict".to_owned(), json(&spans_verdict));
    report.raw.insert("work".to_owned(), json(&totals));
    report
        .raw
        .insert("warmup".to_owned(), json(&warmup_log.digest()));
    report.raw.insert("claims".to_owned(), json(&claims));
    report.raw.insert("ready".to_owned(), json(&ready));
    report
        .raw
        .insert("observer_log".to_owned(), json(&observer::LOG_FILE));
    report.raw.insert(
        "deadline".to_owned(),
        serde_json::json!({ "deadline_ns": deadline_ns, "finished_ns": uptime_ns() }),
    );
    Ok(report)
}

#[cfg(test)]
mod tests;
