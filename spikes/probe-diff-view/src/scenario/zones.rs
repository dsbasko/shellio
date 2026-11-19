//! Оракул сценария `zones` (задача 15): ячейки геометрии — зона и часть зоны в точках — и
//! ожидание каждой, выведенное прямо из порогов плана («Зоны и геометрия»), а не из `adapt`.
//! Таблица одна для юнит-тестов и GUI-прохода (отчёт, строка «Ячейки `zones`»).

use probe_diff_view::model::Mode;
use probe_diff_view::model::layout::{DiffLayout, Gutter, SizeClass, Zone};

use super::pairs;

/// Ячейка геометрии: часть зоны `zone` размером `width × height`, `keys` — проверка `j` и `k`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Case {
    pub zone: Zone,
    pub width: f64,
    pub height: f64,
    pub keys: bool,
}

const fn case(zone: Zone, width: f64, height: f64, keys: bool) -> Case {
    Case {
        zone,
        width,
        height,
        keys,
    }
}

/// Ячейки GUI-прохода по порядку строки отчёта «Ячейки `zones`». Первая — исходная: в ней
/// ставится якорь.
pub const CASES: [Case; 23] = [
    case(Zone::Center, 1180.0, 700.0, true),
    case(Zone::Center, 1000.0, 700.0, true),
    case(Zone::Center, 990.0, 700.0, false),
    case(Zone::Center, 480.0, 700.0, false),
    case(Zone::Center, 479.0, 700.0, true),
    case(Zone::Center, 440.0, 700.0, false),
    case(Zone::Center, 1180.0, 240.0, false),
    case(Zone::Center, 1180.0, 239.0, false),
    case(Zone::Center, 1180.0, 200.0, true),
    case(Zone::Bottom, 1180.0, 400.0, false),
    case(Zone::Bottom, 1180.0, 160.0, false),
    case(Zone::Bottom, 800.0, 400.0, false),
    case(Zone::Bottom, 400.0, 200.0, true),
    case(Zone::Bottom, 800.0, 120.0, false),
    case(Zone::Bottom, 800.0, 119.0, true),
    case(Zone::Left, 1180.0, 700.0, false),
    case(Zone::Left, 236.0, 700.0, true),
    case(Zone::Left, 200.0, 700.0, false),
    case(Zone::Left, 199.0, 700.0, true),
    case(Zone::Right, 600.0, 700.0, false),
    case(Zone::Right, 1180.0, 200.0, false),
    case(Zone::Center, 200.0, 120.0, false),
    case(Zone::Center, 1180.0, 700.0, false),
];

/// Шрифт и разметка для ожидания: клетка, цифры самого большого номера, разделитель.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Text {
    pub char_width: f64,
    pub digits: u32,
    pub divider: f64,
}

/// Ожидание ячейки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Placeholder,
    Diff {
        class: SizeClass,
        /// «Рядом» допустим.
        side: bool,
        /// Колонок номеров в гуттере unified.
        columns: u32,
    },
}

/// Ожидание по порогам плана: `min_size` 200 × 120, `ShortBar` ниже 240, `Narrow` уже 480,
/// `Wide` от 1100; «Рядом» — центр и низ, класс по ширине `Regular` или `Wide` и ширина не
/// меньше `2 × (гуттер половины + 60 клеток) + разделитель`; у `ShortBar` одна колонка.
pub fn expect(case: &Case, text: Text) -> Expect {
    let (width, height) = (case.width, case.height);
    if !(width >= 200.0 && height >= 120.0) {
        return Expect::Placeholder;
    }
    let by_width = if width < 480.0 {
        SizeClass::Narrow
    } else if width >= 1100.0 {
        SizeClass::Wide
    } else {
        SizeClass::Regular
    };
    let class = if height < 240.0 {
        SizeClass::ShortBar
    } else {
        by_width
    };
    let half_gutter = f64::from(text.digits + 3);
    let needed = 2.0 * (half_gutter + 60.0) * text.char_width + text.divider;
    let zone = matches!(case.zone, Zone::Center | Zone::Bottom);
    let wide_enough = matches!(by_width, SizeClass::Regular | SizeClass::Wide);
    Expect::Diff {
        class,
        side: zone && wide_enough && width + 1e-6 * text.char_width * 2.0 >= needed,
        columns: if class == SizeClass::ShortBar { 1 } else { 2 },
    }
}

/// Показанный режим: «Рядом», только если запрошен и допустим.
pub fn shown_mode(expect: Expect, requested: Mode) -> Option<Mode> {
    match expect {
        Expect::Placeholder => None,
        Expect::Diff { side, .. } => Some(match requested {
            Mode::SideBySide if side => Mode::SideBySide,
            Mode::SideBySide | Mode::Unified => Mode::Unified,
        }),
    }
}

/// Длина текста гуттера в клетках: unified — колонки номеров по `digits + 1` и знак, половина
/// «Рядом» — один номер и знак.
pub fn gutter_cells(mode: Mode, columns: u32, digits: u32) -> u32 {
    match mode {
        Mode::Unified => columns * (digits + 1) + 2,
        Mode::SideBySide => digits + 3,
    }
}

/// `adapt` против ожидания ячейки.
pub fn layout_matches(expect: Expect, layout: DiffLayout) -> Result<(), String> {
    let got = match layout {
        DiffLayout::Placeholder => Expect::Placeholder,
        DiffLayout::Diff(geometry) => Expect::Diff {
            class: geometry.size,
            side: geometry.side_by_side,
            columns: match geometry.gutter {
                Gutter::Both => 2,
                Gutter::Single => 1,
            },
        },
    };
    if got == expect {
        Ok(())
    } else {
        Err(format!("adapt дал {got:?}, ожидалось {expect:?}"))
    }
}

/// Нарисованный текст гуттера одной половины против режима и колонок: длина в клетках и номера
/// по формату `paint::gutter_text`.
pub fn gutter_matches(text: &str, mode: Mode, columns: u32, digits: u32) -> Result<(), String> {
    let cells = text.chars().count() as u32;
    let want = gutter_cells(mode, columns, digits);
    if cells != want {
        return Err(format!(
            "гуттер {text:?}: {cells} клеток, ожидалось {want} ({mode:?}, колонок {columns})"
        ));
    }
    let numbers = text
        .split_whitespace()
        .filter(|word| word.parse::<u32>().is_ok())
        .count() as u32;
    let most = match mode {
        Mode::Unified => columns,
        Mode::SideBySide => 1,
    };
    if numbers == 0 || numbers > most {
        return Err(format!(
            "гуттер {text:?}: номеров {numbers}, допустимо от 1 до {most}"
        ));
    }
    if mode == Mode::Unified && columns == 2 && pairs::parse_gutter(text, None).is_none() {
        return Err(format!("гуттер {text:?} не по формату двух колонок"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use probe_diff_view::model::layout::{Geometry, TextMetrics};

    use super::*;

    /// Menlo 12 точек: клетка — ширина десяти `0` Core Text, делённая на 10.
    const MENLO: Text = Text {
        char_width: 7.224609375,
        digits: 6,
        divider: 1.0,
    };
    const COARSE: Text = Text {
        char_width: 8.0,
        digits: 5,
        divider: 1.0,
    };

    fn adapt(case: &Case, text: Text) -> DiffLayout {
        TextMetrics {
            char_width: text.char_width,
            digits: text.digits,
            divider: text.divider,
        }
        .adapt(Geometry::new(case.zone, case.width, case.height))
    }

    #[test]
    fn every_zone_case_matches_adapt() {
        for text in [MENLO, COARSE] {
            for case in &CASES {
                let want = expect(case, text);
                layout_matches(want, adapt(case, text))
                    .unwrap_or_else(|reason| panic!("{case:?}: {reason}"));
            }
        }
    }

    #[test]
    fn table_covers_every_zone_class_and_the_placeholder() {
        for zone in Zone::ALL {
            assert!(CASES.iter().any(|case| case.zone == zone), "{zone:?}");
        }
        let expects: Vec<Expect> = CASES.iter().map(|case| expect(case, MENLO)).collect();
        for class in [
            SizeClass::Narrow,
            SizeClass::Regular,
            SizeClass::Wide,
            SizeClass::ShortBar,
        ] {
            assert!(
                expects
                    .iter()
                    .any(|e| matches!(e, Expect::Diff { class: c, .. } if *c == class)),
                "{class:?}"
            );
            // Клавиши проверяются в каждом классе.
            assert!(
                CASES.iter().zip(&expects).any(|(case, e)| case.keys
                    && matches!(e, Expect::Diff { class: c, .. } if *c == class)),
                "клавиши {class:?}"
            );
        }
        let placeholders: Vec<usize> = expects
            .iter()
            .enumerate()
            .filter(|(_, e)| **e == Expect::Placeholder)
            .map(|(index, _)| index + 1)
            .collect();
        assert_eq!(placeholders, vec![15, 19]);
        assert!(CASES[14].keys && CASES[18].keys);
        // Последняя ячейка возвращает исходную геометрию.
        assert_eq!(
            Case {
                keys: false,
                ..CASES[0]
            },
            CASES[22]
        );
    }

    #[test]
    fn plan_cases_from_the_report() {
        let get = |index: usize| expect(&CASES[index - 1], MENLO);
        let diff = |class, side, columns| Expect::Diff {
            class,
            side,
            columns,
        };
        assert_eq!(get(1), diff(SizeClass::Wide, true, 2));
        assert_eq!(get(2), diff(SizeClass::Regular, true, 2));
        assert_eq!(get(3), diff(SizeClass::Regular, false, 2));
        assert_eq!(get(6), diff(SizeClass::Narrow, false, 2));
        assert_eq!(get(8), diff(SizeClass::ShortBar, true, 1));
        assert_eq!(get(11), diff(SizeClass::ShortBar, true, 1));
        assert_eq!(get(13), diff(SizeClass::ShortBar, false, 1));
        // Широкая боковая часть — unified.
        assert_eq!(get(16), diff(SizeClass::Wide, false, 2));
        // Боковая часть 236 точек — не заглушка.
        assert_eq!(get(17), diff(SizeClass::Narrow, false, 2));
        assert_eq!(get(21), diff(SizeClass::ShortBar, false, 1));
        assert_eq!(get(22), diff(SizeClass::ShortBar, false, 1));
    }

    #[test]
    fn fractional_boundaries_match_adapt() {
        let side_width = 2.0 * (9.0 + 60.0) * MENLO.char_width + 1.0;
        let cases = [
            case(Zone::Left, 199.99, 700.0, false),
            case(Zone::Bottom, 800.0, 119.99, false),
            case(Zone::Center, f64::NAN, 700.0, false),
            case(Zone::Center, 479.99, 700.0, false),
            case(Zone::Center, 1099.99, 700.0, false),
            case(Zone::Center, 1180.0, 239.99, false),
            case(Zone::Center, side_width - 0.01, 700.0, false),
            case(Zone::Center, side_width, 700.0, false),
            case(Zone::Bottom, side_width, 239.99, false),
            case(Zone::Right, side_width, 700.0, false),
        ];
        for case in &cases {
            layout_matches(expect(case, MENLO), adapt(case, MENLO))
                .unwrap_or_else(|reason| panic!("{case:?}: {reason}"));
        }
        assert!(matches!(
            expect(&cases[6], MENLO),
            Expect::Diff { side: false, .. }
        ));
        assert!(matches!(
            expect(&cases[7], MENLO),
            Expect::Diff { side: true, .. }
        ));
    }

    #[test]
    fn shown_mode_needs_the_request() {
        let side = Expect::Diff {
            class: SizeClass::Wide,
            side: true,
            columns: 2,
        };
        assert_eq!(shown_mode(side, Mode::SideBySide), Some(Mode::SideBySide));
        assert_eq!(shown_mode(side, Mode::Unified), Some(Mode::Unified));
        assert_eq!(shown_mode(Expect::Placeholder, Mode::Unified), None);
    }

    #[test]
    fn gutter_text_by_mode_and_columns() {
        assert_eq!(gutter_cells(Mode::Unified, 2, 6), 16);
        assert_eq!(gutter_cells(Mode::Unified, 1, 6), 9);
        assert_eq!(gutter_cells(Mode::SideBySide, 2, 6), 9);
        assert!(gutter_matches("    12     13   ", Mode::Unified, 2, 6).is_ok());
        assert!(gutter_matches("     9        - ", Mode::Unified, 2, 6).is_ok());
        assert!(gutter_matches("    13   ", Mode::Unified, 1, 6).is_ok());
        assert!(gutter_matches("    13 + ", Mode::SideBySide, 2, 6).is_ok());
        // Две колонки там, где ожидалась одна, и наоборот.
        assert!(gutter_matches("    12     13   ", Mode::Unified, 1, 6).is_err());
        assert!(gutter_matches("    13   ", Mode::Unified, 2, 6).is_err());
        assert!(gutter_matches("         ", Mode::SideBySide, 2, 6).is_err());
    }
}
