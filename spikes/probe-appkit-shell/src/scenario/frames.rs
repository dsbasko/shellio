//! Сценарий `frames`: движение ведёт `displayLink` окна в общих режимах цикла. Фаза прокрутки —
//! список на 100 000 строк сдвигается на 40 точек за вызов (`scrollToPoint` у clip view и
//! `reflectScrolledClipView`) до конца. Фаза ресайза — ширина окна меняется на 8 точек за
//! вызов в пределах `visibleFrame` его экрана, до края и обратно. На каждом вызове
//! проверяется, что окно на встроенном дисплее, и записываются `timestamp`,
//! `targetTimestamp`, монотонное время входа и запрошенный диапазон частоты. Ожидаемый
//! интервал — `targetTimestamp − timestamp` того же вызова; длинный интервал больше 1,5
//! ожидаемого; смена частоты выделяется отдельно. Это наблюдение без вердикта: длинные
//! интервалы обратного вызова не приписываются пропущенным кадрам главного потока.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_core_graphics::CGDisplayIsBuiltin;
use objc2_foundation::{
    NSObject, NSObjectProtocol, NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSTimer,
};
use objc2_quartz_core::CADisplayLink;
use serde::Serialize;

use super::counters::{Live, counter};
use super::events;
use super::readback;
use super::sampler;
use super::split::Stable;
use super::startup::require_builtin;
use super::{Poll, Report, SETTLE_TIMEOUT, Scenario, Settle, Stand, Step, poll, run_steps};
use crate::fixtures;
use crate::scene::Scene;
use crate::tiles::CENTER;

const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const LIST_ROWS: usize = 100_000;
/// Сдвиг прокрутки за вызов `displayLink`, точки.
const SCROLL_STEP: f64 = 40.0;
/// Изменение ширины окна за вызов, точки.
const RESIZE_STEP: f64 = 8.0;
/// Проходов ресайза «до края и обратно».
const RESIZE_CYCLES: usize = 3;
/// Длинный интервал — больше этой доли ожидаемого.
const LONG_FACTOR: f64 = 1.5;
/// Ожидаемый интервал, изменившийся больше чем на эту долю, — смена частоты.
const RATE_CHANGE: f64 = 0.05;
/// Срок обеих фаз: около 65 000 вызовов прокрутки идут 9 минут при 120 Гц и 18 — при 60.
pub const PHASES_TIMEOUT: Duration = Duration::from_secs(1_500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Scroll,
    Resize,
}

/// Вызов `displayLink`: метки CA в секундах, вход — `CLOCK_UPTIME_RAW` в секундах (те же
/// часы, что у `CACurrentMediaTime`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Tick {
    pub phase: Phase,
    pub timestamp: f64,
    pub target: f64,
    pub entry: f64,
    pub rate: (f32, f32, f32),
    pub builtin: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RateChange {
    pub index: usize,
    pub from_ms: f64,
    pub to_ms: f64,
}

/// Интервалы между соседними вызовами одной фазы.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Analysis {
    pub calls: usize,
    pub intervals_ms: Vec<f64>,
    pub expected_ms: Vec<f64>,
    /// Интервалы без смены частоты, больше `LONG_FACTOR` ожидаемого.
    pub long: usize,
    pub long_share: f64,
    pub rate_changes: Vec<RateChange>,
    /// Вход в обратный вызов позже его `timestamp`, мс.
    pub entry_lag_ms: Vec<f64>,
}

/// Интервалы фазы. Интервал `i` — между вызовами `i − 1` и `i`, ожидаемый — `target − timestamp`
/// вызова `i`.
pub fn analyze(ticks: &[Tick]) -> Analysis {
    let mut analysis = Analysis {
        calls: ticks.len(),
        entry_lag_ms: ticks
            .iter()
            .map(|tick| (tick.entry - tick.timestamp) * 1e3)
            .collect(),
        ..Analysis::default()
    };
    let mut counted = 0usize;
    for (index, pair) in ticks.windows(2).enumerate() {
        let (previous, tick) = (pair[0], pair[1]);
        let interval = (tick.timestamp - previous.timestamp) * 1e3;
        let expected = (tick.target - tick.timestamp) * 1e3;
        let before = (previous.target - previous.timestamp) * 1e3;
        analysis.intervals_ms.push(interval);
        analysis.expected_ms.push(expected);
        if before > 0.0 && ((expected - before) / before).abs() > RATE_CHANGE {
            analysis.rate_changes.push(RateChange {
                index: index + 1,
                from_ms: before,
                to_ms: expected,
            });
            continue;
        }
        counted += 1;
        if interval > LONG_FACTOR * expected {
            analysis.long += 1;
        }
    }
    analysis.long_share = if counted > 0 {
        analysis.long as f64 / counted as f64
    } else {
        0.0
    };
    analysis
}

counter!(FRAME_DRIVER, "ProbeFrameDriver");

type TickHandler = Box<dyn FnMut(&CADisplayLink)>;

pub struct DriverIvars {
    _live: Live,
    handler: RefCell<Option<TickHandler>>,
}

define_class!(
    /// Цель `displayLink`: каждый вызов уходит обработчику сценария.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeFrameDriver"]
    #[ivars = DriverIvars]
    pub struct FrameDriver;

    unsafe impl NSObjectProtocol for FrameDriver {}

    impl FrameDriver {
        #[unsafe(method(tick:))]
        fn tick(&self, link: &CADisplayLink) {
            let handler = self.ivars().handler.borrow_mut().take();
            if let Some(mut handler) = handler {
                handler(link);
                let mut slot = self.ivars().handler.borrow_mut();
                if slot.is_none() {
                    *slot = Some(handler);
                }
            }
        }
    }
);

impl FrameDriver {
    fn new(mtm: MainThreadMarker, handler: TickHandler) -> Retained<FrameDriver> {
        let this = FrameDriver::alloc(mtm).set_ivars(DriverIvars {
            _live: FRAME_DRIVER.live(),
            handler: RefCell::new(Some(handler)),
        });
        // SAFETY: `init` у NSObject без аргументов.
        unsafe { msg_send![super(this), init] }
    }

    /// Снимает обработчик: замыкание держит сценарий, а `displayLink` — цель.
    fn clear(&self) {
        drop(self.ivars().handler.borrow_mut().take());
    }
}

/// Ход ресайза: ширина, направление и законченные проходы.
struct Resize {
    min: f64,
    max: f64,
    width: f64,
    grow: bool,
    cycles: usize,
}

struct Run {
    stand: Rc<Stand>,
    scene: Rc<Scene>,
    settle: Rc<Settle>,
    report: RefCell<Report>,
    mark: Cell<Option<Duration>>,
    stable: RefCell<Stable>,
    ticks: RefCell<Vec<Tick>>,
    phase: Cell<Phase>,
    resize: RefCell<Option<Resize>>,
    error: RefCell<Option<String>>,
    done: Cell<bool>,
    driver: RefCell<Option<Retained<FrameDriver>>>,
    link: RefCell<Option<Retained<CADisplayLink>>>,
}

impl Run {
    fn touch(&self) {
        self.mark.set(events::elapsed());
        *self.stable.borrow_mut() = Stable::new();
    }

    fn settled(&self) -> bool {
        let Some(mark) = self.mark.get() else {
            return false;
        };
        let Some(list) = self.scene.tiles.tiles[CENTER].list() else {
            return false;
        };
        events::display_pass_after(mark).is_some()
            && self.stable.borrow_mut().quiet(vec![
                readback::rect(self.scene.window.frame()),
                readback::rect(list.view().documentVisibleRect()),
            ])
    }

    fn state(&self) -> String {
        format!(
            "phase={:?} ticks={} {}",
            self.phase.get(),
            self.ticks.borrow().len(),
            events::focus_state(&self.stand.app)
        )
    }

    /// Один вызов `displayLink`: отметка, проверка дисплея и шаг движения.
    fn tick(&self, link: &CADisplayLink) {
        if self.done.get() {
            return;
        }
        let entry = sampler::uptime_ns().unwrap_or(0) as f64 / 1e9;
        let window = &self.scene.window;
        let builtin = window
            .screen()
            .is_some_and(|screen| CGDisplayIsBuiltin(screen.CGDirectDisplayID()));
        let rate = link.preferredFrameRateRange();
        self.ticks.borrow_mut().push(Tick {
            phase: self.phase.get(),
            timestamp: link.timestamp(),
            target: link.targetTimestamp(),
            entry,
            rate: (rate.minimum, rate.maximum, rate.preferred),
            builtin,
        });
        if !builtin {
            return self.stop(Some(format!(
                "окно ушло со встроенного дисплея на вызове {}",
                self.ticks.borrow().len()
            )));
        }
        let finished = match self.phase.get() {
            Phase::Scroll => {
                if self.scroll_step() {
                    self.phase.set(Phase::Resize);
                }
                false
            }
            Phase::Resize => self.resize_step(),
        };
        if finished {
            self.stop(None);
        }
    }

    /// Сдвиг прокрутки; `true`, когда список уже в конце.
    fn scroll_step(&self) -> bool {
        let Some(list) = self.scene.tiles.tiles[CENTER].list() else {
            return true;
        };
        let scroll = list.view();
        let clip = scroll.contentView();
        let bounds = clip.bounds();
        let document = scroll
            .documentView()
            .map_or(0.0, |view| view.frame().size.height);
        let end = (document - bounds.size.height).max(0.0);
        if bounds.origin.y >= end {
            return true;
        }
        let y = (bounds.origin.y + SCROLL_STEP).min(end);
        clip.scrollToPoint(NSPoint::new(bounds.origin.x, y));
        scroll.reflectScrolledClipView(&clip);
        false
    }

    /// Шаг ширины по центру `visibleFrame`; `true` после последнего прохода.
    fn resize_step(&self) -> bool {
        let window = &self.scene.window;
        let Some(screen) = window.screen() else {
            return true;
        };
        let visible = screen.visibleFrame();
        let mut slot = self.resize.borrow_mut();
        let resize = slot.get_or_insert_with(|| {
            let width = window.frame().size.width;
            Resize {
                min: width,
                max: visible.size.width.max(width),
                width,
                grow: true,
                cycles: 0,
            }
        });
        if resize.grow {
            resize.width = (resize.width + RESIZE_STEP).min(resize.max);
            if resize.width >= resize.max {
                resize.grow = false;
            }
        } else {
            resize.width = (resize.width - RESIZE_STEP).max(resize.min);
            if resize.width <= resize.min {
                resize.grow = true;
                resize.cycles += 1;
            }
        }
        let frame = window.frame();
        let x = (visible.origin.x + (visible.size.width - resize.width) / 2.0).floor();
        let rect = NSRect::new(
            NSPoint::new(x, frame.origin.y),
            objc2_foundation::NSSize::new(resize.width, frame.size.height),
        );
        let last = resize.cycles >= RESIZE_CYCLES;
        drop(slot);
        window.setFrame_display(rect, true);
        last
    }

    fn stop(&self, error: Option<String>) {
        if self.done.replace(true) {
            return;
        }
        if let Some(link) = self.link.borrow_mut().take() {
            link.invalidate();
        }
        if let Some(driver) = self.driver.borrow_mut().take() {
            driver.clear();
        }
        *self.error.borrow_mut() = error;
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Frames);
    if let Err(reason) = stand
        .check_session(&mut report)
        .and_then(|()| require_builtin(&stand))
    {
        return stand.conclude(report, Err(reason));
    }
    let Some(scene) = stand.built_scene.clone() else {
        return stand.conclude(report, Err("сцена не собрана".to_owned()));
    };
    report.push("scroll_step_points", SCROLL_STEP);
    report.push("resize_step_points", RESIZE_STEP);
    report.push("resize_cycles", RESIZE_CYCLES);
    report.push("list_rows", LIST_ROWS);
    report.push(
        "interval",
        "длинные интервалы обратного вызова displayLink: между timestamp соседних вызовов одной фазы против targetTimestamp − timestamp вызова; наблюдение без вердикта, не пропущенные кадры главного потока",
    );
    let run = Rc::new(Run {
        settle: Settle::new(vec![scene.window.clone()]),
        stand: Rc::clone(&stand),
        scene,
        report: RefCell::new(report),
        mark: Cell::new(None),
        stable: RefCell::new(Stable::new()),
        ticks: RefCell::new(Vec::new()),
        phase: Cell::new(Phase::Scroll),
        resize: RefCell::new(None),
        error: RefCell::new(None),
        done: Cell::new(false),
        driver: RefCell::new(None),
        link: RefCell::new(None),
    });
    let next = Rc::clone(&run);
    run_steps(steps(&run), move |result| match result {
        Ok(()) => drive(next),
        Err(failure) => finish(next, Err(failure.describe())),
    });
}

fn steps(run: &Rc<Run>) -> Vec<Step> {
    let first = {
        let run = Rc::clone(run);
        move || {
            if let Poll::Pending(state) = run.settle.poll() {
                return Poll::Pending(state);
            }
            let app = &run.stand.app;
            let key = app.keyWindow();
            poll(
                app.isActive() && key.is_some_and(|key| events::same(&*key, &*run.scene.window)),
                || run.state(),
            )
        }
    };
    let warp = {
        let run = Rc::clone(run);
        move || events::warp_cursor_outside(run.stand.app.mtm(), &run.stand.app).map(|_| ())
    };
    let (outside, load) = (Rc::clone(run), Rc::clone(run));
    let (screen, ready) = (Rc::clone(run), Rc::clone(run));
    vec![
        Step::new(
            "окно неподвижно, активность и key window",
            SETTLE_TIMEOUT,
            first,
            warp,
        ),
        Step::new(
            "курсор вне окон",
            STEP_TIMEOUT,
            move || {
                poll(events::cursor_outside(&outside.stand.app), || {
                    outside.state()
                })
            },
            move || {
                let list = load.scene.tiles.tiles[CENTER]
                    .list()
                    .ok_or("у центральной плитки нет списка")?;
                load.touch();
                list.set_rows(fixtures::list(LIST_ROWS));
                Ok(())
            },
        ),
        Step::new(
            "100000: первый экран",
            STEP_TIMEOUT,
            move || poll(screen.settled(), || screen.state()),
            move || {
                ready
                    .report
                    .borrow_mut()
                    .push_json("window_before", &readback::window_info(&ready.scene.window));
                Ok(())
            },
        ),
    ]
}

/// Запускает `displayLink`; дальше движение ведёт он сам, таймеров-предикатов нет. Один
/// разовый таймер ограничивает срок обеих фаз, а окончание проверяет следующий таймер того же
/// вида после остановки ссылки.
fn drive(run: Rc<Run>) {
    let mtm = run.stand.app.mtm();
    // Обработчик держит сценарий, пока `stop` не снимет его: иначе состояние освободится сразу
    // после запуска ссылки.
    let owner = Rc::clone(&run);
    let driver = FrameDriver::new(
        mtm,
        Box::new(move |link| {
            owner.tick(link);
            if owner.done.get() {
                conclude_soon(Rc::clone(&owner));
            }
        }),
    );
    // SAFETY: цель — объект пробы с методом `tick:`, принимающим `CADisplayLink`.
    let link = unsafe {
        run.scene
            .root
            .displayLinkWithTarget_selector(&driver, sel!(tick:))
    };
    // SAFETY: ссылка добавляется в цикл главного потока в общих режимах.
    unsafe { link.addToRunLoop_forMode(&NSRunLoop::currentRunLoop(), NSRunLoopCommonModes) };
    *run.driver.borrow_mut() = Some(driver);
    *run.link.borrow_mut() = Some(link);
    events::note("displayLink запущен");
    let pending = RefCell::new(Some(Rc::downgrade(&run)));
    once(PHASES_TIMEOUT, move || {
        if let Some(run) = pending.borrow_mut().take().and_then(|weak| weak.upgrade())
            && !run.done.get()
        {
            run.stop(Some(format!(
                "фазы не закончились за {} с: {}",
                PHASES_TIMEOUT.as_secs(),
                run.state()
            )));
            conclude_soon(run);
        }
    });
}

/// Итог — не из обратного вызова `displayLink`, а с разового таймера после него.
fn conclude_soon(run: Rc<Run>) {
    let pending = RefCell::new(Some(run));
    once(Duration::ZERO, move || {
        if let Some(run) = pending.borrow_mut().take() {
            let error = run.error.borrow_mut().take();
            finish(run, error.map_or(Ok(()), Err));
        }
    });
}

fn once(after: Duration, action: impl Fn() + 'static) {
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| action());
    // SAFETY: таймер добавляется в цикл главного потока, блок вызывается там же.
    let timer =
        unsafe { NSTimer::timerWithTimeInterval_repeats_block(after.as_secs_f64(), false, &block) };
    // SAFETY: `NSRunLoopCommonModes` — неизменяемая константа Foundation.
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
}

fn finish(run: Rc<Run>, result: Result<(), String>) {
    let stand = Rc::clone(&run.stand);
    run.stop(None);
    let mut report = run.report.replace(Report::default());
    run.settle.add_to(&mut report);
    let ticks = run.ticks.replace(Vec::new());
    record(&mut report, &ticks);
    report.push_json("window_after", &readback::window_info(&run.scene.window));
    stand.conclude(report, result);
}

fn record(report: &mut Report, ticks: &[Tick]) {
    report.push("ticks", ticks.len());
    if let (Some(first), Some(last)) = (ticks.first(), ticks.last()) {
        report.push(
            "ticks_duration_s",
            format!("{:.3}", last.timestamp - first.timestamp),
        );
        report.push_json("requested_rate_first", &first.rate);
        report.push_json(
            "requested_rate_changed",
            &ticks.iter().any(|tick| tick.rate != first.rate),
        );
    }
    for phase in [Phase::Scroll, Phase::Resize] {
        let name = match phase {
            Phase::Scroll => "scroll",
            Phase::Resize => "resize",
        };
        let phase_ticks: Vec<Tick> = ticks
            .iter()
            .copied()
            .filter(|tick| tick.phase == phase)
            .collect();
        let analysis = analyze(&phase_ticks);
        report.push(&format!("{name}_calls"), analysis.calls);
        report.push(&format!("{name}_long_intervals"), analysis.long);
        report.push(
            &format!("{name}_long_share"),
            format!("{:.4}", analysis.long_share),
        );
        report.push_json(&format!("{name}_rate_changes"), &analysis.rate_changes);
        report.metric(
            &format!("frames_{name}_interval"),
            "ms",
            analysis.intervals_ms,
        );
        report.metric(
            &format!("frames_{name}_expected"),
            "ms",
            analysis.expected_ms,
        );
        report.metric(
            &format!("frames_{name}_entry_lag"),
            "ms",
            analysis.entry_lag_ms,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(timestamp: f64, period: f64) -> Tick {
        Tick {
            phase: Phase::Scroll,
            timestamp,
            target: timestamp + period,
            entry: timestamp + 0.001,
            rate: (0.0, 0.0, 0.0),
            builtin: true,
        }
    }

    #[test]
    fn long_intervals_and_rate_changes() {
        let p = 1.0 / 120.0;
        let mut ticks = Vec::new();
        let mut t = 0.0;
        for _ in 0..5 {
            ticks.push(tick(t, p));
            t += p;
        }
        // Пропуск одного вызова: интервал вдвое длиннее ожидаемого.
        t += p;
        ticks.push(tick(t, p));
        // Смена частоты на 60 Гц: интервал смены не считается длинным.
        let q = 1.0 / 60.0;
        t += q;
        ticks.push(tick(t, q));
        t += q;
        ticks.push(tick(t, q));
        let analysis = analyze(&ticks);
        assert_eq!(analysis.calls, 8);
        assert_eq!(analysis.intervals_ms.len(), 7);
        assert_eq!(analysis.long, 1);
        assert_eq!(analysis.rate_changes.len(), 1);
        assert_eq!(analysis.rate_changes[0].index, 6);
        assert!((analysis.long_share - 1.0 / 6.0).abs() < 1e-9);
        assert!(
            analysis
                .entry_lag_ms
                .iter()
                .all(|lag| (lag - 1.0).abs() < 1e-6)
        );
    }

    #[test]
    fn empty_and_single() {
        assert_eq!(analyze(&[]).long_share, 0.0);
        let one = analyze(&[tick(1.0, 0.01)]);
        assert_eq!((one.calls, one.intervals_ms.len()), (1, 0));
    }
}
