//! Кандидаты палитры P1.6 из сцены и реестра (ADR-0010, «Уточнено в P1.6 (планирование):
//! наложения»): закреплённые команды прототипа, действия хоста с названием, открытые панели
//! рабочей области и плагины реестра. Тексты строк повторяют прототип (`overlays.js:161`–`:191`).

use serde_json::{Value, json};
use shellio_core::PanelId;
use shellio_keymap::{Args, Axis, Candidate, Keymap, Mode, PaletteSource};
use shellio_layout::{Location, Zone};
use shellio_plugin_api::{Manifest, Registry};

use crate::scene::Scene;

/// Что видит палитра в момент открытия.
pub struct Facts<'a> {
    pub scene: Option<&'a Scene>,
    pub registry: &'a Registry,
    /// Панель-источник наложения.
    pub panel: Option<PanelId>,
    /// Режим источника.
    pub mode: Mode,
    /// Действующее оформление тёмное.
    pub dark: bool,
    /// Аргументы действия плагина от вида панели-источника (`resolve_args`).
    pub resolve: &'a dyn Fn(&str) -> Option<Value>,
}

/// Действия хоста с названием палитры по командам прототипа (`commands.js:40`–`:143`). Фокус,
/// перенос в зону, размеры, вкладки и `mode.input` в палитре не показываются.
const TITLED: [(&str, Option<&str>, &str); 10] = [
    ("layout.split", Some("row"), "Разделить зону: части рядом"),
    (
        "layout.split",
        Some("stack"),
        "Разделить зону: части одна над другой",
    ),
    ("panel.close", None, "Закрыть панель"),
    ("layout.only", None, "Только эта панель"),
    ("layout.equal", None, "Выровнять размеры"),
    ("layout.zoom", None, "Развернуть панель"),
    ("panel.move_part.next", None, "В соседнюю часть"),
    ("ui.appearance.cycle", None, "Сменить тему"),
    ("app.quit", None, "Выйти из shellio"),
    ("mode.toggle", None, "Переключить режим"),
];

const PANEL_OPEN: &str = "panel.open.";

fn candidate(id: &str, title: String, detail: String, action: &str, args: Args) -> Candidate {
    Candidate {
        id: id.to_owned(),
        title,
        detail,
        action: action.to_owned(),
        args,
    }
}

/// Подпись зоны в строке открытой панели (прототип `overlays.js:137`).
fn zone_name(location: Location) -> &'static str {
    match location {
        Location::Navigator => "слева",
        Location::Part { zone, .. } => match zone {
            Zone::Left => "слева",
            Zone::Right => "справа",
            Zone::Center => "центр",
            Zone::Bottom => "низ",
        },
    }
}

/// Ось деления зоны: левая и правая делятся одна над другой, центр и нижняя рядом, навигатор
/// не делится (ADR-0031).
fn split_axis(location: Location) -> Option<Axis> {
    match location {
        Location::Navigator => None,
        Location::Part { zone, .. } => match zone {
            Zone::Left | Zone::Right => Some(Axis::Stack),
            Zone::Center | Zone::Bottom => Some(Axis::Row),
        },
    }
}

/// Действия плагина панели-источника с названием (прототип `overlays.js:191`). Аргументы даёт вид
/// панели: без них строки нет.
pub fn panel_rows(manifest: &Manifest, resolve: &dyn Fn(&str) -> Option<Value>) -> Vec<Candidate> {
    manifest
        .commands
        .iter()
        .filter(|spec| !spec.title.is_empty())
        .map(|spec| {
            candidate(
                spec.name,
                spec.title.to_owned(),
                format!("в панели «{}»", manifest.title),
                spec.name,
                Args::View(resolve(spec.name)),
            )
        })
        .collect()
}

fn toggle(hidden: bool) -> &'static str {
    if hidden {
        "Показать"
    } else {
        "Скрыть"
    }
}

/// Кандидаты палитры. Закреплённые строки носят id действий, которые они заменяют: модель
/// отбрасывает такие же строки остальных действий.
pub fn source(facts: &Facts<'_>) -> PaletteSource {
    let bench = facts.scene.and_then(|scene| scene.bench.as_ref());
    let titles = |zone: Zone| -> String {
        bench
            .map(|bench| {
                bench
                    .layout
                    .zones
                    .get(zone)
                    .parts
                    .iter()
                    .flat_map(|part| part.tabs.iter())
                    .filter_map(|panel| bench.views.get(panel))
                    .map(|slot| slot.title.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let hidden = bench.map(|bench| bench.layout.hidden).unwrap_or_default();
    let nav = facts.mode == Mode::Navigation;
    let mut pinned = vec![
        candidate(
            "mode.toggle",
            if nav {
                "Режим «Ввод»"
            } else {
                "Режим «Навигация»"
            }
            .to_owned(),
            format!(
                "клавиши {}",
                if nav {
                    "идут в терминал"
                } else {
                    "управляют окном"
                }
            ),
            "mode.toggle",
            Args::None,
        ),
        candidate(
            "ui.appearance.cycle",
            if facts.dark {
                "Светлая тема"
            } else {
                "Тёмная тема"
            }
            .to_owned(),
            "оформление".to_owned(),
            if facts.dark {
                "ui.appearance.light"
            } else {
                "ui.appearance.dark"
            },
            Args::None,
        ),
    ];
    if bench.is_some() {
        pinned.extend([
            candidate(
                "layout.toggle.navigator",
                format!("{} навигацию", toggle(hidden.navigator)),
                "зоны".to_owned(),
                "layout.toggle.navigator",
                Args::None,
            ),
            candidate(
                "layout.toggle.bottom",
                format!("{} нижнюю зону", toggle(hidden.bottom)),
                titles(Zone::Bottom),
                "layout.toggle.bottom",
                Args::None,
            ),
            candidate(
                "layout.toggle.right",
                format!("{} правую зону", toggle(hidden.right)),
                titles(Zone::Right),
                "layout.toggle.right",
                Args::None,
            ),
        ]);
    }
    let commands = TITLED
        .iter()
        .map(|(action, axis, title)| {
            let (id, args) = match axis {
                Some(axis) => (
                    format!("{action}.{axis}"),
                    Args::Fixed(json!({ "axis": axis })),
                ),
                None => ((*action).to_owned(), Args::None),
            };
            candidate(&id, (*title).to_owned(), String::new(), action, args)
        })
        .collect();
    let location = facts
        .panel
        .zip(bench)
        .and_then(|(panel, bench)| bench.layout.locate(panel));
    let plugin = facts
        .panel
        .zip(bench)
        .and_then(|(panel, bench)| bench.views.get(&panel))
        .map(|slot| slot.plugin.as_str());
    let panel = plugin
        .and_then(|plugin| {
            facts
                .registry
                .iter()
                .map(|registered| registered.manifest())
                .find(|manifest| manifest.id == plugin)
        })
        .map(|manifest| panel_rows(manifest, facts.resolve))
        .unwrap_or_default();
    let open = bench
        .map(|bench| {
            bench
                .layout
                .panels()
                .filter_map(|panel| {
                    let slot = bench.views.get(&panel)?;
                    let place = bench.layout.locate(panel)?;
                    let wire = json!(panel);
                    let key = wire
                        .as_str()
                        .map_or_else(|| wire.to_string(), str::to_owned);
                    Some(candidate(
                        &format!("panel:{key}"),
                        slot.title.clone(),
                        format!("панель · {}", zone_name(place)),
                        "panel.select",
                        Args::Fixed(json!({ "panel": wire })),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let plugins = facts
        .registry
        .iter()
        .map(|plugin| {
            let manifest = plugin.manifest();
            let action = format!("{PANEL_OPEN}{}", manifest.id);
            candidate(
                &action,
                format!("Открыть: {}", manifest.title),
                String::new(),
                &action,
                Args::None,
            )
        })
        .collect();
    PaletteSource {
        pinned,
        panel,
        commands,
        open,
        plugins,
        split_axis: location.and_then(split_axis),
        contexts: Keymap::contexts(facts.mode, plugin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(list: &[Candidate]) -> Vec<&str> {
        list.iter()
            .map(|candidate| candidate.title.as_str())
            .collect()
    }

    #[test]
    fn pinned_rows_follow_mode_and_theme_and_hide_their_commands() {
        let registry = crate::plugins::registry().unwrap();
        let facts = |mode, dark| Facts {
            scene: None,
            registry: &registry,
            panel: None,
            mode,
            dark,
            resolve: &|_| None,
        };
        let light = source(&facts(Mode::Navigation, false));
        assert_eq!(titles(&light.pinned), ["Режим «Ввод»", "Тёмная тема"]);
        assert_eq!(light.pinned[1].action, "ui.appearance.dark");
        // Закреплённые носят id заменённых действий: модель отбросит «Сменить тему» и «Переключить режим».
        assert_eq!(light.pinned[0].id, "mode.toggle");
        assert_eq!(light.pinned[1].id, "ui.appearance.cycle");
        assert!(
            light
                .commands
                .iter()
                .any(|row| row.id == "ui.appearance.cycle")
        );

        let dark = source(&facts(Mode::Input, true));
        assert_eq!(titles(&dark.pinned), ["Режим «Навигация»", "Светлая тема"]);
        assert_eq!(dark.pinned[0].detail, "клавиши управляют окном");
        assert_eq!(dark.pinned[1].action, "ui.appearance.light");
        assert_eq!(dark.contexts, Keymap::contexts(Mode::Input, None));
    }

    #[test]
    fn plugins_become_open_rows_and_no_workbench_means_no_split_axis() {
        let registry = crate::plugins::registry().unwrap();
        let source = source(&Facts {
            scene: None,
            registry: &registry,
            panel: None,
            mode: Mode::Navigation,
            dark: false,
            resolve: &|_| None,
        });
        assert!(source.open.is_empty());
        assert_eq!(source.split_axis, None);
        let terminal = source
            .plugins
            .iter()
            .find(|row| row.action == "panel.open.terminal")
            .unwrap();
        assert_eq!(terminal.id, terminal.action);
        assert!(terminal.title.starts_with("Открыть: "));
        assert_eq!(source.plugins.len(), registry.iter().count());
        let splits: Vec<_> = source
            .commands
            .iter()
            .filter(|row| row.action == "layout.split")
            .map(|row| (row.id.as_str(), row.args.clone()))
            .collect();
        assert_eq!(
            splits,
            [
                ("layout.split.row", Args::Fixed(json!({"axis": "row"}))),
                ("layout.split.stack", Args::Fixed(json!({"axis": "stack"}))),
            ]
        );
    }

    #[test]
    fn panel_rows_need_a_title_and_view_args() {
        let registry = crate::plugins::registry().unwrap();
        let manifest = |id: &str| {
            registry
                .iter()
                .map(|plugin| plugin.manifest())
                .find(|manifest| manifest.id == id)
                .unwrap()
        };
        let native = manifest("demo-native");
        let rows = panel_rows(native, &|action| {
            (action == "demo-native.set").then(|| json!({"value": "черновик"}))
        });
        assert_eq!(
            rows,
            [candidate(
                "demo-native.set",
                "Применить поле".to_owned(),
                "в панели «Демо: поле»".to_owned(),
                "demo-native.set",
                Args::View(Some(json!({"value": "черновик"}))),
            )]
        );
        // Вид не дал аргументов: кандидат остаётся, строку палитра не покажет.
        let rows = panel_rows(native, &|_| None);
        assert_eq!(rows[0].args, Args::View(None));
        // Действия без названия в палитру не идут.
        assert!(panel_rows(manifest("demo-list"), &|_| Some(json!({}))).is_empty());
    }

    #[test]
    fn split_axis_follows_the_zone() {
        let part = |zone| Location::Part {
            zone,
            part: 0,
            tab: 0,
        };
        assert_eq!(split_axis(part(Zone::Left)), Some(Axis::Stack));
        assert_eq!(split_axis(part(Zone::Right)), Some(Axis::Stack));
        assert_eq!(split_axis(part(Zone::Center)), Some(Axis::Row));
        assert_eq!(split_axis(part(Zone::Bottom)), Some(Axis::Row));
        assert_eq!(split_axis(Location::Navigator), None);
        assert_eq!(zone_name(part(Zone::Bottom)), "низ");
    }
}
