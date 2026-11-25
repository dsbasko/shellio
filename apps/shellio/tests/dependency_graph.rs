//! Граф зависимостей workspace (ADR-0008, «Уточнено в P1.5 (планирование)»; `docs/architecture.md`,
//! «Правила зависимостей»): чистые crate не доходят до AppKit, контракта вида и плагинов, плагин не
//! доходит до другого плагина, до ядра и до приложений.
//!
//! Граф берётся из `cargo metadata --offline --locked`: обычные и сборочные зависимости для всех
//! целей, без зависимостей тестов. У crate workspace к рёбрам разрешённого графа добавляются
//! объявленные зависимости, в том числе необязательные: cargo-функция, которая подключила бы AppKit
//! к чистому crate, тоже нарушение.

// allow-unwrap-in-tests не распространяется на вспомогательные функции интеграционного теста.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use serde_json::Value;

/// Чистые crate: без AppKit, контракта вида и плагинов.
const PURE: [&str; 7] = [
    "shellio-plugin-api",
    "shellio-layout",
    "shellio-core",
    "shellio-store",
    "shellio-control",
    "shellio-theme",
    "shellio-keymap",
];

const APPKIT: &str = "shellio-appkit";
const PLUGIN_VIEW: &str = "shellio-plugin-view";
const PLUGIN_API: &str = "shellio-plugin-api";
const CORE: &str = "shellio-core";
const APP: &str = "shellio";
const KEYMAP: &str = "shellio-keymap";

struct Package {
    name: String,
    manifest: PathBuf,
}

struct Graph {
    root: PathBuf,
    packages: BTreeMap<String, Package>,
    members: BTreeSet<String>,
    edges: BTreeMap<String, BTreeSet<String>>,
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("в cargo metadata нет строки «{key}»: {value}"))
}

fn list<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value[key]
        .as_array()
        .unwrap_or_else(|| panic!("в cargo metadata нет списка «{key}»"))
}

/// Зависимость тестов: в продукт она не попадает.
fn is_dev(kind: &Value) -> bool {
    kind.as_str() == Some("dev")
}

impl Graph {
    fn of(metadata: &Value) -> Self {
        let packages: BTreeMap<String, Package> = list(metadata, "packages")
            .iter()
            .map(|package| {
                (
                    text(package, "id").to_owned(),
                    Package {
                        name: text(package, "name").to_owned(),
                        manifest: PathBuf::from(text(package, "manifest_path")),
                    },
                )
            })
            .collect();
        let members: BTreeSet<String> = list(metadata, "workspace_members")
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect();

        let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for node in list(&metadata["resolve"], "nodes") {
            let from = edges.entry(text(node, "id").to_owned()).or_default();
            for dep in list(node, "deps") {
                if list(dep, "dep_kinds")
                    .iter()
                    .any(|kind| !is_dev(&kind["kind"]))
                {
                    from.insert(text(dep, "pkg").to_owned());
                }
            }
        }
        for package in list(metadata, "packages") {
            let id = text(package, "id");
            if !members.contains(id) {
                continue;
            }
            let from = edges.entry(id.to_owned()).or_default();
            for dep in list(package, "dependencies") {
                if is_dev(&dep["kind"]) {
                    continue;
                }
                let name = text(dep, "name");
                from.extend(
                    packages
                        .iter()
                        .filter(|(_, package)| package.name == name)
                        .map(|(id, _)| id.clone()),
                );
            }
        }

        Self {
            root: PathBuf::from(text(metadata, "workspace_root")),
            packages,
            members,
            edges,
        }
    }

    fn name(&self, id: &str) -> &str {
        &self.packages[id].name
    }

    /// Crate workspace с таким именем.
    fn member(&self, name: &str) -> &str {
        let found: Vec<&String> = self
            .members
            .iter()
            .filter(|id| self.name(id) == name)
            .collect();
        match found.as_slice() {
            [id] => id,
            _ => panic!("crate «{name}» в workspace: найдено {}", found.len()),
        }
    }

    /// Crate workspace в каталоге `dir` корня: `plugins`, `apps`.
    fn members_under(&self, dir: &str) -> Vec<&str> {
        let dir = self.root.join(dir);
        self.members
            .iter()
            .filter(|id| self.packages[*id].manifest.starts_with(&dir))
            .map(String::as_str)
            .collect()
    }

    /// Всё, до чего доходит `from`, с предыдущим звеном пути.
    fn reach<'a>(&'a self, from: &'a str) -> BTreeMap<&'a str, &'a str> {
        let mut seen = BTreeMap::new();
        let mut queue = VecDeque::from([from]);
        while let Some(id) = queue.pop_front() {
            for next in self.edges.get(id).into_iter().flatten() {
                if next != from && !seen.contains_key(next.as_str()) {
                    seen.insert(next.as_str(), id);
                    queue.push_back(next);
                }
            }
        }
        seen
    }

    /// Пути от `from` до каждого пакета, который `forbidden` запрещает: «a → b → c».
    fn violations(&self, from: &str, forbidden: impl Fn(&str) -> bool) -> Vec<String> {
        let reach = self.reach(from);
        reach
            .keys()
            .filter(|id| forbidden(id))
            .map(|&id| {
                let mut chain = vec![self.name(id)];
                let mut at = id;
                while let Some(&previous) = reach.get(at) {
                    chain.push(self.name(previous));
                    at = previous;
                }
                chain.reverse();
                chain.join(" → ")
            })
            .collect()
    }

    fn reaches(&self, from: &str, name: &str) -> bool {
        self.reach(from).keys().any(|id| self.name(id) == name)
    }
}

/// `cargo metadata` с чистым окружением: cargo, `rustc` рядом с ним и каталоги инструментов.
fn metadata() -> Value {
    let cargo = Path::new(env!("CARGO"));
    let mut command = Command::new(cargo);
    command
        .args(["metadata", "--offline", "--locked", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_clear();
    if let Some(bin) = cargo.parent() {
        command.env("PATH", bin);
    }
    for key in ["HOME", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "cargo metadata: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn workspace() -> &'static Graph {
    static GRAPH: OnceLock<Graph> = OnceLock::new();
    GRAPH.get_or_init(|| Graph::of(&metadata()))
}

fn plugins(graph: &Graph) -> Vec<&str> {
    let plugins = graph.members_under("plugins");
    let names: BTreeSet<&str> = plugins.iter().map(|id| graph.name(id)).collect();
    assert!(
        names.is_superset(&BTreeSet::from(["plugin-navigator", "plugin-terminal"])),
        "плагины workspace: {names:?}"
    );
    plugins
}

#[test]
fn pure_crates_do_not_reach_appkit_the_view_contract_or_plugins() {
    let graph = workspace();
    let plugins = plugins(graph);
    let forbidden = |id: &str| {
        let name = graph.name(id);
        name == APPKIT || name == PLUGIN_VIEW || name.starts_with("objc2") || plugins.contains(&id)
    };
    for name in PURE {
        let found = graph.violations(graph.member(name), forbidden);
        assert!(found.is_empty(), "чистый crate «{name}»: {found:#?}");
    }
}

#[test]
fn plugins_do_not_reach_each_other_the_core_or_apps() {
    let graph = workspace();
    let plugins = plugins(graph);
    let apps = graph.members_under("apps");
    assert!(apps.contains(&graph.member(APP)), "приложения: {apps:?}");
    for &plugin in &plugins {
        let forbidden =
            |id: &str| plugins.contains(&id) || apps.contains(&id) || graph.name(id) == CORE;
        let found = graph.violations(plugin, forbidden);
        assert!(
            found.is_empty(),
            "плагин «{}»: {found:#?}",
            graph.name(plugin)
        );
    }
}

/// Модель клавиатуры не доходит ни до одного crate workspace — ни до ядра, ни до контракта плагина
/// — и зависит только от `serde`, `serde_json` и `toml` (ADR-0010, «Уточнено в P1.6
/// (планирование)», «Аккорд и модель»).
#[test]
fn keymap_reaches_no_workspace_crate() {
    let graph = workspace();
    let keymap = graph.member(KEYMAP);
    let found = graph.violations(keymap, |id| graph.members.contains(id));
    assert!(found.is_empty(), "crate «{KEYMAP}»: {found:#?}");

    let direct: BTreeSet<&str> = graph.edges[keymap]
        .iter()
        .map(|id| graph.name(id))
        .collect();
    assert_eq!(direct, BTreeSet::from(["serde", "serde_json", "toml"]));
    assert!(graph.reaches(graph.member(CORE), PLUGIN_API));
}

/// Обход не пуст: известные зависимости видны, иначе запреты выше проходили бы без проверки.
#[test]
fn walk_sees_the_dependencies_that_exist() {
    let graph = workspace();
    let view = graph.member(PLUGIN_VIEW);
    assert!(graph.reaches(view, APPKIT));
    assert!(graph.reaches(view, PLUGIN_API));
    assert!(graph.reaches(view, "objc2"));
    assert!(graph.reaches(graph.member(APPKIT), "objc2"));
    assert!(graph.reaches(graph.member(CORE), PLUGIN_API));
    assert!(graph.reaches(graph.member(CORE), "shellio-layout"));

    let app = graph.member(APP);
    assert!(graph.reaches(app, CORE));
    for plugin in plugins(graph) {
        assert!(graph.reaches(plugin, PLUGIN_VIEW));
        assert!(graph.reaches(plugin, PLUGIN_API));
        assert!(graph.reaches(plugin, "objc2"));
        assert!(graph.reaches(app, graph.name(plugin)));
    }
}

fn package(name: &str, dir: &str, dependencies: Value) -> Value {
    serde_json::json!({
        "id": name,
        "name": name,
        "manifest_path": format!("/w/{dir}/{name}/Cargo.toml"),
        "dependencies": dependencies,
    })
}

fn node(id: &str, deps: &[(&str, &[Option<&str>])]) -> Value {
    let deps: Vec<Value> = deps
        .iter()
        .map(|(pkg, kinds)| {
            let kinds: Vec<Value> = kinds
                .iter()
                .map(|kind| serde_json::json!({"kind": kind, "target": null}))
                .collect();
            serde_json::json!({"name": pkg, "pkg": pkg, "dep_kinds": kinds})
        })
        .collect();
    serde_json::json!({"id": id, "deps": deps})
}

/// Правила обхода на маленьком графе: зависимость тестов не считается, сборочная и необязательная
/// считаются, путь нарушения назван целиком.
#[test]
fn walk_follows_normal_build_and_optional_edges_but_not_dev_ones() {
    let declared = serde_json::json!([
        {"name": "gated", "kind": null, "optional": true},
        {"name": "tested", "kind": "dev", "optional": false},
    ]);
    let metadata = serde_json::json!({
        "workspace_root": "/w",
        "workspace_members": ["pure", "plugin-a"],
        "packages": [
            package("pure", "crates", declared),
            package("plugin-a", "plugins", serde_json::json!([])),
            package("direct", "vendor", serde_json::json!([])),
            package("built", "vendor", serde_json::json!([])),
            package("deep", "vendor", serde_json::json!([])),
            package("gated", "vendor", serde_json::json!([])),
            package("tested", "vendor", serde_json::json!([])),
            package("both", "vendor", serde_json::json!([])),
        ],
        "resolve": {"nodes": [
            node("pure", &[
                ("direct", &[None]),
                ("built", &[Some("build")]),
                ("tested", &[Some("dev")]),
                ("both", &[Some("dev"), None]),
            ]),
            node("built", &[("deep", &[None])]),
            node("tested", &[("plugin-a", &[None])]),
            node("plugin-a", &[]),
        ]},
    });
    let graph = Graph::of(&metadata);

    let reached: BTreeSet<&str> = graph.reach("pure").into_keys().collect();
    assert_eq!(
        reached,
        BTreeSet::from(["direct", "built", "deep", "gated", "both"])
    );
    assert_eq!(graph.members_under("plugins"), ["plugin-a"]);
    assert_eq!(graph.member("pure"), "pure");
    assert_eq!(
        graph.violations("pure", |id| id == "deep" || id == "plugin-a"),
        ["pure → built → deep"]
    );
    assert!(graph.reaches("pure", "gated"));
    assert!(!graph.reaches("pure", "tested"));
    assert!(graph.reach("plugin-a").is_empty());
}
