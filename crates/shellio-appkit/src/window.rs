//! Окно владельца (ADR-0006, ADR-0023, ADR-0024): 1180×720 точек, минимум тот же, содержимое под
//! прозрачным системным заголовком, системные кнопки окна и [`ChromeBar`] высотой 40 точек.
//!
//! Окно создаётся после выбора роли «владелец» и живёт до выхода процесса. Закрытие (кнопка окна,
//! `Cmd-W`, `window.close`) только прячет его: владелец, сокет и ядро продолжают работать.
//! Пространство цвета окна — sRGB: вуаль и тон смешиваются в sRGB независимо от дисплея
//! (ограничение 15 P0.2). Системный заголовок окна на экране не рисуется, его читают Mission
//! Control, меню «Окно» и VoiceOver.

use std::cell::{Cell, OnceCell};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::NSRunningApplication;
use objc2_app_kit::{
    NSAppearanceCustomization, NSAutoresizingMaskOptions, NSBackingStoreType, NSColorSpace,
    NSResponder, NSTitlebarSeparatorStyle, NSView, NSViewLayerContentsRedrawPolicy, NSWindow,
    NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_foundation::{NSNotification, NSPoint, NSRect, NSSize, NSString};
use shellio_theme::{AppTheme, Overrides};

use crate::app::App;
use crate::chrome_bar::{ChromeBar, ChromeIntent};
use crate::counters::{Live, counter};
use crate::inspect::{self, Inspection, WindowFacts};
use crate::layer::{self, without_actions};
use crate::own::{Callback, ComponentId};
use crate::runloop::{self, LoopMonitor};
use crate::theme_cell::{self, CellObserver, ThemeSelection};

counter!(MAIN_WINDOW_OWNER, Owner, "MainWindow");
counter!(WINDOW_DELEGATE, Class, "ShellioWindowDelegate");
counter!(WINDOW_ROOT, Class, "ShellioWindowRoot");

/// Размер окна и его минимум в точках (ADR-0024).
pub const WINDOW_WIDTH: f64 = 1180.0;
pub const WINDOW_HEIGHT: f64 = 720.0;
/// Системный заголовок до первого снимка зеркала и без активного воркспейса.
pub const DEFAULT_TITLE: &str = "shellio";

/// Части окна. Обратные вызовы видов и делегата достают их через слабую ссылку.
struct Parts {
    _live: Live,
    id: ComponentId,
    // Порядок полей — порядок освобождения: сначала подписка наблюдателя.
    observer: CellObserver,
    chrome: ChromeBar,
    root: Retained<RootView>,
    delegate: Retained<WindowDelegate>,
    window: Retained<NSWindow>,
    selection: Cell<ThemeSelection>,
    on_theme: Box<dyn Fn(ThemeSelection)>,
    /// Время с загрузки, когда окно впервые стало key.
    first_key: Cell<Option<f64>>,
}

impl Parts {
    /// Новая ячейка или движение: окно и заголовок перекрашиваются без неявных анимаций,
    /// получатель узнаёт о смене после перекраски.
    fn apply(&self, selection: ThemeSelection) {
        self.selection.set(selection);
        let theme = selection.theme();
        self.window
            .setBackgroundColor(Some(&layer::ns_color(theme.colors.chrome)));
        self.chrome.set_theme(&theme);
        self.root.setNeedsDisplay(true);
        (self.on_theme)(selection);
    }

    fn theme(&self) -> AppTheme {
        self.selection.get().theme()
    }
}

/// Окно владельца на главном потоке.
pub struct MainWindow {
    parts: Rc<Parts>,
}

impl MainWindow {
    /// Создаёт окно, не показывая его. `on_theme` получает новый выбор ячейки после перекраски,
    /// `on_intent` — намерения заголовка. Оба вызываются на главном потоке изнутри AppKit, поэтому
    /// не должны синхронно трогать владельца окна.
    pub fn new(
        app: &App,
        on_theme: impl Fn(ThemeSelection) + 'static,
        on_intent: impl Fn(ChromeIntent) + 'static,
    ) -> MainWindow {
        let mtm = app.mtm();
        let parts = Rc::new_cyclic(|weak: &std::rc::Weak<Parts>| {
            let weak = weak.clone();
            let observer = CellObserver::new(app, Overrides::default(), move |selection| {
                if let Some(parts) = weak.upgrade() {
                    parts.apply(selection);
                }
            });
            let selection = observer.selection();
            let theme = selection.theme();
            Parts {
                _live: MAIN_WINDOW_OWNER.live(),
                id: ComponentId::next(),
                chrome: ChromeBar::new(app, &theme, on_intent),
                observer,
                root: RootView::new(mtm),
                delegate: WindowDelegate::new(mtm),
                window: new_window(mtm, &theme),
                selection: Cell::new(selection),
                on_theme: Box::new(on_theme),
                first_key: Cell::new(None),
            }
        });
        let _ = parts
            .root
            .ivars()
            .owner
            .set(Callback::new(&parts, "MainWindow.root"));
        let _ = parts
            .delegate
            .ivars()
            .owner
            .set(Callback::new(&parts, "MainWindow.delegate"));
        let window = &parts.window;
        window.setDelegate(Some(ProtocolObject::from_ref(&*parts.delegate)));
        window.setContentView(Some(&parts.root));
        let chrome = parts.chrome.ns_view();
        let height = parts.theme().metrics.titlebar;
        chrome.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(parts.root.bounds().size.width, height),
        ));
        parts.root.addSubview(chrome);
        // Светлота с этой минуты — из оформления содержимого окна.
        parts.observer.attach(&parts.root);
        MainWindow { parts }
    }

    /// Показывает окно и делает его key (`makeKeyAndOrderFront`).
    pub fn show(&self) {
        self.parts.window.makeKeyAndOrderFront(None);
    }

    /// Прячет окно, ничего не освобождая.
    pub fn hide(&self) {
        self.parts.window.orderOut(None);
    }

    pub fn is_visible(&self) -> bool {
        self.parts.window.isVisible()
    }

    pub fn is_key(&self) -> bool {
        self.parts.window.isKeyWindow()
    }

    /// `windowNumber`: номер окна у оконного сервера, постоянный до выхода процесса.
    pub fn number(&self) -> i64 {
        i64::try_from(self.parts.window.windowNumber()).unwrap_or(-1)
    }

    /// Системный заголовок; `None` — заголовок по умолчанию.
    pub fn set_title(&self, title: Option<&str>) {
        let title = NSString::from_str(title.unwrap_or(DEFAULT_TITLE));
        if *self.parts.window.title() != *title {
            self.parts.window.setTitle(&title);
        }
    }

    pub fn title(&self) -> String {
        self.parts.window.title().to_string()
    }

    pub fn chrome(&self) -> &ChromeBar {
        &self.parts.chrome
    }

    /// Действующие ячейка и движение с источниками осей.
    pub fn selection(&self) -> ThemeSelection {
        self.parts.selection.get()
    }

    pub fn overrides(&self) -> Overrides {
        self.parts.observer.overrides()
    }

    /// Новые переопределения ячейки. Окно получает светлоту переопределения, чтобы системные
    /// части рисовались в ней же, или снова следует системе; затем ячейка выбирается заново.
    pub fn set_overrides(&self, overrides: Overrides) {
        let forced = theme_cell::forced_appearance(overrides).and_then(theme_cell::ns_appearance);
        self.parts.window.setAppearance(forced.as_deref());
        self.parts.observer.set_overrides(overrides);
        self.parts.observer.refresh();
    }

    pub fn id(&self) -> ComponentId {
        self.parts.id
    }

    /// Окно для `ui.window`.
    pub fn facts(&self, app: &App) -> WindowFacts {
        let window = &self.parts.window;
        let mtm = app.mtm();
        let color_space = window.colorSpace();
        let srgb = color_space
            .as_ref()
            .is_some_and(|space| **space == *NSColorSpace::sRGBColorSpace());
        WindowFacts {
            number: self.number(),
            frame: inspect::frame_of(window.frame()),
            backing_scale_factor: window.backingScaleFactor(),
            visible: window.isVisible(),
            key: window.isKeyWindow(),
            color_space: color_space
                .and_then(|space| space.localizedName())
                .map(|name| name.to_string()),
            color_space_srgb: srgb,
            title: self.title(),
            screen: inspect::screen_index(window, mtm),
            screens: inspect::screens(mtm),
        }
    }

    /// Видимое дерево компонентов окна для `ui.inspect`.
    pub fn inspect(&self) -> Inspection {
        let size = self.parts.root.bounds().size;
        Inspection {
            content: (size.width, size.height),
            nodes: self.parts.chrome.inspect(),
            materials: inspect::materials(&self.parts.window, &self.parts.root),
        }
    }

    /// Старт процесса: от `launchDate` до первой отрисовки окна с поколением не меньше
    /// `generation` и до первого key. `None` — дата запуска неизвестна или окно ещё не отрисовано
    /// с этим поколением и не становилось key.
    pub fn startup(&self, monitor: &LoopMonitor, generation: u64) -> Option<Startup> {
        let launched = NSRunningApplication::currentApplication().launchDate()?;
        let launch = runloop::uptime() + launched.timeIntervalSinceNow();
        let draw = [self.parts.id, self.parts.chrome.id()]
            .into_iter()
            .filter_map(|component| monitor.first_draw(component, generation))
            .map(|mark| mark.at)
            .min_by(f64::total_cmp)?;
        let key = self.parts.first_key.get()?;
        let ms = |at: f64| (at - launch) * 1000.0;
        Some(Startup {
            first_draw_ms: ms(draw),
            key_ms: ms(key),
            key_draw_ms: ms(draw.max(key)),
        })
    }
}

/// Старт процесса в мс от `launchDate`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Startup {
    /// Первая отрисовка окна с первым снимком зеркала.
    pub first_draw_ms: f64,
    /// Окно впервые стало key.
    pub key_ms: f64,
    /// Позднее из двух: окно key показывает первый снимок.
    pub key_draw_ms: f64,
}

impl Drop for MainWindow {
    fn drop(&mut self) {
        let window = &self.parts.window;
        window.setDelegate(None);
        window.orderOut(None);
        window.setContentView(None);
    }
}

fn new_window(mtm: MainThreadMarker, theme: &AppTheme) -> Retained<NSWindow> {
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView;
    let size = NSSize::new(WINDOW_WIDTH, WINDOW_HEIGHT);
    // SAFETY: назначенный инициализатор NSWindow с обычным окном; экран выбирает AppKit.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), size),
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: окно держит Rust, `close` не должен его освобождать.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(DEFAULT_TITLE));
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    window.setTitlebarAppearsTransparent(true);
    window.setTitlebarSeparatorStyle(NSTitlebarSeparatorStyle::None);
    window.setColorSpace(Some(&NSColorSpace::sRGBColorSpace()));
    window.setOpaque(true);
    window.setBackgroundColor(Some(&layer::ns_color(theme.colors.chrome)));
    window.setContentMinSize(size);
    window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
    window.setAutorecalculatesKeyViewLoop(true);
    window.center();
    window
}

struct RootIvars {
    _live: Live,
    owner: OnceCell<Callback<Parts>>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioWindowRoot"]
    #[ivars = RootIvars]
    struct RootView;

    impl RootView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `wantsUpdateLayer` NSView.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `updateLayer` NSView.
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            let Some(theme) = self.call(|parts| {
                runloop::mark_draw(parts.id);
                parts.theme()
            }) else {
                return;
            };
            if let Some(layer) = self.layer() {
                without_actions(|| layer::set_fill(&layer, theme.colors.chrome));
            }
        }

        // SAFETY: селектор и результат — как у `viewDidChangeEffectiveAppearance` NSView.
        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn view_did_change_effective_appearance(&self) {
            self.call(|parts| parts.observer.refresh());
        }
    }
);

impl RootView {
    fn new(mtm: MainThreadMarker) -> Retained<RootView> {
        let this = RootView::alloc(mtm).set_ivars(RootIvars {
            _live: WINDOW_ROOT.live(),
            owner: OnceCell::new(),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<RootView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setWantsLayer(true);
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        view
    }

    /// Вызов владельца окна. До привязки владельца (вид ещё строится) вызов пропускается.
    fn call<R>(&self, f: impl FnOnce(&Parts) -> R) -> Option<R> {
        self.ivars().owner.get().and_then(|owner| owner.call(f))
    }
}

struct DelegateIvars {
    _live: Live,
    owner: OnceCell<Callback<Parts>>,
}

define_class!(
    // SAFETY: NSObject не требует от подкласса ничего сверх `init`, `Drop` у класса нет.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioWindowDelegate"]
    #[ivars = DelegateIvars]
    struct WindowDelegate;

    // SAFETY: протокол NSObject реализует суперкласс NSObject.
    unsafe impl NSObjectProtocol for WindowDelegate {}

    // SAFETY: протокол требует только необязательные методы, ниже они совпадают с его объявлениями.
    unsafe impl NSWindowDelegate for WindowDelegate {
        // SAFETY: селектор, аргумент и результат — как у `windowShouldClose:` протокола. Закрытие
        // прячет окно, а `NO` не даёт AppKit закрыть его.
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            self.call(|parts| parts.window.orderOut(None));
            false
        }

        // SAFETY: селектор и аргумент — как у `windowDidResize:` протокола.
        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _notification: &NSNotification) {
            self.relayout();
        }

        // SAFETY: селектор и аргумент — как у `windowDidEndLiveResize:` протокола.
        #[unsafe(method(windowDidEndLiveResize:))]
        fn window_did_end_live_resize(&self, _notification: &NSNotification) {
            self.relayout();
        }

        // SAFETY: селектор и аргумент — как у `windowDidBecomeKey:` протокола.
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            self.call(|parts| {
                if parts.first_key.get().is_none() {
                    parts.first_key.set(Some(runloop::uptime()));
                }
            });
            self.relayout();
        }

        // SAFETY: селектор и аргумент — как у `windowDidResignKey:` протокола.
        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            self.relayout();
        }

        // SAFETY: селектор и аргумент — как у `windowDidExitFullScreen:` протокола.
        #[unsafe(method(windowDidExitFullScreen:))]
        fn window_did_exit_full_screen(&self, _notification: &NSNotification) {
            self.relayout();
        }
    }
);

impl WindowDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<WindowDelegate> {
        let this = WindowDelegate::alloc(mtm).set_ivars(DelegateIvars {
            _live: WINDOW_DELEGATE.live(),
            owner: OnceCell::new(),
        });
        // SAFETY: `init` у NSObject без аргументов, ivars уже заданы.
        unsafe { msg_send![super(this), init] }
    }

    fn call<R>(&self, f: impl FnOnce(&Parts) -> R) -> Option<R> {
        self.ivars().owner.get().and_then(|owner| owner.call(f))
    }

    /// Кадр окна или его заголовка изменился: AppKit мог вернуть кнопки окна на место.
    fn relayout(&self) {
        self.call(|parts| parts.chrome.relayout());
    }
}
