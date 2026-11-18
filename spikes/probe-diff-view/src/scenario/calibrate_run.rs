//! GUI-часть сценария `calibrate`: содержимое окна — [`MotionView`], его полосы движутся по S1 от
//! `displayLink` окна. Вызов `displayLink` ставит сдвиг для своего `targetTimestamp`, а не
//! прибавляет шаг. Чистая часть калибровки — `calibrate.rs`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::NSWindow;
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes};
use objc2_quartz_core::CADisplayLink;
use serde::Serialize;

use super::drawmark::{self, Mark};
use super::sampler;
use super::stand::{FrameDriver, MotionView, s1_offset};
use crate::results::{Report, Verdict};

/// Скорость S1, точки в секунду.
pub const SPEED: f64 = 4800.0;
/// Длительность движения пробного отрезка, секунды.
pub const MOTION_S: f64 = 40.0;

/// Ошибка исполнения калибровки с тем, что успело попасть в итог.
pub type Failed = Box<(String, Report)>;

/// Вызов `displayLink`: метки CA в секундах (часы `CLOCK_UPTIME_RAW`), вход — наносекунды.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Tick {
    pub timestamp_s: f64,
    pub target_s: f64,
    pub entry_ns: u64,
    pub generation: u64,
}

#[derive(Default)]
struct State {
    start_s: Cell<Option<f64>>,
    ticks: RefCell<Vec<Tick>>,
    done: Cell<bool>,
}

/// Идущая калибровка: вид, цель и ссылка `displayLink` живут до [`Calibration::poll`] с итогом.
pub struct Calibration {
    view: Retained<MotionView>,
    driver: Retained<FrameDriver>,
    link: Retained<CADisplayLink>,
    state: Rc<State>,
    report: RefCell<Option<Report>>,
}

impl Calibration {
    /// Ставит вид содержимым окна и запускает `displayLink`. `report` — итог готового окна.
    pub fn start(
        mtm: MainThreadMarker,
        window: &NSWindow,
        mut report: Report,
    ) -> Result<Calibration, Failed> {
        match sampler::check_peak() {
            Ok(check) => {
                match &check.failure {
                    None => report.finding(
                        "peak_counter",
                        Verdict::Pass,
                        format!(
                            "ri_lifetime_max_phys_footprint вырос на {} байт при выделении {}",
                            check.touched.lifetime_max_bytes - check.before.lifetime_max_bytes,
                            check.control_bytes
                        ),
                    ),
                    Some(reason) => report.finding("peak_counter", Verdict::NotRun, reason.clone()),
                }
                report.push(
                    "peak_check",
                    serde_json::to_string(&check).unwrap_or_default(),
                );
            }
            Err(reason) => return Err(Box::new((reason, report))),
        }
        let bounds = window
            .contentView()
            .map(|view| view.bounds())
            .unwrap_or_else(|| window.contentLayoutRect());
        let view = MotionView::new(mtm, bounds);
        window.setContentView(Some(&view));
        let state = Rc::new(State::default());
        drawmark::take();
        let handler_state = Rc::clone(&state);
        let handler_view = view.clone();
        let driver = FrameDriver::new(
            mtm,
            Box::new(move |link| {
                let entry_ns = sampler::uptime_ns().unwrap_or(0);
                let target = link.targetTimestamp();
                let start = handler_state.start_s.get().unwrap_or(target);
                handler_state.start_s.set(Some(start));
                let generation = handler_view.move_to(s1_offset(start, target, SPEED));
                handler_state.ticks.borrow_mut().push(Tick {
                    timestamp_s: link.timestamp(),
                    target_s: target,
                    entry_ns,
                    generation,
                });
                if target - start >= MOTION_S {
                    handler_state.done.set(true);
                    link.setPaused(true);
                }
            }),
        );
        // SAFETY: цель — объект пробы с методом `tick:`, принимающим `CADisplayLink`.
        let link = unsafe { view.displayLinkWithTarget_selector(&driver, sel!(tick:)) };
        // SAFETY: ссылка добавляется в цикл главного потока в общих режимах.
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::currentRunLoop(), NSRunLoopCommonModes) };
        Ok(Calibration {
            view,
            driver,
            link,
            state,
            report: RefCell::new(Some(report)),
        })
    }

    /// Проверка с таймера стенда. Итог — когда движение закончилось.
    pub fn poll(&self) -> Option<Result<Report, Failed>> {
        if !self.state.done.get() {
            return None;
        }
        self.link.invalidate();
        self.driver.clear();
        let mut report = self.report.borrow_mut().take()?;
        let ticks = self.state.ticks.take();
        let marks = drawmark::take();
        record(&mut report, &ticks, &marks, self.view.generation());
        Some(Ok(report))
    }
}

fn record(report: &mut Report, ticks: &[Tick], marks: &[Mark], generation: u64) {
    report.push("ticks", ticks.len());
    report.push("marks", marks.len());
    report.push("generations", generation);
    if let (Some(first), Some(last)) = (ticks.first(), ticks.last()) {
        report.push(
            "ticks_span_s",
            format!("{:.3}", last.target_s - first.target_s),
        );
        report.push("first_target_s", format!("{:.6}", first.target_s));
    }
    let intervals: Vec<f64> = ticks
        .windows(2)
        .map(|pair| (pair[1].target_s - pair[0].target_s) * 1e3)
        .collect();
    report.metric("tick_target_interval_ms", "ms", intervals);
    let draws: Vec<f64> = marks
        .iter()
        .map(|mark| mark.draw_end_ns.saturating_sub(mark.draw_start_ns) as f64 / 1e6)
        .collect();
    report.metric("draw_ms", "ms", draws);
}
