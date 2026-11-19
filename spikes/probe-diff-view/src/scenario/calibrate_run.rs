//! GUI-часть сценария `calibrate`: содержимое окна — [`MotionView`], его полосы движутся по S1 от
//! `displayLink` окна. Вызов `displayLink` ставит сдвиг для своего `targetTimestamp`, а не
//! прибавляет шаг. Четыре отрезка идут строго по одному: рабочий поток запускает запись
//! `xctrace`, главный ведёт стимул отрезка, затем рабочий поток останавливает запись, выгружает и
//! разбирает таблицы. Главный поток только опрашивает канал. Чистая часть — `calibrate.rs`.

use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::NSWindow;
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes};
use objc2_quartz_core::CADisplayLink;
use serde::Serialize;

use super::calibrate::{
    self, CaptureSegment, Exported, MEASURE_S, PERIOD_NS, Reverse, Segment, TAIL_S, WARMUP_TICKS,
    Window,
};
use super::capture::Capture;
use super::drawmark::{self, Change, Covered, Mark};
use super::frames::{self, Recorder};
use super::neighbor;
use super::sampler;
use super::stand::{FrameDriver, MotionView, s1_offset};
use crate::results::{Report, Verdict};

/// Скорость S1, точки в секунду.
pub const SPEED: f64 = 4800.0;

/// Ошибка исполнения калибровки с тем, что успело попасть в итог.
pub type Failed = Box<(String, Report)>;

/// Стимул отрезка калибровки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Stimulus {
    /// Прямой контроль: удержание на тактах окна замера 120–122 и 300–309.
    Holds,
    /// Прямой контроль без удержания.
    Clean,
    /// Обратный контроль: поколение удержано весь отрезок, сосед движется.
    Frozen,
    /// Наблюдение: занятое ожидание главного потока на тактах окна 120 и 300.
    Stall,
}

impl Stimulus {
    pub fn name(self) -> &'static str {
        match self {
            Stimulus::Holds => "holds",
            Stimulus::Clean => "clean",
            Stimulus::Frozen => "frozen",
            Stimulus::Stall => "stall",
        }
    }
}

/// Отрезки в порядке записи.
pub const SEGMENTS: [Stimulus; 4] = [
    Stimulus::Holds,
    Stimulus::Clean,
    Stimulus::Frozen,
    Stimulus::Stall,
];

/// Путь замера кадров по правилу «Способ замера кадров».
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum FramesPath {
    Xctrace,
    Capture,
}

impl FramesPath {
    pub fn name(self) -> &'static str {
        match self {
            FramesPath::Xctrace => "xctrace",
            FramesPath::Capture => "screencapturekit",
        }
    }
}

/// Переменная окружения разработки: пути калибровки через запятую, по умолчанию оба по порядку.
/// Отрезки ScreenCaptureKit идут без записи `xctrace`.
pub const PATHS_ENV: &str = "PROBE_DIFF_CALIBRATE_PATHS";

/// Отрезки процесса: пути из [`PATHS_ENV`], у каждого четыре стимула.
pub fn runs(paths: Option<&str>) -> Result<Vec<(FramesPath, Stimulus)>, String> {
    let paths: Vec<FramesPath> = match paths {
        None => vec![FramesPath::Xctrace, FramesPath::Capture],
        Some(list) => list
            .split(',')
            .map(|name| match name.trim() {
                "xctrace" => Ok(FramesPath::Xctrace),
                "screencapturekit" => Ok(FramesPath::Capture),
                other => Err(format!("{PATHS_ENV}: путь «{other}» не известен")),
            })
            .collect::<Result<_, _>>()?,
    };
    Ok(paths
        .into_iter()
        .flat_map(|path| SEGMENTS.map(|stimulus| (path, stimulus)))
        .collect())
}

/// Задержки главного потока: такт окна замера и длительность в периодах.
pub const STALLS: [(u64, u64); 2] = [(120, 3), (300, 10)];

/// Вызов `displayLink`: метки CA в секундах (часы `CLOCK_UPTIME_RAW`), вход — наносекунды. `index`
/// — номер такта отрезка по времени, `held` — вид удержал поколение.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Tick {
    pub index: u64,
    pub held: bool,
    pub timestamp_s: f64,
    pub target_s: f64,
    pub entry_ns: u64,
    pub generation: u64,
}

#[derive(Default)]
struct State {
    origin_s: Cell<Option<f64>>,
    stimulus: Cell<Option<Stimulus>>,
    start_s: Cell<Option<f64>>,
    stalls: Cell<usize>,
    ticks: RefCell<Vec<Tick>>,
    changes: RefCell<Vec<Change>>,
    ended: Cell<bool>,
}

/// Итог отрезка с рабочего потока.
#[derive(Clone, Debug, Serialize)]
pub struct SegmentOutcome {
    pub segment: Segment,
    pub reverse: Option<Reverse>,
    pub neighbor_ticks: Option<usize>,
}

/// Итог отрезка ScreenCaptureKit.
#[derive(Clone, Debug, Serialize)]
pub struct CaptureOutcome {
    pub segment: CaptureSegment,
    pub reverse: Option<Reverse>,
    pub neighbor_ticks: Option<usize>,
}

enum Outcome {
    Xctrace(Box<SegmentOutcome>),
    Capture(Box<CaptureOutcome>),
    /// Запись отрезка-наблюдения `stall` не разобрана: отметка пути, а не ошибка процесса.
    Unanalyzed(String),
}

/// Неразобранный отрезок `stall` пути: причина и такты `displayLink`.
pub struct Unanalyzed {
    pub path: FramesPath,
    pub reason: String,
    pub ticks: Vec<Tick>,
}

/// Итог разбора записи отрезка. Ошибка разбора `stall` — наблюдение: путь из процесса не
/// выбирается, его проверки всех отрезков получают `not_run` (отчёт, раздел «Ошибка разбора
/// отрезка `stall`»). Ошибка разбора отрезка с вердиктом — ошибка исполнения.
fn observed<T>(
    stimulus: Stimulus,
    analysis: Result<T, String>,
) -> Result<Result<T, String>, String> {
    match (stimulus, analysis) {
        (_, Ok(segment)) => Ok(Ok(segment)),
        (Stimulus::Stall, Err(reason)) => Ok(Err(reason)),
        (Stimulus::Holds | Stimulus::Clean | Stimulus::Frozen, Err(reason)) => Err(reason),
    }
}

fn unanalyzed_reason(unanalyzed: &[Unanalyzed], path: FramesPath) -> Option<&str> {
    unanalyzed
        .iter()
        .find(|segment| segment.path == path)
        .map(|segment| segment.reason.as_str())
}

enum FromWorker {
    Started,
    Done(Result<Outcome, String>),
}

struct Stop {
    window: Window,
    held_targets_ns: Vec<u64>,
    ticks: Vec<Tick>,
}

struct Worker {
    to: Sender<Stop>,
    from: Receiver<FromWorker>,
    handle: JoinHandle<()>,
}

enum Phase {
    Next(usize),
    Starting(usize, Worker),
    Running(usize, Worker),
    Saving(usize, Worker),
    Finished,
}

/// Идущая калибровка: вид, цель и ссылка `displayLink` живут до [`Calibration::poll`] с итогом.
pub struct Calibration {
    runs: Vec<(FramesPath, Stimulus)>,
    window_id: u32,
    window_w: f64,
    window_h: f64,
    view: Retained<MotionView>,
    driver: Retained<FrameDriver>,
    link: Retained<CADisplayLink>,
    state: Rc<State>,
    out: PathBuf,
    phase: RefCell<Phase>,
    outcomes: RefCell<Vec<(Stimulus, SegmentOutcome, Vec<Tick>)>>,
    captures: RefCell<Vec<(Stimulus, CaptureOutcome, Vec<Tick>)>>,
    unanalyzed: RefCell<Vec<Unanalyzed>>,
    /// Такты отрезка, который сохраняет рабочий поток.
    pending: RefCell<Vec<Tick>>,
    report: RefCell<Option<Report>>,
}

fn now_ns() -> u64 {
    sampler::uptime_ns().unwrap_or(0)
}

impl Calibration {
    /// Ставит вид содержимым окна и запускает `displayLink`. `report` — итог готового окна, `out`
    /// — каталог процесса, трассы и выгрузки отрезков пишутся в его подкаталоги.
    pub fn start(
        mtm: MainThreadMarker,
        window: &NSWindow,
        mut report: Report,
        out: &Path,
    ) -> Result<Calibration, Failed> {
        let runs = match runs(std::env::var(PATHS_ENV).ok().as_deref()) {
            Ok(runs) => runs,
            Err(reason) => return Err(Box::new((reason, report))),
        };
        match sampler::check_peak() {
            Ok(check) => {
                match &check.failure {
                    None => report.finding(
                        "peak_counter",
                        Verdict::Pass,
                        format!(
                            "ri_lifetime_max_phys_footprint вырос на {} байт при выделении {}",
                            check.touched.lifetime_max_bytes - check.before.lifetime_max_bytes,
                            check.control_bytes
                        ),
                    ),
                    Some(reason) => report.finding("peak_counter", Verdict::NotRun, reason.clone()),
                }
                report.push(
                    "peak_check",
                    serde_json::to_string(&check).unwrap_or_default(),
                );
            }
            Err(reason) => return Err(Box::new((reason, report))),
        }
        let bounds = window
            .contentView()
            .map(|view| view.bounds())
            .unwrap_or_else(|| window.contentLayoutRect());
        let view = MotionView::new(mtm, bounds);
        view.show_marker();
        window.setContentView(Some(&view));
        let frame = window.frame();
        let state = Rc::new(State::default());
        drawmark::take();
        let handler_state = Rc::clone(&state);
        let handler_view = view.clone();
        let driver = FrameDriver::new(
            mtm,
            Box::new(move |link| tick(&handler_state, &handler_view, link)),
        );
        // SAFETY: цель — объект пробы с методом `tick:`, принимающим `CADisplayLink`.
        let link = unsafe { view.displayLinkWithTarget_selector(&driver, sel!(tick:)) };
        // SAFETY: ссылка добавляется в цикл главного потока в общих режимах.
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::currentRunLoop(), NSRunLoopCommonModes) };
        Ok(Calibration {
            window_id: window.windowNumber() as u32,
            window_w: frame.size.width,
            window_h: frame.size.height,
            runs,
            view,
            driver,
            link,
            state,
            out: out.to_path_buf(),
            phase: RefCell::new(Phase::Next(0)),
            outcomes: RefCell::new(Vec::new()),
            captures: RefCell::new(Vec::new()),
            unanalyzed: RefCell::new(Vec::new()),
            pending: RefCell::new(Vec::new()),
            report: RefCell::new(Some(report)),
        })
    }

    /// Проверка с таймера стенда. Итог — когда все отрезки разобраны или один упал.
    pub fn poll(&self) -> Option<Result<Report, Failed>> {
        let phase = std::mem::replace(&mut *self.phase.borrow_mut(), Phase::Finished);
        let next = match self.step(phase) {
            Ok(next) => next,
            Err(reason) => return Some(Err(self.fail(reason))),
        };
        let finished = matches!(next, Phase::Finished);
        *self.phase.borrow_mut() = next;
        finished.then(|| Ok(self.finish()))
    }

    fn step(&self, phase: Phase) -> Result<Phase, String> {
        Ok(match phase {
            Phase::Next(index) if index < self.runs.len() => {
                let (path, stimulus) = self.runs[index];
                let dir = match path {
                    FramesPath::Xctrace => self.out.join(format!("calibrate-{}", stimulus.name())),
                    FramesPath::Capture => {
                        self.out.join(format!("calibrate-sck-{}", stimulus.name()))
                    }
                };
                let target = Target {
                    pid: std::process::id(),
                    window_id: self.window_id,
                    window_w: self.window_w,
                    window_h: self.window_h,
                };
                Phase::Starting(index, spawn_worker(path, stimulus, target, dir)?)
            }
            Phase::Next(_) | Phase::Finished => Phase::Finished,
            Phase::Starting(index, worker) => match worker.from.try_recv() {
                Ok(FromWorker::Started) => {
                    let state = &self.state;
                    state.start_s.set(None);
                    state.stalls.set(0);
                    state.ticks.borrow_mut().clear();
                    state.ended.set(false);
                    state.stimulus.set(Some(self.runs[index].1));
                    Phase::Running(index, worker)
                }
                Ok(FromWorker::Done(result)) => return Err(self.segment_error(index, result)),
                Err(TryRecvError::Empty) => Phase::Starting(index, worker),
                Err(TryRecvError::Disconnected) => {
                    return Err(format!("отрезок {}: рабочий поток упал", self.label(index)));
                }
            },
            Phase::Running(index, worker) => {
                if !self.state.ended.get() {
                    return Ok(Phase::Running(index, worker));
                }
                let ticks = self.state.ticks.take();
                let start_s = self
                    .state
                    .start_s
                    .get()
                    .ok_or("отрезок закончился без тактов")?;
                let warmup_s = WARMUP_TICKS as f64 / calibrate::RATE_HZ;
                let window = Window {
                    start_ns: ((start_s + warmup_s) * 1e9).round() as u64,
                    end_ns: ((start_s + warmup_s + MEASURE_S) * 1e9).round() as u64,
                };
                let held_targets_ns = ticks
                    .iter()
                    .filter(|tick| tick.held && self.runs[index].1 == Stimulus::Holds)
                    .map(|tick| (tick.target_s * 1e9).round() as u64)
                    .collect();
                worker
                    .to
                    .send(Stop {
                        window,
                        held_targets_ns,
                        ticks: ticks.clone(),
                    })
                    .map_err(|_| "рабочий поток отрезка не принимает остановку".to_owned())?;
                *self.pending.borrow_mut() = ticks;
                Phase::Saving(index, worker)
            }
            Phase::Saving(index, worker) => match worker.from.try_recv() {
                Ok(FromWorker::Done(Ok(outcome))) => {
                    let _ = worker.handle.join();
                    let ticks = self.pending.take();
                    let (path, stimulus) = self.runs[index];
                    match outcome {
                        Outcome::Xctrace(outcome) => {
                            self.outcomes.borrow_mut().push((stimulus, *outcome, ticks))
                        }
                        Outcome::Capture(outcome) => {
                            self.captures.borrow_mut().push((stimulus, *outcome, ticks))
                        }
                        Outcome::Unanalyzed(reason) => {
                            self.unanalyzed.borrow_mut().push(Unanalyzed {
                                path,
                                reason,
                                ticks,
                            })
                        }
                    }
                    Phase::Next(index + 1)
                }
                Ok(FromWorker::Done(result)) => return Err(self.segment_error(index, result)),
                Ok(FromWorker::Started) => Phase::Saving(index, worker),
                Err(TryRecvError::Empty) => Phase::Saving(index, worker),
                Err(TryRecvError::Disconnected) => {
                    return Err(format!("отрезок {}: рабочий поток упал", self.label(index)));
                }
            },
        })
    }

    fn label(&self, index: usize) -> String {
        let (path, stimulus) = self.runs[index];
        format!("{} {}", path.name(), stimulus.name())
    }

    fn segment_error(&self, index: usize, result: Result<Outcome, String>) -> String {
        let name = self.label(index);
        match result {
            Ok(_) => format!("отрезок {name}: итог без начала записи"),
            Err(reason) => format!("отрезок {name}: {reason}"),
        }
    }

    fn stop_link(&self) {
        self.link.invalidate();
        self.driver.clear();
    }

    fn fail(&self, reason: String) -> Failed {
        self.stop_link();
        let mut report = self.report.borrow_mut().take().unwrap_or_default();
        record_segments(&mut report, &self.outcomes.borrow());
        record_captures(&mut report, &self.captures.borrow());
        record_unanalyzed(&mut report, &self.unanalyzed.borrow());
        Box::new((reason, report))
    }

    fn finish(&self) -> Report {
        self.stop_link();
        let mut report = self.report.borrow_mut().take().unwrap_or_default();
        let outcomes = self.outcomes.borrow();
        let captures = self.captures.borrow();
        let unanalyzed = self.unanalyzed.borrow();
        record_segments(&mut report, &outcomes);
        record_captures(&mut report, &captures);
        record_unanalyzed(&mut report, &unanalyzed);
        let paths: Vec<FramesPath> = {
            let mut paths: Vec<FramesPath> = self.runs.iter().map(|(path, _)| *path).collect();
            paths.dedup();
            paths
        };
        let xctrace = paths.contains(&FramesPath::Xctrace).then(|| {
            let stall = unanalyzed_reason(&unanalyzed, FramesPath::Xctrace);
            judge(&mut report, &outcomes, stall)
        });
        let screen = paths.contains(&FramesPath::Capture).then(|| {
            let stall = unanalyzed_reason(&unanalyzed, FramesPath::Capture);
            judge_capture(&mut report, &captures, stall)
        });
        report.push("frames_path", frames_path(xctrace, screen));
        let changes = self.state.changes.take();
        let marks = drawmark::take();
        judge_drawmark(&mut report, &changes, &marks);
        report.push("generations", self.view.generation());
        report
    }
}

/// Вызов `displayLink`. Вне отрезка полосы движутся без журнала, в отрезке — по стимулу.
fn tick(state: &State, view: &MotionView, link: &CADisplayLink) {
    let entry_ns = now_ns();
    let target = link.targetTimestamp();
    let origin = state.origin_s.get().unwrap_or(target);
    state.origin_s.set(Some(origin));
    let offset = s1_offset(origin, target, SPEED);
    let Some(stimulus) = state.stimulus.get() else {
        view.move_to(offset);
        return;
    };
    let start = state.start_s.get().unwrap_or(target);
    state.start_s.set(Some(start));
    let index = calibrate::tick_index(start, target);
    let in_window = index.checked_sub(WARMUP_TICKS);
    if stimulus == Stimulus::Stall
        && let Some(&(_, periods)) = STALLS.get(state.stalls.get())
        && in_window.is_some_and(|at| at >= STALLS[state.stalls.get()].0)
    {
        state.stalls.set(state.stalls.get() + 1);
        let until = entry_ns + periods * PERIOD_NS;
        while now_ns() < until {
            std::hint::spin_loop();
        }
    }
    let held = match stimulus {
        Stimulus::Holds => in_window.is_some_and(calibrate::held),
        Stimulus::Frozen => true,
        Stimulus::Clean | Stimulus::Stall => false,
    };
    let before = view.generation();
    let generation = if held { before } else { view.move_to(offset) };
    if generation != before {
        state.changes.borrow_mut().push(Change {
            generation,
            visible: 0..1,
            changed_ns: entry_ns,
        });
    }
    state.ticks.borrow_mut().push(Tick {
        index,
        held,
        timestamp_s: link.timestamp(),
        target_s: target,
        entry_ns,
        generation,
    });
    let warmup_s = WARMUP_TICKS as f64 / calibrate::RATE_HZ;
    if target - start >= warmup_s + MEASURE_S + TAIL_S {
        state.stimulus.set(None);
        state.ended.set(true);
    }
}

/// Сводка отрезка для `result.json` без рядов кадров.
#[derive(Serialize)]
struct SegmentSummary<'a> {
    vsync_display: &'a str,
    window_display: &'a str,
    frames_displays: &'a [String],
    grid: &'a frames::Grid,
    rate: &'a frames::Rate,
    window: Window,
    ticks: usize,
    probe_frames: usize,
    forward: &'a calibrate::Forward,
    reverse: Option<&'a Reverse>,
    neighbor_ticks: Option<usize>,
    displaylink_ticks: usize,
    held_ticks: usize,
}

fn record_segments(report: &mut Report, outcomes: &[(Stimulus, SegmentOutcome, Vec<Tick>)]) {
    for (stimulus, outcome, ticks) in outcomes {
        let segment = &outcome.segment;
        let summary = SegmentSummary {
            vsync_display: &segment.vsync_display,
            window_display: &segment.window_display,
            frames_displays: &segment.frames_displays,
            grid: &segment.grid,
            rate: &segment.rate,
            window: segment.window,
            ticks: segment.ticks,
            probe_frames: segment.probe_frames.len(),
            forward: &segment.forward,
            reverse: outcome.reverse.as_ref(),
            neighbor_ticks: outcome.neighbor_ticks,
            displaylink_ticks: ticks.len(),
            held_ticks: ticks.iter().filter(|tick| tick.held).count(),
        };
        let name = stimulus.name();
        report.push(
            &format!("segment_{name}"),
            serde_json::to_string(&summary).unwrap_or_default(),
        );
        let intervals: Vec<f64> = ticks
            .windows(2)
            .map(|pair| (pair[1].target_s - pair[0].target_s) * 1e3)
            .collect();
        report.metric(&format!("tick_target_interval_ms_{name}"), "ms", intervals);
    }
}

fn record_unanalyzed(report: &mut Report, unanalyzed: &[Unanalyzed]) {
    for segment in unanalyzed {
        let prefix = match segment.path {
            FramesPath::Xctrace => "",
            FramesPath::Capture => "sck_",
        };
        report.push(
            &format!("{prefix}stall_unanalyzed"),
            format!(
                "{}; тактов displayLink {}",
                segment.reason,
                segment.ticks.len()
            ),
        );
    }
}

/// Итог пути при неразобранном `stall`: путь не выбирается, проверки отдельных отрезков — в причине.
fn path_verdict(checked: PathVerdict, stall: Option<&str>) -> PathVerdict {
    match (stall, checked) {
        (None, checked) => checked,
        (Some(reason), Ok(())) => Err(format!("stall не разобран: {reason}")),
        (Some(reason), Err(failed)) => Err(format!("stall не разобран: {reason}; {failed}")),
    }
}

/// Вердикт проверки, которой нужны все четыре отрезка: `not_run`, если `stall` не разобран.
fn across_segments(report: &mut Report, name: &str, ok: bool, detail: String, stall: Option<&str>) {
    match stall {
        None => report.finding(name, verdict(ok), detail),
        Some(reason) => report.finding(
            name,
            Verdict::NotRun,
            format!("stall не разобран: {reason}; {detail}"),
        ),
    }
}

fn outcome_of(
    outcomes: &[(Stimulus, SegmentOutcome, Vec<Tick>)],
    stimulus: Stimulus,
) -> Option<&(Stimulus, SegmentOutcome, Vec<Tick>)> {
    outcomes.iter().find(|(kind, _, _)| *kind == stimulus)
}

/// Медиана интервалов `targetTimestamp`, миллисекунды.
fn median_interval_ms(ticks: &[Tick]) -> Option<f64> {
    let mut intervals: Vec<f64> = ticks
        .windows(2)
        .map(|pair| (pair[1].target_s - pair[0].target_s) * 1e3)
        .collect();
    intervals.sort_by(f64::total_cmp);
    intervals.get(intervals.len() / 2).copied()
}

/// Итог калибровки пути: прошёл ли он и почему нет.
pub type PathVerdict = Result<(), String>;

/// Выбор пути по правилу «Способ замера кадров»: `xctrace`, если прошёл, иначе ScreenCaptureKit,
/// иначе `not_run` с причинами. Путь, который процесс не калибровал, не выбирается.
pub fn frames_path(xctrace: Option<PathVerdict>, screen: Option<PathVerdict>) -> String {
    let reason = |name: &str, verdict: &Option<PathVerdict>| match verdict {
        None => format!("{name} не калибровался"),
        Some(Ok(())) => String::new(),
        Some(Err(reason)) => format!("{name}: {reason}"),
    };
    match (&xctrace, &screen) {
        (Some(Ok(())), _) => "xctrace".to_owned(),
        (_, Some(Ok(()))) => "screencapturekit".to_owned(),
        _ => format!(
            "not_run: {}; {}",
            reason("xctrace", &xctrace),
            reason("screencapturekit", &screen)
        ),
    }
}

fn judge(
    report: &mut Report,
    outcomes: &[(Stimulus, SegmentOutcome, Vec<Tick>)],
    stall: Option<&str>,
) -> PathVerdict {
    let expected_ms = PERIOD_NS as f64 / 1e6;
    let rates: Vec<String> = outcomes
        .iter()
        .map(|(kind, outcome, ticks)| {
            format!(
                "{}: vsync {} нс по {} строкам, displayLink {:.3} мс",
                kind.name(),
                outcome.segment.rate.median_period_ns,
                outcome.segment.rate.ticks,
                median_interval_ms(ticks).unwrap_or(0.0)
            )
        })
        .collect();
    let rate_ok = outcomes.len() == SEGMENTS.len()
        && outcomes.iter().all(|(_, outcome, ticks)| {
            outcome.segment.rate.confirmed
                && median_interval_ms(ticks).is_some_and(|ms| {
                    (ms - expected_ms).abs() / expected_ms <= frames::PERIOD_TOLERANCE
                })
        });
    across_segments(report, "rate_confirmed", rate_ok, rates.join("; "), stall);

    let holds_ok = match outcome_of(outcomes, Stimulus::Holds) {
        Some((_, outcome, _)) => {
            let forward = &outcome.segment.forward;
            let expected: usize = calibrate::HOLDS.iter().map(|&(_, len)| len as usize).sum();
            let ok = forward.passed() && forward.held.len() == expected;
            report.finding(
                "forward_holds",
                if ok { Verdict::Pass } else { Verdict::Fail },
                format!(
                    "сдвиг k = {}, тактов {}, удержано {}, найдено {}, лишних {:?}, ненайденных {:?}",
                    forward.shift,
                    forward.ticks,
                    forward.held.len(),
                    forward.missed.len(),
                    forward.unexpected,
                    forward.undetected
                ),
            );
            ok
        }
        None => false,
    };
    let clean_ok = match outcome_of(outcomes, Stimulus::Clean) {
        Some((_, outcome, _)) => {
            let forward = &outcome.segment.forward;
            let ok = forward.ticks > 0 && forward.missed.is_empty();
            report.finding(
                "forward_clean",
                if ok { Verdict::Pass } else { Verdict::Fail },
                format!(
                    "тактов {}, без нового кадра пробы {:?}",
                    forward.ticks, forward.missed
                ),
            );
            ok
        }
        None => false,
    };
    let reverse_ok = match outcome_of(outcomes, Stimulus::Frozen) {
        Some((
            _,
            SegmentOutcome {
                reverse: Some(reverse),
                ..
            },
            _,
        )) => {
            let ok = reverse.passed();
            report.finding(
                "reverse",
                if ok { Verdict::Pass } else { Verdict::Fail },
                format!(
                    "кадров пробы в окне замера {}, тактов соседа {}",
                    reverse.probe_frames, reverse.neighbor_frames
                ),
            );
            ok
        }
        _ => false,
    };
    if let Some((_, outcome, _)) = outcome_of(outcomes, Stimulus::Stall) {
        report.push(
            "stall_missed",
            format!(
                "задержки {STALLS:?}: тактов без нового кадра {} {:?}",
                outcome.segment.forward.missed.len(),
                outcome.segment.forward.missed
            ),
        );
    }
    let checked = match stall {
        None => failed(&[
            ("rate_confirmed", rate_ok),
            ("forward_holds", holds_ok),
            ("forward_clean", clean_ok),
            ("reverse", reverse_ok),
        ]),
        Some(_) => failed(&[
            ("forward_holds", holds_ok),
            ("forward_clean", clean_ok),
            ("reverse", reverse_ok),
        ]),
    };
    path_verdict(checked, stall)
}

fn failed(checks: &[(&str, bool)]) -> PathVerdict {
    let failed: Vec<&str> = checks
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(name, _)| *name)
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("не прошли {}", failed.join(", ")))
    }
}

/// Сводка отрезка ScreenCaptureKit для `result.json` без рядов кадров.
#[derive(Serialize)]
struct CaptureSummary<'a> {
    grid: &'a frames::Grid,
    window: Window,
    ticks: usize,
    samples: usize,
    complete: usize,
    unread_outside: usize,
    phase_ns: Option<f64>,
    phase_frames: usize,
    new_frames: usize,
    skipped_generations: u64,
    max_handler_ns: u64,
    off_grid_targets: usize,
    residual_min_ns: f64,
    residual_median_ns: f64,
    residual_max_ns: f64,
    frame_period_median_ns: u64,
    forward: &'a calibrate::Forward,
    reverse: Option<&'a Reverse>,
    neighbor_ticks: Option<usize>,
    displaylink_ticks: usize,
    held_ticks: usize,
}

fn record_captures(report: &mut Report, captures: &[(Stimulus, CaptureOutcome, Vec<Tick>)]) {
    for (stimulus, outcome, ticks) in captures {
        let segment = &outcome.segment;
        let summary = CaptureSummary {
            grid: &segment.grid,
            window: segment.window,
            ticks: segment.ticks,
            samples: segment.samples,
            complete: segment.complete,
            unread_outside: segment.unread_outside,
            phase_ns: segment.phase_ns,
            phase_frames: segment.phase_frames,
            new_frames: segment.new_frames.len(),
            skipped_generations: segment.skipped_generations,
            max_handler_ns: segment.max_handler_ns,
            off_grid_targets: segment.off_grid_targets,
            residual_min_ns: segment.residual_min_ns,
            residual_median_ns: segment.residual_median_ns,
            residual_max_ns: segment.residual_max_ns,
            frame_period_median_ns: segment.frame_period_median_ns,
            forward: &segment.forward,
            reverse: outcome.reverse.as_ref(),
            neighbor_ticks: outcome.neighbor_ticks,
            displaylink_ticks: ticks.len(),
            held_ticks: ticks.iter().filter(|tick| tick.held).count(),
        };
        report.push(
            &format!("sck_segment_{}", stimulus.name()),
            serde_json::to_string(&summary).unwrap_or_default(),
        );
    }
}

fn verdict(ok: bool) -> Verdict {
    if ok { Verdict::Pass } else { Verdict::Fail }
}

/// Калибровка пути ScreenCaptureKit теми же контролями, что у `xctrace`. Частота: `displayLink`
/// и медиана интервала новых кадров потока в чистом отрезке — 8,33 мс. Наблюдатель: обработчик
/// кадра короче периода во всех отрезках.
fn judge_capture(
    report: &mut Report,
    captures: &[(Stimulus, CaptureOutcome, Vec<Tick>)],
    stall: Option<&str>,
) -> PathVerdict {
    let find = |stimulus: Stimulus| captures.iter().find(|(kind, _, _)| *kind == stimulus);
    let expected_ms = PERIOD_NS as f64 / 1e6;
    let near = |ms: f64| (ms - expected_ms).abs() / expected_ms <= frames::PERIOD_TOLERANCE;
    let link_ok = captures.len() == SEGMENTS.len()
        && captures
            .iter()
            .all(|(_, _, ticks)| median_interval_ms(ticks).is_some_and(near));
    let stream_ms = find(Stimulus::Clean)
        .map(|(_, outcome, _)| outcome.segment.frame_period_median_ns as f64 / 1e6);
    let rate_ok = link_ok && stream_ms.is_some_and(near);
    across_segments(
        report,
        "sck_rate_confirmed",
        rate_ok,
        format!(
            "displayLink {}; новые кадры потока в clean {:?} мс",
            captures
                .iter()
                .map(|(kind, _, ticks)| format!(
                    "{} {:.3} мс",
                    kind.name(),
                    median_interval_ms(ticks).unwrap_or(0.0)
                ))
                .collect::<Vec<_>>()
                .join(", "),
            stream_ms
        ),
        stall,
    );
    let holds_ok = find(Stimulus::Holds).is_some_and(|(_, outcome, _)| {
        let forward = &outcome.segment.forward;
        let expected: usize = calibrate::HOLDS.iter().map(|&(_, len)| len as usize).sum();
        let ok = forward.passed() && forward.held.len() == expected;
        report.finding(
            "sck_forward_holds",
            verdict(ok),
            format!(
                "сдвиг k = {}, тактов {}, удержано {}, найдено {}, лишних {:?}, ненайденных {:?}",
                forward.shift,
                forward.ticks,
                forward.held.len(),
                forward.missed.len(),
                forward.unexpected,
                forward.undetected
            ),
        );
        ok
    });
    let clean_ok = find(Stimulus::Clean).is_some_and(|(_, outcome, _)| {
        let forward = &outcome.segment.forward;
        let ok = forward.ticks > 0 && forward.missed.is_empty();
        report.finding(
            "sck_forward_clean",
            verdict(ok),
            format!(
                "тактов {}, без нового кадра {:?}, перескочено поколений {}",
                forward.ticks, forward.missed, outcome.segment.skipped_generations
            ),
        );
        ok
    });
    let reverse_ok = find(Stimulus::Frozen).is_some_and(|(_, outcome, _)| {
        let Some(reverse) = &outcome.reverse else {
            return false;
        };
        let ok = reverse.passed();
        report.finding(
            "sck_reverse",
            verdict(ok),
            format!(
                "новых кадров пробы в окне замера {}, тактов соседа {}",
                reverse.probe_frames, reverse.neighbor_frames
            ),
        );
        ok
    });
    let handler_max = captures
        .iter()
        .map(|(_, outcome, _)| outcome.segment.max_handler_ns)
        .max()
        .unwrap_or(0);
    let observer_ok = captures.len() == SEGMENTS.len() && handler_max < PERIOD_NS;
    across_segments(
        report,
        "sck_observer",
        observer_ok,
        format!("наибольший обработчик кадра {handler_max} нс, период {PERIOD_NS} нс"),
        stall,
    );
    if let Some((_, outcome, _)) = find(Stimulus::Stall) {
        report.push(
            "sck_stall_missed",
            format!(
                "задержки {STALLS:?}: тактов без нового кадра {} {:?}",
                outcome.segment.forward.missed.len(),
                outcome.segment.forward.missed
            ),
        );
    }
    let checked = match stall {
        None => failed(&[
            ("sck_rate_confirmed", rate_ok),
            ("sck_forward_holds", holds_ok),
            ("sck_forward_clean", clean_ok),
            ("sck_reverse", reverse_ok),
            ("sck_observer", observer_ok),
        ]),
        Some(_) => failed(&[
            ("sck_forward_holds", holds_ok),
            ("sck_forward_clean", clean_ok),
            ("sck_reverse", reverse_ok),
        ]),
    };
    path_verdict(checked, stall)
}

/// Самопроверка порядка: у каждого изменения первая отметка его или старшего поколения начата не
/// раньше изменения и закончена не раньше начала.
pub fn order_violations(changes: &[Change], marks: &[Mark]) -> (usize, Vec<String>) {
    let mut checked = 0;
    let mut violations = Vec::new();
    for change in changes {
        let Some(mark) = marks.iter().find(|mark| {
            mark.generation >= change.generation && mark.draw_end_ns >= change.changed_ns
        }) else {
            continue;
        };
        checked += 1;
        let covered = Covered {
            change: change.clone(),
            mark: mark.clone(),
            marks: 1,
        };
        if let Some(violation) = covered.order_violation() {
            violations.push(violation);
        }
    }
    (checked, violations)
}

fn judge_drawmark(report: &mut Report, changes: &[Change], marks: &[Mark]) {
    let (checked, violations) = order_violations(changes, marks);
    let ok = checked > 0 && violations.is_empty();
    report.finding(
        "drawmark_order",
        if ok { Verdict::Pass } else { Verdict::Fail },
        format!(
            "изменений {}, отметок {}, проверено {checked}, нарушений {}: {}",
            changes.len(),
            marks.len(),
            violations.len(),
            violations
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        ),
    );
    let draws: Vec<f64> = marks
        .iter()
        .map(|mark| mark.draw_end_ns.saturating_sub(mark.draw_start_ns) as f64 / 1e6)
        .collect();
    report.metric("draw_ms", "ms", draws);
}

/// Сосед обратного контроля: процесс пробы `--neighbor`, живёт до закрытия stdin.
struct Neighbor {
    child: Child,
    stdout: BufReader<ChildStdout>,
}

impl Neighbor {
    fn spawn(dir: &Path) -> Result<Neighbor, String> {
        let exe = std::env::current_exe().map_err(|err| format!("путь пробы: {err}"))?;
        let stderr = std::fs::File::create(dir.join("neighbor-stderr.txt"))
            .map_err(|err| format!("журнал соседа: {err}"))?;
        let mut child = Command::new(exe)
            .arg("--neighbor")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|err| format!("сосед не запущен: {err}"))?;
        let stdout = child.stdout.take().ok_or("сосед без stdout")?;
        let mut neighbor = Neighbor {
            child,
            stdout: BufReader::new(stdout),
        };
        let mut line = String::new();
        neighbor
            .stdout
            .read_line(&mut line)
            .map_err(|err| format!("сосед: {err}"))?;
        if !line.starts_with(neighbor::PID_LINE) {
            return Err(format!("сосед не назвал pid: «{}»", line.trim()));
        }
        Ok(neighbor)
    }

    /// Закрывает stdin соседа и читает его такты.
    fn finish(mut self) -> Result<(usize, Vec<u64>), String> {
        drop(self.child.stdin.take());
        let mut ticks = None;
        let mut targets = None;
        for line in (&mut self.stdout).lines().map_while(Result::ok) {
            if let Some(value) = line.strip_prefix(neighbor::TICKS_LINE) {
                ticks = value.trim().parse::<usize>().ok();
            } else if let Some(value) = line.strip_prefix(neighbor::TARGETS_LINE) {
                targets = value
                    .split(',')
                    .filter(|item| !item.is_empty())
                    .map(|item| item.parse::<u64>())
                    .collect::<Result<Vec<u64>, _>>()
                    .ok();
            }
        }
        let status = self.child.wait().map_err(|err| format!("сосед: {err}"))?;
        if !status.success() {
            return Err(format!("сосед вышел с {status}"));
        }
        match (ticks, targets) {
            (Some(ticks), Some(targets)) => Ok((ticks, targets)),
            _ => Err("сосед не напечатал такты".to_owned()),
        }
    }
}

impl Drop for Neighbor {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Окно пробы для отрезка: процесс записи `xctrace` и окно потока ScreenCaptureKit.
#[derive(Clone, Copy)]
struct Target {
    pid: u32,
    window_id: u32,
    window_w: f64,
    window_h: f64,
}

fn spawn_worker(
    path: FramesPath,
    stimulus: Stimulus,
    target: Target,
    dir: PathBuf,
) -> Result<Worker, String> {
    let (to, stops) = mpsc::channel::<Stop>();
    let (events, from) = mpsc::channel::<FromWorker>();
    let handle = std::thread::Builder::new()
        .name(format!(
            "probe-calibrate-{}-{}",
            path.name(),
            stimulus.name()
        ))
        .spawn(move || {
            let result = match path {
                FramesPath::Xctrace => run_segment(stimulus, target.pid, &dir, &events, &stops),
                FramesPath::Capture => run_capture_segment(stimulus, target, &dir, &events, &stops),
            };
            let _ = events.send(FromWorker::Done(result));
        })
        .map_err(|err| format!("рабочий поток отрезка не запущен: {err}"))?;
    Ok(Worker { to, from, handle })
}

fn write(path: &Path, contents: impl AsRef<[u8]>) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|err| format!("{}: {err}", path.display()))
}

fn run_segment(
    stimulus: Stimulus,
    pid: u32,
    dir: &Path,
    events: &Sender<FromWorker>,
    stops: &Receiver<Stop>,
) -> Result<Outcome, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let neighbor = match stimulus {
        Stimulus::Frozen => Some(Neighbor::spawn(dir)?),
        Stimulus::Holds | Stimulus::Clean | Stimulus::Stall => None,
    };
    let trace = dir.join("trace.trace");
    let recorder = Recorder::start(
        frames::record_command(pid, &trace),
        trace.clone(),
        frames::START_TIMEOUT,
    )?;
    events
        .send(FromWorker::Started)
        .map_err(|_| "главный поток не ждёт отрезок".to_owned())?;
    let stop = stops
        .recv()
        .map_err(|_| "главный поток не остановил отрезок".to_owned())?;
    let neighbor = neighbor.map(Neighbor::finish).transpose()?;
    let journal = recorder.stop(frames::SAVE_TIMEOUT)?;
    write(&dir.join("xctrace.txt"), journal.join("\n"))?;
    let mut tables = Vec::new();
    for schema in frames::TABLES {
        let xml = frames::export_table(&trace, schema)?;
        write(&dir.join(format!("{schema}.xml")), &xml)?;
        tables.push(xml);
    }
    write(
        &dir.join("ticks.json"),
        serde_json::to_string(&stop.ticks).unwrap_or_default(),
    )?;
    write(
        &dir.join("segment-input.json"),
        serde_json::to_string(&serde_json::json!({
            "pid": pid,
            "window": stop.window,
            "held_targets_ns": stop.held_targets_ns,
        }))
        .unwrap_or_default(),
    )?;
    let table = |schema: &str| -> Result<frames::Table, String> {
        let index = frames::TABLES
            .iter()
            .position(|name| *name == schema)
            .ok_or_else(|| format!("таблица {schema} не выгружается"))?;
        frames::parse_table(&tables[index]).map_err(|err| format!("{schema}: {err}"))
    };
    let exported = Exported {
        clock: frames::Clock::from_table(&table("time-info")?)?,
        vsyncs: frames::vsyncs(&table("display-vsyncs-interval")?)?,
        updates: frames::updates(&table("hitches-updates")?)?,
        lifetimes: frames::lifetimes(&table("hitches-frame-lifetimes")?)?,
    };
    let analysis = calibrate::analyze(&exported, pid, stop.window, &stop.held_targets_ns);
    let segment = match observed(stimulus, analysis)? {
        Ok(segment) => segment,
        Err(reason) => return Ok(Outcome::Unanalyzed(reason)),
    };
    let reverse = neighbor.as_ref().map(|(_, targets)| {
        let in_trace: Vec<u64> = targets
            .iter()
            .filter_map(|&at| exported.clock.trace_ns(at))
            .collect();
        calibrate::reverse(&segment.probe_frames, &in_trace, segment.window)
    });
    Ok(Outcome::Xctrace(Box::new(SegmentOutcome {
        segment,
        reverse,
        neighbor_ticks: neighbor.map(|(ticks, _)| ticks),
    })))
}

/// Отрезок ScreenCaptureKit: поток окна пробы без записи `xctrace`, журнал доставки и разбор.
fn run_capture_segment(
    stimulus: Stimulus,
    target: Target,
    dir: &Path,
    events: &Sender<FromWorker>,
    stops: &Receiver<Stop>,
) -> Result<Outcome, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let neighbor = match stimulus {
        Stimulus::Frozen => Some(Neighbor::spawn(dir)?),
        Stimulus::Holds | Stimulus::Clean | Stimulus::Stall => None,
    };
    let capture = Capture::start(target.window_id, Some(dir.join("first-frame.bgra")))?;
    let (window_w, window_h) = (capture.window_w, capture.window_h);
    let started_ns = now_ns();
    events
        .send(FromWorker::Started)
        .map_err(|_| "главный поток не ждёт отрезок".to_owned())?;
    let stop = stops
        .recv()
        .map_err(|_| "главный поток не остановил отрезок".to_owned())?;
    let neighbor = neighbor.map(Neighbor::finish).transpose()?;
    let stopped_ns = now_ns();
    let samples = capture.stop()?;
    write(
        &dir.join("samples.json"),
        serde_json::to_string(&samples).unwrap_or_default(),
    )?;
    write(
        &dir.join("ticks.json"),
        serde_json::to_string(&stop.ticks).unwrap_or_default(),
    )?;
    let streamed = Window {
        start_ns: started_ns,
        end_ns: stopped_ns,
    };
    write(
        &dir.join("segment-input.json"),
        serde_json::to_string(&serde_json::json!({
            "streamed": streamed,
            "window": stop.window,
            "held_targets_ns": stop.held_targets_ns,
            "window_w": window_w,
            "window_h": window_h,
            "window_w_at_start": target.window_w,
            "window_h_at_start": target.window_h,
        }))
        .unwrap_or_default(),
    )?;
    let targets: Vec<u64> = stop
        .ticks
        .iter()
        .map(|tick| (tick.target_s * 1e9).round() as u64)
        .collect();
    let analysis = calibrate::analyze_capture(
        &samples,
        &targets,
        streamed,
        stop.window,
        &stop.held_targets_ns,
    );
    let segment = match observed(stimulus, analysis)? {
        Ok(segment) => segment,
        Err(reason) => return Ok(Outcome::Unanalyzed(reason)),
    };
    let reverse = neighbor
        .as_ref()
        .map(|(_, targets)| calibrate::reverse(&segment.new_frames, targets, segment.window));
    Ok(Outcome::Capture(Box::new(CaptureOutcome {
        segment,
        reverse,
        neighbor_ticks: neighbor.map(|(ticks, _)| ticks),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_come_from_the_environment_list() {
        assert_eq!(runs(None).unwrap().len(), 8);
        let only = runs(Some("screencapturekit")).unwrap();
        assert!(only.iter().all(|(path, _)| *path == FramesPath::Capture));
        assert_eq!(only.len(), 4);
        assert!(runs(Some("vnc")).is_err());
    }

    #[test]
    fn path_choice_follows_the_rule() {
        let fail = || Some(Err("не прошли reverse".to_owned()));
        assert_eq!(frames_path(Some(Ok(())), fail()), "xctrace");
        assert_eq!(frames_path(fail(), Some(Ok(()))), "screencapturekit");
        let none = frames_path(fail(), fail());
        assert!(none.starts_with("not_run: xctrace: не прошли"), "{none}");
        assert!(frames_path(None, fail()).contains("xctrace не калибровался"));
    }

    fn mark(generation: u64, start: u64, end: u64) -> Mark {
        Mark {
            generation,
            rows: 0..1,
            draw_start_ns: start,
            draw_end_ns: end,
        }
    }

    fn change(generation: u64, at: u64) -> Change {
        Change {
            generation,
            visible: 0..1,
            changed_ns: at,
        }
    }

    #[test]
    fn order_check_matches_the_first_mark_of_the_generation() {
        let marks = [mark(1, 110, 120), mark(3, 310, 320)];
        // Поколение 2 слилось с 3: засчитывается отметка старшего поколения.
        let changes = [change(1, 100), change(2, 200), change(3, 300)];
        assert_eq!(order_violations(&changes, &marks), (3, Vec::new()));
        let early = [mark(1, 90, 120)];
        let (checked, violations) = order_violations(&[change(1, 100)], &early);
        assert_eq!(checked, 1);
        assert_eq!(violations.len(), 1);
        assert_eq!(order_violations(&[change(5, 100)], &marks), (0, Vec::new()));
    }

    /// Отрезок `stall` прогона `20261119-142709-26543`: строка vsync встроенного дисплея лежит в
    /// полупериоде от сетки во время задержки, сетка дисплея окна не строится.
    #[test]
    fn recorded_stall_is_an_observation() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/calibrate-stall-142709/"
        );
        let read = |name: &str| {
            let xml = std::fs::read_to_string(format!("{dir}{name}.xml")).unwrap();
            frames::parse_table(&xml).unwrap()
        };
        let exported = Exported {
            clock: frames::Clock::from_table(&read("time-info")).unwrap(),
            vsyncs: frames::vsyncs(&read("display-vsyncs-interval")).unwrap(),
            updates: frames::updates(&read("hitches-updates")).unwrap(),
            lifetimes: frames::lifetimes(&read("hitches-frame-lifetimes")).unwrap(),
        };
        #[derive(serde::Deserialize)]
        struct Input {
            pid: u32,
            window: Window,
            held_targets_ns: Vec<u64>,
        }
        let input: Input = serde_json::from_str(
            &std::fs::read_to_string(format!("{dir}segment-input.json")).unwrap(),
        )
        .unwrap();
        let analysis =
            calibrate::analyze(&exported, input.pid, input.window, &input.held_targets_ns);
        let reason = match &analysis {
            Ok(_) => panic!("разбор stall прогона 142709 прошёл"),
            Err(reason) => reason.clone(),
        };
        assert!(reason.contains("подходят 0 сеток"), "{reason}");
        assert!(matches!(observed(Stimulus::Stall, analysis), Ok(Err(_))));
        for stimulus in [Stimulus::Holds, Stimulus::Clean, Stimulus::Frozen] {
            assert_eq!(
                observed::<()>(stimulus, Err(reason.clone())),
                Err(reason.clone())
            );
        }
        assert_eq!(observed(Stimulus::Holds, Ok(7)), Ok(Ok(7)));
    }

    #[test]
    fn unanalyzed_stall_makes_the_path_not_run() {
        let verdict_of = |report: &Report, name: &str| {
            report
                .findings
                .iter()
                .find(|finding| finding.name == name)
                .map(|finding| finding.verdict)
        };
        let mut report = Report::default();
        let xctrace = judge(&mut report, &[], Some("сетка не строится"));
        assert_eq!(verdict_of(&report, "rate_confirmed"), Some(Verdict::NotRun));
        let reason = xctrace.clone().unwrap_err();
        assert!(
            reason.starts_with("stall не разобран: сетка не строится"),
            "{reason}"
        );
        assert!(!reason.contains("rate_confirmed"), "{reason}");

        let mut report = Report::default();
        let screen = judge_capture(&mut report, &[], Some("маркер не прочитан"));
        for name in ["sck_rate_confirmed", "sck_observer"] {
            assert_eq!(verdict_of(&report, name), Some(Verdict::NotRun), "{name}");
        }
        let reason = screen.clone().unwrap_err();
        assert!(!reason.contains("sck_observer"), "{reason}");
        assert!(frames_path(Some(xctrace), Some(screen)).starts_with("not_run"));

        // Без отметки те же недостающие отрезки — отказ проверок, как раньше.
        let mut report = Report::default();
        assert!(
            judge(&mut report, &[], None)
                .unwrap_err()
                .contains("rate_confirmed")
        );
        assert_eq!(verdict_of(&report, "rate_confirmed"), Some(Verdict::Fail));
        let mut report = Report::default();
        assert!(
            judge_capture(&mut report, &[], None)
                .unwrap_err()
                .contains("sck_observer")
        );
        assert_eq!(verdict_of(&report, "sck_observer"), Some(Verdict::Fail));
    }

    #[test]
    fn segments_are_recorded_in_order() {
        assert_eq!(
            SEGMENTS.map(Stimulus::name),
            ["holds", "clean", "frozen", "stall"]
        );
        assert!(
            STALLS
                .iter()
                .all(|&(at, _)| at < (MEASURE_S * calibrate::RATE_HZ) as u64)
        );
    }
}
