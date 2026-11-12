//! Палитра: дочерняя `NSPanel` основного окна по центру сверху, один экземпляр на всё время
//! сцены. Поверхность `--surface` непрозрачна, с линией `--line-2` и радиусом `--r-float`; тень
//! `--shadow` из двух слоёв рисуют слои под поверхностью. Окно панели без рамки и без системной
//! тени, прозрачно только в поле вокруг поверхности, где лежит тень. Окно под палитрой не
//! затемняется. Поле фильтрует команды, результаты показывает `VirtualList`. Клавиши поля
//! разбирает делегат: стрелки выбирают, Enter выполняет, Escape закрывает, Tab переводит фокус
//! между полем и списком. При открытии запоминается first responder родителя, при закрытии фокус
//! возвращается ему, а если его убрали — заранее выбранному запасному виду. Палитра закрывается
//! и сама: когда теряет key-статус и когда родитель сворачивается, закрывается или перестаёт
//! быть видимым.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Instant;

use objc2::rc::{self, Retained};
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::NSScreen;
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSBackingStoreType, NSColor, NSControl,
    NSControlTextEditingDelegate, NSEvent, NSEventModifierFlags, NSFocusRingType, NSFont, NSPanel,
    NSResponder, NSTextField, NSTextFieldDelegate, NSTextView, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowDelegate, NSWindowDidChangeOcclusionStateNotification,
    NSWindowOcclusionState, NSWindowOrderingMode, NSWindowStyleMask, NSWindowWillCloseNotification,
    NSWindowWillMiniaturizeNotification,
};
use objc2_core_foundation::CGSize;
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString, ns_string,
};
use objc2_quartz_core::CALayer;

use crate::chrome::{color, fill, label, rect};
use crate::fixtures::{Command, ListRow};
use crate::list::{Hooks, ListLook, VirtualList};
use crate::menu::{Commands, Via};
use crate::scenario::counters::{Live, counter};
use crate::tokens::Tokens;

counter!(PANEL, "ProbePalettePanel");
counter!(CONTROLLER, "ProbePaletteController");

/// Ширина поверхности, как `.pal { width: 640px }`.
pub const WIDTH: f64 = 640.0;
/// Наибольшая высота поверхности и запас до низа окна: `min(560px, 100vh - 70px)`.
pub const MAX_HEIGHT: f64 = 560.0;
const BOTTOM_RESERVE: f64 = 70.0;
/// Отступ поверхности от верха окна, как `.pal { top: 46px }`.
pub const TOP: f64 = 46.0;
/// Строка поля, как `.pal-in`: высота 46, поля 14.
pub const INPUT_ROW: f64 = 46.0;
const INPUT_PAD: f64 = 14.0;
const FIELD_HEIGHT: f64 = 22.0;
/// Строка результата, как `.pi`: высота 34, поля 6, радиус 6, подпись с отступом 12.
pub const RESULT_ROW: f64 = 34.0;
pub const RESULT_MARGIN: f64 = 6.0;
const RESULT_RADIUS: f64 = 6.0;
const RESULT_TEXT_INSET: f64 = 12.0;
/// Поля списка сверху и снизу, как `.pal-list { padding: 4px 0 6px }`.
const LIST_PAD_TOP: f64 = 4.0;
const LIST_PAD_BOTTOM: f64 = 6.0;
pub const PLACEHOLDER: &str = "Команда или поиск";
pub const EMPTY: &str = "Ничего не найдено";

const KEY_TAB: u16 = 48;
const KEY_RETURN: u16 = 36;
const KEY_ENTER: u16 = 76;
const KEY_ESCAPE: u16 = 53;

/// Причина закрытия палитры.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Close {
    Escape,
    Perform,
    /// Палитра потеряла key-статус: щелчок в родителя или переход в другое приложение.
    ResignKey,
    ParentMiniaturized,
    ParentClosed,
    /// Родитель перестал быть видимым, например убран `orderOut`.
    ParentHidden,
}

/// Куда вернулся фокус родителя после закрытия.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restore {
    Previous,
    Fallback,
    Window,
    /// Фокус уже взял другой вид родителя, например щелчком, и остался у него.
    Kept,
}

/// Индексы команд, подпись которых содержит запрос без учёта регистра, в исходном порядке.
/// `titles` — подписи, уже приведённые `fold`; пустой запрос отдаёт все.
pub fn filter<'a>(titles: impl IntoIterator<Item = &'a str>, query: &str) -> Vec<usize> {
    let query = fold(query.trim());
    titles
        .into_iter()
        .enumerate()
        .filter(|(_, title)| query.is_empty() || title.contains(&query))
        .map(|(index, _)| index)
        .collect()
}

/// Подпись для сравнения без учёта регистра.
pub fn fold(text: &str) -> String {
    text.to_lowercase()
}

/// Поле вокруг поверхности, в котором помещается тень: наибольшие размытие и смещение.
pub fn shadow_margin(tokens: &Tokens) -> f64 {
    tokens
        .shadow
        .iter()
        .map(|shadow| (shadow.blur + shadow.y.abs()).ceil())
        .fold(0.0, f64::max)
}

/// Высота поверхности при высоте окна-родителя `parent`.
pub fn surface_height(parent: f64) -> f64 {
    MAX_HEIGHT
        .min(parent - BOTTOM_RESERVE)
        .max(INPUT_ROW + RESULT_ROW + LIST_PAD_TOP + LIST_PAD_BOTTOM)
}

/// Кадр поверхности на экране: по центру окна-родителя, на `TOP` ниже его верха.
pub fn surface_frame(parent: NSRect) -> NSRect {
    let height = surface_height(parent.size.height);
    NSRect::new(
        NSPoint::new(
            (parent.origin.x + (parent.size.width - WIDTH) / 2.0).floor(),
            (parent.origin.y + parent.size.height - TOP - height).floor(),
        ),
        NSSize::new(WIDTH, height),
    )
}

pub struct PanelIvars {
    _live: Live,
}

define_class!(
    /// Окно палитры без рамки: становится key, но не main, поэтому основное окно остаётся main.
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbePalettePanel"]
    #[ivars = PanelIvars]
    pub struct PalettePanel;

    impl PalettePanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }

        /// Кадр задаёт палитра от родителя. AppKit иначе сдвигает окно, поле тени которого
        /// заходит под строку меню, например у родителя в полноэкранном режиме.
        #[unsafe(method(constrainFrameRect:toScreen:))]
        fn constrain_frame_rect(&self, frame: NSRect, _screen: Option<&NSScreen>) -> NSRect {
            frame
        }
    }
);

pub struct ControllerIvars {
    _live: Live,
    owner: RefCell<Weak<Palette>>,
}

define_class!(
    /// Делегат поля и окна палитры и наблюдатель окна-родителя.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbePaletteController"]
    #[ivars = ControllerIvars]
    pub struct PaletteController;

    unsafe impl NSObjectProtocol for PaletteController {}

    unsafe impl NSControlTextEditingDelegate for PaletteController {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, _notification: &NSNotification) {
            if let Some(owner) = self.owner() {
                owner.refilter();
            }
        }

        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn do_command(&self, _control: &NSControl, _view: &NSTextView, command: Sel) -> bool {
            self.owner().is_some_and(|owner| owner.command(command))
        }
    }

    unsafe impl NSTextFieldDelegate for PaletteController {}

    unsafe impl NSWindowDelegate for PaletteController {
        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            if let Some(owner) = self.owner() {
                owner.close(Close::ResignKey);
            }
        }
    }

    impl PaletteController {
        #[unsafe(method(parentChanged:))]
        fn parent_changed(&self, notification: &NSNotification) {
            if let Some(owner) = self.owner() {
                owner.parent_changed(&notification.name());
            }
        }
    }
);

impl PaletteController {
    fn new(mtm: MainThreadMarker) -> Retained<PaletteController> {
        let this = PaletteController::alloc(mtm).set_ivars(ControllerIvars {
            _live: CONTROLLER.live(),
            owner: RefCell::new(Weak::new()),
        });
        // SAFETY: `init` у NSObject без аргументов.
        unsafe { msg_send![super(this), init] }
    }

    fn owner(&self) -> Option<Rc<Palette>> {
        self.ivars().owner.borrow().upgrade()
    }
}

/// Слои тени по токенам `--shadow`: под каждым — поверхность `--surface` с радиусом `--r-float`.
fn paint_shadows(layers: &[Retained<CALayer>], tokens: &Tokens) {
    for (layer, shadow) in layers.iter().zip(&tokens.shadow) {
        layer.setBackgroundColor(Some(&color(tokens.surface, 1.0).CGColor()));
        layer.setCornerRadius(tokens.r_float);
        layer.setShadowColor(Some(&color(shadow.color.color, 1.0).CGColor()));
        layer.setShadowOpacity(shadow.color.alpha as f32);
        // Размытие CSS — удвоенное стандартное отклонение, радиус слоя — одно.
        layer.setShadowRadius(shadow.blur / 2.0);
        // Слой не перевёрнут: отрицательное смещение уводит тень вниз.
        layer.setShadowOffset(CGSize::new(0.0, -shadow.y));
    }
}

/// Палитра сцены; её держит сцена до конца процесса.
pub struct Palette {
    parent: Retained<NSWindow>,
    panel: Retained<PalettePanel>,
    root: Retained<NSView>,
    surface: Retained<NSView>,
    casters: Vec<Retained<NSView>>,
    /// Слои тени внутри слоёв-хостов `casters`.
    shadows: Vec<Retained<CALayer>>,
    field: Retained<NSTextField>,
    line: Retained<NSView>,
    empty: Retained<NSTextField>,
    list: VirtualList,
    controller: Retained<PaletteController>,
    commands: Rc<Commands>,
    model: Vec<Command>,
    folded: Vec<String>,
    results: Cell<usize>,
    open: Cell<bool>,
    previous: RefCell<Option<rc::Weak<NSResponder>>>,
    fallback: RefCell<Option<rc::Weak<NSView>>>,
    opened: Cell<usize>,
    closes: RefCell<Vec<(Close, Restore)>>,
    /// Длительности этапов последнего открытия в миллисекундах.
    open_phases: RefCell<Vec<(&'static str, f64)>>,
    margin: f64,
}

impl Palette {
    pub fn new(
        mtm: MainThreadMarker,
        parent: &NSWindow,
        tokens: &Tokens,
        commands: Rc<Commands>,
        model: Vec<Command>,
    ) -> Rc<Palette> {
        let margin = shadow_margin(tokens);
        let surface_rect = rect(margin, margin, WIDTH, MAX_HEIGHT);
        let content = NSSize::new(WIDTH + 2.0 * margin, MAX_HEIGHT + 2.0 * margin);

        let this = PalettePanel::alloc(mtm).set_ivars(PanelIvars {
            _live: PANEL.live(),
        });
        // SAFETY: назначенный инициализатор NSWindow с окном без рамки.
        let panel: Retained<PalettePanel> = unsafe {
            msg_send![
                super(this),
                initWithContentRect: NSRect::new(NSPoint::ZERO, content),
                styleMask: NSWindowStyleMask::Borderless,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        // SAFETY: панель держит Rust, AppKit не должен освобождать её при закрытии.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setTitle(ns_string!("Палитра команд"));
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setHidesOnDeactivate(false);
        panel.setFloatingPanel(false);
        panel.setBecomesKeyOnlyIfNeeded(false);
        panel.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenAuxiliary);

        let root = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::ZERO, content));
        root.setWantsLayer(true);
        let (casters, shadows): (Vec<Retained<NSView>>, Vec<Retained<CALayer>>) = tokens
            .shadow
            .iter()
            .map(|_| {
                // Тень несёт подслой слоя-хоста: у слоя самого вида AppKit сбрасывает
                // `shadowOpacity` по `NSView.shadow`.
                let layer = CALayer::new();
                layer.setFrame(NSRect::new(NSPoint::ZERO, surface_rect.size));
                let host = CALayer::new();
                host.addSublayer(&layer);
                let caster = NSView::initWithFrame(NSView::alloc(mtm), surface_rect);
                caster.setLayer(Some(&host));
                caster.setWantsLayer(true);
                root.addSubview(&caster);
                (caster, layer)
            })
            .unzip();
        paint_shadows(&shadows, tokens);
        let surface = NSView::initWithFrame(NSView::alloc(mtm), surface_rect);
        fill(&surface, tokens.surface, 1.0);
        if let Some(layer) = surface.layer() {
            layer.setCornerRadius(tokens.r_float);
            layer.setBorderWidth(1.0);
            layer.setBorderColor(Some(&color(tokens.line_2, 1.0).CGColor()));
            layer.setMasksToBounds(true);
        }
        root.addSubview(&surface);

        let field = NSTextField::textFieldWithString(ns_string!(""), mtm);
        field.setBezeled(false);
        field.setBordered(false);
        field.setDrawsBackground(true);
        field.setBackgroundColor(Some(&color(tokens.surface, 1.0)));
        field.setTextColor(Some(&color(tokens.text, 1.0)));
        field.setFont(Some(&NSFont::systemFontOfSize(tokens.t_title)));
        field.setPlaceholderString(Some(&NSString::from_str(PLACEHOLDER)));
        field.setFocusRingType(NSFocusRingType::None);
        field.setAccessibilityLabel(Some(&NSString::from_str(PLACEHOLDER)));
        surface.addSubview(&field);
        let line = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        fill(&line, tokens.line, 1.0);
        surface.addSubview(&line);
        let look = ListLook {
            background: tokens.surface,
            row_height: RESULT_ROW,
            font_size: tokens.t_body,
            text_inset: RESULT_TEXT_INSET,
            selection_radius: RESULT_RADIUS,
        };
        let list = VirtualList::new(mtm, tokens, NSRect::ZERO, Vec::new(), look);
        surface.addSubview(list.view());
        let empty = label(mtm, EMPTY, tokens.text_3, NSRect::ZERO);
        empty.setFont(Some(&NSFont::systemFontOfSize(tokens.t_body)));
        empty.setHidden(true);
        surface.addSubview(&empty);
        panel.setContentView(Some(&root));

        let folded = model.iter().map(|command| fold(&command.title)).collect();
        let controller = PaletteController::new(mtm);
        let palette = Rc::new(Palette {
            parent: parent.retain(),
            panel,
            root,
            surface,
            casters,
            shadows,
            field,
            line,
            empty,
            list,
            controller,
            commands,
            model,
            folded,
            results: Cell::new(0),
            open: Cell::new(false),
            previous: RefCell::new(None),
            fallback: RefCell::new(None),
            opened: Cell::new(0),
            closes: RefCell::new(Vec::new()),
            open_phases: RefCell::new(Vec::new()),
            margin,
        });
        palette.connect();
        palette.place(MAX_HEIGHT);
        palette.refilter();
        palette
    }

    /// Делегаты, клавиши списка и подписка на окно-родителя.
    fn connect(self: &Rc<Self>) {
        *self.controller.ivars().owner.borrow_mut() = Rc::downgrade(self);
        let delegate = ProtocolObject::from_ref(&*self.controller);
        // SAFETY: делегаты держатся слабо; контроллер живёт столько же, сколько палитра, и
        // отвязывается в `Drop`.
        unsafe {
            self.field.setDelegate(Some(delegate));
            self.panel
                .setDelegate(Some(ProtocolObject::from_ref(&*self.controller)));
        }
        let owner = Rc::downgrade(self);
        self.list.set_hooks(Hooks {
            on_focus: Box::new(|| {}),
            on_key: Box::new(move |event| {
                owner
                    .upgrade()
                    .is_some_and(|palette| palette.list_key(event))
            }),
        });
        let center = NSNotificationCenter::defaultCenter();
        let parent: &AnyObject = &self.parent;
        // SAFETY: имена уведомлений — неизменяемые константы AppKit.
        let names = unsafe {
            [
                NSWindowWillMiniaturizeNotification,
                NSWindowWillCloseNotification,
                NSWindowDidChangeOcclusionStateNotification,
            ]
        };
        for name in names {
            // SAFETY: `parentChanged:` реализован контроллером; подписка снимается в `Drop`.
            unsafe {
                center.addObserver_selector_name_object(
                    &self.controller,
                    sel!(parentChanged:),
                    Some(name),
                    Some(parent),
                )
            };
        }
    }

    /// Кадры поверхности и её частей при высоте поверхности `height`.
    fn place(&self, height: f64) {
        let margin = self.margin;
        let content = NSSize::new(WIDTH + 2.0 * margin, height + 2.0 * margin);
        self.root.setFrameSize(content);
        let surface = rect(margin, margin, WIDTH, height);
        self.surface.setFrame(surface);
        for caster in &self.casters {
            caster.setFrame(surface);
        }
        for shadow in &self.shadows {
            shadow.setFrame(NSRect::new(NSPoint::ZERO, surface.size));
        }
        // Поверхность не перевёрнута: отступ сверху `top` даёт y = height - top - h.
        let from_top = |x: f64, top: f64, width: f64, h: f64| rect(x, height - top - h, width, h);
        self.field.setFrame(from_top(
            INPUT_PAD,
            (INPUT_ROW - FIELD_HEIGHT) / 2.0,
            WIDTH - 2.0 * INPUT_PAD,
            FIELD_HEIGHT,
        ));
        self.line.setFrame(from_top(0.0, INPUT_ROW, WIDTH, 1.0));
        let list_top = INPUT_ROW + 1.0 + LIST_PAD_TOP;
        self.list.view().setFrame(from_top(
            RESULT_MARGIN,
            list_top,
            WIDTH - 2.0 * RESULT_MARGIN,
            (height - list_top - LIST_PAD_BOTTOM).max(0.0),
        ));
        self.empty.setFrame(from_top(
            INPUT_PAD + 4.0,
            list_top + 14.0,
            WIDTH - 2.0 * INPUT_PAD,
            18.0,
        ));
    }

    /// Цвета темы: поверхность, линии, поле, список, надпись пустого результата и слои тени.
    /// Поле тени от ячейки не зависит: размытие и смещение у всех ячеек одинаковы.
    pub fn set_tokens(&self, tokens: &Tokens) {
        paint_shadows(&self.shadows, tokens);
        fill(&self.surface, tokens.surface, 1.0);
        if let Some(layer) = self.surface.layer() {
            layer.setCornerRadius(tokens.r_float);
            layer.setBorderWidth(1.0);
            layer.setBorderColor(Some(&color(tokens.line_2, 1.0).CGColor()));
        }
        self.field
            .setBackgroundColor(Some(&color(tokens.surface, 1.0)));
        self.field.setTextColor(Some(&color(tokens.text, 1.0)));
        fill(&self.line, tokens.line, 1.0);
        self.list.set_colors(tokens, tokens.surface);
        self.empty.setTextColor(Some(&color(tokens.text_3, 1.0)));
    }

    /// Запасной вид, которому уходит фокус, если прежний убран, пока палитра открыта.
    pub fn set_fallback(&self, view: &NSView) {
        *self.fallback.borrow_mut() = Some(rc::Weak::new(view));
    }

    /// Открывает палитру над родителем с пустым запросом. Уже открытая остаётся как есть.
    pub fn open(&self) {
        let parent = &self.parent;
        if self.open.get() || !parent.isVisible() || parent.isMiniaturized() {
            return;
        }
        let mut phases = Phases::new();
        *self.previous.borrow_mut() = parent
            .firstResponder()
            .map(|responder| rc::Weak::new(&*responder));
        self.field.setStringValue(ns_string!(""));
        self.refilter();
        phases.lap("refilter");
        let surface = surface_frame(parent.frame());
        self.place(surface.size.height);
        let frame = NSRect::new(
            NSPoint::new(
                surface.origin.x - self.margin,
                surface.origin.y - self.margin,
            ),
            NSSize::new(
                surface.size.width + 2.0 * self.margin,
                surface.size.height + 2.0 * self.margin,
            ),
        );
        self.panel.setFrame_display(frame, false);
        phases.lap("frame");
        // SAFETY: панель живёт дольше связи: её держит палитра, а связь снимается при закрытии.
        unsafe {
            parent.addChildWindow_ordered(&self.panel, NSWindowOrderingMode::Above);
        }
        phases.lap("child");
        self.open.set(true);
        self.opened.set(self.opened.get() + 1);
        self.panel.makeKeyAndOrderFront(None);
        phases.lap("key");
        self.panel.makeFirstResponder(Some(&self.field));
        phases.lap("responder");
        *self.open_phases.borrow_mut() = phases.done;
    }

    /// Закрывает палитру и возвращает фокус родителю. Повторное закрытие ничего не делает.
    pub fn close(&self, reason: Close) {
        if !self.open.replace(false) {
            return;
        }
        self.parent.removeChildWindow(&self.panel);
        self.panel.orderOut(None);
        let restore = self.restore_focus(reason);
        self.closes.borrow_mut().push((reason, restore));
    }

    fn restore_focus(&self, reason: Close) -> Restore {
        let parent = &self.parent;
        let in_parent = |view: &NSView| {
            view.window()
                .is_some_and(|window| same(&*window, &**parent))
        };
        let previous = self.previous.take().and_then(|weak| weak.load());
        self.key_back(reason);
        if let Some(current) = parent.firstResponder()
            && current.downcast_ref::<NSView>().is_some_and(in_parent)
            && !previous
                .as_ref()
                .is_some_and(|previous| same(&**previous, &*current))
        {
            return Restore::Kept;
        }
        let previous = previous.filter(|responder| match responder.downcast_ref::<NSView>() {
            Some(view) => in_parent(view),
            None => same(&**responder, &**parent),
        });
        let fallback = || {
            self.fallback
                .borrow()
                .as_ref()
                .and_then(|weak| weak.load())
                .filter(|view| in_parent(view))
        };
        let (target, restore): (Retained<NSResponder>, Restore) = match previous {
            Some(previous) => (previous, Restore::Previous),
            None => match fallback() {
                Some(view) => (Retained::into_super(view), Restore::Fallback),
                None => (Retained::into_super(parent.clone()), Restore::Window),
            },
        };
        parent.makeFirstResponder(Some(&target));
        restore
    }

    /// Сама палитра закрылась: key-статус возвращается родителю. При потере key-статуса и
    /// изменениях родителя key window выбрал AppKit или пользователь.
    fn key_back(&self, reason: Close) {
        let parent = &self.parent;
        let app = NSApplication::sharedApplication(parent.mtm());
        if matches!(reason, Close::Escape | Close::Perform)
            && app.isActive()
            && parent.isVisible()
            && !parent.isKeyWindow()
        {
            parent.makeKeyWindow();
        }
    }

    fn parent_changed(&self, name: &NSString) {
        if !self.open.get() {
            return;
        }
        // SAFETY: имена уведомлений — неизменяемые константы AppKit.
        let (miniaturize, close) = unsafe {
            (
                NSWindowWillMiniaturizeNotification,
                NSWindowWillCloseNotification,
            )
        };
        if name == miniaturize {
            self.close(Close::ParentMiniaturized);
        } else if name == close {
            self.close(Close::ParentClosed);
        } else if !self
            .parent
            .occlusionState()
            .contains(NSWindowOcclusionState::Visible)
        {
            self.close(Close::ParentHidden);
        }
    }

    /// Команда field editor поля: стрелки, Enter, Escape и Tab.
    fn command(&self, command: Sel) -> bool {
        if command == sel!(moveDown:) {
            self.step(1);
        } else if command == sel!(moveUp:) {
            self.step(-1);
        } else if command == sel!(insertNewline:) {
            self.perform();
        } else if command == sel!(cancelOperation:) {
            self.close(Close::Escape);
        } else if command == sel!(insertTab:) || command == sel!(insertBacktab:) {
            self.panel.makeFirstResponder(Some(self.list.table()));
        } else {
            return false;
        }
        true
    }

    /// Клавиши списка, когда фокус в нём. Up и Down обрабатывает сам список.
    fn list_key(&self, event: &NSEvent) -> bool {
        let plain = (event.modifierFlags()
            & (NSEventModifierFlags::Command
                | NSEventModifierFlags::Option
                | NSEventModifierFlags::Control))
            .is_empty();
        match event.keyCode() {
            KEY_RETURN | KEY_ENTER if plain => self.perform(),
            KEY_ESCAPE if plain => self.close(Close::Escape),
            KEY_TAB if plain => {
                self.panel.makeFirstResponder(Some(&self.field));
            }
            _ => return false,
        }
        true
    }

    fn step(&self, delta: isize) {
        let rows = self.list.len();
        if rows == 0 {
            return;
        }
        let next = match self.list.selected() {
            Some(row) => row.saturating_add_signed(delta).min(rows - 1),
            None => 0,
        };
        self.list.select(next);
    }

    /// Выполняет выбранную команду после закрытия палитры: команда видит фокус родителя.
    fn perform(&self) {
        let Some(id) = self.list.selected().and_then(|row| self.list.id(row)) else {
            return;
        };
        self.close(Close::Perform);
        self.commands.perform(id, Via::Palette);
    }

    fn refilter(&self) {
        let query = self.field.stringValue().to_string();
        let indexes = filter(self.folded.iter().map(String::as_str), &query);
        let rows = indexes
            .iter()
            .filter_map(|index| self.model.get(*index))
            .map(|command| ListRow {
                id: command.id,
                title: command.title.clone(),
            })
            .collect();
        self.list.set_rows(rows);
        if !indexes.is_empty() {
            self.list.select(0);
        }
        self.empty.setHidden(!indexes.is_empty());
        self.results.set(indexes.len());
    }

    pub fn is_open(&self) -> bool {
        self.open.get()
    }

    pub fn panel(&self) -> &PalettePanel {
        &self.panel
    }

    pub fn surface(&self) -> &NSView {
        &self.surface
    }

    /// Слои тени по порядку токенов `--shadow`.
    pub fn shadows(&self) -> &[Retained<CALayer>] {
        &self.shadows
    }

    pub fn field(&self) -> &NSTextField {
        &self.field
    }

    /// Линия между полем и списком.
    pub fn separator(&self) -> &NSView {
        &self.line
    }

    pub fn list(&self) -> &VirtualList {
        &self.list
    }

    pub fn empty_label(&self) -> &NSTextField {
        &self.empty
    }

    pub fn margin(&self) -> f64 {
        self.margin
    }

    /// Число результатов последнего фильтра.
    pub fn results(&self) -> usize {
        self.results.get()
    }

    pub fn model(&self) -> &[Command] {
        &self.model
    }

    /// Сколько раз палитра открывалась.
    pub fn opened(&self) -> usize {
        self.opened.get()
    }

    /// Причины закрытий и куда вернулся фокус.
    pub fn closes(&self) -> Vec<(Close, Restore)> {
        self.closes.borrow().clone()
    }

    /// Этапы последнего открытия и их длительности в миллисекундах.
    pub fn open_phases(&self) -> Vec<(&'static str, f64)> {
        self.open_phases.borrow().clone()
    }
}

/// Отметки этапов по монотонным часам.
struct Phases {
    mark: Instant,
    done: Vec<(&'static str, f64)>,
}

impl Phases {
    fn new() -> Phases {
        Phases {
            mark: Instant::now(),
            done: Vec::new(),
        }
    }

    fn lap(&mut self, name: &'static str) {
        let now = Instant::now();
        self.done
            .push((name, now.duration_since(self.mark).as_secs_f64() * 1e3));
        self.mark = now;
    }
}

impl Drop for Palette {
    fn drop(&mut self) {
        // SAFETY: наблюдатель — контроллер палитры, снимаются все его подписки.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&self.controller) };
        // SAFETY: отвязка делегатов до освобождения контроллера.
        unsafe {
            self.field.setDelegate(None);
            self.panel.setDelegate(None);
        }
        if self.open.get() {
            self.parent.removeChildWindow(&self.panel);
            self.panel.orderOut(None);
        }
    }
}

fn same<T: ?Sized, U: ?Sized>(object: &T, expected: &U) -> bool {
    std::ptr::addr_eq(object, expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, MARK_TITLE, PALETTE_COMMANDS};
    use crate::tokens::{self as tokens_mod, Cell as ThemeCell, InputMode, Theme};

    fn folded() -> Vec<String> {
        fixtures::commands(PALETTE_COMMANDS)
            .iter()
            .map(|command| fold(&command.title))
            .collect()
    }

    fn run(query: &str) -> Vec<usize> {
        let titles = folded();
        filter(titles.iter().map(String::as_str), query)
    }

    #[test]
    fn empty_query_keeps_all_in_order() {
        let all = run("");
        assert_eq!(all, (0..PALETTE_COMMANDS).collect::<Vec<_>>());
        assert_eq!(run("   "), all);
    }

    #[test]
    fn query_without_matches_is_empty() {
        assert!(run("zzz").is_empty());
        assert!(run("отметить build").is_empty());
    }

    #[test]
    fn query_matches_substring_without_case() {
        let commands = fixtures::commands(PALETTE_COMMANDS);
        let one = run("l");
        assert!(!one.is_empty() && one.len() < PALETTE_COMMANDS);
        assert!(one.iter().all(|index| commands[*index].title.contains('l')));
        let expected = commands
            .iter()
            .filter(|command| command.title.contains('l'))
            .count();
        assert_eq!(one.len(), expected);
        assert_eq!(run("BUILD"), run("build"));
        assert_eq!(run("ОТМЕТ"), vec![0]);
        assert_eq!(run(&MARK_TITLE.to_lowercase()), vec![0]);
        assert_eq!(run(" найти commit 1999 "), vec![1_999]);
    }

    #[test]
    fn geometry_follows_prototype() {
        let dark = tokens_mod::tokens(
            ThemeCell::new(Theme::Dark, crate::args::Contrast::Normal),
            InputMode::Navigation,
        );
        assert_eq!(shadow_margin(&dark), 66.0);
        assert_eq!(surface_height(720.0), MAX_HEIGHT);
        assert_eq!(surface_height(600.0), 530.0);
        assert_eq!(surface_height(100.0), INPUT_ROW + RESULT_ROW + 10.0);
        let parent = rect(100.0, 50.0, 1180.0, 720.0);
        let surface = surface_frame(parent);
        assert_eq!(
            surface,
            rect(370.0, 50.0 + 720.0 - TOP - MAX_HEIGHT, WIDTH, MAX_HEIGHT)
        );
    }
}
