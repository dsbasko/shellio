//! Геометрия раскладки: кадры зон, частей и панелей по размеру окна (ADR-0009, «Уточнено в P1.4 (планирование)»).
//!
//! Начало координат — левый верхний угол области содержимого окна, ось y направлена вниз. Все кадры
//! в целых точках: остаток деления зоны получает последняя часть.

use serde::{Deserialize, Serialize};

use crate::model::{Axis, Layout, MinSize, Zone, ZoneState};

/// Размер области содержимого окна в точках. Состояние запуска, а не раскладки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

/// Метрики оформления, нужные геометрии. Ядро и адаптер заполняют их из темы.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutMetrics {
    /// Высота заголовка окна вместе с его линией.
    pub titlebar: u32,
    /// Высота строки вкладок вместе с линией под ней.
    pub tabbar: u32,
    /// Промежуток между плитками и от края области зон.
    pub gap: u32,
    /// Толщина линии плитки.
    pub line: u32,
    /// Высота строки статуса вместе с её линией.
    pub status_bar: u32,
    /// Ширина полосы края тела части, где перетаскивание вкладки создаёт новую часть.
    pub drop_edge: u32,
    /// Отступ тела колонки навигатора от левого и нижнего края; сверху и справа — `gap`.
    pub side_pad: u32,
}

/// Кадр в точках.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    #[must_use]
    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    #[must_use]
    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    /// Точка внутри кадра: левая и верхняя граница включены, правая и нижняя — нет.
    #[must_use]
    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x && point.x < self.right() && point.y >= self.y && point.y < self.bottom()
    }
}

/// Размер в точках.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    pub const ZERO: Self = Self {
        width: 0.0,
        height: 0.0,
    };

    #[must_use]
    pub fn of(rect: Rect) -> Self {
        Self {
            width: rect.width,
            height: rect.height,
        }
    }
}

/// Точка в координатах области содержимого окна.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// Класс размера тела панели (пороги пробы P0.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeClass {
    Narrow,
    Regular,
    Wide,
    ShortBar,
}

impl SizeClass {
    /// `ShortBar` — высота меньше этой.
    pub const SHORT_BAR_BELOW: f64 = 240.0;
    /// `Narrow` — ширина меньше этой.
    pub const NARROW_BELOW: f64 = 480.0;
    /// `Wide` — ширина не меньше этой.
    pub const WIDE_FROM: f64 = 1100.0;

    /// Класс по размеру: `ShortBar` проверяется раньше ширины, сравнения строгие.
    #[must_use]
    pub fn of(width: f64, height: f64) -> Self {
        if height < Self::SHORT_BAR_BELOW {
            Self::ShortBar
        } else if width < Self::NARROW_BELOW {
            Self::Narrow
        } else if width >= Self::WIDE_FROM {
            Self::Wide
        } else {
            Self::Regular
        }
    }
}

/// Геометрия тела панели, которую получает `adapt`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    pub zone: Zone,
    pub size: SizeClass,
    pub width: f64,
    pub height: f64,
}

impl Geometry {
    #[must_use]
    pub fn of(zone: Zone, width: f64, height: f64) -> Self {
        Self {
            zone,
            size: SizeClass::of(width, height),
            width,
            height,
        }
    }
}

/// Кадры зон. `None` — зона не показана.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneFrames {
    /// Колонка навигатора.
    pub navigator: Option<Rect>,
    /// Плитка левой зоны.
    pub left: Option<Rect>,
    /// Столбец центра и нижней зоны.
    pub column: Option<Rect>,
    pub center: Option<Rect>,
    pub bottom: Option<Rect>,
    pub right: Option<Rect>,
}

/// Колонка навигатора: плоская, без плитки и строки вкладок.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NavigatorFrame<P> {
    pub panel: P,
    pub frame: Rect,
    pub body: Rect,
    pub geometry: Geometry,
    /// Тело меньше `min_size` панели: показывается заглушка «Развернуть».
    pub stub: bool,
}

/// Плитка части: строка вкладок и тело активной панели.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartFrame<P> {
    pub zone: Zone,
    pub index: usize,
    pub frame: Rect,
    pub tab_strip: Rect,
    pub body: Rect,
    pub tabs: Vec<P>,
    pub active: P,
    pub geometry: Geometry,
    /// Тело меньше `min_size` активной панели: показывается заглушка «Развернуть».
    pub stub: bool,
}

/// Кадры раскладки при данном размере окна.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frames<P> {
    /// Область зон: под заголовком и над строкой статуса.
    pub area: Rect,
    pub status: Rect,
    pub zones: ZoneFrames,
    pub navigator: Option<NavigatorFrame<P>>,
    /// Части показанных зон слева направо: плитка левой зоны, центр, нижняя зона, правая зона.
    pub parts: Vec<PartFrame<P>>,
    /// Развёрнутая панель: одна плитка на всю область зон, `zone` — `Center`, `index` — 0.
    pub zoomed: Option<PartFrame<P>>,
    /// Место сообщения «Нет открытых панелей»: вне колонки навигатора панелей нет.
    pub no_panels: Option<Rect>,
}

impl<P: PartialEq> Frames<P> {
    /// Показанная часть, среди вкладок которой есть панель.
    #[must_use]
    pub fn part_of(&self, panel: &P) -> Option<&PartFrame<P>> {
        self.parts.iter().find(|part| part.tabs.contains(panel))
    }
}

/// Сторона части вдоль оси зоны.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    /// Левее или выше.
    Before,
    /// Правее или ниже.
    After,
}

/// Куда ляжет перетаскиваемая вкладка.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropTarget<P> {
    /// В часть якоря последней вкладкой.
    Part { anchor: P },
    /// В новую часть рядом с частью якоря.
    NewPart { anchor: P, edge: Edge },
    /// Отмена.
    None,
}

/// Кадр в целых точках с неотрицательным размером.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

impl Area {
    fn new(x: i64, y: i64, w: i64, h: i64) -> Self {
        Self {
            x,
            y,
            w: w.max(0),
            h: h.max(0),
        }
    }

    fn inset(self, left: i64, top: i64, right: i64, bottom: i64) -> Self {
        Self::new(
            self.x + left.min(self.w),
            self.y + top.min(self.h),
            self.w - left - right,
            self.h - top - bottom,
        )
    }

    fn rect(self) -> Rect {
        Rect {
            x: self.x as f64,
            y: self.y as f64,
            width: self.w as f64,
            height: self.h as f64,
        }
    }
}

/// Кадры раскладки по размеру окна.
///
/// Без `zoom` колонка навигатора стоит у левого края окна, остальные зоны — в области правее неё
/// с промежутком `gap` от краёв. Плитка левой и правая зоны получают свои размеры, столбец центра —
/// остаток. Пустой центр отдаёт высоту столбца нижней зоне; если пусты оба, столбца нет, и его
/// ширину получает правая зона, иначе плитка левой. Когда окна не хватает, место выдаётся по порядку:
/// колонка навигатора, плитка, правая зона, нижняя зона, каждая получает не больше остатка, а центр —
/// остаток не меньше 0. Раскладка при этом не меняется.
pub fn geometry<P: Copy + Ord>(
    layout: &Layout<P>,
    viewport: Viewport,
    metrics: &LayoutMetrics,
    min_size: impl Fn(P) -> MinSize,
) -> Frames<P> {
    let width = i64::from(viewport.width);
    let height = i64::from(viewport.height);
    let titlebar = i64::from(metrics.titlebar).min(height);
    let status_bar = i64::from(metrics.status_bar).min(height - titlebar);
    let area = Area::new(0, titlebar, width, height - titlebar - status_bar);
    let status = Area::new(0, height - status_bar, width, status_bar);
    let gap = i64::from(metrics.gap);

    let mut frames = Frames {
        area: area.rect(),
        status: status.rect(),
        zones: ZoneFrames::default(),
        navigator: None,
        parts: Vec::new(),
        zoomed: None,
        no_panels: None,
    };

    if let Some(panel) = layout.zoomed_panel() {
        let frame = area.inset(gap, gap, gap, gap);
        frames.zoomed = Some(plate(
            Zone::Center,
            0,
            frame,
            vec![panel],
            panel,
            metrics,
            &min_size,
        ));
        return frames;
    }

    let navigator = layout.navigator.filter(|_| layout.navigator_shown());
    let column_width = navigator.map_or(0, |_| i64::from(layout.sizes.navigator).min(width));
    if let Some(panel) = navigator {
        let frame = Area::new(0, area.y, column_width, area.h);
        let side = i64::from(metrics.side_pad);
        let body = frame.inset(side, gap, gap, side);
        frames.zones.navigator = Some(frame.rect());
        frames.navigator = Some(NavigatorFrame {
            panel,
            frame: frame.rect(),
            body: body.rect(),
            geometry: Geometry::of(Zone::Left, body.w as f64, body.h as f64),
            stub: is_stub(body, min_size(panel)),
        });
    }

    let pad_left = if navigator.is_some() { 0 } else { gap };
    let region = Area::new(
        column_width + pad_left,
        area.y + gap,
        width - column_width - pad_left - gap,
        area.h - 2 * gap,
    );

    let center_shown = layout.zone_shown(Zone::Center);
    let bottom_shown = layout.zone_shown(Zone::Bottom);
    let column_shown = center_shown || bottom_shown;

    let mut slots: Vec<(Slot, Option<i64>)> = Vec::new();
    if layout.zone_shown(Zone::Left) {
        slots.push((Slot::Plate, Some(i64::from(layout.sizes.plate))));
    }
    if column_shown {
        slots.push((Slot::Column, None));
    }
    if layout.zone_shown(Zone::Right) {
        slots.push((Slot::Right, Some(i64::from(layout.sizes.right))));
    }
    if !column_shown && let Some(last) = slots.last_mut() {
        last.1 = None;
    }

    let sizes: Vec<Option<i64>> = slots.iter().map(|&(_, size)| size).collect();
    let mut x = region.x;
    for (&(slot, _), len) in slots.iter().zip(allocate(region.w, &sizes, gap)) {
        let rect = Area::new(x, region.y, len, region.h);
        x += len + gap;
        match slot {
            Slot::Plate => {
                frames.zones.left = Some(rect.rect());
                place_parts(
                    Zone::Left,
                    rect,
                    &layout.zones.left,
                    metrics,
                    &min_size,
                    &mut frames.parts,
                );
            }
            Slot::Column => {
                frames.zones.column = Some(rect.rect());
                let (center, bottom) = match (center_shown, bottom_shown) {
                    (true, true) => {
                        let bottom = i64::from(layout.sizes.bottom).min((rect.h - gap).max(0));
                        let center = rect.h - gap - bottom;
                        (
                            Some(Area::new(rect.x, rect.y, rect.w, center)),
                            Some(Area::new(rect.x, rect.y + center + gap, rect.w, bottom)),
                        )
                    }
                    (true, false) => (Some(rect), None),
                    (false, true) => (None, Some(rect)),
                    (false, false) => (None, None),
                };
                if let Some(center) = center {
                    frames.zones.center = Some(center.rect());
                    place_parts(
                        Zone::Center,
                        center,
                        &layout.zones.center,
                        metrics,
                        &min_size,
                        &mut frames.parts,
                    );
                }
                if let Some(bottom) = bottom {
                    frames.zones.bottom = Some(bottom.rect());
                    place_parts(
                        Zone::Bottom,
                        bottom,
                        &layout.zones.bottom,
                        metrics,
                        &min_size,
                        &mut frames.parts,
                    );
                }
            }
            Slot::Right => {
                frames.zones.right = Some(rect.rect());
                place_parts(
                    Zone::Right,
                    rect,
                    &layout.zones.right,
                    metrics,
                    &min_size,
                    &mut frames.parts,
                );
            }
        }
    }

    if layout.zones.iter().all(|(_, state)| state.is_empty()) {
        frames.no_panels = Some(region.rect());
    }
    frames
}

/// Место в горизонтальном ряду области зон.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Plate,
    Column,
    Right,
}

/// Длины мест ряда: фиксированные по порядку урезаются до остатка, гибкое (`None`) получает остаток.
fn allocate(total: i64, sizes: &[Option<i64>], gap: i64) -> Vec<i64> {
    let gaps = gap * sizes.len().saturating_sub(1) as i64;
    let mut remaining = (total - gaps).max(0);
    let mut lengths: Vec<i64> = sizes
        .iter()
        .map(|size| match size {
            Some(size) => {
                let len = (*size).min(remaining);
                remaining -= len;
                len
            }
            None => 0,
        })
        .collect();
    if let Some(flex) = sizes.iter().position(Option::is_none) {
        lengths[flex] = remaining;
    }
    lengths
}

fn place_parts<P: Copy>(
    zone: Zone,
    rect: Area,
    state: &ZoneState<P>,
    metrics: &LayoutMetrics,
    min_size: &impl Fn(P) -> MinSize,
    out: &mut Vec<PartFrame<P>>,
) {
    let count = state.parts.len();
    if count == 0 {
        return;
    }
    let gap = i64::from(metrics.gap);
    let along = match zone.axis() {
        Axis::SideBySide => rect.w,
        Axis::Stacked => rect.h,
    };
    let available = (along - gap * (count as i64 - 1)).max(0);
    let total = state
        .parts
        .iter()
        .map(|part| i64::from(part.share))
        .sum::<i64>()
        .max(1);
    let mut offset = 0;
    let mut used = 0;
    for (index, part) in state.parts.iter().enumerate() {
        let len = if index + 1 == count {
            available - used
        } else {
            available * i64::from(part.share) / total
        };
        used += len;
        let frame = match zone.axis() {
            Axis::SideBySide => Area::new(rect.x + offset, rect.y, len, rect.h),
            Axis::Stacked => Area::new(rect.x, rect.y + offset, rect.w, len),
        };
        offset += len + gap;
        if let Some(active) = part.active_panel() {
            out.push(plate(
                zone,
                index,
                frame,
                part.tabs.clone(),
                active,
                metrics,
                min_size,
            ));
        }
    }
}

fn plate<P: Copy>(
    zone: Zone,
    index: usize,
    frame: Area,
    tabs: Vec<P>,
    active: P,
    metrics: &LayoutMetrics,
    min_size: &impl Fn(P) -> MinSize,
) -> PartFrame<P> {
    let line = i64::from(metrics.line);
    let inner = frame.inset(line, line, line, line);
    let tab_strip = Area::new(
        inner.x,
        inner.y,
        inner.w,
        i64::from(metrics.tabbar).min(inner.h),
    );
    let body = Area::new(
        inner.x,
        inner.y + tab_strip.h,
        inner.w,
        inner.h - tab_strip.h,
    );
    PartFrame {
        zone,
        index,
        frame: frame.rect(),
        tab_strip: tab_strip.rect(),
        body: body.rect(),
        tabs,
        active,
        geometry: Geometry::of(zone, body.w as f64, body.h as f64),
        stub: is_stub(body, min_size(active)),
    }
}

fn is_stub(body: Area, min: MinSize) -> bool {
    !min.admits(body.w as f64, body.h as f64)
}

/// Цель перетаскивания вкладки в точке: строка вкладок, линия плитки и середина тела — часть,
/// полоса края тела вдоль оси зоны — новая часть с этой стороны, иначе отмена. Полоса не шире
/// половины тела. Колонка навигатора и развёрнутая панель целями не бывают.
pub fn drop_target<P: Copy>(
    frames: &Frames<P>,
    point: Point,
    metrics: &LayoutMetrics,
) -> DropTarget<P> {
    let Some(part) = frames.parts.iter().find(|part| part.frame.contains(point)) else {
        return DropTarget::None;
    };
    let anchor = part.active;
    if part.body.contains(point) {
        let (start, end, at) = match part.zone.axis() {
            Axis::SideBySide => (part.body.x, part.body.right(), point.x),
            Axis::Stacked => (part.body.y, part.body.bottom(), point.y),
        };
        let band = f64::from(metrics.drop_edge).min((end - start) / 2.0);
        if at < start + band {
            return DropTarget::NewPart {
                anchor,
                edge: Edge::Before,
            };
        }
        if at >= end - band {
            return DropTarget::NewPart {
                anchor,
                edge: Edge::After,
            };
        }
    }
    DropTarget::Part { anchor }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Part, SHARE_TOTAL};

    const METRICS: LayoutMetrics = LayoutMetrics {
        titlebar: 40,
        tabbar: 34,
        gap: 6,
        line: 1,
        status_bar: 26,
        drop_edge: 24,
        side_pad: 8,
    };
    const SMALL: Viewport = Viewport {
        width: 1180,
        height: 720,
    };
    const LARGE: Viewport = Viewport {
        width: 1440,
        height: 900,
    };

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn frames(layout: &Layout<u32>, viewport: Viewport) -> Frames<u32> {
        geometry(layout, viewport, &METRICS, |_| MinSize::DEFAULT)
    }

    /// 1 — навигатор, 2 — терминал в центре.
    fn base() -> Layout<u32> {
        Layout::default_for(1, 2)
    }

    fn parts(shares: &[u16], first: u32) -> Vec<Part<u32>> {
        shares
            .iter()
            .zip(first..)
            .map(|(&share, panel)| Part::single(panel, share))
            .collect()
    }

    fn with_right(mut layout: Layout<u32>) -> Layout<u32> {
        layout.zones.right.parts = vec![Part::single(40, SHARE_TOTAL)];
        layout
    }

    fn frames_of(frames: &Frames<u32>, zone: Zone) -> Vec<Rect> {
        frames
            .parts
            .iter()
            .filter(|part| part.zone == zone)
            .map(|part| part.frame)
            .collect()
    }

    #[test]
    fn size_class_thresholds_are_strict_and_short_bar_goes_first() {
        assert_eq!(SizeClass::of(479.0, 300.0), SizeClass::Narrow);
        assert_eq!(SizeClass::of(480.0, 300.0), SizeClass::Regular);
        assert_eq!(SizeClass::of(1099.0, 300.0), SizeClass::Regular);
        assert_eq!(SizeClass::of(1100.0, 300.0), SizeClass::Wide);
        assert_eq!(SizeClass::of(2000.0, 239.0), SizeClass::ShortBar);
        assert_eq!(SizeClass::of(2000.0, 240.0), SizeClass::Wide);
        assert_eq!(SizeClass::of(100.0, 100.0), SizeClass::ShortBar);
    }

    #[test]
    fn default_layout_small_window() {
        let f = frames(&base(), SMALL);
        assert_eq!(f.area, rect(0.0, 40.0, 1180.0, 654.0));
        assert_eq!(f.status, rect(0.0, 694.0, 1180.0, 26.0));

        let nav = f.navigator.as_ref().unwrap();
        assert_eq!(nav.panel, 1);
        assert_eq!(nav.frame, rect(0.0, 40.0, 236.0, 654.0));
        assert_eq!(nav.body, rect(8.0, 46.0, 222.0, 640.0));
        assert_eq!(nav.geometry.zone, Zone::Left);
        assert_eq!(nav.geometry.size, SizeClass::Narrow);
        assert!(!nav.stub);

        assert_eq!(f.parts.len(), 1);
        let center = &f.parts[0];
        assert_eq!(
            (center.zone, center.index, center.active),
            (Zone::Center, 0, 2)
        );
        assert_eq!(center.frame, rect(236.0, 46.0, 938.0, 642.0));
        assert_eq!(center.tab_strip, rect(237.0, 47.0, 936.0, 34.0));
        assert_eq!(center.body, rect(237.0, 81.0, 936.0, 606.0));
        assert_eq!(
            center.geometry,
            Geometry {
                zone: Zone::Center,
                size: SizeClass::Regular,
                width: 936.0,
                height: 606.0
            }
        );
        assert!(!center.stub);

        assert_eq!(
            f.zones,
            ZoneFrames {
                navigator: Some(rect(0.0, 40.0, 236.0, 654.0)),
                left: None,
                column: Some(rect(236.0, 46.0, 938.0, 642.0)),
                center: Some(rect(236.0, 46.0, 938.0, 642.0)),
                bottom: None,
                right: None,
            }
        );
        assert_eq!(f.zoomed, None);
        assert_eq!(f.no_panels, None);
    }

    #[test]
    fn default_layout_large_window_is_wide() {
        let f = frames(&base(), LARGE);
        assert_eq!(f.navigator.as_ref().unwrap().frame.width, 236.0);
        let center = &f.parts[0];
        assert_eq!(center.frame, rect(236.0, 46.0, 1198.0, 822.0));
        assert_eq!(center.body, rect(237.0, 81.0, 1196.0, 786.0));
        assert_eq!(center.geometry.size, SizeClass::Wide);
    }

    #[test]
    fn right_zone_leaves_about_560_for_the_center_and_930_when_hidden() {
        let mut layout = with_right(base());
        let f = frames(&layout, SMALL);
        assert_eq!(
            frames_of(&f, Zone::Center),
            vec![rect(236.0, 46.0, 560.0, 642.0)]
        );
        assert_eq!(
            frames_of(&f, Zone::Right),
            vec![rect(802.0, 46.0, 372.0, 642.0)]
        );
        let right = f.part_of(&40).unwrap();
        assert_eq!(right.geometry.size, SizeClass::Narrow);

        layout.hidden.right = true;
        let f = frames(&layout, SMALL);
        assert_eq!(
            frames_of(&f, Zone::Center),
            vec![rect(236.0, 46.0, 938.0, 642.0)]
        );
        assert!(frames_of(&f, Zone::Right).is_empty());
        assert_eq!(f.zones.right, None);
        assert_eq!(layout.sizes.right, 372, "скрытие сохраняет размер");
    }

    #[test]
    fn halves_of_560_center_fit_and_quarters_do_not() {
        let mut layout = with_right(base());
        layout.zones.center.parts = parts(&[500, 500], 2);
        let f = frames(&layout, SMALL);
        assert_eq!(
            frames_of(&f, Zone::Center),
            vec![
                rect(236.0, 46.0, 277.0, 642.0),
                rect(519.0, 46.0, 277.0, 642.0)
            ]
        );
        for part in f.parts.iter().filter(|part| part.zone == Zone::Center) {
            assert!(part.body.width >= 200.0);
            assert!(!part.stub);
            assert_eq!(part.geometry.size, SizeClass::Narrow);
        }

        layout.zones.center.parts = parts(&[250, 250, 500], 2);
        let f = frames(&layout, SMALL);
        let stubs: Vec<(f64, bool)> = f
            .parts
            .iter()
            .filter(|part| part.zone == Zone::Center)
            .map(|part| (part.body.width, part.stub))
            .collect();
        assert_eq!(stubs, vec![(135.0, true), (135.0, true), (272.0, false)]);
    }

    #[test]
    fn three_center_parts_fit_without_right_zone_and_stub_with_it() {
        let mut layout = with_right(base());
        layout.zones.center.parts = parts(&[333, 333, 334], 2);
        layout.hidden.right = true;
        let f = frames(&layout, SMALL);
        let wide: Vec<(f64, bool)> = f
            .parts
            .iter()
            .map(|part| (part.frame.width, part.stub))
            .collect();
        assert_eq!(wide, vec![(308.0, false), (308.0, false), (310.0, false)]);
        assert!(f.parts.iter().all(|part| part.body.width > 200.0));

        layout.hidden.right = false;
        let f = frames(&layout, SMALL);
        let tight: Vec<(f64, bool)> = f
            .parts
            .iter()
            .filter(|part| part.zone == Zone::Center)
            .map(|part| (part.body.width, part.stub))
            .collect();
        assert_eq!(tight, vec![(180.0, true), (180.0, true), (182.0, true)]);
    }

    #[test]
    fn uneven_shares_fill_the_zone_exactly_and_last_part_takes_the_remainder() {
        let mut layout = base();
        layout.zones.center.parts = parts(&[123, 456, 421], 2);
        for viewport in [
            SMALL,
            LARGE,
            Viewport {
                width: 1301,
                height: 777,
            },
        ] {
            let f = frames(&layout, viewport);
            let zone = f.zones.center.unwrap();
            let center = frames_of(&f, Zone::Center);
            let total: f64 = center.iter().map(|r| r.width).sum::<f64>() + 2.0 * 6.0;
            assert_eq!(total, zone.width);
            assert_eq!(center[0].x, zone.x);
            assert_eq!(center[2].right(), zone.right());
            assert_eq!(center[1].x, center[0].right() + 6.0);
        }
        let f = frames(&layout, SMALL);
        let widths: Vec<f64> = frames_of(&f, Zone::Center)
            .iter()
            .map(|r| r.width)
            .collect();
        assert_eq!(widths, vec![113.0, 422.0, 391.0]);
    }

    #[test]
    fn bottom_zone_sits_under_the_center_as_a_short_bar() {
        let mut layout = base();
        layout.zones.bottom.parts = vec![Part::single(30, SHARE_TOTAL)];
        let f = frames(&layout, SMALL);
        assert_eq!(f.zones.center, Some(rect(236.0, 46.0, 938.0, 400.0)));
        assert_eq!(f.zones.bottom, Some(rect(236.0, 452.0, 938.0, 236.0)));
        let bottom = f.part_of(&30).unwrap();
        assert_eq!(bottom.body.height, 200.0);
        assert_eq!(bottom.geometry.size, SizeClass::ShortBar);

        layout.hidden.bottom = true;
        let f = frames(&layout, SMALL);
        assert_eq!(f.zones.bottom, None);
        assert_eq!(f.zones.center, Some(rect(236.0, 46.0, 938.0, 642.0)));
    }

    #[test]
    fn empty_center_gives_the_column_height_to_the_bottom_zone() {
        let mut layout = base();
        layout.zones.center.parts.clear();
        layout.zones.bottom.parts = parts(&[500, 500], 30);
        for (viewport, column) in [
            (SMALL, rect(236.0, 46.0, 938.0, 642.0)),
            (LARGE, rect(236.0, 46.0, 1198.0, 822.0)),
        ] {
            let f = frames(&layout, viewport);
            assert_eq!(f.zones.center, None);
            assert_eq!(f.zones.column, Some(column));
            assert_eq!(f.zones.bottom, Some(column));
            assert_eq!(f.no_panels, None);
        }
        assert_eq!(layout.sizes.bottom, 236);
    }

    #[test]
    fn empty_center_and_bottom_give_the_column_width_to_the_nearest_side_plate() {
        let mut layout = base();
        layout.zones.center.parts.clear();
        layout.zones.right.parts = vec![Part::single(40, SHARE_TOTAL)];
        let f = frames(&layout, SMALL);
        assert_eq!(f.zones.column, None);
        assert_eq!(f.zones.right, Some(rect(236.0, 46.0, 938.0, 642.0)));

        layout.zones.left.parts = vec![Part::single(10, SHARE_TOTAL)];
        let f = frames(&layout, SMALL);
        assert_eq!(f.zones.left, Some(rect(236.0, 46.0, 372.0, 642.0)));
        assert_eq!(f.zones.right, Some(rect(614.0, 46.0, 560.0, 642.0)));

        layout.hidden.right = true;
        let f = frames(&layout, SMALL);
        assert_eq!(f.zones.left, Some(rect(236.0, 46.0, 938.0, 642.0)));
        assert_eq!(f.zones.right, None);
        assert_eq!(layout.sizes, crate::model::ZoneSizes::DEFAULT);
    }

    #[test]
    fn no_panels_message_only_when_nothing_is_outside_the_navigator() {
        let mut layout = base();
        layout.zones.center.parts.clear();
        let f = frames(&layout, SMALL);
        assert!(f.parts.is_empty());
        assert_eq!(f.no_panels, Some(rect(236.0, 46.0, 938.0, 642.0)));

        layout.zones.right.parts = vec![Part::single(40, SHARE_TOTAL)];
        layout.hidden.right = true;
        let f = frames(&layout, SMALL);
        assert!(f.parts.is_empty());
        assert_eq!(f.no_panels, None);
    }

    #[test]
    fn all_zones_open_squeeze_the_center_into_a_stub() {
        let mut layout = with_right(base());
        layout.zones.left.parts = vec![Part::single(10, SHARE_TOTAL)];
        layout.zones.bottom.parts = vec![Part::single(30, SHARE_TOTAL)];
        let f = frames(&layout, SMALL);
        assert_eq!(f.zones.left, Some(rect(236.0, 46.0, 372.0, 642.0)));
        assert_eq!(f.zones.column, Some(rect(614.0, 46.0, 182.0, 642.0)));
        assert_eq!(f.zones.right, Some(rect(802.0, 46.0, 372.0, 642.0)));
        assert!(f.part_of(&2).unwrap().stub);
        assert!(!f.part_of(&40).unwrap().stub);
        let order: Vec<Zone> = f.parts.iter().map(|part| part.zone).collect();
        assert_eq!(
            order,
            vec![Zone::Left, Zone::Center, Zone::Bottom, Zone::Right]
        );
    }

    #[test]
    fn hidden_navigator_moves_zones_to_the_window_edge() {
        let mut layout = base();
        layout.hidden.navigator = true;
        let f = frames(&layout, SMALL);
        assert_eq!(f.navigator, None);
        assert_eq!(f.zones.navigator, None);
        assert_eq!(f.zones.center, Some(rect(6.0, 46.0, 1168.0, 642.0)));
        assert_eq!(layout.sizes.navigator, 236);
    }

    #[test]
    fn only_bottom_takes_the_whole_area_and_keeps_its_parts() {
        let mut layout = with_right(base());
        layout.zones.left.parts = vec![Part::single(10, SHARE_TOTAL)];
        layout.zones.bottom.parts = parts(&[500, 500], 30);
        layout.only = Some(Zone::Bottom);
        for (viewport, zone, halves) in [
            (
                SMALL,
                rect(6.0, 46.0, 1168.0, 642.0),
                vec![
                    rect(6.0, 46.0, 581.0, 642.0),
                    rect(593.0, 46.0, 581.0, 642.0),
                ],
            ),
            (
                LARGE,
                rect(6.0, 46.0, 1428.0, 822.0),
                vec![
                    rect(6.0, 46.0, 711.0, 822.0),
                    rect(723.0, 46.0, 711.0, 822.0),
                ],
            ),
        ] {
            let f = frames(&layout, viewport);
            assert_eq!(f.navigator, None);
            assert_eq!(
                f.zones,
                ZoneFrames {
                    navigator: None,
                    left: None,
                    column: Some(zone),
                    center: None,
                    bottom: Some(zone),
                    right: None,
                }
            );
            assert_eq!(frames_of(&f, Zone::Bottom), halves);
            assert_eq!(f.parts.len(), 2);
        }
        assert_eq!(layout.hidden, crate::model::Hidden::default());
    }

    #[test]
    fn only_left_shows_the_navigator_column_and_the_plate() {
        let mut layout = with_right(base());
        layout.zones.left.parts = vec![Part::single(10, SHARE_TOTAL)];
        layout.only = Some(Zone::Left);
        let f = frames(&layout, SMALL);
        assert_eq!(f.zones.navigator, Some(rect(0.0, 40.0, 236.0, 654.0)));
        assert_eq!(f.zones.left, Some(rect(236.0, 46.0, 938.0, 642.0)));
        assert_eq!(f.zones.column, None);
        assert_eq!(f.zones.right, None);
    }

    #[test]
    fn zoom_covers_the_area_with_center_geometry() {
        let mut layout = with_right(base());
        layout.zones.center.parts = parts(&[500, 500], 2);
        layout.zoomed = Some(40);
        let f = frames(&layout, SMALL);
        assert_eq!(f.navigator, None);
        assert!(f.parts.is_empty());
        assert_eq!(f.zones, ZoneFrames::default());
        let zoomed = f.zoomed.as_ref().unwrap();
        assert_eq!((zoomed.zone, zoomed.index), (Zone::Center, 0));
        assert_eq!((zoomed.tabs.clone(), zoomed.active), (vec![40], 40));
        assert_eq!(zoomed.frame, rect(6.0, 46.0, 1168.0, 642.0));
        assert_eq!(zoomed.body, rect(7.0, 81.0, 1166.0, 606.0));
        assert_eq!(zoomed.geometry.zone, Zone::Center);
        assert_eq!(zoomed.geometry.size, SizeClass::Wide);
        assert_eq!(layout.zones.center.parts.len(), 2, "zoom не меняет деления");

        let f = frames(&layout, LARGE);
        assert_eq!(f.zoomed.unwrap().frame, rect(6.0, 46.0, 1428.0, 822.0));
    }

    #[test]
    fn stub_follows_min_size_of_the_active_panel() {
        let mut layout = with_right(base());
        layout.zones.center.parts = parts(&[500, 500], 2);
        layout.zones.bottom.parts = vec![Part::single(30, SHARE_TOTAL)];
        let min = |panel: u32| match panel {
            3 => MinSize {
                width: 300.0,
                height: 120.0,
            },
            30 => MinSize {
                width: 200.0,
                height: 201.0,
            },
            _ => MinSize::DEFAULT,
        };
        let f = geometry(&layout, SMALL, &METRICS, min);
        assert!(!f.part_of(&2).unwrap().stub);
        assert!(f.part_of(&3).unwrap().stub);
        assert!(f.part_of(&30).unwrap().stub);

        let f = geometry(&layout, SMALL, &METRICS, |_| MinSize {
            width: 275.0,
            height: 200.0,
        });
        assert!(!f.part_of(&2).unwrap().stub, "граница min_size не заглушка");
        assert!(!f.part_of(&30).unwrap().stub);
    }

    #[test]
    fn tiny_windows_never_produce_negative_frames() {
        let mut layout = with_right(base());
        layout.zones.left.parts = parts(&[500, 500], 10);
        layout.zones.center.parts = parts(&[333, 333, 334], 2);
        layout.zones.bottom.parts = parts(&[500, 500], 30);
        layout.sizes.right = 5000;
        let non_negative = |r: &Rect| r.width >= 0.0 && r.height >= 0.0;
        for (width, height) in [(0, 0), (1, 1), (50, 30), (300, 200), (700, 90)] {
            let f = frames(&layout, Viewport { width, height });
            assert!(non_negative(&f.area) && non_negative(&f.status));
            let zones = [
                f.zones.navigator,
                f.zones.left,
                f.zones.column,
                f.zones.center,
                f.zones.bottom,
                f.zones.right,
            ];
            assert!(zones.iter().flatten().all(non_negative));
            let inside = |inner: &Rect, outer: &Rect| {
                inner.x >= outer.x
                    && inner.y >= outer.y
                    && inner.right() <= outer.right()
                    && inner.bottom() <= outer.bottom()
            };
            for part in &f.parts {
                assert!(non_negative(&part.frame));
                assert!(non_negative(&part.tab_strip) && inside(&part.tab_strip, &part.frame));
                assert!(non_negative(&part.body) && inside(&part.body, &part.frame));
            }
            if let Some(nav) = &f.navigator {
                assert!(non_negative(&nav.body) && inside(&nav.body, &nav.frame));
            }
        }
    }

    #[test]
    fn drop_target_side_by_side_zone() {
        let mut layout = with_right(base());
        layout.zones.center.parts = parts(&[500, 500], 2);
        let f = frames(&layout, SMALL);
        let at = |x: f64, y: f64| drop_target(&f, Point { x, y }, &METRICS);
        assert_eq!(at(300.0, 60.0), DropTarget::Part { anchor: 2 });
        assert_eq!(
            at(240.0, 60.0),
            DropTarget::Part { anchor: 2 },
            "строка вкладок — часть"
        );
        assert_eq!(at(374.0, 300.0), DropTarget::Part { anchor: 2 });
        assert_eq!(
            at(240.0, 300.0),
            DropTarget::NewPart {
                anchor: 2,
                edge: Edge::Before
            }
        );
        assert_eq!(
            at(260.9, 300.0),
            DropTarget::NewPart {
                anchor: 2,
                edge: Edge::Before
            }
        );
        assert_eq!(at(261.0, 300.0), DropTarget::Part { anchor: 2 });
        assert_eq!(at(487.9, 300.0), DropTarget::Part { anchor: 2 });
        assert_eq!(
            at(500.0, 300.0),
            DropTarget::NewPart {
                anchor: 2,
                edge: Edge::After
            }
        );
        assert_eq!(
            at(515.0, 300.0),
            DropTarget::None,
            "промежуток между частями"
        );
        assert_eq!(at(700.0, 300.0), DropTarget::Part { anchor: 3 });
        assert_eq!(at(100.0, 300.0), DropTarget::None, "колонка навигатора");
        assert_eq!(at(600.0, 20.0), DropTarget::None, "заголовок окна");
    }

    #[test]
    fn drop_target_stacked_zone_keeps_the_tab_strip_as_part() {
        let mut layout = base();
        layout.zones.right.parts = parts(&[500, 500], 40);
        let f = frames(&layout, SMALL);
        assert_eq!(
            frames_of(&f, Zone::Right),
            vec![
                rect(802.0, 46.0, 372.0, 318.0),
                rect(802.0, 370.0, 372.0, 318.0)
            ]
        );
        let at = |x: f64, y: f64| drop_target(&f, Point { x, y }, &METRICS);
        assert_eq!(at(900.0, 60.0), DropTarget::Part { anchor: 40 });
        assert_eq!(
            at(900.0, 85.0),
            DropTarget::NewPart {
                anchor: 40,
                edge: Edge::Before
            }
        );
        assert_eq!(at(900.0, 200.0), DropTarget::Part { anchor: 40 });
        assert_eq!(
            at(900.0, 350.0),
            DropTarget::NewPart {
                anchor: 40,
                edge: Edge::After
            }
        );
        assert_eq!(
            at(1160.0, 200.0),
            DropTarget::Part { anchor: 40 },
            "край поперёк оси"
        );
        assert_eq!(at(900.0, 380.0), DropTarget::Part { anchor: 41 });
    }

    #[test]
    fn drop_band_is_at_most_half_of_a_narrow_body() {
        let mut layout = with_right(base());
        layout.zones.center.parts = parts(&[50, 950], 2);
        let f = frames(&layout, SMALL);
        let narrow = f.part_of(&2).unwrap();
        assert!(narrow.body.width < 48.0);
        let middle = narrow.body.x + narrow.body.width / 2.0;
        let at = |x: f64| drop_target(&f, Point { x, y: 300.0 }, &METRICS);
        assert_eq!(
            at(middle - 0.5),
            DropTarget::NewPart {
                anchor: 2,
                edge: Edge::Before
            }
        );
        assert_eq!(
            at(middle),
            DropTarget::NewPart {
                anchor: 2,
                edge: Edge::After
            }
        );
    }

    #[test]
    fn zoom_has_no_drop_targets() {
        let mut layout = base();
        layout.zoomed = Some(2);
        let f = frames(&layout, SMALL);
        assert_eq!(
            drop_target(&f, Point { x: 600.0, y: 300.0 }, &METRICS),
            DropTarget::None
        );
    }

    #[test]
    fn frames_serialize_with_snake_case_spelling() {
        let f = frames(&base(), SMALL);
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(json["parts"][0]["geometry"]["size"], "regular");
        assert_eq!(json["parts"][0]["tab_strip"]["height"], 34.0);
        assert_eq!(json["no_panels"], serde_json::Value::Null);
        assert_eq!(
            serde_json::to_value(SizeClass::ShortBar).unwrap(),
            "short_bar"
        );
        let back: Frames<u32> = serde_json::from_value(json).unwrap();
        assert_eq!(back, f);
    }
}
