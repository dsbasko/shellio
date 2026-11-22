//! Монитор цикла главного потока (ADR-0020, «Уточнено в P1.3 (планирование)»). Метрики называются
//! тем, что меряют:
//!
//! - «занятость итерации цикла» — от пробуждения цикла (`Entry`, `AfterWaiting`) до засыпания
//!   (`BeforeWaiting`) после фиксации транзакции Core Animation. AppKit выходит из цикла на каждое
//!   событие (`Exit`), обрабатывает его вне цикла и входит снова, поэтому выход итерацию не
//!   закрывает: обработка события входит в итерацию своего пробуждения;
//! - «проходы отображения» — вызовы наблюдателя с порядком после этой фиксации: засыпание и выход
//!   цикла, на обоих AppKit фиксирует транзакцию. Пробуждение с событием даёт два прохода и
//!   больше, поэтому число проходов не равно числу пробуждений: пробуждения считаются отдельно;
//! - «отрисовка поколения N» — отметка, которую компонент ставит в `drawRect:` или `updateLayer`
//!   с поколением состояния, действующим в этот момент.
//!
//! Метрики кадра на экране здесь нет. Часы — время с загрузки (`CACurrentMediaTime`), те же, что у
//! `NSEvent.timestamp`. Монитор не заводит ни таймера, ни `CADisplayLink`: в простое цикл спит.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use block2::RcBlock;
use objc2_core_foundation::{
    CFIndex, CFRetained, CFRunLoop, CFRunLoopActivity, CFRunLoopObserver, kCFRunLoopCommonModes,
};
use objc2_quartz_core::CACurrentMediaTime;

use crate::app::App;
use crate::own::ComponentId;

/// Бюджет итерации в мс: дольше — строка в stderr сборки для разработки.
pub const ITERATION_BUDGET_MS: f64 = 16.0;
/// Окно перцентиля занятости: последние итерации.
const WINDOW: usize = 1024;
/// Сколько последних отметок отрисовки хранится.
const DRAW_CAPACITY: usize = 4096;
/// Порядок наблюдателя пробуждения: раньше наблюдателей AppKit.
const ORDER_WAKE: CFIndex = -2_147_483_647;
/// Порядок наблюдателя засыпания: после фиксации транзакции Core Animation (2 000 000) и до снятия
/// пула autorelease (2 147 483 647).
const ORDER_SLEEP: CFIndex = 2_100_000;

/// Время с загрузки в секундах, часы `NSEvent.timestamp`.
pub(crate) fn uptime() -> f64 {
    CACurrentMediaTime()
}

/// Учёт итераций цикла по событиям наблюдателей. Время — секунды с загрузки.
///
/// Порядок записан в задаче 5 плана P1.3 по настоящему окну владельца: в простое пробуждений
/// нет; пробуждение с событием — `AfterWaiting`, `Exit`, обработка вне цикла, `Entry`, … `Exit`,
/// `Entry`, `BeforeWaiting` (так идёт и нажатие клавиши: `keyDown` и отрисовка между выходом и
/// повторным входом); пробуждение без события — `AfterWaiting` и `BeforeWaiting`. Поэтому
/// итерация длится от пробуждения до засыпания, а `Exit` только считает проход отображения. Вложенный цикл
/// (отслеживание меню) засыпает и просыпается внутри итерации внешнего: его `BeforeWaiting`
/// закрывает начатую итерацию, его пробуждение открывает следующую, которую закрывает засыпание
/// внешнего. Засыпание без пробуждения считается непарным и в занятость не входит.
#[derive(Debug, Default)]
pub(crate) struct LoopLog {
    awake_since: Option<f64>,
    busy_ms: VecDeque<f64>,
    wakeups: u64,
    iterations: u64,
    unpaired: u64,
    passes: u64,
    over_budget: u64,
    max_ms: f64,
}

impl LoopLog {
    /// `Entry`: цикл запущен. Внешний цикл, который запускает вложенный, уже не спит.
    pub(crate) fn entry(&mut self, at: f64) {
        self.awake_since.get_or_insert(at);
    }

    /// `AfterWaiting`: цикл проснулся.
    pub(crate) fn wake(&mut self, at: f64) {
        self.wakeups += 1;
        if self.awake_since.replace(at).is_some() {
            self.unpaired += 1;
        }
    }

    /// `BeforeWaiting`: проход отображения выполнен, цикл засыпает. Возвращает занятость в мс.
    pub(crate) fn sleep(&mut self, at: f64) -> Option<f64> {
        self.passes += 1;
        self.close(at)
    }

    /// `Exit`: проход отображения выполнен, цикл вышел. Поток не спит: AppKit обрабатывает событие
    /// и снова входит в цикл, поэтому итерация продолжается.
    pub(crate) fn exit(&mut self, at: f64) {
        self.passes += 1;
        self.awake_since.get_or_insert(at);
    }

    fn close(&mut self, at: f64) -> Option<f64> {
        let Some(since) = self.awake_since.take() else {
            self.unpaired += 1;
            return None;
        };
        let busy = ((at - since) * 1000.0).max(0.0);
        self.iterations += 1;
        self.max_ms = self.max_ms.max(busy);
        if busy > ITERATION_BUDGET_MS {
            self.over_budget += 1;
        }
        if self.busy_ms.len() == WINDOW {
            self.busy_ms.pop_front();
        }
        self.busy_ms.push_back(busy);
        Some(busy)
    }

    pub(crate) fn stats(&self) -> IterationStats {
        let window: Vec<f64> = self.busy_ms.iter().copied().collect();
        IterationStats {
            iterations: self.iterations,
            wakeups: self.wakeups,
            unpaired: self.unpaired,
            display_passes: self.passes,
            over_budget: self.over_budget,
            max_ms: self.max_ms,
            p95_ms: nearest_rank(&window, 0.95),
            window: window.len(),
        }
    }
}

/// Занятость итераций цикла и проходы отображения с установки монитора.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IterationStats {
    /// Закрытые итерации: пробуждение и засыпание или выход.
    pub iterations: u64,
    /// Пробуждения цикла (`AfterWaiting`).
    pub wakeups: u64,
    /// Засыпания без пробуждения и пробуждения без засыпания.
    pub unpaired: u64,
    pub display_passes: u64,
    /// Итерации дольше [`ITERATION_BUDGET_MS`].
    pub over_budget: u64,
    pub max_ms: f64,
    /// p95 занятости по ближайшему рангу за окно последних итераций; `None` — итераций не было.
    pub p95_ms: Option<f64>,
    /// Итераций в окне перцентиля.
    pub window: usize,
}

/// Перцентиль `q` по ближайшему рангу: значение с рангом `⌈q·n⌉`, но не меньше первого.
/// Интерполяции нет, поэтому результат — одно из значений. `None` — значений нет.
pub(crate) fn nearest_rank(samples: &[f64], q: f64) -> Option<f64> {
    let mut sorted: Vec<f64> = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (q * sorted.len() as f64).ceil() as usize;
    let index = rank.clamp(1, sorted.len().max(1)) - 1;
    sorted.get(index).copied()
}

/// Строка stderr для итерации дольше бюджета; `None` — в бюджете.
pub(crate) fn stall_line(busy_ms: f64) -> Option<String> {
    (busy_ms > ITERATION_BUDGET_MS).then(|| {
        format!(
            "[main-loop] занятость итерации цикла {busy_ms:.1} мс, бюджет {ITERATION_BUDGET_MS} мс"
        )
    })
}

/// Отметка отрисовки компонента.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawMark {
    pub component: ComponentId,
    /// Поколение состояния, действующее в момент отрисовки.
    pub generation: u64,
    /// Секунды с загрузки.
    pub at: f64,
}

/// Отметки отрисовки по порядку и текущее поколение состояния.
#[derive(Debug, Default)]
pub(crate) struct DrawLog {
    generation: u64,
    marks: VecDeque<DrawMark>,
    total: u64,
}

impl DrawLog {
    pub(crate) fn set_generation(&mut self, generation: u64) {
        self.generation = generation;
    }

    pub(crate) fn record(&mut self, component: ComponentId, at: f64) {
        if self.marks.len() == DRAW_CAPACITY {
            self.marks.pop_front();
        }
        self.marks.push_back(DrawMark {
            component,
            generation: self.generation,
            at,
        });
        self.total += 1;
    }

    /// Первая хранимая отрисовка компонента с поколением не меньше `generation`.
    pub(crate) fn first(&self, component: ComponentId, generation: u64) -> Option<DrawMark> {
        self.marks
            .iter()
            .find(|mark| mark.component == component && mark.generation >= generation)
            .copied()
    }
}

/// Чтение монитора для `ui.stats`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoopStats {
    pub iterations: IterationStats,
    /// Все вызовы `drawRect:` и `updateLayer` компонентов.
    pub draws: u64,
    pub generation: u64,
}

#[derive(Default)]
struct LoopState {
    log: LoopLog,
    draws: DrawLog,
    installed: bool,
}

thread_local! {
    /// Состояние монитора; его трогают только главный поток и его наблюдатели.
    static LOOP: RefCell<LoopState> = RefCell::new(LoopState::default());
}

/// Работа с состоянием монитора. `None` — состояние занято (повторный вход из наблюдателя) или
/// уже уничтожено деструкторами потока при выходе процесса.
fn with_state<R>(f: impl FnOnce(&mut LoopState) -> R) -> Option<R> {
    LOOP.try_with(|state| state.try_borrow_mut().ok().map(|mut state| f(&mut state)))
        .ok()
        .flatten()
}

/// Отметка отрисовки компонента: вызывается из его `drawRect:` или `updateLayer`. Отметки
/// пишутся и без установленного монитора: это одно прибавление.
pub(crate) fn mark_draw(component: ComponentId) {
    let at = uptime();
    with_state(|state| state.draws.record(component, at));
}

/// Наблюдатели цикла главного потока. Один на процесс; снимаются в `Drop`.
pub struct LoopMonitor {
    observers: Vec<CFRetained<CFRunLoopObserver>>,
    /// Монитор живёт на главном потоке.
    _main: PhantomData<*const ()>,
}

impl LoopMonitor {
    /// Ставит наблюдатели на главный цикл в общих режимах, включая отслеживание меню.
    pub fn install(_app: &App) -> Result<LoopMonitor, MonitorError> {
        if with_state(|state| std::mem::replace(&mut state.installed, true)) != Some(false) {
            return Err(MonitorError::AlreadyInstalled);
        }
        let installed = install_observers();
        if installed.is_err() {
            with_state(|state| state.installed = false);
        }
        installed.map(|observers| LoopMonitor {
            observers,
            _main: PhantomData,
        })
    }

    pub fn stats(&self) -> LoopStats {
        with_state(|state| LoopStats {
            iterations: state.log.stats(),
            draws: state.draws.total,
            generation: state.draws.generation,
        })
        .unwrap_or(LoopStats {
            iterations: LoopLog::default().stats(),
            draws: 0,
            generation: 0,
        })
    }

    /// Поколение состояния, которым помечаются следующие отрисовки.
    pub fn set_generation(&self, generation: u64) {
        with_state(|state| state.draws.set_generation(generation));
    }

    /// Первая хранимая отрисовка компонента с поколением не меньше `generation`.
    pub fn first_draw(&self, component: ComponentId, generation: u64) -> Option<DrawMark> {
        with_state(|state| state.draws.first(component, generation)).flatten()
    }
}

impl Drop for LoopMonitor {
    fn drop(&mut self) {
        for observer in &self.observers {
            observer.invalidate();
        }
        with_state(|state| state.installed = false);
    }
}

fn install_observers() -> Result<Vec<CFRetained<CFRunLoopObserver>>, MonitorError> {
    let run_loop = CFRunLoop::main().ok_or(MonitorError::NoMainRunLoop)?;
    let wake = observer(
        CFRunLoopActivity::Entry.0 | CFRunLoopActivity::AfterWaiting.0,
        ORDER_WAKE,
        |activity, at| {
            with_state(|state| {
                if activity == CFRunLoopActivity::Entry {
                    state.log.entry(at);
                } else {
                    state.log.wake(at);
                }
            });
        },
    )?;
    let sleep = observer(
        CFRunLoopActivity::BeforeWaiting.0 | CFRunLoopActivity::Exit.0,
        ORDER_SLEEP,
        |activity, at| {
            let busy = with_state(|state| {
                if activity == CFRunLoopActivity::Exit {
                    state.log.exit(at);
                    None
                } else {
                    state.log.sleep(at)
                }
            })
            .flatten();
            if cfg!(debug_assertions)
                && let Some(line) = busy.and_then(stall_line)
            {
                eprintln!("{line}");
            }
        },
    );
    let sleep = match sleep {
        Ok(sleep) => sleep,
        Err(error) => {
            wake.invalidate();
            return Err(error);
        }
    };
    for observer in [&wake, &sleep] {
        // SAFETY: `kCFRunLoopCommonModes` — неизменяемая константа Core Foundation.
        run_loop.add_observer(Some(observer), unsafe { kCFRunLoopCommonModes });
    }
    Ok(vec![wake, sleep])
}

fn observer(
    activities: usize,
    order: CFIndex,
    on: impl Fn(CFRunLoopActivity, f64) + 'static,
) -> Result<CFRetained<CFRunLoopObserver>, MonitorError> {
    let block = RcBlock::new(
        move |_observer: *mut CFRunLoopObserver, activity: CFRunLoopActivity| {
            on(activity, uptime())
        },
    );
    // SAFETY: распределитель по умолчанию; блок жив на время вызова, наблюдатель копирует его и
    // вызывает на потоке своего цикла, то есть на главном.
    unsafe { CFRunLoopObserver::with_handler(None, activities, true, order, Some(&block)) }
        .ok_or(MonitorError::ObserverRefused)
}

/// Отказ установки монитора.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorError {
    AlreadyInstalled,
    NoMainRunLoop,
    ObserverRefused,
}

impl fmt::Display for MonitorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AlreadyInstalled => "монитор цикла уже установлен",
            Self::NoMainRunLoop => "нет главного цикла CFRunLoop",
            Self::ObserverRefused => "CFRunLoopObserverCreateWithHandler вернул NULL",
        })
    }
}

impl Error for MonitorError {}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: f64 = 0.001;

    #[test]
    fn wake_and_sleep_measure_busy_iteration() {
        let mut log = LoopLog::default();
        log.wake(10.0);
        assert_eq!(log.sleep(10.0 + 4.0 * MS).map(round), Some(4.0));
        log.wake(11.0);
        assert_eq!(log.sleep(11.0 + 20.0 * MS).map(round), Some(20.0));
        let stats = log.stats();
        assert_eq!(stats.iterations, 2);
        assert_eq!(stats.wakeups, 2);
        assert_eq!(stats.display_passes, 2);
        assert_eq!(stats.over_budget, 1);
        assert_eq!(round(stats.max_ms), 20.0);
        assert_eq!(stats.unpaired, 0);
    }

    #[test]
    fn nested_loop_sleeps_inside_outer_iteration() {
        let mut log = LoopLog::default();
        // Внешний цикл проснулся и в обработчике запустил вложенный (отслеживание меню).
        log.wake(1.0);
        log.entry(1.0 + 2.0 * MS);
        assert_eq!(log.sleep(1.0 + 3.0 * MS).map(round), Some(3.0));
        log.wake(1.5);
        // Вложенный вышел, внешний продолжает ту же итерацию до своего засыпания.
        log.exit(1.5 + 1.0 * MS);
        assert_eq!(log.sleep(1.5 + 6.0 * MS).map(round), Some(6.0));
        let stats = log.stats();
        assert_eq!(stats.iterations, 2);
        assert_eq!(stats.display_passes, 3);
        assert_eq!(stats.unpaired, 0);
    }

    /// Порядок одного нажатия клавиши, записанный в задаче 5 плана P1.3: AppKit выходит из цикла,
    /// обрабатывает событие вне его и входит снова. Нажатие — одна итерация от пробуждения до
    /// засыпания, выходы — проходы отображения.
    #[test]
    fn key_press_with_exit_and_reentry_is_one_iteration() {
        let mut log = LoopLog::default();
        let at = |ms: f64| 10.0 + ms * MS;
        log.wake(at(0.0));
        log.exit(at(0.3));
        // keyDown вне цикла, затем повторные входы с отрисовкой.
        log.entry(at(62.0));
        log.exit(at(62.5));
        log.entry(at(62.6));
        log.exit(at(62.7));
        log.entry(at(62.8));
        assert_eq!(log.sleep(at(62.9)).map(round), Some(62.9));
        let stats = log.stats();
        assert_eq!(stats.iterations, 1);
        assert_eq!(stats.wakeups, 1);
        assert_eq!(stats.display_passes, 4);
        assert_eq!(stats.unpaired, 0);
        assert_eq!(stats.over_budget, 1);
        assert_eq!(stats.p95_ms.map(round), Some(62.9));
    }

    #[test]
    fn idle_wakeup_without_exit_is_one_iteration() {
        let mut log = LoopLog::default();
        log.wake(5.0);
        assert_eq!(log.sleep(5.0 + 1.2 * MS).map(round), Some(1.2));
        let stats = log.stats();
        assert_eq!((stats.iterations, stats.display_passes), (1, 1));
    }

    #[test]
    fn exit_without_wakeup_starts_the_iteration() {
        let mut log = LoopLog::default();
        log.exit(3.0);
        assert_eq!(log.sleep(3.0 + 2.0 * MS).map(round), Some(2.0));
        assert_eq!(log.stats().unpaired, 0);
    }

    #[test]
    fn unpaired_events_are_counted_not_measured() {
        let mut log = LoopLog::default();
        assert_eq!(log.sleep(5.0), None, "засыпание до первого пробуждения");
        log.wake(6.0);
        log.wake(7.0);
        assert_eq!(log.sleep(7.0 + 2.0 * MS).map(round), Some(2.0));
        let stats = log.stats();
        assert_eq!(stats.unpaired, 2);
        assert_eq!(stats.iterations, 1);
        assert_eq!(stats.wakeups, 2);
        assert_eq!(stats.display_passes, 2);
    }

    #[test]
    fn first_entry_starts_iteration_without_wakeup() {
        let mut log = LoopLog::default();
        log.entry(0.5);
        log.entry(0.6);
        assert_eq!(log.sleep(0.5 + 7.0 * MS).map(round), Some(7.0));
        assert_eq!(log.stats().wakeups, 0);
    }

    #[test]
    fn clock_going_back_never_gives_negative_busy() {
        let mut log = LoopLog::default();
        log.wake(2.0);
        assert_eq!(log.sleep(1.0), Some(0.0));
    }

    #[test]
    fn percentile_window_keeps_last_iterations() {
        let mut log = LoopLog::default();
        for index in 0..(WINDOW + 100) {
            let at = index as f64;
            log.wake(at);
            let busy = if index < 100 { 50.0 } else { 1.0 };
            log.sleep(at + busy * MS);
        }
        let stats = log.stats();
        assert_eq!(stats.window, WINDOW);
        assert_eq!(
            stats.p95_ms.map(round),
            Some(1.0),
            "ранние 50 мс вышли из окна"
        );
        assert_eq!(round(stats.max_ms), 50.0, "максимум — за всё время");
        assert_eq!(stats.over_budget, 100);
        assert_eq!(stats.iterations, (WINDOW + 100) as u64);
    }

    #[test]
    fn nearest_rank_percentiles() {
        assert_eq!(nearest_rank(&[], 0.95), None);
        assert_eq!(nearest_rank(&[7.5], 0.95), Some(7.5));
        let hundred: Vec<f64> = (1..=100).rev().map(f64::from).collect();
        assert_eq!(nearest_rank(&hundred, 0.95), Some(95.0));
        assert_eq!(nearest_rank(&hundred, 0.50), Some(50.0));
        let twenty: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(nearest_rank(&twenty, 0.95), Some(19.0));
        assert_eq!(nearest_rank(&[10.0, 20.0], 0.95), Some(20.0));
        assert_eq!(nearest_rank(&[4.0, 1.0, 3.0, 2.0], 0.0), Some(1.0));
    }

    #[test]
    fn stall_line_names_metric_and_budget() {
        assert_eq!(stall_line(16.0), None);
        let line = stall_line(23.44).unwrap();
        assert!(line.starts_with("[main-loop] "), "{line}");
        assert!(line.contains("занятость итерации цикла 23.4 мс"), "{line}");
        assert!(line.contains("бюджет 16 мс"), "{line}");
    }

    #[test]
    fn draw_marks_carry_current_generation() {
        let list = ComponentId::next();
        let field = ComponentId::next();
        let mut draws = DrawLog::default();
        draws.set_generation(4);
        draws.record(list, 1.0);
        draws.set_generation(5);
        draws.record(field, 1.1);
        draws.record(list, 1.2);
        assert_eq!(draws.total, 3);
        assert_eq!(
            draws.first(list, 5),
            Some(DrawMark {
                component: list,
                generation: 5,
                at: 1.2
            })
        );
        assert_eq!(draws.first(list, 4).map(|mark| mark.generation), Some(4));
        assert_eq!(draws.first(field, 4).map(|mark| mark.generation), Some(5));
        assert_eq!(draws.first(field, 6), None);
    }

    #[test]
    fn draw_log_keeps_last_marks_and_counts_all() {
        let id = ComponentId::next();
        let mut draws = DrawLog::default();
        for index in 0..(DRAW_CAPACITY + 10) {
            draws.set_generation(index as u64);
            draws.record(id, index as f64);
        }
        assert_eq!(draws.total, (DRAW_CAPACITY + 10) as u64);
        assert_eq!(draws.marks.len(), DRAW_CAPACITY);
        assert_eq!(draws.first(id, 0).map(|mark| mark.generation), Some(10));
    }

    #[test]
    fn monitor_errors_name_the_cause() {
        assert!(
            MonitorError::AlreadyInstalled
                .to_string()
                .contains("уже установлен")
        );
        assert!(
            MonitorError::NoMainRunLoop
                .to_string()
                .contains("CFRunLoop")
        );
        assert!(MonitorError::ObserverRefused.to_string().contains("NULL"));
    }

    fn round(ms: f64) -> f64 {
        (ms * 1000.0).round() / 1000.0
    }
}
