//! Сверка набора значков продукта с таблицей `P` прототипа (`prototype/js/core/util.js`): по
//! имени и по фрагменту SVG, в обе стороны.

use std::collections::BTreeMap;

use shellio_theme::icons;

const UTIL_JS: &str = include_str!("../../../prototype/js/core/util.js");

type Result<T> = std::result::Result<T, String>;

/// Таблица `P`: строки вида `  имя: 'фрагмент',` между `const P = {` и `};`.
fn prototype_icons() -> Result<BTreeMap<String, String>> {
    let start = UTIL_JS
        .find("const P = {")
        .ok_or("в util.js нет таблицы P")?;
    let body = &UTIL_JS[start..];
    let end = body.find("\n};").ok_or("у таблицы P нет конца")?;
    let mut icons = BTreeMap::new();
    for line in body[..end].lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (name, rest) = line
            .split_once(": '")
            .ok_or_else(|| format!("строка таблицы P не разобрана: {line}"))?;
        let fragment = rest
            .strip_suffix("',")
            .or_else(|| rest.strip_suffix('\''))
            .ok_or_else(|| format!("фрагмент {name} не в одинарных кавычках"))?;
        if fragment.contains('\'') {
            return Err(format!("кавычка внутри фрагмента {name}"));
        }
        if icons.insert(name.to_owned(), fragment.to_owned()).is_some() {
            return Err(format!("значок {name} объявлен дважды"));
        }
    }
    Ok(icons)
}

#[test]
fn product_icons_equal_prototype_icons() -> Result<()> {
    let prototype = prototype_icons()?;
    assert!(prototype.len() >= 60, "таблица P разобрана не целиком");
    let product: BTreeMap<String, String> = icons::names()
        .filter_map(|name| Some((name.to_owned(), icons::fragment(name)?.to_owned())))
        .collect();
    assert_eq!(product.len(), icons::names().count());
    for (name, fragment) in &prototype {
        assert_eq!(
            product.get(name),
            Some(fragment),
            "значок {name} расходится с прототипом"
        );
    }
    for name in product.keys() {
        assert!(
            prototype.contains_key(name),
            "значка {name} нет в прототипе"
        );
    }
    Ok(())
}

#[test]
fn prototype_draws_icons_with_the_same_stroke_and_view_box() -> Result<()> {
    let call = UTIL_JS
        .lines()
        .find(|line| line.starts_with("S.icon = "))
        .ok_or("в util.js нет функции S.icon")?;
    assert!(call.contains(r#"viewBox="0 0 16 16""#));
    assert!(call.contains(r#"stroke-width="1.4""#));
    assert!(call.contains(r#"stroke-linecap="round""#));
    assert!(call.contains(r#"stroke-linejoin="round""#));
    assert!(call.contains("P[name] || P.file"));
    assert_eq!(icons::VIEW_BOX, 16.0);
    assert_eq!(icons::STROKE, 1.4);
    assert_eq!(icons::FALLBACK, "file");
    Ok(())
}

#[test]
fn every_prototype_icon_has_contours() -> Result<()> {
    for name in prototype_icons()?.keys() {
        assert!(
            icons::shapes(name).is_some_and(|shapes| !shapes.is_empty()),
            "значок {name} не разобран в контуры"
        );
    }
    Ok(())
}
