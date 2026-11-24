//! Единственный список регистраций плагинов (ADR-0008, «Уточнено в P1.5 (планирование)»): манифест,
//! модель и фабрика вида каждого плагина. Из него строятся чистый [`Registry`] для ядра и таблица
//! фабрик [`Factories`] главного потока. Веток по id плагина вне этого списка нет.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, OnceLock};

use shellio_core::{NAVIGATOR_PLUGIN, TERMINAL_PLUGIN};
use shellio_plugin_api::{Entry, PluginId, Registry, RegistryError};
use shellio_plugin_view::ViewFactory;

/// Плагины, без которых продуктовый реестр не собирается: их панели создаёт раскладка по умолчанию.
pub const REQUIRED: [&str; 2] = [TERMINAL_PLUGIN, NAVIGATOR_PLUGIN];

/// Регистрация плагина: чистая запись для реестра и фабрика вида.
pub struct Registration {
    pub entry: Entry,
    pub factory: Box<dyn ViewFactory>,
}

/// Плагины приложения в порядке регистрации: его видят `query.plugins`, меню и выбор новой части.
#[must_use]
pub fn registrations() -> Vec<Registration> {
    vec![
        Registration {
            entry: plugin_terminal::entry(),
            factory: plugin_terminal::view::factory(),
        },
        Registration {
            entry: plugin_navigator::entry(),
            factory: plugin_navigator::view::factory(),
        },
        Registration {
            entry: plugin_demo_list::entry(),
            factory: plugin_demo_list::view::factory(),
        },
        Registration {
            entry: plugin_demo_native::entry(),
            factory: plugin_demo_native::view::factory(),
        },
    ]
}

/// Причина, по которой список регистраций не годится для запуска.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginsError {
    Registry(RegistryError),
    /// Нет плагина из [`REQUIRED`].
    RequiredMissing {
        plugin: &'static str,
    },
    /// У плагина реестра нет фабрики вида.
    FactoryMissing {
        plugin: String,
    },
    /// У плагина реестра больше одной фабрики вида.
    FactoryDuplicate {
        plugin: String,
    },
    /// Фабрика вида называет плагин, которого нет в реестре.
    FactoryUnknown {
        plugin: String,
    },
}

impl fmt::Display for PluginsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(error) => write!(f, "реестр плагинов не собран: {error}"),
            Self::RequiredMissing { plugin } => {
                write!(f, "плагин «{plugin}» не зарегистрирован")
            }
            Self::FactoryMissing { plugin } => {
                write!(f, "у плагина «{plugin}» нет фабрики вида")
            }
            Self::FactoryDuplicate { plugin } => {
                write!(f, "у плагина «{plugin}» больше одной фабрики вида")
            }
            Self::FactoryUnknown { plugin } => {
                write!(f, "фабрика вида плагина «{plugin}», которого нет в реестре")
            }
        }
    }
}

impl std::error::Error for PluginsError {}

/// Продуктовый реестр из списка регистраций.
pub fn registry() -> Result<Registry, PluginsError> {
    product_registry(
        registrations()
            .into_iter()
            .map(|registration| registration.entry)
            .collect(),
    )
}

/// Продуктовый реестр процесса: один на ядро и на главный поток. Собирается при первом вызове,
/// отказ сборки запоминается и повторяется каждому вызывающему.
pub fn shared_registry() -> Result<Arc<Registry>, PluginsError> {
    static REGISTRY: OnceLock<Result<Arc<Registry>, PluginsError>> = OnceLock::new();
    REGISTRY.get_or_init(|| registry().map(Arc::new)).clone()
}

/// Таблица фабрик главного потока из списка регистраций для плагинов `registry`.
pub fn factories(registry: &Registry) -> Result<Factories, PluginsError> {
    Factories::build(
        registry,
        registrations()
            .into_iter()
            .map(|registration| registration.factory)
            .collect(),
    )
}

/// Реестр по правилам продукта: проверки [`Registry::build`] и обязательные плагины [`REQUIRED`].
pub fn product_registry(entries: Vec<Entry>) -> Result<Registry, PluginsError> {
    let registry = Registry::build(entries).map_err(PluginsError::Registry)?;
    for plugin in REQUIRED {
        let registered = PluginId::new(plugin).is_ok_and(|id| registry.get(&id).is_some());
        if !registered {
            return Err(PluginsError::RequiredMissing { plugin });
        }
    }
    Ok(registry)
}

/// Фабрики вида по plugin id: ровно одна на каждый плагин реестра.
pub struct Factories {
    by_plugin: BTreeMap<PluginId, Box<dyn ViewFactory>>,
}

impl Factories {
    pub fn build(
        registry: &Registry,
        factories: Vec<Box<dyn ViewFactory>>,
    ) -> Result<Self, PluginsError> {
        let mut by_plugin = BTreeMap::new();
        for factory in factories {
            let plugin = factory.plugin();
            let id = PluginId::new(plugin)
                .ok()
                .filter(|id| registry.get(id).is_some())
                .ok_or_else(|| PluginsError::FactoryUnknown {
                    plugin: plugin.to_owned(),
                })?;
            if by_plugin.insert(id, factory).is_some() {
                return Err(PluginsError::FactoryDuplicate {
                    plugin: plugin.to_owned(),
                });
            }
        }
        match registry
            .iter()
            .find(|plugin| !by_plugin.contains_key(plugin.id()))
        {
            Some(plugin) => Err(PluginsError::FactoryMissing {
                plugin: plugin.id().as_str().to_owned(),
            }),
            None => Ok(Self { by_plugin }),
        }
    }

    #[must_use]
    pub fn get(&self, plugin: &PluginId) -> Option<&dyn ViewFactory> {
        self.by_plugin.get(plugin).map(Box::as_ref)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_plugin.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_plugin.is_empty()
    }
}

impl fmt::Debug for Factories {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.by_plugin.keys()).finish()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use shellio_plugin_view::{PluginView, ViewCtx};

    use super::*;

    struct Fake(&'static str);

    impl ViewFactory for Fake {
        fn plugin(&self) -> &'static str {
            self.0
        }

        fn build(&self, _ctx: ViewCtx<'_>, _state: &Value) -> Box<dyn PluginView> {
            unimplemented!("тесты без NSApplication тело не строят")
        }
    }

    fn fakes(plugins: &[&'static str]) -> Vec<Box<dyn ViewFactory>> {
        plugins
            .iter()
            .map(|&plugin| Box::new(Fake(plugin)) as Box<dyn ViewFactory>)
            .collect()
    }

    fn ids(registry: &Registry) -> Vec<&str> {
        registry.iter().map(|plugin| plugin.id().as_str()).collect()
    }

    #[test]
    fn product_list_registers_terminal_first_and_one_factory_per_plugin() {
        let registry = registry().unwrap();
        assert_eq!(
            ids(&registry),
            [
                TERMINAL_PLUGIN,
                NAVIGATOR_PLUGIN,
                "demo-list",
                "demo-native"
            ]
        );

        let factories = factories(&registry).unwrap();
        assert_eq!(factories.len(), registry.len());
        assert!(!factories.is_empty());
        for plugin in registry.iter() {
            let factory = factories.get(plugin.id()).unwrap();
            assert_eq!(factory.plugin(), plugin.id().as_str());
        }
    }

    #[test]
    fn registration_names_the_same_plugin_in_entry_and_factory() {
        for registration in registrations() {
            let plugin = registration.factory.plugin();
            let registry = Registry::build(vec![registration.entry]).unwrap();
            assert_eq!(ids(&registry), [plugin]);
        }
    }

    #[test]
    fn product_registry_needs_navigator_and_terminal() {
        assert_eq!(
            product_registry(vec![plugin_terminal::entry()]).unwrap_err(),
            PluginsError::RequiredMissing {
                plugin: NAVIGATOR_PLUGIN
            }
        );
        assert_eq!(
            product_registry(vec![plugin_navigator::entry()]).unwrap_err(),
            PluginsError::RequiredMissing {
                plugin: TERMINAL_PLUGIN
            }
        );
        assert_eq!(
            product_registry(Vec::new()).unwrap_err(),
            PluginsError::RequiredMissing {
                plugin: TERMINAL_PLUGIN
            }
        );
    }

    #[test]
    fn product_registry_keeps_the_checks_of_the_registry() {
        let twice = vec![plugin_terminal::entry(), plugin_terminal::entry()];
        assert!(matches!(
            product_registry(twice).unwrap_err(),
            PluginsError::Registry(RegistryError::DuplicateId { plugin }) if plugin == TERMINAL_PLUGIN
        ));
    }

    #[test]
    fn factory_table_wants_exactly_one_factory_per_plugin() {
        let registry = registry().unwrap();
        let build = |plugins: &[&'static str]| Factories::build(&registry, fakes(plugins));

        assert_eq!(
            build(&["navigator", "demo-native", "demo-list", "terminal"])
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            build(&["navigator", "terminal", "demo-list"]).unwrap_err(),
            PluginsError::FactoryMissing {
                plugin: "demo-native".into()
            }
        );
        assert_eq!(
            build(&["navigator", "terminal"]).unwrap_err(),
            PluginsError::FactoryMissing {
                plugin: "demo-list".into()
            }
        );
        assert_eq!(
            build(&["terminal"]).unwrap_err(),
            PluginsError::FactoryMissing {
                plugin: "navigator".into()
            }
        );
        assert_eq!(
            build(&[]).unwrap_err(),
            PluginsError::FactoryMissing {
                plugin: "terminal".into()
            }
        );
        assert_eq!(
            build(&["terminal", "navigator", "terminal"]).unwrap_err(),
            PluginsError::FactoryDuplicate {
                plugin: "terminal".into()
            }
        );
        assert_eq!(
            build(&["terminal", "navigator", "notes"]).unwrap_err(),
            PluginsError::FactoryUnknown {
                plugin: "notes".into()
            }
        );
        assert_eq!(
            build(&["terminal", "navigator", "Не id"]).unwrap_err(),
            PluginsError::FactoryUnknown {
                plugin: "Не id".into()
            }
        );
    }

    #[test]
    fn errors_name_the_plugin() {
        let error = PluginsError::FactoryMissing {
            plugin: "navigator".into(),
        };
        assert_eq!(error.to_string(), "у плагина «navigator» нет фабрики вида");
    }
}
