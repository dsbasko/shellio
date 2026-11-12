//! Перечень сценариев и общая часть стенда: итог процесса, ожидание предиката и предусловия.

pub mod launch;

use std::cell::RefCell;
use std::fmt::Display;
use std::process::ExitCode;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSEvent, NSEventModifierFlags, NSEventType, NSWindow};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFString, CFType};
use objc2_core_graphics::CGSessionCopyCurrentDictionary;
use objc2_foundation::{NSPoint, NSRunLoop, NSRunLoopCommonModes, NSTimer};

use crate::DisplayChoice;
use crate::args::SceneOptions;

/// Код выхода при ошибке исполнения: стенд не дошёл до конца, предусловие не выполнено,
/// тайм-аут. Отрицательный вывод гипотезы ошибкой исполнения не считается.
pub const EXIT_EXECUTION: u8 = 3;

const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Launch,
}

impl Scenario {
    pub const ALL: [Scenario; 1] = [Scenario::Launch];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Launch => "launch",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL
            .into_iter()
            .find(|scenario| scenario.name() == name)
    }
}

pub fn start(scenario: Scenario, stand: Rc<Stand>) {
    match scenario {
        Scenario::Launch => launch::run(stand),
    }
}

/// Сцена, на которой работает сценарий. Её строит код сцены, стенд фокус не исправляет.
pub struct Stand {
    pub app: Retained<NSApplication>,
    pub window: Retained<NSWindow>,
    pub display: DisplayChoice,
    pub scene: SceneOptions,
    pub activation: &'static str,
    pub finisher: Finisher,
}

impl Stand {
    pub fn finish(&self, outcome: Outcome) {
        self.finisher.finish(&self.app, outcome);
    }
}

/// Упорядоченные пары «ключ=значение» итога сценария.
#[derive(Default)]
pub struct Report {
    lines: Vec<(&'static str, String)>,
}

impl Report {
    pub fn push(&mut self, key: &'static str, value: impl Display) {
        self.lines.push((key, value.to_string()));
    }
}

pub enum Execution {
    Ok,
    Error(String),
}

pub struct Outcome {
    pub execution: Execution,
    pub report: Report,
}

impl Outcome {
    pub fn ok(report: Report) -> Outcome {
        Outcome {
            execution: Execution::Ok,
            report,
        }
    }

    pub fn error(reason: impl Into<String>, report: Report) -> Outcome {
        Outcome {
            execution: Execution::Error(reason.into()),
            report,
        }
    }

    pub fn render(&self) -> String {
        let mut text = String::new();
        for (key, value) in &self.report.lines {
            text.push_str(&format!("{key}={value}\n"));
        }
        match &self.execution {
            Execution::Ok => text.push_str("execution=ok\n"),
            Execution::Error(reason) => {
                text.push_str("execution=error\n");
                text.push_str(&format!("reason={reason}\n"));
            }
        }
        text
    }

    pub fn exit_code(&self) -> ExitCode {
        match self.execution {
            Execution::Ok => ExitCode::SUCCESS,
            Execution::Error(_) => ExitCode::from(EXIT_EXECUTION),
        }
    }
}

/// Записывает первый итог процесса и останавливает основной цикл, чтобы `run()` вернулся.
#[derive(Clone)]
pub struct Finisher {
    slot: Rc<RefCell<Option<Outcome>>>,
}

impl Finisher {
    pub fn new(slot: Rc<RefCell<Option<Outcome>>>) -> Finisher {
        Finisher { slot }
    }

    pub fn finish(&self, app: &NSApplication, outcome: Outcome) {
        {
            let mut slot = self.slot.borrow_mut();
            if slot.is_some() {
                return;
            }
            *slot = Some(outcome);
        }
        app.stop(None);
        // `stop:` срабатывает после очередного события, а таймер событием не является.
        let wake = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::ZERO,
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            0,
            0,
            0,
        );
        if let Some(wake) = wake {
            app.postEvent_atStart(&wake, false);
        }
    }
}

pub enum Poll<T> {
    Ready(T),
    /// Предикат ещё не выполнен; строка — прочитанное состояние для сообщения о тайм-ауте.
    Pending(String),
}

pub struct TimedOut {
    pub waited: Duration,
    pub last_state: String,
}

/// Проверяет предикат на таймере в общих режимах цикла, пока он не выполнится или не истечёт
/// `timeout`. Поток не засыпает: между проверками главный цикл обрабатывает события.
pub fn wait_until<T: 'static>(
    timeout: Duration,
    check: impl FnMut() -> Poll<T> + 'static,
    done: impl FnOnce(Result<(T, Duration), TimedOut>) + 'static,
) {
    let started = Instant::now();
    let pending = RefCell::new(Some((check, done)));
    let block = RcBlock::new(move |timer: NonNull<NSTimer>| {
        // SAFETY: таймер жив, пока вызывается его блок.
        let timer = unsafe { timer.as_ref() };
        let mut slot = pending.borrow_mut();
        let Some((check, _)) = slot.as_mut() else {
            timer.invalidate();
            return;
        };
        let result = match check() {
            Poll::Ready(value) => Ok((value, started.elapsed())),
            Poll::Pending(last_state) if started.elapsed() >= timeout => Err(TimedOut {
                waited: started.elapsed(),
                last_state,
            }),
            Poll::Pending(_) => return,
        };
        timer.invalidate();
        let taken = slot.take();
        drop(slot);
        if let Some((_, done)) = taken {
            done(result);
        }
    });
    // SAFETY: таймер добавляется в цикл главного потока и снимается там же, поэтому блок
    // вызывается и освобождается только на главном потоке.
    let timer = unsafe {
        NSTimer::timerWithTimeInterval_repeats_block(POLL_INTERVAL.as_secs_f64(), true, &block)
    };
    // SAFETY: `NSRunLoopCommonModes` — неизменяемая константа Foundation.
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
}

/// Состояние сеанса GUI из `CGSessionCopyCurrentDictionary`.
pub struct Session {
    pub on_console: bool,
    pub locked: bool,
}

impl Session {
    pub fn read() -> Result<Session, String> {
        let dict = CGSessionCopyCurrentDictionary()
            .ok_or("нет сеанса GUI: CGSessionCopyCurrentDictionary вернул NULL")?;
        // SAFETY: ключи словаря сеанса — строки, значения — объекты Core Foundation.
        let dict: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };
        let flag = |key: &'static str| {
            dict.get(&CFString::from_static_str(key))
                .and_then(|value| value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
        };
        Ok(Session {
            on_console: flag("kCGSSessionOnConsoleKey").unwrap_or(false),
            // Ключ есть только у заблокированного сеанса.
            locked: flag("CGSSessionScreenIsLocked").unwrap_or(false),
        })
    }

    pub fn check(&self) -> Result<(), String> {
        if !self.on_console {
            return Err("предусловие: сеанс GUI не на консоли".to_owned());
        }
        if self.locked {
            return Err("предусловие: экран заблокирован".to_owned());
        }
        Ok(())
    }

    pub fn add_to(&self, report: &mut Report) {
        report.push("session_on_console", self.on_console);
        report.push("session_locked", self.locked);
    }
}
