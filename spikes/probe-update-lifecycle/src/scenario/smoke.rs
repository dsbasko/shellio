//! Сценарий стенда `smoke`: разрешения терминала исполнителя и окружение процесса.
//!
//! Процесс сценария запущен из терминала через `make`, поэтому TCC относит его к терминалу.
//! Accessibility — `AXIsProcessTrusted`. Screen Recording проверяется не флагом, а снимком: окно
//! пробы закрашено двумя известными цветами, внешний `/usr/sbin/screencapture` снимает его, и
//! пиксели в известных точках сверяются с цветами.
//!
//! - `screencapture -l` снимает само окно, даже перекрытое: без разрешения он не видит его
//!   содержимого. Это оракул разрешения.
//! - `screencapture -R` снимает экран в прямоугольнике окна: накрывающее окно (щит, как в отчёте
//!   P0.3) попадает в снимок вместо окна. Это оракул «снимок — не щит».
//!
//! Без разрешения или под щитом — ошибка исполнения с причиной, а не `fail`: это пропавшее
//! физическое предусловие. Флаг `CGPreflightScreenCaptureAccess` проверяется до снимков, чтобы
//! не вызвать запрос TCC, и записывается для сведения.

use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::{Retained, autoreleasepool};
use objc2::{AnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBitmapImageRep, NSBox,
    NSBoxType, NSColor, NSColorSpace, NSEventMask, NSFloatingWindowLevel, NSScreen,
    NSTitlePosition, NSWindow, NSWindowOcclusionState, NSWindowStyleMask,
};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFString, CFType};
use objc2_core_graphics::{
    CGPreflightScreenCaptureAccess, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGWindowBounds, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID,
};
use objc2_foundation::{
    MainThreadMarker, NSData, NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize,
};
use serde::Serialize;

use crate::launch;
use crate::results::{self, Cell, Environment, Report, Requirement, SMOKE_CELL, Status};

/// Срок процесса: окно и до `2 × ATTEMPTS` снимков.
pub const DEADLINE: Duration = Duration::from_secs(60);
pub const ENVIRONMENT_ARTIFACT: &str = "environment.json";
pub const PERMISSIONS_ARTIFACT: &str = "permissions.json";
pub const WINDOW_SNAPSHOT: &str = "smoke-window.png";
pub const SCREEN_SNAPSHOT: &str = "smoke-screen.png";
const SCREENCAPTURE: &str = "/usr/sbin/screencapture";
/// Попыток снимка до совпадения цветов: первый кадр окна может ещё не дойти до WindowServer.
const ATTEMPTS: usize = 5;
/// Срок одного запуска `screencapture`.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
/// Сколько событий доставлять между попытками.
const PUMP: Duration = Duration::from_millis(100);
const WINDOW_SIZE: (f64, f64) = (240.0, 160.0);

/// Цвет sRGB, 8 бит на канал.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// Левая половина окна (фон) и правая (вставка).
pub const LEFT: Rgb = Rgb(217, 38, 140);
pub const RIGHT: Rgb = Rgb(26, 179, 77);
/// Наибольшее расхождение канала: снимок проходит профиль дисплея и обратно в sRGB.
pub const TOLERANCE: u8 = 48;

/// Наибольшая разница каналов.
pub fn distance(a: Rgb, b: Rgb) -> u8 {
    [a.0.abs_diff(b.0), a.1.abs_diff(b.1), a.2.abs_diff(b.2)]
        .into_iter()
        .max()
        .unwrap_or(0)
}

/// Точки выборки в снимке размера `pixels`, который покрывает ровно окно: середины левой и
/// правой четвертей по ширине на середине высоты.
pub fn sample_points(pixels: (isize, isize)) -> [(isize, isize); 2] {
    let (width, height) = pixels;
    [(width / 4, height / 2), (width * 3 / 4, height / 2)]
}

/// Снимок показывает окно пробы: обе точки выборки близки к своим цветам.
pub fn shows_window(samples: [Option<Rgb>; 2]) -> bool {
    matches!(
        samples,
        [Some(left), Some(right)] if distance(left, LEFT) <= TOLERANCE && distance(right, RIGHT) <= TOLERANCE
    )
}

/// Аргумент `-R` для прямоугольника в координатах AppKit: `screencapture` считает от верхнего
/// левого угла главного экрана. Источник — `spikes/probe-ghostty-surface/src/scenario/snapshot.rs`.
pub fn region_arg(area: NSRect, primary_height: f64) -> String {
    let top = primary_height - (area.origin.y + area.size.height);
    format!(
        "{},{},{},{}",
        area.origin.x.round(),
        top.round(),
        area.size.width.round(),
        area.size.height.round()
    )
}

/// Прямоугольники пересекаются (координаты Core Graphics: `[x, y, ширина, высота]`).
pub fn intersects(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

/// Окно из списка окон WindowServer.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WindowEntry {
    pub number: i64,
    pub pid: i64,
    pub layer: i64,
    pub owner: String,
    pub bounds: [f64; 4],
}

/// Окна выше окна `own` в списке «спереди назад», которые пересекают его.
pub fn covering(entries: &[WindowEntry], own: i64) -> Vec<WindowEntry> {
    let Some(index) = entries.iter().position(|entry| entry.number == own) else {
        return Vec::new();
    };
    let bounds = entries[index].bounds;
    entries[..index]
        .iter()
        .filter(|entry| intersects(entry.bounds, bounds))
        .cloned()
        .collect()
}

/// Размер снимка в пикселях и пиксели в точках выборки.
type Samples = ((isize, isize), [Option<Rgb>; 2]);

/// Итог одного снимка.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Capture {
    pub command: String,
    pub attempts: usize,
    pub pixels: Option<(isize, isize)>,
    pub samples: [Option<Rgb>; 2],
    pub shows_window: bool,
    /// Отказ `screencapture` или чтения снимка в последней попытке.
    pub error: Option<String>,
}

/// Сырой артефакт разрешений.
#[derive(Clone, Debug, Serialize)]
pub struct Permissions {
    pub accessibility_trusted: bool,
    pub screen_capture_preflight: bool,
    pub window_number: Option<i64>,
    pub occlusion_visible: Option<bool>,
    pub expected: [Rgb; 2],
    pub tolerance: u8,
    pub window_capture: Option<Capture>,
    pub screen_capture: Option<Capture>,
    /// Окна, накрывающие окно пробы, если снимок экрана показал не его.
    pub covering: Vec<WindowEntry>,
}

/// Сведения окружения, без которых отчёт неполон.
pub fn missing_facts(environment: &Environment) -> Vec<String> {
    let mut missing = Vec::new();
    if environment.os.product_version.is_none() {
        missing.push("версия macOS".to_owned());
    }
    if environment.os.build.is_none() {
        missing.push("сборка macOS".to_owned());
    }
    if environment.hardware.model.is_none() {
        missing.push("модель".to_owned());
    }
    if environment.process.nonce.is_none() {
        missing.push("nonce".to_owned());
    }
    if environment.process.state_dir.is_none() {
        missing.push(launch::STATE_DIR.to_owned());
    }
    missing
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
}

fn accessibility_trusted() -> bool {
    // SAFETY: функция без аргументов.
    unsafe { AXIsProcessTrusted() != 0 }
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    results::write_json(out, ENVIRONMENT_ARTIFACT, environment)?;
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| "процесс сценария не на главном потоке".to_owned())?;
    let mut permissions = Permissions {
        accessibility_trusted: accessibility_trusted(),
        screen_capture_preflight: CGPreflightScreenCaptureAccess(),
        window_number: None,
        occlusion_visible: None,
        expected: [LEFT, RIGHT],
        tolerance: TOLERANCE,
        window_capture: None,
        screen_capture: None,
        covering: Vec::new(),
    };
    let checked = check_permissions(mtm, out, &mut permissions);
    results::write_json(out, PERMISSIONS_ARTIFACT, &permissions)?;
    checked?;
    Ok(report(environment, &permissions))
}

/// Проверка разрешений; каждое наблюдение записывается в `permissions` и при отказе.
fn check_permissions(
    mtm: MainThreadMarker,
    out: &Path,
    permissions: &mut Permissions,
) -> Result<(), String> {
    if !permissions.accessibility_trusted {
        return Err(
            "предусловие: терминал исполнителя без разрешения Accessibility \
                    (AXIsProcessTrusted = false); выдайте Accessibility терминалу, из которого \
                    идёт make, в Системных настройках → Конфиденциальность и безопасность"
                .to_owned(),
        );
    }
    if !permissions.screen_capture_preflight {
        return Err(
            "предусловие: терминал исполнителя без разрешения Screen Recording \
                    (CGPreflightScreenCaptureAccess = false); выдайте Screen Recording терминалу, \
                    из которого идёт make, и перезапустите его"
                .to_owned(),
        );
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    let window = paint_window(mtm);
    let number = window.windowNumber();
    permissions.window_number = Some(number as i64);
    let checked = capture_both(mtm, &app, &window, out, permissions);
    permissions.occlusion_visible = Some(
        window
            .occlusionState()
            .contains(NSWindowOcclusionState::Visible),
    );
    if permissions
        .screen_capture
        .as_ref()
        .is_some_and(|capture| !capture.shows_window)
    {
        permissions.covering = covering(&window_list(), number as i64);
    }
    window.close();
    checked?;
    let screen = permissions
        .screen_capture
        .as_ref()
        .ok_or_else(|| "снимок экрана не выполнен".to_owned())?;
    if !screen.shows_window {
        let covers: Vec<String> = permissions
            .covering
            .iter()
            .map(|entry| {
                format!(
                    "«{}» (pid {}, уровень {}, окно {})",
                    entry.owner, entry.pid, entry.layer, entry.number
                )
            })
            .collect();
        return Err(format!(
            "предусловие: снимок экрана (screencapture -R) показывает не окно пробы, а щит: \
             пиксели {:?} при ожидаемых {:?}; {}",
            screen.samples,
            [LEFT, RIGHT],
            if covers.is_empty() {
                "накрывающих окон в списке WindowServer нет".to_owned()
            } else {
                format!("окно пробы накрывают {}", covers.join(", "))
            }
        ));
    }
    Ok(())
}

/// Снимок окна, затем снимок экрана. Снимок экрана делается, только если снимок окна показал
/// окно: иначе причина — разрешение, а не щит.
fn capture_both(
    mtm: MainThreadMarker,
    app: &NSApplication,
    window: &NSWindow,
    out: &Path,
    permissions: &mut Permissions,
) -> Result<(), String> {
    let number = window.windowNumber();
    let by_window = capture_until_window(
        app,
        &["-x", "-o", "-l", &number.to_string()],
        &out.join(WINDOW_SNAPSHOT),
    )?;
    let window_ok = by_window.shows_window;
    permissions.window_capture = Some(by_window);
    if !window_ok {
        let capture = permissions.window_capture.as_ref();
        return Err(format!(
            "предусловие: Screen Recording не действует: снимок собственного окна \
             (screencapture -l) не показывает окно, пиксели {:?} при ожидаемых {:?}{}",
            capture.map(|capture| capture.samples),
            [LEFT, RIGHT],
            capture
                .and_then(|capture| capture.error.as_deref())
                .map(|error| format!("; {error}"))
                .unwrap_or_default()
        ));
    }
    let region = region_arg(window.frame(), primary_height(mtm));
    let by_screen = capture_until_window(app, &["-x", "-R", &region], &out.join(SCREEN_SNAPSHOT))?;
    permissions.screen_capture = Some(by_screen);
    Ok(())
}

/// Непрозрачное окно без рамки над обычными окнами: фон `LEFT`, правая половина — `RIGHT`.
fn paint_window(mtm: MainThreadMarker) -> Retained<NSWindow> {
    let (width, height) = WINDOW_SIZE;
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
    // SAFETY: обычная инициализация окна на главном потоке.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: окно держит Rust, поэтому AppKit не должен освобождать его при закрытии.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(true);
    window.setHasShadow(false);
    window.setBackgroundColor(Some(&srgb(LEFT)));
    let half = NSRect::new(
        NSPoint::new(width / 2.0, 0.0),
        NSSize::new(width / 2.0, height),
    );
    let inset = NSBox::initWithFrame(NSBox::alloc(mtm), half);
    inset.setBoxType(NSBoxType::Custom);
    inset.setTitlePosition(NSTitlePosition::NoTitle);
    inset.setBorderWidth(0.0);
    inset.setFillColor(&srgb(RIGHT));
    if let Some(content) = window.contentView() {
        content.addSubview(&inset);
    }
    window.setLevel(NSFloatingWindowLevel);
    window.center();
    window.orderFrontRegardless();
    window.display();
    window
}

fn srgb(color: Rgb) -> Retained<NSColor> {
    let channel = |value: u8| f64::from(value) / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(
        channel(color.0),
        channel(color.1),
        channel(color.2),
        1.0,
    )
}

/// Высота главного экрана: `screencapture -R` считает от его верхнего левого угла.
fn primary_height(mtm: MainThreadMarker) -> f64 {
    NSScreen::screens(mtm)
        .firstObject()
        .map_or(0.0, |screen| screen.frame().size.height)
}

/// Доставляет события процесса не дольше `wait`: AppKit рисует окно и отдаёт кадр WindowServer.
fn pump(app: &NSApplication, wait: Duration) {
    let until = NSDate::dateWithTimeIntervalSinceNow(wait.as_secs_f64());
    // SAFETY: чтение константы режима Foundation.
    let mode = unsafe { NSDefaultRunLoopMode };
    while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
        NSEventMask::Any,
        Some(&until),
        mode,
        true,
    ) {
        app.sendEvent(&event);
    }
}

/// Снимает `screencapture <args> <path>` до совпадения цветов или `ATTEMPTS` попыток.
fn capture_until_window(
    app: &NSApplication,
    args: &[&str],
    path: &Path,
) -> Result<Capture, String> {
    let mut capture = Capture {
        command: format!("{SCREENCAPTURE} {} {}", args.join(" "), path.display()),
        attempts: 0,
        pixels: None,
        samples: [None, None],
        shows_window: false,
        error: None,
    };
    while capture.attempts < ATTEMPTS && !capture.shows_window {
        pump(app, PUMP);
        capture.attempts += 1;
        let _ = std::fs::remove_file(path);
        match screencapture(app, args, path) {
            Ok(status) if status.success() => match read_samples(path) {
                Ok((pixels, samples)) => {
                    capture.pixels = Some(pixels);
                    capture.samples = samples;
                    capture.shows_window = shows_window(samples);
                    capture.error = None;
                }
                Err(error) => capture.error = Some(error),
            },
            Ok(status) => capture.error = Some(format!("screencapture: {status}")),
            Err(error) => return Err(error),
        }
    }
    Ok(capture)
}

/// Запускает `screencapture` и ждёт его, доставляя события: главный поток не блокируется.
/// Зависший процесс снимается по сроку, это ошибка исполнения.
fn screencapture(app: &NSApplication, args: &[&str], path: &Path) -> Result<ExitStatus, String> {
    let mut child: Child = Command::new(SCREENCAPTURE)
        .args(args)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("{SCREENCAPTURE} не запущен: {err}"))?;
    let pid = child.id() as i32;
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("probe-screencapture".to_owned())
        .spawn(move || {
            let _ = sender.send(child.wait());
        })
        .map_err(|err| format!("поток ожидания screencapture не запущен: {err}"))?;
    let until = Instant::now() + CAPTURE_TIMEOUT;
    loop {
        match receiver.recv_timeout(Duration::ZERO) {
            Ok(status) => return status.map_err(|err| format!("ожидание screencapture: {err}")),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("поток ожидания screencapture завершился без итога".to_owned());
            }
            Err(RecvTimeoutError::Timeout) if Instant::now() >= until => {
                // SAFETY: сигнал собственному потомку по PID, памяти вызов не касается.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                return Err(format!(
                    "screencapture не завершился за {} с",
                    CAPTURE_TIMEOUT.as_secs()
                ));
            }
            Err(RecvTimeoutError::Timeout) => pump(app, Duration::from_millis(20)),
        }
    }
}

/// Размер снимка и пиксели в точках выборки, в sRGB. Источник —
/// `spikes/probe-ghostty-surface/src/scenario/snapshot.rs`.
fn read_samples(path: &Path) -> Result<Samples, String> {
    let bytes = std::fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    autoreleasepool(|_| {
        let data = NSData::with_bytes(&bytes);
        let rep = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data)
            .ok_or_else(|| format!("{}: PNG не разобран", path.display()))?;
        let pixels = (rep.pixelsWide(), rep.pixelsHigh());
        let srgb = NSColorSpace::sRGBColorSpace();
        let channel = |value: f64| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        let samples = sample_points(pixels).map(|(x, y)| {
            rep.colorAtX_y(x, y)
                .and_then(|color| color.colorUsingColorSpace(&srgb))
                .map(|color| {
                    Rgb(
                        channel(color.redComponent()),
                        channel(color.greenComponent()),
                        channel(color.blueComponent()),
                    )
                })
        });
        Ok((pixels, samples))
    })
}

fn number_of(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<f64> {
    dict.get(key)
        .and_then(|value| value.downcast_ref::<CFNumber>().and_then(CFNumber::as_f64))
}

/// Окно из словаря списка окон. Источник — `spikes/probe-ghostty-surface/src/scenario/visibility.rs`.
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

/// Окна на экране спереди назад.
pub fn window_list() -> Vec<WindowEntry> {
    let Some(list) = CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, 0) else {
        return Vec::new();
    };
    // SAFETY: элементы списка окон — словари со строковыми ключами.
    let list: &CFArray<CFDictionary<CFString, CFType>> = unsafe { list.cast_unchecked() };
    list.iter().filter_map(|dict| window_entry(&dict)).collect()
}

/// Итог `smoke`: одна клетка стенда. Разрешения к этому месту уже подтверждены; неполное
/// окружение — `fail`, а не ошибка исполнения: процесс дошёл до конца, но сведений не хватает.
pub fn report(environment: &Environment, permissions: &Permissions) -> Report {
    let missing = missing_facts(environment);
    let attempts = |capture: &Option<Capture>| capture.as_ref().map_or(0, |c| c.attempts);
    let (status, reason) = if missing.is_empty() {
        (
            Status::Pass,
            format!(
                "AXIsProcessTrusted = true; CGPreflightScreenCaptureAccess = true; снимок окна \
                 (попыток {}) и снимок экрана (попыток {}) показывают окно пробы; macOS {} ({}), \
                 временный {} {}",
                attempts(&permissions.window_capture),
                attempts(&permissions.screen_capture),
                environment
                    .os
                    .product_version
                    .as_deref()
                    .unwrap_or_default(),
                environment.os.build.as_deref().unwrap_or_default(),
                launch::STATE_DIR,
                environment.process.state_dir.as_deref().unwrap_or_default()
            ),
        )
    } else {
        (
            Status::Fail,
            format!("в окружении нет: {}", missing.join(", ")),
        )
    };
    let mut report = Report::default();
    report.cells.push(Cell {
        id: SMOKE_CELL.to_owned(),
        part: None,
        requirement: Requirement::Required,
        status,
        stimulus: "процесс сценария запущен через --launch из терминала исполнителя; окно пробы \
                   двух известных цветов снято screencapture -l и -R"
            .to_owned(),
        oracle: "Accessibility доверяет процессу; оба снимка показывают цвета окна в точках \
                 выборки; в окружении есть ОС, модель, nonce и временный SHELLIO_STATE_DIR"
            .to_owned(),
        completion: format!(
            "записаны {ENVIRONMENT_ARTIFACT}, {PERMISSIONS_ARTIFACT}, оба снимка и {}",
            results::RESULT_FILE
        ),
        reason,
        artifacts: vec![
            ENVIRONMENT_ARTIFACT.to_owned(),
            PERMISSIONS_ARTIFACT.to_owned(),
            WINDOW_SNAPSHOT.to_owned(),
            SCREEN_SNAPSHOT.to_owned(),
        ],
    });
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::collect_environment;

    #[test]
    fn samples_classify_window_against_shield() {
        assert!(shows_window([Some(LEFT), Some(RIGHT)]));
        assert!(shows_window([
            Some(Rgb(230, 30, 150)),
            Some(Rgb(40, 170, 90))
        ]));
        // Щит или окно без содержимого: один тон в обеих точках.
        assert!(!shows_window([Some(Rgb(0, 0, 0)), Some(Rgb(0, 0, 0))]));
        assert!(!shows_window([Some(LEFT), Some(LEFT)]));
        assert!(!shows_window([Some(RIGHT), Some(LEFT)]));
        assert!(!shows_window([Some(LEFT), None]));
        assert!(!shows_window([None, None]));
        assert_eq!(distance(LEFT, RIGHT), 191);
        assert!(distance(LEFT, RIGHT) > 2 * TOLERANCE);
    }

    #[test]
    fn sample_points_hit_both_halves() {
        assert_eq!(sample_points((480, 320)), [(120, 160), (360, 160)]);
        assert_eq!(sample_points((240, 160)), [(60, 80), (180, 80)]);
    }

    #[test]
    fn region_counts_from_the_top_of_the_primary_screen() {
        let area = NSRect::new(NSPoint::new(100.0, 200.0), NSSize::new(240.0, 160.0));
        assert_eq!(region_arg(area, 1000.0), "100,640,240,160");
    }

    fn entry(number: i64, bounds: [f64; 4]) -> WindowEntry {
        WindowEntry {
            number,
            pid: number * 10,
            layer: 0,
            owner: format!("w{number}"),
            bounds,
        }
    }

    #[test]
    fn covering_windows_are_above_and_overlapping() {
        let own = [100.0, 100.0, 240.0, 160.0];
        let entries = vec![
            entry(1, [0.0, 0.0, 2000.0, 1200.0]),
            entry(2, [400.0, 400.0, 50.0, 50.0]),
            entry(3, [300.0, 200.0, 100.0, 100.0]),
            entry(7, own),
            entry(4, [0.0, 0.0, 2000.0, 1200.0]),
        ];
        let numbers: Vec<i64> = covering(&entries, 7)
            .iter()
            .map(|entry| entry.number)
            .collect();
        assert_eq!(numbers, [1, 3]);
        assert!(covering(&entries, 99).is_empty());
        assert!(!intersects([0.0, 0.0, 10.0, 10.0], [10.0, 0.0, 5.0, 5.0]));
    }

    fn permissions() -> Permissions {
        let capture = Capture {
            command: "screencapture".to_owned(),
            attempts: 1,
            pixels: Some((480, 320)),
            samples: [Some(LEFT), Some(RIGHT)],
            shows_window: true,
            error: None,
        };
        Permissions {
            accessibility_trusted: true,
            screen_capture_preflight: true,
            window_number: Some(7),
            occlusion_visible: Some(true),
            expected: [LEFT, RIGHT],
            tolerance: TOLERANCE,
            window_capture: Some(capture.clone()),
            screen_capture: Some(capture),
            covering: Vec::new(),
        }
    }

    #[test]
    fn report_is_one_stand_cell() {
        let entry = vec![(launch::STATE_DIR.to_owned(), "/tmp/s".to_owned())];
        let complete = collect_environment(&entry, Some("n1".to_owned()), Vec::new());
        let report = report(&complete, &permissions());
        assert_eq!(report.cells.len(), 1);
        let cell = &report.cells[0];
        assert_eq!(cell.status, Status::Pass, "{}", cell.reason);
        assert!(cell.problems().is_empty(), "{:?}", cell.problems());
        assert_eq!(cell.artifacts.len(), 4);

        let incomplete = collect_environment(&[], None, Vec::new());
        let report = super::report(&incomplete, &permissions());
        assert_eq!(report.cells[0].status, Status::Fail);
        assert!(report.cells[0].reason.contains("nonce"));
        assert!(report.cells[0].reason.contains(launch::STATE_DIR));
    }
}
