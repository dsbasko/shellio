//! Окно сцены и `ChromeBar`: заголовок 40 точек через всё окно. Слева кнопки окна и кнопка
//! воркспейса, по центру поле поиска (оба — неактивные образцы), справа управление зонами и
//! переключатель темы; пустая часть полосы перетаскивает окно. Окно непрозрачно, его фон без
//! альфы.

use std::cell::{Cell, OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    NSAutoresizingMaskOptions, NSBackingStoreType, NSButton, NSColor, NSControl, NSEvent,
    NSResponder, NSSplitViewItem, NSTextField, NSTitlebarSeparatorStyle, NSView, NSWindow,
    NSWindowButton, NSWindowCollectionBehavior, NSWindowDidBecomeKeyNotification,
    NSWindowDidEndLiveResizeNotification, NSWindowDidExitFullScreenNotification,
    NSWindowDidResignKeyNotification, NSWindowDidResizeNotification, NSWindowStyleMask,
    NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSArray, NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSPoint, NSRect,
    NSSize, NSString, NSUserDefaults, ns_string,
};

use crate::scenario::counters::{Live, counter};
use crate::theme::Choice;
use crate::tokens::{Rgba, Tokens};
use crate::{DisplayChoice, WINDOW_SIZE, window_frame};

counter!(SCENE_ROOT, "ProbeSceneRoot");
counter!(CHROME_BAR, "ProbeChromeBar");
counter!(SEARCH_FIELD, "ProbeSearchField");
counter!(CHROME_TARGET, "ProbeChromeTarget");

/// Центр кнопок окна от верхнего края, в точках: середина полосы заголовка.
pub const BUTTON_CENTER_FROM_TOP: f64 = 20.0;
/// Отступы полосы заголовка слева и справа, как в прототипе.
const PAD_LEFT: f64 = 14.0;
const PAD_RIGHT: f64 = 10.0;
const GRID_GAP: f64 = 16.0;
/// Ширина поиска: `minmax(300px, 460px)` средней колонки прототипа.
const SEARCH_MIN: f64 = 300.0;
const SEARCH_MAX: f64 = 460.0;
const SEARCH_HEIGHT: f64 = 26.0;
/// Кнопка воркспейса начинается за кнопками окна: 14 + 3·12 + 2·8 + 12.
const WORKSPACE_X: f64 = 78.0;
const WORKSPACE_SIZE: NSSize = NSSize::new(120.0, 24.0);
const ICON_BUTTON: NSSize = NSSize::new(28.0, 24.0);
const ICON_GAP: f64 = 8.0;

pub fn color(rgba: Rgba, alpha: f64) -> Retained<NSColor> {
    let (r, g, b) = rgba.unit();
    NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, alpha)
}

/// Делает вид слоевым и заливает слой.
pub fn fill(view: &NSView, rgba: Rgba, alpha: f64) {
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        layer.setBackgroundColor(Some(&color(rgba, alpha).CGColor()));
    }
}

pub fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

pub fn label(mtm: MainThreadMarker, text: &str, ink: Rgba, frame: NSRect) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setTextColor(Some(&color(ink, 1.0)));
    label.setFrame(frame);
    label
}

pub fn window_style() -> NSWindowStyleMask {
    NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView
}

/// Окно сцены: содержимое под заголовком, заголовок прозрачен и без подписи, фон окна
/// непрозрачен. Той же функцией строится пустое окно для сравнения материала.
pub fn window(
    mtm: MainThreadMarker,
    display: &DisplayChoice,
    tokens: &Tokens,
) -> Retained<NSWindow> {
    // SAFETY: обычная инициализация окна с существующим экраном.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer_screen(
            NSWindow::alloc(mtm),
            window_frame(display, WINDOW_SIZE),
            window_style(),
            NSBackingStoreType::Buffered,
            false,
            Some(&display.screen),
        )
    };
    // SAFETY: окно держит Rust, поэтому AppKit не должен освобождать его при закрытии.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(ns_string!("probe-appkit-shell"));
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    window.setTitlebarAppearsTransparent(true);
    window.setTitlebarSeparatorStyle(NSTitlebarSeparatorStyle::None);
    paint_window(&window, tokens);
    window.setOpaque(true);
    window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
    window.setContentMinSize(WINDOW_SIZE);
    window
}

/// Непрозрачный фон окна цветом `--chrome`.
pub fn paint_window(window: &NSWindow, tokens: &Tokens) {
    window.setBackgroundColor(Some(&color(tokens.chrome, 1.0)));
}

type AppearanceHook = Box<dyn Fn()>;

pub struct RootIvars {
    _live: Live,
    on_appearance: RefCell<Option<AppearanceHook>>,
    appearance_changes: Cell<usize>,
}

define_class!(
    /// Содержимое окна: непрозрачный фон `chrome`, координаты сверху вниз. Смену своего
    /// оформления сообщает теме сцены.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeSceneRoot"]
    #[ivars = RootIvars]
    pub struct SceneRoot;

    impl SceneRoot {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn view_did_change_effective_appearance(&self) {
            // SAFETY: `viewDidChangeEffectiveAppearance` у NSView без аргументов.
            let _: () = unsafe { msg_send![super(self), viewDidChangeEffectiveAppearance] };
            let ivars = self.ivars();
            ivars.appearance_changes.set(ivars.appearance_changes.get() + 1);
            if let Some(hook) = ivars.on_appearance.borrow().as_ref() {
                hook();
            }
        }
    }
);

impl SceneRoot {
    pub fn new(mtm: MainThreadMarker, frame: NSRect, tokens: &Tokens) -> Retained<SceneRoot> {
        let this = SceneRoot::alloc(mtm).set_ivars(RootIvars {
            _live: SCENE_ROOT.live(),
            on_appearance: RefCell::new(None),
            appearance_changes: Cell::new(0),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let view: Retained<SceneRoot> = unsafe { msg_send![super(this), initWithFrame: frame] };
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        fill(&view, tokens.chrome, 1.0);
        view
    }

    /// Вызов при смене оформления корня.
    pub fn set_on_appearance(&self, hook: AppearanceHook) {
        *self.ivars().on_appearance.borrow_mut() = Some(hook);
    }

    /// Сколько раз AppKit сообщил корню о смене оформления.
    pub fn appearance_changes(&self) -> usize {
        self.ivars().appearance_changes.get()
    }

    pub fn set_tokens(&self, tokens: &Tokens) {
        fill(self, tokens.chrome, 1.0);
    }
}

pub struct TargetIvars {
    _live: Live,
    left: Retained<NSSplitViewItem>,
    right: Retained<NSSplitViewItem>,
}

define_class!(
    /// Цель кнопок заголовка: свёртка зон и тема.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeChromeTarget"]
    #[ivars = TargetIvars]
    pub struct ChromeTarget;

    unsafe impl NSObjectProtocol for ChromeTarget {}

    impl ChromeTarget {
        #[unsafe(method(toggleLeftZone:))]
        fn toggle_left_zone(&self, _sender: Option<&AnyObject>) {
            let left = &self.ivars().left;
            left.setCollapsed(!left.isCollapsed());
        }

        #[unsafe(method(toggleRightZone:))]
        fn toggle_right_zone(&self, _sender: Option<&AnyObject>) {
            let right = &self.ivars().right;
            right.setCollapsed(!right.isCollapsed());
        }

        /// Меняет `NSApp.appearance` на противоположную тему; ячейку сцены пересчитывает смена
        /// оформления корневого вида.
        #[unsafe(method(toggleTheme:))]
        fn toggle_theme(&self, _sender: Option<&AnyObject>) {
            let app = NSApplication::sharedApplication(self.mtm());
            let next = if is_dark(&app.effectiveAppearance()) {
                Choice::Light
            } else {
                Choice::Dark
            };
            next.apply(&app);
        }
    }
);

impl ChromeTarget {
    pub fn new(
        mtm: MainThreadMarker,
        left: Retained<NSSplitViewItem>,
        right: Retained<NSSplitViewItem>,
    ) -> Retained<ChromeTarget> {
        let this = ChromeTarget::alloc(mtm).set_ivars(TargetIvars {
            _live: CHROME_TARGET.live(),
            left,
            right,
        });
        // SAFETY: `init` у NSObject без аргументов.
        unsafe { msg_send![super(this), init] }
    }
}

/// Тёмное ли оформление из двух системных.
pub fn is_dark(appearance: &NSAppearance) -> bool {
    // SAFETY: имена оформления — неизменяемые константы AppKit.
    let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    let names = NSArray::from_slice(&[aqua, dark]);
    appearance
        .bestMatchFromAppearancesWithNames(&names)
        .is_some_and(|name| &*name == dark)
}

pub struct SearchIvars {
    _live: Live,
    presses: Cell<usize>,
    hint: Retained<NSTextField>,
}

define_class!(
    /// Поле поиска в заголовке — неактивный образец. Щелчок по нему не перетаскивает окно.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeSearchField"]
    #[ivars = SearchIvars]
    pub struct SearchField;

    impl SearchField {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// Подпись внутри поля щелчок не получает: адресат — всё поле.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            let frame = self.frame();
            let inside = point.x >= frame.origin.x
                && point.x < frame.origin.x + frame.size.width
                && point.y >= frame.origin.y
                && point.y < frame.origin.y + frame.size.height;
            (inside && !self.isHidden()).then(|| Retained::into_super(self.retain()))
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            let presses = &self.ivars().presses;
            presses.set(presses.get() + 1);
        }
    }
);

impl SearchField {
    fn new(mtm: MainThreadMarker, tokens: &Tokens) -> Retained<SearchField> {
        let hint = label(
            mtm,
            "Поиск или команда",
            tokens.text_3,
            rect(9.0, 4.0, SEARCH_MAX - 18.0, 18.0),
        );
        hint.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let this = SearchField::alloc(mtm).set_ivars(SearchIvars {
            _live: SEARCH_FIELD.live(),
            presses: Cell::new(0),
            hint,
        });
        let frame = rect(0.0, 0.0, SEARCH_MAX, SEARCH_HEIGHT);
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let view: Retained<SearchField> = unsafe { msg_send![super(this), initWithFrame: frame] };
        view.set_tokens(tokens);
        view.setToolTip(Some(ns_string!("Поиск — неактивный образец пробы")));
        view.addSubview(&view.ivars().hint);
        view
    }

    /// Фон `--surface`, линия `--line`, радиус `--r-ctl`, подсказка `--text-3`.
    pub fn set_tokens(&self, tokens: &Tokens) {
        fill(self, tokens.surface, 1.0);
        if let Some(layer) = self.layer() {
            layer.setBorderWidth(1.0);
            layer.setBorderColor(Some(&color(tokens.line, 1.0).CGColor()));
            layer.setCornerRadius(tokens.r_ctl);
        }
        self.ivars()
            .hint
            .setTextColor(Some(&color(tokens.text_3, 1.0)));
    }

    pub fn hint(&self) -> &NSTextField {
        &self.ivars().hint
    }

    /// Число нажатий кнопки мыши, дошедших до поля.
    pub fn presses(&self) -> usize {
        self.ivars().presses.get()
    }
}

/// Части заголовка, которые раскладывает `ChromeBar`.
pub struct ChromeParts {
    pub workspace: Retained<NSButton>,
    pub search: Retained<SearchField>,
    pub left_zone: Retained<NSButton>,
    pub right_zone: Retained<NSButton>,
    pub theme: Retained<NSButton>,
    pub line: Retained<NSView>,
}

pub struct ChromeIvars {
    _live: Live,
    parts: OnceCell<ChromeParts>,
    /// Сколько раз кнопки окна пришлось вернуть в середину полосы.
    button_moves: Cell<usize>,
}

define_class!(
    /// Заголовок окна 40 точек. Пустая часть перетаскивает окно, двойной щелчок действует по
    /// системной настройке. Кнопки окна ставятся по вертикали в середину полосы.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeChromeBar"]
    #[ivars = ChromeIvars]
    pub struct ChromeBar;

    impl ChromeBar {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let Some(window) = self.window() else {
                return;
            };
            if event.clickCount() == 2 {
                match double_click_action() {
                    DoubleClick::Minimize => window.miniaturize(None),
                    DoubleClick::Zoom => window.zoom(None),
                    DoubleClick::Nothing => {}
                }
            } else {
                window.performWindowDragWithEvent(event);
            }
        }

        #[unsafe(method(resizeSubviewsWithOldSize:))]
        fn resize_subviews(&self, _old: NSSize) {
            self.place_parts();
            self.setNeedsLayout(true);
        }

        #[unsafe(method(layout))]
        fn layout(&self) {
            // SAFETY: `layout` у NSView без аргументов.
            let _: () = unsafe { msg_send![super(self), layout] };
            self.place_window_buttons();
        }

        #[unsafe(method(viewWillMoveToWindow:))]
        fn view_will_move_to_window(&self, _window: Option<&NSWindow>) {
            // SAFETY: наблюдатель — этот вид, снимаются все его подписки.
            unsafe { NSNotificationCenter::defaultCenter().removeObserver(self) };
        }

        #[unsafe(method(viewDidMoveToWindow))]
        fn view_did_move_to_window(&self) {
            let Some(window) = self.window() else {
                return;
            };
            let center = NSNotificationCenter::defaultCenter();
            // SAFETY: имена уведомлений — неизменяемые константы AppKit.
            let names = unsafe {
                [
                    NSWindowDidResizeNotification,
                    NSWindowDidEndLiveResizeNotification,
                    NSWindowDidExitFullScreenNotification,
                    NSWindowDidBecomeKeyNotification,
                    NSWindowDidResignKeyNotification,
                ]
            };
            let window: &AnyObject = &window;
            for name in names {
                // SAFETY: `windowChanged:` реализован этим классом и принимает уведомление;
                // подписка снимается в `viewWillMoveToWindow:` и при освобождении вида.
                unsafe {
                    center.addObserver_selector_name_object(
                        self,
                        sel!(windowChanged:),
                        Some(name),
                        Some(window),
                    )
                };
            }
            self.place_window_buttons();
        }

        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn view_did_change_effective_appearance(&self) {
            self.setNeedsLayout(true);
            self.place_window_buttons();
        }

        #[unsafe(method(windowChanged:))]
        fn window_changed(&self, _notification: &NSNotification) {
            self.setNeedsLayout(true);
            self.place_window_buttons();
        }
    }
);

enum DoubleClick {
    Minimize,
    Zoom,
    Nothing,
}

/// Системная настройка «Двойной щелчок по заголовку окна».
fn double_click_action() -> DoubleClick {
    let value = NSUserDefaults::standardUserDefaults()
        .stringForKey(ns_string!("AppleActionOnDoubleClick"))
        .map(|value| value.to_string());
    match value.as_deref() {
        Some("Minimize") => DoubleClick::Minimize,
        Some("None") => DoubleClick::Nothing,
        _ => DoubleClick::Zoom,
    }
}

fn button(
    mtm: MainThreadMarker,
    title: &str,
    tip: &str,
    target: Option<&AnyObject>,
    action: Option<Sel>,
) -> Retained<NSButton> {
    // SAFETY: действие либо пусто, либо реализовано `ChromeTarget`, который живёт дольше кнопок:
    // его держит сцена.
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(&NSString::from_str(title), target, action, mtm)
    };
    button.setBordered(false);
    button.setToolTip(Some(&NSString::from_str(tip)));
    button
}

impl ChromeBar {
    pub fn new(
        mtm: MainThreadMarker,
        width: f64,
        tokens: &Tokens,
        target: &ChromeTarget,
    ) -> Retained<ChromeBar> {
        let this = ChromeBar::alloc(mtm).set_ivars(ChromeIvars {
            _live: CHROME_BAR.live(),
            parts: OnceCell::new(),
            button_moves: Cell::new(0),
        });
        let frame = rect(0.0, 0.0, width, tokens.titlebar);
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let bar: Retained<ChromeBar> = unsafe { msg_send![super(this), initWithFrame: frame] };
        bar.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        let target: &AnyObject = target;
        let workspace = button(
            mtm,
            "shellio ▾",
            "Кнопка воркспейса — неактивный образец пробы",
            None,
            None,
        );
        let control: &NSControl = &workspace;
        control.setEnabled(false);
        let parts = ChromeParts {
            workspace,
            search: SearchField::new(mtm, tokens),
            left_zone: button(
                mtm,
                "◧",
                "Показать или скрыть левую зону",
                Some(target),
                Some(sel!(toggleLeftZone:)),
            ),
            right_zone: button(
                mtm,
                "◨",
                "Показать или скрыть правую зону",
                Some(target),
                Some(sel!(toggleRightZone:)),
            ),
            theme: button(
                mtm,
                "◐",
                "Сменить тему",
                Some(target),
                Some(sel!(toggleTheme:)),
            ),
            line: NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, width, 1.0)),
        };
        for view in [
            &*parts.line,
            &parts.workspace,
            &parts.search,
            &parts.left_zone,
            &parts.right_zone,
            &parts.theme,
        ] {
            bar.addSubview(view);
        }
        let _ = bar.ivars().parts.set(parts);
        bar.set_tokens(tokens);
        bar.place_parts();
        bar
    }

    /// Фон `--chrome`, нижняя линия `--line` и поле поиска.
    pub fn set_tokens(&self, tokens: &Tokens) {
        fill(self, tokens.chrome, 1.0);
        if let Some(parts) = self.parts() {
            fill(&parts.line, tokens.line, 1.0);
            parts.search.set_tokens(tokens);
        }
    }

    pub fn parts(&self) -> Option<&ChromeParts> {
        self.ivars().parts.get()
    }

    pub fn button_moves(&self) -> usize {
        self.ivars().button_moves.get()
    }

    /// Раскладка как в сетке прототипа: `1fr minmax(300px, 460px) 1fr` с зазором 16 и
    /// отступами 14 и 10.
    fn place_parts(&self) {
        let Some(parts) = self.parts() else {
            return;
        };
        let bounds = self.bounds();
        let (width, height) = (bounds.size.width, bounds.size.height);
        parts.line.setFrame(rect(0.0, height - 1.0, width, 1.0));
        parts.workspace.setFrame(NSRect::new(
            NSPoint::new(
                WORKSPACE_X,
                ((height - WORKSPACE_SIZE.height) / 2.0).floor(),
            ),
            WORKSPACE_SIZE,
        ));
        let search = search_frame(width, height);
        parts.search.setFrame(search);
        let y = ((height - ICON_BUTTON.height) / 2.0).floor();
        let mut x = width - PAD_RIGHT - ICON_BUTTON.width;
        for button in [&parts.theme, &parts.right_zone, &parts.left_zone] {
            button.setFrame(NSRect::new(NSPoint::new(x, y), ICON_BUTTON));
            x -= ICON_GAP + ICON_BUTTON.width;
        }
    }

    /// Ставит центры стандартных кнопок окна на середину полосы. Горизонтальное положение
    /// остаётся системным. В полноэкранном режиме кнопки живут в другом окне и не трогаются.
    fn place_window_buttons(&self) {
        let Some(window) = self.window() else {
            return;
        };
        let Some(frame_view) = window.contentView().and_then(|content| {
            // SAFETY: `superview` у вида, который держит окно.
            unsafe { content.superview() }
        }) else {
            return;
        };
        let top = frame_view.bounds().size.height;
        let target = top - BUTTON_CENTER_FROM_TOP;
        let mut moved = false;
        for kind in [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ] {
            let Some(button) = window.standardWindowButton(kind) else {
                continue;
            };
            if !button
                .window()
                .is_some_and(|own| std::ptr::eq(&*own, &*window))
            {
                continue;
            }
            // SAFETY: `superview` у кнопки, которую держит окно.
            let Some(parent) = (unsafe { button.superview() }) else {
                continue;
            };
            let in_window = button.convertRect_toView(button.bounds(), None);
            let delta = target - (in_window.origin.y + in_window.size.height / 2.0);
            if delta.abs() < 0.25 {
                continue;
            }
            let mut origin = button.frame().origin;
            origin.y += if parent.isFlipped() { -delta } else { delta };
            button.setFrameOrigin(origin);
            moved = true;
        }
        if moved {
            let moves = &self.ivars().button_moves;
            moves.set(moves.get() + 1);
        }
    }
}

/// Кадр поля поиска в полосе ширины `width`, координаты сверху вниз.
pub fn search_frame(width: f64, height: f64) -> NSRect {
    let available = width - PAD_LEFT - PAD_RIGHT - 2.0 * GRID_GAP;
    let middle = available
        .clamp(SEARCH_MIN, SEARCH_MAX)
        .min(available.max(0.0));
    let side = ((available - middle) / 2.0).max(0.0);
    NSRect::new(
        NSPoint::new(
            (PAD_LEFT + side + GRID_GAP).floor(),
            ((height - SEARCH_HEIGHT) / 2.0).floor(),
        ),
        NSSize::new(middle, SEARCH_HEIGHT),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_is_centered_and_clamped() {
        let at_min = search_frame(1180.0, 40.0);
        assert_eq!(at_min.size.width, SEARCH_MAX);
        assert_eq!(at_min.origin.y, 7.0);
        // Колонки по бокам равны: 14 + 332 + 16 = 362.
        assert_eq!(at_min.origin.x, 362.0);
        let wide = search_frame(1512.0, 40.0);
        assert_eq!(wide.size.width, SEARCH_MAX);
        assert_eq!(
            wide.origin.x,
            (14.0_f64 + (1512.0 - 56.0 - 460.0) / 2.0 + 16.0).floor()
        );
    }
}
