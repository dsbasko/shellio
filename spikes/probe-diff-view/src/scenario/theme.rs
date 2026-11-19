//! Оракул сценария `theme` (задача 15): какой токен должен лежать под точкой пиксельной пробы и
//! где точка не задевает глиф (отчёт, строка «Пиксельная проба `theme`»).

use probe_diff_view::model::LineKind;
use probe_diff_view::tokens::{Rgb, Theme, ThemeKey};

/// Допуск канала sRGB, как в P0.2.
pub const TOLERANCE: u8 = 1;
/// Отступ точки текста от правого края области текста половины, точки.
pub const TEXT_INSET: f64 = 3.0;
/// Свободных клеток после текста до точки.
pub const FREE_CELLS: f64 = 2.0;

/// Ячейки темы по порядку строки «Ячейки `theme`».
pub const CELLS: [ThemeKey; 4] = [
    ThemeKey::ALL[0],
    ThemeKey::ALL[2],
    ThemeKey::ALL[1],
    ThemeKey::ALL[3],
];

/// Роль строки показа в пробе: обычная или строка R с курсором и выделением.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Plain,
    Marked,
}

/// Ожидаемые токены текста и гуттера половины строки вида `kind` с ролью `role`.
pub fn wants(theme: &Theme, kind: LineKind, role: Role) -> [(&'static str, Rgb); 2] {
    match (role, kind) {
        (Role::Marked, _) => [
            ("selection", theme.selection),
            ("selection", theme.selection),
        ],
        (Role::Plain, LineKind::Context) => [
            ("background", theme.background),
            ("gutter_background", theme.gutter_background),
        ],
        (Role::Plain, LineKind::Added) => [
            ("added_background", theme.added_background),
            ("added_gutter", theme.added_gutter),
        ],
        (Role::Plain, LineKind::Removed) => [
            ("removed_background", theme.removed_background),
            ("removed_gutter", theme.removed_gutter),
        ],
    }
}

/// Половина строки показа в координатах документа: начало и ширина по x, верх и высота текста.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    pub x: f64,
    pub width: f64,
    pub top: f64,
    pub height: f64,
}

/// Точка фона текста: у правого края области текста, если строка в одну визуальную строку и её
/// текст (`cells` клеток, верхняя оценка) кончается не позже чем за 2 клетки до точки.
pub fn text_point(
    band: Band,
    gutter_cells: u32,
    char_width: f64,
    cells: usize,
    visual_lines: u32,
) -> Option<(f64, f64)> {
    let x = band.x + band.width - TEXT_INSET;
    let text_end = band.x + (f64::from(gutter_cells) + cells as f64 + FREE_CELLS) * char_width;
    (visual_lines == 1 && x > text_end).then_some((x, band.top + band.height / 2.0))
}

/// Точка фона гуттера: середина первой клетки, если первый знак текста гуттера — пробел.
pub fn gutter_point(band: Band, char_width: f64, gutter: &str) -> Option<(f64, f64)> {
    gutter
        .starts_with(' ')
        .then_some((band.x + char_width / 2.0, band.top + band.height / 2.0))
}

/// Верхняя оценка клеток текста строки: байты и по 3 клетки сверх байта у табуляции.
pub fn cells_upper(text: &[u8]) -> usize {
    text.len() + 3 * text.iter().filter(|&&byte| byte == b'\t').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAND: Band = Band {
        x: 100.0,
        width: 500.0,
        top: 40.0,
        height: 14.0,
    };

    #[test]
    fn cells_cover_every_theme_once() {
        let mut names: Vec<&str> = CELLS.iter().map(|key| key.name()).collect();
        assert_eq!(names, ["light", "dark", "light-contrast", "dark-contrast"]);
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ThemeKey::ALL.len());
    }

    #[test]
    fn marked_row_wants_selection_everywhere() {
        let theme = Theme::of(ThemeKey::ALL[0]);
        for kind in [LineKind::Context, LineKind::Added, LineKind::Removed] {
            assert_eq!(
                wants(theme, kind, Role::Marked).map(|(_, rgb)| rgb),
                [theme.selection; 2]
            );
        }
        assert_eq!(
            wants(theme, LineKind::Added, Role::Plain).map(|(_, rgb)| rgb),
            [theme.added_background, theme.added_gutter]
        );
        assert_eq!(
            wants(theme, LineKind::Removed, Role::Plain).map(|(_, rgb)| rgb),
            [theme.removed_background, theme.removed_gutter]
        );
        assert_eq!(
            wants(theme, LineKind::Context, Role::Plain).map(|(_, rgb)| rgb),
            [theme.background, theme.gutter_background]
        );
    }

    #[test]
    fn text_point_avoids_glyphs_and_wrapped_rows() {
        // Гуттер 9 клеток по 7 точек: текст начинается с 163, точка на 597.
        assert_eq!(text_point(BAND, 9, 7.0, 10, 1), Some((597.0, 47.0)));
        // 60 клеток текста и 2 свободные доходят до 597: точка задела бы глиф.
        assert_eq!(text_point(BAND, 9, 7.0, 60, 1), None);
        assert_eq!(text_point(BAND, 9, 7.0, 59, 1), Some((597.0, 47.0)));
        assert_eq!(text_point(BAND, 9, 7.0, 10, 2), None);
    }

    #[test]
    fn gutter_point_needs_a_blank_first_cell() {
        assert_eq!(gutter_point(BAND, 7.0, "    12 "), Some((103.5, 47.0)));
        assert_eq!(gutter_point(BAND, 7.0, "123456 "), None);
        assert_eq!(cells_upper(b"\tab"), 6);
    }
}
