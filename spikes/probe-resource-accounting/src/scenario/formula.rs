//! Сценарии `formula-cpu` и `formula-mem`: формулы снимка против квитанций фикстур. Ожидания —
//! отчёт, раздел «Ожидания», `formula-cpu` и `formula-mem`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use super::stand::{Stand, json, key_of, snap};
use crate::background::{self, THRESHOLDS};
use crate::fixture::Spec;
use crate::fixture::receipts::{self, Event, Mode, Receipt};
use crate::proc::System;
use crate::results::{BackgroundRecord, Cell, Metric, NotRunCause, Report, Status};
use crate::snapshot::{Key, Summary, Value};

pub const LOADS: [(&str, f64); 3] = [("load-10", 10.0), ("load-50", 50.0), ("load-150", 150.0)];
pub const CPU_WINDOW: Duration = Duration::from_secs(10);
pub const CPU_WINDOWS: u64 = 3;
pub const MEMS: [(&str, u64); 3] = [("mem-16", 16), ("mem-128", 128), ("mem-512", 512)];
pub const MIB: u64 = 1 << 20;

const ORACLE_CPU: &str = "getrusage(RUSAGE_SELF) фикстуры на её границах (receipts::cpu_pct); |снимок − оракул| ≤ 0,5 п. п. + 2% оракула в каждом окне";
const ORACLE_CPU_SUM: &str =
    "CPU% сводки = Σ CPU% записей; покрытие полное, partial ложен; |Σ − Σ оракулов| ≤ Σ допусков";
const ORACLE_MEM: &str = "собственное чтение ri_phys_footprint фикстурой в квитанции m0; ±1 МиБ; m0 и m1 расходятся больше 1 МиБ — не подтверждено";
const ORACLE_MEM_SUM: &str =
    "память сводки = Σ footprint записей; покрытие памяти полное, partial ложен";

/// Допуск CPU% по оракулу, п. п.
pub fn cpu_tolerance(oracle: f64) -> f64 {
    0.5 + 0.02 * oracle.abs()
}

/// Окно одного процесса: CPU% записи снимка (или причина пропуска) и оракул квитанций.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CpuWindow {
    pub measured: Result<f64, String>,
    pub oracle: Option<f64>,
}

fn noisy(cell: Cell, during_quiet: bool) -> Cell {
    if during_quiet || cell.status == Status::NotRun {
        return cell;
    }
    Cell::new(
        &cell.id,
        Status::Unconfirmed,
        &cell.oracle,
        format!(
            "фон во время серии шумный, без шума был бы итог {}: {}",
            cell.status.name(),
            cell.reason
        ),
    )
}

/// Клетка CPU% одного процесса по окнам.
pub fn cpu_cell(id: &str, windows: &[CpuWindow], above_100: bool, during_quiet: bool) -> Cell {
    let mut parts = Vec::new();
    let mut failed = false;
    let mut below = false;
    for (index, window) in windows.iter().enumerate() {
        match (&window.measured, window.oracle) {
            (Ok(measured), Some(oracle)) => {
                let diff = measured - oracle;
                let ok = diff.abs() <= cpu_tolerance(oracle);
                failed |= !ok;
                below |= *measured <= 100.0 || oracle <= 100.0;
                parts.push(format!(
                    "окно {}: снимок {measured:.3}%, оракул {oracle:.3}%, разница {diff:+.3} п. п., допуск {:.3}{}",
                    index + 1,
                    cpu_tolerance(oracle),
                    if ok { "" } else { " — вне допуска" }
                ));
            }
            (measured, oracle) => {
                failed = true;
                parts.push(format!(
                    "окно {}: снимок {}, оракул {}",
                    index + 1,
                    match measured {
                        Ok(value) => format!("{value:.3}%"),
                        Err(reason) => format!("нет ({reason})"),
                    },
                    oracle.map_or("нет".to_owned(), |o| format!("{o:.3}%"))
                ));
            }
        }
    }
    if windows.is_empty() {
        return Cell::new(id, Status::Fail, ORACLE_CPU, "окон нет");
    }
    let reason = parts.join("; ");
    if above_100 && below && !failed {
        return Cell::not_run(
            id,
            NotRunCause::NotObserved,
            ORACLE_CPU,
            format!("значение выше 100% не достигнуто: {reason}"),
        );
    }
    let status = if failed { Status::Fail } else { Status::Pass };
    noisy(Cell::new(id, status, ORACLE_CPU, reason), during_quiet)
}

/// Окно суммы: сводка и оракулы трёх процессов.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SumWindow {
    pub summary_pct: Option<f64>,
    pub entries_pct: f64,
    pub complete: bool,
    pub partial: bool,
    pub oracles: Vec<f64>,
}

pub fn sum_window(summary: &Summary, oracles: Vec<f64>) -> SumWindow {
    let entries_pct = summary
        .entries
        .iter()
        .filter_map(|entry| match entry.cpu {
            Value::Measured(delta) => Some(delta.pct),
            Value::Missing(_) => None,
        })
        .sum();
    SumWindow {
        summary_pct: summary.cpu.pct,
        entries_pct,
        complete: summary.cpu.coverage.complete() && summary.memory.coverage.complete(),
        partial: summary.partial,
        oracles,
    }
}

pub fn cpu_sum_cell(windows: &[SumWindow], expected_members: usize, during_quiet: bool) -> Cell {
    let mut parts = Vec::new();
    let mut failed = windows.is_empty();
    for (index, window) in windows.iter().enumerate() {
        let oracle: f64 = window.oracles.iter().sum();
        let tolerance: f64 = window.oracles.iter().map(|o| cpu_tolerance(*o)).sum();
        let Some(summary) = window.summary_pct else {
            failed = true;
            parts.push(format!("окно {}: CPU% сводки нет", index + 1));
            continue;
        };
        let arithmetic = (summary - window.entries_pct).abs() <= 1e-9 * summary.abs().max(1.0);
        let close = (summary - oracle).abs() <= tolerance;
        let ok = arithmetic
            && close
            && window.complete
            && !window.partial
            && window.oracles.len() == expected_members;
        failed |= !ok;
        parts.push(format!(
            "окно {}: сводка {summary:.3}%, Σ записей {:.3}%, Σ оракулов {oracle:.3}%, допуск {tolerance:.3}, покрытие {}, partial {}{}",
            index + 1,
            window.entries_pct,
            if window.complete { "полное" } else { "неполное" },
            window.partial,
            if ok { "" } else { " — нарушение" }
        ));
    }
    let status = if failed { Status::Fail } else { Status::Pass };
    noisy(
        Cell::new("formula-cpu-sum", status, ORACLE_CPU_SUM, parts.join("; ")),
        during_quiet,
    )
}

fn entry_pct(summary: &Summary, key: Key) -> Result<f64, String> {
    let entry = summary
        .entries
        .iter()
        .find(|entry| entry.key == key)
        .ok_or_else(|| format!("PID {} нет в сводке", key.pid))?;
    match entry.cpu {
        Value::Measured(delta) => Ok(delta.pct),
        Value::Missing(gap) => Err(format!("{gap:?}")),
    }
}

/// Тело `formula-cpu` после тихого окна фона.
pub fn run_cpu(before: Vec<BackgroundRecord>, out: &Path) -> Result<Report, String> {
    let units = background::current_units();
    let mut stand = Stand::start(out)?;
    let specs: Vec<Spec> = LOADS
        .iter()
        .map(|(name, percent)| {
            let mut spec = Spec::new(Mode::Load, *name);
            spec.percent = Some(*percent);
            spec
        })
        .collect();
    let names: Vec<String> = LOADS.iter().map(|(name, _)| (*name).to_owned()).collect();
    let ready = stand.spawn_ready(&specs, &[])?;
    let mut keys = BTreeMap::new();
    for name in &names {
        keys.insert(name.clone(), key_of(&ready, name)?);
    }
    let set: BTreeSet<Key> = keys.values().copied().collect();

    let during_start = background::sample_bound()?;
    let t0 = background::uptime_ns();
    let mut bounds: Vec<BTreeMap<String, Receipt>> = Vec::new();
    let mut snaps = Vec::new();
    for k in 0..=CPU_WINDOWS {
        if k > 0 {
            background::hold_until(t0 + k * CPU_WINDOW.as_nanos() as u64);
        }
        bounds.push(stand.receipts(&names, &format!("t{k}"))?);
        snaps.push(snap(&System, &set)?);
    }
    let during_end = background::sample_bound()?;
    let during = background::evaluate(
        &during_start,
        &during_end,
        std::process::id() as i32,
        &[],
        units,
        THRESHOLDS,
    );
    let timebase = stand.timebase;
    let faults = stand.finish()?;
    if !faults.is_empty() {
        return Err(format!("канал фикстур: {}", faults.join("; ")));
    }
    let after = background::measure(background::WINDOW, &[])?;

    let mut summaries = Vec::new();
    for k in 1..bounds.len() {
        summaries.push(crate::snapshot::summarize(
            Some(&snaps[k - 1]),
            &snaps[k],
            &set,
            timebase,
        )?);
    }
    let mut report = Report::default();
    let mut sums = Vec::new();
    for (k, summary) in summaries.iter().enumerate() {
        let oracles = names
            .iter()
            .filter_map(|name| receipts::cpu_pct(&bounds[k][name], &bounds[k + 1][name]))
            .collect();
        sums.push(sum_window(summary, oracles));
    }
    let mut windows_json = serde_json::Map::new();
    for (name, percent) in LOADS {
        let key = keys[name];
        let windows: Vec<CpuWindow> = summaries
            .iter()
            .enumerate()
            .map(|(k, summary)| CpuWindow {
                measured: entry_pct(summary, key),
                oracle: receipts::cpu_pct(&bounds[k][name], &bounds[k + 1][name]),
            })
            .collect();
        let id = format!("formula-cpu-{}", percent as u32);
        report
            .cells
            .push(cpu_cell(&id, &windows, percent > 100.0, during.quiet));
        report.metrics.push(Metric::new(
            &format!("{id}.diff_pp"),
            "п. п.",
            windows
                .iter()
                .map(|w| match (&w.measured, w.oracle) {
                    (Ok(m), Some(o)) => m - o,
                    _ => f64::NAN,
                })
                .collect(),
        ));
        windows_json.insert(id, json(&windows));
    }
    report
        .cells
        .push(cpu_sum_cell(&sums, LOADS.len(), during.quiet));
    report.raw.insert("keys".to_owned(), json(&keys));
    report.raw.insert("ready".to_owned(), json(&ready));
    report.raw.insert("receipts".to_owned(), json(&bounds));
    report.raw.insert("summaries".to_owned(), json(&summaries));
    report.raw.insert(
        "windows".to_owned(),
        serde_json::Value::Object(windows_json),
    );
    report.raw.insert("sums".to_owned(), json(&sums));
    report.background = before;
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

/// Клетка footprint одного процесса.
pub fn mem_cell(id: &str, measured: Result<u64, String>, m0: Option<u64>, m1: Option<u64>) -> Cell {
    let (measured, m0) = match (measured, m0) {
        (Ok(measured), Some(m0)) => (measured, m0),
        (measured, m0) => {
            return Cell::new(
                id,
                Status::Fail,
                ORACLE_MEM,
                format!("снимок {measured:?}, квитанция m0 {m0:?}"),
            );
        }
    };
    let diff = measured as i128 - m0 as i128;
    let mut reason =
        format!("снимок {measured} байт, m0 {m0} байт, разница {diff:+} байт, допуск {MIB}");
    if let Some(m1) = m1 {
        reason.push_str(&format!(", m1 {m1} байт"));
        if (m1 as i128 - m0 as i128).unsigned_abs() > u128::from(MIB) {
            return Cell::new(
                id,
                Status::Unconfirmed,
                ORACLE_MEM,
                format!("footprint фикстуры не устоялся: {reason}"),
            );
        }
    }
    let status = if diff.unsigned_abs() <= u128::from(MIB) {
        Status::Pass
    } else {
        Status::Fail
    };
    Cell::new(id, status, ORACLE_MEM, reason)
}

pub fn mem_sum_cell(summary: &Summary, expected_members: usize) -> Cell {
    let entries: u64 = summary
        .entries
        .iter()
        .filter_map(|entry| match entry.memory {
            Some(Value::Measured(bytes)) => Some(bytes),
            _ => None,
        })
        .sum();
    let coverage = summary.memory.coverage;
    let ok = summary.memory.bytes == entries
        && coverage.complete()
        && coverage.expected == expected_members
        && !summary.partial;
    Cell::new(
        "formula-mem-sum",
        if ok { Status::Pass } else { Status::Fail },
        ORACLE_MEM_SUM,
        format!(
            "сводка {} байт, Σ записей {entries} байт, покрытие памяти {}/{}, partial {}",
            summary.memory.bytes, coverage.counted, coverage.expected, summary.partial
        ),
    )
}

fn own_footprint(receipt: &Receipt) -> Option<u64> {
    receipt.boundary.own.ok().map(|usage| usage.phys_footprint)
}

fn entry_footprint(summary: &Summary, key: Key) -> Result<u64, String> {
    let entry = summary
        .entries
        .iter()
        .find(|entry| entry.key == key)
        .ok_or_else(|| format!("PID {} нет в сводке", key.pid))?;
    match entry.memory {
        Some(Value::Measured(bytes)) => Ok(bytes),
        other => Err(format!("{other:?}")),
    }
}

/// Тело `formula-mem`.
pub fn run_mem(out: &Path) -> Result<Report, String> {
    let mut stand = Stand::start(out)?;
    let specs: Vec<Spec> = MEMS
        .iter()
        .map(|(name, mib)| {
            let mut spec = Spec::new(Mode::Memory, *name);
            spec.mib = Some(*mib);
            spec
        })
        .collect();
    let names: Vec<String> = MEMS.iter().map(|(name, _)| (*name).to_owned()).collect();
    let ready = stand.spawn_ready(&specs, &[])?;
    let mut keys = BTreeMap::new();
    for name in &names {
        keys.insert(name.clone(), key_of(&ready, name)?);
    }
    let set: BTreeSet<Key> = keys.values().copied().collect();
    let mut before = BTreeMap::new();
    for name in &names {
        let receipt = stand
            .bench
            .seen(name)
            .and_then(|seen| {
                seen.events.iter().find_map(|event| match event {
                    Event::Receipt { id: None, receipt } if receipt.label == "before" => {
                        Some(receipt.clone())
                    }
                    _ => None,
                })
            })
            .ok_or_else(|| format!("нет квитанции before «{name}»"))?;
        before.insert(name.clone(), receipt);
    }
    let s0 = snap(&System, &set)?;
    let m0 = stand.receipts(&names, "m0")?;
    let s1 = snap(&System, &set)?;
    let m1 = stand.receipts(&names, "m1")?;
    let timebase = stand.timebase;
    let faults = stand.finish()?;
    if !faults.is_empty() {
        return Err(format!("канал фикстур: {}", faults.join("; ")));
    }
    let summary = crate::snapshot::summarize(Some(&s0), &s1, &set, timebase)?;

    let mut report = Report::default();
    let mut growth = Vec::new();
    for (name, mib) in MEMS {
        report.cells.push(mem_cell(
            &format!("formula-mem-{mib}"),
            entry_footprint(&summary, keys[name]),
            own_footprint(&m0[name]),
            own_footprint(&m1[name]),
        ));
        let grown = match (own_footprint(&before[name]), own_footprint(&ready[name])) {
            (Some(b), Some(r)) => (r as f64 - b as f64) / (mib * MIB) as f64,
            _ => f64::NAN,
        };
        growth.push(grown);
    }
    report.cells.push(mem_sum_cell(&summary, MEMS.len()));
    report.metrics.push(Metric::new(
        "formula-mem.growth_ratio",
        "прирост footprint / N МиБ",
        growth,
    ));
    report.raw.insert("keys".to_owned(), json(&keys));
    report.raw.insert("before".to_owned(), json(&before));
    report.raw.insert("ready".to_owned(), json(&ready));
    report.raw.insert("m0".to_owned(), json(&m0));
    report.raw.insert("m1".to_owned(), json(&m1));
    report.raw.insert("summary".to_owned(), json(&summary));
    report.notes.push(
        "прирост footprint после касания N МиБ — наблюдение без вердикта (порядок: 16, 128, 512 МиБ)"
            .to_owned(),
    );
    Ok(report)
}
