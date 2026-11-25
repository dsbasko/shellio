//! Операции над раскладкой (ADR-0009, «Уточнено в P1.4 (планирование)», и ADR-0031).
//!
//! Операция получает раскладку, прошедшую `validate`, и возвращает новую раскладку, которая снова его
//! проходит, или отказ. Раскладка, равная исходной, означает «ничего не изменилось»: ядро отвечает без
//! новой ревизии. Место проверяют только деление и изменение размера, им нужен размер окна.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::geometry::{Edge, LayoutMetrics, PartFrame, Rect, Size, Viewport, geometry};
use crate::model::{Axis, Layout, Location, MinSize, Part, SHARE_TOTAL, Zone, ZoneState};

/// Направление фокуса к соседней части.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Left,
    Down,
    Up,
    Right,
}

/// Что скрывается и показывается отдельно. Центр не скрывается.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hideable {
    Navigator,
    Plate,
    Right,
    Bottom,
}

impl Hideable {
    /// Зона цели: колонка навигатора и плитка относятся к левой.
    #[must_use]
    pub const fn zone(self) -> Zone {
        match self {
            Self::Navigator | Self::Plate => Zone::Left,
            Self::Right => Zone::Right,
            Self::Bottom => Zone::Bottom,
        }
    }
}

/// Измерение относительного размера.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Width,
    Height,
}

/// Размер `current`, сдвинутый на `delta`, не меньше нуля.
fn shifted(current: u32, delta: i32) -> u32 {
    u32::try_from((i64::from(current) + i64::from(delta)).max(0)).unwrap_or(u32::MAX)
}

/// Затронутая делением часть: активная панель, её `min_size` и тело, которое осталось бы ей.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Room<P> {
    pub panel: P,
    pub need: MinSize,
    pub have: Size,
}

/// Отказ операции.
#[derive(Clone, Debug, PartialEq)]
pub enum OpError<P> {
    /// Панели или якоря нет в раскладке.
    NotFound(P),
    /// Новая панель уже стоит в раскладке.
    AlreadyPresent(P),
    /// Операции нужна часть зоны, а панель стоит в колонке навигатора.
    NavigatorColumn(P),
    /// Деление одиночной вкладки без новой панели.
    SplitNeedsPanel(P),
    /// Затронутой части деления не хватит места. Перечислены обе затронутые части.
    SplitNoRoom(Vec<Room<P>>),
    /// У зоны нет частей.
    ZoneEmpty(Zone),
    /// Скрывать и показывать нечего.
    TargetEmpty(Hideable),
    /// Размер центра — остаток, своего размера у него нет.
    ZoneNotResizable(Zone),
    /// Панель стоит не в этой зоне.
    NotInZone { panel: P, zone: Zone },
    /// Размер вне границ, в единицах запроса: точки у зоны, тысячные у части.
    ResizeOutOfRange { min: u32, max: u32 },
}

impl<P: fmt::Debug> fmt::Display for OpError<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(p) => write!(f, "панели {p:?} нет в раскладке"),
            Self::AlreadyPresent(p) => write!(f, "панель {p:?} уже стоит в раскладке"),
            Self::NavigatorColumn(p) => write!(f, "панель {p:?} стоит в колонке навигатора"),
            Self::SplitNeedsPanel(p) => {
                write!(f, "у панели {p:?} одна вкладка: новой части нужна панель")
            }
            Self::SplitNoRoom(rooms) => {
                write!(f, "новой части не хватит места:")?;
                for room in rooms {
                    write!(
                        f,
                        " {:?} нужно {}×{}, есть {}×{};",
                        room.panel,
                        room.need.width,
                        room.need.height,
                        room.have.width,
                        room.have.height
                    )?;
                }
                Ok(())
            }
            Self::ZoneEmpty(zone) => write!(f, "у зоны {zone:?} нет частей"),
            Self::TargetEmpty(target) => write!(f, "{target:?}: нечего показывать"),
            Self::ZoneNotResizable(zone) => write!(f, "у зоны {zone:?} нет своего размера"),
            Self::NotInZone { panel, zone } => write!(f, "панель {panel:?} не в зоне {zone:?}"),
            Self::ResizeOutOfRange { min, max } => write!(f, "размер вне границ {min}..={max}"),
        }
    }
}

impl<P: fmt::Debug> std::error::Error for OpError<P> {}

impl<P: Copy + Ord> Layout<P> {
    /// Новая панель в последней активной части зоны, без зоны — в центре. Панель получает фокус.
    ///
    /// # Errors
    ///
    /// [`OpError::AlreadyPresent`].
    pub fn open(&self, panel: P, zone: Option<Zone>) -> Result<Self, OpError<P>> {
        if self.locate(panel).is_some() {
            return Err(OpError::AlreadyPresent(panel));
        }
        let mut next = self.clone();
        next.insert_into_last_active(zone.unwrap_or(Zone::Center), panel);
        next.focus_on(panel);
        next.settle(None);
        Ok(next)
    }

    /// Деление части панели вдоль оси зоны. Фокус сначала переходит в часть панели, `zoom` снимается.
    /// Без `open` активная вкладка переходит в новую часть правее или ниже, с `open` новая часть
    /// получает новую панель и фокус. Старая и новая часть делят долю старой поровну.
    ///
    /// # Errors
    ///
    /// [`OpError::NotFound`], [`OpError::NavigatorColumn`], [`OpError::AlreadyPresent`] для `open`,
    /// [`OpError::SplitNeedsPanel`], [`OpError::SplitNoRoom`].
    pub fn split(
        &self,
        panel: P,
        open: Option<P>,
        viewport: Viewport,
        metrics: &LayoutMetrics,
        min_size: impl Fn(P) -> MinSize,
    ) -> Result<Self, OpError<P>> {
        let Location::Part { zone, part, .. } = self.require(panel)? else {
            return Err(OpError::NavigatorColumn(panel));
        };
        if let Some(new) = open
            && self.locate(new).is_some()
        {
            return Err(OpError::AlreadyPresent(new));
        }
        let mut next = self.clone();
        next.zoomed = None;
        next.focus_on(panel);
        let moved = match open {
            Some(new) => new,
            None => {
                let source = &mut next.zones.get_mut(zone).parts[part];
                if source.tabs.len() == 1 {
                    return Err(OpError::SplitNeedsPanel(panel));
                }
                let active = source.active;
                remove_tab(source, active);
                panel
            }
        };
        next.insert_part(zone, part, Edge::After, moved);
        next.push_focus(moved);
        next.settle(None);

        let frames = geometry(&next, viewport, metrics, &min_size);
        let state = next.zones.get(zone);
        let mut rooms = Vec::new();
        let mut fits = true;
        for index in [part, part + 1] {
            let Some(target) = state.parts.get(index) else {
                continue;
            };
            let Some(active) = target.active_panel() else {
                continue;
            };
            let need = min_size(active);
            let have = frames
                .parts
                .iter()
                .find(|frame| frame.zone == zone && frame.index == index)
                .map_or(Size::ZERO, |frame| Size::of(frame.body));
            fits &= target.share > 0 && need.admits(have.width, have.height);
            rooms.push(Room {
                panel: active,
                need,
                have,
            });
        }
        if fits {
            Ok(next)
        } else {
            Err(OpError::SplitNoRoom(rooms))
        }
    }

    /// Перенос панели в последнюю активную часть зоны, где она становится активной вкладкой.
    /// Скрытая целевая зона показывается, `only` другой зоны снимается, `zoom` панели снимается.
    /// `navigator` — панель плагина навигатора: в левой зоне она встаёт в пустую колонку навигатора.
    /// Фокус следует за панелью.
    ///
    /// # Errors
    ///
    /// [`OpError::NotFound`].
    pub fn move_to_zone(&self, panel: P, zone: Zone, navigator: bool) -> Result<Self, OpError<P>> {
        let from = self.require(panel)?;
        let had_focus = self.focused() == Some(panel);
        let mut next = self.clone();
        if next.zoomed == Some(panel) {
            next.zoomed = None;
        }
        let to_column = zone == Zone::Left
            && navigator
            && (self.navigator.is_none() || from == Location::Navigator);
        let in_last_active = matches!(
            from,
            Location::Part { zone: z, part, .. } if z == zone && part == self.zones.get(zone).last_active
        );
        if to_column {
            if from != Location::Navigator {
                next.take(panel);
                next.navigator = Some(panel);
            }
        } else if !in_last_active {
            next.take(panel);
            next.insert_into_last_active(zone, panel);
        }
        next.place_moved(panel, had_focus);
        Ok(next)
    }

    /// Перенос вкладки в часть якоря последней, а с краем — в новую часть рядом с частью якоря
    /// вдоль оси зоны. Якорь ищется после того, как панель покинула своё место, поэтому исчезнувшая
    /// исходная часть его не сбивает. Целевая зона показывается, фокус следует за панелью.
    ///
    /// # Errors
    ///
    /// [`OpError::NotFound`] для панели и якоря, [`OpError::NavigatorColumn`] для якоря в колонке.
    pub fn move_to_part(
        &self,
        panel: P,
        anchor: P,
        edge: Option<Edge>,
    ) -> Result<Self, OpError<P>> {
        let from = self.require(panel)?;
        let Location::Part { zone, part, .. } = self.require(anchor)? else {
            return Err(OpError::NavigatorColumn(anchor));
        };
        let had_focus = self.focused() == Some(panel);
        let mut next = self.clone();
        if next.zoomed == Some(panel) {
            next.zoomed = None;
        }
        if panel == anchor {
            let Location::Part { tab, .. } = from else {
                return Err(OpError::NavigatorColumn(anchor));
            };
            let target = &mut next.zones.get_mut(zone).parts[part];
            match edge {
                None => {
                    remove_tab(target, tab);
                    target.tabs.push(panel);
                    target.active = target.tabs.len() - 1;
                }
                Some(_) if target.tabs.len() == 1 => {}
                Some(edge) => {
                    remove_tab(target, tab);
                    next.insert_part(zone, part, edge, panel);
                }
            }
        } else {
            next.take(panel);
            let Some(Location::Part { zone, part, .. }) = next.locate(anchor) else {
                return Err(OpError::NotFound(anchor));
            };
            match edge {
                None => {
                    let target = &mut next.zones.get_mut(zone).parts[part];
                    target.tabs.push(panel);
                    target.active = target.tabs.len() - 1;
                }
                Some(edge) => next.insert_part(zone, part, edge, panel),
            }
        }
        let state = next.zones.get_mut(zone);
        if state.parts.iter().any(|p| p.share == 0) {
            equalize(state);
        }
        next.place_moved(panel, had_focus);
        Ok(next)
    }

    /// Закрытие панели. Последняя вкладка части убирает часть, последняя часть — зону. Если панель
    /// была с фокусом, фокус получает запасная панель (правило «Фокус следует за панелью»).
    ///
    /// # Errors
    ///
    /// [`OpError::NotFound`].
    pub fn close(&self, panel: P) -> Result<Self, OpError<P>> {
        let location = self.require(panel)?;
        let lost = (self.focused() == Some(panel)).then_some(location.zone());
        let mut next = self.clone();
        next.take(panel);
        next.focus.retain(|&p| p != panel);
        next.settle(lost);
        Ok(next)
    }

    /// Активная вкладка своей части с фокусом. Совпадает с [`Self::focus_panel`]: щелчок по вкладке
    /// переводит фокус в её часть.
    ///
    /// # Errors
    ///
    /// [`OpError::NotFound`].
    pub fn select(&self, panel: P) -> Result<Self, OpError<P>> {
        self.focus_panel(panel)
    }

    /// Фокус на панели: она становится активной вкладкой, её место показывается, `zoom` другой панели
    /// снимается.
    ///
    /// # Errors
    ///
    /// [`OpError::NotFound`].
    pub fn focus_panel(&self, panel: P) -> Result<Self, OpError<P>> {
        self.require(panel)?;
        let mut next = self.clone();
        next.focus_on(panel);
        next.settle(None);
        Ok(next)
    }

    /// Фокус на ближайшую видимую плитку в направлении: она лежит по ту сторону края плитки с фокусом
    /// и перекрывается с ней поперёк направления. Без размера окна плитки считаются по опорному
    /// размеру, в котором каждая показанная часть получает ненулевую площадь. Без соседа раскладка
    /// не меняется.
    #[must_use]
    pub fn focus_direction(
        &self,
        direction: Direction,
        viewport: Option<Viewport>,
        metrics: &LayoutMetrics,
    ) -> Self {
        let Some(current) = self.focused().filter(|&p| self.is_visible(p)) else {
            return self.clone();
        };
        let viewport = viewport.unwrap_or_else(|| self.reference_viewport(metrics));
        let frames = geometry(self, viewport, metrics, |_| MinSize::DEFAULT);
        let tiles: Vec<(P, Rect)> = frames
            .navigator
            .iter()
            .map(|column| (column.panel, column.frame))
            .chain(frames.parts.iter().map(|part| (part.active, part.frame)))
            .collect();
        let Some(&(_, from)) = tiles.iter().find(|&&(panel, _)| panel == current) else {
            return self.clone();
        };
        let best = tiles
            .iter()
            .filter(|&&(panel, _)| panel != current)
            .filter_map(|&(panel, to)| neighbour(from, to, direction).map(|key| (key, panel)))
            .min_by(|(a, _), (b, _)| {
                a.gap
                    .total_cmp(&b.gap)
                    .then(b.overlap.total_cmp(&a.overlap))
                    .then(a.across.total_cmp(&b.across))
            });
        match best {
            Some((_, panel)) => {
                let mut next = self.clone();
                next.focus_on(panel);
                next.settle(None);
                next
            }
            None => self.clone(),
        }
    }

    /// Размер зоны в точках: ширина плитки левой и правой зоны, высота нижней. Снизу — `min_size`
    /// активных панелей частей зоны, сверху — остаток столбца центра, где части центра и нижней зоны
    /// не меньше своего `min_size`. Сравнивается только измерение, которое меняет размер: часть, тесная
    /// поперёк, границу не запирает. Граница, которую текущий размер уже нарушает, расширяется до него:
    /// изменение размера не делает тесную раскладку ещё теснее, но и не запирает её.
    ///
    /// # Errors
    ///
    /// [`OpError::ZoneNotResizable`] для центра, [`OpError::ZoneEmpty`], [`OpError::ResizeOutOfRange`].
    pub fn resize_zone(
        &self,
        zone: Zone,
        size: u32,
        viewport: Viewport,
        metrics: &LayoutMetrics,
        min_size: impl Fn(P) -> MinSize,
    ) -> Result<Self, OpError<P>> {
        let state = self.zones.get(zone);
        let needs = state
            .parts
            .iter()
            .filter_map(Part::active_panel)
            .map(&min_size);
        let (current, own, limit, along) = match zone {
            Zone::Left | Zone::Right => (
                if zone == Zone::Left {
                    self.sizes.plate
                } else {
                    self.sizes.right
                },
                needs.map(|need| need.width).fold(0.0, f64::max) + f64::from(2 * metrics.line),
                viewport.width,
                Axis::SideBySide,
            ),
            Zone::Bottom => (
                self.sizes.bottom,
                needs.map(|need| need.height).fold(0.0, f64::max)
                    + f64::from(2 * metrics.line + metrics.tabbar),
                viewport.height,
                Axis::Stacked,
            ),
            Zone::Center => return Err(OpError::ZoneNotResizable(zone)),
        };
        if state.is_empty() {
            return Err(OpError::ZoneEmpty(zone));
        }
        // `own` не отрицателен, приведение насыщается на `u32::MAX`.
        let own_min = own.ceil() as u32;

        let mut scratch = self.clone();
        scratch.zoomed = None;
        scratch.only = None;
        scratch.unhide_zone(zone);
        // Остаток — то, что забирает изменение размера: столбец центра и нижней зоны, а без него
        // правая зона, которая тогда получает свободную ширину (правило «Свободная площадь»).
        let remainder = |part: Zone, column: bool| match zone {
            Zone::Left => {
                matches!(part, Zone::Center | Zone::Bottom) || (!column && part == Zone::Right)
            }
            Zone::Right => matches!(part, Zone::Center | Zone::Bottom),
            Zone::Bottom => part == Zone::Center,
            Zone::Center => false,
        };
        let remainder_fits = |s: u32| {
            let mut candidate = scratch.clone();
            candidate.set_zone_size(zone, s);
            let frames = geometry(&candidate, viewport, metrics, &min_size);
            let column = frames.zones.column.is_some();
            frames
                .parts
                .iter()
                .filter(|part| remainder(part.zone, column))
                .all(|part| fits_along(part, min_size(part.active), along))
        };
        let min = own_min.min(current);
        let max = largest(0, limit, remainder_fits).map_or(current, |max| max.max(current));
        if size < min || size > max {
            return Err(OpError::ResizeOutOfRange { min, max });
        }
        let mut next = self.clone();
        next.set_zone_size(zone, size);
        Ok(next)
    }

    /// Доля части панели в тысячных. Разницу забирает или отдаёт соседняя часть: следующая, а у
    /// последней части — предыдущая. Это разделитель между ними. Обе части вдоль оси зоны не меньше
    /// `min_size` активных панелей; граница, которую текущая доля уже нарушает, расширяется до неё.
    ///
    /// # Errors
    ///
    /// [`OpError::NotFound`], [`OpError::NavigatorColumn`], [`OpError::NotInZone`],
    /// [`OpError::ResizeOutOfRange`].
    pub fn resize_part(
        &self,
        zone: Zone,
        panel: P,
        share: u16,
        viewport: Viewport,
        metrics: &LayoutMetrics,
        min_size: impl Fn(P) -> MinSize,
    ) -> Result<Self, OpError<P>> {
        let location = self.require(panel)?;
        let Location::Part {
            zone: actual, part, ..
        } = location
        else {
            return Err(OpError::NavigatorColumn(panel));
        };
        if actual != zone {
            return Err(OpError::NotInZone { panel, zone });
        }
        let parts = &self.zones.get(zone).parts;
        let current = parts[part].share;
        if parts.len() == 1 {
            return if share == SHARE_TOTAL {
                Ok(self.clone())
            } else {
                let total = u32::from(SHARE_TOTAL);
                Err(OpError::ResizeOutOfRange {
                    min: total,
                    max: total,
                })
            };
        }
        let neighbour = if part + 1 < parts.len() {
            part + 1
        } else {
            part - 1
        };
        let pair = current + parts[neighbour].share;
        let with_share = |layout: &mut Self, s: u16| {
            let parts = &mut layout.zones.get_mut(zone).parts;
            parts[part].share = s;
            parts[neighbour].share = pair - s;
        };

        let mut scratch = self.clone();
        scratch.zoomed = None;
        scratch.reveal(location);
        let part_fits = |s: u32, index: usize| {
            let mut candidate = scratch.clone();
            with_share(&mut candidate, u16::try_from(s).unwrap_or(pair));
            geometry(&candidate, viewport, metrics, &min_size)
                .parts
                .iter()
                .find(|frame| frame.zone == zone && frame.index == index)
                .is_some_and(|frame| fits_along(frame, min_size(frame.active), zone.axis()))
        };
        let current_wide = u32::from(current);
        let last = u32::from(pair) - 1;
        let min =
            smallest(1, last, |s| part_fits(s, part)).map_or(current_wide, |m| m.min(current_wide));
        let max = largest(1, last, |s| part_fits(s, neighbour))
            .map_or(current_wide, |m| m.max(current_wide));
        let wanted = u32::from(share);
        if wanted < min || wanted > max {
            return Err(OpError::ResizeOutOfRange { min, max });
        }
        let mut next = self.clone();
        with_share(&mut next, share);
        Ok(next)
    }

    /// Ширина колонки навигатора в точках. Снизу — `min_size` панели навигатора по ширине, сверху —
    /// остаток, где части зон не уже своего `min_size`. Граница, которую текущая ширина уже нарушает,
    /// расширяется до неё.
    ///
    /// # Errors
    ///
    /// [`OpError::TargetEmpty`] без панели навигатора, [`OpError::ResizeOutOfRange`].
    pub fn resize_navigator(
        &self,
        size: u32,
        viewport: Viewport,
        metrics: &LayoutMetrics,
        min_size: impl Fn(P) -> MinSize,
    ) -> Result<Self, OpError<P>> {
        let Some(navigator) = self.navigator else {
            return Err(OpError::TargetEmpty(Hideable::Navigator));
        };
        let current = self.sizes.navigator;
        let need = min_size(navigator);
        let mut scratch = self.clone();
        scratch.zoomed = None;
        scratch.only = None;
        scratch.hidden.navigator = false;
        let frames_at = |s: u32| {
            let mut candidate = scratch.clone();
            candidate.sizes.navigator = s;
            geometry(&candidate, viewport, metrics, &min_size)
        };
        let own_fits = |s: u32| {
            frames_at(s)
                .navigator
                .is_some_and(|column| column.body.width >= need.width)
        };
        let rest_fits = |s: u32| {
            frames_at(s)
                .parts
                .iter()
                .all(|part| fits_along(part, min_size(part.active), Axis::SideBySide))
        };
        let min = smallest(0, viewport.width, own_fits).map_or(current, |min| min.min(current));
        let max = largest(0, viewport.width, rest_fits).map_or(current, |max| max.max(current));
        if size < min || size > max {
            return Err(OpError::ResizeOutOfRange { min, max });
        }
        let mut next = self.clone();
        next.sizes.navigator = size;
        Ok(next)
    }

    /// Размер места панели на `delta` точек в измерении `dimension`. Вдоль оси зоны меняется доля
    /// части панели, если частей больше одной, поперёк — размер зоны. У центра своего размера нет:
    /// его высота меняет нижнюю зону, ширина единственной части — правую. Панель колонки навигатора
    /// меняет ширину колонки.
    ///
    /// # Errors
    ///
    /// [`OpError::NotFound`], [`OpError::NavigatorColumn`] для высоты колонки навигатора,
    /// [`OpError::ZoneEmpty`], [`OpError::ResizeOutOfRange`] с границами итогового размера в точках.
    pub fn resize_by(
        &self,
        panel: P,
        dimension: Dimension,
        delta: i32,
        viewport: Viewport,
        metrics: &LayoutMetrics,
        min_size: impl Fn(P) -> MinSize,
    ) -> Result<Self, OpError<P>> {
        let Location::Part { zone, .. } = self.require(panel)? else {
            return match dimension {
                Dimension::Width => self.resize_navigator(
                    shifted(self.sizes.navigator, delta),
                    viewport,
                    metrics,
                    min_size,
                ),
                Dimension::Height => Err(OpError::NavigatorColumn(panel)),
            };
        };
        let along = match dimension {
            Dimension::Width => Axis::SideBySide,
            Dimension::Height => Axis::Stacked,
        };
        let single = self.zones.get(zone).parts.len() == 1;
        let (target, delta) = match zone {
            Zone::Center if along == Axis::Stacked => (Zone::Bottom, delta.saturating_neg()),
            Zone::Center if single => (Zone::Right, delta.saturating_neg()),
            Zone::Left | Zone::Right | Zone::Bottom if zone.axis() != along => (zone, delta),
            Zone::Left | Zone::Right | Zone::Center | Zone::Bottom => {
                return self.resize_part_by(panel, delta, viewport, metrics, min_size);
            }
        };
        let current = match target {
            Zone::Left => self.sizes.plate,
            Zone::Right => self.sizes.right,
            Zone::Bottom | Zone::Center => self.sizes.bottom,
        };
        self.resize_zone(target, shifted(current, delta), viewport, metrics, min_size)
    }

    /// Доля части панели на `delta` точек вдоль оси зоны. Границы отказа — длина части в точках.
    fn resize_part_by(
        &self,
        panel: P,
        delta: i32,
        viewport: Viewport,
        metrics: &LayoutMetrics,
        min_size: impl Fn(P) -> MinSize,
    ) -> Result<Self, OpError<P>> {
        let location = self.require(panel)?;
        let Location::Part { zone, part, .. } = location else {
            return Err(OpError::NavigatorColumn(panel));
        };
        let mut scratch = self.clone();
        scratch.zoomed = None;
        scratch.reveal(location);
        let total: f64 = geometry(&scratch, viewport, metrics, &min_size)
            .parts
            .iter()
            .filter(|frame| frame.zone == zone)
            .map(|frame| match zone.axis() {
                Axis::SideBySide => frame.frame.width,
                Axis::Stacked => frame.frame.height,
            })
            .sum();
        let whole = f64::from(SHARE_TOTAL);
        // Длина неотрицательна, приведение насыщается.
        let points = |share: u32| (f64::from(share) * total / whole).round() as u32;
        let parts = &self.zones.get(zone).parts;
        let current = parts[part].share;
        if parts.len() == 1 || total <= 0.0 {
            let size = points(u32::from(current));
            return if delta == 0 {
                Ok(self.clone())
            } else {
                Err(OpError::ResizeOutOfRange {
                    min: size,
                    max: size,
                })
            };
        }
        let wanted = f64::from(current) + (f64::from(delta) * whole / total).round();
        let share = wanted.clamp(0.0, whole) as u16;
        self.resize_part(zone, panel, share, viewport, metrics, min_size)
            .map_err(|error| match error {
                OpError::ResizeOutOfRange { min, max } => OpError::ResizeOutOfRange {
                    min: points(min),
                    max: points(max),
                },
                other => other,
            })
    }

    /// Соседняя вкладка части панели с фокусом по кругу: `forward` — следующая, иначе предыдущая.
    /// Без видимой панели с фокусом, в колонке навигатора и у части с одной вкладкой раскладка не
    /// меняется.
    #[must_use]
    pub fn tab_step(&self, forward: bool) -> Self {
        self.focus_tab(|active, count| {
            Some(if forward {
                (active + 1) % count
            } else {
                (active + count - 1) % count
            })
        })
    }

    /// Вкладка `index` (с нуля) части панели с фокусом. Вкладки с таким номером нет — раскладка не
    /// меняется.
    #[must_use]
    pub fn tab_at(&self, index: usize) -> Self {
        self.focus_tab(|_, count| (index < count).then_some(index))
    }

    fn focus_tab(&self, pick: impl FnOnce(usize, usize) -> Option<usize>) -> Self {
        let target = self
            .focused()
            .filter(|&panel| self.is_visible(panel))
            .and_then(|panel| match self.locate(panel)? {
                Location::Navigator => None,
                Location::Part { zone, part, .. } => {
                    let part = &self.zones.get(zone).parts[part];
                    part.tabs.get(pick(part.active, part.tabs.len())?).copied()
                }
            });
        match target {
            Some(panel) => {
                let mut next = self.clone();
                next.focus_on(panel);
                next.settle(None);
                next
            }
            None => self.clone(),
        }
    }

    /// Равные доли частей зоны, остаток — последней части.
    ///
    /// # Errors
    ///
    /// [`OpError::ZoneEmpty`].
    pub fn equal(&self, zone: Zone) -> Result<Self, OpError<P>> {
        if self.zones.get(zone).is_empty() {
            return Err(OpError::ZoneEmpty(zone));
        }
        let mut next = self.clone();
        equalize(next.zones.get_mut(zone));
        Ok(next)
    }

    /// Разворот панели на всю область зон. Фокус переходит на неё, деления не меняются.
    ///
    /// # Errors
    ///
    /// [`OpError::NotFound`].
    pub fn zoom(&self, panel: P) -> Result<Self, OpError<P>> {
        self.require(panel)?;
        let mut next = self.clone();
        next.focus_on(panel);
        next.zoomed = Some(panel);
        next.settle(None);
        Ok(next)
    }

    /// Снятие разворота.
    #[must_use]
    pub fn unzoom(&self) -> Self {
        let mut next = self.clone();
        next.zoomed = None;
        next.settle(None);
        next
    }

    /// Скрытие или показ колонки навигатора, плитки, правой или нижней зоны. Сохранённый размер не
    /// меняется. Показ цели вне `only` снимает режим. Скрытая вместе с зоной панель отдаёт фокус.
    ///
    /// # Errors
    ///
    /// [`OpError::TargetEmpty`].
    pub fn set_visible(&self, target: Hideable, visible: bool) -> Result<Self, OpError<P>> {
        let empty = match target {
            Hideable::Navigator => self.navigator.is_none(),
            Hideable::Plate => self.zones.left.is_empty(),
            Hideable::Right => self.zones.right.is_empty(),
            Hideable::Bottom => self.zones.bottom.is_empty(),
        };
        if empty {
            return Err(OpError::TargetEmpty(target));
        }
        let mut next = self.clone();
        let flag = match target {
            Hideable::Navigator => &mut next.hidden.navigator,
            Hideable::Plate => &mut next.hidden.plate,
            Hideable::Right => &mut next.hidden.right,
            Hideable::Bottom => &mut next.hidden.bottom,
        };
        *flag = !visible;
        if visible && next.only.is_some_and(|only| only != target.zone()) {
            next.only = None;
        }
        next.settle(None);
        Ok(next)
    }

    /// `Ctrl-w o`: видна только зона с фокусом; повтор возвращает прежнее скрытие. Флаги скрытия не
    /// меняются. Без видимой панели с фокусом раскладка не меняется.
    #[must_use]
    pub fn toggle_only(&self) -> Self {
        let mut next = self.clone();
        if next.only.take().is_none() {
            next.only = self
                .focused()
                .filter(|&p| self.is_visible(p))
                .and_then(|p| self.locate(p))
                .map(Location::zone);
        }
        next.settle(None);
        next
    }

    fn require(&self, panel: P) -> Result<Location, OpError<P>> {
        self.locate(panel).ok_or(OpError::NotFound(panel))
    }

    fn zone_has_panels(&self, zone: Zone) -> bool {
        !self.zones.get(zone).is_empty() || (zone == Zone::Left && self.navigator.is_some())
    }

    fn set_zone_size(&mut self, zone: Zone, size: u32) {
        match zone {
            Zone::Left => self.sizes.plate = size,
            Zone::Right => self.sizes.right = size,
            Zone::Center => {}
            Zone::Bottom => self.sizes.bottom = size,
        }
    }

    fn unhide_zone(&mut self, zone: Zone) {
        match zone {
            Zone::Left => self.hidden.plate = false,
            Zone::Right => self.hidden.right = false,
            Zone::Center => {}
            Zone::Bottom => self.hidden.bottom = false,
        }
    }

    /// Показывает место: снимает скрытие его зоны или колонки и `only` другой зоны.
    fn reveal(&mut self, location: Location) {
        match location {
            Location::Navigator => self.hidden.navigator = false,
            Location::Part { zone, .. } => self.unhide_zone(zone),
        }
        if self.only.is_some_and(|only| only != location.zone()) {
            self.only = None;
        }
    }

    fn activate(&mut self, location: Location) {
        if let Location::Part { zone, part, tab } = location {
            let state = self.zones.get_mut(zone);
            state.parts[part].active = tab;
            state.last_active = part;
        }
    }

    fn push_focus(&mut self, panel: P) {
        self.focus.retain(|&p| p != panel);
        self.focus.push(panel);
    }

    /// Фокус на панели с явной целью: вкладка активна, место показано, `zoom` другой панели снят.
    fn focus_on(&mut self, panel: P) {
        let Some(location) = self.locate(panel) else {
            return;
        };
        if self.zoomed.is_some_and(|zoomed| zoomed != panel) {
            self.zoomed = None;
        }
        self.activate(location);
        self.reveal(location);
        self.push_focus(panel);
    }

    /// Завершение переноса: панель — активная вкладка показанного места, фокус следует за ней.
    fn place_moved(&mut self, panel: P, had_focus: bool) {
        if let Some(location) = self.locate(panel) {
            self.activate(location);
            self.reveal(location);
        }
        if had_focus {
            self.push_focus(panel);
        }
        self.settle(None);
    }

    /// Убирает панель с места и снимает её `zoom`. Часть без вкладок исчезает, её доля уходит
    /// соседней: левее или выше, иначе правее или ниже.
    fn take(&mut self, panel: P) {
        let Some(location) = self.locate(panel) else {
            return;
        };
        if self.zoomed == Some(panel) {
            self.zoomed = None;
        }
        let Location::Part { zone, part, tab } = location else {
            self.navigator = None;
            return;
        };
        let state = self.zones.get_mut(zone);
        remove_tab(&mut state.parts[part], tab);
        if !state.parts[part].tabs.is_empty() {
            return;
        }
        let removed = state.parts.remove(part);
        if state.parts.is_empty() {
            state.last_active = 0;
            return;
        }
        let heir = part.saturating_sub(1);
        state.parts[heir].share += removed.share;
        state.last_active = match state.last_active.cmp(&part) {
            Ordering::Less => state.last_active,
            Ordering::Equal => heir,
            Ordering::Greater => state.last_active - 1,
        };
    }

    fn insert_into_last_active(&mut self, zone: Zone, panel: P) {
        let state = self.zones.get_mut(zone);
        if state.parts.is_empty() {
            state.parts.push(Part::single(panel, SHARE_TOTAL));
            state.last_active = 0;
            return;
        }
        let index = state.last_active.min(state.parts.len() - 1);
        state.last_active = index;
        let target = &mut state.parts[index];
        target.tabs.push(panel);
        target.active = target.tabs.len() - 1;
    }

    /// Новая часть с панелью рядом с частью `anchor`. Обе делят долю `anchor` поровну, большая
    /// половина — у правой или нижней.
    fn insert_part(&mut self, zone: Zone, anchor: usize, edge: Edge, panel: P) {
        let state = self.zones.get_mut(zone);
        let share = state.parts[anchor].share;
        let first = share / 2;
        let second = share - first;
        let (at, kept, given) = match edge {
            Edge::Before => (anchor, second, first),
            Edge::After => (anchor + 1, first, second),
        };
        state.parts[anchor].share = kept;
        state.parts.insert(at, Part::single(panel, given));
        state.last_active = at;
    }

    /// Приводит фокус и `only` к инвариантам после операции. `lost` — зона панели с фокусом, которая
    /// закрыта: тогда цепочка запасного фокуса начинается с этой зоны, а не с предыдущей записи.
    fn settle(&mut self, lost: Option<Zone>) {
        if let Some(only) = self.only
            && !self.zone_has_panels(only)
        {
            self.only = None;
        }
        let present: BTreeSet<P> = self.panels().collect();
        self.focus.retain(|p| present.contains(p));
        if lost.is_none() && self.focused().is_some_and(|p| self.is_visible(p)) {
            return;
        }
        let zone = lost.or_else(|| {
            self.focused()
                .and_then(|p| self.locate(p))
                .map(Location::zone)
        });
        if let Some(fallback) = self.fallback_focus(zone) {
            self.push_focus(fallback);
        }
    }

    /// Запасной фокус ADR-0009: последняя активная видимая панель той же зоны, иначе последняя видимая
    /// запись истории, иначе активная вкладка первой части центра, иначе любая видимая панель в
    /// постоянном порядке (колонка навигатора, затем части по [`Zone::ALL`]).
    fn fallback_focus(&self, zone: Option<Zone>) -> Option<P> {
        let visible = |p: &P| self.is_visible(*p);
        let same_zone = zone.and_then(|zone| {
            let state = self.zones.get(zone);
            let last_active = state
                .parts
                .get(state.last_active)
                .and_then(Part::active_panel)
                .filter(visible);
            match zone {
                Zone::Left => last_active.or_else(|| self.navigator.filter(visible)),
                Zone::Right | Zone::Center | Zone::Bottom => last_active,
            }
        });
        same_zone
            .or_else(|| self.focus.iter().rev().copied().find(visible))
            .or_else(|| {
                self.zones
                    .center
                    .parts
                    .first()
                    .and_then(Part::active_panel)
                    .filter(visible)
            })
            .or_else(|| self.panels().find(visible))
    }

    /// Опорный размер окна для направлений без настоящего: сохранённые размеры зон и запас, в котором
    /// у каждой показанной части ненулевая площадь.
    fn reference_viewport(&self, metrics: &LayoutMetrics) -> Viewport {
        const SPARE: u32 = 4000;
        let sizes = self.sizes;
        Viewport {
            width: sizes
                .navigator
                .saturating_add(sizes.plate)
                .saturating_add(sizes.right)
                .saturating_add(SPARE),
            height: metrics
                .titlebar
                .saturating_add(metrics.status_bar)
                .saturating_add(sizes.bottom)
                .saturating_add(SPARE),
        }
    }
}

/// Убирает вкладку. Активной остаётся та же вкладка, а если убрана активная — следующая, у последней
/// — предыдущая.
fn remove_tab<P>(part: &mut Part<P>, tab: usize) {
    part.tabs.remove(tab);
    if tab < part.active {
        part.active -= 1;
    }
    part.active = part.active.min(part.tabs.len().saturating_sub(1));
}

/// Тело части не меньше `min_size` в измерении оси: ширина у частей рядом, высота у частей одна над
/// другой.
fn fits_along<P>(frame: &PartFrame<P>, need: MinSize, axis: Axis) -> bool {
    match axis {
        Axis::SideBySide => frame.body.width >= need.width,
        Axis::Stacked => frame.body.height >= need.height,
    }
}

fn equalize<P>(state: &mut ZoneState<P>) {
    let count = u16::try_from(state.parts.len().max(1)).unwrap_or(SHARE_TOTAL);
    let base = SHARE_TOTAL / count;
    for part in &mut state.parts {
        part.share = base;
    }
    if let Some(last) = state.parts.last_mut() {
        last.share = SHARE_TOTAL - base * (count - 1);
    }
}

/// Порядок кандидатов направления: меньший зазор, затем большее перекрытие, затем меньшая координата.
struct Neighbour {
    gap: f64,
    overlap: f64,
    across: f64,
}

fn neighbour(from: Rect, to: Rect, direction: Direction) -> Option<Neighbour> {
    let span = |a0: f64, a1: f64, b0: f64, b1: f64| a1.min(b1) - a0.max(b0);
    let vertical = span(from.y, from.bottom(), to.y, to.bottom());
    let horizontal = span(from.x, from.right(), to.x, to.right());
    let (gap, overlap, across) = match direction {
        Direction::Left => (from.x - to.right(), vertical, to.y),
        Direction::Right => (to.x - from.right(), vertical, to.y),
        Direction::Up => (from.y - to.bottom(), horizontal, to.x),
        Direction::Down => (to.y - from.bottom(), horizontal, to.x),
    };
    (gap >= 0.0 && overlap > 0.0).then_some(Neighbour {
        gap,
        overlap,
        across,
    })
}

/// Наименьшее значение `lo..=hi`, где `holds` истинно. `holds` монотонно растёт.
fn smallest(lo: u32, hi: u32, holds: impl Fn(u32) -> bool) -> Option<u32> {
    if lo > hi || !holds(hi) {
        return None;
    }
    let (mut low, mut high) = (lo, hi);
    while low < high {
        let mid = low + (high - low) / 2;
        if holds(mid) {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    Some(low)
}

/// Наибольшее значение `lo..=hi`, где `holds` истинно. `holds` монотонно убывает.
fn largest(lo: u32, hi: u32, holds: impl Fn(u32) -> bool) -> Option<u32> {
    if lo > hi || !holds(lo) {
        return None;
    }
    let (mut low, mut high) = (lo, hi);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if holds(mid) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    Some(low)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Frames;

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
    const NAV: u32 = 1;
    const TERM: u32 = 2;

    fn min(_: u32) -> MinSize {
        MinSize::DEFAULT
    }

    fn base() -> Layout<u32> {
        Layout::default_for(NAV, TERM)
    }

    fn valid(layout: &Layout<u32>) {
        let panels = layout.panels().collect();
        assert_eq!(layout.validate(&panels), Ok(()), "{layout:?}");
    }

    fn frames(layout: &Layout<u32>, viewport: Viewport) -> Frames<u32> {
        geometry(layout, viewport, &METRICS, min)
    }

    fn tabs(layout: &Layout<u32>, zone: Zone) -> Vec<Vec<u32>> {
        layout
            .zones
            .get(zone)
            .parts
            .iter()
            .map(|part| part.tabs.clone())
            .collect()
    }

    fn shares(layout: &Layout<u32>, zone: Zone) -> Vec<u16> {
        layout
            .zones
            .get(zone)
            .parts
            .iter()
            .map(|part| part.share)
            .collect()
    }

    fn split(
        layout: &Layout<u32>,
        panel: u32,
        open: Option<u32>,
    ) -> Result<Layout<u32>, OpError<u32>> {
        layout.split(panel, open, SMALL, &METRICS, min)
    }

    /// Части зоны из одиночных вкладок с долями.
    fn singles(layout: &mut Layout<u32>, zone: Zone, panels: &[(u32, u16)]) {
        layout.zones.get_mut(zone).parts = panels
            .iter()
            .map(|&(panel, share)| Part::single(panel, share))
            .collect();
        layout.zones.get_mut(zone).last_active = 0;
    }

    /// Раскладка по умолчанию с правой зоной из панели 40.
    fn with_right() -> Layout<u32> {
        let mut layout = base();
        singles(&mut layout, Zone::Right, &[(40, SHARE_TOTAL)]);
        layout
    }

    /// Две части центра: 2 и 3, фокус на 2.
    fn two_center() -> Layout<u32> {
        let mut layout = base();
        singles(&mut layout, Zone::Center, &[(2, 500), (3, 500)]);
        layout
    }

    fn body(frames: &Frames<u32>, panel: u32) -> Size {
        Size::of(frames.part_of(&panel).unwrap().body)
    }

    #[test]
    fn open_goes_to_the_last_active_part_and_takes_focus() {
        let layout = base().open(5, None).unwrap();
        valid(&layout);
        assert_eq!(tabs(&layout, Zone::Center), vec![vec![2, 5]]);
        assert_eq!(layout.zones.center.parts[0].active, 1);
        assert_eq!(layout.focused(), Some(5));

        let layout = layout.open(6, Some(Zone::Bottom)).unwrap();
        valid(&layout);
        assert_eq!(tabs(&layout, Zone::Bottom), vec![vec![6]]);
        assert_eq!(layout.focused(), Some(6));
        assert_eq!(layout.open(5, None), Err(OpError::AlreadyPresent(5)));

        let hidden = layout.set_visible(Hideable::Bottom, false).unwrap();
        let reopened = hidden.zoom(2).unwrap().open(7, Some(Zone::Bottom)).unwrap();
        valid(&reopened);
        assert!(!reopened.hidden.bottom, "открытие показывает зону");
        assert_eq!(reopened.zoomed, None, "новая панель с фокусом снимает zoom");
        assert_eq!(tabs(&reopened, Zone::Bottom), vec![vec![6, 7]]);
    }

    #[test]
    fn split_side_by_side_moves_the_active_tab_right_and_keeps_focus() {
        let layout = base().open(5, None).unwrap().select(2).unwrap();
        let split = split(&layout, 2, None).unwrap();
        valid(&split);
        assert_eq!(tabs(&split, Zone::Center), vec![vec![5], vec![2]]);
        assert_eq!(shares(&split, Zone::Center), vec![500, 500]);
        assert_eq!(split.zones.center.last_active, 1);
        assert_eq!(split.focused(), Some(2));
    }

    #[test]
    fn split_stacked_zone_moves_the_tab_below() {
        let layout = with_right().open(41, Some(Zone::Right)).unwrap();
        let layout = layout.focus_panel(40).unwrap();
        let split = layout.split(40, None, LARGE, &METRICS, min).unwrap();
        valid(&split);
        assert_eq!(tabs(&split, Zone::Right), vec![vec![41], vec![40]]);
        assert_eq!(split.focused(), Some(40));
        let f = frames(&split, LARGE);
        let (upper, lower) = (f.part_of(&41).unwrap(), f.part_of(&40).unwrap());
        assert_eq!(upper.frame.x, lower.frame.x);
        assert!(lower.frame.y > upper.frame.bottom(), "новая часть ниже");
    }

    #[test]
    fn split_with_open_creates_the_panel_in_a_new_part_with_focus() {
        let split = split(&base(), 2, Some(9)).unwrap();
        valid(&split);
        assert_eq!(tabs(&split, Zone::Center), vec![vec![2], vec![9]]);
        assert_eq!(shares(&split, Zone::Center), vec![500, 500]);
        assert_eq!(split.focused(), Some(9));
    }

    #[test]
    fn split_refuses_single_tab_navigator_unknown_and_present_panels() {
        assert_eq!(split(&base(), 2, None), Err(OpError::SplitNeedsPanel(2)));
        assert_eq!(
            split(&base(), NAV, Some(9)),
            Err(OpError::NavigatorColumn(NAV))
        );
        assert_eq!(split(&base(), 99, None), Err(OpError::NotFound(99)));
        assert_eq!(
            split(&base(), 2, Some(NAV)),
            Err(OpError::AlreadyPresent(NAV))
        );
    }

    #[test]
    fn split_two_fit_then_split_refused_with_need_and_have() {
        let layout = with_right();
        assert_eq!(frames(&layout, SMALL).zones.center.unwrap().width, 560.0);
        let two = split(&layout, 2, Some(3)).unwrap();
        valid(&two);
        let f = frames(&two, SMALL);
        for panel in [2, 3] {
            assert!(body(&f, panel).width >= 200.0);
            assert!(!f.part_of(&panel).unwrap().stub);
        }

        let refused = split(&two, 3, Some(4));
        let have = Size {
            width: 135.0,
            height: 606.0,
        };
        assert_eq!(
            refused,
            Err(OpError::SplitNoRoom(vec![
                Room {
                    panel: 3,
                    need: MinSize::DEFAULT,
                    have
                },
                Room {
                    panel: 4,
                    need: MinSize::DEFAULT,
                    have
                },
            ]))
        );
        assert!(refused.unwrap_err().to_string().contains("нужно 200×120"));
    }

    #[test]
    fn split_button_in_an_unfocused_part_focuses_and_splits_in_one_step() {
        let mut layout = two_center();
        layout.zones.center.parts[1].tabs = vec![3, 5];
        assert_eq!(layout.focused(), Some(2));
        let split = split(&layout, 3, None).unwrap();
        valid(&split);
        assert_eq!(tabs(&split, Zone::Center), vec![vec![2], vec![5], vec![3]]);
        assert_eq!(shares(&split, Zone::Center), vec![500, 250, 250]);
        assert_eq!(split.focused(), Some(3));
    }

    #[test]
    fn split_clears_zoom_and_shows_the_hidden_zone() {
        let layout = with_right().open(41, Some(Zone::Right)).unwrap();
        let layout = layout
            .zoom(TERM)
            .unwrap()
            .set_visible(Hideable::Right, false)
            .unwrap();
        let split = layout.split(41, None, LARGE, &METRICS, min).unwrap();
        valid(&split);
        assert_eq!(split.zoomed, None);
        assert!(!split.hidden.right);
        assert_eq!(split.focused(), Some(41));
    }

    #[test]
    fn split_never_leaves_a_zero_share_even_without_min_size() {
        let mut layout = base();
        singles(&mut layout, Zone::Center, &[(2, 1), (3, 999)]);
        valid(&layout);
        let nothing = |_| MinSize {
            width: 0.0,
            height: 0.0,
        };
        assert!(matches!(
            layout.split(2, Some(9), LARGE, &METRICS, nothing),
            Err(OpError::SplitNoRoom(_))
        ));
    }

    #[test]
    fn move_cli_moves_the_terminal_to_the_right_with_focus() {
        let moved = base().move_to_zone(TERM, Zone::Right, false).unwrap();
        valid(&moved);
        assert_eq!(tabs(&moved, Zone::Right), vec![vec![TERM]]);
        assert!(moved.zones.center.is_empty());
        assert_eq!(moved.focused(), Some(TERM));
        assert_eq!(
            moved.panels().collect::<BTreeSet<_>>(),
            BTreeSet::from([NAV, TERM])
        );
    }

    #[test]
    fn move_to_zone_shows_the_target_and_drops_only_of_another_zone() {
        let layout = with_right().set_visible(Hideable::Right, false).unwrap();
        let moved = layout.move_to_zone(TERM, Zone::Right, false).unwrap();
        valid(&moved);
        assert!(!moved.hidden.right);
        assert_eq!(tabs(&moved, Zone::Right), vec![vec![40, TERM]]);

        let only = base().open(5, None).unwrap().toggle_only();
        assert_eq!(only.only, Some(Zone::Center));
        let moved = only.move_to_zone(5, Zone::Bottom, false).unwrap();
        valid(&moved);
        assert_eq!(moved.only, None);

        let only_right = with_right().focus_panel(40).unwrap().toggle_only();
        let moved = only_right.move_to_zone(TERM, Zone::Right, false).unwrap();
        valid(&moved);
        assert_eq!(
            moved.only,
            Some(Zone::Right),
            "перенос в ту же зону режим не снимает"
        );
        assert_eq!(
            moved.focused(),
            Some(TERM),
            "вкладка закрыла панель с фокусом и получила его"
        );
    }

    #[test]
    fn move_to_zone_lands_in_the_last_active_part() {
        let mut layout = two_center();
        singles(&mut layout, Zone::Right, &[(40, SHARE_TOTAL)]);
        let layout = layout.focus_panel(3).unwrap().focus_panel(NAV).unwrap();
        let moved = layout.move_to_zone(40, Zone::Center, false).unwrap();
        valid(&moved);
        assert_eq!(tabs(&moved, Zone::Center), vec![vec![2], vec![3, 40]]);
        assert_eq!(moved.zones.center.parts[1].active, 1);
        assert_eq!(
            moved.focused(),
            Some(NAV),
            "панель без фокуса фокус не забирает"
        );
        assert!(moved.zones.right.is_empty());
    }

    #[test]
    fn navigator_moves_into_an_empty_column_otherwise_into_the_plate() {
        let out = base().move_to_zone(NAV, Zone::Center, true).unwrap();
        valid(&out);
        assert_eq!(out.navigator, None);
        assert_eq!(tabs(&out, Zone::Center), vec![vec![TERM, NAV]]);
        let back = out.move_to_zone(NAV, Zone::Left, true).unwrap();
        valid(&back);
        assert_eq!(back.navigator, Some(NAV));
        assert!(back.zones.left.is_empty());

        let second = base().open(8, None).unwrap();
        let plate = second.move_to_zone(8, Zone::Left, true).unwrap();
        valid(&plate);
        assert_eq!(plate.navigator, Some(NAV));
        assert_eq!(tabs(&plate, Zone::Left), vec![vec![8]]);
        let terminal = base().move_to_zone(TERM, Zone::Left, false).unwrap();
        assert_eq!(tabs(&terminal, Zone::Left), vec![vec![TERM]]);

        assert_eq!(base().move_to_zone(NAV, Zone::Left, true), Ok(base()));
    }

    #[test]
    fn move_menu_part_merges_the_single_tab_into_the_right_part() {
        let moved = two_center().move_to_part(2, 3, None).unwrap();
        valid(&moved);
        assert_eq!(tabs(&moved, Zone::Center), vec![vec![3, 2]]);
        assert_eq!(shares(&moved, Zone::Center), vec![SHARE_TOTAL]);
        assert_eq!(moved.zones.center.parts[0].active, 1);
        assert_eq!(moved.focused(), Some(2));
    }

    #[test]
    fn move_to_part_edges_follow_the_anchor() {
        let mut layout = two_center();
        layout.zones.center.parts[0].tabs = vec![2, 5];
        let after = layout.move_to_part(5, 3, Some(Edge::After)).unwrap();
        valid(&after);
        assert_eq!(tabs(&after, Zone::Center), vec![vec![2], vec![3], vec![5]]);
        assert_eq!(shares(&after, Zone::Center), vec![500, 250, 250]);
        let before = layout.move_to_part(5, 3, Some(Edge::Before)).unwrap();
        valid(&before);
        assert_eq!(tabs(&before, Zone::Center), vec![vec![2], vec![5], vec![3]]);

        let vanished = two_center().move_to_part(2, 3, Some(Edge::Before)).unwrap();
        valid(&vanished);
        assert_eq!(tabs(&vanished, Zone::Center), vec![vec![2], vec![3]]);
        assert_eq!(shares(&vanished, Zone::Center), vec![500, 500]);
        assert_eq!(vanished.focused(), Some(2));

        let mut stacked = base();
        singles(&mut stacked, Zone::Right, &[(40, 500), (41, 500)]);
        let across = stacked.move_to_part(TERM, 41, Some(Edge::After)).unwrap();
        valid(&across);
        assert_eq!(
            tabs(&across, Zone::Right),
            vec![vec![40], vec![41], vec![TERM]]
        );
        assert!(across.zones.center.is_empty());
        assert_eq!(across.focused(), Some(TERM));
    }

    #[test]
    fn move_to_part_on_its_own_anchor() {
        let layout = base().open(5, None).unwrap().select(2).unwrap();
        let last = layout.move_to_part(2, 2, None).unwrap();
        valid(&last);
        assert_eq!(tabs(&last, Zone::Center), vec![vec![5, 2]]);
        let split = layout.move_to_part(5, 5, Some(Edge::After)).unwrap();
        valid(&split);
        assert_eq!(tabs(&split, Zone::Center), vec![vec![2], vec![5]]);
        assert_eq!(base().move_to_part(2, 2, Some(Edge::Before)), Ok(base()));
    }

    #[test]
    fn move_to_part_refuses_column_and_unknown_anchors() {
        assert_eq!(
            base().move_to_part(TERM, NAV, None),
            Err(OpError::NavigatorColumn(NAV))
        );
        assert_eq!(
            base().move_to_part(TERM, 99, None),
            Err(OpError::NotFound(99))
        );
        assert_eq!(
            base().move_to_part(99, TERM, None),
            Err(OpError::NotFound(99))
        );

        let hidden = with_right().set_visible(Hideable::Right, false).unwrap();
        let moved = hidden.move_to_part(TERM, 40, None).unwrap();
        valid(&moved);
        assert!(!moved.hidden.right, "целевая зона показывается");
        assert_eq!(tabs(&moved, Zone::Right), vec![vec![40, TERM]]);
    }

    #[test]
    fn repeated_edge_drops_never_leave_a_zero_share() {
        let mut layout = base();
        for panel in 10..30 {
            layout = layout.open(panel, None).unwrap();
        }
        for panel in 10..30 {
            layout = layout
                .move_to_part(panel, TERM, Some(Edge::Before))
                .unwrap();
            valid(&layout);
        }
        assert_eq!(layout.zones.center.parts.len(), 21);
        assert!(layout.zones.center.parts.iter().all(|part| part.share > 0));
    }

    #[test]
    fn closing_parts_gives_their_share_to_the_left_then_right_neighbour() {
        let mut layout = base();
        singles(&mut layout, Zone::Center, &[(2, 300), (3, 300), (5, 400)]);
        let closed = layout.close(3).unwrap();
        valid(&closed);
        assert_eq!(tabs(&closed, Zone::Center), vec![vec![2], vec![5]]);
        assert_eq!(shares(&closed, Zone::Center), vec![600, 400]);
        let first = closed.close(2).unwrap();
        valid(&first);
        assert_eq!(shares(&first, Zone::Center), vec![SHARE_TOTAL]);
        assert_eq!(first.focused(), Some(5));

        let gone = with_right().close(40).unwrap();
        valid(&gone);
        assert!(gone.zones.right.is_empty());
        assert_eq!(gone.zones.right.last_active, 0);
        assert_eq!(gone.close(99), Err(OpError::NotFound(99)));
    }

    #[test]
    fn focus_after_close_starts_in_the_same_zone() {
        let mut layout = two_center();
        layout.zones.center.parts[0].tabs = vec![2, 5];
        singles(&mut layout, Zone::Right, &[(40, SHARE_TOTAL)]);
        layout.focus = vec![3, 40, 2];
        valid(&layout);
        let closed = layout.close(2).unwrap();
        valid(&closed);
        assert_eq!(
            closed.focused(),
            Some(5),
            "соседняя вкладка, а не прошлая запись 40"
        );

        let mut layout = two_center();
        singles(&mut layout, Zone::Right, &[(40, SHARE_TOTAL)]);
        layout.focus = vec![3, 2, 40];
        let closed = layout.close(40).unwrap();
        valid(&closed);
        assert_eq!(
            closed.focused(),
            Some(2),
            "зона исчезла: последняя видимая запись"
        );

        let mut layout = two_center();
        singles(&mut layout, Zone::Right, &[(40, SHARE_TOTAL)]);
        layout.focus = vec![40];
        let closed = layout.close(40).unwrap();
        assert_eq!(
            closed.focused(),
            Some(2),
            "история пуста: первая часть центра"
        );

        let closed = base().close(TERM).unwrap();
        valid(&closed);
        assert_eq!(closed.focused(), Some(NAV), "любая видимая панель");
        assert_eq!(closed.focus, vec![NAV]);
    }

    #[test]
    fn hiding_the_focused_zone_moves_focus_and_show_never_refuses() {
        let layout = with_right().focus_panel(40).unwrap();
        let hidden = layout.set_visible(Hideable::Right, false).unwrap();
        valid(&hidden);
        assert_eq!(hidden.focused(), Some(TERM));
        assert_eq!(hidden.sizes, layout.sizes, "скрытие сохраняет размер");

        let plate = base().open(10, Some(Zone::Left)).unwrap();
        let hidden = plate.set_visible(Hideable::Plate, false).unwrap();
        valid(&hidden);
        assert_eq!(
            hidden.focused(),
            Some(NAV),
            "та же левая зона: колонка навигатора"
        );

        let mut crowded = with_right();
        singles(&mut crowded, Zone::Center, &[(2, 333), (3, 333), (5, 334)]);
        crowded.hidden.right = true;
        let shown = crowded.set_visible(Hideable::Right, true).unwrap();
        valid(&shown);
        assert!(frames(&shown, SMALL).parts.iter().any(|part| part.stub));

        let empty = base().close(NAV).unwrap();
        assert_eq!(
            empty.set_visible(Hideable::Navigator, false),
            Err(OpError::TargetEmpty(Hideable::Navigator))
        );
        for target in [Hideable::Plate, Hideable::Right, Hideable::Bottom] {
            assert_eq!(
                base().set_visible(target, true),
                Err(OpError::TargetEmpty(target))
            );
        }
    }

    #[test]
    fn zoom_restore_returns_the_same_frames() {
        let layout = two_center();
        let before = frames(&layout, SMALL);
        let zoomed = layout.zoom(3).unwrap();
        valid(&zoomed);
        assert_eq!(zoomed.zoomed, Some(3));
        assert_eq!(zoomed.focused(), Some(3), "явная цель переводит фокус");
        assert_eq!(
            tabs(&zoomed, Zone::Center),
            tabs(&layout, Zone::Center),
            "zoom не меняет деления"
        );
        assert_eq!(shares(&zoomed, Zone::Center), shares(&layout, Zone::Center));
        let f = frames(&zoomed, SMALL);
        assert!(f.parts.is_empty());
        assert_eq!(f.zoomed.unwrap().geometry.zone, Zone::Center);

        let restored = zoomed.unzoom();
        valid(&restored);
        assert_eq!(frames(&restored, SMALL), before);
        assert_eq!(restored.focused(), Some(3));
        assert_eq!(base().zoom(NAV).map(|l| l.zoomed), Ok(Some(NAV)));
        assert_eq!(base().zoom(99), Err(OpError::NotFound(99)));
    }

    #[test]
    fn closing_or_moving_the_zoomed_panel_drops_zoom() {
        let zoomed = two_center().zoom(3).unwrap();
        let closed = zoomed.close(3).unwrap();
        valid(&closed);
        assert_eq!(closed.zoomed, None);
        assert_eq!(closed.focused(), Some(2));

        let moved = zoomed.move_to_zone(3, Zone::Right, false).unwrap();
        valid(&moved);
        assert_eq!(moved.zoomed, None);
        assert_eq!(moved.focused(), Some(3));
        assert_eq!(tabs(&moved, Zone::Right), vec![vec![3]]);

        let other = zoomed.move_to_zone(2, Zone::Bottom, false).unwrap();
        valid(&other);
        assert_eq!(
            other.zoomed,
            Some(3),
            "перенос другой панели zoom не снимает"
        );
    }

    #[test]
    fn unzoom_after_the_zone_was_hidden_picks_a_visible_focus() {
        let zoomed = with_right().zoom(40).unwrap();
        let hidden = zoomed.set_visible(Hideable::Right, false).unwrap();
        valid(&hidden);
        assert_eq!(hidden.focused(), Some(40), "развёрнутая панель видна");
        let restored = hidden.unzoom();
        valid(&restored);
        assert_eq!(restored.focused(), Some(TERM));
    }

    #[test]
    fn only_bottom_and_back_restores_the_frames() {
        let mut layout = base();
        singles(&mut layout, Zone::Bottom, &[(30, 500), (31, 500)]);
        let layout = layout.focus_panel(30).unwrap();
        let before = frames(&layout, SMALL);
        let only = layout.toggle_only();
        valid(&only);
        assert_eq!(only.only, Some(Zone::Bottom));
        assert_eq!(only.hidden, layout.hidden);
        let f = frames(&only, SMALL);
        assert_eq!(f.parts.len(), 2);
        assert!(f.parts.iter().all(|part| part.zone == Zone::Bottom));
        assert_eq!(f.navigator, None);

        let back = only.toggle_only();
        valid(&back);
        assert_eq!(back.only, None);
        assert_eq!(frames(&back, SMALL), before);

        let shown = only.set_visible(Hideable::Navigator, true).unwrap();
        assert_eq!(shown.only, None, "показ другой зоны снимает режим");
        let mut single = base();
        singles(&mut single, Zone::Bottom, &[(30, SHARE_TOTAL)]);
        let only = single.focus_panel(30).unwrap().toggle_only();
        let closed = only.close(30).unwrap();
        valid(&closed);
        assert_eq!(closed.only, None, "зона режима исчезла");
        assert_eq!(closed.focused(), Some(TERM));
    }

    #[test]
    fn equal_spreads_shares_and_refuses_an_empty_zone() {
        let mut layout = base();
        singles(&mut layout, Zone::Center, &[(2, 100), (3, 200), (5, 700)]);
        let equal = layout.equal(Zone::Center).unwrap();
        valid(&equal);
        assert_eq!(shares(&equal, Zone::Center), vec![333, 333, 334]);
        assert_eq!(
            base().equal(Zone::Bottom),
            Err(OpError::ZoneEmpty(Zone::Bottom))
        );
    }

    #[test]
    fn resize_zone_bounds_match_where_the_stub_flips() {
        let layout = with_right();
        let resize = |l: &Layout<u32>, size| l.resize_zone(Zone::Right, size, SMALL, &METRICS, min);
        let range = Err(OpError::ResizeOutOfRange { min: 202, max: 730 });
        assert_eq!(resize(&layout, 201), range);
        assert_eq!(resize(&layout, 731), range);
        for (size, right_stub, center_stub) in [(202, false, false), (730, false, false)] {
            let resized = resize(&layout, size).unwrap();
            valid(&resized);
            assert_eq!(resized.sizes.right, size);
            let f = frames(&resized, SMALL);
            assert_eq!(f.part_of(&40).unwrap().stub, right_stub);
            assert_eq!(f.part_of(&TERM).unwrap().stub, center_stub);
        }
        let mut beyond = layout.clone();
        beyond.sizes.right = 201;
        assert!(frames(&beyond, SMALL).part_of(&40).unwrap().stub);
        beyond.sizes.right = 731;
        assert!(frames(&beyond, SMALL).part_of(&TERM).unwrap().stub);

        let mut bottom = base();
        singles(&mut bottom, Zone::Bottom, &[(30, SHARE_TOTAL)]);
        assert_eq!(
            bottom.resize_zone(Zone::Bottom, 155, SMALL, &METRICS, min),
            Err(OpError::ResizeOutOfRange { min: 156, max: 480 })
        );
        let low = bottom
            .resize_zone(Zone::Bottom, 156, SMALL, &METRICS, min)
            .unwrap();
        assert!(!frames(&low, SMALL).part_of(&30).unwrap().stub);
        bottom.sizes.bottom = 155;
        assert!(frames(&bottom, SMALL).part_of(&30).unwrap().stub);

        assert_eq!(
            base().resize_zone(Zone::Center, 300, SMALL, &METRICS, min),
            Err(OpError::ZoneNotResizable(Zone::Center))
        );
        assert_eq!(
            base().resize_zone(Zone::Left, 300, SMALL, &METRICS, min),
            Err(OpError::ZoneEmpty(Zone::Left))
        );
    }

    #[test]
    fn resize_of_the_plate_bounds_and_frames() {
        let mut layout = base();
        singles(&mut layout, Zone::Left, &[(10, SHARE_TOTAL)]);
        let resize = |l: &Layout<u32>, size| l.resize_zone(Zone::Left, size, SMALL, &METRICS, min);
        let resized = resize(&layout, 300).unwrap();
        valid(&resized);
        assert_eq!(resized.sizes.plate, 300);
        assert_eq!(frames(&resized, SMALL).zones.left.unwrap().width, 300.0);
        let range = Err(OpError::ResizeOutOfRange { min: 202, max: 730 });
        assert_eq!(resize(&layout, 201), range);
        assert_eq!(resize(&layout, 731), range);

        // Без столбца правая зона получает свободную ширину и сама становится остатком.
        let mut free = with_right();
        singles(&mut free, Zone::Left, &[(10, SHARE_TOTAL)]);
        let free = free.close(TERM).unwrap();
        assert_eq!(frames(&free, SMALL).zones.column, None);
        assert_eq!(resize(&free, 731), range);
        let widest = resize(&free, 730).unwrap();
        let f = frames(&widest, SMALL);
        assert!(
            !f.part_of(&40).unwrap().stub,
            "правая зона не меньше min_size"
        );
        assert!(!f.part_of(&10).unwrap().stub);
        assert_eq!(
            free.resize_zone(Zone::Right, 250, SMALL, &METRICS, min)
                .map(|l| l.sizes.right),
            Ok(250),
            "размер гибкой зоны — предпочтение до возвращения столбца"
        );
    }

    #[test]
    fn resize_of_a_hidden_zone_is_checked_as_shown_and_keeps_it_hidden() {
        let hidden = with_right().set_visible(Hideable::Right, false).unwrap();
        assert_eq!(
            hidden.resize_zone(Zone::Right, 731, SMALL, &METRICS, min),
            Err(OpError::ResizeOutOfRange { min: 202, max: 730 })
        );
        let resized = hidden
            .resize_zone(Zone::Right, 300, SMALL, &METRICS, min)
            .unwrap();
        assert!(resized.hidden.right);
        assert_eq!(resized.sizes.right, 300);
    }

    #[test]
    fn resize_in_a_tight_window_keeps_the_current_size_allowed() {
        let mut layout = with_right();
        singles(&mut layout, Zone::Left, &[(10, SHARE_TOTAL)]);
        assert!(frames(&layout, SMALL).part_of(&TERM).unwrap().stub);
        let resize = |size| layout.resize_zone(Zone::Right, size, SMALL, &METRICS, min);
        assert_eq!(
            resize(372),
            Ok(layout.clone()),
            "текущий размер — без изменений"
        );
        assert_eq!(resize(360).map(|l| l.sizes.right), Ok(360));
        assert_eq!(
            resize(373),
            Err(OpError::ResizeOutOfRange { min: 202, max: 372 })
        );
    }

    #[test]
    fn a_part_tight_across_the_axis_does_not_lock_the_resize() {
        let mut layout = two_center();
        singles(&mut layout, Zone::Bottom, &[(30, SHARE_TOTAL)]);
        singles(&mut layout, Zone::Right, &[(40, SHARE_TOTAL)]);
        layout.sizes.bottom = 500;
        layout.hidden.right = true;
        let f = frames(&layout, SMALL);
        assert!(f.part_of(&2).unwrap().stub && f.part_of(&3).unwrap().stub);
        assert!(body(&f, 2).height < 120.0, "центр тесный по высоте");

        let part = layout
            .resize_part(Zone::Center, 2, 600, SMALL, &METRICS, min)
            .unwrap();
        assert_eq!(shares(&part, Zone::Center), vec![600, 400]);
        assert_eq!(
            layout.resize_part(Zone::Center, 2, 800, SMALL, &METRICS, min),
            Err(OpError::ResizeOutOfRange { min: 217, max: 784 })
        );
        let shown = layout.set_visible(Hideable::Right, true).unwrap();
        assert_eq!(
            shown
                .resize_zone(Zone::Right, 522, SMALL, &METRICS, min)
                .map(|l| l.sizes.right),
            Ok(522),
            "центр 410 точек: две части по 202"
        );
        assert_eq!(
            shown.resize_zone(Zone::Right, 523, SMALL, &METRICS, min),
            Err(OpError::ResizeOutOfRange { min: 202, max: 522 })
        );
    }

    #[test]
    fn resize_part_moves_the_divider_with_the_next_part() {
        let layout = two_center();
        let resize =
            |panel, share| layout.resize_part(Zone::Center, panel, share, SMALL, &METRICS, min);
        let range = Err(OpError::ResizeOutOfRange { min: 217, max: 784 });
        assert_eq!(resize(2, 216), range);
        assert_eq!(resize(2, 785), range);
        let wide = resize(2, 784).unwrap();
        valid(&wide);
        assert_eq!(shares(&wide, Zone::Center), vec![784, 216]);
        assert!(!frames(&wide, SMALL).part_of(&3).unwrap().stub);
        let narrow = resize(2, 217).unwrap();
        assert!(!frames(&narrow, SMALL).part_of(&2).unwrap().stub);
        let mut edge = layout.clone();
        singles(&mut edge, Zone::Center, &[(2, 216), (3, 784)]);
        assert!(frames(&edge, SMALL).part_of(&2).unwrap().stub);
        singles(&mut edge, Zone::Center, &[(2, 785), (3, 215)]);
        assert!(frames(&edge, SMALL).part_of(&3).unwrap().stub);

        let last = resize(3, 300).unwrap();
        assert_eq!(
            shares(&last, Zone::Center),
            vec![700, 300],
            "последняя часть — с предыдущей"
        );
        assert_eq!(resize(2, 500), Ok(layout.clone()));
    }

    #[test]
    fn resize_part_refusals() {
        let layout = base();
        let resize =
            |zone, panel, share| layout.resize_part(zone, panel, share, SMALL, &METRICS, min);
        assert_eq!(resize(Zone::Center, TERM, SHARE_TOTAL), Ok(base()));
        assert_eq!(
            resize(Zone::Center, TERM, 500),
            Err(OpError::ResizeOutOfRange {
                min: 1000,
                max: 1000
            })
        );
        assert_eq!(
            resize(Zone::Right, TERM, 500),
            Err(OpError::NotInZone {
                panel: TERM,
                zone: Zone::Right
            })
        );
        assert_eq!(
            resize(Zone::Left, NAV, 500),
            Err(OpError::NavigatorColumn(NAV))
        );
        assert_eq!(resize(Zone::Center, 99, 500), Err(OpError::NotFound(99)));
    }

    /// Колонка навигатора, плитка 10, центр 2 и 3 (доли 400 и 600), нижняя 30, правая 40.
    fn crossroads() -> Layout<u32> {
        let mut layout = base();
        singles(&mut layout, Zone::Left, &[(10, SHARE_TOTAL)]);
        singles(&mut layout, Zone::Center, &[(2, 400), (3, 600)]);
        singles(&mut layout, Zone::Bottom, &[(30, SHARE_TOTAL)]);
        singles(&mut layout, Zone::Right, &[(40, SHARE_TOTAL)]);
        valid(&layout);
        layout
    }

    #[test]
    fn focus_direction_walks_to_the_nearest_overlapping_neighbour() {
        let layout = crossroads();
        for viewport in [Some(LARGE), None] {
            let step =
                |l: &Layout<u32>, direction| l.focus_direction(direction, viewport, &METRICS);
            let walk = |from: u32, direction| {
                step(&layout.focus_panel(from).unwrap(), direction).focused()
            };
            assert_eq!(walk(2, Direction::Right), Some(3));
            assert_eq!(walk(3, Direction::Right), Some(40));
            assert_eq!(
                walk(40, Direction::Left),
                Some(3),
                "центр перекрывается больше нижней"
            );
            assert_eq!(walk(2, Direction::Down), Some(30));
            assert_eq!(walk(30, Direction::Up), Some(3), "большее перекрытие");
            assert_eq!(walk(2, Direction::Left), Some(10));
            assert_eq!(walk(10, Direction::Left), Some(NAV));
            assert_eq!(walk(NAV, Direction::Right), Some(10));
            let edge = layout.focus_panel(NAV).unwrap();
            assert_eq!(step(&edge, Direction::Left), edge, "соседа нет");
            let top = layout.focus_panel(2).unwrap();
            assert_eq!(step(&top, Direction::Up), top);
            let moved = step(&top, Direction::Right);
            valid(&moved);
            assert_eq!(moved.zones.center.last_active, 1);
        }
        let zoomed = layout.zoom(2).unwrap();
        assert_eq!(
            zoomed.focus_direction(Direction::Right, Some(LARGE), &METRICS),
            zoomed
        );
    }

    #[test]
    fn stub_expand_keeps_the_split_and_zooms_the_stubbed_panel() {
        let hidden = with_right().set_visible(Hideable::Right, false).unwrap();
        let two = split(&hidden, TERM, Some(3)).unwrap();
        let three = split(&two, 3, Some(5)).unwrap();
        valid(&three);
        let f = frames(&three, SMALL);
        assert!(
            f.parts
                .iter()
                .all(|part| part.body.width > 200.0 && !part.stub)
        );

        let shown = three.set_visible(Hideable::Right, true).unwrap();
        valid(&shown);
        assert_eq!(shown.zones.center, three.zones.center, "деление сохранено");
        let f = frames(&shown, SMALL);
        let stubs: Vec<u32> = f
            .parts
            .iter()
            .filter(|p| p.stub)
            .map(|p| p.active)
            .collect();
        assert_eq!(stubs, vec![3, 5]);

        let zoomed = shown.zoom(5).unwrap();
        valid(&zoomed);
        let f = frames(&zoomed, SMALL);
        assert!(!f.zoomed.unwrap().stub);
    }

    #[test]
    fn center_empty_gives_the_column_to_the_bottom_zone() {
        let moved = base().move_to_zone(TERM, Zone::Bottom, false).unwrap();
        valid(&moved);
        assert!(moved.zones.center.is_empty());
        let f = frames(&moved, SMALL);
        assert_eq!(f.zones.center, None);
        assert_eq!(f.zones.bottom, f.zones.column);
        assert_eq!(f.no_panels, None);
        assert_eq!(moved.focused(), Some(TERM));
    }

    #[test]
    fn narrowing_the_window_changes_nothing_and_ops_keep_working() {
        let layout = crossroads();
        let tiny = Viewport {
            width: 400,
            height: 300,
        };
        assert!(frames(&layout, tiny).parts.iter().all(|part| part.stub));
        let hidden = layout.set_visible(Hideable::Right, false).unwrap();
        let shown = hidden.set_visible(Hideable::Right, true).unwrap();
        assert_eq!(shown, layout);
        let moved = layout.move_to_zone(3, Zone::Right, false).unwrap();
        valid(&moved);
    }

    #[test]
    fn no_op_returns_an_equal_layout() {
        let layout = crossroads();
        assert_eq!(layout.focus_panel(TERM), Ok(layout.clone()));
        assert_eq!(layout.select(TERM), Ok(layout.clone()));
        assert_eq!(
            layout.set_visible(Hideable::Right, true),
            Ok(layout.clone())
        );
        assert_eq!(layout.unzoom(), layout);
        assert_eq!(
            layout.move_to_zone(TERM, Zone::Center, false),
            Ok(layout.clone())
        );
        let equal = layout.equal(Zone::Bottom).unwrap();
        assert_eq!(equal, layout);
        assert_eq!(
            layout.resize_zone(Zone::Right, 372, LARGE, &METRICS, min),
            Ok(layout.clone())
        );
        let zoomed = layout.zoom(TERM).unwrap();
        assert_eq!(zoomed.zoom(TERM), Ok(zoomed.clone()));
    }

    #[test]
    fn errors_render_in_russian() {
        assert_eq!(
            OpError::NotFound(5u32).to_string(),
            "панели 5 нет в раскладке"
        );
        assert_eq!(
            OpError::<u32>::ResizeOutOfRange { min: 1, max: 2 }.to_string(),
            "размер вне границ 1..=2"
        );
    }

    /// Псевдослучайная последовательность без внешних зависимостей.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, bound: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) as usize) % bound
        }
    }

    #[test]
    fn random_operations_keep_every_invariant() {
        let viewports = [
            SMALL,
            LARGE,
            Viewport {
                width: 900,
                height: 600,
            },
        ];
        let zones = Zone::ALL;
        let targets = [
            Hideable::Navigator,
            Hideable::Plate,
            Hideable::Right,
            Hideable::Bottom,
        ];
        let directions = [
            Direction::Left,
            Direction::Down,
            Direction::Up,
            Direction::Right,
        ];
        // Успехи по виду операции за все зёрна: операция, которая всегда отказывает, роняет тест.
        let mut succeeded = [0usize; 15];
        for seed in 1..=6u64 {
            let mut rng = Lcg(seed);
            let mut layout = base();
            let mut alive: BTreeSet<u32> = BTreeSet::from([NAV, TERM]);
            let mut fresh = 100u32;
            let mut applied = 0;
            let mut most_parts = 0;
            for step in 0..2500 {
                let pool: Vec<u32> = alive.iter().copied().chain([999]).collect();
                let pick = |rng: &mut Lcg| pool[rng.next(pool.len())];
                let viewport = viewports[rng.next(viewports.len())];
                let zone = zones[rng.next(zones.len())];
                let mut expected = alive.clone();
                let op = rng.next(15);
                let result = match op {
                    0 | 1 => {
                        fresh += 1;
                        expected.insert(fresh);
                        let zone = (rng.next(2) == 0).then_some(zone);
                        layout.open(fresh, zone)
                    }
                    2 => {
                        let open = (rng.next(2) == 0).then(|| {
                            fresh += 1;
                            fresh
                        });
                        if let Some(new) = open {
                            expected.insert(new);
                        }
                        let panel = pick(&mut rng);
                        layout.split(panel, open, viewport, &METRICS, min)
                    }
                    3 => {
                        let panel = pick(&mut rng);
                        layout.move_to_zone(panel, zone, rng.next(2) == 0)
                    }
                    4 => {
                        let (panel, anchor) = (pick(&mut rng), pick(&mut rng));
                        let edge = [None, Some(Edge::Before), Some(Edge::After)][rng.next(3)];
                        layout.move_to_part(panel, anchor, edge)
                    }
                    5 => {
                        let panel = pick(&mut rng);
                        expected.remove(&panel);
                        layout.close(panel)
                    }
                    6 => layout.select(pick(&mut rng)),
                    7 => Ok(layout.focus_direction(
                        directions[rng.next(4)],
                        (rng.next(2) == 0).then_some(viewport),
                        &METRICS,
                    )),
                    8 => {
                        let size = u32::try_from(rng.next(900)).unwrap();
                        layout.resize_zone(zone, size, viewport, &METRICS, min)
                    }
                    9 => {
                        let share = u16::try_from(rng.next(1001)).unwrap();
                        let panel = pick(&mut rng);
                        layout.resize_part(zone, panel, share, viewport, &METRICS, min)
                    }
                    10 => layout.equal(zone),
                    11 => layout.zoom(pick(&mut rng)),
                    12 => Ok(layout.unzoom()),
                    13 => layout.set_visible(targets[rng.next(4)], rng.next(3) != 0),
                    _ => Ok(layout.toggle_only()),
                };
                if let Ok(next) = result {
                    assert_eq!(
                        next.validate(&expected),
                        Ok(()),
                        "seed {seed}, step {step}: {next:?}"
                    );
                    layout = next;
                    alive = expected;
                    applied += 1;
                    succeeded[op] += 1;
                    most_parts = most_parts.max(
                        layout
                            .zones
                            .iter()
                            .map(|(_, state)| state.parts.len())
                            .sum::<usize>(),
                    );
                }
            }
            assert!(applied > 1000, "seed {seed}: применено {applied}");
            assert!(
                most_parts >= 4,
                "seed {seed}: частей не больше {most_parts}"
            );
        }
        for (op, count) in succeeded.iter().enumerate() {
            assert!(*count > 0, "операция {op} ни разу не прошла: {succeeded:?}");
        }
    }

    fn by(
        layout: &Layout<u32>,
        panel: u32,
        dimension: Dimension,
        delta: i32,
    ) -> Result<Layout<u32>, OpError<u32>> {
        layout.resize_by(panel, dimension, delta, SMALL, &METRICS, min)
    }

    #[test]
    fn resize_by_moves_the_zone_size_across_the_zone_axis() {
        let layout = with_right();
        let wider = by(&layout, 40, Dimension::Width, 40).unwrap();
        valid(&wider);
        assert_eq!(wider.sizes.right, 412);
        assert_eq!(by(&wider, 40, Dimension::Width, -40).unwrap(), layout);
        // Единственная часть центра шире — правая зона уже.
        assert_eq!(
            by(&layout, TERM, Dimension::Width, 40).unwrap().sizes.right,
            332
        );

        let mut bottom = base();
        singles(&mut bottom, Zone::Bottom, &[(30, SHARE_TOTAL)]);
        assert_eq!(
            by(&bottom, 30, Dimension::Height, 40).unwrap().sizes.bottom,
            276
        );
        // Высота центра переводится в размер нижней зоны.
        assert_eq!(
            by(&bottom, TERM, Dimension::Height, 40)
                .unwrap()
                .sizes
                .bottom,
            196
        );

        let mut plate = base();
        singles(&mut plate, Zone::Left, &[(20, SHARE_TOTAL)]);
        assert_eq!(
            by(&plate, 20, Dimension::Width, -40).unwrap().sizes.plate,
            332
        );
    }

    #[test]
    fn resize_by_refuses_past_min_size_and_without_a_zone() {
        let layout = with_right();
        assert_eq!(
            by(&layout, 40, Dimension::Width, -200),
            Err(OpError::ResizeOutOfRange { min: 202, max: 730 })
        );
        assert_eq!(
            by(&base(), TERM, Dimension::Height, 40),
            Err(OpError::ZoneEmpty(Zone::Bottom))
        );
        assert_eq!(
            by(&base(), TERM, Dimension::Width, 40),
            Err(OpError::ZoneEmpty(Zone::Right))
        );
        // Единственной части вдоль оси зоны меняться не за чей счёт.
        let Err(OpError::ResizeOutOfRange { min, max }) = by(&layout, 40, Dimension::Height, 40)
        else {
            panic!("ожидался отказ размера");
        };
        assert!(min == max && min > 0, "{min}..={max}");
        assert_eq!(by(&layout, 40, Dimension::Height, 0).unwrap(), layout);
        assert_eq!(
            by(&layout, 99, Dimension::Width, 40),
            Err(OpError::NotFound(99))
        );
    }

    #[test]
    fn resize_by_moves_the_share_of_a_part_along_the_zone_axis() {
        let layout = two_center();
        let width =
            |l: &Layout<u32>, panel: u32| frames(l, SMALL).part_of(&panel).unwrap().frame.width;
        let wider = by(&layout, 2, Dimension::Width, 40).unwrap();
        valid(&wider);
        let grown = shares(&wider, Zone::Center);
        assert!(grown[0] > 500, "{grown:?}");
        assert_eq!(grown[0] + grown[1], SHARE_TOTAL);
        let step = width(&wider, 2) - width(&layout, 2);
        assert!((step - 40.0).abs() <= 2.0, "шаг {step}");
        // Последняя часть растёт за счёт предыдущей.
        let last = by(&layout, 3, Dimension::Width, 40).unwrap();
        assert_eq!(shares(&last, Zone::Center), [grown[1], grown[0]]);
        // Границы отказа — длина части в точках.
        let Err(OpError::ResizeOutOfRange { min, max }) = by(&layout, 2, Dimension::Width, 4000)
        else {
            panic!("ожидался отказ размера");
        };
        assert!(min >= 200 && min < max, "{min}..={max}");
        assert!(f64::from(max) < 2.0 * width(&layout, 2), "{max}");
    }

    #[test]
    fn resize_by_in_the_navigator_column_changes_only_its_width() {
        let layout = base();
        let wider = by(&layout, NAV, Dimension::Width, 40).unwrap();
        valid(&wider);
        assert_eq!(wider.sizes.navigator, 276);
        assert_eq!(by(&wider, NAV, Dimension::Width, -40).unwrap(), layout);
        assert_eq!(
            by(&layout, NAV, Dimension::Height, 40),
            Err(OpError::NavigatorColumn(NAV))
        );
        let Err(OpError::ResizeOutOfRange { min, max }) = by(&layout, NAV, Dimension::Width, -230)
        else {
            panic!("ожидался отказ размера");
        };
        assert!((200..=236).contains(&min) && max > 236, "{min}..={max}");
        assert_eq!(
            by(&layout, NAV, Dimension::Width, 4000),
            Err(OpError::ResizeOutOfRange { min, max })
        );
        // На границах ни колонка, ни центр не уходят в заглушку.
        for size in [min, max] {
            let resized = layout
                .resize_navigator(size, SMALL, &METRICS, min_size_of)
                .unwrap();
            let f = frames(&resized, SMALL);
            assert!(!f.navigator.as_ref().unwrap().stub, "{size}");
            assert!(!f.part_of(&TERM).unwrap().stub, "{size}");
        }
    }

    fn min_size_of(panel: u32) -> MinSize {
        min(panel)
    }

    #[test]
    fn tab_step_wraps_and_tab_at_ignores_a_missing_tab() {
        let layout = base().open(3, None).unwrap().open(4, None).unwrap();
        assert_eq!(tabs(&layout, Zone::Center), [vec![2, 3, 4]]);
        assert_eq!(layout.focused(), Some(4));
        let next = layout.tab_step(true);
        valid(&next);
        assert_eq!(next.focused(), Some(2), "за последней вкладкой — первая");
        assert_eq!(
            next.tab_step(false).focused(),
            Some(4),
            "перед первой — последняя"
        );
        assert_eq!(layout.tab_step(false).focused(), Some(3));
        assert_eq!(layout.tab_at(0).focused(), Some(2));
        assert_eq!(layout.tab_at(2), layout, "вкладка уже активна");
        assert_eq!(layout.tab_at(3), layout, "вкладки нет");
        assert_eq!(
            tabs(&next, Zone::Center),
            [vec![2, 3, 4]],
            "порядок прежний"
        );

        // Одна вкладка и колонка навигатора.
        assert_eq!(base().tab_step(true), base());
        let navigator = base().focus_panel(NAV).unwrap();
        assert_eq!(navigator.tab_step(true), navigator);
        assert_eq!(navigator.tab_at(0), navigator);
    }
}
