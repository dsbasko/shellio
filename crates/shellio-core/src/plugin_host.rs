//! Хост плагинов в ядре: `min_size` из манифеста, отпечаток реестра, канонический экземпляр и
//! статус панели по строкам базы и реестру (ADR-0008 и ADR-0018, разделы «Уточнено в P1.5
//! (планирование)»).

use std::collections::{BTreeMap, BTreeSet};

use shellio_layout::MinSize;
use shellio_plugin_api::{Cardinality, Plugin, Registry};

use crate::change::Change;
use crate::clock::UnixMillis;
use crate::ids::{PanelId, ProjectId, WorkbenchId, WorkspaceId};
use crate::model::State;
use crate::panel::{Panel, PanelData, PanelStatus, PluginId};

/// Номер правил вычисления статуса панели. Растёт при любом их изменении: он входит в отпечаток
/// реестра, поэтому новая сборка с другими правилами пишет запись запуска.
pub const STATUS_RULES: u32 = 1;

/// `min_size` панели плагина: из манифеста, у незарегистрированного плагина — 200×120 (ADR-0009).
/// Тем же значением пользуются команды, `query.layout` и сцена окна.
pub fn min_size(registry: &Registry, plugin: &PluginId) -> MinSize {
    registry
        .get(plugin)
        .map_or(MinSize::DEFAULT, |plugin| plugin.manifest().min_size)
}

/// Отпечаток реестра: номер правил статуса и отсортированный список
/// `plugin:state_version:cardinality`.
pub fn fingerprint(registry: &Registry) -> String {
    let mut plugins: Vec<String> = registry
        .iter()
        .map(|plugin| {
            let manifest = plugin.manifest();
            format!(
                "{}:{}:{}",
                plugin.id().as_str(),
                manifest.state_version,
                manifest.cardinality
            )
        })
        .collect();
    plugins.sort();
    format!("rules:{STATUS_RULES};{}", plugins.join(";"))
}

/// Владелец ключа экземпляра singleton.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Holder {
    Workbench(WorkbenchId),
    Project(ProjectId),
    Workspace(WorkspaceId),
    App,
}

/// Владелец ключа экземпляра для панели в рабочей области `workbench`. `None` — `Multi`: ключа
/// нет, каждая панель сама себе экземпляр. Рабочая область без проекта в состоянии остаётся
/// своим владельцем.
pub(crate) fn holder(
    state: &State,
    cardinality: Cardinality,
    workbench: WorkbenchId,
) -> Option<Holder> {
    let project = state
        .workbenches
        .get(&workbench)
        .map(|record| record.project);
    let workspace = project
        .and_then(|project| state.projects.get(&project))
        .map(|record| record.workspace);
    let own = Holder::Workbench(workbench);
    match cardinality {
        Cardinality::Multi => None,
        Cardinality::PerWorkbench => Some(own),
        Cardinality::PerProject => Some(project.map_or(own, Holder::Project)),
        Cardinality::PerWorkspace => Some(workspace.map_or(own, Holder::Workspace)),
        Cardinality::PerApp => Some(Holder::App),
    }
}

/// Канонические панели зарегистрированных плагинов: старейшая по `(created_at, PanelId)` среди
/// панелей одного ключа экземпляра. Панели `Multi` канонические все.
///
/// Экземпляр ищется только среди панелей рабочих областей видимых проектов. Панели скрытых
/// рабочих областей (убранный проект, удалённый воркспейс) судятся между собой и экземпляр
/// видимым не занимают.
pub(crate) fn canonical_panels(state: &State, registry: &Registry) -> BTreeSet<PanelId> {
    let mut canonical = BTreeSet::new();
    let mut oldest: BTreeMap<(&PluginId, Holder, bool), (UnixMillis, PanelId)> = BTreeMap::new();
    for panel in state.panels.values() {
        let Some(plugin) = registry.get(&panel.plugin) else {
            continue;
        };
        let Some(holder) = holder(state, plugin.manifest().cardinality, panel.workbench) else {
            canonical.insert(panel.id);
            continue;
        };
        let live = state.live_workbench(panel.workbench).is_some();
        let age = (panel.created_at, panel.id);
        oldest
            .entry((&panel.plugin, holder, live))
            .and_modify(|current| *current = (*current).min(age))
            .or_insert(age);
    }
    canonical.extend(oldest.into_values().map(|(_, panel)| panel));
    canonical
}

/// Канонический экземпляр singleton плагина для рабочей области `workbench`: старейшая по
/// `(created_at, PanelId)` панель плагина с тем же ключом экземпляра среди панелей живых рабочих
/// областей, без панели `except`. `None` — экземпляра нет или плагин `Multi`.
pub(crate) fn instance(
    state: &State,
    plugin: &Plugin,
    workbench: WorkbenchId,
    except: Option<PanelId>,
) -> Option<PanelId> {
    let cardinality = plugin.manifest().cardinality;
    let key = holder(state, cardinality, workbench)?;
    state
        .panels
        .values()
        .filter(|panel| {
            panel.plugin == *plugin.id()
                && Some(panel.id) != except
                && state.live_workbench(panel.workbench).is_some()
                && holder(state, cardinality, panel.workbench) == Some(key)
        })
        .map(|panel| (panel.created_at, panel.id))
        .min()
        .map(|(_, panel)| panel)
}

/// Переход права канонического экземпляра при закрытии панели `closed` (ADR-0008,
/// «Унаследованные дубликаты»): старейший оставшийся дубликат становится каноническим той же
/// записью и проходит `init` или `migrate` по правилу статуса. Его байты без записи не меняются.
pub(crate) fn heir(state: &State, registry: &Registry, closed: PanelId) -> Vec<Change> {
    let mut changes = Vec::new();
    let Some(record) = state.panels.get(&closed) else {
        return changes;
    };
    let Some(plugin) = registry.get(&record.plugin) else {
        return changes;
    };
    if instance(state, plugin, record.workbench, None) != Some(closed) {
        return changes;
    }
    let Some(next) = instance(state, plugin, record.workbench, Some(closed)) else {
        return changes;
    };
    let (status, written) = judge(Some(plugin), true, state.panel_data.get(&next));
    if let Some(Written { version, data }) = written {
        changes.push(Change::PanelData {
            panel: next,
            version,
            data,
        });
    }
    if status != state.status_of(next) {
        changes.push(Change::PanelStatus {
            panel: next,
            status,
        });
    }
    changes
}

/// Новые данные панели, которые даёт правило статуса: `init` или `migrate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Written {
    pub(crate) version: u32,
    pub(crate) data: String,
}

/// Статус панели по порядку ADR-0008 и данные, которые нужно записать, чтобы он стал `ready`.
///
/// Дубликат не инициализируется и не мигрирует. Нечитаемая строка, данные новее и отказ модели
/// данных не дают: исходные байты остаются в базе.
pub(crate) fn judge(
    plugin: Option<&Plugin>,
    canonical: bool,
    data: Option<&PanelData>,
) -> (PanelStatus, Option<Written>) {
    let Some(plugin) = plugin else {
        return (PanelStatus::PluginMissing, None);
    };
    if !canonical {
        return (PanelStatus::Duplicate, None);
    }
    let current = plugin.manifest().state_version;
    let written = |data| {
        Some(Written {
            version: current,
            data,
        })
    };
    match data {
        Some(PanelData::Unreadable {}) => (PanelStatus::StateCorrupt, None),
        None => match plugin.model().init(None) {
            Ok(data) => (PanelStatus::Ready, written(data)),
            Err(_) => (PanelStatus::InitFailed, None),
        },
        Some(PanelData::Text { version, raw }) => {
            if *version < current {
                match plugin.model().migrate(*version, raw) {
                    Ok(data) => (PanelStatus::Ready, written(data)),
                    Err(_) => (PanelStatus::MigrateFailed, None),
                }
            } else if *version > current {
                (PanelStatus::StateNewer, None)
            } else if plugin.model().check(raw).is_ok() {
                (PanelStatus::Ready, None)
            } else {
                (PanelStatus::StateCorrupt, None)
            }
        }
    }
}

/// Итог разбора загруженных панелей.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LoadedPanels {
    /// Статусы панелей не в `ready`. Ставятся в состояние без дельт.
    pub(crate) statuses: BTreeMap<PanelId, PanelStatus>,
    /// Дельты записи запуска: данные `init` и `migrate` и статус `ready` рядом с ними. Статус
    /// нужен клиенту с курсором прежнего запуска, у которого панель была не готова.
    pub(crate) changes: Vec<Change>,
}

/// Статусы загруженных панелей и данные, которые запуск дописывает (ADR-0018, «Запуск с `init` и
/// `migrate`»).
pub(crate) fn load_panels(state: &State, registry: &Registry) -> LoadedPanels {
    let canonical = canonical_panels(state, registry);
    let mut loaded = LoadedPanels::default();
    for panel in state.panels.values() {
        let (status, written) = judge(
            registry.get(&panel.plugin),
            canonical.contains(&panel.id),
            state.panel_data.get(&panel.id),
        );
        match status {
            PanelStatus::Ready => {}
            PanelStatus::PluginMissing
            | PanelStatus::Duplicate
            | PanelStatus::StateCorrupt
            | PanelStatus::InitFailed
            | PanelStatus::MigrateFailed
            | PanelStatus::StateNewer => {
                loaded.statuses.insert(panel.id, status);
            }
        }
        if let Some(Written { version, data }) = written {
            loaded.changes.push(Change::PanelData {
                panel: panel.id,
                version,
                data,
            });
            loaded.changes.push(Change::PanelStatus {
                panel: panel.id,
                status: PanelStatus::Ready,
            });
        }
    }
    loaded
}

/// Дельты новой панели: сама панель и `init` её данных той же записью (ADR-0008, «Начальные
/// данные»). Панель, у которой данных не получилось, несёт свой статус явной дельтой.
pub(crate) fn created(registry: &Registry, panel: Panel) -> Vec<Change> {
    let id = panel.id;
    let (status, written) = judge(registry.get(&panel.plugin), true, None);
    let mut changes = vec![Change::PanelAdd { panel }];
    if let Some(Written { version, data }) = written {
        changes.push(Change::PanelData {
            panel: id,
            version,
            data,
        });
    }
    match status {
        PanelStatus::Ready => {}
        PanelStatus::PluginMissing
        | PanelStatus::Duplicate
        | PanelStatus::StateCorrupt
        | PanelStatus::InitFailed
        | PanelStatus::MigrateFailed
        | PanelStatus::StateNewer => changes.push(Change::PanelStatus { panel: id, status }),
    }
    changes
}

#[cfg(feature = "test-support")]
pub use support::{COUNTER_MAX, Counter, TestInit, TestMigrate, TestPlugin, registry, registry_of};

#[cfg(feature = "test-support")]
mod support {
    use std::sync::Arc;

    use serde::de::{Deserializer, Error as _};
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use shellio_layout::{Geometry, MinSize, Rect, Zone};
    use shellio_plugin_api::{
        Cardinality, CommandSpec, Entry, Manifest, PanelModel, Presentation, Refusal, Region,
        Registry, STATE_LIMIT, SizeVariants, VariantTable, ZoneSet,
    };

    use crate::panel::{NAVIGATOR_PLUGIN, NAVIGATOR_TITLE, TERMINAL_PLUGIN, TERMINAL_TITLE};

    const VARIANT: &str = "full";

    const fn everywhere() -> SizeVariants {
        SizeVariants {
            narrow: VARIANT,
            regular: VARIANT,
            wide: VARIANT,
            short_bar: VARIANT,
        }
    }

    const VARIANTS: VariantTable = VariantTable {
        left: everywhere(),
        right: everywhere(),
        center: everywhere(),
        bottom: everywhere(),
    };

    fn whole_body(geometry: &Geometry) -> Presentation {
        Presentation {
            variant: VARIANT,
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

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct NoArgs {}

    enum NoCommand {}

    impl<'de> Deserialize<'de> for NoCommand {
        fn deserialize<D: Deserializer<'de>>(_deserializer: D) -> Result<Self, D::Error> {
            Err(D::Error::custom("у плагина нет действий"))
        }
    }

    fn no_older_version(manifest: &Manifest, from: u32) -> Refusal {
        Refusal::new(
            "panel",
            "state_version_unknown",
            format!("у плагина «{}» нет данных версии {from}", manifest.id),
        )
    }

    /// Заполнитель без данных, как `navigator` и `terminal` продукта.
    struct Blank {
        manifest: &'static Manifest,
    }

    impl PanelModel for Blank {
        type State = ();
        type Args = NoArgs;
        type Command = NoCommand;

        fn manifest(&self) -> &'static Manifest {
            self.manifest
        }

        fn init(&self, _args: Option<NoArgs>) -> Result<(), Refusal> {
            Ok(())
        }

        fn reopen(&self, _state: &(), _args: Option<NoArgs>) -> Result<(), Refusal> {
            Ok(())
        }

        fn apply(&self, _state: &(), command: NoCommand) -> Result<(), Refusal> {
            match command {}
        }

        fn migrate(&self, from: u32, _data: Value) -> Result<(), Refusal> {
            Err(no_older_version(self.manifest, from))
        }

        fn adapt(&self, geometry: &Geometry, _state: &()) -> Presentation {
            whole_body(geometry)
        }
    }

    const fn placeholder(
        id: &'static str,
        title: &'static str,
        default_zone: Zone,
        cardinality: Cardinality,
    ) -> Manifest {
        Manifest {
            id,
            title,
            icon: "square",
            placements: ZoneSet::ALL,
            default_zone,
            cardinality,
            min_size: MinSize::DEFAULT,
            commands: &[],
            keymap: &[],
            state_version: 1,
            builtin: true,
        }
    }

    static NAVIGATOR: Manifest = placeholder(
        NAVIGATOR_PLUGIN,
        NAVIGATOR_TITLE,
        Zone::Left,
        Cardinality::PerWorkbench,
    );
    static TERMINAL: Manifest = placeholder(
        TERMINAL_PLUGIN,
        TERMINAL_TITLE,
        Zone::Center,
        Cardinality::Multi,
    );

    /// Наибольшее значение счётчика [`Counter`]: больше — нарушение инварианта данных.
    pub const COUNTER_MAX: u32 = 9;

    /// Данные подставного плагина [`TestPlugin`]: счётчик не больше [`COUNTER_MAX`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(try_from = "RawCounter")]
    pub struct Counter {
        pub count: u32,
        #[serde(skip_serializing_if = "String::is_empty")]
        pub pad: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawCounter {
        count: u32,
        #[serde(default)]
        pad: String,
    }

    impl TryFrom<RawCounter> for Counter {
        type Error = String;

        fn try_from(raw: RawCounter) -> Result<Self, String> {
            if raw.count > COUNTER_MAX {
                return Err(format!("счётчик {} больше {COUNTER_MAX}", raw.count));
            }
            Ok(Self {
                count: raw.count,
                pad: raw.pad,
            })
        }
    }

    /// Данные прежней версии [`Counter`]: `{"n": <счётчик>}`.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OldCounter {
        n: u32,
    }

    /// Что делает `init` подставного плагина.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum TestInit {
        /// Счётчик 0.
        Zero,
        /// Отказ.
        Refuse,
        /// Данные больше предела размера.
        Oversize,
    }

    /// Что делает `migrate` подставного плагина.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum TestMigrate {
        /// `{"n": x}` любой прежней версии становится счётчиком `x`.
        Carry,
        /// Отказ.
        Refuse,
    }

    /// Контекст `open` и `reopen` подставного плагина: счётчик.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CounterArgs {
        count: u32,
    }

    /// Действия подставного плагина с [`TestPlugin::actions`]: `<id>.add {by}` прибавляет к
    /// счётчику, `<id>.pad {size}` ставит заполнитель длины `size`.
    enum CounterCommand {
        Add { by: u32 },
        Pad { size: usize },
    }

    impl<'de> Deserialize<'de> for CounterCommand {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Frame {
                name: String,
                args: Value,
            }
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Add {
                by: u32,
            }
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Pad {
                size: usize,
            }
            let frame = Frame::deserialize(deserializer)?;
            match frame.name.rsplit('.').next() {
                Some("add") => serde_json::from_value(frame.args)
                    .map(|Add { by }| Self::Add { by })
                    .map_err(D::Error::custom),
                Some("pad") => serde_json::from_value(frame.args)
                    .map(|Pad { size }| Self::Pad { size })
                    .map_err(D::Error::custom),
                Some(_) | None => Err(D::Error::custom("неизвестное действие")),
            }
        }
    }

    fn count_refusal(count: u32) -> Refusal {
        Refusal::new(
            "count",
            "count_out_of_range",
            format!("счётчик {count} больше {COUNTER_MAX}"),
        )
    }

    /// Подставной плагин с данными [`Counter`] для тестов ядра и хранилища. Не встроенный:
    /// `placements` по умолчанию — все зоны, действия — только с [`Self::actions`].
    #[derive(Clone, Copy, Debug)]
    pub struct TestPlugin {
        pub id: &'static str,
        pub title: &'static str,
        pub cardinality: Cardinality,
        pub state_version: u32,
        pub min_size: MinSize,
        pub init: TestInit,
        pub migrate: TestMigrate,
        pub placements: ZoneSet,
        pub default_zone: Zone,
        pub actions: bool,
    }

    impl TestPlugin {
        /// Плагин `id` с названием `id`: `Multi`, `state_version` 1, `min_size` по умолчанию,
        /// `init` даёт счётчик 0, `migrate` переносит счётчик.
        pub const fn new(id: &'static str) -> Self {
            Self {
                id,
                title: id,
                cardinality: Cardinality::Multi,
                state_version: 1,
                min_size: MinSize::DEFAULT,
                init: TestInit::Zero,
                migrate: TestMigrate::Carry,
                placements: ZoneSet::ALL,
                default_zone: Zone::Center,
                actions: false,
            }
        }

        /// Допустимые зоны и зона по умолчанию.
        #[must_use]
        pub const fn zones(mut self, placements: ZoneSet, default_zone: Zone) -> Self {
            self.placements = placements;
            self.default_zone = default_zone;
            self
        }

        /// Плагин объявляет действия `<id>.add {by}` и `<id>.pad {size}`.
        #[must_use]
        pub const fn actions(mut self) -> Self {
            self.actions = true;
            self
        }

        #[must_use]
        pub const fn title(mut self, title: &'static str) -> Self {
            self.title = title;
            self
        }

        #[must_use]
        pub const fn cardinality(mut self, cardinality: Cardinality) -> Self {
            self.cardinality = cardinality;
            self
        }

        #[must_use]
        pub const fn state_version(mut self, state_version: u32) -> Self {
            self.state_version = state_version;
            self
        }

        #[must_use]
        pub const fn min_size(mut self, min_size: MinSize) -> Self {
            self.min_size = min_size;
            self
        }

        #[must_use]
        pub const fn init(mut self, init: TestInit) -> Self {
            self.init = init;
            self
        }

        #[must_use]
        pub const fn migrate(mut self, migrate: TestMigrate) -> Self {
            self.migrate = migrate;
            self
        }

        /// Запись регистрации. Манифест живёт до конца процесса теста.
        pub fn entry(self) -> Entry {
            let commands: &'static [CommandSpec] = if self.actions {
                let name = |action: &str| -> &'static str {
                    Box::leak(format!("{}.{action}", self.id).into_boxed_str())
                };
                Box::leak(Box::new([
                    CommandSpec {
                        name: name("add"),
                        args: r#"{"by":1}"#,
                    },
                    CommandSpec {
                        name: name("pad"),
                        args: r#"{"size":3}"#,
                    },
                ]))
            } else {
                &[]
            };
            let manifest: &'static Manifest = Box::leak(Box::new(Manifest {
                id: self.id,
                title: self.title,
                icon: "square",
                placements: self.placements,
                default_zone: self.default_zone,
                cardinality: self.cardinality,
                min_size: self.min_size,
                commands,
                keymap: &[],
                state_version: self.state_version,
                builtin: false,
            }));
            Entry::new(
                CounterModel {
                    manifest,
                    init: self.init,
                    migrate: self.migrate,
                },
                VARIANTS,
            )
        }
    }

    struct CounterModel {
        manifest: &'static Manifest,
        init: TestInit,
        migrate: TestMigrate,
    }

    impl PanelModel for CounterModel {
        type State = Counter;
        type Args = CounterArgs;
        type Command = CounterCommand;

        fn manifest(&self) -> &'static Manifest {
            self.manifest
        }

        fn init(&self, args: Option<CounterArgs>) -> Result<Counter, Refusal> {
            let count = args.map_or(0, |args| args.count);
            if count > COUNTER_MAX {
                return Err(count_refusal(count));
            }
            match self.init {
                TestInit::Zero => Ok(Counter {
                    count,
                    pad: String::new(),
                }),
                TestInit::Refuse => Err(Refusal::new(
                    "args",
                    "init_refused",
                    "init отказал по плану теста",
                )),
                TestInit::Oversize => Ok(Counter {
                    count,
                    pad: "x".repeat(STATE_LIMIT),
                }),
            }
        }

        fn reopen(&self, state: &Counter, args: Option<CounterArgs>) -> Result<Counter, Refusal> {
            let Some(CounterArgs { count }) = args else {
                return Ok(state.clone());
            };
            if count > COUNTER_MAX {
                return Err(count_refusal(count));
            }
            Ok(Counter {
                count,
                pad: state.pad.clone(),
            })
        }

        fn apply(&self, state: &Counter, command: CounterCommand) -> Result<Counter, Refusal> {
            match command {
                CounterCommand::Add { by } => {
                    let count = state.count.saturating_add(by);
                    if count > COUNTER_MAX {
                        return Err(Refusal::new(
                            "by",
                            "count_out_of_range",
                            format!("счётчик {count} больше {COUNTER_MAX}"),
                        ));
                    }
                    Ok(Counter {
                        count,
                        pad: state.pad.clone(),
                    })
                }
                CounterCommand::Pad { size } => Ok(Counter {
                    count: state.count,
                    pad: "x".repeat(size),
                }),
            }
        }

        fn migrate(&self, from: u32, data: Value) -> Result<Counter, Refusal> {
            let refusal = |message: String| Refusal::new("panel", "migrate_refused", message);
            match self.migrate {
                TestMigrate::Refuse => Err(refusal("migrate отказал по плану теста".to_owned())),
                TestMigrate::Carry => {
                    let old: OldCounter = serde_json::from_value(data).map_err(|error| {
                        refusal(format!("данные версии {from} не разобраны: {error}"))
                    })?;
                    Counter::try_from(RawCounter {
                        count: old.n,
                        pad: String::new(),
                    })
                    .map_err(refusal)
                }
            }
        }

        fn adapt(&self, geometry: &Geometry, _state: &Counter) -> Presentation {
            whole_body(geometry)
        }
    }

    /// Тестовый реестр: заполнители `navigator` и `terminal` раскладки по умолчанию с названиями
    /// и кардинальностью продукта.
    ///
    /// # Panics
    ///
    /// Не бывает: манифесты заполнителей проходят проверки реестра.
    pub fn registry() -> Arc<Registry> {
        registry_of(Vec::new())
    }

    /// Тестовый реестр: `terminal`, `navigator` и подставные плагины `plugins` по порядку.
    ///
    /// # Panics
    ///
    /// Подставной плагин не проходит проверки реестра.
    pub fn registry_of(plugins: Vec<TestPlugin>) -> Arc<Registry> {
        let mut entries = vec![
            Entry::new(
                Blank {
                    manifest: &TERMINAL,
                },
                VARIANTS,
            ),
            Entry::new(
                Blank {
                    manifest: &NAVIGATOR,
                },
                VARIANTS,
            ),
        ];
        entries.extend(plugins.into_iter().map(TestPlugin::entry));
        match Registry::build(entries) {
            Ok(registry) => Arc::new(registry),
            Err(error) => panic!("тестовый реестр не собран: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use shellio_plugin_api::STATE_LIMIT;

    use super::*;
    use crate::ids::{IdGen, SequentialIds};
    use crate::model::{Project, Workbench, Workspace};
    use crate::title::Title;

    fn title(name: &str) -> Title {
        Title::new(name).unwrap()
    }

    /// Два воркспейса: в первом проект с двумя рабочими областями и второй проект с одной, во
    /// втором — проект с одной.
    struct World {
        state: State,
        ids: SequentialIds,
        a1: WorkbenchId,
        a2: WorkbenchId,
        b: WorkbenchId,
        c: WorkbenchId,
        second_project: ProjectId,
    }

    impl World {
        fn new() -> Self {
            let mut ids = SequentialIds::new();
            let mut state = State::default();
            let workspace = |state: &mut State, ids: &mut SequentialIds, name: &str| {
                let id = WorkspaceId::generate(ids);
                let position = u32::try_from(state.workspaces.len()).unwrap();
                state.workspaces.insert(
                    id,
                    Workspace {
                        id,
                        title: title(name),
                        position,
                        last_workbench: None,
                        deleted_at: None,
                    },
                );
                id
            };
            let w1 = workspace(&mut state, &mut ids, "Работа");
            let w2 = workspace(&mut state, &mut ids, "Дом");
            let project = |state: &mut State, ids: &mut SequentialIds, workspace, name: &str| {
                let id = ProjectId::generate(ids);
                state.projects.insert(
                    id,
                    Project {
                        id,
                        workspace,
                        title: title(name),
                        list_position: None,
                        removed_at: None,
                        last_opened_at: None,
                    },
                );
                id
            };
            let p1 = project(&mut state, &mut ids, w1, "first");
            let p2 = project(&mut state, &mut ids, w1, "second");
            let p3 = project(&mut state, &mut ids, w2, "third");
            let workbench = |state: &mut State, ids: &mut SequentialIds, project, name: &str| {
                let id = WorkbenchId::generate(ids);
                let position = u32::try_from(state.workbenches.len()).unwrap();
                state.workbenches.insert(
                    id,
                    Workbench {
                        id,
                        project,
                        title: title(name),
                        position,
                    },
                );
                id
            };
            let a1 = workbench(&mut state, &mut ids, p1, "a1");
            let a2 = workbench(&mut state, &mut ids, p1, "a2");
            let b = workbench(&mut state, &mut ids, p2, "b");
            let c = workbench(&mut state, &mut ids, p3, "c");
            state.window.active_workspace = Some(w1);
            Self {
                state,
                ids,
                a1,
                a2,
                b,
                c,
                second_project: p2,
            }
        }

        fn panel(&mut self, workbench: WorkbenchId, plugin: &str, created_at: i64) -> PanelId {
            let ids: &mut dyn IdGen = &mut self.ids;
            let id = PanelId::generate(ids);
            self.state.panels.insert(
                id,
                Panel {
                    id,
                    workbench,
                    plugin: PluginId::new(plugin).unwrap(),
                    title: title(plugin),
                    created_at: UnixMillis(created_at),
                },
            );
            id
        }

        fn text(&mut self, panel: PanelId, version: u32, raw: &str) {
            self.state.panel_data.insert(
                panel,
                PanelData::Text {
                    version,
                    raw: raw.to_owned(),
                },
            );
        }
    }

    fn one(plugin: TestPlugin) -> std::sync::Arc<Registry> {
        registry_of(vec![plugin])
    }

    /// Панели `demo` в четырёх рабочих областях по возрастанию возраста: старейшая — в `c`.
    fn spread(world: &mut World) -> [PanelId; 5] {
        [
            world.panel(world.c, "demo", 10),
            world.panel(world.a1, "demo", 20),
            world.panel(world.a1, "demo", 30),
            world.panel(world.a2, "demo", 40),
            world.panel(world.b, "demo", 50),
        ]
    }

    #[test]
    fn canonical_is_the_oldest_panel_of_each_instance_key() {
        let cases: [(Cardinality, &[usize]); 5] = [
            (Cardinality::Multi, &[0, 1, 2, 3, 4]),
            (Cardinality::PerWorkbench, &[0, 1, 3, 4]),
            (Cardinality::PerProject, &[0, 1, 4]),
            (Cardinality::PerWorkspace, &[0, 1]),
            (Cardinality::PerApp, &[0]),
        ];
        for (cardinality, expected) in cases {
            let mut world = World::new();
            let panels = spread(&mut world);
            let registry = one(TestPlugin::new("demo").cardinality(cardinality));
            let expected: BTreeSet<PanelId> = expected.iter().map(|&index| panels[index]).collect();
            assert_eq!(
                canonical_panels(&world.state, &registry),
                expected,
                "{cardinality}"
            );
        }
    }

    #[test]
    fn equal_age_is_decided_by_panel_id_and_unknown_plugins_have_no_canonical() {
        let mut world = World::new();
        let first = world.panel(world.a1, "demo", 7);
        let second = world.panel(world.a2, "demo", 7);
        let unknown = world.panel(world.a1, "gone", 1);
        let registry = one(TestPlugin::new("demo").cardinality(Cardinality::PerApp));
        let canonical = canonical_panels(&world.state, &registry);
        assert_eq!(canonical, BTreeSet::from([first.min(second)]));
        assert!(!canonical.contains(&unknown));
    }

    #[test]
    fn hidden_workbench_does_not_take_the_instance_of_visible_ones() {
        let mut world = World::new();
        let hidden = world.panel(world.b, "demo", 1);
        let visible = world.panel(world.a1, "demo", 2);
        let later = world.panel(world.c, "demo", 3);
        world
            .state
            .projects
            .get_mut(&world.second_project)
            .unwrap()
            .removed_at = Some(UnixMillis(5));
        let registry = one(TestPlugin::new("demo").cardinality(Cardinality::PerApp));
        assert_eq!(
            canonical_panels(&world.state, &registry),
            BTreeSet::from([hidden, visible])
        );
        let loaded = load_panels(&world.state, &registry);
        assert_eq!(
            loaded.statuses,
            BTreeMap::from([(later, PanelStatus::Duplicate)])
        );
    }

    #[test]
    fn status_follows_the_order_of_rules() {
        let mut world = World::new();
        let registry = registry_of(vec![
            TestPlugin::new("demo")
                .state_version(2)
                .cardinality(Cardinality::PerWorkbench),
            TestPlugin::new("stuck")
                .state_version(2)
                .init(TestInit::Refuse)
                .migrate(TestMigrate::Refuse),
            TestPlugin::new("huge").init(TestInit::Oversize),
        ]);

        let missing = world.panel(world.a1, "gone", 1);
        world.text(missing, 1, r#"{"x":1}"#);
        let fresh = world.panel(world.a1, "demo", 2);
        let duplicate = world.panel(world.a1, "demo", 3);
        let older = world.panel(world.a2, "demo", 4);
        world.text(older, 1, r#"{"n":4}"#);
        let ready = world.panel(world.b, "demo", 5);
        world.text(ready, 2, r#"{"count":5}"#);
        let newer = world.panel(world.c, "demo", 6);
        world.text(newer, 3, r#"{"count":1}"#);
        let unreadable = world.panel(world.a1, "stuck", 7);
        world
            .state
            .panel_data
            .insert(unreadable, PanelData::Unreadable {});
        let init_failed = world.panel(world.a1, "stuck", 8);
        let migrate_failed = world.panel(world.a1, "stuck", 9);
        world.text(migrate_failed, 1, r#"{"n":1}"#);
        let broken = world.panel(world.a1, "stuck", 10);
        world.text(broken, 2, "{");
        let null = world.panel(world.a1, "stuck", 11);
        world.text(null, 2, "null");
        let invariant = world.panel(world.a1, "stuck", 12);
        world.text(invariant, 2, r#"{"count":10}"#);
        let oversize = world.panel(world.a1, "huge", 13);

        let loaded = load_panels(&world.state, &registry);
        assert_eq!(
            loaded.statuses,
            BTreeMap::from([
                (missing, PanelStatus::PluginMissing),
                (duplicate, PanelStatus::Duplicate),
                (newer, PanelStatus::StateNewer),
                (unreadable, PanelStatus::StateCorrupt),
                (init_failed, PanelStatus::InitFailed),
                (migrate_failed, PanelStatus::MigrateFailed),
                (broken, PanelStatus::StateCorrupt),
                (null, PanelStatus::StateCorrupt),
                (invariant, PanelStatus::StateCorrupt),
                (oversize, PanelStatus::InitFailed),
            ])
        );
        let ready_delta = |panel| Change::PanelStatus {
            panel,
            status: PanelStatus::Ready,
        };
        assert_eq!(
            loaded.changes,
            [
                Change::PanelData {
                    panel: fresh,
                    version: 2,
                    data: r#"{"count":0}"#.to_owned(),
                },
                ready_delta(fresh),
                Change::PanelData {
                    panel: older,
                    version: 2,
                    data: r#"{"count":4}"#.to_owned(),
                },
                ready_delta(older),
            ]
        );

        // Применённые дельты дают состояние, в котором запуску больше нечего писать.
        world.state.panel_status = loaded.statuses.clone();
        world.state.apply_all(&loaded.changes);
        let again = load_panels(&world.state, &registry);
        assert_eq!(again.statuses, loaded.statuses);
        assert_eq!(again.changes, Vec::new());
    }

    #[test]
    fn duplicate_and_missing_plugin_come_before_the_data_rules() {
        let mut world = World::new();
        let registry = one(TestPlugin::new("demo").cardinality(Cardinality::PerApp));
        let canonical = world.panel(world.a1, "demo", 1);
        world.text(canonical, 1, r#"{"count":1}"#);
        let duplicate = world.panel(world.b, "demo", 2);
        world
            .state
            .panel_data
            .insert(duplicate, PanelData::Unreadable {});
        let missing = world.panel(world.b, "gone", 3);
        world
            .state
            .panel_data
            .insert(missing, PanelData::Unreadable {});
        let loaded = load_panels(&world.state, &registry);
        assert_eq!(
            loaded.statuses,
            BTreeMap::from([
                (duplicate, PanelStatus::Duplicate),
                (missing, PanelStatus::PluginMissing),
            ])
        );
        assert_eq!(loaded.changes, Vec::new());
    }

    #[test]
    fn created_panel_carries_init_data_or_its_status() {
        let mut world = World::new();
        let registry = registry_of(vec![
            TestPlugin::new("demo").state_version(3),
            TestPlugin::new("stuck").init(TestInit::Refuse),
            TestPlugin::new("huge").init(TestInit::Oversize),
        ]);
        let made = |world: &mut World, plugin: &str| {
            let id = world.panel(world.a1, plugin, 1);
            let panel = world.state.panels.remove(&id).unwrap();
            (id, created(&registry, panel.clone()), panel)
        };

        let (id, changes, panel) = made(&mut world, "demo");
        assert_eq!(
            changes,
            [
                Change::PanelAdd { panel },
                Change::PanelData {
                    panel: id,
                    version: 3,
                    data: r#"{"count":0}"#.to_owned(),
                },
            ]
        );

        let (id, changes, panel) = made(&mut world, "terminal");
        assert_eq!(
            changes,
            [
                Change::PanelAdd { panel },
                Change::PanelData {
                    panel: id,
                    version: 1,
                    data: "null".to_owned(),
                },
            ]
        );

        for (plugin, status) in [
            ("gone", PanelStatus::PluginMissing),
            ("stuck", PanelStatus::InitFailed),
            ("huge", PanelStatus::InitFailed),
        ] {
            let (id, changes, panel) = made(&mut world, plugin);
            assert_eq!(
                changes,
                [
                    Change::PanelAdd { panel },
                    Change::PanelStatus { panel: id, status },
                ],
                "{plugin}"
            );
        }
    }

    #[test]
    fn min_size_comes_from_the_manifest_and_defaults_for_unknown_plugins() {
        let wide = MinSize {
            width: 260.0,
            height: 160.0,
        };
        let registry = one(TestPlugin::new("demo").min_size(wide));
        let id = |raw: &str| PluginId::new(raw).unwrap();
        assert_eq!(min_size(&registry, &id("demo")), wide);
        assert_eq!(min_size(&registry, &id("terminal")), MinSize::DEFAULT);
        assert_eq!(min_size(&registry, &id("gone")), MinSize::DEFAULT);
    }

    #[test]
    fn fingerprint_names_rules_and_sorted_plugins() {
        let registry = registry_of(vec![
            TestPlugin::new("demo")
                .state_version(2)
                .cardinality(Cardinality::PerApp),
        ]);
        assert_eq!(
            fingerprint(&registry),
            "rules:1;demo:2:per_app;navigator:1:per_workbench;terminal:1:multi"
        );
        let changed = |plugin: TestPlugin| fingerprint(&one(plugin));
        let base = changed(TestPlugin::new("demo"));
        assert_ne!(base, changed(TestPlugin::new("demo").state_version(2)));
        assert_ne!(
            base,
            changed(TestPlugin::new("demo").cardinality(Cardinality::PerApp))
        );
        assert_ne!(base, fingerprint(&registry_of(Vec::new())));
        // Название, `min_size` и поведение модели в отпечаток не входят.
        assert_eq!(base, changed(TestPlugin::new("demo").title("Демо")));
        assert_eq!(
            base,
            changed(TestPlugin::new("demo").init(TestInit::Refuse))
        );
    }

    #[test]
    fn oversize_init_is_one_byte_over_the_limit_at_least() {
        let registry = one(TestPlugin::new("huge").init(TestInit::Oversize));
        let plugin = registry.get(&PluginId::new("huge").unwrap()).unwrap();
        let refusal = plugin.model().init(None).unwrap_err();
        assert_eq!(refusal.reason, "panel_state_too_large");
        assert!(refusal.details["size"].as_u64().unwrap() > STATE_LIMIT as u64);
    }
}
