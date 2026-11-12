//! Самопроверка сторожа `watchdog-check`: после одного события главный поток зависает, и
//! процесс обязан завершиться кодом ошибки исполнения со следом в `--out`. В общий прогон не
//! входит.

use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use super::events::{self, Key};
use super::{Scenario, Stand, Step, poll, run_steps};

pub fn run(stand: Rc<Stand>) {
    let report = stand.report(Scenario::WatchdogCheck);
    let app = stand.app.clone();
    let steps = vec![Step::new(
        "активность",
        Duration::from_secs(2),
        move || poll(app.isActive(), || events::focus_state(&app)),
        || events::press(Key::DOWN).map(|_| ()),
    )];
    run_steps(steps, move |result| {
        if let Err(failure) = result {
            return stand.conclude(report, Err(failure.describe()));
        }
        events::note("главный поток зависает намеренно");
        // Отправитель жив, пока поток ждёт, поэтому `recv` не вернётся.
        let (_sender, receiver) = mpsc::channel::<()>();
        let _ = receiver.recv();
        stand.conclude(report, Err("главный поток вышел из зависания".to_owned()));
    });
}
