//! Контраст по WCAG 2: относительная яркость кодированного sRGB и отношение яркостей.

use crate::color::{Srgb8, Tint, UnitRgb};

/// Относительная яркость цвета.
pub fn relative_luminance(color: impl Into<UnitRgb>) -> f64 {
    let color = color.into();
    let linear = |value: f64| {
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

/// Отношение контраста двух цветов, от 1 до 21.
pub fn contrast_ratio(a: impl Into<UnitRgb>, b: impl Into<UnitRgb>) -> f64 {
    let (a, b) = (relative_luminance(a), relative_luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Контраст текста на фоне под слоем панели: вуаль и тон лежат над содержимым панели, поэтому
/// смешиваются и с текстом, и с фоном.
pub fn layered_contrast(text: Srgb8, background: Srgb8, layer: Option<Tint>) -> f64 {
    match layer {
        Some(layer) => contrast_ratio(layer.over(text), layer.over(background)),
        None => contrast_ratio(text, background),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Appearance, Cell, Contrast};
    use crate::tokens::{AppTheme, KeyMode};

    const THRESHOLD: f64 = 4.5;
    const TOLERANCE: f64 = 0.01;

    #[test]
    fn ratio_extremes() {
        let black = Srgb8::hex(0x000000);
        let white = Srgb8::hex(0xFFFFFF);
        assert!((contrast_ratio(black, white) - 21.0).abs() < 1e-9);
        assert!((contrast_ratio(white, black) - 21.0).abs() < 1e-9);
        assert!((contrast_ratio(white, white) - 1.0).abs() < 1e-12);
        assert!((relative_luminance(Srgb8::hex(0x808080)) - 0.21586).abs() < 1e-4);
    }

    /// Нейтральный текст на `panel`, `panel_head` и выделенной строке в ячейках Increase
    /// Contrast: без слоя, под вуалью и под тоном каждого режима — не ниже 4,5:1 (ADR-0027).
    #[test]
    fn increased_contrast_neutral_text_passes_threshold() {
        for appearance in Appearance::ALL {
            let cell = Cell::new(appearance, Contrast::Increased);
            let theme = AppTheme::for_cell(cell);
            let c = theme.colors;
            let layers = [
                ("без слоя", None),
                ("вуаль", Some(theme.focus.veil)),
                ("тон навигации", Some(theme.tone(KeyMode::Navigation))),
                ("тон ввода", Some(theme.tone(KeyMode::Input))),
            ];
            for (text_name, text) in [("text", c.text), ("text_2", c.text_2), ("text_3", c.text_3)]
            {
                for (bg_name, bg) in [
                    ("panel", c.panel),
                    ("panel_head", c.panel_head),
                    ("selection", c.selection),
                ] {
                    for (layer_name, layer) in layers {
                        let ratio = layered_contrast(text, bg, layer);
                        assert!(
                            ratio >= THRESHOLD,
                            "{}: {text_name} на {bg_name}, {layer_name}: {ratio:.3}",
                            cell.name()
                        );
                    }
                }
            }
        }
    }

    /// Принятые исключения обычных ячеек сверяются с итоговыми числами ADR-0027, а не с порогом.
    /// Вторичный текст ADR — `text_3` (тот же цвет, что комментарии).
    #[test]
    fn normal_cells_match_adr_0027_exceptions() {
        let cases = [
            (Appearance::Dark, "panel", 3.65),
            (Appearance::Light, "panel", 3.96),
            (Appearance::Dark, "selection", 2.68),
        ];
        for (appearance, bg_name, expected) in cases {
            let cell = Cell::new(appearance, Contrast::Normal);
            let theme = AppTheme::for_cell(cell);
            let bg = match bg_name {
                "panel" => theme.colors.panel,
                _ => theme.colors.selection,
            };
            let ratio = layered_contrast(theme.colors.text_3, bg, Some(theme.focus.veil));
            assert!(
                (ratio - expected).abs() <= TOLERANCE,
                "{}: text_3 на {bg_name} под вуалью {ratio:.4}, ADR-0027 {expected}",
                cell.name()
            );
        }
    }

    /// Без смешения с текстом или с округлением до байта числа ADR не воспроизводятся: тест
    /// закрепляет, что исключения считаются тем же способом, что в ADR.
    #[test]
    fn exception_depends_on_layer_over_text() {
        let theme = AppTheme::for_cell(Cell::new(Appearance::Dark, Contrast::Normal));
        let veil = theme.focus.veil;
        let background_only = contrast_ratio(theme.colors.text_3, veil.over(theme.colors.panel));
        assert!((background_only - 3.65).abs() > TOLERANCE);
        let rounded = contrast_ratio(
            veil.over(theme.colors.text_3).to_srgb8(),
            veil.over(theme.colors.panel).to_srgb8(),
        );
        assert!((rounded - 3.65).abs() > TOLERANCE);
    }
}
