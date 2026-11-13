//! Сценарий `startup`: от запуска процесса до обработанной контрольной клавиши. Начало —
//! `p_starttime` процесса, конец — `gettimeofday` в момент, когда клавиша, отправленная после
//! первого `display` key window, обработана и её эффект прочитан. Первый `display`, готовность и
//! обработка клавиши — отдельные отметки. Проходы и возвраты `sendEvent` перехватываются сразу,
//! без ожидания таймера: таймер-предикат здесь только ограничивает срок.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use objc2::MainThreadOnly;
use objc2_app_kit::NSEventModifierFlags;

use super::events::{self, Key};
use super::sampler::{self, Entry, ProcessStart};
use super::{Poll, Report, Scenario, Stand, Verdict, wait_until};
use crate::results::{ClockCheck, StartupMarks};
use crate::tiles::CENTER;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Цифра выбирает вкладку полосы с фокусом: «2» — вторую, «3» — третью.
const DIGIT_KEYS: [Key; 2] = [
    Key {
        label: "2",
        code: 19,
        characters: "2",
        ignoring: "2",
        flags: NSEventModifierFlags::empty(),
    },
    Key {
        label: "3",
        code: 20,
        characters: "3",
        ignoring: "3",
        flags: NSEventModifierFlags::empty(),
    },
];

#[derive(Default)]
struct Marks {
    first_display: Option<u64>,
    ready: Option<u64>,
    key_display: Option<u64>,
    posted: Option<u64>,
    returned: Option<u64>,
    numbers: Vec<u64>,
    /// Конец: стенные мкс и монотонные нс.
    end: Option<(i64, u64)>,
    effect: Option<Option<usize>>,
    error: Option<String>,
    views_need_display_at_ready: Option<bool>,
}

/// Предусловие замеров старта, простоя и кадров: окно на встроенном дисплее.
pub fn require_builtin(stand: &Stand) -> Result<(), String> {
    if stand.display.builtin {
        Ok(())
    } else {
        Err(format!(
            "предусловие: встроенного дисплея нет, окно на «{}»; startup, idle и frames снимаются только на встроенном дисплее, нужен владелец",
            stand.display.screen.localizedName()
        ))
    }
}

fn now_ns() -> u64 {
    sampler::uptime_ns().unwrap_or(0)
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Startup);
    if let Err(reason) = stand
        .check_session(&mut report)
        .and_then(|()| require_builtin(&stand))
    {
        return stand.conclude(report, Err(reason));
    }
    let Some(scene) = stand.built_scene.clone() else {
        return stand.conclude(report, Err("сцена не собрана".to_owned()));
    };
    let Some(entry) = sampler::main_entry() else {
        return stand.conclude(report, Err("нет отметки входа в main".to_owned()));
    };
    let start = match sampler::process_start() {
        Ok(start) => start,
        Err(reason) => return stand.conclude(report, Err(reason)),
    };
    if let Err(reason) = events::warp_cursor_outside(stand.app.mtm(), &stand.app) {
        return stand.conclude(report, Err(reason));
    }
    let tile = scene.tiles.tile(CENTER).clone();
    let initial = tile.strip().active();
    let (key, target) = if initial == Some(1) {
        (DIGIT_KEYS[1], 2)
    } else {
        (DIGIT_KEYS[0], 1)
    };
    report.push("control_key", key.label);
    report.push("control_tab_before", format!("{initial:?}"));
    report.push("control_tab_target", target);
    report.push(
        "interval_startup",
        "p_starttime процесса → gettimeofday после чтения эффекта контрольной клавиши; клавиша отправлена в конце первого прохода отображения, на котором окно сцены key, видимо и ничего не ждёт отображения",
    );

    let marks = Rc::new(RefCell::new(Marks::default()));
    {
        let marks = Rc::clone(&marks);
        let stand = Rc::clone(&stand);
        events::on_display_pass(move |_| {
            let now = now_ns();
            let window = &stand.window;
            let mut marks = marks.borrow_mut();
            if marks.first_display.is_none() && window.isVisible() {
                marks.first_display = Some(now);
            }
            let key_window = stand
                .app
                .keyWindow()
                .is_some_and(|key| events::same(&*key, &**window));
            if marks.ready.is_none() && stand.app.isActive() && key_window {
                marks.ready = Some(now);
                marks.views_need_display_at_ready = Some(window.viewsNeedDisplay());
            }
            if marks.ready.is_none() || !window.isVisible() || window.viewsNeedDisplay() {
                return true;
            }
            marks.key_display = Some(now);
            marks.posted = Some(now_ns());
            match events::press(key) {
                Ok(numbers) => marks.numbers = numbers,
                Err(reason) => marks.error = Some(reason),
            }
            false
        });
    }
    {
        let marks = Rc::clone(&marks);
        events::on_send_return(move |number, _| {
            let mut marks = marks.borrow_mut();
            if marks.numbers.first() != Some(&number) {
                return true;
            }
            let returned = now_ns();
            let effect = tile.strip().active();
            let wall = sampler::wall_us();
            let mono = now_ns();
            marks.returned = Some(returned);
            marks.effect = Some(effect);
            match wall {
                Ok(wall) => marks.end = Some((wall, mono)),
                Err(reason) => marks.error = Some(reason),
            }
            false
        });
    }

    let observed = Rc::clone(&marks);
    wait_until(
        TIMEOUT,
        move || {
            let marks = observed.borrow();
            let up_returned = marks
                .numbers
                .last()
                .and_then(|number| events::record(*number))
                .is_some_and(|record| record.send_return.is_some());
            if marks.error.is_some() || (marks.end.is_some() && up_returned) {
                Poll::Ready(())
            } else {
                Poll::Pending(format!(
                    "first_display={:?} ready={:?} key_display={:?} posted={:?} returned={:?}",
                    marks.first_display.is_some(),
                    marks.ready.is_some(),
                    marks.key_display.is_some(),
                    marks.posted.is_some(),
                    marks.returned.is_some()
                ))
            }
        },
        move |result| {
            let marks = marks.replace(Marks::default());
            let outcome = match result {
                Ok(_) => conclude(&mut report, entry, start, &marks, target),
                Err(timeout) => Err(format!(
                    "контрольная клавиша не обработана за {} мс; последнее состояние: {}",
                    TIMEOUT.as_millis(),
                    timeout.last_state
                )),
            };
            stand.conclude(report, outcome);
        },
    );
}

fn conclude(
    report: &mut Report,
    entry: Entry,
    start: ProcessStart,
    marks: &Marks,
    target: usize,
) -> Result<(), String> {
    if let Some(error) = &marks.error {
        return Err(error.clone());
    }
    let Some((end_wall_us, end_mono_ns)) = marks.end else {
        return Err("нет отметки конца".to_owned());
    };
    let startup = StartupMarks {
        process_start_wall_us: start.wall_us,
        process_start_bsd_wall_us: start.bsd_wall_us,
        process_start_mono_ns: start.mono_ns,
        main_wall_us: entry.wall_us,
        main_mono_ns: entry.mono_ns,
        first_display_mono_ns: marks.first_display,
        ready_mono_ns: marks.ready,
        key_display_mono_ns: marks.key_display,
        control_posted_mono_ns: marks.posted,
        control_returned_mono_ns: marks.returned,
        end_wall_us,
        end_mono_ns,
    };
    report.startup = Some(startup);
    let since = |mark: Option<u64>| {
        startup
            .since_main_ms(mark)
            .map_or_else(|| "нет".to_owned(), |ms| format!("{ms:.3}"))
    };
    report.push("startup_wall_ms", format!("{:.3}", startup.wall_ms()));
    report.push("main_to_end_mono_ms", format!("{:.3}", startup.mono_ms()));
    report.push("pre_main_mono_ms", format!("{:.3}", startup.pre_main_ms()));
    report.push(
        "pre_main_wall_ms",
        format!("{:.3}", (entry.wall_us - start.wall_us) as f64 / 1e3),
    );
    report.push("first_display_since_main_ms", since(marks.first_display));
    report.push("ready_since_main_ms", since(marks.ready));
    report.push("key_display_since_main_ms", since(marks.key_display));
    report.push("control_posted_since_main_ms", since(marks.posted));
    report.push("control_returned_since_main_ms", since(marks.returned));
    report.push(
        "views_need_display_at_ready",
        format!("{:?}", marks.views_need_display_at_ready),
    );
    report.push(
        "process_start_sources_agree",
        start.bsd_wall_us == Some(start.wall_us),
    );
    report.metric("startup_wall", "ms", vec![startup.wall_ms()]);
    report.metric("startup_main_to_end_mono", "ms", vec![startup.mono_ms()]);
    let effect = marks.effect.flatten();
    report.finding(
        "control_effect",
        if effect == Some(target) {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!("после возврата sendEvent активна вкладка {effect:?}, ожидалась {target}"),
    );
    if effect != Some(target) {
        return Err(format!(
            "эффект контрольной клавиши не прочитан: активна вкладка {effect:?}, ожидалась {target}"
        ));
    }
    match startup.clock_check() {
        ClockCheck::Ok => Ok(()),
        ClockCheck::Distorted(reason) => Err(format!(
            "запуск искажён скачком системного времени, прогон повторяется: {reason}"
        )),
    }
}
