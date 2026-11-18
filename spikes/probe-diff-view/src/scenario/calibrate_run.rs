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
    self, Exported, MEASURE_S, PERIOD_NS, Reverse, Segment, TAIL_S, WARMUP_TICKS, Window,
};
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

enum FromWorker {
    Started,
    Done(Result<Box<SegmentOutcome>, String>),
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
    view: Retained<MotionView>,
    driver: Retained<FrameDriver>,
    link: Retained<CADisplayLink>,
    state: Rc<State>,
    out: PathBuf,
    phase: RefCell<Phase>,
    outcomes: RefCell<Vec<(Stimulus, SegmentOutcome, Vec<Tick>)>>,
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
        window.setContentView(Some(&view));
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
            view,
            driver,
            link,
            state,
            out: out.to_path_buf(),
            phase: RefCell::new(Phase::Next(0)),
            outcomes: RefCell::new(Vec::new()),
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
            Phase::Next(index) if index < SEGMENTS.len() => {
                let stimulus = SEGMENTS[index];
                let dir = self.out.join(format!("calibrate-{}", stimulus.name()));
                Phase::Starting(index, spawn_worker(stimulus, std::process::id(), dir)?)
            }
            Phase::Next(_) | Phase::Finished => Phase::Finished,
            Phase::Starting(index, worker) => match worker.from.try_recv() {
                Ok(FromWorker::Started) => {
                    let state = &self.state;
                    state.start_s.set(None);
                    state.stalls.set(0);
                    state.ticks.borrow_mut().clear();
                    state.ended.set(false);
                    state.stimulus.set(Some(SEGMENTS[index]));
                    Phase::Running(index, worker)
                }
                Ok(FromWorker::Done(result)) => return Err(segment_error(index, result)),
                Err(TryRecvError::Empty) => Phase::Starting(index, worker),
                Err(TryRecvError::Disconnected) => {
                    return Err(format!(
                        "отрезок {}: рабочий поток упал",
                        SEGMENTS[index].name()
                    ));
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
                    .filter(|tick| tick.held && SEGMENTS[index] == Stimulus::Holds)
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
                    self.outcomes
                        .borrow_mut()
                        .push((SEGMENTS[index], *outcome, ticks));
                    Phase::Next(index + 1)
                }
                Ok(FromWorker::Done(result)) => return Err(segment_error(index, result)),
                Ok(FromWorker::Started) => Phase::Saving(index, worker),
                Err(TryRecvError::Empty) => Phase::Saving(index, worker),
                Err(TryRecvError::Disconnected) => {
                    return Err(format!(
                        "отрезок {}: рабочий поток упал",
                        SEGMENTS[index].name()
                    ));
                }
            },
        })
    }

    fn stop_link(&self) {
        self.link.invalidate();
        self.driver.clear();
    }

    fn fail(&self, reason: String) -> Failed {
        self.stop_link();
        let mut report = self.report.borrow_mut().take().unwrap_or_default();
        record_segments(&mut report, &self.outcomes.borrow());
        Box::new((reason, report))
    }

    fn finish(&self) -> Report {
        self.stop_link();
        let mut report = self.report.borrow_mut().take().unwrap_or_default();
        let outcomes = self.outcomes.borrow();
        record_segments(&mut report, &outcomes);
        judge(&mut report, &outcomes);
        let changes = self.state.changes.take();
        let marks = drawmark::take();
        judge_drawmark(&mut report, &changes, &marks);
        report.push("generations", self.view.generation());
        report
    }
}

fn segment_error(index: usize, result: Result<Box<SegmentOutcome>, String>) -> String {
    let name = SEGMENTS[index].name();
    match result {
        Ok(_) => format!("отрезок {name}: итог без начала записи"),
        Err(reason) => format!("отрезок {name}: {reason}"),
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

fn judge(report: &mut Report, outcomes: &[(Stimulus, SegmentOutcome, Vec<Tick>)]) {
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
    report.finding(
        "rate_confirmed",
        if rate_ok {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        rates.join("; "),
    );

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
    let path = if rate_ok && holds_ok && clean_ok && reverse_ok {
        "xctrace".to_owned()
    } else {
        "screencapturekit: калибровка xctrace не прошла".to_owned()
    };
    report.push("frames_path", path);
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

fn spawn_worker(stimulus: Stimulus, pid: u32, dir: PathBuf) -> Result<Worker, String> {
    let (to, stops) = mpsc::channel::<Stop>();
    let (events, from) = mpsc::channel::<FromWorker>();
    let handle = std::thread::Builder::new()
        .name(format!("probe-calibrate-{}", stimulus.name()))
        .spawn(move || {
            let result = run_segment(stimulus, pid, &dir, &events, &stops).map(Box::new);
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
) -> Result<SegmentOutcome, String> {
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
    let segment = calibrate::analyze(&exported, pid, stop.window, &stop.held_targets_ns)?;
    let reverse = neighbor.as_ref().map(|(_, targets)| {
        let in_trace: Vec<u64> = targets
            .iter()
            .filter_map(|&at| exported.clock.trace_ns(at))
            .collect();
        calibrate::reverse(&segment.probe_frames, &in_trace, segment.window)
    });
    Ok(SegmentOutcome {
        segment,
        reverse,
        neighbor_ticks: neighbor.map(|(ticks, _)| ticks),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
