//! Калибровка замера кадров: связь «окно — дисплей — показанный кадр — ожидаемый шаг».
//!
//! Чистая часть: по рядам vsync дисплея и показам кадров источника в окне замера находит такты,
//! после которых новый кадр источника не показан, и сверяет их с тактами, на которых вид намеренно
//! удерживал поколение. Прямой контроль проходит, если найдены ровно удержанные такты, а без
//! удержания — ни одного. Обратный контроль: содержимое пробы замерло, сосед движется, кадры
//! соседа источнику пробы не приписаны.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

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
    let mut shown: Vec<u64> = exported.lifetimes.iter().map(Lifetime::shown_ns).collect();
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
