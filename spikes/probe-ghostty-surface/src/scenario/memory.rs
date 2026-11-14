//! Сценарий `memory` (H10): память на поверхность и цена скрытия по `ri_phys_footprint` процесса и
//! потомков из общего сэмплера.
//!
//! Вариант `bare` — пустое окно без libghostty. Вариант `base`:
//! - окно после `ghostty_init` до `ghostty_app_new`, затем приложение без поверхностей — `M(0)`;
//! - 1, 2, 4, 8 одинаковых поверхностей с фикстурой в равных ячейках вида стенда: одна сетка,
//!   шрифт и масштаб, иначе ошибка предусловия; затем скрыты (скрыт общий родитель, модель
//!   видимости подаёт `false`), снова видимы и закрыты;
//! - ещё два цикла 0 → 8 → 0 сразу по восемь: остаток после циклов отделяет разовые кэши от роста
//!   за цикл.
//!
//! Вариант `scrollback` — заполненный scrollback у двух поверхностей в свежем процессе: в процессе
//! после циклов страницы терминала ложились бы в память, которую процесс удержал от прежних
//! поверхностей, и прирост scrollback был бы занижен.
//!
//! Каждое состояние — серия показаний до устоявшейся памяти (`sampler::settle`). Единицы — МиБ.
//! Бюджета на терминал нет, выводы — наблюдения. Бюджет 150 МБ относится к GUI без терминалов и с
//! поверхностями не сравнивается.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSView};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use serde::Serialize;

use super::input::focused;
use super::surface_env::{self, State};
use super::visibility::{Shield, shield_step};
use super::{Poll, Report, Scenario, Stand, Step, events, poll, run_steps, window};
use crate::fixture::Command;
use crate::ghostty::config::ColorScheme;
use crate::ghostty::runtime::{self, Created};
use crate::ghostty::surface::{GridSize, Phase, SurfaceId};
use crate::results::{Requirement, Verdict};
use crate::sampler::{self, Reply, Sampler, Settled, Unreadable};
use crate::view::SurfaceView;
use crate::visibility::{self, Workarounds};

/// Пустое окно без libghostty.
pub const BARE: &str = "bare";
pub const BASE: &str = "base";
pub const SCROLLBACK_VARIANT: &str = "scrollback";
pub const VARIANTS: [&str; 3] = [BARE, BASE, SCROLLBACK_VARIANT];
/// Ряд `M(N)` первого цикла.
const COUNTS: [usize; 4] = [1, 2, 4, 8];
const MAX: usize = 8;
/// Циклов 0 → 8 → 0, включая первый с рядом `M(N)`.
const CYCLES: usize = 3;
const SCROLLBACK_SURFACES: usize = 2;
// Остаток после циклов отделяет разовые кэши от роста за цикл только при двух циклах и больше.
const _: () = assert!(CYCLES >= 2);
/// 40 000 строк по 100 столбцов на поверхность — больше `scrollback-limit-bytes` конфигурации.
const SCROLLBACK: Command = Command::Scrollback {
    lines: 40_000,
    columns: 100,
};
/// Ячейка поверхности: четыре в ряд, два ряда в окне стенда 1180 × 720.
const SLOT: NSSize = NSSize::new(280.0, 340.0);
const SLOT_GAP: f64 = 8.0;
const SLOT_COLUMNS: usize = 4;
/// Имя процесса фикстуры по `proc_name`: исполняемый файл пробы.
const FIXTURE_PROGRAM: &str = "probe-ghostty";
const FOCUS_TIMEOUT: Duration = Duration::from_secs(5);
const SIZE_TIMEOUT: Duration = Duration::from_secs(3);
const ACT_TIMEOUT: Duration = Duration::from_secs(5);
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const STATE_TIMEOUT: Duration = Duration::from_secs(8);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(20);
const FILL_TIMEOUT: Duration = Duration::from_secs(40);
/// Серия упирается в `SETTLE_CAP_NS`; шаг ждёт дольше на запас.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(12);

const EMPTY: &str = "empty";
const INIT: &str = "init";
const APP: &str = "app";
const VISIBLE: &str = "visible";
const HIDDEN: &str = "hidden";
const SHOWN: &str = "shown";
const CLOSED: &str = "closed";
const SCROLLBACK_BEFORE: &str = "scrollback-before";
const SCROLLBACK_FILLED: &str = "scrollback-filled";
const SCROLLBACK_CLOSED: &str = "scrollback-closed";

#[derive(Clone, Debug, Serialize)]
struct ChildBrief {
    pid: i32,
    ppid: i32,
    program: String,
    mib: f64,
}

/// Устоявшееся состояние памяти.
#[derive(Clone, Debug, Serialize)]
struct MemState {
    label: &'static str,
    /// Номер цикла; 0 — до поверхностей.
    cycle: usize,
    surfaces: usize,
    process_bytes: u64,
    children_bytes: u64,
    process_mib: f64,
    children_mib: f64,
    /// Потомков с именем исполняемого файла пробы: фикстур.
    fixtures: usize,
    children: Vec<ChildBrief>,
    unreadable: Vec<Unreadable>,
    settled: bool,
    readings: usize,
    last_change_ms: Option<f64>,
    at_ms: f64,
    trace: Vec<[f64; 3]>,
}

impl MemState {
    fn new(label: &'static str, cycle: usize, surfaces: usize, settled: Settled) -> MemState {
        let last = &settled.last;
        let children: Vec<ChildBrief> = last
            .children
            .iter()
            .map(|child| ChildBrief {
                pid: child.pid,
                ppid: child.ppid,
                program: child.program.clone(),
                mib: sampler::mib(child.footprint as f64),
            })
            .collect();
        MemState {
            label,
            cycle,
            surfaces,
            process_bytes: last.process.footprint,
            children_bytes: last.children_footprint(),
            process_mib: sampler::mib(last.process.footprint as f64),
            children_mib: sampler::mib(last.children_footprint() as f64),
            fixtures: children
                .iter()
                .filter(|child| child.program.starts_with(FIXTURE_PROGRAM))
                .count(),
            children,
            unreadable: last.unreadable.clone(),
            settled: settled.settled,
            readings: settled.readings,
            last_change_ms: settled.last_change_ms,
            at_ms: events::elapsed_ms().unwrap_or(0.0),
            trace: settled.trace,
        }
    }
}

struct Meter {
    sampler: Sampler,
    states: Vec<MemState>,
}

impl Meter {
    fn find(&self, label: &str, cycle: usize, surfaces: usize) -> Option<&MemState> {
        self.states.iter().find(|state| {
            state.label == label && state.cycle == cycle && state.surfaces == surfaces
        })
    }

    fn bytes(&self, label: &str, cycle: usize, surfaces: usize) -> Option<u64> {
        self.find(label, cycle, surfaces)
            .map(|state| state.process_bytes)
    }
}

type SharedMeter = Rc<RefCell<Meter>>;

/// Поверхность с фикстурой в ячейке.
struct Fx {
    view: Retained<SurfaceView>,
    id: SurfaceId,
    nonce: String,
    pid: Option<i32>,
}

struct Mem {
    env: State,
    container: Retained<NSView>,
    live: Vec<Fx>,
    /// Все виды поверхностей сценария: ни один не должен держать ручку после освобождения.
    views: Vec<Retained<SurfaceView>>,
    suffix: usize,
    grid: Option<GridSize>,
    shield: Option<Shield>,
    /// Заполнение scrollback: индекс поверхности, номера команд, nonce последней строки.
    fill: Vec<(usize, Vec<u64>, String)>,
}

type Shared = Rc<RefCell<Mem>>;

fn slot_frame(slot: usize) -> NSRect {
    let (column, row) = (slot % SLOT_COLUMNS, slot / SLOT_COLUMNS);
    NSRect::new(
        NSPoint::new(
            SLOT_GAP + column as f64 * (SLOT.width + SLOT_GAP),
            SLOT_GAP + row as f64 * (SLOT.height + SLOT_GAP),
        ),
        SLOT,
    )
}

fn mib(bytes: f64) -> f64 {
    sampler::mib(bytes)
}

fn signed_mib(from: u64, to: u64) -> f64 {
    mib(to as f64 - from as f64)
}

impl Mem {
    /// Создаёт первую поверхность, которую зарегистрировала `prepare_in`.
    fn adopt_first(&mut self) -> Result<(), String> {
        create(self.env.id)?;
        self.live.push(Fx {
            view: self.env.view.clone(),
            id: self.env.id,
            nonce: self.env.fixture_nonce.clone(),
            pid: None,
        });
        Ok(())
    }

    /// Добавляет поверхности в свободные ячейки, пока их не станет `count`.
    fn grow_to(&mut self, stand: &Stand, count: usize) -> Result<(), String> {
        while self.live.len() < count {
            let slot = self.live.len();
            let view = SurfaceView::new(stand.view.mtm(), slot_frame(slot));
            self.container.addSubview(&view);
            self.views.push(view.clone());
            self.suffix += 1;
            let extra = self
                .env
                .add_fixture(stand, &format!("m{}", self.suffix), view)?;
            create(extra.id)?;
            self.live.push(Fx {
                view: extra.view,
                id: extra.id,
                nonce: extra.fixture_nonce,
                pid: None,
            });
        }
        events::note(format!("поверхностей с фикстурой {}", self.live.len()));
        Ok(())
    }

    /// Все фикстуры готовы, у каждого слоя есть кадр, модель подала видимость.
    fn ready(&mut self) -> Poll<()> {
        self.env.hub.poll();
        let mut missing = Vec::new();
        for (index, fx) in self.live.iter_mut().enumerate() {
            if fx.pid.is_none() {
                fx.pid = self
                    .env
                    .hub
                    .by_nonce(&fx.nonce)
                    .and_then(|conn| conn.ready.as_ref().map(|ready| ready.pid));
            }
            let contents = fx.view.has_contents();
            let fed = fx.view.visibility_fed();
            if fx.pid.is_none() || !contents || fed != Some((true, false)) {
                missing.push(format!(
                    "#{index}: фикстура {:?}, кадр {contents}, подача {fed:?}, фаза {:?}",
                    fx.pid,
                    runtime::phase(fx.id).map(Phase::name)
                ));
            }
        }
        poll(missing.is_empty(), || missing.join("; "))
    }

    /// Сетки всех живых поверхностей совпадают между собой и с прежними.
    fn check_grid(&mut self) -> Result<(), String> {
        let sizes: Vec<GridSize> = self
            .live
            .iter()
            .map(|fx| {
                runtime::read(fx.id)
                    .map(|reading| reading.size)
                    .ok_or("поверхность не живая после готовности".to_owned())
            })
            .collect::<Result<_, _>>()?;
        let expected = self.grid.or_else(|| sizes.first().copied());
        if let Some(expected) = expected {
            if let Some(other) = sizes.iter().find(|size| **size != expected) {
                return Err(format!(
                    "предусловие: сетки поверхностей различаются: {expected:?} и {other:?}"
                ));
            }
            self.grid = Some(expected);
        }
        Ok(())
    }

    /// Последняя подача модели видимости у всех живых поверхностей равна `visible`.
    fn fed_all(&self, visible: bool) -> Poll<()> {
        let fed: Vec<Option<(bool, bool)>> = self
            .live
            .iter()
            .map(|fx| fx.view.visibility_fed())
            .collect();
        poll(fed.iter().all(|fed| *fed == Some((visible, false))), || {
            format!("подачи {fed:?}, ожидается {visible}")
        })
    }

    fn close_all(&mut self) -> Result<(), String> {
        for fx in &self.live {
            runtime::request_close(fx.id)?;
        }
        Ok(())
    }

    /// Все поверхности освобождены, сокеты фикстур закрыты, процессов нет.
    fn gone_all(&mut self) -> Poll<()> {
        self.env.hub.poll();
        let mut pending = Vec::new();
        for (index, fx) in self.live.iter().enumerate() {
            let phase = runtime::phase(fx.id);
            let closed = self
                .env
                .hub
                .by_nonce(&fx.nonce)
                .is_some_and(|conn| conn.closed);
            let alive = fx.pid.is_some_and(surface_env::process_alive);
            if phase != Some(Phase::Freed) || !closed || alive {
                pending.push(format!(
                    "#{index}: фаза {:?}, сокет закрыт {closed}, процесс жив {alive}",
                    phase.map(Phase::name)
                ));
            }
        }
        poll(pending.is_empty(), || pending.join("; "))
    }

    fn detach_closed(&mut self) {
        for fx in self.live.drain(..) {
            fx.view.removeFromSuperview();
        }
    }

    fn start_fill(&mut self, run_nonce: &str) -> Result<(), String> {
        self.fill.clear();
        for (index, fx) in self.live.iter().enumerate() {
            let line = format!("{run_nonce}-fill-{index}");
            let commands = vec![
                self.env.hub.send(&fx.nonce, SCROLLBACK)?,
                self.env.hub.send(
                    &fx.nonce,
                    Command::Print {
                        text: format!("\r\n{line}\x1b[K"),
                    },
                )?,
            ];
            self.fill.push((index, commands, line));
        }
        Ok(())
    }

    /// `Ok(true)` — фикстуры подтвердили команды и последняя строка видна в модели терминала.
    fn filled(&mut self) -> Result<bool, String> {
        self.env.hub.poll();
        for (index, commands, line) in &self.fill {
            let fx = self.live.get(*index).ok_or("поверхности нет")?;
            let Some(conn) = self.env.hub.by_nonce(&fx.nonce) else {
                return Ok(false);
            };
            for command in commands {
                match conn.ack(*command) {
                    None => return Ok(false),
                    Some((_, Some(error))) => {
                        return Err(format!("фикстура не выполнила команду {command}: {error}"));
                    }
                    Some((_, None)) => {}
                }
            }
            if !runtime::viewport_text(fx.id).is_some_and(|text| text.contains(line.as_str())) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub(super) fn create(id: SurfaceId) -> Result<(), String> {
    match runtime::create(id)? {
        Created::Live { key, new_ms } => {
            events::note(format!("поверхность {key} создана за {new_ms:.1} мс"));
            Ok(())
        }
        Created::Failed { key, failure, .. } => Err(format!(
            "ghostty_surface_new вернул NULL для {key}: {failure:?}"
        )),
        Created::Waiting(decision) => Err(format!("реестр не разрешил создание: {decision:?}")),
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Memory);
    let variant = stand.prelude.variant.clone();
    report.push("variant", &variant);
    if !VARIANTS.contains(&variant.as_str()) {
        let reason = format!("предусловие: вариант {variant}, ожидается один из {VARIANTS:?}");
        return stand.conclude(report, Err(reason));
    }
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let (bundle, problems) = window::verify_bundle();
    report.bundle = Some(bundle);
    if !problems.is_empty() {
        return stand.conclude(
            report,
            Err(format!("не бандл пробы: {}", problems.join("; "))),
        );
    }
    report.push(
        "memory_note",
        "ri_phys_footprint из proc_pid_rusage, потомки (обёртка, оболочка, фикстура) отдельно; МиБ = 2^20 байт; каждое состояние — серия показаний с шагом 100 мс до устоявшейся памяти (окно 1,5 с, порог 1 МиБ, предел 6 с); бюджет 150 МБ относится к GUI без терминалов и с поверхностями не сравнивается",
    );
    let sampler = match Sampler::start(|| {}) {
        Ok(sampler) => sampler,
        Err(reason) => return stand.conclude(report, Err(reason)),
    };
    let meter = Rc::new(RefCell::new(Meter {
        sampler,
        states: Vec::new(),
    }));
    let first_label = if variant == BARE {
        EMPTY
    } else {
        // Модель видимости включается до первой поверхности.
        visibility::enable(Workarounds::default());
        INIT
    };
    let shown = Rc::clone(&stand);
    let steps = vec![
        Step::new(
            "окно на экране, приложение активно",
            FOCUS_TIMEOUT,
            move || {
                poll(shown.window.isVisible() && shown.app.isActive(), || {
                    events::focus_state(&shown.app)
                })
            },
            || Ok(()),
        ),
        settle_step(&meter, first_label, 0, 0),
    ];
    run_steps(steps, move |result| {
        if let Err(failure) = result {
            push_states(&meter.borrow(), &mut report);
            return stand.conclude(report, Err(failure.describe()));
        }
        if variant == BARE {
            judge_bare(&meter.borrow(), &mut report);
            push_states(&meter.borrow(), &mut report);
            return stand.conclude(report, Ok(()));
        }
        start_surfaces(stand, report, meter, variant == SCROLLBACK_VARIANT);
    });
}

/// После замера окна с `ghostty_init`: приложение, вид первой поверхности и шаги циклов или
/// заполнения scrollback.
fn start_surfaces(stand: Rc<Stand>, mut report: Report, meter: SharedMeter, scrollback: bool) {
    let container = NSView::new(stand.view.mtm());
    container.setFrame(stand.view.bounds());
    container.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    stand.view.addSubview(&container);
    let env = surface_env::prepare_in(&stand, &mut report, ColorScheme::Light, |view| {
        view.setFrame(slot_frame(0));
        container.addSubview(view);
        Ok(())
    });
    let env = match env {
        Ok(env) => env,
        Err(reason) => {
            push_states(&meter.borrow(), &mut report);
            return surface_env::finish(&stand, report, Err(reason), None);
        }
    };
    let first_view = env.view.clone();
    let mem = Rc::new(RefCell::new(Mem {
        env,
        container,
        live: Vec::new(),
        views: vec![first_view],
        suffix: 0,
        grid: None,
        shield: None,
        fill: Vec::new(),
    }));
    let steps = if scrollback {
        scrollback_steps(&stand, &mem, &meter)
    } else {
        cycle_steps(&stand, &mem, &meter)
    };
    run_steps(steps, move |result| {
        let verdict = match result {
            Ok(()) => {
                if scrollback {
                    judge_scrollback(&meter.borrow(), &mut report);
                } else {
                    judge(&meter.borrow(), &mut report);
                }
                Ok(())
            }
            Err(failure) => Err(failure.describe()),
        };
        let views = {
            let mem = mem.borrow();
            report.push_json("memory_grid", &mem.grid);
            report.push_json("memory_shield", &mem.shield);
            report.push("occlusion_masked", visibility::occlusion_masked());
            mem.views.clone()
        };
        push_states(&meter.borrow(), &mut report);
        surface_env::finish_views(&stand, report, verdict, views);
    });
}

fn push_states(meter: &Meter, report: &mut Report) {
    report.push_json("memory_states", &meter.states);
    let unsettled: Vec<String> = meter
        .states
        .iter()
        .filter(|state| !state.settled)
        .map(|state| format!("{} ×{} цикл {}", state.label, state.surfaces, state.cycle))
        .collect();
    report.push_json("memory_unsettled", &unsettled);
}

/// Серия показаний до устоявшейся памяти и запись состояния.
fn settle_step(meter: &SharedMeter, label: &'static str, cycle: usize, surfaces: usize) -> Step {
    let (asking, storing) = (Rc::clone(meter), Rc::clone(meter));
    let slot: Rc<RefCell<Option<Result<Settled, String>>>> = Rc::default();
    let filling = Rc::clone(&slot);
    let mut requested = false;
    Step::new(
        "устоявшийся замер памяти",
        SETTLE_TIMEOUT,
        move || {
            let mut meter = asking.borrow_mut();
            if !requested {
                requested = true;
                if let Err(reason) = meter.sampler.settle() {
                    *filling.borrow_mut() = Some(Err(format!("сэмплер: {reason}")));
                    return Poll::Ready(());
                }
            }
            let result = match meter.sampler.take() {
                None => {
                    return Poll::Pending(format!(
                        "серия «{label}» ×{surfaces} цикла {cycle} идёт"
                    ));
                }
                Some(Reply::Settled(settled)) => Ok(settled),
                Some(Reply::Failed(reason)) => Err(format!("сэмплер: {reason}")),
                Some(_) => Err("сэмплер: ответ не на серию".to_owned()),
            };
            *filling.borrow_mut() = Some(result);
            Poll::Ready(())
        },
        move || {
            let settled = slot.borrow_mut().take().ok_or("замера нет".to_owned())??;
            let state = MemState::new(label, cycle, surfaces, settled);
            events::note(format!(
                "память {label} ×{surfaces} цикл {cycle}: процесс {:.1} МиБ, потомки {:.1} МиБ, устоялась {}",
                state.process_mib, state.children_mib, state.settled
            ));
            storing.borrow_mut().states.push(state);
            Ok(())
        },
    )
}

/// Фокус, щит, `M(0)` и создание первой поверхности.
fn first_steps(stand: &Rc<Stand>, mem: &Shared, meter: &SharedMeter) -> Vec<Step> {
    let mut steps = Vec::new();
    let (focus, focus_mem) = (Rc::clone(stand), Rc::clone(mem));
    steps.push(Step::new(
        "активность, key window и фокус вида поверхности",
        FOCUS_TIMEOUT,
        move || focused(&focus, &focus_mem.borrow().env.view),
        || Ok(()),
    ));
    let masking = Rc::clone(mem);
    steps.push(shield_step(stand, move |shield| {
        masking.borrow_mut().shield = Some(shield);
    }));
    steps.push(settle_step(meter, APP, 0, 0));
    let (sized, creating) = (Rc::clone(mem), Rc::clone(mem));
    steps.push(Step::new(
        "ненулевой размер вида в пикселях",
        SIZE_TIMEOUT,
        move || {
            let pixels = sized.borrow().env.view.pixel_size();
            poll(pixels.0 > 0 && pixels.1 > 0, || {
                format!("размер вида в пикселях {pixels:?}")
            })
        },
        move || creating.borrow_mut().adopt_first(),
    ));
    steps
}

fn cycle_steps(stand: &Rc<Stand>, mem: &Shared, meter: &SharedMeter) -> Vec<Step> {
    let mut steps = first_steps(stand, mem, meter);
    for count in COUNTS {
        if count > 1 {
            grow_step(&mut steps, stand, mem, count);
        }
        ready_step(&mut steps, mem);
        steps.push(settle_step(meter, VISIBLE, 1, count));
    }
    visibility_steps(&mut steps, stand, mem, meter, 1);
    close_steps(&mut steps, mem, meter, 1, CLOSED);
    for cycle in 2..=CYCLES {
        grow_step(&mut steps, stand, mem, MAX);
        ready_step(&mut steps, mem);
        steps.push(settle_step(meter, VISIBLE, cycle, MAX));
        visibility_steps(&mut steps, stand, mem, meter, cycle);
        close_steps(&mut steps, mem, meter, cycle, CLOSED);
    }
    steps
}

fn scrollback_steps(stand: &Rc<Stand>, mem: &Shared, meter: &SharedMeter) -> Vec<Step> {
    let mut steps = first_steps(stand, mem, meter);
    grow_step(&mut steps, stand, mem, SCROLLBACK_SURFACES);
    ready_step(&mut steps, mem);
    steps.push(settle_step(
        meter,
        SCROLLBACK_BEFORE,
        1,
        SCROLLBACK_SURFACES,
    ));
    let (filling, nonce) = (Rc::clone(mem), stand.prelude.nonce.clone());
    steps.push(Step::new(
        "заполнение scrollback",
        ACT_TIMEOUT,
        || Poll::Ready(()),
        move || filling.borrow_mut().start_fill(&nonce),
    ));
    let error: Rc<RefCell<Option<String>>> = Rc::default();
    let (checking, failed) = (Rc::clone(mem), Rc::clone(&error));
    steps.push(Step::new(
        "scrollback заполнен: квитанции и последняя строка в модели",
        FILL_TIMEOUT,
        move || match checking.borrow_mut().filled() {
            Ok(true) => Poll::Ready(()),
            Ok(false) => Poll::Pending("заполнение идёт".to_owned()),
            Err(reason) => {
                *failed.borrow_mut() = Some(reason);
                Poll::Ready(())
            }
        },
        move || error.borrow_mut().take().map_or(Ok(()), Err),
    ));
    steps.push(settle_step(
        meter,
        SCROLLBACK_FILLED,
        1,
        SCROLLBACK_SURFACES,
    ));
    close_steps(&mut steps, mem, meter, 1, SCROLLBACK_CLOSED);
    steps
}

fn grow_step(steps: &mut Vec<Step>, stand: &Rc<Stand>, mem: &Shared, count: usize) {
    let (growing, stand) = (Rc::clone(mem), Rc::clone(stand));
    steps.push(Step::new(
        "новые поверхности с фикстурой",
        ACT_TIMEOUT,
        || Poll::Ready(()),
        move || growing.borrow_mut().grow_to(&stand, count),
    ));
}

fn ready_step(steps: &mut Vec<Step>, mem: &Shared) {
    let (waiting, checking) = (Rc::clone(mem), Rc::clone(mem));
    steps.push(Step::new(
        "готовность фикстур, первый кадр и подача видимости",
        READY_TIMEOUT,
        move || waiting.borrow_mut().ready(),
        move || checking.borrow_mut().check_grid(),
    ));
}

fn visibility_steps(
    steps: &mut Vec<Step>,
    stand: &Rc<Stand>,
    mem: &Shared,
    meter: &SharedMeter,
    cycle: usize,
) {
    let hiding = Rc::clone(mem);
    steps.push(Step::new(
        "скрытие общего родителя поверхностей",
        ACT_TIMEOUT,
        || Poll::Ready(()),
        move || {
            hiding.borrow().container.setHidden(true);
            Ok(())
        },
    ));
    let hidden = Rc::clone(mem);
    steps.push(Step::new(
        "модель подала скрытие всем поверхностям",
        STATE_TIMEOUT,
        move || hidden.borrow().fed_all(false),
        || Ok(()),
    ));
    steps.push(settle_step(meter, HIDDEN, cycle, MAX));
    let (showing, stand) = (Rc::clone(mem), Rc::clone(stand));
    steps.push(Step::new(
        "показ общего родителя поверхностей",
        ACT_TIMEOUT,
        || Poll::Ready(()),
        move || {
            let mem = showing.borrow();
            mem.container.setHidden(false);
            if let Some(fx) = mem.live.first() {
                stand.window.makeFirstResponder(Some(&fx.view));
            }
            Ok(())
        },
    ));
    let shown = Rc::clone(mem);
    steps.push(Step::new(
        "модель подала показ всем поверхностям",
        STATE_TIMEOUT,
        move || shown.borrow().fed_all(true),
        || Ok(()),
    ));
    steps.push(settle_step(meter, SHOWN, cycle, MAX));
}

fn close_steps(
    steps: &mut Vec<Step>,
    mem: &Shared,
    meter: &SharedMeter,
    cycle: usize,
    label: &'static str,
) {
    let closing = Rc::clone(mem);
    steps.push(Step::new(
        "намерение закрыть все поверхности",
        ACT_TIMEOUT,
        || Poll::Ready(()),
        move || closing.borrow_mut().close_all(),
    ));
    let (gone, detaching) = (Rc::clone(mem), Rc::clone(mem));
    steps.push(Step::new(
        "освобождение поверхностей и выход фикстур",
        CLOSE_TIMEOUT,
        move || gone.borrow_mut().gone_all(),
        move || {
            detaching.borrow_mut().detach_closed();
            Ok(())
        },
    ));
    steps.push(settle_step(meter, label, cycle, 0));
}

fn observed(report: &mut Report, name: &str, reason: String) {
    report.finding(name, Requirement::Required, Verdict::Observed, reason);
}

fn judge_bare(meter: &Meter, report: &mut Report) {
    let Some(state) = meter.find(EMPTY, 0, 0) else {
        return;
    };
    report.metric("memory.empty_window", "MiB", vec![state.process_mib]);
    observed(
        report,
        "h10-empty-window",
        format!(
            "пустое окно без libghostty: {:.1} МиБ ({:.1} МБ), потомков {}{}",
            state.process_mib,
            sampler::mb(state.process_bytes as f64),
            state.children.len(),
            settle_note(state)
        ),
    );
}

fn settle_note(state: &MemState) -> &'static str {
    if state.settled {
        ""
    } else {
        "; память не устоялась за предел серии"
    }
}

/// Выводы H10: наблюдения без вердикта.
fn judge(meter: &Meter, report: &mut Report) {
    let Some(m0) = meter.bytes(APP, 0, 0) else {
        return;
    };
    if let Some(init) = meter.bytes(INIT, 0, 0) {
        report.metric("memory.app_new", "MiB", vec![signed_mib(init, m0)]);
    }
    let visible = |count: usize| meter.bytes(VISIBLE, 1, count);
    let fixture_note = |count: usize| {
        meter.find(VISIBLE, 1, count).map_or(String::new(), |state| {
            let fixtures: Vec<f64> = state
                .children
                .iter()
                .filter(|child| child.program.starts_with(FIXTURE_PROGRAM))
                .map(|child| child.mib)
                .collect();
            let unreadable = if state.unreadable.is_empty() {
                String::new()
            } else {
                format!(
                    ", непрочитанных потомков {} ({})",
                    state.unreadable.len(),
                    state
                        .unreadable
                        .iter()
                        .map(|child| child.program.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            format!(
                "; потомки отдельно {:.1} МиБ, фикстур {} из {count}, фикстура {:.1} МиБ{unreadable}",
                state.children_mib,
                fixtures.len(),
                fixtures.first().copied().unwrap_or(0.0)
            )
        })
    };

    if let Some(m1) = visible(1) {
        let first = sampler::first_increment(m0, m1);
        report.metric("memory.first_increment", "MiB", vec![mib(first as f64)]);
        observed(
            report,
            "h10-first-increment",
            format!(
                "M(1) − M(0) = {:+.1} МиБ: приложение без поверхностей {:.1} МиБ, одна поверхность {:.1} МиБ{}",
                mib(first as f64),
                mib(m0 as f64),
                mib(m1 as f64),
                fixture_note(1)
            ),
        );
    }
    if let (Some(m1), Some(m2), Some(m4), Some(m8)) =
        (visible(1), visible(2), visible(4), visible(8))
    {
        let marginal = sampler::marginal_increment(m1, m8, MAX).unwrap_or(0.0);
        let steps = [
            sampler::per_surface(m1, m2, 1),
            sampler::per_surface(m2, m4, 2),
            sampler::per_surface(m4, m8, 4),
        ];
        report.metric("memory.marginal_increment", "MiB", vec![mib(marginal)]);
        report.metric(
            "memory.marginal_steps",
            "MiB",
            steps.iter().flatten().map(|bytes| mib(*bytes)).collect(),
        );
        if let Some(state) = meter.find(VISIBLE, 1, MAX) {
            let fixtures: Vec<f64> = state
                .children
                .iter()
                .filter(|child| child.program.starts_with(FIXTURE_PROGRAM))
                .map(|child| child.mib)
                .collect();
            report.metric("memory.fixture", "MiB", fixtures);
        }
        observed(
            report,
            "h10-marginal-increment",
            format!(
                "(M(8) − M(1)) / 7 = {:+.1} МиБ на поверхность; по шагам 1→2 {:+.1}, 2→4 {:+.1}, 4→8 {:+.1} МиБ на поверхность; M(2) {:.1}, M(4) {:.1}, M(8) {:.1} МиБ{}",
                mib(marginal),
                steps[0].map_or(f64::NAN, mib),
                steps[1].map_or(f64::NAN, mib),
                steps[2].map_or(f64::NAN, mib),
                mib(m2 as f64),
                mib(m4 as f64),
                mib(m8 as f64),
                fixture_note(MAX)
            ),
        );
    }

    let mut hidden = Vec::new();
    let mut shown = Vec::new();
    let mut absolute = Vec::new();
    for cycle in 1..=CYCLES {
        let (Some(visible), Some(hide), Some(show)) = (
            meter.bytes(VISIBLE, cycle, MAX),
            meter.bytes(HIDDEN, cycle, MAX),
            meter.bytes(SHOWN, cycle, MAX),
        ) else {
            continue;
        };
        hidden.push(sampler::per_surface(visible, hide, MAX).map_or(0.0, mib));
        shown.push(sampler::per_surface(hide, show, MAX).map_or(0.0, mib));
        absolute.push(format!(
            "цикл {cycle}: {:.1} → {:.1} → {:.1}",
            mib(visible as f64),
            mib(hide as f64),
            mib(show as f64)
        ));
    }
    if !hidden.is_empty() {
        report.metric(
            "memory.visible8",
            "MiB",
            (1..=CYCLES)
                .filter_map(|cycle| meter.bytes(VISIBLE, cycle, MAX))
                .map(|bytes| mib(bytes as f64))
                .collect(),
        );
        report.metric("memory.hidden_per_surface", "MiB", hidden.clone());
        report.metric("memory.shown_per_surface", "MiB", shown.clone());
        observed(
            report,
            "h10-hidden-cost",
            format!(
                "скрытие восьми видимых поверхностей, МиБ на поверхность по циклам: {}; показ скрытых: {}; процесс видимые → скрытые → снова видимые, МиБ: {}; в первом цикле поверхности создавались по одной, в остальных сразу по восемь; минус — память отпущена",
                list(&hidden),
                list(&shown),
                absolute.join("; ")
            ),
        );
    }

    let residual: Vec<f64> = (1..=CYCLES)
        .filter_map(|cycle| meter.bytes(CLOSED, cycle, 0))
        .map(|closed| signed_mib(m0, closed))
        .collect();
    if residual.len() == CYCLES {
        let growth = (residual[CYCLES - 1] - residual[0]) / (CYCLES - 1) as f64;
        report.metric("memory.residual", "MiB", residual.clone());
        observed(
            report,
            "h10-residual",
            format!(
                "остаток над M(0) после закрытия всех поверхностей по циклам: {} МиБ; разовая часть (после первого цикла) {:+.1} МиБ, рост за цикл между первым и последним {:+.1} МиБ — необъяснённый остаток: память, которую процесс не вернул после освобождения поверхностей и выхода фикстур",
                list(&residual),
                residual[0],
                growth
            ),
        );
    }
}

/// Вывод варианта `scrollback`: прирост на поверхность в свежем процессе.
fn judge_scrollback(meter: &Meter, report: &mut Report) {
    let (Some(m0), Some(before), Some(filled)) = (
        meter.bytes(APP, 0, 0),
        meter.find(SCROLLBACK_BEFORE, 1, SCROLLBACK_SURFACES),
        meter.find(SCROLLBACK_FILLED, 1, SCROLLBACK_SURFACES),
    ) else {
        return;
    };
    let per = sampler::per_surface(
        before.process_bytes,
        filled.process_bytes,
        SCROLLBACK_SURFACES,
    )
    .map_or(0.0, mib);
    let children = sampler::per_surface(
        before.children_bytes,
        filled.children_bytes,
        SCROLLBACK_SURFACES,
    )
    .map_or(0.0, mib);
    report.metric("memory.scrollback_per_surface", "MiB", vec![per]);
    let after = meter.bytes(SCROLLBACK_CLOSED, 1, 0).map_or_else(
        || "нет замера".to_owned(),
        |closed| format!("{:+.1} МиБ", signed_mib(m0, closed)),
    );
    let (lines, columns) = match SCROLLBACK {
        Command::Scrollback { lines, columns } => (lines, columns),
        _ => (0, 0),
    };
    observed(
        report,
        "h10-scrollback",
        format!(
            "заполненный scrollback в свежем процессе ({lines} строк по {columns} столбцов на поверхность, предел scrollback-limit-bytes из конфигурации): {per:+.1} МиБ на поверхность; две поверхности до заполнения {:.1} МиБ, после {:.1} МиБ; фикстура отдельно {children:+.1} МиБ на поверхность (её буфер вывода); остаток над M(0) после закрытия {after}",
            before.process_mib, filled.process_mib
        ),
    );
}

fn list(values: &[f64]) -> String {
    values
        .iter()
        .map(|value| format!("{value:+.1}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_equal_and_fit_the_stand_window() {
        let window = crate::WINDOW_SIZE;
        for slot in 0..MAX {
            let frame = slot_frame(slot);
            assert_eq!(frame.size, SLOT);
            assert!(frame.origin.x + frame.size.width <= window.width - SLOT_GAP);
            assert!(frame.origin.y + frame.size.height <= window.height - SLOT_GAP);
        }
        let (a, b) = (slot_frame(0), slot_frame(1));
        assert_eq!(b.origin.x - a.origin.x, SLOT.width + SLOT_GAP);
        assert_eq!(
            slot_frame(SLOT_COLUMNS).origin.y,
            SLOT_GAP * 2.0 + SLOT.height
        );
    }

    #[test]
    fn counts_end_at_max() {
        assert_eq!(COUNTS.last(), Some(&MAX));
    }
}
