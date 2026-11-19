//! Сценарий `keys` с окном: фикстура `modified` генерируется на рабочем потоке, клавиша `o`
//! открывает её в виде варианта и режима, затем `j`, `k`, `Ctrl-d`, `]c`, `G`, `gg` по очереди, у
//! каждой 10 прогревочных и 100 измеренных событий (строка «Клавиши `keys`» отчёта). Перед каждым
//! событием вне замера верхний край и курсор возвращаются в подготовленную позицию — середину
//! документа, затем стенд ждёт покрытия видимых строк без перестройки, без запросов подсветки в
//! работе и без ожидающей отрисовки окна.
//!
//! Конец интервала — `draw_end_ns` первой отметки журнала покрытия после `postEvent`, поколение
//! которой не младше объявленного до возврата `sendEvent` последней клавиши. Совпадение строк
//! концом не служит: после `G` из одной позиции видимые строки каждый раз одни и те же.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::NSWindow;
use objc2_core_graphics::CGDisplayIsBuiltin;
use serde::Serialize;

use probe_diff_view::fixtures::Fixture;
use probe_diff_view::highlight::Source;
use probe_diff_view::model::state::Command;
use probe_diff_view::model::{Mode, Side};
use probe_diff_view::open::OpenFixture;

use super::calibrate_run::Failed;
use super::drawmark::{self, Declared};
use super::events::{self, Key};
use super::keys::{self, KeyName, Press};
use super::sampler;
use super::stage::Stage;
use super::watchdog::SharedTrace;
use crate::args::{Variant, ViewMode, ViewOptions};
use crate::results::Report;

/// Срок генерации фикстуры на рабочем потоке.
pub const LOAD_DEADLINE: Duration = Duration::from_secs(30);
/// Срок готовности до клавиши открытия.
pub const READY_DEADLINE: Duration = Duration::from_secs(10);
/// Срок открытия до покрытия после снимка полного разбора.
pub const OPEN_DEADLINE: Duration = Duration::from_secs(30);
/// Срок подготовленной позиции.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(2);
/// Срок клавиши: отметка нужного поколения и затем догоняющая подсветка.
pub const KEY_DEADLINE: Duration = Duration::from_secs(2);
/// Курсор подготовленной позиции — столько строк показа ниже верхней: `k` и `j` его не уводят
/// за край видимой области.
const CURSOR_BELOW_TOP: u32 = 10;

/// Событий на клавишу.
const PER_KEY: usize = keys::WARMUP + keys::MEASURED;

/// Имя последовательности стенда событий.
fn sequence(key: KeyName) -> &'static str {
    match key {
        KeyName::J => "j",
        KeyName::K => "k",
        KeyName::CtrlD => "Ctrl-d",
        KeyName::NextChange => "]c",
        KeyName::Bottom => "G",
        KeyName::Top => "gg",
    }
}

/// Номер события по порядку → клавиша и её номер.
fn slot(next: usize) -> (KeyName, usize) {
    (KeyName::ALL[next / PER_KEY], next % PER_KEY)
}

/// Подготовленная позиция: верхняя строка показа и курсор.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct Prepared {
    top_row: u32,
    top_y: u64,
    cursor_row: u32,
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
    Preparing {
        next: usize,
        deadline: Instant,
    },
    Pressed {
        next: usize,
        posted: Vec<u64>,
        posted_ns: u64,
        /// Время от установки стенда событий плюс это смещение — `CLOCK_UPTIME_RAW`.
        offset_ns: i128,
        before_generation: u64,
        deadline: Instant,
    },
    Catchup {
        next: usize,
        posted_ns: u64,
        drawn_ns: u64,
        deadline: Instant,
    },
    Done,
}

pub struct KeysRun {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    trace: SharedTrace,
    variant: Variant,
    mode: Mode,
    stage: RefCell<Option<Rc<Stage>>>,
    step: RefCell<Step>,
    prepared: Cell<Option<Prepared>>,
    declared: RefCell<Vec<Declared>>,
    presses: RefCell<Vec<Press>>,
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

/// Возврат `sendEvent` последнего события в `CLOCK_UPTIME_RAW`.
fn returned_ns(posted: &[u64], offset_ns: i128) -> Option<u64> {
    posted
        .iter()
        .map(|number| {
            events::record(*number)
                .and_then(|record| record.send_return)
                .map(|at| (offset_ns + at.as_nanos() as i128).max(0) as u64)
        })
        .try_fold(0u64, |latest, at| at.map(|at| latest.max(at)))
}

/// Поколение, объявленное до возврата `sendEvent`: наибольшее из поколения до клавиши и
/// объявлений не позже возврата.
pub fn declared_until(before: u64, declared: &[Declared], until_ns: u64) -> u64 {
    declared
        .iter()
        .filter(|declared| declared.at_ns <= until_ns)
        .map(|declared| declared.generation)
        .fold(before, u64::max)
}

/// Дисплей окна — встроенный с `maximumFramesPerSecond` 120 (строка «120 Гц `keys`»).
fn rate_120(window: &NSWindow) -> (bool, String) {
    let Some(screen) = window.screen() else {
        return (false, "окно без экрана".to_owned());
    };
    let id = screen.CGDirectDisplayID();
    let builtin = CGDisplayIsBuiltin(id);
    let fps = screen.maximumFramesPerSecond();
    (
        builtin && fps == 120,
        format!(
            "дисплей {id} «{}», встроенный {builtin}, maximumFramesPerSecond {fps}",
            screen.localizedName()
        ),
    )
}

impl KeysRun {
    pub fn start(
        mtm: MainThreadMarker,
        window: &NSWindow,
        report: Report,
        trace: SharedTrace,
        view: ViewOptions,
    ) -> Result<KeysRun, Failed> {
        let (variant, mode) = match (view.variant, view.mode) {
            (Some(variant), Some(mode)) => (variant, mode),
            _ => return Err(Box::new((format!("нет оси клавиш: {view:?}"), report))),
        };
        let Some(name) = OpenFixture::Modified.synthetic() else {
            return Err(Box::new((
                "фикстура modified не синтетическая".to_owned(),
                report,
            )));
        };
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("probe-diff-keys-load".to_owned())
            .spawn(move || {
                let _ = tx.send(probe_diff_view::fixtures::generate(name));
            });
        if let Err(err) = spawned {
            return Err(Box::new((
                format!("поток загрузки не запущен: {err}"),
                report,
            )));
        }
        Ok(KeysRun {
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
            prepared: Cell::new(None),
            declared: RefCell::new(Vec::new()),
            presses: RefCell::new(Vec::with_capacity(KeyName::ALL.len() * PER_KEY)),
            report: RefCell::new(Some(report)),
            closed: Cell::new(false),
        })
    }

    fn stage(&self) -> Result<Rc<Stage>, String> {
        self.stage
            .borrow()
            .clone()
            .ok_or_else(|| "нет стенда клавиш".to_owned())
    }

    pub fn poll(&self) -> Option<Result<Report, Failed>> {
        if let Some(stage) = self.stage.borrow().clone()
            && let Err(reason) = super::frames_run::stage_errors(&stage)
        {
            return Some(Err(self.fail(reason)));
        }
        self.declared.borrow_mut().extend(drawmark::take_declared());
        drawmark::take();
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
                    return self.prepare(0);
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
            Step::Preparing { next, deadline } => {
                let stage = self.stage()?;
                if self.settled(&stage) {
                    return self.press(next, &stage);
                }
                if Instant::now() < deadline {
                    Ok(Step::Preparing { next, deadline })
                } else {
                    let (key, index) = slot(next);
                    Err(format!(
                        "подготовленная позиция перед {} №{index} не установилась за {} с: {}",
                        key.name(),
                        PREPARE_DEADLINE.as_secs(),
                        self.state(&stage)
                    ))
                }
            }
            Step::Pressed {
                next,
                posted,
                posted_ns,
                offset_ns,
                before_generation,
                deadline,
            } => {
                let stage = self.stage()?;
                let drawn = returned_ns(&posted, offset_ns).and_then(|until_ns| {
                    let required =
                        declared_until(before_generation, &self.declared.borrow(), until_ns);
                    stage
                        .coverage
                        .borrow()
                        .iter()
                        .find(|coverage| {
                            coverage.at_ns >= posted_ns && coverage.generation >= required
                        })
                        .map(|coverage| coverage.at_ns)
                });
                match drawn {
                    Some(drawn_ns) => Ok(Step::Catchup {
                        next,
                        posted_ns,
                        drawn_ns,
                        deadline,
                    }),
                    None if Instant::now() < deadline => Ok(Step::Pressed {
                        next,
                        posted,
                        posted_ns,
                        offset_ns,
                        before_generation,
                        deadline,
                    }),
                    None => {
                        let (key, index) = slot(next);
                        Err(format!(
                            "клавиша {} №{index}: нет отметки drawRect: нужного поколения за {} с: {}",
                            key.name(),
                            KEY_DEADLINE.as_secs(),
                            self.state(&stage)
                        ))
                    }
                }
            }
            Step::Catchup {
                next,
                posted_ns,
                drawn_ns,
                deadline,
            } => {
                let stage = self.stage()?;
                if !self.settled(&stage) {
                    if Instant::now() < deadline {
                        return Ok(Step::Catchup {
                            next,
                            posted_ns,
                            drawn_ns,
                            deadline,
                        });
                    }
                    let (key, index) = slot(next);
                    return Err(format!(
                        "клавиша {} №{index}: подсветка не догнала за {} с: {}",
                        key.name(),
                        KEY_DEADLINE.as_secs(),
                        self.state(&stage)
                    ));
                }
                let highlight_ns = self.highlight_ns(&stage, posted_ns);
                let (key, index) = slot(next);
                self.presses.borrow_mut().push(Press {
                    key,
                    index,
                    posted_ns,
                    drawn_ns: Some(drawn_ns),
                    highlight_ns,
                });
                if next + 1 == KeyName::ALL.len() * PER_KEY {
                    Ok(Step::Done)
                } else {
                    self.prepare(next + 1)
                }
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
            "keys",
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
             окну нужна отрисовка {}, верх {:.1}, курсор {:?}",
            visible.start,
            visible.end,
            stage.covered_now(),
            stage.census_now(),
            stage.scene.relayout_pending(),
            stage.scene.highlight_settled(),
            self.window.viewsNeedDisplay(),
            stage.view.viewport_y(),
            stage.scene.cursor()
        )
    }

    /// Подготовленная позиция — середина документа; считается один раз после открытия.
    fn prepared(&self, stage: &Stage) -> Result<Prepared, String> {
        if let Some(prepared) = self.prepared.get() {
            return Ok(prepared);
        }
        let visible = stage.scene.visible_rows();
        if visible.len() <= 2 * CURSOR_BELOW_TOP as usize {
            return Err(format!(
                "видимых строк {} — не больше {}: курсор подготовленной позиции не помещается",
                visible.len(),
                2 * CURSOR_BELOW_TOP
            ));
        }
        let model = stage.scene.model().borrow();
        let shown = model
            .shown()
            .ok_or_else(|| "нет показанного diff".to_owned())?;
        let top_row = shown.heights.len() / 2;
        let top_y = shown
            .heights
            .top(top_row)
            .ok_or_else(|| format!("нет координаты строки {top_row}"))?;
        let prepared = Prepared {
            top_row,
            top_y,
            cursor_row: top_row + CURSOR_BELOW_TOP,
        };
        self.prepared.set(Some(prepared));
        Ok(prepared)
    }

    /// Верхний край и курсор в подготовленную позицию вне замера.
    fn prepare(&self, next: usize) -> Result<Step, String> {
        let stage = self.stage()?;
        let prepared = self.prepared(&stage)?;
        stage.scene.command(Command::ScrollTo(prepared.top_y));
        stage.scene.command(Command::Click {
            row: prepared.cursor_row,
            side: Side::New,
        });
        Ok(Step::Preparing {
            next,
            deadline: Instant::now() + PREPARE_DEADLINE,
        })
    }

    fn press(&self, next: usize, stage: &Stage) -> Result<Step, String> {
        let prepared = self.prepared(stage)?;
        let top = stage.view.viewport_y();
        if (top - prepared.top_y as f64).abs() > 1.0 {
            return Err(format!(
                "верх {top:.1} не в подготовленной позиции {}",
                prepared.top_y
            ));
        }
        let (key, _) = slot(next);
        let before_generation = stage.ledger.borrow().latest();
        self.declared.borrow_mut().clear();
        let posted_ns = now_ns();
        let elapsed = events::elapsed().ok_or_else(|| "стенд событий не установлен".to_owned())?;
        let offset_ns = posted_ns as i128 - elapsed.as_nanos() as i128;
        let posted = events::press_sequence(sequence(key))?;
        Ok(Step::Pressed {
            next,
            posted,
            posted_ns,
            offset_ns,
            before_generation,
            deadline: Instant::now() + KEY_DEADLINE,
        })
    }

    /// Конец отрисовки догоняющего снимка подсветки: первая запись журнала покрытия не раньше
    /// последнего снимка, применённого после `postEvent`, и не младше его поколения.
    fn highlight_ns(&self, stage: &Stage, posted_ns: u64) -> Option<u64> {
        let timeline = stage.scene.timeline();
        let applied = timeline
            .snapshots
            .iter()
            .rev()
            .find(|applied| applied.at_ns >= posted_ns)?;
        stage
            .coverage
            .borrow()
            .iter()
            .find(|coverage| {
                coverage.at_ns >= applied.at_ns && coverage.generation >= applied.generation
            })
            .map(|coverage| coverage.at_ns)
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
        let (mut report, external) = self.close();
        report.push(
            "keys_presses",
            serde_json::to_string(&*self.presses.borrow()).unwrap_or_default(),
        );
        let reason = match external {
            Some(input) => format!("{reason}; внешний ввод во время сценария: {input}"),
            None => reason,
        };
        Box::new((reason, report))
    }

    fn finish(&self) -> Result<Report, Failed> {
        let (rate, display) = rate_120(&self.window);
        let (mut report, external) = self.close();
        let presses = self.presses.borrow();
        report.push(
            "keys_presses",
            serde_json::to_string(&*presses).unwrap_or_default(),
        );
        report.push("keys_display", &display);
        if let Some(prepared) = self.prepared.get() {
            report.push(
                "keys_prepared",
                serde_json::to_string(&prepared).unwrap_or_default(),
            );
        }
        if let Some(input) = external {
            return Err(Box::new((
                format!("внешний ввод во время сценария: {input}"),
                report,
            )));
        }
        let mut summaries = Vec::with_capacity(KeyName::ALL.len());
        for key in KeyName::ALL {
            match keys::summarize(key, &presses) {
                Ok(summary) => summaries.push(summary),
                Err(reason) => return Err(Box::new((reason, report))),
            }
        }
        let (verdict, reason) = keys::judge(&summaries, rate);
        report.finding("keys_budget", verdict, format!("{reason}; {display}"));
        for summary in &summaries {
            report.metric(summary.key.metric(), "ms", summary.samples_ms.clone());
            if let Some(highlight) = &summary.highlight {
                report.push(
                    &format!("{}_highlight", summary.key.metric()),
                    serde_json::to_string(highlight).unwrap_or_default(),
                );
            }
        }
        report.push(
            "keys",
            serde_json::to_string(&summaries).unwrap_or_default(),
        );
        Ok(report)
    }
}

impl crate::scenario::Running for KeysRun {
    fn poll(&self) -> Option<Result<Report, Failed>> {
        KeysRun::poll(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(generation: u64, at_ns: u64) -> Declared {
        Declared {
            generation,
            rows: None,
            at_ns,
        }
    }

    #[test]
    fn required_generation_ignores_declarations_after_send_return() {
        let journal = [declared(7, 100), declared(8, 150), declared(9, 300)];
        assert_eq!(declared_until(6, &journal, 200), 8);
        assert_eq!(declared_until(6, &journal, 50), 6);
        assert_eq!(declared_until(10, &journal, 400), 10);
    }

    #[test]
    fn slots_walk_every_key_in_order() {
        assert_eq!(slot(0), (KeyName::J, 0));
        assert_eq!(slot(PER_KEY - 1), (KeyName::J, PER_KEY - 1));
        assert_eq!(slot(PER_KEY), (KeyName::K, 0));
        assert_eq!(slot(6 * PER_KEY - 1), (KeyName::Top, PER_KEY - 1));
        for key in KeyName::ALL {
            assert!(events::sequence(sequence(key)).is_some(), "{key:?}");
        }
    }
}
