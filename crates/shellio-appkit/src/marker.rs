//! Маркер поколения галереи (план P1.3, «Матрица приёмки»): сетка 8×8 ячеек по 3 точки кодирует
//! 64-битное поколение отрисовки. Ячейка `#000000` — 0, `#FFFFFF` — 1, старший бит слева сверху,
//! построчно. Цвета фиксированы и в токены не входят: маркер — вход оракула снимков, а не часть
//! оформления. Отрисовка ставит отметку поколения, как у компонентов.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSColor, NSRectFill, NSResponder, NSView, NSViewLayerContentsRedrawPolicy};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize};

use crate::app::App;
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode};
use crate::own::{Callback, ComponentId, View};
use crate::runloop;

counter!(MARKER_OWNER, Owner, "GenerationMarker");
counter!(MARKER_VIEW, Class, "ShellioGenerationMarker");

/// Ячеек в стороне сетки и сторона ячейки в точках.
pub const MARKER_CELLS: u32 = 8;
pub const MARKER_CELL: f64 = 3.0;

/// Бит поколения в ячейке `row`, `col`: старший бит — `row` 0, `col` 0.
pub fn marker_bit(generation: u64, row: u32, col: u32) -> bool {
    let bit = 63 - (row * MARKER_CELLS + col);
    (generation >> bit) & 1 == 1
}

struct MarkerModel {
    id: ComponentId,
    generation: Cell<u64>,
}

/// Маркер поколения на главном потоке.
pub struct GenerationMarker {
    view: View<MarkerView, MarkerModel>,
}

impl GenerationMarker {
    pub fn new(app: &App, generation: u64) -> GenerationMarker {
        let mtm = app.mtm();
        let model = MarkerModel {
            id: ComponentId::next(),
            generation: Cell::new(generation),
        };
        let view = View::new(&MARKER_OWNER, model, "GenerationMarker.drawRect", |owner| {
            MarkerView::new(mtm, owner)
        });
        GenerationMarker { view }
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    pub fn generation(&self) -> u64 {
        self.view.rust().generation.get()
    }

    /// Новое поколение: маркер перерисовывается в том же проходе отображения, что и состояние.
    pub fn set_generation(&self, generation: u64) {
        let model = self.view.rust();
        if model.generation.replace(generation) != generation {
            self.view.view().setNeedsDisplay(true);
        }
    }

    pub fn inspect(&self, id: &'static str, parent: Option<&'static str>) -> Vec<InspectNode> {
        vec![inspect::node(
            id,
            "GenerationMarker",
            parent,
            self.view.view(),
            None,
            None,
        )]
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }
}

struct MarkerIvars {
    _live: Live,
    owner: Callback<MarkerModel>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioGenerationMarker"]
    #[ivars = MarkerIvars]
    struct MarkerView;

    impl MarkerView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `isOpaque` NSView: ячейки покрывают весь вид.
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            true
        }

        // SAFETY: селектор и аргумент — как у `drawRect:` NSView.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let Some(generation) = self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                model.generation.get()
            }) else {
                return;
            };
            let zero = NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 1.0);
            let one = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 1.0);
            for row in 0..MARKER_CELLS {
                for col in 0..MARKER_CELLS {
                    let color = if marker_bit(generation, row, col) { &one } else { &zero };
                    color.setFill();
                    let cell = NSRect::new(
                        NSPoint::new(f64::from(col) * MARKER_CELL, f64::from(row) * MARKER_CELL),
                        NSSize::new(MARKER_CELL, MARKER_CELL),
                    );
                    NSRectFill(cell);
                }
            }
        }
    }
);

impl MarkerView {
    fn new(mtm: MainThreadMarker, owner: Callback<MarkerModel>) -> Retained<MarkerView> {
        let this = MarkerView::alloc(mtm).set_ivars(MarkerIvars {
            _live: MARKER_VIEW.live(),
            owner,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<MarkerView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setWantsLayer(true);
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bits: impl Fn(u32, u32) -> bool) -> u64 {
        let mut value = 0_u64;
        for row in 0..MARKER_CELLS {
            for col in 0..MARKER_CELLS {
                value = (value << 1) | u64::from(bits(row, col));
            }
        }
        value
    }

    #[test]
    fn grid_encodes_every_bit_most_significant_first() {
        for generation in [0, 1, 5, 9, 255, (1 << 32) + 7, (1 << 40) | 0xABCD, u64::MAX] {
            assert_eq!(
                decode(|row, col| marker_bit(generation, row, col)),
                generation
            );
        }
        assert!(marker_bit(1 << 63, 0, 0));
        assert!(marker_bit(1, MARKER_CELLS - 1, MARKER_CELLS - 1));
        assert!(!marker_bit(1, 0, 0));
        assert!(marker_bit(1 << 56, 0, 7), "первая строка — старший байт");
    }
}
