//! Ожидаемые цвета слотов: собственный разбор `prototype/css/tokens.css` и таблицы ADR-0027.
//!
//! Код темы приложения сюда не входит: ожидания строятся только из двух источников, которыми
//! приложение не рисует. Блоки `:root[…]` применяются по числу условий, затем по порядку в файле,
//! `var()` разрешается после слияния ячейки. `panel`, `panel-head`, вуаль и доля тона берутся из
//! таблицы ADR-0027, цвет тона — `--focus` режима из CSS.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Error;

/// Источник токенов, встроенный при сборке оракула.
pub const TOKENS_CSS: &str = include_str!("../../../prototype/css/tokens.css");
/// ADR-0027 с таблицей значений фокуса, встроенный при сборке оракула.
pub const ADR_0027: &str = include_str!("../../../docs/adr/0027-focused-panel-contrast.md");

/// Цвет sRGB по 8 бит на канал.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }

    pub const fn channels(self) -> [u8; 3] {
        [self.r, self.g, self.b]
    }

    /// Цвет из `#RRGGBB`.
    pub fn parse_hex(text: &str) -> Result<Rgb, Error> {
        let digits = text
            .trim()
            .strip_prefix('#')
            .filter(|digits| digits.len() == 6 && digits.is_ascii())
            .ok_or_else(|| Error::new(format!("не цвет #RRGGBB: {text:?}")))?;
        let channel = |range: std::ops::Range<usize>| {
            u8::from_str_radix(&digits[range], 16)
                .map_err(|e| Error::new(format!("не цвет #RRGGBB: {text:?}: {e}")))
        };
        Ok(Rgb::new(channel(0..2)?, channel(2..4)?, channel(4..6)?))
    }

    /// Наибольшее расхождение каналов с `other`.
    pub fn max_delta(self, other: Rgb) -> u8 {
        self.channels()
            .into_iter()
            .zip(other.channels())
            .map(|(a, b)| a.abs_diff(b))
            .max()
            .unwrap_or(0)
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

impl Serialize for Rgb {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Полупрозрачный слой над фоном.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tint {
    pub color: Rgb,
    pub alpha: f64,
}

/// Наложение слоя на непрозрачный фон в кодированных значениях sRGB, как `rgba()` в CSS: канал
/// `фон + (слой − фон) × доля`, округление до ближайшего целого один раз в конце.
pub fn blend(base: Rgb, tint: Tint) -> Rgb {
    let mix = |base: u8, over: u8| {
        let value = f64::from(base) + (f64::from(over) - f64::from(base)) * tint.alpha;
        // Значение в пределах 0..=255 по построению: доля проверена при разборе.
        value.round().clamp(0.0, 255.0) as u8
    };
    Rgb::new(
        mix(base.r, tint.color.r),
        mix(base.g, tint.color.g),
        mix(base.b, tint.color.b),
    )
}

/// Светлота ячейки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    Light,
    Dark,
}

/// Контраст ячейки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Contrast {
    Normal,
    Increased,
}

/// Ячейка оформления: имена осей совпадают с `lightness` и `contrast` ответа `ui.window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub appearance: Appearance,
    pub contrast: Contrast,
}

impl Cell {
    pub const ALL: [Cell; 4] = [
        Cell::new(Appearance::Dark, Contrast::Normal),
        Cell::new(Appearance::Light, Contrast::Normal),
        Cell::new(Appearance::Dark, Contrast::Increased),
        Cell::new(Appearance::Light, Contrast::Increased),
    ];

    pub const fn new(appearance: Appearance, contrast: Contrast) -> Cell {
        Cell {
            appearance,
            contrast,
        }
    }
}

impl fmt::Display for Cell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let appearance = match self.appearance {
            Appearance::Light => "светлая",
            Appearance::Dark => "тёмная",
        };
        match self.contrast {
            Contrast::Normal => write!(f, "{appearance}"),
            Contrast::Increased => write!(f, "{appearance}, Increase Contrast"),
        }
    }
}

/// Режим клавиатуры, чей цвет даёт тон панели с фокусом.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Navigation,
    Input,
}

/// Слой фокуса над заливкой участка.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// Без слоя.
    #[default]
    None,
    /// Вуаль панели без фокуса.
    Veil,
    /// Тон цвета режима навигации у панели с фокусом.
    ToneNavigation,
    /// Тон цвета режима ввода у панели с фокусом.
    ToneInput,
}

/// Блок правил `:root[…]`: условия на атрибуты корня и объявления свойств по порядку.
#[derive(Debug)]
struct RootBlock {
    attributes: Vec<(String, String)>,
    declarations: Vec<(String, String)>,
}

fn strip_comments(css: &str) -> Result<String, Error> {
    let mut out = String::new();
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        let end = rest[start + 2..]
            .find("*/")
            .ok_or_else(|| Error::new("tokens.css: незакрытый комментарий"))?;
        rest = &rest[start + 2 + end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Атрибуты селектора `:root[a="b"][c="d"]`. Селектор с потомком (`:root[…] *`) свойств корня
/// не задаёт и даёт `None`, любой другой вид — ошибка.
fn root_selector(selector: &str) -> Result<Option<Vec<(String, String)>>, Error> {
    let selector = selector.trim();
    if selector.contains(char::is_whitespace) {
        return Ok(None);
    }
    let unknown = || Error::new(format!("tokens.css: неизвестный селектор {selector:?}"));
    let mut rest = selector.strip_prefix(":root").ok_or_else(unknown)?;
    let mut attributes = Vec::new();
    while !rest.is_empty() {
        let body = rest.strip_prefix('[').ok_or_else(unknown)?;
        let end = body.find(']').ok_or_else(unknown)?;
        let (name, value) = body[..end].split_once('=').ok_or_else(unknown)?;
        attributes.push((name.to_owned(), value.trim_matches('"').to_owned()));
        rest = &body[end + 1..];
    }
    Ok(Some(attributes))
}

fn parse_blocks(css: &str) -> Result<Vec<RootBlock>, Error> {
    let css = strip_comments(css)?;
    let mut blocks = Vec::new();
    let mut rest = css.as_str();
    while let Some(open) = rest.find('{') {
        let selector = &rest[..open];
        let close = rest[open..]
            .find('}')
            .ok_or_else(|| Error::new(format!("tokens.css: незакрытый блок {selector:?}")))?;
        let body = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];
        let Some(attributes) = root_selector(selector)? else {
            continue;
        };
        let mut declarations = Vec::new();
        for declaration in body.split(';').map(str::trim) {
            if declaration.is_empty() {
                continue;
            }
            let (name, value) = declaration.split_once(':').ok_or_else(|| {
                Error::new(format!(
                    "tokens.css: объявление без значения {declaration:?}"
                ))
            })?;
            let name = name.trim();
            if name.starts_with("--") {
                declarations.push((name.to_owned(), value.trim().to_owned()));
            }
        }
        blocks.push(RootBlock {
            attributes,
            declarations,
        });
    }
    if !rest.trim().is_empty() {
        return Err(Error::new(format!(
            "tokens.css: лишний текст после блоков: {:?}",
            rest.trim()
        )));
    }
    Ok(blocks)
}

/// Переменные `tokens.css` после каскада для атрибутов корня.
#[derive(Debug, Clone)]
struct Variables(BTreeMap<String, String>);

impl Variables {
    fn cascade(blocks: &[RootBlock], root: &[(&str, &str)]) -> Variables {
        let mut matching: Vec<(usize, usize, &RootBlock)> = blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| {
                block
                    .attributes
                    .iter()
                    .all(|(name, value)| root.iter().any(|(n, v)| n == name && v == value))
            })
            .map(|(order, block)| (block.attributes.len(), order, block))
            .collect();
        matching.sort_by_key(|(specificity, order, _)| (*specificity, *order));
        let mut merged = BTreeMap::new();
        for (_, _, block) in matching {
            for (name, value) in &block.declarations {
                merged.insert(name.clone(), value.clone());
            }
        }
        Variables(merged)
    }

    /// Значение переменной с разрешёнными `var()`.
    fn resolve(&self, name: &str) -> Result<String, Error> {
        let mut value = self
            .0
            .get(name)
            .ok_or_else(|| Error::new(format!("tokens.css: переменной {name} нет")))?
            .clone();
        for _ in 0..16 {
            let Some(start) = value.find("var(") else {
                return Ok(value);
            };
            let end = value[start..]
                .find(')')
                .ok_or_else(|| Error::new(format!("tokens.css: незакрытый var() в {name}")))?;
            let inner = value[start + 4..start + end].trim().to_owned();
            if inner.contains(',') {
                return Err(Error::new(format!(
                    "tokens.css: var() с запасным значением в {name} не разбирается"
                )));
            }
            let replacement = self.0.get(&inner).ok_or_else(|| {
                Error::new(format!(
                    "tokens.css: {name} ссылается на отсутствующую {inner}"
                ))
            })?;
            value.replace_range(start..start + end + 1, replacement);
        }
        Err(Error::new(format!("tokens.css: цикл var() в {name}")))
    }
}

fn root_attributes(cell: Cell, mode: Mode) -> Vec<(&'static str, &'static str)> {
    let mut root = Vec::new();
    if cell.appearance == Appearance::Light {
        root.push(("data-theme", "light"));
    }
    if cell.contrast == Contrast::Increased {
        root.push(("data-contrast", "more"));
    }
    if mode == Mode::Input {
        root.push(("data-mode", "input"));
    }
    root
}

/// Строка таблицы ADR-0027.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdrRow {
    pub panel: Rgb,
    pub panel_head: Rgb,
    pub veil: Tint,
    pub tone: f64,
}

fn parse_percent(text: &str) -> Result<f64, Error> {
    let number = text
        .trim()
        .strip_suffix('%')
        .ok_or_else(|| Error::new(format!("ADR-0027: не доля в процентах: {text:?}")))?;
    let percent = number
        .trim()
        .parse::<f64>()
        .map_err(|e| Error::new(format!("ADR-0027: не доля {text:?}: {e}")))?;
    if !(0.0..=100.0).contains(&percent) {
        return Err(Error::new(format!("ADR-0027: доля вне 0–100%: {text:?}")));
    }
    Ok(percent / 100.0)
}

fn table_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().replace('`', ""))
        .collect()
}

fn adr_cell(name: &str) -> Result<Cell, Error> {
    let cell = match name {
        "Тёмная" => Cell::new(Appearance::Dark, Contrast::Normal),
        "Светлая" => Cell::new(Appearance::Light, Contrast::Normal),
        "Тёмная, Increase Contrast" => Cell::new(Appearance::Dark, Contrast::Increased),
        "Светлая, Increase Contrast" => Cell::new(Appearance::Light, Contrast::Increased),
        other => {
            return Err(Error::new(format!(
                "ADR-0027: неизвестная ячейка {other:?}"
            )));
        }
    };
    Ok(cell)
}

/// Таблица раздела «Значения» ADR-0027 по ячейкам. Изменённые столбцы, лишняя или пропущенная
/// ячейка — ошибка.
fn parse_adr_table(adr: &str) -> Result<BTreeMap<Cell, AdrRow>, Error> {
    let section = adr
        .split("### Значения")
        .nth(1)
        .ok_or_else(|| Error::new("ADR-0027: нет раздела «Значения»"))?;
    let mut lines = section
        .lines()
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'));
    let header = lines
        .next()
        .ok_or_else(|| Error::new("ADR-0027: нет таблицы значений"))?;
    let expected = ["Ячейка", "panel", "panel-head", "Вуаль", "Тон режима"];
    if table_cells(header) != expected {
        return Err(Error::new(format!(
            "ADR-0027: столбцы таблицы изменились: {header}"
        )));
    }
    lines
        .next()
        .ok_or_else(|| Error::new("ADR-0027: в таблице нет разделителя"))?;
    let mut rows = BTreeMap::new();
    for line in lines {
        let cells = table_cells(line);
        let [name, panel, panel_head, veil, tone] = cells.as_slice() else {
            return Err(Error::new(format!(
                "ADR-0027: строка не из пяти ячеек: {line}"
            )));
        };
        let (veil_color, veil_alpha) = veil
            .split_once(',')
            .ok_or_else(|| Error::new(format!("ADR-0027: вуаль без доли: {veil:?}")))?;
        let row = AdrRow {
            panel: Rgb::parse_hex(panel)?,
            panel_head: Rgb::parse_hex(panel_head)?,
            veil: Tint {
                color: Rgb::parse_hex(veil_color)?,
                alpha: parse_percent(veil_alpha)?,
            },
            tone: parse_percent(tone)?,
        };
        if rows.insert(adr_cell(name)?, row).is_some() {
            return Err(Error::new(format!("ADR-0027: ячейка {name:?} дважды")));
        }
    }
    if rows.len() != Cell::ALL.len() {
        return Err(Error::new(format!(
            "ADR-0027: в таблице {} ячеек вместо {}",
            rows.len(),
            Cell::ALL.len()
        )));
    }
    Ok(rows)
}

/// Цвета ячеек из двух источников.
#[derive(Debug)]
pub struct Palette {
    blocks: Vec<RootBlock>,
    adr: BTreeMap<Cell, AdrRow>,
}

impl Palette {
    /// Палитра из источников, встроенных при сборке.
    pub fn embedded() -> Result<Palette, Error> {
        Palette::parse(TOKENS_CSS, ADR_0027)
    }

    pub fn parse(tokens_css: &str, adr_0027: &str) -> Result<Palette, Error> {
        Ok(Palette {
            blocks: parse_blocks(tokens_css)?,
            adr: parse_adr_table(adr_0027)?,
        })
    }

    fn variables(&self, cell: Cell, mode: Mode) -> Variables {
        Variables::cascade(&self.blocks, &root_attributes(cell, mode))
    }

    fn adr_row(&self, cell: Cell) -> Result<&AdrRow, Error> {
        self.adr
            .get(&cell)
            .ok_or_else(|| Error::new(format!("ADR-0027: нет строки для ячейки «{cell}»")))
    }

    /// Строка таблицы ADR-0027 ячейки.
    pub fn adr(&self, cell: Cell) -> Result<AdrRow, Error> {
        self.adr_row(cell).copied()
    }

    /// Значение переменной `tokens.css` в ячейке после каскада и `var()`, без разбора.
    pub fn variable(&self, cell: Cell, name: &str) -> Result<String, Error> {
        self.variables(cell, Mode::Navigation).resolve(name)
    }

    /// Цвет слота: имя переменной `tokens.css` без `--`, `panel` и `panel-head` — из ADR-0027.
    pub fn slot(&self, cell: Cell, slot: &str) -> Result<Rgb, Error> {
        let valid = !slot.is_empty()
            && slot
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid {
            return Err(Error::new(format!("неверное имя слота {slot:?}")));
        }
        let row = self.adr_row(cell)?;
        match slot {
            "panel" => Ok(row.panel),
            "panel-head" => Ok(row.panel_head),
            _ => {
                let text = self
                    .variables(cell, Mode::Navigation)
                    .resolve(&format!("--{slot}"))?;
                Rgb::parse_hex(&text)
                    .map_err(|e| Error::new(format!("слот {slot} в ячейке «{cell}»: {e}")))
            }
        }
    }

    /// Слой фокуса ячейки: вуаль из ADR-0027, тон — `--focus` режима с долей из ADR-0027.
    pub fn layer(&self, cell: Cell, layer: Layer) -> Result<Option<Tint>, Error> {
        let row = self.adr_row(cell)?;
        let tone = |mode| -> Result<Option<Tint>, Error> {
            let color = Rgb::parse_hex(&self.variables(cell, mode).resolve("--focus")?)?;
            Ok(Some(Tint {
                color,
                alpha: row.tone,
            }))
        };
        match layer {
            Layer::None => Ok(None),
            Layer::Veil => Ok(Some(row.veil)),
            Layer::ToneNavigation => tone(Mode::Navigation),
            Layer::ToneInput => tone(Mode::Input),
        }
    }

    /// Ожидаемый цвет участка: заливка слота и слой над ней.
    pub fn expected(&self, cell: Cell, slot: &str, layer: Layer) -> Result<Rgb, Error> {
        let fill = self.slot(cell, slot)?;
        Ok(match self.layer(cell, layer)? {
            Some(tint) => blend(fill, tint),
            None => fill,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Result<T> = std::result::Result<T, Error>;

    const DARK: Cell = Cell::new(Appearance::Dark, Contrast::Normal);
    const LIGHT: Cell = Cell::new(Appearance::Light, Contrast::Normal);
    const DARK_IC: Cell = Cell::new(Appearance::Dark, Contrast::Increased);
    const LIGHT_IC: Cell = Cell::new(Appearance::Light, Contrast::Increased);

    #[test]
    fn slots_follow_the_css_cascade_of_each_cell() -> Result<()> {
        let palette = Palette::embedded()?;
        assert_eq!(palette.slot(DARK, "chrome")?, Rgb::new(0x1E, 0x1D, 0x1B));
        assert_eq!(palette.slot(LIGHT, "chrome")?, Rgb::new(0xEF, 0xED, 0xE8));
        assert_eq!(palette.slot(DARK, "text-3")?, Rgb::new(0x8F, 0x89, 0x7E));
        assert_eq!(palette.slot(DARK_IC, "text-3")?, Rgb::new(0xB3, 0xAD, 0xA1));
        assert_eq!(
            palette.slot(LIGHT_IC, "text-3")?,
            Rgb::new(0x4F, 0x4B, 0x44)
        );
        assert_eq!(palette.slot(LIGHT_IC, "line")?, Rgb::new(0x9A, 0x95, 0x8B));
        assert_eq!(
            palette.slot(LIGHT_IC, "surface")?,
            Rgb::new(0xFA, 0xF9, 0xF6)
        );
        assert_eq!(palette.variable(DARK_IC, "--focus-h")?, "3px");
        assert_eq!(palette.variable(LIGHT, "--focus-h")?, "2px");
        Ok(())
    }

    #[test]
    fn panel_veil_and_tone_come_from_the_adr_table() -> Result<()> {
        let palette = Palette::embedded()?;
        assert_eq!(
            palette.slot(LIGHT, "panel-head")?,
            Rgb::new(0xF8, 0xF7, 0xF3)
        );
        let veil = palette.layer(DARK_IC, Layer::Veil)?;
        assert_eq!(
            veil,
            Some(Tint {
                color: Rgb::new(0xFF, 0xFF, 0xFF),
                alpha: 0.07
            })
        );
        let tone = palette.layer(LIGHT_IC, Layer::ToneInput)?;
        assert_eq!(
            tone,
            Some(Tint {
                color: Rgb::new(0x4D, 0x75, 0x53),
                alpha: 0.04
            })
        );
        let tone = palette.layer(DARK, Layer::ToneNavigation)?;
        assert_eq!(
            tone,
            Some(Tint {
                color: Rgb::new(0x7F, 0x9B, 0xC0),
                alpha: 0.05
            })
        );
        assert_eq!(palette.layer(DARK, Layer::None)?, None);
        Ok(())
    }

    /// Числа посчитаны вручную по `tokens.css` и таблице ADR-0027, без кода палитры.
    #[test]
    fn expected_colors_of_the_gallery_regions_in_every_cell() -> Result<()> {
        let palette = Palette::embedded()?;
        let cases = [
            (DARK, "chrome", Layer::None, Rgb::new(30, 29, 27)),
            (DARK, "panel", Layer::ToneNavigation, Rgb::new(16, 16, 17)),
            (DARK, "panel", Layer::Veil, Rgb::new(28, 26, 24)),
            (DARK, "surface", Layer::ToneNavigation, Rgb::new(30, 31, 31)),
            (LIGHT, "chrome", Layer::None, Rgb::new(239, 237, 232)),
            (
                LIGHT,
                "panel",
                Layer::ToneNavigation,
                Rgb::new(251, 252, 253),
            ),
            (LIGHT, "panel", Layer::Veil, Rgb::new(244, 243, 242)),
            (
                LIGHT,
                "surface",
                Layer::ToneNavigation,
                Rgb::new(246, 246, 244),
            ),
            (DARK_IC, "chrome", Layer::None, Rgb::new(30, 29, 27)),
            (
                DARK_IC,
                "panel",
                Layer::ToneNavigation,
                Rgb::new(17, 18, 19),
            ),
            (DARK_IC, "panel", Layer::Veil, Rgb::new(27, 26, 25)),
            (
                DARK_IC,
                "surface",
                Layer::ToneNavigation,
                Rgb::new(31, 32, 32),
            ),
            (LIGHT_IC, "chrome", Layer::None, Rgb::new(239, 237, 232)),
            (
                LIGHT_IC,
                "panel",
                Layer::ToneNavigation,
                Rgb::new(248, 249, 250),
            ),
            (LIGHT_IC, "panel", Layer::Veil, Rgb::new(236, 236, 235)),
            (
                LIGHT_IC,
                "surface",
                Layer::ToneNavigation,
                Rgb::new(243, 243, 242),
            ),
        ];
        for (cell, slot, layer, expected) in cases {
            assert_eq!(
                palette.expected(cell, slot, layer)?,
                expected,
                "{cell}: {slot} {layer:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn blend_rounds_once_at_the_end() {
        let base = Rgb::new(10, 9, 8);
        let tint = Tint {
            color: Rgb::new(67, 64, 58),
            alpha: 0.31,
        };
        // 27.67, 26.05, 23.5
        assert_eq!(blend(base, tint), Rgb::new(28, 26, 24));
        let opaque = Tint {
            color: Rgb::new(1, 2, 3),
            alpha: 1.0,
        };
        assert_eq!(blend(base, opaque), Rgb::new(1, 2, 3));
        let clear = Tint {
            color: Rgb::new(255, 255, 255),
            alpha: 0.0,
        };
        assert_eq!(blend(base, clear), base);
    }

    #[test]
    fn unknown_slot_or_non_color_is_an_error() -> Result<()> {
        let palette = Palette::embedded()?;
        assert!(palette.slot(DARK, "nonexistent").is_err());
        assert!(palette.slot(DARK, "focus-h").is_err(), "не цвет");
        assert!(palette.slot(DARK, "--chrome").is_err(), "имя без --");
        assert!(palette.slot(DARK, "Chrome").is_err());
        assert!(palette.slot(DARK, "").is_err());
        Ok(())
    }

    #[test]
    fn css_parser_handles_comments_cascade_and_late_var() -> Result<()> {
        let css = r#"
            /* комментарий { не блок } */
            :root { --base: #111111; --link: var(--base); --chain: var(--link); --h: 2px; }
            :root[data-contrast="more"][data-theme="light"] { --h: 4px; }
            :root[data-theme="light"] { --base: #EEEEEE; }
            :root[data-contrast="more"] { --h: 3px; --base: #222222; }
            :root[data-motion="reduced"] * { --base: #FF0000; }
        "#;
        let blocks = parse_blocks(css)?;
        assert_eq!(blocks.len(), 4);
        let dark = Variables::cascade(&blocks, &[]);
        assert_eq!(dark.resolve("--chain")?, "#111111");
        let light = Variables::cascade(&blocks, &[("data-theme", "light")]);
        assert_eq!(light.resolve("--chain")?, "#EEEEEE");
        let light_more = Variables::cascade(
            &blocks,
            &[("data-theme", "light"), ("data-contrast", "more")],
        );
        assert_eq!(light_more.resolve("--h")?, "4px");
        assert_eq!(light_more.resolve("--base")?, "#222222");
        assert!(dark.resolve("--missing").is_err());
        let looped = parse_blocks(":root { --a: var(--b); --b: var(--a); }")?;
        assert!(Variables::cascade(&looped, &[]).resolve("--a").is_err());
        assert!(parse_blocks("body { --a: #000000; }").is_err());
        Ok(())
    }

    #[test]
    fn adr_table_must_keep_its_columns_and_cells() {
        let changed = "### Значения\n\n| Ячейка | `panel` | Вуаль |\n|---|---|---|\n";
        assert!(parse_adr_table(changed).is_err());
        let header = "### Значения\n\n| Ячейка | `panel` | `panel-head` | Вуаль | Тон режима |\n|---|---|---|---|---|\n";
        let one = format!("{header}| Тёмная | `#0A0908` | `#11100E` | `#43403A`, 31% | 5% |\n");
        assert!(parse_adr_table(&one).is_err(), "одна ячейка из четырёх");
        let bad_share =
            format!("{header}| Тёмная | `#0A0908` | `#11100E` | `#43403A`, 131% | 5% |\n");
        assert!(parse_adr_table(&bad_share).is_err());
    }

    #[test]
    fn hex_parsing_and_display_round_trip() -> Result<()> {
        let color = Rgb::parse_hex("#1E1D1B")?;
        assert_eq!(color, Rgb::new(30, 29, 27));
        assert_eq!(color.to_string(), "#1E1D1B");
        assert!(Rgb::parse_hex("#1E1D1").is_err());
        assert!(Rgb::parse_hex("1E1D1B").is_err());
        assert!(Rgb::parse_hex("#1E1D1G").is_err());
        assert_eq!(Rgb::new(10, 20, 30).max_delta(Rgb::new(13, 16, 30)), 4);
        Ok(())
    }
}
