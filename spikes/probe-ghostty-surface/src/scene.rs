//! Сцена четырёх зон: левая, центральная, правая и нижняя. Левая и правая зоны и центральная
//! колонка — обычные `NSSplitViewItem` (ADR-0023), центральная и нижняя зоны — обычные элементы
//! второго сплита внутри колонки. Зона делится на части по одной оси (ADR-0031): левая и правая
//! одна над другой, центр и нижняя рядом. Часть — плитка с линией 1 точка, скруглением 7 точек и
//! полосой вкладок 34 точки (ADR-0024); вкладка показывает свой вид в теле плитки. Плитка с
//! фокусом выделена тоном полосы вкладок и линией под активной вкладкой цвета режима: зелёного во
//! `Input`, синего в `Navigation` (токены `--inp-line` и `--nav-line` прототипа).
//!
//! Перенос вкладки между зонами меняет родителя того же `NSView`, ничего не пересоздавая.
//! Расположение вида читается по фактической цепочке `superview`, а не по учёту сцены.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSAutoresizingMaskOptions, NSColor, NSFont, NSResponder, NSSplitView, NSSplitViewController,
    NSSplitViewItem, NSTextField, NSView, NSViewController, NSWindow,
};
use objc2_core_graphics::CGColor;
use objc2_foundation::{NSArray, NSObject, NSPoint, NSRect, NSSize, NSString};
use serde::Serialize;

use crate::router::Mode;
use crate::scenario::counters::{Live, counter};

counter!(SPLIT_VIEW, "ProbeSplitView");
counter!(ZONE_HOST, "ProbeZoneHost");
counter!(TILE, "ProbeTile");

/// Зазор между плитками и ширина делителя сплита.
pub const GAP: f64 = 6.0;
pub const TAB_BAR: f64 = 34.0;
pub const RADIUS: f64 = 7.0;
const LINE: f64 = 1.0;
/// Линия под активной вкладкой плитки с фокусом.
const FOCUS_LINE: f64 = 2.0;
const TAB_WIDTH: f64 = 132.0;
const TAB_INSET: f64 = 12.0;
const TAB_FONT: f64 = 12.0;
/// Доля тона режима в полосе вкладок плитки с фокусом. Значение пробы: тон должен различаться на
/// снимке окна.
const FOCUS_TONE: f64 = 0.14;
pub const LEFT_WIDTH: f64 = 236.0;
pub const RIGHT_WIDTH: f64 = 366.0;
pub const BOTTOM_HEIGHT: f64 = 220.0;
const LEFT_MIN: f64 = 160.0;
const RIGHT_MIN: f64 = 240.0;
const COLUMN_MIN: f64 = 300.0;
const CENTER_MIN: f64 = 200.0;
const BOTTOM_MIN: f64 = 120.0;
/// Колонка и центр уступают размер первыми.
const SIDE_HOLDING: f32 = 260.0;
const GROW_HOLDING: f32 = 250.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Zone {
    Left,
    Center,
    Right,
    Bottom,
}

impl Zone {
    pub const ALL: [Zone; 4] = [Zone::Left, Zone::Center, Zone::Right, Zone::Bottom];

    pub fn name(self) -> &'static str {
        match self {
            Zone::Left => "left",
            Zone::Center => "center",
            Zone::Right => "right",
            Zone::Bottom => "bottom",
        }
    }

    /// Ось деления зоны на части (ADR-0031).
    pub fn axis(self) -> Axis {
        match self {
            Zone::Left | Zone::Right => Axis::Stacked,
            Zone::Center | Zone::Bottom => Axis::SideBySide,
        }
    }

    fn index(self) -> usize {
        match self {
            Zone::Left => 0,
            Zone::Center => 1,
            Zone::Right => 2,
            Zone::Bottom => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Части одна над другой, первая сверху.
    Stacked,
    /// Части рядом, первая слева.
    SideBySide,
}

/// Делители сцены.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Divider {
    LeftCenter,
    CenterRight,
    CenterBottom,
}

impl Divider {
    pub fn name(self) -> &'static str {
        match self {
            Divider::LeftCenter => "left|center",
            Divider::CenterRight => "center|right",
            Divider::CenterBottom => "center/bottom",
        }
    }
}

// ---- геометрия без AppKit ----

/// Рамки `parts` частей зоны с границами `bounds` по её оси, между частями зазор `gap`.
pub fn part_frames(bounds: NSRect, axis: Axis, parts: usize, gap: f64) -> Vec<NSRect> {
    if parts == 0 {
        return Vec::new();
    }
    let count = parts as f64;
    let gaps = gap * (count - 1.0);
    (0..parts)
        .map(|index| {
            let step = index as f64;
            match axis {
                Axis::SideBySide => {
                    let width = ((bounds.size.width - gaps) / count).max(0.0);
                    NSRect::new(
                        NSPoint::new(bounds.origin.x + step * (width + gap), bounds.origin.y),
                        NSSize::new(width, bounds.size.height.max(0.0)),
                    )
                }
                Axis::Stacked => {
                    let height = ((bounds.size.height - gaps) / count).max(0.0);
                    let top = bounds.origin.y + bounds.size.height;
                    NSRect::new(
                        NSPoint::new(bounds.origin.x, top - (step + 1.0) * height - step * gap),
                        NSSize::new(bounds.size.width.max(0.0), height),
                    )
                }
            }
        })
        .collect()
}

/// Тело плитки размера `tile` под полосой вкладок.
pub fn content_frame(tile: NSSize) -> NSRect {
    NSRect::new(
        NSPoint::ZERO,
        NSSize::new(tile.width.max(0.0), (tile.height - TAB_BAR).max(0.0)),
    )
}

/// Размер в точках, переведённый в пиксели подложки масштаба `scale`, с отбрасыванием дробной
/// части. Нечисловой или неположительный размер даёт 0.
pub fn to_pixels(points: NSSize, scale: f64) -> (u32, u32) {
    let pixels = |value: f64| {
        let scaled = value * scale;
        if scaled.is_finite() && scaled > 0.0 {
            scaled.floor().min(f64::from(u32::MAX)) as u32
        } else {
            0
        }
    };
    (pixels(points.width), pixels(points.height))
}

// ---- цвета ----

/// Цвет sRGB, по 8 бит на канал, всегда непрозрачный.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn hex(value: u32) -> Rgb {
        Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
    }

    /// `top` с долей `alpha` поверх `self` в кодированных значениях sRGB.
    pub fn mix(self, top: Rgb, alpha: f64) -> Rgb {
        let channel = |base: u8, over: u8| {
            (f64::from(base) * (1.0 - alpha) + f64::from(over) * alpha).round() as u8
        };
        Rgb(
            channel(self.0, top.0),
            channel(self.1, top.1),
            channel(self.2, top.2),
        )
    }

    pub fn color(self) -> Retained<NSColor> {
        NSColor::colorWithSRGBRed_green_blue_alpha(
            f64::from(self.0) / 255.0,
            f64::from(self.1) / 255.0,
            f64::from(self.2) / 255.0,
            1.0,
        )
    }
}

/// Цвета сцены в одной теме: значения токенов прототипа; `input` и `navigation` — цвета
/// режимов.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Palette {
    pub chrome: Rgb,
    pub panel: Rgb,
    pub panel_head: Rgb,
    pub line: Rgb,
    pub text: Rgb,
    pub text_2: Rgb,
    pub input: Rgb,
    pub navigation: Rgb,
}

impl Palette {
    pub const LIGHT: Palette = Palette {
        chrome: Rgb::hex(0xEFEDE8),
        panel: Rgb::hex(0xFFFFFF),
        panel_head: Rgb::hex(0xF8F7F3),
        line: Rgb::hex(0xDDD9D1),
        text: Rgb::hex(0x1F1D1A),
        text_2: Rgb::hex(0x4F4B44),
        input: Rgb::hex(0x4D7553),
        navigation: Rgb::hex(0x48678C),
    };

    pub const DARK: Palette = Palette {
        chrome: Rgb::hex(0x1E1D1B),
        panel: Rgb::hex(0x0A0908),
        panel_head: Rgb::hex(0x11100E),
        line: Rgb::hex(0x302E2B),
        text: Rgb::hex(0xE6E1D8),
        text_2: Rgb::hex(0xABA59A),
        input: Rgb::hex(0x8DB08F),
        navigation: Rgb::hex(0x7F9BC0),
    };

    pub fn of(dark: bool) -> Palette {
        if dark { Palette::DARK } else { Palette::LIGHT }
    }

    pub fn mode_color(&self, mode: Mode) -> Rgb {
        match mode {
            Mode::Input => self.input,
            Mode::Navigation => self.navigation,
        }
    }

    /// Полоса вкладок: у плитки с фокусом с тоном режима.
    pub fn strip(&self, focused: bool, mode: Mode) -> Rgb {
        if focused {
            self.panel_head.mix(self.mode_color(mode), FOCUS_TONE)
        } else {
            self.panel_head
        }
    }
}

/// Тёмное ли оформление.
pub fn is_dark(appearance: &NSAppearance) -> bool {
    // SAFETY: имена оформления — неизменяемые константы AppKit.
    let names = unsafe { NSArray::from_slice(&[NSAppearanceNameAqua, NSAppearanceNameDarkAqua]) };
    appearance
        .bestMatchFromAppearancesWithNames(&names)
        // SAFETY: см. выше.
        .is_some_and(|name| name.isEqualToString(unsafe { NSAppearanceNameDarkAqua }))
}

fn fill(view: &NSView, color: Rgb) {
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        layer.setBackgroundColor(Some(&color.color().CGColor()));
    }
}

// ---- виды ----

pub struct SplitIvars {
    _live: Live,
    divider: RefCell<Retained<NSColor>>,
}

define_class!(
    /// Сплит с делителем ширины зазора цвета окна.
    #[unsafe(super(NSSplitView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeGhosttySplitView"]
    #[ivars = SplitIvars]
    pub struct ProbeSplitView;

    impl ProbeSplitView {
        #[unsafe(method(dividerThickness))]
        fn divider_thickness(&self) -> f64 {
            GAP
        }

        #[unsafe(method_id(dividerColor))]
        fn divider_color(&self) -> Retained<NSColor> {
            self.ivars().divider.borrow().clone()
        }
    }
);

impl ProbeSplitView {
    fn new(mtm: MainThreadMarker, vertical: bool, palette: &Palette) -> Retained<ProbeSplitView> {
        let this = ProbeSplitView::alloc(mtm).set_ivars(SplitIvars {
            _live: SPLIT_VIEW.live(),
            divider: RefCell::new(palette.chrome.color()),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let split: Retained<ProbeSplitView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        split.setVertical(vertical);
        split
    }

    fn set_divider_color(&self, color: Rgb) {
        *self.ivars().divider.borrow_mut() = color.color();
        self.setNeedsDisplay(true);
    }
}

pub struct HostIvars {
    _live: Live,
    zone: Zone,
    tiles: RefCell<Vec<Retained<Tile>>>,
}

define_class!(
    /// Вид зоны в элементе сплита: раскладывает части по оси зоны.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeZoneHost"]
    #[ivars = HostIvars]
    pub struct ZoneHost;

    impl ZoneHost {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            true
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            // SAFETY: вызов реализации суперкласса с тем же размером.
            let () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.relayout();
        }
    }
);

impl ZoneHost {
    fn new(mtm: MainThreadMarker, zone: Zone, palette: &Palette) -> Retained<ZoneHost> {
        let this = ZoneHost::alloc(mtm).set_ivars(HostIvars {
            _live: ZONE_HOST.live(),
            zone,
            tiles: RefCell::new(Vec::new()),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let host: Retained<ZoneHost> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        fill(&host, palette.chrome);
        host
    }

    pub fn zone(&self) -> Zone {
        self.ivars().zone
    }

    pub fn tiles(&self) -> Vec<Retained<Tile>> {
        self.ivars().tiles.borrow().clone()
    }

    fn add_tile(&self, tile: Retained<Tile>) -> usize {
        self.addSubview(&tile);
        let mut tiles = self.ivars().tiles.borrow_mut();
        tiles.push(tile);
        let index = tiles.len() - 1;
        drop(tiles);
        self.relayout();
        index
    }

    fn remove_tile(&self, index: usize) -> Option<Retained<Tile>> {
        let mut tiles = self.ivars().tiles.borrow_mut();
        if index >= tiles.len() {
            return None;
        }
        let tile = tiles.remove(index);
        drop(tiles);
        tile.removeFromSuperview();
        self.relayout();
        Some(tile)
    }

    fn relayout(&self) {
        let tiles = self.tiles();
        let frames = part_frames(self.bounds(), self.zone().axis(), tiles.len(), GAP);
        for (tile, frame) in tiles.iter().zip(frames) {
            tile.setFrame(frame);
        }
    }
}

struct Tab {
    title: String,
    view: Retained<NSView>,
}

pub struct TileIvars {
    _live: Live,
    tabs: RefCell<Vec<Tab>>,
    active: Cell<Option<usize>>,
    strip: Retained<NSView>,
    underline: Retained<NSView>,
    content: Retained<NSView>,
    labels: RefCell<Vec<Retained<NSTextField>>>,
    focused: Cell<bool>,
    mode: Cell<Mode>,
    palette: Cell<Palette>,
}

define_class!(
    /// Часть зоны: плитка со скруглением, полосой вкладок и телом активной вкладки.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeTile"]
    #[ivars = TileIvars]
    pub struct Tile;

    impl Tile {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            true
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            // SAFETY: вызов реализации суперкласса с тем же размером.
            let () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.relayout();
        }

        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn view_did_change_effective_appearance(&self) {
            // SAFETY: вызов реализации суперкласса без аргументов.
            let () = unsafe { msg_send![super(self), viewDidChangeEffectiveAppearance] };
            self.ivars()
                .palette
                .set(Palette::of(is_dark(&self.effectiveAppearance())));
            self.recolor();
        }
    }
);

impl Tile {
    fn new(mtm: MainThreadMarker, palette: &Palette, mode: Mode) -> Retained<Tile> {
        let strip = NSView::new(mtm);
        let underline = NSView::new(mtm);
        let content = NSView::new(mtm);
        let this = Tile::alloc(mtm).set_ivars(TileIvars {
            _live: TILE.live(),
            tabs: RefCell::new(Vec::new()),
            active: Cell::new(None),
            strip: strip.clone(),
            underline: underline.clone(),
            content: content.clone(),
            labels: RefCell::new(Vec::new()),
            focused: Cell::new(false),
            mode: Cell::new(mode),
            palette: Cell::new(*palette),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let tile: Retained<Tile> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        tile.setWantsLayer(true);
        if let Some(layer) = tile.layer() {
            layer.setCornerRadius(RADIUS);
            layer.setMasksToBounds(true);
            layer.setBorderWidth(LINE);
        }
        tile.addSubview(&content);
        tile.addSubview(&strip);
        strip.addSubview(&underline);
        tile.recolor();
        tile
    }

    /// Тело плитки: родитель вида активной вкладки.
    pub fn content(&self) -> Retained<NSView> {
        self.ivars().content.clone()
    }

    pub fn titles(&self) -> Vec<String> {
        self.ivars()
            .tabs
            .borrow()
            .iter()
            .map(|tab| tab.title.clone())
            .collect()
    }

    pub fn active_view(&self) -> Option<Retained<NSView>> {
        let index = self.ivars().active.get()?;
        self.ivars()
            .tabs
            .borrow()
            .get(index)
            .map(|tab| tab.view.clone())
    }

    pub fn is_focused(&self) -> bool {
        self.ivars().focused.get()
    }

    /// Палитра текущего оформления плитки.
    pub fn palette(&self) -> Palette {
        self.ivars().palette.get()
    }

    pub fn set_focused(&self, focused: bool) {
        if self.ivars().focused.replace(focused) != focused {
            self.recolor();
        }
    }

    /// Режим переживает смену оформления: палитра перестраивается, режим хранится отдельно.
    fn set_mode(&self, mode: Mode) {
        if self.ivars().mode.replace(mode) != mode {
            self.recolor();
        }
    }

    /// Фактический цвет полосы вкладок, прочитанный у её слоя.
    pub fn strip_rgb(&self) -> Option<Rgb> {
        let color = self.ivars().strip.layer()?.backgroundColor()?;
        let count = CGColor::number_of_components(Some(&color));
        let components = CGColor::components(Some(&color));
        if count < 3 || components.is_null() {
            return None;
        }
        // SAFETY: Core Graphics отдаёт `count` компонентов цвета, живого на время чтения.
        let values = unsafe { std::slice::from_raw_parts(components, count) };
        let channel = |value: f64| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        Some(Rgb(
            channel(values[0]),
            channel(values[1]),
            channel(values[2]),
        ))
    }

    /// Новая вкладка становится активной: её вид меняет родителя на тело плитки. Вид прежней
    /// активной вкладки уходит из иерархии, но остаётся во вкладке.
    fn add_tab(&self, title: &str, view: &NSView) {
        if let Some(previous) = self.active_view() {
            previous.removeFromSuperview();
        }
        let index = {
            let mut tabs = self.ivars().tabs.borrow_mut();
            tabs.push(Tab {
                title: title.to_owned(),
                view: view.retain(),
            });
            tabs.len() - 1
        };
        self.ivars().active.set(Some(index));
        self.ivars().content.addSubview(view);
        self.rebuild_labels();
        self.relayout();
    }

    /// Снимает вкладку с вида и возвращает её заголовок. Сам вид остаётся у прежнего родителя,
    /// пока новый родитель его не заберёт. Активной становится соседняя вкладка.
    fn take_tab(&self, view: &NSView) -> Option<String> {
        let ivars = self.ivars();
        let (title, index, remaining) = {
            let mut tabs = ivars.tabs.borrow_mut();
            let index = tabs
                .iter()
                .position(|tab| std::ptr::addr_eq(&*tab.view, view))?;
            (tabs.remove(index).title, index, tabs.len())
        };
        let active = match ivars.active.get() {
            _ if remaining == 0 => None,
            Some(active) if active == index => {
                let next = index.min(remaining - 1);
                if let Some(tab) = ivars.tabs.borrow().get(next) {
                    ivars.content.addSubview(&tab.view);
                }
                Some(next)
            }
            Some(active) if active > index => Some(active - 1),
            other => other,
        };
        ivars.active.set(active);
        self.rebuild_labels();
        self.relayout();
        Some(title)
    }

    fn rebuild_labels(&self) {
        let mtm = self.mtm();
        let ivars = self.ivars();
        for label in ivars.labels.borrow_mut().drain(..) {
            label.removeFromSuperview();
        }
        let titles = self.titles();
        let mut labels = ivars.labels.borrow_mut();
        for title in titles {
            let label = NSTextField::labelWithString(&NSString::from_str(&title), mtm);
            label.setFont(Some(&NSFont::systemFontOfSize(TAB_FONT)));
            ivars.strip.addSubview(&label);
            labels.push(label);
        }
        drop(labels);
        self.recolor();
    }

    fn relayout(&self) {
        let ivars = self.ivars();
        let size = self.bounds().size;
        let content = content_frame(size);
        ivars.content.setFrame(content);
        ivars.strip.setFrame(NSRect::new(
            NSPoint::new(0.0, content.size.height),
            NSSize::new(size.width.max(0.0), TAB_BAR.min(size.height.max(0.0))),
        ));
        if let Some(view) = self.active_view() {
            view.setFrame(NSRect::new(NSPoint::ZERO, content.size));
        }
        for (index, label) in ivars.labels.borrow().iter().enumerate() {
            let height = label.fittingSize().height;
            label.setFrame(NSRect::new(
                NSPoint::new(
                    index as f64 * TAB_WIDTH + TAB_INSET,
                    ((TAB_BAR - height) / 2.0).floor(),
                ),
                NSSize::new(TAB_WIDTH - 2.0 * TAB_INSET, height),
            ));
        }
        let active = ivars.active.get().unwrap_or(0) as f64;
        ivars.underline.setFrame(NSRect::new(
            NSPoint::new(active * TAB_WIDTH, 0.0),
            NSSize::new(TAB_WIDTH, FOCUS_LINE),
        ));
    }

    fn recolor(&self) {
        let ivars = self.ivars();
        let palette = ivars.palette.get();
        let focused = ivars.focused.get();
        let mode = ivars.mode.get();
        if let Some(layer) = self.layer() {
            layer.setBackgroundColor(Some(&palette.panel.color().CGColor()));
            layer.setBorderColor(Some(&palette.line.color().CGColor()));
        }
        fill(&ivars.content, palette.panel);
        fill(&ivars.strip, palette.strip(focused, mode));
        fill(&ivars.underline, palette.mode_color(mode));
        ivars
            .underline
            .setHidden(!focused || ivars.active.get().is_none());
        let active = ivars.active.get();
        for (index, label) in ivars.labels.borrow().iter().enumerate() {
            let ink = if Some(index) == active {
                palette.text
            } else {
                palette.text_2
            };
            label.setTextColor(Some(&ink.color()));
        }
    }
}

// ---- сцена ----

/// Рамка части зоны для итога: координаты окна.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PartInfo {
    pub zone: Zone,
    pub part: usize,
    pub frame: [f64; 4],
    pub content: [f64; 4],
    pub tabs: Vec<String>,
    pub focused: bool,
}

/// Плитка, в которую может перейти фокус.
pub struct TileTarget {
    pub zone: Zone,
    pub part: usize,
    pub frame: NSRect,
    pub view: Retained<NSView>,
}

pub struct Scene {
    pub root: Retained<NSSplitViewController>,
    pub root_split: Retained<ProbeSplitView>,
    pub column_split: Retained<ProbeSplitView>,
    hosts: Vec<Retained<ZoneHost>>,
    palette: Cell<Palette>,
    mode: Cell<Mode>,
}

impl Scene {
    /// Сцена с одной частью в каждой зоне.
    pub fn new(mtm: MainThreadMarker, dark: bool) -> Scene {
        let palette = Palette::of(dark);
        let hosts: Vec<Retained<ZoneHost>> = Zone::ALL
            .into_iter()
            .map(|zone| {
                let host = ZoneHost::new(mtm, zone, &palette);
                host.add_tile(Tile::new(mtm, &palette, Mode::Input));
                host
            })
            .collect();
        let column_split = ProbeSplitView::new(mtm, false, &palette);
        let column = NSSplitViewController::new(mtm);
        column.setSplitView(&column_split);
        for (zone, min, holding) in [
            (Zone::Center, CENTER_MIN, GROW_HOLDING),
            (Zone::Bottom, BOTTOM_MIN, SIDE_HOLDING),
        ] {
            let item = plain_item(mtm, &hosts[zone.index()]);
            configure(&item, min, holding);
            column.addSplitViewItem(&item);
        }
        let root_split = ProbeSplitView::new(mtm, true, &palette);
        let root = NSSplitViewController::new(mtm);
        root.setSplitView(&root_split);
        let left = plain_item(mtm, &hosts[Zone::Left.index()]);
        configure(&left, LEFT_MIN, SIDE_HOLDING);
        root.addSplitViewItem(&left);
        let middle = NSSplitViewItem::splitViewItemWithViewController(&column);
        configure(&middle, COLUMN_MIN, GROW_HOLDING);
        root.addSplitViewItem(&middle);
        let right = plain_item(mtm, &hosts[Zone::Right.index()]);
        configure(&right, RIGHT_MIN, SIDE_HOLDING);
        root.addSplitViewItem(&right);
        Scene {
            root,
            root_split,
            column_split,
            hosts,
            palette: Cell::new(palette),
            mode: Cell::new(Mode::Input),
        }
    }

    /// Кладёт сцену в `container` с зазором у краёв окна и ставит начальные делители.
    pub fn install(&self, container: &NSView, window: &NSWindow) {
        window.setBackgroundColor(Some(&self.palette.get().chrome.color()));
        let view = self.root.view();
        let bounds = container.bounds();
        view.setFrame(NSRect::new(
            NSPoint::new(GAP, GAP),
            NSSize::new(
                (bounds.size.width - 2.0 * GAP).max(0.0),
                (bounds.size.height - 2.0 * GAP).max(0.0),
            ),
        ));
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        container.addSubview(&view);
        view.layoutSubtreeIfNeeded();
        let width = self.root_split.bounds().size.width;
        self.set_divider(Divider::LeftCenter, LEFT_WIDTH);
        self.set_divider(Divider::CenterRight, width - RIGHT_WIDTH - GAP);
        view.layoutSubtreeIfNeeded();
        let height = self.column_split.bounds().size.height;
        self.set_divider(Divider::CenterBottom, height - BOTTOM_HEIGHT - GAP);
        view.layoutSubtreeIfNeeded();
    }

    pub fn host(&self, zone: Zone) -> &ZoneHost {
        &self.hosts[zone.index()]
    }

    pub fn tile(&self, zone: Zone, part: usize) -> Option<Retained<Tile>> {
        self.host(zone).tiles().get(part).cloned()
    }

    /// Новая часть зоны после последней, по оси зоны. Возвращает её номер.
    pub fn split(&self, zone: Zone) -> usize {
        let host = self.host(zone);
        host.add_tile(Tile::new(host.mtm(), &self.palette.get(), self.mode.get()))
    }

    /// Убирает пустую часть зоны, кроме последней оставшейся.
    pub fn unsplit(&self, zone: Zone, part: usize) -> Result<(), String> {
        let host = self.host(zone);
        let tiles = host.tiles();
        let tile = tiles
            .get(part)
            .ok_or(format!("нет части {} зоны {}", part, zone.name()))?;
        if tiles.len() == 1 {
            return Err(format!("часть {part} — последняя в зоне {}", zone.name()));
        }
        if !tile.titles().is_empty() {
            return Err(format!("часть {part} зоны {} не пуста", zone.name()));
        }
        host.remove_tile(part);
        Ok(())
    }

    pub fn add_tab(
        &self,
        zone: Zone,
        part: usize,
        title: &str,
        view: &NSView,
    ) -> Result<(), String> {
        let tile =
            self.tile(zone, part)
                .ok_or(format!("нет части {} зоны {}", part, zone.name()))?;
        tile.add_tab(title, view);
        Ok(())
    }

    /// Снимает вкладку вида с её плитки. Вид остаётся у прежнего родителя до [`Scene::add_tab`].
    pub fn take_tab(&self, view: &NSView) -> Result<String, String> {
        let (zone, part) = self
            .locate(view)
            .ok_or("вид не в теле плитки сцены".to_owned())?;
        let tile =
            self.tile(zone, part)
                .ok_or(format!("нет части {} зоны {}", part, zone.name()))?;
        tile.take_tab(view).ok_or(format!(
            "у части {part} зоны {} нет вкладки вида",
            zone.name()
        ))
    }

    /// Зона и часть вида по фактической цепочке `superview`: тело плитки, плитка, вид зоны.
    pub fn locate(&self, view: &NSView) -> Option<(Zone, usize)> {
        // SAFETY: `superview` возвращает удержанного родителя или nil; вызов на главном потоке.
        let (content, tile, host) = unsafe {
            let content = view.superview()?;
            let tile = content.superview()?;
            let host = tile.superview()?;
            (content, tile, host)
        };
        let host = self.hosts.iter().find(|candidate| {
            std::ptr::addr_eq(Retained::as_ptr(candidate), Retained::as_ptr(&host))
        })?;
        let part = host.tiles().iter().position(|candidate| {
            std::ptr::addr_eq(Retained::as_ptr(candidate), Retained::as_ptr(&tile))
                && std::ptr::addr_eq(
                    Retained::as_ptr(&candidate.content()),
                    Retained::as_ptr(&content),
                )
        })?;
        Some((host.zone(), part))
    }

    /// Делает вид first responder окна и выделяет его плитку. `true` — окно приняло вид.
    pub fn focus(&self, window: &NSWindow, view: &NSView) -> bool {
        let accepted = window.makeFirstResponder(Some(view));
        self.sync_focus(window);
        accepted
    }

    /// Выделение тоном у плитки, в которой фактический first responder окна.
    pub fn sync_focus(&self, window: &NSWindow) {
        let responder = window.firstResponder();
        let focused = responder
            .as_deref()
            .and_then(|responder| responder.downcast_ref::<NSView>())
            .and_then(|view| self.locate(view));
        for host in &self.hosts {
            for (part, tile) in host.tiles().iter().enumerate() {
                tile.set_focused(focused == Some((host.zone(), part)));
            }
        }
    }

    /// Палитра сцены по оформлению: окно, делители и виды зон. Плитки следуют оформлению сами
    /// (`viewDidChangeEffectiveAppearance`).
    pub fn apply_appearance(&self, window: &NSWindow, dark: bool) {
        let palette = Palette::of(dark);
        self.palette.set(palette);
        window.setBackgroundColor(Some(&palette.chrome.color()));
        self.root_split.set_divider_color(palette.chrome);
        self.column_split.set_divider_color(palette.chrome);
        for host in &self.hosts {
            fill(host, palette.chrome);
        }
    }

    /// Режим ввода показывается тоном плитки с фокусом и линией под её активной вкладкой.
    pub fn set_mode(&self, mode: Mode) {
        self.mode.set(mode);
        for host in &self.hosts {
            for tile in host.tiles() {
                tile.set_mode(mode);
            }
        }
    }

    /// Плитки с видом активной вкладки: зона, часть, рамка в координатах окна и вид.
    pub fn targets(&self) -> Vec<TileTarget> {
        self.hosts
            .iter()
            .flat_map(|host| {
                host.tiles()
                    .into_iter()
                    .enumerate()
                    .filter_map(|(part, tile)| {
                        Some(TileTarget {
                            zone: host.zone(),
                            part,
                            frame: tile.convertRect_toView(tile.bounds(), None),
                            view: tile.active_view()?,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Положение делителя по фактическим рамкам элементов сплита: у сплита рядом — правый край
    /// левого вида, у сплита одна над другой (`NSSplitView` перевёрнут) — нижний край верхнего.
    pub fn divider(&self, divider: Divider) -> f64 {
        let (split, index): (&NSSplitView, usize) = match divider {
            Divider::LeftCenter => (&self.root_split, 0),
            Divider::CenterRight => (&self.root_split, 1),
            Divider::CenterBottom => (&self.column_split, 0),
        };
        let subviews = split.subviews();
        if index >= subviews.count() {
            return 0.0;
        }
        let frame = subviews.objectAtIndex(index).frame();
        if split.isVertical() {
            frame.origin.x + frame.size.width
        } else {
            frame.origin.y + frame.size.height
        }
    }

    pub fn set_divider(&self, divider: Divider, position: f64) {
        let (split, index): (&NSSplitView, isize) = match divider {
            Divider::LeftCenter => (&self.root_split, 0),
            Divider::CenterRight => (&self.root_split, 1),
            Divider::CenterBottom => (&self.column_split, 0),
        };
        split.setPosition_ofDividerAtIndex(position, index);
    }

    /// Рамки всех частей в координатах окна.
    pub fn describe(&self) -> Vec<PartInfo> {
        let rect = |frame: NSRect| {
            [
                frame.origin.x,
                frame.origin.y,
                frame.size.width,
                frame.size.height,
            ]
        };
        self.hosts
            .iter()
            .flat_map(|host| {
                host.tiles()
                    .into_iter()
                    .enumerate()
                    .map(|(part, tile)| {
                        let content = tile.content();
                        PartInfo {
                            zone: host.zone(),
                            part,
                            frame: rect(tile.convertRect_toView(tile.bounds(), None)),
                            content: rect(content.convertRect_toView(content.bounds(), None)),
                            tabs: tile.titles(),
                            focused: tile.is_focused(),
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

fn plain_item(mtm: MainThreadMarker, view: &NSView) -> Retained<NSSplitViewItem> {
    let controller = NSViewController::new(mtm);
    controller.setView(view);
    NSSplitViewItem::splitViewItemWithViewController(&controller)
}

fn configure(item: &NSSplitViewItem, min: f64, holding: f32) {
    item.setMinimumThickness(min);
    item.setHoldingPriority(holding);
    item.setCanCollapse(false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ghostty::surface::{Decision, Phase, Registry};

    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
    }

    #[test]
    fn zones_split_along_their_own_axis() {
        assert_eq!(Zone::Left.axis(), Axis::Stacked);
        assert_eq!(Zone::Right.axis(), Axis::Stacked);
        assert_eq!(Zone::Center.axis(), Axis::SideBySide);
        assert_eq!(Zone::Bottom.axis(), Axis::SideBySide);
        let mut names: Vec<&str> = Zone::ALL.iter().map(|zone| zone.name()).collect();
        names.dedup();
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn parts_share_the_zone_with_gaps() {
        let zone = rect(0.0, 0.0, 236.0, 708.0);
        assert_eq!(part_frames(zone, Axis::Stacked, 1, GAP), [zone]);
        assert!(part_frames(zone, Axis::Stacked, 0, GAP).is_empty());
        let stacked = part_frames(zone, Axis::Stacked, 2, GAP);
        assert_eq!(stacked[0], rect(0.0, 357.0, 236.0, 351.0), "первая сверху");
        assert_eq!(stacked[1], rect(0.0, 0.0, 236.0, 351.0));
        let side = part_frames(rect(10.0, 5.0, 566.0, 220.0), Axis::SideBySide, 2, GAP);
        assert_eq!(side[0], rect(10.0, 5.0, 280.0, 220.0), "первая слева");
        assert_eq!(side[1], rect(296.0, 5.0, 280.0, 220.0));
        let crowded = part_frames(rect(0.0, 0.0, 4.0, 100.0), Axis::SideBySide, 3, GAP);
        assert!(crowded.iter().all(|frame| frame.size.width == 0.0));
    }

    #[test]
    fn zone_geometry_to_content_pixels_with_scale() {
        let part = part_frames(rect(0.0, 0.0, 236.0, 708.0), Axis::Stacked, 2, GAP)[0];
        let content = content_frame(part.size);
        assert_eq!(content, rect(0.0, 0.0, 236.0, 317.0));
        assert_eq!(to_pixels(content.size, 2.0), (472, 634));
        assert_eq!(to_pixels(content.size, 1.0), (236, 317));
        assert_eq!(
            to_pixels(content.size, 1.5),
            (354, 475),
            "дробная часть отброшена"
        );
        assert_eq!(content_frame(NSSize::new(100.0, 20.0)).size.height, 0.0);
        assert_eq!(to_pixels(NSSize::new(0.0, 317.0), 2.0), (0, 634));
        assert_eq!(to_pixels(NSSize::new(-5.0, f64::NAN), 2.0), (0, 0));
        assert_eq!(to_pixels(NSSize::new(10.0, 10.0), f64::INFINITY), (0, 0));
    }

    /// Решение «создавать или ждать» по геометрии части: схлопнутая зона ждёт, обычная создаёт,
    /// живая поверхность при нулевых промежуточных границах второй раз не создаётся.
    #[test]
    fn creation_waits_for_nonzero_zone_pixels() {
        let mut registry = Registry::<u32>::default();
        let id = registry.add();
        let collapsed = to_pixels(content_frame(NSSize::new(0.0, 708.0)).size, 2.0);
        assert_eq!(registry.decide(id, collapsed), Ok(Decision::Wait));
        let short = to_pixels(content_frame(NSSize::new(236.0, TAB_BAR)).size, 2.0);
        assert_eq!(
            registry.decide(id, short),
            Ok(Decision::Wait),
            "только полоса"
        );
        let center = to_pixels(content_frame(NSSize::new(560.0, 482.0)).size, 2.0);
        let Ok(Decision::Create(key)) = registry.decide(id, center) else {
            panic!("ожидалось создание");
        };
        registry.begin(key).unwrap();
        registry.created(key, 1).unwrap();
        assert_eq!(
            registry.decide(id, (0, 0)),
            Ok(Decision::Exists(Phase::Live)),
            "нулевые промежуточные границы"
        );
        assert_eq!(registry.created_count(), 1);
        assert_eq!(registry.freed_count(), 0);
    }

    #[test]
    fn focus_tone_is_opaque_mix_of_mode_color() {
        let light = Palette::LIGHT;
        assert_eq!(light.strip(false, Mode::Input), light.panel_head);
        assert_eq!(light.strip(false, Mode::Navigation), light.panel_head);
        assert_ne!(light.strip(true, Mode::Input), light.panel_head);
        assert_ne!(
            light.strip(true, Mode::Input),
            light.strip(true, Mode::Navigation),
            "режимы различаются тоном"
        );
        for palette in [Palette::LIGHT, Palette::DARK] {
            assert_eq!(palette.mode_color(Mode::Input), palette.input);
            assert_eq!(palette.mode_color(Mode::Navigation), palette.navigation);
        }
        assert_eq!(
            Rgb::hex(0x000000).mix(Rgb::hex(0xFFFFFF), 0.5),
            Rgb(128, 128, 128)
        );
        assert_eq!(
            Rgb::hex(0x102030).mix(Rgb::hex(0xFFFFFF), 0.0),
            Rgb(16, 32, 48)
        );
        assert_eq!(Palette::of(true), Palette::DARK);
    }
}
