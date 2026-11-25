//! Проверки репозитория без браузера: раздел «Сверка с прототипом» в незавершённых планах и
//! годность паспортов каждого состояния реестра.

use std::path::{Path, PathBuf};

use shellio_conformance::plans;
use shellio_conformance::registry::Registry;
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
    // Реестр появляется с первым состоянием; без него паспортов, которые нужно проверять, нет.
    let registry = match std::fs::read_to_string(&path) {
        Ok(text) => Registry::parse(&path.display().to_string(), &text).unwrap(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Registry::default(),
        Err(e) => panic!("{}: {e}", path.display()),
    };
    let inputs = Inputs::from_root(&root).unwrap();
    let findings = check_registry(&inputs, &registry).unwrap();
    let report: Vec<String> = findings.iter().map(ToString::to_string).collect();
    assert!(
        report.is_empty(),
        "паспорта требуют пересъёма (make passports):\n{}",
        report.join("\n")
    );
}
