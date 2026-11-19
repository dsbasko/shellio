//! Сценарий `idle` с окном: фикстура `modified` генерируется на рабочем потоке, клавиша `o`
//! открывает её в виде варианта и режима, после снимка полного разбора, покрытия видимых строк и
//! без перестройки идёт окно простоя 10 с. Тело опроса в окне — одно сравнение времени: таймер
//! стенда на время окна переносит срабатывание на его конец (строка «Таймер опроса в окнах
//! `idle`»). До клавиши открытия идёт контрольное окно 10 с без diff — пол стенда (строка «Пол
//! стенда `idle`»).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::NSWindow;

use probe_diff_view::fixtures::Fixture;
use probe_diff_view::highlight::Source;
use probe_diff_view::model::Mode;
use probe_diff_view::open::OpenFixture;

use super::calibrate_run::Failed;
use super::drawmark;
use super::events::{self, Key};
use super::idle;
use super::sampler::{self, Sample};
use super::stage::Stage;
use super::watchdog::SharedTrace;
use crate::args::{Variant, ViewMode, ViewOptions};
use crate::results::Report;

/// Срок генерации фикстуры на рабочем потоке.
pub const LOAD_DEADLINE: Duration = Duration::from_secs(30);
/// Срок готовности до клавиши.
pub const READY_DEADLINE: Duration = Duration::from_secs(10);
/// Срок открытия до покрытия после снимка полного разбора.
pub const OPEN_DEADLINE: Duration = Duration::from_secs(30);

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
    /// Контрольное окно до клавиши открытия: вид в окне, diff не открыт.
    Floor {
        start: Sample,
        until_ns: u64,
    },
    Idle {
        start: Sample,
        until_ns: u64,
    },
    Done,
}

pub struct IdleRun {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    trace: SharedTrace,
    variant: Variant,
    mode: Mode,
    stage: RefCell<Option<Rc<Stage>>>,
    step: RefCell<Step>,
    summary: RefCell<Option<idle::IdleSummary>>,
    floor: RefCell<Option<idle::IdleSummary>>,
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

impl IdleRun {
    pub fn start(
        mtm: MainThreadMarker,
        window: &NSWindow,
        report: Report,
        trace: SharedTrace,
        view: ViewOptions,
    ) -> Result<IdleRun, Failed> {
        let (variant, mode) = match (view.variant, view.mode) {
            (Some(variant), Some(mode)) => (variant, mode),
            _ => return Err(Box::new((format!("нет оси простоя: {view:?}"), report))),
        };
        let Some(name) = OpenFixture::Modified.synthetic() else {
            return Err(Box::new((
                "фикстура modified не синтетическая".to_owned(),
                report,
            )));
        };
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("probe-diff-idle-load".to_owned())
            .spawn(move || {
                let _ = tx.send(probe_diff_view::fixtures::generate(name));
            });
        if let Err(err) = spawned {
            return Err(Box::new((
                format!("поток загрузки не запущен: {err}"),
                report,
            )));
        }
        Ok(IdleRun {
            mtm,
            window: window.retain(),
            trace,
            variant,
            mode: match mode {
                ViewMode::Unified => Mode::Unified,
                ViewMode::Side => Mode::SideBySide,
            },
            stage: RefCell::new(None),
            step: RefCell::new(Step::Loading {
                rx,
                deadline: Instant::now() + LOAD_DEADLINE,
            }),
            summary: RefCell::new(None),
            floor: RefCell::new(None),
            report: RefCell::new(Some(report)),
            closed: Cell::new(false),
        })
    }

    fn stage(&self) -> Result<Rc<Stage>, String> {
        self.stage
            .borrow()
            .clone()
            .ok_or_else(|| "нет стенда простоя".to_owned())
    }

    pub fn poll(&self) -> Option<Result<Report, Failed>> {
        // Окно простоя: только сравнение времени, без чтения сцены.
        if let Step::Idle { until_ns, .. } | Step::Floor { until_ns, .. } = &*self.step.borrow()
            && now_ns() < *until_ns
        {
            return None;
        }
        if let Some(stage) = self.stage.borrow().clone()
            && let Err(reason) = super::frames_run::stage_errors(&stage)
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
                        drawmark::take();
                        let start = sampler::sample()?;
                        super::request_pause(Duration::from_nanos(idle::WINDOW_NS));
                        Ok(Step::Floor {
                            start,
                            until_ns: start.mono_ns + idle::WINDOW_NS,
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
                    drawmark::take();
                    let start = sampler::sample()?;
                    super::request_pause(Duration::from_nanos(idle::WINDOW_NS));
                    return Ok(Step::Idle {
                        start,
                        until_ns: start.mono_ns + idle::WINDOW_NS,
                    });
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
            Step::Floor { start, .. } => {
                let end = sampler::sample()?;
                let passes = drawmark::take().len();
                *self.floor.borrow_mut() = Some(idle::summarize(start, end, passes)?);
                let posted_ns = now_ns();
                let posted = events::press(Key::OPEN)?;
                Ok(Step::Opening {
                    posted,
                    posted_ns,
                    deadline: Instant::now() + OPEN_DEADLINE,
                })
            }
            Step::Idle { start, .. } => {
                let end = sampler::sample()?;
                let passes = drawmark::take().len();
                *self.summary.borrow_mut() = Some(idle::summarize(start, end, passes)?);
                Ok(Step::Done)
            }
            Step::Done => Ok(Step::Done),
        }
    }

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
            "idle",
        );
        *self.report.borrow_mut() = Some(report);
        *self.stage.borrow_mut() = Some(Rc::new(stage?));
        Ok(Step::Ready {
            deadline: Instant::now() + READY_DEADLINE,
        })
    }

    /// Открытие закончено: клавиша доставлена, снимок полного разбора применён после неё,
    /// видимые строки покрыты, перестройки нет, показан запрошенный режим.
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

    fn close(&self) -> (Report, Option<String>) {
        let mut report = self.report.borrow_mut().take().unwrap_or_default();
        if self.closed.replace(true) {
            return (report, None);
        }
        let Some(stage) = self.stage.borrow_mut().take() else {
            return (report, None);
        };
        let released = events::release_open_gestures();
        let external = events::external_input();
        report.push("released_gestures", released);
        report.push(
            "timeline",
            serde_json::to_string(&stage.scene.timeline()).unwrap_or_default(),
        );
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
        let Some(summary) = *self.summary.borrow() else {
            return Err(Box::new(("нет итога окна простоя".to_owned(), report)));
        };
        let (verdict, reason) = idle::judge(&summary);
        report.finding("idle_cpu", verdict, reason);
        report.metric("idle_cpu_percent", "%", vec![summary.cpu_percent]);
        report.metric("idle_passes", "passes", vec![summary.passes as f64]);
        report.push("idle", serde_json::to_string(&summary).unwrap_or_default());
        if let Some(floor) = *self.floor.borrow() {
            report.metric("idle_stand_floor_percent", "%", vec![floor.cpu_percent]);
            report.metric(
                "idle_stand_floor_passes",
                "passes",
                vec![floor.passes as f64],
            );
            report.push(
                "idle_stand_floor",
                serde_json::to_string(&floor).unwrap_or_default(),
            );
        }
        Ok(report)
    }
}

impl crate::scenario::Running for IdleRun {
    fn poll(&self) -> Option<Result<Report, Failed>> {
        IdleRun::poll(self)
    }
}
