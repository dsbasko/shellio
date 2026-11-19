//! Калибровка замера кадров: связь «окно — дисплей — показанный кадр — ожидаемый шаг».
//!
//! Чистая часть: по рядам vsync дисплея и показам кадров источника в окне замера находит такты,
//! после которых новый кадр источника не показан, и сверяет их с тактами, на которых вид намеренно
//! удерживал поколение. Прямой контроль проходит, если найдены ровно удержанные такты, а без
//! удержания — ни одного. Обратный контроль: содержимое пробы замерло, сосед движется, кадры
//! соседа источнику пробы не приписаны.

use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};

use super::capture::Sample;
use super::frames::{self, Clock, GRID_TOLERANCE_NS, Grid, Lifetime, Rate, Update, Vsync};

/// Частота, на которую рассчитаны номера тактов движения, Гц.
pub const RATE_HZ: f64 = 120.0;
/// Период повторения удержаний, такты: отрезок калибровки в 5 с — ровно один период, а в долгом
/// движении удержания повторяются и попадают в запись с любым сдвигом начала.
pub const HOLD_PERIOD: u64 = 600;
/// Группы удержания периода: первый такт и длина. Записаны до первого прогона.
pub const HOLDS: [(u64, u64); 2] = [(120, 3), (300, 10)];

/// Номер такта движения по времени `targetTimestamp`, а не по числу вызовов: пропущенный вызов
/// номер следующего не сдвигает.
pub fn tick_index(start_s: f64, target_s: f64) -> u64 {
    ((target_s - start_s) * RATE_HZ).round().max(0.0) as u64
}

/// Вид удерживает поколение на такте: положение повторяется, нового кадра нет.
pub fn held(index: u64) -> bool {
    let phase = index % HOLD_PERIOD;
    HOLDS
        .iter()
        .any(|&(from, len)| (from..from + len).contains(&phase))
}

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

/// Такты прогрева отрезка до окна замера: 1 с при 120 Гц.
pub const WARMUP_TICKS: u64 = 120;
/// Окно замера отрезка, секунды после прогрева.
pub const MEASURE_S: f64 = 4.0;
/// Движение после окна замера до остановки записи, секунды: кадры последних тактов окна доходят
/// до дисплея на несколько vsync позже, их жизнь должна закрыться в записи.
pub const TAIL_S: f64 = 0.5;
/// Ожидаемый период vsync, наносекунды.
pub const PERIOD_NS: u64 = 8_333_333;
/// Запас отбора обновлений до окна замера: обновление начинается раньше показа своего кадра.
pub const UPDATE_LEAD_NS: u64 = 100_000_000;

/// Выгрузка одной записи, разобранная по таблицам.
#[derive(Clone, Debug)]
pub struct Exported {
    pub clock: Clock,
    pub vsyncs: Vec<Vsync>,
    pub updates: Vec<Update>,
    pub lifetimes: Vec<Lifetime>,
}

/// Разбор отрезка записи: окно замера в тактах сетки, кадры пробы и прямой контроль. Время —
/// время трассы, наносекунды.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub vsync_display: String,
    /// Дисплей жизней кадров пробы: только его показы ложатся на сетку окна.
    pub window_display: String,
    pub frames_displays: Vec<String>,
    pub grid: Grid,
    pub rate: Rate,
    pub window: Window,
    pub ticks: usize,
    pub first_shown_ns: u64,
    pub last_shown_ns: u64,
    pub probe_frames: Vec<u64>,
    pub forward: Forward,
}

/// Окно замера, привязанное к сетке: границы — середины между тактами, ближайшими к границам
/// окна по `targetTimestamp`. Тогда такт на границе не перескакивает в соседнее окно от дрожи.
pub fn snap(grid: &Grid, window: Window) -> Window {
    let half = grid.period_ns / 2.0;
    let edge = |at: u64| (grid.slot_ns(grid.slot(at)) as f64 - half).round() as u64;
    Window {
        start_ns: edge(window.start_ns),
        end_ns: edge(window.end_ns),
    }
}

/// Дисплей `hitches-frame-lifetimes`, на котором показаны кадры пробы `pid` за всю запись. Жизни
/// кадров других дисплеев на сетку окна не ложатся и в разбор не входят: ⚠️ ошибка исполнения
/// стенда, найдена в первом прогоне с двумя дисплеями `20261119-135920-89488`. Кадры пробы на
/// нескольких дисплеях — ошибка. Без кадров пробы дисплей окна — единственный дисплей жизней.
fn probe_display(exported: &Exported, pid: u32) -> Result<String, String> {
    let swaps: HashSet<u64> = exported
        .updates
        .iter()
        .filter(|update| update.pid == pid)
        .map(|update| update.swap_id)
        .collect();
    let displays = |probe_only: bool| {
        let mut names: Vec<&str> = exported
            .lifetimes
            .iter()
            .filter(|lifetime| !probe_only || swaps.contains(&lifetime.swap_id))
            .map(|lifetime| lifetime.display.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    };
    let probe = displays(true);
    let names = if probe.is_empty() {
        displays(false)
    } else {
        probe
    };
    match names.as_slice() {
        [only] => Ok((*only).to_owned()),
        [] => Err("в hitches-frame-lifetimes нет кадров".to_owned()),
        many => Err(format!(
            "дисплей окна не определён: кадры {} на дисплеях {many:?}",
            if swaps.is_empty() {
                "дисплеев"
            } else {
                "пробы"
            }
        )),
    }
}

/// Дисплей окна среди строк vsync: единственный или единственный, на сетку которого ложатся
/// показы кадров пробы. Имена дисплеев таблиц vsync и hitches разные, по имени не сравниваются.
fn vsync_display(vsyncs: &[Vsync], shown_ns: &[u64]) -> Result<(String, Grid), String> {
    let mut names: Vec<&str> = vsyncs.iter().map(|vsync| vsync.display.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    if let [only] = names.as_slice() {
        return Ok((only.to_string(), frames::grid(vsyncs, only)?));
    }
    let fitting: Vec<(String, Grid)> = names
        .iter()
        .filter_map(|name| {
            let grid = frames::grid(vsyncs, name).ok()?;
            grid.off_grid(shown_ns)
                .is_empty()
                .then(|| (name.to_string(), grid))
        })
        .collect();
    match <[(String, Grid); 1]>::try_from(fitting) {
        Ok([only]) => Ok(only),
        Err(fitting) => Err(format!(
            "дисплей окна среди {names:?} не определён: показам пробы подходят {} сеток",
            fitting.len()
        )),
    }
}

/// Разбор отрезка. `window` и `held_targets_ns` — время `CLOCK_UPTIME_RAW`: окно замера по
/// `targetTimestamp` и цели удержанных тактов. Ошибка — нарушение условий записи: окно не покрыто
/// жизнями кадров, показ или строка vsync вне сетки, кадр пробы без жизни.
pub fn analyze(
    exported: &Exported,
    pid: u32,
    window: Window,
    held_targets_ns: &[u64],
) -> Result<Segment, String> {
    let clock = &exported.clock;
    let to_trace = |at: u64| {
        clock
            .trace_ns(at)
            .ok_or_else(|| format!("момент {at} раньше начала трассы"))
    };
    let nominal = Window {
        start_ns: to_trace(window.start_ns)?,
        end_ns: to_trace(window.end_ns)?,
    };
    let window_display = probe_display(exported, pid)?;
    let mut shown: Vec<u64> = exported
        .lifetimes
        .iter()
        .filter(|lifetime| lifetime.display == window_display)
        .map(Lifetime::shown_ns)
        .collect();
    shown.sort_unstable();
    let (first_shown, last_shown) = match (shown.first(), shown.last()) {
        (Some(&first), Some(&last)) => (first, last),
        _ => return Err("в hitches-frame-lifetimes нет кадров".to_owned()),
    };
    let (display, grid) = vsync_display(&exported.vsyncs, &shown)?;
    let rate = frames::confirm_rate(&exported.vsyncs, &display, PERIOD_NS);
    let window = snap(&grid, nominal);
    if first_shown > window.start_ns || last_shown < window.end_ns {
        return Err(format!(
            "жизни кадров {first_shown}–{last_shown} нс не покрывают окно замера {}–{} нс",
            window.start_ns, window.end_ns
        ));
    }
    let inside: Vec<u64> = shown
        .iter()
        .copied()
        .filter(|&at| window.start_ns <= at && at < window.end_ns)
        .collect();
    let off = grid.off_grid(&inside);
    if !off.is_empty() {
        return Err(format!(
            "{} показов окна замера дальше {GRID_TOLERANCE_NS:.0} нс от сетки, первый {}",
            off.len(),
            off[0]
        ));
    }
    let lead = window.start_ns.saturating_sub(UPDATE_LEAD_NS);
    let updates: Vec<Update> = exported
        .updates
        .iter()
        .filter(|update| {
            update.pid == pid && lead <= update.start_ns && update.start_ns < window.end_ns
        })
        .cloned()
        .collect();
    let mut frames_displays: Vec<String> = exported
        .lifetimes
        .iter()
        .map(|lifetime| lifetime.display.clone())
        .collect();
    frames_displays.sort_unstable();
    frames_displays.dedup();
    let probe_frames: Vec<u64> = frames::shown_frames(&updates, &exported.lifetimes, pid)?
        .into_iter()
        .filter(|&at| window.start_ns <= at && at < window.end_ns)
        .collect();
    let ticks = grid.ticks(window.start_ns, window.end_ns);
    let held = held_targets_ns
        .iter()
        .map(|&at| to_trace(at))
        .collect::<Result<Vec<u64>, String>>()?;
    let forward = forward(&ticks, &probe_frames, window, &held);
    Ok(Segment {
        vsync_display: display,
        window_display,
        frames_displays,
        grid,
        rate,
        window,
        ticks: ticks.len(),
        first_shown_ns: first_shown,
        last_shown_ns: last_shown,
        probe_frames,
        forward,
    })
}

/// Разбор отрезка пути ScreenCaptureKit. Время — `CLOCK_UPTIME_RAW`, наносекунды.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureSegment {
    /// Сетка тактов по `targetTimestamp` вызовов `displayLink` пробы.
    pub grid: Grid,
    pub window: Window,
    pub ticks: usize,
    pub samples: usize,
    pub complete: usize,
    /// Кадры `Complete` вне окна замера, маркер которых не прочитан.
    pub unread_outside: usize,
    /// Фаза корзин [`bucket_phase`] по новым кадрам прогрева; `None` — в прогреве нет новых кадров.
    pub phase_ns: Option<f64>,
    pub phase_frames: usize,
    /// `displayTime − φ` кадров с новым маркером в окне замера.
    pub new_frames: Vec<u64>,
    /// Поколения, которые маркер перескочил между соседними новыми кадрами окна.
    pub skipped_generations: u64,
    pub max_handler_ns: u64,
    /// `targetTimestamp` окна замера дальше допуска от сетки: у `stall` — следствие задержки.
    pub off_grid_targets: usize,
    /// Знаковый остаток отклонения сдвинутых новых кадров окна от сетки: наименьший, медиана и
    /// наибольший.
    pub residual_min_ns: f64,
    pub residual_median_ns: f64,
    pub residual_max_ns: f64,
    pub frame_period_median_ns: u64,
    pub forward: Forward,
}

/// Фаза корзин тактов по показам `frames_ns`: круговое среднее их отклонения от сетки, приведённое
/// к `[0; P)`. Отклонение собрано у края ±P/2, где линейная медиана ложится между половинами
/// скопления. `None` — показов нет.
pub fn bucket_phase(grid: &Grid, frames_ns: &[u64]) -> Option<f64> {
    if frames_ns.is_empty() {
        return None;
    }
    let turn = std::f64::consts::TAU / grid.period_ns;
    let (sin, cos) = frames_ns.iter().fold((0.0, 0.0), |(sin, cos), &at| {
        let angle = grid.offset_ns(at) * turn;
        (sin + angle.sin(), cos + angle.cos())
    });
    Some((f64::atan2(sin, cos) / turn).rem_euclid(grid.period_ns))
}

/// Показ, сдвинутый на фазу корзин: кадр такта T ложится в `[T − P/2; T + P/2)`.
pub fn aligned(at_ns: u64, phase_ns: f64) -> u64 {
    at_ns.saturating_sub(phase_ns.round() as u64)
}

/// Разбор журнала потока. `targets_ns` — `targetTimestamp` вызовов `displayLink` отрезка,
/// `streamed` — от запуска до остановки потока. `displayTime` сдвигается на фазу корзин по новым
/// кадрам прогрева один раз, дальше разбор идёт по сдвинутому времени. Ошибка — нарушение условий
/// записи: поток не покрывает окно, сетка не строится, маркер кадра окна замера не прочитан.
pub fn analyze_capture(
    samples: &[Sample],
    targets_ns: &[u64],
    streamed: Window,
    window: Window,
    held_targets_ns: &[u64],
) -> Result<CaptureSegment, String> {
    let from = window.start_ns.saturating_sub(GRID_LEAD_NS);
    let rows: Vec<Vsync> = targets_ns
        .iter()
        .filter(|&&at| from <= at && at < window.start_ns)
        .map(|&at_ns| Vsync {
            at_ns,
            display: "displayLink".to_owned(),
        })
        .collect();
    let grid = frames::grid(&rows, "displayLink")?;
    let window = snap(&grid, window);
    let in_window: Vec<u64> = targets_ns
        .iter()
        .copied()
        .filter(|&at| window.start_ns <= at && at < window.end_ns)
        .collect();
    let off_grid_targets = grid.off_grid(&in_window).len();
    let mut complete: Vec<&Sample> = samples
        .iter()
        .filter(|sample| sample.status == COMPLETE)
        .collect();
    complete.sort_by_key(|sample| sample.display_ns);
    let fresh = fresh_frames(&complete);
    let lead: Vec<u64> = fresh
        .iter()
        .filter_map(|frame| frame.marker.ok().map(|_| frame.display_ns))
        .filter(|&at| from <= at && at < window.start_ns)
        .collect();
    let phase_ns = bucket_phase(&grid, &lead);
    let shift = phase_ns.unwrap_or(0.0);
    let reach = window.end_ns + shift.round() as u64;
    if streamed.start_ns > window.start_ns || streamed.end_ns < reach {
        return Err(format!(
            "поток {}–{} нс не покрывает окно замера {}–{} нс, сдвинутое на фазу {shift:.0} нс",
            streamed.start_ns, streamed.end_ns, window.start_ns, reach
        ));
    }
    let inside = |at: u64| window.start_ns <= at && at < window.end_ns;
    let mut unread_outside = 0;
    let mut new_frames = Vec::new();
    let mut skipped_generations = 0u64;
    for frame in &fresh {
        let at = aligned(frame.display_ns, shift);
        match &frame.marker {
            Err(err) if inside(at) => {
                return Err(format!("маркер кадра {} нс: {err}", frame.display_ns));
            }
            Err(_) => unread_outside += 1,
            Ok(_) if inside(at) => {
                new_frames.push(at);
                skipped_generations += frame.skipped;
            }
            Ok(_) => {}
        }
    }
    let mut residuals: Vec<f64> = new_frames.iter().map(|&at| grid.offset_ns(at)).collect();
    residuals.sort_by(f64::total_cmp);
    let ticks = grid.ticks(window.start_ns, window.end_ns);
    let forward = forward(&ticks, &new_frames, window, held_targets_ns);
    Ok(CaptureSegment {
        grid,
        window,
        ticks: ticks.len(),
        samples: samples.len(),
        complete: complete.len(),
        unread_outside,
        phase_ns,
        phase_frames: lead.len(),
        skipped_generations,
        max_handler_ns: samples
            .iter()
            .map(|sample| sample.handler_ns)
            .max()
            .unwrap_or(0),
        off_grid_targets,
        residual_min_ns: residuals.first().copied().unwrap_or(0.0),
        residual_median_ns: residuals.get(residuals.len() / 2).copied().unwrap_or(0.0),
        residual_max_ns: residuals.last().copied().unwrap_or(0.0),
        frame_period_median_ns: median_period(&new_frames),
        new_frames,
        forward,
    })
}

/// Кадр потока со сменой маркера или с непрочитанным маркером. `skipped` — поколения, которые
/// маркер перескочил после предыдущего нового кадра.
struct Fresh<'a> {
    display_ns: u64,
    marker: Result<u16, &'a str>,
    skipped: u64,
}

/// Новые кадры журнала по возрастанию `displayTime`: маркер сменился. Кадр с непрочитанным
/// маркером идёт отдельной записью и предыдущий маркер не меняет.
fn fresh_frames<'a>(complete: &[&'a Sample]) -> Vec<Fresh<'a>> {
    let mut previous: Option<u16> = None;
    let mut fresh = Vec::new();
    for sample in complete {
        match &sample.marker {
            Some(Ok(marker)) if previous != Some(*marker) => {
                let skipped = previous
                    .map(|before| u64::from(marker.wrapping_sub(before).saturating_sub(1)))
                    .unwrap_or(0);
                fresh.push(Fresh {
                    display_ns: sample.display_ns,
                    marker: Ok(*marker),
                    skipped,
                });
                previous = Some(*marker);
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => fresh.push(Fresh {
                display_ns: sample.display_ns,
                marker: Err(err),
                skipped: 0,
            }),
            None => fresh.push(Fresh {
                display_ns: sample.display_ns,
                marker: Err("кадр без маркера"),
                skipped: 0,
            }),
        }
    }
    fresh
}

/// Сетка пути ScreenCaptureKit строится по `targetTimestamp` последних 0,5 с прогрева, до окна
/// замера: на запуске потока `displayLink` один раз меняет фазу (разведка 03:08, скачок 2,7 мс на
/// 5-м такте прогрева), а задержка `stall` в окне даёт такт вне сетки (разведка 03:12). Записан до
/// первого формального прогона пути.
pub const GRID_LEAD_NS: u64 = 500_000_000;

/// `SCFrameStatus::Complete`.
pub const COMPLETE: isize = 0;

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
    fn holds_repeat_every_period() {
        let held_in = |from: u64| {
            (from..from + HOLD_PERIOD)
                .filter(|&i| held(i))
                .collect::<Vec<_>>()
        };
        let first: Vec<u64> = (120..123).chain(300..310).collect();
        assert_eq!(held_in(0), first);
        let second: Vec<u64> = first.iter().map(|i| i + HOLD_PERIOD).collect();
        assert_eq!(held_in(HOLD_PERIOD), second);
        assert!(!held(119) && held(120) && held(122) && !held(123));
        assert!(!held(299) && held(309) && !held(310));
    }

    #[test]
    fn tick_index_follows_time() {
        assert_eq!(tick_index(10.0, 10.0), 0);
        assert_eq!(tick_index(10.0, 10.0 + 120.4 / RATE_HZ), 120);
        assert_eq!(tick_index(10.0, 10.0 + 120.6 / RATE_HZ), 121);
        assert_eq!(tick_index(10.0, 9.0), 0);
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

    /// Выгрузка с vsync пачками по 10 тактов, жизнями кадров на каждом такте и обновлениями пробы
    /// `pid` на всех тактах, кроме `skip`. Время трассы — от 0, эпоха — 1 с.
    fn exported(slots: u64, skip: &[u64], pid: u32) -> Exported {
        let period = 1e9 / RATE_HZ;
        let at = |slot: u64| (100_000_000.0 + slot as f64 * period).round() as u64;
        let vsyncs = (0..slots)
            .filter(|slot| slot % 120 < 10)
            .map(|slot| Vsync {
                at_ns: at(slot),
                display: "Built-In Display 1".to_owned(),
            })
            .collect();
        let lifetimes = (0..slots)
            .map(|slot| Lifetime {
                start_ns: at(slot) - 33_333_333,
                duration_ns: 33_333_333,
                display: "Display 1".to_owned(),
                swap_id: slot,
                surface_id: 1,
            })
            .collect();
        let updates = (0..slots)
            .filter(|slot| !skip.contains(slot))
            .map(|slot| Update {
                start_ns: at(slot) - 20_000_000,
                duration_ns: 200_000,
                process: "probe".to_owned(),
                pid,
                swap_id: slot,
                surface_id: 1,
            })
            .collect();
        Exported {
            clock: Clock {
                mabs_epoch: 3_000_000_000,
                numer: 1,
                denom: 3,
            },
            vsyncs,
            updates,
            lifetimes,
        }
    }

    #[test]
    fn analyze_finds_holds_on_the_grid_ticks() {
        let period = 1e9 / RATE_HZ;
        let epoch = 1_000_000_000;
        let at = |slot: u64| epoch + (100_000_000.0 + slot as f64 * period).round() as u64;
        // Окно замера — такты 120..600, удержания по `targetTimestamp` с 120-го такта окна,
        // показ на 2 такта позже.
        let held: Vec<u64> = (240..243).chain(420..430).collect();
        let shown_late: Vec<u64> = held.iter().map(|slot| slot + 2).collect();
        let data = exported(700, &shown_late, 42);
        let window = Window {
            start_ns: at(120) + 300,
            end_ns: at(600) - 300,
        };
        let targets: Vec<u64> = held.iter().map(|&slot| at(slot)).collect();
        let segment = analyze(&data, 42, window, &targets).unwrap();
        assert_eq!(segment.vsync_display, "Built-In Display 1");
        assert_eq!(segment.frames_displays, vec!["Display 1".to_owned()]);
        assert_eq!(segment.ticks, 480);
        assert!(segment.rate.confirmed);
        assert_eq!(segment.forward.shift, 2);
        assert_eq!(segment.forward.held.len(), 13);
        assert!(segment.forward.passed(), "{:?}", segment.forward);
        assert_eq!(segment.probe_frames.len(), 480 - 13);

        // Удержание на первом такте окна: дрожь границы его не выбрасывает.
        let edge = analyze(&data, 42, window, &[at(120)]).unwrap();
        assert_eq!(edge.forward.held, vec![edge.forward.shift]);

        let clean = analyze(&exported(700, &[], 42), 42, window, &[]).unwrap();
        assert!(clean.forward.missed.is_empty());
        let other = analyze(&data, 7, window, &[]).unwrap();
        assert!(other.probe_frames.is_empty());
    }

    #[test]
    fn analyze_keeps_to_the_display_of_the_probe_frames() {
        let period = 1e9 / RATE_HZ;
        let epoch = 1_000_000_000;
        let at = |slot: u64| epoch + (100_000_000.0 + slot as f64 * period).round() as u64;
        let window = Window {
            start_ns: at(120) + 300,
            end_ns: at(600) - 300,
        };
        // Второй дисплей 60 Гц со сдвигом фазы: его жизни на сетку окна не ложатся.
        let mut two = exported(700, &[], 42);
        let external: Vec<Lifetime> = (0..350)
            .map(|slot| Lifetime {
                start_ns: 100_000_000 + slot * 16_666_667 + 3_000_000,
                duration_ns: 16_666_667,
                display: "Display 3".to_owned(),
                swap_id: 10_000 + slot,
                surface_id: 2,
            })
            .collect();
        two.lifetimes.extend(external.clone());
        let segment = analyze(&two, 42, window, &[]).unwrap();
        assert_eq!(segment.window_display, "Display 1");
        assert_eq!(
            segment.frames_displays,
            vec!["Display 1".to_owned(), "Display 3".to_owned()]
        );
        assert!(segment.forward.passed(), "{:?}", segment.forward);
        assert_eq!(segment.probe_frames.len(), 480);

        // Кадры пробы на двух дисплеях — ошибка: дисплей окна не определён.
        let mut split = two.clone();
        split.updates.push(Update {
            start_ns: external[200].start_ns,
            duration_ns: 200_000,
            process: "probe".to_owned(),
            pid: 42,
            swap_id: external[200].swap_id,
            surface_id: 2,
        });
        assert!(analyze(&split, 42, window, &[]).is_err());

        // Без кадров пробы и с жизнями двух дисплеев дисплей окна тоже не определён.
        assert!(analyze(&two, 7, window, &[]).is_err());
    }

    #[test]
    fn analyze_rejects_uncovered_or_unknown_windows() {
        let data = exported(300, &[], 42);
        let beyond = Window {
            start_ns: 1_200_000_000,
            end_ns: 1_000_000_000 + 4_000_000_000,
        };
        assert!(analyze(&data, 42, beyond, &[]).is_err());
        let before = Window {
            start_ns: 10,
            end_ns: 1_200_000_000,
        };
        assert!(analyze(&data, 42, before, &[]).is_err());
        let mut two = exported(300, &[], 42);
        let mut second = two.vsyncs.clone();
        for vsync in &mut second {
            vsync.display = "Ext".to_owned();
            vsync.at_ns += 4_000_000;
        }
        two.vsyncs.extend(second);
        let window = Window {
            start_ns: 1_200_000_000,
            end_ns: 1_300_000_000,
        };
        let segment = analyze(&two, 42, window, &[]).unwrap();
        assert_eq!(segment.vsync_display, "Built-In Display 1");
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    const PERIOD: u64 = 8_333_333;
    const START: u64 = 10_000_000_000;

    fn targets(count: u64) -> Vec<u64> {
        (0..count).map(|i| START + i * PERIOD).collect()
    }

    /// Кадр потока на каждом такте `k` с маркером `k`, кроме удержанных: там маркер прежний.
    fn stream(count: u64, held: &[u64], shift: u64) -> Vec<Sample> {
        let mut marker = 0u16;
        (0..count)
            .map(|k| {
                if !held.contains(&k) {
                    marker = marker.wrapping_add(1);
                }
                Sample {
                    status: COMPLETE,
                    display_ns: START + (k + shift) * PERIOD + 400_000,
                    entry_ns: START + (k + shift) * PERIOD + 900_000,
                    handler_ns: 20_000,
                    marker: Some(Ok(marker)),
                }
            })
            .collect()
    }

    fn windows() -> (Window, Window) {
        let streamed = Window {
            start_ns: START - PERIOD,
            end_ns: START + 700 * PERIOD,
        };
        let window = Window {
            start_ns: START + 120 * PERIOD,
            end_ns: START + 600 * PERIOD,
        };
        (streamed, window)
    }

    #[test]
    fn held_ticks_are_found_on_the_display_link_grid() {
        let (streamed, window) = windows();
        let held: Vec<u64> = (240..243).chain(420..430).collect();
        let held_targets: Vec<u64> = held.iter().map(|&k| START + k * PERIOD).collect();
        let segment = analyze_capture(
            &stream(700, &held, 2),
            &targets(700),
            streamed,
            window,
            &held_targets,
        )
        .unwrap();
        assert_eq!(segment.ticks, 480);
        assert_eq!(segment.forward.shift, 2);
        assert!(segment.forward.passed(), "{:?}", segment.forward);
        assert_eq!(segment.forward.missed.len(), 13);
        assert_eq!(segment.skipped_generations, 0);
        assert_eq!(segment.phase_frames, 60);
        assert!(
            segment
                .phase_ns
                .is_some_and(|phase| (phase - 400_000.0).abs() < 1.0)
        );
        assert!(segment.residual_min_ns.abs() < 1.0 && segment.residual_max_ns.abs() < 1.0);
        assert_eq!(segment.frame_period_median_ns, PERIOD);

        let clean =
            analyze_capture(&stream(700, &[], 1), &targets(700), streamed, window, &[]).unwrap();
        assert!(clean.forward.missed.is_empty());
        assert!(clean.forward.passed());
    }

    #[test]
    fn frozen_content_gives_no_new_frames_and_lost_frames_skip_generations() {
        let (streamed, window) = windows();
        let mut frozen = stream(700, &[], 0);
        for sample in &mut frozen {
            sample.marker = Some(Ok(7));
        }
        let segment = analyze_capture(&frozen, &targets(700), streamed, window, &[]).unwrap();
        assert!(segment.new_frames.is_empty());
        assert_eq!((segment.phase_ns, segment.phase_frames), (None, 0));
        let neighbor: Vec<u64> = targets(700);
        assert!(reverse(&segment.new_frames, &neighbor, segment.window).passed());

        let mut lost = stream(700, &[], 0);
        lost.retain(|sample| sample.display_ns != START + 300 * PERIOD + 400_000);
        let segment = analyze_capture(&lost, &targets(700), streamed, window, &[]).unwrap();
        assert_eq!(segment.skipped_generations, 1);
        assert_eq!(segment.forward.unexpected.len(), 1);
    }

    #[test]
    fn a_phase_jump_in_warmup_is_outside_the_grid() {
        let (streamed, window) = windows();
        let mut jumped = targets(700);
        for target in &mut jumped[..5] {
            *target -= 2_700_000;
        }
        let segment =
            analyze_capture(&stream(700, &[], 0), &jumped, streamed, window, &[]).unwrap();
        assert!(segment.forward.passed());
        let mut late = targets(700);
        late[200] += 2_700_000;
        let segment = analyze_capture(&stream(700, &[], 0), &late, streamed, window, &[]).unwrap();
        assert_eq!(segment.off_grid_targets, 1);
        let mut lead = targets(700);
        lead[100] += 2_700_000;
        assert!(analyze_capture(&stream(700, &[], 0), &lead, streamed, window, &[]).is_err());
    }

    #[test]
    fn broken_streams_are_errors() {
        let (streamed, window) = windows();
        let mut unread = stream(700, &[], 0);
        unread[300].marker = Some(Err("пара без перехода".to_owned()));
        assert!(analyze_capture(&unread, &targets(700), streamed, window, &[]).is_err());
        unread[300].marker = Some(Ok(301));
        unread[5].marker = Some(Err("пара без перехода".to_owned()));
        let segment = analyze_capture(&unread, &targets(700), streamed, window, &[]).unwrap();
        assert_eq!(segment.unread_outside, 1);
        let late = Window {
            start_ns: window.start_ns + PERIOD,
            end_ns: streamed.end_ns,
        };
        assert!(analyze_capture(&stream(700, &[], 0), &targets(700), late, window, &[]).is_err());
    }

    /// Кадр такта `k` показан в `k × P + P/2 − 300 мкс ± 600 мкс`: скопление у края корзины, без
    /// фазы половина кадров уходит в соседний такт.
    fn straddling(count: u64, held: &[u64], lost: &[u64]) -> Vec<Sample> {
        let mut marker = 0u16;
        (0..count)
            .filter_map(|k| {
                if !held.contains(&k) {
                    marker = marker.wrapping_add(1);
                }
                let jitter = if k % 2 == 0 { 600_000 } else { 0 };
                let at = START + k * PERIOD + PERIOD / 2 - 600_000 + jitter;
                (!lost.contains(&k)).then_some(Sample {
                    status: COMPLETE,
                    display_ns: at,
                    entry_ns: at + 500_000,
                    handler_ns: 20_000,
                    marker: Some(Ok(marker)),
                })
            })
            .collect()
    }

    #[test]
    fn bucket_phase_is_circular_across_the_half_period() {
        let grid = Grid {
            origin_ns: START,
            intercept_ns: 0.0,
            period_ns: PERIOD as f64,
            rows: 60,
            max_offset_ns: 0.0,
        };
        let edge = [
            START + 10 * PERIOD + 4_000_000,
            START + 11 * PERIOD - 4_000_000,
            START + 12 * PERIOD + 4_100_000,
            START + 13 * PERIOD - 4_100_000,
        ];
        let phase = bucket_phase(&grid, &edge).unwrap();
        assert!((phase - PERIOD as f64 / 2.0).abs() < 50_000.0, "{phase}");
        let early = [START + 10 * PERIOD - 200_000, START + 11 * PERIOD - 200_000];
        let phase = bucket_phase(&grid, &early).unwrap();
        assert!((phase - (PERIOD - 200_000) as f64).abs() < 1.0, "{phase}");
        assert_eq!(bucket_phase(&grid, &[]), None);
        assert_eq!(aligned(START + 4_000_000, 3_999_999.6), START);
    }

    #[test]
    fn frames_at_the_bucket_edge_keep_to_their_ticks() {
        let (streamed, window) = windows();
        let unphased = missed_ticks(
            &targets(700),
            &straddling(700, &[], &[])
                .iter()
                .map(|sample| sample.display_ns)
                .collect::<Vec<_>>(),
            window,
        );
        assert!(!unphased.is_empty());

        let clean = analyze_capture(
            &straddling(700, &[], &[]),
            &targets(700),
            streamed,
            window,
            &[],
        )
        .unwrap();
        assert!(clean.forward.missed.is_empty(), "{:?}", clean.forward);
        assert_eq!(clean.new_frames.len(), 480);
        let phase = clean.phase_ns.unwrap();
        assert!(
            (phase - (PERIOD / 2 - 300_000) as f64).abs() < 1_000.0,
            "{phase}"
        );
        assert!(clean.residual_min_ns >= -300_001.0 && clean.residual_max_ns <= 300_001.0);

        let held: Vec<u64> = (240..243).chain(420..430).collect();
        let held_targets: Vec<u64> = held.iter().map(|&k| START + k * PERIOD).collect();
        let holds = analyze_capture(
            &straddling(700, &held, &[]),
            &targets(700),
            streamed,
            window,
            &held_targets,
        )
        .unwrap();
        assert_eq!(holds.forward.shift, 0);
        assert_eq!(holds.forward.missed.len(), 13);
        assert!(holds.forward.passed(), "{:?}", holds.forward);

        let lost = analyze_capture(
            &straddling(700, &[], &[350]),
            &targets(700),
            streamed,
            window,
            &[],
        )
        .unwrap();
        assert_eq!(lost.forward.unexpected, vec![230]);
        assert_eq!(lost.skipped_generations, 1);
    }

    #[test]
    fn the_stream_must_cover_the_window_shifted_by_the_phase() {
        let (streamed, window) = windows();
        // Окно привязано к сетке: конец — середина перед тактом 600.
        let short = Window {
            start_ns: streamed.start_ns,
            end_ns: window.end_ns - PERIOD / 2 + 1_000_000,
        };
        assert!(
            analyze_capture(
                &straddling(700, &[], &[]),
                &targets(700),
                short,
                window,
                &[]
            )
            .is_err()
        );
        assert!(analyze_capture(&stream(700, &[], 0), &targets(700), short, window, &[]).is_ok());
    }
}

#[cfg(test)]
mod recorded {
    use super::*;

    /// Отрезок `holds` первого прогона `calibrate` (`20261119-024534-65013`): выгрузки и вход разбора.
    #[test]
    fn first_calibrate_holds_segment() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/calibrate-holds/");
        let read = |name: &str| {
            let xml = std::fs::read_to_string(format!("{dir}{name}.xml")).unwrap();
            frames::parse_table(&xml).unwrap()
        };
        let exported = Exported {
            clock: Clock::from_table(&read("time-info")).unwrap(),
            vsyncs: frames::vsyncs(&read("display-vsyncs-interval")).unwrap(),
            updates: frames::updates(&read("hitches-updates")).unwrap(),
            lifetimes: frames::lifetimes(&read("hitches-frame-lifetimes")).unwrap(),
        };
        #[derive(Deserialize)]
        struct Input {
            pid: u32,
            window: Window,
            held_targets_ns: Vec<u64>,
        }
        let input: Input = serde_json::from_str(
            &std::fs::read_to_string(format!("{dir}segment-input.json")).unwrap(),
        )
        .unwrap();
        let segment = analyze(&exported, input.pid, input.window, &input.held_targets_ns).unwrap();
        assert_eq!(segment.vsync_display, "Built-In Display 1");
        assert!(segment.rate.confirmed);
        assert_eq!(segment.ticks, 480);
        assert_eq!(segment.probe_frames.len(), 467);
        let forward = &segment.forward;
        assert_eq!(forward.shift, 2);
        let expected: Vec<usize> = (122..125).chain(302..312).collect();
        assert_eq!(forward.missed, expected);
        assert!(forward.passed(), "{forward:?}");
        assert!(
            analyze(&exported, input.pid, input.window, &[])
                .unwrap()
                .forward
                .unexpected
                .len()
                == 13
        );
    }
}

#[cfg(test)]
mod recorded_capture {
    use super::*;

    /// Отрезок ScreenCaptureKit формального прогона `20261119-140449-92675`: журнал потока, такты
    /// `displayLink` и вход разбора. Повторный разбор тех же данных, на которых найдена ошибка фазы.
    fn replay(name: &str) -> CaptureSegment {
        let dir = format!(
            "{}/testdata/calibrate-sck-140449/{name}/",
            env!("CARGO_MANIFEST_DIR")
        );
        let read = |file: &str| std::fs::read_to_string(format!("{dir}{file}")).unwrap();
        let samples: Vec<Sample> = serde_json::from_str(&read("samples.json")).unwrap();
        let ticks: Vec<serde_json::Value> = serde_json::from_str(&read("ticks.json")).unwrap();
        let targets: Vec<u64> = ticks
            .iter()
            .map(|tick| (tick["target_s"].as_f64().unwrap() * 1e9).round() as u64)
            .collect();
        #[derive(Deserialize)]
        struct Input {
            streamed: Window,
            window: Window,
            held_targets_ns: Vec<u64>,
        }
        let input: Input = serde_json::from_str(&read("segment-input.json")).unwrap();
        analyze_capture(
            &samples,
            &targets,
            input.streamed,
            input.window,
            &input.held_targets_ns,
        )
        .unwrap()
    }

    #[test]
    fn clean_has_a_new_frame_on_every_tick() {
        let segment = replay("clean");
        assert_eq!(segment.ticks, 480);
        assert_eq!(segment.new_frames.len(), 480);
        assert_eq!(segment.skipped_generations, 0);
        assert_eq!(segment.phase_frames, 60);
        let phase = segment.phase_ns.unwrap();
        assert!((3_900_000.0..3_960_000.0).contains(&phase), "{phase}");
        assert!(segment.forward.missed.is_empty(), "{:?}", segment.forward);
        assert!(segment.residual_max_ns < PERIOD_NS as f64 / 2.0);
        // Без фазы те же кадры дают 17 тактов без кадра: два кадра в одной корзине.
        let raw: Vec<u64> = segment
            .new_frames
            .iter()
            .map(|&at| at + phase.round() as u64)
            .collect();
        let ticks = segment
            .grid
            .ticks(segment.window.start_ns, segment.window.end_ns);
        assert_eq!(missed_ticks(&ticks, &raw, segment.window).len(), 17);
    }

    #[test]
    fn holds_are_found_exactly() {
        let segment = replay("holds");
        assert_eq!(segment.new_frames.len(), 467);
        let forward = &segment.forward;
        assert_eq!(forward.shift, 0);
        let expected: Vec<usize> = (120..123).chain(300..310).collect();
        assert_eq!(forward.missed, expected);
        assert!(forward.passed(), "{forward:?}");
    }
}
