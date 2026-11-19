//! Сценарий `memory` с окном: сверка пика первым делом, затем фазы одного процесса по строке
//! «Фазы `memory`» отчёта — `empty` → `added-unified` → `added-bottom` → `added-side` →
//! `threads-expanded` → `wrap-s3` → `closed`. Выборки `ri_phys_footprint` и
//! `ri_lifetime_max_phys_footprint` берутся на каждом опросе стенда, первая — до действия фазы.
//! Генерация фикстур идёт на рабочем потоке между окнами фаз. Оценка `≈` данных модели — в конце
//! фаз `added-side`, `threads-expanded` и `wrap-s3`, деревья tree-sitter — счётчиком живых байт
//! распределителя tree-sitter, поставленного до первого вызова tree-sitter в процессе.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, Message, sel};
use objc2_app_kit::{NSView, NSWindow};
use objc2_foundation::{NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize};
use objc2_quartz_core::CADisplayLink;
use serde::Serialize;

use probe_diff_view::estimate::{self, Estimate, Trees};
use probe_diff_view::fixtures::{Fixture, FixtureName};
use probe_diff_view::highlight::Source;
use probe_diff_view::model::Mode;
use probe_diff_view::model::state::Command;
use probe_diff_view::open::OpenFixture;

use super::calibrate;
use super::calibrate_run::Failed;
use super::drawmark;
use super::events::{self, Key};
use super::frames_run::{S3_MAX_WIDTH, S3_MIN_WIDTH};
use super::memory::{self, Phase, PhaseSummary};
use super::sampler::{self, PeakCheck, Sample};
use super::scroll;
use super::stage::{Stage, View};
use super::stand::FrameDriver;
use super::watchdog::SharedTrace;
use crate::args::{Variant, ViewOptions};
use crate::results::{Report, Verdict};

/// Срок генерации фикстуры на рабочем потоке.
pub const LOAD_DEADLINE: Duration = Duration::from_secs(30);
/// Срок готовности до клавиши открытия.
pub const READY_DEADLINE: Duration = Duration::from_secs(10);
/// Срок открытия до покрытия после снимка полного разбора.
pub const OPEN_DEADLINE: Duration = Duration::from_secs(30);
/// Срок покрытия после действия фазы и после раскрытия тредов.
pub const SETTLE_DEADLINE: Duration = Duration::from_secs(10);
/// Длительность ресайза S3 фазы `wrap-s3` по `targetTimestamp`, секунды.
pub const RESIZE_S: f64 = 1.0;

static LIVE: AtomicI64 = AtomicI64::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

fn counted(ptr: *mut c_void) -> i64 {
    if ptr.is_null() {
        0
    } else {
        // SAFETY: `ptr` выдан `malloc`, `calloc` или `realloc` libc и ещё не освобождён.
        unsafe { libc::malloc_size(ptr) as i64 }
    }
}

unsafe extern "C" fn ts_malloc(size: usize) -> *mut c_void {
    // SAFETY: обычный вызов libc.
    let ptr = unsafe { libc::malloc(size) };
    if ptr.is_null() && size != 0 {
        std::process::abort();
    }
    LIVE.fetch_add(counted(ptr), Ordering::Relaxed);
    ptr
}

unsafe extern "C" fn ts_calloc(count: usize, size: usize) -> *mut c_void {
    // SAFETY: обычный вызов libc.
    let ptr = unsafe { libc::calloc(count, size) };
    if ptr.is_null() && count != 0 && size != 0 {
        std::process::abort();
    }
    LIVE.fetch_add(counted(ptr), Ordering::Relaxed);
    ptr
}

unsafe extern "C" fn ts_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    let before = counted(ptr);
    // SAFETY: `ptr` выдан этим же семейством функций или пуст.
    let next = unsafe { libc::realloc(ptr, size) };
    if next.is_null() {
        if size != 0 {
            std::process::abort();
        }
        LIVE.fetch_sub(before, Ordering::Relaxed);
    } else {
        LIVE.fetch_add(counted(next) - before, Ordering::Relaxed);
    }
    next
}

unsafe extern "C" fn ts_free(ptr: *mut c_void) {
    LIVE.fetch_sub(counted(ptr), Ordering::Relaxed);
    // SAFETY: `ptr` выдан этим же семейством функций или пуст.
    unsafe { libc::free(ptr) };
}

/// Ставит распределитель tree-sitter со счётчиком живых байт по `malloc_size`. Вызывается в
/// начале процесса `memory`, до первого вызова tree-sitter (до `check_grammar`).
pub fn install_counter() {
    // SAFETY: четыре функции одного семейства libc, выравнивание как у `malloc`, ненулевой
    // результат для ненулевого размера; вызов идёт до любого другого вызова tree-sitter и до
    // запуска потоков процесса.
    unsafe {
        tree_sitter::set_allocator(Some(tree_sitter::Allocator {
            malloc: ts_malloc,
            calloc: ts_calloc,
            realloc: ts_realloc,
            free: ts_free,
        }));
    }
    COUNTING.store(true, Ordering::Release);
}

/// Живые выделения tree-sitter: деревья обеих сторон, парсеры и курсоры запросов.
fn trees() -> Trees {
    if COUNTING.load(Ordering::Acquire) {
        Trees::Counted {
            bytes: LIVE.load(Ordering::Relaxed).max(0) as u64,
        }
    } else {
        Trees::NotEstimated {
            reason: "распределитель tree-sitter не поставлен".to_owned(),
        }
    }
}

enum Step {
    Loading {
        phase: Phase,
        rx: Receiver<Fixture>,
        deadline: Instant,
    },
    Ready {
        phase: Phase,
        begun: bool,
        deadline: Instant,
    },
    Opening {
        phase: Phase,
        posted: Vec<u64>,
        posted_ns: u64,
        deadline: Instant,
    },
    Settling {
        phase: Phase,
        deadline: Instant,
    },
    Resizing {
        phase: Phase,
        deadline: Instant,
    },
    Holding {
        phase: Phase,
        until_ns: u64,
    },
    Done,
}

/// Ресайз S3 одной секунды по `targetTimestamp`.
struct Resize {
    start_s: Option<f64>,
    origin: objc2_foundation::NSPoint,
    height: f64,
    ticks: usize,
    error: Option<String>,
    done: bool,
}

/// Оценка `≈` в конце фазы.
#[derive(Clone, Debug, Serialize)]
struct PhaseEstimate {
    phase: Phase,
    estimate: Estimate,
    layout_cache: String,
}

pub struct MemoryRun {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    trace: SharedTrace,
    variant: Variant,
    peak: PeakCheck,
    stage: RefCell<Option<Rc<Stage>>>,
    fixture: RefCell<Option<Fixture>>,
    step: RefCell<Step>,
    current: RefCell<Option<(Phase, Vec<Sample>)>>,
    samples: RefCell<Vec<(Phase, Vec<Sample>)>>,
    estimates: RefCell<Vec<PhaseEstimate>>,
    resize: Rc<RefCell<Resize>>,
    link: RefCell<Option<(Retained<FrameDriver>, Retained<CADisplayLink>)>>,
    external: RefCell<Vec<String>>,
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

fn generate(name: FixtureName) -> Result<Receiver<Fixture>, String> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("probe-diff-memory-load".to_owned())
        .spawn(move || {
            let _ = tx.send(probe_diff_view::fixtures::generate(name));
        })
        .map_err(|err| format!("поток загрузки не запущен: {err}"))?;
    Ok(rx)
}

fn synthetic(fixture: OpenFixture) -> Result<FixtureName, String> {
    fixture
        .synthetic()
        .ok_or_else(|| format!("фикстура {fixture:?} не синтетическая"))
}

/// Фикстура и режим фазы с открытием.
fn opening(phase: Phase) -> Option<(OpenFixture, Mode)> {
    match phase {
        Phase::AddedUnified => Some((OpenFixture::Added, Mode::Unified)),
        Phase::AddedSide => Some((OpenFixture::Added, Mode::SideBySide)),
        Phase::Threads => Some((OpenFixture::Threads, Mode::SideBySide)),
        Phase::WrapResize => Some((OpenFixture::Wrap, Mode::SideBySide)),
        Phase::Empty | Phase::AddedBottom | Phase::Closed => None,
    }
}

impl MemoryRun {
    pub fn start(
        mtm: MainThreadMarker,
        window: &NSWindow,
        report: Report,
        trace: SharedTrace,
        view: ViewOptions,
    ) -> Result<MemoryRun, Failed> {
        let Some(variant) = view.variant else {
            return Err(Box::new((format!("нет оси памяти: {view:?}"), report)));
        };
        let peak = match sampler::check_peak() {
            Ok(peak) => peak,
            Err(reason) => {
                return Err(Box::new((
                    format!("сверка пика не выполнена: {reason}"),
                    report,
                )));
            }
        };
        let run = MemoryRun {
            mtm,
            window: window.retain(),
            trace,
            variant,
            peak,
            stage: RefCell::new(None),
            fixture: RefCell::new(None),
            step: RefCell::new(Step::Done),
            current: RefCell::new(None),
            samples: RefCell::new(Vec::new()),
            estimates: RefCell::new(Vec::new()),
            resize: Rc::new(RefCell::new(Resize {
                start_s: None,
                origin: objc2_foundation::NSPoint::ZERO,
                height: 0.0,
                ticks: 0,
                error: None,
                done: true,
            })),
            link: RefCell::new(None),
            external: RefCell::new(Vec::new()),
            report: RefCell::new(Some(report)),
            closed: Cell::new(false),
        };
        if let Err(reason) = run.begin(Phase::Empty) {
            return Err(run.fail(reason));
        }
        *run.step.borrow_mut() = Step::Holding {
            phase: Phase::Empty,
            until_ns: now_ns() + memory::PHASE_NS,
        };
        Ok(run)
    }

    fn stage(&self) -> Result<Rc<Stage>, String> {
        self.stage
            .borrow()
            .clone()
            .ok_or_else(|| "нет стенда памяти".to_owned())
    }

    /// Начало окна фазы: первая выборка до её действия.
    fn begin(&self, phase: Phase) -> Result<(), String> {
        let sample = sampler::sample()?;
        *self.current.borrow_mut() = Some((phase, vec![sample]));
        Ok(())
    }

    fn end(&self, phase: Phase) -> Result<(), String> {
        let Some((current, mut samples)) = self.current.borrow_mut().take() else {
            return Err(format!("фаза {} не начата", phase.name()));
        };
        if current != phase {
            return Err(format!(
                "идёт фаза {}, а закрывается {}",
                current.name(),
                phase.name()
            ));
        }
        samples.push(sampler::sample()?);
        self.samples.borrow_mut().push((phase, samples));
        Ok(())
    }

    pub fn poll(&self) -> Option<Result<Report, Failed>> {
        if let Some((_, samples)) = self.current.borrow_mut().as_mut() {
            match sampler::sample() {
                Ok(sample) => samples.push(sample),
                Err(reason) => return Some(Err(self.fail(reason))),
            }
        }
        let stage = self.stage.borrow().clone();
        if let Some(stage) = stage
            && let Err(reason) = super::frames_run::stage_errors(&stage)
        {
            return Some(Err(self.fail(reason)));
        }
        drawmark::take();
        drawmark::take_declared();
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
            Step::Loading {
                phase,
                rx,
                deadline,
            } => match rx.try_recv() {
                Ok(fixture) => {
                    self.load(phase, fixture)?;
                    Ok(Step::Ready {
                        phase,
                        begun: false,
                        deadline: Instant::now() + READY_DEADLINE,
                    })
                }
                Err(TryRecvError::Empty) if Instant::now() < deadline => Ok(Step::Loading {
                    phase,
                    rx,
                    deadline,
                }),
                Err(TryRecvError::Empty) => Err(format!(
                    "фикстура фазы {} не сгенерирована за {} с",
                    phase.name(),
                    LOAD_DEADLINE.as_secs()
                )),
                Err(TryRecvError::Disconnected) => Err("поток загрузки упал".to_owned()),
            },
            Step::Ready {
                phase,
                begun,
                deadline,
            } => {
                let stage = self.stage()?;
                match stage.scene.query_ready() {
                    Some(Err(reason)) => {
                        Err(format!("запрос подсветки не скомпилирован: {reason}"))
                    }
                    Some(Ok(_)) if !self.window.viewsNeedDisplay() => {
                        if !begun {
                            self.begin(phase)?;
                        }
                        let posted_ns = now_ns();
                        let posted = events::press(Key::OPEN)?;
                        Ok(Step::Opening {
                            phase,
                            posted,
                            posted_ns,
                            deadline: Instant::now() + OPEN_DEADLINE,
                        })
                    }
                    _ if Instant::now() < deadline => Ok(Step::Ready {
                        phase,
                        begun,
                        deadline,
                    }),
                    state => Err(format!(
                        "фаза {}: до клавиши не готово за {} с: запрос {state:?}",
                        phase.name(),
                        READY_DEADLINE.as_secs()
                    )),
                }
            }
            Step::Opening {
                phase,
                posted,
                posted_ns,
                deadline,
            } => {
                if self.opened(phase, &posted, posted_ns)? {
                    return match phase {
                        Phase::Threads => {
                            let expanded = self.stage()?.scene.expand_threads();
                            if expanded == 0 {
                                return Err("у threads не раскрыт ни один тред".to_owned());
                            }
                            Ok(Step::Settling {
                                phase,
                                deadline: Instant::now() + SETTLE_DEADLINE,
                            })
                        }
                        Phase::WrapResize => self.start_resize(phase),
                        _ => Ok(self.hold(phase)),
                    };
                }
                if Instant::now() < deadline {
                    Ok(Step::Opening {
                        phase,
                        posted,
                        posted_ns,
                        deadline,
                    })
                } else {
                    Err(format!(
                        "фаза {}: открытие не закончено за {} с",
                        phase.name(),
                        OPEN_DEADLINE.as_secs()
                    ))
                }
            }
            Step::Settling { phase, deadline } => {
                let stage = self.stage()?;
                if self.settled(&stage) {
                    return Ok(self.hold(phase));
                }
                if Instant::now() < deadline {
                    Ok(Step::Settling { phase, deadline })
                } else {
                    Err(format!(
                        "фаза {}: видимые строки не покрыты за {} с",
                        phase.name(),
                        SETTLE_DEADLINE.as_secs()
                    ))
                }
            }
            Step::Resizing { phase, deadline } => {
                let (done, error) = {
                    let resize = self.resize.borrow();
                    (resize.done, resize.error.clone())
                };
                if let Some(error) = error {
                    return Err(error);
                }
                if done {
                    self.stop_link();
                    return Ok(Step::Settling {
                        phase,
                        deadline: Instant::now() + SETTLE_DEADLINE,
                    });
                }
                if Instant::now() < deadline {
                    Ok(Step::Resizing { phase, deadline })
                } else {
                    Err(format!(
                        "ресайз S3 не закончен за {} с",
                        SETTLE_DEADLINE.as_secs()
                    ))
                }
            }
            Step::Holding { phase, until_ns } => {
                if now_ns() < until_ns {
                    return Ok(Step::Holding { phase, until_ns });
                }
                self.next(phase)
            }
            Step::Done => Ok(Step::Done),
        }
    }

    fn hold(&self, phase: Phase) -> Step {
        Step::Holding {
            phase,
            until_ns: now_ns() + memory::PHASE_NS,
        }
    }

    /// Конец окна фазы и действие следующей.
    fn next(&self, phase: Phase) -> Result<Step, String> {
        if matches!(phase, Phase::AddedSide | Phase::Threads | Phase::WrapResize) {
            self.estimate(phase)?;
        }
        self.end(phase)?;
        match phase {
            Phase::Empty => self.loading(Phase::AddedUnified),
            Phase::AddedUnified => {
                self.begin(Phase::AddedBottom)?;
                self.stage()?.scene.command(Command::Bottom);
                Ok(Step::Settling {
                    phase: Phase::AddedBottom,
                    deadline: Instant::now() + SETTLE_DEADLINE,
                })
            }
            Phase::AddedBottom => {
                self.begin(Phase::AddedSide)?;
                let fixture = self
                    .fixture
                    .borrow()
                    .clone()
                    .ok_or_else(|| "нет фикстуры added".to_owned())?;
                self.drop_stage();
                self.load(Phase::AddedSide, fixture)?;
                Ok(Step::Ready {
                    phase: Phase::AddedSide,
                    begun: true,
                    deadline: Instant::now() + READY_DEADLINE,
                })
            }
            Phase::AddedSide => {
                self.drop_stage();
                self.loading(Phase::Threads)
            }
            Phase::Threads => {
                self.drop_stage();
                self.loading(Phase::WrapResize)
            }
            Phase::WrapResize => {
                self.begin(Phase::Closed)?;
                self.drop_stage();
                Ok(self.hold(Phase::Closed))
            }
            Phase::Closed => Ok(Step::Done),
        }
    }

    /// Генерация фикстуры следующей фазы на рабочем потоке: прежняя сцена уже снята.
    fn loading(&self, phase: Phase) -> Result<Step, String> {
        let (fixture, _) =
            opening(phase).ok_or_else(|| format!("у фазы {} нет фикстуры", phase.name()))?;
        Ok(Step::Loading {
            phase,
            rx: generate(synthetic(fixture)?)?,
            deadline: Instant::now() + LOAD_DEADLINE,
        })
    }

    fn load(&self, phase: Phase, fixture: Fixture) -> Result<(), String> {
        let (_, mode) =
            opening(phase).ok_or_else(|| format!("у фазы {} нет открытия", phase.name()))?;
        let mut report = self
            .report
            .borrow_mut()
            .take()
            .ok_or_else(|| "итог уже забран".to_owned())?;
        *self.fixture.borrow_mut() = Some(fixture.clone());
        let stage = super::stage::prepare(
            self.mtm,
            &self.window,
            &mut report,
            self.trace.clone(),
            self.variant,
            mode,
            fixture,
            "memory",
        );
        *self.report.borrow_mut() = Some(report);
        *self.stage.borrow_mut() = Some(Rc::new(stage?));
        Ok(())
    }

    /// Открытие закончено: клавиша доставлена, снимок полного разбора применён после неё,
    /// видимые строки покрыты без перестройки, показан режим фазы.
    fn opened(&self, phase: Phase, posted: &[u64], posted_ns: u64) -> Result<bool, String> {
        let stage = self.stage()?;
        let timeline = stage.scene.timeline();
        if !returned(posted) || !timeline.open_ns.is_some_and(|ns| ns >= posted_ns) {
            return Ok(false);
        }
        let full = timeline
            .snapshots
            .iter()
            .any(|applied| applied.source == Source::Full && applied.at_ns >= posted_ns);
        if !full || !self.settled(&stage) {
            return Ok(false);
        }
        let expected = opening(phase).map(|(_, mode)| mode);
        let shown = stage.scene.mode();
        if expected != Some(shown) {
            return Err(format!(
                "фаза {}: показан режим {shown:?}, а нужен {expected:?}",
                phase.name()
            ));
        }
        Ok(true)
    }

    fn settled(&self, stage: &Stage) -> bool {
        stage.covered_now() && !stage.scene.relayout_pending()
    }

    fn start_resize(&self, phase: Phase) -> Result<Step, String> {
        let stage = self.stage()?;
        let content = self.window.contentRectForFrameRect(self.window.frame());
        *self.resize.borrow_mut() = Resize {
            start_s: None,
            origin: content.origin,
            height: content.size.height,
            ticks: 0,
            error: None,
            done: false,
        };
        let resize = Rc::clone(&self.resize);
        let handler_stage = Rc::clone(&stage);
        let window = self.window.clone();
        let driver = FrameDriver::new(
            self.mtm,
            Box::new(move |link| resize_tick(&resize, &handler_stage, &window, link)),
        );
        let scroll: &NSView = stage.view.scroll_view();
        // SAFETY: цель — объект пробы с методом `tick:`, принимающим `CADisplayLink`.
        let link = unsafe { scroll.displayLinkWithTarget_selector(&driver, sel!(tick:)) };
        // SAFETY: ссылка добавляется в цикл главного потока в общих режимах.
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::currentRunLoop(), NSRunLoopCommonModes) };
        *self.link.borrow_mut() = Some((driver, link));
        Ok(Step::Resizing {
            phase,
            deadline: Instant::now() + SETTLE_DEADLINE,
        })
    }

    fn stop_link(&self) {
        if let Some((driver, link)) = self.link.borrow_mut().take() {
            link.invalidate();
            driver.clear();
        }
    }

    fn estimate(&self, phase: Phase) -> Result<(), String> {
        let stage = self.stage()?;
        let mut estimate = Estimate::new(trees());
        if let Some(fixture) = self.fixture.borrow().as_ref() {
            estimate.add("текст diff", estimate::slice_bytes(&fixture.diff));
            estimate.add("blob старой стороны", estimate::slice_bytes(&fixture.old));
            estimate.add("blob новой стороны", estimate::slice_bytes(&fixture.new));
        }
        {
            let model = stage.scene.model().borrow();
            if let Some(snapshot) = &model.diff {
                estimate.add("строки diff и ханки", snapshot.diff.lines_bytes());
                estimate.add(
                    "строки показа «Рядом» и индексы адресов",
                    snapshot.diff.rows_bytes(),
                );
            }
            if let Some(heights) = &model.heights {
                estimate.add("индекс высот", heights.heap_bytes());
            }
        }
        let layout_cache = match &stage.view {
            View::A(view) => {
                let (len, capacity) = view.cache_len();
                format!("кэш раскладки A: {len} записей при пределе {capacity}")
            }
            View::B(_) => "у B кэша раскладки нет".to_owned(),
        };
        self.estimates.borrow_mut().push(PhaseEstimate {
            phase,
            estimate,
            layout_cache,
        });
        Ok(())
    }

    /// Снимает сцену и вид: стенд событий, наблюдатели `drawmark`, содержимое окна.
    fn drop_stage(&self) {
        self.stop_link();
        *self.fixture.borrow_mut() = None;
        let Some(stage) = self.stage.borrow_mut().take() else {
            return;
        };
        events::release_open_gestures();
        if let Some(input) = events::external_input() {
            self.external.borrow_mut().push(input);
        }
        drawmark::take();
        drawmark::take_declared();
        drawmark::on_mark(|_| {});
        drawmark::on_declare(|_| {});
        events::clear();
        events::uninstall();
        stage.scene.close();
        self.window.setContentView(None);
        drop(stage);
    }

    fn close(&self) -> (Report, Option<String>) {
        let report = self.report.borrow_mut().take().unwrap_or_default();
        if !self.closed.replace(true) {
            self.drop_stage();
        }
        let external = self.external.borrow();
        let external = (!external.is_empty()).then(|| external.join("; "));
        (report, external)
    }

    fn fail(&self, reason: String) -> Failed {
        let (mut report, external) = self.close();
        self.push_raw(&mut report);
        let reason = match external {
            Some(input) => format!("{reason}; внешний ввод во время сценария: {input}"),
            None => reason,
        };
        Box::new((reason, report))
    }

    fn push_raw(&self, report: &mut Report) {
        report.push(
            "memory_peak_check",
            serde_json::to_string(&self.peak).unwrap_or_default(),
        );
        report.push(
            "memory_samples",
            serde_json::to_string(&*self.samples.borrow()).unwrap_or_default(),
        );
        for estimate in self.estimates.borrow().iter() {
            report.push(
                &format!("memory_estimate_{}", estimate.phase.name()),
                format!("{}; {}", estimate.estimate.line(), estimate.layout_cache),
            );
        }
    }

    fn finish(&self) -> Result<Report, Failed> {
        let (mut report, external) = self.close();
        self.push_raw(&mut report);
        if let Some(input) = external {
            return Err(Box::new((
                format!("внешний ввод во время сценария: {input}"),
                report,
            )));
        }
        let checked = self.peak.failure.is_none();
        match &self.peak.failure {
            None => report.finding(
                "memory_peak_check",
                Verdict::Pass,
                "ri_lifetime_max_phys_footprint растёт на контрольном выделении 64 МиБ",
            ),
            Some(reason) => report.finding(
                "memory_peak_check",
                Verdict::NotRun,
                format!("пик между выборками не наблюдается: {reason}"),
            ),
        }
        let samples = self.samples.borrow();
        let floor = samples
            .iter()
            .filter(|(phase, _)| phase.judged())
            .filter_map(|(_, samples)| samples.first())
            .map(|sample| sample.lifetime_max_bytes)
            .max()
            .unwrap_or(0);
        report.metric("memory_peak_floor_bytes", "bytes", vec![floor as f64]);
        let between = checked && floor < memory::BUDGET_BYTES;
        if checked && !between {
            report.finding(
                "memory_peak_between",
                Verdict::NotRun,
                format!("пол пика не ниже бюджета: {floor} байт"),
            );
        }
        let mut phases: Vec<PhaseSummary> = Vec::with_capacity(samples.len());
        for (phase, samples) in samples.iter() {
            match memory::summarize(*phase, samples, between) {
                Ok(summary) => phases.push(summary),
                Err(reason) => return Err(Box::new((reason, report))),
            }
        }
        let transitions: Vec<(String, u64)> = phases
            .windows(2)
            .filter(|pair| pair[1].lifetime_first > pair[0].lifetime_last)
            .map(|pair| {
                (
                    format!("{} → {}", pair[0].phase.name(), pair[1].phase.name()),
                    pair[1].lifetime_first,
                )
            })
            .collect();
        report.push(
            "transition_peak_bytes",
            serde_json::to_string(&transitions).unwrap_or_default(),
        );
        match memory::judge(&phases) {
            Ok((verdict, reason)) => report.finding("memory_budget", verdict, reason),
            Err(reason) => return Err(Box::new((reason, report))),
        }
        for summary in &phases {
            report.metric(
                &format!(
                    "memory_{}_peak_bytes",
                    summary.phase.name().replace('-', "_")
                ),
                "bytes",
                vec![summary.peak as f64],
            );
        }
        report.push(
            "memory_phases",
            serde_json::to_string(&phases).unwrap_or_default(),
        );
        Ok(report)
    }
}

fn resize_tick(resize: &RefCell<Resize>, stage: &Stage, window: &NSWindow, link: &CADisplayLink) {
    let mut resize = resize.borrow_mut();
    if resize.done {
        return;
    }
    let target = link.targetTimestamp();
    let start = *resize.start_s.get_or_insert(target);
    if target - start >= RESIZE_S {
        resize.done = true;
        return;
    }
    let index = calibrate::tick_index(start, target);
    let width = scroll::resize_width(index, S3_MAX_WIDTH, S3_MIN_WIDTH);
    let content = NSRect::new(resize.origin, NSSize::new(width, resize.height));
    window.setFrame_display(window.frameRectForContentRect(content), true);
    let shown = stage.scene.mode();
    if shown != Mode::SideBySide {
        resize.error = Some(format!(
            "S3: на ширине {width} показан режим {shown:?}, а не «Рядом»"
        ));
        resize.done = true;
        return;
    }
    resize.ticks += 1;
}

impl crate::scenario::Running for MemoryRun {
    fn poll(&self) -> Option<Result<Report, Failed>> {
        MemoryRun::poll(self)
    }
}
