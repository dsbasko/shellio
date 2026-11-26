//! Перевод действия таблицы keymap в исполнение (ADR-0010, «Уточнено в P1.6 (планирование)»).
//!
//! Перевод идёт в два шага. [`recognize`] по имени и `args` привязки называет путь исполнения и от
//! сцены не зависит: неизвестное имя — `None`, а не молча поглощённая клавиша. [`command`] строит
//! команду ядра по сцене в момент нажатия и вправе ничего не построить: действовать не на что.

use std::num::NonZeroU32;

use serde_json::Value;
use shellio_core::{Command, PanelId, ResizeStep, SplitAxis, UiCommand};
use shellio_layout::Direction;
use shellio_plugin_api::Registry;

use crate::scene::{self, PanelMenuItem, Scene};

const PANEL_OPEN: &str = "panel.open.";

/// Шаг истории переходов.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryStep {
    Back,
    Forward,
}

/// Действие раскладки для панели с фокусом: одна команда ядра.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutAction {
    /// `panel.focus.*`: фокус на соседнюю плитку.
    Focus(Direction),
    /// `panel.move.*`, `panel.move_part.*` и `panel.close`: пункт меню «…» плитки.
    Item(PanelMenuItem),
    /// `layout.split` с осью привязки или без неё.
    Split(Option<SplitAxis>),
    Only,
    Equal,
    Zoom,
    /// `layout.grow`, `layout.shrink`, `layout.wider`, `layout.narrower`.
    Resize(ResizeStep),
    TabPrev,
    TabNext,
    TabSelect(NonZeroU32),
    /// `panel.select` с явной панелью.
    Select(PanelId),
}

/// Путь исполнения действия keymap.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyAction {
    /// Действие интерфейса: единый обработчик `UiCommand`. У `mode.*` режим уже сменило решение
    /// маршрутизатора, клавиша его только применяет.
    Ui(UiCommand),
    /// `panel.filter` с текстом привязки.
    Filter(String),
    Layout(LayoutAction),
    /// `panel.open.<plugin>`: панель плагина в открытой рабочей области.
    Open(String),
    /// Действие плагина панели с фокусом. Без `args` аргументы даёт вид панели.
    Plugin {
        name: String,
        args: Option<Value>,
    },
    History(HistoryStep),
    /// `keymap.reload`: служба читает файл заново.
    Reload,
}

/// Единственное поле `key` таблицы `args`.
fn field<'a>(args: Option<&'a Value>, key: &str) -> Result<Option<&'a Value>, String> {
    let Some(args) = args else {
        return Ok(None);
    };
    let Some(object) = args.as_object() else {
        return Err("аргументы — таблица".to_owned());
    };
    match object.keys().find(|name| name.as_str() != key) {
        Some(extra) => Err(format!("лишнее поле «{extra}»")),
        None => Ok(object.get(key)),
    }
}

fn no_args(args: Option<&Value>) -> Result<(), String> {
    match args {
        None => Ok(()),
        Some(_) => Err("действие не принимает аргументы".to_owned()),
    }
}

fn direction(name: &str) -> Option<Direction> {
    match name {
        "panel.focus.left" => Some(Direction::Left),
        "panel.focus.down" => Some(Direction::Down),
        "panel.focus.up" => Some(Direction::Up),
        "panel.focus.right" => Some(Direction::Right),
        _ => None,
    }
}

/// Действие раскладки по имени; `None` — имя не из раскладки.
fn layout(name: &str, args: Option<&Value>) -> Option<Result<LayoutAction, String>> {
    let plain = |action: LayoutAction| Some(no_args(args).map(|()| action));
    if let Some(direction) = direction(name) {
        return plain(LayoutAction::Focus(direction));
    }
    if let Some(item) = PanelMenuItem::of_id(name) {
        return plain(LayoutAction::Item(item));
    }
    if let Some(step) = ResizeStep::from_keymap_name(name) {
        return plain(LayoutAction::Resize(step));
    }
    match name {
        "layout.only" => plain(LayoutAction::Only),
        "layout.equal" => plain(LayoutAction::Equal),
        "layout.zoom" => plain(LayoutAction::Zoom),
        "tab.prev" => plain(LayoutAction::TabPrev),
        "tab.next" => plain(LayoutAction::TabNext),
        "layout.split" => Some(field(args, "axis").and_then(|axis| {
            match axis {
                None => Ok(LayoutAction::Split(None)),
                Some(axis) => serde_json::from_value(axis.clone())
                    .map(|axis| LayoutAction::Split(Some(axis)))
                    .map_err(|_| "axis — row или stack".to_owned()),
            }
        })),
        "tab.select" => Some(field(args, "index").and_then(|index| {
            index
                .and_then(Value::as_u64)
                .and_then(|index| u32::try_from(index).ok())
                .and_then(NonZeroU32::new)
                .map(LayoutAction::TabSelect)
                .ok_or_else(|| "index — номер вкладки с единицы".to_owned())
        })),
        "panel.select" => Some(field(args, "panel").and_then(|panel| {
            panel
                .and_then(|panel| serde_json::from_value(panel.clone()).ok())
                .map(LayoutAction::Select)
                .ok_or_else(|| "panel — id панели".to_owned())
        })),
        _ => None,
    }
}

/// Путь исполнения действия `name` с `args` привязки. `None` — такого действия нет ни у хоста, ни
/// у плагинов реестра; `Err` — `args` действию не подходят, причину показывает строка статуса.
pub fn recognize(
    registry: &Registry,
    name: &str,
    args: Option<&Value>,
) -> Option<Result<KeyAction, String>> {
    if name == UiCommand::PanelFilter.keymap_name() {
        return Some(field(args, "text").and_then(|text| match text {
            None => Ok(KeyAction::Ui(UiCommand::PanelFilter)),
            Some(Value::String(text)) => Ok(KeyAction::Filter(text.clone())),
            Some(_) => Err("text — строка".to_owned()),
        }));
    }
    if let Some(command) = UiCommand::from_keymap_name(name) {
        return Some(no_args(args).map(|()| KeyAction::Ui(command)));
    }
    if let Some(action) = layout(name, args) {
        return Some(action.map(KeyAction::Layout));
    }
    let plain = |action: KeyAction| Some(no_args(args).map(|()| action));
    match name {
        "history.back" => return plain(KeyAction::History(HistoryStep::Back)),
        "history.forward" => return plain(KeyAction::History(HistoryStep::Forward)),
        "keymap.reload" => return plain(KeyAction::Reload),
        _ => {}
    }
    if let Some(plugin) = name.strip_prefix(PANEL_OPEN) {
        let known = registry
            .iter()
            .find(|known| known.id().as_str() == plugin)?;
        return plain(KeyAction::Open(known.id().as_str().to_owned()));
    }
    registry
        .iter()
        .any(|plugin| plugin.manifest().has_command(name))
        .then(|| {
            Ok(KeyAction::Plugin {
                name: name.to_owned(),
                args: args.cloned(),
            })
        })
}

/// Команда ядра действия раскладки для панели с фокусом открытой рабочей области. Цель называется
/// явно. `None` — рабочая область не открыта, панели с фокусом нет или действовать не на что.
pub fn command(scene: &Scene, action: LayoutAction) -> Option<Command> {
    let bench = scene.bench.as_ref()?;
    let workbench = bench.workbench;
    match action {
        LayoutAction::Focus(direction) => Some(Command::PanelFocus {
            panel: None,
            workbench: Some(workbench),
            direction: Some(direction),
        }),
        LayoutAction::Only => Some(Command::LayoutOnly {
            workbench: Some(workbench),
        }),
        LayoutAction::TabPrev => Some(Command::TabPrev { workbench }),
        LayoutAction::TabNext => Some(Command::TabNext { workbench }),
        LayoutAction::TabSelect(index) => Some(Command::TabSelect { workbench, index }),
        LayoutAction::Select(panel) => Some(Command::PanelSelect { panel }),
        LayoutAction::Item(item) => scene::item_command(bench, bench.focus?, item),
        LayoutAction::Split(axis) => Some(Command::LayoutSplit {
            panel: bench.focus?,
            open: None,
            axis,
        }),
        LayoutAction::Equal => {
            let zone = bench.layout.locate(bench.focus?)?.zone();
            Some(Command::LayoutEqual {
                workbench: Some(workbench),
                zone,
            })
        }
        // Развёрнутая панель возвращает раскладку той же клавишей, как кнопка её плитки.
        LayoutAction::Zoom if bench.zoomed.is_some() => Some(Command::LayoutZoom {
            panel: None,
            workbench: Some(workbench),
            off: Some(true),
        }),
        LayoutAction::Zoom => Some(Command::LayoutZoom {
            panel: Some(bench.focus?),
            workbench: None,
            off: None,
        }),
        LayoutAction::Resize(step) => Some(step.command(bench.focus?)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use serde_json::json;
    use shellio_core::layout_metrics;
    use shellio_keymap::Keymap;
    use shellio_layout::Zone;

    use super::*;
    use crate::keymap_service::{Actions, host_action_names};
    use crate::scene::tests::{SMALL, fixture, registry};

    fn known(registry: &Registry, name: &str, args: Option<Value>) -> KeyAction {
        match recognize(registry, name, args.as_ref()) {
            Some(Ok(action)) => action,
            other => panic!("{name} {args:?}: {other:?}"),
        }
    }

    /// Пример `args` действия хоста, которому они обязательны.
    fn sample(name: &str) -> Option<Value> {
        match name {
            "tab.select" => Some(json!({ "index": 2 })),
            "panel.select" => Some(json!({ "panel": "00000000-0000-0000-0000-000000000009" })),
            _ => None,
        }
    }

    #[test]
    fn every_action_of_the_table_and_of_the_registry_has_a_path() {
        let registry = Arc::new(registry());
        let actions = Actions::of(Arc::clone(&registry));
        let keymap = Keymap::defaults(actions.panels()).unwrap();
        for binding in keymap.bindings() {
            known(&registry, &binding.action, binding.args.clone());
        }
        for name in host_action_names() {
            known(&registry, name, sample(name));
        }
        for plugin in registry.iter() {
            let open = format!("{PANEL_OPEN}{}", plugin.id().as_str());
            assert_eq!(
                known(&registry, &open, None),
                KeyAction::Open(plugin.id().as_str().to_owned())
            );
            for spec in plugin.manifest().commands {
                assert_eq!(
                    known(&registry, spec.name, None),
                    KeyAction::Plugin {
                        name: spec.name.to_owned(),
                        args: None,
                    }
                );
            }
        }
    }

    #[test]
    fn unknown_names_have_no_path() {
        let registry = registry();
        for name in ["layout.resize_by", "panel.open.nobody", "nobody.act", ""] {
            assert_eq!(recognize(&registry, name, None), None, "{name}");
        }
    }

    #[test]
    fn host_names_keep_their_paths() {
        let registry = registry();
        let paths: BTreeSet<&str> = host_action_names()
            .filter(|name| matches!(known(&registry, name, sample(name)), KeyAction::Layout(_)))
            .collect();
        let expected: BTreeSet<&str> = [
            "layout.equal",
            "layout.grow",
            "layout.narrower",
            "layout.only",
            "layout.shrink",
            "layout.split",
            "layout.wider",
            "layout.zoom",
            "panel.close",
            "panel.focus.down",
            "panel.focus.left",
            "panel.focus.right",
            "panel.focus.up",
            "panel.move.down",
            "panel.move.left",
            "panel.move.right",
            "panel.move.up",
            "panel.move_part.next",
            "panel.move_part.prev",
            "panel.select",
            "tab.next",
            "tab.prev",
            "tab.select",
        ]
        .into();
        assert_eq!(paths, expected);
        assert_eq!(
            known(&registry, "mode.toggle", None),
            KeyAction::Ui(UiCommand::from_keymap_name("mode.toggle").unwrap())
        );
        assert_eq!(
            known(&registry, "history.back", None),
            KeyAction::History(HistoryStep::Back)
        );
        assert_eq!(
            known(&registry, "history.forward", None),
            KeyAction::History(HistoryStep::Forward)
        );
        assert_eq!(known(&registry, "keymap.reload", None), KeyAction::Reload);
    }

    #[test]
    fn binding_args_are_parsed_strictly() {
        let registry = registry();
        assert_eq!(
            known(&registry, "layout.split", Some(json!({ "axis": "stack" }))),
            KeyAction::Layout(LayoutAction::Split(Some(SplitAxis::Stack)))
        );
        assert_eq!(
            known(&registry, "layout.split", None),
            KeyAction::Layout(LayoutAction::Split(None))
        );
        assert_eq!(
            known(&registry, "tab.select", Some(json!({ "index": 3 }))),
            KeyAction::Layout(LayoutAction::TabSelect(NonZeroU32::new(3).unwrap()))
        );
        assert_eq!(
            known(&registry, "panel.filter", Some(json!({ "text": "ab" }))),
            KeyAction::Filter("ab".to_owned())
        );
        assert_eq!(
            known(&registry, "panel.filter", None),
            KeyAction::Ui(UiCommand::PanelFilter)
        );
        for (name, args) in [
            ("layout.split", json!({ "axis": "up" })),
            ("layout.split", json!({ "axis": "row", "open": "x" })),
            ("tab.select", json!({ "index": 0 })),
            ("tab.select", json!({})),
            ("panel.select", json!({ "panel": 7 })),
            ("panel.filter", json!({ "text": 1 })),
            ("layout.zoom", json!({})),
            ("app.quit", json!({ "now": true })),
            ("history.back", json!([])),
        ] {
            assert!(
                matches!(recognize(&registry, name, Some(&args)), Some(Err(_))),
                "{name} {args}"
            );
        }
        assert!(matches!(
            recognize(&registry, "tab.select", None),
            Some(Err(_))
        ));
    }

    #[test]
    fn layout_actions_name_the_focused_panel_and_the_open_workbench() {
        let fixture = fixture();
        let scene = Scene::of(&fixture.state, &registry(), SMALL, layout_metrics());
        let bench = scene.bench.as_ref().unwrap();
        let focus = bench.focus.unwrap();
        let workbench = fixture.workbench;
        let zone = bench.layout.locate(focus).unwrap().zone();

        assert_eq!(
            command(&scene, LayoutAction::Focus(Direction::Left)),
            Some(Command::PanelFocus {
                panel: None,
                workbench: Some(workbench),
                direction: Some(Direction::Left),
            })
        );
        assert_eq!(
            command(&scene, LayoutAction::Split(Some(SplitAxis::Row))),
            Some(Command::LayoutSplit {
                panel: focus,
                open: None,
                axis: Some(SplitAxis::Row),
            })
        );
        assert_eq!(
            command(&scene, LayoutAction::Zoom),
            Some(Command::LayoutZoom {
                panel: Some(focus),
                workbench: None,
                off: None,
            })
        );
        assert_eq!(
            command(&scene, LayoutAction::Equal),
            Some(Command::LayoutEqual {
                workbench: Some(workbench),
                zone,
            })
        );
        assert_eq!(
            command(&scene, LayoutAction::Only),
            Some(Command::LayoutOnly {
                workbench: Some(workbench),
            })
        );
        assert_eq!(
            command(&scene, LayoutAction::Resize(ResizeStep::Wider)),
            Some(ResizeStep::Wider.command(focus))
        );
        assert_eq!(
            command(&scene, LayoutAction::TabNext),
            Some(Command::TabNext { workbench })
        );
        assert_eq!(
            command(&scene, LayoutAction::TabSelect(NonZeroU32::new(2).unwrap())),
            Some(Command::TabSelect {
                workbench,
                index: NonZeroU32::new(2).unwrap(),
            })
        );
        assert_eq!(
            command(&scene, LayoutAction::Item(PanelMenuItem::Close)),
            Some(Command::PanelClose { panel: focus })
        );
        let target = if zone == Zone::Bottom {
            Zone::Center
        } else {
            Zone::Bottom
        };
        assert_eq!(
            command(&scene, LayoutAction::Item(PanelMenuItem::Zone(target))),
            Some(Command::PanelMove {
                panel: focus,
                zone: target,
            })
        );
        assert_eq!(
            command(&scene, LayoutAction::Select(fixture.terminal)),
            Some(Command::PanelSelect {
                panel: fixture.terminal,
            })
        );
    }

    #[test]
    fn zoomed_panel_returns_the_layout_by_the_same_action() {
        let mut fixture = fixture();
        let terminal = fixture.terminal;
        fixture
            .state
            .layouts
            .get_mut(&fixture.workbench)
            .unwrap()
            .zoomed = Some(terminal);
        let scene = Scene::of(&fixture.state, &registry(), SMALL, layout_metrics());
        assert!(scene.bench.as_ref().unwrap().zoomed.is_some());
        assert_eq!(
            command(&scene, LayoutAction::Zoom),
            Some(Command::LayoutZoom {
                panel: None,
                workbench: Some(fixture.workbench),
                off: Some(true),
            })
        );
    }

    #[test]
    fn without_an_open_workbench_there_is_nothing_to_act_on() {
        let mut fixture = fixture();
        fixture.state.window.route = shellio_core::Route::WorkspaceHome;
        let scene = Scene::of(&fixture.state, &registry(), SMALL, layout_metrics());
        for action in [
            LayoutAction::Focus(Direction::Up),
            LayoutAction::Only,
            LayoutAction::Zoom,
            LayoutAction::TabPrev,
            LayoutAction::Item(PanelMenuItem::Close),
        ] {
            assert_eq!(command(&scene, action), None, "{action:?}");
        }
    }
}
