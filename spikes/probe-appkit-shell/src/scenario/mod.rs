//! Перечень сценариев и общая часть стенда: итог процесса, ожидание предиката, цепочка шагов и
//! предусловия.

pub mod counters;
mod cycles;
pub mod events;
mod frames;
mod idle;
pub mod key_delivery;
pub mod launch;
mod list;
pub mod material;
mod menu;
pub mod native_menu;
mod palette;
pub mod parent_click;
pub mod passes;
pub mod readback;
mod readback_check;
pub mod sampler;
pub mod snapshot;
mod split;
pub mod stand;
mod startup;
mod tabs;
mod theme;
mod tree;
pub mod watchdog;
mod watchdog_check;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::PathBuf;
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
use crate::args::SceneOptions;
pub use crate::results::{Execution, Report, Verdict};
use crate::results::{LiveObjects, StepRecord, StepStatus};
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
    /// Самопроверка чтения состояния, прохода отображения и снимков на известных ответах.
    ReadbackCheck,
    /// Самопроверка сторожа: главный поток зависает, процесс обязан выйти с кодом 3.
    WatchdogCheck,
    Split,
    Tabs,
    List,
    Tree,
    Palette,
    Menu,
    Theme,
    Startup,
    Idle,
    Frames,
    Cycles,
}

impl Scenario {
    pub const ALL: [Scenario; 17] = [
        Scenario::Launch,
        Scenario::NativeMenu,
        Scenario::ParentClick,
        Scenario::KeyDelivery,
        Scenario::ReadbackCheck,
        Scenario::WatchdogCheck,
        Scenario::Split,
        Scenario::Tabs,
        Scenario::List,
        Scenario::Tree,
        Scenario::Palette,
        Scenario::Menu,
        Scenario::Theme,
        Scenario::Startup,
        Scenario::Idle,
        Scenario::Frames,
        Scenario::Cycles,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Launch => "launch",
            Scenario::NativeMenu => "native-menu",
            Scenario::ParentClick => "parent-click",
            Scenario::KeyDelivery => "key-delivery",
            Scenario::ReadbackCheck => "readback-check",
            Scenario::WatchdogCheck => "watchdog-check",
            Scenario::Split => "split",
            Scenario::Tabs => "tabs",
            Scenario::List => "list",
            Scenario::Tree => "tree",
            Scenario::Palette => "palette",
            Scenario::Menu => "menu",
            Scenario::Theme => "theme",
            Scenario::Startup => "startup",
            Scenario::Idle => "idle",
            Scenario::Frames => "frames",
            Scenario::Cycles => "cycles",
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
            Scenario::ReadbackCheck => 45,
            Scenario::WatchdogCheck => 3,
            Scenario::Split => 150,
            Scenario::Tabs => 150,
            Scenario::List => 300,
            Scenario::Tree => 150,
            Scenario::Palette => 300,
            Scenario::Menu => 90,
            Scenario::Theme => 90,
            Scenario::Startup => 20,
            Scenario::Idle => 120,
            Scenario::Frames => frames::PHASES_TIMEOUT.as_secs() + 120,
            Scenario::Cycles => 420,
        })
    }

    /// Сценарию нужна собранная сцена; стендовые сценарии работают на голом окне.
    pub fn needs_scene(self) -> bool {
        match self {
            Scenario::Split
            | Scenario::Tabs
            | Scenario::List
            | Scenario::Tree
            | Scenario::Palette
            | Scenario::Menu
            | Scenario::Theme
            | Scenario::Startup
            | Scenario::Frames => true,
            // Пустое окно — первое состояние памяти, сцену `idle` собирает сам. `cycles` тоже
            // собирает сцену сам, чтобы закрыть её окно и освободить сцену.
            Scenario::Idle
            | Scenario::Cycles
            | Scenario::Launch
            | Scenario::NativeMenu
            | Scenario::ParentClick
            | Scenario::KeyDelivery
            | Scenario::ReadbackCheck
            | Scenario::WatchdogCheck => false,
        }
    }
}

pub fn start(scenario: Scenario, stand: Rc<Stand>) {
    if let Err(reason) = events::install(
        stand.app.clone(),
        stand.finisher.clone(),
        stand.trace.clone(),
    ) {
        let report = stand.report(scenario);
        return stand.conclude(
            report,
            Err(format!("наблюдатель прохода отображения: {reason}")),
        );
    }
    events::note(format!("сценарий {}", scenario.name()));
    match scenario {
        Scenario::Launch => launch::run(stand),
        Scenario::NativeMenu => native_menu::run(stand),
        Scenario::ParentClick => parent_click::run(stand),
        Scenario::KeyDelivery => key_delivery::run(stand),
        Scenario::ReadbackCheck => readback_check::run(stand),
        Scenario::WatchdogCheck => watchdog_check::run(stand),
        Scenario::Split => split::run(stand),
        Scenario::Tabs => tabs::run(stand),
        Scenario::List => list::run(stand),
        Scenario::Tree => tree::run(stand),
        Scenario::Palette => palette::run(stand),
        Scenario::Menu => menu::run(stand),
        Scenario::Theme => theme::run(stand),
        Scenario::Startup => startup::run(stand),
        Scenario::Idle => idle::run(stand),
        Scenario::Frames => frames::run(stand),
        Scenario::Cycles => cycles::run(stand),
    }
}

/// Сцена, на которой работает сценарий. Её строит код сцены, стенд фокус не исправляет.
pub struct Stand {
    pub app: Retained<NSApplication>,
    pub window: Retained<NSWindow>,
    pub display: DisplayChoice,
    pub scene: SceneOptions,
    /// Собранная сцена, если сценарию она нужна (`Scenario::needs_scene`).
    pub built_scene: Option<Rc<crate::scene::Scene>>,
    pub activation: &'static str,
    pub finisher: Finisher,
    pub trace: SharedTrace,
    /// Каталог `--out` процесса: снимки и `result.json`.
    pub out: PathBuf,
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
        self.environment_to(&mut report);
        events::close_then(move |unclosed| {
            events::clear();
            events::add_to(&mut report);
            report.diagnostics.live_objects = counters::snapshot()
                .into_iter()
                .map(|count| LiveObjects {
                    class: count.name.to_owned(),
                    created: count.created,
                    released: count.released,
                    live: count.live(),
                })
                .collect();
            let result = match (result, unclosed) {
                (Ok(()), Some(unclosed)) => Err(format!("жест не закрыт: {unclosed}")),
                (result, unclosed) => {
                    report.diagnostics.unclosed_dispatch = unclosed;
                    result
                }
            };
            let outcome = match result {
                Ok(()) => Outcome::ok(report),
                Err(reason) => Outcome::error_at(reason, events::focus_state(&app), report),
            };
            finisher.finish(&app, outcome);
        });
    }

    /// Окружение, которое читается у окна: дисплей, окно и источник ввода.
    fn environment_to(&self, report: &mut Report) {
        let screen = self
            .window
            .screen()
            .unwrap_or_else(|| self.display.screen.clone());
        let mut display = readback::display_info(&screen, self.display.builtin);
        if screen.CGDirectDisplayID() != self.display.screen.CGDirectDisplayID() {
            display.chosen = format!("{} (окно на другом дисплее)", display.chosen);
        }
        report.display = Some(display);
        report.window = readback::window_info(&self.window);
        report.input_source = launch::input_source(&self.window);
    }

    /// Начало итога: сценарий и настройки сцены.
    pub fn report(&self, scenario: Scenario) -> Report {
        let mut report = Report::default();
        report.push("scenario", scenario.name());
        report.push("split", self.scene.split.name());
        report.push("contrast", self.scene.contrast_name());
        report.push("motion", self.scene.motion_name());
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
            execution: Execution::Error {
                reason: reason.into(),
                last_state: None,
            },
            report,
        }
    }

    /// Ошибка исполнения с состоянием AppKit, прочитанным в момент ошибки.
    pub fn error_at(reason: impl Into<String>, last_state: String, report: Report) -> Outcome {
        Outcome {
            execution: Execution::Error {
                reason: reason.into(),
                last_state: Some(last_state),
            },
            report,
        }
    }

    /// Текстовый итог для stdout; полный итог — `result.json`.
    pub fn render(&self) -> String {
        let report = &self.report;
        let mut text = String::new();
        for observation in &report.observations {
            match &observation.value {
                serde_json::Value::String(value) => {
                    let _ = writeln!(text, "{}={value}", observation.key);
                }
                value => {
                    let _ = writeln!(text, "{}={value}", observation.key);
                }
            }
        }
        for step in &report.steps {
            let _ = writeln!(
                text,
                "step.{}={:?} {:.1}ms at={:.1}ms [{}]",
                step.index, step.status, step.waited_ms, step.at_ms, step.name
            );
        }
        for event in &report.events {
            let _ = writeln!(text, "event={}", render_event(event));
        }
        for line in &report.diagnostics.appkit_internal_events {
            let _ = writeln!(text, "appkit_internal_event={line}");
        }
        for count in &report.diagnostics.live_objects {
            let _ = writeln!(
                text,
                "live_objects={} created={} released={} live={}",
                count.class, count.created, count.released, count.live
            );
        }
        if let Some(unclosed) = &report.diagnostics.unclosed_dispatch {
            let _ = writeln!(text, "unclosed_dispatch={unclosed}");
        }
        for metric in &report.metrics {
            match &metric.summary {
                Some(summary) => {
                    let _ = writeln!(
                        text,
                        "metric.{}=p50 {:.3} p95 {:.3} max {:.3} {} (n={})",
                        metric.name,
                        summary.p50,
                        summary.p95,
                        summary.max,
                        metric.unit,
                        summary.count
                    );
                }
                None => {
                    let _ = writeln!(
                        text,
                        "metric.{}={}",
                        metric.name,
                        metric.summary_error.as_deref().unwrap_or("нет замеров")
                    );
                }
            }
        }
        for snapshot in &report.snapshots {
            let _ = writeln!(
                text,
                "snapshot.{}={:?} {:?} {}",
                snapshot.name,
                snapshot.kind,
                snapshot.status,
                snapshot
                    .file
                    .as_deref()
                    .or(snapshot.reason.as_deref())
                    .unwrap_or("")
            );
        }
        for finding in &report.findings {
            let _ = writeln!(
                text,
                "finding.{}={}: {}",
                finding.name,
                finding.verdict.name(),
                finding.reason
            );
        }
        match &self.execution {
            Execution::Ok => text.push_str("execution=ok\n"),
            Execution::Error { reason, last_state } => {
                text.push_str("execution=error\n");
                let _ = writeln!(text, "reason={reason}");
                if let Some(state) = last_state {
                    let _ = writeln!(text, "last_state={state}");
                }
            }
        }
        text
    }

    pub fn exit_code(&self) -> ExitCode {
        match self.execution {
            Execution::Ok => ExitCode::SUCCESS,
            Execution::Error { .. } => ExitCode::from(EXIT_EXECUTION),
        }
    }
}

fn render_event(event: &crate::results::EventRecord) -> String {
    let at =
        |value: Option<f64>| value.map_or_else(|| "none".to_owned(), |ms| format!("{ms:.3}ms"));
    let dispatch = match event.dispatch_start_ms {
        Some(ms) => format!("{ms:.3}ms"),
        None if event.undispatched => "none(вложенный цикл)".to_owned(),
        None => "none".to_owned(),
    };
    format!(
        "#{} {} win={} posted={:.3}ms dispatch={dispatch} return={} pass={} before[{}]",
        event.number,
        event.label,
        event.window,
        event.posted_ms,
        at(event.send_return_ms),
        at(event.display_pass_ms),
        event.before
    )
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
            let state = events::current_state();
            let mut record = step_record(index, name, StepStatus::Ready, waited, state);
            match act() {
                Ok(()) => {
                    events::step(record);
                    run_from(steps, index + 1, done)
                }
                Err(reason) => {
                    events::note(format!("шаг «{name}»: ошибка действия: {reason}"));
                    record.status = StepStatus::ActionError;
                    record.state = format!("{}; ошибка действия: {reason}", record.state);
                    events::step(record);
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
            events::step(step_record(
                index,
                name,
                StepStatus::Timeout,
                timeout.waited,
                timeout.last_state.clone(),
            ));
            done(Err(StepFailure {
                step: name,
                index,
                error: StepError::TimedOut(timeout),
            }))
        }
    });
}

fn step_record(
    index: usize,
    name: &str,
    status: StepStatus,
    waited: Duration,
    state: String,
) -> StepRecord {
    StepRecord {
        index,
        name: name.to_owned(),
        status,
        waited_ms: events::to_ms(waited),
        at_ms: events::elapsed().map_or(0.0, events::to_ms),
        state,
    }
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
