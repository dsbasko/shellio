//! Паспорт состояния прототипа и его каноническая запись.
//!
//! Каноническая запись — JSON с ключами объектов по алфавиту, отступом в два пробела и переводом
//! строки в конце. Дробные числа пишутся с шестью знаками после запятой, отрицательный ноль — как
//! ноль. Целые числа JSON (номер схемы, число глифов) остаются целыми: поля геометрии и стиля в
//! паспорте имеют тип `f64` и потому всегда записываются дробными, так что запись не зависит от
//! того, отдал ли источник `12` или `12.0`.

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Error;
use crate::fields::ElementKind;
use crate::scene::Composition;

/// Тема ячейки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Light,
    Dark,
}

/// Контраст ячейки: обычный или Increase Contrast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Contrast {
    Normal,
    High,
}

/// Ячейка: тема и контраст. Имя ячейки — имя файла паспорта, `<тема>-<контраст>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cell {
    pub theme: Theme,
    pub contrast: Contrast,
}

impl Cell {
    pub const ALL: [Cell; 4] = [
        Cell::new(Theme::Light, Contrast::Normal),
        Cell::new(Theme::Light, Contrast::High),
        Cell::new(Theme::Dark, Contrast::Normal),
        Cell::new(Theme::Dark, Contrast::High),
    ];

    pub const fn new(theme: Theme, contrast: Contrast) -> Self {
        Self { theme, contrast }
    }

    pub fn name(self) -> &'static str {
        match (self.theme, self.contrast) {
            (Theme::Light, Contrast::Normal) => "light-normal",
            (Theme::Light, Contrast::High) => "light-high",
            (Theme::Dark, Contrast::Normal) => "dark-normal",
            (Theme::Dark, Contrast::High) => "dark-high",
        }
    }
}

impl fmt::Display for Cell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Cell {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Cell::ALL
            .into_iter()
            .find(|cell| cell.name() == s)
            .ok_or_else(|| {
                Error::new(
                    "ячейка",
                    format!("неизвестная ячейка «{s}», ожидается light|dark-normal|high"),
                )
            })
    }
}

impl Serialize for Cell {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Cell {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}

/// Поля годности паспорта (ADR-0037, «Паспорт и годность»).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Validity {
    /// SHA-256 по отсортированным парам «относительный путь, байты» файлов `prototype/`.
    pub prototype: String,
    /// SHA-256 канонической записи сцены.
    pub scene: String,
    /// SHA-256 канонической записи рецепта.
    pub recipe: String,
    pub cell: Cell,
    /// SHA-256 файла `scripts/conformance/pin`.
    pub pin: String,
    /// Номер схемы паспорта и съёмщика.
    pub schema: u32,
}

/// Окно съёма: размер в точках и плотность пикселей.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub width: f64,
    pub height: f64,
    pub dpr: f64,
}

/// Кадр в точках окна.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Цвет sRGB с непрозрачностью 0–1.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f64,
}

/// Внутренние отступы в точках.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Insets {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

/// Шрифт, фактически выбранный для части глифов текста элемента.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FontFace {
    pub family: String,
    /// `true` — вложенный файл (`@font-face` у прототипа, файл бандла у продукта).
    pub custom: bool,
    pub glyphs: u32,
}

/// Шрифт текста элемента.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Font {
    pub family: String,
    pub weight: f64,
    pub size: f64,
    /// Все шрифты, которыми набран текст, с числом глифов.
    pub faces: Vec<FontFace>,
}

/// Тень элемента.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shadow {
    pub color: Rgba,
    pub x: f64,
    pub y: f64,
    pub blur: f64,
}

/// Значок набора `P`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Icon {
    pub name: String,
    pub size: f64,
}

/// Элемент паспорта. Отсутствующее поле — «не применяется» к типу элемента или не задано в
/// прототипе; что обязательно у какого типа, решает контракт полей ([`crate::fields`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Element {
    pub ck: String,
    pub kind: ElementKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub frame: Rect,
    pub visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<Font>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_height: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub letter_spacing: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgba>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Rgba>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_color: Option<Rgba>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding: Option<Insets>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<Shadow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<Icon>,
}

/// Паспорт состояния в одной ячейке.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Passport {
    pub validity: Validity,
    pub window: Window,
    pub composition: Composition,
    pub elements: Vec<Element>,
}

impl Passport {
    /// Разбирает паспорт и проверяет его внутренние правила: ключи и контракт полей.
    pub fn parse(place: &str, text: &str) -> Result<Self, Error> {
        let passport: Passport =
            serde_json::from_str(text).map_err(|e| Error::new(place, e.to_string()))?;
        passport.check().map_err(|e| Error::new(place, e))?;
        Ok(passport)
    }

    /// Каноническая запись паспорта.
    pub fn to_canonical(&self) -> Result<String, Error> {
        let value = serde_json::to_value(self).map_err(|e| Error::new("паспорт", e.to_string()))?;
        canonical_json(&value)
    }

    /// Ключи по таблице шаблонов, уникальность ключей, родитель среди элементов и контракт полей.
    pub fn check(&self) -> Result<(), String> {
        crate::keys::check_unique(self.elements.iter().map(|e| e.ck.as_str()))?;
        let keys: BTreeMap<&str, ()> = self.elements.iter().map(|e| (e.ck.as_str(), ())).collect();
        for element in &self.elements {
            crate::keys::check_key(&element.ck)?;
            if let Some(parent) = &element.parent
                && !keys.contains_key(parent.as_str())
            {
                return Err(format!(
                    "{}: родитель «{parent}» не найден среди элементов паспорта",
                    element.ck
                ));
            }
            crate::fields::check_element(element)?;
        }
        Ok(())
    }
}

/// Каноническая запись значения JSON (описание формата — в документации модуля).
pub fn canonical_json(value: &Value) -> Result<String, Error> {
    let mut out = String::new();
    write_value(&mut out, value, 0)?;
    out.push('\n');
    Ok(out)
}

fn write_value(out: &mut String, value: &Value, depth: usize) -> Result<(), Error> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(out, n)?,
        Value::String(s) => write_string(out, s)?,
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return Ok(());
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                write_value(out, item, depth + 1)?;
            }
            newline(out, depth);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                write_string(out, key)?;
                out.push_str(": ");
                if let Some(item) = map.get(key) {
                    write_value(out, item, depth + 1)?;
                }
            }
            newline(out, depth);
            out.push('}');
        }
    }
    Ok(())
}

fn newline(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_string(out: &mut String, s: &str) -> Result<(), Error> {
    let quoted = serde_json::to_string(s).map_err(|e| Error::new("строка", e.to_string()))?;
    out.push_str(&quoted);
    Ok(())
}

fn write_number(out: &mut String, n: &serde_json::Number) -> Result<(), Error> {
    if let Some(i) = n.as_i64() {
        let _ = write!(out, "{i}");
    } else if let Some(u) = n.as_u64() {
        let _ = write!(out, "{u}");
    } else {
        let f = n
            .as_f64()
            .filter(|f| f.is_finite())
            .ok_or_else(|| Error::new("число", format!("{n} не представимо")))?;
        let text = format!("{f:.6}");
        // Отрицательный ноль после округления пишется как ноль: запись не зависит от знака нуля.
        if text
            .trim_start_matches('-')
            .bytes()
            .all(|b| b == b'0' || b == b'.')
        {
            out.push_str("0.000000");
        } else {
            out.push_str(&text);
        }
    }
    Ok(())
}
