//! Текстовый тест модулей `model` плагинов (ADR-0008, «Уточнено в P1.5 (планирование)»): модель и её
//! подмодули не знают ни контракта вида, ни AppKit, поэтому её тесты идут без `NSApplication`.

// allow-unwrap-in-tests не распространяется на вспомогательные функции интеграционного теста.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use shellio::plugins;

/// Чего в модели быть не должно: crate вида и AppKit, а также собственный модуль вида плагина.
const FORBIDDEN: [&str; 5] = [
    "shellio_plugin_view",
    "shellio_appkit",
    "objc2",
    "crate::view",
    "super::view",
];

fn plugins_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins")
}

fn plugin_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(plugins_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("Cargo.toml").is_file())
        .collect();
    dirs.sort();
    dirs
}

fn name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// Файлы модуля `model` плагина: `src/model.rs` и всё под `src/model/`.
fn model_files(plugin: &Path) -> Vec<PathBuf> {
    let src = plugin.join("src");
    let mut files = Vec::new();
    if src.join("model.rs").is_file() {
        files.push(src.join("model.rs"));
    }
    if src.join("model").is_dir() {
        rust_files(&src.join("model"), &mut files);
    }
    files
}

fn violations(text: &str) -> Vec<&'static str> {
    FORBIDDEN
        .into_iter()
        .filter(|word| text.contains(word))
        .collect()
}

#[test]
fn every_plugin_directory_is_a_registered_plugin() {
    let on_disk: BTreeSet<String> = plugin_dirs().iter().map(|dir| name(dir)).collect();
    let registered: BTreeSet<String> = plugins::registry()
        .unwrap()
        .iter()
        .map(|plugin| plugin.id().as_str().to_owned())
        .collect();
    assert!(on_disk.len() >= 2, "плагины не найдены: {on_disk:?}");
    assert_eq!(on_disk, registered);
}

#[test]
fn model_modules_do_not_know_the_view_or_appkit() {
    for plugin in plugin_dirs() {
        let files = model_files(&plugin);
        assert!(
            !files.is_empty(),
            "у плагина «{}» нет модуля model",
            name(&plugin)
        );
        for file in files {
            let text = fs::read_to_string(&file).unwrap();
            let found = violations(&text);
            assert!(found.is_empty(), "{}: {found:?}", file.display());
        }
    }
}

#[test]
fn every_plugin_forbids_unsafe_code_and_has_a_view_module() {
    for plugin in plugin_dirs() {
        let lib = fs::read_to_string(plugin.join("src/lib.rs")).unwrap();
        assert!(
            lib.contains("#![forbid(unsafe_code)]"),
            "плагин «{}» не запрещает unsafe",
            name(&plugin)
        );
        let src = plugin.join("src");
        assert!(
            src.join("view.rs").is_file() || src.join("view").is_dir(),
            "у плагина «{}» нет модуля view",
            name(&plugin)
        );
    }
}

#[test]
fn search_finds_each_forbidden_import() {
    assert!(violations("use shellio_layout::Geometry;").is_empty());
    assert_eq!(
        violations("use shellio_plugin_view::ViewFactory;"),
        ["shellio_plugin_view"]
    );
    assert_eq!(violations("use shellio_appkit::App;"), ["shellio_appkit"]);
    assert_eq!(violations("use objc2_app_kit::NSView;"), ["objc2"]);
    assert_eq!(violations("use crate::view::factory;"), ["crate::view"]);
    assert_eq!(violations("use super::view;"), ["super::view"]);
}
