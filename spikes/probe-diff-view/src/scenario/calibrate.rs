//! Калибровка замера кадров: связь «окно — дисплей — показанный кадр — ожидаемый шаг».
//!
//! Чистая часть: по рядам vsync дисплея и показам кадров источника в окне замера находит такты,
//! после которых новый кадр источника не показан, и сверяет их с тактами, на которых вид намеренно
//! удерживал поколение. Прямой контроль проходит, если найдены ровно удержанные такты, а без
//! удержания — ни одного. Обратный контроль: содержимое пробы замерло, сосед движется, кадры
//! соседа источнику пробы не приписаны.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Окно замера, время `CLOCK_UPTIME_RAW`, наносекунды, `[start; end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub start_ns: u64,
    pub end_ns: u64,
}

/// Такты vsync окна замера, после которых нет нового кадра источника: номер такта — индекс в
/// отсортированных vsync окна. Кадр относится к такту, если его показ начался не раньше
/// `vsync − период/2` и раньше `vsync + период/2`.
pub fn missed_ticks(vsyncs_ns: &[u64], frames_ns: &[u64], window: Window) -> Vec<usize> {
    let mut ticks: Vec<u64> = vsyncs_ns
        .iter()
        .copied()
        .filter(|&at| window.start_ns <= at && at < window.end_ns)
        .collect();
    ticks.sort_unstable();
    let mut frames: Vec<u64> = frames_ns.to_vec();
    frames.sort_unstable();
    let period = median_period(&ticks);
    let half = period / 2;
    ticks
        .iter()
        .enumerate()
        .filter(|&(_, &tick)| {
            let from = tick.saturating_sub(half);
            let to = tick + half.max(1);
            let first = frames.partition_point(|&frame| frame < from);
            frames.get(first).is_none_or(|&frame| frame >= to)
        })
        .map(|(index, _)| index)
        .collect()
}

fn median_period(ticks: &[u64]) -> u64 {
    let mut periods: Vec<u64> = ticks.windows(2).map(|pair| pair[1] - pair[0]).collect();
    periods.sort_unstable();
    periods.get(periods.len() / 2).copied().unwrap_or(0)
}

/// Такт окна замера, для которого вид получил `targetTimestamp`: ближайший vsync не раньше
/// `target − период/2`.
pub fn tick_of(vsyncs_ns: &[u64], window: Window, target_ns: u64) -> Option<usize> {
    let mut ticks: Vec<u64> = vsyncs_ns
        .iter()
        .copied()
        .filter(|&at| window.start_ns <= at && at < window.end_ns)
        .collect();
    ticks.sort_unstable();
    let half = median_period(&ticks) / 2;
    let index = ticks.partition_point(|&tick| tick + half < target_ns);
    (index < ticks.len()).then_some(index)
}

/// Наибольший сдвиг показа против такта `targetTimestamp`, такты: кадр для такта T доходит до
/// дисплея на целое число vsync позже. Записан до первого прогона.
pub const MAX_SHIFT: usize = 3;

/// Итог прямого контроля одного отрезка.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    pub ticks: usize,
    /// Сдвиг `k` удержанных тактов к найденным, один для всех групп удержания.
    pub shift: usize,
    pub held: Vec<usize>,
    pub missed: Vec<usize>,
    pub unexpected: Vec<usize>,
    pub undetected: Vec<usize>,
}

impl Forward {
    pub fn passed(&self) -> bool {
        self.ticks > 0 && self.unexpected.is_empty() && self.undetected.is_empty()
    }
}

/// Прямой контроль: удержанные такты, сдвинутые на `k` от 0 до [`MAX_SHIFT`], против найденных.
/// Берётся `k` с наименьшим числом расхождений, при равенстве — меньший.
pub fn forward(
    vsyncs_ns: &[u64],
    frames_ns: &[u64],
    window: Window,
    held_targets_ns: &[u64],
) -> Forward {
    (0..=MAX_SHIFT)
        .map(|shift| forward_at(vsyncs_ns, frames_ns, window, held_targets_ns, shift))
        .min_by_key(|result| {
            (
                result.unexpected.len() + result.undetected.len(),
                result.shift,
            )
        })
        .unwrap_or_else(|| forward_at(vsyncs_ns, frames_ns, window, held_targets_ns, 0))
}

fn forward_at(
    vsyncs_ns: &[u64],
    frames_ns: &[u64],
    window: Window,
    held_targets_ns: &[u64],
    shift: usize,
) -> Forward {
    let ticks = vsyncs_ns
        .iter()
        .filter(|&&at| window.start_ns <= at && at < window.end_ns)
        .count();
    let held: BTreeSet<usize> = held_targets_ns
        .iter()
        .filter_map(|&target| tick_of(vsyncs_ns, window, target))
        .map(|tick| tick + shift)
        .collect();
    let missed: BTreeSet<usize> = missed_ticks(vsyncs_ns, frames_ns, window)
        .into_iter()
        .collect();
    Forward {
        ticks,
        shift,
        unexpected: missed.difference(&held).copied().collect(),
        undetected: held.difference(&missed).copied().collect(),
        held: held.into_iter().collect(),
        missed: missed.into_iter().collect(),
    }
}

/// Итог обратного контроля: кадры источника пробы в окне, где её содержимое замерло.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reverse {
    pub probe_frames: usize,
    pub neighbor_frames: usize,
}

impl Reverse {
    /// Сосед показал кадры, а пробе ни один не приписан.
    pub fn passed(&self) -> bool {
        self.probe_frames == 0 && self.neighbor_frames > 0
    }
}

pub fn reverse(probe_frames_ns: &[u64], neighbor_frames_ns: &[u64], window: Window) -> Reverse {
    let inside = |frames: &[u64]| {
        frames
            .iter()
            .filter(|&&at| window.start_ns <= at && at < window.end_ns)
            .count()
    };
    Reverse {
        probe_frames: inside(probe_frames_ns),
        neighbor_frames: inside(neighbor_frames_ns),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: u64 = 8_333_333;

    fn vsyncs(count: u64) -> Vec<u64> {
        (0..count).map(|i| 1_000_000 + i * PERIOD).collect()
    }

    /// Кадр на каждом такте со сдвигом в пределах полупериода, кроме тактов из `skip`.
    fn frames(vsyncs: &[u64], skip: &[usize]) -> Vec<u64> {
        vsyncs
            .iter()
            .enumerate()
            .filter(|(index, _)| !skip.contains(index))
            .map(|(index, &at)| {
                if index % 2 == 0 {
                    at + 1_000
                } else {
                    at - 1_000
                }
            })
            .collect()
    }

    fn whole(vsyncs: &[u64]) -> Window {
        Window {
            start_ns: vsyncs[0],
            end_ns: vsyncs[vsyncs.len() - 1] + 1,
        }
    }

    #[test]
    fn finds_exactly_the_held_ticks() {
        let vsyncs = vsyncs(200);
        let held: Vec<usize> = (40..43).chain(100..110).collect();
        let shown = frames(&vsyncs, &held);
        let targets: Vec<u64> = held.iter().map(|&i| vsyncs[i] + 200).collect();
        let result = forward(&vsyncs, &shown, whole(&vsyncs), &targets);
        assert_eq!(result.ticks, 200);
        assert_eq!(result.missed, held);
        assert!(result.passed(), "{result:?}");

        let late: Vec<usize> = held.iter().map(|&i| i + 2).collect();
        let shifted = forward(&vsyncs, &frames(&vsyncs, &late), whole(&vsyncs), &targets);
        assert_eq!(shifted.shift, 2);
        assert!(shifted.passed(), "{shifted:?}");
        let beyond: Vec<usize> = held.iter().map(|&i| i + MAX_SHIFT + 1).collect();
        assert!(!forward(&vsyncs, &frames(&vsyncs, &beyond), whole(&vsyncs), &targets).passed());
        let split: Vec<usize> = (41..44).chain(100..110).collect();
        assert!(!forward(&vsyncs, &frames(&vsyncs, &split), whole(&vsyncs), &targets).passed());

        let clean = forward(&vsyncs, &frames(&vsyncs, &[]), whole(&vsyncs), &[]);
        assert!(clean.missed.is_empty());
        assert!(clean.passed());
    }

    #[test]
    fn extra_or_missing_losses_fail() {
        let vsyncs = vsyncs(100);
        let targets = [vsyncs[10] + 100];
        let extra = forward(
            &vsyncs,
            &frames(&vsyncs, &[10, 50]),
            whole(&vsyncs),
            &targets,
        );
        assert_eq!(extra.unexpected, vec![50]);
        assert!(!extra.passed());
        let hidden = forward(&vsyncs, &frames(&vsyncs, &[]), whole(&vsyncs), &targets);
        assert_eq!(hidden.undetected, vec![10]);
        assert!(!hidden.passed());
        assert!(!forward(&[], &[], whole(&vsyncs), &[]).passed());
    }

    #[test]
    fn window_bounds_ticks() {
        let vsyncs = vsyncs(100);
        let window = Window {
            start_ns: vsyncs[20],
            end_ns: vsyncs[30],
        };
        assert_eq!(
            missed_ticks(&vsyncs, &frames(&vsyncs, &[25]), window),
            vec![5]
        );
        assert_eq!(tick_of(&vsyncs, window, vsyncs[25] - PERIOD / 3), Some(5));
        assert_eq!(tick_of(&vsyncs, window, vsyncs[40]), None);
    }

    #[test]
    fn reverse_control() {
        let window = Window {
            start_ns: 100,
            end_ns: 200,
        };
        assert!(reverse(&[50, 250], &[120, 150], window).passed());
        assert!(!reverse(&[150], &[120], window).passed());
        assert!(!reverse(&[], &[], window).passed());
    }
}
