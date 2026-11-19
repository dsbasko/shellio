//! Токены темы пробы: цвета фона, текста, добавления и удаления, гуттера, выделения и классов
//! подсветки для светлой и тёмной темы, каждая также в варианте Increase Contrast. Модуль не
//! зависит от AppKit, вид переводит цвета в `CGColor` сам.
//!
//! Все поверхности непрозрачны (ADR-0023): у цвета нет альфа-канала, компоненты для Core
//! Graphics всегда с альфой 1. Контраст текста к каждому фону, на котором он рисуется, не ниже
//! 4,5:1 (WCAG AA), в Increase Contrast — не ниже 7:1 (WCAG AAA).

use crate::highlight::TokenClass;

/// Непрозрачный цвет sRGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn hex(value: u32) -> Rgb {
        Rgb {
            r: (value >> 16) as u8,
            g: (value >> 8) as u8,
            b: value as u8,
        }
    }

    /// Компоненты для `CGColor` в sRGB: красный, зелёный, синий и альфа 1.
    pub fn components(self) -> [f64; 4] {
        [
            f64::from(self.r) / 255.0,
            f64::from(self.g) / 255.0,
            f64::from(self.b) / 255.0,
            1.0,
        ]
    }

    /// Относительная яркость WCAG 2.
    pub fn luminance(self) -> f64 {
        let channel = |value: u8| {
            let c = f64::from(value) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }

    /// Контраст WCAG 2 двух цветов, от 1 до 21.
    pub fn contrast(self, other: Rgb) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        let (light, dark) = if a >= b { (a, b) } else { (b, a) };
        (light + 0.05) / (dark + 0.05)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Appearance {
    Light,
    Dark,
}

/// Тема вида: оформление системы и Increase Contrast.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ThemeKey {
    pub appearance: Appearance,
    pub increase_contrast: bool,
}

impl ThemeKey {
    pub const ALL: [ThemeKey; 4] = [
        ThemeKey {
            appearance: Appearance::Light,
            increase_contrast: false,
        },
        ThemeKey {
            appearance: Appearance::Light,
            increase_contrast: true,
        },
        ThemeKey {
            appearance: Appearance::Dark,
            increase_contrast: false,
        },
        ThemeKey {
            appearance: Appearance::Dark,
            increase_contrast: true,
        },
    ];

    pub fn name(self) -> &'static str {
        match (self.appearance, self.increase_contrast) {
            (Appearance::Light, false) => "light",
            (Appearance::Light, true) => "light-contrast",
            (Appearance::Dark, false) => "dark",
            (Appearance::Dark, true) => "dark-contrast",
        }
    }

    /// Наименьший контраст текста к фону в этой теме.
    pub fn min_contrast(self) -> f64 {
        if self.increase_contrast { 7.0 } else { 4.5 }
    }
}

/// Цвета классов подсветки. Класс без своего цвета рисуется цветом текста.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Syntax {
    pub keyword: Rgb,
    pub string: Rgb,
    pub comment: Rgb,
    pub function: Rgb,
    pub macro_: Rgb,
    pub type_: Rgb,
    pub constant: Rgb,
    pub attribute: Rgb,
    pub property: Rgb,
    pub punctuation: Rgb,
}

/// Токены вида diff одной темы.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub key: ThemeKey,
    pub background: Rgb,
    pub text: Rgb,
    pub added_background: Rgb,
    pub removed_background: Rgb,
    pub gutter_background: Rgb,
    pub gutter_text: Rgb,
    pub added_gutter: Rgb,
    pub removed_gutter: Rgb,
    /// Фон выделенных строк и строки курсора поверх добавления и удаления.
    pub selection: Rgb,
    pub cursor_line: Rgb,
    /// Граница ханка и разделитель половин «Рядом».
    pub separator: Rgb,
    pub syntax: Syntax,
}

const LIGHT: Theme = Theme {
    key: ThemeKey::ALL[0],
    background: Rgb::hex(0xFFFFFF),
    text: Rgb::hex(0x1F2328),
    added_background: Rgb::hex(0xE6FFEC),
    removed_background: Rgb::hex(0xFFEBE9),
    gutter_background: Rgb::hex(0xF6F8FA),
    gutter_text: Rgb::hex(0x57606A),
    added_gutter: Rgb::hex(0xCCFFD8),
    removed_gutter: Rgb::hex(0xFFD7D5),
    selection: Rgb::hex(0xDDF4FF),
    cursor_line: Rgb::hex(0xEEF1F4),
    separator: Rgb::hex(0xD0D7DE),
    syntax: Syntax {
        keyword: Rgb::hex(0xB3261E),
        string: Rgb::hex(0x0A3069),
        comment: Rgb::hex(0x57606A),
        function: Rgb::hex(0x6639BA),
        macro_: Rgb::hex(0x6639BA),
        type_: Rgb::hex(0x8A3300),
        constant: Rgb::hex(0x0550AE),
        attribute: Rgb::hex(0x116329),
        property: Rgb::hex(0x0550AE),
        punctuation: Rgb::hex(0x1F2328),
    },
};

const LIGHT_CONTRAST: Theme = Theme {
    key: ThemeKey::ALL[1],
    background: Rgb::hex(0xFFFFFF),
    text: Rgb::hex(0x000000),
    added_background: Rgb::hex(0xDDFBE4),
    removed_background: Rgb::hex(0xFFE3E0),
    gutter_background: Rgb::hex(0xF0F2F4),
    gutter_text: Rgb::hex(0x30363D),
    added_gutter: Rgb::hex(0xBDF5CB),
    removed_gutter: Rgb::hex(0xFFCBC7),
    selection: Rgb::hex(0xCCEBFF),
    cursor_line: Rgb::hex(0xE4E8EC),
    separator: Rgb::hex(0x6E7781),
    syntax: Syntax {
        keyword: Rgb::hex(0x7F0D16),
        string: Rgb::hex(0x0A2550),
        comment: Rgb::hex(0x30363D),
        function: Rgb::hex(0x4A1F99),
        macro_: Rgb::hex(0x4A1F99),
        type_: Rgb::hex(0x5C2200),
        constant: Rgb::hex(0x033777),
        attribute: Rgb::hex(0x0B4A1E),
        property: Rgb::hex(0x033777),
        punctuation: Rgb::hex(0x000000),
    },
};

const DARK: Theme = Theme {
    key: ThemeKey::ALL[2],
    background: Rgb::hex(0x0D1117),
    text: Rgb::hex(0xE6EDF3),
    added_background: Rgb::hex(0x12261E),
    removed_background: Rgb::hex(0x2A1417),
    gutter_background: Rgb::hex(0x161B22),
    gutter_text: Rgb::hex(0x9DA7B1),
    added_gutter: Rgb::hex(0x1B3A2A),
    removed_gutter: Rgb::hex(0x3D1C20),
    selection: Rgb::hex(0x1C2D44),
    cursor_line: Rgb::hex(0x1A1F27),
    separator: Rgb::hex(0x30363D),
    syntax: Syntax {
        keyword: Rgb::hex(0xFF7B72),
        string: Rgb::hex(0xA5D6FF),
        comment: Rgb::hex(0x8B949E),
        function: Rgb::hex(0xD2A8FF),
        macro_: Rgb::hex(0xD2A8FF),
        type_: Rgb::hex(0xFFA657),
        constant: Rgb::hex(0x79C0FF),
        attribute: Rgb::hex(0x7EE787),
        property: Rgb::hex(0x79C0FF),
        punctuation: Rgb::hex(0xE6EDF3),
    },
};

const DARK_CONTRAST: Theme = Theme {
    key: ThemeKey::ALL[3],
    background: Rgb::hex(0x000000),
    text: Rgb::hex(0xFFFFFF),
    added_background: Rgb::hex(0x0B2016),
    removed_background: Rgb::hex(0x260C10),
    gutter_background: Rgb::hex(0x0A0C10),
    gutter_text: Rgb::hex(0xC9D1D9),
    added_gutter: Rgb::hex(0x123222),
    removed_gutter: Rgb::hex(0x3A141A),
    selection: Rgb::hex(0x14263D),
    cursor_line: Rgb::hex(0x151A21),
    separator: Rgb::hex(0x8B949E),
    syntax: Syntax {
        keyword: Rgb::hex(0xFFA198),
        string: Rgb::hex(0xCAE8FF),
        comment: Rgb::hex(0xC9D1D9),
        function: Rgb::hex(0xE2C5FF),
        macro_: Rgb::hex(0xE2C5FF),
        type_: Rgb::hex(0xFFC680),
        constant: Rgb::hex(0xA5D6FF),
        attribute: Rgb::hex(0xAFF5B4),
        property: Rgb::hex(0xA5D6FF),
        punctuation: Rgb::hex(0xFFFFFF),
    },
};

impl Theme {
    pub fn of(key: ThemeKey) -> &'static Theme {
        match (key.appearance, key.increase_contrast) {
            (Appearance::Light, false) => &LIGHT,
            (Appearance::Light, true) => &LIGHT_CONTRAST,
            (Appearance::Dark, false) => &DARK,
            (Appearance::Dark, true) => &DARK_CONTRAST,
        }
    }

    /// Цвет класса подсветки. Разбор исчерпывающий: новый класс получит цвет явно.
    pub fn class(&self, class: TokenClass) -> Rgb {
        let syntax = &self.syntax;
        match class {
            TokenClass::Keyword => syntax.keyword,
            TokenClass::String | TokenClass::Escape => syntax.string,
            TokenClass::Comment | TokenClass::CommentDoc => syntax.comment,
            TokenClass::Function | TokenClass::FunctionMethod => syntax.function,
            TokenClass::FunctionMacro => syntax.macro_,
            TokenClass::Type | TokenClass::TypeBuiltin | TokenClass::Constructor => syntax.type_,
            TokenClass::Constant | TokenClass::ConstantBuiltin => syntax.constant,
            TokenClass::Attribute | TokenClass::Label => syntax.attribute,
            TokenClass::Property | TokenClass::VariableParameter | TokenClass::VariableBuiltin => {
                syntax.property
            }
            TokenClass::Operator
            | TokenClass::PunctuationBracket
            | TokenClass::PunctuationDelimiter => syntax.punctuation,
        }
    }

    /// Все цвета текста строк diff: обычный текст и каждый класс подсветки.
    pub fn text_colors(&self) -> Vec<(&'static str, Rgb)> {
        let mut colors = vec![("text", self.text)];
        colors.extend(
            TokenClass::ALL
                .into_iter()
                .map(|class| (class.name(), self.class(class))),
        );
        colors
    }

    /// Фоны, на которых рисуется текст строк diff.
    pub fn text_backgrounds(&self) -> [(&'static str, Rgb); 5] {
        [
            ("background", self.background),
            ("added_background", self.added_background),
            ("removed_background", self.removed_background),
            ("selection", self.selection),
            ("cursor_line", self.cursor_line),
        ]
    }

    /// Фоны гуттера, на которых рисуются номера строк.
    pub fn gutter_backgrounds(&self) -> [(&'static str, Rgb); 3] {
        [
            ("gutter_background", self.gutter_background),
            ("added_gutter", self.added_gutter),
            ("removed_gutter", self.removed_gutter),
        ]
    }

    /// Пары «текст — фон» с контрастом ниже порога темы.
    pub fn contrast_failures(&self) -> Vec<String> {
        let min = self.key.min_contrast();
        let mut failures = Vec::new();
        for (text, color) in self.text_colors() {
            for (background, fill) in self.text_backgrounds() {
                let ratio = color.contrast(fill);
                if ratio < min {
                    failures.push(format!("{text} на {background}: {ratio:.2}"));
                }
            }
        }
        for (background, fill) in self.gutter_backgrounds() {
            let ratio = self.gutter_text.contrast(fill);
            if ratio < min {
                failures.push(format!("gutter_text на {background}: {ratio:.2}"));
            }
        }
        failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_meets_its_contrast() {
        for key in ThemeKey::ALL {
            let theme = Theme::of(key);
            assert_eq!(theme.key, key);
            let failures = theme.contrast_failures();
            assert!(
                failures.is_empty(),
                "{}: {}",
                key.name(),
                failures.join("; ")
            );
        }
    }

    #[test]
    fn components_are_opaque_srgb() {
        for key in ThemeKey::ALL {
            let theme = Theme::of(key);
            let mut all = theme.text_colors();
            all.extend(theme.text_backgrounds());
            all.extend(theme.gutter_backgrounds());
            all.push(("gutter_text", theme.gutter_text));
            all.push(("separator", theme.separator));
            for (name, color) in all {
                let [r, g, b, a] = color.components();
                assert_eq!(a, 1.0, "{}: {name}", key.name());
                for c in [r, g, b] {
                    assert!((0.0..=1.0).contains(&c), "{}: {name}", key.name());
                }
            }
        }
        assert_eq!(
            Rgb::hex(0x0A3069),
            Rgb {
                r: 10,
                g: 48,
                b: 105
            }
        );
    }

    #[test]
    fn surfaces_are_distinct_within_a_theme() {
        for key in ThemeKey::ALL {
            let theme = Theme::of(key);
            let surfaces = [
                theme.background,
                theme.added_background,
                theme.removed_background,
                theme.selection,
                theme.cursor_line,
            ];
            for (i, a) in surfaces.iter().enumerate() {
                for b in &surfaces[i + 1..] {
                    assert_ne!(a, b, "{}", key.name());
                }
            }
        }
    }

    #[test]
    fn contrast_matches_wcag_reference_values() {
        let black = Rgb::hex(0x000000);
        let white = Rgb::hex(0xFFFFFF);
        assert!((black.contrast(white) - 21.0).abs() < 1e-9);
        assert!((white.contrast(white) - 1.0).abs() < 1e-9);
        // #767676 на белом — известная граница AA, 4,54:1.
        assert!((Rgb::hex(0x767676).contrast(white) - 4.54).abs() < 0.01);
    }

    #[test]
    fn theme_names_are_unique() {
        let mut names: Vec<&str> = ThemeKey::ALL.iter().map(|key| key.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ThemeKey::ALL.len());
    }
}
