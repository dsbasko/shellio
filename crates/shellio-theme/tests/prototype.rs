//! Сверка темы с `prototype/css/tokens.css` и таблицей значений ADR-0027.
//!
//! Разбор CSS свой: блоки `:root[…]` применяются по специфичности и порядку в файле, `var()`
//! разрешается после слияния ячейки. Источник каждого слота называет `AppTheme::slots`.

use std::collections::{BTreeMap, BTreeSet};

use shellio_theme::{
    Adr0027Column, AppTheme, Appearance, Cell, Contrast, CubicBezier, KeyMode, MONO_FAMILIES,
    Motion, MotionTokens, Origin, Srgb8, Tint, Value,
};

const TOKENS_CSS: &str = include_str!("../../../prototype/css/tokens.css");
const ADR_0027: &str = include_str!("../../../docs/adr/0027-focused-panel-contrast.md");

const EPSILON: f64 = 1e-9;

type Result<T> = std::result::Result<T, String>;

/// Блок правил: условия на атрибуты корня и объявления пользовательских свойств по порядку.
#[derive(Debug)]
struct RootBlock {
    attributes: Vec<(String, String)>,
    declarations: Vec<(String, String)>,
}

fn strip_comments(css: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        let end = rest[start + 2..]
            .find("*/")
            .ok_or("незакрытый комментарий")?;
        rest = &rest[start + 2 + end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Атрибуты селектора `:root[a="b"][c="d"]`. Селектор с потомком (`:root[…] *`) корень не
/// описывает и даёт `None`, любой другой вид — ошибка.
fn parse_root_selector(selector: &str) -> Result<Option<Vec<(String, String)>>> {
    let selector = selector.trim();
    if selector.contains(char::is_whitespace) {
        return Ok(None);
    }
    let mut rest = selector
        .strip_prefix(":root")
        .ok_or_else(|| format!("неизвестный селектор {selector:?}"))?;
    let mut attributes = Vec::new();
    while !rest.is_empty() {
        let body = rest
            .strip_prefix('[')
            .ok_or_else(|| format!("неизвестный селектор {selector:?}"))?;
        let end = body
            .find(']')
            .ok_or_else(|| format!("незакрытый атрибут в {selector:?}"))?;
        let (name, value) = body[..end]
            .split_once('=')
            .ok_or_else(|| format!("атрибут без значения в {selector:?}"))?;
        attributes.push((name.to_owned(), value.trim_matches('"').to_owned()));
        rest = &body[end + 1..];
    }
    Ok(Some(attributes))
}

fn parse_css(css: &str) -> Result<Vec<RootBlock>> {
    let css = strip_comments(css)?;
    let mut blocks = Vec::new();
    let mut rest = css.as_str();
    while let Some(open) = rest.find('{') {
        let selector = &rest[..open];
        let close = rest[open..]
            .find('}')
            .ok_or_else(|| format!("незакрытый блок {selector:?}"))?;
        let body = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];
        let Some(attributes) = parse_root_selector(selector)? else {
            continue;
        };
        let mut declarations = Vec::new();
        for declaration in body.split(';') {
            let declaration = declaration.trim();
            if declaration.is_empty() {
                continue;
            }
            let (name, value) = declaration
                .split_once(':')
                .ok_or_else(|| format!("объявление без значения {declaration:?}"))?;
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
        return Err(format!("лишний текст после блоков: {:?}", rest.trim()));
    }
    Ok(blocks)
}

/// Слияние блоков, которые подходят к атрибутам корня: по числу условий, затем по порядку в
/// файле. `var()` разрешается после слияния.
fn cascade(blocks: &[RootBlock], root: &[(&str, &str)]) -> BTreeMap<String, String> {
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
    merged
}

fn resolve(merged: &BTreeMap<String, String>, name: &str) -> Result<String> {
    let mut value = merged
        .get(name)
        .ok_or_else(|| format!("переменной {name} нет"))?
        .clone();
    for _ in 0..16 {
        let Some(start) = value.find("var(") else {
            return Ok(value);
        };
        let end = value[start..]
            .find(')')
            .ok_or_else(|| format!("незакрытый var() в {name}"))?;
        let inner = value[start + 4..start + end].trim();
        if inner.contains(',') {
            return Err(format!(
                "var() с запасным значением в {name} не разбирается"
            ));
        }
        let replacement = merged
            .get(inner)
            .ok_or_else(|| format!("{name} ссылается на отсутствующую {inner}"))?
            .clone();
        value.replace_range(start..start + end + 1, &replacement);
    }
    Err(format!("цикл var() в {name}"))
}

fn root_attributes(cell: Cell, mode: KeyMode, motion: Motion) -> Vec<(&'static str, &'static str)> {
    let mut root = Vec::new();
    if cell.appearance == Appearance::Light {
        root.push(("data-theme", "light"));
    }
    if cell.contrast == Contrast::Increased {
        root.push(("data-contrast", "more"));
    }
    if mode == KeyMode::Input {
        root.push(("data-mode", "input"));
    }
    if motion == Motion::Reduced {
        root.push(("data-motion", "reduced"));
    }
    root
}

fn parse_hex(text: &str) -> Result<Srgb8> {
    let digits = text
        .trim()
        .strip_prefix('#')
        .filter(|digits| digits.len() == 6)
        .ok_or_else(|| format!("не цвет #RRGGBB: {text:?}"))?;
    let value = u32::from_str_radix(digits, 16).map_err(|e| format!("{text:?}: {e}"))?;
    Ok(Srgb8::hex(value))
}

fn parse_number(text: &str) -> Result<f64> {
    text.trim()
        .parse::<f64>()
        .map_err(|e| format!("не число {text:?}: {e}"))
}

fn parse_unit(text: &str, unit: &str) -> Result<f64> {
    let number = text
        .trim()
        .strip_suffix(unit)
        .ok_or_else(|| format!("{text:?} не в {unit}"))?;
    parse_number(number)
}

fn parse_function<'a>(text: &'a str, function: &str) -> Result<Vec<&'a str>> {
    let inner = text
        .trim()
        .strip_prefix(function)
        .and_then(|rest| rest.strip_prefix('('))
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or_else(|| format!("не {function}(): {text:?}"))?;
    Ok(inner.split(',').map(str::trim).collect())
}

fn parse_rgba(text: &str) -> Result<Tint> {
    let parts = parse_function(text, "rgba")?;
    let [r, g, b, alpha] = parts.as_slice() else {
        return Err(format!("rgba() не из четырёх чисел: {text:?}"));
    };
    let channel = |part: &str| {
        part.parse::<u8>()
            .map_err(|e| format!("канал {part:?} в {text:?}: {e}"))
    };
    Ok(Tint::new(
        Srgb8::new(channel(r)?, channel(g)?, channel(b)?),
        parse_number(alpha)?,
    ))
}

/// Сравнение значения слота с текстом источника в его форме.
fn check_value(value: Value, text: &str) -> Result<()> {
    let matches = match value {
        Value::Color(color) => parse_hex(text)? == color,
        Value::Tint(tint) => {
            let parsed = parse_rgba(text)?;
            parsed.color == tint.color && (parsed.alpha - tint.alpha).abs() < EPSILON
        }
        Value::Alpha(alpha) => (parse_number(text)? - alpha).abs() < EPSILON,
        Value::Points(points) => (parse_unit(text, "px")? - points).abs() < EPSILON,
    };
    if matches {
        Ok(())
    } else {
        Err(format!("тема {value:?}, источник {text:?}"))
    }
}

/// Строка таблицы ADR-0027.
#[derive(Debug)]
struct AdrRow {
    panel: Srgb8,
    panel_head: Srgb8,
    veil: Tint,
    tone: f64,
}

fn parse_percent(text: &str) -> Result<f64> {
    Ok(parse_unit(text, "%")? / 100.0)
}

fn table_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().replace('`', ""))
        .collect()
}

fn adr_cell(name: &str) -> Result<Cell> {
    match name {
        "Тёмная" => Ok(Cell::new(Appearance::Dark, Contrast::Normal)),
        "Светлая" => Ok(Cell::new(Appearance::Light, Contrast::Normal)),
        "Тёмная, Increase Contrast" => Ok(Cell::new(Appearance::Dark, Contrast::Increased)),
        "Светлая, Increase Contrast" => {
            Ok(Cell::new(Appearance::Light, Contrast::Increased))
        }
        other => Err(format!("неизвестная ячейка ADR-0027 {other:?}")),
    }
}

fn parse_adr_table(adr: &str) -> Result<BTreeMap<&'static str, AdrRow>> {
    let section = adr
        .split("### Значения")
        .nth(1)
        .ok_or("в ADR-0027 нет раздела «Значения»")?;
    let mut lines = section
        .lines()
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'));
    let header = lines.next().ok_or("в ADR-0027 нет таблицы значений")?;
    let expected = ["Ячейка", "panel", "panel-head", "Вуаль", "Тон режима"];
    if table_cells(header) != expected {
        return Err(format!("столбцы таблицы ADR-0027 изменились: {header}"));
    }
    lines.next().ok_or("в таблице ADR-0027 нет разделителя")?;
    let mut rows = BTreeMap::new();
    for line in lines {
        let cells = table_cells(line);
        let [name, panel, panel_head, veil, tone] = cells.as_slice() else {
            return Err(format!("строка ADR-0027 не из пяти ячеек: {line}"));
        };
        let (veil_color, veil_alpha) = veil
            .split_once(',')
            .ok_or_else(|| format!("вуаль без доли: {veil:?}"))?;
        let row = AdrRow {
            panel: parse_hex(panel)?,
            panel_head: parse_hex(panel_head)?,
            veil: Tint::new(parse_hex(veil_color)?, parse_percent(veil_alpha)?),
            tone: parse_percent(tone)?,
        };
        let cell = adr_cell(name)?;
        if rows.insert(cell.name(), row).is_some() {
            return Err(format!("ячейка {name:?} в таблице ADR-0027 дважды"));
        }
    }
    if rows.len() != Cell::ALL.len() {
        return Err(format!("в таблице ADR-0027 {} ячеек", rows.len()));
    }
    Ok(rows)
}

fn check_adr(value: Value, column: Adr0027Column, row: &AdrRow) -> Result<()> {
    let matches = match (column, value) {
        (Adr0027Column::Panel, Value::Color(color)) => color == row.panel,
        (Adr0027Column::PanelHead, Value::Color(color)) => color == row.panel_head,
        (Adr0027Column::Veil, Value::Tint(tint)) => {
            tint.color == row.veil.color && (tint.alpha - row.veil.alpha).abs() < EPSILON
        }
        (Adr0027Column::Tone, Value::Alpha(alpha)) => (alpha - row.tone).abs() < EPSILON,
        (column, value) => {
            return Err(format!("значение {value:?} не того вида для {column:?}"));
        }
    };
    if matches {
        Ok(())
    } else {
        Err(format!("тема {value:?}, ADR-0027 {row:?}"))
    }
}

#[test]
fn every_slot_matches_its_source_in_every_cell() -> Result<()> {
    let blocks = parse_css(TOKENS_CSS)?;
    let table = parse_adr_table(ADR_0027)?;
    let mut failures = Vec::new();
    let mut checked = 0;
    for cell in Cell::ALL {
        let merged = cascade(
            &blocks,
            &root_attributes(cell, KeyMode::Navigation, Motion::Normal),
        );
        let row = table
            .get(cell.name())
            .ok_or_else(|| format!("нет строки ADR-0027 для {}", cell.name()))?;
        for slot in AppTheme::for_cell(cell).slots() {
            checked += 1;
            let outcome = match slot.origin {
                Origin::Css(variable) => {
                    resolve(&merged, variable).and_then(|text| check_value(slot.value, &text))
                }
                Origin::Adr0027(column) => check_adr(slot.value, column, row),
            };
            if let Err(reason) = outcome {
                failures.push(format!(
                    "{}: слот {} ({:?}): {reason}",
                    cell.name(),
                    slot.name,
                    slot.origin
                ));
            }
        }
    }
    assert!(checked > 0);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

#[test]
fn mode_color_matches_focus_variable() -> Result<()> {
    let blocks = parse_css(TOKENS_CSS)?;
    for cell in Cell::ALL {
        let theme = AppTheme::for_cell(cell);
        for mode in KeyMode::ALL {
            let merged = cascade(&blocks, &root_attributes(cell, mode, Motion::Normal));
            let focus = parse_hex(&resolve(&merged, "--focus")?)?;
            assert_eq!(
                theme.mode_color(mode),
                focus,
                "{} {mode:?}: --focus",
                cell.name()
            );
        }
    }
    Ok(())
}

#[test]
fn motion_matches_duration_and_easing() -> Result<()> {
    let blocks = parse_css(TOKENS_CSS)?;
    for motion in Motion::ALL {
        let MotionTokens {
            duration_ms,
            easing: CubicBezier { x1, y1, x2, y2 },
        } = AppTheme::motion(motion);
        for cell in Cell::ALL {
            let merged = cascade(&blocks, &root_attributes(cell, KeyMode::Navigation, motion));
            let duration = parse_unit(&resolve(&merged, "--dur")?, "ms")?;
            assert!(
                (duration - duration_ms).abs() < EPSILON,
                "{} {}: --dur {duration}, тема {duration_ms}",
                cell.name(),
                motion.name()
            );
            let ease = resolve(&merged, "--ease")?;
            let points = parse_function(&ease, "cubic-bezier")?
                .into_iter()
                .map(parse_number)
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(points.len(), 4, "--ease {ease:?}");
            for (css, theme) in points.iter().zip([x1, y1, x2, y2]) {
                assert!((css - theme).abs() < EPSILON, "--ease {ease:?}");
            }
        }
    }
    Ok(())
}

/// Прототип не заводит осей, которых нет в теме: новая ось — повод обновить `shellio-theme`.
#[test]
fn prototype_has_only_known_root_axes() -> Result<()> {
    let known: BTreeSet<(&str, &str)> = [
        ("data-theme", "light"),
        ("data-contrast", "more"),
        ("data-mode", "input"),
        ("data-motion", "reduced"),
    ]
    .into_iter()
    .collect();
    for block in parse_css(TOKENS_CSS)? {
        for (name, value) in &block.attributes {
            assert!(
                known.contains(&(name.as_str(), value.as_str())),
                "неизвестная ось прототипа [{name}={value:?}]"
            );
        }
    }
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
    let blocks = parse_css(css)?;
    assert_eq!(blocks.len(), 4);

    let dark = cascade(&blocks, &[]);
    assert_eq!(resolve(&dark, "--chain")?, "#111111");

    let light = cascade(&blocks, &[("data-theme", "light")]);
    assert_eq!(resolve(&light, "--chain")?, "#EEEEEE");

    let light_more = cascade(
        &blocks,
        &[("data-theme", "light"), ("data-contrast", "more")],
    );
    assert_eq!(resolve(&light_more, "--h")?, "4px");
    assert_eq!(resolve(&light_more, "--base")?, "#222222");

    let reduced = cascade(&blocks, &[("data-motion", "reduced")]);
    assert_eq!(resolve(&reduced, "--base")?, "#111111");

    assert!(resolve(&dark, "--missing").is_err());
    let looped = parse_css(":root { --a: var(--b); --b: var(--a); }")?;
    assert!(resolve(&cascade(&looped, &[]), "--a").is_err());
    assert!(parse_css("body { --a: #000000; }").is_err());
    Ok(())
}

#[test]
fn mono_families_match_the_prototype_stack() -> Result<()> {
    let line = TOKENS_CSS
        .lines()
        .find_map(|line| line.trim().strip_prefix("--mono:"))
        .ok_or("в tokens.css нет --mono")?;
    let stack: Vec<&str> = line
        .trim()
        .trim_end_matches(';')
        .split(',')
        .map(|family| family.trim().trim_matches('"'))
        .collect();
    let mut expected: Vec<&str> = MONO_FAMILIES.to_vec();
    expected.push("monospace");
    assert_eq!(stack, expected);
    Ok(())
}

#[test]
fn adr_table_parser_rejects_changed_columns() {
    let changed = "### Значения\n\n| Ячейка | `panel` | Вуаль |\n|---|---|---|\n";
    assert!(parse_adr_table(changed).is_err());
}

#[test]
fn value_check_reports_mismatch() {
    assert!(check_value(Value::Color(Srgb8::hex(0x1E1D1B)), "#1E1D1B").is_ok());
    assert!(check_value(Value::Color(Srgb8::hex(0x1E1D1B)), "#1E1D1C").is_err());
    assert!(check_value(Value::Points(2.0), "3px").is_err());
    assert!(
        check_value(
            Value::Tint(Tint::new(Srgb8::hex(0x43403A), 0.31)),
            "rgba(67,64,58,.31)"
        )
        .is_ok()
    );
    assert!(
        check_value(
            Value::Tint(Tint::new(Srgb8::hex(0x43403A), 0.31)),
            "rgba(67,64,58,.3)"
        )
        .is_err()
    );
}
