//! Модель «Демо: поле»: сохранённое значение и счётчик правок. Черновик поля в данные панели не
//! входит: он принадлежит телу.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use shellio_layout::{Geometry, MinSize, Rect, SizeClass, Zone};
use shellio_plugin_api::{
    Cardinality, CommandSpec, KeyDefault, Manifest, PanelModel, Presentation, Reach, Refusal,
    Region, SizeVariants, VariantTable, Via, ZoneSet,
};
use shellio_theme::METRICS;

/// Сохранить значение поля.
pub const SET: &str = "demo-native.set";

pub const MANIFEST: Manifest = Manifest {
    id: "demo-native",
    title: "Демо: поле",
    icon: "character.cursor.ibeam",
    placements: ZoneSet::ALL,
    default_zone: Zone::Center,
    cardinality: Cardinality::PerApp,
    min_size: MinSize {
        width: 220.0,
        height: 140.0,
    },
    commands: &[CommandSpec {
        name: SET,
        args: r#"{"value":"shellio"}"#,
        title: "Применить поле",
    }],
    keymap: &[KeyDefault {
        action: SET,
        key: "s",
    }],
    list_keys: false,
    filter: false,
    input: true,
    state_version: 1,
    builtin: true,
};

/// Поле и подпись в строку.
pub const ROW: &str = "row";
/// Поле над подписью.
pub const STACK: &str = "stack";
/// Поле по центру тела, подпись сбоку.
pub const CENTER: &str = "center";

/// Регион поля ввода.
pub const FIELD_REGION: &str = "field";
/// Регион подписи с сохранённым значением.
pub const CAPTION_REGION: &str = "caption";

const SIDE: SizeVariants = SizeVariants {
    narrow: STACK,
    regular: STACK,
    wide: STACK,
    short_bar: ROW,
};

const fn everywhere(variant: &'static str) -> SizeVariants {
    SizeVariants {
        narrow: variant,
        regular: variant,
        wide: variant,
        short_bar: variant,
    }
}

/// Сначала `ShortBar`, затем зона.
pub const VARIANTS: VariantTable = VariantTable {
    left: SIDE,
    right: SIDE,
    center: SizeVariants {
        narrow: CENTER,
        regular: CENTER,
        wide: CENTER,
        short_bar: ROW,
    },
    bottom: everywhere(ROW),
};

/// Сколько скалярных значений Unicode держит значение поля.
pub const VALUE_MAX: usize = 256;

/// Причина отказа: значение длиннее [`VALUE_MAX`].
pub const VALUE_TOO_LONG: &str = "value_too_long";

fn length(value: &str) -> usize {
    value.chars().count()
}

/// Данные панели.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawState")]
pub struct State {
    /// Сохранённое значение поля.
    pub value: String,
    /// Сколько раз значение менялось.
    pub edits: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawState {
    value: String,
    edits: u64,
}

impl TryFrom<RawState> for State {
    type Error = String;

    fn try_from(raw: RawState) -> Result<Self, Self::Error> {
        let length = length(&raw.value);
        if length > VALUE_MAX {
            return Err(format!(
                "значение из {length} символов при пределе {VALUE_MAX}"
            ));
        }
        Ok(Self {
            value: raw.value,
            edits: raw.edits,
        })
    }
}

/// Контекст открытия: его нет.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Args {}

#[derive(Deserialize)]
#[serde(tag = "name", content = "args", deny_unknown_fields)]
pub enum Command {
    #[serde(rename = "demo-native.set")]
    Set { value: String },
}

/// Вариант представления по зоне и классу размера.
fn variant(geometry: &Geometry) -> &'static str {
    match geometry.zone {
        Zone::Left => SIDE.get(geometry.size),
        Zone::Right => SIDE.get(geometry.size),
        Zone::Center => match geometry.size {
            SizeClass::ShortBar => ROW,
            SizeClass::Narrow | SizeClass::Regular | SizeClass::Wide => CENTER,
        },
        Zone::Bottom => ROW,
    }
}

/// Отступ поля и подписи от края тела: два промежутка темы.
const PAD: f64 = 2.0 * METRICS.gap;
/// Промежуток между полем и подписью.
const GAP: f64 = METRICS.gap;
/// Высота поля и подписи: строка основного текста с отступом и линией сверху и снизу, как у
/// компонента поля.
pub const LINE: f64 = METRICS.line_height.body + 2.0 * (METRICS.gap + METRICS.line);
/// Наибольшая ширина пары поля и подписи в `center`: граница раскладки тела, не отступ темы.
const CENTER_MAX: f64 = 560.0;
/// Доля поля в ширине пары: в `row` поровну с подписью, в `center` поле шире.
const ROW_FIELD_SHARE: f64 = 0.5;
const CENTER_FIELD_SHARE: f64 = 0.6;

fn region(name: &'static str, x: f64, y: f64, width: f64) -> Region {
    Region {
        name,
        frame: Rect {
            x,
            y,
            width,
            height: LINE,
        },
    }
}

/// Поле и подпись в одну строку шириной `width` с левым верхним углом `(x, y)`.
fn in_line(x: f64, y: f64, width: f64, share: f64) -> Vec<Region> {
    let field = ((width - GAP) * share).floor();
    vec![
        region(FIELD_REGION, x, y, field),
        region(CAPTION_REGION, x + field + GAP, y, width - GAP - field),
    ]
}

fn regions(variant: &'static str, geometry: &Geometry) -> Vec<Region> {
    let inner = geometry.width - 2.0 * PAD;
    match variant {
        STACK => vec![
            region(FIELD_REGION, PAD, PAD, inner),
            region(CAPTION_REGION, PAD, PAD + LINE + GAP, inner),
        ],
        CENTER => {
            let width = inner.min(CENTER_MAX);
            let x = ((geometry.width - width) / 2.0).floor();
            let y = ((geometry.height - LINE) / 2.0).floor();
            in_line(x, y, width, CENTER_FIELD_SHARE)
        }
        _ => in_line(PAD, PAD, inner, ROW_FIELD_SHARE),
    }
}

pub struct Model;

impl PanelModel for Model {
    type State = State;
    type Args = Args;
    type Command = Command;

    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn init(&self, _args: Option<Args>) -> Result<State, Refusal> {
        Ok(State::default())
    }

    fn reopen(&self, state: &State, _args: Option<Args>) -> Result<State, Refusal> {
        Ok(state.clone())
    }

    fn apply(&self, state: &State, command: Command) -> Result<State, Refusal> {
        let Command::Set { value } = command;
        let length = length(&value);
        if length > VALUE_MAX {
            return Err(Refusal::new(
                "value",
                VALUE_TOO_LONG,
                format!("значение длиннее {VALUE_MAX} символов: {length}"),
            ));
        }
        if value == state.value {
            return Ok(state.clone());
        }
        Ok(State {
            value,
            edits: state.edits.saturating_add(1),
        })
    }

    fn migrate(&self, from: u32, _data: Value) -> Result<State, Refusal> {
        Err(Refusal::new(
            "panel",
            "state_version_unknown",
            format!("у плагина «{}» нет данных версии {from}", MANIFEST.id),
        ))
    }

    fn adapt(&self, geometry: &Geometry, _state: &State) -> Presentation {
        let variant = variant(geometry);
        Presentation {
            variant,
            regions: regions(variant, geometry),
            reach: vec![Reach {
                action: SET,
                via: Via::Direct,
            }],
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
        side_pad: 8,
    };

    fn model() -> DynModel {
        DynModel::new(Model)
    }

    /// Данные панели текстом, как их пишет модель: порядок полей — порядок `State`.
    fn text(value: &str, edits: u64) -> String {
        serde_json::to_string(&State {
            value: value.to_owned(),
            edits,
        })
        .unwrap()
    }

    fn set(state: &str, value: &str) -> Result<Step, Refusal> {
        model().command(state, SET, Some(json!({"value": value})))
    }

    fn changed(step: Step) -> String {
        match step {
            Step::Changed(text) => text,
            Step::Unchanged => panic!("состояние не изменилось"),
        }
    }

    #[test]
    fn manifest_is_one_per_app_in_the_center() {
        assert_eq!(MANIFEST.id, "demo-native");
        assert_eq!(MANIFEST.title, "Демо: поле");
        assert_eq!(MANIFEST.cardinality, Cardinality::PerApp);
        assert_eq!(MANIFEST.default_zone, Zone::Center);
        assert_eq!(
            (MANIFEST.min_size.width, MANIFEST.min_size.height),
            (220.0, 140.0)
        );
        assert_eq!(MANIFEST.state_version, 1);
        assert_eq!(MANIFEST.placements, ZoneSet::ALL);
    }

    #[test]
    fn new_panel_has_an_empty_value_and_no_edits() {
        let model = model();
        let initial = model.init(None).unwrap();
        assert_eq!(initial, r#"{"value":"","edits":0}"#);
        model.check(&initial).unwrap();
        assert_eq!(model.init(Some(json!({}))).unwrap(), initial);
        let refusal = model.init(Some(json!({"value": "a"}))).unwrap_err();
        assert_eq!(refusal.reason, Refusal::ARGS_INVALID);
    }

    #[test]
    fn reopen_changes_nothing() {
        let current = text("shellio", 3);
        assert_eq!(model().reopen(&current, None).unwrap(), Step::Unchanged);
        assert_eq!(
            model().reopen(&current, Some(json!({}))).unwrap(),
            Step::Unchanged
        );
    }

    #[test]
    fn set_counts_every_new_value() {
        let mut current = model().init(None).unwrap();
        for (edits, value) in [(1, "a"), (2, "ab"), (3, ""), (4, "проверка")] {
            current = changed(set(&current, value).unwrap());
            assert_eq!(current, text(value, edits));
        }
    }

    #[test]
    fn the_same_text_changes_nothing() {
        let current = text("shellio", 2);
        assert_eq!(set(&current, "shellio").unwrap(), Step::Unchanged);
        let empty = model().init(None).unwrap();
        assert_eq!(set(&empty, "").unwrap(), Step::Unchanged);
    }

    #[test]
    fn value_holds_256_scalar_values_and_refuses_257() {
        let current = text("a", 1);
        // Кириллица и эмодзи: предел считается в скалярных значениях, а не в байтах.
        for unit in ["x", "я", "🙂"] {
            let fits = unit.repeat(VALUE_MAX);
            assert_eq!(fits.chars().count(), 256);
            let next = changed(set(&current, &fits).unwrap());
            assert_eq!(next, text(&fits, 2), "{unit}");
            model().check(&next).unwrap();

            let over = unit.repeat(VALUE_MAX + 1);
            let refusal = set(&current, &over).unwrap_err();
            assert_eq!(
                (refusal.field.as_str(), refusal.reason.as_str()),
                ("value", VALUE_TOO_LONG),
                "{unit}"
            );
            assert_eq!(refusal.message, "значение длиннее 256 символов: 257");
        }
    }

    #[test]
    fn counter_stops_at_its_limit() {
        let current = text("a", u64::MAX);
        assert_eq!(
            changed(set(&current, "b").unwrap()),
            text("b", u64::MAX),
            "счётчик не переполняется"
        );
    }

    #[test]
    fn examples_of_every_action_parse() {
        model().check_examples().unwrap();
        let names: Vec<&str> = MANIFEST.commands.iter().map(|spec| spec.name).collect();
        assert_eq!(names, [SET]);
        assert!(SET.starts_with("demo-native."));
    }

    #[test]
    fn unknown_actions_and_loose_args_are_refused() {
        let current = text("a", 1);
        let model = model();
        let refusal = model
            .command(&current, "demo-native.clear", None)
            .unwrap_err();
        assert_eq!(refusal.reason, Refusal::COMMAND_UNKNOWN);
        // Без аргументов действие не выполняется: пример `args` манифеста не подставляется.
        let refusal = model.command(&current, SET, None).unwrap_err();
        assert_eq!(refusal.reason, Refusal::ARGS_INVALID);
        for args in [
            json!({}),
            json!({"value": 1}),
            json!({"value": "a", "extra": 1}),
        ] {
            let refusal = model.command(&current, SET, Some(args)).unwrap_err();
            assert_eq!(refusal.reason, Refusal::ARGS_INVALID);
        }
    }

    #[test]
    fn stored_data_must_keep_the_invariants() {
        let model = model();
        model.check(&text(&"я".repeat(VALUE_MAX), 7)).unwrap();
        let broken = [
            text(&"я".repeat(VALUE_MAX + 1), 7),
            json!({"value": "a"}).to_string(),
            json!({"value": "a", "edits": -1}).to_string(),
            json!({"value": "a", "edits": 1, "extra": 1}).to_string(),
            "null".to_owned(),
            "{".to_owned(),
        ];
        for data in broken {
            let refusal = model.check(&data).unwrap_err();
            assert_eq!(refusal.reason, Refusal::STATE_CORRUPT, "{data}");
        }
    }

    #[test]
    fn older_data_does_not_exist() {
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
        (Zone::Left, [STACK, STACK, STACK, ROW]),
        (Zone::Right, [STACK, STACK, STACK, ROW]),
        (Zone::Center, [CENTER, CENTER, CENTER, ROW]),
        (Zone::Bottom, [ROW, ROW, ROW, ROW]),
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
        for data in [text("", 0), text(&"я".repeat(VALUE_MAX), 9)] {
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
            // Ровно `min_size` — класс `ShortBar`: поле и подпись в строку.
            let presentation = model.adapt(&cases[0].geometry, &data).unwrap();
            assert_eq!(presentation.variant, ROW);
        }
    }

    fn frames(zone: Zone, width: f64, height: f64) -> Vec<(&'static str, Rect)> {
        model()
            .adapt(&Geometry::of(zone, width, height), &text("", 0))
            .unwrap()
            .regions
            .into_iter()
            .map(|region| (region.name, region.frame))
            .collect()
    }

    fn rect(x: f64, y: f64, width: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height: 32.0,
        }
    }

    #[test]
    fn every_variant_shows_the_field_and_the_caption() {
        assert_eq!(
            frames(Zone::Right, 300.0, 400.0),
            [
                (FIELD_REGION, rect(12.0, 12.0, 276.0)),
                (CAPTION_REGION, rect(12.0, 50.0, 276.0)),
            ],
            "stack: поле над подписью"
        );
        assert_eq!(
            frames(Zone::Bottom, 700.0, 300.0),
            [
                (FIELD_REGION, rect(12.0, 12.0, 335.0)),
                (CAPTION_REGION, rect(353.0, 12.0, 335.0)),
            ],
            "row: поле и подпись в строку поровну"
        );
        assert_eq!(
            frames(Zone::Center, 900.0, 600.0),
            [
                (FIELD_REGION, rect(170.0, 284.0, 332.0)),
                (CAPTION_REGION, rect(508.0, 284.0, 222.0)),
            ],
            "center: пара не шире 560 точек по центру тела"
        );
        assert_eq!(
            frames(Zone::Center, 300.0, 400.0),
            [
                (FIELD_REGION, rect(12.0, 184.0, 162.0)),
                (CAPTION_REGION, rect(180.0, 184.0, 108.0)),
            ],
            "center в узком теле занимает ширину без отступов"
        );
    }

    #[test]
    fn the_only_action_is_always_direct() {
        for (zone, _) in TABLE {
            for (_, width, height) in SIZES {
                let presentation = model()
                    .adapt(&Geometry::of(zone, width, height), &text("a", 1))
                    .unwrap();
                let reach: Vec<(&str, Via)> = presentation
                    .reach
                    .into_iter()
                    .map(|reach| (reach.action, reach.via))
                    .collect();
                assert_eq!(reach, [(SET, Via::Direct)]);
            }
        }
    }
}
