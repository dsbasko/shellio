//! Такты `CADisplayLink` окна владельца на время серии замера (план `command-overlays`, правило
//! «Бюджет палитры», блоки A). Display link будит главный цикл каждый кадр и меняет то, что мерит,
//! поэтому живёт только пока жив [`DisplayTicks`]: `Drop` снимает его, и в блоке B цикл снова спит.

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes};
use objc2_quartz_core::CADisplayLink;

use crate::counters::{Live, counter};
use crate::window::MainWindow;

counter!(DISPLAY_TICKER, Class, "ShellioDisplayTicker");

/// Такты окна идут в `on_tick` моментом такта (`CADisplayLink.timestamp`, секунды с загрузки).
pub struct DisplayTicks {
    link: Retained<CADisplayLink>,
}

impl DisplayTicks {
    /// Заводит display link корневого вида окна в общих режимах главного цикла.
    pub fn start(window: &MainWindow, on_tick: impl Fn(f64) + 'static) -> DisplayTicks {
        let view = window.root_view();
        let ticker = DisplayTicker::alloc(view.mtm()).set_ivars(TickerIvars {
            _live: DISPLAY_TICKER.live(),
            on_tick: Box::new(on_tick),
        });
        // SAFETY: `init` у NSObject, ivars уже заданы.
        let ticker: Retained<DisplayTicker> = unsafe { msg_send![super(ticker), init] };
        // SAFETY: цель — объект с методом `tick:` для `CADisplayLink`; ссылка держит цель до
        // `invalidate` в `Drop`.
        let link = unsafe { view.displayLinkWithTarget_selector(&ticker, sel!(tick:)) };
        // SAFETY: главный цикл и общий режим — константы Foundation, ссылка создана на главном
        // потоке.
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
        DisplayTicks { link }
    }
}

impl Drop for DisplayTicks {
    fn drop(&mut self) {
        // Снятая ссылка уходит из цикла и отпускает цель.
        self.link.invalidate();
    }
}

struct TickerIvars {
    _live: Live,
    on_tick: Box<dyn Fn(f64)>,
}

define_class!(
    // SAFETY: NSObject не требует от подкласса ничего сверх `init`, `Drop` у класса нет.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioDisplayTicker"]
    #[ivars = TickerIvars]
    struct DisplayTicker;

    impl DisplayTicker {
        // SAFETY: селектор и аргумент — как у цели `CADisplayLink`: метод с одним аргументом-ссылкой.
        #[unsafe(method(tick:))]
        fn tick(&self, link: &CADisplayLink) {
            (self.ivars().on_tick)(link.timestamp());
        }
    }
);
