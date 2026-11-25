//! Строка статуса — нижняя строка окна со слотом сообщения (ADR-0024). Отказ команды раскладки из
//! интерфейса показывается в ней текстом ADR-0031, следующее действие пользователя его убирает.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityStaticTextRole, NSAutoresizingMaskOptions, NSFont,
    NSFontWeightRegular, NSLineBreakMode, NSResponder, NSTextField, NSView,
    NSViewLayerContentsRedrawPolicy,
};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CALayer;
use shellio_theme::AppTheme;

use crate::app::App;
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode};
use crate::layer::{self, without_actions};
use crate::own::{Callback, ComponentId, View};
use crate::runloop;

counter!(STATUS_OWNER, Owner, "StatusLine");
counter!(STATUS_VIEW, Class, "ShellioStatusLine");

/// Отступ текста от левого края строки.
const PAD: f64 = 12.0;
/// Высота строки текста.
const LINE: f64 = 16.0;

struct StatusModel {
    id: ComponentId,
    theme: Cell<AppTheme>,
}

/// Строка статуса на главном потоке. Освобождение убирает её из иерархии.
pub struct StatusLine {
    view: View<StatusView, StatusModel>,
}

impl StatusLine {
    pub fn new(app: &App, theme: &AppTheme) -> StatusLine {
        let mtm = app.mtm();
        let model = StatusModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
        };
        let view = View::new(&STATUS_OWNER, model, "StatusLine.updateLayer", |owner| {
            StatusView::new(mtm, owner)
        });
        let line = StatusLine { view };
        line.apply_theme();
        line
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    /// Текст слота сообщения; пустая строка — сообщения нет.
    pub fn message(&self) -> String {
        self.view.view().ivars().text.stringValue().to_string()
    }

    /// Сообщение в слоте; `None` убирает его.
    pub fn set_message(&self, message: Option<&str>) {
        let view = self.view.view();
        let label = &view.ivars().text;
        let value = NSString::from_str(message.unwrap_or(""));
        if *label.stringValue() != *value {
            label.setStringValue(&value);
            label.setAccessibilityLabel(Some(&value));
            label.setHidden(message.is_none());
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

    /// Строка и слот сообщения для `ui.inspect`: слот — `<id>.message`, его подпись — текст.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        let view = self.view.view();
        vec![
            inspect::node(id, "StatusLine", parent, view, None, None),
            inspect::node(
                &format!("{id}.message"),
                "StatusMessage",
                Some(id),
                &view.ivars().text,
                None,
                None,
            ),
        ]
    }

    fn apply_theme(&self) {
        let view = self.view.view();
        let theme = self.view.rust().theme.get();
        let label = &view.ivars().text;
        label.setTextColor(Some(&layer::ns_color(theme.colors.text_2)));
        // SAFETY: вес шрифта — неизменяемая константа AppKit.
        let weight = unsafe { NSFontWeightRegular };
        label.setFont(Some(&NSFont::systemFontOfSize_weight(
            theme.metrics.font.small,
            weight,
        )));
        view.setNeedsDisplay(true);
    }
}

struct StatusIvars {
    _live: Live,
    owner: Callback<StatusModel>,
    text: Retained<NSTextField>,
    /// Линия над строкой (`.status { border-top }` прототипа).
    line: Retained<CALayer>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioStatusLine"]
    #[ivars = StatusIvars]
    struct StatusView;

    impl StatusView {
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
                without_actions(|| {
                    layer::set_fill(&layer, theme.colors.chrome);
                    layer::set_fill(&self.ivars().line, theme.colors.line);
                });
            }
        }

        // SAFETY: селектор и результат — как у `layout` NSView.
        #[unsafe(method(layout))]
        fn layout(&self) {
            // SAFETY: `layout` у NSView без аргументов.
            let _: () = unsafe { msg_send![super(self), layout] };
            let (width, line) = self.ivars().owner.call(|model| {
                (self.bounds().size.width, model.theme.get().metrics.line)
            }).unwrap_or((0.0, 0.0));
            without_actions(|| {
                self.ivars().line.setFrame(NSRect::new(
                    NSPoint::ZERO,
                    NSSize::new(width, line),
                ));
            });
        }
    }
);

impl StatusView {
    fn new(mtm: MainThreadMarker, owner: Callback<StatusModel>) -> Retained<StatusView> {
        let text = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        text.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        text.setHidden(true);
        // SAFETY: роль доступности — неизменяемая константа AppKit.
        text.setAccessibilityRole(Some(unsafe { NSAccessibilityStaticTextRole }));
        let this = StatusView::alloc(mtm).set_ivars(StatusIvars {
            _live: STATUS_VIEW.live(),
            owner,
            text,
            line: CALayer::new(),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<StatusView> = unsafe {
            msg_send![super(this), initWithFrame: NSRect::new(NSPoint::ZERO, NSSize::new(2.0 * PAD, 2.0 * LINE))]
        };
        view.setWantsLayer(true);
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
        if let Some(layer) = view.layer() {
            layer.addSublayer(&view.ivars().line);
        }
        let text = &view.ivars().text;
        text.setFrame(NSRect::new(
            NSPoint::new(PAD, LINE / 2.0),
            NSSize::new(0.0, LINE),
        ));
        text.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        view.addSubview(text);
        view
    }
}
