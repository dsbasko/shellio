//! Ядро режима сверки: паспорт репозитория против дерева продукта. Годное дерево продукта
//! строится зеркалом паспорта, отрицательные случаи — правкой годного дерева.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use shellio_conformance::fields::{Field, is_ring};
use shellio_conformance::passport::{Element, Passport};
use shellio_conformance::registry::Registry;
use shellio_e2e::conform::{
    BUNDLED_FONTS, Mismatch, Product, Report, SnapshotKind, Tolerances, judge,
};

const TOLERANCES: Tolerances = Tolerances {
    text_width: 1.25,
    mask_iou: 0.9,
};

const CELLS: [&str; 4] = ["light-normal", "light-high", "dark-normal", "dark-high"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn passport(state: &str, cell: &str) -> Passport {
    let path = root().join(format!("conformance/passports/{state}/{cell}.json"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("паспорт читается: {e}"));
    Passport::parse(&path.display().to_string(), &text)
        .unwrap_or_else(|e| panic!("паспорт разбирается: {e}"))
}

fn registry() -> Registry {
    let path = root().join("conformance/registry.toml");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("реестр читается: {e}"));
    Registry::parse("registry.toml", &text).unwrap_or_else(|e| panic!("реестр разбирается: {e}"))
}

fn hidden(passport: &Passport, element: &Element) -> bool {
    let mut current = Some(element);
    while let Some(e) = current {
        if !e.visible {
            return true;
        }
        current = e
            .parent
            .as_deref()
            .and_then(|p| passport.elements.iter().find(|x| x.ck == p));
    }
    false
}

fn style(e: &Element) -> Value {
    let mut style = serde_json::Map::new();
    let mut put = |name: &str, value: Value| {
        style.insert(name.to_owned(), value);
    };
    if let Some(font) = &e.font {
        let main = font
            .faces
            .iter()
            .max_by_key(|f| f.glyphs)
            .unwrap_or_else(|| panic!("шрифт"));
        let (family, face, file) = if main.custom {
            (
                font.family.clone(),
                main.family.clone(),
                format!("file:///A/shellio.app{BUNDLED_FONTS}{}.ttf", main.family),
            )
        } else {
            (
                ".AppleSystemUIFont".to_owned(),
                ".AppleSystemUIFont".to_owned(),
                "file:///System/Library/Fonts/SFNS.ttf".to_owned(),
            )
        };
        put(
            "font",
            json!({"family": family, "face": face, "weight": font.weight, "size": font.size, "file": file}),
        );
    }
    if let Some(line) = e.line_height.filter(|line| e.frame.h >= 2.0 * line) {
        put("line_height", json!(line));
    }
    for (name, color) in [("color", e.color), ("fill", e.fill)] {
        if let Some(color) = color {
            put(name, json!(color));
        }
    }
    let all = json!(["top", "right", "bottom", "left"]);
    // Системные кнопки окна AppKit рисует без слоя: линии у их узлов нет.
    let ring = e.shadow.iter().all(is_ring);
    if e.line_width.is_some_and(|w| w > 0.0) {
        put("line_color", json!(e.line_color));
        put("line_width", json!(e.line_width));
        put("line_sides", json!(e.line_sides));
    } else if !e.ck.starts_with("chrome.window.") && !e.shadow.is_empty() && ring {
        put("line_color", json!(e.shadow[0].color));
        put("line_width", json!(e.shadow[0].spread));
        put("line_sides", all);
    }
    if !e.shadow.is_empty() && !ring {
        put("shadow", json!(e.shadow));
    }
    if let Some(radius) = e.radius.filter(|r| *r > 0.0) {
        put("radius", json!(radius));
    }
    if let Some(icon) = &e.icon {
        put("icon", json!({"name": icon.name, "size": icon.size}));
    }
    Value::Object(style)
}

/// Дерево продукта, равное паспорту: по узлу на видимый элемент, кадры от нижнего края окна.
fn mirror(passport: &Passport) -> Value {
    let composition = &passport.composition;
    let mut menu = composition.menu.clone();
    let mut components = Vec::new();
    for e in &passport.elements {
        if e.ck.contains("::") || hidden(passport, e) {
            continue;
        }
        let mut state = Value::Null;
        for (zone, parts) in &composition.zones {
            for (i, part) in parts.iter().enumerate() {
                let key = format!("zone.{zone}.part.{}", i + 1);
                if e.ck == key {
                    let focused = composition
                        .focus
                        .is_some_and(|f| f.zone == *zone && f.part == i + 1);
                    state = json!({"part": {"focused": focused}});
                }
                if e.ck == format!("{key}.tabs") {
                    let tabs: Vec<Value> = part
                        .tabs
                        .iter()
                        .enumerate()
                        .map(|(t, kind)| json!({"id": format!("t{}", t + 1), "kind": kind}))
                        .collect();
                    let buttons: Vec<Value> = part.tools.iter().map(|b| json!({"id": b})).collect();
                    state = json!({"tab_strip": {
                        "selected": part.active.map(|a| format!("t{a}")),
                        "tabs": tabs,
                        "buttons": buttons,
                    }});
                }
            }
        }
        if (e.ck.ends_with(".tabs.menu") || e.ck == "picker")
            && state.is_null()
            && let Some(items) = menu.take()
        {
            let items: Vec<Value> = items
                .iter()
                .map(|i| json!({"title": i.title, "enabled": i.enabled}))
                .collect();
            state = json!({"menu": {"open": true, "items": items}});
        }
        components.push(json!({
            "id": e.ck,
            "class": "View",
            "parent": e.parent,
            "ck": e.ck,
            "frame": {
                "x": e.frame.x,
                "y": passport.window.height - e.frame.y - e.frame.h,
                "width": e.frame.w,
                "height": e.frame.h,
            },
            "visible": true,
            "label": e.text,
            "truncated": e.truncated,
            "style": style(e),
            "state": state,
        }));
    }
    json!({
        "content": {"width": passport.window.width, "height": passport.window.height},
        "components": components,
    })
}

fn node<'a>(tree: &'a mut Value, ck: &str) -> &'a mut Value {
    tree["components"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("узлы"))
        .iter_mut()
        .find(|n| n["ck"] == ck)
        .unwrap_or_else(|| panic!("нет узла {ck}"))
}

fn remove(tree: &mut Value, ck: &str) {
    let prefix = format!("{ck}.");
    tree["components"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("узлы"))
        .retain(|n| {
            let key = n["ck"].as_str().unwrap_or_default();
            key != ck && !key.starts_with(&prefix)
        });
}

fn run(state: &str, passport: &Passport, tree: &Value) -> Report {
    let product =
        Product::parse(&tree.to_string()).unwrap_or_else(|e| panic!("дерево продукта: {e}"));
    judge(passport, &product, &registry(), state, TOLERANCES)
        .unwrap_or_else(|e| panic!("сверка: {e}"))
}

fn fields(report: &Report, ck: &str) -> Vec<Field> {
    report
        .mismatches
        .iter()
        .filter(|m| m.ck == ck)
        .map(|m| m.field)
        .collect()
}

fn only(report: &Report, ck: &str, field: Field) -> Mismatch {
    assert_eq!(
        report.mismatches.len(),
        1,
        "ожидается одно расхождение: {:#?}",
        report.mismatches
    );
    let mismatch = report.mismatches[0].clone();
    assert_eq!((mismatch.ck.as_str(), mismatch.field), (ck, field));
    mismatch
}

#[test]
fn mirror_of_every_passport_has_no_mismatch() {
    for state in [
        "chrome-titlebar",
        "layout-default",
        "layout-all-zones",
        "layout-split-right",
        "layout-zoom",
        "layout-stub",
        "menu-panel",
        "picker-new-part",
    ] {
        for cell in CELLS {
            let passport = passport(state, cell);
            let report = run(state, &passport, &mirror(&passport));
            assert_eq!(report.mismatches, [], "{state}, {cell}");
        }
    }
}

#[test]
fn snapshot_stage_gets_pseudo_masks_and_system_buttons() {
    let passport = passport("layout-all-zones", "dark-normal");
    let report = run("layout-all-zones", &passport, &mirror(&passport));
    let kind = |ck: &str, field: Field| {
        report
            .snapshot
            .iter()
            .find(|c| c.ck == ck && c.field == field)
            .map(|c| c.kind)
    };
    assert_eq!(
        kind("zone.center.part.1::after", Field::Fill),
        Some(SnapshotKind::Region)
    );
    assert_eq!(
        kind("zone.center.part.1.tabs.tab.1::after", Field::Underline),
        Some(SnapshotKind::Underline)
    );
    assert_eq!(
        kind("chrome.search.icon", Field::IconMask),
        Some(SnapshotKind::Mask)
    );
    assert_eq!(
        kind("chrome.window.close", Field::Fill),
        Some(SnapshotKind::Region)
    );
    assert_eq!(
        kind("chrome.workspace.chevron", Field::Color),
        Some(SnapshotKind::IconColor)
    );
    // Значок вкладки демо-плагина: силуэт покрыт исключением реестра.
    let demo = "zone.center.part.1.tabs.tab.2.icon";
    assert!(passport.elements.iter().any(|e| e.ck == demo));
    assert_eq!(kind(demo, Field::IconMask), None);
}

#[test]
fn missing_expand_button_is_a_composition_mismatch() {
    let passport = passport("layout-stub", "dark-normal");
    let mut tree = mirror(&passport);
    remove(&mut tree, "zone.center.part.2.body.stub.expand");
    let report = run("layout-stub", &passport, &tree);
    assert_eq!(
        fields(&report, "zone.center.part.2.body.stub.expand"),
        [Field::Composition]
    );
}

#[test]
fn swapped_tabs_changed_active_tab_and_reordered_menu_are_composition() {
    let passport = passport("menu-panel", "dark-normal");
    let strip = "zone.center.part.1.tabs";
    let composition = |tree: &Value| {
        let report = run("menu-panel", &passport, tree);
        assert!(!report.mismatches.is_empty());
        assert!(
            report
                .mismatches
                .iter()
                .all(|m| m.field == Field::Composition && m.ck.is_empty()),
            "{:#?}",
            report.mismatches
        );
    };

    let mut tree = mirror(&passport);
    let tabs = node(&mut tree, strip)["state"]["tab_strip"]["tabs"]
        .as_array_mut()
        .expect("вкладки");
    assert!(tabs.len() >= 2 && tabs[0]["kind"] != tabs[1]["kind"]);
    let (first, second) = (tabs[0]["kind"].clone(), tabs[1]["kind"].clone());
    tabs[0]["kind"] = second;
    tabs[1]["kind"] = first;
    composition(&tree);

    let mut tree = mirror(&passport);
    node(&mut tree, strip)["state"]["tab_strip"]["selected"] = json!("t2");
    composition(&tree);

    let mut tree = mirror(&passport);
    let holder = tree["components"]
        .as_array_mut()
        .expect("узлы")
        .iter_mut()
        .find(|n| n["state"]["menu"]["open"] == true)
        .expect("открытое меню");
    let items = holder["state"]["menu"]["items"]
        .as_array_mut()
        .expect("пункты");
    items.swap(0, 1);
    composition(&tree);
}

#[test]
fn visible_button_without_key_is_a_composition_mismatch() {
    let passport = passport("chrome-titlebar", "dark-normal");
    let mut tree = mirror(&passport);
    node(&mut tree, "chrome.appearance")["ck"] = Value::Null;
    let report = run("chrome-titlebar", &passport, &tree);
    assert!(
        report
            .mismatches
            .iter()
            .any(|m| m.ck.is_empty() && m.field == Field::Composition)
    );
    assert_eq!(fields(&report, "chrome.appearance"), [Field::Composition]);
}

#[test]
fn mono_text_from_a_system_font_of_the_same_name_is_a_font_mismatch() {
    let passport = passport("chrome-titlebar", "dark-normal");
    let mut tree = mirror(&passport);
    node(&mut tree, "chrome.search.kbd")["style"]["font"]["file"] =
        json!("file:///Users/u/Library/Fonts/JetBrainsMono-Regular.ttf");
    only(
        &run("chrome-titlebar", &passport, &tree),
        "chrome.search.kbd",
        Field::Font,
    );

    // Системное семейство — одна явная пара имён, а не любое имя.
    let mut tree = mirror(&passport);
    node(&mut tree, "chrome.search.label")["style"]["font"]["family"] = json!("Helvetica");
    only(
        &run("chrome-titlebar", &passport, &tree),
        "chrome.search.label",
        Field::Font,
    );
}

#[test]
fn label_with_extra_padding_or_vertical_shift_is_a_frame_mismatch() {
    let passport = passport("layout-all-zones", "dark-normal");
    let label = "zone.center.part.1.tabs.tab.1.label";

    let mut tree = mirror(&passport);
    let x = node(&mut tree, label)["frame"]["x"].as_f64().expect("x");
    node(&mut tree, label)["frame"]["x"] = json!(x + 2.0);
    only(
        &run("layout-all-zones", &passport, &tree),
        label,
        Field::Frame,
    );

    let mut tree = mirror(&passport);
    let y = node(&mut tree, label)["frame"]["y"].as_f64().expect("y");
    node(&mut tree, label)["frame"]["y"] = json!(y + 2.0);
    only(
        &run("layout-all-zones", &passport, &tree),
        label,
        Field::Frame,
    );

    // Естественная высота набора вместо строки CSS с той же серединой проходит.
    let mut tree = mirror(&passport);
    let frame = &mut node(&mut tree, label)["frame"];
    let (y, h) = (
        frame["y"].as_f64().expect("y"),
        frame["height"].as_f64().expect("h"),
    );
    frame["y"] = json!(y + 1.5);
    frame["height"] = json!(h - 3.0);
    assert_eq!(run("layout-all-zones", &passport, &tree).mismatches, []);
}

#[test]
fn frame_tolerance_is_one_point_and_text_width_is_calibrated() {
    let passport = passport("chrome-titlebar", "dark-normal");
    let shifted = |ck: &str, side: &str, by: f64| {
        let mut tree = mirror(&passport);
        let value = node(&mut tree, ck)["frame"][side]
            .as_f64()
            .expect("сторона");
        node(&mut tree, ck)["frame"][side] = json!(value + by);
        run("chrome-titlebar", &passport, &tree).mismatches
    };
    assert_eq!(shifted("chrome.search", "x", 1.0), []);
    // Сдвиг поиска уводит и его детей: расхождение у каждого.
    assert!(
        shifted("chrome.search", "x", 1.01)
            .iter()
            .any(|m| m.ck == "chrome.search" && m.field == Field::Frame)
    );
    assert_eq!(shifted("chrome.search", "height", -1.0), []);
    assert!(!shifted("chrome.search", "height", -1.01).is_empty());
    // Ширина, заданная текстом: допуск калибровки вместо точки.
    assert_eq!(
        shifted("chrome.search.label", "width", TOLERANCES.text_width),
        []
    );
    assert!(!shifted("chrome.search.label", "width", TOLERANCES.text_width + 0.01).is_empty());
}

#[test]
fn color_radius_and_line_tolerances_hold_at_the_boundary() {
    let passport = passport("chrome-titlebar", "dark-normal");
    let with = |ck: &str, path: [&str; 2], value: Value| {
        let mut tree = mirror(&passport);
        node(&mut tree, ck)["style"][path[0]][path[1]] = value;
        run("chrome-titlebar", &passport, &tree).mismatches
    };
    let fill = passport
        .elements
        .iter()
        .find(|e| e.ck == "chrome.search")
        .and_then(|e| e.fill)
        .expect("заливка поиска");
    assert_eq!(with("chrome.search", ["fill", "r"], json!(fill.r + 3)), []);
    let over = with("chrome.search", ["fill", "r"], json!(fill.r + 4));
    assert_eq!(over.len(), 1);
    assert_eq!(
        (over[0].field, over[0].difference),
        (Field::Fill, Some(4.0))
    );

    let mut tree = mirror(&passport);
    let radius = node(&mut tree, "chrome.search")["style"]["radius"]
        .as_f64()
        .expect("радиус");
    node(&mut tree, "chrome.search")["style"]["radius"] = json!(radius + 0.5);
    only(
        &run("chrome-titlebar", &passport, &tree),
        "chrome.search",
        Field::Radius,
    );

    let mut tree = mirror(&passport);
    node(&mut tree, "chrome")["style"]["line_sides"] = json!(["top"]);
    only(
        &run("chrome-titlebar", &passport, &tree),
        "chrome",
        Field::LineWidth,
    );
}

#[test]
fn multiline_line_height_is_judged_within_half_a_point() {
    let passport = passport("layout-stub", "dark-normal");
    let label = "zone.center.part.2.body.stub.label";
    let line = passport
        .elements
        .iter()
        .find(|e| e.ck == label)
        .and_then(|e| e.line_height)
        .expect("интервал подписи заглушки");
    let with = |value: Value| {
        let mut tree = mirror(&passport);
        node(&mut tree, label)["style"]["line_height"] = value;
        run("layout-stub", &passport, &tree).mismatches
    };
    assert_eq!(with(json!(line + 0.5)), []);
    assert_eq!(with(json!(line + 0.6))[0].field, Field::LineHeight);
    // Обязательное значение, которого продукт не отдаёт, — «свойство не читается».
    let unread = with(Value::Null);
    assert_eq!(unread[0].field, Field::LineHeight);
    assert_eq!(unread[0].actual, "свойство не читается");
}

#[test]
fn required_font_without_value_is_unread() {
    let passport = passport("chrome-titlebar", "dark-normal");
    let mut tree = mirror(&passport);
    node(&mut tree, "chrome.search.label")["style"]["font"] = Value::Null;
    let mismatch = only(
        &run("chrome-titlebar", &passport, &tree),
        "chrome.search.label",
        Field::Font,
    );
    assert_eq!(mismatch.actual, "свойство не читается");
}

#[test]
fn element_without_pair_fails_on_either_side() {
    let passport = passport("chrome-titlebar", "dark-normal");
    let mut tree = mirror(&passport);
    remove(&mut tree, "chrome.search.icon");
    let mismatch = only(
        &run("chrome-titlebar", &passport, &tree),
        "chrome.search.icon",
        Field::Composition,
    );
    assert_eq!(mismatch.actual, "нет узла продукта");

    let mut tree = mirror(&passport);
    let mut extra = node(&mut tree, "chrome.search.icon").clone();
    extra["id"] = json!("extra");
    extra["ck"] = json!("chrome.search.clear");
    tree["components"].as_array_mut().expect("узлы").push(extra);
    let mismatch = only(
        &run("chrome-titlebar", &passport, &tree),
        "chrome.search.clear",
        Field::Composition,
    );
    assert_eq!(mismatch.expected, "нет элемента паспорта");
}

#[test]
fn invisible_passport_element_equals_absent_node() {
    let passport = passport("layout-stub", "dark-normal");
    let close = "zone.right.part.1.tabs.tab.1.close";
    let element = passport
        .elements
        .iter()
        .find(|e| e.ck == close)
        .expect("крестик");
    assert!(!element.visible);
    let mut tree = mirror(&passport);
    assert!(
        tree["components"]
            .as_array()
            .expect("узлы")
            .iter()
            .all(|n| n["ck"] != close)
    );

    let mut shown = node(&mut tree, "zone.right.part.1.tabs.tab.1").clone();
    shown["id"] = json!(close);
    shown["ck"] = json!(close);
    shown["parent"] = json!("zone.right.part.1.tabs.tab.1");
    shown["state"] = Value::Null;
    tree["components"].as_array_mut().expect("узлы").push(shown);
    only(&run("layout-stub", &passport, &tree), close, Field::Visible);
}

#[test]
fn frame_x_exception_covers_only_x_and_children_follow_the_parent() {
    let passport = passport("chrome-titlebar", "dark-normal");
    let shift = |tree: &mut Value, ck: &str, side: &str, by: f64| {
        let value = node(tree, ck)["frame"][side].as_f64().expect("сторона");
        node(tree, ck)["frame"][side] = json!(value + by);
    };

    // Кнопка воркспейса начинается на 3 точки левее вместе с детьми: исключение `frame_x`.
    let mut tree = mirror(&passport);
    for ck in [
        "chrome.workspace",
        "chrome.workspace.label",
        "chrome.workspace.chevron",
    ] {
        shift(&mut tree, ck, "x", -3.0);
    }
    assert_eq!(run("chrome-titlebar", &passport, &tree).mismatches, []);

    // Ребёнок сдвинут внутри кнопки: исключение его не покрывает.
    shift(&mut tree, "chrome.workspace.chevron", "x", 2.0);
    only(
        &run("chrome-titlebar", &passport, &tree),
        "chrome.workspace.chevron",
        Field::Frame,
    );

    // Другая сторона кадра самой кнопки исключением не снята.
    let mut tree = mirror(&passport);
    shift(&mut tree, "chrome.workspace", "y", 2.0);
    assert!(
        run("chrome-titlebar", &passport, &tree)
            .mismatches
            .iter()
            .any(|m| m.ck == "chrome.workspace" && m.field == Field::Frame)
    );
}

#[test]
fn icon_name_exception_keeps_the_size_judged() {
    let passport = passport("layout-all-zones", "dark-normal");
    let demo = "zone.center.part.1.tabs.tab.2.icon";
    let mut tree = mirror(&passport);
    node(&mut tree, demo)["style"]["icon"]["name"] = json!("list.bullet");
    assert_eq!(run("layout-all-zones", &passport, &tree).mismatches, []);

    let size = node(&mut tree, demo)["style"]["icon"]["size"]
        .as_f64()
        .expect("размер");
    node(&mut tree, demo)["style"]["icon"]["size"] = json!(size + 1.0);
    only(
        &run("layout-all-zones", &passport, &tree),
        demo,
        Field::Icon,
    );

    // Значок вне исключения: имя сверяется.
    let mut tree = mirror(&passport);
    node(&mut tree, "chrome.search.icon")["style"]["icon"]["name"] = json!("file");
    only(
        &run("layout-all-zones", &passport, &tree),
        "chrome.search.icon",
        Field::Icon,
    );
}

#[test]
fn button_title_is_left_to_the_snapshot_stage() {
    let passport = passport("chrome-titlebar", "dark-normal");
    let label = "chrome.workspace.label";
    let mut tree = mirror(&passport);
    let title = node(&mut tree, label);
    title["class"] = json!("ButtonTitle");
    // Место заголовка в ячейке шире набора и начинается раньше.
    let x = title["frame"]["x"].as_f64().expect("x");
    title["frame"]["x"] = json!(x - 4.0);
    let report = run("chrome-titlebar", &passport, &tree);
    assert_eq!(report.mismatches, []);
    assert!(
        report
            .snapshot
            .iter()
            .any(|c| c.ck == label && c.kind == SnapshotKind::ButtonTitleInk)
    );

    // Середина строки по вертикали судится и у подписи кнопки.
    let y = node(&mut tree, label)["frame"]["y"].as_f64().expect("y");
    node(&mut tree, label)["frame"]["y"] = json!(y + 2.0);
    only(
        &run("chrome-titlebar", &passport, &tree),
        label,
        Field::Frame,
    );
}
