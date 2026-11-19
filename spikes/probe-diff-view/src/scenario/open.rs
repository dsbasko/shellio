//! Сценарии `open` и `open-warm`: открытие фикстуры в виде варианта и режима, бюджет 200 мс.
//! `open` — одно открытие в свежем процессе, `run.sh` запускает 11 таких процессов на фикстуру,
//! вариант и режим. `open-warm` — 20 открытий в одном процессе без вердикта.
//!
//! До клавиши открытия рабочий поток генерирует или читает фикстуру и читает эталон подсветки,
//! затем стенд ставит вид и ждёт компиляции запроса подсветки и отображения окна. В памяти в
//! этот момент только сырые байты фикстуры: разбор, индексы, деревья и раскладка строятся после
//! клавиши. Отметки открытия считает [`probe_diff_view::open::marks`] по журналу применённых
//! снимков и журналу покрытий видимых строк. Открытие закончено, когда исправление полным
//! разбором применено и видимые строки покрыты после него.

use std::cell::RefCell;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::NSWindow;
use serde::Serialize;

use probe_diff_view::fixtures::Fixture;
use probe_diff_view::highlight::reference;
use probe_diff_view::highlight::{SideStyles, Sides};
use probe_diff_view::model::Mode;
use probe_diff_view::open::{self, Cover, Kept, OpenFixture, OpenInput, OpenMarks};
use probe_diff_view::real::{self, RealSource};

use super::calibrate_run::Failed;
use super::drawmark;
use super::events::{self, Key};
use super::sampler;
use super::stage::{Stage, View};
use super::watchdog::SharedTrace;
use crate::args::{Variant, ViewMode, ViewOptions};
use crate::results::{OPEN_FIXTURE_FINDING, Report, Verdict};
use crate::scene::OpenAt;

/// Срок загрузки фикстуры и эталона на рабочем потоке.
pub const LOAD_DEADLINE: Duration = Duration::from_secs(30);
/// Срок готовности до клавиши: запрос подсветки скомпилирован, окно ничего не ждёт отображения.
pub const READY_DEADLINE: Duration = Duration::from_secs(10);
/// Срок одного открытия: от `postEvent` клавиши до покрытия видимых строк после исправления.
pub const OPEN_DEADLINE: Duration = Duration::from_secs(30);

/// Фикстура и эталон её активного файла в памяти до клавиши открытия.
struct Loaded {
    fixture: Fixture,
    reference: Sides<SideStyles>,
    metrics: Vec<(&'static str, f64)>,
    notes: Vec<(&'static str, String)>,
}

fn ms(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1e3
}

/// Загрузка на рабочем потоке: генерация или чтение фикстуры и эталон с проверкой отпечатка.
fn load(fixture: OpenFixture, dir: &Path) -> Result<Loaded, String> {
    let started = Instant::now();
    let mut metrics = Vec::new();
    let mut notes = Vec::new();
    let (loaded, digest) = match fixture.synthetic() {
        Some(name) => {
            let loaded = probe_diff_view::fixtures::generate(name);
            metrics.push(("fixture_generate_ms", ms(started.elapsed())));
            let digest = loaded.digest();
            (loaded, digest)
        }
        None => {
            let source = real::load(&real::dir(dir))?
                .ok_or_else(|| format!("фикстура real не подготовлена в {}", dir.display()))?;
            let prepared = match source {
                RealSource::Prepared(prepared) => prepared,
                RealSource::NotRun(reason) => return Err(format!("фикстура real: {reason}")),
            };
            metrics.push(("fixture_read_ms", ms(started.elapsed())));
            if let Some(git) = prepared.git_read_ms {
                metrics.push(("real_git_read_ms", git));
            }
            notes.push((
                "real_source",
                format!(
                    "коммиты {} → {}, файлов {}, байт {}, вызовов git {:?}",
                    prepared.commits.0,
                    prepared.commits.1,
                    prepared.files.len(),
                    prepared.bytes(),
                    prepared.git_calls
                ),
            ));
            let digest = prepared.digest();
            let active = prepared
                .fixture(0)
                .ok_or_else(|| "фикстура real без файлов".to_owned())?;
            (active, digest)
        }
    };
    let read = Instant::now();
    let name = fixture.name();
    let file = reference::read(dir, name)?;
    file.check(name, digest)?;
    let sides = file
        .file(&loaded.path)
        .ok_or_else(|| format!("в эталоне {name} нет файла {}", loaded.path))?
        .sides()?;
    metrics.push(("reference_read_ms", ms(read.elapsed())));
    notes.push((
        "fixture_bytes",
        format!(
            "diff {}, старая сторона {}, новая {}",
            loaded.diff.len(),
            loaded.old.len(),
            loaded.new.len()
        ),
    ));
    Ok(Loaded {
        fixture: loaded,
        reference: sides,
        metrics,
        notes,
    })
}

/// Причина `not_run` процесса без окна: `run.sh` не нашёл закреплённых коммитов фикстуры `real`.
pub fn not_run_reason(view: ViewOptions, fixtures: &Path) -> Option<String> {
    if view.fixture != Some(OpenFixture::Real) {
        return None;
    }
    match real::load(&real::dir(fixtures)) {
        Ok(Some(RealSource::NotRun(reason))) => Some(reason),
        Ok(Some(RealSource::Prepared(_)) | None) | Err(_) => None,
    }
}

/// `not_run` фикстуры в итог процесса.
pub fn not_run_report(reason: &str) -> Report {
    let mut report = Report::default();
    report.finding(OPEN_FIXTURE_FINDING, Verdict::NotRun, reason);
    report
}

enum Step {
    Loading {
        rx: Receiver<Result<Loaded, String>>,
        deadline: Instant,
    },
    Ready {
        deadline: Instant,
    },
    Opening {
        index: usize,
        posted: Vec<u64>,
        posted_ns: u64,
        deadline: Instant,
    },
    Done,
}

/// Наблюдение открытия рядом с отметками.
#[derive(Clone, Debug, Serialize)]
struct OpenNote {
    index: usize,
    path: String,
    visible_rows: (u32, u32),
    scroll_y: f64,
    snapshots: usize,
    kept: Vec<(String, u64, usize)>,
}

pub struct Open {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    trace: SharedTrace,
    variant: Variant,
    mode: Mode,
    fixture: OpenFixture,
    opens: usize,
    stage: RefCell<Option<Stage>>,
    loaded: RefCell<Option<(String, Sides<SideStyles>)>>,
    step: RefCell<Step>,
    marks: RefCell<Vec<OpenMarks>>,
    notes: RefCell<Vec<OpenNote>>,
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

impl Open {
    /// `warm` — 20 открытий в одном процессе (`open-warm`), иначе одно.
    pub fn start(
        mtm: MainThreadMarker,
        window: &NSWindow,
        report: Report,
        trace: SharedTrace,
        view: ViewOptions,
        fixtures: Option<&Path>,
        warm: bool,
    ) -> Result<Open, Failed> {
        let (variant, mode, fixture) = match (view.variant, view.mode, view.fixture) {
            (Some(variant), Some(mode), Some(fixture)) => (
                variant,
                match mode {
                    ViewMode::Unified => Mode::Unified,
                    ViewMode::Side => Mode::SideBySide,
                },
                fixture,
            ),
            _ => {
                return Err(Box::new((format!("нет оси открытия: {view:?}"), report)));
            }
        };
        let Some(dir) = fixtures.map(Path::to_path_buf) else {
            return Err(Box::new(("сценарию нужен --fixtures".to_owned(), report)));
        };
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("probe-diff-open-load".to_owned())
            .spawn(move || {
                let _ = tx.send(load(fixture, &dir));
            });
        if let Err(err) = spawned {
            return Err(Box::new((
                format!("поток загрузки не запущен: {err}"),
                report,
            )));
        }
        Ok(Open {
            mtm,
            window: window.retain(),
            trace,
            variant,
            mode,
            fixture,
            opens: if warm { open::WARM_OPENS } else { 1 },
            stage: RefCell::new(None),
            loaded: RefCell::new(None),
            step: RefCell::new(Step::Loading {
                rx,
                deadline: Instant::now() + LOAD_DEADLINE,
            }),
            marks: RefCell::new(Vec::new()),
            notes: RefCell::new(Vec::new()),
            report: RefCell::new(Some(report)),
        })
    }

    pub fn poll(&self) -> Option<Result<Report, Failed>> {
        if let Some(stage) = self.stage.borrow().as_ref() {
            let errors = stage.scene.errors();
            if !errors.is_empty() {
                return Some(Err(self.fail(format!("сцена: {}", errors.join("; ")))));
            }
            if let View::B(view) = &stage.view {
                let failures = view.stats().height_failures;
                if failures > 0 {
                    return Some(Err(self.fail(format!(
                        "таблица B получила {failures} высот строк при занятой модели"
                    ))));
                }
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
            Step::Loading { rx, deadline } => match rx.try_recv() {
                Ok(loaded) => self.loaded(loaded?),
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    Ok(Step::Loading { rx, deadline })
                }
                Err(TryRecvError::Empty) => Err(format!(
                    "фикстура и эталон не загружены за {} с",
                    LOAD_DEADLINE.as_secs()
                )),
                Err(TryRecvError::Disconnected) => Err("поток загрузки упал".to_owned()),
            },
            Step::Ready { deadline } => {
                let stage = self.stage.borrow();
                let stage = stage
                    .as_ref()
                    .ok_or_else(|| "нет стенда до клавиши".to_owned())?;
                match stage.scene.query_ready() {
                    Some(Err(reason)) => {
                        Err(format!("запрос подсветки не скомпилирован: {reason}"))
                    }
                    Some(Ok(compile_ms)) if !self.window.viewsNeedDisplay() => {
                        if let Some(report) = self.report.borrow_mut().as_mut() {
                            report.metric("query_compile_ms", "ms", vec![compile_ms]);
                        }
                        self.press(0)
                    }
                    _ if Instant::now() < deadline => Ok(Step::Ready { deadline }),
                    state => Err(format!(
                        "до клавиши не готово за {} с: запрос {state:?}, viewsNeedDisplay {}",
                        READY_DEADLINE.as_secs(),
                        self.window.viewsNeedDisplay()
                    )),
                }
            }
            Step::Opening {
                index,
                posted,
                posted_ns,
                deadline,
            } => match self.opened(index, &posted, posted_ns)? {
                Some(marks) => {
                    self.marks.borrow_mut().push(marks);
                    if index + 1 < self.opens {
                        self.press(index + 1)
                    } else {
                        Ok(Step::Done)
                    }
                }
                None if Instant::now() < deadline => Ok(Step::Opening {
                    index,
                    posted,
                    posted_ns,
                    deadline,
                }),
                None => Err(format!(
                    "открытие {} не закончено за {} с: {}",
                    index + 1,
                    OPEN_DEADLINE.as_secs(),
                    self.describe(posted_ns)
                )),
            },
            Step::Done => Ok(Step::Done),
        }
    }

    /// Фикстура в памяти: вид ставится в окно, сцена держит применённые снимки.
    fn loaded(&self, loaded: Loaded) -> Result<Step, String> {
        let mut report = self
            .report
            .borrow_mut()
            .take()
            .ok_or_else(|| "итог уже забран".to_owned())?;
        for (name, value) in &loaded.metrics {
            report.metric(name, "ms", vec![*value]);
        }
        for (key, value) in &loaded.notes {
            report.push(key, value);
        }
        report.push(
            "open_at",
            if self.fixture.at_last_hunk() {
                "последний ханк"
            } else {
                "начало файла"
            },
        );
        let path = loaded.fixture.path.clone();
        let stage = super::stage::prepare(
            self.mtm,
            &self.window,
            &mut report,
            self.trace.clone(),
            self.variant,
            self.mode,
            loaded.fixture,
            if self.opens > 1 { "open-warm" } else { "open" },
        );
        match sampler::sample() {
            Ok(sample) => report.metric(
                "footprint_before_open_bytes",
                "bytes",
                vec![sample.footprint_bytes as f64],
            ),
            Err(reason) => report.push("sample_error", reason),
        }
        *self.report.borrow_mut() = Some(report);
        let stage = stage?;
        stage.scene.keep_snapshots();
        stage.scene.set_open_at(if self.fixture.at_last_hunk() {
            OpenAt::LastHunk
        } else {
            OpenAt::Top
        });
        *self.stage.borrow_mut() = Some(stage);
        *self.loaded.borrow_mut() = Some((path, loaded.reference));
        Ok(Step::Ready {
            deadline: Instant::now() + READY_DEADLINE,
        })
    }

    fn press(&self, index: usize) -> Result<Step, String> {
        let posted_ns = now_ns();
        let posted = events::press(Key::OPEN)?;
        Ok(Step::Opening {
            index,
            posted,
            posted_ns,
            deadline: Instant::now() + OPEN_DEADLINE,
        })
    }

    /// Отметки законченного открытия; `None` — ещё идёт.
    fn opened(
        &self,
        index: usize,
        posted: &[u64],
        posted_ns: u64,
    ) -> Result<Option<OpenMarks>, String> {
        let stage = self.stage.borrow();
        let stage = stage
            .as_ref()
            .ok_or_else(|| "нет стенда открытия".to_owned())?;
        let timeline = stage.scene.timeline();
        // Пока клавиша не доставлена, журналы прежнего открытия выглядят законченными.
        if !returned(posted) || !timeline.open_ns.is_some_and(|ns| ns >= posted_ns) {
            return Ok(None);
        }
        if !stage.covered_now() {
            return Ok(None);
        }
        let Some(visible) = stage.scene.visible_lines() else {
            return Ok(None);
        };
        let loaded = self.loaded.borrow();
        let (path, reference) = loaded
            .as_ref()
            .ok_or_else(|| "нет эталона открытия".to_owned())?;
        let kept = stage.scene.kept();
        let kept_refs: Vec<Kept<'_>> = kept
            .iter()
            .map(|kept| Kept {
                source: kept.applied.source,
                generation: kept.applied.generation,
                at_ns: kept.applied.at_ns,
                styles: &kept.styles,
            })
            .collect();
        let coverage: Vec<Cover> = stage
            .coverage
            .borrow()
            .iter()
            .map(|cover| Cover {
                generation: cover.generation,
                at_ns: cover.at_ns,
            })
            .collect();
        let marks = open::marks(&OpenInput {
            posted_ns,
            model_ns: timeline.model_ns,
            open_generation: timeline.open_generation,
            kept: &kept_refs,
            coverage: &coverage,
            visible: &visible,
            reference,
        })?;
        if !marks.complete() {
            return Ok(None);
        }
        let shown = stage.scene.file_path().unwrap_or_default();
        if shown != *path {
            return Err(format!(
                "активный файл {shown}, а фикстура и эталон — {path}"
            ));
        }
        let rows = stage.scene.visible_rows();
        self.notes.borrow_mut().push(OpenNote {
            index,
            path: shown,
            visible_rows: (rows.start, rows.end),
            scroll_y: stage.view.viewport_y(),
            snapshots: timeline.snapshots.len(),
            kept: kept
                .iter()
                .map(|kept| {
                    (
                        format!("{:?}", kept.applied.source),
                        kept.applied.generation,
                        kept.applied.changed_rows,
                    )
                })
                .collect(),
        });
        Ok(Some(marks))
    }

    fn describe(&self, posted_ns: u64) -> String {
        let stage = self.stage.borrow();
        let Some(stage) = stage.as_ref() else {
            return "нет стенда".to_owned();
        };
        let timeline = stage.scene.timeline();
        format!(
            "клавиша {} мс назад, открытие {:?}, модель {:?}, снимков {}, видимые строки {:?}, покрыты {}, покрытий {}",
            now_ns().saturating_sub(posted_ns) / 1_000_000,
            timeline.open_ns.map(|ns| ns >= posted_ns),
            timeline
                .model_ns
                .map(|ns| ns.saturating_sub(posted_ns) / 1_000_000),
            timeline.snapshots.len(),
            stage.scene.visible_rows(),
            stage.covered_now(),
            stage.coverage.borrow().len()
        )
    }

    /// Журналы в итог; стенд и сцена снимаются.
    fn close(&self) -> (Report, Option<String>) {
        let mut report = self.report.borrow_mut().take().unwrap_or_default();
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
            "opens",
            serde_json::to_string(&*self.notes.borrow()).unwrap_or_default(),
        );
        report.push("coverage_events", stage.coverage.borrow().len());
        report.push("marks", drawmark::take().len());
        report.push("ledger_size", format!("{:?}", stage.ledger.borrow().size()));
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
            Ok(sample) => {
                report.metric(
                    "phys_footprint_bytes",
                    "bytes",
                    vec![sample.footprint_bytes as f64],
                );
            }
            Err(reason) => report.push("sample_error", reason),
        }
        self.table_findings(&stage, &mut report);
        drawmark::take_declared();
        drawmark::on_mark(|_| {});
        drawmark::on_declare(|_| {});
        events::clear();
        events::uninstall();
        stage.scene.close();
        (report, external)
    }

    /// Вид B: высота таблицы против индекса высот и число `reloadData` — 1 на открытие и
    /// `2N − 1` на N открытий в одном процессе (очистка и загрузка у каждого повторного).
    fn table_findings(&self, stage: &Stage, report: &mut Report) {
        let View::B(view) = &stage.view else {
            return;
        };
        let stats = view.stats();
        report.finding(
            "table_geometry",
            if stats.frame_mismatches == 0 {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!(
                "расхождений высоты таблицы с индексом высот {}",
                stats.frame_mismatches
            ),
        );
        let opened = self.marks.borrow().len() as u64;
        let expected = (2 * opened).saturating_sub(1);
        report.finding(
            "reload_data",
            if stats.reload_data == expected {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            format!(
                "reloadData {} при {} открытиях, ожидалось {expected}",
                stats.reload_data, opened
            ),
        );
    }

    fn fail(&self, reason: String) -> Failed {
        let (mut report, external) = self.close();
        report.opens = self.marks.borrow().clone();
        let reason = match external {
            Some(input) => format!("{reason}; внешний ввод во время сценария: {input}"),
            None => reason,
        };
        Box::new((reason, report))
    }

    fn finish(&self) -> Result<Report, Failed> {
        let (mut report, external) = self.close();
        let marks = self.marks.borrow().clone();
        if let Some(input) = external {
            report.opens = marks;
            return Err(Box::new((
                format!("внешний ввод во время сценария: {input}"),
                report,
            )));
        }
        let series = |pick: fn(&OpenMarks) -> Option<f64>| -> Vec<f64> {
            marks.iter().filter_map(pick).collect()
        };
        for (name, values) in [
            ("open_model_ms", series(|marks| marks.model_ms)),
            ("open_first_text_ms", series(|marks| marks.first_text_ms)),
            ("open_preliminary_ms", series(|marks| marks.preliminary_ms)),
            ("open_correction_ms", series(|marks| marks.correction_ms)),
            ("open_end_ms", series(|marks| marks.end_ms)),
        ] {
            if !values.is_empty() {
                report.metric(name, "ms", values);
            }
        }
        let wrong: Vec<String> = marks
            .iter()
            .enumerate()
            .filter(|(_, marks)| marks.correction_mismatched != Some(0))
            .map(|(index, marks)| format!("{}: {:?}", index + 1, marks.correction_mismatched))
            .collect();
        report.finding(
            "open_reference_full",
            if wrong.is_empty() {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            if wrong.is_empty() {
                format!(
                    "исправление полным разбором совпало с эталоном по {} видимым строкам в каждом из {} открытий",
                    marks.first().map_or(0, |marks| marks.visible_lines),
                    marks.len()
                )
            } else {
                format!(
                    "исправление расходится с эталоном, открытие: строк — {}",
                    wrong.join(", ")
                )
            },
        );
        report.opens = marks;
        Ok(report)
    }
}

impl crate::scenario::Running for Open {
    fn poll(&self) -> Option<Result<Report, Failed>> {
        Open::poll(self)
    }
}
