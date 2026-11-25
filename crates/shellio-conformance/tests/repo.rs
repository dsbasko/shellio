//! Проверки репозитория без браузера: раздел «Сверка с прототипом» в незавершённых планах и
//! годность паспортов каждого состояния реестра.

use std::path::{Path, PathBuf};

use shellio_conformance::plans;
use shellio_conformance::recipe::Recipe;
use shellio_conformance::registry::Registry;
use shellio_conformance::scene::Scene;
use shellio_conformance::validity::{Inputs, check_registry};

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
