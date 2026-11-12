//! Сценарий `parent-click`: дочерняя `NSPanel` стенда становится key, а синтетический щелчок
//! по точке родителя вне панели доходит до родителя. Доставка до окна (`sendEvent`) и до вида
//! записываются раздельно: вид стенда принимает первый щелчок, чтобы политика
//! `acceptsFirstMouse:` не выдавалась за отказ доставки.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSPanel, NSWindowOrderingMode, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, ns_string};

use super::events::{self, Gesture};
use super::stand::InputView;
use super::{
    Poll, Report, SETTLE_TIMEOUT, Scenario, Settle, Stand, Step, StepError, StepFailure, Verdict,
    poll, run_steps,
};

const STEP_TIMEOUT: Duration = Duration::from_secs(3);
const PANEL_SIZE: NSSize = NSSize::new(360.0, 180.0);
/// Отступ точки щелчка от края содержимого родителя и запас вокруг панели.
const MARGIN: f64 = 40.0;
/// Индекс шага «нажатие обработано»: с него идёт проверка доставки.
const DELIVERY: usize = 2;

struct Run {
    stand: Rc<Stand>,
    panel: Retained<NSPanel>,
    view: Retained<InputView>,
    settle: Rc<Settle>,
    gesture: RefCell<Option<Gesture>>,
    up: RefCell<Option<u64>>,
    expected: RefCell<Option<NSPoint>>,
}

impl Run {
    fn returned(&self, number: Option<u64>) -> bool {
        number
            .and_then(events::record)
            .is_some_and(|record| record.send_return.is_some())
    }

    fn down(&self) -> Option<u64> {
        self.gesture.borrow().as_ref().map(|gesture| gesture.down)
    }

    fn state(&self) -> String {
        let arrivals: Vec<String> = self
            .view
            .arrivals()
            .iter()
            .map(|arrival| arrival.describe())
            .collect();
        format!("view={arrivals:?} {}", events::focus_state(&self.stand.app))
    }

    /// Точка в содержимом родителя вне панели и на видимой части экрана.
    fn click_point(&self) -> Result<NSPoint, String> {
        let window = &self.stand.window;
        let content = window
            .contentView()
            .ok_or("у окна нет contentView")?
            .frame();
        let area = events::inset(window.convertRectToScreen(content), MARGIN);
        let visible = window
            .screen()
            .map(|screen| events::inset(screen.visibleFrame(), 2.0))
            .ok_or("окно не на экране")?;
        let panel = events::inset(self.panel.frame(), -MARGIN / 2.0);
        let (x0, y0) = (area.origin.x, area.origin.y);
        let (x1, y1) = (x0 + area.size.width, y0 + area.size.height);
        [
            NSPoint::new(x0, y0),
            NSPoint::new(x0, y1),
            NSPoint::new(x1, y0),
            NSPoint::new(x1, y1),
        ]
        .into_iter()
        .find(|point| events::contains(visible, *point) && !events::contains(panel, *point))
        .ok_or_else(|| {
            format!(
                "нет точки родителя вне панели: содержимое {} панель {} экран {}",
                events::describe_rect(area),
                events::describe_rect(panel),
                events::describe_rect(visible)
            )
        })
    }

    fn observe(&self, report: &mut Report) {
        self.settle.add_to(report);
        let window = self.stand.window.windowNumber();
        report.push("parent_window", window);
        report.push("panel_window", self.panel.windowNumber());
        report.push("panel_frame", events::describe_rect(self.panel.frame()));
        report.push(
            "parent_frame",
            events::describe_rect(self.stand.window.frame()),
        );
        if let Some(point) = *self.expected.borrow() {
            report.push("expected_view_point", events::describe_point(point));
        }
        for arrival in self.view.arrivals() {
            report.push("view_arrival", arrival.describe());
        }
        report.push("after_click", events::focus_state(&self.stand.app));
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::ParentClick);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let Some(content) = stand.window.contentView() else {
        return stand.conclude(report, Err("у окна нет contentView".to_owned()));
    };
    let mtm = stand.app.mtm();
    let view = InputView::new(
        mtm,
        content.bounds(),
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    content.addSubview(view.as_view());

    let parent = stand.window.frame();
    let origin = NSPoint::new(
        (parent.origin.x + (parent.size.width - PANEL_SIZE.width) / 2.0).floor(),
        (parent.origin.y + (parent.size.height - PANEL_SIZE.height) / 2.0).floor(),
    );
    // Панель утилиты: AeroSpace и другие менеджеры окон не раскладывают такие окна плиткой.
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        NSRect::new(origin, PANEL_SIZE),
        NSWindowStyleMask::Titled | NSWindowStyleMask::UtilityWindow,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: панель держит Rust, AppKit не должен освобождать её при закрытии.
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setTitle(ns_string!("Стенд: дочерняя панель"));
    // SAFETY: панель жива дольше связи: её держит сценарий до конца процесса.
    unsafe {
        stand
            .window
            .addChildWindow_ordered(&panel, NSWindowOrderingMode::Above)
    };
    // Key-статус панели назначает код сцены, как в обычном приложении.
    panel.makeKeyAndOrderFront(None);

    let settle = Settle::new(vec![stand.window.clone(), panel.clone().into_super()]);
    let run = Rc::new(Run {
        stand: Rc::clone(&stand),
        panel,
        view,
        settle,
        gesture: RefCell::new(None),
        up: RefCell::new(None),
        expected: RefCell::new(None),
    });
    run_steps(steps(&run), move |result| finish(run, report, result));
}

fn steps(run: &Rc<Run>) -> Vec<Step> {
    let focus = {
        let run = Rc::clone(run);
        move || {
            if let Poll::Pending(state) = run.settle.poll() {
                return Poll::Pending(state);
            }
            let app = &run.stand.app;
            let key = app.keyWindow();
            poll(
                app.isActive() && key.is_some_and(|key| events::same(&*key, &*run.panel)),
                || events::focus_state(app),
            )
        }
    };
    let warp = {
        let run = Rc::clone(run);
        move || events::warp_cursor_outside(run.stand.app.mtm(), &run.stand.app).map(|_| ())
    };
    let outside = {
        let run = Rc::clone(run);
        move || {
            poll(events::cursor_outside(&run.stand.app), || {
                events::focus_state(&run.stand.app)
            })
        }
    };
    let press = {
        let run = Rc::clone(run);
        move || {
            let point = run.click_point()?;
            let window = &run.stand.window;
            *run.expected.borrow_mut() = Some(
                run.view
                    .convertPoint_fromView(window.convertPointFromScreen(point), None),
            );
            let gesture = events::mouse_down(window.mtm(), point, window, "родитель вне панели")?;
            *run.gesture.borrow_mut() = Some(gesture);
            Ok(())
        }
    };
    let pressed = {
        let run = Rc::clone(run);
        move || poll(run.returned(run.down()), || run.state())
    };
    let release = {
        let run = Rc::clone(run);
        move || {
            let gesture = run.gesture.borrow();
            let gesture = gesture.as_ref().ok_or("жест не начат")?;
            *run.up.borrow_mut() = Some(gesture.up("родитель вне панели")?);
            Ok(())
        }
    };
    let released = {
        let run = Rc::clone(run);
        move || poll(run.returned(*run.up.borrow()), || run.state())
    };
    vec![
        Step::new(
            "окна неподвижны, активность и key-панель",
            SETTLE_TIMEOUT,
            focus,
            warp,
        ),
        Step::new("курсор вне окон", STEP_TIMEOUT, outside, press),
        Step::new("нажатие обработано", STEP_TIMEOUT, pressed, release),
        Step::new("отпускание обработано", STEP_TIMEOUT, released, || Ok(())),
    ]
}

fn finish(run: Rc<Run>, mut report: Report, result: Result<(), StepFailure>) {
    let stand = Rc::clone(&run.stand);
    run.observe(&mut report);
    let delivered_to_window = run
        .down()
        .and_then(events::record)
        .is_some_and(|record| record.dispatch_start.is_some());
    report.push("delivered_to_window", delivered_to_window);
    match result {
        Ok(()) => {
            let expected = *run.expected.borrow();
            let arrivals = run.view.arrivals();
            let at_point = |kind: &str| {
                arrivals.iter().any(|arrival| {
                    arrival.kind == kind
                        && arrival.window == run.stand.window.windowNumber()
                        && match (arrival.location, expected) {
                            (Some(got), Some(want)) => {
                                (got.x - want.x).abs() <= 0.5 && (got.y - want.y).abs() <= 0.5
                            }
                            _ => false,
                        }
                })
            };
            let (down, up) = (at_point("mouseDown"), at_point("mouseUp"));
            report.push("view_mouse_down", down);
            report.push("view_mouse_up", up);
            if down && up {
                report.finding(
                    "parent_click",
                    Verdict::Pass,
                    "при key-панели щелчок по родителю вне панели дошёл до окна родителя и до его вида в ожидаемой точке",
                );
            } else {
                report.finding(
                    "parent_click",
                    Verdict::PendingManual,
                    format!(
                        "окно родителя получило событие: {delivered_to_window}; вид получил mouseDown: {down}, mouseUp: {up}"
                    ),
                );
            }
            stand.conclude(report, Ok(()));
        }
        Err(failure) => match failure.error {
            StepError::TimedOut(_) if failure.index >= DELIVERY => {
                report.finding(
                    "parent_click",
                    Verdict::PendingManual,
                    format!(
                        "щелчок по родителю не обработан: {}; окно родителя получило событие: {delivered_to_window}",
                        failure.describe()
                    ),
                );
                stand.conclude(report, Ok(()));
            }
            _ => stand.conclude(report, Err(failure.describe())),
        },
    }
}
