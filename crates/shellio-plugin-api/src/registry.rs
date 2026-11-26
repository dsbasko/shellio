//! Реестр плагинов: проверенные записи в порядке регистрации (ADR-0008, «Реестр»).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use shellio_layout::Zone;

use crate::contract::VariantTable;
use crate::id::{PluginId, PluginIdError};
use crate::manifest::{Manifest, PluginInfo, ZoneSet};
use crate::model::{DynModel, PanelModel};

/// Пространства имён команд, чтений и действий хоста: первый сегмент имени до точки. Имя действия
/// плагина начинается с его id, поэтому действие плагина с таким id совпало бы с командой хоста,
/// нынешней или будущей. Тест ядра сверяет со списком каждое имя команды хоста.
pub const HOST_NAMESPACES: &[&str] = &[
    "app",
    "cmdline",
    "gallery",
    "hints",
    "history",
    "keymap",
    "layout",
    "list",
    "mode",
    "palette",
    "panel",
    "project",
    "query",
    "tab",
    "ui",
    "window",
    "workbench",
    "workspace",
];

/// Наибольшая длина названия плагина в символах: правило названия панели в ядре.
pub const TITLE_MAX_CHARS: usize = 200;

/// Чистая запись регистрации: модель плагина с манифестом и таблица вариантов представления.
#[derive(Debug)]
pub struct Entry {
    model: DynModel,
    variants: VariantTable,
}

impl Entry {
    pub fn new<M: PanelModel>(model: M, variants: VariantTable) -> Self {
        Self {
            model: DynModel::new(model),
            variants,
        }
    }
}

/// Зарегистрированный плагин.
#[derive(Debug)]
pub struct Plugin {
    id: PluginId,
    model: DynModel,
    variants: VariantTable,
}

impl Plugin {
    #[must_use]
    pub fn id(&self) -> &PluginId {
        &self.id
    }

    #[must_use]
    pub fn manifest(&self) -> &'static Manifest {
        self.model.manifest()
    }

    #[must_use]
    pub fn model(&self) -> &DynModel {
        &self.model
    }

    /// Таблица вариантов для [`crate::contract::check`].
    #[must_use]
    pub fn variants(&self) -> &VariantTable {
        &self.variants
    }

    #[must_use]
    pub fn info(&self) -> PluginInfo {
        self.manifest().info(self.id.clone())
    }
}

/// Причина отказа в сборке реестра. `plugin` — id манифеста как записан.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    InvalidId {
        plugin: String,
        error: PluginIdError,
    },
    DuplicateId {
        plugin: String,
    },
    /// После обрезки пробелов название пустое.
    TitleEmpty {
        plugin: String,
    },
    TitleControlChar {
        plugin: String,
        ch: char,
    },
    /// Название длиннее [`TITLE_MAX_CHARS`] символов.
    TitleTooLong {
        plugin: String,
        chars: usize,
    },
    /// Встроенный плагин объявил не все четыре зоны.
    BuiltinPlacements {
        plugin: String,
        missing: Zone,
    },
    DefaultZoneNotPlaced {
        plugin: String,
        zone: Zone,
    },
    /// `min_size` не конечный или не положительный.
    MinSize {
        plugin: String,
    },
    /// `state_version` меньше 1.
    StateVersion {
        plugin: String,
    },
    /// Имя действия не вида `<plugin>.<name>` с именем из `[a-z0-9_.-]`.
    ActionName {
        plugin: String,
        action: String,
    },
    DuplicateAction {
        plugin: String,
        action: String,
    },
    /// Имя действия лежит в пространстве имён команд хоста.
    HostCommand {
        plugin: String,
        action: String,
    },
    /// `keymap` ссылается на действие, которого нет в манифесте.
    KeymapAction {
        plugin: String,
        action: String,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId { plugin, error } => write!(f, "плагин «{plugin}»: {error}"),
            Self::DuplicateId { plugin } => write!(f, "плагин «{plugin}» зарегистрирован дважды"),
            Self::TitleEmpty { plugin } => write!(f, "плагин «{plugin}»: название пустое"),
            Self::TitleControlChar { plugin, ch } => write!(
                f,
                "плагин «{plugin}»: название содержит управляющий символ U+{:04X}",
                u32::from(*ch)
            ),
            Self::TitleTooLong { plugin, chars } => write!(
                f,
                "плагин «{plugin}»: название длиной {chars} символов длиннее {TITLE_MAX_CHARS} \
                 символов"
            ),
            Self::BuiltinPlacements { plugin, missing } => write!(
                f,
                "встроенный плагин «{plugin}» не объявил зону {missing:?}"
            ),
            Self::DefaultZoneNotPlaced { plugin, zone } => write!(
                f,
                "плагин «{plugin}»: default_zone {zone:?} нет в placements"
            ),
            Self::MinSize { plugin } => write!(
                f,
                "плагин «{plugin}»: min_size не конечный или не положительный"
            ),
            Self::StateVersion { plugin } => {
                write!(f, "плагин «{plugin}»: state_version меньше 1")
            }
            Self::ActionName { plugin, action } => write!(
                f,
                "плагин «{plugin}»: имя действия «{action}» не вида «{plugin}.<имя>»"
            ),
            Self::DuplicateAction { plugin, action } => {
                write!(f, "плагин «{plugin}»: действие «{action}» объявлено дважды")
            }
            Self::HostCommand { plugin, action } => write!(
                f,
                "плагин «{plugin}»: действие «{action}» лежит в пространстве имён команд хоста"
            ),
            Self::KeymapAction { plugin, action } => write!(
                f,
                "плагин «{plugin}»: keymap ссылается на неизвестное действие «{action}»"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

/// Проверенные плагины в порядке регистрации: этот порядок видят `query.plugins` и меню.
#[derive(Debug, Default)]
pub struct Registry {
    plugins: Vec<Plugin>,
    index: BTreeMap<PluginId, usize>,
}

impl Registry {
    /// Проверяет манифесты записей и собирает реестр. Первая найденная ошибка — отказ.
    pub fn build(entries: Vec<Entry>) -> Result<Self, RegistryError> {
        let mut registry = Self::default();
        for Entry { model, variants } in entries {
            let manifest = model.manifest();
            let plugin = || manifest.id.to_owned();
            let id = PluginId::new(manifest.id).map_err(|error| RegistryError::InvalidId {
                plugin: plugin(),
                error,
            })?;
            if registry.index.contains_key(&id) {
                return Err(RegistryError::DuplicateId { plugin: plugin() });
            }
            check_manifest(manifest)?;
            registry.index.insert(id.clone(), registry.plugins.len());
            registry.plugins.push(Plugin {
                id,
                model,
                variants,
            });
        }
        Ok(registry)
    }

    #[must_use]
    pub fn get(&self, id: &PluginId) -> Option<&Plugin> {
        self.index.get(id).map(|&index| &self.plugins[index])
    }

    /// Плагины в порядке регистрации.
    pub fn iter(&self) -> impl Iterator<Item = &Plugin> {
        self.plugins.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Описания плагинов в порядке регистрации.
    #[must_use]
    pub fn infos(&self) -> Vec<PluginInfo> {
        self.plugins.iter().map(Plugin::info).collect()
    }
}

fn check_manifest(manifest: &Manifest) -> Result<(), RegistryError> {
    let plugin = || manifest.id.to_owned();

    let title = manifest.title.trim();
    if title.is_empty() {
        return Err(RegistryError::TitleEmpty { plugin: plugin() });
    }
    if let Some(ch) = title.chars().find(|ch| ch.is_control()) {
        return Err(RegistryError::TitleControlChar {
            plugin: plugin(),
            ch,
        });
    }
    let chars = title.chars().count();
    if chars > TITLE_MAX_CHARS {
        return Err(RegistryError::TitleTooLong {
            plugin: plugin(),
            chars,
        });
    }

    if manifest.builtin
        && let Some(missing) = ZoneSet::ALL
            .iter()
            .find(|&zone| !manifest.placements.contains(zone))
    {
        return Err(RegistryError::BuiltinPlacements {
            plugin: plugin(),
            missing,
        });
    }
    if !manifest.placements.contains(manifest.default_zone) {
        return Err(RegistryError::DefaultZoneNotPlaced {
            plugin: plugin(),
            zone: manifest.default_zone,
        });
    }

    let positive = |value: f64| value.is_finite() && value > 0.0;
    if !positive(manifest.min_size.width) || !positive(manifest.min_size.height) {
        return Err(RegistryError::MinSize { plugin: plugin() });
    }
    if manifest.state_version < 1 {
        return Err(RegistryError::StateVersion { plugin: plugin() });
    }

    let host = HOST_NAMESPACES.contains(&manifest.id);
    let mut actions = BTreeSet::new();
    for spec in manifest.commands {
        let action = || spec.name.to_owned();
        let named = spec
            .name
            .strip_prefix(manifest.id)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|name| {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|ch| matches!(ch, 'a'..='z' | '0'..='9' | '_' | '.' | '-'))
            });
        if !named {
            return Err(RegistryError::ActionName {
                plugin: plugin(),
                action: action(),
            });
        }
        if host {
            return Err(RegistryError::HostCommand {
                plugin: plugin(),
                action: action(),
            });
        }
        if !actions.insert(spec.name) {
            return Err(RegistryError::DuplicateAction {
                plugin: plugin(),
                action: action(),
            });
        }
    }
    if let Some(key) = manifest
        .keymap
        .iter()
        .find(|key| !actions.contains(key.action))
    {
        return Err(RegistryError::KeymapAction {
            plugin: plugin(),
            action: key.action.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use shellio_layout::MinSize;

    use super::*;
    use crate::fake::{Fake, MANIFEST, VARIANTS, manifest_with};
    use crate::manifest::{Cardinality, CommandSpec, KeyDefault};

    fn entry(edit: impl FnOnce(&mut Manifest)) -> Entry {
        Entry::new(Fake::with_manifest(manifest_with(edit)), VARIANTS)
    }

    fn build_one(edit: impl FnOnce(&mut Manifest)) -> Result<Registry, RegistryError> {
        Registry::build(vec![entry(edit)])
    }

    fn refused(edit: impl FnOnce(&mut Manifest)) -> RegistryError {
        build_one(edit).unwrap_err()
    }

    fn accepted(edit: impl FnOnce(&mut Manifest)) {
        if let Err(error) = build_one(edit) {
            panic!("{error}");
        }
    }

    fn commands(names: &[&'static str]) -> &'static [CommandSpec] {
        Box::leak(
            names
                .iter()
                .map(|&name| CommandSpec { name, args: "{}" })
                .collect(),
        )
    }

    /// Плагин `id` без действий: имена действий подставного манифеста начинаются с `fake.`.
    fn renamed(id: &'static str) -> impl FnOnce(&mut Manifest) {
        move |manifest| {
            manifest.id = id;
            manifest.commands = &[];
            manifest.keymap = &[];
        }
    }

    fn plugin() -> String {
        "fake".to_owned()
    }

    #[test]
    fn registry_keeps_registration_order() {
        let registry = Registry::build(vec![
            entry(renamed("terminal")),
            entry(renamed("navigator")),
            entry(|_| {}),
            entry(renamed("a-first-by-name")),
        ])
        .unwrap();
        assert_eq!(registry.len(), 4);
        assert!(!registry.is_empty());
        let order: Vec<&str> = registry.iter().map(|plugin| plugin.id().as_str()).collect();
        assert_eq!(order, ["terminal", "navigator", "fake", "a-first-by-name"]);
        let infos: Vec<String> = registry
            .infos()
            .into_iter()
            .map(|info| info.id.to_string())
            .collect();
        assert_eq!(infos, order);

        let fake = registry.get(&PluginId::new("fake").unwrap()).unwrap();
        assert_eq!(fake.manifest(), &MANIFEST);
        assert_eq!(fake.variants(), &VARIANTS);
        assert_eq!(fake.info().commands, ["fake.fill", "fake.bump"]);
        assert_eq!(fake.model().manifest().id, "fake");
        assert!(registry.get(&PluginId::new("notes").unwrap()).is_none());

        let empty = Registry::build(Vec::new()).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.infos(), Vec::new());
    }

    #[test]
    fn registry_is_shared_between_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Registry>();
    }

    #[test]
    fn id_must_be_a_plugin_id_and_unique() {
        assert_eq!(
            refused(renamed("Bad Id")),
            RegistryError::InvalidId {
                plugin: "Bad Id".to_owned(),
                error: PluginIdError::InvalidChar { ch: 'B' },
            }
        );
        assert_eq!(
            refused(renamed("")),
            RegistryError::InvalidId {
                plugin: String::new(),
                error: PluginIdError::Empty,
            }
        );
        assert_eq!(
            Registry::build(vec![entry(|_| {}), entry(renamed("notes")), entry(|_| {})])
                .unwrap_err(),
            RegistryError::DuplicateId { plugin: plugin() }
        );
        accepted(|_| {});
    }

    #[test]
    fn title_follows_the_panel_title_rules() {
        for empty in ["", "   ", "\t\n"] {
            assert_eq!(
                refused(|manifest| manifest.title = empty),
                RegistryError::TitleEmpty { plugin: plugin() },
                "{empty:?}"
            );
        }
        assert_eq!(
            refused(|manifest| manifest.title = "Демо\u{7}список"),
            RegistryError::TitleControlChar {
                plugin: plugin(),
                ch: '\u{7}',
            }
        );
        let long: &'static str = Box::leak("я".repeat(TITLE_MAX_CHARS + 1).into_boxed_str());
        assert_eq!(
            refused(|manifest| manifest.title = long),
            RegistryError::TitleTooLong {
                plugin: plugin(),
                chars: TITLE_MAX_CHARS + 1,
            }
        );
        // Длина считается в символах, а не в байтах, пробелы по краям не в счёт.
        let longest: &'static str =
            Box::leak(format!(" {} ", "я".repeat(TITLE_MAX_CHARS)).into_boxed_str());
        accepted(|manifest| manifest.title = longest);
        accepted(|manifest| manifest.title = "Демо: список");
    }

    #[test]
    fn builtin_declares_all_four_zones() {
        for missing in Zone::ALL {
            let rest: Vec<Zone> = Zone::ALL
                .into_iter()
                .filter(|&zone| zone != missing)
                .collect();
            let placements = ZoneSet::of(&rest);
            let default_zone = rest[0];
            assert_eq!(
                refused(|manifest| {
                    manifest.placements = placements;
                    manifest.default_zone = default_zone;
                }),
                RegistryError::BuiltinPlacements {
                    plugin: plugin(),
                    missing,
                }
            );
            // Подставной регистрации тестов узкие `placements` разрешены.
            accepted(|manifest| {
                manifest.builtin = false;
                manifest.placements = placements;
                manifest.default_zone = default_zone;
            });
        }
        accepted(|manifest| manifest.placements = ZoneSet::ALL);
    }

    #[test]
    fn default_zone_is_among_placements() {
        assert_eq!(
            refused(|manifest| {
                manifest.builtin = false;
                manifest.placements = ZoneSet::of(&[Zone::Left, Zone::Bottom]);
                manifest.default_zone = Zone::Right;
            }),
            RegistryError::DefaultZoneNotPlaced {
                plugin: plugin(),
                zone: Zone::Right,
            }
        );
        assert_eq!(
            refused(|manifest| {
                manifest.builtin = false;
                manifest.placements = ZoneSet::EMPTY;
            }),
            RegistryError::DefaultZoneNotPlaced {
                plugin: plugin(),
                zone: Zone::Right,
            }
        );
        accepted(|manifest| {
            manifest.builtin = false;
            manifest.placements = ZoneSet::of(&[Zone::Bottom]);
            manifest.default_zone = Zone::Bottom;
        });
    }

    #[test]
    fn min_size_is_finite_and_positive() {
        for (width, height) in [
            (0.0, 120.0),
            (200.0, 0.0),
            (-1.0, 120.0),
            (200.0, -0.5),
            (f64::NAN, 120.0),
            (200.0, f64::NAN),
            (f64::INFINITY, 120.0),
            (200.0, f64::NEG_INFINITY),
        ] {
            assert_eq!(
                refused(|manifest| manifest.min_size = MinSize { width, height }),
                RegistryError::MinSize { plugin: plugin() },
                "{width}×{height}"
            );
        }
        accepted(|manifest| {
            manifest.min_size = MinSize {
                width: 0.5,
                height: 1.0,
            }
        });
    }

    #[test]
    fn state_version_starts_at_one() {
        assert_eq!(
            refused(|manifest| manifest.state_version = 0),
            RegistryError::StateVersion { plugin: plugin() }
        );
        accepted(|manifest| manifest.state_version = 1);
        accepted(|manifest| manifest.state_version = u32::MAX);
    }

    #[test]
    fn action_names_carry_the_plugin_prefix() {
        for action in [
            "fill",
            "fake",
            "fake.",
            "fakefill",
            "other.fill",
            "fake.Fill",
            "fake.fill now",
            ".fake.fill",
        ] {
            assert_eq!(
                refused(|manifest| {
                    manifest.commands = commands(&[action]);
                    manifest.keymap = &[];
                }),
                RegistryError::ActionName {
                    plugin: plugin(),
                    action: action.to_owned(),
                },
                "{action}"
            );
        }
        accepted(|manifest| {
            manifest.commands = commands(&["fake.fill", "fake.move_part.next", "fake.a-b"]);
            manifest.keymap = &[];
        });
        accepted(|manifest| {
            manifest.commands = &[];
            manifest.keymap = &[];
        });
    }

    #[test]
    fn action_names_are_unique() {
        assert_eq!(
            refused(|manifest| {
                manifest.commands = commands(&["fake.fill", "fake.bump", "fake.fill"]);
            }),
            RegistryError::DuplicateAction {
                plugin: plugin(),
                action: "fake.fill".to_owned(),
            }
        );
        // Один и тот же короткий хвост у двух плагинов — разные полные имена.
        let registry = Registry::build(vec![
            entry(|_| {}),
            entry(|manifest| {
                manifest.id = "notes";
                manifest.commands = commands(&["notes.fill", "notes.bump"]);
                manifest.keymap = &[];
            }),
        ])
        .unwrap();
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn actions_do_not_collide_with_host_commands() {
        for (id, action) in [
            ("panel", "panel.open"),
            ("panel", "panel.custom"),
            ("layout", "layout.split"),
            ("query", "query.plugins"),
            ("ui", "ui.receipts"),
        ] {
            assert_eq!(
                refused(|manifest| {
                    manifest.id = id;
                    manifest.commands = commands(&[action]);
                    manifest.keymap = &[];
                }),
                RegistryError::HostCommand {
                    plugin: id.to_owned(),
                    action: action.to_owned(),
                },
                "{action}"
            );
        }
        for namespace in HOST_NAMESPACES {
            assert!(PluginId::new(namespace).is_ok(), "{namespace}");
        }
        let mut sorted = HOST_NAMESPACES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, HOST_NAMESPACES);
        // Id с тем же началом — другое пространство имён.
        accepted(|manifest| {
            manifest.id = "panel-notes";
            manifest.commands = commands(&["panel-notes.open"]);
            manifest.keymap = &[];
        });
    }

    #[test]
    fn keymap_refers_to_actions_of_the_manifest() {
        const KEYMAP: &[KeyDefault] = &[
            KeyDefault {
                action: "fake.bump",
                key: "b",
            },
            KeyDefault {
                action: "fake.gone",
                key: "g",
            },
        ];
        assert_eq!(
            refused(|manifest| manifest.keymap = KEYMAP),
            RegistryError::KeymapAction {
                plugin: plugin(),
                action: "fake.gone".to_owned(),
            }
        );
        accepted(|manifest| manifest.keymap = &KEYMAP[..1]);
        accepted(|manifest| manifest.keymap = &[]);
    }

    #[test]
    fn every_cardinality_registers() {
        for cardinality in Cardinality::ALL {
            accepted(|manifest| manifest.cardinality = cardinality);
        }
    }

    #[test]
    fn errors_name_the_plugin() {
        let error = refused(|manifest| manifest.state_version = 0);
        assert_eq!(error.to_string(), "плагин «fake»: state_version меньше 1");
        let error = refused(renamed("Bad Id"));
        assert_eq!(
            error.to_string(),
            "плагин «Bad Id»: plugin id содержит символ U+0042 вне [a-z0-9-]"
        );
    }
}
