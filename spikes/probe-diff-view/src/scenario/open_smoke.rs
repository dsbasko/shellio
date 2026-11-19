//! Сценарий `open-smoke`: открытие фикстуры `added` в виде варианта и режима, первый экран
//! нарисован, затем `G`, `gg` и щелчок по строке. Ожидания и сроки записаны в отчёт до первого
//! прогона. Бюджет открытия здесь не выносится: отметки открытия — наблюдение, вердикт по ним
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
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSResponder, NSWindow};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use serde::Serialize;

use probe_diff_view::fixtures::{self, FixtureName};
use probe_diff_view::highlight::Source;
use probe_diff_view::model::Mode;

use super::calibrate_run::Failed;
use super::drawmark::{self, Ledger};
use super::events::{self, Key};
use super::sampler;
use super::watchdog::SharedTrace;
use crate::args::{Variant, ViewMode, ViewOptions};
use crate::results::{Report, Verdict};
use crate::scene::{Scene, Timeline};
use crate::view_a::ViewA;

/// Срок открытия: от `postEvent` клавиши до покрытия после снимка полного разбора.
pub const OPEN_DEADLINE: Duration = Duration::from_secs(5);
/// Срок шагов `G`, `gg` и щелчка.
pub const STEP_DEADLINE: Duration = Duration::from_secs(2);
/// Щелчок по этой видимой строке, считая с 1.
pub const CLICK_ROW: u32 = 5;
/// Допуск положения прокрутки, точки.
const SCROLL_TOLERANCE: f64 = 1.0;

/// Покрытие видимых строк: поколение учёта и время отметки, которая его закрыла.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Coverage {
    pub generation: u64,
    pub at_ns: u64,
    pub rows: (u32, u32),
}

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
        posted: Vec<u64>,
        posted_ns: u64,
        deadline: Instant,
    },
    Done,
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

pub struct OpenSmoke {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    view: Rc<ViewA>,
    scene: Rc<Scene>,
    coverage: Rc<RefCell<Vec<Coverage>>>,
    ledger: Rc<RefCell<Ledger>>,
    step: RefCell<Step>,
    milestones: RefCell<Option<Milestones>>,
    steps: RefCell<Vec<StepResult>>,
    max_entries_seen: RefCell<(usize, usize)>,
    report: RefCell<Option<Report>>,
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
        match (view.variant, view.mode) {
            (Some(Variant::A), Some(ViewMode::Unified)) => {}
            (variant, mode) => {
                return Err(Box::new((
                    format!(
                        "не реализовано: вариант {}, режим {} — задачи 8 и 9",
                        variant.map_or("?", Variant::name),
                        mode.map_or("?", ViewMode::name)
                    ),
                    report,
                )));
            }
        }
        let generated = Instant::now();
        let fixture = fixtures::generate(FixtureName::Added);
        report.metric(
            "fixture_generate_ms",
            "ms",
            vec![generated.elapsed().as_secs_f64() * 1e3],
        );
        report.push(
            "fixture",
            format!("added, отпечаток {:016x}", fixture.digest()),
        );
        let app = NSApplication::sharedApplication(mtm);
        let theme = crate::text::current_theme(mtm);
        report.push("theme", theme.name());
        let bounds = window
            .contentView()
            .map(|content| content.bounds())
            .unwrap_or_else(|| window.contentLayoutRect());
        let surface = ViewA::new(mtm, bounds, theme);
        let font = surface.font();
        report.push(
            "font",
            format!(
                "{} {} точек: клетка {:.4} точки, строка {} точек",
                crate::text::FONT_NAME,
                crate::text::FONT_SIZE,
                font.char_width,
                font.line_height
            ),
        );
        let scene = match Scene::new(fixture, Mode::Unified, font) {
            Ok(scene) => scene,
            Err(reason) => return Err(Box::new((reason, report))),
        };
        surface.bind(&scene);
        scene.attach(surface.clone());
        window.setContentView(Some(surface.scroll_view()));
        let responder: &NSResponder = surface.responder();
        if !window.makeFirstResponder(Some(responder))
            || !events::is_first_responder(window, responder)
        {
            return Err(Box::new((
                format!(
                    "document view не стал first responder: {}",
                    events::focus_state(&app)
                ),
                report,
            )));
        }
        events::install(app.clone(), trace);
        events::note("сценарий open-smoke");
        let cursor = match events::warp_cursor_outside(mtm, &app) {
            Ok(point) => point,
            Err(reason) => {
                events::uninstall();
                return Err(Box::new((reason, report)));
            }
        };
        report.push("cursor_warped_to", events::describe_point(cursor));
        drawmark::take();
        drawmark::take_declared();
        let ledger = Rc::new(RefCell::new(Ledger::default()));
        let coverage = Rc::new(RefCell::new(Vec::new()));
        {
            let ledger = Rc::clone(&ledger);
            drawmark::on_declare(move |declared| ledger.borrow_mut().declare(declared));
        }
        {
            let ledger = Rc::clone(&ledger);
            let coverage = Rc::clone(&coverage);
            let scene = Rc::downgrade(&scene);
            drawmark::on_mark(move |mark| {
                let mut ledger = ledger.borrow_mut();
                ledger.mark(mark);
                let Some(scene) = scene.upgrade() else {
                    return;
                };
                let visible = scene.visible_rows();
                if visible.is_empty() {
                    return;
                }
                let rows = visible.start as usize..visible.end as usize;
                if ledger.uncovered(rows).is_none() {
                    let generation = ledger.latest();
                    let mut coverage = coverage.borrow_mut();
                    if coverage.last().is_none_or(|last: &Coverage| {
                        last.generation != generation || last.rows != (visible.start, visible.end)
                    }) {
                        coverage.push(Coverage {
                            generation,
                            at_ns: mark.draw_end_ns,
                            rows: (visible.start, visible.end),
                        });
                    }
                }
            });
        }
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
            max_entries_seen: RefCell::new((0, 0)),
            report: RefCell::new(Some(report)),
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
        let (len, capacity) = self.view.cache_len();
        let mut seen = self.max_entries_seen.borrow_mut();
        if len > seen.0 {
            *seen = (len, capacity);
        }
    }

    pub fn poll(&self) -> Option<Result<Report, Failed>> {
        self.observe_cache();
        let errors = self.scene.errors();
        if !errors.is_empty() {
            return Some(Err(self.fail(format!("сцена: {}", errors.join("; ")))));
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
                        _ => self.click(),
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
                posted,
                posted_ns,
                deadline,
            } => {
                let expected = self.scene.row_addr(row);
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
                        detail: format!("строка {row}, курсор {cursor:?}"),
                    });
                    return Ok(Step::Done);
                }
                if Instant::now() < deadline {
                    return Ok(Step::Click {
                        row,
                        posted,
                        posted_ns,
                        deadline,
                    });
                }
                Err(format!(
                    "щелчок по строке {row} не поставил курсор за {} мс: ожидался {expected:?}, курсор {cursor:?}, покрыты {}",
                    STEP_DEADLINE.as_millis(),
                    self.covered_now()
                ))
            }
            Step::Done => Ok(Step::Done),
        }
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

    /// Щелчок по центру видимой строки [`CLICK_ROW`] без модификаторов.
    fn click(&self) -> Result<Step, String> {
        let visible = self.scene.visible_rows();
        let row = visible.start + CLICK_ROW - 1;
        if row >= visible.end {
            return Err(format!("видимых строк меньше {CLICK_ROW}: {visible:?}"));
        }
        let area = self
            .view
            .row_rect(row)
            .ok_or_else(|| format!("нет прямоугольника строки {row}"))?;
        let screen = self.window.convertRectToScreen(self.view.to_window(area));
        let point = events::center(NSRect::new(
            screen.origin,
            NSSize::new(screen.size.width.min(400.0), screen.size.height),
        ));
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
        report.push(
            "layout_cache",
            serde_json::to_string(&self.view.cache_counters()).unwrap_or_default(),
        );
        report.push(
            "draw_stats",
            serde_json::to_string(&self.view.draw_stats()).unwrap_or_default(),
        );
        report.push(
            "dirty_rects",
            serde_json::to_string(&self.view.dirty_log()).unwrap_or_default(),
        );
        report.push(
            "steps",
            serde_json::to_string(&*self.steps.borrow()).unwrap_or_default(),
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
        let counters = self.view.cache_counters();
        let seen = *self.max_entries_seen.borrow();
        let visible = self.scene.visible_rows();
        let farthest = self
            .view
            .cache_rows()
            .into_iter()
            .map(|row| crate::view_a::layout_cache::distance(row, &visible))
            .max()
            .unwrap_or(0);
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
        match steps.iter().find(|step| step.name == "click") {
            Some(step) => report.finding("click_cursor", Verdict::Pass, step.detail.clone()),
            None => report.finding("click_cursor", Verdict::Fail, "щелчка нет"),
        }
        let bounded = counters.max_entries <= counters.max_capacity && counters.far_builds == 0;
        report.finding(
            "layout_cache_bounded",
            if bounded { Verdict::Pass } else { Verdict::Fail },
            format!(
                "записей не больше {} при пределе до {} (наблюдалось {} при пределе {}), построено {}, вытеснено {}, дальних раскладок {}; в конце дальняя запись в {farthest} строках от видимых {visible:?}",
                counters.max_entries, counters.max_capacity, seen.0, seen.1, counters.built, counters.evicted, counters.far_builds
            ),
        );
        Ok(report)
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
