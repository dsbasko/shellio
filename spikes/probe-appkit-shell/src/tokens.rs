//! Токены темы, нужные сцене, для четырёх ячеек и обоих режимов. Значения перенесены из
//! `prototype/css/tokens.css` и таблицы ADR-0027; тесты сверяют их с файлом прототипа с учётом
//! каскада ячеек и разрешения `var()`. Модуль не зависит от AppKit.

use serde::Serialize;

use crate::args::Contrast;

/// Цвет в sRGB, по 8 бит на канал.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn opaque(r: u8, g: u8, b: u8) -> Rgba {
        Rgba { r, g, b, a: 255 }
    }

    pub const fn from_hex(value: u32) -> Rgba {
        Rgba::opaque((value >> 16) as u8, (value >> 8) as u8, value as u8)
    }

    /// Каналы 0…1 в 8 бит с округлением.
    pub fn from_unit(r: f64, g: f64, b: f64, a: f64) -> Rgba {
        let channel = |value: f64| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        Rgba {
            r: channel(r),
            g: channel(g),
            b: channel(b),
            a: channel(a),
        }
    }

    /// Наложение `top` с непрозрачностью `alpha` на непрозрачный `self` в кодированных
    /// значениях sRGB, как смешивает композитор.
    pub fn blend(self, top: Rgba, alpha: f64) -> Rgba {
        let mix = |base: u8, over: u8| {
            (f64::from(base) * (1.0 - alpha) + f64::from(over) * alpha).round() as u8
        };
        Rgba {
            r: mix(self.r, top.r),
            g: mix(self.g, top.g),
            b: mix(self.b, top.b),
            a: 255,
        }
    }

    /// Наибольшее расхождение по каналам цвета и альфе.
    pub fn distance(self, other: Rgba) -> u8 {
        [
            self.r.abs_diff(other.r),
            self.g.abs_diff(other.g),
            self.b.abs_diff(other.b),
            self.a.abs_diff(other.a),
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
    }

    pub fn hex(self) -> String {
        if self.a == 255 {
            format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
        } else {
            format!("#{:02X}{:02X}{:02X}{:02X}", self.r, self.g, self.b, self.a)
        }
    }

    pub fn unit(self) -> (f64, f64, f64) {
        (
            f64::from(self.r) / 255.0,
            f64::from(self.g) / 255.0,
            f64::from(self.b) / 255.0,
        )
    }
}

/// Цвет с долей непрозрачности, как `rgba()` в CSS: доля хранится точно, без округления до байта.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Tint {
    pub color: Rgba,
    pub alpha: f64,
}

impl Tint {
    const fn new(r: u8, g: u8, b: u8, alpha: f64) -> Tint {
        Tint {
            color: Rgba::opaque(r, g, b),
            alpha,
        }
    }

    /// Цвет, который даёт этот слой поверх непрозрачного `base`.
    pub fn over(self, base: Rgba) -> Rgba {
        base.blend(self.color, self.alpha)
    }
}

/// Слой тени: смещение по вертикали, размытие и цвет. Смещения по горизонтали и растяжения в
/// токенах нет.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Shadow {
    pub y: f64,
    pub blur: f64,
    pub color: Tint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

/// Режим ввода: от него зависит цвет фокуса.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Navigation,
    Input,
}

/// Ячейка темы: тема и контраст.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub theme: Theme,
    pub contrast: Contrast,
}

impl Cell {
    #[cfg(test)]
    pub const ALL: [Cell; 4] = [
        Cell::new(Theme::Dark, Contrast::Normal),
        Cell::new(Theme::Light, Contrast::Normal),
        Cell::new(Theme::Dark, Contrast::More),
        Cell::new(Theme::Light, Contrast::More),
    ];

    pub const fn new(theme: Theme, contrast: Contrast) -> Cell {
        Cell { theme, contrast }
    }

    pub fn name(self) -> &'static str {
        match (self.theme, self.contrast) {
            (Theme::Dark, Contrast::Normal) => "dark",
            (Theme::Light, Contrast::Normal) => "light",
            (Theme::Dark, Contrast::More) => "dark-more",
            (Theme::Light, Contrast::More) => "light-more",
        }
    }
}

/// Токены сцены в одной ячейке и одном режиме. Размеры — в логических точках.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Tokens {
    pub chrome: Rgba,
    pub chrome_hi: Rgba,
    pub chrome_press: Rgba,
    pub surface: Rgba,
    pub panel: Rgba,
    pub panel_head: Rgba,
    pub line: Rgba,
    pub line_2: Rgba,
    pub text: Rgba,
    pub text_2: Rgba,
    pub text_3: Rgba,
    pub sel: Rgba,
    pub hover: Rgba,
    pub run: Rgba,
    /// Цвет режима: `--nav-line` в Навигации, `--inp-line` во Вводе.
    pub focus: Rgba,
    /// Доля тона режима у панели с фокусом.
    pub focus_a: f64,
    /// Толщина линии под активной вкладкой.
    pub focus_h: f64,
    /// Вуаль панели без фокуса.
    pub dim: Tint,
    pub shadow: [Shadow; 2],
    pub r_row: f64,
    pub r_ctl: f64,
    pub r_plate: f64,
    pub r_float: f64,
    pub gap: f64,
    pub row: f64,
    pub titlebar: f64,
    pub tabbar: f64,
    /// Кегль подписи вкладки.
    pub t_tab: f64,
    /// Кегль нагрузки и служебных подписей.
    pub t_meta: f64,
    /// Кегль основного текста, строк палитры.
    pub t_body: f64,
    /// Кегль поля палитры.
    pub t_title: f64,
}

/// Значение токена для сверки и записи в итог.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Value {
    Color(Rgba),
    Tint(Tint),
    Px(f64),
    Number(f64),
    Shadow(Vec<Shadow>),
}

impl Tokens {
    /// Имена переменных прототипа и значения этой ячейки.
    pub fn entries(&self) -> Vec<(&'static str, Value)> {
        use Value::{Color, Number, Px};
        vec![
            ("--chrome", Color(self.chrome)),
            ("--chrome-hi", Color(self.chrome_hi)),
            ("--chrome-press", Color(self.chrome_press)),
            ("--surface", Color(self.surface)),
            ("--panel", Color(self.panel)),
            ("--panel-head", Color(self.panel_head)),
            ("--line", Color(self.line)),
            ("--line-2", Color(self.line_2)),
            ("--text", Color(self.text)),
            ("--text-2", Color(self.text_2)),
            ("--text-3", Color(self.text_3)),
            ("--sel", Color(self.sel)),
            ("--hover", Color(self.hover)),
            ("--run", Color(self.run)),
            ("--focus", Color(self.focus)),
            ("--focus-a", Number(self.focus_a)),
            ("--focus-h", Px(self.focus_h)),
            ("--dim", Value::Tint(self.dim)),
            ("--shadow", Value::Shadow(self.shadow.to_vec())),
            ("--r-row", Px(self.r_row)),
            ("--r-ctl", Px(self.r_ctl)),
            ("--r-plate", Px(self.r_plate)),
            ("--r-float", Px(self.r_float)),
            ("--gap", Px(self.gap)),
            ("--row", Px(self.row)),
            ("--titlebar", Px(self.titlebar)),
            ("--tabbar", Px(self.tabbar)),
            ("--t-tab", Px(self.t_tab)),
            ("--t-meta", Px(self.t_meta)),
            ("--t-body", Px(self.t_body)),
            ("--t-title", Px(self.t_title)),
        ]
    }

    /// Цвет плитки без фокуса: вуаль поверх `panel`.
    pub fn veiled_panel(&self) -> Rgba {
        self.dim.over(self.panel)
    }

    /// Цвет плитки с фокусом: тон режима поверх `panel`.
    pub fn focused_panel(&self) -> Rgba {
        self.panel.blend(self.focus, self.focus_a)
    }

    /// Тон режима как слой.
    pub fn tone(&self) -> Tint {
        Tint {
            color: self.focus,
            alpha: self.focus_a,
        }
    }
}

const DARK_SHADOW: [Shadow; 2] = [
    Shadow {
        y: 18.0,
        blur: 48.0,
        color: Tint::new(0, 0, 0, 0.55),
    },
    Shadow {
        y: 2.0,
        blur: 8.0,
        color: Tint::new(0, 0, 0, 0.4),
    },
];

const LIGHT_SHADOW: [Shadow; 2] = [
    Shadow {
        y: 18.0,
        blur: 48.0,
        color: Tint::new(20, 22, 30, 0.18),
    },
    Shadow {
        y: 2.0,
        blur: 8.0,
        color: Tint::new(20, 22, 30, 0.10),
    },
];

pub fn tokens(cell: Cell, mode: InputMode) -> Tokens {
    let dark = Tokens {
        chrome: Rgba::from_hex(0x1E1D1B),
        chrome_hi: Rgba::from_hex(0x282725),
        chrome_press: Rgba::from_hex(0x312F2C),
        surface: Rgba::from_hex(0x191816),
        panel: Rgba::from_hex(0x0A0908),
        panel_head: Rgba::from_hex(0x11100E),
        line: Rgba::from_hex(0x302E2B),
        line_2: Rgba::from_hex(0x403D39),
        text: Rgba::from_hex(0xD9D4CA),
        text_2: Rgba::from_hex(0xABA59A),
        text_3: Rgba::from_hex(0x8F897E),
        sel: Rgba::from_hex(0x2E2C29),
        hover: Rgba::from_hex(0x252421),
        run: Rgba::from_hex(0x8FAACC),
        focus: match mode {
            InputMode::Navigation => Rgba::from_hex(0x7F9BC0),
            InputMode::Input => Rgba::from_hex(0x8DB08F),
        },
        focus_a: 0.05,
        focus_h: 2.0,
        dim: Tint::new(67, 64, 58, 0.31),
        shadow: DARK_SHADOW,
        r_row: 5.0,
        r_ctl: 6.0,
        r_plate: 7.0,
        r_float: 10.0,
        gap: 6.0,
        row: 26.0,
        titlebar: 40.0,
        tabbar: 34.0,
        t_tab: 12.5,
        t_meta: 11.5,
        t_body: 13.0,
        t_title: 15.0,
    };
    let light = Tokens {
        chrome: Rgba::from_hex(0xEFEDE8),
        chrome_hi: Rgba::from_hex(0xE5E2DC),
        chrome_press: Rgba::from_hex(0xDAD6CF),
        surface: Rgba::from_hex(0xFAF9F6),
        panel: Rgba::from_hex(0xFFFFFF),
        panel_head: Rgba::from_hex(0xF8F7F3),
        line: Rgba::from_hex(0xDDD9D1),
        line_2: Rgba::from_hex(0xC9C4BA),
        text: Rgba::from_hex(0x2B2925),
        text_2: Rgba::from_hex(0x4F4B44),
        text_3: Rgba::from_hex(0x66615A),
        sel: Rgba::from_hex(0xE0DCD3),
        hover: Rgba::from_hex(0xE8E5DF),
        run: Rgba::from_hex(0x48678C),
        focus: match mode {
            InputMode::Navigation => Rgba::from_hex(0x48678C),
            InputMode::Input => Rgba::from_hex(0x4D7553),
        },
        focus_a: 0.02,
        dim: Tint::new(205, 202, 195, 0.22),
        shadow: LIGHT_SHADOW,
        ..dark
    };
    match (cell.theme, cell.contrast) {
        (Theme::Dark, Contrast::Normal) => dark,
        (Theme::Light, Contrast::Normal) => light,
        (Theme::Dark, Contrast::More) => Tokens {
            line: Rgba::from_hex(0x5C5952),
            line_2: Rgba::from_hex(0x7F7A70),
            text_2: Rgba::from_hex(0xD0CBC0),
            text_3: Rgba::from_hex(0xB3ADA1),
            dim: Tint::new(255, 255, 255, 0.07),
            focus_a: 0.06,
            focus_h: 3.0,
            ..dark
        },
        (Theme::Light, Contrast::More) => Tokens {
            line: Rgba::from_hex(0x9A958B),
            line_2: Rgba::from_hex(0x6F6B62),
            text_2: Rgba::from_hex(0x38352F),
            text_3: Rgba::from_hex(0x4F4B44),
            dim: Tint::new(67, 64, 58, 0.1),
            focus_a: 0.04,
            focus_h: 3.0,
            ..light
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    const TOKENS_CSS: &str = include_str!("../../../prototype/css/tokens.css");
    const ADR_0027: &str = include_str!("../../../docs/adr/0027-focused-panel-contrast.md");

    const ROOT: &str = ":root";
    const INPUT: &str = ":root[data-mode=\"input\"]";
    const LIGHT: &str = ":root[data-theme=\"light\"]";
    const MORE: &str = ":root[data-contrast=\"more\"]";
    const MORE_LIGHT: &str = ":root[data-contrast=\"more\"][data-theme=\"light\"]";

    /// Блоки `селектор { --имя: значение; }` в порядке файла; свойства без `--` пропускаются.
    fn css_blocks(css: &str) -> Vec<(String, BTreeMap<String, String>)> {
        let mut text = String::new();
        let mut rest = css;
        while let Some(start) = rest.find("/*") {
            text.push_str(&rest[..start]);
            rest = rest[start..]
                .find("*/")
                .map_or("", |end| &rest[start + end + 2..]);
        }
        text.push_str(rest);
        let mut blocks = Vec::new();
        let mut rest = text.as_str();
        while let Some(open) = rest.find('{') {
            let selector = rest[..open]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let Some(close) = rest[open..].find('}') else {
                break;
            };
            let body = &rest[open + 1..open + close];
            let declarations = body
                .split(';')
                .filter_map(|declaration| {
                    let (name, value) = declaration.split_once(':')?;
                    let name = name.trim();
                    name.starts_with("--")
                        .then(|| (name.to_owned(), value.trim().to_owned()))
                })
                .collect();
            blocks.push((selector, declarations));
            rest = &rest[open + close + 1..];
        }
        blocks
    }

    /// Каскад ячейки и режима: блоки применяются в порядке файла, у всех одна специфичность,
    /// кроме последнего, у которого она выше и который в файле тоже последний.
    fn cascade(cell: Cell, mode: InputMode) -> BTreeMap<String, String> {
        let mut applied = vec![ROOT];
        if mode == InputMode::Input {
            applied.push(INPUT);
        }
        if cell.theme == Theme::Light {
            applied.push(LIGHT);
        }
        if cell.contrast == Contrast::More {
            applied.push(MORE);
        }
        if cell == Cell::new(Theme::Light, Contrast::More) {
            applied.push(MORE_LIGHT);
        }
        let blocks = css_blocks(TOKENS_CSS);
        for selector in &applied {
            assert!(
                blocks.iter().any(|(s, _)| s == selector),
                "в tokens.css нет блока {selector}"
            );
        }
        let mut merged = BTreeMap::new();
        for (selector, declarations) in blocks {
            if applied.contains(&selector.as_str()) {
                merged.extend(declarations);
            }
        }
        merged
    }

    /// Значение с разрешением `var()` после каскада.
    fn resolve(merged: &BTreeMap<String, String>, name: &str) -> String {
        let mut value = merged
            .get(name)
            .unwrap_or_else(|| panic!("в каскаде нет {name}"))
            .clone();
        for _ in 0..8 {
            let Some(inner) = value
                .strip_prefix("var(")
                .and_then(|rest| rest.strip_suffix(')'))
            else {
                return value;
            };
            value = merged
                .get(inner.trim())
                .unwrap_or_else(|| panic!("{name}: нет {inner}"))
                .clone();
        }
        panic!("{name}: цикл var()")
    }

    fn parse_hex(value: &str) -> Rgba {
        let hex = value.strip_prefix('#').unwrap_or_else(|| panic!("{value}"));
        assert_eq!(hex.len(), 6, "{value}");
        Rgba::from_hex(u32::from_str_radix(hex, 16).unwrap_or_else(|_| panic!("{value}")))
    }

    fn parse_tint(value: &str) -> Tint {
        let inner = value
            .strip_prefix("rgba(")
            .and_then(|rest| rest.strip_suffix(')'))
            .unwrap_or_else(|| panic!("{value}"));
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        assert_eq!(parts.len(), 4, "{value}");
        let channel = |part: &str| part.parse::<u8>().unwrap_or_else(|_| panic!("{value}"));
        Tint {
            color: Rgba::opaque(channel(parts[0]), channel(parts[1]), channel(parts[2])),
            alpha: parts[3].parse().unwrap_or_else(|_| panic!("{value}")),
        }
    }

    fn parse_px(value: &str) -> f64 {
        value
            .strip_suffix("px")
            .and_then(|number| number.parse().ok())
            .unwrap_or_else(|| panic!("{value}"))
    }

    /// Запятые верхнего уровня, вне скобок.
    fn split_top(value: &str) -> Vec<&str> {
        let (mut depth, mut start, mut parts) = (0usize, 0usize, Vec::new());
        for (index, ch) in value.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    parts.push(value[start..index].trim());
                    start = index + 1;
                }
                _ => {}
            }
        }
        parts.push(value[start..].trim());
        parts
    }

    fn parse_shadow(value: &str) -> Vec<Shadow> {
        split_top(value)
            .into_iter()
            .map(|layer| {
                let color_at = layer.find("rgba(").unwrap_or_else(|| panic!("{layer}"));
                let lengths: Vec<&str> = layer[..color_at].split_whitespace().collect();
                assert_eq!(lengths.len(), 3, "{layer}");
                let x = if lengths[0] == "0" {
                    0.0
                } else {
                    parse_px(lengths[0])
                };
                assert_eq!(x, 0.0, "смещение по x: {layer}");
                Shadow {
                    y: parse_px(lengths[1]),
                    blur: parse_px(lengths[2]),
                    color: parse_tint(&layer[color_at..]),
                }
            })
            .collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn matches(expected: &Value, css: &str) -> bool {
        match expected {
            Value::Color(color) => parse_hex(css) == *color,
            Value::Tint(tint) => {
                let parsed = parse_tint(css);
                parsed.color == tint.color && close(parsed.alpha, tint.alpha)
            }
            Value::Px(px) => close(parse_px(css), *px),
            Value::Number(number) => css.parse::<f64>().is_ok_and(|n| close(n, *number)),
            Value::Shadow(layers) => {
                let parsed = parse_shadow(css);
                parsed.len() == layers.len()
                    && parsed.iter().zip(layers).all(|(a, b)| {
                        close(a.y, b.y)
                            && close(a.blur, b.blur)
                            && a.color.color == b.color.color
                            && close(a.color.alpha, b.color.alpha)
                    })
            }
        }
    }

    #[test]
    fn every_token_matches_prototype_cell() {
        for cell in Cell::ALL {
            for mode in [InputMode::Navigation, InputMode::Input] {
                let merged = cascade(cell, mode);
                for (name, value) in tokens(cell, mode).entries() {
                    let css = resolve(&merged, name);
                    assert!(
                        matches(&value, &css),
                        "{} {mode:?} {name}: в пробе {value:?}, в прототипе {css}",
                        cell.name()
                    );
                }
            }
        }
    }

    #[test]
    fn focus_follows_mode_and_cell() {
        let dark = Cell::new(Theme::Dark, Contrast::Normal);
        let light = Cell::new(Theme::Light, Contrast::Normal);
        assert_eq!(
            tokens(dark, InputMode::Navigation).focus,
            Rgba::from_hex(0x7F9BC0)
        );
        assert_eq!(
            tokens(dark, InputMode::Input).focus,
            Rgba::from_hex(0x8DB08F)
        );
        // Светлая ячейка переопределяет `--nav-line`, а `--focus` — ссылка на него.
        assert_eq!(
            tokens(light, InputMode::Navigation).focus,
            Rgba::from_hex(0x48678C)
        );
        assert_eq!(
            tokens(light, InputMode::Input).focus,
            Rgba::from_hex(0x4D7553)
        );
    }

    #[test]
    fn css_parser_handles_comments_and_var() {
        let blocks =
            css_blocks(":root { /* c */ --a: #010203; --b: var(--a); color-scheme: dark; }");
        assert_eq!(blocks.len(), 1);
        let (selector, declarations) = &blocks[0];
        assert_eq!(selector, ":root");
        assert_eq!(declarations.len(), 2);
        assert_eq!(resolve(declarations, "--b"), "#010203");
        assert_eq!(
            split_top("0 1px 2px rgba(0,0,0,.5), 0 3px 4px rgba(1,2,3,.1)").len(),
            2
        );
    }

    /// Строки таблицы «Значения» ADR-0027: ячейка, `panel`, `panel-head`, вуаль, тон режима.
    fn adr_rows() -> Vec<Vec<String>> {
        ADR_0027
            .lines()
            .filter(|line| line.starts_with("| Тёмная") || line.starts_with("| Светлая"))
            .map(|line| {
                line.trim_matches('|')
                    .split('|')
                    .map(|cell| cell.trim().replace('`', ""))
                    .collect()
            })
            .collect()
    }

    fn adr_cell(name: &str) -> Cell {
        match name {
            "Тёмная" => Cell::new(Theme::Dark, Contrast::Normal),
            "Светлая" => Cell::new(Theme::Light, Contrast::Normal),
            "Тёмная, Increase Contrast" => Cell::new(Theme::Dark, Contrast::More),
            "Светлая, Increase Contrast" => Cell::new(Theme::Light, Contrast::More),
            other => panic!("неизвестная ячейка ADR-0027: {other}"),
        }
    }

    fn percent(text: &str) -> f64 {
        text.trim()
            .strip_suffix('%')
            .and_then(|n| n.trim().parse::<f64>().ok())
            .map(|n| n / 100.0)
            .unwrap_or_else(|| panic!("{text}"))
    }

    #[test]
    fn tokens_match_adr_0027_table() {
        let rows = adr_rows();
        assert_eq!(rows.len(), 4, "{rows:?}");
        for row in rows {
            assert_eq!(row.len(), 5, "{row:?}");
            let tokens = tokens(adr_cell(&row[0]), InputMode::Navigation);
            assert_eq!(tokens.panel, parse_hex(&row[1]), "{row:?}");
            assert_eq!(tokens.panel_head, parse_hex(&row[2]), "{row:?}");
            let (veil, share) = row[3].split_once(',').unwrap_or_else(|| panic!("{row:?}"));
            assert_eq!(tokens.dim.color, parse_hex(veil.trim()), "{row:?}");
            assert!(close(tokens.dim.alpha, percent(share)), "{row:?}");
            assert!(close(tokens.focus_a, percent(&row[4])), "{row:?}");
        }
    }

    /// Ожидаемый цвет вуали посчитан вручную: канал `panel·(1−a) + dim·a`. У тёмной ячейки
    /// синий канал ровно 23,5, поэтому сравнение идёт с допуском в одну единицу.
    #[test]
    fn veil_color_in_every_cell() {
        let expected = [
            ("dark", Rgba::opaque(28, 26, 24)),
            ("light", Rgba::opaque(244, 243, 242)),
            ("dark-more", Rgba::opaque(27, 26, 25)),
            ("light-more", Rgba::opaque(236, 236, 235)),
        ];
        for (cell, (name, color)) in Cell::ALL.into_iter().zip(expected) {
            assert_eq!(cell.name(), name);
            let veiled = tokens(cell, InputMode::Navigation).veiled_panel();
            assert!(
                veiled.distance(color) <= 1,
                "{name}: {} против {}",
                veiled.hex(),
                color.hex()
            );
        }
    }

    #[test]
    fn focused_panel_tone() {
        let dark = tokens(
            Cell::new(Theme::Dark, Contrast::Normal),
            InputMode::Navigation,
        );
        // #0A0908 и 5% #7F9BC0: 10·0,95 + 127·0,05 = 15,85; 9·0,95 + 155·0,05 = 16,3;
        // 8·0,95 + 192·0,05 = 17,2.
        assert!(dark.focused_panel().distance(Rgba::opaque(16, 16, 17)) <= 1);
        let light = tokens(
            Cell::new(Theme::Light, Contrast::Normal),
            InputMode::Navigation,
        );
        // #FFFFFF и 2% #48678C: 255 − 0,02·(183, 152, 115).
        assert!(light.focused_panel().distance(Rgba::opaque(251, 252, 253)) <= 1);
    }

    #[test]
    fn rgba_helpers() {
        assert_eq!(Rgba::from_hex(0x0A0908), Rgba::opaque(10, 9, 8));
        assert_eq!(Rgba::opaque(10, 9, 8).hex(), "#0A0908");
        assert_eq!(Rgba::from_unit(-1.0, 2.0, 0.5, 0.5).hex(), "#00FF8080");
        let base = Rgba::opaque(128, 128, 128);
        assert_eq!(
            base.blend(Rgba::opaque(0, 0, 0), 0.5),
            Rgba::opaque(64, 64, 64)
        );
        assert_eq!(Rgba::opaque(10, 9, 8).distance(Rgba::opaque(11, 9, 6)), 2);
        let translucent = Rgba {
            a: 250,
            ..Rgba::opaque(0, 0, 0)
        };
        assert_eq!(Rgba::opaque(0, 0, 0).distance(translucent), 5);
    }
}
