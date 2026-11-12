//! Сценарий `native-menu`: синтетический щелчок по `NSPopUpButton` стенда открывает меню, а
//! пункт выбирается клавишами во вложенном цикле отслеживания. Если цикл синтетические события
//! не принимает, стенд по тайм-ауту вызывает `cancelTracking()`: штатное закрытие меню и
//! восстановленный основной цикл дают вывод `pending-manual`, иначе это ошибка исполнения.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSMenu, NSPopUpButton};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use super::events::{self, Gesture, Key};
use super::stand::StandTarget;
use super::{
    Poll, Report, SETTLE_TIMEOUT, Scenario, Settle, Stand, Step, StepError, StepFailure, Verdict,
    default_mode, poll, run_loop_mode, run_steps, wait_until,
};

const STEP_TIMEOUT: Duration = Duration::from_secs(3);
const ITEMS: [&str; 3] = ["Первый", "Второй", "Третий"];
/// Индекс шага «меню открыто»: с него стенд уже внутри жеста.
const GESTURE_STARTED: usize = 2;

struct Run {
    stand: Rc<Stand>,
    target: Retained<StandTarget>,
    button: Retained<NSPopUpButton>,
    menu: Retained<NSMenu>,
    settle: Rc<Settle>,
    gesture: RefCell<Option<Gesture>>,
    tracking_mode: RefCell<Option<String>>,
    highlight_at_open: RefCell<Option<String>>,
    highlight_after_key: RefCell<Option<String>>,
    /// Число подсветок делегата к моменту отправки Down.
    highlights_before_key: Cell<usize>,
}

impl Run {
    /// Первая подсветка делегата после отправки Down, отличная от начальной.
    fn highlight_by_key(&self) -> Option<String> {
        let at_open = self.highlight_at_open.borrow().clone();
        self.target
            .highlights()
            .into_iter()
            .skip(self.highlights_before_key.get())
            .flatten()
            .find(|title| Some(title) != at_open.as_ref())
    }

    fn highlighted(&self) -> Option<String> {
        self.menu
            .highlightedItem()
            .map(|item| item.title().to_string())
    }

    fn down_returned(&self) -> bool {
        self.down_record()
            .is_some_and(|record| record.send_return.is_some())
    }

    fn down_record(&self) -> Option<events::Record> {
        let number = self.gesture.borrow().as_ref().map(|gesture| gesture.down)?;
        events::record(number)
    }

    fn state(&self) -> String {
        format!(
            "opened={} closed={} highlighted={:?} chosen={:?} mode={} down_returned={} {}",
            self.target.opened(),
            self.target.closed(),
            self.highlighted(),
            self.target.chosen(),
            run_loop_mode(),
            self.down_returned(),
            events::focus_state(&self.stand.app)
        )
    }

    fn observe(&self, report: &mut Report) {
        self.settle.add_to(report);
        report.push("menu_opened", self.target.opened());
        report.push("menu_closed", self.target.closed());
        report.push(
            "run_loop_mode_in_tracking",
            self.tracking_mode.borrow().as_deref().unwrap_or("none"),
        );
        report.push(
            "highlight_at_open",
            format!("{:?}", self.highlight_at_open.borrow()),
        );
        report.push(
            "highlight_after_down",
            format!("{:?}", self.highlight_after_key.borrow()),
        );
        report.push("highlights", format!("{:?}", self.target.highlights()));
        report.push("chosen_tags", format!("{:?}", self.target.chosen()));
        report.push(
            "selected_item",
            self.button
                .titleOfSelectedItem()
                .map_or_else(|| "none".to_owned(), |title| title.to_string()),
        );
        if let Some(record) = self.down_record()
            && let (Some(start), Some(back)) = (record.dispatch_start, record.send_return)
        {
            let ms = (back - start).as_secs_f64() * 1e3;
            report.push("mouse_down_send_event_ms", format!("{ms:.1}"));
            report.metric("mouse_down_dispatch_to_return", "ms", vec![ms]);
        }
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::NativeMenu);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let Some(content) = stand.window.contentView() else {
        return stand.conclude(report, Err("у окна нет contentView".to_owned()));
    };
    let mtm = stand.app.mtm();
    let target = StandTarget::new(mtm);
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Стенд"));
    for (index, title) in ITEMS.iter().enumerate() {
        menu.addItem(&target.item(mtm, title, StandTarget::choose_action(), "", index as isize));
    }
    menu.setDelegate(Some(target.as_delegate()));
    let height = content.bounds().size.height;
    let button = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        NSRect::new(NSPoint::new(20.0, height - 46.0), NSSize::new(180.0, 26.0)),
        false,
    );
    button.setMenu(Some(&menu));
    button.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewMaxXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
    );
    content.addSubview(&button);

    let run = Rc::new(Run {
        stand: Rc::clone(&stand),
        target,
        button,
        menu,
        settle: Settle::new(vec![stand.window.clone()]),
        gesture: RefCell::new(None),
        tracking_mode: RefCell::new(None),
        highlight_at_open: RefCell::new(None),
        highlight_after_key: RefCell::new(None),
        highlights_before_key: Cell::new(0),
    });
    run_steps(steps(&run), move |result| finish(run, report, result));
}

fn steps(run: &Rc<Run>) -> Vec<Step> {
    let app = run.stand.app.clone();
    let window = run.stand.window.clone();
    let focus = {
        let app = app.clone();
        let settle = Rc::clone(&run.settle);
        move || {
            if let Poll::Pending(state) = settle.poll() {
                return Poll::Pending(state);
            }
            let key = app.keyWindow();
            poll(
                app.isActive() && key.is_some_and(|key| events::same(&*key, &*window)),
                || events::focus_state(&app),
            )
        }
    };
    let warp = {
        let app = app.clone();
        move || events::warp_cursor_outside(app.mtm(), &app).map(|_| ())
    };
    let outside = {
        let app = app.clone();
        move || poll(events::cursor_outside(&app), || events::focus_state(&app))
    };
    let click = {
        let run = Rc::clone(run);
        move || {
            let button = &run.button;
            let in_window = button.convertRect_toView(button.bounds(), None);
            let point = events::center(run.stand.window.convertRectToScreen(in_window));
            let gesture = events::mouse_down(
                run.stand.app.mtm(),
                point,
                &run.stand.window,
                "NSPopUpButton",
            )?;
            *run.gesture.borrow_mut() = Some(gesture);
            let menu = run.menu.clone();
            events::on_abort(move || menu.cancelTracking());
            Ok(())
        }
    };
    let opened = {
        let run = Rc::clone(run);
        move || poll(run.target.opened() > 0, || run.state())
    };
    let release = {
        let run = Rc::clone(run);
        move || {
            *run.tracking_mode.borrow_mut() = Some(run_loop_mode());
            let gesture = run.gesture.borrow();
            let gesture = gesture.as_ref().ok_or("жест не начат")?;
            gesture.up("NSPopUpButton").map(|_| ())
        }
    };
    // AppKit сам подсвечивает выбранный пункт вскоре после открытия; Down до этой подсветки
    // спутал бы её с откликом на клавишу.
    let initial = {
        let run = Rc::clone(run);
        move || poll(!run.target.highlights().is_empty(), || run.state())
    };
    let down = {
        let run = Rc::clone(run);
        move || {
            let highlights = run.target.highlights();
            *run.highlight_at_open.borrow_mut() = highlights.last().cloned().flatten();
            run.highlights_before_key.set(highlights.len());
            events::press(Key::DOWN).map(|_| ())
        }
    };
    let moved = {
        let run = Rc::clone(run);
        move || poll(run.highlight_by_key().is_some(), || run.state())
    };
    let confirm = {
        let run = Rc::clone(run);
        move || {
            *run.highlight_after_key.borrow_mut() = run.highlight_by_key();
            events::press(Key::RETURN).map(|_| ())
        }
    };
    let chosen = {
        let run = Rc::clone(run);
        let default = default_mode();
        move || {
            poll(
                !run.target.chosen().is_empty()
                    && run.target.closed() > 0
                    && run.down_returned()
                    && run_loop_mode() == default,
                || run.state(),
            )
        }
    };
    vec![
        Step::new(
            "окно неподвижно, активность и key window",
            SETTLE_TIMEOUT,
            focus,
            warp,
        ),
        Step::new("курсор вне окон", STEP_TIMEOUT, outside, click),
        Step::new("меню открыто", STEP_TIMEOUT, opened, release),
        Step::new("начальная подсветка AppKit", STEP_TIMEOUT, initial, down),
        Step::new(
            "подсветка сдвинулась клавишей",
            STEP_TIMEOUT,
            moved,
            confirm,
        ),
        Step::new(
            "пункт выбран, меню закрыто, основной цикл восстановлен",
            STEP_TIMEOUT,
            chosen,
            || Ok(()),
        ),
    ]
}

fn finish(run: Rc<Run>, mut report: Report, result: Result<(), StepFailure>) {
    let stand = Rc::clone(&run.stand);
    match result {
        Ok(()) => {
            run.observe(&mut report);
            let expected = run
                .highlight_after_key
                .borrow()
                .as_deref()
                .and_then(|title| ITEMS.iter().position(|item| *item == title));
            let chosen = run.target.chosen();
            match expected {
                Some(index) if chosen == [index as isize] => report.finding(
                    "native_menu",
                    Verdict::Pass,
                    format!(
                        "синтетический щелчок открыл меню, клавиша Down подсветила «{}», Return вызвал его обработчик",
                        ITEMS[index]
                    ),
                ),
                _ => report.finding(
                    "native_menu",
                    Verdict::Fail,
                    format!(
                        "вызван обработчик пунктов {chosen:?}, а подсвечен клавишей {:?}",
                        run.highlight_after_key.borrow()
                    ),
                ),
            }
            stand.conclude(report, Ok(()));
        }
        // Внутри жеста меню может быть открыто: пока цикл отслеживания крутится, `stop:` основной
        // цикл не завершит, поэтому сначала стенд выходит из отслеживания.
        Err(failure) if failure.index >= GESTURE_STARTED => cancel(run, report, failure),
        Err(failure) => {
            run.observe(&mut report);
            stand.conclude(report, Err(failure.describe()));
        }
    }
}

/// Выход из отслеживания после сбоя шага внутри жеста. Тайм-аут даёт вывод `pending-manual`,
/// ошибка действия остаётся ошибкой исполнения.
fn cancel(run: Rc<Run>, mut report: Report, failure: StepFailure) {
    let menu_open = run.target.opened() > 0 && run.target.closed() == 0;
    report.push("timed_out_step", failure.describe());
    report.push("cancel_tracking", menu_open);
    if menu_open {
        events::note("cancelTracking()");
        run.menu.cancelTracking();
    }
    let default = default_mode();
    let observed = Rc::clone(&run);
    wait_until(
        STEP_TIMEOUT,
        move || {
            let restored = observed.target.closed() >= observed.target.opened()
                && observed.down_returned()
                && run_loop_mode() == default;
            if restored {
                Poll::Ready(())
            } else {
                Poll::Pending(observed.state())
            }
        },
        move |result| {
            let stand = Rc::clone(&run.stand);
            run.observe(&mut report);
            match result {
                Ok(_) if matches!(failure.error, StepError::Action(_)) => stand.conclude(
                    report,
                    Err(format!(
                        "{}; отслеживание закрыто{}, основной цикл восстановлен",
                        failure.describe(),
                        if menu_open {
                            " через cancelTracking()"
                        } else {
                            ""
                        }
                    )),
                ),
                Ok(_) => {
                    let opened = run.target.opened() > 0;
                    report.finding(
                        "native_menu",
                        Verdict::PendingManual,
                        if opened {
                            format!(
                                "меню открылось, но вложенный цикл не довёл выбор клавишами: {}; меню закрыто{}, основной цикл восстановлен",
                                failure.describe(),
                                if menu_open { " через cancelTracking()" } else { "" }
                            )
                        } else {
                            format!(
                                "синтетический щелчок не открыл меню: {}; основной цикл восстановлен",
                                failure.describe()
                            )
                        },
                    );
                    stand.conclude(report, Ok(()));
                }
                Err(timeout) => stand.conclude(
                    report,
                    Err(format!(
                        "выйти из отслеживания не удалось после «{}»; последнее состояние: {}",
                        failure.describe(),
                        timeout.last_state
                    )),
                ),
            }
        },
    );
}
