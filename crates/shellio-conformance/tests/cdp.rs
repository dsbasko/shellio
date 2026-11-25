//! Сборка паспорта из записанных ответов CDP (`shellio-snap --record`): факты, шрифты,
//! псевдоэлементы, маски значков и постусловия рецепта без браузера.

use base64::Engine as _;
use serde_json::{Value, json};
use shellio_conformance::cdp::{
    Capture, PseudoReply, assemble, elements, font_faces, kind_of, parse_color, parse_px,
    pseudo_fact,
};
use shellio_conformance::fields::ElementKind;
use shellio_conformance::mask::{ICON_FG, Mask, PixelClip, backdrop, foreground};
use shellio_conformance::passport::{Rect, Rgba};
use shellio_conformance::recipe::Recipe;
use shellio_conformance::scene::Scene;

fn capture() -> Capture {
    let text = include_str!("fixtures/capture-chrome-titlebar-dark-normal.json");
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}"))
}

fn scene() -> Scene {
    Scene::parse("p15", include_str!("../../../conformance/scenes/p15.json"))
        .unwrap_or_else(|e| panic!("{e}"))
}

fn recipe() -> Recipe {
    Recipe::parse(
        "chrome-titlebar",
        include_str!("../../../conformance/recipes/chrome-titlebar.json"),
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

/// Захват с частью центра и её вуалью фокуса (`.panel::after`), записанный на копии прототипа с
/// ключом у части.
fn with_veil() -> Capture {
    let veil: Value =
        serde_json::from_str(include_str!("fixtures/pseudo-center-veil-light-high.json"))
            .unwrap_or_else(|e| panic!("{e}"));
    let mut capture = capture();
    capture.facts["elements"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("в фактах нет списка elements"))
        .push(veil["owner"].clone());
    capture.pseudo =
        serde_json::from_value(veil["pseudo"].clone()).unwrap_or_else(|e| panic!("{e}"));
    capture
}

#[test]
fn titlebar_capture_becomes_passport_elements() {
    let out = assemble(&scene(), &recipe(), &capture()).unwrap();
    assert_eq!(out.elements.len(), 20);
    let kbd = out
        .elements
        .iter()
        .find(|e| e.ck == "chrome.search.kbd")
        .unwrap();
    assert_eq!(kbd.kind, ElementKind::Badge);
    assert_eq!(kbd.text.as_deref(), Some("⌘K"));
    assert_eq!(kbd.truncated, Some(false));
    assert!(kbd.padding.is_some() && kbd.radius.is_some());
    let label = out
        .elements
        .iter()
        .find(|e| e.ck == "chrome.search.label")
        .unwrap();
    assert_eq!(label.text.as_deref(), Some("Сессии, файлы, команды"));
    let icon = out
        .elements
        .iter()
        .find(|e| e.ck == "chrome.search.icon")
        .unwrap();
    assert_eq!(icon.icon.as_ref().map(|i| i.name.as_str()), Some("search"));
    // Корень поверхности несёт заливку, которую видно под ним, а не прозрачную.
    let root = out.elements.iter().find(|e| e.ck == "chrome").unwrap();
    assert_eq!(root.fill.map(|f| f.a), Some(1.0));
    let workspace = out
        .elements
        .iter()
        .find(|e| e.ck == "chrome.workspace.label")
        .unwrap();
    assert_eq!(workspace.text.as_deref(), Some("Работа"));
}

#[test]
fn fonts_come_from_recorded_platform_fonts() {
    let capture = capture();
    let faces = font_faces(&capture.fonts["chrome.search.kbd"]).unwrap();
    assert_eq!(faces.len(), 1);
    assert_eq!(faces[0].family, "JetBrainsMono-Regular");
    assert_eq!(faces[0].glyphs, 2);
    // Оба знака «⌘K» нарисованы файлом `prototype/fonts/`, а не одноимённым шрифтом системы.
    assert!(faces[0].custom);
    let out = assemble(&scene(), &recipe(), &capture).unwrap();
    let kbd = out
        .elements
        .iter()
        .find(|e| e.ck == "chrome.search.kbd")
        .unwrap();
    assert_eq!(kbd.font.as_ref().unwrap().faces, faces);

    let sorted = font_faces(&json!({"fonts": [
        {"familyName": "A", "postScriptName": "", "isCustomFont": false, "glyphCount": 1},
        {"familyName": "B", "postScriptName": "B-Bold", "isCustomFont": true, "glyphCount": 5},
    ]}))
    .unwrap();
    assert_eq!(
        sorted.iter().map(|f| f.family.as_str()).collect::<Vec<_>>(),
        ["B-Bold", "A"]
    );
    assert!(sorted[0].custom);
    let partial = json!({"fonts": [{"familyName": "A", "glyphCount": 1}]});
    assert!(font_faces(&partial).unwrap_err().contains("isCustomFont"));
}

#[test]
fn text_without_font_reply_is_an_error() {
    let mut capture = capture();
    capture.fonts.remove("chrome.search.label");
    let err = assemble(&scene(), &recipe(), &capture).unwrap_err();
    assert!(
        err.contains("chrome.search.label: нет ответа о шрифтах"),
        "{err}"
    );
}

#[test]
fn pseudo_element_follows_its_owner() {
    let out = assemble(&scene(), &recipe(), &with_veil()).unwrap();
    let at = out
        .elements
        .iter()
        .position(|e| e.ck == "zone.center.part.1")
        .unwrap();
    let veil = &out.elements[at + 1];
    assert_eq!(veil.ck, "zone.center.part.1::after");
    assert_eq!(veil.kind, ElementKind::Pseudo);
    assert_eq!(veil.parent.as_deref(), Some("zone.center.part.1"));
    assert!(veil.visible);
    // Непрозрачность вуали входит в альфу заливки.
    let fill = veil.fill.unwrap();
    assert!((fill.a - 0.04).abs() < 1e-9, "{fill:?}");
    assert_eq!(veil.frame.w, out.elements[at].frame.w - 2.0);
}

#[test]
fn pseudo_without_box_is_invisible() {
    let veil = with_veil();
    let mut reply: PseudoReply = veil.pseudo[0].clone();
    reply.box_model = Value::Null;
    let fact = pseudo_fact(&reply).unwrap();
    assert_eq!(fact["visible"], false);
    reply.pseudo = "marker".to_owned();
    assert!(pseudo_fact(&reply).is_err());
}

#[test]
fn pseudo_without_owner_is_an_error() {
    let mut capture = capture();
    capture.pseudo = with_veil().pseudo;
    let err = assemble(&scene(), &recipe(), &capture).unwrap_err();
    assert!(err.contains("без элемента-владельца"), "{err}");
}

#[test]
fn icon_masks_follow_the_foreground_rule() {
    let capture = capture();
    let out = assemble(&scene(), &recipe(), &capture).unwrap();
    let visible_icons: Vec<&str> = out
        .elements
        .iter()
        .filter(|e| e.kind == ElementKind::Icon && e.visible)
        .map(|e| e.ck.as_str())
        .collect();
    assert_eq!(out.masks.keys().map(String::as_str).collect::<Vec<_>>(), {
        let mut v = visible_icons.clone();
        v.sort_unstable();
        v
    });
    let icon = out
        .elements
        .iter()
        .find(|e| e.ck == "chrome.search.icon")
        .unwrap();
    let clip = PixelClip::of(&icon.frame, capture.dpr).unwrap();
    let mask = Mask::read_png(&out.masks["chrome.search.icon"]).unwrap();
    assert_eq!((mask.width, mask.height), (clip.width, clip.height));
    let on = mask.on.iter().filter(|&&p| p).count();
    assert!(on > 20 && on < mask.on.len() / 2, "{on}");
    // Фон — заливка поля поиска: ближайший предок с ключом и непрозрачной заливкой.
    let search = out
        .elements
        .iter()
        .find(|e| e.ck == "chrome.search")
        .unwrap();
    assert_eq!(
        backdrop(&out.elements, "chrome.search.icon").unwrap(),
        search.fill.unwrap()
    );
    let shot = base64::engine::general_purpose::STANDARD
        .decode(&capture.icons["chrome.search.icon"])
        .unwrap();
    let again = Mask::from_png(&shot, search.fill.unwrap()).unwrap();
    assert_eq!(again, mask);
    assert_eq!(again.iou(&mask).unwrap(), 1.0);
}

#[test]
fn icon_without_screenshot_or_of_wrong_size_is_an_error() {
    let mut missing = capture();
    missing.icons.remove("chrome.appearance.icon");
    let err = assemble(&scene(), &recipe(), &missing).unwrap_err();
    assert!(err.contains("chrome.appearance.icon: нет снимка"), "{err}");

    let mut wrong = capture();
    let other = wrong.icons["chrome.workspace.chevron"].clone();
    wrong.icons.insert("chrome.search.icon".to_owned(), other);
    let err = assemble(&scene(), &recipe(), &wrong).unwrap_err();
    assert!(err.contains("вместо области"), "{err}");
}

#[test]
fn translucent_or_missing_backdrop_is_an_error() {
    let out = assemble(&scene(), &recipe(), &capture()).unwrap();
    let mut elements = out.elements.clone();
    let search = elements
        .iter_mut()
        .find(|e| e.ck == "chrome.search")
        .unwrap();
    search.fill = search.fill.map(|f| Rgba { a: 0.5, ..f });
    assert!(
        backdrop(&elements, "chrome.search.icon")
            .unwrap_err()
            .contains("полупрозрачная")
    );
    for e in &mut elements {
        e.fill = None;
    }
    assert!(
        backdrop(&elements, "chrome.search.icon")
            .unwrap_err()
            .contains("нет заливки")
    );
}

#[test]
fn foreground_threshold_and_pixel_grid() {
    let bg = Rgba {
        r: 100,
        g: 100,
        b: 100,
        a: 1.0,
    };
    let pixels = [
        100 + ICON_FG - 1,
        100,
        100,
        255,
        100,
        100 - ICON_FG,
        100,
        255,
    ];
    assert_eq!(foreground(&pixels, bg, ICON_FG), [false, true]);
    let clip = PixelClip::of(
        &Rect {
            x: 10.25,
            y: 3.5,
            w: 15.0,
            h: 15.0,
        },
        2.0,
    )
    .unwrap();
    assert_eq!((clip.x, clip.y, clip.width, clip.height), (20, 7, 31, 30));
    assert!(
        PixelClip::of(
            &Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 4.0
            },
            2.0
        )
        .is_err()
    );
}

#[test]
fn unmet_postcondition_is_an_error() {
    let mut capture = capture();
    capture.facts["composition"]["titlebar"]
        .as_array_mut()
        .unwrap()
        .retain(|c| c != "chrome.appearance");
    let err = assemble(&scene(), &recipe(), &capture).unwrap_err();
    assert!(err.contains("постусловие рецепта не выполнено"), "{err}");
    assert!(err.contains("контролы заголовка"), "{err}");

    let mut hidden = self::capture();
    hidden.facts["composition"]["zones"]
        .as_object_mut()
        .unwrap()
        .remove("left");
    let err = assemble(&scene(), &recipe(), &hidden).unwrap_err();
    assert!(err.contains("зона left: ожидается видимой"), "{err}");
}

#[test]
fn duplicate_key_is_rejected() {
    let mut f = capture().facts;
    let list = f["elements"].as_array_mut().unwrap();
    let copy = list[1].clone();
    list.push(copy);
    assert!(elements(&f).unwrap_err().contains("повторяется"));
}

#[test]
fn css_values_parse() {
    assert_eq!(parse_px("12.5px"), Some(12.5));
    assert_eq!(parse_px("normal"), None);
    let c = parse_color("rgba(1, 2, 3, 0.5)").unwrap();
    assert_eq!((c.r, c.g, c.b, c.a), (1, 2, 3, 0.5));
    assert_eq!(parse_color("rgb(255, 0, 10)").unwrap().a, 1.0);
    assert_eq!(kind_of("div", "chrome::before"), ElementKind::Pseudo);
    assert_eq!(
        kind_of("button", "zone.center.part.1.tabs.tab.2"),
        ElementKind::Tab
    );
}
