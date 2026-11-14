//! Сценарий `idle` (H11): CPU процесса пробы в простое и при выводе, окна замера по 10 с, по пять
//! повторов. Вариант `blink` — обычная конфигурация с миганием курсора, `noblink` — без мигания.
//!
//! Окна варианта `blink`: окно без поверхностей (runtime запущен), видимый простой, видимый вывод,
//! скрытый простой и скрытый вывод тем же потоком фикстуры. Окна `noblink`: окно без поверхностей
//! и видимый простой. Поверхность одна, заполняет окно и держит фокус: без фокуса ghostty мигание
//! останавливает. Самопроверка считает смены `contents` слоя вне окон замера: таймер курсора
//! ghostty перерисовывает кадр в фокусе в обоих вариантах (`blink_self_check`).
//!
//! Окно замера не будит главный поток. Показания в начале и в конце снимает поток сэмплера, который
//! ждёт на канале, а продолжение сценария приходит одним блоком главной очереди. Во время окна нет
//! таймеров стенда, опроса фикстуры и модели терминала; квитанции потока читаются после окна.
//!
//! С бюджетом 1 % сравнивается медиана видимого простоя в обоих вариантах. Под щитом поверх окна
//! (`occlusionState` исключён из признаков) вердикт не выносится: WindowServer окна не показывает,
//! и стоимость кадра может отличаться. Остальные окна — наблюдения.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs;
use std::rc::Rc;
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use serde::Serialize;

use super::input::focused;
use super::memory::create;
use super::surface_env::{self, State};
use super::visibility::{Shield, contents_ptr, shield_step};
use super::{Poll, Report, Scenario, Stand, Step, events, poll, run_steps, window};
use crate::fixture::{self, Command};
use crate::ghostty::config::{self, ColorScheme};
use crate::ghostty::runtime;
use crate::results::{Requirement, Verdict};
use crate::sampler::{self, Reply, Sampler, Window};
use crate::stats;
use crate::visibility::{self, Workarounds};

pub const BLINK: &str = "blink";
/// Вариант без мигания курсора: `run.sh` пишет `cursor-style-blink = false`.
pub const NOBLINK: &str = "noblink";
pub const VARIANTS: [&str; 2] = [BLINK, NOBLINK];
pub const WINDOW: Duration = Duration::from_secs(10);
pub const REPEATS: usize = 5;
pub const BUDGET_PERCENT: f64 = 1.0;
/// Сверка перевода тиков: `getrusage` насчитал хотя бы столько, отношение в пределах.
const UNITS_MIN_NS: u64 = 20_000_000;
const UNITS_LOW: f64 = 0.8;
const UNITS_HIGH: f64 = 1.25;
/// Допуск совпадения пропускной способности видимого и скрытого вывода.
const THROUGHPUT_TOLERANCE: f64 = 0.05;
/// Поток вывода: 1000 строк по 80 столбцов в секунду, 20 с — дольше окна с запасом на запуск.
const STREAM: Command = Command::Stream {
    lines: 20_000,
    interval_us: 1_000,
    columns: 80,
    osc_every: 0,
    report_every: 100,
};
/// Самопроверка мигания: смены `contents` слоя за это время; период мигания ghostty 600 мс.
const BLINK_WATCH: Duration = Duration::from_millis(2_500);
const BLINK_MIN_FRAMES: usize = 2;
const FOCUS_TIMEOUT: Duration = Duration::from_secs(5);
const SIZE_TIMEOUT: Duration = Duration::from_secs(3);
const READY_TIMEOUT: Duration = Duration::from_secs(12);
const STATE_TIMEOUT: Duration = Duration::from_secs(8);
const ACT_TIMEOUT: Duration = Duration::from_secs(5);
const STREAM_TIMEOUT: Duration = Duration::from_secs(8);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
const WATCH_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Kind {
    Empty,
    VisibleIdle,
    VisibleOutput,
    HiddenIdle,
    HiddenOutput,
}

impl Kind {
    const ALL: [Kind; 5] = [
        Kind::Empty,
        Kind::VisibleIdle,
        Kind::VisibleOutput,
        Kind::HiddenIdle,
        Kind::HiddenOutput,
    ];

    fn name(self) -> &'static str {
        match self {
            Kind::Empty => "empty",
            Kind::VisibleIdle => "visible-idle",
            Kind::VisibleOutput => "visible-output",
            Kind::HiddenIdle => "hidden-idle",
            Kind::HiddenOutput => "hidden-output",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Kind::Empty => "окно без поверхностей, runtime запущен",
            Kind::VisibleIdle => "видимый простой",
            Kind::VisibleOutput => "видимый вывод",
            Kind::HiddenIdle => "скрытый простой",
            Kind::HiddenOutput => "скрытый вывод",
        }
    }

    fn output(self) -> bool {
        matches!(self, Kind::VisibleOutput | Kind::HiddenOutput)
    }

    /// Ожидаемая подача модели видимости; `None` — поверхности ещё нет.
    fn fed(self) -> Option<bool> {
        match self {
            Kind::Empty => None,
            Kind::VisibleIdle | Kind::VisibleOutput => Some(true),
            Kind::HiddenIdle | Kind::HiddenOutput => Some(false),
        }
    }

    /// Окно идёт при фокусе на виде поверхности: от него зависит мигание.
    fn focused(self) -> bool {
        matches!(self, Kind::Empty | Kind::VisibleIdle | Kind::VisibleOutput)
    }
}

/// Окна варианта по порядку.
fn kinds(blink: bool) -> Vec<Kind> {
    if blink {
        Kind::ALL.to_vec()
    } else {
        vec![Kind::Empty, Kind::VisibleIdle]
    }
}

/// Итог одного окна замера.
#[derive(Clone, Debug, Serialize)]
struct Record {
    kind: Kind,
    repeat: usize,
    duration_ms: f64,
    cpu_percent: f64,
    rusage_percent: Option<f64>,
    children_cpu_percent: Option<f64>,
    wakeups_per_s: Option<f64>,
    units_ratio: Option<f64>,
    footprint_mib: f64,
    lines_per_s: Option<f64>,
    bytes_per_s: Option<f64>,
    children: Vec<String>,
}

struct Idle {
    env: State,
    sampler: Sampler,
    blink: bool,
    records: Vec<Record>,
    /// Номер команды текущего потока вывода.
    stream: Option<u64>,
    shield: Option<Shield>,
    /// Самопроверка мигания: начало, последний `contents`, смены.
    watch: Option<(Instant, usize, usize)>,
    blink_frames: Option<usize>,
}

type Shared = Rc<RefCell<Idle>>;

enum Phase {
    Steps(Vec<Step>),
    Quiet(Kind, usize),
}

type Done = Box<dyn FnOnce(Result<(), String>)>;

thread_local! {
    /// Продолжение после окна замера; его вызывает блок главной очереди от потока сэмплера.
    static RESUME: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

fn resume() {
    let next = RESUME.with(|slot| slot.borrow_mut().take());
    if let Some(next) = next {
        next();
    }
}

fn wake_main() {
    DispatchQueue::main().exec_async(resume);
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Idle);
    let variant = stand.prelude.variant.clone();
    report.push("variant", &variant);
    if !VARIANTS.contains(&variant.as_str()) {
        let reason = format!("предусловие: вариант {variant}, ожидается один из {VARIANTS:?}");
        return stand.conclude(report, Err(reason));
    }
    let blink = variant == BLINK;
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
    if let Err(reason) = check_config(&stand, blink) {
        return stand.conclude(report, Err(reason));
    }
    report.push(
        "idle_note",
        "CPU процесса — 100 × Δ(ri_user_time + ri_system_time) / Δ CLOCK_UPTIME_RAW без деления на ядра, потомки отдельно; окно 10 с, показания снимает поток сэмплера, главный поток во время окна не будится стендом; медиана и разброс по пяти повторам",
    );
    visibility::enable(Workarounds::default());
    let sampler = match Sampler::start(wake_main) {
        Ok(sampler) => sampler,
        Err(reason) => return stand.conclude(report, Err(reason)),
    };
    let env = match surface_env::prepare(&stand, &mut report, ColorScheme::Light) {
        Ok(env) => env,
        Err(reason) => return surface_env::finish(&stand, report, Err(reason), None),
    };
    let view = env.view.clone();
    let idle = Rc::new(RefCell::new(Idle {
        env,
        sampler,
        blink,
        records: Vec::new(),
        stream: None,
        shield: None,
        watch: None,
        blink_frames: None,
    }));
    let phases = plan(&stand, &idle, blink);
    let finishing = Rc::clone(&stand);
    run_phases(
        Rc::clone(&stand),
        Rc::clone(&idle),
        phases,
        Box::new(move |result| {
            if result.is_ok() {
                judge(&idle.borrow(), &variant, &mut report);
            }
            {
                let idle = idle.borrow();
                report.push_json("idle_windows", &idle.records);
                report.push_json("idle_shield", &idle.shield);
                report.push_json("idle_blink_frames", &idle.blink_frames);
            }
            report.push("occlusion_masked", visibility::occlusion_masked());
            surface_env::finish(&finishing, report, result, Some(view));
        }),
    );
}

/// Конфигурация процесса задаёт мигание курсора по варианту.
fn check_config(stand: &Stand, blink: bool) -> Result<(), String> {
    let path = stand.prelude.out.join(config::FILE);
    let text = fs::read_to_string(&path)
        .map_err(|err| format!("предусловие: конфигурация {}: {err}", path.display()))?;
    let expected = format!("cursor-style-blink = {blink}");
    if text.lines().any(|line| line.trim() == expected) {
        Ok(())
    } else {
        Err(format!(
            "предусловие: в {} нет строки «{expected}» для варианта",
            path.display()
        ))
    }
}

fn plan(stand: &Rc<Stand>, idle: &Shared, blink: bool) -> VecDeque<Phase> {
    let mut phases = VecDeque::new();
    phases.push_back(Phase::Steps(setup_steps(stand, idle)));
    for kind in kinds(blink) {
        match kind {
            Kind::VisibleIdle => phases.push_back(Phase::Steps(create_steps(stand, idle))),
            Kind::HiddenIdle => phases.push_back(Phase::Steps(hide_steps(idle))),
            Kind::Empty | Kind::VisibleOutput | Kind::HiddenOutput => {}
        }
        for repeat in 1..=REPEATS {
            phases.push_back(Phase::Steps(before_window(stand, idle, kind)));
            phases.push_back(Phase::Quiet(kind, repeat));
            if kind.output() {
                phases.push_back(Phase::Steps(after_output(idle)));
            }
        }
        if kind == Kind::VisibleIdle {
            phases.push_back(Phase::Steps(blink_steps(idle)));
        }
    }
    phases.push_back(Phase::Steps(close_steps(stand, idle, blink)));
    phases
}

fn run_phases(stand: Rc<Stand>, idle: Shared, mut phases: VecDeque<Phase>, done: Done) {
    match phases.pop_front() {
        None => done(Ok(())),
        Some(Phase::Steps(steps)) => run_steps(steps, move |result| match result {
            Ok(()) => run_phases(stand, idle, phases, done),
            Err(failure) => done(Err(failure.describe())),
        }),
        Some(Phase::Quiet(kind, repeat)) => {
            let (next_stand, next_idle) = (Rc::clone(&stand), Rc::clone(&idle));
            quiet(
                stand,
                idle,
                kind,
                repeat,
                Box::new(move |result| match result {
                    Ok(()) => run_phases(next_stand, next_idle, phases, done),
                    Err(reason) => done(Err(reason)),
                }),
            );
        }
    }
}

/// Окно замера: запрос сэмплеру и продолжение по блоку главной очереди.
fn quiet(stand: Rc<Stand>, idle: Shared, kind: Kind, repeat: usize, done: Done) {
    events::note(format!(
        "окно {} №{repeat}: {} с без таймеров стенда",
        kind.name(),
        WINDOW.as_secs()
    ));
    if let Err(reason) = idle.borrow_mut().sampler.window(WINDOW) {
        return done(Err(format!("сэмплер: {reason}")));
    }
    RESUME.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            let reply = idle.borrow_mut().sampler.take();
            let result = match reply {
                Some(Reply::Window(window)) => record(&stand, &idle, kind, repeat, window),
                Some(Reply::Failed(reason)) => Err(format!("сэмплер: {reason}")),
                Some(_) => Err("сэмплер: ответ не на окно".to_owned()),
                None => Err("главный поток разбужен без ответа сэмплера".to_owned()),
            };
            done(result);
        }));
    });
}

/// Проверки состояния после окна и запись итога окна.
fn record(
    stand: &Stand,
    idle: &Shared,
    kind: Kind,
    repeat: usize,
    window: Window,
) -> Result<(), String> {
    let mut idle = idle.borrow_mut();
    let view = idle.env.view.clone();
    let label = format!("окно {} №{repeat}", kind.name());
    if kind.focused()
        && let Poll::Pending(state) = focused(stand, &view)
    {
        return Err(format!("{label}: фокус потерян во время окна: {state}"));
    }
    if !stand.window.isVisible() {
        return Err(format!("{label}: окно пробы ушло с экрана"));
    }
    if let Some(visible) = kind.fed() {
        let fed = view.visibility_fed();
        if fed != Some((visible, false)) {
            return Err(format!(
                "{label}: подача модели видимости {fed:?}, ожидается {visible}"
            ));
        }
    }
    let units_ratio = window.units_ratio(UNITS_MIN_NS);
    if let Some(ratio) = units_ratio
        && !(UNITS_LOW..=UNITS_HIGH).contains(&ratio)
    {
        return Err(format!(
            "{label}: CPU по proc_pid_rusage расходится с getrusage, отношение {ratio:.3}"
        ));
    }
    let cpu_percent = window
        .cpu_percent()
        .ok_or(format!("{label}: показания без прошедшего времени"))?;
    let (lines_per_s, bytes_per_s) = if kind.output() {
        throughput(&mut idle, &window).map_err(|reason| format!("{label}: {reason}"))?
    } else {
        (None, None)
    };
    events::note(format!(
        "{label}: CPU {cpu_percent:.3} %, потомки {:?} %, пробуждений {:?}/с",
        window.children_cpu_percent(),
        window.wakeups_per_s()
    ));
    let record = Record {
        kind,
        repeat,
        duration_ms: window.duration_ms(),
        cpu_percent,
        rusage_percent: window.rusage_percent(),
        children_cpu_percent: window.children_cpu_percent(),
        wakeups_per_s: window.wakeups_per_s(),
        units_ratio,
        footprint_mib: sampler::mib(window.end.process.footprint as f64),
        lines_per_s,
        bytes_per_s,
        children: window
            .end
            .children
            .iter()
            .map(|child| format!("{} {}", child.pid, child.program))
            .collect(),
    };
    idle.records.push(record);
    Ok(())
}

/// Пропускная способность потока по квитанциям хода внутри окна: строки и байты в секунду.
fn throughput(idle: &mut Idle, window: &Window) -> Result<(Option<f64>, Option<f64>), String> {
    let id = idle.stream.ok_or("потока вывода нет")?;
    idle.env.hub.poll();
    let nonce = idle.env.fixture_nonce.clone();
    let conn = idle
        .env
        .hub
        .by_nonce(&nonce)
        .ok_or("нет соединения фикстуры")?;
    let (from, to) = (window.start.t_ns, window.end.t_ns);
    let mut progress = Vec::new();
    for receipt in &conn.receipts {
        match receipt.event {
            fixture::Event::StreamProgress {
                id: got,
                lines,
                bytes,
            } if got == id && (from..=to).contains(&receipt.t_ns) => {
                progress.push((receipt.t_ns, lines, bytes));
            }
            fixture::Event::StreamDone { id: got, .. } if got == id && receipt.t_ns <= to => {
                return Err("поток фикстуры закончился раньше окна".to_owned());
            }
            _ => {}
        }
    }
    let (Some(first), Some(last)) = (progress.first(), progress.last()) else {
        return Err("нет квитанций хода потока внутри окна".to_owned());
    };
    Ok((
        sampler::per_second(first.1, last.1, first.0, last.0),
        sampler::per_second(first.2, last.2, first.0, last.0),
    ))
}

fn setup_steps(stand: &Rc<Stand>, idle: &Shared) -> Vec<Step> {
    let (focus, focus_idle) = (Rc::clone(stand), Rc::clone(idle));
    let masking = Rc::clone(idle);
    vec![
        Step::new(
            "активность, key window и фокус вида поверхности",
            FOCUS_TIMEOUT,
            move || focused(&focus, &focus_idle.borrow().env.view),
            || Ok(()),
        ),
        shield_step(stand, move |shield| {
            masking.borrow_mut().shield = Some(shield);
        }),
    ]
}

fn create_steps(stand: &Rc<Stand>, idle: &Shared) -> Vec<Step> {
    let (sized, creating) = (Rc::clone(idle), Rc::clone(idle));
    let ready = Rc::clone(idle);
    let (framed, frame_stand) = (Rc::clone(idle), Rc::clone(stand));
    vec![
        Step::new(
            "ненулевой размер вида в пикселях",
            SIZE_TIMEOUT,
            move || {
                let pixels = sized.borrow().env.view.pixel_size();
                poll(pixels.0 > 0 && pixels.1 > 0, || {
                    format!("размер вида в пикселях {pixels:?}")
                })
            },
            move || create(creating.borrow().env.id),
        ),
        Step::new(
            "готовность фикстуры в поверхности",
            READY_TIMEOUT,
            move || surface_env::ready(&mut ready.borrow_mut().env),
            || Ok(()),
        ),
        Step::new(
            "первый кадр, подача видимости и фокус",
            STATE_TIMEOUT,
            move || {
                let idle = framed.borrow();
                let view = &idle.env.view;
                let (contents, fed) = (view.has_contents(), view.visibility_fed());
                match focused(&frame_stand, view) {
                    Poll::Ready(()) if contents && fed == Some((true, false)) => Poll::Ready(()),
                    Poll::Ready(()) => Poll::Pending(format!("кадр {contents}, подача {fed:?}")),
                    Poll::Pending(state) => Poll::Pending(state),
                }
            },
            || Ok(()),
        ),
    ]
}

/// Перед окном: фокус или подача видимости по виду окна, для вывода — запущенный поток.
fn before_window(stand: &Rc<Stand>, idle: &Shared, kind: Kind) -> Vec<Step> {
    let mut steps = Vec::new();
    let (checking, check_stand) = (Rc::clone(idle), Rc::clone(stand));
    steps.push(Step::new(
        "состояние перед окном замера",
        STATE_TIMEOUT,
        move || {
            let idle = checking.borrow();
            let view = &idle.env.view;
            if kind.focused()
                && let Poll::Pending(state) = focused(&check_stand, view)
            {
                return Poll::Pending(state);
            }
            match kind.fed() {
                Some(visible) => {
                    let fed = view.visibility_fed();
                    poll(fed == Some((visible, false)), || {
                        format!("подача {fed:?}, ожидается {visible}")
                    })
                }
                None => Poll::Ready(()),
            }
        },
        || Ok(()),
    ));
    if kind.output() {
        let starting = Rc::clone(idle);
        steps.push(Step::new(
            "запуск потока вывода фикстуры",
            ACT_TIMEOUT,
            || Poll::Ready(()),
            move || {
                let mut idle = starting.borrow_mut();
                let nonce = idle.env.fixture_nonce.clone();
                let id = idle.env.hub.send(&nonce, STREAM)?;
                idle.stream = Some(id);
                Ok(())
            },
        ));
        let flowing = Rc::clone(idle);
        steps.push(Step::new(
            "первая квитанция хода потока",
            STREAM_TIMEOUT,
            move || {
                let mut idle = flowing.borrow_mut();
                let Some(id) = idle.stream else {
                    return Poll::Pending("потока нет".to_owned());
                };
                idle.env.hub.poll();
                let nonce = idle.env.fixture_nonce.clone();
                let started = idle.env.hub.by_nonce(&nonce).is_some_and(|conn| {
                    conn.receipts.iter().any(|receipt| {
                        matches!(receipt.event, fixture::Event::StreamProgress { id: got, .. } if got == id)
                    })
                });
                poll(started, || format!("поток {id}: квитанций хода нет"))
            },
            || Ok(()),
        ));
    }
    steps
}

/// После окна вывода: остановка потока и его последняя квитанция.
fn after_output(idle: &Shared) -> Vec<Step> {
    let stopping = Rc::clone(idle);
    let stopped = Rc::clone(idle);
    vec![
        Step::new(
            "остановка потока вывода",
            ACT_TIMEOUT,
            || Poll::Ready(()),
            move || {
                let mut idle = stopping.borrow_mut();
                let nonce = idle.env.fixture_nonce.clone();
                idle.env.hub.send(&nonce, Command::StopStream).map(|_| ())
            },
        ),
        Step::new(
            "поток вывода остановлен",
            STREAM_TIMEOUT,
            move || {
                let mut idle = stopped.borrow_mut();
                let Some(id) = idle.stream else {
                    return Poll::Ready(());
                };
                idle.env.hub.poll();
                let nonce = idle.env.fixture_nonce.clone();
                let done = idle.env.hub.by_nonce(&nonce).is_some_and(|conn| {
                    conn.receipts.iter().any(|receipt| {
                        matches!(receipt.event, fixture::Event::StreamDone { id: got, .. } if got == id)
                    })
                });
                if done {
                    idle.stream = None;
                }
                poll(done, || format!("поток {id}: квитанции окончания нет"))
            },
            || Ok(()),
        ),
    ]
}

/// Самопроверка мигания вне окон замера: смены `contents` слоя видимой поверхности в фокусе.
fn blink_steps(idle: &Shared) -> Vec<Step> {
    let watching = Rc::clone(idle);
    vec![Step::new(
        "самопроверка мигания курсора",
        WATCH_TIMEOUT,
        move || {
            let mut idle = watching.borrow_mut();
            let current = contents_ptr(&idle.env.view);
            let (started, last, changes) = idle.watch.get_or_insert((Instant::now(), current, 0));
            if current != *last {
                *last = current;
                *changes += 1;
            }
            if started.elapsed() < BLINK_WATCH {
                return Poll::Pending(format!("смен contents {changes}"));
            }
            let changes = *changes;
            idle.blink_frames = Some(changes);
            idle.watch = None;
            Poll::Ready(())
        },
        || Ok(()),
    )]
}

fn hide_steps(idle: &Shared) -> Vec<Step> {
    let hiding = Rc::clone(idle);
    let hidden = Rc::clone(idle);
    vec![
        Step::new(
            "скрытие вида поверхности",
            ACT_TIMEOUT,
            || Poll::Ready(()),
            move || {
                hiding.borrow().env.view.setHidden(true);
                Ok(())
            },
        ),
        Step::new(
            "модель подала скрытие",
            STATE_TIMEOUT,
            move || {
                let fed = hidden.borrow().env.view.visibility_fed();
                poll(fed == Some((false, false)), || format!("подача {fed:?}"))
            },
            || Ok(()),
        ),
    ]
}

fn close_steps(stand: &Rc<Stand>, idle: &Shared, shown: bool) -> Vec<Step> {
    let mut steps = Vec::new();
    if shown {
        let (showing, show_stand) = (Rc::clone(idle), Rc::clone(stand));
        steps.push(Step::new(
            "показ вида поверхности",
            ACT_TIMEOUT,
            || Poll::Ready(()),
            move || {
                let idle = showing.borrow();
                idle.env.view.setHidden(false);
                show_stand.window.makeFirstResponder(Some(&idle.env.view));
                Ok(())
            },
        ));
        let visible = Rc::clone(idle);
        steps.push(Step::new(
            "модель подала показ",
            STATE_TIMEOUT,
            move || {
                let fed = visible.borrow().env.view.visibility_fed();
                poll(fed == Some((true, false)), || format!("подача {fed:?}"))
            },
            || Ok(()),
        ));
    }
    let closing = Rc::clone(idle);
    steps.push(Step::new(
        "намерение закрыть поверхность",
        ACT_TIMEOUT,
        || Poll::Ready(()),
        move || {
            let id = closing.borrow().env.id;
            runtime::request_close(id).map(|_| ())
        },
    ));
    let gone = Rc::clone(idle);
    steps.push(Step::new(
        "освобождение поверхности и выход фикстуры",
        CLOSE_TIMEOUT,
        move || surface_env::freed(&mut gone.borrow_mut().env),
        || Ok(()),
    ));
    steps
}

fn median(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let values: Vec<f64> = values.flatten().collect();
    stats::repeats(&values).ok().map(|repeats| repeats.median)
}

fn format_opt(value: Option<f64>, digits: usize) -> String {
    value.map_or_else(|| "нет".to_owned(), |value| format!("{value:.digits$}"))
}

/// Выводы H11 и метрики окон.
fn judge(idle: &Idle, variant: &str, report: &mut Report) {
    let masked = visibility::occlusion_masked();
    let self_check = blink_self_check(idle, report);
    let mut output_rates: Vec<(Kind, f64)> = Vec::new();
    for kind in kinds(idle.blink) {
        let records: Vec<&Record> = idle.records.iter().filter(|r| r.kind == kind).collect();
        let cpu: Vec<f64> = records.iter().map(|record| record.cpu_percent).collect();
        let name = kind.name();
        report.metric(&format!("cpu.{name}"), "%", cpu.clone());
        report.metric(
            &format!("cpu_children.{name}"),
            "%",
            records
                .iter()
                .filter_map(|r| r.children_cpu_percent)
                .collect(),
        );
        report.metric(
            &format!("wakeups.{name}"),
            "1/s",
            records.iter().filter_map(|r| r.wakeups_per_s).collect(),
        );
        if kind.output() {
            let rates: Vec<f64> = records.iter().filter_map(|r| r.lines_per_s).collect();
            report.metric(&format!("throughput.{name}"), "lines/s", rates.clone());
            if let Ok(repeats) = stats::repeats(&rates) {
                output_rates.push((kind, repeats.median));
            }
        }
        let Ok(repeats) = stats::repeats(&cpu) else {
            continue;
        };
        let title = match (kind, idle.blink) {
            (Kind::VisibleIdle, true) => "видимый простой с миганием курсора",
            (Kind::VisibleIdle, false) => "видимый простой без мигания курсора",
            _ => kind.title(),
        };
        let text = format!(
            "{title}: медиана {:.3} %, разброс {:.3}–{:.3} % ({} повторов по {} с, CPU процесса без деления на ядра); потомки медиана {} %; пробуждений процесса медиана {}/с",
            repeats.median,
            repeats.min,
            repeats.max,
            repeats.count,
            WINDOW.as_secs(),
            format_opt(median(records.iter().map(|r| r.children_cpu_percent)), 3),
            format_opt(median(records.iter().map(|r| r.wakeups_per_s)), 0),
        );
        if kind != Kind::VisibleIdle {
            report.finding(
                &format!("h11-{name}"),
                Requirement::Required,
                Verdict::Observed,
                text,
            );
            continue;
        }
        let budget = format!("h11-idle-budget-{variant}");
        if masked {
            report.finding(
                &budget,
                Requirement::Required,
                Verdict::Observed,
                format!(
                    "{text}; вердикт по бюджету {BUDGET_PERCENT} % не вынесен: окно пробы под щитом, occlusionState исключён из признаков"
                ),
            );
            report.finding(
                &format!("h11-idle-unshielded-{variant}"),
                Requirement::Required,
                Verdict::PendingManual,
                "прогон без щита поверх окна пробы: make probe-ghostty SCENARIOS=\"idle\" с владельцем у машины (задача 19)",
            );
        } else if !self_check {
            report.finding(
                &budget,
                Requirement::Required,
                Verdict::Observed,
                format!(
                    "{text}; вердикт по бюджету {BUDGET_PERCENT} % не вынесен: самопроверка мигания не прошла"
                ),
            );
        } else {
            let verdict = if repeats.median <= BUDGET_PERCENT {
                Verdict::Pass
            } else {
                Verdict::Fail
            };
            report.finding(
                &budget,
                Requirement::Required,
                verdict,
                format!("{text}; бюджет {BUDGET_PERCENT} % по медиане"),
            );
        }
    }
    let rate = |kind: Kind| {
        output_rates
            .iter()
            .find(|(candidate, _)| *candidate == kind)
            .map(|(_, rate)| *rate)
    };
    if let (Some(visible), Some(hidden)) = (rate(Kind::VisibleOutput), rate(Kind::HiddenOutput)) {
        let gap = (hidden - visible).abs() / visible.max(f64::MIN_POSITIVE);
        report.finding(
            "h11-output-throughput",
            Requirement::Required,
            Verdict::Observed,
            format!(
                "пропускная способность по квитанциям фикстуры: видимый вывод медиана {visible:.0} строк/с, скрытый {hidden:.0} строк/с, расхождение {:.1} % ({} допуска {:.0} %)",
                gap * 100.0,
                if gap <= THROUGHPUT_TOLERANCE {
                    "в пределах"
                } else {
                    "вне"
                },
                THROUGHPUT_TOLERANCE * 100.0
            ),
        );
    }
    let ratios: Vec<f64> = idle.records.iter().filter_map(|r| r.units_ratio).collect();
    report.push_json("idle_units_ratios", &ratios);
}

/// Таймер курсора рендера ghostty (`src/renderer/Thread.zig:240, 654–688`) будит отрисовку каждые
/// 600 мс, пока поверхность в фокусе, независимо от `cursor-style-blink`: настройка меняет только
/// видимость курсора на кадре. Поэтому смены `contents` ожидаются в обоих вариантах, а вариант без
/// мигания отличает строка конфигурации (`check_config`). `true` — самопроверка прошла.
fn blink_self_check(idle: &Idle, report: &mut Report) -> bool {
    let Some(frames) = idle.blink_frames else {
        return false;
    };
    let pass = frames >= BLINK_MIN_FRAMES;
    report.finding(
        "h11-blink-self-check",
        Requirement::Required,
        if pass { Verdict::Pass } else { Verdict::Fail },
        format!(
            "смен contents слоя видимой поверхности в фокусе за {} мс вне окон замера: {frames}; ожидается не меньше {BLINK_MIN_FRAMES}: таймер курсора рендера ghostty перерисовывает кадр каждые 600 мс в фокусе {}",
            BLINK_WATCH.as_millis(),
            if idle.blink {
                "и переключает видимость курсора"
            } else {
                "и при cursor-style-blink = false, курсор на кадре не меняется; неизменность курсора по пикселям не проверяется"
            }
        ),
    );
    pass
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_choose_windows() {
        assert_eq!(kinds(true), Kind::ALL.to_vec());
        assert_eq!(kinds(false), vec![Kind::Empty, Kind::VisibleIdle]);
        for kind in Kind::ALL {
            assert_eq!(kind.output(), kind.name().ends_with("output"));
            assert_eq!(kind.fed() == Some(false), kind.name().starts_with("hidden"));
            assert_eq!(kind.focused(), kind.fed() != Some(false));
        }
    }

    /// Срок сценария покрывает окна варианта `blink` с запасом на подготовку.
    #[test]
    fn deadline_covers_windows() {
        let windows = WINDOW * (Kind::ALL.len() * REPEATS) as u32;
        assert!(Scenario::Idle.deadline() >= windows + Duration::from_secs(90));
    }

    /// Поток вывода длиннее окна.
    #[test]
    fn stream_outlasts_window() {
        let Command::Stream {
            lines, interval_us, ..
        } = STREAM
        else {
            panic!("не поток");
        };
        let duration = Duration::from_micros(u64::from(lines) * interval_us);
        assert!(duration >= WINDOW + STREAM_TIMEOUT);
    }

    #[test]
    fn run_script_writes_blink_by_variant() {
        let script = include_str!("../../run.sh");
        assert!(script.contains(&format!("NOBLINK_VARIANT={NOBLINK}")));
        assert!(script.contains("cursor-style-blink = $blink"));
        assert!(script.contains("idle) echo \"blink $NOBLINK_VARIANT\""));
    }
}
