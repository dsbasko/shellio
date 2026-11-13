//! Сценарий `menu`: команда пробы вызывается тремя путями, и каждый ровно один раз увеличивает
//! счётчик её обработчика. Сочетание Cmd-J идёт через эквивалент пункта главного меню. Пункт
//! меню «…» плитки выбирается так, как решено в задаче 3: синтетический щелчок по кнопке «…»
//! открывает меню, пункт подсвечивается клавишами Down во вложенном цикле отслеживания и
//! выбирается Return. Команда палитры выполняется Enter после Cmd-K. Если цикл отслеживания
//! синтетические события не принял, стенд закрывает меню `cancelTracking()`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::{MainThreadOnly, Message};
use objc2_app_kit::{NSEventModifierFlags, NSMenu};

use super::events::{self, Gesture, Key};
use super::stand::StandTarget;
use super::{
    Poll, Report, SETTLE_TIMEOUT, Scenario, Settle, Stand, Step, StepFailure, Verdict,
    default_mode, poll, run_loop_mode, run_steps,
};
use crate::fixtures::{MARK_COMMAND, MARK_TITLE};
use crate::menu::{COMMANDS_MENU, Invocation, Via};
use crate::scene::Scene;
use crate::tabs::MENU_LABEL;
use crate::tiles::CENTER;

const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// Наибольшее число нажатий Down до пункта команды в меню «…».
const MAX_DOWNS: usize = 10;

const KEY_CMD_J: Key = Key {
    label: "Cmd-J",
    code: 38,
    characters: "j",
    ignoring: "j",
    flags: NSEventModifierFlags::Command,
};

fn verdict(ok: bool) -> Verdict {
    if ok { Verdict::Pass } else { Verdict::Fail }
}

struct Run {
    stand: Rc<Stand>,
    scene: Rc<Scene>,
    settle: Rc<Settle>,
    report: RefCell<Report>,
    /// Делегат меню «…» на время сценария: открытие, закрытие и подсветка.
    target: Retained<StandTarget>,
    keys: RefCell<Vec<u64>>,
    gesture: RefCell<Option<Gesture>>,
    before: Cell<usize>,
    downs: Cell<usize>,
    /// Подсветок делегата к моменту последнего Down.
    highlights_at_down: Cell<usize>,
    paths: RefCell<Vec<(String, usize, Option<Invocation>)>>,
}

impl Run {
    fn menu(&self) -> Retained<NSMenu> {
        self.scene.tiles.tile(CENTER).strip().menu().retain()
    }

    fn count(&self) -> usize {
        self.scene.commands.count(MARK_COMMAND)
    }

    fn press(&self, key: Key) -> Result<(), String> {
        *self.keys.borrow_mut() = events::press(key)?;
        Ok(())
    }

    fn keys_done(&self) -> bool {
        let numbers = self.keys.borrow();
        let returns: Option<Vec<Duration>> = numbers
            .iter()
            .map(|number| events::record(*number).and_then(|record| record.send_return))
            .collect();
        !numbers.is_empty()
            && returns
                .and_then(|returns| returns.into_iter().max())
                .is_some_and(|last| events::display_pass_after(last).is_some())
    }

    fn down_record(&self) -> Option<events::Record> {
        let number = self.gesture.borrow().as_ref().map(|gesture| gesture.down)?;
        events::record(number)
    }

    fn highlighted(&self) -> Option<String> {
        self.menu()
            .highlightedItem()
            .map(|item| item.title().to_string())
    }

    fn state(&self) -> String {
        format!(
            "count={} log={:?} opened={} closed={} highlighted={:?} downs={} mode={} palette_open={} {}",
            self.count(),
            self.scene.commands.log(),
            self.target.opened(),
            self.target.closed(),
            self.highlighted(),
            self.downs.get(),
            run_loop_mode(),
            self.scene.palette.is_open(),
            events::focus_state(&self.stand.app)
        )
    }

    /// Итог одного пути: прирост счётчика и последний вызов.
    fn path(&self, name: &str, via: Via) {
        let count = self.count();
        let delta = count.saturating_sub(self.before.get());
        let last = self.scene.commands.log().last().cloned();
        let ok = count == self.before.get() + 1
            && last
                .as_ref()
                .is_some_and(|last| last.id == MARK_COMMAND && last.via == via);
        self.report.borrow_mut().finding(
            name,
            verdict(ok),
            format!(
                "счётчик команды «{MARK_TITLE}» вырос на {delta} (ожидалось 1), последний вызов {last:?}, ожидался путь {via:?}"
            ),
        );
        self.paths.borrow_mut().push((name.to_owned(), delta, last));
        self.before.set(count);
    }

    /// Шлёт Down, пока делегат не подсветит пункт команды. Подсветку от клавиши ждёт перед
    /// следующим нажатием.
    fn drive_highlight(&self) -> Poll<()> {
        if self.highlighted().as_deref() == Some(MARK_TITLE) {
            return Poll::Ready(());
        }
        let downs = self.downs.get();
        if downs > 0 && self.target.highlights().len() <= self.highlights_at_down.get() {
            return Poll::Pending(format!(
                "Down {downs}: подсветка не сменилась; {}",
                self.state()
            ));
        }
        if downs >= MAX_DOWNS {
            return Poll::Pending(format!(
                "за {MAX_DOWNS} нажатий Down пункт «{MARK_TITLE}» не подсвечен; {}",
                self.state()
            ));
        }
        self.highlights_at_down.set(self.target.highlights().len());
        self.downs.set(downs + 1);
        if let Err(reason) = events::press(Key::DOWN) {
            return Poll::Pending(format!("Down не отправлен: {reason}"));
        }
        Poll::Pending(format!("Down {} отправлен", downs + 1))
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Menu);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let Some(scene) = stand.built_scene.clone() else {
        return stand.conclude(report, Err("сцена не собрана".to_owned()));
    };
    report.push("command", format!("{MARK_COMMAND} «{MARK_TITLE}»"));
    report.push(
        "menu_path",
        "пункт меню «…» центральной плитки: синтетический щелчок по кнопке и клавиши во вложенном цикле отслеживания, как выбрано в задаче 3",
    );
    report.push(
        "shortcut_path",
        "Cmd-J — эквивалент пункта главного меню «Команды»",
    );
    report.push(
        "palette_path",
        "Cmd-K, затем Enter по первой строке пустого запроса",
    );
    let target = StandTarget::new(stand.app.mtm());
    let run = Rc::new(Run {
        settle: Settle::new(vec![scene.window.clone()]),
        stand: Rc::clone(&stand),
        scene,
        report: RefCell::new(report),
        target,
        keys: RefCell::new(Vec::new()),
        gesture: RefCell::new(None),
        before: Cell::new(0),
        downs: Cell::new(0),
        highlights_at_down: Cell::new(0),
        paths: RefCell::new(Vec::new()),
    });
    run.menu().setDelegate(Some(run.target.as_delegate()));
    run_steps(steps(&run), move |result| finish(run, result));
}

fn step(
    run: &Rc<Run>,
    name: &'static str,
    ready: impl Fn(&Run) -> bool + 'static,
    act: impl FnOnce(&Run) -> Result<(), String> + 'static,
) -> Step {
    let (for_ready, for_act) = (Rc::clone(run), Rc::clone(run));
    Step::new(
        name,
        STEP_TIMEOUT,
        move || poll(ready(&for_ready), || for_ready.state()),
        move || act(&for_act),
    )
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
    let highlight = {
        let run = Rc::clone(run);
        move || run.drive_highlight()
    };
    let confirm = {
        let run = Rc::clone(run);
        move || run.press(Key::RETURN)
    };
    vec![
        Step::new(
            "окно неподвижно, активность и key window",
            SETTLE_TIMEOUT,
            first,
            warp,
        ),
        step(
            run,
            "курсор вне окон",
            |run| events::cursor_outside(&run.stand.app),
            |run| {
                run.before.set(run.count());
                run.press(KEY_CMD_J)
            },
        ),
        step(run, "Cmd-J обработан", Run::keys_done, |run| {
            run.path("shortcut", Via::Menu(COMMANDS_MENU.to_owned()));
            let key_down = run
                .keys
                .borrow()
                .get(KEY_CMD_J.modifiers().len())
                .and_then(|number| events::record(*number));
            if let Some(record) = key_down
                && let (Some(start), Some(back)) = (record.dispatch_start, record.send_return)
            {
                run.report.borrow_mut().metric(
                    "shortcut_dispatch_to_return",
                    "ms",
                    vec![events::to_ms(back.saturating_sub(start))],
                );
            }
            let tile = run.scene.tiles.tile(CENTER);
            let strip = tile.strip();
            let more = strip.buttons()[2];
            let in_window = more.convertRect_toView(more.bounds(), None);
            let point = events::center(run.scene.window.convertRectToScreen(in_window));
            let gesture =
                events::mouse_down(run.stand.app.mtm(), point, &run.scene.window, "кнопка «…»")?;
            *run.gesture.borrow_mut() = Some(gesture);
            let menu = run.menu().retain();
            events::on_abort(move || menu.cancelTracking());
            Ok(())
        }),
        step(
            run,
            "нажатие кнопки «…» принято",
            |run| {
                run.down_record()
                    .is_some_and(|record| record.dispatch_start.is_some())
            },
            |run| {
                let gesture = run.gesture.borrow();
                gesture
                    .as_ref()
                    .ok_or("жест не начат")?
                    .up("кнопка «…»")
                    .map(|_| ())
            },
        ),
        step(
            run,
            "меню «…» открыто",
            |run| run.target.opened() > 0,
            |run| {
                run.report
                    .borrow_mut()
                    .push("run_loop_mode_in_tracking", run_loop_mode());
                Ok(())
            },
        ),
        Step::new(
            "пункт команды подсвечен клавишами",
            STEP_TIMEOUT,
            highlight,
            confirm,
        ),
        step(
            run,
            "пункт выбран, меню закрыто",
            |run| {
                let default = default_mode();
                run.count() > run.before.get()
                    && run.target.closed() > 0
                    && run
                        .down_record()
                        .is_some_and(|record| record.send_return.is_some())
                    && run_loop_mode() == default
            },
            |run| {
                run.report.borrow_mut().push("downs", run.downs.get());
                run.report
                    .borrow_mut()
                    .push("highlights", format!("{:?}", run.target.highlights()));
                run.path("menu_item", Via::Menu(MENU_LABEL.to_owned()));
                run.press(Key::CMD_K)
            },
        ),
        step(
            run,
            "палитра открыта",
            |run| run.keys_done() && run.scene.palette.is_open(),
            |run| run.press(Key::RETURN),
        ),
        step(
            run,
            "команда палитры выполнена",
            |run| run.keys_done() && !run.scene.palette.is_open(),
            |run| {
                run.path("palette_command", Via::Palette);
                Ok(())
            },
        ),
    ]
}

fn finish(run: Rc<Run>, result: Result<(), StepFailure>) {
    let stand = Rc::clone(&run.stand);
    run.menu().setDelegate(None);
    let mut report = run.report.replace(Report::default());
    run.settle.add_to(&mut report);
    let log = run.scene.commands.log();
    report.push("commands_log", format!("{log:?}"));
    if result.is_ok() {
        let paths = run.paths.borrow();
        let others = log.iter().filter(|call| call.id != MARK_COMMAND).count();
        let ok = paths.len() == 3
            && paths.iter().all(|(_, delta, _)| *delta == 1)
            && run.count() == 3
            && others == 0;
        report.finding(
            "one_handler",
            verdict(ok),
            format!(
                "сочетание, пункт меню и команда палитры вызвали один обработчик: всего вызовов команды {}, по путям {:?}, вызовов других команд {others}",
                run.count(),
                paths
                    .iter()
                    .map(|(name, delta, _)| format!("{name} +{delta}"))
                    .collect::<Vec<_>>()
            ),
        );
    }
    let result = result.map_err(|failure| failure.describe());
    drop(run);
    stand.conclude(report, result);
}
