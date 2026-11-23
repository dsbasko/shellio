//! Модель раскладки рабочей области и её инварианты (ADR-0009, «Уточнено в P1.4 (планирование)»).

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Сумма долей частей непустой зоны в тысячных.
pub const SHARE_TOTAL: u16 = 1000;

/// Зона рабочей области. Везде разбирается исчерпывающим `match` без `_`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Zone {
    Left,
    Right,
    Center,
    Bottom,
}

impl Zone {
    /// Все зоны в порядке обхода модели.
    pub const ALL: [Zone; 4] = [Zone::Left, Zone::Right, Zone::Center, Zone::Bottom];

    /// Ось деления зоны (ADR-0031).
    #[must_use]
    pub const fn axis(self) -> Axis {
        match self {
            Zone::Left | Zone::Right => Axis::Stacked,
            Zone::Center | Zone::Bottom => Axis::SideBySide,
        }
    }
}

/// Ось, вдоль которой стоят части зоны.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// Одна над другой.
    Stacked,
    /// Рядом.
    SideBySide,
}

/// Значение для каждой зоны.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneMap<T> {
    pub left: T,
    pub right: T,
    pub center: T,
    pub bottom: T,
}

impl<T> ZoneMap<T> {
    #[must_use]
    pub fn get(&self, zone: Zone) -> &T {
        match zone {
            Zone::Left => &self.left,
            Zone::Right => &self.right,
            Zone::Center => &self.center,
            Zone::Bottom => &self.bottom,
        }
    }

    pub fn get_mut(&mut self, zone: Zone) -> &mut T {
        match zone {
            Zone::Left => &mut self.left,
            Zone::Right => &mut self.right,
            Zone::Center => &mut self.center,
            Zone::Bottom => &mut self.bottom,
        }
    }

    /// Пары зона — значение в порядке [`Zone::ALL`].
    pub fn iter(&self) -> impl Iterator<Item = (Zone, &T)> {
        Zone::ALL
            .into_iter()
            .map(move |zone| (zone, self.get(zone)))
    }
}

/// Части зоны вдоль её оси. У левой зоны это плитка рядом с колонкой навигатора.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneState<P> {
    pub parts: Vec<Part<P>>,
    /// Последняя активная часть. У зоны без частей — 0.
    pub last_active: usize,
}

impl<P> Default for ZoneState<P> {
    fn default() -> Self {
        Self {
            parts: Vec::new(),
            last_active: 0,
        }
    }
}

impl<P> ZoneState<P> {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

/// Часть зоны: вкладки, активная вкладка и доля зоны в тысячных.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Part<P> {
    pub tabs: Vec<P>,
    pub active: usize,
    pub share: u16,
}

impl<P: Copy> Part<P> {
    /// Часть из одной вкладки.
    #[must_use]
    pub fn single(panel: P, share: u16) -> Self {
        Self {
            tabs: vec![panel],
            active: 0,
            share,
        }
    }

    /// Активная вкладка. `None` только у части, нарушающей инвариант.
    #[must_use]
    pub fn active_panel(&self) -> Option<P> {
        self.tabs.get(self.active).copied()
    }
}

/// Сохранённые размеры зон в точках. Это предпочтения: кадры считаются по текущему окну.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneSizes {
    /// Ширина колонки навигатора.
    pub navigator: u32,
    /// Ширина плитки левой зоны.
    pub plate: u32,
    /// Ширина правой зоны.
    pub right: u32,
    /// Высота нижней зоны.
    pub bottom: u32,
}

impl ZoneSizes {
    /// Размеры по умолчанию: колонка навигатора ADR-0024, остальное — `L.size` прототипа.
    pub const DEFAULT: Self = Self {
        navigator: 236,
        plate: 372,
        right: 372,
        bottom: 236,
    };
}

impl Default for ZoneSizes {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Скрытые зоны. Центр не скрывается.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hidden {
    pub navigator: bool,
    pub plate: bool,
    pub right: bool,
    pub bottom: bool,
}

/// Наименьший размер тела панели в точках, ниже которого часть показывает заглушку.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinSize {
    pub width: f64,
    pub height: f64,
}

impl MinSize {
    /// `min_size` по умолчанию. Манифест плагина в P1.5 может задать свой.
    pub const DEFAULT: Self = Self {
        width: 200.0,
        height: 120.0,
    };

    /// Тело такого размера не меньше `min_size`: заглушка не нужна.
    #[must_use]
    pub fn admits(&self, width: f64, height: f64) -> bool {
        width >= self.width && height >= self.height
    }
}

impl Default for MinSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Раскладка рабочей области.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout<P> {
    /// Панель колонки навигатора левой зоны.
    pub navigator: Option<P>,
    /// Части зон. У левой зоны — плитка рядом с колонкой навигатора.
    pub zones: ZoneMap<ZoneState<P>>,
    pub sizes: ZoneSizes,
    pub hidden: Hidden,
    /// Видна только эта зона.
    pub only: Option<Zone>,
    pub zoomed: Option<P>,
    /// История фокуса без повторов, последняя запись — панель с фокусом.
    pub focus: Vec<P>,
}

/// Место панели в раскладке.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Location {
    /// Колонка навигатора.
    Navigator,
    /// Вкладка `tab` части `part` зоны `zone`.
    Part { zone: Zone, part: usize, tab: usize },
}

impl Location {
    /// Зона места: колонка навигатора относится к левой зоне.
    #[must_use]
    pub const fn zone(self) -> Zone {
        match self {
            Self::Navigator => Zone::Left,
            Self::Part { zone, .. } => zone,
        }
    }
}

impl<P: Copy + Ord> Layout<P> {
    /// Раскладка по умолчанию: `navigator` в колонке, `terminal` в центре с фокусом.
    #[must_use]
    pub fn default_for(navigator: P, terminal: P) -> Self {
        let mut zones: ZoneMap<ZoneState<P>> = ZoneMap::default();
        zones.center.parts.push(Part::single(terminal, SHARE_TOTAL));
        Self {
            navigator: Some(navigator),
            zones,
            sizes: ZoneSizes::DEFAULT,
            hidden: Hidden::default(),
            only: None,
            zoomed: None,
            focus: vec![terminal],
        }
    }

    /// Все панели раскладки: колонка навигатора, затем вкладки частей в порядке [`Zone::ALL`].
    pub fn panels(&self) -> impl Iterator<Item = P> + '_ {
        self.navigator.into_iter().chain(
            self.zones
                .iter()
                .flat_map(|(_, state)| state.parts.iter())
                .flat_map(|part| part.tabs.iter().copied()),
        )
    }

    /// Первое место панели в раскладке.
    #[must_use]
    pub fn locate(&self, panel: P) -> Option<Location> {
        if self.navigator == Some(panel) {
            return Some(Location::Navigator);
        }
        self.zones.iter().find_map(|(zone, state)| {
            state.parts.iter().enumerate().find_map(|(part, p)| {
                p.tabs
                    .iter()
                    .position(|&t| t == panel)
                    .map(|tab| Location::Part { zone, part, tab })
            })
        })
    }

    /// Панель с фокусом.
    #[must_use]
    pub fn focused(&self) -> Option<P> {
        self.focus.last().copied()
    }

    /// Развёрнутая панель, если она есть в раскладке.
    #[must_use]
    pub fn zoomed_panel(&self) -> Option<P> {
        self.zoomed.filter(|&p| self.locate(p).is_some())
    }

    fn only_allows(&self, zone: Zone) -> bool {
        self.only.is_none_or(|only| only == zone)
    }

    /// Колонка навигатора показана при текущем скрытии и `only`, без учёта `zoom`.
    #[must_use]
    pub fn navigator_shown(&self) -> bool {
        self.navigator.is_some() && !self.hidden.navigator && self.only_allows(Zone::Left)
    }

    /// Части зоны показаны при текущем скрытии и `only`, без учёта `zoom`. Зона без частей не показана.
    #[must_use]
    pub fn zone_shown(&self, zone: Zone) -> bool {
        let unhidden = match zone {
            Zone::Left => !self.hidden.plate,
            Zone::Right => !self.hidden.right,
            Zone::Center => true,
            Zone::Bottom => !self.hidden.bottom,
        };
        unhidden && !self.zones.get(zone).is_empty() && self.only_allows(zone)
    }

    /// Панель видна: при `zoom` — только развёрнутая; иначе панель колонки навигатора при показанной
    /// колонке и активная вкладка части показанной зоны. Неактивная вкладка не видна.
    #[must_use]
    pub fn is_visible(&self, panel: P) -> bool {
        if let Some(zoomed) = self.zoomed_panel() {
            return panel == zoomed;
        }
        match self.locate(panel) {
            None => false,
            Some(Location::Navigator) => self.navigator_shown(),
            Some(Location::Part { zone, part, tab }) => {
                self.zone_shown(zone) && self.zones.get(zone).parts[part].active == tab
            }
        }
    }

    /// Проверка инвариантов против множества живых панелей рабочей области.
    ///
    /// # Errors
    ///
    /// Первое найденное нарушение.
    pub fn validate(&self, panels: &BTreeSet<P>) -> Result<(), LayoutError<P>> {
        for (zone, state) in self.zones.iter() {
            validate_zone(zone, state)?;
        }

        let mut seen = BTreeSet::new();
        for panel in self.panels() {
            if !seen.insert(panel) {
                return Err(LayoutError::DuplicatePanel(panel));
            }
            if !panels.contains(&panel) {
                return Err(LayoutError::UnknownPanel(panel));
            }
        }
        if let Some(&missing) = panels.difference(&seen).next() {
            return Err(LayoutError::MissingPanel(missing));
        }

        if let Some(zoomed) = self.zoomed
            && !seen.contains(&zoomed)
        {
            return Err(LayoutError::UnknownZoomed(zoomed));
        }

        let mut history = BTreeSet::new();
        for &entry in &self.focus {
            if !seen.contains(&entry) {
                return Err(LayoutError::UnknownFocus(entry));
            }
            if !history.insert(entry) {
                return Err(LayoutError::DuplicateFocus(entry));
            }
        }
        if seen.iter().any(|&p| self.is_visible(p)) {
            match self.focused() {
                None => return Err(LayoutError::NoFocus),
                Some(focused) if !self.is_visible(focused) => {
                    return Err(LayoutError::FocusHidden(focused));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

fn validate_zone<P>(zone: Zone, state: &ZoneState<P>) -> Result<(), LayoutError<P>> {
    let parts = state.parts.len();
    let last_in_range = if parts == 0 {
        state.last_active == 0
    } else {
        state.last_active < parts
    };
    if !last_in_range {
        return Err(LayoutError::LastActiveOutOfRange {
            zone,
            last_active: state.last_active,
            parts,
        });
    }
    let mut sum = 0u32;
    for (index, part) in state.parts.iter().enumerate() {
        if part.tabs.is_empty() {
            return Err(LayoutError::EmptyPart { zone, part: index });
        }
        if part.active >= part.tabs.len() {
            return Err(LayoutError::ActiveOutOfRange {
                zone,
                part: index,
                active: part.active,
                tabs: part.tabs.len(),
            });
        }
        if part.share == 0 {
            return Err(LayoutError::ZeroShare { zone, part: index });
        }
        sum += u32::from(part.share);
    }
    if parts > 0 && sum != u32::from(SHARE_TOTAL) {
        return Err(LayoutError::ShareSum { zone, sum });
    }
    Ok(())
}

/// Нарушение инварианта раскладки.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutError<P> {
    /// `last_active` вне частей зоны, у зоны без частей — не 0.
    LastActiveOutOfRange {
        zone: Zone,
        last_active: usize,
        parts: usize,
    },
    EmptyPart {
        zone: Zone,
        part: usize,
    },
    ActiveOutOfRange {
        zone: Zone,
        part: usize,
        active: usize,
        tabs: usize,
    },
    ZeroShare {
        zone: Zone,
        part: usize,
    },
    /// Доли непустой зоны в сумме не 1000.
    ShareSum {
        zone: Zone,
        sum: u32,
    },
    /// Панель стоит в раскладке больше одного раза.
    DuplicatePanel(P),
    /// Панели раскладки нет среди живых панелей.
    UnknownPanel(P),
    /// Живой панели нет в раскладке.
    MissingPanel(P),
    UnknownZoomed(P),
    UnknownFocus(P),
    DuplicateFocus(P),
    /// Видимые панели есть, а история фокуса пуста.
    NoFocus,
    /// Панель с фокусом не видна, хотя видимые панели есть.
    FocusHidden(P),
}

impl<P: fmt::Debug> fmt::Display for LayoutError<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LastActiveOutOfRange {
                zone,
                last_active,
                parts,
            } => write!(
                f,
                "зона {zone:?}: last_active {last_active} вне частей ({parts})"
            ),
            Self::EmptyPart { zone, part } => write!(f, "зона {zone:?}: часть {part} без вкладок"),
            Self::ActiveOutOfRange {
                zone,
                part,
                active,
                tabs,
            } => write!(
                f,
                "зона {zone:?}: у части {part} активная вкладка {active} вне вкладок ({tabs})"
            ),
            Self::ZeroShare { zone, part } => write!(f, "зона {zone:?}: у части {part} доля 0"),
            Self::ShareSum { zone, sum } => {
                write!(f, "зона {zone:?}: доли в сумме {sum}, а не {SHARE_TOTAL}")
            }
            Self::DuplicatePanel(p) => write!(f, "панель {p:?} стоит в раскладке дважды"),
            Self::UnknownPanel(p) => write!(f, "панели {p:?} нет среди живых панелей"),
            Self::MissingPanel(p) => write!(f, "живой панели {p:?} нет в раскладке"),
            Self::UnknownZoomed(p) => write!(f, "развёрнутой панели {p:?} нет в раскладке"),
            Self::UnknownFocus(p) => write!(f, "панели {p:?} из истории фокуса нет в раскладке"),
            Self::DuplicateFocus(p) => write!(f, "панель {p:?} в истории фокуса дважды"),
            Self::NoFocus => write!(f, "видимые панели есть, а фокуса нет"),
            Self::FocusHidden(p) => write!(f, "панель с фокусом {p:?} не видна"),
        }
    }
}

impl<P: fmt::Debug> std::error::Error for LayoutError<P> {}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(panels: &[u32]) -> BTreeSet<u32> {
        panels.iter().copied().collect()
    }

    /// Раскладка по умолчанию с двумя частями центра и правой зоной: 1 — навигатор, 2 и 3 — центр, 4 — справа.
    fn wide() -> Layout<u32> {
        let mut layout = Layout::default_for(1, 2);
        layout.zones.center.parts = vec![
            Part {
                tabs: vec![2, 5],
                active: 0,
                share: 500,
            },
            Part::single(3, 500),
        ];
        layout.zones.right.parts = vec![Part::single(4, SHARE_TOTAL)];
        layout
    }

    #[test]
    fn axis_is_a_property_of_the_zone() {
        assert_eq!(Zone::Left.axis(), Axis::Stacked);
        assert_eq!(Zone::Right.axis(), Axis::Stacked);
        assert_eq!(Zone::Center.axis(), Axis::SideBySide);
        assert_eq!(Zone::Bottom.axis(), Axis::SideBySide);
    }

    #[test]
    fn default_layout_has_navigator_column_and_focused_terminal() {
        let layout = Layout::default_for(1u32, 2);
        assert_eq!(layout.navigator, Some(1));
        assert_eq!(
            layout.zones.center.parts,
            vec![Part::single(2, SHARE_TOTAL)]
        );
        assert!(layout.zones.left.is_empty());
        assert!(layout.zones.right.is_empty());
        assert!(layout.zones.bottom.is_empty());
        assert_eq!(layout.focused(), Some(2));
        assert_eq!(layout.sizes, ZoneSizes::DEFAULT);
        assert_eq!(layout.sizes.navigator, 236);
        assert_eq!(layout.hidden, Hidden::default());
        assert_eq!(layout.validate(&live(&[1, 2])), Ok(()));
    }

    #[test]
    fn locate_and_panels_cover_column_and_tabs() {
        let layout = wide();
        assert_eq!(layout.panels().collect::<Vec<_>>(), vec![1, 4, 2, 5, 3]);
        assert_eq!(layout.locate(1), Some(Location::Navigator));
        assert_eq!(
            layout.locate(5),
            Some(Location::Part {
                zone: Zone::Center,
                part: 0,
                tab: 1
            })
        );
        assert_eq!(layout.locate(9), None);
        assert_eq!(layout.validate(&live(&[1, 2, 3, 4, 5])), Ok(()));
    }

    #[test]
    fn visibility_follows_active_tab_hidden_only_and_zoom() {
        let mut layout = wide();
        assert!(layout.is_visible(1));
        assert!(layout.is_visible(2));
        assert!(!layout.is_visible(5), "неактивная вкладка не видна");
        assert!(layout.is_visible(4));

        layout.hidden.right = true;
        assert!(!layout.is_visible(4));
        layout.hidden.right = false;

        layout.only = Some(Zone::Center);
        assert!(
            !layout.is_visible(1),
            "колонка навигатора видна только при only левой зоны"
        );
        assert!(!layout.is_visible(4));
        assert!(layout.is_visible(3));
        layout.only = Some(Zone::Left);
        assert!(layout.is_visible(1));
        assert!(!layout.is_visible(2));
        layout.only = None;

        layout.zoomed = Some(5);
        assert!(layout.is_visible(5));
        assert!(!layout.is_visible(2));
        assert!(!layout.is_visible(1));
    }

    #[test]
    fn validate_rejects_last_active_out_of_range() {
        let mut layout = wide();
        layout.zones.center.last_active = 2;
        assert_eq!(
            layout.validate(&live(&[1, 2, 3, 4, 5])),
            Err(LayoutError::LastActiveOutOfRange {
                zone: Zone::Center,
                last_active: 2,
                parts: 2
            })
        );

        let mut layout = wide();
        layout.zones.bottom.last_active = 1;
        assert_eq!(
            layout.validate(&live(&[1, 2, 3, 4, 5])),
            Err(LayoutError::LastActiveOutOfRange {
                zone: Zone::Bottom,
                last_active: 1,
                parts: 0
            })
        );
    }

    #[test]
    fn validate_rejects_active_out_of_range_and_empty_part() {
        let mut layout = wide();
        layout.zones.center.parts[0].active = 2;
        assert_eq!(
            layout.validate(&live(&[1, 2, 3, 4, 5])),
            Err(LayoutError::ActiveOutOfRange {
                zone: Zone::Center,
                part: 0,
                active: 2,
                tabs: 2
            })
        );

        let mut layout = wide();
        layout.zones.right.parts[0].tabs.clear();
        assert_eq!(
            layout.validate(&live(&[1, 2, 3, 5])),
            Err(LayoutError::EmptyPart {
                zone: Zone::Right,
                part: 0
            })
        );
    }

    #[test]
    fn validate_rejects_bad_shares() {
        let mut layout = wide();
        layout.zones.center.parts[0].share = 400;
        assert_eq!(
            layout.validate(&live(&[1, 2, 3, 4, 5])),
            Err(LayoutError::ShareSum {
                zone: Zone::Center,
                sum: 900
            })
        );

        let mut layout = wide();
        layout.zones.center.parts[0].share = 0;
        layout.zones.center.parts[1].share = SHARE_TOTAL;
        assert_eq!(
            layout.validate(&live(&[1, 2, 3, 4, 5])),
            Err(LayoutError::ZeroShare {
                zone: Zone::Center,
                part: 0
            })
        );
    }

    #[test]
    fn validate_rejects_missing_extra_and_duplicate_panels() {
        let layout = wide();
        assert_eq!(
            layout.validate(&live(&[1, 2, 3, 4, 5, 6])),
            Err(LayoutError::MissingPanel(6))
        );
        assert_eq!(
            layout.validate(&live(&[1, 2, 3, 4])),
            Err(LayoutError::UnknownPanel(5))
        );

        let mut layout = wide();
        layout.zones.right.parts[0].tabs = vec![3];
        assert_eq!(
            layout.validate(&live(&[1, 2, 3, 5])),
            Err(LayoutError::DuplicatePanel(3))
        );

        let mut layout = wide();
        layout.navigator = Some(4);
        assert_eq!(
            layout.validate(&live(&[2, 3, 4, 5])),
            Err(LayoutError::DuplicatePanel(4))
        );
    }

    #[test]
    fn validate_rejects_unknown_zoomed_and_focus_entries() {
        let mut layout = wide();
        layout.zoomed = Some(9);
        assert_eq!(
            layout.validate(&live(&[1, 2, 3, 4, 5])),
            Err(LayoutError::UnknownZoomed(9))
        );

        let mut layout = wide();
        layout.focus = vec![9, 2];
        assert_eq!(
            layout.validate(&live(&[1, 2, 3, 4, 5])),
            Err(LayoutError::UnknownFocus(9))
        );

        let mut layout = wide();
        layout.focus = vec![2, 3, 2];
        assert_eq!(
            layout.validate(&live(&[1, 2, 3, 4, 5])),
            Err(LayoutError::DuplicateFocus(2))
        );
    }

    #[test]
    fn validate_rejects_invisible_focus() {
        let live = live(&[1, 2, 3, 4, 5]);

        let mut layout = wide();
        layout.focus = vec![2, 5];
        assert_eq!(layout.validate(&live), Err(LayoutError::FocusHidden(5)));

        let mut layout = wide();
        layout.focus = vec![2, 4];
        layout.hidden.right = true;
        assert_eq!(layout.validate(&live), Err(LayoutError::FocusHidden(4)));

        let mut layout = wide();
        layout.only = Some(Zone::Right);
        assert_eq!(layout.validate(&live), Err(LayoutError::FocusHidden(2)));

        let mut layout = wide();
        layout.zoomed = Some(3);
        assert_eq!(layout.validate(&live), Err(LayoutError::FocusHidden(2)));
        layout.focus = vec![2, 3];
        assert_eq!(layout.validate(&live), Ok(()));

        let mut layout = wide();
        layout.focus.clear();
        assert_eq!(layout.validate(&live), Err(LayoutError::NoFocus));
    }

    #[test]
    fn focus_may_point_nowhere_only_when_nothing_is_visible() {
        let mut layout = Layout::default_for(1u32, 2);
        layout.zones.center.parts.clear();
        layout.zones.right.parts = vec![Part::single(2, SHARE_TOTAL)];
        layout.hidden.navigator = true;
        layout.hidden.right = true;
        assert_eq!(layout.validate(&live(&[1, 2])), Ok(()));
        layout.focus.clear();
        assert_eq!(layout.validate(&live(&[1, 2])), Ok(()));

        let empty: Layout<u32> = Layout {
            navigator: None,
            zones: ZoneMap::default(),
            sizes: ZoneSizes::DEFAULT,
            hidden: Hidden::default(),
            only: None,
            zoomed: None,
            focus: Vec::new(),
        };
        assert_eq!(empty.validate(&BTreeSet::new()), Ok(()));
    }

    #[test]
    fn json_spelling_is_stable_and_strict() {
        let layout = Layout::default_for(1u32, 2);
        let json = serde_json::to_string(&layout).unwrap();
        assert_eq!(
            json,
            concat!(
                r#"{"navigator":1,"zones":{"left":{"parts":[],"last_active":0},"#,
                r#""right":{"parts":[],"last_active":0},"#,
                r#""center":{"parts":[{"tabs":[2],"active":0,"share":1000}],"last_active":0},"#,
                r#""bottom":{"parts":[],"last_active":0}},"#,
                r#""sizes":{"navigator":236,"plate":372,"right":372,"bottom":236},"#,
                r#""hidden":{"navigator":false,"plate":false,"right":false,"bottom":false},"#,
                r#""only":null,"zoomed":null,"focus":[2]}"#
            )
        );
        let back: Layout<u32> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, layout);

        let extra = json.replacen(r#""focus""#, r#""extra":1,"focus""#, 1);
        assert!(serde_json::from_str::<Layout<u32>>(&extra).is_err());
        assert_eq!(serde_json::to_string(&Zone::Bottom).unwrap(), r#""bottom""#);
    }
}
