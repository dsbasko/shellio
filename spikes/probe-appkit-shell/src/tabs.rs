//! `TabStrip`: полоса вкладок `--tabbar` внутри плитки по прототипу и ADR-0032. Слева вкладки в
//! горизонтальной прокрутке, справа нагрузка активной вкладки, «Разделить» и «Развернуть панель»
//! как отключённые образцы и меню «…». При нехватке места первой скрывается нагрузка и остаётся в
//! заголовке меню, потом вкладки прокручиваются; кнопки не скрываются. Клавиши `H`, `L` и
//! `1`…`9` передаёт плитка с фокусом, и действуют они только на видимую полосу.

use std::cell::{Cell, RefCell};

use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, Sel};
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class,
    msg_send, sel,
};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityButton, NSAccessibilityElementProtocol,
    NSAccessibilityPressAction, NSAccessibilityRadioButton, NSAccessibilityRadioButtonRole,
    NSAccessibilityTabButtonSubrole, NSAccessibilityTabGroupRole, NSButton, NSControl, NSEvent,
    NSEventModifierFlags, NSFont, NSImage, NSImageScaling, NSImageView, NSMenu, NSMenuItem,
    NSResponder, NSScrollElasticity, NSScrollView, NSTextField, NSTrackingArea,
    NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{
    NSArray, NSNumber, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, ns_string,
};
use serde::Serialize;

use crate::chrome::{color, fill, label, rect};
use crate::fixtures::{Meter, TabKind, TabSpec, TabStatus};
use crate::scenario::counters::{self, Live, counter};
use crate::tokens::{Rgba, Tokens};

counter!(TAB_STRIP, "ProbeTabStrip");
counter!(TAB_ROW, "ProbeTabRow");
counter!(TAB_SCROLL, "ProbeTabScroll");
counter!(TAB, "ProbeTab");

/// Отступы полосы слева и справа и зазор между её частями, как у `.tabbar` прототипа.
pub const PAD_LEFT: f64 = 4.0;
pub const PAD_RIGHT: f64 = 6.0;
pub const GAP: f64 = 2.0;
pub const TAB_HEIGHT: f64 = 26.0;
/// Верх вкладки в полосе: вкладка по середине полосы.
pub const TAB_TOP: f64 = 4.0;
/// Кнопка `.ib.sm`.
pub const BUTTON: f64 = 22.0;
/// Отступ нагрузки с каждой стороны.
pub const METER_PAD: f64 = 6.0;
/// Линия под активной вкладкой короче вкладки на столько точек с каждой стороны.
pub const LINE_INSET: f64 = 8.0;
/// Запас ряда вкладок по краям, чтобы рамка крайней вкладки не обрезалась.
pub const ROW_INSET: f64 = 1.0;
const TAB_PAD_LEFT: f64 = 10.0;
const TAB_PAD_RIGHT: f64 = 6.0;
const TAB_GAP: f64 = 7.0;
const ICON: f64 = 13.0;
const DOT: f64 = 7.0;
const CLOSE: f64 = 16.0;
/// Шаг прокрутки ряда колесом мыши без точных приращений.
const WHEEL_LINE: f64 = 10.0;

/// Плитка, в которой стоит полоса: от неё зависят значок типа и ось деления.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Place {
    Center,
    Side,
    Bottom,
}

impl Place {
    /// Подпись и клавиша «Разделить» по оси зоны: рядом в центре и внизу, одна над другой сбоку.
    pub fn split_action(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Place::Center | Place::Bottom => {
                ("Разделить: части рядом", "Ctrl-w v", "rectangle.split.2x1")
            }
            Place::Side => (
                "Разделить: одна над другой",
                "Ctrl-w s",
                "rectangle.split.1x2",
            ),
        }
    }

    pub fn has_icons(self) -> bool {
        match self {
            Place::Center | Place::Bottom => true,
            Place::Side => false,
        }
    }
}

pub const ZOOM_ACTION: (&str, &str) = ("Развернуть панель", "Ctrl-w z");
pub const MENU_LABEL: &str = "Действия панели";

/// Режим полосы по ADR-0032.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Regime {
    /// Вкладки и нагрузка помещаются целиком.
    Fit,
    /// Нагрузка скрыта в заголовок меню «…», вкладки помещаются.
    MeterInMenu,
    /// Нагрузка скрыта, вкладки прокручиваются.
    Scroll,
}

/// Раскладка полосы. Координаты по горизонтали в точках полосы, у вкладок — в точках ряда.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StripLayout {
    pub regime: Regime,
    /// Начало и ширина видимой части ряда.
    pub viewport: (f64, f64),
    /// Ширина ряда вкладок с запасом по краям.
    pub content: f64,
    pub tabs: Vec<f64>,
    /// Начало видимой нагрузки вместе с отступами.
    pub meter: Option<f64>,
    /// Начала кнопок «Разделить», «Развернуть панель» и «…».
    pub buttons: [f64; 3],
    /// Наименьшая ширина полосы, при которой видны вкладки и нагрузка, и без нагрузки.
    pub fit_width: f64,
    pub no_meter_width: f64,
}

/// Раскладка полосы ширины `width` с вкладками ширин `tabs` и нагрузкой ширины `meter` вместе с
/// отступами. Кнопки стоят у правого края всегда; нагрузка видна, только если всё помещается.
pub fn layout(width: f64, tabs: &[f64], meter: Option<f64>) -> StripLayout {
    let more = width - PAD_RIGHT - BUTTON;
    let zoom = more - GAP - BUTTON;
    let split = zoom - GAP - BUTTON;
    let mut x = ROW_INSET;
    let mut starts = Vec::with_capacity(tabs.len());
    for (index, tab) in tabs.iter().enumerate() {
        if index > 0 {
            x += GAP;
        }
        starts.push(x);
        x += tab;
    }
    let needed = x - ROW_INSET;
    let content = x + ROW_INSET;
    let cluster = 3.0 * BUTTON + 2.0 * GAP + PAD_RIGHT;
    let no_meter_width = PAD_LEFT + needed + GAP + cluster;
    let fit_width = meter.map_or(no_meter_width, |meter| no_meter_width + meter + GAP);
    const EPSILON: f64 = 1e-6;
    let regime = if width + EPSILON >= fit_width {
        Regime::Fit
    } else if meter.is_some() && width + EPSILON >= no_meter_width {
        Regime::MeterInMenu
    } else {
        Regime::Scroll
    };
    let meter_x = meter
        .filter(|_| regime == Regime::Fit)
        .map(|meter| split - GAP - meter);
    let available = (meter_x.unwrap_or(split) - GAP - PAD_LEFT).max(0.0);
    StripLayout {
        regime,
        viewport: (
            PAD_LEFT - ROW_INSET,
            needed.min(available) + 2.0 * ROW_INSET,
        ),
        content,
        tabs: starts,
        meter: meter_x,
        buttons: [split, zoom, more],
        fit_width,
        no_meter_width,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Prev,
    Next,
    Number(usize),
}

/// Клавиши вкладок по физической клавише, как в keymap прототипа: `H` и `L` с Shift, `1`…`9`
/// без модификаторов. Раскладка их не меняет.
pub fn key_action(code: u16, shift: bool, other_modifiers: bool) -> Option<KeyAction> {
    const DIGITS: [u16; 9] = [18, 19, 20, 21, 23, 22, 26, 28, 25];
    if other_modifiers {
        return None;
    }
    match (code, shift) {
        (4, true) => Some(KeyAction::Prev),
        (37, true) => Some(KeyAction::Next),
        (code, false) => DIGITS
            .iter()
            .position(|digit| *digit == code)
            .map(KeyAction::Number),
        _ => None,
    }
}

/// Вкладка после действия; соседние переходы идут по кругу, номер за краем набора ничего не
/// выбирает.
pub fn next_index(active: Option<usize>, len: usize, action: KeyAction) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let active = active.unwrap_or(0).min(len - 1);
    match action {
        KeyAction::Prev => Some((active + len - 1) % len),
        KeyAction::Next => Some((active + 1) % len),
        KeyAction::Number(number) => (number < len).then_some(number),
    }
}

/// Активная вкладка после закрытия вкладки `closed` из `len`.
pub fn after_close(active: Option<usize>, closed: usize, len: usize) -> Option<usize> {
    let left = len.checked_sub(1)?;
    if left == 0 {
        return None;
    }
    let active = active.unwrap_or(0);
    Some(if closed < active {
        active - 1
    } else {
        active.min(left - 1)
    })
}

/// Ширина нагрузки вместе с отступами: подпись целиком, без обрезки.
pub fn meter_width(meter: &NSTextField) -> f64 {
    meter.intrinsicContentSize().width.ceil() + 2.0 * METER_PAD
}

/// Символ SF Symbols; `description` — подпись картинки для доступности.
fn symbol(name: &str, description: Option<&str>) -> Option<Retained<NSImage>> {
    let description = description.map(NSString::from_str);
    NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        description.as_deref(),
    )
}

fn kind_symbol(kind: TabKind) -> &'static str {
    match kind {
        TabKind::Terminal => "terminal",
        TabKind::Agent => "sparkles",
        TabKind::Editor => "chevron.left.forwardslash.chevron.right",
        TabKind::Changes => "plusminus",
        TabKind::History => "clock.arrow.circlepath",
    }
}

fn set_layer(view: &NSView, background: Option<Rgba>, border: Option<(f64, Rgba)>, radius: f64) {
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        let background = background.map(|rgba| color(rgba, 1.0).CGColor());
        layer.setBackgroundColor(background.as_deref());
        match border {
            Some((width, rgba)) => {
                layer.setBorderWidth(width);
                layer.setBorderColor(Some(&color(rgba, 1.0).CGColor()));
            }
            None => {
                layer.setBorderWidth(0.0);
                layer.setBorderColor(None);
            }
        }
        layer.setCornerRadius(radius);
    }
}

fn icon_button(
    mtm: MainThreadMarker,
    name: &str,
    action: Option<Sel>,
    label: &str,
    tip: &str,
    help: Option<&str>,
) -> Retained<NSButton> {
    let image = symbol(name, Some(label)).unwrap_or_default();
    // SAFETY: цель задаётся позже: полоса, которая держит кнопку и живёт дольше неё.
    let button = unsafe { NSButton::buttonWithImage_target_action(&image, None, action, mtm) };
    button.setBordered(false);
    button.setImageScaling(NSImageScaling::ScaleProportionallyDown);
    button.setFrame(rect(0.0, 0.0, BUTTON, BUTTON));
    button.setToolTip(Some(&NSString::from_str(tip)));
    let label = NSString::from_str(label);
    let help = help.map(NSString::from_str);
    button.setAccessibilityLabel(Some(&label));
    button.setAccessibilityHelp(help.as_deref());
    // Элемент доступности кнопки — её ячейка, подпись и подсказка нужны и ей.
    if let Some(cell) = button.cell() {
        cell.setAccessibilityLabel(Some(&label));
        cell.setAccessibilityHelp(help.as_deref());
    }
    button
}

pub struct TabIvars {
    _live: Live,
    strip: Weak<TabStrip>,
    index: Cell<usize>,
    place: Place,
    icon: Option<Retained<NSImageView>>,
    title: Retained<NSTextField>,
    dot: Retained<NSView>,
    close: Retained<NSButton>,
    status: Cell<TabStatus>,
    width: Cell<f64>,
    selected: Cell<bool>,
    hovered: Cell<bool>,
}

define_class!(
    /// Вкладка: значок типа, подпись, точка статуса и кнопка закрытия. Элемент доступности с
    /// ролью переключателя вкладки; нажатие идёт тем же путём, что щелчок.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeTab"]
    #[ivars = TabIvars]
    pub struct TabView;

    impl TabView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// Щелчок по подписи, значку и точке достаётся вкладке, по кнопке закрытия — кнопке.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            // SAFETY: `hitTest:` у NSView с точкой в координатах родителя.
            let hit: Option<Retained<NSView>> = unsafe { msg_send![super(self), hitTest: point] };
            let close: &NSView = &self.ivars().close;
            hit.map(|view| {
                if std::ptr::eq(&*view, close) {
                    view
                } else {
                    Retained::into_super(self.retain())
                }
            })
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            if let Some(strip) = counters::owner("ProbeTab.mouseDown", self.ivars().strip.load()) {
                strip.press(self.index());
            }
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.ivars().hovered.set(true);
            self.restyle();
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().hovered.set(false);
            self.restyle();
        }

        #[unsafe(method(isAccessibilityElement))]
        fn ax_element(&self) -> bool {
            true
        }

        #[unsafe(method(isAccessibilityEnabled))]
        fn ax_enabled(&self) -> bool {
            true
        }

        #[unsafe(method_id(accessibilityRole))]
        fn ax_role(&self) -> Option<Retained<NSString>> {
            // SAFETY: имена ролей — неизменяемые константы AppKit.
            Some(unsafe { NSAccessibilityRadioButtonRole }.retain())
        }

        #[unsafe(method_id(accessibilitySubrole))]
        fn ax_subrole(&self) -> Option<Retained<NSString>> {
            // SAFETY: см. выше.
            Some(unsafe { NSAccessibilityTabButtonSubrole }.retain())
        }

        #[unsafe(method(isAccessibilitySelected))]
        fn ax_selected(&self) -> bool {
            self.ivars().selected.get()
        }

        /// `accessibilityPerformPress` сам действие в `accessibilityActionNames` вида не
        /// добавляет, поэтому список действий и их выполнение заданы явно.
        #[unsafe(method_id(accessibilityActionNames))]
        fn ax_action_names(&self) -> Retained<NSArray<NSString>> {
            // SAFETY: имена действий — неизменяемые константы AppKit.
            NSArray::from_slice(&[unsafe { NSAccessibilityPressAction }])
        }

        #[unsafe(method(accessibilityPerformAction:))]
        fn ax_perform_action(&self, action: &NSString) {
            // SAFETY: см. выше.
            if action == unsafe { NSAccessibilityPressAction } {
                self.press();
            }
        }
    }

    unsafe impl NSObjectProtocol for TabView {}

    unsafe impl NSAccessibilityElementProtocol for TabView {}

    unsafe impl NSAccessibilityButton for TabView {
        #[unsafe(method_id(accessibilityLabel))]
        fn ax_label(&self) -> Option<Retained<NSString>> {
            Some(self.ivars().title.stringValue())
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn ax_press(&self) -> bool {
            self.press()
        }
    }

    unsafe impl NSAccessibilityRadioButton for TabView {
        #[unsafe(method_id(accessibilityValue))]
        fn ax_value(&self) -> Option<Retained<NSNumber>> {
            Some(NSNumber::new_i32(i32::from(self.ivars().selected.get())))
        }
    }
);

impl TabView {
    fn new(
        mtm: MainThreadMarker,
        strip: &TabStrip,
        index: usize,
        spec: &TabSpec,
        tokens: &Tokens,
    ) -> Retained<TabView> {
        let place = strip.ivars().place;
        let icon = place.has_icons().then(|| {
            let view =
                NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(0.0, 0.0, ICON, ICON));
            view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
            view
        });
        let title = label(mtm, "", tokens.text_3, rect(0.0, 0.0, 10.0, 16.0));
        title.setFont(Some(&NSFont::systemFontOfSize(tokens.t_tab)));
        let dot = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, DOT, DOT));
        let close = icon_button(
            mtm,
            "xmark",
            Some(sel!(closeTab:)),
            "Закрыть вкладку",
            "Закрыть вкладку",
            None,
        );
        close.setFrame(rect(0.0, 0.0, CLOSE, CLOSE));
        let this = TabView::alloc(mtm).set_ivars(TabIvars {
            _live: TAB.live(),
            strip: Weak::new(strip),
            index: Cell::new(index),
            place,
            icon,
            title,
            dot,
            close,
            status: Cell::new(TabStatus::None),
            width: Cell::new(0.0),
            selected: Cell::new(false),
            hovered: Cell::new(false),
        });
        let frame = rect(0.0, TAB_TOP, 10.0, TAB_HEIGHT);
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let view: Retained<TabView> = unsafe { msg_send![super(this), initWithFrame: frame] };
        let ivars = view.ivars();
        if let Some(icon) = &ivars.icon {
            view.addSubview(icon);
        }
        view.addSubview(&ivars.title);
        view.addSubview(&ivars.dot);
        view.addSubview(&ivars.close);
        let target: &AnyObject = strip;
        // SAFETY: полоса держит вкладку и живёт дольше её кнопки закрытия.
        unsafe { ivars.close.setTarget(Some(target)) };
        // SAFETY: владелец — эта вкладка; область следует за видимой частью вида.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::ActiveInKeyWindow
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&area);
        view.configure(index, spec, tokens);
        view
    }

    pub fn index(&self) -> usize {
        self.ivars().index.get()
    }

    /// Нажатие доступности идёт тем же путём, что щелчок.
    fn press(&self) -> bool {
        match counters::owner(
            "ProbeTab.accessibilityPerformPress",
            self.ivars().strip.load(),
        ) {
            Some(strip) => {
                strip.press(self.index());
                true
            }
            None => false,
        }
    }

    pub fn natural_width(&self) -> f64 {
        self.ivars().width.get()
    }

    pub fn icon(&self) -> Option<&NSImageView> {
        self.ivars().icon.as_deref()
    }

    pub fn dot(&self) -> &NSView {
        &self.ivars().dot
    }

    pub fn title(&self) -> &NSTextField {
        &self.ivars().title
    }

    pub fn close_button(&self) -> &NSButton {
        &self.ivars().close
    }

    pub fn status(&self) -> TabStatus {
        self.ivars().status.get()
    }

    /// Подписи и значки по описанию вкладки; ширина — сумма частей с отступами `.tab`.
    fn configure(&self, index: usize, spec: &TabSpec, tokens: &Tokens) {
        let ivars = self.ivars();
        ivars.index.set(index);
        let control: &NSControl = &ivars.close;
        control.setTag(index as isize);
        ivars.status.set(spec.status);
        ivars.title.setStringValue(&NSString::from_str(&spec.title));
        let title = ivars.title.intrinsicContentSize();
        let (title_width, title_height) = (title.width.ceil(), title.height.ceil());
        let mut x = TAB_PAD_LEFT;
        if let Some(icon) = &ivars.icon {
            icon.setImage(symbol(kind_symbol(spec.kind), None).as_deref());
            icon.setFrame(rect(x, ((TAB_HEIGHT - ICON) / 2.0).floor(), ICON, ICON));
            x += ICON + TAB_GAP;
        }
        ivars.title.setFrame(rect(
            x,
            ((TAB_HEIGHT - title_height) / 2.0).floor(),
            title_width,
            title_height,
        ));
        x += title_width;
        let has_dot = spec.status != TabStatus::None;
        ivars.dot.setHidden(!has_dot);
        if has_dot {
            x += TAB_GAP;
            let (height, y) = match spec.status {
                TabStatus::Exit => (2.0, ((TAB_HEIGHT - 2.0) / 2.0).floor()),
                _ => (DOT, ((TAB_HEIGHT - DOT) / 2.0).floor()),
            };
            ivars.dot.setFrame(rect(x, y, DOT, height));
            x += DOT;
        }
        let close_y = ((TAB_HEIGHT - CLOSE) / 2.0).floor();
        let width = match ivars.place {
            Place::Center | Place::Bottom => {
                x += TAB_GAP;
                ivars.close.setFrame(rect(x, close_y, CLOSE, CLOSE));
                x + CLOSE + TAB_PAD_RIGHT
            }
            // Сбоку кнопка закрытия лежит поверх правого края вкладки и видна при наведении.
            Place::Side => {
                let width = x + TAB_PAD_RIGHT;
                ivars
                    .close
                    .setFrame(rect(width - 2.0 - CLOSE, close_y, CLOSE, CLOSE));
                width
            }
        };
        ivars.width.set(width);
        self.paint(tokens);
    }

    fn set_selected(&self, selected: bool, tokens: &Tokens) {
        self.ivars().selected.set(selected);
        self.paint(tokens);
    }

    fn restyle(&self) {
        if let Some(strip) = counters::owner("ProbeTab.restyle", self.ivars().strip.load()) {
            self.paint(&strip.tokens());
        }
    }

    /// Активная вкладка на фоне плитки `--panel` с рамкой `--line`, наведённая — на `--hover`.
    fn paint(&self, tokens: &Tokens) {
        let ivars = self.ivars();
        let (selected, hovered) = (ivars.selected.get(), ivars.hovered.get());
        let (background, ink) = if selected {
            (Some(tokens.panel), tokens.text)
        } else if hovered {
            (Some(tokens.hover), tokens.text_2)
        } else {
            (None, tokens.text_3)
        };
        set_layer(
            self,
            background,
            selected.then_some((1.0, tokens.line)),
            tokens.r_row,
        );
        let ink_color = color(ink, 1.0);
        ivars.title.setTextColor(Some(&ink_color));
        if let Some(icon) = &ivars.icon {
            icon.setContentTintColor(Some(&ink_color));
        }
        let close_visible = match ivars.place {
            Place::Center | Place::Bottom => selected || hovered,
            Place::Side => hovered,
        };
        ivars.close.setHidden(!close_visible);
        ivars
            .close
            .setContentTintColor(Some(&color(tokens.text_3, 1.0)));
        if ivars.place == Place::Side {
            set_layer(&ivars.close, Some(tokens.hover), None, 4.0);
        }
        match ivars.status.get() {
            TabStatus::Run => set_layer(&ivars.dot, Some(tokens.run), None, DOT / 2.0),
            TabStatus::Idle => set_layer(&ivars.dot, None, Some((1.5, tokens.text_3)), DOT / 2.0),
            TabStatus::Exit => set_layer(&ivars.dot, Some(tokens.text_3), None, 1.0),
            TabStatus::None => {}
        }
    }
}

pub struct RowIvars {
    _live: Live,
    label: Retained<NSString>,
}

define_class!(
    /// Ряд вкладок внутри прокрутки. Для доступности — группа вкладок, дети которой все вкладки,
    /// и прокрученные за край тоже.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeTabRow"]
    #[ivars = RowIvars]
    pub struct TabRow;

    impl TabRow {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(isAccessibilityElement))]
        fn ax_element(&self) -> bool {
            true
        }

        #[unsafe(method_id(accessibilityRole))]
        fn ax_role(&self) -> Option<Retained<NSString>> {
            // SAFETY: имена ролей — неизменяемые константы AppKit.
            Some(unsafe { NSAccessibilityTabGroupRole }.retain())
        }

        #[unsafe(method_id(accessibilityLabel))]
        fn ax_label(&self) -> Option<Retained<NSString>> {
            Some(self.ivars().label.clone())
        }

        #[unsafe(method_id(accessibilityChildren))]
        fn ax_children(&self) -> Option<Retained<NSArray<TabView>>> {
            Some(NSArray::from_retained_slice(&self.tab_views()))
        }

        #[unsafe(method_id(accessibilityChildrenInNavigationOrder))]
        fn ax_navigation_children(&self) -> Option<Retained<NSArray<TabView>>> {
            Some(NSArray::from_retained_slice(&self.tab_views()))
        }

        #[unsafe(method_id(accessibilityTabs))]
        fn ax_tabs(&self) -> Option<Retained<NSArray<TabView>>> {
            Some(NSArray::from_retained_slice(&self.tab_views()))
        }
    }
);

impl TabRow {
    fn new(mtm: MainThreadMarker, title: &str) -> Retained<TabRow> {
        let this = TabRow::alloc(mtm).set_ivars(RowIvars {
            _live: TAB_ROW.live(),
            label: NSString::from_str(&format!("Вкладки: {title}")),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, 10.0, 34.0)] }
    }

    /// Вкладки ряда слева направо.
    pub fn tab_views(&self) -> Vec<Retained<TabView>> {
        let mut tabs: Vec<Retained<TabView>> = self
            .subviews()
            .iter()
            .filter_map(|view| view.downcast::<TabView>().ok())
            .collect();
        tabs.sort_by(|a, b| a.frame().origin.x.total_cmp(&b.frame().origin.x));
        tabs
    }
}

pub struct ScrollIvars {
    _live: Live,
}

define_class!(
    /// Прокрутка ряда вкладок без полос прокрутки. Вертикальное колесо мыши двигает ряд по
    /// горизонтали.
    #[unsafe(super(NSScrollView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeTabScroll"]
    #[ivars = ScrollIvars]
    pub struct TabScroll;

    impl TabScroll {
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let (dx, dy) = (event.scrollingDeltaX(), event.scrollingDeltaY());
            if dy.abs() <= dx.abs() {
                // SAFETY: `scrollWheel:` у NSScrollView с событием прокрутки.
                let _: () = unsafe { msg_send![super(self), scrollWheel: event] };
                return;
            }
            let Some(document) = self.documentView() else {
                return;
            };
            let clip = self.contentView();
            let bounds = clip.bounds();
            let step = if event.hasPreciseScrollingDeltas() { 1.0 } else { WHEEL_LINE };
            let max = (document.frame().size.width - bounds.size.width).max(0.0);
            let x = (bounds.origin.x - dy * step).clamp(0.0, max);
            clip.scrollToPoint(NSPoint::new(x, bounds.origin.y));
            self.reflectScrolledClipView(&clip);
        }
    }
);

impl TabScroll {
    fn new(mtm: MainThreadMarker, row: &TabRow) -> Retained<TabScroll> {
        let this = TabScroll::alloc(mtm).set_ivars(ScrollIvars {
            _live: TAB_SCROLL.live(),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let scroll: Retained<TabScroll> =
            unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, 10.0, 34.0)] };
        scroll.setHasHorizontalScroller(false);
        scroll.setHasVerticalScroller(false);
        scroll.setDrawsBackground(false);
        scroll.contentView().setDrawsBackground(false);
        scroll.setVerticalScrollElasticity(NSScrollElasticity::None);
        scroll.setDocumentView(Some(row));
        scroll
    }
}

pub struct StripIvars {
    _live: Live,
    place: Place,
    tokens: Cell<Tokens>,
    scroll: Retained<TabScroll>,
    row: Retained<TabRow>,
    line: Retained<NSView>,
    border: Retained<NSView>,
    meter: Retained<NSTextField>,
    split: Retained<NSButton>,
    zoom: Retained<NSButton>,
    more: Retained<NSButton>,
    menu: Retained<NSMenu>,
    header: Retained<NSMenuItem>,
    specs: RefCell<Vec<TabSpec>>,
    views: RefCell<Vec<Retained<TabView>>>,
    active: Cell<Option<usize>>,
    focused: Cell<bool>,
    focus_target: RefCell<Option<Weak<NSView>>>,
    keys: Cell<usize>,
}

define_class!(
    /// Полоса вкладок плитки. Сама фокус не принимает: щелчок по пустому месту уходит плитке.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeTabStrip"]
    #[ivars = StripIvars]
    pub struct TabStrip;

    unsafe impl NSObjectProtocol for TabStrip {}

    impl TabStrip {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(resizeSubviewsWithOldSize:))]
        fn resize_subviews(&self, _old: NSSize) {
            self.place();
            self.reveal_active();
        }

        #[unsafe(method(closeTab:))]
        fn close_tab(&self, sender: Option<&AnyObject>) {
            let index = sender
                .and_then(|sender| sender.downcast_ref::<NSControl>())
                .map(|control| control.tag());
            if let Some(index) = index.and_then(|tag| usize::try_from(tag).ok()) {
                self.close(index);
            }
        }

        #[unsafe(method(closeActive:))]
        fn close_active(&self, _sender: Option<&AnyObject>) {
            if let Some(index) = self.active() {
                self.close(index);
            }
        }

        #[unsafe(method(showMenu:))]
        fn show_menu(&self, _sender: Option<&AnyObject>) {
            let more = &self.ivars().more;
            let height = more.bounds().size.height;
            let y = if more.isFlipped() { height + 2.0 } else { -2.0 };
            self.ivars().menu.popUpMenuPositioningItem_atLocation_inView(
                None,
                NSPoint::new(0.0, y),
                Some(more),
            );
        }
    }
);

impl TabStrip {
    pub fn new(
        mtm: MainThreadMarker,
        place: Place,
        title: &str,
        tokens: &Tokens,
        width: f64,
    ) -> Retained<TabStrip> {
        let row = TabRow::new(mtm, title);
        let scroll = TabScroll::new(mtm, &row);
        let line = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        line.setHidden(true);
        row.addSubview(&line);
        let border = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        let meter = label(mtm, "", tokens.text_3, rect(0.0, 0.0, 10.0, 15.0));
        meter.setFont(Some(&NSFont::systemFontOfSize(tokens.t_meta)));
        let (split_label, split_key, split_symbol) = place.split_action();
        let split = icon_button(
            mtm,
            split_symbol,
            None,
            split_label,
            &format!("{split_label}  {split_key}"),
            Some(split_key),
        );
        let (zoom_label, zoom_key) = ZOOM_ACTION;
        let zoom = icon_button(
            mtm,
            "arrow.up.left.and.arrow.down.right",
            None,
            zoom_label,
            &format!("{zoom_label}  {zoom_key}"),
            Some(zoom_key),
        );
        for sample in [&split, &zoom] {
            let control: &NSControl = sample;
            control.setEnabled(false);
        }
        let more = icon_button(
            mtm,
            "ellipsis",
            Some(sel!(showMenu:)),
            MENU_LABEL,
            MENU_LABEL,
            None,
        );
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(MENU_LABEL));
        let header = NSMenuItem::sectionHeaderWithTitle(ns_string!(""), mtm);
        menu.addItem(&header);
        let close = NSMenuItem::new(mtm);
        close.setTitle(ns_string!("Закрыть вкладку"));
        // SAFETY: `closeActive:` реализован полосой, цель задаётся после её создания.
        unsafe { close.setAction(Some(sel!(closeActive:))) };
        menu.addItem(&close);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        for title in [split_label, zoom_label] {
            let sample = NSMenuItem::new(mtm);
            sample.setTitle(&NSString::from_str(title));
            menu.addItem(&sample);
        }
        let this = TabStrip::alloc(mtm).set_ivars(StripIvars {
            _live: TAB_STRIP.live(),
            place,
            tokens: Cell::new(*tokens),
            scroll,
            row,
            line,
            border,
            meter,
            split,
            zoom,
            more,
            menu,
            header,
            specs: RefCell::new(Vec::new()),
            views: RefCell::new(Vec::new()),
            active: Cell::new(None),
            focused: Cell::new(false),
            focus_target: RefCell::new(None),
            keys: Cell::new(0),
        });
        let frame = rect(0.0, 0.0, width, tokens.tabbar);
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let strip: Retained<TabStrip> = unsafe { msg_send![super(this), initWithFrame: frame] };
        let ivars = strip.ivars();
        let target: &AnyObject = &strip;
        // SAFETY: кнопки и пункт меню держит полоса, поэтому цель живёт дольше них.
        unsafe {
            ivars.more.setTarget(Some(target));
            close.setTarget(Some(target));
        }
        for view in [
            &*ivars.border,
            &ivars.scroll,
            &ivars.meter,
            &ivars.split,
            &ivars.zoom,
            &ivars.more,
        ] {
            strip.addSubview(view);
        }
        strip.paint();
        strip
    }

    pub fn place_kind(&self) -> Place {
        self.ivars().place
    }

    pub fn tokens(&self) -> Tokens {
        self.ivars().tokens.get()
    }

    /// Токены новой ячейки: цвета полосы, вкладок и нагрузки, толщина линии `--focus-h`.
    pub fn set_tokens(&self, tokens: &Tokens) {
        self.ivars().tokens.set(*tokens);
        self.ivars()
            .meter
            .setTextColor(Some(&color(tokens.text_3, 1.0)));
        self.paint();
    }

    /// Вид, которому щелчок и нажатие вкладки сначала передают фокус: плитка полосы.
    pub fn set_focus_target(&self, view: &NSView) {
        *self.ivars().focus_target.borrow_mut() = Some(Weak::new(view));
    }

    /// Новый набор вкладок. Виды вкладок переиспользуются, лишние убираются.
    pub fn set_tabs(&self, specs: Vec<TabSpec>) {
        let ivars = self.ivars();
        let tokens = self.tokens();
        {
            let mut views = ivars.views.borrow_mut();
            while views.len() > specs.len() {
                if let Some(view) = views.pop() {
                    view.removeFromSuperview();
                }
            }
            for (index, spec) in specs.iter().enumerate() {
                match views.get(index) {
                    Some(view) => view.configure(index, spec, &tokens),
                    None => {
                        let view = TabView::new(self.mtm(), self, index, spec, &tokens);
                        ivars.row.addSubview(&view);
                        views.push(view);
                    }
                }
            }
        }
        let active = match ivars.active.get() {
            _ if specs.is_empty() => None,
            Some(active) if active < specs.len() => Some(active),
            _ => Some(0),
        };
        *ivars.specs.borrow_mut() = specs;
        ivars.active.set(active);
        self.paint();
        self.reveal_active();
    }

    pub fn active(&self) -> Option<usize> {
        self.ivars().active.get()
    }

    pub fn len(&self) -> usize {
        self.ivars().specs.borrow().len()
    }

    pub fn titles(&self) -> Vec<String> {
        self.ivars()
            .specs
            .borrow()
            .iter()
            .map(|spec| spec.title.clone())
            .collect()
    }

    /// Нагрузка активной вкладки.
    pub fn active_meter(&self) -> Option<Meter> {
        let active = self.active()?;
        self.ivars()
            .specs
            .borrow()
            .get(active)
            .map(|spec| spec.meter.clone())
    }

    /// Щелчок и нажатие доступности: фокус плитке, затем выбор вкладки.
    pub fn press(&self, index: usize) {
        let target = self
            .ivars()
            .focus_target
            .borrow()
            .as_ref()
            .and_then(|target| counters::owner("ProbeTabStrip.press", target.load()));
        if let (Some(target), Some(window)) = (target, self.window()) {
            window.makeFirstResponder(Some(&target));
        }
        self.select(index);
    }

    pub fn select(&self, index: usize) {
        if index >= self.len() {
            return;
        }
        self.ivars().active.set(Some(index));
        self.paint();
        self.reveal_active();
    }

    pub fn close(&self, index: usize) {
        let len = self.len();
        if index >= len {
            return;
        }
        let active = after_close(self.active(), index, len);
        let mut specs = self.ivars().specs.borrow().clone();
        specs.remove(index);
        self.ivars().active.set(active);
        self.set_tabs(specs);
    }

    /// Полоса видна: в окне, без скрытых предков и с непустой видимой частью.
    pub fn visible(&self) -> bool {
        let visible = self.visibleRect();
        self.window().is_some()
            && !self.isHiddenOrHasHiddenAncestor()
            && visible.size.width > 0.0
            && visible.size.height > 0.0
    }

    /// Клавиша вкладок от плитки с фокусом. Скрытая полоса клавиш не принимает.
    pub fn handle_key(&self, event: &NSEvent) -> bool {
        if !self.visible() {
            return false;
        }
        let flags = event.modifierFlags();
        let shift = flags.contains(NSEventModifierFlags::Shift);
        let other = flags.intersects(
            NSEventModifierFlags::Command
                | NSEventModifierFlags::Control
                | NSEventModifierFlags::Option,
        );
        let Some(action) = key_action(event.keyCode(), shift, other) else {
            return false;
        };
        let Some(index) = next_index(self.active(), self.len(), action) else {
            return false;
        };
        let keys = &self.ivars().keys;
        keys.set(keys.get() + 1);
        self.select(index);
        true
    }

    /// Сколько клавиш вкладок полоса приняла.
    pub fn keys_handled(&self) -> usize {
        self.ivars().keys.get()
    }

    /// Линия под активной вкладкой только у плитки с фокусом.
    pub fn set_focused(&self, focused: bool) {
        self.ivars().focused.set(focused);
        self.place();
    }

    pub fn tab_views(&self) -> Vec<Retained<TabView>> {
        self.ivars().views.borrow().clone()
    }

    pub fn row(&self) -> &TabRow {
        &self.ivars().row
    }

    pub fn scroll(&self) -> &TabScroll {
        &self.ivars().scroll
    }

    pub fn line(&self) -> &NSView {
        &self.ivars().line
    }

    /// Нижняя линия полосы.
    pub fn border(&self) -> &NSView {
        &self.ivars().border
    }

    pub fn meter(&self) -> &NSTextField {
        &self.ivars().meter
    }

    /// «Разделить», «Развернуть панель» и «…».
    pub fn buttons(&self) -> [&NSButton; 3] {
        let ivars = self.ivars();
        [&ivars.split, &ivars.zoom, &ivars.more]
    }

    pub fn menu(&self) -> &NSMenu {
        &self.ivars().menu
    }

    /// Пункты сцены в конце меню «…» после разделителя.
    pub fn add_menu_items(&self, items: &[Retained<NSMenuItem>]) {
        let menu = &self.ivars().menu;
        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        for item in items {
            menu.addItem(item);
        }
    }

    /// Цвета по токенам, выбор, нагрузка и заголовок меню; затем раскладка.
    fn paint(&self) {
        let ivars = self.ivars();
        let tokens = self.tokens();
        fill(self, tokens.panel_head, 1.0);
        fill(&ivars.border, tokens.line, 1.0);
        fill(&ivars.line, tokens.focus, 1.0);
        if let Some(layer) = ivars.line.layer() {
            layer.setCornerRadius(1.0);
        }
        let ink = color(tokens.text_3, 1.0);
        for button in self.buttons() {
            button.setContentTintColor(Some(&ink));
        }
        let active = self.active();
        for (index, view) in ivars.views.borrow().iter().enumerate() {
            view.set_selected(Some(index) == active, &tokens);
        }
        let meter = self.active_meter();
        let text = meter.as_ref().map_or("", Meter::text);
        ivars.meter.setStringValue(&NSString::from_str(text));
        ivars
            .meter
            .setToolTip(meter.as_ref().map(|meter| match meter {
                Meter::Measured(_) => ns_string!("Измерено: процесс вкладки"),
                Meter::Estimate(_) => ns_string!("Оценка: панель работает внутри процесса shellio"),
            }));
        ivars.header.setTitle(&NSString::from_str(text));
        ivars.header.setHidden(meter.is_none());
        self.place();
    }

    /// Кадры по `layout`: прокрутка, вкладки, нагрузка, кнопки, линии.
    fn place(&self) {
        let ivars = self.ivars();
        let tokens = self.tokens();
        let size = self.bounds().size;
        let views = ivars.views.borrow();
        let widths: Vec<f64> = views.iter().map(|view| view.natural_width()).collect();
        let meter = self.active_meter().map(|_| meter_width(&ivars.meter));
        let layout = layout(size.width, &widths, meter);
        let (viewport_x, viewport_width) = layout.viewport;
        ivars
            .scroll
            .setFrame(rect(viewport_x, 0.0, viewport_width, size.height));
        ivars.row.setFrame(rect(
            0.0,
            0.0,
            layout.content.max(viewport_width),
            size.height,
        ));
        for ((view, x), width) in views.iter().zip(&layout.tabs).zip(&widths) {
            view.setFrame(rect(*x, TAB_TOP, *width, TAB_HEIGHT));
        }
        ivars.meter.setHidden(layout.meter.is_none());
        if let Some(x) = layout.meter {
            let label = ivars.meter.intrinsicContentSize();
            let (width, height) = (label.width.ceil(), label.height.ceil());
            ivars.meter.setFrame(rect(
                x + METER_PAD,
                ((size.height - height) / 2.0).floor(),
                width,
                height,
            ));
        }
        let button_y = ((size.height - BUTTON) / 2.0).floor();
        for (button, x) in self.buttons().into_iter().zip(layout.buttons) {
            button.setFrame(rect(x, button_y, BUTTON, BUTTON));
        }
        ivars
            .border
            .setFrame(rect(0.0, size.height - 1.0, size.width, 1.0));
        let under = self
            .active()
            .and_then(|active| views.get(active))
            .filter(|_| ivars.focused.get())
            .map(|view| view.frame());
        ivars.line.setHidden(under.is_none());
        if let Some(tab) = under {
            ivars.line.setFrame(rect(
                tab.origin.x + LINE_INSET,
                size.height - 1.0 - tokens.focus_h,
                (tab.size.width - 2.0 * LINE_INSET).max(0.0),
                tokens.focus_h,
            ));
        }
    }

    /// Прокручивает ряд так, чтобы активная вкладка была видна целиком.
    fn reveal_active(&self) {
        let Some(view) = self
            .active()
            .and_then(|active| self.ivars().views.borrow().get(active).cloned())
        else {
            return;
        };
        let frame = view.frame();
        self.ivars().row.scrollRectToVisible(rect(
            frame.origin.x - ROW_INSET,
            frame.origin.y,
            frame.size.width + 2.0 * ROW_INSET,
            frame.size.height,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLUSTER: f64 = 3.0 * BUTTON + 2.0 * GAP + PAD_RIGHT;

    #[test]
    fn layout_fits_tabs_and_meter() {
        let layout = layout(360.0, &[100.0], Some(80.0));
        assert_eq!(layout.regime, Regime::Fit);
        assert_eq!(layout.buttons, [284.0, 308.0, 332.0]);
        // Нагрузка вплотную к «Разделить» через зазор.
        assert_eq!(layout.meter, Some(284.0 - GAP - 80.0));
        assert_eq!(layout.tabs, vec![ROW_INSET]);
        assert_eq!(layout.viewport, (3.0, 102.0));
        assert_eq!(layout.no_meter_width, PAD_LEFT + 100.0 + GAP + CLUSTER);
        assert_eq!(layout.fit_width, layout.no_meter_width + 80.0 + GAP);
    }

    #[test]
    fn layout_moves_meter_to_menu_then_scrolls() {
        let tabs = [120.0, 120.0];
        let fit = layout(1000.0, &tabs, Some(80.0)).fit_width;
        let no_meter = layout(1000.0, &tabs, Some(80.0)).no_meter_width;
        assert_eq!(fit - no_meter, 82.0);
        assert_eq!(layout(fit, &tabs, Some(80.0)).regime, Regime::Fit);
        let middle = layout(fit - 1.0, &tabs, Some(80.0));
        assert_eq!(middle.regime, Regime::MeterInMenu);
        assert_eq!(middle.meter, None);
        assert_eq!(middle.viewport.1, middle.content);
        let scroll = layout(no_meter - 1.0, &tabs, Some(80.0));
        assert_eq!(scroll.regime, Regime::Scroll);
        assert_eq!(scroll.meter, None);
        assert!(scroll.viewport.1 < scroll.content);
        // Без нагрузки середины нет.
        assert_eq!(layout(no_meter, &tabs, None).regime, Regime::Fit);
        assert_eq!(layout(no_meter - 1.0, &tabs, None).regime, Regime::Scroll);
    }

    #[test]
    fn buttons_never_move_between_regimes() {
        let tabs: Vec<f64> = vec![90.0; 30];
        let width = 572.0;
        let wide = layout(width, &tabs[..1], Some(80.0));
        let narrow = layout(width, &tabs, Some(80.0));
        assert_eq!(wide.regime, Regime::Fit);
        assert_eq!(narrow.regime, Regime::Scroll);
        assert_eq!(wide.buttons, narrow.buttons);
        // Прокрутка заканчивается перед кнопками.
        assert!(narrow.viewport.0 + narrow.viewport.1 <= narrow.buttons[0] - GAP + ROW_INSET);
        // Полоса уже кнопок: прокрутка нулевая, кнопки на месте.
        let tiny = layout(40.0, &tabs, Some(80.0));
        assert_eq!(tiny.viewport.1, 2.0 * ROW_INSET);
        assert_eq!(tiny.buttons[2], 40.0 - PAD_RIGHT - BUTTON);
    }

    #[test]
    fn empty_strip_layout() {
        let layout = layout(360.0, &[], None);
        assert_eq!(layout.regime, Regime::Fit);
        assert!(layout.tabs.is_empty());
        assert_eq!(layout.content, 2.0 * ROW_INSET);
        assert_eq!(layout.meter, None);
    }

    #[test]
    fn keys_map_by_physical_key() {
        assert_eq!(key_action(37, true, false), Some(KeyAction::Next));
        assert_eq!(key_action(4, true, false), Some(KeyAction::Prev));
        assert_eq!(key_action(37, false, false), None);
        assert_eq!(key_action(18, false, false), Some(KeyAction::Number(0)));
        assert_eq!(key_action(25, false, false), Some(KeyAction::Number(8)));
        assert_eq!(key_action(18, true, false), None);
        assert_eq!(key_action(37, true, true), None);
        assert_eq!(key_action(125, false, false), None);
    }

    #[test]
    fn next_index_wraps_and_ignores_missing_numbers() {
        assert_eq!(next_index(Some(0), 3, KeyAction::Prev), Some(2));
        assert_eq!(next_index(Some(2), 3, KeyAction::Next), Some(0));
        assert_eq!(next_index(Some(1), 3, KeyAction::Number(2)), Some(2));
        assert_eq!(next_index(Some(1), 3, KeyAction::Number(5)), None);
        assert_eq!(next_index(None, 0, KeyAction::Next), None);
        assert_eq!(next_index(None, 2, KeyAction::Next), Some(1));
    }

    #[test]
    fn close_keeps_a_neighbour_active() {
        assert_eq!(after_close(Some(2), 0, 4), Some(1));
        assert_eq!(after_close(Some(1), 1, 4), Some(1));
        assert_eq!(after_close(Some(3), 3, 4), Some(2));
        assert_eq!(after_close(Some(0), 2, 4), Some(0));
        assert_eq!(after_close(Some(0), 0, 1), None);
        assert_eq!(after_close(None, 0, 0), None);
    }
}
