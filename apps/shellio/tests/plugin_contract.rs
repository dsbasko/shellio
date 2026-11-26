//! Контрактный тест зон для каждого зарегистрированного плагина (ADR-0009; ADR-0008, «Проверка
//! представления»): `adapt` модели по фикстурам `shellio_layout::contract` с `min_size` манифеста,
//! четыре зоны у встроенных плагинов и названия по правилам ядра.

// allow-unwrap-in-tests не распространяется на вспомогательные функции интеграционного теста.
#![allow(clippy::unwrap_used)]

use shellio::plugins;
use shellio_core::{
    NAVIGATOR_PLUGIN, NAVIGATOR_TITLE, TERMINAL_PLUGIN, TERMINAL_TITLE, Title, layout_metrics,
};
use shellio_layout::contract::{Outcome, fixtures};
use shellio_layout::{MinSize, Zone};
use shellio_plugin_api::{Cardinality, Plugin, PluginId, Registry, ZoneSet, contract};

fn registry() -> Registry {
    plugins::registry().unwrap()
}

fn plugin<'a>(registry: &'a Registry, id: &str) -> &'a Plugin {
    registry.get(&PluginId::new(id).unwrap()).unwrap()
}

#[test]
fn every_registered_plugin_passes_the_zone_contract() {
    let registry = registry();
    let fixtures = fixtures(&layout_metrics());
    assert!(!registry.is_empty());
    for plugin in registry.iter() {
        let id = plugin.id();
        let model = plugin.model();
        let min_size = plugin.manifest().min_size;
        let state = model.init(None).unwrap();
        model.check(&state).unwrap();

        let report = contract::check(model, &state, &fixtures, plugin.variants());
        assert!(report.passed(), "плагин «{id}»:\n{report}");
        assert_eq!(report.entries.len(), fixtures.len(), "плагин «{id}»");
        for entry in &report.entries {
            let fits = min_size.admits(entry.geometry.width, entry.geometry.height);
            let expected = if fits {
                Outcome::Adapted
            } else {
                Outcome::Stub
            };
            assert_eq!(
                entry.outcome, expected,
                "плагин «{id}», фикстура «{}» при min_size {min_size:?}",
                entry.fixture
            );
        }
        for zone in Zone::ALL {
            let adapted = report
                .entries
                .iter()
                .any(|entry| entry.geometry.zone == zone && entry.outcome == Outcome::Adapted);
            assert!(adapted, "плагин «{id}»: зона {zone:?} не проверена");
        }
    }
}

/// Намерения панели объявляет только «Демо: список»: вид без признака намерений не получает, а
/// вид с признаком проходит контракт намерений своего плагина во всех зонах.
#[test]
fn only_the_demo_list_declares_panel_intents() {
    let registry = registry();
    for plugin in registry.iter() {
        let id = plugin.id().to_string();
        let manifest = plugin.manifest();
        let expected = id == "demo-list";
        assert_eq!(
            (manifest.list_keys, manifest.filter),
            (expected, expected),
            "плагин «{id}»: признак намерений требует собственного прогона check_intents вида плагина"
        );
    }
}

#[test]
fn every_builtin_plugin_declares_all_four_zones() {
    for plugin in registry().iter() {
        let manifest = plugin.manifest();
        assert!(
            manifest.builtin,
            "плагин «{}» в списке приложения — встроенный",
            plugin.id()
        );
        assert_eq!(
            manifest.placements,
            ZoneSet::ALL,
            "плагин «{}»",
            plugin.id()
        );
        assert!(manifest.placements.contains(manifest.default_zone));
    }
}

#[test]
fn every_title_is_a_panel_title() {
    for plugin in registry().iter() {
        let title = plugin.manifest().title;
        let parsed = Title::new(title).unwrap();
        assert_eq!(parsed.as_str(), title, "плагин «{}»", plugin.id());
    }
}

#[test]
fn examples_of_every_action_parse() {
    for plugin in registry().iter() {
        plugin
            .model()
            .check_examples()
            .unwrap_or_else(|refusal| panic!("плагин «{}»: {refusal}", plugin.id()));
    }
}

/// Заполнители повторяют панели раскладки по умолчанию ядра: плагин от ядра не зависит, поэтому
/// равенство сверяется здесь.
#[test]
fn placeholders_match_the_default_layout_of_the_core() {
    let registry = registry();
    let cases = [
        (
            TERMINAL_PLUGIN,
            TERMINAL_TITLE,
            Cardinality::Multi,
            Zone::Center,
        ),
        (
            NAVIGATOR_PLUGIN,
            NAVIGATOR_TITLE,
            Cardinality::PerWorkbench,
            Zone::Left,
        ),
    ];
    for (id, title, cardinality, default_zone) in cases {
        let plugin = plugin(&registry, id);
        let manifest = plugin.manifest();
        assert_eq!(manifest.title, title);
        assert_eq!(manifest.cardinality, cardinality);
        assert_eq!(manifest.default_zone, default_zone);
        assert_eq!(manifest.min_size, MinSize::DEFAULT);
        assert_eq!(
            (manifest.min_size.width, manifest.min_size.height),
            (200.0, 120.0)
        );
        assert_eq!(manifest.state_version, 1);
        assert!(manifest.commands.is_empty());
        assert_eq!(plugin.model().init(None).unwrap(), "null");
    }
}
