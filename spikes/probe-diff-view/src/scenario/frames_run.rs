//! Сценарий `frames` с окном: открытие фикстуры клавишей `o`, у `threads` раскрытие всех тредов,
//! затем три отрезка S1, у `modified` после них бросок S2 и контрольный ряд без фаз, у `wrap` в
//! «Рядом» вместо S1 ресайз S3. Стимулы, ритм `displayLink`, время `drawRect:` и подсчёт потерь — чистая часть
//! [`super::scroll`]. Путь замера здесь только `none`: трасса не пишется, вердикт по кадрам —
//! `not_run` «не откалиброван», потери в процессе — наблюдение (отчёт, раздел «Ошибка разбора
//! отрезка `stall`», итог калибровки задачи 12).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, Message, sel};
use objc2_app_kit::{NSScreen, NSView, NSWindow};
use objc2_foundation::{NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize};
use objc2_quartz_core::CADisplayLink;
use serde::Serialize;

use probe_diff_view::fixtures::Fixture;
use probe_diff_view::highlight::Source;
use probe_diff_view::model::Mode;
use probe_diff_view::model::threads::BlockItem;
use probe_diff_view::open::OpenFixture;

use super::calibrate;
use super::calibrate_run::Failed;
use super::drawmark;
use super::events::{self, Key};
use super::sampler;
use super::scroll::{
    self, FLING_SETTLE_TICKS, PLAIN_EVENTS, RESIZE_S, S1Summary, S1Tick, S2Tick, Sweep, WheelSeen,
    WheelStep,
};
use super::stage::{Stage, View};
use super::stand::FrameDriver;
use super::watchdog::SharedTrace;
use crate::args::{FramesPath, Variant, ViewMode, ViewOptions};
use crate::results::{Report, Verdict};

/// Срок генерации фикстуры на рабочем потоке.
pub const LOAD_DEADLINE: Duration = Duration::from_secs(30);
/// Срок готовности до клавиши: запрос подсветки скомпилирован, окно ничего не ждёт отображения.
pub const READY_DEADLINE: Duration = Duration::from_secs(10);
/// Срок открытия: от `postEvent` клавиши до покрытия после снимка полного разбора.
pub const OPEN_DEADLINE: Duration = Duration::from_secs(30);
/// Срок раскрытия тредов `threads` до покрытия.
pub const EXPAND_DEADLINE: Duration = Duration::from_secs(10);
/// Срок покрытия видимых строк в начальном положении отрезка.
pub const COVER_DEADLINE: Duration = Duration::from_secs(2);
/// Срок отрезка S1 по часам процесса: 10 с по `targetTimestamp` и запас на первый вызов.
pub const SEGMENT_DEADLINE: Duration = Duration::from_secs(20);
/// Сроки S2 и S3 по часам процесса.
pub const S2_DEADLINE: Duration = Duration::from_secs(10);
pub const S3_DEADLINE: Duration = Duration::from_secs(10);
/// Наибольшая и наименьшая ширина содержимого окна S3, точки.
pub const S3_MAX_WIDTH: f64 = 1180.0;
pub const S3_MIN_WIDTH: f64 = 1060.0;

/// Отказ пути замера, который GUI-часть не ведёт: ни один путь не прошёл формальную калибровку.
pub fn unsupported_path(path: FramesPath) -> Option<String> {
    match path {
        FramesPath::None => None,
        FramesPath::Xctrace | FramesPath::Screencapturekit => Some(format!(
            "путь замера {} в frames не реализован: по объединённому итогу калибровки ни один путь \
             не годен, frames идёт с --frames-path none",
            path.name()
        )),
    }
}

const NOT_CALIBRATED: &str = "не откалиброван: ни один путь не прошёл формальную калибровку \
     (xctrace — 20261119-140449-92675, ScreenCaptureKit — 20261119-144446-57522), путь замера none";

/// Что делает вызов `displayLink`.
enum Drive {
    Idle,
    S1 {
        sweep: Sweep,
        limit: f64,
        start_s: Option<f64>,
        ticks: Vec<S1Tick>,
        done: bool,
    },
    /// Бросок S2 или контрольный ряд без фаз (`plain`).
    S2 {
        plain: bool,
        steps: Vec<WheelStep>,
        point: objc2_foundation::NSPoint,
        primary_height: f64,
        posted: Vec<u64>,
        seen: Vec<WheelSeen>,
        ticks: Vec<S2Tick>,
        settle: usize,
        error: Option<String>,
        done: bool,
    },
    S3 {
        start_s: Option<f64>,
        origin: objc2_foundation::NSPoint,
        height: f64,
        ticks: Vec<S3Tick>,
        error: Option<String>,
        done: bool,
    },
}

/// Вызов `displayLink` ресайза S3.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct S3Tick {
    pub target_s: f64,
    pub entry_ns: u64,
    pub width: f64,
    pub pending: bool,
    pub covered: bool,
}

enum Step {
    Loading {
        rx: Receiver<Fixture>,
        deadline: Instant,
    },
    Ready {
        deadline: Instant,
    },
    Opening {
        posted: Vec<u64>,
        posted_ns: u64,
        deadline: Instant,
    },
    Expanding {
        deadline: Instant,
    },
    /// Начальное положение отрезка S1 `index` или S2 и ожидание покрытия.
    Positioning {
        next: Next,
        deadline: Instant,
    },
    Running {
        next: Next,
        deadline: Instant,
    },
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Next {
    S1(usize),
    S2,
    Plain,
    S3,
}

/// Итог отрезка S1 в `result.json`.
#[derive(Clone, Debug, Serialize)]
struct SegmentNote {
    sweep: &'static str,
    limit: f64,
    y0: f64,
    summary: S1Summary,
    /// Строка с раскрытым тредом на пути отрезка (`threads`).
    expanded_block_row: Option<u32>,
}

pub struct FramesRun {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    trace: SharedTrace,
    variant: Variant,
    mode: Mode,
    fixture: OpenFixture,
    stage: RefCell<Option<Rc<Stage>>>,
    drive: Rc<RefCell<Drive>>,
    link: RefCell<Option<(Retained<FrameDriver>, Retained<CADisplayLink>)>>,
    step: RefCell<Step>,
    segments: RefCell<Vec<SegmentNote>>,
    s1_targets: RefCell<Vec<f64>>,
    fling: RefCell<Option<scroll::FlingCheck>>,
    plain: RefCell<Option<scroll::PlainCheck>>,
    report: RefCell<Option<Report>>,
    closed: Cell<bool>,
}

fn now_ns() -> u64 {
    sampler::uptime_ns().unwrap_or(0)
}

fn returned(posted: &[u64]) -> bool {
    posted
        .iter()
        .all(|number| events::record(*number).is_some_and(|record| record.send_return.is_some()))
}

impl FramesRun {
    pub fn start(
        mtm: MainThreadMarker,
        window: &NSWindow,
        mut report: Report,
        trace: SharedTrace,
        view: ViewOptions,
    ) -> Result<FramesRun, Failed> {
        let (variant, mode, fixture) = match (view.variant, view.mode, view.fixture) {
            (Some(variant), Some(mode), Some(fixture)) => (variant, mode, fixture),
            _ => return Err(Box::new((format!("нет оси кадров: {view:?}"), report))),
        };
        let Some(name) = fixture.synthetic() else {
            return Err(Box::new((
                format!("фикстура {} у frames не синтетическая", fixture.name()),
                report,
            )));
        };
        report.push("frames_path", FramesPath::None.name());
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("probe-diff-frames-load".to_owned())
            .spawn(move || {
                let _ = tx.send(probe_diff_view::fixtures::generate(name));
            });
        if let Err(err) = spawned {
            return Err(Box::new((
                format!("поток загрузки не запущен: {err}"),
                report,
            )));
        }
        Ok(FramesRun {
            mtm,
            window: window.retain(),
            trace,
            variant,
            mode: match mode {
                ViewMode::Unified => Mode::Unified,
                ViewMode::Side => Mode::SideBySide,
            },
            fixture,
            stage: RefCell::new(None),
            drive: Rc::new(RefCell::new(Drive::Idle)),
            link: RefCell::new(None),
            step: RefCell::new(Step::Loading {
                rx,
                deadline: Instant::now() + LOAD_DEADLINE,
            }),
            segments: RefCell::new(Vec::new()),
            s1_targets: RefCell::new(Vec::new()),
            fling: RefCell::new(None),
            plain: RefCell::new(None),
            report: RefCell::new(Some(report)),
            closed: Cell::new(false),
        })
    }

    fn stage(&self) -> Result<Rc<Stage>, String> {
        self.stage
            .borrow()
            .clone()
            .ok_or_else(|| "нет стенда кадров".to_owned())
    }

    pub fn poll(&self) -> Option<Result<Report, Failed>> {
        let stage = self.stage.borrow().clone();
        if let Some(stage) = stage
            && let Err(reason) = stage_errors(&stage)
        {
            return Some(Err(self.fail(reason)));
        }
        let step = std::mem::replace(&mut *self.step.borrow_mut(), Step::Done);
        match self.advance(step) {
            Ok(Step::Done) => Some(self.finish()),
            Ok(next) => {
                *self.step.borrow_mut() = next;
                None
            }
            Err(reason) => Some(Err(self.fail(reason))),
        }
    }

    fn advance(&self, step: Step) -> Result<Step, String> {
        match step {
            Step::Loading { rx, deadline } => match rx.try_recv() {
                Ok(fixture) => self.loaded(fixture),
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    Ok(Step::Loading { rx, deadline })
                }
                Err(TryRecvError::Empty) => Err(format!(
                    "фикстура не сгенерирована за {} с",
                    LOAD_DEADLINE.as_secs()
                )),
                Err(TryRecvError::Disconnected) => Err("поток загрузки упал".to_owned()),
            },
            Step::Ready { deadline } => {
                let stage = self.stage()?;
                match stage.scene.query_ready() {
                    Some(Err(reason)) => {
                        Err(format!("запрос подсветки не скомпилирован: {reason}"))
                    }
                    Some(Ok(_)) if !self.window.viewsNeedDisplay() => {
                        let posted_ns = now_ns();
                        let posted = events::press(Key::OPEN)?;
                        Ok(Step::Opening {
                            posted,
                            posted_ns,
                            deadline: Instant::now() + OPEN_DEADLINE,
                        })
                    }
                    _ if Instant::now() < deadline => Ok(Step::Ready { deadline }),
                    state => Err(format!(
                        "до клавиши не готово за {} с: запрос {state:?}",
                        READY_DEADLINE.as_secs()
                    )),
                }
            }
            Step::Opening {
                posted,
                posted_ns,
                deadline,
            } => {
                if self.opened(&posted, posted_ns)? {
                    if let Some(report) = self.report.borrow_mut().as_mut() {
                        report.metric(
                            "open_ms",
                            "ms",
                            vec![now_ns().saturating_sub(posted_ns) as f64 / 1e6],
                        );
                    }
                    if self.fixture == OpenFixture::Threads {
                        let stage = self.stage()?;
                        let expanded = stage.scene.expand_threads();
                        if expanded == 0 {
                            return Err("у фикстуры threads нет свёрнутых тредов".to_owned());
                        }
                        if let Some(report) = self.report.borrow_mut().as_mut() {
                            report.push("threads_expanded", expanded);
                        }
                        return Ok(Step::Expanding {
                            deadline: Instant::now() + EXPAND_DEADLINE,
                        });
                    }
                    return self.position(self.first());
                }
                if Instant::now() < deadline {
                    Ok(Step::Opening {
                        posted,
                        posted_ns,
                        deadline,
                    })
                } else {
                    Err(format!(
                        "открытие не закончено за {} с",
                        OPEN_DEADLINE.as_secs()
                    ))
                }
            }
            Step::Expanding { deadline } => {
                let stage = self.stage()?;
                if !stage.scene.relayout_pending() && stage.covered_now() {
                    return self.position(self.first());
                }
                if Instant::now() < deadline {
                    Ok(Step::Expanding { deadline })
                } else {
                    Err(format!(
                        "треды не раскрыты и не покрыты за {} с",
                        EXPAND_DEADLINE.as_secs()
                    ))
                }
            }
            Step::Positioning { next, deadline } => {
                let stage = self.stage()?;
                if stage.covered_now() && !stage.scene.relayout_pending() {
                    return self.run(next);
                }
                if Instant::now() < deadline {
                    Ok(Step::Positioning { next, deadline })
                } else {
                    Err(format!(
                        "начальное положение {next:?} не покрыто за {} с",
                        COVER_DEADLINE.as_secs()
                    ))
                }
            }
            Step::Running { next, deadline } => {
                if let Some(after) = self.collect(next)? {
                    return match after {
                        Some(next) => self.position(next),
                        None => Ok(Step::Done),
                    };
                }
                if Instant::now() < deadline {
                    Ok(Step::Running { next, deadline })
                } else {
                    Err(format!("{next:?} не закончен за срок"))
                }
            }
            Step::Done => Ok(Step::Done),
        }
    }

    fn first(&self) -> Next {
        if self.fixture == OpenFixture::Wrap {
            Next::S3
        } else {
            Next::S1(0)
        }
    }

    fn after(&self, done: Next) -> Option<Next> {
        match done {
            Next::S1(index) if index + 1 < Sweep::ALL.len() => Some(Next::S1(index + 1)),
            Next::S1(_) if self.fixture == OpenFixture::Modified => Some(Next::S2),
            Next::S2 => Some(Next::Plain),
            Next::S1(_) | Next::Plain | Next::S3 => None,
        }
    }

    /// Фикстура в памяти: вид ставится в окно, сцена держит применённые снимки.
    fn loaded(&self, fixture: Fixture) -> Result<Step, String> {
        let mut report = self
            .report
            .borrow_mut()
            .take()
            .ok_or_else(|| "итог уже забран".to_owned())?;
        let stage = super::stage::prepare(
            self.mtm,
            &self.window,
            &mut report,
            self.trace.clone(),
            self.variant,
            self.mode,
            fixture,
            "frames",
        );
        *self.report.borrow_mut() = Some(report);
        let stage = Rc::new(stage?);
        let scroll: &NSView = stage.view.scroll_view();
        events::route_wheel_to(Some(scroll.retain()));
        *self.stage.borrow_mut() = Some(stage);
        Ok(Step::Ready {
            deadline: Instant::now() + READY_DEADLINE,
        })
    }

    /// Открытие закончено: клавиша доставлена, снимок полного разбора применён после неё,
    /// видимые строки покрыты, показан запрошенный режим.
    fn opened(&self, posted: &[u64], posted_ns: u64) -> Result<bool, String> {
        let stage = self.stage()?;
        let timeline = stage.scene.timeline();
        if !returned(posted) || !timeline.open_ns.is_some_and(|ns| ns >= posted_ns) {
            return Ok(false);
        }
        let full = timeline
            .snapshots
            .iter()
            .any(|applied| applied.source == Source::Full && applied.at_ns >= posted_ns);
        if !full || !stage.covered_now() || stage.scene.relayout_pending() {
            return Ok(false);
        }
        let shown = stage.scene.mode();
        if shown != self.mode {
            return Err(format!(
                "показан режим {shown:?}, а запрошен {:?}",
                self.mode
            ));
        }
        Ok(true)
    }

    /// Ставит начальное положение и ждёт покрытия: S1 — `y0` отрезка, S2 и ряд без фаз — начало
    /// документа, S3 — без прокрутки.
    fn position(&self, next: Next) -> Result<Step, String> {
        let stage = self.stage()?;
        let limit = self.limit(&stage)?;
        match next {
            Next::S1(index) => stage.view.scroll_to(Sweep::ALL[index].origin(limit)),
            Next::S2 | Next::Plain => stage.view.scroll_to(0.0),
            Next::S3 => {}
        }
        Ok(Step::Positioning {
            next,
            deadline: Instant::now() + COVER_DEADLINE,
        })
    }

    fn limit(&self, stage: &Stage) -> Result<f64, String> {
        let (_, limit) = stage
            .scene
            .scroll_limit()
            .ok_or_else(|| "нет индекса высот".to_owned())?;
        Ok(limit as f64)
    }

    fn run(&self, next: Next) -> Result<Step, String> {
        let stage = self.stage()?;
        self.ensure_link(&stage);
        drawmark::take();
        let deadline = match next {
            Next::S1(index) => {
                let limit = self.limit(&stage)?;
                scroll::fits(limit)?;
                *self.drive.borrow_mut() = Drive::S1 {
                    sweep: Sweep::ALL[index],
                    limit,
                    start_s: None,
                    ticks: Vec::with_capacity(1300),
                    done: false,
                };
                SEGMENT_DEADLINE
            }
            Next::S2 | Next::Plain => {
                let plain = next == Next::Plain;
                let scroll: &NSView = stage.view.scroll_view();
                let in_window = scroll.convertRect_toView(scroll.bounds(), None);
                let screen = self.window.convertRectToScreen(in_window);
                let point = objc2_foundation::NSPoint::new(
                    screen.origin.x + screen.size.width / 2.0,
                    screen.origin.y + screen.size.height / 2.0,
                );
                let primary_height = NSScreen::screens(self.mtm)
                    .firstObject()
                    .map(|screen| screen.frame().size.height)
                    .ok_or_else(|| "нет дисплея для координат колеса".to_owned())?;
                *self.drive.borrow_mut() = Drive::S2 {
                    plain,
                    steps: if plain {
                        scroll::plain()
                    } else {
                        scroll::fling()
                    },
                    point,
                    primary_height,
                    posted: Vec::new(),
                    seen: Vec::new(),
                    ticks: Vec::new(),
                    settle: 0,
                    error: None,
                    done: false,
                };
                S2_DEADLINE
            }
            Next::S3 => {
                let content = self.window.contentRectForFrameRect(self.window.frame());
                *self.drive.borrow_mut() = Drive::S3 {
                    start_s: None,
                    origin: content.origin,
                    height: content.size.height,
                    ticks: Vec::new(),
                    error: None,
                    done: false,
                };
                S3_DEADLINE
            }
        };
        Ok(Step::Running {
            next,
            deadline: Instant::now() + deadline,
        })
    }

    /// `displayLink` у scroll view создаётся один раз и живёт до конца процесса.
    fn ensure_link(&self, stage: &Rc<Stage>) {
        if self.link.borrow().is_some() {
            return;
        }
        let drive = Rc::clone(&self.drive);
        let handler_stage = Rc::clone(stage);
        let window = self.window.clone();
        let driver = FrameDriver::new(
            self.mtm,
            Box::new(move |link| tick(&drive, &handler_stage, &window, link)),
        );
        let scroll: &NSView = stage.view.scroll_view();
        // SAFETY: цель — объект пробы с методом `tick:`, принимающим `CADisplayLink`.
        let link = unsafe { scroll.displayLinkWithTarget_selector(&driver, sel!(tick:)) };
        // SAFETY: ссылка добавляется в цикл главного потока в общих режимах.
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::currentRunLoop(), NSRunLoopCommonModes) };
        *self.link.borrow_mut() = Some((driver, link));
    }

    /// Итог законченного шага: `Some(следующий)` или `Some(None)` в конце, `None` — ещё идёт.
    fn collect(&self, next: Next) -> Result<Option<Option<Next>>, String> {
        let finished = {
            let drive = self.drive.borrow();
            match &*drive {
                Drive::S1 { done, .. } | Drive::S2 { done, .. } | Drive::S3 { done, .. } => *done,
                Drive::Idle => true,
            }
        };
        if !finished {
            return Ok(None);
        }
        let drive = std::mem::replace(&mut *self.drive.borrow_mut(), Drive::Idle);
        let marks: Vec<(u64, u64)> = drawmark::take()
            .iter()
            .map(|mark| (mark.draw_start_ns, mark.draw_end_ns))
            .collect();
        match drive {
            Drive::S1 {
                sweep,
                limit,
                start_s,
                ticks,
                ..
            } => self.collect_s1(sweep, limit, start_s, &ticks, &marks)?,
            Drive::S2 {
                plain,
                steps,
                posted,
                seen,
                ticks,
                error,
                ..
            } => {
                if let Some(error) = error {
                    return Err(error);
                }
                if !scroll::phases_match(&steps, &seen) {
                    return Err(format!(
                        "ошибка стенда: фазы готового NSEvent {seen:?} не совпали с заданными"
                    ));
                }
                let delivered = posted
                    .iter()
                    .filter(|&&number| {
                        events::record(number).is_some_and(|record| {
                            record.dispatch_start.is_some() && record.send_return.is_some()
                        })
                    })
                    .count();
                let journal: Vec<serde_json::Value> = posted
                    .iter()
                    .filter_map(|&number| events::record(number))
                    .map(|record| {
                        serde_json::json!({
                            "label": record.label,
                            "posted_ms": events::to_ms(record.posted),
                            "send_start_ms": record.dispatch_start.map(events::to_ms),
                            "send_return_ms": record.send_return.map(events::to_ms),
                        })
                    })
                    .collect();
                let (via_window, direct) = events::wheel_routes();
                let prefix = if plain { "s2_plain" } else { "s2" };
                if let Some(report) = self.report.borrow_mut().as_mut() {
                    report.push(
                        &format!("{prefix}_events"),
                        serde_json::to_string(&journal).unwrap_or_default(),
                    );
                    report.push(
                        &format!("{prefix}_seen"),
                        serde_json::to_string(&seen).unwrap_or_default(),
                    );
                    report.push(
                        &format!("{prefix}_ticks"),
                        serde_json::to_string(&ticks).unwrap_or_default(),
                    );
                    report.push(
                        &format!("{prefix}_wheel_routes"),
                        format!("через окно {via_window}, стендом виду {direct}"),
                    );
                }
                if plain {
                    *self.plain.borrow_mut() = Some(scroll::judge_plain(&steps, delivered, &ticks));
                } else {
                    *self.fling.borrow_mut() = Some(scroll::judge_fling(&steps, delivered, &ticks));
                }
            }
            Drive::S3 { ticks, error, .. } => {
                if let Some(error) = error {
                    return Err(error);
                }
                self.collect_s3(&ticks);
            }
            Drive::Idle => return Err(format!("{next:?} без журнала вызовов")),
        }
        Ok(Some(self.after(next)))
    }

    fn collect_s1(
        &self,
        sweep: Sweep,
        limit: f64,
        start_s: Option<f64>,
        ticks: &[S1Tick],
        marks: &[(u64, u64)],
    ) -> Result<(), String> {
        let start = start_s.ok_or_else(|| format!("отрезок {} без вызовов", sweep.name()))?;
        let entries: Vec<u64> = ticks.iter().map(|tick| tick.entry_ns).collect();
        let draw = scroll::draw_per_tick(&entries, marks);
        let summary = scroll::s1_summary(start, ticks, &draw)
            .map_err(|reason| format!("отрезок {}: {reason}", sweep.name()))?;
        let expanded_block_row = if self.fixture == OpenFixture::Threads {
            let stage = self.stage()?;
            let row = expanded_row(&stage, summary.rows_passed);
            if row.is_none() {
                return Err(format!(
                    "путь отрезка {} строк {:?} не проходит ни одной строки с раскрытым тредом",
                    sweep.name(),
                    summary.rows_passed
                ));
            }
            row
        } else {
            None
        };
        let window = scroll::measure_window(start);
        self.s1_targets
            .borrow_mut()
            .extend(ticks.iter().map(|tick| tick.target_s).filter(|&target| {
                let at = (target * 1e9).round() as u64;
                window.start_ns <= at && at < window.end_ns
            }));
        if let Some(report) = self.report.borrow_mut().as_mut() {
            report.metric(
                &format!("s1_{}_draw_ms", sweep.name()),
                "ms",
                summary.draw_ms.clone(),
            );
            report.push(
                &format!("s1_{}_ticks", sweep.name()),
                serde_json::to_string(ticks).unwrap_or_default(),
            );
        }
        self.segments.borrow_mut().push(SegmentNote {
            sweep: sweep.name(),
            limit,
            y0: sweep.origin(limit),
            summary,
            expanded_block_row,
        });
        Ok(())
    }

    fn collect_s3(&self, ticks: &[S3Tick]) {
        let stage = self.stage.borrow().clone();
        let relayout = stage.as_ref().map(|stage| stage.scene.relayout());
        let pending = ticks.iter().filter(|tick| tick.pending).count();
        let mut longest = 0usize;
        let mut run = 0usize;
        for tick in ticks {
            run = if tick.pending { run + 1 } else { 0 };
            longest = longest.max(run);
        }
        let uncovered = ticks.iter().filter(|tick| !tick.covered).count();
        let targets: Vec<f64> = ticks.iter().map(|tick| tick.target_s).collect();
        let rhythm = scroll::rhythm(&targets);
        if let Some(report) = self.report.borrow_mut().as_mut() {
            report.push(
                "s3_resize",
                serde_json::json!({
                    "ticks": ticks.len(),
                    "pending_ticks": pending,
                    "longest_pending_run": longest,
                    "uncovered_ticks": uncovered,
                    "rhythm": rhythm,
                    "relayout": relayout,
                }),
            );
            report.push("s3_ticks", serde_json::to_string(ticks).unwrap_or_default());
        }
    }

    /// Журналы в итог; ссылка, стенд и сцена снимаются.
    fn close(&self) -> (Report, Option<String>) {
        let mut report = self.report.borrow_mut().take().unwrap_or_default();
        if self.closed.replace(true) {
            return (report, None);
        }
        if let Some((driver, link)) = self.link.borrow_mut().take() {
            link.invalidate();
            driver.clear();
        }
        *self.drive.borrow_mut() = Drive::Idle;
        let stage = self.stage.borrow_mut().take();
        let Some(stage) = stage else {
            return (report, None);
        };
        let released = events::release_open_gestures();
        let external = events::external_input();
        report.push("released_gestures", released);
        report.push(
            "timeline",
            serde_json::to_string(&stage.scene.timeline()).unwrap_or_default(),
        );
        report.push(
            "document_frame",
            crate::describe_rect(stage.view.document_frame()),
        );
        match &stage.view {
            View::A(view) => {
                report.push(
                    "layout_cache",
                    serde_json::to_string(&view.cache_counters()).unwrap_or_default(),
                );
                report.push(
                    "draw_stats",
                    serde_json::to_string(&view.draw_stats()).unwrap_or_default(),
                );
            }
            View::B(view) => {
                report.push(
                    "table_stats",
                    serde_json::to_string(&view.stats()).unwrap_or_default(),
                );
            }
        }
        match sampler::sample() {
            Ok(sample) => report.metric(
                "phys_footprint_bytes",
                "bytes",
                vec![sample.footprint_bytes as f64],
            ),
            Err(reason) => report.push("sample_error", reason),
        }
        events::route_wheel_to(None);
        drawmark::take();
        drawmark::take_declared();
        drawmark::on_mark(|_| {});
        drawmark::on_declare(|_| {});
        events::clear();
        events::uninstall();
        stage.scene.close();
        (report, external)
    }

    fn fail(&self, reason: String) -> Failed {
        let (report, external) = self.close();
        let reason = match external {
            Some(input) => format!("{reason}; внешний ввод во время сценария: {input}"),
            None => reason,
        };
        Box::new((reason, report))
    }

    fn finish(&self) -> Result<Report, Failed> {
        let (mut report, external) = self.close();
        if let Some(input) = external {
            return Err(Box::new((
                format!("внешний ввод во время сценария: {input}"),
                report,
            )));
        }
        let segments = self.segments.borrow();
        let targets = self.s1_targets.borrow();
        if !segments.is_empty() {
            let rhythm = scroll::rhythm(&targets);
            let rate = scroll::rate_confirmed(rhythm.interval_median_ms);
            report.finding(
                "rate_120",
                if rate { Verdict::Pass } else { Verdict::Fail },
                format!(
                    "медиана интервала targetTimestamp окон замера S1 {:.3} мс по {} вызовам",
                    rhythm.interval_median_ms, rhythm.calls
                ),
            );
            let losses: Vec<String> = segments
                .iter()
                .map(|segment| {
                    let split = segment.summary.undrawn_split;
                    format!(
                        "{}: потерь в процессе {} (пропуски накрыли {}, недорисовано {}: только \
                         отставшие {}, только ненарисованные {}, оба {})",
                        segment.sweep,
                        segment.summary.lost_in_process,
                        segment.summary.rhythm.skipped_ticks,
                        segment.summary.undrawn.len(),
                        split.stale_only,
                        split.undrawn_only,
                        split.both
                    )
                })
                .collect();
            let rate_note = if rate {
                String::new()
            } else {
                "; 120 Гц не подтверждены".to_owned()
            };
            report.finding(
                "frames_s1",
                Verdict::NotRun,
                format!("{NOT_CALIBRATED}{rate_note}; {}", losses.join("; ")),
            );
            report.metric(
                "s1_lost_in_process",
                "ticks",
                segments
                    .iter()
                    .map(|segment| segment.summary.lost_in_process as f64)
                    .collect(),
            );
            report.metric(
                "s1_interval_ms",
                "ms",
                targets
                    .windows(2)
                    .map(|pair| (pair[1] - pair[0]) * 1e3)
                    .filter(|&gap| gap > 0.0)
                    .collect(),
            );
            report.push(
                "s1_segments",
                serde_json::to_string(&*segments).unwrap_or_default(),
            );
        }
        let plain = self.plain.borrow();
        if let Some(plain) = plain.as_ref() {
            report.push("s2_plain", serde_json::to_string(plain).unwrap_or_default());
        }
        if let Some(check) = self.fling.borrow().as_ref() {
            let cause = if check.pass {
                String::new()
            } else {
                format!("; {}", scroll::fling_cause(true, plain.as_ref()))
            };
            report.finding(
                "s2_fling",
                if check.pass {
                    Verdict::Pass
                } else {
                    Verdict::Fail
                },
                format!(
                    "событий с возвратом {} из {}, сдвиг жеста {:.1}, инерции {:.1}, итог {:.1} против {:.0}, успокоился {}{}{cause}",
                    check.delivered,
                    check.events,
                    check.gesture_shift,
                    check.momentum_shift,
                    check.total_shift,
                    check.expected_shift,
                    check.settled,
                    if check.reasons.is_empty() {
                        String::new()
                    } else {
                        format!("; {}", check.reasons.join("; "))
                    }
                ),
            );
        }
        Ok(report)
    }
}

impl crate::scenario::Running for FramesRun {
    fn poll(&self) -> Option<Result<Report, Failed>> {
        FramesRun::poll(self)
    }
}

/// Ошибки сцены и таблицы B — ошибка исполнения.
pub fn stage_errors(stage: &Stage) -> Result<(), String> {
    let errors = stage.scene.errors();
    if !errors.is_empty() {
        return Err(format!("сцена: {}", errors.join("; ")));
    }
    if let View::B(view) = &stage.view {
        let stats = view.stats();
        if stats.height_failures > 0 || stats.frame_mismatches > 0 {
            return Err(format!(
                "таблица B: высот при занятой модели {}, расхождений высоты с индексом {}: {}",
                stats.height_failures,
                stats.frame_mismatches,
                view.mismatches().join("; ")
            ));
        }
    }
    Ok(())
}

/// Первая строка из `rows` с раскрытым тредом под ней.
fn expanded_row(stage: &Stage, rows: (u32, u32)) -> Option<u32> {
    (rows.0..rows.1).find(|&row| {
        stage.scene.block_height(row).is_some_and(|height| height > 0)
            && stage.scene.block_frames(row).iter().any(|frame| {
                matches!(frame.entry.item, BlockItem::Thread(id) if stage.scene.is_expanded(id))
            })
    })
}

/// Вызов `displayLink`.
fn tick(drive: &RefCell<Drive>, stage: &Stage, window: &NSWindow, link: &CADisplayLink) {
    let entry_ns = now_ns();
    let target = link.targetTimestamp();
    let mut drive = drive.borrow_mut();
    match &mut *drive {
        Drive::Idle => {}
        Drive::S1 {
            sweep,
            limit,
            start_s,
            ticks,
            done,
        } => {
            if *done {
                return;
            }
            if let Some(last) = ticks.last_mut() {
                let covered = stage.covered_now();
                last.covered_next = Some(covered);
                if !covered {
                    last.uncovered = stage.census_now();
                }
            }
            let start = *start_s.get_or_insert(target);
            if scroll::s1_done(start, target) {
                *done = true;
                return;
            }
            let y = scroll::s1_y(*sweep, *limit, start, target);
            stage.view.scroll_to(y);
            let rows = stage.scene.visible_rows();
            ticks.push(S1Tick {
                target_s: target,
                entry_ns,
                y,
                top: stage.view.viewport_y(),
                rows: (rows.start, rows.end),
                covered_next: None,
                uncovered: None,
            });
        }
        Drive::S2 {
            plain,
            steps,
            point,
            primary_height,
            posted,
            seen,
            ticks,
            settle,
            error,
            done,
        } => {
            if *done {
                return;
            }
            ticks.push(S2Tick {
                target_s: target,
                top: stage.view.viewport_y(),
                posted: posted.len(),
            });
            if let Some(step) = steps.get(posted.len()).copied() {
                let label = format!(
                    "{} #{}",
                    if *plain { "s2-plain" } else { "s2" },
                    posted.len()
                );
                match events::post_wheel(
                    window,
                    *point,
                    *primary_height,
                    step.delta,
                    step.phase,
                    step.momentum,
                    &label,
                ) {
                    Ok((number, fields)) => {
                        posted.push(number);
                        if seen.len() < PLAIN_EVENTS {
                            seen.push(fields);
                        }
                    }
                    Err(reason) => {
                        *error = Some(format!("событие колеса {label}: {reason}"));
                        *done = true;
                    }
                }
                return;
            }
            if returned(posted) {
                *settle += 1;
                if *settle > FLING_SETTLE_TICKS {
                    *done = true;
                }
            }
        }
        Drive::S3 {
            start_s,
            origin,
            height,
            ticks,
            error,
            done,
        } => {
            if *done {
                return;
            }
            let start = *start_s.get_or_insert(target);
            if target - start >= RESIZE_S {
                *done = true;
                return;
            }
            let index = calibrate::tick_index(start, target);
            let width = scroll::resize_width(index, S3_MAX_WIDTH, S3_MIN_WIDTH);
            let content = NSRect::new(*origin, NSSize::new(width, *height));
            window.setFrame_display(window.frameRectForContentRect(content), true);
            let shown = stage.scene.mode();
            if shown != Mode::SideBySide {
                *error = Some(format!(
                    "S3: на ширине {width} показан режим {shown:?}, а не «Рядом»"
                ));
                *done = true;
                return;
            }
            ticks.push(S3Tick {
                target_s: target,
                entry_ns,
                width,
                pending: stage.scene.relayout_pending(),
                covered: stage.covered_now(),
            });
        }
    }
}
