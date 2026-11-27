//! Перевод фактов, которые вход `?conformance=1` прототипа отдаёт по CDP (`conformance.facts()`),
//! в элементы паспорта. Чистая часть съёмщика: тесты идут на записанных ответах CDP без браузера.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fields::ElementKind;
use crate::mask::{Mask, PixelClip, backdrop};
use crate::passport::{Element, Font, FontFace, Insets, Rect, Rgba, Shadow, Side};
use crate::recipe::Recipe;
use crate::registry::Surface;
use crate::scene::{Composition, Scene};

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
    let numbered = |marker: &str| {
        ck.rsplit_once(marker)
            .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    match tag {
        "svg" => ElementKind::Icon,
        "kbd" => ElementKind::Badge,
        "button" if numbered(".tab.") => ElementKind::Tab,
        "button" => ElementKind::Control,
        // Крестик вкладки — контрол, хотя прототип рисует его строчным элементом со значком.
        "span" if last == "close" => ElementKind::Control,
        // Подсказка строки статуса — группа из клавиш и подписи, а не текст.
        "span" if numbered(".hint.") => ElementKind::Container,
        // Плашка категории палитры — с заливкой и радиусом, как знак клавиши.
        "span" if ck == "palette.category" => ElementKind::Badge,
        // Метка `f` — плавающая плитка с заливкой, радиусом и тенью: свой текст она не несёт,
        // набранная часть и остаток — её дети-тексты.
        "span" if ck.starts_with("hints.item.") && numbered(".item.") => ElementKind::Plate,
        "span" | "em" | "strong" | "b" | "label" => ElementKind::Text,
        // Строка фильтра панели — контрол с подложкой, её поле — текст: значение или placeholder.
        "input" => ElementKind::Text,
        "div" if last == "filter" => ElementKind::Control,
        // Часть зоны — плитка с рамкой и радиусом; меню, выбор, палитра и which-key — плавающие плитки.
        "section" if numbered(".part.") => ElementKind::Plate,
        _ if last == "menu" || matches!(ck, "picker" | "palette" | "whichkey") => {
            ElementKind::Plate
        }
        "div" if ck == "palette.empty" => ElementKind::Text,
        // Подписи заглушки и пустой части прототип рисует блочными элементами.
        "div" if matches!(last, "label" | "title" | "hint") => ElementKind::Text,
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
/// Линия элемента: нарисованные стороны, их общий цвет и толщина. Сторона нарисована, если её
/// толщина больше нуля, стиль не `none` и цвет не прозрачен. Стороны разного цвета или толщины —
/// ошибка съёма: у контракта одна линия на элемент.
pub fn line(style: &Value) -> Result<Option<(Vec<Side>, Rgba, f64)>, String> {
    let mut sides = Vec::new();
    let mut drawn: Option<(Rgba, f64)> = None;
    for border in style
        .get("border")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let width = parse_px(str_of(border, "width")).unwrap_or(0.0);
        let Some(color) = parse_color(str_of(border, "color")).filter(|c| c.a > 0.0) else {
            continue;
        };
        if width <= 0.0 || matches!(str_of(border, "style"), "none" | "hidden" | "") {
            continue;
        }
        let side = match str_of(border, "side") {
            "top" => Side::Top,
            "right" => Side::Right,
            "bottom" => Side::Bottom,
            "left" => Side::Left,
            other => return Err(format!("сторона линии «{other}» неизвестна")),
        };
        match drawn {
            Some(first) if first != (color, width) => {
                return Err("стороны линии различаются цветом или толщиной".to_owned());
            }
            _ => drawn = Some((color, width)),
        }
        sides.push(side);
    }
    Ok(drawn.map(|(color, width)| (sides, color, width)))
}

/// Тени вычисленного `box-shadow`: записи через запятую вне скобок цвета, у каждой цвет, затем
/// смещения, размытие и растяжение в точках и необязательное `inset`. Прозрачная тень не пишется.
pub fn parse_shadows(text: &str) -> Result<Vec<Shadow>, String> {
    let text = text.trim();
    if text.is_empty() || text == "none" {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0_i32, 0_usize);
    for (index, ch) in text.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    let mut out = Vec::new();
    for part in parts {
        let part = part.trim();
        let bad = || format!("тень «{part}» не разобрана");
        let close = part.find(')').ok_or_else(bad)?;
        let color = parse_color(&part[..=close]).ok_or_else(bad)?;
        let mut numbers = Vec::new();
        let mut inset = false;
        for token in part[close + 1..].split_whitespace() {
            match token {
                "inset" => inset = true,
                px => numbers.push(parse_px(px).ok_or_else(bad)?),
            }
        }
        let [x, y, blur, spread] = numbers.as_slice() else {
            return Err(bad());
        };
        if color.a > 0.0 {
            out.push(Shadow {
                color,
                x: *x,
                y: *y,
                blur: *blur,
                spread: *spread,
                inset,
            });
        }
    }
    Ok(out)
}

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
        line_sides: Vec::new(),
        radius: None,
        padding: None,
        shadow: Vec::new(),
        icon: None,
    };
    let texty = matches!(
        kind,
        ElementKind::Text | ElementKind::Control | ElementKind::Tab | ElementKind::Badge
    );
    // Текст пишут подпись и плашка, а также контрол и вкладка с собственным текстом без
    // вложенных элементов.
    let own_text = fact
        .get("text")
        .and_then(Value::as_str)
        .filter(|t| match kind {
            ElementKind::Text | ElementKind::Badge => true,
            ElementKind::Control | ElementKind::Tab => !t.trim().is_empty(),
            ElementKind::Container
            | ElementKind::Icon
            | ElementKind::Plate
            | ElementKind::Pseudo => false,
        });
    if let Some(text) = own_text {
        out.text = Some(text.to_owned());
        out.truncated = Some(
            fact.get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
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
    if let Some((sides, color, width)) = line(&style).map_err(place)? {
        out.line_sides = sides;
        out.line_color = Some(color);
        out.line_width = Some(width);
    }
    if matches!(
        kind,
        ElementKind::Control | ElementKind::Plate | ElementKind::Badge
    ) {
        out.line_width = Some(out.line_width.unwrap_or(0.0));
        out.radius = Some(parse_px(str_of(&style, "radius")).unwrap_or(0.0));
    }
    out.shadow = parse_shadows(str_of(&style, "shadow")).map_err(place)?;
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

/// Шрифты, которыми Chrome набрал текст узла, по ответу `CSS.getPlatformFontsForNode`: имя
/// PostScript (иначе семейство), вложен ли файл (`isCustomFont`) и число глифов, по убыванию
/// глифов.
pub fn font_faces(reply: &Value) -> Result<Vec<FontFace>, String> {
    let list = reply
        .get("fonts")
        .and_then(Value::as_array)
        .ok_or("в ответе CSS.getPlatformFontsForNode нет списка fonts")?;
    let mut faces = list
        .iter()
        .map(|f| {
            let family = f
                .get("postScriptName")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .or_else(|| f.get("familyName").and_then(Value::as_str))
                .filter(|n| !n.is_empty())
                .ok_or("шрифт без имени")?;
            let glyphs = f
                .get("glyphCount")
                .and_then(Value::as_u64)
                .ok_or("шрифт без glyphCount")?;
            Ok(FontFace {
                family: family.to_owned(),
                custom: f
                    .get("isCustomFont")
                    .and_then(Value::as_bool)
                    .ok_or("шрифт без isCustomFont")?,
                glyphs: u32::try_from(glyphs).map_err(|e| e.to_string())?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    faces.sort_by(|a, b| {
        b.glyphs
            .cmp(&a.glyphs)
            .then_with(|| a.family.cmp(&b.family))
    });
    Ok(faces)
}

/// Псевдоэлемент узла с ключом: ответы CDP для него.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PseudoReply {
    /// Ключ узла-владельца.
    pub owner: String,
    /// `before` или `after`.
    pub pseudo: String,
    /// Ответ `CSS.getComputedStyleForNode`.
    pub style: Value,
    /// Ответ `DOM.getBoxModel`; `null` — у псевдоэлемента нет блока (`display: none`).
    pub box_model: Value,
}

/// Факт псевдоэлемента в форме фактов `conformance.facts()`: ключ `<владелец>::<before|after>`,
/// кадр по рамке блока, видимость и вычисленный стиль. Непрозрачность псевдоэлемента входит в
/// альфу заливки: вуаль фокуса задана заливкой и `opacity`.
pub fn pseudo_fact(reply: &PseudoReply) -> Result<Value, String> {
    if reply.pseudo != "before" && reply.pseudo != "after" {
        return Err(format!(
            "{}: псевдоэлемент «{}» не сверяется",
            reply.owner, reply.pseudo
        ));
    }
    let ck = format!("{}::{}", reply.owner, reply.pseudo);
    let css: BTreeMap<&str, &str> = reply
        .style
        .get("computedStyle")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{ck}: нет computedStyle"))?
        .iter()
        .filter_map(|p| Some((p.get("name")?.as_str()?, p.get("value")?.as_str()?)))
        .collect();
    let get = |name: &str| css.get(name).copied().unwrap_or_default();
    let frame = match reply.box_model.get("model").and_then(|m| m.get("border")) {
        Some(quad) => {
            let q: Vec<f64> = quad
                .as_array()
                .ok_or_else(|| format!("{ck}: рамка не список"))?
                .iter()
                .filter_map(Value::as_f64)
                .collect();
            if q.len() != 8 {
                return Err(format!("{ck}: в рамке {} чисел вместо 8", q.len()));
            }
            let xs = [q[0], q[2], q[4], q[6]];
            let ys = [q[1], q[3], q[5], q[7]];
            let min = |v: [f64; 4]| v.into_iter().fold(f64::INFINITY, f64::min);
            let max = |v: [f64; 4]| v.into_iter().fold(f64::NEG_INFINITY, f64::max);
            Rect {
                x: min(xs),
                y: min(ys),
                w: max(xs) - min(xs),
                h: max(ys) - min(ys),
            }
        }
        None if reply.box_model.is_null() => Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        },
        None => return Err(format!("{ck}: в ответе DOM.getBoxModel нет рамки")),
    };
    let opacity: f64 = get("opacity").parse().unwrap_or(1.0);
    let background = parse_color(get("background-color")).map(|c| {
        format!(
            "rgba({}, {}, {}, {})",
            c.r,
            c.g,
            c.b,
            (c.a * opacity * 1e6).round() / 1e6
        )
    });
    let visible = get("content") != "none"
        && get("display") != "none"
        && get("visibility") != "hidden"
        && frame.w > 0.0
        && frame.h > 0.0;
    let border: Vec<Value> = ["top", "right", "bottom", "left"]
        .iter()
        .map(|side| {
            json!({
                "side": side,
                "color": get(&format!("border-{side}-color")),
                "width": get(&format!("border-{side}-width")),
                "style": get(&format!("border-{side}-style")),
            })
        })
        .collect();
    Ok(json!({
        "ck": ck,
        "tag": "pseudo",
        "parent": reply.owner,
        "frame": {"x": frame.x, "y": frame.y, "w": frame.w, "h": frame.h},
        "visible": visible,
        "text": null,
        "truncated": false,
        "icon": null,
        "style": {
            "color": get("color"),
            "background": background.unwrap_or_default(),
            "border": border,
            "radius": get("border-top-left-radius"),
            "shadow": get("box-shadow"),
        },
    }))
}

/// Всё, что съёмщик получил от браузера для одного паспорта. Записывается целиком (`--record`),
/// поэтому сборка паспорта проверяется на записанных ответах без браузера.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    /// Плотность пикселей снимков значков.
    pub dpr: f64,
    /// Ответ `conformance.facts()`.
    pub facts: Value,
    /// Ответы `CSS.getPlatformFontsForNode` по ключу узла с текстом.
    pub fonts: BTreeMap<String, Value>,
    /// Псевдоэлементы узлов с ключом в порядке документа.
    pub pseudo: Vec<PseudoReply>,
    /// Снимки видимых значков: ключ → PNG в base64 из `Page.captureScreenshot` по области
    /// [`PixelClip`].
    pub icons: BTreeMap<String, String>,
}

/// Собранный паспорт без полей годности и окна: наблюдаемый состав, элементы и маски значков
/// (ключ → PNG маски).
#[derive(Debug, Clone, PartialEq)]
pub struct Assembled {
    pub composition: Composition,
    pub elements: Vec<Element>,
    pub masks: BTreeMap<String, Vec<u8>>,
}

/// Собирает паспорт из записанного съёма: постусловия рецепта, элементы с псевдоэлементами сразу
/// за владельцем, шрифты по ключу, маски видимых значков. Невыполненное постусловие, узел текста
/// без ответа о шрифтах, видимый значок без снимка или снимок не того размера — ошибка.
pub fn assemble(scene: &Scene, recipe: &Recipe, capture: &Capture) -> Result<Assembled, String> {
    assemble_surface(Surface::Layout, scene, recipe, capture)
}

/// Паспорт поверхности состояния: состав и постусловия сверяются по всему окну, а элементы,
/// псевдоэлементы и маски берутся только у ключей поверхности.
pub fn assemble_surface(
    surface: Surface,
    scene: &Scene,
    recipe: &Recipe,
    capture: &Capture,
) -> Result<Assembled, String> {
    let observed: Composition = serde_json::from_value(
        capture
            .facts
            .get("composition")
            .cloned()
            .ok_or("в фактах нет состава")?,
    )
    .map_err(|e| format!("состав: {e}"))?;
    let unmet = recipe.check_post(scene, &observed);
    if !unmet.is_empty() {
        return Err(format!(
            "постусловие рецепта не выполнено: {}",
            unmet.join("; ")
        ));
    }
    let base: Vec<Element> = elements(&capture.facts)?
        .into_iter()
        .filter(|e| surface.owns(&e.ck))
        .collect();
    let mut pseudo: BTreeMap<&str, Vec<Element>> = BTreeMap::new();
    let mut order = capture.pseudo.iter().collect::<Vec<_>>();
    order.sort_by_key(|p| p.pseudo != "before");
    for reply in order {
        if !surface.owns(&reply.owner) {
            continue;
        }
        if !base.iter().any(|e| e.ck == reply.owner) {
            return Err(format!(
                "псевдоэлемент узла «{}» без элемента-владельца",
                reply.owner
            ));
        }
        let element = element(&pseudo_fact(reply)?)?;
        pseudo
            .entry(reply.owner.as_str())
            .or_default()
            .push(element);
    }
    let mut out = Vec::new();
    for mut element in base {
        if let Some(font) = element.font.as_mut() {
            let reply = capture
                .fonts
                .get(&element.ck)
                .ok_or_else(|| format!("{}: нет ответа о шрифтах", element.ck))?;
            font.faces = font_faces(reply).map_err(|e| format!("{}: {e}", element.ck))?;
        }
        let owned = pseudo.remove(element.ck.as_str()).unwrap_or_default();
        out.push(element);
        out.extend(owned);
    }
    crate::keys::check_unique(out.iter().map(|e| e.ck.as_str()))?;
    let mut masks = BTreeMap::new();
    for icon in out
        .iter()
        .filter(|e| e.kind == ElementKind::Icon && e.visible)
    {
        let data = capture
            .icons
            .get(&icon.ck)
            .ok_or_else(|| format!("{}: нет снимка значка", icon.ck))?;
        let png = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| format!("{}: снимок не base64: {e}", icon.ck))?;
        let clip = PixelClip::of(&icon.frame, capture.dpr)?;
        let mask = Mask::from_png(&png, backdrop(&out, &icon.ck)?)?;
        if (mask.width, mask.height) != (clip.width, clip.height) {
            return Err(format!(
                "{}: снимок {}×{} вместо области {}×{}",
                icon.ck, mask.width, mask.height, clip.width, clip.height
            ));
        }
        masks.insert(icon.ck.clone(), mask.to_png()?);
    }
    if let Some(stray) = capture
        .icons
        .keys()
        .find(|k| surface.owns(k) && !masks.contains_key(*k))
    {
        return Err(format!("{stray}: снимок без видимого значка"));
    }
    Ok(Assembled {
        composition: observed,
        elements: out,
        masks,
    })
}
