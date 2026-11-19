//! Чистая часть сценария `frames`: стимулы S1, S2 и S3 раздела «Кадры» плана, ритм `displayLink`,
//! время `drawRect:` по тактам и подсчёт потерянных кадров. Время — `CLOCK_UPTIME_RAW`, секунды у
//! меток CA и наносекунды у рядов трассы.

use std::collections::BTreeSet;

use serde::Serialize;

use super::calibrate::{self, Window};

/// Скорость S1, точки в секунду.
pub const SPEED: f64 = 4800.0;
/// Длительность отрезка S1 по `targetTimestamp`, секунды.
pub const SEGMENT_S: f64 = 10.0;
/// Прогрев отрезка S1 вне окна замера, секунды.
pub const WARMUP_S: f64 = 1.0;
/// Наименьшее число тактов vsync в окне замера: меньше — ошибка исполнения отрезка.
pub const MIN_TICKS: usize = 1000;
/// Период такта 120 Гц, секунды.
pub const PERIOD_S: f64 = 1.0 / 120.0;
/// Пропуск вызова `displayLink`: интервал `targetTimestamp` больше полутора периодов.
pub const SKIP_FACTOR: f64 = 1.5;

/// Отрезок S1: откуда и куда движется верхний край clip view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Sweep {
    /// От начала вниз.
    TopDown,
    /// Из середины вниз.
    MiddleDown,
    /// От конца вверх.
    BottomUp,
}

impl Sweep {
    pub const ALL: [Sweep; 3] = [Sweep::TopDown, Sweep::MiddleDown, Sweep::BottomUp];

    pub fn name(self) -> &'static str {
        match self {
            Sweep::TopDown => "top-down",
            Sweep::MiddleDown => "middle-down",
            Sweep::BottomUp => "bottom-up",
        }
    }

    /// Начальный верхний край при пределе прокрутки `limit`.
    pub fn origin(self, limit: f64) -> f64 {
        match self {
            Sweep::TopDown => 0.0,
            Sweep::MiddleDown => (limit / 2.0).round(),
            Sweep::BottomUp => limit,
        }
    }

    fn sign(self) -> f64 {
        match self {
            Sweep::TopDown | Sweep::MiddleDown => 1.0,
            Sweep::BottomUp => -1.0,
        }
    }
}

/// Путь отрезка S1, точки.
pub const SWEEP_PATH: f64 = SPEED * SEGMENT_S;

/// Документ вмещает каждый отрезок S1 без упора в край: иначе положение перестаёт быть функцией
/// времени, и отрезок — ошибка исполнения.
pub fn fits(limit: f64) -> Result<(), String> {
    if limit / 2.0 >= SWEEP_PATH {
        Ok(())
    } else {
        Err(format!(
            "предел прокрутки {limit:.0} точек: половины не хватает на путь отрезка S1 {SWEEP_PATH:.0} точек"
        ))
    }
}

/// Верхний край S1 для `targetTimestamp`: `y0 ± скорость × (target − start)`, без накопления шагов.
pub fn s1_y(sweep: Sweep, limit: f64, start_s: f64, target_s: f64) -> f64 {
    let y = sweep.origin(limit) + sweep.sign() * SPEED * (target_s - start_s);
    y.clamp(0.0, limit)
}

/// Отрезок S1 закончился: `targetTimestamp` дошёл до конца отрезка.
pub fn s1_done(start_s: f64, target_s: f64) -> bool {
    target_s - start_s >= SEGMENT_S
}

fn ns(seconds: f64) -> u64 {
    (seconds * 1e9).round() as u64
}

/// Окно замера отрезка по `targetTimestamp` первого такта: после прогрева до конца отрезка.
pub fn measure_window(start_s: f64) -> Window {
    Window {
        start_ns: ns(start_s + WARMUP_S),
        end_ns: ns(start_s + SEGMENT_S),
    }
}

/// Ритм `displayLink` отрезка, как в P0.2: интервалы `targetTimestamp` и пропуски вызовов.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Rhythm {
    pub calls: usize,
    pub interval_median_ms: f64,
    pub interval_max_ms: f64,
    /// Интервалы больше [`SKIP_FACTOR`] периодов: пропущенный вызов виден скачком положения.
    pub skips: usize,
    /// Такты, которые пропуски накрыли, по номинальному периоду.
    pub skipped_ticks: u64,
}

pub fn rhythm(targets_s: &[f64]) -> Rhythm {
    let mut intervals: Vec<f64> = targets_s.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let skipped: Vec<f64> = intervals
        .iter()
        .copied()
        .filter(|&gap| gap > SKIP_FACTOR * PERIOD_S)
        .collect();
    intervals.sort_by(f64::total_cmp);
    Rhythm {
        calls: targets_s.len(),
        interval_median_ms: intervals.get(intervals.len() / 2).copied().unwrap_or(0.0) * 1e3,
        interval_max_ms: intervals.last().copied().unwrap_or(0.0) * 1e3,
        skips: skipped.len(),
        skipped_ticks: skipped
            .iter()
            .map(|gap| ((gap / PERIOD_S).round() as u64).saturating_sub(1))
            .sum(),
    }
}

/// Время `drawRect:` по тактам: сумма длительностей отметок, конец которых лежит между входом в
/// вызов `displayLink` и входом в следующий. `entries_ns` по возрастанию, отметка — начало и конец.
pub fn draw_per_tick(entries_ns: &[u64], marks: &[(u64, u64)]) -> Vec<f64> {
    let mut sums = vec![0.0; entries_ns.len()];
    for &(start, end) in marks {
        let index = entries_ns.partition_point(|&entry| entry <= end);
        if index == 0 {
            continue;
        }
        sums[index - 1] += end.saturating_sub(start) as f64 / 1e6;
    }
    sums
}

/// Потери отрезка S1. Такт — vsync окна замера. Потерян такт, после которого нет нового кадра
/// окна пробы (`missed`), или такт показа кадра, на `targetTimestamp` которого видимые строки
/// не были дорисованы (`undrawn`): кадр показан, но не соответствует положению S1 целиком.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Losses {
    pub ticks: usize,
    pub shift: usize,
    pub missed: Vec<usize>,
    pub undrawn: Vec<usize>,
    pub lost: usize,
}

/// Подсчёт потерь по тактам vsync и показам кадров окна пробы. Показы пути ScreenCaptureKit
/// приходят уже сдвинутыми на фазу корзин ([`calibrate::aligned`]). `undrawn_targets_ns` —
/// `targetTimestamp` тактов, видимые строки которых к следующему вызову не покрыты. Кадр такта T
/// показан на такте T + `shift`, где `shift` — сдвиг `k` из итога калибровки: недорисованный
/// такт T и такт T + k без нового кадра — одна потеря. Меньше [`MIN_TICKS`] тактов в окне —
/// ошибка исполнения.
pub fn count_losses(
    vsyncs_ns: &[u64],
    frames_ns: &[u64],
    window: Window,
    undrawn_targets_ns: &[u64],
    shift: usize,
) -> Result<Losses, String> {
    let ticks = vsyncs_ns
        .iter()
        .filter(|&&at| window.start_ns <= at && at < window.end_ns)
        .count();
    if ticks < MIN_TICKS {
        return Err(format!(
            "в окне замера {ticks} тактов vsync, нужно не меньше {MIN_TICKS}"
        ));
    }
    let missed = calibrate::missed_ticks(vsyncs_ns, frames_ns, window);
    let undrawn: BTreeSet<usize> = undrawn_targets_ns
        .iter()
        .filter_map(|&at| calibrate::tick_of(vsyncs_ns, window, at))
        .map(|tick| tick + shift)
        .filter(|&tick| tick < ticks)
        .collect();
    let lost = missed
        .iter()
        .copied()
        .collect::<BTreeSet<usize>>()
        .union(&undrawn)
        .count();
    Ok(Losses {
        ticks,
        shift,
        missed,
        undrawn: undrawn.into_iter().collect(),
        lost,
    })
}

/// Фаза события колеса: `NSEventPhase` жеста или инерции.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum WheelPhase {
    None,
    Began,
    Changed,
    Ended,
}

/// Событие броска S2: время от начала, фазы жеста и инерции, дельта в точках к концу документа.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct WheelStep {
    pub at_s: f64,
    pub phase: WheelPhase,
    pub momentum: WheelPhase,
    pub delta: f64,
}

/// Начальная дельта броска, точки.
pub const FLING_START: f64 = 200.0;
/// Затухание дельты за событие.
pub const FLING_DECAY: f64 = 0.95;
/// Событий фазы жеста с дельтой: `began` и `changed`, затем `ended` без дельты и инерция.
pub const GESTURE_EVENTS: usize = 8;

/// Траектория S2: дельты `200 × 0,95^i` до последней не меньше 1 точки, по событию на такт.
/// Первые [`GESTURE_EVENTS`] — жест (`began`, затем `changed`), затем `ended` жеста без дельты,
/// инерция `began`, `changed` и `ended` без дельты.
pub fn fling() -> Vec<WheelStep> {
    let deltas: Vec<f64> = (0..)
        .map(|index| FLING_START * FLING_DECAY.powi(index))
        .take_while(|&delta| delta >= 1.0)
        .collect();
    let mut steps = Vec::with_capacity(deltas.len() + 2);
    let mut push = |phase, momentum, delta| {
        steps.push(WheelStep {
            at_s: steps.len() as f64 * PERIOD_S,
            phase,
            momentum,
            delta,
        });
    };
    for (index, &delta) in deltas.iter().enumerate() {
        if index < GESTURE_EVENTS {
            let phase = if index == 0 {
                WheelPhase::Began
            } else {
                WheelPhase::Changed
            };
            push(phase, WheelPhase::None, delta);
            continue;
        }
        if index == GESTURE_EVENTS {
            push(WheelPhase::Ended, WheelPhase::None, 0.0);
        }
        let momentum = if index == GESTURE_EVENTS {
            WheelPhase::Began
        } else {
            WheelPhase::Changed
        };
        push(WheelPhase::None, momentum, delta);
    }
    push(WheelPhase::None, WheelPhase::Ended, 0.0);
    steps
}

/// Шаг ширины окна S3 за вызов `displayLink`, точки.
pub const RESIZE_STEP: f64 = 8.0;
/// Длительность S3 по `targetTimestamp`, секунды.
pub const RESIZE_S: f64 = 5.0;

/// Ширина содержимого окна S3 на такте `tick`: треугольник от `max` вниз до `min` и обратно с
/// шагом [`RESIZE_STEP`].
pub fn resize_width(tick: u64, max: f64, min: f64) -> f64 {
    let steps = ((max - min) / RESIZE_STEP).floor().max(1.0) as u64;
    let phase = tick % (2 * steps);
    let down = if phase <= steps {
        phase
    } else {
        2 * steps - phase
    };
    max - down as f64 * RESIZE_STEP
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s1_is_a_function_of_target_time() {
        let limit = 1_000_000.0;
        assert_eq!(s1_y(Sweep::TopDown, limit, 5.0, 5.5), 2400.0);
        assert_eq!(s1_y(Sweep::MiddleDown, limit, 5.0, 5.0), 500_000.0);
        assert_eq!(s1_y(Sweep::BottomUp, limit, 5.0, 6.0), limit - SPEED);
        // Пропущенный вызов — скачок, а не замедление.
        let jump = s1_y(Sweep::TopDown, limit, 0.0, 3.0 * PERIOD_S)
            - s1_y(Sweep::TopDown, limit, 0.0, PERIOD_S);
        assert!((jump - 2.0 * SPEED * PERIOD_S).abs() < 1e-9);
        assert_eq!(s1_y(Sweep::BottomUp, 100.0, 0.0, 1.0), 0.0);
        assert!(s1_done(1.0, 11.0) && !s1_done(1.0, 10.99));
    }

    #[test]
    fn short_documents_do_not_fit_s1() {
        assert!(fits(2.0 * SWEEP_PATH).is_ok());
        assert!(fits(2.0 * SWEEP_PATH - 1.0).is_err());
    }

    #[test]
    fn measure_window_skips_the_warmup() {
        let window = measure_window(2.0);
        assert_eq!(window.start_ns, 3_000_000_000);
        assert_eq!(window.end_ns, 12_000_000_000);
    }

    #[test]
    fn rhythm_counts_skipped_calls() {
        let mut targets: Vec<f64> = (0..10).map(|index| index as f64 * PERIOD_S).collect();
        targets.push(12.0 * PERIOD_S);
        let rhythm = rhythm(&targets);
        assert_eq!(rhythm.calls, 11);
        assert_eq!(rhythm.skips, 1);
        assert_eq!(rhythm.skipped_ticks, 2);
        assert!((rhythm.interval_median_ms - PERIOD_S * 1e3).abs() < 1e-9);
        assert!((rhythm.interval_max_ms - 3.0 * PERIOD_S * 1e3).abs() < 1e-9);
    }

    #[test]
    fn draw_time_goes_to_the_tick_before_the_mark_end() {
        let entries = [5_000_000, 15_000_000, 25_000_000];
        let marks = [
            (6_000_000, 7_000_000),
            (8_000_000, 8_500_000),
            (16_000_000, 18_000_000),
            (1_000_000, 2_000_000),
        ];
        let sums = draw_per_tick(&entries, &marks);
        assert_eq!(sums.len(), 3);
        assert!((sums[0] - 1.5).abs() < 1e-9);
        assert!((sums[1] - 2.0).abs() < 1e-9);
        assert_eq!(sums[2], 0.0);
    }

    const PERIOD_NS: u64 = 8_333_333;

    fn vsyncs(count: u64) -> Vec<u64> {
        (0..count)
            .map(|slot| 1_000_000_000 + slot * PERIOD_NS)
            .collect()
    }

    fn window(count: u64) -> Window {
        Window {
            start_ns: 1_000_000_000 - PERIOD_NS / 2,
            end_ns: 1_000_000_000 + count * PERIOD_NS - PERIOD_NS / 2,
        }
    }

    #[test]
    fn losses_on_synthetic_vsync_and_surface_rows() {
        let ticks = vsyncs(1100);
        // Кадр показан на каждом vsync, кроме тактов 10, 11 и 500; показ с дрожью 1 мс.
        let frames: Vec<u64> = ticks
            .iter()
            .enumerate()
            .filter(|(index, _)| ![10, 11, 500].contains(index))
            .map(|(_, &at)| at + 1_000_000)
            .collect();
        let losses = count_losses(&ticks, &frames, window(1100), &[], 0).unwrap();
        assert_eq!(losses.ticks, 1100);
        assert_eq!(losses.missed, vec![10, 11, 500]);
        assert!(losses.undrawn.is_empty());
        assert_eq!(losses.lost, 3);

        let clean = count_losses(&ticks, &ticks, window(1100), &[], 0).unwrap();
        assert_eq!(clean.lost, 0);
    }

    #[test]
    fn undrawn_frames_are_lost_once() {
        let ticks = vsyncs(1100);
        let frames: Vec<u64> = ticks
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != 40)
            .map(|(_, &at)| at)
            .collect();
        // Недорисованы такты 40 (он же без кадра) и 700, цель — с дрожью 2 мс до такта.
        let undrawn = [ticks[40] - 2_000_000, ticks[700] - 2_000_000];
        let losses = count_losses(&ticks, &frames, window(1100), &undrawn, 0).unwrap();
        assert_eq!(losses.missed, vec![40]);
        assert_eq!(losses.undrawn, vec![40, 700]);
        assert_eq!(losses.lost, 2);
    }

    #[test]
    fn an_undrawn_tick_and_its_shifted_missing_frame_are_one_loss() {
        let ticks = vsyncs(1100);
        // Кадр такта T показан на T + 2: кадра такта 40 нет, на такте 42 нового показа нет.
        let frames: Vec<u64> = ticks
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != 42)
            .map(|(_, &at)| at)
            .collect();
        let undrawn = [ticks[40] - 2_000_000, ticks[700], ticks[1099]];
        let losses = count_losses(&ticks, &frames, window(1100), &undrawn, 2).unwrap();
        assert_eq!(losses.missed, vec![42]);
        assert_eq!(losses.undrawn, vec![42, 702]);
        assert_eq!(losses.lost, 2);
        let unshifted = count_losses(&ticks, &frames, window(1100), &undrawn, 0).unwrap();
        assert_eq!(unshifted.lost, 4);
    }

    #[test]
    fn edge_frames_are_counted_after_the_bucket_phase() {
        let ticks = vsyncs(1100);
        // Показы пути ScreenCaptureKit у края корзины: такт + P/2 − 600 мкс или + P/2.
        let shown: Vec<u64> = ticks
            .iter()
            .enumerate()
            .map(|(index, &at)| at + PERIOD_NS / 2 - 600_000 + (index as u64 % 2) * 600_000)
            .collect();
        assert!(
            !count_losses(&ticks, &shown, window(1100), &[], 0)
                .unwrap()
                .missed
                .is_empty()
        );
        let grid = super::super::frames::Grid {
            origin_ns: ticks[0],
            intercept_ns: 0.0,
            period_ns: PERIOD_NS as f64,
            rows: 60,
            max_offset_ns: 0.0,
        };
        let phase = calibrate::bucket_phase(&grid, &shown[..60]).unwrap();
        let frames: Vec<u64> = shown
            .iter()
            .map(|&at| calibrate::aligned(at, phase))
            .collect();
        assert_eq!(
            count_losses(&ticks, &frames, window(1100), &[], 0)
                .unwrap()
                .lost,
            0
        );
    }

    #[test]
    fn too_few_ticks_are_an_execution_error() {
        let ticks = vsyncs(999);
        assert!(count_losses(&ticks, &ticks, window(999), &[], 0).is_err());
        // Такты вне окна не считаются.
        let ticks = vsyncs(1200);
        assert!(count_losses(&ticks, &ticks, window(900), &[], 0).is_err());
    }

    #[test]
    fn fling_follows_the_decay_with_phases() {
        let steps = fling();
        let deltas: Vec<f64> = steps
            .iter()
            .map(|step| step.delta)
            .filter(|&delta| delta > 0.0)
            .collect();
        assert_eq!(deltas.len(), 104);
        assert_eq!(deltas[0], FLING_START);
        assert!(
            deltas
                .last()
                .is_some_and(|&last| (1.0..1.05).contains(&last))
        );
        assert!(
            deltas
                .windows(2)
                .all(|pair| (pair[1] / pair[0] - FLING_DECAY).abs() < 1e-12)
        );
        assert_eq!(steps.len(), 106);
        assert_eq!(steps[0].phase, WheelPhase::Began);
        assert!(
            steps[1..GESTURE_EVENTS]
                .iter()
                .all(|step| step.phase == WheelPhase::Changed)
        );
        assert_eq!(
            (steps[GESTURE_EVENTS].phase, steps[GESTURE_EVENTS].delta),
            (WheelPhase::Ended, 0.0)
        );
        assert_eq!(steps[GESTURE_EVENTS + 1].momentum, WheelPhase::Began);
        let last = steps[steps.len() - 1];
        assert_eq!((last.momentum, last.delta), (WheelPhase::Ended, 0.0));
        assert!(
            steps
                .iter()
                .all(|step| step.phase == WheelPhase::None || step.momentum == WheelPhase::None)
        );
        // Около 0,87 с, по событию на такт.
        assert!((last.at_s - 105.0 * PERIOD_S).abs() < 1e-12);
        assert!((0.86..0.89).contains(&last.at_s));
    }

    #[test]
    fn resize_is_a_triangle_between_the_widths() {
        let widths: Vec<f64> = (0..32)
            .map(|tick| resize_width(tick, 1180.0, 1060.0))
            .collect();
        assert_eq!(widths[0], 1180.0);
        assert_eq!(widths[1], 1172.0);
        assert_eq!(widths[15], 1060.0);
        assert_eq!(widths[16], 1068.0);
        assert_eq!(widths[30], 1180.0);
        assert!(
            widths
                .windows(2)
                .all(|pair| (pair[1] - pair[0]).abs() == RESIZE_STEP)
        );
        assert!(
            widths
                .iter()
                .all(|&width| (1060.0..=1180.0).contains(&width))
        );
    }
}
