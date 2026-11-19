//! Общий исполнитель сценариев контракта корректности (задача 14): фикстура и её сырой разбор
//! генерируются на рабочем потоке, клавиша `o` открывает их в виде варианта и режима, как у
//! `keys`, затем сценарий выдаёт шаги «стимул, ожидание, проверка» по строкам отчёта
//! «… контракта». Стенд пересобирается по запросу сценария: малая фикстура `pair31` у `pairs`,
//! циклы открытия и закрытия у `bounded`.
//!
//! Расхождение оракула — проверка с `ok: false` и `fail` части контракта. Ошибка исполнения —
//! только сломанный стенд: истёкший срок ожидания, стимул без эффекта, внешний ввод, ошибки
//! сцены и таблицы B.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::NSWindow;
use objc2_foundation::{NSPoint, NSRect, NSSize};

use probe_diff_view::fixtures::Fixture;
use probe_diff_view::highlight::Source;
use probe_diff_view::model::state::{Anchor, Command};
use probe_diff_view::model::{Mode, Side};
use probe_diff_view::open::OpenFixture;

use super::calibrate_run::Failed;
use super::contract::{self, Check, Raw};
use super::drawmark;
use super::events::{self, Key};
use super::sampler;
use super::stage::Stage;
use super::watchdog::SharedTrace;
use crate::args::{Scenario, Variant, ViewMode, ViewOptions};
use crate::results::Report;

/// Сроки из строки отчёта «Сроки контракта».
pub const LOAD_DEADLINE: Duration = Duration::from_secs(30);
pub const READY_DEADLINE: Duration = Duration::from_secs(10);
pub const OPEN_DEADLINE: Duration = Duration::from_secs(30);
/// Шаг клавиш, щелчков, выделения, тредов и прокрутки.
pub const STEP_DEADLINE: Duration = Duration::from_secs(2);
/// Смена ширины и режима: перестройка индекса высот `wrap`.
pub const RELAYOUT_DEADLINE: Duration = Duration::from_secs(10);
/// Цикл открытия и закрытия `bounded`.
pub const CYCLE_DEADLINE: Duration = Duration::from_secs(30);

/// Часть контракта — сценарий.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Anchor,
    Selection,
    Threads,
    Pairs,
    Bounded,
}

impl Part {
    pub fn of(scenario: Scenario) -> Option<Part> {
        Some(match scenario {
            Scenario::Anchor => Part::Anchor,
            Scenario::Selection => Part::Selection,
            Scenario::Threads => Part::Threads,
            Scenario::Pairs => Part::Pairs,
            Scenario::Bounded => Part::Bounded,
            Scenario::Smoke
            | Scenario::Highlight
            | Scenario::Calibrate
            | Scenario::OpenSmoke
            | Scenario::Ax
            | Scenario::Open
            | Scenario::OpenWarm
            | Scenario::Frames
            | Scenario::Memory
            | Scenario::Keys
            | Scenario::Idle => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Part::Anchor => "anchor",
            Part::Selection => "selection",
            Part::Threads => "threads",
            Part::Pairs => "pairs",
            Part::Bounded => "bounded",
        }
    }

    /// Фикстура процесса (строка отчёта «Процессы контракта»): у `anchor` и `pairs` — ось
    /// фикстур, у остальных своя.
    pub fn fixture(self, given: Option<OpenFixture>) -> Result<OpenFixture, String> {
        let own = match self {
            Part::Selection | Part::Threads => OpenFixture::Threads,
            Part::Bounded => OpenFixture::Modified,
            Part::Anchor | Part::Pairs => {
                return given.ok_or_else(|| format!("у {} нет оси фикстур", self.name()));
            }
        };
        match given {
            None => Ok(own),
            Some(fixture) if fixture == own => Ok(own),
            Some(fixture) => Err(format!(
                "у {} фикстура {}, а не {}",
                self.name(),
                own.name(),
                fixture.name()
            )),
        }
    }
}

/// Условие сценария сверх успокоения вида.
pub type Until = Box<dyn Fn(&Ctx<'_>) -> Result<bool, String>>;

/// Ожидание после стимула: события доставлены, вид успокоился, условие сценария выполнено.
pub struct Wait {
    pub name: String,
    pub posted: Vec<u64>,
    pub deadline: Duration,
    pub until: Option<Until>,
}

impl Wait {
    pub fn new(name: impl Into<String>, posted: Vec<u64>, deadline: Duration) -> Wait {
        Wait {
            name: name.into(),
            posted,
            deadline,
            until: None,
        }
    }

    pub fn until(mut self, until: impl Fn(&Ctx<'_>) -> Result<bool, String> + 'static) -> Wait {
        self.until = Some(Box::new(until));
        self
    }
}

/// Пересборка стенда: прежняя сцена и вид снимаются, новые открывают `fixture`. `quiet` —
/// наблюдения подготовки не идут в итог (100 циклов `bounded`). `cycle` — цикл `bounded`:
/// пересборка, готовность и открытие укладываются в [`CYCLE_DEADLINE`].
pub struct Restage {
    pub fixture: Fixture,
    pub quiet: bool,
    pub cycle: bool,
}

pub enum Next {
    Wait(Wait),
    Restage(Restage),
    Done,
}

/// Что видит шаг сценария.
pub struct Ctx<'a> {
    pub mtm: MainThreadMarker,
    pub window: &'a NSWindow,
    pub stage: &'a Stage,
    pub raw: &'a Raw,
    pub fixture: &'a Fixture,
    /// Запрошенный режим ячейки.
    pub mode: Mode,
}

impl Ctx<'_> {
    /// Показанный режим.
    pub fn shown_mode(&self) -> Mode {
        self.stage.scene.mode()
    }

    /// Якорь прокрутки модели и его строка показа.
    pub fn anchor_row(&self) -> Option<(Anchor, u32)> {
        let anchor = self.stage.scene.anchor()?;
        let model = self.stage.scene.model().borrow();
        let shown = model.shown()?;
        let row = shown.body.row_of_addr(model.mode, anchor.addr)?.index;
        Some((anchor, row))
    }

    /// Верх строки показа в документе по установленному индексу высот.
    pub fn top_of(&self, row: u32) -> Option<f64> {
        let model = self.stage.scene.model().borrow();
        model.shown()?.heights.top(row).map(|top| top as f64)
    }

    /// Высота текста строки показа.
    pub fn text_height(&self, row: u32) -> Option<u32> {
        let model = self.stage.scene.model().borrow();
        model.shown()?.heights.text_height(row)
    }

    pub fn line_height(&self) -> u32 {
        self.stage.scene.model().borrow().font.line_height
    }

    /// Счётчик перестроек раскладки: установленные индексы и смены геометрии без перестройки.
    pub fn relayouts(&self) -> u64 {
        let relayout = self.stage.scene.relayout();
        relayout.installed + relayout.geometry_only
    }

    /// Высота видимой области документа.
    pub fn viewport_height(&self) -> f64 {
        self.stage
            .view
            .scroll_view()
            .contentView()
            .bounds()
            .size
            .height
    }

    /// Ширина содержимого окна.
    pub fn content_width(&self) -> f64 {
        self.window
            .contentRectForFrameRect(self.window.frame())
            .size
            .width
    }

    /// Сменить ширину содержимого окна на `width`, верхний левый угол на месте.
    pub fn set_content_width(&self, width: f64) {
        let frame = self.window.frame();
        let content = self.window.contentRectForFrameRect(frame);
        let next = NSRect::new(content.origin, NSSize::new(width, content.size.height));
        self.window
            .setFrame_display(self.window.frameRectForContentRect(next), true);
    }

    /// Клавиши стенда событий по имени последовательности.
    pub fn press(&self, name: &str) -> Result<Vec<u64>, String> {
        events::press_sequence(name)
    }

    /// Точка экрана для щелчка по строке показа: в «Рядом» центр половины `side`, в unified
    /// центр левых 400 точек строки, как у `open-smoke`.
    pub fn point(&self, row: u32, side: Side) -> Result<NSPoint, String> {
        let view = &self.stage.view;
        let area = match self.shown_mode() {
            Mode::SideBySide => view.half_rect(row, side),
            Mode::Unified => view.row_rect(row).map(|area| {
                NSRect::new(
                    area.origin,
                    NSSize::new(area.size.width.min(400.0), area.size.height),
                )
            }),
        }
        .ok_or_else(|| format!("нет прямоугольника строки {row}"))?;
        let screen = self.window.convertRectToScreen(view.to_window(area));
        Ok(events::center(screen))
    }

    /// Команда состояния вида без событий: начальное положение шага.
    pub fn command(&self, command: Command) {
        self.stage.scene.command(command);
    }
}

/// Шаги одной части контракта.
pub trait Script {
    /// Следующий стимул после успокоения вида. `Err` — ошибка исполнения стенда.
    fn next(&mut self, ctx: &Ctx<'_>) -> Result<Next, String>;

    /// Проверки после ожидания шага `name`. Расхождение оракула — проверка с `ok: false`, `Err`
    /// — стимул без эффекта или сломанный стенд.
    fn checked(&mut self, ctx: &Ctx<'_>, name: &str, checks: &mut Vec<Check>)
    -> Result<(), String>;

    /// Сырые наблюдения и находки сверх части контракта.
    fn finish(&mut self, _report: &mut Report, _checks: &[Check]) {}
}

enum Step {
    Loading {
        rx: Receiver<(Fixture, Result<Raw, String>)>,
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
    Acting,
    Waiting {
        wait: Wait,
        deadline: Instant,
    },
    Done,
}

impl Step {
    fn name(&self) -> String {
        match self {
            Step::Loading { .. } => "загрузка".to_owned(),
            Step::Ready { .. } => "готовность".to_owned(),
            Step::Opening { .. } => "открытие".to_owned(),
            Step::Acting => "шаг".to_owned(),
            Step::Waiting { wait, .. } => format!("ожидание {}", wait.name),
            Step::Done => "конец".to_owned(),
        }
    }
}

pub struct ContractRun {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    trace: SharedTrace,
    part: Part,
    variant: Variant,
    mode: Mode,
    script: RefCell<Box<dyn Script>>,
    stage: RefCell<Option<Rc<Stage>>>,
    fixture: RefCell<Option<Fixture>>,
    raw: RefCell<Option<Rc<Raw>>>,
    step: RefCell<Step>,
    checks: RefCell<Vec<Check>>,
    external: RefCell<Vec<String>>,
    report: RefCell<Option<Report>>,
    opens: Cell<u64>,
    closed: Cell<bool>,
    /// Конец срока идущего цикла `bounded`.
    cycle_end: Cell<Option<Instant>>,
}

fn now_ns() -> u64 {
    sampler::uptime_ns().unwrap_or(0)
}

/// У всех событий шага есть возврат `sendEvent`.
pub fn returned(posted: &[u64]) -> bool {
    posted
        .iter()
        .all(|number| events::record(*number).is_some_and(|record| record.send_return.is_some()))
}

fn script_of(part: Part, fixture: OpenFixture, mode: Mode) -> Result<Box<dyn Script>, String> {
    match part {
        Part::Anchor => Ok(Box::new(super::anchor_run::AnchorScript::new(fixture)?)),
        Part::Selection => Ok(Box::new(super::selection_run::SelectionScript::new(mode))),
        Part::Threads => Ok(Box::new(super::threads_run::ThreadsScript::default())),
        Part::Pairs => Ok(Box::new(super::pairs_run::PairsScript::new(fixture, mode)?)),
        Part::Bounded => Ok(Box::new(super::bounded_run::BoundedScript::new())),
    }
}

impl ContractRun {
    pub fn start(
        mtm: MainThreadMarker,
        window: &NSWindow,
        report: Report,
        trace: SharedTrace,
        view: ViewOptions,
        part: Part,
    ) -> Result<ContractRun, Failed> {
        let (variant, mode) = match (view.variant, view.mode) {
            (Some(variant), Some(mode)) => (variant, mode),
            _ => {
                return Err(Box::new((
                    format!("нет оси {}: {view:?}", part.name()),
                    report,
                )));
            }
        };
        let fixture = match part.fixture(view.fixture) {
            Ok(fixture) => fixture,
            Err(reason) => return Err(Box::new((reason, report))),
        };
        let Some(name) = fixture.synthetic() else {
            return Err(Box::new((
                format!("фикстура {} не синтетическая", fixture.name()),
                report,
            )));
        };
        let requested = match mode {
            ViewMode::Unified => Mode::Unified,
            ViewMode::Side => Mode::SideBySide,
        };
        let script = match script_of(part, fixture, requested) {
            Ok(script) => script,
            Err(reason) => return Err(Box::new((reason, report))),
        };
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name(format!("probe-diff-{}-load", part.name()))
            .spawn(move || {
                let fixture = probe_diff_view::fixtures::generate(name);
                let raw = Raw::parse(&fixture.diff);
                let _ = tx.send((fixture, raw));
            });
        if let Err(err) = spawned {
            return Err(Box::new((
                format!("поток загрузки не запущен: {err}"),
                report,
            )));
        }
        Ok(ContractRun {
            mtm,
            window: window.retain(),
            trace,
            part,
            variant,
            mode: requested,
            script: RefCell::new(script),
            stage: RefCell::new(None),
            fixture: RefCell::new(None),
            raw: RefCell::new(None),
            step: RefCell::new(Step::Loading {
                rx,
                deadline: Instant::now() + LOAD_DEADLINE,
            }),
            checks: RefCell::new(Vec::new()),
            external: RefCell::new(Vec::new()),
            report: RefCell::new(Some(report)),
            opens: Cell::new(0),
            closed: Cell::new(false),
            cycle_end: Cell::new(None),
        })
    }

    /// Срок шага, не дальше конца идущего цикла `bounded`.
    fn capped(&self, deadline: Instant) -> Instant {
        self.cycle_end
            .get()
            .map_or(deadline, |end| end.min(deadline))
    }

    /// Пометка срока цикла в причине истёкшего ожидания.
    fn cycle_note(&self) -> String {
        if self.cycle_end.get().is_some() {
            format!(" (срок цикла {} с)", CYCLE_DEADLINE.as_secs())
        } else {
            String::new()
        }
    }

    fn stage(&self) -> Result<Rc<Stage>, String> {
        self.stage
            .borrow()
            .clone()
            .ok_or_else(|| format!("нет стенда {}", self.part.name()))
    }

    pub fn poll(&self) -> Option<Result<Report, Failed>> {
        let stage = self.stage.borrow().clone();
        if let Some(stage) = stage
            && let Err(reason) = super::frames_run::stage_errors(&stage)
        {
            return Some(Err(self.fail(reason)));
        }
        drawmark::take_declared();
        drawmark::take();
        let step = std::mem::replace(&mut *self.step.borrow_mut(), Step::Done);
        let name = step.name();
        match self.advance(step) {
            Ok(Step::Done) => Some(self.finish()),
            Ok(next) => {
                *self.step.borrow_mut() = next;
                None
            }
            Err(reason) => Some(Err(self.fail(format!("{name}: {reason}")))),
        }
    }

    /// Шаг сценария с контекстом текущего стенда.
    fn with_ctx<T>(&self, run: impl FnOnce(&Ctx<'_>) -> Result<T, String>) -> Result<T, String> {
        let stage = self.stage()?;
        let raw = self
            .raw
            .borrow()
            .clone()
            .ok_or_else(|| "нет сырого разбора".to_owned())?;
        let fixture = self.fixture.borrow();
        let fixture = fixture.as_ref().ok_or_else(|| "нет фикстуры".to_owned())?;
        let ctx = Ctx {
            mtm: self.mtm,
            window: &self.window,
            stage: &stage,
            raw: &raw,
            fixture,
            mode: self.mode,
        };
        run(&ctx)
    }

    fn advance(&self, step: Step) -> Result<Step, String> {
        match step {
            Step::Loading { rx, deadline } => match rx.try_recv() {
                Ok((fixture, raw)) => {
                    let raw = raw.map_err(|reason| format!("сырой diff не разобран: {reason}"))?;
                    self.restage(fixture, Some(raw), false)
                }
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
                            deadline: self.capped(Instant::now() + OPEN_DEADLINE),
                        })
                    }
                    _ if Instant::now() < deadline => Ok(Step::Ready { deadline }),
                    state => Err(format!(
                        "до клавиши не готово за {} с{}: запрос {state:?}",
                        READY_DEADLINE.as_secs(),
                        self.cycle_note()
                    )),
                }
            }
            Step::Opening {
                posted,
                posted_ns,
                deadline,
            } => {
                if self.opened(&posted, posted_ns)? {
                    self.opens.set(self.opens.get() + 1);
                    self.cycle_end.set(None);
                    return Ok(Step::Acting);
                }
                if Instant::now() < deadline {
                    Ok(Step::Opening {
                        posted,
                        posted_ns,
                        deadline,
                    })
                } else {
                    Err(format!(
                        "открытие не закончено за {} с{}: {}",
                        OPEN_DEADLINE.as_secs(),
                        self.cycle_note(),
                        self.state(&*self.stage()?)
                    ))
                }
            }
            Step::Acting => {
                let next = self.with_ctx(|ctx| self.script.borrow_mut().next(ctx))?;
                match next {
                    Next::Wait(wait) => {
                        let deadline = Instant::now() + wait.deadline;
                        Ok(Step::Waiting { wait, deadline })
                    }
                    Next::Restage(restage) => {
                        if restage.cycle {
                            self.cycle_end.set(Some(Instant::now() + CYCLE_DEADLINE));
                        }
                        self.restage(restage.fixture, None, restage.quiet)
                    }
                    Next::Done => Ok(Step::Done),
                }
            }
            Step::Waiting { wait, deadline } => {
                let stage = self.stage()?;
                let ready = returned(&wait.posted)
                    && self.settled(&stage)
                    && match &wait.until {
                        Some(until) => self.with_ctx(|ctx| until(ctx))?,
                        None => true,
                    };
                if ready {
                    let mut checks = self.checks.borrow_mut();
                    self.with_ctx(|ctx| {
                        self.script
                            .borrow_mut()
                            .checked(ctx, &wait.name, &mut checks)
                    })?;
                    return Ok(Step::Acting);
                }
                if Instant::now() < deadline {
                    Ok(Step::Waiting { wait, deadline })
                } else {
                    Err(format!(
                        "не установилось за {:.0} с: события доставлены {}, {}",
                        wait.deadline.as_secs_f64(),
                        returned(&wait.posted),
                        self.state(&stage)
                    ))
                }
            }
            Step::Done => Ok(Step::Done),
        }
    }

    /// Новый стенд с фикстурой `fixture`: прежний снимается, сырой разбор берётся готовым, от
    /// прежней фикстуры с тем же именем или заново.
    fn restage(&self, fixture: Fixture, raw: Option<Raw>, quiet: bool) -> Result<Step, String> {
        self.drop_stage();
        let raw = match raw {
            Some(raw) => Rc::new(raw),
            None => {
                let same = self
                    .fixture
                    .borrow()
                    .as_ref()
                    .is_some_and(|current| current.name == fixture.name);
                match (same, self.raw.borrow().clone()) {
                    (true, Some(raw)) => raw,
                    _ => Rc::new(
                        Raw::parse(&fixture.diff)
                            .map_err(|reason| format!("сырой diff не разобран: {reason}"))?,
                    ),
                }
            }
        };
        *self.raw.borrow_mut() = Some(raw);
        *self.fixture.borrow_mut() = Some(fixture.clone());
        let mut report = self
            .report
            .borrow_mut()
            .take()
            .ok_or_else(|| "итог уже забран".to_owned())?;
        let mut scratch = Report::default();
        let stage = super::stage::prepare(
            self.mtm,
            &self.window,
            if quiet { &mut scratch } else { &mut report },
            self.trace.clone(),
            self.variant,
            self.mode,
            fixture,
            self.part.name(),
        );
        *self.report.borrow_mut() = Some(report);
        *self.stage.borrow_mut() = Some(Rc::new(stage?));
        drawmark::record_rows(true);
        Ok(Step::Ready {
            deadline: self.capped(Instant::now() + READY_DEADLINE),
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
        if !full || !self.settled(&stage) {
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

    /// Покрыты видимые строки, перестройки нет, запросов подсветки в работе нет, окну не нужна
    /// отрисовка.
    fn settled(&self, stage: &Stage) -> bool {
        stage.covered_now()
            && !stage.scene.relayout_pending()
            && stage.scene.highlight_settled()
            && !self.window.viewsNeedDisplay()
    }

    fn state(&self, stage: &Stage) -> String {
        let visible = stage.scene.visible_rows();
        format!(
            "видимые {}..{}, покрыты {}, непокрытые {:?}, перестройка {}, подсветка ответила {}, \
             окну нужна отрисовка {}, верх {:.1}, курсор {:?}, якорь {:?}, режим {:?}",
            visible.start,
            visible.end,
            stage.covered_now(),
            stage.census_now(),
            stage.scene.relayout_pending(),
            stage.scene.highlight_settled(),
            self.window.viewsNeedDisplay(),
            stage.view.viewport_y(),
            stage.scene.cursor(),
            stage.scene.anchor(),
            stage.scene.mode()
        )
    }

    /// Снимает сцену и вид: стенд событий, наблюдатели `drawmark`, журнал строк, содержимое
    /// окна.
    fn drop_stage(&self) {
        let Some(stage) = self.stage.borrow_mut().take() else {
            return;
        };
        events::release_open_gestures();
        if let Some(input) = events::external_input() {
            self.external.borrow_mut().push(input);
        }
        drawmark::record_rows(false);
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

    fn push_checks(&self, report: &mut Report) {
        let checks = self.checks.borrow();
        report.push(
            &format!("{}_checks", self.part.name()),
            serde_json::to_string(&*checks).unwrap_or_default(),
        );
        report.push(&format!("{}_opens", self.part.name()), self.opens.get());
    }

    fn fail(&self, reason: String) -> Failed {
        let (mut report, external) = self.close();
        self.push_checks(&mut report);
        let reason = match external {
            Some(input) => format!("{reason}; внешний ввод во время сценария: {input}"),
            None => reason,
        };
        Box::new((reason, report))
    }

    fn finish(&self) -> Result<Report, Failed> {
        let (mut report, external) = self.close();
        self.push_checks(&mut report);
        if let Some(input) = external {
            return Err(Box::new((
                format!("внешний ввод во время сценария: {input}"),
                report,
            )));
        }
        let checks = self.checks.borrow();
        let (verdict, reason) = contract::judge(self.part.name(), &checks);
        report.finding(&format!("{}_contract", self.part.name()), verdict, reason);
        self.script.borrow_mut().finish(&mut report, &checks);
        Ok(report)
    }
}

impl crate::scenario::Running for ContractRun {
    fn poll(&self) -> Option<Result<Report, Failed>> {
        ContractRun::poll(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_follow_the_process_table() {
        for scenario in Scenario::ALL {
            if let Some(part) = Part::of(scenario) {
                assert_eq!(part.name(), scenario.name());
            }
        }
        assert_eq!(Part::Selection.fixture(None).unwrap(), OpenFixture::Threads);
        assert_eq!(Part::Bounded.fixture(None).unwrap(), OpenFixture::Modified);
        assert!(Part::Bounded.fixture(Some(OpenFixture::Wrap)).is_err());
        assert!(Part::Anchor.fixture(None).is_err());
        assert_eq!(
            Part::Pairs.fixture(Some(OpenFixture::Wrap)).unwrap(),
            OpenFixture::Wrap
        );
    }
}
