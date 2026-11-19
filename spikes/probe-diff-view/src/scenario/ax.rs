//! Сценарий `ax`: доступность вида варианта и режима для внешнего клиента. Стенд открывает
//! `modified` (в unified есть контекстные, удалённые и добавленные строки, в «Рядом» — пары) и
//! запускает процесс пробы `--ax-client` со своим pid. Шаги: первый экран, прокрутка на 10
//! экранов, щелчок и `v j j`, `Escape` и `G`, прокрутка на полэкрана вверх. После каждого шага
//! видимые строки покрыты, затем клиент читает дерево AX, а стенд сравнивает снимок с моделью,
//! описанием строк которой пользуются и виды ([`probe_diff_view::access`]). Ожидание снимка
//! записывается в момент запроса: пока клиент читает, ничего не меняется. Снимок клиента и
//! ожидание каждого шага стенд пишет в `ax-<шаг>.json` каталога процесса.
//!
//! Главный поток клиента не ждёт: запросы AX обслуживает основной цикл этого процесса.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::{NSEventModifierFlags, NSScreen, NSWindow};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use serde::Serialize;

use probe_diff_view::access::{
    self, AxFrame, AxSnapshot, Comparison, Expectation, ExpectedFocus, ExpectedRow,
};
use probe_diff_view::fixtures::FixtureName;
use probe_diff_view::model::{Mode, Side};

use super::ax_client::{Client, Reply};
use super::calibrate_run::Failed;
use super::drawmark;
use super::events::{self, Key};
use super::open_smoke::Milestones;
use super::sampler;
use super::stage::{Stage, View};
use super::watchdog::SharedTrace;
use crate::args::{Variant, ViewMode, ViewOptions};
use crate::results::{Report, Verdict};
use crate::view_b::Item;

/// Срок готовности клиента от запуска.
pub const CLIENT_DEADLINE: Duration = Duration::from_secs(10);
/// Срок открытия: от `postEvent` клавиши до покрытия после снимка полного разбора.
pub const OPEN_DEADLINE: Duration = Duration::from_secs(5);
/// Срок шага: от действия до выполнения условия и покрытия видимых строк.
pub const STEP_DEADLINE: Duration = Duration::from_secs(2);
/// Срок снимка: от запроса до ответа клиента.
pub const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(20);
/// Экранов прокрутки шага `scroll`.
const SCROLL_SCREENS: f64 = 10.0;
/// Щелчок шага `select` по этой видимой строке, считая с 1.
const CLICK_ROW: u32 = 5;
/// Допуск положения прокрутки, точки.
const SCROLL_TOLERANCE: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum Step {
    First,
    Scroll,
    Select,
    Bottom,
    Reuse,
}

impl Step {
    const ALL: [Step; 5] = [
        Step::First,
        Step::Scroll,
        Step::Select,
        Step::Bottom,
        Step::Reuse,
    ];

    fn name(self) -> &'static str {
        match self {
            Step::First => "first",
            Step::Scroll => "scroll",
            Step::Select => "select",
            Step::Bottom => "bottom",
            Step::Reuse => "reuse",
        }
    }

    fn next(self) -> Option<Step> {
        let at = Step::ALL.iter().position(|step| *step == self)?;
        Step::ALL.get(at + 1).copied()
    }

    /// Уведомления, которые клиент обязан получить между прошлым снимком и этим.
    fn wanted_notes(self) -> &'static [&'static str] {
        match self {
            Step::First => &[],
            Step::Scroll | Step::Reuse => &["AXLayoutChanged"],
            Step::Select => &["AXFocusedUIElementChanged", "AXSelectedRowsChanged"],
            Step::Bottom => &[
                "AXFocusedUIElementChanged",
                "AXSelectedRowsChanged",
                "AXLayoutChanged",
            ],
        }
    }
}

enum Phase {
    Client {
        deadline: Instant,
    },
    Settle {
        step: Step,
        posted: Vec<u64>,
        posted_ns: u64,
        target_y: Option<f64>,
        deadline: Instant,
    },
    Snapshot {
        step: Step,
        expectation: Box<Expectation>,
        settled_ms: f64,
        requested: Instant,
    },
    Done,
}

/// Итог шага: снимок клиента и сверка с моделью.
#[derive(Clone, Debug, Serialize)]
struct StepRecord {
    step: Step,
    settled_ms: f64,
    snapshot_ms: f64,
    rows: usize,
    focus: String,
    comparison: Comparison,
    notes: BTreeMap<String, usize>,
    missing_notes: Vec<&'static str>,
    reuse: (usize, usize),
    held: usize,
    held_released: usize,
}

/// Файл шага: снимок клиента и ожидание модели в момент запроса.
#[derive(Serialize)]
struct StepFile<'a> {
    snapshot: &'a AxSnapshot,
    expected: &'a Expectation,
}

pub struct Ax {
    mtm: MainThreadMarker,
    out: PathBuf,
    window: Retained<NSWindow>,
    stage: Stage,
    mode: Mode,
    client: RefCell<Client>,
    phase: RefCell<Phase>,
    records: RefCell<Vec<StepRecord>>,
    snapshots: RefCell<Vec<AxSnapshot>>,
    expectations: RefCell<Vec<Expectation>>,
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

impl Ax {
    pub fn start(
        mtm: MainThreadMarker,
        window: &NSWindow,
        mut report: Report,
        trace: SharedTrace,
        view: ViewOptions,
        out: &Path,
    ) -> Result<Ax, Failed> {
        let (variant, mode) = match (view.variant, view.mode) {
            (Some(variant), Some(ViewMode::Unified)) => (variant, Mode::Unified),
            (Some(variant), Some(ViewMode::Side)) => (variant, Mode::SideBySide),
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
        let fixture = super::stage::generate(&mut report, FixtureName::Modified);
        let stage = match super::stage::prepare(
            mtm,
            window,
            &mut report,
            trace,
            variant,
            mode,
            fixture,
            "ax",
        ) {
            Ok(stage) => stage,
            Err(reason) => return Err(Box::new((reason, report))),
        };
        let client = match Client::spawn(std::process::id(), out) {
            Ok(client) => client,
            Err(reason) => {
                events::uninstall();
                return Err(Box::new((reason, report)));
            }
        };
        report.push("ax_client_pid", client.pid());
        Ok(Ax {
            mtm,
            out: out.to_path_buf(),
            window: window.retain(),
            stage,
            mode,
            client: RefCell::new(client),
            phase: RefCell::new(Phase::Client {
                deadline: Instant::now() + CLIENT_DEADLINE,
            }),
            records: RefCell::new(Vec::new()),
            snapshots: RefCell::new(Vec::new()),
            expectations: RefCell::new(Vec::new()),
            report: RefCell::new(Some(report)),
        })
    }

    fn view(&self) -> &View {
        &self.stage.view
    }

    pub fn poll(&self) -> Option<Result<Report, Failed>> {
        let errors = self.stage.scene.errors();
        if !errors.is_empty() {
            return Some(Err(self.fail(format!("сцена: {}", errors.join("; ")))));
        }
        if let View::B(view) = self.view() {
            let failures = view.stats().height_failures;
            if failures > 0 {
                return Some(Err(self.fail(format!(
                    "таблица B получила {failures} высот строк при занятой модели"
                ))));
            }
        }
        let phase = std::mem::replace(&mut *self.phase.borrow_mut(), Phase::Done);
        match self.advance(phase) {
            Ok(Phase::Done) => Some(self.finish()),
            Ok(next) => {
                *self.phase.borrow_mut() = next;
                None
            }
            Err(reason) => Some(Err(self.fail(reason))),
        }
    }

    fn advance(&self, phase: Phase) -> Result<Phase, String> {
        match phase {
            Phase::Client { deadline } => match self.client.borrow().poll() {
                Some(Reply::Ready(info)) => {
                    if let Some(report) = self.report.borrow_mut().as_mut() {
                        report.push("ax_client_ready", info);
                    }
                    let posted_ns = now_ns();
                    let posted = events::press(Key::OPEN)?;
                    Ok(Phase::Settle {
                        step: Step::First,
                        posted,
                        posted_ns,
                        target_y: None,
                        deadline: Instant::now() + OPEN_DEADLINE,
                    })
                }
                Some(Reply::Failed(reason)) => Err(format!("клиент доступности: {reason}")),
                Some(Reply::Snapshot(_)) => Err("снимок клиента до запроса".to_owned()),
                None if Instant::now() < deadline => Ok(Phase::Client { deadline }),
                None => Err(format!(
                    "клиент доступности не готов за {} с",
                    CLIENT_DEADLINE.as_secs()
                )),
            },
            Phase::Settle {
                step,
                posted,
                posted_ns,
                target_y,
                deadline,
            } => {
                let (met, detail) = self.settled(step, &posted, posted_ns, target_y);
                if met {
                    let expectation = self.expectation()?;
                    self.client.borrow_mut().request(step.name())?;
                    return Ok(Phase::Snapshot {
                        step,
                        expectation: Box::new(expectation),
                        settled_ms: now_ns().saturating_sub(posted_ns) as f64 / 1e6,
                        requested: Instant::now(),
                    });
                }
                if Instant::now() < deadline {
                    return Ok(Phase::Settle {
                        step,
                        posted,
                        posted_ns,
                        target_y,
                        deadline,
                    });
                }
                Err(format!(
                    "шаг {} не завершён в срок: {detail}, видимые строки {:?}, покрыты {}, sendEvent вернулся {}",
                    step.name(),
                    self.stage.scene.visible_rows(),
                    self.stage.covered_now(),
                    returned(&posted)
                ))
            }
            Phase::Snapshot {
                step,
                expectation,
                settled_ms,
                requested,
            } => match self.client.borrow().poll() {
                Some(Reply::Snapshot(snapshot)) => {
                    if snapshot.label != step.name() {
                        return Err(format!(
                            "снимок «{}» вместо «{}»",
                            snapshot.label,
                            step.name()
                        ));
                    }
                    if let Some(error) = &snapshot.error {
                        return Err(format!("снимок {}: {error}", step.name()));
                    }
                    self.write(step, &snapshot, &expectation)?;
                    self.record(step, &snapshot, &expectation, settled_ms, requested);
                    self.snapshots.borrow_mut().push(*snapshot);
                    self.expectations.borrow_mut().push(*expectation);
                    match step.next() {
                        Some(next) => self.begin(next),
                        None => Ok(Phase::Done),
                    }
                }
                Some(Reply::Failed(reason)) => Err(format!("клиент доступности: {reason}")),
                Some(Reply::Ready(_)) => Err("повторная готовность клиента".to_owned()),
                None if requested.elapsed() < SNAPSHOT_DEADLINE => Ok(Phase::Snapshot {
                    step,
                    expectation,
                    settled_ms,
                    requested,
                }),
                None => Err(format!(
                    "снимок {} не пришёл за {} с",
                    step.name(),
                    SNAPSHOT_DEADLINE.as_secs()
                )),
            },
            Phase::Done => Ok(Phase::Done),
        }
    }

    /// Действие шага: прокрутка, щелчок и клавиши.
    fn begin(&self, step: Step) -> Result<Phase, String> {
        let scene = &self.stage.scene;
        let posted_ns = now_ns();
        let visible_height = self.view().scroll_view().contentSize().height;
        let y = self.view().viewport_y();
        let (posted, target_y) = match step {
            Step::First => (events::press(Key::OPEN)?, None),
            Step::Scroll => {
                let (_, max_y) = scene.scroll_limit().ok_or("нет индекса высот")?;
                let target = (y + SCROLL_SCREENS * visible_height)
                    .min(max_y as f64)
                    .round();
                self.view().scroll_to(target);
                (Vec::new(), Some(target))
            }
            Step::Select => {
                let mut posted = self.click()?;
                for key in [Key::V, Key::J, Key::J] {
                    posted.extend(events::press(key)?);
                }
                (posted, None)
            }
            Step::Bottom => {
                let mut posted = events::press(Key::ESCAPE)?;
                posted.extend(events::press(Key::SHIFT_G)?);
                (posted, None)
            }
            Step::Reuse => {
                let target = (y - visible_height / 2.0).max(0.0).round();
                self.view().scroll_to(target);
                (Vec::new(), Some(target))
            }
        };
        Ok(Phase::Settle {
            step,
            posted,
            posted_ns,
            target_y,
            deadline: Instant::now() + STEP_DEADLINE,
        })
    }

    /// Щелчок по видимой строке [`CLICK_ROW`]: в unified по левым 400 точкам строки, в «Рядом» по
    /// центру новой половины.
    fn click(&self) -> Result<Vec<u64>, String> {
        let visible = self.stage.scene.visible_rows();
        let row = visible.start + CLICK_ROW - 1;
        if row >= visible.end {
            return Err(format!("видимых строк меньше {CLICK_ROW}: {visible:?}"));
        }
        let area = match self.mode {
            Mode::SideBySide => self.view().half_rect(row, Side::New),
            Mode::Unified => self.view().row_rect(row).map(|area| {
                NSRect::new(
                    area.origin,
                    NSSize::new(area.size.width.min(400.0), area.size.height),
                )
            }),
        }
        .ok_or_else(|| format!("нет прямоугольника строки {row}"))?;
        let screen = self.window.convertRectToScreen(self.view().to_window(area));
        let point = events::center(screen);
        let gesture = events::mouse_down(
            self.mtm,
            NSPoint::new(point.x, point.y),
            &self.window,
            NSEventModifierFlags::empty(),
            1,
            "ax",
        )?;
        Ok(vec![gesture.down, gesture.up("ax")?])
    }

    /// Выполнено ли условие шага и что видно сейчас.
    fn settled(
        &self,
        step: Step,
        posted: &[u64],
        posted_ns: u64,
        target_y: Option<f64>,
    ) -> (bool, String) {
        let scene = &self.stage.scene;
        let y = self.view().viewport_y();
        let base = returned(posted) && self.stage.covered_now() && !scene.relayout_pending();
        match step {
            Step::First => {
                let milestones =
                    Milestones::of(posted_ns, &scene.timeline(), &self.stage.coverage.borrow());
                let shown = scene.mode();
                (
                    base && milestones.full_ms.is_some() && shown == self.mode,
                    format!("{milestones:?}, показан режим {shown:?}"),
                )
            }
            Step::Scroll | Step::Reuse => {
                let target = target_y.unwrap_or(-1.0);
                (
                    base && (y - target).abs() <= SCROLL_TOLERANCE,
                    format!("верхний край {y}, цель {target}"),
                )
            }
            Step::Select => {
                let selection = scene.selection();
                (
                    base && selection.is_some_and(|selection| selection.head != selection.tail),
                    format!("выделение {selection:?}, курсор {:?}", scene.cursor()),
                )
            }
            Step::Bottom => {
                let rows = scene.timeline().rows.unwrap_or(0);
                let visible = scene.visible_rows();
                let at_end = scene
                    .scroll_limit()
                    .is_some_and(|(_, max_y)| (y - max_y as f64).abs() <= SCROLL_TOLERANCE);
                (
                    base && at_end && visible.end == rows && scene.selection().is_none(),
                    format!("верхний край {y}, видимые {visible:?} из {rows}"),
                )
            }
        }
    }

    /// Рамка строки показа в глобальных координатах доступности.
    fn frame_of(&self, row: u32, primary_height: f64) -> Option<AxFrame> {
        let area = self.view().row_rect(row)?;
        let screen = self.window.convertRectToScreen(self.view().to_window(area));
        Some(AxFrame {
            x: screen.origin.x,
            y: primary_height - (screen.origin.y + screen.size.height),
            width: screen.size.width,
            height: screen.size.height,
        })
    }

    /// Что модель ждёт от снимка сейчас: видимые строки контейнера, их сообщения и рамки,
    /// фокус.
    fn expectation(&self) -> Result<Expectation, String> {
        let scene = &self.stage.scene;
        let visible = scene.visible_rows();
        let primary_height = NSScreen::screens(self.mtm)
            .firstObject()
            .map_or(0.0, |screen| screen.frame().size.height);
        let (items, cursor_row): (Vec<(i64, Option<u32>)>, Option<u32>) = {
            let model = scene
                .model()
                .try_borrow()
                .map_err(|_| "модель занята при снимке")?;
            let cursor_row = model.cursor_row();
            let items = match self.view() {
                View::A(_) => visible
                    .clone()
                    .map(|row| (i64::from(row), Some(row)))
                    .collect(),
                View::B(view) => {
                    let map = view.map();
                    view.visible_table_rows()
                        .map(|index| {
                            let row = match map.item(index) {
                                Some(Item::Text(row)) => Some(row),
                                Some(Item::Block(_)) | None => None,
                            };
                            (index as i64, row)
                        })
                        .collect()
                }
            };
            (items, cursor_row)
        };
        let mut rows = Vec::with_capacity(items.len());
        for (index, row) in items {
            let (ax, frame) = match row {
                Some(row) => {
                    let ax = scene
                        .model()
                        .borrow()
                        .row_ax(row)
                        .ok_or_else(|| format!("нет описания строки {row}"))?;
                    (Some(ax), self.frame_of(row, primary_height))
                }
                None => (None, None),
            };
            rows.push(ExpectedRow {
                index,
                row: ax,
                frame,
            });
        }
        let focus_index = cursor_row.and_then(|row| match self.view() {
            View::A(_) => visible.contains(&row).then_some(i64::from(row)),
            View::B(view) => {
                let index = view.map().index(Item::Text(row))?;
                view.visible_table_rows()
                    .contains(&index)
                    .then_some(index as i64)
            }
        });
        Ok(Expectation {
            rows,
            focus: focus_index.map_or(ExpectedFocus::Table, ExpectedFocus::Row),
        })
    }

    /// Снимок и ожидание шага в файл: без файла снимок не сохранён, это ошибка исполнения.
    fn write(
        &self,
        step: Step,
        snapshot: &AxSnapshot,
        expected: &Expectation,
    ) -> Result<(), String> {
        let path = self.out.join(format!("ax-{}.json", step.name()));
        let json = serde_json::to_vec(&StepFile { snapshot, expected })
            .map_err(|err| format!("снимок {}: JSON: {err}", step.name()))?;
        std::fs::write(&path, json).map_err(|err| format!("{}: {err}", path.display()))
    }

    fn record(
        &self,
        step: Step,
        snapshot: &AxSnapshot,
        expectation: &Expectation,
        settled_ms: f64,
        requested: Instant,
    ) {
        let comparison = access::compare(snapshot, expectation);
        let mut notes: BTreeMap<String, usize> = BTreeMap::new();
        for note in &snapshot.notes {
            *notes.entry(note.name.clone()).or_default() += 1;
        }
        let missing_notes = step
            .wanted_notes()
            .iter()
            .copied()
            .filter(|name| access::notes_named(snapshot, name) == 0)
            .collect();
        self.records.borrow_mut().push(StepRecord {
            step,
            settled_ms,
            snapshot_ms: requested.elapsed().as_secs_f64() * 1e3,
            rows: snapshot.rows.len(),
            focus: format!("{:?}", expectation.focus),
            comparison,
            notes,
            missing_notes,
            reuse: access::reuse(snapshot),
            held: snapshot.held.len(),
            held_released: snapshot
                .held
                .iter()
                .filter(|held| held.now.error.is_some())
                .count(),
        });
    }

    /// Журналы в итог; стенд, клиент и сцена снимаются.
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
            "ax_steps",
            serde_json::to_string(&*self.records.borrow()).unwrap_or_default(),
        );
        report.push(
            "ax_snapshots",
            serde_json::to_string(&*self.snapshots.borrow()).unwrap_or_default(),
        );
        report.push(
            "ax_expected",
            serde_json::to_string(&*self.expectations.borrow()).unwrap_or_default(),
        );
        match self.view() {
            View::A(view) => report.push(
                "ax_stats",
                serde_json::to_string(&view.ax_stats()).unwrap_or_default(),
            ),
            View::B(view) => {
                report.push(
                    "ax_stats",
                    serde_json::to_string(&view.ax_stats()).unwrap_or_default(),
                );
                report.push(
                    "table_stats",
                    serde_json::to_string(&view.stats()).unwrap_or_default(),
                );
                report.push(
                    "table_mismatches",
                    serde_json::to_string(&view.mismatches()).unwrap_or_default(),
                );
            }
        }
        drawmark::on_mark(|_| {});
        drawmark::on_declare(|_| {});
        events::clear();
        events::uninstall();
        self.stage.scene.close();
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
        let table = match self.view() {
            View::B(view) => Some(view.stats()),
            View::A(_) => None,
        };
        let a_stats = match self.view() {
            View::A(view) => Some(view.ax_stats()),
            View::B(_) => None,
        };
        let b_stats = match self.view() {
            View::B(view) => Some(view.ax_stats()),
            View::A(_) => None,
        };
        let (mut report, external) = self.close();
        if let Some(input) = external {
            return Err(Box::new((
                format!("внешний ввод во время сценария: {input}"),
                report,
            )));
        }
        let records = self.records.borrow();
        let complete = records.len() == Step::ALL.len();
        let joined = |pick: &dyn Fn(&StepRecord) -> Vec<String>| {
            records
                .iter()
                .flat_map(|record| {
                    pick(record)
                        .into_iter()
                        .map(move |error| format!("{}: {error}", record.step.name()))
                })
                .collect::<Vec<_>>()
        };
        let verdict = |errors: &[String]| {
            if complete && errors.is_empty() {
                Verdict::Pass
            } else {
                Verdict::Fail
            }
        };
        let summary = |errors: &[String], ok: String| {
            if errors.is_empty() {
                ok
            } else {
                format!(
                    "расхождений {}: {}",
                    errors.len(),
                    errors
                        .iter()
                        .take(6)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            }
        };
        let rows = joined(&|record| record.comparison.rows.clone());
        report.finding(
            "ax_rows",
            verdict(&rows),
            summary(
                &rows,
                records
                    .iter()
                    .map(|record| format!("{}: {} строк", record.step.name(), record.rows))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        );
        let focus = joined(&|record| record.comparison.focus.clone());
        report.finding(
            "ax_focus",
            verdict(&focus),
            summary(
                &focus,
                records
                    .iter()
                    .map(|record| format!("{}: {}", record.step.name(), record.focus))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        );
        let held = joined(&|record| record.comparison.held.clone());
        report.finding(
            "ax_held",
            verdict(&held),
            summary(
                &held,
                records
                    .iter()
                    .map(|record| {
                        format!(
                            "{}: удержано {}, освобождено {}",
                            record.step.name(),
                            record.held,
                            record.held_released
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        );
        let notes = joined(&|record| {
            record
                .missing_notes
                .iter()
                .map(|name| format!("нет {name}"))
                .collect()
        });
        report.finding(
            "ax_notifications",
            verdict(&notes),
            summary(
                &notes,
                records
                    .iter()
                    .map(|record| format!("{}: {:?}", record.step.name(), record.notes))
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
        );
        let reuse = records
            .iter()
            .find(|record| record.step == Step::Reuse)
            .map(|record| record.reuse);
        let reuse_text = format!(
            "строки, видимые до и после прокрутки на полэкрана: тот же элемент / другой {reuse:?}"
        );
        match a_stats {
            Some(stats) => {
                let bounded = stats.max_live <= stats.max_visible;
                let ok = reuse.is_some_and(|(same, other)| same > 0 && other == 0) && bounded;
                report.finding(
                    "ax_reuse",
                    if complete && ok {
                        Verdict::Pass
                    } else {
                        Verdict::Fail
                    },
                    format!(
                        "{reuse_text}; элементов создано {}, переиспользовано {}, отпущено {}, живых не больше {} при {} видимых строках",
                        stats.created, stats.reused, stats.dropped, stats.max_live, stats.max_visible
                    ),
                );
            }
            None => report.push(
                "ax_reuse",
                match b_stats {
                    Some(stats) => format!(
                        "{reuse_text}; элементов создано {}, переиспользовано {}, отпущено {}, живых не больше {} при {} видимых строках таблицы, видов строк не своей строки {}, запросов при занятой модели {}",
                        stats.created,
                        stats.reused,
                        stats.dropped,
                        stats.max_live,
                        stats.max_visible,
                        stats.view_mismatches,
                        stats.busy
                    ),
                    None => reuse_text,
                },
            ),
        }
        if let Some(stats) = table {
            let ok = stats.frame_mismatches == 0 && stats.estimates_off;
            report.finding(
                "table_state",
                if ok { Verdict::Pass } else { Verdict::Fail },
                format!(
                    "оценка высот выключена {}, расхождений высоты таблицы {}, высот при занятой модели {}",
                    stats.estimates_off, stats.frame_mismatches, stats.height_failures
                ),
            );
        }
        report.finding(
            "voiceover",
            Verdict::NotRun,
            "VoiceOver не наблюдается: озвучивание автоматикой не проверяется",
        );
        Ok(report)
    }
}

impl crate::scenario::Running for Ax {
    fn poll(&self) -> Option<Result<Report, Failed>> {
        Ax::poll(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_follow_the_plan_order() {
        let names: Vec<&str> = Step::ALL.iter().map(|step| step.name()).collect();
        assert_eq!(names, ["first", "scroll", "select", "bottom", "reuse"]);
        assert_eq!(Step::First.next(), Some(Step::Scroll));
        assert_eq!(Step::Reuse.next(), None);
        assert!(Step::First.wanted_notes().is_empty());
        assert!(
            Step::Select
                .wanted_notes()
                .contains(&"AXFocusedUIElementChanged")
        );
    }
}
