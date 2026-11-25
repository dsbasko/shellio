//! Вывод допусков калибровки: правила `text.json` и `controls.json` на границах. Замеры
//! синтезированы: настоящие числа пишет только `make calibrate`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use shellio_e2e::calibrate::{Half, IconControl, Measured, Plan, Width, derive};
use shellio_e2e::conform::Tolerances;

const CELLS: [&str; 2] = ["dark-normal", "light-high"];
const FIT: &str = "ui.fit";
const HOLDOUT: &str = "ui.holdout";
const PADDING: &str = "padding-2pt.ui.holdout";
const CIRCLE: &str = "circle-10";
const EMPTY: &str = "empty-10";

fn plan() -> Plan {
    Plan {
        cells: CELLS.iter().map(|c| (*c).to_owned()).collect(),
        text_margin: 0.5,
        mask_margin: 0.02,
        cases: BTreeMap::from([
            (FIT.to_owned(), Half::Fit),
            (HOLDOUT.to_owned(), Half::Holdout),
        ]),
        text_controls: BTreeSet::from([PADDING.to_owned()]),
        primitives: BTreeSet::from([CIRCLE.to_owned()]),
        icon_controls: vec![IconControl {
            id: EMPTY.to_owned(),
            reference: "rrect-10".to_owned(),
            product: "ctl-empty-10".to_owned(),
        }],
    }
}

/// Ширина прототипа у всех случаев.
const BASE: f64 = 40.0;

struct Numbers {
    fit: f64,
    holdout: f64,
    padding: f64,
    positive: f64,
    control: f64,
}

/// Годный замер: разница половины `fit` 0,25, отступ контроля 2 точки, пара 0,9, контроль 0,3.
const GOOD: Numbers = Numbers {
    fit: 0.25,
    holdout: 0.125,
    padding: 2.0,
    positive: 0.9,
    control: 0.3,
};

fn measured(n: &Numbers) -> Measured {
    let wide = |gap: f64| Width {
        prototype: BASE,
        product: BASE + gap,
    };
    let per_cell = |value: BTreeMap<String, Width>| -> BTreeMap<String, BTreeMap<String, Width>> {
        CELLS
            .iter()
            .map(|c| ((*c).to_owned(), value.clone()))
            .collect()
    };
    let ratio = |id: &str, iou: f64| -> BTreeMap<String, BTreeMap<String, f64>> {
        CELLS
            .iter()
            .map(|c| ((*c).to_owned(), BTreeMap::from([(id.to_owned(), iou)])))
            .collect()
    };
    Measured {
        text: per_cell(BTreeMap::from([
            (FIT.to_owned(), wide(n.fit)),
            (HOLDOUT.to_owned(), wide(n.holdout)),
        ])),
        text_controls: per_cell(BTreeMap::from([(PADDING.to_owned(), wide(n.padding))])),
        positives: ratio(CIRCLE, n.positive),
        icon_controls: ratio(EMPTY, n.control),
        two_tone: CELLS.iter().map(|c| ((*c).to_owned(), true)).collect(),
    }
}

/// Допуски состоявшейся калибровки либо текст отказа.
fn outcome(n: &Numbers) -> Result<Tolerances, String> {
    derive(&plan(), &measured(n))
        .and_then(|record| Tolerances::from_calibration(&record))
        .map_err(|e| e.message().to_owned())
}

#[test]
fn record_carries_the_worst_fit_gap_plus_margin_and_the_least_pair_minus_margin() {
    let record = derive(&plan(), &measured(&GOOD)).expect("годный замер");
    let read = Tolerances::from_calibration(&record).expect("запись читается судьёй");
    assert_eq!(read.text_width, 0.25 + 0.5);
    assert_eq!(read.mask_iou, 0.9 - 0.02);
    let value: serde_json::Value = serde_json::from_str(&record).expect("JSON");
    assert_eq!(value["text"]["fit_max_pt"], 0.25);
    assert_eq!(
        value["text"]["cases"]["dark-normal"][HOLDOUT]["half"],
        "holdout"
    );
    assert_eq!(
        value["text"]["controls"]["light-high"][PADDING]["product"],
        42.0
    );
    assert_eq!(value["icons"]["controls"]["dark-normal"][EMPTY], 0.3);
    assert_eq!(value["icons"]["two_tone"]["light-high"], true);
}

#[test]
fn the_worst_case_is_taken_over_every_cell() {
    let mut m = measured(&GOOD);
    let case = m.text.get_mut("light-high").and_then(|c| c.get_mut(FIT));
    case.expect("случай").product = BASE - 0.375;
    let pair = m
        .positives
        .get_mut("light-high")
        .and_then(|c| c.get_mut(CIRCLE));
    *pair.expect("пара") = 0.75;
    let read = Tolerances::from_calibration(&derive(&plan(), &m).expect("годный замер"))
        .expect("запись читается судьёй");
    assert_eq!(read.text_width, 0.375 + 0.5);
    assert_eq!(read.mask_iou, 0.75 - 0.02);
}

#[test]
fn holdout_passes_at_the_tolerance_and_fails_beyond_it() {
    let tolerance = outcome(&GOOD).expect("калибровка состоялась").text_width;
    let at = Numbers {
        holdout: tolerance,
        ..GOOD
    };
    assert_eq!(
        outcome(&at).expect("калибровка состоялась").text_width,
        tolerance
    );
    let beyond = outcome(&Numbers {
        holdout: tolerance + 0.125,
        ..GOOD
    })
    .expect_err("отказ");
    assert!(
        beyond.contains("контроли калибровки не разделены"),
        "{beyond}"
    );
    assert!(
        beyond.contains("отложенный случай dark-normal ui.holdout"),
        "{beyond}"
    );
    assert!(beyond.contains("dark-normal ui.fit — 0.25"), "{beyond}");
}

#[test]
fn padding_control_must_fail_what_the_judge_applies() {
    // Допуск 0,75 меньше точки кадра: судья применяет точку, и контроль обязан выйти за неё.
    let limit = outcome(&GOOD)
        .expect("калибровка состоялась")
        .text_width_limit();
    assert_eq!(limit, 1.0);
    let at = outcome(&Numbers {
        padding: limit,
        ..GOOD
    })
    .expect_err("отказ");
    assert!(
        at.contains("контроль ширины dark-normal padding-2pt.ui.holdout прошёл"),
        "{at}"
    );
    outcome(&Numbers {
        padding: limit + 0.125,
        ..GOOD
    })
    .expect("калибровка состоялась");
    // Разница половины `fit` 1,5 даёт допуск 2: отступ 2 точки им поглощён.
    let wide = Numbers { fit: 1.5, ..GOOD };
    let swallowed = outcome(&wide).expect_err("отказ");
    assert!(swallowed.contains("допуск ширины 2,"), "{swallowed}");
    assert!(swallowed.contains("контроль ширины"), "{swallowed}");
    outcome(&Numbers {
        padding: 2.125,
        ..wide
    })
    .expect("калибровка состоялась");
}

#[test]
fn mask_control_passes_at_the_threshold_and_that_refuses_the_calibration() {
    let threshold = outcome(&GOOD).expect("калибровка состоялась").mask_iou;
    let at = outcome(&Numbers {
        control: threshold,
        ..GOOD
    })
    .expect_err("отказ");
    assert!(
        at.contains("контроль маски dark-normal empty-10 прошёл"),
        "{at}"
    );
    outcome(&Numbers {
        control: threshold - 1e-9,
        ..GOOD
    })
    .expect("калибровка состоялась");
}

#[test]
fn two_tone_and_incomplete_or_unreadable_measurements_are_refused() {
    let refused = |m: &Measured| derive(&plan(), m).expect_err("отказ").message().to_owned();

    let mut m = measured(&GOOD);
    m.two_tone.insert("light-high".to_owned(), false);
    assert!(refused(&m).contains("два тона: в light-high"));

    let mut m = measured(&GOOD);
    m.text
        .get_mut("light-high")
        .expect("ячейка")
        .remove(HOLDOUT);
    assert!(refused(&m).contains("в light-high нет замера ui.holdout"));

    let mut m = measured(&GOOD);
    m.icon_controls.remove("dark-normal");
    assert!(refused(&m).contains("контроль маски: ячейки замера"));

    let mut m = measured(&GOOD);
    let extra = m.positives.get_mut("dark-normal").expect("ячейка");
    extra.insert("rrect-10".to_owned(), 0.95);
    assert!(refused(&m).contains("замер rrect-10 вне плана"));

    let mut m = measured(&GOOD);
    let case = m.text.get_mut("dark-normal").and_then(|c| c.get_mut(FIT));
    case.expect("случай").product = f64::NAN;
    assert!(refused(&m).contains("не положительное число"));

    let mut m = measured(&GOOD);
    let pair = m
        .positives
        .get_mut("dark-normal")
        .and_then(|c| c.get_mut(CIRCLE));
    *pair.expect("пара") = 1.5;
    assert!(refused(&m).contains("вне отрезка от 0 до 1"));

    // Пара с мерой не больше запаса порога не даёт: запись без порога судья не читает.
    let low = Numbers {
        positive: 0.02,
        control: 0.0,
        ..GOOD
    };
    assert!(
        outcome(&low)
            .expect_err("отказ")
            .contains("/icons/threshold")
    );
}

#[test]
fn plan_of_the_pinned_materials_lists_cases_and_controls_from_the_files() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/calibration");
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).expect(name);
    let primitives = ["circle-10".to_owned(), "rrect-10".to_owned()];
    let plan = Plan::parse(&read("text.json"), &read("controls.json"), &primitives).expect("план");

    assert_eq!(
        plan.cells,
        ["dark-normal", "dark-high", "light-normal", "light-high"]
    );
    assert_eq!((plan.text_margin, plan.mask_margin), (0.5, 0.02));
    assert_eq!(plan.cases.len(), 360);
    let halves = |half: Half| plan.cases.values().filter(|h| **h == half).count();
    assert_eq!((halves(Half::Fit), halves(Half::Holdout)), (180, 180));
    // Два стиля контроля на 12 строк отложенной половины.
    assert_eq!(plan.text_controls.len(), 24);
    assert!(
        plan.text_controls
            .iter()
            .all(|case| case.starts_with("padding-2pt.ui-500-12.5.")
                || case.starts_with("padding-2pt.mono-400-10.5."))
    );
    let stripped: Vec<&str> = plan
        .text_controls
        .iter()
        .filter_map(|case| case.strip_prefix("padding-2pt."))
        .collect();
    assert!(
        stripped
            .iter()
            .all(|case| plan.cases.get(*case) == Some(&Half::Holdout))
    );
    // Три контроля на четыре размера листа; `other-primitive` — две ячейки с ролью primitive.
    assert_eq!(plan.icon_controls.len(), 12);
    let other = plan
        .icon_controls
        .iter()
        .find(|c| c.id == "other-primitive-13");
    let other = other.expect("контроль другого примитива");
    assert_eq!(
        (other.reference.as_str(), other.product.as_str()),
        ("circle-13", "rrect-13")
    );
    let missing = plan
        .icon_controls
        .iter()
        .find(|c| c.id == "missing-stroke-16");
    assert_eq!(
        missing.map(|c| (c.reference.as_str(), c.product.as_str())),
        Some(("strokes-16", "ctl-strokes-missing-16"))
    );

    assert!(Plan::parse(&read("text.json"), &read("controls.json"), &[]).is_err());
}

mod measure {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::process::Command;

    use serde_json::{Value, json};
    use shellio_conformance::mask::Mask;
    use shellio_conformance::passport::Rgba;
    use shellio_e2e::calibrate::{ChromeCell, Half, IconControl, Plan, measure_cell, primitives};
    use shellio_e2e::conform::Product;

    const UI: &str = "ui-1.a";
    const MONO: &str = "mono-1.b";
    const PADDING: &str = "padding-2pt.ui-1.a";
    const FILL: [u8; 3] = [20, 22, 24];
    const INK: [u8; 3] = [150, 160, 170];
    /// Масштаб снимка и сторона ячейки листа в пикселях.
    const SCALE: f64 = 2.0;
    const SIDE: u32 = 20;

    fn rgba([r, g, b]: [u8; 3]) -> Rgba {
        Rgba { r, g, b, a: 1.0 }
    }

    fn plan() -> Plan {
        Plan {
            cells: vec!["dark-normal".to_owned()],
            text_margin: 0.5,
            mask_margin: 0.02,
            cases: BTreeMap::from([(UI.to_owned(), Half::Fit), (MONO.to_owned(), Half::Holdout)]),
            text_controls: BTreeSet::from([PADDING.to_owned()]),
            primitives: BTreeSet::from(["circle-10".to_owned()]),
            icon_controls: vec![IconControl {
                id: "empty-10".to_owned(),
                reference: "rrect-10".to_owned(),
                product: "ctl-empty-10".to_owned(),
            }],
        }
    }

    /// Рисунок ячейки: диагональ и столбец.
    fn drawing() -> Mask {
        let on = (0..SIDE * SIDE)
            .map(|i| i % SIDE == i / SIDE || i % SIDE == 3)
            .collect();
        Mask {
            width: SIDE,
            height: SIDE,
            on,
        }
    }

    fn blank(side: u32) -> Mask {
        Mask {
            width: side,
            height: side,
            on: vec![false; (side * side) as usize],
        }
    }

    fn chrome() -> ChromeCell {
        ChromeCell {
            text: BTreeMap::from([(UI.to_owned(), 40.0), (MONO.to_owned(), 30.0)]),
            two_tone: true,
            background: rgba(FILL),
            primary: rgba(INK),
            masks: BTreeMap::from([
                ("circle-10".to_owned(), drawing()),
                ("rrect-10".to_owned(), drawing()),
                ("ctl-empty-10".to_owned(), blank(SIDE)),
            ]),
        }
    }

    const BUNDLED: &str = "/x/shellio.app/Contents/Resources/Fonts/JetBrainsMono-Regular.ttf";
    const SYSTEM: &str = "/System/Library/Fonts/SFNS.ttf";

    fn label(tail: &str, width: f64, file: &str) -> Value {
        json!({
            "id": format!("gallery.calibration.{tail}"),
            "frame": {"x": 128.0, "y": 50.0, "width": width, "height": 15.0},
            "visible": true,
            "label": "текст",
            "style": {
                "font": {"family": "f", "face": "f", "weight": 400.0, "size": 12.0, "file": file},
                "color": {"r": INK[0], "g": INK[1], "b": INK[2], "a": 1.0},
            },
        })
    }

    /// Ячейка листа 10×10 точек: `x` от левого края, верхний край в 10 точках от верха окна.
    fn icon(id: &str, x: f64) -> Value {
        json!({
            "id": format!("gallery.calibration.icon.{id}"),
            "frame": {"x": x, "y": 80.0, "width": 10.0, "height": 10.0},
            "visible": true,
        })
    }

    /// Годное дерево страницы: окно 200×100 точек, круг в x 10, пустой контроль в x 30.
    fn tree() -> Value {
        json!({
            "content": {"width": 200.0, "height": 100.0},
            "components": [
                {
                    "id": "gallery.calibration",
                    "frame": {"x": 0.0, "y": 0.0, "width": 200.0, "height": 100.0},
                    "visible": true,
                    "style": {"fill": {"r": FILL[0], "g": FILL[1], "b": FILL[2], "a": 1.0}},
                },
                label(&format!("text.{UI}"), 40.25, SYSTEM),
                label(&format!("text.{MONO}"), 30.0, BUNDLED),
                label(&format!("control.{PADDING}"), 42.25, SYSTEM),
                icon("circle-10", 10.0),
                icon("ctl-empty-10", 30.0),
            ],
        })
    }

    /// Снимок: фон `back`, рисунок круга на месте его ячейки.
    fn snapshot(back: [u8; 3]) -> impl Fn(u32, u32) -> Option<[u8; 3]> {
        let mask = drawing();
        move |x, y| {
            if x >= 400 || y >= 200 {
                return None;
            }
            let inside = (20..20 + SIDE).contains(&x) && (20..20 + SIDE).contains(&y);
            let on = inside && mask.on[((y - 20) * SIDE + x - 20) as usize];
            Some(if on { INK } else { back })
        }
    }

    fn refusal(tree: &Value, chrome: &ChromeCell, back: [u8; 3]) -> Option<String> {
        let product = Product::parse(&tree.to_string()).ok()?;
        measure_cell(&plan(), chrome, &product, SCALE, &snapshot(back))
            .err()
            .map(|e| e.to_string())
    }

    fn without(tree: &Value, id: &str) -> Value {
        let mut tree = tree.clone();
        if let Some(list) = tree["components"].as_array_mut() {
            list.retain(|node| node["id"] != id);
        }
        tree
    }

    fn edited(tree: &Value, id: &str, pointer: &str, value: Value) -> Value {
        let mut tree = tree.clone();
        if let Some(list) = tree["components"].as_array_mut() {
            for node in list.iter_mut().filter(|node| node["id"] == id) {
                if let Some(slot) = node.pointer_mut(pointer) {
                    *slot = value.clone();
                }
            }
        }
        tree
    }

    #[test]
    fn cell_measure_reads_label_frames_and_masks_of_the_snapshot() {
        let product = Product::parse(&tree().to_string()).expect("tree");
        let measure =
            measure_cell(&plan(), &chrome(), &product, SCALE, &snapshot(FILL)).expect("measure");
        assert_eq!(measure.text[UI].prototype, 40.0);
        assert_eq!(measure.text[UI].product, 40.25);
        assert_eq!(measure.text[MONO].product, 30.0);
        // Контроль отступа: ширина Chrome той же строки против кадра подписи с отступом.
        assert_eq!(measure.text_controls[PADDING].prototype, 40.0);
        assert_eq!(measure.text_controls[PADDING].product, 42.25);
        assert_eq!(measure.positives["circle-10"], 1.0);
        assert_eq!(measure.icon_controls["empty-10"], 0.0);
        assert!(measure.two_tone);
    }

    #[test]
    fn measurement_is_refused_rather_than_guessed() {
        let (tree, chrome) = (tree(), chrome());
        let text = format!("gallery.calibration.text.{UI}");
        let mono = format!("gallery.calibration.text.{MONO}");
        let circle = "gallery.calibration.icon.circle-10";
        assert_eq!(refusal(&tree, &chrome, FILL), None);

        let cases = [
            (without(&tree, &text), "нет узла"),
            (without(&tree, "gallery.calibration"), "страница не открыта"),
            (
                edited(&tree, &mono, "/style/font/file", json!(SYSTEM)),
                "вложенным шрифтом",
            ),
            (
                edited(&tree, &text, "/style/font/file", json!(BUNDLED)),
                "вложенным шрифтом",
            ),
            (
                edited(&tree, &text, "/style/color/r", json!(INK[0] + 4)),
                "тон подписи",
            ),
            (
                edited(
                    &tree,
                    "gallery.calibration",
                    "/style/fill/g",
                    json!(FILL[1] + 4),
                ),
                "тон листа",
            ),
            (edited(&tree, circle, "/visible", json!(false)), "вне окна"),
            (
                edited(&tree, circle, "/frame/width", json!(12.0)),
                "больше чем на пиксель",
            ),
            (edited(&tree, circle, "/frame/x", json!(195.0)), "за кадром"),
        ];
        for (tree, reason) in cases {
            let got = refusal(&tree, &chrome, FILL).expect(reason);
            assert!(got.contains(reason), "{reason}: {got}");
        }

        // Фон листа на снимке в допуске цвета проходит, за ним — предусловие экрана.
        let near = [FILL[0] + 3, FILL[1], FILL[2]];
        assert_eq!(refusal(&tree, &chrome, near), None);
        let far = [FILL[0] + 4, FILL[1], FILL[2]];
        let got = refusal(&tree, &chrome, far).expect("screen");
        assert!(got.starts_with("предусловие: фон листа"), "{got}");

        let mut short = chrome.clone();
        short.text.remove(MONO);
        assert!(
            refusal(&tree, &short, FILL)
                .expect("width")
                .contains("нет ширины случая")
        );
        let mut lost = chrome.clone();
        lost.masks.remove("rrect-10");
        assert!(
            refusal(&tree, &lost, FILL)
                .expect("mask")
                .contains("нет маски")
        );
    }

    #[test]
    fn chrome_cell_is_read_from_the_record_with_its_masks() {
        let record = json!({"cells": {"dark-normal": {
            "text": {UI: 40.0},
            "two_tone": true,
            "tones": {
                "background": {"r": 20, "g": 22, "b": 24, "a": 1.0},
                "primary": {"r": 150, "g": 160, "b": 170, "a": 1.0},
                "secondary": {"r": 60, "g": 60, "b": 60, "a": 1.0},
            },
            "icons": {"circle-10": {"pixels": 1}},
        }}});
        let cell = ChromeCell::parse(&record, "dark-normal", |_| Ok(drawing())).expect("cell");
        assert_eq!(cell.text[UI], 40.0);
        assert_eq!(cell.background, rgba(FILL));
        assert_eq!(cell.masks["circle-10"], drawing());
        let missing = ChromeCell::parse(&record, "light-high", |_| Ok(drawing()));
        assert!(
            missing
                .expect_err("cell")
                .to_string()
                .contains("/cells/light-high")
        );
        let unread = ChromeCell::parse(&record, "dark-normal", |id| Err(format!("нет {id}.png")));
        assert!(
            unread
                .expect_err("mask")
                .to_string()
                .contains("нет circle-10.png")
        );
    }

    #[test]
    fn primitive_cells_come_from_the_pinned_sheet() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/calibration");
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).expect(name);
        let cells = primitives(&read("icons.svg")).expect("sheet");
        assert_eq!(cells.len(), 28);
        assert!(cells.contains(&"two-tone-16".to_owned()));
        assert!(!cells.iter().any(|id| id.starts_with("ctl-")));
        let plan = Plan::parse(&read("text.json"), &read("controls.json"), &cells).expect("plan");
        assert_eq!(plan.primitives.len(), 28);
        // Контроль сравнивает ячейки листа: каждая названа листом.
        let sheet = read("icons.svg");
        for control in &plan.icon_controls {
            assert!(sheet.contains(&format!(" id=\"{}\"", control.reference)));
            assert!(sheet.contains(&format!(" id=\"{}\"", control.product)));
        }

        let only_controls = "<svg><svg id=\"c\" data-role=\"control\"></svg></svg>";
        assert!(primitives(only_controls).is_err());
        let unnamed = "<svg><svg data-id=\"x\" data-role=\"primitive\"></svg></svg>";
        assert!(
            primitives(unnamed)
                .expect_err("id")
                .to_string()
                .contains("без id")
        );
    }

    #[test]
    fn calibrate_subcommand_refuses_a_partial_call_and_missing_readings() {
        let oracle = env!("CARGO_BIN_EXE_shellio-e2e-oracle");
        let code = |args: &[&str]| {
            Command::new(oracle)
                .args(args)
                .output()
                .expect("run")
                .status
                .code()
        };
        assert_eq!(code(&["calibrate", "--chrome", "/nonexistent"]), Some(2));
        assert_eq!(code(&["compare", "--raw", "/nonexistent"]), Some(2));
        let materials =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/calibration");
        let out =
            std::env::temp_dir().join(format!("shellio-calibrate-{}.json", std::process::id()));
        let args = [
            "calibrate",
            "--chrome",
            "/nonexistent",
            "--product",
            "/nonexistent",
            "--materials",
            materials.to_str().expect("path"),
            "--raw",
            out.to_str().expect("path"),
            "--out",
            out.to_str().expect("path"),
        ];
        // Нет замера Chrome — ошибка исполнения, запись не появляется.
        assert_eq!(code(&args), Some(1));
        assert!(!out.exists());
    }
}
