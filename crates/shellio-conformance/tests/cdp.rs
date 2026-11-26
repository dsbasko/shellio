//! Сборка паспорта из записанных ответов CDP (`shellio-snap --record`): факты, шрифты,
//! псевдоэлементы, маски значков и постусловия рецепта без браузера.

use base64::Engine as _;
use serde_json::{Value, json};
use shellio_conformance::cdp::{
    Capture, PseudoReply, assemble, assemble_surface, elements, font_faces, kind_of, line,
    parse_color, parse_px, parse_shadows, pseudo_fact,
};
use shellio_conformance::fields::ElementKind;
use shellio_conformance::mask::{
    ICON_FG, Mask, PixelClip, backdrop, crop_png, decode_rgba, foreground,
};
use shellio_conformance::passport::{Rect, Rgba, Side};
use shellio_conformance::recipe::Recipe;
use shellio_conformance::registry::Surface;
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

/// Захват всего окна: в нём есть часть центра с вуалью фокуса (`.panel::after`).
fn with_veil() -> Capture {
    capture()
}

#[test]
fn titlebar_capture_becomes_passport_elements() {
    let out = assemble_surface(Surface::Titlebar, &scene(), &recipe(), &capture()).unwrap();
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
    // Непрозрачность вуали входит в альфу заливки: `--focus-a` тёмной темы обычного контраста.
    let fill = veil.fill.unwrap();
    assert!((fill.a - 0.05).abs() < 1e-9, "{fill:?}");
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
    capture.facts["elements"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["ck"] != "zone.center.part.1");
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
    assert_eq!(again.coverage(&mask).unwrap(), 1.0);
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

/// Область снимка целая в точках CSS при любом начале кадра, а кадр вырезается из её снимка по
/// своим пикселям: начало в половине точки не сдвигает маску.
#[test]
fn capture_area_is_whole_points_and_the_frame_is_cropped_from_it() {
    // Кадр (1050.5, 12, 15×15) при плотности 2: пиксели 2101..2131 и 24..54.
    let clip = PixelClip::of(
        &Rect {
            x: 1050.5,
            y: 12.0,
            w: 15.0,
            h: 15.0,
        },
        2.0,
    )
    .unwrap();
    let (area, inner) = clip.capture(2.0).unwrap();
    assert_eq!((area.x, area.y, area.w, area.h), (1050.0, 12.0, 16.0, 15.0));
    assert_eq!(
        (inner.x, inner.y, inner.width, inner.height),
        (1, 0, 30, 30)
    );
    // Кадр с целым началом снимается как есть.
    let whole = PixelClip {
        x: 740,
        y: 26,
        width: 28,
        height: 28,
    };
    let (area, inner) = whole.capture(2.0).unwrap();
    assert_eq!((area.x, area.y, area.w, area.h), (370.0, 13.0, 14.0, 14.0));
    assert_eq!((inner.x, inner.y), (0, 0));
    assert!(clip.capture(1.5).is_err());

    // Снимок области 4×2 пикселя: столбец 1 красный. Вырезка кадра со сдвигом 1 начинается с него.
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, 4, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let row = [
            [0, 0, 0, 255],
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
        ]
        .concat();
        writer
            .write_image_data(&[row.clone(), row].concat())
            .unwrap();
    }
    let part = PixelClip {
        x: 1,
        y: 0,
        width: 2,
        height: 2,
    };
    let cropped = crop_png(&png, part).unwrap();
    let (width, height, rgba) = decode_rgba(&cropped).unwrap();
    assert_eq!((width, height), (2, 2));
    assert_eq!(rgba[..8], [255, 0, 0, 255, 0, 255, 0, 255]);
    assert_eq!(
        cropped,
        crop_png(&png, part).unwrap(),
        "запись детерминирована"
    );
    let outside = PixelClip {
        x: 3,
        y: 0,
        width: 2,
        height: 2,
    };
    assert!(crop_png(&png, outside).unwrap_err().contains("за краем"));
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

#[test]
fn filter_row_is_a_control_with_a_text_input() {
    assert_eq!(
        kind_of("div", "zone.right.part.1.filter"),
        ElementKind::Control
    );
    assert_eq!(
        kind_of("svg", "zone.right.part.1.filter.icon"),
        ElementKind::Icon
    );
    assert_eq!(
        kind_of("input", "zone.right.part.1.filter.input"),
        ElementKind::Text
    );
    let field = shellio_conformance::cdp::element(&json!({
        "ck": "zone.right.part.1.filter.input", "tag": "input", "parent": "zone.right.part.1.filter",
        "frame": {"x": 1.0, "y": 2.0, "w": 170.0, "h": 16.0}, "visible": true,
        "text": "альфа", "truncated": false,
        "style": {"fontFamily": "-apple-system", "fontWeight": "400", "fontSize": "13px",
                  "lineHeight": "16px", "letterSpacing": "normal", "color": "rgb(1, 2, 3)",
                  "background": "rgba(0, 0, 0, 0)", "border": [], "radius": "0px",
                  "padding": ["0px", "0px", "0px", "0px"], "shadow": "none"},
    }))
    .unwrap();
    assert_eq!(field.text.as_deref(), Some("альфа"));
    assert_eq!(field.truncated, Some(false));
    assert_eq!(field.font.as_ref().map(|f| f.size), Some(13.0));
}

#[test]
fn surface_keeps_only_its_keys() {
    let whole = assemble(&scene(), &recipe(), &capture()).unwrap();
    assert!(whole.elements.iter().any(|e| e.ck.starts_with("zone.")));
    assert!(whole.elements.iter().any(|e| e.ck == "status"));

    let titlebar = assemble_surface(Surface::Titlebar, &scene(), &recipe(), &capture()).unwrap();
    assert!(titlebar.elements.iter().all(|e| e.ck.starts_with("chrome")));
    assert!(titlebar.masks.keys().all(|k| k.starts_with("chrome")));
    // Состав и постусловия сверяются по всему окну при любой поверхности.
    assert_eq!(titlebar.composition, whole.composition);

    let status = assemble_surface(Surface::Statusline, &scene(), &recipe(), &capture()).unwrap();
    let keys: Vec<&str> = status.elements.iter().map(|e| e.ck.as_str()).collect();
    assert_eq!(keys, ["status"]);
    assert!(status.masks.is_empty());

    assert!(Surface::Titlebar.owns("chrome"));
    assert!(Surface::Titlebar.owns("chrome.search.kbd"));
    assert!(Surface::Titlebar.owns("chrome::after"));
    assert!(!Surface::Titlebar.owns("chromeless"));
    assert!(!Surface::Statusline.owns("chrome"));
    assert!(Surface::Layout.owns("picker.item.1"));
}

#[test]
fn line_is_read_from_every_side() {
    let out = assemble(&scene(), &recipe(), &capture()).unwrap();
    let sides = |ck: &str| {
        let e = out.elements.iter().find(|e| e.ck == ck).unwrap();
        (e.line_sides.clone(), e.line_width)
    };
    // Заголовок рисует нижнюю линию, строка статуса — верхнюю, кнопка зон — левый разделитель.
    assert_eq!(sides("chrome"), (vec![Side::Bottom], Some(1.0)));
    assert_eq!(sides("status"), (vec![Side::Top], Some(1.0)));
    assert_eq!(sides("chrome.zones.bottom"), (vec![Side::Left], Some(1.0)));
    assert_eq!(
        sides("chrome.search"),
        (
            vec![Side::Top, Side::Right, Side::Bottom, Side::Left],
            Some(1.0)
        )
    );
    assert_eq!(sides("chrome.appearance"), (Vec::new(), Some(0.0)));

    let side = |name: &str, color: &str, width: &str, style: &str| json!({"side": name, "color": color, "width": width, "style": style});
    let none = json!({"border": [side("top", "rgb(1, 2, 3)", "0px", "none")]});
    assert_eq!(line(&none).unwrap(), None);
    let clear = json!({"border": [side("top", "rgba(0, 0, 0, 0)", "1px", "solid")]});
    assert_eq!(line(&clear).unwrap(), None);
    let mixed = json!({"border": [
        side("top", "rgb(1, 2, 3)", "1px", "solid"),
        side("left", "rgb(1, 2, 3)", "2px", "solid"),
    ]});
    assert!(line(&mixed).unwrap_err().contains("различаются"));
    let unknown = json!({"border": [side("front", "rgb(1, 2, 3)", "1px", "solid")]});
    assert!(line(&unknown).is_err());
}

#[test]
fn box_shadow_is_parsed_outer_and_inset() {
    assert!(parse_shadows("none").unwrap().is_empty());
    assert!(parse_shadows("").unwrap().is_empty());
    let out = assemble(&scene(), &recipe(), &capture()).unwrap();
    let close = out
        .elements
        .iter()
        .find(|e| e.ck == "chrome.window.close")
        .unwrap();
    assert_eq!(close.shadow.len(), 1);
    assert!(close.shadow[0].inset);
    assert_eq!(close.shadow[0].spread, 0.5);

    let two = parse_shadows(
        "rgba(0, 0, 0, 0.5) 0px 12px 32px 0px, rgb(48, 46, 43) 0px 0px 0px 1px inset",
    )
    .unwrap();
    assert_eq!(two.len(), 2);
    assert_eq!((two[0].y, two[0].blur, two[0].inset), (12.0, 32.0, false));
    assert_eq!(two[0].color.a, 0.5);
    assert_eq!((two[1].spread, two[1].inset), (1.0, true));
    // Прозрачная тень не пишется.
    assert!(
        parse_shadows("rgba(0, 0, 0, 0) 0px 0px 0px 0px")
            .unwrap()
            .is_empty()
    );
    assert!(parse_shadows("0px 0px 4px").is_err());
    assert!(parse_shadows("rgb(1, 2, 3) 0px 0px").is_err());
}

#[test]
fn kinds_of_layout_keys() {
    assert_eq!(kind_of("section", "zone.center.part.1"), ElementKind::Plate);
    assert_eq!(
        kind_of("div", "zone.center.part.2.menu"),
        ElementKind::Plate
    );
    assert_eq!(kind_of("div", "picker"), ElementKind::Plate);
    // Подсказка строки статуса — группа, её подпись — текст, клавиша — плашка.
    assert_eq!(kind_of("span", "status.hint.2"), ElementKind::Container);
    assert_eq!(kind_of("em", "status.hint.2.label"), ElementKind::Text);
    assert_eq!(kind_of("kbd", "status.hint.2.key.1"), ElementKind::Badge);
    assert_eq!(kind_of("span", "status.message"), ElementKind::Text);
    assert_eq!(
        kind_of("span", "zone.center.part.1.tabs.tab.1.close"),
        ElementKind::Control
    );
    assert_eq!(
        kind_of("button", "zone.center.part.1.tabs.tab.12"),
        ElementKind::Tab
    );
    assert_eq!(
        kind_of("button", "zone.center.part.1.tabs.menu"),
        ElementKind::Control
    );
    assert_eq!(
        kind_of("div", "zone.center.part.1.body.stub.label"),
        ElementKind::Text
    );
    assert_eq!(
        kind_of("div", "zone.center.part.1.body"),
        ElementKind::Container
    );
    assert_eq!(
        kind_of("div", "zone.center.divider.1"),
        ElementKind::Container
    );
}
