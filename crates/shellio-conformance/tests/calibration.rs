//! Сторона прототипа калибровки: корпус, страница замера и проверка шрифта на закреплённых
//! материалах `conformance/calibration/`. Браузер не запускается.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use shellio_conformance::calibration::{
    Corpus, Family, case_id, cells, mono_faces, page, shot, two_tone,
};
use shellio_conformance::mask::ICON_FG;
use shellio_conformance::passport::{Cell, FontFace, Rgba};

fn material(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../conformance/calibration")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn corpus() -> Corpus {
    Corpus::parse("text.json", &material("text.json")).unwrap_or_else(|e| panic!("{e}"))
}

fn rgb(r: u8, g: u8, b: u8) -> Rgba {
    Rgba { r, g, b, a: 1.0 }
}

#[test]
fn page_carries_every_case_of_the_corpus_and_the_sheet() {
    let corpus = corpus();
    let html = page("file:///p/css/tokens.css", &material("icons.svg"), &corpus);
    assert_eq!(corpus.cases().count(), 360);
    assert_eq!(html.matches("<span data-case=\"").count(), 360);
    for (style, line) in corpus.cases() {
        assert!(
            html.contains(&format!("data-case=\"{}\"", case_id(style, line))),
            "нет случая {}",
            case_id(style, line)
        );
    }
    assert!(html.contains("<link rel=\"stylesheet\" href=\"file:///p/css/tokens.css\">"));
    assert!(html.contains("id=\"ctl-strokes-missing-16\""));
    assert!(html.contains("#icons [data-tone=\"secondary\"] { stroke: var(--line-2); }"));
    assert!(!html.contains("http://www.w3.org/2000/svg\" width=\"112\" height=\"232\" x="));
    let tracked = corpus
        .styles
        .iter()
        .filter(|s| s.tracking_em != 0.0)
        .count();
    assert_eq!(tracked, 2);
    assert_eq!(
        html.matches("letter-spacing:").count(),
        tracked * corpus.strings.len()
    );
    assert!(html.contains(
        "style=\"font-family: var(--mono); font-weight: 400; font-size: 10.5px\">main</span>"
    ));
    assert!(html.contains(
        "font-family: var(--ui); font-weight: 700; font-size: 11px; letter-spacing: 0.08em\">main<"
    ));
}

#[test]
fn page_escapes_text_and_corpus_rejects_a_repeated_case() {
    let text = r#"{"styles": [{"id": "s", "family": "ui", "weight": 400, "size": 12, "tracking_em": 0}],
                   "strings": [{"id": "a", "half": "fit", "text": "<b> & \"x\""}]}"#;
    let corpus = Corpus::parse("корпус", text).expect("корпус разбирается");
    let html = page("t.css", "<svg></svg>", &corpus);
    assert!(html.contains(">&lt;b&gt; &amp; &quot;x&quot;</span>"));

    let repeated = text.replace(
        r#"{"id": "a", "half": "fit", "text": "<b> & \"x\""}"#,
        r#"{"id": "a", "half": "fit", "text": "1"}, {"id": "a", "half": "holdout", "text": "2"}"#,
    );
    let error = Corpus::parse("корпус", &repeated).expect_err("повтор случая");
    assert!(error.reason.contains("s.a"), "{error}");
    assert!(Corpus::parse("корпус", r#"{"styles": [], "strings": []}"#).is_err());
}

#[test]
fn control_cells_are_the_four_passport_cells() {
    let found = cells("controls.json", &material("controls.json")).expect("ячейки разбираются");
    assert_eq!(found.len(), 4);
    let names: BTreeSet<&str> = found.iter().map(|c| c.name()).collect();
    let all: BTreeSet<&str> = Cell::ALL.iter().map(|c| c.name()).collect();
    assert_eq!(names, all);
    let odd = r#"{"cells": [{"id": "x", "theme": "dark", "contrast": "less"}]}"#;
    assert!(cells("controls.json", odd).is_err());
}

#[test]
fn mono_case_set_in_a_system_or_fallback_font_is_refused() {
    let face = |family: &str, custom: bool| FontFace {
        family: family.to_owned(),
        custom,
        glyphs: 4,
    };
    assert_eq!(mono_faces("c", &[face("JetBrainsMono-Bold", true)]), Ok(()));
    let system = mono_faces("c", &[face("JetBrainsMono-Regular", false)]).expect_err("системный");
    assert!(system.contains("JetBrainsMono-Regular"), "{system}");
    let fallback = mono_faces(
        "c",
        &[
            face("JetBrainsMono-Regular", true),
            face(".SFNS-Regular", false),
        ],
    )
    .expect_err("запасной шрифт");
    assert!(fallback.contains(".SFNS-Regular"), "{fallback}");
    assert!(mono_faces("c", &[face("Menlo-Regular", true)]).is_err());
    assert!(mono_faces("c", &[]).is_err());
    assert!(corpus().styles.iter().any(|s| s.family == Family::Mono));
}

#[test]
fn two_tone_needs_both_tones_at_the_foreground_threshold() {
    let background = rgb(25, 24, 22);
    let at = rgb(25 + ICON_FG, 24, 22);
    let below = rgb(25 + ICON_FG - 1, 24 + ICON_FG - 1, 22);
    assert!(two_tone(background, at, at, ICON_FG));
    assert!(!two_tone(background, at, below, ICON_FG));
    assert!(!two_tone(background, below, at, ICON_FG));
    let light = rgb(250, 249, 246);
    assert!(two_tone(
        light,
        rgb(79, 75, 68),
        rgb(250, 249, 246 - ICON_FG),
        ICON_FG
    ));
}

#[test]
fn screenshot_data_that_is_not_base64_is_refused() {
    assert_eq!(shot("AQID"), Ok(vec![1, 2, 3]));
    assert!(shot("не base64").is_err());
}
