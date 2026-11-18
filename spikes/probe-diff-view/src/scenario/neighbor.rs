//! Служебный процесс `--neighbor` обратного контроля калибровки: узкое окно без рамки у левого края
//! дисплея, его полосы движутся по S1 от `displayLink`. Приложение не активируется и key у пробы
//! не отбирает. Процесс печатает свой pid, живёт до закрытия stdin и перед выходом печатает число
//! своих тактов — свидетельство движения, независимое от трассы.

use std::cell::Cell;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSScreen, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize};

use super::calibrate_run::SPEED;
use super::stand::{FrameDriver, MotionView, s1_offset};

/// Ширина окна соседа и отступ от края видимой области, точки. Окно пробы стоит по центру, сбоку
/// от него на встроенном дисплее остаётся больше этой ширины.
pub const WIDTH: f64 = 120.0;
pub const MARGIN: f64 = 8.0;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Строит окно соседа и крутит основной цикл. Возвращается только с отказом.
pub fn run(mtm: MainThreadMarker, screen: &NSScreen) -> Result<(), String> {
    let app = NSApplication::sharedApplication(mtm);
    if !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        return Err("setActivationPolicy(.accessory) отклонён".to_owned());
    }
    let full = screen.frame();
    let visible = screen.visibleFrame();
    let frame = NSRect::new(
        NSPoint::new(
            visible.origin.x - full.origin.x + MARGIN,
            visible.origin.y - full.origin.y + MARGIN,
        ),
        NSSize::new(WIDTH, (visible.size.height - 2.0 * MARGIN).floor()),
    );
    // SAFETY: обычная инициализация окна с существующим экраном.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer_screen(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
            Some(screen),
        )
    };
    // SAFETY: окно держит Rust до выхода процесса.
    unsafe { window.setReleasedWhenClosed(false) };
    let view = MotionView::new(mtm, NSRect::new(NSPoint::ZERO, frame.size));
    window.setContentView(Some(&view));
    window.orderFrontRegardless();

    let start = Cell::new(None::<f64>);
    let moving = view.clone();
    let driver = FrameDriver::new(
        mtm,
        Box::new(move |link| {
            let target = link.targetTimestamp();
            let origin = start.get().unwrap_or(target);
            start.set(Some(origin));
            moving.move_to(s1_offset(origin, target, SPEED));
            TICKS.fetch_add(1, Ordering::Relaxed);
        }),
    );
    // SAFETY: цель — объект пробы с методом `tick:`, принимающим `CADisplayLink`.
    let link = unsafe { view.displayLinkWithTarget_selector(&driver, sel!(tick:)) };
    // SAFETY: ссылка добавляется в цикл главного потока в общих режимах.
    unsafe { link.addToRunLoop_forMode(&NSRunLoop::currentRunLoop(), NSRunLoopCommonModes) };

    std::thread::Builder::new()
        .name("probe-neighbor-stdin".to_owned())
        .spawn(|| {
            let mut sink = Vec::new();
            let _ = std::io::stdin().lock().read_to_end(&mut sink);
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "neighbor_ticks={}", TICKS.load(Ordering::Relaxed));
            let _ = stdout.flush();
            // SAFETY: `_exit` завершает процесс без обработчиков `atexit`, главный поток в цикле.
            unsafe { libc::_exit(0) }
        })
        .map_err(|err| format!("поток stdin не запущен: {err}"))?;
    let _ = writeln!(
        std::io::stdout().lock(),
        "neighbor_pid={}",
        std::process::id()
    );
    app.run();
    drop((link, driver, view, window));
    Err("основной цикл соседа завершился".to_owned())
}
