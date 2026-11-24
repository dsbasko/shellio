//! Модель «Демо: список»: выбранная строка, история «назад» и флаг раскрытых деталей. Строки и их
//! детали фиксированы и в данные панели не входят.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use shellio_layout::{Geometry, MinSize, Rect, SizeClass, Zone};
use shellio_plugin_api::{
    Cardinality, CommandSpec, Manifest, PanelModel, Presentation, Reach, Refusal, Region,
    SizeVariants, VariantTable, Via, ZoneSet,
};

/// Выбор строки без записи в историю.
pub const SELECT: &str = "demo-list.select";
/// Раскрыть или закрыть детали выбранной строки.
pub const DETAILS: &str = "demo-list.details";
/// Вернуться к последней строке истории.
pub const BACK: &str = "demo-list.back";

pub const MANIFEST: Manifest = Manifest {
    id: "demo-list",
    title: "Демо: список",
    icon: "list.bullet",
    placements: ZoneSet::ALL,
    default_zone: Zone::Right,
    cardinality: Cardinality::PerWorkbench,
    min_size: MinSize {
        width: 260.0,
        height: 160.0,
    },
    commands: &[
        CommandSpec {
            name: SELECT,
            args: r#"{"key":"c"}"#,
        },
        CommandSpec {
            name: DETAILS,
            args: r#"{"open":true}"#,
        },
        CommandSpec {
            name: BACK,
            args: "{}",
        },
    ],
    keymap: &[],
    state_version: 1,
    builtin: true,
};

/// Список, детали вглубь.
pub const DRILL: &str = "drill";
/// Список и детали рядом.
pub const SPLIT: &str = "split";
/// Строки в ширину, детали справа.
pub const BAR: &str = "bar";

/// Регион списка строк.
pub const LIST_REGION: &str = "list";
/// Регион деталей выбранной строки.
pub const DETAILS_REGION: &str = "details";

const SIDE: SizeVariants = SizeVariants {
    narrow: DRILL,
    regular: DRILL,
    wide: SPLIT,
    short_bar: BAR,
};

/// Сначала `ShortBar`, затем ширина.
pub const VARIANTS: VariantTable = VariantTable {
    left: SIDE,
    right: SIDE,
    center: SizeVariants {
        narrow: DRILL,
        regular: SPLIT,
        wide: SPLIT,
        short_bar: BAR,
    },
    bottom: SizeVariants {
        narrow: DRILL,
        regular: BAR,
        wide: BAR,
        short_bar: BAR,
    },
};

/// Строка списка с тремя строками деталей.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub key: &'static str,
    pub title: &'static str,
    pub details: [&'static str; 3],
}

const fn item(key: &'static str, title: &'static str, details: [&'static str; 3]) -> Item {
    Item {
        key,
        title,
        details,
    }
}

/// Строки списка в порядке показа.
pub const ITEMS: [Item; 8] = [
    item(
        "a",
        "Альфа",
        ["Ключ: a", "Строка 1 из 8", "Первая буква: альфа"],
    ),
    item(
        "b",
        "Бета",
        ["Ключ: b", "Строка 2 из 8", "Вторая буква: бета"],
    ),
    item(
        "c",
        "Гамма",
        ["Ключ: c", "Строка 3 из 8", "Третья буква: гамма"],
    ),
    item(
        "d",
        "Дельта",
        ["Ключ: d", "Строка 4 из 8", "Четвёртая буква: дельта"],
    ),
    item(
        "e",
        "Эпсилон",
        ["Ключ: e", "Строка 5 из 8", "Пятая буква: эпсилон"],
    ),
    item(
        "f",
        "Дзета",
        ["Ключ: f", "Строка 6 из 8", "Шестая буква: дзета"],
    ),
    item(
        "g",
        "Эта",
        ["Ключ: g", "Строка 7 из 8", "Седьмая буква: эта"],
    ),
    item(
        "h",
        "Тета",
        ["Ключ: h", "Строка 8 из 8", "Восьмая буква: тета"],
    ),
];

/// Сколько ключей держит история: старейший вытесняется.
pub const HISTORY_MAX: usize = 32;

/// Строка с ключом `key`.
#[must_use]
pub fn find(key: &str) -> Option<&'static Item> {
    ITEMS.iter().find(|item| item.key == key)
}

/// Данные панели.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawState")]
pub struct State {
    /// Ключ выбранной строки.
    pub key: String,
    /// Прежние выборы `reopen`, последний — в конце.
    pub history: Vec<String>,
    /// Детали раскрыты. На представление влияет только в варианте `drill`.
    pub details: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawState {
    key: String,
    history: Vec<String>,
    details: bool,
}

impl TryFrom<RawState> for State {
    type Error = String;

    fn try_from(raw: RawState) -> Result<Self, Self::Error> {
        if find(&raw.key).is_none() {
            return Err(format!("строки «{}» нет", raw.key));
        }
        if raw.history.len() > HISTORY_MAX {
            return Err(format!(
                "история из {} ключей при пределе {HISTORY_MAX}",
                raw.history.len()
            ));
        }
        if let Some(key) = raw.history.iter().find(|key| find(key).is_none()) {
            return Err(format!("в истории строка «{key}», которой нет"));
        }
        Ok(Self {
            key: raw.key,
            history: raw.history,
            details: raw.details,
        })
    }
}

impl State {
    /// Данные новой панели без контекста.
    #[must_use]
    pub fn initial() -> Self {
        Self {
            key: ITEMS[0].key.to_owned(),
            history: Vec::new(),
            details: false,
        }
    }
}

/// Контекст `open` и `reopen`: строка, которую показать.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Args {
    pub key: String,
}

#[derive(Deserialize)]
#[serde(tag = "name", content = "args", deny_unknown_fields)]
pub enum Command {
    #[serde(rename = "demo-list.select")]
    Select { key: String },
    #[serde(rename = "demo-list.details")]
    Details { open: bool },
    #[serde(rename = "demo-list.back")]
    Back {},
}

/// Причина отказа: строки с таким ключом нет.
pub const KEY_UNKNOWN: &str = "key_unknown";

fn known(key: String) -> Result<String, Refusal> {
    if find(&key).is_some() {
        Ok(key)
    } else {
        Err(Refusal::new(
            "key",
            KEY_UNKNOWN,
            format!("строки «{key}» в списке нет"),
        ))
    }
}

/// Вариант представления по зоне и классу размера.
fn variant(geometry: &Geometry) -> &'static str {
    match geometry.zone {
        Zone::Left => SIDE.get(geometry.size),
        Zone::Right => SIDE.get(geometry.size),
        Zone::Center => match geometry.size {
            SizeClass::ShortBar => BAR,
            SizeClass::Narrow => DRILL,
            SizeClass::Regular | SizeClass::Wide => SPLIT,
        },
        Zone::Bottom => match geometry.size {
            SizeClass::Narrow => DRILL,
            SizeClass::ShortBar | SizeClass::Regular | SizeClass::Wide => BAR,
        },
    }
}

/// Доля ширины списка в `split`: детали шире.
const SPLIT_LIST_SHARE: f64 = 0.4;
/// Доля ширины списка в `bar`: строки шире.
const BAR_LIST_SHARE: f64 = 0.6;

fn region(name: &'static str, x: f64, width: f64, height: f64) -> Region {
    Region {
        name,
        frame: Rect {
            x,
            y: 0.0,
            width,
            height,
        },
    }
}

/// Список слева на долю `share` ширины, детали — на остальной.
fn side_by_side(geometry: &Geometry, share: f64) -> Vec<Region> {
    let list = (geometry.width * share).floor();
    vec![
        region(LIST_REGION, 0.0, list, geometry.height),
        region(DETAILS_REGION, list, geometry.width - list, geometry.height),
    ]
}

pub struct Model;

impl PanelModel for Model {
    type State = State;
    type Args = Args;
    type Command = Command;

    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn init(&self, args: Option<Args>) -> Result<State, Refusal> {
        let mut state = State::initial();
        if let Some(args) = args {
            state.key = known(args.key)?;
        }
        Ok(state)
    }

    fn reopen(&self, state: &State, args: Option<Args>) -> Result<State, Refusal> {
        let Some(args) = args else {
            return Ok(state.clone());
        };
        let key = known(args.key)?;
        if key == state.key {
            return Ok(state.clone());
        }
        let mut next = state.clone();
        next.history.push(std::mem::replace(&mut next.key, key));
        if next.history.len() > HISTORY_MAX {
            let excess = next.history.len() - HISTORY_MAX;
            next.history.drain(..excess);
        }
        Ok(next)
    }

    fn apply(&self, state: &State, command: Command) -> Result<State, Refusal> {
        let mut next = state.clone();
        match command {
            Command::Select { key } => next.key = known(key)?,
            Command::Details { open } => next.details = open,
            Command::Back {} => {
                if let Some(key) = next.history.pop() {
                    next.key = key;
                }
            }
        }
        Ok(next)
    }

    fn migrate(&self, from: u32, _data: Value) -> Result<State, Refusal> {
        Err(Refusal::new(
            "panel",
            "state_version_unknown",
            format!("у плагина «{}» нет данных версии {from}", MANIFEST.id),
        ))
    }

    fn adapt(&self, geometry: &Geometry, state: &State) -> Presentation {
        let variant = variant(geometry);
        let deep = variant == DRILL && state.details;
        let regions = match variant {
            DRILL => {
                let name = if deep { DETAILS_REGION } else { LIST_REGION };
                vec![region(name, 0.0, geometry.width, geometry.height)]
            }
            SPLIT => side_by_side(geometry, SPLIT_LIST_SHARE),
            _ => side_by_side(geometry, BAR_LIST_SHARE),
        };
        // В `drill` детали — уровень вглубь; пока они открыты, список и «назад» лежат уровнем выше.
        let (list, details) = match (variant == DRILL, deep) {
            (true, true) => (Via::Drill, Via::Drill),
            (true, false) => (Via::Direct, Via::Drill),
            (false, _) => (Via::Direct, Via::Direct),
        };
        Presentation {
            variant,
            regions,
            reach: vec![
                Reach {
                    action: SELECT,
                    via: list,
                },
                Reach {
                    action: DETAILS,
                    via: details,
                },
                Reach {
                    action: BACK,
                    via: list,
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::json;
    use shellio_layout::LayoutMetrics;
    use shellio_layout::contract::{Fixture, Outcome, fixtures};
    use shellio_plugin_api::{DynModel, Step, contract};

    use super::*;

    /// Метрики раскладки для фикстур: плагин от ядра не зависит. С метриками темы плагин прогоняет
    /// контрактный тест приложения.
    const METRICS: LayoutMetrics = LayoutMetrics {
        titlebar: 40,
        tabbar: 34,
        gap: 6,
        line: 1,
        status_bar: 26,
        drop_edge: 24,
    };

    fn model() -> DynModel {
        DynModel::new(Model)
    }

    /// Данные панели текстом, как их пишет модель: порядок полей — порядок `State`.
    fn text(key: &str, history: &[&str], details: bool) -> String {
        serde_json::to_string(&State {
            key: key.to_owned(),
            history: history.iter().map(|&key| key.to_owned()).collect(),
            details,
        })
        .unwrap()
    }

    fn state(text: &str) -> State {
        serde_json::from_str(text).unwrap()
    }

    fn changed(step: Step) -> String {
        match step {
            Step::Changed(text) => text,
            Step::Unchanged => panic!("состояние не изменилось"),
        }
    }

    fn reopen(text: &str, key: &str) -> Step {
        model().reopen(text, Some(json!({"key": key}))).unwrap()
    }

    fn command(text: &str, name: &str, args: Value) -> Step {
        model().command(text, name, Some(args)).unwrap()
    }

    #[test]
    fn rows_are_eight_keys_with_three_detail_lines() {
        let keys: Vec<&str> = ITEMS.iter().map(|item| item.key).collect();
        assert_eq!(keys, ["a", "b", "c", "d", "e", "f", "g", "h"]);
        let titles: HashSet<&str> = ITEMS.iter().map(|item| item.title).collect();
        assert_eq!(titles.len(), ITEMS.len());
        for item in &ITEMS {
            assert_eq!(find(item.key), Some(item));
            assert!(item.details.iter().all(|line| !line.is_empty()));
        }
        assert_eq!(find("z"), None);
    }

    #[test]
    fn new_panel_selects_the_first_row_or_the_row_of_the_context() {
        let model = model();
        let initial = model.init(None).unwrap();
        assert_eq!(initial, text("a", &[], false));
        model.check(&initial).unwrap();
        assert_eq!(state(&initial), State::initial());

        let picked = model.init(Some(json!({"key": "e"}))).unwrap();
        assert_eq!(picked, text("e", &[], false));

        let refusal = model.init(Some(json!({"key": "z"}))).unwrap_err();
        assert_eq!(
            (refusal.field.as_str(), refusal.reason.as_str()),
            ("key", KEY_UNKNOWN)
        );
        let refusal = model.init(Some(json!({"row": "a"}))).unwrap_err();
        assert_eq!(refusal.reason, Refusal::ARGS_INVALID);
    }

    #[test]
    fn reopen_with_another_key_remembers_the_current_one() {
        let start = text("a", &[], true);
        let next = changed(reopen(&start, "c"));
        assert_eq!(next, text("c", &["a"], true));
        let next = changed(reopen(&next, "e"));
        assert_eq!(next, text("e", &["a", "c"], true));
    }

    #[test]
    fn reopen_with_the_same_key_or_without_context_changes_nothing() {
        let start = text("c", &["a"], false);
        assert_eq!(reopen(&start, "c"), Step::Unchanged);
        assert_eq!(model().reopen(&start, None).unwrap(), Step::Unchanged);
    }

    #[test]
    fn reopen_with_an_unknown_key_is_refused_on_the_key_field() {
        let start = text("c", &["a"], false);
        let refusal = model()
            .reopen(&start, Some(json!({"key": "z"})))
            .unwrap_err();
        assert_eq!(refusal.field, "key");
        assert_eq!(refusal.reason, KEY_UNKNOWN);
        let refusal = model()
            .reopen(&start, Some(json!({"key": "c", "extra": 1})))
            .unwrap_err();
        assert_eq!(refusal.reason, Refusal::ARGS_INVALID);
    }

    #[test]
    fn history_keeps_the_last_thirty_two_keys() {
        let mut current = model().init(None).unwrap();
        let keys: Vec<&str> = ITEMS.iter().map(|item| item.key).collect();
        let mut visited = vec!["a"];
        // 40 переходов по кругу: соседние ключи разные.
        for step in 1..=40 {
            let key = keys[step % keys.len()];
            current = changed(reopen(&current, key));
            visited.push(key);
        }
        let last = state(&current);
        assert_eq!(last.key, visited[40]);
        assert_eq!(last.history.len(), HISTORY_MAX);
        assert_eq!(last.history, visited[8..40]);
        model().check(&current).unwrap();

        // Ровно 32 записи ещё не вытесняют.
        let mut current = model().init(None).unwrap();
        for step in 1..=HISTORY_MAX {
            current = changed(reopen(&current, keys[step % keys.len()]));
        }
        let full = state(&current);
        assert_eq!(full.history.len(), HISTORY_MAX);
        assert_eq!(full.history[0], "a");
    }

    #[test]
    fn back_walks_the_history_down_to_empty() {
        let mut current = text("e", &["a", "c"], false);
        for (key, history) in [("c", vec!["a"]), ("a", vec![])] {
            current = changed(command(&current, BACK, json!({})));
            assert_eq!(current, text(key, &history, false));
        }
        assert_eq!(command(&current, BACK, json!({})), Step::Unchanged);
        assert_eq!(
            model().command(&current, BACK, None).unwrap(),
            Step::Unchanged
        );
    }

    #[test]
    fn select_changes_the_row_without_history() {
        let start = text("a", &["h"], true);
        let next = changed(command(&start, SELECT, json!({"key": "d"})));
        assert_eq!(next, text("d", &["h"], true));
        let refusal = model()
            .command(&start, SELECT, Some(json!({"key": "z"})))
            .unwrap_err();
        assert_eq!(
            (refusal.field.as_str(), refusal.reason.as_str()),
            ("key", KEY_UNKNOWN)
        );
    }

    #[test]
    fn details_flag_opens_and_closes() {
        let closed = text("d", &["a"], false);
        let open = changed(command(&closed, DETAILS, json!({"open": true})));
        assert_eq!(open, text("d", &["a"], true));
        let again = changed(command(&open, DETAILS, json!({"open": false})));
        assert_eq!(again, closed);
    }

    #[test]
    fn repeating_the_same_value_changes_nothing() {
        let current = text("d", &["a"], true);
        assert_eq!(
            command(&current, SELECT, json!({"key": "d"})),
            Step::Unchanged
        );
        assert_eq!(
            command(&current, DETAILS, json!({"open": true})),
            Step::Unchanged
        );
        assert_eq!(reopen(&current, "d"), Step::Unchanged);
        let empty = text("d", &[], false);
        assert_eq!(command(&empty, BACK, json!({})), Step::Unchanged);
    }

    #[test]
    fn examples_of_every_action_parse() {
        model().check_examples().unwrap();
        let names: Vec<&str> = MANIFEST.commands.iter().map(|spec| spec.name).collect();
        assert_eq!(names, [SELECT, DETAILS, BACK]);
        for name in names {
            assert!(name.starts_with("demo-list."));
        }
    }

    #[test]
    fn unknown_actions_and_loose_args_are_refused() {
        let current = text("a", &[], false);
        let model = model();
        let refusal = model.command(&current, "demo-list.open", None).unwrap_err();
        assert_eq!(refusal.reason, Refusal::COMMAND_UNKNOWN);
        for (name, args) in [
            (SELECT, json!({})),
            (SELECT, json!({"key": "a", "extra": 1})),
            (DETAILS, json!({"open": "yes"})),
            (BACK, json!({"key": "a"})),
        ] {
            let refusal = model.command(&current, name, Some(args)).unwrap_err();
            assert_eq!(refusal.reason, Refusal::ARGS_INVALID, "{name}");
        }
    }

    #[test]
    fn stored_data_must_keep_the_invariants() {
        let model = model();
        model.check(&text("h", &["a", "a", "b"], true)).unwrap();
        let full: Vec<&str> = std::iter::repeat_n("a", HISTORY_MAX).collect();
        model.check(&text("b", &full, false)).unwrap();
        let over: Vec<&str> = std::iter::repeat_n("a", HISTORY_MAX + 1).collect();
        let broken = [
            text("z", &[], false),
            text("a", &["z"], false),
            text("a", &over, false),
            json!({"key": "a", "history": []}).to_string(),
            json!({"key": "a", "history": [], "details": false, "extra": 1}).to_string(),
            "null".to_owned(),
        ];
        for data in broken {
            let refusal = model.check(&data).unwrap_err();
            assert_eq!(refusal.reason, Refusal::STATE_CORRUPT, "{data}");
        }
    }

    #[test]
    fn older_data_does_not_exist() {
        assert_eq!(MANIFEST.state_version, 1);
        assert!(model().migrate(0, "null").is_err());
    }

    /// Размер тела каждого класса: не ниже `min_size` плагина.
    const SIZES: [(SizeClass, f64, f64); 4] = [
        (SizeClass::Narrow, 300.0, 400.0),
        (SizeClass::Regular, 700.0, 400.0),
        (SizeClass::Wide, 1200.0, 400.0),
        (SizeClass::ShortBar, 600.0, 200.0),
    ];

    /// Таблица плана: варианты зоны в порядке [`SIZES`].
    const TABLE: [(Zone, [&str; 4]); 4] = [
        (Zone::Left, [DRILL, DRILL, SPLIT, BAR]),
        (Zone::Right, [DRILL, DRILL, SPLIT, BAR]),
        (Zone::Center, [DRILL, SPLIT, SPLIT, BAR]),
        (Zone::Bottom, [DRILL, BAR, BAR, BAR]),
    ];

    /// Фикстура каждой из 16 пар зоны и класса размера.
    fn pair_fixtures() -> Vec<Fixture> {
        const NAMES: [[&str; 4]; 4] = [
            ["left-narrow", "left-regular", "left-wide", "left-short"],
            ["right-narrow", "right-regular", "right-wide", "right-short"],
            [
                "center-narrow",
                "center-regular",
                "center-wide",
                "center-short",
            ],
            [
                "bottom-narrow",
                "bottom-regular",
                "bottom-wide",
                "bottom-short",
            ],
        ];
        let mut all = Vec::new();
        for ((zone, _), names) in TABLE.into_iter().zip(NAMES) {
            for ((size, width, height), name) in SIZES.into_iter().zip(names) {
                let geometry = Geometry::of(zone, width, height);
                assert_eq!(geometry.size, size, "{name}");
                all.push(Fixture { name, geometry });
            }
        }
        all
    }

    #[test]
    fn every_zone_and_size_class_gets_the_variant_of_the_table() {
        let initial = model().init(None).unwrap();
        for (zone, variants) in TABLE {
            for ((size, width, height), expected) in SIZES.into_iter().zip(variants) {
                let geometry = Geometry::of(zone, width, height);
                let presentation = model().adapt(&geometry, &initial).unwrap();
                assert_eq!(presentation.variant, expected, "{zone:?} {size:?}");
                assert_eq!(VARIANTS.get(zone, size), expected, "{zone:?} {size:?}");
                assert_eq!(
                    contract::judge(&presentation, &geometry, &MANIFEST, expected),
                    Ok(()),
                    "{zone:?} {size:?}"
                );
            }
        }
    }

    #[test]
    fn contract_fixtures_and_all_sixteen_pairs_pass() {
        let model = model();
        let mut all = fixtures(&METRICS);
        let from_layout = all.len();
        all.extend(pair_fixtures());
        for data in [text("a", &[], false), text("e", &["a"], true)] {
            let report = contract::check(&model, &data, &all, &VARIANTS);
            assert!(report.passed(), "{data}\n{report}");
            assert_eq!(report.entries.len(), all.len());
            for entry in &report.entries {
                let fits = MANIFEST
                    .min_size
                    .admits(entry.geometry.width, entry.geometry.height);
                let expected = if fits {
                    Outcome::Adapted
                } else {
                    Outcome::Stub
                };
                assert_eq!(entry.outcome, expected, "{}", entry.fixture);
            }
            let adapted: HashSet<(Zone, SizeClass)> = report
                .entries
                .iter()
                .filter(|entry| entry.outcome == Outcome::Adapted)
                .map(|entry| (entry.geometry.zone, entry.geometry.size))
                .collect();
            assert_eq!(adapted.len(), 16, "{adapted:?}");
            // Фикстуры раскладки сами доходят до каждой зоны.
            for zone in Zone::ALL {
                let reached = report.entries[..from_layout]
                    .iter()
                    .any(|entry| entry.geometry.zone == zone && entry.outcome == Outcome::Adapted);
                assert!(reached, "{zone:?}");
            }
        }
    }

    #[test]
    fn body_at_min_size_is_adapted_and_a_point_less_is_the_stub() {
        let MinSize { width, height } = MANIFEST.min_size;
        assert_eq!((width, height), (260.0, 160.0));
        let model = model();
        let data = model.init(None).unwrap();
        for zone in Zone::ALL {
            let cases = [
                Fixture {
                    name: "min-size",
                    geometry: Geometry::of(zone, width, height),
                },
                Fixture {
                    name: "narrower",
                    geometry: Geometry::of(zone, width - 1.0, height),
                },
                Fixture {
                    name: "shorter",
                    geometry: Geometry::of(zone, width, height - 1.0),
                },
            ];
            let report = contract::check(&model, &data, &cases, &VARIANTS);
            let outcomes: Vec<&Outcome> =
                report.entries.iter().map(|entry| &entry.outcome).collect();
            assert_eq!(
                outcomes,
                [&Outcome::Adapted, &Outcome::Stub, &Outcome::Stub],
                "{zone:?}\n{report}"
            );

            // Ровно `min_size` — не заглушка, а вариант `bar` с обоими регионами.
            let presentation = model.adapt(&cases[0].geometry, &data).unwrap();
            assert_eq!(presentation.variant, BAR);
            let names: Vec<&str> = presentation
                .regions
                .iter()
                .map(|region| region.name)
                .collect();
            assert_eq!(names, [LIST_REGION, DETAILS_REGION]);
        }
    }

    fn regions(zone: Zone, width: f64, height: f64, data: &str) -> Vec<(&'static str, Rect)> {
        model()
            .adapt(&Geometry::of(zone, width, height), data)
            .unwrap()
            .regions
            .into_iter()
            .map(|region| (region.name, region.frame))
            .collect()
    }

    fn rect(x: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y: 0.0,
            width,
            height,
        }
    }

    #[test]
    fn drill_shows_one_level_and_the_other_variants_show_both() {
        let closed = text("c", &[], false);
        let open = text("c", &[], true);
        assert_eq!(
            regions(Zone::Right, 300.0, 400.0, &closed),
            [(LIST_REGION, rect(0.0, 300.0, 400.0))]
        );
        assert_eq!(
            regions(Zone::Right, 300.0, 400.0, &open),
            [(DETAILS_REGION, rect(0.0, 300.0, 400.0))]
        );
        // Флаг деталей на `split` и `bar` не влияет.
        for data in [&closed, &open] {
            assert_eq!(
                regions(Zone::Center, 700.0, 400.0, data),
                [
                    (LIST_REGION, rect(0.0, 280.0, 400.0)),
                    (DETAILS_REGION, rect(280.0, 420.0, 400.0)),
                ]
            );
            assert_eq!(
                regions(Zone::Bottom, 700.0, 200.0, data),
                [
                    (LIST_REGION, rect(0.0, 420.0, 200.0)),
                    (DETAILS_REGION, rect(420.0, 280.0, 200.0)),
                ]
            );
        }
    }

    #[test]
    fn reach_names_the_level_of_every_action() {
        let reach = |zone, width, height, data: &str| -> Vec<(&str, Via)> {
            model()
                .adapt(&Geometry::of(zone, width, height), data)
                .unwrap()
                .reach
                .into_iter()
                .map(|reach| (reach.action, reach.via))
                .collect()
        };
        let closed = text("c", &[], false);
        let open = text("c", &[], true);
        assert_eq!(
            reach(Zone::Right, 300.0, 400.0, &closed),
            [
                (SELECT, Via::Direct),
                (DETAILS, Via::Drill),
                (BACK, Via::Direct)
            ]
        );
        assert_eq!(
            reach(Zone::Right, 300.0, 400.0, &open),
            [
                (SELECT, Via::Drill),
                (DETAILS, Via::Drill),
                (BACK, Via::Drill)
            ]
        );
        for data in [&closed, &open] {
            assert_eq!(
                reach(Zone::Center, 700.0, 400.0, data),
                [
                    (SELECT, Via::Direct),
                    (DETAILS, Via::Direct),
                    (BACK, Via::Direct)
                ]
            );
        }
    }
}
