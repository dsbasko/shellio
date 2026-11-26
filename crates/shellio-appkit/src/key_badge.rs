//! Плашка клавиши — правило `kbd` прототипа: высота и наименьшая ширина 17, поля 4, линия 1,
//! радиус 4, моноширинный кегль 10,5. Один вид на поле поиска заголовка и подсказки строки
//! статуса.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSTextField, NSView, NSViewLayerContentsRedrawPolicy};
use objc2_foundation::{NSRect, NSString};
use shellio_theme::Colors;

use crate::chrome_bar::Frame;
use crate::font;
use crate::inspect::{self, InspectNode};
use crate::layer::{self, without_actions};
use crate::style;

/// Высота и наименьшая ширина плашки.
pub(crate) const KEY_SIZE: f64 = 17.0;
const KEY_PAD: f64 = 4.0;
const KEY_RADIUS: f64 = 4.0;
const KEY_FONT: f64 = 10.5;

/// Ширина плашки клавиши с набором подписи `text_width`.
pub(crate) fn key_width(text_width: f64) -> f64 {
    (text_width.max(0.0) + 2.0 * KEY_PAD + 2.0)
        .ceil()
        .max(KEY_SIZE)
}

/// Плашка с подписью внутри.
pub(crate) struct KeyBadge {
    plate: Retained<NSView>,
    label: Retained<NSTextField>,
}

impl KeyBadge {
    /// Плашка с подписью `text`; цвета и шрифт ставит [`KeyBadge::apply_theme`].
    pub(crate) fn new(mtm: MainThreadMarker, text: &str) -> Self {
        let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        let plate = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        plate.setWantsLayer(true);
        plate.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
        plate.addSubview(&label);
        Self { plate, label }
    }

    pub(crate) fn view(&self) -> &NSView {
        &self.plate
    }

    /// Подпись плашки.
    pub(crate) fn text(&self) -> String {
        self.label.stringValue().to_string()
    }

    /// Ширина плашки по набору подписи.
    pub(crate) fn width(&self) -> f64 {
        key_width(self.label.intrinsicContentSize().width)
    }

    /// Ставит плашку левым верхним углом в (`x`, `y`) вида-родителя, подпись — по её середине.
    pub(crate) fn place(&self, x: f64, y: f64) {
        let box_width = self.width();
        let natural = self.label.intrinsicContentSize();
        let width = natural.width + 2.0 * style::LABEL_INSET;
        self.plate
            .setFrame(Frame::new(x, y, box_width, KEY_SIZE).rect());
        self.label.setFrame(
            Frame::new(
                ((box_width - width) / 2.0).round(),
                ((KEY_SIZE - natural.height) / 2.0).round(),
                width,
                natural.height,
            )
            .rect(),
        );
    }

    pub(crate) fn apply_theme(&self, colors: &Colors) {
        self.label
            .setTextColor(Some(&layer::ns_color(colors.text_2)));
        self.label.setFont(Some(&font::mono(KEY_FONT)));
        if let Some(layer) = self.plate.layer() {
            without_actions(|| {
                layer::set_fill(&layer, colors.kbd);
                layer::set_line(&layer, Some((colors.line_2, 1.0)));
                layer::set_radius(&layer, KEY_RADIUS);
            });
        }
    }

    /// Узел плашки в `ui.inspect` с текстом подписи.
    pub(crate) fn node(&self, id: &str, parent: &str) -> InspectNode {
        inspect::node(id, "KeyBadge", Some(parent), &self.plate, None, None)
            .with_text_of(&self.label)
    }
}
