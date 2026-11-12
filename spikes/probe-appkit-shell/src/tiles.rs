//! Плитки центральной, правой и нижней зон и плоский навигатор слева. Плитка — линия 1 точка,
//! радиус `--r-plate`, `TabStrip` высоты `--tabbar` внутри; между плитками и у краёв окна зазор
//! `--gap`, его дают делители сплита или зазоры хостов зон. Плитка с фокусом получает тон
//! режима, остальные — вуаль `--dim` внутри своих границ. Навигатор лежит
//! на фоне окна, вуали и тона не имеет; при фокусе в нём все плитки под вуалью, а выбранная
//! строка в рамке. Вуаль не принимает щелчок и не элемент доступности. Клавиши вкладок плитка с
//! фокусом передаёт своей полосе. В центральной и правой плитках под полосой `VirtualList`:
//! щелчок по строке делает first responder его таблицу, и фокус переходит плитке.

use std::cell::Cell;
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSEvent, NSResponder, NSView};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize};

use crate::chrome::{color, fill, label, rect};
use crate::fixtures::{self, SCENE_ROWS, TabSpec};
use crate::list::{Hooks, VirtualList};
use crate::scenario::counters::{Live, counter};
use crate::tabs::{Place, TabStrip};
use crate::tokens::Tokens;

counter!(TILE, "ProbeTile");
counter!(VEIL, "ProbeVeil");
counter!(NAVIGATOR, "ProbeNavigator");
counter!(ZONE_HOST, "ProbeZoneHost");

/// Высота нижней зоны.
pub const BOTTOM_HEIGHT: f64 = 180.0;
/// Отступ содержимого навигатора слева и сверху, как у `.side` прототипа.
pub const NAV_PAD_LEFT: f64 = 8.0;
pub const NAV_PAD_TOP: f64 = 6.0;
const NAV_ROWS: [&str; 6] = [
    "shellio",
    "probe-appkit-shell",
    "main",
    "Сессии",
    "terminal · zsh",
    "agent · claude",
];
/// Выбранная строка навигатора.
pub const NAV_SELECTED: usize = 2;
const TITLE_INSET: f64 = 12.0;

pub const CENTER: usize = 0;
pub const RIGHT: usize = 1;
pub const BOTTOM: usize = 2;
/// Вкладки плиток сцены по индексам `CENTER`, `RIGHT`, `BOTTOM`.
pub const SCENE_TABS: [usize; 3] = [8, 2, 1];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Navigator,
    Tile(usize),
}

/// Плитки, навигатор и хосты зон сцены; держит фокус.
pub struct Tiles {
    pub navigator: Retained<Navigator>,
    /// По индексам `CENTER`, `RIGHT`, `BOTTOM`.
    pub tiles: Vec<Retained<Tile>>,
    /// Хост центральной колонки (центр и низ) и хост правой зоны.
    pub center_host: Retained<ZoneHost>,
    pub right_host: Retained<ZoneHost>,
    focus: Cell<Focus>,
    tokens: Tokens,
}

impl Tiles {
    /// `sides` — зазоры слева и справа у хостов центра и правой зоны.
    pub fn new(mtm: MainThreadMarker, tokens: Tokens, sides: [(f64, f64); 2]) -> Rc<Tiles> {
        let tiles = Rc::new_cyclic(|owner: &Weak<Tiles>| {
            let tile = |index: usize, title: &str, place: Place| {
                let tabs = fixtures::tabs(SCENE_TABS[index]);
                Tile::new(mtm, index, title, place, tabs, owner, &tokens)
            };
            let center = tile(CENTER, "Терминал", Place::Center);
            let right = tile(RIGHT, "Изменения", Place::Side);
            let bottom = tile(BOTTOM, "Задачи", Place::Bottom);
            let center_host = ZoneHost::new(
                mtm,
                &tokens,
                vec![center.clone(), bottom.clone()],
                Some(BOTTOM_HEIGHT),
                sides[0],
            );
            let right_host = ZoneHost::new(mtm, &tokens, vec![right.clone()], None, sides[1]);
            Tiles {
                navigator: Navigator::new(mtm, owner, &tokens),
                tiles: vec![center, right, bottom],
                center_host,
                right_host,
                focus: Cell::new(Focus::Tile(CENTER)),
                tokens,
            }
        });
        tiles.apply(Focus::Tile(CENTER));
        tiles
    }

    pub fn focus(&self) -> Focus {
        self.focus.get()
    }

    /// Тон у плитки с фокусом, вуаль у остальных, рамка у выбранной строки навигатора.
    fn apply(&self, focus: Focus) {
        self.focus.set(focus);
        for (index, tile) in self.tiles.iter().enumerate() {
            let focused = focus == Focus::Tile(index);
            tile.paint_overlay(focused, &self.tokens);
            tile.strip().set_focused(focused);
        }
        self.navigator
            .paint_selection(focus == Focus::Navigator, &self.tokens);
    }
}

pub struct VeilIvars {
    _live: Live,
}

define_class!(
    /// Слой над содержимым плитки: вуаль или тон режима. Щелчок проходит сквозь него.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeVeil"]
    #[ivars = VeilIvars]
    pub struct Veil;

    impl Veil {
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> Option<Retained<NSView>> {
            None
        }

        #[unsafe(method(isAccessibilityElement))]
        fn is_accessibility_element(&self) -> bool {
            false
        }
    }
);

impl Veil {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Veil> {
        let this = Veil::alloc(mtm).set_ivars(VeilIvars { _live: VEIL.live() });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let view: Retained<Veil> = unsafe { msg_send![super(this), initWithFrame: frame] };
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        view.setWantsLayer(true);
        view
    }
}

pub struct TileIvars {
    _live: Live,
    index: usize,
    owner: Weak<Tiles>,
    strip: Retained<TabStrip>,
    body: Retained<NSView>,
    /// Список центральной и правой плиток; таблица — first responder плитки.
    list: Option<VirtualList>,
    veil: Retained<Veil>,
}

define_class!(
    /// Плитка зоны. Щелчок по ней переводит фокус на неё, клавиши вкладок уходят её полосе.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeTile"]
    #[ivars = TileIvars]
    pub struct Tile;

    impl Tile {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !self.ivars().strip.handle_key(event) {
                // SAFETY: `keyDown:` у NSResponder с событием клавиши.
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
            }
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: `becomeFirstResponder` у NSResponder без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if accepted && let Some(owner) = self.ivars().owner.upgrade() {
                owner.apply(Focus::Tile(self.ivars().index));
            }
            accepted
        }
    }
);

impl Tile {
    fn new(
        mtm: MainThreadMarker,
        index: usize,
        title: &str,
        place: Place,
        tabs: Vec<TabSpec>,
        owner: &Weak<Tiles>,
        tokens: &Tokens,
    ) -> Retained<Tile> {
        let frame = rect(0.0, 0.0, 400.0, 300.0);
        let width = frame.size.width;
        let strip = TabStrip::new(mtm, place, title, tokens, width);
        strip.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        strip.set_tabs(tabs);

        let body = NSView::initWithFrame(
            NSView::alloc(mtm),
            rect(0.0, tokens.tabbar, width, frame.size.height - tokens.tabbar),
        );
        body.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        if place == Place::Bottom {
            // Заглушка индикатора работы; движение появляется в задаче 10.
            let dot = NSView::initWithFrame(NSView::alloc(mtm), rect(TITLE_INSET, 16.0, 8.0, 8.0));
            fill(&dot, tokens.run, 1.0);
            if let Some(layer) = dot.layer() {
                layer.setCornerRadius(4.0);
            }
            body.addSubview(&dot);
            body.addSubview(&label(
                mtm,
                "Работа идёт",
                tokens.text_2,
                rect(TITLE_INSET + 16.0, 11.0, 200.0, 18.0),
            ));
        }

        let list = (place != Place::Bottom).then(|| {
            let list = VirtualList::new(mtm, tokens, body.bounds(), fixtures::list(SCENE_ROWS));
            let focus_owner = owner.clone();
            let key_strip = strip.clone();
            list.set_hooks(Hooks {
                on_focus: Box::new(move || {
                    if let Some(owner) = focus_owner.upgrade() {
                        owner.apply(Focus::Tile(index));
                    }
                }),
                on_key: Box::new(move |event| key_strip.handle_key(event)),
            });
            body.addSubview(list.view());
            list
        });

        let veil = Veil::new(mtm, NSRect::new(NSPoint::ZERO, frame.size));
        let this = Tile::alloc(mtm).set_ivars(TileIvars {
            _live: TILE.live(),
            index,
            owner: owner.clone(),
            strip,
            body,
            list,
            veil,
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let tile: Retained<Tile> = unsafe { msg_send![super(this), initWithFrame: frame] };
        fill(&tile, tokens.panel, 1.0);
        if let Some(layer) = tile.layer() {
            layer.setBorderWidth(1.0);
            layer.setBorderColor(Some(&color(tokens.line, 1.0).CGColor()));
            layer.setCornerRadius(tokens.r_plate);
            layer.setMasksToBounds(true);
        }
        let ivars = tile.ivars();
        tile.addSubview(&ivars.strip);
        tile.addSubview(&ivars.body);
        tile.addSubview(&ivars.veil);
        ivars.strip.set_focus_target(&tile);
        tile
    }

    pub fn index(&self) -> usize {
        self.ivars().index
    }

    pub fn head(&self) -> &NSView {
        &self.ivars().strip
    }

    pub fn strip(&self) -> &TabStrip {
        &self.ivars().strip
    }

    pub fn body(&self) -> &NSView {
        &self.ivars().body
    }

    pub fn veil(&self) -> &Veil {
        &self.ivars().veil
    }

    pub fn list(&self) -> Option<&VirtualList> {
        self.ivars().list.as_ref()
    }

    fn paint_overlay(&self, focused: bool, tokens: &Tokens) {
        let tint = if focused { tokens.tone() } else { tokens.dim };
        if let Some(layer) = self.ivars().veil.layer() {
            layer.setBackgroundColor(Some(&color(tint.color, tint.alpha).CGColor()));
        }
    }
}

pub struct NavigatorIvars {
    _live: Live,
    owner: Weak<Tiles>,
    rows: Vec<Retained<NSView>>,
}

define_class!(
    /// Плоский навигатор на фоне окна. Строки — заглушки до `TreeList` задачи 8.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeNavigator"]
    #[ivars = NavigatorIvars]
    pub struct Navigator;

    impl Navigator {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: `becomeFirstResponder` у NSResponder без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if accepted && let Some(owner) = self.ivars().owner.upgrade() {
                owner.apply(Focus::Navigator);
            }
            accepted
        }
    }
);

impl Navigator {
    fn new(mtm: MainThreadMarker, owner: &Weak<Tiles>, tokens: &Tokens) -> Retained<Navigator> {
        let width = crate::split::LEFT_THICKNESS;
        let rows: Vec<Retained<NSView>> = NAV_ROWS
            .iter()
            .enumerate()
            .map(|(index, title)| {
                let row = NSView::initWithFrame(
                    NSView::alloc(mtm),
                    rect(
                        NAV_PAD_LEFT,
                        NAV_PAD_TOP + tokens.row * index as f64,
                        width - NAV_PAD_LEFT,
                        tokens.row,
                    ),
                );
                row.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
                row.setWantsLayer(true);
                if let Some(layer) = row.layer() {
                    layer.setCornerRadius(tokens.r_row);
                    if index == NAV_SELECTED {
                        layer.setBackgroundColor(Some(&color(tokens.sel, 1.0).CGColor()));
                    }
                }
                row.addSubview(&label(
                    mtm,
                    title,
                    if index == NAV_SELECTED {
                        tokens.text
                    } else {
                        tokens.text_2
                    },
                    rect(8.0, (tokens.row - 18.0) / 2.0, width - 24.0, 18.0),
                ));
                row
            })
            .collect();
        let this = Navigator::alloc(mtm).set_ivars(NavigatorIvars {
            _live: NAVIGATOR.live(),
            owner: owner.clone(),
            rows,
        });
        let frame = NSRect::new(NSPoint::ZERO, NSSize::new(width, 600.0));
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let view: Retained<Navigator> = unsafe { msg_send![super(this), initWithFrame: frame] };
        fill(&view, tokens.chrome, 1.0);
        for row in &view.ivars().rows {
            view.addSubview(row);
        }
        view
    }

    pub fn selected_row(&self) -> Option<&NSView> {
        self.ivars().rows.get(NAV_SELECTED).map(|row| &**row)
    }

    fn paint_selection(&self, focused: bool, tokens: &Tokens) {
        let Some(layer) = self.selected_row().and_then(|row| row.layer()) else {
            return;
        };
        if focused {
            layer.setBorderWidth(1.0);
            layer.setBorderColor(Some(&color(tokens.focus, 1.0).CGColor()));
        } else {
            layer.setBorderWidth(0.0);
            layer.setBorderColor(None);
        }
    }
}

pub struct HostIvars {
    _live: Live,
    tiles: Vec<Retained<Tile>>,
    bottom: Option<f64>,
    gap: f64,
    /// Зазоры слева и справа.
    sides: (f64, f64),
}

define_class!(
    /// Вид элемента сплита с плитками зоны на непрозрачном фоне окна и зазорами по краям.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeZoneHost"]
    #[ivars = HostIvars]
    pub struct ZoneHost;

    impl ZoneHost {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(resizeSubviewsWithOldSize:))]
        fn resize_subviews(&self, _old: NSSize) {
            self.place();
        }

        #[unsafe(method(layout))]
        fn layout(&self) {
            // SAFETY: `layout` у NSView без аргументов.
            let _: () = unsafe { msg_send![super(self), layout] };
            self.place();
        }
    }
);

impl ZoneHost {
    fn new(
        mtm: MainThreadMarker,
        tokens: &Tokens,
        tiles: Vec<Retained<Tile>>,
        bottom: Option<f64>,
        sides: (f64, f64),
    ) -> Retained<ZoneHost> {
        let this = ZoneHost::alloc(mtm).set_ivars(HostIvars {
            _live: ZONE_HOST.live(),
            tiles,
            bottom,
            gap: tokens.gap,
            sides,
        });
        let frame = rect(0.0, 0.0, 600.0, 600.0);
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let view: Retained<ZoneHost> = unsafe { msg_send![super(this), initWithFrame: frame] };
        fill(&view, tokens.chrome, 1.0);
        for tile in &view.ivars().tiles {
            view.addSubview(tile);
        }
        view.place();
        view
    }

    fn place(&self) {
        let ivars = self.ivars();
        let frames = zone_frames(self.bounds().size, ivars.sides, ivars.gap, ivars.bottom);
        for (tile, rect) in ivars.tiles.iter().zip(frames) {
            if tile.frame() != rect {
                tile.setFrame(rect);
            }
        }
    }
}

/// Кадры плиток в хосте размера `size`: сверху и снизу зазор, по бокам зазоры `sides`.
/// Координаты сверху вниз. С `bottom` первая плитка — верхняя, вторая — нижняя высоты `bottom`.
pub fn zone_frames(size: NSSize, sides: (f64, f64), gap: f64, bottom: Option<f64>) -> Vec<NSRect> {
    let (left, right) = sides;
    let width = (size.width - left - right).max(0.0);
    let height = (size.height - 2.0 * gap).max(0.0);
    match bottom {
        None => vec![rect(left, gap, width, height)],
        Some(bottom) => {
            let top = (height - gap - bottom).max(0.0);
            vec![
                rect(left, gap, width, top),
                rect(left, gap + top + gap, width, bottom),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_frames_inset_by_sides_and_gap() {
        // Центр окна 1180x720 в варианте sidebar: хост 230..814 без делителей рядом.
        let center = zone_frames(
            NSSize::new(584.0, 680.0),
            (6.0, 6.0),
            6.0,
            Some(BOTTOM_HEIGHT),
        );
        assert_eq!(center[0], rect(6.0, 6.0, 572.0, 482.0));
        assert_eq!(center[1], rect(6.0, 494.0, 572.0, 180.0));
        // В варианте plain хост 236..808 между делителями.
        let plain = zone_frames(
            NSSize::new(572.0, 680.0),
            (0.0, 0.0),
            6.0,
            Some(BOTTOM_HEIGHT),
        );
        assert_eq!(plain[0], rect(0.0, 6.0, 572.0, 482.0));
        // Правая зона 814..1180: зазор только у края окна.
        let right = zone_frames(NSSize::new(366.0, 680.0), (0.0, 6.0), 6.0, None);
        assert_eq!(right, vec![rect(0.0, 6.0, 360.0, 668.0)]);
        // Хост уже зазоров: плитки нулевой ширины, а не отрицательной.
        let narrow = zone_frames(NSSize::new(8.0, 680.0), (6.0, 6.0), 6.0, None);
        assert_eq!(narrow[0].size.width, 0.0);
    }
}
