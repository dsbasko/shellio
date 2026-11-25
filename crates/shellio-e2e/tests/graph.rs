//! Граф зависимостей оракула: он судит продукт независимо и потому не зависит ни от темы, ни от
//! кода, которым продукт рисует (ADR-0037, «Режим сверки оракула»).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// Имена всех пакетов, от которых транзитивно зависит `package`.
fn closure(metadata: &Value, package: &str) -> BTreeSet<String> {
    let names: BTreeMap<&str, &str> = metadata["packages"]
        .as_array()
        .unwrap_or_else(|| panic!("packages"))
        .iter()
        .filter_map(|p| Some((p["id"].as_str()?, p["name"].as_str()?)))
        .collect();
    let edges: BTreeMap<&str, Vec<&str>> = metadata["resolve"]["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("resolve.nodes"))
        .iter()
        .filter_map(|n| {
            let deps = n["dependencies"].as_array()?;
            Some((
                n["id"].as_str()?,
                deps.iter().filter_map(Value::as_str).collect(),
            ))
        })
        .collect();
    let start = names
        .iter()
        .find(|(_, name)| **name == package)
        .map(|(id, _)| *id)
        .unwrap_or_else(|| panic!("пакет в графе"));
    let mut seen = BTreeSet::new();
    let mut queue = vec![start];
    while let Some(id) = queue.pop() {
        for dep in edges.get(id).into_iter().flatten() {
            if seen.insert(*dep) {
                queue.push(dep);
            }
        }
    }
    seen.into_iter()
        .filter_map(|id| names.get(id).map(|name| (*name).to_owned()))
        .collect()
}

#[test]
fn oracle_does_not_depend_on_theme_or_appkit() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--offline", "--locked"])
        .current_dir(root)
        .output()
        .expect("cargo metadata запускается");
    assert!(
        output.status.success(),
        "cargo metadata: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("JSON графа");
    let deps = closure(&metadata, "shellio-e2e");
    assert!(deps.contains("shellio-conformance"), "{deps:?}");
    for forbidden in [
        "shellio-theme",
        "shellio-appkit",
        "shellio-ghostty",
        "objc2",
    ] {
        assert!(!deps.contains(forbidden), "оракул зависит от {forbidden}");
    }
}

#[test]
fn closure_follows_transitive_edges() {
    let metadata = serde_json::json!({
        "packages": [
            {"id": "a 1", "name": "a"},
            {"id": "b 1", "name": "b"},
            {"id": "c 1", "name": "c"},
            {"id": "d 1", "name": "d"}
        ],
        "resolve": {"nodes": [
            {"id": "a 1", "dependencies": ["b 1"]},
            {"id": "b 1", "dependencies": ["c 1"]},
            {"id": "c 1", "dependencies": []},
            {"id": "d 1", "dependencies": ["a 1"]}
        ]}
    });
    let deps: Vec<String> = closure(&metadata, "a").into_iter().collect();
    assert_eq!(deps, ["b", "c"]);
}
