use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::json;
use shellio_conformance::fields::{ElementKind, check_element};
use shellio_conformance::keys::{check_glob, check_key, check_unique, glob_matches, template_of};
use shellio_conformance::passport::{
    Cell, Contrast, Element, Passport, Rect, Theme, Window, canonical_json,
};
use shellio_conformance::recipe::{Recipe, Step};
use shellio_conformance::registry::{
    Inventory, InventoryPlugin, Registry, completeness, done_steps,
};
use shellio_conformance::scene::{Capabilities, Scene, StatusHint};
use shellio_conformance::validity::{Inputs, Problem, SNAP_SCHEMA, check_registry, tree_stamp};
use shellio_conformance::{Zone, plans};

const SCENE: &str = r#"{
  "workspace": "Работа",
  "catalog": [
    {"id": "terminal", "title": "Терминал", "icon": "terminal", "cardinality": "multi",
     "placements": ["left", "right", "center", "bottom"], "default_zone": "center",
     "min_size": {"width": 320, "height": 120}},
    {"id": "navigator", "title": "Навигатор", "icon": "folder", "cardinality": "per_workbench",
     "placements": ["left", "right", "center", "bottom"], "default_zone": "left",
     "min_size": {"width": 200, "height": 160}}
  ],
  "zones": [
    {"zone": "left", "parts": [{"tabs": [{"plugin": "navigator", "active": true}]}]},
    {"zone": "center", "parts": [{"tabs": [{"plugin": "terminal", "active": true}]}]}
  ],
  "focus": {"zone": "center", "part": 1},
  "appearance": "dark"
}"#;

const RECIPE: &str = r#"{"scene": "s", "post": {"parts": {"left": 1, "center": 1}}}"#;

const REGISTRY: &str = r#"
[[state]]
id = "chrome-titlebar"
step = "P1.3"
surface = "titlebar"
scene = "s"
recipe = "r"
cells = ["dark-normal", "light-high"]
"#;

fn element(ck: &str, kind: ElementKind) -> Element {
    Element {
        ck: ck.to_owned(),
        kind,
        parent: None,
        frame: Rect {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        },
        visible: true,
        text: None,
        truncated: None,
        font: None,
        line_height: None,
        letter_spacing: None,
        color: None,
        fill: None,
        line_color: None,
        line_width: None,
        radius: None,
        padding: None,
        line_sides: Vec::new(),
        shadow: Vec::new(),
        icon: None,
    }
}

fn ok<T, E: std::fmt::Display>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|e| panic!("{e}"))
}

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        ok(fs::create_dir_all(dir));
    }
    ok(fs::write(path, text));
}

/// Временный корень репозитория с прототипом, закреплением, сценой, рецептом и годными
/// паспортами состояния `chrome-titlebar`.
fn repo() -> (tempfile::TempDir, Inputs, Registry) {
    let dir = ok(tempfile::tempdir());
    let root = dir.path();
    write(&root.join("prototype/index.html"), "<!doctype html>\n");
    write(&root.join("prototype/js/main.js"), "export {};\n");
    write(&root.join("scripts/conformance/pin"), "CHROME_VERSION=1\n");
    write(&root.join("conformance/scenes/s.json"), SCENE);
    write(&root.join("conformance/recipes/r.json"), RECIPE);
    let registry = ok(Registry::parse("registry.toml", REGISTRY));
    let inputs = ok(Inputs::from_root(root));
    let scene = ok(Scene::parse("s", SCENE));
    for cell in registry.states[0].cells() {
        let passport = Passport {
            validity: ok(inputs.expected("r", cell)),
            window: Window {
                width: 1180.0,
                height: 720.0,
                dpr: 2.0,
            },
            composition: scene.composition(),
            elements: vec![element("chrome", ElementKind::Container)],
        };
        write(
            &inputs.passport_path("chrome-titlebar", cell),
            &ok(passport.to_canonical()),
        );
    }
    (dir, inputs, registry)
}

fn stale(inputs: &Inputs, registry: &Registry) -> Vec<(Cell, Problem)> {
    ok(check_registry(inputs, registry))
        .into_iter()
        .map(|f| (f.cell, f.problem))
        .collect()
}

#[test]
fn canonical_json_is_stable_under_field_order() {
    let a = json!({"b": 1, "a": {"y": 2.5, "x": [1.0, -0.0000001]}, "c": "т"});
    let b: serde_json::Value =
        serde_json::from_str(r#"{"c":"т","a":{"x":[1.0,-1e-7],"y":2.5},"b":1}"#).unwrap();
    let text = canonical_json(&a).unwrap();
    assert_eq!(text, canonical_json(&b).unwrap());
    assert_eq!(
        text,
        "{\n  \"a\": {\n    \"x\": [\n      1.000000,\n      0.000000\n    ],\n    \"y\": 2.500000\n  },\n  \"b\": 1,\n  \"c\": \"т\"\n}\n"
    );
}

#[test]
fn passport_round_trips_through_canonical_form() {
    let (_dir, inputs, _registry) = repo();
    let path = inputs.passport_path("chrome-titlebar", Cell::new(Theme::Dark, Contrast::Normal));
    let text = fs::read_to_string(&path).unwrap();
    let passport = Passport::parse("p", &text).unwrap();
    assert_eq!(passport.to_canonical().unwrap(), text);
    assert_eq!(passport.validity.schema, SNAP_SCHEMA);
}

#[test]
fn fresh_passports_are_valid() {
    let (_dir, inputs, registry) = repo();
    assert_eq!(stale(&inputs, &registry), vec![]);
}

#[test]
fn byte_of_prototype_makes_passports_stale() {
    let (dir, _inputs, registry) = repo();
    write(&dir.path().join("prototype/js/main.js"), "export {} ;\n");
    let inputs = Inputs::from_root(dir.path()).unwrap();
    let found = stale(&inputs, &registry);
    assert_eq!(found.len(), 2);
    assert!(
        found
            .iter()
            .all(|(_, p)| *p == Problem::Stale(vec!["prototype"]))
    );
}

#[test]
fn new_file_in_prototype_changes_stamp() {
    let (dir, inputs, _registry) = repo();
    write(&dir.path().join("prototype/fonts/a.ttf"), "");
    assert_ne!(
        tree_stamp(&dir.path().join("prototype")).unwrap(),
        inputs.prototype
    );
}

#[test]
fn recipe_change_names_stale_passport() {
    let (dir, inputs, registry) = repo();
    write(
        &dir.path().join("conformance/recipes/r.json"),
        r#"{"scene": "s", "motion": "reduce", "post": {"parts": {"left": 1, "center": 1}}}"#,
    );
    let findings = check_registry(&inputs, &registry).unwrap();
    assert_eq!(findings.len(), 2);
    for finding in &findings {
        assert_eq!(finding.problem, Problem::Stale(vec!["recipe"]));
        assert!(finding.to_string().contains("chrome-titlebar"));
        assert!(finding.path.ends_with(format!("{}.json", finding.cell)));
    }
}

#[test]
fn recipe_reformatting_keeps_passports_valid() {
    let (dir, inputs, registry) = repo();
    write(
        &dir.path().join("conformance/recipes/r.json"),
        "{\n  \"post\": {\"parts\": {\"center\": 1, \"left\": 1}},\n  \"scene\": \"s\"\n}\n",
    );
    assert_eq!(stale(&inputs, &registry), vec![]);
}

#[test]
fn scene_change_makes_passports_stale() {
    let (dir, inputs, registry) = repo();
    write(
        &dir.path().join("conformance/scenes/s.json"),
        &SCENE.replace("\"dark\"", "\"light\""),
    );
    let found = stale(&inputs, &registry);
    assert_eq!(found.len(), 2);
    assert!(
        found
            .iter()
            .all(|(_, p)| *p == Problem::Stale(vec!["scene"]))
    );
}

#[test]
fn pin_change_makes_passports_stale() {
    let (dir, _inputs, registry) = repo();
    write(
        &dir.path().join("scripts/conformance/pin"),
        "CHROME_VERSION=2\n",
    );
    let inputs = Inputs::from_root(dir.path()).unwrap();
    let found = stale(&inputs, &registry);
    assert_eq!(found.len(), 2);
    assert!(found.iter().all(|(_, p)| *p == Problem::Stale(vec!["pin"])));
}

#[test]
fn schema_change_makes_passports_stale() {
    let (_dir, mut inputs, registry) = repo();
    inputs.schema = SNAP_SCHEMA + 1;
    let found = stale(&inputs, &registry);
    assert_eq!(found.len(), 2);
    assert!(
        found
            .iter()
            .all(|(_, p)| *p == Problem::Stale(vec!["schema"]))
    );
}

#[test]
fn passport_of_other_cell_is_stale() {
    let (_dir, inputs, registry) = repo();
    let dark = inputs.passport_path("chrome-titlebar", Cell::new(Theme::Dark, Contrast::Normal));
    let light = inputs.passport_path("chrome-titlebar", Cell::new(Theme::Light, Contrast::High));
    fs::copy(&dark, &light).unwrap();
    assert_eq!(
        stale(&inputs, &registry),
        vec![(
            Cell::new(Theme::Light, Contrast::High),
            Problem::Stale(vec!["cell"])
        )]
    );
}

#[test]
fn missing_and_non_canonical_passports_are_reported() {
    let (_dir, inputs, registry) = repo();
    let dark = inputs.passport_path("chrome-titlebar", Cell::new(Theme::Dark, Contrast::Normal));
    let light = inputs.passport_path("chrome-titlebar", Cell::new(Theme::Light, Contrast::High));
    let text = fs::read_to_string(&dark).unwrap();
    let compact: serde_json::Value = serde_json::from_str(&text).unwrap();
    fs::write(&dark, compact.to_string()).unwrap();
    fs::remove_file(&light).unwrap();
    let found = stale(&inputs, &registry);
    assert!(found.contains(&(Cell::new(Theme::Light, Contrast::High), Problem::Missing)));
    assert!(found.contains(&(
        Cell::new(Theme::Dark, Contrast::Normal),
        Problem::Broken("запись не каноническая".to_owned())
    )));
}

fn registry_error(text: &str) -> String {
    match Registry::parse("registry.toml", text) {
        Err(e) => e.reason,
        Ok(_) => panic!("реестр принят:\n{text}"),
    }
}

#[test]
fn registry_accepts_three_kinds_of_rows() {
    let text = format!(
        "{REGISTRY}
[[assignment]]
scenario = \"palette\"
step = \"P1.6\"

[[assignment]]
scenario = \"light\"
step = \"P1.3\"
axis = \"theme\"

[[assignment]]
plugin = \"agent\"
zone = \"right\"
step = \"P3.1\"

[[exception]]
states = [\"chrome-titlebar\"]
scope = {{ keys = [\"chrome.window.*\"] }}
fields = [\"frame\"]
adr = \"ADR-0023\"
reason = \"кнопки окна системные\"

[[exception]]
scope = {{ mode = \"gallery\" }}
fields = [\"all\"]
adr = \"ADR-0037\"
reason = \"галерея P1.3 без эталона\"
"
    );
    let registry = Registry::parse("registry.toml", &text).unwrap();
    assert_eq!(registry.assignments.len(), 3);
    assert_eq!(registry.states.len(), 1);
    assert_eq!(registry.exceptions.len(), 2);
}

#[test]
fn registry_rejects_duplicates() {
    let twice = format!("{REGISTRY}{REGISTRY}");
    assert!(registry_error(&twice).contains("повторяется"));
    let assignment = "[[assignment]]\nscenario = \"palette\"\nstep = \"P1.6\"\n";
    assert!(registry_error(&format!("{assignment}{assignment}")).contains("повторяется"));
}

#[test]
fn registry_rejects_rows_without_step() {
    assert!(registry_error("[[assignment]]\nscenario = \"palette\"\n").contains("step"));
    assert!(registry_error(&REGISTRY.replace("step = \"P1.3\"\n", "")).contains("step"));
    assert!(registry_error(&REGISTRY.replace("P1.3", "скоро")).contains("шаг"));
}

#[test]
fn registry_rejects_incomplete_exceptions() {
    let base =
        |body: &str| format!("{REGISTRY}\n[[exception]]\nstates = [\"chrome-titlebar\"]\n{body}");
    let scope = "scope = { keys = [\"chrome.window.*\"] }\n";
    let fields = "fields = [\"frame\"]\n";
    let adr = "adr = \"ADR-0023\"\n";
    let reason = "reason = \"кнопки окна системные\"\n";
    assert!(registry_error(&base(&format!("{fields}{adr}{reason}"))).contains("без области"));
    assert!(registry_error(&base(&format!("{scope}{adr}{reason}"))).contains("без списка полей"));
    assert!(
        registry_error(&base(&format!("{scope}fields = []\n{adr}{reason}")))
            .contains("без списка полей")
    );
    assert!(registry_error(&base(&format!("{scope}{fields}{reason}"))).contains("без ADR"));
    assert!(registry_error(&base(&format!("{scope}{fields}{adr}"))).contains("без причины"));
    let stateless = format!("{REGISTRY}\n[[exception]]\n{scope}{fields}{adr}{reason}");
    assert!(registry_error(&stateless).contains("без состояния"));
    let unknown = base(&format!("{scope}{fields}{adr}{reason}")).replace(
        "states = [\"chrome-titlebar\"]",
        "states = [\"layout-zoom\"]",
    );
    assert!(registry_error(&unknown).contains("нет в реестре"));
}

#[test]
fn completeness_names_unassigned_scenarios_and_pairs() {
    let registry = Registry::parse(
        "registry.toml",
        "[[assignment]]\nscenario = \"palette\"\nstep = \"P1.6\"\n\n[[assignment]]\nplugin = \"agent\"\nzone = \"right\"\nstep = \"P3.1\"\n",
    )
    .unwrap();
    let inventory = Inventory {
        prototype: "x".to_owned(),
        scenarios: vec!["palette".to_owned(), "narrow".to_owned()],
        plugins: vec![InventoryPlugin {
            id: "agent".to_owned(),
            placements: vec![Zone::Right, Zone::Center],
        }],
    };
    let problems = completeness(&registry, &inventory, &BTreeSet::new());
    assert_eq!(problems.len(), 2);
    assert!(problems[0].contains("narrow"));
    assert!(problems[1].contains("agent × center"));
    let done = BTreeSet::from(["P1.6".to_owned()]);
    let problems = completeness(&registry, &inventory, &done);
    assert!(problems.iter().any(|p| p.contains("P1.6 готов")));
}

/// Правило готового шага судится по назначению: состояние того же шага не пропускает новое
/// назначение без оси и без рецептов состояний.
#[test]
fn finished_step_rule_is_judged_per_assignment() {
    let state = "[[state]]\nid = \"layout-default\"\nstep = \"P1.4\"\nsurface = \"layout\"\nscene = \"p15\"\nrecipe = \"layout-default\"\n";
    let rows =
        |extra: &str| Registry::parse("registry.toml", &format!("{state}\n{extra}")).unwrap();
    let inventory = Inventory {
        prototype: "x".to_owned(),
        scenarios: Vec::new(),
        plugins: Vec::new(),
    };
    let done = BTreeSet::from(["P1.3".to_owned(), "P1.4".to_owned()]);
    let passing = rows(
        "[[assignment]]\nscenario = \"layout\"\nstep = \"P1.4\"\nrecipes = [\"layout-default\"]\n\n\
         [[assignment]]\nscenario = \"light\"\nstep = \"P1.3\"\naxis = \"theme\"\n\n\
         [[assignment]]\nscenario = \"palette\"\nstep = \"P1.6\"\n",
    );
    assert_eq!(
        completeness(&passing, &inventory, &done),
        Vec::<String>::new()
    );

    let bare = rows("[[assignment]]\nscenario = \"fresh\"\nstep = \"P1.4\"\n");
    let problems = completeness(&bare, &inventory, &done);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("P1.4 готов") && problems[0].contains("fresh"));

    let unknown =
        rows("[[assignment]]\nscenario = \"fresh\"\nstep = \"P1.4\"\nrecipes = [\"nowhere\"]\n");
    let problems = completeness(&unknown, &inventory, &done);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("nowhere"));
}

#[test]
fn done_steps_are_read_from_roadmap_tables() {
    let roadmap = "| Готов | ID | slug |\n| :---: | --- | --- |\n| ✅ | P1.3 | `a` |\n| ⬜ | P1.6 | `b` |\n\
                   |✅|P0.1|`c`|\n- ✅ P9.9 в тексте\n| ✅ | не шаг | `d` |\n";
    assert_eq!(
        done_steps(roadmap),
        BTreeSet::from(["P0.1".to_owned(), "P1.3".to_owned()])
    );
}

#[test]
fn recipe_steps_move_and_focus_parse() {
    let recipe = Recipe::parse(
        "r.json",
        r#"{"scene": "p15", "steps": [
            {"op": "move", "zone": "right", "part": 1, "plugin": "terminal", "to": "center", "new_part": true},
            {"op": "move", "zone": "right", "part": 1, "plugin": "terminal", "to": "left"},
            {"op": "focus", "zone": "center", "part": 1}],
           "post": {"parts": {"center": 1}}}"#,
    )
    .unwrap();
    assert_eq!(
        recipe.steps,
        vec![
            Step::Move {
                zone: Zone::Right,
                part: 1,
                plugin: "terminal".to_owned(),
                to: Zone::Center,
                new_part: true,
            },
            Step::Move {
                zone: Zone::Right,
                part: 1,
                plugin: "terminal".to_owned(),
                to: Zone::Left,
                new_part: false,
            },
            Step::Focus {
                zone: Zone::Center,
                part: 1,
            },
        ]
    );
}

#[test]
fn key_templates_accept_semantic_paths() {
    for key in [
        "chrome.search",
        "chrome.search.kbd",
        "chrome.zones.left",
        "chrome.window.close",
        "zone.right.part.1.tabs.tab.2.close",
        "zone.center.part.2.tabs.zoom",
        "zone.bottom.part.10.menu.item.3.label",
        "status.mode",
        "zone.center.part.1::after",
        "zone.center.part.1.tabs.tab.1::after",
        "chrome.zones.right::before",
        "picker.item.4.label",
    ] {
        assert!(check_key(key).is_ok(), "{key}");
    }
    assert_eq!(
        template_of("zone.right.part.1.tabs.tab.2.close"),
        Some("zone.{zone}.part.{n}.tabs.tab.{n}.close")
    );
}

#[test]
fn key_templates_reject_names_ids_and_unknown_paths() {
    for key in [
        "",
        "chrome.search.Терминал",
        "zone.top.part.1",
        "zone.right.part.0",
        "zone.right.part.01",
        "zone.right.part.g12",
        "zone.right.part.1.tabs.tab.terminal",
        "chrome.window.fullscreen",
        "chrome.search::marker",
        "chrome.search.",
        "status.mode.extra.deep",
        "zone.center.part.1.veil::after",
        "picker.item.4.icon",
    ] {
        assert!(check_key(key).is_err(), "{key}");
    }
}

#[test]
fn keys_are_unique() {
    assert!(check_unique(["chrome", "chrome.search"]).is_ok());
    assert!(
        check_unique(["chrome.search", "chrome", "chrome.search"])
            .unwrap_err()
            .contains("chrome.search")
    );
}

#[test]
fn exception_globs_match_one_or_more_segments() {
    assert!(glob_matches("chrome.window.*", "chrome.window.close"));
    assert!(!glob_matches("chrome.window.*", "chrome.window"));
    assert!(glob_matches(
        "*.menu.item.*",
        "zone.right.part.1.menu.item.2"
    ));
    assert!(glob_matches(
        "*.menu.item.*",
        "zone.right.part.1.menu.item.2.label"
    ));
    assert!(glob_matches(
        "*.tabs.tab.*.icon",
        "zone.center.part.1.tabs.tab.2.icon"
    ));
    assert!(!glob_matches(
        "*.tabs.tab.*.icon",
        "zone.center.part.1.tabs.tab.2.close"
    ));
    assert!(check_glob("*.menu.item.*").is_ok());
    assert!(check_glob("chrome.win*").is_err());
    assert!(check_glob("chrome..x").is_err());
}

#[test]
fn field_contract_requires_and_forbids_by_kind() {
    let mut text = element("status.message", ElementKind::Text);
    assert!(check_element(&text).unwrap_err().contains("Text"));
    text.text = Some("Готово".to_owned());
    text.truncated = Some(false);
    assert!(check_element(&text).unwrap_err().contains("Font"));

    let mut container = element("chrome", ElementKind::Container);
    assert!(check_element(&container).is_ok());
    container.radius = Some(4.0);
    assert!(
        check_element(&container)
            .unwrap_err()
            .contains("не применяется")
    );

    let control = element("chrome.search", ElementKind::Control);
    assert!(check_element(&control).unwrap_err().contains("LineWidth"));

    let pseudo = element("chrome.search", ElementKind::Pseudo);
    assert!(check_element(&pseudo).unwrap_err().contains("pseudo"));
}

#[test]
fn passport_rejects_duplicate_and_unknown_keys() {
    let (_dir, inputs, _registry) = repo();
    let path = inputs.passport_path("chrome-titlebar", Cell::new(Theme::Dark, Contrast::Normal));
    let mut passport = Passport::parse("p", &fs::read_to_string(&path).unwrap()).unwrap();
    passport
        .elements
        .push(element("chrome", ElementKind::Container));
    assert!(passport.check().unwrap_err().contains("повторяется"));
    passport.elements.pop();
    passport
        .elements
        .push(element("chrome.terminal", ElementKind::Container));
    assert!(passport.check().unwrap_err().contains("таблице шаблонов"));
}

#[test]
fn recipe_postconditions_check_composition() {
    let scene = Scene::parse("s", SCENE).unwrap();
    let recipe = Recipe::parse("r", RECIPE).unwrap();
    assert_eq!(recipe.window.width, 1180);
    assert_eq!(recipe.window.dpr, 2);
    let mut observed = scene.composition();
    assert!(recipe.check_post(&scene, &observed).is_empty());
    observed.zones.remove(&Zone::Left);
    let failures = recipe.check_post(&scene, &observed);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("зона left"));

    let wrong = Recipe::parse("r", r#"{"scene": "s", "post": {"parts": {"center": 2}}}"#).unwrap();
    assert!(wrong.expected(&scene).unwrap_err().contains("число частей"));

    let dark = Recipe::parse(
        "r",
        r#"{"scene": "s", "theme": "dark", "post": {"parts": {}}}"#,
    )
    .unwrap();
    assert!(dark.allows(Cell::new(Theme::Dark, Contrast::High)));
    assert!(!dark.allows(Cell::new(Theme::Light, Contrast::Normal)));
}

#[test]
fn recipe_composition_detects_swapped_tabs() {
    let scene = Scene::parse("s", SCENE).unwrap();
    let mut want = scene.composition();
    want.zones.get_mut(&Zone::Center).unwrap()[0].tabs =
        vec!["terminal".to_owned(), "navigator".to_owned()];
    let mut got = want.clone();
    got.zones.get_mut(&Zone::Center).unwrap()[0].tabs.reverse();
    let diff = want.diff(&got);
    assert_eq!(diff.len(), 1);
    assert!(diff[0].contains("вкладки"));
}

#[test]
fn scene_rejects_inconsistent_inputs() {
    let unknown = SCENE.replace("{\"plugin\": \"terminal\"", "{\"plugin\": \"agent\"");
    assert!(
        Scene::parse("s", &unknown)
            .unwrap_err()
            .reason
            .contains("agent")
    );
    let no_active = SCENE.replace(
        "\"plugin\": \"terminal\", \"active\": true",
        "\"plugin\": \"terminal\"",
    );
    assert!(
        Scene::parse("s", &no_active)
            .unwrap_err()
            .reason
            .contains("активная")
    );
    let focus = SCENE.replace("\"part\": 1", "\"part\": 2");
    assert!(
        Scene::parse("s", &focus)
            .unwrap_err()
            .reason
            .contains("фокус")
    );
}

#[test]
fn plans_without_conformance_section_are_named() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("a.md"),
        "# A\n\n## Сверка с прототипом\n\nUI-результата нет.\n",
    );
    write(&dir.path().join("b.md"), "# B\n\n### Сверка с прототипом\n");
    write(&dir.path().join("completed/c.md"), "# C\n");
    let missing = plans::missing_section(dir.path()).unwrap();
    assert_eq!(missing, vec![dir.path().join("b.md")]);
}

#[test]
fn scene_cardinality_uses_product_names() {
    for name in [
        "multi",
        "per_workbench",
        "per_project",
        "per_workspace",
        "per_app",
    ] {
        let text = SCENE.replace("\"multi\"", &format!("\"{name}\""));
        assert!(Scene::parse("s", &text).is_ok(), "{name}");
    }
    assert!(Scene::parse("s", &SCENE.replace("\"multi\"", "\"Multi\"")).is_err());
}

#[test]
fn composition_lists_titlebar_and_tab_strip_controls() {
    let scene = Scene::parse("s", SCENE).unwrap();
    let want = scene.composition();
    assert_eq!(
        want.titlebar.first().map(String::as_str),
        Some("chrome.window.close")
    );
    for key in &want.titlebar {
        check_key(key).unwrap();
    }
    let mut observed = want.clone();
    observed.titlebar.retain(|k| k != "chrome.search");
    assert!(want.diff(&observed).iter().any(|d| d.contains("заголовка")));
    let mut observed = want.clone();
    if let Some(parts) = observed.zones.get_mut(&Zone::Center) {
        parts[0].tools.reverse();
    }
    assert!(
        want.diff(&observed)
            .iter()
            .any(|d| d.contains("строки вкладок"))
    );
    let mut observed = want.clone();
    observed.statusline.push("status.mode".to_owned());
    assert!(
        want.diff(&observed)
            .iter()
            .any(|d| d.contains("строки статуса"))
    );
}

#[test]
fn state_scene_must_match_recipe_scene() {
    let (_dir, inputs, mut registry) = repo();
    registry.states[0].scene = "other".to_owned();
    let err = check_registry(&inputs, &registry).unwrap_err();
    assert!(err.reason.contains("other"), "{err}");
}

#[test]
fn state_cells_must_fit_recipe_axes() {
    let (dir, inputs, registry) = repo();
    write(
        &dir.path().join("conformance/recipes/r.json"),
        r#"{"scene": "s", "theme": "dark", "post": {"parts": {"left": 1, "center": 1}}}"#,
    );
    let err = check_registry(&inputs, &registry).unwrap_err();
    assert!(err.reason.contains("light-high"), "{err}");
}

#[test]
fn judgement_rows_cover_fields_appkit_does_not_carry() {
    use shellio_conformance::fields::{Field, Judgement, absent_value, judgement};
    use shellio_conformance::passport::{Rgba, Shadow};

    let ink = Rgba {
        r: 1,
        g: 2,
        b: 3,
        a: 1.0,
    };
    let shadow = |spread: f64, blur: f64, inset: bool| Shadow {
        color: ink,
        x: 0.0,
        y: 0.0,
        blur,
        spread,
        inset,
    };

    let mut button = element("chrome.appearance", ElementKind::Control);
    button.color = Some(ink);
    button.radius = Some(5.0);
    button.line_width = Some(0.0);
    assert_eq!(judgement(&button, Field::Padding), Judgement::ByFrames);
    assert_eq!(judgement(&button, Field::Color), Judgement::ByChildren);
    assert_eq!(judgement(&button, Field::Radius), Judgement::NotDrawn);
    button.fill = Some(ink);
    assert_eq!(judgement(&button, Field::Radius), Judgement::ByRule);
    assert_eq!(judgement(&button, Field::Fill), Judgement::ByRule);
    button.text = Some("Развернуть".to_owned());
    assert_eq!(judgement(&button, Field::Color), Judgement::ByRule);

    let mut label = element("chrome.search.label", ElementKind::Text);
    label.color = Some(ink);
    label.line_height = Some(18.0);
    label.frame.h = 18.0;
    assert_eq!(judgement(&label, Field::LineHeight), Judgement::ByFrames);
    assert_eq!(judgement(&label, Field::Color), Judgement::ByRule);
    label.frame.h = 35.9;
    assert_eq!(judgement(&label, Field::LineHeight), Judgement::ByFrames);
    label.frame.h = 36.0;
    assert_eq!(judgement(&label, Field::LineHeight), Judgement::ByRule);

    let mut tab = element("zone.center.part.1.tabs.tab.1", ElementKind::Tab);
    tab.shadow = vec![shadow(1.0, 0.0, false)];
    assert_eq!(judgement(&tab, Field::Shadow), Judgement::RingAsLine);
    assert_eq!(judgement(&tab, Field::Radius), Judgement::ByRule);
    tab.shadow = vec![shadow(0.5, 0.0, true)];
    assert_eq!(judgement(&tab, Field::Shadow), Judgement::RingAsLine);
    tab.shadow = vec![shadow(1.0, 4.0, false)];
    assert_eq!(judgement(&tab, Field::Shadow), Judgement::ByRule);
    tab.shadow = vec![shadow(0.0, 0.0, false)];
    assert_eq!(judgement(&tab, Field::Shadow), Judgement::ByRule);

    let mut close = element("chrome.window.close", ElementKind::Control);
    close.fill = Some(ink);
    close.shadow = vec![shadow(0.5, 0.0, true)];
    assert_eq!(judgement(&close, Field::Fill), Judgement::BySnapshot);
    assert_eq!(judgement(&close, Field::Shadow), Judgement::SystemDetail);
    assert_eq!(judgement(&close, Field::Frame), Judgement::ByRule);

    let mut chevron = element("chrome.workspace.chevron", ElementKind::Icon);
    chevron.color = Some(ink);
    assert_eq!(judgement(&chevron, Field::Color), Judgement::BySnapshot);
    let mut glass = element("chrome.search.icon", ElementKind::Icon);
    glass.color = Some(ink);
    assert_eq!(judgement(&glass, Field::Color), Judgement::ByRule);

    assert_eq!(absent_value(Field::LineWidth), Some(0.0));
    assert_eq!(absent_value(Field::Radius), Some(0.0));
    assert_eq!(absent_value(Field::Fill), None);
}

#[test]
fn recipe_keyboard_steps_parse() {
    let recipe = Recipe::parse(
        "r.json",
        r#"{"scene": "p15", "steps": [
            {"op": "key", "keys": "Ctrl-w v"},
            {"op": "keymap", "text": "[navigation]\n\"Ctrl-w m\" = \"layout.zoom\"\n",
             "commands": {"zoom": ["Ctrl-w m"], "split.v": []}},
            {"op": "keymap", "text": ""},
            {"op": "focus_field", "zone": "center", "part": 1},
            {"op": "filter", "text": "два слова"}],
           "post": {"parts": {"center": 1}}}"#,
    )
    .unwrap();
    assert_eq!(
        recipe.steps,
        vec![
            Step::Key {
                keys: "Ctrl-w v".to_owned(),
            },
            Step::Keymap {
                text: "[navigation]\n\"Ctrl-w m\" = \"layout.zoom\"\n".to_owned(),
                commands: BTreeMap::from([
                    ("split.v".to_owned(), Vec::new()),
                    ("zoom".to_owned(), vec!["Ctrl-w m".to_owned()]),
                ]),
            },
            Step::Keymap {
                text: String::new(),
                commands: BTreeMap::new(),
            },
            Step::FocusField {
                zone: Zone::Center,
                part: 1,
            },
            Step::Filter {
                text: "два слова".to_owned(),
            },
        ]
    );
    let ops: Vec<_> = recipe.steps.iter().map(Step::op).collect();
    assert_eq!(ops, ["key", "keymap", "keymap", "focus_field", "filter"]);
    // Имя шага на проводе совпадает с `op` записи, запись читается обратно без потерь.
    for step in &recipe.steps {
        let wire = serde_json::to_value(step).unwrap();
        assert_eq!(wire["op"], step.op());
        assert_eq!(&serde_json::from_value::<Step>(wire).unwrap(), step);
    }
}

#[test]
fn recipe_rejects_malformed_keyboard_steps() {
    let parse = |step: &str| {
        Recipe::parse(
            "r.json",
            &format!(r#"{{"scene": "p15", "steps": [{step}], "post": {{"parts": {{}}}}}}"#),
        )
    };
    for (step, word) in [
        (r#"{"op": "key", "keys": ""}"#, "key"),
        (r#"{"op": "key", "keys": "Ctrl-w  v"}"#, "key"),
        (r#"{"op": "key"}"#, "keys"),
        (r#"{"op": "key", "keys": "j", "zone": "left"}"#, "zone"),
        (
            r#"{"op": "keymap", "text": "", "commands": {"zoom": [""]}}"#,
            "zoom",
        ),
        (
            r#"{"op": "keymap", "text": "", "commands": {"": ["j"]}}"#,
            "keymap",
        ),
        (r#"{"op": "keymap", "commands": {}}"#, "text"),
        (r#"{"op": "focus_field", "zone": "center"}"#, "part"),
        (r#"{"op": "filter"}"#, "text"),
        (r#"{"op": "type", "text": "x"}"#, "type"),
    ] {
        let reason = parse(step).unwrap_err().reason;
        assert!(reason.contains(word), "{step}: {reason}");
    }
}

const P15: &str = include_str!("../../../conformance/scenes/p15.json");

#[test]
fn scene_reads_capabilities_absent_commands_and_right_summaries() {
    let scene = Scene::parse("p15.json", P15).unwrap();
    let caps = |id: &str| {
        scene
            .catalog
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.capabilities.clone())
            .unwrap()
    };
    // Заполнитель терминала ввода не принимает, навигатор списка не объявляет.
    assert_eq!(caps("terminal"), Capabilities::default());
    assert_eq!(caps("navigator"), Capabilities::default());
    assert_eq!(
        caps("demo-list"),
        Capabilities {
            list_keys: true,
            filter: true,
            ..Capabilities::default()
        }
    );
    assert_eq!(
        caps("demo-native"),
        Capabilities {
            input: true,
            ..Capabilities::default()
        }
    );
    assert_eq!(scene.commands_absent, ["palette"]);
    // Плашка режима есть в строке статуса любой сцены.
    assert_eq!(scene.composition().statusline, ["status.mode"]);

    let mut value: serde_json::Value = serde_json::from_str(P15).unwrap();
    value["status"] = serde_json::json!({"load": "3% CPU · 210 МБ"});
    value["catalog"][0]["capabilities"] = serde_json::json!({
        "input": true,
        "hints": [{"key": "Enter", "label": "открыть"}]
    });
    let scene = Scene::parse("s", &value.to_string()).unwrap();
    assert_eq!(scene.status.load.as_deref(), Some("3% CPU · 210 МБ"));
    assert_eq!(
        scene.composition().statusline,
        ["status.mode", "status.load"]
    );
    assert_eq!(
        scene.catalog[0].capabilities.hints,
        [StatusHint {
            key: "Enter".to_owned(),
            label: "открыть".to_owned(),
        }]
    );
    // Запись без возможностей и без отсутствующих действий остаётся прежней сценой P1.5.
    let back = serde_json::to_value(Scene::parse("s", SCENE).unwrap()).unwrap();
    assert!(back.get("commands_absent").is_none());
    assert!(back["catalog"][0].get("capabilities").is_none());
}

#[test]
fn scene_rejects_bad_capabilities_and_absent_commands() {
    let with = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut value: serde_json::Value = serde_json::from_str(P15).unwrap();
        edit(&mut value);
        Scene::parse("s", &value.to_string()).unwrap_err().reason
    };
    let hint = serde_json::json!({"key": "j", "label": "вниз"});
    let reason = with(&|v| {
        v["catalog"][0]["capabilities"] = serde_json::json!({"hints": [hint, hint, hint, hint]});
    });
    assert!(reason.contains("подсказок"), "{reason}");
    let reason = with(&|v| v["catalog"][0]["capabilities"] = serde_json::json!({"typing": true}));
    assert!(reason.contains("typing"), "{reason}");
    let reason = with(&|v| v["commands_absent"] = serde_json::json!(["palette", "palette"]));
    assert!(reason.contains("palette"), "{reason}");
    let reason = with(&|v| v["commands_absent"] = serde_json::json!([" "]));
    assert!(reason.contains("commands_absent"), "{reason}");
}
