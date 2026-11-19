//! Чистая часть сценария `memory`, бюджет 150 МБ = 150 000 000 байт: фазы одного процесса,
//! `ri_phys_footprint` по выборкам и `ri_lifetime_max_phys_footprint` для пика между ними.
//! Отчёт, раздел «Память, бюджет 150 МБ».
//!
//! Пик между выборками засчитывается фазе, только если `ri_lifetime_max_phys_footprint` вырос
//! внутри её окна: это поле — максимум за жизнь процесса, и выше него ни одна фаза не поднималась.
//! Если внутри окна поле не выросло, пик фазы — максимум её выборок. Поэтому сверка контрольным
//! выделением идёт до всех фаз, а её 64 МиБ и генерация фикстур вне окон фаз дают полу пика:
//! пик фазы ниже пола между выборками не виден, виден только максимум выборок. Пол фазы — поле
//! в начале её окна, его поднимают и пики прежних фаз. Рост поля внутри окна засчитывается фазе
//! при любом поле. Фаза до закрытия с полом не ниже бюджета и без роста поля слепа к пику между
//! выборками: её пик — максимум выборок, а пик между выборками — `not_run`.

use serde::Serialize;

use super::sampler::Sample;
use crate::results::Verdict;

/// Бюджет памяти GUI без терминалов, байты.
pub const BUDGET_BYTES: u64 = 150_000_000;
/// Окно выборок фазы после её действия и покрытия, наносекунды.
pub const PHASE_NS: u64 = 1_000_000_000;

/// Фаза сценария в порядке прохода.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Phase {
    Empty,
    AddedUnified,
    AddedBottom,
    AddedSide,
    Threads,
    WrapResize,
    Closed,
}

impl Phase {
    pub const ALL: [Phase; 7] = [
        Phase::Empty,
        Phase::AddedUnified,
        Phase::AddedBottom,
        Phase::AddedSide,
        Phase::Threads,
        Phase::WrapResize,
        Phase::Closed,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Phase::Empty => "empty",
            Phase::AddedUnified => "added-unified",
            Phase::AddedBottom => "added-bottom",
            Phase::AddedSide => "added-side",
            Phase::Threads => "threads-expanded",
            Phase::WrapResize => "wrap-s3",
            Phase::Closed => "closed",
        }
    }

    /// Входит ли фаза в вердикт: всё до закрытия diff.
    pub fn judged(self) -> bool {
        match self {
            Phase::Empty
            | Phase::AddedUnified
            | Phase::AddedBottom
            | Phase::AddedSide
            | Phase::Threads
            | Phase::WrapResize => true,
            Phase::Closed => false,
        }
    }
}

/// Итог фазы.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PhaseSummary {
    pub phase: Phase,
    pub samples: usize,
    pub window_ns: u64,
    pub footprint_first: u64,
    pub footprint_max: u64,
    pub footprint_last: u64,
    pub lifetime_first: u64,
    pub lifetime_last: u64,
    /// Пик между выборками: `ri_lifetime_max_phys_footprint` в конце окна, если он вырос внутри
    /// окна и сверка поля прошла.
    pub peak_between: Option<u64>,
    /// Пик фазы: больший из максимума выборок и пика между выборками.
    pub peak: u64,
}

/// Итог фазы по её выборкам. Ошибка исполнения: меньше двух выборок или часы назад.
pub fn summarize(
    phase: Phase,
    samples: &[Sample],
    peak_checked: bool,
) -> Result<PhaseSummary, String> {
    let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
        return Err(format!("фаза {}: нет выборок", phase.name()));
    };
    if samples.len() < 2 {
        return Err(format!("фаза {}: одна выборка", phase.name()));
    }
    if samples
        .windows(2)
        .any(|pair| pair[1].mono_ns < pair[0].mono_ns)
    {
        return Err(format!("фаза {}: выборки не по времени", phase.name()));
    }
    let footprint_max = samples
        .iter()
        .map(|sample| sample.footprint_bytes)
        .max()
        .unwrap_or(0);
    let peak_between = (peak_checked && last.lifetime_max_bytes > first.lifetime_max_bytes)
        .then_some(last.lifetime_max_bytes);
    Ok(PhaseSummary {
        phase,
        samples: samples.len(),
        window_ns: last.mono_ns - first.mono_ns,
        footprint_first: first.footprint_bytes,
        footprint_max,
        footprint_last: last.footprint_bytes,
        lifetime_first: first.lifetime_max_bytes,
        lifetime_last: last.lifetime_max_bytes,
        peak_between,
        peak: footprint_max.max(peak_between.unwrap_or(0)),
    })
}

/// Фазы до закрытия, слепые к пику между выборками: поле в начале окна не ниже бюджета и внутри
/// окна не выросло, поэтому пик от бюджета до пола не виден.
pub fn blind(phases: &[PhaseSummary]) -> Vec<Phase> {
    phases
        .iter()
        .filter(|summary| {
            summary.phase.judged()
                && summary.lifetime_first >= BUDGET_BYTES
                && summary.lifetime_last <= summary.lifetime_first
        })
        .map(|summary| summary.phase)
        .collect()
}

/// Вердикт: пик каждой фазы до закрытия не больше 150 000 000 байт. Ошибка исполнения: нет итога
/// какой-либо фазы до закрытия или фазы не по порядку.
pub fn judge(phases: &[PhaseSummary]) -> Result<(Verdict, String), String> {
    let order: Vec<Phase> = phases.iter().map(|summary| summary.phase).collect();
    let expected: Vec<Phase> = Phase::ALL
        .iter()
        .copied()
        .filter(|phase| phase.judged())
        .collect();
    let judged: Vec<Phase> = order
        .iter()
        .copied()
        .filter(|phase| phase.judged())
        .collect();
    if judged != expected {
        return Err(format!("фазы {order:?} вместо {:?}", Phase::ALL));
    }
    let Some(worst) = phases
        .iter()
        .filter(|summary| summary.phase.judged())
        .max_by_key(|summary| summary.peak)
    else {
        return Err("нет фаз до закрытия".to_owned());
    };
    let detail = format!(
        "наибольший пик {} байт в фазе {}{}",
        worst.peak,
        worst.phase.name(),
        if worst.peak_between == Some(worst.peak) {
            " (между выборками)"
        } else {
            ""
        }
    );
    Ok(if worst.peak <= BUDGET_BYTES {
        (Verdict::Pass, format!("{detail}, не больше 150 000 000"))
    } else {
        (Verdict::Fail, format!("{detail}, больше 150 000 000"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64, footprint: u64, lifetime: u64) -> Sample {
        Sample {
            mono_ns: ms * 1_000_000,
            cpu_ns: 0,
            footprint_bytes: footprint,
            lifetime_max_bytes: lifetime,
        }
    }

    fn phase(phase: Phase, peak: u64) -> PhaseSummary {
        summarize(
            phase,
            &[at(0, peak, 200_000_000), at(5, peak, 200_000_000)],
            true,
        )
        .unwrap()
    }

    #[test]
    fn peak_between_samples_counts_only_when_the_lifetime_max_grew_inside_the_window() {
        let grown = [at(0, 90, 120), at(5, 100, 120), at(10, 95, 140)];
        let summary = summarize(Phase::AddedUnified, &grown, true).unwrap();
        assert_eq!(summary.footprint_max, 100);
        assert_eq!(summary.peak_between, Some(140));
        assert_eq!(summary.peak, 140);
        assert_eq!(summary.window_ns, 10_000_000);

        let floor = [at(0, 90, 200), at(5, 100, 200)];
        let summary = summarize(Phase::AddedUnified, &floor, true).unwrap();
        assert_eq!(summary.peak_between, None);
        assert_eq!(summary.peak, 100);

        let unchecked = summarize(Phase::AddedUnified, &grown, false).unwrap();
        assert_eq!(unchecked.peak_between, None);
        assert_eq!(unchecked.peak, 100);
    }

    #[test]
    fn peak_grown_inside_a_window_survives_a_floor_above_the_budget() {
        const MB: u64 = 1_000_000;
        let window = |footprint: u64, first: u64, last: u64| {
            vec![
                at(0, footprint * MB, first * MB),
                at(5, footprint * MB, first * MB),
                at(10, footprint * MB, last * MB),
            ]
        };
        let samples: Vec<(Phase, Vec<Sample>)> = Phase::ALL
            .iter()
            .map(|&phase| match phase {
                Phase::Empty => (phase, window(20, 90, 90)),
                Phase::AddedUnified => (phase, window(120, 100, 160)),
                _ => (phase, window(130, 160, 160)),
            })
            .collect();
        let phases: Vec<PhaseSummary> = samples
            .iter()
            .map(|(phase, samples)| summarize(*phase, samples, true).unwrap())
            .collect();
        let (verdict, reason) = judge(&phases).unwrap();
        assert_eq!(verdict, Verdict::Fail);
        assert!(
            reason.contains("added-unified (между выборками)"),
            "{reason}"
        );
        assert_eq!(
            blind(&phases),
            vec![
                Phase::AddedBottom,
                Phase::AddedSide,
                Phase::Threads,
                Phase::WrapResize
            ]
        );
        assert!(blind(&phases[..2]).is_empty());
    }

    #[test]
    fn broken_windows_are_execution_errors() {
        assert!(summarize(Phase::Empty, &[], true).is_err());
        assert!(summarize(Phase::Empty, &[at(0, 1, 1)], true).is_err());
        assert!(summarize(Phase::Empty, &[at(5, 1, 1), at(0, 1, 1)], true).is_err());
    }

    #[test]
    fn verdict_takes_the_worst_phase_before_close() {
        let mut phases: Vec<PhaseSummary> =
            Phase::ALL.iter().map(|&p| phase(p, 100_000_000)).collect();
        assert_eq!(judge(&phases).unwrap().0, Verdict::Pass);
        phases[6] = phase(Phase::Closed, 400_000_000);
        assert_eq!(judge(&phases).unwrap().0, Verdict::Pass);
        phases[4] = phase(Phase::Threads, BUDGET_BYTES + 1);
        let (verdict, reason) = judge(&phases).unwrap();
        assert_eq!(verdict, Verdict::Fail);
        assert!(reason.contains("threads-expanded"), "{reason}");
        phases[4] = phase(Phase::Threads, BUDGET_BYTES);
        assert_eq!(judge(&phases).unwrap().0, Verdict::Pass);
        phases.remove(2);
        assert!(judge(&phases).is_err());
    }
}
