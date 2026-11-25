//! Модель без данных: каждый вызов открывает новую панель, действий нет, представление — один регион на всё тело.

use serde::de::{Deserialize, Deserializer, Error};
use serde_json::Value;
use shellio_layout::{Geometry, MinSize, Rect, Zone};
use shellio_plugin_api::{
    Cardinality, Manifest, PanelModel, Presentation, Refusal, Region, SizeVariants, VariantTable,
    ZoneSet,
};

pub const MANIFEST: Manifest = Manifest {
    id: "terminal",
    title: "Терминал",
    icon: "term",
    placements: ZoneSet::ALL,
    default_zone: Zone::Center,
    cardinality: Cardinality::Multi,
    min_size: MinSize::DEFAULT,
    commands: &[],
    keymap: &[],
    state_version: 1,
    builtin: true,
};

/// Боковая зона.
pub const SIDE: &str = "side";
/// Центр.
pub const FULL: &str = "full";
/// Нижняя зона.
pub const BAR: &str = "bar";

const fn everywhere(variant: &'static str) -> SizeVariants {
    SizeVariants {
        narrow: variant,
        regular: variant,
        wide: variant,
        short_bar: variant,
    }
}

/// Вариант зависит только от зоны.
pub const VARIANTS: VariantTable = VariantTable {
    left: everywhere(SIDE),
    right: everywhere(SIDE),
    center: everywhere(FULL),
    bottom: everywhere(BAR),
};

/// Контекст открытия: его нет.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Args {}

/// Действий у заполнителя нет.
pub enum Command {}

impl<'de> Deserialize<'de> for Command {
    fn deserialize<D: Deserializer<'de>>(_deserializer: D) -> Result<Self, D::Error> {
        Err(D::Error::custom("у плагина нет действий"))
    }
}

pub struct Model;

impl PanelModel for Model {
    type State = ();
    type Args = Args;
    type Command = Command;

    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn init(&self, _args: Option<Args>) -> Result<(), Refusal> {
        Ok(())
    }

    fn reopen(&self, _state: &(), _args: Option<Args>) -> Result<(), Refusal> {
        Ok(())
    }

    fn apply(&self, _state: &(), command: Command) -> Result<(), Refusal> {
        match command {}
    }

    fn migrate(&self, from: u32, _data: Value) -> Result<(), Refusal> {
        Err(Refusal::new(
            "panel",
            "state_version_unknown",
            format!("у плагина «{}» нет данных версии {from}", MANIFEST.id),
        ))
    }

    fn adapt(&self, geometry: &Geometry, _state: &()) -> Presentation {
        let variant = match geometry.zone {
            Zone::Left => SIDE,
            Zone::Right => SIDE,
            Zone::Center => FULL,
            Zone::Bottom => BAR,
        };
        Presentation {
            variant,
            regions: vec![Region {
                name: "body",
                frame: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: geometry.width,
                    height: geometry.height,
                },
            }],
            reach: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shellio_plugin_api::{DynModel, Step};

    use super::*;

    fn model() -> DynModel {
        DynModel::new(Model)
    }

    #[test]
    fn panel_has_no_data() {
        let model = model();
        let state = model.init(None).unwrap();
        assert_eq!(state, "null");
        model.check(&state).unwrap();
        assert_eq!(model.init(Some(json!({}))).unwrap(), "null");
        assert_eq!(model.reopen(&state, None).unwrap(), Step::Unchanged);
        assert_eq!(
            model.reopen(&state, Some(json!({}))).unwrap(),
            Step::Unchanged
        );
    }

    #[test]
    fn open_context_and_actions_are_refused() {
        let model = model();
        let refusal = model.init(Some(json!({"path": "/"}))).unwrap_err();
        assert_eq!(refusal.reason, Refusal::ARGS_INVALID);
        let refusal = model
            .command("null", "terminal.anything", None)
            .unwrap_err();
        assert_eq!(refusal.reason, Refusal::COMMAND_UNKNOWN);
        assert!(serde_json::from_value::<Command>(json!({"name": "terminal.anything"})).is_err());
        model.check_examples().unwrap();
    }

    #[test]
    fn older_data_does_not_exist() {
        assert!(model().migrate(0, "null").is_err());
    }

    #[test]
    fn body_is_one_region_and_the_variant_follows_the_zone() {
        let model = model();
        for (zone, variant) in [
            (Zone::Left, SIDE),
            (Zone::Right, SIDE),
            (Zone::Center, FULL),
            (Zone::Bottom, BAR),
        ] {
            let geometry = Geometry::of(zone, 320.0, 240.0);
            let presentation = model.adapt(&geometry, "null").unwrap();
            assert_eq!(presentation.variant, variant);
            assert_eq!(presentation.variant, VARIANTS.get(zone, geometry.size));
            assert!(presentation.reach.is_empty());
            let [region] = presentation.regions.as_slice() else {
                panic!("один регион на всё тело: {:?}", presentation.regions);
            };
            assert_eq!(
                (
                    region.frame.x,
                    region.frame.y,
                    region.frame.width,
                    region.frame.height
                ),
                (0.0, 0.0, 320.0, 240.0)
            );
        }
    }
}
