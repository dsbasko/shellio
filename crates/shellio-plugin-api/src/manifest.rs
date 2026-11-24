//! Манифест плагина и его описание для чтения `query.plugins` (ADR-0008, «Уточнено в P1.5
//! (планирование)»).

use std::fmt;

use serde::{Deserialize, Serialize};
use shellio_layout::{MinSize, Zone};

use crate::id::PluginId;

/// Сколько экземпляров панели плагина бывает и что служит ключом экземпляра.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    /// Каждый вызов открывает новую панель.
    Multi,
    /// Одна панель на рабочую область.
    PerWorkbench,
    /// Одна панель на проект.
    PerProject,
    /// Одна панель на воркспейс.
    PerWorkspace,
    /// Одна панель на приложение.
    PerApp,
}

impl Cardinality {
    /// Все виды.
    pub const ALL: [Self; 5] = [
        Self::Multi,
        Self::PerWorkbench,
        Self::PerProject,
        Self::PerWorkspace,
        Self::PerApp,
    ];

    /// Имя в JSON и в отпечатке реестра.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Multi => "multi",
            Self::PerWorkbench => "per_workbench",
            Self::PerProject => "per_project",
            Self::PerWorkspace => "per_workspace",
            Self::PerApp => "per_app",
        }
    }
}

impl fmt::Display for Cardinality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Множество зон. В JSON — список зон в порядке [`Zone::ALL`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "Vec<Zone>", into = "Vec<Zone>")]
pub struct ZoneSet(u8);

const fn bit(zone: Zone) -> u8 {
    match zone {
        Zone::Left => 1,
        Zone::Right => 2,
        Zone::Center => 4,
        Zone::Bottom => 8,
    }
}

impl ZoneSet {
    pub const EMPTY: Self = Self(0);
    /// Все четыре зоны: их объявляет каждый встроенный плагин.
    pub const ALL: Self = Self::of(&Zone::ALL);

    #[must_use]
    pub const fn of(zones: &[Zone]) -> Self {
        let mut bits = 0;
        let mut index = 0;
        while index < zones.len() {
            bits |= bit(zones[index]);
            index += 1;
        }
        Self(bits)
    }

    #[must_use]
    pub const fn contains(self, zone: Zone) -> bool {
        self.0 & bit(zone) != 0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Зоны множества в порядке [`Zone::ALL`].
    pub fn iter(self) -> impl Iterator<Item = Zone> {
        Zone::ALL
            .into_iter()
            .filter(move |&zone| self.contains(zone))
    }
}

impl From<Vec<Zone>> for ZoneSet {
    fn from(zones: Vec<Zone>) -> Self {
        Self::of(&zones)
    }
}

impl From<ZoneSet> for Vec<Zone> {
    fn from(set: ZoneSet) -> Self {
        set.iter().collect()
    }
}

impl fmt::Debug for ZoneSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// Действие плагина: полное имя `<plugin>.<name>` — оно же имя на проводе и в keymap — и пример
/// `args` текстом JSON-объекта. Пример разбирает тест плагина ([`crate::DynModel::check_examples`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub args: &'static str,
}

/// Клавиша действия по умолчанию. Ссылается на действие манифеста, привязку делает P1.6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyDefault {
    pub action: &'static str,
    pub key: &'static str,
}

/// Манифест плагина. Его проверяет [`crate::Registry::build`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Manifest {
    pub id: &'static str,
    pub title: &'static str,
    /// Имя символа SF Symbols.
    pub icon: &'static str,
    pub placements: ZoneSet,
    pub default_zone: Zone,
    pub cardinality: Cardinality,
    pub min_size: MinSize,
    pub commands: &'static [CommandSpec],
    pub keymap: &'static [KeyDefault],
    /// Версия данных панели, не меньше 1.
    pub state_version: u32,
    /// Встроенный плагин, а не подставная регистрация тестов: он обязан объявить все четыре зоны.
    pub builtin: bool,
}

impl Manifest {
    /// Есть ли у плагина действие с таким полным именем.
    #[must_use]
    pub fn has_command(&self, name: &str) -> bool {
        self.commands.iter().any(|spec| spec.name == name)
    }

    pub(crate) fn info(&self, id: PluginId) -> PluginInfo {
        PluginInfo {
            id,
            title: self.title.to_owned(),
            icon: self.icon.to_owned(),
            placements: self.placements,
            default_zone: self.default_zone,
            cardinality: self.cardinality,
            min_size: self.min_size,
            commands: self
                .commands
                .iter()
                .map(|spec| spec.name.to_owned())
                .collect(),
            state_version: self.state_version,
        }
    }
}

/// Описание зарегистрированного плагина для чтения `query.plugins`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginInfo {
    pub id: PluginId,
    pub title: String,
    pub icon: String,
    pub placements: ZoneSet,
    pub default_zone: Zone,
    pub cardinality: Cardinality,
    pub min_size: MinSize,
    /// Полные имена действий в порядке манифеста.
    pub commands: Vec<String>,
    pub state_version: u32,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn cardinality_names_match_json() {
        for cardinality in Cardinality::ALL {
            assert_eq!(
                serde_json::to_value(cardinality).unwrap(),
                json!(cardinality.as_str())
            );
            assert_eq!(cardinality.to_string(), cardinality.as_str());
        }
        assert_eq!(Cardinality::PerApp.as_str(), "per_app");
        assert!(serde_json::from_value::<Cardinality>(json!("single")).is_err());
    }

    #[test]
    fn zone_set_is_ordered_and_const() {
        const SIDES: ZoneSet = ZoneSet::of(&[Zone::Bottom, Zone::Left, Zone::Left]);
        assert_eq!(SIDES.iter().collect::<Vec<_>>(), [Zone::Left, Zone::Bottom]);
        assert!(SIDES.contains(Zone::Bottom));
        assert!(!SIDES.contains(Zone::Center));
        assert!(!SIDES.is_empty());
        assert!(ZoneSet::EMPTY.is_empty());
        assert_eq!(ZoneSet::ALL.iter().collect::<Vec<_>>(), Zone::ALL);
        assert_eq!(ZoneSet::default(), ZoneSet::EMPTY);
        assert_eq!(format!("{SIDES:?}"), "{Left, Bottom}");
    }

    #[test]
    fn zone_set_json_is_a_list_of_zones() {
        let set = ZoneSet::of(&[Zone::Center, Zone::Right]);
        let json = serde_json::to_value(set).unwrap();
        assert_eq!(json, json!(["right", "center"]));
        assert_eq!(serde_json::from_value::<ZoneSet>(json).unwrap(), set);
        assert_eq!(
            serde_json::from_value::<ZoneSet>(json!(["left", "left"])).unwrap(),
            ZoneSet::of(&[Zone::Left])
        );
        assert!(serde_json::from_value::<ZoneSet>(json!(["top"])).is_err());
    }

    #[test]
    fn plugin_info_mirrors_the_manifest() {
        const MANIFEST: Manifest = Manifest {
            id: "notes",
            title: "Заметки",
            icon: "note.text",
            placements: ZoneSet::of(&[Zone::Right, Zone::Center]),
            default_zone: Zone::Right,
            cardinality: Cardinality::PerWorkbench,
            min_size: MinSize {
                width: 260.0,
                height: 160.0,
            },
            commands: &[
                CommandSpec {
                    name: "notes.pick",
                    args: r#"{"key":"a"}"#,
                },
                CommandSpec {
                    name: "notes.back",
                    args: "{}",
                },
            ],
            keymap: &[],
            state_version: 3,
            builtin: false,
        };
        assert!(MANIFEST.has_command("notes.back"));
        assert!(!MANIFEST.has_command("back"));
        let info = MANIFEST.info(PluginId::new(MANIFEST.id).unwrap());
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            json,
            json!({
                "id": "notes",
                "title": "Заметки",
                "icon": "note.text",
                "placements": ["right", "center"],
                "default_zone": "right",
                "cardinality": "per_workbench",
                "min_size": {"width": 260.0, "height": 160.0},
                "commands": ["notes.pick", "notes.back"],
                "state_version": 3,
            })
        );
        assert_eq!(serde_json::from_value::<PluginInfo>(json).unwrap(), info);
    }
}
