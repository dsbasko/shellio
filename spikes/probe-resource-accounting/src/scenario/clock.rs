//! Сценарий `clock-bench`: микрозамер правила «Часы потока» — `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`
//! против `thread_info(THREAD_BASIC_INFO)` по точности против `getrusage(RUSAGE_SELF)` в
//! однопоточном процессе и по стоимости чтения. Ожидания — отчёт, раздел «Ожидания», `clock-bench`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;

use super::stand::json;
use crate::background::uptime_ns;
use crate::proc::units::{self, Load};
use crate::results::{Cell, Metric, Report, Status};
use crate::spans::clock::{self, ClockKind};

/// Длительности участков по монотонным часам: размер операции «Стоимости», средний и длинный.
pub const SPANS: [Duration; 3] = [
    Duration::from_micros(500),
    Duration::from_millis(5),
    Duration::from_millis(100),
];
pub const REPEATS: usize = 5;
pub const COST_PAIRS: usize = 100;
pub const COST_BATCH: usize = 50_000;
/// Допуск строки «CPU участков владельца»: 0,2 мс + 2% оракула.
pub const ABSOLUTE_NS: f64 = 200_000.0;
pub const RELATIVE: f64 = 0.02;

const ORACLE_ACCURACY: &str = "getrusage(RUSAGE_SELF) однопоточного процесса на зеркальных границах участка; |часы − оракул| ≤ 0,2 мс + 2% оракула во всех участках, отказов чтения нет";
const ORACLE_COST: &str = "медиана CPU процесса на чтение: clock_gettime не дороже thread_info";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Sample {
    pub load: Load,
    pub target_us: u64,
    pub wall_ns: u64,
    /// Дельта `ru_utime + ru_stime`, нс.
    pub oracle_ns: u64,
    /// Дельта часов по кандидату; `None` — чтение отказало.
    pub clocks: BTreeMap<ClockKind, Option<u64>>,
    /// Кандидат, который читался первым на входе и последним на выходе.
    pub outer: ClockKind,
}

pub fn tolerance_ns(oracle_ns: u64) -> f64 {
    ABSOLUTE_NS + RELATIVE * oracle_ns as f64
}

pub fn accuracy_cell(kind: ClockKind, samples: &[Sample]) -> Cell {
    let id = format!("clock-accuracy-{}", kind.name());
    let mut failed_reads = 0usize;
    let mut outside = 0usize;
    let mut worst: Option<(f64, &Sample, u64)> = None;
    for sample in samples {
        let Some(value) = sample.clocks.get(&kind).copied().flatten() else {
            failed_reads += 1;
            continue;
        };
        let diff = (value as f64 - sample.oracle_ns as f64).abs();
        let share = diff / tolerance_ns(sample.oracle_ns);
        if share > 1.0 {
            outside += 1;
        }
        if worst.is_none_or(|(w, _, _)| share > w) {
            worst = Some((share, sample, value));
        }
    }
    let status = if !samples.is_empty() && failed_reads == 0 && outside == 0 {
        Status::Pass
    } else {
        Status::Fail
    };
    let worst = worst.map_or("нет".to_owned(), |(share, sample, value)| {
        format!(
            "{:?} {} мкс: часы {:.1} мкс, оракул {:.1} мкс, доля допуска {:.3}",
            sample.load,
            sample.target_us,
            value as f64 / 1e3,
            sample.oracle_ns as f64 / 1e3,
            share
        )
    });
    Cell::new(
        &id,
        status,
        ORACLE_ACCURACY,
        format!(
            "участков {}, вне допуска {outside}, отказов чтения {failed_reads}, худший — {worst}",
            samples.len()
        ),
    )
}

fn median(values: &[f64]) -> Option<f64> {
    Metric::new("m", "", values.to_vec()).median
}

pub fn cost_cell(gettime_ns: &[f64], thread_info_ns: &[f64]) -> Cell {
    let (a, b) = (median(gettime_ns), median(thread_info_ns));
    let status = match (a, b) {
        (Some(a), Some(b)) if a <= b => Status::Pass,
        _ => Status::Fail,
    };
    let show = |v: Option<f64>| v.map_or("нет".to_owned(), |v| format!("{v:.1} нс"));
    Cell::new(
        "clock-cost",
        status,
        ORACLE_COST,
        format!(
            "медиана CPU на чтение: clock_gettime {}, thread_info {}, партий по {COST_BATCH}: {}",
            show(a),
            show(b),
            gettime_ns.len().min(thread_info_ns.len())
        ),
    )
}

/// Итог правила «Часы потока».
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Choice {
    pub clock: ClockKind,
    /// Ни одни часы не точны: отрицательный вывод «искажение учёта участков».
    pub none_accurate: bool,
}

/// Из точных — более дешёвые; `thread_info` только при строго меньшей медиане, иначе
/// `clock_gettime`. Ни одни не точны — `clock_gettime` с отрицательным выводом.
pub fn choose(accurate: [bool; 2], median_ns: [Option<f64>; 2]) -> Choice {
    let [gettime, thread_info] = accurate;
    let cheaper_thread_info = matches!(median_ns, [Some(a), Some(b)] if b < a);
    let clock = match (gettime, thread_info) {
        (true, true) if cheaper_thread_info => ClockKind::ThreadInfo,
        (false, true) => ClockKind::ThreadInfo,
        _ => ClockKind::ClockGettime,
    };
    Choice {
        clock,
        none_accurate: !gettime && !thread_info,
    }
}

/// Число потоков процесса по `proc_pidinfo(PROC_PIDTASKINFO)`.
pub(super) fn thread_count() -> Result<i32, String> {
    // SAFETY: proc_taskinfo — POD, нули допустимы.
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: info — действительный выходной буфер размера size.
    let got = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    if got != size {
        return Err(format!(
            "proc_pidinfo(PROC_PIDTASKINFO): {got} байт, {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(info.pti_threadnum)
}

fn oracle_ns() -> Result<u64, String> {
    let (user, system) = units::self_getrusage()?;
    Ok(user + system)
}

fn sample(load: Load, duration: Duration, outer: ClockKind) -> Result<Sample, String> {
    let inner = match outer {
        ClockKind::ClockGettime => ClockKind::ThreadInfo,
        ClockKind::ThreadInfo => ClockKind::ClockGettime,
    };
    let wall_start = uptime_ns();
    let oracle_start = oracle_ns()?;
    let outer_start = outer.now_ns();
    let inner_start = inner.now_ns();
    let deadline = wall_start + duration.as_nanos() as u64;
    match load {
        Load::User => units::busy_user(deadline),
        Load::System => units::busy_system(deadline)?,
    }
    let inner_end = inner.now_ns();
    let outer_end = outer.now_ns();
    let oracle_end = oracle_ns()?;
    let wall_end = uptime_ns();
    let delta = |start: Option<u64>, end: Option<u64>| Some(end?.saturating_sub(start?));
    Ok(Sample {
        load,
        target_us: duration.as_micros() as u64,
        wall_ns: wall_end.saturating_sub(wall_start),
        oracle_ns: oracle_end.saturating_sub(oracle_start),
        clocks: BTreeMap::from([
            (outer, delta(outer_start, outer_end)),
            (inner, delta(inner_start, inner_end)),
        ]),
        outer,
    })
}

/// Партия чтений: CPU процесса и стенное время на чтение, нс, и число отказов.
fn batch(kind: ClockKind) -> Result<(f64, f64, usize), String> {
    let mut failed = 0usize;
    let mut sum = 0u64;
    let cpu_start = oracle_ns()?;
    let wall_start = uptime_ns();
    for _ in 0..COST_BATCH {
        match kind.now_ns() {
            Some(ns) => sum = sum.wrapping_add(ns),
            None => failed += 1,
        }
    }
    let wall_end = uptime_ns();
    let cpu_end = oracle_ns()?;
    std::hint::black_box(sum);
    let per = |ns: u64| ns as f64 / COST_BATCH as f64;
    Ok((
        per(cpu_end.saturating_sub(cpu_start)),
        per(wall_end.saturating_sub(wall_start)),
        failed,
    ))
}

pub fn run() -> Result<Report, String> {
    let threads_before = thread_count()?;
    let mut samples = Vec::new();
    let mut index = 0usize;
    for _ in 0..REPEATS {
        for duration in SPANS {
            for load in [Load::User, Load::System] {
                let outer = ClockKind::ALL[index % 2];
                index += 1;
                samples.push(sample(load, duration, outer)?);
            }
        }
    }
    let mut cpu = [Vec::new(), Vec::new()];
    let mut wall = [Vec::new(), Vec::new()];
    let mut cost_failed = [0usize; 2];
    for pair in 0..COST_PAIRS {
        for step in 0..2 {
            let which = (pair + step) % 2;
            let (c, w, failed) = batch(ClockKind::ALL[which])?;
            cpu[which].push(c);
            wall[which].push(w);
            cost_failed[which] += failed;
        }
    }
    let threads_after = thread_count()?;
    if threads_before != 1 || threads_after != 1 {
        return Err(format!(
            "предусловие: процесс однопоточный — потоков {threads_before} до серии и {threads_after} после"
        ));
    }
    let mut report = Report::default();
    let mut accurate = [false; 2];
    for (which, kind) in ClockKind::ALL.into_iter().enumerate() {
        let mut cell = accuracy_cell(kind, &samples);
        if cost_failed[which] > 0 {
            cell.status = Status::Fail;
            cell.reason.push_str(&format!(
                ", отказов чтения в партиях стоимости {}",
                cost_failed[which]
            ));
        }
        accurate[which] = cell.status == Status::Pass;
        report.cells.push(cell);
        report.metrics.push(Metric::new(
            &format!("clock.{}.cpu_ns_per_read", kind.name()),
            "нс",
            cpu[which].clone(),
        ));
        report.metrics.push(Metric::new(
            &format!("clock.{}.wall_ns_per_read", kind.name()),
            "нс",
            wall[which].clone(),
        ));
        report.metrics.push(Metric::new(
            &format!("clock.{}.error_share_of_tolerance", kind.name()),
            "доля допуска",
            samples
                .iter()
                .filter_map(|s| {
                    let value = s.clocks.get(&kind).copied().flatten()?;
                    Some((value as f64 - s.oracle_ns as f64).abs() / tolerance_ns(s.oracle_ns))
                })
                .collect(),
        ));
    }
    report.cells.push(cost_cell(&cpu[0], &cpu[1]));
    let choice = choose(accurate, [median(&cpu[0]), median(&cpu[1])]);
    report.raw.insert("samples".to_owned(), json(&samples));
    report
        .raw
        .insert("threads".to_owned(), json(&[threads_before, threads_after]));
    report.raw.insert("choice".to_owned(), json(&choice));
    report.notes.push(format!(
        "выбор по правилу «Часы потока»: {}{}; в коде spans::clock::Chosen = {}; профиль сборки {}",
        choice.clock.name(),
        if choice.none_accurate {
            " (ни одни часы не точны: искажение учёта участков)"
        } else {
            ""
        },
        clock::chosen_name(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(target_us: u64, oracle_ns: u64, gettime: Option<u64>, info: Option<u64>) -> Sample {
        Sample {
            load: Load::User,
            target_us,
            wall_ns: target_us * 1_000,
            oracle_ns,
            clocks: BTreeMap::from([
                (ClockKind::ClockGettime, gettime),
                (ClockKind::ThreadInfo, info),
            ]),
            outer: ClockKind::ClockGettime,
        }
    }

    #[test]
    fn tolerance_is_absolute_plus_relative() {
        assert_eq!(tolerance_ns(0), 200_000.0);
        assert_eq!(tolerance_ns(100_000_000), 2_200_000.0);
    }

    #[test]
    fn accuracy_needs_every_span_inside_and_no_failed_reads() {
        // 100 мс: на границе допуска (2,2 мс) — в допуске, на 2,3 мс — нет.
        let inside = [
            sample(500, 500_000, Some(690_000), Some(500_000)),
            sample(100_000, 100_000_000, Some(102_200_000), Some(97_800_000)),
        ];
        assert_eq!(
            accuracy_cell(ClockKind::ClockGettime, &inside).status,
            Status::Pass
        );
        assert_eq!(
            accuracy_cell(ClockKind::ThreadInfo, &inside).status,
            Status::Pass
        );
        let scale_error = [sample(
            100_000,
            100_000_000,
            Some(102_300_000),
            Some(100_000_000),
        )];
        let cell = accuracy_cell(ClockKind::ClockGettime, &scale_error);
        assert_eq!(cell.status, Status::Fail);
        assert!(cell.reason.contains("вне допуска 1"));
        let failed = [sample(500, 500_000, Some(500_000), None)];
        let cell = accuracy_cell(ClockKind::ThreadInfo, &failed);
        assert_eq!(cell.status, Status::Fail);
        assert!(cell.reason.contains("отказов чтения 1"));
        assert_eq!(
            accuracy_cell(ClockKind::ThreadInfo, &[]).status,
            Status::Fail
        );
    }

    #[test]
    fn cost_cell_passes_when_clock_gettime_is_not_dearer() {
        assert_eq!(
            cost_cell(&[20.0, 22.0, 21.0], &[40.0, 41.0]).status,
            Status::Pass
        );
        assert_eq!(cost_cell(&[30.0], &[30.0]).status, Status::Pass);
        assert_eq!(cost_cell(&[50.0], &[30.0]).status, Status::Fail);
        assert_eq!(cost_cell(&[], &[30.0]).status, Status::Fail);
    }

    #[test]
    fn rule_takes_cheaper_accurate_clock_and_clock_gettime_on_tie() {
        let pick = |accurate, medians| choose(accurate, medians).clock;
        assert_eq!(
            pick([true, true], [Some(30.0), Some(20.0)]),
            ClockKind::ThreadInfo
        );
        assert_eq!(
            pick([true, true], [Some(20.0), Some(30.0)]),
            ClockKind::ClockGettime
        );
        assert_eq!(
            pick([true, true], [Some(25.0), Some(25.0)]),
            ClockKind::ClockGettime
        );
        assert_eq!(
            pick([false, true], [Some(10.0), Some(90.0)]),
            ClockKind::ThreadInfo
        );
        assert_eq!(
            pick([true, false], [Some(90.0), Some(10.0)]),
            ClockKind::ClockGettime
        );
        let none = choose([false, false], [Some(90.0), Some(10.0)]);
        assert_eq!(none.clock, ClockKind::ClockGettime);
        assert!(none.none_accurate);
        assert!(!choose([true, false], [None, None]).none_accurate);
    }
}
