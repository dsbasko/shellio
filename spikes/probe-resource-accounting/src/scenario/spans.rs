//! Сценарии учёта участков и арен (задача 9): `span-busy`, `span-wait`, `span-migrate`,
//! `span-nested`, `span-parallel`, `span-blocking` и `arena`. Ожидания — отчёт, раздел
//! «Ожидания», «Сценарии учёта участков и арен (задача 9)».

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use serde::Serialize;

use super::clock::{thread_count, tolerance_ns};
use super::formula::noisy;
use super::stand::json;
use crate::arena::{Arena, Arenas, Usage};
use crate::attrib::Owner;
use crate::background::{self, THRESHOLDS};
use crate::proc::units::{self, Load};
use crate::results::{BackgroundRecord, Cell, Metric, NotRunCause, Report, Status};
use crate::spans::clock::Chosen;
use crate::spans::{Charge, Ledger, thread_index};

pub const BUSY_SPANS: [Duration; 3] = [
    Duration::from_micros(500),
    Duration::from_millis(5),
    Duration::from_millis(50),
];
pub const BUSY_REPEATS: usize = 3;
pub const WAIT_WORK: Duration = Duration::from_millis(5);
pub const WAIT_B_POLLS: usize = 10;
pub const WAIT_OUTSIDE: Duration = Duration::from_millis(20);
pub const NESTED_POLLS: usize = 5;
pub const NESTED_OUTER: Duration = Duration::from_millis(5);
pub const NESTED_GIT: Duration = Duration::from_millis(10);
pub const NESTED_SAME: Duration = Duration::from_millis(3);
pub const MIGRATE_POLLS: usize = 8;
pub const MIGRATE_WORK: Duration = Duration::from_millis(5);
pub const TOKIO_TASKS: usize = 1000;
pub const TOKIO_POLLS: usize = 4;
pub const TOKIO_WORK: Duration = Duration::from_micros(500);
pub const PARALLEL_TASKS: usize = 8;
pub const PARALLEL_POLLS: usize = 10;
pub const PARALLEL_WORK: Duration = Duration::from_millis(2);
pub const BLOCKING_RUNS: usize = 10;
pub const BLOCKING_WORK: Duration = Duration::from_millis(10);
/// Рабочие потоки `tokio` по правилу «Асинхронный рантайм».
pub const WORKERS: usize = 2;
const HOP_TIMEOUT: Duration = Duration::from_secs(30);

const ORACLE_SPAN: &str = "getrusage(RUSAGE_SELF) в скобке вокруг каждого участка однопоточного процесса; |начислено − оракул| ≤ 0,2 мс + 2% оракула на каждом участке и в сумме владельца, неизмеренных участков нет";
const ORACLE_WAIT: &str = "участки A в допуске от своих скобок getrusage, CPU между её poll больше допуска и ей не начислен";
const ORACLE_NESTED: &str = "на каждом poll: «Git» — от внутренней скобки, outer — от внешней минус «Git», сумма владельцев — от внешней скобки; 0,2 мс + 2% оракула";
const ORACLE_THREADS: &str = "части по потокам — от сумм скобок getrusage своих poll (другие потоки ждут в канале), смен потока на одну меньше числа poll, части ровно на двух потоках";
const ORACLE_SERIES: &str = "сумма начисленного владельцу — от скобки getrusage(RUSAGE_SELF) процесса вокруг серии, допуск 0,2 мс + 2% один раз от оракула серии";
const ORACLE_BLOCKING: &str = "каждый запуск spawn_blocking — от скобки getrusage вокруг block_on запуска; 0,2 мс + 2% оракула";
const ORACLE_ARENA: &str =
    "известные размеры арен: занятые байты, ёмкость, число долей и запросившие совпадают точно";

/// Итог серии до оценки фона.
#[derive(Default)]
pub struct Series {
    pub cells: Vec<Cell>,
    pub metrics: Vec<Metric>,
    pub raw: BTreeMap<String, serde_json::Value>,
    pub notes: Vec<String>,
}

impl Series {
    fn extend(&mut self, other: Series) {
        self.cells.extend(other.cells);
        self.metrics.extend(other.metrics);
        self.raw.extend(other.raw);
        self.notes.extend(other.notes);
    }
}

/// Сравнение начисленного с оракулом.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Sample {
    pub label: String,
    pub charged_ns: u64,
    pub oracle_ns: u64,
}

impl Sample {
    pub fn new(label: impl Into<String>, charged_ns: u64, oracle_ns: u64) -> Sample {
        Sample {
            label: label.into(),
            charged_ns,
            oracle_ns,
        }
    }

    /// Доля допуска; больше 1 — вне допуска.
    pub fn share(&self) -> f64 {
        (self.charged_ns as f64 - self.oracle_ns as f64).abs() / tolerance_ns(self.oracle_ns)
    }
}

/// Клетка по сравнениям: все в допуске и нарушений нет — `pass`, иначе `fail`.
pub fn samples_cell(id: &str, oracle: &str, samples: &[Sample], problems: &[String]) -> Cell {
    let outside = samples.iter().filter(|s| s.share() > 1.0).count();
    let worst = samples
        .iter()
        .max_by(|a, b| a.share().total_cmp(&b.share()))
        .map_or("нет".to_owned(), |s| {
            format!(
                "{}: начислено {:.1} мкс, оракул {:.1} мкс, доля допуска {:.3}",
                s.label,
                s.charged_ns as f64 / 1e3,
                s.oracle_ns as f64 / 1e3,
                s.share()
            )
        });
    let mut reason = format!(
        "сравнений {}, вне допуска {outside}, худшее — {worst}",
        samples.len()
    );
    if !problems.is_empty() {
        reason.push_str(&format!("; нарушения: {}", problems.join("; ")));
    }
    let status = if !samples.is_empty() && outside == 0 && problems.is_empty() {
        Status::Pass
    } else {
        Status::Fail
    };
    Cell::new(id, status, oracle, reason)
}

/// Участков ровно `spans`, неизмеренных нет, сумма частей по потокам равна начисленному.
pub fn charge_problems(name: &str, charge: &Charge, spans: u64) -> Vec<String> {
    let mut problems = Vec::new();
    if charge.spans != spans {
        problems.push(format!(
            "{name}: участков {}, ожидалось {spans}",
            charge.spans
        ));
    }
    if charge.unmeasured_spans != 0 {
        problems.push(format!(
            "{name}: неизмеренных участков {}",
            charge.unmeasured_spans
        ));
    }
    let parts: u64 = charge.per_thread_ns.values().sum();
    if parts != charge.cpu_ns {
        problems.push(format!(
            "{name}: сумма частей по потокам {parts} нс, начислено {} нс",
            charge.cpu_ns
        ));
    }
    problems
}

/// `span-wait-idle`: CPU между `poll` задачи не больше допуска — ожидания не было.
pub fn wait_idle_cell(samples: &[Sample], problems: &[String], wait_oracle_ns: u64) -> Cell {
    let total = samples.last().map_or(0, |s| s.oracle_ns);
    let mut cell = samples_cell("span-wait-idle", ORACLE_WAIT, samples, problems);
    cell.reason.push_str(&format!(
        "; CPU между poll A {:.1} мкс",
        wait_oracle_ns as f64 / 1e3
    ));
    if wait_oracle_ns as f64 <= tolerance_ns(total) {
        return Cell::not_run(
            "span-wait-idle",
            NotRunCause::NotObserved,
            ORACLE_WAIT,
            format!("CPU между poll не больше допуска: {}", cell.reason),
        );
    }
    cell
}

/// Клетка серии на `tokio`: `not_run`, если условие наблюдения не наступило.
pub fn series_cell(
    id: &str,
    sample: &Sample,
    problems: &[String],
    observed: Result<(), String>,
) -> Cell {
    let cell = samples_cell(id, ORACLE_SERIES, std::slice::from_ref(sample), problems);
    match observed {
        Ok(()) => cell,
        Err(what) => Cell::not_run(
            id,
            NotRunCause::NotObserved,
            ORACLE_SERIES,
            format!("{what}: {}", cell.reason),
        ),
    }
}

/// Расхождение учтённого с ожидаемым.
pub fn usage_problems(
    name: &str,
    usage: &Usage,
    used: u64,
    capacity: u64,
    leases: u64,
    requested_by: &[&str],
) -> Vec<String> {
    let requested: BTreeSet<String> = requested_by.iter().map(|p| (*p).to_owned()).collect();
    let mut problems = Vec::new();
    if usage.used_bytes != used {
        problems.push(format!(
            "{name}: занято {}, ожидалось {used}",
            usage.used_bytes
        ));
    }
    if usage.capacity_bytes != capacity {
        problems.push(format!(
            "{name}: ёмкость {}, ожидалось {capacity}",
            usage.capacity_bytes
        ));
    }
    if usage.leases != leases {
        problems.push(format!(
            "{name}: долей {}, ожидалось {leases}",
            usage.leases
        ));
    }
    if usage.requested_by != requested {
        problems.push(format!(
            "{name}: запросившие {:?}, ожидалось {requested:?}",
            usage.requested_by
        ));
    }
    problems
}

fn arena_cell(id: &str, problems: Vec<String>, observed: String) -> Cell {
    if problems.is_empty() {
        Cell::new(id, Status::Pass, ORACLE_ARENA, observed)
    } else {
        Cell::new(
            id,
            Status::Fail,
            ORACLE_ARENA,
            format!("{observed}; нарушения: {}", problems.join("; ")),
        )
    }
}

fn panel(id: &str) -> Owner {
    Owner::Panel(id.to_owned())
}

fn work(load: Load, duration: Duration) -> Result<(), String> {
    let deadline = background::uptime_ns() + duration.as_nanos() as u64;
    match load {
        Load::User => {
            units::busy_user(deadline);
            Ok(())
        }
        Load::System => units::busy_system(deadline),
    }
}

fn cpu_ns() -> Result<u64, String> {
    let (user, system) = units::self_getrusage()?;
    Ok(user + system)
}

/// Скобка `getrusage(RUSAGE_SELF)` вокруг `f`: значение и CPU процесса, нс.
fn bracket<T>(f: impl FnOnce() -> T) -> Result<(T, u64), String> {
    let start = cpu_ns()?;
    let value = f();
    let end = cpu_ns()?;
    Ok((value, end.saturating_sub(start)))
}

type Work = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// На каждом `poll` — очередной шаг работы, `Ready` после последнего.
fn steps(plan: Vec<(Load, Duration)>) -> Work {
    let mut index = 0;
    Box::pin(poll_fn(move |_| {
        let Some(&(load, duration)) = plan.get(index) else {
            return Poll::Ready(Ok(()));
        };
        if let Err(err) = work(load, duration) {
            return Poll::Ready(Err(err));
        }
        index += 1;
        if index == plan.len() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }))
}

/// `Pending` один раз, затем `Ready`: точка переключения при ручном опросе.
fn yield_once() -> impl Future<Output = ()> + Send {
    let mut yielded = false;
    poll_fn(move |_| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            Poll::Pending
        }
    })
}

fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    let mut cx = Context::from_waker(Waker::noop());
    Pin::new(future).poll(&mut cx)
}

fn ready(poll: Poll<Result<(), String>>) -> Result<bool, String> {
    match poll {
        Poll::Ready(result) => result.map(|()| true),
        Poll::Pending => Ok(false),
    }
}

fn ledger() -> Ledger<Chosen> {
    Ledger::new(Chosen::default())
}

/// Предусловие однопоточных сценариев: скобка `getrusage` — CPU единственного потока.
fn single_threaded(body: impl FnOnce() -> Result<Series, String>) -> Result<Series, String> {
    let before = thread_count()?;
    let series = body()?;
    let after = thread_count()?;
    if before != 1 || after != 1 {
        return Err(format!(
            "предусловие: процесс однопоточный — потоков {before} до серии и {after} после"
        ));
    }
    Ok(series)
}

/// Замерный сценарий: окно фона «во время» на всю серию и окно «после».
fn measured_series(
    before: Vec<BackgroundRecord>,
    series: impl FnOnce() -> Result<Series, String>,
) -> Result<Report, String> {
    let units = background::current_units();
    let during_start = background::sample_bound()?;
    let series = series()?;
    let during_end = background::sample_bound()?;
    let during = background::evaluate(
        &during_start,
        &during_end,
        std::process::id() as i32,
        &[],
        units,
        THRESHOLDS,
    );
    let after = background::measure(background::WINDOW, &[])?;
    let mut report = Report {
        cells: series
            .cells
            .into_iter()
            .map(|cell| noisy(cell, during.quiet))
            .collect(),
        metrics: series.metrics,
        raw: series.raw,
        notes: series.notes,
        background: before,
    };
    report.background.push(BackgroundRecord {
        label: "during:series".to_owned(),
        check: during,
    });
    report.background.push(BackgroundRecord {
        label: "after".to_owned(),
        check: after,
    });
    Ok(report)
}

fn share_metric(name: &str, samples: &[Sample]) -> Metric {
    Metric::new(
        &format!("{name}.share_of_tolerance"),
        "доля допуска",
        samples.iter().map(Sample::share).collect(),
    )
}

pub fn run_busy(before: Vec<BackgroundRecord>) -> Result<Report, String> {
    measured_series(before, || single_threaded(busy))
}

pub fn run_wait(before: Vec<BackgroundRecord>) -> Result<Report, String> {
    measured_series(before, || single_threaded(wait))
}

pub fn run_nested(before: Vec<BackgroundRecord>) -> Result<Report, String> {
    measured_series(before, || single_threaded(nested))
}

pub fn run_migrate(before: Vec<BackgroundRecord>) -> Result<Report, String> {
    measured_series(before, || {
        let mut series = migrate_threads()?;
        series.extend(migrate_tokio()?);
        Ok(series)
    })
}

pub fn run_parallel(before: Vec<BackgroundRecord>) -> Result<Report, String> {
    measured_series(before, parallel)
}

pub fn run_blocking(before: Vec<BackgroundRecord>) -> Result<Report, String> {
    measured_series(before, blocking)
}

/// Тело `span-busy` без предусловия однопоточности.
pub fn busy() -> Result<Series, String> {
    let ledger = ledger();
    let owner = panel("busy");
    let mut plan = Vec::new();
    for _ in 0..BUSY_REPEATS {
        for duration in BUSY_SPANS {
            for load in [Load::User, Load::System] {
                plan.push((load, duration));
            }
        }
    }
    let mut future = ledger.wrap(&owner, steps(plan.clone()));
    let mut samples = Vec::new();
    let mut problems = Vec::new();
    for (index, (load, duration)) in plan.iter().enumerate() {
        let charged_before = ledger.charge(&owner).cpu_ns;
        let (poll, oracle_ns) = bracket(|| poll_once(&mut future))?;
        let charged_ns = ledger.charge(&owner).cpu_ns.saturating_sub(charged_before);
        samples.push(Sample::new(
            format!("poll {} {load:?} {} мкс", index + 1, duration.as_micros()),
            charged_ns,
            oracle_ns,
        ));
        if ready(poll)? != (index + 1 == plan.len()) {
            problems.push(format!("poll {}: готовность не по плану", index + 1));
        }
    }
    drop(future);
    let charge = ledger.charge(&owner);
    let total: u64 = samples.iter().map(|s| s.oracle_ns).sum();
    problems.extend(charge_problems("busy", &charge, plan.len() as u64));
    let mut all = samples.clone();
    all.push(Sample::new("сумма busy", charge.cpu_ns, total));
    let mut series = Series::default();
    series
        .cells
        .push(samples_cell("span-busy", ORACLE_SPAN, &all, &problems));
    series.metrics.push(share_metric("span-busy", &samples));
    series.raw.insert("samples".to_owned(), json(&all));
    series
        .raw
        .insert("charges".to_owned(), json(&ledger.charges()));
    Ok(series)
}

/// Тело `span-wait` без предусловия однопоточности.
pub fn wait() -> Result<Series, String> {
    let ledger = ledger();
    let (a_owner, b_owner) = (panel("wait-a"), panel("wait-b"));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let a_barrier = Arc::clone(&barrier);
    let a_future: Work = Box::pin(async move {
        work(Load::User, WAIT_WORK)?;
        a_barrier.wait().await;
        work(Load::User, WAIT_WORK)
    });
    let b_future: Work = Box::pin(async move {
        for k in 0..WAIT_B_POLLS {
            work(Load::User, WAIT_WORK)?;
            if k + 1 < WAIT_B_POLLS {
                yield_once().await;
            }
        }
        barrier.wait().await;
        Ok(())
    });
    let mut a = ledger.wrap(&a_owner, a_future);
    let mut b = ledger.wrap(&b_owner, b_future);
    let mut a_problems = Vec::new();
    let mut b_problems = Vec::new();
    let mut poll_a = |label: &str, expect_ready: bool| -> Result<Sample, String> {
        let before = ledger.charge(&a_owner).cpu_ns;
        let (poll, oracle_ns) = bracket(|| poll_once(&mut a))?;
        if ready(poll)? != expect_ready {
            a_problems.push(format!("{label}: готовность не по плану"));
        }
        let charged = ledger.charge(&a_owner).cpu_ns.saturating_sub(before);
        Ok(Sample::new(label, charged, oracle_ns))
    };
    let a1 = poll_a("A poll 1", false)?;
    let wait_start = cpu_ns()?;
    let mut b_samples = Vec::new();
    for k in 0..WAIT_B_POLLS {
        let before = ledger.charge(&b_owner).cpu_ns;
        let (poll, oracle_ns) = bracket(|| poll_once(&mut b))?;
        if ready(poll)? != (k + 1 == WAIT_B_POLLS) {
            b_problems.push(format!("B poll {}: готовность не по плану", k + 1));
        }
        let charged = ledger.charge(&b_owner).cpu_ns.saturating_sub(before);
        b_samples.push(Sample::new(format!("B poll {}", k + 1), charged, oracle_ns));
    }
    work(Load::User, WAIT_OUTSIDE)?;
    let wait_oracle_ns = cpu_ns()?.saturating_sub(wait_start);
    let a2 = poll_a("A poll 2", true)?;
    drop(poll_a);
    drop((a, b));
    let a_charge = ledger.charge(&a_owner);
    let b_charge = ledger.charge(&b_owner);
    a_problems.extend(charge_problems("wait-a", &a_charge, 2));
    b_problems.extend(charge_problems("wait-b", &b_charge, WAIT_B_POLLS as u64));
    let a_total = a1.oracle_ns + a2.oracle_ns;
    let a_samples = vec![
        a1,
        a2,
        Sample::new("сумма wait-a", a_charge.cpu_ns, a_total),
    ];
    let b_total: u64 = b_samples.iter().map(|s| s.oracle_ns).sum();
    b_samples.push(Sample::new("сумма wait-b", b_charge.cpu_ns, b_total));
    let mut series = Series::default();
    series
        .cells
        .push(wait_idle_cell(&a_samples, &a_problems, wait_oracle_ns));
    series.cells.push(samples_cell(
        "span-wait-busy",
        ORACLE_SPAN,
        &b_samples,
        &b_problems,
    ));
    series.metrics.push(share_metric("span-wait-a", &a_samples));
    series.metrics.push(share_metric("span-wait-b", &b_samples));
    series.raw.insert("a".to_owned(), json(&a_samples));
    series.raw.insert("b".to_owned(), json(&b_samples));
    series
        .raw
        .insert("wait_oracle_ns".to_owned(), json(&wait_oracle_ns));
    series
        .raw
        .insert("charges".to_owned(), json(&ledger.charges()));
    Ok(series)
}

/// Скобки одного внешнего `poll` `span-nested`: «Git» и вложенный участок того же владельца.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Inner {
    pub git_ns: u64,
    pub same_ns: u64,
}

/// Тело `span-nested` без предусловия однопоточности.
pub fn nested() -> Result<Series, String> {
    let ledger = Arc::new(ledger());
    let outer = panel("outer");
    let inner_ledger = Arc::clone(&ledger);
    let inner_owner = outer.clone();
    let inner: Rc<RefCell<Vec<Inner>>> = Rc::default();
    let recorded = Rc::clone(&inner);
    let mut count = 0usize;
    let mut step = move || -> Result<bool, String> {
        work(Load::User, NESTED_OUTER)?;
        let mut git =
            inner_ledger.wrap_shared_git(&["outer"], steps(vec![(Load::User, NESTED_GIT)]));
        let (git_poll, git_ns) = bracket(|| poll_once(&mut git))?;
        drop(git);
        let mut same = inner_ledger.wrap(&inner_owner, steps(vec![(Load::User, NESTED_SAME)]));
        let (same_poll, same_ns) = bracket(|| poll_once(&mut same))?;
        drop(same);
        if !ready(git_poll)? || !ready(same_poll)? {
            return Err("вложенный участок не завершился за один poll".to_owned());
        }
        work(Load::User, NESTED_OUTER)?;
        recorded.borrow_mut().push(Inner { git_ns, same_ns });
        count += 1;
        Ok(count == NESTED_POLLS)
    };
    let mut future = ledger.wrap(
        &outer,
        poll_fn(move |_| match step() {
            Ok(true) => Poll::Ready(Ok(())),
            Ok(false) => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }),
    );
    let mut samples = Vec::new();
    let mut problems = Vec::new();
    let mut outer_total = 0u64;
    for k in 0..NESTED_POLLS {
        let outer_before = ledger.charge(&outer).cpu_ns;
        let git_before = ledger.charge(&Owner::Git).cpu_ns;
        let (poll, outer_ns) = bracket(|| poll_once(&mut future))?;
        if ready(poll)? != (k + 1 == NESTED_POLLS) {
            problems.push(format!("poll {}: готовность не по плану", k + 1));
        }
        let d_outer = ledger.charge(&outer).cpu_ns.saturating_sub(outer_before);
        let d_git = ledger.charge(&Owner::Git).cpu_ns.saturating_sub(git_before);
        let Some(&Inner { git_ns, .. }) = inner.borrow().get(k) else {
            return Err(format!("poll {}: скобки вложенных участков нет", k + 1));
        };
        outer_total += outer_ns;
        samples.push(Sample::new(format!("poll {} Git", k + 1), d_git, git_ns));
        samples.push(Sample::new(
            format!("poll {} outer", k + 1),
            d_outer,
            outer_ns.saturating_sub(git_ns),
        ));
        samples.push(Sample::new(
            format!("poll {} сумма", k + 1),
            d_outer + d_git,
            outer_ns,
        ));
    }
    drop(future);
    let outer_charge = ledger.charge(&outer);
    let git_charge = ledger.charge(&Owner::Git);
    problems.extend(charge_problems(
        "outer",
        &outer_charge,
        2 * NESTED_POLLS as u64,
    ));
    problems.extend(charge_problems("Git", &git_charge, NESTED_POLLS as u64));
    if git_charge.requested_by != ["outer"] {
        problems.push(format!(
            "Git: запросившие {:?}, ожидалось [outer]",
            git_charge.requested_by
        ));
    }
    let git_total: u64 = inner.borrow().iter().map(|i| i.git_ns).sum();
    samples.push(Sample::new("сумма Git", git_charge.cpu_ns, git_total));
    samples.push(Sample::new(
        "сумма outer",
        outer_charge.cpu_ns,
        outer_total.saturating_sub(git_total),
    ));
    samples.push(Sample::new(
        "сумма владельцев",
        outer_charge.cpu_ns + git_charge.cpu_ns,
        outer_total,
    ));
    let mut series = Series::default();
    series.cells.push(samples_cell(
        "span-nested",
        ORACLE_NESTED,
        &samples,
        &problems,
    ));
    series.metrics.push(share_metric("span-nested", &samples));
    series.raw.insert("samples".to_owned(), json(&samples));
    series
        .raw
        .insert("inner".to_owned(), json(&*inner.borrow()));
    series
        .raw
        .insert("charges".to_owned(), json(&ledger.charges()));
    Ok(series)
}

/// Квитанция управляемого потока о `poll`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Hop {
    pub thread: u64,
    pub thread_id: String,
    pub oracle_ns: u64,
    pub ready: bool,
}

type Job = crate::spans::Accounted<Work, Chosen>;

/// Управляемая часть `span-migrate`: future по очереди опрашивают два потока.
pub fn migrate_threads() -> Result<Series, String> {
    let ledger = ledger();
    let owner = panel("migrate");
    let mut job: Option<Job> = Some(ledger.wrap(
        &owner,
        steps(vec![(Load::User, MIGRATE_WORK); MIGRATE_POLLS]),
    ));
    let (back_tx, back_rx) = mpsc::channel::<(Job, Result<Hop, String>)>();
    let mut senders = Vec::new();
    let mut handles = Vec::new();
    for index in 0..2 {
        let (tx, rx) = mpsc::channel::<Job>();
        let back = back_tx.clone();
        let handle = std::thread::Builder::new()
            .name(format!("span-migrate-{index}"))
            .spawn(move || {
                for mut job in rx {
                    let hop = bracket(|| poll_once(&mut job)).and_then(|(poll, oracle_ns)| {
                        Ok(Hop {
                            thread: thread_index(),
                            thread_id: format!("{:?}", std::thread::current().id()),
                            oracle_ns,
                            ready: ready(poll)?,
                        })
                    });
                    if back.send((job, hop)).is_err() {
                        return;
                    }
                }
            })
            .map_err(|err| format!("поток опроса: {err}"))?;
        senders.push(tx);
        handles.push(handle);
    }
    drop(back_tx);
    let mut hops = Vec::new();
    let mut problems = Vec::new();
    for k in 0..MIGRATE_POLLS {
        let current = job.take().ok_or("future потеряна между потоками")?;
        senders[k % 2]
            .send(current)
            .map_err(|_| "поток опроса вышел до квитанции".to_owned())?;
        let (returned, hop) = back_rx
            .recv_timeout(HOP_TIMEOUT)
            .map_err(|err| format!("квитанция потока опроса: {err}"))?;
        job = Some(returned);
        let hop = hop?;
        if hop.ready != (k + 1 == MIGRATE_POLLS) {
            problems.push(format!("poll {}: готовность не по плану", k + 1));
        }
        hops.push(hop);
    }
    drop(senders);
    for handle in handles {
        handle
            .join()
            .map_err(|_| "поток опроса завершился паникой".to_owned())?;
    }
    drop(job);
    let charge = ledger.charge(&owner);
    problems.extend(charge_problems("migrate", &charge, MIGRATE_POLLS as u64));
    if charge.migrations != MIGRATE_POLLS as u64 - 1 {
        problems.push(format!(
            "смен потока {}, ожидалось {}",
            charge.migrations,
            MIGRATE_POLLS - 1
        ));
    }
    let ids: BTreeSet<&str> = hops.iter().map(|h| h.thread_id.as_str()).collect();
    if ids.len() != 2 || charge.per_thread_ns.len() != 2 {
        problems.push(format!(
            "ThreadId {}, частей по потокам {}, ожидалось по 2",
            ids.len(),
            charge.per_thread_ns.len()
        ));
    }
    let mut per_thread: BTreeMap<u64, u64> = BTreeMap::new();
    for hop in &hops {
        *per_thread.entry(hop.thread).or_default() += hop.oracle_ns;
    }
    let mut samples: Vec<Sample> = per_thread
        .iter()
        .map(|(thread, oracle)| {
            Sample::new(
                format!("часть потока {thread}"),
                charge.per_thread_ns.get(thread).copied().unwrap_or(0),
                *oracle,
            )
        })
        .collect();
    samples.push(Sample::new(
        "сумма migrate",
        charge.cpu_ns,
        hops.iter().map(|h| h.oracle_ns).sum(),
    ));
    let mut series = Series::default();
    series.cells.push(samples_cell(
        "span-migrate-threads",
        ORACLE_THREADS,
        &samples,
        &problems,
    ));
    series.raw.insert("hops".to_owned(), json(&hops));
    series
        .raw
        .insert("threads_samples".to_owned(), json(&samples));
    series
        .raw
        .insert("threads_charge".to_owned(), json(&charge));
    Ok(series)
}

/// Рантайм модели ADR-0020 после прогрева рабочих потоков и пула `spawn_blocking`.
fn runtime() -> Result<tokio::runtime::Runtime, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .build()
        .map_err(|err| format!("рантайм tokio: {err}"))?;
    let handles: Vec<_> = (0..WORKERS * 4)
        .map(|_| runtime.spawn(tokio::task::yield_now()))
        .collect();
    runtime.block_on(async {
        for handle in handles {
            handle.await.map_err(|err| format!("прогрев: {err}"))?;
        }
        Ok::<(), String>(())
    })?;
    runtime
        .block_on(runtime.spawn_blocking(|| ()))
        .map_err(|err| format!("прогрев spawn_blocking: {err}"))?;
    Ok(runtime)
}

/// Серия задач владельца на `tokio`: начисленное и скобка процесса вокруг серии.
fn tokio_series(
    runtime: &tokio::runtime::Runtime,
    ledger: &Ledger<Chosen>,
    owner: &Owner,
    tasks: usize,
    polls: usize,
    each: Duration,
) -> Result<(Charge, u64, u64), String> {
    let wall_start = background::uptime_ns();
    let start = cpu_ns()?;
    let handles: Vec<_> = (0..tasks)
        .map(|_| {
            runtime.spawn(ledger.wrap(owner, async move {
                for k in 0..polls {
                    work(Load::User, each)?;
                    if k + 1 < polls {
                        tokio::task::yield_now().await;
                    }
                }
                Ok::<(), String>(())
            }))
        })
        .collect();
    runtime.block_on(async {
        for handle in handles {
            handle
                .await
                .map_err(|err| format!("задача tokio: {err}"))??;
        }
        Ok::<(), String>(())
    })?;
    let oracle = cpu_ns()?.saturating_sub(start);
    let wall = background::uptime_ns().saturating_sub(wall_start);
    Ok((ledger.charge(owner), oracle, wall))
}

/// Часть `span-migrate` на `tokio`.
pub fn migrate_tokio() -> Result<Series, String> {
    let runtime = runtime()?;
    let ledger = ledger();
    let owner = panel("migrate-tokio");
    let (charge, oracle, wall) = tokio_series(
        &runtime,
        &ledger,
        &owner,
        TOKIO_TASKS,
        TOKIO_POLLS,
        TOKIO_WORK,
    )?;
    drop(runtime);
    let problems = charge_problems("migrate-tokio", &charge, (TOKIO_TASKS * TOKIO_POLLS) as u64);
    let sample = Sample::new("серия migrate-tokio", charge.cpu_ns, oracle);
    let observed = if charge.migrations > 0 {
        Ok(())
    } else {
        Err(format!(
            "смена потока на tokio не наблюдалась за {TOKIO_TASKS} задач"
        ))
    };
    let mut series = Series::default();
    series.cells.push(series_cell(
        "span-migrate-tokio",
        &sample,
        &problems,
        observed,
    ));
    series.raw.insert("tokio_charge".to_owned(), json(&charge));
    series.raw.insert("tokio_sample".to_owned(), json(&sample));
    series.raw.insert("tokio_wall_ns".to_owned(), json(&wall));
    Ok(series)
}

/// Тело `span-parallel`.
pub fn parallel() -> Result<Series, String> {
    let runtime = runtime()?;
    let ledger = ledger();
    let owner = panel("parallel");
    let (charge, oracle, wall) = tokio_series(
        &runtime,
        &ledger,
        &owner,
        PARALLEL_TASKS,
        PARALLEL_POLLS,
        PARALLEL_WORK,
    )?;
    drop(runtime);
    let problems = charge_problems(
        "parallel",
        &charge,
        (PARALLEL_TASKS * PARALLEL_POLLS) as u64,
    );
    let sample = Sample::new("серия parallel", charge.cpu_ns, oracle);
    let threads = charge.per_thread_ns.len();
    let observed = if threads >= 2 {
        Ok(())
    } else {
        Err(format!("части на {threads} потоках: параллели нет"))
    };
    let mut series = Series::default();
    series
        .cells
        .push(series_cell("span-parallel", &sample, &problems, observed));
    series.notes.push(format!(
        "span-parallel: CPU серии {:.1} мс за {:.1} мс стенного времени, частей по потокам {threads}",
        oracle as f64 / 1e6,
        wall as f64 / 1e6
    ));
    series.raw.insert("charge".to_owned(), json(&charge));
    series.raw.insert("sample".to_owned(), json(&sample));
    series.raw.insert("wall_ns".to_owned(), json(&wall));
    Ok(series)
}

/// Тело `span-blocking`.
pub fn blocking() -> Result<Series, String> {
    let runtime = runtime()?;
    let handle = runtime.handle().clone();
    let ledger = ledger();
    let owner = panel("blocking");
    let mut samples = Vec::new();
    for k in 0..BLOCKING_RUNS {
        let load = if k % 2 == 0 { Load::User } else { Load::System };
        let before = ledger.charge(&owner).cpu_ns;
        let (result, oracle_ns) = bracket(|| {
            runtime
                .block_on(ledger.spawn_blocking(&handle, &owner, move || work(load, BLOCKING_WORK)))
        })?;
        result.map_err(|err| format!("spawn_blocking: {err}"))??;
        let charged = ledger.charge(&owner).cpu_ns.saturating_sub(before);
        samples.push(Sample::new(
            format!("запуск {} {load:?}", k + 1),
            charged,
            oracle_ns,
        ));
    }
    drop(runtime);
    let charge = ledger.charge(&owner);
    let problems = charge_problems("blocking", &charge, BLOCKING_RUNS as u64);
    let mut series = Series::default();
    series.cells.push(samples_cell(
        "span-blocking",
        ORACLE_BLOCKING,
        &samples,
        &problems,
    ));
    series.metrics.push(share_metric("span-blocking", &samples));
    series.raw.insert("samples".to_owned(), json(&samples));
    series.raw.insert("charge".to_owned(), json(&charge));
    Ok(series)
}

fn alloc(arena: &mut Arena, bytes: &[u8]) -> Result<crate::arena::Slot, String> {
    arena.alloc(bytes).map_err(|err| err.to_string())
}

/// Сценарий `arena`: точные учтённые байты на известных размерах.
pub fn run_arena() -> Result<Report, String> {
    let arenas = Arenas::new();
    let (a, b) = (panel("A"), panel("B"));
    let mut report = Report::default();
    let mut steps = serde_json::Map::new();

    let mut arena_a = Arena::new(arenas.lease(&a), 65_536);
    let mut first = Vec::new();
    for i in 0..100u8 {
        first.push(alloc(&mut arena_a, &[i; 1000])?);
    }
    let mut arena_b = Arena::new(arenas.lease(&b), 4096);
    for _ in 0..2 {
        alloc(&mut arena_b, &[7; 3000])?;
    }
    let mut problems = usage_problems("A", &arenas.usage_of(&a), 100_000, 131_072, 1, &[]);
    problems.extend(usage_problems(
        "B",
        &arenas.usage_of(&b),
        6000,
        8192,
        1,
        &[],
    ));
    steps.insert("grow".to_owned(), json(&arenas.usage()));
    report.cells.push(arena_cell(
        "arena-grow",
        problems,
        format!("A {:?}, B {:?}", arenas.usage_of(&a), arenas.usage_of(&b)),
    ));

    arena_a.reset();
    let mut problems = usage_problems("A после сброса", &arenas.usage_of(&a), 0, 131_072, 1, &[]);
    if arena_a.get(first[0]).is_some() {
        problems.push("место прошлого поколения читается после сброса".to_owned());
    }
    let mut second = Vec::new();
    for _ in 0..50 {
        second.push(alloc(&mut arena_a, &[42; 1000])?);
    }
    problems.extend(usage_problems(
        "A после переиспользования",
        &arenas.usage_of(&a),
        50_000,
        131_072,
        1,
        &[],
    ));
    if arena_a.get(second[49]) != Some(&[42u8; 1000][..]) {
        problems.push("новое место не читается".to_owned());
    }
    steps.insert("reuse".to_owned(), json(&arenas.usage()));
    report.cells.push(arena_cell(
        "arena-reuse",
        problems,
        format!("A {:?}", arenas.usage_of(&a)),
    ));

    let freed = arena_a.trim().map_err(|err| err.to_string())?;
    let mut problems = Vec::new();
    if freed != 65_536 {
        problems.push(format!("освобождено {freed}, ожидалось 65536"));
    }
    problems.extend(usage_problems(
        "A после освобождения",
        &arenas.usage_of(&a),
        50_000,
        65_536,
        1,
        &[],
    ));
    alloc(&mut arena_a, &vec![1; 200_000])?;
    problems.extend(usage_problems(
        "A после большого выделения",
        &arenas.usage_of(&a),
        250_000,
        265_536,
        1,
        &[],
    ));
    let after_large = arenas.usage_of(&a);
    drop(arena_a);
    problems.extend(usage_problems(
        "A после уничтожения",
        &arenas.usage_of(&a),
        0,
        0,
        0,
        &[],
    ));
    problems.extend(usage_problems(
        "B",
        &arenas.usage_of(&b),
        6000,
        8192,
        1,
        &[],
    ));
    steps.insert("release".to_owned(), json(&arenas.usage()));
    report.cells.push(arena_cell(
        "arena-release",
        problems,
        format!("освобождено {freed}, A после большого выделения {after_large:?}"),
    ));

    let mut git = Arena::new(arenas.shared_git_lease(&["A", "B"]), 32_768);
    alloc(&mut git, &[3; 10_000])?;
    let mut problems = usage_problems(
        "Git",
        &arenas.usage_of(&Owner::Git),
        10_000,
        32_768,
        1,
        &["A", "B"],
    );
    problems.extend(usage_problems("A", &arenas.usage_of(&a), 0, 0, 0, &[]));
    problems.extend(usage_problems(
        "B",
        &arenas.usage_of(&b),
        6000,
        8192,
        1,
        &[],
    ));
    steps.insert("git".to_owned(), json(&arenas.usage()));
    report.cells.push(arena_cell(
        "arena-git",
        problems,
        format!("Git {:?}", arenas.usage_of(&Owner::Git)),
    ));
    drop((git, arena_b));
    report
        .raw
        .insert("steps".to_owned(), serde_json::Value::Object(steps));
    report.raw.insert("final".to_owned(), json(&arenas.usage()));
    Ok(report)
}

#[cfg(test)]
mod tests;
