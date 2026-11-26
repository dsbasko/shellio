//! Контейнер панели хоста ([`PanelView`]) и нейтральная карточка ([`NeutralCard`]). Контейнер живёт
//! столько же, сколько панель, переносится между плитками без пересоздания и держит тело панели.
//! Карточка — тело без собственного содержимого: название, plugin id, зона и класс размера.
//! [`ExpandStub`] — заглушка хоста на месте тела: «Развернуть» у тела меньше `min_size` панели
//! (ADR-0009, ADR-0031) или причина статуса панели без кнопки (ADR-0008). Контейнер панели под ней
//! скрыт и жив. [`EmptyNote`] — сообщение на месте области зон: «Нет открытых панелей» или «Рабочая
//! область не открыта».

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityGroupRole, NSApplication, NSAutoresizingMaskOptions,
    NSBaselineOffsetAttributeName, NSButton, NSColor, NSControl, NSEvent, NSEventType, NSFont,
    NSFontAttributeName, NSFontWeightMedium, NSFontWeightRegular, NSForegroundColorAttributeName,
    NSLayoutManager, NSLineBreakMode, NSMutableParagraphStyle, NSParagraphStyleAttributeName,
    NSResponder, NSText, NSTextAlignment, NSTextField, NSView, NSViewLayerContentsRedrawPolicy,
};
use objc2_foundation::{
    NSAlignmentOptions, NSAttributedString, NSMutableAttributedString, NSNumber, NSObject, NSPoint,
    NSRange, NSRect, NSSize, NSString,
};
use shellio_theme::AppTheme;

use crate::app::App;
use crate::chrome_bar::Frame;
use crate::components::tab_strip::model::{self as strip, StripButton};
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode};
use crate::layer::{self, without_actions};
use crate::own::{Callback, ComponentId, View};
use crate::piece::{Piece, ns_rect};
use crate::runloop;
use crate::style;

counter!(PANEL_OWNER, Owner, "PanelView");
counter!(PANEL_VIEW, Class, "ShellioPanelView");
counter!(CARD_OWNER, Owner, "NeutralCard");
counter!(CARD_VIEW, Class, "ShellioNeutralCard");
counter!(STUB_OWNER, Owner, "ExpandStub");
counter!(STUB_VIEW, Class, "ShellioExpandStub");
counter!(STUB_BUTTON, Class, "ShellioExpandButton");
counter!(NOTE_OWNER, Owner, "EmptyNote");
counter!(NOTE_VIEW, Class, "ShellioEmptyNote");

/// Отступ текста карточки от краёв тела.
const PAD: f64 = 14.0;
/// Высота строки текста карточки.
const LINE: f64 = 18.0;

struct PanelModel {
    id: ComponentId,
    theme: Cell<AppTheme>,
    title: RefCell<String>,
    /// Контейнер колонки навигатора лежит на фоне окна без своей плитки (ADR-0024).
    flat: Cell<bool>,
    on_press: Box<dyn Fn()>,
}

/// Контейнер панели на главном потоке: место тела панели и её имя в AX. Освобождение убирает его
/// из иерархии.
pub struct PanelView {
    view: View<PanelStubView, PanelModel>,
}

impl PanelView {
    /// Контейнер панели с названием `title`; `on_press` получает щелчок в тело на главном потоке
    /// изнутри AppKit: и тот, что тело не обработало, и тот, что достался вложенному компоненту
    /// тела. Одно нажатие — один вызов.
    pub fn new(
        app: &App,
        theme: &AppTheme,
        title: &str,
        on_press: impl Fn() + 'static,
    ) -> PanelView {
        let mtm = app.mtm();
        let model = PanelModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
            title: RefCell::new(title.to_owned()),
            flat: Cell::new(false),
            on_press: Box::new(on_press),
        };
        let view = View::new(&PANEL_OWNER, model, "PanelView.updateLayer", |owner| {
            PanelStubView::new(mtm, owner)
        });
        let panel = PanelView { view };
        panel.show_title();
        panel
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    pub fn title(&self) -> String {
        self.view.rust().title.borrow().clone()
    }

    pub fn set_title(&self, title: &str) {
        let model = self.view.rust();
        if *model.title.borrow() != title {
            model.title.replace(title.to_owned());
            self.show_title();
        }
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        let model = self.view.rust();
        if model.theme.get() != *theme {
            model.theme.set(*theme);
            self.view.view().setNeedsDisplay(true);
        }
    }

    /// Плоский контейнер красится фоном окна, обычный — фоном панели.
    pub fn set_flat(&self, flat: bool) {
        let model = self.view.rust();
        if model.flat.get() != flat {
            model.flat.set(flat);
            self.view.view().setNeedsDisplay(true);
        }
    }

    /// Ставит тело на всю площадь контейнера; дальше оно растягивается вместе с ним. Тело из
    /// другого родителя переставляется без пересоздания.
    pub fn set_body<'a>(&self, piece: impl Into<Piece<'a>>) {
        let piece = piece.into();
        let body = piece.ns_view();
        let host: &NSView = self.view.view();
        let bounds = host.bounds();
        if body.frame() != bounds {
            body.setFrame(bounds);
        }
        // SAFETY: `superview` у живого вида только читается.
        let parent = unsafe { body.superview() };
        if !parent.is_some_and(|parent| std::ptr::eq(&*parent, host)) {
            host.addSubview(body);
        }
        body.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
    }

    /// Прячет или показывает контейнер без неявных анимаций. Скрытый контейнер ввод не держит:
    /// иначе AppKit сам отдал бы его следующему виду цикла клавиш.
    pub fn set_hidden(&self, hidden: bool) {
        let view = self.view.view();
        if view.isHidden() != hidden {
            if hidden {
                self.resign_focus();
            }
            without_actions(|| view.setHidden(hidden));
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.view.view().isHidden()
    }

    /// Ввод внутри контейнера: first responder окна — вид его тела.
    pub fn holds_focus(&self) -> bool {
        let host: &NSView = self.view.view();
        host.window()
            .and_then(|window| window.firstResponder())
            .and_then(|responder| {
                responder
                    .downcast_ref::<NSView>()
                    .map(|view| view.isDescendantOf(host))
            })
            .unwrap_or(false)
    }

    /// Ввод у поля внутри контейнера: first responder окна — редактор поля его тела.
    pub fn holds_field(&self) -> bool {
        let host: &NSView = self.view.view();
        host.window()
            .and_then(|window| window.firstResponder())
            .and_then(|responder| {
                responder
                    .downcast_ref::<NSText>()
                    .map(|editor| editor.isFieldEditor() && editor.isDescendantOf(host))
            })
            .unwrap_or(false)
    }

    /// Отдаёт ввод окну, если он внутри контейнера. `true` — ввод был здесь.
    pub fn resign_focus(&self) -> bool {
        let held = self.holds_focus();
        if held && let Some(window) = self.view.view().window() {
            window.makeFirstResponder(None);
        }
        held
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }

    /// Контейнер для `ui.inspect`.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> InspectNode {
        inspect::node(id, "PanelView", parent, self.view.view(), None, None)
    }

    fn show_title(&self) {
        let title = self.view.rust().title.borrow();
        self.view
            .view()
            .setAccessibilityLabel(Some(&NSString::from_str(&title)));
    }
}

struct PanelIvars {
    _live: Live,
    owner: Callback<PanelModel>,
    /// Нажатие, о котором владелец уже знает: номер и время события.
    pressed: Cell<Option<(isize, u64)>>,
    /// Нажатие, адресат которого уже искали при выборе адресата.
    judged: Cell<Option<(isize, u64)>>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioPanelView"]
    #[ivars = PanelIvars]
    struct PanelStubView;

    impl PanelStubView {
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
            let Some((theme, flat)) = self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                (model.theme.get(), model.flat.get())
            }) else {
                return;
            };
            let fill = if flat {
                theme.colors.chrome
            } else {
                theme.colors.panel
            };
            if let Some(layer) = self.layer() {
                without_actions(|| layer::set_fill(&layer, fill));
            }
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder. Щелчок в тело, которое его
        // не обработало, приходит сюда по цепочке ответчиков.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.press(event);
        }

        // SAFETY: селектор, аргумент и результат — как у `hitTest:` NSView. Адресат нажатия остаётся
        // тем, что выбрал NSView: вложенный компонент тела получает его как обычно. Контейнер только
        // узнаёт о нажатии, которое до него по цепочке ответчиков не дойдёт.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            // SAFETY: `hitTest:` у NSView с той же точкой.
            let hit: *mut NSView = unsafe { msg_send![super(self), hitTest: point] };
            let own: &NSView = self;
            if !hit.is_null() && !std::ptr::eq(hit.cast_const(), own) && !PROBING.get() {
                let event = NSApplication::sharedApplication(self.mtm()).currentEvent();
                if let Some(event) = event
                    && event.r#type() == NSEventType::LeftMouseDown
                    && self.ivars().judged.replace(Some(press_key(&event))) != Some(press_key(&event))
                    && self.receives(&event)
                {
                    self.press(&event);
                }
            }
            hit
        }

        // SAFETY: селектор и результат — как у `acceptsFirstMouse:` NSView.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }
    }
);

impl PanelStubView {
    fn new(mtm: MainThreadMarker, owner: Callback<PanelModel>) -> Retained<PanelStubView> {
        let this = PanelStubView::alloc(mtm).set_ivars(PanelIvars {
            _live: PANEL_VIEW.live(),
            owner,
            pressed: Cell::new(None),
            judged: Cell::new(None),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<PanelStubView> = unsafe {
            msg_send![super(this), initWithFrame: NSRect::new(NSPoint::ZERO, NSSize::new(2.0 * PAD, 3.0 * LINE))]
        };
        layered(&view);
        view.setAccessibilityElement(true);
        // SAFETY: роль доступности — неизменяемая константа AppKit.
        view.setAccessibilityRole(Some(unsafe { NSAccessibilityGroupRole }));
        view
    }

    /// Нажатие `event` в теле: владелец узнаёт о нём один раз, каким бы путём оно ни пришло —
    /// выбором адресата или по цепочке ответчиков.
    fn press(&self, event: &NSEvent) {
        let key = press_key(event);
        if self.ivars().pressed.replace(Some(key)) != Some(key) {
            self.ivars().owner.call(|model| (model.on_press)());
        }
    }

    /// Нажатие `event` достаётся виду внутри контейнера. Адресат ищется от содержимого окна в точке
    /// события, как его ищет само окно: вопрос `hitTest:` в другой точке и нажатие, которое забрал
    /// вид поверх тела (разделитель, вкладка), нажатием в тело не считаются.
    fn receives(&self, event: &NSEvent) -> bool {
        let own: &NSView = self;
        let Some(window) = own.window() else {
            return false;
        };
        if event.windowNumber() != window.windowNumber() {
            return false;
        }
        let Some(content) = window.contentView() else {
            return false;
        };
        let at = event.locationInWindow();
        // SAFETY: `superview` у вида содержимого окна на главном потоке.
        let point = match unsafe { content.superview() } {
            Some(parent) => parent.convertPoint_fromView(at, None),
            None => at,
        };
        PROBING.set(true);
        let target = content.hitTest(point);
        PROBING.set(false);
        target.is_some_and(|target| !std::ptr::eq(&*target, own) && target.isDescendantOf(own))
    }
}

thread_local! {
    /// Идёт собственный поиск адресата нажатия: контейнеры в нём только отвечают.
    static PROBING: Cell<bool> = const { Cell::new(false) };
}

/// Номер и время события нажатия.
fn press_key(event: &NSEvent) -> (isize, u64) {
    (event.eventNumber(), event.timestamp().to_bits())
}

/// Что показывает нейтральная карточка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelCard {
    pub title: String,
    pub plugin: String,
    /// Зона или колонка навигатора.
    pub place: String,
    /// Класс размера тела.
    pub size: String,
}

impl PanelCard {
    /// Вторая строка: plugin id, место и класс размера.
    pub fn meta(&self) -> String {
        format!("{} · {} · {}", self.plugin, self.place, self.size)
    }
}

struct CardModel {
    id: ComponentId,
    theme: Cell<AppTheme>,
    card: RefCell<PanelCard>,
}

/// Нейтральная карточка на главном потоке: тело панели без собственного содержимого. В AX её нет:
/// имя панели несёт контейнер. Щелчок она не обрабатывает, он уходит контейнеру. Освобождение
/// убирает её из иерархии.
pub struct NeutralCard {
    view: View<CardView, CardModel>,
}

impl NeutralCard {
    pub fn new(app: &App, theme: &AppTheme, card: PanelCard) -> NeutralCard {
        let mtm = app.mtm();
        let model = CardModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
            card: RefCell::new(card),
        };
        let view = View::new(&CARD_OWNER, model, "NeutralCard.updateLayer", |owner| {
            CardView::new(mtm, owner)
        });
        let card = NeutralCard { view };
        card.show_card();
        card.apply_theme();
        card
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    pub fn card(&self) -> PanelCard {
        self.view.rust().card.borrow().clone()
    }

    pub fn set_card(&self, card: PanelCard) {
        let model = self.view.rust();
        if *model.card.borrow() != card {
            model.card.replace(card);
            self.show_card();
        }
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        let model = self.view.rust();
        if model.theme.get() != *theme {
            model.theme.set(*theme);
            self.apply_theme();
        }
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }

    /// Карточка для `ui.inspect`.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> InspectNode {
        inspect::node(id, "NeutralCard", parent, self.view.view(), None, None)
    }

    fn show_card(&self) {
        let card = self.view.rust().card.borrow();
        let parts = self.view.view().ivars();
        parts.title.setStringValue(&NSString::from_str(&card.title));
        parts.meta.setStringValue(&NSString::from_str(&card.meta()));
    }

    fn apply_theme(&self) {
        let view = self.view.view();
        let theme = self.view.rust().theme.get();
        let parts = view.ivars();
        parts
            .title
            .setTextColor(Some(&layer::ns_color(theme.colors.text)));
        parts
            .title
            .setFont(Some(&font(theme.metrics.font.body, true)));
        parts
            .meta
            .setTextColor(Some(&layer::ns_color(theme.colors.text_2)));
        parts
            .meta
            .setFont(Some(&font(theme.metrics.font.meta, false)));
        view.setNeedsDisplay(true);
    }
}

fn font(size: f64, medium: bool) -> Retained<NSFont> {
    // SAFETY: веса шрифта — неизменяемые константы AppKit.
    let weight = unsafe {
        if medium {
            NSFontWeightMedium
        } else {
            NSFontWeightRegular
        }
    };
    NSFont::systemFontOfSize_weight(size, weight)
}

fn label(mtm: MainThreadMarker) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    label.setAccessibilityElement(false);
    label
}

struct CardIvars {
    _live: Live,
    owner: Callback<CardModel>,
    title: Retained<NSTextField>,
    meta: Retained<NSTextField>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioNeutralCard"]
    #[ivars = CardIvars]
    struct CardView;

    impl CardView {
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
            let Some(theme) = self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                model.theme.get()
            }) else {
                return;
            };
            // Фон даёт контейнер панели: в плитке — фон панели, в колонке навигатора — фон окна.
            let _ = theme;
            if let Some(layer) = self.layer() {
                without_actions(|| layer.setBackgroundColor(None));
            }
        }

        // SAFETY: селектор и результат — как у `acceptsFirstMouse:` NSView: первый щелчок в
        // неактивном окне доходит до контейнера панели.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }
    }
);

impl CardView {
    fn new(mtm: MainThreadMarker, owner: Callback<CardModel>) -> Retained<CardView> {
        let this = CardView::alloc(mtm).set_ivars(CardIvars {
            _live: CARD_VIEW.live(),
            owner,
            title: label(mtm),
            meta: label(mtm),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<CardView> = unsafe {
            msg_send![super(this), initWithFrame: NSRect::new(NSPoint::ZERO, NSSize::new(2.0 * PAD, 3.0 * LINE))]
        };
        layered(&view);
        let parts = view.ivars();
        let width =
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin;
        for (index, text) in [&parts.title, &parts.meta].into_iter().enumerate() {
            text.setFrame(NSRect::new(
                NSPoint::new(PAD, PAD + LINE * index as f64),
                NSSize::new(0.0, LINE),
            ));
            text.setAutoresizingMask(width);
            view.addSubview(text);
        }
        view.setAccessibilityElement(false);
        view
    }
}

/// Подпись кнопки заглушки: действие `layout.zoom` (ADR-0032).
pub const EXPAND_TITLE: &str = "Развернуть";
/// Отступ содержимого заглушки от краёв тела (`.stub` прототипа).
const STUB_PAD: f64 = 12.0;
/// Промежуток между текстом и кнопкой заглушки (`.stub` прототипа).
const STUB_GAP: f64 = 8.0;
/// Высота, поля и рамка кнопки заглушки (`.btn { height: 26px; padding: 0 11px; border: 1px }`
/// прототипа).
const BUTTON_HEIGHT: f64 = 26.0;
const BUTTON_PAD: f64 = 11.0;
const BUTTON_BORDER: f64 = 1.0;

/// Что показывает заглушка на месте тела.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StubCard {
    /// Название панели под заглушкой.
    pub title: String,
    /// Кнопка «Развернуть» есть; у развёрнутой панели разворачивать некуда, у заглушки статуса
    /// кнопки нет.
    pub expandable: bool,
    /// Причина статуса панели: текст заглушки вместо «<название>: здесь тесно».
    pub status: Option<String>,
}

impl StubCard {
    /// Текст заглушки: причина статуса панели, иначе «<название>: здесь тесно» (`docs/ux.md`,
    /// прототип).
    pub fn text(&self) -> String {
        match &self.status {
            Some(status) => status.clone(),
            None => format!("{}: здесь тесно", self.title),
        }
    }
}

/// Намерение заглушки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StubIntent {
    /// Кнопка «Развернуть».
    Expand,
    /// Щелчок в тело заглушки мимо кнопки.
    Press,
}

/// Кадры содержимого заглушки в точках тела с началом сверху слева; `None` — части нет.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StubFrames {
    pub text: Option<Frame>,
    pub button: Option<Frame>,
}

/// Раскладка заглушки в теле `width`×`height`: текст над кнопкой по центру тела. Текст переносится
/// по словам: `text_height` — высота его набора в ширине тела без отступов, целое число строк
/// [`LINE`]. Кнопка важнее текста: в теле ниже одной строки с кнопкой текст уходит, строки, которым
/// нет места над кнопкой, обрезаются, а кнопка остаётся и не шире тела без отступов. Без кнопки —
/// только текст. `title_width` — ширина набора подписи кнопки: кнопка шире её на поля и рамку.
pub fn stub_layout(
    width: f64,
    height: f64,
    text_height: f64,
    title_width: f64,
    expandable: bool,
) -> StubFrames {
    let inner = (width - 2.0 * STUB_PAD).max(0.0);
    let under = if expandable {
        STUB_GAP + BUTTON_HEIGHT
    } else {
        0.0
    };
    let text = !expandable || LINE + under + 2.0 * STUB_PAD <= height;
    let room = ((height - 2.0 * STUB_PAD - under) / LINE).floor().max(1.0);
    let lines = (text_height / LINE).round().clamp(1.0, room);
    let text_height = lines * LINE;
    let column = if text {
        text_height + under
    } else {
        BUTTON_HEIGHT
    };
    let top = ((height - column) / 2.0).max(0.0).floor();
    let button_width = (title_width + 2.0 * (BUTTON_PAD + BUTTON_BORDER)).min(inner);
    StubFrames {
        text: text.then(|| Frame::new(STUB_PAD, top, inner, text_height)),
        button: expandable.then(|| {
            Frame::new(
                (width - button_width) / 2.0,
                if text {
                    top + text_height + STUB_GAP
                } else {
                    top
                },
                button_width,
                BUTTON_HEIGHT,
            )
        }),
    }
}

/// Место набора подписи высотой `title_height` в кнопке заглушки шириной `width`: по центру, не
/// шире кнопки без полей и рамки.
pub fn stub_button_label(width: f64, title_width: f64, title_height: f64) -> Frame {
    let room = (width - 2.0 * (BUTTON_PAD + BUTTON_BORDER)).max(0.0);
    let label = title_width.clamp(0.0, room);
    Frame::new(
        (width - label) / 2.0,
        (BUTTON_HEIGHT - title_height) / 2.0,
        label,
        title_height,
    )
}

/// На сколько поднять набор в строке высотой `line` над её низом, чтобы строка шрифта высотой
/// `natural` стояла по середине: AppKit ставит набор к низу строки, прототип — по середине.
fn line_lift(line: f64, natural: f64) -> f64 {
    ((line - natural) / 2.0).max(0.0)
}

/// Текст заглушки с переносом по словам: строки высотой [`LINE`] по центру, набор по середине
/// строки (`.stub { text-align: center }` и межстрочный интервал прототипа).
fn stub_text(text: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    let text = NSMutableAttributedString::from_nsstring(&NSString::from_str(text));
    let range = NSRange::new(0, text.length());
    let paragraph = NSMutableParagraphStyle::new();
    paragraph.setMinimumLineHeight(LINE);
    paragraph.setMaximumLineHeight(LINE);
    paragraph.setAlignment(NSTextAlignment::Center);
    paragraph.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
    let natural = NSLayoutManager::new().defaultLineHeightForFont(font);
    let lift = NSNumber::new_f64(line_lift(LINE, natural));
    // SAFETY: имена атрибутов — неизменяемые константы AppKit, значения — те, что они требуют:
    // стиль абзаца, шрифт, цвет и число; диапазон лежит в тексте.
    unsafe {
        text.addAttribute_value_range(NSParagraphStyleAttributeName, &paragraph, range);
        text.addAttribute_value_range(NSFontAttributeName, font, range);
        text.addAttribute_value_range(NSForegroundColorAttributeName, color, range);
        text.addAttribute_value_range(NSBaselineOffsetAttributeName, &lift, range);
    }
    Retained::into_super(text)
}

struct StubModel {
    id: ComponentId,
    theme: Cell<AppTheme>,
    card: RefCell<StubCard>,
    /// Заглушка стоит в плоской колонке навигатора, а не в плитке.
    flat: Cell<bool>,
    on_intent: Box<dyn Fn(StubIntent)>,
}

/// Заглушка «Развернуть» на главном потоке. Хост держит одну на место тела и переиспользует её;
/// освобождение убирает её из иерархии.
pub struct ExpandStub {
    view: View<StubView, StubModel>,
}

impl ExpandStub {
    /// Заглушка; `on_intent` получает кнопку и щелчок в тело на главном потоке изнутри AppKit и
    /// не должен синхронно менять заглушку.
    pub fn new(
        app: &App,
        theme: &AppTheme,
        card: StubCard,
        on_intent: impl Fn(StubIntent) + 'static,
    ) -> ExpandStub {
        let mtm = app.mtm();
        let model = StubModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
            card: RefCell::new(card),
            flat: Cell::new(false),
            on_intent: Box::new(on_intent),
        };
        let mut view = View::new(&STUB_OWNER, model, "ExpandStub.view", |owner| {
            StubView::new(mtm, owner)
        });
        let button = view.view().ivars().button.clone();
        let control: &NSControl = &button;
        view.bind_action(control, "ExpandStub.expand", |model| {
            (model.on_intent)(StubIntent::Expand);
        });
        let stub = ExpandStub { view };
        stub.show_card();
        stub.apply_theme();
        stub
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    pub fn card(&self) -> StubCard {
        self.view.rust().card.borrow().clone()
    }

    /// Клавиша `layout.zoom` из действующей таблицы keymap у кнопки «Развернуть»: подсказка и
    /// `accessibilityHelp`; `None` — «Клавиша не назначена».
    pub fn set_key(&self, key: Option<&str>) {
        self.view.view().ivars().button.set_key(key);
    }

    /// Плоская заглушка красится фоном окна, обычная — фоном панели.
    pub fn set_flat(&self, flat: bool) {
        let model = self.view.rust();
        if model.flat.get() != flat {
            model.flat.set(flat);
            self.apply_theme();
        }
    }

    pub fn set_card(&self, card: StubCard) {
        let model = self.view.rust();
        if *model.card.borrow() != card {
            model.card.replace(card);
            self.show_card();
        }
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        let model = self.view.rust();
        if model.theme.get() != *theme {
            model.theme.set(*theme);
            self.apply_theme();
        }
    }

    /// Прячет или показывает заглушку без неявных анимаций.
    pub fn set_hidden(&self, hidden: bool) {
        let view = self.view.view();
        if view.isHidden() != hidden {
            without_actions(|| view.setHidden(hidden));
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.view.view().isHidden()
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }

    /// Заглушка и её кнопка для `ui.inspect`: кнопка — `<id>.expand`, она есть только у заглушки
    /// с кнопкой.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        let view = self.view.view();
        let button = &view.ivars().button;
        let text = &view.ivars().text;
        let mut nodes = vec![inspect::node(id, "ExpandStub", parent, view, None, None)];
        if !text.isHidden() {
            nodes.push(inspect::typeset_node(
                &format!("{id}.label"),
                "StubLabel",
                id,
                text,
            ));
        }
        if !button.isHidden() {
            nodes.push(inspect::node(
                &format!("{id}.expand"),
                "ExpandButton",
                Some(id),
                button,
                Some(button.isEnabled()),
                None,
            ));
            // Подпись — поле внутри кнопки: кадр — место её набора.
            nodes.push(inspect::typeset_node(
                &format!("{id}.expand.label"),
                "Label",
                &format!("{id}.expand"),
                &button.ivars().label,
            ));
        }
        nodes
    }

    fn show_card(&self) {
        let view = self.view.view();
        let card = self.view.rust().card.borrow();
        let parts = view.ivars();
        view.setAccessibilityLabel(Some(&NSString::from_str(&card.text())));
        parts.button.setHidden(!card.expandable);
        drop(card);
        self.set_text();
        view.place();
    }

    /// Текст заглушки шрифтом и цветом темы: набор с переносом собирается заново при смене
    /// текста и темы.
    fn set_text(&self) {
        let model = self.view.rust();
        let theme = model.theme.get();
        let text = &self.view.view().ivars().text;
        let small = font(theme.metrics.font.small, false);
        let color = layer::ns_color(theme.colors.text_2);
        // Шрифт и цвет поля читает `ui.inspect`, набор несёт их в атрибутах.
        text.setFont(Some(&small));
        text.setTextColor(Some(&color));
        text.setAttributedStringValue(&stub_text(&model.card.borrow().text(), &small, &color));
    }

    fn apply_theme(&self) {
        let view = self.view.view();
        let theme = self.view.rust().theme.get();
        let parts = view.ivars();
        self.set_text();
        let button = &parts.button;
        let title = &button.ivars().label;
        title.setFont(Some(&font(theme.metrics.font.tab, false)));
        title.setTextColor(Some(&layer::ns_color(theme.colors.text)));
        // Кнопка берёт фон своей поверхности (`.btn` прототипа): в плитке — фон панели, в плоской
        // колонке — фон поля.
        let button_fill = if self.view.rust().flat.get() {
            theme.colors.surface
        } else {
            theme.colors.panel
        };
        if let Some(fill) = button.layer() {
            without_actions(|| {
                layer::set_fill(&fill, button_fill);
                layer::set_line(&fill, Some((theme.colors.line_2, theme.metrics.line)));
                layer::set_radius(&fill, theme.metrics.radius.control);
            });
        }
        view.place();
        view.setNeedsDisplay(true);
    }
}

struct StubIvars {
    _live: Live,
    owner: Callback<StubModel>,
    text: Retained<NSTextField>,
    button: Retained<StubButton>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioExpandStub"]
    #[ivars = StubIvars]
    struct StubView;

    impl StubView {
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
            let Some(theme) = self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                (model.theme.get(), model.flat.get())
            }) else {
                return;
            };
            let (theme, flat) = theme;
            let fill = if flat {
                theme.colors.chrome
            } else {
                theme.colors.panel
            };
            if let Some(layer) = self.layer() {
                without_actions(|| layer::set_fill(&layer, fill));
            }
        }

        // SAFETY: селектор и аргумент — как у `resizeSubviewsWithOldSize:` NSView: новый размер
        // тела от владельца ставит текст и кнопку на место без ожидания прохода раскладки.
        #[unsafe(method(resizeSubviewsWithOldSize:))]
        fn resize_subviews_with_old_size(&self, old_size: NSSize) {
            // SAFETY: `resizeSubviewsWithOldSize:` у NSView с тем же размером.
            let _: () = unsafe { msg_send![super(self), resizeSubviewsWithOldSize: old_size] };
            self.place();
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            self.ivars()
                .owner
                .call(|model| (model.on_intent)(StubIntent::Press));
        }

        // SAFETY: селектор и результат — как у `acceptsFirstMouse:` NSView.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }
    }
);

impl StubView {
    fn new(mtm: MainThreadMarker, owner: Callback<StubModel>) -> Retained<StubView> {
        let text = label(mtm);
        text.setAlignment(NSTextAlignment::Center);
        text.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        text.setUsesSingleLineMode(false);
        text.setMaximumNumberOfLines(0);
        let this = StubView::alloc(mtm).set_ivars(StubIvars {
            _live: STUB_VIEW.live(),
            owner,
            text,
            button: StubButton::new(mtm),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<StubView> = unsafe {
            msg_send![super(this), initWithFrame: NSRect::new(NSPoint::ZERO, NSSize::new(2.0 * STUB_PAD, 2.0 * STUB_PAD))]
        };
        layered(&view);
        let parts = view.ivars();
        view.addSubview(&parts.text);
        view.addSubview(&parts.button);
        view.setAccessibilityElement(true);
        // SAFETY: роль доступности — неизменяемая константа AppKit.
        view.setAccessibilityRole(Some(unsafe { NSAccessibilityGroupRole }));
        view
    }

    /// Текст и кнопка по [`stub_layout`] для текущего размера.
    fn place(&self) {
        let parts = self.ivars();
        let size = self.bounds().size;
        let expandable = !parts.button.isHidden();
        // Поле текста шире набора на поля ячейки: набор переносится в ширине тела без отступов.
        let field = (size.width - 2.0 * STUB_PAD).max(0.0) + 2.0 * style::LABEL_INSET;
        let text_height = style::wrapped_height(&parts.text, field);
        let title = parts.button.title_size();
        let frames = stub_layout(
            size.width,
            size.height,
            text_height,
            title.width,
            expandable,
        );
        parts.text.setHidden(frames.text.is_none());
        if let Some(frame) = frames.text {
            parts.text.setFrame(style::label_field_rect(self, frame));
        }
        if let Some(frame) = frames.button {
            // Кнопка встаёт на пиксели экрана: дробный край размывает её рамку.
            let aligned = self.backingAlignedRect_options(
                ns_rect(frame),
                NSAlignmentOptions::AlignAllEdgesNearest,
            );
            parts.button.setFrame(aligned);
            parts.button.place_label(title);
        }
    }
}

struct StubButtonIvars {
    _live: Live,
    /// Подпись кнопки: поле внутри неё, ячейка кнопки подписи не рисует.
    label: Retained<NSTextField>,
}

define_class!(
    // SAFETY: NSButton не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSButton, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioExpandButton"]
    #[ivars = StubButtonIvars]
    struct StubButton;

    impl StubButton {
        // SAFETY: селектор и результат — как у `acceptsFirstResponder` NSResponder: кнопка
        // получает фокус клавиатуры независимо от Full Keyboard Access.
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            self.isEnabled()
        }

        // SAFETY: селектор и результат — как у `canBecomeKeyView` NSView.
        #[unsafe(method(canBecomeKeyView))]
        fn can_become_key_view(&self) -> bool {
            self.isEnabled() && !self.isHiddenOrHasHiddenAncestor()
        }

        // SAFETY: селектор и результат — как у `mouseDownCanMoveWindow` NSView.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        // SAFETY: селектор, аргумент и результат — как у `hitTest:` NSView. Подпись внутри кнопки
        // нажатий не получает: адресат — вся кнопка.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            let frame = self.frame();
            let inside = point.x >= frame.origin.x
                && point.x < frame.origin.x + frame.size.width
                && point.y >= frame.origin.y
                && point.y < frame.origin.y + frame.size.height;
            if inside && !self.isHidden() {
                let view: &NSView = self;
                std::ptr::from_ref(view).cast_mut()
            } else {
                std::ptr::null_mut()
            }
        }
    }
);

impl StubButton {
    /// Кнопка «Развернуть»: подпись и AX-имя — действие, подсказка и `accessibilityHelp` — клавиша
    /// `layout.zoom`, без неё «Клавиша не назначена» (ADR-0032).
    fn new(mtm: MainThreadMarker) -> Retained<StubButton> {
        let title = NSString::from_str(EXPAND_TITLE);
        let label = label(mtm);
        label.setStringValue(&title);
        let this = StubButton::alloc(mtm).set_ivars(StubButtonIvars {
            _live: STUB_BUTTON.live(),
            label,
        });
        // SAFETY: назначенный инициализатор NSButton (через NSControl), ivars уже заданы.
        let button: Retained<StubButton> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        button.setBordered(false);
        button.setWantsLayer(true);
        button.setTitle(&NSString::from_str(""));
        button.addSubview(&button.ivars().label);
        button.setAccessibilityLabel(Some(&title));
        button.set_key(None);
        button
    }

    fn set_key(&self, key: Option<&str>) {
        self.setToolTip(Some(&NSString::from_str(&strip::hint(
            StripButton::Zoom,
            key,
        ))));
        self.setAccessibilityHelp(Some(&NSString::from_str(&strip::help(key))));
    }

    /// Размер набора подписи её шрифтом: ширина без полей ячейки, высота строки поля.
    fn title_size(&self) -> NSSize {
        let label = &self.ivars().label;
        let width = label.font().map_or(0.0, |font| {
            style::typeset_width(&label.stringValue().to_string(), &font)
        });
        NSSize::new(width, label.intrinsicContentSize().height)
    }

    /// Ставит поле подписи по [`stub_button_label`] для текущей ширины кнопки.
    fn place_label(&self, title: NSSize) {
        let frame = stub_button_label(self.bounds().size.width, title.width, title.height);
        self.ivars()
            .label
            .setFrame(style::label_field_rect(self, frame));
    }
}

struct NoteModel {
    id: ComponentId,
    theme: Cell<AppTheme>,
}

/// Сообщение на месте области зон. Освобождение убирает его из иерархии.
pub struct EmptyNote {
    view: View<NoteView, NoteModel>,
}

impl EmptyNote {
    pub fn new(app: &App, theme: &AppTheme, text: &str) -> EmptyNote {
        let mtm = app.mtm();
        let model = NoteModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
        };
        let view = View::new(&NOTE_OWNER, model, "EmptyNote.updateLayer", |owner| {
            NoteView::new(mtm, owner)
        });
        let note = EmptyNote { view };
        note.set_text(text);
        note.apply_theme();
        note
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    pub fn text(&self) -> String {
        self.view.view().ivars().text.stringValue().to_string()
    }

    pub fn set_text(&self, text: &str) {
        let view = self.view.view();
        let label = &view.ivars().text;
        let value = NSString::from_str(text);
        if *label.stringValue() != *value {
            label.setStringValue(&value);
            view.setAccessibilityLabel(Some(&value));
        }
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        let model = self.view.rust();
        if model.theme.get() != *theme {
            model.theme.set(*theme);
            self.apply_theme();
        }
    }

    /// Прячет или показывает сообщение без неявных анимаций.
    pub fn set_hidden(&self, hidden: bool) {
        let view = self.view.view();
        if view.isHidden() != hidden {
            without_actions(|| view.setHidden(hidden));
        }
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }

    /// Сообщение для `ui.inspect`.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> InspectNode {
        // Сообщение рисует вложенное поле: шрифт, цвет и обрезка — его, имя узла остаётся именем
        // вида для доступности.
        let view = self.view.view();
        let node = inspect::node(id, "EmptyNote", parent, view, None, None);
        let label = node.facts.label.clone();
        let mut node = node.with_text_of(&view.ivars().text);
        node.facts.label = label;
        node
    }

    fn apply_theme(&self) {
        let view = self.view.view();
        let theme = self.view.rust().theme.get();
        let label = &view.ivars().text;
        label.setTextColor(Some(&layer::ns_color(theme.colors.text_2)));
        label.setFont(Some(&font(theme.metrics.font.body, false)));
        view.setNeedsDisplay(true);
    }
}

struct NoteIvars {
    _live: Live,
    owner: Callback<NoteModel>,
    text: Retained<NSTextField>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioEmptyNote"]
    #[ivars = NoteIvars]
    struct NoteView;

    impl NoteView {
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
            let Some(theme) = self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                model.theme.get()
            }) else {
                return;
            };
            if let Some(layer) = self.layer() {
                without_actions(|| layer::set_fill(&layer, theme.colors.chrome));
            }
        }
    }
);

impl NoteView {
    fn new(mtm: MainThreadMarker, owner: Callback<NoteModel>) -> Retained<NoteView> {
        let text = label(mtm);
        text.setAlignment(NSTextAlignment::Center);
        let this = NoteView::alloc(mtm).set_ivars(NoteIvars {
            _live: NOTE_VIEW.live(),
            owner,
            text,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<NoteView> = unsafe {
            msg_send![super(this), initWithFrame: NSRect::new(NSPoint::ZERO, NSSize::new(0.0, 2.0 * LINE))]
        };
        layered(&view);
        let text = &view.ivars().text;
        text.setFrame(NSRect::new(
            NSPoint::new(0.0, LINE / 2.0),
            NSSize::new(0.0, LINE),
        ));
        text.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        view.addSubview(text);
        view.setAccessibilityElement(true);
        // SAFETY: роль доступности — неизменяемая константа AppKit.
        view.setAccessibilityRole(Some(unsafe { NSAccessibilityGroupRole }));
        view
    }
}

/// Вид со своим слоем, который перерисовывается только по `setNeedsDisplay`.
fn layered(view: &NSView) {
    view.setWantsLayer(true);
    view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_line_names_plugin_place_and_size() {
        let card = PanelCard {
            title: "Терминал".into(),
            plugin: "terminal".into(),
            place: "Центр".into(),
            size: "Regular".into(),
        };
        assert_eq!(card.meta(), "terminal · Центр · Regular");
    }

    #[test]
    fn stub_text_names_the_panel() {
        let card = StubCard {
            title: "Терминал".into(),
            expandable: true,
            status: None,
        };
        assert_eq!(card.text(), "Терминал: здесь тесно");
    }

    #[test]
    fn status_stub_shows_the_reason_instead_of_the_cramped_text() {
        let card = StubCard {
            title: "Демо: список".into(),
            expandable: false,
            status: Some("Состояние не прочитано".into()),
        };
        assert_eq!(card.text(), "Состояние не прочитано");
    }

    #[test]
    fn stub_centers_text_over_the_button() {
        let frames = stub_layout(180.0, 400.0, LINE, 60.0, true);
        let text = frames.text.unwrap();
        let button = frames.button.unwrap();
        assert_eq!(text, Frame::new(12.0, 174.0, 156.0, LINE));
        assert_eq!(
            button,
            Frame::new(48.0, 200.0, 84.0, BUTTON_HEIGHT),
            "кнопка шире подписи на поля 11 и рамку 1 с каждой стороны"
        );
        assert_eq!(button.y, text.y + LINE + STUB_GAP);
    }

    #[test]
    fn narrow_part_wraps_the_text_into_two_lines_as_the_prototype() {
        // Паспорт `layout-stub`: тело 135×364 с началом (517, 81), подпись (529, 228, 111×36),
        // кнопка (537,53125, 272, 93,9375×26), подпись кнопки (549,53125, …, 69,9375).
        let title = 69.9375;
        let frames = stub_layout(135.0, 364.0, 2.0 * LINE, title, true);
        let text = frames.text.unwrap();
        let button = frames.button.unwrap();
        assert_eq!(text, Frame::new(12.0, 147.0, 111.0, 36.0));
        assert_eq!(button, Frame::new(20.53125, 191.0, 93.9375, BUTTON_HEIGHT));
        assert_eq!(button.y, text.y + text.height + STUB_GAP);
        assert_eq!(button.width, title + 24.0);
        let label = stub_button_label(button.width, title, LINE);
        assert_eq!(label, Frame::new(12.0, 4.0, title, LINE));
        assert_eq!(
            stub_button_label(button.width, title, 15.0).y,
            5.5,
            "строка шрифта по середине кнопки"
        );
    }

    #[test]
    fn short_body_keeps_the_button_and_drops_the_text() {
        let room = LINE + STUB_GAP + BUTTON_HEIGHT + 2.0 * STUB_PAD;
        assert_eq!(room, 76.0, "порог прототипа");
        for text_height in [LINE, 2.0 * LINE] {
            let fits = stub_layout(180.0, room, text_height, 60.0, true);
            assert_eq!(
                fits.text.unwrap().height,
                LINE,
                "порог — одна строка, лишние обрезаются"
            );
            let short = stub_layout(180.0, room - 1.0, text_height, 60.0, true);
            assert_eq!(short.text, None, "в низком теле текст уходит");
            let button = short.button.unwrap();
            assert_eq!(button.y, ((room - 1.0 - BUTTON_HEIGHT) / 2.0).floor());
        }
    }

    #[test]
    fn tall_text_never_pushes_the_button_out_of_the_body() {
        let frames = stub_layout(60.0, 120.0, 8.0 * LINE, 60.0, true);
        let text = frames.text.unwrap();
        let button = frames.button.unwrap();
        assert_eq!(text.height, 3.0 * LINE, "строк столько, сколько есть места");
        assert!(text.y >= STUB_PAD);
        assert!(button.y + button.height <= 120.0 - STUB_PAD);
        let status = stub_layout(60.0, 50.0, 8.0 * LINE, 60.0, false);
        assert_eq!(status.text.unwrap().height, LINE);
    }

    #[test]
    fn narrow_body_clamps_the_button_and_zoomed_stub_has_none() {
        let narrow = stub_layout(80.0, 400.0, LINE, 60.0, true).button.unwrap();
        assert_eq!(narrow.width, 80.0 - 2.0 * STUB_PAD, "кнопка не шире тела");
        assert_eq!(narrow.x, STUB_PAD);
        let label = stub_button_label(narrow.width, 60.0, 15.0);
        assert_eq!(
            label.width,
            narrow.width - 2.0 * (BUTTON_PAD + BUTTON_BORDER),
            "подпись не шире кнопки без полей и рамки"
        );
        assert_eq!(label.x, BUTTON_PAD + BUTTON_BORDER);
        let zoomed = stub_layout(180.0, 60.0, LINE, 60.0, false);
        assert_eq!(zoomed.button, None);
        assert_eq!(zoomed.text.unwrap().y, 21.0, "без кнопки текст по центру");
    }

    #[test]
    fn stub_text_is_set_in_lines_of_the_prototype_height() {
        // SAFETY: вес шрифта — неизменяемая константа AppKit.
        let font = NSFont::systemFontOfSize_weight(12.0, unsafe { NSFontWeightRegular });
        let text = stub_text("Терминал: здесь тесно", &font, &NSColor::blackColor());
        assert_eq!(text.string().to_string(), "Терминал: здесь тесно");
        assert!((style::text_line_height(&text, &font) - LINE).abs() < f64::EPSILON);
        assert_eq!(line_lift(18.0, 15.0), 1.5);
        assert_eq!(
            line_lift(18.0, 20.0),
            0.0,
            "высокая строка шрифта не опускается"
        );
    }
}
