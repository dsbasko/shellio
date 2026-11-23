//! Нейтральный вид панели до фабрик P1.5 ([`PanelView`]): название, plugin id, зона и класс размера
//! тела. Хост рисует его для любого plugin id; вид живёт столько же, сколько панель, и переносится
//! между плитками без пересоздания. [`EmptyNote`] — сообщение на месте области зон: «Нет открытых
//! панелей» или «Рабочая область не открыта».

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityGroupRole, NSAutoresizingMaskOptions, NSEvent, NSFont,
    NSFontWeightMedium, NSFontWeightRegular, NSLineBreakMode, NSResponder, NSTextAlignment,
    NSTextField, NSView, NSViewLayerContentsRedrawPolicy,
};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize, NSString};
use shellio_theme::AppTheme;

use crate::app::App;
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode};
use crate::layer::{self, without_actions};
use crate::own::{Callback, ComponentId, View};
use crate::runloop;

counter!(PANEL_OWNER, Owner, "PanelView");
counter!(PANEL_VIEW, Class, "ShellioPanelView");
counter!(NOTE_OWNER, Owner, "EmptyNote");
counter!(NOTE_VIEW, Class, "ShellioEmptyNote");

/// Отступ текста вида панели от краёв тела.
const PAD: f64 = 14.0;
/// Высота строки текста вида панели.
const LINE: f64 = 18.0;

/// Что показывает вид панели.
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

struct PanelModel {
    id: ComponentId,
    theme: Cell<AppTheme>,
    card: RefCell<PanelCard>,
    on_press: Box<dyn Fn()>,
}

/// Вид панели на главном потоке. Освобождение убирает его из иерархии.
pub struct PanelView {
    view: View<PanelStubView, PanelModel>,
}

impl PanelView {
    /// Вид панели; `on_press` получает щелчок в тело на главном потоке изнутри AppKit.
    pub fn new(
        app: &App,
        theme: &AppTheme,
        card: PanelCard,
        on_press: impl Fn() + 'static,
    ) -> PanelView {
        let mtm = app.mtm();
        let model = PanelModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
            card: RefCell::new(card),
            on_press: Box::new(on_press),
        };
        let view = View::new(&PANEL_OWNER, model, "PanelView.updateLayer", |owner| {
            PanelStubView::new(mtm, owner)
        });
        let panel = PanelView { view };
        panel.show_card();
        panel.apply_theme();
        panel
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

    /// Прячет или показывает вид без неявных анимаций.
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

    /// Вид для `ui.inspect`.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> InspectNode {
        inspect::node(id, "PanelView", parent, self.view.view(), None, None)
    }

    fn show_card(&self) {
        let view = self.view.view();
        let card = self.view.rust().card.borrow();
        let parts = view.ivars();
        parts.title.setStringValue(&NSString::from_str(&card.title));
        parts.meta.setStringValue(&NSString::from_str(&card.meta()));
        view.setAccessibilityLabel(Some(&NSString::from_str(&card.title)));
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

struct PanelIvars {
    _live: Live,
    owner: Callback<PanelModel>,
    title: Retained<NSTextField>,
    meta: Retained<NSTextField>,
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
            let Some(theme) = self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                model.theme.get()
            }) else {
                return;
            };
            if let Some(layer) = self.layer() {
                without_actions(|| layer::set_fill(&layer, theme.colors.panel));
            }
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            self.ivars().owner.call(|model| (model.on_press)());
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
            title: label(mtm),
            meta: label(mtm),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<PanelStubView> = unsafe {
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
        view.setAccessibilityElement(true);
        // SAFETY: роль доступности — неизменяемая константа AppKit.
        view.setAccessibilityRole(Some(unsafe { NSAccessibilityGroupRole }));
        view
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
        inspect::node(id, "EmptyNote", parent, self.view.view(), None, None)
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
}
