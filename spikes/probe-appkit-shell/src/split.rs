//! Деление окна на левую, центральную и правую зоны через `NSSplitViewController`. Вариант
//! `sidebar` строит левую и правую зоны элементами сайдбара и инспектора, вариант `plain` —
//! обычными элементами. Всё остальное у вариантов одинаково: толщины, делитель, приоритеты
//! удержания, свёртка и содержимое. Место под делитель AppKit на macOS 27 оставляет только
//! обычным элементам, и у свёрнутой зоны тоже; рядом с элементами сайдбара и инспектора его
//! нет, поэтому зазоры у центра в варианте `sidebar` рисует хост зоны (`zone_sides`).

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSColor, NSResponder, NSSplitView, NSSplitViewController,
    NSSplitViewItem, NSSplitViewItemCollapseBehavior, NSView, NSViewController,
};
use objc2_foundation::{NSObject, NSRect};

use crate::args::Split;
use crate::chrome::color;
use crate::scenario::counters::{Live, counter};
use crate::tokens::Tokens;

counter!(SPLIT_VIEW, "ProbeSplitView");

/// Ширина делителя равна зазору `--gap` между плитками.
pub const DIVIDER: f64 = 6.0;
/// Навигатор; вместе с зазором перед центром — плоская левая зона 236 точек.
pub const LEFT_THICKNESS: f64 = 230.0;
/// Правая зона без зазора перед ней.
pub const RIGHT_THICKNESS: f64 = 366.0;
const CENTER_MIN: f64 = 300.0;
const SIDE_HOLDING: f32 = 260.0;
const CENTER_HOLDING: f32 = 250.0;

pub struct SplitIvars {
    _live: Live,
    divider: RefCell<Retained<NSColor>>,
}

define_class!(
    /// Сплит с делителем ширины зазора цвета окна.
    #[unsafe(super(NSSplitView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeSplitView"]
    #[ivars = SplitIvars]
    pub struct ProbeSplitView;

    impl ProbeSplitView {
        #[unsafe(method(dividerThickness))]
        fn divider_thickness(&self) -> f64 {
            DIVIDER
        }

        #[unsafe(method_id(dividerColor))]
        fn divider_color(&self) -> Retained<NSColor> {
            self.ivars().divider.borrow().clone()
        }
    }
);

impl ProbeSplitView {
    fn new(mtm: MainThreadMarker, frame: NSRect, tokens: &Tokens) -> Retained<ProbeSplitView> {
        let this = ProbeSplitView::alloc(mtm).set_ivars(SplitIvars {
            _live: SPLIT_VIEW.live(),
            divider: RefCell::new(color(tokens.chrome, 1.0)),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Делитель цвета окна `--chrome`.
    pub fn set_tokens(&self, tokens: &Tokens) {
        *self.ivars().divider.borrow_mut() = color(tokens.chrome, 1.0);
        self.setNeedsDisplay(true);
    }
}

#[derive(Clone, Copy)]
enum Side {
    Leading,
    Center,
    Trailing,
}

/// Сплит сцены. Контроллер держит элементы, их контроллеры видов и сам сплит.
pub struct SplitScene {
    pub variant: Split,
    pub controller: Retained<NSSplitViewController>,
    pub split_view: Retained<ProbeSplitView>,
    pub left: Retained<NSSplitViewItem>,
    pub center: Retained<NSSplitViewItem>,
    pub right: Retained<NSSplitViewItem>,
}

impl SplitScene {
    pub fn new(
        mtm: MainThreadMarker,
        variant: Split,
        frame: NSRect,
        tokens: &Tokens,
        zones: [&NSView; 3],
    ) -> SplitScene {
        let split_view = ProbeSplitView::new(mtm, frame, tokens);
        split_view.setVertical(true);
        let controller = NSSplitViewController::new(mtm);
        controller.setSplitView(&split_view);
        let [left, center, right] = [
            (Side::Leading, zones[0]),
            (Side::Center, zones[1]),
            (Side::Trailing, zones[2]),
        ]
        .map(|(side, view)| {
            let item = item(mtm, variant, side, view);
            configure(&item, side);
            controller.addSplitViewItem(&item);
            item
        });
        let host = controller.view();
        host.setFrame(frame);
        host.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        SplitScene {
            variant,
            controller,
            split_view,
            left,
            center,
            right,
        }
    }

    pub fn set_tokens(&self, tokens: &Tokens) {
        self.split_view.set_tokens(tokens);
    }

    /// Настройки элементов и делителя, как их видит AppKit.
    pub fn describe(&self) -> Vec<ItemInfo> {
        [
            ("left", &self.left),
            ("center", &self.center),
            ("right", &self.right),
        ]
        .into_iter()
        .map(|(name, item)| ItemInfo {
            name,
            behavior: item.behavior().0,
            minimum_thickness: item.minimumThickness(),
            maximum_thickness: item.maximumThickness(),
            holding_priority: item.holdingPriority(),
            can_collapse: item.canCollapse(),
            collapse_behavior: item.collapseBehavior().0,
            allows_full_height_layout: item.allowsFullHeightLayout(),
            divider_thickness: self.split_view.dividerThickness(),
        })
        .collect()
    }

    /// Вид контроллера, который кладётся в окно.
    pub fn view(&self) -> Retained<NSView> {
        self.controller.view()
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ItemInfo {
    pub name: &'static str,
    /// `NSSplitViewItemBehavior`: 0 — обычный, 1 — сайдбар, 3 — инспектор.
    pub behavior: isize,
    pub minimum_thickness: f64,
    pub maximum_thickness: f64,
    pub holding_priority: f32,
    pub can_collapse: bool,
    pub collapse_behavior: isize,
    pub allows_full_height_layout: bool,
    pub divider_thickness: f64,
}

/// Зазоры слева и справа у хостов центра и правой зоны. Правой зоне зазор нужен только у края
/// окна. Центру в варианте `plain` зазоры дают делители, которые остаются и у свёрнутой зоны.
pub fn zone_sides(variant: Split, gap: f64) -> [(f64, f64); 2] {
    let right = (0.0, gap);
    match variant {
        Split::Plain => [(0.0, 0.0), right],
        Split::Sidebar => [(gap, gap), right],
    }
}

/// Единственное различие вариантов — фабрика элемента.
fn item(
    mtm: MainThreadMarker,
    variant: Split,
    side: Side,
    view: &NSView,
) -> Retained<NSSplitViewItem> {
    let controller = NSViewController::new(mtm);
    controller.setView(view);
    match (variant, side) {
        (Split::Sidebar, Side::Leading) => NSSplitViewItem::sidebarWithViewController(&controller),
        (Split::Sidebar, Side::Trailing) => {
            NSSplitViewItem::inspectorWithViewController(&controller)
        }
        (Split::Sidebar, Side::Center) | (Split::Plain, _) => {
            NSSplitViewItem::splitViewItemWithViewController(&controller)
        }
    }
}

/// Одинаковые для обоих вариантов толщины, приоритеты и свёртка.
fn configure(item: &NSSplitViewItem, side: Side) {
    let (min, max, holding, collapsible) = match side {
        Side::Leading => (LEFT_THICKNESS, LEFT_THICKNESS, SIDE_HOLDING, true),
        Side::Center => (CENTER_MIN, f64::MAX, CENTER_HOLDING, false),
        Side::Trailing => (RIGHT_THICKNESS, RIGHT_THICKNESS, SIDE_HOLDING, true),
    };
    item.setMinimumThickness(min);
    if max < f64::MAX {
        item.setMaximumThickness(max);
    }
    item.setHoldingPriority(holding);
    item.setCanCollapse(collapsible);
    item.setCanCollapseFromWindowResize(false);
    item.setCollapseBehavior(
        NSSplitViewItemCollapseBehavior::PreferResizingSiblingsWithFixedSplitView,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_center_draws_gaps_that_plain_dividers_give() {
        assert_eq!(zone_sides(Split::Plain, DIVIDER), [(0.0, 0.0), (0.0, 6.0)]);
        assert_eq!(
            zone_sides(Split::Sidebar, DIVIDER),
            [(6.0, 6.0), (0.0, 6.0)]
        );
    }
}
