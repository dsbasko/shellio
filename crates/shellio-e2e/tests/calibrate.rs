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
