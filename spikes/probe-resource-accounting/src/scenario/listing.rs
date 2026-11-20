//! Сценарий `list-bench`: микрозамер правила «Список процессов» — `sysctl(KERN_PROC_ALL)` одним
//! вызовом против `proc_listallpids` и `proc_pidinfo(PROC_PIDTBSDINFO)` на каждый PID. Ожидания —
//! отчёт, раздел «Ожидания», `list-bench`.

use std::collections::BTreeMap;

use super::stand::json;
use crate::background::{self, uptime_ns};
use crate::proc;
use crate::results::{Cell, Metric, Report, Status};

pub const PAIRS: usize = 100;

const ORACLE_FIELDS: &str = "proc_pidinfo(PROC_PIDTBSDINFO) отвечает для каждого PID proc_listallpids: тот же набор полей, что у записи sysctl";
const ORACLE_COST: &str =
    "медиана CPU потока на вызов: sysctl(KERN_PROC_ALL) не дороже proc_listallpids + proc_pidinfo";

fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts — действительный выходной буфер.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
}

/// Один вызов способа B: перечисленные PID и отказы `proc_pidinfo` по `errno`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct PidInfoPass {
    pub listed: usize,
    pub answered: usize,
    pub failed_by_errno: BTreeMap<i32, usize>,
}

fn pidinfo_pass() -> Result<PidInfoPass, String> {
    let mut pass = PidInfoPass::default();
    for pid in background::list_pids()? {
        pass.listed += 1;
        match proc::bsd_info(pid) {
            Ok(info) => {
                std::hint::black_box(info);
                pass.answered += 1;
            }
            Err(error) => {
                *pass
                    .failed_by_errno
                    .entry(error.errno().unwrap_or(0))
                    .or_default() += 1
            }
        }
    }
    Ok(pass)
}

fn median(values: &[f64]) -> f64 {
    Metric::new("m", "", values.to_vec())
        .median
        .unwrap_or(f64::NAN)
}

pub fn fields_cell(pass: &PidInfoPass) -> Cell {
    let failed: usize = pass.failed_by_errno.values().sum();
    Cell::new(
        "list-fields",
        if failed == 0 && pass.listed > 0 {
            Status::Pass
        } else {
            Status::Fail
        },
        ORACLE_FIELDS,
        format!(
            "PID {}, proc_pidinfo ответил {}, отказы по errno {:?}",
            pass.listed, pass.answered, pass.failed_by_errno
        ),
    )
}

pub fn cost_cell(sysctl_cpu_ns: &[f64], pidinfo_cpu_ns: &[f64]) -> Cell {
    let (a, b) = (median(sysctl_cpu_ns), median(pidinfo_cpu_ns));
    let status = if a.is_finite() && b.is_finite() && a <= b {
        Status::Pass
    } else {
        Status::Fail
    };
    Cell::new(
        "list-cost",
        status,
        ORACLE_COST,
        format!(
            "медиана CPU потока: sysctl {:.1} мкс, proc_listallpids + proc_pidinfo {:.1} мкс, пар {}",
            a / 1e3,
            b / 1e3,
            sysctl_cpu_ns.len().min(pidinfo_cpu_ns.len())
        ),
    )
}

/// Выбор по правилу: B — только если он дешевле и даёт тот же набор полей.
pub fn choice(fields: &Cell, cost: &Cell) -> &'static str {
    if fields.status == Status::Pass && cost.status == Status::Fail {
        "proc_listallpids + proc_pidinfo"
    } else {
        "sysctl(KERN_PROC_ALL)"
    }
}

pub fn run() -> Result<Report, String> {
    let mut cpu = [Vec::new(), Vec::new()];
    let mut wall = [Vec::new(), Vec::new()];
    let mut listed = Vec::new();
    let mut last = PidInfoPass::default();
    for index in 0..PAIRS {
        // Порядок в паре чередуется: ни один способ не идёт всегда первым.
        for step in 0..2 {
            let method = (index + step) % 2;
            let (c0, w0) = (thread_cpu_ns(), uptime_ns());
            if method == 0 {
                listed.push(proc::list_all()?.len() as f64);
            } else {
                last = pidinfo_pass()?;
            }
            let (c1, w1) = (thread_cpu_ns(), uptime_ns());
            cpu[method].push(c1.saturating_sub(c0) as f64);
            wall[method].push(w1.saturating_sub(w0) as f64);
        }
    }
    let fields = fields_cell(&last);
    let cost = cost_cell(&cpu[0], &cpu[1]);
    let chosen = choice(&fields, &cost);
    let mut report = Report::default();
    for (index, name) in ["sysctl", "pidinfo"].iter().enumerate() {
        report.metrics.push(Metric::new(
            &format!("list.{name}.thread_cpu_us"),
            "мкс",
            cpu[index].iter().map(|ns| ns / 1e3).collect(),
        ));
        report.metrics.push(Metric::new(
            &format!("list.{name}.wall_us"),
            "мкс",
            wall[index].iter().map(|ns| ns / 1e3).collect(),
        ));
    }
    report
        .metrics
        .push(Metric::new("list.sysctl.listed", "процессов", listed));
    report.raw.insert("pidinfo_last".to_owned(), json(&last));
    report.raw.insert("choice".to_owned(), json(&chosen));
    report.notes.push(format!(
        "выбор по правилу «Список процессов»: {chosen}; профиль сборки {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    ));
    report.cells.push(fields);
    report.cells.push(cost);
    Ok(report)
}
