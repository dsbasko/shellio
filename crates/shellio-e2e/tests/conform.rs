//! Ядро режима сверки: паспорт репозитория против дерева продукта. Годное дерево продукта
//! строится зеркалом паспорта, отрицательные случаи — правкой годного дерева.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use shellio_conformance::fields::{Field, is_ring};
use shellio_conformance::passport::{Element, Passport};
use shellio_conformance::registry::Registry;
use shellio_e2e::conform::{
    BUNDLED_FONTS, Mismatch, Product, Report, Snapshot, SnapshotKind, Staged, Tolerances,
    free_strip, judge, snapshot_stage,
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
    // Кольцо кнопки окна: строка контракта «деталь системного вида», стадии снимка не отдаётся.
    assert_eq!(kind("chrome.window.close", Field::Shadow), None);
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

    // Факт обрезки подписи кнопки сверяется, как у поля.
    let mut cut = tree.clone();
    node(&mut cut, label)["truncated"] = json!(true);
    only(
        &run("chrome-titlebar", &passport, &cut),
        label,
        Field::Truncated,
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

#[test]
fn plugin_scope_narrows_a_key_exception() {
    // Значок вкладки терминала под ключи исключения значков `demo-*` подходит, под плагин — нет.
    let passport = passport("layout-all-zones", "dark-normal");
    let terminal = "zone.center.part.1.tabs.tab.1.icon";
    let mut tree = mirror(&passport);
    node(&mut tree, terminal)["style"]["icon"]["name"] = json!("list.bullet");
    let report = run("layout-all-zones", &passport, &tree);
    only(&report, terminal, Field::Icon);
    assert!(
        report
            .snapshot
            .iter()
            .any(|c| c.ck == terminal && c.kind == SnapshotKind::Mask)
    );
}

#[test]
fn badge_height_is_a_drawn_box_not_a_text_line() {
    let passport = passport("chrome-titlebar", "dark-normal");
    let badge = "chrome.search.kbd";
    let mut tree = mirror(&passport);
    let frame = &mut node(&mut tree, badge)["frame"];
    let (y, h) = (
        frame["y"].as_f64().expect("y"),
        frame["height"].as_f64().expect("h"),
    );
    // Выше на 4 точки с той же серединой.
    frame["y"] = json!(y - 2.0);
    frame["height"] = json!(h + 4.0);
    only(
        &run("chrome-titlebar", &passport, &tree),
        badge,
        Field::Frame,
    );
}

#[test]
fn body_exception_covers_content_without_pair_but_not_keys_outside_the_body() {
    let passport = passport("layout-all-zones", "dark-normal");
    let body = "zone.center.part.1.body";
    let extra = |ck: &str| {
        let mut tree = mirror(&passport);
        let mut card = node(&mut tree, body).clone();
        card["id"] = json!(ck);
        card["ck"] = json!(ck);
        card["parent"] = json!(body);
        tree["components"].as_array_mut().expect("узлы").push(card);
        run("layout-all-zones", &passport, &tree).mismatches
    };
    // Нейтральная карточка терминала: содержимое тела исключено вместе с составом.
    assert_eq!(extra("zone.center.part.1.body.card"), []);
    // Узел без пары вне тела — расхождение состава.
    let outside = extra("zone.center.part.1.tabs.extra");
    assert_eq!(outside.len(), 1);
    assert_eq!(outside[0].field, Field::Composition);
}

#[test]
fn calibration_gives_tolerances_and_rejects_bad_numbers() {
    let good = json!({"schema": 1, "text": {"tolerance_pt": 0.75}, "icons": {"threshold": 0.81}});
    assert_eq!(
        Tolerances::from_calibration(&good.to_string()).unwrap_or_else(|e| panic!("{e}")),
        Tolerances {
            text_width: 0.75,
            mask_iou: 0.81
        }
    );
    for bad in [
        json!({"schema": 2, "text": {"tolerance_pt": 0.75}, "icons": {"threshold": 0.81}}),
        json!({"schema": 1, "text": {}, "icons": {"threshold": 0.81}}),
        json!({"schema": 1, "text": {"tolerance_pt": 0.75}, "icons": {"threshold": 1.2}}),
        json!({"schema": 1, "text": {"tolerance_pt": -1.0}, "icons": {"threshold": 0.81}}),
    ] {
        assert!(
            Tolerances::from_calibration(&bad.to_string()).is_err(),
            "{bad}"
        );
    }
}

fn staged(passport: &Passport, tree: &Value, color: [u8; 3]) -> (Report, Staged) {
    staged_in("layout-all-zones", passport, tree, color)
}

fn staged_in(state: &str, passport: &Passport, tree: &Value, color: [u8; 3]) -> (Report, Staged) {
    let report = run(state, passport, tree);
    let product =
        Product::parse(&tree.to_string()).unwrap_or_else(|e| panic!("дерево продукта: {e}"));
    let pixel = move |_: u32, _: u32| Some(color);
    let mask = |ck: &str| Err(format!("нет маски {ck}"));
    let staged = snapshot_stage(
        passport,
        &product,
        &registry(),
        state,
        &report,
        TOLERANCES,
        &Snapshot {
            scale: 2.0,
            pixel: &pixel,
            mask: &mask,
        },
    );
    (report, staged)
}

#[test]
fn snapshot_stage_judges_opaque_fill_and_names_what_it_left() {
    let passport = passport("layout-all-zones", "dark-normal");
    let tree = mirror(&passport);
    let report = run("layout-all-zones", &passport, &tree);
    // Непрозрачная заливка участка, который кадр покрывает хотя бы одним целым пикселем.
    let (ck, fill) = report
        .snapshot
        .iter()
        .filter(|c| c.kind == SnapshotKind::Region && c.field == Field::Fill)
        .find_map(|c| {
            let e = passport.elements.iter().find(|e| e.ck == c.ck)?;
            let fill = e.fill.filter(|f| f.a >= 1.0)?;
            (e.frame.w >= 2.0 && e.frame.h >= 2.0).then(|| (c.ck.clone(), fill))
        })
        .unwrap_or_else(|| panic!("в паспорте есть непрозрачный участок"));
    let touches = |staged: &Staged| {
        (
            staged.mismatches.iter().any(|m| m.ck == ck),
            staged
                .unjudged
                .iter()
                .any(|u| u.ck == ck && u.field == Field::Fill),
        )
    };
    let (_, exact) = staged(&passport, &tree, [fill.r, fill.g, fill.b]);
    assert_eq!(touches(&exact), (false, false), "{ck}");
    let within = [fill.r.saturating_add(3), fill.g, fill.b];
    assert_eq!(touches(&staged(&passport, &tree, within).1), (false, false));
    let off = if fill.r > 127 { fill.r - 4 } else { fill.r + 4 };
    let (_, wrong) = staged(&passport, &tree, [off, fill.g, fill.b]);
    assert_eq!(touches(&wrong), (true, false), "{ck}");
    let mismatch = wrong
        .mismatches
        .iter()
        .find(|m| m.ck == ck)
        .unwrap_or_else(|| panic!("расхождение"));
    assert_eq!(
        (mismatch.field, mismatch.difference),
        (Field::Fill, Some(4.0))
    );
    // Цвет цветного значка судится: на одноцветном снимке шеврона нужного цвета нет.
    let chevron = "chrome.workspace.chevron";
    assert!(
        exact
            .mismatches
            .iter()
            .any(|m| m.ck == chevron && m.field == Field::Color)
    );
    assert!(
        exact
            .unjudged
            .iter()
            .all(|u| u.kind != SnapshotKind::IconColor)
    );
    // Линия под активной вкладкой судится: на одноцветном снимке её нет.
    let line = "zone.center.part.1.tabs.tab.1::after";
    assert!(
        exact
            .mismatches
            .iter()
            .any(|m| m.ck == line && m.field == Field::Underline)
    );
    assert!(
        exact
            .unjudged
            .iter()
            .all(|u| u.kind != SnapshotKind::Underline)
    );
    // Что стадия не судит, названо с причиной: маска без эталона.
    assert!(exact.unjudged.iter().any(|u| u.ck == "chrome.search.icon"
        && u.kind == SnapshotKind::Mask
        && !u.reason.is_empty()));
}

#[test]
fn veil_and_tone_are_judged_on_the_free_tab_strip_in_every_cell() {
    let state = "layout-all-zones";
    for cell in CELLS {
        let passport = passport(state, cell);
        let tree = mirror(&passport);
        let fill = |ck: &str| {
            passport
                .elements
                .iter()
                .find(|e| e.ck == ck)
                .and_then(|e| e.fill)
                .unwrap_or_else(|| panic!("{cell}: заливка {ck}"))
        };
        for part in ["zone.center.part.1", "zone.right.part.1"] {
            let veil = format!("{part}::after");
            let (base, tint) = (fill(&format!("{part}.tabs")), fill(&veil));
            let mix = |base: u8, over: u8| {
                (f64::from(base) + (f64::from(over) - f64::from(base)) * tint.a).round() as u8
            };
            let over = [
                mix(base.r, tint.r),
                mix(base.g, tint.g),
                mix(base.b, tint.b),
            ];
            let judged = |colour: [u8; 3]| {
                let (_, staged) = staged(&passport, &tree, colour);
                assert!(
                    staged.unjudged.iter().all(|u| u.ck != veil),
                    "{cell} {veil}: {:?}",
                    staged.unjudged
                );
                staged.mismatches.into_iter().find(|m| m.ck == veil)
            };
            assert_eq!(judged(over), None, "{cell} {veil}");
            // Плитка без вуали: виден цвет самой полосы — расхождение в каждой ячейке.
            let bare = judged([base.r, base.g, base.b])
                .unwrap_or_else(|| panic!("{cell} {veil}: отсутствие слоя не поймано"));
            assert_eq!(bare.field, Field::Fill);
            // Допуск канала на границе и за ней.
            let shift = |by: u8| {
                let r = if over[0] > 127 {
                    over[0] - by
                } else {
                    over[0] + by
                };
                [r, over[1], over[2]]
            };
            assert_eq!(judged(shift(3)), None, "{cell} {veil}");
            assert_eq!(
                judged(shift(4)).and_then(|m| m.difference),
                Some(4.0),
                "{cell} {veil}"
            );
        }
    }
    // Ни у полосы, ни у её вкладок нет читаемой заливки: суждения нет, поле названо.
    let passport = passport(state, "dark-normal");
    let mut tree = mirror(&passport);
    for key in ["zone.center.part.1.tabs", "zone.center.part.1.tabs.tab.1"] {
        node(&mut tree, key)["style"]["fill"] = Value::Null;
    }
    let (_, blind) = staged(&passport, &tree, [0, 0, 0]);
    assert!(
        blind
            .unjudged
            .iter()
            .any(|u| u.ck == "zone.center.part.1::after" && u.kind == SnapshotKind::Region)
    );
}

#[test]
fn veil_falls_back_to_a_tab_only_when_the_strip_has_no_gap() {
    let state = "layout-stub";
    let mut fallbacks = 0;
    for cell in CELLS {
        let passport = passport(state, cell);
        let tree = mirror(&passport);
        let product =
            Product::parse(&tree.to_string()).unwrap_or_else(|e| panic!("дерево продукта: {e}"));
        let opaque = |ck: &str| {
            let fill = product.keyed(ck)?.style.as_ref()?.fill?;
            (fill.a >= 1.0).then_some(fill)
        };
        let report = run(state, &passport, &tree);
        for check in report
            .snapshot
            .iter()
            .filter(|c| c.kind == SnapshotKind::Region && c.ck.contains(".part."))
        {
            let veil = check.ck.as_str();
            let part = veil.trim_end_matches("::after");
            let strip = format!("{part}.tabs");
            let node = product
                .keyed(&strip)
                .unwrap_or_else(|| panic!("{cell}: нет полосы {strip}"));
            let tint = passport
                .elements
                .iter()
                .find(|e| e.ck == veil)
                .and_then(|e| e.fill)
                .unwrap_or_else(|| panic!("{cell}: заливка {veil}"));
            // Основа: полоса со свободным промежутком, иначе первая вкладка с ним и с заливкой.
            let base = if free_strip(&product, node).is_ok() {
                opaque(&strip)
            } else {
                fallbacks += 1;
                (1..9).find_map(|n| {
                    let tab = format!("{strip}.tab.{n}");
                    let fill = opaque(&tab)?;
                    free_strip(&product, product.keyed(&tab)?).ok()?;
                    Some(fill)
                })
            }
            .unwrap_or_else(|| panic!("{cell} {veil}: нет основы"));
            let mix = |base: u8, over: u8| {
                (f64::from(base) + (f64::from(over) - f64::from(base)) * tint.a).round() as u8
            };
            let judged = |colour: [u8; 3]| {
                let (_, staged) = staged_in(state, &passport, &tree, colour);
                assert!(
                    staged.unjudged.iter().all(|u| u.ck != veil),
                    "{cell} {veil}: {:?}",
                    staged.unjudged
                );
                staged.mismatches.into_iter().find(|m| m.ck == veil)
            };
            let over = [
                mix(base.r, tint.r),
                mix(base.g, tint.g),
                mix(base.b, tint.b),
            ];
            assert_eq!(judged(over), None, "{cell} {veil}");
            // Плитка без слоя: виден цвет основы — расхождение и у запасной основы-вкладки.
            let bare = judged([base.r, base.g, base.b])
                .unwrap_or_else(|| panic!("{cell} {veil}: отсутствие слоя не поймано"));
            assert_eq!(bare.field, Field::Fill, "{cell} {veil}");
        }
    }
    assert!(fallbacks >= 4, "запасная основа есть в каждой ячейке");
}

/// Порядок основ вуали — полоса, затем вкладки — не зависит от порядка узлов `ui.inspect`: живое
/// окно отдаёт узлы в порядке построения видов, и вкладка может стоять раньше своей полосы.
#[test]
fn veil_base_is_the_strip_even_when_a_tab_node_comes_first() {
    let state = "layout-all-zones";
    let passport = passport(state, "dark-normal");
    let mut tree = mirror(&passport);
    let product =
        Product::parse(&tree.to_string()).unwrap_or_else(|e| panic!("дерево продукта: {e}"));
    let opaque = |ck: &str| {
        let fill = product.keyed(ck)?.style.as_ref()?.fill?;
        (fill.a >= 1.0).then_some(fill)
    };
    // Часть, у которой и полоса, и вкладка годятся основой, а заливки у них разные.
    let (part, strip_fill, tab, tab_fill) = ["zone.center.part.1", "zone.right.part.1"]
        .into_iter()
        .find_map(|part| {
            let strip = format!("{part}.tabs");
            let strip_fill = opaque(&strip)?;
            free_strip(&product, product.keyed(&strip)?).ok()?;
            (1..9).find_map(|n| {
                let tab = format!("{strip}.tab.{n}");
                let tab_fill = opaque(&tab)?;
                free_strip(&product, product.keyed(&tab)?).ok()?;
                (tab_fill != strip_fill).then_some((part, strip_fill, tab, tab_fill))
            })
        })
        .unwrap_or_else(|| panic!("нет части с двумя разными основами"));
    let veil = format!("{part}::after");
    let tint = passport
        .elements
        .iter()
        .find(|e| e.ck == veil)
        .and_then(|e| e.fill)
        .unwrap_or_else(|| panic!("заливка {veil}"));
    let over = |base: shellio_conformance::passport::Rgba| {
        let mix = |base: u8, over: u8| {
            (f64::from(base) + (f64::from(over) - f64::from(base)) * tint.a).round() as u8
        };
        [
            mix(base.r, tint.r),
            mix(base.g, tint.g),
            mix(base.b, tint.b),
        ]
    };
    assert_ne!(
        over(strip_fill),
        over(tab_fill),
        "основы различимы под вуалью"
    );
    // Узел вкладки переставлен в начало дерева, перед полосой.
    let nodes = tree["components"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("узлы"));
    let at = nodes
        .iter()
        .position(|n| n["ck"] == tab.as_str())
        .unwrap_or_else(|| panic!("нет узла {tab}"));
    let moved = nodes.remove(at);
    nodes.insert(0, moved);
    let judged = |colour: [u8; 3]| {
        let (_, staged) = staged(&passport, &tree, colour);
        assert!(
            staged.unjudged.iter().all(|u| u.ck != veil),
            "{veil}: {:?}",
            staged.unjudged
        );
        staged.mismatches.into_iter().find(|m| m.ck == veil)
    };
    assert_eq!(judged(over(strip_fill)), None, "основа — полоса");
    let wrong = judged(over(tab_fill))
        .unwrap_or_else(|| panic!("вуаль над заливкой вкладки принята за вуаль над полосой"));
    assert_eq!(wrong.field, Field::Fill);
}

#[test]
fn pseudo_element_of_an_absent_owner_is_left_to_the_owner_mismatch() {
    let state = "layout-empty-workbench";
    let veil = "zone.center.part.1::after";
    let part = "zone.center.part.1";
    for cell in CELLS {
        let passport = passport(state, cell);
        // Продукт без плитки пустой части (на её месте сообщение): запись даёт владелец.
        let mut tree = mirror(&passport);
        remove(&mut tree, part);
        let (report, staged) = staged_in(state, &passport, &tree, [255, 255, 255]);
        assert!(
            fields(&report, part).contains(&Field::Composition),
            "{cell}"
        );
        assert!(report.snapshot.iter().all(|c| c.ck != veil), "{cell}");
        assert!(staged.unjudged.iter().all(|u| u.ck != veil), "{cell}");
        assert!(staged.mismatches.iter().all(|m| m.ck != veil), "{cell}");
        // Плитка на месте: слой отдан стадии снимка, правило его не прячет.
        let (report, _) = staged_in(state, &passport, &mirror(&passport), [255, 255, 255]);
        assert!(
            report
                .snapshot
                .iter()
                .any(|c| c.ck == veil && c.kind == SnapshotKind::Region),
            "{cell}"
        );
    }
    // Вкладка пропала: линия под ней стадии не отдаётся, вкладку называет состав.
    let passport = passport("layout-all-zones", "dark-normal");
    let mut tree = mirror(&passport);
    let tab = "zone.center.part.1.tabs.tab.1";
    remove(&mut tree, tab);
    let (report, staged) = staged(&passport, &tree, [255, 255, 255]);
    let line = format!("{tab}::after");
    assert!(fields(&report, tab).contains(&Field::Composition));
    assert!(report.snapshot.iter().all(|c| c.ck != line));
    assert!(staged.unjudged.iter().all(|u| u.ck != line));
}

fn oracle(args: &[&str]) -> (Option<i32>, Value) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_shellio-e2e-oracle"))
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("оракул запускается: {e}"));
    let report = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    (output.status.code(), report)
}

/// Вызов `conform` на зеркале паспорта состояния и белом снимке окна: код, отчёт и пути входов
/// (паспорт, `ui.inspect`, `ui.window`, PNG).
fn conform_on_white(
    dir: &Path,
    state: &str,
    edit: impl Fn(&mut Value),
) -> (Option<i32>, Value, [String; 4]) {
    let passport_path = root().join(format!("conformance/passports/{state}/dark-normal.json"));
    let passport = passport(state, "dark-normal");
    let (width, height) = (passport.window.width, passport.window.height);
    let mut tree = mirror(&passport);
    tree["window_number"] = json!(7);
    tree["generation"] = json!(1);
    edit(&mut tree);
    let window = json!({"ok": {"ui": {"window": {
        "pid": 1, "mode": "normal", "window_number": 7,
        "frame": {"x": 0.0, "y": 0.0, "width": width, "height": height},
        "backing_scale_factor": 2.0, "visible": true, "key": true,
        "color_space": {"name": "sRGB IEC61966-2.1", "srgb": true},
        "title": "Основной", "appearance_choice": "dark",
        "lightness": {"value": "dark", "source": "choice"},
        "contrast": {"value": "normal", "source": "system"},
        "motion": {"value": "normal", "source": "system"},
        "screen": 0, "maximum_frames_per_second": 120, "screens": [],
        "mirror_revision": 1, "generation": 1
    }}}});
    let (w, h) = ((width * 2.0) as u32, (height * 2.0) as u32);
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        let mut writer = encoder
            .write_header()
            .unwrap_or_else(|e| panic!("png: {e}"));
        writer
            .write_image_data(&vec![255; (w * h * 4) as usize])
            .unwrap_or_else(|e| panic!("png: {e}"));
    }
    let file = |name: &str, bytes: &[u8]| {
        let path = dir.join(format!("{state}.{name}"));
        fs::write(&path, bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        path.display().to_string()
    };
    let inspect = file("inspect.json", tree.to_string().as_bytes());
    let window = file("window.json", window.to_string().as_bytes());
    let png = file("window.png", &png_bytes);
    let calibration = file(
        "calibration.json",
        json!({"schema": 1, "text": {"tolerance_pt": 1.25}, "icons": {"threshold": 0.9}})
            .to_string()
            .as_bytes(),
    );
    let registry = root()
        .join("conformance/registry.toml")
        .display()
        .to_string();
    let passport_arg = passport_path.display().to_string();
    let (code, report) = oracle(&[
        "conform",
        "--passport",
        &passport_arg,
        "--inspect",
        &inspect,
        "--window",
        &window,
        "--png",
        &png,
        "--registry",
        &registry,
        "--calibration",
        &calibration,
    ]);
    (code, report, [passport_arg, inspect, window, png])
}

/// Отрицательные случаи оракула подкомандой: правка годного дерева даёт код 3, а отчёт называет
/// поле, которого без правки в нём нет.
#[test]
fn conform_subcommand_names_the_field_of_every_negative_case() {
    type Edit = fn(&mut Value);
    let cases: [(&str, &str, &str, Edit); 7] = [
        (
            "layout-stub",
            "zone.center.part.2.body.stub.expand",
            "composition",
            |tree| remove(tree, "zone.center.part.2.body.stub.expand"),
        ),
        ("menu-panel", "", "composition", |tree| {
            let tabs = node(tree, "zone.center.part.1.tabs")["state"]["tab_strip"]["tabs"]
                .as_array_mut()
                .expect("вкладки");
            let (first, second) = (tabs[0]["kind"].clone(), tabs[1]["kind"].clone());
            tabs[0]["kind"] = second;
            tabs[1]["kind"] = first;
        }),
        ("menu-panel", "", "composition", |tree| {
            node(tree, "zone.center.part.1.tabs")["state"]["tab_strip"]["selected"] = json!("t2");
        }),
        ("menu-panel", "", "composition", |tree| {
            let holder = tree["components"]
                .as_array_mut()
                .expect("узлы")
                .iter_mut()
                .find(|n| n["state"]["menu"]["open"] == true)
                .expect("открытое меню");
            holder["state"]["menu"]["items"]
                .as_array_mut()
                .expect("пункты")
                .swap(0, 1);
        }),
        (
            "chrome-titlebar",
            "chrome.appearance",
            "composition",
            |tree| node(tree, "chrome.appearance")["ck"] = Value::Null,
        ),
        ("chrome-titlebar", "chrome.search.kbd", "font", |tree| {
            node(tree, "chrome.search.kbd")["style"]["font"]["file"] =
                json!("file:///Users/u/Library/Fonts/JetBrainsMono-Regular.ttf");
        }),
        (
            "layout-all-zones",
            "zone.center.part.1.tabs.tab.1.label",
            "frame",
            |tree| {
                let label = node(tree, "zone.center.part.1.tabs.tab.1.label");
                let x = label["frame"]["x"].as_f64().expect("x");
                label["frame"]["x"] = json!(x + 2.0);
            },
        ),
    ];
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("каталог: {e}"));
    let named = |report: &Value, ck: &str, field: &str| {
        report["mismatches"]
            .as_array()
            .is_some_and(|m| m.iter().any(|m| m["ck"] == ck && m["field"] == field))
    };
    for (index, (state, ck, field, edit)) in cases.into_iter().enumerate() {
        let (_, clean, _) = conform_on_white(dir.path(), state, |_| {});
        assert!(!named(&clean, ck, field), "случай {index}: {clean:#}");
        let (code, report, _) = conform_on_white(dir.path(), state, edit);
        assert_eq!(code, Some(3), "случай {index}: {report:#}");
        assert_eq!(report["unjudged"], json!([]), "случай {index}");
        assert!(named(&report, ck, field), "случай {index}: {report:#}");
    }
}

#[test]
fn conform_subcommand_gives_code_3_with_named_fields_and_code_1_while_a_field_is_unjudged() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("каталог: {e}"));
    // Заголовок: каждое поле снимка судится, белый снимок — расхождение с названными полями.
    let (code, report, [passport_arg, inspect, window, png]) =
        conform_on_white(dir.path(), "chrome-titlebar", |_| {});
    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["verdict"], "mismatch");
    assert_eq!(report["state"], "chrome-titlebar");
    assert_eq!(report["unjudged"], json!([]), "{report:#}");
    let named = |ck: &str, field: &str| {
        report["mismatches"]
            .as_array()
            .is_some_and(|m| m.iter().any(|m| m["ck"] == ck && m["field"] == field))
    };
    assert!(named("chrome.search.icon", "icon_mask"), "{report:#}");
    assert!(named("chrome.workspace.chevron", "color"), "{report:#}");
    assert!(named("chrome.window.close", "fill"), "{report:#}");
    // Кольцо кнопки окна не сверяется строкой контракта: ни расхождения, ни поля без суждения.
    assert!(!named("chrome.window.close", "shadow"), "{report:#}");

    // Раскладка: каждое поле снимка судится — код 3, названы линия под вкладкой и тон плитки.
    let (code, report, _) = conform_on_white(dir.path(), "layout-all-zones", |_| {});
    assert_eq!(code, Some(3), "{report:#}");
    assert_eq!(report["unjudged"], json!([]), "{report:#}");
    let named = |ck: &str, field: &str| {
        report["mismatches"]
            .as_array()
            .is_some_and(|m| m.iter().any(|m| m["ck"] == ck && m["field"] == field))
    };
    assert!(
        named("zone.center.part.1.tabs.tab.1::after", "underline"),
        "{report:#}"
    );
    assert!(named("zone.center.part.1::after", "fill"), "{report:#}");

    // Полоса вкладок и её вкладка без читаемой заливки: тон плитки судить не на чем — вердикта
    // нет, код 1.
    let (code, report, _) = conform_on_white(dir.path(), "layout-all-zones", |tree| {
        for key in ["zone.center.part.1.tabs", "zone.center.part.1.tabs.tab.1"] {
            node(tree, key)["style"]["fill"] = Value::Null;
        }
    });
    assert_eq!(code, Some(1), "{report:#}");
    assert_eq!(report["verdict"], "error");
    assert!(
        report["unjudged"].as_array().is_some_and(|u| u
            .iter()
            .any(|u| u["ck"] == "zone.center.part.1::after" && u["kind"] == "region")),
        "{report:#}"
    );
    // Неполный вызов и чужой флаг — код 2.
    let (code, _) = oracle(&["conform", "--passport", &passport_arg]);
    assert_eq!(code, Some(2));
    let (code, _) = oracle(&[
        "marker",
        "--passport",
        &passport_arg,
        "--inspect",
        &inspect,
        "--window",
        &window,
        "--png",
        &png,
    ]);
    assert_eq!(code, Some(2));
}

/// Запись живого окна сцены `p15` в ячейке `light-normal`: `ui.inspect`, `ui.window` и снимок окна
/// 1180×720 при масштабе 2 (путь бандла в записи заменён на `/fixture/`). Допуски — свои, тестовые:
/// калибровки они не заменяют.
fn recorded(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/conform")
        .join(name)
        .display()
        .to_string()
}

fn conform_recorded(state: &str, window: &str, extra: &[&str]) -> (Option<i32>, Value) {
    let passport = root()
        .join(format!("conformance/passports/{state}/light-normal.json"))
        .display()
        .to_string();
    let registry = root()
        .join("conformance/registry.toml")
        .display()
        .to_string();
    let (inspect, png, tolerances) = (
        recorded("p15-light-normal.inspect.json"),
        recorded("p15-light-normal.png"),
        recorded("tolerances.json"),
    );
    let mut args = vec![
        "conform",
        "--passport",
        &passport,
        "--inspect",
        &inspect,
        "--window",
        window,
        "--png",
        &png,
        "--registry",
        &registry,
        "--calibration",
        &tolerances,
    ];
    args.extend_from_slice(extra);
    oracle(&args)
}

/// Настоящее окно судится целиком: вердикт есть у каждого поля (код 0 или 3, не 1), а силуэты
/// значков заголовка с началом кадра в половине точки совпадают с масками паспорта.
#[test]
fn conform_subcommand_judges_the_recorded_p15_window() {
    let window = recorded("p15-light-normal.window.json");
    for state in ["chrome-titlebar", "chrome-statusline", "layout-all-zones"] {
        let (code, report) = conform_recorded(state, &window, &[]);
        assert!(matches!(code, Some(0 | 3)), "{state}: {code:?} {report:#}");
        assert_eq!(report["state"], state);
        assert_eq!(report["unjudged"], json!([]), "{state}: {report:#}");
        let masks: Vec<&Value> = report["mismatches"]
            .as_array()
            .map(|m| {
                m.iter()
                    .filter(|m| {
                        m["field"] == "icon_mask"
                            && m["ck"].as_str().is_some_and(|ck| ck.starts_with("chrome."))
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert!(masks.is_empty(), "{state}: {masks:#?}");
    }
}

/// `coverage` на записи окна `inspect`: код и отчёт.
fn coverage_recorded(inspect: &str) -> (Option<i32>, Value) {
    let passport = root()
        .join("conformance/passports/layout-all-zones/light-normal.json")
        .display()
        .to_string();
    let registry = root()
        .join("conformance/registry.toml")
        .display()
        .to_string();
    oracle(&[
        "coverage",
        "--passport",
        &passport,
        "--inspect",
        inspect,
        "--registry",
        &registry,
    ])
}

/// Пункт `ck-coverage`: на записи настоящего окна состав сцены найден и у каждого сверяемого узла
/// есть ключ из таблицы; калибровка и снимок подкоманде не нужны.
#[test]
fn coverage_subcommand_passes_the_recorded_p15_window() {
    let (code, report) = coverage_recorded(&recorded("p15-light-normal.inspect.json"));
    assert_eq!(code, Some(0), "{report:#}");
    assert_eq!(report["state"], "layout-all-zones");
    assert_eq!(report["mismatches"], json!([]));
}

/// Отрицательные записи `coverage`: видимая кнопка без ключа, ключ вне таблицы и повтор ключа —
/// код 3 с названным ключом.
#[test]
fn coverage_subcommand_names_a_lost_a_foreign_and_a_repeated_key() {
    const BUTTON: &str = "zone.center.part.1.tabs.zoom";
    const OTHER: &str = "zone.bottom.part.1.tabs.zoom";
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("каталог: {e}"));
    let path = recorded("p15-light-normal.inspect.json");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let cases: [(&str, Value, &str, &str); 3] = [
        ("lost", Value::Null, BUTTON, "нет узла продукта"),
        (
            "foreign",
            json!("zone.center.part.1.tabs.bogus"),
            "zone.center.part.1.tabs.bogus",
            "не соответствует таблице шаблонов",
        ),
        ("repeated", json!(OTHER), OTHER, "ключ повторяется"),
    ];
    for (name, ck, named, actual) in cases {
        let mut tree: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}"));
        let node = tree["ok"]["ui"]["inspect"]["components"]
            .as_array_mut()
            .and_then(|nodes| nodes.iter_mut().find(|node| node["ck"] == BUTTON))
            .unwrap_or_else(|| panic!("в записи нет узла {BUTTON}"));
        node["ck"] = ck;
        let file = dir.path().join(format!("{name}.inspect.json"));
        fs::write(&file, tree.to_string()).unwrap_or_else(|e| panic!("{e}"));
        let (code, report) = coverage_recorded(&file.display().to_string());
        assert_eq!(code, Some(3), "{name}: {report:#}");
        let hit = report["mismatches"].as_array().is_some_and(|rows| {
            rows.iter().any(|row| {
                row["ck"] == named
                    && row["field"] == "composition"
                    && row["actual"].as_str().is_some_and(|a| a.contains(actual))
            })
        });
        assert!(hit, "{name}: {report:#}");
    }
}

/// Режим без маркера у `conform`: пара одинаковых снимков одного поколения принимается, другое
/// поколение прежнего снимка — код 4 с повтором до срока и без повтора после него.
#[test]
fn conform_subcommand_accepts_a_settled_pair_and_rejects_another_generation() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("каталог: {e}"));
    let window = recorded("p15-light-normal.window.json");
    let png = recorded("p15-light-normal.png");
    let state = "chrome-statusline";
    let (alone, _) = conform_recorded(state, &window, &[]);
    let (code, report) = conform_recorded(
        state,
        &window,
        &[
            "--previous-window",
            &window,
            "--previous-png",
            &png,
            "--elapsed-ms",
            "0",
        ],
    );
    assert_eq!(code, alone, "{report:#}");
    assert_eq!(report["retry"], false, "{report:#}");

    let text = fs::read_to_string(&window).unwrap_or_else(|e| panic!("{window}: {e}"));
    let mut before: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}"));
    let generation = before["ok"]["ui"]["window"]["generation"]
        .as_u64()
        .unwrap_or_else(|| panic!("поколение записи"));
    before["ok"]["ui"]["window"]["generation"] = json!(generation - 1);
    let earlier = dir.path().join("earlier.window.json");
    fs::write(&earlier, before.to_string()).unwrap_or_else(|e| panic!("{e}"));
    let earlier = earlier.display().to_string();
    for (elapsed, retry) in [("0", true), ("60000", false)] {
        let (code, report) = conform_recorded(
            state,
            &window,
            &[
                "--previous-window",
                &earlier,
                "--previous-png",
                &png,
                "--elapsed-ms",
                elapsed,
            ],
        );
        assert_eq!(code, Some(4), "{report:#}");
        assert_eq!(report["verdict"], "generation", "{report:#}");
        assert_eq!(report["retry"], retry, "{report:#}");
        assert_eq!(report["mismatches"], json!([]));
    }
}

/// Пара снимков одного поколения, между которыми изменились пиксели окна, не принимается.
#[test]
fn conform_subcommand_rejects_a_pair_with_changed_pixels() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("каталог: {e}"));
    let window = recorded("p15-light-normal.window.json");
    let file = fs::File::open(recorded("p15-light-normal.png")).unwrap_or_else(|e| panic!("{e}"));
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .unwrap_or_else(|e| panic!("png: {e}"));
    let mut pixels = vec![
        0;
        reader
            .output_buffer_size()
            .unwrap_or_else(|| panic!("размер"))
    ];
    let info = reader
        .next_frame(&mut pixels)
        .unwrap_or_else(|e| panic!("png: {e}"));
    pixels.truncate(info.buffer_size());
    // Первые пиксели верхней строки инвертированы: разница канала больше допуска.
    for byte in pixels.iter_mut().take(64) {
        *byte = 255 - *byte;
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, info.width, info.height);
        encoder.set_color(info.color_type);
        encoder.set_depth(info.bit_depth);
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        let mut writer = encoder
            .write_header()
            .unwrap_or_else(|e| panic!("png: {e}"));
        writer
            .write_image_data(&pixels)
            .unwrap_or_else(|e| panic!("png: {e}"));
    }
    let changed = dir.path().join("changed.png");
    fs::write(&changed, bytes).unwrap_or_else(|e| panic!("{e}"));
    let changed = changed.display().to_string();
    let (code, report) = conform_recorded(
        "chrome-statusline",
        &window,
        &[
            "--previous-window",
            &window,
            "--previous-png",
            &changed,
            "--elapsed-ms",
            "0",
        ],
    );
    assert_eq!(code, Some(4), "{report:#}");
    assert_eq!(report["retry"], true, "{report:#}");
    assert!(
        report["reason"]
            .as_str()
            .is_some_and(|r| r.contains("изменилось пикселей")),
        "{report:#}"
    );
}
