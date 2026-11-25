//! Проверки репозитория без браузера: раздел «Сверка с прототипом» в незавершённых планах,
//! годность паспортов каждого состояния реестра и файлы шрифта прототипа против ADR-0022.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use shellio_conformance::plans;
use shellio_conformance::recipe::Recipe;
use shellio_conformance::registry::Registry;
use shellio_conformance::scene::Scene;
use shellio_conformance::validity::{Inputs, check_registry, sha256_hex};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn unfinished_plans_have_conformance_section() {
    let missing = plans::missing_section(&root().join("docs/plans")).unwrap();
    assert!(
        missing.is_empty(),
        "планы без раздела «{}»: {missing:?}",
        plans::SECTION
    );
}

#[test]
fn registry_passports_are_valid() {
    let root = root();
    let path = root.join("conformance/registry.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let registry = Registry::parse(&path.display().to_string(), &text).unwrap();
    assert!(!registry.states.is_empty(), "в реестре нет состояний");
    let inputs = Inputs::from_root(&root).unwrap();
    let findings = check_registry(&inputs, &registry).unwrap();
    let report: Vec<String> = findings.iter().map(ToString::to_string).collect();
    assert!(
        report.is_empty(),
        "паспорта требуют пересъёма (make passports):\n{}",
        report.join("\n")
    );
}

/// Каждая сцена и каждый рецепт каталога сверки разбираются, а рецепт ссылается на сцену, с
/// которой сходится его постусловие.
#[test]
fn scenes_and_recipes_parse() {
    let root = root();
    let read = |path: &Path| std::fs::read_to_string(path).unwrap();
    let mut recipes = 0;
    for entry in std::fs::read_dir(root.join("conformance/recipes")).unwrap() {
        let path = entry.unwrap().path();
        let place = path.display().to_string();
        let recipe = Recipe::parse(&place, &read(&path)).unwrap();
        let scene_path = root.join(format!("conformance/scenes/{}.json", recipe.scene));
        let scene = Scene::parse(&scene_path.display().to_string(), &read(&scene_path)).unwrap();
        recipe.expected(&scene).unwrap();
        recipes += 1;
    }
    assert!(recipes > 0, "в conformance/recipes нет рецептов");
}

#[test]
fn icon_threshold_matches_calibration_controls() {
    let text =
        std::fs::read_to_string(root().join("conformance/calibration/controls.json")).unwrap();
    let controls: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        controls["icon_fg"].as_u64(),
        Some(u64::from(shellio_conformance::mask::ICON_FG))
    );
}

/// Строки таблицы файлов закреплённого выпуска JetBrains Mono из ADR-0022: имя файла без каталога
/// архива → длина и SHA-256.
fn pinned_fonts(adr: &str) -> BTreeMap<String, (u64, String)> {
    let mut pinned = BTreeMap::new();
    for line in adr.lines() {
        let cells: Vec<&str> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().trim_matches('`'))
            .collect();
        let [path, _, len, sum] = cells[..] else {
            continue;
        };
        let (Ok(len), true) = (len.parse::<u64>(), sum.len() == 64) else {
            continue;
        };
        let name = path.rsplit('/').next().unwrap_or(path);
        assert!(
            pinned
                .insert(name.to_owned(), (len, sum.to_owned()))
                .is_none(),
            "ADR-0022 называет {name} дважды"
        );
    }
    pinned
}

/// `prototype/fonts/` — ровно файлы закреплённого выпуска из ADR-0022, без правок: бандл сверяет
/// свои шрифты с этим каталогом.
#[test]
fn prototype_fonts_are_the_pinned_release() {
    let root = root();
    let adr = std::fs::read_to_string(root.join("docs/adr/0022-licensing-and-borrowing.md"));
    let pinned = pinned_fonts(&adr.unwrap());
    assert_eq!(
        pinned.len(),
        5,
        "в ADR-0022 ждали четыре начертания и лицензию"
    );
    let mut found = BTreeMap::new();
    for entry in std::fs::read_dir(root.join("prototype/fonts")).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        if name == ".DS_Store" {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        found.insert(name, (bytes.len() as u64, sha256_hex(&bytes)));
    }
    assert_eq!(found, pinned);
}

#[test]
fn pinned_font_table_is_read_by_columns() {
    let sum = "a".repeat(64);
    let adr = format!(
        "| Файл архива | Начертание | Байт | SHA-256 |\n| --- | --- | --- | --- |\n\
         | `fonts/ttf/A-Regular.ttf` | 400 | 12 | `{sum}` |\n| `OFL.txt` | лицензия | 7 | `{sum}` |\n\
         | `other` | 400 | много | `{sum}` |\n| `short` | 400 | 3 | `abc` |\n"
    );
    let pinned = pinned_fonts(&adr);
    assert_eq!(
        pinned.keys().map(String::as_str).collect::<Vec<_>>(),
        ["A-Regular.ttf", "OFL.txt"]
    );
    assert_eq!(pinned["A-Regular.ttf"], (12, sum));
}
