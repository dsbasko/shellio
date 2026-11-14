//! Сценарий `visibility` (H7a, H7b): матрица видимости в сцене четырёх зон. Вариант процесса задаёт
//! набор обходов agterm (`base`, `bounds`, `defer`, `syncdraw`, `all`), каждая клетка отмечает его.
//!
//! Клетка: прогрев (новый маркер фикстуры виден в текущей `IOSurface` слоя) → скрытие своим способом
//! → установление скрытия (модель подала `false`, способ дошёл до состояния) → серия замеров
//! счётчиков задачи 11 → нагрузка клетки → маркер другого цвета, подтверждённый в модели
//! терминала → окно тишины → показ → свежий кадр по маркеру и размер кадра → серия замеров.
//!
//! Кадры считаются по смене `contents` слоя поверхности (таймер стенда в общих режимах цикла, нижняя
//! граница числа кадров). Свежесть читается из `IOSurface`, которую слой держит как `contents`, а не
//! снимком экрана: снимки в этом окружении показывают только обои (задача 10). Сводные счётчики —
//! диагностика в пределах калибровки задачи 11, вердикты H7a и H7b выносятся по ручной записи
//! Instruments в задаче 19.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::MainThreadOnly;
use objc2::rc::{Retained, Weak};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSView, NSWindow, NSWindowOcclusionState, NSWindowStyleMask,
};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFString, CFType};
use objc2_core_graphics::{
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGWindowBounds, kCGWindowLayer,
    kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID,
};
use objc2_foundation::{NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSTimer};
use objc2_io_surface::{IOSurfaceLockOptions, IOSurfaceRef};
use serde::Serialize;

use super::input::focused;
use super::snapshot;
use super::surface_env::{self, Extra, State};
use super::{Poll, Report, Scenario, Stand, Step, events, poll, run_steps, window};
use crate::clock;
use crate::fixture::Command;
use crate::ghostty::config::ColorScheme;
use crate::ghostty::runtime::{self, Created};
use crate::ghostty::surface::{Phase, SurfaceId};
use crate::gpu::{self, Sample, Series};
use crate::palette;
use crate::results::{Requirement, Verdict};
use crate::scene::{Divider, Rgb, Scene, Zone};
use crate::view::SurfaceView;
use crate::visibility::{self, Front, SyncDraw, Workarounds};

const FOCUS_TIMEOUT: Duration = Duration::from_secs(5);
const SIZE_TIMEOUT: Duration = Duration::from_secs(3);
const READY_TIMEOUT: Duration = Duration::from_secs(12);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Внутренний предел состояния способа скрытия и показа: по нему клетка записывает, что состояние
/// не наступило, а не роняет прогон. Шаг ждёт дольше.
const STATE_LIMIT: Duration = Duration::from_secs(5);
const STATE_TIMEOUT: Duration = Duration::from_secs(8);
/// Внутренний предел свежего кадра после показа.
const FRESH_LIMIT: Duration = Duration::from_secs(3);
const FRESH_TIMEOUT: Duration = Duration::from_secs(6);
/// Прогрев видимой поверхности: маркер обязан появиться в кадре, иначе стенд не знает состояния.
const WARM_TIMEOUT: Duration = Duration::from_secs(6);
const MODEL_TIMEOUT: Duration = Duration::from_secs(6);
const SERIES_TIMEOUT: Duration = Duration::from_secs(15);
/// Окно наблюдения скрытой поверхности после маркера: курсор мигает (`cursor-style-blink`), и
/// рисующая поверхность дала бы кадр.
const QUIET: Duration = Duration::from_millis(1200);
const QUIET_TIMEOUT: Duration = Duration::from_secs(4);
const LONG_HIDE: Duration = Duration::from_secs(10);
const LONG_TIMEOUT: Duration = Duration::from_secs(16);
const DURING_TIMEOUT: Duration = Duration::from_secs(12);
const OVERLAY_WINDOW: Duration = Duration::from_millis(600);
/// Сколько ждать бита `Visible` у `occlusionState`, прежде чем искать щит поверх окна.
const OCCLUSION_LIMIT: Duration = Duration::from_secs(3);
/// Поток вывода скрытой поверхности: 3 с по строке в миллисекунду, OSC каждые 25 строк.
const STREAM: Command = Command::Stream {
    lines: 3_000,
    interval_us: 1_000,
    columns: 48,
    osc_every: 25,
    report_every: 500,
};
const REPARENT_CYCLES: usize = 6;
const DIVIDER_SHIFT: f64 = 120.0;
const COVER_MARGIN: f64 = 60.0;
const WATCH_INTERVAL: f64 = 0.002;
const WATCH_LIMIT: usize = 50_000;
/// Допуск совпадения цвета маркера в кадре.
const NEAR: u8 = 60;
const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const TITLE: &str = "терминал";
const COLD_TITLE: &str = "холодная";
const STUB_TITLE: &str = "заглушка";
/// Цвета маркеров: соседние в списке далеки друг от друга, клетка берёт два подряд.
const COLORS: [Rgb; 8] = [
    Rgb(230, 40, 40),
    Rgb(40, 80, 230),
    Rgb(240, 200, 0),
    Rgb(0, 200, 210),
    Rgb(210, 0, 210),
    Rgb(0, 170, 60),
    Rgb(255, 120, 0),
    Rgb(120, 40, 200),
];
/// Счётчики калибровки, годные для задачи 12, и диагностика жизни текстур.
const COUNTERS: [&str; 5] = [
    "footprint.footprint",
    "footprint.phys_footprint",
    "footprint.IOSurface.dirty",
    "footprint.Owned physical footprint (unmapped).dirty",
    "metal.current_allocated_size",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Mechanism {
    /// Другая вкладка плитки: вид уходит из окна.
    Tab,
    /// Плитка зоны скрыта `setHidden`: вид в окне, предок скрыт, границы прежние.
    Zone,
    Miniaturize,
    OrderOut,
    /// Окно перекрыто другим непрозрачным окном.
    Covered,
    AppHidden,
    /// Многократная короткая смена родителя с уходом из окна.
    Reparent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum During {
    Nothing,
    ResizeHidden,
    Long,
    Output,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum Who {
    A,
    C,
}

struct CellSpec {
    name: &'static str,
    who: Who,
    mechanism: Mechanism,
    during: During,
    note: &'static str,
}

const CELLS: [CellSpec; 11] = [
    CellSpec {
        name: "tab",
        who: Who::A,
        mechanism: Mechanism::Tab,
        during: During::Nothing,
        note: "скрытая вкладка прогретой поверхности",
    },
    CellSpec {
        name: "zone",
        who: Who::A,
        mechanism: Mechanism::Zone,
        during: During::Nothing,
        note: "скрытая зона: плитка setHidden, вид в окне",
    },
    CellSpec {
        name: "miniaturize",
        who: Who::A,
        mechanism: Mechanism::Miniaturize,
        during: During::Nothing,
        note: "свёрнутое окно",
    },
    CellSpec {
        name: "order-out",
        who: Who::A,
        mechanism: Mechanism::OrderOut,
        during: During::Nothing,
        note: "убранное окно (orderOut)",
    },
    CellSpec {
        name: "covered",
        who: Who::A,
        mechanism: Mechanism::Covered,
        during: During::Nothing,
        note: "окно перекрыто другим непрозрачным окном",
    },
    CellSpec {
        name: "app-hidden",
        who: Who::A,
        mechanism: Mechanism::AppHidden,
        during: During::Nothing,
        note: "скрытие и показ приложения",
    },
    CellSpec {
        name: "cold-tab",
        who: Who::C,
        mechanism: Mechanism::Tab,
        during: During::Nothing,
        note: "скрытие до первого кадра: новая поверхность C уходит за вкладку сразу после ghostty_surface_new; прогретая — клетка tab",
    },
    CellSpec {
        name: "zone-resize",
        who: Who::A,
        mechanism: Mechanism::Zone,
        during: During::ResizeHidden,
        note: "скрытие → ресайз скрытого вида (делитель) → показ; вид в окне",
    },
    CellSpec {
        name: "reparent",
        who: Who::A,
        mechanism: Mechanism::Reparent,
        during: During::Nothing,
        note: "многократная короткая смена родителя: снятие из окна и вставка в другую зону по одному шагу таймера",
    },
    CellSpec {
        name: "long",
        who: Who::A,
        mechanism: Mechanism::Tab,
        during: During::Long,
        note: "длительное скрытие за вкладкой",
    },
    CellSpec {
        name: "output",
        who: Who::A,
        mechanism: Mechanism::Tab,
        during: During::Output,
        note: "скрытая поверхность с непрерывным выводом за вкладкой",
    },
];

// ---- кадр слоя ----

/// Кадр, который слой держит как `contents`.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Frame {
    surface_id: u32,
    contents: usize,
    width: usize,
    height: usize,
    samples: Vec<Rgb>,
}

fn contents_ptr(view: &SurfaceView) -> usize {
    view.layer()
        // SAFETY: чтение свойства слоя на главном потоке; объект не разыменовывается.
        .and_then(|layer| unsafe { layer.contents() })
        .map_or(0, |contents| Retained::as_ptr(&contents) as usize)
}

/// Точки выборки внутри маркера, нарисованного в сетке размера `area`. Сетка привязана к левому
/// верхнему углу, поэтому в большем кадре маркер остаётся на тех же пикселях.
fn marker_points(width: usize, height: usize, area: Option<(u32, u32)>) -> Vec<(usize, usize)> {
    let (w, h) = area.map_or((width, height), |(w, h)| {
        (width.min(w as usize), height.min(h as usize))
    });
    sample_points(w, h)
}

/// Точки выборки: центр кадра и четыре точки вокруг, все внутри маркера на половину экрана.
fn sample_points(width: usize, height: usize) -> Vec<(usize, usize)> {
    let (cx, cy) = (width / 2, height / 2);
    let (dx, dy) = (width / 10, height / 10);
    vec![
        (cx, cy),
        (cx - dx, cy - dy),
        (cx + dx, cy - dy),
        (cx - dx, cy + dy),
        (cx + dx, cy + dy),
    ]
}

/// Читает `IOSurface` слоя поверхности без записи. `None` — у слоя нет `contents`. `area` —
/// размер вида, при котором рисовался маркер.
fn read_frame(view: &SurfaceView, area: Option<(u32, u32)>) -> Result<Option<Frame>, String> {
    let Some(layer) = view.layer() else {
        return Ok(None);
    };
    // SAFETY: чтение свойства слоя на главном потоке.
    let Some(contents) = (unsafe { layer.contents() }) else {
        return Ok(None);
    };
    // SAFETY: объекты Foundation бесплатно мостятся в Core Foundation; тип проверяется ниже.
    let object: &CFType = unsafe { &*Retained::as_ptr(&contents).cast::<CFType>() };
    let surface = object.downcast_ref::<IOSurfaceRef>().ok_or_else(|| {
        format!(
            "contents слоя поверхности не IOSurface, а {}",
            contents.class().name().to_string_lossy()
        )
    })?;
    let format = surface.pixel_format();
    if format != BGRA {
        return Err(format!(
            "формат IOSurface слоя {format:#x}, стенд читает только BGRA"
        ));
    }
    let (width, height, row) = (surface.width(), surface.height(), surface.bytes_per_row());
    if width < 10 || height < 10 {
        return Ok(Some(Frame {
            surface_id: surface.id(),
            contents: Retained::as_ptr(&contents) as usize,
            width,
            height,
            samples: Vec::new(),
        }));
    }
    // SAFETY: значение seed не запрашивается, указатель NULL допустим.
    let status = unsafe { surface.lock(IOSurfaceLockOptions::ReadOnly, ptr::null_mut()) };
    if status != 0 {
        return Err(format!("IOSurfaceLock вернул {status}"));
    }
    let base = surface.base_address().as_ptr().cast::<u8>();
    let samples = marker_points(width, height, area)
        .into_iter()
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
    Ok(Some(Frame {
        surface_id: surface.id(),
        contents: Retained::as_ptr(&contents) as usize,
        width,
        height,
        samples,
    }))
}

/// Цвет sRGB в кодированных значениях Display P3 (та же передаточная функция). Ghostty может
/// рисовать в `IOSurface` с цветовым пространством окна, поэтому маркер сравнивается с обоими.
fn display_p3(rgb: Rgb) -> Rgb {
    let decode = |value: u8| {
        let c = f64::from(value) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let encode = |c: f64| {
        let c = c.clamp(0.0, 1.0);
        let v = if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        (v * 255.0).round() as u8
    };
    let (r, g, b) = (decode(rgb.0), decode(rgb.1), decode(rgb.2));
    Rgb(
        encode(0.822_5 * r + 0.177_4 * g),
        encode(0.033_2 * r + 0.966_9 * g),
        encode(0.017_1 * r + 0.072_4 * g + 0.910_8 * b),
    )
}

fn near(sample: Rgb, expected: Rgb) -> bool {
    snapshot::distance(sample, expected) <= NEAR
        || snapshot::distance(sample, display_p3(expected)) <= NEAR
}

/// Что показывает кадр: маркер после скрытия, маркер до него или другое.
fn classify(frame: Option<&Frame>, fresh: Rgb, stale: Rgb) -> &'static str {
    let Some(frame) = frame else {
        return "none";
    };
    if frame.samples.is_empty() {
        "empty"
    } else if frame.samples.iter().all(|rgb| near(*rgb, fresh)) {
        "fresh"
    } else if frame.samples.iter().all(|rgb| near(*rgb, stale)) {
        "stale"
    } else {
        "other"
    }
}

// ---- наблюдатель кадров ----

/// Смены `contents` слоя видов поверхности, снятые таймером стенда.
#[derive(Default)]
struct Watch {
    views: Vec<(Who, Weak<SurfaceView>, usize)>,
    changes: Vec<(Who, u64)>,
    ticks: u64,
}

impl Watch {
    fn add(&mut self, who: Who, view: &SurfaceView) {
        self.views.push((who, Weak::new(view), contents_ptr(view)));
    }

    fn tick(&mut self) {
        self.ticks += 1;
        let now = clock::uptime_ns();
        for (who, weak, last) in &mut self.views {
            let Some(view) = weak.load() else {
                continue;
            };
            let current = contents_ptr(&view);
            if current != *last {
                *last = current;
                if self.changes.len() < WATCH_LIMIT {
                    self.changes.push((*who, now));
                }
            }
        }
    }

    fn between(&self, who: Who, from: u64, to: u64) -> usize {
        self.changes
            .iter()
            .filter(|(changed, at)| *changed == who && *at >= from && *at < to)
            .count()
    }
}

fn start_watch(watch: &Rc<RefCell<Watch>>) -> Retained<NSTimer> {
    let watch = Rc::clone(watch);
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| watch.borrow_mut().tick());
    // SAFETY: таймер добавляется в цикл главного потока и снимается там же, поэтому блок
    // вызывается и освобождается только на главном потоке.
    let timer =
        unsafe { NSTimer::timerWithTimeInterval_repeats_block(WATCH_INTERVAL, true, &block) };
    // SAFETY: `NSRunLoopCommonModes` — неизменяемая константа Foundation.
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    timer
}

// ---- щит поверх окна ----

/// Чужое окно выше окна пробы, которое накрывает его целиком.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct Shield {
    owner: String,
    pid: i64,
    layer: i64,
    bounds: [f64; 4],
    own_layer: i64,
    own_bounds: [f64; 4],
}

struct WindowEntry {
    number: i64,
    pid: i64,
    layer: i64,
    owner: String,
    bounds: [f64; 4],
}

fn number_of(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<f64> {
    dict.get(key)
        .and_then(|value| value.downcast_ref::<CFNumber>().and_then(CFNumber::as_f64))
}

fn window_entry(dict: &CFDictionary<CFString, CFType>) -> Option<WindowEntry> {
    // SAFETY: ключи — неизменяемые константы Core Graphics.
    let (number, pid, layer, owner, bounds) = unsafe {
        (
            kCGWindowNumber,
            kCGWindowOwnerPID,
            kCGWindowLayer,
            kCGWindowOwnerName,
            kCGWindowBounds,
        )
    };
    let rect = dict.get(bounds)?;
    let rect = rect.downcast_ref::<CFDictionary>()?;
    // SAFETY: словарь границ окна — строки X, Y, Width, Height с числами.
    let rect: &CFDictionary<CFString, CFType> = unsafe { rect.cast_unchecked() };
    let field = |name: &'static str| number_of(rect, &CFString::from_static_str(name));
    Some(WindowEntry {
        number: number_of(dict, number)? as i64,
        pid: number_of(dict, pid)? as i64,
        layer: number_of(dict, layer)? as i64,
        owner: dict
            .get(owner)
            .and_then(|value| {
                value
                    .downcast_ref::<CFString>()
                    .map(|name| name.to_string())
            })
            .unwrap_or_default(),
        bounds: [field("X")?, field("Y")?, field("Width")?, field("Height")?],
    })
}

/// `outer` накрывает `inner` целиком (координаты Core Graphics).
fn covers(outer: [f64; 4], inner: [f64; 4]) -> bool {
    outer[0] <= inner[0]
        && outer[1] <= inner[1]
        && outer[0] + outer[2] >= inner[0] + inner[2]
        && outer[1] + outer[3] >= inner[1] + inner[3]
}

/// Окно другого процесса на более высоком уровне, накрывающее окно пробы, по списку окон на экране.
fn foreign_cover(window: &NSWindow) -> Option<Shield> {
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, 0)?;
    // SAFETY: элементы списка окон — словари со строковыми ключами.
    let list: &CFArray<CFDictionary<CFString, CFType>> = unsafe { list.cast_unchecked() };
    let entries: Vec<WindowEntry> = list.iter().filter_map(|dict| window_entry(&dict)).collect();
    let own_pid = i64::from(std::process::id());
    let own = entries
        .iter()
        .find(|entry| entry.number == window.windowNumber() as i64)?;
    entries
        .iter()
        .find(|entry| {
            entry.pid != own_pid && entry.layer > own.layer && covers(entry.bounds, own.bounds)
        })
        .map(|entry| Shield {
            owner: entry.owner.clone(),
            pid: entry.pid,
            layer: entry.layer,
            bounds: entry.bounds,
            own_layer: own.layer,
            own_bounds: own.bounds,
        })
}

// ---- итог клетки ----

#[derive(Clone, Debug, Default, Serialize)]
struct Counters {
    before: BTreeMap<String, i64>,
    hidden: BTreeMap<String, i64>,
    shown: BTreeMap<String, i64>,
    hidden_settled: bool,
    shown_settled: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
struct CellRecord {
    name: &'static str,
    mechanism: Option<Mechanism>,
    note: &'static str,
    who: Option<Who>,
    surface: Option<String>,
    workarounds: Vec<&'static str>,
    /// `occlusionState` исключён стендом из признаков: окно под щитом.
    occlusion_masked: bool,
    /// Оценок видимости у вида до скрытия и к установлению скрытия.
    evaluations_at_hide: u64,
    evaluations_established: u64,
    pre_color: Option<Rgb>,
    post_color: Option<Rgb>,
    /// Размер вида в пикселях, когда рисовался маркер после скрытия: сетка скрытого вида
    /// может быть меньше кадра при показе.
    marker_pixels: Option<(u32, u32)>,
    /// У слоя было `contents` в момент скрытия.
    contents_at_hide: bool,
    hidden_state_reached: bool,
    hidden_state: String,
    /// Модель подала `false` к установлению скрытия.
    model_hidden: bool,
    /// Скрытие отложено обходом `defer` в момент скрытия.
    deferred_at_hide: bool,
    established_ms: Option<f64>,
    /// Смены `contents` от скрытия до показа и после установления скрытия.
    frames_hidden_total: usize,
    frames_hidden_late: usize,
    during: Option<String>,
    at_show: String,
    at_first_pass: Option<String>,
    shown_state_reached: bool,
    shown_state: String,
    fresh: bool,
    fresh_ms: Option<f64>,
    frame: Option<Frame>,
    view_pixels: (u32, u32),
    size_matches: bool,
    fronts: Vec<Front>,
    sync_draws: Vec<SyncDraw>,
    counters: Counters,
}

/// Палитра частично перекрывает A.
#[derive(Clone, Debug, Default, Serialize)]
struct Overlay {
    fronts_during: usize,
    model_visible: bool,
    window_occlusion_visible: bool,
    overlay_signal: bool,
    palette_open: bool,
}

struct Cur {
    spec: &'static CellSpec,
    record: CellRecord,
    hide_ns: u64,
    established_ns: Option<u64>,
    quiet_from: Option<Instant>,
    state_since: Instant,
    fed_from: usize,
    sync_from: usize,
    nonce: String,
    pending: Vec<u64>,
    stream: Option<u64>,
    reparent_steps: usize,
    resize_from: (u32, u32),
    long_until: Option<Instant>,
    first_pass: Rc<RefCell<Option<String>>>,
    show_at: Option<Instant>,
}

struct Vis {
    env: State,
    scene: Scene,
    workarounds: Workarounds,
    devices: Rc<Vec<gpu::Device>>,
    renderer: usize,
    watch: Rc<RefCell<Watch>>,
    watch_timer: Option<Retained<NSTimer>>,
    stub: Retained<NSView>,
    c: Option<Extra>,
    color: usize,
    /// Последний маркер, подтверждённый в кадре поверхности.
    shown: [Option<Rgb>; 2],
    cells: Vec<CellRecord>,
    cur: Option<Cur>,
    series: Option<Series>,
    series_result: Option<Result<Sample, String>>,
    last_sample: Option<Sample>,
    initial: Option<Sample>,
    roundtrips_ms: Vec<f64>,
    cover: Option<Retained<NSWindow>>,
    overlay: Overlay,
    overlay_from: Option<(Instant, usize)>,
    home_divider: f64,
    warm: Option<(Who, Rgb, String, Vec<u64>)>,
    out: std::path::PathBuf,
    /// Чужое окно поверх окна пробы; при нём `occlusionState` исключён из признаков.
    shield: Option<Shield>,
}

type Shared = Rc<RefCell<Vis>>;

fn index(who: Who) -> usize {
    match who {
        Who::A => 0,
        Who::C => 1,
    }
}

impl Vis {
    fn view(&self, who: Who) -> Option<Retained<SurfaceView>> {
        match who {
            Who::A => Some(self.env.view.clone()),
            Who::C => self.c.as_ref().map(|c| c.view.clone()),
        }
    }

    fn id(&self, who: Who) -> Option<SurfaceId> {
        match who {
            Who::A => Some(self.env.id),
            Who::C => self.c.as_ref().map(|c| c.id),
        }
    }

    fn nonce(&self, who: Who) -> Option<String> {
        match who {
            Who::A => Some(self.env.fixture_nonce.clone()),
            Who::C => self.c.as_ref().map(|c| c.fixture_nonce.clone()),
        }
    }

    fn next_color(&mut self) -> Rgb {
        let color = COLORS[self.color % COLORS.len()];
        self.color += 1;
        color
    }

    /// Маркер на половину экрана и nonce в нижней строке; номера команд.
    fn paint(&mut self, who: Who, rgb: Rgb, nonce: &str) -> Result<Vec<u64>, String> {
        let id = self.id(who).ok_or("поверхности нет")?;
        let fixture = self.nonce(who).ok_or("фикстуры нет")?;
        let size = runtime::read(id).ok_or("поверхность не живая")?.size;
        let (rows, cols) = (size.rows.max(4), size.columns.max(4));
        let marker = Command::Marker {
            row: rows / 4 + 1,
            col: cols / 4 + 1,
            rows: rows / 2,
            cols: cols / 2,
            rgb: [rgb.0, rgb.1, rgb.2],
        };
        let print = Command::Print {
            text: format!("\r{nonce}\x1b[K"),
        };
        Ok(vec![
            self.env.hub.send(&fixture, marker)?,
            self.env.hub.send(&fixture, print)?,
        ])
    }

    /// Все команды подтверждены и nonce виден в модели терминала.
    fn confirmed(&mut self, who: Who, commands: &[u64], nonce: &str) -> Result<bool, String> {
        self.env.hub.poll();
        let fixture = self.nonce(who).ok_or("фикстуры нет")?;
        let Some(conn) = self.env.hub.by_nonce(&fixture) else {
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
        let id = self.id(who).ok_or("поверхности нет")?;
        Ok(runtime::viewport_text(id).is_some_and(|text| text.contains(nonce)))
    }

    fn fed_len(&self, who: Who) -> (usize, usize) {
        self.view(who)
            .and_then(|view| view.visibility_log())
            .map_or((0, 0), |log| (log.fed.len(), log.sync_draws.len()))
    }

    /// Новая серия замеров счётчиков.
    fn start_series(&mut self, label: &str) {
        self.series_result = None;
        self.series = Some(Series::start(
            &self.out,
            std::process::id() as i32,
            label,
            Rc::clone(&self.devices),
            self.renderer,
        ));
    }

    fn poll_series(&mut self) -> Poll<()> {
        if self.series_result.is_some() {
            return Poll::Ready(());
        }
        let Some(series) = self.series.as_mut() else {
            self.series_result = Some(Err("серии замеров нет".to_owned()));
            return Poll::Ready(());
        };
        match series.poll() {
            Some(result) => {
                self.roundtrips_ms.append(&mut series.roundtrips_ms);
                self.series_result = Some(result);
                self.series = None;
                Poll::Ready(())
            }
            None => Poll::Pending(series.describe()),
        }
    }

    fn take_sample(&mut self) -> Result<Sample, String> {
        let sample = self.series_result.take().ok_or("замера нет".to_owned())??;
        self.last_sample = Some(sample.clone());
        Ok(sample)
    }
}

fn selected(sample: &Sample) -> BTreeMap<String, i64> {
    COUNTERS
        .iter()
        .filter_map(|name| {
            sample
                .counters
                .get(*name)
                .map(|value| ((*name).to_owned(), *value))
        })
        .collect()
}

fn home(who: Who) -> Zone {
    match who {
        Who::A => Zone::Center,
        Who::C => Zone::Right,
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Visibility);
    let variant = stand.prelude.variant.clone();
    report.push("variant", &variant);
    let Some(workarounds) = Workarounds::from_variant(&variant) else {
        let reason = format!(
            "предусловие: вариант {variant}, ожидается один из {:?}",
            Workarounds::VARIANTS
        );
        return stand.conclude(report, Err(reason));
    };
    report.push_json("workarounds", &workarounds.names());
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
        "visibility_note",
        "кадры — смены contents слоя (нижняя граница), свежесть — пиксели IOSurface слоя по маркеру фикстуры; счётчики — диагностика в пределах калибровки задачи 11; вердикты H7a и H7b — по записи Instruments в задаче 19",
    );
    visibility::enable(workarounds);
    let devices = gpu::devices();
    let traits: Vec<gpu::DeviceTraits> = devices.iter().map(|d| d.traits.clone()).collect();
    let Some(renderer) = gpu::renderer_choice(&traits) else {
        return stand.conclude(report, Err("нет устройства Metal для рендера".to_owned()));
    };
    report.push_json("gpu_devices", &traits);
    let mtm = stand.view.mtm();
    let scene = Scene::new(mtm, false);
    scene.install(&stand.view, &stand.window);
    let env = surface_env::prepare_in(&stand, &mut report, ColorScheme::Light, |view| {
        scene.add_tab(Zone::Center, 0, TITLE, view)
    });
    let env = match env {
        Ok(env) => env,
        Err(reason) => return surface_env::finish(&stand, report, Err(reason), None),
    };
    scene.sync_focus(&stand.window);
    let home_divider = scene.divider(Divider::LeftCenter);
    let out = stand.prelude.out.clone();
    let vis = Rc::new(RefCell::new(Vis {
        env,
        scene,
        workarounds,
        devices: Rc::new(devices),
        renderer,
        watch: Rc::new(RefCell::new(Watch::default())),
        watch_timer: None,
        stub: NSView::new(mtm),
        c: None,
        color: 0,
        shown: [None, None],
        cells: Vec::new(),
        cur: None,
        series: None,
        series_result: None,
        last_sample: None,
        initial: None,
        roundtrips_ms: Vec::new(),
        cover: None,
        overlay: Overlay::default(),
        overlay_from: None,
        home_divider,
        warm: None,
        out,
        shield: None,
    }));
    let steps = steps(&stand, &vis);
    run_steps(steps, move |result| {
        let verdict = match result {
            Ok(()) => {
                judge(&vis.borrow(), &mut report);
                Ok(())
            }
            Err(failure) => Err(failure.describe()),
        };
        let views = cleanup(&stand, &vis, &mut report);
        surface_env::finish_views(&stand, report, verdict, views);
    });
}

/// Возвращает сцену в исходное состояние и пишет данные клеток, в том числе при ошибке.
fn cleanup(stand: &Stand, vis: &Shared, report: &mut Report) -> Vec<Retained<SurfaceView>> {
    let mut vis = vis.borrow_mut();
    if let Some(timer) = vis.watch_timer.take() {
        timer.invalidate();
    }
    if let Some(cover) = vis.cover.take() {
        cover.orderOut(None);
    }
    if palette::is_open() {
        palette::close(palette::Close::Command);
    }
    let _ = palette::uninstall();
    if stand.app.isHidden() {
        stand.app.unhide(None);
    }
    if let Some(cur) = vis.cur.take() {
        let mut record = cur.record;
        record.during = Some(format!(
            "{} (клетка прервана)",
            record.during.unwrap_or_default()
        ));
        vis.cells.push(record);
    }
    report.push_json("visibility_cells", &vis.cells);
    report.push_json("visibility_overlay", &vis.overlay);
    report.push_json("visibility_shield", &vis.shield);
    report.push("occlusion_masked", visibility::occlusion_masked());
    report.push_json(
        "visibility_initial_counters",
        &vis.initial.as_ref().map(selected),
    );
    report.metric("footprint_roundtrip", "ms", vis.roundtrips_ms.clone());
    let ticks = vis.watch.borrow().ticks;
    report.push("frame_watch_ticks", ticks);
    let mut views = vec![vis.env.view.clone()];
    for who in [Who::A, Who::C] {
        if let Some(view) = vis.view(who) {
            report.push_json(
                &format!("visibility_log_{}", index(who)),
                &view.visibility_log(),
            );
            if who == Who::C {
                views.push(view);
            }
        }
    }
    views
}

fn steps(stand: &Rc<Stand>, vis: &Shared) -> Vec<Step> {
    let mut steps = Vec::new();
    let (focus, focus_vis) = (Rc::clone(stand), Rc::clone(vis));
    steps.push(Step::new(
        "активность, key window и фокус вида поверхности",
        FOCUS_TIMEOUT,
        move || focused(&focus, &focus_vis.borrow().env.view),
        || Ok(()),
    ));
    let masking = Rc::clone(vis);
    steps.push(shield_step(stand, move |shield| {
        masking.borrow_mut().shield = Some(shield);
    }));
    let (sized, creating) = (Rc::clone(vis), Rc::clone(vis));
    steps.push(Step::new(
        "ненулевой размер вида в пикселях",
        SIZE_TIMEOUT,
        move || {
            let pixels = sized.borrow().env.view.pixel_size();
            poll(pixels.0 > 0 && pixels.1 > 0, || {
                format!("размер вида в пикселях {pixels:?}")
            })
        },
        move || {
            let id = creating.borrow().env.id;
            match runtime::create(id)? {
                Created::Live { key, new_ms } => {
                    events::note(format!("поверхность {key} создана за {new_ms:.1} мс"));
                    Ok(())
                }
                Created::Failed { key, failure, .. } => Err(format!(
                    "ghostty_surface_new вернул NULL для {key}: {failure:?}"
                )),
                Created::Waiting(decision) => {
                    Err(format!("реестр не разрешил создание: {decision:?}"))
                }
            }
        },
    ));
    let (ready, started) = (Rc::clone(vis), Rc::clone(vis));
    steps.push(Step::new(
        "готовность фикстуры A",
        READY_TIMEOUT,
        move || surface_env::ready(&mut ready.borrow_mut().env),
        move || {
            let mut vis = started.borrow_mut();
            let view = vis.env.view.clone();
            vis.watch.borrow_mut().add(Who::A, &view);
            let timer = start_watch(&vis.watch);
            vis.watch_timer = Some(timer);
            begin_warm(&mut vis, Who::A, "v-initial")
        },
    ));
    warm_step(&mut steps, vis, "начальный маркер в кадре A");
    let (sampling, recorded) = (Rc::clone(vis), Rc::clone(vis));
    steps.push(Step::new(
        "начальная серия замеров",
        SERIES_TIMEOUT,
        move || sampling.borrow_mut().poll_series(),
        move || {
            let mut vis = recorded.borrow_mut();
            let sample = vis.take_sample()?;
            vis.initial = Some(sample);
            Ok(())
        },
    ));
    overlay_steps(&mut steps, stand, vis);
    for spec in &CELLS {
        cell_steps(&mut steps, stand, vis, spec);
    }
    let closing = Rc::clone(vis);
    steps.push(Step::new(
        "намерение закрыть A",
        CLOSE_TIMEOUT,
        || Poll::Ready(()),
        move || {
            let id = closing.borrow().env.id;
            runtime::request_close(id).map(|_| ())
        },
    ));
    let gone = Rc::clone(vis);
    steps.push(Step::new(
        "освобождение A и выход фикстуры",
        CLOSE_TIMEOUT,
        move || surface_env::freed(&mut gone.borrow_mut().env),
        || Ok(()),
    ));
    steps
}

/// Шаг «окно видимо по `occlusionState` или опознан щит»: под чужим окном поверх окна пробы
/// `occlusionState` исключается из признаков видимости, иначе — ошибка исполнения.
pub(super) fn shield_step(stand: &Rc<Stand>, store: impl FnOnce(Shield) + 'static) -> Step {
    let (occluded, masking_stand) = (Rc::clone(stand), Rc::clone(stand));
    let mut since: Option<Instant> = None;
    Step::new(
        "окно видимо по occlusionState или опознан щит",
        STATE_TIMEOUT,
        move || {
            let started = *since.get_or_insert_with(Instant::now);
            let state = occluded.window.occlusionState();
            let visible = state.contains(NSWindowOcclusionState::Visible);
            poll(visible || started.elapsed() >= OCCLUSION_LIMIT, || {
                format!("occlusionState {:#x}", state.0)
            })
        },
        move || {
            let window = &masking_stand.window;
            let state = window.occlusionState();
            if state.contains(NSWindowOcclusionState::Visible) {
                return Ok(());
            }
            let shield = foreign_cover(window).ok_or_else(|| {
                format!(
                    "предусловие: WindowServer считает окно пробы перекрытым (occlusionState {:#x}, isVisible {}), а чужого окна поверх него в списке окон нет",
                    state.0,
                    window.isVisible()
                )
            })?;
            events::note(format!(
                "окно перекрыто щитом {} (pid {}, уровень {}): occlusionState исключён из признаков",
                shield.owner, shield.pid, shield.layer
            ));
            visibility::mask_occlusion();
            store(shield);
            Ok(())
        },
    )
}

/// Маркер нового цвета на видимой поверхности; подтверждение ждёт [`warm_step`].
fn begin_warm(vis: &mut Vis, who: Who, nonce: &str) -> Result<(), String> {
    let color = vis.next_color();
    let nonce = format!("{nonce}-{}", vis.color);
    let commands = vis.paint(who, color, &nonce)?;
    vis.warm = Some((who, color, nonce, commands));
    Ok(())
}

/// Ждёт маркер прогрева в модели и в кадре, затем начинает серию замеров.
fn warm_step(steps: &mut Vec<Step>, vis: &Shared, name: &'static str) {
    let (waiting, done) = (Rc::clone(vis), Rc::clone(vis));
    steps.push(Step::new(
        name,
        WARM_TIMEOUT,
        move || warm_ready(&mut waiting.borrow_mut()),
        move || {
            let mut vis = done.borrow_mut();
            if let Some((who, color, _, _)) = vis.warm.take() {
                vis.shown[index(who)] = Some(color);
            }
            vis.start_series("initial");
            Ok(())
        },
    ));
}

fn warm_ready(vis: &mut Vis) -> Poll<()> {
    let Some((who, color, nonce, commands)) = vis.warm.clone() else {
        return Poll::Ready(());
    };
    match vis.confirmed(who, &commands, &nonce) {
        Ok(true) => {}
        Ok(false) => return Poll::Pending(format!("маркер {nonce} ещё не в модели")),
        Err(reason) => return Poll::Pending(reason),
    }
    let Some(view) = vis.view(who) else {
        return Poll::Pending("вида нет".to_owned());
    };
    match read_frame(&view, None) {
        Ok(frame) => {
            let pixels = view.pixel_size();
            let class = classify(frame.as_ref(), color, color);
            let sized = frame
                .as_ref()
                .is_some_and(|frame| (frame.width as u32, frame.height as u32) == pixels);
            poll(class == "fresh" && sized, || {
                format!(
                    "кадр {class}, размер {:?} при виде {pixels:?}, выборка {:?}",
                    frame.as_ref().map(|frame| (frame.width, frame.height)),
                    frame.as_ref().map(|frame| frame.samples.clone())
                )
            })
        }
        Err(reason) => Poll::Pending(reason),
    }
}

fn overlay_steps(steps: &mut Vec<Step>, stand: &Rc<Stand>, vis: &Shared) {
    let (opening, opening_stand) = (Rc::clone(vis), Rc::clone(stand));
    steps.push(Step::new(
        "палитра частично перекрывает A",
        STATE_TIMEOUT,
        || Poll::Ready(()),
        move || {
            let mut vis = opening.borrow_mut();
            palette::install(&opening_stand.window, None);
            let fed = vis.fed_len(Who::A).0;
            vis.overlay_from = Some((Instant::now(), fed));
            palette::open("visibility")
        },
    ));
    let (waiting, closing, closing_stand) = (Rc::clone(vis), Rc::clone(vis), Rc::clone(stand));
    steps.push(Step::new(
        "окно наблюдения под палитрой",
        STATE_TIMEOUT,
        move || {
            let vis = waiting.borrow();
            let since = vis
                .overlay_from
                .map_or(Duration::ZERO, |(at, _)| at.elapsed());
            poll(palette::is_open() && since >= OVERLAY_WINDOW, || {
                format!(
                    "палитра открыта {}, {} мс",
                    palette::is_open(),
                    since.as_millis()
                )
            })
        },
        move || {
            let mut vis = closing.borrow_mut();
            let from = vis.overlay_from.map_or(0, |(_, fed)| fed);
            let view = vis.env.view.clone();
            let signals = view.visibility_signals();
            vis.overlay = Overlay {
                fronts_during: vis.fed_len(Who::A).0.saturating_sub(from),
                model_visible: view.visibility_fed().is_some_and(|(fed, _)| fed),
                window_occlusion_visible: closing_stand
                    .window
                    .occlusionState()
                    .contains(NSWindowOcclusionState::Visible),
                overlay_signal: signals.overlay_partial,
                palette_open: palette::is_open(),
            };
            palette::close(palette::Close::Command);
            let _ = palette::uninstall();
            closing_stand.window.makeFirstResponder(Some(&*view));
            Ok(())
        },
    ));
}

/// Шаги одной клетки матрицы.
fn cell_steps(steps: &mut Vec<Step>, stand: &Rc<Stand>, vis: &Shared, spec: &'static CellSpec) {
    let cold = spec.who == Who::C;
    let (begin, begin_stand) = (Rc::clone(vis), Rc::clone(stand));
    steps.push(Step::new(
        spec.name,
        STATE_TIMEOUT,
        || Poll::Ready(()),
        move || begin_cell(&begin_stand, &mut begin.borrow_mut(), spec),
    ));
    if !cold {
        let (waiting, hiding, hiding_stand) = (Rc::clone(vis), Rc::clone(vis), Rc::clone(stand));
        steps.push(Step::new(
            "прогрев: маркер в кадре",
            WARM_TIMEOUT,
            move || warm_ready(&mut waiting.borrow_mut()),
            move || {
                let mut vis = hiding.borrow_mut();
                if let Some((who, color, _, _)) = vis.warm.take() {
                    vis.shown[index(who)] = Some(color);
                }
                hide_cell(&hiding_stand, &mut vis)
            },
        ));
    }
    let (established, starting, starting_stand) =
        (Rc::clone(vis), Rc::clone(vis), Rc::clone(stand));
    steps.push(Step::new(
        "скрытие установлено",
        STATE_TIMEOUT,
        move || hidden_ready(&starting_stand, &mut established.borrow_mut()),
        move || {
            let mut vis = starting.borrow_mut();
            record_established(&mut vis);
            vis.start_series(&format!("{}-hidden", spec.name));
            Ok(())
        },
    ));
    let (sampling, during) = (Rc::clone(vis), Rc::clone(vis));
    steps.push(Step::new(
        "замер скрытой",
        SERIES_TIMEOUT,
        move || sampling.borrow_mut().poll_series(),
        move || {
            let mut vis = during.borrow_mut();
            let sample = vis.take_sample()?;
            let cur = vis.cur.as_mut().ok_or("клетки нет")?;
            cur.record.counters.hidden = selected(&sample);
            cur.record.counters.hidden_settled = sample.settled;
            start_during(&mut vis)
        },
    ));
    let (loading, painting) = (Rc::clone(vis), Rc::clone(vis));
    steps.push(Step::new(
        "нагрузка скрытой",
        match spec.during {
            During::Long => LONG_TIMEOUT,
            _ => DURING_TIMEOUT,
        },
        move || during_ready(&mut loading.borrow_mut()),
        move || {
            let mut vis = painting.borrow_mut();
            let color = vis.next_color();
            let cur = vis.cur.as_ref().ok_or("клетки нет")?;
            let (who, nonce) = (cur.spec.who, format!("{}-post-{}", cur.nonce, vis.color));
            let commands = vis.paint(who, color, &nonce)?;
            let pixels = vis.view(who).map(|view| view.pixel_size());
            let cur = vis.cur.as_mut().ok_or("клетки нет")?;
            cur.record.post_color = Some(color);
            cur.record.marker_pixels = pixels;
            cur.pending = commands;
            cur.nonce = nonce;
            Ok(())
        },
    ));
    let (model, quiet) = (Rc::clone(vis), Rc::clone(vis));
    steps.push(Step::new(
        "маркер скрытой в модели",
        MODEL_TIMEOUT,
        move || {
            let mut vis = model.borrow_mut();
            let Some(cur) = vis.cur.as_ref() else {
                return Poll::Ready(());
            };
            let (who, commands, nonce) = (cur.spec.who, cur.pending.clone(), cur.nonce.clone());
            match vis.confirmed(who, &commands, &nonce) {
                Ok(true) => Poll::Ready(()),
                Ok(false) => Poll::Pending(format!("маркер {nonce} ещё не в модели")),
                Err(reason) => Poll::Pending(reason),
            }
        },
        move || {
            let mut vis = quiet.borrow_mut();
            let cur = vis.cur.as_mut().ok_or("клетки нет")?;
            cur.quiet_from = Some(Instant::now());
            Ok(())
        },
    ));
    let (silent, showing, showing_stand) = (Rc::clone(vis), Rc::clone(vis), Rc::clone(stand));
    steps.push(Step::new(
        "тишина скрытой",
        QUIET_TIMEOUT,
        move || {
            let vis = silent.borrow();
            let since = vis
                .cur
                .as_ref()
                .and_then(|cur| cur.quiet_from)
                .map_or(Duration::MAX, |at| at.elapsed());
            poll(since >= QUIET, || {
                format!("тишина {} мс", since.as_millis())
            })
        },
        move || show_cell(&showing_stand, &mut showing.borrow_mut()),
    ));
    let (fresh, recording, recording_stand) = (Rc::clone(vis), Rc::clone(vis), Rc::clone(stand));
    steps.push(Step::new(
        "свежий кадр после показа",
        FRESH_TIMEOUT,
        move || shown_ready(&recording_stand, &mut fresh.borrow_mut()),
        move || {
            let mut vis = recording.borrow_mut();
            vis.start_series(&format!("{}-shown", spec.name));
            Ok(())
        },
    ));
    let (sampling, finishing) = (Rc::clone(vis), Rc::clone(vis));
    steps.push(Step::new(
        "замер после показа",
        SERIES_TIMEOUT,
        move || sampling.borrow_mut().poll_series(),
        move || finish_cell(&mut finishing.borrow_mut()),
    ));
    if cold {
        let gone = Rc::clone(vis);
        steps.push(Step::new(
            "освобождение C и выход её фикстуры",
            CLOSE_TIMEOUT,
            move || c_freed(&mut gone.borrow_mut()),
            || Ok(()),
        ));
    } else if spec.during == During::ResizeHidden {
        let (restored, settled) = (Rc::clone(vis), Rc::clone(vis));
        steps.push(Step::new(
            "делитель на прежнем месте",
            STATE_TIMEOUT,
            move || {
                let vis = restored.borrow();
                let at = vis.scene.divider(Divider::LeftCenter);
                poll((at - vis.home_divider).abs() < 1.0, || {
                    format!("делитель {at:.1}, ожидался {:.1}", vis.home_divider)
                })
            },
            move || {
                let vis = settled.borrow();
                events::note(format!(
                    "делитель возвращён, вид {:?} пикселей",
                    vis.env.view.pixel_size()
                ));
                Ok(())
            },
        ));
    }
}

fn begin_cell(stand: &Stand, vis: &mut Vis, spec: &'static CellSpec) -> Result<(), String> {
    events::note(format!("клетка {}", spec.name));
    let before = vis.last_sample.as_ref().map(selected).unwrap_or_default();
    let mut record = CellRecord {
        name: spec.name,
        mechanism: Some(spec.mechanism),
        note: spec.note,
        who: Some(spec.who),
        workarounds: vis.workarounds.names(),
        occlusion_masked: visibility::occlusion_masked(),
        ..CellRecord::default()
    };
    record.counters.before = before;
    let (fed_from, sync_from) = vis.fed_len(spec.who);
    vis.cur = Some(Cur {
        spec,
        record,
        hide_ns: 0,
        established_ns: None,
        quiet_from: None,
        state_since: Instant::now(),
        fed_from,
        sync_from,
        nonce: format!("v-{}", spec.name),
        pending: Vec::new(),
        stream: None,
        reparent_steps: 0,
        resize_from: (0, 0),
        long_until: None,
        first_pass: Rc::new(RefCell::new(None)),
        show_at: None,
    });
    if spec.who == Who::C {
        return hide_cold(stand, vis);
    }
    let nonce = format!("v-{}-pre", spec.name);
    begin_warm(vis, spec.who, &nonce)?;
    let color = vis.warm.as_ref().map(|warm| warm.1);
    if let Some(cur) = vis.cur.as_mut() {
        cur.record.pre_color = color;
    }
    Ok(())
}

/// Новая поверхность C в правой зоне уходит за вкладку сразу после `ghostty_surface_new`, в том
/// же проходе главного потока: до первого кадра.
fn hide_cold(stand: &Stand, vis: &mut Vis) -> Result<(), String> {
    let mtm = stand.view.mtm();
    let view = SurfaceView::new(mtm, NSRect::ZERO);
    vis.scene.add_tab(Zone::Right, 0, COLD_TITLE, &view)?;
    let extra = vis.env.add_fixture(stand, "c", view.clone())?;
    let hide_ns = clock::uptime_ns();
    match runtime::create(extra.id)? {
        Created::Live { key, new_ms } => {
            events::note(format!("поверхность C {key} создана за {new_ms:.1} мс"));
        }
        Created::Failed { key, failure, .. } => {
            return Err(format!(
                "ghostty_surface_new вернул NULL для {key}: {failure:?}"
            ));
        }
        Created::Waiting(decision) => {
            return Err(format!("реестр не разрешил создание C: {decision:?}"));
        }
    }
    let contents = view.has_contents();
    vis.scene.add_tab(Zone::Right, 0, STUB_TITLE, &vis.stub)?;
    let deferred = view.visibility_fed().is_some_and(|(_, deferred)| deferred);
    vis.watch.borrow_mut().add(Who::C, &view);
    vis.c = Some(extra);
    let pre = vis.next_color();
    let cur = vis.cur.as_mut().ok_or("клетки нет")?;
    cur.hide_ns = hide_ns;
    cur.record.contents_at_hide = contents;
    cur.record.deferred_at_hide = deferred;
    cur.record.pre_color = Some(pre);
    cur.state_since = Instant::now();
    Ok(())
}

fn hide_cell(stand: &Stand, vis: &mut Vis) -> Result<(), String> {
    let cur = vis.cur.as_ref().ok_or("клетки нет")?;
    let (spec, who) = (cur.spec, cur.spec.who);
    let view = vis.view(who).ok_or("вида нет")?;
    let contents = view.has_contents();
    let evaluations_before = evaluations(&view);
    let hide_ns = clock::uptime_ns();
    match spec.mechanism {
        Mechanism::Tab => vis.scene.add_tab(home(who), 0, STUB_TITLE, &vis.stub)?,
        Mechanism::Zone => {
            let tile = vis.scene.tile(home(who), 0).ok_or("плитки зоны нет")?;
            tile.setHidden(true);
        }
        Mechanism::Miniaturize => stand.window.miniaturize(None),
        Mechanism::OrderOut => stand.window.orderOut(None),
        Mechanism::Covered => {
            let frame = stand.window.frame();
            let area = NSRect::new(
                NSPoint::new(frame.origin.x - COVER_MARGIN, frame.origin.y - COVER_MARGIN),
                NSSize::new(
                    frame.size.width + 2.0 * COVER_MARGIN,
                    frame.size.height + 2.0 * COVER_MARGIN,
                ),
            );
            let cover = vis.cover.clone().unwrap_or_else(|| {
                // SAFETY: назначенный инициализатор окна без рамки.
                let cover = unsafe {
                    NSWindow::initWithContentRect_styleMask_backing_defer(
                        NSWindow::alloc(stand.view.mtm()),
                        area,
                        NSWindowStyleMask::Borderless,
                        NSBackingStoreType::Buffered,
                        false,
                    )
                };
                // SAFETY: окно держит стенд, закрытие его не освобождает.
                unsafe { cover.setReleasedWhenClosed(false) };
                cover.setOpaque(true);
                cover.setBackgroundColor(Some(&NSColor::grayColor()));
                cover
            });
            cover.setFrame_display(area, true);
            cover.orderFront(None);
            vis.cover = Some(cover);
        }
        Mechanism::AppHidden => stand.app.hide(None),
        Mechanism::Reparent => {
            vis.scene.take_tab(&view)?;
            view.removeFromSuperview();
        }
    }
    let deferred = view.visibility_fed().is_some_and(|(_, deferred)| deferred);
    let cur = vis.cur.as_mut().ok_or("клетки нет")?;
    cur.hide_ns = hide_ns;
    cur.record.contents_at_hide = contents;
    cur.record.deferred_at_hide = deferred;
    cur.record.evaluations_at_hide = evaluations_before;
    cur.state_since = Instant::now();
    Ok(())
}

fn evaluations(view: &SurfaceView) -> u64 {
    view.visibility_log().map_or(0, |log| log.evaluations)
}

/// Способ скрытия дошёл до своего состояния.
fn hidden_state(stand: &Stand, mechanism: Mechanism, view: &SurfaceView) -> (bool, String) {
    let window = &stand.window;
    match mechanism {
        Mechanism::Tab | Mechanism::Reparent => (
            view.window().is_none(),
            format!("вид в окне {}", view.window().is_some()),
        ),
        Mechanism::Zone => (
            view.window().is_some() && view.isHiddenOrHasHiddenAncestor(),
            format!(
                "вид в окне {}, скрыт предок {}",
                view.window().is_some(),
                view.isHiddenOrHasHiddenAncestor()
            ),
        ),
        Mechanism::Miniaturize => (
            window.isMiniaturized(),
            format!("окно свёрнуто {}", window.isMiniaturized()),
        ),
        Mechanism::OrderOut => (
            !window.isVisible(),
            format!("окно на экране {}", window.isVisible()),
        ),
        Mechanism::Covered => {
            let visible = window
                .occlusionState()
                .contains(NSWindowOcclusionState::Visible);
            (!visible, format!("occlusionState Visible {visible}"))
        }
        Mechanism::AppHidden => (
            stand.app.isHidden(),
            format!("приложение скрыто {}", stand.app.isHidden()),
        ),
    }
}

/// Способ показа вернул состояние.
fn shown_state(stand: &Stand, mechanism: Mechanism, view: &SurfaceView) -> (bool, String) {
    let window = &stand.window;
    let base = view.window().is_some()
        && !view.isHiddenOrHasHiddenAncestor()
        && window.isVisible()
        && !window.isMiniaturized()
        && !stand.app.isHidden();
    let occlusion = window
        .occlusionState()
        .contains(NSWindowOcclusionState::Visible);
    let ok = match mechanism {
        Mechanism::Covered | Mechanism::Miniaturize | Mechanism::OrderOut => {
            base && (occlusion || visibility::occlusion_masked())
        }
        _ => base,
    };
    (
        ok,
        format!(
            "вид в окне {}, скрыт предок {}, окно на экране {}, свёрнуто {}, occlusion Visible {occlusion}, приложение скрыто {}",
            view.window().is_some(),
            view.isHiddenOrHasHiddenAncestor(),
            window.isVisible(),
            window.isMiniaturized(),
            stand.app.isHidden()
        ),
    )
}

/// Смена родителя по шагу таймера: вставка в другую зону, затем снятие из окна.
fn reparent_tick(vis: &mut Vis) -> Result<bool, String> {
    let cur = vis.cur.as_ref().ok_or("клетки нет")?;
    let steps = cur.reparent_steps;
    if steps >= 2 * REPARENT_CYCLES {
        return Ok(true);
    }
    let view = vis.env.view.clone();
    if steps % 2 == 0 {
        let zone = if (steps / 2) % 2 == 0 {
            Zone::Bottom
        } else {
            Zone::Center
        };
        vis.scene.add_tab(zone, 0, TITLE, &view)?;
    } else {
        vis.scene.take_tab(&view)?;
        view.removeFromSuperview();
    }
    if let Some(cur) = vis.cur.as_mut() {
        cur.reparent_steps += 1;
    }
    Ok(false)
}

fn hidden_ready(stand: &Stand, vis: &mut Vis) -> Poll<()> {
    let Some(cur) = vis.cur.as_ref() else {
        return Poll::Ready(());
    };
    let spec = cur.spec;
    if spec.mechanism == Mechanism::Reparent {
        match reparent_tick(vis) {
            Ok(true) => {}
            Ok(false) => return Poll::Pending("смены родителя идут".to_owned()),
            Err(reason) => return Poll::Pending(reason),
        }
    }
    let Some(view) = vis.view(spec.who) else {
        return Poll::Pending("вида нет".to_owned());
    };
    if spec.who == Who::C {
        let nonce = vis.nonce(Who::C).unwrap_or_default();
        vis.env.hub.poll();
        let ready = vis
            .env
            .hub
            .by_nonce(&nonce)
            .is_some_and(|conn| conn.ready.is_some());
        if !ready {
            return Poll::Pending(format!("готовность фикстуры C; {}", vis.env.hub.describe()));
        }
    }
    let (reached, state) = hidden_state(stand, spec.mechanism, &view);
    let (fed, deferred) = view.visibility_fed().unwrap_or((true, false));
    let model_hidden = !fed && !deferred;
    let Some(cur) = vis.cur.as_mut() else {
        return Poll::Ready(());
    };
    cur.record.hidden_state_reached = reached;
    cur.record.hidden_state = state.clone();
    cur.record.model_hidden = model_hidden;
    cur.record.evaluations_established = evaluations(&view);
    if (reached && model_hidden) || cur.state_since.elapsed() >= STATE_LIMIT {
        return Poll::Ready(());
    }
    Poll::Pending(format!("{state}; подано {fed}, отложено {deferred}"))
}

fn record_established(vis: &mut Vis) {
    let Some(cur) = vis.cur.as_mut() else {
        return;
    };
    let now = clock::uptime_ns();
    cur.established_ns = Some(now);
    cur.record.established_ms = Some(clock::interval_ms(cur.hide_ns, now));
    if !cur.record.hidden_state_reached || !cur.record.model_hidden {
        events::note(format!(
            "клетка {}: скрытие не наступило за {} мс: {}",
            cur.spec.name,
            STATE_LIMIT.as_millis(),
            cur.record.hidden_state
        ));
    }
}

fn start_during(vis: &mut Vis) -> Result<(), String> {
    let cur = vis.cur.as_ref().ok_or("клетки нет")?;
    let (spec, who) = (cur.spec, cur.spec.who);
    match spec.during {
        During::Nothing => {}
        During::Long => {
            let cur = vis.cur.as_mut().ok_or("клетки нет")?;
            cur.long_until = Some(Instant::now() + LONG_HIDE);
            cur.record.during = Some(format!("скрытие ещё {} с", LONG_HIDE.as_secs()));
        }
        During::Output => {
            let fixture = vis.nonce(who).ok_or("фикстуры нет")?;
            let id = vis.env.hub.send(&fixture, STREAM)?;
            let cur = vis.cur.as_mut().ok_or("клетки нет")?;
            cur.stream = Some(id);
        }
        During::ResizeHidden => {
            let view = vis.view(who).ok_or("вида нет")?;
            let from = view.pixel_size();
            let at = vis.scene.divider(Divider::LeftCenter);
            vis.scene
                .set_divider(Divider::LeftCenter, at + DIVIDER_SHIFT);
            let cur = vis.cur.as_mut().ok_or("клетки нет")?;
            cur.resize_from = from;
            cur.state_since = Instant::now();
        }
    }
    Ok(())
}

fn during_ready(vis: &mut Vis) -> Poll<()> {
    let Some(cur) = vis.cur.as_ref() else {
        return Poll::Ready(());
    };
    let (spec, who) = (cur.spec, cur.spec.who);
    match spec.during {
        During::Nothing => Poll::Ready(()),
        During::Long => {
            let left = cur.long_until.map_or(Duration::ZERO, |until| {
                until.saturating_duration_since(Instant::now())
            });
            poll(left.is_zero(), || {
                format!("скрытие ещё {} мс", left.as_millis())
            })
        }
        During::Output => {
            let stream = cur.stream;
            vis.env.hub.poll();
            let fixture = vis.nonce(who).unwrap_or_default();
            let done = vis.env.hub.by_nonce(&fixture).and_then(|conn| {
                conn.receipts
                    .iter()
                    .find_map(|receipt| match &receipt.event {
                        crate::fixture::Event::StreamDone {
                            id,
                            lines,
                            cancelled,
                            ..
                        } if Some(*id) == stream => Some((*lines, *cancelled)),
                        _ => None,
                    })
            });
            match done {
                Some((lines, cancelled)) => {
                    if let Some(cur) = vis.cur.as_mut() {
                        cur.record.during = Some(format!(
                            "поток вывода скрытой: строк {lines}, прерван {cancelled}"
                        ));
                    }
                    Poll::Ready(())
                }
                None => Poll::Pending("поток вывода идёт".to_owned()),
            }
        }
        During::ResizeHidden => {
            let Some(view) = vis.view(who) else {
                return Poll::Pending("вида нет".to_owned());
            };
            let now = view.pixel_size();
            let from = cur.resize_from;
            let elapsed = cur.state_since.elapsed();
            if now != from || elapsed >= STATE_LIMIT {
                let feeds = view.feeds();
                if let Some(cur) = vis.cur.as_mut() {
                    cur.record.during = Some(format!(
                        "ресайз скрытого вида {from:?} → {now:?} пикселей, подач размера {}, последний поданный {:?}",
                        feeds.size_feeds, feeds.last_size_px
                    ));
                }
                return Poll::Ready(());
            }
            Poll::Pending(format!(
                "размер вида {now:?}, ожидалось изменение от {from:?}"
            ))
        }
    }
}

fn show_cell(stand: &Stand, vis: &mut Vis) -> Result<(), String> {
    let cur = vis.cur.as_ref().ok_or("клетки нет")?;
    let (spec, who) = (cur.spec, cur.spec.who);
    let view = vis.view(who).ok_or("вида нет")?;
    let (established, now) = (
        cur.established_ns.unwrap_or(cur.hide_ns),
        clock::uptime_ns(),
    );
    let watch = vis.watch.borrow();
    let total = watch.between(who, cur.hide_ns, now);
    let late = watch.between(who, established, now);
    drop(watch);
    match spec.mechanism {
        Mechanism::Tab => {
            vis.scene.take_tab(&vis.stub)?;
            vis.stub.removeFromSuperview();
        }
        Mechanism::Zone => {
            let tile = vis.scene.tile(home(who), 0).ok_or("плитки зоны нет")?;
            tile.setHidden(false);
        }
        Mechanism::Miniaturize => stand.window.deminiaturize(None),
        Mechanism::OrderOut => stand.window.makeKeyAndOrderFront(None),
        Mechanism::Covered => {
            if let Some(cover) = &vis.cover {
                cover.orderOut(None);
            }
        }
        Mechanism::AppHidden => {
            stand.app.unhide(None);
            #[allow(deprecated)]
            stand.app.activateIgnoringOtherApps(true);
        }
        Mechanism::Reparent => vis.scene.add_tab(home(who), 0, TITLE, &view)?,
    }
    let area = vis.cur.as_ref().and_then(|cur| cur.record.marker_pixels);
    let frame = read_frame(&view, area)?;
    let cur = vis.cur.as_mut().ok_or("клетки нет")?;
    let (pre, post) = (
        cur.record.pre_color.unwrap_or(Rgb(0, 0, 0)),
        cur.record.post_color.unwrap_or(Rgb(0, 0, 0)),
    );
    cur.record.frames_hidden_total = total;
    cur.record.frames_hidden_late = late;
    cur.record.at_show = classify(frame.as_ref(), post, pre).to_owned();
    cur.show_at = Some(Instant::now());
    cur.state_since = Instant::now();
    let slot = Rc::clone(&cur.first_pass);
    let weak = Weak::new(&*view);
    events::on_display_pass(move |_at| {
        let class = match weak.load() {
            Some(view) => match read_frame(&view, area) {
                Ok(frame) => classify(frame.as_ref(), post, pre).to_owned(),
                Err(reason) => format!("ошибка чтения: {reason}"),
            },
            None => "вида нет".to_owned(),
        };
        *slot.borrow_mut() = Some(class);
        false
    });
    if who == Who::A {
        stand.window.makeFirstResponder(Some(&*view));
        vis.scene.sync_focus(&stand.window);
    }
    Ok(())
}

fn shown_ready(stand: &Stand, vis: &mut Vis) -> Poll<()> {
    let Some(cur) = vis.cur.as_ref() else {
        return Poll::Ready(());
    };
    let (spec, who) = (cur.spec, cur.spec.who);
    let Some(view) = vis.view(who) else {
        return Poll::Pending("вида нет".to_owned());
    };
    let area = cur.record.marker_pixels;
    let (reached, state) = shown_state(stand, spec.mechanism, &view);
    let frame = match read_frame(&view, area) {
        Ok(frame) => frame,
        Err(reason) => return Poll::Pending(reason),
    };
    let pixels = view.pixel_size();
    let Some(cur) = vis.cur.as_mut() else {
        return Poll::Ready(());
    };
    let (pre, post) = (
        cur.record.pre_color.unwrap_or(Rgb(0, 0, 0)),
        cur.record.post_color.unwrap_or(Rgb(0, 0, 0)),
    );
    let class = classify(frame.as_ref(), post, pre);
    let sized = frame
        .as_ref()
        .is_some_and(|frame| (frame.width as u32, frame.height as u32) == pixels);
    cur.record.shown_state_reached = reached;
    cur.record.shown_state = state.clone();
    cur.record.view_pixels = pixels;
    cur.record.size_matches = sized;
    cur.record.frame = frame.clone();
    let fresh = class == "fresh" && sized;
    if fresh && cur.record.fresh_ms.is_none() {
        cur.record.fresh = true;
        cur.record.fresh_ms = cur.show_at.map(|at| at.elapsed().as_secs_f64() * 1e3);
    }
    let first_pass_known = cur.first_pass.borrow().is_some();
    if (reached && fresh && first_pass_known) || cur.state_since.elapsed() >= FRESH_LIMIT {
        cur.record.at_first_pass = cur.first_pass.borrow().clone();
        return Poll::Ready(());
    }
    Poll::Pending(format!(
        "{state}; кадр {class}, размер {:?} при виде {pixels:?}",
        frame.as_ref().map(|frame| (frame.width, frame.height))
    ))
}

fn finish_cell(vis: &mut Vis) -> Result<(), String> {
    let sample = vis.take_sample()?;
    let mut cur = vis.cur.take().ok_or("клетки нет")?;
    cur.record.counters.shown = selected(&sample);
    cur.record.counters.shown_settled = sample.settled;
    let (who, spec) = (cur.spec.who, cur.spec);
    if let Some(view) = vis.view(who) {
        cur.record.surface = view.surface_key().map(|key| key.to_string());
        if let Some(log) = view.visibility_log() {
            cur.record.fronts = log.fed.get(cur.fed_from..).unwrap_or_default().to_vec();
            cur.record.sync_draws = log
                .sync_draws
                .get(cur.sync_from..)
                .unwrap_or_default()
                .to_vec();
        }
    }
    if cur.record.fresh
        && let Some(post) = cur.record.post_color
    {
        vis.shown[index(who)] = Some(post);
    }
    vis.cells.push(cur.record);
    if who == Who::C {
        let id = vis.id(Who::C).ok_or("поверхности C нет")?;
        runtime::request_close(id)?;
    }
    if spec.during == During::ResizeHidden {
        // Делитель возвращается после проверки кадра: маркер нарисован в сетке скрытого размера.
        let home = vis.home_divider;
        vis.scene.set_divider(Divider::LeftCenter, home);
    }
    Ok(())
}

fn c_freed(vis: &mut Vis) -> Poll<()> {
    let Some(c) = vis.c.as_ref() else {
        return Poll::Ready(());
    };
    let (id, nonce) = (c.id, c.fixture_nonce.clone());
    vis.env.hub.poll();
    let phase = runtime::phase(id);
    let closed = vis.env.hub.by_nonce(&nonce).is_some_and(|conn| conn.closed);
    poll(phase == Some(Phase::Freed) && closed, || {
        format!(
            "C {}, сокет фикстуры закрыт {closed}",
            phase.map_or("нет", Phase::name)
        )
    })
}

// ---- выводы ----

fn judge(vis: &Vis, report: &mut Report) {
    let workarounds = vis.workarounds.names();
    let set = if workarounds.is_empty() {
        "базовый вариант".to_owned()
    } else {
        format!("обходы {}", workarounds.join("+"))
    };
    let mut alternate = Vec::new();
    for who in [Who::A, Who::C] {
        if let Some(log) = vis.view(who).and_then(|view| view.visibility_log()) {
            let fed: Vec<bool> = log.fed.iter().map(|front| front.visible).collect();
            alternate.push((who, visibility::fronts_alternate(&fed), fed.len()));
        }
    }
    let ok = alternate.iter().all(|(_, ok, _)| *ok);
    report.finding(
        "h7-fronts-only",
        Requirement::Required,
        if ok { Verdict::Pass } else { Verdict::Fail },
        format!(
            "{set}: подачи set_occlusion чередуются и начинаются с false: {}",
            alternate
                .iter()
                .map(|(who, ok, len)| format!("{who:?} {ok} ({len} подач)"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
    let masked = vis.shield.is_some();
    let shield = vis.shield.as_ref().map_or(String::new(), |shield| {
        format!(
            "окно под щитом {} (pid {}, уровень {} над {}), occlusionState исключён стендом из признаков",
            shield.owner, shield.pid, shield.layer, shield.own_layer
        )
    });
    let overlay = &vis.overlay;
    let overlay_ok = overlay.palette_open
        && overlay.fronts_during == 0
        && overlay.model_visible
        && (overlay.window_occlusion_visible || masked);
    report.finding(
        "h7-partial-overlay",
        Requirement::Required,
        if overlay_ok { Verdict::Pass } else { Verdict::Fail },
        format!(
            "палитра открыта {}, подач за {} мс под палитрой {}, модель видима {}, occlusionState окна Visible {}, признак наложения {}",
            overlay.palette_open,
            OVERLAY_WINDOW.as_millis(),
            overlay.fronts_during,
            overlay.model_visible,
            overlay.window_occlusion_visible,
            overlay.overlay_signal
        ) + &if masked { format!("; {shield}") } else { String::new() },
    );
    if masked {
        report.finding(
            "h7-live-occlusion",
            Requirement::Required,
            Verdict::PendingManual,
            format!(
                "{set}: фронт по occlusionState окна не проверен: {shield}; повтор visibility без щита — задача 19"
            ),
        );
    }
    for cell in &vis.cells {
        judge_cell(cell, &set, &shield, report);
    }
    for name in ["h7a-instruments", "h7b-instruments"] {
        report.finding(
            name,
            Requirement::Required,
            Verdict::PendingManual,
            format!(
                "{set}: вердикт по записи Instruments (отправки команд GPU, выделения ресурсов Metal) в задаче 19; счётчики пробы — диагностика"
            ),
        );
    }
}

fn delta(counters: &Counters, name: &str) -> String {
    let get = |map: &BTreeMap<String, i64>| map.get(name).copied();
    match (
        get(&counters.before),
        get(&counters.hidden),
        get(&counters.shown),
    ) {
        (Some(before), Some(hidden), Some(shown)) => format!(
            "{name} скрытие {:+.2}, показ {:+.2} МиБ",
            (hidden - before) as f64 / (1024.0 * 1024.0),
            (shown - hidden) as f64 / (1024.0 * 1024.0)
        ),
        _ => format!("{name} нет замера"),
    }
}

fn judge_cell(cell: &CellRecord, set: &str, shield: &str, report: &mut Report) {
    let name = cell.name;
    let blocked = cell.occlusion_masked
        && (cell.mechanism == Some(Mechanism::Covered)
            || (!cell.model_hidden && cell.evaluations_established == cell.evaluations_at_hide));
    if blocked {
        for prefix in ["h7-hide", "h7b-fresh"] {
            report.finding(
                &format!("{prefix}-{name}"),
                Requirement::Required,
                Verdict::PendingManual,
                format!(
                    "{set}: {}: клетка закрыта щитом — {shield}; оценок видимости до скрытия {}, к установлению {}, модель скрыла {}",
                    cell.note,
                    cell.evaluations_at_hide,
                    cell.evaluations_established,
                    cell.model_hidden
                ),
            );
        }
    }
    let hide = if blocked {
        (Verdict::PendingManual, String::new())
    } else if !cell.hidden_state_reached {
        (
            Verdict::Observed,
            format!(
                "клетка не состоялась: способ скрытия не дошёл до состояния за {} мс ({})",
                STATE_LIMIT.as_millis(),
                cell.hidden_state
            ),
        )
    } else if cell.model_hidden {
        (
            Verdict::Pass,
            format!(
                "{} ({}); модель подала false, отложено при скрытии {}, contents при скрытии {}, установлено за {:.1} мс",
                cell.note,
                cell.hidden_state,
                cell.deferred_at_hide,
                cell.contents_at_hide,
                cell.established_ms.unwrap_or(0.0)
            ),
        )
    } else {
        (
            Verdict::Fail,
            format!(
                "{}: способ дошёл до состояния ({}), а модель false не подала за {} мс; отложено при скрытии {}",
                cell.note,
                cell.hidden_state,
                STATE_LIMIT.as_millis(),
                cell.deferred_at_hide
            ),
        )
    };
    if !blocked {
        report.finding(
            &format!("h7-hide-{name}"),
            Requirement::Required,
            hide.0,
            format!("{set}: {}", hide.1),
        );
    }
    let fresh = if !cell.hidden_state_reached {
        Verdict::Observed
    } else if cell.fresh && cell.shown_state_reached {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    let frame = cell.frame.as_ref().map_or("нет".to_owned(), |frame| {
        format!("{}×{}", frame.width, frame.height)
    });
    report.finding(
        &format!("h7b-fresh-{name}{}", if blocked { "-observed" } else { "" }),
        if blocked { Requirement::Research } else { Requirement::Required },
        if blocked { Verdict::Observed } else { fresh },
        format!(
            "{set}: показ ({}): в момент показа кадр {}, на первом проходе отображения {}, свежий маркер {} за {} мс, кадр {frame} при виде {:?} (совпадает {}){}",
            cell.shown_state,
            cell.at_show,
            cell.at_first_pass.as_deref().unwrap_or("не записан"),
            cell.fresh,
            cell.fresh_ms.map_or("—".to_owned(), |ms| format!("{ms:.1}")),
            cell.view_pixels,
            cell.size_matches,
            if cell.sync_draws.is_empty() {
                String::new()
            } else {
                format!(
                    ", синхронных кадров {} ({:.1} мс)",
                    cell.sync_draws.len(),
                    cell.sync_draws.iter().map(|draw| draw.ms).sum::<f64>()
                )
            }
        ),
    );
    report.finding(
        &format!("h7-hidden-diagnostics-{name}"),
        Requirement::Required,
        Verdict::Observed,
        format!(
            "{set}: смен contents от скрытия до показа {}, после установления скрытия {}{}; {}; {}; {}; серии устоялись {}/{}",
            cell.frames_hidden_total,
            cell.frames_hidden_late,
            cell.during
                .as_ref()
                .map_or(String::new(), |during| format!("; {during}")),
            delta(&cell.counters, "footprint.footprint"),
            delta(&cell.counters, "footprint.IOSurface.dirty"),
            delta(
                &cell.counters,
                "footprint.Owned physical footprint (unmapped).dirty"
            ),
            cell.counters.hidden_settled,
            cell.counters.shown_settled
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(samples: Vec<Rgb>) -> Frame {
        Frame {
            surface_id: 1,
            contents: 1,
            width: 100,
            height: 100,
            samples,
        }
    }

    #[test]
    fn classify_by_marker_colors() {
        let (fresh, stale) = (COLORS[1], COLORS[0]);
        assert_eq!(classify(None, fresh, stale), "none");
        assert_eq!(classify(Some(&frame(vec![])), fresh, stale), "empty");
        assert_eq!(
            classify(Some(&frame(vec![fresh; 5])), fresh, stale),
            "fresh"
        );
        assert_eq!(
            classify(Some(&frame(vec![stale; 5])), fresh, stale),
            "stale"
        );
        let mut mixed = vec![fresh; 5];
        mixed[2] = Rgb(255, 255, 255);
        assert_eq!(classify(Some(&frame(mixed)), fresh, stale), "other");
        let shifted = Rgb(fresh.0 + 20, fresh.1 + 20, fresh.2.saturating_sub(20));
        assert_eq!(
            classify(Some(&frame(vec![shifted; 5])), fresh, stale),
            "fresh"
        );
    }

    #[test]
    fn neighbouring_marker_colors_are_distinguishable() {
        for index in 0..COLORS.len() {
            let (a, b) = (COLORS[index], COLORS[(index + 1) % COLORS.len()]);
            assert!(snapshot::distance(a, b) > 2 * NEAR, "{a:?} и {b:?}");
            let (pa, pb) = (display_p3(a), display_p3(b));
            assert!(snapshot::distance(pa, pb) > 2 * NEAR, "P3 {pa:?} и {pb:?}");
            assert!(
                !near(pa, b) && !near(pb, a),
                "{a:?} и {b:?} в разных пространствах"
            );
        }
    }

    #[test]
    fn display_p3_of_known_colors() {
        assert_eq!(display_p3(Rgb(0, 0, 0)), Rgb(0, 0, 0));
        assert_eq!(display_p3(Rgb(255, 255, 255)), Rgb(255, 255, 255));
        let cyan = display_p3(Rgb(0, 200, 210));
        assert!(snapshot::distance(cyan, Rgb(90, 197, 208)) <= 3, "{cyan:?}");
        assert!(near(Rgb(90, 197, 208), Rgb(0, 200, 210)));
    }

    #[test]
    fn marker_points_stay_inside_a_marker_from_a_smaller_grid() {
        // Маркер нарисован в сетке скрытого вида 868 пикселей, кадр при показе — 1108.
        let points = marker_points(1108, 896, Some((868, 896)));
        for (x, y) in points {
            assert!((868 * 3 / 10..=868 * 7 / 10).contains(&x), "{x}");
            assert!((896 * 3 / 10..=896 * 7 / 10).contains(&y), "{y}");
        }
        assert_eq!(marker_points(500, 400, None), sample_points(500, 400));
        assert_eq!(
            marker_points(500, 400, Some((900, 800))),
            sample_points(500, 400)
        );
    }

    #[test]
    fn sample_points_stay_inside_the_marker() {
        for (width, height) in [(100, 100), (2240, 1356), (31, 17)] {
            for (x, y) in sample_points(width, height) {
                assert!(x >= width * 3 / 10 && x <= width * 7 / 10, "{x} из {width}");
                assert!(
                    y >= height * 3 / 10 && y <= height * 7 / 10,
                    "{y} из {height}"
                );
            }
        }
    }

    #[test]
    fn watch_counts_changes_per_surface_and_window() {
        let watch = Watch {
            views: Vec::new(),
            changes: vec![(Who::A, 10), (Who::A, 20), (Who::C, 25), (Who::A, 30)],
            ticks: 0,
        };
        assert_eq!(watch.between(Who::A, 0, 100), 3);
        assert_eq!(watch.between(Who::A, 20, 30), 1);
        assert_eq!(watch.between(Who::C, 0, 100), 1);
    }

    #[test]
    fn cells_cover_the_matrix() {
        let mechanisms: Vec<Mechanism> = CELLS.iter().map(|cell| cell.mechanism).collect();
        for mechanism in [
            Mechanism::Tab,
            Mechanism::Zone,
            Mechanism::Miniaturize,
            Mechanism::OrderOut,
            Mechanism::Covered,
            Mechanism::AppHidden,
            Mechanism::Reparent,
        ] {
            assert!(mechanisms.contains(&mechanism), "{mechanism:?}");
        }
        assert!(CELLS.iter().any(|cell| cell.who == Who::C));
        for during in [During::ResizeHidden, During::Long, During::Output] {
            assert!(CELLS.iter().any(|cell| cell.during == during));
        }
        let mut names: Vec<&str> = CELLS.iter().map(|cell| cell.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CELLS.len());
        assert!(
            names
                .iter()
                .all(|name| name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
        );
    }
}
