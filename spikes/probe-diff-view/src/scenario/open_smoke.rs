//! Сценарий `open-smoke`: открытие фикстуры `added` (в «Рядом» — `modified`) в виде варианта и
//! режима, первый экран нарисован, затем `G`, `gg`, щелчок по строке, смена ширины окна с
//! проверкой якоря, выделение, треды и черновик. Варианты A и B идут одними шагами. Ожидания
//! и сроки записаны в отчёт до первого прогона. Бюджет открытия здесь не выносится: отметки открытия — наблюдение, вердикт по ним
//! даёт сценарий `open` задачи 11.
//!
//! Готовность экрана — барьер покрытия: вид объявляет изменения содержимого и отмечает
//! отрисованные строки, учёт [`Ledger`] считает видимую строку покрытой, когда её последняя
//! отметка не младше последнего затронувшего её изменения. Каждое покрытие видимых строк стенд
//! записывает со временем отметки, которая его закрыла.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::{NSEventModifierFlags, NSWindow};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use serde::Serialize;

use probe_diff_view::fixtures::FixtureName;
use probe_diff_view::highlight::Source;
use probe_diff_view::model::state::{Anchor, Selection, ThreadId};
use probe_diff_view::model::threads::{Comment, ThreadAnchor};
use probe_diff_view::model::{LineAddr, Mode, Side};

use super::calibrate_run::Failed;
use super::drawmark::{self, Ledger};
use super::events::{self, Key};
use super::sampler;
use super::watchdog::SharedTrace;
use crate::args::{Variant, ViewMode, ViewOptions};
use crate::results::{Report, Verdict};
use crate::scene::{Scene, Timeline};

/// Срок открытия: от `postEvent` клавиши до покрытия после снимка полного разбора.
pub const OPEN_DEADLINE: Duration = Duration::from_secs(5);
/// Срок шагов `G`, `gg` и щелчка.
pub const STEP_DEADLINE: Duration = Duration::from_secs(2);
/// Щелчок по этой видимой строке, считая с 1.
pub const CLICK_ROW: u32 = 5;
/// Допуск положения прокрутки, точки.
const SCROLL_TOLERANCE: f64 = 1.0;
/// Окно сужается на столько точек и возвращается обратно.
const RESIZE_DELTA: f64 = 120.0;
/// Шагов выделения: щелчок, `v j j`, `Shift`-щелчок, протяжка, `Escape`.
const SELECT_STEPS: usize = 5;

pub use super::stage::Coverage;
use super::stage::View;

/// Первое покрытие поколения не младше `generation`, не раньше `after_ns`.
pub fn covered_since(events: &[Coverage], generation: u64, after_ns: u64) -> Option<u64> {
    events
        .iter()
        .find(|event| event.generation >= generation)
        .map(|event| event.at_ns.max(after_ns))
}

/// Отметки открытия от `postEvent` клавиши, миллисекунды.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Milestones {
    pub model_ms: Option<f64>,
    pub first_text_ms: Option<f64>,
    pub preliminary_ms: Option<f64>,
    pub full_ms: Option<f64>,
}

impl Milestones {
    /// Отметки по времени клавиши, отметкам сцены и покрытиям.
    pub fn of(posted_ns: u64, timeline: &Timeline, coverage: &[Coverage]) -> Milestones {
        let ms = |at: u64| at.saturating_sub(posted_ns) as f64 / 1e6;
        let first_text = timeline
            .open_generation
            .and_then(|generation| covered_since(coverage, generation, 0));
        let snapshot = |source: Source| {
            timeline
                .snapshots
                .iter()
                .find(|applied| applied.source == source)
                .and_then(|applied| covered_since(coverage, applied.generation, applied.at_ns))
        };
        Milestones {
            model_ms: timeline.model_ns.map(ms),
            first_text_ms: first_text.map(ms),
            preliminary_ms: snapshot(Source::Local).map(ms),
            full_ms: snapshot(Source::Full).map(ms),
        }
    }
}

enum Step {
    Open {
        posted: Vec<u64>,
        posted_ns: u64,
        deadline: Instant,
    },
    Key {
        name: &'static str,
        posted: Vec<u64>,
        posted_ns: u64,
        deadline: Instant,
    },
    Click {
        row: u32,
        side: Option<Side>,
        posted: Vec<u64>,
        posted_ns: u64,
        deadline: Instant,
    },
    Resize {
        name: &'static str,
        before: Anchor,
        from_width: f64,
        posted_ns: u64,
        deadline: Instant,
    },
    Select {
        name: &'static str,
        expect: Expect,
        posted: Vec<u64>,
        posted_ns: u64,
        deadline: Instant,
    },
    Thread {
        name: &'static str,
        check: ThreadCheck,
        posted: Vec<u64>,
        posted_ns: u64,
        deadline: Instant,
    },
    Done,
}

/// Условие шага тредов и черновика.
#[derive(Clone, Debug)]
enum ThreadCheck {
    /// `v j c`: черновик на диапазоне стороны `from` вниз от него, поле над блоком и с фокусом.
    Opened { from: LineAddr },
    /// Клавиши в поле: текст в модели, курсор diff на месте.
    Typed {
        text: &'static str,
        cursor: Option<LineAddr>,
    },
    /// `Cmd-Enter`: раскрытый тред вместо черновика под строкой `row`, фокус у документа.
    Saved {
        anchor: ThreadAnchor,
        text: &'static str,
        row: u32,
    },
    /// Щелчок по блоку треда: тред свёрнут, якорь прокрутки прежний.
    Toggled {
        id: ThreadId,
        row: u32,
        anchor: Option<Anchor>,
    },
    /// `c` без выделения: черновик на строке курсора в том же поле. `thread` — тред и его строка
    /// для смены ширины.
    Reopened {
        at: LineAddr,
        thread: (ThreadId, u32),
    },
    /// Смена ширины при свёрнутом треде и открытом черновике: установлен индекс новой ширины,
    /// блок треда в нём той же высоты, поле над блоком черновика, якорь прежний.
    Resized {
        from_width: f64,
        thread: (ThreadId, u32),
        anchor: Option<Anchor>,
    },
    /// `Escape` в поле: черновика нет, тредов прежнее число, фокус у документа.
    Cancelled { threads: usize },
}

/// Текст черновика, набранный клавишами стенда.
const DRAFT_TEXT: &str = "jk";
/// Шагов тредов и черновика.
const THREAD_STEPS: usize = 8;

/// Условие шага выделения.
#[derive(Clone, Copy, Debug)]
enum Expect {
    /// Курсор на адресе, выделения нет.
    Cursor(LineAddr),
    /// Выделение стороны `side` от `tail`. `head` — точный конец, `None` — конец ниже `tail`.
    /// `cursor` — где обязан стоять курсор.
    Selection {
        side: Side,
        tail: u32,
        head: Option<u32>,
        cursor: Option<LineAddr>,
    },
    NoSelection,
}

/// Выполнено ли условие шага выделения при курсоре `cursor` и выделении `selection`.
fn expect_met(expect: Expect, cursor: Option<LineAddr>, selection: Option<Selection>) -> bool {
    match expect {
        Expect::Cursor(addr) => cursor == Some(addr) && selection.is_none(),
        Expect::Selection {
            side,
            tail,
            head,
            cursor: at,
        } => selection.is_some_and(|selection| {
            selection.side == side
                && selection.tail == tail
                && head.map_or(selection.head > tail, |head| selection.head == head)
                && at.is_none_or(|at| cursor == Some(at))
        }),
        Expect::NoSelection => selection.is_none(),
    }
}

/// Итог смены ширины: якорь до и после, верхний край против выведенного из якоря.
#[derive(Clone, Debug, Serialize)]
struct ResizeResult {
    name: &'static str,
    latency_ms: f64,
    from_width: f64,
    to_width: f64,
    mode: String,
    document_height: Option<u64>,
    before: String,
    after: String,
    scroll_y: f64,
    anchor_y: Option<u64>,
    held: bool,
}

/// Итог шага клавиши или щелчка.
#[derive(Clone, Debug, Serialize)]
struct StepResult {
    name: &'static str,
    latency_ms: f64,
    scroll_y: f64,
    visible: Range<u32>,
    detail: String,
}

/// Наибольшее число видов строк B за сценарий и предел `2 × видимых + 10` в тот момент.
#[derive(Clone, Copy, Debug, Default, Serialize)]
struct RowViews {
    views: usize,
    limit: usize,
    visible: usize,
    /// Наблюдений, где видов было больше предела.
    over: u64,
}

pub struct OpenSmoke {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    view: View,
    scene: Rc<Scene>,
    coverage: Rc<RefCell<Vec<Coverage>>>,
    ledger: Rc<RefCell<Ledger>>,
    step: RefCell<Step>,
    milestones: RefCell<Option<Milestones>>,
    steps: RefCell<Vec<StepResult>>,
    resizes: RefCell<Vec<ResizeResult>>,
    selections: RefCell<Vec<StepResult>>,
    thread_steps: RefCell<Vec<StepResult>>,
    max_entries_seen: RefCell<(usize, usize)>,
    row_views: RefCell<RowViews>,
    /// Расхождения строк таблицы B с индексом высот: где проверено, строк и расхождения.
    geometry: RefCell<Vec<(String, usize, Vec<String>)>>,
    report: RefCell<Option<Report>>,
    mode: Mode,
}

fn now_ns() -> u64 {
    sampler::uptime_ns().unwrap_or(0)
}

fn returned(posted: &[u64]) -> bool {
    posted
        .iter()
        .all(|number| events::record(*number).is_some_and(|record| record.send_return.is_some()))
}

impl OpenSmoke {
    pub fn start(
        mtm: MainThreadMarker,
        window: &NSWindow,
        mut report: Report,
        trace: SharedTrace,
        view: ViewOptions,
    ) -> Result<OpenSmoke, Failed> {
        let (variant, mode, name) = match (view.variant, view.mode) {
            (Some(variant), Some(ViewMode::Unified)) => {
                (variant, Mode::Unified, FixtureName::Added)
            }
            // У `added` старая половина пустая: «Рядом» открывает `modified`, где пары полные.
            (Some(variant), Some(ViewMode::Side)) => {
                (variant, Mode::SideBySide, FixtureName::Modified)
            }
            (variant, mode) => {
                return Err(Box::new((
                    format!(
                        "нет оси вида: вариант {:?}, режим {:?}",
                        variant.map(Variant::name),
                        mode.map(ViewMode::name)
                    ),
                    report,
                )));
            }
        };
        let fixture = super::stage::generate(&mut report, name);
        let stage = match super::stage::prepare(
            mtm,
            window,
            &mut report,
            trace,
            variant,
            mode,
            fixture,
            "open-smoke",
        ) {
            Ok(stage) => stage,
            Err(reason) => return Err(Box::new((reason, report))),
        };
        let super::stage::Stage {
            view: surface,
            scene,
            ledger,
            coverage,
        } = stage;
        let run = OpenSmoke {
            mtm,
            window: window.retain(),
            view: surface,
            scene,
            coverage,
            ledger,
            step: RefCell::new(Step::Done),
            milestones: RefCell::new(None),
            steps: RefCell::new(Vec::new()),
            resizes: RefCell::new(Vec::new()),
            selections: RefCell::new(Vec::new()),
            thread_steps: RefCell::new(Vec::new()),
            max_entries_seen: RefCell::new((0, 0)),
            row_views: RefCell::new(RowViews::default()),
            geometry: RefCell::new(Vec::new()),
            report: RefCell::new(Some(report)),
            mode,
        };
        if let Some(report) = run.report.borrow_mut().as_mut() {
            match sampler::sample() {
                Ok(sample) => report.metric(
                    "footprint_before_open_bytes",
                    "bytes",
                    vec![sample.footprint_bytes as f64],
                ),
                Err(reason) => report.push("sample_error", reason),
            }
        }
        let posted_ns = now_ns();
        match events::press(Key::OPEN) {
            Ok(posted) => {
                *run.step.borrow_mut() = Step::Open {
                    posted,
                    posted_ns,
                    deadline: Instant::now() + OPEN_DEADLINE,
                }
            }
            Err(reason) => return Err(run.fail(reason)),
        }
        Ok(run)
    }

    /// Покрыты ли видимые строки сейчас.
    fn covered_now(&self) -> bool {
        let visible = self.scene.visible_rows();
        !visible.is_empty()
            && self
                .ledger
                .borrow()
                .uncovered(visible.start as usize..visible.end as usize)
                .is_none()
    }

    fn observe_cache(&self) {
        match &self.view {
            View::A(view) => {
                let (len, capacity) = view.cache_len();
                let mut seen = self.max_entries_seen.borrow_mut();
                if len > seen.0 {
                    *seen = (len, capacity);
                }
            }
            View::B(view) => {
                let views = view.available_row_views();
                let visible = view.visible_table_rows().len();
                let limit = 2 * visible + 10;
                let mut seen = self.row_views.borrow_mut();
                seen.over += u64::from(views > limit);
                if views > seen.views {
                    *seen = RowViews {
                        views,
                        limit,
                        visible,
                        over: seen.over,
                    };
                }
            }
        }
    }

    /// Строки таблицы B против индекса высот у видимых строк, `when` — после какого шага.
    fn check_geometry(&self, when: &str) {
        if let View::B(view) = &self.view {
            let (checked, errors) = view.geometry();
            self.geometry
                .borrow_mut()
                .push((when.to_owned(), checked, errors));
        }
    }

    pub fn poll(&self) -> Option<Result<Report, Failed>> {
        self.observe_cache();
        let errors = self.scene.errors();
        if !errors.is_empty() {
            return Some(Err(self.fail(format!("сцена: {}", errors.join("; ")))));
        }
        // Таблица запоминает высоту строки: ответ при занятой модели ломает вид.
        if let View::B(view) = &self.view {
            let failures = view.stats().height_failures;
            if failures > 0 {
                return Some(Err(self.fail(format!(
                    "таблица B получила {failures} высот строк при занятой модели"
                ))));
            }
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
            Step::Open {
                posted,
                posted_ns,
                deadline,
            } => {
                let timeline = self.scene.timeline();
                let milestones = Milestones::of(posted_ns, &timeline, &self.coverage.borrow());
                if returned(&posted)
                    && milestones.first_text_ms.is_some()
                    && milestones.full_ms.is_some()
                    && self.covered_now()
                {
                    *self.milestones.borrow_mut() = Some(milestones);
                    self.check_geometry("open");
                    let shown = self.scene.mode();
                    if shown != self.mode {
                        return Err(format!(
                            "запрошен режим {:?}, показан {shown:?}: ширины окна не хватило",
                            self.mode
                        ));
                    }
                    return self.press("G", Key::SHIFT_G);
                }
                if Instant::now() < deadline {
                    return Ok(Step::Open {
                        posted,
                        posted_ns,
                        deadline,
                    });
                }
                *self.milestones.borrow_mut() = Some(milestones.clone());
                Err(format!(
                    "открытие не дошло до покрытия после полного разбора за {} с: {milestones:?}, снимков {}, видимые строки {:?}, первая непокрытая {:?}",
                    OPEN_DEADLINE.as_secs(),
                    timeline.snapshots.len(),
                    self.scene.visible_rows(),
                    {
                        let visible = self.scene.visible_rows();
                        self.ledger
                            .borrow()
                            .uncovered(visible.start as usize..visible.end as usize)
                    }
                ))
            }
            Step::Key {
                name,
                posted,
                posted_ns,
                deadline,
            } => {
                let visible = self.scene.visible_rows();
                let y = self.view.viewport_y();
                let limit = self.scene.scroll_limit();
                let rows = self.scene.timeline().rows.unwrap_or(0);
                let (placed, detail) = match (name, limit) {
                    ("G", Some((_, max_y))) => (
                        (y - max_y as f64).abs() <= SCROLL_TOLERANCE && visible.end == rows,
                        format!("предел прокрутки {max_y}, верхний край {y}, строк {rows}"),
                    ),
                    ("gg", Some(_)) => (
                        y.abs() <= SCROLL_TOLERANCE && visible.start == 0,
                        format!("верхний край {y}"),
                    ),
                    _ => (false, "нет индекса высот".to_owned()),
                };
                if returned(&posted) && placed && self.covered_now() {
                    let at = self
                        .coverage
                        .borrow()
                        .iter()
                        .rev()
                        .find(|event| event.rows == (visible.start, visible.end))
                        .map_or(now_ns(), |event| event.at_ns);
                    self.steps.borrow_mut().push(StepResult {
                        name,
                        latency_ms: at.saturating_sub(posted_ns) as f64 / 1e6,
                        scroll_y: y,
                        visible,
                        detail,
                    });
                    return match name {
                        "G" => self.press_sequence("gg"),
                        _ => self.click(match self.mode {
                            Mode::Unified => None,
                            Mode::SideBySide => Some(Side::Old),
                        }),
                    };
                }
                if Instant::now() < deadline {
                    return Ok(Step::Key {
                        name,
                        posted,
                        posted_ns,
                        deadline,
                    });
                }
                Err(format!(
                    "шаг {name} не завершён за {} мс: {detail}, видимые строки {visible:?}, покрыты {}, sendEvent вернулся {}",
                    STEP_DEADLINE.as_millis(),
                    self.covered_now(),
                    returned(&posted)
                ))
            }
            Step::Click {
                row,
                side,
                posted,
                posted_ns,
                deadline,
            } => {
                let expected = match side {
                    Some(side) => self.scene.row_addr_at(row, side),
                    None => self.scene.row_addr(row),
                };
                let cursor = self.scene.cursor();
                if returned(&posted)
                    && expected.is_some()
                    && cursor == expected
                    && self.covered_now()
                {
                    let visible = self.scene.visible_rows();
                    let at = self
                        .coverage
                        .borrow()
                        .last()
                        .map_or(now_ns(), |event| event.at_ns);
                    self.steps.borrow_mut().push(StepResult {
                        name: "click",
                        latency_ms: at.saturating_sub(posted_ns) as f64 / 1e6,
                        scroll_y: self.view.viewport_y(),
                        visible,
                        detail: format!("строка {row}, половина {side:?}, курсор {cursor:?}"),
                    });
                    return match side {
                        Some(Side::Old) => self.click(Some(Side::New)),
                        Some(Side::New) | None => self.resize("narrow"),
                    };
                }
                if Instant::now() < deadline {
                    return Ok(Step::Click {
                        row,
                        side,
                        posted,
                        posted_ns,
                        deadline,
                    });
                }
                Err(format!(
                    "щелчок по строке {row}, половина {side:?}, не поставил курсор за {} мс: ожидался {expected:?}, курсор {cursor:?}, покрыты {}",
                    STEP_DEADLINE.as_millis(),
                    self.covered_now()
                ))
            }
            Step::Resize {
                name,
                before,
                from_width,
                posted_ns,
                deadline,
            } => {
                let geometry = self.scene.geometry();
                let installed = geometry
                    .is_some_and(|geometry| (geometry.width() - from_width).abs() >= 0.5)
                    && !self.scene.relayout_pending();
                if installed && self.covered_now() {
                    let after = self.scene.anchor();
                    let anchor_y = self.scene.anchor_y();
                    let scroll_y = self.view.viewport_y();
                    let held = after == Some(before)
                        && anchor_y
                            .is_some_and(|y| (y as f64 - scroll_y).abs() <= SCROLL_TOLERANCE);
                    let at = self
                        .coverage
                        .borrow()
                        .last()
                        .map_or(now_ns(), |event| event.at_ns);
                    self.resizes.borrow_mut().push(ResizeResult {
                        name,
                        latency_ms: at.saturating_sub(posted_ns) as f64 / 1e6,
                        from_width,
                        to_width: geometry.map_or(0.0, |geometry| geometry.width()),
                        mode: format!("{:?}", self.scene.mode()),
                        document_height: self.scene.scroll_limit().map(|(total, _)| total),
                        before: format!("{before:?}"),
                        after: format!("{after:?}"),
                        scroll_y,
                        anchor_y,
                        held,
                    });
                    self.check_geometry(name);
                    return match name {
                        "narrow" => self.resize("restore"),
                        _ => self.select_click(),
                    };
                }
                if Instant::now() < deadline {
                    return Ok(Step::Resize {
                        name,
                        before,
                        from_width,
                        posted_ns,
                        deadline,
                    });
                }
                Err(format!(
                    "смена ширины {name} не установлена за {} с: ширина {from_width} → {:?}, перестройка ждёт {}, покрыты {}, перестройки {:?}",
                    OPEN_DEADLINE.as_secs(),
                    geometry.map(|geometry| geometry.width()),
                    self.scene.relayout_pending(),
                    self.covered_now(),
                    self.scene.relayout()
                ))
            }
            Step::Select {
                name,
                expect,
                posted,
                posted_ns,
                deadline,
            } => {
                let cursor = self.scene.cursor();
                let selection = self.scene.selection();
                if returned(&posted) && expect_met(expect, cursor, selection) && self.covered_now()
                {
                    let at = self
                        .coverage
                        .borrow()
                        .last()
                        .map_or(now_ns(), |event| event.at_ns.max(posted_ns));
                    self.selections.borrow_mut().push(StepResult {
                        name,
                        latency_ms: at.saturating_sub(posted_ns) as f64 / 1e6,
                        scroll_y: self.view.viewport_y(),
                        visible: self.scene.visible_rows(),
                        detail: format!("курсор {cursor:?}, выделение {selection:?}"),
                    });
                    return self.after_select(name, selection);
                }
                if Instant::now() < deadline {
                    return Ok(Step::Select {
                        name,
                        expect,
                        posted,
                        posted_ns,
                        deadline,
                    });
                }
                Err(format!(
                    "шаг выделения {name} не выполнен за {} мс: ожидалось {expect:?}, курсор {cursor:?}, выделение {selection:?}, покрыты {}, sendEvent вернулся {}",
                    STEP_DEADLINE.as_millis(),
                    self.covered_now(),
                    returned(&posted)
                ))
            }
            Step::Thread {
                name,
                check,
                posted,
                posted_ns,
                deadline,
            } => {
                let (met, detail) = self.thread_met(&check);
                if returned(&posted) && met && self.covered_now() {
                    let at = self
                        .coverage
                        .borrow()
                        .last()
                        .map_or(now_ns(), |event| event.at_ns.max(posted_ns));
                    self.thread_steps.borrow_mut().push(StepResult {
                        name,
                        latency_ms: at.saturating_sub(posted_ns) as f64 / 1e6,
                        scroll_y: self.view.viewport_y(),
                        visible: self.scene.visible_rows(),
                        detail,
                    });
                    self.check_geometry(name);
                    return self.after_thread(name, &check);
                }
                if Instant::now() < deadline {
                    return Ok(Step::Thread {
                        name,
                        check,
                        posted,
                        posted_ns,
                        deadline,
                    });
                }
                Err(format!(
                    "шаг черновика {name} не выполнен в срок: ожидалось {check:?}; {detail}; покрыты {}, sendEvent вернулся {}",
                    self.covered_now(),
                    returned(&posted)
                ))
            }
            Step::Done => Ok(Step::Done),
        }
    }

    /// Поле черновика стоит ровно над блоком черновика.
    fn field_over_draft(&self) -> bool {
        match (self.view.draft_shown(), self.scene.draft_frame()) {
            (Some(field), Some(frame)) => {
                (field.origin.x - frame.x).abs() <= 0.5
                    && (field.origin.y - frame.y).abs() <= 0.5
                    && (field.size.width - frame.width).abs() <= 0.5
                    && (field.size.height - frame.height).abs() <= 0.5
            }
            _ => false,
        }
    }

    /// Блоки строки `row` в установленном индексе равны блокам раскладки, свёрнутый тред `id`
    /// среди них своей высоты. Черновик на последней строке якоря треда стоит в той же строке.
    fn blocks_installed(&self, id: ThreadId, row: u32) -> bool {
        let frames = self.scene.block_frames(row);
        let sum: f64 = frames.iter().map(|frame| frame.height).sum();
        let thread = frames.iter().find(|frame| {
            frame.entry.item == probe_diff_view::model::threads::BlockItem::Thread(id)
        });
        self.scene
            .block_height(row)
            .is_some_and(|height| f64::from(height) == sum)
            && thread.is_some_and(|frame| {
                self.scene.thread_height(id, false) == Some(frame.height as u32)
            })
    }

    /// Выполнено ли условие шага тредов и черновика, и что видно сейчас.
    fn thread_met(&self, check: &ThreadCheck) -> (bool, String) {
        let draft = self.scene.draft();
        let threads = self.scene.threads();
        let fields = crate::draft_field::count();
        let detail = format!(
            "черновик {:?}, тредов {}, поле {:?}, фокус у поля {}, у документа {}, полей создано {}",
            draft.as_ref().map(|draft| (&draft.anchor, draft.text())),
            threads.len(),
            self.view.draft_shown().map(crate::describe_rect),
            self.view.draft_focused(),
            self.view.document_focused(),
            fields.created
        );
        let met = match check {
            ThreadCheck::Opened { from } => draft.as_ref().is_some_and(|draft| {
                let anchor = &draft.anchor;
                let row_height = self
                    .scene
                    .draft_frame()
                    .and_then(|frame| Some((self.scene.block_height(frame.row)?, frame.height)));
                anchor.side == from.side
                    && anchor.start_line == from.line
                    && anchor.line > from.line
                    && self.scene.selection().is_none()
                    && self.field_over_draft()
                    && self.view.draft_focused()
                    && row_height.is_some_and(|(block, draft)| f64::from(block) == draft)
            }),
            ThreadCheck::Typed { text, cursor } => {
                draft.as_ref().is_some_and(|draft| draft.text() == *text)
                    && self.scene.cursor() == *cursor
                    && self.view.draft_focused()
                    && self.field_over_draft()
            }
            ThreadCheck::Saved { anchor, text, row } => {
                draft.is_none()
                    && threads.len() == 1
                    && threads.first().is_some_and(|thread| {
                        thread.anchor == *anchor
                            && thread.comments
                                == [Comment {
                                    author: crate::blocks::AUTHOR.to_owned(),
                                    body: (*text).to_owned(),
                                }]
                            && self.scene.is_expanded(thread.id)
                            && self.scene.block_height(*row)
                                == self.scene.thread_height(thread.id, true)
                    })
                    && self.view.draft_shown().is_none()
                    && self.view.document_focused()
            }
            ThreadCheck::Toggled { id, row, anchor } => {
                !self.scene.is_expanded(*id)
                    && self.scene.block_height(*row) == self.scene.thread_height(*id, false)
                    && self.scene.anchor() == *anchor
            }
            ThreadCheck::Resized {
                from_width,
                thread: (id, row),
                anchor,
            } => {
                self.scene
                    .geometry()
                    .is_some_and(|geometry| (geometry.width() - from_width).abs() >= 0.5)
                    && !self.scene.relayout_pending()
                    && self.blocks_installed(*id, *row)
                    && draft.is_some()
                    && self.field_over_draft()
                    && self.scene.anchor() == *anchor
            }
            ThreadCheck::Reopened { at, .. } => draft.as_ref().is_some_and(|draft| {
                draft.anchor.side == at.side
                    && draft.anchor.start_line == at.line
                    && draft.anchor.line == at.line
                    && draft.text().is_empty()
                    && self.field_over_draft()
                    && self.view.draft_focused()
                    && fields.created == 1
            }),
            ThreadCheck::Cancelled { threads: count } => {
                draft.is_none()
                    && threads.len() == *count
                    && self.view.draft_shown().is_none()
                    && self.view.document_focused()
            }
        };
        (met, detail)
    }

    fn thread_step(
        &self,
        name: &'static str,
        check: ThreadCheck,
        posted: Vec<u64>,
        posted_ns: u64,
    ) -> Step {
        let deadline = match check {
            ThreadCheck::Resized { .. } => OPEN_DEADLINE,
            _ => STEP_DEADLINE,
        };
        Step::Thread {
            name,
            check,
            posted,
            posted_ns,
            deadline: Instant::now() + deadline,
        }
    }

    /// Смена ширины окна на [`RESIZE_DELTA`] при открытом черновике и свёрнутом треде.
    fn resize_with_blocks(
        &self,
        name: &'static str,
        narrow: bool,
        thread: (ThreadId, u32),
    ) -> Result<Step, String> {
        let from = self
            .scene
            .geometry()
            .ok_or_else(|| format!("нет геометрии до {name}"))?;
        let anchor = self.scene.anchor();
        let frame = self.window.frame();
        let width = if narrow {
            frame.size.width - RESIZE_DELTA
        } else {
            frame.size.width + RESIZE_DELTA
        };
        let posted_ns = now_ns();
        self.window.setFrame_display(
            NSRect::new(frame.origin, NSSize::new(width, frame.size.height)),
            true,
        );
        Ok(self.thread_step(
            name,
            ThreadCheck::Resized {
                from_width: from.width(),
                thread,
                anchor,
            },
            Vec::new(),
            posted_ns,
        ))
    }

    /// `v j c` от курсора: черновик на диапазоне из двух строк.
    fn draft_open(&self) -> Result<Step, String> {
        let from = self
            .scene
            .cursor()
            .ok_or_else(|| "нет курсора перед черновиком".to_owned())?;
        let posted_ns = now_ns();
        let mut posted = events::press(Key::V)?;
        posted.extend(events::press(Key::J)?);
        posted.extend(events::press(Key::C)?);
        Ok(self.thread_step(
            "draft_open",
            ThreadCheck::Opened { from },
            posted,
            posted_ns,
        ))
    }

    /// Следующий шаг тредов и черновика после `name`.
    fn after_thread(&self, name: &'static str, check: &ThreadCheck) -> Result<Step, String> {
        match (name, check) {
            ("draft_open", ThreadCheck::Opened { .. }) => {
                let posted_ns = now_ns();
                let mut posted = events::press(Key::J)?;
                posted.extend(events::press(Key::K)?);
                Ok(self.thread_step(
                    "draft_type",
                    ThreadCheck::Typed {
                        text: DRAFT_TEXT,
                        cursor: self.scene.cursor(),
                    },
                    posted,
                    posted_ns,
                ))
            }
            ("draft_type", ThreadCheck::Typed { text, .. }) => {
                let anchor = self
                    .scene
                    .draft()
                    .map(|draft| draft.anchor)
                    .ok_or_else(|| "нет черновика перед сохранением".to_owned())?;
                let row = self
                    .scene
                    .draft_frame()
                    .map(|frame| frame.row)
                    .ok_or_else(|| "нет блока черновика перед сохранением".to_owned())?;
                let posted_ns = now_ns();
                let posted = events::press(Key::CMD_RETURN)?;
                Ok(self.thread_step(
                    "draft_save",
                    ThreadCheck::Saved { anchor, text, row },
                    posted,
                    posted_ns,
                ))
            }
            ("draft_save", ThreadCheck::Saved { row, .. }) => {
                let frame = self
                    .scene
                    .block_frames(*row)
                    .into_iter()
                    .find(|frame| {
                        matches!(
                            frame.entry.item,
                            probe_diff_view::model::threads::BlockItem::Thread(_)
                        )
                    })
                    .ok_or_else(|| format!("нет блока треда под строкой {row}"))?;
                let probe_diff_view::model::threads::BlockItem::Thread(id) = frame.entry.item
                else {
                    return Err("блок не тред".to_owned());
                };
                let line = f64::from(self.view.font().line_height);
                let area = NSRect::new(
                    NSPoint::new(frame.x, frame.y),
                    NSSize::new(frame.width.min(200.0), line),
                );
                let screen = self.window.convertRectToScreen(self.view.to_window(area));
                let anchor = self.scene.anchor();
                let posted_ns = now_ns();
                let gesture = events::mouse_down(
                    self.mtm,
                    events::center(screen),
                    &self.window,
                    NSEventModifierFlags::empty(),
                    1,
                    "thread_toggle",
                )?;
                let posted = vec![gesture.down, gesture.up("thread_toggle")?];
                Ok(self.thread_step(
                    "thread_toggle",
                    ThreadCheck::Toggled {
                        id,
                        row: *row,
                        anchor,
                    },
                    posted,
                    posted_ns,
                ))
            }
            ("thread_toggle", ThreadCheck::Toggled { id, row, .. }) => {
                let at = self
                    .scene
                    .cursor()
                    .ok_or_else(|| "нет курсора перед повторным черновиком".to_owned())?;
                let posted_ns = now_ns();
                let posted = events::press(Key::C)?;
                Ok(self.thread_step(
                    "draft_reopen",
                    ThreadCheck::Reopened {
                        at,
                        thread: (*id, *row),
                    },
                    posted,
                    posted_ns,
                ))
            }
            ("draft_reopen", ThreadCheck::Reopened { thread, .. }) => {
                self.resize_with_blocks("blocks_narrow", true, *thread)
            }
            ("blocks_narrow", ThreadCheck::Resized { thread, .. }) => {
                self.resize_with_blocks("blocks_restore", false, *thread)
            }
            ("blocks_restore", _) => {
                let posted_ns = now_ns();
                let posted = events::press(Key::ESCAPE)?;
                Ok(self.thread_step(
                    "draft_cancel",
                    ThreadCheck::Cancelled {
                        threads: self.scene.threads().len(),
                    },
                    posted,
                    posted_ns,
                ))
            }
            _ => Ok(Step::Done),
        }
    }

    /// Первая видимая строка с адресом новой стороны, начиная с `from`-й видимой (с 1) и ниже
    /// строки `after`.
    fn new_side_row(&self, from: u32, after: Option<u32>) -> Result<(u32, LineAddr), String> {
        let visible = self.scene.visible_rows();
        let start = (visible.start + from - 1).max(after.map_or(0, |row| row + 1));
        (start..visible.end)
            .find_map(|row| Some((row, self.scene.row_addr_at(row, Side::New)?)))
            .ok_or_else(|| format!("нет видимой строки с новой стороной от {start} в {visible:?}"))
    }

    /// Точка экрана для щелчка по строке `row`: в «Рядом» центр новой половины, в unified центр
    /// левых 400 точек строки.
    fn new_side_point(&self, row: u32) -> Result<NSPoint, String> {
        let area = match self.mode {
            Mode::SideBySide => self.view.half_rect(row, Side::New),
            Mode::Unified => self.view.row_rect(row).map(|area| {
                NSRect::new(
                    area.origin,
                    NSSize::new(area.size.width.min(400.0), area.size.height),
                )
            }),
        }
        .ok_or_else(|| format!("нет прямоугольника строки {row}"))?;
        let screen = self.window.convertRectToScreen(self.view.to_window(area));
        Ok(events::center(screen))
    }

    fn select_step(
        &self,
        name: &'static str,
        expect: Expect,
        posted: Vec<u64>,
        posted_ns: u64,
    ) -> Step {
        Step::Select {
            name,
            expect,
            posted,
            posted_ns,
            deadline: Instant::now() + STEP_DEADLINE,
        }
    }

    /// Щелчок без модификаторов по первой строке с новой стороной от 5-й видимой.
    fn select_click(&self) -> Result<Step, String> {
        let (row, addr) = self.new_side_row(CLICK_ROW, None)?;
        let point = self.new_side_point(row)?;
        let posted_ns = now_ns();
        let gesture = events::mouse_down(
            self.mtm,
            point,
            &self.window,
            NSEventModifierFlags::empty(),
            1,
            "sel_click",
        )?;
        let posted = vec![gesture.down, gesture.up("sel_click")?];
        Ok(self.select_step("sel_click", Expect::Cursor(addr), posted, posted_ns))
    }

    /// Следующий шаг выделения после `name`.
    fn after_select(
        &self,
        name: &'static str,
        selection: Option<Selection>,
    ) -> Result<Step, String> {
        match name {
            "sel_click" => {
                let cursor = self
                    .scene
                    .cursor()
                    .ok_or_else(|| "нет курсора после щелчка".to_owned())?;
                let posted_ns = now_ns();
                let mut posted = events::press(Key::V)?;
                posted.extend(events::press(Key::J)?);
                posted.extend(events::press(Key::J)?);
                Ok(self.select_step(
                    "sel_visual",
                    Expect::Selection {
                        side: cursor.side,
                        tail: cursor.line,
                        head: None,
                        cursor: None,
                    },
                    posted,
                    posted_ns,
                ))
            }
            "sel_visual" => {
                let selection = selection.ok_or_else(|| "нет выделения после v j j".to_owned())?;
                let (row, addr) = self.new_side_row(CLICK_ROW + 6, None)?;
                let point = self.new_side_point(row)?;
                let posted_ns = now_ns();
                let gesture = events::mouse_down(
                    self.mtm,
                    point,
                    &self.window,
                    NSEventModifierFlags::Shift,
                    1,
                    "sel_shift_click",
                )?;
                let posted = vec![gesture.down, gesture.up("sel_shift_click")?];
                Ok(self.select_step(
                    "sel_shift_click",
                    Expect::Selection {
                        side: selection.side,
                        tail: selection.tail,
                        head: Some(addr.line),
                        cursor: Some(addr),
                    },
                    posted,
                    posted_ns,
                ))
            }
            "sel_shift_click" => {
                let (first, from) = self.new_side_row(CLICK_ROW + 2, None)?;
                let (second, to) = self.new_side_row(1, Some(first + 3))?;
                let (start, end) = (self.new_side_point(first)?, self.new_side_point(second)?);
                let posted_ns = now_ns();
                let gesture = events::mouse_down(
                    self.mtm,
                    start,
                    &self.window,
                    NSEventModifierFlags::empty(),
                    1,
                    "sel_drag",
                )?;
                let posted = vec![
                    gesture.down,
                    gesture.drag_to(end, "sel_drag")?,
                    gesture.up("sel_drag")?,
                ];
                Ok(self.select_step(
                    "sel_drag",
                    Expect::Selection {
                        side: Side::New,
                        tail: from.line,
                        head: Some(to.line),
                        cursor: Some(to),
                    },
                    posted,
                    posted_ns,
                ))
            }
            "sel_drag" => {
                let posted_ns = now_ns();
                let posted = events::press(Key::ESCAPE)?;
                Ok(self.select_step("sel_escape", Expect::NoSelection, posted, posted_ns))
            }
            "sel_escape" => self.draft_open(),
            _ => Ok(Step::Done),
        }
    }

    /// Смена ширины окна на [`RESIZE_DELTA`]: `narrow` сужает, `restore` возвращает. Перед
    /// сужением верхний край встаёт на середину предела прокрутки со смещением внутри строки.
    fn resize(&self, name: &'static str) -> Result<Step, String> {
        let from = self
            .scene
            .geometry()
            .ok_or_else(|| "нет геометрии до смены ширины".to_owned())?;
        if name == "narrow" {
            let (_, max_y) = self
                .scene
                .scroll_limit()
                .ok_or_else(|| "нет индекса высот до смены ширины".to_owned())?;
            let line = u64::from(self.view.font().line_height);
            self.view.scroll_to((max_y / 2 + line / 2) as f64);
        }
        let before = self
            .scene
            .anchor()
            .ok_or_else(|| "нет якоря до смены ширины".to_owned())?;
        let frame = self.window.frame();
        let width = match name {
            "narrow" => frame.size.width - RESIZE_DELTA,
            _ => frame.size.width + RESIZE_DELTA,
        };
        let posted_ns = now_ns();
        self.window.setFrame_display(
            NSRect::new(frame.origin, NSSize::new(width, frame.size.height)),
            true,
        );
        Ok(Step::Resize {
            name,
            before,
            from_width: from.width(),
            posted_ns,
            deadline: Instant::now() + OPEN_DEADLINE,
        })
    }

    fn press(&self, name: &'static str, key: Key) -> Result<Step, String> {
        let posted_ns = now_ns();
        let posted = events::press(key)?;
        Ok(Step::Key {
            name,
            posted,
            posted_ns,
            deadline: Instant::now() + STEP_DEADLINE,
        })
    }

    fn press_sequence(&self, name: &'static str) -> Result<Step, String> {
        let posted_ns = now_ns();
        let posted = events::press_sequence(name)?;
        Ok(Step::Key {
            name,
            posted,
            posted_ns,
            deadline: Instant::now() + STEP_DEADLINE,
        })
    }

    /// Щелчок по центру видимой строки [`CLICK_ROW`] без модификаторов: в unified по левым
    /// 400 точкам строки, в «Рядом» по центру половины `side`.
    fn click(&self, side: Option<Side>) -> Result<Step, String> {
        let visible = self.scene.visible_rows();
        let row = visible.start + CLICK_ROW - 1;
        if row >= visible.end {
            return Err(format!("видимых строк меньше {CLICK_ROW}: {visible:?}"));
        }
        let area = match side {
            Some(side) => self.view.half_rect(row, side),
            None => self.view.row_rect(row).map(|area| {
                NSRect::new(
                    area.origin,
                    NSSize::new(area.size.width.min(400.0), area.size.height),
                )
            }),
        }
        .ok_or_else(|| format!("нет прямоугольника строки {row}"))?;
        let screen = self.window.convertRectToScreen(self.view.to_window(area));
        let point = events::center(screen);
        let posted_ns = now_ns();
        let gesture = events::mouse_down(
            self.mtm,
            NSPoint::new(point.x, point.y),
            &self.window,
            NSEventModifierFlags::empty(),
            1,
            "open-smoke",
        )?;
        let posted = vec![gesture.down, gesture.up("open-smoke")?];
        Ok(Step::Click {
            row,
            side,
            posted,
            posted_ns,
            deadline: Instant::now() + STEP_DEADLINE,
        })
    }

    /// Журналы в итог; стенд и сцена снимаются.
    fn close(&self) -> (Report, Option<String>) {
        let mut report = self.report.borrow_mut().take().unwrap_or_default();
        let released = events::release_open_gestures();
        let external = events::external_input();
        report.push(
            "events",
            serde_json::to_string(&events::export()).unwrap_or_default(),
        );
        report.push("released_gestures", released);
        report.push(
            "timeline",
            serde_json::to_string(&self.scene.timeline()).unwrap_or_default(),
        );
        report.push(
            "coverage",
            serde_json::to_string(&*self.coverage.borrow()).unwrap_or_default(),
        );
        report.push(
            "declared",
            serde_json::to_string(&drawmark::take_declared()).unwrap_or_default(),
        );
        report.push("marks", drawmark::take().len());
        report.push("ledger_size", format!("{:?}", self.ledger.borrow().size()));
        match &self.view {
            View::A(view) => {
                report.push(
                    "layout_cache",
                    serde_json::to_string(&view.cache_counters()).unwrap_or_default(),
                );
                report.push(
                    "draw_stats",
                    serde_json::to_string(&view.draw_stats()).unwrap_or_default(),
                );
                report.push(
                    "dirty_rects",
                    serde_json::to_string(&view.dirty_log()).unwrap_or_default(),
                );
                report.push(
                    "block_cache",
                    serde_json::to_string(&view.block_cache_counters()).unwrap_or_default(),
                );
            }
            View::B(view) => {
                report.push(
                    "table_stats",
                    serde_json::to_string(&view.stats()).unwrap_or_default(),
                );
                report.push(
                    "table_mismatches",
                    serde_json::to_string(&view.mismatches()).unwrap_or_default(),
                );
                let map = view.map();
                report.push(
                    "table_rows",
                    format!(
                        "строк показа {}, строк блоков {:?}, строк таблицы {}",
                        map.rows(),
                        map.blocks(),
                        map.len()
                    ),
                );
                report.push(
                    "row_views",
                    serde_json::to_string(&*self.row_views.borrow()).unwrap_or_default(),
                );
                report.push(
                    "table_geometry",
                    serde_json::to_string(&*self.geometry.borrow()).unwrap_or_default(),
                );
            }
        }
        report.push(
            "steps",
            serde_json::to_string(&*self.steps.borrow()).unwrap_or_default(),
        );
        report.push(
            "resizes",
            serde_json::to_string(&*self.resizes.borrow()).unwrap_or_default(),
        );
        report.push(
            "thread_steps",
            serde_json::to_string(&*self.thread_steps.borrow()).unwrap_or_default(),
        );
        report.push(
            "draft_stats",
            serde_json::to_string(&self.scene.draft_stats()).unwrap_or_default(),
        );
        report.push(
            "selections",
            serde_json::to_string(&*self.selections.borrow()).unwrap_or_default(),
        );
        report.push(
            "relayout",
            serde_json::to_string(&self.scene.relayout()).unwrap_or_default(),
        );
        report.push(
            "document_frame",
            crate::describe_rect(self.view.document_frame()),
        );
        match sampler::sample() {
            Ok(sample) => {
                report.metric(
                    "phys_footprint_bytes",
                    "bytes",
                    vec![sample.footprint_bytes as f64],
                );
                report.metric(
                    "lifetime_max_footprint_bytes",
                    "bytes",
                    vec![sample.lifetime_max_bytes as f64],
                );
            }
            Err(reason) => report.push("sample_error", reason),
        }
        drawmark::on_mark(|_| {});
        drawmark::on_declare(|_| {});
        events::clear();
        events::uninstall();
        self.scene.close();
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
        let milestones = self.milestones.borrow().clone().unwrap_or_default();
        self.check_geometry("finish");
        let seen = *self.max_entries_seen.borrow();
        let visible = self.scene.visible_rows();
        let bounded = match &self.view {
            View::A(view) => {
                let counters = view.cache_counters();
                let farthest = view
                    .cache_rows()
                    .into_iter()
                    .map(|row| crate::paint::distance(row, &visible))
                    .max()
                    .unwrap_or(0);
                Bounded::A {
                    counters,
                    seen,
                    farthest,
                }
            }
            View::B(view) => Bounded::B {
                stats: view.stats(),
                views: *self.row_views.borrow(),
            },
        };
        let drafts = self.scene.draft_stats();
        let (mut report, external) = self.close();
        if let Some(input) = external {
            return Err(Box::new((
                format!("внешний ввод во время сценария: {input}"),
                report,
            )));
        }
        for (name, value) in [
            ("open_model_ms", milestones.model_ms),
            ("open_first_text_ms", milestones.first_text_ms),
            ("open_preliminary_ms", milestones.preliminary_ms),
            ("open_full_ms", milestones.full_ms),
        ] {
            if let Some(value) = value {
                report.metric(name, "ms", vec![value]);
            }
        }
        match (milestones.model_ms, milestones.first_text_ms) {
            (Some(model), Some(text)) => report.finding(
                "open_first_screen",
                Verdict::Pass,
                format!("модель и индекс высот за {model:.1} мс, первый экран покрыт за {text:.1} мс от postEvent"),
            ),
            _ => report.finding("open_first_screen", Verdict::Fail, format!("{milestones:?}")),
        }
        match milestones.full_ms {
            Some(full) if full <= OPEN_DEADLINE.as_secs_f64() * 1e3 => report.finding(
                "open_full_highlight",
                Verdict::Pass,
                format!(
                    "снимок полного разбора применён и видимые строки покрыты за {full:.1} мс, предварительная подсветка — {:?} мс",
                    milestones.preliminary_ms.map(|ms| (ms * 10.0).round() / 10.0)
                ),
            ),
            other => report.finding("open_full_highlight", Verdict::Fail, format!("{other:?}")),
        }
        let steps = self.steps.borrow();
        let keys: Vec<&StepResult> = steps.iter().filter(|step| step.name != "click").collect();
        report.metric(
            "step_latency_ms",
            "ms",
            steps.iter().map(|step| step.latency_ms).collect(),
        );
        if keys.len() == 2 {
            report.finding(
                "keys_scroll",
                Verdict::Pass,
                keys.iter()
                    .map(|step| {
                        format!(
                            "{}: {}, строки {:?} покрыты за {:.1} мс",
                            step.name, step.detail, step.visible, step.latency_ms
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        } else {
            report.finding(
                "keys_scroll",
                Verdict::Fail,
                format!("шагов клавиш {}", keys.len()),
            );
        }
        let clicks: Vec<String> = steps
            .iter()
            .filter(|step| step.name == "click")
            .map(|step| step.detail.clone())
            .collect();
        let expected_clicks = match self.mode {
            Mode::Unified => 1,
            Mode::SideBySide => 2,
        };
        report.finding(
            "click_cursor",
            if clicks.len() == expected_clicks {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            if clicks.is_empty() {
                "щелчка нет".to_owned()
            } else {
                clicks.join("; ")
            },
        );
        let resizes = self.resizes.borrow();
        report.finding(
            "resize_anchor",
            if resizes.len() == 2 && resizes.iter().all(|resize| resize.held) {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            resizes
                .iter()
                .map(|resize| {
                    format!(
                        "{}: ширина {} → {}, режим {}, высота документа {:?}, якорь до {}, после {}, верхний край {} при {:?} по якорю, покрыто за {:.1} мс",
                        resize.name,
                        resize.from_width,
                        resize.to_width,
                        resize.mode,
                        resize.document_height,
                        resize.before,
                        resize.after,
                        resize.scroll_y,
                        resize.anchor_y,
                        resize.latency_ms
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        );
        let selections = self.selections.borrow();
        report.finding(
            "selection",
            if selections.len() == SELECT_STEPS {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            selections
                .iter()
                .map(|step| {
                    format!(
                        "{}: {}, покрыто за {:.1} мс",
                        step.name, step.detail, step.latency_ms
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        );
        let thread_steps = self.thread_steps.borrow();
        report.finding(
            "threads_draft",
            if thread_steps.len() == THREAD_STEPS {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            thread_steps
                .iter()
                .map(|step| {
                    format!(
                        "{}: {}, покрыто за {:.1} мс",
                        step.name, step.detail, step.latency_ms
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        );
        let fields = crate::draft_field::count();
        report.finding(
            "draft_field_single",
            if fields.created == 1 && thread_steps.len() == THREAD_STEPS {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!(
                "полей черновика создано {}, освобождено {}, исходы {:?}",
                fields.created, fields.released, drafts
            ),
        );
        match bounded {
            Bounded::A {
                counters,
                seen,
                farthest,
            } => {
                let bounded =
                    counters.max_entries <= counters.max_capacity && counters.far_builds == 0;
                report.finding(
                    "layout_cache_bounded",
                    if bounded { Verdict::Pass } else { Verdict::Fail },
                    format!(
                        "записей не больше {} при пределе до {} (наблюдалось {} при пределе {}), построено {}, вытеснено {}, дальних раскладок {}; в конце дальняя запись в {farthest} строках от видимых {visible:?}",
                        counters.max_entries, counters.max_capacity, seen.0, seen.1, counters.built, counters.evicted, counters.far_builds
                    ),
                );
            }
            Bounded::B { stats, views } => self.table_findings(&mut report, stats, views),
        }
        Ok(report)
    }
}

/// Ограниченность работы вида к вердикту: кэш раскладки A или виды строк B.
enum Bounded {
    A {
        counters: crate::view_a::layout_cache::Counters,
        seen: (usize, usize),
        farthest: u32,
    },
    B {
        stats: crate::view_b::TableStats,
        views: RowViews,
    },
}

impl OpenSmoke {
    /// Вердикты таблицы B: виды строк ограничены, строки таблицы совпадают с индексом высот,
    /// полный `reloadData` — только при замене diff.
    fn table_findings(
        &self,
        report: &mut Report,
        stats: crate::view_b::TableStats,
        views: RowViews,
    ) {
        report.finding(
            "row_views_bounded",
            if views.over == 0 && stats.far_builds == 0 {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!(
                "видов строк не больше {} при пределе {} ({} видимых строк таблицы), наблюдений сверх предела {}, создано {}, раскладок построено {}, дальних {}",
                views.views, views.limit, views.visible, views.over, stats.row_views_created, stats.layouts_built, stats.far_builds
            ),
        );
        let geometry = self.geometry.borrow();
        let errors: Vec<String> = geometry
            .iter()
            .flat_map(|(when, _, errors)| {
                errors.iter().map(move |error| format!("{when}: {error}"))
            })
            .collect();
        let checked: usize = geometry.iter().map(|(_, checked, _)| checked).sum();
        report.finding(
            "table_geometry",
            if errors.is_empty() && checked > 0 && stats.frame_mismatches == 0 {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!(
                "проверок {}, строк таблицы проверено {checked}, расхождений {}, расхождений высоты таблицы {}{}",
                geometry.len(),
                errors.len(),
                stats.frame_mismatches,
                errors
                    .first()
                    .map_or(String::new(), |error| format!(", первое: {error}"))
            ),
        );
        report.finding(
            "reload_data_once",
            if stats.reload_data == 1 {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!(
                "reloadData {}, noteHeightOfRows {}, noteNumberOfRows {}, вставлено строк {}, удалено {}, вызовов heightOfRow {}",
                stats.reload_data,
                stats.note_heights,
                stats.note_rows,
                stats.inserted_rows,
                stats.removed_rows,
                stats.height_calls
            ),
        );
    }
}

impl crate::scenario::Running for OpenSmoke {
    fn poll(&self) -> Option<Result<Report, Failed>> {
        OpenSmoke::poll(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Applied;

    fn applied(source: Source, at_ns: u64, generation: u64, changed_rows: usize) -> Applied {
        Applied {
            source,
            request: 1,
            at_ns,
            generation,
            changed_rows,
            compute_ms: 0.0,
        }
    }

    fn cover(generation: u64, at_ns: u64) -> Coverage {
        Coverage {
            generation,
            at_ns,
            rows: (0, 48),
        }
    }

    #[test]
    fn milestones_take_coverage_after_each_change() {
        let timeline = Timeline {
            open_ns: Some(1_000_000),
            parsed_ns: Some(5_000_000),
            model_ns: Some(10_000_000),
            open_generation: Some(2),
            rows: Some(100_000),
            document_height: Some(1_500_000),
            snapshots: vec![
                applied(Source::Local, 12_000_000, 3, 48),
                applied(Source::Full, 400_000_000, 3, 0),
            ],
        };
        let coverage = [cover(2, 11_000_000), cover(3, 14_000_000)];
        let milestones = Milestones::of(0, &timeline, &coverage);
        assert_eq!(milestones.model_ms, Some(10.0));
        assert_eq!(milestones.first_text_ms, Some(11.0));
        assert_eq!(milestones.preliminary_ms, Some(14.0));
        // Полный разбор строк не изменил: экран верен с момента применения снимка.
        assert_eq!(milestones.full_ms, Some(400.0));
    }

    #[test]
    fn correction_waits_for_its_own_coverage() {
        let timeline = Timeline {
            open_generation: Some(2),
            model_ns: Some(10_000_000),
            snapshots: vec![
                applied(Source::Local, 12_000_000, 3, 48),
                applied(Source::Full, 190_000_000, 4, 5),
            ],
            ..Timeline::default()
        };
        let coverage = [cover(2, 11_000_000), cover(3, 14_000_000)];
        assert_eq!(Milestones::of(0, &timeline, &coverage).full_ms, None);
        let coverage = [
            cover(2, 11_000_000),
            cover(3, 14_000_000),
            cover(4, 195_000_000),
        ];
        assert_eq!(Milestones::of(0, &timeline, &coverage).full_ms, Some(195.0));
    }

    #[test]
    fn selection_steps_check_side_ends_and_cursor() {
        let at = |line| LineAddr {
            side: Side::New,
            line,
        };
        let selection = |tail, head| Selection {
            side: Side::New,
            tail,
            head,
        };
        assert!(expect_met(Expect::Cursor(at(5)), Some(at(5)), None));
        assert!(!expect_met(
            Expect::Cursor(at(5)),
            Some(at(5)),
            Some(selection(5, 5))
        ));
        let below = Expect::Selection {
            side: Side::New,
            tail: 5,
            head: None,
            cursor: None,
        };
        assert!(expect_met(below, Some(at(7)), Some(selection(5, 7))));
        assert!(!expect_met(below, Some(at(5)), Some(selection(5, 5))));
        assert!(!expect_met(
            below,
            Some(at(7)),
            Some(Selection {
                side: Side::Old,
                tail: 5,
                head: 7
            })
        ));
        let exact = Expect::Selection {
            side: Side::New,
            tail: 5,
            head: Some(9),
            cursor: Some(at(9)),
        };
        assert!(expect_met(exact, Some(at(9)), Some(selection(5, 9))));
        assert!(!expect_met(exact, Some(at(8)), Some(selection(5, 9))));
        assert!(expect_met(Expect::NoSelection, Some(at(1)), None));
        assert!(!expect_met(
            Expect::NoSelection,
            Some(at(1)),
            Some(selection(1, 2))
        ));
    }

    #[test]
    fn first_text_needs_the_open_generation() {
        let timeline = Timeline {
            open_generation: Some(5),
            ..Timeline::default()
        };
        assert_eq!(covered_since(&[cover(4, 3)], 5, 0), None);
        assert_eq!(
            Milestones::of(0, &timeline, &[cover(4, 3)]).first_text_ms,
            None
        );
        assert_eq!(covered_since(&[cover(4, 3), cover(6, 9)], 5, 0), Some(9));
    }
}
