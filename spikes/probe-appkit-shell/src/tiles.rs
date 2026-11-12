//! Плитки центральной, правой и нижней зон и плоский навигатор слева. Плитка — линия 1 точка,
//! радиус `--r-plate`, `TabStrip` высоты `--tabbar` внутри; между плитками и у краёв окна зазор
//! `--gap`, его дают делители сплита или зазоры хостов зон. Плитка с фокусом получает тон
//! режима, остальные — вуаль `--dim` внутри своих границ. Навигатор лежит
//! на фоне окна, вуали и тона не имеет; при фокусе в нём все плитки под вуалью, а выбранная
//! строка в рамке. Вуаль не принимает щелчок и не элемент доступности. Клавиши вкладок плитка с
//! фокусом передаёт своей полосе. В центральной и правой плитках под полосой `VirtualList`:
//! щелчок по строке делает first responder его таблицу, и фокус переходит плитке. В навигаторе
//! `TreeList`: его дерево — first responder навигатора. В нижней плитке индикатор работы:
//! вращающаяся дуга, при Reduce Motion — неподвижная пунктирная.

use std::cell::Cell;
use std::f64::consts::PI;
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::NSTableRowView;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSEvent, NSResponder, NSTextField, NSView};
use objc2_core_graphics::CGMutablePath;
use objc2_foundation::{NSArray, NSNumber, NSObject, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{CABasicAnimation, CAMediaTiming, CAShapeLayer, kCALineCapRound};

use crate::args::Motion;
use crate::chrome::{color, fill, label, rect};
use crate::fixtures::{self, SCENE_ROWS, TREE_BRANCHING, TabSpec};
use crate::list::{Hooks, ListLook, VirtualList};
use crate::scenario::counters::{Live, counter};
use crate::tabs::{Place, TabStrip};
use crate::tokens::Tokens;
use crate::tree::TreeList;

counter!(TILE, "ProbeTile");
counter!(VEIL, "ProbeVeil");
counter!(NAVIGATOR, "ProbeNavigator");
counter!(ZONE_HOST, "ProbeZoneHost");

/// Высота нижней зоны.
pub const BOTTOM_HEIGHT: f64 = 180.0;
/// Отступы содержимого навигатора, как `padding: 6px 6px 8px 8px` у `.side` прототипа.
pub const NAV_PAD_LEFT: f64 = 8.0;
pub const NAV_PAD_TOP: f64 = 6.0;
const NAV_PAD_RIGHT: f64 = 6.0;
const NAV_PAD_BOTTOM: f64 = 8.0;
/// Корень, раскрытый в начальной сцене.
pub const NAV_EXPANDED_ROOT: usize = 0;
/// Выбранная строка начальной сцены: второй ребёнок раскрытого корня.
pub const NAV_SELECTED: isize = 2;
const TITLE_INSET: f64 = 12.0;

pub const CENTER: usize = 0;
pub const RIGHT: usize = 1;
pub const BOTTOM: usize = 2;
/// Вкладки плиток сцены по индексам `CENTER`, `RIGHT`, `BOTTOM`.
pub const SCENE_TABS: [usize; 3] = [8, 2, 1];
/// Ключ анимации вращения дуги индикатора.
pub const SPIN_KEY: &str = "probe.work.spin";
/// Индикатор как `.st-spin` прототипа: 12 точек, окружность радиуса 4,5, линия 1,6, оборот за
/// секунду; дорожка — `--run` с непрозрачностью 0,25.
const SPINNER: f64 = 12.0;
const SPIN_RADIUS: f64 = 4.5;
const SPIN_LINE: f64 = 1.6;
const SPIN_TRACK_ALPHA: f64 = 0.25;
const SPIN_PERIOD: f64 = 1.0;
/// Пунктир неподвижной дуги: `stroke-dasharray: 2 2`.
const SPIN_DASH: f64 = 2.0;
const STATUS_GAP: f64 = 5.0;
const RUNNING: &str = "Работа идёт";

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
    tokens: Cell<Tokens>,
}

impl Tiles {
    /// `sides` — зазоры слева и справа у хостов центра и правой зоны.
    pub fn new(
        mtm: MainThreadMarker,
        tokens: Tokens,
        motion: Motion,
        sides: [(f64, f64); 2],
    ) -> Rc<Tiles> {
        let tiles = Rc::new_cyclic(|owner: &Weak<Tiles>| {
            let tile = |index: usize, title: &str, place: Place| {
                let tabs = fixtures::tabs(SCENE_TABS[index]);
                Tile::new(mtm, index, title, place, tabs, owner, &tokens, motion)
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
                tokens: Cell::new(tokens),
            }
        });
        tiles.apply(Focus::Tile(CENTER));
        tiles
    }

    pub fn focus(&self) -> Focus {
        self.focus.get()
    }

    /// Токены новой ячейки: хосты зон, навигатор, плитки и их слои над содержимым.
    pub fn set_tokens(&self, tokens: Tokens) {
        self.tokens.set(tokens);
        for host in [&self.center_host, &self.right_host] {
            host.set_tokens(&tokens);
        }
        self.navigator.set_tokens(&tokens);
        for tile in &self.tiles {
            tile.set_tokens(&tokens);
        }
        self.apply(self.focus());
    }

    /// Движение индикатора работы нижней плитки.
    pub fn set_motion(&self, motion: Motion) {
        if let Some(indicator) = self.indicator() {
            indicator.set_motion(motion);
        }
    }

    pub fn indicator(&self) -> Option<&Indicator> {
        self.tiles.get(BOTTOM).and_then(|tile| tile.indicator())
    }

    /// Тон у плитки с фокусом, вуаль у остальных, рамка у выбранной строки навигатора.
    fn apply(&self, focus: Focus) {
        self.focus.set(focus);
        let tokens = self.tokens.get();
        for (index, tile) in self.tiles.iter().enumerate() {
            let focused = focus == Focus::Tile(index);
            tile.paint_overlay(focused, &tokens);
            tile.strip().set_focused(focused);
        }
        self.navigator.paint_selection(focus == Focus::Navigator);
    }
}

/// Индикатор работы: дорожка и дуга `--run` и подпись. Дуга вращается бесконечной анимацией
/// слоя; при Reduce Motion анимация снимается, а дуга становится пунктирной и неподвижной, как
/// у прототипа. Остановленный индикатор скрывается вместе с подписью.
pub struct Indicator {
    spinner: Retained<NSView>,
    track: Retained<CAShapeLayer>,
    arc: Retained<CAShapeLayer>,
    status: Retained<NSTextField>,
    motion: Cell<Motion>,
    running: Cell<bool>,
}

impl Indicator {
    fn new(mtm: MainThreadMarker, tokens: &Tokens, motion: Motion) -> Indicator {
        let spinner = NSView::initWithFrame(
            NSView::alloc(mtm),
            rect(TITLE_INSET, 14.0, SPINNER, SPINNER),
        );
        spinner.setWantsLayer(true);
        let bounds = NSRect::new(NSPoint::ZERO, NSSize::new(SPINNER, SPINNER));
        let center = SPINNER / 2.0;
        let circle = CGMutablePath::new();
        // SAFETY: путь только что создан, преобразования нет.
        unsafe {
            CGMutablePath::add_arc(
                Some(&circle),
                std::ptr::null(),
                center,
                center,
                SPIN_RADIUS,
                0.0,
                2.0 * PI,
                false,
            )
        };
        let quarter = CGMutablePath::new();
        // Четверть от верха вправо по часовой стрелке; слой не перевёрнут, верх — π/2.
        // SAFETY: путь только что создан, преобразования нет.
        unsafe {
            CGMutablePath::add_arc(
                Some(&quarter),
                std::ptr::null(),
                center,
                center,
                SPIN_RADIUS,
                PI / 2.0,
                0.0,
                true,
            )
        };
        let shape = |path: &CGMutablePath| {
            let layer = CAShapeLayer::new();
            layer.setFrame(bounds);
            layer.setPath(Some(path));
            layer.setFillColor(None);
            layer.setLineWidth(SPIN_LINE);
            // SAFETY: имя окончания линии — неизменяемая константа Core Animation.
            layer.setLineCap(unsafe { kCALineCapRound });
            layer
        };
        let track = shape(&circle);
        let arc = shape(&quarter);
        if let Some(host) = spinner.layer() {
            host.addSublayer(&track);
            host.addSublayer(&arc);
        }
        let status = label(
            mtm,
            RUNNING,
            tokens.run,
            rect(TITLE_INSET + SPINNER + STATUS_GAP, 11.0, 200.0, 18.0),
        );
        let indicator = Indicator {
            spinner,
            track,
            arc,
            status,
            motion: Cell::new(motion),
            running: Cell::new(true),
        };
        indicator.set_tokens(tokens);
        indicator.update();
        indicator
    }

    fn add_to(&self, body: &NSView) {
        body.addSubview(&self.spinner);
        body.addSubview(&self.status);
    }

    /// Дорожка, дуга и подпись цвета `--run`.
    pub fn set_tokens(&self, tokens: &Tokens) {
        self.track
            .setStrokeColor(Some(&color(tokens.run, SPIN_TRACK_ALPHA).CGColor()));
        self.arc
            .setStrokeColor(Some(&color(tokens.run, 1.0).CGColor()));
        self.status.setTextColor(Some(&color(tokens.run, 1.0)));
    }

    pub fn set_motion(&self, motion: Motion) {
        self.motion.set(motion);
        self.update();
    }

    /// Работа идёт или закончилась; закончившаяся скрывает индикатор.
    pub fn set_running(&self, running: bool) {
        self.running.set(running);
        self.update();
    }

    pub fn motion(&self) -> Motion {
        self.motion.get()
    }

    pub fn spinner(&self) -> &NSView {
        &self.spinner
    }

    pub fn track(&self) -> &CAShapeLayer {
        &self.track
    }

    /// Слой дуги, который вращается.
    pub fn arc(&self) -> &CAShapeLayer {
        &self.arc
    }

    pub fn status(&self) -> &NSTextField {
        &self.status
    }

    fn update(&self) {
        let running = self.running.get();
        self.spinner.setHidden(!running);
        self.status.setHidden(!running);
        let key = NSString::from_str(SPIN_KEY);
        let spinning = running && self.motion.get() == Motion::Normal;
        let dash = (!spinning).then(|| {
            let step = NSNumber::new_f64(SPIN_DASH);
            NSArray::from_retained_slice(&[step.clone(), step])
        });
        self.arc.setLineDashPattern(dash.as_deref());
        if !spinning {
            self.arc.removeAnimationForKey(&key);
            return;
        }
        let present = self
            .arc
            .animationKeys()
            .is_some_and(|keys| keys.iter().any(|existing| *existing == *key));
        if present {
            return;
        }
        let spin = CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str(
            "transform.rotation.z",
        )));
        let (from, to) = (NSNumber::new_f64(0.0), NSNumber::new_f64(-2.0 * PI));
        let (from, to): (&AnyObject, &AnyObject) = (&from, &to);
        // SAFETY: значения анимации поворота — NSNumber.
        unsafe {
            spin.setFromValue(Some(from));
            spin.setToValue(Some(to));
        }
        spin.setDuration(SPIN_PERIOD);
        spin.setRepeatCount(f32::INFINITY);
        self.arc.addAnimation_forKey(&spin, Some(&key));
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
    /// Индикатор работы нижней плитки.
    indicator: Option<Indicator>,
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
    #[allow(clippy::too_many_arguments)]
    fn new(
        mtm: MainThreadMarker,
        index: usize,
        title: &str,
        place: Place,
        tabs: Vec<TabSpec>,
        owner: &Weak<Tiles>,
        tokens: &Tokens,
        motion: Motion,
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
        let indicator = (place == Place::Bottom).then(|| {
            let indicator = Indicator::new(mtm, tokens, motion);
            indicator.add_to(&body);
            indicator
        });

        let list = (place != Place::Bottom).then(|| {
            let list = VirtualList::new(
                mtm,
                tokens,
                body.bounds(),
                fixtures::list(SCENE_ROWS),
                ListLook::tile(tokens),
            );
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
            indicator,
            veil,
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let tile: Retained<Tile> = unsafe { msg_send![super(this), initWithFrame: frame] };
        tile.paint_plate(tokens);
        if let Some(layer) = tile.layer() {
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

    pub fn indicator(&self) -> Option<&Indicator> {
        self.ivars().indicator.as_ref()
    }

    /// Фон `--panel`, линия `--line` и радиус `--r-plate`.
    fn paint_plate(&self, tokens: &Tokens) {
        fill(self, tokens.panel, 1.0);
        if let Some(layer) = self.layer() {
            layer.setBorderWidth(1.0);
            layer.setBorderColor(Some(&color(tokens.line, 1.0).CGColor()));
            layer.setCornerRadius(tokens.r_plate);
        }
    }

    /// Токены новой ячейки для плитки, её полосы, списка и индикатора. Слой над содержимым
    /// перекрашивает владелец плиток по фокусу.
    fn set_tokens(&self, tokens: &Tokens) {
        self.paint_plate(tokens);
        let ivars = self.ivars();
        ivars.strip.set_tokens(tokens);
        if let Some(list) = &ivars.list {
            list.set_colors(tokens, ListLook::tile(tokens).background);
        }
        if let Some(indicator) = &ivars.indicator {
            indicator.set_tokens(tokens);
        }
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
    tree: TreeList,
}

define_class!(
    /// Плоский навигатор на фоне окна с `TreeList` внутри отступов.
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

        /// Щелчок по отступу навигатора отдаёт фокус дереву.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self.ivars().tree.outline()));
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
        let height = 600.0;
        let tree = TreeList::new(
            mtm,
            tokens,
            rect(
                NAV_PAD_LEFT,
                NAV_PAD_TOP,
                width - NAV_PAD_LEFT - NAV_PAD_RIGHT,
                height - NAV_PAD_TOP - NAV_PAD_BOTTOM,
            ),
            &fixtures::tree(&TREE_BRANCHING),
        );
        if let Some(root) = tree.roots().get(NAV_EXPANDED_ROOT) {
            tree.expand(root, false);
        }
        tree.select_row(NAV_SELECTED);
        let focus_owner = owner.clone();
        tree.set_on_focus(Box::new(move || {
            if let Some(owner) = focus_owner.upgrade() {
                owner.apply(Focus::Navigator);
            }
        }));
        let this = Navigator::alloc(mtm).set_ivars(NavigatorIvars {
            _live: NAVIGATOR.live(),
            owner: owner.clone(),
            tree,
        });
        let frame = NSRect::new(NSPoint::ZERO, NSSize::new(width, height));
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let view: Retained<Navigator> = unsafe { msg_send![super(this), initWithFrame: frame] };
        fill(&view, tokens.chrome, 1.0);
        view.addSubview(view.ivars().tree.view());
        view
    }

    pub fn tree(&self) -> &TreeList {
        &self.ivars().tree
    }

    /// Фон окна `--chrome` и цвета дерева.
    fn set_tokens(&self, tokens: &Tokens) {
        fill(self, tokens.chrome, 1.0);
        self.ivars().tree.set_colors(tokens);
    }

    /// Вид выбранной строки дерева, если он создан.
    pub fn selected_row(&self) -> Option<Retained<NSTableRowView>> {
        self.ivars().tree.selected_row_view()
    }

    fn paint_selection(&self, focused: bool) {
        self.ivars().tree.set_focused(focused);
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

    /// Фон окна `--chrome`.
    fn set_tokens(&self, tokens: &Tokens) {
        fill(self, tokens.chrome, 1.0);
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
