//! Сценарий `key-delivery`: синтетические клавиши, положенные `postEvent`, доходят до
//! `keyDown` first responder, до эквивалента пункта главного меню через `performKeyEquivalent`
//! и до field editor поля. Используются стрелки и Cmd-K с явно заданными символами, поэтому
//! латинская раскладка не нужна.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSMenu, NSMenuItem, NSTextField};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use super::events::{self, Gesture, Key};
use super::launch::input_source;
use super::stand::{InputView, StandTarget};
use super::{
    Poll, Report, SETTLE_TIMEOUT, Scenario, Settle, Stand, Step, StepError, StepFailure, Verdict,
    poll, run_steps,
};

const STEP_TIMEOUT: Duration = Duration::from_secs(3);
const FIELD_TEXT: &str = "abc";

/// Индексы шагов, после которых путь доставки считается проверенным.
const FIRST_RESPONDER_DONE: usize = 2;
const KEY_EQUIVALENT_DONE: usize = 3;

struct Run {
    stand: Rc<Stand>,
    view: Retained<InputView>,
    field: Retained<NSTextField>,
    target: Retained<StandTarget>,
    settle: Rc<Settle>,
    chord: RefCell<Vec<u64>>,
    gesture: RefCell<Option<Gesture>>,
    selection_before: Cell<Option<(usize, usize)>>,
}

impl Run {
    fn selection(&self) -> Option<(usize, usize)> {
        let editor = self.field.currentEditor()?;
        let range = editor.selectedRange();
        Some((range.location, range.length))
    }

    fn editor_is_first_responder(&self) -> bool {
        let window = &self.stand.window;
        match (self.field.currentEditor(), window.firstResponder()) {
            (Some(editor), Some(responder)) => events::same(&*editor, &*responder),
            _ => false,
        }
    }

    fn view_has(&self, kind: &str, key: Key) -> bool {
        self.view
            .arrivals()
            .iter()
            .any(|arrival| arrival.kind == kind && arrival.key_code == Some(key.code))
    }

    fn all_returned(numbers: &[u64]) -> bool {
        !numbers.is_empty()
            && numbers.iter().all(|number| {
                events::record(*number).is_some_and(|record| record.send_return.is_some())
            })
    }

    fn state(&self) -> String {
        let arrivals: Vec<String> = self
            .view
            .arrivals()
            .iter()
            .map(|arrival| arrival.describe())
            .collect();
        format!(
            "view={arrivals:?} keyEquivalents={} selection={:?} {}",
            self.target.key_equivalents(),
            self.selection(),
            events::focus_state(&self.stand.app)
        )
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::KeyDelivery);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let Some(content) = stand.window.contentView() else {
        return stand.conclude(report, Err("у окна нет contentView".to_owned()));
    };
    if let Some(source) = input_source(&stand.window) {
        report.push("input_source", source);
    }
    let mtm = stand.app.mtm();
    let target = StandTarget::new(mtm);
    if let Some(main_menu) = stand.app.mainMenu() {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Стенд"));
        menu.addItem(&target.item(
            mtm,
            "Эквивалент стенда",
            StandTarget::key_equivalent_action(),
            Key::CMD_K.ignoring,
            0,
        ));
        let item = NSMenuItem::new(mtm);
        item.setSubmenu(Some(&menu));
        main_menu.addItem(&item);
    }

    let view = InputView::new(
        mtm,
        NSRect::new(NSPoint::new(20.0, 20.0), NSSize::new(240.0, 120.0)),
        NSAutoresizingMaskOptions::ViewMaxXMargin | NSAutoresizingMaskOptions::ViewMaxYMargin,
    );
    content.addSubview(view.as_view());
    let height = content.bounds().size.height;
    let field = NSTextField::textFieldWithString(&NSString::from_str(FIELD_TEXT), mtm);
    field.setFrame(NSRect::new(
        NSPoint::new(20.0, height - 50.0),
        NSSize::new(240.0, 24.0),
    ));
    field.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewMaxXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
    );
    content.addSubview(&field);
    // Начальный first responder назначает код сцены.
    stand.window.makeFirstResponder(Some(&view));

    let run = Rc::new(Run {
        stand: Rc::clone(&stand),
        view,
        field,
        target,
        settle: Settle::new(vec![stand.window.clone()]),
        chord: RefCell::new(Vec::new()),
        gesture: RefCell::new(None),
        selection_before: Cell::new(None),
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
            let window = &run.stand.window;
            let key = app.keyWindow();
            poll(
                app.isActive()
                    && key.is_some_and(|key| events::same(&*key, &**window))
                    && events::is_first_responder(window, &run.view),
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
    let arrow = || events::press(Key::DOWN).map(|_| ());
    let key_down = {
        let run = Rc::clone(run);
        move || {
            poll(
                run.view_has("keyDown", Key::DOWN) && run.view_has("keyUp", Key::DOWN),
                || run.state(),
            )
        }
    };
    let chord = {
        let run = Rc::clone(run);
        move || {
            *run.chord.borrow_mut() = events::press(Key::CMD_K)?;
            Ok(())
        }
    };
    let equivalent = {
        let run = Rc::clone(run);
        move || {
            poll(
                run.target.key_equivalents() > 0 && Run::all_returned(&run.chord.borrow()),
                || run.state(),
            )
        }
    };
    let click = {
        let run = Rc::clone(run);
        move || {
            let field = &run.field;
            let window = &run.stand.window;
            let in_window = field.convertRect_toView(field.bounds(), None);
            let point = events::center(window.convertRectToScreen(in_window));
            *run.gesture.borrow_mut() = Some(events::mouse_down(
                window.mtm(),
                point,
                window,
                "NSTextField",
            )?);
            Ok(())
        }
    };
    let editing = {
        let run = Rc::clone(run);
        move || poll(run.editor_is_first_responder(), || run.state())
    };
    let release = {
        let run = Rc::clone(run);
        move || {
            let gesture = run.gesture.borrow();
            gesture
                .as_ref()
                .ok_or("жест не начат")?
                .up("NSTextField")
                .map(|_| ())
        }
    };
    let clicked = {
        let run = Rc::clone(run);
        move || {
            let down = run.gesture.borrow().as_ref().map(|gesture| gesture.down);
            poll(
                Run::all_returned(&down.into_iter().collect::<Vec<_>>()),
                || run.state(),
            )
        }
    };
    let left = {
        let run = Rc::clone(run);
        move || {
            run.selection_before.set(run.selection());
            events::press(Key::LEFT).map(|_| ())
        }
    };
    let moved = {
        let run = Rc::clone(run);
        move || {
            let now = run.selection();
            poll(now.is_some() && now != run.selection_before.get(), || {
                run.state()
            })
        }
    };
    vec![
        Step::new(
            "окно неподвижно, активность, key window и фокус на виде",
            SETTLE_TIMEOUT,
            focus,
            warp,
        ),
        Step::new("курсор вне окон", STEP_TIMEOUT, outside, arrow),
        Step::new(
            "keyDown и keyUp у first responder",
            STEP_TIMEOUT,
            key_down,
            chord,
        ),
        Step::new(
            "эквивалент пункта меню вызван",
            STEP_TIMEOUT,
            equivalent,
            click,
        ),
        Step::new(
            "field editor стал first responder",
            STEP_TIMEOUT,
            editing,
            release,
        ),
        Step::new("щелчок по полю обработан", STEP_TIMEOUT, clicked, left),
        Step::new(
            "выделение field editor сдвинулось",
            STEP_TIMEOUT,
            moved,
            || Ok(()),
        ),
    ]
}

fn finish(run: Rc<Run>, mut report: Report, result: Result<(), StepFailure>) {
    let stand = Rc::clone(&run.stand);
    run.settle.add_to(&mut report);
    for arrival in run.view.arrivals() {
        report.push("view_arrival", arrival.describe());
    }
    report.push("key_equivalents", run.target.key_equivalents());
    report.push(
        "selection_before_left",
        format!("{:?}", run.selection_before.get()),
    );
    report.push("selection_after_left", format!("{:?}", run.selection()));
    report.push("after", events::focus_state(&stand.app));

    let (reached, failed) = match &result {
        Ok(()) => (usize::MAX, None),
        Err(failure) => (failure.index, Some(failure)),
    };
    if let Some(failure) = failed
        && (failure.index < FIRST_RESPONDER_DONE || matches!(failure.error, StepError::Action(_)))
    {
        return stand.conclude(report, Err(failure.describe()));
    }
    let reason = |done: &str| match failed {
        Some(failure) => format!("{done}: {}", failure.describe()),
        None => done.to_owned(),
    };

    if reached > FIRST_RESPONDER_DONE {
        report.finding(
            "key_first_responder",
            Verdict::Pass,
            "keyDown и keyUp стрелки Down дошли до first responder",
        );
    } else {
        report.finding(
            "key_first_responder",
            Verdict::Fail,
            reason("keyDown не дошёл до first responder"),
        );
    }
    if reached > KEY_EQUIVALENT_DONE {
        let leaked = run.view_has("keyDown", Key::CMD_K);
        report.finding(
            "key_equivalent",
            if leaked { Verdict::Fail } else { Verdict::Pass },
            if leaked {
                "эквивалент вызван, но keyDown Cmd-K дошёл и до first responder".to_owned()
            } else {
                "Cmd-K вызвал пункт главного меню через performKeyEquivalent и не дошёл до keyDown"
                    .to_owned()
            },
        );
    } else if reached == KEY_EQUIVALENT_DONE {
        report.finding(
            "key_equivalent",
            Verdict::Fail,
            reason("Cmd-K не вызвал пункт меню"),
        );
    }
    if result.is_ok() {
        report.finding(
            "field_editor",
            Verdict::Pass,
            "щелчок сделал field editor поля first responder, стрелка Left сдвинула его выделение",
        );
    } else if reached > KEY_EQUIVALENT_DONE {
        report.finding(
            "field_editor",
            Verdict::Fail,
            reason("клавиша не дошла до field editor"),
        );
    }
    stand.conclude(report, Ok(()));
}
