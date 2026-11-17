//! Окно и вид поверхности. Вид — обычный `NSView` без обработки событий, раскладки и слежения за
//! окном: libghostty сама ставит ему слой, а размер, фокус, видимость и схема меняются только
//! явными вызовами стенда. Так внешние события (например `mouseMoved` от оконного менеджера)
//! до ghostty не доходят и отчётов не порождают.
//!
//! Главный цикл не запускается `NSApplication::run`: стенд крутит `CFRunLoop` короткими проходами
//! и между ними проверяет квитанции фикстуры и просьбы libghostty о проходе.

use std::time::Duration;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use super::keyboard::KeyView;

pub struct Shell {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
}

impl Shell {
    pub fn new(mtm: MainThreadMarker, title: &str) -> Shell {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();
        let rect = NSRect::new(NSPoint::new(80.0, 80.0), NSSize::new(640.0, 400.0));
        // SAFETY: обычная инициализация окна на главном потоке.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                NSWindowStyleMask::Titled,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: окно держит Rust, поэтому AppKit не должен освобождать его при закрытии.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(title));
        window.orderFrontRegardless();
        Shell { mtm, window }
    }

    /// Новый вид с клавиатурой: содержимое окна и его first responder. Окно не становится key и
    /// приложение не активируется: события стенд доставляет сам (`surface::input`).
    pub fn key_view(&self, width: f64, height: f64) -> Retained<KeyView> {
        let size = NSSize::new(width, height);
        let view = KeyView::new(self.mtm, NSRect::new(NSPoint::ZERO, size));
        self.window.setContentSize(size);
        self.window.setContentView(Some(&view));
        self.window.makeFirstResponder(Some(&view));
        view
    }

    pub fn app(&self) -> Retained<NSApplication> {
        NSApplication::sharedApplication(self.mtm)
    }

    /// Новый вид содержимого окна; прежний вид снимается с окна.
    pub fn view(&self, width: f64, height: f64) -> Retained<NSView> {
        let size = NSSize::new(width, height);
        let view = NSView::initWithFrame(NSView::alloc(self.mtm), NSRect::new(NSPoint::ZERO, size));
        self.window.setContentSize(size);
        self.window.setContentView(Some(&view));
        view
    }

    pub fn scale(&self) -> f64 {
        self.window.backingScaleFactor()
    }

    pub fn close(&self) {
        self.window.setContentView(None);
        self.window.orderOut(None);
    }
}

/// Один проход главного цикла не дольше `timeout`; возвращается раньше, когда сработал источник
/// или libghostty разбудила цикл.
pub fn pump(timeout: Duration) {
    // SAFETY: чтение константы режима CoreFoundation.
    let mode = unsafe { kCFRunLoopDefaultMode };
    CFRunLoop::run_in_mode(mode, timeout.as_secs_f64(), true);
}
