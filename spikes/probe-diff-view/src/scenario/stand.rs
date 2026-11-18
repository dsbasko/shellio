//! Объекты стенда калибровки: цель `displayLink` и вид с движущимися полосами, который в конце
//! `drawRect:` ставит отметку поколения. Цель `displayLink` — из
//! `spikes/probe-appkit-shell/src/scenario/frames.rs` (P0.2), без изменений поведения.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{NSColor, NSRectFill, NSView};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize};
use objc2_quartz_core::CADisplayLink;

use super::counters::{Live, counter};
use super::drawmark::{self, Mark};
use super::sampler;

counter!(FRAME_DRIVER, "ProbeDiffFrameDriver");
counter!(MOTION_VIEW, "ProbeDiffMotionView");

type TickHandler = Box<dyn FnMut(&CADisplayLink)>;

pub struct DriverIvars {
    _live: Live,
    handler: RefCell<Option<TickHandler>>,
    cleared: Cell<bool>,
}

define_class!(
    /// Цель `displayLink`: каждый вызов уходит обработчику сценария.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeDiffFrameDriver"]
    #[ivars = DriverIvars]
    pub struct FrameDriver;

    unsafe impl NSObjectProtocol for FrameDriver {}

    impl FrameDriver {
        #[unsafe(method(tick:))]
        fn tick(&self, link: &CADisplayLink) {
            // Снятие обработчика изнутри вызова отпускает последний `Retained` цели, поэтому цель
            // удерживается до конца вызова.
            let _keep = self.retain();
            let handler = self.ivars().handler.borrow_mut().take();
            if let Some(mut handler) = handler {
                handler(link);
                if !self.ivars().cleared.get() {
                    *self.ivars().handler.borrow_mut() = Some(handler);
                }
            }
        }
    }
);

impl FrameDriver {
    pub fn new(mtm: MainThreadMarker, handler: TickHandler) -> Retained<FrameDriver> {
        let this = FrameDriver::alloc(mtm).set_ivars(DriverIvars {
            _live: FRAME_DRIVER.live(),
            handler: RefCell::new(Some(handler)),
            cleared: Cell::new(false),
        });
        // SAFETY: `init` у NSObject без аргументов.
        unsafe { msg_send![super(this), init] }
    }

    /// Снимает обработчик. Вызов из обработчика снимает его после возврата из `tick:`.
    pub fn clear(&self) {
        self.ivars().cleared.set(true);
        drop(self.ivars().handler.borrow_mut().take());
    }
}

/// Период полос вида, точки.
pub const STRIPE_PERIOD: f64 = 64.0;

pub struct MotionIvars {
    _live: Live,
    offset: Cell<f64>,
    generation: Cell<u64>,
}

define_class!(
    /// Вид с горизонтальными полосами, сдвинутыми на `offset`. Каждое изменение сдвига — новое
    /// поколение, `drawRect:` отмечает его в конце прохода.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeDiffMotionView"]
    #[ivars = MotionIvars]
    pub struct MotionView;

    impl MotionView {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            let start = sampler::uptime_ns().unwrap_or(0);
            let bounds = self.bounds();
            NSColor::textBackgroundColor().setFill();
            NSRectFill(dirty);
            NSColor::textColor().setFill();
            let phase = self.ivars().offset.get().rem_euclid(STRIPE_PERIOD);
            let mut y = bounds.origin.y - STRIPE_PERIOD + phase;
            while y < bounds.origin.y + bounds.size.height {
                NSRectFill(NSRect::new(
                    NSPoint::new(bounds.origin.x, y),
                    NSSize::new(bounds.size.width, STRIPE_PERIOD / 2.0),
                ));
                y += STRIPE_PERIOD;
            }
            drawmark::mark(Mark {
                generation: self.ivars().generation.get(),
                rows: 0..1,
                draw_start_ns: start,
                draw_end_ns: sampler::uptime_ns().unwrap_or(0),
            });
        }
    }
);

impl MotionView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<MotionView> {
        let this = MotionView::alloc(mtm).set_ivars(MotionIvars {
            _live: MOTION_VIEW.live(),
            offset: Cell::new(0.0),
            generation: Cell::new(0),
        });
        // SAFETY: назначенный инициализатор NSView с прямоугольником.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Ставит сдвиг и начинает новое поколение. Тот же сдвиг поколение не меняет: удержание.
    pub fn move_to(&self, offset: f64) -> u64 {
        let ivars = self.ivars();
        if ivars.offset.get() != offset {
            ivars.offset.set(offset);
            ivars.generation.set(ivars.generation.get() + 1);
            self.setNeedsDisplay(true);
        }
        ivars.generation.get()
    }

    pub fn generation(&self) -> u64 {
        self.ivars().generation.get()
    }
}

/// Положение S1 по независимой шкале времени: `y0 + скорость × (target − t0)`, точки.
pub fn s1_offset(start_s: f64, target_s: f64, speed: f64) -> f64 {
    speed * (target_s - start_s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s1_is_a_function_of_time() {
        assert_eq!(s1_offset(10.0, 10.5, 4800.0), 2400.0);
        assert_eq!(s1_offset(10.0, 10.0, 4800.0), 0.0);
        let skipped = s1_offset(0.0, 3.0 / 120.0, 4800.0) - s1_offset(0.0, 1.0 / 120.0, 4800.0);
        assert!((skipped - 80.0).abs() < 1e-9);
    }
}
