//! Перечень сценариев и общая часть стенда: итог процесса, ожидание предиката, цепочка шагов и
//! предусловия.

pub mod channel;
pub mod chord;
pub mod counters;
pub mod events;
pub mod gpu_calibration;
pub mod idle;
pub mod ime;
pub mod input;
pub mod latency;
pub mod memory;
pub mod nvim;
pub mod palette;
pub mod snapshot;
pub mod stand;
pub mod startup;
pub mod surface_env;
pub mod teardown;
pub mod visibility;
pub mod watchdog;
pub mod window;
pub mod zones;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
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
use crate::args::Route;
use crate::ghostty::init::InitRecord;
pub use crate::results::{Execution, Report};
use crate::results::{LiveObjects, StepRecord, StepStatus};
use events::SharedTrace;
use stand::StandView;

/// Код выхода при ошибке исполнения: стенд не дошёл до конца, предусловие не выполнено,
/// тайм-аут, сторож. Отрицательный вывод гипотезы ошибкой исполнения не считается.
pub const EXIT_EXECUTION: u8 = 3;

const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Window,
    SurfaceEnv,
    Startup,
    StartupLs,
    Zones,
    Input,
    Ime,
    Chord,
    Palette,
    GpuCalibration,
    Visibility,
    Teardown,
    Latency,
    Memory,
    Idle,
    Nvim,
}

impl Scenario {
    pub const ALL: [Scenario; 16] = [
        Scenario::Window,
        Scenario::SurfaceEnv,
        Scenario::Startup,
        Scenario::StartupLs,
        Scenario::Zones,
        Scenario::Input,
        Scenario::Ime,
        Scenario::Chord,
        Scenario::Palette,
        Scenario::GpuCalibration,
        Scenario::Visibility,
        Scenario::Teardown,
        Scenario::Latency,
        Scenario::Memory,
        Scenario::Idle,
        Scenario::Nvim,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Window => "window",
            Scenario::SurfaceEnv => "surface-env",
            Scenario::Startup => "startup",
            Scenario::StartupLs => "startup-ls",
            Scenario::Zones => "zones",
            Scenario::Input => "input",
            Scenario::Ime => "ime",
            Scenario::Chord => "chord",
            Scenario::Palette => "palette",
            Scenario::GpuCalibration => "gpu-calibration",
            Scenario::Visibility => "visibility",
            Scenario::Teardown => "teardown",
            Scenario::Latency => "latency",
            Scenario::Memory => "memory",
            Scenario::Idle => "idle",
            Scenario::Nvim => "nvim",
        }
    }

    /// Сценарию нужны `ghostty_init` и сокет фикстур до первого рабочего потока. `window` и
    /// варианты `bare` сценариев старта и `memory` — базовая линия без libghostty,
    /// `gpu-calibration` калибрует наблюдатель на собственных ресурсах.
    pub fn uses_ghostty(self, variant: &str) -> bool {
        match self {
            Scenario::Window | Scenario::GpuCalibration => false,
            Scenario::SurfaceEnv
            | Scenario::Zones
            | Scenario::Input
            | Scenario::Ime
            | Scenario::Chord
            | Scenario::Palette
            | Scenario::Visibility
            | Scenario::Teardown
            | Scenario::Latency
            | Scenario::Idle
            | Scenario::Nvim => true,
            Scenario::Startup | Scenario::StartupLs => variant != startup::BARE,
            Scenario::Memory => variant != memory::BARE,
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL
            .into_iter()
            .find(|scenario| scenario.name() == name)
    }

    /// Срок внутреннего сторожа от входа в `main`. Он больше суммы тайм-аутов шагов сценария,
    /// поэтому обычный тайм-аут успевает закончить сценарий своим итогом. Внешний сторож
    /// `--launch` ждёт дольше на свой запас.
    pub fn deadline(self) -> Duration {
        Duration::from_secs(match self {
            Scenario::Window => 20,
            Scenario::SurfaceEnv => 50,
            Scenario::Startup | Scenario::StartupLs => 40,
            Scenario::Zones => 300,
            Scenario::Input => 240,
            Scenario::Ime => 90,
            Scenario::Chord => 240,
            Scenario::Palette => 300,
            Scenario::GpuCalibration => 270,
            Scenario::Visibility => 480,
            // 103 цикла создать-закрыть (под MallocScribble медленнее) и пять частей закрытия.
            Scenario::Teardown => 420,
            // Четыре фазы по 110 повторов, самопроверка навигации и частота displayLink.
            Scenario::Latency => 360,
            // Три цикла 0 → 8 → 0, вариант со scrollback и до 21 серии замеров по 6 с.
            Scenario::Memory => 300,
            // До пяти окон по пять повторов по 10 с, потоки вывода и самопроверка мигания.
            Scenario::Idle => 420,
            // Запуск nvim, запись, :terminal с аккордом, ресайз и выход: шаги до 15 с.
            Scenario::Nvim => 180,
        })
    }
}

pub fn start(scenario: Scenario, stand: Rc<Stand>) {
    if let Err(reason) = events::install(
        stand.app.clone(),
        stand.finisher.clone(),
        stand.trace.clone(),
    ) {
        let report = stand.report(scenario);
        return stand.conclude(report, Err(format!("стенд событий: {reason}")));
    }
    events::note(format!("сценарий {}", scenario.name()));
    match scenario {
        Scenario::Window => window::run(stand),
        Scenario::SurfaceEnv => surface_env::run(stand),
        Scenario::Startup | Scenario::StartupLs => startup::run(stand, scenario),
        Scenario::Zones => zones::run(stand),
        Scenario::Input => input::run(stand),
        Scenario::Ime => ime::run(stand),
        Scenario::Chord => chord::run(stand),
        Scenario::Palette => palette::run(stand),
        Scenario::GpuCalibration => gpu_calibration::run(stand),
        Scenario::Visibility => visibility::run(stand),
        Scenario::Teardown => teardown::run(stand),
        Scenario::Latency => latency::run(stand),
        Scenario::Memory => memory::run(stand),
        Scenario::Idle => idle::run(stand),
        Scenario::Nvim => nvim::run(stand),
    }
}

/// Часть процесса сценария, которая выполняется до первого рабочего потока: сокет фикстур
/// привязывается сменой рабочего каталога, `ghostty_init` касается всего процесса.
pub struct GhosttyPrelude {
    pub init: InitRecord,
    pub hub: channel::Hub,
}

/// Параметры процесса сценария и то, что сделано до `NSApplication`.
pub struct Prelude {
    pub out: PathBuf,
    pub variant: String,
    /// Nonce запуска от `--launch` или собственный, если процесс запущен вручную.
    pub nonce: String,
    pub ghostty: RefCell<Option<GhosttyPrelude>>,
}

impl Prelude {
    /// Вызывается до сторожа и окна: оба создают потоки.
    pub fn prepare(
        scenario: Scenario,
        variant: &str,
        out: &Path,
        nonce: Option<String>,
    ) -> Result<Prelude, String> {
        let nonce = match nonce {
            Some(nonce) => nonce,
            None => crate::launch::nonce()?,
        };
        let ghostty = if scenario.uses_ghostty(variant) {
            let hub = channel::Hub::bind(out)?;
            let init = crate::ghostty::init::init_once(crate::PROBE_NAME)?;
            if init.code != 0 {
                return Err(format!(
                    "ghostty_init вернул {}; изменения окружения: {}",
                    init.code,
                    init.env_changes().join(", ")
                ));
            }
            Some(GhosttyPrelude { init, hub })
        } else {
            None
        };
        Ok(Prelude {
            out: out.to_path_buf(),
            variant: variant.to_owned(),
            nonce,
            ghostty: RefCell::new(ghostty),
        })
    }
}

/// Окно, на котором работает сценарий. Его строит код пробы, стенд фокус не исправляет.
pub struct Stand {
    pub app: Retained<NSApplication>,
    pub window: Retained<NSWindow>,
    pub view: Retained<StandView>,
    pub display: DisplayChoice,
    pub route: Route,
    pub activation: &'static str,
    pub finisher: Finisher,
    pub trace: SharedTrace,
    pub prelude: Prelude,
}

impl Stand {
    /// Итог сценария со стендом событий: отметки событий и счётчики живых объектов после
    /// очистки таблицы.
    pub fn conclude(&self, mut report: Report, result: Result<(), String>) {
        self.environment_to(&mut report);
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
        let outcome = match result {
            Ok(()) => Outcome::ok(report),
            Err(reason) => Outcome::error_at(reason, events::focus_state(&self.app), report),
        };
        self.finisher.finish(&self.app, outcome);
    }

    /// Окружение, которое читается у окна: дисплей, окно и источник ввода.
    fn environment_to(&self, report: &mut Report) {
        let screen = self
            .window
            .screen()
            .unwrap_or_else(|| self.display.screen.clone());
        let mut display = window::display_info(&screen, self.display.builtin);
        if screen.CGDirectDisplayID() != self.display.screen.CGDirectDisplayID() {
            display.chosen = format!("{} (окно на другом дисплее)", display.chosen);
        }
        report.display = Some(display);
        report.window = window::window_info(&self.window);
        report.input_source = window::input_source(&self.window);
    }

    /// Начало итога: сценарий, маршрут и способ активации.
    pub fn report(&self, scenario: Scenario) -> Report {
        let mut report = Report::default();
        report.push("scenario", scenario.name());
        report.push("route", self.route.name());
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
            execution: Execution::error(reason),
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
            let at = |value: Option<u64>| {
                value.map_or_else(
                    || "none".to_owned(),
                    |ns| format!("+{:.3}ms", crate::clock::interval_ms(event.t_post_ns, ns)),
                )
            };
            let _ = writeln!(
                text,
                "event=#{} {} win={} t_post={}ns dispatch={} return={} pass={}{}",
                event.number,
                event.label,
                event.window,
                event.t_post_ns,
                at(event.dispatch_start_ns),
                at(event.send_return_ns),
                at(event.display_pass_ns),
                if event.undispatched {
                    " undispatched"
                } else {
                    ""
                }
            );
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
        for finding in &report.findings {
            let _ = writeln!(
                text,
                "finding.{}={} {}: {}",
                finding.name,
                finding.verdict.name(),
                finding.requirement.name(),
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

/// Выполняет шаги по очереди. Каждое ожидание идёт на таймере в общих режимах цикла.
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
        waited_ms: waited.as_secs_f64() * 1e3,
        at_ms: events::elapsed_ms().unwrap_or(0.0),
        state,
    }
}

/// Готово, если `ready`; иначе в сообщение о тайм-ауте идёт `state()`.
pub fn poll(ready: bool, state: impl FnOnce() -> String) -> Poll<()> {
    if ready {
        Poll::Ready(())
    } else {
        Poll::Pending(state())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::{Finding, Requirement, Verdict};

    #[test]
    fn exit_code_follows_execution_not_findings() {
        let mut report = Report::default();
        report.findings.push(Finding {
            name: "budget".to_owned(),
            requirement: Requirement::Required,
            verdict: Verdict::Fail,
            reason: "превышен".to_owned(),
        });
        let ok = Outcome::ok(report);
        assert_eq!(
            format!("{:?}", ok.exit_code()),
            format!("{:?}", ExitCode::SUCCESS)
        );
        assert!(
            ok.render()
                .contains("finding.budget=fail required: превышен")
        );
        assert_eq!(
            format!(
                "{:?}",
                Outcome::error("тайм-аут", Report::default()).exit_code()
            ),
            format!("{:?}", ExitCode::from(EXIT_EXECUTION))
        );
    }

    #[test]
    fn session_precondition() {
        let session = |on_console, locked| Session { on_console, locked }.check();
        assert_eq!(session(true, false), Ok(()));
        assert!(session(false, false).is_err_and(|reason| reason.contains("консоли")));
        assert!(session(true, true).is_err_and(|reason| reason.contains("заблокирован")));
    }

    /// `run.sh` перечисляет сценарии общего прогона сам; перечень должен совпадать с
    /// `Scenario::ALL`.
    #[test]
    fn run_script_matches_scenarios() {
        let listed: Vec<String> = script_value("ALL_SCENARIOS")
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let mut sorted = listed.clone();
        sorted.sort();
        let mut expected: Vec<String> = Scenario::ALL
            .into_iter()
            .map(|scenario| scenario.name().to_owned())
            .collect();
        expected.sort();
        assert_eq!(sorted, expected);
    }

    pub fn script_value(name: &str) -> String {
        include_str!("../../run.sh")
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .map(|value| value.trim_matches('"').to_owned())
            .unwrap_or_else(|| panic!("в run.sh нет {name}"))
    }
}
