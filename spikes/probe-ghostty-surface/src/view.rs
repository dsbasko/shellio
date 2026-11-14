//! Вид поверхности: подкласс `NSView`, который libghostty делает layer-hosting в
//! `ghostty_surface_new` (`platform.macos.nsview`). Вид сам `layer` и `wantsLayer` не ставит.
//!
//! Вид подаёт поверхности размер и масштаб в пикселях с `refresh` после каждой подачи размера,
//! фокус и `set_display_id`. Вне окна размер и масштаб не подаются: у вида без окна нет своей
//! подложки, и подача дала бы ребёнку лишний `SIGWINCH`. Ручку поверхности вид получает в [`SurfaceView::attach`] и снимает в
//! [`SurfaceView::detach`] до освобождения; состояние runtime он не трогает.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSResponder, NSView, NSWindowDidChangeScreenNotification};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObject, NSRect, NSSize};
use objc2_quartz_core::CATransaction;
use serde::Serialize;

use crate::ghostty::surface::Handle;
use crate::scenario::counters::{Live, counter};

counter!(SURFACE_VIEW, "ProbeSurfaceView");

/// Что вид подал поверхности.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Feeds {
    pub size_feeds: u64,
    pub refreshes: u64,
    /// Подачи, пропущенные из-за нулевого размера в пикселях.
    pub zero_size_skipped: u64,
    /// Подачи размера и масштаба, пропущенные, пока вид вне окна.
    pub detached_skipped: u64,
    pub last_size_px: Option<(u32, u32)>,
    pub scale_feeds: u64,
    pub last_scale: Option<(f64, f64)>,
    pub display_ids: Vec<u32>,
    pub focus: Vec<bool>,
}

pub struct ViewIvars {
    _live: Live,
    handle: Cell<Option<Handle>>,
    focused: Cell<bool>,
    feeds: RefCell<Feeds>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeSurfaceView"]
    #[ivars = ViewIvars]
    pub struct SurfaceView;

    impl SurfaceView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            true
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: вызов реализации суперкласса без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if accepted {
                self.set_focused(true);
            }
            accepted
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            // SAFETY: вызов реализации суперкласса без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), resignFirstResponder] };
            if accepted {
                self.set_focused(false);
            }
            accepted
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            // SAFETY: вызов реализации суперкласса с тем же размером.
            let () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.feed_size();
        }

        #[unsafe(method(viewDidChangeBackingProperties))]
        fn view_did_change_backing_properties(&self) {
            // SAFETY: вызов реализации суперкласса без аргументов.
            let () = unsafe { msg_send![super(self), viewDidChangeBackingProperties] };
            self.backing_changed();
        }

        #[unsafe(method(viewDidMoveToWindow))]
        fn view_did_move_to_window(&self) {
            // SAFETY: вызов реализации суперкласса без аргументов.
            let () = unsafe { msg_send![super(self), viewDidMoveToWindow] };
            self.observe_screen();
            self.feed_display();
            self.backing_changed();
        }

        #[unsafe(method(windowDidChangeScreen:))]
        fn window_did_change_screen(&self, _notification: &NSNotification) {
            self.feed_display();
            self.backing_changed();
        }
    }
);

impl SurfaceView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<SurfaceView> {
        let this = SurfaceView::alloc(mtm).set_ivars(ViewIvars {
            _live: SURFACE_VIEW.live(),
            handle: Cell::new(None),
            focused: Cell::new(false),
            feeds: RefCell::new(Feeds::default()),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Размер содержимого в пикселях подложки, с отбрасыванием дробной части.
    pub fn pixel_size(&self) -> (u32, u32) {
        crate::scene::to_pixels(self.bounds().size, self.backing_scale())
    }

    /// Масштаб подложки окна или главного экрана, если окна ещё нет.
    pub fn backing_scale(&self) -> f64 {
        match self.window() {
            Some(window) => window.backingScaleFactor(),
            None => {
                let size = self.convertSizeToBacking(NSSize::new(1.0, 1.0));
                if size.width > 0.0 { size.width } else { 1.0 }
            }
        }
    }

    pub fn feeds(&self) -> Feeds {
        self.ivars().feeds.borrow().clone()
    }

    pub fn is_attached(&self) -> bool {
        self.ivars().handle.get().is_some()
    }

    /// Поверхность создана: рождённая с размером 800×600, она сразу получает масштаб, размер в
    /// пикселях с `refresh`, дисплей и фокус.
    pub fn attach(&self, handle: Handle) {
        self.ivars().handle.set(Some(handle));
        self.feed_scale();
        self.feed_size();
        self.feed_display();
        let focused = self.ivars().focused.get();
        self.feed_focus(focused);
    }

    /// Ручка снимается до `ghostty_surface_free`.
    pub fn detach(&self) {
        self.ivars().handle.set(None);
    }

    fn set_focused(&self, focused: bool) {
        self.ivars().focused.set(focused);
        self.feed_focus(focused);
    }

    fn feed_focus(&self, focused: bool) {
        let Some(handle) = self.ivars().handle.get() else {
            return;
        };
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        unsafe { handle.set_focus(focused) };
        self.ivars().feeds.borrow_mut().focus.push(focused);
    }

    /// Ручка для подачи размера и масштаба: только пока вид в окне.
    fn sized_handle(&self) -> Option<Handle> {
        let handle = self.ivars().handle.get()?;
        if self.window().is_none() {
            self.ivars().feeds.borrow_mut().detached_skipped += 1;
            return None;
        }
        Some(handle)
    }

    fn feed_size(&self) {
        let Some(handle) = self.sized_handle() else {
            return;
        };
        let (width, height) = self.pixel_size();
        if width == 0 || height == 0 {
            self.ivars().feeds.borrow_mut().zero_size_skipped += 1;
            return;
        }
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        unsafe {
            handle.set_size(width, height);
            handle.refresh();
        }
        let mut feeds = self.ivars().feeds.borrow_mut();
        feeds.size_feeds += 1;
        feeds.refreshes += 1;
        feeds.last_size_px = Some((width, height));
    }

    fn feed_scale(&self) {
        let Some(handle) = self.sized_handle() else {
            return;
        };
        let frame = self.frame();
        if frame.size.width <= 0.0 || frame.size.height <= 0.0 {
            return;
        }
        let backing = self.convertRectToBacking(frame);
        let x = backing.size.width / frame.size.width;
        let y = backing.size.height / frame.size.height;
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        unsafe { handle.set_content_scale(x, y) };
        let mut feeds = self.ivars().feeds.borrow_mut();
        feeds.scale_feeds += 1;
        feeds.last_scale = Some((x, y));
    }

    fn feed_display(&self) {
        let Some(handle) = self.ivars().handle.get() else {
            return;
        };
        let Some(screen) = self.window().and_then(|window| window.screen()) else {
            return;
        };
        let display = screen.CGDirectDisplayID();
        // SAFETY: ручка снимается до освобождения поверхности, вызов на главном потоке.
        unsafe { handle.set_display_id(display) };
        self.ivars().feeds.borrow_mut().display_ids.push(display);
    }

    /// Смена масштаба подложки: `contentsScale` слоя без анимации, затем масштаб и размер.
    fn backing_changed(&self) {
        if let (Some(layer), Some(window)) = (self.layer(), self.window()) {
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            layer.setContentsScale(window.backingScaleFactor());
            CATransaction::commit();
        }
        self.feed_scale();
        self.feed_size();
    }

    /// Подписка на смену экрана окна переустанавливается при каждом переходе в другое окно.
    fn observe_screen(&self) {
        let center = NSNotificationCenter::defaultCenter();
        // SAFETY: имя уведомления — неизменяемая константа AppKit; `windowDidChangeScreen:`
        // реализован видом. Подписки по селектору снимаются сами при освобождении вида.
        unsafe {
            center.removeObserver_name_object(
                self,
                Some(NSWindowDidChangeScreenNotification),
                None,
            );
            if let Some(window) = self.window() {
                center.addObserver_selector_name_object(
                    self,
                    sel!(windowDidChangeScreen:),
                    Some(NSWindowDidChangeScreenNotification),
                    Some(&window),
                );
            }
        }
    }
}
