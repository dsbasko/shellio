//! Перечень сценариев и общая часть стенда: итог процесса, ожидание предиката, цепочка шагов и
//! предусловия.

pub mod counters;
pub mod events;
pub mod key_delivery;
pub mod launch;
pub mod native_menu;
pub mod parent_click;
pub mod stand;
pub mod watchdog;
mod watchdog_check;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
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
use objc2_foundation::{
    NSDefaultRunLoopMode, NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSTimer,
};

use crate::DisplayChoice;
use crate::args::{Contrast, Motion, SceneOptions};
use events::SharedTrace;

/// Код выхода при ошибке исполнения: стенд не дошёл до конца, предусловие не выполнено,
/// тайм-аут, сторож. Отрицательный вывод гипотезы ошибкой исполнения не считается.
pub const EXIT_EXECUTION: u8 = 3;

const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Launch,
    NativeMenu,
    ParentClick,
    KeyDelivery,
    /// Самопроверка сторожа: главный поток зависает, процесс обязан выйти с кодом 3.
    WatchdogCheck,
}

impl Scenario {
    pub const ALL: [Scenario; 5] = [
        Scenario::Launch,
        Scenario::NativeMenu,
        Scenario::ParentClick,
        Scenario::KeyDelivery,
        Scenario::WatchdogCheck,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Launch => "launch",
            Scenario::NativeMenu => "native-menu",
            Scenario::ParentClick => "parent-click",
            Scenario::KeyDelivery => "key-delivery",
            Scenario::WatchdogCheck => "watchdog-check",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL
            .into_iter()
            .find(|scenario| scenario.name() == name)
    }

    /// Срок сторожа от входа в `main`. Он больше суммы тайм-аутов шагов сценария, поэтому
    /// обычный тайм-аут успевает закончить сценарий своим итогом.
    pub fn deadline(self) -> Duration {
        Duration::from_secs(match self {
            Scenario::Launch => 15,
            Scenario::NativeMenu => 40,
            Scenario::ParentClick => 25,
            Scenario::KeyDelivery => 40,
            Scenario::WatchdogCheck => 3,
        })
    }
}

pub fn start(scenario: Scenario, stand: Rc<Stand>) {
    events::install(
        stand.app.clone(),
        stand.finisher.clone(),
        stand.trace.clone(),
    );
    events::note(format!("сценарий {}", scenario.name()));
    match scenario {
        Scenario::Launch => launch::run(stand),
        Scenario::NativeMenu => native_menu::run(stand),
        Scenario::ParentClick => parent_click::run(stand),
        Scenario::KeyDelivery => key_delivery::run(stand),
        Scenario::WatchdogCheck => watchdog_check::run(stand),
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
    pub trace: SharedTrace,
}

impl Stand {
    /// Итог сценария со стендом событий: отметки событий и счётчики живых объектов после
    /// очистки таблицы.
    ///
    /// Сначала закрываются незавершённые жесты: пока крутится вложенный цикл отслеживания,
    /// `stop:` основной цикл не завершит.
    pub fn conclude(&self, mut report: Report, result: Result<(), String>) {
        let app = self.app.clone();
        let finisher = self.finisher.clone();
        events::close_then(move |unclosed| {
            events::clear();
            events::add_to(&mut report);
            for count in counters::snapshot() {
                report.push(
                    "live_objects",
                    format!(
                        "{} created={} released={} live={}",
                        count.name,
                        count.created,
                        count.released,
                        count.live()
                    ),
                );
            }
            let result = match (result, unclosed) {
                (Ok(()), Some(unclosed)) => Err(format!("жест не закрыт: {unclosed}")),
                (result, unclosed) => {
                    if let Some(unclosed) = unclosed {
                        report.push("unclosed_dispatch", unclosed);
                    }
                    result
                }
            };
            let outcome = match result {
                Ok(()) => Outcome::ok(report),
                Err(reason) => Outcome::error(reason, report),
            };
            finisher.finish(&app, outcome);
        });
    }

    /// Начало итога: сценарий и настройки сцены.
    pub fn report(&self, scenario: Scenario) -> Report {
        let mut report = Report::default();
        report.push("scenario", scenario.name());
        report.push("split", self.scene.split.name());
        report.push(
            "contrast",
            match self.scene.contrast {
                None => "system",
                Some(Contrast::Normal) => "normal",
                Some(Contrast::More) => "more",
            },
        );
        report.push(
            "motion",
            match self.scene.motion {
                None => "system",
                Some(Motion::Normal) => "normal",
                Some(Motion::Reduced) => "reduced",
            },
        );
        report.push("activation", self.activation);
        report
    }

    /// Предусловие сеанса: на консоли и не заблокирован.
    pub fn check_session(&self, report: &mut Report) -> Result<(), String> {
        let session = Session::read()?;
        session.add_to(report);
        session.check()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    PendingManual,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::PendingManual => "pending-manual",
        }
    }
}

/// Вывод гипотезы. Он отделён от исполнения: `fail` и `pending-manual` — законченные
/// наблюдения.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub name: &'static str,
    pub verdict: Verdict,
    pub reason: String,
}

/// Упорядоченные пары «ключ=значение» итога сценария и выводы.
#[derive(Default)]
pub struct Report {
    lines: Vec<(&'static str, String)>,
    findings: Vec<Finding>,
}

impl Report {
    pub fn push(&mut self, key: &'static str, value: impl Display) {
        self.lines.push((key, value.to_string()));
    }

    pub fn finding(&mut self, name: &'static str, verdict: Verdict, reason: impl Into<String>) {
        self.findings.push(Finding {
            name,
            verdict,
            reason: reason.into(),
        });
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
        for finding in &self.report.findings {
            text.push_str(&format!(
                "finding.{}={}: {}\n",
                finding.name,
                finding.verdict.name(),
                finding.reason
            ));
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

/// Шаг сценария: ожидание фактического состояния, затем действие, например отправка событий.
pub struct Step {
    name: &'static str,
    timeout: Duration,
    ready: Box<dyn FnMut() -> Poll<()>>,
    act: Box<dyn FnOnce() -> Result<(), String>>,
}

impl Step {
    pub fn new(
        name: &'static str,
        timeout: Duration,
        ready: impl FnMut() -> Poll<()> + 'static,
        act: impl FnOnce() -> Result<(), String> + 'static,
    ) -> Step {
        Step {
            name,
            timeout,
            ready: Box::new(ready),
            act: Box::new(act),
        }
    }
}

pub enum StepError {
    TimedOut(TimedOut),
    Action(String),
}

pub struct StepFailure {
    pub step: &'static str,
    pub index: usize,
    pub error: StepError,
}

impl StepFailure {
    pub fn describe(&self) -> String {
        match &self.error {
            StepError::TimedOut(timeout) => format!(
                "шаг «{}»: нет состояния за {} мс; последнее состояние: {}",
                self.step,
                timeout.waited.as_millis(),
                timeout.last_state
            ),
            StepError::Action(reason) => format!("шаг «{}»: {reason}", self.step),
        }
    }
}

/// Выполняет шаги по очереди. Каждое ожидание идёт на таймере в общих режимах цикла, поэтому
/// продолжение жеста отправляется и во вложенном цикле отслеживания.
pub fn run_steps(steps: Vec<Step>, done: impl FnOnce(Result<(), StepFailure>) + 'static) {
    run_from(steps.into(), 0, Box::new(done));
}

type StepsDone = Box<dyn FnOnce(Result<(), StepFailure>)>;

fn run_from(mut steps: VecDeque<Step>, index: usize, done: StepsDone) {
    let Some(step) = steps.pop_front() else {
        return done(Ok(()));
    };
    let Step {
        name,
        timeout,
        ready,
        act,
    } = step;
    wait_until(timeout, ready, move |result| match result {
        Ok(((), waited)) => {
            events::note(format!(
                "шаг «{name}» готов за {:.1} мс",
                waited.as_secs_f64() * 1e3
            ));
            match act() {
                Ok(()) => run_from(steps, index + 1, done),
                Err(reason) => {
                    events::note(format!("шаг «{name}»: ошибка действия: {reason}"));
                    done(Err(StepFailure {
                        step: name,
                        index,
                        error: StepError::Action(reason),
                    }))
                }
            }
        }
        Err(timeout) => {
            events::note(format!("шаг «{name}»: тайм-аут, {}", timeout.last_state));
            done(Err(StepFailure {
                step: name,
                index,
                error: StepError::TimedOut(timeout),
            }))
        }
    });
}

/// Срок, за который менеджер окон обязан перестать двигать окна сценария.
pub const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
/// Окна считаются неподвижными, если их кадры не менялись столько времени.
const SETTLE_QUIET: Duration = Duration::from_millis(500);

/// Ожидание, пока внешний менеджер окон (например, AeroSpace) перестанет двигать окна. Стенд
/// окна не возвращает на место: жест мыши начинается только в той геометрии, которая
/// установилась, а сдвиг окна посреди жеста остаётся ошибкой исполнения.
pub struct Settle {
    windows: Vec<Retained<NSWindow>>,
    initial: Vec<NSRect>,
    last: RefCell<(Vec<NSRect>, Instant)>,
    changes: Cell<usize>,
}

impl Settle {
    pub fn new(windows: Vec<Retained<NSWindow>>) -> Rc<Settle> {
        let initial: Vec<NSRect> = windows.iter().map(|window| window.frame()).collect();
        Rc::new(Settle {
            windows,
            last: RefCell::new((initial.clone(), Instant::now())),
            initial,
            changes: Cell::new(0),
        })
    }

    fn frames(&self) -> Vec<NSRect> {
        self.windows.iter().map(|window| window.frame()).collect()
    }

    /// Предикат шага; вызывается на каждой проверке, чтобы заметить каждый сдвиг.
    pub fn poll(&self) -> Poll<()> {
        let frames = self.frames();
        let mut last = self.last.borrow_mut();
        if frames != last.0 {
            self.changes.set(self.changes.get() + 1);
            events::note(format!(
                "кадры окон сдвинулись: {}",
                describe_frames(&frames)
            ));
            *last = (frames, Instant::now());
        }
        let quiet = last.1.elapsed();
        poll(quiet >= SETTLE_QUIET, || {
            format!(
                "кадры окон {} неподвижны {} мс",
                describe_frames(&last.0),
                quiet.as_millis()
            )
        })
    }

    pub fn add_to(&self, report: &mut Report) {
        report.push("window_frames_initial", describe_frames(&self.initial));
        report.push(
            "window_frames_settled",
            describe_frames(&self.last.borrow().0),
        );
        report.push("window_frame_changes", self.changes.get());
    }
}

fn describe_frames(frames: &[NSRect]) -> String {
    frames
        .iter()
        .map(|frame| events::describe_rect(*frame))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Готово, если `ready`; иначе в сообщение о тайм-ауте идёт `state()`.
pub fn poll(ready: bool, state: impl FnOnce() -> String) -> Poll<()> {
    if ready {
        Poll::Ready(())
    } else {
        Poll::Pending(state())
    }
}

/// Имя основного режима цикла.
pub fn default_mode() -> String {
    // SAFETY: `NSDefaultRunLoopMode` — неизменяемая константа Foundation.
    unsafe { NSDefaultRunLoopMode }.to_string()
}

/// Текущий режим цикла главного потока.
pub fn run_loop_mode() -> String {
    NSRunLoop::currentRunLoop()
        .currentMode()
        .map_or_else(|| "none".to_owned(), |mode| mode.to_string())
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
