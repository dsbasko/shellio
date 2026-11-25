//! Перевод фактов, которые вход `?conformance=1` прототипа отдаёт по CDP (`conformance.facts()`),
//! в элементы паспорта. Чистая часть съёмщика: тесты идут на записанных ответах CDP без браузера.

use serde_json::Value;

use crate::fields::ElementKind;
use crate::passport::{Element, Font, Insets, Rect, Rgba};

/// Длина CSS в пикселях (`12px`); `normal` и пустое значение — нет значения.
pub fn parse_px(text: &str) -> Option<f64> {
    text.trim().strip_suffix("px")?.trim().parse().ok()
}

/// Цвет вычисленного стиля Chrome: `rgb(r, g, b)` или `rgba(r, g, b, a)`.
pub fn parse_color(text: &str) -> Option<Rgba> {
    let text = text.trim();
    let inner = text
        .strip_prefix("rgba(")
        .or_else(|| text.strip_prefix("rgb("))?
        .strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    let channel = |s: &str| {
        s.parse::<f64>()
            .ok()
            .map(|v| v.round().clamp(0.0, 255.0) as u8)
    };
    match parts.as_slice() {
        [r, g, b] => Some(Rgba {
            r: channel(r)?,
            g: channel(g)?,
            b: channel(b)?,
            a: 1.0,
        }),
        [r, g, b, a] => Some(Rgba {
            r: channel(r)?,
            g: channel(g)?,
            b: channel(b)?,
            a: a.parse().ok()?,
        }),
        _ => None,
    }
}

/// Тип элемента по тегу и ключу.
pub fn kind_of(tag: &str, ck: &str) -> ElementKind {
    if ck.ends_with("::before") || ck.ends_with("::after") {
        return ElementKind::Pseudo;
    }
    let last = ck.rsplit('.').next().unwrap_or_default();
    let is_tab = ck
        .rsplit_once(".tab.")
        .is_some_and(|(_, n)| n.bytes().all(|b| b.is_ascii_digit()));
    match tag {
        "svg" => ElementKind::Icon,
        "kbd" => ElementKind::Badge,
        "button" if is_tab => ElementKind::Tab,
        "button" => ElementKind::Control,
        "span" | "em" | "strong" | "b" | "label" if last != "plate" => ElementKind::Text,
        _ => ElementKind::Container,
    }
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn num_of(value: &Value, key: &str) -> Result<f64, String> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("нет числа «{key}»"))
}

/// Первое семейство стека `font-family` без кавычек.
fn first_family(stack: &str) -> String {
    stack
        .split(',')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .to_owned()
}

/// Элемент паспорта по факту прототипа. Поля, которые контракт не применяет к типу, не пишутся.
pub fn element(fact: &Value) -> Result<Element, String> {
    let ck = str_of(fact, "ck").to_owned();
    if ck.is_empty() {
        return Err("факт без ключа".to_owned());
    }
    let place = |e: String| format!("{ck}: {e}");
    let kind = kind_of(str_of(fact, "tag"), &ck);
    let frame = fact.get("frame").ok_or_else(|| place("нет кадра".into()))?;
    let style = fact.get("style").cloned().unwrap_or(Value::Null);
    let mut out = Element {
        ck: ck.clone(),
        kind,
        parent: fact
            .get("parent")
            .and_then(Value::as_str)
            .map(str::to_owned),
        frame: Rect {
            x: num_of(frame, "x").map_err(place)?,
            y: num_of(frame, "y").map_err(place)?,
            w: num_of(frame, "w").map_err(place)?,
            h: num_of(frame, "h").map_err(place)?,
        },
        visible: fact
            .get("visible")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        text: None,
        truncated: None,
        font: None,
        line_height: None,
        letter_spacing: None,
        color: None,
        fill: None,
        line_color: None,
        line_width: None,
        radius: None,
        padding: None,
        shadow: None,
        icon: None,
    };
    let texty = matches!(
        kind,
        ElementKind::Text | ElementKind::Control | ElementKind::Tab | ElementKind::Badge
    );
    if matches!(kind, ElementKind::Text | ElementKind::Badge)
        && let Some(text) = fact.get("text").and_then(Value::as_str)
    {
        if kind == ElementKind::Text {
            out.text = Some(text.to_owned());
            out.truncated = Some(
                fact.get("truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
        }
        out.font = Some(Font {
            family: first_family(str_of(&style, "fontFamily")),
            weight: str_of(&style, "fontWeight").parse().unwrap_or(400.0),
            size: parse_px(str_of(&style, "fontSize")).unwrap_or(0.0),
            faces: Vec::new(),
        });
        out.line_height = parse_px(str_of(&style, "lineHeight"));
        out.letter_spacing = parse_px(str_of(&style, "letterSpacing"));
    }
    if texty || kind == ElementKind::Icon {
        out.color = parse_color(str_of(&style, "color"));
    }
    out.fill = parse_color(str_of(&style, "background")).filter(|c| c.a > 0.0);
    let border = parse_px(str_of(&style, "borderWidth")).unwrap_or(0.0);
    let drawn = border > 0.0 && str_of(&style, "borderStyle") != "none";
    if drawn {
        out.line_color = parse_color(str_of(&style, "borderColor"));
    }
    if matches!(
        kind,
        ElementKind::Control | ElementKind::Plate | ElementKind::Badge
    ) {
        out.line_width = Some(if drawn { border } else { 0.0 });
        out.radius = Some(parse_px(str_of(&style, "radius")).unwrap_or(0.0));
    }
    if matches!(kind, ElementKind::Control | ElementKind::Badge) {
        let pad: Vec<f64> = style
            .get("padding")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().and_then(parse_px).unwrap_or(0.0))
                    .collect()
            })
            .unwrap_or_default();
        if let [top, right, bottom, left] = pad.as_slice() {
            out.padding = Some(Insets {
                top: *top,
                right: *right,
                bottom: *bottom,
                left: *left,
            });
        }
    }
    if kind == ElementKind::Icon
        && let Some(icon) = fact.get("icon").filter(|v| !v.is_null())
    {
        out.icon = Some(crate::passport::Icon {
            name: str_of(icon, "name").to_owned(),
            size: num_of(icon, "size").map_err(place)?,
        });
    }
    crate::fields::check_element(&out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Элементы паспорта по всем фактам; ключи уникальны и соответствуют таблице.
pub fn elements(facts: &Value) -> Result<Vec<Element>, String> {
    let list = facts
        .get("elements")
        .and_then(Value::as_array)
        .ok_or("в фактах нет списка elements")?;
    let out = list.iter().map(element).collect::<Result<Vec<_>, _>>()?;
    crate::keys::check_unique(out.iter().map(|e| e.ck.as_str()))?;
    for e in &out {
        crate::keys::check_key(&e.ck)?;
    }
    Ok(out)
}
