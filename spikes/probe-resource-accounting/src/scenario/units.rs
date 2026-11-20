//! Сценарий `units`: калибровка единиц `ri_user_time`/`ri_system_time` против `getrusage` и
//! `mach_timebase_info`, проверка источника процессов на собственном процессе. Ожидания — отчёт,
//! раздел «Ожидания», `units`.

use std::time::{SystemTime, UNIX_EPOCH};

use super::stand::json;
use crate::background::{self, BackgroundCheck, THRESHOLDS};
use crate::proc::units::{self as calib, Comparison, Component, Sample, Timebase, Verdict};
use crate::proc::{self, ProcRead, Unread};
use crate::results::{BackgroundRecord, Cell, Metric, NotRunCause, Report, Status};

const ORACLE_MACH: &str = "getrusage(RUSAGE_SELF) на зеркальных границах образца; |значение − оракул| ≤ 0,5% окна + 2% оракула; «тики Mach» в допуске во всех образцах, «наносекунды» вне допуска хотя бы в одном";
const ORACLE_SOURCE: &str = "свой PID в списке sysctl(KERN_PROC_ALL) с getppid, getpgrp, getuid; время старта списка = proc_bsdinfo; getsid = getsid(0); rusage прочитан, повторное чтение той же идентичности сходится";

/// Клетка калибровки по сравнениям её образцов; шумное окно «во время» даёт «не подтверждено».
pub fn mach_cell(
    id: &str,
    comparisons: &[Comparison],
    timebase: Timebase,
    during_quiet: bool,
) -> Cell {
    let verdict = calib::verdict(comparisons, timebase);
    if verdict == Verdict::NoSignal {
        return Cell::not_run(
            id,
            NotRunCause::NotObserved,
            ORACLE_MACH,
            format!(
                "нет образцов с сигналом: оракул system ниже {:.0}% окна во всех образцах `system`",
                calib::SYSTEM_SIGNAL_FRACTION * 100.0
            ),
        );
    }
    let range = |values: Vec<f64>| -> String {
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        if values.is_empty() {
            "нет".to_owned()
        } else {
            format!("{min:.4}–{max:.4}")
        }
    };
    let worst = comparisons
        .iter()
        .map(|c| {
            c.mach_ns
                .map_or(f64::INFINITY, |ns| (ns as f64 - c.oracle_ns as f64).abs())
                / c.tolerance_ns
        })
        .fold(0.0, f64::max);
    let mut reason = format!(
        "образцов {}, timebase {}/{}, отношение к оракулу: «тики Mach» {}, «наносекунды» {}; худшее отклонение «тиков Mach» — {:.2} допуска; «тики Mach» в допуске {}/{}, «наносекунды» в допуске {}/{}",
        comparisons.len(),
        timebase.numer,
        timebase.denom,
        range(comparisons.iter().filter_map(|c| c.mach_ratio).collect()),
        range(comparisons.iter().filter_map(|c| c.raw_ratio).collect()),
        worst,
        comparisons.iter().filter(|c| c.mach_ok).count(),
        comparisons.len(),
        comparisons.iter().filter(|c| c.raw_ok).count(),
        comparisons.len(),
    );
    let status = match verdict {
        Verdict::Pass => Status::Pass,
        Verdict::PassIndistinct => {
            reason.push_str("; timebase 1/1 — гипотезы не различимы");
            Status::Pass
        }
        Verdict::Fail => Status::Fail,
        Verdict::Undecided => {
            reason.push_str("; обе гипотезы в допуске — не различены");
            Status::Unconfirmed
        }
        Verdict::NoSignal => Status::NotRun,
    };
    super::formula::noisy(Cell::new(id, status, ORACLE_MACH, reason), during_quiet)
}

/// Что известно о своём процессе помимо источника: оракул клетки `source-self`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SelfFacts {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub uid: u32,
    pub sid: i32,
}

impl SelfFacts {
    pub fn current() -> SelfFacts {
        // SAFETY: вызовы без указателей; getsid(0) — сессия своего процесса.
        unsafe {
            SelfFacts {
                pid: std::process::id() as i32,
                ppid: libc::getppid(),
                pgid: libc::getpgrp(),
                uid: libc::getuid(),
                sid: libc::getsid(0),
            }
        }
    }
}

/// Клетка `source-self`: свой процесс в проходе источника и повторное чтение идентичности.
pub fn source_cell(
    facts: &SelfFacts,
    own: Option<&ProcRead>,
    reread: Option<Result<proc::Usage, Unread>>,
) -> Cell {
    let Some(own) = own else {
        return Cell::new(
            "source-self",
            Status::Fail,
            ORACLE_SOURCE,
            format!("PID {} нет в списке sysctl(KERN_PROC_ALL)", facts.pid),
        );
    };
    let mut problems = Vec::new();
    let listed = &own.listed;
    if (listed.ppid, listed.pgid, listed.uid, listed.ruid)
        != (facts.ppid, facts.pgid, facts.uid, facts.uid)
    {
        problems.push(format!(
            "список: родитель {}, группа {}, uid {}, ruid {}; ожидалось {}, {}, {}, {}",
            listed.ppid,
            listed.pgid,
            listed.uid,
            listed.ruid,
            facts.ppid,
            facts.pgid,
            facts.uid,
            facts.uid
        ));
    }
    match &own.info {
        Ok(info) if info.start == listed.start => {}
        Ok(info) => problems.push(format!(
            "время старта: список {}.{:06}, proc_bsdinfo {}.{:06}",
            listed.start.sec, listed.start.usec, info.start.sec, info.start.usec
        )),
        Err(error) => problems.push(format!("proc_pidinfo: {error}")),
    }
    if own.session != Ok(facts.sid) {
        problems.push(format!(
            "getsid: {:?}, ожидалось {}",
            own.session, facts.sid
        ));
    }
    let identity = match &own.reading {
        Ok(reading) => Some(reading.identity),
        Err(unread) => {
            problems.push(format!("чтение: {unread}"));
            None
        }
    };
    match (identity, reread) {
        (Some(identity), Some(Ok(usage))) if usage.start_abstime == identity.start_abstime => {}
        (Some(_), Some(Ok(usage))) => problems.push(format!(
            "повторное чтение: другая ri_proc_start_abstime {}",
            usage.start_abstime
        )),
        (Some(_), Some(Err(unread))) => problems.push(format!("повторное чтение: {unread}")),
        (Some(_), None) => problems.push("повторного чтения нет".to_owned()),
        (None, _) => {}
    }
    if problems.is_empty() {
        let reading = own.reading.as_ref().ok();
        Cell::new(
            "source-self",
            Status::Pass,
            ORACLE_SOURCE,
            format!(
                "PID {}, родитель {}, группа {}, сессия {}, старт {}.{:06}, ri_proc_start_abstime {}, footprint {} байт",
                facts.pid,
                listed.ppid,
                listed.pgid,
                facts.sid,
                listed.start.sec,
                listed.start.usec,
                reading.map_or(0, |r| r.identity.start_abstime),
                reading.map_or(0, |r| r.usage.phys_footprint),
            ),
        )
    } else {
        Cell::new(
            "source-self",
            Status::Fail,
            ORACLE_SOURCE,
            problems.join("; "),
        )
    }
}

/// Сводка одного прохода источника по всем процессам: покрытие и отказы по вызовам.
pub fn pass_summary(reads: &[ProcRead], pass_ns: u64) -> serde_json::Value {
    let mut unread = std::collections::BTreeMap::<&str, usize>::new();
    for read in reads {
        if let Err(kind) = &read.reading {
            let name = match kind {
                Unread::Failed { .. } => "failed",
                Unread::Replaced { .. } => "replaced",
                Unread::Unconfirmed { .. } => "unconfirmed",
                Unread::IdentityChanged { .. } => "identity_changed",
            };
            *unread.entry(name).or_default() += 1;
        }
    }
    serde_json::json!({
        "pass_ns": pass_ns,
        "listed": reads.len(),
        "read": reads.iter().filter(|r| r.reading.is_ok()).count(),
        "zombies": reads.iter().filter(|r| r.listed.zombie).count(),
        "unread": unread,
        "failures_by_call_errno": proc::failure_counts(reads),
        "session_read": reads.iter().filter(|r| r.session.is_ok()).count(),
    })
}

fn wall_now() -> (u64, u32) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_secs(), now.subsec_micros())
}

#[allow(deprecated)]
fn mach_now() -> u64 {
    // SAFETY: mach_absolute_time без аргументов.
    unsafe { libc::mach_absolute_time() }
}

/// Возраст процесса по часам Mach и по настенным часам: наблюдение без вердикта.
fn age_observation(timebase: Timebase) -> serde_json::Value {
    let pid = std::process::id() as i32;
    let usage = proc::usage(pid);
    let info = proc::bsd_info(pid);
    let now_mach = mach_now();
    let (sec, usec) = wall_now();
    match (usage, info) {
        (Ok(usage), Ok(info)) => {
            let mach_ns = timebase.ticks_to_ns(now_mach.saturating_sub(usage.start_abstime));
            let wall_ns = (sec.saturating_sub(info.start.sec) as i128) * 1_000_000_000
                + (i128::from(usec) - i128::from(info.start.usec)) * 1_000;
            serde_json::json!({
                "start_abstime": usage.start_abstime,
                "mach_absolute_time": now_mach,
                "age_mach_ns": mach_ns,
                "start_wall": { "sec": info.start.sec, "usec": info.start.usec },
                "now_wall": { "sec": sec, "usec": usec },
                "age_wall_ns": wall_ns,
                "difference_ns": mach_ns.map(|m| i128::from(m) - wall_ns),
            })
        }
        (usage, info) => serde_json::json!({
            "error": format!("rusage {:?}, proc_pidinfo {:?}", usage.err(), info.err()),
        }),
    }
}

fn record(label: &str, check: BackgroundCheck) -> BackgroundRecord {
    BackgroundRecord {
        label: label.to_owned(),
        check,
    }
}

/// Тело сценария после тихого окна фона.
pub fn run(before: Vec<BackgroundRecord>) -> Result<Report, String> {
    let units = background::current_units();
    let timebase = units.timebase();
    let facts = SelfFacts::current();
    let source = proc::System;

    let pass_start = background::uptime_ns();
    let reads = proc::read_all(&source)?;
    let pass_ns = background::uptime_ns() - pass_start;
    let own = reads.iter().find(|r| r.listed.pid == facts.pid);
    let reread = own
        .and_then(|own| own.reading.as_ref().ok())
        .map(|reading| proc::read_identity(&source, reading.identity));
    let source_self = source_cell(&facts, own, reread);

    let during_start = background::sample_bound()?;
    let mut samples: Vec<Sample> = Vec::new();
    for (load, duration) in calib::schedule() {
        samples.push(calib::measure(load, duration)?);
    }
    let during_end = background::sample_bound()?;
    let during = background::evaluate(
        &during_start,
        &during_end,
        facts.pid,
        &[],
        units,
        THRESHOLDS,
    );
    let during_quiet = during.quiet;
    let after = background::measure(background::WINDOW, &[])?;
    let age = age_observation(timebase);

    let mut report = Report::default();
    let mut comparisons = serde_json::Map::new();
    for (id, component) in [
        ("units-user", Component::User),
        ("units-system", Component::System),
        ("units-total", Component::Total),
    ] {
        let cell_comparisons = calib::cell_comparisons(&samples, component, timebase);
        report
            .cells
            .push(mach_cell(id, &cell_comparisons, timebase, during_quiet));
        report.metrics.push(Metric::new(
            &format!("{id}.mach_ratio"),
            "отношение к оракулу",
            cell_comparisons
                .iter()
                .map(|c| c.mach_ratio.unwrap_or(f64::NAN))
                .collect(),
        ));
        report.metrics.push(Metric::new(
            &format!("{id}.raw_ratio"),
            "отношение к оракулу",
            cell_comparisons
                .iter()
                .map(|c| c.raw_ratio.unwrap_or(f64::NAN))
                .collect(),
        ));
        comparisons.insert(
            id.to_owned(),
            serde_json::to_value(&cell_comparisons).map_err(|err| err.to_string())?,
        );
    }
    report.cells.push(source_self);
    report.metrics.push(Metric::new(
        "source_pass_ms",
        "мс",
        vec![pass_ns as f64 / 1e6],
    ));

    report.raw.insert("timebase".to_owned(), json(&timebase));
    report.raw.insert("samples".to_owned(), json(&samples));
    report.raw.insert(
        "comparisons".to_owned(),
        serde_json::Value::Object(comparisons),
    );
    report.raw.insert("self".to_owned(), json(&facts));
    report
        .raw
        .insert("own_read".to_owned(), json(&own.cloned()));
    report
        .raw
        .insert("source_pass".to_owned(), pass_summary(&reads, pass_ns));
    report.raw.insert("age".to_owned(), age);
    report.notes.push(format!(
        "допуск: {:.1}% окна образца + {:.0}% оракула (строка «CPU% процесса» таблицы оракулов, переведённая в нс окна)",
        calib::WINDOW_FRACTION * 100.0,
        calib::RELATIVE * 100.0
    ));

    report.background = before;
    report.background.push(record("during:series", during));
    report.background.push(record("after", after));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::{FakeSource, listed};
    use crate::proc::units::Load;

    const APPLE: Timebase = Timebase {
        numer: 125,
        denom: 3,
    };

    fn sample(load: Load, user_ns: u64, system_ns: u64, scale: u64) -> Sample {
        Sample {
            load,
            target_ms: 1000,
            wall_ns: 1_000_000_000,
            oracle_user_ns: user_ns,
            oracle_system_ns: system_ns,
            raw_user: user_ns * 3 / 125 * scale,
            raw_system: system_ns * 3 / 125 * scale,
        }
    }

    #[test]
    fn mach_cell_maps_verdicts_and_noise() {
        let good = calib::cell_comparisons(
            &[sample(Load::User, 990_000_000, 1_000_000, 1)],
            Component::User,
            APPLE,
        );
        assert_eq!(
            mach_cell("units-user", &good, APPLE, true).status,
            Status::Pass
        );
        let noisy = mach_cell("units-user", &good, APPLE, false);
        assert_eq!(noisy.status, Status::Unconfirmed);
        assert!(noisy.reason.contains("без шума был бы итог pass"));
        let bad = calib::cell_comparisons(
            &[sample(Load::User, 990_000_000, 1_000_000, 2)],
            Component::User,
            APPLE,
        );
        assert_eq!(
            mach_cell("units-user", &bad, APPLE, true).status,
            Status::Fail
        );
        let none = mach_cell("units-system", &[], APPLE, false);
        assert_eq!(none.status, Status::NotRun);
        assert_eq!(none.not_run_cause, Some(NotRunCause::NotObserved));
        assert!(none.problems().is_empty());
    }

    fn facts() -> SelfFacts {
        SelfFacts {
            pid: 10,
            ppid: 1,
            pgid: 1,
            uid: 501,
            sid: 1,
        }
    }

    #[test]
    fn source_cell_passes_on_consistent_reads() {
        let mut source = FakeSource::default();
        source.add(listed(10, 1, 100), 5_000, 240);
        let read = proc::read_listed(&source, &source.listed[0]);
        let identity = read.reading.as_ref().unwrap().identity;
        let cell = source_cell(
            &facts(),
            Some(&read),
            Some(proc::read_identity(&source, identity)),
        );
        assert_eq!(cell.status, Status::Pass, "{}", cell.reason);
    }

    #[test]
    fn source_cell_fails_on_any_mismatch() {
        let mut source = FakeSource::default();
        source.add(listed(10, 1, 100), 5_000, 240);
        let read = proc::read_listed(&source, &source.listed[0]);
        let identity = read.reading.as_ref().unwrap().identity;
        let reread = || Some(proc::read_identity(&source, identity));
        assert_eq!(source_cell(&facts(), None, None).status, Status::Fail);
        let missing = source_cell(&facts(), Some(&read), None);
        assert_eq!(missing.status, Status::Fail);
        assert!(missing.reason.contains("повторного чтения нет"));

        let mut wrong_parent = facts();
        wrong_parent.ppid = 2;
        let cell = source_cell(&wrong_parent, Some(&read), reread());
        assert_eq!(cell.status, Status::Fail);
        assert!(cell.reason.contains("список: родитель"), "{}", cell.reason);

        let mut wrong_session = facts();
        wrong_session.sid = 2;
        let cell = source_cell(&wrong_session, Some(&read), reread());
        assert_eq!(cell.status, Status::Fail);
        assert!(cell.reason.contains("getsid"), "{}", cell.reason);

        let mut moved = read.clone();
        if let Ok(info) = &mut moved.info {
            info.start.usec += 1;
        }
        let cell = source_cell(&facts(), Some(&moved), reread());
        assert_eq!(cell.status, Status::Fail);
        assert!(cell.reason.contains("время старта"), "{}", cell.reason);
        let changed = Err(Unread::IdentityChanged {
            expected: 5_000,
            observed: 6_000,
        });
        let cell = source_cell(&facts(), Some(&read), Some(changed));
        assert_eq!(cell.status, Status::Fail);
        assert!(cell.reason.contains("другая жизнь PID"));
    }

    #[test]
    fn pass_summary_counts_unread_kinds() {
        let mut source = FakeSource::default();
        source.add(listed(10, 1, 100), 5_000, 240);
        source.listed.push(listed(11, 1, 100));
        source.usage.insert(11, Err(proc::ReadError::Denied));
        source.session.insert(11, Ok(1));
        let reads = proc::read_all(&source).unwrap();
        let summary = pass_summary(&reads, 1_000);
        assert_eq!(summary["listed"], 2);
        assert_eq!(summary["read"], 1);
        assert_eq!(summary["unread"]["failed"], 1);
        assert_eq!(summary["failures_by_call_errno"]["proc_pid_rusage"]["1"], 1);
    }
}
