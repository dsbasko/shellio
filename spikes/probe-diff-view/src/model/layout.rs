//! Адаптация вида diff к зоне и размеру части зоны (ADR-0009, ADR-0031).
//!
//! Пороги классов — параметры пробы, а не решения: они передаются P1.4 и P3.2. Приоритет
//! правил:
//!
//! 1. ниже `min_size` — заглушка «Развернуть» без строк;
//! 2. левая и правая зоны всегда unified при любой ширине;
//! 3. центр и нижняя зона — «Рядом», если класс по ширине `Regular` или `Wide` и ширины хватает
//!    на две стороны по 60 клеток текста, иначе unified;
//! 4. `ShortBar` — в unified одна колонка номеров, режим по пункту 3 с классом по ширине.
//!
//! «Рядом» показывается, только если его запросили: [`DiffGeometry::mode`] снимает запрос, когда
//! правила его не допускают.

use super::rows::Mode;
use super::wrap::{TAB_WIDTH, Wrap};

/// Высота меньше этой — `ShortBar`.
pub const SHORT_BAR_HEIGHT: f64 = 240.0;
/// Ширина меньше этой — `Narrow`.
pub const NARROW_WIDTH: f64 = 480.0;
/// Ширина от этой — `Wide`.
pub const WIDE_WIDTH: f64 = 1100.0;
/// `min_size` пробы: ниже по любой оси — заглушка.
pub const MIN_WIDTH: f64 = 200.0;
pub const MIN_HEIGHT: f64 = 120.0;
/// Наименьшая ширина текста одной стороны «Рядом» в клетках.
pub const SIDE_MIN_COLUMNS: u32 = 60;
/// Клетки колонки номеров сверх цифр: пробел после номера.
const NUMBER_PAD: u32 = 1;
/// Клетки знака `+` или `-` и пробела после него.
const SIGN_CELLS: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Zone {
    Left,
    Right,
    Center,
    Bottom,
}

impl Zone {
    pub const ALL: [Zone; 4] = [Zone::Left, Zone::Right, Zone::Center, Zone::Bottom];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SizeClass {
    Narrow,
    Regular,
    Wide,
    ShortBar,
}

impl SizeClass {
    /// Класс части зоны: сначала высота (`ShortBar`), затем ширина.
    pub fn of(width: f64, height: f64) -> Self {
        if height < SHORT_BAR_HEIGHT {
            SizeClass::ShortBar
        } else {
            SizeClass::of_width(width)
        }
    }

    /// Класс только по ширине: `Narrow`, `Regular` или `Wide`.
    pub fn of_width(width: f64) -> Self {
        if width < NARROW_WIDTH {
            SizeClass::Narrow
        } else if width >= WIDE_WIDTH {
            SizeClass::Wide
        } else {
            SizeClass::Regular
        }
    }
}

/// Геометрия части зоны в точках, как в ADR-0009.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub zone: Zone,
    pub size: SizeClass,
    pub width: f64,
    pub height: f64,
}

impl Geometry {
    /// Геометрия с классом по порогам пробы. В продукте класс задаёт хост.
    pub fn new(zone: Zone, width: f64, height: f64) -> Self {
        Geometry {
            zone,
            size: SizeClass::of(width, height),
            width,
            height,
        }
    }

    pub fn below_min_size(&self) -> bool {
        !(self.width >= MIN_WIDTH && self.height >= MIN_HEIGHT)
    }
}

/// Колонки номеров в гуттере unified. В «Рядом» у каждой половины одна колонка своей стороны.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gutter {
    /// Старый и новый номер.
    Both,
    /// Один номер: новый, у удалённой строки старый.
    Single,
}

impl Gutter {
    fn numbers(self) -> u32 {
        match self {
            Gutter::Both => 2,
            Gutter::Single => 1,
        }
    }
}

/// Шрифт и разметка вида: ширина клетки моноширинного шрифта, число цифр самого большого
/// номера строки файла и ширина разделителя половин «Рядом» в точках.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextMetrics {
    pub char_width: f64,
    pub digits: u32,
    pub divider: f64,
}

impl TextMetrics {
    /// Цифр у номера `max_line`, не меньше одной.
    pub fn digits_of(max_line: u32) -> u32 {
        max_line.checked_ilog10().map_or(1, |log| log + 1)
    }

    fn gutter_cells(&self, numbers: u32) -> u32 {
        numbers
            .saturating_mul(self.digits.saturating_add(NUMBER_PAD))
            .saturating_add(SIGN_CELLS)
    }

    /// Адаптация вида diff к геометрии части зоны.
    pub fn adapt(&self, geometry: Geometry) -> DiffLayout {
        if geometry.below_min_size() {
            return DiffLayout::Placeholder;
        }
        let zone_allows_side = match geometry.zone {
            Zone::Left | Zone::Right => false,
            Zone::Center | Zone::Bottom => true,
        };
        let (width_class, gutter) = match geometry.size {
            SizeClass::ShortBar => (SizeClass::of_width(geometry.width), Gutter::Single),
            class @ (SizeClass::Narrow | SizeClass::Regular | SizeClass::Wide) => {
                (class, Gutter::Both)
            }
        };
        let class_allows_side = match width_class {
            SizeClass::Regular | SizeClass::Wide => true,
            SizeClass::Narrow | SizeClass::ShortBar => false,
        };
        let mut layout = DiffGeometry {
            size: geometry.size,
            side_by_side: false,
            gutter,
            width: geometry.width,
            metrics: *self,
        };
        layout.side_by_side = zone_allows_side
            && class_allows_side
            && layout.wrap(Mode::SideBySide).columns() >= SIDE_MIN_COLUMNS;
        DiffLayout::Diff(layout)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DiffLayout {
    /// Ниже `min_size`: заглушка «Развернуть», строки не рисуются.
    Placeholder,
    Diff(DiffGeometry),
}

/// Раскладка вида diff в части зоны.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiffGeometry {
    pub size: SizeClass,
    /// «Рядом» допустим.
    pub side_by_side: bool,
    pub gutter: Gutter,
    width: f64,
    metrics: TextMetrics,
}

impl DiffGeometry {
    /// Показанный режим для запрошенного: «Рядом» снимается, если он недопустим.
    pub fn mode(&self, requested: Mode) -> Mode {
        match requested {
            Mode::SideBySide if self.side_by_side => Mode::SideBySide,
            Mode::SideBySide | Mode::Unified => Mode::Unified,
        }
    }

    /// Ширина гуттера в клетках: в unified всей строки, в «Рядом» одной половины.
    pub fn gutter_cells(&self, mode: Mode) -> u32 {
        match mode {
            Mode::Unified => self.metrics.gutter_cells(self.gutter.numbers()),
            Mode::SideBySide => self.metrics.gutter_cells(1),
        }
    }

    /// Ширина области текста одной стороны в точках: в unified строки без гуттера, в «Рядом»
    /// половины без разделителя и своего гуттера.
    pub fn text_width(&self, mode: Mode) -> f64 {
        let gutter = f64::from(self.gutter_cells(mode)) * self.metrics.char_width;
        match mode {
            Mode::Unified => self.width - gutter,
            Mode::SideBySide => (self.width - self.metrics.divider) / 2.0 - gutter,
        }
    }

    /// Перенос области текста режима: то же число клеток, что у индекса высот.
    pub fn wrap(&self, mode: Mode) -> Wrap {
        Wrap::from_width(self.text_width(mode), self.metrics.char_width, TAB_WIDTH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Клетка 8 точек, номера до 5 цифр, разделитель 1 точка: гуттер половины 8 клеток, двух
    /// номеров — 14. «Рядом» нужно `2 × (8 + 60) × 8 + 1 = 1089` точек.
    const METRICS: TextMetrics = TextMetrics {
        char_width: 8.0,
        digits: 5,
        divider: 1.0,
    };
    const SIDE_WIDTH: f64 = 1089.0;
    const TALL: f64 = 800.0;

    fn adapt(zone: Zone, width: f64, height: f64) -> DiffLayout {
        METRICS.adapt(Geometry::new(zone, width, height))
    }

    fn diff(layout: DiffLayout) -> DiffGeometry {
        match layout {
            DiffLayout::Diff(geometry) => geometry,
            DiffLayout::Placeholder => panic!("заглушка вместо diff"),
        }
    }

    #[test]
    fn size_class_thresholds() {
        let cases = [
            (479.99, TALL, SizeClass::Narrow),
            (480.0, TALL, SizeClass::Regular),
            (1099.99, TALL, SizeClass::Regular),
            (1100.0, TALL, SizeClass::Wide),
            (1400.0, 239.99, SizeClass::ShortBar),
            (300.0, 239.99, SizeClass::ShortBar),
            (1400.0, 240.0, SizeClass::Wide),
            (440.0, TALL, SizeClass::Narrow),
        ];
        for (width, height, class) in cases {
            assert_eq!(SizeClass::of(width, height), class, "{width} × {height}");
        }
        assert_eq!(SizeClass::of_width(300.0), SizeClass::Narrow);
    }

    #[test]
    fn placeholder_below_min_size_in_every_zone() {
        for zone in Zone::ALL {
            assert_eq!(adapt(zone, 199.99, TALL), DiffLayout::Placeholder);
            assert_eq!(adapt(zone, 1400.0, 119.99), DiffLayout::Placeholder);
            assert_eq!(adapt(zone, f64::NAN, TALL), DiffLayout::Placeholder);
            let at_min = diff(adapt(zone, MIN_WIDTH, MIN_HEIGHT));
            assert_eq!(at_min.size, SizeClass::ShortBar);
            assert_eq!(at_min.mode(Mode::SideBySide), Mode::Unified);
        }
        // Боковая часть 236 точек — выше порога: unified, не заглушка.
        let side = diff(adapt(Zone::Left, 236.0, TALL));
        assert_eq!(side.size, SizeClass::Narrow);
        assert_eq!(side.mode(Mode::SideBySide), Mode::Unified);
        assert_eq!(side.gutter, Gutter::Both);
    }

    #[test]
    fn side_zones_are_always_unified() {
        for zone in [Zone::Left, Zone::Right] {
            for (width, height) in [
                (236.0, TALL),
                (800.0, TALL),
                (2400.0, TALL),
                (2400.0, 200.0),
            ] {
                let layout = diff(adapt(zone, width, height));
                assert!(!layout.side_by_side, "{zone:?} {width} × {height}");
                assert_eq!(layout.mode(Mode::SideBySide), Mode::Unified);
                assert_eq!(layout.mode(Mode::Unified), Mode::Unified);
            }
        }
    }

    #[test]
    fn center_and_bottom_follow_class_and_width() {
        for zone in [Zone::Center, Zone::Bottom] {
            // Половина центра 440 точек — `Narrow`.
            let half = diff(adapt(zone, 440.0, TALL));
            assert_eq!(half.size, SizeClass::Narrow);
            assert!(!half.side_by_side);
            // `Regular`, но двум сторонам по 60 клеток не хватает 0,01 точки.
            let short = diff(adapt(zone, SIDE_WIDTH - 0.01, TALL));
            assert_eq!(short.size, SizeClass::Regular);
            assert!(!short.side_by_side);
            assert_eq!(short.wrap(Mode::SideBySide).columns(), 59);
            // Ровно хватает: та же граница, что у переноса индекса высот.
            let exact = diff(adapt(zone, SIDE_WIDTH, TALL));
            assert_eq!(exact.size, SizeClass::Regular);
            assert!(exact.side_by_side);
            assert_eq!(exact.wrap(Mode::SideBySide).columns(), SIDE_MIN_COLUMNS);
            assert_eq!(exact.mode(Mode::SideBySide), Mode::SideBySide);
            assert_eq!(exact.mode(Mode::Unified), Mode::Unified);
            let wide = diff(adapt(zone, 1600.0, TALL));
            assert_eq!(wide.size, SizeClass::Wide);
            assert!(wide.side_by_side);
            assert_eq!(wide.gutter, Gutter::Both);
        }
    }

    #[test]
    fn narrow_class_blocks_side_even_when_cells_fit() {
        // Мелкий шрифт: 60 клеток с каждой стороны влезают в 470 точек, но класс `Narrow`.
        let tiny = TextMetrics {
            char_width: 3.0,
            digits: 3,
            divider: 1.0,
        };
        let layout = match tiny.adapt(Geometry::new(Zone::Center, 470.0, TALL)) {
            DiffLayout::Diff(layout) => layout,
            DiffLayout::Placeholder => panic!("заглушка"),
        };
        assert!(layout.wrap(Mode::SideBySide).columns() >= SIDE_MIN_COLUMNS);
        assert!(!layout.side_by_side);
    }

    #[test]
    fn short_bar_uses_width_for_mode_and_single_gutter() {
        // Короткая широкая часть центра: `ShortBar` с «Рядом».
        let bar = diff(adapt(Zone::Center, 1400.0, 200.0));
        assert_eq!(bar.size, SizeClass::ShortBar);
        assert!(bar.side_by_side);
        assert_eq!(bar.mode(Mode::SideBySide), Mode::SideBySide);
        assert_eq!(bar.gutter, Gutter::Single);
        assert_eq!(bar.gutter_cells(Mode::Unified), 8);
        assert_eq!(bar.gutter_cells(Mode::SideBySide), 8);
        let bottom = diff(adapt(Zone::Bottom, 1400.0, 160.0));
        assert!(bottom.side_by_side);
        // Короткая узкая часть: unified с одной колонкой.
        let narrow = diff(adapt(Zone::Bottom, 400.0, 200.0));
        assert_eq!(narrow.size, SizeClass::ShortBar);
        assert!(!narrow.side_by_side);
        assert_eq!(narrow.gutter, Gutter::Single);
        // Короткая `Regular` без ширины на две стороны.
        let regular = diff(adapt(Zone::Center, 900.0, 200.0));
        assert!(!regular.side_by_side);
        // Боковая короткая широкая — всё равно unified.
        assert!(!diff(adapt(Zone::Right, 1400.0, 200.0)).side_by_side);
    }

    #[test]
    fn text_width_and_wrap_per_mode() {
        let layout = diff(adapt(Zone::Center, 1600.0, TALL));
        assert_eq!(layout.gutter_cells(Mode::Unified), 14);
        assert_eq!(layout.gutter_cells(Mode::SideBySide), 8);
        assert_eq!(layout.text_width(Mode::Unified), 1600.0 - 14.0 * 8.0);
        assert_eq!(
            layout.text_width(Mode::SideBySide),
            (1600.0 - 1.0) / 2.0 - 64.0
        );
        assert_eq!(layout.wrap(Mode::Unified).columns(), 186);
        assert_eq!(layout.wrap(Mode::SideBySide).columns(), 91);
        assert_eq!(TextMetrics::digits_of(0), 1);
        assert_eq!(TextMetrics::digits_of(9), 1);
        assert_eq!(TextMetrics::digits_of(10), 2);
        assert_eq!(TextMetrics::digits_of(100_000), 6);
    }
}
