//! Сценарий `latency`: задержка клавиши терминала (H9) на обвязке `input` — A в центральной
//! зоне, B в нижней, обе фикстуры в raw-режиме. Вариант — дисплей окна: `builtin` или
//! `external`.
//!
//! Участки по разделу «Задержка» плана, все от `t_post` клавиши в таблице стенда:
//! - A — очередь до начала `sendEvent`, длительность `sendEvent` и `ghostty_surface_key`;
//! - B — чтение байта фикстурой (время из квитанции, снято сразу после `read`);
//! - C — ответ фикстуры виден в модели: наблюдатель стенда на таймере 1 мс в общих режимах цикла
//!   опрашивает `read_text` без `tick`, `draw` и `refresh`; результат — интервал между последним
//!   опросом без ответа и первым с ответом;
//! - D (исследование) — цветной блок ответа появился в текущей `IOSurface` слоя: готовность
//!   кадра, а не время до фотона;
//! - N — клавиша `Navigation`, меняющая фокус между A и B, → конец прохода отображения, по
//!   определению P0.2. Сравнение с бюджетом выносит только `--aggregate` и только при пройденной
//!   самопроверке.
//!
//! Каждая фаза — 10 прогревочных и 100 замеров, без фона и с потоком вывода в B. Принудительного
//! `draw` и `refresh` нет. Время до фотона не измеряется.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSEventModifierFlags, NSView};
use objc2_core_foundation::CFType;
use objc2_core_graphics::CGDisplayIsBuiltin;
use objc2_foundation::{
    MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSRunLoop, NSRunLoopCommonModes, NSTimer,
};
use objc2_io_surface::{IOSurfaceLockOptions, IOSurfaceRef};
use objc2_quartz_core::CADisplayLink;
use serde::Serialize;

use super::counters::{Live, counter};
use super::events::{self, Key};
use super::input::{self, A, B, Harness, Input, Plan, STATE_TIMEOUT, focused, key, only_a};
use super::snapshot::{self, Pending};
use super::visibility::near;
use super::{Poll, Scenario, Stand, poll};
use crate::clock;
use crate::fixture::{Command, Event, Receipt};
use crate::ghostty::runtime;
use crate::ghostty::surface::SurfaceId;
use crate::results::{LATENCY_PREFIX, LatencyMarks, Requirement, Verdict};
use crate::router::{self, Mode};
use crate::scene::{self, Rgb, Scene, Tile, Zone};
use crate::view::{Sent, SurfaceView};

pub const BUILTIN: &str = "builtin";
pub const EXTERNAL: &str = "external";
/// Встроенный дисплей в режиме 60 Гц на время процесса: замена внешнего дисплея 60 Гц
/// приёмочного прохода исполнителя.
pub const BUILTIN60: &str = "builtin60";
/// Варианты сценария, они же дисплеи окна (`run.sh`, `LATENCY_DISPLAYS`).
pub const DISPLAYS: [&str; 3] = [BUILTIN, EXTERNAL, BUILTIN60];

const CONTROL: usize = NSEventModifierFlags::Control.0;
const KEY_Z: Key = key("z", 6, "z", "z", 0);
const KEY_J: Key = key("j", 38, "j", "j", 0);
const KEY_K: Key = key("k", 40, "k", "k", 0);
const KEY_I: Key = key("i", 34, "i", "i", 0);
const CHORD: Key = key("Ctrl-\\", 42, "\u{1c}", "\\", CONTROL);

const WARMUP: usize = 10;
const MEASURED: usize = 100;
const REPS: usize = WARMUP + MEASURED;
/// Шаг наблюдателя C и D, секунды.
const OBSERVER_INTERVAL: f64 = 0.001;
/// Ответа в модели нет за этот срок — стенд потерял контроль над фикстурой.
const ANSWER_LIMIT_NS: u64 = 2_000_000_000;
/// Срок поиска блока ответа в `IOSurface` слоя от `t_post`.
const FRAME_LIMIT_NS: u64 = 300_000_000;
const REP_TIMEOUT: Duration = Duration::from_secs(5);
const MARK_TIMEOUT: Duration = Duration::from_secs(3);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);
const STREAM_TIMEOUT: Duration = Duration::from_secs(8);
/// Вызовов `displayLink` для частоты.
const RATE_FRAMES: usize = 121;
/// Без вызовов ссылки вида за этот срок частота меряется ссылкой экрана.
const LINK_START_LIMIT: Duration = Duration::from_secs(2);
const RATE_LIMIT: Duration = Duration::from_secs(6);
const RATE_TIMEOUT: Duration = Duration::from_secs(10);
/// Блок ответа: строки и столбцы с левого верхнего угла.
const BLOCK_ROWS: u16 = 4;
const BLOCK_COLS: u16 = 24;
/// Цвета блока чередуются от повтора к повтору: свежий блок отличим от прежнего.
const COLORS: [Rgb; 2] = [Rgb(0, 200, 0), Rgb(255, 0, 255)];
/// Наименьшая разница линии под вкладкой у плитки с фокусом и без него на снимке.
const TONE_CONTRAST: u8 = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Load {
    Quiet,
    Background,
}

impl Load {
    fn name(self) -> &'static str {
        match self {
            Load::Quiet => "quiet",
            Load::Background => "background",
        }
    }
}

/// Поток вывода в B на время фазы с фоном: около 2000 строк в секунду до 60 с.
fn background_stream() -> Command {
    Command::Stream {
        lines: 120_000,
        interval_us: 500,
        columns: 100,
        osc_every: 0,
        report_every: 200,
    }
}

pub fn run(stand: Rc<Stand>) {
    input::run_harness(
        stand,
        Harness {
            scenario: Scenario::Latency,
            prefix: "h9",
            notes: &[
                (
                    "route_note",
                    "клавиши идут через postEvent и обычную доставку NSApplication; t_post и отметки sendEvent — таблица стенда, CLOCK_UPTIME_RAW",
                ),
                (
                    "observer_note",
                    "наблюдатель C — таймер стенда 1 мс в общих режимах цикла с момента t_post; опрашивает read_text без tick, draw и refresh; результат — интервал между последним опросом без ответа и первым с ответом",
                ),
                (
                    "photon_note",
                    "время до фотона не измерено: D — готовность кадра в IOSurface слоя, N — конец прохода отображения",
                ),
                (
                    "budget_note",
                    "с бюджетом 16 мс сравнивается только p95 N без фона и только в --aggregate при пройденной самопроверке",
                ),
            ],
            build,
        },
    );
}

// ---- чистые расчёты ----

/// Опрос наблюдателя: начало, конец и найден ли ответ.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Probe {
    pub t0: u64,
    pub t1: u64,
    pub hit: bool,
}

/// Граница появления: не раньше `lower` и не позже `upper`, наносекунды.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub lower: u64,
    pub upper: u64,
}

impl Bounds {
    /// Погрешность шага наблюдателя: ширина интервала.
    pub fn step_ns(self) -> u64 {
        self.upper.saturating_sub(self.lower)
    }
}

/// Последний опрос без ответа и первый с ответом. Чтение внутри опроса видит состояние в
/// какой-то момент между `t0` и `t1`, поэтому нижняя граница — начало опроса без ответа, верхняя
/// — конец опроса с ответом. Ответ уже на первом опросе даёт нижней границей `since`.
pub fn bounds(since: u64, probes: &[Probe]) -> Option<Bounds> {
    let first = probes.iter().position(|probe| probe.hit)?;
    let lower = match first {
        0 => since,
        _ => probes[first - 1].t0,
    };
    Some(Bounds {
        lower,
        upper: probes[first].t1,
    })
}

/// Промежутки между началами соседних опросов, мс.
pub fn gaps_ms(probes: &[Probe]) -> Vec<f64> {
    probes
        .windows(2)
        .map(|pair| clock::interval_ms(pair[0].t0, pair[1].t0))
        .collect()
}

/// Время квитанции чтения, на которое фикстура записала ответ команды `id`. Ответ на два чтения
/// — нарушение протокола.
pub fn read_for_reply(receipts: &[Receipt], id: u64) -> Result<Option<u64>, String> {
    let mut found = receipts.iter().filter(|receipt| match &receipt.event {
        Event::Read { replied, .. } => replied.iter().any(|replied| replied.id == id),
        _ => false,
    });
    match (found.next(), found.next()) {
        (None, _) => Ok(None),
        (Some(receipt), None) => Ok(Some(receipt.t_ns)),
        (Some(_), Some(_)) => Err(format!("ответ команды #{id} записан на два чтения")),
    }
}

/// Длительность вызова `ghostty_surface_key` нажатия, начатого внутри `sendEvent` события.
pub fn surface_key_ns(sent: &[Sent], start: u64, end: u64) -> Option<u64> {
    sent.iter()
        .find(|sent| sent.from == "keyDown" && sent.at_ns >= start && sent.at_ns <= end)
        .map(|sent| sent.done_ns.saturating_sub(sent.at_ns))
}

/// Значения замеров без прогревочных повторов.
pub fn measured<T: Copy>(values: &[(bool, Option<T>)]) -> Vec<T> {
    values
        .iter()
        .filter(|(warmup, _)| !warmup)
        .filter_map(|(_, value)| *value)
        .collect()
}

/// Частота по меткам `displayLink`, Гц: обратная медиана промежутков.
pub fn rate_hz(timestamps: &[f64]) -> Option<f64> {
    let mut gaps: Vec<f64> = timestamps
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .filter(|gap| *gap > 0.0)
        .collect();
    if gaps.is_empty() {
        return None;
    }
    gaps.sort_by(f64::total_cmp);
    let median = gaps[(gaps.len() - 1) / 2];
    Some(1.0 / median)
}

/// Ответ фикстуры: блок цвета `color` в левом верхнем углу и уникальная метка строкой ниже.
pub fn answer(tag: &str, color: Rgb) -> String {
    let mut text = String::new();
    for row in 1..=BLOCK_ROWS {
        let _ = write!(
            text,
            "\x1b[{row};1H\x1b[48;2;{};{};{}m{:width$}\x1b[0m",
            color.0,
            color.1,
            color.2,
            "",
            width = usize::from(BLOCK_COLS)
        );
    }
    let _ = write!(text, "\x1b[{};1H\x1b[2K{tag}", BLOCK_ROWS + 2);
    text
}

/// Точки выборки блока ответа в пикселях: середина второй строки, три столбца.
pub fn block_points(cell_width: u32, cell_height: u32) -> Vec<(usize, usize)> {
    let (cw, ch) = (cell_width as usize, cell_height as usize);
    let y = 2 * ch;
    [6, 12, 18].into_iter().map(|col| (col * cw, y)).collect()
}

// ---- наблюдатель C и D ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Watching {
    Text,
    Frame,
    Done,
}

struct Observer {
    t_post: u64,
    id: SurfaceId,
    view: Retained<SurfaceView>,
    tag: String,
    color: Rgb,
    points: Vec<(usize, usize)>,
    text: Vec<Probe>,
    frame: Vec<Probe>,
    watching: Watching,
    answer_timed_out: bool,
    frame_error: Option<String>,
}

impl Observer {
    fn tick(&mut self) {
        let t0 = clock::uptime_ns();
        match self.watching {
            Watching::Text => {
                let text = runtime::viewport_text(self.id).unwrap_or_default();
                let t1 = clock::uptime_ns();
                let hit = text.contains(&self.tag);
                self.text.push(Probe { t0, t1, hit });
                if hit {
                    self.watching = Watching::Frame;
                } else if t0.saturating_sub(self.t_post) > ANSWER_LIMIT_NS {
                    self.answer_timed_out = true;
                    self.watching = Watching::Done;
                }
            }
            Watching::Frame => {
                let hit = match read_pixels(&self.view, &self.points) {
                    Ok(Some(pixels)) => pixels.iter().all(|pixel| near(*pixel, self.color)),
                    Ok(None) => false,
                    Err(reason) => {
                        self.frame_error = Some(reason);
                        self.watching = Watching::Done;
                        return;
                    }
                };
                let t1 = clock::uptime_ns();
                self.frame.push(Probe { t0, t1, hit });
                if hit || t0.saturating_sub(self.t_post) > FRAME_LIMIT_NS {
                    self.watching = Watching::Done;
                }
            }
            Watching::Done => {}
        }
    }
}

/// Пиксели текущей `IOSurface` слоя вида под `points`. `None` — у слоя нет `contents` или точка
/// вне кадра.
fn read_pixels(view: &SurfaceView, points: &[(usize, usize)]) -> Result<Option<Vec<Rgb>>, String> {
    let Some(layer) = view.layer() else {
        return Ok(None);
    };
    // SAFETY: чтение свойства слоя на главном потоке.
    let Some(contents) = (unsafe { layer.contents() }) else {
        return Ok(None);
    };
    // SAFETY: объекты Foundation бесплатно мостятся в Core Foundation; тип проверяется ниже.
    let object: &CFType = unsafe { &*Retained::as_ptr(&contents).cast::<CFType>() };
    let Some(surface) = object.downcast_ref::<IOSurfaceRef>() else {
        return Err("contents слоя поверхности не IOSurface".to_owned());
    };
    const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
    if surface.pixel_format() != BGRA {
        return Err(format!(
            "формат IOSurface слоя {:#x}, стенд читает только BGRA",
            surface.pixel_format()
        ));
    }
    let (width, height, row) = (surface.width(), surface.height(), surface.bytes_per_row());
    if points.iter().any(|(x, y)| *x >= width || *y >= height) {
        return Ok(None);
    }
    // SAFETY: значение seed не запрашивается, указатель NULL допустим.
    let status = unsafe { surface.lock(IOSurfaceLockOptions::ReadOnly, ptr::null_mut()) };
    if status != 0 {
        return Err(format!("IOSurfaceLock вернул {status}"));
    }
    let base = surface.base_address().as_ptr().cast::<u8>();
    let pixels = points
        .iter()
        .map(|(x, y)| {
            // SAFETY: между lock и unlock базовый адрес указывает на `row × height` байт
            // единственной плоскости BGRA, точка внутри кадра.
            let pixel = unsafe { std::slice::from_raw_parts(base.add(y * row + x * 4), 4) };
            Rgb(pixel[2], pixel[1], pixel[0])
        })
        .collect();
    // SAFETY: как у lock.
    let status = unsafe { surface.unlock(IOSurfaceLockOptions::ReadOnly, ptr::null_mut()) };
    if status != 0 {
        return Err(format!("IOSurfaceUnlock вернул {status}"));
    }
    Ok(Some(pixels))
}

/// Запускает наблюдатель на таймере главного потока в общих режимах цикла.
fn start_observer(observer: &Rc<RefCell<Observer>>) {
    let owner = Rc::clone(observer);
    let block = RcBlock::new(move |timer: NonNull<NSTimer>| {
        // SAFETY: таймер жив, пока вызывается его блок.
        let timer = unsafe { timer.as_ref() };
        let mut observer = owner.borrow_mut();
        observer.tick();
        if observer.watching == Watching::Done {
            timer.invalidate();
        }
    });
    // SAFETY: таймер добавляется в цикл главного потока и снимается там же.
    let timer =
        unsafe { NSTimer::timerWithTimeInterval_repeats_block(OBSERVER_INTERVAL, true, &block) };
    timer.setTolerance(0.0);
    // SAFETY: `NSRunLoopCommonModes` — неизменяемая константа Foundation.
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
}

// ---- частота displayLink ----

counter!(RATE_DRIVER, "ProbeLatencyRateDriver");

pub struct DriverIvars {
    _live: Live,
    timestamps: RefCell<Vec<f64>>,
}

define_class!(
    /// Цель `displayLink`: записывает метку каждого вызова.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeLatencyRateDriver"]
    #[ivars = DriverIvars]
    pub struct RateDriver;

    unsafe impl NSObjectProtocol for RateDriver {}

    impl RateDriver {
        #[unsafe(method(tick:))]
        fn tick(&self, link: &CADisplayLink) {
            self.ivars().timestamps.borrow_mut().push(link.timestamp());
        }
    }
);

impl RateDriver {
    fn new(mtm: MainThreadMarker) -> Retained<RateDriver> {
        let this = RateDriver::alloc(mtm).set_ivars(DriverIvars {
            _live: RATE_DRIVER.live(),
            timestamps: RefCell::new(Vec::new()),
        });
        // SAFETY: `init` у NSObject без аргументов.
        unsafe { msg_send![super(this), init] }
    }

    fn count(&self) -> usize {
        self.ivars().timestamps.borrow().len()
    }
}

struct Rate {
    driver: Retained<RateDriver>,
    link: Retained<CADisplayLink>,
    source: &'static str,
    started: Instant,
}

fn start_rate(stand: &Stand, view: bool) -> Result<Rate, String> {
    let driver = RateDriver::new(stand.view.mtm());
    let target: &AnyObject = &driver;
    let link = if view {
        // SAFETY: цель — объект пробы с методом `tick:`, принимающим `CADisplayLink`.
        unsafe {
            stand
                .view
                .displayLinkWithTarget_selector(target, sel!(tick:))
        }
    } else {
        let screen = stand
            .window
            .screen()
            .ok_or("у окна нет экрана для displayLink")?;
        // SAFETY: как у ссылки вида.
        unsafe { screen.displayLinkWithTarget_selector(target, sel!(tick:)) }
    };
    // SAFETY: ссылка добавляется в цикл главного потока в общих режимах.
    unsafe { link.addToRunLoop_forMode(&NSRunLoop::currentRunLoop(), NSRunLoopCommonModes) };
    Ok(Rate {
        driver,
        link,
        source: if view { "view" } else { "screen" },
        started: Instant::now(),
    })
}

// ---- состояние сценария ----

/// Повтор клавиши `z` с ответом фикстуры.
#[derive(Clone, Debug, Default, Serialize)]
struct KeyRep {
    load: &'static str,
    index: usize,
    warmup: bool,
    reply_id: u64,
    tag: String,
    event: u64,
    t_post: u64,
    queue_ms: Option<f64>,
    send_event_ms: Option<f64>,
    surface_key_ms: Option<f64>,
    b_ms: Option<f64>,
    c_lower_ms: Option<f64>,
    c_upper_ms: Option<f64>,
    c_step_ms: Option<f64>,
    d_lower_ms: Option<f64>,
    d_upper_ms: Option<f64>,
    text_polls: usize,
    frame_polls: usize,
    poll_gaps_ms: Vec<f64>,
    poll_costs_ms: Vec<f64>,
    frame_error: Option<String>,
}

/// Тон плиток, прочитанный у слоёв в конце прохода отображения.
#[derive(Clone, Debug, Serialize)]
struct Tone {
    target: Option<Rgb>,
    other: Option<Rgb>,
    expected_focused: Rgb,
    expected_plain: Rgb,
    committed: bool,
}

/// Повтор клавиши `Navigation`.
#[derive(Clone, Debug, Serialize)]
struct NavRep {
    load: &'static str,
    index: usize,
    warmup: bool,
    key: &'static str,
    event: u64,
    n_ms: f64,
    queue_ms: Option<f64>,
    send_event_ms: Option<f64>,
    tone: Tone,
}

struct Mark {
    at: u64,
    tone: Tone,
}

/// Охват фазы фоном: квитанции хода B до первого и после последнего повтора.
#[derive(Clone, Debug, Default, Serialize)]
struct Coverage {
    phase: String,
    stream_id: u64,
    progress_before_ns: Option<u64>,
    phase_end_ns: u64,
    progress_after_ns: Option<u64>,
    done_before_end: bool,
    covered: bool,
}

#[derive(Clone, Debug, Serialize)]
struct SelfCheck {
    tone: Option<Tone>,
    snapshot: Option<String>,
    samples: Vec<Option<Rgb>>,
    passed: bool,
    reason: String,
}

#[derive(Default)]
struct Lat {
    scene: Option<Rc<Scene>>,
    observer: Option<Rc<RefCell<Observer>>>,
    current: Option<KeyRep>,
    keys: Vec<KeyRep>,
    navs: Vec<NavRep>,
    mark: Option<Mark>,
    nav_event: Option<(u64, usize, &'static str, Load)>,
    rate: Option<Rate>,
    timestamps: Vec<f64>,
    rate_source: Option<&'static str>,
    builtin: bool,
    stream: Option<Coverage>,
    coverage: Vec<Coverage>,
    self_check: Option<SelfCheck>,
    snapshot: Option<Pending>,
    snapshot_points: usize,
}

type Shared = Rc<RefCell<Lat>>;

fn build(plan: &mut Plan) {
    let lat: Shared = Rc::new(RefCell::new(Lat::default()));
    display(plan, &lat);
    keys(plan, &lat, Load::Quiet);
    keys(plan, &lat, Load::Background);
    navigation(plan, &lat);
    conclude(plan, &lat);
}

// ---- дисплей и частота ----

fn display(plan: &mut Plan, lat: &Shared) {
    let check = Rc::clone(lat);
    plan.act(
        "дисплей окна и ссылка displayLink вида",
        move |stand, input| {
            let screen = stand.window.screen().ok_or("у окна нет экрана")?;
            let builtin = CGDisplayIsBuiltin(screen.CGDirectDisplayID());
            let want = stand.prelude.variant.as_str();
            if !DISPLAYS.contains(&want) {
                return Err(format!("вариант {want}: ожидается один из {DISPLAYS:?}"));
            }
            let expected = want != EXTERNAL;
            if builtin != expected {
                return Err(format!(
                    "предусловие: окно варианта {want} на {} дисплее",
                    if builtin {
                        "встроенном"
                    } else {
                        "внешнем"
                    }
                ));
            }
            let mut lat = check.borrow_mut();
            lat.builtin = builtin;
            lat.scene = Some(Rc::clone(&input.scene));
            lat.rate = Some(start_rate(stand, true)?);
            Ok(())
        },
    );
    let started = Rc::clone(lat);
    plan.step(
        "первые вызовы displayLink",
        RATE_TIMEOUT,
        move |_, _| {
            let lat = started.borrow();
            let Some(rate) = lat.rate.as_ref() else {
                return Poll::Ready(());
            };
            let count = rate.driver.count();
            poll(
                count > 0 || rate.started.elapsed() >= LINK_START_LIMIT,
                || format!("вызовов {count}"),
            )
        },
        {
            let lat = Rc::clone(lat);
            move |stand, _| {
                let mut lat = lat.borrow_mut();
                let idle = lat
                    .rate
                    .as_ref()
                    .is_some_and(|rate| rate.driver.count() == 0);
                if idle {
                    if let Some(rate) = lat.rate.take() {
                        rate.link.invalidate();
                    }
                    events::note("displayLink вида не вызывается, частота по ссылке экрана");
                    lat.rate = Some(start_rate(stand, false)?);
                }
                Ok(())
            }
        },
    );
    let counted = Rc::clone(lat);
    plan.step(
        "частота displayLink",
        RATE_TIMEOUT,
        move |_, _| {
            let lat = counted.borrow();
            let Some(rate) = lat.rate.as_ref() else {
                return Poll::Ready(());
            };
            let count = rate.driver.count();
            poll(
                count >= RATE_FRAMES || rate.started.elapsed() >= RATE_LIMIT,
                || format!("вызовов {count} из {RATE_FRAMES}"),
            )
        },
        {
            let lat = Rc::clone(lat);
            move |_, input| {
                let mut lat = lat.borrow_mut();
                if let Some(rate) = lat.rate.take() {
                    rate.link.invalidate();
                    lat.timestamps = rate.driver.ivars().timestamps.borrow().clone();
                    lat.rate_source = Some(rate.source);
                }
                let hz = rate_hz(&lat.timestamps);
                input.note(format!(
                    "displayLink ({}): {} вызовов, {}",
                    lat.rate_source.unwrap_or("нет"),
                    lat.timestamps.len(),
                    hz.map_or_else(
                        || "частота не измерена".to_owned(),
                        |hz| format!("{hz:.1} Гц")
                    )
                ));
                Ok(())
            }
        },
    );
}

// ---- клавиша терминала: A, B, C, D ----

fn keys(plan: &mut Plan, lat: &Shared, load: Load) {
    if load == Load::Background {
        background_on(plan, lat, "keys");
    }
    let name = match load {
        Load::Quiet => "keys-quiet",
        Load::Background => "keys-background",
    };
    plan.begin(name, A, |_| only_a(&[b'z'; REPS]));
    for index in 0..REPS {
        let arm = Rc::clone(lat);
        plan.act(
            "взвод ответа фикстуры A",
            move |stand, input| {
                let color = COLORS[index % COLORS.len()];
                let tag = format!(
                    "lt{}{index:03}x{}",
                    &load.name()[..1],
                    short(&stand.prelude.nonce)
                );
                let reply_id = input.send_id(
                    A,
                    Command::Reply {
                        trigger: "z".to_owned(),
                        answer: answer(&tag, color),
                    },
                )?;
                let size = runtime::read(input.id(A)).ok_or("A не жива")?.size;
                let observer = Observer {
                    t_post: 0,
                    id: input.id(A),
                    view: input.view(A),
                    tag: tag.clone(),
                    color,
                    points: block_points(size.cell_width_px, size.cell_height_px),
                    text: Vec::new(),
                    frame: Vec::new(),
                    watching: Watching::Text,
                    answer_timed_out: false,
                    frame_error: None,
                };
                let mut lat = arm.borrow_mut();
                lat.observer = Some(Rc::new(RefCell::new(observer)));
                lat.current = Some(KeyRep {
                    load: load.name(),
                    index,
                    warmup: index < WARMUP,
                    reply_id,
                    tag,
                    ..KeyRep::default()
                });
                Ok(())
            },
        );
        let press = Rc::clone(lat);
        plan.step(
            "ack ответа и клавиша z",
            STATE_TIMEOUT,
            |_, input| input.acked(),
            move |_, input| {
                let posted = events::press(KEY_Z)?;
                let down = *posted.first().ok_or("клавиша z не отправлена")?;
                let t_post = events::record(down)
                    .map(|record| record.t_post)
                    .ok_or("нет записи события z")?;
                input.posted = posted;
                let mut lat = press.borrow_mut();
                let observer = lat.observer.clone().ok_or("наблюдатель не взведён")?;
                observer.borrow_mut().t_post = t_post;
                start_observer(&observer);
                if let Some(rep) = lat.current.as_mut() {
                    rep.event = down;
                    rep.t_post = t_post;
                }
                Ok(())
            },
        );
        let ready = Rc::clone(lat);
        let finish = Rc::clone(lat);
        plan.step(
            "ответ в модели, кадр и квитанция чтения",
            REP_TIMEOUT,
            move |_, input| key_ready(&ready, input),
            move |_, input| key_finish(&finish, input),
        );
    }
    if load == Load::Background {
        background_off(plan, lat);
    }
    plan.finish();
}

fn key_ready(lat: &Shared, input: &mut Input) -> Poll<()> {
    let lat = lat.borrow();
    let (Some(observer), Some(rep)) = (lat.observer.as_ref(), lat.current.as_ref()) else {
        return Poll::Ready(());
    };
    let watching = observer.borrow().watching;
    if watching != Watching::Done {
        return Poll::Pending(format!("наблюдатель: {watching:?}"));
    }
    if let Poll::Pending(state) = input.dispatched() {
        return Poll::Pending(state);
    }
    if observer.borrow().answer_timed_out {
        return Poll::Ready(());
    }
    let receipts = input.receipts(A);
    match read_for_reply(&receipts, rep.reply_id) {
        Ok(Some(_)) | Err(_) => Poll::Ready(()),
        Ok(None) => Poll::Pending(format!("нет квитанции чтения с ответом #{}", rep.reply_id)),
    }
}

fn key_finish(lat: &Shared, input: &mut Input) -> Result<(), String> {
    let mut lat = lat.borrow_mut();
    let observer = lat.observer.take().ok_or("нет наблюдателя")?;
    let mut rep = lat.current.take().ok_or("нет повтора")?;
    let observer = observer.borrow();
    if observer.answer_timed_out {
        return Err(format!(
            "ответ {} не появился в модели A за {} мс",
            rep.tag,
            ANSWER_LIMIT_NS / 1_000_000
        ));
    }
    let receipts = input.receipts(A);
    let read = read_for_reply(&receipts, rep.reply_id)?;
    let ms = |to: u64| clock::interval_ms(rep.t_post, to);
    rep.b_ms = read.map(ms);
    if let Some(record) = events::record(rep.event) {
        rep.queue_ms = record.dispatch_start.map(ms);
        if let (Some(start), Some(end)) = (record.dispatch_start, record.send_return) {
            rep.send_event_ms = Some(clock::interval_ms(start, end));
            rep.surface_key_ms = surface_key_ns(&input.view(A).input_log().sent, start, end)
                .map(|ns| ns as f64 / 1e6);
        }
    }
    if let Some(text) = bounds(rep.t_post, &observer.text) {
        rep.c_lower_ms = Some(ms(text.lower));
        rep.c_upper_ms = Some(ms(text.upper));
        rep.c_step_ms = Some(text.step_ns() as f64 / 1e6);
        if let Some(frame) = bounds(text.lower, &observer.frame) {
            rep.d_lower_ms = Some(ms(frame.lower));
            rep.d_upper_ms = Some(ms(frame.upper));
        }
    }
    rep.text_polls = observer.text.len();
    rep.frame_polls = observer.frame.len();
    rep.poll_gaps_ms = gaps_ms(&observer.text);
    rep.poll_costs_ms = observer
        .text
        .iter()
        .map(|probe| clock::interval_ms(probe.t0, probe.t1))
        .collect();
    rep.frame_error = observer.frame_error.clone();
    lat.keys.push(rep);
    Ok(())
}

// ---- фон: поток вывода B ----

fn progress_of(receipts: &[Receipt], id: u64) -> (Vec<u64>, Option<u64>) {
    let mut progress = Vec::new();
    let mut done = None;
    for receipt in receipts {
        match &receipt.event {
            Event::StreamProgress { id: of, .. } if *of == id => progress.push(receipt.t_ns),
            Event::StreamDone { id: of, .. } if *of == id => done = Some(receipt.t_ns),
            _ => {}
        }
    }
    (progress, done)
}

fn background_on(plan: &mut Plan, lat: &Shared, phase: &'static str) {
    let start = Rc::clone(lat);
    plan.act("фон: поток вывода B", move |_, input| {
        let id = input.send_id(B, background_stream())?;
        start.borrow_mut().stream = Some(Coverage {
            phase: phase.to_owned(),
            stream_id: id,
            ..Coverage::default()
        });
        Ok(())
    });
    plan.wait("ack потока B", STATE_TIMEOUT, |_, input| {
        input.acked()
    });
    let first = Rc::clone(lat);
    plan.wait(
        "квитанция хода B до первого повтора",
        STREAM_TIMEOUT,
        move |_, input| {
            let mut lat = first.borrow_mut();
            let Some(stream) = lat.stream.as_mut() else {
                return Poll::Ready(());
            };
            let (progress, _) = progress_of(&input.receipts(B), stream.stream_id);
            stream.progress_before_ns = progress.first().copied();
            poll(stream.progress_before_ns.is_some(), || {
                format!("нет квитанции хода потока #{}", stream.stream_id)
            })
        },
    );
}

fn background_off(plan: &mut Plan, lat: &Shared) {
    let end = Rc::clone(lat);
    plan.act("конец фазы с фоном", move |_, _| {
        if let Some(stream) = end.borrow_mut().stream.as_mut() {
            stream.phase_end_ns = clock::uptime_ns();
        }
        Ok(())
    });
    let after = Rc::clone(lat);
    plan.step(
        "квитанция хода B после последнего повтора",
        STREAM_TIMEOUT,
        move |_, input| {
            let mut lat = after.borrow_mut();
            let Some(stream) = lat.stream.as_mut() else {
                return Poll::Ready(());
            };
            let (progress, done) = progress_of(&input.receipts(B), stream.stream_id);
            stream.done_before_end = done.is_some_and(|done| done <= stream.phase_end_ns);
            stream.progress_after_ns = progress
                .iter()
                .copied()
                .find(|at| *at > stream.phase_end_ns);
            poll(stream.progress_after_ns.is_some() || done.is_some(), || {
                format!("нет квитанции хода потока #{} после фазы", stream.stream_id)
            })
        },
        {
            let lat = Rc::clone(lat);
            move |_, input| {
                let mut lat = lat.borrow_mut();
                let Some(mut stream) = lat.stream.take() else {
                    return Ok(());
                };
                stream.covered = stream.progress_before_ns.is_some()
                    && stream.progress_after_ns.is_some()
                    && !stream.done_before_end;
                input.verify(
                    format!(
                        "поток B идёт всю фазу {} (ход до первого и после последнего повтора)",
                        stream.phase
                    ),
                    stream.covered,
                );
                let id = stream.stream_id;
                lat.coverage.push(stream);
                drop(lat);
                input.send(B, Command::StopStream)?;
                input.note(format!("поток B #{id} остановлен"));
                Ok(())
            }
        },
    );
    plan.wait(
        "ack остановки потока B",
        STATE_TIMEOUT,
        |_, input| input.acked(),
    );
}

// ---- навигация: N ----

fn tile_of(scene: &Scene, zone: Zone) -> Option<Retained<Tile>> {
    scene.tile(zone, 0)
}

fn tone(scene: &Scene, target: Zone) -> Tone {
    let other = if target == Zone::Center {
        Zone::Bottom
    } else {
        Zone::Center
    };
    let read = |zone| tile_of(scene, zone).and_then(|tile| tile.strip_rgb());
    let palette =
        tile_of(scene, target).map_or_else(|| scene::Palette::of(false), |tile| tile.palette());
    let expected_focused = palette.strip(true, Mode::Navigation);
    let expected_plain = palette.strip(false, Mode::Navigation);
    let (target_rgb, other_rgb) = (read(target), read(other));
    Tone {
        target: target_rgb,
        other: other_rgb,
        expected_focused,
        expected_plain,
        committed: target_rgb == Some(expected_focused) && other_rgb == Some(expected_plain),
    }
}

/// Отметка конца первого прохода отображения после возврата `sendEvent` события и тон плиток в
/// этот момент.
fn arm_mark(lat: &Shared, number: u64, target: Zone) {
    let owner = Rc::clone(lat);
    events::on_display_pass(move |at| {
        let Some(record) = events::record(number) else {
            return true;
        };
        if record.send_return.is_none_or(|returned| at < returned) {
            return true;
        }
        let mut lat = owner.borrow_mut();
        let Some(scene) = lat.scene.clone() else {
            return false;
        };
        lat.mark = Some(Mark {
            at,
            tone: tone(&scene, target),
        });
        false
    });
}

fn navigation(plan: &mut Plan, lat: &Shared) {
    plan.begin("navigation", A, |_| only_a(b""));
    plan.act(
        "маршрутизатор режимов",
        |stand, input| {
            router::install(Rc::clone(&input.scene), &stand.window);
            Ok(())
        },
    );
    plan.act("Ctrl-\\ — Navigation", |_, input| {
        input.posted = events::press(CHORD)?;
        Ok(())
    });
    plan.wait("режим Navigation", STATE_TIMEOUT, |_, input| {
        if let Poll::Pending(state) = input.dispatched() {
            return Poll::Pending(state);
        }
        let mode = router::snapshot().map(|router| router.mode);
        poll(mode == Some(Mode::Navigation), || format!("режим {mode:?}"))
    });
    self_check(plan, lat);
    nav_phase(plan, lat, Load::Quiet);
    background_on(plan, lat, "navigation");
    nav_phase(plan, lat, Load::Background);
    background_off(plan, lat);
    plan.act("k — фокус A", |_, input| {
        input.posted = events::press(KEY_K)?;
        Ok(())
    });
    plan.wait("фокус A", STATE_TIMEOUT, |stand, input| {
        match input.dispatched() {
            Poll::Ready(()) => focused(stand, &input.view(A)),
            pending => pending,
        }
    });
    plan.act("i — Input", |_, input| {
        input.posted = events::press(KEY_I)?;
        Ok(())
    });
    plan.wait(
        "режим Input и фокус A",
        STATE_TIMEOUT,
        |stand, input| {
            if let Poll::Pending(state) = input.dispatched() {
                return Poll::Pending(state);
            }
            let mode = router::snapshot().map(|router| router.mode);
            if mode != Some(Mode::Input) {
                return Poll::Pending(format!("режим {mode:?}"));
            }
            focused(stand, &input.view(A))
        },
    );
    plan.finish();
}

/// Точка в плитке в координатах экрана: `x` от левого края, `y` от низа плитки.
fn tile_point(tile: &Tile, x: f64, y: f64) -> Option<NSPoint> {
    let window = tile.window()?;
    let view: &NSView = tile;
    let point = view.convertPoint_toView(NSPoint::new(x, y), None);
    Some(window.convertPointToScreen(point))
}

/// Линия под активной вкладкой и правый край полосы вкладок плитки.
fn strip_points(tile: &Tile) -> Option<[NSPoint; 2]> {
    let size = tile.bounds().size;
    let content = scene::content_frame(size).size.height;
    Some([
        tile_point(
            tile,
            scene::TAB_WIDTH / 2.0,
            content + scene::FOCUS_LINE / 2.0,
        )?,
        tile_point(tile, size.width - 24.0, content + scene::TAB_BAR / 2.0)?,
    ])
}

fn self_check(plan: &mut Plan, lat: &Shared) {
    let press = Rc::clone(lat);
    plan.act(
        "j самопроверки: отметка и снимок",
        move |stand, input| {
            input.posted = events::press(KEY_J)?;
            let down = *input.posted.first().ok_or("клавиша j не отправлена")?;
            press.borrow_mut().mark = None;
            arm_mark(&press, down, Zone::Bottom);
            let owner = Rc::clone(&press);
            let out = stand.prelude.out.clone();
            events::on_display_pass(move |_| {
                let mut lat = owner.borrow_mut();
                if lat.mark.is_none() {
                    return true;
                }
                let Some(scene) = lat.scene.clone() else {
                    return false;
                };
                let tiles = (tile_of(&scene, Zone::Bottom), tile_of(&scene, Zone::Center));
                let (Some(target), Some(other)) = tiles else {
                    return false;
                };
                let points: Vec<NSPoint> = [strip_points(&target), strip_points(&other)]
                    .into_iter()
                    .flatten()
                    .flatten()
                    .collect();
                let Some(window) = target.window() else {
                    return false;
                };
                lat.snapshot_points = points.len();
                lat.snapshot = Some(snapshot::capture(
                    window.mtm(),
                    window.frame(),
                    points,
                    &out,
                    "latency-n-self-check",
                ));
                false
            });
            Ok(())
        },
    );
    let judged = Rc::clone(lat);
    plan.step(
        "отметка и снимок самопроверки",
        SNAPSHOT_TIMEOUT,
        {
            let lat = Rc::clone(lat);
            move |_, input| {
                if let Poll::Pending(state) = input.dispatched() {
                    return Poll::Pending(state);
                }
                let lat = lat.borrow();
                poll(lat.mark.is_some() && lat.snapshot.is_some(), || {
                    format!(
                        "отметка {}, снимок {}",
                        lat.mark.is_some(),
                        lat.snapshot.is_some()
                    )
                })
            }
        },
        move |_, _| Ok(()),
    );
    plan.step(
        "итог снимка самопроверки",
        SNAPSHOT_TIMEOUT,
        {
            let lat = Rc::clone(&judged);
            move |_, input| {
                let mut lat = lat.borrow_mut();
                if lat.self_check.is_some() {
                    return Poll::Ready(());
                }
                let Some(capture) = lat.snapshot.as_ref().and_then(Pending::take) else {
                    return Poll::Pending("снимок в работе".to_owned());
                };
                let tone = lat.mark.take().map(|mark| mark.tone);
                let samples = capture.samples.clone();
                let check = judge_self_check(tone, &capture, samples, lat.snapshot_points);
                input.snapshots.push(capture.snapshot.clone());
                input.note(format!("самопроверка N: {}", check.reason));
                lat.self_check = Some(check);
                Poll::Ready(())
            }
        },
        |_, _| Ok(()),
    );
}

fn judge_self_check(
    tone: Option<Tone>,
    capture: &snapshot::Capture,
    samples: Vec<Option<Rgb>>,
    expected_points: usize,
) -> SelfCheck {
    let committed = tone.as_ref().is_some_and(|tone| tone.committed);
    let (passed, reason) = if !committed {
        (
            false,
            format!("к отметке тон плиток в слоях не тот: {tone:?}"),
        )
    } else if !capture.ok() {
        (
            false,
            format!(
                "снимок окна не снят ({:?}): {}",
                capture.snapshot.status,
                capture.snapshot.reason.as_deref().unwrap_or("без причины")
            ),
        )
    } else if samples.len() != expected_points || expected_points != 4 {
        (false, format!("точек снимка {} из 4", samples.len()))
    } else {
        match (samples[0], samples[2], tone.as_ref()) {
            (Some(focused_line), Some(plain_line), Some(tone)) => {
                let navigation = scene::Palette::of(false).mode_color(Mode::Navigation);
                let contrast = snapshot::distance(focused_line, plain_line);
                let closer = |pixel: Rgb, to: Rgb, than: Rgb| {
                    snapshot::distance(pixel, to) < snapshot::distance(pixel, than)
                };
                let ok = contrast >= TONE_CONTRAST
                    && closer(focused_line, navigation, tone.expected_plain)
                    && closer(plain_line, tone.expected_plain, navigation);
                (
                    ok,
                    format!(
                        "тон в слоях к отметке зафиксирован; линия под вкладкой B на снимке {focused_line:?}, A {plain_line:?}, разница {contrast} (нужно от {TONE_CONTRAST}){}",
                        if ok {
                            ""
                        } else {
                            "; снимок не показывает смену тона: вероятно, окно перекрыто (щит dndmode) или снимок без окон"
                        }
                    ),
                )
            }
            _ => (false, "точки линии под вкладкой вне снимка".to_owned()),
        }
    };
    SelfCheck {
        tone,
        snapshot: capture.snapshot.file.clone(),
        samples,
        passed,
        reason,
    }
}

fn nav_phase(plan: &mut Plan, lat: &Shared, load: Load) {
    // После самопроверки фокус у B; повтор с чётным номером возвращает его в A.
    for index in 0..REPS {
        let (key, target) = if index % 2 == 0 {
            (KEY_K, Zone::Center)
        } else {
            (KEY_J, Zone::Bottom)
        };
        let press = Rc::clone(lat);
        plan.act("клавиша Navigation", move |_, input| {
            input.posted = events::press(key)?;
            let down = *input.posted.first().ok_or("клавиша не отправлена")?;
            let mut lat = press.borrow_mut();
            lat.mark = None;
            lat.nav_event = Some((down, index, key.label, load));
            drop(lat);
            arm_mark(&press, down, target);
            Ok(())
        });
        let ready = Rc::clone(lat);
        let finish = Rc::clone(lat);
        plan.step(
            "конец прохода отображения после клавиши",
            MARK_TIMEOUT,
            move |_, input| {
                if let Poll::Pending(state) = input.dispatched() {
                    return Poll::Pending(state);
                }
                poll(ready.borrow().mark.is_some(), || {
                    "нет отметки прохода".to_owned()
                })
            },
            move |stand, input| {
                let mut lat = finish.borrow_mut();
                let mark = lat.mark.take().ok_or("нет отметки")?;
                let (event, index, label, load) = lat.nav_event.take().ok_or("нет события")?;
                let record = events::record(event).ok_or("нет записи события")?;
                let view = if target == Zone::Center {
                    input.view(A)
                } else {
                    input.view(B)
                };
                if !input::is_responder(stand, &view) {
                    return Err(format!(
                        "после {label} фокус не у {}",
                        if target == Zone::Center { "A" } else { "B" }
                    ));
                }
                lat.navs.push(NavRep {
                    load: load.name(),
                    index,
                    warmup: index < WARMUP,
                    key: label,
                    event,
                    n_ms: clock::interval_ms(record.t_post, mark.at),
                    queue_ms: record
                        .dispatch_start
                        .map(|start| clock::interval_ms(record.t_post, start)),
                    send_event_ms: record
                        .dispatch_start
                        .zip(record.send_return)
                        .map(|(start, end)| clock::interval_ms(start, end)),
                    tone: mark.tone,
                });
                Ok(())
            },
        );
    }
}

// ---- итог ----

fn conclude(plan: &mut Plan, lat: &Shared) {
    let lat = Rc::clone(lat);
    plan.act("итог задержки", move |stand, input| {
        let lat = lat.borrow();
        let mut metrics: Vec<(String, &'static str, Vec<f64>)> = Vec::new();
        for load in [Load::Quiet, Load::Background] {
            let reps: Vec<&KeyRep> = lat.keys.iter().filter(|rep| rep.load == load.name()).collect();
            let series = |pick: fn(&KeyRep) -> Option<f64>| -> Vec<f64> {
                measured(
                    &reps
                        .iter()
                        .map(|rep| (rep.warmup, pick(rep)))
                        .collect::<Vec<_>>(),
                )
            };
            let name = |segment: &str| format!("{LATENCY_PREFIX}{segment}.{}", load.name());
            metrics.push((name("a-queue"), "ms", series(|rep| rep.queue_ms)));
            metrics.push((name("a-send-event"), "ms", series(|rep| rep.send_event_ms)));
            metrics.push((name("a-surface-key"), "ms", series(|rep| rep.surface_key_ms)));
            metrics.push((name("b"), "ms", series(|rep| rep.b_ms)));
            metrics.push((name("c-lower"), "ms", series(|rep| rep.c_lower_ms)));
            metrics.push((name("c-upper"), "ms", series(|rep| rep.c_upper_ms)));
            metrics.push((name("c-step"), "ms", series(|rep| rep.c_step_ms)));
            metrics.push((name("d-upper"), "ms", series(|rep| rep.d_upper_ms)));
            let measured_reps = reps.iter().filter(|rep| !rep.warmup);
            metrics.push((
                name("c-poll-gap"),
                "ms",
                measured_reps
                    .clone()
                    .flat_map(|rep| rep.poll_gaps_ms.iter().copied())
                    .collect(),
            ));
            metrics.push((
                name("c-poll-cost"),
                "ms",
                measured_reps
                    .flat_map(|rep| rep.poll_costs_ms.iter().copied())
                    .collect(),
            ));
            let navs: Vec<(bool, Option<f64>)> = lat
                .navs
                .iter()
                .filter(|rep| rep.load == load.name())
                .map(|rep| (rep.warmup, Some(rep.n_ms)))
                .collect();
            metrics.push((name("n"), "ms", measured(&navs)));
            let nav_queue: Vec<(bool, Option<f64>)> = lat
                .navs
                .iter()
                .filter(|rep| rep.load == load.name())
                .map(|rep| (rep.warmup, rep.queue_ms))
                .collect();
            metrics.push((name("n-queue"), "ms", measured(&nav_queue)));
            let nav_send: Vec<(bool, Option<f64>)> = lat
                .navs
                .iter()
                .filter(|rep| rep.load == load.name())
                .map(|rep| (rep.warmup, rep.send_event_ms))
                .collect();
            metrics.push((name("n-send-event"), "ms", measured(&nav_send)));
        }
        let counts = |load: Load| {
            let reps = lat.keys.iter().filter(|rep| rep.load == load.name() && !rep.warmup);
            let total = reps.clone().count();
            let complete = reps
                .clone()
                .filter(|rep| {
                    rep.queue_ms.is_some()
                        && rep.send_event_ms.is_some()
                        && rep.surface_key_ms.is_some()
                        && rep.b_ms.is_some()
                        && rep.c_upper_ms.is_some()
                })
                .count();
            let frames = reps.filter(|rep| rep.d_upper_ms.is_some()).count();
            (total, complete, frames)
        };
        let navs = |load: Load| {
            lat.navs
                .iter()
                .filter(|rep| rep.load == load.name() && !rep.warmup)
                .count()
        };
        let committed = lat.navs.iter().filter(|rep| rep.tone.committed).count();
        for load in [Load::Quiet, Load::Background] {
            let (total, complete, _) = counts(load);
            input.verify(
                format!(
                    "{}: повторов клавиши с участками A, B и C {complete} из {total} (нужно {MEASURED})",
                    load.name()
                ),
                total == MEASURED && complete == MEASURED,
            );
            input.verify(
                format!("{}: замеров N {} из {MEASURED}", load.name(), navs(load)),
                navs(load) == MEASURED,
            );
        }
        input.verify(
            format!(
                "тон плиток в слоях к отметке зафиксирован у {committed} из {} повторов N",
                lat.navs.len()
            ),
            committed == lat.navs.len(),
        );
        let hz = rate_hz(&lat.timestamps);
        let summary = |values: &[f64]| match crate::stats::summarize(values) {
            Ok(summary) => format!(
                "p50 {:.2}, p95 {:.2}, max {:.2}",
                summary.p50, summary.p95, summary.max
            ),
            Err(err) => format!("нет сводки: {err:?}"),
        };
        let text = metrics
            .iter()
            .filter(|(name, _, _)| !name.contains(".d-upper."))
            .map(|(name, _, values)| {
                format!(
                    "{}: {}",
                    name.trim_start_matches(LATENCY_PREFIX),
                    summary(values)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        input.findings.push((
            "h9-segments".to_owned(),
            Requirement::Required,
            Verdict::Observed,
            format!(
                "дисплей {} ({}); {text} мс; время до фотона не измерено",
                stand.prelude.variant,
                hz.map_or_else(|| "частота не измерена".to_owned(), |hz| format!("{hz:.1} Гц по displayLink"))
            ),
        ));
        let frames = [Load::Quiet, Load::Background]
            .map(|load| {
                let (total, _, found) = counts(load);
                let values: Vec<f64> = metrics
                    .iter()
                    .find(|(name, _, _)| name == &format!("{LATENCY_PREFIX}d-upper.{}", load.name()))
                    .map(|(_, _, values)| values.clone())
                    .unwrap_or_default();
                format!(
                    "{}: блок ответа в IOSurface слоя найден в {found} из {total}, верхняя граница {}",
                    load.name(),
                    summary(&values)
                )
            })
            .join("; ");
        input.findings.push((
            "h9-d-frame".to_owned(),
            Requirement::Research,
            Verdict::Observed,
            format!(
                "{frames} мс; это готовность кадра в IOSurface, а не время до фотона: WindowServer и вывод на экран не входят"
            ),
        ));
        input.findings.push((
            "h9-display-rate".to_owned(),
            Requirement::Required,
            Verdict::Observed,
            format!(
                "displayLink ({}): {} вызовов, {}; окно на {} дисплее",
                lat.rate_source.unwrap_or("нет"),
                lat.timestamps.len(),
                hz.map_or_else(|| "частота не измерена".to_owned(), |hz| format!("{hz:.1} Гц")),
                if lat.builtin { "встроенном" } else { "внешнем" }
            ),
        ));
        let (self_passed, self_reason) = lat.self_check.as_ref().map_or_else(
            || (false, "самопроверка не состоялась".to_owned()),
            |check| (check.passed, check.reason.clone()),
        );
        input.findings.push((
            "h9-n-self-check".to_owned(),
            Requirement::Required,
            if self_passed {
                Verdict::Pass
            } else {
                Verdict::PendingManual
            },
            format!(
                "{self_reason}{}",
                if self_passed {
                    ""
                } else {
                    "; без самопроверки вердикт по 16 мс не выносится, повтор без щита в задаче 19"
                }
            ),
        ));
        input.latency = Some(LatencyMarks {
            display: stand.prelude.variant.clone(),
            builtin: lat.builtin,
            measured_hz: hz,
            self_check: self_passed,
            self_check_reason: self_reason,
        });
        input.observe("latency_keys", &lat.keys);
        input.observe("latency_navigation", &lat.navs);
        input.observe("latency_background", &lat.coverage);
        input.observe("latency_self_check", &lat.self_check);
        input.observe("latency_display_link", &lat.timestamps);
        input.observe("latency_display_link_source", lat.rate_source);
        input.metrics.extend(metrics);
        Ok(())
    });
}

fn short(nonce: &str) -> String {
    nonce
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .rev()
        .take(6)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Replied;
    use crate::keys::KeyInput;

    fn probe(t0: u64, t1: u64, hit: bool) -> Probe {
        Probe { t0, t1, hit }
    }

    #[test]
    fn bounds_from_last_miss_to_first_hit() {
        let probes = [
            probe(1_000, 1_100, false),
            probe(2_000, 2_150, false),
            probe(3_500, 3_600, true),
            probe(4_500, 4_600, true),
        ];
        let found = bounds(500, &probes).unwrap();
        assert_eq!(
            found,
            Bounds {
                lower: 2_000,
                upper: 3_600
            }
        );
        // Погрешность шага: промежуток между опросами плюс длительность опроса с ответом.
        assert_eq!(found.step_ns(), (3_500 - 2_000) + (3_600 - 3_500));
    }

    #[test]
    fn hit_on_first_probe_starts_at_since() {
        let found = bounds(700, &[probe(1_000, 1_200, true)]).unwrap();
        assert_eq!(
            found,
            Bounds {
                lower: 700,
                upper: 1_200
            }
        );
    }

    #[test]
    fn never_hit_gives_no_bounds() {
        assert_eq!(bounds(0, &[probe(1, 2, false), probe(3, 4, false)]), None);
        assert_eq!(bounds(0, &[]), None);
    }

    #[test]
    fn uneven_gaps_are_recorded() {
        let probes = [
            probe(1_000_000, 1_100_000, false),
            probe(2_000_000, 2_100_000, false),
            probe(6_000_000, 6_100_000, true),
        ];
        assert_eq!(gaps_ms(&probes), vec![1.0, 4.0]);
        assert_eq!(bounds(0, &probes).unwrap().step_ns(), 4_100_000);
    }

    fn read(seq: u64, t_ns: u64, replied: &[u64]) -> Receipt {
        Receipt {
            seq,
            t_ns,
            event: Event::Read {
                index: seq,
                hex: "7a".to_owned(),
                replied: replied
                    .iter()
                    .map(|id| Replied {
                        id: *id,
                        t_ns: t_ns + 10,
                    })
                    .collect(),
            },
        }
    }

    #[test]
    fn receipts_match_events_by_reply_id() {
        let receipts = vec![
            read(1, 100, &[]),
            read(2, 200, &[7]),
            Receipt {
                seq: 3,
                t_ns: 250,
                event: Event::Ack {
                    id: 8,
                    bytes: 3,
                    error: None,
                },
            },
            read(4, 300, &[8]),
        ];
        assert_eq!(read_for_reply(&receipts, 7), Ok(Some(200)));
        assert_eq!(read_for_reply(&receipts, 8), Ok(Some(300)));
        assert_eq!(read_for_reply(&receipts, 9), Ok(None));
        let doubled = vec![read(1, 100, &[5]), read(2, 200, &[5])];
        assert!(read_for_reply(&doubled, 5).is_err_and(|reason| reason.contains("два чтения")));
    }

    fn sent(from: &'static str, at_ns: u64, done_ns: u64) -> Sent {
        Sent {
            from,
            input: KeyInput::committed(crate::keys::Action::Press, "z"),
            handled: Ok(true),
            at_ns,
            done_ns,
        }
    }

    #[test]
    fn surface_key_inside_send_event_window() {
        let log = [
            sent("keyDown", 50, 60),
            sent("keyUp", 120, 125),
            sent("keyDown", 150, 190),
            sent("keyDown", 400, 410),
        ];
        assert_eq!(surface_key_ns(&log, 100, 300), Some(40));
        assert_eq!(surface_key_ns(&log, 200, 300), None);
    }

    #[test]
    fn percentiles_per_segment_skip_warmup() {
        let values: Vec<(bool, Option<f64>)> = (0..REPS)
            .map(|index| {
                (
                    index < WARMUP,
                    Some(if index < WARMUP { 1000.0 } else { index as f64 }),
                )
            })
            .chain([(false, None)])
            .collect();
        let kept = measured(&values);
        assert_eq!(kept.len(), MEASURED);
        let summary = crate::stats::summarize(&kept).unwrap();
        assert_eq!(summary.count, MEASURED);
        assert_eq!(summary.max, (REPS - 1) as f64);
        assert_eq!(summary.p50, (WARMUP + MEASURED / 2 - 1) as f64);
        assert_eq!(summary.p95, (WARMUP + 95 - 1) as f64);
    }

    #[test]
    fn display_rate_from_link_timestamps() {
        let at_120: Vec<f64> = (0..121).map(|index| f64::from(index) / 120.0).collect();
        assert!((rate_hz(&at_120).unwrap() - 120.0).abs() < 1e-6);
        let mut skipped: Vec<f64> = (0..61).map(|index| f64::from(index) / 60.0).collect();
        skipped.remove(30);
        assert!((rate_hz(&skipped).unwrap() - 60.0).abs() < 1e-6);
        assert_eq!(rate_hz(&[1.0]), None);
    }

    #[test]
    fn answer_draws_block_and_tag() {
        let text = answer("lt q007xabc", Rgb(0, 200, 0));
        assert!(text.starts_with("\x1b[1;1H\x1b[48;2;0;200;0m"));
        assert_eq!(
            text.matches("\x1b[48;2;0;200;0m").count(),
            usize::from(BLOCK_ROWS)
        );
        assert!(text.ends_with("\x1b[6;1H\x1b[2Klt q007xabc"));
        assert!(text.len() < 4096);
    }

    #[test]
    fn block_points_stay_inside_block() {
        let points = block_points(16, 30);
        for (x, y) in points {
            assert!(x > 0 && x < 16 * usize::from(BLOCK_COLS));
            assert!(y > 30 && y < 30 * usize::from(BLOCK_ROWS));
        }
    }

    #[test]
    fn displays_match_run_script() {
        let script = include_str!("../../run.sh");
        assert!(script.contains("latency) echo \"${LATENCY_DISPLAYS:-builtin}\" ;;"));
        assert_eq!(DISPLAYS, ["builtin", "external", "builtin60"]);
    }
}
