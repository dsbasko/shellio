//! Живая часть `smoke`: первый потребитель стенда событий, отметки отрисовки и снимка окна.
//! Содержимое окна — [`InputView`], first responder окна. Клавиша `j` и жест мыши с Shift,
//! двойным щелчком и протяжкой идут через фабрики стенда (`events.rs`). `keyDown:` и
//! `mouseDown:` начинают новое поколение, и стенд ждёт его отметку `drawmark` через [`Waiter`] со
//! сроком из отчёта. Затем снимок окна в рабочем потоке, итог забирает опрос главного потока.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSColor, NSEvent, NSEventModifierFlags, NSRectFill, NSResponder, NSView,
    NSWindow,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use serde::Serialize;

use super::calibrate_run::Failed;
use super::counters::{Live, counter};
use super::drawmark::{self, Change, Covered, Mark, Waiter};
use super::events::{self, Key};
use super::sampler;
use super::snapshot::{self, Background, Snapshot, SnapshotStatus};
use super::watchdog::SharedTrace;
use crate::results::{Report, Verdict};

/// Срок ожидания отметки отрисовки нужного поколения, отчёт «Сроки и размеры».
pub const MARK_DEADLINE: Duration = Duration::from_secs(2);
/// Срок снимка окна `smoke`, отчёт «Сроки и размеры».
pub const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(10);
/// Число щелчков жеста мыши.
pub const CLICKS: isize = 2;
/// Сдвиг протяжки от центра вида, точки.
const DRAG: NSPoint = NSPoint::new(24.0, -12.0);
/// Ширина полосы, которая смещается с поколением.
const BAR_W: f64 = 40.0;

counter!(INPUT_VIEW, "ProbeDiffInputView");

/// Событие, которое дошло до вида.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Arrival {
    pub kind: &'static str,
    pub key_code: Option<u16>,
    /// Независимые от устройства модификаторы.
    pub flags: usize,
    pub clicks: isize,
    pub generation: u64,
    pub at_ns: u64,
}

pub struct InputIvars {
    _live: Live,
    generation: Cell<u64>,
    arrivals: RefCell<Vec<Arrival>>,
}

define_class!(
    /// Вид, который принимает клавиши и мышь. Нажатие клавиши и кнопки — новое поколение,
    /// `drawRect:` отмечает его в конце прохода.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeDiffInputView"]
    #[ivars = InputIvars]
    pub struct InputView;

    impl InputView {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            let start = sampler::uptime_ns().unwrap_or(0);
            let bounds = self.bounds();
            let generation = self.ivars().generation.get();
            NSColor::textBackgroundColor().setFill();
            NSRectFill(dirty);
            NSColor::textColor().setFill();
            let slots = (bounds.size.width / BAR_W).floor().max(1.0) as u64;
            NSRectFill(NSRect::new(
                NSPoint::new(
                    bounds.origin.x + (generation % slots) as f64 * BAR_W,
                    bounds.origin.y,
                ),
                NSSize::new(BAR_W, bounds.size.height),
            ));
            drawmark::mark(Mark {
                generation,
                rows: 0..1,
                draw_start_ns: start,
                draw_end_ns: sampler::uptime_ns().unwrap_or(0),
            });
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.arrive("keyDown", event, true);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.arrive("mouseDown", event, true);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.arrive("mouseDragged", event, false);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.arrive("mouseUp", event, false);
        }
    }
);

impl InputView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<InputView> {
        let this = InputView::alloc(mtm).set_ivars(InputIvars {
            _live: INPUT_VIEW.live(),
            generation: Cell::new(0),
            arrivals: RefCell::new(Vec::new()),
        });
        // SAFETY: назначенный инициализатор NSView с прямоугольником.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub fn generation(&self) -> u64 {
        self.ivars().generation.get()
    }

    pub fn arrivals(&self) -> Vec<Arrival> {
        self.ivars().arrivals.borrow().clone()
    }

    fn arrive(&self, kind: &'static str, event: &NSEvent, advance: bool) {
        let ivars = self.ivars();
        if advance {
            ivars.generation.set(ivars.generation.get() + 1);
            self.setNeedsDisplay(true);
        }
        let is_key = kind == "keyDown";
        ivars.arrivals.borrow_mut().push(Arrival {
            kind,
            key_code: is_key.then(|| event.keyCode()),
            flags: (event.modifierFlags() & NSEventModifierFlags::DeviceIndependentFlagsMask).0,
            clicks: if is_key { 0 } else { event.clickCount() },
            generation: ivars.generation.get(),
            at_ns: sampler::uptime_ns().unwrap_or(0),
        });
    }
}

/// Сверка дошедших до вида событий с отправленными: `keyDown` клавиши `j` без модификаторов, затем
/// `mouseDown` с Shift и [`CLICKS`] щелчками, `mouseDragged` и `mouseUp` того же жеста.
pub fn check_arrivals(arrivals: &[Arrival]) -> Result<(), String> {
    let shift = NSEventModifierFlags::Shift.0;
    let expected: [(&str, Option<u16>, usize, isize); 4] = [
        ("keyDown", Some(Key::J.code), 0, 0),
        ("mouseDown", None, shift, CLICKS),
        ("mouseDragged", None, shift, CLICKS),
        ("mouseUp", None, shift, CLICKS),
    ];
    let got: Vec<(&str, Option<u16>, usize, isize)> = arrivals
        .iter()
        .map(|arrival| {
            (
                arrival.kind,
                arrival.key_code,
                arrival.flags,
                arrival.clicks,
            )
        })
        .collect();
    if got == expected {
        Ok(())
    } else {
        Err(format!(
            "ожидались (вид, keyCode, флаги, щелчки) {expected:?}, дошли {got:?}"
        ))
    }
}

enum Step {
    /// Ожидание покрытия поколения и возврата `sendEvent` всех событий шага.
    Input {
        name: &'static str,
        generation: u64,
        posted: Vec<u64>,
        deadline: Instant,
    },
    Snapshot {
        job: Background<Snapshot>,
        deadline: Instant,
    },
    Done,
}

/// Идущая живая часть `smoke`: вид, стенд событий и наблюдатель отметок живут до итога
/// [`Smoke::poll`].
pub struct Smoke {
    mtm: MainThreadMarker,
    app: Retained<NSApplication>,
    window: Retained<NSWindow>,
    view: Retained<InputView>,
    out: PathBuf,
    waiter: Rc<RefCell<Waiter>>,
    covered: Rc<RefCell<Vec<Covered>>>,
    /// Итоги ожиданий по шагам: имя шага и покрытие.
    steps: RefCell<Vec<(&'static str, Covered)>>,
    step: RefCell<Step>,
    snapshot: RefCell<Option<Snapshot>>,
    report: RefCell<Option<Report>>,
}

fn now_ns() -> u64 {
    sampler::uptime_ns().unwrap_or(0)
}

impl Smoke {
    /// Ставит стенд событий и вид, уводит курсор и отправляет клавишу. `report` — итог готового
    /// окна, `out` — каталог процесса для снимка.
    pub fn start(
        mtm: MainThreadMarker,
        window: &NSWindow,
        mut report: Report,
        out: &Path,
        trace: SharedTrace,
    ) -> Result<Smoke, Failed> {
        let app = NSApplication::sharedApplication(mtm);
        let class = {
            let object: &objc2::runtime::AnyObject = &app;
            object.class().name().to_string_lossy().into_owned()
        };
        report.push("app_class", &class);
        events::install(app.clone(), trace);
        events::note("сценарий smoke");
        let bounds = window
            .contentView()
            .map(|view| view.bounds())
            .unwrap_or_else(|| window.contentLayoutRect());
        let view = InputView::new(mtm, bounds);
        window.setContentView(Some(&view));
        let responder: &NSResponder = &view;
        if !window.makeFirstResponder(Some(responder))
            || !events::is_first_responder(window, responder)
        {
            let state = events::focus_state(&app);
            events::uninstall();
            return Err(Box::new((
                format!("вид не стал first responder: {state}"),
                report,
            )));
        }
        let cursor = match events::warp_cursor_outside(mtm, &app) {
            Ok(point) => point,
            Err(reason) => {
                events::uninstall();
                return Err(Box::new((reason, report)));
            }
        };
        report.push("cursor_warped_to", events::describe_point(cursor));
        drawmark::take();
        let waiter = Rc::new(RefCell::new(Waiter::default()));
        let covered = Rc::new(RefCell::new(Vec::new()));
        {
            let waiter = Rc::clone(&waiter);
            let covered = Rc::clone(&covered);
            drawmark::on_mark(move |mark| {
                if let Some(done) = waiter.borrow_mut().observe(mark) {
                    covered.borrow_mut().push(done);
                }
            });
        }
        let smoke = Smoke {
            mtm,
            app,
            window: window.retain(),
            view,
            out: out.to_path_buf(),
            waiter,
            covered,
            steps: RefCell::new(Vec::new()),
            step: RefCell::new(Step::Done),
            snapshot: RefCell::new(None),
            report: RefCell::new(Some(report)),
        };
        match smoke.press_key() {
            Ok(step) => *smoke.step.borrow_mut() = step,
            Err(reason) => return Err(smoke.fail(reason)),
        }
        Ok(smoke)
    }

    fn expect_next(&self) -> u64 {
        let generation = self.view.generation() + 1;
        self.waiter.borrow_mut().expect(Change {
            generation,
            visible: 0..1,
            changed_ns: now_ns(),
        });
        generation
    }

    fn press_key(&self) -> Result<Step, String> {
        let generation = self.expect_next();
        let posted = events::press(Key::J)?;
        Ok(Step::Input {
            name: "key",
            generation,
            posted,
            deadline: Instant::now() + MARK_DEADLINE,
        })
    }

    fn click(&self) -> Result<Step, String> {
        let frame = self.window.convertRectToScreen(self.view.frame());
        let point = events::center(frame);
        let generation = self.expect_next();
        let gesture = events::mouse_down(
            self.mtm,
            point,
            &self.window,
            NSEventModifierFlags::Shift,
            CLICKS,
            "smoke",
        )?;
        let mut posted = vec![gesture.down];
        posted.push(gesture.drag_to(NSPoint::new(point.x + DRAG.x, point.y + DRAG.y), "smoke")?);
        posted.push(gesture.up("smoke")?);
        Ok(Step::Input {
            name: "click",
            generation,
            posted,
            deadline: Instant::now() + MARK_DEADLINE,
        })
    }

    /// Проверка с таймера стенда. Итог — после снимка или при ошибке исполнения.
    pub fn poll(&self) -> Option<Result<Report, Failed>> {
        let step = std::mem::replace(&mut *self.step.borrow_mut(), Step::Done);
        match self.advance(step) {
            Ok(Step::Done) => Some(self.finish()),
            Ok(next) => {
                *self.step.borrow_mut() = next;
                None
            }
            Err(reason) => Some(Err(self.fail(reason))),
        }
    }

    fn advance(&self, step: Step) -> Result<Step, String> {
        match step {
            Step::Input {
                name,
                generation,
                posted,
                deadline,
            } => {
                let returned = posted.iter().all(|number| {
                    events::record(*number).is_some_and(|record| record.send_return.is_some())
                });
                let done = returned.then(|| self.covered.borrow_mut().pop()).flatten();
                if let Some(covered) = done {
                    self.steps.borrow_mut().push((name, covered));
                    return match name {
                        "key" => self.click(),
                        _ => Ok(Step::Snapshot {
                            job: snapshot::capture_window(&self.window, &self.out, "smoke-window"),
                            deadline: Instant::now() + SNAPSHOT_DEADLINE,
                        }),
                    };
                }
                if Instant::now() < deadline {
                    return Ok(Step::Input {
                        name,
                        generation,
                        posted,
                        deadline,
                    });
                }
                let records: Vec<String> = posted
                    .iter()
                    .map(|number| match events::record(*number) {
                        Some(record) => format!(
                            "#{number} {} start={:?} return={:?}",
                            record.label, record.dispatch_start, record.send_return
                        ),
                        None => format!("#{number} нет записи"),
                    })
                    .collect();
                Err(format!(
                    "шаг {name}: поколение {generation} не покрыто отметкой или sendEvent не вернулся за {} мс; поколение вида {}, события {}; {}",
                    MARK_DEADLINE.as_millis(),
                    self.view.generation(),
                    records.join(", "),
                    events::focus_state(&self.app)
                ))
            }
            Step::Snapshot { job, deadline } => match job.try_take() {
                Some(Ok(snapshot)) if snapshot.status == SnapshotStatus::Error => Err(format!(
                    "снимок окна не записан: {}",
                    snapshot.reason.unwrap_or_default()
                )),
                Some(Ok(snapshot)) => {
                    *self.snapshot.borrow_mut() = Some(snapshot);
                    Ok(Step::Done)
                }
                Some(Err(reason)) => Err(format!("снимок окна: {reason}")),
                None if Instant::now() < deadline => Ok(Step::Snapshot { job, deadline }),
                None => Err(format!(
                    "снимок окна не готов за {} с",
                    SNAPSHOT_DEADLINE.as_secs()
                )),
            },
            Step::Done => Ok(Step::Done),
        }
    }

    /// Отметки событий, дошедшие события и отметки отрисовки в итог; стенд снимается.
    fn close(&self) -> (Report, Option<String>, usize) {
        let mut report = self.report.borrow_mut().take().unwrap_or_default();
        let released = events::release_open_gestures();
        let external = events::external_input();
        let records = events::export();
        let complete = records
            .iter()
            .filter(|record| record.dispatch_start_ms.is_some() && record.send_return_ms.is_some())
            .count();
        report.push(
            "events",
            serde_json::to_string(&records).unwrap_or_default(),
        );
        report.push(
            "arrivals",
            serde_json::to_string(&self.view.arrivals()).unwrap_or_default(),
        );
        report.push("released_gestures", released);
        report.push("marks", drawmark::take().len());
        drawmark::on_mark(|_| {});
        events::clear();
        events::uninstall();
        (report, external, complete)
    }

    fn fail(&self, reason: String) -> Failed {
        let (report, external, _) = self.close();
        let reason = match external {
            Some(input) => format!("{reason}; внешний ввод во время сценария: {input}"),
            None => reason,
        };
        Box::new((reason, report))
    }

    fn finish(&self) -> Result<Report, Failed> {
        let arrivals = self.view.arrivals();
        let (mut report, external, complete) = self.close();
        if let Some(input) = external {
            return Err(Box::new((
                format!("внешний ввод во время сценария: {input}"),
                report,
            )));
        }
        match check_arrivals(&arrivals) {
            Ok(()) => report.finding(
                "events_delivered",
                Verdict::Pass,
                format!(
                    "клавиша j и жест мыши с Shift и {CLICKS} щелчками дошли до first responder, начало и возврат sendEvent у {complete} событий стенда"
                ),
            ),
            Err(reason) => report.finding("events_delivered", Verdict::Fail, reason),
        }
        let steps = self.steps.borrow();
        let violations: Vec<String> = steps
            .iter()
            .filter_map(|(name, covered)| {
                covered
                    .order_violation()
                    .map(|violation| format!("{name}: {violation}"))
            })
            .collect();
        let latencies: Vec<f64> = steps
            .iter()
            .map(|(_, covered)| covered.latency_ns() as f64 / 1e6)
            .collect();
        report.push(
            "covered",
            serde_json::to_string(
                &steps
                    .iter()
                    .map(|(name, covered)| (name, covered))
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_default(),
        );
        report.metric("post_to_draw_ms", "ms", latencies.clone());
        if violations.is_empty() {
            report.finding(
                "drawmark_wait",
                Verdict::Pass,
                format!(
                    "поколения шагов {} покрыты отметкой за {} мс при сроке {} мс, порядок соблюдён",
                    steps
                        .iter()
                        .map(|(name, covered)| format!("{name}={}", covered.change.generation))
                        .collect::<Vec<_>>()
                        .join(", "),
                    latencies
                        .iter()
                        .map(|ms| format!("{ms:.1}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    MARK_DEADLINE.as_millis()
                ),
            );
        } else {
            report.finding("drawmark_wait", Verdict::Fail, violations.join("; "));
        }
        match self.snapshot.borrow().as_ref() {
            Some(snapshot) if snapshot.status == SnapshotStatus::Ok => report.finding(
                "window_snapshot",
                Verdict::Pass,
                format!(
                    "снимок окна {} записан в рабочем потоке",
                    snapshot.file.as_deref().unwrap_or_default()
                ),
            ),
            Some(snapshot) => report.finding(
                "window_snapshot",
                Verdict::NotRun,
                snapshot.reason.clone().unwrap_or_default(),
            ),
            None => report.finding("window_snapshot", Verdict::NotRun, "снимка нет"),
        }
        if let Some(snapshot) = self.snapshot.borrow().as_ref() {
            report.push(
                "snapshot",
                serde_json::to_string(snapshot).unwrap_or_default(),
            );
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrival(kind: &'static str, key_code: Option<u16>, flags: usize, clicks: isize) -> Arrival {
        Arrival {
            kind,
            key_code,
            flags,
            clicks,
            generation: 1,
            at_ns: 0,
        }
    }

    #[test]
    fn arrivals_must_match_what_was_sent() {
        let shift = NSEventModifierFlags::Shift.0;
        let good = vec![
            arrival("keyDown", Some(38), 0, 0),
            arrival("mouseDown", None, shift, 2),
            arrival("mouseDragged", None, shift, 2),
            arrival("mouseUp", None, shift, 2),
        ];
        assert_eq!(check_arrivals(&good), Ok(()));
        let mut lost_shift = good.clone();
        lost_shift[1].flags = 0;
        assert!(check_arrivals(&lost_shift).is_err());
        let mut single = good.clone();
        single[1].clicks = 1;
        assert!(check_arrivals(&single).is_err());
        assert!(check_arrivals(&good[..3]).is_err());
        let mut wrong_key = good;
        wrong_key[0].key_code = Some(40);
        assert!(check_arrivals(&wrong_key).is_err());
    }
}
